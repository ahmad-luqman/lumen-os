"""The screen as the kernel sees it (`screenshot`, #150) and the desktop
(#153: the compositor, its windows, the pointer on it, leaving and
resuming it). ARCHITECTURE section 11."""

from __future__ import annotations

import re
import time

import pytest
from lumen_harness import gui
from lumen_harness.image import assert_matches_golden, compare
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import expect_screen, read_screen
from lumen_harness.screenshot import FrameError, assert_matches_screendump
from lumen_harness.shell import PROMPT, run
from PIL import Image

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
PAUSED = ", paused (a command is drawing)"


def boot_shells(boot) -> Machine:
    # No tablet: no mouse pointer on the screen.
    m = boot("qemutest")
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def test_screenshot_matches_screendump(boot, artifacts):
    # The text console after boot (the header, log lines, the first
    # screen shell's prompt), then a busier screen: the colour test
    # pattern typed on the keyboard (16 and 256 colours, a truecolor
    # gradient). Every pixel must equal QEMU's screendump
    # (assert_matches_screendump: retaken until the cursor's blink phase
    # matches).
    m = boot_shells(boot)
    assert_matches_screendump(m, artifacts, "boot")
    m.type_text("colors\n")
    expect_screen(m, r"^colors: done\nlumen>$")
    assert_matches_screendump(m, artifacts, "colors")


def test_screenshot_refuses_a_screen_shell(boot):
    # On a virtual terminal the PNG's text would only fill the screen: the
    # command says to use serial, and draws no frame.
    m = boot_shells(boot)
    m.type_text("screenshot\n")
    expect_screen(m, r"^screenshot: run it on serial \(the PNG comes out as text\)\nlumen>$")
    assert "@@png" not in read_screen(m).text


def wait_paused(m: Machine, paused: bool, timeout: float = 30) -> None:
    """Wait until `term` says a command is drawing on a canvas (or not)."""
    deadline = time.monotonic() + timeout * m.timeout_scale
    while (PAUSED in run(m, "term")) != paused:
        if time.monotonic() > deadline:
            raise AssertionError(f"the terminal never became {'paused' if paused else 'unpaused'}")
        time.sleep(0.1)


def test_screenshot_says_when_a_canvas_was_drawing(boot):
    # `life` on the screen holds a canvas and draws without the screen's
    # lock: a screenshot meanwhile may be torn, says so, and the harness
    # refuses it unless told otherwise. Once life ends, the flag is gone.
    m = boot_shells(boot)
    m.type_text("life\n")
    wait_paused(m, True)
    with pytest.raises(FrameError, match="a command held the screen during the capture"):
        m.screenshot_via_serial()
    assert m.screenshot_via_serial(allow_torn=True).size == (1280, 800)
    m.type_text(" ")
    wait_paused(m, False)
    assert m.screenshot_via_serial().size == (1280, 800)


# -- the desktop (#153) --------------------------------------------------------

# The demo windows `desktop` opens (kernel/src/desktop.rs `open_demo_windows`:
# frames placed at these corners around 440x210 and 400x210 clients, with
# lumen-wm's Metrics::DESKTOP: a 24-row title bar, 1-pixel borders, a
# 16-pixel close box 4 in from the right border).
WELCOME = (96, 96, 442, 235)
COLOURS = (440, 300, 402, 235)
TITLE_H = 24
# lumen_wm::Theme::LIGHT.
DESKTOP_RGB = (0x3A, 0x6E, 0xA5)
TITLE_FOCUSED_RGB = (0x2B, 0x57, 0x97)
TITLE_RGB = (0xD6, 0xD6, 0xD6)
POINTER_OUTLINE_RGB = (0, 0, 0)
POINTER_FILL_RGB = (0xFF, 0xFF, 0xFF)
STARTED = "desktop: started (Alt+F1 shows the terminals)\n"


