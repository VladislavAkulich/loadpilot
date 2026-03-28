"""LoadPilot CLI — command definitions."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console

from loadpilot import report as _report
from loadpilot._compare import run_compare
from loadpilot._coordinator import _find_coordinator_binary, _load_scenario_file
from loadpilot._display import _check_thresholds, _resolve_target
from loadpilot._init import scaffold_project
from loadpilot._k8s import helm_deploy, helm_teardown
from loadpilot._plan_builder import _build_plan
from loadpilot._results import save_baseline, save_results_json
from loadpilot._runner import _build_coordinator_cmd, _run_local, _run_remote
from loadpilot._tui import _tui_pick_scenario
from loadpilot.dsl import _scenarios

app = typer.Typer(
    name="loadpilot",
    help="LoadPilot — Python DSL + Rust engine load testing tool.",
    add_completion=False,
)

agents_app = typer.Typer(help="Manage LoadPilot agents.")
app.add_typer(agents_app, name="agents")

console = Console()

# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


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
        help="External NATS URL for remote agents (e.g. nats://host:4222 or tls://host:4222). Use with --external-agents.",
    ),
    nats_token: Optional[str] = typer.Option(
        None,
        "--nats-token",
        envvar="NATS_TOKEN",
        help="Token for NATS authentication. Can also be set via NATS_TOKEN env var.",
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
    save_baseline_flag: bool = typer.Option(
        False,
        "--save-baseline",
        help="Save results as the baseline for future comparisons (.loadpilot/baseline.json).",
    ),
    coordinator_url: Optional[str] = typer.Option(
        None,
        "--coordinator-url",
        help="URL of coordinator service in k8s (e.g. http://localhost:8080). "
        "When set, the coordinator runs in the cluster instead of locally.",
        envvar="LOADPILOT_COORDINATOR_URL",
    ),
):
    """Run a load test scenario against TARGET. Omit the file to browse interactively."""
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

        if target is None:
            import questionary

            entered = questionary.text("Target URL:", default="http://localhost:8000").ask()
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
            selected = questionary.select("Select a scenario to run:", choices=choices).ask()
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
        plan, _preauth_instances = _build_plan(
            scenario_file, target, scenario_name, distributed=is_distributed
        )
    except ValueError as exc:
        console.print(f"[red]Error:[/] {exc}")
        raise typer.Exit(1)
    except Exception as exc:
        if type(exc).__name__ == "ValidationError":
            console.print("[red]Scenario validation failed:[/]")
            for err in exc.errors():
                loc = " → ".join(str(p) for p in err["loc"]) if err["loc"] else "plan"
                console.print(f"  [red]•[/] {loc}: {err['msg']}")
            raise typer.Exit(1)
        raise

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
        for instance, client in _preauth_instances:
            try:
                if hasattr(instance, "on_stop"):
                    instance.on_stop(client)
            except Exception:
                pass
            finally:
                client.close()
        raise typer.Exit(0)

    bridge_mode = "[cyan]PyO3[/]" if plan.scenario_file else "[dim]static[/]"
    console.print(
        f"[bold green]Starting LoadPilot[/] — scenario: [cyan]{plan.name}[/] "
        f"| target: [cyan]{target}[/] "
        f"| {plan.rps} RPS for {plan.duration_secs}s (ramp {plan.ramp_up_secs}s) "
        f"| mode: {bridge_mode}"
    )

    scenario_name = plan.name
    proc = None

    if coordinator_url:
        last_metrics, all_snapshots = _run_remote(plan_json, coordinator_url, scenario_name)
    else:
        try:
            coordinator_bin = _find_coordinator_binary()
        except FileNotFoundError as exc:
            console.print(f"[red]{exc}[/]")
            raise typer.Exit(1)

        coordinator_cmd = _build_coordinator_cmd(
            coordinator_bin, nats_url, nats_token, external_agents, agents
        )
        last_metrics, all_snapshots, proc = _run_local(plan_json, coordinator_cmd, scenario_name)

    # Run on_stop for each pre-auth VUser.
    if _preauth_instances:
        console.print("[dim]Distributed post-test: running on_stop for pre-auth VUsers…[/]")
        for instance, client in _preauth_instances:
            try:
                if hasattr(instance, "on_stop"):
                    instance.on_stop(client)
            except Exception:
                pass
            finally:
                client.close()

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
        save_results_json(results_json, last_metrics, scenario_name, target, plan)

    if save_baseline_flag and last_metrics:
        save_baseline(last_metrics, scenario_name, target, plan)

    threshold_failed = False
    if last_metrics and plan.thresholds:
        threshold_failed = _check_thresholds(last_metrics, plan.thresholds)

    if proc is not None and proc.returncode != 0:
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
    scaffold_project(directory)


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
    run_compare(current, baseline, regression_threshold)


@app.command("deploy")
def deploy_command(
    agents: int = typer.Option(3, "--agents", "-a", help="Number of agent replicas to deploy."),
    namespace: str = typer.Option("loadpilot", "--namespace", "-n", help="Kubernetes namespace."),
    nats_service: str = typer.Option(
        "LoadBalancer",
        "--nats-service",
        help="NATS service type: LoadBalancer (cloud) or NodePort (minikube).",
    ),
    no_monitoring: bool = typer.Option(
        False, "--no-monitoring", help="Skip Prometheus and Grafana."
    ),
    set_values: Optional[list[str]] = typer.Option(
        None, "--set", help="Override chart values: --set key=value."
    ),
):
    """Deploy LoadPilot agents + NATS + monitoring to Kubernetes."""
    helm_deploy(agents, namespace, nats_service, no_monitoring, set_values)


@app.command("teardown")
def teardown_command(
    namespace: str = typer.Option("loadpilot", "--namespace", "-n", help="Kubernetes namespace."),
):
    """Remove the LoadPilot stack from Kubernetes."""
    helm_teardown(namespace)


if __name__ == "__main__":
    app()
