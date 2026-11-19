"""lumen_harness.calibration must read each verdict line the kernel prints
and catch one that doesn't follow from the numbers it prints (#171): on the
lines CI's loaded KVM runner printed in #171's two failed boots (run
44158904486, verbatim), and edits of them."""

import re

import pytest
from lumen_harness.calibration import (
    HPET,
    PIT,
    TIMER,
    TSC,
    Judged,
    agree,
    assert_quiet_boot,
    follows_the_tsc,
    judged,
    no_other_warnings,
    percent,
    ppm_range,
    stalled,
    tsc_reference,
    unexpected_boot_warnings,
    verdict,
    warned_exactly_when_past,
    weighed,
)

# The 1% timer stall's boot: the TSC took 4 windows again, within 0.2%; the timer
# past 2% only through the TSC's 0.2%.
TSC_RETOOK = (
    "[    1.170906] INFO  time: tsc calibration took 4 windows again, 5 of 5 kept steady, median within 0.2% "
    "(read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: "
    "106722245/4364653/447/301472/382494)\n"
)
TIMER_THROUGH_THE_TSC = (
    "[    1.782188] WARN  time: local APIC timer calibration: 0 of 20 windows read promptly, but the TSC it is "
    "measured against may be off by up to 0.2%, so its rate may be off by up to 2.1%, over the 2.0% allowed "
    "(tsc/reference/polls/start_gap/end_gap: 61853597 Hz [24818280/627810/1/109564/366569 "
    "24817495/627907/1/110838/365809 24823081/627743/1/106673/371346 24820975/627874/1/113043/369240 "
    "24824037/627984/1/110642/372326] retook 20)\n"
)
# The 3% timer stall's boot: its own reads to blame.
TIMER_OWN = (
    "[    1.313905] WARN  time: local APIC timer calibration: only 0 of 20 windows read promptly (5 wanted; a "
    "stall next to the timer's register accesses), so its rate may be off by up to 3.8% (0.2% of it the TSC's "
    "calibration), over the 2.0% allowed (tsc/reference/polls/start_gap/end_gap: 60659182 Hz "
    "[25270182/626985/1/79649/821387 25272877/627039/1/79257/824106 25260235/627270/1/92316/811440 "
    "25296642/627193/1/78963/847822 25304598/627753/1/88225/839370] retook 20)\n"
)
# Made up, in the kernel's words (time.rs's report ktest pins the verdicts',
# the_steadier the weighing lines'; the kernel prints five windows).
TIMER_FILLED = (
    "[    1.781000] INFO  time: local APIC timer calibration took 20 windows again, 0 of 5 kept steady, median "
    "within 1.4% (read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: "
    "24818280/627810/1/109564/366569)\n"
)
TSC_OWN = (
    "[    1.170000] WARN  time: tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to "
    "the PIT's reads), so its rate may be off by up to 1.6%, over the 1.0% allowed (tsc/reference/polls/"
    "start_gap/end_gap: 2457600000 Hz [24576000/12000/2000/1000/400000 24576000/12000/2000/1000/400000 "
    "24576000/12000/2000/1000/400000 24576000/12000/2000/1000/400000 24576000/12000/2000/1000/400000] "
    "retook 20)\n"
)
WEIGHED = (
    "[    1.160000] INFO  time: tsc calibration: against the HPET within 1.7% (past the 1.0% allowed), against "
    "the PIT within 1.6%: kept the PIT\n"
)
WEIGHED_TIE = WEIGHED.replace("PIT within 1.6", "PIT within 1.7")
NO_BOUND = (
    "[    1.160000] INFO  time: tsc calibration: against the HPET within 1.7% (past the 1.0% allowed), against "
    "the PIT with no bound: kept the HPET\n"
)
PIT_FAILED = (
    "[    1.160000] WARN  time: tsc calibration: against the HPET within 1.7% (past the 1.0% allowed), against "
    "the PIT it failed (the PIT didn't count): kept the HPET\n"
)


