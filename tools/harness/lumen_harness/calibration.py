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
tests/harness/test_calibration.py, on the lines #171's two boots printed.
"""

from __future__ import annotations

import re
from fractions import Fraction
from typing import NamedTuple

from lumen_harness.clock_diags import TIMER_TOLERANCE_PPM, TSC_TOLERANCE_PPM
from lumen_harness.klog import LOUD, STAMP, level_of

TSC = "tsc"
TIMER = "local APIC timer"
# Each clock's tolerance in ppm (lumen_time's).
TOLERANCE_PPM = {TSC: TSC_TOLERANCE_PPM, TIMER: TIMER_TOLERANCE_PPM}
# The reads a stall is next to (`Calibrated::reads`), by clock.
READS = {TSC: ("the HPET's reads", "the PIT's reads"), TIMER: ("the timer's register accesses",)}


def percent(ppm: int) -> str:
    """A bound as time.rs's `Percent` prints it: rounded up to a tenth."""
    tenths = -(-ppm // 1_000)
    return f"{tenths // 10}.{tenths % 10}%"


def ppm_range(printed: str) -> tuple[int, int]:
    """The ppm a `Percent` printed as `printed` (`2.1`) can be: (low, high],
    as it rounds up to a tenth."""
    tenths = round(float(printed) * 10)
    return (tenths - 1) * 1_000, tenths * 1_000


class Judged(NamedTuple):
    """A boot calibration's verdict line (`time::report`)."""

    # The whole line, timestamp and newline included.
    line: str
    # `retook` (within the tolerance, windows taken again: INFO), `own_reads`
    # (past it, a stall next to the clock's own reads: WARN) or
    # `through_tsc` (the timer past it only through the TSC's error: WARN).
    kind: str
    # The median's bound it prints, in % (rounded up to a tenth).
    bound: float
    steady: int
    # The windows taken again (the unsteady ones).
    retaken: int
    # The share of the bound that is the TSC's calibration, in %, where the
    # line names it (a timer's warning; None otherwise).
    tsc_share: float | None

    @property
    def warned(self) -> bool:
        return self.kind != "retook"


# Each kind's line; `{name}` and `{tolerance}` filled in per clock.
VERDICT_LINES = {
    "retook": (
        r"INFO  time: {name} calibration took (?P<retaken>\d+) windows again, (?P<steady>\d+) of 5 kept "
        r"steady, median within (?P<bound>\d+\.\d)%"
    ),
    "own_reads": (
        r"WARN  time: {name} calibration: only (?P<steady>\d+) of (?P<taken>\d+) windows read promptly "
        r"\(5 wanted; a stall next to (?P<reads>[^)]*)\), so its rate may be off by up to "
        r"(?P<bound>\d+\.\d)%(?: \((?P<tsc_share>\d+\.\d)% of it the TSC's calibration\))?, over the "
        r"{tolerance} allowed"
    ),
    "through_tsc": (
        r"WARN  time: {name} calibration: (?P<steady>\d+) of (?P<taken>\d+) windows read promptly, but the "
        r"TSC it is measured against may be off by up to (?P<tsc_share>\d+\.\d)%, so its rate may be off by "
        r"up to (?P<bound>\d+\.\d)%, over the {tolerance} allowed"
    ),
}


def verdict(out: str, clock: str) -> Judged | None:
    """`clock`'s (`TSC` or `TIMER`) verdict line in `out`, checked
    (`_check`), or None if it has none (every window steady at once says
    nothing). Two are an error."""
    tolerance = TOLERANCE_PPM[clock]
    found = []
    for kind, pattern in VERDICT_LINES.items():
        line = pattern.format(name=re.escape(clock), tolerance=re.escape(percent(tolerance)))
        found += [(kind, m) for m in re.finditer(rf"(?m)^{STAMP} {line}[^\n]*\n", out)]
    assert len(found) <= 1, f"two verdicts on the {clock} calibration: {[m[0] for _, m in found]}"
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
    )
    _check(j, clock, g.get("reads"))
    return j


