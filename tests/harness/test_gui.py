"""lumen_harness.gui: pointer and key steps that confirm each effect (#149),
against a fake machine whose serial shell answers `mouse` and `kbd` from
counters its keyboard and tablet advance (or, to check the failures,
don't)."""

import re

import pytest
from lumen_harness import gui
from lumen_harness.gui import (
    KeyCounts,
    PointerReport,
    StepNotConfirmed,
    confirmed_keys,
    drag,
    drag_path,
    key_steps,
    parse_kbd,
    parse_mouse,
    pointer_down,
    pointer_move,
    pointer_up,
    type_confirmed,
)
from lumen_harness.qemu import ExpectTimeout
from lumen_harness.shell import PROMPT

COLUMNS = "  device           x     y  buttons  presses  wheel  hwheel  pointer  stale  failed"
WARN = "[    1.000000] WARN  mouse: 0000:00:03.0: 30 events in one batch: the event ring was nearly full"


def mouse_text(
    frames: int,
    xy: tuple[int, int] | None,
    buttons: str = "none",
    *,
    held: str = "",
    failed: str | None = None,
) -> str:
    """`mouse`'s output as kernel/src/shell/sysinfo.rs prints it."""
    tail = held + (f", failed: {failed}" if failed else "")
    header = (
        f'mouse: "QEMU Virtio Tablet" on 0000:00:03.0, screen 1280x800, {3 * frames} events in '
        f"{frames} frames (0 ignored, 0 dropped, 0 overruns){tail}"
    )
    if xy is None:
        row = "  0000:00:03.0     -     -  -              -      -       -  -        -      -"
    else:
        row = (
            f"  0000:00:03.0  {xy[0]:>4}  {xy[1]:>4}  {buttons:<7}        0      0       0  "
            "drawn    no     -"
        )
    return f"{header}\n{COLUMNS}\n{row}\n"


def kbd_text(presses: int, dropped: int = 0) -> str:
    """`kbd`'s output as kernel/src/shell/commands.rs prints it."""
    return (
        f"kbd: layout us, IRQ 1 (vector 0x51), {2 * presses} interrupts, {2 * presses} bytes, "
        f"{presses} presses, {dropped} dropped, 0 undecoded, 0 line errors, 0 skipped at setup\n"
    )


class FakeMachine:
    """A serial shell answering `mouse` and `kbd` (each after its echo, then
    the prompt), a tablet whose frames count unless the event is in
    `lost_frames` (its index, from 0), and a keyboard whose keys count unless the step
    is in `lost_keys` (from 1), or whose queue drops bytes from step
    `drops_at` on."""

    timeout_scale = 1.0

    def __init__(
        self,
        *,
        lost_frames: set[int] = frozenset(),
        lost_keys: set[int] = frozenset(),
        drops_at: int | None = None,
        no_effect: set[int] = frozenset(),
        warn_with: str | None = None,
    ) -> None:
        self.output = PROMPT
        self._pos = 0
        self.sent: list[str] = []
        self.pointer_events: list[tuple] = []
        self.key_presses: list[list[str]] = []
        self.frames = 0
        self.xy: tuple[int, int] | None = None
        self.buttons: list[str] = []
        self.presses = 0
        self.dropped = 0
        self.lost_frames = lost_frames
        self.lost_keys = lost_keys
        self.drops_at = drops_at
        # Pointer events (from 0) whose frame counts but whose effect never
        # shows.
        self.no_effect = no_effect
        self.warn_with = warn_with

    # -- serial ---------------------------------------------------------
    def send_line(self, line: str) -> None:
        self.sent.append(line)
        if line == "mouse":
            order = [b for b in gui.HELD_BUTTONS if b in self.buttons]
            out = mouse_text(self.frames, self.xy, " ".join(order) or "none")
        elif line == "kbd":
            out = kbd_text(self.presses, self.dropped)
        else:
            out = f"unknown command: {line}\n"
        if self.warn_with == line:
            out += WARN + "\n"
        self.output += f"{line}\n{out}{PROMPT}"

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        found = re.compile(pattern).search(self.output, self._pos)
        if not found:
            raise ExpectTimeout(f"timeout waiting for {pattern!r}")
        self._pos = found.end()
        return found

    # -- tablet ---------------------------------------------------------
    def _frame(self, apply) -> None:
        n = len(self.pointer_events) - 1  # this event's index
        if n in self.lost_frames:
            return
        self.frames += 1
        if n not in self.no_effect:
            apply()

    def move_pointer(self, x: int, y: int) -> None:
        self.pointer_events.append(("move", x, y))
        self._frame(lambda: setattr(self, "xy", (x, y)))

    def click(self, button: str = "left", release: bool = True) -> None:
        assert not release, "the confirmed steps press and release apart"
        self.pointer_events.append(("down", button))
        self._frame(lambda: self.buttons.append(button))

    def release(self, button: str = "left") -> None:
        self.pointer_events.append(("up", button))
        self._frame(lambda: self.buttons.remove(button))

    # -- keyboard -------------------------------------------------------
    def press(self, qcodes) -> None:
        self.key_presses.append(list(qcodes))
        n = len(self.key_presses)
        if self.drops_at is not None and n >= self.drops_at:
            self.dropped += 2
        if n not in self.lost_keys:
            self.presses += len(qcodes)


