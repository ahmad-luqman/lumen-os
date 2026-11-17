"""The PS/2 keyboard: keys sent through QMP reach the screen's shell, decoded
in the layout `lumen.keymap=` names (read back from the screen with the
serial shell's `term text`). The mouse: virtio's tablet, moved and clicked
through QMP's `input-send-event`, reported by `mouse` and drawn as an
arrow that leaves no trail; the virtio keyboard (same PCI id) isn't taken
for a tablet."""

import re
import shlex
import threading
import time
from collections.abc import Callable
from typing import NamedTuple

import pytest
from lumen_harness import gui
from lumen_harness.image import assert_matches_golden, compare
from lumen_harness.qemu import Devices, Machine
from lumen_harness.qmp import QmpClient, QmpError, pointer_move_args, qcodes_for
from lumen_harness.screen import (
    CELL_H,
    CELL_W,
    GRID_X,
    GRID_Y,
    cursor_box,
    expect_screen,
    read_screen,
)
from lumen_harness.shell import run
from PIL import Image
from test_pci import lspci_rows, msix_rows, qdev_addresses

KBD_LINE = r"INFO  kbd: PS/2 keyboard on ISA IRQ 1 \(GSI 1, vector 0x51\), layout {}\n"
KBD_STATS = re.compile(
    r"kbd: layout (\S+), IRQ 1 \(vector 0x51\), (\d+) interrupts, (\d+) bytes, (\d+) presses, "
    r"(\d+) held, (\d+) dropped, "
    r"(\d+) undecoded, (\d+) line errors, (\d+) skipped at setup, newest key event \d+\n"
)
# Without an 8042 its status reads 0xff: never ready for a command.
NO_8042 = r"no reply in time \(command 0xad: the controller to take it\)"
SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"


class KbdStats(NamedTuple):
    layout: str
    irqs: int
    bytes: int
    presses: int
    held: int
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
    # queued, and counts as one press; they raised IRQ 1 (one interrupt can
    # move several, so at least once a character), and nothing was dropped,
    # undecoded or unexpected.
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
    # `presses` counts each key pressed once (shift too), not its release.
    pressed = sum(len(qcodes_for(c)) for c in text)
    assert s.presses - before.presses == pressed, (before.presses, s.presses, pressed)
    assert s.irqs - before.irqs >= len(text), (before.irqs, s.irqs)
    m.send_line("ticks")
    strays = m.expect(
        r"ticks: \d+ \(spurious (\d+), unexpected (\d+), local APIC errors (\d+)\)", 10
    )
    assert strays.groups() == ("0", "0", "0")
    # Keys typed just before (unconfirmed, still arriving) don't confirm a
    # confirmed key (the case review reproduced): its step waits until they
    # settle, then counts exactly its own presses and bytes. Its counts
    # show the burst counted before it, all of it.
    burst = "echo abcdefghijklmnop"
    m.type_text(burst)
    got = gui.confirmed_keys(m, "x", "\n")
    assert got.presses - s.presses == len(burst) + 2, (s.presses, got.presses)
    assert (got.bytes - s.bytes, got.held) == (2 * (len(burst) + 2), 0), got
    expect_screen(m, rf"^lumen> {burst}x\nabcdefghijklmnopx\nlumen>$")
    # A key held when a confirmed one goes (shift, 400 ms, sent past
    # `press`'s schedule; reproduced in review): the step waits for its
    # release, then counts its own keys, so shift's release can't stand in
    # for the key's own.
    m.qmp().send_key(["shift"], hold_ms=400)
    gui.confirmed_keys(m, "echo b\n")
    expect_screen(m, r"^lumen> echo b\nb\nlumen>$")
    assert "WARN" not in m.output


