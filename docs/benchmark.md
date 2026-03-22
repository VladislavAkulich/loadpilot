---
title: Benchmark
nav_order: 6
layout: default
---

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

LoadPilot and k6 hold the target accurately. Locust reaches the RPS but its Python/GIL scheduler adds significant latency (p99 ≥ 1500ms at only 500 RPS). LoadPilot uses **9× less CPU** than k6 at the same load — it is mostly idle, waiting for the token bucket to fire.

### Max throughput — 30s constant, no artificial cap

| Tool | RPS | p50 | p99 | Errors | CPU avg | CPU peak | Mem peak |
|------|-----|-----|-----|--------|---------|----------|----------|
| **LoadPilot (PyO3)** | **2205** | 11ms | 38ms | 0% | 165% | 179% | **105 MB** |
| k6 | 1799 | 14ms | 175ms | 0% | 212% | 229% | 107 MB |
| Locust | 677 | 100ms | 170ms | 0% | 117% | 122% | 50 MB |

LoadPilot runs in PyO3 mode with `on_start` + `check_*` (full Python callbacks — a realistic scenario). It still delivers **1.2× k6** and **3.3× Locust** at max throughput. Per CPU: LoadPilot 2205/165% ≈ 13.4 RPS/core vs k6 1799/212% ≈ 8.5 RPS/core — roughly **1.6× better CPU efficiency per request**. Locust burns similar CPU to LoadPilot but delivers only 31% of the RPS.

### PyO3 precision — 500 RPS, on_start + optional check_*

Measured with Python 3.12 (GIL). One OS thread per VUser, GIL released during HTTP via `py.detach()`.

| Architecture | RPS actual | p50 | p99 | CPU avg | Mem peak | Notes |
|---|---|---|---|---|---|---|
| Static (no callbacks) | 499 | 3ms | 11ms | 24% | 43 MB | Rust only |
| + on_start | 486 | 2ms | 5ms | 77% | 74 MB | login per VUser |
| + on_start + check_* | 478 | 4ms | 15ms | 14% | 68 MB | assertion per task |

Adding Python callbacks at 500 RPS has near-zero latency cost — persistent threads pay `Python::attach` once per task message, and `py.detach()` allows all VUser threads to run HTTP concurrently. CPU increases (24% → 77%) because VUser threads are now active Python threads, but latency is unaffected.

### PyO3 max throughput — optimisation experiments

Measured at 3 500 RPS target, Python 3.12 (GIL), Docker bridge network. All variants use `on_start` (login) + async or sync task.

| Approach | HTTP RPS | p50 | p99 | CPU avg | Mem peak | Notes |
|---|---|---|---|---|---|---|
| `asyncio.run_until_complete` | 1591 | — | — | — | — | historical baseline |
| `asyncio.run_coroutine_threadsafe` | 731 | — | — | — | — | OS pipe wakeup per request |
| `coro.send(None)` fast path | 2289 | 11ms | 37ms | 190% | 115 MB | current async task impl |
| sync `def` task | 2487 | 22ms | 67ms | 167% | 116 MB | no asyncio overhead |
| async task + `check_*` | 2205 | 11ms | 38ms | 165% | 105 MB | `check_*(self, status_code, body)` |
| `asyncio.gather(5)` | 1450 | 13ms | 28ms | — | — | `call_soon_threadsafe` × 5 negates gain |
| **`client.batch(5)`** | **3385** | **14ms** | **34ms** | **147%** | **79 MB** | **pure Rust JoinSet** |
| Static ceiling (no Python) | 3494 | 18ms | 537ms | 115% | 76 MB | reference |

`client.batch(N)` dispatches N HTTP requests concurrently inside a single Rust `block_on` with `py.detach()` — the GIL is released for the entire batch. PyO3 overhead is paid once per N requests. At batch size 5 this reaches **97% of static mode**.

`check_*(self, status_code, body)` adds only ~4% overhead vs no-check async task (2205 vs 2289 RPS). JSON is pre-parsed in `py.detach()` (no GIL) and cached; the check method receives a plain Python int and dict — no wrapper object overhead.

Note: `asyncio.gather` was tested but performs worse than sequential because each future resolution requires `Python::attach → call_soon_threadsafe` from a tokio thread, adding pipe-wakeup overhead × N per task dispatch.

## Methodology notes

**Why Docker?**
Reproducible on any machine with Docker. The bridge network adds a small fixed overhead equally for all tools, so relative comparisons remain valid.

**Why sequential runs?**
Running tools simultaneously would saturate the target server and mix results. Sequential runs with a 10s cooldown give each tool a clean slate.

**Resource measurement**
CPU and memory are sampled via `docker stats --no-stream` every 1 second while the load-generator container runs (`collect_stats.py`). CPU % is relative to one core (100% = one core fully busy). Memory is peak RSS reported by the Docker cgroup. The target server is excluded — only the load generator container is measured.

**LoadPilot PyO3 mode in cross-tool comparison**
The main cross-tool tables use LoadPilot in PyO3 mode with `on_start` (login) and `check_*` (assertion per task) — a realistic scenario that mirrors how users write load tests. This is more conservative than static mode and makes the comparison fair.

Static mode (pure Rust, no Python) is the performance ceiling. PyO3 mode activates when `on_start`, `on_stop`, `check_*`, or `client.batch()` are present. The PyO3 precision results show callbacks add negligible overhead at 500 RPS. The max throughput experiments show the ceiling and how different PyO3 architectures compare.

**Locust latency in precision mode**
Locust achieves the target RPS but adds significant latency overhead (p99 ≈ 1400ms at 500 RPS). This is not a network issue — it is the cost of the Python/GIL coroutine scheduler dispatching requests. At higher RPS this overhead grows.

**PyO3 batch API**
`client.batch([...])` is available as an optimisation for tasks that make multiple HTTP calls. It accepts a list of request dicts and executes them concurrently via a tokio `JoinSet`, returning a list of responses. At batch size 5 it reaches 3170 RPS — 91% of the static ceiling. Useful when individual request latency is high enough that concurrency significantly reduces total task time.

## Scenario file layout

```
bench/scenarios/
  precision/
    loadpilot.py   ← static (no callbacks), rps=500
    pyo3.py        ← PyO3OnStart, PyO3Full
    locust.py
    k6.js
  max/
    loadpilot.py   ← static (no callbacks), rps=3500
    pyo3.py        ← PyO3MaxOnStart, PyO3MaxFull, PyO3MaxSync, PyO3Batch5
    locust.py
    k6.js
```

Edit the RPS targets or duration, then re-run `./run.sh`. The report is regenerated automatically.
