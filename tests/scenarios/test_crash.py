"""Crash reports instead of reboots (signature feature 5).

Every deliberate fault must produce a complete, symbolized crash report on
serial and exit QEMU with the crash code. A triple fault would instead end
QEMU with status 0 (`-no-reboot`), which fails these tests.
"""

import re

import pytest
from lumen_harness import REPO_ROOT
from lumen_harness.crash import BEGIN, END, backtrace, page_walk
from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import EXIT_CRASH

SCREEN_DONE = "(crash report shown on screen)"

# kind -> (exception line regex, function the rip must resolve to)
FAULTS = {
    "pf": (
        r"exception: #PF page fault \(vector 14, error code 0x2\)",
        "shell::crash::crash_page_fault",
    ),
    "ud": (r"exception: #UD invalid opcode \(vector 6,", "shell::crash::crash_invalid_opcode"),
    "de": (r"exception: #DE divide error \(vector 0,", "shell::crash::crash_divide_error"),
    "gp": (
        r"exception: #GP general protection fault \(vector 13,",
        "shell::crash::crash_general_protection",
    ),
    "df": (r"exception: #DF double fault \(vector 8,", "shell::crash::crash_double_fault"),
}


@pytest.mark.parametrize("kind", sorted(FAULTS))
def test_fault_produces_symbolized_report(crash_report, kind):
    report = crash_report(kind)
    exc_rx, func = FAULTS[kind]
    assert re.search(exc_rx, report), report
    rip = re.search(r"^rip: 0x[0-9a-f]{16} <(.+)>$", report, re.MULTILINE)
    assert rip and func in rip.group(1), report
    for reg in ("rax", "rbx", "rsp", "rbp", "r15", "cr0", "cr3", "cr4"):
        assert re.search(rf"{reg}\s*: 0x[0-9a-f]{{16}}", report), f"missing {reg}\n{report}"
    frames = backtrace(report)
    assert func in frames[0], report
    # The chain is walked all the way back to the entry point: from the
    # allocated main stack across the switch into `kmain` on the boot stack.
    assert any("kmain_on_kernel_stack" in f for f in frames), report
    assert any(f.startswith("kmain+") for f in frames), report
    assert frames[-1].startswith("_start"), report
    assert "<unknown>" not in report, report
    # The serial shell runs in the boot CPU's `main` thread (kmain's context).
    assert re.search(r"^thread: main \(id \d+\)$", report, re.MULTILINE), report
    # None of these is a stack overflow (`df` breaks rsp itself, so its #DF
    # runs on the IST stack with cr2 outside every guard page).
    assert "kernel stack overflow" not in report and "guard page hit" not in report, report


def test_report_shows_the_trace_events_leading_up_to_it(crash_report):
    report = crash_report("pf")
    trace = report[report.index("recent trace events (oldest first):") :]
    lines = re.findall(r"^  #\d+ +tsc=\d+ +(\S+.*)$", trace, re.MULTILINE)
    assert len(lines) == 16, trace
    # The last thing recorded before the report is the fault itself.
    assert re.match(r"exception +vector=14 rip=0x[0-9a-f]+ error=0x2$", lines[-1]), trace
    assert re.search(r"^trace: \d+ events claimed, 0 dropped$", trace, re.MULTILINE), trace


def test_panic_report_trace_ends_with_the_panic(crash_report):
    report = crash_report("panic")
    trace = report[report.index("recent trace events (oldest first):") :]
    lines = re.findall(r"^  #\d+ +tsc=\d+ +(\S+.*)$", trace, re.MULTILINE)
    at = re.search(r"^  at kernel/src/shell/crash\.rs:(\d+):\d+$", report, re.MULTILINE)
    assert at, report
    assert re.match(rf"panic +line={at.group(1)}$", lines[-1]), trace


def test_page_fault_report_names_address_and_cause(crash_report):
    report = crash_report("pf")
    assert "faulting address (cr2): 0x00000000deadb000" in report
    assert "page fault: write of non-present page (kernel mode)" in report
    # The page-table walk of cr2, after the backtrace: the lower half is
    # empty, so it stops at the first level.
    walk = page_walk(report)
    assert walk.virt == 0xDEADB000, report
    assert walk.steps == [(4, 0, 0)] and walk.end == "not present at L4", report
    assert report.index("page walk of") > report.index("backtrace:"), report
    # Then the heap's state, and last whether interrupts are (still) off.
    lines = report.splitlines()
    assert lines[-3].startswith("heap: ") and lines[-2:] == ["interrupts: off", END], lines[-3:]
    assert report.index("heap: ") > report.index("  -> not present"), report


def test_panic_produces_report(crash_report):
    report = crash_report("panic")
    assert "panic: deliberate panic from the shell" in report
    assert re.search(r"at kernel/src/shell/crash\.rs:\d+:\d+", report)
    frames = backtrace(report)
    assert any("shell::crash::crash_panic" in f for f in frames), report
    assert frames[-1].startswith("_start"), report


