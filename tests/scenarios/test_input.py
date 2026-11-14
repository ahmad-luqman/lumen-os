"""The PS/2 keyboard: keys sent through QMP reach the screen's shell, decoded
in the layout `lumen.keymap=` names (read back from the screen with the
serial shell's `term text`)."""

import re
import shlex
import threading
import time
from typing import NamedTuple

from lumen_harness.qemu import Machine
from lumen_harness.qmp import QmpClient, QmpError, qcodes_for
from lumen_harness.screen import expect_screen

KBD_LINE = r"INFO  kbd: PS/2 keyboard on ISA IRQ 1 \(GSI 1, vector 0x51\), layout {}\n"
KBD_STATS = re.compile(
    r"kbd: layout (\S+), IRQ 1 \(vector 0x51\), (\d+) interrupts, (\d+) bytes, (\d+) dropped, "
    r"(\d+) undecoded, (\d+) line errors, (\d+) skipped at setup\n"
)
# Without an 8042 its status reads 0xff: never ready for a command.
NO_8042 = r"no reply in time \(command 0xad: the controller to take it\)"
SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"


class KbdStats(NamedTuple):
    layout: str
    irqs: int
    bytes: int
    dropped: int
    undecoded: int
    line_errors: int
    skipped: int


def shells_ready(m: Machine, layout: str) -> None:
    """Wait for the keyboard's boot line (in `layout`), both shells, and the
    serial shell's prompt."""
    m.expect(KBD_LINE.format(layout), 60)
    m.expect(SHELLS, 60)
    m.expect(r"lumen> ", 60)


def kbd_stats(m: Machine) -> KbdStats:
    """The serial shell's `kbd`."""
    m.send_line("kbd")
    found = m.expect(KBD_STATS, 10)
    m.expect(r"lumen> ", 10)
    layout, *counts = found.groups()
    return KbdStats(layout, *map(int, counts))


def echo_physical_yz(m: Machine, typed: str) -> None:
    """Press the keys labelled e c h o space Y Z Enter on a US keyboard; the
    guest reads `typed` for Y Z (one character each), and the screen shows
    the line typed and `echo`'s answer."""
    for key in ["e", "c", "h", "o", "spc", "y", "z", "ret"]:
        m.press([key])
    expect_screen(m, rf"^lumen> echo {typed}\n{typed}$")


def test_keys_via_qmp_send_key(boot):
    # Typed on the keyboard (shift included), a command runs in the screen's
    # shell, which answers on the screen (serial only sees the rows `term
    # text` prints). Every key held made two scancodes (press, release), all
    # queued; they raised IRQ 1 (one interrupt can move several, so at least
    # once a character), and nothing was dropped, undecoded or unexpected.
    text = "echo Hello, World!\n"
    m = boot("qemutest")
    shells_ready(m, "us")
    before = kbd_stats(m)
    start = len(m.output)
    m.type_text(text)
    expect_screen(m, r"^lumen> echo Hello, World!\nHello, World!\nlumen>$")
    assert not re.search(r"^Hello", m.output[start:], re.MULTILINE)
    want = before.bytes + 2 * sum(len(qcodes_for(c)) for c in text)
    # The last key's release comes after its press was taken, once QMP's
    # hold time (20 ms) has passed: ask again until it has, for up to 10 s.
    deadline = time.monotonic() + 10
    while (s := kbd_stats(m)).bytes < want and time.monotonic() <= deadline:
        time.sleep(0.05)
    assert (s.layout, s.dropped, s.undecoded, s.line_errors) == ("us", 0, 0, 0)
    assert s.bytes == want, (before.bytes, s.bytes, want)
    assert s.irqs - before.irqs >= len(text), (before.irqs, s.irqs)
    m.send_line("ticks")
    strays = m.expect(
        r"ticks: \d+ \(spurious (\d+), unexpected (\d+), local APIC errors (\d+)\)", 10
    )
    assert strays.groups() == ("0", "0", "0")
    assert "WARN" not in m.output


