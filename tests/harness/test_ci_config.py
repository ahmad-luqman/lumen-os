"""The build and CI wiring that no other test exercises: scenarios run in
parallel by default, CI's feature gate (which writes the job summary) also
runs after failed scenarios, and CI's TCG re-run of failed scenarios (its
step's shell, and the pinned pytest's --last-failed behaviour it relies on)."""

import os
import re
import subprocess
import sys
import textwrap
from pathlib import Path

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


def ci_step(needle: str) -> str:
    """The one ci.yml step that contains `needle`."""
    [step] = [s for s in ci_steps() if needle in s]
    return step


def test_scenarios_run_in_parallel_by_default(monkeypatch):
    # Also from inside `make test SCENARIO_JOBS=0`, which must not fail here.
    monkeypatch.setenv("MAKEFLAGS", "-- SCENARIO_JOBS=0")
    monkeypatch.setenv("SCENARIO_JOBS", "7")
    assert re.search(r" -n auto\b", scenario_command())
    assert re.search(r" -n 0\b", scenario_command("SCENARIO_JOBS=0"))


def test_tcg_rerun_only_reruns_failed_scenarios():
    rerun = ci_step("Re-run failed scenarios under TCG")
    assert "if: ${{ failure() && steps.scenarios.outcome == 'failure' }}" in rerun
    assert "id: scenarios" in ci_step("run: make test-scenarios")
    assert re.search(r"^ +timeout-minutes: 10$", rerun, re.MULTILINE), rerun
    # The re-run's root is uploaded, after the re-run.
    steps = ci_steps()
    upload = ci_step("Upload test artifacts on failure")
    assert "build/test-artifacts-tcg" in upload
    assert steps.index(rerun) < steps.index(upload)


def run_rerun_step(
    tmp_path: Path, uv_exit: int, junit: str = ""
) -> subprocess.CompletedProcess[str]:
    """The re-run step's shell, as GitHub runs it (bash -e), with a stub `uv`
    that records its environment and arguments and exits with `uv_exit` (after
    writing `junit` as the re-run's scenarios.xml, if given)."""
    script = textwrap.dedent(ci_step("Re-run failed scenarios under TCG").split("run: |\n", 1)[1])
    stub = tmp_path / "bin/uv"
    stub.parent.mkdir(exist_ok=True)
    stub.write_text(
        "#!/bin/sh\n"
        'echo "LUMEN_ACCEL=$LUMEN_ACCEL LUMEN_ARTIFACTS_DIR=$LUMEN_ARTIFACTS_DIR uv $*" > uv.args\n'
        'if [ -n "$JUNIT" ]; then mkdir -p build/test-artifacts-tcg; '
        'printf %s "$JUNIT" > build/test-artifacts-tcg/scenarios.xml; fi\n'
        f"exit {uv_exit}\n"
    )
    stub.chmod(0o755)
    env = {**os.environ, "PATH": f"{stub.parent}:{os.environ['PATH']}", "JUNIT": junit}
    return subprocess.run(
        ["bash", "-e", "-c", script],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_tcg_rerun_step_annotates_every_outcome(tmp_path):
    outcomes = {
        0: "::notice::the scenarios that failed under KVM passed under TCG",
        1: "",
        5: "::notice::pytest recorded no failure at all",
        4: "::warning::TCG re-run broke (pytest exit 4)",
        127: "::warning::TCG re-run broke (pytest exit 127)",
    }
    for code, annotation in outcomes.items():
        step = run_rerun_step(tmp_path, code)
        # Never fatal: the job is red already; the step only adds evidence.
        assert step.returncode == 0, (code, step.stdout, step.stderr)
        assert step.stdout.startswith(annotation) if annotation else step.stdout == "", (
            code,
            step.stdout,
        )
    errors = run_rerun_step(tmp_path, 1, junit='<testsuite errors="2" failures="0"></testsuite>')
    assert "::warning::the TCG re-run had setup errors" in errors.stdout, errors.stdout
    # The command it ran: the failed scenarios only, under TCG, into its own root.
    assert (tmp_path / "uv.args").read_text().split() == [
        "LUMEN_ACCEL=tcg",
        "LUMEN_ARTIFACTS_DIR=build/test-artifacts-tcg",
        "uv",
        "run",
        "--frozen",
        "pytest",
        "tests/scenarios",
        "--last-failed",
        "--lfnf=none",
        "-v",
        "--junitxml=build/test-artifacts-tcg/scenarios.xml",
    ]


def pytest_in(directory: Path, *args: str) -> subprocess.CompletedProcess[str]:
    """This environment's pytest, run in `directory` with its own root and
    cache (an empty pytest.ini there) and no inherited PYTEST_ADDOPTS."""
    (directory / "pytest.ini").touch()
    env = {k: v for k, v in os.environ.items() if k != "PYTEST_ADDOPTS"}
    return subprocess.run(
        [sys.executable, "-m", "pytest", *args],
        cwd=directory,
        env=env,
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
        "def test_ok():\n    pass\n\ndef test_fails():\n    assert False\n"
    )
    rerun = ("-p", "no:xdist", "scenarios", "--last-failed", "--lfnf=none", "-q")
    # Nothing recorded yet: nothing re-run (exit 5).
    nothing = pytest_in(tmp_path, *rerun)
    assert nothing.returncode == 5, nothing.stdout + nothing.stderr
    # A failure recorded outside the selection: every selected test runs,
    # --lfnf notwithstanding. Hence the step runs only after failed scenarios.
    pytest_in(tmp_path, "-p", "no:xdist", "harness")
    leak = pytest_in(tmp_path, *rerun)
    assert "1 failed, 1 passed" in leak.stdout, leak.stdout + leak.stderr
    # A fresh cache, then a scenario failure recorded by an xdist run (as
    # `make test-scenarios` records them): only it is re-run.
    first = pytest_in(tmp_path, "scenarios", "--cache-clear", "-n", "2")
    assert "1 failed, 1 passed" in first.stdout, first.stdout + first.stderr
    only = pytest_in(tmp_path, *rerun)
    assert "1 failed" in only.stdout and "passed" not in only.stdout, only.stdout + only.stderr


def test_feature_gate_runs_after_failed_scenarios():
    steps = ci_steps()
    [scenarios] = [s for s in steps if "run: make test-scenarios" in s]
    [gate] = [s for s in steps if "run: make status-check" in s]
    assert "id: scenarios" in scenarios
    assert "if: ${{ !cancelled() && steps.scenarios.outcome != 'skipped' }}" in gate
