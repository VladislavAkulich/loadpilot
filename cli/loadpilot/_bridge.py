"""loadpilot._bridge — MockClient for PyO3 callback interception.

The Rust coordinator calls Python task methods with a MockClient instead of a
real LoadClient. MockClient records the first HTTP call made by the task
(method, path, headers, body) and returns a dummy response. Rust then executes
the real HTTP request via reqwest.

For tasks that make more than one HTTP call, AllCallsMockClient is used by the
CLI at plan-build time to detect and flag them as multi_call=True.
"""
from __future__ import annotations

import json as _json
from typing import Any


class MockResponse:
    """Minimal response that keeps task code from crashing on attribute access."""

    status_code: int = 200
    ok: bool = True
    text: str = ""
    headers: dict = {}

    def json(self) -> Any:
        return {}

    def raise_for_status(self) -> None:
        pass  # Real error handling is done by Rust on the actual HTTP response.


class CheckResponse:
    """Real HTTP response passed to check_{task_name} methods by the Rust coordinator.

    Rust executes the actual HTTP request via reqwest and constructs this object
    from the real status code, headers, and body so that assertions can inspect
    the full response.
    """

    def __init__(self, status_code: int, headers: dict[str, str], body: str) -> None:
        self.status_code = status_code
        self.headers = headers
        self.text = body
        self.ok = status_code < 400
        self._body = body

    def json(self) -> Any:
        return _json.loads(self._body) if self._body else {}

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise ValueError(f"HTTP error {self.status_code}")


class MockClient:
    """
    Drop-in replacement for LoadClient used during task execution.

    Records the *first* HTTP call made by the task so Rust can execute it via
    reqwest. Subsequent calls within the same task invocation are silently
    ignored (limitation: tasks that make multiple requests map only the first).
    """

    def __init__(self) -> None:
        self._method: str | None = None
        self._path: str | None = None
        self._headers: dict[str, str] = {}
        self._body: str | None = None

    def _record(self, method: str, path: str, kwargs: dict) -> None:
        if self._method is not None:
            return  # Only the first call is captured.
        self._method = method
        self._path = path
        self._headers = dict(kwargs.get("headers") or {})
        if "json" in kwargs:
            self._body = _json.dumps(kwargs["json"])

    def get(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("GET", path, kwargs)
        return MockResponse()

    def post(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("POST", path, kwargs)
        return MockResponse()

    def put(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("PUT", path, kwargs)
        return MockResponse()

    def patch(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("PATCH", path, kwargs)
        return MockResponse()

    def delete(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("DELETE", path, kwargs)
        return MockResponse()

    def get_call(self) -> tuple[str | None, str | None, dict[str, str], str | None]:
        """Return (method, path, headers, body). Called by Rust via PyO3."""
        return self._method, self._path, self._headers, self._body


class AllCallsMockClient:
    """Like MockClient but records every HTTP call, not just the first.

    Used by the CLI at plan-build time to detect tasks that make multiple
    HTTP calls and flag them as multi_call=True in the TaskPlan.
    """

    def __init__(self) -> None:
        self._calls: list[tuple[str, str]] = []  # (method, path)
        self._headers: dict[str, str] = {}
        self._body: str | None = None

    def _record(self, method: str, path: str, kwargs: dict) -> None:
        self._calls.append((method, path))
        if len(self._calls) == 1:
            self._headers = dict(kwargs.get("headers") or {})
            if "json" in kwargs:
                self._body = _json.dumps(kwargs["json"])

    def get(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("GET", path, kwargs)
        return MockResponse()

    def post(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("POST", path, kwargs)
        return MockResponse()

    def put(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("PUT", path, kwargs)
        return MockResponse()

    def patch(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("PATCH", path, kwargs)
        return MockResponse()

    def delete(self, path: str, **kwargs: Any) -> MockResponse:
        self._record("DELETE", path, kwargs)
        return MockResponse()

    def call_count(self) -> int:
        return len(self._calls)

    def get_first_call(self) -> tuple[str | None, str | None, dict[str, str], str | None]:
        """Return (method, path, headers, body) of the first call, or Nones."""
        if not self._calls:
            return None, None, {}, None
        method, path = self._calls[0]
        return method, path, self._headers, self._body
