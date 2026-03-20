"""Hatchling build hook — compiles the Rust coordinator and bundles it into the wheel.

During `pip install` from source (or `hatch build` in CI) this hook:
  1. Runs `cargo build --release` for the coordinator crate.
  2. Copies the resulting binary into `loadpilot/` so it is packaged as data.
  3. Tells hatchling that the wheel is platform-specific (not pure Python).

When installing a pre-built wheel from PyPI the hook is NOT invoked — the binary
is already present inside the wheel (baked in by CI).
"""
from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

# Path from cli/ (where pyproject.toml lives) up to the engine workspace root.
_ENGINE_DIR = Path(__file__).parent.parent / "engine"
_PKG_DIR = Path(__file__).parent / "loadpilot"


def _binary_name() -> str:
    return "coordinator.exe" if sys.platform == "win32" else "coordinator"


class CustomBuildHook(BuildHookInterface):
    """Hatchling hook: build + bundle Rust coordinator binary."""

    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict) -> None:
        if self.target_name not in ("wheel", "editable"):
            return

        binary_name = _binary_name()
        dst = _PKG_DIR / binary_name

        # In CI the binary may already be built and copied by the workflow.
        # Skip compilation if it's already there and SKIP_CARGO env var is set.
        if dst.exists() and os.environ.get("LOADPILOT_SKIP_CARGO"):
            self.app.display_info(
                f"[loadpilot] Skipping cargo build — {dst} already exists."
            )
        else:
            self.app.display_info("[loadpilot] Building Rust coordinator (cargo build --release)…")
            env = os.environ.copy()
            # PyO3 0.23 officially supports up to Python 3.13.
            # This flag enables forward-compatibility with newer Python versions
            # via the stable ABI — safe for binary-only use.
            env.setdefault("PYO3_USE_ABI3_FORWARD_COMPATIBILITY", "1")
            subprocess.run(
                ["cargo", "build", "--release", "--package", "coordinator"],
                cwd=str(_ENGINE_DIR),
                env=env,
                check=True,
            )
            src = _ENGINE_DIR / "target" / "release" / binary_name
            shutil.copy2(src, dst)
            dst.chmod(dst.stat().st_mode | 0o111)  # ensure executable bit
            self.app.display_info(f"[loadpilot] Bundled {binary_name} → {dst}")

        # Tell hatchling to include the binary in the wheel as package data.
        build_data["shared_data"] = {}
        build_data["artifacts"] = [str(dst)]

        # Mark the wheel as platform-specific so pip picks the right one.
        build_data["pure_python"] = False
        build_data["infer_tag"] = True
