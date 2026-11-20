"""The screen as the kernel sees it (`screenshot`, #150) and the desktop
(#153: the compositor, its windows, the pointer on it, leaving and
resuming it). ARCHITECTURE section 11."""

from __future__ import annotations

import re
import time
from pathlib import Path

import pytest
from lumen_harness import gui
from lumen_harness.image import assert_matches_golden, compare
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import expect_screen, expect_window, read_screen, read_window
from lumen_harness.screenshot import FrameError, assert_matches_screendump, parse_frame
from lumen_harness.shell import PROMPT, heap_line, read_output, run
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
    gui.confirmed_keys(m, "colors\n")
    expect_screen(m, r"^colors: done\nlumen>$")
    assert_matches_screendump(m, artifacts, "colors")


def test_screenshot_refuses_a_screen_shell(boot):
    # On a virtual terminal the PNG's text would only fill the screen: the
    # command says to use serial, and draws no frame.
    m = boot_shells(boot)
    gui.confirmed_keys(m, "screenshot\n")
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
    gui.confirmed_keys(m, "life\n")
    wait_paused(m, True)
    with pytest.raises(FrameError, match="a command held the screen during the capture"):
        m.screenshot_via_serial()
    assert m.screenshot_via_serial(allow_torn=True).size == (1280, 800)
    gui.confirmed_keys(m, " ")
    wait_paused(m, False)
    assert m.screenshot_via_serial().size == (1280, 800)


# -- the desktop (#153) --------------------------------------------------------

# The demo windows `desktop` opens (kernel/src/desktop.rs `open_demo_windows`:
# frames placed at these corners around 440x210 and 400x210 clients, with
# lumen-wm's Metrics::DESKTOP: a 24-row title bar, 1-pixel borders, a
# 16-pixel close box 4 in from the right border).
WELCOME = (96, 96, 442, 235)
COLOURS = (440, 300, 402, 235)
# The wallpaper the desktop draws behind the windows (made by
# tools/wallpaper.py, embedded by the kernel): 1280x800, so on the screen
# each of its pixels is at its own place.
WALLPAPER = Path(__file__).resolve().parents[2] / "assets" / "wallpaper" / "lumen.png"
# lumen_wm::Theme::LIGHT.
TITLE_FOCUSED_RGB = (0x2B, 0x57, 0x97)
TITLE_RGB = (0xD6, 0xD6, 0xD6)
CLIENT_RGB = (0xF3, 0xF3, 0xF3)
# lumen_wm::Theme::DARK's client colour.
DARK_CLIENT_RGB = (0x1F, 0x1F, 0x1F)
POINTER_OUTLINE_RGB = (0, 0, 0)
POINTER_FILL_RGB = (0xFF, 0xFF, 0xFF)
SCREEN_PIXELS = 1280 * 800
# lumen_input::cursor's sprite.
SPRITE_W, SPRITE_H = 11, 17


def wallpaper_rgb(x: int, y: int) -> tuple[int, int, int]:
    """The desktop's pixel at (x, y) where no window covers it."""
    return pixel(WALLPAPER, x, y)


# A row's bounds (a `windows` row's frame): x, y, w, h.
frame_of = gui.bounds


def pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    with Image.open(shot) as img:
        return img.convert("RGB").getpixel((x, y))


def close_window(m: Machine, title: str) -> None:
    """Click `title`'s close box (its owner closes it)."""
    x, y, w, _ = frame_of(gui.window(m, title))
    gui.pointer_move(m, x + w - 1 - 4 - 8, y + 12)
    gui.pointer_down(m)
    gui.pointer_up(m)


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
    # the front one (Colours) focused, their titles in the UI font, over the
    # wallpaper, the whole screen against a golden, taken while settled and
    # not presenting.
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
        masks=gui.clock_mask(m),
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
    assert pixel(shot, x + 20, y + 100) == wallpaper_rgb(x + 20, y + 100), (
        "the old place shows the desktop"
    )
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
    # on a key that reached IRQ 1, the desktop isn't settled (read within the
    # 2 s the compositor waits, the knob's most: only a stall that long
    # passes it first).
    m = boot_cut_delay(boot, 2000)
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
    # unconfirmed: a race on purpose: `y` must wake the compositor just
    # before `line` lands (and Alt+F1 goes while the serial shell is busy
    # with it)
    m.press(["y"])
    time.sleep(0.3)
    m.send_line(line)
    m.press(["alt", "f1"])  # unconfirmed: the same race, `line` still running


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
    # damage-only drawing misses nothing. Then a theme switch to dark and
    # back (the whole screen damaged and recomposed; that the owners redraw
    # in the new colours is the dark golden's to show).
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
    close_window(m, "Welcome")
    gui.wait_windows(m, 1)
    gui.wait_settled(m)
    assert gui.window(m, "Colours")["focused"] == "yes"
    assert gui.desktop_check(m) == (0, 0), "after a close"
    # The titles' glyphs are cached: a switch (new colours, the same
    # glyphs) rasterizes none, and no glyph is ever dropped.
    misses = gui.desktop_status(m)["glyph_misses"]
    for name in ("dark", "light"):
        assert run(m, f"theme {name}") == f"theme: {name}\n"
        status = gui.wait_settled(m)
        assert status["theme"] == name
        assert (status["glyph_misses"], status["glyphs_dropped"]) == (misses, "0"), status
        assert gui.desktop_check(m) == (0, 0), f"after the switch to {name}"


