/// LoadPilot agent — connects to NATS broker, receives a plan shard,
/// runs static HTTP load, and streams metrics back every second.
///
/// After completing a run the agent reconnects and waits for the next plan,
/// making it suitable for long-running Railway/Docker deployments.
///
/// Usage:
///   agent --coordinator <host:port> --agent-id <id>
mod nats;
mod runner;

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, sleep, Duration};

use crate::nats::NatsClient;
use crate::runner::run_load;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "agent")]
struct Args {
    /// NATS broker address (coordinator's embedded broker or external NATS).
    #[arg(long)]
    coordinator: String,

    /// Unique agent identifier. Each agent in a run must have a distinct ID.
    #[arg(long)]
    agent_id: String,

    /// Optional: run ID passed by coordinator (informational only, not required).
    #[arg(long, default_value = "")]
    run_id: String,
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
    /// Unix timestamp (ms) at which this agent should start. Coordinator sends
    /// `now + 2000ms` so all agents begin simultaneously regardless of network lag.
    #[serde(default)]
    start_at_unix_ms: u64,
}

#[derive(Deserialize)]
struct ControlMsg {
    command: String,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    loop {
        match run_once(&args).await {
            Ok(()) => {
                eprintln!(
                    "[agent {}] run complete — reconnecting in 2s...",
                    args.agent_id
                );
                sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                eprintln!(
                    "[agent {}] error: {e} — reconnecting in 5s...",
                    args.agent_id
                );
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_once(args: &Args) -> Result<()> {
    let mut nats = NatsClient::connect(&args.coordinator).await?;

    let shard_subject = format!("loadpilot.shard.{}", args.agent_id);
    let metrics_subject = format!("loadpilot.metrics.{}", args.agent_id);

    nats.subscribe(&shard_subject, "shard").await?;
    nats.subscribe("loadpilot.control", "ctrl").await?;

    // Announce to coordinator and re-announce every 3s until shard arrives.
    // NATS is fire-and-forget: if the coordinator starts after the agent,
    // it would miss the initial register. Periodic re-announce fixes this.
    let reg = serde_json::to_string(&RegisterMsg {
        agent_id: args.agent_id.clone(),
    })?;
    nats.publish("loadpilot.register", reg.as_bytes()).await?;

    eprintln!(
        "[agent {}] registered — waiting for plan shard...",
        args.agent_id
    );

    // Wait for shard plan or stop signal.
    let mut reannounce = interval(Duration::from_secs(3));
    reannounce.tick().await; // consume the immediate first tick
    let msg: ShardMsg = loop {
        tokio::select! {
            _ = reannounce.tick() => {
                nats.publish("loadpilot.register", reg.as_bytes()).await?;
            }
            result = nats.next_message() => {
                let (subject, payload) = result?;
                if subject == shard_subject {
                    let msg: ShardMsg = serde_json::from_slice(&payload)?;
                    eprintln!(
                        "[agent {}] received shard ({} RPS)",
                        args.agent_id, msg.plan.rps
                    );
                    break msg;
                }
                if subject == "loadpilot.control" {
                    if let Ok(ctrl) = serde_json::from_slice::<ControlMsg>(&payload) {
                        if ctrl.command == "stop" {
                            eprintln!(
                                "[agent {}] received stop before shard — will retry",
                                args.agent_id
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }
    };

    // Synchronised start: sleep until start_at_unix_ms to align all agents.
    if msg.start_at_unix_ms > 0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if msg.start_at_unix_ms > now_ms {
            let wait_ms = msg.start_at_unix_ms - now_ms;
            eprintln!(
                "[agent {}] waiting {wait_ms}ms for synchronised start...",
                args.agent_id
            );
            sleep(Duration::from_millis(wait_ms)).await;
        }
    }

    // Run load test.
    let mut metrics_rx = run_load(msg.plan).await;
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
