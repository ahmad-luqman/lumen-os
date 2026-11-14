"""The shell: one on serial (plain text unless `lumen.ansi`) and one on the
screen (typed on the keyboard, read back with the serial shell's `term
text`), each with its own line editing, history and completion. The line
editor, word splitting and completion are tested on the host
(`cargo test -p lumen-shell`); this checks them in the kernel, through
both consoles."""

import re

import pytest
from lumen_harness.qemu import Machine
from lumen_harness.screen import PROMPT, expect_screen, read_screen

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"


def shells(boot, cmdline: str | None = None) -> Machine:
    m = boot("qemutest", cmdline=cmdline)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, 10)
    return m.output[echo.end() : end.start()]


def keys(m: Machine, *presses: str | list[str]) -> None:
    """Type on the keyboard: a string is typed as text, a list is keys
    pressed together (QEMU's names: `left`, `home`, `ctrl` and `a`...)."""
    for p in presses:
        if isinstance(p, str):
            m.type_text(p)
        else:
            m.press(p)


def test_echo(boot):
    # Quoting as in a POSIX shell, on both consoles; the serial shell echoes
    # what is typed, in plain text.
    m = shells(boot)
    start = len(m.output)
    assert run(m, "echo 'a  b' \"c d\" e\\ f  g") == "a  b c d e f g\n"
    assert run(m, "echo one # a comment") == "one\n"
    assert run(m, "echo '' x") == " x\n"
    keys(m, "echo 'x  y' \"\\$HOME\" z\n")
    expect_screen(m, r"^lumen> echo 'x  y' \"\\\$HOME\" z\nx  y \$HOME z$")
    assert "\x1b" not in m.output[start:]


BOOTING = r"INFO  boot: Lumen \d+\.\d+\.\d+ \(x86_64\) booting"


def reboots(m: Machine, method: str) -> None:
    """Run `reboot` in the serial shell of `m` (booted with reboot=True) and
    check it reset by `method` (its log line; no later method's) and booted
    to the shells again."""
    m.expect(BOOTING, 60)
    shells_ready = m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    m.qmp()  # connected before the reset, so it gets the RESET event
    m.send_line("reboot")
    m.expect(r"reboot: restarting\n", 10)
    m.expect(rf"INFO  power: rebooting: {method}\n", 10)
    assert m.wait_event("RESET", 30)["data"]["reason"] == "guest-reset"
    m.expect(BOOTING, 120)
    m.expect(SHELLS, 60)
    out = m.output
    banners = list(re.finditer(BOOTING, out))
    assert len(banners) == 2
    # QEMU reports every guest reset the same way (guest-reset), so the
    # method that did it is told apart by the log: no later method started.
    first_boot = out[shells_ready.end() : banners[1].start()]
    assert "INFO  power: rebooting:" in first_boot
    assert len(re.findall(r"INFO  power: rebooting: ", first_boot)) == 1, first_boot
    assert "didn't reset the machine" not in first_boot
    assert m.proc is not None and m.proc.poll() is None, "QEMU ended instead of rebooting"


def test_reboot_boots_again(boot):
    # `reboot` resets the machine through port 0xcf9 and it boots to the
    # shells again. QEMU runs without -no-reboot here, so the reset doesn't
    # end it.
    reboots(boot("qemutest", reboot=True), "reset through port 0xcf9")


@pytest.mark.parametrize(
    ("first", "method"),
    [("8042", "pulsing the 8042's reset line"), ("triple", "triple fault")],
    ids=["8042", "triple"],
)
def test_reboot_fallbacks_boot_again(boot, first, method):
    # The fallbacks, started with `lumen.reboot=` (q35 always takes 0xcf9's
    # reset, so they wouldn't run otherwise).
    reboots(boot("qemutest", reboot=True, cmdline=f"lumen.reboot={first}"), method)


