"""check_clock_diags must pass a good run and catch each way the clock
cross-checks' diagnostics can go wrong (#117), on synthetic TAP."""

import pytest
from lumen_harness import tap
from lumen_harness.clock_diags import check_clock_diags, placement_is_judged

NOW_NS = (
    "# bracket widths: start [{start}] ns, end [{end}] ns ({retries} taken again, 20 ms "
    "apart; at most {limit} ns trusted)\n"
    "# now_ns moved 50000000 ns, the HPET 50001000 ns: 1000 ns apart, of a 1000020 ns budget\n"
)
TIMER = (
    "# bracket widths min/median/max 0/{median}/{max} ns over {brackets} brackets; 0 of 7 one-shot windows "
    "with brackets over 5% of them\n"
    "# samples [62500000, 62500100, 62499900, 62500000, 62500000, 62500000, 62500000] Hz\n"
    "# timer counts at 62.5 MHz (calibrated 62.5 MHz)\n"
)
BOOT_TIMER = (
    "# timer boot calibration 62500000 Hz [{windows}] retook {retook}, {steady} steady, "
    "median within {bound} ppm; window rates [{rates}] Hz\n"
)
BOOT_TSC = (
    "# tsc boot calibration (hpet) 2457600000 Hz [{windows}] retook {retook}, {steady} steady, "
    "median within {bound} ppm; window rates [{rates}] Hz\n"
)
TSC_WINDOW = "24576000/1000000/2000/1000/1000"
WINDOW = "10000000/625000/1/0/0"
PLACEMENT = (
    "# injected pause: {what} placement {off} ns off (at most 10000000 ns allowed) on try {taken} "
    "of 8, 20 ms apart; tries (reference, paused, by the brackets, by the read, in ns) [{tries}]\n"
)
# A try a 20 ms stall keeps from being judged, and one judged.
STALLED_TRY = (30000, 60000000, 20000000, 20000000)


def placement(
    what="HPET", before=(), last=(30000, 40048000, 20000000, 20005000), off=None, taken=None
):
    """A placement test's judged line: the tries `before` it, then `last`."""
    tries = [*before, last]
    off = abs(last[2] - last[3]) if off is None else off
    listed = ", ".join(f"({a}, {b}, {c}, {d})" for a, b, c, d in tries)
    return PLACEMENT.format(what=what, off=off, taken=taken or len(tries), tries=listed)


