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


# The ktest kernel's summary of the lines `klog::hold_repeats` held back
# from serial at one call site (#347): how many (`n`), after the first, and
# that first line from its level on (`first`, as printed before; cut ones end
# `...`). Matched from its stamp, so the first line inside it never reads as
# a log line of its own.
HELD_BACK = re.compile(rf"{STAMP} INFO  klog: (?P<n>\d+) more held back like: (?P<first>[^\n]*)\n")


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


# The HPET's calibration said nothing and the PIT's is taken instead
# (calibration.tsc_reference reads it).
PIT_FALLBACK_WARNING = (
    r"WARN  time: can't calibrate the tsc against the HPET: .*; calibrating against the PIT$"
)
# A sound's underruns, its count as group `n` (test_sound adds them up).
UNDERRUN_WARNING = r"WARN  sound: a sound found the queue empty (?P<n>\d+) times \(underruns\)"


@dataclass(frozen=True)
class HostTimed:
    """A warning a loaded host can cause on its own: stalling the vCPU
    decides whether it is logged, not the code. A whole run's "no warning"
    check leaves these out (CLAUDE.md, "Tests on a busy host")."""

    # The line from its level on (as `shell.run`'s `allow`), matched from
    # its start.
    pattern: str
    # The kernel file that logs it, and text its format string holds, once
    # in that file; lines in the kernel's words (a calibration's windows
    # made up), each matching this pattern only; and lines that must not
    # match (other warnings near it). tests/harness/test_klog.py ties them.
    source: str
    text: str
    examples: tuple[str, ...]
    near_misses: tuple[str, ...]
    # Why the host decides it, and what judges it instead.
    why: str
    # If only some runs' lines are the host's: a decision from the match and
    # the accelerator the run used.
    holds: Callable[[re.Match[str], str], bool] | None = None


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
        (
            (
                "WARN  time: tsc calibration: against the HPET within 1.4% (past the 1.0% allowed), against the "
                "PIT it failed (it didn't count through a window): kept the HPET"
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
        (
            "WARN  time: lumen.stall_timer_windows=x: not <end ppm>[,<start ppm>], each at most 10000000; ignored",
        ),
        "the timer's bound past its tolerance through a stalled TSC calibration's; judged as the one above",
    ),
    HostTimed(
        PIT_FALLBACK_WARNING,
        "kernel/src/time.rs",
        "{judged}; calibrating against the PIT",
        (
            (
                "WARN  time: can't calibrate the tsc against the HPET: only 0 of 20 windows read promptly (5 wanted), "
                "and the middle ones' end stalls were as long as the rest of their windows, so the rate says nothing "
                "(tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5); calibrating against the PIT"
            ),
        ),
        (
            "WARN  time: the HPET at 0xfed00000 is unusable (it doesn't count); calibrating against the PIT",
        ),
        "the HPET's windows stalled until its median had no bound; calibration.fallback checks the "
        "line's numbers say so, and tsc_reference takes the PIT as the boot's reference",
    ),
    HostTimed(
        r"WARN  irq: no timer interrupt in 100 ms$",
        "kernel/src/main.rs",
        "no timer interrupt in 100 ms",
        ("WARN  irq: no timer interrupt in 100 ms",),
        ("WARN  irq: unexpected interrupt on vector 0x8f",),
        "under TCG only: QEMU's timer is its own thread there, which a loaded host can starve while the "
        "vCPU runs on (#45). Under KVM the timer is the host kernel's, and a tick due while the vCPU is "
        "descheduled is delivered as it resumes: a stall delays the first tick, never loses it, so the "
        "warning is the code's (unexpected under any other accelerator: fails closed)",
        holds=lambda _m, accel: accel == "tcg",
    ),
    HostTimed(
        r"WARN  shell: serial prompt held: .* not yet waiting for keys \(still waiting\)$",
        "kernel/src/shell/mod.rs",
        "not yet waiting for keys (still waiting)",
        (
            "WARN  shell: serial prompt held: shell-vt2 (ready) not yet waiting for keys (still waiting)",
        ),
        ("WARN  shell: top left the screen paused; took it back",),
        "a screen shell's thread stalled past the prompt's 5 s warning; the ktest "
        "shell::the_serial_prompt_waits_on_past_its_warning judges the wait (test_smp and test_shell "
        "still assert it never comes)",
    ),
    HostTimed(
        r"WARN  mm: cpus 0x[0-9a-f]+ didn't flush their TLBs within 2 s: freed stack addresses wait for them$",
        "kernel/src/mm/tlb.rs",
        "didn't flush their TLBs within 2 s: freed stack addresses wait for them",
        (
            "WARN  mm: cpus 0x4 didn't flush their TLBs within 2 s: freed stack addresses wait for them",
        ),
        (
            "WARN  mm: too many free stack ranges: the stack at 0xffffd00000001000 is never reused",
            "WARN  mm: cpus 0x4 still haven't flushed their TLBs: freed stack addresses wait for them",
        ),
        "a vCPU stalled past the 2 s a spawn waits for every CPU to flush a freed stack's pages (#300): "
        "the spawn takes other addresses instead (at worst one page table mapped for good), and later "
        "spawns don't wait until that CPU catches up, each of those that finds its range not flushed "
        "taking other addresses too (maybe a table each; mem counts them; a CPU still behind 10 s on is "
        "warned about again, not host-timed); a leaked_frames count it changes fails on its own",
    ),
    HostTimed(
        UNDERRUN_WARNING + "$",
        "kernel/src/sound.rs",
        "a sound found the queue empty {} times (underruns)",
        ("WARN  sound: a sound found the queue empty 3 times (underruns)",),
        ("WARN  sound: 0000:00:05.0: playing failed: no answer; the device is given up",),
        "the sound thread late past the 160 ms queued (or QEMU returning periods in a burst after a "
        "stall); test_sound checks the count `sound` shows is the warnings' sum, at most one per 8 "
        "periods (the queue's depth), and none for a sound of 8 periods or fewer",
    ),
)
UNDERRUNS = re.compile(UNDERRUN_WARNING)


def is_host_timed(line: str, accel: str | None = None) -> bool:
    """Whether log line `line` (its stamp optional) is a `HOST_TIMED` warning,
    for a run under `accel` (by default the scenarios', `qemu.default_accel`)."""
    accel = accel or default_accel()
    rest = without_stamp(line)
    for h in HOST_TIMED:
        m = re.match(h.pattern, rest)
        if m and (h.holds is None or h.holds(m, accel)):
            return True
    return False


def unexpected_warnings(out: str, target: str | None = None, accel: str | None = None) -> list[str]:
    """The ERROR and WARN lines in `out` (only `target`'s, if given) that
    aren't host-timed (`HOST_TIMED`) for a run under `accel`: what a run
    that should be quiet must not log, however loaded its host. A line is
    found wherever it starts (it can follow part of a line on serial), from
    its stamp."""
    tag = re.escape(target) if target else r"[^ \n]+"
    lines = re.findall(rf"{STAMP} (?:ERROR|WARN ) {tag}: [^\n]*", out)
    return [line for line in lines if not is_host_timed(line, accel)]
