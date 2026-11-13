"""`make run` and `make debug` work (developer entry points are deliverables too).

Not covered here: the QEMU window that `make run` opens (manual check).
"""

import os
import select
import shutil
import subprocess
import sys
import time

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT
from lumen_harness.__main__ import gdb_binary

ENV = dict(os.environ, PYTHONPATH=str(REPO_ROOT / "tools/harness"))


def test_make_run_serial_console():
    """`run` attaches the kernel's serial console to the terminal (headless here)."""
    proc = subprocess.Popen(
        [sys.executable, "-m", "lumen_harness", "run", "--headless",
         "--iso", str(BUILD_DIR / "lumen.iso")],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        env=ENV, cwd=REPO_ROOT,
    )  # fmt: skip
    out = b""
    try:
        deadline = time.monotonic() + 60
        sent = False
        while (remaining := deadline - time.monotonic()) > 0:
            # Wait with a timeout: a bare os.read() would block forever if QEMU
            # stays alive but goes silent, and the deadline would never fire.
            ready, _, _ = select.select([proc.stdout], [], [], remaining)
            if not ready:
                break
            chunk = os.read(proc.stdout.fileno(), 4096)
            if not chunk:
                break
            out += chunk
            if not sent and b"kmon> " in out:
                proc.stdin.write(b"help\r")
                proc.stdin.flush()
                sent = True
            if b"crash <kind>" in out:
                break
        assert b"crash <kind>" in out, out[-2000:].decode(errors="replace")
        proc.stdin.write(b"\x01x")  # Ctrl-A x: quit QEMU from the mon:stdio mux
        proc.stdin.flush()
        assert proc.wait(timeout=20) == 0
    finally:
        if proc.poll() is None:
            proc.kill()


@pytest.mark.skipif(shutil.which(gdb_binary()) is None, reason="gdb not installed")
def test_make_debug_stops_at_kmain():
    """`debug` boots paused under gdb, and a hardware breakpoint on kmain is hit."""
    # Uses the fixed gdb port 1234: do not run two of these at once.
    out = subprocess.run(
        [sys.executable, "-m", "lumen_harness", "debug", "--batch",
         "--iso", str(BUILD_DIR / "lumen.iso"), "--kernel", str(BUILD_DIR / "lumen-kernel")],
        check=False, capture_output=True, text=True, env=ENV, cwd=REPO_ROOT, timeout=180,
    ).stdout  # fmt: skip
    assert "#0  kmain () at kernel/src/main.rs" in out, out[-3000:]
    assert "_start" in out, out[-3000:]
