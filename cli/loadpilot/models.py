from typing import Literal

from pydantic import BaseModel, Field


class TaskPlan(BaseModel):
    """Definition of a single task that workers will execute."""

    name: str
    weight: int = 1
    url: str
    method: str = "GET"
    headers: dict[str, str] = Field(default_factory=dict)
    body_template: str | None = None  # JSON string template, may contain {{variable}} placeholders


class DataPoolEntry(BaseModel):
    """A single row in the pre-generated data pool."""

    values: dict[str, str | int | float | bool]


class ScenarioPlan(BaseModel):
    """Serialized test plan sent from Python CLI to Rust coordinator."""

    name: str
    rps: int
    duration_secs: int
    ramp_up_secs: int
    mode: Literal["constant", "ramp", "spike"] = "ramp"
    target_url: str
    tasks: list[TaskPlan] = Field(default_factory=list)
    data_pool: list[DataPoolEntry] = Field(default_factory=list)
    # PyO3 bridge fields — present when the scenario has Python callbacks.
    scenario_file: str | None = None
    scenario_class: str | None = None
    n_vusers: int | None = None
    # SLA thresholds: {metric: max_value}. Checked by CLI after the test.
    thresholds: dict[str, float] = Field(default_factory=dict)


class LatencyStats(BaseModel):
    """Latency percentile statistics."""

    p50_ms: float = 0.0
    p95_ms: float = 0.0
    p99_ms: float = 0.0
    max_ms: float = 0.0
    min_ms: float = 0.0
    mean_ms: float = 0.0


class AgentMetrics(BaseModel):
    """Metrics snapshot streamed from Rust coordinator to Python CLI."""

    timestamp_secs: float
    elapsed_secs: float
    current_rps: float
    target_rps: float
    requests_total: int
    errors_total: int
    active_workers: int
    latency: LatencyStats
    phase: Literal["ramp_up", "steady", "ramp_down", "done"] = "ramp_up"


def parse_duration(s: str) -> int:
    """Parse a duration string like '1m', '30s', '2m30s' into seconds."""
    s = s.strip()
    total = 0
    current = ""
    for ch in s:
        if ch.isdigit():
            current += ch
        elif ch == "m":
            total += int(current) * 60
            current = ""
        elif ch == "s":
            total += int(current)
            current = ""
        else:
            raise ValueError(f"Unknown duration unit: {ch!r} in {s!r}")
    if current:
        # bare number → seconds
        total += int(current)
    return total
