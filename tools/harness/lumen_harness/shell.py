"""Running a command in the serial shell and reading what it printed, with
the kernel log's lines taken out (#124).

Serial carries two streams at once: the serial shell (the echo of what is
typed, a command's output, the prompt) and the kernel log, whose lines any
thread may print at any moment. Each log line is written whole, under the
log's line lock (kernel/src/klog.rs); the shell's writes take that lock
too, each `write!` whole. So a log line can land between two of the
shell's writes: before a typed line's echo, inside it (the line editor
writes once per key, and Enter's newline on its own), between two writes
of a command's output, or after the output, before the prompt. The log
isn't held while a command prints: a line shows when it is logged, in
stamp order with the others.

`run` takes the log's lines out of what it returns, so a command's output
parses the same whether a log line came along or not, and keeps them on
the result (`Output.logs`). A WARN or ERROR line among them fails the run,
unless the caller expects it (`allow`): it is something a test should
see, and before #124 it showed up as output a test didn't expect. A
command whose output *is* log lines (`dmesg`) is read with `keep_logs`:
its lines can't be told from live ones.

Limits: the serial shell in plain text only (`lumen.ansi`'s editor
redraws lines with escapes, which the echo pattern doesn't follow); a log
message with a newline in it leaves its continuation lines in the output;
`Machine.output` interleaves QEMU's stderr with serial by chunk, so a
QEMU message can land inside a log line (as can the crash reporter's
writes, which take no lock), and that line is then not recognised.
"""

from __future__ import annotations

import re
from typing import TYPE_CHECKING, Self

from lumen_harness.klog import LOG_LINE, LOUD, level_of

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

PROMPT = "lumen> "


class Output(str):
    """A command's output; `logs` are the log lines taken out of it (and
    out of its echo), in order, without their newlines."""

    logs: list[str]

    def __new__(cls, text: str, logs: list[str] | None = None) -> Self:
        out = super().__new__(cls, text)
        out.logs = list(logs or [])
        return out


def split_logs(text: str) -> tuple[str, list[str]]:
    """`text` without the log lines in it, and those lines (without their
    newlines), in order."""
    logs = [found[0][:-1] for found in LOG_LINE.finditer(text)]
    return LOG_LINE.sub("", text), logs


def echo_pattern(line: str) -> str:
    """A regex for `line`'s echo and the newline after it: right after the
    prompt or a newline (a log line that landed between them), with only
    log lines between its characters (and before its newline)."""
    gap = f"(?:{LOG_LINE.pattern})*"
    anchor = rf"(?:(?<={re.escape(PROMPT)})|(?<=\n))"
    return anchor + gap.join(re.escape(c) for c in line + "\n")


def run(
    m: Machine,
    line: str,
    timeout: float = 10,
    *,
    keep_logs: bool = False,
    allow: str | None = None,
) -> Output:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt (waiting up to `timeout` seconds for the prompt), the
    log's lines taken out unless `keep_logs`. Raises AssertionError if a
    WARN or ERROR line came with it that doesn't match `allow` (a regex,
    searched in the line)."""
    m.send_line(line)
    return read_output(m, line, timeout, keep_logs=keep_logs, allow=allow)


def read_output(
    m: Machine,
    line: str,
    timeout: float = 10,
    *,
    keep_logs: bool = False,
    allow: str | None = None,
) -> Output:
    """`run`'s reading, for a `line` already sent (in pieces, say)."""
    from lumen_harness.qemu import ExpectTimeout

    try:
        echo = m.expect(echo_pattern(line), 10)
    except ExpectTimeout as e:
        tail = m.output[-2000:]
        raise ExpectTimeout(f"no echo of {line!r}\n--- last serial output ---\n{tail}") from e
    end = m.expect(PROMPT, timeout)
    out = m.output[echo.end() : end.start()]
    if keep_logs:
        return Output(out)
    text, logs = split_logs(out)
    logs = split_logs(echo[0])[1] + logs
    loud = [log for log in logs if level_of(log) in LOUD and not (allow and re.search(allow, log))]
    if loud:
        shown = "\n".join(loud)
        raise AssertionError(
            f"{line!r}: a warning or error came with its output (allow= it if expected):\n"
            f"{shown}\n--- output ---\n{text}"
        )
    return Output(text, logs)