@pytest.fixture(autouse=True)
def fast(monkeypatch):
    # The fakes answer at once: no need to wait between readings.
    monkeypatch.setattr(gui, "POLL", 0.0)


# -- parsing --------------------------------------------------------------


def test_mouse_is_parsed_before_and_after_the_first_frame():
    assert parse_mouse(mouse_text(0, None)) == PointerReport(0, 0, 0, 0, "", None, None, ())
    assert parse_mouse(mouse_text(7, (900, 500), "left right")) == PointerReport(
        21, 7, 0, 0, "", None, (900, 500), ("left", "right")
    )
    held = parse_mouse(mouse_text(2, (1, 2), held=", held", failed="FAILED"))
    assert (held.held, held.failed) == ("held", "FAILED")
    assert held.counts() == "2 frames (6 events, 0 dropped, 0 overruns), held, failed: FAILED"


def test_mouse_without_a_tablet_or_an_unknown_header_fails():
    with pytest.raises(AssertionError, match="a header, the columns and a row"):
        parse_mouse("mouse: no tablet (QEMU: -device virtio-tablet-pci)\n")
    bad = mouse_text(1, (1, 1)).replace(" frames", " framez")
    with pytest.raises(AssertionError, match="a header this doesn't know"):
        parse_mouse(bad)


def test_kbd_is_parsed_and_a_missing_keyboard_fails():
    assert parse_kbd(kbd_text(5, 2)) == KeyCounts(5, 2)
    with pytest.raises(AssertionError, match="no keyboard"):
        parse_kbd("kbd: no keyboard (no reply in time)\n")


# -- pointer --------------------------------------------------------------


def test_each_pointer_step_sends_one_event_and_reads_mouse_after_it():
    m = FakeMachine()
    r = pointer_move(m, 10, 20)
    assert (r.frames, r.position) == (1, (10, 20))
    r = pointer_down(m, "right")
    assert (r.frames, r.buttons) == (2, ("right",))
    r = pointer_up(m, "right")
    assert (r.frames, r.buttons) == (3, ())
    assert m.pointer_events == [("move", 10, 20), ("down", "right"), ("up", "right")]
    # One reading before each step, one (at least) after.
    assert m.sent == ["mouse"] * 6


def test_drag_moves_presses_steps_and_releases_each_confirmed():
    m = FakeMachine()
    r = drag(m, (100, 100), (400, 250), 3)
    assert m.pointer_events == [
        ("move", 100, 100),
        ("down", "left"),
        ("move", 200, 150),
        ("move", 300, 200),
        ("move", 400, 250),
        ("up", "left"),
    ]
    assert (r.frames, r.position, r.buttons) == (6, (400, 250), ())
    # The first reading, then one after each of the 6 steps (and no more:
    # each was read on the first try).
    assert m.sent == ["mouse"] * 7


def test_drag_path_is_even_and_ends_at_the_end():
    assert drag_path((0, 0), (10, 5), 4) == [(2, 1), (5, 2), (8, 4), (10, 5)]
    assert drag_path((10, 10), (0, 0), 1) == [(0, 0)]
    assert drag_path((5, 5), (5, 5), 2) == [(5, 5), (5, 5)]
    with pytest.raises(ValueError, match="at least one step"):
        drag_path((0, 0), (1, 1), 0)


