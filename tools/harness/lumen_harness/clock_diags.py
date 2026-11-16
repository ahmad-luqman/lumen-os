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
# over 100 µs, at most 4 times per end. In PR #121 it needed none in 90 KVM
# runs (45 on a loaded 4-vCPU runner, at e4b2462 and 48345e3), and in 79 TCG
# runs on a busy Mac (43 loaded, loadavg up to 51) 4 runs needed 1 and none
# more. More than 2 in one run means stalls hit readings far more often than
# ever measured: a host or `now_ns` getting worse, which a pass would hide.
MOST_HPET_RETRIES = 2
# The placement tests' try counts aren't bounded: they measure how often a
# loaded host stalls an 800 µs or 10 ms spin, not the code (6 of 8 under KVM
# load in PR #121), and each test already fails when no try can be judged.
HPET_BRACKET_NS = 100_000

HPET_BRACKETS = re.compile(
    r"bracket widths: start \[(?P<start>[\d, ]*)\] ns, end \[(?P<end>[\d, ]*)\] ns "
    rf"\((?P<retries>\d+) taken again; at most {HPET_BRACKET_NS} ns trusted\)"
)
TIMER_BRACKETS = re.compile(
    r"bracket widths min/median/max (?P<min>\d+)/(?P<median>\d+)/(?P<max>\d+) ns over "
    r"(?P<brackets>\d+) brackets; windows taken again: \d+ too long; \d+ kept with brackets "
    r"over 5% of them"
)
PLACEMENT = re.compile(
    r"injected pause: (?P<what>HPET|timer) placement (?P<off>\d+) ns off \(at most "
    r"(?P<allowed>\d+) ns allowed\) on try (?P<try>\d+) of (?P<tries>\d+); .*"
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
    assert low <= median <= high and high > 0 and count >= 14, t.group(0)
    # The placement tests each report one judged try, within their tries
    # and their allowance.
    for name in (
        "time::an_hpet_reading_is_placed_at_its_brackets_midpoint",
        "time::a_timer_reading_is_placed_at_its_brackets_midpoint",
    ):
        p = one_diagnostic(report, name, PLACEMENT)
        assert 1 <= int(p["try"]) <= int(p["tries"]), p.group(0)
        assert int(p["off"]) <= int(p["allowed"]), f"placed too far off: {p.group(0)}"
