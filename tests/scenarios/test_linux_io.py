"""M8 ring-3 scenarios of pipes, FIFOs, readiness (`poll`, `select`), PTYs
and the console as a tty (ARCHITECTURE section 15, "M8's tests"): most
checks an `ok` line of `tools/linux-m8/m8_io`, the console's job control
(C10) Alpine's `ash -i` on the serial console."""

import re

from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs
from lumen_harness.shell import run

BOOT_TIMEOUT = 60
ATTEMPTS = 20
M8_IO = "/usr/local/bin/m8_io"
SERIAL_POLL = re.compile(
    r"serial: poll safety-step hits: (?P<hits>\d+), woken: (?P<woken>\d+), subscribers: (?P<subs>\d+)\n"
)


def _serial_poll(m) -> tuple[int, int, int]:
    out = run(m, "serial poll")
    found = SERIAL_POLL.fullmatch(out)
    assert found, out
    return int(found["hits"]), int(found["woken"]), int(found["subs"])


def boot_shell(boot, *, devices=None):
    m = boot("qemutest", devices=devices)
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m


def _console_poll_run(m) -> tuple[int, int, int]:
    """Run every console readiness check and return the checked poll counters."""
    start = len(m.output)
    m.send_line(f"linux {M8_IO} console-poll")

    for check in (
        "poll_nval",
        "poll_timeout_no_input",
        "ppoll_timeout_no_input",
        "select_timeout_no_input",
        "pselect6_timeout_no_input",
    ):
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 20)

    # Interactive readiness checks: `m8_io` prints `ready <tag>` immediately
    # before each syscall; send the serial bytes and confirm the check passes.
    for tag, payload, check in (
        ("poll", "ab", "poll_with_input"),
        ("ppoll", "c", "ppoll_with_input"),
        ("select", "d", "select_with_input"),
        ("pselect6", "e", "pselect6_with_input"),
    ):
        m.expect(re.compile(rf"^ready {tag}\r?$", re.MULTILINE), 20)
        m.send(payload)
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 20)

    m.expect(rf"linux: {re.escape(M8_IO)} exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)
    assert CRASH_BEGIN not in m.output[start:]

    # A safety-step hit means the wait found IRQ-queued input at the 100 ms
    # step without a notify setting `fired` (a dropped notify). It does not
    # detect an IRQ that never queued input. Check cleanup on every exit too.
    hits, woken, subs = _serial_poll(m)
    assert hits == 0, (hits, woken, subs)
    assert subs == 0, (hits, woken, subs)

    return hits, woken, subs


def test_poll_and_select_on_console(boot, tmp_path):
    """`poll`, `ppoll`, `select` and `pselect6` on the console's stdin with and
    without typed input; timeouts last at least their requested floor; remaining
    time is written back (0 on timeout; `rem < asked` and `rem + elapsed >= asked`
    when woken); invalid/O_PATH descriptors report POLLNVAL / ready / EBADF.

    COM1 must report input by interrupt. `serial poll` requires no wait to find
    IRQ-queued input at the 100 ms safety step without a notify (`hits == 0`),
    and all subscribers to unsubscribe on every exit. Repeat up to `ATTEMPTS` runs:
    a run with no wake is legitimate (input can arrive before syscall entry),
    but `ATTEMPTS` without one fails. `woken >= 1` proves notify reached a subscriber
    and set `fired`, not that IRQ 4 readied the thread promptly: a dropped
    `wake_one()` with `fired` kept is still counted as woken at the safety step.
    """
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    serial = run(m, "serial")
    assert "serial: COM1 input by interrupt (ISA IRQ 4," in serial, serial
    assert _serial_poll(m) == (0, 0, 0)

    # `ready` precedes syscall entry, not the block. Repeat up to ATTEMPTS
    # successful runs: one with no wake is legitimate, ATTEMPTS without one
    # fails. Bound the run count so a missing wake cannot hang the CI job.
    for _ in range(ATTEMPTS):
        hits, woken, subs = _console_poll_run(m)
        if woken >= 1:
            break
    else:
        raise AssertionError(
            f"no console wait was woken by IRQ 4's notify in {ATTEMPTS} runs: {(hits, woken, subs)}"
        )

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
