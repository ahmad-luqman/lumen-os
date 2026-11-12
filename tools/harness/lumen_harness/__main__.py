"""Command line: `python -m lumen_harness {run,debug,audit,status}`."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

from . import BUILD_DIR
from .qemu import QemuOptions, fresh_vars, qemu_args


def cmd_run(a: argparse.Namespace) -> int:
    opts = QemuOptions(
        iso=a.iso,
        vars_file=fresh_vars(BUILD_DIR / "run/OVMF_VARS.fd"),
        headless=a.headless,
        serial="mon:stdio",  # Ctrl-A c reaches the QEMU monitor
        monitor=None,
        log_file=BUILD_DIR / "run/qemu.log",
    )
    args = qemu_args(opts)
    print("+", " ".join(args), file=sys.stderr)
    print("(serial is attached here; Ctrl-A x quits QEMU)", file=sys.stderr)
    return subprocess.call(args)


def gdb_command(kernel: Path, batch: bool) -> list[str]:
    """gdb invocation that attaches to QEMU's stub and stops at kmain."""
    cmd = [os.environ.get("LUMEN_GDB") or "gdb"]
    if batch:
        cmd += ["--batch", "-nx"]
    cmd += [
        str(kernel),
        "-ex", "set architecture i386:x86-64",
        "-ex", "target remote :1234",
        # `*kmain`: kmain's first instruction. Plain `kmain` stops after the
        # prologue, where the next instruction may belong to code inlined
        # into kmain (e.g. serial setup), and gdb then reports that frame.
        "-ex", "hbreak *kmain",
        "-ex", "continue",
    ]  # fmt: skip
    if batch:
        cmd += ["-ex", "bt", "-ex", "info registers rip", "-ex", "kill"]
    return cmd


def cmd_debug(a: argparse.Namespace) -> int:
    opts = QemuOptions(
        iso=a.iso,
        vars_file=fresh_vars(BUILD_DIR / "debug/OVMF_VARS.fd"),
        headless=a.batch,
        serial=f"file:{BUILD_DIR / 'debug/serial.log'}",
        log_file=BUILD_DIR / "debug/qemu.log",
        gdb=True,
        accel="tcg",  # gdb breakpoints are most reliable under TCG
    )
    (BUILD_DIR / "debug").mkdir(parents=True, exist_ok=True)
    qemu = subprocess.Popen(qemu_args(opts))
    print(f"QEMU paused, gdb stub on :1234, serial -> {BUILD_DIR / 'debug/serial.log'}")
    try:
        return subprocess.call(gdb_command(a.kernel, a.batch), timeout=120 if a.batch else None)
    finally:
        qemu.terminate()
        qemu.wait(timeout=10)


def cmd_audit(a: argparse.Namespace) -> int:
    from .audit import run_audit

    return run_audit([Path(p) for p in a.kernels])


def cmd_status(a: argparse.Namespace) -> int:
    from .status import run_status

    return run_status(write=a.write, check=a.check)


def main() -> int:
    p = argparse.ArgumentParser(prog="lumen_harness")
    sub = p.add_subparsers(dest="cmd", required=True)

    r = sub.add_parser("run", help="boot with a window, serial on this terminal")
    r.add_argument("--iso", type=Path, required=True)
    r.add_argument("--headless", action="store_true", help="no window (e.g. over SSH)")
    r.set_defaults(func=cmd_run)

    d = sub.add_parser("debug", help="boot paused under gdb")
    d.add_argument("--iso", type=Path, required=True)
    d.add_argument("--kernel", type=Path, required=True)
    d.add_argument("--batch", action="store_true", help="headless: stop at kmain, print bt, quit")
    d.set_defaults(func=cmd_debug)

    au = sub.add_parser("audit", help="check kernel code-generation flags")
    au.add_argument("kernels", nargs="+")
    au.set_defaults(func=cmd_audit)

    st = sub.add_parser("status", help="generate docs/STATUS.md from test results")
    g = st.add_mutually_exclusive_group(required=True)
    g.add_argument("--write", action="store_true")
    g.add_argument("--check", action="store_true")
    st.set_defaults(func=cmd_status)

    a = p.parse_args()
    return a.func(a)


if __name__ == "__main__":
    sys.exit(main())
