"""Parsing crash reports (the format is written by kernel/src/crash.rs and
kernel/src/mm/paging.rs `write_walk`; change them together)."""

from __future__ import annotations

import re
from dataclasses import dataclass

BEGIN = "==== LUMEN CRASH REPORT ===="
END = "==== END CRASH REPORT ===="
# Printed by kmon's `crash wtext` / `crash nx` when the fault didn't happen.
PROTECTION_BROKEN = re.compile(r"crash: .* succeeded \((?:W\^X|NX) broken\)")

_FRAME = re.compile(r"^  #\d+\s+0x[0-9a-f]{16} <(.+)>$", re.MULTILINE)
_WALK_HEAD = re.compile(r"^page walk of (0x[0-9a-f]+) \(cr3 (0x[0-9a-f]+)\):$", re.MULTILINE)
_WALK_STEP = re.compile(r"  L(\d)\[(\d{3})\] (0x[0-9a-f]{16})")


def backtrace(report: str) -> list[str]:
    """The symbol of every backtrace frame, innermost first."""
    return _FRAME.findall(report)


@dataclass
class PageWalk:
    virt: int
    steps: list[tuple[int, int, int]]  # (level, index, raw entry)
    end: str  # the "-> ..." line without the arrow


def page_walk(report: str) -> PageWalk:
    """The page-table walk in a #PF report."""
    head = _WALK_HEAD.search(report)
    if not head:
        raise AssertionError(f"no page walk in the report:\n{report}")
    steps = []
    for line in report[head.end() + 1 :].splitlines():
        step = _WALK_STEP.fullmatch(line)
        if step:
            steps.append((int(step.group(1)), int(step.group(2)), int(step.group(3), 16)))
        elif line.startswith("  -> "):
            return PageWalk(int(head.group(1), 16), steps, line[len("  -> ") :])
        else:
            break
    raise AssertionError(f"page walk has no end line:\n{report}")
