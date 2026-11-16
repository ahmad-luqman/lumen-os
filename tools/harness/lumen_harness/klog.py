"""The kernel log's lines on serial, as the harness matches them.

The format is kernel/src/klog.rs's (the stamp is lumen_time::Stamp):
`[seconds.micros] LEVEL tag: message` and a newline, the seconds
right-aligned in at least 5 columns, or `[    ?.??????]` before the TSC is
calibrated; the level padded to 5 columns. Change this module with them:
`lumen_harness.shell` takes these lines out of a serial command's output,
and test_boot checks every boot line against a stricter form built from
the same pieces.
"""

from __future__ import annotations

import re

# A calibrated stamp's seconds (5 columns or more, right-aligned) and
# microseconds.
SECONDS = r"(?=[ \d]{5}) *\d+\.\d{6}"
# Before the TSC is calibrated.
UNKNOWN = r"    \?\.\?{6}"
STAMP = rf"\[(?:{SECONDS}|{UNKNOWN})\]"
LEVEL = r"(?:ERROR|WARN |INFO |DEBUG|TRACE)"
# The levels a test should see: they fail a command's output (shell.run).
LOUD = ("ERROR", "WARN ")
# One whole line, newline included, wherever it starts: one can follow part
# of a line on serial (a key's echo, a command's write that didn't end its
# line). No groups, so patterns can repeat it.
LOG_LINE = re.compile(rf"{STAMP} {LEVEL} [^ \n]+: [^\n]*\n")


def level_of(line: str) -> str:
    """A log line's level, as printed (`WARN `, `INFO `, ...)."""
    found = re.match(rf"{STAMP} ({LEVEL}) ", line)
    if not found:
        raise ValueError(f"not a log line: {line!r}")
    return found[1]
