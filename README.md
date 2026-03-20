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

### Prerequisites

- Python 3.12+
- Rust 1.75+ — install via [rustup](https://rustup.rs)
- `uv` (recommended) or `pip`

### 1. Build the Rust engine

```bash
cd engine
cargo build --release
```

### 2. Install the Python CLI

```bash
cd cli
uv pip install -e .
```

### 3. Write a scenario

```python
# scenarios/health.py
from loadpilot import VUser, scenario, task

@scenario(rps=30, duration="2m", ramp_up="15s")
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

### 4. Run

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

@scenario(rps=100, duration="2m", ramp_up="30s")
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
        """Called by the Rust engine after each browse HTTP response.
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

### Assertions — `check_{task_name}`

Define an optional `check_{task_name}(self, response)` method alongside any task.
The Rust engine executes the HTTP request, then calls the check method with the
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

| Parameter  | Type  | Default | Description                       |
|------------|-------|---------|-----------------------------------|
| `rps`      | `int` | `10`    | Target requests per second        |
| `duration` | `str` | `"1m"`  | Total run time (`"30s"`, `"2m"`)  |
| `ramp_up`  | `str` | `"10s"` | Time to reach target RPS from 0   |

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

| Flag         | Default                   | Description                              |
|--------------|---------------------------|------------------------------------------|
| `--target`   | `http://localhost:8000`   | Base URL of the system under test        |
| `--report`   | off                       | Write HTML report to this path           |
| `--dry-run`  | off                       | Print the generated plan JSON and exit   |
| `--agents`   | `1`                       | Number of agent processes (MVP: 1)       |

---

## HTML Report

Pass `--report report.html` to generate a self-contained HTML report after the test:

```bash
loadpilot run scenarios/health.py --target https://api.example.com --report report.html
```

The report includes:
- Summary cards: total requests, error rate, peak RPS, duration
- Latency table: p50 / p95 / p99 / max / min / mean
- RPS chart: actual vs target over time
- Latency chart: p50 / p95 / p99 over time
- Test configuration

No external files required — open the `.html` file in any browser.

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
              Phase 1: prepare_task() — Python task runs with MockClient
                        GIL acquired → URL + method captured → GIL released
              Phase 2: reqwest HTTP — no GIL held
              Phase 3: run_check() — GIL acquired → check_{task}(response)
                        AssertionError → error in metrics → GIL released
        │
        ├── stdout (JSON lines, 1/sec) → CLI renders live terminal dashboard
        └── :9090/metrics              → Prometheus / Grafana
```

### PyO3 bridge

The bridge is the key architectural decision. Python tasks run to *capture intent*
(which URL, which method, which headers) via a `MockClient` that intercepts the
first HTTP call. Rust executes the real HTTP via `reqwest` without holding the
Python GIL. After the response arrives, the real status + headers + body are
passed back to Python for assertion checking.

This keeps the hot I/O path in Rust while preserving full Python expressiveness
for scenario logic and assertions.

---

## Comparison with Locust / k6 / Gatling

| | Gatling | k6 | Locust | **LoadPilot** |
|---|---|---|---|---|
| **Language** | Scala | JavaScript | Python | **Python** |
| **HTTP engine** | Netty (async Java) | Go runtime | Python (gevent) | **Rust / reqwest** |
| **GIL problem** | — | — | Yes | **Partial*** |
| **Assertions on response body** | ✅ | ✅ | ✅ | ✅ |
| **HTML report** | ✅ | ✅ | ✅ | ✅ |
| **Prometheus metrics** | ✅ | ✅ | ✅ | ✅ |
| **Distributed (free)** | ❌ paid | ❌ paid | ✅ | planned |
| **Thresholds / CI fail** | ✅ | ✅ | ❌ | planned |
| **Web UI** | ✅ | ✅ | ✅ | ❌ (Grafana) |

*GIL is held only during Python callbacks. The HTTP I/O path runs without GIL.

**Where LoadPilot wins today:**
- Against Locust — HTTP throughput. Locust hits GIL wall at ~5k RPS.
  LoadPilot's reqwest async keeps going.
- Against k6 / Gatling — same Python ergonomics your team already knows.
  No JavaScript, no Scala, no JVM.

**Where LoadPilot is not yet competitive:**
- Multiple HTTP calls per task (login → get → post in one scenario step)
- Thresholds (fail CI when p99 > 500ms)
- Distributed mode (currently single machine only)
- `pip install` without `cargo build`

---

## Path to Beating Gatling / k6 on RPS

Current bottlenecks in order of impact:

**1. Body reading in static mode** — `execute_request` always calls
`resp.text().await` even when there is no `check_*` method. Unnecessary
allocation on every request. Fix: skip body read when no PyO3 bridge.

**2. GIL acquisition per request** — in PyO3 mode, two GIL acquisitions happen
per HTTP request (`prepare_task` + `run_check`). At 10k RPS that is 20k
GIL acquisitions per second — a hard ceiling.

Fix: batch `prepare_task` calls — acquire the GIL once, prepare N requests,
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

**Benchmark plan:**
1. Axum mock HTTP server (localhost, not the bottleneck)
2. `hey` or `wrk` as the absolute ceiling baseline
3. LoadPilot static mode vs k6 vs Locust — same scenario, same target
4. Publish results

---

## Distributed Mode (planned v0.4)

The design goal: launch a multi-machine load test with one command, with zero
infrastructure beyond the machines themselves.

### Architecture

```
CLI (Python)
  └── publishes ScenarioPlan → NATS
          │
          ├── Coordinator (Rust)
          │     ├── receives plan
          │     ├── splits into shards (rps / N agents)
          │     ├── publishes shard → each agent
          │     └── aggregates metrics from all agents → CLI stdout
          │
          ├── Agent 1 (Rust)  — executes its RPS slice, reports metrics
          ├── Agent 2 (Rust)
          └── Agent N (Rust)
```

NATS subject schema:

```
loadpilot.plan.{run_id}        CLI → Coordinator     (publish once)
loadpilot.shard.{agent_id}     Coordinator → Agent   (per agent)
loadpilot.metrics.{run_id}.*   Agent → Coordinator   (1/sec stream)
loadpilot.control.{run_id}     Coordinator → Agents  (stop / pause)
```

Agents are **stateless and fungible** — they self-register on startup and the
coordinator distributes load automatically. Add or remove agents mid-test;
the coordinator rebalances.

### Installing agents on remote machines

The agent is a standalone Rust binary — no Python, no Docker required.

**Option 1 — install script (like rustup / Tailscale):**

```bash
curl -fsSL https://get.loadpilot.dev | sh
loadpilot-agent install --coordinator 10.0.1.1:4222 --daemon
# registers as a systemd / launchd service, starts on boot
```

**Option 2 — uvx (npx equivalent for Python):**

```bash
uvx loadpilot agent --coordinator 10.0.1.1:4222
```

No install step. `uvx` downloads from PyPI into an isolated env and runs.

**Option 3 — GitHub release binary:**

```bash
gh release download v0.4.0 --pattern "loadpilot-agent-linux-x86_64"
chmod +x loadpilot-agent
./loadpilot-agent --coordinator 10.0.1.1:4222
```

### Setting up a persistent load test fleet

```bash
# One-time setup — provision 3 agent machines:
for host in 10.0.1.10 10.0.1.11 10.0.1.12; do
  ssh $host "curl -fsSL https://get.loadpilot.dev | sh && \
             loadpilot-agent install --coordinator 10.0.1.1:4222 --daemon"
done

# Every test run — from any laptop:
loadpilot run scenarios/checkout.py \
  --nats nats://10.0.1.1:4222 \
  --target https://staging.api.example.com \
  --report report.html
```

The CLI output is identical to single-machine mode — the coordinator aggregates
all agent metrics into a single stream before forwarding to the CLI.

### Embedded NATS — no separate broker needed

For teams without a dedicated NATS server, the coordinator can run an embedded
NATS node. Agents connect directly to the coordinator's IP:

```bash
# Coordinator machine (also runs embedded NATS):
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --listen 0.0.0.0:4222

# Agent machines:
loadpilot-agent --coordinator 10.0.1.1:4222
```

One flag — no separate process.

---

## Roadmap

| Version | Feature | Status |
|---------|---------|--------|
| v0.1 | Python DSL + Rust coordinator, terminal dashboard, Prometheus | ✅ done |
| v0.2 | PyO3 bridge — on_start / on_stop / assertions on response body | ✅ done |
| v0.2 | HTML report | ✅ done |
| v0.3 | Multiple HTTP calls per task | next |
| v0.3 | Thresholds — fail with exit code 1 on SLA breach | next |
| v0.3 | `pip install loadpilot` — prebuilt coordinator binaries | next |
| v0.4 | Distributed mode — NATS-based coordinator + remote agents | planned |
| v0.4 | Agent install script (`curl \| sh`) + embedded NATS | planned |
| v0.5 | Spike / step / constant load profiles | planned |
| v1.0 | Benchmark showing 5× Locust throughput | planned |

**Removed from roadmap:** built-in Web UI — Grafana covers this better.

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
1. `pip install loadpilot` without requiring `cargo build` (prebuilt binaries)
2. Multiple HTTP calls per task — unlocks real-world scenarios
3. Thresholds — makes LoadPilot usable in CI pipelines
4. Publish the Locust benchmark — the only concrete proof of the value proposition

The project is not trying to compete with k6 or Gatling on features.
It is trying to be the obvious choice for Python teams that have outgrown Locust.

---

## License

MIT