def run(
    start="25000",
    end="1000",
    retries=0,
    limit=100000,
    now_ns_case="now_ns_follows_the_hpet",
    extra_line=False,
    timer_max=39000,
    timer_brackets=14,
    boot_lines=1,
    boot_windows=5,
    boot_rates=5,
    boot_steady=5,
    boot_retook=7,
    boot_bound=100,
    tsc_lines=1,
    tsc_steady=5,
    tsc_bound=100,
    hpet_placement=None,
    timer_placement=None,
    injected_retries=2,
):
    hpet_placement = hpet_placement or placement("HPET")
    timer_placement = timer_placement or placement(
        "timer", last=(18000, 40029000, 20000000, 20000500)
    )
    now_ns = NOW_NS.format(start=start, end=end, retries=retries, limit=limit)
    cases = [
        (now_ns_case, now_ns + (now_ns if extra_line else "")),
        (
            "an_hpet_reading_is_placed_at_its_brackets_midpoint",
            hpet_placement,
        ),
        (
            "a_stalled_hpet_bracket_is_read_again",
            "# injected pause: bracket widths [246000, 101000, 1000] ns\n",
        ),
        (
            "one_stalled_reading_at_each_end_still_passes",
            (
                "# injected pause: bracket widths: start [5051000, 1000] ns, end [5028000, 1000] "
                f"ns ({injected_retries} taken again, 20 ms apart; at most 100000 ns trusted)\n"
            ),
        ),
        (
            "every_stalled_hpet_bracket_is_a_failure",
            (
                "# injected pause: bracket widths: start [201000, 201000, 201000, 201000, 201000, "
                "201000, 201000, 201000, 201000] ns, end [] ns (8 taken again, 20 ms apart; at most "
                "100000 ns trusted)\n"
            ),
        ),
        (
            "the_timer_is_programmed_for_1_khz",
            TIMER.format(median=min(1000, timer_max), max=timer_max, brackets=timer_brackets),
        ),
        ("a_timer_reading_is_placed_at_its_brackets_midpoint", timer_placement),
        (
            "the_boot_timer_calibration_is_within_what_the_tick_needs",
            (
                BOOT_TIMER.format(
                    windows=" ".join([WINDOW] * boot_windows),
                    retook=boot_retook,
                    steady=boot_steady,
                    bound=boot_bound,
                    rates=", ".join(["62500000"] * boot_rates),
                )
                * boot_lines
            ),
        ),
        (
            "the_boot_tsc_calibration_is_within_what_now_ns_needs",
            (
                BOOT_TSC.format(
                    windows=" ".join([TSC_WINDOW] * 5),
                    retook=0,
                    steady=tsc_steady,
                    bound=tsc_bound,
                    rates=", ".join(["2457600000"] * 5),
                )
                * tsc_lines
            ),
        ),
    ]
    if now_ns_case != "now_ns_follows_the_hpet":
        cases.append(("now_ns_follows_the_hpet", ""))
    text = f"TAP version 14\n1..{len(cases)}\n"
    for n, (name, diags) in enumerate(cases, 1):
        text += f"{diags}ok {n} - lumen_kernel::time::{name}\n"
    return tap.parse(text), text


def test_a_good_run_passes():
    check_clock_diags(*run())


def test_four_retries_pass():
    # One burst of slow reads on one end, sampled around 20 ms apart.
    start = "150000, 140000, 130000, 120000, 25000"
    check_clock_diags(*run(start=start, end="1000", retries=4))


def test_five_retries_fail():
    with pytest.raises(AssertionError, match="over 4 retries"):
        check_clock_diags(
            *run(start="150000, 140000, 130000, 25000", end="120000, 110000, 1000", retries=5)
        )


def test_a_retry_count_that_doesnt_match_the_readings_fails():
    # The diag counting only the start's retries, in the real check or in an
    # injected one that retakes both ends.
    with pytest.raises(AssertionError, match="retries don't match"):
        check_clock_diags(*run(start="25000", end="120000, 1000", retries=0))
    with pytest.raises(AssertionError, match="retries don't match"):
        check_clock_diags(*run(injected_retries=1))


def test_two_bracket_lines_fail():
    with pytest.raises(AssertionError, match="2 now_ns bracket lines"):
        check_clock_diags(*run(extra_line=True))


def test_the_bracket_line_in_another_case_fails():
    with pytest.raises(AssertionError, match="now_ns_follows_the_hpet: 0 lines"):
        check_clock_diags(*run(now_ns_case="sleep_10ms_elapses"))


def test_another_limit_fails():
    with pytest.raises(AssertionError, match="not a bracket line"):
        check_clock_diags(*run(limit=50000))


def test_timer_brackets_without_width_fail():
    with pytest.raises(AssertionError, match="over 14 brackets"):
        check_clock_diags(*run(timer_max=0))


@pytest.mark.parametrize("brackets", [12, 15])
def test_timer_brackets_other_than_two_per_window_fail(brackets):
    with pytest.raises(AssertionError, match=f"over {brackets} brackets"):
        check_clock_diags(*run(timer_brackets=brackets))


def test_a_missing_timer_boot_calibration_line_fails():
    with pytest.raises(AssertionError, match="0 timer boot calibration lines"):
        check_clock_diags(*run(boot_lines=0))


