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
from lumen_harness.screen import expect_screen, expect_window, read_screen, read_window
from lumen_harness.screenshot import FrameError, assert_matches_screendump, parse_frame
from lumen_harness.shell import PROMPT, read_output, run
from PIL import Image

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
PAUSED = ", paused (a command is drawing)"


def boot_shells(boot, **kwargs) -> Machine:
    # No tablet (unless `devices` says so): no mouse pointer on the screen.
    m = boot("qemutest", **kwargs)
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
# lumen_wm::Theme::LIGHT.
DESKTOP_RGB = (0x3A, 0x6E, 0xA5)
TITLE_FOCUSED_RGB = (0x2B, 0x57, 0x97)
TITLE_RGB = (0xD6, 0xD6, 0xD6)
CLIENT_RGB = (0xF3, 0xF3, 0xF3)
POINTER_OUTLINE_RGB = (0, 0, 0)
POINTER_FILL_RGB = (0xFF, 0xFF, 0xFF)
SCREEN_PIXELS = 1280 * 800
# lumen_input::cursor's sprite.
SPRITE_W, SPRITE_H = 11, 17


def frame_of(w: dict[str, str]) -> tuple[int, int, int, int]:
    return int(w["x"]), int(w["y"]), int(w["w"]), int(w["h"])


def pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    with Image.open(shot) as img:
        return img.convert("RGB").getpixel((x, y))


def boot_tablet(boot, **kwargs) -> Machine:
    return boot_shells(boot, devices=Devices(tablet=True), **kwargs)


def boot_cut_delay(boot, ms: int) -> Machine:
    """A machine whose compositor waits `ms` between reading the rings and
    reading the cut (`lumen.desktop_cut_delay_ms`), its desktop started;
    the knob's INFO line checked, so a typo can't silently skip the race."""
    m = boot_shells(boot, cmdline=f"lumen.desktop_cut_delay_ms={ms}")
    gui.start_desktop(m)
    said = f"INFO  desktop: waiting {ms} ms between reading input and the cut (lumen.desktop_cut_delay_ms)"
    assert said in m.output, "the cut delay wasn't taken"
    return m


def keys_of(m: Machine, title: str) -> int:
    return int(gui.window(m, title)["keys"])


def wait_keys(m: Machine, title: str, count: int) -> None:
    gui.wait_for(m, f"{title} keys {count}", lambda: keys_of(m, title), lambda k: k == count)


def test_two_windows_golden(boot, artifacts, golden_dir):
    # `desktop` with no tablet (no pointer drawn): the two demo windows,
    # the front one (Colours) focused, over the desktop's colour, the whole
    # screen against a golden, taken while settled and not presenting.
    # `windows` gives records to a pipeline too.
    m = boot_shells(boot)
    gui.start_desktop(m)
    rows = gui.windows(m)
    assert [(w["title"], w["z"], w["focused"]) for w in rows] == [
        ("Colours", "0", "yes"),
        ("Welcome", "1", "no"),
    ], rows
    assert [frame_of(w) for w in rows] == [COLOURS, WELCOME], rows
    focused = gui.parse_records(run(m, "windows | where focused = yes"))
    assert [w["title"] for w in focused] == ["Colours"], focused
    assert run(m, "windows | where focused = no | count") == "1\n"
    assert gui.parse_records(run(m, "desktop status | select state windows")) == [
        {"state": "shown", "windows": "2"}
    ]
    shot = gui.settled_screendump(m, "desktop.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-two-windows.png",
        origin=(0, 0),
        size=(1280, 800),
        artifacts=artifacts,
    )
    assert gui.desktop_check(m) == (0, 0)


def test_drag_moves_window(boot, artifacts):
    # A drag of the back window's title bar (#149's confirming steps)
    # raises and focuses it and moves its frame by exactly the drag; the
    # old place shows the desktop again, the new one the window, and
    # damage-only drawing equals a full repaint on the screen too.
    m = boot_tablet(boot)
    gui.start_desktop(m)
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
    gui.start_desktop(m)
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
    # the front; both title bars show it (the focused one's colour moves),
    # and the strip of the Colours title bar under the Welcome window is
    # the Welcome window's client area now.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    shot = gui.settled_screendump(m, "before.png")
    welcome_bar = (WELCOME[0] + 300, WELCOME[1] + 4)
    colours_bar = (COLOURS[0] + 200, COLOURS[1] + 4)
    overlap = (COLOURS[0] + 20, COLOURS[1] + 4)
    assert pixel(shot, *welcome_bar) == TITLE_RGB
    assert pixel(shot, *colours_bar) == TITLE_FOCUSED_RGB
    assert pixel(shot, *overlap) == TITLE_FOCUSED_RGB
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
    assert pixel(shot, *overlap) == CLIENT_RGB
    assert gui.desktop_check(m) == (0, 0)


