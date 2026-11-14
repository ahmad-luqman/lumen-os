"""The terminal on the screen (lumen-term below the header): ANSI colours,
the screen's shell, scrollback with Shift+PageUp/PageDown."""

import re
from typing import NamedTuple

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine
from lumen_harness.screen import expect_screen, read_screen

# 1280x800 (limine.conf) less the header and margins, in 7x16 cells.
TERM_LINE = (
    r"INFO  term: 4 terminals 176x41 \(Alt\+F1 to F4 switch\), 500 lines of scrollback each\n"
)
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
    # `term text` reads the view, and the cursor (on the screen's last row)
    # is below it.
    view = read_screen(m)
    assert view.rows[0] == str(scrollback - half) and view.cursor is None, view
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


# The header's virtual-terminal boxes (`tab_x` in kernel/src/term.rs: four
# 28x24 boxes 6 apart, ending a margin from the right edge, 36 pixels down),
# with a few pixels around them.
TABS_ORIGIN, TABS_SIZE = (1112, 32), (144, 32)


def vt_shown(m: Machine) -> int:
    """The serial shell's `vt`: the virtual terminal shown (from 1)."""
    m.send_line("vt")
    found = m.expect(
        r"vt: terminal (\d) of 4 shown \(Alt\+F1 to F4 switch\), \d+ keys dropped\n", 10
    )
    m.expect(r"lumen> ", 10)
    return int(found[1])


def test_virtual_terminals_are_independent(boot, artifacts, golden_dir):
    # Typed in terminal 1 (on a cleared screen), then Alt+F2 shows terminal
    # 2, its own shell at its first prompt (nothing of 1's), and what is
    # typed next goes there; Alt+F1 shows 1 again with its text intact (the
    # golden: its first two rows, above the blinking cursor's). The header
    # marks the shown one (a golden of its boxes with 2 shown).
    m = boot_term(boot)
    m.type_text("clear\n")
    m.type_text("echo one\n")
    expect_screen(m, r"\Alumen> echo one\none\nlumen>\s*\Z")
    m.press(["alt", "f2"])
    expect_screen(m, r"\Alumen>\s*\Z")
    assert vt_shown(m) == 2
    m.type_text("echo two\n")
    expect_screen(m, r"\Alumen> echo two\ntwo\nlumen>\s*\Z")
    shot = m.screendump("vt2.png")
    assert_matches_golden(
        shot,
        golden_dir / "term-vt-tabs.png",
        origin=TABS_ORIGIN,
        size=TABS_SIZE,
        artifacts=artifacts,
    )
    m.press(["alt", "f1"])
    expect_screen(m, r"\Alumen> echo one\none\nlumen>\s*\Z")
    assert vt_shown(m) == 1
    shot = m.screendump("vt1.png")
    assert_matches_golden(
        shot,
        golden_dir / "term-vt1.png",
        origin=(0, TOP),
        size=(320, 2 * CELL_H),
        artifacts=artifacts,
    )
    # From serial too: terminal 3, untouched, then back to 1. There are
    # four.
    m.send_line("vt 3")
    m.expect(r"vt: terminal 3 shown\n", 10)
    expect_screen(m, r"\Alumen>\s*\Z")
    m.send_line("vt 1")
    m.expect(r"vt: terminal 1 shown\n", 10)
    m.send_line("vt 5")
    m.expect(r"usage: vt \[n\] \(n from 1 to 4\)\n", 10)
    expect_screen(m, r"\Alumen> echo one\none\nlumen>\s*\Z")


def test_a_command_drawing_keeps_its_terminal_shown(boot):
    # While `life` (typed in terminal 1) has the screen, Alt+F2 and `vt 2`
    # don't switch: the key that ends it still reaches it, in terminal 1.
    m = boot_term(boot)
    m.type_text("life\n")
    expect_screen(m, r"^life: gun on 160x80 cells; a key quits$")
    m.press(["alt", "f2"])
    m.send_line("vt 2")
    m.expect(r"vt: not showing terminal 2: a command has the screen\n", 10)
    m.expect(r"lumen> ", 10)
    assert vt_shown(m) == 1
    m.press(["x"])
    expect_screen(m, r"^life: generation \d+, \d+ alive\nlumen>$")
    assert vt_shown(m) == 1
