"""SMP: the boot CPU starts every other CPU Limine reports (up to 8), each on
its own allocated stacks, and each says `INFO  smp: cpu N online (lapic id X)`.
A crash on any CPU is reported by that CPU (`cpu: N`, on its own stacks)
after it has stopped every other one (`other cpus stopped: ...`).

QEMU numbers q35's local APICs 0, 1, 2, ... in CPU order, and Limine lists
them that way, so CPU N has APIC id N here (the kernel itself only requires
that each AP's local APIC agrees with what Limine reported).
"""

import re
from typing import NamedTuple

import pytest
from lumen_harness import crash
from lumen_harness.qemu import EXIT_CRASH
from test_crash import SCREEN_DONE
from test_mm import OVERFLOW, assert_main_stack_overflow

BOOT_TIMEOUT = 60
ONLINE = re.compile(r"INFO  smp: cpu (\d+) online \(lapic id (\d+)\)\n")
STACKS = re.compile(
    r"INFO  smp: cpu (\d+) stacks: main (0x[0-9a-f]+)\.\.(0x[0-9a-f]+), "
    r"IST (0x[0-9a-f]+) (0x[0-9a-f]+) (0x[0-9a-f]+) \(guard page below each\)\n"
)
BOOT_CPU_STACKS = re.compile(
    r"INFO  mem: stacks: main (0x[0-9a-f]+)\.\.0x[0-9a-f]+, "
    r"IST (0x[0-9a-f]+) (0x[0-9a-f]+) (0x[0-9a-f]+) \(guard page below each\)\n"
)
SUMMARY = re.compile(r"INFO  smp: (\d+) CPUs? online\n")
RUNNING_ON = re.compile(
    r"^running on: the (.+) stack (0x[0-9a-f]+)\.\.(0x[0-9a-f]+)$", re.MULTILINE
)


class ApStacks(NamedTuple):
    """An AP's stacks, from its boot log line (the IST ones by bottom)."""

    main_bottom: str
    main_top: str
    df: str
    nmi: str
    mc: str


def ap_stacks(output: str) -> dict[int, ApStacks]:
    return {int(s[0]): ApStacks(*s[1:]) for s in STACKS.findall(output)}


def boot_to_prompt(boot, cpus: int):
    m = boot("qemutest", extra=["-smp", str(cpus)])
    summary = m.expect(SUMMARY, BOOT_TIMEOUT)
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m, int(summary.group(1))


def reported_by(report: str, cpu: int) -> bool:
    return f"\ncpu: {cpu} (lapic id {cpu})\n" in report


def running_on(report: str) -> tuple[str, str, str]:
    """The report's own stack: (kind, bottom, top)."""
    found = RUNNING_ON.search(report)
    assert found, report
    return found.groups()


def test_all_cpus_online(boot):
    m, online = boot_to_prompt(boot, 4)
    assert online == 4
    # The APs start together, so they may say so in any order.
    assert sorted((int(c), int(a)) for c, a in ONLINE.findall(m.output)) == [(1, 1), (2, 2), (3, 3)]
    # Every AP has its own main and IST stacks, none shared with another AP
    # or with the boot CPU's four.
    stacks = ap_stacks(m.output)
    assert sorted(stacks) == [1, 2, 3]
    bottoms = list(BOOT_CPU_STACKS.search(m.output).groups())
    bottoms += [b for s in stacks.values() for b in (s.main_bottom, s.df, s.nmi, s.mc)]
    assert len(set(bottoms)) == len(bottoms) == 16
    assert "WARN  smp" not in m.output
    # Every CPU runs threads (its timer ticks), and none is warned about.
    assert "INFO  sched: threads on 4 CPUs, 10 ms slices\n" in m.output
    assert "WARN  sched" not in m.output
    # The boot timeline is still complete (it is logged at warn if not).
    assert "WARN  boot" not in m.output


def test_one_cpu_boots_alone(boot):
    m, online = boot_to_prompt(boot, 1)
    assert online == 1
    assert not ONLINE.search(m.output) and "WARN  smp" not in m.output


def test_cpus_past_the_limit_are_halted(boot):
    m, online = boot_to_prompt(boot, 10)
    assert online == 8
    assert sorted(int(c) for c, _ in ONLINE.findall(m.output)) == list(range(1, 8))
    # Counted by the halt loop they were sent to, so they got there.
    assert "WARN  smp: 2 CPU(s) past the limit halted: the kernel runs on at most 8\n" in m.output


def crash_on(boot, args: str, cpus: int = 4):
    """Boot `cpus` CPUs, run `crash <args>` in the shell, check QEMU exits with the
    crash code after a complete report (no `!!!` line: every other CPU
    stopped through its stop NMI; the heap line and `interrupts: off` at
    the end; drawn on screen too). Returns (report, the APs' logged stacks)."""
    m, online = boot_to_prompt(boot, cpus)
    assert online == cpus
    m.send_line(f"crash {args}")
    # A deliberate protection fault that doesn't fault says so at once.
    done = m.expect(rf"{re.escape(crash.END)}|{crash.PROTECTION_BROKEN}", 30)
    assert done.group(0) == crash.END, f"no crash: {done.group(0)}"
    assert m.wait_exit(10) == EXIT_CRASH
    out = m.output
    report = out[out.index(crash.BEGIN) : out.index(crash.END) + len(crash.END)]
    assert "!!!" not in report, report
    lines = report.splitlines()
    assert lines[-3].startswith("heap: ") and lines[-2:] == ["interrupts: off", crash.END], report
    assert SCREEN_DONE in out[out.index(crash.END) :], out[-2000:]
    return report, ap_stacks(out)


