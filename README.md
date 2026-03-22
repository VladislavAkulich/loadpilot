# LoadPilot

> Python DSL for writing load test scenarios. Rust engine for executing them.

```
LoadPilot — HealthCheckFlow  [02:15]  steady: 30/30 RPS

  Requests/sec:      30.0      Total:     3,825
  Errors:             0.0%     Failed:         0

  Latency:
    p50:   31ms
    p95:   39ms
    p99:   71ms
    max:  382ms

  [████████████████████] 100%
```

Write your scenarios in Python. The HTTP engine runs in Rust.

---

## What it is

LoadPilot is a load testing tool for teams that want to write scenarios in Python
without the throughput ceiling of a pure-Python engine.

- Scenarios are plain Python classes — no new DSL to learn
- HTTP execution runs in async Rust via reqwest
- Distributed mode is built-in and free — run agents on any machine or cloud

→ [Getting started in 5 minutes](docs/getting-started.md)

---

## Features

- **Python DSL** — `@scenario`, `@task`, `on_start`, `check_*`
- **Load profiles** — ramp, constant, step, spike
- **Thresholds** — fail CI with exit code 1 on SLA breach
- **Distributed mode** — coordinator + N agents over NATS, free
- **HTML report** — self-contained, no server required
- **Prometheus metrics** — live Grafana dashboards while the test runs
- **Interactive TUI** — `loadpilot run` with no args opens a scenario browser
- **`pip install`** — coordinator binary bundled in the wheel, no Rust needed

---

## Quick Start

```bash
pip install loadpilot

# scaffold a project
loadpilot init my-load-tests
cd my-load-tests

# run interactively — pick scenario from TUI
loadpilot run --target https://your-api.example.com

# or run directly
loadpilot run scenarios/example.py \
  --target https://your-api.example.com \
  --report report.html
```

→ [Full getting started guide](docs/getting-started.md)

---

## Writing Scenarios

### Scenario anatomy

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
        """Runs once per virtual user before tasks start."""
        resp = client.post("/auth/login", json={"username": "test", "password": "secret"})
        self.token = resp.json()["access_token"]

    @task(weight=5)
    def browse(self, client: LoadClient):
        client.get("/api/products", headers=self._auth())

    def check_browse(self, status_code: int, body) -> None:
        assert status_code == 200
        assert isinstance(body, list)

    @task(weight=1)
    def purchase(self, client: LoadClient):
        client.post("/api/orders",
            json={"product_id": 42, "qty": 1},
            headers=self._auth(),
        )

    def check_purchase(self, status_code: int, body) -> None:
        assert status_code in (200, 201)
        assert "id" in body

    def _auth(self):
        return {"Authorization": f"Bearer {self.token}"}
```

### Multiple HTTP calls per task

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
the status code and parsed JSON body of the last call.

### Multiple scenarios in one file

```python
@scenario(rps=30, duration="1m")
class LightFlow(VUser): ...

