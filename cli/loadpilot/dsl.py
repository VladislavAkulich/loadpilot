import functools
from dataclasses import dataclass, field
from typing import Callable, Any

_scenarios: list["ScenarioDef"] = []


def _clear_scenarios() -> None:
    """Reset the scenario registry. Called before loading each scenario file."""
    _scenarios.clear()


@dataclass
class TaskDef:
    name: str
    weight: int
    func: Callable


@dataclass
class ScenarioDef:
    name: str
    rps: int
    duration: str
    ramp_up: str
    tasks: list[TaskDef] = field(default_factory=list)
    cls: Any = None
    thresholds: dict[str, float] = field(default_factory=dict)


class VUser:
    """Base class for virtual users. Subclass and use @task decorators."""

    def on_start(self, client: Any) -> None:
        """Called once per virtual user before tasks begin. Override to set up state."""

    def on_stop(self, client: Any) -> None:
        """Called once per virtual user after all tasks complete. Override to tear down."""


def scenario(
    rps: int = 10,
    duration: str = "1m",
    ramp_up: str = "10s",
    thresholds: dict[str, float] | None = None,
):
    """Class decorator that registers a scenario definition.

    Args:
        rps: Target requests per second at peak load.
        duration: Total test duration, e.g. "1m", "30s", "2m30s".
        ramp_up: Time to ramp from 0 to target RPS, e.g. "10s", "1m".
        thresholds: Optional SLA limits. If any are exceeded the CLI exits with
            code 1. Supported keys: ``p50_ms``, ``p95_ms``, ``p99_ms``,
            ``max_ms``, ``error_rate`` (percent, e.g. ``1.0`` = 1 %).
            Example: ``{"p99_ms": 500, "error_rate": 1.0}``
    """

    def decorator(cls):
        s = ScenarioDef(
            name=cls.__name__,
            rps=rps,
            duration=duration,
            ramp_up=ramp_up,
            thresholds=thresholds or {},
            cls=cls,
        )
        # Collect tasks from class by inspecting methods for _task_weight attribute
        for attr_name in dir(cls):
            method = getattr(cls, attr_name)
            if hasattr(method, "_task_weight"):
                s.tasks.append(
                    TaskDef(
                        name=attr_name,
                        weight=method._task_weight,
                        func=method,
                    )
                )
        _scenarios.append(s)
        return cls

    return decorator


def task(weight: int = 1):
    """Method decorator that marks a method as a load test task.

    Args:
        weight: Relative weight for task selection. Higher weight = more frequent execution.
    """

    def decorator(func):
        @functools.wraps(func)
        def wrapper(*args, **kwargs):
            return func(*args, **kwargs)

        wrapper._task_weight = weight  # type: ignore[attr-defined]
        return wrapper

    return decorator
