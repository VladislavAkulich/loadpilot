use crate::metrics::LatencySnapshot;

use super::wire::{
    aggregate, aggregate_tasks, percentile_from_merged, AgentMetricsMsg, TaskWireSnapshot,
    MAX_BUCKET,
};

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

// ── percentile_from_merged ────────────────────────────────────────────────────

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

// ── aggregate: histogram merging ──────────────────────────────────────────────

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

// ── aggregate_tasks ────────────────────────────────────────────────────────────

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
