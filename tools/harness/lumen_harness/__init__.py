"""Host-side harness for Lumen: boots QEMU, scripts serial, compares screenshots."""

import os
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
BUILD_DIR = REPO_ROOT / "build"


def rerun_dir() -> Path | None:
    """LUMEN_ARTIFACTS_DIR (relative to the repo root), if set and non-empty:
    a re-run keeps its artifacts and its ktest.tap there, apart from the first
    run's (CI re-runs failed scenarios under TCG). Otherwise None. It may not
    be build/test-results, whose ktest.tap it exists to keep."""
    name = os.environ.get("LUMEN_ARTIFACTS_DIR")
    if not name:
        return None
    path = REPO_ROOT / name
    if path.resolve() == (BUILD_DIR / "test-results").resolve():
        raise ValueError(
            f"LUMEN_ARTIFACTS_DIR={name!r} is build/test-results: a re-run must keep apart from it"
        )
    return path


def artifacts_root() -> Path:
    """Where scenarios keep their artifacts: build/test-artifacts, or a
    re-run's own directory (`rerun_dir`)."""
    return rerun_dir() or BUILD_DIR / "test-artifacts"


def save_ktest_tap(text: str) -> Path:
    """Save the in-kernel tests' TAP where `make status` reads it
    (build/test-results/ktest.tap), or in a re-run's own directory
    (`rerun_dir`), so the first run's file (which the feature gate read for
    the CI job summary) survives. Returns the path."""
    path = (rerun_dir() or BUILD_DIR / "test-results") / "ktest.tap"
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
