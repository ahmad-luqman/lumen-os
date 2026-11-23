"""lumen_harness.klog's host-timed warnings and the "no warning" check."""

import re
from pathlib import Path

import pytest
from lumen_harness.klog import (
    HELD_BACK,
    HOST_TIMED,
    UNDERRUNS,
    is_host_timed,
    unexpected_warnings,
    without_stamp,
)

ROOT = Path(__file__).resolve().parents[2]
EXAMPLES = [(e, h) for h in HOST_TIMED for e in h.examples]
NEAR_MISSES = [(e, h) for h in HOST_TIMED for e in h.near_misses]


@pytest.mark.parametrize("h", HOST_TIMED, ids=lambda h: h.text)
def test_each_host_timed_text_is_in_the_kernel_once_with_an_example(h):
    assert (ROOT / h.source).read_text().count(h.text) == 1, (h.source, h.text)
    assert h.examples and h.near_misses, h.text


@pytest.mark.parametrize(("example", "h"), EXAMPLES, ids=[e[:60] for e, _ in EXAMPLES])
def test_each_example_is_left_out_and_matches_its_own_pattern_only(example, h):
    stamped = f"[    1.204210] {example}"
    assert (
        is_host_timed(stamped, accel="tcg")
        and unexpected_warnings(stamped + "\n", accel="tcg") == []
    )
    assert [g for g in HOST_TIMED if re.match(g.pattern, without_stamp(stamped))] == [h]
    # And it is the kernel's text, its placeholders filled in.
    words = re.split(r"\{[^}]*\}", h.text)
    assert re.search(".+".join(map(re.escape, words)), example), (h.text, example)


@pytest.mark.parametrize(("near", "h"), NEAR_MISSES, ids=[e[:60] for e, _ in NEAR_MISSES])
def test_each_near_miss_is_unexpected(near, h):
    # A warning beside the host-timed one (its file's, or its subsystem's)
    # stays unexpected: the pattern isn't the whole subsystem.
    assert not is_host_timed(f"[    1.204210] {near}", accel="tcg"), (h.text, near)


def test_other_warnings_and_errors_are_unexpected():
    out = (
        "[    0.100000] INFO  boot: boot complete\n"
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
        "[    0.300000] ERROR mouse: 0000:00:03.0: the mouse thread (a bug): not held\n"
        "[    0.400000] WARN  sound: a sound found the queue empty 2 times (underruns)\n"
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f\n"
    )
    assert unexpected_warnings(out, accel="kvm") == [
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)",
        "[    0.300000] ERROR mouse: 0000:00:03.0: the mouse thread (a bug): not held",
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f",
    ]
    assert unexpected_warnings(out, "irq", accel="kvm") == [
        "[    0.500000] WARN  irq: unexpected interrupt on vector 0x8f"
    ]
    assert unexpected_warnings(out, "time", accel="kvm") == []


def test_a_first_tick_missing_is_the_hosts_only_under_tcg():
    # Under KVM a tick due while the vCPU is descheduled is delivered as it
    # resumes: no stall makes the first one missing for 100 ms. Under TCG
    # the host can starve QEMU's timer thread meanwhile. Any accelerator
    # but TCG counts as KVM (fails closed).
    line = "[    0.400000] WARN  irq: no timer interrupt in 100 ms"
    for accel in ("kvm", "kvm,kernel-irqchip=split", "hvf"):
        assert unexpected_warnings(line + "\n", accel=accel) == [line], accel
    assert is_host_timed(line, accel="tcg")
    # And a longer line is something else, even under TCG.
    assert not is_host_timed(line + " on cpu 3", accel="tcg")


def test_a_broken_pit_is_unexpected():
    # The PIT measured again after an imprecise HPET calibration failed: a
    # broken PIT (time.rs's the_steadier), not the host's.
    line = (
        "[    1.000000] WARN  time: tsc calibration: against the HPET within 1.4% (past the 1.0% allowed), "
        "against the PIT it failed (it didn't count through a window): kept the HPET"
    )
    assert unexpected_warnings(line + "\n", accel="tcg") == [line]


def test_a_warning_after_part_of_a_line_is_found():
    out = "lumen> ech[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)\n"
    assert unexpected_warnings(out) == [
        "[    0.200000] WARN  boot: timeline: kmain 0.1 ms (incomplete)"
    ]


def test_the_underrun_count_is_read_from_the_warning():
    m = UNDERRUNS.search(
        "[    9.330000] WARN  sound: a sound found the queue empty 3 times (underruns)\n"
    )
    assert m and m["n"] == "3"


def test_a_held_back_summary_is_read_and_its_first_line_is_no_log_line():
    # kernel/src/klog.rs's summary of a call site's held lines (#347).
    assert (ROOT / "kernel/src/klog.rs").read_text().count(
        '"{count} more held back like: {first}"'
    ) == 1
    line = (
        "[   11.241244] INFO  klog: 1294 more held back like: ERROR vfs: ext4 DevId(7) inode 13: "
        "links_count on disk is 2, keeping 1 in memory until a write lands\n"
    )
    m = HELD_BACK.fullmatch(line)
    assert m and m["n"] == "1294"
    assert m["first"].startswith("ERROR vfs: ext4 DevId(7) inode 13: ")
    # The copy of the first line inside it isn't a warning of its own.
    assert unexpected_warnings(line) == []
    # Found wherever its stamped line starts, as other log lines are.
    assert HELD_BACK.search(
        "ok 9 - x\nlumen> [    1.000000] INFO  klog: 2 more held back like: WARN  a: b\n"
    )
