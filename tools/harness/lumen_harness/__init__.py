"""Host-side harness for Lumen: boots QEMU, scripts serial, compares screenshots."""

import os
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
BUILD_DIR = REPO_ROOT / "build"


def rerun_dir() -> Path | None:
    """LUMEN_ARTIFACTS_DIR (relative to the repo root), if set: a re-run keeps
    its artifacts and results there, apart from the first run's (CI re-runs
    failed scenarios under TCG). Unset or empty: None."""
    name = os.environ.get("LUMEN_ARTIFACTS_DIR")
    return REPO_ROOT / name if name else None


def artifacts_root() -> Path:
    """Where scenarios keep their artifacts: build/test-artifacts, or a
    re-run's own directory (`rerun_dir`)."""
    return rerun_dir() or BUILD_DIR / "test-artifacts"


def save_ktest_tap(text: str) -> Path:
    """Save the in-kernel tests' TAP: build/test-results/ktest.tap, which
    `make status` reads, or, in a re-run, `ktest.tap` in its own directory,
    so the first run's file (the one the CI job summary reported) survives.
    (So with LUMEN_ARTIFACTS_DIR set on a first run, `make status` finds no
    ktest results and reports the ktests as not run.) Returns the path."""
    rerun = rerun_dir()
    path = rerun / "ktest.tap" if rerun else BUILD_DIR / "test-results" / "ktest.tap"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def artifact_dir(root: Path, nodeid: str) -> Path:
    """Where a test keeps its artifacts: `root/<module>/<test>`, from a pytest
    node ID: `tests/scenarios/test_crash.py::test_fault_produces_symbolized_report[pf]`
    -> `root/test_crash/test_fault_produces_symbolized_report_pf_`.

    The module is part of it because tests run side by side (pytest-xdist): two
    same-named tests in different files must not delete each other's files.
    (Only the file's stem is used, so two test files with one name in different
    directories would still share a directory. Today the fixture's users are
    tests/scenarios and tests/harness/test_artifacts.py, whose stems differ.)"""
    file, _, test = nodeid.partition("::")
    return root / Path(file).stem / re.sub(r"[^A-Za-z0-9_.-]+", "_", test)
