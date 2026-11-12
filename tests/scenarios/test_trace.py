"""The kernel trace ring, seen through the kmon `trace` command.

The ring itself is tested on the host (`cargo test -p lumen-trace`) and
in-kernel (`lumen_kernel::trace::*`); this checks what a person sees.
"""

import re

from test_boot import PHASES

BOOT_TIMEOUT = 60
EVENT_LINE = re.compile(r"^#(\d+) +tsc=(\d+) +(\S+)(.*)$", re.MULTILINE)


GAP_LINE = re.compile(r"^#\d+ +\((.+)\)$", re.MULTILINE)


def trace(m, arg: str) -> tuple[list[tuple[str, str, str, str]], int, int]:
    """Run `trace <arg>`; returns (event lines, claimed, dropped)."""
    start = len(m.output)
    m.send_line(f"trace {arg}".strip())
    summary = m.expect(r"trace: (\d+) events claimed, (\d+) dropped\n", 20)
    out = m.output[start : summary.start()]
    assert not GAP_LINE.findall(out), out  # nothing records while kmon prints
    return EVENT_LINE.findall(out), int(summary.group(1)), int(summary.group(2))


def test_trace_command_shows_every_event_since_boot(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    events, claimed, dropped = trace(m, "4096")
    assert dropped == 0
    # Nothing was overwritten or lost: one line per claimed event, in order.
    assert [int(seq) for seq, *_ in events] == list(range(claimed)), events
    phases = [rest.strip() for _, _, kind, rest in events if kind == "boot-phase"]
    assert phases == PHASES, phases
    assert any(kind == "log" and rest.strip() == "INFO boot" for _, _, kind, rest in events)
    # Boot phases come from straight-line code, so their timestamps are in
    # order (nested exceptions, and later interrupts, can reorder others').
    phase_tsc = [int(tsc) for _, tsc, kind, _ in events if kind == "boot-phase"]
    assert phase_tsc == sorted(phase_tsc)


def test_trace_defaults_and_usage(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    events, claimed, _ = trace(m, "")
    assert len(events) == min(16, claimed)
    m.send_line("trace lots")
    m.expect(r"usage: trace \[n\]\n", 10)


def test_exception_appears_in_trace(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1")
    events, _, _ = trace(m, "3")
    exceptions = [rest.strip() for _, _, kind, rest in events if kind == "exception"]
    assert any(re.fullmatch(r"vector=3 rip=0x[0-9a-f]+ error=0x0", e) for e in exceptions), events
