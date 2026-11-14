"""The QMP client and key helpers: events kept and waited for, characters to
QEMU key names, and `send-key`'s arguments."""

import json
import socket

import pytest
from lumen_harness.qmp import QmpClient, QmpError, qcodes_for, send_key_args


def test_letters_digits_and_space():
    assert qcodes_for("a") == ["a"]
    assert qcodes_for("z") == ["z"]
    assert qcodes_for("7") == ["7"]
    assert qcodes_for(" ") == ["spc"]
    assert qcodes_for("\n") == ["ret"]


def test_shifted_characters_hold_shift():
    assert qcodes_for("H") == ["shift", "h"]
    assert qcodes_for("!") == ["shift", "1"]
    assert qcodes_for("_") == ["shift", "minus"]
    assert qcodes_for("?") == ["shift", "slash"]
    assert qcodes_for('"') == ["shift", "apostrophe"]


def test_every_printable_ascii_character_has_keys():
    for code in range(0x20, 0x7F):
        keys = qcodes_for(chr(code))
        assert keys and all(isinstance(k, str) and k for k in keys), chr(code)


def test_what_no_key_types_is_refused():
    for bad in ["é", "\t", "ab", ""]:
        with pytest.raises(ValueError):
            qcodes_for(bad)


def test_send_key_presses_together_and_holds():
    assert send_key_args(["shift", "a"], 20) == {
        "keys": [{"type": "qcode", "data": "shift"}, {"type": "qcode", "data": "a"}],
        "hold-time": 20,
    }
    with pytest.raises(ValueError):
        send_key_args([], 20)


def _client(*lines: dict) -> tuple[QmpClient, socket.socket]:
    """A client whose QEMU end has already sent `lines`."""
    ours, theirs = socket.socketpair()
    for line in lines:
        theirs.sendall(json.dumps(line).encode() + b"\n")
    ours.settimeout(5)
    return QmpClient(ours), theirs


def test_events_are_kept_and_waited_for():
    # An event arriving while a command waits for its reply is kept;
    # wait_event takes the first of a name (and those before it), then
    # reads on for one not yet received.
    shutdown = {"event": "SHUTDOWN", "data": {"guest": True, "reason": "guest-shutdown"}}
    client, qemu = _client({"event": "RESET", "data": {}}, {"return": {}}, shutdown)
    assert client.command("stop") == {}
    assert [e["event"] for e in client.events] == ["RESET"]
    assert client.wait_event("SHUTDOWN", 5) == shutdown
    assert client.events == []
    qemu.close()


def test_waiting_for_an_event_ends():
    # QEMU closing the connection, or the time running out, without it.
    client, qemu = _client({"event": "RESET", "data": {}})
    qemu.close()
    with pytest.raises(QmpError, match="closed"):
        client.wait_event("SHUTDOWN", 5)
    client, qemu = _client()
    with pytest.raises(QmpError, match="no SHUTDOWN event after 0.2s"):
        client.wait_event("SHUTDOWN", 0.2)
    qemu.close()
