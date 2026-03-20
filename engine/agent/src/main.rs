/// LoadPilot agent — connects to the embedded NATS broker, receives a plan
/// shard, runs static HTTP load, and streams metrics back every second.
///
/// Usage (launched by coordinator):
///   agent --coordinator 127.0.0.1:4222 --run-id <uuid> --agent-id agent-0

mod nats;
mod runner;

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, Duration};

use crate::nats::NatsClient;
use crate::runner::run_load;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "agent")]
struct Args {
    #[arg(long)]
    coordinator: String,

    #[arg(long)]
    run_id: String,

    #[arg(long)]
    agent_id: String,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RegisterMsg {
    agent_id: String,
}

#[derive(Deserialize)]
struct ShardMsg {
    #[allow(dead_code)]
    agent_id: String,
    plan: runner::Plan,
}

#[derive(Deserialize)]
struct ControlMsg {
    command: String,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut nats = NatsClient::connect(&args.coordinator).await?;

    let shard_subject = format!("loadpilot.shard.{}", args.agent_id);
    let metrics_subject = format!("loadpilot.metrics.{}", args.agent_id);

    nats.subscribe(&shard_subject, "shard").await?;
    nats.subscribe("loadpilot.control", "ctrl").await?;

    // Announce to coordinator.
    let reg = serde_json::to_string(&RegisterMsg { agent_id: args.agent_id.clone() })?;
    nats.publish("loadpilot.register", reg.as_bytes()).await?;

    eprintln!("[agent {}] registered, waiting for plan shard...", args.agent_id);

    // Wait for shard plan.
    let plan = loop {
        let (subject, payload) = nats.next_message().await?;
        if subject == shard_subject {
            let msg: ShardMsg = serde_json::from_slice(&payload)?;
            eprintln!("[agent {}] received shard ({} RPS)", args.agent_id, msg.plan.rps);
            break msg.plan;
        }
        if subject == "loadpilot.control" {
            if let Ok(ctrl) = serde_json::from_slice::<ControlMsg>(&payload) {
                if ctrl.command == "stop" { return Ok(()); }
            }
        }
    };

    // Run load test — returns a channel of metric snapshots.
    let mut metrics_rx = run_load(plan).await;
    let mut tick = interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Ok(snapshot) = metrics_rx.try_recv() {
                    let is_done = snapshot.phase == "done";
                    let payload = serde_json::to_string(&snapshot)?;
                    nats.publish(&metrics_subject, payload.as_bytes()).await?;
                    if is_done { break; }
                }
            }
            Ok((subject, payload)) = nats.next_message() => {
                if subject == "loadpilot.control" {
                    if let Ok(ctrl) = serde_json::from_slice::<ControlMsg>(&payload) {
                        if ctrl.command == "stop" { break; }
                    }
                }
            }
        }
    }

    eprintln!("[agent {}] done", args.agent_id);
    Ok(())
}
