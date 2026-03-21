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

use anyhow::Result;
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

fn default_weight() -> u32 { 1 }
fn default_method() -> String { "GET".to_string() }

/// Per-VUser pre-auth headers shipped with the plan for distributed mode.
#[derive(Debug, Clone, Deserialize)]
pub struct VUserConfig {
    /// task_name → headers map
    #[serde(default)]
    pub task_headers: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode { Constant, Ramp, Step, Spike }

impl Default for Mode { fn default() -> Self { Mode::Ramp } }

#[derive(Debug, Clone, Deserialize)]
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

fn default_steps() -> u64 { 5 }

fn compute_target_rps(elapsed_secs: f64, plan: &Plan) -> f64 {
    let rps = plan.rps as f64;
    match plan.mode {
        Mode::Constant => rps,
        Mode::Ramp => {
            let ramp = plan.ramp_up_secs as f64;
            if ramp > 0.0 && elapsed_secs < ramp { rps * elapsed_secs / ramp } else { rps }
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
            if elapsed_secs < plan.ramp_up_secs as f64 { "ramp_up" } else { "steady" }
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
        for _ in 0..=MAX_MS { buckets.push(AtomicU64::new(0)); }
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
        if is_error { self.errors_total.fetch_add(1, Ordering::Relaxed); }

        let mut cur = self.latency_max.load(Ordering::Relaxed);
        while latency_ms > cur {
            match self.latency_max.compare_exchange_weak(cur, latency_ms, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break, Err(v) => cur = v,
            }
        }
        let mut cur = self.latency_min.load(Ordering::Relaxed);
        while latency_ms < cur {
            match self.latency_min.compare_exchange_weak(cur, latency_ms, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break, Err(v) => cur = v,
            }
        }
    }

    fn percentile(&self, p: f64) -> f64 {
        let total = self.requests_total.load(Ordering::Relaxed);
        if total == 0 { return 0.0; }
        let target = (total as f64 * p).ceil() as u64;
        let mut cum = 0u64;
        for (i, b) in self.latency_buckets.iter().enumerate() {
            cum += b.load(Ordering::Relaxed);
            if cum >= target { return i as f64; }
        }
        MAX_MS as f64
    }

    fn snapshot(&self, elapsed: f64, current_rps: f64, target_rps: f64, phase: &str) -> AgentMetrics {
        let total = self.requests_total.load(Ordering::Relaxed);
        let mean_ms = if total > 0 {
            self.latency_sum.load(Ordering::Relaxed) as f64 / total as f64
        } else { 0.0 };
        let max_ms = { let v = self.latency_max.load(Ordering::Relaxed); if v == 0 { 0.0 } else { v as f64 } };
        let min_ms = { let v = self.latency_min.load(Ordering::Relaxed); if v == u64::MAX { 0.0 } else { v as f64 } };

        let histogram_buckets: Vec<[u64; 2]> = self.latency_buckets
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let c = b.load(Ordering::Relaxed);
                if c > 0 { Some([i as u64, c]) } else { None }
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
                max_ms, min_ms, mean_ms,
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
        if slot < acc { return t; }
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
    let mut current_rps = 0.0f64;

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

        let requests_this_tick = (t_rps * tick_ms as f64 / 1000.0).round() as u64;

        for _ in 0..requests_this_tick {
            if tasks.is_empty() { break; }
            let task = pick_task(&tasks, request_idx).clone();
            // Pick per-VUser headers by round-robin through the pre-auth pool.
            let extra_headers: HashMap<String, String> = if pool_size > 0 {
                let slot = (request_idx % pool_size) as usize;
                vuser_configs[slot]
                    .task_headers
                    .get(&task.name)
                    .cloned()
                    .unwrap_or_default()
            } else {
                HashMap::new()
            };
            request_idx += 1;

            let url = format!("{}{}", base_url, task.url);
            let http2 = http.clone();
            let counters2 = Arc::clone(&counters);
            let permit = Arc::clone(&sem).acquire_owned().await.ok();

            tokio::spawn(async move {
                let _permit = permit;
                let t0 = Instant::now();
                let mut req = match task.method.as_str() {
                    "POST" => http2.post(&url),
                    "PUT"  => http2.put(&url),
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
                        counters2.record(ms, resp.status().is_client_error() || resp.status().is_server_error());
                    }
                    Err(_) => { counters2.record(ms, true); }
                }
            });
        }

        // RPS calculation (rolling 1s window).
        let window_elapsed = window_start.elapsed();
        window_reqs += requests_this_tick;
        if window_elapsed >= Duration::from_secs(1) {
            current_rps = window_reqs as f64 / window_elapsed.as_secs_f64();
            window_reqs = 0;
            window_start = Instant::now();

            let snap = counters.snapshot(elapsed_secs, current_rps, t_rps, phase);
            let _ = tx.send(snap).await;
        }

        sleep(Duration::from_millis(tick_ms)).await;
    }
}
