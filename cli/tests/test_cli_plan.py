"""Tests for _build_plan() scenario selection logic."""

from pathlib import Path

import pytest

from loadpilot.cli import _build_plan
from loadpilot.dsl import VUser, _scenarios, scenario, task


@pytest.fixture(autouse=True)
def clear_scenarios():
    _scenarios.clear()
    yield
    _scenarios.clear()


def _register_two():
    @scenario(rps=10, duration="1m")
    class FlowA(VUser):
        @task()
        def hit(self, client):
            client.get("/a")

    @scenario(rps=20, duration="2m")
    class FlowB(VUser):
        @task()
        def hit(self, client):
            client.get("/b")


# ── single scenario ───────────────────────────────────────────────────────────

def test_single_scenario_no_name_uses_it(tmp_path):
    @scenario(rps=30)
    class OnlyFlow(VUser):
        @task()
        def hit(self, client):
            client.get("/")

    plan = _build_plan(tmp_path / "s.py", "http://localhost", scenario_name=None)
    assert plan.name == "OnlyFlow"
    assert plan.rps == 30


# ── multiple scenarios ────────────────────────────────────────────────────────

def test_select_by_name_first(tmp_path):
    _register_two()
    plan = _build_plan(tmp_path / "s.py", "http://localhost", scenario_name="FlowA")
    assert plan.name == "FlowA"
    assert plan.rps == 10


def test_select_by_name_second(tmp_path):
    _register_two()
    plan = _build_plan(tmp_path / "s.py", "http://localhost", scenario_name="FlowB")
    assert plan.name == "FlowB"
    assert plan.rps == 20


def test_wrong_name_raises_value_error(tmp_path):
    _register_two()
    with pytest.raises(ValueError, match="FlowA"):
        _build_plan(tmp_path / "s.py", "http://localhost", scenario_name="Missing")


def test_wrong_name_error_lists_available(tmp_path):
    _register_two()
    with pytest.raises(ValueError) as exc_info:
        _build_plan(tmp_path / "s.py", "http://localhost", scenario_name="Missing")
    msg = str(exc_info.value)
    assert "FlowA" in msg
    assert "FlowB" in msg


def test_no_scenarios_raises_value_error(tmp_path):
    with pytest.raises(ValueError, match="No @scenario"):
        _build_plan(tmp_path / "s.py", "http://localhost", scenario_name=None)


# ── distributed mode ──────────────────────────────────────────────────────────

def test_static_scenario_distributed_no_pyo3(tmp_path):
    """Static scenario (no on_start) in distributed mode: no PyO3 bridge, no vuser_configs."""
    @scenario(rps=10)
    class StaticFlow(VUser):
        @task()
        def hit(self, client):
            client.get("/ping")

    plan = _build_plan(tmp_path / "s.py", "http://localhost", distributed=True)
    assert plan.scenario_file is None
    assert plan.scenario_class is None
    assert plan.vuser_configs == []


def test_non_distributed_with_on_start_uses_pyo3(tmp_path):
    """Non-distributed scenario with on_start uses PyO3 bridge."""
    @scenario(rps=10)
    class AuthFlow(VUser):
        def on_start(self, client):
            self.token = "tok"

        @task()
        def hit(self, client):
            client.get("/api", headers={"Authorization": f"Bearer {self.token}"})

    plan = _build_plan(tmp_path / "s.py", "http://localhost", distributed=False)
    assert plan.scenario_file is not None
    assert plan.scenario_class == "AuthFlow"
    assert plan.vuser_configs == []


def test_distributed_with_on_start_disables_pyo3(tmp_path):
    """Distributed mode with on_start: PyO3 bridge disabled even if on_start present."""
    @scenario(rps=10)
    class AuthFlow(VUser):
        def on_start(self, client):
            # on_start would fail against real server — pre-auth pool handles it
            pass

        @task()
        def hit(self, client):
            client.get("/api")

    # distributed=True — on_start will fail (no real server), pre-auth falls back
    plan = _build_plan(tmp_path / "s.py", "http://localhost", distributed=True)
    # PyO3 bridge must be disabled regardless
    assert plan.scenario_file is None
    assert plan.scenario_class is None


def test_distributed_vuser_configs_empty_when_no_on_start(tmp_path):
    """No on_start → vuser_configs stays empty in distributed mode."""
    @scenario(rps=5)
    class SimpleFlow(VUser):
        @task()
        def ping(self, client):
            client.get("/ping")

    plan = _build_plan(tmp_path / "s.py", "http://localhost", distributed=True)
    assert plan.vuser_configs == []
