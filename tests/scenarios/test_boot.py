"""First boot: Limine loads the kernel, which reaches the logger and the monitor."""

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
    out = m.output
    assert "WARN" not in out, "boot logged a warning"
    assert "ERROR" not in out, "boot logged an error"


# `[seconds.micro] LEVEL tag: message`, or `[    ?.??????]` before the TSC is
# calibrated (kernel/src/klog.rs, lumen_time::Stamp).
LOG_LINE = re.compile(
    r"^\[(?P<stamp> *\d+\.\d{6}|    \?\.\?{6})\] (?P<level>ERROR|WARN |INFO |DEBUG|TRACE) [a-z0-9_:]+: \S"
)
TIMELINE = re.compile(r"INFO  boot: timeline: (?P<phases>.*) \(total (?P<total>\d+\.\d{3}) ms\)\n")
PHASES = [
    "entry",
    "arch-init",
    "logger",
    "symbols",
    "memory",
    "stacks",
    "acpi",
    "interrupts",
    "time",
    "framebuffer",
    "complete",
]


def kernel_lines(output: str) -> list[str]:
    """The kernel's lines, from its first log line to before the prompt.
    (Limine's console output runs into the first line, so start at its
    stamp's bracket.)"""
    start = output.rindex("[", 0, output.index("INFO  boot: Lumen"))
    return output[start:].splitlines()[:-1]


def test_log_lines_are_structured(boot):
    """Every kernel log line is `[time] LEVEL tag: message` (hard rule 8)."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    lines = kernel_lines(m.output)
    bad = [line for line in lines if not LOG_LINE.match(line)]
    assert not bad, f"unstructured kernel output: {bad}"
    # The time is unknown until the TSC is calibrated, then real and
    # non-decreasing.
    stamps = [LOG_LINE.match(line)["stamp"] for line in lines]
    calibrated = next(i for i, line in enumerate(lines) if "INFO  time: tsc " in line)
    assert all("?" in s for s in stamps[:calibrated]), stamps
    seconds = [float(s) for s in stamps[calibrated:]]
    assert seconds == sorted(seconds), seconds
    assert seconds[0] > 0, seconds


def test_boot_timeline_printed(boot):
    """At boot complete, every phase's time since `kmain` began, in order."""
    m = boot("qemutest")
    line = m.expect(TIMELINE, BOOT_TIMEOUT)
    phases = [p.rsplit(" +", 1) for p in line["phases"].split(", ")]
    assert [name for name, _ in phases] == PHASES, line[0]
    ms = [float(t.removesuffix(" ms")) for _, t in phases]
    assert ms[0] == 0 and ms == sorted(ms), line[0]
    total = float(line["total"])
    assert total == ms[-1], line[0]
    # In guest time: calibrating the TSC and the timer alone takes ten 10 ms
    # windows, so at least 100 ms; generously under 30 s (an emulator runs
    # slower, never that slow).
    assert 100 <= total <= 30_000, line[0]
    # The line's own timestamp, taken just after the last phase, agrees.
    stamp = m.output[: line.start() + 15].rsplit("[", 1)[1].split("]")[0]
    assert total <= float(stamp) * 1000 <= total + 50, (stamp, line[0])


def test_serial_input_round_trip(boot):
    """The harness can type into the kernel and read the reply."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("echo hello-from-the-harness")
    m.expect(r"\nhello-from-the-harness\n", 10)
    m.expect(r"kmon> ", 10)
    m.send_line("frobnicate")
    m.expect(r"unknown command: frobnicate", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_breakpoint_is_survivable(boot):
    """#BP returns to the interrupted code; the stub restores all registers."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1\n", 10)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 2\n", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
