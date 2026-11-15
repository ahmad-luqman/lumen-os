"""Helpers the QEMU harness tests share: options without a real ISO or
firmware, and a stand-in QEMU binary."""

from pathlib import Path

from lumen_harness.qemu import QemuOptions


def opts(**kw) -> QemuOptions:
    """Options for x.iso under TCG, with `kw` on top."""
    return QemuOptions(iso=Path("x.iso"), vars_file=Path("v.fd"), accel="tcg", **kw)


def fake_qemu(tmp_path: Path, script: str, name: str = "fake-qemu") -> Path:
    """An executable shell script standing in for QEMU."""
    exe = tmp_path / name
    exe.write_text("#!/bin/sh\n" + script + "\n")
    exe.chmod(0o755)
    return exe
