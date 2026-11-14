"""rhai scripts in the shell (#67): `rhai -e`, the REPL, the built-in demos,
drawing on the screen, and Ctrl-C stopping a runaway script on either
console. The engine's limits (operations, call depth, expression depth) are
ktests in kernel/src/script.rs."""

import re
import time

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine
from lumen_harness.screen import PROMPT, expect_screen
from PIL import Image

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
# Where the canvas starts: below the header (kernel/src/fb.rs CONSOLE_TOP).
CANVAS_TOP = 144


def shells(boot, extra: tuple[str, ...] = ()) -> Machine:
    m = boot("qemutest", extra=extra)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str, timeout: float = 30) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, timeout)
    return m.output[echo.end() : end.start()]


def test_rhai_eval(boot):
    m = shells(boot, ("-smp", "4"))
    assert run(m, "rhai -e 'cpus().len()'") == "4\n"
    assert run(m, "rhai -e 'cpus().map(|c| c.index)'") == "[0, 1, 2, 3]\n"
    # The rest of the machine's functions, and a value of each kind.
    assert run(
        m, "rhai -e 'let m = mem(); print(m.heap_used + m.heap_free == m.heap_size); uptime() > 0'"
    ) == ("true\ntrue\n")
    assert run(m, "rhai -e 'trace(2).len()'") == "2\n"
    assert run(m, "rhai -e '\"a\" + 1'") == "a1\n"
    assert run(m, "rhai -e 'let x = 1'") == ""
    # An error is one line; so is a word the command doesn't know.
    assert (
        run(m, "rhai -e 'frobnicate()'")
        == "rhai: Function not found: frobnicate () (line 1, position 1)\n"
    )
    assert run(m, "rhai -x") == "usage: rhai [-e <expr> | demo [name]]\n"
    # A map holding one 60 KB string 8000 times (added by item assignment,
    # which rhai counts only as the items) made into text: an error at a
    # size limit, not 480 MB of text.
    big = run(
        m,
        'rhai -e \'let s = "x"; s.pad(60000, "x"); let m = #{}; '
        'for i in 0..8000 { m["k" + i] = s; } m.to_string().len()\'',
        timeout=60,
    )
    assert big.startswith("rhai: more than ") and big.count("\n") == 1, big
    # The built-in scripts.
    demos = run(m, "rhai demo")
    assert demos.startswith("demos (rhai demo <name>):\n  rainbow ")
    assert "  fib " in demos and "  count " in demos
    assert run(m, "rhai demo fib").endswith("fib(9) = 34\nfib(10) = 55\n")
    assert run(m, "rhai demo nope") == "rhai: no demo called nope (try 'rhai demo')\n"


def test_rhai_repl(boot):
    # `rhai` alone keeps variables from line to line until `exit`.
    m = shells(boot)
    m.send_line("rhai")
    m.expect(r"rhai: Lumen's functions are cpus\(\), mem\(\), .*; 'exit' or Ctrl-D leaves\n", 10)
    m.expect(r"rhai> ", 10)
    m.send_line("let x = 6 * 7")
    m.expect(r"let x = 6 \* 7\nrhai> ", 10)
    m.send_line("x * 2")
    m.expect(r"x \* 2\n84\nrhai> ", 10)
    m.send_line("exit")
    m.expect(r"exit\n" + PROMPT, 10)
    assert run(m, "echo back") == "back\n"


def test_script_draws_golden(boot, artifacts, golden_dir):
    # The `rainbow` demo draws bars, boxes, a pixel checkerboard and text on
    # the canvas below the header, and keeps it until a key; then the
    # terminal is drawn again (its lines from before are back).
    m = shells(boot, ("-smp", "2"))
    m.send_line("rhai demo rainbow")
    m.expect(r"rhai demo rainbow\nrhai: press a key on serial\n", 30)
    shot = m.screendump("rainbow.png")
    width, height = Image.open(shot).size
    assert_matches_golden(
        shot,
        golden_dir / "script-rainbow.png",
        origin=(0, CANVAS_TOP),
        size=(width, height - CANVAS_TOP),
        artifacts=artifacts,
    )
    m.send(" ")
    m.expect(PROMPT, 10)
    expect_screen(m, r"^INFO  shell: shells on serial and the screen; type 'help'$")


def test_runaway_script_is_stopped(boot):
    # A loop that never ends stops at Ctrl-C, on serial and on the screen,
    # and the shell goes on.
    m = shells(boot)
    m.send_line("rhai -e 'let n = 0; loop { n += 1 }'")
    echo = m.expect(r"loop \{ n \+= 1 \}'\n", 10)
    # Still running a second later (nothing printed: not stopped by itself).
    time.sleep(1)
    assert m.output[echo.end() :] == ""
    m.send("\x03")
    m.expect(r"rhai: stopped \(Ctrl-C\)\n" + PROMPT, 10)
    assert run(m, "echo still here") == "still here\n"
    # The same on the screen's shell, with Ctrl-C on the keyboard; and
    # `sleep` stops too.
    m.type_text("rhai -e 'loop { sleep(1000) }'\n")
    expect_screen(m, r"^lumen> rhai -e 'loop \{ sleep\(1000\) \}'$")
    m.press(["ctrl", "c"])
    expect_screen(m, r"^rhai: stopped \(Ctrl-C\)\nlumen>$")


def test_screen_shell_scripts(boot, artifacts):
    # On the screen's shell: a value whose formatting allocates (a function
    # pointer's `Debug`) is printed: the screen console formats each write
    # before taking the screen's lock, under which the heap would panic;
    # the screen's size
    # doesn't take the screen; a script that drew shows "press a key" on a
    # strip at the bottom of what it drew, and a key brings the terminal
    # back.
    m = shells(boot)
    m.type_text('rhai -e \'[Fn("cpus"), #{f: Fn("x")}]\'\n')
    expect_screen(m, r'^\[Fn\("cpus"\), #\{"f": Fn\("x"\)\}\]\nlumen>$')
    m.type_text("rhai -e 'screen_width()'\n")
    expect_screen(m, r"^1280\nlumen>$")
    m.type_text("rhai -e 'draw_rect(0, 0, 100, 100, 0x00ff00)'\n")
    # (The terminal holds the line, drawn once the screen is given back.)
    expect_screen(m, r"^rhai: press a key on the keyboard$")
    shot = m.screendump("drawn.png")
    img = Image.open(shot).convert("RGB")
    width, height = img.size
    assert img.getpixel((50, CANVAS_TOP + 50)) == (0, 255, 0)
    assert img.getpixel((width - 4, height - 4)) == (0x20, 0x20, 0x20)
    # The strip's text: some pixels lighter than the strip.
    strip = [img.getpixel((x, y)) for x in range(8, 150) for y in range(height - 20, height - 4)]
    assert any(p[0] > 0x80 for p in strip)
    m.type_text(" ")
    expect_screen(m, r"^rhai: press a key on the keyboard\nlumen>$")
