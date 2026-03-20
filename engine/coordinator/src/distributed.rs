/// Distributed load test coordinator.
///
/// Three modes:
///   run()                — embedded NATS broker + N spawned local agents  (original)
///   run_external_agents()— embedded NATS broker, wait for agents to connect externally
///   run_with_nats_url()  — connect to external NATS, wait for remote agents (Railway etc.)

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    time::{interval, sleep, timeout},
};

use crate::{
    broker,
    coordinator::{MetricsSnapshot, Phase, SharedSnapshot},
    metrics::LatencySnapshot,
    plan::ScenarioPlan,
};

// ── NATS subjects ─────────────────────────────────────────────────────────────

fn subject_register() -> &'static str { "loadpilot.register" }
fn subject_shard(agent_id: &str) -> String { format!("loadpilot.shard.{agent_id}") }
fn subject_metrics() -> &'static str { "loadpilot.metrics.>" }
fn subject_control() -> &'static str { "loadpilot.control" }

// ── Wire messages ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct RegisterMsg {
    agent_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ShardMsg {
    agent_id: String,
    plan: ScenarioPlan,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentMetricsMsg {
    agent_id: String,
    elapsed_secs: f64,
    current_rps: f64,
    target_rps: f64,
    requests_total: u64,
    errors_total: u64,
    active_workers: u64,
    phase: String,
    latency: LatencySnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlMsg {
    command: String, // "stop"
}

// ── Aggregated snapshot (same shape as coordinator stdout) ────────────────────

#[derive(Debug, Serialize)]
struct AggregatedSnapshot {
    timestamp_secs: f64,
    elapsed_secs: f64,
    current_rps: f64,
    target_rps: f64,
    requests_total: u64,
    errors_total: u64,
    active_workers: u64,
    phase: String,
    latency: LatencySnapshot,
}

fn aggregate(snapshots: &[AgentMetricsMsg]) -> AggregatedSnapshot {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    if snapshots.is_empty() {
        return AggregatedSnapshot {
            timestamp_secs: now, elapsed_secs: 0.0, current_rps: 0.0, target_rps: 0.0,
            requests_total: 0, errors_total: 0, active_workers: 0,
            phase: "ramp_up".to_string(),
            latency: LatencySnapshot::default(),
        };
    }

    let elapsed = snapshots.iter().map(|s| s.elapsed_secs).fold(0.0_f64, f64::max);
    let current_rps: f64 = snapshots.iter().map(|s| s.current_rps).sum();
    let target_rps: f64 = snapshots.iter().map(|s| s.target_rps).sum();
    let requests_total: u64 = snapshots.iter().map(|s| s.requests_total).sum();
    let errors_total: u64 = snapshots.iter().map(|s| s.errors_total).sum();
    let active_workers: u64 = snapshots.iter().map(|s| s.active_workers).sum();

    let phase = if snapshots.iter().all(|s| s.phase == "done") {
        "done".to_string()
    } else {
        let priority = |p: &str| match p {
            "steady" => 2, "ramp_down" => 3, "done" => 4, _ => 1,
        };
        snapshots.iter()
            .max_by_key(|s| priority(s.phase.as_str()))
            .map(|s| s.phase.clone())
            .unwrap_or_else(|| "ramp_up".to_string())
    };

    let total_req = requests_total.max(1) as f64;
    let mean_ms = snapshots.iter()
        .map(|s| s.latency.mean_ms * s.requests_total as f64)
        .sum::<f64>() / total_req;

    let p50_ms = snapshots.iter().map(|s| s.latency.p50_ms).sum::<f64>() / snapshots.len() as f64;
    let p95_ms = snapshots.iter().map(|s| s.latency.p95_ms).sum::<f64>() / snapshots.len() as f64;
    let p99_ms = snapshots.iter().map(|s| s.latency.p99_ms).sum::<f64>() / snapshots.len() as f64;
    let max_ms = snapshots.iter().map(|s| s.latency.max_ms).fold(0.0_f64, f64::max);
    let min_ms = snapshots.iter().map(|s| s.latency.min_ms).fold(f64::MAX, f64::min);

    AggregatedSnapshot {
        timestamp_secs: now,
        elapsed_secs: elapsed,
        current_rps,
        target_rps,
        requests_total,
        errors_total,
        active_workers,
        phase,
        latency: LatencySnapshot { p50_ms, p95_ms, p99_ms, max_ms, min_ms, mean_ms },
    }
}

// ── Plan sharding ─────────────────────────────────────────────────────────────

fn shard(plan: &ScenarioPlan, n: usize, idx: usize) -> ScenarioPlan {
    let base_rps = plan.rps / n as u64;
    let rps = if idx == n - 1 { plan.rps - base_rps * (n - 1) as u64 } else { base_rps };

    let n_vusers = plan.n_vusers.map(|v| {
        let base = v / n as u64;
        if idx == n - 1 { v - base * (n - 1) as u64 } else { base }.max(1)
    });

    ScenarioPlan { rps, n_vusers, ..plan.clone() }
}

// ── Agent binary discovery ────────────────────────────────────────────────────

fn agent_binary() -> Result<PathBuf> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let name = format!("agent{suffix}");

    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.with_file_name(&name);
        if candidate.exists() { return Ok(candidate); }
    }

    for profile in ["release", "debug"] {
        let candidate = PathBuf::from(format!("target/{profile}/{name}"));
        if candidate.exists() { return Ok(candidate); }
    }

    anyhow::bail!(
        "Could not find agent binary '{name}'. \
         Build it with: cargo build --release --package agent"
    )
}

