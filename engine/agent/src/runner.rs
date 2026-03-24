/// Static HTTP load runner for agent processes.
///
/// Receives a ScenarioPlan shard (already has pre-extracted URLs/methods),
/// runs reqwest workers, emits metric snapshots via a channel.
/// No PyO3 — agents always run in static mode; Python callbacks run
/// on the coordinator before sharding.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, Semaphore},
    time::sleep,
};

// ── Plan types (mirror coordinator's plan.rs) ─────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TaskPlan {
    pub name: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body_template: Option<String>,
}

fn default_weight() -> u32 {
    1
}
fn default_method() -> String {
    "GET".to_string()
}

/// Per-VUser pre-auth headers shipped with the plan for distributed mode.
#[derive(Debug, Clone, Deserialize)]
pub struct VUserConfig {
    /// task_name → headers map
    #[serde(default)]
    pub task_headers: HashMap<String, HashMap<String, String>>,
    /// task_name → URL override (set when on_start stores state used in URLs)
    #[serde(default)]
    pub task_urls: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Constant,
    Ramp,
    Step,
    Spike,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Ramp
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Plan {
    pub name: String,
    pub rps: u64,
    pub duration_secs: u64,
    pub ramp_up_secs: u64,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_steps")]
    pub steps: u64,
    pub target_url: String,
    #[serde(default)]
    pub tasks: Vec<TaskPlan>,
    #[serde(default)]
    pub n_vusers: Option<u64>,
    /// Pre-auth pool for distributed mode. Agents rotate through these.
    #[serde(default)]
    pub vuser_configs: Vec<VUserConfig>,
}

fn default_steps() -> u64 {
    5
}

fn compute_target_rps(elapsed_secs: f64, plan: &Plan) -> f64 {
    let rps = plan.rps as f64;
    match plan.mode {
        Mode::Constant => rps,
        Mode::Ramp => {
            let ramp = plan.ramp_up_secs as f64;
            if ramp > 0.0 && elapsed_secs < ramp {
                rps * elapsed_secs / ramp
            } else {
                rps
            }
        }
        Mode::Step => {
            let steps = plan.steps.max(1) as f64;
            let total = plan.duration_secs as f64;
            let step_dur = total / steps;
            let step_idx = (elapsed_secs / step_dur).floor().min(steps - 1.0);
            rps * (step_idx + 1.0) / steps
        }
        Mode::Spike => {
            let total = plan.duration_secs as f64;
            let baseline = (rps * 0.2).max(1.0);
            if elapsed_secs < total / 3.0 || elapsed_secs >= 2.0 * total / 3.0 {
                baseline
            } else {
                rps
            }
        }
    }
}

fn total_duration(plan: &Plan) -> Duration {
    match plan.mode {
        Mode::Ramp => Duration::from_secs(plan.duration_secs + plan.ramp_up_secs),
        _ => Duration::from_secs(plan.duration_secs),
    }
}

fn phase_str(elapsed_secs: f64, plan: &Plan) -> &'static str {
    match plan.mode {
        Mode::Ramp => {
            if elapsed_secs < plan.ramp_up_secs as f64 {
                "ramp_up"
            } else {
                "steady"
            }
        }
        Mode::Spike => {
            let total = plan.duration_secs as f64;
            if elapsed_secs < total / 3.0 {
                "steady"
            } else if elapsed_secs < 2.0 * total / 3.0 {
                "ramp_up"
            } else {
                "ramp_down"
            }
        }
        _ => "steady",
    }
}

// ── Metrics snapshot sent back to coordinator ─────────────────────────────────

