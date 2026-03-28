"""Post-run result serialization: results JSON and baseline saving."""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING

from rich.console import Console

from loadpilot.models import AgentMetrics, ScenarioPlan

if TYPE_CHECKING:
    from loadpilot._knee import KneePoint

console = Console()


def _metrics_dict(
    metrics: AgentMetrics,
    scenario_name: str,
    target: str,
    plan: ScenarioPlan,
    knee: "KneePoint | None" = None,
) -> dict:
    error_rate = (
        metrics.errors_total / metrics.requests_total * 100 if metrics.requests_total > 0 else 0.0
    )
    rps_actual = metrics.requests_total / metrics.elapsed_secs if metrics.elapsed_secs > 0 else 0.0
    data: dict = {
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
    if knee is not None:
        data["knee_point"] = {
            "step": knee.step,
            "total_steps": knee.total_steps,
            "rps": knee.rps,
            "max_rps": knee.max_rps,
            "p99_ms": knee.p99_ms,
            "error_rate_pct": round(knee.error_rate_pct, 3),
        }
    return data


def save_results_json(
    path: Path,
    metrics: AgentMetrics,
    scenario_name: str,
    target: str,
    plan: ScenarioPlan,
    knee: "KneePoint | None" = None,
) -> None:
    data = _metrics_dict(metrics, scenario_name, target, plan, knee)
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