// ── Embedded broker coordination (shared by run + run_external_agents) ────────

/// Core coordination loop over an embedded broker.
/// If `spawn` is true, spawns N local agent processes; otherwise waits for external agents.
async fn run_with_embedded_broker(
    plan: ScenarioPlan,
    n_agents: usize,
    broker_addr: &str,
    shared_snapshot: SharedSnapshot,
    spawn: bool,
) -> Result<()> {
    eprintln!("[distributed] starting embedded broker on {broker_addr}");
    let broker = broker::start(broker_addr).await
        .with_context(|| format!("Failed to start embedded broker on {broker_addr}"))?;

    let mut reg_rx = broker::subscribe(&broker, subject_register()).await;
    let mut metrics_rx = broker::subscribe(&broker, subject_metrics()).await;

    let mut children = Vec::new();

    if spawn {
        let agent_bin = agent_binary()?;
        eprintln!("[distributed] spawning {n_agents} agents using {}", agent_bin.display());
        let run_id = uuid::Uuid::new_v4().to_string();

        for i in 0..n_agents {
            let child = Command::new(&agent_bin)
                .arg("--coordinator").arg(broker_addr)
                .arg("--agent-id").arg(format!("agent-{i}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("Failed to spawn agent {i}"))?;
            children.push(child);
        }
        let _ = run_id; // kept for potential future use
    } else {
        eprintln!("[distributed] embedded broker ready — waiting for {n_agents} external agent(s) on {broker_addr}");
    }

    // Registration timeout: 30s for local agents, 5 min for external.
    let reg_timeout_secs = if spawn { 30 } else { 300 };
    eprintln!("[distributed] waiting for {n_agents} agent(s) to register (timeout: {reg_timeout_secs}s)...");
    let mut registered: Vec<String> = Vec::new();

    timeout(Duration::from_secs(reg_timeout_secs), async {
        while registered.len() < n_agents {
            if let Some(payload) = reg_rx.recv().await {
                if let Ok(msg) = serde_json::from_slice::<RegisterMsg>(&payload) {
                    eprintln!("[distributed] agent registered: {}", msg.agent_id);
                    registered.push(msg.agent_id);
                }
            }
        }
    }).await.context("Timed out waiting for agents to register")?;

    for (idx, agent_id) in registered.iter().enumerate() {
        let shard_plan = shard(&plan, n_agents, idx);
        let msg = ShardMsg { agent_id: agent_id.clone(), plan: shard_plan };
        let payload = serde_json::to_vec(&msg)?;
        broker::publish(&broker, &subject_shard(agent_id), &payload).await;
        eprintln!("[distributed] sent plan shard to {agent_id} ({} RPS)", msg.plan.rps);
    }

    aggregate_loop(&mut metrics_rx, n_agents, &shared_snapshot).await?;

    let stop = serde_json::to_vec(&ControlMsg { command: "stop".to_string() })?;
    broker::publish(&broker, subject_control(), &stop).await;

    sleep(Duration::from_millis(500)).await;

    for mut child in children {
        let _ = child.kill().await;
    }

    Ok(())
}

/// Metrics aggregation loop — shared by embedded and external NATS modes.
/// Reads from `metrics_rx`, emits aggregated JSON to stdout, updates Prometheus.
async fn aggregate_loop(
    metrics_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    n_agents: usize,
    shared_snapshot: &SharedSnapshot,
) -> Result<()> {
    let mut last_snapshots: std::collections::HashMap<String, AgentMetricsMsg> =
        std::collections::HashMap::new();
    let mut tick = interval(Duration::from_secs(1));
    let mut done_count = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snaps: Vec<AgentMetricsMsg> = last_snapshots.values().map(|s| AgentMetricsMsg {
                    agent_id: s.agent_id.clone(),
                    elapsed_secs: s.elapsed_secs,
                    current_rps: s.current_rps,
                    target_rps: s.target_rps,
                    requests_total: s.requests_total,
                    errors_total: s.errors_total,
                    active_workers: s.active_workers,
                    phase: s.phase.clone(),
                    latency: s.latency.clone(),
                }).collect();

                let agg = aggregate(&snaps);
                println!("{}", serde_json::to_string(&agg)?);
                update_prometheus(shared_snapshot, &agg);

                if done_count >= n_agents {
                    break;
                }
            }

            Some(payload) = metrics_rx.recv() => {
                if let Ok(msg) = serde_json::from_slice::<AgentMetricsMsg>(&payload) {
                    if msg.phase == "done" { done_count += 1; }
                    last_snapshots.insert(msg.agent_id.clone(), msg);
                }
            }
        }
    }

    // Final snapshot.
    let snaps: Vec<AgentMetricsMsg> = last_snapshots.into_values().collect();
    let agg = aggregate(&snaps);
    println!("{}", serde_json::to_string(&agg)?);
    update_prometheus(shared_snapshot, &agg);

    Ok(())
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Embedded broker + N spawned local agent processes. Original mode.
pub async fn run(plan: ScenarioPlan, n_agents: usize, broker_addr: &str, shared_snapshot: SharedSnapshot) -> Result<()> {
    run_with_embedded_broker(plan, n_agents, broker_addr, shared_snapshot, true).await
}

