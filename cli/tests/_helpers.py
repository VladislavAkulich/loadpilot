"""Shared fixtures and helpers for all LoadPilot tests."""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

from loadpilot.models import AgentMetrics, ScenarioPlan, TaskPlan

# ── Binary discovery ──────────────────────────────────────────────────────────

_ENGINE_ROOT = Path(__file__).parent.parent.parent / "engine"

_COORDINATOR_CANDIDATES = [
    _ENGINE_ROOT / "target" / "debug" / "coordinator",
    _ENGINE_ROOT / "target" / "release" / "coordinator",
]


def find_coordinator() -> Path | None:
    for p in _COORDINATOR_CANDIDATES:
        if p.exists() and p.is_file():
            return p
    return None


requires_coordinator = pytest.mark.skipif(
    find_coordinator() is None,
    reason="coordinator binary not found — run `cargo build` inside engine/",
)

_CLI_ROOT = Path(__file__).parent.parent
_PY_VER = f"python{sys.version_info.major}.{sys.version_info.minor}"
_SITE_PACKAGES = _CLI_ROOT / ".venv" / "lib" / _PY_VER / "site-packages"
PYTHONPATH = f"{_CLI_ROOT}{os.pathsep}{_SITE_PACKAGES}"


# ── Mock HTTP server ──────────────────────────────────────────────────────────


class _Handler(BaseHTTPRequestHandler):
    """
    Returns 200 JSON for any path.
    /fail  → 404   (error response)
    /error → 500   (server error)
    /slow  → 200 after latency_ms delay (set via server._latency_ms)
    """

    def do_GET(self):
        self._respond()

    do_POST = do_GET
    do_PUT = do_GET
    do_PATCH = do_GET
    do_DELETE = do_GET

    def _respond(self):
        latency_ms = getattr(self.server, "_latency_ms", 0)
        if latency_ms:
            import time

            time.sleep(latency_ms / 1000)

        if self.path.startswith("/fail"):
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"not found")
        elif self.path.startswith("/error"):
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"internal error")
        else:
            body = b'{"status":"ok","id":1}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    def log_message(self, *args):
        pass  # suppress output in test runs


class MockServer:
    """Lightweight HTTP server that runs in a background daemon thread."""

    def __init__(self, latency_ms: int = 0):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self._server = HTTPServer(("127.0.0.1", self.port), _Handler)
        self._server._latency_ms = latency_ms  # type: ignore[attr-defined]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *args):
        self._server.shutdown()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


# ── Port helpers ──────────────────────────────────────────────────────────────


def free_port() -> int:
    """Return an available TCP port on localhost."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# ── Plan helpers ──────────────────────────────────────────────────────────────


def static_plan(
    url: str,
    path: str = "/ping",
    *,
    rps: int = 5,
    duration_secs: int = 2,
    ramp_up_secs: int = 1,
    mode: str = "ramp",
) -> ScenarioPlan:
    return ScenarioPlan(
        name="IntTest",
        rps=rps,
        duration_secs=duration_secs,
        ramp_up_secs=ramp_up_secs,
        mode=mode,  # type: ignore[arg-type]
        target_url=url,
        tasks=[TaskPlan(name="t", url=path, method="GET", weight=1)],
    )


# ── Coordinator runner ────────────────────────────────────────────────────────


def run_coordinator(
    plan: ScenarioPlan,
    extra_args: list[str] | None = None,
    timeout: int = 30,
) -> tuple[list[AgentMetrics], int]:
    """Spawn coordinator, feed plan JSON via stdin.

    Returns (metrics, returncode).
    """
    binary = find_coordinator()
    env = {**os.environ, "PYTHONPATH": PYTHONPATH}
    proc = subprocess.Popen(
        [str(binary)] + (extra_args or []),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )
    stdout, _ = proc.communicate(input=plan.model_dump_json() + "\n", timeout=timeout)

    metrics: list[AgentMetrics] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            metrics.append(AgentMetrics(**json.loads(line)))
        except Exception:
            pass
    return metrics, proc.returncode