def drained(m: Machine, node: dict[str, str], allow: str | None = None) -> None:
    """Wait until a released window's owner has read its whole queue: a
    `focus` request on one of its nodes is answered only after everything
    queued before it (refused as busy while the queue is still full: only
    that is retried; any other message fails)."""

    def answered(got: gui.Invoked) -> bool:
        if got.row is not None:
            return True
        assert got.message is not None and got.message.endswith("busy (try again)"), got
        return False

    gui.wait_for(
        m, "the queue drained", lambda: gui.invoke(m, node["id"], "focus", allow=allow), answered
    )


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
    stop = gui.node(gui.wait_ui(m, "spin's tree", lambda r: len(r) > 1, spin), "button", "Stop")
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
    drained(m, stop)
    gui.type_confirmed(m, "z")
    wait_keys(m, "spin", keys + 2)
    assert int(gui.window(m, "spin")["lost"]) == lost
    # Other windows refuse a hold.
    assert run(m, "desktop hold 1") == "desktop hold: window 1 isn't a `desktop spin` window\n"
    # Held again, its Stop button's `ui invoke` is queued but never taken:
    # cancelled at its 1 s deadline (counted). With the queue full, the
    # next is refused as busy (never queued). Released, the owner skips the
    # cancelled press: the window is still open once its queue drained.
    assert run(m, f"desktop hold {spin}") == f"desktop hold: window {spin}\n"
    cancelled = int(gui.desktop_status(m)["cancelled"])
    out = run(m, f"ui invoke {stop['id']} press")
    assert out == f"ui invoke: cancelled: window {spin} didn't take it in 1 s (try again)\n", out
    assert int(gui.desktop_status(m)["cancelled"]) == cancelled + 1
    gui.type_confirmed(m, "abcdefghijklmnopqrstuvwxyz0123", allow=warn)
    gui.wait_for(
        m,
        "a full queue",
        lambda: int(gui.window(m, "spin", allow=warn)["lost"]),
        lambda n: n > lost,
    )
    busy = f"ui invoke: window {spin} busy (try again)\n"
    assert run(m, f"ui invoke {stop['id']} press", allow=warn) == busy
    assert run(m, f"desktop release {spin}", allow=warn) == f"desktop release: window {spin}\n"
    drained(m, stop, allow=warn)
    assert "spin" in [w["title"] for w in gui.windows(m, allow=warn)], "the cancelled press ran"
    # Held, its root's `close` times out: still open, its close still
    # queued; released, the owner reads it and the window goes.
    root = gui.ui_tree(m, spin)[0]
    assert run(m, f"desktop hold {spin}") == f"desktop hold: window {spin}\n"
    still_open = f"ui invoke: window {spin} still open (its close is still queued)\n"
    assert run(m, f"ui invoke {root['id']} close") == still_open
    assert run(m, f"desktop release {spin}") == f"desktop release: window {spin}\n"
    assert [w["title"] for w in gui.wait_windows(m, 2)] == ["Colours", "Welcome"]


def test_pointer_moves_over_the_desktop(boot):
    # The compositor draws the pointer straight onto the screen: at its
    # tip after a move, then taken off its old place after the next (the
    # wallpaper back, the back buffer never touched: `desktop check` 0 and
    # 0 outside the pointer's box).
    m = boot_tablet(boot)
    gui.start_desktop(m)
    gui.pointer_move(m, 700, 650)
    shot = gui.settled_screendump(m, "pointer.png")
    assert pixel(shot, 700, 650) == POINTER_OUTLINE_RGB
    assert pixel(shot, 701, 652) == POINTER_FILL_RGB
    assert pixel(shot, 720, 650) == wallpaper_rgb(720, 650)
    gui.pointer_move(m, 1000, 700)
    shot = gui.settled_screendump(m, "moved.png")
    assert pixel(shot, 700, 650) == wallpaper_rgb(700, 650), "taken off its old place"
    assert pixel(shot, 701, 652) == wallpaper_rgb(701, 652)
    assert pixel(shot, 1000, 700) == POINTER_OUTLINE_RGB
    assert gui.desktop_check(m) == (0, 0)


def test_desktop_and_canvas_exclude_each_other(boot):
    # A command drawing on a canvas keeps the desktop out (the start fails
    # after its compositor thread began, and is undone), and the desktop
    # keeps a command's canvas out.
    m = boot_shells(boot)
    gui.confirmed_keys(m, "life\n")
    wait_paused(m, True)
    assert run(m, "desktop") == "desktop: not started: the screen is in use by another command\n"
    assert gui.desktop_status(m)["state"] == "off"
    gui.confirmed_keys(m, " ")
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
    # screendump has the arrow; the screenshot, the wallpaper).
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
    assert shot.getpixel(tip) == wallpaper_rgb(*tip)
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
TITLE_H = gui.TITLE_H
NO_CANVAS = "life: a terminal window has no canvas: run it on a virtual terminal (Alt+F1)"
TERM_THREADS = ("terminal", "term-shell")


def open_terminal(m: Machine) -> int:
    """`terminal` on the serial shell: the new window's id, once its shell's
    prompt shows in it."""
    out = run(m, "terminal")
    found = re.fullmatch(r"terminal: window (\d+)\n", out)
    assert found, f"terminal said {out!r}"
    window = int(found[1])
    expect_window(m, window, r"^lumen>$", 30)
    return window


def cell_xy(frame: tuple[int, int, int, int], col: int, row: int) -> tuple[int, int]:
    """The top-left pixel of cell (`col`, `row`) of the terminal window whose
    frame is `frame`."""
    return (
        frame[0] + 1 + TERM_PAD + col * TERM_CELL_W,
        frame[1] + TITLE_H + TERM_PAD + row * TERM_CELL_H,
    )


def close_box(frame: tuple[int, int, int, int]) -> tuple[int, int]:
    """The middle of a frame's close box (16 pixels, 4 in from the right
    border, centred in the title bar)."""
    return frame[0] + frame[2] - 1 - 4 - 8, frame[1] + TITLE_H // 2


def wait_focused(m: Machine, title: str) -> None:
    gui.wait_for(
        m, f"{title} focused", lambda: gui.window(m, title)["focused"], lambda f: f == "yes"
    )


def thread_names(m: Machine) -> list[str]:
    """`threads`' names (after its count line and its header)."""
    return [line.split()[0] for line in run(m, "threads").splitlines()[2:] if line.strip()]


def heap_used(m: Machine) -> int:
    return int(heap_line(run(m, "heap"))["used"])


# Where the pointer waits out of the way (over the wallpaper).
PARK = (1200, 790)


def click_to_focus(m: Machine, title: str, x: int, y: int) -> None:
    """Click (`x`, `y`), wait until window `title` is focused, then park the
    pointer."""
    gui.click(m, x, y)
    wait_focused(m, title)
    gui.pointer_move(m, *PARK)


def close_terminal(m: Machine, title: str, threads: tuple[str, ...] = TERM_THREADS) -> None:
    """Click terminal window `title`'s close box; wait until the window is
    gone from `windows` and none of `threads` is in `threads`."""
    gui.click(m, *close_box(frame_of(gui.window(m, title))))
    gui.wait_for(
        m,
        f"{title} closed",
        lambda: ([w["title"] for w in gui.windows(m)], thread_names(m)),
        lambda got: title not in got[0] and not set(threads) & set(got[1]),
        30,
    )
    gui.pointer_move(m, *PARK)


def screendump_until(m: Machine, name: str, done, timeout: float = 5) -> Path:
    """Take screendumps until `done(path)` holds (`gui.wait_for`); the
    last one's path."""
    return gui.wait_for(m, f"{name} as wanted", lambda: m.screendump(name), done, timeout)


