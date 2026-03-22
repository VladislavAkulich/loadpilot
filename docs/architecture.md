---
title: Architecture
nav_order: 7
layout: default
---

# Architecture

## Overview

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
              — 97% of static ceiling at batch size 5
        │
        ├── stdout JSON lines (1/sec) → CLI live dashboard
        └── :9090/metrics → Prometheus / Grafana
```

---

## Static vs PyO3 mode

The coordinator runs in one of two modes, selected automatically by the CLI:

**Static mode** — pure Rust, maximum throughput. Activated when the scenario has
no `on_start`, `on_stop`, or `check_*` methods. The coordinator fires HTTP requests
directly via reqwest without touching Python at runtime. No GIL, no thread overhead.

**PyO3 mode** — activated when any of the following are present:
- `on_start` / `on_stop` lifecycle hooks
- `check_{task}` assertion methods
- `async def` task functions
- `client.batch()` calls

In PyO3 mode the coordinator spawns one OS thread per virtual user. Each thread
holds a persistent Python interpreter attachment (`Python::attach` once per task
message, not once per thread). HTTP I/O releases the GIL via `py.detach()`, so all
VUser threads run their HTTP requests concurrently even under Python's GIL.

---

## PyO3 bridge details

### GIL strategy

```
VUser thread (OS thread, persistent)
  Python::attach                ← GIL acquired once per task message
    call task method(client)    ← Python executes task body
      py.detach()               ← GIL released
        reqwest HTTP            ← concurrent with all other VUser threads
      GIL re-acquired           ← back in Python callback
    call check_{task}(status, body)   ← Python assertion
  Python released               ← GIL free until next task message
```

### Async task fast path

`async def` tasks are driven with `coro.send(None)` rather than
`asyncio.run_until_complete`. For sync-body coroutines (the common case where the
task body doesn't contain real `await` expressions) this avoids the asyncio
scheduler entirely — roughly 10µs vs 200µs per coroutine. The coordinator
automatically falls back to `run_until_complete` when `coro.send(None)` raises
`StopIteration` before the coroutine is exhausted, i.e. when real `await` is used.

### `check_*` implementation

JSON is pre-parsed inside `py.detach()` (pure Rust, no GIL) and stored in a cache
on the response object. When `check_{task}` is called, it receives a plain Python
`int` (status code) and a plain Python `dict` (pre-built from the parsed JSON) —
no wrapper object, no descriptor-protocol overhead. This adds only ~4% latency vs
a task with no check method.

### `client.batch()` implementation

`client.batch([...])` dispatches N HTTP requests concurrently via a tokio `JoinSet`
inside a single `py.detach()` block. The GIL is released for the entire batch —
PyO3 overhead is paid once per N requests rather than once per request. At batch
size 5 this reaches 97% of static-mode ceiling.

---

## Metrics pipeline

The coordinator emits one JSON line per second to stdout. The Python CLI parses
these lines and renders the live TUI dashboard. Each line is an `AgentMetrics`
object:

```json
{
  "timestamp_secs": 1234567890.0,
  "elapsed_secs": 12.5,
  "current_rps": 100.2,
  "target_rps": 100.0,
  "requests_total": 1250,
  "errors_total": 0,
  "active_workers": 1,
  "phase": "steady",
  "latency": {
    "p50_ms": 12.0,
    "p95_ms": 28.0,
    "p99_ms": 41.0,
    "max_ms": 203.0,
    "min_ms": 4.0,
    "mean_ms": 14.2
  }
}
```

Latency percentiles use a histogram with power-of-two bucket boundaries. In
distributed mode, histograms from all agents are merged before computing percentiles
— this gives exact (not estimated) percentiles across the fleet.

Simultaneously, the coordinator exposes the same metrics on `:9090/metrics` in
Prometheus format for live Grafana dashboards.
