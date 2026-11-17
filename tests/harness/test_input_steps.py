"""lumen_harness.gui: pointer and key steps that confirm each effect (#149),
against a fake machine whose serial shell answers `mouse` and `kbd` from
counters its keyboard and tablet advance (or, to check the failures,
don't), on a fake clock."""

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
    key_bytes,
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


# `mouse` without a tablet, as kernel/src/shell/sysinfo.rs prints it.
NO_TABLET = (
    "mouse: no tablet (QEMU: -device virtio-tablet-pci)\n"
    f"{COLUMNS}\n"
    "  none             -     -  -              -      -       -  -        -      -\n"
)


def kbd_text(bytes_: int, presses: int, dropped: int = 0, undecoded: int = 0, line: int = 0) -> str:
    """`kbd`'s output as kernel/src/shell/commands.rs prints it."""
    return (
        f"kbd: layout us, IRQ 1 (vector 0x51), {bytes_} interrupts, {bytes_} bytes, "
        f"{presses} presses, {dropped} dropped, {undecoded} undecoded, {line} line errors, "
        "0 skipped at setup\n"
    )


class Clock:
    """`gui`'s time: `sleep` moves it on, nothing else does."""

    def __init__(self) -> None:
        self.now = 1000.0

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        assert seconds >= 0
        self.now += seconds


@pytest.fixture(autouse=True)
def clock(monkeypatch) -> Clock:
    c = Clock()
    monkeypatch.setattr(gui, "time", c)
    return c


class FakeMachine:
    """A serial shell answering `mouse` and `kbd` (each after its echo, then
    the prompt), a tablet and a keyboard.

    Each pointer event (`pointer_events`, from 0) is one frame, unless its
    index is in `lost_frames`; one in `no_effect` counts but changes
    nothing. Each key press (from 1) counts its keys and `key_bytes`
    bytes, unless in `lost_keys`; `short_bytes` takes bytes off a press's
    (a lost prefix or release); from press `drops_at` on, each press drops
    two bytes; one in `undecoded_at` makes a byte the decoder refuses;
    one in `release_dropped` shows its press first, and a reading later
    its last byte dropped (not queued, so not in `bytes`), as the kernel
    counts a release the full queue had no room for.
    What a step sends shows `delay` readings later. `arrivals` adds
    (presses, bytes, frames) before the reading of that number (from 1,
    counting `mouse` and `kbd` together): input still arriving."""

    timeout_scale = 1.0

    def __init__(
        self,
        *,
        lost_frames: set[int] = frozenset(),
        no_effect: set[int] = frozenset(),
        lost_keys: set[int] = frozenset(),
        short_bytes: dict[int, int] | None = None,
        drops_at: int | None = None,
        undecoded_at: set[int] = frozenset(),
        release_dropped: set[int] = frozenset(),
        delay: int = 0,
        arrivals: dict[int, tuple[int, int, int]] | None = None,
        answers: dict[str, str] | None = None,
        warn_with: str | None = None,
    ) -> None:
        self.output = PROMPT
        self._pos = 0
        self._keys_sent_by = 0.0
        self.sent: list[str] = []
        self.prompt_timeouts: list[float] = []
        self.pointer_events: list[tuple] = []
        self.key_presses: list[list[str]] = []
        self.frames = 0
        self.xy: tuple[int, int] | None = None
        self.buttons: list[str] = []
        self.counts = {"presses": 0, "bytes": 0, "dropped": 0, "undecoded": 0}
        self.held = ""
        self.lost_frames = lost_frames
        self.no_effect = no_effect
        self.lost_keys = lost_keys
        self.short_bytes = short_bytes or {}
        self.drops_at = drops_at
        self.undecoded_at = undecoded_at
        self.release_dropped = release_dropped
        self.delay = delay
        self.arrivals = arrivals or {}
        self.answers = answers or {}
        self.warn_with = warn_with
        self.reads = 0
        # (the reading it shows at, what to do)
        self._pending: list[tuple[int, object]] = []

    def _later(self, apply, extra: int = 0) -> None:
        self._pending.append((self.reads + self.delay + extra, apply))

    # -- serial ---------------------------------------------------------
    def send_line(self, line: str) -> None:
        self.sent.append(line)
        self.reads += 1
        for at, apply in [p for p in self._pending if p[0] <= self.reads]:
            self._pending.remove((at, apply))
            apply()
        presses, bytes_, frames = self.arrivals.get(self.reads, (0, 0, 0))
        self.counts["presses"] += presses
        self.counts["bytes"] += bytes_
        self.frames += frames
        if line in self.answers:
            out = self.answers[line]
        elif line == "mouse":
            order = [b for b in gui.HELD_BUTTONS if b in self.buttons]
            out = mouse_text(self.frames, self.xy, " ".join(order) or "none", held=self.held)
        elif line == "kbd":
            c = self.counts
            out = kbd_text(c["bytes"], c["presses"], c["dropped"], c["undecoded"])
        else:
            out = f"unknown command: {line}\n"
        if self.warn_with == line:
            out += WARN + "\n"
        self.output += f"{line}\n{out}{PROMPT}"

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        if pattern == PROMPT:
            self.prompt_timeouts.append(timeout)
        found = re.compile(pattern).search(self.output, self._pos)
        if not found:
            raise ExpectTimeout(f"timeout waiting for {pattern!r}")
        self._pos = found.end()
        return found

    # -- tablet ---------------------------------------------------------
    def _frame(self, event: tuple, apply) -> None:
        self.pointer_events.append(event)
        n = len(self.pointer_events) - 1
        if n in self.lost_frames:
            return

        def show() -> None:
            self.frames += 1
            if n not in self.no_effect:
                apply()

        self._later(show)

    def move_pointer(self, x: int, y: int) -> None:
        self._frame(("move", x, y), lambda: setattr(self, "xy", (x, y)))

    def click(self, button: str = "left", release: bool = True) -> None:
        assert not release, "the confirmed steps press and release apart"
        self._frame(("down", button), lambda: self.buttons.append(button))

    def release(self, button: str = "left") -> None:
        self._frame(("up", button), lambda: self.buttons.remove(button))

    # -- keyboard -------------------------------------------------------
    def press(self, qcodes) -> None:
        self.key_presses.append(list(qcodes))
        n = len(self.key_presses)
        c = self.counts

        def show() -> None:
            if self.drops_at is not None and n >= self.drops_at:
                c["dropped"] += 2
            if n in self.undecoded_at:
                c["undecoded"] += 1
            if n not in self.lost_keys:
                c["presses"] += len(qcodes)
                c["bytes"] += key_bytes(qcodes) - self.short_bytes.get(n, 0)
                if n in self.release_dropped:
                    c["bytes"] -= 1

        def drop_release() -> None:
            c["dropped"] += 1

        self._later(show)
        if n in self.release_dropped:
            self._later(drop_release, 1)