@pytest.mark.parametrize("steady", [0, 2])
def test_a_boot_calibration_with_most_windows_unsteady_passes_within_its_tolerance(steady):
    # Least-unsteady windows filled in (#139): the bound decides, not the count.
    check_clock_diags(*run(boot_steady=steady, boot_bound=7000))
    check_clock_diags(*run(tsc_steady=steady, tsc_bound=7000))


def test_two_timer_boot_calibration_lines_fail():
    with pytest.raises(AssertionError, match="2 timer boot calibration lines"):
        check_clock_diags(*run(boot_lines=2))


@pytest.mark.parametrize("windows, rates", [(4, 5), (5, 4), (6, 6)])
def test_a_timer_boot_calibration_without_5_windows_and_rates_fails(windows, rates):
    with pytest.raises(AssertionError, match="not 5 windows and rates"):
        check_clock_diags(*run(boot_windows=windows, boot_rates=rates))


def test_a_timer_boot_calibration_with_more_steady_than_windows_fails():
    with pytest.raises(AssertionError, match="more steady than windows"):
        check_clock_diags(*run(boot_steady=6))


def test_a_timer_boot_calibration_bound_over_its_tolerance_fails():
    check_clock_diags(*run(boot_bound=20000))
    with pytest.raises(AssertionError, match="timer median bound over 20000 ppm"):
        check_clock_diags(*run(boot_bound=20001))


def test_a_tsc_boot_calibration_bound_over_its_tolerance_fails():
    check_clock_diags(*run(tsc_bound=10000))
    with pytest.raises(AssertionError, match="tsc median bound over 10000 ppm"):
        check_clock_diags(*run(tsc_bound=10001))


@pytest.mark.parametrize("lines", [0, 2])
def test_other_than_one_tsc_boot_calibration_line_fails(lines):
    with pytest.raises(AssertionError, match=f"{lines} tsc boot calibration lines"):
        check_clock_diags(*run(tsc_lines=lines))


def test_a_tsc_boot_calibration_with_more_steady_than_windows_fails():
    with pytest.raises(AssertionError, match="more steady than windows"):
        check_clock_diags(*run(tsc_steady=6))


def test_any_number_of_timer_boot_retakes_passes():
    # Retakes count a loaded host's stalls at boot: not bounded.
    check_clock_diags(*run(boot_retook=60))
    check_clock_diags(*run(boot_steady=3, boot_retook=17))


def test_a_placement_too_far_off_fails():
    check_clock_diags(*run(hpet_placement=placement(last=(30000, 40048000, 20000000, 30000000))))
    with pytest.raises(AssertionError, match="placed too far off"):
        check_clock_diags(
            *run(hpet_placement=placement(last=(30000, 40048000, 20000000, 30000001)))
        )


def test_a_placement_on_its_last_try_passes():
    # Any number of tries taken again (a loaded host's stalls), each one
    # that couldn't be judged: not bounded.
    check_clock_diags(*run(timer_placement=placement("timer", before=[STALLED_TRY] * 7)))


def test_a_placement_line_must_follow_from_its_tries():
    bad = {
        "another clock's line": {"timer_placement": placement("HPET")},
        "not one listed try per try taken": {"hpet_placement": placement(taken=2)},
        "judged a try it can't": {"hpet_placement": placement(last=STALLED_TRY)},
        "took a try again that could be judged": {
            "hpet_placement": placement(before=[(30000, 40048000, 20000000, 20000000)])
        },
        "off isn't its try's two times apart": {"hpet_placement": placement(off=4999)},
    }
    for match, kwargs in bad.items():
        with pytest.raises(AssertionError, match=match):
            check_clock_diags(*run(**kwargs))


def test_the_harness_judges_tries_as_lumen_time_does():
    # crates/lumen-time's a_placement_try_is_judged_within_a_quarter_of_the_pause,
    # case for case.
    assert placement_is_judged(100, 800, 400)
    assert not placement_is_judged(101, 800, 400)
    assert placement_is_judged(0, 1000, 400)
    assert not placement_is_judged(0, 1001, 400)
