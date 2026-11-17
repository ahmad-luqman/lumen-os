"""The automation tree's harness helpers (#156): `ui tree`'s records read
back, a node found by role and name, and `ui invoke`'s outputs told apart
(a row, a late row, a cancelled request to retry, a message)."""

from __future__ import annotations

import pytest
from lumen_harness import gui

# As kernel/src/ui.rs prints them (lumen-shell's table: two spaces in,
# columns padded).
TREE = """\
   id  window  depth  role      name      value  state    actions       x    y    w    h
    9       3      0  window    Counter   -      focused  focus,close  161  169  300  140
   12       3      1  group     Display   -      -        -            169  177   72   21
   13       3      2  label     Caption   Count: -        -            169  177   48   21
   14       3      2  label     Count     1      -        -            225  177    8   21
   17       3      2  button    Add       -      focused  press,focus  169  206   50   33
"""

ROW = """\
   id  window  depth  role      name      value  state    actions       x    y    w    h
   17       3      2  button    Add       -      focused  press,focus  169  206   50   33
"""


def test_the_tree_parses_into_one_dict_per_node():
    rows = gui.parse_records(TREE)
    assert [r["name"] for r in rows] == ["Counter", "Display", "Caption", "Count", "Add"]
    assert rows[0] == {
        "id": "9",
        "window": "3",
        "depth": "0",
        "role": "window",
        "name": "Counter",
        "value": "-",
        "state": "focused",
        "actions": "focus,close",
        "x": "161",
        "y": "169",
        "w": "300",
        "h": "140",
    }
    assert rows[3]["value"] == "1"


def test_a_node_is_found_by_role_and_name_exactly_once():
    rows = gui.parse_records(TREE)
    assert gui.node(rows, "button", "Add")["id"] == "17"
    with pytest.raises(AssertionError, match="0 button nodes named 'Quit'"):
        gui.node(rows, "button", "Quit")
    with pytest.raises(AssertionError, match="2 label nodes"):
        gui.node(rows + [rows[3]], "label", "Count")


def test_invoke_outputs_are_told_apart():
    got = gui.parse_invoke(ROW)
    assert got is not None and got.row is not None and got.row["name"] == "Add" and not got.late
    late = gui.parse_invoke("ui invoke: ran late\n" + ROW)
    assert late is not None and late.late and late.row is not None and late.row["id"] == "17"
    assert (
        gui.parse_invoke("ui invoke: cancelled: window 3 didn't take it in 1 s (try again)\n")
        is None
    )
    for message in [
        "ui invoke: window 3 closed",
        "ui invoke: ran; window 3 closed",
        "ui invoke: window 3 busy",
        "ui invoke: no node 17",
        "ui invoke: still running (window 3)",
    ]:
        got = gui.parse_invoke(message + "\n")
        assert got == gui.Invoked(None, message)
    with pytest.raises(AssertionError, match="ran late but said nothing more"):
        gui.parse_invoke("ui invoke: ran late\n")


def test_a_late_answer_may_be_a_message():
    # Answered after the first deadline with a message, not a row: Quit
    # closing its window, or the owner's refusal; both kept as late.
    closed = gui.parse_invoke("ui invoke: ran late\nui invoke: ran; window 3 closed\n")
    assert closed == gui.Invoked(None, "ui invoke: ran; window 3 closed", late=True)
    refused = gui.parse_invoke("ui invoke: ran late\nui invoke: the node is disabled\n")
    assert refused == gui.Invoked(None, "ui invoke: the node is disabled", late=True)
