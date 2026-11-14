"""Shared pytest fixtures for Lumen.

`boot` starts a headless QEMU machine from one of the ISOs built by
`make iso` and closes it after the test. Each test gets its own artifacts
directory, build/test-artifacts/<module>/<test>/ (a re-run's, with
LUMEN_ARTIFACTS_DIR set, goes under that root instead): keyed by module too,
so same-named tests in two files can run side by side under pytest-xdist.
Artifacts of passing tests are deleted; failing tests keep serial.log, qemu.log
(CPU resets, and the `-d int` interrupt trace under TCG), cmdline.txt,
environment.txt and any screenshots, and CI uploads them.
"""

from __future__ import annotations

import re
import shutil
import subprocess
from collections.abc import Callable, Iterator, Sequence
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT, artifact_dir, artifacts_root
from lumen_harness.crash import BEGIN, END, PROTECTION_BROKEN
from lumen_harness.qemu import EXIT_CRASH, Machine, default_accel, environment

ISOS = {
    "interactive": BUILD_DIR / "lumen.iso",
    "qemutest": BUILD_DIR / "lumen-qemutest.iso",
    "ktest": BUILD_DIR / "lumen-ktest.iso",
}
# The kernel behind each test ISO, and the command line the Makefile's `iso`
# targets give it (keep in step), for `boot(..., cmdline=...)`.
KERNELS = {"qemutest": BUILD_DIR / "lumen-kernel", "ktest": BUILD_DIR / "lumen-kernel-ktest"}
ISO_CMDLINE = "lumen.qemu_exit"
GOLDEN_DIR = Path(__file__).parent / "golden"


def pytest_report_header(config: pytest.Config) -> list[str]:
    # Under pytest-xdist only the controller's header is shown; workers would
    # each probe QEMU for nothing.
    if hasattr(config, "workerinput"):
        return []
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
    path = artifact_dir(artifacts_root(), request.node.nodeid)
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    yield path
    if not request.node.stash.get(_FAILED, True):
        shutil.rmtree(path, ignore_errors=True)


@pytest.fixture
def boot(artifacts: Path) -> Iterator[Callable[..., Machine]]:
    """`boot("qemutest")` -> a started Machine (closed automatically).
    `extra` is appended to QEMU's command line, e.g. `extra=["-smp", "2"]`;
    `cmdline` to the kernel's, e.g. `cmdline="lumen.keymap=de"`, in an ISO
    built for this test in its artifacts directory (so parallel tests don't
    share one; `qemutest` and `ktest` only). `serial=False` boots without a
    serial port (keyboard, screendumps and the exit code only)."""
    machines: list[Machine] = []

    def _boot(
        variant: str = "qemutest",
        *,
        extra: Sequence[str] = (),
        cmdline: str | None = None,
        serial: bool = True,
    ) -> Machine:
        iso = ISOS[variant]
        if not iso.exists():
            pytest.fail(f"{iso} not found; run `make iso` first")
        sub = artifacts if not machines else artifacts / f"run{len(machines)}"
        if cmdline is not None:
            iso = sub / "custom.iso"
            words = [ISO_CMDLINE, *cmdline.split()]
            subprocess.run(
                [REPO_ROOT / "tools" / "mkiso.sh", KERNELS[variant], iso, *words], check=True
            )
        m = Machine(iso, sub, trace_interrupts=True, extra_args=extra, serial=serial).start()
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
    """`crash_report("pf")` boots, runs `crash pf` in the serial shell, checks
    QEMU exits with the crash code, and returns the report (both markers
    included)."""

    def _crash(kind: str) -> str:
        m = boot("qemutest")
        m.expect(r"lumen> ", 60)
        m.send_line(f"crash {kind}")
        # A deliberate protection fault that doesn't fault says so at once.
        done = m.expect(rf"{re.escape(END)}|{PROTECTION_BROKEN}", 30)
        assert done.group(0) == END, f"no crash: {done.group(0)}"
        assert m.wait_exit(10) == EXIT_CRASH
        out = m.output
        return out[out.index(BEGIN) : out.index(END) + len(END)]

    return _crash