/// Embedded broker + wait for N externally started agents (no spawning).
/// Useful for local testing without Railway: run coordinator, then start agents manually.
pub async fn run_external_agents(plan: ScenarioPlan, n_agents: usize, broker_addr: &str, shared_snapshot: SharedSnapshot) -> Result<()> {
    run_with_embedded_broker(plan, n_agents, broker_addr, shared_snapshot, false).await
}

/// Connect to an external NATS server and wait for N remote agents.
/// Agents connect to the same NATS independently (e.g. Railway services).
pub async fn run_with_nats_url(
    plan: ScenarioPlan,
    n_agents: usize,
    nats_url: &str,
    shared_snapshot: SharedSnapshot,
) -> Result<()> {
    use crate::nats_client::NatsClient;

    eprintln!("[distributed] connecting to external NATS at {nats_url}");
    let mut nats = NatsClient::connect(nats_url).await
        .with_context(|| format!("Failed to connect to NATS at {nats_url}"))?;

    nats.subscribe(subject_register(), "reg").await?;
    nats.subscribe(subject_metrics(), "metrics").await?;

    // Wait for N agents (5 min timeout for remote agents).
    eprintln!("[distributed] waiting for {n_agents} remote agent(s) to register (timeout: 300s)...");
    let mut registered: Vec<String> = Vec::new();

    let reg_deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(reg_deadline);

    loop {
        tokio::select! {
            _ = &mut reg_deadline => {
                anyhow::bail!(
                    "Timed out waiting for agents ({}/{} registered). \
                     Make sure agents are running and can reach {}.",
                    registered.len(), n_agents, nats_url
                );
            }
            Ok((subject, payload)) = nats.next_message() => {
                if subject == subject_register() {
                    if let Ok(msg) = serde_json::from_slice::<RegisterMsg>(&payload) {
                        eprintln!("[distributed] agent registered: {}", msg.agent_id);
                        registered.push(msg.agent_id.clone());
                        if registered.len() >= n_agents { break; }
                    }
                }
            }
        }
    }

    // Send shards.
    for (idx, agent_id) in registered.iter().enumerate() {
        let shard_plan = shard(&plan, n_agents, idx);
        let msg = ShardMsg { agent_id: agent_id.clone(), plan: shard_plan };
        let payload = serde_json::to_vec(&msg)?;
        nats.publish(&subject_shard(agent_id), &payload).await?;
        eprintln!("[distributed] sent shard to {agent_id} ({} RPS)", msg.plan.rps);
    }

    // Aggregate metrics via a channel fed from the NATS read loop.
    let (metrics_tx, mut metrics_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Spawn a task that reads all remaining NATS messages and routes metrics.
    tokio::spawn(async move {
        loop {
            match nats.next_message().await {
                Ok((subject, payload)) if subject.starts_with("loadpilot.metrics.") => {
                    if metrics_tx.send(payload).is_err() { break; }
                }
                Ok(_) => {}  // ignore other subjects (register, control echoes)
                Err(_) => break,
            }
        }
    });

    aggregate_loop(&mut metrics_rx, n_agents, &shared_snapshot).await?;

    // Note: we don't publish "stop" here because we can't — nats was moved into the task.
    // Remote agents will stop on their own when the plan duration expires.

    sleep(Duration::from_millis(500)).await;

    Ok(())
}

fn update_prometheus(shared: &SharedSnapshot, agg: &AggregatedSnapshot) {
    let phase = match agg.phase.as_str() {
        "steady" => Phase::Steady,
        "done" => Phase::Done,
        _ => Phase::RampUp,
    };
    let snapshot = MetricsSnapshot {
        timestamp_secs: agg.timestamp_secs,
        elapsed_secs: agg.elapsed_secs,
        current_rps: agg.current_rps,
        target_rps: agg.target_rps,
        requests_total: agg.requests_total,
        errors_total: agg.errors_total,
        active_workers: agg.active_workers,
        latency: agg.latency.clone(),
        phase,
    };
    if let Ok(mut guard) = shared.write() {
        *guard = Some(snapshot);
    }
}
