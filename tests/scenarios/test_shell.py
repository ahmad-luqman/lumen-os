"""The shell: one on serial (plain text unless `lumen.ansi`) and one on
each virtual terminal on the screen (typed on the keyboard, read back with
the serial shell's `term text`), each with its own line editing, history and
completion. The line
editor, word splitting and completion are tested on the host
(`cargo test -p lumen-shell`); this checks them in the kernel, through
both consoles."""

import re
from collections.abc import Sequence

import pytest
from lumen_harness.gui import confirmed_keys
from lumen_harness.klog import SECONDS, is_log_line
from lumen_harness.qemu import NO_DEVICES, Devices, Machine, default_accel
from lumen_harness.screen import PROMPT, expect_screen, read_screen
from lumen_harness.shell import read_output, run, run_dmesg, wait_for_output

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"


def shells(
    boot, cmdline: str | None = None, extra: Sequence[str] = (), devices: Devices = NO_DEVICES
) -> Machine:
    m = boot("qemutest", cmdline=cmdline, extra=extra, devices=devices)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def test_irqs_lists_msix(boot):
    # virtio-rng's MSI-X vector (#96): `irqs` names it by its function and
    # table entry (`msix <pci address> #0`), with a non-zero count (the
    # probe's request already came back through it), and a request
    # (`random`) makes it fire again. No MSI vector is listed as plain
    # `MSI n` (nothing raised one no device claimed).
    from test_pci import msix_rows

    m = boot("qemutest", devices=Devices(rng=True))
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    rows = msix_rows(m)
    assert len(rows) == 1, rows
    [(addr, (vector, entry, before))] = rows.items()
    assert 0x90 <= vector <= 0xEF and entry == 0, rows
    assert before > 0
    lspci = run(m, "lspci | where name = 'virtio rng' | select addr")
    assert addr in lspci, lspci
    assert re.fullmatch(r"[0-9a-f]{64}\n", run(m, "random 32"))
    after = msix_rows(m)
    assert after.keys() == {addr} and after[addr][:2] == (vector, 0) and after[addr][2] > before, (
        after
    )
    assert "interrupt never came" not in m.output
    assert run(m, "irqs | where use ~ 'MSI ' | count") == "0\n"


def test_echo(boot):
    # Quoting as in a POSIX shell, on both consoles; the serial shell echoes
    # what is typed, in plain text.
    m = shells(boot)
    start = len(m.output)
    assert run(m, "echo 'a  b' \"c d\" e\\ f  g") == "a  b c d e f g\n"
    assert run(m, "echo one # a comment") == "one\n"
    assert run(m, "echo '' x") == " x\n"
    confirmed_keys(m, "echo 'x  y' \"\\$HOME\" z\n")
    expect_screen(m, r"^lumen> echo 'x  y' \"\\\$HOME\" z\nx  y \$HOME z$")
    assert "\x1b" not in m.output[start:]


# Logged (debug: serial only) when a screen's view moves (test_term).
VIEW_MOVED = rf'\[{SECONDS}\] DEBUG term: view \d+ lines up, top row "[^"]*"\n'
# Logged (debug) at the tablet's first event it has no use for: a wheel
# click's release is BTN_GEAR_UP, 0x151 (QEMU's keymap_button), not a
# button the tablet uses.
IGNORED = (
    rf"\[{SECONDS}\] DEBUG mouse: \S+: ignoring events a tablet has no use for, "
    r"first type 0x1 code 0x151\n"
)


def scrolled_back_ready(boot) -> Machine:
    """Shells at debug level, the screen's shell with lines to scroll back
    over (`seq 100`): Shift+PageUp then logs its move on serial."""
    m = shells(boot, cmdline="lumen.log=debug")
    confirmed_keys(m, "seq 100\n")
    expect_screen(m, r"^100\nlumen>$")
    return m


