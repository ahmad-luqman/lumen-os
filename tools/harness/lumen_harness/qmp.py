"""Minimal QEMU Machine Protocol (QMP) client over a unix socket."""

from __future__ import annotations

import json
import socket
import time
from collections.abc import Sequence
from pathlib import Path
from typing import Any


class QmpError(RuntimeError):
    pass


class QmpTimeout(QmpError):
    """A read from QEMU timed out (the client is unusable after it)."""


# QEMU's key names (QKeyCode, qapi/ui.json) for unshifted characters on a US
# keyboard; `qcodes_for` adds shift for the rest.
_UNSHIFTED = {
    " ": "spc",
    "\n": "ret",
    "-": "minus",
    "=": "equal",
    "[": "bracket_left",
    "]": "bracket_right",
    "\\": "backslash",
    ";": "semicolon",
    "'": "apostrophe",
    "`": "grave_accent",
    ",": "comma",
    ".": "dot",
    "/": "slash",
}
_SHIFTED = dict(zip('!@#$%^&*()_+{}|:"~<>?', "1234567890-=[]\\;'`,./", strict=True))


def qcodes_for(ch: str) -> list[str]:
    """The keys (QEMU qcodes, held together) that type `ch` on a US layout."""
    if len(ch) != 1:
        raise ValueError(f"one character at a time, not {ch!r}")
    if "a" <= ch <= "z" or "0" <= ch <= "9":
        return [ch]
    if "A" <= ch <= "Z":
        return ["shift", ch.lower()]
    if ch in _UNSHIFTED:
        return [_UNSHIFTED[ch]]
    if ch in _SHIFTED:
        return ["shift", *qcodes_for(_SHIFTED[ch])]
    raise ValueError(f"no US key types {ch!r}")


def send_key_args(qcodes: Sequence[str], hold_ms: int) -> dict[str, Any]:
    """The arguments of QMP's `send-key`: press `qcodes` together, hold them
    `hold_ms`, release them."""
    if not qcodes:
        raise ValueError("send-key needs at least one key")
    return {"keys": [{"type": "qcode", "data": q} for q in qcodes], "hold-time": hold_ms}


# QEMU's absolute axes run from 0 to this (`INPUT_EVENT_ABS_MAX`), on QMP's
# `input-send-event` and on virtio's tablet alike.
ABS_MAX = 0x7FFF
# QMP's pointer buttons (`InputButton`, qapi/ui.json) the harness presses.
BUTTONS = ("left", "middle", "right", "side", "extra")


