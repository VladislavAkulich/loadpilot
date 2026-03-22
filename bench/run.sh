#!/usr/bin/env bash
# Run the full benchmark suite.
#
# Precision (500 RPS):  LoadPilot static, k6, Locust, LoadPilot PyO3 x2
# Max throughput:       LoadPilot static, k6, Locust, LoadPilot PyO3 x4
#
# Tools run sequentially; target server stays up throughout.
#
# Usage:
#   cd bench && ./run.sh
#
# Requirements: Docker with Compose v2, Python 3.x (for report.py)

set -euo pipefail
cd "$(dirname "$0")"

COOLDOWN=10

run() {
    local profile="$1"
    local label="$2"
    echo "  → $label"
    docker compose --profile "$profile" run --rm "$profile"
}

cooldown() {
    echo "  cooling down ${COOLDOWN}s..."
    sleep "$COOLDOWN"
}

echo "=== LoadPilot Benchmark ==="
echo

echo "[1/4] Building images..."
docker compose build
echo

echo "[2/4] Starting target server..."
docker compose up -d --wait target
echo "  target ready"
echo

# ── Precision (500 RPS) ───────────────────────────────────────────────────────
echo "[3/4] Precision (target: 500 RPS, 30s)"

run loadpilot-precision   "LoadPilot static"      ; cooldown
run locust-precision      "Locust"                ; cooldown
run k6-precision          "k6"                    ; cooldown
run loadpilot-pyo3-onstart "LoadPilot PyO3 on_start" ; cooldown
run loadpilot-pyo3-full   "LoadPilot PyO3 full"
echo

# ── Max throughput ────────────────────────────────────────────────────────────
echo "[3/4] Max throughput (30s, no RPS cap)"

run loadpilot-max         "LoadPilot static"      ; cooldown
run locust-max            "Locust"                ; cooldown
run k6-max                "k6"                    ; cooldown
run loadpilot-pyo3-max-onstart "LoadPilot PyO3 on_start (max)" ; cooldown
run loadpilot-pyo3-max-full   "LoadPilot PyO3 full (max)"     ; cooldown
run loadpilot-pyo3-max-sync   "LoadPilot PyO3 sync (max)"     ; cooldown
run loadpilot-pyo3-batch5     "LoadPilot PyO3 batch×5"
echo

# ── Report ────────────────────────────────────────────────────────────────────
echo "[4/4] Generating report..."
python3 report.py
echo

docker compose down
echo "Done. Open results/report.html"
