/// Wire types, NATS subjects, and metrics aggregation functions.
use serde::{Deserialize, Serialize};

use crate::{
    coordinator::{MetricsSnapshot, Phase, SharedSnapshot},
    metrics::{LatencySnapshot, TaskSnapshot},
    plan::ScenarioPlan,
};

// ── NATS subjects ──────────────────────────────────────────────────────────────

pub(super) fn subject_register() -> &'static str {
    "loadpilot.register"
}
pub(super) fn subject_shard(agent_id: &str) -> String {
    format!("loadpilot.shard.{agent_id}")
}
pub(super) fn subject_metrics() -> &'static str {
    "loadpilot.metrics.>"
}
pub(super) fn subject_control() -> &'static str {
    "loadpilot.control"
}

// ── Wire messages ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RegisterMsg {
    pub agent_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ShardMsg {
    pub agent_id: String,
    pub plan: ScenarioPlan,
    /// Unix timestamp (ms) at which all agents should start the test simultaneously.
    /// Coordinator sets this to `now + 2000ms` so agents can synchronise clocks.
    pub start_at_unix_ms: u64,
}

/// Wire type for per-task data received from agents (matches agent's TaskWireSnapshot).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct TaskWireSnapshot {
    pub name: String,
    pub requests: u64,
    pub errors: u64,
    pub latency: LatencySnapshot,
    pub histogram_buckets: Vec<[u64; 2]>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AgentMetricsMsg {
    pub agent_id: String,
    pub elapsed_secs: f64,
    pub current_rps: f64,
    pub target_rps: f64,
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_workers: u64,
    pub phase: String,
    pub latency: LatencySnapshot,
    /// Sparse histogram: each entry is [bucket_index_ms, count].
    #[serde(default)]
    pub histogram_buckets: Vec<[u64; 2]>,
    /// Per-task breakdown.
    #[serde(default)]
    pub task_snapshots: Vec<TaskWireSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ControlMsg {
    pub command: String, // "stop"
}

// ── Aggregated snapshot (same shape as coordinator stdout) ────────────────────

#[derive(Debug, Serialize)]
pub(super) struct AggregatedSnapshot {
    pub timestamp_secs: f64,
    pub elapsed_secs: f64,
    pub current_rps: f64,
    pub target_rps: f64,
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_workers: u64,
    pub phase: String,
    pub latency: LatencySnapshot,
    /// Per-task breakdown aggregated across all agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskSnapshot>,
}

// ── Aggregation helpers ────────────────────────────────────────────────────────

pub(super) const MAX_BUCKET: usize = 10_000;

pub(super) fn percentile_from_merged(buckets: &[u64; MAX_BUCKET + 1], total: u64, p: f64) -> f64 {
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
pub(super) fn aggregate_tasks(snapshots: &[AgentMetricsMsg]) -> Vec<TaskSnapshot> {
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

pub(super) fn aggregate(snapshots: &[AgentMetricsMsg]) -> AggregatedSnapshot {
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

pub(super) fn update_prometheus(shared: &SharedSnapshot, agg: &AggregatedSnapshot) {
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
