"""Parse benchmark results and generate a comparison report."""

from __future__ import annotations

import csv
import json
from pathlib import Path
from datetime import datetime, timezone

RESULTS = Path(__file__).parent / "results"


# ── Resource usage ─────────────────────────────────────────────────────────────

def _resources(profile: str) -> dict:
    """Return resource stats for a profile, or empty dict if not collected yet."""
    p = RESULTS / f"resources_{profile}.json"
    if not p.exists():
        return {}
    return json.loads(p.read_text())


def _enrich(row: dict | None, profile: str) -> dict | None:
    """Merge resource stats into a result row."""
    if row is None:
        return None
    return {**row, **_resources(profile)}


# ── Parsers ────────────────────────────────────────────────────────────────────

def _loadpilot(filename: str, label: str) -> dict | None:
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


def _k6(filename: str) -> dict | None:
    p = RESULTS / filename
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


def _locust(csv_prefix: str) -> dict | None:
    p = RESULTS / f"{csv_prefix}_stats.csv"
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


# ── Formatting ─────────────────────────────────────────────────────────────────

def _fmt(v: float | int | None, unit: str = "") -> str:
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:.1f}{unit}"
    return f"{v:,}{unit}"


def _fmt_mem(mb: float | None) -> str:
    if mb is None:
        return "—"
    if mb >= 1024:
        return f"{mb/1024:.1f} GB"
    return f"{mb:.0f} MB"


def _html_row(d: dict, highlight: bool = False, show_resources: bool = False) -> str:
    style = ' style="background:#f0faf0"' if highlight else ""
    cells = (
        f"<td><strong>{d['tool']}</strong></td>"
        f"<td>{_fmt(d['rps_actual'])}</td>"
        f"<td>{_fmt(d['p50_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['p95_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['p99_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['max_ms'], 'ms')}</td>"
        f"<td>{_fmt(d['error_rate_pct'], '%')}</td>"
        f"<td>{_fmt(d['requests_total'])}</td>"
    )
    if show_resources:
        cells += (
            f"<td>{_fmt(d.get('cpu_avg_pct'), '%')}</td>"
            f"<td>{_fmt(d.get('cpu_peak_pct'), '%')}</td>"
            f"<td>{_fmt_mem(d.get('mem_peak_mb'))}</td>"
        )
    return f"<tr{style}>{cells}</tr>"


def _html_table(rows: list[dict], highlight_tool: str | None = None) -> str:
    if not rows:
        return "<p><em>No results found.</em></p>"
    show_resources = any("cpu_avg_pct" in r for r in rows)
    header = (
        "<thead><tr>"
        "<th>Tool</th><th>RPS actual</th>"
        "<th>p50</th><th>p95</th><th>p99</th><th>max</th>"
        "<th>Error rate</th><th>Requests</th>"
    )
    if show_resources:
        header += "<th>CPU avg</th><th>CPU peak</th><th>Mem peak</th>"
    header += "</tr></thead>"
    return (
        "<table>"
        + header
        + "<tbody>"
        + "\n".join(
            _html_row(r, highlight=bool(highlight_tool and r["tool"] == highlight_tool),
                      show_resources=show_resources)
            for r in rows
        )
        + "</tbody></table>"
    )


def _print_table(label: str, rows: list[dict]) -> None:
    show_resources = any("cpu_avg_pct" in r for r in rows)
    print(f"\n{label}")
    if show_resources:
        print(f"  {'Tool':<36} {'RPS':>8} {'p50':>8} {'p95':>8} {'p99':>8} {'errors':>8} {'CPU avg':>9} {'CPU peak':>9} {'Mem peak':>10}")
        print("  " + "-" * 104)
        for r in rows:
            print(
                f"  {r['tool']:<36}"
                f" {_fmt(r['rps_actual']):>8}"
                f" {_fmt(r['p50_ms'], 'ms'):>8}"
                f" {_fmt(r['p95_ms'], 'ms'):>8}"
                f" {_fmt(r['p99_ms'], 'ms'):>8}"
                f" {_fmt(r['error_rate_pct'], '%'):>8}"
                f" {_fmt(r.get('cpu_avg_pct'), '%'):>9}"
                f" {_fmt(r.get('cpu_peak_pct'), '%'):>9}"
                f" {_fmt_mem(r.get('mem_peak_mb')):>10}"
            )
    else:
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


# ── Main ───────────────────────────────────────────────────────────────────────

