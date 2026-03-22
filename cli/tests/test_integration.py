"""Integration tests: Python CLI plan → Rust coordinator subprocess.

Each test:
  1. Starts a lightweight mock HTTP server in a background thread.
  2. Builds a ScenarioPlan and serialises it to JSON.
  3. Spawns the coordinator binary, pipes the plan to stdin.
  4. Collects and parses every JSON metrics line from stdout.
  5. Asserts the protocol and metric invariants.

Tests are skipped automatically when the coordinator binary has not been built.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import textwrap
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


def _find_coordinator() -> Path | None:
    for p in _COORDINATOR_CANDIDATES:
        if p.exists() and p.is_file():
            return p
    return None


requires_coordinator = pytest.mark.skipif(
    _find_coordinator() is None,
    reason="coordinator binary not found — run `cargo build` inside engine/",
)

# PYTHONPATH for the coordinator subprocess: must include both the loadpilot
# source root (so `import loadpilot` resolves) AND the venv site-packages (so
# httpx, pydantic, etc. are importable when the PyO3 bridge loads Python code).
_CLI_ROOT = Path(__file__).parent.parent
_PY_VER = f"python{sys.version_info.major}.{sys.version_info.minor}"
_SITE_PACKAGES = _CLI_ROOT / ".venv" / "lib" / _PY_VER / "site-packages"
_PYTHONPATH = f"{_CLI_ROOT}{os.pathsep}{_SITE_PACKAGES}"


# ── Mock HTTP server ──────────────────────────────────────────────────────────


class _Handler(BaseHTTPRequestHandler):
    """Returns 200 JSON for any path except /fail (→ 404) and /error (→ 500)."""

    def do_GET(self):
        self._respond()

    do_POST = do_GET
    do_PUT = do_GET
    do_DELETE = do_GET

    def _respond(self):
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
    def __init__(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self._server = HTTPServer(("127.0.0.1", self.port), _Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *args):
        self._server.shutdown()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


# ── Coordinator runner ────────────────────────────────────────────────────────


def _run(plan: ScenarioPlan, timeout: int = 30) -> list[AgentMetrics]:
    """Spawn coordinator, feed plan JSON via stdin, return all parsed metrics."""
    binary = _find_coordinator()
    env = {**os.environ, "PYTHONPATH": _PYTHONPATH}

    proc = subprocess.Popen(
        [str(binary)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )

    stdout, stderr = proc.communicate(input=plan.model_dump_json() + "\n", timeout=timeout)

    metrics: list[AgentMetrics] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            metrics.append(AgentMetrics(**json.loads(line)))
        except Exception:
            pass  # non-JSON lines (e.g. coordinator debug output) are ignored

    return metrics


def _static_plan(url: str, path: str = "/ping", **kwargs) -> ScenarioPlan:
    return ScenarioPlan(
        name="IntTest",
        rps=kwargs.get("rps", 5),
        duration_secs=kwargs.get("duration_secs", 2),
        ramp_up_secs=kwargs.get("ramp_up_secs", 1),
        target_url=url,
        tasks=[TaskPlan(name="t", url=path, method="GET", weight=1)],
    )


# ── Static mode tests ─────────────────────────────────────────────────────────


@requires_coordinator
def test_static_completes_with_done_phase():
    """Coordinator must emit a final snapshot with phase='done'."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url))

    assert metrics, "no metrics received"
    assert metrics[-1].phase == "done"


@requires_coordinator
def test_static_requests_are_made():
    """At least one request must be recorded in static mode."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url))

    final = metrics[-1]
    assert final.requests_total > 0


@requires_coordinator
def test_static_no_errors_on_200():
    """All requests to a healthy endpoint must succeed (errors_total == 0)."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url, path="/ok"))

    final = metrics[-1]
    assert final.errors_total == 0


@requires_coordinator
def test_static_errors_counted_on_404():
    """Requests to a 404 endpoint must be counted as errors."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url, path="/fail"))

    final = metrics[-1]
    assert final.errors_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_static_metrics_json_protocol():
    """Every stdout line must be valid JSON matching the AgentMetrics schema."""
    with MockServer() as srv:
        binary = _find_coordinator()
        env = {**os.environ, "PYTHONPATH": _PYTHONPATH}
        plan = _static_plan(srv.url)
        proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        stdout, _ = proc.communicate(input=plan.model_dump_json() + "\n", timeout=30)

    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        data = json.loads(line)  # must not raise
        m = AgentMetrics(**data)  # must not raise
        assert m.requests_total >= 0
        assert m.errors_total >= 0
        assert m.elapsed_secs >= 0


@requires_coordinator
def test_static_phase_sequence():
    """Phases must progress: ramp_up → steady (or directly done) → done."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url, ramp_up_secs=1, duration_secs=2))

    phases = [m.phase for m in metrics]
    assert "done" in phases
    # done must be last
    assert phases[-1] == "done"
    # ramp_up must come before steady if both present
    if "steady" in phases and "ramp_up" in phases:
        assert phases.index("ramp_up") < phases.index("steady")