def test_the_lines_from_171s_failed_boots_are_read_as_printed():
    out = TSC_RETOOK + TIMER_THROUGH_THE_TSC
    assert judged(out, TIMER) == Judged(
        TIMER_THROUGH_THE_TSC, "through_tsc", bound=2.1, steady=0, retaken=20, tsc_share=0.2
    )
    assert judged(out, TSC) == Judged(
        TSC_RETOOK, "retook", bound=0.2, steady=5, retaken=4, tsc_share=None
    )
    no_other_warnings(out)
    assert judged(TIMER_OWN, TIMER) == Judged(
        TIMER_OWN,
        "own_reads",
        bound=3.8,
        steady=0,
        retaken=20,
        tsc_share=0.2,
        reads="the timer's register accesses",
    )


def test_the_other_lines_are_read_as_printed():
    assert judged(TIMER_FILLED, TIMER) == Judged(
        TIMER_FILLED, "retook", bound=1.4, steady=0, retaken=20, tsc_share=None
    )
    assert judged(TSC_OWN, TSC) == Judged(
        TSC_OWN, "own_reads", bound=1.6, steady=0, retaken=20, reads="the PIT's reads"
    )
    # Retaken counts only the unsteady windows.
    some_steady = judged(TIMER_OWN.replace("only 0 of 20", "only 2 of 20"), TIMER)
    assert (some_steady.steady, some_steady.retaken) == (2, 18)
    every_steady = judged(TIMER_THROUGH_THE_TSC.replace("0 of 20", "5 of 5"), TIMER)
    assert (every_steady.steady, every_steady.retaken) == (5, 0)


@pytest.mark.parametrize(
    "out, clock",
    [
        # Within the tolerance at INFO, but its bound past it.
        (TIMER_FILLED.replace("1.4%", "2.1%"), TIMER),
        # A warning at exactly the tolerance (2.0% is 20000 ppm at most), or
        # under it.
        (TIMER_OWN.replace("3.8%", "2.0%"), TIMER),
        (TIMER_THROUGH_THE_TSC.replace("2.1%", "1.9%"), TIMER),
        (TSC_OWN.replace("1.6%", "0.9%"), TSC),
        # Another clock's tolerance: not a verdict line at all.
        (TIMER_OWN.replace("over the 2.0%", "over the 1.0%"), TIMER),
        (TSC_OWN.replace("over the 1.0%", "over the 2.0%"), TSC),
        # A level that doesn't fit the wording.
        (TIMER_FILLED.replace("INFO ", "WARN "), TIMER),
        # Two verdicts, or none (the timer's rate line isn't one).
        (TIMER_FILLED + TIMER_OWN, TIMER),
        (
            "[    1.816020] INFO  time: local APIC timer 61.85 MHz (bus 989.66 MHz / 16), ticking at 1 kHz\n",
            TIMER,
        ),
        # The timer's bound under the TSC's share.
        (TIMER_OWN.replace("3.8%", "2.1%").replace("0.2% of it", "2.5% of it"), TIMER),
        # "Its own reads" with its own windows provably within 2%: 3.3% less
        # the TSC's 3.1% leaves under 0.3%.
        (TIMER_OWN.replace("3.8%", "3.3%").replace("0.2% of it", "3.1% of it"), TIMER),
        # "But the TSC" with its own windows provably past 2%: 3.8% less 0.2%.
        (TIMER_THROUGH_THE_TSC.replace("up to 2.1%", "up to 3.8%"), TIMER),
        # The timer's wording or reads on the TSC's line, and the reverse.
        (TSC_OWN.replace("the PIT's reads", "the timer's register accesses"), TSC),
        (TSC_OWN.replace("1.6%, over", "1.6% (0.2% of it the TSC's calibration), over"), TSC),
        (
            TIMER_THROUGH_THE_TSC.replace("local APIC timer", "tsc").replace(
                "2.0% allowed", "1.0% allowed"
            ),
            TSC,
        ),
        (TIMER_OWN.replace("the timer's register accesses", "the HPET's reads"), TIMER),
    ],
    ids=[
        "info-past",
        "warn-at",
        "warn-under",
        "tsc-warn-under",
        "timer-wrong-tolerance",
        "tsc-wrong-tolerance",
        "info-text-at-warn",
        "two",
        "none",
        "under-the-tsc",
        "own-reads-within",
        "through-tsc-past",
        "tsc-timer-reads",
        "tsc-share",
        "tsc-through-tsc",
        "timer-hpet-reads",
    ],
)
def test_a_verdict_that_doesnt_follow_its_numbers_fails(out, clock):
    with pytest.raises(AssertionError):
        judged(out, clock)


