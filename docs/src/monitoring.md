# Monitoring

LoadPilot exposes a Prometheus metrics endpoint and ships a pre-provisioned
Grafana dashboard that updates in real time during a test run.

---

## Grafana Dashboard

![LoadPilot Grafana Dashboard](images/grafana.png)

The dashboard has three sections:

| Section | Panels |
|---|---|
| **Throughput** | RPS — Current vs Target, Active Workers |
| **Latency** | Latency Percentiles (p50 / p95 / p99 / max), p99 gauge |
| **Errors** | Error Rate %, Cumulative Requests & Errors |

---

## Prometheus metrics

The coordinator exposes metrics at `:9090/metrics` during a run:

| Metric | Description |
|---|---|
| `loadpilot_current_rps` | Observed request rate |
| `loadpilot_target_rps` | Configured target RPS |
| `loadpilot_active_workers` | Active VUser threads |
| `loadpilot_latency_p50_ms` | p50 latency (ms) |
| `loadpilot_latency_p95_ms` | p95 latency (ms) |
| `loadpilot_latency_p99_ms` | p99 latency (ms) |
| `loadpilot_latency_max_ms` | Max latency (ms) |
| `loadpilot_requests_total` | Cumulative request count |
| `loadpilot_errors_total` | Cumulative error count |

---

## Local setup (single machine)

```bash
loadpilot run scenarios/checkout.py --target https://api.example.com
```

The coordinator starts automatically. Forward Prometheus and open Grafana:

```bash
# Grafana ships embedded in the HTML report — open after the run:
open results/report.html

# Or run Prometheus + Grafana separately and point them at :9090
```

---

## Kubernetes (Helm)

The Helm chart deploys Prometheus and Grafana with the dashboard
pre-provisioned. See [Development → Helm Chart](development.md#helm-chart-in-progress)
for install instructions.

```bash
# Forward Grafana to localhost
kubectl port-forward -n loadpilot svc/loadpilot-grafana 3000:3000
```

Then open [http://localhost:3000](http://localhost:3000) — login `admin` / `<adminPassword>`.

To scrape the coordinator running locally:

```bash
helm upgrade loadpilot cli/loadpilot/charts/loadpilot \
  --set monitoring.coordinator.scrapeTarget=host.docker.internal:9090
```