def start_desktop(m: Machine) -> None:
    """`desktop` on the serial shell, then wait for both demo windows and
    for the desktop to settle."""
    assert run(m, "desktop", 30) == STARTED
    deadline = time.monotonic() + 30 * m.timeout_scale
    while len(gui.windows(m)) != 2:
        assert time.monotonic() < deadline, "the demo windows never opened"
        time.sleep(0.1)
    gui.wait_settled(m)


def frame_of(w: dict[str, str]) -> tuple[int, int, int, int]:
    return int(w["x"]), int(w["y"]), int(w["w"]), int(w["h"])


def pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    with Image.open(shot) as img:
        return img.convert("RGB").getpixel((x, y))


def test_two_windows_golden(boot, artifacts, golden_dir):
    # `desktop` with no tablet (no pointer drawn): the two demo windows,
    # the front one (Colours) focused, over the desktop's colour, the whole
    # screen against a golden, taken while settled and not presenting.
    m = boot_shells(boot)
    start_desktop(m)
    rows = gui.windows(m)
    assert [(w["title"], w["z"], w["focused"]) for w in rows] == [
        ("Colours", "0", "yes"),
        ("Welcome", "1", "no"),
    ], rows
    assert [frame_of(w) for w in rows] == [COLOURS, WELCOME], rows
    shot = gui.settled_screendump(m, "desktop.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-two-windows.png",
        origin=(0, 0),
        size=(1280, 800),
        artifacts=artifacts,
    )
    assert gui.desktop_check(m) == (0, 0)


def boot_tablet(boot) -> Machine:
    m = boot("qemutest", devices=Devices(tablet=True))
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def test_drag_moves_window(boot, artifacts):
    # A drag of the back window's title bar (#149's confirming steps)
    # raises and focuses it and moves its frame by exactly the drag; the
    # old place shows the desktop again, the new one the window, and
    # damage-only drawing equals a full repaint on the screen too.
    m = boot_tablet(boot)
    start_desktop(m)
    x, y, w, h = WELCOME
    start = (x + 100, y + 12)
    end = (start[0] + 150, start[1] + 280)
    gui.drag(m, start, end, 6)
    gui.wait_settled(m)
    moved = gui.window(m, "Welcome")
    assert frame_of(moved) == (x + 150, y + 280, w, h), moved
    assert (moved["z"], moved["focused"]) == ("0", "yes"), moved
    assert gui.window(m, "Colours")["focused"] == "no"
    # The pointer ends inside the moved title bar: move it away first.
    gui.pointer_move(m, 1200, 40)
    shot = gui.settled_screendump(m, "moved.png")
    assert pixel(shot, x + 20, y + 100) == DESKTOP_RGB, "the old place shows the desktop"
    assert pixel(shot, x + 150 + 60, y + 280 + 2) == TITLE_FOCUSED_RGB, "the moved title bar"
    assert gui.desktop_check(m) == (0, 0)


def test_a_button_held_at_the_start_is_ignored(boot):
    # The left button pressed (in the terminal) where the Colours window's
    # title bar will be, and held while `desktop` starts: the first entry
    # takes it as held across the start, so moving with it and letting it
    # go drags nothing and presses nothing; once it is up, a drag there
    # moves the window as usual.
    m = boot_tablet(boot)
    x, y, w, h = COLOURS
    bar = (x + 100, y + 10)
    gui.pointer_move(m, *bar)
    gui.pointer_down(m)
    start_desktop(m)
    # The first frame the desktop reads is still on the title bar (taken
    # as a press there, it would start a drag), the next far off it.
    gui.pointer_move(m, bar[0] + 5, bar[1])
    gui.pointer_move(m, bar[0] + 200, bar[1] + 150)
    gui.pointer_up(m)
    gui.wait_settled(m)
    rows = gui.windows(m)
    assert [(r["title"], frame_of(r), r["focused"]) for r in rows] == [
        ("Colours", COLOURS, "yes"),
        ("Welcome", WELCOME, "no"),
    ], rows
    status = gui.desktop_status(m)
    assert (status["grab_ends"], status["stray_unpressed"]) == ("0", "0"), status
    assert gui.desktop_check(m) == (0, 0)
    gui.drag(m, bar, (bar[0] + 30, bar[1] + 40), 2)
    gui.wait_settled(m)
    assert frame_of(gui.window(m, "Colours")) == (x + 30, y + 40, w, h)


