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
    sync::watch,
    time::{interval, sleep, timeout},
};

use crate::{
    broker,
    coordinator::{MetricsSnapshot, Phase, SharedSnapshot},
    metrics::{LatencySnapshot, MetricSink, TaskSnapshot},
    plan::ScenarioPlan,
};

// ── NATS subjects ─────────────────────────────────────────────────────────────

fn subject_register() -> &'static str {
    "loadpilot.register"
}
fn subject_shard(agent_id: &str) -> String {
    format!("loadpilot.shard.{agent_id}")
}
fn subject_metrics() -> &'static str {
    "loadpilot.metrics.>"
}
fn subject_control() -> &'static str {
    "loadpilot.control"
}

// ── Wire messages ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct RegisterMsg {
    agent_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ShardMsg {
    agent_id: String,
    plan: ScenarioPlan,
    /// Unix timestamp (ms) at which all agents should start the test simultaneously.
    /// Coordinator sets this to `now + 2000ms` so agents can synchronise clocks.
    start_at_unix_ms: u64,
}

/// Wire type for per-task data received from agents (matches agent's TaskWireSnapshot).
#[derive(Debug, Serialize, Deserialize, Clone)]
struct TaskWireSnapshot {
    name: String,
    requests: u64,
    errors: u64,
    latency: LatencySnapshot,
    histogram_buckets: Vec<[u64; 2]>,
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
    /// Sparse histogram: each entry is [bucket_index_ms, count].
    #[serde(default)]
    histogram_buckets: Vec<[u64; 2]>,
    /// Per-task breakdown.
    #[serde(default)]
    task_snapshots: Vec<TaskWireSnapshot>,
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
    /// Per-task breakdown aggregated across all agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tasks: Vec<TaskSnapshot>,
}

const MAX_BUCKET: usize = 10_000;

fn percentile_from_merged(buckets: &[u64; MAX_BUCKET + 1], total: u64, p: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let target = (total as f64 * p).ceil() as u64;
    let mut cum = 0u64;
    for (i, &count) in buckets.iter().enumerate() {
        cum += count;
        if cum >= target {
            return i as f64;
        }
    }
    MAX_BUCKET as f64
}

/// Merge per-task snapshots from all agents into aggregated TaskSnapshot list.
fn aggregate_tasks(snapshots: &[AgentMetricsMsg]) -> Vec<TaskSnapshot> {
    // task name → (merged_histogram, total_requests, total_errors, weighted_latency_sum)
    let mut map: std::collections::HashMap<String, ([u64; MAX_BUCKET + 1], u64, u64, f64)> =
        std::collections::HashMap::new();

    for snap in snapshots {
        for ts in &snap.task_snapshots {
            let entry = map
                .entry(ts.name.clone())
                .or_insert_with(|| ([0u64; MAX_BUCKET + 1], 0, 0, 0.0));
            for &[idx, count] in &ts.histogram_buckets {
                entry.0[(idx as usize).min(MAX_BUCKET)] += count;
            }
            entry.1 += ts.requests;
            entry.2 += ts.errors;
            entry.3 += ts.latency.mean_ms * ts.requests as f64;
        }
    }

    let mut tasks: Vec<TaskSnapshot> = map
        .into_iter()
        .map(|(name, (buckets, requests, errors, weighted_sum))| {
            let mean_ms = if requests > 0 {
                weighted_sum / requests as f64
            } else {
                0.0
            };
            // Derive max/min from merged histogram.
            let max_ms = buckets
                .iter()
                .enumerate()
                .rev()
                .find(|(_, &c)| c > 0)
                .map(|(i, _)| i as f64)
                .unwrap_or(0.0);
            let min_ms = buckets
                .iter()
                .enumerate()
                .find(|(_, &c)| c > 0)
                .map(|(i, _)| i as f64)
                .unwrap_or(0.0);
            TaskSnapshot {
                name,
                requests,
                errors,
                latency: LatencySnapshot {
                    p50_ms: percentile_from_merged(&buckets, requests, 0.50),
                    p95_ms: percentile_from_merged(&buckets, requests, 0.95),
                    p99_ms: percentile_from_merged(&buckets, requests, 0.99),
                    max_ms,
                    min_ms,
                    mean_ms,
                },
            }
        })
        .collect();
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    tasks
}