def test_a_log_line_inside_a_commands_echo_is_not_output(boot):
    # The serial shell echoes each key in its own write, and a log line can
    # land between two of them (#124): here Shift+PageUp on the screen (its
    # view moves over `seq 100`'s lines), logged at debug, comes between
    # `echo hel` and `lo` typed on serial. The command's output, as
    # lumen_harness.shell reads it, is without it; the line was there.
    m = scrolled_back_ready(boot)
    start = len(m.output)
    m.send("echo hel")
    wait_for_output(m, start, r"echo hel$")
    # unconfirmed: a command is half typed on serial, so no `kbd`; its log
    # line, read next, confirms it
    m.press(["shift", "pgup"])
    wait_for_output(m, start, VIEW_MOVED)
    m.send("lo\r")
    out = read_output(m, "echo hello")
    assert out == "hello\n" and re.fullmatch(VIEW_MOVED, out.logs[-1] + "\n"), (out, out.logs)
    assert re.match(rf"echo hel{VIEW_MOVED}lo\nhello\n{PROMPT}", m.output[start:]), m.output[start:]


def test_a_log_line_between_output_writes_is_not_output(boot):
    # A command's output can be several writes (rhai's each `print`, then
    # the stop): Shift+PageUp's debug line lands after `a` is printed,
    # while the script waits, and before Ctrl-C stops it.
    m = scrolled_back_ready(boot)
    start = len(m.output)
    line = "rhai -e 'print(\"a\"); loop { sleep(1000) }'"
    m.send_line(line)
    wait_for_output(m, start, r"\na\n$")
    # unconfirmed: a script runs on serial, so no `kbd`; its log line, read
    # next, confirms it
    m.press(["shift", "pgup"])
    wait_for_output(m, start, VIEW_MOVED)
    m.send("\x03")
    out = read_output(m, line)
    assert out == "a\nrhai: stopped (Ctrl-C)\n", (out, out.logs)
    assert re.fullmatch(VIEW_MOVED, out.logs[-1] + "\n"), out.logs
    assert re.search(rf"\na\n{VIEW_MOVED}rhai: stopped", m.output[start:]), m.output[start:]


def test_a_log_line_after_the_output_before_the_prompt_is_not_output(boot):
    # #124's position: `mandelbrot` prints its lines, then waits for a key
    # on its console; meanwhile a wheel click on the tablet makes the
    # mouse thread log (debug) the first event it has no use for, and only
    # then does the key come.
    m = shells(boot, cmdline="lumen.log=debug", devices=Devices(tablet=True))
    start = len(m.output)
    m.send_line("mandelbrot")
    wait_for_output(m, start, r"mandelbrot: press a key\n$", 60)
    m.click("wheel-up")
    wait_for_output(m, start, IGNORED)
    m.send(" ")
    out = read_output(m, "mandelbrot")
    lines = out.splitlines()
    assert lines[-1] == "mandelbrot: press a key", out
    assert all(line.startswith("mandelbrot: ") for line in lines), out
    assert re.fullmatch(IGNORED, out.logs[-1] + "\n"), out.logs
    assert re.search(rf"press a key\n{IGNORED}{PROMPT}", m.output[start:]), m.output[start:]


SCREEN_SHELLS_WAITING = r"DEBUG shell: screen shells waiting for keys: (\d+) of (\d+)\n"
# The kernel command line under which the readiness line is logged.
PARKED_CMDLINE = "lumen.log=debug"
HELD = "WARN  shell: serial prompt held"


def expect_screen_shells_parked(m: Machine, timeout: float = 60) -> None:
    """For a machine booted with `PARKED_CMDLINE` (`lumen.log=debug`): the
    kernel's line, just before the serial shell's first prompt, says all
    four screen shells were blocked waiting for a key (#119); no prompt came
    before it, nor the warning that the prompt was held."""
    found = m.expect(SCREEN_SHELLS_WAITING, timeout)
    assert found.groups() == ("4", "4"), found.group(0)
    before = m.output[: found.start()]
    assert PROMPT not in before, before[-2000:]
    assert HELD not in before, before[-2000:]


