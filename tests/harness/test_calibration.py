"""lumen_harness.calibration must read each verdict line the kernel prints
and catch one that doesn't follow from its own bound (#171), on the lines
CI's loaded KVM runner printed (run 44158904486) and edits of them."""

import pytest
from lumen_harness.calibration import (
    TIMER,
    TIMER_TOLERANCE,
    TSC_TOLERANCE,
    judged,
    kept_reference,
    no_other_warnings,
    timer_verdicts,
    tsc_verdicts,
)

WINDOWS = "(tsc/reference/polls/start_gap/end_gap: 61853597 Hz [24818280/627810/1/109564/366569])"
TSC_RETOOK = (
    "[    1.170906] INFO  time: tsc calibration took 4 windows again, 5 of 5 kept steady, median within 0.2% "
    "(read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: "
    "106722245/4364653/447/301472/382494)\n"
)
# A 1% timer stall, the host's on top: past 2% only through the TSC's 0.2%.
TIMER_THROUGH_THE_TSC = (
    "[    1.782188] WARN  time: local APIC timer calibration: 0 of 20 windows read promptly, but the TSC it is "
    "measured against may be off by up to 0.2%, so its rate may be off by up to 2.1%, over the 2.0% allowed "
    f"{WINDOWS}\n"
)
# A 3% timer stall: its own reads to blame.
TIMER_OWN = (
    "[    1.781000] WARN  time: local APIC timer calibration: only 0 of 20 windows read promptly (5 wanted; a "
    "stall next to the timer's register accesses), so its rate may be off by up to 3.8% (0.2% of it the TSC's "
    f"calibration), over the 2.0% allowed {WINDOWS}\n"
)
TIMER_FILLED = (
    "[    1.781000] INFO  time: local APIC timer calibration took 20 windows again, 0 of 5 kept steady, median "
    f"within 1.4% (read too late to trust; the last, {WINDOWS})\n"
)
TSC_OWN = (
    "[    1.170000] WARN  time: tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to "
    f"the PIT's reads), so its rate may be off by up to 1.6%, over the 1.0% allowed {WINDOWS}\n"
)
WEIGHED = (
    "[    1.160000] INFO  time: tsc calibration: against the HPET within 1.7% (past the 1.0% allowed), against "
    "the PIT within 1.6%: kept the PIT\n"
)


def test_the_lines_from_171s_failed_boot_are_read_as_printed():
    out = TSC_RETOOK + TIMER_THROUGH_THE_TSC
    timer = judged(out, TIMER, TIMER_TOLERANCE)
    assert (timer.warned, timer.bound, timer.tsc_share, timer.steady, timer.retaken) == (
        True,
        2.1,
        0.2,
        0,
        20,
    )
    tsc = judged(out, "tsc", TSC_TOLERANCE)
    assert (tsc.warned, tsc.bound, tsc.tsc_share, tsc.steady, tsc.retaken) == (
        False,
        0.2,
        None,
        5,
        4,
    )
    no_other_warnings(out, tsc, timer)
    own = judged(TIMER_OWN, TIMER, TIMER_TOLERANCE)
    assert (own.warned, own.bound, own.tsc_share, own.steady, own.retaken) == (
        True,
        3.8,
        0.2,
        0,
        20,
    )
    filled = judged(TIMER_FILLED, TIMER, TIMER_TOLERANCE)
    assert (filled.warned, filled.bound, filled.steady, filled.retaken) == (False, 1.4, 0, 20)
    tsc_own = judged(TSC_OWN, "tsc", TSC_TOLERANCE)
    assert (tsc_own.warned, tsc_own.bound, tsc_own.tsc_share) == (True, 1.6, None)


@pytest.mark.parametrize(
    "out",
    [
        # Within the tolerance at INFO, but its bound past it.
        TIMER_FILLED.replace("1.4%", "2.1%"),
        # Within the tolerance exactly (2.0% is 20000 ppm at most): a
        # warning is wrong.
        TIMER_OWN.replace("3.8%", "2.0%"),
        TIMER_THROUGH_THE_TSC.replace("2.1%", "1.9%"),
        # The TSC's tolerance on the timer's line: no verdict line at all.
        TIMER_OWN.replace("over the 2.0%", "over the 1.0%"),
        # Two verdicts, or none.
        TIMER_FILLED + TIMER_OWN,
        "INFO  time: local APIC timer 61.85 MHz (bus 989.66 MHz / 16), ticking at 1 kHz\n",
    ],
    ids=["info-past", "warn-at", "warn-under", "wrong-tolerance", "two", "none"],
)
def test_a_verdict_that_doesnt_follow_its_bound_fails(out):
    with pytest.raises(AssertionError):
        judged(out, TIMER, TIMER_TOLERANCE)


def test_a_clock_that_says_nothing_has_no_verdict():
    # Every window steady at once: no line (and the timer's rate line isn't one).
    out = "INFO  time: tsc 2.45 GHz (calibrated against hpet)\nINFO  time: local APIC timer 61.85 MHz\n"
    assert timer_verdicts(out) == [] and tsc_verdicts(out) == []
    assert [j.line for j in timer_verdicts(TIMER_OWN)] == [TIMER_OWN]


def test_only_the_verdicts_may_warn():
    out = TSC_RETOOK + TIMER_THROUGH_THE_TSC
    timer = judged(out, TIMER, TIMER_TOLERANCE)
    no_other_warnings(out, timer)
    with pytest.raises(AssertionError):
        no_other_warnings(out + "[    2.0] WARN  time: something else\n", timer)
    # Another subsystem's warning is its own scenarios' to judge.
    no_other_warnings(out + "[    ?.??????] WARN  acpi: no usable HPET: no HPET table\n", timer)
    # A verdict at INFO allows no warning.
    with pytest.raises(AssertionError):
        no_other_warnings(TIMER_OWN, judged(TIMER_FILLED, TIMER, TIMER_TOLERANCE))


def test_the_reference_kept_is_the_one_with_the_smaller_bound():
    assert kept_reference(TSC_RETOOK) == "hpet"
    assert kept_reference(WEIGHED) == "pit"
    assert (
        kept_reference(
            WEIGHED.replace("kept the PIT", "kept the HPET").replace(
                "PIT within 1.6", "PIT within 1.7"
            )
        )
        == "hpet"
    )
    with pytest.raises(AssertionError):
        kept_reference(WEIGHED.replace("kept the PIT", "kept the HPET"))
