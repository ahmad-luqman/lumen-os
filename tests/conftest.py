"""Shared pytest fixtures for Lumen.

`boot` starts a headless QEMU machine from one of the ISOs built by
`make iso` and closes it after the test. Each test gets its own artifacts
directory, build/test-artifacts/<module>/<test>/ (unique even when tests run in
parallel under pytest-xdist). Artifacts of passing tests are deleted; failing
tests keep serial.log, qemu.log (with the `-d int` interrupt trace and CPU
resets), environment.txt and any screenshots, and CI uploads them.
"""

from __future__ import annotations

import os
import re
import shutil
from collections.abc import Callable, Iterator, Sequence
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT, artifact_dir
from lumen_harness.crash import BEGIN, END, PROTECTION_BROKEN
from lumen_harness.qemu import EXIT_CRASH, Machine, default_accel, environment

ISOS = {
    "interactive": BUILD_DIR / "lumen.iso",
    "qemutest": BUILD_DIR / "lumen-qemutest.iso",
    "ktest": BUILD_DIR / "lumen-ktest.iso",
}
# LUMEN_ARTIFACTS_DIR (relative to the repo root) lets a re-run keep its
# artifacts apart from the first run's: CI re-runs KVM failures under TCG.
ARTIFACTS = REPO_ROOT / (os.environ.get("LUMEN_ARTIFACTS_DIR") or BUILD_DIR / "test-artifacts")
GOLDEN_DIR = Path(__file__).parent / "golden"


def pytest_report_header(config: pytest.Config) -> list[str]:
    # Runs once, in the controlling process even under pytest-xdist.
    return [f"lumen {line}" for line in environment(default_accel())]


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[None]):
    report = yield
    if report.when == "call":
        item.stash[_FAILED] = report.failed
    return report


_FAILED = pytest.StashKey[bool]()


@pytest.fixture
def artifacts(request: pytest.FixtureRequest) -> Iterator[Path]:
    path = artifact_dir(ARTIFACTS, request.node.nodeid)
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    yield path
    if not request.node.stash.get(_FAILED, True):
        shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def boot(artifacts: Path) -> Iterator[Callable[..., Machine]]:
    """`boot("qemutest")` -> a started Machine (closed automatically).
    `extra` is appended to QEMU's command line, e.g. `extra=["-smp", "2"]`."""
    machines: list[Machine] = []

    def _boot(variant: str = "qemutest", *, extra: Sequence[str] = ()) -> Machine:
        iso = ISOS[variant]
        if not iso.exists():
            pytest.fail(f"{iso} not found; run `make iso` first")
        sub = artifacts if not machines else artifacts / f"run{len(machines)}"
        m = Machine(iso, sub, trace_interrupts=True, extra_args=extra).start()
        machines.append(m)
        return m

    yield _boot
    for m in machines:
        m.close()


@pytest.fixture
def golden_dir() -> Path:
    return GOLDEN_DIR


@pytest.fixture
def crash_report(boot: Callable[..., Machine]) -> Callable[..., str]:
    """`crash_report("pf")` boots, runs `crash pf` in kmon, checks QEMU exits
    with the crash code, and returns the report (both markers included).
    `at_prompt(m)`, if given, runs at the kmon prompt first (and must leave
    kmon at a prompt), e.g. to read the heap before the crash."""

    def _crash(kind: str, *, at_prompt: Callable[[Machine], None] | None = None) -> str:
        m = boot("qemutest")
        m.expect(r"kmon> ", 60)
        if at_prompt:
            at_prompt(m)
        m.send_line(f"crash {kind}")
        # A deliberate protection fault that doesn't fault says so at once.
        done = m.expect(rf"{re.escape(END)}|{PROTECTION_BROKEN}", 30)
        assert done.group(0) == END, f"no crash: {done.group(0)}"
        assert m.wait_exit(10) == EXIT_CRASH
        out = m.output
        return out[out.index(BEGIN) : out.index(END) + len(END)]

    return _crash
