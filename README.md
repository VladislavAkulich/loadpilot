# LoadPilot

> Python DSL for writing load test scenarios. Rust engine for executing them.

Write your tests in Python — get near-native HTTP performance.

```
LoadPilot — HealthCheckFlow  [02:15]  done: 30/30 RPS

  Requests/sec:      30.0      Total:     3,825
  Errors:             0.0%     Failed:         0

  Latency:
    p50:   31ms
    p95:   39ms
    p99:   71ms
    max:  382ms

  [████████████████████] 100%
```

---

## Why LoadPilot

The existing tools each have a real problem:

| Tool | Problem |
|---|---|
| **Locust** | Python GIL limits single-machine throughput to ~1–5k RPS |
| **k6** | JavaScript — context switch for Python teams |
| **Gatling** | Scala DSL — high barrier to entry |
| **k6 / Gatling** | Distributed mode is paid ($300–2000/month) |

LoadPilot's position: **Locust-compatible Python DSL + Rust HTTP engine + forever free**.

Same ergonomics as Locust. Meaningfully faster HTTP execution. No cloud lock-in.

---

## Quick Start

### Install

```bash
pip install loadpilot
```

No Rust, no `cargo build`. The coordinator binary is bundled inside the wheel —
pip picks the right one for your platform automatically.

### Write a scenario

```python
# scenarios/health.py
from loadpilot import VUser, scenario, task

@scenario(rps=30, duration="2m", ramp_up="15s",
          thresholds={"p99_ms": 500, "error_rate": 1.0})
class HealthCheckFlow(VUser):

    @task(weight=3)
    def health(self, client):
        client.get("/health")

    def check_health(self, response):
        assert response.status_code == 200

    @task(weight=1)
    def root(self, client):
        client.get("/")
```

### Run

```bash
loadpilot run scenarios/health.py \
  --target https://your-api.example.com \
  --report report.html
```

---

## Writing Scenarios

### Project structure

```
my-load-tests/
├── scenarios/
│   ├── health_check_flow.py
│   ├── authenticated_read_flow.py
│   └── project_crud_flow.py
├── helpers.py
└── README.md
```

### Full scenario anatomy

```python
from loadpilot import VUser, scenario, task, LoadClient

@scenario(
    rps=100,
    duration="2m",
    ramp_up="30s",
    thresholds={"p99_ms": 500, "p95_ms": 300, "error_rate": 1.0},
)
class CheckoutFlow(VUser):

    def on_start(self, client: LoadClient):
        """Runs once per virtual user before tasks start.
        Real HTTP — use for login, token retrieval, test data setup."""
        resp = client.post("/auth/login", json={
            "username": "testuser",
            "password": "secret",
        })
        self.token = resp.json()["access_token"]

    def on_stop(self, client: LoadClient):
        """Optional cleanup after the virtual user finishes."""
        client.post("/auth/logout", headers=self._auth())

    @task(weight=5)
    def browse(self, client: LoadClient):
        client.get("/api/products", headers=self._auth())

    def check_browse(self, response) -> None:
        """Called after each browse HTTP response.
        Receives the real response — assert freely."""
        assert response.status_code == 200
        assert isinstance(response.json(), list)

    @task(weight=1)
    def purchase(self, client: LoadClient):
        client.post("/api/orders",
            json={"product_id": 42, "qty": 1},
            headers=self._auth(),
        )

    def check_purchase(self, response) -> None:
        assert response.status_code in (200, 201)
        assert "id" in response.json()

    def _auth(self):
        return {"Authorization": f"Bearer {self.token}"}
```

### Multiple HTTP calls per task

A task can make any number of HTTP calls. Each call is recorded separately in
metrics (latency, success/fail). No extra annotations needed — it just works:

