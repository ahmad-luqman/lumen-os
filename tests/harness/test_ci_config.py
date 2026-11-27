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


_USES_RE = re.compile(r"^\s*(?:-\s+)?uses:\s*(\S+)\s*$", re.MULTILINE)
_ALPINE_DIGEST_RE = re.compile(r"docker\.io/library/alpine:3\.21@sha256:[0-9a-f]{64}")


def _workflow_actions(text: str) -> dict[str, set[str]]:
    """Map action name (before `@`) to the set of `@ref` values in a workflow."""
    actions: dict[str, set[str]] = {}
    for ref in _USES_RE.findall(text):
        name, sep, pin = ref.partition("@")
        actions.setdefault(name, set()).add(pin if sep else "")
    return actions


def _syscall_census_pin_problems(
    workflow_text: str, ci_text: str, census_sh_text: str
) -> list[str]:
    problems: list[str] = []
    script_pin = re.search(r"^IMAGE=(\S+)$", census_sh_text, re.MULTILINE)
    if not script_pin:
        return ["tools/syscall-census.sh has no IMAGE= pin"]
    expected_image = script_pin.group(1)
    if not _ALPINE_DIGEST_RE.fullmatch(expected_image):
        problems.append(f"script IMAGE is not a sha256-pinned alpine:3.21 digest: {expected_image}")

    wf_images = set(_ALPINE_DIGEST_RE.findall(workflow_text))
    if wf_images != {expected_image}:
        problems.append(
            f"workflow alpine digest {sorted(wf_images)} != script IMAGE {expected_image}"
        )

    ci_actions = _workflow_actions(ci_text)
    wf_actions = _workflow_actions(workflow_text)
    if not wf_actions:
        problems.append("workflow uses no actions")
    for action, pins in sorted(wf_actions.items()):
        for pin in sorted(pins):
            if not pin or pin in {"main", "master", "HEAD", "latest"}:
                problems.append(f"{action}: unpinned ref {pin!r}")
            elif action in ci_actions and pin not in ci_actions[action]:
                problems.append(
                    f"{action}@{pin} does not match ci.yml's {sorted(ci_actions[action])}"
                )
    return problems


def test_syscall_census_workflow_is_pinned():
    """The unattended syscall-census workflow (#451) runs on ubuntu-24.04 with
    timeouts and if: always() artifact upload, matches tools/syscall-census.sh's
    pinned alpine:3.21 digest, and pins every action as ci.yml does."""
    wf_text = (REPO_ROOT / ".github/workflows/syscall-census.yml").read_text()
    ci_text = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
    sh_text = (REPO_ROOT / "tools/syscall-census.sh").read_text()

    assert _syscall_census_pin_problems(wf_text, ci_text, sh_text) == []
    assert "runs-on: ubuntu-24.04" in wf_text
    assert "workflow_dispatch:" in wf_text
    assert "pull_request:" in wf_text
    assert "tools/census/**" in wf_text
    assert "tools/syscall-census.sh" in wf_text
    assert "contents: read" in wf_text
    assert re.search(r"^\s+timeout-minutes:\s+\d+$", wf_text, re.MULTILINE)

    wf_steps = re.split(r"\n(?=      - )", wf_text)
    [report_step] = [s for s in wf_steps if "| tee" in s]
    assert re.search(r"^\s+shell:\s+bash$", report_step, re.MULTILINE), report_step
    [upload_step] = [s for s in wf_steps if "actions/upload-artifact" in s]
    assert re.search(r"^\s+if:\s+always\(\)$", upload_step, re.MULTILINE), upload_step

    # Negative checks: drifting digest, unpinned action or ci.yml mismatch fails.
    bad_digest = _ALPINE_DIGEST_RE.sub(
        "docker.io/library/alpine:3.21@sha256:" + "0" * 64, wf_text, count=1
    )
    assert _syscall_census_pin_problems(bad_digest, ci_text, sh_text)
    unpinned = wf_text.replace("actions/checkout@v7", "actions/checkout@main", 1)
    assert _syscall_census_pin_problems(unpinned, ci_text, sh_text)
    drifted = wf_text.replace("actions/upload-artifact@v7", "actions/upload-artifact@v4", 1)
    assert _syscall_census_pin_problems(drifted, ci_text, sh_text)
