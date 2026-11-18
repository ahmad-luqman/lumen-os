"""Running a command in the serial shell and reading what it printed, with
the kernel log's lines taken out (#124).

Serial carries two streams at once: the serial shell (the echo of what is
typed, a command's output, the prompt) and the kernel log, whose lines any
thread may print at any moment. Each log line is written whole, under the
log's line lock (kernel/src/klog.rs); the shell's writes take that lock
too, each `write!` whole. So a log line can land between two of the
shell's writes: between the prompt and a typed line's echo, inside the
echo (the line editor writes once per key, and Enter's newline on its
own), between two writes of a command's output, or after the output,
before the prompt. The log isn't held while a command prints: a line
shows when it is logged, in stamp order with the others.

`run` takes the log's lines out of what it returns, so a command's output
parses the same whether a log line came along or not, and keeps them on
the result (`Output.logs`), from the prompt before the echo on. A WARN or
ERROR line among them fails the run, unless the caller expects it
(`allow`): it is something a test should see, and before #124 it showed up
as output a test didn't expect. A command whose output *is* log lines
(`dmesg`) is read with `keep_logs`, which skips that check (its lines
can't be told from live ones), or with `run_dmesg`, which tells them apart.

Limits: the serial shell in plain text only (`lumen.ansi`'s editor
redraws lines with escapes, which the echo pattern doesn't follow); a log
message with a newline in it leaves its continuation lines in the output;
`Machine.output` interleaves QEMU's stderr with serial by chunk, so a
QEMU message can land inside a log line (as can the crash reporter's
writes, which take no lock), and that line is then not recognised.
"""

from __future__ import annotations

import re
import time
from typing import TYPE_CHECKING, Self

from lumen_harness.klog import LOG_LINE, LOUD, STAMP, is_log_line, level_of
from lumen_harness.qemu import ExpectTimeout

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

PROMPT = "lumen> "


class Output(str):
    """A command's output; `logs` are the log lines taken out of it (and
    out of its echo, and from the prompt before it), in order, without their
    newlines; None when they were left in (`keep_logs`)."""

    logs: list[str] | None

    def __new__(cls, text: str, logs: list[str] | None = None) -> Self:
        out = super().__new__(cls, text)
        out.logs = logs
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


def allowed(log: str, allow: str | None) -> bool:
    """Whether log line `log` matches `allow`, a regex matched from the
    level on (`WARN  mouse: ...`)."""
    return allow is not None and re.match(allow, re.sub(rf"^{STAMP} ", "", log)) is not None


def check_quiet(line: str, logs: list[str], allow: str | None, text: str) -> None:
    """Fail if a WARN or ERROR among `logs` (which came with `line`'s output
    `text`) isn't `allow`ed."""
    loud = [log for log in logs if level_of(log) in LOUD and not allowed(log, allow)]
    if loud:
        shown = "\n".join(loud)
        raise AssertionError(
            f"{line!r}: a warning or error came with its output (allow= it if expected):\n"
            f"{shown}\n--- output ---\n{text}"
        )


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
    WARN or ERROR line came with it, from the prompt before its echo on,
    that doesn't match `allow`; `keep_logs` skips that check."""
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
    if keep_logs and allow is not None:
        raise ValueError("keep_logs leaves the log lines in: nothing to allow")
    echo, end = _echo_and_prompt(m, line, timeout)
    output = m.output
    out = output[echo.end() : end.start()]
    if keep_logs:
        return Output(out)
    text, logs = split_logs(out)
    logs = _lines_before_output(output, echo) + logs
    check_quiet(line, logs, allow, text)
    return Output(text, logs)


def _echo_and_prompt(m: Machine, line: str, timeout: float) -> tuple[re.Match[str], re.Match[str]]:
    """Wait for `line`'s echo (up to 10 s, or `timeout` if shorter), then
    the prompt after it (`timeout` s)."""
    try:
        echo = m.expect(echo_pattern(line), min(10, timeout))
    except ExpectTimeout as e:
        tail = m.output[-2000:]
        raise ExpectTimeout(f"no echo of {line!r}\n--- last serial output ---\n{tail}") from e
    return echo, m.expect(PROMPT, timeout)


def run_dmesg(m: Machine, line: str = "dmesg", *, allow: str | None = None) -> list[str]:
    """Run `line` (`dmesg`, perhaps piped): its lines, less any log line
    logged while it ran, wherever that landed. dmesg copies lines serial
    printed already (up to its echo's end: it runs after Enter), so a log
    line in its output that serial didn't print before is a live one. A
    live WARN or ERROR, or one between the prompt and the end of the echo,
    fails the run unless `allow`ed, as in `run`."""
    m.send_line(line)
    echo, end = _echo_and_prompt(m, line, 10)
    output = m.output
    before = output[: echo.end()]
    lines, live = [], []
    for text in output[echo.end() : end.start()].splitlines():
        if is_log_line(text) and text + "\n" not in before:
            live.append(text)
        else:
            lines.append(text)
    check_quiet(line, _lines_before_output(output, echo) + live, allow, "\n".join(lines))
    return lines


def _lines_before_output(output: str, echo: re.Match[str]) -> list[str]:
    """The log lines from the prompt `echo` follows (not earlier: boot's
    lines stay out) to the end of the echo."""
    prompt = output.rfind(PROMPT, 0, echo.start())
    start = prompt + len(PROMPT) if prompt >= 0 else echo.start()
    return split_logs(output[start : echo.start()])[1] + split_logs(echo[0])[1]


def wait_for_output(m: Machine, start: int, pattern: str, timeout: float = 10) -> re.Match[str]:
    """Wait until `pattern` (a regex) matches serial's output from `start`
    on (up to `timeout` seconds, scaled; lookbehinds see before `start`),
    without moving `Machine.expect`'s place; its match."""
    rx = re.compile(pattern)
    deadline = time.monotonic() + timeout * m.timeout_scale
    while not (found := rx.search(m.output, start)):
        if time.monotonic() > deadline:
            raise AssertionError(f"no {pattern!r} in {m.output[start:]!r}")
        time.sleep(0.05)
    return found


# The heap's one-line summary (the shell's `heap`, crash reports), written by
# `Snapshot::write` in kernel/src/mm/heap.rs. This matches only the plain
# form with an intact free list: a crash with the heap's lock held labels it
# "heap (lock held; may be mid-update)", and a failed walk replaces the
# "in N blocks" part.
HEAP_LINE = re.compile(
    r"^heap: (?P<mapped>\d+) bytes mapped at (?P<base>0x[0-9a-f]+), (?P<used>\d+) used in "
    r"(?P<live>\d+) allocations, (?P<free>\d+) free in (?P<blocks>\d+) blocks \(largest (?P<largest>\d+)\)"
    r"(?:; last growth failed for (?P<fail_size>\d+) bytes \(align (?P<fail_align>\d+)\): (?P<error>.+))?$",
    re.MULTILINE,
)


def heap_line(text: str) -> dict[str, str]:
    """The fields of the heap line in `text` (the shell's `heap`, a crash
    report)."""
    m = HEAP_LINE.search(text)
    assert m, f"no heap line in:\n{text}"
    return m.groupdict()
