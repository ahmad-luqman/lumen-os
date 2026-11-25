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


def boot_shell(boot, *, devices=None):
    m = boot("qemutest", devices=devices)
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m


def test_poll_and_select_on_console(boot, tmp_path):
    """`poll`, `ppoll`, `select` and `pselect6` on the console's stdin with and
    without typed input; timeouts last at least their requested floor; remaining
    time is written back (0 on timeout; `rem < asked` and `rem + elapsed >= asked`
    when woken); invalid/O_PATH descriptors report POLLNVAL / ready / EBADF;
    IRQ 4 wakes the typed-byte waits without falling back to the 100 ms safety step."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    assert run(m, "serial poll") == "serial: poll safety-step hits: 0, subscribers: 0\n"

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

    # Interactive readiness checks: wait for each prompt from `m8_io`, send
    # serial bytes, and confirm the corresponding syscall wakes and reads them.
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

    # Every typed-byte wait woke via IRQ 4's `PollHead::notify`, never the
    # 100 ms safety step, and all subscribers unsubscribed on exit.
    assert run(m, "serial poll") == "serial: poll safety-step hits: 0, subscribers: 0\n"

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
