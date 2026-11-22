"""A `linux` run's leaked frames (`linux: <name> exit=N leaked_frames=K`,
`run_exec`'s diff of the free frames), kept exact around a kernel leak.

The leak is #300: each process's kernel stack is `STACK_SIZE` plus a
`GUARD_SIZE` guard page at addresses never reused, and `unmap` never frees
an emptied page table, so every `STACK_TABLE_EVERY`th or so process maps a
new L1 table (2 MiB of the stack window) that outlives it, and that run
counts one frame no file explains. Which run depends on how many threads the
boot made. `start_stack_table` runs `busybox true` until one maps that
table; the next `STACK_TABLE_EVERY - 1` processes then map none, so their
counts are exact. `CleanRuns` does that before a test's first run and again
before every `CLEAN_RUNS`th. Once #300 is fixed `start_stack_table` finds no
table and fails: remove this module with that fix."""

import re

from lumen_harness.shell import run

# Copies of the kernel's sizes, checked against the Rust sources by
# tests/harness/test_leaks.py: `STACK_SIZE` (kernel/src/mm/stack.rs, a
# thread's stack) and `GUARD_SIZE` (crates/lumen-mm/src/stacks.rs).
STACK_SIZE = 64 * 1024
GUARD_SIZE = 4096
# The span one L1 page table maps.
L1_SPAN = 2 << 20
# Tables come this many stacks apart, or one more (2 MiB / 68 KiB = 30.1).
STACK_TABLE_EVERY = L1_SPAN // (STACK_SIZE + GUARD_SIZE)
# The runs `CleanRuns` allows after a table, short of the next one.
CLEAN_RUNS = STACK_TABLE_EVERY - 2

TRUE_LINE = re.compile(r"linux: /bin/busybox exit=0 leaked_frames=(-?\d+)")


def start_stack_table(m) -> int:
    """Run `linux /bin/busybox true` (a rootfs's) until a run maps a new
    stack-window page table (`leaked_frames=1`, #300), at most
    `STACK_TABLE_EVERY + 2` runs, every other run leaking 0. How many runs
    it took."""
    for runs in range(1, STACK_TABLE_EVERY + 3):
        out = run(m, "linux /bin/busybox true", 60).replace("\r\n", "\n")
        done = TRUE_LINE.fullmatch(out.rstrip("\n").rpartition("\n")[2])
        assert done and done.group(1) in ("0", "1"), out
        if done.group(1) == "1":
            return runs
    raise AssertionError(
        f"no stack-window page table in {STACK_TABLE_EVERY + 2} runs (#300 fixed?)"
    )


class CleanRuns:
    """One machine's `linux` runs kept clear of #300's page tables: call
    `before_run` before each, and every one's leaked frames are exact."""

    def __init__(self, m) -> None:
        self.m = m
        self.since = None  # runs since the last table; None: none yet

    def before_run(self) -> None:
        if self.since is None or self.since >= CLEAN_RUNS:
            start_stack_table(self.m)
            self.since = 0
        self.since += 1
