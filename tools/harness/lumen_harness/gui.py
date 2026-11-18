"""Pointer and key steps that wait for each to reach the guest (#149).

`Machine.move_pointer`, `click`, `press` and `type_text` hand events to
QEMU and return: whether the guest got them shows only later, on the
screen. When a key typed under load went missing (#128: `echo abcd` read
as `echo abc`), a test that typed a burst and then looked at the screen
could only say that the screen was wrong. The steps here check each event
against counters the kernel keeps as the events arrive, read through the
serial shell, before the next step goes:

- **Pointer** (a virtio tablet, `Devices(tablet=True)`): `pointer_move`,
  `pointer_down`, `pointer_up` and `drag`. Each QMP `input-send-event` is
  one frame, and `mouse`'s header counts the frames the kernel's `mouse`
  thread read. A step waits until that count is exactly one more than
  before it *and* the `mouse` thread's state (`mouse`'s row) shows the
  step's effect: the position, or the button held or not. That is the
  state a reader of the tablet sees, not yet anything drawn. A held or
  failed tablet fails the step at once.
- **Keys** (the PS/2 keyboard): `confirmed_keys` and `type_confirmed`. On
  IRQ 1 the kernel counts the keys pressed among the bytes it reads
  (`kbd`'s `presses`, `lumen_input::Presses`), the keys held now (`held`:
  pressed, not yet released), before any shell decodes them, and the
  bytes it queued (`bytes`) or had no room for (`dropped`). A key (or keys
  held together) is confirmed when, after QEMU sent everything
  (`Machine.keys_sent_by`), `presses` rose by exactly the keys pressed,
  `bytes` by exactly the bytes they make (`key_bytes`, a closed table:
  any other key is refused), no key is held, and nothing was dropped,
  undecoded or flagged with a line error. So a confirmed key reached IRQ 1
  whole, released, and was queued. A loss later, in the decoder, the
  virtual terminals' router or a shell, still shows only on the screen.
  (`undecoded` is the decoder's count, taken as a shell reads the keys:
  it can come a reading late, so it is checked as well as it can be.)

A failure names the step ("key 3 of 4 ('c') never reached the guest ...",
"drag: step 2 of 3, to (20, 20): the guest never read its frame ...") and
the counts it saw (`StepNotConfirmed`, with them as fields). Counts that
went past the step's target fail too: input sent earlier was still
arriving (or the step's keys send more than modelled), and would have
confirmed the step without it. So before taking its first reading, each
call waits until earlier keys have been sent (`Machine.keys_sent_by`) and
then until two readings agree (with no key held).

Reading the counters perturbs nothing they count: `mouse` and `kbd` run in
the serial shell, typed on serial, not on the keyboard or the tablet.
Their output goes through `lumen_harness.shell.run`, so a WARN or ERROR
logged meanwhile fails the step (`allow=` lets one through), as does a
reading that fails another way, each naming the step; a reading waits for
the serial shell no longer than the step has left.

Pacing stays where `Machine.press` and `Machine._pointer` keep it. The
steps wait out `Machine.press`'s schedule (the time QEMU takes to send the
keys it was handed) and poll between two readings of a counter (`POLL`),
until the step's deadline (its timeout, scaled by LUMEN_TIMEOUT_SCALE like
`Machine.expect`'s); they add no other sleeps.
"""

from __future__ import annotations

import re
import shlex
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import TYPE_CHECKING, NamedTuple

from lumen_harness.qmp import BUTTONS, qcodes_for
from lumen_harness.shell import run

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

# The least time between two readings of a counter while a step waits.
POLL = 0.02
# The least time a reading of a counter may wait for the serial shell
# (unscaled), however little of the step's time is left.
LEAST_READ = 1.0

# The pointer buttons a tablet holds down (QEMU's wheel "buttons" turn the
# wheel instead).
HELD_BUTTONS = tuple(b for b in BUTTONS if not b.startswith("wheel-"))

# kernel/src/shell/sysinfo.rs `mouse_header` (change them together).
MOUSE_HEADER = re.compile(
    r'mouse: "[^"]*" on \S+, screen \d+x\d+, (\d+) events in (\d+) frames '
    r"\((\d+) ignored, (\d+) dropped, (\d+) overruns, newest event \d+\)(?:, (holding|held))?"
    r"(?:, failed: (.+))?"
)
NO_TABLET = "mouse: no tablet"
# kernel/src/shell/commands.rs `kbd` (change them together).
KBD_COUNTS = re.compile(
    r"kbd: layout \S+, IRQ \d+ \(vector 0x[0-9a-f]+\), \d+ interrupts, (?P<bytes>\d+) bytes, "
    r"(?P<presses>\d+) presses, (?P<held>\d+) held, (?P<dropped>\d+) dropped, "
    r"(?P<undecoded>\d+) undecoded, (?P<line_errors>\d+) line errors, \d+ skipped at setup, "
    r"newest key event \d+"
)

