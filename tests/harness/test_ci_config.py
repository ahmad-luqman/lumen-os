"""The build and CI wiring that no other test exercises: scenarios run in
parallel by default, and CI's feature gate (which writes the job summary) also
runs after failed scenarios."""

import os
import re
import subprocess

from lumen_harness import REPO_ROOT

# What a calling make (`make test SCENARIO_JOBS=0`) or the shell would pass
# down: the defaults are checked, not this session's settings.
_INHERITED = {"MAKEFLAGS", "MFLAGS", "MAKELEVEL", "MAKEOVERRIDES", "SCENARIO_JOBS"}


def scenario_command(*make_args: str) -> str:
    """The pytest line `make test-scenarios` would run (`make -n` runs nothing)."""
    env = {k: v for k, v in os.environ.items() if k not in _INHERITED}
    run = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), "test-scenarios", *make_args],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )
    assert run.returncode == 0, run.stderr
    lines = [line for line in run.stdout.splitlines() if "pytest tests/scenarios" in line]
    assert len(lines) == 1, run.stdout
    return lines[0]


def ci_steps() -> list[str]:
    text = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
    steps = re.split(r"\n(?=      - )", text)
    assert len(steps) > 5, "ci.yml steps are no longer indented by 6 spaces"
    return steps


def test_scenarios_run_in_parallel_by_default(monkeypatch):
    # Also from inside `make test SCENARIO_JOBS=0`, which must not fail here.
    monkeypatch.setenv("MAKEFLAGS", "-- SCENARIO_JOBS=0")
    monkeypatch.setenv("SCENARIO_JOBS", "7")
    assert re.search(r" -n auto\b", scenario_command())
    assert re.search(r" -n 0\b", scenario_command("SCENARIO_JOBS=0"))


def test_feature_gate_runs_after_failed_scenarios():
    steps = ci_steps()
    [scenarios] = [s for s in steps if "run: make test-scenarios" in s]
    [gate] = [s for s in steps if "run: make status-check" in s]
    assert "id: scenarios" in scenarios
    assert "if: ${{ !cancelled() && steps.scenarios.outcome != 'skipped' }}" in gate