VT_LINE = re.compile(r"(\d+) switches refused, (\d+) keys to the desktop\n")


def to_desktop(m: Machine) -> int:
    found = VT_LINE.search(run(m, "vt"))
    assert found, "no vt line"
    return int(found[2])


def test_leaving_and_resuming_the_desktop(boot):
    # On two CPUs. Keys typed while the desktop is shown go to the focused
    # window and never reach terminal 1's shell (`term text`, and `vt`'s
    # count of keys to the desktop); one key's rectangles presented add
    # under a tenth of the screen's pixels; Alt+F1 leaves it (the F1 press
    # and release reach neither side: the window gets only the Alt press
    # before it, then `FocusLost` in its queue, while `windows` still shows
    # it focused, to give it back at the resume); keys typed in the terminal
    # then never reach a window; `desktop` resumes it, focus back on the
    # same window.
    m = boot_shells(boot, extra=["-smp", "2"])
    gui.start_desktop(m)
    terminal = read_screen(m)
    sent = to_desktop(m)
    gui.type_confirmed(m, "ab")
    wait_keys(m, "Colours", 4)
    gui.wait_settled(m)
    assert to_desktop(m) == sent + 2
    assert read_screen(m).rows == terminal.rows
    gui.confirmed_keys(m, ["alt", "f1"])
    gui.wait_desktop_state(m, "paused")
    # (The compositor acts on the pause at its next pass.)
    wait_keys(m, "Colours", 5)
    assert gui.window(m, "Colours")["focused"] == "yes"
    # (The desktop's start was logged after the screen shell's prompt, so
    # what is typed there shows on a row of its own.)
    gui.type_confirmed(m, "cd")
    expect_screen(m, r"^cd$")
    assert keys_of(m, "Colours") == 5
    assert to_desktop(m) == sent + 2
    assert run(m, "desktop", 30) == "desktop: resumed\n"
    before = gui.wait_settled(m)
    gui.type_confirmed(m, "e")
    wait_keys(m, "Colours", 7)
    status = gui.wait_settled(m)
    assert to_desktop(m) == sent + 3
    drawn = int(status["pixels"]) - int(before["pixels"])
    assert 0 < drawn < SCREEN_PIXELS // 10, f"one key presented {drawn} pixels"
    assert status["state"] == "shown" and status["stray_unpressed"] == "0", status
    assert gui.desktop_check(m) == (0, 0)
    # Left again, the terminal still shows what was typed there and not
    # the `e`.
    gui.confirmed_keys(m, ["alt", "f1"])
    expect_screen(m, r"^cd$")


def test_keys_just_before_alt_f_reach_the_window(boot):
    # A key typed just before Alt+F1, while the compositor is between
    # reading the rings and reading the cut (widened by
    # `lumen.desktop_cut_delay_ms`): the screen shell's decoding pass pushes
    # the rest of the key events and writes the pause meanwhile, and the
    # window still gets every event before the Alt+F1 press: `x`'s press
    # and release and the Alt press. And while the compositor hasn't acted
    # on a key that reached IRQ 1, the desktop isn't settled.
    m = boot_cut_delay(boot, 500)
    gui.confirmed_keys(m, "y")
    assert gui.desktop_status(m)["settled"] == "no", "settled with a key not acted on"
    wait_keys(m, "Colours", 2)
    gui.wait_settled(m)
    gui.confirmed_keys(m, "x", ["alt", "f1"])
    gui.wait_desktop_state(m, "paused")
    wait_keys(m, "Colours", 5)


def pause_while(m: Machine, line: str) -> None:
    """Send `line` on the serial shell while the compositor waits between
    reading the rings and reading the cut (a key wakes it into that
    wait), then Alt+F1: the request `line` makes is accepted while the
    desktop is shown and still queued when the pause comes."""
    m.press(["y"])
    time.sleep(0.3)
    m.send_line(line)
    m.press(["alt", "f1"])


