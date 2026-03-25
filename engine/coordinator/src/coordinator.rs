/// Coordinator: receives the test plan, spawns tokio workers, aggregates metrics,
/// and streams JSON metric lines to stdout every second.
///
/// When the plan contains `scenario_file` / `scenario_class`, the PyO3 bridge
/// is activated:
///   • on_start is called for each VUser (staggered over the ramp-up window).
///   • Every HTTP request is parameterised by calling the Python task callback
///     with a MockClient; the captured (method, url, headers, body) is then
///     executed by reqwest.
///   • on_stop is called for each VUser after the test ends.
///
/// Without Python bridge fields the coordinator falls back to the static URL /
/// method from the plan (original behaviour, maximum Rust throughput).
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use std::sync::RwLock;

use crate::metrics::{LatencySnapshot, MetricSink, Metrics, TaskSnapshot};
use crate::plan::{HttpMethod, Mode, ScenarioPlan, TaskPlan};
use crate::python_bridge::PythonBridge;

pub type SharedSnapshot = Arc<RwLock<Option<MetricsSnapshot>>>;

/// Phase of the load test.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    RampUp,
    Steady,
    RampDown,
    Done,
}

/// A snapshot of metrics for one reporting interval.
#[derive(serde::Serialize, Clone)]
pub struct MetricsSnapshot {
    pub timestamp_secs: f64,
    pub elapsed_secs: f64,
    pub current_rps: f64,
    pub target_rps: f64,
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_workers: u64,
    pub latency: LatencySnapshot,
    pub phase: Phase,
    /// Per-task breakdown; empty when running single-task plans.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskSnapshot>,
}

pub struct Coordinator {
    plan: ScenarioPlan,
    metrics: Metrics,
}

impl Coordinator {
    pub fn new(plan: ScenarioPlan) -> Self {
        Self {
            plan,
            metrics: Metrics::new(),
        }
    }

