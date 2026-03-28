"""Coordinator subprocess and remote HTTP streaming logic."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from rich.console import Console
from rich.live import Live

from loadpilot._display import _render_dashboard
from loadpilot.models import AgentMetrics

console = Console()


def _run_remote(
    plan_json: str,
    coordinator_url: str,
    scenario_name: str,
) -> tuple[AgentMetrics | None, list[AgentMetrics]]:
    """Stream metrics from a remote coordinator via HTTP. Returns (last, all_snapshots)."""
    import httpx
    import typer

    last_metrics: AgentMetrics | None = None
    all_snapshots: list[AgentMetrics] = []

    with httpx.stream(
        "POST",
        f"{coordinator_url.rstrip('/')}/run",
        content=plan_json,
        timeout=None,
    ) as r:
        if r.status_code == 409:
            console.print("[red]Coordinator is busy — another test is running.[/]")
            raise typer.Exit(1)
        if r.status_code != 200:
            console.print(f"[red]Coordinator error: {r.status_code}[/]")
            raise typer.Exit(1)

        with Live(console=console, refresh_per_second=4) as live:
            for line in r.iter_lines():
                line = line.strip()
                if not line:
                    continue
                try:
                    data = json.loads(line)
                    metrics = AgentMetrics(**data)
                    last_metrics = metrics
                    all_snapshots.append(metrics)
                    live.update(_render_dashboard(metrics, scenario_name))
                    if metrics.phase == "done":
                        break
                except (json.JSONDecodeError, Exception):
                    console.print(f"[dim]{line}[/]")

    return last_metrics, all_snapshots


def _run_local(
    plan_json: str,
    coordinator_cmd: list[str],
    scenario_name: str,
) -> tuple[AgentMetrics | None, list[AgentMetrics], subprocess.Popen]:
    """Run a local coordinator subprocess and stream its stdout metrics.
    Returns (last, all_snapshots, proc)."""
    last_metrics: AgentMetrics | None = None
    all_snapshots: list[AgentMetrics] = []

    proc = subprocess.Popen(
        coordinator_cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    assert proc.stdin is not None
    proc.stdin.write(plan_json + "\n")
    proc.stdin.flush()
    proc.stdin.close()

    assert proc.stdout is not None

    with Live(console=console, refresh_per_second=4) as live:
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                data = json.loads(line)
                metrics = AgentMetrics(**data)
                last_metrics = metrics
                all_snapshots.append(metrics)
                live.update(_render_dashboard(metrics, scenario_name))
                if metrics.phase == "done":
                    break
            except (json.JSONDecodeError, Exception):
                console.print(f"[dim]{line}[/]")

    proc.wait()
    return last_metrics, all_snapshots, proc


def _build_coordinator_cmd(
    coordinator_bin: Path,
    nats_url: str | None,
    nats_token: str | None,
    external_agents: int,
    agents: int,
) -> list[str]:
    """Assemble the coordinator command-line arguments."""
    cmd = [str(coordinator_bin)]
    if nats_url:
        cmd += ["--nats-url", nats_url, "--external-agents", str(external_agents or 1)]
        if nats_token:
            cmd += ["--nats-token", nats_token]
    elif external_agents > 0:
        cmd += ["--external-agents", str(external_agents)]
    elif agents > 1:
        cmd += ["--local-agents", str(agents)]
    return cmd
