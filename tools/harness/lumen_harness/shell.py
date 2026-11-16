"""Running a command in the serial shell and reading what it printed, with
the kernel log's lines taken out.

Serial carries two streams at once: the serial shell (the echo of what is
typed, a command's output, the prompt) and the kernel log, whose lines any
thread may print at any moment (kernel/src/klog.rs). Each log line is
written whole, holding the log's line lock, and so is each of the shell's
writes, so neither splits the other's write. But a log line can land
between two of the shell's writes: inside a typed line's echo (the line
editor writes once per key), between two writes of a command's output, or
after the output, before the prompt. The log isn't held while a command
prints: a line shows when it is logged, in stamp order with the others
(#124 chose this over holding it). So `run` takes the log's lines out of
what it returns, and a command's output parses the same whether a log line
came along or not; a test that wants the log reads `Machine.output`.

A log line is `[seconds.micros] LEVEL tag: message` and its newline, or
`[    ?.??????]` before the TSC is calibrated (klog.rs, lumen_time::Stamp):
change `LOG_LINE` with them. A command whose output *is* log lines
(`dmesg`) is read with `keep_logs=True`: its lines can't be told from
live ones.
"""

from __future__ import annotations

import re
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

PROMPT = "lumen> "
# The seconds are right-aligned in at least 5 columns. Matched wherever it
# starts: one can follow part of a line (a key's echo, a command's write
# that didn't end its line).
LOG_LINE = re.compile(
    r"\[(?:(?=[ \d]{5}) *\d+\.\d{6}|    \?\.\?{6})\] (?:ERROR|WARN |INFO |DEBUG|TRACE) [^ \n]+: [^\n]*\n"
)


def split_logs(text: str) -> tuple[str, list[str]]:
    """`text` without the log lines in it, and those lines (without their
    newlines), in order."""
    logs = [found[0][:-1] for found in LOG_LINE.finditer(text)]
    return LOG_LINE.sub("", text), logs


def echo_pattern(line: str) -> str:
    """A regex for `line`'s echo and the newline after it, with any log
    lines between its characters."""
    gap = f"(?:{LOG_LINE.pattern})*"
    return gap.join(re.escape(c) for c in line + "\n")


def run(m: Machine, line: str, timeout: float = 10, *, keep_logs: bool = False) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt (waiting up to `timeout` seconds for the prompt), the
    log's lines taken out unless `keep_logs`."""
    m.send_line(line)
    return read_output(m, line, timeout, keep_logs=keep_logs)


def read_output(m: Machine, line: str, timeout: float = 10, *, keep_logs: bool = False) -> str:
    """`run`'s reading, for a `line` already sent (in pieces, say)."""
    echo = m.expect(echo_pattern(line), 10)
    end = m.expect(PROMPT, timeout)
    out = m.output[echo.end() : end.start()]
    return out if keep_logs else split_logs(out)[0]
