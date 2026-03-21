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

    def check_browse(self, response) -> None:
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

```python
@task(weight=1)
def checkout(self, client: LoadClient):
    cart = client.get("/cart", headers=self._auth())
    item_id = cart.json()["items"][0]["id"]
    client.post("/orders", json={"item_id": item_id, "qty": 1}, headers=self._auth())

def check_checkout(self, response) -> None:
    assert response.status_code in (200, 201)
```

Each HTTP call inside a task is measured independently. `check_checkout` receives
the response from the last call.

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
| `check_{task}(self, resp)`| After each task's HTTP response | — | ❌ status-based only |

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
        └── PyO3 mode (on_start / check_* present)
              RustClient (PyO3 pyclass) passed to Python task
              py.allow_threads(|| reqwest HTTP) — GIL released for I/O
              GIL re-acquired only for Python callback
        │
        ├── stdout JSON lines (1/sec) → CLI live dashboard
        └── :9090/metrics → Prometheus / Grafana
```

---

## Benchmark

All tools run in Docker against a Rust/axum echo server on the same machine.
Tools run sequentially with a 10s cooldown. LoadPilot uses static mode (no Python callbacks).

### Precision — 500 RPS target, 30s constant

| Tool | RPS actual | p50 | p95 | p99 | Errors |
|------|-----------|-----|-----|-----|--------|
| LoadPilot | 500 | 3ms | 6ms | 11ms | 0% |
| k6 | 500 | 1ms | 3ms | 8ms | 0% |
| Locust | 497 | 170ms | 360ms | 1500ms | 0% |

LoadPilot and k6 are both accurate. Locust hits the target RPS but adds significant latency overhead from its Python/GIL scheduler — visible even at moderate load.

### Max throughput — 30s constant, no artificial cap

| Tool | RPS | p50 | p95 | p99 | Errors |
|------|-----|-----|-----|-----|--------|
| LoadPilot | **3326** | 19ms | 192ms | 512ms | 0% |
| k6 | 1617 | 17ms | 113ms | 164ms | 0% |
| Locust | 694 | 99ms | 130ms | 160ms | 0% |

LoadPilot delivers **2.1× k6** and **4.8× Locust** at max throughput with zero errors.

Reproduce: `cd bench && ./run.sh` — see [docs/benchmark.md](docs/benchmark.md) for full methodology.

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
| v0.6 | Benchmark — LoadPilot vs Locust vs k6, published results | planned |
| v0.6 | GitHub Releases + verify install.sh end-to-end | planned |
| v1.0 | Public benchmark, production hardening | planned |

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