_PLAIN_KEYS = (
    [chr(c) for c in range(ord("a"), ord("z") + 1)]
    + [str(d) for d in range(10)]
    + [
        "spc",
        "ret",
        "minus",
        "equal",
        "bracket_left",
        "bracket_right",
        "backslash",
        "semicolon",
        "apostrophe",
    ]
    + [
        "grave_accent",
        "comma",
        "dot",
        "slash",
        "esc",
        "tab",
        "backspace",
        "shift",
        "shift_r",
        "ctrl",
        "alt",
    ]
    + [f"f{n}" for n in range(1, 13)]
)
# Keys QEMU's PS/2 keyboard sends with an `0xE0` prefix (set 2, translated
# to set 1: two bytes pressed, two released).
EXTENDED_KEYS = frozenset(
    [
        "ctrl_r",
        "alt_r",
        "meta_l",
        "meta_r",
        "compose",
        "kp_divide",
        "kp_enter",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pgup",
        "pgdn",
        "insert",
        "delete",
    ]
)
# The bytes QEMU's PS/2 keyboard sends, through the 8042's translation,
# for each key pressed on its own and released: two for a key, four for an
# extended one, eight for Print Screen, six for Pause (pressed only, never
# released). A closed table, pinned against QEMU by
# test_input::test_every_modelled_key_is_confirmed: QEMU sends some keys
# otherwise (media keys four bytes, SysRq eight, some nothing at all).
KEY_BYTES: dict[str, int] = (
    dict.fromkeys(_PLAIN_KEYS, 2) | dict.fromkeys(EXTENDED_KEYS, 4) | {"print": 8, "pause": 6}
)


class PointerReport(NamedTuple):
    """`mouse`: the header's counts, and the row's position and buttons
    (None before the tablet's first frame)."""

    events: int
    frames: int
    ignored: int
    dropped: int
    overruns: int
    # "" while reading, "holding" or "held" after `mouse hold`.
    held: str
    # Why the tablet failed, or None.
    failed: str | None
    position: tuple[int, int] | None
    # The buttons held, in `mouse`'s order (empty: none).
    buttons: tuple[str, ...]

    def counts(self) -> str:
        """The counts, as a failure message says them."""
        said = (
            f"{self.frames} frames ({self.events} events, {self.ignored} ignored, "
            f"{self.dropped} dropped, {self.overruns} overruns)"
        )
        if self.held:
            said += f", {self.held}"
        if self.failed:
            said += f", failed: {self.failed}"
        return said

    def shows(self) -> str:
        """The row's position and buttons, as a failure message says them."""
        if self.position is None:
            return "no frame yet"
        buttons = " ".join(self.buttons) or "none"
        return f"at {self.position}, buttons {buttons}"


class KeyCounts(NamedTuple):
    """`kbd`'s counts this module uses."""

    bytes: int
    presses: int
    held: int
    dropped: int
    undecoded: int
    line_errors: int


class StepNotConfirmed(AssertionError):
    """An input step whose effect the guest never showed in time, or whose
    counts went past what it sent. `step` names it, `index` and `total`
    place it among a call's steps (from 1; None for a single step), `last`
    is the last reading (None if none was taken, or the reading failed)."""

    def __init__(
        self,
        message: str,
        *,
        step: str,
        index: int | None = None,
        total: int | None = None,
        last: PointerReport | KeyCounts | None = None,
    ) -> None:
        super().__init__(message)
        self.step = step
        self.index = index
        self.total = total
        self.last = last


def parse_mouse(out: str) -> PointerReport:
    """`mouse`'s output (its header, the columns' line and one row)."""
    lines = out.splitlines()
    if lines and lines[0].startswith(NO_TABLET):
        raise AssertionError(f"mouse: no tablet (boot with Devices(tablet=True)):\n{out}")
    if len(lines) != 3:
        raise AssertionError(f"mouse: expected a header, the columns and a row, got:\n{out}")
    header = MOUSE_HEADER.fullmatch(lines[0])
    if not header:
        raise AssertionError(f"mouse: a header this doesn't know:\n{out}")
    events, frames, ignored, dropped, overruns, held, failed = header.groups()
    # A cell with a space (buttons `left right`) comes quoted: split as words.
    cells = shlex.split(lines[2])
    # device, x, y, buttons, presses, wheel, hwheel, pointer, stale, failed
    if len(cells) != 10:
        raise AssertionError(f"mouse: a row of {len(cells)} cells, not 10:\n{out}")
    x, y, buttons = cells[1:4]
    position = None if x == "-" else (int(x), int(y))
    held_buttons = () if buttons in ("-", "none") else tuple(buttons.split())
    return PointerReport(
        int(events),
        int(frames),
        int(ignored),
        int(dropped),
        int(overruns),
        held or "",
        failed,
        position,
        held_buttons,
    )