def test_a_check_queued_as_the_desktop_pauses_is_answered_paused(boot):
    # `desktop check` accepted just before Alt+F1: the paused pass answers
    # it at once (within 10 s, not after the asker's 20 s), and no WARN.
    m = boot_cut_delay(boot, 1500)
    pause_while(m, "desktop check")
    assert read_output(m, "desktop check", 10) == "desktop check: paused (`desktop` resumes it)\n"
    gui.wait_desktop_state(m, "paused")


def test_a_screenshot_queued_as_the_desktop_pauses_reads_the_text_screen(boot):
    # `screenshot` accepted just before Alt+F1: answered at the paused
    # pass, it reads the text screen instead (what is shown then: the
    # header's background where the desktop was blue), and no WARN.
    m = boot_cut_delay(boot, 1500)
    pause_while(m, "screenshot")
    frame = parse_frame(read_output(m, "screenshot", 60))
    assert frame.image().getpixel((5, 5)) == (0x0F, 0x14, 0x20), "not the text screen"
    gui.wait_desktop_state(m, "paused")


def test_presents_match_a_full_repaint(boot):
    # After a drag whose frame edge cuts through the other window's title
    # text (titles drawn clipped to each damaged rectangle), a drag, a
    # raise, a focus change and a close, the back buffer equals a full
    # repaint and the screen the back buffer (outside the pointer's box):
    # damage-only drawing misses nothing. (A theme switch joins these with
    # #154.)
    m = boot_tablet(boot)
    gui.start_desktop(m)
    assert gui.desktop_check(m) == (0, 0)
    # The Welcome window's right edge, in small steps, through "Colours"
    # in the other title bar (from x 448).
    wx, wy, _, _ = WELCOME
    gui.drag(m, (wx + 200, wy + 12), (wx + 200 - 68, wy + 12), 4)
    gui.wait_settled(m)
    assert frame_of(gui.window(m, "Welcome"))[0] + WELCOME[2] == 470
    assert gui.desktop_check(m) == (0, 0), "after an edge through a title"
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
    gui.wait_windows(m, 1)
    gui.wait_settled(m)
    assert gui.window(m, "Colours")["focused"] == "yes"
    assert gui.desktop_check(m) == (0, 0), "after a close"


def test_a_window_behind_drops_and_says_busy(boot):
    # `desktop spin`'s window, never idle (the desktop never settles while
    # it runs), held (`desktop hold`: its owner stops reading its events),
    # focused by a click and typed at: past 48 entries its queue drops
    # presses (`rationed`), a release that doesn't fit sets `Lost`
    # (`overflowed`), `windows` counts them in `lost`, and one WARN says so
    # for the burst. Released, it reads its queue again: the count stays,
    # and a key typed then is queued.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    spin = gui.desktop_spin(m)
    assert gui.desktop_status(m)["settled"] == "no", "settled with a window that never idles"
    assert run(m, f"desktop hold {spin}") == f"desktop hold: window {spin}\n"
    row = gui.window(m, "spin")
    sx, sy = int(row["x"]), int(row["y"])
    gui.pointer_move(m, sx + 100, sy + 50)
    gui.pointer_down(m)
    gui.pointer_up(m)
    assert gui.window(m, "spin")["focused"] == "yes"
    warn = r"WARN  desktop: window \d+ is behind: dropping events"
    gui.type_confirmed(m, "abcdefghijklmnopqrstuvwxyz0123", allow=warn)
    gui.wait_for(
        m, "events lost", lambda: int(gui.window(m, "spin", allow=warn)["lost"]), lambda n: n > 0
    )
    warned = re.findall(rf"WARN  desktop: window {spin} is behind: dropping events", m.output)
    assert len(warned) == 1, warned
    lost = int(gui.window(m, "spin")["lost"])
    keys = keys_of(m, "spin")
    status = gui.desktop_status(m)
    assert int(status["rationed"]) + int(status["overflowed"]) == lost, status
    assert run(m, f"desktop release {spin}") == f"desktop release: window {spin}\n"
    # Its queue drained, a key typed now is queued (held, it would be
    # rationed).
    time.sleep(0.5)
    gui.type_confirmed(m, "z")
    wait_keys(m, "spin", keys + 2)
    assert int(gui.window(m, "spin")["lost"]) == lost
    # Other windows refuse a hold.
    assert run(m, "desktop hold 1") == "desktop hold: window 1 isn't a `desktop spin` window\n"


