"""Pointer and key steps that wait for each to reach the guest (#149).

`Machine.move_pointer`, `click`, `press` and `type_text` hand events to
QEMU and return: whether the guest got them shows only later, on the
screen. When a key typed under load went missing (#128: `echo abcd` read
as `echo abc`), a test that typed a burst and then looked at the screen
could only say that the screen was wrong. The steps here confirm each
event against a counter the kernel keeps as the events arrive, read
through the serial shell, before the next step goes:

- **Pointer** (a virtio tablet, `Devices(tablet=True)`): `pointer_move`,
  `pointer_down`, `pointer_up` and `drag`. Each event QEMU sends is one
  frame, and `mouse`'s header counts the frames the kernel's `mouse`
  thread read. A step waits until that count has gone up by one *and*
  `mouse`'s row shows the step's effect (the position, or the button held
  or not), then returns the report. Otherwise it fails, naming the step
  and the counts it saw.
- **Keys** (the PS/2 keyboard): `confirmed_keys` and `type_confirmed`.
  `kbd`'s `presses` counts the keys pressed among the bytes the
  keyboard's interrupt read (`lumen_input::Presses`), before any shell
  decodes them, and its `dropped` the bytes the kernel's queue had no room
  for. After each key (or keys held together) this waits until `presses`
  has gone up by the keys pressed. Otherwise it fails with "key N of M
  ... never reached the guest". A byte dropped fails it too: a key then
  reached the guest but not its shell, and won't have its effect.

Reading the counters perturbs nothing they count: `mouse` and `kbd` run in
the serial shell, typed on serial, not on the keyboard or the tablet.
Their output goes through `lumen_harness.shell.run`, so a WARN or ERROR
logged meanwhile fails the step (`allow=` lets one through).

Pacing stays where `Machine.press` and `Machine._pointer` keep it: the
steps add no sleeps of their own, only the polls between two readings of a
counter (`POLL`), until the step's deadline (its timeout, scaled by
LUMEN_TIMEOUT_SCALE like `Machine.expect`'s).
"""

from __future__ import annotations

import re
import time
from collections.abc import Callable, Sequence
from typing import TYPE_CHECKING, NamedTuple

from lumen_harness.qmp import qcodes_for
from lumen_harness.shell import run

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

# The least time between two readings of a counter while a step waits.
POLL = 0.02

# The pointer buttons a tablet holds down (the others QEMU sends as
# buttons, the wheel's, turn the wheel instead).
HELD_BUTTONS = ("left", "middle", "right", "side", "extra")

# kernel/src/shell/sysinfo.rs `mouse_header` (change them together).
MOUSE_HEADER = re.compile(
    r'mouse: "[^"]*" on \S+, screen \d+x\d+, (\d+) events in (\d+) frames '
    r"\((\d+) ignored, (\d+) dropped, (\d+) overruns\)(, holding|, held)?(?:, failed: (.+))?"
)
# kernel/src/shell/commands.rs `kbd` (change them together).
KBD_COUNTS = re.compile(
    r"kbd: layout \S+, IRQ \d+ \(vector 0x[0-9a-f]+\), (\d+) interrupts, (\d+) bytes, "
    r"(\d+) presses, (\d+) dropped, "
)


class StepNotConfirmed(AssertionError):
    """An input step whose effect the guest never showed in time."""


class PointerReport(NamedTuple):
    """`mouse`: the header's counts, and the row's position and buttons
    (None before the tablet's first frame)."""

    events: int
    frames: int
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
            f"{self.frames} frames ({self.events} events, {self.dropped} dropped, "
            f"{self.overruns} overruns)"
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

    presses: int
    dropped: int


def parse_mouse(out: str) -> PointerReport:
    """`mouse`'s output (its header, the columns' line and one row)."""
    lines = out.splitlines()
    if len(lines) != 3:
        raise AssertionError(f"mouse: expected a header, the columns and a row, got:\n{out}")
    header = MOUSE_HEADER.fullmatch(lines[0])
    if not header:
        raise AssertionError(f"mouse: no tablet, or a header this doesn't know:\n{out}")
    events, frames, _ignored, dropped, overruns, held, failed = header.groups()
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
        int(dropped),
        int(overruns),
        (held or "").removeprefix(", "),
        failed,
        position,
        held_buttons,
    )


def parse_kbd(out: str) -> KeyCounts:
    """`kbd`'s output."""
    found = KBD_COUNTS.match(out)
    if not found:
        raise AssertionError(f"kbd: no keyboard, or a line this doesn't know:\n{out}")
    _irqs, _bytes, presses, dropped = found.groups()
    return KeyCounts(int(presses), int(dropped))


