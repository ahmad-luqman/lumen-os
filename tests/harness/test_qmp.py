"""The QMP client and input helpers: events kept and waited for, characters to
QEMU key names, `send-key`'s arguments, and `input-send-event`'s for an
absolute pointer (pixels to QEMU's 0-32767 axes) and its buttons."""

import json
import socket

import pytest
from lumen_harness.qmp import (
    ABS_MAX,
    QmpClient,
    QmpError,
    abs_of,
    button_args,
    pointer_move_args,
    qcodes_for,
    send_key_args,
)


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


def kernel_scale(value: int, pixels: int) -> int:
    """`lumen_input::AbsAxis::scale` on QEMU's 0-32767 axis: mirrors it (and
    its test `every_pixel_the_harness_aims_at_comes_back` mirrors `abs_of`),
    so change them together."""
    return (value * (pixels - 1) + ABS_MAX // 2) // ABS_MAX


def test_pixels_become_axis_values_the_kernel_scales_back():
    assert abs_of(0, 1280) == 0
    assert abs_of(1279, 1280) == ABS_MAX
    assert abs_of(400, 801) == 16384
    for pixels in (2, 3, 640, 800, 1280, 1920, 4096):
        for px in range(pixels):
            assert kernel_scale(abs_of(px, pixels), pixels) == px, (px, pixels)


def test_a_pixel_off_the_screen_is_refused():
    for px, pixels in [(-1, 800), (800, 800), (5, 1), (0, 0), (1.5, 800)]:
        with pytest.raises(ValueError):
            abs_of(px, pixels)


def test_a_move_sends_both_axes_and_a_button_its_state():
    assert pointer_move_args(1279, 0, (1280, 800)) == {
        "events": [
            {"type": "abs", "data": {"axis": "x", "value": ABS_MAX}},
            {"type": "abs", "data": {"axis": "y", "value": 0}},
        ]
    }
    assert button_args("right", True) == {
        "events": [{"type": "btn", "data": {"down": True, "button": "right"}}]
    }
    assert button_args("wheel-down", True)["events"][0]["data"]["button"] == "wheel-down"
    with pytest.raises(ValueError, match="no pointer button 'wheel'"):
        button_args("wheel", True)


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
    with pytest.raises(QmpError, match=r"closed while waiting for SHUTDOWN \(got \[.*RESET"):
        client.wait_event("SHUTDOWN", 5)
    # After a timeout the client refuses to be used (its socket's file is
    # unusable), rather than misreading what comes next, and sends nothing.
    client, qemu = _client()
    with pytest.raises(QmpError, match="no SHUTDOWN event after 0.2s"):
        client.wait_event("SHUTDOWN", 0.2)
    qemu.sendall(b'{"return": {}}\n')
    with pytest.raises(QmpError, match="unusable: a read timed out"):
        client.command("stop")
    assert _sent(qemu) == b""
    # An error reply, and a reply that isn't JSON, end the wait too (the
    # second leaves the client unusable).
    client, qemu = _client({"error": {"class": "GenericError", "desc": "no"}})
    with pytest.raises(QmpError, match="error reply while waiting for SHUTDOWN"):
        client.wait_event("SHUTDOWN", 5)
    client, qemu = _client()
    qemu.sendall(b"not json\n")
    with pytest.raises(QmpError, match="unreadable QMP reply .* while waiting for SHUTDOWN"):
        client.wait_event("SHUTDOWN", 5)
    with pytest.raises(QmpError, match="unusable: unreadable reply"):
        client.command("stop")
    assert _sent(qemu) == b""


def test_a_command_that_times_out_breaks_the_client():
    # A command whose reply never comes: the client is unusable afterwards
    # and sends no second command.
    client, qemu = _client()
    client._sock.settimeout(0.2)
    with pytest.raises(QmpError, match="timed out"):
        client.command("stop")
    assert json.loads(_sent(qemu)) == {"execute": "stop"}
    with pytest.raises(QmpError, match="unusable"):
        client.command("cont")
    assert _sent(qemu) == b""


def _sent(qemu: socket.socket) -> bytes:
    """What the client has sent QEMU so far (without waiting)."""
    qemu.setblocking(False)
    try:
        return qemu.recv(4096)
    except BlockingIOError:
        return b""
    finally:
        qemu.setblocking(True)
