/// Minimal embedded NATS-compatible broker.
///
/// Implements a subset of the NATS 1.x wire protocol sufficient for
/// coordinator↔agent communication:
///   CONNECT, SUB, UNSUB, PUB → MSG routing, PING/PONG, INFO on connect.
///   Wildcard matching: `*` = one token, `>` = rest of subject.
///
/// The coordinator also publishes/subscribes directly via `BrokerHandle`
/// without going through TCP (no round-trip for local messages).

use std::sync::Arc;
use anyhow::Result;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
};

// ── Subject wildcard matching ─────────────────────────────────────────────────

pub fn subject_matches(pattern: &str, subject: &str) -> bool {
    let pp: Vec<&str> = pattern.split('.').collect();
    let sp: Vec<&str> = subject.split('.').collect();
    let mut pi = 0;
    let mut si = 0;
    loop {
        match (pp.get(pi), sp.get(si)) {
            (Some(&">"), _) => return true,
            (Some(&"*"), Some(_)) => { pi += 1; si += 1; }
            (Some(p), Some(s)) if p == s => { pi += 1; si += 1; }
            (None, None) => return true,
            _ => return false,
        }
    }
}

// ── Broker state ──────────────────────────────────────────────────────────────

type ClientId = u64;

struct Sub {
    sid: String,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Default)]
struct State {
    next_id: ClientId,
    /// TCP client subscriptions — receive full NATS MSG frame.
    tcp_subs: Vec<(String, ClientId, Sub)>,
    /// In-process subscriptions (coordinator itself) — receive raw payload only.
    internal_subs: Vec<(String, mpsc::UnboundedSender<Vec<u8>>)>,
}

impl State {
    fn alloc_id(&mut self) -> ClientId {
        self.next_id += 1;
        self.next_id
    }

    fn subscribe_tcp(&mut self, pattern: String, cid: ClientId, sid: String, tx: mpsc::UnboundedSender<Vec<u8>>) {
        self.tcp_subs.push((pattern, cid, Sub { sid, tx }));
    }

    fn subscribe_internal(&mut self, pattern: String, tx: mpsc::UnboundedSender<Vec<u8>>) {
        self.internal_subs.push((pattern, tx));
    }

    fn unsubscribe(&mut self, cid: ClientId, sid: &str) {
        self.tcp_subs.retain(|(_, c, s)| !(*c == cid && s.sid == sid));
    }

    fn disconnect(&mut self, cid: ClientId) {
        self.tcp_subs.retain(|(_, c, _)| *c != cid);
    }

    fn publish(&self, subject: &str, payload: &[u8]) {
        // TCP clients: wrap in NATS MSG frame.
        for (pattern, _, sub) in &self.tcp_subs {
            if subject_matches(pattern, subject) {
                let header = format!("MSG {} {} {}\r\n", subject, sub.sid, payload.len());
                let mut msg = header.into_bytes();
                msg.extend_from_slice(payload);
                msg.extend_from_slice(b"\r\n");
                let _ = sub.tx.send(msg);
            }
        }
        // In-process: send raw payload.
        for (pattern, tx) in &self.internal_subs {
            if subject_matches(pattern, subject) {
                let _ = tx.send(payload.to_vec());
            }
        }
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Opaque handle to the broker — used by distributed.rs to publish/subscribe
/// without exposing the internal State type.
#[derive(Clone)]
pub struct BrokerHandle(Arc<Mutex<State>>);

/// Start the embedded broker on `addr`. Returns a handle for in-process
/// publish/subscribe (coordinator talks to itself without a TCP round-trip).
pub async fn start(addr: &str) -> Result<BrokerHandle> {
    let state = BrokerHandle(Arc::new(Mutex::new(State::default())));
    let listener = TcpListener::bind(addr).await?;

    let broker = state.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let b = broker.0.clone();
                    tokio::spawn(handle_client(stream, b));
                }
                Err(e) => { eprintln!("[broker] accept error: {e}"); break; }
            }
        }
    });

    Ok(state)
}

/// Publish directly from the coordinator process (no TCP).
pub async fn publish(handle: &BrokerHandle, subject: &str, payload: &[u8]) {
    handle.0.lock().await.publish(subject, payload);
}

/// Subscribe from the coordinator process. Returns a channel of raw payloads.
pub async fn subscribe(handle: &BrokerHandle, pattern: &str) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    handle.0.lock().await.subscribe_internal(pattern.to_string(), tx);
    rx
}

// ── Per-client TCP handler ────────────────────────────────────────────────────

const INFO_BANNER: &[u8] = b"INFO {\"server_id\":\"LOADPILOT\",\"version\":\"2.10.0\",\
\"host\":\"0.0.0.0\",\"port\":4222,\"max_payload\":1048576,\
\"proto\":1,\"auth_required\":false,\"tls_required\":false}\r\n";

async fn handle_client(stream: TcpStream, state: Arc<Mutex<State>>) {
    let cid = state.lock().await.alloc_id();
    let (read_half, write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Writer task — drains rx and writes to socket.
    let mut writer = write_half;
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // Send NATS INFO banner immediately on connect.
    let _ = tx.send(INFO_BANNER.to_vec());

    let mut lines = BufReader::new(read_half).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        let cmd = line.trim().to_uppercase();

        if cmd.starts_with("CONNECT") {
            // No auth required — accept silently (verbose:false default).
        } else if cmd.starts_with("PING") {
            let _ = tx.send(b"PONG\r\n".to_vec());
        } else if cmd.starts_with("SUB") {
            // SUB <subject> [queue-group] <sid>
            let parts: Vec<&str> = line.trim().splitn(4, ' ').collect();
            if parts.len() >= 3 {
                let subject = parts[1].to_string();
                let sid = parts[parts.len() - 1].to_string();
                state.lock().await.subscribe_tcp(subject, cid, sid, tx.clone());
            }
        } else if cmd.starts_with("UNSUB") {
            // UNSUB <sid> [max-msgs]
            let parts: Vec<&str> = line.trim().splitn(3, ' ').collect();
            if parts.len() >= 2 {
                state.lock().await.unsubscribe(cid, parts[1]);
            }
        } else if cmd.starts_with("PUB") {
            // PUB <subject> <#bytes>\r\n<payload>\r\n
            let parts: Vec<&str> = line.trim().splitn(3, ' ').collect();
            if parts.len() < 3 { continue; }
            let subject = parts[1].to_string();
            let payload_len: usize = parts[2].parse().unwrap_or(0);

            let payload_line = match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => break,
            };

            let payload = payload_line.as_bytes();
            let payload = &payload[..payload_len.min(payload.len())];
            state.lock().await.publish(&subject, payload);
        }
    }

    state.lock().await.disconnect(cid);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::subject_matches;

    #[test]
    fn exact_match() {
        assert!(subject_matches("a.b.c", "a.b.c"));
        assert!(!subject_matches("a.b.c", "a.b.d"));
    }

    #[test]
    fn wildcard_star() {
        assert!(subject_matches("a.*.c", "a.b.c"));
        assert!(!subject_matches("a.*.c", "a.b.d"));
        assert!(!subject_matches("a.*.c", "a.b.c.d"));
    }

    #[test]
    fn wildcard_gt() {
        assert!(subject_matches("a.>", "a.b.c.d"));
        assert!(subject_matches("loadpilot.metrics.>", "loadpilot.metrics.run1.agent0"));
        assert!(!subject_matches("a.>", "b.c"));
    }
}