def test_serial_prompt_waits_for_the_screen_shells(boot):
    # #119: the serial shell's first prompt comes only once all four screen
    # shells are blocked waiting for their first key, so the first line
    # typed on serial runs while they are parked (test_smp's crash_on
    # relies on it, and checks this line in every boot but one). The
    # kernel logs, at debug level, when the wait starts and, just before the
    # prompt, what its last check found. The wait itself is checked
    # deterministically by the ktests
    # sched::yield_until_blocked_returns_once_every_thread_is_blocked and
    # shell::the_serial_prompt_waits_on_past_its_warning.
    m = boot("qemutest", cmdline=PARKED_CMDLINE)
    m.expect(r"DEBUG shell: serial prompt waits for 4 screen shells to wait for keys\n", 60)
    expect_screen_shells_parked(m)
    m.expect(PROMPT, 60)
    assert HELD not in m.output, m.output[-2000:]


BOOTING = r"INFO  boot: Lumen \d+\.\d+\.\d+ \(x86_64\) booting"


def reboots(m: Machine, method: str, warning: str | None = None) -> None:
    """Run `reboot` in the serial shell of `m` (booted with reboot=True) and
    check it reset by `method` (its log line; no later method's), after
    `warning` if one is given, and booted to the shells again."""
    m.expect(BOOTING, 60)
    shells_ready = m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    m.qmp()  # connected before the reset, so it gets the RESET event
    m.send_line("reboot")
    m.expect(r"reboot: restarting\n", 10)
    if warning:
        m.expect(rf"WARN  power: {warning}\n", 10)
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
    assert len(re.findall(r"INFO  power: rebooting: ", first_boot)) == 1, first_boot
    assert "didn't reset the machine" not in first_boot
    assert m.proc is not None and m.proc.poll() is None, "QEMU ended instead of rebooting"
    # Under TCG QEMU logs a triple fault (`-d cpu_reset`): only the triple
    # fault method may cause one (so 0xcf9 or the 8042 can't pass by
    # falling through to it without their warnings).
    if default_accel() == "tcg":
        triple = "Triple fault" in (m.artifacts / "qemu.log").read_text(errors="replace")
        assert triple == (method == "triple fault")


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


def test_reboot_with_an_unknown_first_method_warns(boot):
    # A `lumen.reboot=` value it doesn't know: a warning, then 0xcf9.
    m = boot("qemutest", reboot=True, cmdline="lumen.reboot=bogus")
    reboots(
        m,
        "reset through port 0xcf9",
        "lumen.reboot=bogus: not cf9, 8042 or triple; starting with 0xcf9",
    )


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
    # Each key is confirmed before the next goes (lumen_harness.gui,
    # #149): its bytes reached IRQ 1 whole and were queued, or the step
    # fails naming the key. A key lost on the way in (#128) then fails
    # there; one lost later, in the decoder, router or shell, still shows
    # only as a wrong screen.
    m = shells(boot)
    confirmed_keys(m, "echo one\n", "echo two\n", ["up"], ["up"], ["ret"])
    expect_screen(m, r"^two\nlumen> echo one\none\nlumen>$")
    # Down goes back to a newer line ("echo one" again: a repeat of the
    # newest, not kept twice).
    confirmed_keys(m, ["up"], ["up"], ["down"], ["ret"])
    expect_screen(m, r"^lumen> echo one\none\nlumen> echo one\none\nlumen>$")
    # Delete takes the character at the cursor, Right moves past one.
    confirmed_keys(m, "echo abXc", ["left"], ["left"], ["delete"], ["right"], "d\n")
    expect_screen(m, r"^lumen> echo abcd\nabcd$")
    # Edits anywhere in the line: "cho wrld" becomes "echo world".
    confirmed_keys(m, "cho wrld", ["home"], "e", ["end"], ["left"], ["left"], ["left"], "o")
    confirmed_keys(m, ["ctrl", "e"], " xyz", ["ctrl", "w"], ["ret"])
    # (The screen's rows come without trailing blanks.)
    expect_screen(m, r"^lumen> echo world\nworld$")
    # Ctrl-U deletes before the cursor, Ctrl-K after it.
    confirmed_keys(m, "junk echo ", *[["left"]] * 5, ["ctrl", "u"], ["end"], "kept\n")
    expect_screen(m, r"^lumen> echo kept\nkept$")
    # Ctrl-K deletes after the cursor; Ctrl-A goes to the start and Ctrl-F
    # one on ("eho cut" becomes "echo cut" only if both move).
    confirmed_keys(
        m, "eho cut junk", *[["left"]] * 5, ["ctrl", "k"], ["ctrl", "a"], ["ctrl", "f"], "c\n"
    )
    expect_screen(m, r"^lumen> echo cut\ncut$")
    # Ctrl-C drops the line; nothing runs.
    confirmed_keys(m, "echo dropped", ["ctrl", "c"], "echo after\n")
    expect_screen(m, r"^lumen> echo dropped\^C\nlumen> echo after\nafter$")
    assert not re.search(r"^dropped$", read_screen(m).text, re.MULTILINE)
    confirmed_keys(m, "history\n")
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
    confirmed_keys(m, "ec", ["tab"], "hi\n")
    expect_screen(m, r"^lumen> echo hi\nhi$")
    confirmed_keys(m, "crash p", ["tab"])
    expect_screen(m, r"^lumen> crash p\npanic  pf\nlumen> crash p$")
    confirmed_keys(m, "a", ["tab"])
    expect_screen(m, r"^lumen> crash panic$")
    # `term` and `terminal` (#155) share their first four letters: Tab
    # stops there, a second lists both, then a space and Tab complete
    # `term`'s argument.
    confirmed_keys(m, ["ctrl", "u"], "te", ["tab"])
    expect_screen(m, r"^lumen> term$")
    confirmed_keys(m, ["tab"])
    expect_screen(m, r"^term  terminal\nlumen> term$")
    confirmed_keys(m, " ", ["tab"])
    expect_screen(m, r"^lumen> term text$")
    confirmed_keys(m, ["ctrl", "u"], "cl", ["tab"])
    expect_screen(m, r"^clear  clocks\nlumen> cl$")
    confirmed_keys(m, "o", ["tab"])
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
    confirmed_keys(m, "frob\n")
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


