"""Timekeeping: the TSC is calibrated at boot, the local APIC timer
interrupts arrive, and kmon's `sleep` waits in guest time.

The arithmetic is host-tested (`cargo test -p lumen-time`); calibration
against a second clock, `time::sleep` and the 1 kHz rate are checked in-kernel
(`lumen_kernel::time::*`, run by test_ktest). Host wall-clock time is never
asserted on: TCG and KVM run at different speeds.
"""

import re
from typing import NamedTuple

BOOT_TIMEOUT = 60
TSC_LINE = re.compile(
    r"INFO  time: tsc (\d+(?:\.\d+)?) (GHz|MHz) \(calibrated against (hpet|pit)\)\n"
)
TICKS = re.compile(r"ticks: (\d+) \(spurious (\d+), unexpected (\d+), local APIC errors (\d+)\)\n")
CLOCKS = re.compile(
    r"clocks: tsc (\d+) Hz \(against the (hpet|pit) at boot\); "
    r"pit (?:(\d+) Hz|none|failed \((.*)\)); hpet (?:(\d+) Hz|none|failed \((.*)\))\n"
)


# One measurement on the `clocks: windows:` line:
# `<name> <hz> Hz [tsc/reference/polls/start_gap/end_gap ...] retook <n>`.
WINDOWS_SEGMENT = re.compile(r"(boot|pit|hpet) (\d+) Hz \[([^\]]*)\] retook (\d+)")
PIT_HZ = 1_193_182
HPET_HZ = 100_000_000  # QEMU's HPET ticks every 10 ns
# A calibration takes at most 20 windows for 5 steady ones (time::ATTEMPTS).
MOST_RETAKEN = 15


class Calibration(NamedTuple):
    hz: int
    windows: str
    retook: int


def clocks(m) -> tuple[int, str, int | None, int | None]:
    """kmon `clocks`: the TSC's boot frequency and reference, and the TSC
    measured again against the PIT and the HPET (None: no such clock)."""
    return clocks_and_windows(m)[0]


def clocks_and_windows(m) -> tuple[tuple[int, str, int | None, int | None], dict[str, Calibration]]:
    """kmon `clocks`, and the windows line after it: each measurement's
    calibration by name (`boot`, `pit`, `hpet`; a failed or missing one is
    left out)."""
    m.send_line("clocks")
    c = m.expect(CLOCKS, 30)
    w = m.expect(r"clocks: windows:(.*)\n", 30)
    m.expect(r"kmon> ")
    for name, reason in (("pit", c[4]), ("hpet", c[6])):
        assert reason is None, f"measuring against the {name} failed: {reason}"
    pit, hpet = (None if g is None else int(g) for g in c.group(3, 5))
    # Every segment must be a whole calibration; findall alone would skip a
    # malformed one.
    segments = [seg.strip() for seg in w[1].split(";")]
    calibrations = {}
    for seg in segments:
        found = WINDOWS_SEGMENT.fullmatch(seg)
        assert found, f"not a calibration: {seg!r} in {w[0]!r}"
        name, hz, windows, retook = found.groups()
        assert name not in calibrations, f"{name} twice in {w[0]!r}"
        calibrations[name] = Calibration(int(hz), windows, int(retook))
    return (int(c[1]), c[2], pit, hpet), calibrations


def assert_close(a: int, b: int) -> None:
    """Within 2%: the tolerance for two calibrations of the same TSC."""
    assert abs(a - b) * 50 <= a, (a, b)


def check_calibration(cal: Calibration, reference_hz: int) -> None:
    """The frequency is exactly what the kernel computes from the windows:
    the median of five, each steady (the gaps at its start and end add up to
    at most 0.5% of it) and so within 1% of the median (a steady window is off
    by at most its gaps), with at most MOST_RETAKEN taken again."""
    hz = []
    for w in cal.windows.split():
        fields = w.split("/")
        assert len(fields) == 5, f"not tsc/reference/polls/start_gap/end_gap: {w}"
        tsc, ref, polls, start_gap, end_gap = (int(x) for x in fields)
        assert ref > 0 and polls > 0, w
        assert (start_gap + end_gap) * 200 <= tsc, f"not steady: {w}"
        hz.append(tsc * reference_hz // ref)
    assert len(hz) == 5, cal
    assert sorted(hz)[2] == cal.hz, cal
    for one in hz:
        assert abs(one - cal.hz) * 100 <= cal.hz, (
            f"a window {one} Hz, the median {cal.hz} Hz: {cal}"
        )
    assert cal.retook <= MOST_RETAKEN, cal


def check_retake_line(m, cal: Calibration) -> None:
    """Boot logs how many windows it took again exactly when it took any."""
    line = f"INFO  time: tsc calibration took {cal.retook} windows again"
    assert (line in m.output) == (cal.retook > 0), (cal.retook, line)


def ticks(m) -> tuple[int, int, int, int]:
    """kmon `ticks`: timer interrupts, then spurious and unexpected ones and
    local APIC errors."""
    m.send_line("ticks")
    t = m.expect(TICKS)
    m.expect(r"kmon> ")
    return int(t[1]), int(t[2]), int(t[3]), int(t[4])


def test_boot_calibrates_the_tsc_against_the_hpet(boot):
    m = boot("qemutest")
    line = m.expect(TSC_LINE, BOOT_TIMEOUT)
    assert line[3] == "hpet", line[0]
    m.expect(r"INFO  time: local APIC timer .*, ticking at 1 kHz\n", BOOT_TIMEOUT)
    m.expect(r"INFO  irq: interrupts on\n", BOOT_TIMEOUT)


def test_clocks_shows_the_windows_behind_each_measurement(boot):
    # kmon `clocks` lists every window behind the boot calibration and the two
    # measurements, and each frequency is exactly their median.
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    (tsc, reference, pit, hpet), cals = clocks_and_windows(m)
    assert reference == "hpet"
    assert sorted(cals) == ["boot", "hpet", "pit"]
    assert (cals["boot"].hz, cals["pit"].hz, cals["hpet"].hz) == (tsc, pit, hpet)
    check_calibration(cals["boot"], HPET_HZ)
    check_calibration(cals["pit"], PIT_HZ)
    check_calibration(cals["hpet"], HPET_HZ)
    check_retake_line(m, cals["boot"])


def test_timer_interrupts_counted(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    before, *_ = ticks(m)
    # 100 ms of guest time between the two readings.
    m.send_line("sleep 100")
    m.expect(r"slept 100 ms\n")
    m.expect(r"kmon> ")
    after, *strays = ticks(m)
    # Up to 100 at 1 kHz. A smoke test that interrupts arrive (the rate is
    # checked in-kernel against the HPET): QEMU's TCG delivers ticks late, as
    # few as 18% (9 in test_no_madt's 50 ms) in #45's stress runs (kmon's
    # sleep busy-waited then; it now halts in the idle thread).
    assert after - before >= 1, (before, after)
    assert strays == [0, 0, 0]
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