def parse_kbd(out: str) -> KeyCounts:
    """`kbd`'s output."""
    found = KBD_COUNTS.match(out)
    if not found:
        raise AssertionError(f"kbd: no keyboard, or a line this doesn't know:\n{out}")
    return KeyCounts(**{k: int(v) for k, v in found.groupdict().items()})


def read_pointer(m: Machine, allow: str | None = None, timeout: float = 10) -> PointerReport:
    """The serial shell's `mouse` (a WARN or ERROR matching `allow` may
    come with it; `timeout`: for its echo and prompt, as `run`'s)."""
    return parse_mouse(run(m, "mouse", timeout, allow=allow))


def read_keys(m: Machine, allow: str | None = None, timeout: float = 10) -> KeyCounts:
    """The serial shell's `kbd` (see `read_pointer`)."""
    return parse_kbd(run(m, "kbd", timeout, allow=allow))


def key_bytes(qcodes: Sequence[str]) -> int:
    """The bytes QEMU's PS/2 keyboard sends for `qcodes` pressed together
    and released (`KEY_BYTES`). A key not in the table, or Print Screen or
    Pause with other keys (QEMU sends other sequences then), isn't
    modelled: ValueError."""
    unknown = [q for q in qcodes if q not in KEY_BYTES]
    if unknown:
        raise ValueError(f"{'+'.join(qcodes)}: {', '.join(unknown)} not modelled (KEY_BYTES)")
    if len(qcodes) > 1 and any(q in ("print", "pause") for q in qcodes):
        raise ValueError(f"{'+'.join(qcodes)}: Print Screen or Pause with other keys")
    return sum(KEY_BYTES[q] for q in qcodes)


def poll[T](
    m: Machine, read: Callable[[], T], done: Callable[[T], bool], timeout: float
) -> tuple[T, bool]:
    """Read until `done(got)`, for up to `timeout` seconds (scaled by
    LUMEN_TIMEOUT_SCALE), `POLL` apart: the last reading and whether it
    was done."""
    deadline = time.monotonic() + timeout * m.timeout_scale
    while True:
        got = read()
        if done(got):
            return got, True
        if time.monotonic() > deadline:
            return got, False
        time.sleep(POLL)


def _said(m: Machine, timeout: float) -> str:
    """A step's timeout, as its failure says it."""
    if m.timeout_scale == 1:
        return f"{timeout:g}s"
    return f"{timeout * m.timeout_scale:g}s ({timeout:g}s scaled by {m.timeout_scale:g})"


def _wait_for_keys_sent(m: Machine) -> None:
    """Wait until QEMU has sent every key `Machine.press` handed it."""
    wait = m.keys_sent_by() - time.monotonic()
    if wait > 0:
        time.sleep(wait)


class _Step:
    """One step: its name, place, deadline (from now) and settings, and how
    its readings fail."""

    def __init__(
        self,
        m: Machine,
        step: str,
        timeout: float,
        allow: str | None,
        index: int | None = None,
        total: int | None = None,
    ) -> None:
        self.m = m
        self.step = step
        self.timeout = timeout
        self.allow = allow
        self.index = index
        self.total = total
        self.deadline = time.monotonic() + timeout * m.timeout_scale

    def fail(self, message: str, last: PointerReport | KeyCounts | None) -> StepNotConfirmed:
        return StepNotConfirmed(
            message, step=self.step, index=self.index, total=self.total, last=last
        )

    def read[T](self, what: str, reader: Callable[[Machine, str | None, float], T]) -> T:
        """A reading of `what` (`mouse`, `kbd`), waiting for the serial shell
        no longer than the step has left (but at least `LEAST_READ`); a
        failed reading fails the step, naming it."""
        left = max((self.deadline - time.monotonic()) / self.m.timeout_scale, LEAST_READ)
        try:
            return reader(self.m, self.allow, left)
        except AssertionError as e:  # ExpectTimeout and failed runs among them
            raise self.fail(f"{self.step}: reading {what} failed: {e}", None) from e

    def poll[T](self, what: str, reader, done: Callable[[T], bool]) -> tuple[T, bool]:
        """`poll` with this step's readings, until its deadline."""
        left = max(self.deadline - time.monotonic(), 0) / self.m.timeout_scale
        return poll(self.m, lambda: self.read(what, reader), done, left)