def test_every_modelled_key_is_confirmed(boot):
    # Every key gui.KEY_BYTES models, Print Screen and Pause included, sent
    # on its own through QEMU: each is confirmed with exactly the presses
    # and bytes the table says, released, nothing dropped or undecoded.
    # This pins the table against QEMU's keyboard, which nothing else does.
    m = boot("qemutest")
    shells_ready(m, "us")
    keys = sorted(gui.KEY_BYTES, key=lambda q: q == "ret")  # Enter last
    for q in keys:
        gui.confirmed_keys(m, [q])
    gui.confirmed_keys(m, ["ctrl", "u"], "echo done\n")
    expect_screen(m, r"^lumen> echo done\ndone\nlumen>$")


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


TABLET_LINE = re.compile(
    r'INFO  mouse: (\S+): "QEMU Virtio Tablet", axes 0 to 32767 by 0 to 32767, '
    r"scaled to 1280x800, frames of up to 12 events\n"
)
MOUSE_HEADER = re.compile(
    r'mouse: "QEMU Virtio Tablet" on (\S+), screen 1280x800, (\d+) events in (\d+) frames '
    r"\((\d+) ignored, (\d+) dropped, (\d+) overruns, newest event \d+\)"
)


class Mouse(NamedTuple):
    device: str
    x: str
    y: str
    buttons: str
    presses: str
    wheel: str
    hwheel: str
    pointer: str
    stale: str
    failed: str


NOTHING_YET = ("-",) * 9


# Said once, by the batch that counted the first overrun (kernel/src/mouse.rs).
def overrun_warn(events: str = r"\d+", after_hold: str = r"(?: after `mouse hold`)?") -> str:
    """The overrun warning's line: `events` in the batch, and whether it
    came after `mouse hold` (either, unless given)."""
    return (
        rf"WARN  mouse: \S+: {events} events in one batch{after_hold}: the event ring was nearly "
        r"full, so frames may have been lost \(said once; `mouse` counts such batches\)"
    )