```python
@scenario(rps=50, duration="2m", ramp_up="15s")
class CheckoutFlow(VUser):

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={"username": "test", "password": "secret"})
        self.token = resp.json()["access_token"]

    @task(weight=1)
    def checkout(self, client: LoadClient):
        # Step 1 — get the cart
        cart = client.get("/cart", headers=self._auth())
        item_id = cart.json()["items"][0]["id"]

        # Step 2 — place the order using data from step 1
        client.post("/orders", json={"item_id": item_id, "qty": 1}, headers=self._auth())

    def check_checkout(self, response) -> None:
        # called with the response from the LAST HTTP call in the task
        assert response.status_code in (200, 201)
        assert "order_id" in response.json()

    def _auth(self):
        return {"Authorization": f"Bearer {self.token}"}
```

Each HTTP call inside `checkout` is measured and counted independently. The
`check_checkout` method receives the response from the last call in the task.

### Assertions — `check_{task_name}`

Define an optional `check_{task_name}(self, response)` method alongside any task.
The engine executes the HTTP request(s), then calls the check method with the
real response. Any exception (including `AssertionError`) is counted as an error
in metrics.

```python
@task(weight=1)
def get_user(self, client):
    client.get(f"/users/{self.user_id}")

def check_get_user(self, response) -> None:
    assert response.status_code == 200
    data = response.json()
    assert data["id"] == self.user_id
    assert "email" in data
```

`response` exposes: `.status_code`, `.ok`, `.text`, `.headers`, `.json()`, `.raise_for_status()`.

If no check method is defined — the request is counted as an error only when
the HTTP status is 4xx or 5xx.

### `weight` — task frequency

`weight` controls the relative frequency of task selection within one scheduler
cycle. It is deterministic, not random:

```
tasks: health(weight=3), root(weight=1)  →  total_weight=4

index % 4:  0 → health
            1 → health
            2 → health
            3 → root
```

At 100 RPS with weights 70/20/10 you get exactly 70/20/10 RPS per task.
Model your real traffic distribution directly.

---

## DSL Reference

### `@scenario`

| Parameter    | Type               | Default | Description                                      |
|--------------|--------------------|---------|--------------------------------------------------|
| `rps`        | `int`              | `10`    | Target requests per second                       |
| `duration`   | `str`              | `"1m"`  | Total run time (`"30s"`, `"2m"`)                 |
| `ramp_up`    | `str`              | `"10s"` | Time to reach target RPS from 0                  |
| `thresholds` | `dict[str, float]` | `{}`    | SLA limits — test fails with exit code 1 if breached |

Supported threshold keys: `p50_ms`, `p95_ms`, `p99_ms`, `max_ms`, `error_rate` (percent).

```python
@scenario(
    rps=50,
    duration="2m",
    thresholds={"p99_ms": 500, "error_rate": 1.0},
)
```

### `@task`

| Parameter | Type  | Default | Description                               |
|-----------|-------|---------|-------------------------------------------|
| `weight`  | `int` | `1`     | Relative frequency vs other tasks         |

### Lifecycle hooks

| Method                    | When                                          | Client |
|---------------------------|-----------------------------------------------|--------|
| `on_start(self, client)`  | Once per VUser, before tasks begin            | Real HTTP (httpx) |
| `on_stop(self, client)`   | Once per VUser, after test ends               | Real HTTP (httpx) |
| `check_{task}(self, resp)`| After each task's HTTP response is received   | — |

### `LoadClient`

