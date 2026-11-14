"""The terminal on the screen (lumen-term below the header): ANSI colours,
the screen's shell, scrollback with Shift+PageUp/PageDown."""

import re
from typing import NamedTuple

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine
from lumen_harness.screen import expect_screen

# 1280x800 (limine.conf) less the header and margins, in 7x16 cells.
TERM_LINE = r"INFO  term: terminal 176x41, 500 lines of scrollback\n"
TERM_STATUS = re.compile(
    r"term: (\d+)x(\d+), cursor \((\d+), (\d+)\)( hidden)?, scrollback (\d+)/500 lines, "
    r'view (\d+) lines up, top row "([^"]*)", (\d+) cells drawn\n'
)
# The terminal's top edge (y) and its cells' height.
TOP, CELL_H = 144, 16


class TermStatus(NamedTuple):
    cols: int
    rows: int
    scrollback: int
    view: int
    top_row: str


def term_status(m: Machine) -> TermStatus:
    """The serial shell's `term`."""
    m.send_line("term")
    found = m.expect(TERM_STATUS, 10)
    m.expect(r"lumen> ", 10)
    cols, rows, _, _, _, scrollback, view, top_row, _ = found.groups()
    return TermStatus(int(cols), int(rows), int(scrollback), int(view), top_row)


def boot_term(boot, cmdline: str | None = None) -> Machine:
    m = boot("qemutest", cmdline=cmdline)
    m.expect(TERM_LINE, 60)
    m.expect(r"INFO  shell: shells on serial and the screen", 60)
    m.expect(r"lumen> ", 60)
    return m


def test_ansi_colours_golden(boot, artifacts, golden_dir):
    # Typed on the keyboard, on a cleared screen: the command line, then the
    # pattern's 11 rows (named colours, text colours, the cube, the grey
    # ramp, attributes, truecolor). The rows after it (`colors: done`, the
    # prompt with the blinking cursor) are left out.
    m = boot_term(boot)
    start = len(m.output)
    m.type_text("clear\n")
    m.type_text("colors\n")
    expect_screen(m, r"^colors: done\nlumen>$")
    shot = m.screendump("colors.png")
    assert_matches_golden(
        shot,
        golden_dir / "term-colors.png",
        origin=(0, TOP),
        size=(800, 12 * CELL_H),
        artifacts=artifacts,
    )
    # The escapes stayed off serial (the firmware's own, before the kernel,
    # don't count).
    assert "\x1b" not in m.output[start:]


# Logged (debug, serial only) when the view moves.
VIEW_MOVED = re.compile(r'DEBUG term: view (\d+) lines up, top row "([^"]*)"\n')


def view_moved(m: Machine) -> tuple[int, str]:
    found = m.expect(VIEW_MOVED, 10)
    return int(found[1]), found[2]


def test_scrollback(boot, artifacts, golden_dir):
    # After `clear` (typed on the keyboard) the screen's lines are, from the
    # first: `lumen> seq 100`, 1 to 100, the prompt. Shift+PageUp scrolls
    # back half a screen per press, Shift+PageDown forward (each move logged
    # at debug level, which only serial shows); a key typed shows the screen
    # again. (That output keeps a scrolled-back view on its lines is the
    # ktest output_keeps_a_scrolled_back_view: the screen's shell, the only
    # thing writing there besides log lines, takes keys only between
    # commands.)
    m = boot_term(boot, cmdline="lumen.log=debug")
    m.type_text("clear\n")
    m.type_text("seq 100\n")
    expect_screen(m, r"^100\nlumen>$")
    before = term_status(m)
    assert before.view == 0, before
    half = before.rows // 2
    # The scrollback's line i is `i` (line 0 the command), and the view's top
    # row is line scrollback - view.
    scrollback = before.scrollback
    qmp = m.qmp()
    qmp.send_key(["shift", "pgup"])
    assert view_moved(m) == (half, str(scrollback - half))
    qmp.send_key(["shift", "pgup"])
    assert view_moved(m) == (2 * half, str(scrollback - 2 * half))
    qmp.send_key(["shift", "pgdn"])
    assert view_moved(m) == (half, str(scrollback - half))
    still = term_status(m)
    assert (still.view, still.top_row) == (half, str(scrollback - half)), still
    shot = m.screendump("scrolled.png")
    # The view shows `seq`'s lines from there on; the cursor (on the
    # screen's last row) is out of it anyway (a ktest checks that a
    # scrolled-back view doesn't draw one it holds).
    assert_matches_golden(
        shot,
        golden_dir / "term-scrollback.png",
        origin=(0, TOP),
        size=(320, before.rows * CELL_H),
        artifacts=artifacts,
    )
    m.press(["spc"])
    assert view_moved(m)[0] == 0
    back = term_status(m)
    assert back.view == 0, back
