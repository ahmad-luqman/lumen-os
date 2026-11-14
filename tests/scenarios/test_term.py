"""The terminal on the screen (lumen-term below the header): ANSI colours,
kmon on screen, scrollback with Shift+PageUp/PageDown."""

import re
from typing import NamedTuple

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine

TERM_LINE = r"INFO  term: terminal (\d+)x(\d+), 500 lines of scrollback\n"
TERM_STATUS = re.compile(
    r"term: (\d+)x(\d+), cursor \((\d+), (\d+)\)( hidden)?, scrollback (\d+)/500 lines, "
    r'view (\d+) lines up, top row "([^"]*)", (\d+) cells drawn, (\d+) log lines dropped\n'
)
# The terminal's top-left pixel and its cells' height.
TOP, CELL_H = 144, 16


class TermStatus(NamedTuple):
    cols: int
    rows: int
    scrollback: int
    view: int
    top_row: str
    dropped: int


def term_status(m: Machine) -> TermStatus:
    """kmon's `term`."""
    m.send_line("term")
    found = m.expect(TERM_STATUS, 10)
    m.expect(r"kmon> ", 10)
    cols, rows, _, _, _, scrollback, view, top_row, _, dropped = found.groups()
    return TermStatus(int(cols), int(rows), int(scrollback), int(view), top_row, int(dropped))


def kmon(m: Machine, line: str, until: str = "") -> None:
    """Run a kmon command; wait for `until` (regex) and the next prompt."""
    m.send_line(line)
    if until:
        m.expect(until, 30)
    m.expect(r"kmon> ", 30)


def boot_term(boot) -> Machine:
    m = boot("qemutest")
    m.expect(TERM_LINE, 60)
    m.expect(r"kmon> ", 60)
    return m


def test_ansi_colours_golden(boot, artifacts, golden_dir):
    # On a cleared screen: the command line, then the pattern's 11 rows
    # (named colours, text colours, the cube, the grey ramp, attributes,
    # truecolor). The rows after it (`colors: done`, the prompt with the
    # blinking cursor) are left out.
    m = boot_term(boot)
    start = len(m.output)
    kmon(m, "clear")
    kmon(m, "colors", r"colors: done\n")
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
    # After `clear` the screen's lines are, from the first: `kmon> seq 100`,
    # 1 to 100, then prompts and `term`'s answers. Shift+PageUp scrolls back
    # half a screen per press, Shift+PageDown forward (each move logged at
    # debug level, which only serial shows); new output doesn't move the
    # view off the lines it shows (it only counts more lines up); a key
    # typed on the keyboard shows the screen again.
    m = boot("qemutest", cmdline="lumen.log=debug")
    m.expect(TERM_LINE, 60)
    m.expect(r"kmon> ", 60)
    kmon(m, "clear")
    kmon(m, "seq 100", r"\n100\n")
    before = term_status(m)
    assert before.view == 0, before
    half = before.rows // 2
    # `term`'s own line went up after it measured: the scrollback's line i
    # is `i` (line 0 the command), and the view's top row is line
    # scrollback - view.
    scrollback = before.scrollback + 1
    qmp = m.qmp()
    qmp.send_key(["shift", "pgup"])
    assert view_moved(m) == (half, str(scrollback - half))
    qmp.send_key(["shift", "pgup"])
    assert view_moved(m) == (2 * half, str(scrollback - 2 * half))
    # Output while scrolled back: the view stays on its lines.
    kmon(m, "echo more")
    still = term_status(m)
    assert (still.top_row, still.view > 2 * half) == (str(scrollback - 2 * half), True), still
    qmp.send_key(["shift", "pgdn"])
    view, top = view_moved(m)
    assert top == str(scrollback - half), (view, top)
    shot = m.screendump("scrolled.png")
    # The view shows `seq`'s lines from there on; the cursor (on the
    # screen's last rows) is out of it.
    assert_matches_golden(
        shot,
        golden_dir / "term-scrollback.png",
        origin=(0, TOP),
        size=(320, before.rows * CELL_H),
        artifacts=artifacts,
    )
    m.press(["spc"], " ")
    assert view_moved(m)[0] == 0
    back = term_status(m)
    assert (back.view, back.dropped) == (0, 0), back
