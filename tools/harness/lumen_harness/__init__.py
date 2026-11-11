"""Host-side harness for Lumen: boots QEMU, scripts serial, compares screenshots."""

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
BUILD_DIR = REPO_ROOT / "build"
