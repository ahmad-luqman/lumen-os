"""Shared pytest fixtures for Lumen.

`boot` starts a headless QEMU machine from one of the ISOs built by
`make iso` and closes it after the test. Each test gets its own artifacts
directory under build/test-artifacts/<test name>/. Artifacts of passing tests
are deleted; failing tests keep serial.log, qemu.log (with the `-d int`
interrupt trace and CPU resets) and any screenshots, and CI uploads them.
"""

from __future__ import annotations

import os
import re
import shutil
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT
from lumen_harness.qemu import EXIT_CRASH, Machine

ISOS = {
    "interactive": BUILD_DIR / "lumen.iso",
    "qemutest": BUILD_DIR / "lumen-qemutest.iso",
    "ktest": BUILD_DIR / "lumen-ktest.iso",
}
# LUMEN_ARTIFACTS_DIR (relative to the repo root) lets a re-run keep its
# artifacts apart from the first run's: CI re-runs KVM failures under TCG.
ARTIFACTS = REPO_ROOT / (os.environ.get("LUMEN_ARTIFACTS_DIR") or BUILD_DIR / "test-artifacts")
GOLDEN_DIR = Path(__file__).parent / "golden"
CRASH_BEGIN = "==== LUMEN CRASH REPORT ===="
CRASH_END = "==== END CRASH REPORT ===="


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[None]):
    report = yield
    if report.when == "call":
        item.stash[_FAILED] = report.failed
    return report


_FAILED = pytest.StashKey[bool]()


@pytest.fixture
def artifacts(request: pytest.FixtureRequest) -> Iterator[Path]:
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "_", request.node.nodeid.split("::", 1)[-1])
    path = ARTIFACTS / safe
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    yield path
    if not request.node.stash.get(_FAILED, True):
        shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def boot(artifacts: Path) -> Iterator[Callable[[str], Machine]]:
    """`boot("qemutest")` -> a started Machine (closed automatically)."""
    machines: list[Machine] = []

    def _boot(variant: str = "qemutest") -> Machine:
        iso = ISOS[variant]
        if not iso.exists():
            pytest.fail(f"{iso} not found; run `make iso` first")
        sub = artifacts if not machines else artifacts / f"run{len(machines)}"
        m = Machine(iso, sub, trace_interrupts=True).start()
        machines.append(m)
        return m

    yield _boot
    for m in machines:
        m.close()


@pytest.fixture
def golden_dir() -> Path:
    return GOLDEN_DIR


@pytest.fixture
def crash_report(boot: Callable[[str], Machine]) -> Callable[[str], str]:
    """`crash_report("pf")` boots, runs `crash pf` in kmon, checks QEMU exits
    with the crash code, and returns the report (both markers included)."""

    def _crash(kind: str) -> str:
        m = boot("qemutest")
        m.expect(r"kmon> ", 60)
        m.send_line(f"crash {kind}")
        m.expect(re.escape(CRASH_END), 30)
        assert m.wait_exit(10) == EXIT_CRASH
        out = m.output
        return out[out.index(CRASH_BEGIN) : out.index(CRASH_END) + len(CRASH_END)]

    return _crash
