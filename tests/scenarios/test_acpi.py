"""ACPI: the kernel reads the CPU list from the MADT and the HPET from its
table, and logs `INFO  acpi: N CPUs, LAPIC at 0x..., HPET at 0x...`.

The parser itself is host-tested (`cargo test -p lumen-acpi`) and the boot
CPU's MADT entry is checked in-kernel (`lumen_kernel::acpi::*`, run by
test_ktest); this checks the count against the machine QEMU was told to
build, including hot-pluggable CPU slots (`maxcpus`), which the MADT lists
but doesn't enable.
"""

import re

import pytest

BOOT_TIMEOUT = 60
ACPI_LINE = re.compile(r"INFO  acpi: (\d+) CPUs?, LAPIC at (0x[0-9a-f]+), HPET at (0x[0-9a-f]+)\n")
# Where q35 puts them (QEMU's hw/i386 defaults).
LAPIC = 0xFEE0_0000
HPET = 0xFED0_0000


@pytest.mark.parametrize(("smp", "cpus"), [("1", 1), ("2", 2), ("2,maxcpus=4", 2)])
def test_cpu_count_matches_qemu(boot, smp, cpus):
    m = boot("qemutest", extra=["-smp", smp])
    line = m.expect(ACPI_LINE, BOOT_TIMEOUT)
    assert int(line[1]) == cpus, line[0]
    assert (int(line[2], 16), int(line[3], 16)) == (LAPIC, HPET), line[0]
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    assert "WARN  acpi" not in m.output
