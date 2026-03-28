"""Project scaffolding logic for loadpilot init command."""

from __future__ import annotations

import shutil
from pathlib import Path

from rich.console import Console

console = Console()

_EXAMPLE_SCENARIO = '''\
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
'''


def scaffold_project(directory: Path) -> None:
    directory = directory.resolve()
    scenarios_dir = directory / "scenarios"
    scenarios_dir.mkdir(parents=True, exist_ok=True)

    example = scenarios_dir / "example.py"
    if not example.exists():
        example.write_text(_EXAMPLE_SCENARIO, encoding="utf-8")

    env_example = directory / ".env.example"
    if not env_example.exists():
        env_example.write_text("LOADPILOT_TARGET=http://localhost:8000\n", encoding="utf-8")

    # Copy monitoring stack (Prometheus + Grafana with auto-provisioned dashboard).
    monitoring_dir = directory / "monitoring"
    monitoring_dir.mkdir(exist_ok=True)
    templates_dir = Path(__file__).parent / "templates" / "monitoring"
    for src in templates_dir.iterdir():
        dst = monitoring_dir / src.name
        if not dst.exists():
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
