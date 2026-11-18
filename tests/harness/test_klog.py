"""lumen_harness.klog's host-timed warnings and the "no warning" check."""

import re
from pathlib import Path

import pytest
from lumen_harness.klog import HOST_TIMED, unexpected_warnings

ROOT = Path(__file__).resolve().parents[2]

# One example of each host-timed warning, in HOST_TIMED's order, as the
# kernel prints it.
EXAMPLES = [
    (
        "[    1.204210] WARN  time: local APIC timer calibration: only 0 of 20 windows read promptly (5 "
        "wanted; a stall next to the timer's reads), so its rate may be off by up to 2.5%, over the 2.0% allowed"
    ),
    (
        "[    1.204210] WARN  time: tsc calibration: against the HPET within 1.4% (past the 1.0% allowed), "
        "against the PIT it failed (no information): kept the HPET"
    ),
    (
        "[    1.204210] WARN  time: can't calibrate the tsc against the HPET: only 0 of 20 windows read "
        "promptly (5 wanted), and no window is narrow enough, so the rate says nothing "
        "(tsc/reference/polls/start_gap/end_gap: 1/2/3/4/5); calibrating against the PIT"
    ),
    "[    0.981003] WARN  irq: no timer interrupt in 100 ms",
    "[    2.101000] WARN  shell: serial prompt held: shell-vt2 (ready) not yet waiting for keys (still waiting)",
    "[    9.330000] WARN  sound: a sound found the queue empty 3 times (underruns)",
]


@pytest.mark.parametrize("h", HOST_TIMED, ids=lambda h: h.text)
def test_each_host_timed_warning_is_the_kernels_text(h):
    assert h.text in (ROOT / h.source).read_text(), (h.source, h.text)


def test_each_example_is_left_out_and_only_its_own_pattern_matches():
    assert len(EXAMPLES) == len(HOST_TIMED)
    for example, h in zip(EXAMPLES, HOST_TIMED, strict=True):
        assert unexpected_warnings(example + "\n") == [], example
        rest = example.split("] ", 1)[1]
        assert [g for g in HOST_TIMED if re.match(g.pattern, rest)] == [h], example


def test_other_warnings_and_errors_are_unexpected():
    out = (
        "[    0.100000] INFO  boot: boot complete\n"
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
        "[    0.300000] ERROR mouse: 0000:00:03.0: the mouse thread (a bug): not held\n"
        "[    0.400000] WARN  irq: no timer interrupt in 100 ms\n"
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f\n"
    )
    assert unexpected_warnings(out) == [
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)",
        "[    0.300000] ERROR mouse: 0000:00:03.0: the mouse thread (a bug): not held",
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f",
    ]
    assert unexpected_warnings(out, "irq") == [
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f"
    ]
    assert unexpected_warnings(out, "time") == []


def test_a_warning_after_part_of_a_line_is_found():
    out = "lumen> ech[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
    assert unexpected_warnings(out) == [
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)"
    ]


def test_a_host_timed_text_with_more_after_it_is_still_unexpected_where_anchored():
    # The irq warning is matched whole: a longer line is something else.
    out = "[    0.400000] WARN  irq: no timer interrupt in 100 ms on cpu 3\n"
    assert unexpected_warnings(out) == [out.rstrip("\n")]
