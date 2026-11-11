"""Minimal QEMU Machine Protocol (QMP) client over a unix socket."""

from __future__ import annotations

import json
import socket
import time
from pathlib import Path
from typing import Any


class QmpError(RuntimeError):
    pass


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

    def close(self) -> None:
        try:
            self._file.close()
            self._sock.close()
        except OSError:
            pass
