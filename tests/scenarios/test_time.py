"""Timekeeping: the TSC is calibrated at boot, the local APIC timer
interrupts arrive, and the shell's `sleep` waits in guest time.

The arithmetic is host-tested (`cargo test -p lumen-time`); calibration
against a second clock, `time::sleep` and the 1 kHz rate are checked in-kernel
(`lumen_kernel::time::*`, run by test_ktest). Host wall-clock time is never
asserted on: TCG and KVM run at different speeds.
"""

import re
from typing import NamedTuple

from lumen_harness.calibration import (
    TIMER,
    TSC,
    Judged,
    judged,
    no_other_warnings,
    ppm_range,
    verdict,
    weighed,
)
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
#
# A busy host's own stalls add to the injected ones (#171), so these
# scenarios assert only what it can't change (CLAUDE.md, "Tests on a busy
# host"): the windows taken and none steady, a bound at least the stall
# (`stalled`), verdict lines that follow from the numbers they print and no
# other `time` warning (lumen_harness.calibration). Never a ceiling.
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


def stalled(out: str, clock: str, stall_ppm: int) -> Judged:
    """`clock`'s verdict after `stall_ppm` at every window's end, past
    steady: all 20 taken, none steady, and the bound at least the stall,
    whatever the host adds (a floor, never a ceiling). The TSC's bound is at
    least the stall itself (each end gap over the rest of the window); the
    timer's window counts its own end stall, so its share is s / (1 + s):
    0.99% for 1%."""
    said = judged(out, clock)
    assert (said.steady, said.retaken) == (0, 20), said
    floor = stall_ppm if clock == TSC else stall_ppm * 1_000_000 // (1_000_000 + stall_ppm)
    assert ppm_range(str(said.bound))[1] >= floor, (floor, said)
    return said


def timer_follows_tsc(timer: Judged | None, tsc: Judged | None) -> None:
    """The timer's verdict, measured against the TSC whose verdict is `tsc`
    (None: either said nothing): its bound at least the TSC's (`compose_ppm`
    adds the TSC's to its windows', both rounded up), and a warning names the
    TSC's bound as its share."""
    if timer is None or tsc is None:
        return
    assert timer.bound >= tsc.bound, (timer, tsc)
    if timer.warned:
        assert timer.tsc_share == tsc.bound, (timer, tsc)


def test_tsc_windows_stalled_past_steady_fill_in(boot):
    # 0.6% at each window's end: none steady, the median within 0.6% and a
    # bit (rounded up to a tenth): within the TSC's 1%, an INFO line and the
    # HPET kept (an unsteady HPET used to give way to the PIT, which stalls
    # with it), unless the host's own stalls took it past (a warning then, or
    # the PIT kept for a smaller bound). The bound is honest: the stall makes
    # the boot's rate high by about that much, and the HPET measured again
    # (unstalled) agrees within the two bounds.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=6000")
    out = boot_messages(m)
    tsc_said = stalled(out, TSC, 6000)
    w = weighed(out)
    assert TSC_LINE.search(out)[3] == (w.kept if w else "hpet"), out
    if w:
        assert tsc_said.bound == w.kept_bound, (tsc_said, w)
    timer_follows_tsc(verdict(out, TIMER), tsc_said)
    no_other_warnings(out)
    (tsc, _, _, hpet), cals = clocks_and_windows(m)
    boot_ppm, hpet_ppm = median_bound_ppm(cals["boot"]), median_bound_ppm(cals["hpet"])
    assert within_bounds(tsc, hpet, boot_ppm, hpet_ppm), (
        tsc,
        hpet,
        boot_ppm,
        hpet_ppm,
    )
    # The stall moved the rate: by about 0.6%, and the HPET measured again
    # may be off by its own bound, so at least 0.3% less that bound. (A busy
    # host can widen that bound, which only weakens this, never fails it.)
    assert (tsc - hpet) * 1_000_000 >= hpet * (3_000 - hpet_ppm), (
        f"the stall didn't move the rate: {tsc} against {hpet} within {hpet_ppm} ppm"
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
    w = weighed(out)
    assert w and w.pit is not None, (w, out)
    assert TSC_LINE.search(out)[3] == w.kept, out
    tsc_said = stalled(out, TSC, 15000)
    assert tsc_said.warned and tsc_said.bound == w.kept_bound, (tsc_said, w)
    assert f"a stall next to the {w.kept.upper()}'s reads" in tsc_said.line, tsc_said
    timer_follows_tsc(verdict(out, TIMER), tsc_said)
    no_other_warnings(out)
    # The HPET counted, so it stays a second clock whichever was kept.
    (_, reference, pit, hpet), _ = clocks_and_windows(m)
    assert reference == w.kept and pit is not None and hpet is not None, (
        reference,
        pit,
        hpet,
    )


def test_a_timer_off_through_the_tsc_says_so(boot):
    # The TSC stalled 3% (past its 1%: a warning) and the timer not: measured
    # against that TSC, the timer's rate may be off by over its 2%, and its
    # warning names the TSC's bound. Which wording follows from its numbers
    # (`calibration._check`): its own windows within 2%, "but the TSC it is
    # measured against"; past it too, as a busy host can make them, both named.
    m = boot("qemutest", cmdline="lumen.stall_tsc_windows=30000")
    out = boot_messages(m)
    tsc_said = stalled(out, TSC, 30000)
    assert tsc_said.warned, tsc_said
    timer_said = judged(out, TIMER)
    assert timer_said.warned, timer_said
    timer_follows_tsc(timer_said, tsc_said)
    no_other_warnings(out)


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
    tsc_said = stalled(out, TSC, 6000)
    assert weighed(out) is None, out
    timer_follows_tsc(verdict(out, TIMER), tsc_said)
    no_other_warnings(out)


def test_timer_windows_stalled_past_steady_fill_in(boot):
    # 1% at each timer window's end: none steady, the median within 1% and a
    # bit, inside the timer's 2% (#139: the 0.7% one seen on loaded KVM used
    # to warn), so an INFO line, unless the host's own stalls took it past
    # (#171); either way the verdict follows the bound, and the timer ticks.
    m = boot("qemutest", cmdline="lumen.stall_timer_windows=10000")
    out = boot_messages(m)
    timer_said = stalled(out, TIMER, 10000)
    timer_follows_tsc(timer_said, verdict(out, TSC))
    no_other_warnings(out)
    m.send_line("sleep 100")
    m.expect(r"slept 100 ms\n")
    m.expect(r"lumen> ")
    assert ticks(m)[0] > 0


def test_timer_windows_stalled_past_tolerance_warn(boot):
    # 3%: past the timer's 2% whatever the host adds, its own reads to blame,
    # and the boot goes on.
    m = boot("qemutest", cmdline="lumen.stall_timer_windows=30000")
    out = boot_messages(m)
    timer_said = stalled(out, TIMER, 30000)
    assert timer_said.kind == "own_reads", timer_said
    timer_follows_tsc(timer_said, verdict(out, TSC))
    no_other_warnings(out)


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
