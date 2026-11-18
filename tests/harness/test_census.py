"""The flake census (lumen_harness.census): what a CI log says failed, the
summary, and collecting first attempts, with the GitHub API faked."""

import subprocess
from datetime import UTC, datetime

import pytest
from lumen_harness import census
from lumen_harness.census import LOG_UNAVAILABLE, Run, failures_in_log, summary

LOG = """\
2026-11-18T04:00:00.0Z ##[group]Run make test-host
2026-11-18T04:00:01.0Z test tests::writing_never_allocates ... FAILED
2026-11-18T04:00:01.1Z test surface::tests::row_outside_panics - should panic ... FAILED
2026-11-18T04:00:01.2Z test crates/lumen-mm/src/window.rs - window::Window (line 34) ... FAILED
2026-11-18T04:00:02.0Z ##[group]Run make test-scenarios
2026-11-18T04:11:44.5Z E           not ok 25 - lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns
2026-11-18T04:11:44.6Z     not ok 25 - lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns
2026-11-18T04:11:44.7Z [gw3] [ 54%] FAILED tests/scenarios/test_time.py::progress_lines_are_not_the_record
2026-11-18T04:11:45.0Z \x1b[31mFAILED\x1b[0m tests/scenarios/test_acpi.py::test_cpu_count_matches_qemu[1-1 CPU] - boom
2026-11-18T04:11:45.1Z FAILED tests/scenarios/test_serial.py::test_the_safety_net - AssertionError
2026-11-18T04:11:45.2Z ERROR tests/scenarios/test_gui.py::test_a_teardown_failed - OSError
2026-11-18T04:11:45.3Z FAILED tests/scenarios/test_serial.py::test_the_safety_net - again
2026-11-18T04:12:00.0Z ##[group]Run rc=0
2026-11-18T04:12:00.1Z rc=0
2026-11-18T04:12:00.2Z LUMEN_ACCEL=tcg LUMEN_ARTIFACTS_DIR=build/test-artifacts-tcg \\
2026-11-18T04:12:30.0Z FAILED tests/scenarios/test_time.py::only_under_tcg - the re-run's failure
2026-11-18T04:12:30.1Z ok 26 - lumen_kernel::sched::fine
"""


def test_a_log_s_failures_are_read_once_each_in_order():
    assert failures_in_log(LOG) == [
        "cargo:tests::writing_never_allocates",
        "cargo:surface::tests::row_outside_panics",
        "cargo:crates/lumen-mm/src/window.rs - window::Window (line 34)",
        "lumen_kernel::sched::a_scope_joins_every_thread_before_it_returns",
        "tests/scenarios/test_acpi.py::test_cpu_count_matches_qemu[1-1 CPU]",
        "tests/scenarios/test_serial.py::test_the_safety_net",
        "tests/scenarios/test_gui.py::test_a_teardown_failed",
    ]


def test_early_ci_s_tcg_step_is_left_out_too():
    # Before PR #46 the TCG step's header named it, and with no ISO built it
    # failed every scenario.
    log = (
        "2026-11-12T04:00:00Z ##[group]Run make test-host\n"
        "2026-11-12T04:00:01Z test tests::torn ... FAILED\n"
        "2026-11-12T04:01:00Z ##[group]Run LUMEN_ACCEL=tcg uv run pytest tests/scenarios --last-failed\n"
        "2026-11-12T04:01:30Z FAILED tests/scenarios/test_boot.py::test_x - build/lumen-qemutest.iso "
        "not found; run `make iso` first\n"
    )
    assert failures_in_log(log) == ["cargo:tests::torn"]


def test_a_step_cut_short_is_read_from_its_progress_lines():
    # pytest killed before its summary: xdist's progress lines are all there is.
    cut = (
        "##[group]Run make test-scenarios\n"
        "[gw1] [ 10%] PASSED tests/scenarios/test_a.py::fine\n"
        "[gw2] [ 54%] FAILED tests/scenarios/test_b.py::hung_neighbour\n"
        "##[error]The job has exceeded the maximum execution time\n"
    )
    assert failures_in_log(cut) == ["tests/scenarios/test_b.py::hung_neighbour"]
    # With a summary, the summary is the record.
    summed = cut + "FAILED tests/scenarios/test_c.py::summed - x\n"
    assert failures_in_log(summed) == ["tests/scenarios/test_c.py::summed"]


