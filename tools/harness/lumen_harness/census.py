"""The flake census: how often CI's first attempt goes red, and on what.

A run's first attempt is what a pull request meets. A red one is a flake
when re-running the same commit passes; one whose re-run failed too, or
that wasn't re-run, may be either a flake or a fault in that push. So the
census counts every red first attempt and, of those, how many passed on a
re-run: a floor on the flakes. Each red first attempt's failing pytest
tests, ktests and host tests are read from its failed jobs' logs, so the
census says which tests keep failing, not only how often
(`make census`, through `gh`; CI's weekly `flake-census` workflow writes it
to the job summary). Self-tested in tests/harness/test_census.py, with the
GitHub API faked.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import time
from collections import Counter
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Any

# What a CI log names as failed, each line maybe prefixed with a timestamp:
# - pytest's short summary, `FAILED tests/x.py::name - why` (or `ERROR` for
#   a setup error; an id may hold spaces, `[1-1 CPU]`; scenarios and harness
#   self-tests alike);
# - a ktest's TAP line, as pytest prints it (`E   not ok 25 - name`);
# - a host test, as cargo prints it (`test name ... FAILED`, with
#   ` - should panic` for one, or a doctest's `src/x.rs - f (line 3)`).
FAILED = re.compile(
    r"^(?:\S+ )?(?:"
    r"(?:FAILED|ERROR) (?P<scenario>tests/.+?)(?: - .*)?"
    r"|(?:E\s+)?not ok \d+ - (?P<ktest>\S+)"
    r"|test (?P<cargo>.+?)(?: - should panic)? \.\.\. FAILED"
    r")$",
    re.MULTILINE,
)
# xdist's progress lines (`[gw3] [ 54%] FAILED tests/...`): read only from a
# step with no short summary (pytest cut short: a timeout, a killed runner);
# else the summary is the record.
PROGRESS = re.compile(
    r"^(?:\S+ )?\[gw\d+\] \[ *\d+%\] (?:FAILED|ERROR) (?P<scenario>tests/\S+)", re.MULTILINE
)
# Colour codes in pytest's output.
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Where each step's part of a job's log starts.
STEP = re.compile(r"^(?:\S+ )?##\[group\]Run ", re.MULTILINE)
# CI's re-run of failed scenarios under TCG (`.github/workflows/ci.yml`'s
# `--last-failed` step, whose script starts with it; tied by a test): it
# never decides a run, so what it prints isn't what failed the run. (Early
# CI's version of it, before PR #46, ran every scenario with no ISO built.)
TCG_RERUN = "LUMEN_ACCEL=tcg"
# How far into a step's part its echoed script can name TCG_RERUN.
SCRIPT_ECHO = 2000
# A failed job whose log couldn't be read.
LOG_UNAVAILABLE = "(a failed job's log couldn't be read)"
# Conclusions that make a first attempt red; others are left out: cancelled
# (a newer push replaced it), skipped, neutral, action_required (it didn't
# really run), and None (not finished). A red attempt with no jobs at all
# never ran either, and is left out too. Within a red attempt, every job
# that didn't pass (or wasn't skipped) has its log read.
RED = ("failure", "timed_out", "startup_failure")
PASSED = ("success", "skipped")


def failures_in_log(log: str) -> list[str]:
    """The tests a CI job's log says failed, each once, in the order the log
    first names them: pytest tests as `tests/...`, ktests by name, host
    tests as `cargo:<name>`. The TCG re-run's part of the log is left out."""
    found = []
    for part in STEP.split(ANSI.sub("", log)):
        # The step's own script is echoed at its start.
        if TCG_RERUN in part[:SCRIPT_ECHO]:
            continue
        named = [
            m["scenario"] or m["ktest"] or f"cargo:{m['cargo']}" for m in FAILED.finditer(part)
        ]
        if not any(n.startswith("tests/") for n in named):
            named += [m["scenario"] for m in PROGRESS.finditer(part)]
        found += named
    return list(dict.fromkeys(found))


@dataclass(frozen=True)
class Run:
    """One CI run's first attempt."""

    # Its UTC date, `YYYY-MM-DD`.
    day: str
    red: bool
    # What its failed jobs' logs named (red runs only), each once.
    failures: tuple[str, ...] = ()
    # Whether a re-run of the same commit passed (red runs only).
    rerun_passed: bool = False


def _share(n: int, of: int) -> str:
    return f"{n} ({100 * n // of if of else 0}%)"


