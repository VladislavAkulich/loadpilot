"""loadpilot.report — HTML report generator.

Produces a single self-contained HTML file from the metrics snapshots
collected during a test run.  No external files are required; charts are
rendered via Chart.js loaded from a CDN.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

from loadpilot.models import AgentMetrics


def generate(
    snapshots: list[AgentMetrics],
    scenario_name: str,
    target_url: str,
    rps_target: int,
    duration_secs: int,
    ramp_up_secs: int,
    output_path: Path,
    thresholds: dict[str, float] | None = None,
    n_agents: int = 1,
) -> None:
    """Build and write the HTML report to *output_path*."""
    html = _build(
        snapshots,
        scenario_name,
        target_url,
        rps_target,
        duration_secs,
        ramp_up_secs,
        thresholds or {},
        n_agents,
    )
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(html, encoding="utf-8")


# ── Internal builder ──────────────────────────────────────────────────────────

_THRESHOLD_LABELS: dict[str, str] = {
    "p50_ms": "p50 latency",
    "p95_ms": "p95 latency",
    "p99_ms": "p99 latency",
    "max_ms": "max latency",
    "error_rate": "error rate",
}


def _build(
    snapshots: list[AgentMetrics],
    scenario_name: str,
    target_url: str,
    rps_target: int,
    duration_secs: int,
    ramp_up_secs: int,
    thresholds: dict[str, float] | None = None,
    n_agents: int = 1,
) -> str:
    # Exclude the final "done" snapshot from charts (rps=0, target=0).
    chart_snaps = [s for s in snapshots if s.phase != "done"]
    final = snapshots[-1] if snapshots else None

    # ── Summary stats ─────────────────────────────────────────────────────────
    total_req = final.requests_total if final else 0
    total_err = final.errors_total if final else 0
    error_rate = (total_err / total_req * 100) if total_req > 0 else 0.0
    peak_rps = max((s.current_rps for s in chart_snaps), default=0.0)
    actual_duration = final.elapsed_secs if final else 0.0
    lat = final.latency if final else None

    # ── Time-series data for charts ───────────────────────────────────────────
    elapsed = [round(s.elapsed_secs, 2) for s in chart_snaps]
    actual_rps = [round(s.current_rps, 2) for s in chart_snaps]
    target_rps_ts = [round(s.target_rps, 2) for s in chart_snaps]
    p50_ts = [round(s.latency.p50_ms, 2) for s in chart_snaps]
    p95_ts = [round(s.latency.p95_ms, 2) for s in chart_snaps]
    p99_ts = [round(s.latency.p99_ms, 2) for s in chart_snaps]

    chart_data = json.dumps(
        {
            "elapsed": elapsed,
            "actual_rps": actual_rps,
            "target_rps": target_rps_ts,
            "p50": p50_ts,
            "p95": p95_ts,
            "p99": p99_ts,
        }
    )

    # ── Derived display values ────────────────────────────────────────────────
    ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    dur_str = _fmt_dur(int(actual_duration))
    err_class = "err" if error_rate >= 5 else ("warn" if error_rate > 0 else "ok")

    lat_p50 = f"{lat.p50_ms:.0f}" if lat else "—"
    lat_p95 = f"{lat.p95_ms:.0f}" if lat else "—"
    lat_p99 = f"{lat.p99_ms:.0f}" if lat else "—"
    lat_max = f"{lat.max_ms:.0f}" if lat else "—"
    lat_min = f"{lat.min_ms:.0f}" if lat else "—"
    lat_mean = f"{lat.mean_ms:.1f}" if lat else "—"

    # ── Threshold rows ─────────────────────────────────────────────────────────
    thresholds_html = ""
    if thresholds:
        actual_vals: dict[str, float] = {
            "p50_ms": lat.p50_ms if lat else 0.0,
            "p95_ms": lat.p95_ms if lat else 0.0,
            "p99_ms": lat.p99_ms if lat else 0.0,
            "max_ms": lat.max_ms if lat else 0.0,
            "error_rate": error_rate,
        }
        rows = []
        for key, limit in thresholds.items():
            value = actual_vals.get(key)
            if value is None:
                continue
            passed = value <= limit
            status_cls = "thr-pass" if passed else "thr-fail"
            icon = "✓" if passed else "✗"
            unit = "%" if key == "error_rate" else "ms"
            label = _THRESHOLD_LABELS.get(key, key)
            rows.append(
                f'<tr class="{status_cls}">'
                f'<td class="thr-icon">{icon}</td>'
                f'<td class="thr-name">{label}</td>'
                f'<td class="thr-actual">{value:.1f}{unit}</td>'
                f'<td class="thr-sep">&lt;</td>'
                f'<td class="thr-limit">{limit:.1f}{unit}</td>'
                f"</tr>"
            )
        if rows:
            thresholds_html = (
                "\n  <!-- Thresholds -->\n"
                '  <div class="section">\n'
                '    <div class="section-title">SLA Thresholds</div>\n'
                '    <table class="thr-table">\n'
                "      <tbody>\n"
                + "\n".join(f"        {r}" for r in rows)
                + "\n      </tbody>\n    </table>\n  </div>"
            )

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>LoadPilot — {scenario_name}</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.4/dist/chart.umd.min.js"></script>
<style>
  :root {{
    --primary: #6366f1;
    --primary-light: #eef2ff;
    --success: #16a34a;
    --error: #dc2626;
    --warn: #d97706;
    --bg: #f1f5f9;
    --card: #ffffff;
    --text: #0f172a;
    --muted: #64748b;
    --border: #e2e8f0;
    --ramp: #fef3c7;
  }}
  *, *::before, *::after {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
          background: var(--bg); color: var(--text); line-height: 1.5; }}
  .wrap {{ max-width: 1120px; margin: 0 auto; padding: 36px 24px; }}

  /* ── Header ── */
  header {{ display: flex; justify-content: space-between; align-items: flex-end;
            margin-bottom: 32px; gap: 16px; flex-wrap: wrap; }}
  .brand {{ font-size: 0.75rem; font-weight: 700; text-transform: uppercase;
            letter-spacing: 0.12em; color: var(--primary); margin-bottom: 6px; }}
  h1 {{ font-size: 1.75rem; font-weight: 800; }}
  .meta {{ font-size: 0.8rem; color: var(--muted); text-align: right; line-height: 1.8; }}
  .meta b {{ color: var(--text); }}

  /* ── Cards ── */
  .cards {{ display: grid; grid-template-columns: repeat(4, 1fr); gap: 16px; margin-bottom: 24px; }}
  .card {{ background: var(--card); border-radius: 14px; padding: 22px 24px;
           border: 1px solid var(--border); }}
  .card-label {{ font-size: 0.7rem; font-weight: 700; text-transform: uppercase;
                 letter-spacing: 0.08em; color: var(--muted); margin-bottom: 10px; }}
  .card-value {{ font-size: 2.1rem; font-weight: 800; line-height: 1; }}
  .card-sub {{ font-size: 0.78rem; color: var(--muted); margin-top: 6px; }}
  .c-ok    .card-value {{ color: var(--success); }}
  .c-err   .card-value {{ color: var(--error); }}
  .c-warn  .card-value {{ color: var(--warn); }}
  .c-blue  .card-value {{ color: var(--primary); }}

  /* ── Latency strip ── */
  .section {{ background: var(--card); border-radius: 14px; padding: 24px 28px;
              border: 1px solid var(--border); margin-bottom: 24px; }}
  .section-title {{ font-size: 0.85rem; font-weight: 700; text-transform: uppercase;
                    letter-spacing: 0.06em; color: var(--muted); margin-bottom: 20px; }}
  .lat-grid {{ display: grid; grid-template-columns: repeat(6, 1fr); gap: 8px; }}
  .lat-cell {{ text-align: center; padding: 12px 6px;
               border-radius: 10px; background: var(--bg); }}
  .lat-label {{ font-size: 0.65rem; font-weight: 700; text-transform: uppercase;
                letter-spacing: 0.06em; color: var(--muted); }}
  .lat-value {{ font-size: 1.5rem; font-weight: 800; margin-top: 4px; color: var(--text); }}
  .lat-unit  {{ font-size: 0.65rem; color: var(--muted); }}
  .lat-cell.p99 .lat-value {{ color: var(--error); }}
  .lat-cell.p95 .lat-value {{ color: var(--warn); }}
  .lat-cell.p50 .lat-value {{ color: var(--success); }}

  /* ── Charts ── */
  .charts {{ display: grid; grid-template-columns: 1fr 1fr; gap: 24px; margin-bottom: 24px; }}
  .chart-card {{ background: var(--card); border-radius: 14px; padding: 24px 28px;
                 border: 1px solid var(--border); }}
  .chart-card h2 {{ font-size: 0.85rem; font-weight: 700; text-transform: uppercase;
                    letter-spacing: 0.06em; color: var(--muted); margin-bottom: 18px; }}

  /* ── Config strip ── */
  .config {{ display: flex; gap: 32px; flex-wrap: wrap; }}
  .config-item {{ display: flex; flex-direction: column; }}
  .config-label {{ font-size: 0.65rem; font-weight: 700; text-transform: uppercase;
                   letter-spacing: 0.06em; color: var(--muted); }}
  .config-value {{ font-size: 0.95rem; font-weight: 600; margin-top: 2px; }}

  /* ── Thresholds table ── */
  .thr-table {{ width: 100%; border-collapse: collapse; font-size: 0.9rem; }}
  .thr-table td {{ padding: 8px 12px; }}
  .thr-icon {{ font-weight: 800; font-size: 1rem; width: 28px; }}
  .thr-name {{ font-weight: 600; color: var(--text); min-width: 120px; }}
  .thr-actual {{ font-weight: 700; text-align: right; width: 90px; }}
  .thr-sep {{ color: var(--muted); text-align: center; width: 24px; }}
  .thr-limit {{ color: var(--muted); width: 90px; }}
  .thr-pass .thr-icon {{ color: var(--success); }}
  .thr-pass .thr-actual {{ color: var(--success); }}
  .thr-fail .thr-icon {{ color: var(--error); }}
  .thr-fail .thr-actual {{ color: var(--error); }}
  .thr-table tr:not(:last-child) td {{ border-bottom: 1px solid var(--border); }}

  /* ── Footer ── */
  footer {{ text-align: center; color: var(--muted); font-size: 0.75rem; padding: 24px 0 8px; }}
  footer a {{ color: var(--primary); text-decoration: none; }}

  @media (max-width: 800px) {{
    .cards  {{ grid-template-columns: repeat(2, 1fr); }}
    .charts {{ grid-template-columns: 1fr; }}
    .lat-grid {{ grid-template-columns: repeat(3, 1fr); }}
  }}
</style>
</head>
<body>
<div class="wrap">

  <header>
    <div>
      <div class="brand">LoadPilot Report</div>
      <h1>{scenario_name}</h1>
    </div>
    <div class="meta">
      <div><b>Target</b> {target_url}</div>
      {'<div><b>Mode</b> <span style="color:var(--primary);font-weight:700;">distributed &times; ' + str(n_agents) + " agents</span></div>" if n_agents > 1 else ""}
      <div><b>Generated</b> {ts}</div>
    </div>
  </header>

  <!-- Summary cards -->
  <div class="cards">
    <div class="card c-blue">
      <div class="card-label">Total Requests</div>
      <div class="card-value">{total_req:,}</div>
      <div class="card-sub">across {dur_str}</div>
    </div>
    <div class="card c-{err_class}">
      <div class="card-label">Error Rate</div>
      <div class="card-value">{error_rate:.1f}%</div>
      <div class="card-sub">{total_err:,} failed</div>
    </div>
    <div class="card c-blue">
      <div class="card-label">Peak RPS</div>
      <div class="card-value">{peak_rps:.0f}</div>
      <div class="card-sub">target {rps_target} RPS</div>
    </div>
    <div class="card c-blue">
      <div class="card-label">Duration</div>
      <div class="card-value">{dur_str}</div>
      <div class="card-sub">ramp-up {_fmt_dur(ramp_up_secs)}</div>
    </div>
  </div>

{thresholds_html}

  <!-- Latency strip -->
  <div class="section">
    <div class="section-title">Latency</div>
    <div class="lat-grid">
      <div class="lat-cell p50">
        <div class="lat-label">p50</div>
        <div class="lat-value">{lat_p50}</div>
        <div class="lat-unit">ms</div>
      </div>
      <div class="lat-cell p95">
        <div class="lat-label">p95</div>
        <div class="lat-value">{lat_p95}</div>
        <div class="lat-unit">ms</div>
      </div>
      <div class="lat-cell p99">
        <div class="lat-label">p99</div>
        <div class="lat-value">{lat_p99}</div>
        <div class="lat-unit">ms</div>
      </div>
      <div class="lat-cell">
        <div class="lat-label">max</div>
        <div class="lat-value">{lat_max}</div>
        <div class="lat-unit">ms</div>
      </div>
      <div class="lat-cell">
        <div class="lat-label">min</div>
        <div class="lat-value">{lat_min}</div>
        <div class="lat-unit">ms</div>
      </div>
      <div class="lat-cell">
        <div class="lat-label">mean</div>
        <div class="lat-value">{lat_mean}</div>
        <div class="lat-unit">ms</div>
      </div>
    </div>
  </div>

  <!-- Charts -->
  <div class="charts">
    <div class="chart-card">
      <h2>Requests per Second</h2>
      <canvas id="rpsChart"></canvas>
    </div>
    <div class="chart-card">
      <h2>Latency over Time</h2>
      <canvas id="latChart"></canvas>
    </div>
  </div>

  <!-- Test config -->
  <div class="section">
    <div class="section-title">Configuration</div>
    <div class="config">
      <div class="config-item">
        <span class="config-label">Scenario</span>
        <span class="config-value">{scenario_name}</span>
      </div>
      <div class="config-item">
        <span class="config-label">Target URL</span>
        <span class="config-value">{target_url}</span>
      </div>
      <div class="config-item">
        <span class="config-label">Target RPS</span>
        <span class="config-value">{rps_target}</span>
      </div>
      <div class="config-item">
        <span class="config-label">Duration</span>
        <span class="config-value">{_fmt_dur(duration_secs)}</span>
      </div>
      <div class="config-item">
        <span class="config-label">Ramp-up</span>
        <span class="config-value">{_fmt_dur(ramp_up_secs)}</span>
      </div>
      <div class="config-item">
        <span class="config-label">Agents</span>
        <span class="config-value">{"distributed (" + str(n_agents) + ")" if n_agents > 1 else "single"}</span>
      </div>
    </div>
  </div>

  <footer>Generated by <a href="https://github.com/loadpilot/loadpilot">LoadPilot</a></footer>
</div>

<script>
const D = {chart_data};

const rpsCtx = document.getElementById("rpsChart");
new Chart(rpsCtx, {{
  type: "line",
  data: {{
    labels: D.elapsed,
    datasets: [
      {{
        label: "Actual RPS",
        data: D.actual_rps,
        borderColor: "#6366f1",
        backgroundColor: "rgba(99,102,241,0.08)",
        borderWidth: 2,
        pointRadius: 0,
        fill: true,
        tension: 0.3,
      }},
      {{
        label: "Target RPS",
        data: D.target_rps,
        borderColor: "#94a3b8",
        borderWidth: 1.5,
        borderDash: [5, 4],
        pointRadius: 0,
        fill: false,
        tension: 0.3,
      }},
    ],
  }},
  options: {{
    animation: false,
    plugins: {{ legend: {{ position: "bottom", labels: {{ boxWidth: 12, padding: 16 }} }} }},
    scales: {{
      x: {{ title: {{ display: true, text: "elapsed (s)", color: "#94a3b8" }},
             grid: {{ color: "#f1f5f9" }}, ticks: {{ maxTicksLimit: 10 }} }},
      y: {{ title: {{ display: true, text: "RPS", color: "#94a3b8" }},
             grid: {{ color: "#f1f5f9" }}, beginAtZero: true }},
    }},
  }},
}});

const latCtx = document.getElementById("latChart");
new Chart(latCtx, {{
  type: "line",
  data: {{
    labels: D.elapsed,
    datasets: [
      {{
        label: "p50",
        data: D.p50,
        borderColor: "#16a34a",
        borderWidth: 2,
        pointRadius: 0,
        fill: false,
        tension: 0.3,
      }},
      {{
        label: "p95",
        data: D.p95,
        borderColor: "#d97706",
        borderWidth: 2,
        pointRadius: 0,
        fill: false,
        tension: 0.3,
      }},
      {{
        label: "p99",
        data: D.p99,
        borderColor: "#dc2626",
        borderWidth: 2,
        pointRadius: 0,
        fill: false,
        tension: 0.3,
      }},
    ],
  }},
  options: {{
    animation: false,
    plugins: {{ legend: {{ position: "bottom", labels: {{ boxWidth: 12, padding: 16 }} }} }},
    scales: {{
      x: {{ title: {{ display: true, text: "elapsed (s)", color: "#94a3b8" }},
             grid: {{ color: "#f1f5f9" }}, ticks: {{ maxTicksLimit: 10 }} }},
      y: {{ title: {{ display: true, text: "ms", color: "#94a3b8" }},
             grid: {{ color: "#f1f5f9" }}, beginAtZero: true }},
    }},
  }},
}});
</script>
</body>
</html>"""


def _fmt_dur(secs: int) -> str:
    m, s = divmod(secs, 60)
    h, m = divmod(m, 60)
    if h:
        return f"{h}h {m}m {s}s"
    if m:
        return f"{m}m {s}s" if s else f"{m}m"
    return f"{s}s"
