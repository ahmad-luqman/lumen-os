"""A boot calibration's verdict line (kernel/src/time.rs's `report`) and the
TSC's HPET-or-PIT choice (`the_steadier`), read back and checked against the
numbers they print.

A busy host stalls the vCPU too, and its stalls add to any injected into the
calibration windows (#171: on a loaded KVM runner each timer register access
took about 45 µs, 0.44% of a window, on top of a 1% end stall: 2.1% with the
TSC's 0.2%, past the timer's 2%). So which line a stalled boot gets is the
host's to change; that the line follows from the numbers it prints is not.
The exact bounds, verdicts and texts for given windows are pinned by
lumen-time's host tests and time.rs's report ktest. Self-tested in
tests/harness/test_calibration.py, on the lines #171's two boots printed
and made-up ones in the kernel's words.
"""

from __future__ import annotations

import re
from typing import NamedTuple

from lumen_harness.clock_diags import TIMER_TOLERANCE_PPM, TSC_TOLERANCE_PPM
from lumen_harness.klog import STAMP

TSC = "tsc"
TIMER = "local APIC timer"
HPET = "hpet"
PIT = "pit"
# Each clock's tolerance in ppm (lumen_time's).
TOLERANCE_PPM = {TSC: TSC_TOLERANCE_PPM, TIMER: TIMER_TOLERANCE_PPM}
# Windows a calibration keeps and takes at most (time::WINDOWS, ATTEMPTS).
WINDOWS = 5
ATTEMPTS = 20

# A verdict's kinds: within the tolerance with windows taken again (INFO);
# past it, a stall next to the clock's own reads (WARN); the timer past it
# only through the TSC's error (WARN).
RETOOK = "retook"
OWN_READS = "own_reads"
THROUGH_TSC = "through_tsc"


