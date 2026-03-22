---
title: Getting Started
nav_order: 2
layout: default
---

# Getting started in 5 minutes

## Prerequisites

- Python 3.12+
- An HTTP service to test against
- Platform: Linux x86_64, macOS (Intel or Apple Silicon), Windows x86_64

## Install

```bash
pip install loadpilot
```

Or with [uv](https://github.com/astral-sh/uv):

```bash
uv tool install loadpilot
```

## 1. Scaffold a project

```bash
loadpilot init my-load-tests
cd my-load-tests
```

This creates:

```
my-load-tests/
  scenarios/
    example.py               ← starter scenario
  monitoring/
    docker-compose.yml       ← Prometheus + Grafana, pre-configured
    grafana-dashboard.json   ← LoadPilot dashboard, auto-imported
  .env.example
```

## 2. Start live monitoring (optional)

Your project includes a ready-to-use Grafana + Prometheus stack:

```bash
docker compose -f monitoring/docker-compose.yml up -d
```

Open [http://localhost:3000](http://localhost:3000) → **Dashboards → LoadPilot**.

The dashboard auto-imports on first start — no manual setup. Grafana shows RPS, latency percentiles, and error rate in real time while a test runs.

> Requires Docker with Compose v2 (`docker compose` not `docker-compose`).

## 4. Write your first scenario

Open `scenarios/example.py`. You'll see a working example. Here's the minimal version:

```python
from loadpilot import VUser, scenario, task, LoadClient

@scenario(rps=10, duration="30s")
class HealthCheck(VUser):

    @task
    def ping(self, client: LoadClient):
        client.get("/health")
```

Replace `/health` with an endpoint on your service. That's it — LoadPilot will infer the method and URL automatically.

## 5. Run it

```bash
loadpilot run scenarios/example.py --target https://your-api.example.com
```

You'll see a live dashboard:

```
LoadPilot — HealthCheck  [00:12]  ramp: 4/10 RPS

  Requests/sec:       4.0      Total:        48
  Errors:             0.0%     Failed:         0

  Latency:
    p50:   28ms
    p95:   41ms
    p99:   89ms
    max:  203ms

  [████░░░░░░░░░░░░░░░░]  20%
```

Press `Ctrl+C` to stop early.

## 6. Interactive mode

If you run `loadpilot run` without a file, you get a scenario browser:

```bash
loadpilot run --target https://your-api.example.com
```

Pick a file, then pick a scenario from the list. Useful when you have multiple scenarios and don't want to remember their names.

## 7. Save a report

```bash
loadpilot run scenarios/example.py \
  --target https://your-api.example.com \
  --report report.html
```

Opens in any browser — no server required.

---

## Next steps

### Set SLA thresholds

Fail CI automatically if latency or error rate exceeds your SLA:

```python
@scenario(
    rps=50,
    duration="1m",
    thresholds={"p99_ms": 500, "error_rate": 1.0},
)
class HealthCheck(VUser): ...
```

Exit code `1` on breach. Override from the CLI without changing the file:

```bash
loadpilot run scenarios/example.py \
  --target https://staging.api.example.com \
  --threshold p99_ms=800
```

### Add authentication

```python
@scenario(rps=50, duration="1m")
class AuthenticatedFlow(VUser):

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={"username": "test", "password": "secret"})
        self.token = resp.json()["access_token"]

    @task
    def browse(self, client: LoadClient):
        client.get("/api/products", headers={"Authorization": f"Bearer {self.token}"})
```

`on_start` runs once per virtual user before tasks begin.

### Choose a load profile

```python
@scenario(rps=100, duration="2m", mode="constant")   # full load immediately
@scenario(rps=100, duration="2m", mode="ramp")        # linear ramp (default)
@scenario(rps=100, duration="2m", mode="step", steps=5)  # staircase
@scenario(rps=100, duration="2m", mode="spike")       # 20% → 100% → 20%
```

### Run distributed

Scale across multiple machines:

```bash
# 4 local processes
loadpilot run scenarios/example.py --target https://api.example.com --agents 4

# External agents on remote machines
loadpilot run scenarios/example.py \
  --target https://api.example.com \
  --external-agents 4
```

See the [README](../README.md#distributed-mode) for full distributed setup including Railway deployment.

### Live Grafana metrics

The `monitoring/` directory scaffolded by `loadpilot init` gives you a full observability stack:

```bash
docker compose -f monitoring/docker-compose.yml up -d
# Prometheus → http://localhost:9091
# Grafana    → http://localhost:3000  (admin / admin)
```

The LoadPilot dashboard auto-imports on first start. It shows RPS (actual vs target), latency percentiles, active workers, and error rate — updated every 2 seconds while a test runs.

Use Grafana to correlate load test metrics with your service's own metrics (CPU, DB latency, error rates) on the same timeline.
