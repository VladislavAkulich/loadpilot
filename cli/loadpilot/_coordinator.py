"""Coordinator binary discovery and scenario file loading."""

from __future__ import annotations

import importlib.util
import shutil
import sys
from pathlib import Path

from loadpilot.dsl import _clear_scenarios

_EXE = ".exe" if sys.platform == "win32" else ""

COORDINATOR_BINARY_CANDIDATES = [
    # 1. Bundled inside the installed wheel (pip install loadpilot).
    Path(__file__).parent / f"coordinator{_EXE}",
    # 2. Local dev build (cargo build --release inside engine/).
    Path(__file__).parent.parent.parent / "engine" / "target" / "release" / f"coordinator{_EXE}",
    # 3. Local dev build debug.
    Path(__file__).parent.parent.parent / "engine" / "target" / "debug" / f"coordinator{_EXE}",
    # 4. System PATH.
    Path(shutil.which("loadpilot-coordinator") or ""),
]


def _find_coordinator_binary() -> Path:
    for candidate in COORDINATOR_BINARY_CANDIDATES:
        p = Path(str(candidate))
        if p.exists() and p.is_file():
            return p
    raise FileNotFoundError(
        "Could not find the loadpilot-coordinator binary.\n"
        "Build it with:  cargo build --release  inside engine/\n"
        "Or add it to your PATH as 'loadpilot-coordinator'."
    )


def _load_scenario_file(scenario_file: Path) -> None:
    """Import the scenario file so its @scenario decorators populate _scenarios."""
    _clear_scenarios()
    scenario_dir = str(scenario_file.parent.resolve())
    if scenario_dir not in sys.path:
        sys.path.insert(0, scenario_dir)

    spec = importlib.util.spec_from_file_location("_loadpilot_scenario", scenario_file)
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load scenario file: {scenario_file}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["_loadpilot_scenario"] = module
    spec.loader.exec_module(module)  # type: ignore[union-attr]
