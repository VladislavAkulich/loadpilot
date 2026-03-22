# Benchmark

How to reproduce the LoadPilot benchmark results.

## What is tested

Four scenarios:

- **Precision** — all tools target 500 RPS for 30s. Measures how accurately each tool holds the target RPS and what latency overhead the load generator itself adds.
- **Max throughput** — each tool runs at maximum capacity for 30s. Measures the throughput ceiling on a single machine.
- **PyO3 precision** (LoadPilot only) — measures the cost of enabling Python callbacks (`on_start`, `check_*`) at 500 RPS.
- **PyO3 max throughput** (LoadPilot only) — measures the ceiling of different PyO3 architectures and optimisations.

## Setup

**Requirements:**
- Docker with Compose v2 (`docker compose`, not `docker-compose`)
- Python 3.x (for the report script)

**Tools under test:**
- LoadPilot (static mode and PyO3 mode)
- [k6](https://k6.io/) v0.55+
- [Locust](https://locust.io/) v2.x

**Target server:** Rust/axum echo server built and run in Docker. Endpoints:
- `POST /auth/login` → `{"access_token": "tok"}` (used by `on_start`)
- `GET /api/user` → `{"id": 1, "name": "bench"}` (main task endpoint)
- `GET /health` → `{"status": "ok"}`

All containers share the same Docker bridge network. Tools run sequentially with a 10s cooldown between runs.

## Running

```bash
cd bench
./run.sh
```

This will:
1. Build the target server and LoadPilot Docker images
2. Start the target server
3. Run each tool sequentially (8 runs total)
4. Generate `results/report.html`

First run takes longer — Rust compilation is cached after that.

To run a single profile:

```bash
cd bench

# Precision
docker compose --profile loadpilot-precision run --rm loadpilot-precision
docker compose --profile k6-precision       run --rm k6-precision
docker compose --profile locust-precision   run --rm locust-precision

# Max throughput
docker compose --profile loadpilot-max      run --rm loadpilot-max
docker compose --profile k6-max            run --rm k6-max
docker compose --profile locust-max        run --rm locust-max

# PyO3 — precision
docker compose --profile loadpilot-pyo3-onstart run --rm loadpilot-pyo3-onstart
docker compose --profile loadpilot-pyo3-full    run --rm loadpilot-pyo3-full

# PyO3 — max throughput
docker compose --profile loadpilot-pyo3-max-onstart run --rm loadpilot-pyo3-max-onstart
docker compose --profile loadpilot-pyo3-max-full    run --rm loadpilot-pyo3-max-full

# PyO3 — batch API
docker compose --profile loadpilot-pyo3-batch5 run --rm loadpilot-pyo3-batch5

# Regenerate report from existing results
python3 report.py
```

## Results

### Precision — 500 RPS target, 30s constant

| Tool | RPS actual | p50 | p99 | Errors | CPU avg | CPU peak | Mem peak |
|------|-----------|-----|-----|--------|---------|----------|----------|
| **LoadPilot (PyO3)** | **478** | 4ms | 15ms | 0% | **14%** | 108% | **68 MB** |
| k6 | 491 | 8ms | 118ms | 0% | 129% | 140% | 59 MB |
| Locust | 498 | 150ms | 1500ms | 0% | 88% | 119% | 85 MB |

> CPU % is relative to one core (200% = two cores fully busy).
> LoadPilot runs in PyO3 mode with `on_start` (login) and `check_*` (assertion per task) — a realistic scenario with Python callbacks.

LoadPilot and k6 hold the target accurately. Locust reaches the RPS but its Python/GIL scheduler adds significant latency (p99 ≥ 1500ms at only 500 RPS). LoadPilot uses **9× less CPU** than k6 at the same load.

### Max throughput — 30s constant, no artificial cap

| Tool | RPS | p50 | p99 | Errors | CPU avg | CPU peak | Mem peak |
|------|-----|-----|-----|--------|---------|----------|----------|
| **LoadPilot (PyO3)** | **2205** | 11ms | 38ms | 0% | 165% | 179% | **105 MB** |
| k6 | 1799 | 14ms | 175ms | 0% | 212% | 229% | 107 MB |
| Locust | 677 | 100ms | 170ms | 0% | 117% | 122% | 50 MB |

LoadPilot runs in PyO3 mode with `on_start` + `check_*`. It delivers **1.2× k6** and **3.3× Locust** at max throughput. Per CPU: LoadPilot ≈ 13.4 RPS/core vs k6 ≈ 8.5 RPS/core — roughly **1.6× better CPU efficiency**.

### PyO3 precision — 500 RPS, on_start + optional check_*

| Architecture | RPS actual | p50 | p99 | CPU avg | Mem peak | Notes |
|---|---|---|---|---|---|---|
| Static (no callbacks) | 499 | 3ms | 11ms | 24% | 43 MB | Rust only |
| + on_start | 486 | 2ms | 5ms | 77% | 74 MB | login per VUser |
| + on_start + check_* | 478 | 4ms | 15ms | 14% | 68 MB | assertion per task |

Adding Python callbacks at 500 RPS has near-zero latency cost.

### PyO3 max throughput — optimisation experiments

| Approach | HTTP RPS | p50 | p99 | CPU avg | Mem peak | Notes |
|---|---|---|---|---|---|---|
| `asyncio.run_until_complete` | 1591 | — | — | — | — | historical baseline |
| `coro.send(None)` fast path | 2289 | 11ms | 37ms | 190% | 115 MB | current async task impl |
| sync `def` task | 2487 | 22ms | 67ms | 167% | 116 MB | no asyncio overhead |
| async task + `check_*` | 2205 | 11ms | 38ms | 165% | 105 MB | `check_*(self, status_code, body)` |
| **`client.batch(5)`** | **3385** | **14ms** | **34ms** | **147%** | **79 MB** | **pure Rust JoinSet** |
| Static ceiling (no Python) | 3494 | 18ms | 537ms | 115% | 76 MB | reference |

`client.batch(N)` reaches **97% of static mode** at batch size 5.

## Methodology notes

**Why Docker?**
Reproducible on any machine with Docker. The bridge network adds a small fixed overhead equally for all tools, so relative comparisons remain valid.

**Why sequential runs?**
Running tools simultaneously would saturate the target server and mix results. Sequential runs with a 10s cooldown give each tool a clean slate.

**Resource measurement**
CPU and memory are sampled via `docker stats --no-stream` every 1 second. CPU % is relative to one core (100% = one core fully busy). Memory is peak RSS reported by the Docker cgroup. The target server is excluded.
