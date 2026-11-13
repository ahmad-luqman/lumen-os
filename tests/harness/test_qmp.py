"""The QMP key helpers: characters to QEMU key names, and `send-key`'s
arguments."""

import pytest
from lumen_harness.qmp import qcodes_for, send_key_args


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
