"""X-ray's kernel side (#198): run-queue changes, device interrupts and
contended locks in their own trace rings, one per CPU, read through the
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

BOOT_TIMEOUT = 60
PARKED = 3

QUEUE = r"cpu (\d+): trace (\d+|-), scheduler (\d+) \((?:event #\d+|no runq event in its ring)\)"
XRAY_SUMMARY = re.compile(
    r"^xray: cpu (\d+), (\d+) copied, (\d+) missed, next (\d+) \(claimed (\d+), dropped (\d+)\)$",
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
    # CPU 1 holds still (interrupts off in a thread there) while 3 threads
    # are spawned onto it: they wait on its run queue. The X-ray ring's
    # newest run-queue event for CPU 1 says the length the scheduler does,
    # which grew by exactly the threads the ring saw queued since the hold
    # began (the parked ones among them; nothing taken off, nothing
    # missed), and `threads` lists the parked ones as ready there.
    m = shell(boot, "-smp", "2")
    out = run(m, f"trace runq park 1 {PARKED}", timeout=30)
    assert "runq park: released (held throughout)" in out, out
    parked = ids(
        re.search(r"^runq park: cpu 1 held, \d+ threads parked:(.*)$", out, re.MULTILINE)[1]
    )
    assert len(parked) == PARKED, out
    before = re.search(rf"^  before: {QUEUE}$", out, re.MULTILINE)
    after = re.search(rf"^  after: {QUEUE}$", out, re.MULTILINE)
    assert before and after, out
    assert after[2] == after[3], out  # the trace's length is the scheduler's
    ready = ids(re.search(r"^  ready on cpu 1 \(threads\):(.*)$", out, re.MULTILINE)[1])
    enqueued = ids(re.search(r"^  traced since the hold: enqueued(.*)$", out, re.MULTILINE)[1])
    dequeued = ids(re.search(r"^  traced since the hold: dequeued(.*)$", out, re.MULTILINE)[1])
    missed = int(re.search(r"^  traced since the hold: missed (\d+)$", out, re.MULTILINE)[1])
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


def com1_interrupts(m) -> int:
    """`irqs`' count for COM1's input (as test_serial reads it)."""
    out = run(m, "irqs")
    row = re.search(
        r"^\s+4\s+0x54\s+Fixed\s+edge\s+high\s+0\s+unmasked\s+(\d+)$", out, re.MULTILINE
    )
    assert row, out
    return int(row[1])


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
    irqs = re.findall(
        rf"^#\d+ +tsc=\d+ +irq +vector={COM1_VECTOR} rip=0x[0-9a-f]+$", events, re.MULTILINE
    )
    assert len(irqs) >= second - first, (len(irqs), first, second, events)
