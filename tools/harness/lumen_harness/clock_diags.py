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
# over 100 µs, up to 8 times per end, 20 ms apart. Before the spacing it
# needed none in 90 KVM runs (45 on a loaded 4-vCPU runner), and in 79 TCG
# runs on a busy Mac (43 loaded, loadavg up to 51) 4 runs needed 1. But a
# loaded KVM runner can slow HPET reads for 60 ms or more (PR #121's load-5),
# which now costs up to 3 or 4 spaced retries on one end. So a run may take
# 4: one such burst. More means stalls hit readings far more often than ever
# measured: a host or `now_ns` getting worse, which a pass would hide.
MOST_HPET_RETRIES = 4
# The placement tests' try counts aren't bounded: they measure how often a
# loaded host stalls an 800 µs or 10 ms spin, not the code (6 of 8 under KVM
# load in PR #121), and each test already fails when no try can be judged.
HPET_BRACKET_NS = 100_000
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
PLACEMENT = re.compile(
    r"injected pause: (?P<what>HPET|timer) placement (?P<off>\d+) ns off \(at most "
    rf"(?P<allowed>\d+) ns allowed\) on try (?P<try>\d+) of (?P<tries>\d+), {RETRY_SPACING_MS} ms "
    r"apart; .*"
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
    assert retries <= MOST_HPET_RETRIES, f"over {MOST_HPET_RETRIES} retries: {m.group(0)}"
    assert start[-1] <= HPET_BRACKET_NS and end[-1] <= HPET_BRACKET_NS, m.group(0)
    # The timer test's brackets each hold an HPET read, so they have widths.
    t = one_diagnostic(report, "time::the_timer_is_programmed_for_1_khz", TIMER_BRACKETS)
    low, median, high, count = (int(t[k]) for k in ("min", "median", "max", "brackets"))
    assert low <= median <= high and high > 0 and count == 14, t.group(0)
    # The boot calibrations, once each in the run.
    _check_boot_calibration(report, text, "timer", BOOT_TIMER_CASE, BOOT_TIMER, TIMER_TOLERANCE_PPM)
    _check_boot_calibration(report, text, "tsc", BOOT_TSC_CASE, BOOT_TSC, TSC_TOLERANCE_PPM)
    # The placement tests each report one judged try, within their tries
    # and their allowance.
    for name in (
        "time::an_hpet_reading_is_placed_at_its_brackets_midpoint",
        "time::a_timer_reading_is_placed_at_its_brackets_midpoint",
    ):
        p = one_diagnostic(report, name, PLACEMENT)
        assert 1 <= int(p["try"]) <= int(p["tries"]), p.group(0)
        assert int(p["off"]) <= int(p["allowed"]), f"placed too far off: {p.group(0)}"
