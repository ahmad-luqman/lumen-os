"""X-ray (#198, #208): run-queue changes, device interrupts and locks that
waited in their own trace rings, one per CPU, read through the serial
shell's `trace runq` and `trace xray`; and the X-ray app (`apps/xray`),
which reads them through the `Trace` capability.

The rings' logic is tested on the host (`cargo test -p lumen-trace`:
`read_since`'s accounting) and in-kernel (`lumen_kernel::trace::*`: the
queue's changes in order, a contended lock, the main rings left alone
under X-ray's load). These check what the scheduler and the interrupt
counters say against what the rings say: counts only, never a time
(CLAUDE.md, "Tests on a busy host").
"""

import re

from lumen_harness import gui
from lumen_harness.gui import parse_records
from lumen_harness.image import assert_matches_golden
from lumen_harness.shell import PROMPT, run, wait_for_output
from test_apps import app, below_root, end_line, start, wait_gone, wait_tree
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
    # before the first to after the last (a wider window): at least as
    # many `irq` events on COM1's vector, and none missed. Typing a command
    # usually raises IRQ 4, but #220 saw input reach the shell without one
    # (read straight from the UART: at the reader's safety-net deadline, or
    # before it blocks): `irqs` is run until its count has risen, an
    # interrupt confirmed before comparing.
    m = shell(boot)
    _, start = xray(m)
    first = com1_interrupts(m)
    second = gui.wait_for(
        m, "a COM1 interrupt", lambda: com1_interrupts(m), lambda n: n > first, 30
    )
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


# -- the X-ray app (#208) -----------------------------------------------------

# `trace runq spin`'s threads: one runs on their CPU, the rest wait on its
# run queue.
SPIN = 3
SPINNING = re.compile(r"runq spin: (\d+) threads spinning on cpu (\d+):((?: \d+)+)\n")


def desktop(boot, *extra: str):
    m = shell(boot, *extra)
    gui.start_desktop(m)
    return m


def label(rows: list[dict[str, str]], name: str) -> str:
    return gui.node(rows, "label", name)["value"]


def scheduler_queue(m, cpu: int) -> str:
    """CPU `cpu`'s run-queue length as the scheduler says it (`trace
    runq`, read under that queue's lock)."""
    out = run(m, "trace runq")
    queues = {int(c): sched for c, _, sched in re.findall(rf"^  {QUEUE}$", out, re.MULTILINE)}
    assert cpu in queues, out
    return queues[cpu]


def spin(m, n: int) -> tuple[int, list[str]]:
    """`trace runq spin <n>`: their CPU and their thread ids."""
    out = run(m, f"trace runq spin {n}")
    found = SPINNING.fullmatch(out)
    assert found and int(found[1]) == n, out
    return int(found[2]), found[3].split()


def stop_spinning(m, n: int, cpu: int) -> None:
    out = run(m, "trace runq spin stop")
    assert out == f"runq spin: {n} threads stopped on cpu {cpu}\n", out


# The app's lanes (apps/xray's layout, client coordinates): where each
# CPU's lane starts, its columns, and a run-queue bar's pixels a thread.
LANES_X = 424 + 4
COL_W = 4
COLUMNS = 50
LANE_Y0 = 10
LANE_H = 56
LANE_BASE = 48  # a bar's bottom row is LANE_BASE - 1 below the lane's top
RUNQ_UNIT = 3


def bar_of(img, cpu: int, length: int) -> int | None:
    """The first column in CPU `cpu`'s lane whose run-queue bar is exactly
    `length` threads tall (one colour, not the background, over those rows,
    the background above them), or None."""
    top = LANE_Y0 + cpu * LANE_H
    bottom = top + LANE_BASE
    h = length * RUNQ_UNIT
    background = img.getpixel((LANES_X - 2, top + 30))
    for c in range(COLUMNS):
        x = LANES_X + c * COL_W + 1
        rows = {img.getpixel((x, y)) for y in range(bottom - h, bottom)}
        if (
            len(rows) == 1
            and rows != {background}
            and img.getpixel((x, bottom - h - 1)) == background
        ):
            return c
    return None


def start_xray(m) -> tuple[int, int, str, dict[str, str]]:
    """`run xray`, live: its id, where serial was, its window's id and its
    `apps` record."""
    app_id, mark = start(m, "xray")
    wid, _ = wait_tree(m, "X-ray", "cpu 0 runq")
    record = app(m, app_id)
    assert record is not None, app_id
    return app_id, mark, wid, record


