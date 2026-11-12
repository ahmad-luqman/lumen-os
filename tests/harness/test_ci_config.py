"""The build and CI wiring that no other test exercises: scenarios run in
parallel by default, and CI's feature gate (which writes the job summary) also
runs after failed scenarios."""

import re
import subprocess

from lumen_harness import REPO_ROOT


def scenario_command(*make_args: str) -> str:
    """The pytest line `make test-scenarios` would run (`make -n` runs nothing)."""
    out = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), "test-scenarios", *make_args],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    [line] = [line for line in out.splitlines() if "pytest tests/scenarios" in line]
    return line


def test_scenarios_run_in_parallel_by_default():
    assert re.search(r" -n auto\b", scenario_command())
    assert re.search(r" -n 0\b", scenario_command("SCENARIO_JOBS=0"))


def ci_steps() -> list[str]:
    text = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
    return re.split(r"\n(?=      - )", text)


def test_feature_gate_runs_after_failed_scenarios():
    steps = ci_steps()
    [scenarios] = [s for s in steps if "run: make test-scenarios" in s]
    [gate] = [s for s in steps if "run: make status-check" in s]
    assert "id: scenarios" in scenarios
    assert "if: ${{ !cancelled() && steps.scenarios.outcome != 'skipped' }}" in gate
