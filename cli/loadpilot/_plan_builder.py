"""Plan construction from @scenario definitions."""

from __future__ import annotations

from pathlib import Path

from rich.console import Console

from loadpilot.dsl import _scenarios
from loadpilot.models import ScenarioPlan, TaskPlan, VUserConfig, parse_duration

console = Console()


def _build_plan(
    scenario_file: Path,
    target: str,
    scenario_name: str | None = None,
    distributed: bool = False,
) -> tuple[ScenarioPlan, list]:
    if not _scenarios:
        raise ValueError("No @scenario classes found in the scenario file.")

    if scenario_name:
        matches = [s for s in _scenarios if s.name == scenario_name]
        if not matches:
            available = ", ".join(s.name for s in _scenarios)
            raise ValueError(f"Scenario {scenario_name!r} not found. Available: {available}")
        s = matches[0]
    else:
        s = _scenarios[0]

    # n_vusers: enough VUsers to sustain peak RPS, staggered over ramp-up to
    # avoid triggering server-side rate limits on on_start (e.g. login endpoints).
    # Cap at 100: in PyO3 mode each VUser runs RustClient HTTP (~1ms/task),
    # so 100 VUsers can easily sustain 50 000 RPS — no need for more threads.
    n_vusers = min(max(5, s.rps // 100), 100)

    # Pre-run each task with a MockClient to extract the real URL and method.
    # This works for pure static scenarios (no on_start state needed).
    # For scenarios that reference self.* set in on_start the mock run may fail;
    # those errors are silently ignored and the task falls back to url="/".
    from loadpilot._bridge import MockClient as _MockClient

    try:
        tmp = s.cls()
    except Exception:
        tmp = None

    tasks: list[TaskPlan] = []
    for td in s.tasks:
        url, method, headers, body = "/", "GET", {}, None
        if tmp is not None:
            mock = _MockClient()
            try:
                import asyncio as _asyncio
                import inspect as _inspect

                result = td.func(tmp, mock)
                if _inspect.iscoroutine(result):
                    _asyncio.run(result)
                m, p, h, b = mock.get_call()
                if p:
                    url, method, headers, body = p, (m or "GET"), h, b
            except Exception:
                pass  # fall back to defaults for state-dependent tasks
        tasks.append(
            TaskPlan(
                name=td.name,
                weight=td.weight,
                url=url,
                method=method,
                headers=headers,
                body_template=body,
            )
        )

    # Enable the PyO3 bridge when Python needs to run at request time:
    # - lifecycle hooks (on_start / on_stop carry per-VUser state)
    # - check_{task} methods need the real HTTP response
    has_check = any(f"check_{td.name}" in s.cls.__dict__ for td in s.tasks)
    has_callbacks = "on_start" in s.cls.__dict__ or "on_stop" in s.cls.__dict__ or has_check

    # ── Distributed pre-auth pool ──────────────────────────────────────────────
    # In distributed mode PyO3 callbacks can't run on remote agents.
    # If the scenario has on_start (e.g. login → per-VUser auth token), we run
    # on_start N times here on the coordinator side using a real HTTP client,
    # then probe each @task with MockClient to capture what headers on_start set.
    # The resulting vuser_configs list is shipped with the plan so agents can
    # rotate through pre-authenticated header sets in pure Rust.
    vuser_configs: list[VUserConfig] = []
    # Keep pre-auth instances so we can call on_stop after the test completes.
    _preauth_instances: list[tuple] = []  # list of (instance, LoadClient)
    if distributed and "on_start" in s.cls.__dict__:
        from loadpilot.client import LoadClient as _LoadClient

        pool_size = min(n_vusers, 20)
        console.print(f"[dim]Distributed pre-auth: running on_start for {pool_size} VUsers…[/]")
        failed_first = False
        for i in range(pool_size):
            try:
                instance = s.cls()
                real_client = _LoadClient(target)
                instance.on_start(real_client)
                task_headers: dict[str, dict[str, str]] = {}
                task_urls: dict[str, str] = {}
                import asyncio as _asyncio
                import inspect as _inspect

                for td in s.tasks:
                    mock = _MockClient()
                    try:
                        result = td.func(instance, mock)
                        if _inspect.iscoroutine(result):
                            _asyncio.run(result)
                        _, p, h, _ = mock.get_call()
                        task_headers[td.name] = h
                        if p:
                            task_urls[td.name] = p
                    except Exception:
                        task_headers[td.name] = {}
                vuser_configs.append(VUserConfig(task_headers=task_headers, task_urls=task_urls))
                _preauth_instances.append((instance, real_client))
            except Exception as exc:
                if i == 0:
                    console.print(
                        f"[yellow]on_start pre-auth failed: {exc}. Falling back to static mode.[/]"
                    )
                    failed_first = True
                    break
        if failed_first:
            vuser_configs = []

    # In distributed mode with pre-auth pool, disable PyO3 bridge for agents —
    # agents will rotate through vuser_configs headers in pure Rust.
    # check_* / on_stop are skipped in distributed (status-based errors only).
    use_pyo3 = has_callbacks and not distributed

    return ScenarioPlan(
        name=s.name,
        rps=s.rps,
        duration_secs=parse_duration(s.duration),
        ramp_up_secs=parse_duration(s.ramp_up),
        mode=s.mode,
        steps=s.steps,
        target_url=target,
        tasks=tasks,
        scenario_file=str(scenario_file.resolve()) if use_pyo3 else None,
        scenario_class=s.name if use_pyo3 else None,
        n_vusers=n_vusers if use_pyo3 else None,
        vuser_configs=vuser_configs,
        thresholds=s.thresholds,
    ), _preauth_instances
