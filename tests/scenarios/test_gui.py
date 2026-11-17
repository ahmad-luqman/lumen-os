"""The screen as the kernel sees it (`screenshot`, #150); the desktop's
scenarios (M4) join it here."""

from __future__ import annotations

from pathlib import Path

from lumen_harness.qemu import Machine
from lumen_harness.screen import Screen, expect_screen, read_screen
from lumen_harness.shell import PROMPT
from PIL import Image, ImageChops, ImageDraw

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
# The terminal's grid on the 1280x800 screen (kernel/src/term.rs): its
# top-left pixel and its 7x16 cells.
GRID_X, GRID_Y, CELL_W, CELL_H = 32, 144, 7, 16


def boot_shells(boot) -> Machine:
    # No tablet: no mouse pointer on the screen.
    m = boot("qemutest")
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def cursor_box(screen: Screen) -> tuple[int, int, int, int]:
    """The pixels of the terminal cursor's cell (left, top, right, bottom,
    exclusive): the cursor blinks every 500 ms, so the screenshot and the
    screendump may each catch it on or off."""
    assert screen.cursor is not None, "the view isn't scrolled back"
    col, row = screen.cursor
    x, y = GRID_X + col * CELL_W, GRID_Y + row * CELL_H
    return (x, y, x + CELL_W, y + CELL_H)


def assert_screenshot_matches_screendump(m: Machine, artifacts: Path, name: str) -> None:
    """`screenshot` over serial equals QEMU's screendump, pixel for pixel,
    except exactly the cursor's cell; the screen (as `term text` reads it)
    is the same before and after both, so nothing else changed meanwhile."""
    before = read_screen(m)
    shot = m.screenshot_via_serial()
    shot.save(artifacts / f"{name}-screenshot.png")
    dump = Image.open(m.screendump(f"{name}-screendump.png")).convert("RGB")
    after = read_screen(m)
    assert (after.rows, after.cursor) == (before.rows, before.cursor), (
        "the screen changed during the capture"
    )
    assert shot.size == dump.size == (1280, 800), (shot.size, dump.size)
    box = cursor_box(before)
    diff = ImageChops.difference(shot, dump)
    # Everything outside the cursor's cell must match exactly: blank the
    # cell in the difference, then nothing may be left.
    outside = diff.copy()
    ImageDraw.Draw(outside).rectangle((box[0], box[1], box[2] - 1, box[3] - 1), fill=(0, 0, 0))
    if outside.getbbox() is not None:
        diff.point(lambda v: 255 if v else 0).save(artifacts / f"{name}-diff.png")
        differing = sum(1 for p in outside.getdata() if p != (0, 0, 0))
        raise AssertionError(
            f"{differing} pixels differ outside the cursor's cell {box}, "
            f"within {outside.getbbox()} (see {name}-diff.png)"
        )
    # The cell itself differs only if the blink caught the cursor in
    # different phases: then both images hold one of the cell's two looks.
    cell = (box[0], box[1], box[2], box[3])
    assert diff.crop(cell).getbbox() is None or shot.crop(cell) != dump.crop(cell)


def test_screenshot_matches_screendump(boot, artifacts):
    # The text console after boot (the header, log lines, the first
    # screen shell's prompt), then a busier screen: the colour test
    # pattern typed on the keyboard (16 and 256 colours, a truecolor
    # gradient).
    m = boot_shells(boot)
    assert_screenshot_matches_screendump(m, artifacts, "boot")
    m.type_text("colors\n")
    expect_screen(m, r"^colors: done\nlumen>$")
    assert_screenshot_matches_screendump(m, artifacts, "colors")


def test_screenshot_refuses_a_screen_shell(boot):
    # On a virtual terminal the PNG's text would only fill the screen: the
    # command says to use serial, and draws no frame.
    m = boot_shells(boot)
    m.type_text("screenshot\n")
    expect_screen(m, r"^screenshot: run it on serial \(the PNG comes out as text\)\nlumen>$")
    assert "@@png" not in read_screen(m).text
