"""`make run` and `make debug` work (developer entry points are deliverables too).

Not covered here: the QEMU window that `make run` opens (manual check).
"""

import os
import select
import subprocess
import sys
import time

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT
from lumen_harness.__main__ import find_gdb

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


def test_make_debug_stops_at_kmain():
    """`debug` boots paused under gdb, and a hardware breakpoint on kmain is hit."""
    # Checked here, not in a skipif: a LUMEN_GDB that can't be found raises, and
    # fails this test rather than skipping it. (Raised at collection, it would
    # error the whole module, and without xdist abort the whole run.) Without the
    # override, gdb is optional.
    if find_gdb() is None:
        pytest.skip("gdb not installed")
    # Uses the fixed gdb port 1234: do not run two of these at once. stderr too,
    # so a failing run (e.g. gdb timing out) shows why.
    out = subprocess.run(
        [sys.executable, "-m", "lumen_harness", "debug", "--batch",
         "--iso", str(BUILD_DIR / "lumen.iso"), "--kernel", str(BUILD_DIR / "lumen-kernel")],
        check=False, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=ENV,
        cwd=REPO_ROOT, timeout=180,
    ).stdout  # fmt: skip
    assert "#0  kmain () at kernel/src/main.rs" in out, out[-3000:]
    assert "_start" in out, out[-3000:]
