"""The build and CI wiring that no other test exercises: scenarios run in
parallel by default, and CI's feature gate (which writes the job summary) also
runs after failed scenarios."""

import os
import re
import subprocess
import sys

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


def ci_step(needle: str) -> str:
    """The one ci.yml step that contains `needle`."""
    [step] = [s for s in ci_steps() if needle in s]
    return step


def test_tcg_rerun_only_reruns_failed_scenarios():
    rerun = ci_step("Re-run failed scenarios under TCG")
    assert "if: ${{ failure() && steps.scenarios.outcome == 'failure' }}" in rerun
    assert "id: scenarios" in ci_step("run: make test-scenarios")
    assert "LUMEN_ACCEL=tcg LUMEN_ARTIFACTS_DIR=build/test-artifacts-tcg" in rerun
    assert "pytest tests/scenarios --last-failed --lfnf=none -v" in rerun
    assert "--junitxml=build/test-artifacts-tcg/scenarios.xml" in rerun
    assert "timeout-minutes: 10" in rerun
    assert "|| true" not in rerun and '*) echo "::warning::TCG re-run broke' in rerun
    # The re-run's root is uploaded, after the re-run.
    steps = ci_steps()
    upload = ci_step("Upload test artifacts on failure")
    assert "build/test-artifacts-tcg" in upload and steps.index(rerun) < steps.index(upload)


def pytest_in(directory, *args: str) -> subprocess.CompletedProcess:
    """pytest (this environment's) run in `directory`, with its own cache."""
    return subprocess.run(
        [sys.executable, "-m", "pytest", "-p", "no:xdist", "-o", "addopts=", *args],
        cwd=directory,
        capture_output=True,
        text=True,
        check=False,
    )


def test_last_failed_behaves_as_the_rerun_step_relies_on(tmp_path):
    # The pinned pytest's --last-failed/--lfnf semantics, which the re-run
    # step's condition and flags depend on (a pytest upgrade could change them).
    (tmp_path / "harness").mkdir()
    (tmp_path / "scenarios").mkdir()
    (tmp_path / "harness/test_h.py").write_text("def test_h():\n    assert False\n")
    (tmp_path / "scenarios/test_s.py").write_text(
        "import os\n\ndef test_ok():\n    pass\n\n"
        "def test_flaky():\n    assert os.environ.get('PASS')\n"
    )
    rerun = ("scenarios", "--last-failed", "--lfnf=none", "-q")
    # Nothing recorded yet: nothing re-run (exit 5).
    nothing = pytest_in(tmp_path, *rerun)
    assert nothing.returncode == 5, nothing.stdout
    # A failure recorded outside the selection: every selected test runs,
    # --lfnf notwithstanding. Hence the step runs only after failed scenarios.
    pytest_in(tmp_path, "harness")
    leak = pytest_in(tmp_path, *rerun)
    assert "2 passed" in leak.stdout or "1 failed, 1 passed" in leak.stdout, leak.stdout
    # A failed scenario recorded: only it is re-run.
    (tmp_path / ".pytest_cache").rename(tmp_path / "old-cache")
    pytest_in(tmp_path, "scenarios")
    only = pytest_in(tmp_path, *rerun)
    assert "1 failed" in only.stdout and "passed" not in only.stdout, only.stdout


def test_feature_gate_runs_after_failed_scenarios():
    steps = ci_steps()
    [scenarios] = [s for s in steps if "run: make test-scenarios" in s]
    [gate] = [s for s in steps if "run: make status-check" in s]
    assert "id: scenarios" in scenarios
    assert "if: ${{ !cancelled() && steps.scenarios.outcome != 'skipped' }}" in gate