def percent(ppm: int) -> str:
    """A bound under 100% as time.rs's `Percent` prints it: rounded up to a
    tenth (from 1,000,000 ppm on it prints "no bound", which no verdict line
    shows: that calibration panics)."""
    tenths = -(-ppm // 1_000)
    return f"{tenths // 10}.{tenths % 10}%"


def ppm_range(printed: float) -> tuple[int, int]:
    """The ppm a `Percent` printed as `printed` (2.1) can be: (low, high],
    as it rounds up to a tenth."""
    tenths = round(printed * 10)
    return (tenths - 1) * 1_000, tenths * 1_000


def reads_of(reference: str) -> str:
    """What a TSC warning says its stall was next to, for `reference`."""
    return f"the {reference.upper()}'s reads"


# The reads a stall is next to (`Calibrated::reads`), by clock.
READS = {TSC: (reads_of(HPET), reads_of(PIT)), TIMER: ("the timer's register accesses",)}


class Judged(NamedTuple):
    """A boot calibration's verdict line (`time::report`)."""

    # The whole line, timestamp and newline included.
    line: str
    # RETOOK, OWN_READS or THROUGH_TSC.
    kind: str
    # The median's bound it prints, in % (rounded up to a tenth).
    bound: float
    steady: int
    # The windows taken again (the unsteady ones).
    retaken: int
    # The share of the bound that is the TSC's calibration, in %, where the
    # line names it (a timer's warning; None otherwise).
    tsc_share: float | None = None
    # What its stall was next to (OWN_READS; None otherwise).
    reads: str | None = None

    @property
    def warned(self) -> bool:
        return self.kind != RETOOK


# Each kind's line; `{name}` and `{tolerance}` filled in per clock.
VERDICT_LINES = {
    RETOOK: (
        r"INFO  time: {name} calibration took (?P<retaken>\d+) windows again, (?P<steady>\d+) of 5 kept "
        r"steady, median within (?P<bound>\d+\.\d)%"
    ),
    OWN_READS: (
        r"WARN  time: {name} calibration: only (?P<steady>\d+) of (?P<taken>\d+) windows read promptly "
        r"\(5 wanted; a stall next to (?P<reads>[^)]*)\), so its rate may be off by up to "
        r"(?P<bound>\d+\.\d)%(?: \((?P<tsc_share>\d+\.\d)% of it the TSC's calibration\))?, over the "
        r"{tolerance} allowed"
    ),
    THROUGH_TSC: (
        r"WARN  time: {name} calibration: (?P<steady>\d+) of (?P<taken>\d+) windows read promptly, but the "
        r"TSC it is measured against may be off by up to (?P<tsc_share>\d+\.\d)%, so its rate may be off by "
        r"up to (?P<bound>\d+\.\d)%, over the {tolerance} allowed"
    ),
}
# Where the TSC's lines start (the HPET-or-PIT choice starts the same way).
WEIGHING_START = "time: tsc calibration: against the HPET"


def verdict(out: str, clock: str) -> Judged | None:
    """`clock`'s (`TSC` or `TIMER`) verdict line in `out`, checked
    (`_check`), or None if it has none: every window steady at once says
    nothing. Any line that starts like one (`<clock> calibration`, the TSC's
    HPET-or-PIT choice aside) must be one, and two are an error."""
    tolerance = TOLERANCE_PPM[clock]
    found = []
    for kind, pattern in VERDICT_LINES.items():
        line = pattern.format(name=re.escape(clock), tolerance=re.escape(percent(tolerance)))
        found += [(kind, m) for m in re.finditer(rf"(?m)^{STAMP} {line}[^\n]*\n", out)]
    starts = [
        line
        for line in out.splitlines()
        if re.match(rf"{STAMP} (?:INFO |WARN ) time: {re.escape(clock)} calibration\b", line)
        and WEIGHING_START not in line
    ]
    assert len(found) == len(starts) <= 1, (
        f"want at most one verdict on the {clock} calibration, and nothing like one: {starts}"
    )
    if not found:
        return None
    kind, m = found[0]
    g = m.groupdict()
    steady = int(g["steady"])
    j = Judged(
        line=m[0],
        kind=kind,
        bound=float(g["bound"]),
        steady=steady,
        retaken=int(g["retaken"]) if "retaken" in g else int(g["taken"]) - steady,
        tsc_share=None if g.get("tsc_share") is None else float(g["tsc_share"]),
        reads=g.get("reads"),
    )
    _check(j, clock)
    return j


def judged(out: str, clock: str) -> Judged:
    """`clock`'s one verdict line in `out` (`verdict`), which must be there."""
    j = verdict(out, clock)
    assert j, f"no verdict on the {clock} calibration: {out}"
    return j


def _check(j: Judged, clock: str) -> None:
    """That `j` follows from the numbers it prints. A warning exactly when its
    bound is past the tolerance: exact on the printed bound, as `Percent`
    rounds up to a tenth, `lumen_time::verdict` compares with `>`, and both
    tolerances are whole tenths. And the wording the kernel picks for it."""
    tolerance = TOLERANCE_PPM[clock]
    low, high = ppm_range(j.bound)
    assert j.warned == (high > tolerance), j
    # Windows: at most 5 steady, at most 20 taken; INFO only after retakes
    # (every window steady at once says nothing).
    assert 0 <= j.steady <= WINDOWS and 0 <= j.retaken <= ATTEMPTS - j.steady, j
    assert j.kind != RETOOK or j.retaken > 0, j
    if j.kind == OWN_READS:
        assert j.reads in READS[clock], j
    if clock == TSC:
        # Only the timer's lines name the TSC.
        assert j.kind != THROUGH_TSC and j.tsc_share is None, j
        return
    if j.kind == RETOOK:
        return
    # A timer's warning names the TSC's share: always some, as the TSC's
    # bound includes a reference tick (`Window::tsc_error_ppm`).
    assert j.tsc_share is not None, j
    # The timer's bound is its windows' own, w, with the TSC's, s, on top
    # (`compose_ppm`: w + s + w*s/1e6, rounded up), so at least the TSC's.
    assert j.bound >= j.tsc_share, j
    # The kernel names the TSC as why ("but the TSC") exactly when w alone is
    # within the tolerance (time.rs's `windows_bound_ppm() <= tolerance`).
    # The printed numbers, low < bound <= high and s_low < s <= s_high, bound
    # w from both sides: w <= bound - s < high - s_low, and
    # w * (1 + s/1e6) > bound - s - 1 >= low - s_high. (The upper side leaves
    # out w*s, so it is loose for large shares, never strict.)
    s_low, s_high = ppm_range(j.tsc_share)
    if j.kind == THROUGH_TSC:
        # Some w <= tolerance with w * (1e6 + s_high) > (low - s_high) * 1e6.
        assert (low - s_high) * 1_000_000 < tolerance * (1_000_000 + s_high), j
    else:
        assert high - s_low > tolerance, j


def stalled(out: str, clock: str, stall_ppm: int, reference: str | None = None) -> Judged:
    """`clock`'s verdict after `stall_ppm` at every window's end, past steady:
    all 20 taken, none steady, and the bound at least the stall, whatever the
    host adds (a floor, never a ceiling). The TSC's bound is at least the
    stall itself (each end gap over the rest of the window); the timer's
    window counts its own end stall, so its share is s / (1 + s): 0.99% for
    1%. A TSC warning names the reads of the `reference` it kept, if given."""
    said = judged(out, clock)
    assert (said.steady, said.retaken) == (0, ATTEMPTS), said
    floor = stall_ppm if clock == TSC else stall_ppm * 1_000_000 // (1_000_000 + stall_ppm)
    assert ppm_range(said.bound)[1] >= floor, (floor, said)
    if reference is not None and said.kind == OWN_READS:
        assert said.reads == reads_of(reference), (reference, said)
    return said


def follows_the_tsc(out: str, tsc_ppm: int) -> None:
    """The verdicts in `out` against the TSC's bound, `tsc_ppm` (`clocks`'
    `within N ppm`, the boot calibration's): the TSC's verdict, if any,
    prints it; the timer's, if any, is at least it (`compose_ppm` adds it to
    its windows' own) and, if a warning, names it as its share."""
    tsc_bound = float(percent(tsc_ppm)[:-1])
    tsc = verdict(out, TSC)
    if tsc:
        assert tsc.bound == tsc_bound, (tsc, tsc_ppm)
    timer = verdict(out, TIMER)
    if timer:
        assert ppm_range(timer.bound)[1] >= tsc_ppm, (timer, tsc_ppm)
        if timer.warned:
            assert timer.tsc_share == tsc_bound, (timer, tsc_ppm)


class Weighed(NamedTuple):
    """The TSC's HPET-or-PIT choice (`the_steadier`): the HPET's median past
    the TSC's tolerance, a PIT calibration weighed against it."""

    line: str
    # Each median's bound in %; the PIT's None if it had none or failed.
    hpet: float
    pit: float | None
    # HPET or PIT.
    kept: str

    @property
    def kept_bound(self) -> float:
        if self.kept == HPET:
            return self.hpet
        assert self.pit is not None, self
        return self.pit


WEIGHED = (
    r"(?P<level>INFO |WARN ) " + WEIGHING_START + r" within (?P<hpet>\d+\.\d)% "
    r"\(past the {tolerance} allowed\), against the PIT (?:within (?P<pit>\d+\.\d)%|with no bound|"
    r"it failed \((?P<failed>[^\n]*)\)): kept the (?P<kept>HPET|PIT)"
)


def weighed(out: str) -> Weighed | None:
    """The TSC's HPET-or-PIT choice in `out`, checked, or None if there was
    none (the HPET within the TSC's tolerance, unusable or absent).
    Whichever median can be off less is kept (equal tenths either way: the
    kernel compares ppm); the HPET if the PIT has no bound, or failed (a
    broken PIT: a warning). Any line that starts like one must be one."""
    starts = [line for line in out.splitlines() if WEIGHING_START in line]
    if not starts:
        return None
    tolerance = re.escape(percent(TOLERANCE_PPM[TSC]))
    found = list(re.finditer(rf"(?m)^{STAMP} {WEIGHED.format(tolerance=tolerance)}\n", out))
    assert len(found) == len(starts) == 1, (starts, [m[0] for m in found])
    m = found[0]
    hpet = float(m["hpet"])
    pit = None if m["pit"] is None else float(m["pit"])
    w = Weighed(m[0], hpet, pit, m["kept"].lower())
    assert ppm_range(hpet)[1] > TOLERANCE_PPM[TSC], w
    assert (m["level"] == "WARN ") == (m["failed"] is not None), w
    if pit is None:
        assert w.kept == HPET, w
    elif pit != hpet:
        assert w.kept == (PIT if pit < hpet else HPET), w
    return w


def no_other_warnings(out: str) -> None:
    """Every `time` warning or error in `out` is a calibration's verdict
    (each checked by `verdict`): a stalled calibration warns about nothing
    else. Other subsystems' warnings are their own scenarios' to judge (a q35
    without an HPET warns in `acpi`)."""
    allowed = {j.line for j in (verdict(out, TSC), verdict(out, TIMER)) if j and j.warned}
    others = [
        line
        for line in out.splitlines(keepends=True)
        if re.match(rf"{STAMP} (?:ERROR|WARN ) time: ", line) and line not in allowed
    ]
    assert not others, (others, out)
