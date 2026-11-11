"""Parse the TAP (version 14) stream the in-kernel test runner prints."""

from __future__ import annotations

import re
from dataclasses import dataclass, field

_PLAN = re.compile(r"^1\.\.(\d+)\s*$")
_RESULT = re.compile(r"^(ok|not ok) (\d+)(?: - (.*?))?\s*$")


@dataclass
class TapCase:
    number: int
    name: str
    ok: bool
    diagnostics: list[str] = field(default_factory=list)


@dataclass
class TapReport:
    planned: int | None = None
    cases: list[TapCase] = field(default_factory=list)

    @property
    def passed(self) -> list[TapCase]:
        return [c for c in self.cases if c.ok]

    @property
    def failed(self) -> list[TapCase]:
        return [c for c in self.cases if not c.ok]

    @property
    def complete(self) -> bool:
        """The plan was seen and every planned test reported, in order."""
        return self.planned is not None and [c.number for c in self.cases] == list(
            range(1, self.planned + 1)
        )

    @property
    def all_passed(self) -> bool:
        return self.complete and not self.failed

    def summary(self) -> str:
        lines = [f"planned {self.planned}, ran {len(self.cases)}, failed {len(self.failed)}"]
        for c in self.failed:
            lines.append(f"  not ok {c.number} - {c.name}")
            lines += [f"    {d}" for d in c.diagnostics]
        return "\n".join(lines)


def parse(text: str) -> TapReport:
    """Parse TAP from serial output; non-TAP lines (logs) are ignored.

    Lumen's runner prints a test's diagnostics (`# ...`) *before* its result
    line, because assertions print while the test runs. So `#` lines are held
    and attached to the next result; any after the last result are dropped.
    """
    report = TapReport()
    in_tap = False
    pending: list[str] = []
    for raw in text.splitlines():
        line = raw.rstrip("\r")
        if line.startswith("TAP version"):
            in_tap = True
            continue
        if not in_tap:
            continue
        if m := _PLAN.match(line):
            report.planned = int(m.group(1))
        elif m := _RESULT.match(line):
            case = TapCase(int(m.group(2)), m.group(3) or "", m.group(1) == "ok", pending)
            report.cases.append(case)
            pending = []
        elif line.startswith("#"):
            pending.append(line.lstrip("# ").rstrip())
    return report
