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
  (`kbd`'s `presses`, `lumen_input::Presses`), before any shell decodes
  them, and the bytes it queued (`bytes`) or had no room for (`dropped`).
  A key (or keys held together) is confirmed when `presses` rose by
  exactly the keys pressed and `bytes` by exactly the bytes they make
  (`key_bytes`: press and release, with any `0xE0` prefix), and nothing
  was dropped, undecoded or flagged with a line error. So a confirmed key
  reached IRQ 1 whole and was queued. A loss later, in the decoder, the
  virtual terminals' router or a shell, still shows only on the screen.

A failure names the step ("key 3 of 4 ('c') never reached the guest ...",
"drag: step 2 of 3, to (20, 20): the guest never read its frame ...") and
the counts it saw (`StepNotConfirmed`, with them as fields). Counts that
went past the step's target fail too: input sent earlier was still
arriving, and would have confirmed the step without it. So before taking
its first reading, each call waits until earlier keys have been sent
(`Machine.press`'s schedule) and two readings agree.

Reading the counters perturbs nothing they count: `mouse` and `kbd` run in
the serial shell, typed on serial, not on the keyboard or the tablet.
Their output goes through `lumen_harness.shell.run`, so a WARN or ERROR
logged meanwhile fails the step (`allow=` lets one through), as does a
reading that fails another way, each naming the step.

Pacing stays where `Machine.press` and `Machine._pointer` keep it: the
steps add no sleeps of their own, only the polls between two readings of a
counter (`POLL`), until the step's deadline (its timeout, scaled by
LUMEN_TIMEOUT_SCALE like `Machine.expect`'s).
"""

from __future__ import annotations

import re
import time
from collections.abc import Callable, Sequence
from typing import TYPE_CHECKING, Any, NamedTuple

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
    r"\((\d+) ignored, (\d+) dropped, (\d+) overruns\)(?:, (holding|held))?(?:, failed: (.+))?"
)
NO_TABLET = "mouse: no tablet"
# kernel/src/shell/commands.rs `kbd` (change them together).
KBD_COUNTS = re.compile(
    r"kbd: layout \S+, IRQ \d+ \(vector 0x[0-9a-f]+\), \d+ interrupts, (\d+) bytes, "
    r"(\d+) presses, (\d+) dropped, (\d+) undecoded, (\d+) line errors, \d+ skipped at setup"
)

# Keys QEMU's PS/2 keyboard sends with an `0xE0` prefix (set 2, translated
# to set 1: two bytes pressed, two released).
EXTENDED_KEYS = frozenset(
    {
        "ctrl_r",
        "alt_r",
        "meta_l",
        "meta_r",
        "menu",
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
    }
)


class StepNotConfirmed(AssertionError):
    """An input step whose effect the guest never showed in time, or whose
    counts went past what it sent. `step` names it, `index` and `total`
    place it among a call's steps (from 1; None for a single step), `last`
    is the last reading (a `PointerReport` or `KeyCounts`, or None if no
    reading was taken)."""

    def __init__(
        self,
        message: str,
        *,
        step: str,
        index: int | None = None,
        total: int | None = None,
        last: Any = None,
    ) -> None:
        super().__init__(message)
        self.step = step
        self.index = index
        self.total = total
        self.last = last


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
    dropped: int
    undecoded: int
    line_errors: int


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
    cells = re.split(r"\s{2,}", lines[2].strip())
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
    return KeyCounts(*map(int, found.groups()))


def read_pointer(m: Machine, allow: str | None = None, timeout: float = 10) -> PointerReport:
    """The serial shell's `mouse` (a WARN or ERROR matching `allow` may
    come with it; `timeout`: for its prompt, as `run`'s)."""
    return parse_mouse(run(m, "mouse", timeout, allow=allow))


def read_keys(m: Machine, allow: str | None = None, timeout: float = 10) -> KeyCounts:
    """The serial shell's `kbd` (see `read_pointer`)."""
    return parse_kbd(run(m, "kbd", timeout, allow=allow))


def key_bytes(qcodes: Sequence[str]) -> int:
    """The bytes QEMU's PS/2 keyboard sends, through the 8042's translation,
    for `qcodes` pressed together and released: two for a key, four for an
    `0xE0`-extended one (`EXTENDED_KEYS`), eight for Print Screen and six
    for Pause (only pressed) on their own. Print Screen or Pause with other
    keys (QEMU sends other sequences then) isn't modelled: ValueError. A
    key QEMU sends otherwise (a media key, say) would fail its step as more
    or fewer bytes than sent, not pass silently."""
    special = {"print": 8, "pause": 6}
    if any(q in special for q in qcodes):
        if len(qcodes) != 1:
            raise ValueError(f"{'+'.join(qcodes)}: Print Screen or Pause with other keys")
        return special[qcodes[0]]
    return sum(4 if q in EXTENDED_KEYS else 2 for q in qcodes)


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


class _Step:
    """One step's deadline and how its readings fail."""

    def __init__(
        self,
        m: Machine,
        step: str,
        timeout: float,
        index: int | None = None,
        total: int | None = None,
    ) -> None:
        self.m = m
        self.step = step
        self.timeout = timeout
        self.index = index
        self.total = total
        self.deadline = time.monotonic() + timeout * m.timeout_scale

    def fail(self, message: str, last: Any) -> StepNotConfirmed:
        return StepNotConfirmed(
            message, step=self.step, index=self.index, total=self.total, last=last
        )

    def read[T](self, what: str, reader: Callable[[Machine, str | None, float], T], allow) -> T:
        """A reading of `what` (`mouse`, `kbd`), waiting for the serial shell
        no longer than the step has left (but at least `LEAST_READ`); a
        failed reading fails the step, naming it."""
        left = max((self.deadline - time.monotonic()) / self.m.timeout_scale, LEAST_READ)
        try:
            return reader(self.m, allow, left)
        except AssertionError as e:  # ExpectTimeout and failed runs among them
            raise self.fail(f"{self.step}: reading {what} failed: {e}", None) from e


def _settled[T](
    m: Machine, step: _Step, what: str, reader, allow, same: Callable[[T, T], bool]
) -> T:
    """The first reading of a call: once input sent earlier stopped
    arriving. Waits until `Machine.press` has had every key it queued sent,
    then reads until two readings in a row agree (`same`)."""
    wait = getattr(m, "_keys_sent_by", 0.0) - time.monotonic()
    if wait > 0:
        time.sleep(wait)
    last = step.read(what, reader, allow)
    while time.monotonic() <= step.deadline:
        time.sleep(POLL)
        got = step.read(what, reader, allow)
        if same(last, got):
            return got
        last = got
    raise step.fail(
        f"{step.step}: input sent before it was still arriving after "
        f"{_said(m, step.timeout)}: {what} kept changing (last {last})",
        last,
    )


# -- pointer -----------------------------------------------------------------


def _first_pointer(m: Machine, step: str, timeout: float, allow: str | None) -> PointerReport:
    s = _Step(m, step, timeout)
    return _settled(m, s, "mouse", read_pointer, allow, lambda a, b: a.frames == b.frames)


def _pointer_step(
    m: Machine,
    before: PointerReport,
    step: str,
    send: Callable[[], None],
    effect: Callable[[PointerReport], bool],
    wanted: str,
    timeout: float,
    allow: str | None,
    index: int | None = None,
    total: int | None = None,
) -> PointerReport:
    """Send one frame (`send`), then wait until `mouse` counts exactly it
    and shows `effect` (described as `wanted`); the report that did."""
    s = _Step(m, step, timeout, index, total)
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

    got, _ = poll(
        m,
        lambda: s.read("mouse", read_pointer, allow),
        lambda r: (r.frames >= want and effect(r)) or r.frames > want or stuck(r),
        timeout,
    )
    if got.frames > want:
        raise s.fail(
            f"{step}: mouse counted {got.frames - before.frames} frames since the step, 1 sent: "
            f"input sent before it was still arriving ({got.counts()}; it shows {got.shows()})",
            got,
        )
    if got.frames == want and effect(got):
        return got
    when = "(the tablet is held or failed)" if stuck(got) else f"in {_said(m, timeout)}"
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


def _check_button(button: str) -> None:
    if button not in HELD_BUTTONS:
        raise ValueError(f"no pointer button to hold {button!r} (one of {', '.join(HELD_BUTTONS)})")


def _move(
    m: Machine,
    before: PointerReport,
    xy: tuple[int, int],
    step: str,
    timeout: float,
    allow: str | None,
    index: int | None = None,
    total: int | None = None,
) -> PointerReport:
    x, y = xy
    return _pointer_step(
        m,
        before,
        step,
        lambda: m.move_pointer(x, y),
        lambda r: r.position == (x, y),
        f"the pointer at {(x, y)}",
        timeout,
        allow,
        index,
        total,
    )


def _button(
    m: Machine,
    before: PointerReport,
    button: str,
    down: bool,
    step: str,
    timeout: float,
    allow: str | None,
    index: int | None = None,
    total: int | None = None,
) -> PointerReport:
    _check_button(button)
    return _pointer_step(
        m,
        before,
        step,
        (lambda: m.click(button, release=False)) if down else (lambda: m.release(button)),
        lambda r: (button in r.buttons) == down,
        f"{button} {'held' if down else 'released'}",
        timeout,
        allow,
        index,
        total,
    )


def pointer_move(
    m: Machine, x: int, y: int, *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Move the pointer to pixel (`x`, `y`) and wait until the guest read
    exactly that frame and its state has the pointer there; `mouse`'s
    report then."""
    step = f"move to {(x, y)}"
    return _move(m, _first_pointer(m, step, timeout, allow), (x, y), step, timeout, allow)


def pointer_down(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Press `button` (and keep it held) and wait until the guest read
    exactly that frame and its state has the button held; `mouse`'s report
    then."""
    _check_button(button)
    step = f"press {button}"
    return _button(m, _first_pointer(m, step, timeout, allow), button, True, step, timeout, allow)


def pointer_up(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Release `button` and wait until the guest read exactly that frame and
    its state has the button released; `mouse`'s report then."""
    _check_button(button)
    step = f"release {button}"
    return _button(m, _first_pointer(m, step, timeout, allow), button, False, step, timeout, allow)


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
    r = _first_pointer(m, f"drag: move to the start {start}", timeout, allow)
    r = _move(m, r, start, f"drag: move to the start {start}", timeout, allow, 1, total)
    r = _button(m, r, button, True, f"drag: press {button} at {start}", timeout, allow, 2, total)
    for i, xy in enumerate(path, 1):
        r = _move(m, r, xy, f"drag: step {i} of {steps}, to {xy}", timeout, allow, i + 2, total)
    return _button(
        m, r, button, False, f"drag: release {button} at {end}", timeout, allow, total, total
    )


# -- keys --------------------------------------------------------------------


def key_steps(*presses: str | Sequence[str]) -> list[tuple[str, list[str]]]:
    """What `confirmed_keys(m, *presses)` presses, one step at a time: each
    step's label and the QEMU qcodes held together."""
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
    `left`, `home`, `ctrl` and `a`...). After each step, wait until `kbd`
    counts exactly its keys pressed and its bytes (`key_bytes`), with
    nothing dropped, undecoded or flagged since before key 1; otherwise
    fail, naming the step ("key N of M"). This confirms each key reached
    IRQ 1 and was queued, not what a shell did with it (see the module
    doc). `kbd`'s counts after the last step."""
    steps = key_steps(*presses)
    sizes = [key_bytes(q) for _, q in steps]
    total = len(steps)
    label = f"key 1 of {total} ({steps[0][0]})" if steps else "no keys"
    first = got = _settled(
        m,
        _Step(m, label, timeout),
        "kbd",
        read_keys,
        allow,
        lambda a, b: (a.presses, a.bytes) == (b.presses, b.bytes),
    )
    want_presses, want_bytes = first.presses, first.bytes
    for n, ((label, qcodes), size) in enumerate(zip(steps, sizes, strict=True), 1):
        what = f"key {n} of {total} ({label})"
        s = _Step(m, what, timeout, n, total)
        m.press(qcodes)
        want_presses += len(qcodes)
        want_bytes += size

        def lost(k: KeyCounts) -> bool:
            return (k.dropped, k.undecoded, k.line_errors) != (
                first.dropped,
                first.undecoded,
                first.line_errors,
            )

        got, ok = poll(
            m,
            lambda s=s: s.read("kbd", read_keys, allow),
            lambda k, p=want_presses, b=want_bytes: (k.presses >= p and k.bytes >= b) or lost(k),
            timeout,
        )
        since = (
            f"kbd counted {got.presses - first.presses} presses and {got.bytes - first.bytes} "
            f"bytes since before key 1, expected {want_presses - first.presses} and "
            f"{want_bytes - first.bytes}"
        )
        if lost(got):
            raise s.fail(
                f"{what}: since before key 1 the keyboard's queue dropped "
                f"{got.dropped - first.dropped} byte(s), the decoder refused "
                f"{got.undecoded - first.undecoded} and {got.line_errors - first.line_errors} "
                f"came with a line error: a key typed won't reach its shell whole ({since})",
                got,
            )
        if got.presses > want_presses or got.bytes > want_bytes:
            raise s.fail(
                f"{what}: more arrived than was sent: input sent before key 1 was still "
                f"arriving ({since})",
                got,
            )
        if not ok and got.presses < want_presses:
            raise s.fail(f"{what} never reached the guest in {_said(m, timeout)}: {since}", got)
        if not ok:
            raise s.fail(
                f"{what}: {got.bytes - (want_bytes - size)} of its {size} bytes arrived in "
                f"{_said(m, timeout)}: a prefix or release was lost ({since})",
                got,
            )
    return got


def type_confirmed(
    m: Machine, text: str, *, timeout: float = 10, allow: str | None = None
) -> KeyCounts:
    """Type `text` as a US layout would, confirming each character's keys
    (`confirmed_keys`)."""
    return confirmed_keys(m, text, timeout=timeout, allow=allow)
