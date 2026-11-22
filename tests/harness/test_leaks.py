"""lumen_harness.leaks: a `linux` run's leaked frames against what its files
explain, one stack-window page table now and then allowed."""

import pytest
from lumen_harness.leaks import STACK_TABLE_EVERY, Leaks


def line(n: int) -> str:
    return f"linux: /bin/busybox exit=0 leaked_frames={n}"


def test_stack_table_every_matches_the_kernel_stack_layout():
    # 2 MiB / (64 KiB + 4 KiB guard): measured as one extra frame at the
    # 20th and the 50th of 70 runs of `busybox true`.
    assert STACK_TABLE_EVERY == 30


def test_exact_counts_pass_and_others_fail():
    leaks = Leaks()
    leaks.check(line(0), 0)
    leaks.check(line(2), 2)
    leaks.check(line(-1), -1)
    with pytest.raises(AssertionError, match="expected leaked_frames=0"):
        leaks.check(line(2), 0)
    with pytest.raises(AssertionError, match="expected leaked_frames=1"):
        leaks.check(line(0), 1)
    with pytest.raises(AssertionError, match="no leaked_frames"):
        leaks.check("linux: hello exit=0", 0)


def test_one_extra_frame_allowed_once_in_half_a_table():
    leaks = Leaks()
    leaks.check(line(1), 0)  # a table on the first run
    for _ in range(STACK_TABLE_EVERY // 2 - 1):
        leaks.check(line(0), 0)
    leaks.check(line(2), 1)  # half a table's runs later: allowed
    with pytest.raises(AssertionError, match="1 runs after the last one"):
        leaks.check(line(1), 0)


def test_back_to_back_extra_frames_fail():
    # A path leaking a frame each time it runs, run twice.
    leaks = Leaks()
    leaks.check(line(1), 0)
    with pytest.raises(AssertionError, match="runs after the last one"):
        leaks.check(line(1), 0)
