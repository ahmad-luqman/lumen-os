"""Checks on the clock cross-check ktests' diagnostics (#117).

They print their brackets and retries on passing runs too, where nobody reads
the TAP (CI keeps `ktest.tap` only on failure), so `test_ktest.py` reads them
with `check_clock_diags`. Self-tested on synthetic TAP in
tests/harness/test_clock_diags.py.
"""

from __future__ import annotations

import re

from lumen_harness import tap

# `now_ns_follows_the_hpet` takes an HPET reading again when its bracket is
# over 1 ms (kernel/src/time.rs's HPET_BRACKET_NS), up to 8 times per end,
# 20 ms apart. How many it took isn't bounded: that counts how often a
# loaded host stalled a bracket, not the code (the ktest fails when every
# reading at one end was wide). What is checked is that the count matches
# the readings and the kept ones are within the limit.
HPET_BRACKET_NS = 1_000_000
RETRY_SPACING_MS = 20

HPET_BRACKETS = re.compile(
    r"bracket widths: start \[(?P<start>[\d, ]*)\] ns, end \[(?P<end>[\d, ]*)\] ns "
    rf"\((?P<retries>\d+) taken again, {RETRY_SPACING_MS} ms apart; at most {HPET_BRACKET_NS} ns "
    r"trusted\)"
)
# `the_timer_is_programmed_for_1_khz` measures the timer's rate over 7
# one-shot windows (#129).
TIMER_BRACKETS = re.compile(
    r"bracket widths min/median/max (?P<min>\d+)/(?P<median>\d+)/(?P<max>\d+) ns over "
    r"(?P<brackets>\d+) brackets; \d+ of 7 one-shot windows with brackets over 5% of them"
)
# The boot ktests print the boot calibrations: the local APIC timer's (#129)
# and the TSC's (#139), each with its windows, retakes, how many are steady
# and how far off the median can be. Retakes and the steady count aren't
# bounded: they count how often a loaded host stalled boot, not the code. The
# bound is, by each clock's tolerance (lumen_time's
# TIMER_TOLERANCE_PPM and TSC_TOLERANCE_PPM): past it, boot warned.
TIMER_TOLERANCE_PPM = 20_000
TSC_TOLERANCE_PPM = 10_000
_BOOT_FIELDS = (
    r"(?P<hz>\d+) Hz \[(?P<windows>[\d/ ]*)\] retook (?P<retook>\d+), "
    r"(?P<steady>\d+) steady, median within (?P<bound>\d+) ppm; window rates \[(?P<rates>[\d, ]*)\] Hz"
)
BOOT_TIMER = re.compile(r"timer boot calibration " + _BOOT_FIELDS)
BOOT_TSC = re.compile(r"tsc boot calibration \((?P<reference>hpet|pit)\) " + _BOOT_FIELDS)
BOOT_TIMER_CASE = "time::the_boot_timer_calibration_is_within_what_the_tick_needs"
BOOT_TSC_CASE = "time::the_boot_tsc_calibration_is_within_what_now_ns_needs"
# The placement tests (#158) pause 20 ms on each side of a read (a 40 ms
# bracket) and judge a try only when its reference bracket and its
# uncertainty are each at most a quarter of the pause (lumen_time's
# placement_is_judged), taking it again otherwise, 8 tries at most. How many
# of those 8 a run took isn't held lower: that counts how often a loaded host
# stalled a bracket, not the code, and each test fails when none can be
# judged. What is checked is that the try it judged, and the ones it took
# again, follow from the widths it printed; and, for the HPET test (its pause
# and bracket both on the TSC), that no paused bracket is short of its pauses
# by more than PAUSE_SLACK_NS, the kernel's rounding allowance. (The timer
# test's floor also allows the boot TSC calibration's bound, not printed.)
PLACEMENT_PAUSE_NS = 20_000_000
PLACEMENT_TRIES = 8
PAUSE_SLACK_NS = 10_000
PLACEMENT = re.compile(
    r"injected pause: (?P<what>HPET|timer) placement (?P<off>\d+) ns off \(at most "
    rf"(?P<allowed>\d+) ns allowed\) on try (?P<try>\d+) of (?P<tries>\d+), {RETRY_SPACING_MS} ms "
    r"apart; tries \(reference, paused, by the brackets, by the read, in ns\) \[(?P<list>.*)\]"
)
# One try in that list: (reference, paused, by the brackets, by the read).
PLACEMENT_TRY = re.compile(r"\((\d+), (\d+), (\d+), (\d+)\)")


