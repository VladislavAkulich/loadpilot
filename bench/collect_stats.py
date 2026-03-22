#!/usr/bin/env python3
"""Sample docker stats for one container until it exits, then write a summary JSON.

Usage:
    python3 collect_stats.py <container_id> <output_path>

Output JSON fields:
    cpu_avg_pct   — average CPU % over all samples
    cpu_peak_pct  — maximum CPU % observed
    mem_peak_mb   — peak RSS in MiB
    samples       — number of 1-second samples collected
"""
from __future__ import annotations

import json
import subprocess
import sys
import time


def _parse_mem_mib(s: str) -> float:
    """Parse the 'used' side of '123.4MiB / 7.5GiB' → float MiB."""
    used = s.split("/")[0].strip()
    for suffix, factor in [("GiB", 1024), ("GB", 1024), ("MiB", 1), ("MB", 1), ("KiB", 1/1024), ("kB", 1/1024)]:
        if suffix in used:
            return float(used.replace(suffix, "").strip()) * factor
    return 0.0


def main() -> None:
    if len(sys.argv) < 3:
        print("Usage: collect_stats.py <container_id> <output_path>", file=sys.stderr)
        sys.exit(1)

    cid, out_path = sys.argv[1], sys.argv[2]
    cpu_samples: list[float] = []
    mem_peak_mib = 0.0

    while True:
        try:
            r = subprocess.run(
                ["docker", "stats", cid, "--no-stream", "--format", "{{.CPUPerc}},{{.MemUsage}}"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            if r.returncode != 0 or not r.stdout.strip():
                break
            # Take the last non-empty line (docker stats sometimes emits a header)
            line = r.stdout.strip().split("\n")[-1]
            cpu_str, mem_str = line.split(",", 1)
            cpu = float(cpu_str.replace("%", "").strip())
            mem = _parse_mem_mib(mem_str)
            cpu_samples.append(cpu)
            mem_peak_mib = max(mem_peak_mib, mem)
        except Exception:
            break
        time.sleep(1.0)

    result = {
        "cpu_avg_pct": round(sum(cpu_samples) / len(cpu_samples), 1) if cpu_samples else None,
        "cpu_peak_pct": round(max(cpu_samples), 1) if cpu_samples else None,
        "mem_peak_mb": round(mem_peak_mib, 1),
        "samples": len(cpu_samples),
    }
    with open(out_path, "w") as f:
        json.dump(result, f)


if __name__ == "__main__":
    main()
