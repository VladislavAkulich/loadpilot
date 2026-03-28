"""Interactive TUI for browsing and selecting scenarios."""

from __future__ import annotations

from pathlib import Path

from rich.console import Console

from loadpilot._coordinator import _load_scenario_file
from loadpilot.dsl import _scenarios

console = Console()


def _tui_pick_scenario(search_dir: Path) -> tuple[Path, str] | None:
    """Two-step TUI: pick file → pick scenario. Returns (file, scenario_name) or None."""
    import questionary

    # Collect all .py files that contain at least one @scenario
    candidates: list[tuple[Path, list]] = []
    for py_file in sorted(search_dir.rglob("*.py")):
        if py_file.name.startswith("_"):
            continue
        try:
            _load_scenario_file(py_file)
            if _scenarios:
                candidates.append((py_file, list(_scenarios)))
        except Exception:
            pass
        finally:
            _scenarios.clear()

    if not candidates:
        console.print("[yellow]No scenario files found.[/]")
        return None

    _BACK = "__back__"

    while True:
        # Step 1: pick file
        file_choices = [
            questionary.Choice(
                title=f"{f.name:<35} {len(sc)} scenario{'s' if len(sc) != 1 else ''}",
                value=i,
            )
            for i, (f, sc) in enumerate(candidates)
        ]
        file_idx = questionary.select("Select scenario file:", choices=file_choices).ask()
        if file_idx is None:
            return None

        chosen_file, chosen_scenarios = candidates[file_idx]

        # Step 2: pick scenario within that file
        if len(chosen_scenarios) == 1:
            return chosen_file, chosen_scenarios[0].name

        scenario_choices = [
            questionary.Choice(title="← Back", value=_BACK),
            *[
                questionary.Choice(
                    title=f"{s.name:<30} {s.rps} RPS · {s.duration} · {s.mode}",
                    value=s.name,
                )
                for s in chosen_scenarios
            ],
        ]
        chosen_name = questionary.select("Select scenario:", choices=scenario_choices).ask()
        if chosen_name is None:
            return None
        if chosen_name == _BACK:
            continue  # back to file picker

        return chosen_file, chosen_name