def never_settled(m: Machine, reads: int = 8) -> None:
    """`desktop status` says `settled no` in each of `reads` reads 0.2 s
    apart."""
    got = []
    for _ in range(reads):
        got.append(gui.desktop_status(m)["settled"])
        time.sleep(0.2)
    assert got == ["no"] * reads, got


def test_terminal_window_runs_shell(boot):
    # Two terminal windows, each its own shell. The second opens in front
    # and focused; a click on the first's client area focuses it, a click on
    # the second's title bar focuses the second again. Keys typed (confirmed
    # one at a time) run `echo hello` in the second only: its view (`term
    # text <window>`) shows the output and it is drawn there, while the
    # first's view and pixels are unchanged. A command that draws on the
    # screen is refused there with one line. The focused window's cursor
    # blinks: both looks of its cell show within 2 s.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    first, second = open_terminal(m), open_terminal(m)
    assert second != first
    one, two = frame_of(gui.window(m, "Terminal-1")), frame_of(gui.window(m, "Terminal-2"))
    assert gui.window(m, "Terminal-2")["focused"] == "yes"
    click_to_focus(m, "Terminal-1", one[0] + 100, one[1] + 200)
    click_to_focus(m, "Terminal-2", two[0] + 200, two[1] + 8)
    before = read_window(m, first)
    gui.type_confirmed(m, "echo hello\n")
    expect_window(m, second, r"^lumen> echo hello\nhello\nlumen>$")
    assert read_window(m, first) == before
    # Drawn in the second window's row of "hello" (some pixel of its five
    # cells isn't the background), once its owner has drawn it, and never
    # in the first's same row.
    row = read_window(m, second).rows.index("hello")

    def inked(frame: tuple[int, int, int, int], shot) -> bool:
        x0, y0 = cell_xy(frame, 0, row)
        with Image.open(shot) as img:
            rgb = img.convert("RGB")
            return any(
                rgb.getpixel((x, y)) != TERM_BG_RGB
                for x in range(x0, x0 + 5 * TERM_CELL_W)
                for y in range(y0, y0 + TERM_CELL_H)
            )

    shot = screendump_until(m, "hello.png", lambda shot: inked(two, shot))
    assert not inked(one, shot)
    gui.type_confirmed(m, "life\n")
    expect_window(m, second, "^" + re.escape(NO_CANVAS) + r"\nlumen>$")
    assert read_window(m, first) == before
    # The focused second window's cursor: on (the cell inverted: its middle
    # the text colour) and off (the background), both within 2 s.
    cursor = read_window(m, second).cursor
    assert cursor is not None
    x, y = cell_xy(two, *cursor)
    seen = set()

    def both_looks(shot) -> bool:
        seen.add(pixel(shot, x + 3, y + 8))
        return {TERM_TEXT_RGB, TERM_BG_RGB} <= seen

    screendump_until(m, "blink.png", both_looks, 2)


def test_terminal_windows_settle_only_when_idle(boot):
    # `desktop status`'s `settled` and terminal windows. An unfocused
    # terminal window whose shell waits for a key settles, its owner idle:
    # its cursor hollow (outlined in the text colour, the background in its
    # middle) and steady, nothing presented for over a second. A focused one
    # never settles (its cursor blinks). One running `top` never settles
    # either, unfocused (its shell is busy, its output keeps coming).
    m = boot_tablet(boot)
    gui.start_desktop(m)
    window = open_terminal(m)
    frame = frame_of(gui.window(m, "Terminal-1"))
    never_settled(m)
    click_to_focus(m, "Colours", COLOURS[0] + 200, COLOURS[1] + 150)
    gui.quiet_for(m, 1.2)
    cursor = read_window(m, window).cursor
    assert cursor is not None
    x, y = cell_xy(frame, *cursor)
    shot = gui.settled_screendump(m, "hollow.png")
    assert pixel(shot, x + 3, y) == TERM_TEXT_RGB
    assert pixel(shot, x, y + 8) == TERM_TEXT_RGB
    assert pixel(shot, x + 3, y + 8) == TERM_BG_RGB
    # `top`, typed in it focused, then left running unfocused.
    click_to_focus(m, "Terminal-1", frame[0] + 200, frame[1] + 200)
    gui.type_confirmed(m, "top\n")
    expect_window(m, window, r"^top: frame", 10)
    click_to_focus(m, "Colours", COLOURS[0] + 200, COLOURS[1] + 150)
    never_settled(m)


def test_closing_a_terminal_window_frees_it(boot):
    # The close box ends a terminal window: the window goes, its owner and
    # its shell end (joined: neither in `threads`). First the serial
    # shell's history is filled to its cap (100 lines), so the lines typed
    # meanwhile replace others rather than add, and one window is opened
    # and closed, so first-use allocations don't count; then four, one at a
    # time, each holding well over 1 MiB (its grid and scrollback, two
    # surfaces, its shell): the heap after all four is back within 1.5 KiB
    # of before (a steady 992 bytes when measured). Then damage-only drawing
    # still equals a full repaint.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    for i in range(100):
        run(m, f"echo filling-the-history-{i}")
    open_terminal(m)
    close_terminal(m, "Terminal-1")
    before = heap_used(m)
    for n in range(2, 6):
        open_terminal(m)
        names = thread_names(m)
        assert "terminal" in names and "term-shell" in names, names
        assert heap_used(m) - before > 1024 * 1024, n
        close_terminal(m, f"Terminal-{n}")
    after = heap_used(m)
    assert after - before <= 1536, (before, after)
    assert gui.desktop_check(m) == (0, 0)


@pytest.mark.parametrize(
    ("command", "started", "threads"),
    [
        ("rhai", r"^rhai> ?$", ("rhai", *TERM_THREADS)),
        ("top", r"^top: frame", TERM_THREADS),
        ("rhai -e loop{}", r"^lumen> rhai -e loop\{\}$", ("rhai", *TERM_THREADS)),
    ],
    ids=["rhai-repl", "top", "script-loop"],
)
def test_closing_a_terminal_window_ends_what_runs_in_it(boot, command, started, threads):
    # A command running in a terminal window when its close box is
    # clicked: `rhai`'s REPL waiting for a line (a closed console is no
    # Ctrl-C, which would only give a new prompt), `top` between frames, a
    # script in a loop (its Ctrl-C check). Each stops on the closed
    # console's `Key::Closed`, then the shell, so the owner joins it: the
    # command's thread, `term-shell` and `terminal` all leave `threads`.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    window = open_terminal(m)
    gui.type_confirmed(m, command + "\n")
    expect_window(m, window, started, 10)
    close_terminal(m, "Terminal-1", threads)


