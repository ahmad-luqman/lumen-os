"""The screen as the kernel sees it (`screenshot`, #150); the desktop's
scenarios (M4) join it here."""

from __future__ import annotations

import time

import pytest
from lumen_harness.qemu import Machine
from lumen_harness.screen import expect_screen, read_screen
from lumen_harness.screenshot import FrameError, assert_matches_screendump
from lumen_harness.shell import PROMPT, run

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