fn aggregate(snapshots: &[AgentMetricsMsg]) -> AggregatedSnapshot {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    if snapshots.is_empty() {
        return AggregatedSnapshot {
            timestamp_secs: now,
            elapsed_secs: 0.0,
            current_rps: 0.0,
            target_rps: 0.0,
            requests_total: 0,
            errors_total: 0,
            active_workers: 0,
            phase: "ramp_up".to_string(),
            latency: LatencySnapshot::default(),
            tasks: vec![],
        };
    }

    let elapsed = snapshots
        .iter()
        .map(|s| s.elapsed_secs)
        .fold(0.0_f64, f64::max);
    let current_rps: f64 = snapshots.iter().map(|s| s.current_rps).sum();
    let target_rps: f64 = snapshots.iter().map(|s| s.target_rps).sum();
    let requests_total: u64 = snapshots.iter().map(|s| s.requests_total).sum();
    let errors_total: u64 = snapshots.iter().map(|s| s.errors_total).sum();
    let active_workers: u64 = snapshots.iter().map(|s| s.active_workers).sum();

    let phase = if snapshots.iter().all(|s| s.phase == "done") {
        "done".to_string()
    } else {
        // Exclude "done" agents — overall phase is only "done" when ALL are done.
        let priority = |p: &str| match p {
            "steady" => 2,
            "ramp_down" => 3,
            _ => 1,
        };
        snapshots
            .iter()
            .filter(|s| s.phase != "done")
            .max_by_key(|s| priority(s.phase.as_str()))
            .map(|s| s.phase.clone())
            .unwrap_or_else(|| "ramp_up".to_string())
    };

    let total_req = requests_total.max(1) as f64;
    let mean_ms = snapshots
        .iter()
        .map(|s| s.latency.mean_ms * s.requests_total as f64)
        .sum::<f64>()
        / total_req;
    let max_ms = snapshots
        .iter()
        .map(|s| s.latency.max_ms)
        .fold(0.0_f64, f64::max);
    let min_ms = snapshots
        .iter()
        .map(|s| s.latency.min_ms)
        .fold(f64::MAX, f64::min);

    // Merge histograms for exact percentiles.
    // Fall back to simple average only if no agent sent histogram data.
    let has_histograms = snapshots.iter().any(|s| !s.histogram_buckets.is_empty());
    let (p50_ms, p95_ms, p99_ms) = if has_histograms {
        let mut merged = [0u64; MAX_BUCKET + 1];
        for s in snapshots {
            for &[idx, count] in &s.histogram_buckets {
                let i = (idx as usize).min(MAX_BUCKET);
                merged[i] += count;
            }
        }
        (
            percentile_from_merged(&merged, requests_total, 0.50),
            percentile_from_merged(&merged, requests_total, 0.95),
            percentile_from_merged(&merged, requests_total, 0.99),
        )
    } else {
        // Fallback: simple average (old behaviour, used when histogram_buckets absent)
        let n = snapshots.len() as f64;
        (
            snapshots.iter().map(|s| s.latency.p50_ms).sum::<f64>() / n,
            snapshots.iter().map(|s| s.latency.p95_ms).sum::<f64>() / n,
            snapshots.iter().map(|s| s.latency.p99_ms).sum::<f64>() / n,
        )
    };

    AggregatedSnapshot {
        timestamp_secs: now,
        elapsed_secs: elapsed,
        current_rps,
        target_rps,
        requests_total,
        errors_total,
        active_workers,
        phase,
        latency: LatencySnapshot {
            p50_ms,
            p95_ms,
            p99_ms,
            max_ms,
            min_ms,
            mean_ms,
        },
        tasks: aggregate_tasks(snapshots),
    }
}

