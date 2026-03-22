"""Unit tests for LoadClient and ResponseWrapper."""

from __future__ import annotations

import socket
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

from loadpilot.client import LoadClient, ResponseWrapper


# ── Minimal mock server ────────────────────────────────────────────────────────


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self._respond()

    do_POST = do_PUT = do_PATCH = do_DELETE = do_GET

    def _respond(self):
        method = self.command
        body = f'{{"method":"{method}","path":"{self.path}"}}'.encode()
        if self.path == "/error":
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"error")
        elif self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/")
            self.end_headers()
        else:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    def log_message(self, *args):
        pass


class _MockServer:
    def __init__(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self._server = HTTPServer(("127.0.0.1", self.port), _Handler)
        threading.Thread(target=self._server.serve_forever, daemon=True).start()

    def shutdown(self):
        self._server.shutdown()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"


@pytest.fixture(scope="module")
def server():
    s = _MockServer()
    yield s
    s.shutdown()


@pytest.fixture
def client(server):
    with LoadClient(server.url) as c:
        yield c


# ── ResponseWrapper ────────────────────────────────────────────────────────────


def test_response_wrapper_status_code(client):
    resp = client.get("/")
    assert resp.status_code == 200


def test_response_wrapper_ok_true(client):
    resp = client.get("/")
    assert resp.ok is True


def test_response_wrapper_ok_false(client):
    resp = client.get("/error")
    assert resp.ok is False


def test_response_wrapper_json(client):
    resp = client.get("/")
    data = resp.json()
    assert data["method"] == "GET"


def test_response_wrapper_text(client):
    resp = client.get("/")
    assert '"method"' in resp.text


def test_response_wrapper_elapsed_ms(client):
    resp = client.get("/")
    assert resp.elapsed_ms >= 0


def test_response_wrapper_headers(client):
    resp = client.get("/")
    assert "content-type" in resp.headers


def test_response_wrapper_raise_for_status_ok(client):
    resp = client.get("/")
    resp.raise_for_status()  # should not raise


def test_response_wrapper_raise_for_status_error(client):
    import httpx

    resp = client.get("/error")
    with pytest.raises(httpx.HTTPStatusError):
        resp.raise_for_status()


# ── LoadClient HTTP methods ────────────────────────────────────────────────────


def test_get(client):
    resp = client.get("/")
    assert resp.status_code == 200
    assert resp.json()["method"] == "GET"


def test_post(client):
    resp = client.post("/", json={"x": 1})
    assert resp.status_code == 200
    assert resp.json()["method"] == "POST"


def test_put(client):
    resp = client.put("/")
    assert resp.status_code == 200
    assert resp.json()["method"] == "PUT"


def test_patch(client):
    resp = client.patch("/")
    assert resp.status_code == 200
    assert resp.json()["method"] == "PATCH"


def test_delete(client):
    resp = client.delete("/")
    assert resp.status_code == 200
    assert resp.json()["method"] == "DELETE"


# ── LoadClient internals ───────────────────────────────────────────────────────


def test_path_without_leading_slash(client):
    resp = client.get("health")
    assert resp.status_code == 200


def test_last_response_updated(client):
    client.get("/")
    assert client._last_response is not None
    assert client._last_response.status_code == 200


def test_calls_recorded(server):
    with LoadClient(server.url) as c:
        c.get("/")
        c.post("/")
        assert len(c._calls) == 2
        for elapsed_ms, status_code in c._calls:
            assert elapsed_ms >= 0
            assert status_code == 200


def test_base_url_trailing_slash_stripped(server):
    with LoadClient(server.url + "/") as c:
        assert not c.base_url.endswith("/")


def test_follow_redirects(client):
    resp = client.get("/redirect")
    assert resp.status_code == 200


def test_context_manager_closes(server):
    with LoadClient(server.url) as c:
        c.get("/")
    assert c._client.is_closed
