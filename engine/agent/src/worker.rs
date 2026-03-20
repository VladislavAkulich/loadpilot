#![allow(dead_code)]
/// Tokio HTTP workers that execute individual requests.
///
/// In MVP mode, equivalent logic lives in coordinator/src/coordinator.rs.
/// This module is the v2 refactor target so agents can run workers independently.
use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;

/// Execute a single HTTP request and return (status_code, latency_ms).
pub async fn execute_request(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
) -> Result<(u16, u64)> {
    let mut req = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        other => return Err(anyhow::anyhow!("Unknown HTTP method: {}", other)),
    };

    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }

    let t0 = Instant::now();
    let resp = req.send().await?;
    let latency_ms = t0.elapsed().as_millis() as u64;
    Ok((resp.status().as_u16(), latency_ms))
}

/// Worker pool configuration.
pub struct WorkerConfig {
    pub target_rps: f64,
    pub ramp_up_secs: f64,
    pub duration_secs: f64,
    pub concurrency: usize,
    pub base_url: String,
}

impl WorkerConfig {
    pub fn max_concurrency(&self) -> usize {
        (self.target_rps as usize * 4).max(100)
    }
}