def test_the_tcg_marker_is_ci_s_rerun_step_and_only_it():
    # ci.yml names it once, in the step that re-runs the failed scenarios,
    # near enough its script's start to be in the echo `failures_in_log` reads.
    import pathlib

    ci = pathlib.Path(__file__).parents[2].joinpath(".github/workflows/ci.yml").read_text()
    assert ci.count(census.TCG_RERUN) == 1, "the TCG marker must name one step"
    step = ci[ci.rindex("- name:", 0, ci.index(census.TCG_RERUN)) :]
    step = step[: step.find("\n      - ", 1) if "\n      - " in step[1:] else len(step)]
    assert "--last-failed" in step, step
    script = step[step.index("run: |") :]
    head = script[: script.index(census.TCG_RERUN)]
    # Each echoed line carries a timestamp of about 30 characters.
    assert len(head) + 30 * (head.count("\n") + 2) < census.SCRIPT_ECHO, head


def test_a_log_with_nothing_failed_has_no_failures():
    assert failures_in_log("ok 1 - a\nPASSED tests/x.py::y\ntest t ... ok\n1 passed\n") == []


def test_the_summary_counts_red_first_attempts_flakes_and_their_tests():
    runs = [
        Run("2026-11-18", False),
        Run("2026-11-18", True, ("k::heap", "tests/x.py::y"), rerun_passed=True),
        Run("2026-11-18", True),
        Run("2026-11-17", False),
        Run("2026-11-17", True, ("k::heap",)),
        Run("2026-11-17", False),
    ]
    text = summary(runs)
    # Days in order, whatever order the runs came in; shares round down.
    assert text.index("2026-11-17") < text.index("2026-11-18")
    assert "| 2026-11-17 | 3 | 1 (33%) | 0 |" in text
    assert "| 2026-11-18 | 3 | 2 (66%) | 1 |" in text
    assert "| **all** | 6 | 3 (50%) | 1 |" in text
    assert text.index("`k::heap` | 2 |") < text.index("`tests/x.py::y` | 1 |")
    assert "(red, no test named in its logs" in text and text.rstrip().endswith("| 1 |")


def test_the_unnamed_row_is_left_out_when_every_red_run_names_a_test():
    assert "no test named" not in summary([Run("2026-11-18", True, ("t",))])


def test_no_runs_is_no_division_by_zero():
    assert "| **all** | 0 | 0 (0%) | 0 |" in summary([])


REPO = "o/r"


@pytest.fixture(autouse=True)
def no_network(monkeypatch):
    """No test here reaches GitHub: gh fails loudly unless a fake stands in."""

    def refuse(*a, **k):
        raise AssertionError(f"a test ran a real command: {a}")

    monkeypatch.setattr(census.subprocess, "run", refuse)


def _run(id, conclusion, attempt=1, day="2026-11-17"):
    return {
        "id": id,
        "run_attempt": attempt,
        "conclusion": conclusion,
        "created_at": f"{day}T10:00:00Z",
    }


class FakeGitHub:
    """`_json` and `_log` over canned answers; records every call."""

    def __init__(self, pages, attempts=None, jobs=None, logs=None):
        self.pages = pages  # [[run, ...], ...], page 1 first
        self.attempts = attempts or {}  # run id -> attempt 1's run
        self.jobs = jobs or {}  # run id -> attempt 1's jobs
        self.logs = logs or {}  # job id -> log text, or None: unreadable
        self.calls: list[str] = []
        self.logs_read: list[int] = []

    def json(self, path):
        self.calls.append(path)
        if "/workflows/" in path:
            page = int(path.rsplit("page=", 1)[1])
            return {"workflow_runs": self.pages[page - 1] if page <= len(self.pages) else []}
        run_id = int(path.split("/runs/")[1].split("/")[0])
        if path.endswith("/attempts/1/jobs"):
            return {"jobs": self.jobs.get(run_id, [])}
        if path.endswith("/attempts/1"):
            return self.attempts[run_id]
        raise AssertionError(f"unexpected gh api {path}")

    def log(self, repo, job_id):
        assert repo == REPO
        self.logs_read.append(job_id)
        return self.logs.get(job_id, "")


@pytest.fixture
def github(monkeypatch):
    def make(**kw):
        f = FakeGitHub(**kw)
        monkeypatch.setattr(census, "_json", f.json)
        monkeypatch.setattr(census, "_log", f.log)
        return f

    return make


