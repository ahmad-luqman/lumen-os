"""A `linux` run's leaked frames (`linux: <name> exit=N leaked_frames=K`,
`run_exec`'s diff of the free frames), kept exact around a kernel leak.

The leak is #300: each process's kernel stack is `STACK_SIZE` plus a
`GUARD_SIZE` guard page at addresses never reused, and `unmap` never frees
an emptied page table, so every `STACK_TABLE_EVERY`th or next process maps a
new L1 table (2 MiB of the stack window) that outlives it, and that run
counts one frame no file explains. Which run depends on how many threads the
boot made. `start_stack_table` runs `busybox true` until one maps that
table; the next `STACK_TABLE_EVERY - 1` processes then map none, so their
counts are exact. `CleanRuns` runs a machine's `linux` commands that way,
starting a new table once `CLEAN_RUNS` runs have followed the last one, and
checks each table comes where the stack layout puts it. Once #300 is fixed
`start_stack_table` finds no table and fails: remove this module with that
fix."""

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
# The runs a table leaves clean: the next table is `STACK_TABLE_EVERY` or
# one more stacks after it.
CLEAN_RUNS = STACK_TABLE_EVERY - 1

# A `linux /bin/busybox ...` run's last line.
BUSYBOX_EXIT = re.compile(r"linux: /bin/busybox exit=(\d+) leaked_frames=(-?\d+)")


def start_stack_table(m) -> int:
    """Run `linux /bin/busybox true` (a rootfs's) until a run maps a new
    stack-window page table (`leaked_frames=1`, #300), at most
    `STACK_TABLE_EVERY + 2` runs, every other run leaking 0. How many runs
    it took."""
    for runs in range(1, STACK_TABLE_EVERY + 3):
        out = run(m, "linux /bin/busybox true", 60)
        lines = out.splitlines()
        done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
        assert done and done.group(1) == "0" and done.group(2) in ("0", "1"), out
        if done.group(2) == "1":
            return runs
    raise AssertionError(
        f"no stack-window page table in {STACK_TABLE_EVERY + 2} runs (#300 fixed?)"
    )


class CleanRuns:
    """One machine's `linux` runs, each one's leaked frames exact. It holds
    if every kernel stack the machine makes from the first run on is one of
    these runs (through `run`, or `before_run` just before one), and nothing
    else on the machine spawns a thread between them; `finish` after the
    last checks that held."""

    def __init__(self, m) -> None:
        self.m = m
        # Runs since the last table; None before the first one, whose place
        # nothing predicts.
        self.since: int | None = None

    def _table(self) -> None:
        """Start a table, at the place the last one puts it if there was one."""
        took = start_stack_table(self.m)
        if self.since is not None:
            at = self.since + took
            assert at in (STACK_TABLE_EVERY, STACK_TABLE_EVERY + 1), (
                f"a stack-window table {at} processes after the last one, not "
                f"{STACK_TABLE_EVERY} or {STACK_TABLE_EVERY + 1}: a run leaked a frame "
                "or another thread was spawned"
            )
        self.since = 0

    def before_run(self) -> None:
        """Call just before a `linux` run not made through `run`."""
        if self.since is None or self.since >= CLEAN_RUNS:
            self._table()
        self.since += 1

    def run(self, line: str, timeout: float = 60, **kwargs):
        """`shell.run(m, line, timeout, **kwargs)` for a `linux` command."""
        self.before_run()
        return run(self.m, line, timeout, **kwargs)

    def finish(self) -> None:
        """After the machine's last run: the next table comes where the last
        one puts it, so the runs between made one stack each and took no
        frame as a table (as the first table's run might have)."""
        assert self.since is not None, "no run"
        self._table()