def test_pointer_moves_over_the_desktop(boot):
    # The compositor draws the pointer straight onto the screen: at its
    # tip after a move, then taken off its old place after the next (the
    # desktop's colour back, the back buffer never touched: `desktop check`
    # 0 and 0 outside the pointer's box).
    m = boot_tablet(boot)
    gui.start_desktop(m)
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
    # A command drawing on a canvas keeps the desktop out (the start fails
    # after its compositor thread began, and is undone), and the desktop
    # keeps a command's canvas out.
    m = boot_shells(boot)
    m.type_text("life\n")
    wait_paused(m, True)
    assert run(m, "desktop") == "desktop: not started: the screen is in use by another command\n"
    assert gui.desktop_status(m)["state"] == "off"
    m.type_text(" ")
    wait_paused(m, False)
    gui.start_desktop(m)
    assert run(m, "mandelbrot") == "mandelbrot: the screen is in use by another command\n"
    assert gui.desktop_status(m)["state"] == "shown"


def test_boot_flag_starts_the_desktop(boot):
    # `lumen.desktop` starts it before the serial prompt.
    m = boot("qemutest", cmdline="lumen.desktop")
    m.expect(r"INFO  desktop: started on 1280x800\n", 60)
    m.expect(PROMPT, 60)
    gui.wait_windows(m, 2)
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
    # compositor copies it between passes), which never holds the pointer:
    # with the pointer over the desktop it equals QEMU's screendump in
    # every pixel outside the arrow's box, and differs inside it (the
    # screendump has the arrow; the screenshot, the desktop's colour).
    m = boot_tablet(boot)
    gui.start_desktop(m)
    tip = (900, 650)
    gui.pointer_move(m, *tip)

    def take():
        with Image.open(m.screendump("desktop-dump.png")) as dumped:
            return m.screenshot_via_serial(), dumped.convert("RGB")

    shot, dump = gui.settled_capture(m, take)
    shot.save(artifacts / "desktop-screenshot.png")
    arrow = (tip[0], tip[1], tip[0] + SPRITE_W, tip[1] + SPRITE_H)
    inside, _ = compare(shot.crop(arrow), dump.crop(arrow), tolerance=0)
    assert inside.differing > 0, "the screenshot shows the pointer: not the back buffer"
    assert shot.getpixel(tip) == DESKTOP_RGB
    a, b = shot.copy(), dump.copy()
    for img in (a, b):
        img.paste((0, 0, 0), arrow)
    diff, mask = compare(a, b, tolerance=0)
    if diff.differing:
        mask.save(artifacts / "desktop-diff.png")
    assert diff.differing == 0, (
        f"{diff.differing} pixels differ outside the arrow (see desktop-diff.png)"
    )


# -- terminal windows (#155) ---------------------------------------------------

# kernel/src/termwin.rs: an 80x24 grid of 7x16 cells 4 pixels in from the
# client area's edge; its default colours are the console's (kernel/src/fb.rs
# TEXT on BG). The client area sits 1 pixel in from the frame's left and 24
# down (lumen_wm::Metrics::DESKTOP's border and title bar).
TERM_PAD, TERM_CELL_W, TERM_CELL_H = 4, 7, 16
TERM_TEXT_RGB = (0xE6, 0xE6, 0xE6)
TERM_BG_RGB = (0x0F, 0x14, 0x20)
TITLE_H = 24
NO_CANVAS = "life: a terminal window has no canvas: run it on a virtual terminal (Alt+F1)"


def open_terminal(m: Machine) -> int:
    """`terminal` on the serial shell: the new window's id, once its shell's
    prompt shows in it."""
    out = run(m, "terminal")
    found = re.fullmatch(r"terminal: window (\d+)\n", out)
    assert found, f"terminal said {out!r}"
    window = int(found[1])
    expect_window(m, window, r"^lumen>$", 30)
    return window


def click(m: Machine, x: int, y: int) -> None:
    gui.pointer_move(m, x, y)
    gui.pointer_down(m)
    gui.pointer_up(m)


def wait_focused(m: Machine, title: str) -> None:
    gui.wait_for(
        m, f"{title} focused", lambda: gui.window(m, title)["focused"], lambda f: f == "yes"
    )


def thread_names(m: Machine) -> list[str]:
    """`threads`' names (after its count line and its header)."""
    return [line.split()[0] for line in run(m, "threads").splitlines()[2:] if line.strip()]


def heap_used(m: Machine) -> int:
    found = re.search(
        r"^heap: \d+ bytes mapped at 0x[0-9a-f]+, (\d+) used", run(m, "heap"), re.MULTILINE
    )
    assert found, "no heap line"
    return int(found[1])


