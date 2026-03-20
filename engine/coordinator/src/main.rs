mod broker;
mod coordinator;
mod distributed;
mod metrics;
mod plan;
mod prometheus_server;
mod python_bridge;

use std::sync::{Arc, RwLock};

use anyhow::Result;
use clap::Parser;

use crate::coordinator::SharedSnapshot;

#[derive(Parser)]
#[command(name = "coordinator", about = "LoadPilot coordinator process")]
struct Args {
    /// Spawn N local agent processes for distributed mode.
    /// 0 (default) = single-process mode.
    #[arg(long, default_value = "0")]
    local_agents: usize,

    /// Embedded broker address (used in distributed mode).
    #[arg(long, default_value = "127.0.0.1:4222")]
    broker_addr: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut input = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut input)?;

    let preview: String = input.chars().take(200).collect();
    let plan: plan::ScenarioPlan = serde_json::from_str(input.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse plan JSON: {}\nInput: {}", e, preview))?;

    if args.local_agents > 0 {
        // Distributed mode — start embedded broker, spawn agents.
        distributed::run(plan, args.local_agents, &args.broker_addr).await?;
    } else {
        // Single-process mode — current behaviour.
        let shared_snapshot: SharedSnapshot = Arc::new(RwLock::new(None));

        let prom_snapshot = Arc::clone(&shared_snapshot);
        let _prom_handle = tokio::spawn(async move {
            if let Err(e) = prometheus_server::serve(9090, prom_snapshot).await {
                eprintln!("Prometheus server error: {}", e);
            }
        });

        let coord = coordinator::Coordinator::new(plan);
        coord.run(shared_snapshot).await?;
    }

    Ok(())
}
