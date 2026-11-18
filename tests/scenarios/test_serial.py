"""The serial port: COM1 found at boot or not (`lumen-uart`'s probe), and
its input by interrupt (#120).

Where each part is checked:
- the FIFO bursts and the probe: the ktests in `arch::x86_64::serial` and
  `cargo test -p lumen-uart`;
- the input path end to end, here: the serial shell blocks (the CPU
  idles), IRQ 4's count goes up, the interrupt wakes a blocked wait and no
  wait finds its input at the deadline (`serial`'s counts), and with COM1
  moved to IRQ 3 the 100 ms safety net still answers and warns once, when
  `serial` has counted three waits that found input at the deadline;
- the receive interrupt's setup (OUT2, the FIFO's trigger level 14), the
  reader's policy against a model of an interrupt that comes only on an
  edge (bytes a full queue left in the UART included), how the shell
  waits and when it warns: `cargo test -p lumen-uart` only. Under QEMU
  the bytes-left-in-the-UART path doesn't show (QEMU raises IRQ 4 again
  whenever bytes wait).
Not tested: `serial_in::init`'s queuing of bytes that came before the
unmask, the polling fallback, and blocking for good without a UART.
"""

import re
import time
from typing import NamedTuple

from lumen_harness.image import assert_matches_golden, compare, crop, update_requested
from lumen_harness.qemu import EXIT_CRASH, ExpectTimeout, Machine
from lumen_harness.shell import PROMPT, run
from PIL import Image

# The boot warning's row on the screen: the third log line, after the
# terminal's and the framebuffer's (the screen shows no timestamps). A new
# info line logged before it moves it, and this golden with it.
WARNING_ROW = {"origin": (0, 176), "size": (640, 16)}
BOOT_TIMEOUT = 60
# Retyping the command this often until QEMU exits: keys pressed before the
# screen's shell runs may be lost or land in a half-typed line (Ctrl-U clears
# it first).
ATTEMPTS = 30
WAIT_PER_ATTEMPT = 2


def _wait_for_row(m, golden):
    """Screendump until the warning's row matches `golden` (when
    re-recording: has something and holds still), or the boot timeout;
    the last screendump."""
    deadline = time.monotonic() + BOOT_TIMEOUT * m.timeout_scale
    previous = None
    while True:
        shot = m.screendump("screen.png")
        region = crop(shot, **WARNING_ROW)
        if update_requested():
            if region.getbbox() is not None and region == previous:
                return shot
            previous = region
        elif golden.exists() and compare(region, Image.open(golden), tolerance=0)[0].differing == 0:
            return shot
        if time.monotonic() > deadline:
            return shot
        time.sleep(1)


def test_boots_without_a_uart_and_warns_on_screen_golden(boot, artifacts, golden_dir):
    # No serial port at all (`-serial none`: q35 creates no COM1). The probe
    # finds none and the screen says so (the hazard it removes: an absent
    # port's line status says "data ready" forever, so the serial shell
    # would read 0xff without end). With no serial output to say when boot
    # got there, the screen is polled until the warning's row matches its
    # golden. Then the screen's shell runs: `crash panic` typed there ends
    # QEMU with the crash code.
    m = boot("qemutest", serial=False)
    golden = golden_dir / "no-serial-warning.png"
    shot = _wait_for_row(m, golden)
    assert_matches_golden(shot, golden, artifacts=artifacts, **WARNING_ROW)

    for _ in range(ATTEMPTS):
        m.press(["ctrl", "u"])
        m.type_text("crash panic\n")
        try:
            code = m.wait_exit(WAIT_PER_ATTEMPT)
            break
        except ExpectTimeout:
            pass
    else:
        raise AssertionError(f"no crash after typing `crash panic` {ATTEMPTS} times")
    assert code == EXIT_CRASH
    # The harness really ran without COM1: no kernel output arrived (QEMU's
    # own warnings, such as KVM's about CPUID features, share the stream).
    assert "boot:" not in m.output and "lumen>" not in m.output, m.output


SHELLS = r"INFO  shell: shells? on serial.*; type 'help'\n"
# The safety net's warning (allowed where a test expects it: `run` fails on
# any other WARN line).
MISSED_WARNING = "WARN  serial: COM1 input found without its interrupt"


