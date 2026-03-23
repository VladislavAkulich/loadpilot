# LoadPilot

[![CI](https://github.com/VladislavAkulich/loadpilot/actions/workflows/ci.yml/badge.svg)](https://github.com/VladislavAkulich/loadpilot/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> Write load tests in Python. Run them at Rust speed.

```
LoadPilot — CheckoutFlow  [01:42]  steady: 100/100 RPS

  Requests/sec:     100.0      Total:     9,812
  Errors:             0.0%     Failed:         0

  Latency:
    p50:   12ms
    p95:   28ms
    p99:   41ms
    max:  203ms

  [████████████████████] 100%
```

LoadPilot is a load testing tool for teams that want to write scenarios in Python
without the throughput ceiling of a pure-Python engine.

Scenarios are plain Python classes. HTTP execution runs in async Rust.

---

## Install

```bash
pip install loadpilot
```

No Rust required — the coordinator binary is bundled in the wheel.

**Supported platforms:** Linux x86_64 · macOS Intel · macOS Apple Silicon · Windows x86_64

---

## Quick Start

```bash
# scaffold a project
loadpilot init my-load-tests
cd my-load-tests

# run a scenario
loadpilot run scenarios/example.py --target https://your-api.example.com
```

Or run without arguments to pick a scenario interactively:

```bash
loadpilot run --target https://your-api.example.com
```

→ [Getting Started in 5 minutes](https://vladislavakulich.github.io/loadpilot/getting-started.html)

---

## Write a Scenario

```python
from loadpilot import VUser, scenario, task, LoadClient

@scenario(rps=100, duration="2m", ramp_up="30s")
class CheckoutFlow(VUser):

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={"user": "test", "pass": "secret"})
        self.token = resp.json()["access_token"]

    @task(weight=5)
    def browse(self, client: LoadClient):
        client.get("/api/products", headers={"Authorization": f"Bearer {self.token}"})

    def check_browse(self, status_code: int, body) -> None:
        assert status_code == 200
        assert isinstance(body, list)

    @task(weight=1)
    def purchase(self, client: LoadClient):
        client.post("/api/orders", json={"product_id": 42, "qty": 1},
                    headers={"Authorization": f"Bearer {self.token}"})
```

- `on_start` — runs once per virtual user before tasks start (login, setup)
- `@task` — weighted HTTP task; multiple tasks run in proportion
- `check_{task}` — assertion after each request; fail = error counted

---

## Features

| | |
|---|---|
| **Python DSL** | `@scenario`, `@task`, `on_start`, `check_*` — no new syntax to learn |
| **Load profiles** | ramp (default), constant, step, spike |
| **SLA thresholds** | fail CI with exit code 1 on p99/p95/error-rate breach |
| **Distributed mode** | coordinator + N agents over NATS; local or remote, free |
| **HTML report** | self-contained, open in any browser, no server required |
| **Prometheus metrics** | live Grafana dashboards while the test runs |
| **Interactive TUI** | `loadpilot run` with no args opens a scenario browser |
| **`pip install`** | coordinator binary bundled in the wheel, no Rust needed |

---

## Performance

LoadPilot is benchmarked against k6 and Locust against a Rust/axum echo server in Docker.
LoadPilot runs in **PyO3 mode** — with `on_start` (login) and `check_*` (assertion per task) — a realistic workload with Python callbacks active.

### Precision — 500 RPS target

| Tool | RPS actual | p50 | p99 | Errors | CPU avg |
|------|-----------|-----|-----|--------|---------|
| **LoadPilot (PyO3)** | **478** | 4ms | 15ms | 0% | **14%** |
| k6 | 491 | 8ms | 118ms | 0% | 129% |
| Locust | 498 | 150ms | 1500ms | 0% | 88% |

LoadPilot uses **9× less CPU** than k6 at the same load. Locust reaches the RPS target but its Python/GIL scheduler adds significant latency (p99 ≥ 1500ms at only 500 RPS).

### Max throughput — 30s, no cap

| Tool | RPS | p50 | p99 | Errors | CPU avg |
|------|-----|-----|-----|--------|---------|
| **LoadPilot (PyO3)** | **2205** | 11ms | 38ms | 0% | **165%** |
| k6 | 1799 | 14ms | 175ms | 0% | 212% |
| Locust | 677 | 100ms | 170ms | 0% | 117% |

**1.2× k6** and **3.3× Locust** at max throughput. **1.6× better CPU efficiency per request** vs k6.

→ [Full benchmark details and methodology](https://vladislavakulich.github.io/loadpilot/benchmark.html)

---

## SLA Thresholds

```python
@scenario(
    rps=100, duration="2m",
    thresholds={"p99_ms": 500, "p95_ms": 300, "error_rate": 1.0},
)
class CheckoutFlow(VUser): ...
```

```
Thresholds
  ✓  p99 latency       41ms  <  500ms
  ✓  p95 latency       28ms  <  300ms
  ✓  error rate         0%   <    1%

All thresholds passed.
```

Exit code `1` on breach — drop into any CI pipeline. Override without editing the file:

```bash
loadpilot run scenarios/checkout.py --target https://staging.example.com \
  --threshold p99_ms=800 --threshold error_rate=2
```

---

## Distributed Mode

```bash
# 4 local processes
loadpilot run scenarios/checkout.py --target https://api.example.com --agents 4

# external agents on remote machines
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --external-agents 4
```

The CLI output is identical to single-machine mode — the coordinator aggregates
everything transparently. `on_start` (login, setup) runs on the coordinator and
ships per-user auth headers to agents, so distributed tests can authenticate
without Python running on each agent.

→ [Distributed mode guide](https://vladislavakulich.github.io/loadpilot/distributed.html)

---

## Live Monitoring

```bash
# scaffolded by `loadpilot init`
docker compose -f monitoring/docker-compose.yml up -d
# Grafana → http://localhost:3000   (LoadPilot dashboard auto-imported)
# Prometheus → http://localhost:9091
```

Use Grafana to correlate load test metrics with your service's own metrics
(CPU, DB latency, error rates) on the same timeline.

---

## Documentation

- [Getting Started](https://vladislavakulich.github.io/loadpilot/getting-started.html)
- [DSL Reference](https://vladislavakulich.github.io/loadpilot/dsl-reference.html) — `@scenario`, `@task`, `on_start`, `check_*`, `client.batch()`
- [CLI Reference](https://vladislavakulich.github.io/loadpilot/cli-reference.html) — all flags for `loadpilot run` and `loadpilot init`
- [Distributed Mode](https://vladislavakulich.github.io/loadpilot/distributed.html) — local agents, remote agents, Railway, NATS
- [Benchmark](https://vladislavakulich.github.io/loadpilot/benchmark.html) — methodology, full results, how to reproduce
- [Architecture](https://vladislavakulich.github.io/loadpilot/architecture.html) — how the Python DSL and Rust engine interact
- [Development](https://vladislavakulich.github.io/loadpilot/development.html) — building from source, running tests

---

## License

MIT