def assert_laid_out(row: str, layout: Sequence[tuple[int | None, str]]) -> None:
    """`row` is a table row as the system commands print them: indented two
    spaces, each column `(width, "<" or ">")` exactly that wide (aligned
    so), two spaces apart; a last column of width `None` unpadded."""
    at = 2
    assert row[:2] == "  ", row
    for i, (width, align) in enumerate(layout):
        if i:
            assert row[at : at + 2] == "  ", (row, i)
            at += 2
        field = row[at:] if width is None else row[at : at + width]
        assert field.strip(), (row, i)
        if width is None:
            assert field == field.strip(), (row, i)
        elif align == "<":
            assert field == field.rstrip().ljust(width) and not field[0].isspace(), (row, i)
        else:
            assert field == field.lstrip().rjust(width) and not field[-1].isspace(), (row, i)
        at += len(field)
    assert at == len(row), row


def test_system_commands(boot):
    # Each system command prints its header and at least one row, on serial
    # in plain text; under -smp 2 both CPUs are listed, the keyboard's input
    # (GSI 1) is decoded as routed and unmasked, the timer's vector counts.
    m = shells(boot, extra=["-smp", "2"])
    start = len(m.output)
    out = run(m, "help")
    assert out.startswith("commands:\n")
    for name in ("cpus", "mem", "irqs", "threads", "lspci", "dmesg", "date", "top"):
        assert re.search(rf"^  {name} ", out, re.MULTILINE), name

    out = run(m, "cpus")
    assert out.startswith(
        "cpus: 2 online\n  cpu  apic  scheduler    ticks       busy  threads\n"
    ), out
    # Laid out exactly as before the rows became tables.
    for row in out.splitlines()[2:]:
        assert_laid_out(row, [(3, ">"), (4, ">"), (11, "<"), (10, "<"), (4, ">"), (7, ">")])
    rows = re.findall(r"^\s+(\d)\s+\d+\s+running\s+(\d+)\s+(\d+)%\s+(\d+)$", out, re.MULTILINE)
    assert [r[0] for r in rows] == ["0", "1"], out
    assert all(
        int(ticks) > 0 and 0 <= int(busy) <= 100 and int(n) >= 1 for _, ticks, busy, n in rows
    )

    out = run(m, "mem")
    assert out.startswith("mem: "), out
    assert re.search(
        r"^  frames  [\d.]+ [KMG]iB of [\d.]+ [KMG]iB used \(\d+ frames of \d+ free",
        out,
        re.MULTILINE,
    ), out
    assert re.search(r"^  heap    .* used in \d+ allocations, .* free$", out, re.MULTILINE), out
    assert re.search(r"^  stacks  \d+ \(.*\): .*\d+ thread", out, re.MULTILINE), out

    out = run(m, "irqs")
    assert re.match(r"irqs: \d+ I/O APIC inputs", out), out
    lines = out.splitlines()
    assert lines[1] == "  gsi  vector  delivery        trigger  polarity  dest  state     count", (
        out
    )
    # (A use with a space comes quoted, so the column is as wide as that.)
    vectors = next(i for i, line in enumerate(lines) if re.fullmatch(r"  vector  use +count", line))
    for row in lines[2:vectors]:
        if row.endswith("masked    -"):
            # An input no driver routed: its number and `masked`.
            assert re.fullmatch(
                r"  [ \d]{3}  -       -               -        -         -     masked    -", row
            )
        else:
            assert_laid_out(
                row,
                [
                    (3, ">"),
                    (6, "<"),
                    (14, "<"),
                    (7, "<"),
                    (8, "<"),
                    (4, "<"),
                    (8, "<"),
                    (None, "<"),
                ],
            )
    for row in lines[vectors + 1 :]:
        assert re.fullmatch(r'  0x[0-9a-f]{2}    ("[^"]+"|\S+) +\d+', row), row
    assert re.search(
        r"^\s+1\s+0x51\s+Fixed\s+edge\s+high\s+0\s+unmasked\s+\d+$", out, re.MULTILINE
    ), out
    timer = re.search(r'^\s+0x40\s+"local APIC timer"\s+(\d+)$', out, re.MULTILINE)
    assert timer and int(timer[1]) > 0, out

    out = run(m, "threads")
    assert re.match(r"threads: \d+\n  thread            id  cpu  state     ticks\n", out), out
    assert re.search(r"^  main +\d+ +0 +running +\d+$", out, re.MULTILINE), out
    for vt in range(1, 5):
        assert re.search(
            rf"^  shell-vt{vt} +\d+ +0 +(blocked|ready|running) +\d+$", out, re.MULTILINE
        ), out

    lines = run_dmesg(m)
    assert re.fullmatch(r"dmesg: the newest \d+ log lines \(all since boot\)", lines[0]), lines
    # Only whole lines, the first of them the boot banner.
    assert lines[1].startswith("[    ?.??????] INFO  boot: Lumen "), lines
    assert all(is_log_line(line) for line in lines[1:]), lines
    assert any(
        re.match(r"\[\s*\d+\.\d{6}\] INFO  shell: shells on serial and the screen", line)
        for line in lines
    ), lines

    out = run(m, "trace 4")
    assert re.search(r"^trace: \d+ events claimed, \d+ dropped$", out, re.MULTILINE), out
    assert len(out.splitlines()) >= 2, out

    assert re.fullmatch(r"uptime: \d+ ns\n", run(m, "uptime"))
    assert re.fullmatch(r"date: \d{4}-\d\d-\d\d \d\d:\d\d:\d\d UTC\n", run(m, "date"))
    assert run(m, "clear").startswith("clear: this console is plain text")
    listed = [line.split(maxsplit=1)[1] for line in run(m, "history").splitlines()]
    assert listed[:3] == ["help", "cpus", "mem"] and listed[-1] == "history", listed
    assert "\x1b" not in m.output[start:]