def test_crash_screen_golden(boot, artifacts, golden_dir):
    """The crash report is also shown on screen (banner checked against a golden)."""
    m = boot("interactive")  # no qemu_exit: the crash screen stays up
    m.expect(r"lumen> ", 60)
    m.send_line("crash ud")
    m.expect(re.escape(SCREEN_DONE), 30)  # drawing finished, not just serial
    shot = m.screendump("crash.png")
    assert_matches_golden(
        shot, golden_dir / "crash-banner.png", origin=(0, 0), size=(640, 72), artifacts=artifacts
    )


@pytest.mark.parametrize("kind", ["panic", "pf"])
def test_report_is_written_with_interrupts_off(crash_report, kind):
    # The serial shell runs with interrupts on; a crash turns them off for good, and no
    # lock the report takes and releases turns them back on.
    report = crash_report(kind)
    assert re.search(r"^interrupts: off$", report, re.MULTILINE), report


def test_report_names_the_thread(crash_report):
    # A fault in a spawned thread: the report names the thread, runs on the
    # thread's own stack, and the backtrace ends where every thread starts
    # (`context::thread_start`, rbp = 0), not in kmain.
    report = crash_report("pf thread")
    assert re.search(r"^thread: shell-crash \(id \d+\)$", report, re.MULTILINE), report
    assert re.search(
        r"^running on: the thread stack 0x[0-9a-f]+\.\.0x[0-9a-f]+$", report, re.MULTILINE
    ), report
    assert re.search(FAULTS["pf"][0], report), report
    frames = backtrace(report)
    assert "shell::crash::crash_page_fault" in frames[0], report
    assert any("sched::thread_entry" in f for f in frames), report
    assert "context::thread_start" in frames[-1], report
    assert not any("kmain" in f for f in frames), report
    assert "<unknown>" not in report and "(stopped:" not in report, report


def test_thread_stack_overflow_hits_its_guard(crash_report):
    # A thread's stack has its own guard page: unbounded recursion in a
    # spawned thread is reported as that stack overflowing.
    report = crash_report("stack thread")
    assert re.search(
        r"^kernel stack overflow \(guard page hit\): cr2 0x[0-9a-f]{16} is in the guard page below the "
        r"thread stack",
        report,
        re.MULTILINE,
    ), report
    assert re.search(r"^thread: shell-crash \(id \d+\)$", report, re.MULTILINE), report


@pytest.mark.parametrize("cpu", ["0", "1"])
def test_spinlock_relock_is_reported(boot, cpu):
    # A CPU taking a lock it already holds panics instead of spinning
    # forever with interrupts off: the report names the lock (the address
    # `crash relock` printed first, and its type), the CPU, and the second
    # `lock()`'s caller (track_caller: `crash_relock` in shell/crash.rs, not
    # sync.rs). On CPU 1 too, so the CPU isn't a constant 0.
    m = boot("qemutest", extra=["-smp", "2"])
    m.expect(r"lumen> ", 60)
    m.send_line(f"crash relock {cpu}")
    said = m.expect(r"crash: locking the SpinLock at (0x[0-9a-f]+) twice on cpu (\d+)\n", 30)
    addr, said_cpu = said.groups()
    assert said_cpu == cpu, said.group(0)
    # A missed re-lock spins with interrupts off: on CPU 1 the shell says so
    # after 10 s; on CPU 0 serial goes quiet (the expect times out).
    done = m.expect(
        rf"{re.escape(END)}|crash: SpinLock::lock returned .*|crash: cpu 1 took it but hasn't finished",
        30,
    )
    assert done.group(0) == END, f"no crash: {done.group(0)}"
    assert m.wait_exit(10) == EXIT_CRASH
    out = m.output
    report = out[out.index(BEGIN) : out.index(END) + len(END)]
    assert (
        f"\npanic: SpinLock<()> at {addr} re-locked on cpu {cpu}, which already holds it\n"
        in report
    ), report
    assert f"\ncpu: {cpu} (lapic id {cpu})\n" in report, report
    # The second `lock()`, the one marked in the source.
    source = (REPO_ROOT / "kernel/src/shell/crash.rs").read_text().splitlines()
    marked = [n for n, text in enumerate(source, 1) if "// the re-lock: panics here" in text]
    assert len(marked) == 1, marked
    assert re.search(rf"^  at kernel/src/shell/crash\.rs:{marked[0]}:\d+$", report, re.MULTILINE), (
        report
    )
    frames = backtrace(report)
    assert any("shell::crash::crash_relock" in f for f in frames), report
    # A complete report from inside a held lock (interrupts off): no nested
    # crash, then the heap line, `interrupts: off`, END.
    assert "!!!" not in report, report
    lines = report.splitlines()
    assert lines[-3].startswith("heap: ") and lines[-2:] == ["interrupts: off", END], lines[-3:]
