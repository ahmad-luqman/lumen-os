"""A boot calibration's verdict line (kernel/src/time.rs's `report`), read
back and checked against the bound it prints.

A busy host stalls the vCPU too, and its stalls add to any injected into the
calibration windows (#171: on a loaded KVM runner each timer register access
took about 45 µs, 0.44% of a window at each end, on top of a 1% stall: 2.1%
with the TSC's 0.2%, past the timer's 2%). So which line a stalled boot gets
is the host's to change; that the line follows from its own bound is not.
The exact bounds, verdicts and texts for given windows are pinned by
lumen-time's host tests and time.rs's report ktest. Self-tested in
tests/harness/test_calibration.py, on the lines #171's run printed.
"""

from __future__ import annotations

import re
from typing import NamedTuple

from lumen_harness.clock_diags import TIMER_TOLERANCE_PPM, TSC_TOLERANCE_PPM

# Each clock's tolerance in %, as the verdict lines print it.
TSC_TOLERANCE = TSC_TOLERANCE_PPM / 10_000
TIMER_TOLERANCE = TIMER_TOLERANCE_PPM / 10_000
TIMER = "local APIC timer"


class Judged(NamedTuple):
    """A boot calibration's verdict line (`time::report`)."""

    line: str
    warned: bool
    # The median's bound it prints, in % (rounded up to a tenth).
    bound: float
    steady: int
    # The windows taken again (the unsteady ones).
    retaken: int
    # The share of the bound that is the TSC's calibration, in % (the timer's
    # warnings name it; None where the line doesn't).
    tsc_share: float | None


JUDGED = (
    # Within the tolerance, windows taken again: INFO.
    (
        False,
        (
            r"INFO  time: {name} calibration took (?P<retaken>\d+) windows again, (?P<steady>\d+) of 5 kept "
            r"steady, median within (?P<bound>\d+\.\d)%"
        ),
    ),
    # Past it, the clock's own reads to blame (the TSC's share named for the
    # timer).
    (
        True,
        (
            r"WARN  time: {name} calibration: only (?P<steady>\d+) of (?P<taken>\d+) windows read promptly "
            r"\(5 wanted; a stall next to [^)]*\), so its rate may be off by up to (?P<bound>\d+\.\d)%"
            r"(?: \((?P<tsc_share>\d+\.\d)% of it the TSC's calibration\))?, over the {tolerance:.1f}% allowed"
        ),
    ),
    # The timer past it only through the TSC's error.
    (
        True,
        (
            r"WARN  time: {name} calibration: (?P<steady>\d+) of (?P<taken>\d+) windows read promptly, but the "
            r"TSC it is measured against may be off by up to (?P<tsc_share>\d+\.\d)%, so its rate may be off by "
            r"up to (?P<bound>\d+\.\d)%, over the {tolerance:.1f}% allowed"
        ),
    ),
)


def judged(out: str, name: str, tolerance: float) -> Judged:
    """The boot's one verdict line on the `name` calibration (`tsc`, or
    `TIMER`) in `out`, checked against the bound it prints: a warning exactly
    when that bound is past `tolerance` (%). Which line a stalled boot gets is
    the host's to change (its stalls add to an injected one); that the line
    follows from its own bound is not."""
    found = []
    for warned, pattern in JUDGED:
        # The whole line, its timestamp too, as `no_other_warnings` compares it.
        rx = re.compile(
            r"(?m)^[^\n]*?" + pattern.format(name=re.escape(name), tolerance=tolerance) + r".*\n"
        )
        found += [(warned, m) for m in rx.finditer(out)]
    assert len(found) == 1, (
        f"want one verdict on the {name} calibration, got {[m[0] for _, m in found]}: {out}"
    )
    warned, m = found[0]
    steady = int(m["steady"])
    retaken = int(m["retaken"]) if "retaken" in m.re.groupindex else int(m["taken"]) - steady
    share = m.groupdict().get("tsc_share")
    j = Judged(
        m[0], warned, float(m["bound"]), steady, retaken, None if share is None else float(share)
    )
    assert j.warned == (j.bound > tolerance), j
    return j


def timer_verdicts(out: str) -> list[Judged]:
    """The timer calibration's verdict line, checked by `judged`, if it has
    one (every window steady at once says nothing)."""
    return [judged(out, TIMER, TIMER_TOLERANCE)] if f"time: {TIMER} calibration" in out else []


def tsc_verdicts(out: str) -> list[Judged]:
    """The TSC calibration's verdict line, the same way."""
    return [judged(out, "tsc", TSC_TOLERANCE)] if "time: tsc calibration" in out else []


def no_other_warnings(out: str, *verdicts: Judged) -> None:
    """Every `time` warning in `out` is one of `verdicts` (each checked by
    `judged` already): a stalled calibration warns about nothing else. Other
    subsystems' warnings are their own scenarios' to judge (a q35 without an
    HPET warns in `acpi`)."""
    allowed = {j.line for j in verdicts if j.warned}
    others = [
        line + "\n"
        for line in out.splitlines()
        if "WARN  time:" in line and line + "\n" not in allowed
    ]
    assert not others, (others, out)


def kept_reference(out: str) -> str:
    """The reference the TSC's calibration kept (`hpet` or `pit`): the HPET,
    unless its median was past the TSC's tolerance, when the PIT is tried too
    and whichever's median can be off less is kept, said at INFO."""
    chose = re.search(
        r"INFO  time: tsc calibration: against the HPET within (\d+\.\d)% \(past the 1\.0% allowed\), "
        r"against the PIT within (\d+\.\d)%: kept the (HPET|PIT)\n",
        out,
    )
    if not chose:
        return "hpet"
    hpet_pc, pit_pc, kept = float(chose[1]), float(chose[2]), chose[3]
    assert hpet_pc > TSC_TOLERANCE, chose[0]
    assert kept == ("PIT" if pit_pc < hpet_pc else "HPET") or hpet_pc == pit_pc, chose[0]
    return kept.lower()
