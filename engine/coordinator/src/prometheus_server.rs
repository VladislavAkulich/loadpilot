use anyhow::Result;
use axum::{extract::State, routing::get, Router};

use crate::coordinator::SharedSnapshot;

async fn metrics_handler(State(snapshot): State<SharedSnapshot>) -> String {
    let guard = match snapshot.read() {
        Ok(g) => g,
        Err(_) => return "# loadpilot metrics unavailable\n".to_string(),
    };

    let Some(ref snap) = *guard else {
        return "# loadpilot: no data yet\n".to_string();
    };

    let phase = match snap.phase {
        crate::coordinator::Phase::RampUp => "ramp_up",
        crate::coordinator::Phase::Steady => "steady",
        crate::coordinator::Phase::Done => "done",
    };

    format!(
        "# HELP loadpilot_requests_total Total number of HTTP requests made.\n\
         # TYPE loadpilot_requests_total counter\n\
         loadpilot_requests_total {requests_total}\n\
         # HELP loadpilot_errors_total Total number of failed HTTP requests.\n\
         # TYPE loadpilot_errors_total counter\n\
         loadpilot_errors_total {errors_total}\n\
         # HELP loadpilot_current_rps Requests per second in the last 1s window.\n\
         # TYPE loadpilot_current_rps gauge\n\
         loadpilot_current_rps {current_rps}\n\
         # HELP loadpilot_target_rps Target requests per second for this phase.\n\
         # TYPE loadpilot_target_rps gauge\n\
         loadpilot_target_rps {target_rps}\n\
         # HELP loadpilot_active_workers Number of in-flight HTTP requests.\n\
         # TYPE loadpilot_active_workers gauge\n\
         loadpilot_active_workers {active_workers}\n\
         # HELP loadpilot_latency_p50_ms 50th percentile latency in milliseconds.\n\
         # TYPE loadpilot_latency_p50_ms gauge\n\
         loadpilot_latency_p50_ms {p50}\n\
         # HELP loadpilot_latency_p95_ms 95th percentile latency in milliseconds.\n\
         # TYPE loadpilot_latency_p95_ms gauge\n\
         loadpilot_latency_p95_ms {p95}\n\
         # HELP loadpilot_latency_p99_ms 99th percentile latency in milliseconds.\n\
         # TYPE loadpilot_latency_p99_ms gauge\n\
         loadpilot_latency_p99_ms {p99}\n\
         # HELP loadpilot_latency_max_ms Maximum observed latency in milliseconds.\n\
         # TYPE loadpilot_latency_max_ms gauge\n\
         loadpilot_latency_max_ms {max}\n\
         # HELP loadpilot_latency_mean_ms Mean latency in milliseconds.\n\
         # TYPE loadpilot_latency_mean_ms gauge\n\
         loadpilot_latency_mean_ms {mean}\n\
         # HELP loadpilot_elapsed_secs Seconds elapsed since the test started.\n\
         # TYPE loadpilot_elapsed_secs gauge\n\
         loadpilot_elapsed_secs {elapsed}\n\
         # HELP loadpilot_phase Current test phase (0=ramp_up, 1=steady, 2=done).\n\
         # TYPE loadpilot_phase gauge\n\
         loadpilot_phase{{phase=\"{phase}\"}} 1\n",
        requests_total = snap.requests_total,
        errors_total = snap.errors_total,
        current_rps = snap.current_rps,
        target_rps = snap.target_rps,
        active_workers = snap.active_workers,
        p50 = snap.latency.p50_ms,
        p95 = snap.latency.p95_ms,
        p99 = snap.latency.p99_ms,
        max = snap.latency.max_ms,
        mean = snap.latency.mean_ms,
        elapsed = snap.elapsed_secs,
        phase = phase,
    )
}

pub async fn serve(port: u16, snapshot: SharedSnapshot) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(snapshot);
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
