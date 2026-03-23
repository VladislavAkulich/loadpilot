"""LoadPilot CLI — loadpilot run <scenario.py> and loadpilot agents start."""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console
from rich.live import Live
from rich.panel import Panel

from loadpilot import report as _report
from loadpilot.dsl import _clear_scenarios, _scenarios
from loadpilot.models import AgentMetrics, ScenarioPlan, TaskPlan, VUserConfig, parse_duration

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

_EXE = ".exe" if sys.platform == "win32" else ""

COORDINATOR_BINARY_CANDIDATES = [
    # 1. Bundled inside the installed wheel (pip install loadpilot).
    Path(__file__).parent / f"coordinator{_EXE}",
    # 2. Local dev build (cargo build --release inside engine/).
    Path(__file__).parent.parent.parent / "engine" / "target" / "release" / f"coordinator{_EXE}",
    # 3. Local dev build debug.
    Path(__file__).parent.parent.parent / "engine" / "target" / "debug" / f"coordinator{_EXE}",
    # 4. System PATH.
    Path(shutil.which("loadpilot-coordinator") or ""),
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
    _clear_scenarios()
    scenario_dir = str(scenario_file.parent.resolve())
    if scenario_dir not in sys.path:
        sys.path.insert(0, scenario_dir)

    spec = importlib.util.spec_from_file_location("_loadpilot_scenario", scenario_file)
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load scenario file: {scenario_file}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["_loadpilot_scenario"] = module
    spec.loader.exec_module(module)  # type: ignore[union-attr]


def _build_plan(
    scenario_file: Path,
    target: str,
    scenario_name: str | None = None,
    distributed: bool = False,
) -> ScenarioPlan:
    if not _scenarios:
        raise ValueError("No @scenario classes found in the scenario file.")

    if scenario_name:
        matches = [s for s in _scenarios if s.name == scenario_name]
        if not matches:
            available = ", ".join(s.name for s in _scenarios)
            raise ValueError(f"Scenario {scenario_name!r} not found. Available: {available}")
        s = matches[0]
    else:
        s = _scenarios[0]

    # n_vusers: enough VUsers to sustain peak RPS, staggered over ramp-up to
    # avoid triggering server-side rate limits on on_start (e.g. login endpoints).
    # Cap at 100: in PyO3 mode each VUser runs RustClient HTTP (~1ms/task),
    # so 100 VUsers can easily sustain 50 000 RPS — no need for more threads.
    n_vusers = min(max(5, s.rps // 100), 100)

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
                import asyncio as _asyncio
                import inspect as _inspect

                result = td.func(tmp, mock)
                if _inspect.iscoroutine(result):
                    _asyncio.run(result)
                m, p, h, b = mock.get_call()
                if p:
                    url, method, headers, body = p, (m or "GET"), h, b
            except Exception:
                pass  # fall back to defaults for state-dependent tasks
        tasks.append(
            TaskPlan(
                name=td.name,
                weight=td.weight,
                url=url,
                method=method,
                headers=headers,
                body_template=body,
            )
        )

    # Enable the PyO3 bridge when Python needs to run at request time:
    # - lifecycle hooks (on_start / on_stop carry per-VUser state)
    # - check_{task} methods need the real HTTP response
    has_check = any(f"check_{td.name}" in s.cls.__dict__ for td in s.tasks)
    has_callbacks = "on_start" in s.cls.__dict__ or "on_stop" in s.cls.__dict__ or has_check

    # ── Distributed pre-auth pool ──────────────────────────────────────────────
    # In distributed mode PyO3 callbacks can't run on remote agents.
    # If the scenario has on_start (e.g. login → per-VUser auth token), we run
    # on_start N times here on the coordinator side using a real HTTP client,
    # then probe each @task with MockClient to capture what headers on_start set.
    # The resulting vuser_configs list is shipped with the plan so agents can
    # rotate through pre-authenticated header sets in pure Rust.
    vuser_configs: list[VUserConfig] = []
    if distributed and "on_start" in s.cls.__dict__:
        from loadpilot.client import LoadClient as _LoadClient

        pool_size = min(n_vusers, 20)
        console.print(f"[dim]Distributed pre-auth: running on_start for {pool_size} VUsers…[/]")
        failed_first = False
        for i in range(pool_size):
            try:
                instance = s.cls()
                with _LoadClient(target) as real_client:
                    instance.on_start(real_client)
                task_headers: dict[str, dict[str, str]] = {}
                for td in s.tasks:
                    mock = _MockClient()
                    try:
                        td.func(instance, mock)
                        _, _, h, _ = mock.get_call()
                        task_headers[td.name] = h
                    except Exception:
                        task_headers[td.name] = {}
                vuser_configs.append(VUserConfig(task_headers=task_headers))
            except Exception as exc:
                if i == 0:
                    console.print(
                        f"[yellow]on_start pre-auth failed: {exc}. Falling back to static mode.[/]"
                    )
                    failed_first = True
                    break
        if failed_first:
            vuser_configs = []

    # In distributed mode with pre-auth pool, disable PyO3 bridge for agents —
    # agents will rotate through vuser_configs headers in pure Rust.
    # check_* / on_stop are skipped in distributed (status-based errors only).
    use_pyo3 = has_callbacks and not distributed

    return ScenarioPlan(
        name=s.name,
        rps=s.rps,
        duration_secs=parse_duration(s.duration),
        ramp_up_secs=parse_duration(s.ramp_up),
        mode=s.mode,
        steps=s.steps,
        target_url=target,
        tasks=tasks,
        scenario_file=str(scenario_file.resolve()) if use_pyo3 else None,
        scenario_class=s.name if use_pyo3 else None,
        n_vusers=n_vusers if use_pyo3 else None,
        vuser_configs=vuser_configs,
        thresholds=s.thresholds,
    )


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


_THRESHOLD_LABELS: dict[str, str] = {
    "p50_ms": "p50 latency",
    "p95_ms": "p95 latency",
    "p99_ms": "p99 latency",
    "max_ms": "max latency",
    "error_rate": "error rate",
}


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


def _resolve_target(cli_target: str | None) -> str | None:
    """Resolve target URL with priority: CLI flag > LOADPILOT_TARGET env > .env file."""
    import os

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


def _fmt_duration(secs: int) -> str:
    m, s = divmod(secs, 60)
    h, m = divmod(m, 60)
    if h:
        return f"{h:02d}:{m:02d}:{s:02d}"
    return f"{m:02d}:{s:02d}"


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def _tui_pick_scenario(search_dir: Path) -> tuple[Path, str] | None:
    """Two-step TUI: pick file → pick scenario. Returns (file, scenario_name) or None."""
    import questionary

    # Collect all .py files that contain at least one @scenario
    candidates: list[tuple[Path, list]] = []
    for py_file in sorted(search_dir.rglob("*.py")):
        if py_file.name.startswith("_"):
            continue
        try:
            _load_scenario_file(py_file)
            if _scenarios:
                candidates.append((py_file, list(_scenarios)))
        except Exception:
            pass
        finally:
            _scenarios.clear()

    if not candidates:
        console.print("[yellow]No scenario files found.[/]")
        return None

    _BACK = "__back__"

    while True:
        # Step 1: pick file
        file_choices = [
            questionary.Choice(
                title=f"{f.name:<35} {len(sc)} scenario{'s' if len(sc) != 1 else ''}",
                value=i,
            )
            for i, (f, sc) in enumerate(candidates)
        ]
        file_idx = questionary.select("Select scenario file:", choices=file_choices).ask()
        if file_idx is None:
            return None

        chosen_file, chosen_scenarios = candidates[file_idx]

        # Step 2: pick scenario within that file
        if len(chosen_scenarios) == 1:
            return chosen_file, chosen_scenarios[0].name

        scenario_choices = [
            questionary.Choice(title="← Back", value=_BACK),
            *[
                questionary.Choice(
                    title=f"{s.name:<30} {s.rps} RPS · {s.duration} · {s.mode}",
                    value=s.name,
                )
                for s in chosen_scenarios
            ],
        ]
        chosen_name = questionary.select("Select scenario:", choices=scenario_choices).ask()
        if chosen_name is None:
            return None
        if chosen_name == _BACK:
            continue  # back to file picker

        return chosen_file, chosen_name


@app.command("run")
def run_command(
    scenario_file: Optional[Path] = typer.Argument(
        None,
        help="Path to the Python scenario file. Omit to browse interactively.",
    ),
    target: Optional[str] = typer.Option(
        None, "--target", "-t", help="Base URL of the system under test."
    ),
    scenario_name: Optional[str] = typer.Option(
        None,
        "--scenario",
        "-s",
        help="Scenario class name to run. Required when the file defines multiple @scenario classes.",
    ),
    agents: int = typer.Option(
        1,
        "--agents",
        "-a",
        help="Number of agent processes. 1 = single-process mode; >1 = distributed mode with embedded NATS broker.",
    ),
    external_agents: int = typer.Option(
        0,
        "--external-agents",
        "-e",
        help="Wait for N externally started agents instead of spawning them. Use with --nats-url for remote agents.",
    ),
    nats_url: Optional[str] = typer.Option(
        None,
        "--nats-url",
        help="External NATS URL for remote agents (e.g. nats://host:4222). Use with --external-agents.",
    ),
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Print the test plan JSON and exit without running."
    ),
    report: Optional[Path] = typer.Option(
        None,
        "--report",
        "-r",
        help="Write an HTML report to this path after the test (e.g. --report report.html).",
    ),
    threshold: Optional[list[str]] = typer.Option(
        None,
        "--threshold",
        help=(
            "SLA threshold in KEY=VALUE format. Overrides thresholds from @scenario. "
            "Supported keys: p50_ms, p95_ms, p99_ms, max_ms, error_rate. "
            "Example: --threshold p99_ms=500 --threshold error_rate=1"
        ),
    ),
    results_json: Optional[Path] = typer.Option(
        None,
        "--results-json",
        help="Write final metrics as JSON to this path (e.g. --results-json results.json).",
    ),
    save_baseline: bool = typer.Option(
        False,
        "--save-baseline",
        help="Save results as the baseline for future comparisons (.loadpilot/baseline.json).",
    ),
):
    """Run a load test scenario against TARGET. Omit the file to browse interactively."""
    # ── Resolve target: CLI > LOADPILOT_TARGET env > .env > prompt/default ────
    target = _resolve_target(target)

    # ── No file given: TUI browser or error ───────────────────────────────────
    if scenario_file is None:
        if not sys.stdin.isatty():
            console.print(
                "[red]Error:[/] Specify a scenario file or run interactively in a terminal."
            )
            raise typer.Exit(1)
        search_dir = Path("scenarios") if Path("scenarios").is_dir() else Path(".")
        result = _tui_pick_scenario(search_dir)
        if result is None:
            raise typer.Exit(0)
        scenario_file, scenario_name = result

        # Prompt for target if not resolved from CLI / env / .env.
        if target is None:
            import questionary

            entered = questionary.text(
                "Target URL:",
                default="http://localhost:8000",
            ).ask()
            if not entered:
                raise typer.Exit(0)
            target = entered.strip()

    if target is None:
        target = "http://localhost:8000"

    if not scenario_file.exists():
        console.print(f"[red]Error:[/] Scenario file not found: {scenario_file}")
        raise typer.Exit(1)

    try:
        _load_scenario_file(scenario_file.resolve())
    except Exception as exc:
        console.print(f"[red]Error loading scenario:[/] {exc}")
        raise typer.Exit(1)

    if len(_scenarios) > 1 and not scenario_name:
        if sys.stdin.isatty():
            import questionary

            choices = [
                questionary.Choice(
                    title=f"{s.name}  ({s.rps} RPS · {s.duration} · {s.mode})",
                    value=s.name,
                )
                for s in _scenarios
            ]
            selected = questionary.select(
                "Select a scenario to run:",
                choices=choices,
            ).ask()
            if selected is None:
                raise typer.Exit(0)
            scenario_name = selected
        else:
            console.print("[bold]Scenarios in this file:[/]")
            for s in _scenarios:
                console.print(f"  [cyan]{s.name}[/]  {s.rps} RPS · {s.duration} · {s.mode}")
            console.print("\nRun with [bold]--scenario <name>[/] to select one.")
            raise typer.Exit(0)

    is_distributed = agents > 1 or external_agents > 0 or nats_url is not None
    try:
        plan = _build_plan(scenario_file, target, scenario_name, distributed=is_distributed)
    except ValueError as exc:
        console.print(f"[red]Error:[/] {exc}")
        raise typer.Exit(1)

    # Apply --threshold CLI overrides on top of @scenario thresholds.
    if threshold:
        for t in threshold:
            if "=" not in t:
                console.print(f"[red]Invalid threshold format:[/] {t!r} (expected KEY=VALUE)")
                raise typer.Exit(1)
            k, _, v = t.partition("=")
            try:
                plan.thresholds[k.strip()] = float(v.strip())
            except ValueError:
                console.print(f"[red]Threshold value must be a number:[/] {t!r}")
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

    coordinator_cmd = [str(coordinator)]
    if nats_url:
        coordinator_cmd += ["--nats-url", nats_url, "--external-agents", str(external_agents or 1)]
    elif external_agents > 0:
        coordinator_cmd += ["--external-agents", str(external_agents)]
    elif agents > 1:
        coordinator_cmd += ["--local-agents", str(agents)]

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
            thresholds=plan.thresholds,
            n_agents=external_agents if external_agents > 0 else agents,
        )
        console.print(f"  Report         : [cyan]{report}[/]")

    if results_json is not None and last_metrics:
        error_rate = (
            last_metrics.errors_total / last_metrics.requests_total * 100
            if last_metrics.requests_total > 0
            else 0.0
        )
        rps_actual = (
            last_metrics.requests_total / last_metrics.elapsed_secs
            if last_metrics.elapsed_secs > 0
            else 0.0
        )
        summary = {
            "scenario": scenario_name,
            "target_url": target,
            "rps_target": plan.rps,
            "rps_actual": round(rps_actual, 2),
            "requests_total": last_metrics.requests_total,
            "errors_total": last_metrics.errors_total,
            "error_rate_pct": round(error_rate, 3),
            "p50_ms": last_metrics.latency.p50_ms,
            "p95_ms": last_metrics.latency.p95_ms,
            "p99_ms": last_metrics.latency.p99_ms,
            "max_ms": last_metrics.latency.max_ms,
            "duration_secs": last_metrics.elapsed_secs,
        }
        results_json.parent.mkdir(parents=True, exist_ok=True)
        results_json.write_text(json.dumps(summary, indent=2), encoding="utf-8")
        console.print(f"  Results JSON   : [cyan]{results_json}[/]")

    if save_baseline and last_metrics:
        baseline_path = Path(".loadpilot") / "baseline.json"
        baseline_path.parent.mkdir(exist_ok=True)
        error_rate = (
            last_metrics.errors_total / last_metrics.requests_total * 100
            if last_metrics.requests_total > 0
            else 0.0
        )
        rps_actual = (
            last_metrics.requests_total / last_metrics.elapsed_secs
            if last_metrics.elapsed_secs > 0
            else 0.0
        )
        baseline_data = {
            "scenario": plan.name,
            "target_url": target,
            "rps_target": plan.rps,
            "rps_actual": round(rps_actual, 2),
            "requests_total": last_metrics.requests_total,
            "errors_total": last_metrics.errors_total,
            "error_rate_pct": round(error_rate, 3),
            "p50_ms": last_metrics.latency.p50_ms,
            "p95_ms": last_metrics.latency.p95_ms,
            "p99_ms": last_metrics.latency.p99_ms,
            "max_ms": last_metrics.latency.max_ms,
            "duration_secs": last_metrics.elapsed_secs,
        }
        baseline_path.write_text(json.dumps(baseline_data, indent=2), encoding="utf-8")
        console.print(f"  Baseline saved : [cyan]{baseline_path}[/]")

    threshold_failed = False
    if last_metrics and plan.thresholds:
        threshold_failed = _check_thresholds(last_metrics, plan.thresholds)

    if proc.returncode != 0:
        stderr_output = proc.stderr.read() if proc.stderr else ""
        if stderr_output:
            console.print(f"[red]Coordinator stderr:[/]\n{stderr_output}")
        raise typer.Exit(proc.returncode)

    if threshold_failed:
        raise typer.Exit(1)


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


@app.command("init")
def init_command(
    directory: Path = typer.Argument(
        Path("."),
        help="Directory to initialise. Created if it does not exist.",
    ),
):
    """Scaffold a new LoadPilot project with an example scenario."""
    directory = directory.resolve()
    scenarios_dir = directory / "scenarios"
    scenarios_dir.mkdir(parents=True, exist_ok=True)

    example = scenarios_dir / "example.py"
    if not example.exists():
        example.write_text(
            '''\
"""
Example LoadPilot scenario.

Run:
    loadpilot run scenarios/example.py --target http://localhost:8000
"""

from loadpilot import LoadClient, VUser, scenario, task


@scenario(
    rps=20,
    duration="1m",
    ramp_up="10s",
    thresholds={"p99_ms": 500, "error_rate": 1.0},
)
class ExampleFlow(VUser):
    """Hits /health (weight 3) and / (weight 1)."""

    @task(weight=3)
    def health(self, client: LoadClient):
        client.get("/health")

    @task(weight=1)
    def root(self, client: LoadClient):
        client.get("/")
''',
            encoding="utf-8",
        )

    env_example = directory / ".env.example"
    if not env_example.exists():
        env_example.write_text(
            "LOADPILOT_TARGET=http://localhost:8000\n",
            encoding="utf-8",
        )

    # Copy monitoring stack (Prometheus + Grafana with auto-provisioned dashboard).
    monitoring_dir = directory / "monitoring"
    monitoring_dir.mkdir(exist_ok=True)
    templates_dir = Path(__file__).parent / "templates" / "monitoring"
    for src in templates_dir.iterdir():
        dst = monitoring_dir / src.name
        if not dst.exists():
            import shutil

            shutil.copy2(src, dst)

    console.print(f"[bold green]Initialised LoadPilot project[/] in [cyan]{directory}[/]")
    console.print()
    console.print(
        "  [dim]scenarios/example.py[/]          — edit this to write your first scenario"
    )
    console.print(
        "  [dim].env.example[/]                   — copy to .env and set LOADPILOT_TARGET"
    )
    console.print(
        "  [dim]monitoring/docker-compose.yml[/]  — Prometheus + Grafana (auto-configured)"
    )
    console.print()
    console.print("Run your first test:")
    try:
        scenario_path = scenarios_dir.relative_to(Path.cwd()) / "example.py"
    except ValueError:
        scenario_path = scenarios_dir / "example.py"
    console.print(f"  [bold]loadpilot run {scenario_path} --target http://localhost:8000[/]")
    console.print()
    console.print("Start live monitoring (optional):")
    try:
        monitoring_rel = monitoring_dir.relative_to(Path.cwd())
    except ValueError:
        monitoring_rel = monitoring_dir
    console.print(f"  [bold]docker compose -f {monitoring_rel}/docker-compose.yml up -d[/]")
    console.print("  Then open [cyan]http://localhost:3000[/] → Dashboards → LoadPilot")


@app.command("version")
def version_command():
    """Print LoadPilot version."""
    from importlib.metadata import version as pkg_version

    try:
        v = pkg_version("loadpilot")
    except Exception:
        v = "0.1.0-dev"
    console.print(f"LoadPilot [bold]{v}[/]")


_DEFAULT_BASELINE = Path(".loadpilot") / "baseline.json"


@app.command("compare")
def compare_command(
    current: Path = typer.Argument(..., help="Current results JSON to compare against baseline."),
    baseline: Path = typer.Argument(
        _DEFAULT_BASELINE,
        help="Baseline results JSON. Defaults to .loadpilot/baseline.json.",
    ),
    regression_threshold: float = typer.Option(
        10.0,
        "--threshold",
        help="Fail with exit code 1 if any metric regressed by more than this % (default: 10).",
    ),
):
    """Compare two --results-json files and show metric deltas."""
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
            console.print(f"[yellow]Warning:[/] different scenarios ({b_scenario} vs {c_scenario})\n")

    metrics = [
        ("RPS actual",   "rps_actual",    "higher",  "{:.1f}",  ""),
        ("p50 latency",  "p50_ms",        "lower",   "{:.0f}",  "ms"),
        ("p95 latency",  "p95_ms",        "lower",   "{:.0f}",  "ms"),
        ("p99 latency",  "p99_ms",        "lower",   "{:.0f}",  "ms"),
        ("max latency",  "max_ms",        "lower",   "{:.0f}",  "ms"),
        ("error rate",   "error_rate_pct","lower",   "{:.2f}",  "%"),
    ]

    header = f"  {'':18}  {'baseline':>12}  {'current':>12}  {'diff':>10}"
    console.print(header)
    console.print("  " + "─" * 58)

    regressions: list[str] = []

    for label, key, better, fmt, unit in metrics:
        b_val = b.get(key)
        c_val = c.get(key)
        if b_val is None or c_val is None:
            continue

        b_str = fmt.format(b_val) + unit
        c_str = fmt.format(c_val) + unit

        if b_val == 0:
            diff_pct = 0.0
        else:
            diff_pct = (c_val - b_val) / abs(b_val) * 100

        if diff_pct == 0:
            diff_str = "—"
            color = "dim"
        elif (better == "lower" and diff_pct < 0) or (better == "higher" and diff_pct > 0):
            diff_str = f"{diff_pct:+.1f}%"
            color = "green"
        else:
            diff_str = f"{diff_pct:+.1f}%"
            color = "red"
            if abs(diff_pct) >= regression_threshold:
                regressions.append(f"{label} regressed {diff_pct:+.1f}%")

        console.print(
            f"  {label:<18}  {b_str:>12}  {c_str:>12}  [{color}]{diff_str:>10}[/{color}]"
        )

    console.print()

    if regressions:
        for r in regressions:
            console.print(f"  [red]✗[/]  {r}")
        console.print(
            f"\n[bold red]Regression detected (threshold: {regression_threshold:.0f}%).[/] "
            "Exiting with code 1."
        )
        raise typer.Exit(1)
    else:
        console.print("[bold green]No regressions detected.[/]")


if __name__ == "__main__":
    app()
