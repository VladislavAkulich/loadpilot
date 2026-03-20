import time
from typing import Any

import httpx


class ResponseWrapper:
    """Thin wrapper around httpx.Response with timing information."""

    def __init__(self, response: httpx.Response, elapsed_ms: float):
        self._response = response
        self.elapsed_ms = elapsed_ms
        self.status_code = response.status_code
        self.headers = response.headers
        self.text = response.text

    def json(self) -> Any:
        return self._response.json()

    def raise_for_status(self) -> None:
        self._response.raise_for_status()

    @property
    def ok(self) -> bool:
        return self._response.is_success


class LoadClient:
    """HTTP client wrapper around httpx with timing capture.

    Used by VUser task methods to make HTTP requests. All requests are
    automatically timed and the last response is available via _last_response.
    """

    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self._last_response: ResponseWrapper | None = None
        self._client = httpx.Client(
            base_url=self.base_url,
            timeout=httpx.Timeout(30.0),
            follow_redirects=True,
        )

    def _request(self, method: str, path: str, **kwargs: Any) -> ResponseWrapper:
        if not path.startswith("/"):
            path = "/" + path
        start = time.perf_counter()
        response = self._client.request(method, path, **kwargs)
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        wrapped = ResponseWrapper(response, elapsed_ms)
        self._last_response = wrapped
        return wrapped

    def get(self, path: str, **kwargs: Any) -> ResponseWrapper:
        return self._request("GET", path, **kwargs)

    def post(self, path: str, **kwargs: Any) -> ResponseWrapper:
        return self._request("POST", path, **kwargs)

    def put(self, path: str, **kwargs: Any) -> ResponseWrapper:
        return self._request("PUT", path, **kwargs)

    def patch(self, path: str, **kwargs: Any) -> ResponseWrapper:
        return self._request("PATCH", path, **kwargs)

    def delete(self, path: str, **kwargs: Any) -> ResponseWrapper:
        return self._request("DELETE", path, **kwargs)

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> "LoadClient":
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()
