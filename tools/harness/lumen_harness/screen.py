"""Reading the shown virtual terminal through the serial shell's `term
text` (written by kernel/src/shell/commands.rs `term`; change them
together): how scenarios see what the screen shells, typed on the keyboard,
did. `term text <window>` reads a terminal window's (#155) the same way
(`read_window`, `expect_window`).

`term text` prints each row of the view after a `|` (trailing blanks
trimmed), then `term text: <n> rows, cursor (<col>, <row>)` (the row
counted in the view, the column in cells), or `term text: <n> rows, cursor
below the view` when the view is scrolled back past it.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

from lumen_harness.shell import PROMPT

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

# The terminal's grid on the 1280x800 screen (kernel/src/term.rs, limine.conf):
# its top-left pixel and its cells' size.
GRID_X, GRID_Y, CELL_W, CELL_H = 32, 144, 7, 16

SUMMARY = re.compile(
    r"^term text: (\d+) rows, cursor (?:\((\d+), (\d+)\)|below the view)\n", re.MULTILINE
)


@dataclass
class Screen:
    rows: list[str]
    cursor: tuple[int, int] | None  # (col, row in the view); None below the view

    @property
    def text(self) -> str:
        return "\n".join(self.rows)


def cursor_box(screen: Screen) -> tuple[int, int, int, int]:
    """The pixels of the cursor's cell as a PIL box (left, top, right,
    bottom; the last two exclusive). The cursor blinks every 500 ms, so two
    screenshots may each catch it on or off there."""
    if screen.cursor is None:
        raise AssertionError("the cursor is below the view (scrolled back)")
    col, row = screen.cursor
    x, y = GRID_X + col * CELL_W, GRID_Y + row * CELL_H
    return (x, y, x + CELL_W, y + CELL_H)


def parse_term_text(out: str, summary: re.Match[str]) -> Screen:
    """The screen from `term text`'s output (`out`: what came before its
    summary line, from the command's echo on)."""
    rows = [line[1:] for line in out.split("\n") if line.startswith("|")]
    n, col, row = summary.groups()
    if len(rows) != int(n):
        raise AssertionError(f"term text: {len(rows)} rows read, {n} written:\n{out}")
    return Screen(rows, None if col is None else (int(col), int(row)))


def read_screen(m: Machine, window: int | None = None) -> Screen:
    """Run `term text` (or `term text <window>`: a terminal window's) at the
    serial shell's prompt; back at the prompt."""
    start = len(m.output)
    m.send_line("term text" if window is None else f"term text {window}")
    summary = m.expect(SUMMARY, 10)
    screen = parse_term_text(m.output[start : summary.start()], summary)
    m.expect(PROMPT, 10)
    return screen


def read_window(m: Machine, window: int) -> Screen:
    """Terminal window `window`'s view (`term text <window>`)."""
    return read_screen(m, window)


def expect_screen(
    m: Machine, pattern: str, timeout: float = 10, window: int | None = None
) -> re.Match[str]:
    """Read the screen until `pattern` (a regex, `^` and `$` matching at each
    row) matches it; its match. Keys typed on the keyboard arrive in order
    but aren't echoed on serial: this is how a scenario waits for them."""
    rx = re.compile(pattern, re.MULTILINE)
    deadline = time.monotonic() + timeout * m.timeout_scale
    where = "the screen" if window is None else f"terminal window {window}"
    while True:
        screen = read_screen(m, window)
        if found := rx.search(screen.text):
            return found
        if time.monotonic() > deadline:
            raise AssertionError(f"{pattern!r} not in {where} after {timeout}s:\n{screen.text}")
        time.sleep(0.05)


def expect_window(m: Machine, window: int, pattern: str, timeout: float = 10) -> re.Match[str]:
    """`expect_screen` on terminal window `window` (`term text <window>`)."""
    return expect_screen(m, pattern, timeout, window)