def test_a_clock_that_says_nothing_has_no_verdict():
    # Every window steady at once: no line. Nor is the TSC's HPET-or-PIT
    # choice a verdict (the PIT kept with every window steady says nothing
    # more).
    out = WEIGHED + "[    1.170906] INFO  time: tsc 2.45 GHz (calibrated against pit)\n"
    assert verdict(out, TSC) is None and verdict(out, TIMER) is None
    no_other_warnings(out)


def test_only_the_verdicts_may_warn_in_time():
    out = TSC_RETOOK + TIMER_THROUGH_THE_TSC
    for extra in [
        "[    2.000000] WARN  time: something else\n",
        "[    2.000000] ERROR time: something else\n",
    ]:
        with pytest.raises(AssertionError):
            no_other_warnings(out + extra)
    # Another subsystem's warning is its own scenarios' to judge.
    no_other_warnings(out + "[    ?.??????] WARN  acpi: no usable HPET: no HPET table\n")
    # A broken PIT's warning is no verdict.
    with pytest.raises(AssertionError):
        no_other_warnings(PIT_FAILED)


def test_the_reference_kept_is_the_one_with_the_smaller_bound():
    assert weighed(TSC_RETOOK) is None
    assert (weighed(WEIGHED).kept, weighed(WEIGHED).kept_bound) == ("pit", 1.6)
    # Equal tenths may go either way: the kernel compares ppm.
    assert weighed(WEIGHED_TIE).kept == "pit"
    assert weighed(WEIGHED_TIE.replace("kept the PIT", "kept the HPET")).kept == "hpet"
    # The PIT with no bound, or failed: the HPET.
    assert (weighed(NO_BOUND).kept, weighed(NO_BOUND).pit) == ("hpet", None)
    assert weighed(PIT_FAILED).kept == "hpet"
    for bad in [
        # The larger one kept.
        WEIGHED.replace("kept the PIT", "kept the HPET"),
        NO_BOUND.replace("kept the HPET", "kept the PIT"),
        # A failed PIT said at INFO, or a weighing at WARN.
        PIT_FAILED.replace("WARN ", "INFO "),
        WEIGHED.replace("INFO ", "WARN "),
        # Reworded: it starts like one, so it must be one.
        WEIGHED.replace(": kept the PIT", "; kept the PIT"),
        # Two.
        WEIGHED + WEIGHED,
    ]:
        with pytest.raises(AssertionError):
            weighed(bad)


@pytest.mark.parametrize("hpet", ["0.7", "1.0"])
def test_an_hpet_within_the_tolerance_is_never_weighed(hpet):
    # #139: an unsteady HPET within the TSC's 1% is kept, not given up for
    # the PIT, which stalls with it.
    within = WEIGHED.replace("HPET within 1.7%", f"HPET within {hpet}%")
    with pytest.raises(AssertionError):
        weighed(within.replace("PIT within 1.6", "PIT within 0.6"))


def test_percent_rounds_up_to_a_tenth():
    printed = [percent(p) for p in (0, 1, 1_000, 1_001, 20_000, 20_001, 999_999)]
    assert printed == ["0.0%", "0.1%", "0.1%", "0.2%", "2.0%", "2.1%", "100.0%"]


@pytest.mark.parametrize(
    "out, clock",
    [
        # At exactly the tolerance: within it, an INFO line.
        pytest.param(TIMER_FILLED.replace("1.4%", "2.0%"), TIMER, id="timer-info-at"),
        pytest.param(TSC_RETOOK.replace("within 0.2%", "within 1.0%"), TSC, id="tsc-info-at"),
        # Its own reads at the least bound that allows them: w > 2% needs
        # w + s + ws >= 20001 + 1001 + 21 (2.2% with 0.2%).
        pytest.param(TIMER_OWN.replace("3.8%", "2.2%"), TIMER, id="own-reads-least"),
        # Through the TSC at the most it can be: w <= 2%, s <= 0.2%: 2.3%.
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("up to 2.1%", "up to 2.3%"), TIMER, id="through-tsc-most"
        ),
        # Its own windows near nothing: the bound rounds to the TSC's.
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("0 of 20", "5 of 5")
            .replace("up to 0.2%", "up to 2.5%")
            .replace("up to 2.1%", "up to 2.5%"),
            TIMER,
            id="bound-is-share",
        ),
        # A large TSC share: w <= 2%, s <= 6%: 8.2% (needs the division).
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("up to 0.2%", "up to 6.0%").replace(
                "up to 2.1%", "up to 8.2%"
            ),
            TIMER,
            id="big-share",
        ),
    ],
)
def test_a_verdict_at_the_edge_of_its_numbers_is_read(out, clock):
    judged(out, clock)


