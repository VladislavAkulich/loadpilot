"""Post-run result serialization: results JSON and baseline saving."""

from __future__ import annotations

import json
from pathlib import Path

from rich.console import Console

from loadpilot.models import AgentMetrics, ScenarioPlan

console = Console()


def _metrics_dict(
    metrics: AgentMetrics,
    scenario_name: str,
    target: str,
    plan: ScenarioPlan,
) -> dict:
    error_rate = (
        metrics.errors_total / metrics.requests_total * 100
        if metrics.requests_total > 0
        else 0.0
    )
    rps_actual = (
        metrics.requests_total / metrics.elapsed_secs
        if metrics.elapsed_secs > 0
        else 0.0
    )
    return {
        "scenario": scenario_name,
        "target_url": target,
        "rps_target": plan.rps,
        "rps_actual": round(rps_actual, 2),
        "requests_total": metrics.requests_total,
        "errors_total": metrics.errors_total,
        "error_rate_pct": round(error_rate, 3),
        "p50_ms": metrics.latency.p50_ms,
        "p95_ms": metrics.latency.p95_ms,
        "p99_ms": metrics.latency.p99_ms,
        "max_ms": metrics.latency.max_ms,
        "duration_secs": metrics.elapsed_secs,
    }


def save_results_json(
    path: Path,
    metrics: AgentMetrics,
    scenario_name: str,
    target: str,
    plan: ScenarioPlan,
) -> None:
    data = _metrics_dict(metrics, scenario_name, target, plan)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2), encoding="utf-8")
    console.print(f"  Results JSON   : [cyan]{path}[/]")


def save_baseline(
    metrics: AgentMetrics,
    scenario_name: str,
    target: str,
    plan: ScenarioPlan,
) -> None:
    baseline_path = Path(".loadpilot") / "baseline.json"
    baseline_path.parent.mkdir(exist_ok=True)
    data = _metrics_dict(metrics, scenario_name, target, plan)
    baseline_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
    console.print(f"  Baseline saved : [cyan]{baseline_path}[/]")
