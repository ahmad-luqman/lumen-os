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


class QmpClient:
    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._file = sock.makefile("rwb")

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

    def _recv(self) -> dict[str, Any]:
        line = self._file.readline()
        if not line:
            raise QmpError("QMP connection closed")
        return json.loads(line)

    def command(self, name: str, **arguments: Any) -> Any:
        msg: dict[str, Any] = {"execute": name}
        if arguments:
            msg["arguments"] = arguments
        self._file.write(json.dumps(msg).encode() + b"\n")
        self._file.flush()
        while True:
            reply = self._recv()
            if "return" in reply:
                return reply["return"]
            if "error" in reply:
                raise QmpError(f"{name}: {reply['error']}")
            # Asynchronous events ({"event": ...}) are ignored.

    def send_key(self, qcodes: Sequence[str], hold_ms: int = 20) -> None:
        """Press `qcodes` together on the guest's keyboard, then release."""
        self.command("send-key", **send_key_args(qcodes, hold_ms))

    def close(self) -> None:
        try:
            self._file.close()
            self._sock.close()
        except OSError:
            pass