@pytest.mark.parametrize(
    "out, clock",
    [
        pytest.param(TSC_OWN.replace("1.6%, over", "1.0%, over"), TSC, id="tsc-warn-at"),
        pytest.param(TIMER_OWN.replace("3.8%", "2.1%"), TIMER, id="own-reads-just-within"),
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("up to 2.1%", "up to 2.4%"),
            TIMER,
            id="through-tsc-just-past",
        ),
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("up to 0.2%", "up to 2.5%"),
            TIMER,
            id="through-under-the-tsc",
        ),
        # Past the division's edge: w <= 2%, s <= 6% can't print 8.3%.
        pytest.param(
            TIMER_THROUGH_THE_TSC.replace("up to 0.2%", "up to 6.0%").replace(
                "up to 2.1%", "up to 8.3%"
            ),
            TIMER,
            id="big-share-past",
        ),
        # A timer warning that doesn't name the TSC's share.
        pytest.param(
            TIMER_OWN.replace(" (0.2% of it the TSC's calibration)", ""), TIMER, id="no-share"
        ),
        # Windows that can't be: an INFO line with nothing retaken (every
        # window steady at once says nothing), more steady than kept, fewer
        # taken than steady.
        pytest.param(
            TIMER_FILLED.replace("took 20 windows", "took 0 windows"), TIMER, id="retook-none"
        ),
        pytest.param(
            TIMER_FILLED.replace("0 of 5 kept", "9 of 5 kept"), TIMER, id="steady-past-five"
        ),
        pytest.param(
            TIMER_OWN.replace("only 0 of 20", "only 4 of 2"), TIMER, id="taken-under-steady"
        ),
        # Reworded: it starts like a verdict, so it must be one.
        pytest.param(TIMER_FILLED.replace("kept steady", "held steady"), TIMER, id="reworded"),
    ],
)
def test_a_verdict_just_past_the_edge_fails(out, clock):
    with pytest.raises(AssertionError):
        judged(out, clock)


def test_a_reworded_verdict_is_no_silence():
    with pytest.raises(AssertionError):
        verdict(TIMER_FILLED.replace("kept steady", "held steady"), TIMER)


def test_weighing_edges():
    # The least the HPET can be weighed at: 1.1%.
    w = weighed(
        WEIGHED.replace("HPET within 1.7%", "HPET within 1.1%").replace(
            "PIT within 1.6", "PIT within 1.0"
        )
    )
    assert (w.kept, w.kept_bound) == (PIT, 1.0)
    for bad in [
        WEIGHED.replace("past the 1.0%", "past the 2.0%"),
        WEIGHED + WEIGHED.replace(": kept", "; kept"),
    ]:
        with pytest.raises(AssertionError):
            weighed(bad)
    assert weighed(NO_BOUND).kept_bound == 1.7


def test_a_tsc_warning_is_a_verdict():
    no_other_warnings(TSC_OWN)


def test_ppm_range_is_what_a_percent_can_be():
    assert ppm_range(2.1) == (20_000, 21_000)
    assert all(
        low < p <= high
        for p in range(1, 100_000, 7)
        for low, high in [ppm_range(float(percent(p)[:-1]))]
    )