Thin wrapper around [httpx](https://www.python-httpx.org/).

```python
client.get(path, **kwargs)     # → ResponseWrapper
client.post(path, **kwargs)
client.put(path, **kwargs)
client.patch(path, **kwargs)
client.delete(path, **kwargs)
```

`ResponseWrapper`: `.status_code`, `.ok`, `.text`, `.headers`, `.json()`, `.elapsed_ms`, `.raise_for_status()`.

---

## CLI Reference

```bash
loadpilot run <scenario.py> [OPTIONS]
```

| Flag                | Default                   | Description                                                    |
|---------------------|---------------------------|----------------------------------------------------------------|
| `--target`          | `http://localhost:8000`   | Base URL of the system under test                              |
| `--report`          | off                       | Write HTML report to this path                                 |
| `--dry-run`         | off                       | Print the generated plan JSON and exit                         |
| `--agents`          | `1`                       | Spawn N local agent processes (embedded NATS)                  |
| `--external-agents` | `0`                       | Wait for N externally started agents (embedded NATS, no spawn) |
| `--nats-url`        | off                       | Connect to external NATS. Use with `--external-agents`         |
| `--threshold`       | from `@scenario`          | Override SLA threshold: `--threshold p99_ms=500`               |

`--threshold` can be repeated and overrides values set in `@scenario(thresholds=...)`.

---

## SLA Thresholds

Thresholds let you fail a CI pipeline automatically when performance degrades.

```python
@scenario(
    rps=100,
    duration="2m",
    thresholds={
        "p99_ms":     500,   # p99 latency must be < 500ms
        "p95_ms":     300,   # p95 latency must be < 300ms
        "error_rate": 1.0,   # error rate must be < 1%
    },
)
```

After the test, LoadPilot prints a threshold report:

```
Thresholds
  ✓  p99 latency       243.0ms  <  500.0ms
  ✓  p95 latency       158.0ms  <  300.0ms
  ✓  error rate          0.0%   <    1.0%

All thresholds passed.
```

Exit code is `1` if any threshold is breached — plug directly into GitHub Actions,
Jenkins, or any other CI system.

Override thresholds from the command line without editing the scenario file:

```bash
loadpilot run scenarios/health.py \
  --target https://staging.api.example.com \
  --threshold p99_ms=800 \
  --threshold error_rate=2
```

---

## HTML Report

Pass `--report report.html` to generate a self-contained HTML report after the test:

```bash
loadpilot run scenarios/health.py --target https://api.example.com --report report.html
```

The report includes:
- Summary cards: total requests, error rate, peak RPS, duration
- **SLA thresholds**: each threshold shown as ✓ / ✗ with actual vs limit
- Latency table: p50 / p95 / p99 / max / min / mean
- RPS chart: actual vs target over time
- Latency chart: p50 / p95 / p99 over time
- Test configuration — including agent count in distributed mode

No external files required — open the `.html` file in any browser.

---

## Distributed Mode

Run a load test across multiple machines with one command. The coordinator
manages NATS, distributes plan shards, aggregates metrics — the CLI output
is identical to single-machine mode.

### Local agents (same machine)

Useful for testing distributed mode without extra infrastructure:

```bash
loadpilot run scenarios/health.py \
  --target https://api.example.com \
  --agents 4
```

Spawns 4 agent processes locally, each handling `rps / 4`.

### External agents (separate machines or Railway)

**Step 1 — install the agent on each machine:**

```bash
curl -fsSL https://raw.githubusercontent.com/VladislavAkulich/loadpilot/main/install.sh | sh
```

Installs `loadpilot-agent` to `/usr/local/bin`. Supports Linux x86_64/aarch64 and macOS x86_64/arm64.

**Step 2 — start the agents** (they wait for a plan, then reconnect automatically):

```bash
# Machine 1
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-0

# Machine 2
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-1
```

**Step 3 — run the test** (coordinator uses embedded NATS, agents connect to it):

```bash
loadpilot run scenarios/health.py \
  --target https://api.example.com \
  --external-agents 2
```

### Railway / external NATS

For cloud deployments, point both coordinator and agents at an external NATS server:

```bash
# Deploy NATS on Railway (Docker image: nats:latest)
# Deploy agent services with Dockerfile.agent, set env vars:
#   COORDINATOR=<nats-tcp-address>
#   AGENT_ID=agent-0  (agent-1 for the second service)

# Run locally — coordinator connects to Railway NATS, waits for 2 remote agents:
loadpilot run scenarios/health.py \
  --target https://api.example.com \
  --nats-url nats.railway.internal:4222 \
  --external-agents 2 \
  --report report.html
```

Agents are persistent — after completing a run they reconnect to NATS and wait
for the next plan. Prometheus metrics are exposed on `localhost:9090` by the
coordinator as usual.

### Architecture

```
CLI (Python)
  build plan → spawn coordinator
        │
        ▼ stdin (JSON)
Coordinator (Rust)
  ├── embedded NATS broker  (or connect to external NATS)
  ├── wait for N agents to register
  ├── shard plan → publish to each agent
  ├── aggregate metrics from all agents (sum RPS, weighted latency)
  ├── stdout (JSON lines, 1/sec) → CLI live dashboard
  └── :9090/metrics → Prometheus / Grafana

Agent (Rust, one per machine)
  ├── connect to NATS
  ├── register → receive shard
  ├── run HTTP load (token-bucket scheduler + reqwest)
  ├── stream metrics → NATS → coordinator
  └── reconnect and wait for next plan
```

---

## Metrics

Prometheus metrics are exposed on port **9090** while a test is running:

```bash
curl http://localhost:9090/metrics
```

Point a Prometheus scrape job at `host:9090` for live Grafana dashboards.

**On Grafana vs a built-in Web UI:**
LoadPilot deliberately has no browser-based control UI. Grafana gives you
real-time correlation of load test metrics with your service's own metrics
(CPU, DB latency, error rates) — which a standalone UI can never provide.
The HTML report covers the "share results with stakeholders" use case.

---

## How It Works

```
CLI (Python)
  load scenario file
  introspect @scenario classes
  pre-run each @task with MockClient → extract URL + method
  detect on_start / on_stop / check_* → enable PyO3 bridge
  build JSON plan → spawn coordinator binary
        │
        ▼ stdin (JSON)
Coordinator (Rust / tokio)
  token-bucket scheduler (50ms ticks)
        │
        ├── Static mode (no Python callbacks)
        │     reqwest async HTTP → record success/error
        │
        └── PyO3 mode (on_start / on_stop / check_* present)
              RustClient (PyO3 pyclass) passed to Python task
              py.allow_threads(|| reqwest HTTP) — GIL released for I/O
              GIL re-acquired only for Python assertion check
              Each HTTP call recorded as separate metric
        │
        ├── stdout (JSON lines, 1/sec) → CLI renders live terminal dashboard
        └── :9090/metrics              → Prometheus / Grafana
```

### PyO3 bridge

The bridge is the key architectural decision. In PyO3 mode a `RustClient` is
passed directly to the Python task. The task calls `.get()` / `.post()` etc. —
each call releases the GIL via `py.allow_threads`, executes the HTTP request
via `reqwest`, and re-acquires the GIL only to run the `check_{task}` assertion.

This means:
- **Multiple HTTP calls per task** work natively — no annotations needed
- **GIL is held only for Python code** — the hot I/O path runs without it
- **Each call is recorded independently** — full per-call latency in metrics

---

## Comparison with Locust / k6 / Gatling

| | Gatling | k6 | Locust | **LoadPilot** |
|---|---|---|---|---|
| **Language** | Scala | JavaScript | Python | **Python** |
| **HTTP engine** | Netty (async Java) | Go runtime | Python (gevent) | **Rust / reqwest** |
| **GIL problem** | — | — | Yes | **Partial*** |
| **Multiple calls per task** | ✅ | ✅ | ✅ | ✅ |
| **Assertions on response body** | ✅ | ✅ | ✅ | ✅ |
| **Thresholds / CI fail** | ✅ | ✅ | ❌ | ✅ |
| **HTML report** | ✅ | ✅ | ✅ | ✅ |
| **Prometheus metrics** | ✅ | ✅ | ✅ | ✅ |
| **`pip install` (no build step)** | ✅ | ✅ | ✅ | ✅ |
| **Distributed (free)** | ❌ paid | ❌ paid | ✅ | ✅ |
| **Web UI** | ✅ | ✅ | ✅ | ❌ (Grafana) |

*GIL is held only during Python callbacks. The HTTP I/O path runs without GIL.

**Where LoadPilot wins today:**
- Against Locust — HTTP throughput. Locust hits GIL wall at ~5k RPS.
  LoadPilot's reqwest async keeps going.
- Against k6 / Gatling — same Python ergonomics your team already knows.
  No JavaScript, no Scala, no JVM.
- Against all — thresholds with exit code 1, free distributed mode.

**Where LoadPilot is not yet competitive:**
- Web UI (by design — use Grafana)

---

## Path to Beating Gatling / k6 on RPS

Current bottlenecks in order of impact:

**1. Body reading in static mode** — `execute_request` always calls
`resp.text().await` even when there is no `check_*` method. Unnecessary
allocation on every request. Fix: skip body read when no PyO3 bridge.

**2. GIL acquisition per request** — in PyO3 mode, one GIL acquisition happens
per HTTP request (for `check_{task}`). At 10k RPS that is 10k GIL acquisitions
per second — a hard ceiling.

Fix: batch `check_{task}` calls — acquire the GIL once, check N responses,
release. One GIL acquisition per 50 requests instead of per 1.

**3. Python 3.13 free-threaded mode** — Python 3.13 ships an experimental
no-GIL build (`python3.13t`). PyO3 0.23 supports it. With free threading,
each tokio worker thread runs Python independently — the GIL bottleneck
disappears entirely.

**Estimated RPS ceiling after each fix:**

| Mode | Now | After body fix | After batch GIL | After nogil |
|---|---|---|---|---|
| Static | ~20k* | ~50k+* | ~50k+* | ~50k+* |
| PyO3 | ~5–10k* | — | ~20–30k* | ~50k+* |

*architecture estimate, not yet benchmarked

---

## Roadmap

| Version | Feature | Status |
|---------|---------|--------|
| v0.1 | Python DSL + Rust coordinator, terminal dashboard, Prometheus | ✅ done |
| v0.2 | PyO3 bridge — on_start / on_stop / assertions on response body | ✅ done |
| v0.2 | HTML report | ✅ done |
| v0.3 | Multiple HTTP calls per task | ✅ done |
| v0.3 | Thresholds — fail with exit code 1 on SLA breach | ✅ done |
| v0.3 | `pip install loadpilot` — prebuilt coordinator binaries | ✅ done |
| v0.4 | Distributed mode — embedded NATS + local agents (`--agents N`) | ✅ done |
| v0.4 | External agents — Railway / remote machines (`--nats-url`) | ✅ done |
| v0.4 | Agent install script (`curl \| sh`) | ✅ done |
| v0.5 | Spike / step / constant load profiles | planned |
| v1.0 | Benchmark showing 5× Locust throughput | planned |

**Removed from roadmap:** built-in Web UI — Grafana covers this better.

---

## Building from Source

If you want to contribute or build the coordinator yourself:

```bash
# Prerequisites: Python 3.12+, Rust 1.85+, uv

git clone https://github.com/VladislavAkulich/loadpilot.git
cd loadpilot

# Build Rust coordinator + agent
cd engine && cargo build --release && cd ..

# Install Python CLI in editable mode
cd cli && uv pip install -e .
```

---

## Testing

```bash
# Python unit + integration tests
cd cli
uv pip install pytest
pytest tests/ -v

# Integration tests require the coordinator binary
# (skipped automatically if not built)

# Rust unit tests
cd engine
cargo test
```

Test coverage:
- `test_models.py` — `parse_duration`, `ScenarioPlan`, `TaskPlan`
- `test_dsl.py` — `@scenario` / `@task` decorators, VUser lifecycle
- `test_bridge.py` — `MockClient`, `MockResponse`, `CheckResponse`
- `test_report.py` — HTML report generation
- `test_integration.py` — CLI → coordinator subprocess protocol (static + PyO3 mode)
- `metrics.rs` — `LatencyHistogram`, `Metrics` counters
- `plan.rs` — serde deserialization
- `coordinator.rs` — `pick_task`, `compute_target_rps`

---

## Open Source Strategy

LoadPilot occupies a specific gap: **Python load testing that does not hit
the Locust performance ceiling, with distributed mode that is not paywalled**.

k6 cloud and Gatling Enterprise charge $300–2000/month for distributed load
testing. LoadPilot's distributed mode will always be free.

For traction on GitHub the priority order is:
1. Publish the Locust benchmark — the only concrete proof of the value proposition
2. Spike / step load profiles — covers more CI use cases

---

## License

MIT