def test_shutdown_powers_off(boot):
    # `shutdown` announces itself, enters ACPI S5 and QEMU exits because the
    # guest powered off: the SHUTDOWN event says `guest-shutdown` (a triple
    # fault under -no-reboot also ends QEMU with status 0, but its reason is
    # `guest-reset`). Status 0 is a pass only here. OVMF leaves q35 in ACPI
    # mode, so no switch to it is needed.
    m = shells(boot)
    m.qmp()  # connected before the power-off, so it gets the SHUTDOWN event
    start = len(m.output)
    m.send_line("shutdown")
    m.expect(r"shutdown: powering off\n", 10)
    m.expect(r"INFO  power: powering off: ACPI S5, SLP_TYP 0/0 into PM1a control 0x604\n", 10)
    assert m.wait_event("SHUTDOWN", 30)["data"] == {"guest": True, "reason": "guest-shutdown"}
    assert m.wait_exit(30) == 0
    after = m.output[start:]
    assert "can't power off" not in after
    assert "ACPI mode" not in after


def test_history_and_line_editing(boot):
    # On the screen (QMP keys): history with Up/Down, the cursor keys,
    # Home/End, Ctrl-A/E/K/U/W/C; `history` lists the lines entered there.
    m = shells(boot)
    keys(m, "echo one\n", "echo two\n", ["up"], ["up"], ["ret"])
    expect_screen(m, r"^two\nlumen> echo one\none\nlumen>$")
    # Down goes back to a newer line ("echo one" again: a repeat of the
    # newest, not kept twice).
    keys(m, ["up"], ["up"], ["down"], ["ret"])
    expect_screen(m, r"^lumen> echo one\none\nlumen> echo one\none\nlumen>$")
    # Delete takes the character at the cursor, Right moves past one.
    keys(m, "echo abXc", ["left"], ["left"], ["delete"], ["right"], "d\n")
    expect_screen(m, r"^lumen> echo abcd\nabcd$")
    # Edits anywhere in the line: "cho wrld" becomes "echo world".
    keys(m, "cho wrld", ["home"], "e", ["end"], ["left"], ["left"], ["left"], "o")
    keys(m, ["ctrl", "e"], " xyz", ["ctrl", "w"], ["ret"])
    # (The screen's rows come without trailing blanks.)
    expect_screen(m, r"^lumen> echo world\nworld$")
    # Ctrl-U deletes before the cursor, Ctrl-K after it.
    keys(m, "junk echo ", *[["left"]] * 5, ["ctrl", "u"], ["end"], "kept\n")
    expect_screen(m, r"^lumen> echo kept\nkept$")
    # Ctrl-K deletes after the cursor; Ctrl-A goes to the start and Ctrl-F
    # one on ("eho cut" becomes "echo cut" only if both move).
    keys(m, "eho cut junk", *[["left"]] * 5, ["ctrl", "k"], ["ctrl", "a"], ["ctrl", "f"], "c\n")
    expect_screen(m, r"^lumen> echo cut\ncut$")
    # Ctrl-C drops the line; nothing runs.
    keys(m, "echo dropped", ["ctrl", "c"], "echo after\n")
    expect_screen(m, r"^lumen> echo dropped\^C\nlumen> echo after\nafter$")
    assert not re.search(r"^dropped$", read_screen(m).text, re.MULTILINE)
    keys(m, "history\n")
    expect_screen(
        m,
        r"^ +1  echo one\n +2  echo two\n +3  echo one\n +4  echo abcd\n +5  echo world\n"
        r" +6  echo kept\n +7  echo cut\n +8  echo after\n +9  history$",
    )
    # The serial shell has its own history: Up recalls its last line, in
    # plain text: what was typed erased, the line typed instead, no escape
    # codes.
    start = len(m.output)
    assert run(m, "echo serial") == "serial\n"
    m.send("xyz\x1b[A\r")
    m.expect(r"xyz(\x08 \x08){3}echo serial\nserial\n" + PROMPT, 10)
    assert "\x1b" not in m.output[start:]
    # (The recalled line repeats the last one: kept once. The serial history
    # also has the `term text` lines reading the screen ran.)
    listed = [line.split(maxsplit=1)[1] for line in run(m, "history").splitlines()]
    assert listed[-2:] == ["echo serial", "history"] and listed.count("echo serial") == 1, listed


