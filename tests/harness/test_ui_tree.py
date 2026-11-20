"""The automation tree's harness helpers (#156): `ui tree`'s records read
back (quoted cells too), a node found by role and name, `ui invoke`'s
outputs told apart (a row or a message, either late; a cancelled request
to retry), its retries, and `ui tree`'s errors failing."""

from __future__ import annotations

import pytest
from lumen_harness import gui

# Hand-written in the shape kernel/src/ui.rs prints (lumen-shell's table:
# two spaces in, columns two apart and padded); the widths here are
# illustrative, not the kernel's, and `Count:` sits one space from the
# next cell only to show the reader splits on any blanks.
TREE = """\
   id  window  depth  role      name      value  state    actions       x    y    w    h
    9       3      0  window    Counter   -      focused  focus,close  161  169  300  140
   12       3      1  group     Display   -      -        -            169  177   72   21
   13       3      2  label     Caption   Count: -        -            169  177   48   21
   14       3      2  label     Count     1      -        -            225  177    8   21
   17       3      2  button    Add       -      focused  press,focus  169  206   50   33
"""

CANCELLED = "ui invoke: cancelled: window 3 didn't take it in 1 s (try again)\n"

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
    assert gui.parse_invoke(CANCELLED) is None
    # Refused before it was queued (a rationed queue, or an app's window
    # holding one request, #207): it never ran, so it is retried too.
    for busy in [
        "ui invoke: window 3 busy (try again)",
        "ui invoke: a request is still queued or running in window 3",
    ]:
        assert gui.parse_invoke(busy + "\n") is None, busy
    assert gui.INVOKE_STILL_RUNNING.match("ui invoke: still running (window 3)")
    for message in [
        "ui invoke: window 3 closed",
        "ui invoke: ran; window 3 closed",
        "ui invoke: no node 17",
        "ui invoke: still running (window 3)",
    ]:
        got = gui.parse_invoke(message + "\n")
        assert got == gui.Invoked(None, message)
        assert got.still_running == ("still running" in message), message
    with pytest.raises(AssertionError, match="ran late but said nothing more"):
        gui.parse_invoke("ui invoke: ran late\n")


def test_a_late_answer_may_be_a_message():
    # Answered after the first deadline with a message, not a row: Quit
    # closing its window, or the owner's refusal; both kept as late.
    closed = gui.parse_invoke("ui invoke: ran late\nui invoke: ran; window 3 closed\n")
    assert closed == gui.Invoked(None, "ui invoke: ran; window 3 closed", late=True)
    refused = gui.parse_invoke("ui invoke: ran late\nui invoke: the node is disabled\n")
    assert refused == gui.Invoked(None, "ui invoke: the node is disabled", late=True)


def test_quoted_cells_are_one_value_each():
    # A name with a space, an empty text and a text `-` print quoted (as the
    # shell's words read them); an absent value stays a bare `-`.
    out = """\
  id  name         value  state
   1  "File name"  ""     -
   2  Add          "-"    focused
"""
    rows = gui.parse_records(out, "id")
    assert rows[0] == {"id": "1", "name": "File name", "value": "", "state": "-"}
    assert rows[1]["value"] == "-"
    # An apostrophe, a backslash and a leading `#` come quoted too (bare,
    # they would open a quote, vanish, or start a comment).
    tricky = """  id  name
   3  "it's"
   4  "a\\\\b"
   5  "#tag"
"""
    assert [r["name"] for r in gui.parse_records(tricky, "id")] == ["it's", "a\\b", "#tag"]
    # A row that doesn't split (an unclosed quote) fails, showing the output.
    with pytest.raises(AssertionError, match="doesn't split as words"):
        gui.parse_records("  id  name\n   6  it's\n", "id")


def test_an_error_line_is_not_a_table():
    with pytest.raises(AssertionError, match="not a table starting with 'id'"):
        gui.parse_records("ui tree: no window 99\n", "id")


class FakeShell:
    """`gui.run`'s replies, one per call, in order."""

    def __init__(self, replies: list[str]) -> None:
        self.replies = list(replies)
        self.lines: list[str] = []

    def __call__(self, m, line, *args, **kwargs) -> str:
        self.lines.append(line)
        return self.replies.pop(0)


def test_ui_tree_fails_on_its_errors_but_not_without_a_desktop(monkeypatch):
    monkeypatch.setattr(gui, "run", FakeShell(["ui tree: no desktop\n", "ui tree: no window 99\n"]))
    assert gui.ui_tree(None) == []
    with pytest.raises(AssertionError, match="no window 99"):
        gui.ui_tree(None, 99)


def test_invoke_retries_a_cancelled_request(monkeypatch):
    # Cancelled (it never ran), then answered: the row, after one retry.
    shell = FakeShell([CANCELLED, ROW])
    monkeypatch.setattr(gui, "run", shell)
    monkeypatch.setattr(gui.time, "sleep", lambda s: None)
    got = gui.invoke(None, 17, "press")
    assert got.row is not None and got.row["name"] == "Add" and got.retries == 1
    assert shell.lines == ["ui invoke 17 press", "ui invoke 17 press"]


def test_invoke_quotes_its_arguments(monkeypatch):
    # `set_value "File name"` is one argument to the kernel's shell.
    shell = FakeShell([ROW])
    monkeypatch.setattr(gui, "run", shell)
    gui.invoke(None, 5, "set_value", "File name")
    assert shell.lines == ["ui invoke 5 set_value 'File name'"]


def test_invoke_gives_up_after_its_tries(monkeypatch):
    monkeypatch.setattr(gui, "run", FakeShell([CANCELLED] * 3))
    monkeypatch.setattr(gui.time, "sleep", lambda s: None)
    with pytest.raises(AssertionError, match="cancelled or busy 3 times"):
        gui.invoke(None, 17, "press", tries=3)


def test_invoke_waits_out_a_held_slot_without_counting_it(monkeypatch):
    # An app window's slot held by an earlier request: wait for the desktop
    # to settle (that request taken), then try again, not counted; a rationed
    # queue's busy counts like a cancel.
    held = "ui invoke: a request is still queued or running in window 3\n"
    busy = "ui invoke: window 3 busy (try again)\n"
    shell = FakeShell([held, held, busy, ROW])
    settled = []
    monkeypatch.setattr(gui, "run", shell)
    monkeypatch.setattr(gui.time, "sleep", lambda s: None)
    monkeypatch.setattr(gui, "wait_settled", lambda m, allow=None: settled.append(1))
    got = gui.invoke(None, 17, "press", tries=2)
    assert got.row is not None and got.retries == 1
    assert len(settled) == 2 and len(shell.lines) == 4
