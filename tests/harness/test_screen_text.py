"""lumen_harness.screen: parsing the serial shell's `term text`."""

import pytest
from lumen_harness.screen import SUMMARY, expect_window, parse_term_text, read_window
from qemu_fakes import FakeMachine


def parse(out: str):
    summary = SUMMARY.search(out)
    assert summary
    return parse_term_text(out[: summary.start()], summary)


def test_rows_and_cursor():
    out = "term text\n|lumen> echo hi\n|hi\n|lumen>\n|\nterm text: 4 rows, cursor (7, 2)\n"
    screen = parse(out)
    assert screen.rows == ["lumen> echo hi", "hi", "lumen>", ""]
    assert screen.cursor == (7, 2)
    assert screen.text == "lumen> echo hi\nhi\nlumen>\n"


def test_no_cursor_below_the_view():
    screen = parse("|41\n|42\nterm text: 2 rows, cursor below the view\n")
    assert (screen.rows, screen.cursor) == (["41", "42"], None)


def test_the_summary_only_counts_at_the_start_of_a_line():
    # A screen row showing a `term text` summary is a row, not the end.
    out = "|term text: 1 rows, cursor (0, 0)\nterm text: 1 rows, cursor (3, 0)\n"
    summary = SUMMARY.search(out)
    assert summary and summary.start() > 0
    assert parse_term_text(out[: summary.start()], summary).cursor == (3, 0)


def test_a_row_that_starts_with_a_bar_keeps_it():
    screen = parse("|| x\n| lead\nterm text: 2 rows, cursor (0, 0)\n")
    assert screen.rows == ["| x", " lead"]


def test_lines_not_rows_are_skipped_but_the_count_must_match():
    # A log line landing in the middle doesn't start with a bar.
    screen = parse("|a\n[    1.0] INFO  x: y\n|b\nterm text: 2 rows, cursor (1, 1)\n")
    assert screen.rows == ["a", "b"]
    with pytest.raises(AssertionError, match="1 rows read, 2 written"):
        parse("|a\nterm text: 2 rows, cursor (0, 0)\n")


class _Machine(FakeMachine):
    """A fake machine answering each line sent with the next of `answers`
    (what the guest prints for it), as serial output arriving after it."""

    timeout_scale = 1.0

    def __init__(self, *answers: str) -> None:
        super().__init__("")
        self.answers = list(answers)

    def send_line(self, line: str) -> None:
        super().send_line(line)
        if self.answers:
            self.output += self.answers.pop(0)


def test_read_window_asks_for_that_window_and_parses_its_view():
    # `term text <window>` (#155): the same format, sent with the id.
    m = _Machine("term text 3\n|lumen> ls\n|lumen>\nterm text: 2 rows, cursor (7, 1)\nlumen> ")
    screen = read_window(m, 3)
    assert m.sent == ["term text 3"]
    assert screen.rows == ["lumen> ls", "lumen>"] and screen.cursor == (7, 1)


def test_expect_window_reads_the_window_until_the_pattern_shows():
    m = _Machine(
        "term text 2\n|lumen>\nterm text: 1 rows, cursor (7, 0)\nlumen> ",
        "term text 2\n|hello\n|lumen>\nterm text: 2 rows, cursor (7, 1)\nlumen> ",
    )
    found = expect_window(m, 2, r"^hello$", timeout=5)
    assert found[0] == "hello" and m.sent == ["term text 2", "term text 2"]


def test_expect_window_names_the_window_when_it_gives_up():
    m = _Machine("term text 4\n|lumen>\nterm text: 1 rows, cursor (7, 0)\nlumen> ")
    with pytest.raises(AssertionError, match="not in terminal window 4"):
        expect_window(m, 4, r"^never$", timeout=0)