@scenario(rps=100, duration="2m", mode="spike")
class HeavyFlow(VUser): ...
```

```bash
loadpilot run scenarios/flows.py --scenario HeavyFlow --target https://api.example.com
# or omit --scenario to pick interactively
```

---

## DSL Reference

### `@scenario`

| Parameter    | Type               | Default    | Description |
|--------------|--------------------|------------|-------------|
| `rps`        | `int`              | `10`       | Target RPS at peak load |
| `duration`   | `str`              | `"1m"`     | Steady-state duration for `ramp`; total for other modes |
| `ramp_up`    | `str`              | `"10s"`    | Ramp-up window (used only by `mode="ramp"`) |
| `mode`       | `str`              | `"ramp"`   | Load profile |
| `steps`      | `int`              | `5`        | Steps for `mode="step"` |
| `thresholds` | `dict[str, float]` | `{}`       | SLA limits — exit 1 if breached |

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

### `@task`

| Parameter | Type  | Default | Description |
|-----------|-------|---------|-------------|
| `weight`  | `int` | `1`     | Relative frequency vs other tasks |

### Lifecycle hooks

| Method                    | When | Client | Distributed |
|---------------------------|------|--------|-------------|
| `on_start(self, client)`  | Once per VUser, before tasks | Real HTTP (httpx) | ✅ pre-auth pool |
| `on_stop(self, client)`   | Once per VUser, after test   | Real HTTP (httpx) | ❌ skipped |
| `check_{task}(self, status_code, body)`| After each task's last HTTP response | — | ❌ status-based only |

Tasks can be `async def` — LoadPilot drives them via a `coro.send(None)` fast path
with automatic fallback to `asyncio.run_until_complete` for tasks with real `await`.

> In distributed mode `on_start` runs on the coordinator, captures per-VUser
> headers, and ships them with the plan. Agents rotate through pre-authenticated
> header sets in pure Rust. `check_*` is intentionally skipped — at high RPS
> the signal is status code, latency, and throughput, not body content.

### `LoadClient`

Thin wrapper around [httpx](https://www.python-httpx.org/).

```python
client.get(path, **kwargs)
client.post(path, **kwargs)
client.put(path, **kwargs)
client.patch(path, **kwargs)
client.delete(path, **kwargs)
```

`ResponseWrapper`: `.status_code`, `.ok`, `.text`, `.headers`, `.json()`,
`.elapsed_ms`, `.raise_for_status()`.

#### `client.batch(requests)` — concurrent requests in one PyO3 call

Execute N HTTP requests concurrently inside Rust, paying the PyO3 boundary cost
once for the whole batch. All requests are dispatched via a tokio `JoinSet` with
the GIL released for the entire duration.

```python
@task(weight=1)
def fetch_profile(self, client: LoadClient):
    auth = {"Authorization": f"Bearer {self.token}"}
    responses = client.batch([
        {"method": "GET", "path": "/api/user",    "headers": auth},
        {"method": "GET", "path": "/api/orders",  "headers": auth},
        {"method": "GET", "path": "/api/cart",    "headers": auth},
    ])
    # responses is a list of ResponseWrapper in dispatch order
```

Each dict accepts the same keys as the per-method helpers: `method`, `path`,
`headers`, `json`, `data`. `method` defaults to `"GET"`.

Benchmark result at batch size 5: **3355 RPS** (96% of static-mode ceiling, +45%
vs sequential tasks). Useful when each request has non-trivial latency — N
concurrent requests complete in `max(latency_i)` instead of `sum(latency_i)`.

---

## CLI Reference

```bash
loadpilot run [SCENARIO_FILE] [OPTIONS]
loadpilot init [DIRECTORY]
loadpilot version
```

### `loadpilot run`

Omit `SCENARIO_FILE` to open the interactive scenario browser (TTY only).

| Flag                | Default                 | Description |
|---------------------|-------------------------|-------------|
| `--target`          | `http://localhost:8000` | Base URL of the system under test |
| `--scenario`        | —                       | Scenario class name (required when file has multiple) |
| `--report`          | off                     | Write HTML report to this path |
| `--dry-run`         | off                     | Print the generated plan JSON and exit |
| `--agents`          | `1`                     | Spawn N local agent processes (embedded NATS) |
| `--external-agents` | `0`                     | Wait for N externally started agents |
| `--nats-url`        | —                       | Connect to external NATS (use with `--external-agents`) |
| `--threshold`       | from `@scenario`        | Override SLA threshold: `--threshold p99_ms=500` |
| `--results-json`    | off                     | Write final metrics as JSON to this path |

### `loadpilot init`

Scaffolds a new project:

```bash
loadpilot init my-load-tests
```

Creates `scenarios/example.py`, `.env.example`, and `monitoring/` (Prometheus +
Grafana stack, pre-configured with the LoadPilot dashboard). Safe to run on an
existing directory — does not overwrite existing files.

```bash
# Start live monitoring in one command
docker compose -f monitoring/docker-compose.yml up -d
# Grafana → http://localhost:3000  (LoadPilot dashboard auto-imported)
```