def test_click_focuses_and_raises(boot):
    # A click on the back window's client area focuses it and raises it to
    # the front; both title bars show it (the focused one's colour moves).
    m = boot_tablet(boot)
    start_desktop(m)
    shot = gui.settled_screendump(m, "before.png")
    welcome_bar = (WELCOME[0] + 300, WELCOME[1] + 4)
    colours_bar = (COLOURS[0] + 200, COLOURS[1] + 4)
    assert pixel(shot, *welcome_bar) == TITLE_RGB
    assert pixel(shot, *colours_bar) == TITLE_FOCUSED_RGB
    gui.pointer_move(m, WELCOME[0] + 60, WELCOME[1] + 120)
    gui.pointer_down(m)
    gui.pointer_up(m)
    gui.pointer_move(m, 1200, 760)
    gui.wait_settled(m)
    rows = gui.windows(m)
    assert [(w["title"], w["z"], w["focused"]) for w in rows] == [
        ("Welcome", "0", "yes"),
        ("Colours", "1", "no"),
    ], rows
    shot = gui.settled_screendump(m, "after.png")
    assert pixel(shot, *welcome_bar) == TITLE_FOCUSED_RGB
    assert pixel(shot, *colours_bar) == TITLE_RGB
    # The overlap is the Welcome window's now (its client area's colour).
    assert pixel(shot, COLOURS[0] + 20, COLOURS[1] + 28) == (0xF3, 0xF3, 0xF3)
    assert gui.desktop_check(m) == (0, 0)


VT_LINE = re.compile(r"(\d+) switches refused, (\d+) keys to the desktop\n")


def to_desktop(m: Machine) -> int:
    found = VT_LINE.search(run(m, "vt"))
    assert found, "no vt line"
    return int(found[2])


def keys_of(m: Machine, title: str) -> int:
    return int(gui.window(m, title)["keys"])


def test_leaving_and_resuming_the_desktop(boot):
    # Keys typed while the desktop is shown go to the focused window and
    # never reach terminal 1's shell (`term text`, and `vt`'s count of
    # keys to the desktop); Alt+F1 leaves it (the F1 press and release
    # reach neither side: the window gets only the Alt press before it,
    # then loses the focus); keys typed in the terminal then never reach a
    # window; `desktop` resumes it, focus back on the same window.
    m = boot_shells(boot)
    start_desktop(m)
    terminal = read_screen(m)
    sent = to_desktop(m)
    gui.type_confirmed(m, "ab")
    deadline = time.monotonic() + 10 * m.timeout_scale
    while keys_of(m, "Colours") != 4:
        assert time.monotonic() < deadline, gui.windows(m)
        time.sleep(0.1)
    gui.wait_settled(m)
    assert to_desktop(m) == sent + 2
    assert read_screen(m).rows == terminal.rows
    gui.confirmed_keys(m, ["alt", "f1"])
    deadline = time.monotonic() + 10 * m.timeout_scale
    while gui.desktop_status(m)["state"] != "paused":
        assert time.monotonic() < deadline, "Alt+F1 didn't leave the desktop"
        time.sleep(0.1)
    colours = gui.window(m, "Colours")
    assert (colours["keys"], colours["focused"]) == ("5", "yes"), colours
    # (The desktop's start was logged after the screen shell's prompt, so
    # what is typed there shows on a row of its own.)
    gui.type_confirmed(m, "cd")
    expect_screen(m, r"^cd$")
    assert keys_of(m, "Colours") == 5
    assert to_desktop(m) == sent + 2
    assert run(m, "desktop", 30) == "desktop: resumed\n"
    gui.wait_settled(m)
    gui.type_confirmed(m, "e")
    deadline = time.monotonic() + 10 * m.timeout_scale
    while keys_of(m, "Colours") != 7:
        assert time.monotonic() < deadline, gui.windows(m)
        time.sleep(0.1)
    gui.wait_settled(m)
    assert to_desktop(m) == sent + 3
    status = gui.desktop_status(m)
    assert status["state"] == "shown" and status["stray_unpressed"] == "0", status
    assert gui.desktop_check(m) == (0, 0)
    # Left again, the terminal still shows what was typed there and not
    # the `e`.
    gui.confirmed_keys(m, ["alt", "f1"])
    expect_screen(m, r"^cd$")