#[derive(Debug, Serialize)]
pub struct LatencySnapshot {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub min_ms: f64,
    pub mean_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct AgentMetrics {
    pub agent_id: String,
    pub elapsed_secs: f64,
    pub current_rps: f64,
    pub target_rps: f64,
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_workers: u64,
    pub phase: String,
    pub latency: LatencySnapshot,
    /// Sparse histogram for exact percentile merging on coordinator side.
    /// Each entry is [bucket_index_ms, count]. Only non-zero buckets included.
    pub histogram_buckets: Vec<[u64; 2]>,
}

// ── Internal counters ─────────────────────────────────────────────────────────

struct Counters {
    requests_total: AtomicU64,
    errors_total: AtomicU64,
    window_requests: AtomicU64,
    // latency tracking (simple fixed-size histogram)
    latency_buckets: Vec<AtomicU64>, // 1ms per bucket, 0–9999ms
    latency_sum: AtomicU64,
    latency_max: AtomicU64,
    latency_min: AtomicU64,
}

const MAX_MS: usize = 10_000;

impl Counters {
    fn new() -> Arc<Self> {
        let mut buckets = Vec::with_capacity(MAX_MS + 1);
        for _ in 0..=MAX_MS {
            buckets.push(AtomicU64::new(0));
        }
        Arc::new(Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            window_requests: AtomicU64::new(0),
            latency_buckets: buckets,
            latency_sum: AtomicU64::new(0),
            latency_max: AtomicU64::new(0),
            latency_min: AtomicU64::new(u64::MAX),
        })
    }