// ── Plan sharding ─────────────────────────────────────────────────────────────

fn shard(plan: &ScenarioPlan, n: usize, idx: usize) -> ScenarioPlan {
    let base_rps = plan.rps / n as u64;
    let rps = if idx == n - 1 {
        plan.rps - base_rps * (n - 1) as u64
    } else {
        base_rps
    };

    let n_vusers = plan.n_vusers.map(|v| {
        let base = v / n as u64;
        if idx == n - 1 {
            v - base * (n - 1) as u64
        } else {
            base
        }
        .max(1)
    });

    ScenarioPlan {
        rps,
        n_vusers,
        ..plan.clone()
    }
}

// ── Agent binary discovery ────────────────────────────────────────────────────

fn agent_binary() -> Result<PathBuf> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let name = format!("agent{suffix}");

    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.with_file_name(&name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    for profile in ["release", "debug"] {
        let candidate = PathBuf::from(format!("target/{profile}/{name}"));
        if candidate.exists() {
            return Ok(candidate);
        }
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
    sink: MetricSink,
    spawn: bool,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    tracing::info!("starting embedded broker on {broker_addr}");
    let broker = broker::start(broker_addr)
        .await
        .with_context(|| format!("Failed to start embedded broker on {broker_addr}"))?;

    let mut reg_rx = broker::subscribe(&broker, subject_register()).await;
    let mut metrics_rx = broker::subscribe(&broker, subject_metrics()).await;

    let mut children = Vec::new();

    if spawn {
        let agent_bin = agent_binary()?;
        tracing::info!(n_agents, agent_bin = %agent_bin.display(), "spawning agents");
        let run_id = uuid::Uuid::new_v4().to_string();

        for i in 0..n_agents {
            let child = Command::new(&agent_bin)
                .arg("--coordinator")
                .arg(broker_addr)
                .arg("--agent-id")
                .arg(format!("agent-{i}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("Failed to spawn agent {i}"))?;
            children.push(child);
        }
        let _ = run_id; // kept for potential future use
    } else {
        tracing::info!(
            "embedded broker ready — waiting for {n_agents} external agent(s) on {broker_addr}"
        );
    }

    // Registration timeout: 30s for local agents, 5 min for external.
    let reg_timeout_secs = if spawn { 30 } else { 300 };
    tracing::info!("waiting for {n_agents} agent(s) to register (timeout: {reg_timeout_secs}s)...");
    let mut registered: Vec<String> = Vec::new();

    timeout(Duration::from_secs(reg_timeout_secs), async {
        while registered.len() < n_agents {
            if let Some(payload) = reg_rx.recv().await {
                if let Ok(msg) = serde_json::from_slice::<RegisterMsg>(&payload) {
                    tracing::info!("agent registered: {}", msg.agent_id);
                    registered.push(msg.agent_id);
                }
            }
        }
    })
    .await
    .context("Timed out waiting for agents to register")?;

    // All agents start at the same wall-clock time to eliminate clock skew.
    let start_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + 2_000;

    for (idx, agent_id) in registered.iter().enumerate() {
        let shard_plan = shard(&plan, n_agents, idx);
        let msg = ShardMsg {
            agent_id: agent_id.clone(),
            plan: shard_plan,
            start_at_unix_ms,
        };
        let payload = serde_json::to_vec(&msg)?;
        broker::publish(&broker, &subject_shard(agent_id), &payload).await;
        tracing::info!(
            agent_id,
            rps = msg.plan.rps,
            "sent plan shard (start_at +2s)"
        );
    }

    let shutdown = aggregate_loop(
        &mut metrics_rx,
        n_agents,
        &registered,
        &shared_snapshot,
        sink,
        shutdown_rx,
    )
    .await?;

    // Send "stop" to all agents regardless of how the loop exited.
    let stop = serde_json::to_vec(&ControlMsg {
        command: "stop".to_string(),
    })?;
    broker::publish(&broker, subject_control(), &stop).await;

    if shutdown {
        // Agents receive "stop" and exit their metrics loop immediately;
        // they do not send a final "done" snapshot. Give them a short window
        // to complete any in-flight HTTP requests (typically < 200ms each)
        // before we forcefully terminate the processes.
        tracing::info!("shutdown: waiting 3s for agents to drain in-flight requests...");
        sleep(Duration::from_secs(3)).await;
    } else {
        sleep(Duration::from_millis(500)).await;
    }

    for mut child in children {
        let _ = child.kill().await;
    }

    Ok(())
}

/// How long without a metric update before an agent is considered timed-out.
const AGENT_TIMEOUT_SECS: u64 = 15;

/// Metrics aggregation loop — shared by embedded and external NATS modes.
/// Reads from `metrics_rx`, emits aggregated JSON to stdout, updates Prometheus.
///
/// Returns `true` if the loop exited due to a shutdown signal (caller should
/// drain in-flight requests), `false` if all agents completed normally.
///
/// NATS SPOF protection: if an agent stops reporting for AGENT_TIMEOUT_SECS,
/// it is marked as timed-out and excluded from the completion count so the
/// test can still finish even if one agent dies.
///
/// `registered_agents` must contain every agent ID that received a shard.
/// Pre-seeding last_seen with them ensures that agents which never emit a
/// single metric are still caught by the timeout logic.
async fn aggregate_loop(
    metrics_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    n_agents: usize,
    registered_agents: &[String],
    shared_snapshot: &SharedSnapshot,
    sink: MetricSink,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let mut last_snapshots: std::collections::HashMap<String, AgentMetricsMsg> =
        std::collections::HashMap::new();
    // Pre-seed last_seen with every registered agent so that agents which never
    // emit a metric (e.g. slow debug-build startup) are still caught by the
    // AGENT_TIMEOUT_SECS timeout and don't stall the loop indefinitely.
    let seed_time = std::time::Instant::now();
    let mut last_seen: std::collections::HashMap<String, std::time::Instant> = registered_agents
        .iter()
        .map(|id| (id.clone(), seed_time))
        .collect();
    let mut timed_out: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut done_agents: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tick = interval(Duration::from_secs(1));
    // effective_agents shrinks when agents time out.
    let mut effective_agents = n_agents;

    let shutdown_triggered = 'agg: loop {
        tokio::select! {
            _ = tick.tick() => {
                // Check for timed-out agents.
                let now = std::time::Instant::now();
                for (agent_id, &seen) in &last_seen {
                    if !timed_out.contains(agent_id)
                        && now.duration_since(seen).as_secs() > AGENT_TIMEOUT_SECS
                    {
                        tracing::warn!(agent_id, timeout_secs = AGENT_TIMEOUT_SECS, "agent has not reported — treating as timed-out");
                        timed_out.insert(agent_id.clone());
                        effective_agents = effective_agents.saturating_sub(1);
                    }
                }

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
                    histogram_buckets: s.histogram_buckets.clone(),
                    task_snapshots: s.task_snapshots.clone(),
                }).collect();

                let agg = aggregate(&snaps);
                sink(serde_json::to_string(&agg)?);
                update_prometheus(shared_snapshot, &agg);

                if done_agents.len() + timed_out.len() >= effective_agents.max(1) {
                    break 'agg false;
                }
            }

            Some(payload) = metrics_rx.recv() => {
                if let Ok(msg) = serde_json::from_slice::<AgentMetricsMsg>(&payload) {
                    // If a timed-out agent recovers, restore it.
                    if timed_out.remove(&msg.agent_id) {
                        tracing::info!("agent {} recovered", msg.agent_id);
                        effective_agents = (effective_agents + 1).min(n_agents);
                    }
                    last_seen.insert(msg.agent_id.clone(), std::time::Instant::now());
                    if msg.phase == "done" { done_agents.insert(msg.agent_id.clone()); }
                    last_snapshots.insert(msg.agent_id.clone(), msg);
                }
            }

            Ok(()) = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("shutdown signal — stopping test");
                    break 'agg true;
                }
            }
        }
    };

    // Always emit a final snapshot regardless of how we exited.
    let snaps: Vec<AgentMetricsMsg> = last_snapshots.into_values().collect();
    let agg = aggregate(&snaps);
    sink(serde_json::to_string(&agg)?);
    update_prometheus(shared_snapshot, &agg);

    Ok(shutdown_triggered)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Embedded broker + N spawned local agent processes. Original mode.