# -- parsing --------------------------------------------------------------


def test_mouse_is_parsed_before_and_after_the_first_frame():
    assert parse_mouse(mouse_text(0, None)) == PointerReport(0, 0, 0, 0, 0, "", None, None, ())
    assert parse_mouse(mouse_text(7, (900, 500), "left right")) == PointerReport(
        21, 7, 0, 0, 0, "", None, (900, 500), ("left", "right")
    )
    held = parse_mouse(mouse_text(2, (1, 2), held=", held", failed="FAILED"))
    assert (held.held, held.failed) == ("held", "FAILED")
    assert held.counts() == (
        "2 frames (6 events, 0 ignored, 0 dropped, 0 overruns), held, failed: FAILED"
    )
    assert parse_mouse(mouse_text(1, (1, 2), held=", holding")).held == "holding"


def test_mouse_without_a_tablet_or_an_unknown_header_fails():
    with pytest.raises(AssertionError, match=r"^mouse: no tablet \(boot with Devices"):
        parse_mouse(NO_TABLET)
    bad = mouse_text(1, (1, 1)).replace(" frames", " framez")
    with pytest.raises(AssertionError, match="a header this doesn't know"):
        parse_mouse(bad)
    with pytest.raises(AssertionError, match="a header, the columns and a row"):
        parse_mouse(mouse_text(1, (1, 1)) + "extra\n")


def test_kbd_is_parsed_and_a_missing_keyboard_fails():
    assert parse_kbd(kbd_text(10, 5, 2, 1, 3)) == KeyCounts(10, 5, 2, 1, 3)
    with pytest.raises(AssertionError, match="no keyboard"):
        parse_kbd("kbd: no keyboard (no reply in time)\n")


