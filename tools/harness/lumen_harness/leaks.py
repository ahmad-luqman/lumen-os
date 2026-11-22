"""The frames a `linux` run counts as leaked (`linux: <name> exit=N
leaked_frames=K`, from `run_exec`'s diff of the free frames).

Besides the page-cache pages of the files a run creates or frees, a run may
count one page table that no file explains. Each process's kernel stack is
`mm::stack::STACK_SIZE` plus a guard page, at addresses never reused, so
about every `STACK_TABLE_EVERY`th process maps a new L1 table (2 MiB of
the stack window) that outlives it. Which run that lands on depends on how
many threads the boot made. A test with fewer runs than that first runs
`start_stack_table`, after which its counts are exact; one with more uses
`Leaks`, which allows the extra frame on any run, but only once in
`STACK_TABLE_EVERY // 2` runs: a frame leaked by the same code path twice in
a row still fails."""

import re

from lumen_harness.shell import run

# 2 MiB of the stack window per L1 table over a 64 KiB stack and its 4 KiB
# guard page. Change it with `STACK_SIZE` in kernel/src/mm/stack.rs.
STACK_TABLE_EVERY = (2 << 20) // (64 * 1024 + 4096)

LEAKED = re.compile(r"leaked_frames=(-?\d+)$")


class Leaks:
    """One machine's `linux` runs, checked in order: each run's count must
    be what its files explain, or one more (a stack-window page table, no
    sooner than `STACK_TABLE_EVERY // 2` runs after the last one)."""

    def __init__(self) -> None:
        self.runs = 0
        self.last_extra: int | None = None

    def check(self, line: str, expected: int) -> None:
        """Check the `linux:` exit line `line` against `expected` frames."""
        found = LEAKED.search(line)
        assert found, f"no leaked_frames in {line!r}"
        extra = int(found.group(1)) - expected
        assert extra in (0, 1), f"{line!r}: expected leaked_frames={expected}"
        if extra:
            spacing = STACK_TABLE_EVERY // 2
            assert self.last_extra is None or self.runs - self.last_extra >= spacing, (
                f"{line!r}: a frame more than expected {self.runs - self.last_extra} runs "
                f"after the last one (a stack-window page table comes every {STACK_TABLE_EVERY})"
            )
            self.last_extra = self.runs
        self.runs += 1


def start_stack_table(m) -> int:
    """Run `linux /bin/busybox true` (a rootfs's) until a run maps a new
    stack-window page table (`leaked_frames=1`), at most
    `STACK_TABLE_EVERY + 2` runs: the next `STACK_TABLE_EVERY - 1` processes
    then map none, and their counts are exact. How many runs it took."""
    for runs in range(1, STACK_TABLE_EVERY + 3):
        out = run(m, "linux /bin/busybox true", 60).replace("\r\n", "\n")
        done = out.splitlines()[-1] if out else ""
        found = re.fullmatch(r"linux: /bin/busybox exit=0 leaked_frames=(-?\d+)", done)
        assert found and found.group(1) in ("0", "1"), out
        if found.group(1) == "1":
            return runs
    raise AssertionError(f"no stack-window page table in {STACK_TABLE_EVERY + 2} runs")