---

## SLA Thresholds

```python
@scenario(
    rps=100,
    duration="2m",
    thresholds={
        "p99_ms":     500,
        "p95_ms":     300,
        "error_rate": 1.0,   # percent
    },
)
```

After the test:

```
Thresholds
  ✓  p99 latency       243.0ms  <  500.0ms
  ✓  p95 latency       158.0ms  <  300.0ms
  ✓  error rate          0.0%   <    1.0%

All thresholds passed.
```

Exit code `1` on breach. Override from CLI without editing the file:

```bash
loadpilot run scenarios/health.py \
  --target https://staging.api.example.com \
  --threshold p99_ms=800 \
  --threshold error_rate=2
```

---

## HTML Report

```bash
loadpilot run scenarios/health.py --target https://api.example.com --report report.html
```

Includes: summary cards, SLA threshold results, latency table (p50/p95/p99/max),
RPS chart (actual vs target), latency chart over time, agent count in distributed mode.

Self-contained — open in any browser, no server required.
Parent directory is created automatically if it does not exist.

---

## Distributed Mode

Run a test across multiple machines. The CLI output is identical to single-machine
mode — coordinator aggregates everything transparently.

### Local agents

```bash
loadpilot run scenarios/health.py --target https://api.example.com --agents 4
```

Spawns 4 local agent processes, each handling `rps / 4`.

### External agents (separate machines)

```bash
# Install agent on each machine
curl -fsSL https://raw.githubusercontent.com/VladislavAkulich/loadpilot/main/install.sh | sh

# Start agents — they wait for a plan, complete it, then reconnect automatically
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-0
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-1

# Run test — coordinator uses embedded NATS
loadpilot run scenarios/health.py \
  --target https://api.example.com \
  --external-agents 2 \
  --report report.html
```

### Railway / external NATS

```bash
# Deploy NATS on Railway (Docker image: nats:latest, TCP port 4222)
# Deploy agent services with Dockerfile.agent, env vars:
#   COORDINATOR=<nats-tcp-address>
#   AGENT_ID=agent-0

loadpilot run scenarios/health.py \
  --target https://api.example.com \
  --nats-url nats://monorail.proxy.rlwy.net:PORT \
  --external-agents 2 \
  --report report.html
```

Agents are persistent — after a run they reconnect and wait for the next plan.

### Reliability

- **Synchronised start**: all agents begin within ~1ms of each other (coordinator
  sends `start_at` timestamp, agents sleep until it)
- **Agent timeout**: if an agent stops reporting for 15s it is marked timed-out;
  the test completes on the remaining agents without hanging
- **Agent recovery**: if a timed-out agent reconnects it is restored to the pool

### Architecture

```
CLI (Python)
  build plan → spawn coordinator
        │
        ▼ stdin (JSON)
Coordinator (Rust)
  ├── embedded NATS broker  (or connect to external NATS)
  ├── wait for N agents to register
  ├── shard plan + set synchronised start_at → publish to each agent
  ├── aggregate metrics (sum RPS, histogram-merged percentiles)
  ├── stdout JSON lines → CLI live dashboard
  └── :9090/metrics → Prometheus / Grafana

Agent (Rust, one per machine)
  ├── connect to NATS → register → receive shard
  ├── sleep until start_at (clock sync)
  ├── run HTTP load (token-bucket + reqwest)
  ├── stream metrics → NATS → coordinator
  └── reconnect and wait for next plan
```

---

## Metrics

Prometheus metrics on port **9090** while a test is running:

```bash
curl http://localhost:9090/metrics
```

Point a Prometheus scrape job at `host:9090` for live Grafana dashboards.

LoadPilot deliberately has no built-in web UI. Grafana lets you correlate load
test metrics with your service's own metrics (CPU, DB latency, error rates) —
something a standalone UI can't provide.

---

## How It Works

