"""LoadPilot CLI — loadpilot run <scenario.py> and loadpilot agents start."""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console
from rich.live import Live
from rich.panel import Panel

from loadpilot.dsl import _scenarios
from loadpilot.models import AgentMetrics, ScenarioPlan, TaskPlan, parse_duration
from loadpilot import report as _report

app = typer.Typer(
    name="loadpilot",
    help="LoadPilot — Python DSL + Rust engine load testing tool.",
    add_completion=False,
)

agents_app = typer.Typer(help="Manage LoadPilot agents.")
app.add_typer(agents_app, name="agents")

console = Console()

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

COORDINATOR_BINARY_CANDIDATES = [
    Path(__file__).parent.parent.parent / "engine" / "target" / "release" / "coordinator",
    Path(__file__).parent.parent.parent / "engine" / "target" / "debug" / "coordinator",
    shutil.which("loadpilot-coordinator") or "",
]


def _find_coordinator_binary() -> Path:
    for candidate in COORDINATOR_BINARY_CANDIDATES:
        p = Path(str(candidate))
        if p.exists() and p.is_file():
            return p
    raise FileNotFoundError(
        "Could not find the loadpilot-coordinator binary.\n"
        "Build it with:  cargo build --release  inside engine/\n"
        "Or add it to your PATH as 'loadpilot-coordinator'."
    )


def _load_scenario_file(scenario_file: Path) -> None:
    """Import the scenario file so its @scenario decorators populate _scenarios."""
    scenario_dir = str(scenario_file.parent.resolve())
    if scenario_dir not in sys.path:
        sys.path.insert(0, scenario_dir)

    spec = importlib.util.spec_from_file_location("_loadpilot_scenario", scenario_file)
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load scenario file: {scenario_file}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["_loadpilot_scenario"] = module
    spec.loader.exec_module(module)  # type: ignore[union-attr]


