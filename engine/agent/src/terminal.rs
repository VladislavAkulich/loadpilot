#![allow(dead_code)]
/// Terminal stats display for standalone agent mode (v2).
///
/// In MVP, terminal rendering is done on the Python side using Rich.
/// This module provides a Rust-native fallback using crossterm/stdout.
///
/// Note: crossterm is not in the agent Cargo.toml for MVP (avoids an extra
/// dependency). Add it when implementing standalone agent terminal UI.
use std::fmt::Write as FmtWrite;

/// Render a one-line progress summary to stdout.
/// Suitable for piping to the Python CLI or for standalone agent logging.
pub fn render_one_line(
    scenario_name: &str,
    elapsed_secs: f64,
    duration_secs: f64,
    current_rps: f64,
    target_rps: f64,
    requests_total: u64,
    errors_total: u64,
    p50_ms: f64,
    p99_ms: f64,
) -> String {
    let mut s = String::new();
    let elapsed_m = (elapsed_secs / 60.0) as u64;
    let elapsed_s = (elapsed_secs % 60.0) as u64;
    let total_m = (duration_secs / 60.0) as u64;
    let total_s = (duration_secs % 60.0) as u64;

    let progress = if duration_secs > 0.0 {
        (elapsed_secs / duration_secs * 100.0).min(100.0) as u32
    } else {
        0
    };

    let bar_filled = (progress / 5) as usize;
    let bar_empty = 20usize.saturating_sub(bar_filled);
    let bar: String = "█".repeat(bar_filled) + &"░".repeat(bar_empty);

    let error_pct = if requests_total > 0 {
        errors_total as f64 / requests_total as f64 * 100.0
    } else {
        0.0
    };

    let _ = write!(
        s,
        "LoadPilot — {name}  [{em:02}:{es:02}/{tm:02}:{ts:02}]  \
         rps: {crps:.0}/{trps:.0}  total: {total}  err: {epct:.1}%  \
         p50: {p50:.0}ms  p99: {p99:.0}ms  [{bar}] {prog}%",
        name = scenario_name,
        em = elapsed_m,
        es = elapsed_s,
        tm = total_m,
        ts = total_s,
        crps = current_rps,
        trps = target_rps,
        total = requests_total,
        epct = error_pct,
        p50 = p50_ms,
        p99 = p99_ms,
        bar = bar,
        prog = progress,
    );
    s
}