def read_pointer(m: Machine, allow: str | None = None) -> PointerReport:
    """The serial shell's `mouse` (a WARN or ERROR matching `allow` may
    come with it)."""
    return parse_mouse(run(m, "mouse", allow=allow))


def read_keys(m: Machine, allow: str | None = None) -> KeyCounts:
    """The serial shell's `kbd` (see `read_pointer` for `allow`)."""
    return parse_kbd(run(m, "kbd", allow=allow))


def _wait_until[T](
    m: Machine, read: Callable[[], T], done: Callable[[T], bool], timeout: float
) -> tuple[T, bool]:
    """Read until `done(got)`, for up to `timeout` seconds (scaled): the last
    reading and whether it was done."""
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


# -- pointer -----------------------------------------------------------------


def _pointer_step(
    m: Machine,
    before: PointerReport,
    step: str,
    send: Callable[[], None],
    effect: Callable[[PointerReport], bool],
    wanted: str,
    timeout: float,
    allow: str | None,
) -> PointerReport:
    """Send one frame (`send`), then wait until `mouse` counts it and shows
    `effect` (described as `wanted`); the report that did."""
    send()
    want = before.frames + 1
    got, ok = _wait_until(
        m,
        lambda: read_pointer(m, allow),
        lambda r: r.frames >= want and effect(r),
        timeout,
    )
    if ok:
        return got
    said = _said(m, timeout)
    if got.frames < want:
        raise StepNotConfirmed(
            f"{step}: the guest never read its frame in {said}: mouse counted "
            f"{got.counts()}, expected {want} frames ({before.frames} before the step); "
            f"it shows {got.shows()}"
        )
    raise StepNotConfirmed(
        f"{step}: the guest read {got.frames - before.frames} frame(s) but never showed "
        f"{wanted} in {said}: mouse shows {got.shows()}, {got.counts()}"
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
    )


def _button(
    m: Machine,
    before: PointerReport,
    button: str,
    down: bool,
    step: str,
    timeout: float,
    allow: str | None,
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
    )


def pointer_move(
    m: Machine, x: int, y: int, *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Move the pointer to pixel (`x`, `y`) and wait until the guest read the
    frame and shows the pointer there; `mouse`'s report then."""
    before = read_pointer(m, allow)
    return _move(m, before, (x, y), f"move to {(x, y)}", timeout, allow)


def pointer_down(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Press `button` (and keep it held) and wait until the guest read the
    frame and shows it held; `mouse`'s report then."""
    before = read_pointer(m, allow)
    return _button(m, before, button, True, f"press {button}", timeout, allow)


def pointer_up(
    m: Machine, button: str = "left", *, timeout: float = 10, allow: str | None = None
) -> PointerReport:
    """Release `button` and wait until the guest read the frame and shows it
    released; `mouse`'s report then."""
    before = read_pointer(m, allow)
    return _button(m, before, button, False, f"release {button}", timeout, allow)


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
    report = read_pointer(m, allow)
    report = _move(m, report, start, f"drag: move to the start {start}", timeout, allow)
    report = _button(m, report, button, True, f"drag: press {button} at {start}", timeout, allow)
    for i, xy in enumerate(path, 1):
        report = _move(m, report, xy, f"drag: step {i} of {steps}, to {xy}", timeout, allow)
    return _button(m, report, button, False, f"drag: release {button} at {end}", timeout, allow)


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
    counts its keys pressed (and no byte dropped); otherwise fail, naming
    the step ("key N of M"). `kbd`'s counts after the last step."""
    steps = key_steps(*presses)
    first = got = read_keys(m, allow)
    want = first.presses
    for n, (label, qcodes) in enumerate(steps, 1):
        what = f"key {n} of {len(steps)} ({label})"
        m.press(qcodes)
        want += len(qcodes)
        got, ok = _wait_until(
            m,
            lambda: read_keys(m, allow),
            lambda k, want=want: k.presses >= want or k.dropped > first.dropped,
            timeout,
        )
        if got.dropped > first.dropped:
            raise StepNotConfirmed(
                f"{what}: the keyboard's queue dropped {got.dropped - first.dropped} byte(s) "
                "since the first key (its shell fell behind): a key reached the guest but not "
                "its shell"
            )
        if not ok:
            raise StepNotConfirmed(
                f"{what} never reached the guest in {_said(m, timeout)}: kbd counted "
                f"{got.presses - first.presses} presses since the first key, expected "
                f"{want - first.presses}"
            )
    return got


def type_confirmed(
    m: Machine, text: str, *, timeout: float = 10, allow: str | None = None
) -> KeyCounts:
    """Type `text` as a US layout would, confirming each character's keys
    (`confirmed_keys`)."""
    return confirmed_keys(m, text, timeout=timeout, allow=allow)
