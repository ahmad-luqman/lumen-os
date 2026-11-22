"""lumen_harness.leaks: its copies of the kernel's stack sizes agree with the
Rust sources, `start_stack_table` stops at the run that maps #300's page
table, and `CleanRuns` starts a table before the first run and again once
`CLEAN_RUNS` runs have followed one, each new table (and `finish`'s) where
the last one puts it."""

import re

import pytest
from lumen_harness import REPO_ROOT, leaks
from lumen_harness.leaks import CLEAN_RUNS, STACK_TABLE_EVERY, CleanRuns, start_stack_table
from qemu_fakes import FakeMachine


def rust_const(path: str, name: str, names: dict[str, int] | None = None) -> int:
    """`pub const <name>: u64 = <a> [* <b>]` in `path`, as a number (an
    operand a literal or one of `names`)."""
    text = (REPO_ROOT / path).read_text()
    found = re.search(rf"pub const {name}: u64 = ([^;]+);", text)
    assert found, f"no {name} in {path}"
    value = 1
    for part in found.group(1).split("*"):
        part = part.strip()
        value *= (names or {})[part] if part in (names or {}) else int(part.replace("_", ""))
    return value


def test_sizes_match_the_kernels():
    frame = rust_const("crates/lumen-mm/src/lib.rs", "FRAME_SIZE")
    assert leaks.STACK_SIZE == rust_const("kernel/src/mm/stack.rs", "STACK_SIZE")
    assert leaks.GUARD_SIZE == rust_const(
        "crates/lumen-mm/src/stacks.rs", "GUARD_SIZE", {"FRAME_SIZE": frame}
    )
    # Seen in a boot: one frame more at the 20th and the 50th of 70 runs.
    assert STACK_TABLE_EVERY == 30
    # The runs after a table, short of the next (`STACK_TABLE_EVERY` or one
    # more stacks after it).
    assert CLEAN_RUNS < STACK_TABLE_EVERY


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
    # A boot's first table may need STACK_TABLE_EVERY + 1 runs.
    assert (
        start_stack_table(FakeMachine(runs(*[0] * STACK_TABLE_EVERY, 1))) == STACK_TABLE_EVERY + 1
    )


def test_start_stack_table_refuses_other_counts_and_no_table():
    with pytest.raises(AssertionError):
        start_stack_table(FakeMachine(runs(0, 2, 1)))
    with pytest.raises(AssertionError):
        start_stack_table(FakeMachine(runs(0, -1, 1)))
    with pytest.raises(AssertionError, match="no stack-window page table"):
        start_stack_table(FakeMachine(runs(*[0] * (STACK_TABLE_EVERY + 2))))


def counted(clean: CleanRuns, n: int) -> None:
    for _ in range(n):
        clean.before_run()


def test_clean_runs_starts_a_table_first_and_after_clean_runs():
    # The first table anywhere; the next on the re-anchor's first run (30
    # after it) and the one after on its second (31 after).
    m = FakeMachine(runs(0, 1, 1, 0, 1))
    clean = CleanRuns(m)
    clean.before_run()  # a table first (two runs)
    assert len(m.sent) == 2
    counted(clean, CLEAN_RUNS - 1)
    assert len(m.sent) == 2, "no table needed within CLEAN_RUNS runs"
    clean.before_run()  # a new table, on its first run: 30 after the last
    assert len(m.sent) == 3
    counted(clean, CLEAN_RUNS)  # the last: a new table on its second run
    assert len(m.sent) == 5


def test_clean_runs_refuses_a_table_out_of_place():
    # After CLEAN_RUNS runs, a table can't come 2 runs later (32 after).
    clean = CleanRuns(FakeMachine(runs(1, 0, 0, 1)))
    counted(clean, CLEAN_RUNS)
    with pytest.raises(AssertionError, match="32 processes after the last one"):
        clean.before_run()
    # Nor sooner than STACK_TABLE_EVERY: a frame leaked by a run looks like
    # a table early.
    clean = CleanRuns(FakeMachine(runs(1, 1)))
    counted(clean, 5)
    with pytest.raises(AssertionError, match="6 processes after the last one"):
        clean.finish()


def test_clean_runs_finish_checks_the_next_table():
    clean = CleanRuns(FakeMachine(runs(1, *[0] * 26, 1)))
    counted(clean, 4)  # 4 runs after the table, then the 27th filler: 31
    clean.finish()
    with pytest.raises(AssertionError, match="no run"):
        CleanRuns(FakeMachine("")).finish()


def test_clean_runs_run_counts_its_command():
    m = FakeMachine(
        runs(1) + "linux /bin/busybox ls\nlinux: /bin/busybox exit=0 leaked_frames=0\nlumen> "
    )
    clean = CleanRuns(m)
    assert clean.run("linux /bin/busybox ls") == "linux: /bin/busybox exit=0 leaked_frames=0\n"
    assert clean.since == 1 and m.sent == ["linux /bin/busybox true", "linux /bin/busybox ls"]
