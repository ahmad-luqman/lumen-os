"""The flake census: how often CI's first attempt goes red, and on what.

A run's first attempt is what a pull request meets. A red one is a flake
when re-running the same commit passes, or a fault in that push (fixed, if
at all, by a later one); the census counts both, and how many of them
passed on a re-run (the plainest flake signal). Each red first attempt's
failing scenarios, ktests and host tests are read from its failed jobs'
logs, so the census says which tests keep failing, not only how often
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

# What a CI log names as failed, each line prefixed with a timestamp:
# - pytest's short summary, `FAILED tests/x.py::name - why` (or `ERROR` for
#   a setup error; an id may hold spaces, `[1-1 CPU]`). xdist's progress
#   lines (`[gw3] [ 54%] FAILED ...`) aren't matched: the summary is the
#   record;
# - a ktest's TAP line, as pytest prints it (`E   not ok 25 - name`);
# - a host test, as cargo prints it (`test name ... FAILED`).
FAILED = re.compile(
    r"^(?:\S+ )?(?:"
    r"(?:FAILED|ERROR) (?P<scenario>tests/.+?)(?: - .*)?"
    r"|(?:E\s+)?not ok \d+ - (?P<ktest>\S+)"
    r"|test (?P<cargo>\S+) \.\.\. FAILED"
    r")$",
    re.MULTILINE,
)
# Colour codes in pytest's output.
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Where each step's part of a job's log starts.
STEP = re.compile(r"^(?:\S+ )?##\[group\]Run ", re.MULTILINE)
# CI's re-run of failed scenarios under TCG: it never decides a run, so what
# it prints isn't what failed the run.
TCG_RERUN = "LUMEN_ACCEL=tcg"
# Early CI ran every scenario after any failed step, with no ISO built
# (before PR #46): such a line names no failure of the test.
NO_ISO = "run `make iso` first"
# A red run whose failed jobs' logs couldn't be read.
LOG_UNAVAILABLE = "(a failed job's log couldn't be read)"
# First-attempt conclusions that are a red run; others are left out:
# cancelled (a newer push replaced it), skipped, neutral, action_required (it
# didn't really run), and None (not finished).
RED = ("failure", "timed_out", "startup_failure")


def failures_in_log(log: str) -> list[str]:
    """The tests a CI job's log says failed, each once, in the order the log
    first names them: scenarios as `tests/...`, ktests by name, host tests
    as `cargo:<name>`. The TCG re-run's part of the log is left out."""
    log = ANSI.sub("", log)
    starts = [m.start() for m in STEP.finditer(log)]
    parts = [log[a:b] for a, b in zip([0, *starts], [*starts, len(log)], strict=True)]
    found = []
    for part in parts:
        # The step's own script is echoed at its start.
        if TCG_RERUN in part[:2000]:
            continue
        for m in FAILED.finditer(part):
            if NO_ISO not in m[0]:
                found.append(m["scenario"] or m["ktest"] or f"cargo:{m['cargo']}")
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
        lines.append(
            f"| (red, no test named in its logs: setup, fmt, lint, build, audit, "
            f"feature gate) | {unnamed} |"
        )
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
    """A job's log, or None (said on stderr) if it can't be had. Newer gh
    refuses to print colour codes without --allow-escape-sequences; older gh
    doesn't know the flag."""
    path = f"repos/{repo}/actions/jobs/{job_id}/logs"
    for args in (("--allow-escape-sequences", path), (path,)):
        try:
            return _gh(*args)
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
    page = 1
    while True:
        batch = _json(
            f"repos/{repo}/actions/workflows/{workflow}/runs"
            f"?created=>={since}&per_page=100&page={page}"
        )["workflow_runs"]
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
                for job in jobs:
                    if job["conclusion"] in RED:
                        log = _log(repo, job["id"])
                        failures += [LOG_UNAVAILABLE] if log is None else failures_in_log(log)
                rerun_passed = r["run_attempt"] > 1 and r["conclusion"] == "success"
                runs[r["id"]] = Run(day, True, tuple(dict.fromkeys(failures)), rerun_passed)
            elif first["conclusion"] == "success":
                runs[r["id"]] = Run(day, False)
        if len(batch) < 100:
            return list(runs.values())
        page += 1


def run_census(repo: str | None, days: int) -> int:
    """`make census`: the census of the last `days` days, printed."""
    repo = repo or _gh("repos/{owner}/{repo}", "--jq", ".full_name").strip()
    print(f"## CI's first attempts over the last {days} days ({repo})\n")
    print(summary(collect(repo, days)))
    return 0
