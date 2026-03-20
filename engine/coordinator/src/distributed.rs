/// Distributed load test coordinator.
///
/// Starts the embedded NATS broker, spawns N local agent processes,
/// distributes sharded plan to each, aggregates their metrics, and
/// streams the combined result to stdout (same JSON format as single-process mode).

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::Mutex,
    time::{interval, sleep, timeout},
};

use crate::{
    broker::{self, BrokerHandle},
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

    // Phase: "done" only if all agents are done.
    let phase = if snapshots.iter().all(|s| s.phase == "done") {
        "done".to_string()
    } else {
        // Use the phase of the agent that has progressed furthest.
        let priority = |p: &str| match p {
            "steady" => 2, "ramp_down" => 3, "done" => 4, _ => 1,
        };
        snapshots.iter()
            .max_by_key(|s| priority(s.phase.as_str()))
            .map(|s| s.phase.clone())
            .unwrap_or_else(|| "ramp_up".to_string())
    };

    // Latency: weighted average for mean, max of maxes, min of mins,
    // simple average for percentiles (approximation without histogram merging).
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
    // Last agent absorbs any remainder.
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

    // 1. Same directory as current binary (installed wheel or cargo run).
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.with_file_name(&name);
        if candidate.exists() { return Ok(candidate); }
    }

    // 2. Cargo workspace target directories (dev).
    for profile in ["release", "debug"] {
        let candidate = PathBuf::from(format!("target/{profile}/{name}"));
        if candidate.exists() { return Ok(candidate); }
    }

    anyhow::bail!(
        "Could not find agent binary '{name}'. \
         Build it with: cargo build --release --package agent"
    )
}

// ── Distributed coordinator ───────────────────────────────────────────────────

pub async fn run(plan: ScenarioPlan, n_agents: usize, broker_addr: &str) -> Result<()> {
    eprintln!("[distributed] starting embedded broker on {broker_addr}");
    let broker = broker::start(broker_addr).await
        .with_context(|| format!("Failed to start embedded broker on {broker_addr}"))?;

    // Subscribe to agent registrations and metrics before spawning agents
    // so we don't miss any messages.
    let mut reg_rx = broker::subscribe(&broker, subject_register()).await;
    let mut metrics_rx = broker::subscribe(&broker, subject_metrics()).await;

    let agent_bin = agent_binary()?;
    eprintln!("[distributed] spawning {n_agents} agents using {}", agent_bin.display());

    let run_id = uuid::Uuid::new_v4().to_string();

    // Spawn agent processes.
    let mut children = Vec::new();
    for i in 0..n_agents {
        let mut child = Command::new(&agent_bin)
            .arg("--coordinator").arg(broker_addr)
            .arg("--run-id").arg(&run_id)
            .arg("--agent-id").arg(format!("agent-{i}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn agent {i}"))?;
        children.push(child);
    }

    // Wait for all agents to register (timeout: 30s).
    eprintln!("[distributed] waiting for {n_agents} agent(s) to register...");
    let mut registered: Vec<String> = Vec::new();

    timeout(Duration::from_secs(30), async {
        while registered.len() < n_agents {
            if let Some(payload) = reg_rx.recv().await {
                if let Ok(msg) = serde_json::from_slice::<RegisterMsg>(&payload) {
                    eprintln!("[distributed] agent registered: {}", msg.agent_id);
                    registered.push(msg.agent_id);
                }
            }
        }
    }).await.context("Timed out waiting for agents to register")?;

    // Send each agent its shard.
    for (idx, agent_id) in registered.iter().enumerate() {
        let shard_plan = shard(&plan, n_agents, idx);
        let msg = ShardMsg { agent_id: agent_id.clone(), plan: shard_plan };
        let payload = serde_json::to_vec(&msg)?;
        broker::publish(&broker, &subject_shard(agent_id), &payload).await;
        eprintln!("[distributed] sent plan shard to {agent_id} ({} RPS)", msg.plan.rps);
    }

    // Aggregate metrics and stream to stdout until all agents are done.
    let mut last_snapshots: std::collections::HashMap<String, AgentMetricsMsg> =
        std::collections::HashMap::new();
    let mut tick = interval(Duration::from_secs(1));
    let mut done_count = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snaps: Vec<&AgentMetricsMsg> = last_snapshots.values().collect();
                let agg = aggregate(&snaps.iter().map(|s| AgentMetricsMsg {
                    agent_id: s.agent_id.clone(),
                    elapsed_secs: s.elapsed_secs,
                    current_rps: s.current_rps,
                    target_rps: s.target_rps,
                    requests_total: s.requests_total,
                    errors_total: s.errors_total,
                    active_workers: s.active_workers,
                    phase: s.phase.clone(),
                    latency: s.latency.clone(),
                }).collect::<Vec<_>>());

                println!("{}", serde_json::to_string(&agg)?);

                if agg.phase == "done" && last_snapshots.len() == n_agents {
                    break;
                }
            }

            Some(payload) = metrics_rx.recv() => {
                if let Ok(msg) = serde_json::from_slice::<AgentMetricsMsg>(&payload) {
                    if msg.phase == "done" {
                        done_count += 1;
                    }
                    last_snapshots.insert(msg.agent_id.clone(), msg);
                }
            }
        }

        if done_count >= n_agents {
            // Emit final snapshot then exit.
            let snaps: Vec<AgentMetricsMsg> = last_snapshots.into_values().collect();
            let agg = aggregate(&snaps);
            println!("{}", serde_json::to_string(&agg)?);
            break;
        }
    }

    // Signal all agents to stop (in case any are still running).
    let stop = serde_json::to_vec(&ControlMsg { command: "stop".to_string() })?;
    broker::publish(&broker, subject_control(), &stop).await;

    // Give agents a moment to clean up.
    sleep(Duration::from_millis(500)).await;

    for mut child in children {
        let _ = child.kill().await;
    }

    Ok(())
}
