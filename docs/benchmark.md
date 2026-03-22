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

| Tool | RPS actual | p50 | p99 | Errors |
|------|-----------|-----|-----|--------|
| LoadPilot | 499 | 3ms | 8ms | 0% |
| k6 | 500 | 1ms | 9ms | 0% |
| Locust | 497 | 120ms | 1400ms | 0% |

LoadPilot and k6 track the target accurately. Locust reaches the RPS but its Python/GIL scheduler adds significant latency even at moderate load.

### Max throughput — 30s constant, no artificial cap

| Tool | RPS | p50 | p99 | Errors |
|------|-----|-----|-----|--------|
| **LoadPilot** | **3494** | 18ms | 430ms | 0% |
| k6 | 1638 | 22ms | 187ms | 0% |
| Locust | 697 | 99ms | 190ms | 0% |

LoadPilot delivers **2.1× k6** and **5.0× Locust** at max throughput.

### PyO3 precision — 500 RPS, on_start + optional check_*

Measured with Python 3.12 (GIL). One OS thread per VUser, GIL released during HTTP via `py.detach()`.

| Architecture | RPS actual | p50 | p99 | Notes |
|---|---|---|---|---|
| Static (no callbacks) | 499 | 3ms | 8ms | Rust only |
| + on_start | 487 | 2ms | 7ms | login per VUser |
| + on_start + check_* | 487 | 2ms | 7ms | assertion per task |

Adding Python callbacks at 500 RPS has near-zero cost — persistent threads pay `Python::attach` once per task message, and `py.detach()` allows all VUser threads to run HTTP concurrently.

### PyO3 max throughput — optimisation experiments

Measured at 3 500 RPS target, Python 3.12 (GIL), Docker bridge network. All variants use `on_start` (login) + async or sync task.

| Approach | HTTP RPS | p50 | p99 | Notes |
|---|---|---|---|---|
| `asyncio.run_until_complete` | 1591 | — | — | baseline |
| `asyncio.run_coroutine_threadsafe` | 731 | — | — | OS pipe wakeup per request |
| sync `def` task | 2139 | — | — | no asyncio overhead |
| `coro.send(None)` fast path | 2226 | 12ms | 39ms | +40% vs baseline |
| `asyncio.gather(5)` | 1450 | 13ms | 28ms | `call_soon_threadsafe` × 5 negates gain |
| **`client.batch(5)`** | **3170** | **6ms** | **15ms** | **pure Rust JoinSet, +42% vs fast path** |
| Static ceiling (no Python) | 3494 | 18ms | 430ms | reference |

`client.batch(N)` dispatches N HTTP requests concurrently inside a single Rust `block_on` with `py.detach()` — the GIL is released for the entire batch. PyO3 overhead is paid once per N requests. At batch size 5 this reaches **91% of static mode**.

Note: `asyncio.gather` was tested but performs worse than sequential because each future resolution requires `Python::attach → call_soon_threadsafe` from a tokio thread, adding pipe-wakeup overhead × N per task dispatch.

## Methodology notes

**Why Docker?**
Reproducible on any machine with Docker. The bridge network adds a small fixed overhead equally for all tools, so relative comparisons remain valid.

**Why sequential runs?**
Running tools simultaneously would saturate the target server and mix results. Sequential runs with a 10s cooldown give each tool a clean slate.

**LoadPilot static mode vs PyO3 mode**
Static mode is used for the main cross-tool comparison (no `on_start` or `check_*`). In this mode LoadPilot runs as pure Rust — no Python is invoked during the test. This is equivalent to k6/Locust scenarios with no custom logic beyond the HTTP call.

PyO3 mode activates when `on_start`, `on_stop`, `check_*`, or `client.batch()` are present. The PyO3 precision results show this adds negligible overhead at 500 RPS. The max throughput experiments show the ceiling and how different designs affect it.

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
