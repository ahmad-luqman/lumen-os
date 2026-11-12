"""SMP: the boot CPU starts every other CPU Limine reports (up to 8), each on
its own allocated stacks, and each says `INFO  smp: cpu N online (lapic id X)`.

QEMU numbers q35's local APICs 0, 1, 2, ... in CPU order, and Limine lists
them that way, so CPU N has APIC id N here (the kernel itself only requires
that each AP's local APIC agrees with what Limine reported).
"""

import re

BOOT_TIMEOUT = 60
ONLINE = re.compile(r"INFO  smp: cpu (\d+) online \(lapic id (\d+)\)\n")
STACKS = re.compile(
    r"INFO  smp: cpu (\d+) stacks: main (0x[0-9a-f]+)\.\.(0x[0-9a-f]+), "
    r"IST (0x[0-9a-f]+) (0x[0-9a-f]+) (0x[0-9a-f]+) \(guard page below each\)\n"
)
SUMMARY = re.compile(r"INFO  smp: (\d+) CPUs? online\n")


def boot_to_prompt(boot, cpus: int):
    m = boot("qemutest", extra=["-smp", str(cpus)])
    summary = m.expect(SUMMARY, BOOT_TIMEOUT)
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    return m, int(summary.group(1))


def test_all_cpus_online(boot):
    m, online = boot_to_prompt(boot, 4)
    assert online == 4
    assert [(int(c), int(a)) for c, a in ONLINE.findall(m.output)] == [(1, 1), (2, 2), (3, 3)]
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
    assert [int(c) for c, _ in ONLINE.findall(m.output)] == list(range(1, 8))
    assert "WARN  smp: 2 CPU(s) halted: the kernel runs on at most 8\n" in m.output
