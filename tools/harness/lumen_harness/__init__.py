"""Host-side harness for Lumen: boots QEMU, scripts serial, compares screenshots."""

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
BUILD_DIR = REPO_ROOT / "build"


def artifact_dir(root: Path, nodeid: str) -> Path:
    """Where a test keeps its artifacts: `root/<module>/<test>`, from a pytest
    node ID such as `tests/scenarios/test_crash.py::test_fault[pf]`.

    The module is part of it because tests run side by side (pytest-xdist): two
    same-named tests in different files must not delete each other's files."""
    file, _, test = nodeid.partition("::")
    return root / Path(file).stem / re.sub(r"[^A-Za-z0-9_.-]+", "_", test)
