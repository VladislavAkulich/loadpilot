/// Minimal NATS 1.x client over raw TCP or TLS.
/// Implements exactly the subset our services need: CONNECT, SUB, PUB, PING/PONG.
///
/// URL schemes:
///   "host:port"         — plain TCP (legacy, no scheme)
///   "nats://host:port"  — plain TCP
///   "tls://host:port"   — TLS (system certificate store)
///
/// Token auth: pass `Some("secret")` to `connect_authenticated`; adds `auth_token` to CONNECT.
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
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
    /// Connect using a URL (`nats://`, `tls://`, or bare `host:port`) without auth.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_inner(url, None).await
    }

    /// Connect with token authentication.
    pub async fn connect_authenticated(url: &str, token: &str) -> Result<Self> {
        Self::connect_inner(url, Some(token)).await
    }

    async fn connect_inner(url: &str, token: Option<&str>) -> Result<Self> {
        let (tls, host, addr) = parse_url(url)?;

        let (read_half, write_half) = if tls {
            let stream = make_tls_stream(&host, &addr).await?;
            let (r, w) = tokio::io::split(stream);
            (
                Box::new(r) as Box<dyn AsyncRead + Unpin + Send + 'static>,
                Box::new(w) as Box<dyn AsyncWrite + Unpin + Send + 'static>,
            )
        } else {
            let stream = TcpStream::connect(&addr)
                .await
                .with_context(|| format!("Cannot connect to NATS at {addr}"))?;
            let (r, w) = tokio::io::split(stream);
            (
                Box::new(r) as Box<dyn AsyncRead + Unpin + Send + 'static>,
                Box::new(w) as Box<dyn AsyncWrite + Unpin + Send + 'static>,
            )
        };

        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(write_loop(write_half, write_rx));
        tokio::spawn(read_loop(read_half, msg_tx, write_tx.clone()));

        let connect_msg = build_connect(tls, token);
        write_tx.send(connect_msg.into_bytes()).ok();
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

// ── URL parsing ───────────────────────────────────────────────────────────────

/// Returns `(tls, host, addr)` where addr is `host:port`.
fn parse_url(url: &str) -> Result<(bool, String, String)> {
    if let Some(rest) = url.strip_prefix("tls://") {
        let host = rest.split(':').next().unwrap_or(rest).to_string();
        Ok((true, host, rest.to_string()))
    } else if let Some(rest) = url.strip_prefix("nats://") {
        let host = rest.split(':').next().unwrap_or(rest).to_string();
        Ok((false, host, rest.to_string()))
    } else {
        // bare host:port
        let host = url.split(':').next().unwrap_or(url).to_string();
        Ok((false, host, url.to_string()))
    }
}

// ── TLS ───────────────────────────────────────────────────────────────────────

async fn make_tls_stream(
    host: &str,
    addr: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    let mut root_store = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs()? {
        root_store.add(cert).ok();
    }
    if root_store.is_empty() {
        anyhow::bail!("No native TLS root certificates found — cannot establish TLS connection");
    }

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("Cannot connect to NATS at {addr}"))?;

    let server_name = ServerName::try_from(host.to_string())
        .with_context(|| format!("Invalid TLS server name: {host}"))?;

    connector
        .connect(server_name, stream)
        .await
        .with_context(|| format!("TLS handshake failed with {host}"))
}

// ── CONNECT message ───────────────────────────────────────────────────────────

fn build_connect(tls: bool, token: Option<&str>) -> String {
    let auth = match token {
        Some(t) => format!(",\"auth_token\":\"{}\"", t.replace('"', "\\\"")),
        None => String::new(),
    };
    format!("CONNECT {{\"verbose\":false,\"pedantic\":false,\"tls_required\":{tls}{auth}}}\r\n")
}

// ── Writer task ───────────────────────────────────────────────────────────────

async fn write_loop(
    mut write_half: Box<dyn AsyncWrite + Unpin + Send + 'static>,
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
    read_half: Box<dyn AsyncRead + Unpin + Send + 'static>,
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_bare() {
        let (tls, host, addr) = parse_url("127.0.0.1:4222").unwrap();
        assert!(!tls);
        assert_eq!(host, "127.0.0.1");
        assert_eq!(addr, "127.0.0.1:4222");
    }

    #[test]
    fn parse_url_nats_scheme() {
        let (tls, host, addr) = parse_url("nats://my-nats.example.com:4222").unwrap();
        assert!(!tls);
        assert_eq!(host, "my-nats.example.com");
        assert_eq!(addr, "my-nats.example.com:4222");
    }

    #[test]
    fn parse_url_tls_scheme() {
        let (tls, host, addr) = parse_url("tls://secure-nats.example.com:4222").unwrap();
        assert!(tls);
        assert_eq!(host, "secure-nats.example.com");
        assert_eq!(addr, "secure-nats.example.com:4222");
    }

    #[test]
    fn build_connect_no_auth() {
        let msg = build_connect(false, None);
        assert!(msg.contains("\"tls_required\":false"));
        assert!(!msg.contains("auth_token"));
    }

    #[test]
    fn build_connect_with_token() {
        let msg = build_connect(false, Some("secret123"));
        assert!(msg.contains("\"auth_token\":\"secret123\""));
    }

    #[test]
    fn build_connect_tls_with_token() {
        let msg = build_connect(true, Some("tok"));
        assert!(msg.contains("\"tls_required\":true"));
        assert!(msg.contains("\"auth_token\":\"tok\""));
    }

    #[test]
    fn build_connect_escapes_quotes_in_token() {
        let msg = build_connect(false, Some("tok\"en"));
        assert!(msg.contains("\"auth_token\":\"tok\\\"en\""));
    }

    #[test]
    fn nats_scheme_resolves_same_as_bare() {
        let (tls1, host1, addr1) = parse_url("my-nats.example.com:4222").unwrap();
        let (tls2, host2, addr2) = parse_url("nats://my-nats.example.com:4222").unwrap();
        assert_eq!(tls1, tls2);
        assert_eq!(host1, host2);
        assert_eq!(addr1, addr2);
    }
}
