"""lumen_harness.leaks: its copies of the kernel's stack sizes agree with the
Rust sources, `start_stack_table` stops at the run that maps #300's page
table, and `CleanRuns` starts a table before the first run and again
before the run that could map the next one."""

import re

import pytest
from lumen_harness import REPO_ROOT, leaks
from lumen_harness.leaks import CLEAN_RUNS, STACK_TABLE_EVERY, CleanRuns, start_stack_table
from qemu_fakes import FakeMachine


def rust_const(path: str, name: str) -> int:
    """`pub const <name>: u64 = <a> [* <b>]` in `path`, as a number (an
    operand may be `FRAME_SIZE`, 4096)."""
    text = (REPO_ROOT / path).read_text()
    found = re.search(rf"pub const {name}: u64 = ([^;]+);", text)
    assert found, f"no {name} in {path}"
    value = 1
    for part in found.group(1).split("*"):
        part = part.strip()
        value *= 4096 if part == "FRAME_SIZE" else int(part.replace("_", ""))
    return value


def test_sizes_match_the_kernels():
    assert leaks.STACK_SIZE == rust_const("kernel/src/mm/stack.rs", "STACK_SIZE")
    assert leaks.GUARD_SIZE == rust_const("crates/lumen-mm/src/stacks.rs", "GUARD_SIZE")
    assert re.search(
        r"pub const FRAME_SIZE: u64 = 4096;", (REPO_ROOT / "crates/lumen-mm/src/lib.rs").read_text()
    )
    # Seen in a boot: one frame more at the 20th and the 50th of 70 runs.
    assert STACK_TABLE_EVERY == 30


def runs(*leaked: int) -> str:
    """Serial output of `linux /bin/busybox true` runs leaking `leaked`."""
    line = "linux /bin/busybox true"
    return "lumen> " + "".join(
        f"{line}\nlinux: /bin/busybox exit=0 leaked_frames={n}\nlumen> " for n in leaked
    )


def test_start_stack_table_stops_at_the_table():
    m = FakeMachine(runs(0, 0, 1, 0))
    assert start_stack_table(m) == 3
    assert m.sent == ["linux /bin/busybox true"] * 3


def test_start_stack_table_refuses_other_counts_and_no_table():
    with pytest.raises(AssertionError):
        start_stack_table(FakeMachine(runs(0, 2, 1)))
    with pytest.raises(AssertionError):
        start_stack_table(FakeMachine(runs(0, -1, 1)))
    with pytest.raises(AssertionError, match="no stack-window page table"):
        start_stack_table(FakeMachine(runs(*[0] * (STACK_TABLE_EVERY + 2))))


def test_clean_runs_starts_a_table_first_and_every_clean_runs():
    m = FakeMachine(runs(1, 0, 1, 1))
    clean = CleanRuns(m)
    clean.before_run()  # the first run: a table first (one run)
    assert len(m.sent) == 1
    for _ in range(CLEAN_RUNS - 1):
        clean.before_run()
    assert len(m.sent) == 1, "no table needed within CLEAN_RUNS runs"
    clean.before_run()  # the run after CLEAN_RUNS: a new table (two runs)
    assert len(m.sent) == 3
    for _ in range(CLEAN_RUNS - 1):
        clean.before_run()
    clean.before_run()
    assert len(m.sent) == 4
    # Short of the next table either way (30 or 31 stacks apart), with
    # the table's own run counted.
    assert CLEAN_RUNS + 1 < STACK_TABLE_EVERY
