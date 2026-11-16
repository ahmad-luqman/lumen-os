"""check_clock_diags must pass a good run and catch each way the clock
cross-checks' diagnostics can go wrong (#117), on synthetic TAP."""

import pytest
from lumen_harness import tap
from lumen_harness.clock_diags import check_clock_diags

NOW_NS = (
    "# bracket widths: start [{start}] ns, end [{end}] ns ({retries} taken again; "
    "at most {limit} ns trusted)\n"
    "# now_ns moved 50000000 ns, the HPET 50001000 ns: 1000 ns apart, of a 1000020 ns budget\n"
)
TIMER = (
    "# bracket widths min/median/max 0/{median}/{max} ns over 14 brackets; windows taken again: "
    "0 too long; 0 kept with brackets over 5% of them\n"
    "# timer counts at 62.5 MHz (calibrated 62.5 MHz)\n"
)
HPET_PLACEMENT = (
    "# injected pause: HPET placement 5000 ns off (at most 2500000 ns allowed) on try {tries} "
    "of 8; paused brackets [10048000] ns, the reference's 30000 ns\n"
)
TIMER_PLACEMENT = (
    "# injected pause: timer placement 500 ns off (at most 200000 ns allowed) on try 1 of 8; "
    '427500 ns by the HPET, 428000 ns by the timer; tries (...) [("", 18000, 829000, 427500)]\n'
)


def run(
    start="25000",
    end="1000",
    retries=0,
    limit=100000,
    now_ns_case="now_ns_follows_the_hpet",
    extra_line=False,
    timer_max=39000,
    placement_tries=1,
):
    now_ns = NOW_NS.format(start=start, end=end, retries=retries, limit=limit)
    cases = [
        (now_ns_case, now_ns + (now_ns if extra_line else "")),
        (
            "an_hpet_reading_is_placed_at_its_brackets_midpoint",
            HPET_PLACEMENT.format(tries=placement_tries),
        ),
        (
            "a_stalled_hpet_bracket_is_read_again",
            "# injected pause: bracket widths [246000, 101000, 1000] ns\n",
        ),
        (
            "every_stalled_hpet_bracket_is_a_failure",
            (
                "# injected pause: bracket widths: start [201000, 201000, 201000, 201000, 201000] "
                "ns, end [] ns (4 taken again; at most 100000 ns trusted)\n"
            ),
        ),
        (
            "the_timer_is_programmed_for_1_khz",
            TIMER.format(median=min(1000, timer_max), max=timer_max),
        ),
        ("a_timer_reading_is_placed_at_its_brackets_midpoint", TIMER_PLACEMENT),
    ]
    if now_ns_case != "now_ns_follows_the_hpet":
        cases.append(("now_ns_follows_the_hpet", ""))
    text = f"TAP version 14\n1..{len(cases)}\n"
    for n, (name, diags) in enumerate(cases, 1):
        text += f"{diags}ok {n} - lumen_kernel::time::{name}\n"
    return tap.parse(text), text


def test_a_good_run_passes():
    check_clock_diags(*run())


def test_two_retries_pass():
    check_clock_diags(*run(start="150000, 25000", end="120000, 1000", retries=2))


def test_three_retries_fail():
    with pytest.raises(AssertionError, match="over 2 retries"):
        check_clock_diags(*run(start="150000, 140000, 25000", end="120000, 1000", retries=3))


def test_a_retry_count_that_doesnt_match_the_readings_fails():
    # The diag counting only the start's retries.
    with pytest.raises(AssertionError, match="retries don't match"):
        check_clock_diags(*run(start="25000", end="120000, 1000", retries=0))


def test_two_bracket_lines_fail():
    with pytest.raises(AssertionError, match="2 now_ns bracket lines"):
        check_clock_diags(*run(extra_line=True))


def test_the_bracket_line_in_another_case_fails():
    with pytest.raises(AssertionError, match="now_ns_follows_the_hpet: 0 lines"):
        check_clock_diags(*run(now_ns_case="sleep_10ms_elapses"))


def test_another_limit_fails():
    with pytest.raises(AssertionError, match="now_ns_follows_the_hpet: 0 lines"):
        check_clock_diags(*run(limit=50000))


def test_timer_brackets_without_width_fail():
    with pytest.raises(AssertionError, match="over 14 brackets"):
        check_clock_diags(*run(timer_max=0))


def test_too_many_placement_tries_fail():
    check_clock_diags(*run(placement_tries=4))
    with pytest.raises(AssertionError, match="over 4 tries"):
        check_clock_diags(*run(placement_tries=5))