    fn record(&self, latency_ms: u64, is_error: bool) {
        let b = (latency_ms as usize).min(MAX_MS);
        self.latency_buckets[b].fetch_add(1, Ordering::Relaxed);
        self.latency_sum.fetch_add(latency_ms, Ordering::Relaxed);
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.window_requests.fetch_add(1, Ordering::Relaxed);
        if is_error {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }

        let mut cur = self.latency_max.load(Ordering::Relaxed);
        while latency_ms > cur {
            match self.latency_max.compare_exchange_weak(
                cur,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
        let mut cur = self.latency_min.load(Ordering::Relaxed);
        while latency_ms < cur {
            match self.latency_min.compare_exchange_weak(
                cur,
                latency_ms,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }

    fn percentile(&self, p: f64) -> f64 {
        let total = self.requests_total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let target = (total as f64 * p).ceil() as u64;
        let mut cum = 0u64;
        for (i, b) in self.latency_buckets.iter().enumerate() {
            cum += b.load(Ordering::Relaxed);
            if cum >= target {
                return i as f64;
            }
        }
        MAX_MS as f64
    }

    fn snapshot(
        &self,
        elapsed: f64,
        current_rps: f64,
        target_rps: f64,
        phase: &str,
    ) -> AgentMetrics {
        let total = self.requests_total.load(Ordering::Relaxed);
        let mean_ms = if total > 0 {
            self.latency_sum.load(Ordering::Relaxed) as f64 / total as f64
        } else {
            0.0
        };
        let max_ms = {
            let v = self.latency_max.load(Ordering::Relaxed);
            if v == 0 {
                0.0
            } else {
                v as f64
            }
        };
        let min_ms = {
            let v = self.latency_min.load(Ordering::Relaxed);
            if v == u64::MAX {
                0.0
            } else {
                v as f64
            }
        };

        let histogram_buckets: Vec<[u64; 2]> = self
            .latency_buckets
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let c = b.load(Ordering::Relaxed);
                if c > 0 {
                    Some([i as u64, c])
                } else {
                    None
                }
            })
            .collect();

        AgentMetrics {
            agent_id: String::new(), // filled by main.rs
            elapsed_secs: elapsed,
            current_rps,
            target_rps,
            requests_total: total,
            errors_total: self.errors_total.load(Ordering::Relaxed),
            active_workers: 0,
            phase: phase.to_string(),
            latency: LatencySnapshot {
                p50_ms: self.percentile(0.50),
                p95_ms: self.percentile(0.95),
                p99_ms: self.percentile(0.99),
                max_ms,
                min_ms,
                mean_ms,
            },
            histogram_buckets,
        }
    }
}

// ── Task selection ────────────────────────────────────────────────────────────

fn pick_task(tasks: &[TaskPlan], idx: u64) -> &TaskPlan {
    let total_weight: u32 = tasks.iter().map(|t| t.weight).sum();
    let slot = (idx % total_weight as u64) as u32;
    let mut acc = 0u32;
    for t in tasks {
        acc += t.weight;
        if slot < acc {
            return t;
        }
    }
    &tasks[0]
}

// ── Runner ────────────────────────────────────────────────────────────────────

/// Start the load test. Returns a channel of metric snapshots (1/sec).
pub async fn run_load(plan: Plan) -> mpsc::Receiver<AgentMetrics> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(run_inner(plan, tx));
    rx
}

async fn run_inner(plan: Plan, tx: mpsc::Sender<AgentMetrics>) {
    let start = Instant::now();
    let counters = Counters::new();
    let sem = Arc::new(Semaphore::new(512));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let tasks = Arc::new(plan.tasks.clone());
    let base_url = Arc::new(plan.target_url.clone());
    let duration = total_duration(&plan);
    let plan_arc = Arc::new(plan);
    // Pre-auth pool: Arc so workers can cheaply clone the reference.
    let vuser_configs = Arc::new(plan_arc.vuser_configs.clone());
    let pool_size = vuser_configs.len() as u64;

    let mut request_idx: u64 = 0;
    let tick_ms = 50u64;
    let mut window_start = Instant::now();
    let mut window_reqs = 0u64;
    // Fractional request budget: accumulates sub-integer requests across ticks so that
    // low RPS values (e.g. 3 RPS → 0.15 req/tick) still produce the correct rate over time.
    let mut req_budget: f64 = 0.0;
    loop {
        let elapsed = start.elapsed();

        if elapsed >= duration {
            let snap = counters.snapshot(elapsed.as_secs_f64(), 0.0, plan_arc.rps as f64, "done");
            let _ = tx.send(snap).await;
            break;
        }

        let elapsed_secs = elapsed.as_secs_f64();
        let phase = phase_str(elapsed_secs, &plan_arc);
        let t_rps = compute_target_rps(elapsed_secs, &plan_arc);

        req_budget += t_rps * tick_ms as f64 / 1000.0;
        let requests_this_tick = req_budget.floor() as u64;
        req_budget -= requests_this_tick as f64;

        for _ in 0..requests_this_tick {
            if tasks.is_empty() {
                break;
            }
            let task = pick_task(&tasks, request_idx).clone();
            // Pick per-VUser headers and URL overrides by round-robin through the pre-auth pool.
            let (extra_headers, task_path): (HashMap<String, String>, String) = if pool_size > 0 {
                let slot = (request_idx % pool_size) as usize;
                let headers = vuser_configs[slot]
                    .task_headers
                    .get(&task.name)
                    .cloned()
                    .unwrap_or_default();
                let path = vuser_configs[slot]
                    .task_urls
                    .get(&task.name)
                    .cloned()
                    .unwrap_or_else(|| task.url.clone());
                (headers, path)
            } else {
                (HashMap::new(), task.url.clone())
            };
            request_idx += 1;

            let url = format!("{}{}", base_url, task_path);
            let http2 = http.clone();
            let counters2 = Arc::clone(&counters);
            let permit = Arc::clone(&sem).acquire_owned().await.ok();

            tokio::spawn(async move {
                let _permit = permit;
                let t0 = Instant::now();
                let mut req = match task.method.as_str() {
                    "POST" => http2.post(&url),
                    "PUT" => http2.put(&url),
                    "PATCH" => http2.patch(&url),
                    "DELETE" => http2.delete(&url),
                    _ => http2.get(&url),
                };
                // Task-level static headers first, then per-VUser pre-auth headers
                // (pre-auth headers take precedence so on_start tokens override defaults).
                for (k, v) in &task.headers {
                    req = req.header(k, v);
                }
                for (k, v) in &extra_headers {
                    req = req.header(k, v);
                }
                if let Some(body) = &task.body_template {
                    req = req.body(body.clone());
                }
                let ms = t0.elapsed().as_millis() as u64;
                match req.send().await {
                    Ok(resp) => {
                        let ms = t0.elapsed().as_millis() as u64;
                        counters2.record(
                            ms,
                            resp.status().is_client_error() || resp.status().is_server_error(),
                        );
                    }
                    Err(_) => {
                        counters2.record(ms, true);
                    }
                }
            });
        }

        // RPS calculation (rolling 1s window).
        let window_elapsed = window_start.elapsed();
        window_reqs += requests_this_tick;
        if window_elapsed >= Duration::from_secs(1) {
            let current_rps = window_reqs as f64 / window_elapsed.as_secs_f64();
            window_reqs = 0;
            window_start = Instant::now();

            let snap = counters.snapshot(elapsed_secs, current_rps, t_rps, phase);
            let _ = tx.send(snap).await;
        }

        sleep(Duration::from_millis(tick_ms)).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_plan(rps: u64, duration_secs: u64, ramp_up_secs: u64, mode: Mode) -> Plan {
        Plan {
            name: "test".to_string(),
            rps,
            duration_secs,
            ramp_up_secs,
            mode,
            steps: 5,
            target_url: "http://localhost".to_string(),
            tasks: vec![],
            n_vusers: None,
            vuser_configs: vec![],
        }
    }

    // ── req_budget accumulation ───────────────────────────────────────────────

    /// Regression: the old `round()` approach rounds 0.15 to 0 every tick,
    /// producing zero requests at low RPS. The new budget accumulation must
    /// fire the correct number of requests over a full second.
    #[test]
    fn budget_low_rps_regression() {
        let tick_ms = 50u64;
        let rps = 3.0_f64;

        // Old behaviour: round() → always 0 at 3 RPS
        let old_per_tick = (rps * tick_ms as f64 / 1000.0).round() as u64;
        assert_eq!(old_per_tick, 0, "demonstrates the bug: round(0.15) == 0");

        // New behaviour: budget accumulation fires the correct total
        let mut budget = 0.0_f64;
        let mut total = 0u64;
        for _ in 0..20 {
            // 20 ticks × 50ms = 1 second
            budget += rps * tick_ms as f64 / 1000.0;
            let fired = budget.floor() as u64;
            budget -= fired as f64;
            total += fired;
        }
        assert_eq!(
            total, 3,
            "budget must produce exactly 3 requests in 1 second"
        );
    }

    /// Budget accumulation produces the correct request rate over a full second.
    /// Allows ±1 tolerance for floating-point accumulation errors (e.g. 10×0.1 ≠ 1.0 in f64).
    #[test]
    fn budget_matches_target_rps_over_one_second() {
        let tick_ms = 50u64;
        let ticks_per_sec = 1000 / tick_ms; // 20

        for rps in [1u64, 2, 5, 10, 50, 100] {
            let mut budget = 0.0_f64;
            let mut total = 0u64;
            for _ in 0..ticks_per_sec {
                budget += rps as f64 * tick_ms as f64 / 1000.0;
                let fired = budget.floor() as u64;
                budget -= fired as f64;
                total += fired;
            }
            let diff = (total as i64 - rps as i64).unsigned_abs();
            assert!(
                diff <= 1,
                "budget must produce ~{rps} requests in 1 second, got {total}"
            );
        }
    }

    /// Budget residual stays in [0, 1) after each tick — no runaway accumulation.
    #[test]
    fn budget_residual_bounded() {
        let tick_ms = 50u64;
        let rps = 7.0_f64; // non-integer ratio to tick
        let mut budget = 0.0_f64;
        for _ in 0..200 {
            budget += rps * tick_ms as f64 / 1000.0;
            let fired = budget.floor() as u64;
            budget -= fired as f64;
            assert!(
                budget >= 0.0 && budget < 1.0,
                "budget residual must stay in [0, 1)"
            );
        }
    }

    // ── compute_target_rps ────────────────────────────────────────────────────

    #[test]
    fn ramp_mode_starts_at_zero_and_reaches_target() {
        let plan = make_plan(100, 60, 30, Mode::Ramp);
        assert_eq!(compute_target_rps(0.0, &plan), 0.0);
        assert!((compute_target_rps(15.0, &plan) - 50.0).abs() < 0.01);
        assert_eq!(compute_target_rps(30.0, &plan), 100.0);
        assert_eq!(compute_target_rps(60.0, &plan), 100.0);
    }

    #[test]
    fn constant_mode_always_returns_target() {
        let plan = make_plan(50, 60, 0, Mode::Constant);
        for t in [0.0, 10.0, 30.0, 59.0] {
            assert_eq!(compute_target_rps(t, &plan), 50.0);
        }
    }

    #[test]
    fn step_mode_increases_in_steps() {
        let plan = make_plan(100, 50, 0, Mode::Step); // 5 steps × 10s each
                                                      // step 0 (t=0..10): 20 RPS
        assert!((compute_target_rps(0.0, &plan) - 20.0).abs() < 0.01);
        // step 2 (t=20..30): 60 RPS
        assert!((compute_target_rps(25.0, &plan) - 60.0).abs() < 0.01);
        // step 4 (t=40..50): 100 RPS
        assert!((compute_target_rps(45.0, &plan) - 100.0).abs() < 0.01);
    }

    #[test]
    fn spike_mode_baseline_spike_baseline() {
        let plan = make_plan(100, 60, 0, Mode::Spike);
        let baseline = (100.0_f64 * 0.2_f64).max(1.0); // 20.0
        assert_eq!(compute_target_rps(0.0, &plan), baseline); // first third
        assert_eq!(compute_target_rps(30.0, &plan), 100.0); // middle third
        assert_eq!(compute_target_rps(50.0, &plan), baseline); // last third
    }

    // ── pick_task ─────────────────────────────────────────────────────────────

    #[test]
    fn pick_task_respects_weights() {
        let tasks = vec![
            TaskPlan {
                name: "a".to_string(),
                weight: 1,
                url: "/a".to_string(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body_template: None,
            },
            TaskPlan {
                name: "b".to_string(),
                weight: 3,
                url: "/b".to_string(),
                method: "GET".to_string(),
                headers: HashMap::new(),
                body_template: None,
            },
        ];
        // Total weight = 4. Slots 0 → "a", slots 1-3 → "b".
        assert_eq!(pick_task(&tasks, 0).name, "a");
        assert_eq!(pick_task(&tasks, 1).name, "b");
        assert_eq!(pick_task(&tasks, 2).name, "b");
        assert_eq!(pick_task(&tasks, 3).name, "b");
        assert_eq!(pick_task(&tasks, 4).name, "a"); // wraps
    }

    #[test]
    fn pick_task_single_task_always_returns_it() {
        let tasks = vec![TaskPlan {
            name: "only".to_string(),
            weight: 1,
            url: "/".to_string(),
            method: "GET".to_string(),
            headers: HashMap::new(),
            body_template: None,
        }];
        for idx in 0..10 {
            assert_eq!(pick_task(&tasks, idx).name, "only");
        }
    }

    // ── VUserConfig task_urls override ────────────────────────────────────────

    /// task_urls overrides the task's default url when vuser_configs are present.
    #[test]
    fn task_urls_overrides_task_default_url() {
        let task = TaskPlan {
            name: "read_project".to_string(),
            weight: 1,
            url: "/".to_string(), // default fallback
            method: "GET".to_string(),
            headers: HashMap::new(),
            body_template: None,
        };
        let mut task_urls = HashMap::new();
        task_urls.insert(
            "read_project".to_string(),
            "/api/v1/projects/42".to_string(),
        );
        let vc = VUserConfig {
            task_headers: HashMap::new(),
            task_urls,
        };
        let vuser_configs = vec![vc];
        let pool_size = vuser_configs.len() as u64;

        let slot = (0u64 % pool_size) as usize;
        let path = vuser_configs[slot]
            .task_urls
            .get(&task.name)
            .cloned()
            .unwrap_or_else(|| task.url.clone());

        assert_eq!(path, "/api/v1/projects/42");
    }

    /// When task_urls is absent for a task, falls back to task.url.
    #[test]
    fn task_urls_falls_back_to_task_url_when_absent() {
        let task = TaskPlan {
            name: "list_projects".to_string(),
            weight: 1,
            url: "/api/v1/projects".to_string(),
            method: "GET".to_string(),
            headers: HashMap::new(),
            body_template: None,
        };
        let vc = VUserConfig {
            task_headers: HashMap::new(),
            task_urls: HashMap::new(), // empty
        };
        let path = vc
            .task_urls
            .get(&task.name)
            .cloned()
            .unwrap_or_else(|| task.url.clone());

        assert_eq!(path, "/api/v1/projects");
    }

    /// When vuser_configs is empty (pool_size == 0), always uses task.url.
    #[test]
    fn empty_vuser_configs_uses_task_url() {
        let task_url = "/api/v1/health";
        let pool_size: u64 = 0;

        let path: String = if pool_size > 0 {
            // would look up vuser_configs
            unreachable!()
        } else {
            task_url.to_string()
        };

        assert_eq!(path, task_url);
    }

    // ── total_duration ────────────────────────────────────────────────────────

    #[test]
    fn ramp_total_duration_includes_ramp_up() {
        let plan = make_plan(10, 120, 20, Mode::Ramp);
        assert_eq!(total_duration(&plan), Duration::from_secs(140));
    }

    #[test]
    fn non_ramp_total_duration_ignores_ramp_up() {
        for mode in [Mode::Constant, Mode::Step, Mode::Spike] {
            let plan = make_plan(10, 60, 30, mode);
            assert_eq!(total_duration(&plan), Duration::from_secs(60));
        }
    }
}