def test_terminal_window_runs_shell(boot):
    # Two terminal windows, each its own shell. The second opens in front
    # and focused; a click on the first's client area focuses it, a click on
    # the second's title bar focuses the second again. Keys typed (confirmed
    # one at a time) run `echo hello` in the second only: its view (`term
    # text <window>`) shows the output, the first's is unchanged. A command
    # that draws on the screen is refused there with one line. The
    # unfocused first window's cursor is hollow and steady: outlined in the
    # text colour, its middle the background, in two screendumps more than
    # a blink apart.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    first, second = open_terminal(m), open_terminal(m)
    assert second != first
    one, two = frame_of(gui.window(m, "Terminal-1")), frame_of(gui.window(m, "Terminal-2"))
    assert gui.window(m, "Terminal-2")["focused"] == "yes"
    click(m, one[0] + 100, one[1] + 200)
    wait_focused(m, "Terminal-1")
    click(m, two[0] + 200, two[1] + 8)
    wait_focused(m, "Terminal-2")
    gui.pointer_move(m, 1200, 790)
    before = read_window(m, first)
    gui.type_confirmed(m, "echo hello\n")
    expect_window(m, second, r"^lumen> echo hello\nhello\nlumen>$")
    assert read_window(m, first) == before
    # Drawn in the second window's row of "hello" (some pixel of its five
    # cells isn't the background), and not in the first's same row.
    row = read_window(m, second).rows.index("hello")

    def inked(frame: tuple[int, int, int, int], shot) -> bool:
        x0 = frame[0] + 1 + TERM_PAD
        y0 = frame[1] + TITLE_H + TERM_PAD + row * TERM_CELL_H
        with Image.open(shot) as img:
            rgb = img.convert("RGB")
            return any(
                rgb.getpixel((x, y)) != TERM_BG_RGB
                for x in range(x0, x0 + 5 * TERM_CELL_W)
                for y in range(y0, y0 + TERM_CELL_H)
            )

    shot = m.screendump("hello.png")
    assert inked(two, shot) and not inked(one, shot)
    gui.type_confirmed(m, "life\n")
    expect_window(m, second, "^" + re.escape(NO_CANVAS) + r"\nlumen>$")
    assert read_window(m, first) == before
    assert before.cursor is not None
    col, row = before.cursor
    x = one[0] + 1 + TERM_PAD + col * TERM_CELL_W
    y = one[1] + TITLE_H + TERM_PAD + row * TERM_CELL_H
    for name in ("cursor-a.png", "cursor-b.png"):
        shot = m.screendump(name)
        assert pixel(shot, x + 3, y) == TERM_TEXT_RGB, name
        assert pixel(shot, x, y + 8) == TERM_TEXT_RGB, name
        assert pixel(shot, x + 3, y + 8) == TERM_BG_RGB, name
        time.sleep(0.7)


def test_closing_a_terminal_window_frees_it(boot):
    # The close box ends a terminal window: the window goes, its owner and
    # its shell end (joined: neither in `threads`), and the heap is back
    # within 16 KiB of what it was before the window opened (it held its
    # grid and scrollback, two surfaces and its shell, well over 1 MiB;
    # the serial shell's own history grows by the lines typed meanwhile).
    # One window opened and closed first, so first-use allocations don't
    # count. Then damage-only drawing still equals a full repaint.
    m = boot_tablet(boot)
    gui.start_desktop(m)

    def close(title: str) -> None:
        x, y, w, _ = frame_of(gui.window(m, title))
        # The close box: 16 pixels, 4 in from the right border, centred in
        # the title bar.
        click(m, x + w - 1 - 4 - 8, y + TITLE_H // 2)
        gui.wait_for(
            m,
            f"{title} closed",
            lambda: ([w["title"] for w in gui.windows(m)], thread_names(m)),
            lambda got: (
                title not in got[0] and "terminal" not in got[1] and "term-shell" not in got[1]
            ),
            30,
        )
        gui.pointer_move(m, 1200, 790)

    open_terminal(m)
    close("Terminal-1")
    before = heap_used(m)
    open_terminal(m)
    names = thread_names(m)
    assert "terminal" in names and "term-shell" in names, names
    opened = heap_used(m)
    close("Terminal-2")
    after = heap_used(m)
    assert opened - before > 1024 * 1024, (before, opened)
    assert after - before <= 16 * 1024, (before, opened, after)
    assert gui.desktop_check(m) == (0, 0)