def test_presents_match_a_full_repaint(boot):
    # After a drag, a raise, a focus change and a close, the back buffer
    # equals a full repaint and the screen the back buffer (outside the
    # pointer's box): damage-only drawing misses nothing. (A theme switch
    # joins these with #154.)
    m = boot_tablet(boot)
    start_desktop(m)
    assert gui.desktop_check(m) == (0, 0)
    x, y, _, _ = COLOURS
    gui.drag(m, (x + 120, y + 10), (x - 200, y - 150), 4)
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0), "after a drag"
    welcome = frame_of(gui.window(m, "Welcome"))
    # Left of the Colours window, which now covers most of it.
    gui.pointer_move(m, welcome[0] + 10, welcome[1] + 150)
    gui.pointer_down(m)
    gui.pointer_up(m)
    gui.wait_settled(m)
    assert gui.window(m, "Welcome")["z"] == "0"
    assert gui.desktop_check(m) == (0, 0), "after a raise and focus change"
    # The Welcome window's close box: its owner closes it, the Colours
    # window gets the focus.
    wx, wy, ww, _ = welcome
    close = (wx + ww - 1 - 4 - 8, wy + 12)
    gui.pointer_move(m, *close)
    gui.pointer_down(m)
    gui.pointer_up(m)
    deadline = time.monotonic() + 10 * m.timeout_scale
    while len(gui.windows(m)) != 1:
        assert time.monotonic() < deadline, gui.windows(m)
        time.sleep(0.1)
    gui.wait_settled(m)
    assert gui.window(m, "Colours")["focused"] == "yes"
    assert gui.desktop_check(m) == (0, 0), "after a close"


def test_a_window_behind_drops_and_says_busy(boot):
    # `desktop spin`'s window, held (`desktop hold`: its owner stops reading
    # its events), focused by a click and typed at: past 48 entries its
    # queue drops presses (`rationed`), a release that doesn't fit sets
    # `Lost` (`overflowed`), `windows` counts them in `lost`, and one WARN
    # says so for the burst. Released, it reads them and the count stays.
    m = boot_tablet(boot)
    start_desktop(m)
    out = run(m, "desktop spin")
    found = re.fullmatch(r"desktop spin: window (\d+)\n", out)
    assert found, out
    spin = found[1]
    assert run(m, f"desktop hold {spin}") == f"desktop hold: window {spin}\n"
    row = gui.window(m, "spin")
    sx, sy = int(row["x"]), int(row["y"])
    gui.pointer_move(m, sx + 100, sy + 50)
    gui.pointer_down(m)
    gui.pointer_up(m)
    assert gui.window(m, "spin")["focused"] == "yes"
    warn = r"WARN  desktop: window \d+ is behind: dropping events"
    gui.type_confirmed(m, "abcdefghijklmnopqrstuvwxyz0123", allow=warn)
    deadline = time.monotonic() + 10 * m.timeout_scale
    while int(gui.window(m, "spin", allow=warn)["lost"]) == 0:
        assert time.monotonic() < deadline, gui.windows(m)
        time.sleep(0.1)
    warned = re.findall(rf"WARN  desktop: window {spin} is behind: dropping events", m.output)
    assert len(warned) == 1, warned
    lost = int(gui.window(m, "spin")["lost"])
    status = gui.desktop_status(m)
    assert int(status["rationed"]) + int(status["overflowed"]) == lost, status
    assert run(m, f"desktop release {spin}") == f"desktop release: window {spin}\n"
    time.sleep(0.5)
    assert int(gui.window(m, "spin")["lost"]) == lost
    # Other windows refuse a hold.
    assert run(m, "desktop hold 1") == "desktop hold: window 1 isn't a `desktop spin` window\n"


