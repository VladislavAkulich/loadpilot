"""Parse benchmark results and generate a comparison report."""

from __future__ import annotations

import csv
import json
from pathlib import Path
from datetime import datetime, timezone

RESULTS = Path(__file__).parent / "results"


# ── Parsers ───────────────────────────────────────────────────────────────────

def _loadpilot(prefix: str) -> dict | None:
    p = RESULTS / f"{prefix}_loadpilot.json"
    if not p.exists():
        return None
    d = json.loads(p.read_text())
    return {
        "tool": "LoadPilot",
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
        # value is failure ratio (0.0–1.0)
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


def _html_row(d: dict) -> str:
    return (
        f"<tr>"
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


def _html_table(rows: list[dict]) -> str:
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
        + "\n".join(_html_row(r) for r in rows)
        + "</tbody></table>"
    )


def _print_table(label: str, rows: list[dict]) -> None:
    print(f"\n{label}")
    print(f"  {'Tool':<12} {'RPS':>8} {'p50':>8} {'p95':>8} {'p99':>8} {'errors':>8}")
    print("  " + "-" * 54)
    for r in rows:
        print(
            f"  {r['tool']:<12}"
            f" {_fmt(r['rps_actual']):>8}"
            f" {_fmt(r['p50_ms'], 'ms'):>8}"
            f" {_fmt(r['p95_ms'], 'ms'):>8}"
            f" {_fmt(r['p99_ms'], 'ms'):>8}"
            f" {_fmt(r['error_rate_pct'], '%'):>8}"
        )


# ── Main ──────────────────────────────────────────────────────────────────────

def _loadpilot_pyo3(variant: str) -> dict | None:
    p = RESULTS / f"pyo3_{variant}_loadpilot.json"
    if not p.exists():
        return None
    d = json.loads(p.read_text())
    label = {"onstart": "LoadPilot + on_start", "full": "LoadPilot + on_start + check_*"}
    return {
        "tool": label.get(variant, variant),
        "rps_actual": d.get("rps_actual"),
        "p50_ms": d.get("p50_ms"),
        "p95_ms": d.get("p95_ms"),
        "p99_ms": d.get("p99_ms"),
        "max_ms": d.get("max_ms"),
        "error_rate_pct": d.get("error_rate_pct"),
        "requests_total": d.get("requests_total"),
    }


def generate() -> None:
    precision = [r for r in [_loadpilot("precision"), _k6("precision"), _locust("precision")] if r]
    max_tp    = [r for r in [_loadpilot("max"),       _k6("max"),       _locust("max")]       if r]

    # PyO3 section: static baseline + on_start + on_start+check_*
    static = _loadpilot("precision")
    if static:
        static = {**static, "tool": "LoadPilot static (no callbacks)"}
    pyo3 = [r for r in [static, _loadpilot_pyo3("onstart"), _loadpilot_pyo3("full")] if r]

    ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>LoadPilot Benchmark</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 960px; margin: 40px auto; padding: 0 20px; color: #1a1a1a; }}
  h1 {{ font-size: 1.6rem; margin-bottom: 4px; }}
  h2 {{ font-size: 1.1rem; margin: 32px 0 8px; }}
  .meta {{ color: #666; font-size: 0.85rem; margin-bottom: 32px; }}
  table {{ width: 100%; border-collapse: collapse; margin-bottom: 8px; }}
  th {{ text-align: left; padding: 9px 13px; background: #f4f4f4; border-bottom: 2px solid #ddd; font-size: 0.8rem; text-transform: uppercase; letter-spacing: .04em; }}
  td {{ padding: 9px 13px; border-bottom: 1px solid #eee; }}
  tr:last-child td {{ border-bottom: none; }}
  .note {{ color: #555; font-size: 0.83rem; line-height: 1.65; margin-top: 8px; }}
  .section {{ margin-bottom: 40px; }}
</style>
</head>
<body>
<h1>LoadPilot Benchmark</h1>
<p class="meta">Target: Rust/axum server · Docker · {ts}</p>

<div class="section">
  <h2>Precision — 500 RPS target, 30s</h2>
  <p class="note">All tools configured to deliver exactly 500 RPS.
  Measures accuracy and load generator latency overhead.</p>
  {_html_table(precision)}
</div>

<div class="section">
  <h2>Max throughput — 30s constant</h2>
  <p class="note">Each tool runs at maximum capacity (LoadPilot static mode, pure Rust).</p>
  {_html_table(max_tp)}
</div>

<div class="section">
  <h2>PyO3 mode — Python callbacks cost (LoadPilot only, 500 RPS target)</h2>
  <p class="note">Shows the throughput impact of enabling Python callbacks.
  Static mode: pure Rust, no Python.
  on_start only: Python runs once per VUser before tasks; tasks run via Python task runner.
  on_start + check_*: full Python round-trip — task + response assertion per request.</p>
  {_html_table(pyo3)}
</div>

<p class="note">
  <strong>Methodology:</strong> tools run sequentially with a 10s cooldown.
  All containers share the same Docker bridge network.
  Reproduce: <code>cd bench &amp;&amp; ./run.sh</code>
</p>
</body>
</html>"""

    out = RESULTS / "report.html"
    out.write_text(html, encoding="utf-8")

    _print_table("Precision (500 RPS target):", precision)
    _print_table("Max throughput:", max_tp)
    if pyo3:
        _print_table("PyO3 mode — Python callbacks cost:", pyo3)
    print(f"\nReport → {out}")


if __name__ == "__main__":
    generate()