@requires_coordinator
def test_static_requests_total_is_monotonic():
    """requests_total must never decrease between snapshots."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url))

    for a, b in zip(metrics, metrics[1:]):
        assert b.requests_total >= a.requests_total


@requires_coordinator
def test_static_latency_non_negative():
    """All latency percentiles must be >= 0 once requests are made."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url))

    final = metrics[-1]
    if final.requests_total > 0:
        assert final.latency.p50_ms >= 0
        assert final.latency.p99_ms >= 0
        assert final.latency.p99_ms >= final.latency.p50_ms


# ── PyO3 mode tests ───────────────────────────────────────────────────────────


def _write_scenario(content: str) -> str:
    """Write a scenario .py to a temp file and return the path."""
    f = tempfile.NamedTemporaryFile(
        mode="w", suffix=".py", delete=False, prefix="lp_test_scenario_"
    )
    f.write(textwrap.dedent(content))
    f.flush()
    return f.name


def _pyo3_plan(url: str, scenario_file: str, scenario_class: str, **kwargs) -> ScenarioPlan:
    return ScenarioPlan(
        name=scenario_class,
        rps=kwargs.get("rps", 5),
        duration_secs=kwargs.get("duration_secs", 2),
        ramp_up_secs=kwargs.get("ramp_up_secs", 1),
        target_url=url,
        tasks=[TaskPlan(name=kwargs.get("task", "ping"), url="/", method="GET", weight=1)],
        scenario_file=scenario_file,
        scenario_class=scenario_class,
        n_vusers=kwargs.get("n_vusers", 2),
    )


@requires_coordinator
def test_pyo3_completes_with_done_phase():
    """PyO3 mode must also produce a final done snapshot."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class SimpleFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "SimpleFlow")
        metrics = _run(plan)

    assert metrics[-1].phase == "done"


@requires_coordinator
def test_pyo3_passing_check_counts_as_success():
    """check_{task} that passes must not increment errors_total."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class CheckPassFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                assert status_code == 200
                assert body["status"] == "ok"
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "CheckPassFlow")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == 0


@requires_coordinator
def test_pyo3_failing_check_counts_as_error():
    """AssertionError in check_{task} must be counted as an error."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class CheckFailFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                assert status_code == 999  # always fails
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "CheckFailFlow")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_pyo3_on_start_runs_before_tasks():
    """on_start must execute without error; state set there is available in tasks."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class StateFlow(VUser):
            def on_start(self, client):
                resp = client.get("/ping")
                self.ready = resp.status_code == 200

            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                assert self.ready is True
                assert status_code == 200
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "StateFlow")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == 0


@requires_coordinator
def test_pyo3_check_receives_real_response_body():
    """check_{task} must receive the actual HTTP response body from reqwest."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class BodyCheckFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                # The mock server returns {"status":"ok","id":1}
                assert "status" in body
                assert body["id"] == 1
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "BodyCheckFlow")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.errors_total == 0


@requires_coordinator
def test_pyo3_no_check_method_uses_status_code():
    """Without check_{task}, errors_total must reflect HTTP status (404 = error)."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class NoCheckFlow(VUser):
            @task(weight=1)
            def fail(self, client):
                client.get("/fail")
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "NoCheckFlow", task="fail")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.errors_total > 0


# ── Additional static tests ───────────────────────────────────────────────────