@pytest.mark.parametrize(
    "out, clock, stall_ppm",
    [
        # #171's boots: the timer's 1% and 3% stalls.
        pytest.param(TSC_RETOOK + TIMER_THROUGH_THE_TSC, TIMER, 10_000, id="timer-1%"),
        pytest.param(TIMER_OWN, TIMER, 30_000, id="timer-3%"),
        # At the floor: the timer's share of 1% is 0.99%, printed 1.0%.
        pytest.param(TIMER_FILLED.replace("1.4%", "1.0%"), TIMER, 10_000, id="timer-floor"),
        # The TSC's is the stall itself: 0.6% for 0.6%.
        pytest.param(
            TSC_OWN.replace("1.6%", "1.1%").replace("PIT's", "HPET's"), TSC, 11_000, id="tsc-floor"
        ),
    ],
)
def test_a_stall_reaches_the_bound(out, clock, stall_ppm):
    stalled(out, clock, stall_ppm)


@pytest.mark.parametrize(
    "out, clock, stall_ppm",
    [
        # Under the floor.
        pytest.param(TIMER_FILLED.replace("1.4%", "0.9%"), TIMER, 10_000, id="timer-under"),
        pytest.param(TSC_OWN.replace("1.6%", "1.1%"), TSC, 12_000, id="tsc-under"),
        # Not every window taken, or one kept steady.
        pytest.param(TIMER_FILLED.replace("took 20", "took 19"), TIMER, 10_000, id="nineteen"),
        pytest.param(
            TIMER_FILLED.replace("took 20", "took 19").replace("0 of 5", "1 of 5"),
            TIMER,
            10_000,
            id="one-steady",
        ),
    ],
)
def test_a_stall_short_of_the_bound_fails(out, clock, stall_ppm):
    with pytest.raises(AssertionError):
        stalled(out, clock, stall_ppm)


def test_a_tsc_warning_names_the_reference_kept():
    stalled(TSC_OWN, TSC, 15_000, reference=PIT)
    with pytest.raises(AssertionError):
        stalled(TSC_OWN, TSC, 15_000, reference=HPET)


def test_the_verdicts_follow_the_tsc_clocks_prints():
    # #171's 1% boot: the TSC within 0.2% (1,6xx ppm), the timer's share 0.2%.
    out = TSC_RETOOK + TIMER_THROUGH_THE_TSC
    follows_the_tsc(out, 1_650)
    for tsc_ppm in (2_500, 900):
        # The TSC's line or the timer's share disagrees.
        with pytest.raises(AssertionError):
            follows_the_tsc(out, tsc_ppm)
    # The TSC silent (every window steady): the timer alone, against it.
    follows_the_tsc(TIMER_OWN, 1_200)
    with pytest.raises(AssertionError):
        follows_the_tsc(TIMER_OWN, 3_000)
    # A timer INFO line under the TSC's bound.
    with pytest.raises(AssertionError):
        follows_the_tsc(TIMER_FILLED.replace("1.4%", "0.2%"), 3_000)


def test_a_warning_comes_exactly_when_the_bound_is_past_the_tolerance():
    warned_exactly_when_past("", TSC, 10_000)
    warned_exactly_when_past(TSC_OWN, TSC, 16_000)
    warned_exactly_when_past(TIMER_FILLED, TIMER, 14_000)
    with pytest.raises(AssertionError, match="did not warn"):
        warned_exactly_when_past("", TSC, 10_001)
    with pytest.raises(AssertionError, match="did not warn"):
        warned_exactly_when_past(TIMER_FILLED, TIMER, 20_001)
    with pytest.raises(AssertionError, match="but boot warned"):
        warned_exactly_when_past(TSC_OWN, TSC, 10_000)
    # The line prints the bound it was given (rounded up to a tenth).
    with pytest.raises(AssertionError, match="verdict prints 1.6%"):
        warned_exactly_when_past(TSC_OWN, TSC, 15_000)
    with pytest.raises(AssertionError, match="verdict prints 1.6%"):
        warned_exactly_when_past(TSC_OWN, TSC, 16_001)
    with pytest.raises(AssertionError, match="verdict prints 1.4%"):
        warned_exactly_when_past(TIMER_FILLED, TIMER, 15_000)
    # With the counts: a line exactly when windows were taken again or the
    # bound is past the tolerance, saying them.
    warned_exactly_when_past("", TSC, 10_000, 0, 5)
    warned_exactly_when_past(TIMER_FILLED, TIMER, 14_000, 20, 0)
    with pytest.raises(AssertionError, match="4 windows taken again"):
        warned_exactly_when_past("", TSC, 2_000, 4, 5)
    with pytest.raises(AssertionError, match="says 20 taken again and 0 steady, not 19 and 1"):
        warned_exactly_when_past(TIMER_FILLED, TIMER, 14_000, 19, 1)


