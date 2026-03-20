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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use std::sync::RwLock;

use crate::metrics::Metrics;
use crate::plan::{ScenarioPlan, TaskPlan};
use crate::python_bridge::PythonBridge;

pub type SharedSnapshot = Arc<RwLock<Option<MetricsSnapshot>>>;

/// Phase of the load test.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    RampUp,
    Steady,
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
}

#[derive(serde::Serialize, Clone)]
pub struct LatencySnapshot {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub min_ms: f64,
    pub mean_ms: f64,
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

    pub async fn run(self, shared_snapshot: SharedSnapshot) -> Result<()> {
        let plan = Arc::new(self.plan);
        let metrics = self.metrics;

        let start = Instant::now();
        let ramp_up = Duration::from_secs(plan.ramp_up_secs);
        let total = Duration::from_secs(plan.duration_secs + plan.ramp_up_secs);

        let done_flag = Arc::new(AtomicBool::new(false));
        let active_workers = Arc::new(AtomicU64::new(0));

        let max_concurrency = (plan.rps * 4).max(100) as usize;
        let sem = Arc::new(Semaphore::new(max_concurrency));

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(256)
            .build()?;

        // ── PyO3 bridge (optional) ────────────────────────────────────────────
        // Wrap in Arc<Mutex<>> so it can be cloned into tokio::spawn closures
        // and accessed from spawn_blocking threads.
        let bridge: Option<Arc<Mutex<PythonBridge>>> =
            if let (Some(sf), Some(sc)) = (&plan.scenario_file, &plan.scenario_class) {
                let n = plan.n_vusers.unwrap_or(5) as usize;
                match PythonBridge::new(sf, sc, n, &plan.target_url, http_client.clone(), tokio::runtime::Handle::current()) {
                    Ok(b) => {
                        eprintln!("[loadpilot] PyO3 bridge ready ({} VUsers)", n);
                        Some(Arc::new(Mutex::new(b)))
                    }
                    Err(e) => {
                        eprintln!("[loadpilot] PyO3 bridge init failed: {} — using static URLs", e);
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
            let n = b.lock().unwrap().n_vusers();
            // Space on_start calls evenly across the ramp-up window.
            let stagger_ms = if n > 1 && plan.ramp_up_secs > 0 {
                plan.ramp_up_secs * 1000 / (n as u64 - 1)
            } else {
                0
            };

            tokio::spawn(async move {
                for i in 0..n {
                    let b_clone = Arc::clone(&b);
                    let result = tokio::task::spawn_blocking(move || {
                        b_clone.lock().unwrap().call_on_start(i)
                    })
                    .await;

                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("[loadpilot] on_start[{}] error: {}", i, e),
                        Err(e) => eprintln!("[loadpilot] on_start[{}] panic: {}", i, e),
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
        let reporter_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let elapsed = start.elapsed();
                let phase = if elapsed < ramp_up {
                    Phase::RampUp
                } else if elapsed < total {
                    Phase::Steady
                } else {
                    Phase::Done
                };

                let target_rps = compute_target_rps(
                    elapsed.as_secs_f64(),
                    plan_clone.ramp_up_secs as f64,
                    plan_clone.rps as f64,
                );

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
                };

                if let Ok(line) = serde_json::to_string(&snap) {
                    println!("{}", line);
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

            let target_rps = compute_target_rps(
                elapsed.as_secs_f64(),
                plan.ramp_up_secs as f64,
                plan.rps as f64,
            );

            deficit += target_rps * dt;
            let n_requests = deficit.floor() as usize;
            deficit -= n_requests as f64;
            let n_ready = ready_vusers.load(Ordering::Acquire);

            if n_ready == 0 {
                // VUsers are still initialising; wait.
                continue;
            }

            for i in 0..n_requests {
                let t = pick_task(&plan.tasks, i);
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

                        let b_run = Arc::clone(&b);
                        let task_name_r = task_name.clone();
                        let run_result = tokio::task::spawn_blocking(move || {
                            b_run.lock().unwrap().run_task(vuser_idx, &task_name_r)
                        })
                        .await;

                        match run_result {
                            Ok(Ok(calls)) => {
                                if calls.is_empty() {
                                    metrics_task.record_error(0);
                                } else {
                                    for cr in calls {
                                        if let Some(ref err) = cr.error {
                                            eprintln!("[loadpilot] task error ({}): {}", task_name, err);
                                        }
                                        if cr.success {
                                            metrics_task.record_success(cr.elapsed_ms);
                                        } else {
                                            metrics_task.record_error(cr.elapsed_ms);
                                        }
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                eprintln!("[loadpilot] run_task error: {}", e);
                                metrics_task.record_error(0);
                            }
                            Err(e) => {
                                eprintln!("[loadpilot] spawn_blocking panic: {}", e);
                                metrics_task.record_error(0);
                            }
                        }
                        active_clone2.fetch_sub(1, Ordering::Relaxed);
                    });
                } else {
                    // Static path: URL / method come directly from the plan.
                    let client = http_client.clone();
                    let url =
                        format!("{}{}", plan.target_url.trim_end_matches('/'), t.url);
                    let method = t.method.clone();
                    let headers = t.headers.clone();
                    let body = t.body_template.clone();

                    tokio::spawn(async move {
                        let _permit = match sem_clone.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        active_clone2.fetch_add(1, Ordering::Relaxed);
                        let t0 = Instant::now();
                        let result =
                            execute_request(&client, &method, &url, &headers, body.as_deref())
                                .await;
                        let latency_ms = t0.elapsed().as_millis() as u64;
                        match result {
                            Ok(r) if r.status < 400 => metrics_task.record_success(latency_ms),
                            _ => metrics_task.record_error(latency_ms),
                        }
                        active_clone2.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }

        // ── Teardown ──────────────────────────────────────────────────────────
        done_flag.store(true, Ordering::Relaxed);
        sleep(Duration::from_millis(1200)).await;

        // Call on_stop for all VUsers.
        if let Some(ref b) = bridge {
            let n = b.lock().unwrap().n_vusers();
            for i in 0..n {
                let b_clone = Arc::clone(b);
                let _ = tokio::task::spawn_blocking(move || {
                    b_clone.lock().unwrap().call_on_stop(i)
                })
                .await;
            }
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
        };
        if let Ok(line) = serde_json::to_string(&snap) {
            println!("{}", line);
        }

        let _ = reporter_handle.await;
        Ok(())
    }
}

fn compute_target_rps(elapsed_secs: f64, ramp_up_secs: f64, target_rps: f64) -> f64 {
    if ramp_up_secs <= 0.0 || elapsed_secs >= ramp_up_secs {
        return target_rps;
    }
    target_rps * (elapsed_secs / ramp_up_secs)
}

fn pick_task<'a>(tasks: &'a [TaskPlan], index: usize) -> &'a TaskPlan {
    if tasks.is_empty() {
        panic!("No tasks in plan");
    }
    let total_weight: u32 = tasks.iter().map(|t| t.weight).sum();
    if total_weight == 0 {
        return &tasks[index % tasks.len()];
    }
    let slot = (index as u32) % total_weight;
    let mut cumulative = 0u32;
    for t in tasks {
        cumulative += t.weight;
        if slot < cumulative {
            return t;
        }
    }
    &tasks[tasks.len() - 1]
}

/// Response data returned from a live HTTP request.
struct HttpResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

async fn execute_request(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&str>,
) -> Result<HttpResponse> {
    let mut req = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        other => return Err(anyhow::anyhow!("Unknown HTTP method: {}", other)),
    };

    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b.to_string());
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let resp_headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let body_text = resp.text().await.unwrap_or_default();
    Ok(HttpResponse { status, headers: resp_headers, body: body_text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::TaskPlan;
    use std::collections::HashMap;

    fn make_task(name: &str, weight: u32) -> TaskPlan {
        TaskPlan {
            name: name.to_string(),
            weight,
            url: "/".to_string(),
            method: "GET".to_string(),
            headers: HashMap::new(),
            body_template: None,
        }
    }

    // ── compute_target_rps ────────────────────────────────────────────────────

    #[test]
    fn target_rps_before_ramp_scales_linearly() {
        // At 50% of ramp-up time, target should be 50% of peak RPS.
        let rps = compute_target_rps(5.0, 10.0, 100.0);
        assert!((rps - 50.0).abs() < 0.01);
    }

    #[test]
    fn target_rps_after_ramp_is_full() {
        let rps = compute_target_rps(15.0, 10.0, 100.0);
        assert_eq!(rps, 100.0);
    }

    #[test]
    fn target_rps_at_ramp_boundary_is_full() {
        let rps = compute_target_rps(10.0, 10.0, 100.0);
        assert_eq!(rps, 100.0);
    }

    #[test]
    fn target_rps_zero_ramp_up_is_full() {
        let rps = compute_target_rps(0.0, 0.0, 100.0);
        assert_eq!(rps, 100.0);
    }

    #[test]
    fn target_rps_start_of_ramp_is_zero() {
        let rps = compute_target_rps(0.0, 10.0, 100.0);
        assert_eq!(rps, 0.0);
    }

    // ── pick_task ─────────────────────────────────────────────────────────────

    #[test]
    fn pick_task_single_task_always_selected() {
        let tasks = vec![make_task("only", 1)];
        for i in 0..5 {
            assert_eq!(pick_task(&tasks, i).name, "only");
        }
    }

    #[test]
    fn pick_task_equal_weights_round_robin() {
        let tasks = vec![make_task("a", 1), make_task("b", 1)];
        assert_eq!(pick_task(&tasks, 0).name, "a");
        assert_eq!(pick_task(&tasks, 1).name, "b");
        assert_eq!(pick_task(&tasks, 2).name, "a");
    }

    #[test]
    fn pick_task_higher_weight_selected_more() {
        // weights: a=1, b=3 → total=4; slots 0→a, 1,2,3→b
        let tasks = vec![make_task("a", 1), make_task("b", 3)];
        let names: Vec<&str> = (0..4).map(|i| pick_task(&tasks, i).name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "b", "b"]);
    }

    #[test]
    fn pick_task_wraps_around() {
        let tasks = vec![make_task("a", 1), make_task("b", 1)];
        // index 4 → slot 4 % 2 = 0 → "a"
        assert_eq!(pick_task(&tasks, 4).name, "a");
        // index 5 → slot 5 % 2 = 1 → "b"
        assert_eq!(pick_task(&tasks, 5).name, "b");
    }
}