pub async fn run(
    plan: ScenarioPlan,
    n_agents: usize,
    broker_addr: &str,
    shared_snapshot: SharedSnapshot,
    sink: MetricSink,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    run_with_embedded_broker(
        plan,
        n_agents,
        broker_addr,
        shared_snapshot,
        sink,
        true,
        &mut shutdown_rx,
    )
    .await
}

/// Embedded broker + wait for N externally started agents (no spawning).
/// Useful for local testing without Railway: run coordinator, then start agents manually.
pub async fn run_external_agents(
    plan: ScenarioPlan,
    n_agents: usize,
    broker_addr: &str,
    shared_snapshot: SharedSnapshot,
    sink: MetricSink,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    run_with_embedded_broker(
        plan,
        n_agents,
        broker_addr,
        shared_snapshot,
        sink,
        false,
        &mut shutdown_rx,
    )
    .await
}

/// Connect to an external NATS server and wait for N remote agents.
/// Agents connect to the same NATS independently (e.g. Railway services).
pub async fn run_with_nats_url(
    plan: ScenarioPlan,
    n_agents: usize,
    nats_url: &str,
    token: Option<&str>,
    shared_snapshot: SharedSnapshot,
    sink: MetricSink,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    use crate::nats_client::NatsClient;

    tracing::info!("connecting to external NATS at {nats_url}");
    let mut nats = match token {
        Some(t) => NatsClient::connect_authenticated(nats_url, t).await,
        None => NatsClient::connect(nats_url).await,
    }
    .with_context(|| format!("Failed to connect to NATS at {nats_url}"))?;

    nats.subscribe(subject_register(), "reg").await?;
    nats.subscribe(subject_metrics(), "metrics").await?;

    // Wait for N agents (5 min timeout for remote agents).
    tracing::info!(
        n_agents,
        "waiting for remote agents to register (timeout: 300s)"
    );
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
                        tracing::info!("agent registered: {}", msg.agent_id);
                        registered.push(msg.agent_id.clone());
                        if registered.len() >= n_agents { break; }
                    }
                }
            }
        }
    }

    // All agents start at the same wall-clock time to eliminate clock skew.
    let start_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + 2_000;

    // Send shards.
    for (idx, agent_id) in registered.iter().enumerate() {
        let shard_plan = shard(&plan, n_agents, idx);
        let msg = ShardMsg {
            agent_id: agent_id.clone(),
            plan: shard_plan,
            start_at_unix_ms,
        };
        let payload = serde_json::to_vec(&msg)?;
        nats.publish(&subject_shard(agent_id), &payload).await?;
        tracing::info!(agent_id, rps = msg.plan.rps, "sent shard (start_at +2s)");
    }

    // Channel to signal the NATS reader task to publish "stop" and exit.
    let (nats_stop_tx, mut nats_stop_rx) = watch::channel(false);
    let stop_payload = serde_json::to_vec(&ControlMsg {
        command: "stop".to_string(),
    })?;

    // Aggregate metrics via a channel fed from the NATS read loop.
    let (metrics_tx, mut metrics_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Spawn a task that reads NATS messages, forwards metrics, and sends "stop"
    // to agents when signalled.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(()) = nats_stop_rx.changed() => {
                    if *nats_stop_rx.borrow() {
                        let _ = nats.publish(subject_control(), &stop_payload).await;
                        // Continue reading so the drain loop below can collect "done" messages.
                    }
                }
                result = nats.next_message() => {
                    match result {
                        Ok((subject, payload)) if subject.starts_with("loadpilot.metrics.") => {
                            if metrics_tx.send(payload).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {} // ignore other subjects
                        Err(_) => break,
                    }
                }
            }
        }
    });

    let shutdown = aggregate_loop(
        &mut metrics_rx,
        n_agents,
        &registered,
        &shared_snapshot,
        sink,
        &mut shutdown_rx,
    )
    .await?;

    // Signal the NATS task to publish "stop" (always, not just on shutdown).
    let _ = nats_stop_tx.send(true);

    if shutdown {
        // Agents receive "stop" and exit their metrics loop immediately;
        // they do not send a final "done" snapshot. Give them a short window
        // to complete any in-flight HTTP requests (typically < 200ms each)
        // before we forcefully terminate the processes.
        tracing::info!("shutdown: waiting 3s for agents to drain in-flight requests...");
        sleep(Duration::from_secs(3)).await;
    } else {
        sleep(Duration::from_millis(500)).await;
    }

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
        tasks: agg.tasks.clone(),
    };
    if let Ok(mut guard) = shared.write() {
        *guard = Some(snapshot);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(agent_id: &str, buckets: Vec<[u64; 2]>, requests: u64) -> AgentMetricsMsg {
        AgentMetricsMsg {
            agent_id: agent_id.to_string(),
            elapsed_secs: 1.0,
            current_rps: requests as f64,
            target_rps: requests as f64,
            requests_total: requests,
            errors_total: 0,
            active_workers: 1,
            phase: "steady".to_string(),
            latency: LatencySnapshot::default(),
            histogram_buckets: buckets,
            task_snapshots: vec![],
        }
    }

    // ── percentile_from_merged ────────────────────────────────────────────────

    #[test]
    fn percentile_empty_returns_zero() {
        let buckets = [0u64; MAX_BUCKET + 1];
        assert_eq!(percentile_from_merged(&buckets, 0, 0.99), 0.0);
    }

    #[test]
    fn percentile_single_bucket() {
        let mut buckets = [0u64; MAX_BUCKET + 1];
        buckets[42] = 100;
        assert_eq!(percentile_from_merged(&buckets, 100, 0.50), 42.0);
        assert_eq!(percentile_from_merged(&buckets, 100, 0.99), 42.0);
    }

    #[test]
    fn percentile_two_equal_groups() {
        // 100 requests at 10ms, 100 requests at 100ms
        let mut buckets = [0u64; MAX_BUCKET + 1];
        buckets[10] = 100;
        buckets[100] = 100;
        // p50: target=100, cumulative hits 100 at bucket 10
        assert_eq!(percentile_from_merged(&buckets, 200, 0.50), 10.0);
        // p99: target=198, cumulative at 10=100 < 198, at 100=200 >= 198
        assert_eq!(percentile_from_merged(&buckets, 200, 0.99), 100.0);
    }

    // ── aggregate: histogram merging ─────────────────────────────────────────

    #[test]
    fn aggregate_empty_returns_ramp_up_phase() {
        let agg = aggregate(&[]);
        assert_eq!(agg.phase, "ramp_up");
        assert_eq!(agg.requests_total, 0);
    }

    #[test]
    fn aggregate_merges_histograms_exactly() {
        // Agent 1: 100 req all at 10ms
        // Agent 2: 100 req all at 100ms
        // Simple average p99 = (10 + 100) / 2 = 55ms  ← WRONG
        // Merged     p99 = 100ms                       ← CORRECT
        let agents = vec![
            make_agent("agent-0", vec![[10, 100]], 100),
            make_agent("agent-1", vec![[100, 100]], 100),
        ];
        let agg = aggregate(&agents);
        assert_eq!(agg.requests_total, 200);
        assert_eq!(
            agg.latency.p50_ms, 10.0,
            "p50 should be 10ms (first half of requests)"
        );
        assert_eq!(
            agg.latency.p99_ms, 100.0,
            "p99 should be 100ms, not 55ms average"
        );
    }

    #[test]
    fn aggregate_falls_back_to_average_without_histograms() {
        // When no histogram_buckets, coordinator falls back to simple average.
        let agents = vec![
            make_agent("agent-0", vec![], 100),
            make_agent("agent-1", vec![], 100),
        ];
        // Both have latency p99=0 (default), average = 0
        let agg = aggregate(&agents);
        assert_eq!(agg.latency.p99_ms, 0.0);
    }

    #[test]
    fn aggregate_sums_rps_and_requests() {
        let agents = vec![
            make_agent("agent-0", vec![[10, 50]], 50),
            make_agent("agent-1", vec![[20, 50]], 50),
        ];
        let agg = aggregate(&agents);
        assert_eq!(agg.requests_total, 100);
        assert_eq!(agg.current_rps, 100.0);
    }

    #[test]
    fn aggregate_phase_done_only_when_all_done() {
        let mut a0 = make_agent("agent-0", vec![], 100);
        let mut a1 = make_agent("agent-1", vec![], 100);
        a0.phase = "done".to_string();
        a1.phase = "steady".to_string();
        let agg = aggregate(&[a0, a1]);
        assert_ne!(agg.phase, "done");
    }

    #[test]
    fn aggregate_phase_done_when_all_done() {
        let mut a0 = make_agent("agent-0", vec![], 100);
        let mut a1 = make_agent("agent-1", vec![], 100);
        a0.phase = "done".to_string();
        a1.phase = "done".to_string();
        let agg = aggregate(&[a0, a1]);
        assert_eq!(agg.phase, "done");
    }

    #[test]
    fn aggregate_max_ms_is_maximum_across_agents() {
        let mut a0 = make_agent("agent-0", vec![], 100);
        let mut a1 = make_agent("agent-1", vec![], 100);
        a0.latency.max_ms = 200.0;
        a1.latency.max_ms = 500.0;
        let agg = aggregate(&[a0, a1]);
        assert_eq!(agg.latency.max_ms, 500.0);
    }

    // ── aggregate_tasks ───────────────────────────────────────────────────────

    fn make_task_wire(
        name: &str,
        requests: u64,
        errors: u64,
        buckets: Vec<[u64; 2]>,
        mean_ms: f64,
    ) -> TaskWireSnapshot {
        TaskWireSnapshot {
            name: name.to_string(),
            requests,
            errors,
            latency: LatencySnapshot {
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                min_ms: 0.0,
                mean_ms,
            },
            histogram_buckets: buckets,
        }
    }

    fn make_agent_with_tasks(agent_id: &str, tasks: Vec<TaskWireSnapshot>) -> AgentMetricsMsg {
        let requests_total: u64 = tasks.iter().map(|t| t.requests).sum();
        AgentMetricsMsg {
            agent_id: agent_id.to_string(),
            elapsed_secs: 1.0,
            current_rps: requests_total as f64,
            target_rps: requests_total as f64,
            requests_total,
            errors_total: tasks.iter().map(|t| t.errors).sum(),
            active_workers: 1,
            phase: "steady".to_string(),
            latency: LatencySnapshot::default(),
            histogram_buckets: vec![],
            task_snapshots: tasks,
        }
    }

    #[test]
    fn aggregate_tasks_empty_snapshots() {
        let tasks = aggregate_tasks(&[]);
        assert!(tasks.is_empty());
    }

    #[test]
    fn aggregate_tasks_single_agent_single_task() {
        let agent = make_agent_with_tasks(
            "a0",
            vec![make_task_wire("login", 10, 2, vec![[50, 10]], 50.0)],
        );
        let tasks = aggregate_tasks(&[agent]);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, "login");
        assert_eq!(t.requests, 10);
        assert_eq!(t.errors, 2);
        assert_eq!(t.latency.mean_ms, 50.0);
    }

    #[test]
    fn aggregate_tasks_merges_same_task_across_agents() {
        // Two agents each do 10 reqs for "search"
        let a0 = make_agent_with_tasks(
            "a0",
            vec![make_task_wire("search", 10, 0, vec![[20, 10]], 20.0)],
        );
        let a1 = make_agent_with_tasks(
            "a1",
            vec![make_task_wire("search", 10, 1, vec![[40, 10]], 40.0)],
        );
        let tasks = aggregate_tasks(&[a0, a1]);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, "search");
        assert_eq!(t.requests, 20);
        assert_eq!(t.errors, 1);
        // Weighted mean: (20*10 + 40*10) / 20 = 30ms
        assert_eq!(t.latency.mean_ms, 30.0);
    }

    #[test]
    fn aggregate_tasks_merges_histograms_for_exact_percentiles() {
        // Agent 0: "api" — 100 reqs all at 10ms
        // Agent 1: "api" — 100 reqs all at 200ms
        // Simple average p99 = (10+200)/2 = 105ms ← WRONG
        // Merged     p99 = 200ms                   ← CORRECT
        let a0 = make_agent_with_tasks(
            "a0",
            vec![make_task_wire("api", 100, 0, vec![[10, 100]], 10.0)],
        );
        let a1 = make_agent_with_tasks(
            "a1",
            vec![make_task_wire("api", 100, 0, vec![[200, 100]], 200.0)],
        );
        let tasks = aggregate_tasks(&[a0, a1]);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].latency.p99_ms, 200.0);
        assert_eq!(tasks[0].latency.p50_ms, 10.0);
    }

    #[test]
    fn aggregate_tasks_multiple_tasks_sorted_by_name() {
        let agent = make_agent_with_tasks(
            "a0",
            vec![
                make_task_wire("search", 5, 0, vec![[30, 5]], 30.0),
                make_task_wire("login", 5, 0, vec![[10, 5]], 10.0),
                make_task_wire("checkout", 5, 0, vec![[50, 5]], 50.0),
            ],
        );
        let tasks = aggregate_tasks(&[agent]);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].name, "checkout");
        assert_eq!(tasks[1].name, "login");
        assert_eq!(tasks[2].name, "search");
    }

    #[test]
    fn aggregate_tasks_max_min_derived_from_merged_histogram() {
        let a0 =
            make_agent_with_tasks("a0", vec![make_task_wire("t", 1, 0, vec![[500, 1]], 500.0)]);
        let a1 = make_agent_with_tasks("a1", vec![make_task_wire("t", 1, 0, vec![[10, 1]], 10.0)]);
        let tasks = aggregate_tasks(&[a0, a1]);
        assert_eq!(tasks[0].latency.max_ms, 500.0);
        assert_eq!(tasks[0].latency.min_ms, 10.0);
    }

    #[test]
    fn aggregate_tasks_empty_task_snapshots_produces_no_tasks() {
        // Agents send metrics but no task_snapshots (old agent version).
        let a0 = make_agent("agent-0", vec![[10, 100]], 100);
        let a1 = make_agent("agent-1", vec![[20, 100]], 100);
        let tasks = aggregate_tasks(&[a0, a1]);
        assert!(tasks.is_empty());
    }

    #[test]
    fn aggregate_includes_tasks_via_aggregate_fn() {
        // aggregate() must populate the tasks field.
        let agent = make_agent_with_tasks(
            "a0",
            vec![make_task_wire("ping", 10, 0, vec![[5, 10]], 5.0)],
        );
        let agg = aggregate(&[agent]);
        assert_eq!(agg.tasks.len(), 1);
        assert_eq!(agg.tasks[0].name, "ping");
        assert_eq!(agg.tasks[0].requests, 10);
    }
}