def held_overrun_warn(frame: int) -> str:
    """The warning for the batch read after a held overflow of
    `frame`-event frames: the ring's worth of them, after `mouse hold`."""
    return overrun_warn(str(RING // frame * frame), " after `mouse hold`")


def mouse(m: Machine, allow: str | None = None) -> tuple[str, Mouse]:
    """The serial shell's `mouse`: its header line and its row (a warning
    matching `allow` may come with it)."""
    lines = run(m, "mouse", allow=allow).splitlines()
    assert len(lines) == 3, lines
    assert re.fullmatch(
        r"  device\s+x\s+y\s+buttons\s+presses\s+wheel\s+hwheel\s+pointer\s+stale\s+failed",
        lines[1],
    ), lines
    return lines[0], Mouse(*re.split(r"\s{2,}", lines[2].strip()))


def mouse_counts(m: Machine, allow: str | None = None, held: bool = False) -> tuple[int, ...]:
    """`mouse`'s header counts: events, frames, ignored, dropped, overruns
    (the header ending `, held` if `held`, and only then)."""
    header = mouse(m, allow)[0]
    found = re.fullmatch(MOUSE_HEADER.pattern + (", held" if held else ""), header)
    assert found, header
    return tuple(map(int, found.groups()[1:]))


def poll[T](what: Callable[[], T], done: Callable[[T], bool], m: Machine, timeout: float = 10) -> T:
    """Call `what()` until `done(result)` (up to `timeout` seconds, scaled);
    the last result (`gui.poll`)."""
    return gui.poll(m, what, done, timeout)[0]


def wait_mouse(m: Machine, allow: str | None = None, **want: str) -> Mouse:
    """Ask `mouse` until its row has the values `want` (up to 10 s)."""

    def shows(row: Mouse) -> bool:
        return all(getattr(row, k) == v for k, v in want.items())

    row = poll(lambda: mouse(m, allow)[1], shows, m)
    assert shows(row), f"mouse never showed {want}: {row}"
    return row


def tablet_ready(m: Machine) -> str:
    """Wait for the keyboard's line, the shells and the serial prompt, and
    find the tablet's boot line; the tablet's PCI address."""
    shells_ready(m, "us")
    found = TABLET_LINE.search(m.output)
    assert found, "no tablet line at boot"
    return found[1]


def test_mouse_moves_pointer(boot, artifacts, golden_dir):
    # Before the tablet reports anything `mouse` shows its device and
    # dashes (nothing reported yet; the header comparison in
    # test_cursor_leaves_no_trail checks nothing is drawn); moved through
    # QMP, it reports
    # the pixel aimed at (the screen's corners included), as records too,
    # and the arrow's tip is drawn there (a golden of it on the empty
    # terminal); buttons are held and released, presses counted; the wheel
    # turns both ways.
    m = boot("qemutest", devices=Devices(tablet=True))
    addr = tablet_ready(m)
    assert mouse(m)[1] == Mouse(addr, *NOTHING_YET)
    assert mouse_counts(m) == (0, 0, 0, 0, 0)
    target = (900, 500)
    m.move_pointer(*target)
    row = wait_mouse(m, x="900", y="500")
    assert row == Mouse(addr, "900", "500", "none", "0", "0", "0", "drawn", "no", "-")
    shot = m.screendump("pointer.png")
    assert_matches_golden(
        shot,
        golden_dir / "mouse-pointer.png",
        origin=(target[0] - 4, target[1] - 4),
        size=(20, 26),
        artifacts=artifacts,
    )
    # Its records filter on numbers.
    assert run(m, "mouse | where x > 899 | count") == "1\n"
    assert run(m, "mouse | where x > 900 | count") == "0\n"
    for corner in [(0, 0), (1279, 799), (1279, 0), (0, 799)]:
        m.move_pointer(*corner)
        wait_mouse(m, x=str(corner[0]), y=str(corner[1]), pointer="drawn")
    m.click("left", release=False)
    wait_mouse(m, buttons="left")
    m.click("right", release=False)
    wait_mouse(m, buttons="left right")
    assert run(m, "mouse | where buttons = 'left right' | select x y").split() == [
        "x", "y", "0", "799",
    ]  # fmt: skip
    m.release("left")
    m.release("right")
    wait_mouse(m, buttons="none")
    m.click("middle")
    # Each move is x, y and a sync; each button change a key and a sync:
    # all of them read, none ignored, dropped or overrun.
    want = (5 * 3 + 6 * 2, 5 + 6, 0, 0, 0)
    assert poll(lambda: mouse_counts(m), lambda c: c == want, m) == want
    assert wait_mouse(m, presses="3").buttons == "none"
    # The wheel: a click away from the user, then one back.
    m.click("wheel-up")
    wait_mouse(m, wheel="1")
    m.click("wheel-down")
    wait_mouse(m, wheel="0", hwheel="0")
    assert "WARN" not in m.output


def pixels(shot: Image.Image, box: tuple[int, int, int, int]) -> list:
    return list(shot.crop(box).convert("RGB").getdata())


def test_cursor_leaves_no_trail(boot, artifacts):
    # The arrow crosses text, the header and its boxes; text is typed and
    # drawn below it; a command takes the screen (the arrow is hidden while
    # it draws) and gives it back. Then, with the arrow parked on an empty
    # spot, the screen must be exactly what drawing the terminal afresh
    # gives (`vt 2`, `vt 1`), the blinking text cursor's cell aside, and
    # the header exactly what it was before the arrow ever showed.
    m = boot("qemutest", devices=Devices(tablet=True))
    tablet_ready(m)
    m.type_text("clear\n")
    m.type_text("seq 20\n")
    expect_screen(m, r"^20\nlumen>$")
    # Row 0 the command, rows 1-20 its output, row 21 the prompt.
    prompt_row = 21
    assert read_screen(m).rows[prompt_row].startswith("lumen>")
    before = Image.open(m.screendump("before.png")).convert("RGB")
    path = [
        (GRID_X, GRID_Y),
        (GRID_X + 3, GRID_Y + 5 * CELL_H + 7),
        (GRID_X + 9 * CELL_W + 2, GRID_Y + 12 * CELL_H),
        (300, 100),
        (600, 130),
        (1150, 40),
        (1210, 50),
        (GRID_X + 4, GRID_Y - 6),
        (GRID_X + 5 * CELL_W, GRID_Y + prompt_row * CELL_H + 3),
    ]
    for x, y in path:
        m.move_pointer(x, y)
        wait_mouse(m, x=str(x), y=str(y), pointer="drawn")
    # Typed below the arrow (its tip on the prompt row): the line, then
    # its output and a new prompt are drawn under and around it.
    m.type_text("echo under the pointer\n")
    expect_screen(m, r"^lumen> echo under the pointer\nunder the pointer\nlumen>$")
    assert read_screen(m).rows[prompt_row] == "lumen> echo under the pointer"
    wait_mouse(m, pointer="drawn")
    # A command drawing on the screen: hidden meanwhile, back after.
    m.type_text("life glider 40\n")
    expect_screen(m, r"^life: press a key$", 120)
    wait_mouse(m, pointer="hidden")
    m.press(["x"])
    expect_screen(m, r"^life: press a key\nlumen>$")
    wait_mouse(m, pointer="drawn")
    park = (1200, 720)
    m.move_pointer(*park)
    wait_mouse(m, x=str(park[0]), y=str(park[1]), pointer="drawn")
    after = Image.open(m.screendump("after.png")).convert("RGB")
    screen = read_screen(m)
    m.send_line("vt 2")
    m.expect(r"vt: terminal 2 shown\n", 10)
    m.send_line("vt 1")
    m.expect(r"vt: terminal 1 shown\n", 10)
    fresh = Image.open(m.screendump("fresh.png")).convert("RGB")
    # The arrow is drawn at the parked spot in both (its tip is outline).
    assert after.getpixel(park) == (0, 0, 0)
    assert fresh.getpixel(park) == (0, 0, 0)
    # The text cursor blinks: its cell may differ.
    box = cursor_box(screen)
    for img in (after, fresh):
        img.paste((0, 0, 0), box)
    diff, mask = compare(after, fresh, tolerance=0)
    if diff.differing:
        mask.save(artifacts / "trail.diff.png")
    assert diff.differing == 0, (
        f"{diff.differing} pixels differ from a fresh redraw (trail.diff.png)"
    )
    # `vt` redraws below the header and its boxes only, so a trail across
    # the rest of the header would survive it: the header must be as it
    # was before the arrow ever showed.
    header = (0, 0, 1280, GRID_Y)
    assert pixels(after, header) == pixels(before, header)


# A burst's frames by size (events, the sync included): moves (x, y), or
# moves with 4 buttons pressed and released in turn. A 7-event frame
# separates the tablet's bound (12: a batch of 21 events or more counts)
# from moves' (3: 30 or more): the ring holds 4 of them (28 events), so a
# batch that read a full ring and nothing more counts only by the bound.
BURST_BUTTONS = ("left", "right", "middle", "side")
# The event buffers the tablet's driver keeps posted (kernel/src/mouse.rs).
RING = 32
# Frames sent past the ring's room: each is dropped.
PAST_THE_RING = 5


def burst_frame(i: int, frame: int, size: tuple[int, int]) -> dict:
    """Frame `i` of a burst of `frame`-event frames (3 or 7)."""
    args = pointer_move_args(100 + i % 2, 100 + i % 500, size)
    if frame == 7:
        down = i % 2 == 0
        buttons = [{"type": "btn", "data": {"down": down, "button": b}} for b in BURST_BUTTONS]
        args["events"] = buttons + args["events"]
    return args


def overflow_held(m: Machine, frame: int, warn: bool) -> None:
    """Hold the mouse thread, send `frame`-event frames until PAST_THE_RING
    past the ring's room (each of those dropped, traced at its send),
    release it, and wait for `mouse` to show exactly the frames that fit
    read in one batch and one more overrun. Hold and release are logged
    (INFO). `warn`: the overrun warning (pinned to that batch) may come
    with the release and the counts after it; otherwise no warning may
    come at all."""
    held = run(m, "mouse hold")
    assert held == "mouse: held: reading nothing until `mouse release`\n"
    assert any(
        re.search(r"INFO  mouse: \S+: held: the mouse thread reads nothing", log)
        for log in held.logs
    )
    events, frames, ignored, dropped, overruns = mouse_counts(m, held=True)
    fit = RING // frame
    log = len(m.trace_log())
    for i in range(fit + PAST_THE_RING):
        m.qmp().send_input(burst_frame(i, frame, m.screen_size()))
        dropped_now = m.trace_log(log).count("virtio_input_queue_full")
        # Synchronous: the drop of a frame is logged before QMP answers.
        assert dropped_now == max(0, i + 1 - fit), (i, dropped_now)
    # Held: nothing was read meanwhile.
    assert mouse_counts(m, held=True) == (events, frames, ignored, dropped, overruns)
    allow = held_overrun_warn(frame) if warn else None
    released = run(m, "mouse release", allow=allow)
    assert released == "mouse: released\n"
    assert any(
        re.search(r"INFO  mouse: \S+: released: the mouse thread reads again", log)
        for log in released.logs
    )
    want = (events + frame * fit, frames + fit, ignored, dropped, overruns + 1)
    got = poll(lambda: mouse_counts(m, allow), lambda c: c == want, m, 30)
    assert got == want, (got, want)


@pytest.mark.parametrize("frame", [3, 7], ids=["moves", "buttons"])
def test_a_burst_of_moves_counts_overruns(boot, frame):
    # With the mouse thread held (`mouse hold`: parked, reading nothing,
    # until `mouse release`; `mouse` says so; both logged), a burst fills
    # the 32-buffer ring whatever the host's speed (#142: waiting for a busy
    # guest to fall behind didn't always overflow it under KVM). QEMU keeps
    # the frames that fit (10 moves, or 4 with buttons) and drops each later
    # one whole (traced as virtio_input_queue_full, one line per frame,
    # logged before QMP answers) without telling the guest. Released, the
    # thread reads the full ring in one batch: exactly the frames that fit,
    # every event used (none ignored or SYN_DROPPED), one overrun counted
    # (see lumen_input::may_have_lost_frames), the state stale, the warning
    # naming that batch's events and the hold. A paced move afterwards is a
    # clean frame: the state isn't stale. A second held overflow is counted
    # too, but not warned about (once per boot).
    m = boot("qemutest", devices=Devices(tablet=True), trace_events=["virtio_input_queue_full"])
    tablet_ready(m)
    overflow_held(m, frame, warn=True)
    assert len(re.findall(held_overrun_warn(frame), m.output)) == 1
    assert mouse(m)[1].stale == "yes"
    m.move_pointer(640, 400)
    assert wait_mouse(m, x="640", y="400").stale == "no"
    overflow_held(m, frame, warn=False)
    assert len(re.findall(overrun_warn(), m.output)) == 1
    # No frame was dropped outside the held windows (the paced move fit).
    assert m.trace_log().count("virtio_input_queue_full") == 2 * PAST_THE_RING


def test_drag_steps_are_confirmed(boot):
    # lumen_harness.gui's drag (#149) waits for each frame to be read
    # before the next: a drag across the screen in 12 steps is 15 frames
    # (the move to its start, the press, the steps, the release), each one
    # counted by `mouse` and nothing else read, every event used, no
    # overrun; the pointer ends at the drag's end with the button released
    # and its press counted. A step the guest can't read (the mouse thread
    # held: `mouse hold`) fails at once, naming the step and the counts it
    # saw, and sends nothing; once released, the step goes through.
    m = boot("qemutest", devices=Devices(tablet=True))
    tablet_ready(m)
    before = gui.read_pointer(m)
    assert before.frames == 0 and before.position is None, before
    end = gui.drag(m, (40, 60), (1240, 740), 12)
    assert end.frames - before.frames == 12 + 3, end
    assert (end.position, end.buttons) == ((1240, 740), ()), end
    assert (end.events, end.ignored, end.dropped, end.overruns) == (3 * 13 + 2 + 2, 0, 0, 0), end
    row = mouse(m)[1]
    assert (row.presses, row.stale) == ("1", "no"), row
    assert run(m, "mouse hold") == "mouse: held: reading nothing until `mouse release`\n"
    with pytest.raises(gui.StepNotConfirmed) as e:
        gui.pointer_move(m, 640, 400, timeout=1)
    assert str(e.value) == (
        "move to (640, 400): the guest can't read its frame (the tablet is held or failed): "
        "mouse counted 15 frames (43 events, 0 ignored, 0 dropped, 0 overruns), held"
    ), e.value
    assert run(m, "mouse release") == "mouse: released\n"
    after = gui.pointer_move(m, 640, 400)
    assert (after.frames, after.position) == (16, (640, 400)), after


def events(m: Machine, command: str) -> list[list[str]]:
    """`mouse events n` or `kbd events n`: its rows, each split into its
    cells (the header checked against the columns)."""
    lines = run(m, command).splitlines()
    columns = {
        "mouse": ["seq", "x", "y", "buttons", "after_overrun"],
        "kbd": ["seq", "code", "action", "text", "mods"],
    }[command.split()[0]]
    assert lines and lines[0].split() == columns, lines
    return [re.split(r"\s{2,}", line.strip()) for line in lines[1:]]


def test_click_position_is_kept(boot):
    # (#151) With the mouse thread held, a move to (x1, y1), a press there,
    # a move to (x2, y2) and a release there wait in the device's ring;
    # released, the thread reads them all in one batch, yet the pointer
    # ring has a frame for each, in order, numbered one after another: the
    # press where it was pressed and the release where it was released,
    # none marked as following frames lost at the device. `mouse` shows the
    # ring's newest number.
    m = boot("qemutest", devices=Devices(tablet=True))
    tablet_ready(m)
    assert run(m, "mouse events") == "  seq     x     y  buttons  after_overrun\n"
    assert run(m, "mouse hold") == "mouse: held: reading nothing until `mouse release`\n"
    (x1, y1), (x2, y2) = (100, 200), (900, 600)
    m.move_pointer(x1, y1)
    m.click("left", release=False)
    m.move_pointer(x2, y2)
    m.release("left")
    assert run(m, "mouse release") == "mouse: released\n"
    end = poll(lambda: gui.read_pointer(m), lambda r: r.frames >= 4, m)
    assert (end.frames, end.position, end.buttons, end.overruns) == (4, (x2, y2), (), 0), end
    rows = events(m, "mouse events 8")
    assert rows == [
        ["1", str(x1), str(y1), "none", "no"],
        ["2", str(x1), str(y1), "left", "no"],
        ["3", str(x2), str(y2), "left", "no"],
        ["4", str(x2), str(y2), "none", "no"],
    ], rows
    assert ", newest event 4)" in run(m, "mouse").splitlines()[0]
    # In a pipeline the same events are records.
    assert run(m, "mouse events | where buttons = left | select seq x y") == (
        f"  seq     x     y\n    2  {x1:>4}  {y1:>4}\n    3  {x2:>4}  {y2:>4}\n"
    )


def test_key_events_have_releases_and_modifiers(boot):
    # (#151) Shift+a typed on the keyboard is four key events in the key
    # ring, each with the modifiers after it: press LShift (shifted), press
    # A (shifted, text 'A'), release A (shifted), release LShift (not
    # shifted). Num Lock is on (pc-keyboard's start). The screen's shell
    # still gets its 'A' as before, and `kbd` shows the ring's newest
    # number.
    m = boot("qemutest")
    shells_ready(m, "us")
    assert run(m, "kbd events") == "  seq  code        action   text    mods\n"
    m.press(["shift", "a"])
    rows = poll(lambda: events(m, "kbd events 8"), lambda r: len(r) >= 4, m)
    assert rows == [
        ["1", "LShift", "press", "-", "shift+num"],
        ["2", "A", "press", "'A'", "shift+num"],
        ["3", "A", "release", "-", "shift+num"],
        ["4", "LShift", "release", "-", "num"],
    ], rows
    expect_screen(m, r"^lumen> A$")
    assert re.search(r", newest key event 4\n", run(m, "kbd"))
    assert run(m, "kbd events 2 | where action = press | count") == "0\n"
    assert run(m, "kbd events 4 | where action = press | count") == "2\n"


def test_mouse_hold_needs_a_tablet_and_takes_one_word(boot):
    # `mouse hold` without a tablet fails and says why; `mouse` takes only
    # `hold` or `release`; `mouse release` without a hold is harmless.
    m = boot("qemutest")
    shells_ready(m, "us")
    assert run(m, "mouse hold") == "mouse: can't hold: no tablet\n"
    assert run(m, "mouse release") == "mouse: released\n"
    for bad in ["mouse up", "mouse hold now"]:
        assert run(m, bad) == "usage: mouse [hold|release|events [n]]\n", bad


def test_a_pointer_move_raises_the_tablets_interrupt(boot):
    # The tablet interrupts through MSI-X (#96): its vector, `msix <addr>
    # #0` in `irqs`, counts up when a QMP move makes the device return an
    # event buffer (the 1 s status look would drain the ring too, but
    # raises no interrupt), and no interrupt is reported missing.
    m = boot("qemutest", devices=Devices(tablet=True))
    addr = tablet_ready(m)
    _, entry, before = msix_rows(m)[addr]
    assert entry == 0
    m.move_pointer(320, 200)
    wait_mouse(m, x="320", y="200")
    assert msix_rows(m)[addr][2] > before
    assert "interrupt never came" not in m.output


def test_tablet_is_bound_and_the_keyboard_is_not(boot):
    # virtio's keyboard and tablet share a PCI id; the tablet driver takes
    # only the tablet (by its class), so the keyboard is never offered to
    # it: `lspci` shows the tablet bound and the keyboard without a driver.
    # Only one tablet is used: a second one is declined (before its
    # transport is set up), and `mouse` names the first.
    m = boot(
        "qemutest",
        devices=Devices(tablet=True, keyboard=True),
        extra=["-device", "virtio-tablet-pci,id=lumen-tablet2"],
    )
    addr = tablet_ready(m)
    rows = lspci_rows(m)
    qdev = qdev_addresses(m)
    first, second = sorted([qdev["lumen-tablet"], qdev["lumen-tablet2"]])
    assert addr == first
    assert rows[first][4] == "virtio-tablet: bound"
    assert rows[second][4] == "no driver (virtio-tablet declined it)"
    assert rows[qdev["lumen-keyboard"]][4] == "no driver"
    assert re.search(
        r"INFO  drivers: 1 bound, 0 failed, 1 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert (
        f"INFO  mouse: {second}: a tablet is bound already (on {first}): only one is used\n"
        in m.output
    )
    assert re.findall(r"probing virtio-tablet on (\S+)\n", m.output) == [first, second]
    assert MOUSE_HEADER.fullmatch(mouse(m)[0])[1] == first
    assert "WARN" not in m.output


def test_an_input_device_without_absolute_axes_is_declined(boot):
    # QEMU's multitouch panel has the tablet's class (input, other) but
    # only multitouch axes: read from its configuration before the crate
    # posts any buffer, it is declined, and nothing is bound or leaked.
    m = boot("qemutest", extra=["-device", "virtio-multitouch-pci,id=lumen-mt"])
    shells_ready(m, "us")
    addr = qdev_addresses(m)["lumen-mt"]
    assert lspci_rows(m)[addr][4] == "no driver (virtio-tablet declined it)"
    assert f"INFO  mouse: {addr}: no absolute X and Y axes, so not a tablet\n" in m.output
    assert re.search(
        r"INFO  drivers: 0 bound, 0 failed, 1 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert mouse(m)[0] == "mouse: no tablet (QEMU: -device virtio-tablet-pci)"
    assert "WARN" not in m.output


def test_mouse_without_a_tablet(boot):
    m = boot("qemutest")
    shells_ready(m, "us")
    header, row = mouse(m)
    assert header == "mouse: no tablet (QEMU: -device virtio-tablet-pci)"
    assert row == Mouse("none", *NOTHING_YET)
    assert run(m, "mouse | select device pointer").split() == ["device", "pointer", "none", "-"]
    assert "INFO  mouse" not in m.output
