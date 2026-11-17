"""Timekeeping: the TSC is calibrated at boot, the local APIC timer
interrupts arrive, and the shell's `sleep` waits in guest time.

The arithmetic is host-tested (`cargo test -p lumen-time`); calibration
against a second clock, `time::sleep` and the 1 kHz rate are checked in-kernel
(`lumen_kernel::time::*`, run by test_ktest). Host wall-clock time is never
asserted on: TCG and KVM run at different speeds.
"""

import re
from typing import NamedTuple

from lumen_harness.clock_diags import TSC_TOLERANCE_PPM
from lumen_harness.crash import BEGIN, END
from lumen_harness.qemu import EXIT_CRASH

BOOT_TIMEOUT = 60
TSC_LINE = re.compile(
    r"INFO  time: tsc (\d+(?:\.\d+)?) (GHz|MHz) \(calibrated against (hpet|pit)\)\n"
)
TICKS = re.compile(r"ticks: (\d+) \(spurious (\d+), unexpected (\d+), local APIC errors (\d+)\)\n")
CLOCKS = re.compile(
    r"clocks: tsc (\d+) Hz \(against the (hpet|pit) at boot, within (?P<boot_ppm>\d+) ppm\); "
    r"pit (?:(\d+) Hz within (?P<pit_ppm>\d+) ppm|none|failed \((.*)\)); "
    r"hpet (?:(\d+) Hz within (?P<hpet_ppm>\d+) ppm|none|failed \((.*)\))\n"
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
    """the shell's `clocks`: the TSC's boot frequency and reference, and the TSC
    measured again against the PIT and the HPET (None: no such clock)."""
    return clocks_and_windows(m)[0]


def clocks_and_windows(m) -> tuple[tuple[int, str, int | None, int | None], dict[str, Calibration]]:
    """the shell's `clocks`, and the windows line after it: each measurement's
    calibration by name (`boot`, `pit`, `hpet`; a failed or missing one is
    left out)."""
    m.send_line("clocks")
    c = m.expect(CLOCKS, 30)
    w = m.expect(r"clocks: windows:(.*)\n", 30)
    m.expect(r"lumen> ")
    for name, reason in (("pit", c[6]), ("hpet", c[9])):
        assert reason is None, f"measuring against the {name} failed: {reason}"
    pit, hpet = (None if g is None else int(g) for g in c.group(4, 7))
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
    # Each bound `clocks` prints is its windows' (the TSC's rate's).
    for name, cal in calibrations.items():
        printed = c[f"{name}_ppm"]
        assert printed is not None and int(printed) == median_bound_ppm(cal), (
            name,
            c[0],
            w[0],
        )
    return (int(c[1]), c[2], pit, hpet), calibrations


def assert_close(a: int, b: int) -> None:
    """Within 2%: the tolerance for two calibrations of the same TSC."""
    assert abs(a - b) * 50 <= a, (a, b)


def tsc_error_ppm(tsc: int, ref: int, start_gap: int, end_gap: int) -> int:
    """How far off (at most, ppm of the true rate) the TSC rate a window gives
    can be: lumen_time's Window::tsc_error_ppm, rounded up the same way."""
    if tsc == 0 or end_gap >= tsc or ref < 2:
        return 2**64 - 1
    high = -(-(tsc + end_gap * ref) * 1_000_000 // ((tsc - end_gap) * ref))
    low = -(-(tsc + start_gap * ref) * 1_000_000 // ((tsc + start_gap) * ref))
    return max(high, low)


def within_bounds(a: int, b: int, a_ppm: int, b_ppm: int) -> bool:
    """Whether `a` (off by at most `a_ppm` of the true rate) and `b` (at most
    `b_ppm`) can measure the same rate: `|a - b| <= (a_ppm + b_ppm) * true`,
    the true rate being at most `b / (1 - b_ppm)`; never with a measurement
    that has no bound (as lumen_time::within_bounds, without its floor)."""
    if a_ppm >= 1_000_000 or b_ppm >= 1_000_000:
        return False
    return abs(a - b) * (1_000_000 - b_ppm) <= b * (a_ppm + b_ppm)


def window_fields(w: str) -> tuple[int, int, int, int, int]:
    """`tsc/reference/polls/start_gap/end_gap` as numbers."""
    fields = w.split("/")
    assert len(fields) == 5, f"not tsc/reference/polls/start_gap/end_gap: {w}"
    return tuple(int(x) for x in fields)  # type: ignore[return-value]


def median_bound_ppm(cal: Calibration) -> int:
    """The most the median of `cal`'s windows can be off: the third smallest
    window bound (lumen_time::median_bound_ppm_by)."""
    bounds = []
    for w in cal.windows.split():
        tsc, ref, _, start_gap, end_gap = window_fields(w)
        bounds.append(tsc_error_ppm(tsc, ref, start_gap, end_gap))
    return sorted(bounds)[2]


def check_calibration(cal: Calibration, reference_hz: int) -> None:
    """The frequency is exactly what the kernel computes from the windows: the
    median of five (steady ones first, then, if attempts ran out, the least
    unsteady filling in, #139), its bound within the TSC's tolerance, and each
    window within its own bound plus the median's of it (each is off from the
    true rate by at most its bound, so the two within their bounds' sum of
    it), with at most MOST_RETAKEN taken again
    unless some filled in."""
    hz, bounds = [], []
    for w in cal.windows.split():
        tsc, ref, polls, start_gap, end_gap = window_fields(w)
        assert ref > 0 and polls > 0, w
        hz.append(tsc * reference_hz // ref)
        bounds.append(tsc_error_ppm(tsc, ref, start_gap, end_gap))
    assert len(hz) == 5, cal
    assert sorted(hz)[2] == cal.hz, cal
    median_ppm = median_bound_ppm(cal)
    assert median_ppm <= TSC_TOLERANCE_PPM, f"median within {median_ppm} ppm: {cal}"
    for one, ppm in zip(hz, bounds, strict=True):
        assert within_bounds(one, cal.hz, ppm, median_ppm), (
            f"a window {one} Hz ({ppm} ppm), the median {cal.hz} Hz ({median_ppm} ppm): {cal}"
        )
    steady = sum(
        (sg + eg) * 200 <= t for t, _, _, sg, eg in map(window_fields, cal.windows.split())
    )
    assert cal.retook <= MOST_RETAKEN or steady < 5, cal


def check_retake_line(m, cal: Calibration) -> None:
    """Boot logs how many windows it took again exactly when it took any."""
    line = f"INFO  time: tsc calibration took {cal.retook} windows again"
    assert (line in m.output) == (cal.retook > 0), (cal.retook, line)


def ticks(m) -> tuple[int, int, int, int]:
    """the shell's `ticks`: timer interrupts, then spurious and unexpected ones and
    local APIC errors."""
    m.send_line("ticks")
    t = m.expect(TICKS)
    m.expect(r"lumen> ")
    return int(t[1]), int(t[2]), int(t[3]), int(t[4])


def test_boot_calibrates_the_tsc_against_the_hpet(boot):
    m = boot("qemutest")
    line = m.expect(TSC_LINE, BOOT_TIMEOUT)
    assert line[3] == "hpet", line[0]
    m.expect(r"INFO  time: local APIC timer .*, ticking at 1 kHz\n", BOOT_TIMEOUT)
    m.expect(r"INFO  irq: interrupts on\n", BOOT_TIMEOUT)


def test_clocks_shows_the_windows_behind_each_measurement(boot):
    # the shell's `clocks` lists every window behind the boot calibration and the two
    # measurements, and each frequency is exactly their median.
    m = boot("qemutest")
    m.expect(r"lumen> ", BOOT_TIMEOUT)
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
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    before, *_ = ticks(m)
    # 100 ms of guest time between the two readings.
    m.send_line("sleep 100")
    m.expect(r"slept 100 ms\n")
    m.expect(r"lumen> ")
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
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    tsc, reference, pit, hpet = clocks(m)
    assert reference == "hpet"
    assert pit is not None and hpet is not None
    assert_close(tsc, pit)
    assert_close(tsc, hpet)


# Stalls injected into the boot's calibration windows (time::stall_window_ends:
# `lumen.stall_*_windows=<end ppm>[,<start ppm>]`, a deliberate fault like the
# shell's `crash`), so a real boot reaches the fill-in, its warning and its
# panic (#139). An end stall of over 0.5% makes every window unsteady: all 20
# are taken and the five with the smallest bounds fill in; the median's bound
# is then about the stall. (A stalled end counts for at most two reads, so
# HPET windows stretch for it only where reads are slow, as on KVM, and a
# little; each window stalls a share of its own run, so the bound stays about
# the stall either way.) Its verdict is by each clock's tolerance
# (lumen_time's: the TSC's 1%, the timer's 2%, the TSC's error included).
FILLED = r"took 20 windows again, 0 of 5 kept steady, median within (\d+\.\d)%"
PANIC = re.compile(r"\npanic: (.*)\n")


def panic_message(out: str) -> str:
    """The `panic:` line of the crash report in `out` (between its BEGIN and
    END markers, as test_crash reads it)."""
    report = out[out.index(BEGIN) : out.index(END) + len(END)]
    said = PANIC.search(report)
    assert said, report
    return said[1]


def boot_messages(m) -> str:
    """The boot's output up to the first prompt."""
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m.output[: m.output.index("lumen> ")]


def test_tsc_windows_stalled_past_steady_fill_in_within_tolerance(boot):
    # 0.6% at each window's end: none steady, the median within 0.6% and a
    # bit (rounded up to a tenth), so an INFO line, no warning, and the HPET
    # kept (an unsteady HPET used to give way to the PIT, which stalls with
    # it). The bound is honest: the stall makes the boot's rate high by about
    # that much, and the HPET measured again (unstalled) agrees within the
    # two bounds.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=6000")
    out = boot_messages(m)
    assert TSC_LINE.search(out)[3] == "hpet", out
    said = re.search(r"INFO  time: tsc calibration " + FILLED, out)
    assert said, out
    assert 0.6 <= float(said[1]) <= 1.0, said[0]
    assert "WARN" not in out, out
    (tsc, _, _, hpet), cals = clocks_and_windows(m)
    boot_ppm, hpet_ppm = median_bound_ppm(cals["boot"]), median_bound_ppm(cals["hpet"])
    # The unstalled measurement must be within the TSC's tolerance itself, or
    # the comparison could pass on its bound alone.
    assert hpet_ppm <= TSC_TOLERANCE_PPM, (hpet_ppm, cals["hpet"])
    assert within_bounds(tsc, hpet, boot_ppm, hpet_ppm), (
        tsc,
        hpet,
        boot_ppm,
        hpet_ppm,
    )
    assert (tsc - hpet) * 1_000_000 >= hpet * 3_000, (
        f"the stall didn't move the rate: {tsc} against {hpet}"
    )


def test_tsc_windows_stalled_past_tolerance_warn(boot):
    # 1.5%: past the TSC's 1%. The HPET isn't kept on its own word: the PIT
    # is tried too (stalled the same way here) and whichever's median can be
    # off less is kept, said at INFO; then a warning with that one's bound,
    # and the boot goes on.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=15000")
    out = boot_messages(m)
    assert (
        "INFO  time: stalling calibration windows by 15000,0 ppm (lumen.stall_tsc_windows)\n" in out
    ), out
    chose = re.search(
        r"INFO  time: tsc calibration: against the HPET within (\d+\.\d)% \(past the 1\.0% allowed\), "
        r"against the PIT within (\d+\.\d)%: kept the (HPET|PIT)\n",
        out,
    )
    assert chose, out
    hpet_pc, pit_pc, kept = float(chose[1]), float(chose[2]), chose[3]
    assert kept == ("PIT" if pit_pc < hpet_pc else "HPET") or hpet_pc == pit_pc, chose[0]
    assert TSC_LINE.search(out)[3] == kept.lower(), out
    said = re.search(
        rf"WARN  time: tsc calibration: only 0 of 20 windows read promptly \(5 wanted; a stall next to "
        rf"the {kept}'s reads\), so its rate may be off by up to (\d+\.\d)%, over the 1\.0% allowed",
        out,
    )
    assert said, out
    assert 1.5 <= float(said[1]) <= 2.0, said[0]
    assert float(said[1]) == min(hpet_pc, pit_pc), (said[0], chose[0])
    # The timer, its own windows fine, with the TSC's 1.5-2% on top is still
    # within its 2%, or says the TSC is why not (never its own reads).
    assert "a stall next to the timer's register accesses" not in out, out
    # The HPET counted, so it stays a second clock whichever was kept.
    (_, reference, pit, hpet), _ = clocks_and_windows(m)
    assert reference == kept.lower() and pit is not None and hpet is not None, (
        reference,
        pit,
        hpet,
    )


def test_a_timer_off_through_the_tsc_says_so(boot):
    # The TSC stalled 3% (past its 1%: a warning) and the timer not: the
    # timer's windows are fine, but measured against that TSC its rate may be
    # off by over its 2%. Its warning says the TSC is why, not its own reads.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=30000")
    out = boot_messages(m)
    assert re.search(
        r"WARN  time: local APIC timer calibration: (\d) of (\d+) windows read promptly, but the TSC it is "
        r"measured against may be off by up to (\d+\.\d)%, so its rate may be off by up to (\d+\.\d)%, over the "
        r"2\.0% allowed",
        out,
    ), out
    assert "a stall next to the timer's register accesses" not in out, out


def test_tsc_windows_with_no_bound_left_panic(boot):
    # Each window's end stalled twice as long as it ran: the end gap is more
    # than the rest of the window, so the median's bound is over 100% and
    # the rate says nothing. The HPET gives way to the PIT (warned), which
    # stalls too, and boot panics saying why.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=2000000")
    assert m.wait_exit(BOOT_TIMEOUT) == EXIT_CRASH
    out = m.output
    no_information = (
        r"only 0 of 20 windows read promptly \(5 wanted\), and the middle ones' end stalls were as long as the "
        r"rest of their windows, so the rate says nothing"
    )
    assert re.search(
        rf"WARN  time: can't calibrate the tsc against the HPET: {no_information} .*; calibrating against the PIT\n",
        out,
    ), out
    said = panic_message(out)
    assert re.match(rf"can't calibrate the tsc against the PIT: {no_information}", said), said


def test_a_stall_past_ten_windows_is_ignored(boot):
    # Over lumen_time::MOST_STALL_PPM (10 windows' worth): warned about and
    # ignored, so the calibration is a normal one.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=20000000")
    out = boot_messages(m)
    assert (
        "WARN  time: lumen.stall_tsc_windows=20000000: not <end ppm>[,<start ppm>], each at most 10000000; "
        "ignored\n" in out
    ), out
    assert "stalling calibration windows" not in out, out
    assert TSC_LINE.search(out)[3] == "hpet", out


def test_tsc_windows_stalled_without_an_hpet_fill_in_against_the_pit(boot):
    # No HPET: the PIT's windows fill in the same way.
    m = boot(
        "qemutest",
        extra=["-machine", "q35,hpet=off"],
        cmdline="lumen.stall_tsc_windows=6000",
    )
    out = boot_messages(m)
    assert TSC_LINE.search(out)[3] == "pit", out
    said = re.search(r"INFO  time: tsc calibration " + FILLED, out)
    assert said, out
    assert 0.6 <= float(said[1]) <= 1.0, said[0]
    assert "WARN  time" not in out, out


def test_timer_windows_stalled_past_steady_fill_in_within_tolerance(boot):
    # 1% at each timer window's end: none steady, the median within 1% and a
    # bit, inside the timer's 2%: an INFO line, no warning, and the timer
    # ticks (#139: the 0.7% one seen on loaded KVM used to warn).
    m = boot("qemutest", cmdline="lumen.stall_timer_windows=10000")
    out = boot_messages(m)
    said = re.search(r"INFO  time: local APIC timer calibration " + FILLED, out)
    assert said, out
    assert 1.0 <= float(said[1]) <= 1.5, said[0]
    assert "WARN" not in out, out
    m.send_line("sleep 100")
    m.expect(r"slept 100 ms\n")
    m.expect(r"lumen> ")
    assert ticks(m)[0] > 0


def test_timer_windows_stalled_past_tolerance_warn(boot):
    # 3%: past the timer's 2%, a warning with the bound, and the boot goes on.
    m = boot("qemutest", cmdline="lumen.stall_timer_windows=30000")
    out = boot_messages(m)
    said = re.search(
        r"WARN  time: local APIC timer calibration: only 0 of 20 windows read promptly \(5 wanted; a stall "
        r"next to the timer's register accesses\), so its rate may be off by up to (\d+\.\d)% "
        r"\(\d+\.\d% of it the TSC's calibration\), over the 2\.0% allowed",
        out,
    )
    assert said, out
    assert 2.9 <= float(said[1]) <= 3.5, said[0]


def test_timer_windows_with_no_bound_left_panic(boot):
    # From the second window on, each one's start stalled three times as long
    # as the last ran: its gaps outlast it (an end stall alone can't: the
    # window counts it), so the median's bound is gone and boot panics.
    m = boot("qemutest", cmdline="lumen.stall_timer_windows=0,3000000")
    assert m.wait_exit(BOOT_TIMEOUT) == EXIT_CRASH
    said = panic_message(m.output)
    # The first window (no stall) may be steady or not: TCG's timer reads alone
    # can take 1% of a window.
    assert re.match(
        r"can't calibrate the local APIC timer against the TSC: only [01] of 20 windows read promptly "
        r"\(5 wanted\), and the middle ones' stalls were as long as themselves, so the rate says nothing",
        said,
    ), said