def test_two_calibrations_agree_within_2_or_their_bounds_under_9():
    hz = 1_000_000_000
    # Within 2%: whatever the bounds.
    agree(1_020_000_000, hz, 1_000, 1_000)
    agree(hz, hz, 50_000, 50_000)
    with pytest.raises(AssertionError, match="disagree"):
        agree(1_021_000_000, hz, 1_000, 1_000)
    # Past it: within the bounds, under the cap (b the reference, as the
    # kernel's pit_agrees takes the boot).
    agree(1_040_000_000, hz, 30_000, 20_000)
    agree(1_049_000_000, hz, 30_000, 20_000)
    agree(1_080_000_000, hz, 60_000, 25_000)
    with pytest.raises(AssertionError, match="disagree"):
        agree(1_060_000_000, hz, 30_000, 20_000)
    with pytest.raises(AssertionError, match="too wide to tell"):
        agree(1_030_000_000, hz, 60_000, 30_000)
    # #45's 11.8%, bounds just under the cap, either side high.
    with pytest.raises(AssertionError, match="disagree"):
        agree(1_118_000_000, hz, 60_000, 29_000)
    with pytest.raises(AssertionError, match="disagree"):
        agree(hz, 1_118_000_000, 1_000, 88_000)


FALLBACK = (
    "[    1.204210] WARN  time: can't calibrate the tsc against the HPET: only 0 of 20 windows read promptly "
    "(5 wanted), and the middle ones' end stalls were as long as the rest of their windows, so the rate says "
    "nothing (tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5); calibrating against the PIT\n"
)


def test_the_tsc_reference_is_the_kernels_choice():
    assert tsc_reference("") == HPET
    assert tsc_reference(TSC_OWN + WEIGHED) == PIT
    assert tsc_reference(TSC_OWN + NO_BOUND) == HPET
    # The HPET's calibration said nothing: the PIT, with no weighing.
    assert tsc_reference(FALLBACK) == PIT


def test_the_fallback_line_is_checked_against_its_numbers():
    # 5 steady would have had a bound; fewer than 20 taken, attempts left.
    for wrong in ("only 5 of 20", "only 0 of 19"):
        with pytest.raises(AssertionError):
            tsc_reference(FALLBACK.replace("only 0 of 20", wrong))
    with pytest.raises(AssertionError):
        tsc_reference(FALLBACK + FALLBACK)
    with pytest.raises(AssertionError, match="and a weighing"):
        tsc_reference(FALLBACK + WEIGHED)


def test_unexpected_boot_warnings_checks_the_calibration_lines():
    # Well-formed verdicts and weighing: left out.
    assert unexpected_boot_warnings(TSC_OWN + WEIGHED + TIMER_OWN) == []
    assert unexpected_boot_warnings(FALLBACK) == []
    # A broken PIT's warning is not the host's.
    assert unexpected_boot_warnings(TSC_OWN + PIT_FAILED) == [PIT_FAILED.rstrip("\n")]
    # A line that contradicts its own numbers fails, though host-timed: the
    # TSC's verdict, the timer's, the weighing, the fallback.
    for wrong in (
        TSC_OWN.replace("1.6%", "0.2%"),
        TSC_OWN + TIMER_OWN.replace("3.8%", "1.0%"),
        TSC_OWN + WEIGHED.replace("kept the PIT", "kept the HPET"),
        FALLBACK.replace("only 0 of 20", "only 5 of 20"),
    ):
        with pytest.raises(AssertionError):
            unexpected_boot_warnings(wrong)
    # One that follows part of a line, where those checks wouldn't see it.
    with pytest.raises(AssertionError):
        unexpected_boot_warnings("abc" + TSC_OWN)


def test_assert_quiet_boot_names_the_unexpected_lines():
    assert_quiet_boot(TSC_OWN + WEIGHED + TIMER_OWN)
    with pytest.raises(AssertionError, match=re.escape(PIT_FAILED.rstrip("\n"))):
        assert_quiet_boot(TSC_OWN + PIT_FAILED)