def test_key_bytes_follow_qemus_ps2_keyboard():
    assert key_bytes(["a"]) == 2
    assert key_bytes(["shift", "a"]) == 4
    assert key_bytes(["left"]) == 4
    assert key_bytes(["ctrl", "alt", "delete"]) == 8
    assert key_bytes(["print"]) == 8
    assert key_bytes(["pause"]) == 6
    with pytest.raises(ValueError, match="Print Screen or Pause with other keys"):
        key_bytes(["shift", "print"])
    with pytest.raises(ValueError, match="Print Screen or Pause with other keys"):
        key_bytes(["ctrl", "pause"])


def test_held_buttons_are_qmps_without_the_wheel():
    assert gui.HELD_BUTTONS == ("left", "middle", "right", "side", "extra")


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
    # Two agreeing readings before each step, one after it.
    assert m.sent == ["mouse"] * 9


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
    # Two agreeing readings, then one after each of the 6 steps (each read
    # on the first try).
    assert m.sent == ["mouse"] * 8


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
    assert str(e.value) == (
        "drag: step 2 of 3, to (20, 20): the guest never read its frame in 0.05s: mouse "
        "counted 3 frames (9 events, 0 ignored, 0 dropped, 0 overruns), expected 4 frames "
        "(3 before the step); it shows at (10, 10), buttons left"
    )
    assert (e.value.step, e.value.index, e.value.total) == ("drag: step 2 of 3, to (20, 20)", 4, 6)
    assert e.value.last.frames == 3
    # Nothing after the lost step was sent.
    assert len(m.pointer_events) == 4


def test_a_lost_frame_fails_even_where_its_effect_already_shows():
    # The pointer is at (5, 5) already, so only the frame count tells the
    # second move's frame never came.
    m = FakeMachine(lost_frames={1})
    pointer_move(m, 5, 5)
    with pytest.raises(
        StepNotConfirmed, match=r"^move to \(5, 5\): the guest never read .*expected 2 frames"
    ):
        pointer_move(m, 5, 5, timeout=0.05)
    m = FakeMachine(lost_frames={3})
    with pytest.raises(
        StepNotConfirmed, match=r"^drag: step 2 of 2, to \(5, 5\): the guest never read"
    ):
        drag(m, (5, 5), (5, 5), 2, timeout=0.05)


def test_a_frame_read_without_its_effect_fails_saying_what_was_missing():
    m = FakeMachine(no_effect={0})
    with pytest.raises(StepNotConfirmed) as e:
        pointer_move(m, 7, 8, timeout=0.05)
    assert str(e.value) == (
        "move to (7, 8): the guest read its frame but never showed the pointer at (7, 8) in "
        "0.05s: mouse shows no frame yet, 1 frames (3 events, 0 ignored, 0 dropped, 0 overruns)"
    )
    m = FakeMachine(no_effect={1})
    pointer_move(m, 1, 1)
    with pytest.raises(StepNotConfirmed, match=r"^press middle: .* never showed middle held"):
        pointer_down(m, "middle", timeout=0.05)


def test_a_frame_more_than_sent_fails():
    # A frame sent before the step arrives during it (after the readings
    # before it agreed): the step can't tell which was its own.
    m = FakeMachine(arrivals={3: (0, 0, 1)})
    with pytest.raises(
        StepNotConfirmed, match=r"^move to \(1, 1\): mouse counted 2 frames since the step, 1 sent"
    ):
        pointer_move(m, 1, 1)


def test_frames_still_arriving_settle_before_the_first_reading():
    # Frames sent earlier show over the first readings: the step counts
    # from where they stopped.
    m = FakeMachine(arrivals={1: (0, 0, 1), 2: (0, 0, 1)})
    r = pointer_move(m, 3, 4)
    assert r.frames == 3
    # Three readings to agree (1, 2, 2), one after the move.
    assert m.sent == ["mouse"] * 4


def test_a_held_tablet_fails_the_step_at_once(clock):
    m = FakeMachine()
    pointer_move(m, 1, 1)
    m.held = ", held"
    start = clock.now
    with pytest.raises(StepNotConfirmed) as e:
        pointer_move(m, 2, 2, timeout=10)
    assert str(e.value) == (
        "move to (2, 2): the guest can't read its frame (the tablet is held or failed): mouse "
        "counted 1 frames (3 events, 0 ignored, 0 dropped, 0 overruns), held"
    )
    assert clock.now - start < 1, "it didn't wait out the timeout"
    # Nothing was sent to a tablet that can't read it.
    assert m.pointer_events == [("move", 1, 1)]


