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


def test_poll_and_select_on_console(boot, tmp_path):
    """`poll`, `ppoll`, `select` and `pselect6` on the console's stdin with and
    without typed input; timeouts last at least their requested floor; remaining
    time is written back (0 on timeout; `rem < asked` and `rem + elapsed >= asked`
    when woken); invalid/O_PATH descriptors report POLLNVAL / ready / EBADF;
    `serial poll` confirms blocked waits woke via IRQ 4 (`woken >= 1`), none
    fell back to the 100 ms safety step (`hits == 0`), and all subscribers
    unsubscribed on every exit. Repeat complete runs until the IRQ wake is
    observed: a byte arriving before `sys_*` blocks is legitimately read on
    pass 1, so a run with no wake is not evidence of a broken wake path."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    assert _serial_poll(m) == (0, 0, 0)

    # `ready` precedes syscall entry, not the block. There is no fixed
    # number of runs (or delay) that guarantees an IRQ wake on a busy host.
    # Confirm that effect before finishing, without retrying any failures.
    while True:
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

        # A failed syscall, missed IRQ wake, or leaked subscription fails this
        # run immediately; only a successful run with no observed wake repeats.
        hits, woken, subs = _serial_poll(m)
        assert hits == 0, (hits, woken, subs)
        assert subs == 0, (hits, woken, subs)

        if woken >= 1:
            break

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