def placement_is_judged(reference: int, paused: int, pause: int) -> bool:
    """lumen_time's `placement_is_judged`: the reference bracket and the
    uncertainty (half of the reference's width plus what a stall added to the
    paused one's, rounded up) each at most a quarter of the pause."""
    stalled = max(paused - 2 * pause, 0)
    return 4 * reference <= pause and 4 * ((reference + stalled + 1) // 2) <= pause


def _check_placement(p: re.Match[str], what: str) -> None:
    """A placement test's judged line: its clock, tries and allowance as the
    kernel's constants say; one listed try per try taken; the last one judged
    and the ones before it not (else the kernel stopped too late or too
    soon); `off` that last try's two times apart, within the allowance; and
    the HPET test's paused brackets within rounding of their pauses."""
    line = p.group(0)
    assert p["what"] == what, f"{what} test printed another clock's line: {line}"
    allowed, taken, off = int(p["allowed"]), int(p["try"]), int(p["off"])
    assert (allowed, int(p["tries"])) == (PLACEMENT_PAUSE_NS // 2, PLACEMENT_TRIES), (
        f"not the kernel's pause and try count: {line}"
    )
    tries = [tuple(map(int, t)) for t in PLACEMENT_TRY.findall(p["list"])]
    listed = ", ".join(map(str, tries))
    assert listed == p["list"] and len(tries) == taken and 1 <= taken <= PLACEMENT_TRIES, (
        f"not one listed try per try taken: {line}"
    )
    *before, (reference, paused, by_brackets, by_read) = tries
    assert placement_is_judged(reference, paused, PLACEMENT_PAUSE_NS), (
        f"judged a try it can't: {line}"
    )
    assert not any(placement_is_judged(r, q, PLACEMENT_PAUSE_NS) for r, q, _, _ in before), (
        f"took a try again that could be judged: {line}"
    )
    assert off == abs(by_brackets - by_read), f"off isn't its try's two times apart: {line}"
    assert off <= allowed, f"placed too far off: {line}"
    if what == "HPET":
        assert all(q + PAUSE_SLACK_NS >= 2 * PLACEMENT_PAUSE_NS for _, q, _, _ in tries), (
            f"a paused bracket short of its pauses: {line}"
        )


def case_diagnostics(report: tap.TapReport, name: str) -> list[str]:
    """The diagnostics of the ktest `lumen_kernel::<name>`; it ran once."""
    cases = [c for c in report.cases if c.name == f"lumen_kernel::{name}"]
    assert len(cases) == 1, f"{name}: {len(cases)} cases"
    return cases[0].diagnostics


def one_diagnostic(report: tap.TapReport, name: str, pattern: re.Pattern[str]) -> re.Match[str]:
    """The one diagnostic of ktest `name` that matches `pattern` in full."""
    lines = case_diagnostics(report, name)
    found = [m for m in map(pattern.fullmatch, lines) if m]
    assert len(found) == 1, f"{name}: {len(found)} lines like {pattern.pattern!r}: {lines}"
    return found[0]


def _check_boot_calibration(
    report: tap.TapReport,
    text: str,
    clock: str,
    case: str,
    pattern: re.Pattern[str],
    tolerance_ppm: int,
) -> None:
    """The `clock` boot calibration's line: once in the run, in `case`, with
    five windows and rates, and its median within `tolerance_ppm` (however
    many windows are steady: enough least-unsteady ones may fill in)."""
    lines = re.findall(rf"^# {clock} boot calibration ", text, re.MULTILINE)
    assert len(lines) == 1, f"{len(lines)} {clock} boot calibration lines"
    b = one_diagnostic(report, case, pattern)
    windows, rates, steady = b["windows"].split(), _widths(b["rates"]), int(b["steady"])
    assert len(windows) == 5 and len(rates) == 5, f"not 5 windows and rates: {b.group(0)}"
    assert steady <= len(windows), f"more steady than windows: {b.group(0)}"
    assert int(b["bound"]) <= tolerance_ppm, (
        f"{clock} median bound over {tolerance_ppm} ppm: {b.group(0)}"
    )


def _widths(field: str) -> list[int]:
    return [int(w) for w in field.split(",") if w.strip()]


def check_clock_diags(report: tap.TapReport, text: str) -> None:
    """Assert the clock cross-checks' diagnostics in `report` (parsed from
    serial output `text`) are there, once each, and within their bounds."""
    # One real bracket line in the whole run (the injected-pause tests' say
    # so), in now_ns_follows_the_hpet.
    real = re.findall(r"^# bracket widths: start ", text, re.MULTILINE)
    assert len(real) == 1, f"{len(real)} now_ns bracket lines"
    # Every bracket line's retry count matches its readings, the injected
    # tests' too (they retake at the end as well as at the start).
    for line in re.findall(
        r"^# (?:injected pause: )?(bracket widths: start .*)$", text, re.MULTILINE
    ):
        b = HPET_BRACKETS.fullmatch(line)
        assert b, f"not a bracket line: {line}"
        taken = [len(_widths(b[end])) for end in ("start", "end")]
        expected = sum(max(n - 1, 0) for n in taken)
        assert int(b["retries"]) == expected, f"retries don't match the readings: {line}"
    m = one_diagnostic(report, "time::now_ns_follows_the_hpet", HPET_BRACKETS)
    start, end, retries = _widths(m["start"]), _widths(m["end"]), int(m["retries"])
    assert start and end, f"an end has no readings: {m.group(0)}"
    assert start[-1] <= HPET_BRACKET_NS and end[-1] <= HPET_BRACKET_NS, m.group(0)
    # The timer test's brackets each hold an HPET read, so they have widths.
    t = one_diagnostic(report, "time::the_timer_is_programmed_for_1_khz", TIMER_BRACKETS)
    low, median, high, count = (int(t[k]) for k in ("min", "median", "max", "brackets"))
    assert low <= median <= high and high > 0 and count == 14, t.group(0)
    # The boot calibrations, once each in the run.
    _check_boot_calibration(report, text, "timer", BOOT_TIMER_CASE, BOOT_TIMER, TIMER_TOLERANCE_PPM)
    _check_boot_calibration(report, text, "tsc", BOOT_TSC_CASE, BOOT_TSC, TSC_TOLERANCE_PPM)
    # The placement tests each report one judged try (_check_placement).
    for name, what in (
        ("time::an_hpet_reading_is_placed_at_its_brackets_midpoint", "HPET"),
        ("time::a_timer_reading_is_placed_at_its_brackets_midpoint", "timer"),
    ):
        _check_placement(one_diagnostic(report, name, PLACEMENT), what)