def test_a_tablet_held_during_the_step_fails_it_without_waiting(clock):
    m = FakeMachine(delay=3)
    pointer_move(m, 1, 1)
    start = clock.now
    original = m.move_pointer

    def move_then_hold(x: int, y: int) -> None:
        original(x, y)
        m.held = ", held"
        m._pending.clear()

    m.move_pointer = move_then_hold
    with pytest.raises(
        StepNotConfirmed, match=r"never read its frame \(the tablet is held or failed\)"
    ):
        pointer_move(m, 2, 2, timeout=10)
    assert clock.now - start < 1


def test_a_scaled_timeout_is_waited_out_and_said_both_ways(clock):
    m = FakeMachine(lost_frames={0})
    m.timeout_scale = 4.0
    start = clock.now
    with pytest.raises(StepNotConfirmed, match=r"in 8s \(2s scaled by 4\)"):
        pointer_move(m, 1, 1, timeout=2)
    waited = clock.now - start
    assert 8 <= waited < 9, waited


def test_a_step_polls_until_the_frame_shows():
    # The frame shows only on the 6th reading after it was sent.
    m = FakeMachine(delay=6)
    r = pointer_move(m, 9, 9)
    assert (r.frames, r.position) == (1, (9, 9))
    assert m.sent.count("mouse") >= 2 + 6


def test_wheel_buttons_cant_be_held():
    with pytest.raises(ValueError, match="no pointer button to hold 'wheel-up'"):
        pointer_down(FakeMachine(), "wheel-up")
    with pytest.raises(ValueError, match="no pointer button to hold"):
        drag(FakeMachine(), (0, 0), (1, 1), 1, button="wheel-down")


def test_a_warning_while_reading_mouse_fails_the_step_unless_allowed():
    with pytest.raises(
        StepNotConfirmed, match=r"^move to \(1, 1\): reading mouse failed: .*a warning or error"
    ):
        pointer_move(FakeMachine(warn_with="mouse"), 1, 1)
    r = pointer_move(FakeMachine(warn_with="mouse"), 1, 1, allow=r"WARN  mouse")
    assert r.position == (1, 1)


def test_no_tablet_fails_the_step_naming_it():
    with pytest.raises(
        StepNotConfirmed, match=r"^press left: reading mouse failed: mouse: no tablet"
    ):
        pointer_down(FakeMachine(answers={"mouse": NO_TABLET}))


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
    assert got == KeyCounts(10, 4, 0, 0, 0)
    # Two agreeing readings before the first key, then one after each key.
    assert m.sent == ["kbd"] * 5
    assert type_confirmed(FakeMachine(), "") == KeyCounts(0, 0, 0, 0, 0)


def test_a_key_that_never_arrives_is_named():
    m = FakeMachine(lost_keys={3})
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abcd", timeout=0.05)
    assert str(e.value) == (
        "key 3 of 4 ('c') never reached the guest in 0.05s: kbd counted 2 presses and 4 bytes "
        "since before key 1, expected 3 and 6"
    )
    assert (e.value.step, e.value.index, e.value.total) == ("key 3 of 4 ('c')", 3, 4)
    assert e.value.last == KeyCounts(4, 2, 0, 0, 0)
    # The fourth key was never typed.
    assert m.key_presses == [["a"], ["b"], ["c"]]


def test_keys_held_together_count_each_key():
    # shift+a is two presses: one missing (here the whole step) names it.
    m = FakeMachine(lost_keys={2})
    with pytest.raises(StepNotConfirmed, match=r"^key 2 of 2 \('A'\) never .* expected 3 and 6$"):
        type_confirmed(m, "aA", timeout=0.05)


def test_a_lost_prefix_or_release_fails_the_key():
    # Left's E0 prefix lost: its press still counts (as keypad 4), its
    # bytes fall one short.
    m = FakeMachine(short_bytes={2: 1})
    with pytest.raises(StepNotConfirmed) as e:
        confirmed_keys(m, "a", ["left"], "b", timeout=0.05)
    assert str(e.value) == (
        "key 2 of 3 (left): 3 of its 4 bytes arrived in 0.05s: a prefix or release was lost "
        "(kbd counted 2 presses and 5 bytes since before key 1, expected 2 and 6)"
    )


