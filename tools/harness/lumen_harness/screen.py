"""Reading the screen's terminal through the serial shell's `term text`
(written by kernel/src/shell/commands.rs `term`; change them together): how
scenarios see what the screen shell, typed on the keyboard, did.

`term text` prints each row of the view after a `|` (trailing blanks
trimmed), then `term text: <n> rows, cursor (<col>, <row>)`.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

SUMMARY = re.compile(r"term text: (\d+) rows, cursor \((\d+), (\d+)\)\n")
PROMPT = r"lumen> "


@dataclass
class Screen:
    rows: list[str]
    cursor: tuple[int, int]  # (col, row)

    @property
    def text(self) -> str:
        return "\n".join(self.rows)

    @property
    def cursor_row(self) -> str:
        return self.rows[self.cursor[1]]


def parse_term_text(out: str, summary: re.Match[str]) -> Screen:
    """The screen from `term text`'s output (`out`: what came before its
    summary line, from the command's echo on)."""
    rows = [line[1:] for line in out.split("\n") if line.startswith("|")]
    n, col, row = map(int, summary.groups())
    if len(rows) != n:
        raise AssertionError(f"term text: {len(rows)} rows read, {n} written:\n{out}")
    return Screen(rows, (col, row))


def read_screen(m: Machine) -> Screen:
    """Run `term text` at the serial shell's prompt; back at the prompt."""
    start = len(m.output)
    m.send_line("term text")
    summary = m.expect(SUMMARY, 10)
    screen = parse_term_text(m.output[start : summary.start()], summary)
    m.expect(PROMPT, 10)
    return screen


def expect_screen(m: Machine, pattern: str, timeout: float = 10) -> re.Match[str]:
    """Read the screen until `pattern` (a regex, `^` and `$` matching at each
    row) matches it; its match. Keys typed on the keyboard arrive in order
    but aren't echoed on serial: this is how a scenario waits for them."""
    rx = re.compile(pattern, re.MULTILINE)
    deadline = time.monotonic() + timeout * m.timeout_scale
    while True:
        screen = read_screen(m)
        if found := rx.search(screen.text):
            return found
        if time.monotonic() > deadline:
            raise AssertionError(f"{pattern!r} not on the screen after {timeout}s:\n{screen.text}")
        time.sleep(0.05)