def test_a_rerun_counts_by_its_first_attempt(github):
    # Run 2's first attempt was red and its re-run passed: red, a flake, its
    # failures from attempt 1's jobs. Run 3 went red only on a re-run.
    f = github(
        pages=[[_run(1, "success"), _run(2, "success", attempt=2), _run(3, "failure", attempt=2)]],
        attempts={2: _run(2, "failure"), 3: _run(3, "success")},
        jobs={2: [{"id": 20, "conclusion": "failure"}]},
        logs={20: "FAILED tests/scenarios/a.py::t - boom\n"},
    )
    assert census.collect(REPO, 7) == [
        Run("2026-11-17", False),
        Run("2026-11-17", True, ("tests/scenarios/a.py::t",), rerun_passed=True),
        Run("2026-11-17", False),
    ]
    assert f"repos/{REPO}/actions/runs/2/attempts/1/jobs" in f.calls
    assert not any("runs/1/attempts" in c for c in f.calls), "a first attempt fetched again"


def test_only_red_conclusions_count_and_the_rest_are_left_out(github):
    github(
        pages=[
            [
                _run(1, "cancelled"),
                _run(2, "skipped"),
                _run(3, None),
                _run(4, "action_required"),
                _run(5, "neutral"),
                _run(6, "success", attempt=2),  # a re-run of a cancelled first attempt
                _run(7, "cancelled", attempt=2),  # a red first attempt, its re-run cancelled
                _run(8, "timed_out"),
                _run(9, "startup_failure"),
            ]
        ],
        attempts={6: _run(6, "cancelled"), 7: _run(7, "failure")},
        jobs={i: [{"id": i * 10, "conclusion": "failure"}] for i in (7, 8, 9)},
    )
    runs = census.collect(REPO, 7)
    assert [(r.red, r.rerun_passed) for r in runs] == [(True, False)] * 3


def test_failed_and_timed_out_jobs_logs_are_read_and_a_run_counts_a_test_once(github):
    f = github(
        pages=[[_run(1, "failure", day="2026-11-18")]],
        jobs={
            1: [
                {"id": 10, "conclusion": "success"},
                {"id": 11, "conclusion": "failure"},
                {"id": 12, "conclusion": "timed_out"},
            ]
        },
        logs={
            10: "FAILED tests/never.py::read\n",
            11: "FAILED tests/a.py::t\nE   not ok 3 - k::t\n",
            12: "FAILED tests/a.py::t\nFAILED tests/b.py::u\n",
        },
    )
    assert census.collect(REPO, 7) == [
        Run("2026-11-18", True, ("tests/a.py::t", "k::t", "tests/b.py::u"))
    ]
    assert f.logs_read == [11, 12]


def test_a_red_attempt_with_no_jobs_never_ran_and_is_left_out(github):
    github(pages=[[_run(1, "failure"), _run(2, "success")]], jobs={1: []})
    assert census.collect(REPO, 7) == [Run("2026-11-17", False)]


def test_a_cancelled_job_in_a_red_attempt_is_read(github):
    f = github(
        pages=[[_run(1, "failure")]],
        jobs={1: [{"id": 10, "conclusion": "cancelled"}, {"id": 11, "conclusion": "skipped"}]},
        logs={10: "[gw0] [ 50%] FAILED tests/scenarios/a.py::hang\n"},
    )
    assert census.collect(REPO, 7) == [Run("2026-11-17", True, ("tests/scenarios/a.py::hang",))]
    assert f.logs_read == [10]


def test_github_s_cap_on_a_listing_is_said(github, monkeypatch, capsys):
    f = github(pages=[[_run(1, "success")]])

    def capped(path):
        got = f.json(path)
        if "/workflows/" in path:
            got["total_count"] = 1500
        return got

    monkeypatch.setattr(census, "_json", capped)
    census.collect(REPO, 30)
    assert "GitHub listed 1 of 1500 runs" in capsys.readouterr().err


def test_a_log_that_cant_be_read_is_said_not_taken_for_no_failure(github):
    github(
        pages=[[_run(1, "failure")]],
        jobs={1: [{"id": 10, "conclusion": "failure"}]},
        logs={10: None},
    )
    assert census.collect(REPO, 7) == [Run("2026-11-17", True, (LOG_UNAVAILABLE,))]