def generate() -> None:
    # ── Cross-tool: precision (500 RPS target) ─────────────────────────────────
    precision = [r for r in [
        _enrich(_loadpilot("precision_pyo3_full.json", "LoadPilot (PyO3)"), "loadpilot-pyo3-full"),
        _enrich(_k6("precision_k6.json"),                                   "k6-precision"),
        _enrich(_locust("precision_locust"),                                "locust-precision"),
    ] if r]

    # ── Cross-tool: max throughput ─────────────────────────────────────────────
    max_tp = [r for r in [
        _enrich(_loadpilot("max_pyo3_full.json", "LoadPilot (PyO3)"), "loadpilot-pyo3-max-full"),
        _enrich(_k6("max_k6.json"),                                   "k6-max"),
        _enrich(_locust("max_locust"),                                "locust-max"),
    ] if r]

    # ── PyO3 precision: cost of Python callbacks at 500 RPS ───────────────────
    pyo3_precision = [r for r in [
        _enrich(_loadpilot("precision_loadpilot.json",    "Static (no callbacks)"),   "loadpilot-precision"),
        _enrich(_loadpilot("precision_pyo3_onstart.json", "+ on_start"),              "loadpilot-pyo3-onstart"),
        _enrich(_loadpilot("precision_pyo3_full.json",    "+ on_start + check_*"),    "loadpilot-pyo3-full"),
    ] if r]

    # ── PyO3 max: architecture experiments at full throttle ───────────────────
    pyo3_max = [r for r in [
        _enrich(_loadpilot("max_loadpilot.json",       "Static (no callbacks)"),      "loadpilot-max"),
        _enrich(_loadpilot("max_pyo3_onstart.json",    "async task (coro.send)"),     "loadpilot-pyo3-max-onstart"),
        _enrich(_loadpilot("max_pyo3_full.json",       "async task + check_*"),       "loadpilot-pyo3-max-full"),
        _enrich(_loadpilot("max_pyo3_sync.json",       "sync def task"),              "loadpilot-pyo3-max-sync"),
        _enrich(_loadpilot("max_pyo3_batch5.json",     "client.batch(5)"),            "loadpilot-pyo3-batch5"),
    ] if r]

    ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>LoadPilot Benchmark</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 1080px; margin: 40px auto; padding: 0 20px; color: #1a1a1a; }}
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
<p class="meta">Target: Rust/axum server · Docker bridge network · {ts}</p>

<div class="section">
  <h2>Precision — 500 RPS target, 30s</h2>
  <p class="note">All tools configured to deliver exactly 500 RPS.
  LoadPilot runs in PyO3 mode (on_start + check_*): realistic scenario with Python callbacks.
  Measures RPS accuracy, latency overhead, and resource cost of the load generator itself.</p>
  {_html_table(precision)}
</div>

<div class="section">
  <h2>Max throughput — 30s constant load</h2>
  <p class="note">Each tool runs at maximum capacity for 30s.
  LoadPilot in PyO3 mode (on_start + check_*). k6 uses Go goroutines. Locust uses Python/GIL threads.
  Resource columns show the generator's footprint at full speed.</p>
  {_html_table(max_tp)}
</div>

<div class="section">
  <h2>PyO3 precision — 500 RPS, Python callback cost</h2>
  <p class="note">Cost of enabling Python callbacks at 500 RPS.
  Persistent VUser threads pay <code>Python::attach</code> once per task message;
  <code>py.detach()</code> releases the GIL for the HTTP call so all threads run concurrently.</p>
  {_html_table(pyo3_precision)}
</div>

<div class="section">
  <h2>PyO3 max throughput — architecture experiments</h2>
  <p class="note">LoadPilot with Python callbacks at full throttle.
  All variants use <code>on_start</code> (login).
  <strong>client.batch(5)</strong> dispatches 5 requests per PyO3 call via a Rust <code>JoinSet</code>,
  releasing the GIL for the entire batch.</p>
  {_html_table(pyo3_max, highlight_tool="client.batch(5)")}
</div>

<p class="note">
  <strong>Methodology:</strong> tools run sequentially with a 10s cooldown between runs.
  All containers share the same Docker bridge network.
  CPU/memory measured via <code>docker stats</code> sampled every 1s during the run.
  Reproduce: <code>cd bench &amp;&amp; ./run.sh</code>
</p>
</body>
</html>"""

    out = RESULTS / "report.html"
    out.write_text(html, encoding="utf-8")

    _print_table("Precision (500 RPS target):", precision)
    _print_table("Max throughput:", max_tp)
    if pyo3_precision:
        _print_table("PyO3 precision (500 RPS):", pyo3_precision)
    if pyo3_max:
        _print_table("PyO3 max throughput:", pyo3_max)
    print(f"\nReport → {out}")


if __name__ == "__main__":
    generate()
