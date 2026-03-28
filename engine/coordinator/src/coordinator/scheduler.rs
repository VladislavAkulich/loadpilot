/// Phase/RPS computation, task selection, and HTTP execution helpers.
use anyhow::Result;

use crate::plan::{HttpMethod, Mode, ScenarioPlan, TaskPlan};

use super::Phase;

// ── Phase + RPS ───────────────────────────────────────────────────────────────

/// Compute the current (phase, target_rps) for the given elapsed time and plan.
///
/// Modes:
///   constant — full RPS immediately, no ramp.
///   ramp     — linear ramp from 0 to target over ramp_up_secs, then steady.
///   step     — divide duration_secs into `steps` equal windows; each window
///              runs at rps * step_number / steps.
///   spike    — divide duration_secs into thirds: baseline (20% rps) →
///              peak (100% rps) → recovery (20% rps).
pub(super) fn compute_phase_and_rps(elapsed_secs: f64, plan: &ScenarioPlan) -> (Phase, f64) {
    let rps = plan.rps as f64;
    match plan.mode {
        Mode::Constant => (Phase::Steady, rps),
        Mode::Ramp => {
            let ramp = plan.ramp_up_secs as f64;
            if ramp > 0.0 && elapsed_secs < ramp {
                (Phase::RampUp, rps * elapsed_secs / ramp)
            } else {
                (Phase::Steady, rps)
            }
        }
        Mode::Step => {
            let steps = plan.steps.max(1) as f64;
            let total = plan.duration_secs as f64;
            let step_dur = total / steps;
            let step_idx = (elapsed_secs / step_dur).floor().min(steps - 1.0);
            (Phase::Steady, rps * (step_idx + 1.0) / steps)
        }
        Mode::Spike => {
            let total = plan.duration_secs as f64;
            let t1 = total / 3.0;
            let t2 = 2.0 * total / 3.0;
            let baseline = (rps * 0.2).max(1.0);
            if elapsed_secs < t1 {
                (Phase::Steady, baseline)
            } else if elapsed_secs < t2 {
                (Phase::RampUp, rps)
            } else {
                (Phase::RampDown, baseline)
            }
        }
    }
}

// ── Task selection ────────────────────────────────────────────────────────────

pub(super) fn pick_task(tasks: &[TaskPlan], index: usize) -> Option<&TaskPlan> {
    if tasks.is_empty() {
        return None;
    }
    let total_weight: u32 = tasks.iter().map(|t| t.weight).sum();
    if total_weight == 0 {
        return Some(&tasks[index % tasks.len()]);
    }
    let slot = (index as u32) % total_weight;
    let mut cumulative = 0u32;
    for t in tasks {
        cumulative += t.weight;
        if slot < cumulative {
            return Some(t);
        }
    }
    Some(&tasks[tasks.len() - 1])
}

// ── HTTP execution ────────────────────────────────────────────────────────────

/// Response data returned from a live HTTP request.
pub(super) struct HttpResponse {
    pub(super) status: u16,
    #[allow(dead_code)]
    pub(super) headers: std::collections::HashMap<String, String>,
    #[allow(dead_code)]
    pub(super) body: String,
}

pub(super) async fn execute_request(
    client: &reqwest::Client,
    method: HttpMethod,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    body: Option<&str>,
) -> Result<HttpResponse> {
    let mut req = match method {
        HttpMethod::Get => client.get(url),
        HttpMethod::Post => client.post(url),
        HttpMethod::Put => client.put(url),
        HttpMethod::Patch => client.patch(url),
        HttpMethod::Delete => client.delete(url),
    };

    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let resp_headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    // Body intentionally not read in static mode — no check_* method runs here,
    // so allocating the response body on every request is unnecessary overhead.
    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body: String::new(),
    })
}