def test_pages_are_read_until_a_short_one_and_a_run_shifted_onto_two_counts_once(github):
    first = [_run(i, "success") for i in range(100)]
    # A new run during paging pushes run 99 onto page 2 as well.
    f = github(
        pages=[first, [_run(99, "success"), _run(100, "failure")]],
        jobs={100: [{"id": 1000, "conclusion": "failure"}]},
    )
    assert len(census.collect(REPO, 7)) == 101
    listed = [c.rsplit("page=", 1)[1] for c in f.calls if "/workflows/" in c]
    assert listed == ["1", "2"]


def test_a_full_last_page_stops_at_the_empty_one(github):
    f = github(pages=[[_run(i, "success") for i in range(100)]])
    assert len(census.collect(REPO, 7)) == 100
    assert len([c for c in f.calls if "/workflows/" in c]) == 2


def test_the_window_is_the_last_days_to_the_second(github, monkeypatch):
    class Frozen(datetime):
        @classmethod
        def now(cls, tz=None):
            return datetime(2026, 11, 18, 3, 4, 5, tzinfo=UTC)

    monkeypatch.setattr(census, "datetime", Frozen)
    f = github(pages=[])
    assert census.collect(REPO, 3) == []
    assert f.calls == [
        f"repos/{REPO}/actions/workflows/ci.yml/runs?created=>=2026-11-15T03:04:05Z&per_page=100&page=1"
    ]


def _proc(rc, out="", err=""):
    return subprocess.CompletedProcess([], rc, out, err)


def test_gh_is_tried_again_and_its_last_error_raised(monkeypatch):
    answers = [_proc(1, err="502"), _proc(1, err="502"), _proc(0, '{"ok": 1}')]
    monkeypatch.setattr(census.subprocess, "run", lambda *a, **k: answers.pop(0))
    slept = []
    monkeypatch.setattr(census.time, "sleep", slept.append)
    assert census._json("p") == {"ok": 1}
    assert slept == [2, 4]
    monkeypatch.setattr(census.subprocess, "run", lambda *a, **k: _proc(1, err="gone"))
    with pytest.raises(RuntimeError, match="gh api p: gone"):
        census._gh("p")


def test_a_log_is_asked_for_with_allow_escape_sequences_first(monkeypatch):
    seen = []

    def run(args, **k):
        seen.append(args)
        return _proc(0, "LOG")

    monkeypatch.setattr(census.subprocess, "run", run)
    assert census._log(REPO, 7) == "LOG"
    assert seen == [["gh", "api", "--allow-escape-sequences", f"repos/{REPO}/actions/jobs/7/logs"]]


def test_a_log_is_tried_again_through_gh_s_retries(monkeypatch):
    answers = [_proc(1, err="502"), _proc(0, "LOG")]
    monkeypatch.setattr(census.subprocess, "run", lambda *a, **k: answers.pop(0))
    slept = []
    monkeypatch.setattr(census.time, "sleep", slept.append)
    assert census._log(REPO, 7) == "LOG"
    assert slept == [2]


def test_a_log_falls_back_to_a_gh_without_allow_escape_sequences(monkeypatch, capsys):
    monkeypatch.setattr(census.time, "sleep", lambda s: None)
    seen = []
    unknown = "unknown flag: --allow-escape-sequences"

    def run(args, **k):
        seen.append(args)
        return _proc(1, err=unknown) if "--allow-escape-sequences" in args else _proc(0, "LOG")

    monkeypatch.setattr(census.subprocess, "run", run)
    assert census._log(REPO, 7) == "LOG"
    assert seen[-1] == ["gh", "api", f"repos/{REPO}/actions/jobs/7/logs"]
    # Any other error is the real one, said as it was, with no fallback.
    monkeypatch.setattr(census.subprocess, "run", lambda *a, **k: _proc(1, err="HTTP 410: gone"))
    assert census._log(REPO, 7) is None
    err = capsys.readouterr().err
    assert "job 7's log couldn't be read" in err and "410" in err


def test_the_census_command_names_the_repository_and_its_days(monkeypatch, capsys):
    asked = []
    monkeypatch.setattr(census, "collect", lambda repo, days: asked.append((repo, days)) or [])
    monkeypatch.setattr(census, "_gh", lambda *a, **k: "found/repo\n")
    assert census.run_census(None, 3) == 0
    assert census.run_census("o/r", 30) == 0
    assert asked == [("found/repo", 3), ("o/r", 30)]
    out = capsys.readouterr().out
    assert "last 3 days (found/repo)" in out and "last 30 days (o/r)" in out
