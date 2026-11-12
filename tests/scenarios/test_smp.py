"""SMP: the boot CPU starts every other CPU Limine reports (up to 8), each on
its own allocated stacks, and each says `INFO  smp: cpu N online (lapic id X)`.
A crash on any CPU is reported by that CPU (`cpu: N`, on its own stacks)
after it has stopped every other one (`other cpus stopped: ...`).

QEMU numbers q35's local APICs 0, 1, 2, ... in CPU order, and Limine lists
them that way, so CPU N has APIC id N here (the kernel itself only requires
that each AP's local APIC agrees with what Limine reported).
"""

import re

from lumen_harness import crash
from lumen_harness.qemu import EXIT_CRASH
from test_mm import OVERFLOW, assert_main_stack_overflow

BOOT_TIMEOUT = 60
ONLINE = re.compile(r"INFO  smp: cpu (\d+) online \(lapic id (\d+)\)\n")
STACKS = re.compile(
    r"INFO  smp: cpu (\d+) stacks: main (0x[0-9a-f]+)\.\.(0x[0-9a-f]+), "
    r"IST (0x[0-9a-f]+) (0x[0-9a-f]+) (0x[0-9a-f]+) \(guard page below each\)\n"
)
SUMMARY = re.compile(r"INFO  smp: (\d+) CPUs? online\n")
RUNNING_ON = re.compile(
    r"^running on: the (.+) stack (0x[0-9a-f]+)\.\.(0x[0-9a-f]+)$", re.MULTILINE
)


def boot_to_prompt(boot, cpus: int):
    m = boot("qemutest", extra=["-smp", str(cpus)])
    summary = m.expect(SUMMARY, BOOT_TIMEOUT)
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    return m, int(summary.group(1))


def test_all_cpus_online(boot):
    m, online = boot_to_prompt(boot, 4)
    assert online == 4
    # The APs start together, so they may say so in any order.
    assert sorted((int(c), int(a)) for c, a in ONLINE.findall(m.output)) == [(1, 1), (2, 2), (3, 3)]
    # Every AP has its own main and IST stacks, none shared with another
    # AP or with the boot CPU's (the `mem: stacks:` line).
    stacks = {int(s[0]): s[1:] for s in STACKS.findall(m.output)}
    assert sorted(stacks) == [1, 2, 3]
    boot_cpu = re.search(r"INFO  mem: stacks: main (0x[0-9a-f]+)\.\.", m.output).group(1)
    bottoms = [boot_cpu] + [b for s in stacks.values() for b in (s[0], s[2], s[3], s[4])]
    assert len(set(bottoms)) == len(bottoms) == 13
    assert "WARN  smp" not in m.output
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
    assert (
        "WARN  smp: 2 of 2 CPU(s) past the limit halted: the kernel runs on at most 8\n" in m.output
    )


def crash_on(boot, args: str, cpus: int = 4):
    """Boot `cpus` CPUs, run `crash <args>` in kmon, check QEMU exits with the
    crash code; returns (report, the boot log's stacks per AP)."""
    m, online = boot_to_prompt(boot, cpus)
    assert online == cpus
    m.send_line(f"crash {args}")
    m.expect(re.escape(crash.END), 30)
    assert m.wait_exit(10) == EXIT_CRASH
    out = m.output
    report = out[out.index(crash.BEGIN) : out.index(crash.END)]
    stacks = {int(s[0]): s[1:] for s in STACKS.findall(out)}
    return report, stacks


def test_crash_on_ap_reports_cpu(boot):
    report, stacks = crash_on(boot, "pf 1")
    assert "\ncpu: 1 (lapic id 1)\n" in report, report
    assert "\nother cpus stopped: 0 2 3\n" in report and "not stopped" not in report, report
    assert "exception: #PF page fault (vector 14," in report, report
    # It runs on CPU 1's own main stack, from its idle loop.
    kind, bottom, top = RUNNING_ON.search(report).groups()
    assert (kind, bottom, top) == ("kernel main", stacks[1][0], stacks[1][1]), report
    frames = crash.backtrace(report)
    assert "kmon::crash_page_fault" in frames[0] and any("smp::ap_main" in f for f in frames), (
        frames
    )
    # The trace is CPU 1's own ring: its online log line and the fault, none
    # of the boot CPU's events.
    events = re.findall(
        r"^  #\d+ +tsc=\d+ +(.*)$", report.split("recent trace events")[1], re.MULTILINE
    )
    assert events and all(e.startswith("cpu=1 ") for e in events), report
    assert "exception  vector=14" in events[-1], events


def test_double_fault_on_ap_runs_on_its_own_ist_stack(boot):
    report, stacks = crash_on(boot, "df 2")
    assert "\ncpu: 2 (lapic id 2)\n" in report, report
    # CPU 2's #DF stack (the first IST stack it logged), not another CPU's:
    # each AP has its own TSS.
    kind, bottom, _ = RUNNING_ON.search(report).groups()
    assert (kind, bottom) == ("#DF IST", stacks[2][2]), report


def test_stack_overflow_on_ap_hits_its_own_guard(boot):
    # Unbounded recursion on CPU 1's main stack runs into that stack's guard
    # page; the #DF runs on CPU 1's own #DF stack and names the overflow.
    report, stacks = crash_on(boot, "stack 1")
    assert "\ncpu: 1 (lapic id 1)\n" in report, report
    assert_main_stack_overflow(report, "kmon::recurse+")
    _, bottom, top = OVERFLOW.search(report).groups()
    assert (bottom, top) == (stacks[1][0], stacks[1][1]), report
    kind, ist_bottom, _ = RUNNING_ON.search(report).groups()
    assert (kind, ist_bottom) == ("#DF IST", stacks[1][2]), report


def test_panic_on_ap_is_reported_by_it(boot):
    report, _ = crash_on(boot, "panic 3")
    assert "\ncpu: 3 (lapic id 3)\n" in report and "\npanic: " in report, report
    assert "\nother cpus stopped: 0 1 2\n" in report, report


def test_crash_on_the_boot_cpu_stops_the_others(boot):
    report, _ = crash_on(boot, "pf")
    assert "\ncpu: 0 (lapic id 0)\n" in report, report
    assert "\nother cpus stopped: 1 2 3\n" in report and "not stopped" not in report, report


def test_crash_alone_names_no_other_cpus(boot):
    report, _ = crash_on(boot, "pf", cpus=1)
    assert "\ncpu: 0 (lapic id 0)\n" in report and "other cpus" not in report, report


def test_crash_on_a_cpu_that_isnt_online_is_refused(boot):
    m, _ = boot_to_prompt(boot, 2)
    m.send_line("crash pf 5")
    m.expect(r"crash: cpu 5 isn't online\n", 10)
    m.send_line("crash pf x")
    m.expect(r"crash: the cpu is a number \(0: this one\)\n", 10)
    m.send_line("echo still here")
    m.expect(r"still here\n", 10)
