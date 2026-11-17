"""Helpers the QEMU harness tests share: options without a real ISO or
firmware, a stand-in QEMU binary, and a machine whose serial output is
given up front."""

import re
from pathlib import Path

from lumen_harness.qemu import ExpectTimeout, QemuOptions


def opts(**kw) -> QemuOptions:
    """Options for x.iso under TCG, with `kw` on top."""
    return QemuOptions(iso=Path("x.iso"), vars_file=Path("v.fd"), accel="tcg", **kw)


def fake_qemu(tmp_path: Path, script: str, name: str = "fake-qemu") -> Path:
    """An executable shell script standing in for QEMU."""
    exe = tmp_path / name
    exe.write_text("#!/bin/sh\n" + script + "\n")
    exe.chmod(0o755)
    return exe


class FakeMachine:
    """Serial output given up front; `expect` searches it from the previous
    match, like `Machine.expect` (without waiting: no match times out)."""

    def __init__(self, output: str) -> None:
        self.output = output
        self.sent: list[str] = []
        self._pos = 0

    def send_line(self, line: str) -> None:
        self.sent.append(line)

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        found = re.compile(pattern).search(self.output, self._pos)
        if not found:
            raise ExpectTimeout(f"timeout waiting for {pattern!r}")
        self._pos = found.end()
        return found