def test_pointer_moves_over_the_desktop(boot):
    # The compositor draws the pointer straight onto the screen: at its
    # tip after a move, then taken off its old place after the next (the
    # desktop's colour back, the back buffer never touched: `desktop check`
    # 0 and 0 outside the pointer's box).
    m = boot_tablet(boot)
    start_desktop(m)
    gui.pointer_move(m, 700, 650)
    shot = gui.settled_screendump(m, "pointer.png")
    assert pixel(shot, 700, 650) == POINTER_OUTLINE_RGB
    assert pixel(shot, 701, 652) == POINTER_FILL_RGB
    assert pixel(shot, 720, 650) == DESKTOP_RGB
    gui.pointer_move(m, 1000, 700)
    shot = gui.settled_screendump(m, "moved.png")
    assert pixel(shot, 700, 650) == DESKTOP_RGB, "taken off its old place"
    assert pixel(shot, 701, 652) == DESKTOP_RGB
    assert pixel(shot, 1000, 700) == POINTER_OUTLINE_RGB
    assert gui.desktop_check(m) == (0, 0)


def test_desktop_and_canvas_exclude_each_other(boot):
    # A command drawing on a canvas keeps the desktop out, and the desktop
    # keeps a command's canvas out.
    m = boot_shells(boot)
    m.type_text("life\n")
    wait_paused(m, True)
    assert run(m, "desktop") == "desktop: not started: the screen is in use by another command\n"
    assert gui.desktop_status(m)["state"] == "off"
    m.type_text(" ")
    wait_paused(m, False)
    start_desktop(m)
    assert run(m, "mandelbrot") == "mandelbrot: the screen is in use by another command\n"
    assert gui.desktop_status(m)["state"] == "shown"


def test_boot_flag_starts_the_desktop(boot):
    # `lumen.desktop` starts it before the serial prompt.
    m = boot("qemutest", cmdline="lumen.desktop")
    m.expect(r"INFO  desktop: started on 1280x800\n", 60)
    m.expect(PROMPT, 60)
    deadline = time.monotonic() + 30 * m.timeout_scale
    while len(gui.windows(m)) != 2:
        assert time.monotonic() < deadline, "the demo windows never opened"
        time.sleep(0.1)
    status = gui.wait_settled(m)
    assert status["state"] == "shown"
    assert gui.desktop_check(m) == (0, 0)


def test_boot_flag_without_a_screen_warns(boot):
    # With no framebuffer the flag says why once, as a warning, and the
    # boot goes on in text mode: the serial shell answers.
    m = boot("qemutest", cmdline="lumen.desktop", extra=["-vga", "none"])
    m.expect(r"WARN  desktop: not started: no screen to draw on\n", 60)
    m.expect(PROMPT, 60)
    assert gui.desktop_status(m)["state"] == "off"
    assert run(m, "windows") == "windows: no desktop\n"
    assert len(re.findall(r"desktop: not started", m.output)) == 1


def test_desktop_screenshot_matches_screendump(boot, artifacts):
    # While the desktop is shown `screenshot` reads the back buffer (the
    # compositor copies it between passes): it equals QEMU's screendump in
    # every pixel (no tablet: no pointer in either).
    m = boot_shells(boot)
    start_desktop(m)
    for _ in range(5):
        before = gui.wait_settled(m)
        shot = m.screenshot_via_serial()
        dump = Image.open(m.screendump("desktop-dump.png")).convert("RGB")
        after = gui.desktop_status(m)
        if after["frames"] == before["frames"] and after["settled"] == "yes":
            break
    shot.save(artifacts / "desktop-screenshot.png")
    diff, mask = compare(shot, dump, tolerance=0)
    if diff.differing:
        mask.save(artifacts / "desktop-diff.png")
    assert diff.differing == 0, f"{diff.differing} pixels differ (see desktop-diff.png)"
