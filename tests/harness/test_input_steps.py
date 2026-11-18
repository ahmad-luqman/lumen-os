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
        f"{frames} frames (0 ignored, 0 dropped, 0 overruns, newest event {frames}){tail}"
    )
    if xy is None:
        row = "  0000:00:03.0     -     -  -              -      -       -  -        -      -"
    else:
        # A cell with a space comes double-quoted, as records print it.
        cell = f'"{buttons}"' if " " in buttons else buttons
        row = (
            f"  0000:00:03.0  {xy[0]:>4}  {xy[1]:>4}  {cell:<7}        0      0       0  "
            "drawn    no     -"
        )
    return f"{header}\n{COLUMNS}\n{row}\n"


# `mouse` without a tablet, as kernel/src/shell/sysinfo.rs prints it.
NO_TABLET = (
    "mouse: no tablet (QEMU: -device virtio-tablet-pci)\n"
    f"{COLUMNS}\n"
    "  none             -     -  -              -      -       -  -        -      -\n"
)


def kbd_text(k: KeyCounts) -> str:
    """`kbd`'s output as kernel/src/shell/commands.rs prints it."""
    return (
        f"kbd: layout us, IRQ 1 (vector 0x51), {k.bytes} interrupts, {k.bytes} bytes, "
        f"{k.presses} presses, {k.held} held, {k.dropped} dropped, {k.undecoded} undecoded, "
        f"{k.line_errors} line errors, 0 skipped at setup, newest key event {k.presses}\n"
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
    (a lost prefix or release); `still_held` leaves that many of a press's
    keys held. From press `drops_at` on, each press's last two bytes are
    dropped (not queued). A press in `undecoded_at` makes a byte the
    decoder refuses, a reading after it shows (the decoder's count comes
    as a shell reads); one in `line_error_at` a byte with a line error.
    What a step sends shows `delay` readings later. `arrivals` adds counts
    (`presses`, `bytes`, `held`, `frames`) before the reading of that
    number (from 1, counting `mouse` and `kbd` together): input still
    arriving."""

    timeout_scale = 1.0

    def __init__(
        self,
        *,
        lost_frames: set[int] = frozenset(),
        no_effect: set[int] = frozenset(),
        lost_keys: set[int] = frozenset(),
        short_bytes: dict[int, int] | None = None,
        still_held: dict[int, int] | None = None,
        drops_at: int | None = None,
        undecoded_at: set[int] = frozenset(),
        line_error_at: set[int] = frozenset(),
        delay: int = 0,
        arrivals: dict[int, dict[str, int]] | None = None,
        answers: dict[str, str] | None = None,
        warn_with: str | None = None,
    ) -> None:
        self.output = PROMPT
        self._pos = 0
        self.sent_by = 0.0
        self.sent: list[str] = []
        self.prompt_timeouts: list[float] = []
        self.echo_timeouts: list[float] = []
        self.pointer_events: list[tuple] = []
        self.key_presses: list[list[str]] = []
        self.frames = 0
        self.xy: tuple[int, int] | None = None
        self.buttons: list[str] = []
        self.keys = dict.fromkeys(KeyCounts._fields, 0)
        self.held = ""
        self.failed: str | None = None
        self.lost_frames = lost_frames
        self.no_effect = no_effect
        self.lost_keys = lost_keys
        self.short_bytes = short_bytes or {}
        self.still_held = still_held or {}
        self.drops_at = drops_at
        self.undecoded_at = undecoded_at
        self.line_error_at = line_error_at
        self.delay = delay
        self.arrivals = arrivals or {}
        self.answers = answers or {}
        self.warn_with = warn_with
        self.reads = 0
        # (the reading it shows at, what to do)
        self._pending: list[tuple[int, object]] = []

    def keys_sent_by(self) -> float:
        return self.sent_by

    def _later(self, apply, extra: int = 0) -> None:
        self._pending.append((self.reads + self.delay + extra, apply))

    # -- serial ---------------------------------------------------------
    def send_line(self, line: str) -> None:
        self.sent.append(line)
        self.reads += 1
        for at, apply in [p for p in self._pending if p[0] <= self.reads]:
            self._pending.remove((at, apply))
            apply()
        for name, n in self.arrivals.get(self.reads, {}).items():
            if name == "frames":
                self.frames += n
            else:
                self.keys[name] += n
        if line in self.answers:
            out = self.answers[line]
        elif line == "mouse":
            order = [b for b in gui.HELD_BUTTONS if b in self.buttons]
            out = mouse_text(
                self.frames, self.xy, " ".join(order) or "none", held=self.held, failed=self.failed
            )
        elif line == "kbd":
            out = kbd_text(KeyCounts(**self.keys))
        else:
            out = f"unknown command: {line}\n"
        if self.warn_with == line:
            out += WARN + "\n"
        self.output += f"{line}\n{out}{PROMPT}"

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        (self.prompt_timeouts if pattern == PROMPT else self.echo_timeouts).append(timeout)
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
        k = self.keys

        def show() -> None:
            if n in self.lost_keys:
                return
            k["presses"] += len(qcodes)
            k["bytes"] += key_bytes(qcodes) - self.short_bytes.get(n, 0)
            k["held"] += self.still_held.get(n, 0)
            if self.drops_at is not None and n >= self.drops_at:
                k["bytes"] -= 2
                k["dropped"] += 2
            if n in self.line_error_at:
                k["line_errors"] += 1

        def refuse() -> None:
            k["undecoded"] += 1

        self._later(show)
        if n in self.undecoded_at:
            self._later(refuse, 1)


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
    k = KeyCounts(bytes=10, presses=5, held=1, dropped=2, undecoded=3, line_errors=4)
    assert parse_kbd(kbd_text(k)) == k
    with pytest.raises(AssertionError, match="no keyboard"):
        parse_kbd("kbd: no keyboard (no reply in time)\n")


def test_key_bytes_is_a_closed_table():
    assert key_bytes(["a"]) == 2
    assert key_bytes(["shift", "a"]) == 4
    assert key_bytes(["left"]) == 4
    assert key_bytes(["ctrl", "alt", "delete"]) == 8
    assert key_bytes(["print"]) == 8
    assert key_bytes(["pause"]) == 6
    assert key_bytes(["f12"]) == key_bytes(["esc"]) == key_bytes(["backspace"]) == 2
    # Every key qcodes_for types is in it.
    for c in map(chr, range(32, 127)):
        key_bytes(gui.qcodes_for(c))
    key_bytes(["ret"])
    # Keys QEMU sends otherwise (media keys four bytes, `menu` nothing) and
    # any other name are refused.
    for unknown in ["volumeup", "menu", "sysrq", "kp_4", "nosuchkey"]:
        with pytest.raises(ValueError, match=f"{unknown} not modelled"):
            key_bytes(["shift", unknown])
    with pytest.raises(ValueError, match="Print Screen or Pause with other keys"):
        key_bytes(["shift", "print"])
    with pytest.raises(ValueError, match="Print Screen or Pause with other keys"):
        key_bytes(["ctrl", "pause"])


def test_an_unmodelled_key_is_refused_before_anything_is_typed():
    m = FakeMachine()
    with pytest.raises(ValueError, match="volumeup not modelled"):
        confirmed_keys(m, "ab", ["volumeup"])
    assert (m.key_presses, m.sent) == ([], [])


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
    m = FakeMachine(arrivals={3: {"frames": 1}})
    with pytest.raises(
        StepNotConfirmed, match=r"^move to \(1, 1\): mouse counted 2 frames since the step, 1 sent"
    ):
        pointer_move(m, 1, 1)


def test_frames_still_arriving_settle_before_the_first_reading():
    # Frames sent earlier show over the first readings: the step counts
    # from where they stopped.
    m = FakeMachine(arrivals={1: {"frames": 1}, 2: {"frames": 1}})
    r = pointer_move(m, 3, 4)
    assert r.frames == 3
    # Three readings to agree (1, 2, 2), one after the move.
    assert m.sent == ["mouse"] * 4


@pytest.mark.parametrize("why", ["held", "failed"])
def test_a_held_or_failed_tablet_fails_the_step_at_once(clock, why):
    m = FakeMachine()
    pointer_move(m, 1, 1)
    if why == "held":
        m.held, tail = ", held", ", held"
    else:
        m.failed, tail = "FAILED", ", failed: FAILED"
    start = clock.now
    with pytest.raises(StepNotConfirmed) as e:
        pointer_move(m, 2, 2, timeout=10)
    assert str(e.value) == (
        "move to (2, 2): the guest can't read its frame (the tablet is held or failed): mouse "
        f"counted 1 frames (3 events, 0 ignored, 0 dropped, 0 overruns){tail}"
    )
    assert clock.now - start < 1, "it didn't wait out the timeout"
    # Nothing was sent to a tablet that can't read it.
    assert m.pointer_events == [("move", 1, 1)]


@pytest.mark.parametrize("why", ["held", "failed"])
def test_a_tablet_held_or_failed_during_the_step_fails_it_without_waiting(clock, why):
    m = FakeMachine(delay=3)
    pointer_move(m, 1, 1)
    start = clock.now
    original = m.move_pointer

    def move_then_stop(x: int, y: int) -> None:
        original(x, y)
        if why == "held":
            m.held = ", held"
        else:
            m.failed = "FAILED"
        m._pending.clear()

    m.move_pointer = move_then_stop
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
    # The frame shows only on the 6th reading after the two before it.
    m = FakeMachine(delay=6)
    r = pointer_move(m, 9, 9)
    assert (r.frames, r.position) == (1, (9, 9))
    assert m.sent == ["mouse"] * 8


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
    ) as e:
        pointer_down(FakeMachine(answers={"mouse": NO_TABLET}))
    assert e.value.last is None


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
    assert got == KeyCounts(bytes=10, presses=4, held=0, dropped=0, undecoded=0, line_errors=0)
    # Two agreeing readings before the first key, then two after each key
    # (at its target, and once more after QEMU sent everything).
    assert m.sent == ["kbd"] * 8
    assert type_confirmed(FakeMachine(), "") == KeyCounts(0, 0, 0, 0, 0, 0)


def test_a_key_that_never_arrives_is_named():
    m = FakeMachine(lost_keys={3})
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abcd", timeout=0.05)
    assert str(e.value) == (
        "key 3 of 4 ('c') never reached the guest in 0.05s: kbd counted 2 presses and 4 bytes "
        "since before key 1, expected 3 and 6; 0 held"
    )
    assert (e.value.step, e.value.index, e.value.total) == ("key 3 of 4 ('c')", 3, 4)
    assert e.value.last == KeyCounts(4, 2, 0, 0, 0, 0)
    # The fourth key was never typed.
    assert m.key_presses == [["a"], ["b"], ["c"]]


def test_keys_held_together_count_each_key():
    # shift+a is two presses: one missing (here the whole step) names it.
    m = FakeMachine(lost_keys={2})
    with pytest.raises(
        StepNotConfirmed, match=r"^key 2 of 2 \('A'\) never .* expected 3 and 6; 0 held$"
    ):
        type_confirmed(m, "aA", timeout=0.05)


def test_a_lost_prefix_or_release_fails_the_key():
    # Left's E0 prefix lost: its press still counts (as keypad 4), its
    # bytes fall one short.
    m = FakeMachine(short_bytes={2: 1})
    with pytest.raises(StepNotConfirmed) as e:
        confirmed_keys(m, "a", ["left"], "b", timeout=0.05)
    assert str(e.value) == (
        "key 2 of 3 (left): 3 of its 4 bytes arrived in 0.05s: a prefix or release was lost "
        "(kbd counted 2 presses and 5 bytes since before key 1, expected 2 and 6; 0 held)"
    )


def test_a_lost_release_covered_by_other_bytes_fails_as_held():
    # The reproduced case: the step's bytes all arrived, but one of them
    # was another key's release standing in for its own lost one; the key
    # is still held.
    m = FakeMachine(still_held={1: 1})
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "b", timeout=0.05)
    assert str(e.value) == (
        "key 1 of 1 ('b'): its bytes arrived but 1 key(s) are still held: a release was lost, "
        "and another key's bytes stood in for it (kbd counted 1 presses and 2 bytes since "
        "before key 1, expected 1 and 2; 1 held)"
    )


def test_settling_waits_for_held_keys_to_be_released():
    # A key held when the call starts (shift, held 400 ms), released at
    # the 4th reading: the step counts from after its release.
    m = FakeMachine(arrivals={1: {"held": 1}, 4: {"held": -1, "bytes": 1}})
    got = type_confirmed(m, "b")
    assert (got.presses, got.bytes, got.held) == (1, 3, 0)
    # held 1, 1, 1, then 0 (changed), 0 (agrees), then b: two readings.
    assert m.sent == ["kbd"] * 7


def test_a_dropped_undecoded_or_flagged_byte_fails_the_key():
    m = FakeMachine(drops_at=2)
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "abc", timeout=0.05)
    assert str(e.value).startswith(
        "key 2 of 3 ('b'): since before key 1 the keyboard's queue dropped 2 byte(s), the "
        "decoder refused 0 and 0 came with a line error: a key typed won't reach its shell whole"
    ), e.value
    with pytest.raises(StepNotConfirmed, match=r"^key 1 of 2 \('a'\): .* the decoder refused 1"):
        type_confirmed(FakeMachine(undecoded_at={1}), "ab", timeout=0.05)
    with pytest.raises(
        StepNotConfirmed, match=r"^key 2 of 2 \('b'\): .* refused 0 and 1 came with a line error"
    ):
        type_confirmed(FakeMachine(line_error_at={2}), "ab", timeout=0.05)


def test_keys_still_arriving_settle_before_the_first_reading():
    # The case review reproduced: keys typed before (unconfirmed) still
    # arriving when the confirmed key goes. Counted from where they
    # stopped, the key confirms exactly.
    m = FakeMachine(arrivals={1: {"presses": 2, "bytes": 4}, 2: {"presses": 1, "bytes": 2}})
    got = type_confirmed(m, "x")
    assert (got.bytes, got.presses) == (8, 4)
    assert m.sent == ["kbd"] * 5


def test_settling_compares_bytes_too():
    # A release still arriving changes only the bytes: not settled yet.
    m = FakeMachine(arrivals={2: {"bytes": 1}})
    got = type_confirmed(m, "x")
    assert got.bytes == 3
    # 0, 1 (bytes changed), 1 (agrees), then x's two readings.
    assert m.sent == ["kbd"] * 5


def test_settling_waits_for_the_keys_press_has_queued(clock):
    m = FakeMachine()
    m.sent_by = clock.now + 2.5
    type_confirmed(m, "x")
    assert clock.now >= m.sent_by


def test_a_short_timeout_after_a_burst_still_settles(clock):
    # The deadline starts after QEMU has sent the burst, not before.
    m = FakeMachine()
    m.sent_by = clock.now + 2.5
    assert type_confirmed(m, "x", timeout=0.05).presses == 1


@pytest.mark.parametrize(
    "arrivals, short",
    [
        ({3: {"bytes": 1}}, {}),
        ({3: {"presses": 1, "bytes": 1}}, {1: 1}),
    ],
    ids=["bytes", "presses"],
)
def test_a_key_more_than_sent_fails(arrivals, short):
    # More arrives during key 1's step, after the readings before it
    # agreed: too many bytes (presses exact), or too many presses (bytes
    # exact, the key's own one short).
    m = FakeMachine(arrivals=arrivals, short_bytes=short)
    with pytest.raises(StepNotConfirmed) as e:
        type_confirmed(m, "xy")
    assert re.match(
        r"key 1 of 2 \('x'\): more arrived than was sent: input sent before key 1 was still "
        r"arriving, or the step's keys send more than modelled \(kbd counted (\d) presses and "
        r"(\d) bytes since before key 1, expected 1 and 2; 0 held\)$",
        str(e.value),
    ), e.value
    # Exactly one count over.
    over = re.search(r"counted (\d) presses and (\d) bytes", str(e.value)).groups()
    assert over == (("1", "3") if not short else ("2", "2")), over


def test_more_bytes_after_the_target_fail():
    # A key that sends more than modelled: its first bytes reach the
    # target, the rest come after QEMU sent everything (the re-read).
    m = FakeMachine(arrivals={4: {"bytes": 2}})
    with pytest.raises(StepNotConfirmed, match=r"or the step's keys send more than modelled"):
        type_confirmed(m, "x")


def test_input_that_never_settles_fails(clock):
    # Every reading differs: a key repeating forever, say.
    m = FakeMachine(arrivals={n: {"presses": 1, "bytes": 1} for n in range(1, 1000)})
    with pytest.raises(
        StepNotConfirmed,
        match=r"^key 1 of 1 \('x'\): input sent before it was still arriving after 0\.2s: kbd "
        r"kept changing",
    ) as e:
        type_confirmed(m, "x", timeout=0.2)
    assert m.key_presses == []
    assert isinstance(e.value.last, KeyCounts)


def test_settling_says_when_only_one_reading_fit():
    m = FakeMachine()
    with pytest.raises(StepNotConfirmed, match=r"only one reading fit"):
        type_confirmed(m, "x", timeout=0.01)


def test_a_key_step_polls_until_its_bytes_show():
    m = FakeMachine(delay=5)
    assert type_confirmed(m, "a") == KeyCounts(2, 1, 0, 0, 0, 0)
    # Two readings to settle; the key shows on the 5th after; one more.
    assert m.sent == ["kbd"] * 8


def test_a_key_steps_scaled_timeout_is_waited_out(clock):
    m = FakeMachine(lost_keys={1})
    m.timeout_scale = 4.0
    start = clock.now
    with pytest.raises(StepNotConfirmed, match=r"in 8s \(2s scaled by 4\)"):
        type_confirmed(m, "a", timeout=2)
    assert 8 <= clock.now - start < 9


def test_readings_wait_only_the_time_the_step_has_left():
    # A key that never arrives, under LUMEN_TIMEOUT_SCALE=4: each reading
    # waits (unscaled, as `run` scales it) for what the step has left, from
    # its whole timeout down to the floor, echo and prompt alike.
    m = FakeMachine(lost_keys={1})
    m.timeout_scale = 4.0
    with pytest.raises(StepNotConfirmed):
        type_confirmed(m, "a", timeout=3)
    step = m.prompt_timeouts[2:]  # (after the two settling readings)
    assert step[0] == pytest.approx(3, abs=0.01)
    assert step == sorted(step, reverse=True)
    assert step[-1] == gui.LEAST_READ
    assert m.echo_timeouts[2:] == [min(10, t) for t in step]


def test_a_reading_that_fails_names_the_step():
    m = FakeMachine(answers={"kbd": "kbd: not set up\n"})
    with pytest.raises(
        StepNotConfirmed, match=r"^key 1 of 1 \('a'\): reading kbd failed: kbd: no keyboard"
    ):
        type_confirmed(m, "a", timeout=3)


def test_a_warning_while_reading_kbd_fails_the_step_unless_allowed():
    with pytest.raises(StepNotConfirmed, match="reading kbd failed: .*a warning or error"):
        type_confirmed(FakeMachine(warn_with="kbd"), "a")
    assert type_confirmed(FakeMachine(warn_with="kbd"), "a", allow=r"WARN  mouse").presses == 1


def test_poll_returns_the_reading_that_was_done():
    readings = iter([1, 2, 3, 4])
    got, ok = gui.poll(FakeMachine(), lambda: next(readings), lambda n: n == 3, 1)
    assert (got, ok) == (3, True)