def test_a_dropped_or_undecoded_byte_fails_the_key():
    m = FakeMachine(drops_at=2)
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abc", timeout=0.05)
    assert str(e.value).startswith(
        "key 2 of 3 ('b'): since before key 1 the keyboard's queue dropped 2 byte(s), the "
        "decoder refused 0 and 0 came with a line error: a key typed won't reach its shell whole"
    ), e.value
    with pytest.raises(StepNotConfirmed, match=r"^key 1 of 2 \('a'\): .* the decoder refused 1"):
        type_confirmed(FakeMachine(undecoded_at={1}), "ab", timeout=0.05)


def test_the_last_keys_dropped_release_is_seen():
    # The last key's press arrives first, its release a reading later,
    # dropped: the step waits for all its bytes, so it sees the drop
    # (counting presses alone would have confirmed it at once).
    with pytest.raises(StepNotConfirmed, match=r"^key 2 of 2 \('b'\): .* dropped 1 byte"):
        type_confirmed(FakeMachine(release_dropped={2}), "ab", timeout=0.05)


def test_keys_still_arriving_settle_before_the_first_reading():
    # The reproduced case: keys typed before (unconfirmed) still arriving
    # when the confirmed key goes. Counted from where they stopped, the
    # key confirms exactly.
    m = FakeMachine(arrivals={1: (2, 4, 0), 2: (1, 2, 0)})
    got = type_confirmed(m, "x")
    assert got == KeyCounts(8, 4, 0, 0, 0)
    assert m.sent == ["kbd"] * 4


def test_settling_waits_for_the_keys_press_has_queued(clock):
    m = FakeMachine()
    m._keys_sent_by = clock.now + 2.5
    type_confirmed(m, "x")
    assert clock.now >= m._keys_sent_by


def test_a_key_more_than_sent_fails():
    # A press arrives during key 1's step, after the readings before it
    # agreed: it would have confirmed the key on its own.
    m = FakeMachine(arrivals={3: (1, 2, 0)})
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "xy")
    assert str(e.value) == (
        "key 1 of 2 ('x'): more arrived than was sent: input sent before key 1 was still "
        "arriving (kbd counted 2 presses and 4 bytes since before key 1, expected 1 and 2)"
    )


def test_input_that_never_settles_fails(clock):
    # Every reading differs: a key repeating forever, say.
    m = FakeMachine(arrivals={n: (1, 1, 0) for n in range(1, 1000)})
    with pytest.raises(
        StepNotConfirmed,
        match=r"^key 1 of 1 \('x'\): input sent before it was still arriving after 0\.2s",
    ):
        type_confirmed(m, "x", timeout=0.2)
    assert m.key_presses == []


def test_a_key_step_polls_until_its_bytes_show():
    m = FakeMachine(delay=5)
    assert type_confirmed(m, "a") == KeyCounts(2, 1, 0, 0, 0)
    assert len(m.sent) >= 2 + 5


def test_a_key_steps_scaled_timeout_is_waited_out(clock):
    m = FakeMachine(lost_keys={1})
    m.timeout_scale = 4.0
    start = clock.now
    with pytest.raises(StepNotConfirmed, match=r"in 8s \(2s scaled by 4\)"):
        type_confirmed(m, "a", timeout=2)
    assert 8 <= clock.now - start < 9


def test_a_reading_that_fails_names_the_step_and_waits_only_the_time_left():
    m = FakeMachine(answers={"kbd": "kbd: not set up\n"})
    with pytest.raises(
        StepNotConfirmed, match=r"^key 1 of 1 \('a'\): reading kbd failed: kbd: no keyboard"
    ):
        type_confirmed(m, "a", timeout=3)
    # The serial shell's prompt was waited for at most the step's time.
    assert m.prompt_timeouts and all(gui.LEAST_READ <= t <= 3 for t in m.prompt_timeouts)


def test_a_warning_while_reading_kbd_fails_the_step_unless_allowed():
    with pytest.raises(StepNotConfirmed, match="reading kbd failed: .*a warning or error"):
        type_confirmed(FakeMachine(warn_with="kbd"), "a")
    assert type_confirmed(FakeMachine(warn_with="kbd"), "a", allow=r"WARN  mouse").presses == 1


def test_poll_returns_the_reading_that_was_done():
    readings = iter([1, 2, 3, 4])
    got, ok = gui.poll(FakeMachine(), lambda: next(readings), lambda n: n == 3, 1)
    assert (got, ok) == (3, True)
