---
title: Development
nav_order: 8
layout: default
---

# Development

## Prerequisites

- Python 3.12+
- Rust 1.85+ (`rustup` recommended)
- [uv](https://github.com/astral-sh/uv)

## Building from Source

```bash
git clone https://github.com/VladislavAkulich/loadpilot.git
cd loadpilot

# Build Rust coordinator + agent
cd engine && cargo build --release && cd ..

# Install Python CLI in editable mode
cd cli && uv pip install -e .
```

The coordinator binary is picked up from `engine/target/release/coordinator`
by the CLI at runtime when running from source.

## Running Tests

### Python tests

```bash
cd cli
uv sync --extra dev
uv run pytest tests/ -v
```

Coverage is reported automatically after every run (configured in `pyproject.toml`):

```
Name                    Stmts   Miss  Cover   Missing
-----------------------------------------------------
loadpilot/models.py        46      2    96%   ...
loadpilot/dsl.py           47      6    87%   ...
...
```

HTML coverage report is written to `cli/htmlcov/index.html`.

Integration tests require the coordinator binary to be built:

```bash
cd engine && cargo build  # or cargo build --release
```

Tests skip automatically with a clear message if the binary is not found.

### Rust tests + coverage

```bash
cd engine

# run unit tests
cargo test

# unit tests + coverage summary (requires cargo-llvm-cov)
cargo cov

# unit tests + HTML coverage report → target/llvm-cov/html/index.html
cargo cov-html
```

Install `cargo-llvm-cov` if not present:

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
```

## Benchmark

```bash
cd bench
./run.sh
```

Runs LoadPilot, k6, and Locust sequentially against a Rust/axum echo server in Docker
and generates `results/report.html`. See [benchmark.md](benchmark.md) for full details.

## Project Structure

```
loadpilot/
  cli/                    ← Python package (pip install loadpilot)
    loadpilot/
      cli.py              ← CLI entry point, _build_plan()
      dsl.py              ← @scenario, @task, VUser, _scenarios registry
      models.py           ← Pydantic models: ScenarioPlan, AgentMetrics, ...
      client.py           ← LoadClient (httpx wrapper for on_start)
      _bridge.py          ← MockClient (used by _build_plan to extract URLs)
      report.py           ← HTML report generator
    tests/
      test_integration.py ← End-to-end: Python plan → Rust coordinator subprocess
      test_cli_plan.py    ← Unit tests for _build_plan() scenario selection logic

  engine/                 ← Rust workspace
    coordinator/src/
      coordinator.rs      ← Main run loop, token-bucket scheduler
      python_bridge.rs    ← PyO3 bridge, VUser threads, RustClient
      metrics.rs          ← Histogram, AgentMetrics, JSON serialisation
      plan.rs             ← ScenarioPlan deserialization + validation
      distributed.rs      ← NATS integration, agent coordination
      broker.rs           ← Embedded NATS broker
    agent/                ← Standalone agent binary (for remote machines)

  bench/                  ← Benchmark suite
    scenarios/            ← LoadPilot / k6 / Locust scenario files
    run.sh                ← Orchestration script
    report.py             ← HTML report generator

  docs/                   ← Documentation
```
