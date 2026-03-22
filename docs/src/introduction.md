# LoadPilot

**Write load tests in Python. Run them at Rust speed.**

LoadPilot is a load testing tool with a Python DSL for writing scenarios and a Rust execution engine for running them with minimal overhead.

```bash
pip install loadpilot
```

---

## Quick example

```python
from loadpilot import VUser, scenario, task, LoadClient

@scenario(rps=50, duration="1m")
class HealthCheck(VUser):

    @task
    def ping(self, client: LoadClient):
        client.get("/health")
```

```bash
loadpilot run scenarios/health.py --target https://your-api.example.com
```

---

## Features

- **Python scenarios** — write tests in plain Python, no YAML or XML
- **Rust execution engine** — low-overhead HTTP engine built on Tokio + reqwest
- **Live TUI** — real-time RPS, latency percentiles (p50/p95/p99), and error rate
- **Load profiles** — ramp, constant, step, spike
- **Distributed mode** — scale across multiple machines with a single flag
- **SLA thresholds** — fail CI automatically on p99 or error rate breaches
- **Grafana dashboard** — Prometheus metrics + pre-built dashboard, zero config
- **HTML reports** — self-contained report file after every run

---

## Get started

→ [Getting Started](getting-started.md) — install and run your first test in 5 minutes

→ [DSL Reference](dsl-reference.md) — `@scenario`, `VUser`, `LoadClient`, thresholds

→ [CLI Reference](cli-reference.md) — all flags and commands

→ [Distributed Mode](distributed.md) — scale across machines

→ [Benchmark](benchmark.md) — how LoadPilot compares to Locust and k6