def abs_of(px: int, pixels: int) -> int:
    """Pixel `px` of a screen `pixels` wide as an absolute axis value
    (0-`ABS_MAX`), rounded to the nearest: the kernel scales it back to the
    same pixel (`lumen_input::AbsAxis::scale`)."""
    if not (isinstance(pixels, int) and pixels >= 2):
        raise ValueError(f"a screen {pixels!r} pixels wide: expected 2 or more")
    if not (isinstance(px, int) and 0 <= px < pixels):
        raise ValueError(f"pixel {px!r} is off a screen {pixels} pixels wide")
    last = pixels - 1
    return (px * ABS_MAX + last // 2) // last


def pointer_move_args(x: int, y: int, size: tuple[int, int]) -> dict[str, Any]:
    """The arguments of QMP's `input-send-event` moving an absolute pointer
    to pixel (`x`, `y`) of a screen `size` (width, height) pixels big."""
    width, height = size
    return {
        "events": [
            {"type": "abs", "data": {"axis": "x", "value": abs_of(x, width)}},
            {"type": "abs", "data": {"axis": "y", "value": abs_of(y, height)}},
        ]
    }


def button_args(button: str, down: bool) -> dict[str, Any]:
    """The arguments of QMP's `input-send-event` pressing or releasing a
    pointer button."""
    if button not in BUTTONS:
        raise ValueError(f"no pointer button {button!r} (one of {', '.join(BUTTONS)})")
    return {"events": [{"type": "btn", "data": {"down": down, "button": button}}]}


class QmpClient:
    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._file = sock.makefile("rwb")
        # Asynchronous events received and not yet taken by `wait_event`.
        self.events: list[dict[str, Any]] = []
        # Why the connection can't be used any more (a read timed out part
        # way, leaving the socket's file unusable), once it can't.
        self._broken: str | None = None

    @classmethod
    def connect(cls, path: Path, timeout: float = 10) -> QmpClient:
        deadline = time.monotonic() + timeout
        while True:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(timeout)
                s.connect(str(path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                s.close()
                if time.monotonic() > deadline:
                    raise QmpError(f"QMP socket {path} not available after {timeout}s") from None
                time.sleep(0.05)
        client = cls(s)
        greeting = client._recv()
        if "QMP" not in greeting:
            raise QmpError(f"unexpected QMP greeting: {greeting}")
        client.command("qmp_capabilities")
        return client

    def _check(self) -> None:
        if self._broken:
            raise QmpError(f"QMP connection unusable: {self._broken}")

    def _recv(self) -> dict[str, Any]:
        """The next message from QEMU (an event is also kept on `events`).
        A read that times out or fails leaves the client unusable: a
        timed-out read leaves the socket's file unusable, and what is read
        next could be half a message."""
        self._check()
        try:
            line = self._file.readline()
        except TimeoutError:
            self._broken = "a read timed out"
            raise QmpTimeout("QMP read timed out") from None
        except OSError as e:
            self._broken = f"a read failed ({e})"
            raise QmpError(f"QMP read failed: {e}") from e
        if not line:
            raise QmpError("QMP connection closed")
        try:
            msg = json.loads(line)
        except ValueError as e:
            self._broken = f"unreadable reply {line!r}"
            raise QmpError(f"unreadable QMP reply {line!r}") from e
        if "event" in msg:
            self.events.append(msg)
        return msg

    def command(self, name: str, **arguments: Any) -> Any:
        self._check()
        msg: dict[str, Any] = {"execute": name}
        if arguments:
            msg["arguments"] = arguments
        try:
            self._file.write(json.dumps(msg).encode() + b"\n")
            self._file.flush()
        except OSError as e:
            self._broken = f"a write failed ({e})"
            raise QmpError(f"{name}: QMP write failed: {e}") from e
        while True:
            reply = self._recv()
            if "return" in reply:
                return reply["return"]
            if "error" in reply:
                raise QmpError(f"{name}: {reply['error']}")

    def wait_event(self, name: str, timeout: float = 30) -> dict[str, Any]:
        """The first event called `name` (QEMU's, e.g. `SHUTDOWN` with its
        `data`), taking it and those before it off `events`; waits up to
        `timeout` seconds for it. Raises QmpError if the connection closes,
        a read fails, QEMU sends an error, or the time runs out first; after
        a timeout or a failed read the client is unusable."""
        deadline = time.monotonic() + timeout
        while True:
            for i, event in enumerate(self.events):
                if event["event"] == name:
                    del self.events[: i + 1]
                    return event
            remaining = deadline - time.monotonic()
            self._check()
            previous = self._sock.gettimeout()
            self._sock.settimeout(max(remaining, 0.001))
            try:
                reply = self._recv()
            except QmpTimeout:
                raise QmpError(f"no {name} event after {timeout:g}s (got {self.events})") from None
            except QmpError as e:
                raise QmpError(f"{e} while waiting for {name} (got {self.events})") from e
            finally:
                self._sock.settimeout(previous)
            if "error" in reply:
                raise QmpError(f"error reply while waiting for {name}: {reply['error']}")

    def send_key(self, qcodes: Sequence[str], hold_ms: int = 20) -> None:
        """Press `qcodes` together on the guest's keyboard, then release."""
        self.command("send-key", **send_key_args(qcodes, hold_ms))

    def send_input(self, args: dict[str, Any]) -> None:
        """Send input events (`pointer_move_args`, `button_args`); QEMU ends
        them with a sync, so the guest sees them as one frame."""
        self.command("input-send-event", **args)

    def close(self) -> None:
        try:
            self._file.close()
            self._sock.close()
        except OSError:
            pass
