"""ACPI: the kernel reads the CPU list from the MADT and the HPET from its
table, and logs `INFO  acpi: N CPUs, LAPIC at 0x..., HPET at 0x...` (or
`..., no HPET`).

The parser itself is host-tested (`cargo test -p lumen-acpi`) and what the
kernel copies out is checked in-kernel (`lumen_kernel::acpi::*`, run by
test_ktest); this checks the count against the machine QEMU was told to
build, and that missing tables are warnings, not failures. QEMU lists the
extra `maxcpus` slots as MADT entries that aren't enabled, so that case
fails if all listed CPUs are counted.
"""

import re

import pytest
from test_time import assert_close, clocks, ticks

BOOT_TIMEOUT = 60
ACPI_LINE = re.compile(
    r"INFO  acpi: (\d+) CPUs?, LAPIC at (0x[0-9a-f]+), (?:HPET at (0x[0-9a-f]+)|no HPET)\n"
)
IOAPIC_LINE = re.compile(r"INFO  acpi: I/O APIC (\d+) at (0x[0-9a-f]+), GSI base (\d+)\n")
# Where q35 puts them (QEMU's hw/i386 defaults).
LAPIC = 0xFEE0_0000
IOAPIC = 0xFEC0_0000
HPET = 0xFED0_0000


@pytest.mark.parametrize(
    ("smp", "cpus"), [("1", "1 CPU"), ("2", "2 CPUs"), ("2,maxcpus=4", "2 CPUs")]
)
def test_cpu_count_matches_qemu(boot, smp, cpus):
    m = boot("qemutest", extra=["-smp", smp])
    ioapic = m.expect(IOAPIC_LINE, BOOT_TIMEOUT)
    assert ioapic[0] == f"INFO  acpi: I/O APIC 0 at {IOAPIC:#x}, GSI base 0\n"
    line = m.expect(ACPI_LINE, BOOT_TIMEOUT)
    assert line[0] == f"INFO  acpi: {cpus}, LAPIC at {LAPIC:#x}, HPET at {HPET:#x}\n"
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    assert "WARN  acpi" not in m.output


def test_no_hpet_is_a_warning(boot):
    m = boot("qemutest", extra=["-machine", "q35,hpet=off"])
    m.expect(r"WARN  acpi: no usable HPET: no HPET table\n", BOOT_TIMEOUT)
    line = m.expect(ACPI_LINE, BOOT_TIMEOUT)
    assert line[0] == f"INFO  acpi: 1 CPU, LAPIC at {LAPIC:#x}, no HPET\n"
    # Timekeeping falls back to the PIT, and the PIT calibration repeats:
    # measured again against the PIT, within 2% (the same clock, so not an
    # independent check; the default boot cross-checks the PIT against the
    # HPET).
    m.expect(r"INFO  time: tsc .* \(calibrated against pit\)\n", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    tsc, reference, pit, hpet = clocks(m)
    assert (reference, hpet) == ("pit", None)
    assert pit is not None
    assert_close(tsc, pit)


def test_no_madt_boots_without_acpi(boot):
    # `acpi=off` leaves OVMF's RSDP and root table but no MADT.
    m = boot("qemutest", extra=["-machine", "q35,acpi=off"])
    m.expect(r"WARN  acpi: running without ACPI: no APIC table\n", BOOT_TIMEOUT)
    # The local APIC is found without ACPI, and timekeeping falls back to
    # the PIT (the HPET is only known from ACPI).
    m.expect(r"WARN  irq: no I/O APIC known: device interrupts can't be routed\n", BOOT_TIMEOUT)
    m.expect(r"INFO  time: tsc .* \(calibrated against pit\)\n", BOOT_TIMEOUT)
    m.expect(r"INFO  irq: interrupts on\n", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: boot complete", BOOT_TIMEOUT)
    # And the local APIC found through IA32_APIC_BASE ticks.
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    before, *_ = ticks(m)
    m.send_line("sleep 50")
    m.expect(r"kmon> ")
    after, *_ = ticks(m)
    assert after > before, (before, after)
    assert not ACPI_LINE.search(m.output)