```
CLI (Python)
  load scenario file
  introspect @scenario classes
  pre-run each @task with MockClient → extract URL + method
  detect on_start / check_* → enable PyO3 bridge
  build JSON plan → spawn coordinator binary
        │
        ▼ stdin (JSON)
Coordinator (Rust / tokio)
  token-bucket scheduler (50ms ticks)
        │
        ├── Static mode (no Python callbacks)
        │     reqwest async HTTP → record success/error
        │     body not read (no check_* to feed)
        │
        └── PyO3 mode (on_start / check_* / async tasks / batch present)
              one OS thread per VUser — persistent, no per-task spawn overhead
              Python::attach per message only (~1–5µs channel overhead)
              RustClient (PyO3 pyclass) passed to Python task
              py.detach(|| reqwest HTTP) — GIL released during I/O
              GIL re-acquired only for Python callback execution
              async def tasks driven via coro.send(None) fast path
              — avoids asyncio scheduling overhead for sync-body coroutines
              client.batch([...]) — N concurrent requests, one PyO3 call
              — py.detach() + tokio JoinSet, GIL free for entire batch
              — 91% of static ceiling at batch size 5
        │
        ├── stdout JSON lines (1/sec) → CLI live dashboard
        └── :9090/metrics → Prometheus / Grafana
```

---

## Benchmark

All tools run in Docker against a Rust/axum echo server on the same machine.
Tools run sequentially with a 10s cooldown. LoadPilot uses static mode (no Python callbacks)
for the precision and max-throughput comparisons.

Reproduce: `cd bench && ./run.sh`

### Precision — 500 RPS target, 30s constant

| Tool | RPS actual | p50 | p99 | Errors |
|------|-----------|-----|-----|--------|
| LoadPilot | 499 | 3ms | 8ms | 0% |
| k6 | 500 | 1ms | 9ms | 0% |
| Locust | 497 | 120ms | 1400ms | 0% |

LoadPilot and k6 track the target accurately. Locust reaches the target RPS but its
Python/GIL scheduler adds significant latency even at moderate load.

### Max throughput — 30s constant, no artificial cap

| Tool | RPS | p50 | p99 | Errors | CPU avg | Mem peak |
|------|-----|-----|-----|--------|---------|----------|
| **LoadPilot (PyO3)** | **2205** | 11ms | 38ms | 0% | **165%** | 105 MB |
| k6 | 1799 | 14ms | 175ms | 0% | 212% | 107 MB |
| Locust | 677 | 100ms | 170ms | 0% | 117% | 50 MB |

LoadPilot runs in PyO3 mode with `on_start` + `check_*` (full Python callbacks). It delivers **1.2× k6** and **3.3× Locust** at max throughput. Per CPU core: LoadPilot 13.4 RPS/% vs k6 8.5 RPS/% — **1.6× better CPU efficiency per request**.

### PyO3 mode — architecture comparison (500 RPS target)

LoadPilot supports Python lifecycle hooks (`on_start`, `check_*`) and async tasks via
a PyO3 bridge. The bridge architecture evolved across versions:

| Architecture | RPS actual | p50 | p99 | Notes |
|---|---|---|---|---|
| Static (no callbacks) | 499 | 3ms | 8ms | Rust only |
| spawn_blocking (old) | 488 | 7ms | 20ms | new OS thread + GIL attach per task |
| no-GIL Python 3.13t | 478 | 14ms | 127ms | no GIL but higher overhead |
| **persistent threads (current)** | **487** | **2ms** | **7ms** | one thread per VUser, GIL released during I/O |

The current architecture keeps one OS thread per VUser and calls `Python::attach`
once per task message. HTTP I/O releases the GIL via `py.detach()`, allowing all
VUser threads to run HTTP requests concurrently. `async def` tasks are driven with a
`coro.send(None)` fast path — no asyncio scheduling overhead for the common case of
sync-body coroutines.

### PyO3 max throughput — optimisation experiments

Measured at 3500 RPS target with `on_start` (login) + sync or async task. All
experiments on Python 3.12 (GIL), Docker bridge network.

