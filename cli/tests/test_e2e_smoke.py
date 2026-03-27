"""E2e smoke tests covering all coordinator run modes.

Each test runs the real coordinator binary against an in-process mock HTTP
server. No external services required (embedded NATS for distributed mode).

Marks:
  @e2e — all tests in this file; run with: pytest -m e2e
  @requires_coordinator — skipped when coordinator binary not built

Modes covered:
  - Single-process (static, constant, ramp)
  - Distributed (--local-agents N)
  - Graceful shutdown (SIGTERM)
"""

from __future__ import annotations

import os
import signal
import subprocess
import time

import pytest

from tests._helpers import (
    PYTHONPATH,
    MockServer,
    find_coordinator,
    free_port,
    requires_coordinator,
    run_coordinator,
    static_plan,
)

pytestmark = [pytest.mark.e2e, requires_coordinator]


# ── Helpers ───────────────────────────────────────────────────────────────────


def _spawn(plan, extra_args=None):
    """Start coordinator process without waiting — for shutdown tests."""
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
    proc.stdin.write(plan.model_dump_json() + "\n")
    proc.stdin.close()
    return proc


# ── Single-process: constant mode ────────────────────────────────────────────


def test_constant_mode_completes():
    """mode=constant must complete with phase='done' and no ramp_up phase."""
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=3, ramp_up_secs=0, mode="constant")
        metrics, rc = run_coordinator(plan)

    assert rc == 0
    assert metrics, "no metrics received"
    assert metrics[-1].phase == "done"
    phases = {m.phase for m in metrics}
    assert "ramp_up" not in phases, "constant mode must not produce ramp_up phase"


def test_constant_mode_requests_approximate_rps_times_duration():
    """total requests must be within ±25% of rps × duration in constant mode."""
    rps, duration = 30, 3
    with MockServer() as srv:
        plan = static_plan(
            srv.url, rps=rps, duration_secs=duration, ramp_up_secs=0, mode="constant"
        )
        metrics, _ = run_coordinator(plan)

    final = metrics[-1]
    expected = rps * duration
    assert final.requests_total >= int(expected * 0.75), (
        f"too few requests: {final.requests_total} < {int(expected * 0.75)} (expected ~{expected})"
    )
    assert final.requests_total <= int(expected * 1.25), (
        f"too many requests: {final.requests_total} > {int(expected * 1.25)} (expected ~{expected})"
    )


# ── Single-process: ramp mode ─────────────────────────────────────────────────


def test_ramp_mode_phase_sequence():
    """mode=ramp must produce ramp_up before steady, done last."""
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=4, ramp_up_secs=2, mode="ramp")
        metrics, rc = run_coordinator(plan)

    assert rc == 0
    phases = [m.phase for m in metrics]
    assert phases[-1] == "done"
    if "ramp_up" in phases and "steady" in phases:
        assert phases.index("ramp_up") < phases.index("steady")


def test_ramp_mode_no_errors_on_healthy_server():
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=3, ramp_up_secs=1, mode="ramp")
        metrics, _ = run_coordinator(plan)

    assert metrics[-1].errors_total == 0


# ── Distributed: --local-agents ───────────────────────────────────────────────


def test_distributed_two_agents_completes():
    """--local-agents 2 must complete with phase='done'."""
    broker = f"127.0.0.1:{free_port()}"
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=4, ramp_up_secs=1)
        metrics, rc = run_coordinator(
            plan, extra_args=["--local-agents", "2", "--broker-addr", broker], timeout=45
        )

    assert rc == 0
    assert metrics, "no metrics received in distributed mode"
    assert metrics[-1].phase == "done"


def test_distributed_two_agents_no_errors():
    """Distributed mode against a healthy server must produce zero errors."""
    broker = f"127.0.0.1:{free_port()}"
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=4, ramp_up_secs=1)
        metrics, _ = run_coordinator(
            plan, extra_args=["--local-agents", "2", "--broker-addr", broker], timeout=45
        )

    assert metrics[-1].errors_total == 0


def test_distributed_four_agents_completes():
    """--local-agents 4 must complete cleanly."""
    broker = f"127.0.0.1:{free_port()}"
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=40, duration_secs=4, ramp_up_secs=1)
        metrics, rc = run_coordinator(
            plan, extra_args=["--local-agents", "4", "--broker-addr", broker], timeout=60
        )

    assert rc == 0
    assert metrics[-1].phase == "done"


def test_distributed_metric_invariants():
    """errors_total must never exceed requests_total in any distributed snapshot."""
    broker = f"127.0.0.1:{free_port()}"
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=20, duration_secs=4, ramp_up_secs=1)
        metrics, _ = run_coordinator(
            plan, extra_args=["--local-agents", "2", "--broker-addr", broker], timeout=45
        )

    for m in metrics:
        assert m.errors_total <= m.requests_total


# ── Graceful shutdown (SIGTERM) ───────────────────────────────────────────────


def test_graceful_shutdown_exits_cleanly():
    """SIGTERM during an active test must produce exit code 0 within 10s."""
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=50, duration_secs=30, ramp_up_secs=5)
        proc = _spawn(plan)

        time.sleep(2)
        proc.send_signal(signal.SIGTERM)

        try:
            proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            pytest.fail("coordinator did not exit within 10s after SIGTERM")

    assert proc.returncode == 0


def test_graceful_shutdown_emits_metrics_before_exit():
    """SIGTERM must not discard already-collected metrics — stdout must be non-empty."""
    import json

    from loadpilot.models import AgentMetrics

    with MockServer() as srv:
        plan = static_plan(srv.url, rps=50, duration_secs=30, ramp_up_secs=5)
        proc = _spawn(plan)

        time.sleep(2)
        proc.send_signal(signal.SIGTERM)

        try:
            stdout, _ = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            pytest.fail("coordinator did not exit within 10s after SIGTERM")

    metrics = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            metrics.append(AgentMetrics(**json.loads(line)))
        except Exception:
            pass

    assert metrics, "coordinator produced no metrics before shutdown"
    assert metrics[-1].phase == "done", (
        f"last phase after shutdown was {metrics[-1].phase!r}, expected 'done'"
    )


def test_graceful_shutdown_distributed():
    """SIGTERM in distributed mode must also exit cleanly."""
    broker = f"127.0.0.1:{free_port()}"
    with MockServer() as srv:
        plan = static_plan(srv.url, rps=40, duration_secs=30, ramp_up_secs=5)
        proc = _spawn(plan, extra_args=["--local-agents", "2", "--broker-addr", broker])

        # Wait for agents to register and start (synchronized start +2s needs to complete).
        time.sleep(6)
        proc.send_signal(signal.SIGTERM)

        try:
            proc.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            pytest.fail("coordinator (distributed) did not exit within 15s after SIGTERM")

    assert proc.returncode == 0
