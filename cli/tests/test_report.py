import json
import tempfile
from pathlib import Path

import pytest

from loadpilot.models import AgentMetrics, LatencyStats
from loadpilot import report as _report


def _snap(elapsed: float, rps: float, errors: int = 0, phase: str = "steady") -> AgentMetrics:
    total = max(1, int(rps * elapsed))
    return AgentMetrics(
        timestamp_secs=1_700_000_000.0 + elapsed,
        elapsed_secs=elapsed,
        current_rps=rps,
        target_rps=10.0,
        requests_total=total,
        errors_total=errors,
        active_workers=2,
        latency=LatencyStats(p50_ms=5.0, p95_ms=15.0, p99_ms=30.0, max_ms=50.0, min_ms=1.0, mean_ms=7.0),
        phase=phase,
    )


SNAPSHOTS = [
    _snap(1.0, 3.0, phase="ramp_up"),
    _snap(2.0, 7.0, phase="ramp_up"),
    _snap(3.0, 10.0),
    _snap(4.0, 10.0, errors=1),
    _snap(5.0, 0.0, errors=1, phase="done"),
]


# ── _fmt_dur ──────────────────────────────────────────────────────────────────

def test_fmt_dur_seconds():
    assert _report._fmt_dur(45) == "45s"


def test_fmt_dur_minutes():
    assert _report._fmt_dur(120) == "2m"


def test_fmt_dur_minutes_and_seconds():
    assert _report._fmt_dur(150) == "2m 30s"


def test_fmt_dur_hours():
    assert _report._fmt_dur(3661) == "1h 1m 1s"


# ── _build ────────────────────────────────────────────────────────────────────

def test_build_returns_html_string():
    html = _report._build(SNAPSHOTS, "MyScenario", "http://localhost", 10, 60, 10)
    assert html.startswith("<!DOCTYPE html>")
    assert "</html>" in html


def test_build_contains_scenario_name():
    html = _report._build(SNAPSHOTS, "MyScenario", "http://localhost", 10, 60, 10)
    assert "MyScenario" in html


def test_build_contains_target_url():
    html = _report._build(SNAPSHOTS, "S", "http://api.example.com", 10, 60, 10)
    assert "http://api.example.com" in html


def test_build_embeds_chart_data_json():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10)
    # Extract the JSON blob from the script
    start = html.index("const D = ") + len("const D = ")
    end = html.index(";\n", start)
    data = json.loads(html[start:end])
    assert "elapsed" in data
    assert "actual_rps" in data
    assert "p50" in data
    assert "p99" in data


def test_build_excludes_done_snapshot_from_charts():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10)
    start = html.index("const D = ") + len("const D = ")
    end = html.index(";\n", start)
    data = json.loads(html[start:end])
    # done snapshot has current_rps=0.0; chart_snaps excludes it
    # So last actual_rps value should NOT be 0.0 (it's from the steady snapshots)
    assert data["actual_rps"][-1] != 0.0 or len(data["elapsed"]) < len(SNAPSHOTS)


def test_build_shows_correct_error_rate():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10)
    # final snapshot has errors_total=1, requests_total=50
    # error_rate < 5% → should use "ok" class, not "err"
    assert 'c-ok' in html


def test_build_shows_error_class_for_high_error_rate():
    bad_snaps = [
        _snap(1.0, 10.0, errors=0),
        _snap(2.0, 10.0, errors=10, phase="done"),
    ]
    html = _report._build(bad_snaps, "S", "http://localhost", 10, 60, 10)
    assert "c-err" in html


def test_build_with_empty_snapshots():
    html = _report._build([], "S", "http://localhost", 10, 60, 10)
    assert "<!DOCTYPE html>" in html


def test_build_contains_latency_values():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10)
    assert ">5<" in html   # p50 = 5ms
    assert ">30<" in html  # p99 = 30ms


def test_build_contains_chartjs_script():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10)
    assert "chart.js" in html
    assert "new Chart" in html


# ── generate ─────────────────────────────────────────────────────────────────

def test_generate_writes_file():
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "report.html"
        _report.generate(SNAPSHOTS, "MyScenario", "http://localhost", 10, 60, 10, out)
        assert out.exists()
        content = out.read_text(encoding="utf-8")
        assert "MyScenario" in content


def test_generate_file_is_valid_html():
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "report.html"
        _report.generate(SNAPSHOTS, "S", "http://localhost", 10, 60, 10, out)
        content = out.read_text()
        assert content.startswith("<!DOCTYPE html>")
        assert content.endswith("</html>")


# ── n_agents ──────────────────────────────────────────────────────────────────

def test_build_single_agent_no_distributed_badge():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10, n_agents=1)
    assert "distributed" not in html


def test_build_distributed_shows_agent_count():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10, n_agents=3)
    assert "distributed" in html
    assert "3" in html


def test_build_distributed_badge_absent_for_single():
    html = _report._build(SNAPSHOTS, "S", "http://localhost", 10, 60, 10, n_agents=1)
    assert "agents" not in html


def test_generate_passes_n_agents():
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "report.html"
        _report.generate(SNAPSHOTS, "S", "http://localhost", 10, 60, 10, out, n_agents=4)
        content = out.read_text()
        assert "distributed" in content
        assert "4" in content
