/// Minimal NATS 1.x client over raw TCP.
/// Implements exactly the subset our agent needs: CONNECT, SUB, PUB, PING/PONG.
/// Compatible with the embedded broker in coordinator/src/broker.rs.
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub struct NatsClient {
    // Channel to the write task — decouples reads from writes so read_loop
    // can send PONG without holding the writer lock.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    // Incoming MSG payloads: (subject, payload).
    msg_rx: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
}

impl NatsClient {
    /// Connect and perform the NATS handshake (INFO → CONNECT → PING → PONG).
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("Cannot connect to broker at {addr}"))?;

        let (read_half, write_half) = stream.into_split();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Spawn a writer task that drains the channel to TCP.
        tokio::spawn(write_loop(write_half, write_rx));

        // Spawn a reader task; pass a write_tx clone so it can reply PONG to server PINGs.
        tokio::spawn(read_loop(read_half, msg_tx, write_tx.clone()));

        // NATS handshake: send CONNECT then PING; server will reply PONG.
        let connect = b"CONNECT {\"verbose\":false,\"pedantic\":false,\"tls_required\":false}\r\n";
        write_tx.send(connect.to_vec()).ok();
        write_tx.send(b"PING\r\n".to_vec()).ok();

        Ok(Self { write_tx, msg_rx })
    }

    /// Subscribe to `subject` with a given `sid` (subscription ID).
    pub async fn subscribe(&mut self, subject: &str, sid: &str) -> Result<()> {
        let cmd = format!("SUB {subject} {sid}\r\n");
        self.write_tx
            .send(cmd.into_bytes())
            .map_err(|_| anyhow::anyhow!("NATS write channel closed"))?;
        Ok(())
    }

    /// Publish `payload` to `subject`.
    pub async fn publish(&mut self, subject: &str, payload: &[u8]) -> Result<()> {
        let header = format!("PUB {subject} {}\r\n", payload.len());
        let mut data = header.into_bytes();
        data.extend_from_slice(payload);
        data.extend_from_slice(b"\r\n");
        self.write_tx
            .send(data)
            .map_err(|_| anyhow::anyhow!("NATS write channel closed"))?;
        Ok(())
    }

    /// Wait for the next inbound message. Returns `(subject, payload)`.
    pub async fn next_message(&mut self) -> Result<(String, Vec<u8>)> {
        self.msg_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("NATS connection closed"))
    }
}

// ── Writer task ───────────────────────────────────────────────────────────────

async fn write_loop(
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(data) = rx.recv().await {
        if write_half.write_all(&data).await.is_err() {
            break;
        }
    }
}

// ── Reader task ───────────────────────────────────────────────────────────────

/// Reads NATS frames from the server and forwards MSG payloads to `tx`.
/// Responds to server PING with PONG via `write_tx`.
async fn read_loop(
    read_half: tokio::net::tcp::OwnedReadHalf,
    tx: mpsc::UnboundedSender<(String, Vec<u8>)>,
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
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
            if parts.len() < 4 {
                continue;
            }
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
            // Server keepalive — must respond with PONG or server closes connection.
            let _ = write_tx.send(b"PONG\r\n".to_vec());
        }
        // INFO, PONG, +OK — ignore.
    }
}
