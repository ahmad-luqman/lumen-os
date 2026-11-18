"""The kernel log's lines on serial, as the harness matches them.

The format is kernel/src/klog.rs's (the stamp is lumen_time::Stamp):
`[seconds.micros] LEVEL tag: message` and a newline, the seconds
right-aligned in at least 5 columns, or `[    ?.??????]` before the TSC is
calibrated; the level padded to 5 columns. Change this module with them:
`lumen_harness.shell` takes these lines out of a serial command's output,
and test_boot checks every boot line against a stricter form built from
the same pieces. `unexpected_warnings` is a run's "no warning" check, less
the warnings a loaded host can cause on its own (`HOST_TIMED`).
"""

from __future__ import annotations

import re
from dataclasses import dataclass

# A calibrated stamp's seconds (5 columns or more, right-aligned) and
# microseconds.
SECONDS = r"(?=[ \d]{5}) *\d+\.\d{6}"
# Before the TSC is calibrated.
UNKNOWN = r"    \?\.\?{6}"
STAMP = rf"\[(?:{SECONDS}|{UNKNOWN})\]"


def stamp_capturing(name: str) -> str:
    """`STAMP`, capturing a calibrated stamp's `seconds.micros` (without its
    padding) as group `name`; the group is None for `[    ?.??????]`."""
    return rf"\[(?:(?=[ \d]{{5}}) *(?P<{name}>\d+\.\d{{6}})|{UNKNOWN})\]"


LEVEL = r"(?:ERROR|WARN |INFO |DEBUG|TRACE)"
# The levels a test should see: they fail a command's output (shell.run).
LOUD = ("ERROR", "WARN")
# One whole line, newline included, wherever it starts: one can follow part
# of a line on serial (a key's echo, a command's write that didn't end its
# line). No groups, so patterns can repeat it.
LOG_LINE = re.compile(rf"{STAMP} {LEVEL} [^ \n]+: [^\n]*\n")


def is_log_line(text: str) -> bool:
    """Whether `text` is one whole log line (its newline optional)."""
    return LOG_LINE.fullmatch(text if text.endswith("\n") else text + "\n") is not None


def level_of(line: str) -> str:
    """A log line's level: `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`."""
    found = re.match(rf"{STAMP} ({LEVEL}) ", line)
    if not found:
        raise ValueError(f"not a log line: {line!r}")
    return found[1].strip()


@dataclass(frozen=True)
class HostTimed:
    """A warning a loaded host can cause on its own: stalling the vCPU
    decides whether it is logged, not the code. A whole run's "no warning"
    check leaves these out (CLAUDE.md, "Tests on a busy host"); each is
    judged by its own scenario."""

    # The line after its stamp, matched from its start.
    pattern: str
    # The kernel file that logs it, and text its format string holds
    # (tests/harness/test_klog.py ties the two).
    source: str
    text: str
    # Why the host decides it, and what judges it instead.
    why: str


HOST_TIMED = (
    HostTimed(
        r"WARN  time: (?:tsc|local APIC timer) calibration: (?:only )?\d+ of \d+ windows read promptly",
        "kernel/src/time.rs",
        "calibration: only {steady} of {taken} windows read promptly",
        "stalls past the calibration windows' gaps (#158's audit); "
        "calibration.warned_exactly_when_past judges it by the bound printed",
    ),
    HostTimed(
        r"WARN  time: tsc calibration: against the HPET within ",
        "kernel/src/time.rs",
        "tsc calibration: against the HPET within {against_the_hpet}",
        "the PIT's calibration failed after a stalled HPET one (the choice is otherwise INFO; "
        "calibration.weighed judges it)",
    ),
    HostTimed(
        r"WARN  time: can't calibrate the tsc against the HPET: .*; calibrating against the PIT$",
        "kernel/src/time.rs",
        "; calibrating against the PIT",
        "every HPET window stalled; the PIT's calibration that follows is judged as any is",
    ),
    HostTimed(
        r"WARN  irq: no timer interrupt in 100 ms$",
        "kernel/src/main.rs",
        "no timer interrupt in 100 ms",
        "a vCPU stalled past the first tick's 100 ms; test_time counts the timer's interrupts",
    ),
    HostTimed(
        r"WARN  shell: serial prompt held: ",
        "kernel/src/shell/mod.rs",
        "serial prompt held: ",
        "a screen shell's thread stalled past the prompt's 1 s warning; "
        "test_smp and the ktest judge the wait",
    ),
    HostTimed(
        r"WARN  sound: a sound found the queue empty \d+ times \(underruns\)$",
        "kernel/src/sound.rs",
        "a sound found the queue empty {} times (underruns)",
        "the sound thread stalled past the 160 ms queued; test_sound checks the count it gives "
        "is the one `sound` shows",
    ),
)


def is_host_timed(line: str) -> bool:
    """Whether log line `line` (its stamp optional) is a `HOST_TIMED` warning."""
    rest = re.sub(rf"^{STAMP} ", "", line.rstrip("\n"))
    return any(re.match(h.pattern, rest) for h in HOST_TIMED)


def unexpected_warnings(out: str, target: str | None = None) -> list[str]:
    """The ERROR and WARN lines in `out` (only `target`'s, if given) that
    aren't host-timed (`HOST_TIMED`): what a run that should be quiet must
    not log, however loaded its host. A line is found wherever it starts
    (it can follow part of a line on serial), from its stamp."""
    tag = re.escape(target) if target else r"[^ \n]+"
    lines = re.findall(rf"{STAMP} (?:ERROR|WARN ) {tag}: [^\n]*", out)
    return [line for line in lines if not is_host_timed(line)]