def test_terminal_is_refused_unless_the_desktop_is_shown(boot):
    # `terminal` before `desktop` and after Alt+F1 left it: refused with
    # the desktop's own words, and no window opens.
    m = boot_tablet(boot)
    assert run(m, "terminal") == "terminal: desktop: not started (`desktop` starts it)\n"
    gui.start_desktop(m)
    open_terminal(m)
    gui.confirmed_keys(m, ["alt", "f1"])
    gui.wait_desktop_state(m, "paused")
    assert run(m, "terminal") == "terminal: desktop: paused (`desktop` resumes it)\n"
    assert [w["title"] for w in gui.windows(m)] == ["Terminal-1", "Colours", "Welcome"]


def test_a_terminal_window_refuses_screenshot_and_keeps_its_colours(boot):
    # `screenshot` typed in a terminal window says to use serial (base64 in
    # a window is no use). A theme switch leaves the window open, its cells
    # in the console's colours (its background where nothing is written),
    # and keys still reach its shell.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    window = open_terminal(m)
    gui.type_confirmed(m, "screenshot\n")
    expect_window(
        m, window, r"^screenshot: run it on serial \(the PNG comes out as text\)\nlumen>$"
    )
    run(m, "theme dark")
    frame = frame_of(gui.window(m, "Terminal-1"))
    x, y = cell_xy(frame, 40, 12)
    screendump_until(m, "dark.png", lambda shot: pixel(shot, x + 3, y + 8) == TERM_BG_RGB)
    gui.type_confirmed(m, "echo still\n")
    expect_window(m, window, r"^still\nlumen>$")


def test_a_terminal_window_scrolls(boot):
    # More lines than its 24 rows: the view shows the last ones, its pixels
    # moved up (a ktest compares them with a full repaint).
    m = boot_tablet(boot)
    gui.start_desktop(m)
    window = open_terminal(m)
    gui.type_confirmed(m, "seq 40\n")
    expect_window(m, window, r"^38\n39\n40\nlumen>$")
    screen = read_window(m, window)
    assert screen.rows[0] == "18", screen.rows


# -- text, wallpaper, themes (#154) ----------------------------------------------

# The Welcome window's text: its client area (inside its frame: a 1-pixel
# border, a 24-row title bar) down to where the Colours window's frame
# starts.
WELCOME_TEXT = (WELCOME[0] + 1, WELCOME[1] + 24, 440, COLOURS[1] - WELCOME[1] - 24)


def test_text_rendering_golden(boot, artifacts, golden_dir):
    # The Welcome window's text in the UI font (Atkinson Hyperlegible,
    # anti-aliased) at two sizes: its heading at 24 pixels, its lines at 16,
    # against a golden of that part of its client area.
    m = boot_shells(boot)
    gui.start_desktop(m)
    shot = gui.settled_screendump(m, "text.png")
    x, y, w, h = WELCOME_TEXT
    assert_matches_golden(
        shot, golden_dir / "desktop-text.png", origin=(x, y), size=(w, h), artifacts=artifacts
    )


def test_png_wallpaper_golden(boot, artifacts):
    # The desktop with no windows: both closed by their close boxes, the
    # pointer moved to the right end of the last row above the taskbar.
    # The screen above the taskbar shows the wallpaper, decoded from the
    # PNG the kernel embeds, in every pixel (the one under the parked
    # arrow's tip taken from the PNG): the golden is that PNG itself.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    assert "INFO  desktop: wallpaper: 1280x800, " in m.output, "the wallpaper wasn't decoded"
    close_window(m, "Colours")
    gui.wait_windows(m, 1)
    close_window(m, "Welcome")
    gui.wait_windows(m, 0)
    # The arrow's tip on the last row above the taskbar: the rest of it
    # falls on the taskbar, outside the comparison.
    top = gui.bounds(gui.taskbar(m)[0])[1]
    tip = (1279, top - 1)
    gui.pointer_move(m, *tip)
    assert gui.desktop_status(m)["wallpaper"] == "shown"
    shot = gui.settled_screendump(m, "wallpaper.png")
    with Image.open(shot) as dumped, Image.open(WALLPAPER) as wallpaper:
        actual, want = dumped.convert("RGB"), wallpaper.convert("RGB")
    assert actual.size == want.size == (1280, 800)
    actual.putpixel(tip, want.getpixel(tip))
    actual, want = actual.crop((0, 0, 1280, top)), want.crop((0, 0, 1280, top))
    diff, mask = compare(actual, want, tolerance=0)
    if diff.differing:
        actual.save(artifacts / "wallpaper.actual.png")
        mask.save(artifacts / "wallpaper.diff.png")
    assert diff.differing == 0, (
        f"{diff.differing} pixels differ from the wallpaper (see wallpaper.diff.png)"
    )
    assert gui.desktop_check(m) == (0, 0)


def type_x_into_colours(m: Machine) -> None:
    """Type `x` into the focused (Colours) window, which shows it as its
    last key."""
    gui.type_confirmed(m, "x")
    wait_keys(m, "Colours", 2)
    gui.wait_settled(m)


def test_dark_theme_golden(boot, artifacts, golden_dir):
    # `theme dark`, then the two-window desktop: the frames in the dark
    # palette and the windows' owners redrawing their client areas in it
    # (the golden: the Colours window still showing the key typed before
    # the switch), and `desktop check` 0 and 0. A name that isn't a theme
    # is refused.
    m = boot_shells(boot)
    gui.start_desktop(m)
    type_x_into_colours(m)
    assert run(m, "theme") == "theme: light\n"
    assert run(m, "theme dark") == "theme: dark\n"
    assert run(m, "theme dark") == "theme: dark (already)\n"
    assert run(m, "theme blue") == 'theme: no theme "blue" (light, dark)\n'
    status = gui.wait_settled(m)
    assert status["theme"] == "dark", status
    shot = gui.settled_screendump(m, "dark.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-dark.png",
        origin=(0, 0),
        size=(1280, 800),
        artifacts=artifacts,
        masks=gui.clock_mask(m),
    )
    assert gui.desktop_check(m) == (0, 0)


def test_theme_boot_flag_picks_dark(boot, artifacts, golden_dir):
    # `lumen.theme=dark`: said at boot, `theme` says so before the desktop
    # starts, and the desktop starts dark: the same screen as switching to
    # it (the dark golden, the same key typed).
    m = boot_shells(boot, cmdline="lumen.theme=dark")
    assert "INFO  desktop: theme dark (lumen.theme)" in m.output
    assert run(m, "theme") == "theme: dark\n"
    assert gui.desktop_status(m)["theme"] == "dark"
    gui.start_desktop(m)
    type_x_into_colours(m)
    shot = gui.settled_screendump(m, "dark-at-boot.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-dark.png",
        origin=(0, 0),
        size=(1280, 800),
        artifacts=artifacts,
        masks=gui.clock_mask(m),
    )


