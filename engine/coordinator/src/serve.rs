//! HTTP serve mode for running coordinator as a persistent k8s service.
//!
//! POST /run   — accepts ScenarioPlan JSON, runs the load test, streams
//!               metric JSON lines as newline-delimited JSON (ndjson).
//!               Returns 409 if a test is already running.
use std::convert::Infallible;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::Response,
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    coordinator::SharedSnapshot, distributed, metrics::MetricSink, plan::ScenarioPlan,
    prometheus_server,
};

struct ServeState {
    nats_url: String,
    n_agents: usize,
    nats_token: Option<String>,
    busy: AtomicBool,
    shared_snapshot: SharedSnapshot,
}

pub async fn run(
    nats_url: String,
    n_agents: usize,
    nats_token: Option<String>,
    shared_snapshot: SharedSnapshot,
) -> Result<()> {
    // Start Prometheus on 9090.
    let prom = Arc::clone(&shared_snapshot);
    tokio::spawn(async move {
        if let Err(e) = prometheus_server::serve(9090, prom).await {
            eprintln!("[serve] Prometheus error: {e}");
        }
    });

    let state = Arc::new(ServeState {
        nats_url,
        n_agents,
        nats_token,
        busy: AtomicBool::new(false),
        shared_snapshot,
    });

    let app = Router::new()
        .route("/run", post(run_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    eprintln!("[serve] listening on 0.0.0.0:8080");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn run_handler(State(state): State<Arc<ServeState>>, body: String) -> Response {
    if state
        .busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .body(Body::from("a test is already running\n"))
            .unwrap();
    }

    let plan: ScenarioPlan = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            state.busy.store(false, Ordering::Release);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(format!("invalid plan: {e}\n")))
                .unwrap();
        }
    };

    let (metric_tx, metric_rx) = mpsc::unbounded_channel::<String>();
    let sink: MetricSink = {
        let tx = metric_tx.clone();
        Arc::new(move |line: String| {
            let _ = tx.send(line);
        })
    };

    let state2 = Arc::clone(&state);
    let metric_tx2 = metric_tx.clone();
    tokio::spawn(async move {
        let result = distributed::run_with_nats_url(
            plan,
            state2.n_agents,
            &state2.nats_url,
            state2.nats_token.as_deref(),
            Arc::clone(&state2.shared_snapshot),
            sink,
        )
        .await;
        if let Err(e) = result {
            eprintln!("[serve] test error: {e}");
        }
        state2.busy.store(false, Ordering::Release);
        drop(metric_tx2);
    });

    let stream = UnboundedReceiverStream::new(metric_rx)
        .map(|line: String| Ok::<Bytes, Infallible>(Bytes::from(format!("{line}\n"))));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .header("Transfer-Encoding", "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
}
