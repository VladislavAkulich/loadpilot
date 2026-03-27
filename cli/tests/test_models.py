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
        tasks=[TaskPlan(name="t", url="/ping")],
    )
    assert plan.name == "MyScenario"
    assert plan.rps == 100
    assert plan.mode == "ramp"
    assert len(plan.tasks) == 1


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
        name="S",
        rps=1,
        duration_secs=10,
        ramp_up_secs=5,
        target_url="http://localhost",
        tasks=[TaskPlan(name="t", url="/ping")],
    )
    assert plan.scenario_file is None
    assert plan.scenario_class is None
    assert plan.n_vusers is None


def test_scenario_plan_with_bridge_fields():
    plan = ScenarioPlan(
        name="S",
        rps=10,
        duration_secs=60,
        ramp_up_secs=10,
        target_url="http://localhost",
        tasks=[TaskPlan(name="t", url="/ping")],
        scenario_file="/path/to/scenario.py",
        scenario_class="MyFlow",
        n_vusers=5,
    )
    assert plan.scenario_file == "/path/to/scenario.py"
    assert plan.scenario_class == "MyFlow"
    assert plan.n_vusers == 5


# ── Validation ────────────────────────────────────────────────────────────────

_base_task = TaskPlan(name="t", url="/ping")


def _base_plan(**overrides):
    defaults = dict(
        name="S",
        rps=100,
        duration_secs=30,
        ramp_up_secs=5,
        target_url="http://localhost",
        tasks=[_base_task],
    )
    defaults.update(overrides)
    return ScenarioPlan(**defaults)


def test_validation_zero_rps():
    with pytest.raises(Exception, match="rps must be > 0"):
        _base_plan(rps=0)


def test_validation_zero_duration():
    with pytest.raises(Exception, match="duration must be > 0"):
        _base_plan(duration_secs=0)


def test_validation_ramp_up_exceeds_duration():
    with pytest.raises(Exception, match="ramp_up.*exceeds duration"):
        _base_plan(duration_secs=10, ramp_up_secs=20)


def test_validation_empty_tasks():
    with pytest.raises(Exception, match="no @task"):
        _base_plan(tasks=[])


def test_validation_invalid_target_url():
    with pytest.raises(Exception, match="http"):
        _base_plan(target_url="localhost:8080")


def test_validation_task_zero_weight():
    with pytest.raises(Exception, match="weight must be > 0"):
        TaskPlan(name="t", url="/ping", weight=0)


def test_validation_task_invalid_method():
    with pytest.raises(Exception, match="unknown HTTP method"):
        TaskPlan(name="t", url="/ping", method="FETCH")


def test_validation_task_method_normalized():
    t = TaskPlan(name="t", url="/ping", method="post")
    assert t.method == "POST"
