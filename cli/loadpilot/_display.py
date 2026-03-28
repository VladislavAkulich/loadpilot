"""Rich dashboard rendering, threshold checking, and display utilities."""

from __future__ import annotations

import os
from pathlib import Path

import typer
from rich.console import Console
from rich.panel import Panel

from loadpilot.models import AgentMetrics

console = Console()

_THRESHOLD_LABELS: dict[str, str] = {
    "p50_ms": "p50 latency",
    "p95_ms": "p95 latency",
    "p99_ms": "p99 latency",
    "max_ms": "max latency",
    "error_rate": "error rate",
}


def _fmt_duration(secs: int) -> str:
    m, s = divmod(secs, 60)
    h, m = divmod(m, 60)
    if h:
        return f"{h:02d}:{m:02d}:{s:02d}"
    return f"{m:02d}:{s:02d}"


def _resolve_target(cli_target: str | None) -> str | None:
    """Resolve target URL with priority: CLI flag > LOADPILOT_TARGET env > .env file."""
    if cli_target is not None:
        return cli_target

    env_target = os.environ.get("LOADPILOT_TARGET")
    if env_target:
        return env_target

    dot_env = Path(".env")
    if dot_env.exists():
        for line in dot_env.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line.startswith("LOADPILOT_TARGET="):
                value = line.split("=", 1)[1].strip().strip('"').strip("'")
                if value:
                    return value

    return None


def _render_dashboard(metrics: AgentMetrics, scenario_name: str) -> Panel:
    duration_str = _fmt_duration(int(metrics.elapsed_secs))

    error_pct = (
        (metrics.errors_total / metrics.requests_total * 100) if metrics.requests_total > 0 else 0.0
    )

    progress_pct = min(
        int(metrics.current_rps / metrics.target_rps * 100) if metrics.target_rps > 0 else 0,
        100,
    )
    bar_filled = int(progress_pct / 5)
    bar_empty = 20 - bar_filled
    bar = "█" * bar_filled + "░" * bar_empty

    phase_label = {
        "ramp_up": "ramp-up",
        "steady": "steady",
        "ramp_down": "ramp-down",
        "done": "done",
    }.get(metrics.phase, metrics.phase)

    lines = [
        f"[bold cyan]LoadPilot[/] — [bold]{scenario_name}[/]  "
        f"[[green]{duration_str}[/]]  "
        f"[dim]{phase_label}: {metrics.current_rps:.0f}/{metrics.target_rps:.0f} RPS[/]",
        "",
        f"  [bold]Requests/sec:[/]  {metrics.current_rps:>8.1f}      [bold]Total:[/]  {metrics.requests_total:>8,}",
        f"  [bold]Errors:[/]        {error_pct:>7.1f}%     [bold]Failed:[/]  {metrics.errors_total:>8,}",
        "",
        "  [bold]Latency:[/]",
        f"    p50:  {metrics.latency.p50_ms:>6.0f}ms",
        f"    p95:  {metrics.latency.p95_ms:>6.0f}ms",
        f"    p99:  {metrics.latency.p99_ms:>6.0f}ms",
        f"    max:  {metrics.latency.max_ms:>6.0f}ms",
        "",
        f"  [[green]{bar}[/]] [bold]{progress_pct}%[/]",
    ]

    return Panel("\n".join(lines), title="[bold magenta]LoadPilot[/]", border_style="magenta")


def _check_thresholds(metrics: AgentMetrics, thresholds: dict[str, float]) -> bool:
    """Evaluate SLA thresholds against final metrics. Prints a result table.
    Returns True if any threshold is breached."""
    error_rate = (
        metrics.errors_total / metrics.requests_total * 100 if metrics.requests_total > 0 else 0.0
    )
    actual: dict[str, float] = {
        "p50_ms": metrics.latency.p50_ms,
        "p95_ms": metrics.latency.p95_ms,
        "p99_ms": metrics.latency.p99_ms,
        "max_ms": metrics.latency.max_ms,
        "error_rate": error_rate,
    }

    breached: list[tuple[str, float, float]] = []
    passed: list[tuple[str, float, float]] = []

    for key, limit in thresholds.items():
        value = actual.get(key)
        if value is None:
            console.print(f"[yellow]Unknown threshold key:[/] {key!r} — skipped")
            continue
        if value > limit:
            breached.append((key, value, limit))
        else:
            passed.append((key, value, limit))

    console.print("\n[bold]Thresholds[/]")
    unit = {"error_rate": "%"}.get
    for key, value, limit in passed:
        u = unit(key) or "ms"
        label = _THRESHOLD_LABELS.get(key, key)
        console.print(f"  [green]✓[/]  {label:<16} {value:>8.1f}{u}  <  {limit:.1f}{u}")
    for key, value, limit in breached:
        u = unit(key) or "ms"
        label = _THRESHOLD_LABELS.get(key, key)
        console.print(
            f"  [red]✗[/]  {label:<16} {value:>8.1f}{u}  >  {limit:.1f}{u}  [red][BREACHED][/]"
        )

    if breached:
        console.print(
            f"\n[bold red]SLA breach — {len(breached)} threshold(s) exceeded.[/] "
            "Exiting with code 1."
        )
        return True

    console.print("\n[bold green]All thresholds passed.[/]")
    return False
