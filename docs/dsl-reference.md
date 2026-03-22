---
title: DSL Reference
nav_order: 3
layout: default
---

# DSL Reference

## `@scenario`

| Parameter    | Type               | Default    | Description |
|--------------|--------------------|------------|-------------|
| `rps`        | `int`              | `10`       | Target RPS at peak load |
| `duration`   | `str`              | `"1m"`     | Steady-state duration for `ramp`; total for other modes |
| `ramp_up`    | `str`              | `"10s"`    | Ramp-up window (used only by `mode="ramp"`) |
| `mode`       | `str`              | `"ramp"`   | Load profile: `ramp`, `constant`, `step`, `spike` |
| `steps`      | `int`              | `5`        | Number of steps for `mode="step"` |
| `thresholds` | `dict[str, float]` | `{}`       | SLA limits — exit code 1 if breached |

### Load profiles

| Mode       | Behaviour |
|------------|-----------|
| `ramp`     | Linear ramp 0 → target RPS over `ramp_up`, then steady. Total = `duration + ramp_up`. |
| `constant` | Full RPS immediately, no ramp. Total = `duration`. |
| `step`     | Divide `duration` into `steps` equal windows; RPS increases each step. |
| `spike`    | Thirds: 20% RPS (baseline) → 100% RPS (spike) → 20% RPS (recovery). |

```python
@scenario(rps=100, duration="2m", ramp_up="15s", mode="ramp")    # default
@scenario(rps=100, duration="2m", mode="constant")
@scenario(rps=100, duration="2m30s", mode="step", steps=5)
@scenario(rps=100, duration="2m", mode="spike")
```

All profiles work in distributed mode.

---

## `@task`

| Parameter | Type  | Default | Description |
|-----------|-------|---------|-------------|
| `weight`  | `int` | `1`     | Relative frequency vs other tasks |

Tasks with higher `weight` are called proportionally more often. A scenario with
`@task(weight=5) def browse` and `@task(weight=1) def purchase` will call `browse`
5 times for every 1 call to `purchase`.

Tasks can be `async def` — LoadPilot drives them with a `coro.send(None)` fast path
that avoids asyncio scheduling overhead for sync-body coroutines, with automatic
fallback to `run_until_complete` for tasks that contain real `await` expressions.

---

## Lifecycle hooks

| Method | When | Client |
|--------|------|--------|
| `on_start(self, client)` | Once per virtual user, before tasks start | Real HTTP (httpx) |
| `on_stop(self, client)` | Once per virtual user, after test ends | Real HTTP (httpx) |
| `check_{task}(self, status_code, body)` | After each task's last HTTP response | — |

### `on_start`

Runs once per virtual user before any tasks are dispatched. Use it for
authentication, session setup, or any per-user state.

```python
def on_start(self, client: LoadClient):
    resp = client.post("/auth/login", json={"username": "test", "password": "secret"})
    self.token = resp.json()["access_token"]
```

In distributed mode `on_start` runs on the coordinator, captures per-VUser headers,
and ships them with the plan. Agents rotate through pre-authenticated header sets in
pure Rust — no Python required on agents.

### `check_{task}`

Called after each invocation of the matching task, with the status code and parsed
JSON body of the last HTTP call made inside that task. Raise any exception to count
the request as an error.

```python
@task(weight=1)
def browse(self, client: LoadClient):
    client.get("/api/products", headers=self._auth())

def check_browse(self, status_code: int, body) -> None:
    assert status_code == 200
    assert isinstance(body, list)
```

If no `check_{task}` is defined, errors are determined by HTTP status code (≥ 400 = error).

> In distributed mode `check_*` is intentionally skipped — at high RPS the signal
> is status code, latency, and throughput, not body content.

---

## `LoadClient`

Thin wrapper around [httpx](https://www.python-httpx.org/).

```python
client.get(path, **kwargs)
client.post(path, **kwargs)
client.put(path, **kwargs)
client.patch(path, **kwargs)
client.delete(path, **kwargs)
```

All methods accept the same keyword arguments as httpx (`headers`, `json`, `data`,
`params`, `timeout`, etc.). `path` is relative to the `--target` base URL.

**`ResponseWrapper`** attributes: `.status_code`, `.ok`, `.text`, `.headers`,
`.json()`, `.elapsed_ms`, `.raise_for_status()`.

### `client.batch(requests)` — concurrent requests in one PyO3 call

Execute N HTTP requests concurrently inside Rust, releasing the GIL for the entire
batch. Useful when a task makes multiple independent requests and latency matters.

```python
@task(weight=1)
def fetch_profile(self, client: LoadClient):
    auth = {"Authorization": f"Bearer {self.token}"}
    responses = client.batch([
        {"method": "GET", "path": "/api/user",   "headers": auth},
        {"method": "GET", "path": "/api/orders", "headers": auth},
        {"method": "GET", "path": "/api/cart",   "headers": auth},
    ])
    # responses is a list of ResponseWrapper in dispatch order
```

Each dict accepts: `method` (default `"GET"`), `path`, `headers`, `json`, `data`.

At batch size 5 this reaches **97% of static-mode ceiling** (+45% vs sequential).
Useful when individual request latency is high enough that concurrency significantly
reduces total task time.

---

## Multiple tasks per scenario

```python
@scenario(rps=100, duration="2m")
class CheckoutFlow(VUser):

    @task(weight=5)
    def browse(self, client: LoadClient):
        client.get("/api/products", headers=self._auth())

    @task(weight=1)
    def purchase(self, client: LoadClient):
        client.post("/api/orders", json={"product_id": 42, "qty": 1},
                    headers=self._auth())
```

### Multiple HTTP calls inside a task

```python
@task(weight=1)
def checkout(self, client: LoadClient):
    cart = client.get("/cart", headers=self._auth())
    item_id = cart.json()["items"][0]["id"]
    client.post("/orders", json={"item_id": item_id, "qty": 1}, headers=self._auth())

def check_checkout(self, status_code: int, body) -> None:
    assert status_code in (200, 201)
```

Each HTTP call inside a task is measured independently. `check_checkout` receives
the status code and parsed JSON body of the **last** call.

---

## Multiple scenarios in one file

```python
@scenario(rps=30, duration="1m")
class LightFlow(VUser): ...

@scenario(rps=100, duration="2m", mode="spike")
class HeavyFlow(VUser): ...
```

```bash
loadpilot run scenarios/flows.py --scenario HeavyFlow --target https://api.example.com
# omit --scenario to pick interactively
```

---

## SLA thresholds

```python
@scenario(
    rps=100,
    duration="2m",
    thresholds={
        "p99_ms":     500,   # p99 latency must be < 500ms
        "p95_ms":     300,
        "error_rate": 1.0,   # error rate must be < 1%
    },
)
```

After the test:

```
Thresholds
  ✓  p99 latency       243ms  <  500ms
  ✓  p95 latency       158ms  <  300ms
  ✓  error rate          0%   <    1%

All thresholds passed.
```

Exit code `1` on breach. Override from CLI without editing the file:

```bash
loadpilot run scenarios/health.py \
  --target https://staging.api.example.com \
  --threshold p99_ms=800 \
  --threshold error_rate=2
```