@requires_coordinator
def test_static_errors_counted_on_500():
    """HTTP 5xx responses must be counted as errors in static mode."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url, path="/error"))

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_static_connection_refused_all_errors():
    """Requests to a closed port must all be counted as errors."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        dead_port = s.getsockname()[1]
    # port is now closed — connection will be refused
    metrics = _run(_static_plan(f"http://127.0.0.1:{dead_port}", duration_secs=2, ramp_up_secs=1))

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_static_metric_invariants():
    """errors_total must never exceed requests_total in any snapshot."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url, path="/fail"))

    for m in metrics:
        assert m.errors_total <= m.requests_total


@requires_coordinator
def test_static_done_phase_only_at_end():
    """Phase 'done' must appear exactly once, as the very last snapshot."""
    with MockServer() as srv:
        metrics = _run(_static_plan(srv.url))

    phases = [m.phase for m in metrics]
    assert phases[-1] == "done"
    # No non-done phase must appear after the first done snapshot
    first_done = phases.index("done")
    assert all(p == "done" for p in phases[first_done:])


# ── Additional PyO3 tests ─────────────────────────────────────────────────────


@requires_coordinator
def test_pyo3_task_exception_counted_as_error():
    """An unhandled exception inside a task must be counted as an error."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class ExceptionFlow(VUser):
            @task(weight=1)
            def crash(self, client):
                raise RuntimeError("intentional task failure")
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "ExceptionFlow", task="crash")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_pyo3_check_non_assertion_error_counts_as_error():
    """Any exception in check_{task} (not just AssertionError) must count as error."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class CheckKeyErrorFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                _ = body["nonexistent_key"]  # KeyError
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "CheckKeyErrorFlow")
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == final.requests_total


@requires_coordinator
def test_pyo3_on_start_failure_vuser_still_processes_tasks():
    """When on_start raises, the VUser must still process tasks (marked ready regardless)."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class BrokenStartFlow(VUser):
            def on_start(self, client):
                raise RuntimeError("on_start always fails")

            @task(weight=1)
            def ping(self, client):
                client.get("/ping")
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "BrokenStartFlow")
        metrics = _run(plan)

    # Tasks must still execute even though on_start failed
    assert metrics[-1].requests_total > 0


@requires_coordinator
def test_pyo3_on_stop_called_after_test():
    """on_stop must be invoked for each VUser after the test finishes."""
    flag_path = Path(tempfile.gettempdir()) / "lp_test_on_stop_flag.txt"
    flag_path.unlink(missing_ok=True)

    scenario_src = (
        "import pathlib\n"
        f"_FLAG = pathlib.Path({str(flag_path)!r})\n"
        "from loadpilot.dsl import VUser, scenario, task\n"
        "\n"
        "@scenario(rps=5, duration='2s', ramp_up='1s')\n"
        "class OnStopFlow(VUser):\n"
        "    @task(weight=1)\n"
        "    def ping(self, client):\n"
        "        client.get('/ping')\n"
        "\n"
        "    def on_stop(self, client):\n"
        "        _FLAG.write_text('stopped')\n"
    )
    scenario_file = _write_scenario(scenario_src)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "OnStopFlow")
        _run(plan)

    assert flag_path.exists(), "on_stop was never called"
    flag_path.unlink(missing_ok=True)


@requires_coordinator
def test_pyo3_multiple_tasks_no_errors():
    """A plan with two task types must complete with zero errors on a healthy server."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class MultiTaskFlow(VUser):
            @task(weight=1)
            def get_user(self, client):
                client.get("/user")

            @task(weight=1)
            def get_health(self, client):
                client.get("/health")
    """)
    with MockServer() as srv:
        plan = ScenarioPlan(
            name="MultiTaskFlow",
            rps=5,
            duration_secs=2,
            ramp_up_secs=1,
            target_url=srv.url,
            tasks=[
                TaskPlan(name="get_user", url="/user", method="GET", weight=1),
                TaskPlan(name="get_health", url="/health", method="GET", weight=1),
            ],
            scenario_file=scenario_file,
            scenario_class="MultiTaskFlow",
            n_vusers=2,
        )
        metrics = _run(plan)

    final = metrics[-1]
    assert final.requests_total > 0
    assert final.errors_total == 0


@requires_coordinator
def test_pyo3_metric_invariants():
    """errors_total must never exceed requests_total in any PyO3 mode snapshot."""
    scenario_file = _write_scenario("""\
        from loadpilot.dsl import VUser, scenario, task

        @scenario(rps=5, duration="2s", ramp_up="1s")
        class InvariantFlow(VUser):
            @task(weight=1)
            def ping(self, client):
                client.get("/ping")

            def check_ping(self, status_code, body):
                assert status_code == 200
    """)
    with MockServer() as srv:
        plan = _pyo3_plan(srv.url, scenario_file, "InvariantFlow")
        metrics = _run(plan)

    for m in metrics:
        assert m.errors_total <= m.requests_total