def test_pipe_text(boot):
    # A command's text through filters, on both consoles: `|` outside quotes
    # ends a command; a mistake is one line.
    m = shells(boot)
    lines = run_dmesg(m, "dmesg | grep time")
    assert lines and all("time" in line for line in lines), lines
    assert any("INFO  time: " in line for line in lines), lines
    assert run(m, "seq 20 | grep 1 | count") == "11\n"
    assert run(m, "seq 20 | grep 1 | first 3") == "1\n10\n11\n"
    assert run(m, "echo 'a | b' x\\|y | grep '|'") == "a | b x|y\n"
    assert run(m, "echo 'a | b'") == "a | b\n"
    assert run(m, "| count") == "syntax error: a | with nothing before it\n"
    assert run(m, "seq 3 |") == "syntax error: a | with nothing after it\n"
    assert run(m, "seq 3 | echo") == (
        "echo: not a filter (after a | come grep, where, sort, first, count, select)\n"
    )
    assert run(m, "grep x") == (
        "grep: a filter: it takes what a command gives it after a | (cpus | grep <text>)\n"
    )
    # A command's usage or error line reaches the console, not the next
    # command; one that would wait unseen says so instead.
    assert run(m, "seq x | count") == "usage: seq <n> (up to 10000)\n"
    assert run(m, "top | count") == "top: in a pipeline it needs a number of frames (top 1 | ...)\n"
    assert run(m, "mandelbrot | count") == (
        "mandelbrot: draws on the screen and waits for a key: not in a pipeline\n"
    )
    # Text that is a number compares as one.
    assert run(m, "seq 12 | where line > 5 | count") == "7\n"
    # On the screen, typed on the keyboard.
    confirmed_keys(m, "seq 30 | grep 2 | count\n")
    expect_screen(m, r"^lumen> seq 30 \| grep 2 \| count\n12\nlumen>$")


