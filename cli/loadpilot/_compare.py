"""Baseline comparison logic for loadpilot compare command."""

from __future__ import annotations

import json
from pathlib import Path

import typer
from rich.console import Console

console = Console()

_METRICS = [
    ("RPS actual", "rps_actual", "higher", "{:.1f}", ""),
    ("p50 latency", "p50_ms", "lower", "{:.0f}", "ms"),
    ("p95 latency", "p95_ms", "lower", "{:.0f}", "ms"),
    ("p99 latency", "p99_ms", "lower", "{:.0f}", "ms"),
    ("max latency", "max_ms", "lower", "{:.0f}", "ms"),
    ("error rate", "error_rate_pct", "lower", "{:.2f}", "%"),
]


def run_compare(current: Path, baseline: Path, regression_threshold: float) -> None:
    for path in (baseline, current):
        if not path.exists():
            console.print(f"[red]Error:[/] File not found: {path}")
            raise typer.Exit(1)

    try:
        b = json.loads(baseline.read_text(encoding="utf-8"))
        c = json.loads(current.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        console.print(f"[red]Error parsing JSON:[/] {exc}")
        raise typer.Exit(1)

    console.print(f"\nComparing [cyan]{baseline.name}[/] → [cyan]{current.name}[/]\n")

    if b.get("scenario") or c.get("scenario"):
        b_scenario = b.get("scenario", "—")
        c_scenario = c.get("scenario", "—")
        if b_scenario != c_scenario:
            console.print(
                f"[yellow]Warning:[/] different scenarios ({b_scenario} vs {c_scenario})\n"
            )

    header = f"  {'':18}  {'baseline':>12}  {'current':>12}  {'diff':>10}"
    console.print(header)
    console.print("  " + "─" * 58)

    regressions: list[str] = []

    for label, key, better, fmt, unit in _METRICS:
        b_val = b.get(key)
        c_val = c.get(key)
        if b_val is None or c_val is None:
            continue

        b_str = fmt.format(b_val) + unit
        c_str = fmt.format(c_val) + unit

        diff_pct = 0.0 if b_val == 0 else (c_val - b_val) / abs(b_val) * 100

        if diff_pct == 0:
            diff_str, color = "—", "dim"
        elif (better == "lower" and diff_pct < 0) or (better == "higher" and diff_pct > 0):
            diff_str, color = f"{diff_pct:+.1f}%", "green"
        else:
            diff_str, color = f"{diff_pct:+.1f}%", "red"
            if abs(diff_pct) >= regression_threshold:
                regressions.append(f"{label} regressed {diff_pct:+.1f}%")

        console.print(f"  {label:<18}  {b_str:>12}  {c_str:>12}  [{color}]{diff_str:>10}[/{color}]")

    console.print()

    if regressions:
        for r in regressions:
            console.print(f"  [red]✗[/]  {r}")
        console.print(
            f"\n[bold red]Regression detected (threshold: {regression_threshold:.0f}%).[/] "
            "Exiting with code 1."
        )
        raise typer.Exit(1)

    console.print("[bold green]No regressions detected.[/]")
