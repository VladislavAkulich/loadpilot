# CLI Reference

## Commands

```bash
loadpilot run [SCENARIO_FILE] [OPTIONS]
loadpilot compare CURRENT [BASELINE] [OPTIONS]
loadpilot init [DIRECTORY]
loadpilot version
```

---

## `loadpilot run`

Run a load test scenario.

Omit `SCENARIO_FILE` to open the interactive scenario browser (requires a TTY).

```bash
loadpilot run scenarios/checkout.py --target https://api.example.com
```

### Options

| Flag                | Default                 | Description |
|---------------------|-------------------------|-------------|
| `--target`          | `http://localhost:8000` | Base URL of the system under test |
| `--scenario`        | —                       | Scenario class name (required when a file defines multiple `@scenario` classes) |
| `--report`          | off                     | Write an HTML report to this path after the test |
| `--dry-run`         | off                     | Print the generated plan JSON and exit without running |
| `--agents`          | `1`                     | Spawn N local agent processes (embedded NATS) |
| `--external-agents` | `0`                     | Wait for N externally started agents to connect before starting |
| `--nats-url`        | —                       | Connect to an external NATS server (use with `--external-agents`) |
| `--threshold`       | from `@scenario`        | Override an SLA threshold at run time: `--threshold p99_ms=500` |
| `--results-json`    | off                     | Write final metrics as JSON to this path |
| `--save-baseline`   | off                     | Save results as baseline to `.loadpilot/baseline.json` |
| `--coordinator-url` | —                       | URL of an in-cluster coordinator (`POST /run`). When set the coordinator runs as a k8s pod instead of a local subprocess. Also readable from `LOADPILOT_COORDINATOR_URL`. |

### Examples

```bash
# basic run
loadpilot run scenarios/checkout.py --target https://api.example.com

# save HTML report
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --report results/report.html

# override threshold without editing the file
loadpilot run scenarios/checkout.py \
  --target https://staging.example.com \
  --threshold p99_ms=800

# distributed — 4 local processes
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --agents 4

# distributed — external agents
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --external-agents 2 \
  --report results/report.html

# dry-run: inspect the generated plan
loadpilot run scenarios/checkout.py --target https://api.example.com --dry-run

# save baseline for future comparisons
loadpilot run scenarios/checkout.py --target https://api.example.com --save-baseline

# run via in-cluster coordinator (k8s)
kubectl port-forward -n loadpilot svc/loadpilot-coordinator 8080:8080
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --coordinator-url http://localhost:8080
```

---

## `loadpilot compare`

Compare two results JSON files and show metric deltas. Useful for detecting
regressions after a deploy or code change.

```bash
loadpilot compare current.json
loadpilot compare current.json baseline.json
```

If `BASELINE` is omitted, LoadPilot reads `.loadpilot/baseline.json` (saved by `--save-baseline`).

### Options

| Flag            | Default | Description |
|-----------------|---------|-------------|
| `--threshold`   | `10`    | Fail with exit code 1 if any metric regressed by more than this % |

### Output

```
Comparing baseline.json → current.json

                    baseline       current         diff
  RPS actual          498.2         501.1        +0.6%
  p50 latency           12ms          9ms       -25.0%
  p95 latency           28ms         25ms       -10.7%
  p99 latency           41ms         38ms        -7.3%
  error rate           0.00%        0.00%           —

No regressions detected.
```

Green = improvement, red = regression. Exit code `1` if any metric regressed
beyond `--threshold`.

### Typical workflow

```bash
# 1. save baseline once (or after intentional improvement)
loadpilot run scenarios/checkout.py --save-baseline

# 2. run after changes
loadpilot run scenarios/checkout.py --results-json current.json

# 3. compare
loadpilot compare current.json

# 4. in CI — fail on > 5% regression
loadpilot compare current.json --threshold 5
```

`.loadpilot/baseline.json` is gitignored by default. Commit it if you want
to share the baseline with your team.

---

## `loadpilot init`

Scaffold a new load test project.

```bash
loadpilot init my-load-tests
cd my-load-tests
```

Creates:

```
my-load-tests/
  scenarios/
    example.py               ← starter scenario (edit this)
  monitoring/
    docker-compose.yml       ← Prometheus + Grafana, pre-configured
    grafana-dashboard.json   ← LoadPilot dashboard, auto-imported on first start
  .env.example
```

Safe to run on an existing directory — does not overwrite files that already exist.

### Start live monitoring

```bash
docker compose -f monitoring/docker-compose.yml up -d
# Grafana    → http://localhost:3000  (admin / admin)
# Prometheus → http://localhost:9091
```

The LoadPilot dashboard auto-imports on first start. It shows RPS (actual vs target),
latency percentiles, active workers, and error rate — updated every 2 seconds while
a test runs.

> Requires Docker with Compose v2 (`docker compose`, not `docker-compose`).

---

## `loadpilot version`

Print the installed version and exit.

```bash
loadpilot version
```
