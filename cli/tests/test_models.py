import pytest

from loadpilot.models import ScenarioPlan, TaskPlan, parse_duration


# ── parse_duration ────────────────────────────────────────────────────────────

def test_parse_duration_seconds():
    assert parse_duration("30s") == 30


def test_parse_duration_minutes():
    assert parse_duration("2m") == 120


def test_parse_duration_combined():
    assert parse_duration("2m30s") == 150


def test_parse_duration_bare_number():
    assert parse_duration("45") == 45


def test_parse_duration_whitespace():
    assert parse_duration("  1m  ") == 60


def test_parse_duration_unknown_unit():
    with pytest.raises(ValueError, match="Unknown duration unit"):
        parse_duration("5h")


# ── TaskPlan ──────────────────────────────────────────────────────────────────

def test_task_plan_defaults():
    t = TaskPlan(name="read", url="/api/data")
    assert t.method == "GET"
    assert t.weight == 1
    assert t.headers == {}
    assert t.body_template is None


def test_task_plan_explicit_fields():
    t = TaskPlan(
        name="write",
        url="/api/items",
        method="POST",
        weight=5,
        headers={"X-Auth": "token"},
        body_template='{"name": "test"}',
    )
    assert t.weight == 5
    assert t.headers["X-Auth"] == "token"
    assert t.body_template is not None


# ── ScenarioPlan ──────────────────────────────────────────────────────────────

def test_scenario_plan_required_fields():
    plan = ScenarioPlan(
        name="MyScenario",
        rps=100,
        duration_secs=60,
        ramp_up_secs=10,
        target_url="http://localhost:8000",
    )
    assert plan.name == "MyScenario"
    assert plan.rps == 100
    assert plan.mode == "ramp"
    assert plan.tasks == []


def test_scenario_plan_json_roundtrip():
    plan = ScenarioPlan(
        name="S",
        rps=50,
        duration_secs=30,
        ramp_up_secs=5,
        target_url="http://localhost",
        tasks=[TaskPlan(name="t", url="/ping", method="GET")],
    )
    restored = ScenarioPlan.model_validate_json(plan.model_dump_json())
    assert restored.name == plan.name
    assert restored.rps == plan.rps
    assert restored.tasks[0].name == "t"
    assert restored.tasks[0].method == "GET"


def test_scenario_plan_bridge_fields_default_none():
    plan = ScenarioPlan(
        name="S", rps=1, duration_secs=10, ramp_up_secs=5,
        target_url="http://localhost",
    )
    assert plan.scenario_file is None
    assert plan.scenario_class is None
    assert plan.n_vusers is None


def test_scenario_plan_with_bridge_fields():
    plan = ScenarioPlan(
        name="S", rps=10, duration_secs=60, ramp_up_secs=10,
        target_url="http://localhost",
        scenario_file="/path/to/scenario.py",
        scenario_class="MyFlow",
        n_vusers=5,
    )
    assert plan.scenario_file == "/path/to/scenario.py"
    assert plan.scenario_class == "MyFlow"
    assert plan.n_vusers == 5
