import json

import pytest

from loadpilot._bridge import CheckResponse, MockClient, MockResponse


# ── MockResponse ──────────────────────────────────────────────────────────────

def test_mock_response_defaults():
    r = MockResponse()
    assert r.status_code == 200
    assert r.ok is True
    assert r.text == ""
    assert r.json() == {}


def test_mock_response_raise_for_status_is_noop():
    MockResponse().raise_for_status()  # must not raise


# ── MockClient — capture ──────────────────────────────────────────────────────

def test_mock_client_captures_get():
    c = MockClient()
    c.get("/users", headers={"X-Token": "abc"})
    method, path, headers, body = c.get_call()
    assert method == "GET"
    assert path == "/users"
    assert headers == {"X-Token": "abc"}
    assert body is None


def test_mock_client_captures_post_with_json():
    c = MockClient()
    c.post("/items", json={"name": "foo"})
    method, path, _, body = c.get_call()
    assert method == "POST"
    assert path == "/items"
    assert json.loads(body) == {"name": "foo"}


def test_mock_client_ignores_subsequent_calls():
    c = MockClient()
    c.get("/first")
    c.post("/second")
    _, path, _, _ = c.get_call()
    assert path == "/first"


def test_mock_client_no_call_returns_nones():
    c = MockClient()
    method, path, headers, body = c.get_call()
    assert method is None
    assert path is None
    assert headers == {}
    assert body is None


def test_mock_client_returns_mock_response():
    c = MockClient()
    assert isinstance(c.get("/ping"), MockResponse)
    assert isinstance(c.post("/ping"), MockResponse)
    assert isinstance(c.put("/ping"), MockResponse)
    assert isinstance(c.patch("/ping"), MockResponse)
    assert isinstance(c.delete("/ping"), MockResponse)


@pytest.mark.parametrize("method_name,expected", [
    ("get", "GET"),
    ("post", "POST"),
    ("put", "PUT"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
])
def test_mock_client_all_methods_record_correct_verb(method_name, expected):
    c = MockClient()
    getattr(c, method_name)("/path")
    verb, _, _, _ = c.get_call()
    assert verb == expected


# ── CheckResponse ─────────────────────────────────────────────────────────────

def test_check_response_success():
    r = CheckResponse(200, {"content-type": "application/json"}, '{"status":"ok"}')
    assert r.status_code == 200
    assert r.ok is True
    assert r.json() == {"status": "ok"}
    assert r.text == '{"status":"ok"}'
    assert r.headers["content-type"] == "application/json"


def test_check_response_error_status():
    r = CheckResponse(404, {}, "not found")
    assert r.ok is False


def test_check_response_server_error():
    r = CheckResponse(500, {}, "internal error")
    assert r.ok is False


def test_check_response_raise_for_status_on_success():
    CheckResponse(200, {}, "ok").raise_for_status()  # must not raise


def test_check_response_raise_for_status_on_error():
    with pytest.raises(ValueError, match="HTTP error 404"):
        CheckResponse(404, {}, "not found").raise_for_status()


def test_check_response_empty_body_json():
    r = CheckResponse(200, {}, "")
    assert r.json() == {}


def test_check_response_assertion_pattern():
    """Simulates the check_{task_name} assertion pattern."""
    r = CheckResponse(200, {}, '{"id": 42, "name": "foo"}')
    assert r.status_code == 200
    data = r.json()
    assert data["id"] == 42
    assert "name" in data


def test_check_response_assertion_failure():
    r = CheckResponse(200, {}, '{"id": 99}')
    with pytest.raises(AssertionError):
        assert r.json()["id"] == 42
