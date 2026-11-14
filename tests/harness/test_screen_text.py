"""lumen_harness.screen: parsing the serial shell's `term text`."""

import pytest
from lumen_harness.screen import SUMMARY, parse_term_text


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