def test_a_theme_boot_flag_naming_no_theme_warns(boot):
    # `lumen.theme=` with a name that isn't a theme: a warning, and the
    # theme stays light.
    m = boot("qemutest", cmdline="lumen.theme=blue")
    m.expect(r"WARN  desktop: lumen.theme=blue: not a theme \(light, dark\), ignored\n", 60)
    m.expect(PROMPT, 60)
    assert run(m, "theme") == "theme: light\n"


# -- the toolkit and the automation tree (#156) ----------------------------------------


def start_counter(m: Machine) -> tuple[str, list[dict[str, str]]]:
    """`desktop counter`: its window's id, and its tree once published,
    the desktop settled."""
    wid = gui.desktop_counter(m)
    gui.wait_ui(m, "the Counter's tree", lambda r: any(n["name"] == "Quit" for n in r), window=wid)
    gui.wait_settled(m)
    return wid, gui.ui_tree(m, wid)


def count_of(m: Machine, wid: str) -> str:
    return gui.node(gui.ui_tree(m, wid), "label", "Count")["value"]


# ARCHITECTURE section 11's `ui tree` columns, in order.
UI_TREE_COLUMNS = [
    "id",
    "window",
    "depth",
    "role",
    "name",
    "value",
    "state",
    "actions",
    "x",
    "y",
    "w",
    "h",
]


def test_automation_tree_lists_buttons(boot):
    # The Counter window's tree: its root (role window, named by its
    # title, focused, in front), then its widgets; `where role = button`
    # finds Add, Reset and Quit by name, each taking press and focus, each
    # inside the window's client area on the screen. `ui tree` lists every
    # window's root, front to back, under section 11's columns. A back
    # window's root takes `focus`: raised, focused, and the Counter's root
    # no longer focused.
    m = boot_shells(boot)
    gui.start_desktop(m)
    wid, rows = start_counter(m)
    assert run(m, f"ui tree {wid}").splitlines()[0].split() == UI_TREE_COLUMNS
    root = rows[0]
    assert (root["role"], root["name"], root["depth"], root["state"], root["actions"]) == (
        "window",
        "Counter",
        "0",
        "focused",
        "focus,close,minimize,restore",
    ), root
    buttons = gui.parse_records(run(m, f"ui tree {wid} | where role = button"))
    assert [b["name"] for b in buttons] == ["Add", "Reset", "Quit"], buttons
    assert all(b["actions"] == "press,focus" and b["window"] == wid for b in buttons), buttons
    x, y, w, h = frame_of(gui.window(m, "Counter"))
    for b in buttons:
        bx, by, bw, bh = frame_of(b)
        assert x < bx and bx + bw < x + w and y + 24 < by and by + bh < y + h, (b, (x, y, w, h))
    count = gui.node(rows, "label", "Count")
    assert count["value"] == "0" and count["actions"] == "-", count
    lock = gui.node(rows, "checkbox", "Lock")
    assert lock["value"] == "unchecked", lock
    # Front to back: the taskbar (a panel, in front of every window), then
    # the windows.
    roots = [(r["role"], r["name"]) for r in gui.ui_tree(m) if r["depth"] == "0"]
    assert roots == [
        ("group", "Taskbar"),
        ("window", "Counter"),
        ("window", "Colours"),
        ("window", "Welcome"),
    ], roots
    assert run(m, "ui tree 99") == "ui tree: no window 99\n"
    welcome = gui.node(gui.ui_tree(m), "window", "Welcome")
    got = gui.invoke(m, welcome["id"], "focus")
    assert got.row is not None and (got.row["name"], got.row["state"]) == ("Welcome", "focused"), (
        got
    )
    front = gui.windows(m)[0]
    assert (front["title"], front["z"], front["focused"]) == ("Welcome", "0", "yes"), front
    assert gui.node(gui.ui_tree(m), "window", "Counter")["state"] == "-"
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)


def test_invoke_press_changes_state(boot, artifacts, golden_dir):
    # `ui invoke <Add> press` runs the button's own handler: the reply is
    # Add's row, the Count label's value is 1 in the tree, and the screen
    # shows it (a golden of the window, the pointer never drawn yet). A
    # real click on Add then (the confirming pointer steps) reaches the same
    # handler: 2.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    wid, rows = start_counter(m)
    add = gui.node(rows, "button", "Add")
    got = gui.invoke(m, add["id"], "press")
    assert got.row is not None and (got.row["id"], got.row["name"]) == (add["id"], "Add"), got
    assert not got.late
    # Published before it was answered: read at once, no poll.
    assert count_of(m, wid) == "1"
    frame = frame_of(gui.window(m, "Counter"))
    shot = gui.settled_screendump(m, "counter.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-counter-1.png",
        origin=frame[:2],
        size=frame[2:],
        artifacts=artifacts,
    )
    cx, cy = int(add["x"]) + int(add["w"]) // 2, int(add["y"]) + int(add["h"]) // 2
    gui.pointer_move(m, cx, cy)
    gui.pointer_down(m)
    gui.pointer_up(m)
    gui.wait_for(m, "the count at 2", lambda: count_of(m, wid), lambda v: v == "2")
    gui.pointer_move(m, 1200, 760)
    status = gui.wait_settled(m)
    # A press cancelled at its deadline (a loaded host) is retried by
    # `gui.invoke` and counted: never run, so the count is still exact.
    assert (status["cancelled"], status["late"]) == (str(got.retries), "0"), status
    assert gui.desktop_check(m) == (0, 0)


