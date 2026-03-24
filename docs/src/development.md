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

# run unit tests (coordinator + agent)
cargo test

# agent-only tests
cargo test --package agent

# unit tests + coverage summary (requires cargo-llvm-cov)
cargo cov

# unit tests + HTML coverage report → target/llvm-cov/html/index.html
cargo cov-html
```

The agent test suite (`engine/agent/src/runner.rs`) covers:

| Test | What it guards |
|---|---|
| `budget_low_rps_regression` | `round()` → 0 bug at low RPS; budget accumulation fix |
| `budget_matches_target_rps_over_one_second` | Correct request rate for 1–100 RPS |
| `budget_residual_bounded` | Budget stays in `[0, 1)` — no runaway accumulation |
| `task_urls_overrides_task_default_url` | Per-VUser URL from `on_start` reaches agents |
| `task_urls_falls_back_to_task_url_when_absent` | Fallback to task's static URL |
| `empty_vuser_configs_uses_task_url` | Pool-size=0 path |
| `ramp/constant/step/spike_mode_*` | All load profile modes |
| `pick_task_respects_weights` | Weighted task selection |
| `ramp_total_duration_*` | Duration includes ramp-up for Ramp mode |

Install `cargo-llvm-cov` if not present:

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
```

## Helm Chart (in progress)

A Helm chart for deploying the distributed agent stack to Kubernetes is located at
`cli/loadpilot/charts/loadpilot/`. It is **not yet published** to a Helm repository
but can be installed directly from the source tree.

### What the chart deploys

| Component | Description |
|---|---|
| `loadpilot-nats` | Embedded NATS broker (single-node) |
| `loadpilot-agent` | N agent pods (configurable replicas) |
| `loadpilot-prometheus` | Prometheus scraping coordinator metrics |
| `loadpilot-grafana` | Grafana with pre-provisioned LoadPilot dashboard |

### Local install (kind / minikube)

```bash
# Build and load images
docker build -f Dockerfile.agent -t loadpilot-agent:local .
docker build -f Dockerfile.coordinator -t loadpilot-coordinator:local .
kind load docker-image loadpilot-agent:local --name <cluster-name>

# Install chart
helm install loadpilot cli/loadpilot/charts/loadpilot \
  --namespace loadpilot --create-namespace \
  --set agent.image=loadpilot-agent \
  --set agent.tag=local

# Forward NATS + Grafana
kubectl port-forward -n loadpilot svc/loadpilot-nats 4222:4222
kubectl port-forward -n loadpilot svc/loadpilot-grafana 3000:3000
```

### Known limitations (not yet production-ready)

- Prometheus scrape target is hardcoded to `host.docker.internal:9090` (coordinator
  runs locally and is not deployed to k8s)
- No `imagePullSecrets` support (add manually for private registries)
- Grafana admin password stored in `values.yaml` plaintext (no Secret)
- No PVC for Grafana / Prometheus — data lost on pod restart
- No `NOTES.txt` post-install instructions

## Benchmark

```bash
cd bench
./run.sh
```

Runs LoadPilot, k6, and Locust sequentially against a Rust/axum echo server in Docker
and generates `results/report.html`. See [Benchmark](benchmark.md) for full details.

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
