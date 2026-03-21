# Benchmark

How to reproduce the LoadPilot benchmark results.

## What is tested

Three scenarios:

- **Precision** — all tools target 500 RPS for 30s. Measures how accurately each tool holds the target RPS and what latency overhead the load generator itself adds.
- **Max throughput** — each tool runs at maximum capacity for 30s. Measures the throughput ceiling on a single machine.
- **PyO3 callbacks** (LoadPilot only) — measures the throughput cost of enabling Python callbacks (`on_start`, `check_*`). Compares static mode (pure Rust) against on_start-only and on_start+check_* modes.

## Setup

**Requirements:**
- Docker with Compose v2 (`docker compose`, not `docker-compose`)
- Python 3.x (for the report script)

**Tools under test:**
- LoadPilot (static mode — no Python callbacks, pure Rust HTTP engine)
- [k6](https://k6.io/) v0.55+
- [Locust](https://locust.io/) v2.x

**Target server:** Rust/axum echo server built and run in Docker. `GET /health` returns `{"status":"ok"}`. Fast enough that it does not become the bottleneck at these RPS levels.

All containers share the same Docker bridge network. Tools run sequentially with a 10s cooldown between runs.

## Running

```bash
cd bench
./run.sh
```

This will:
1. Build the target server and LoadPilot Docker images
2. Start the target server
3. Run each tool sequentially for both scenarios (6 runs total)
4. Generate `results/report.html`

First run takes longer — Rust compilation is cached after that.

## Results

```
Precision (500 RPS target):
  Tool          RPS actual   p50     p95     p99    errors
  LoadPilot        500      3ms     6ms    11ms      0%
  k6               500      1ms     3ms     8ms      0%
  Locust           497    170ms   360ms  1500ms      0%

Max throughput:
  Tool          RPS actual   p50     p95     p99    errors
  LoadPilot       3326     19ms   192ms   512ms      0%
  k6              1617     17ms   113ms   164ms      0%
  Locust           694     99ms   130ms   160ms      0%

PyO3 callbacks — cost of Python (500 RPS target):
  Mode                         RPS actual   p50     p99    errors
  LoadPilot static                 500      3ms    11ms      0%
  + on_start (login per VUser)     351      1ms     3ms      0%
  + on_start + check_*             351      1ms     3ms      0%
```

PyO3 mode achieves ~70% of the target RPS (351/500) due to GIL serialisation — Python callbacks run one at a time. Individual request latency is actually lower (1ms vs 3ms static) because fewer requests are in flight. Adding `check_*` on top of `on_start` has no additional throughput cost in this benchmark.

## Methodology notes

**Why Docker?**
Reproducible on any machine with Docker. The Docker bridge network adds a small fixed overhead equally for all tools, so relative comparisons remain valid.

**Why sequential runs?**
Running tools simultaneously would saturate the target server and mix results. Sequential runs with a cooldown give each tool a clean slate.

**LoadPilot static mode**
The benchmark uses scenarios with no `on_start` or `check_*` callbacks. In this mode LoadPilot runs as pure Rust — no Python is invoked during the test. This is the equivalent of k6/Locust scenarios with no custom logic beyond the HTTP call.

When `on_start` or `check_*` are present, LoadPilot uses the PyO3 bridge and throughput will be lower (GIL is acquired per callback). This is intentional — the Python DSL trades some throughput for flexibility.

**Locust latency in precision mode**
Locust achieves the target RPS but adds significant latency overhead (p99=1500ms at 500 RPS). This is not a network issue — it is the cost of the Python/GIL scheduler dispatching requests. At higher RPS this overhead grows.

**LoadPilot max throughput ceiling**
The clean ceiling is ~3300 RPS in this Docker setup. Above ~4000 RPS the Docker bridge network itself starts to queue, which shows up as rising p99. This is a Docker networking constraint, not a LoadPilot limitation — on bare metal the ceiling is higher.

## Tweaking the benchmark

Scenarios are in `bench/scenarios/`:

```
bench/scenarios/
  precision/
    loadpilot.py   ← rps=500, mode="constant"
    locust.py
    k6.js
  max/
    loadpilot.py   ← rps=3500, mode="constant"
    locust.py
    k6.js
```

Edit the RPS targets or duration, then re-run `./run.sh`. The report is regenerated automatically.

To run a single tool:

```bash
cd bench

# LoadPilot precision only
docker compose --profile loadpilot-precision run --rm loadpilot-precision

# k6 max only
docker compose --profile k6-max run --rm k6-max

# Regenerate report from existing results
python3 report.py
```