def summary(runs: list[Run]) -> str:
    """A Markdown census of `runs`: red first attempts per day and in all
    (and how many of those passed on a re-run), then the tests that failed
    them, most often first."""
    per_day = Counter(r.day for r in runs)
    red = Counter(r.day for r in runs if r.red)
    flaky = Counter(r.day for r in runs if r.red and r.rerun_passed)
    lines = [
        "| Day | Runs | Red on first attempt | Of those, passed on a re-run |",
        "|---|---|---|---|",
    ]
    lines += [
        f"| {d} | {per_day[d]} | {_share(red[d], per_day[d])} | {flaky[d]} |"
        for d in sorted(per_day)
    ]
    lines.append(f"| **all** | {len(runs)} | {_share(red.total(), len(runs))} | {flaky.total()} |")
    tests = Counter(t for r in runs if r.red for t in set(r.failures))
    unnamed = sum(r.red and not r.failures for r in runs)
    lines += ["", "| Failed on a first attempt | Runs |", "|---|---|"]
    lines += [f"| `{t}` | {n} |" for t, n in sorted(tests.items(), key=lambda kv: (-kv[1], kv[0]))]
    if unnamed:
        lines.append(f"| (red, no test named in its logs) | {unnamed} |")
    return "\n".join(lines) + "\n"


def _gh(*args: str, tries: int = 3) -> str:
    """`gh api args`'s output, tried again (GitHub's API fails now and then)."""
    for attempt in range(tries):
        got = subprocess.run(["gh", "api", *args], check=False, capture_output=True, text=True)
        if got.returncode == 0:
            return got.stdout
        if attempt + 1 < tries:
            time.sleep(2 * (attempt + 1))
    raise RuntimeError(f"gh api {' '.join(args)}: {got.stderr.strip()}")


def _json(path: str) -> Any:
    return json.loads(_gh(path))


def _log(repo: str, job_id: int) -> str | None:
    """A job's log, or None (said on stderr, with why) if it can't be had.
    Newer gh refuses to print colour codes without --allow-escape-sequences;
    an older one that doesn't know the flag is asked without it."""
    path = f"repos/{repo}/actions/jobs/{job_id}/logs"
    try:
        return _gh("--allow-escape-sequences", path)
    except RuntimeError as e:
        error = e
    if "unknown flag" in str(error):
        try:
            return _gh(path)
        except RuntimeError as e:
            error = e
    print(f"census: job {job_id}'s log couldn't be read: {error}", file=sys.stderr)
    return None


def collect(repo: str, days: int, workflow: str = "ci.yml") -> list[Run]:
    """The first attempts of `workflow`'s runs (pushes to main and pull
    requests) created in the last `days` days, those that didn't finish or
    really run left out (`RED`)."""
    since = (datetime.now(UTC) - timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")
    runs: dict[int, Run] = {}
    listed: set[int] = set()
    page = 1
    while True:
        response = _json(
            f"repos/{repo}/actions/workflows/{workflow}/runs"
            f"?created=>={since}&per_page=100&page={page}"
        )
        batch = response["workflow_runs"]
        listed |= {r["id"] for r in batch}
        for r in batch:
            # Runs created while paging shift the pages: one may come twice.
            if r["id"] in runs:
                continue
            first = (
                r
                if r["run_attempt"] == 1
                else _json(f"repos/{repo}/actions/runs/{r['id']}/attempts/1")
            )
            day = r["created_at"][:10]
            if first["conclusion"] in RED:
                failures: list[str] = []
                jobs = _json(f"repos/{repo}/actions/runs/{r['id']}/attempts/1/jobs")["jobs"]
                if not jobs:
                    continue
                for job in jobs:
                    if job["conclusion"] not in PASSED:
                        log = _log(repo, job["id"])
                        failures += [LOG_UNAVAILABLE] if log is None else failures_in_log(log)
                rerun_passed = r["run_attempt"] > 1 and r["conclusion"] == "success"
                runs[r["id"]] = Run(day, True, tuple(dict.fromkeys(failures)), rerun_passed)
            elif first["conclusion"] == "success":
                runs[r["id"]] = Run(day, False)
        if len(batch) < 100:
            # GitHub lists at most 1000 runs for a filter: say if it cut some.
            if len(listed) < response.get("total_count", 0):
                print(
                    f"census: GitHub listed {len(listed)} of {response['total_count']} runs; "
                    "the oldest are missing (ask for fewer days)",
                    file=sys.stderr,
                )
            return list(runs.values())
        page += 1


def run_census(repo: str | None, days: int) -> int:
    """`make census`: the census of the last `days` days, printed."""
    repo = repo or _gh("repos/{owner}/{repo}", "--jq", ".full_name").strip()
    print(f"## CI's first attempts over the last {days} days ({repo})\n")
    print(summary(collect(repo, days)))
    return 0