def test_where_sort_first(boot):
    # The system commands hand on records: `where` compares a column
    # (numbers as numbers), `sort` orders by one, `first` keeps the first
    # rows, `select` some columns; the table is printed at the end.
    m = shells(boot, extra=["-smp", "2"])
    # The issue's pipeline: the three vectors that took the fewest
    # interrupts, but some.
    out = run(m, "irqs | where count > 0 | sort count | first 3")
    lines = out.splitlines()
    assert re.fullmatch(r"  vector  use +count", lines[0]), out
    counts = [int(line.rsplit(maxsplit=1)[1]) for line in lines[1:]]
    assert 1 <= len(counts) <= 3 and counts == sorted(counts) and counts[0] > 0, out
    out = run(m, "irqs | where count > 0 | sort count desc | first 3")
    lines = out.splitlines()
    assert re.fullmatch(r"  vector  use +count", lines[0]), out
    # (A use with a space comes quoted.)
    rows = [re.fullmatch(r'  (0x[0-9a-f]{2})    ("[^"]+"|\S+) +(\d+)', line) for line in lines[1:]]
    assert 1 <= len(rows) <= 3 and all(rows), out
    counts = [int(r[3]) for r in rows]
    assert counts == sorted(counts, reverse=True) and counts[-1] > 0, out
    # The timer interrupts most.
    assert rows[0][2] == '"local APIC timer"', out

    assert run(m, "cpus | count") == "2\n"
    out = run(m, "cpus | where cpu >= 1 | select cpu scheduler busy")
    assert re.fullmatch(r"  cpu  scheduler    busy\n    1  running      +\d+%\n", out), out
    out = run(m, "threads | where thread ~ shell | sort id | select thread")
    assert out == "  thread\n  shell-vt1\n  shell-vt2\n  shell-vt3\n  shell-vt4\n", out
    out = run(m, "mem | where area = heap | select area used")
    assert re.fullmatch(r"  area  +used\n  heap  +\d+\n", out), out
    # Lines that are numbers sort by value (as `where` compares them), the
    # rest as text after them.
    assert run(m, "seq 10 | sort line desc | first 3") == "10\n9\n8\n"
    assert run(m, "echo b | sort line") == "b\n"
    out = run(m, "threads | sort ticks desc | select ticks")
    ticks = [int(t) for t in out.splitlines()[1:]]
    assert ticks == sorted(ticks, reverse=True), out
    assert run(m, "cpus | where busy > lots") == "where: busy holds numbers, and 'lots' isn't one\n"
    assert run(m, "irqs | sort calls") == "sort: no column 'calls' (columns: vector, use, count)\n"
    assert (
        run(m, "irqs | where vector = 64") == "where: vector is hex: write it with 0x (e.g. 0x20)\n"
    )
    assert run(m, "cpus x | count") == "usage: cpus\n"
    # Tab completes the command after a `|`: only the filters there.
    m.send("cpus | cou\t\r")
    m.expect(r"cpus \| count \n2\n" + PROMPT, 10)
    m.send("cpus | c\t\r")
    m.expect(r"cpus \| count \n2\n" + PROMPT, 10)


