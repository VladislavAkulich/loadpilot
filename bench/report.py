"""Parse benchmark results and generate a comparison report."""

from __future__ import annotations

import csv
import json
from pathlib import Path
from datetime import datetime, timezone

RESULTS = Path(__file__).parent / "results"


# ── Parsers ───────────────────────────────────────────────────────────────────

def _loadpilot(prefix: str, label: str | None = None) -> dict | None:
    p = RESULTS / f"{prefix}_loadpilot.json"
    if not p.exists():
        return None
    d = json.loads(p.read_text())
    return {
        "tool": label or "LoadPilot",
        "rps_actual": d.get("rps_actual"),
        "p50_ms": d.get("p50_ms"),
        "p95_ms": d.get("p95_ms"),
        "p99_ms": d.get("p99_ms"),
        "max_ms": d.get("max_ms"),
        "error_rate_pct": d.get("error_rate_pct"),
        "requests_total": d.get("requests_total"),
    }


def _loadpilot_file(filename: str, label: str) -> dict | None:
    p = RESULTS / filename
    if not p.exists():
        return None
    d = json.loads(p.read_text())
    return {
        "tool": label,
        "rps_actual": d.get("rps_actual"),
        "p50_ms": d.get("p50_ms"),
        "p95_ms": d.get("p95_ms"),
        "p99_ms": d.get("p99_ms"),
        "max_ms": d.get("max_ms"),
        "error_rate_pct": d.get("error_rate_pct"),
        "requests_total": d.get("requests_total"),
    }


def _k6(prefix: str) -> dict | None:
    p = RESULTS / f"{prefix}_k6.json"
    if not p.exists():
        return None
    d = json.loads(p.read_text())
    m = d.get("metrics", {})

    dur = m.get("http_req_duration", {})
    reqs = m.get("http_reqs", {})
    failed = m.get("http_req_failed", {})

    return {
        "tool": "k6",
        "rps_actual": round(reqs.get("rate", 0.0), 2),
        "p50_ms": round(dur.get("p(50)", 0.0), 2),
        "p95_ms": round(dur.get("p(95)", 0.0), 2),
        "p99_ms": round(dur.get("p(99)", 0.0), 2),
        "max_ms": round(dur.get("max", 0.0), 2),
        "error_rate_pct": round(failed.get("value", 0.0) * 100, 3),
        "requests_total": int(reqs.get("count", 0)),
    }


def _locust(prefix: str) -> dict | None:
    p = RESULTS / f"{prefix}_locust_stats.csv"
    if not p.exists():
        return None
    with p.open() as f:
        rows = list(csv.DictReader(f))
    agg = next((r for r in rows if r.get("Name") == "Aggregated"), rows[-1] if rows else None)
    if not agg:
        return None
    total = int(agg.get("Request Count", 0))
    failures = int(agg.get("Failure Count", 0))
    return {
        "tool": "Locust",
        "rps_actual": round(float(agg.get("Requests/s", 0)), 2),
        "p50_ms": float(agg.get("50%", 0) or 0),
        "p95_ms": float(agg.get("95%", 0) or 0),
        "p99_ms": float(agg.get("99%", 0) or 0),
        "max_ms": float(agg.get("Max Response Time", 0) or 0),
        "error_rate_pct": round(failures / total * 100, 3) if total else 0.0,
        "requests_total": total,
    }


# ── Formatting ────────────────────────────────────────────────────────────────

def _fmt(v: float | int | None, unit: str = "") -> str:
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:.1f}{unit}"
    return f"{v:,}{unit}"


def _html_row(d: dict, highlight: bool = False) -> str:
    style = ' style="background:#f0faf0"' if highlight else ""
    return (
        f"<tr{style}>"
        f"<td><strong>{d['tool']}</strong></td>"
        f"<td>{_fmt(d['rps_actual'])}</td>"
        f"<td>{_fmt(d['p50_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['p95_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['p99_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['max_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['error_rate_pct'], '%')}</td>"
        f"<td>{_fmt(d['requests_total'])}</td>"
        f"</tr>"
    )


def _html_table(rows: list[dict], highlight_tool: str | None = None) -> str:
    if not rows:
        return "<p><em>No results found.</em></p>"
    return (
        "<table>"
        "<thead><tr>"
        "<th>Tool</th><th>RPS actual</th>"
        "<th>p50</th><th>p95</th><th>p99</th><th>max</th>"
        "<th>Error rate</th><th>Requests</th>"
        "</tr></thead>"
        "<tbody>"
        + "\n".join(_html_row(r, highlight=highlight_tool and r["tool"] == highlight_tool) for r in rows)
        + "</tbody></table>"
    )


def _print_table(label: str, rows: list[dict]) -> None:
    print(f"\n{label}")
    print(f"  {'Tool':<36} {'RPS':>8} {'p50':>8} {'p95':>8} {'p99':>8} {'errors':>8}")
    print("  " + "-" * 78)
    for r in rows:
        print(
            f"  {r['tool']:<36}"
            f" {_fmt(r['rps_actual']):>8}"
            f" {_fmt(r['p50_ms'], 'ms'):>8}"
            f" {_fmt(r['p95_ms'], 'ms'):>8}"
            f" {_fmt(r['p99_ms'], 'ms'):>8}"
            f" {_fmt(r['error_rate_pct'], '%'):>8}"
        )


# ── Main ──────────────────────────────────────────────────────────────────────