| Approach | HTTP RPS | p50 | p99 | Notes |
|---|---|---|---|---|
| `asyncio.run_until_complete` | 1591 | — | — | historical baseline |
| `asyncio.run_coroutine_threadsafe` | 731 | — | — | −54% — OS pipe wakeup overhead |
| `coro.send(None)` fast path | 2289 | 11ms | 37ms | current async task impl |
| sync `def` task | 2487 | 22ms | 67ms | no asyncio overhead |
| async task + `check_*` | 2205 | 11ms | 38ms | `check_*(self, status_code, body)` |
| `asyncio.gather(5)` | 1450 | 13ms | 28ms | `call_soon_threadsafe` × 5 negates gain |
| **`client.batch(5)`** | **3385** | **14ms** | **34ms** | **pure Rust JoinSet** |
| Static ceiling (no Python) | 3494 | 18ms | 537ms | reference |

`client.batch(N)` dispatches N HTTP requests concurrently inside a single Rust
`block_on` with `py.detach()` — the GIL is released for the entire batch. PyO3
overhead is paid once per N requests rather than once per request. At batch
size 5 this reaches **97% of static mode**.

`check_*(self, status_code, body)` adds only ~4% overhead vs async task without
checks — JSON is pre-parsed in `py.detach()` and the check method receives plain
Python primitives, not a wrapper object.

`coro.send(None)` fast path drives sync-body `async def` without the asyncio
scheduler — ~10µs vs ~200µs per coroutine. Falls back to `run_until_complete`
automatically when the coroutine has real `await` expressions.

`asyncio.gather` was tested but performs worse than sequential: each future
resolution requires `Python::attach → call_soon_threadsafe` from a tokio thread,
adding pipe-wakeup overhead × N per task dispatch. Only advantageous when
individual request latency is high (>100ms) and N is large.

---

## Roadmap

| Version | Feature | Status |
|---------|---------|--------|
| v0.1 | Python DSL + Rust coordinator, terminal dashboard, Prometheus | ✅ done |
| v0.2 | PyO3 bridge — on_start / on_stop / check_* | ✅ done |
| v0.2 | HTML report | ✅ done |
| v0.3 | Multiple HTTP calls per task | ✅ done |
| v0.3 | Thresholds — fail with exit code 1 on SLA breach | ✅ done |
| v0.3 | `pip install` — prebuilt coordinator binaries | ✅ done |
| v0.4 | Distributed mode — embedded NATS + local agents | ✅ done |
| v0.4 | External agents — Railway / remote machines | ✅ done |
| v0.4 | Agent install script (`curl \| sh`) | ✅ done |
| v0.5 | Multiple `@scenario` per file | ✅ done |
| v0.5 | Load profiles — ramp / constant / step / spike | ✅ done |
| v0.5 | `on_start` in distributed mode (pre-auth pool) | ✅ done |
| v0.5 | Histogram merging — exact percentiles in distributed | ✅ done |
| v0.5 | Clock skew fix — synchronised agent start | ✅ done |
| v0.5 | NATS SPOF — agent timeout + recovery | ✅ done |
| v0.5 | Interactive TUI — `loadpilot run` with no args | ✅ done |
| v0.5 | `loadpilot init` — project scaffold | ✅ done |
| v0.6 | Benchmark — LoadPilot vs Locust vs k6, published results | ✅ done |
| v0.6 | PyO3 persistent threads — one OS thread per VUser, GIL released during I/O | ✅ done |
| v0.6 | `async def` task support — `coro.send(None)` fast path | ✅ done |
| v0.6 | `client.batch()` — N concurrent HTTP, one PyO3 call, 97% of static ceiling | ✅ done |
| v0.6 | GitHub Releases + verify install.sh end-to-end | planned |
| v1.0 | Production hardening | planned |

---

## Building from Source

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
# Python tests
cd cli
uv sync --extra dev
uv run python -m pytest tests/ -v

# Rust tests
cd engine
cargo test
```

---

## License

MIT
