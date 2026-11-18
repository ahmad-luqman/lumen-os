"""The flake census reads CI logs' failures and summarizes red first
attempts (lumen_harness.census)."""

from lumen_harness.census import Run, failures_in_log, summary

LOG = """\
2026-11-18T04:11:44.5981918Z E           not ok 25 - lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns
2026-11-18T04:11:44.6003643Z     not ok 25 - lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns
2026-11-18T04:11:44.7Z FAILED tests/scenarios/test_ktest.py::test_in_kernel_tests_pass[tsc-0.6%] - AssertionError: planned 304
2026-11-18T04:11:45.0Z FAILED tests/scenarios/test_serial.py::test_the_safety_net_reads_input_when_irq_4_never_comes
2026-11-18T04:11:45.1Z [gw3] [ 54%] FAILED tests/scenarios/test_time.py::test_x
2026-11-18T04:11:45.2Z ok 26 - lumen_kernel::sched::fine
"""


def test_a_log_s_failures_are_read_once_each():
    assert failures_in_log(LOG) == [
        "tests/scenarios/test_ktest.py::test_in_kernel_tests_pass[tsc-0.6%]",
        "tests/scenarios/test_serial.py::test_the_safety_net_reads_input_when_irq_4_never_comes",
        "lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns",
    ]


def test_colour_codes_are_ignored():
    assert failures_in_log("\x1b[31mFAILED\x1b[0m tests/x.py::y - boom\n") == ["tests/x.py::y"]


def test_a_log_with_nothing_failed_has_no_failures():
    assert failures_in_log("ok 1 - a\nPASSED tests/x.py::y\n1 passed\n") == []


def test_the_summary_counts_red_first_attempts_and_ranks_their_tests():
    runs = [
        Run("2026-11-17", "a", False),
        Run("2026-11-17", "b", True, ["lumen_kernel::mm::heap::fragmentation_stress"]),
        Run(
            "2026-11-18",
            "c",
            True,
            ["lumen_kernel::mm::heap::fragmentation_stress", "tests/x.py::y"],
        ),
        Run("2026-11-18", "d", True),
        Run("2026-11-18", "e", False),
    ]
    text = summary(runs)
    assert "| 2026-11-17 | 2 | 1 (50%) |" in text
    assert "| 2026-11-18 | 3 | 2 (66%) |" in text
    assert "| **all** | 5 | 3 (60%) |" in text
    assert text.index("fragmentation_stress` | 2 |") < text.index("`tests/x.py::y` | 1 |")
    assert "(red with no failing test in its logs: build, lint or audit) | 1 |" in text


def test_no_runs_is_no_division_by_zero():
    assert "| **all** | 0 | 0 (0%) |" in summary([])