def judged(out: str, clock: str) -> Judged:
    """`clock`'s one verdict line in `out` (`verdict`), which must be there."""
    j = verdict(out, clock)
    assert j, f"no verdict on the {clock} calibration: {out}"
    return j


def _check(j: Judged, clock: str, reads: str | None) -> None:
    """That `j` follows from the numbers it prints. A warning exactly when its
    bound is past the tolerance: exact on the printed bound, as `Percent`
    rounds up to a tenth, `lumen_time::verdict` compares with `>`, and both
    tolerances are whole tenths. And the wording the kernel picks for it."""
    tolerance = TOLERANCE_PPM[clock]
    low, high = ppm_range(str(j.bound))
    assert j.warned == (high > tolerance), j
    if clock == TSC:
        # Only the timer's lines name the TSC.
        assert j.kind != "through_tsc" and j.tsc_share is None, j
    if j.kind == "own_reads":
        assert reads in READS[clock], (reads, j)
    if j.tsc_share is None:
        return
    # The timer's bound is its windows' own, w, with the TSC's, s, on top
    # (`compose_ppm`: w + s + w*s/1e6, rounded up), so at least the TSC's.
    s_low, s_high = ppm_range(str(j.tsc_share))
    assert j.bound >= j.tsc_share, j
    # The kernel names the TSC as why ("but the TSC") exactly when w alone is
    # within the tolerance (time.rs's `windows_bound_ppm() <= tolerance`).
    # The printed numbers bound w: w <= bound - s, and w >= (bound - s) / (1 + s).
    w_most = high - s_low
    w_least = Fraction(low - s_high, 1) / (1 + Fraction(s_high, 1_000_000))
    if j.kind == "through_tsc":
        assert w_least <= tolerance, (float(w_least), j)
    else:
        assert w_most > tolerance, (w_most, j)


class Weighed(NamedTuple):
    """The TSC's HPET-or-PIT choice (`the_steadier`): the HPET's median past
    the TSC's tolerance, a PIT calibration weighed against it."""

    line: str
    # Each median's bound in %; the PIT's None if it had none or failed.
    hpet: float
    pit: float | None
    # `hpet` or `pit`.
    kept: str

    @property
    def kept_bound(self) -> float:
        return self.pit if self.kept == "pit" else self.hpet


WEIGHED = (
    r"(?P<level>INFO |WARN ) time: tsc calibration: against the HPET within (?P<hpet>\d+\.\d)% "
    r"\(past the {tolerance} allowed\), against the PIT (?:within (?P<pit>\d+\.\d)%|with no bound|"
    r"it failed \((?P<failed>[^\n]*)\)): kept the (?P<kept>HPET|PIT)"
)


def weighed(out: str) -> Weighed | None:
    """The TSC's HPET-or-PIT choice in `out`, checked, or None if there was
    none (the HPET within the TSC's tolerance, or no HPET). Whichever median
    can be off less is kept (equal tenths either way: the kernel compares
    ppm); the HPET if the PIT has no bound, or failed (a broken PIT: a
    warning). Any line that starts like one must be one."""
    starts = [
        line for line in out.splitlines() if "time: tsc calibration: against the HPET" in line
    ]
    if not starts:
        return None
    tolerance = re.escape(percent(TOLERANCE_PPM[TSC]))
    found = list(re.finditer(rf"(?m)^{STAMP} {WEIGHED.format(tolerance=tolerance)}\n", out))
    assert len(found) == len(starts) == 1, (starts, [m[0] for m in found])
    m = found[0]
    hpet = float(m["hpet"])
    pit = None if m["pit"] is None else float(m["pit"])
    w = Weighed(m[0], hpet, pit, m["kept"].lower())
    assert ppm_range(m["hpet"])[1] > TOLERANCE_PPM[TSC], w
    assert (m["level"] == "WARN ") == (m["failed"] is not None), w
    if pit is None:
        assert w.kept == "hpet", w
    else:
        assert w.kept == ("pit" if pit < hpet else "hpet") or pit == hpet, w
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
        if re.match(rf"{STAMP} (?:ERROR|WARN ) time: ", line)
        and level_of(line) in LOUD
        and line not in allowed
    ]
    assert not others, (others, out)
