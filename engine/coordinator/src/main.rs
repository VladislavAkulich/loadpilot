mod coordinator;
mod metrics;
mod plan;
mod prometheus_server;
mod python_bridge;

use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::coordinator::SharedSnapshot;

#[tokio::main]
async fn main() -> Result<()> {
    let mut input = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut input)?;

    let preview: String = input.chars().take(200).collect();
    let plan: plan::ScenarioPlan = serde_json::from_str(input.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse plan JSON: {}\nInput: {}", e, preview))?;

    let shared_snapshot: SharedSnapshot = Arc::new(RwLock::new(None));

    // Start Prometheus metrics server in the background (port 9090).
    let prom_snapshot = Arc::clone(&shared_snapshot);
    let _prom_handle = tokio::spawn(async move {
        if let Err(e) = prometheus_server::serve(9090, prom_snapshot).await {
            eprintln!("Prometheus server error: {}", e);
        }
    });

    let coord = coordinator::Coordinator::new(plan);
    coord.run(shared_snapshot).await?;

    Ok(())
}
