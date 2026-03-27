mod broker;
mod coordinator;
mod distributed;
mod metrics;
mod nats_client;
mod plan;
mod prometheus_server;
mod python_bridge;
mod serve;

use std::sync::{Arc, RwLock};

use anyhow::Result;
use clap::Parser;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use crate::coordinator::SharedSnapshot;
use crate::metrics::stdout_sink;


#[derive(Parser)]
#[command(name = "coordinator", about = "LoadPilot coordinator process")]
struct Args {
    /// Spawn N local agent processes for distributed mode.
    /// 0 (default) = single-process mode.
    #[arg(long, default_value = "0")]
    local_agents: usize,

    /// Wait for N externally started agents using the embedded broker.
    /// Agents must connect to --broker-addr.
    /// 0 (default) = disabled.
    #[arg(long, default_value = "0")]
    external_agents: usize,

    /// Connect to an external NATS server instead of starting an embedded broker.
    /// Must be used together with --external-agents N.
    /// Example: --nats-url nats://my-nats.railway.app:4222
    ///          --nats-url tls://my-nats.railway.app:4222
    #[arg(long)]
    nats_url: Option<String>,

    /// Token for NATS authentication (used with --nats-url).
    /// Can also be set via the NATS_TOKEN environment variable.
    #[arg(long, env = "NATS_TOKEN")]
    nats_token: Option<String>,

    /// Embedded broker address (used in local and external-agents modes).
    #[arg(long, default_value = "127.0.0.1:4222")]
    broker_addr: String,

    /// Run as a persistent HTTP service (serve mode for k8s deployments).
    /// Listens on 0.0.0.0:8080 for POST /run requests.
    #[arg(long)]
    serve: bool,

    /// Number of agents to expect in serve mode (used with --serve).
    #[arg(long, default_value = "1")]
    serve_agents: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("coordinator=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Prometheus runs in all modes.
    let shared_snapshot: SharedSnapshot = Arc::new(RwLock::new(None));

    // Graceful shutdown: fires on Ctrl+C or SIGTERM.
    // Signal handlers are registered here — immediately at startup, before any
    // spawned tasks or blocking operations — so SIGTERM is always caught even
    // under high task load (e.g. 100+ RPS spawning request tasks).
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    #[cfg(unix)]
    let mut sigterm_stream = {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate()).expect("failed to install SIGTERM handler")
    };
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm_stream.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        }
        tracing::info!("shutdown signal — initiating graceful shutdown");
        let _ = shutdown_tx.send(true);
    });

    if args.serve {
        let nats_url = args
            .nats_url
            .unwrap_or_else(|| "nats://localhost:4222".to_string());
        return serve::run(
            nats_url,
            args.serve_agents,
            args.nats_token,
            shared_snapshot,
            shutdown_rx,
        )
        .await;
    }

    let prom_snapshot = Arc::clone(&shared_snapshot);
    tokio::spawn(async move {
        if let Err(e) = prometheus_server::serve(9090, prom_snapshot).await {
            tracing::error!("Prometheus server error: {}", e);
        }
    });

    let mut input = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut input)?;

    let preview: String = input.chars().take(200).collect();
    let plan: plan::ScenarioPlan = serde_json::from_str(input.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse plan JSON: {}\nInput: {}", e, preview))?;

    if let Some(nats_url) = args.nats_url {
        if args.external_agents == 0 {
            anyhow::bail!(
                "--nats-url requires --external-agents N (how many remote agents to wait for)"
            );
        }
        distributed::run_with_nats_url(
            plan,
            args.external_agents,
            &nats_url,
            args.nats_token.as_deref(),
            shared_snapshot,
            stdout_sink(),
            shutdown_rx,
        )
        .await?;
    } else if args.local_agents > 0 {
        distributed::run(
            plan,
            args.local_agents,
            &args.broker_addr,
            shared_snapshot,
            stdout_sink(),
            shutdown_rx,
        )
        .await?;
    } else if args.external_agents > 0 {
        distributed::run_external_agents(
            plan,
            args.external_agents,
            &args.broker_addr,
            shared_snapshot,
            stdout_sink(),
            shutdown_rx,
        )
        .await?;
    } else {
        let coord = coordinator::Coordinator::new(plan);
        coord
            .run(shared_snapshot, stdout_sink(), shutdown_rx)
            .await?;
    }

    Ok(())
}
