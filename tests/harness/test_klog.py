"""lumen_harness.klog's host-timed warnings and the "no warning" check."""

import re
from pathlib import Path

import pytest
from lumen_harness.klog import (
    HOST_TIMED,
    UNDERRUNS,
    is_host_timed,
    unexpected_warnings,
    without_stamp,
)

ROOT = Path(__file__).resolve().parents[2]
EXAMPLES = [(e, h) for h in HOST_TIMED for e in h.examples]


@pytest.mark.parametrize("h", HOST_TIMED, ids=lambda h: h.text)
def test_each_host_timed_text_is_in_the_kernel_once(h):
    assert (ROOT / h.source).read_text().count(h.text) == 1, (h.source, h.text)


def test_each_host_timed_warning_has_an_example():
    assert all(h.examples for h in HOST_TIMED)


@pytest.mark.parametrize(("example", "h"), EXAMPLES, ids=[e[:60] for e, _ in EXAMPLES])
def test_each_example_is_left_out_and_matches_its_own_pattern_only(example, h):
    stamped = f"[    1.204210] {example}"
    assert is_host_timed(stamped) and unexpected_warnings(stamped + "\n") == []
    assert [g for g in HOST_TIMED if re.match(g.pattern, without_stamp(stamped))] == [h]
    # And it is the kernel's text, its placeholders filled in.
    words = re.split(r"\{[^}]*\}", h.text)
    assert re.search(".+".join(map(re.escape, words)), example), (h.text, example)


def test_other_warnings_and_errors_are_unexpected():
    out = (
        "[    0.100000] INFO  boot: boot complete\n"
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
        "[    0.300000] ERROR mouse: 0000:00:03.0: the mouse thread (a bug): not held\n"
        "[    0.400000] WARN  irq: no timer interrupt in 100 ms (the CPU's reads at most 2000 us apart)\n"
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


def test_a_first_tick_missing_with_the_cpu_running_throughout_is_unexpected_under_kvm(monkeypatch):
    # Under KVM, reads under a tick period apart all along: the timer didn't
    # tick, no host stalled the CPU past it. Under TCG the host can starve
    # QEMU's timer thread meanwhile: the host's either way.
    line = "[    0.400000] WARN  irq: no timer interrupt in 100 ms (the CPU's reads at most 999 us apart)"
    monkeypatch.setenv("LUMEN_ACCEL", "kvm")
    assert unexpected_warnings(line + "\n") == [line]
    assert is_host_timed(line.replace("999 us", "1000 us"))
    monkeypatch.setenv("LUMEN_ACCEL", "tcg")
    assert is_host_timed(line)


def test_a_broken_pit_is_unexpected():
    # The PIT measured again after an imprecise HPET calibration failed: a
    # broken PIT (time.rs's the_steadier), not the host's.
    line = (
        "[    1.000000] WARN  time: tsc calibration: against the HPET within 1.4% (past the 1.0% allowed), "
        "against the PIT it failed (it didn't count through a window): kept the HPET"
    )
    assert unexpected_warnings(line + "\n") == [line]


def test_a_warning_after_part_of_a_line_is_found():
    out = "lumen> ech[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
    assert unexpected_warnings(out) == [
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)"
    ]


def test_a_host_timed_text_with_more_after_it_is_still_unexpected_where_anchored():
    # The irq warning is matched whole: a longer line is something else.
    out = "[    0.400000] WARN  irq: no timer interrupt in 100 ms (the CPU's reads at most 2000 us apart) on cpu 3\n"
    assert unexpected_warnings(out) == [out.rstrip("\n")]


def test_the_underrun_count_is_read_from_the_warning():
    m = UNDERRUNS.search(
        "[    9.330000] WARN  sound: a sound found the queue empty 3 times (underruns)\n"
    )
    assert m and m["n"] == "3"