def test_invoke_on_a_closed_window_fails(boot):
    # Errors change nothing (an action a role doesn't list, a disabled
    # node, wrong arguments, a node that doesn't exist); `ui invoke <root>
    # close` closes the window through its owner, and its nodes then name
    # nothing.
    m = boot_shells(boot)
    assert run(m, "ui invoke 1 focus") == "ui invoke: no desktop\n"
    gui.start_desktop(m)
    wid, rows = start_counter(m)
    add, lock = gui.node(rows, "button", "Add"), gui.node(rows, "checkbox", "Lock")
    count = gui.node(rows, "label", "Count")
    assert run(m, f"ui invoke {rows[0]['id']} press") == "ui invoke: a window doesn't take press\n"
    assert run(m, f"ui invoke {add['id']} fly").startswith("ui invoke: no such action (press, ")
    # The owner's refusals (through `gui.invoke`, which retries a request
    # cancelled at its deadline), then the asker's own (wrong arguments).
    assert gui.invoke(m, count["id"], "press").message == "ui invoke: a label doesn't take press"
    got = gui.invoke(m, add["id"], "set_value", "3")
    assert got.message == "ui invoke: a button doesn't take set_value", got
    assert run(m, f"ui invoke {add['id']} press now").startswith("ui invoke: wrong arguments")
    assert gui.invoke(m, lock["id"], "press").row["value"] == "checked"
    assert gui.invoke(m, add["id"], "press").message == "ui invoke: the node is disabled"
    assert count_of(m, wid) == "0"
    root = rows[0]
    assert run(m, f"ui invoke {root['id']} close") == f"ui invoke: window {wid} closed\n"
    gui.wait_windows(m, 2)
    assert run(m, f"ui invoke {add['id']} press") == f"ui invoke: no node {add['id']}\n"
    assert run(m, f"ui invoke {root['id']} focus") == f"ui invoke: no node {root['id']}\n"
    assert all(r["window"] != wid for r in gui.ui_tree(m))
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)


def test_invoke_quit_closes_its_window(boot):
    # Quit's own handler closes its window: the reply says it ran and the
    # window closed, and the window is gone.
    m = boot_shells(boot)
    gui.start_desktop(m)
    wid, rows = start_counter(m)
    quit_button = gui.node(rows, "button", "Quit")
    got = gui.invoke(m, quit_button["id"], "press")
    assert got.message == f"ui invoke: ran; window {wid} closed", got
    # Answered once the window was gone: read at once, no poll.
    assert [w["title"] for w in gui.windows(m)] == ["Colours", "Welcome"]
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)


def test_a_toolkit_window_repaints_in_a_new_theme(boot):
    # Counter open: `theme dark` reaches its owner as a `Theme` event, and
    # it repaints its whole tree in the dark palette (its client background
    # the dark theme's `client`); back to light, the light one.
    m = boot_shells(boot)
    gui.start_desktop(m)
    _, rows = start_counter(m)
    x, y, w, h = frame_of(gui.window(m, "Counter"))
    # A pixel of Counter's client background: inside its frame, below the
    # last widget (Quit), clear of every widget.
    quit_button = gui.node(rows, "button", "Quit")
    _, qy, _, qh = frame_of(quit_button)
    probe = (x + w - 20, qy + qh + 4)
    assert probe[1] < y + h - 2, (probe, (x, y, w, h))
    light = pixel(gui.settled_screendump(m, "counter-light.png"), *probe)
    assert light == CLIENT_RGB, light
    assert run(m, "theme dark") == "theme: dark\n"
    gui.wait_settled(m)
    assert pixel(gui.settled_screendump(m, "counter-dark.png"), *probe) == DARK_CLIENT_RGB
    assert run(m, "theme light") == "theme: light\n"
    gui.wait_settled(m)
    assert pixel(gui.settled_screendump(m, "counter-light-again.png"), *probe) == CLIENT_RGB
    assert gui.desktop_check(m) == (0, 0)


# -- the desktop shell (#157) ---------------------------------------------------

# The bundled apps whose manifest grants `desktop` (#206: never a test app),
# but `life`, which the Life item starts (#202: listed once).
DESKTOP_APPS = ["calculator", "clock", "comet", "template", "viewer"]
LAUNCHER_ITEMS = ["Terminal", "Mandelbrot", "Life", "Counter", *DESKTOP_APPS]
# kernel/src/demo.rs `mandelbrot_owner`'s line.
MANDEL_WINDOW = re.compile(
    r"INFO  demo: mandelbrot window (\d+): (\d+) CPUs? in \d+ ms, rows by CPU:((?: cpu\d+=\d+)+)\n"
)
MANDEL_CLIENT = (480, 320)


def taskbar_buttons(m: Machine) -> list[str]:
    return [r["name"] for r in gui.taskbar(m) if r["role"] == "button"]


def minimize_box(frame: tuple[int, int, int, int]) -> tuple[int, int]:
    """The middle of a frame's minimize box (as big as the close box, 4
    pixels left of it)."""
    x, y = close_box(frame)
    return x - 16 - 4, y


def test_launcher_opens_terminal(boot):
    # The taskbar's launcher, through `ui invoke` then through confirming
    # clicks: `press` unfolds it (`expanded`, its four items after it in
    # the tree), its Terminal item opens a terminal window, focused in
    # front, and folds it; the window's button is on the taskbar as soon as
    # `ui invoke` returns (published before the answer). The same by clicks
    # on the launcher's head and on its item.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    bar = gui.taskbar(m)
    assert (bar[0]["role"], bar[0]["actions"], bar[0]["state"]) == ("group", "-", "-"), bar[0]
    assert taskbar_buttons(m) == ["Welcome", "Colours"]
    launcher = gui.node(bar, "menu", "Launcher")
    assert (launcher["state"], launcher["actions"]) == ("-", "press,focus"), launcher
    assert not [r for r in bar if r["role"] == "menuitem"]
    unfolded = gui.invoke(m, launcher["id"], "press")
    assert unfolded.row is not None and unfolded.row["state"] == "expanded", unfolded
    items = [r for r in gui.taskbar(m) if r["role"] == "menuitem"]
    assert [r["name"] for r in items] == LAUNCHER_ITEMS, items
    assert all(r["actions"] == "press" and r["depth"] == "2" for r in items), items
    entry = gui.node(items, "menuitem", "Terminal")
    got = gui.invoke(m, entry["id"], "press")
    assert got.message == f"ui invoke: ran; node {entry['id']} is gone", got
    # At once: the window, focused in front, and its button.
    rows = gui.windows(m)
    assert (rows[0]["title"], rows[0]["z"], rows[0]["focused"]) == ("Terminal-1", "0", "yes"), rows
    assert taskbar_buttons(m) == ["Welcome", "Colours", "Terminal-1"]
    assert not [r for r in gui.taskbar(m) if r["role"] == "menuitem"], "folded again"
    expect_window(m, int(rows[0]["id"]), r"^lumen>$", 30)
    # Clicks: the launcher's head, then its Terminal item.
    gui.click(m, *gui.centre(gui.node(gui.taskbar(m), "menu", "Launcher")))
    bar = gui.wait_for(
        m,
        "the launcher unfolded",
        lambda: gui.taskbar(m),
        lambda b: [r["name"] for r in b if r["role"] == "menuitem"] == LAUNCHER_ITEMS,
    )
    gui.click(m, *gui.centre(gui.node(bar, "menuitem", "Terminal")))
    rows = gui.wait_for(
        m, "Terminal-2 open", lambda: gui.windows(m), lambda w: w[0]["title"] == "Terminal-2"
    )
    assert rows[0]["focused"] == "yes", rows
    expect_window(m, int(rows[0]["id"]), r"^lumen>$", 30)
    gui.wait_for(
        m,
        "Terminal-2's button",
        lambda: taskbar_buttons(m),
        lambda b: b == ["Welcome", "Colours", "Terminal-1", "Terminal-2"],
    )
    gui.pointer_move(m, *PARK)


