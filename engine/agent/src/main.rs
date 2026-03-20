/// LoadPilot Agent binary (v2 — not used in MVP single-process mode).
///
/// In v2, multiple agents will connect to a coordinator over TCP, receive
/// sub-plans, execute HTTP workers, and stream aggregated metrics back.
mod agent;
mod python_bridge;
mod terminal;
mod worker;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "agent", about = "LoadPilot agent process (v2)")]
struct Args {
    /// Coordinator address in host:port format.
    #[arg(long, default_value = "localhost:7000")]
    coordinator: String,

    /// Human-readable agent ID for log output.
    #[arg(long, default_value = "agent-1")]
    id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    eprintln!(
        "[agent] {} connecting to coordinator at {} (v2 — not available in MVP)",
        args.id, args.coordinator
    );
    eprintln!(
        "[agent] In MVP mode, run 'loadpilot run <scenario.py>' — the coordinator binary handles everything."
    );
    Ok(())
}
