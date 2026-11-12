"""Timekeeping: the TSC is calibrated at boot, the local APIC timer
interrupts arrive, and kmon's `sleep` waits in guest time.

The arithmetic is host-tested (`cargo test -p lumen-time`); calibration
against a second clock, `time::sleep` and the 1 kHz rate are checked in-kernel
(`lumen_kernel::time::*`, run by test_ktest). Host wall-clock time is never
asserted on: TCG and KVM run at different speeds.
"""

import re

BOOT_TIMEOUT = 60
TSC_LINE = re.compile(
    r"INFO  time: tsc (\d+(?:\.\d+)?) (GHz|MHz) \(calibrated against (hpet|pit)\)\n"
)
TICKS = re.compile(r"ticks: (\d+) \(spurious (\d+), unexpected (\d+)\)\n")
CLOCKS = re.compile(
    r"clocks: tsc (\d+) Hz \(against the (hpet|pit) at boot\); pit (\d+ Hz|none); hpet (\d+ Hz|none)\n"
)


def clocks(m) -> tuple[int, str, int | None, int | None]:
    """kmon `clocks`: the TSC's boot frequency and reference, and the TSC
    measured again against the PIT and the HPET (None: no such clock)."""
    m.send_line("clocks")
    c = m.expect(CLOCKS, 30)
    m.expect(r"kmon> ")

    def hz(s: str) -> int | None:
        return None if s == "none" else int(s.split()[0])

    return int(c[1]), c[2], hz(c[3]), hz(c[4])


def assert_close(a: int, b: int) -> None:
    """Within 2%: the tolerance for two calibrations of the same TSC."""
    assert abs(a - b) * 50 <= a, (a, b)


def ticks(m) -> tuple[int, int, int]:
    m.send_line("ticks")
    t = m.expect(TICKS)
    m.expect(r"kmon> ")
    return int(t[1]), int(t[2]), int(t[3])


def test_boot_calibrates_the_tsc_against_the_hpet(boot):
    m = boot("qemutest")
    line = m.expect(TSC_LINE, BOOT_TIMEOUT)
    assert line[3] == "hpet", line[0]
    m.expect(r"INFO  time: local APIC timer .*, ticking at 1 kHz\n", BOOT_TIMEOUT)
    m.expect(r"INFO  irq: interrupts on\n", BOOT_TIMEOUT)


def test_timer_interrupts_counted(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    before, _, _ = ticks(m)
    # 100 ms of guest time between the two readings.
    m.send_line("sleep 100")
    m.expect(r"slept 100 ms\n")
    m.expect(r"kmon> ")
    after, spurious, unexpected = ticks(m)
    # 100 at 1 kHz; QEMU's TCG delivers about 2/3 of a busy guest's ticks.
    assert after - before >= 40, (before, after)
    assert (spurious, unexpected) == (0, 0)
    assert "WARN  irq" not in m.output


def test_boot_calibration_agrees_with_both_clocks(boot):
    # The TSC measured again against the PIT and the HPET, independently of
    # the boot calibration (a wrong calibration cancels out in tick counts:
    # the local APIC timer is calibrated against the TSC).
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    tsc, reference, pit, hpet = clocks(m)
    assert reference == "hpet"
    assert pit is not None and hpet is not None
    assert_close(tsc, pit)
    assert_close(tsc, hpet)
