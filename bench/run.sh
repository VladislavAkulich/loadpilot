#!/usr/bin/env bash
# Run the full benchmark suite:
#   - Precision: all tools target 500 RPS → measures accuracy & latency overhead
#   - Max throughput: each tool runs flat-out → measures ceiling
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

# Coordinator binary must exist before building the Docker image.
COORDINATOR=../engine/target/release/coordinator
if [ ! -f "$COORDINATOR" ]; then
    echo "Building coordinator..."
    (cd ../engine && cargo build --release -p coordinator)
fi

echo "[1/4] Building images..."
docker compose build
echo

echo "[2/4] Starting target server..."
docker compose up -d --wait target
echo "  target ready"
echo

# ── Precision (500 RPS) ────────────────────────────────────────────────────────
echo "[3/4] Precision scenario (target: 500 RPS, 30s)"

run loadpilot-precision "LoadPilot"  ; cooldown
run locust-precision    "Locust"     ; cooldown
run k6-precision        "k6"
echo

# ── Max throughput ─────────────────────────────────────────────────────────────
echo "[3/4] Max throughput scenario (30s, no RPS cap)"

run loadpilot-max "LoadPilot" ; cooldown
run locust-max    "Locust"    ; cooldown
run k6-max        "k6"
echo

# ── PyO3 mode (LoadPilot only) ────────────────────────────────────────────────
echo "[3/4] PyO3 scenario — on_start only (target: 500 RPS, 30s)"

run loadpilot-pyo3-onstart "LoadPilot (on_start only)" ; cooldown

echo "[3/4] PyO3 scenario — on_start + check_* (target: 500 RPS, 30s)"

run loadpilot-pyo3-full "LoadPilot (on_start + check_*)"
echo

# ── Report ─────────────────────────────────────────────────────────────────────
echo "[4/4] Generating report..."
python3 report.py
echo

docker compose down
echo "Done. Open results/report.html"
