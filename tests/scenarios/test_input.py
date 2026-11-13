"""The PS/2 keyboard: keys sent through QMP reach kmon, decoded in the layout
`lumen.keymap=` names."""

import re
import threading

from lumen_harness.qemu import Machine
from lumen_harness.qmp import QmpClient

KBD_LINE = r"INFO  kbd: PS/2 keyboard on ISA IRQ 1 \(GSI 1, vector 0x51\), layout {}\n"
KBD_STATS = re.compile(
    r"kbd: layout (\S+), IRQ 1 \(vector 0x51\), (\d+) interrupts, (\d+) bytes dropped, "
    r"(\d+) undecoded, (\d+) line errors\n"
)


def kmon_ready(m: Machine, layout: str) -> None:
    """Wait for the keyboard's boot line (in `layout`), then kmon's prompt."""
    m.expect(KBD_LINE.format(layout), 60)
    m.expect(r"kmon> ", 60)


def kbd_stats(m: Machine) -> tuple[str, int, int, int, int]:
    """kmon's `kbd`: (layout, interrupts, dropped, undecoded, line errors)."""
    m.send_line("kbd")
    found = m.expect(KBD_STATS, 10)
    m.expect(r"kmon> ", 10)
    layout, *counts = found.groups()
    return (layout, *map(int, counts))


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
    # serial; the keys raised IRQ 1 (at least once a key: one interrupt can
    # move several scancodes) and nothing was dropped, undecoded or
    # unexpected.
    m = boot("qemutest")
    kmon_ready(m, "us")
    _, before, *_ = kbd_stats(m)
    m.type_text("echo Hello, World!\n")
    m.expect(r"Hello, World!\n", 10)
    m.expect(r"kmon> ", 10)
    layout, after, dropped, undecoded, line_errors = kbd_stats(m)
    assert (layout, dropped, undecoded, line_errors) == ("us", 0, 0, 0)
    assert after - before >= len("echo Hello, World!\n"), (before, after)
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
    assert kbd_stats(de)[0] == "de"
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
    # reply), and the keyboard must come up anyway.
    m = boot("qemutest")
    qmp = QmpClient.connect(m.qmp_path, timeout=10)
    done = threading.Event()

    def press_until_set_up() -> None:
        while not done.is_set() and "kbd: " not in m.output:
            qmp.send_key(["a"], hold_ms=1)
            done.wait(0.002)

    presser = threading.Thread(target=press_until_set_up, daemon=True)
    presser.start()
    try:
        m.expect(KBD_LINE.format("us"), 60)
    finally:
        done.set()
        presser.join(10)
        qmp.close()
    m.expect(r"kmon> ", 60)
    # Whatever reached kmon before the presses stopped makes one line of
    # a's: end it, then the keyboard types normally.
    m.press(["ret"], "\n")
    m.expect(r"kmon> ", 10)
    echo_physical_yz(m, "yz")
    assert "WARN" not in m.output