def _settled[T](
    m: Machine,
    step: str,
    timeout: float,
    allow: str | None,
    what: str,
    reader,
    quiet: Callable[[T, T], bool],
) -> T:
    """The first reading of a call: once input sent earlier stopped
    arriving. Waits until QEMU has sent every key `Machine.press` handed
    it, then (its deadline starting there) reads until two readings in a
    row are `quiet`."""
    _wait_for_keys_sent(m)
    s = _Step(m, step, timeout, allow)
    last = s.read(what, reader)
    readings = 1
    while True:
        time.sleep(POLL)
        if time.monotonic() > s.deadline:
            break
        got = s.read(what, reader)
        readings += 1
        if quiet(last, got):
            return got
        last = got
    why = "only one reading fit" if readings == 1 else f"{what} kept changing"
    raise s.fail(
        f"{step}: input sent before it was still arriving after {_said(m, timeout)}: "
        f"{why} (last {last})",
        last,
    )


# -- pointer -----------------------------------------------------------------


def _first_pointer(m: Machine, step: str, timeout: float, allow: str | None) -> PointerReport:
    return _settled(
        m, step, timeout, allow, "mouse", read_pointer, lambda a, b: a.frames == b.frames
    )


def _pointer_step(
    s: _Step,
    before: PointerReport,
    send: Callable[[], None],
    effect: Callable[[PointerReport], bool],
    wanted: str,
) -> PointerReport:
    """Send one frame (`send`), then wait until `mouse` counts exactly it
    and shows `effect` (described as `wanted`); the report that did."""
    step = s.step
    if before.held or before.failed:
        raise s.fail(
            f"{step}: the guest can't read its frame (the tablet is held or failed): mouse "
            f"counted {before.counts()}",
            before,
        )
    send()
    want = before.frames + 1

    def stuck(r: PointerReport) -> bool:
        return bool(r.held or r.failed)

    got, _ = s.poll(
        "mouse",
        read_pointer,
        lambda r: (r.frames >= want and effect(r)) or r.frames > want or stuck(r),
    )
    if got.frames > want:
        raise s.fail(
            f"{step}: mouse counted {got.frames - before.frames} frames since the step, 1 sent: "
            f"input sent before it was still arriving ({got.counts()}; it shows {got.shows()})",
            got,
        )
    if got.frames == want and effect(got):
        return got
    when = "(the tablet is held or failed)" if stuck(got) else f"in {_said(s.m, s.timeout)}"
    if got.frames < want:
        raise s.fail(
            f"{step}: the guest never read its frame {when}: mouse counted "
            f"{got.counts()}, expected {want} frames ({before.frames} before the step); "
            f"it shows {got.shows()}",
            got,
        )
    raise s.fail(
        f"{step}: the guest read its frame but never showed {wanted} {when}: "
        f"mouse shows {got.shows()}, {got.counts()}",
        got,
    )


def _move(s: _Step, before: PointerReport, xy: tuple[int, int]) -> PointerReport:
    x, y = xy
    return _pointer_step(
        s,
        before,
        lambda: s.m.move_pointer(x, y),
        lambda r: r.position == (x, y),
        f"the pointer at {(x, y)}",
    )


def _button(s: _Step, before: PointerReport, button: str, down: bool) -> PointerReport:
    return _pointer_step(
        s,
        before,
        (lambda: s.m.click(button, release=False)) if down else (lambda: s.m.release(button)),
        lambda r: (button in r.buttons) == down,
        f"{button} {'held' if down else 'released'}",
    )


def _check_button(button: str) -> None:
    if button not in HELD_BUTTONS:
        raise ValueError(f"no pointer button to hold {button!r} (one of {', '.join(HELD_BUTTONS)})")


