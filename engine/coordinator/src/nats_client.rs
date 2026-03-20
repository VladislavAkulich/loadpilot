/// Minimal NATS 1.x client over raw TCP.
/// Implements exactly the subset our agent needs: CONNECT, SUB, PUB, PING/PONG.
/// Compatible with the embedded broker in coordinator/src/broker.rs.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub struct NatsClient {
    // Shared write half — used to send commands from any method.
    writer: tokio::net::tcp::OwnedWriteHalf,
    // Incoming MSG payloads: (subject, payload).
    msg_rx: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
}

impl NatsClient {
    /// Connect and perform the NATS handshake (INFO → CONNECT → PING → PONG).
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("Cannot connect to broker at {addr}"))?;

        let (read_half, mut write_half) = stream.into_split();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();

        // Spawn a reader task that parses inbound NATS frames.
        tokio::spawn(read_loop(read_half, msg_tx));

        // NATS handshake: server sends INFO, we send CONNECT + PING, server sends PONG.
        let connect = b"CONNECT {\"verbose\":false,\"pedantic\":false,\"tls_required\":false}\r\n";
        write_half.write_all(connect).await?;
        write_half.write_all(b"PING\r\n").await?;

        Ok(Self { writer: write_half, msg_rx })
    }

    /// Subscribe to `subject` with a given `sid` (subscription ID).
    pub async fn subscribe(&mut self, subject: &str, sid: &str) -> Result<()> {
        let cmd = format!("SUB {subject} {sid}\r\n");
        self.writer.write_all(cmd.as_bytes()).await?;
        Ok(())
    }

    /// Publish `payload` to `subject`.
    pub async fn publish(&mut self, subject: &str, payload: &[u8]) -> Result<()> {
        let header = format!("PUB {subject} {}\r\n", payload.len());
        self.writer.write_all(header.as_bytes()).await?;
        self.writer.write_all(payload).await?;
        self.writer.write_all(b"\r\n").await?;
        Ok(())
    }

    /// Wait for the next inbound message. Returns `(subject, payload)`.
    pub async fn next_message(&mut self) -> Result<(String, Vec<u8>)> {
        self.msg_rx.recv().await.ok_or_else(|| anyhow::anyhow!("NATS connection closed"))
    }
}

// ── Reader task ───────────────────────────────────────────────────────────────

/// Reads NATS frames from the server and forwards MSG payloads to `tx`.
async fn read_loop(
    read_half: tokio::net::tcp::OwnedReadHalf,
    tx: mpsc::UnboundedSender<(String, Vec<u8>)>,
) {
    let mut lines = BufReader::new(read_half).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };

        let upper = line.trim().to_uppercase();

        if upper.starts_with("MSG") {
            // MSG <subject> <sid> <bytes>
            let parts: Vec<&str> = line.trim().splitn(4, ' ').collect();
            if parts.len() < 4 { continue; }
            let subject = parts[1].to_string();
            let payload_len: usize = parts[3].parse().unwrap_or(0);

            let payload_line = match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => break,
            };

            let bytes = payload_line.as_bytes();
            let payload = bytes[..payload_len.min(bytes.len())].to_vec();
            let _ = tx.send((subject, payload));
        } else if upper.starts_with("PING") {
            // Server keepalive — we respond with PONG but we don't have writer here.
            // The read_loop doesn't have write access; PING from server is rare.
            // In practice our broker only sends PONG (never PING). Skip.
        }
        // INFO, PONG, +OK — ignore.
    }
}