def test_date_matches_host(boot):
    # `date` reads the RTC, which QEMU keeps in UTC (`-rtc base=utc`) from
    # the host's clock: no later than the host's clock when it printed, and
    # at most 3 s behind the host's whole second when it was sent. The RTC
    # counts whole seconds, and QEMU's ticks on a phase of its own: measured
    # here it reads 1 to 2 s behind the host (3 allowed, in case another
    # QEMU truncates once more).
    from datetime import UTC, datetime

    m = shells(boot)
    for _ in range(3):
        before = datetime.now(UTC).replace(microsecond=0)
        out = run(m, "date")
        after = datetime.now(UTC)
        found = re.fullmatch(r"date: (\d{4}-\d\d-\d\d \d\d:\d\d:\d\d) UTC\n", out)
        assert found, out
        rtc = datetime.strptime(found[1], "%Y-%m-%d %H:%M:%S").replace(tzinfo=UTC)
        assert -3 <= (rtc - before).total_seconds() and rtc <= after, (before, rtc, after)


def test_top_refreshes_and_quits(boot):
    # On plain serial, `top` prints a frame a second as lines (the CPUs and
    # how busy, interrupt rates, the heap, the threads), until `q`. Under
    # -smp 2: frames 0.9 to 1.5 s apart by the kernel's own clock, and each
    # CPU's timer at about 1 kHz.
    m = shells(boot, extra=["-smp", "2"])
    start = len(m.output)
    m.send_line("top")
    header = r"top: frame {}, up (\d+)\.(\d) s, 2 CPUs; q quits\n"
    first = m.expect(header.format(1), 15)
    second = m.expect(header.format(2), 15)
    third = m.expect(header.format(3), 15)

    def tenths(frame: re.Match[str]) -> int:
        return int(frame[1]) * 10 + int(frame[2])

    assert 9 <= tenths(second) - tenths(first) <= 15, (first[0], second[0])
    assert 9 <= tenths(third) - tenths(second) <= 15, (second[0], third[0])
    frame = m.output[second.end() : third.start()]
    assert re.search(r"^\s+0\s+\d+%\s+\d+$", frame, re.MULTILINE), frame
    # The AP's ticks/s about half the timers' interrupts/s (two CPUs), each
    # CPU's at most 1.5 kHz and at least 200 Hz (TCG under load ticks at
    # ~800 Hz).
    ap = re.search(r"^\s+1\s+\d+%\s+(\d+)$", frame, re.MULTILINE)
    timer = re.search(r"^  irqs/s: .*local APIC timer (\d+)", frame, re.MULTILINE)
    assert ap and timer, frame
    per_cpu = int(timer[1]) / 2
    assert 200 <= int(ap[1]) <= 1500 and 200 <= per_cpu <= 1500, frame
    assert abs(int(ap[1]) - per_cpu) <= per_cpu / 2, frame
    assert re.search(r"^  heap .* used of .* in \d+ allocations; frames ", frame, re.MULTILINE), (
        frame
    )
    assert re.search(r"^  main\s+\d+\s+0\s+\w+\s+\d+%$", frame, re.MULTILINE), frame
    for vt in range(1, 5):
        assert re.search(rf"^  shell-vt{vt}\s+\d+\s+0\s+\w+\s+\d+%$", frame, re.MULTILINE), frame
    m.send("q")
    m.expect(r"top: quit after \d+ frames\n" + PROMPT, 10)
    # It stopped: no frame after the prompt.
    end = len(m.output)
    assert run(m, "echo still here") == "still here\n"
    assert "top: frame" not in m.output[end:]
    assert "\x1b" not in m.output[start:]
