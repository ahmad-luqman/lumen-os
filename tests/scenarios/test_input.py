"""The PS/2 keyboard: keys sent through QMP reach kmon, decoded in the layout
`lumen.keymap=` names."""

import re
import threading
import time
from typing import NamedTuple

from lumen_harness.qemu import Machine
from lumen_harness.qmp import QmpClient, QmpError, qcodes_for

KBD_LINE = r"INFO  kbd: PS/2 keyboard on ISA IRQ 1 \(GSI 1, vector 0x51\), layout {}\n"
KBD_STATS = re.compile(
    r"kbd: layout (\S+), IRQ 1 \(vector 0x51\), (\d+) interrupts, (\d+) bytes, (\d+) dropped, "
    r"(\d+) undecoded, (\d+) line errors, (\d+) skipped at setup\n"
)
# Without an 8042 its status reads 0xff: never ready for a command.
NO_8042 = r"no reply in time \(command 0xad: the controller to take it\)"


class KbdStats(NamedTuple):
    layout: str
    irqs: int
    bytes: int
    dropped: int
    undecoded: int
    line_errors: int
    skipped: int


def kmon_ready(m: Machine, layout: str) -> None:
    """Wait for the keyboard's boot line (in `layout`), then kmon's prompt."""
    m.expect(KBD_LINE.format(layout), 60)
    m.expect(r"kmon> ", 60)


def kbd_stats(m: Machine) -> KbdStats:
    """kmon's `kbd`."""
    m.send_line("kbd")
    found = m.expect(KBD_STATS, 10)
    m.expect(r"kmon> ", 10)
    layout, *counts = found.groups()
    return KbdStats(layout, *map(int, counts))


def echo_physical_yz(m: Machine, typed: str) -> None:
    """Press the keys labelled e c h o space Y Z Enter on a US keyboard; the
    guest reads `typed` for Y Z (one character each), echoes each key, and
    kmon's echo prints `typed`."""
    keys = ["e", "c", "h", "o", "spc", "y", "z", "ret"]
    for key, echo in zip(keys, f"echo {typed}\n", strict=True):
        m.press([key], echo)
    # (The echo of the Enter key was the last thing `press` waited for.)
    m.expect(rf"{typed}\n", 10)


def test_keys_via_qmp_send_key(boot):
    # Typed on the keyboard (shift included), a command runs as if typed on
    # serial. Every key held made two scancodes (press, release), all
    # queued; they raised IRQ 1 (one interrupt can move several, so at least
    # once a character), and nothing was dropped, undecoded or unexpected.
    text = "echo Hello, World!\n"
    m = boot("qemutest")
    kmon_ready(m, "us")
    before = kbd_stats(m)
    m.type_text(text)
    m.expect(r"Hello, World!\n", 10)
    m.expect(r"kmon> ", 10)
    want = before.bytes + 2 * sum(len(qcodes_for(c)) for c in text)
    # The last key's release comes after its press was echoed, once QMP's
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
    kmon_ready(us, "us")
    echo_physical_yz(us, "yz")
    de = boot("qemutest", cmdline="lumen.keymap=de")
    kmon_ready(de, "de")
    echo_physical_yz(de, "zy")
    assert kbd_stats(de).layout == "de"
    assert "WARN" not in de.output


def test_an_unknown_keymap_falls_back_to_us(boot):
    m = boot("qemutest", cmdline="lumen.keymap=klingon")
    m.expect(r"WARN  kbd: unknown keymap 'klingon' \(known: us uk de [a-z ]+\); using us\n", 60)
    kmon_ready(m, "us")
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
    m.expect(r"kmon> ", 60)
    # Whatever reached kmon before the presses stopped makes one line of
    # a's: end it, then the keyboard types normally.
    m.press(["ret"], "\n")
    m.expect(r"kmon> ", 10)
    echo_physical_yz(m, "yz")
    assert kbd_stats(m).skipped > 0
    assert "WARN" not in m.output


def test_every_printable_character_and_backspace(boot):
    # Every printable ASCII character typed on a US keyboard reaches kmon as
    # itself, and backspace erases the last one.
    every = "".join(chr(c) for c in range(0x21, 0x7F)) + " end"
    m = boot("qemutest")
    kmon_ready(m, "us")
    m.type_text("echo " + every + "X")
    m.press(["backspace"], "\x08 \x08")
    m.press(["ret"], "\n")
    m.expect(re.escape(every) + "\n", 10)


def test_no_8042_is_a_warning(boot):
    # Without the controller the keyboard is skipped with a warning naming
    # the command, and kmon says why there is none (IRQ 1 stays routed,
    # masked).
    m = boot("qemutest", extra=["-machine", "q35,i8042=off"])
    m.expect(rf"WARN  kbd: no PS/2 keyboard: {NO_8042}\n", 60)
    m.expect(r"kmon> ", 60)
    m.send_line("kbd")
    m.expect(rf"kbd: no keyboard \({NO_8042}\); IRQ 1 routed \(vector 0x51\), masked\n", 10)
