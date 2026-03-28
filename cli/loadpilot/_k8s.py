"""Kubernetes deploy/teardown logic for loadpilot deploy and teardown commands."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console

console = Console()

_CHART_DIR = Path(__file__).parent / "charts" / "loadpilot"
_HELM_RELEASE = "loadpilot"


def _check_helm() -> None:
    if shutil.which("helm") is None:
        console.print(
            "[red]Error:[/] helm is not installed.\n"
            "Install it from https://helm.sh/docs/intro/install/"
        )
        raise typer.Exit(1)


def helm_deploy(
    agents: int,
    namespace: str,
    nats_service: str,
    no_monitoring: bool,
    set_values: Optional[list[str]],
) -> None:
    _check_helm()
    console.print(
        f"[bold green]Deploying LoadPilot[/] — {agents} agent(s) · namespace: [cyan]{namespace}[/]"
    )

    cmd = [
        "helm", "upgrade", "--install", _HELM_RELEASE, str(_CHART_DIR),
        "--namespace", namespace,
        "--create-namespace",
        "--set", f"agent.replicas={agents}",
        "--set", f"nats.service.type={nats_service}",
        "--set", f"monitoring.enabled={str(not no_monitoring).lower()}",
        "--wait",
    ]
    if set_values:
        for v in set_values:
            cmd += ["--set", v]

    result = subprocess.run(cmd)
    if result.returncode != 0:
        raise typer.Exit(result.returncode)

    console.print("\n[bold green]Deployed.[/]")
    console.print(
        "\nGet the NATS external address:\n"
        f"  [bold]kubectl get svc {_HELM_RELEASE}-nats -n {namespace}[/]\n"
    )
    console.print(
        "Then run your test:\n"
        f"  [bold]loadpilot run scenarios/checkout.py "
        f"--nats-url nats://<EXTERNAL-IP>:4222 --external-agents {agents}[/]"
    )


def helm_teardown(namespace: str) -> None:
    _check_helm()
    console.print(f"Removing [cyan]{_HELM_RELEASE}[/] from namespace [cyan]{namespace}[/]…")
    result = subprocess.run(["helm", "uninstall", _HELM_RELEASE, "--namespace", namespace])
    if result.returncode != 0:
        raise typer.Exit(result.returncode)
    console.print("[bold green]Teardown complete.[/]")