def test_a_frame_the_guest_never_reads_fails_naming_the_step_and_counts():
    # The fourth event (step 2 of 3's move) is lost: the drag stops there.
    m = FakeMachine(lost_frames={3})
    with pytest.raises(StepNotConfirmed) as e:
        drag(m, (0, 0), (30, 30), 3, timeout=0.05)
    assert str(e.value).startswith(
        "drag: step 2 of 3, to (20, 20): the guest never read its frame in 0.05s: mouse counted "
        "3 frames (9 events, 0 dropped, 0 overruns), expected 4 frames (3 before the step); "
        "it shows at (10, 10), buttons left"
    ), e.value
    # Nothing after the lost step was sent.
    assert len(m.pointer_events) == 4


def test_a_frame_read_without_its_effect_fails_saying_what_was_missing():
    m = FakeMachine(no_effect={0})
    with pytest.raises(StepNotConfirmed) as e:
        pointer_move(m, 7, 8, timeout=0.05)
    assert str(e.value) == (
        "move to (7, 8): the guest read 1 frame(s) but never showed the pointer at (7, 8) in "
        "0.05s: mouse shows no frame yet, 1 frames (3 events, 0 dropped, 0 overruns)"
    )
    m = FakeMachine(no_effect={1})
    pointer_move(m, 1, 1)
    with pytest.raises(StepNotConfirmed, match=r"^press middle: .* never showed middle held"):
        pointer_down(m, "middle", timeout=0.05)


def test_a_scaled_timeout_is_said_both_ways():
    m = FakeMachine(lost_frames={0})
    m.timeout_scale = 4.0
    with pytest.raises(StepNotConfirmed, match=r"in 0\.2s \(0\.05s scaled by 4\)"):
        pointer_move(m, 1, 1, timeout=0.05)


def test_wheel_buttons_cant_be_held():
    with pytest.raises(ValueError, match="no pointer button to hold 'wheel-up'"):
        pointer_down(FakeMachine(), "wheel-up")
    with pytest.raises(ValueError, match="no pointer button to hold"):
        drag(FakeMachine(), (0, 0), (1, 1), 1, button="wheel-down")


def test_a_warning_while_reading_mouse_fails_unless_allowed():
    with pytest.raises(AssertionError, match="a warning or error came with its output"):
        pointer_move(FakeMachine(warn_with="mouse"), 1, 1)
    r = pointer_move(FakeMachine(warn_with="mouse"), 1, 1, allow=r"WARN  mouse")
    assert r.position == (1, 1)


# -- keys -----------------------------------------------------------------


def test_key_steps_split_text_into_characters_and_keep_lists_together():
    assert key_steps("aB", ["ctrl", "a"], "\n") == [
        ("'a'", ["a"]),
        ("'B'", ["shift", "b"]),
        ("ctrl+a", ["ctrl", "a"]),
        ("'\\n'", ["ret"]),
    ]
    with pytest.raises(ValueError, match="at least one"):
        key_steps([])


def test_each_key_is_confirmed_before_the_next():
    m = FakeMachine()
    got = confirmed_keys(m, "Hi", ["left"])
    assert m.key_presses == [["shift", "h"], ["i"], ["left"]]
    assert got == KeyCounts(4, 0)
    # A reading before the first key, then one after each key.
    assert m.sent == ["kbd"] * 4
    assert type_confirmed(FakeMachine(), "") == KeyCounts(0, 0)


def test_a_key_that_never_arrives_is_named():
    m = FakeMachine(lost_keys={3})
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abcd", timeout=0.05)
    assert str(e.value) == (
        "key 3 of 4 ('c') never reached the guest in 0.05s: kbd counted 2 presses since the "
        "first key, expected 3"
    )
    # The fourth key was never typed.
    assert m.key_presses == [["a"], ["b"], ["c"]]


def test_keys_held_together_count_each_key():
    # shift+a is two presses: one missing (here the whole step) names it.
    m = FakeMachine(lost_keys={2})
    with pytest.raises(StepNotConfirmed, match=r"^key 2 of 2 \('A'\) never .* expected 3$"):
        type_confirmed(m, "aA", timeout=0.05)


def test_a_dropped_byte_fails_the_key_even_if_it_arrived():
    m = FakeMachine(drops_at=2)
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abc", timeout=0.05)
    assert str(e.value) == (
        "key 2 of 3 ('b'): the keyboard's queue dropped 2 byte(s) since the first key (its "
        "shell fell behind): a key reached the guest but not its shell"
    )
