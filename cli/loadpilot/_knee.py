"""Knee point detection for step mode load tests.

The knee point is the last step where all defined SLO thresholds
(p99_ms, error_rate) were not violated. It represents the maximum
sustainable load under the given SLO constraints.

Metric notes
------------
* ``p99_ms``     — cumulative from test start (histogram never resets).
  Bad requests in a step raise the cumulative p99 when they cross the
  99th-percentile boundary, so this is a practical indicator of knee.
* ``error_rate`` — computed as a delta within each step window, giving
  the per-step error percentage independently of earlier steps.
"""

from __future__ import annotations

from dataclasses import dataclass

from loadpilot.models import AgentMetrics, ScenarioPlan


@dataclass
class KneePoint:
    """The last step where all relevant SLO thresholds were satisfied."""

    step: int  # 1-based step index
    rps: float  # Target RPS at the knee step
    p99_ms: float  # Cumulative p99 at end of knee step
    error_rate_pct: float  # Delta error rate during knee step
    total_steps: int  # Total number of steps in the plan
    max_rps: float  # Plan's peak RPS target


def detect_knee_point(
    snapshots: list[AgentMetrics],
    plan: ScenarioPlan,
    thresholds: dict[str, float],
) -> KneePoint | None:
    """Find the last step satisfying all SLO thresholds in step mode.

    Returns ``None`` when:
    - ``plan.mode`` is not ``"step"``
    - no SLO-relevant thresholds (``p99_ms``, ``error_rate``) are defined
    - not enough snapshot data to evaluate at least one step
    """
    if plan.mode != "step" or not thresholds:
        return None

    slo_p99 = thresholds.get("p99_ms")
    slo_err = thresholds.get("error_rate")
    if slo_p99 is None and slo_err is None:
        return None

    n_steps = max(plan.steps, 1)
    step_dur = plan.duration_secs / n_steps

    # Group snapshots by step index; skip the final "done" snapshot.
    step_snaps: list[list[AgentMetrics]] = [[] for _ in range(n_steps)]
    for snap in snapshots:
        if snap.phase == "done":
            continue
        idx = min(int(snap.elapsed_secs / step_dur), n_steps - 1)
        step_snaps[idx].append(snap)

    knee: KneePoint | None = None
    prev_req = 0
    prev_err = 0

    for i, snaps in enumerate(step_snaps):
        if len(snaps) < 2:
            # Not enough data for this step; stop evaluation.
            break

        last = snaps[-1]
        step_rps = plan.rps * (i + 1) / n_steps

        # Cumulative p99 from all requests up to end of this step.
        p99 = last.latency.p99_ms

        # Delta error rate isolated to this step.
        delta_req = max(last.requests_total - prev_req, 0)
        delta_err = max(last.errors_total - prev_err, 0)
        error_rate = (delta_err / delta_req * 100) if delta_req > 0 else 0.0

        prev_req = last.requests_total
        prev_err = last.errors_total

        slo_ok = True
        if slo_p99 is not None and p99 > slo_p99:
            slo_ok = False
        if slo_err is not None and error_rate > slo_err:
            slo_ok = False

        if slo_ok:
            knee = KneePoint(
                step=i + 1,
                rps=step_rps,
                p99_ms=p99,
                error_rate_pct=error_rate,
                total_steps=n_steps,
                max_rps=float(plan.rps),
            )

    return knee
