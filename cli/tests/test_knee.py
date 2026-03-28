"""Tests for knee point detection (_knee.py)."""

from __future__ import annotations

import pytest

from loadpilot._knee import KneePoint, detect_knee_point
from loadpilot.models import AgentMetrics, LatencyStats, ScenarioPlan, TaskPlan

# ── Helpers ───────────────────────────────────────────────────────────────────


def _plan(
    rps: int = 100,
    duration_secs: int = 50,
    steps: int = 5,
    mode: str = "step",
) -> ScenarioPlan:
    return ScenarioPlan(
        name="T",
        rps=rps,
        duration_secs=duration_secs,
        ramp_up_secs=0,
        mode=mode,
        steps=steps,
        target_url="http://localhost",
        tasks=[TaskPlan(name="t", weight=1, url="/", method="GET")],
    )


def _snap(
    elapsed: float,
    phase: str = "steady",
    p99_ms: float = 10.0,
    requests_total: int = 0,
    errors_total: int = 0,
    target_rps: float = 20.0,
) -> AgentMetrics:
    return AgentMetrics(
        timestamp_secs=1000.0 + elapsed,
        elapsed_secs=elapsed,
        current_rps=target_rps,
        target_rps=target_rps,
        requests_total=requests_total,
        errors_total=errors_total,
        active_workers=1,
        latency=LatencyStats(p99_ms=p99_ms),
        phase=phase,
    )


def _build_snapshots(
    plan: ScenarioPlan,
    per_step_p99: list[float],
    per_step_errors: list[int],
    requests_per_step: int = 200,
) -> list[AgentMetrics]:
    """Generate 10 snapshots per step with cumulative counters."""
    snaps: list[AgentMetrics] = []
    step_dur = plan.duration_secs / plan.steps
    ticks_per_step = 10
    tick_dur = step_dur / ticks_per_step

    cum_req = 0
    cum_err = 0
    for step_i, (p99, errs) in enumerate(zip(per_step_p99, per_step_errors)):
        cum_err += errs
        for t in range(ticks_per_step):
            elapsed = step_i * step_dur + (t + 1) * tick_dur
            cum_req += requests_per_step // ticks_per_step
            target_rps = plan.rps * (step_i + 1) / plan.steps
            snaps.append(
                _snap(
                    elapsed=elapsed,
                    p99_ms=p99,
                    requests_total=cum_req,
                    errors_total=cum_err,
                    target_rps=target_rps,
                )
            )
    return snaps


# ── detect_knee_point: guard conditions ──────────────────────────────────────


def test_returns_none_for_non_step_mode():
    plan = _plan(mode="ramp")
    snaps = [_snap(1.0)]
    assert detect_knee_point(snaps, plan, {"p99_ms": 500.0}) is None


def test_returns_none_when_no_thresholds():
    plan = _plan()
    snaps = _build_snapshots(plan, [10.0] * 5, [0] * 5)
    assert detect_knee_point(snaps, plan, {}) is None


def test_returns_none_when_thresholds_have_no_relevant_keys():
    plan = _plan()
    snaps = _build_snapshots(plan, [10.0] * 5, [0] * 5)
    assert detect_knee_point(snaps, plan, {"max_ms": 1000.0}) is None


def test_returns_none_when_snapshots_empty():
    plan = _plan()
    assert detect_knee_point([], plan, {"p99_ms": 500.0}) is None


# ── detect_knee_point: basic detection ───────────────────────────────────────


def test_all_steps_pass_returns_last_step():
    plan = _plan(steps=5)
    snaps = _build_snapshots(plan, [10.0, 20.0, 30.0, 40.0, 50.0], [0] * 5)
    knee = detect_knee_point(snaps, plan, {"p99_ms": 500.0})
    assert knee is not None
    assert knee.step == 5
    assert knee.total_steps == 5


def test_detects_knee_at_step_three_by_p99():
    # Steps 1-3 pass (p99 ≤ 100ms), steps 4-5 fail (p99 > 100ms).
    plan = _plan(steps=5, rps=100)
    snaps = _build_snapshots(
        plan,
        per_step_p99=[20.0, 40.0, 80.0, 200.0, 500.0],
        per_step_errors=[0] * 5,
    )
    knee = detect_knee_point(snaps, plan, {"p99_ms": 100.0})
    assert knee is not None
    assert knee.step == 3
    assert knee.rps == pytest.approx(60.0)


def test_detects_knee_at_step_two_by_error_rate():
    # 200 req/step; step 3 has 10 errors = 5% > 1% threshold.
    plan = _plan(steps=5, rps=100)
    snaps = _build_snapshots(
        plan,
        per_step_p99=[10.0] * 5,
        per_step_errors=[0, 0, 10, 30, 50],
        requests_per_step=200,
    )
    knee = detect_knee_point(snaps, plan, {"error_rate": 1.0})
    assert knee is not None
    assert knee.step == 2


def test_returns_none_when_first_step_fails():
    plan = _plan(steps=3, rps=100)
    snaps = _build_snapshots(
        plan,
        per_step_p99=[999.0, 999.0, 999.0],
        per_step_errors=[0] * 3,
    )
    knee = detect_knee_point(snaps, plan, {"p99_ms": 100.0})
    assert knee is None


def test_knee_point_fields_are_correct():
    plan = _plan(steps=4, rps=100, duration_secs=40)
    snaps = _build_snapshots(
        plan,
        per_step_p99=[10.0, 20.0, 40.0, 500.0],
        per_step_errors=[0] * 4,
        requests_per_step=100,
    )
    knee = detect_knee_point(snaps, plan, {"p99_ms": 100.0})
    assert knee is not None
    assert isinstance(knee, KneePoint)
    assert knee.step == 3
    assert knee.total_steps == 4
    assert knee.max_rps == 100.0
    assert knee.rps == pytest.approx(75.0)


def test_combined_slo_both_metrics_must_pass():
    # Step 3: p99 OK but error_rate fails → knee should be step 2.
    plan = _plan(steps=4, rps=100)
    snaps = _build_snapshots(
        plan,
        per_step_p99=[10.0, 20.0, 30.0, 500.0],
        per_step_errors=[0, 0, 20, 50],
        requests_per_step=200,
    )
    knee = detect_knee_point(snaps, plan, {"p99_ms": 100.0, "error_rate": 5.0})
    assert knee is not None
    assert knee.step == 2