def test_crash_on_ap_reports_cpu(boot):
    report, stacks = crash_on(boot, "pf 1")
    assert reported_by(report, 1), report
    assert "\nother cpus stopped: 0 2 3\n" in report, report
    assert "exception: #PF page fault (vector 14," in report, report
    # It runs on CPU 1's own main stack, from its idle loop (its idle thread).
    assert running_on(report) == ("kernel main", stacks[1].main_bottom, stacks[1].main_top), report
    assert re.search(r"^thread: idle \(id \d+\)$", report, re.MULTILINE), report
    frames = crash.backtrace(report)
    assert "shell::crash::crash_page_fault" in frames[0] and any(
        "smp::ap_main" in f for f in frames
    ), frames
    # The trace is CPU 1's own ring: its online log line and the fault, none
    # of the boot CPU's events.
    events = re.findall(
        r"^  #\d+ +tsc=\d+ +(.*)$", report.split("recent trace events")[1], re.MULTILINE
    )
    assert events and all(e.startswith("cpu=1 ") for e in events), report
    assert "exception  vector=14" in events[-1], events


def test_double_fault_on_ap_runs_on_its_own_ist_stack(boot):
    report, stacks = crash_on(boot, "df 2")
    assert reported_by(report, 2) and "\nother cpus stopped: 0 1 3\n" in report, report
    # CPU 2's #DF stack (the first IST stack it logged), not another CPU's:
    # each AP has its own TSS.
    kind, bottom, _ = running_on(report)
    assert (kind, bottom) == ("#DF IST", stacks[2].df), report


def test_stack_overflow_on_ap_hits_its_own_guard(boot):
    # Unbounded recursion on CPU 1's main stack runs into that stack's guard
    # page; the #DF runs on CPU 1's own #DF stack and names the overflow.
    report, stacks = crash_on(boot, "stack 1")
    assert reported_by(report, 1), report
    assert_main_stack_overflow(report, "shell::crash::recurse+")
    _, bottom, top = OVERFLOW.search(report).groups()
    assert (bottom, top) == (stacks[1].main_bottom, stacks[1].main_top), report
    kind, ist_bottom, _ = running_on(report)
    assert (kind, ist_bottom) == ("#DF IST", stacks[1].df), report


def test_touching_a_guard_page_on_an_ap_names_its_own_stack(boot):
    # `crash guard 2` writes into CPU 2's own main-stack guard page: the
    # stray-access line names CPU 2's stack, not the boot CPU's.
    report, stacks = crash_on(boot, "guard 2")
    assert reported_by(report, 2), report
    hit = re.search(
        r"^guard page hit: cr2 0x[0-9a-f]{16} is in the guard page below the kernel main stack \[(0x[0-9a-f]+), (0x[0-9a-f]+)\)",
        report,
        re.MULTILINE,
    )
    assert hit and hit.groups() == (stacks[2].main_bottom, stacks[2].main_top), report


@pytest.mark.parametrize(
    ("kind", "error"),
    [
        ("wtext", "write of protection violation"),
        ("nx", "instruction fetch of protection violation"),
    ],
)
def test_protection_holds_on_aps(boot, kind, error):
    # W^X binds on an AP too (its own CR0.WP and EFER.NXE, on the kernel's
    # tables): writing the kernel's text, or running its data, faults.
    report, _ = crash_on(boot, f"{kind} 3")
    assert reported_by(report, 3) and f"page fault: {error} (kernel mode)" in report, report


def test_panic_on_ap_is_reported_by_it(boot):
    report, _ = crash_on(boot, "panic 3")
    assert reported_by(report, 3) and "\npanic: " in report, report
    assert "\nother cpus stopped: 0 1 2\n" in report, report


def test_crash_on_the_boot_cpu_stops_the_others(boot):
    report, _ = crash_on(boot, "pf")
    assert reported_by(report, 0) and "\nother cpus stopped: 1 2 3\n" in report, report


def test_crash_alone_names_no_other_cpus(boot):
    # `0` is the boot CPU, which runs the serial shell: the fault runs right there.
    report, _ = crash_on(boot, "pf 0", cpus=1)
    assert reported_by(report, 0) and "other cpus" not in report, report


def test_crash_on_a_cpu_that_isnt_online_is_refused(boot):
    m, _ = boot_to_prompt(boot, 2)
    for cpu in (5, 8, 99):  # past the CPUs present, at the limit, far past it
        m.send_line(f"crash pf {cpu}")
        m.expect(rf"crash: cpu {cpu} isn't online\n", 10)
    m.send_line("crash pf x")
    m.expect(r"crash: the cpu is a number \(0: this one\) or 'thread'\n", 10)
    m.send_line("crash pf 1 2")
    m.expect(r"crash: usage: crash <kind> \[cpu\|thread\]\n", 10)
    m.send_line("echo still here")
    m.expect(r"still here\n", 10)