def xray_cpu(m, record: dict[str, str]) -> str:
    """The CPU X-ray's thread runs on (`threads`)."""
    rows = parse_records(run(m, "threads").split("\n", 1)[1], "thread")
    return next(t["cpu"] for t in rows if t["id"] == record["thread"])


def test_xray_matches_the_scheduler(boot):
    # Two CPUs: X-ray (the first app) runs on CPU 0, and `trace runq spin`
    # starts 3 threads spinning on CPU 1, the last: one runs, two wait on
    # its queue. X-ray's run-queue label for CPU 1 and the scheduler's
    # count (`trace runq`) are read together until both say 2, its thread
    # label names one of the spinning threads and its switches there grow;
    # once they stop, until both say 0. Counts, polled until they agree,
    # never a time.
    m = desktop(boot, "-smp", "2")
    app_id, mark, wid, record = start_xray(m)
    assert xray_cpu(m, record) == "0", record
    cpu, spinning = spin(m, SPIN)
    assert cpu == 1, spinning

    def read() -> tuple[str, str, str]:
        rows = gui.ui_tree(m, wid)
        return scheduler_queue(m, cpu), label(rows, "cpu 1 runq"), label(rows, "cpu 1 thread")

    waiting = str(SPIN - 1)
    got = gui.wait_for(
        m,
        "X-ray's queue agreeing with the scheduler's",
        read,
        lambda g: g[0] == g[1] == waiting and g[2] in spinning,
        60,
    )
    # Its lane for that CPU draws them: a column whose bar is 3 threads
    # tall (a column shows its longest queue, and at each turn the thread
    # leaving the CPU is queued before the next is taken off: all 3),
    # polled for in screendumps (the lanes are its own damage).
    shots = iter(range(1000))
    gui.wait_for(
        m,
        "a 3-thread bar in cpu 1's lane",
        lambda: bar_of(gui.client_pixels(m, "X-ray", f"xray-lane-{next(shots)}.png"), 1, SPIN),
        lambda c: c is not None,
        60,
    )
    # Its switches on that CPU grow while they take turns (a floor).
    switched = int(label(gui.ui_tree(m, wid), "cpu 1 switches"))
    gui.wait_ui(
        m, "switches on cpu 1", lambda r: int(label(r, "cpu 1 switches")) > switched, wid, 60
    )
    stop_spinning(m, SPIN, cpu)
    gui.wait_for(m, "both queues empty again", read, lambda g: g[0] == g[1] == "0", 60)
    assert got[2] in spinning, got

    gui.close_window(m, "X-ray")
    end_line(m, mark, "xray", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])


def test_xray_pause_stops_its_reads(boot):
    # Paused, X-ray reads nothing (no period: the window settles), so its
    # label stays where it was while the queue it shows grows; resumed, it
    # reads on and catches up with the scheduler.
    m = desktop(boot, "-smp", "2")
    app_id, mark, wid, record = start_xray(m)
    # CPU 1 idle: its queue empty, or no `runq` event in its ring yet (`-`).
    rows = gui.wait_ui(m, "X-ray reading cpu 1", lambda r: label(r, "cpu 1") == "cpu 1", wid, 60)
    rows = gui.press_animated(
        m, below_root(rows), "Pause", "paused", lambda r: label(r, "State") == "paused", wid
    )
    gui.wait_settled(m)
    before = label(rows, "cpu 1 runq")
    assert before in ("-", "0"), rows
    cpu, _ = spin(m, SPIN)
    waiting = str(SPIN - 1)
    gui.wait_for(
        m, "the scheduler's queue", lambda: scheduler_queue(m, cpu), lambda q: q == waiting, 60
    )
    paused = below_root(gui.ui_tree(m, wid))
    assert label(paused, "cpu 1 runq") == before, paused
    assert label(paused, "State") == "paused", paused
    gui.press_animated(
        m,
        paused,
        "Resume",
        "X-ray caught up",
        lambda r: label(r, "State") == "live" and label(r, "cpu 1 runq") == waiting,
        wid,
        60,
    )
    stop_spinning(m, SPIN, cpu)
    gui.close_window(m, "X-ray")
    end_line(m, mark, "xray", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])


