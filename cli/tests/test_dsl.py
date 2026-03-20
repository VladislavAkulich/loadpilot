import pytest

from loadpilot.dsl import VUser, _scenarios, scenario, task


@pytest.fixture(autouse=True)
def clear_scenarios():
    _scenarios.clear()
    yield
    _scenarios.clear()


# ── @scenario decorator ───────────────────────────────────────────────────────

def test_scenario_registers_class():
    @scenario(rps=50, duration="1m", ramp_up="10s")
    class MyFlow(VUser):
        pass

    assert len(_scenarios) == 1
    s = _scenarios[0]
    assert s.name == "MyFlow"
    assert s.rps == 50
    assert s.duration == "1m"
    assert s.ramp_up == "10s"


def test_scenario_defaults():
    @scenario()
    class MyFlow(VUser):
        pass

    s = _scenarios[0]
    assert s.rps == 10
    assert s.duration == "1m"
    assert s.ramp_up == "10s"


def test_scenario_returns_class_unchanged():
    @scenario(rps=10)
    class MyFlow(VUser):
        pass

    assert MyFlow.__name__ == "MyFlow"
    assert issubclass(MyFlow, VUser)


def test_multiple_scenarios_registered():
    @scenario(rps=10)
    class FlowA(VUser):
        pass

    @scenario(rps=20)
    class FlowB(VUser):
        pass

    assert len(_scenarios) == 2
    assert {s.name for s in _scenarios} == {"FlowA", "FlowB"}


# ── @task decorator ───────────────────────────────────────────────────────────

def test_task_weight_collected():
    @scenario(rps=10)
    class MyFlow(VUser):
        @task(weight=3)
        def do_thing(self, client):
            pass

    assert len(_scenarios[0].tasks) == 1
    t = _scenarios[0].tasks[0]
    assert t.name == "do_thing"
    assert t.weight == 3


def test_task_default_weight():
    @scenario(rps=10)
    class MyFlow(VUser):
        @task()
        def do_thing(self, client):
            pass

    assert _scenarios[0].tasks[0].weight == 1


def test_multiple_tasks_collected():
    @scenario(rps=10)
    class MyFlow(VUser):
        @task(weight=2)
        def task_a(self, client):
            pass

        @task(weight=5)
        def task_b(self, client):
            pass

    weights = {t.name: t.weight for t in _scenarios[0].tasks}
    assert weights == {"task_a": 2, "task_b": 5}


def test_task_preserves_function_name():
    @scenario(rps=10)
    class MyFlow(VUser):
        @task(weight=1)
        def my_task(self, client):
            pass

    assert MyFlow.my_task.__name__ == "my_task"


def test_non_task_methods_not_collected():
    @scenario(rps=10)
    class MyFlow(VUser):
        @task(weight=1)
        def do_thing(self, client):
            pass

        def helper(self):
            pass

    assert len(_scenarios[0].tasks) == 1


# ── VUser base class ──────────────────────────────────────────────────────────

def test_vuser_on_start_is_noop():
    u = VUser()
    u.on_start(None)  # must not raise


def test_vuser_on_stop_is_noop():
    u = VUser()
    u.on_stop(None)  # must not raise