def test_tab_completion(boot):
    # Tab completes command names and their arguments: a word that fits in
    # full (and a space), several up to where they differ or, if they
    # already do, listed.
    m = shells(boot)
    keys(m, "ec", ["tab"], "hi\n")
    expect_screen(m, r"^lumen> echo hi\nhi$")
    keys(m, "crash p", ["tab"])
    expect_screen(m, r"^lumen> crash p\npanic  pf\nlumen> crash p$")
    keys(m, "a", ["tab"])
    expect_screen(m, r"^lumen> crash panic$")
    keys(m, ["ctrl", "u"], "te", ["tab"], ["tab"])
    expect_screen(m, r"^lumen> term text$")
    keys(m, ["ctrl", "u"], "cl", ["tab"])
    expect_screen(m, r"^clear  clocks\nlumen> cl$")
    keys(m, "o", ["tab"])
    expect_screen(m, r"^lumen> clocks$")
    # On serial, in plain text: a completion is appended, a listing is a
    # line of its own followed by the prompt and the line again (the serial
    # shell's prompt was the last thing read).
    start = len(m.output)
    m.send("upt\t\r")
    m.expect(r"uptime \nuptime: \d+ ns\n" + PROMPT, 10)
    m.send("cr\tp\t\x15\r")
    m.expect(r"crash p\npanic  pf\nlumen> crash p(\x08 \x08){7}\n" + PROMPT, 10)
    # One option the start of another: "df" is all they share.
    m.send("crash df\t\x15\r")
    m.expect(r"crash df\ndf  dfree\nlumen> crash df(\x08 \x08){8}\n" + PROMPT, 10)
    assert "\x1b" not in m.output[start:]


def test_unknown_command_single_error(boot):
    # An unknown command, or a line that can't be split, is one line of
    # error on the console it was typed on, and nothing else.
    m = shells(boot)
    assert run(m, "frobnicate now") == "unknown command: frobnicate (try 'help')\n"
    # A line longer than the console takes (160 columns) isn't run cut
    # short: the shell says so.
    m.send_line("echo " + "x" * 160)
    m.expect(r"echo x{155}\nline too long \(more than 160 columns\): not run\n" + PROMPT, 10)
    # (`help` lists every command: a ktest checks the table against it.)
    assert run(m, "help").startswith("commands:\n  help ")
    assert run(m, "echo 'open") == "syntax error: unclosed ' quote\n"
    assert run(m, "echo end\\") == "syntax error: a backslash at the end of the line\n"
    start = len(m.output)
    keys(m, "frob\n")
    expect_screen(m, r"^lumen> frob\nunknown command: frob \(try 'help'\)\nlumen>$")
    # Nothing about it on serial but the screen's rows.
    assert not re.search(r"^unknown", m.output[start:], re.MULTILINE)


def test_serial_takes_escape_codes_with_lumen_ansi(boot):
    # With `lumen.ansi` the serial shell edits like the screen's: the cursor
    # keys move in the line (the redraw uses escape codes), and the colour
    # pattern and `clear` work there; without it, they say why not.
    plain = shells(boot)
    assert run(plain, "colors") == (
        "colors: this console is plain text (boot with lumen.ansi for escape codes)\n"
    )
    assert run(plain, "clear").startswith("clear: this console is plain text")
    m = shells(boot, cmdline="lumen.ansi")
    start = len(m.output)
    m.send("echo abd\x1b[Dc\x1b[H\x1b[3~e\r")
    m.expect(r"\nabcd\n" + PROMPT, 10)  # (the harness reads CR LF as LF)
    assert "\x1b[K" in m.output[start:]
    m.send_line("colors")
    m.expect(re.compile(r"\x1b\[48;5;16m.*colors: done\n", re.DOTALL), 10)
    m.send_line("clear")
    m.expect(r"clear\n\x1b\[H\x1b\[2J\x1b\[3J" + PROMPT, 10)
