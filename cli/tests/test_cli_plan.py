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
