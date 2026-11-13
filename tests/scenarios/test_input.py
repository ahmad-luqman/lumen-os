"""The PS/2 keyboard: keys sent through QMP reach kmon, decoded in the layout
`lumen.keymap=` names."""

KBD_LINE = r"INFO  kbd: PS/2 keyboard on ISA IRQ 1 \(GSI 1, vector 0x51\), layout {}\n"


def kbd_stats(m) -> tuple[str, int, int]:
    m.send_line("kbd")
    found = m.expect(r"kbd: layout (\S+), (\d+) interrupts, (\d+) bytes dropped\n", 10)
    m.expect(r"kmon> ", 10)
    return found.group(1), int(found.group(2)), int(found.group(3))


def test_keys_via_qmp_send_key(boot):
    # Typed on the keyboard (shift included), a command runs as if typed on
    # serial; each key raised IRQ 1 (two scancodes, press and release, each
    # its own interrupt) and nothing was dropped or unexpected.
    m = boot("qemutest")
    m.expect(KBD_LINE.format("us"), 60)
    m.expect(r"kmon> ", 60)
    _, before, _ = kbd_stats(m)
    m.type_text("echo Hello, World!\n")
    # (The echo of the Enter key was the last thing `type_text` waited for.)
    m.expect(r"Hello, World!\n", 10)
    m.expect(r"kmon> ", 10)
    layout, after, dropped = kbd_stats(m)
    assert (layout, dropped) == ("us", 0)
    assert after - before >= len("echo Hello, World!\n"), (before, after)
    m.send_line("ticks")
    strays = m.expect(
        r"ticks: \d+ \(spurious (\d+), unexpected (\d+), local APIC errors (\d+)\)", 10
    )
    assert strays.groups() == ("0", "0", "0")
    assert "WARN" not in m.output


def type_physical_yz(m, echoes: str) -> None:
    """Press the keys labelled e c h o space Y Z Enter on a US keyboard,
    expecting the guest to echo `echoes` (one character per key)."""
    for key, echo in zip(["e", "c", "h", "o", "spc", "y", "z", "ret"], echoes, strict=True):
        m.press([key], echo)


def test_second_layout(boot):
    # The same physical keys: Y and Z trade places on a German keyboard.
    us = boot("qemutest")
    us.expect(KBD_LINE.format("us"), 60)
    us.expect(r"kmon> ", 60)
    type_physical_yz(us, "echo yz\n")
    us.expect(r"yz\n", 10)
    de = boot("qemutest", cmdline="lumen.keymap=de")
    de.expect(KBD_LINE.format("de"), 60)
    de.expect(r"kmon> ", 60)
    type_physical_yz(de, "echo zy\n")
    de.expect(r"zy\n", 10)
    assert kbd_stats(de)[0] == "de"
    assert "WARN" not in de.output


def test_an_unknown_keymap_falls_back_to_us(boot):
    m = boot("qemutest", cmdline="lumen.keymap=klingon")
    m.expect(r"WARN  kbd: unknown keymap 'klingon' \(known: us uk de [a-z ]+\); using us\n", 60)
    m.expect(KBD_LINE.format("us"), 60)
    m.expect(r"kmon> ", 60)
    type_physical_yz(m, "echo yz\n")
    m.expect(r"yz\n", 10)
