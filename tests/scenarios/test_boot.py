"""First boot: Limine loads the kernel, which reaches the logger and the shell."""

import re

from lumen_harness.qemu import EXIT_SUCCESS

BOOT_TIMEOUT = 60


def test_boot_reaches_logger(boot):
    m = boot("qemutest")
    m.expect(r"INFO  boot: Lumen \d+\.\d+\.\d+ \(x86_64\) booting", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: cmdline: 'lumen.qemu_exit'")
    m.expect(r"INFO  boot: symbols: \d+ entries")
    m.expect(r"INFO  mem: \d+ regions, \d+ MiB usable")
    m.expect(r"INFO  fb: framebuffer 1280x800 ready")
    m.expect(r"INFO  boot: boot complete")
    # The timeline is logged after `boot complete` (at warn if incomplete).
    m.expect(r"boot: timeline: .*\n")
    # Then Limine's times (at warn if unknown or out of order).
    m.expect(r"boot: before kmain: .*\n")
    out = m.output
    assert "WARN" not in out, "boot logged a warning"
    assert "ERROR" not in out, "boot logged an error"


# `[seconds.micro] LEVEL tag: message`, or `[    ?.??????]` before the TSC is
# calibrated (kernel/src/klog.rs, lumen_time::Stamp).
LOG_LINE = re.compile(
    r"^\[(?: *(?P<stamp>\d+\.\d{6})|    \?\.\?{6})\] "
    r"(?:ERROR|WARN |INFO |DEBUG|TRACE) [a-z0-9_:]+: \S"
)
# The timeline line, with its own stamp (kernel/src/trace.rs, `Timeline`).
# An incomplete one is logged at warn: matched too, so the test fails at once
# with the line instead of timing out.
TIMELINE = re.compile(
    r"\[ *(?P<stamp>\d+\.\d{6})\] (?P<level>INFO |WARN ) boot: timeline: (?P<text>.*)\n"
)
COMPLETE = re.compile(r"(?P<phases>.*) \(total (?P<total>\d+\.\d{3}) ms\)")
# `boot complete`, logged just before the last phase (`complete`) is marked;
# its stamp.
BOOT_COMPLETE = re.compile(r"\[ *(\d+\.\d{6})\] INFO  boot: boot complete\n")
PHASE = re.compile(r"(?P<name>\S+) \+(?P<ms>\d+\.\d{3}) ms")
PHASES = [
    "entry",
    "arch-init",
    "logger",
    "symbols",
    "memory",
    "stacks",
    "acpi",
    "pci",
    "interrupts",
    "time",
    "smp",
    "framebuffer",
    "complete",
]
# Logged just after the timeline (kernel/src/main.rs, `log_before_kmain`;
# the text is lumen_time::handoff's `BeforeKmain`, pinned in its host tests):
# Limine's own times for the boot (microseconds of the TSC by its
# calibration), and how long after its handoff `kmain` read the timeline's
# origin, from ticks (signed: an origin before the handoff reads `-`). At
# warn, and matched too to fail at once, if anything is unknown
# (`unknown (...)`) or out of order (`...; out of order: <reason>`).
BEFORE_KMAIN = re.compile(
    r"(?P<level>INFO |WARN ) boot: before kmain: (?P<text>limine started at (?P<started>\d+\.\d{3}) ms "
    r"and handed off at (?P<handoff>\d+\.\d{3}) ms \(TSC time\), "
    r"kmain began (?P<gap>[+-]\d+\.\d{3}) ms after the handoff|.*)\n"
)


def kernel_lines(output: str) -> list[str]:
    """The kernel's lines, from its first log line to before the prompt.
    (Limine's console output runs into the first line, so start at its
    stamp's bracket.)"""
    start = output.rindex("[", 0, output.index("INFO  boot: Lumen"))
    return output[start:].splitlines()[:-1]


def micros(decimal: str) -> int:
    """A decimal number of seconds (6 places) or milliseconds (3 places) as
    an integer count of microseconds: exact, unlike floats."""
    whole, frac = decimal.split(".")
    return int(whole) * 10 ** len(frac) + int(frac)


def test_log_lines_are_structured(boot):
    """Every kernel log line is `[time] LEVEL tag: message` (hard rule 8)."""
    m = boot("qemutest")
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    lines = kernel_lines(m.output)
    matches = [LOG_LINE.match(line) for line in lines]
    bad = [line for line, match in zip(lines, matches, strict=True) if not match]
    assert not bad, f"unstructured kernel output: {bad}"
    # The time is unknown until the TSC is calibrated, then real and
    # non-decreasing.
    stamps = [match["stamp"] for match in matches]
    calibrated = next((i for i, line in enumerate(lines) if "INFO  time: tsc " in line), None)
    assert calibrated is not None, "no `time: tsc` line"
    assert all(s is None for s in stamps[:calibrated]), stamps
    assert all(s is not None for s in stamps[calibrated:]), stamps
    us = [micros(s) for s in stamps[calibrated:]]
    assert us == sorted(us), stamps


def test_boot_timeline_printed(boot):
    """At boot complete, every phase's time since the timeline's origin, in
    order, the total between the log stamps around it; then Limine's times,
    with the origin after Limine's handoff."""
    m = boot("qemutest")
    line = m.expect(TIMELINE, BOOT_TIMEOUT)
    timeline = COMPLETE.fullmatch(line["text"])
    assert line["level"] == "INFO " and timeline, line[0]
    phases = [PHASE.fullmatch(p) for p in timeline["phases"].split(", ")]
    assert all(phases) and [p["name"] for p in phases] == PHASES, line[0]
    us = {p["name"]: micros(p["ms"]) for p in phases}
    assert list(us.values()) == sorted(us.values()), line[0]
    total = micros(timeline["total"])
    assert total == us["complete"], line[0]
    # The origin is at or before arch-init (above), and at least Limine's
    # 10 ms post-handoff wait after its handoff, which the kernel checks in
    # ticks (INFO means in order): so between Limine's handoff and
    # arch-init, not the TSC's reset or the firmware's handoff to Limine
    # (both before Limine started). That holds however slowly the emulator
    # ran; a time bound on arch-init didn't (#55). An origin read later,
    # before arch-init, isn't caught.
    before = m.expect(BEFORE_KMAIN, 10)
    what = f"arch-init +{us['arch-init'] / 1000:.3f} ms; {before['text']}"
    assert not before["text"].startswith("unknown ("), what
    assert before["level"] == "INFO " and before["gap"], what
    assert 0 < micros(before["started"]) < micros(before["handoff"]), what
    assert not before["gap"].startswith("-"), what
    # The floor, checked here too rather than only through INFO: the kernel
    # requires the origin at least ceil(hz_limine / 100) ticks (Limine's
    # 10 ms wait by its own frequency) after the handoff tick, and Limine's
    # frequency at least half of ours, so the gap in our clock is at least
    # 5 ms. Starvation only lengthens it: a floor, not a time bound.
    assert float(before["gap"]) >= 5.0, what
    # In guest time, measured by the TSC: the time phase calibrates the timer
    # against the TSC (`time::WINDOWS` (5) windows of `WINDOW_NS` (10 ms),
    # each waited out on the TSC) and sleeps 10 ms on it for the first tick:
    # at least 60 ms by construction. Calibrating the TSC itself (5 to 20 more
    # windows, timed by the HPET or PIT) adds 50 ms or more, but in TSC time
    # only as closely as the median frequency fits each window, so the bound
    # leaves it out. The whole boot: generously under 30 s (an emulator runs
    # slower, never that slow).
    assert us["time"] - us["interrupts"] >= 60_000, line[0]
    assert total <= 30_000_000, line[0]
    # The log's stamps count from the timeline's origin in its unit: both
    # are the boot CPU's TSC since the same entry tick, converted with the
    # same calibrated clock and truncated to the microsecond
    # (`time::since_boot_ns`, `Timeline::boot`). `boot complete` is stamped
    # just before the last phase is marked and this line just after
    # (kernel/src/main.rs), so the total falls between their stamps. Order
    # only, no time bound: a starved vCPU widens the gap between the stamps
    # but can't reverse it (#125). A log origin earlier than the timeline's,
    # or a faster unit, puts `boot complete` after the total once the error
    # exceeds that gap; a later origin or a slower unit puts this line
    # before it.
    stamps = BOOT_COMPLETE.findall(m.output)
    assert len(stamps) == 1, stamps
    assert micros(stamps[0]) <= total <= micros(line["stamp"]), (stamps[0], line[0])


def test_serial_input_round_trip(boot):
    """The harness can type into the kernel and read the reply."""
    m = boot("qemutest")
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    m.send_line("echo hello-from-the-harness")
    m.expect(r"\nhello-from-the-harness\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("frobnicate")
    m.expect(r"unknown command: frobnicate", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_breakpoint_is_survivable(boot):
    """#BP returns to the interrupted code; the stub restores all registers."""
    m = boot("qemutest")
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1\n", 10)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 2\n", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
