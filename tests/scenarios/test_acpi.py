"""ACPI: the kernel reads the CPU list from the MADT and the HPET from its
table, and logs `INFO  acpi: N CPUs, LAPIC at 0x..., HPET at 0x...` (or
`..., no HPET`).

The parser itself is host-tested (`cargo test -p lumen-acpi`) and what the
kernel copies out is checked in-kernel (`lumen_kernel::acpi::*`, run by
test_ktest); this checks the count against the machine QEMU was told to
build, and that missing tables are warnings, not failures. QEMU lists the
extra `maxcpus` slots as disabled MADT entries, so that case fails if
disabled CPUs are counted.
"""

import re

import pytest

BOOT_TIMEOUT = 60
ACPI_LINE = re.compile(
    r"INFO  acpi: (\d+) CPUs?, LAPIC at (0x[0-9a-f]+), (?:HPET at (0x[0-9a-f]+)|no HPET)\n"
)
IOAPIC_LINE = re.compile(r"INFO  acpi: I/O APIC (\d+) at (0x[0-9a-f]+), GSI base (\d+)\n")
# Where q35 puts them (QEMU's hw/i386 defaults).
LAPIC = 0xFEE0_0000
IOAPIC = 0xFEC0_0000
HPET = 0xFED0_0000


@pytest.mark.parametrize(("smp", "cpus"), [("1", 1), ("2", 2), ("2,maxcpus=4", 2)])
def test_cpu_count_matches_qemu(boot, smp, cpus):
    m = boot("qemutest", extra=["-smp", smp])
    ioapic = m.expect(IOAPIC_LINE, BOOT_TIMEOUT)
    assert (int(ioapic[1]), int(ioapic[2], 16), int(ioapic[3])) == (0, IOAPIC, 0), ioapic[0]
    line = m.expect(ACPI_LINE, BOOT_TIMEOUT)
    assert int(line[1]) == cpus, line[0]
    assert (int(line[2], 16), line[3] and int(line[3], 16)) == (LAPIC, HPET), line[0]
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    assert "WARN  acpi" not in m.output


def test_no_hpet_is_a_warning(boot):
    m = boot("qemutest", extra=["-machine", "q35,hpet=off"])
    m.expect(r"WARN  acpi: no usable HPET: no HPET table\n", BOOT_TIMEOUT)
    line = m.expect(ACPI_LINE, BOOT_TIMEOUT)
    assert line[0] == f"INFO  acpi: 1 CPU, LAPIC at {LAPIC:#x}, no HPET\n"
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)


def test_no_madt_boots_without_acpi(boot):
    # `acpi=off` leaves OVMF's RSDP and root table but no MADT.
    m = boot("qemutest", extra=["-machine", "q35,acpi=off"])
    m.expect(r"WARN  acpi: running without ACPI: no APIC table\n", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    assert not ACPI_LINE.search(m.output)
