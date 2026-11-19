"""X-ray's kernel side (#198): run-queue changes, device interrupts and
locks that waited in their own trace rings, one per CPU, read through the
serial shell's `trace runq` and `trace xray`.

The rings' logic is tested on the host (`cargo test -p lumen-trace`:
`read_since`'s accounting) and in-kernel (`lumen_kernel::trace::*`: the
queue's changes in order, a contended lock, the main rings left alone
under X-ray's load). These check what the scheduler and the interrupt
counters say against what the rings say: counts only, never a time
(CLAUDE.md, "Tests on a busy host").
"""

import re

from lumen_harness.shell import PROMPT, run
from test_serial import com1_interrupts

BOOT_TIMEOUT = 60
PARKED = 3

QUEUE = r"cpu (\d+): trace (\d+|-), scheduler (\d+) \((?:event #\d+|no runq event in its ring)\)"
XRAY_SUMMARY = re.compile(
    r"^xray: cpu (\d+), (\d+) copied, (\d+) missed, next (\d+) \(claimed (\d+), dropped (\d+)\)"
    r"(?P<stopped>; stopped at #\d+ \(.+\))?$",
    re.MULTILINE,
)
# COM1's input: ISA IRQ 4, GSI 4, vector 0x54 on q35 (test_serial).
COM1_VECTOR = "0x54"


def shell(boot, *extra: str):
    m = boot("qemutest", extra=list(extra))
    m.expect(PROMPT, BOOT_TIMEOUT)
    return m


def ids(line: str) -> list[int]:
    """The thread ids after a label (`none`: no ids)."""
    words = line.split()
    return [] if words == ["none"] else [int(w) for w in words]


def test_run_queue_matches_trace(boot):
    # The shell's CPU (0) holds still (its interrupts off: no tick, so
    # nothing there is taken off its queue) while 3 threads are spawned
    # onto it: they wait on its run queue. The X-ray ring's newest
    # run-queue event for CPU 0 says the length the scheduler does, which
    # grew by exactly the threads the ring saw queued between the two
    # readings (the parked ones among them; nothing taken off, nothing
    # missed), the CPU took no tick meanwhile (the hold held), and
    # `threads` lists the parked ones as ready there.
    m = shell(boot, "-smp", "2")
    out = run(m, f"trace runq park {PARKED}", timeout=30)
    assert out.endswith("runq park: released\n"), out
    held = re.search(r"^runq park: cpu (\d+) held, \d+ threads parked:(.*)$", out, re.MULTILINE)
    assert held and held[1] == "0", out
    parked = ids(held[2])
    assert len(parked) == PARKED, out
    before = re.search(rf"^  before: {QUEUE}$", out, re.MULTILINE)
    after = re.search(rf"^  after: {QUEUE}$", out, re.MULTILINE)
    assert before and after and before[1] == after[1] == "0", out
    assert after[2] == after[3], out  # the trace's length is the scheduler's
    ticks = re.search(r"^  ticks on cpu 0: before (\d+), after (\d+)$", out, re.MULTILINE)
    assert ticks and ticks[1] == ticks[2], out
    ready = ids(re.search(r"^  ready on cpu 0 \(threads\):(.*)$", out, re.MULTILINE)[1])
    enqueued = ids(re.search(r"^  traced meanwhile: enqueued(.*)$", out, re.MULTILINE)[1])
    dequeued = ids(re.search(r"^  traced meanwhile: dequeued(.*)$", out, re.MULTILINE)[1])
    missed = int(re.search(r"^  traced meanwhile: missed (\d+)$", out, re.MULTILINE)[1])
    assert (dequeued, missed) == ([], 0), out
    assert set(parked) <= set(enqueued), out
    assert set(parked) <= set(ready), out
    assert int(after[3]) == int(before[3]) + len(enqueued), out

    # Unheld, each CPU's queue still reads the same both ways (each line is
    # read under that queue's lock).
    out = run(m, "trace runq")
    queues = re.findall(rf"^  {QUEUE}$", out, re.MULTILINE)
    assert [cpu for cpu, *_ in queues] == ["0", "1"], out
    for _, traced, scheduler in queues:
        assert traced == scheduler or (traced, scheduler) == ("-", "0"), out
    assert "MISMATCH" not in out, out


def xray(m, since: int | None = None) -> tuple[str, re.Match[str]]:
    """`trace xray 0 [since]`: its event lines and its summary."""
    out = run(m, "trace xray 0" if since is None else f"trace xray 0 {since}")
    summary = XRAY_SUMMARY.search(out)
    assert summary, out
    return out[: summary.start()], summary


def test_device_interrupts_are_traced(boot):
    # One CPU, so COM1's interrupts are all handled there. Every one `irqs`
    # counts between two of its runs is also in the X-ray ring, read from
    # before the first to after the second (a wider window): at least as
    # many `irq` events on COM1's vector, and none missed.
    m = shell(boot)
    _, start = xray(m)
    first = com1_interrupts(m)
    second = com1_interrupts(m)
    assert second > first, (first, second)  # typing `irqs` interrupts
    events, summary = xray(m, int(start[4]))
    assert summary[3] == "0" and summary[6] == "0", summary[0]
    assert summary["stopped"] is None, summary[0]  # read to the head
    irqs = re.findall(
        rf"^#\d+ +tsc=\d+ +irq +vector={COM1_VECTOR} rip=0x[0-9a-f]+$", events, re.MULTILINE
    )
    assert len(irqs) >= second - first, (len(irqs), first, second, events)

    # None of it is in the main ring, which the boot's events and the crash
    # report read: every kind there is one of the old ones.
    out = run(m, "trace 4096")
    kinds = set(re.findall(r"^#\d+ +tsc=\d+ +(?:cpu=\d+ pid=\d+ +)?(\S+)", out, re.MULTILINE))
    assert kinds and not kinds & {"runq", "irq", "lock"}, kinds

    # A CPU that isn't online has no ring to read.
    out = run(m, "trace xray 1")
    assert out == "trace xray: cpu 1: no such cpu online\n", out