    pub async fn run(self, shared_snapshot: SharedSnapshot, sink: MetricSink) -> Result<()> {
        anyhow::ensure!(!self.plan.tasks.is_empty(), "plan contains no tasks");
        let plan = Arc::new(self.plan);
        let metrics = self.metrics;

        let start = Instant::now();
        let _ramp_up = Duration::from_secs(plan.ramp_up_secs);
        // For ramp mode, total = steady duration + ramp-up window.
        // For all other modes, ramp_up_secs is unused and total = duration_secs.
        let total = match plan.mode {
            Mode::Ramp => Duration::from_secs(plan.duration_secs + plan.ramp_up_secs),
            _ => Duration::from_secs(plan.duration_secs),
        };

        let done_flag = Arc::new(AtomicBool::new(false));
        let active_workers = Arc::new(AtomicU64::new(0));

        // Little's Law: concurrency = RPS × expected_latency.
        // We budget 200ms of in-flight tolerance, capped at 2000 to prevent
        // connection storms at high RPS targets.
        let max_concurrency = ((plan.rps as f64 * 0.2) as usize).clamp(50, 2000);
        let sem = Arc::new(Semaphore::new(max_concurrency));

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(max_concurrency.min(512))
            .build()?;

        // ── PyO3 bridge (optional) ────────────────────────────────────────────
        // Wrap in Arc<Mutex<>> so it can be cloned into tokio::spawn closures
        // and accessed from spawn_blocking threads.
        let bridge: Option<Arc<PythonBridge>> =
            if let (Some(sf), Some(sc)) = (&plan.scenario_file, &plan.scenario_class) {
                let n = plan.n_vusers.unwrap_or(5) as usize;
                match PythonBridge::new(
                    sf,
                    sc,
                    n,
                    &plan.target_url,
                    http_client.clone(),
                    tokio::runtime::Handle::current(),
                ) {
                    Ok(b) => {
                        tracing::info!(vusers = n, "PyO3 bridge ready");
                        Some(Arc::new(b))
                    }
                    Err(e) => {
                        tracing::warn!("PyO3 bridge init failed: {e} — using static URLs");
                        None
                    }
                }
            } else {
                None
            };

        // Track how many VUsers have completed on_start and are ready to serve requests.
        let ready_vusers = Arc::new(AtomicUsize::new(0));

        // ── Staggered on_start ────────────────────────────────────────────────
        // Runs concurrently with the scheduler so VUsers become available
        // progressively during the ramp-up window.
        if let Some(ref b) = bridge {
            let b = Arc::clone(b);
            let ready = Arc::clone(&ready_vusers);
            let n = b.n_vusers();
            // Space on_start calls evenly across the ramp-up window.
            let stagger_ms = if n > 1 && plan.ramp_up_secs > 0 {
                plan.ramp_up_secs * 1000 / (n as u64 - 1)
            } else {
                0
            };

            tokio::spawn(async move {
                for i in 0..n {
                    let b_clone = Arc::clone(&b);
                    match b_clone.call_on_start(i).await {
                        Ok(()) => {}
                        Err(e) => tracing::warn!(vuser = i, "on_start error: {e}"),
                    }

                    ready.fetch_add(1, Ordering::Release);

                    if stagger_ms > 0 && i < n - 1 {
                        sleep(Duration::from_millis(stagger_ms)).await;
                    }
                }
            });
        } else {
            // No bridge — all "VUsers" are immediately ready (static mode).
            ready_vusers.store(1, Ordering::Release);
        }

        // ── Metrics reporter ──────────────────────────────────────────────────
        let metrics_reporter = metrics.clone();
        let plan_clone = plan.clone();
        let active_clone = active_workers.clone();
        let done_clone = done_flag.clone();
        let snapshot_writer = Arc::clone(&shared_snapshot);
        let sink_reporter = Arc::clone(&sink);
        let reporter_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let elapsed = start.elapsed();
                let (phase, target_rps) = if elapsed >= total {
                    (Phase::Done, 0.0)
                } else {
                    compute_phase_and_rps(elapsed.as_secs_f64(), &plan_clone)
                };

                let (window_reqs, _) = metrics_reporter.drain_window();
                let snap = MetricsSnapshot {
                    timestamp_secs: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64(),
                    elapsed_secs: elapsed.as_secs_f64(),
                    current_rps: window_reqs as f64,
                    target_rps,
                    requests_total: metrics_reporter.requests_total.load(Ordering::Relaxed),
                    errors_total: metrics_reporter.errors_total.load(Ordering::Relaxed),
                    active_workers: active_clone.load(Ordering::Relaxed),
                    latency: LatencySnapshot {
                        p50_ms: metrics_reporter.histogram.percentile(0.50),
                        p95_ms: metrics_reporter.histogram.percentile(0.95),
                        p99_ms: metrics_reporter.histogram.percentile(0.99),
                        max_ms: metrics_reporter.histogram.max_ms(),
                        min_ms: metrics_reporter.histogram.min_ms(),
                        mean_ms: metrics_reporter.histogram.mean_ms(),
                    },
                    phase,
                    tasks: metrics_reporter.task_snapshots(),
                };

                if let Ok(line) = serde_json::to_string(&snap) {
                    sink_reporter(line.clone());
                }

                if let Ok(mut guard) = snapshot_writer.write() {
                    *guard = Some(snap);
                }

                if done_clone.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        // ── Main scheduler loop ───────────────────────────────────────────────
        let tick_interval_ms = 50u64; // 20 ticks / second
        let mut ticker = tokio::time::interval(Duration::from_millis(tick_interval_ms));
        let mut last_tick = Instant::now();
        let mut dispatch_idx: usize = 0;
        let mut deficit = 0.0f64; // fractional request accumulator

        loop {
            ticker.tick().await;

            let elapsed = start.elapsed();
            if elapsed >= total {
                break;
            }

            let now = Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f64();
            last_tick = now;

            let (_, target_rps) = compute_phase_and_rps(elapsed.as_secs_f64(), &plan);

            deficit += target_rps * dt;
            let n_requests = deficit.floor() as usize;
            deficit -= n_requests as f64;
            let n_ready = ready_vusers.load(Ordering::Acquire);

            if n_ready == 0 {
                // VUsers are still initialising; wait.
                continue;
            }

            for i in 0..n_requests {
                let t = match pick_task(&plan.tasks, i) {
                    Some(t) => t,
                    None => continue,
                };
                let task_name = t.name.clone();

                let metrics_task = metrics.clone();
                let sem_clone = sem.clone();
                let active_clone2 = active_workers.clone();

                if let Some(ref b) = bridge {
                    // PyO3 path: run the full Python task with a real httpx client.
                    // http_client (reqwest) is not used here — Python drives HTTP via httpx.
                    // Each HTTP call the task makes is recorded separately in metrics.
                    // This handles both single-call and multi-call tasks transparently.
                    let b = Arc::clone(b);
                    let vuser_idx = dispatch_idx % n_ready;
                    dispatch_idx = dispatch_idx.wrapping_add(1);

                    tokio::spawn(async move {
                        let _permit = match sem_clone.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        active_clone2.fetch_add(1, Ordering::Relaxed);

                        match b.run_task(vuser_idx, task_name.clone()).await {
                            Ok(calls) => {
                                if calls.is_empty() {
                                    metrics_task.record_error_task(&task_name, 0);
                                } else {
                                    for cr in calls {
                                        if let Some(ref err) = cr.error {
                                            tracing::warn!(task = %task_name, "task error: {err}");
                                        }
                                        if cr.success {
                                            metrics_task
                                                .record_success_task(&task_name, cr.elapsed_ms);
                                        } else {
                                            metrics_task
                                                .record_error_task(&task_name, cr.elapsed_ms);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(task = %task_name, "run_task error: {e}");
                                metrics_task.record_error_task(&task_name, 0);
                            }
                        }
                        active_clone2.fetch_sub(1, Ordering::Relaxed);
                    });
                } else {
                    // Static path: URL / method come directly from the plan.
                    let client = http_client.clone();
                    let url = format!("{}{}", plan.target_url.trim_end_matches('/'), t.url);
                    let method = t.method; // Copy
                    let headers = t.headers.clone(); // Arc clone — O(1)
                    let body = t.body_template.clone();

                    tokio::spawn(async move {
                        let _permit = match sem_clone.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        active_clone2.fetch_add(1, Ordering::Relaxed);
                        let t0 = Instant::now();
                        let result =
                            execute_request(&client, method, &url, &headers, body.as_deref())
                                .await;
                        let latency_ms = t0.elapsed().as_millis() as u64;
                        match result {
                            Ok(r) if r.status < 400 => {
                                metrics_task.record_success_task(&task_name, latency_ms)
                            }
                            _ => metrics_task.record_error_task(&task_name, latency_ms),
                        }
                        active_clone2.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }

        // ── Teardown ──────────────────────────────────────────────────────────
        done_flag.store(true, Ordering::Relaxed);
        sleep(Duration::from_millis(1200)).await;

        // Call on_stop for all VUsers, then signal threads to exit.
        if let Some(ref b) = bridge {
            let n = b.n_vusers();
            for i in 0..n {
                if let Err(e) = b.call_on_stop(i).await {
                    tracing::warn!(vuser = i, "on_stop error: {e}");
                }
            }
            b.shutdown();
        }

        // Final "done" metrics snapshot.
        let elapsed = start.elapsed();
        let snap = MetricsSnapshot {
            timestamp_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            elapsed_secs: elapsed.as_secs_f64(),
            current_rps: 0.0,
            target_rps: 0.0,
            requests_total: metrics.requests_total.load(Ordering::Relaxed),
            errors_total: metrics.errors_total.load(Ordering::Relaxed),
            active_workers: 0,
            latency: LatencySnapshot {
                p50_ms: metrics.histogram.percentile(0.50),
                p95_ms: metrics.histogram.percentile(0.95),
                p99_ms: metrics.histogram.percentile(0.99),
                max_ms: metrics.histogram.max_ms(),
                min_ms: metrics.histogram.min_ms(),
                mean_ms: metrics.histogram.mean_ms(),
            },
            phase: Phase::Done,
            tasks: metrics.task_snapshots(),
        };
        if let Ok(line) = serde_json::to_string(&snap) {
            sink(line.clone());
        }

        let _ = reporter_handle.await;
        Ok(())
    }
}

/// Compute the current (phase, target_rps) for the given elapsed time and plan.
///
/// Modes:
///   constant — full RPS immediately, no ramp.
///   ramp     — linear ramp from 0 to target over ramp_up_secs, then steady.
///   step     — divide duration_secs into `steps` equal windows; each window
///              runs at rps * step_number / steps.
///   spike    — divide duration_secs into thirds: baseline (20% rps) →
///              peak (100% rps) → recovery (20% rps).
fn compute_phase_and_rps(elapsed_secs: f64, plan: &ScenarioPlan) -> (Phase, f64) {
    let rps = plan.rps as f64;
    match plan.mode {
        Mode::Constant => (Phase::Steady, rps),
        Mode::Ramp => {
            let ramp = plan.ramp_up_secs as f64;
            if ramp > 0.0 && elapsed_secs < ramp {
                (Phase::RampUp, rps * elapsed_secs / ramp)
            } else {
                (Phase::Steady, rps)
            }
        }
        Mode::Step => {
            let steps = plan.steps.max(1) as f64;
            let total = plan.duration_secs as f64;
            let step_dur = total / steps;
            let step_idx = (elapsed_secs / step_dur).floor().min(steps - 1.0);
            (Phase::Steady, rps * (step_idx + 1.0) / steps)
        }
        Mode::Spike => {
            let total = plan.duration_secs as f64;
            let t1 = total / 3.0;
            let t2 = 2.0 * total / 3.0;
            let baseline = (rps * 0.2).max(1.0);
            if elapsed_secs < t1 {
                (Phase::Steady, baseline)
            } else if elapsed_secs < t2 {
                (Phase::RampUp, rps)
            } else {
                (Phase::RampDown, baseline)
            }
        }
    }
}

fn pick_task<'a>(tasks: &'a [TaskPlan], index: usize) -> Option<&'a TaskPlan> {
    if tasks.is_empty() {
        return None;
    }
    let total_weight: u32 = tasks.iter().map(|t| t.weight).sum();
    if total_weight == 0 {
        return Some(&tasks[index % tasks.len()]);
    }
    let slot = (index as u32) % total_weight;
    let mut cumulative = 0u32;
    for t in tasks {
        cumulative += t.weight;
        if slot < cumulative {
            return Some(t);
        }
    }
    Some(&tasks[tasks.len() - 1])
}

/// Response data returned from a live HTTP request.
struct HttpResponse {
    status: u16,
    #[allow(dead_code)]
    headers: std::collections::HashMap<String, String>,
    #[allow(dead_code)]
    body: String,
}

async fn execute_request(
    client: &reqwest::Client,
    method: HttpMethod,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&str>,
) -> Result<HttpResponse> {
    let mut req = match method {
        HttpMethod::Get => client.get(url),
        HttpMethod::Post => client.post(url),
        HttpMethod::Put => client.put(url),
        HttpMethod::Patch => client.patch(url),
        HttpMethod::Delete => client.delete(url),
    };

    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let resp_headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    // Body intentionally not read in static mode — no check_* method runs here,
    // so allocating the response body on every request is unnecessary overhead.
    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::TaskPlan;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_task(name: &str, weight: u32) -> TaskPlan {
        TaskPlan {
            name: name.to_string(),
            weight,
            url: "/".to_string(),
            method: HttpMethod::Get,
            headers: Arc::new(HashMap::new()),
            body_template: None,
        }
    }

    // ── compute_phase_and_rps ─────────────────────────────────────────────────

    fn plan_for_mode(
        mode: Mode,
        rps: u64,
        duration: u64,
        ramp_up: u64,
        steps: u64,
    ) -> ScenarioPlan {
        ScenarioPlan {
            name: "T".into(),
            rps,
            duration_secs: duration,
            ramp_up_secs: ramp_up,
            mode,
            target_url: "http://localhost".into(),
            tasks: vec![],
            scenario_file: None,
            scenario_class: None,
            n_vusers: None,
            vuser_configs: vec![],
            steps,
        }
    }

    #[test]
    fn ramp_before_ramp_scales_linearly() {
        let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
        let (phase, rps) = compute_phase_and_rps(5.0, &plan);
        assert_eq!(phase, Phase::RampUp);
        assert!((rps - 50.0).abs() < 0.01);
    }

    #[test]
    fn ramp_after_ramp_is_full() {
        let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
        let (phase, rps) = compute_phase_and_rps(15.0, &plan);
        assert_eq!(phase, Phase::Steady);
        assert_eq!(rps, 100.0);
    }

    #[test]
    fn ramp_start_is_zero() {
        let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
        let (phase, rps) = compute_phase_and_rps(0.0, &plan);
        assert_eq!(phase, Phase::RampUp);
        assert_eq!(rps, 0.0);
    }

    #[test]
    fn constant_always_full_rps() {
        let plan = plan_for_mode(Mode::Constant, 100, 60, 10, 5);
        for t in [0.0, 1.0, 30.0, 59.9] {
            let (phase, rps) = compute_phase_and_rps(t, &plan);
            assert_eq!(phase, Phase::Steady);
            assert_eq!(rps, 100.0);
        }
    }

    #[test]
    fn step_first_step_is_fraction() {
        // 5 steps over 60s → each step = 12s. At t=5 (step 0): rps = 100*1/5 = 20.
        let plan = plan_for_mode(Mode::Step, 100, 60, 0, 5);
        let (_, rps) = compute_phase_and_rps(5.0, &plan);
        assert!((rps - 20.0).abs() < 0.01);
    }

    #[test]
    fn step_last_step_is_full_rps() {
        // At t=55 (step 4): rps = 100*5/5 = 100.
        let plan = plan_for_mode(Mode::Step, 100, 60, 0, 5);
        let (_, rps) = compute_phase_and_rps(55.0, &plan);
        assert!((rps - 100.0).abs() < 0.01);
    }

    #[test]
    fn spike_middle_third_is_peak() {
        // duration=60s → spike at 20s–40s.
        let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
        let (phase, rps) = compute_phase_and_rps(30.0, &plan);
        assert_eq!(phase, Phase::RampUp);
        assert_eq!(rps, 100.0);
    }

    #[test]
    fn spike_final_third_is_recovery() {
        let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
        let (phase, rps) = compute_phase_and_rps(50.0, &plan);
        assert_eq!(phase, Phase::RampDown);
        assert!((rps - 20.0).abs() < 0.01);
    }

    #[test]
    fn spike_first_third_is_baseline() {
        let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
        let (phase, rps) = compute_phase_and_rps(5.0, &plan);
        assert_eq!(phase, Phase::Steady);
        assert!((rps - 20.0).abs() < 0.01);
    }

    // ── pick_task ─────────────────────────────────────────────────────────────

    #[test]
    fn pick_task_empty_returns_none() {
        assert!(pick_task(&[], 0).is_none());
    }

    #[test]
    fn pick_task_single_task_always_selected() {
        let tasks = vec![make_task("only", 1)];
        for i in 0..5 {
            assert_eq!(pick_task(&tasks, i).unwrap().name, "only");
        }
    }

    #[test]
    fn pick_task_equal_weights_round_robin() {
        let tasks = vec![make_task("a", 1), make_task("b", 1)];
        assert_eq!(pick_task(&tasks, 0).unwrap().name, "a");
        assert_eq!(pick_task(&tasks, 1).unwrap().name, "b");
        assert_eq!(pick_task(&tasks, 2).unwrap().name, "a");
    }

    #[test]
    fn pick_task_higher_weight_selected_more() {
        // weights: a=1, b=3 → total=4; slots 0→a, 1,2,3→b
        let tasks = vec![make_task("a", 1), make_task("b", 3)];
        let names: Vec<&str> = (0..4).map(|i| pick_task(&tasks, i).unwrap().name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "b", "b"]);
    }

    #[test]
    fn pick_task_wraps_around() {
        let tasks = vec![make_task("a", 1), make_task("b", 1)];
        assert_eq!(pick_task(&tasks, 4).unwrap().name, "a");
        assert_eq!(pick_task(&tasks, 5).unwrap().name, "b");
    }
}
