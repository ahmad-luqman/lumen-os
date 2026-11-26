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
        "rusage",
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


def test_futex_timeouts_and_nanosleep(boot, tmp_path):
    """C4: backing keys, queued wakes/requeues, robust recovery and time floors."""
    m = boot("qemutest", devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True))
    m.expect(r"lumen> ", 60)
    out = run(m, f"linux {PROC} futex", 180)
    assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == [
        "futex-errors-and-demand",
        "futex-and-sleep-floors",
        "futex-wakes-requeues-bitsets",
        "futex-requeue-wake-limits",
        "futex-keys-across-fork",
        "futex-private-file-cow",
        "futex-robust-owner-death",
        "futex-clear-child-tid-exit-exec",
        "futex-robust-exec",
    ], out
    assert f"linux: {PROC} exit=0 " in out, out
    assert "FAIL" not in out, out
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_small_syscalls(boot, tmp_path):
    """C12: scheduling, names, accounting, capacity and stack-limit exec behavior."""
    m = boot(
        "qemutest", extra=["-smp", "2"], devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True)
    )
    m.expect(r"lumen> ", 60)
    memory = run(m, "mem | where area = frames")
    frames = re.search(r"^\s*frames\s+(\d+)\s+\d+\s+\d+\s*$", memory, re.MULTILINE)
    assert frames, memory
    out = run(m, f"linux {PROC} small-syscalls", 180)
    assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == [
        "affinity-getcpu",
        "priority-name-ids",
        "fork-raw-comm",
        "exec-raw-comm",
        "zombie-priority",
        "name-page-end",
        "syscall-error-order",
        "statfs",
        "rusage",
        "sysinfo-clocks",
        "stack-reduced",
        "stack-reduced-budget",
        "stack-exec",
    ], out
    info = re.search(r"sysinfo totalram=(\d+) uptime=(\d+) before=(\d+)", out)
    assert info, out
    assert int(info[1]) == int(frames[1]), out
    assert int(info[2]) >= int(info[3]), out
    assert f"linux: {PROC} exit=0 " in out, out
    assert "FAIL" not in out, out
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_sigchld_and_sigint_interrupt_wait(boot, tmp_path):
    """C6: real handlers, signal waits, mask swaps, restart and stop/continue."""
    from lumen_harness.screen import read_screen

    m = boot("qemutest", devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True))
    m.expect(r"lumen> ", 60)
    out = run(m, f"linux {PROC} signal-waits", 120)
    assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == [
        "signal-wanted-and-timeout",
        "signal-child-mask-swap",
        "signal-stop-continue",
    ], out
    assert f"linux: {PROC} exit=0 " in out, out

    attempts = 20

    def blocked(pid):
        for _ in range(attempts):
            out = run(m, "ps")
            if re.search(rf"^\s*{pid}\s+\d+\s+{pid}\s+blocked\s", out, re.MULTILINE):
                return
        raise AssertionError(f"waiter {pid} never blocked: {out}")

    confirmed_keys(m, ["alt", "f2"])
    for kind in [
        "read",
        "wait4",
        "nanosleep",
        "pselect6",
        "ppoll",
        "pause",
        "suspend",
        "timedwait",
    ]:
        for mode in ["eintr", "restart"]:
            type_confirmed(m, "clear\n")
            expect_screen(m, r"\Alumen>\s*\Z")
            type_confirmed(m, f"linux {PROC} c6-wait {kind} {mode}\n")
            ready = expect_screen(m, r"^c6: pid=(\d+) child=(\d+) ready$", 30)
            pid, child = map(int, ready.groups())
            blocked(pid)
            # A signal can land after the ready line but before the wait.
            # Resend for a bounded number of confirmed attempts.
            restarts = mode == "restart" and kind in ("read", "wait4")
            for _ in range(attempts):
                sent = run(m, f"linux {PROC} c6-kill {pid} 2", 30)
                screen = read_screen(m).text
                assert "FAIL" not in screen, screen
                if (restarts and "c6: handler" in screen) or (
                    not restarts and "ok c6-blocking" in screen
                ):
                    break
                assert "ok c6-send" in sent, sent
            else:
                raise AssertionError(f"signal never interrupted {kind}: {screen}")
            if restarts:
                blocked(pid)
                if kind == "read":
                    confirmed_keys(m, "z")
                else:
                    sent = run(m, f"linux {PROC} c6-kill {child} 9", 30)
                    assert "ok c6-send" in sent, sent
            expect_screen(m, rf"^ok c6-blocking\nlinux: {PROC} exit=0 leaked_frames=-?\d+$", 30)

    # A background child polls serial with events=0, so these serial-shell
    # queries cannot satisfy its wait. Confirm each effect before proceeding.
    serial_start = len(m.output)
    out = run(m, f"linux {PROC} c6-wait poll eintr")
    child = re.search(r"^c6: poll-child=(\d+)$", out, re.MULTILINE)
    assert child, out
    pid = int(child[1])
    for _ in range(attempts):
        # A serial ps command itself wakes this subscriber. Sample on VT2,
        # confirming all keys, then read the snapshot. Retry if a keyboard
        # confirmation's serial traffic caught the poller before it reblocked.
        type_confirmed(m, "ps\n")
        screen = read_screen(m).text
        if re.search(rf"^\s*{pid}\s+\d+\s+{pid}\s+blocked\s", screen, re.MULTILINE):
            break
    else:
        raise AssertionError(f"serial poller never blocked: {screen}")
    for _ in range(attempts):
        out = run(m, "serial poll")
        if re.search(r"subscribers: 1\n", out):
            break
    else:
        raise AssertionError(f"poll never subscribed: {out}")
    out = run(m, f"linux {PROC} c6-kill {pid} 2")
    assert "ok c6-send" in out, out
    for _ in range(attempts):
        out = run(m, "serial poll")
        if "ok c6-blocking" in m.output[serial_start:] and re.search(r"subscribers: 0\n", out):
            break
    else:
        raise AssertionError(f"interrupted poll did not finish and unsubscribe: {out}")
    assert "FAIL" not in m.output[serial_start:], m.output[serial_start:]
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
