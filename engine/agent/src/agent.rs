#![allow(dead_code)]
/// Agent: connects to coordinator, manages the VUser pool.
///
/// This module is the v2 implementation skeleton. In MVP, the coordinator
/// binary handles all worker management directly.
use anyhow::Result;

pub struct Agent {
    pub id: String,
    pub coordinator_addr: String,
}

impl Agent {
    pub fn new(id: String, coordinator_addr: String) -> Self {
        Self { id, coordinator_addr }
    }

    /// Connect to the coordinator and start receiving work.
    pub async fn run(&self) -> Result<()> {
        eprintln!(
            "[{}] Connecting to coordinator at {} (v2 stub)",
            self.id, self.coordinator_addr
        );
        // v2: open TCP connection, receive ScenarioPlan slice, delegate to worker pool
        Ok(())
    }
}
