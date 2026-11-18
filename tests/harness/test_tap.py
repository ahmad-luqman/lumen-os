"""The TAP parser must not report success for broken or partial runs."""

from lumen_harness import tap

GOOD = """INFO  boot: boot complete
TAP version 14
1..2
ok 1 - a::one
ok 2 - a::two
# ktest: 2 passed, 0 failed
"""


def test_all_passing():
    r = tap.parse(GOOD)
    assert r.complete and r.all_passed
    assert [c.name for c in r.cases] == ["a::one", "a::two"]


def test_failure_keeps_diagnostics_printed_before_the_result():
    text = (
        "TAP version 14\n1..2\nok 1 - a\n"
        "# assertion failed: x\n#    left: 2\nnot ok 2 - b\n# ktest: 1 passed, 1 failed\n"
    )
    r = tap.parse(text)
    assert r.complete and not r.all_passed
    assert r.cases[0].diagnostics == []
    assert r.failed[0].name == "b"
    assert r.failed[0].diagnostics == ["assertion failed: x", "left: 2"]


def test_truncated_run_is_not_success():
    # e.g. the kernel crashed during test 2
    r = tap.parse("TAP version 14\n1..3\nok 1 - a\n")
    assert not r.complete and not r.all_passed


def test_missing_plan_is_not_success():
    assert not tap.parse("TAP version 14\nok 1 - a\n").all_passed


def test_lines_before_tap_header_are_ignored():
    r = tap.parse("ok 99 - not a test\n" + GOOD)
    assert [c.number for c in r.cases] == [1, 2]


def test_comments_after_the_last_result_belong_to_no_test():
    # The ktest runner prints its summary and `lumen.ktest=` comment there.
    r = tap.parse(
        "TAP version 14\n1..1\nok 1 - a\n# ktest: 1 passed, 0 failed\n# ktest: only a: 1 of 3\n"
    )
    assert r.complete and r.cases[-1].diagnostics == []
