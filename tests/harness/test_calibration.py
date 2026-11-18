"""lumen_harness.calibration must read each verdict line the kernel prints
and catch one that doesn't follow from the numbers it prints (#171): on the
lines CI's loaded KVM runner printed in #171's two failed boots (run
44158904486, verbatim), and edits of them."""

import pytest
from lumen_harness.calibration import (
    TIMER,
    TSC,
    Judged,
    judged,
    no_other_warnings,
    percent,
    verdict,
    weighed,
)

# The 1% timer stall's boot: the TSC retook a window, within 0.2%; the timer
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
# Made up, in the kernel's words (time.rs's report ktest pins them).
TIMER_FILLED = (
    "[    1.781000] INFO  time: local APIC timer calibration took 20 windows again, 0 of 5 kept steady, median "
    "within 1.4% (read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: "
    "24818280/627810/1/109564/366569)\n"
)
TSC_OWN = (
    "[    1.170000] WARN  time: tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to "
    "the PIT's reads), so its rate may be off by up to 1.6%, over the 1.0% allowed (tsc/reference/polls/"
    "start_gap/end_gap: 2457600000 Hz [24576000/1000000/2000/1000/400000] retook 20)\n"
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
        TIMER_OWN, "own_reads", bound=3.8, steady=0, retaken=20, tsc_share=0.2
    )


def test_the_other_lines_are_read_as_printed():
    assert judged(TIMER_FILLED, TIMER) == Judged(
        TIMER_FILLED, "retook", bound=1.4, steady=0, retaken=20, tsc_share=None
    )
    assert judged(TSC_OWN, TSC) == Judged(
        TSC_OWN, "own_reads", bound=1.6, steady=0, retaken=20, tsc_share=None
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