def generate() -> None:
    precision = [r for r in [_loadpilot("precision"), _k6("precision"), _locust("precision")] if r]
    max_tp    = [r for r in [_loadpilot("max"),       _k6("max"),       _locust("max")]       if r]

    # PyO3 architecture comparison: 4 variants across 2 scenarios (on_start and full).
    # Each variant has a label describing the architecture.
    pyo3_variants_onstart = [r for r in [
        _loadpilot("precision",                        label="Static (no callbacks)"),
        _loadpilot_file("pyo3_onstart_loadpilot.json", "PyO3 — spawn_blocking (old arch)"),
        _loadpilot_file("pyo3_onstart_py312.json",     "PyO3 — persistent threads (new arch)"),
    ] if r]

    pyo3_variants_full = [r for r in [
        _loadpilot("precision",                      label="Static (no callbacks)"),
        _loadpilot_file("pyo3_full_loadpilot.json",  "PyO3 — spawn_blocking (old arch)"),
        _loadpilot_file("pyo3_full_py312.json",      "PyO3 — persistent threads (new arch)"),
    ] if r]

    pyo3_max = [r for r in [
        _loadpilot("max",                                   label="Static (no callbacks)"),
        _loadpilot_file("pyo3_max_onstart_py312.json",      "PyO3 — on_start + async task"),
        _loadpilot_file("pyo3_max_full_py312.json",         "PyO3 — on_start + async task + check_*"),
    ] if r]

    ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>LoadPilot Benchmark</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 980px; margin: 40px auto; padding: 0 20px; color: #1a1a1a; }}
  h1 {{ font-size: 1.6rem; margin-bottom: 4px; }}
  h2 {{ font-size: 1.1rem; margin: 32px 0 8px; }}
  .meta {{ color: #666; font-size: 0.85rem; margin-bottom: 32px; }}
  table {{ width: 100%; border-collapse: collapse; margin-bottom: 8px; }}
  th {{ text-align: left; padding: 9px 13px; background: #f4f4f4; border-bottom: 2px solid #ddd; font-size: 0.8rem; text-transform: uppercase; letter-spacing: .04em; }}
  td {{ padding: 9px 13px; border-bottom: 1px solid #eee; }}
  tr:last-child td {{ border-bottom: none; }}
  .note {{ color: #555; font-size: 0.83rem; line-height: 1.65; margin-top: 8px; }}
  .section {{ margin-bottom: 40px; }}
  .badge {{ display: inline-block; font-size: 0.7rem; padding: 2px 6px; border-radius: 3px;
            background: #d4edda; color: #155724; margin-left: 6px; vertical-align: middle; }}
</style>
</head>
<body>
<h1>LoadPilot Benchmark</h1>
<p class="meta">Target: Rust/axum server · Docker bridge network · {ts}</p>

<div class="section">
  <h2>Precision — 500 RPS target, 30s</h2>
  <p class="note">All tools configured to deliver exactly 500 RPS.
  LoadPilot runs in static mode (pure Rust, no Python callbacks).
  Measures RPS accuracy and load generator latency overhead.</p>
  {_html_table(precision)}
</div>

<div class="section">
  <h2>Max throughput — 30s constant load</h2>
  <p class="note">Each tool runs at maximum capacity.
  LoadPilot in static mode (pure Rust). k6 uses Go goroutines. Locust uses Python/GIL threads.</p>
  {_html_table(max_tp)}
</div>

<div class="section">
  <h2>PyO3 max throughput — Python callbacks cost</h2>
  <p class="note">LoadPilot running flat-out with Python callbacks (persistent threads).
  Compares static mode ceiling vs PyO3 with async tasks.</p>
  {_html_table(pyo3_max, highlight_tool="PyO3 — on_start + async task")}
</div>

<div class="section">
  <h2>PyO3 architecture comparison — on_start scenario, 500 RPS target</h2>
  <p class="note">
    One Python <code>on_start</code> callback per VUser (login/setup), then Rust tasks.<br>
    <strong>spawn_blocking (old):</strong> each task spun up a new blocking thread and called <code>Python::attach</code> — ~50–100µs overhead per task.<br>
    <strong>persistent threads (new):</strong> one OS thread per VUser, <code>Python::attach</code> called per message only — channel overhead ~1–5µs.
  </p>
  {_html_table(pyo3_variants_onstart, highlight_tool="PyO3 — persistent threads (new arch)")}
</div>

<div class="section">
  <h2>PyO3 architecture comparison — on_start + check_* scenario, 500 RPS target</h2>
  <p class="note">
    Python <code>on_start</code> per VUser <em>and</em> <code>check_&lt;task&gt;</code> response assertion per request.
    Full Python round-trip every task.
  </p>
  {_html_table(pyo3_variants_full, highlight_tool="PyO3 — persistent threads (new arch)")}
</div>

<p class="note">
  <strong>Methodology:</strong> tools run sequentially with a 10s cooldown between runs.
  All containers share the same Docker bridge network.
  Reproduce: <code>cd bench &amp;&amp; ./run.sh</code>
</p>
</body>
</html>"""

    out = RESULTS / "report.html"
    out.write_text(html, encoding="utf-8")

    _print_table("Precision (500 RPS target):", precision)
    _print_table("Max throughput:", max_tp)
    if pyo3_max:
        _print_table("PyO3 max throughput:", pyo3_max)
    if pyo3_variants_onstart:
        _print_table("PyO3 — on_start scenario (500 RPS):", pyo3_variants_onstart)
    if pyo3_variants_full:
        _print_table("PyO3 — on_start + check_* scenario (500 RPS):", pyo3_variants_full)
    print(f"\nReport → {out}")


if __name__ == "__main__":
    generate()