def pointer_move(
    m: Machine, x: int, y: int, *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Move the pointer to pixel (`x`, `y`) and wait until the guest read
    exactly that frame and its state has the pointer there; `mouse`'s
    report then."""
    step = f"move to {(x, y)}"
    before = _first_pointer(m, step, timeout, allow)
    return _move(_Step(m, step, timeout, allow), before, (x, y))


def _press_or_release(
    m: Machine, button: str, down: bool, timeout: float, allow: str | None
) -> PointerReport:
    _check_button(button)
    step = f"{'press' if down else 'release'} {button}"
    before = _first_pointer(m, step, timeout, allow)
    return _button(_Step(m, step, timeout, allow), before, button, down)


def pointer_down(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Press `button` (and keep it held) and wait until the guest read
    exactly that frame and its state has the button held; `mouse`'s report
    then."""
    return _press_or_release(m, button, True, timeout, allow)


def pointer_up(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Release `button` and wait until the guest read exactly that frame and
    its state has the button released; `mouse`'s report then."""
    return _press_or_release(m, button, False, timeout, allow)


def click(
    m: Machine, x: int, y: int, button: str = "left", timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Move the pointer to (`x`, `y`), then press and release `button`
    there, each step confirmed (`pointer_move`, `pointer_down`,
    `pointer_up`). (`Machine.click` sends the same frames unconfirmed.)"""
    pointer_move(m, x, y, timeout=timeout, allow=allow)
    pointer_down(m, button, timeout=timeout, allow=allow)
    return pointer_up(m, button, timeout=timeout, allow=allow)


def drag_path(start: tuple[int, int], end: tuple[int, int], steps: int) -> list[tuple[int, int]]:
    """The `steps` positions a drag from `start` moves through to `end`,
    evenly spaced (rounded to pixels), the last one `end`."""
    if steps < 1:
        raise ValueError(f"a drag needs at least one step, not {steps}")
    (x0, y0), (x1, y1) = start, end
    return [
        (x0 + round((x1 - x0) * i / steps), y0 + round((y1 - y0) * i / steps))
        for i in range(1, steps + 1)
    ]


def drag(
    m: Machine,
    start: tuple[int, int],
    end: tuple[int, int],
    steps: int,
    *,
    button: str = "left",
    timeout: float = 10,
    allow: str | None = None,
) -> PointerReport:
    """Drag with `button` from `start` to `end`: move to `start`, press
    `button`, move to `end` in `steps` even steps (`drag_path`), release
    it. Each of those `steps + 3` frames is confirmed before the next goes
    (see the module doc); `mouse`'s report after the release."""
    _check_button(button)
    path = drag_path(start, end, steps)
    total = steps + 3

    def step(name: str, index: int) -> _Step:
        return _Step(m, name, timeout, allow, index, total)

    first = f"drag: move to the start {start}"
    r = _first_pointer(m, first, timeout, allow)
    r = _move(step(first, 1), r, start)
    r = _button(step(f"drag: press {button} at {start}", 2), r, button, True)
    for i, xy in enumerate(path, 1):
        r = _move(step(f"drag: step {i} of {steps}, to {xy}", i + 2), r, xy)
    return _button(step(f"drag: release {button} at {end}", total), r, button, False)


# -- keys --------------------------------------------------------------------


def key_steps(*presses: str | Sequence[str]) -> list[tuple[str, list[str]]]:
    """What `confirmed_keys(m, *presses)` presses, one step at a time: each
    step's label and the QEMU qcodes held together. A string is typed as
    text (a character a step), a list is keys pressed together."""
    steps: list[tuple[str, list[str]]] = []
    for p in presses:
        if isinstance(p, str):
            steps.extend((repr(ch), qcodes_for(ch)) for ch in p)
        else:
            if not p:
                raise ValueError("keys pressed together: at least one")
            steps.append(("+".join(p), list(p)))
    return steps


def confirmed_keys(
    m: Machine, *presses: str | Sequence[str], timeout: float = 10, allow: str | None = None
) -> KeyCounts:
    """Type on the keyboard, a step at a time: a string is typed as text (a
    character a step), a list is keys pressed together (QEMU's names:
    `left`, `home`, `ctrl` and `a`...; each in `KEY_BYTES`, or ValueError
    before anything is typed). After each step, wait until `kbd` counts
    exactly its keys pressed and its bytes (`key_bytes`), no key held, and
    nothing dropped, undecoded or flagged since before key 1; otherwise
    fail, naming the step ("key N of M"). This confirms each key reached
    IRQ 1 whole, released, and was queued, not what a shell did with it
    (see the module doc). `kbd`'s counts after the last step."""
    steps = key_steps(*presses)
    sizes = [key_bytes(q) for _, q in steps]
    total = len(steps)
    label = f"key 1 of {total} ({steps[0][0]})" if steps else "no keys"
    first = got = _settled(
        m,
        label,
        timeout,
        allow,
        "kbd",
        read_keys,
        lambda a, b: (a.presses, a.bytes) == (b.presses, b.bytes) and b.held == 0,
    )

    def lost(k: KeyCounts) -> bool:
        return (k.dropped, k.undecoded, k.line_errors) != (
            first.dropped,
            first.undecoded,
            first.line_errors,
        )

    want_presses, want_bytes = first.presses, first.bytes
    for n, ((label, qcodes), size) in enumerate(zip(steps, sizes, strict=True), 1):
        what = f"key {n} of {total} ({label})"
        m.press(qcodes)
        s = _Step(m, what, timeout, allow, n, total)
        want_presses += len(qcodes)
        want_bytes += size

        def over(k: KeyCounts, p: int = want_presses, b: int = want_bytes) -> bool:
            return k.presses > p or k.bytes > b

        got, ok = s.poll(
            "kbd",
            read_keys,
            lambda k, p=want_presses, b=want_bytes: (
                (k.presses >= p and k.bytes >= b) or lost(k) or over(k)
            ),
        )
        if ok and not lost(got) and not over(got):
            # At the target: once QEMU sent all it was handed, read once
            # more, in case more arrives (keys that send more than
            # modelled).
            _wait_for_keys_sent(m)
            got = s.read("kbd", read_keys)
        since = (
            f"kbd counted {got.presses - first.presses} presses and {got.bytes - first.bytes} "
            f"bytes since before key 1, expected {want_presses - first.presses} and "
            f"{want_bytes - first.bytes}; {got.held} held"
        )
        if lost(got):
            raise s.fail(
                f"{what}: since before key 1 the keyboard's queue dropped "
                f"{got.dropped - first.dropped} byte(s), the decoder refused "
                f"{got.undecoded - first.undecoded} and {got.line_errors - first.line_errors} "
                f"came with a line error: a key typed won't reach its shell whole ({since})",
                got,
            )
        if over(got):
            raise s.fail(
                f"{what}: more arrived than was sent: input sent before key 1 was still "
                f"arriving, or the step's keys send more than modelled ({since})",
                got,
            )
        if got.presses < want_presses:
            raise s.fail(f"{what} never reached the guest in {_said(m, timeout)}: {since}", got)
        if got.bytes < want_bytes:
            raise s.fail(
                f"{what}: {got.bytes - (want_bytes - size)} of its {size} bytes arrived in "
                f"{_said(m, timeout)}: a prefix or release was lost ({since})",
                got,
            )
        if got.held:
            raise s.fail(
                f"{what}: its bytes arrived but {got.held} key(s) are still held: a release "
                f"was lost, and another key's bytes stood in for it ({since})",
                got,
            )
    return got


def type_confirmed(
    m: Machine, text: str, *, timeout: float = 10, allow: str | None = None
) -> KeyCounts:
    """Type `text` as a US layout would, confirming each character's keys
    (`confirmed_keys`)."""
    return confirmed_keys(m, text, timeout=timeout, allow=allow)


# -- the desktop (#153) --------------------------------------------------------

# kernel/src/desktop.rs `command` (change them together).
STARTED = "desktop: started (Alt+F1 shows the terminals)\n"


def parse_records(out: str, first: str | None = None) -> list[dict[str, str]]:
    """A records table as the kernel prints it (a header line of column
    names, then a row per record, each split as the shell's words: a cell
    that is empty, holds whitespace or is a text `-` comes double-quoted):
    one dict per row, column name to text. With `first`, the header must
    start with that column (so an error line isn't taken for a table)."""
    text = [line for line in out.splitlines() if line.strip()]
    if not text:
        raise AssertionError(f"no table in {out!r}")
    # The header first, by plain words: an error line (`ui tree: no window
    # 9`, or one with an apostrophe) is said as itself, not as a split error.
    if first is not None and text[0].split()[:1] != [first]:
        raise AssertionError(f"not a table starting with {first!r}:\n{out}")
    try:
        lines = [shlex.split(line) for line in text]
    except ValueError as e:
        raise AssertionError(f"a row that doesn't split as words ({e}):\n{out}") from e
    header, rows = lines[0], lines[1:]
    for row in rows:
        if len(row) != len(header):
            raise AssertionError(f"a row of {len(row)} values under {len(header)} columns:\n{out}")
    return [dict(zip(header, row, strict=True)) for row in rows]


def desktop_status(m: Machine, allow: str | None = None) -> dict[str, str]:
    """`desktop status`'s one record."""
    rows = parse_records(run(m, "desktop status", allow=allow), "state")
    if len(rows) != 1:
        raise AssertionError(f"desktop status gave {len(rows)} records")
    return rows[0]


def records_or_none(
    m: Machine, line: str, first: str, allow: str | None = None
) -> list[dict[str, str]]:
    """`line`'s records (a table starting with column `first`), or none if
    it says `<command>: no desktop`; any other line it says is an error."""
    out = run(m, line, allow=allow)
    if out.strip().endswith(": no desktop"):
        return []
    return parse_records(out, first)


def one(rows: list[dict[str, str]], what: str, **want: str) -> dict[str, str]:
    """The one row of `rows` whose columns have the values in `want`."""
    found = [r for r in rows if all(r[k] == v for k, v in want.items())]
    if len(found) != 1:
        raise AssertionError(f"{len(found)} {what} in {rows}")
    return found[0]


def windows(m: Machine, allow: str | None = None) -> list[dict[str, str]]:
    """`windows`' rows, front to back (none without a desktop: it says so
    instead of a table)."""
    return records_or_none(m, "windows", "id", allow)


def window(m: Machine, title: str, allow: str | None = None) -> dict[str, str]:
    """The `windows` row titled `title` (exactly one)."""
    return one(windows(m, allow), f"windows titled {title!r}", title=title)


def wait_for[T](
    m: Machine, what: str, read: Callable[[], T], done: Callable[[T], bool], timeout: float = 10
) -> T:
    """`poll` that fails, naming `what` and the last reading, if `done`
    never holds."""
    got, ok = poll(m, read, done, timeout)
    if not ok:
        raise AssertionError(f"{what} never happened in {_said(m, timeout)}: last {got}")
    return got


def wait_windows(m: Machine, count: int, timeout: float = 30) -> list[dict[str, str]]:
    """Wait until `windows` lists `count` windows; its rows then."""
    return wait_for(m, f"{count} windows", lambda: windows(m), lambda w: len(w) == count, timeout)


def wait_settled(m: Machine, timeout: float = 30, allow: str | None = None) -> dict[str, str]:
    """Wait until `desktop status` says `settled yes` (every byte typed
    decoded, both input rings read, no damage gathered or pass running, no
    commit under way, every window's queue empty with its owner waiting (not
    animating) and told of the last theme switch, none marked busy or
    woken by another thread, no `ui invoke` request outstanding: the full
    list is ARCHITECTURE section 11's); its
    record then. Poll for the effect a step should have first (a
    `windows` row, say): settling says nothing more is moving, not that
    the step happened."""
    return wait_for(
        m,
        "a settled desktop",
        lambda: desktop_status(m, allow),
        lambda s: s["settled"] == "yes",
        timeout,
    )


def start_desktop(m: Machine) -> None:
    """`desktop` on the serial shell, then wait for the two demo windows and
    for the desktop to settle."""
    out = run(m, "desktop", 30)
    if out != STARTED:
        raise AssertionError(f"desktop said {out!r}")
    wait_windows(m, 2)
    wait_settled(m)


def wait_desktop_state(m: Machine, state: str, timeout: float = 10) -> dict[str, str]:
    """Wait until `desktop status` says `state` (`shown`, `paused`); its
    record then."""
    return wait_for(
        m, f"the desktop {state}", lambda: desktop_status(m), lambda s: s["state"] == state, timeout
    )


def desktop_spin(m: Machine, allow: str | None = None) -> str:
    """`desktop spin`: the id of its window (a pure green bar moving every
    20 ms)."""
    out = run(m, "desktop spin", allow=allow)
    found = re.fullmatch(r"desktop spin: window (\d+)\n", out)
    if found is None:
        raise AssertionError(f"desktop spin said {out!r}")
    return found[1]


def desktop_check(m: Machine, tries: int = 20, allow: str | None = None) -> tuple[int, int]:
    """`desktop check` (retried while it says `busy`, up to `tries`
    times): the pixels that differ from a full repaint, and from the screen
    (outside the pointer's box)."""
    for _ in range(tries):
        out = run(m, "desktop check", 60, allow=allow)
        if out.strip() == "desktop check: busy":
            time.sleep(0.2)
            continue
        found = re.fullmatch(
            r"desktop check: (\d+) pixels differ from a full repaint, (\d+) from the screen\n", out
        )
        if found is None:
            raise AssertionError(f"desktop check said {out!r}")
        return int(found[1]), int(found[2])
    raise AssertionError(f"desktop check was busy {tries} times")


def settled_capture[T](
    m: Machine, take: Callable[[], T], tries: int = 5, allow: str | None = None
) -> T:
    """`take()` on a settled desktop: between two `desktop status` reads
    that are both settled with the same `frames` (nothing was presented
    meanwhile), taken again otherwise; fails if no try was bracketed."""
    for _ in range(tries):
        before = wait_settled(m, allow=allow)
        got = take()
        after = desktop_status(m, allow)
        if after["settled"] == "yes" and after["frames"] == before["frames"]:
            return got
    raise AssertionError(f"the desktop presented during each of {tries} captures")


def settled_screendump(m: Machine, name: str, tries: int = 5, allow: str | None = None) -> Path:
    """A screendump of a settled desktop (`settled_capture`); its path."""
    return settled_capture(m, lambda: m.screendump(name), tries, allow)


# -- the automation tree (#156) ------------------------------------------------

# kernel/src/ui.rs `invoke`'s lines (change them together).
INVOKE_CANCELLED = "ui invoke: cancelled"
INVOKE_LATE = "ui invoke: ran late"


def ui_tree(
    m: Machine, window: str | int | None = None, allow: str | None = None
) -> list[dict[str, str]]:
    """`ui tree [window]`'s rows: every node, depth first, each window's
    root (depth 0) first, front to back; none without a desktop. Any other
    error (`ui tree: no window 9`) fails."""
    line = "ui tree" if window is None else f"ui tree {window}"
    return records_or_none(m, line, "id", allow)


def node(rows: list[dict[str, str]], role: str, name: str) -> dict[str, str]:
    """The one row of `rows` with this role and name."""
    return one(rows, f"{role} nodes named {name!r}", role=role, name=name)


def desktop_counter(m: Machine) -> str:
    """`desktop counter`: the id of its window (the toolkit's demo)."""
    out = run(m, "desktop counter")
    found = re.fullmatch(r"desktop counter: window (\d+)\n", out)
    if found is None:
        raise AssertionError(f"desktop counter said {out!r}")
    return found[1]


def wait_ui(
    m: Machine,
    what: str,
    done: Callable[[list[dict[str, str]]], bool],
    window: str | int | None = None,
    timeout: float = 10,
) -> list[dict[str, str]]:
    """Wait until `ui tree [window]`'s rows satisfy `done`; the rows then."""
    return wait_for(m, what, lambda: ui_tree(m, window), done, timeout)


@dataclass(frozen=True)
class Invoked:
    """What `ui invoke` said: the node's row after the action, or a message
    (a window closed, busy, an error); `late` if either came after the first
    deadline (`ui invoke: ran late` above it), `retries` how many tries
    before it were cancelled."""

    row: dict[str, str] | None
    message: str | None
    late: bool = False
    retries: int = 0


def parse_invoke(out: str) -> Invoked | None:
    """`ui invoke`'s output read: `None` for a request cancelled at its
    deadline (to retry: it never ran), else what it said: a row or a
    message, either after `ui invoke: ran late` (`late`)."""
    lines = [line for line in out.splitlines() if line.strip()]
    if lines and lines[0].startswith(INVOKE_CANCELLED):
        return None
    late = bool(lines) and lines[0] == INVOKE_LATE
    body = lines[1:] if late else lines
    if body and body[0].split()[:1] == ["id"]:
        rows = parse_records("\n".join(body), "id")
        if len(rows) != 1:
            raise AssertionError(f"ui invoke gave {len(rows)} rows:\n{out}")
        return Invoked(rows[0], None, late)
    if late:
        # Answered late with a message: a refusal, or its window closed.
        if not body:
            raise AssertionError(f"ui invoke ran late but said nothing more:\n{out}")
        return Invoked(None, "\n".join(body).strip(), late)
    return Invoked(None, out.strip())


def invoke(
    m: Machine,
    node_id: str | int,
    action: str,
    *args: str,
    tries: int = 5,
    allow: str | None = None,
) -> Invoked:
    """`ui invoke <node> <action> [args]`: retried while cancelled at its
    deadline (a cancelled request never ran, so a retry can't act twice);
    "ran late" with its row or message counts as done (`Invoked.late`), and
    `Invoked.retries` says how many were cancelled first."""
    line = shlex.join(["ui", "invoke", str(node_id), action, *args])
    for retries in range(tries):
        got = parse_invoke(run(m, line, 10, allow=allow))
        if got is not None:
            return replace(got, retries=retries)
        time.sleep(0.2)
    raise AssertionError(f"{line} was cancelled {tries} times")


# -- the desktop shell (#157) --------------------------------------------------


def taskbar(m: Machine) -> list[dict[str, str]]:
    """The taskbar's nodes in `ui tree` (its root, a `group` named
    `Taskbar` at depth 0, first), screen coordinates."""
    rows = ui_tree(m)
    root = one(rows, "taskbar roots", role="group", name="Taskbar", depth="0")
    return [r for r in rows if r["window"] == root["window"]]


def bounds(row: dict[str, str]) -> tuple[int, int, int, int]:
    """A tree row's bounds: x, y, w, h."""
    return int(row["x"]), int(row["y"]), int(row["w"]), int(row["h"])


def centre(row: dict[str, str]) -> tuple[int, int]:
    """The middle of a tree row's bounds (where a click lands on it); a
    row with no bounds (a hidden node) fails."""
    x, y, w, h = bounds(row)
    assert w > 0 and h > 0, f"nothing to click: {row}"
    return x + w // 2, y + h // 2


def clock(m: Machine) -> dict[str, str]:
    """The taskbar clock's row in `ui tree` (its text in `value`)."""
    return node(taskbar(m), "label", "Clock")


def clock_mask(m: Machine) -> tuple[tuple[int, int, int, int], ...]:
    """The taskbar clock's bounds, for a golden's `masks` (its text
    changes with the time)."""
    return (bounds(clock(m)),)


def clock_text(m: Machine) -> str:
    """The taskbar clock's text (`HH:MM`)."""
    return clock(m)["value"]


def quiet_for(m: Machine, seconds: float, tries: int = 3) -> dict[str, str]:
    """A settled desktop that presents nothing for `seconds`: `settled`
    read before and after with the same `frames`. Tried again (up to
    `tries`) only when the taskbar clock's minute turned meanwhile (its
    redraw is a present) or the after read caught an owner between two
    waits (the clock polls the RTC every second); `desktop status`'s
    record after."""
    for _ in range(tries):
        clock = clock_text(m)
        before = wait_settled(m)
        time.sleep(seconds)
        after = desktop_status(m)
        if after["frames"] == before["frames"] and after["settled"] == "yes":
            return after
        if clock_text(m) == clock and after["frames"] != before["frames"]:
            raise AssertionError(f"presented while quiet: {before} then {after}")
    raise AssertionError(f"never quiet for {seconds} s in {tries} tries")
