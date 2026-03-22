---
title: Distributed Mode
nav_order: 5
---

# Distributed Mode

Run a load test across multiple machines. The CLI output is identical to
single-machine mode — the coordinator aggregates all agent metrics transparently.

---

## Local agents

Spawn N agent processes on the same machine sharing an embedded NATS broker:

```bash
loadpilot run scenarios/checkout.py --target https://api.example.com --agents 4
```

Each agent handles `rps / N` of the total load. Useful for saturating the network
interface or bypassing OS-level connection limits.

---

## External agents — separate machines

### Start agents

Install the agent binary on each machine:

```bash
curl -fsSL https://raw.githubusercontent.com/VladislavAkulich/loadpilot/main/install.sh | sh
```

Start an agent — it connects to the coordinator and waits for a plan:

```bash
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-0
loadpilot-agent --coordinator <coordinator-ip>:4222 --agent-id agent-1
```

Agents are **persistent** — after a run completes they reconnect and wait for the
next plan automatically.

### Run a test

```bash
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --external-agents 2 \
  --report results/report.html
```

The coordinator uses an embedded NATS broker listening on `:4222` by default.

---

## External NATS (Railway / cloud)

Deploy a NATS server separately (e.g. Railway, Fly.io, or a VPS):

```bash
# Deploy NATS: Docker image nats:latest, TCP port 4222

# Start agents with COORDINATOR env var pointing at your NATS
COORDINATOR=your-nats.railway.app:PORT AGENT_ID=agent-0 loadpilot-agent

# Run test with external NATS
loadpilot run scenarios/checkout.py \
  --target https://api.example.com \
  --nats-url nats://your-nats.railway.app:PORT \
  --external-agents 2 \
  --report results/report.html
```

---

## `on_start` in distributed mode

When a scenario uses `on_start` (e.g. login → per-user auth token), the coordinator
runs `on_start` N times locally against the target before the test begins. It
captures the headers each VUser would set and ships them with the plan. Agents
rotate through these pre-authenticated header sets in pure Rust — no Python required
on agent machines.

```python
@scenario(rps=100, duration="2m")
class CheckoutFlow(VUser):
    def on_start(self, client):
        resp = client.post("/auth/login", json={"user": "test", "pass": "secret"})
        self.token = resp.json()["access_token"]

    @task
    def browse(self, client):
        # self.token from on_start is captured and shipped to agents automatically
        client.get("/api/products", headers={"Authorization": f"Bearer {self.token}"})
```

---

## Reliability guarantees

- **Synchronised start** — all agents begin within ~1ms of each other. The
  coordinator sends a `start_at` timestamp; agents sleep until it fires.
- **Agent timeout** — if an agent stops reporting for 15s it is marked timed-out;
  the test continues on remaining agents without hanging.
- **Agent recovery** — if a timed-out agent reconnects mid-test it is restored to
  the active pool.

---

## Architecture

```
CLI (Python)
  build plan → spawn coordinator
        │
        ▼ stdin (JSON)
Coordinator (Rust)
  ├── embedded NATS broker  (or connect to external NATS)
  ├── wait for N agents to register
  ├── shard plan + set synchronised start_at → publish to each agent
  ├── aggregate metrics (sum RPS, histogram-merged percentiles)
  ├── stdout JSON lines → CLI live dashboard
  └── :9090/metrics → Prometheus / Grafana

Agent (Rust, one per machine)
  ├── connect to NATS → register → receive shard
  ├── sleep until start_at (clock sync)
  ├── run HTTP load (token-bucket + reqwest)
  ├── stream metrics → NATS → coordinator
  └── reconnect and wait for next plan
```
