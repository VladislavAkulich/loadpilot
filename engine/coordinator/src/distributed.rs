/// Distributed load test coordinator.
///
/// Three modes:
///   run()                — embedded NATS broker + N spawned local agents  (original)
///   run_external_agents()— embedded NATS broker, wait for agents to connect externally
///   run_with_nats_url()  — connect to external NATS, wait for remote agents (Railway etc.)
mod wire;

#[cfg(test)]
mod tests;

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use tokio::{
    process::Command,
    sync::watch,
    time::{interval, sleep, timeout},
};

use crate::{broker, coordinator::SharedSnapshot, metrics::MetricSink, plan::ScenarioPlan};

use wire::{
    aggregate, subject_control, subject_metrics, subject_register, subject_shard, update_prometheus,
    AgentMetricsMsg, ControlMsg, RegisterMsg, ShardMsg,
};

// ── Plan sharding ──────────────────────────────────────────────────────────────

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

// ── Agent binary discovery ─────────────────────────────────────────────────────

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

// ── Embedded broker coordination (shared by run + run_external_agents) ─────────

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

// ── Public API ─────────────────────────────────────────────────────────────────

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
