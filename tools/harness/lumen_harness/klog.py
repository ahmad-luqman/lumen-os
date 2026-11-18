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
from collections.abc import Callable
from dataclasses import dataclass

from lumen_harness.qemu import default_accel

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


def without_stamp(line: str) -> str:
    """Log line `line` from its level on (`WARN  mouse: ...`), its stamp
    taken off if it has one."""
    return re.sub(rf"^{STAMP} ", "", line)


@dataclass(frozen=True)
class HostTimed:
    """A warning a loaded host can cause on its own: stalling the vCPU
    decides whether it is logged, not the code. A whole run's "no warning"
    check leaves these out (CLAUDE.md, "Tests on a busy host")."""

    # The line from its level on (as `shell.run`'s `allow`), matched from
    # its start.
    pattern: str
    # The kernel file that logs it, and text its format string holds, once
    # in that file; and lines as the kernel prints them, each matching this
    # pattern only (tests/harness/test_klog.py ties the four).
    source: str
    text: str
    examples: tuple[str, ...]
    # Why the host decides it, and what judges it instead.
    why: str
    # A further decision from the line's own numbers (its pattern's match),
    # if only some of what matches is the host's.
    holds: Callable[[re.Match[str]], bool] | None = None


HOST_TIMED = (
    HostTimed(
        r"WARN  time: (?:tsc|local APIC timer) calibration: only \d+ of \d+ windows read promptly \(5 wanted; ",
        "kernel/src/time.rs",
        "calibration: only {steady} of {taken} windows read promptly ({WINDOWS} wanted; a stall",
        (
            (
                "WARN  time: local APIC timer calibration: only 2 of 20 windows read promptly (5 wanted; a stall "
                "next to the timer's register accesses), so its rate may be off by up to 3.1% (0.5% of it the TSC's "
                "calibration), over the 2.0% allowed (tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5)"
            ),
            (
                "WARN  time: tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to the "
                "HPET's reads), so its rate may be off by up to 1.3%, over the 1.0% allowed "
                "(tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5)"
            ),
        ),
        "stalls next to a calibration's reads widen its windows' bounds past the clock's tolerance "
        "(#158's audit); calibration.verdict checks the line against its own numbers wherever "
        "unexpected_boot_warnings runs, and warned_exactly_when_past against the windows in test_time",
    ),
    HostTimed(
        r"WARN  time: local APIC timer calibration: \d+ of \d+ windows read promptly, but the TSC ",
        "kernel/src/time.rs",
        "calibration: {steady} of {taken} windows read promptly, but the TSC it is measured",
        (
            (
                "WARN  time: local APIC timer calibration: 0 of 20 windows read promptly, but the TSC it is "
                "measured against may be off by up to 1.0%, so its rate may be off by up to 2.6%, over the 2.0% "
                "allowed (tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5)"
            ),
        ),
        "the timer's bound past its tolerance through a stalled TSC calibration's; judged as the one above",
    ),
    HostTimed(
        r"WARN  time: can't calibrate the tsc against the HPET: .*; calibrating against the PIT$",
        "kernel/src/time.rs",
        "{judged}; calibrating against the PIT",
        (
            (
                "WARN  time: can't calibrate the tsc against the HPET: only 0 of 20 windows read promptly (5 wanted), "
                "and the middle ones' end stalls were as long as the rest of their windows, so the rate says nothing "
                "(tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5); calibrating against the PIT"
            ),
        ),
        "the HPET's windows stalled until its median had no bound; the PIT's calibration that follows "
        "is judged as any is (calibration.tsc_reference takes it as the boot's reference)",
    ),
    HostTimed(
        r"WARN  irq: no timer interrupt in 100 ms \(the CPU's reads at most (?P<us>\d+) us apart\)$",
        "kernel/src/main.rs",
        "no timer interrupt in 100 ms (the CPU's reads at most {us} us apart)",
        ("WARN  irq: no timer interrupt in 100 ms (the CPU's reads at most 31000 us apart)",),
        "the first tick lost to the host. Under KVM the timer runs in the host's kernel and only a stall "
        "of the vCPU loses a tick: host-timed only if the CPU went a tick period (1 ms) or more between "
        "its own reads while it waited (shorter gaps throughout: the timer itself didn't tick). Under "
        "TCG the timer is QEMU's own thread, which a loaded host starves while the vCPU runs on (#45): "
        "host-timed whatever the gaps",
        holds=lambda m: int(m["us"]) >= 1_000 or default_accel() != "kvm",
    ),
    HostTimed(
        r"WARN  shell: serial prompt held: .* not yet waiting for keys \(still waiting\)$",
        "kernel/src/shell/mod.rs",
        "not yet waiting for keys (still waiting)",
        (
            "WARN  shell: serial prompt held: shell-vt2 (ready) not yet waiting for keys (still waiting)",
        ),
        "a screen shell's thread stalled past the prompt's 5 s warning; the ktest "
        "shell::the_serial_prompt_waits_on_past_its_warning judges the wait (test_smp and test_shell "
        "still assert it never comes)",
    ),
    HostTimed(
        r"WARN  sound: a sound found the queue empty (?P<n>\d+) times \(underruns\)$",
        "kernel/src/sound.rs",
        "a sound found the queue empty {} times (underruns)",
        ("WARN  sound: a sound found the queue empty 3 times (underruns)",),
        "the sound thread stalled past the 160 ms queued; test_sound checks the count `sound` shows is "
        "the warnings' sum, and at most one per 8 periods (the queue's depth)",
    ),
)
# The underrun warning (HOST_TIMED's), its count as group `n`.
UNDERRUNS = re.compile(HOST_TIMED[-1].pattern.rstrip("$"))


def is_host_timed(line: str) -> bool:
    """Whether log line `line` (its stamp optional) is a `HOST_TIMED` warning."""
    rest = without_stamp(line)
    for h in HOST_TIMED:
        m = re.match(h.pattern, rest)
        if m and (h.holds is None or h.holds(m)):
            return True
    return False


def unexpected_warnings(out: str, target: str | None = None) -> list[str]:
    """The ERROR and WARN lines in `out` (only `target`'s, if given) that
    aren't host-timed (`HOST_TIMED`): what a run that should be quiet must
    not log, however loaded its host. A line is found wherever it starts
    (it can follow part of a line on serial), from its stamp."""
    tag = re.escape(target) if target else r"[^ \n]+"
    lines = re.findall(rf"{STAMP} (?:ERROR|WARN ) {tag}: [^\n]*", out)
    return [line for line in lines if not is_host_timed(line)]
