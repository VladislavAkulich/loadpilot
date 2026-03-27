from typing import Literal
from urllib.parse import urlparse

from pydantic import BaseModel, Field, field_validator, model_validator

_VALID_METHODS = {"GET", "POST", "PUT", "PATCH", "DELETE"}


class TaskPlan(BaseModel):
    """Definition of a single task that workers will execute."""

    name: str
    weight: int = 1
    url: str
    method: str = "GET"
    headers: dict[str, str] = Field(default_factory=dict)
    body_template: str | None = None  # JSON string template, may contain {{variable}} placeholders

    @field_validator("name")
    @classmethod
    def name_not_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("task name must not be empty")
        return v

    @field_validator("weight")
    @classmethod
    def weight_positive(cls, v: int) -> int:
        if v <= 0:
            raise ValueError(f"task weight must be > 0, got {v}")
        return v

    @field_validator("url")
    @classmethod
    def url_not_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("task url must not be empty")
        return v

    @field_validator("method")
    @classmethod
    def method_valid(cls, v: str) -> str:
        upper = v.upper()
        if upper not in _VALID_METHODS:
            raise ValueError(f"unknown HTTP method {v!r}; valid: {', '.join(sorted(_VALID_METHODS))}")
        return upper


class DataPoolEntry(BaseModel):
    """A single row in the pre-generated data pool."""

    values: dict[str, str | int | float | bool]


class VUserConfig(BaseModel):
    """Per-VUser state pre-extracted from on_start for distributed mode.

    Coordinator runs on_start for each VUser and captures the HTTP headers
    and URLs each task would use (via MockClient). Agents receive this pool
    and rotate through it, keeping all HTTP execution in pure Rust.
    """

    task_headers: dict[str, dict[str, str]] = Field(default_factory=dict)
    # Per-VUser URL overrides: populated when on_start sets state used in URLs
    # (e.g. self.project_id). Takes precedence over the task's default URL.
    task_urls: dict[str, str] = Field(default_factory=dict)


class ScenarioPlan(BaseModel):
    """Serialized test plan sent from Python CLI to Rust coordinator."""

    name: str
    rps: int
    duration_secs: int
    ramp_up_secs: int
    mode: Literal["constant", "ramp", "step", "spike"] = "ramp"
    steps: int = 5  # used by mode="step": number of equal steps over duration
    target_url: str
    tasks: list[TaskPlan] = Field(default_factory=list)
    data_pool: list[DataPoolEntry] = Field(default_factory=list)
    # PyO3 bridge fields — present only in single-process mode with Python callbacks.
    scenario_file: str | None = None
    scenario_class: str | None = None
    n_vusers: int | None = None
    # Pre-auth pool for distributed mode (replaces PyO3 in distributed).
    # Each entry holds the headers each task would use after on_start.
    vuser_configs: list[VUserConfig] = Field(default_factory=list)
    # SLA thresholds: {metric: max_value}. Checked by CLI after the test.
    thresholds: dict[str, float] = Field(default_factory=dict)

    @field_validator("name")
    @classmethod
    def name_not_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("scenario name must not be empty")
        return v

    @field_validator("rps")
    @classmethod
    def rps_positive(cls, v: int) -> int:
        if v <= 0:
            raise ValueError(f"rps must be > 0, got {v}")
        return v

    @field_validator("duration_secs")
    @classmethod
    def duration_positive(cls, v: int) -> int:
        if v <= 0:
            raise ValueError(f"duration must be > 0, got {v}s")
        return v

    @field_validator("ramp_up_secs")
    @classmethod
    def ramp_up_non_negative(cls, v: int) -> int:
        if v < 0:
            raise ValueError(f"ramp_up must be >= 0, got {v}s")
        return v

    @field_validator("steps")
    @classmethod
    def steps_positive(cls, v: int) -> int:
        if v < 1:
            raise ValueError(f"steps must be >= 1, got {v}")
        return v

    @field_validator("target_url")
    @classmethod
    def target_url_valid(cls, v: str) -> str:
        parsed = urlparse(v)
        if parsed.scheme not in ("http", "https"):
            raise ValueError(
                f"target_url must start with http:// or https://, got {v!r}"
            )
        if not parsed.netloc:
            raise ValueError(f"target_url has no host: {v!r}")
        return v

    @model_validator(mode="after")
    def cross_field_checks(self) -> "ScenarioPlan":
        errors: list[str] = []
        if not self.tasks:
            errors.append("scenario has no @task methods — add at least one @task")
        if self.ramp_up_secs > self.duration_secs:
            errors.append(
                f"ramp_up ({self.ramp_up_secs}s) exceeds duration ({self.duration_secs}s)"
            )
        if errors:
            raise ValueError("; ".join(errors))
        return self


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