def test_taskbar_focuses_window(boot):
    # A click on the taskbar's button for the back window (Welcome) raises
    # and focuses it; `press` on Colours' button does the same for Colours,
    # its effect in `windows` as soon as `ui invoke` returns. The taskbar
    # never takes the focus (its root, a group, takes no action), and a
    # closed window's button goes (its node then names nothing). Ends with
    # the screen a full repaint would draw.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    bar = gui.taskbar(m)
    gui.click(m, *gui.centre(gui.node(bar, "button", "Welcome")))
    rows = gui.wait_for(
        m, "Welcome focused", lambda: gui.windows(m), lambda r: r[0]["title"] == "Welcome"
    )
    assert [(w["title"], w["z"], w["focused"]) for w in rows] == [
        ("Welcome", "0", "yes"),
        ("Colours", "1", "no"),
    ], rows
    got = gui.invoke(m, gui.node(bar, "button", "Colours")["id"], "press")
    assert got.row is not None and got.row["name"] == "Colours", got
    rows = gui.windows(m)
    assert [(w["title"], w["focused"]) for w in rows] == [("Colours", "yes"), ("Welcome", "no")]
    root = bar[0]
    assert run(m, f"ui invoke {root['id']} focus") == "ui invoke: a group doesn't take focus\n"
    assert (
        run(m, f"ui invoke {root['id']} minimize") == "ui invoke: a group doesn't take minimize\n"
    )
    # A closed window's button goes, and its node names nothing.
    welcome = gui.node(gui.taskbar(m), "button", "Welcome")
    close_window(m, "Welcome")
    gui.wait_for(m, "Welcome's button gone", lambda: taskbar_buttons(m), lambda b: b == ["Colours"])
    assert run(m, f"ui invoke {welcome['id']} press") == f"ui invoke: no node {welcome['id']}\n"
    gui.pointer_move(m, *PARK)
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)


def test_minimize_and_restore(boot):
    # Colours' minimize box: it leaves the screen (`minimized yes`, the
    # wallpaper where it was) and Welcome gets the focus; its taskbar
    # button brings it back, raised and focused. Then the root's
    # `minimize` and `restore` through `ui invoke`, each refused when there
    # is nothing to do. Each step ends with the screen a full repaint
    # would draw.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    wid = gui.window(m, "Colours")["id"]
    gui.click(m, *minimize_box(COLOURS))
    rows = gui.wait_for(
        m,
        "Colours minimized",
        lambda: gui.windows(m),
        lambda r: gui.one(r, "Colours rows", title="Colours")["minimized"] == "yes",
    )
    assert [(w["title"], w["focused"], w["minimized"]) for w in rows] == [
        ("Colours", "no", "yes"),
        ("Welcome", "yes", "no"),
    ], rows
    assert gui.desktop_status(m)["windows"] == "2"
    gui.pointer_move(m, *PARK)
    shot = gui.settled_screendump(m, "minimized.png")
    # Inside where Colours was and clear of Welcome: the wallpaper.
    for x, y in [(700, 450), (COLOURS[0] + COLOURS[2] - 2, COLOURS[1] + 2)]:
        assert pixel(shot, x, y) == wallpaper_rgb(x, y), (x, y)
    assert gui.desktop_check(m) == (0, 0)
    # Its taskbar button restores it.
    gui.click(m, *gui.centre(gui.node(gui.taskbar(m), "button", "Colours")))
    rows = gui.wait_for(
        m, "Colours restored", lambda: gui.windows(m), lambda r: r[0]["title"] == "Colours"
    )
    assert [(w["title"], w["z"], w["focused"], w["minimized"]) for w in rows] == [
        ("Colours", "0", "yes", "no"),
        ("Welcome", "1", "no", "no"),
    ], rows
    assert frame_of(rows[0]) == COLOURS
    gui.pointer_move(m, *PARK)
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)
    # The root's actions.
    root = gui.node(gui.ui_tree(m), "window", "Colours")
    assert root["actions"] == "focus,close,minimize,restore", root
    got = gui.invoke(m, root["id"], "minimize")
    assert got.row is not None and got.row["state"] == "-", got
    assert gui.window(m, "Colours")["minimized"] == "yes"
    assert (
        run(m, f"ui invoke {root['id']} minimize")
        == f"ui invoke: window {wid} is minimized already\n"
    )
    got = gui.invoke(m, root["id"], "restore")
    assert got.row is not None and got.row["state"] == "focused", got
    assert (gui.window(m, "Colours")["minimized"], gui.windows(m)[0]["title"]) == ("no", "Colours")
    assert run(m, f"ui invoke {root['id']} restore") == f"ui invoke: window {wid} isn't minimized\n"
    gui.wait_settled(m)
    assert gui.desktop_check(m) == (0, 0)


def test_mandelbrot_window_golden(boot, artifacts, golden_dir):
    # The launcher's Mandelbrot with four CPUs: the window's surface lent as
    # bands to a thread on each (`sched::scope`), every CPU drawing rows and
    # every row drawn once (its line on serial); the client area against a
    # golden (the same pixels as `mandelbrot` on one CPU: View::fit at
    # 64 iterations); its owner waits without animating (the desktop
    # settles), and closing the window ends it.
    m = boot_shells(boot, extra=["-smp", "4"])
    assert "INFO  smp: 4 CPUs online" in m.output
    gui.start_desktop(m)
    gui.launch(m, "Mandelbrot")
    found = gui.wait_for(
        m, "the Mandelbrot window drawn", lambda: MANDEL_WINDOW.search(m.output), bool, 60
    )
    rows = {int(cpu): int(n) for cpu, n in re.findall(r"cpu(\d+)=(\d+)", found[3])}
    assert int(found[2]) == 4 and sorted(rows) == [0, 1, 2, 3], found[0]
    assert sum(rows.values()) == MANDEL_CLIENT[1], found[0]
    window = gui.window(m, "Mandelbrot")
    assert window["id"] == found[1] and window["focused"] == "yes", window
    x, y, _, _ = frame_of(window)
    shot = gui.settled_screendump(m, "mandelbrot.png")
    assert_matches_golden(
        shot,
        golden_dir / "desktop-mandelbrot-window.png",
        origin=(x + 1, y + TITLE_H),
        size=MANDEL_CLIENT,
        artifacts=artifacts,
    )
    assert gui.desktop_check(m) == (0, 0)
    root = gui.node(gui.ui_tree(m), "window", "Mandelbrot")
    assert run(m, f"ui invoke {root['id']} close") == f"ui invoke: window {found[1]} closed\n"
    gui.wait_for(
        m, "its owner gone", lambda: thread_names(m), lambda t: "desktop-mandelbrot" not in t
    )
    assert "Mandelbrot" not in taskbar_buttons(m)


