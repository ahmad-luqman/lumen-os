"""The flake census: how often CI's first attempt goes red, and on what.

A run's first attempt is what a pull request meets; a red one that passes
when re-run is a flake or a real fault that later commits fixed. Each red
first attempt's failing scenarios and ktests are read from its jobs' logs,
so the census says which tests keep failing, not only how often
(`python -m lumen_harness census --days 7`, through `gh`; CI's weekly
`flake-census` workflow writes it to the job summary). The parsing and the
summary are self-tested in tests/harness/test_census.py.
"""

from __future__ import annotations

import json
import re
import subprocess
import time
from collections import Counter
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta

# pytest's short summary (`FAILED tests/scenarios/x.py::name - why`) and a
# ktest's TAP line (`not ok 25 - lumen_kernel::sched::name`), as CI prints
# them (each log line prefixed with a timestamp).
FAILED = re.compile(r"^(?:\S+ )?FAILED (tests/\S+?)(?: - |$)", re.MULTILINE)
NOT_OK = re.compile(r"^(?:\S+ )?(?:E\s+)?not ok \d+ - (\S+)$", re.MULTILINE)


# Colour codes in pytest's output.
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def failures_in_log(log: str) -> list[str]:
    """The scenarios and ktests a CI job's log says failed, each once, in
    the order they first appear."""
    log = ANSI.sub("", log)
    found = [m[1] for m in FAILED.finditer(log)] + [m[1] for m in NOT_OK.finditer(log)]
    return list(dict.fromkeys(found))


@dataclass
class Run:
    """One CI run's first attempt."""

    day: str
    branch: str
    red: bool
    failures: list[str] = field(default_factory=list)


def summary(runs: list[Run]) -> str:
    """A Markdown census of `runs`: red first attempts per day and in all,
    then the tests that failed them, most often first."""
    days: dict[str, list[Run]] = {}
    for r in sorted(runs, key=lambda r: r.day):
        days.setdefault(r.day, []).append(r)
    lines = ["| Day | Runs | Red on first attempt |", "|---|---|---|"]
    for day, rs in days.items():
        red = sum(r.red for r in rs)
        lines.append(f"| {day} | {len(rs)} | {red} ({100 * red // len(rs)}%) |")
    red = sum(r.red for r in runs)
    total = len(runs)
    lines.append(f"| **all** | {total} | {red} ({100 * red // total if total else 0}%) |")
    tests = Counter(t for r in runs if r.red for t in r.failures)
    unexplained = sum(r.red and not r.failures for r in runs)
    lines += ["", "| Failed on a first attempt | Runs |", "|---|---|"]
    lines += [f"| `{t}` | {n} |" for t, n in tests.most_common()]
    if unexplained:
        lines.append(
            f"| (red with no failing test in its logs: build, lint or audit) | {unexplained} |"
        )
    return "\n".join(lines) + "\n"


def _gh(path: str, tries: int = 3) -> object:
    """`gh api path` as JSON, tried again (GitHub's API fails now and then)."""
    for attempt in range(tries):
        got = subprocess.run(["gh", "api", path], check=False, capture_output=True, text=True)
        if got.returncode == 0:
            return json.loads(got.stdout)
        if attempt + 1 < tries:
            time.sleep(2 * (attempt + 1))
    raise RuntimeError(f"gh api {path}: {got.stderr.strip()}")


def _log(repo: str, job_id: int) -> str:
    """A job's log ("" if it can't be had). Newer gh refuses to print colour
    codes without --allow-escape-sequences, older gh doesn't know the flag."""
    path = f"repos/{repo}/actions/jobs/{job_id}/logs"
    for args in (["gh", "api", "--allow-escape-sequences", path], ["gh", "api", path]):
        got = subprocess.run(args, check=False, capture_output=True, text=True)
        if got.returncode == 0:
            return got.stdout
    return ""


def collect(repo: str, days: int, workflow: str = "ci.yml") -> list[Run]:
    """The first attempts of `workflow`'s runs over the last `days` days
    (cancelled ones left out: a newer push replaced them)."""
    since = (datetime.now(UTC) - timedelta(days=days)).strftime("%Y-%m-%d")
    runs: list[Run] = []
    page = 1
    while True:
        batch = _gh(
            f"repos/{repo}/actions/workflows/{workflow}/runs?created=>={since}&per_page=100&page={page}"
        )["workflow_runs"]
        if not batch:
            return runs
        for r in batch:
            first = (
                r
                if r["run_attempt"] == 1
                else _gh(f"repos/{repo}/actions/runs/{r['id']}/attempts/1")
            )
            if first["conclusion"] in (None, "cancelled", "skipped"):
                continue
            red = first["conclusion"] != "success"
            failures: list[str] = []
            if red:
                jobs = _gh(f"repos/{repo}/actions/runs/{r['id']}/attempts/1/jobs")["jobs"]
                for job in jobs:
                    if job["conclusion"] == "failure":
                        failures += failures_in_log(_log(repo, job["id"]))
            runs.append(
                Run(r["created_at"][:10], r["head_branch"], red, list(dict.fromkeys(failures)))
            )
        if len(batch) < 100:
            return runs
        page += 1