def test_second_layout(boot):
    # The same physical keys: Y and Z trade places on a German keyboard.
    us = boot("qemutest")
    shells_ready(us, "us")
    echo_physical_yz(us, "yz")
    de = boot("qemutest", cmdline="lumen.keymap=de")
    shells_ready(de, "de")
    echo_physical_yz(de, "zy")
    assert kbd_stats(de).layout == "de"
    assert "WARN" not in de.output


def test_an_unknown_keymap_falls_back_to_us(boot):
    m = boot("qemutest", cmdline="lumen.keymap=klingon")
    m.expect(r"WARN  kbd: unknown keymap 'klingon' \(known: us uk de [a-z ]+\); using us\n", 60)
    shells_ready(m, "us")
    echo_physical_yz(m, "yz")


def test_keys_held_during_boot_leave_a_working_keyboard(boot):
    # A key pressed over and over from power-on until the keyboard is set up:
    # its scancodes arrive while the 8042 and the keyboard are being
    # initialized (with port 1 live, the self-test once read one as its
    # reply), are flushed there (and counted), and the keyboard comes up.
    # (QEMU never puts one in front of a reply init waits for: that path
    # is the ktest a_stray_byte_before_the_ack_is_skipped.)
    m = boot("qemutest")
    qmp = QmpClient.connect(m.qmp_path, timeout=10)
    done = threading.Event()
    presses: list[int] = [0]
    errors: list[Exception] = []

    def press_until_set_up() -> None:
        try:
            while not done.is_set() and "kbd: " not in m.output:
                qmp.send_key(["a"], hold_ms=1)
                presses[0] += 1
                done.wait(0.002)
        except (QmpError, OSError) as e:  # reported by the test, not lost in the thread
            errors.append(e)

    presser = threading.Thread(target=press_until_set_up, daemon=True)
    presser.start()
    try:
        first = m.expect(r"(INFO|WARN)  kbd: [^\n]*\n", 60)
    finally:
        done.set()
        presser.join(10)
        qmp.close()
    assert not presser.is_alive(), "the key presser didn't stop"
    assert not errors, errors
    assert presses[0] > 10, presses
    assert re.fullmatch(KBD_LINE.format("us"), first.group(0)), first.group(0)
    m.expect(r"INFO  kbd: skipped \d+ stray byte\(s\) while setting the keyboard up\n", 10)
    m.expect(SHELLS, 60)
    m.expect(r"lumen> ", 60)
    # Whatever reached the screen's shell before the presses stopped makes
    # one line of a's: end it, then the keyboard types normally.
    m.press(["ret"])
    echo_physical_yz(m, "yz")
    assert kbd_stats(m).skipped > 0
    assert "WARN" not in m.output


def test_every_printable_character_and_backspace(boot):
    # Every printable ASCII character typed on a US keyboard reaches the
    # screen's shell as itself (quoted, as the shell splits words), and
    # backspace erases the last one.
    every = "".join(chr(c) for c in range(0x21, 0x7F)) + " end"
    m = boot("qemutest")
    shells_ready(m, "us")
    m.type_text("echo " + shlex.quote(every) + "X")
    m.press(["backspace"])
    m.press(["ret"])
    expect_screen(m, "^" + re.escape(every) + "$")


def test_a_long_line_of_shifted_keys(boot):
    # 150 shifted keys typed back to back (each takes 12 of the 1024 entries
    # of QEMU's input queue, which drops the rest unreported): the harness
    # paces them, and every one arrives.
    m = boot("qemutest")
    shells_ready(m, "us")
    m.type_text("echo " + "A" * 150 + "\n")
    expect_screen(m, r"^A{150}$", 30)


def test_no_8042_is_a_warning(boot):
    # Without the controller the keyboard is skipped with a warning naming
    # the command, the serial shell says why there is none (IRQ 1 stays
    # routed, masked), and only the serial shell runs.
    m = boot("qemutest", extra=["-machine", "q35,i8042=off"])
    m.expect(rf"WARN  kbd: no PS/2 keyboard: {NO_8042}\n", 60)
    m.expect(r"INFO  shell: shell on serial \(none on the screen: no keyboard\); type 'help'\n", 60)
    m.expect(r"lumen> ", 60)
    m.send_line("kbd")
    m.expect(rf"kbd: no keyboard \({NO_8042}\); IRQ 1 routed \(vector 0x51\), masked\n", 10)