def shell(boot, **kwargs) -> Machine:
    m = boot("qemutest", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def _thread_ticks(m: Machine, allow: str | None = None) -> dict[str, int]:
    """`threads`' tick count per thread name (one CPU: one `idle`)."""
    out = run(m, "threads", allow=allow)
    rows = re.findall(r"^  (\S+)\s+\d+\s+\d+\s+\w+\s+(\d+)$", out, re.MULTILINE)
    return {name: int(ticks) for name, ticks in rows}


def _com1_interrupts(m: Machine, allow: str | None = None) -> int:
    """`irqs`' count for COM1's input (ISA IRQ 4: GSI 4 on q35, no
    override), which must be routed and unmasked."""
    out = run(m, "irqs", allow=allow)
    row = re.search(
        r"^\s+4\s+0x54\s+Fixed\s+edge\s+high\s+0\s+unmasked\s+(\d+)$", out, re.MULTILINE
    )
    assert row, out
    return int(row[1])


SERIAL = re.compile(
    r"serial: COM1 input by interrupt \(ISA IRQ 4, GSI 4, vector 0x54\): (\d+) interrupts brought "
    r"input; waits woken by one: (\d+), readied by the deadline first: (\d+), input found at the "
    r"deadline: (\d+)\n"
)


class SerialCounts(NamedTuple):
    """`serial`'s counts of how the serial shell's waits ended."""

    # IRQ 4 interrupts that brought input.
    brought: int
    # Waits they woke.
    woken: int
    # Waits their deadline readied first (an interrupt brought the input
    # before the shell ran).
    late: int
    # Waits that found input at their deadline (no interrupt brought it).
    at_deadline: int


def _serial(m: Machine, allow: str | None = None) -> SerialCounts:
    """`serial`'s counts. Its own line's wait is counted before it prints."""
    out = run(m, "serial", allow=allow)
    found = SERIAL.fullmatch(out)
    assert found, out
    return SerialCounts(*map(int, found.groups()))


def _idle_second(m: Machine, allow: str | None = None) -> tuple[int, int]:
    """The idle and `main` threads' ticks over a second at the prompt."""
    before = _thread_ticks(m, allow)
    time.sleep(1)
    after = _thread_ticks(m, allow)
    return after["idle"] - before["idle"], after["main"] - before["main"]


def test_the_serial_shell_waits_for_input_by_interrupt(boot):
    # (#120) The serial shell blocks for a key until IRQ 4 brings one (or
    # its 100 ms safety-net deadline passes), instead of polling: over a
    # second at the prompt the CPU halts, so the idle thread takes most of
    # the ticks and more than the shell's own `main` thread. (Polling,
    # `main` took every tick and `idle` none: the CPU was 100% busy, and
    # under TCG so was a host CPU per guest.) Lines typed while it waits
    # wake it by interrupt: GSI 4's count goes up, the handler itself wakes
    # at least one of the waits for them (one that queues bytes without
    # waking the shell never would), every counted wait's input came by an
    # interrupt, and no wait finds input at its deadline instead (a handler
    # that doesn't wake the shell, or doesn't queue the bytes, would show
    # there). Not every line is counted: one that arrives before the shell
    # blocks again is read without blocking, at the wait's first look
    # (`WaitEnd::Idle`) or before it waits at all (a few percent of lines on
    # a loaded KVM runner, #170). Lines of 14+ bytes reach the FIFO's
    # trigger level, not only its timeout.
    m = shell(boot)
    first = _com1_interrupts(m)
    before = _serial(m)
    idle, main = _idle_second(m)
    assert idle >= 50 and idle > main, (idle, main)
    for i in range(6):
        assert run(m, f"echo a line typed while waiting {i}") == f"a line typed while waiting {i}\n"
    assert _com1_interrupts(m) > first
    after = _serial(m)
    assert after.at_deadline == 0, after
    assert after.woken > before.woken, (before, after)
    assert after.brought >= after.woken + after.late, after
    # And again after a pause (the handler wakes every wait, not just one).
    time.sleep(1)
    assert run(m, "echo after a pause") == "after a pause\n"
    again = _serial(m)
    assert again.at_deadline == 0 and again.woken > after.woken, (after, again)
    assert MISSED_WARNING not in m.output


# `serial` reads the safety-net scenario makes, at most, for three waits to
# find input at their deadline: far more than the few percent of lines read
# without blocking need (#165).
MOST_READS = 20


def _until_at_deadline(m: Machine, at_least: int) -> SerialCounts:
    """Read `serial` (each read a line, its wait counted before it prints)
    until it has counted `at_least` waits that found input at their
    deadline, with COM1 off IRQ 4: each read checks the warning is there
    exactly when three have (no interrupt resets the row, and a wait that
    found nothing, or found its input at the first look, neither counts nor
    breaks it)."""
    for _ in range(MOST_READS):
        # A deadline that finds nothing in between: it mustn't break the row.
        time.sleep(0.3)
        counts = _serial(m, MISSED_WARNING)
        assert (counts.brought, counts.woken, counts.late) == (0, 0, 0), counts
        assert (MISSED_WARNING in m.output) == (counts.at_deadline >= 3), (counts, m.output)
        if counts.at_deadline >= at_least:
            return counts
    raise AssertionError(
        f"{MOST_READS} reads of `serial` found only {counts.at_deadline} waits at their deadline"
    )


def test_the_safety_net_reads_input_when_irq_4_never_comes(boot):
    # (#120) COM1 moved to IRQ 3 (QEMU's isa-serial), so the IRQ 4 the
    # kernel routes never comes: every line is still read (by the 100 ms
    # deadline wherever the shell had blocked), GSI 4's count stays 0, and
    # one warning (once) says to check IRQ 4's routing, exactly when
    # `serial`'s count of waits that found input at their deadline reaches 3
    # (read after every line: each `serial` is one). A line that arrives
    # before the shell blocks again is read without blocking (#165), so the
    # reads go on until three have. The CPU still idles.
    m = shell(boot, com1_irq=3)
    assert run(m, "echo line 0", allow=MISSED_WARNING) == "line 0\n"
    counts = _until_at_deadline(m, 3)
    assert m.output.count(MISSED_WARNING) == 1, m.output
    assert re.search(
        MISSED_WARNING + r" 3 times in a row; check IRQ 4's routing \(GSI 4, vector 0x54\)",
        m.output,
    )
    # More found at the deadline: still the one warning.
    for i in range(3):
        assert run(m, f"echo more {i}", allow=MISSED_WARNING) == f"more {i}\n"
    _until_at_deadline(m, counts.at_deadline + 3)
    assert _com1_interrupts(m, MISSED_WARNING) == 0
    idle, main = _idle_second(m, MISSED_WARNING)
    assert idle >= 50 and idle > main, (idle, main)
    assert m.output.count(MISSED_WARNING) == 1, m.output