def test_launcher_lists_desktop_apps_only(boot):
    # The launcher's items: the demos, then the bundled apps granting
    # `desktop`, never a test app (`window` grants it too). Choosing one
    # starts it through `run`'s path: `apps` lists it and its window opens;
    # its close ends it.
    m = boot_shells(boot)
    gui.start_desktop(m)
    launcher = gui.node(gui.taskbar(m), "menu", "Launcher")
    assert gui.invoke(m, launcher["id"], "press").row is not None
    items = [r["name"] for r in gui.taskbar(m) if r["role"] == "menuitem"]
    assert items == LAUNCHER_ITEMS, items
    entry = gui.node(gui.taskbar(m), "menuitem", "template")
    got = gui.invoke(m, entry["id"], "press")
    assert got.message == f"ui invoke: ran; node {entry['id']} is gone", got
    listed = gui.wait_for(
        m,
        "apps listing template",
        lambda: gui.parse_records(run(m, "apps"), "id"),
        lambda rows: any(r["name"] == "template" for r in rows),
    )
    assert [r["name"] for r in listed] == ["template"], listed
    gui.wait_window(m, "template")
    gui.close_window(m, "template")
    gui.wait_for(
        m,
        "template ended",
        lambda: gui.parse_records(run(m, "apps"), "id"),
        lambda rows: not rows,
        30,
    )


def plus_minute(hhmm: str) -> str:
    h, mm = map(int, hhmm.split(":"))
    t = (h * 60 + mm + 1) % (24 * 60)
    return f"{t // 60:02}:{t % 60:02}"


def test_taskbar_clock_matches_rtc(boot):
    # The taskbar's Clock (a fixed-size label) shows `date`'s hour and
    # minute (polled until they agree: the clock reads the RTC every
    # second). `desktop tick` moves it a minute ahead: the redraw is one
    # present of one rectangle, the label's bounds, and nothing else.
    m = boot_shells(boot)
    gui.start_desktop(m)

    def date_and_clock() -> tuple[str, str]:
        found = re.fullmatch(r"date: \d{4}-\d\d-\d\d (\d\d:\d\d):\d\d UTC\n", run(m, "date"))
        assert found, "date's format"
        return found[1], gui.clock_text(m)

    gui.wait_for(m, "the clock showing date's minute", date_and_clock, lambda dc: dc[0] == dc[1])
    clock = gui.clock(m)
    _, _, w, h = gui.bounds(clock)
    assert (clock["role"], clock["actions"]) == ("label", "-")
    for _ in range(3):
        shown = gui.clock_text(m)
        before = gui.wait_settled(m)
        assert run(m, "desktop tick") == "desktop tick: the clock moved a minute ahead\n"
        gui.wait_for(m, "the clock moved", lambda: gui.clock_text(m), lambda t, s=shown: t != s)
        after = gui.wait_settled(m)
        if gui.clock_text(m) != plus_minute(shown):
            continue  # a real minute turned as well: measure again
        assert gui.bounds(gui.clock(m)) == gui.bounds(clock)
        presented = {k: int(after[k]) - int(before[k]) for k in ("frames", "rects", "pixels")}
        assert presented == {"frames": 1, "rects": 1, "pixels": w * h}, presented
        break
    else:
        raise AssertionError("a real minute turned during each of 3 ticks")


def test_a_window_opened_from_the_shell_gets_a_taskbar_button(boot):
    # Windows the launcher didn't open (`desktop counter` on the serial
    # shell) get their button too: the open pokes the taskbar. Then Counter
    # through the launcher: a second button of the same name.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    assert taskbar_buttons(m) == ["Welcome", "Colours"]
    gui.desktop_counter(m)
    gui.wait_for(
        m,
        "Counter's button",
        lambda: taskbar_buttons(m),
        lambda b: b == ["Welcome", "Colours", "Counter"],
    )
    gui.launch(m, "Counter")
    assert taskbar_buttons(m) == ["Welcome", "Colours", "Counter", "Counter"]
    assert [w["title"] for w in gui.windows(m)][:2] == ["Counter", "Counter"]
    assert gui.desktop_status(m)["taskbar"] == "yes"


def test_a_launch_while_paused_is_refused(boot):
    # Alt+F1, then each launcher item in turn (the launcher unfolded first
    # each time: choosing one folds it): `press` is refused as `terminal`
    # is while the desktop is paused, `ui invoke` saying why, and no window
    # opens.
    m = boot_tablet(boot)
    gui.start_desktop(m)
    gui.confirmed_keys(m, ["alt", "f1"])
    gui.wait_desktop_state(m, "paused")
    for item in LAUNCHER_ITEMS:
        launcher = gui.node(gui.taskbar(m), "menu", "Launcher")
        assert gui.invoke(m, launcher["id"], "press").row is not None
        entry = gui.node(gui.taskbar(m), "menuitem", item)
        warn = rf"WARN  taskbar: {item}: desktop: paused"
        got = gui.invoke(m, entry["id"], "press", allow=warn)
        assert got.message == f"ui invoke: {item}: desktop: paused (`desktop` resumes it)", got
    assert [w["title"] for w in gui.windows(m)] == ["Colours", "Welcome"]
    assert run(m, "desktop", 30) == "desktop: resumed\n"
    assert taskbar_buttons(m) == ["Welcome", "Colours"]


def test_the_launcher_answers_with_the_new_window_s_button(boot):
    # With the taskbar's handling of a poke held back 2 s
    # (`lumen.taskbar_wake_delay_ms`), the new window's button is there as
    # soon as `ui invoke` on the launcher's item returns: the launcher
    # brings the buttons in line itself before it answers, not the poke.
    m = boot_tablet(boot, cmdline="lumen.taskbar_wake_delay_ms=2000")
    gui.start_desktop(m)
    said = "INFO  taskbar: waiting 2000 ms after each poke (lumen.taskbar_wake_delay_ms)"
    assert said in m.output, "the knob wasn't taken"
    gui.launch(m, "Counter")
    assert taskbar_buttons(m) == ["Welcome", "Colours", "Counter"]
