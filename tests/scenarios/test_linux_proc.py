"""M8 process and signal behavior through real ring-3 programs."""

import re
import time

import pytest
from lumen_harness.gui import confirmed_keys, type_confirmed
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs, warm_up
from lumen_harness.screen import expect_screen
from lumen_harness.shell import run

PROC = "/usr/local/bin/m8_proc"
SIGNALS = [
    "senders_and_masks",
    "raise_without_siginfo",
    "blocked_pending_and_sigqueue",
    "nodefer_and_resethand",
    "alternate_stack",
    "fork_altstack",
    "vfork_altstack",
    "clone_vm_clears_altstack",
    "fp_and_mxcsr",
    "sigreturn_ss_sanitized",
    "sigreturn_rax_is_not_restart",
    "force_sigsegv",
]
FAULTS = ["de", "ud", "bp", "gp", "ss", "xm", "maperr", "accerr", "bus", "db"]


@pytest.mark.parametrize("cpu", ["qemu64", "max"])
def test_signals_and_sigaltstack(boot, tmp_path, cpu):
    """Signals, real trap recovery, and sigreturn's hardware safety on FX/XSAVE."""
    m = boot("qemutest", cpu=cpu, devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True))
    m.expect(r"lumen> ", 60)
    if cpu == "max":
        assert "INFO  fpu: XSAVE64" in m.output
    else:
        assert "INFO  fpu: FXSAVE64" in m.output

    def checks(mode, names):
        out = run(m, f"linux {PROC} {mode}", 90)
        assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == names, out
        assert f"linux: {PROC} exit=0 " in out, out
        assert "FAIL" not in out, out

    checks("signals", SIGNALS)
    checks("faults", FAULTS)
    for kind in ["mxcsr", "cs", "rip", "segvframe"] + (["xstate", "xcomp"] if cpu == "max" else []):
        serial_start = len(m.output)
        out = run(m, f"linux {PROC} signal-bad {kind}", 60)
        assert f"linux: {PROC} exit=139 signal=11 " in out, out
        assert "survived" not in out, out
        serial = m.output[serial_start:]
        assert (
            len(re.findall(r"INFO  signal: pid \d+: could not deliver SIG11: .+", serial)) == 1
        ), serial
    if cpu == "max":
        checks("signal-bad magic", ["fx_fallback"])
    for kind in ["blocked", "ignored"]:
        out = run(
            m,
            f"linux {PROC} signal-fatal {kind}",
            60,
            allow=r"WARN  user: pid \d+: killed by signal 11",
        )
        assert f"linux: {PROC} exit=139 signal=11 " in out, out

    # Two independent linux runs: the target is confirmed alive before the
    # sender attempts a privileged si_code. No fork/clone dependency.
    confirmed_keys(m, ["alt", "f2"])
    expect_screen(m, r"\Alumen>\s*\Z")
    type_confirmed(m, f"linux {PROC} signal-hold\n")
    pid = int(expect_screen(m, r"^signal-hold: pid=(\d+) ready$", 30)[1])
    checks(f"signal-probe {pid}", ["sigqueue_permission"])
    confirmed_keys(m, "n")
    expect_screen(m, rf"^signal-hold: done\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)

    def blocked(pid):
        deadline = time.monotonic() + 30 * m.timeout_scale
        while True:
            out = run(m, "ps")
            if re.search(rf"^\s*{pid}\s+\d+\s+{pid}\s+blocked\s", out, re.MULTILINE):
                return
            assert time.monotonic() < deadline, out

    for mode in ["eintr", "restart"]:
        confirmed_keys(m, ["alt", "f2"])
        type_confirmed(m, "clear\n")
        expect_screen(m, r"\Alumen>\s*\Z")
        type_confirmed(m, f"linux {PROC} signal-hold\n")
        expect_screen(m, r"^signal-hold: pid=\d+ ready$", 30)
        confirmed_keys(m, ["alt", "f3"])
        type_confirmed(m, "clear\n")
        expect_screen(m, r"\Alumen>\s*\Z")
        type_confirmed(m, f"linux {PROC} signal-wait {mode}\n")
        waiter = int(expect_screen(m, r"^signal-wait: pid=(\d+) ready$", 30)[1])
        blocked(waiter)  # Confirm F_SETLKW blocked before sending.
        checks(f"signal-kill {waiter}", ["send_usr1"])
        expect_screen(m, r"^signal-wait: handler$", 30)
        if mode == "eintr":
            expect_screen(m, rf"^ok eintr\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)
        else:
            blocked(waiter)  # SA_RESTART re-entered the contended lock wait.
        confirmed_keys(m, ["alt", "f2"])
        confirmed_keys(m, "n")
        expect_screen(m, rf"^signal-hold: done\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)
        confirmed_keys(m, ["alt", "f3"])
        if mode == "restart":
            expect_screen(m, rf"^ok sa_restart\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)
    confirmed_keys(m, ["alt", "f2"])
    type_confirmed(m, "clear\n")
    expect_screen(m, r"\Alumen>\s*\Z")
    type_confirmed(m, f"linux {PROC} signal-spin\n")
    spinner = int(expect_screen(m, r"^signal-spin: pid=(\d+) ready$", 30)[1])
    checks(f"signal-kill {spinner}", ["send_usr1"])
    expect_screen(m, rf"^ok async_delivery\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_fork_wait_and_exit_status(boot, tmp_path):
    """C1: fork, zombies, waits, autoreaping, reparenting and vfork.

    C3/C6/C8/C9/C12 extend this scenario with their design-tagged checks.
    """
    disk = copy_rootfs(tmp_path)
    m = boot("qemutest", devices=Devices(blk_image=disk, rng=True))
    m.expect(r"lumen> ", 60)
    warm_up(m)
    out = run(m, "linux /usr/local/bin/m8_proc fork-wait", 120)
    for check in (
        "exit-status",
        "wait-nohang",
        "waitid-zombie",
        "wait-copyout-fault",
        "sigchld-autoreap",
        "orphan-init",
        "vfork-clone",
        "clone-tids",
        "clone-wait-errors",
        "fork-context",
        "clone-stack-tls",
        "clone-shared-resources",
        "fork-inherited-state",
    ):
        assert f"ok {check}\n" in out, out
    assert re.search(r"linux: /usr/local/bin/m8_proc exit=0 leaked_frames=0\b", out), out
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_fork_cow_and_exec_shebang(boot, tmp_path):
    """C2: private CoW, scripts, exec state, vfork and streaming a padded ELF."""
    m = boot("qemutest", devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True))
    m.expect(r"lumen> ", 60)
    out = run(m, f"linux {PROC} fork-exec", 180)
    assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == [
        "fork-cow",
        "exec-self-identity",
        "exec-renamed-fd",
        "exec-random-fresh",
        "exec-shebang-state",
        "exec-refusals",
        "exec-relative-and-bytes",
        "exec-nesting-limit",
        "vfork-exec-files",
        "exec-over-16m",
    ], out
    assert f"linux: {PROC} exit=0 " in out, out
    assert "FAIL" not in out, out
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_exec_uses_unseeded_random_fallback(boot, tmp_path):
    """Shell load and execve share an infallible AT_RANDOM policy, warning once."""
    m = boot("qemutest", devices=Devices(blk_image=copy_rootfs(tmp_path)), cpu="qemu64")
    m.expect(r"lumen> ", 60)
    warning = r"WARN  exec: AT_RANDOM: no seeded RNG; using TSC/time fallback"
    first = run(m, f"linux {PROC} fork-exec", 180, allow=warning)
    assert "FAIL" not in first, first
    assert f"linux: {PROC} exit=0 " in first, first
    assert sum("AT_RANDOM:" in line for line in first.logs) == 1, first.logs
    second = run(m, f"linux {PROC} exec-exit", 60)
    assert f"linux: {PROC} exit=42 leaked_frames=0" in second, second
    assert not any("AT_RANDOM:" in line for line in second.logs), second.logs
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