def _build_plan(scenario_file: Path, target: str) -> ScenarioPlan:
    if not _scenarios:
        raise ValueError("No @scenario classes found in the scenario file.")

    s = _scenarios[0]

    # n_vusers: enough VUsers to sustain peak RPS, staggered over ramp-up to
    # avoid triggering server-side rate limits on on_start (e.g. login endpoints).
    n_vusers = max(3, s.rps // 2)

    # Pre-run each task with a MockClient to extract the real URL and method.
    # This works for pure static scenarios (no on_start state needed).
    # For scenarios that reference self.* set in on_start the mock run may fail;
    # those errors are silently ignored and the task falls back to url="/".
    from loadpilot._bridge import MockClient as _MockClient
    try:
        tmp = s.cls()
    except Exception:
        tmp = None

    tasks: list[TaskPlan] = []
    for td in s.tasks:
        url, method, headers, body = "/", "GET", {}, None
        if tmp is not None:
            mock = _MockClient()
            try:
                td.func(tmp, mock)
                m, p, h, b = mock.get_call()
                if p:
                    url, method, headers, body = p, (m or "GET"), h, b
            except Exception:
                pass  # fall back to defaults for state-dependent tasks
        tasks.append(TaskPlan(name=td.name, weight=td.weight,
                               url=url, method=method,
                               headers=headers, body_template=body))

    # Enable the PyO3 bridge when Python needs to run at request time:
    # - lifecycle hooks (on_start / on_stop carry per-VUser state)
    # - check_{task} methods need the real HTTP response
    has_check = any(f"check_{td.name}" in s.cls.__dict__ for td in s.tasks)
    has_callbacks = (
        "on_start" in s.cls.__dict__
        or "on_stop" in s.cls.__dict__
        or has_check
    )

    return ScenarioPlan(
        name=s.name,
        rps=s.rps,
        duration_secs=parse_duration(s.duration),
        ramp_up_secs=parse_duration(s.ramp_up),
        mode="ramp",
        target_url=target,
        tasks=tasks,
        scenario_file=str(scenario_file.resolve()) if has_callbacks else None,
        scenario_class=s.name if has_callbacks else None,
        n_vusers=n_vusers if has_callbacks else None,
    )


def _render_dashboard(metrics: AgentMetrics, scenario_name: str) -> Panel:
    duration_str = _fmt_duration(int(metrics.elapsed_secs))

    error_pct = (
        (metrics.errors_total / metrics.requests_total * 100)
        if metrics.requests_total > 0
        else 0.0
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


def _fmt_duration(secs: int) -> str:
    m, s = divmod(secs, 60)
    h, m = divmod(m, 60)
    if h:
        return f"{h:02d}:{m:02d}:{s:02d}"
    return f"{m:02d}:{s:02d}"


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


@app.command("run")
def run_command(
    scenario_file: Path = typer.Argument(..., help="Path to the Python scenario file."),
    target: str = typer.Option(
        "http://localhost:8000", "--target", "-t", help="Base URL of the system under test."
    ),
    agents: int = typer.Option(1, "--agents", "-a", help="Number of agent processes (MVP: 1)."),
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Print the test plan JSON and exit without running."
    ),
    report: Optional[Path] = typer.Option(
        None, "--report", "-r",
        help="Write an HTML report to this path after the test (e.g. --report report.html).",
    ),
):
    """Run a load test scenario against TARGET."""
    if not scenario_file.exists():
        console.print(f"[red]Error:[/] Scenario file not found: {scenario_file}")
        raise typer.Exit(1)

    try:
        _load_scenario_file(scenario_file.resolve())
    except Exception as exc:
        console.print(f"[red]Error loading scenario:[/] {exc}")
        raise typer.Exit(1)

    try:
        plan = _build_plan(scenario_file, target)
    except ValueError as exc:
        console.print(f"[red]Error:[/] {exc}")
        raise typer.Exit(1)

    plan_json = plan.model_dump_json(indent=2)

    if dry_run:
        console.print(plan_json)
        raise typer.Exit(0)

    try:
        coordinator = _find_coordinator_binary()
    except FileNotFoundError as exc:
        console.print(f"[red]{exc}[/]")
        raise typer.Exit(1)

    bridge_mode = "[cyan]PyO3[/]" if plan.scenario_file else "[dim]static[/]"
    console.print(
        f"[bold green]Starting LoadPilot[/] — scenario: [cyan]{plan.name}[/] "
        f"| target: [cyan]{target}[/] "
        f"| {plan.rps} RPS for {plan.duration_secs}s (ramp {plan.ramp_up_secs}s) "
        f"| mode: {bridge_mode}"
    )

    proc = subprocess.Popen(
        [str(coordinator)],
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

    scenario_name = plan.name
    last_metrics: AgentMetrics | None = None
    all_snapshots: list[AgentMetrics] = []

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

    if last_metrics:
        console.print("\n[bold green]Test complete![/]")
        console.print(f"  Total requests : {last_metrics.requests_total:,}")
        console.print(f"  Errors         : {last_metrics.errors_total:,}")
        console.print(f"  p50 latency    : {last_metrics.latency.p50_ms:.0f}ms")
        console.print(f"  p99 latency    : {last_metrics.latency.p99_ms:.0f}ms")
    else:
        console.print("[yellow]No metrics received from coordinator.[/]")

    if report is not None and all_snapshots:
        _report.generate(
            snapshots=all_snapshots,
            scenario_name=scenario_name,
            target_url=target,
            rps_target=plan.rps,
            duration_secs=plan.duration_secs,
            ramp_up_secs=plan.ramp_up_secs,
            output_path=report,
        )
        console.print(f"  Report         : [cyan]{report}[/]")

    if proc.returncode != 0:
        stderr_output = proc.stderr.read() if proc.stderr else ""
        if stderr_output:
            console.print(f"[red]Coordinator stderr:[/]\n{stderr_output}")
        raise typer.Exit(proc.returncode)


@agents_app.command("start")
def agents_start_command(
    coordinator: str = typer.Option(
        "localhost:7000", "--coordinator", "-c", help="Coordinator host:port."
    ),
):
    """Start an agent process and connect it to a coordinator."""
    console.print(f"[yellow]Agent start[/] — connecting to coordinator at [cyan]{coordinator}[/]")
    console.print(
        "[dim]Note: In MVP mode, the coordinator and agent run as a single process. "
        "Separate agent mode is planned for v2.[/]"
    )


@app.command("version")
def version_command():
    """Print LoadPilot version."""
    from importlib.metadata import version as pkg_version

    try:
        v = pkg_version("loadpilot")
    except Exception:
        v = "0.1.0-dev"
    console.print(f"LoadPilot [bold]{v}[/]")


if __name__ == "__main__":
    app()