# `lumen_xray::replay`'s fixed trace, read once (its host test pins it):
# each CPU's labels as the frozen view shows them.
REPLAY = {
    "cpu 0": "cpu 0",
    "cpu 0 runq": "0",
    "cpu 0 thread": "3",
    "cpu 0 switches": "98",
    "cpu 0 irqs": "40",
    "cpu 0 locks": "4",
    "cpu 0 missed": "0",
    "cpu 1": "cpu 1 off",
    "cpu 1 runq": "-",
    "cpu 1 thread": "-",
    "cpu 1 switches": "9",
    "cpu 1 irqs": "9",
    "cpu 1 locks": "0",
    "cpu 1 missed": "0",
    "cpu 2": "cpu 2",
    "cpu 2 runq": "2",
    "cpu 2 thread": "22",
    "cpu 2 switches": "98",
    "cpu 2 irqs": "14",
    "cpu 2 locks": "2",
    "cpu 2 missed": "5",
    "State": "replay",
}


def test_xray_golden(boot, artifacts, golden_dir):
    # `run xray --replay`: the frozen view of a fixed trace (three CPUs,
    # the middle one there for ten columns, then offline), its labels as the trace gives them, Pause
    # and Resume disabled, no period (it settles), its window against a
    # golden; closed, it exits with 0.
    m = desktop(boot)
    app_id, mark = start(m, "xray --replay")
    _, rows = wait_tree(m, "X-ray", "cpu 2 runq")
    assert {name: label(rows, name) for name in REPLAY} == REPLAY, rows
    for button in ("Pause", "Resume"):
        assert "disabled" in gui.node(rows, "button", button)["state"], rows
    record = app(m, app_id)
    assert record is not None
    row = gui.window(m, "X-ray")
    x0, y0, x1, y1 = gui.client_box(row)
    shot = gui.settled_screendump(m, "xray.png")
    assert_matches_golden(
        shot,
        golden_dir / "app-xray.png",
        origin=(x0, y0),
        size=(x1 - x0, y1 - y0),
        artifacts=artifacts,
    )
    gui.close_window(m, "X-ray")
    end_line(m, mark, "xray", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])


def test_xray_without_trace_is_not_granted(boot):
    # `run --without trace xray`: live X-ray can't read the rings, so it
    # logs one ERROR naming the grant and exits with 1, opening no window.
    m = desktop(boot)
    refused = r"ERROR app: xray: no trace grant: NotGranted"
    app_id, mark = start(m, "--without trace xray", allow=refused)
    wait_for_output(m, mark, rf"(?m){refused}$", 30)
    end_line(m, mark, "xray", app_id, "exited with 1")
    assert not [w for w in gui.windows(m, refused) if w["title"] == "X-ray"]


def test_xray_counts_no_more_than_its_ring_holds(boot):
    # One CPU. After a few counted refreshes X-ray is paused (its labels
    # then stay put); the interrupts it counted on CPU 0 are at most the
    # `irq` events CPU 0's X-ray ring holds, read from 0, plus the numbers
    # gone from it: an upper bound from the same ring, never a time. A
    # reader that counted a ring's events more than once breaks it.
    m = desktop(boot)
    app_id, mark, wid, record = start_xray(m)
    rows = gui.wait_ui(
        m, "cpu 0's switches known", lambda r: label(r, "cpu 0 switches") != "-", wid, 60
    )
    for _ in range(3):
        seen = int(label(rows, "cpu 0 switches"))
        rows = gui.wait_ui(
            m,
            "a counted refresh",
            lambda r, seen=seen: int(label(r, "cpu 0 switches")) > seen,
            wid,
            60,
        )
    rows = gui.press_animated(
        m, below_root(rows), "Pause", "paused", lambda r: label(r, "State") == "paused", wid
    )
    gui.wait_settled(m)
    counted = int(label(below_root(gui.ui_tree(m, wid)), "cpu 0 irqs"))
    # The whole ring, up to 4096 lines: more time than a short command.
    out = run(m, "trace xray 0 0", 120)
    summary = XRAY_SUMMARY.search(out)
    assert summary, out
    events = out[: summary.start()]
    held = len(re.findall(r"^#\d+ +tsc=\d+ +irq ", events, re.MULTILINE))
    gone = int(summary[3])
    assert counted <= held + gone, (counted, held, gone)
    assert label(below_root(gui.ui_tree(m, wid)), "cpu 0 irqs") == str(counted)
    gui.close_window(m, "X-ray")
    end_line(m, mark, "xray", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])


def test_xray_refuses_arguments_it_doesnt_take(boot):
    # `run xray bogus`: its usage as one ERROR, exit 2, no window.
    m = desktop(boot)
    usage = r"ERROR app: xray: usage: xray \[--replay\]"
    app_id, mark = start(m, "xray bogus", allow=usage)
    wait_for_output(m, mark, rf"(?m){usage}$", 30)
    end_line(m, mark, "xray", app_id, "exited with 2")
    assert not [w for w in gui.windows(m, usage) if w["title"] == "X-ray"]
