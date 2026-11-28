"""Pinned gVisor build helpers and complete, resumable native gtest reports.

Case IDs are binary:Suite.Case/parameter. A 300 s watchdog bounds each case,
not the binary. Timeouts and flaky failures retry once; the retry decides.
Crashes are ERROR outcomes and never excused. Listing has its own 120 s budget.
Xfails accept an optional fail (default), skip, timeout or flaky kind before
the required reason and issue.
"""

from __future__ import annotations

import argparse
import ast
import enum
import json
import math
import os
import platform
import re
import select
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from types import MappingProxyType
from typing import NamedTuple

from . import BUILD_DIR
from .libctest import kill_group

BUILD = BUILD_DIR / "gvisor-tests"
CASE_TIMEOUT_S = 300
LISTING_TIMEOUT_S = 120
RECORD_COMMAND = (
    "PYTHONPATH=tools/harness python3 -m lumen_harness.gvisor "
    "record <downloaded gvisor-reference artifact dir> "
    "--output tests/conformance/gvisor-native.txt"
)
_BENCHMARK_SYMBOLS = frozenset({"gbenchmark", "gbenchmark_internal"})
_KNOWN_ATTRS = frozenset(
    {
        "name",
        "srcs",
        "hdrs",
        "deps",
        "testonly",
        "defines",
        "linkstatic",
        "malloc",
        "alwayslink",
        "tags",
        "data",
        "linkopts",
        "copts",
    }
)
_NAME = r"[A-Za-z0-9_/]+\.[A-Za-z0-9_/]+"
_BINARY = re.compile(r"[a-z][a-z0-9_]*_test")
_ID = re.compile(rf"[a-z][a-z0-9_]*_test:{_NAME}")
_START = re.compile(rf"^\[ RUN      \] ({_NAME})(?:\s+#.*)?$")
_END = re.compile(rf"^\[(       OK |  FAILED  |  SKIPPED )\] ({_NAME})(?:,.*?)? \(\d+ ms\)$")
_BEGIN = re.compile(r"^\[==========\] Running (\d+) tests? from \d+ test suites?\.$")
_DONE = re.compile(r"^\[==========\] (\d+) tests? from \d+ test suites? ran\. \(\d+ ms total\)$")
_ANSI = re.compile(r"\x1b\[[0-9;]*m")
_XFAIL = re.compile(
    rf"^({_ID.pattern})(?: (fail|skip|timeout|flaky))?\s+#\s*([^#\s][^#]*?)\s+#([1-9]\d*)$"
)


class Outcome(enum.Enum):
    PASS = "pass"
    FAIL = "fail"
    SKIP = "skip"
    TIMEOUT = "timeout"
    ERROR = "error"


OUTCOMES = {"OK": Outcome.PASS, "FAILED": Outcome.FAIL, "SKIPPED": Outcome.SKIP}
ALLOWED = {
    "fail": frozenset({Outcome.FAIL}),
    "skip": frozenset({Outcome.SKIP}),
    "timeout": frozenset({Outcome.TIMEOUT}),
    "flaky": frozenset({Outcome.PASS, Outcome.FAIL, Outcome.TIMEOUT}),
}


@dataclass(frozen=True)
class Run:
    results: Mapping[str, Outcome] = field(default_factory=dict)
    errors: tuple[str, ...] = ()
    listed: frozenset[str] = frozenset()
    # First attempts stay in results; a required retry is authoritative, even ERROR.
    retries: Mapping[str, Outcome] = field(default_factory=dict)

    def __post_init__(self):
        object.__setattr__(self, "results", MappingProxyType(dict(self.results)))
        object.__setattr__(self, "errors", tuple(self.errors))
        object.__setattr__(self, "listed", frozenset(self.listed))
        object.__setattr__(self, "retries", MappingProxyType(dict(self.retries)))

    @property
    def final_results(self) -> dict[str, Outcome]:
        return {**self.results, **self.retries}

    @property
    def not_run(self) -> frozenset[str]:
        return self.listed - self.results.keys()


def parse(
    text: str, *, exit_code: int, listed: Iterable[str] | None = None, timed_out: bool = False
) -> Run:
    """Retain verdicts and protocol errors, including failures outside cases."""
    results, errors = {}, []
    active = None
    announced = finished = None
    for raw in text.splitlines():
        line = _ANSI.sub("", raw).rstrip()
        if m := _BEGIN.fullmatch(line):
            if announced is not None or finished is not None:
                errors.append("duplicate run header")
            announced = int(m[1])
        elif m := _START.fullmatch(line):
            if announced is None or finished is not None or active is not None:
                errors.append(f"out-of-order RUN: {m[1]}")
            if m[1] in results:
                errors.append(f"duplicate case: {m[1]}")
            active = m[1]
        elif m := _END.fullmatch(line):
            name = m[2]
            if active != name or finished is not None:
                errors.append(f"terminal case {name} does not match RUN {active}")
                continue
            results.setdefault(name, OUTCOMES[m[1].strip()])
            active = None
        elif m := _DONE.fullmatch(line):
            if finished is not None or active is not None or announced is None:
                errors.append("out-of-order or duplicate run footer")
            finished = int(m[1])
        elif "SetUpTestSuite or TearDownTestSuite" in line:
            errors.append(f"suite failure outside a case: {line}")
        elif line.endswith(": Failure") and active is None:
            errors.append(f"assertion outside a case: {line}")
        elif line.startswith(("[ RUN", "[       OK", "[==========]")):
            errors.append(f"malformed gtest record: {line}")
        elif line.startswith(("[  FAILED  ]", "[  SKIPPED ]")) and finished is None:
            errors.append(f"terminal record without duration: {line}")
    interrupted = active is not None
    if interrupted:
        results.setdefault(active, Outcome.TIMEOUT if timed_out else Outcome.ERROR)
    elif timed_out:
        errors.append("watchdog expired outside a case")
    if not interrupted and not timed_out:
        if not results or announced != len(results) or finished != len(results):
            errors.append(
                f"incomplete run: header={announced}, footer={finished}, cases={len(results)}"
            )
        wanted_code = int(Outcome.FAIL in results.values())
        if exit_code != wanted_code:
            errors.append(f"exit {exit_code} disagrees with verdicts (expected {wanted_code})")
    expected = frozenset(listed) if listed is not None else frozenset()
    if listed is not None and (
        announced != len(expected)
        or results.keys() - expected
        or (not interrupted and not timed_out and results.keys() != expected)
    ):
        errors.append(
            f"listing mismatch: missing={sorted(expected - results.keys())}, "
            f"extra={sorted(results.keys() - expected)}, header={announced}"
        )
    return Run(results, tuple(errors), expected)


@dataclass(frozen=True)
class Xfail:
    reason: str
    issue: int
    kind: str = "fail"


def parse_xfail(text: str) -> dict[str, Xfail]:
    """Require unique exact IDs, an optional kind, a reason and an issue."""
    entries = {}
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        m = _XFAIL.fullmatch(line)
        if m is None:
            raise ValueError(
                f"line {number}: expected '<binary:Suite.Case> [kind] # <reason> #<issue>'"
            )
        if m[1] in entries:
            raise ValueError(f"line {number}: duplicate expected failure {m[1]}")
        entries[m[1]] = Xfail(m[3].strip(), int(m[4]), m[2] or "fail")
    return entries


def load_xfail(path: Path) -> dict[str, Xfail]:
    return parse_xfail(path.read_text())


def check_xfail_binaries(root: Path, xfail: Mapping[str, Xfail]) -> None:
    """Reject entries whose binary cannot run, before splitting the xfail list."""
    unknown = sorted({name.split(":", 1)[0] for name in xfail} - set(read_tests(root)))
    if unknown:
        raise ValueError(f"xfail binaries absent from tests.txt: {', '.join(unknown)}")


def _needs_retry(outcome: Outcome, entry: Xfail | None) -> bool:
    return outcome is Outcome.TIMEOUT or (
        outcome is Outcome.FAIL and entry is not None and entry.kind == "flaky"
    )


def compare(
    binary: str, run: Run, baseline: Path | Mapping[str, str], xfail: dict[str, Xfail]
) -> list[str]:
    """Require baseline passes minus xfails; native skips may skip or pass.

    Required retries decide the verdict. Neither ERROR nor missing attempts
    can be covered by any expectation kind. Baseline failures impose no pass
    requirement, but must still produce a non-error, non-timeout verdict.
    """
    reference = read_native(baseline) if isinstance(baseline, Path) else baseline
    reference = {k: v for k, v in reference.items() if k.startswith(binary + ":")}
    xfail = {k: v for k, v in xfail.items() if k.startswith(binary + ":")}
    problems = list(run.errors)
    got = {f"{binary}:{name}": result for name, result in run.final_results.items()}
    problems += [f"not run: {name}" for name in sorted(reference.keys() - got.keys())]
    problems += [f"unexpected case: {name}" for name in sorted(got.keys() - reference.keys())]
    problems += [f"stale xfail: {name}" for name in sorted(xfail.keys() - reference.keys())]
    for case, first in run.results.items():
        required = _needs_retry(first, xfail.get(f"{binary}:{case}"))
        if required and case not in run.retries:
            problems.append(f"missing retry: {binary}:{case}")
        elif case in run.retries and not required:
            problems.append(f"unexpected retry: {binary}:{case}")
    for case in run.retries.keys() - run.results.keys():
        problems.append(f"retry without first attempt: {binary}:{case}")
    for name in sorted(got.keys() & reference.keys()):
        if name in xfail:
            allowed = ALLOWED[xfail[name].kind]
        else:
            allowed = {
                "pass": {Outcome.PASS},
                "skip": {Outcome.PASS, Outcome.SKIP},
                "fail": {Outcome.PASS, Outcome.FAIL, Outcome.SKIP},
            }[reference[name]]
        if got[name] not in allowed:
            problems.append(
                f"{name}: expected {','.join(sorted(o.value for o in allowed))}, got {got[name].value}"
            )
    return problems


def parse_list(text: str) -> set[str]:
    """Read gtest's listing, retaining parameter suffixes, excluding DISABLED_."""
    suite = None
    cases = set()
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].rstrip()
        if not line:
            continue
        if re.fullmatch(r"[A-Za-z0-9_/]+\.", line):
            suite = line
        elif line.startswith("  ") and re.fullmatch(r"[A-Za-z0-9_/]+", line.strip()) and suite:
            name = suite + line.strip()
            if any(part.startswith("DISABLED_") for part in re.split(r"[/.]", name)):
                continue
            if name in cases:
                raise ValueError(f"duplicate listed case: {name}")
            cases.add(name)
        else:
            raise ValueError(f"unrecognized gtest listing: {raw}")
    if not cases:
        raise ValueError("gtest listed no enabled cases")
    return cases


def read_tests(root: Path = BUILD) -> list[str]:
    names = (root / "tests.txt").read_text().splitlines()
    if not names or len(set(names)) != len(names) or any(not _BINARY.fullmatch(n) for n in names):
        raise ValueError("tests.txt must contain unique binary names, one per line")
    return names


def _exited(proc: subprocess.Popen, queue) -> bool:
    """Observe child exit without reaping or releasing its process-group ID."""
    if queue is not None:
        return bool(queue.control([], 1, 0))
    return os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def _execute(
    argv: list[str], log: Path, timeout_s: float, *, cases: bool = False
) -> tuple[str, int, bool]:
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GTEST_", "GVISOR_", "TEST_"))}
    with tempfile.TemporaryDirectory(prefix="gvisor-") as tmp, log.open("wb") as output:
        env["TEST_TMPDIR"] = str(Path(tmp).resolve())
        proc = subprocess.Popen(
            argv,
            cwd=tmp,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        timed_out = False
        queue = None
        try:
            # Observe exit WITHOUT reaping: the pgid must remain reserved until
            # descendants are killed. waitid/WNOWAIT on Linux; kqueue on macOS.
            if not hasattr(os, "waitid"):
                queue = select.kqueue()
                queue.control(
                    [
                        select.kevent(
                            proc.pid,
                            filter=select.KQ_FILTER_PROC,
                            flags=select.KQ_EV_ADD,
                            fflags=select.KQ_NOTE_EXIT,
                        )
                    ],
                    0,
                    0,
                )
            deadline = time.monotonic() + timeout_s
            pending = b""
            with log.open("rb") as reader:
                while True:
                    pending += reader.read()
                    lines = pending.split(b"\n")
                    pending = lines.pop()
                    lines = [line.decode(errors="replace") for line in lines]
                    if cases and any(
                        _START.fullmatch(line) or _END.fullmatch(line) for line in lines
                    ):
                        deadline = time.monotonic() + timeout_s
                    if _exited(proc, queue):
                        break
                    if time.monotonic() >= deadline:
                        timed_out = True
                        break
                    time.sleep(0.01)
        except ProcessLookupError:
            # kqueue registration can race an already exited, unreaped child.
            pass
        finally:
            try:
                kill_group(proc.pid)
            finally:
                proc.wait()
                if queue is not None:
                    queue.close()
    return log.read_text(errors="replace"), proc.returncode, timed_out


def _execute_case(
    argv: list[str], log: Path, timeout_s: float, *, mode: str
) -> tuple[str, int, bool]:
    """Give each per-case attempt a fresh network stack, including retries."""
    if mode == "per-case":
        # Pinned test/runner/main.go:249-255 creates CLONE_NEWNET, and
        # setup_container/setup_container.cc:101-119 brings lo up. The stress
        # helpers shrink ip_local_port_range; sharing that sysctl/TIME_WAIT
        # state across processes exhausts the pool (socket_util.cc:1201-1219).
        argv = [
            "unshare",
            "--net",
            "--",
            "sh",
            "-c",
            (
                'ip link set dev lo up || { echo "gvisor netns: cannot bring lo up" >&2; '
                'exit 125; }; exec "$@"'
            ),
            "gvisor-netns",
            *argv,
        ]
    try:
        text, code, timed_out = _execute(argv, log, timeout_s, cases=True)
    except OSError as error:
        if mode == "per-case":
            raise ValueError(
                f"per-case network namespace launch failed (requires unshare and iproute2): {error}"
            ) from error
        raise
    lines = text.splitlines()
    if (
        mode == "per-case"
        and not any(_START.fullmatch(line) for line in lines)
        and any(line.startswith(("unshare:", "gvisor netns:")) for line in lines)
    ):
        raise ValueError(f"per-case network namespace setup failed: {text.strip()}")
    return text, code, timed_out


def run_binary(
    root: Path,
    binary: str,
    *,
    logs: Path,
    timeout_s: float = CASE_TIMEOUT_S,
    listing_timeout_s: float = LISTING_TIMEOUT_S,
    filters: dict[str, set[str]] | None = None,
    xfail: dict[str, Xfail] | None = None,
    mode: str = "batched",
) -> Run:
    """Isolate per-case processes and networking; batched runs share networking.

    Every timeout and every flaky failure gets exactly one exact-case retry;
    its result decides, with the first attempt preserved for diagnostics.
    """
    if mode not in {"batched", "per-case"}:
        raise ValueError("mode must be batched or per-case")
    if xfail:
        check_xfail_binaries(root, xfail)
    for name, seconds in (("timeout_s", timeout_s), ("listing_timeout_s", listing_timeout_s)):
        if seconds <= 0 or not math.isfinite(seconds):
            raise ValueError(f"{name} must be finite and positive")
    if not _BINARY.fullmatch(binary):
        raise ValueError("invalid binary name")
    if filters is not None and (
        binary not in filters
        or not filters[binary]
        or any(not re.fullmatch(_NAME, c) for c in filters[binary])
    ):
        raise ValueError("filter must contain exact nonempty case names for the binary")
    logs.mkdir(parents=True, exist_ok=True)
    argv = [str((root / "cmake" / binary).resolve()), "--gtest_color=no"]
    positive = ":".join(sorted(filters[binary])) if filters is not None else ""
    listing_args = ["--gtest_filter=" + positive] if positive else []
    results, errors, retries = {}, [], {}
    expected = set()
    try:
        listing, code, timed_out = _execute(
            [*argv, *listing_args, "--gtest_list_tests"],
            logs / f"{binary}.list.log",
            listing_timeout_s,
        )
        if timed_out:
            raise ValueError("listing timed out")
        if code:
            raise ValueError(f"listing failed: exit {code}")
        expected = parse_list(listing)
        if filters is not None and expected != filters[binary]:
            raise ValueError("listing does not match the exact requested filter")
        attempt = 0
        while remaining := expected - results.keys():
            attempt += 1
            selected = {min(remaining)} if mode == "per-case" else remaining
            # Explicit filters shrink as cases complete. Repeating the entire
            # positive list plus exclusions can exceed Linux MAX_ARG_STRLEN.
            if filters is not None or mode == "per-case":
                expression = ":".join(sorted(selected))
            else:
                expression = "-" + ":".join(sorted(results)) if results else ""
            args = ["--gtest_filter=" + expression] if expression else []
            text, code, timed_out = _execute_case(
                [*argv, *args], logs / f"{binary}.{attempt}.log", timeout_s, mode=mode
            )
            run = parse(text, exit_code=code, listed=selected, timed_out=timed_out)
            errors.extend(run.errors)
            for case, verdict in run.results.items():
                if case in results:
                    errors.append(f"resumed case repeated: {case}")
                results.setdefault(case, verdict)
            if errors:
                break
            for case, verdict in run.results.items():
                if _needs_retry(verdict, (xfail or {}).get(f"{binary}:{case}")):
                    try:
                        text, code, timed_out = _execute_case(
                            [*argv, "--gtest_filter=" + case],
                            logs / f"{binary}.retry-{len(retries) + 1}.log",
                            timeout_s,
                            mode=mode,
                        )
                    except (OSError, ValueError):
                        if mode == "per-case":
                            retries[case] = Outcome.ERROR
                        raise
                    retry = parse(text, exit_code=code, listed={case}, timed_out=timed_out)
                    errors.extend(retry.errors)
                    # A retry that never reaches RUN must not fall back to the
                    # original outcome (or become an excusable missing result).
                    retries[case] = retry.results.get(case, Outcome.ERROR)
            if errors:
                break
    except (OSError, ValueError) as error:
        errors.append(str(error))
    return Run(results, tuple(errors), frozenset(expected), retries)


def host_identity() -> dict:
    """Record the actual process identity, kernel, and Linux capability masks."""
    status = Path("/proc/self/status")
    capabilities = None
    if status.exists():
        capabilities = {}
        for line in status.read_text().splitlines():
            if line.startswith("Cap"):
                key, value = line.split(":", 1)
                capabilities[key] = value.strip()
    return {
        "uid": os.getuid(),
        "euid": os.geteuid(),
        "capabilities": capabilities,
        "uname_r": platform.release(),
    }


def report_data(
    binaries: list[str],
    runs: Mapping[str, Run],
    elapsed: float,
    *,
    environment: Mapping | None = None,
) -> dict:
    """Include unmeasured binaries even in a report saved between invocations."""
    return {
        "schema": 3,
        "environment": dict(environment or {}),
        "elapsed_seconds": round(elapsed, 3),
        "binaries": {
            name: {
                "cases": {c: o.value for c, o in r.final_results.items()},
                "errors": r.errors,
                "listed": sorted(r.listed),
                "not_run": sorted(r.not_run),
                "first_attempts": {c: r.results[c].value for c in r.retries},
            }
            for name, r in runs.items()
        },
        "summary": {
            **{
                o.value: sum(list(r.final_results.values()).count(o) for r in runs.values())
                for o in Outcome
            },
            "listed": sum(len(r.listed) for r in runs.values()),
            "not_run": sum(len(r.not_run) for r in runs.values()),
            "errors": sum(len(r.errors) for r in runs.values()),
            "unmeasured": sorted(set(binaries) - runs.keys()),
        },
    }


def native_outcomes(root: Path) -> dict[str, str]:
    """Reject incomplete artifacts before recording or comparing a baseline."""
    binaries = set(read_tests(root))
    if (
        set((root / "targets.txt").read_text().splitlines()) != binaries
        or (root / "not-built.txt").read_text().strip()
    ):
        raise ValueError("native reference needs every target built")
    report = json.loads((root / "native-report.json").read_text())
    if report.get("schema") != 3 or set(report["binaries"]) != binaries:
        raise ValueError("native reference has unmeasured binaries or an old report schema")
    environment = report.get("environment", {})
    if environment.get("netns") != "per-case":
        raise ValueError("native reference requires netns=per-case")
    if (
        environment.get("mode") != "per-case"
        or environment.get("uid") != 0
        or environment.get("euid") != 0
    ):
        raise ValueError("native reference requires a per-case root run")
    if (
        not environment.get("zig_version")
        or not environment.get("uname_r")
        or not environment.get("capabilities")
    ):
        raise ValueError("native reference missing toolchain, kernel or capabilities")
    outcomes = {}
    for name, run in report["binaries"].items():
        cases, listed = run["cases"], run["listed"]
        if (
            run["errors"]
            or not listed
            or len(set(listed)) != len(listed)
            or set(cases) != set(listed)
            or run["not_run"]
            or run["first_attempts"].keys() - cases.keys()
            or any(first != "timeout" for first in run["first_attempts"].values())
        ):
            raise ValueError(f"{name}: incomplete or erroneous native run: {run['errors']}")
        for case, outcome in cases.items():
            key = f"{name}:{case}"
            if not _ID.fullmatch(key) or outcome not in ("pass", "fail", "skip"):
                raise ValueError(f"{key}: cannot record {outcome}")
            outcomes[key] = outcome
    return outcomes


def format_native(outcomes: Mapping[str, str]) -> str:
    """Passes are bare IDs; skips/failures are annotated and compared too."""
    return "# #454 full native reference; annotated skips/failures are not passes.\n" + "".join(
        (name if outcome == "pass" else f"# {outcome}: {name}") + "\n"
        for name, outcome in sorted(outcomes.items())
    )


def read_native(path: Path) -> dict[str, str]:
    outcomes = {}
    try:
        text = path.read_text()
    except FileNotFoundError as error:
        raise ValueError(f"missing {path.name}; re-record with:\n{RECORD_COMMAND}") from error
    for line in text.splitlines():
        outcome = "pass"
        if line.startswith(("# skip: ", "# fail: ")):
            prefix, line = line.split(": ", 1)
            outcome = prefix[2:]
        elif not line or line.startswith("#"):
            if re.match(r"#\s*(?:skip|fail)\b", line, re.IGNORECASE):
                raise ValueError(f"malformed native annotation: {line}")
            continue
        if not _ID.fullmatch(line) or line in outcomes:
            raise ValueError(f"invalid or duplicate native case: {line}")
        outcomes[line] = outcome
    if not outcomes:
        raise ValueError("empty native reference")
    return outcomes


def native_diff(recorded: Mapping[str, str], measured: Mapping[str, str]) -> str:
    """Summarize drift without dumping the entire recorded suite."""
    missing = sorted(recorded.keys() - measured.keys())
    added = sorted(measured.keys() - recorded.keys())
    changed = sorted(k for k in recorded.keys() & measured.keys() if recorded[k] != measured[k])
    details = [f"missing={len(missing)}, added={len(added)}, changed={len(changed)}"]
    details += [f"missing: {k}" for k in missing[:10]]
    details += [f"added: {k} ({measured[k]})" for k in added[:10]]
    details += [f"changed: {k}: {recorded[k]} -> {measured[k]}" for k in changed[:10]]
    return "\n".join(details) + f"\nRe-record after reviewing drift:\n{RECORD_COMMAND}"


class Rule(NamedTuple):
    kind: str
    sources: tuple[str, ...]
    deps: tuple[str, ...]
    allocator: bool
    unsupported: tuple[str, ...]


def build_rules(text: str, package: str) -> dict[str, Rule]:
    """Parse the pinned BUILD vocabulary for x86-64 Linux; reject unknowns.

    This is deliberately not a Starlark interpreter. Single-line lists and
    list concatenations have the same meaning as multiline declarations.
    """

    def strings(node, *, benchmark=False):
        if isinstance(node, ast.List):
            values = []
            for value in node.elts:
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    values.append(value.value)
                elif benchmark and isinstance(value, ast.Name) and value.id in _BENCHMARK_SYMBOLS:
                    # tools/bazeldefs/cc.bzl maps both symbols to this label.
                    values.append("@com_google_benchmark//:benchmark")
                else:
                    raise ValueError(f"unsupported BUILD list element: {ast.unparse(value)}")
            return values
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            return strings(node.left, benchmark=benchmark) + strings(
                node.right, benchmark=benchmark
            )
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and not node.args:
            name = node.func.id
            if name in {"select_gtest", "coreutil", "default_net_util"} and not node.keywords:
                # gmock is linked explicitly; the other two return [] upstream.
                return []
            options = {kw.arg: strings(kw.value, benchmark=benchmark) for kw in node.keywords}
            if name == "select_arch" and options.keys() <= {"amd64", "arm64", "riscv64", "default"}:
                if "amd64" not in options and "default" not in options:
                    raise ValueError("unsupported BUILD selection: no amd64 or default branch")
                return options.get("amd64", options.get("default", []))
            if name == "select_system" and options.keys() <= {"linux", "darwin"}:
                return options.get("linux", ["__linux__"])
        raise ValueError(f"unsupported BUILD expression: {ast.unparse(node)}")

    rules = {}
    for statement in ast.parse(text).body:
        if not isinstance(statement, ast.Expr) or not isinstance(statement.value, ast.Call):
            continue
        call = statement.value
        if not isinstance(call.func, ast.Name) or call.func.id not in {"cc_library", "cc_binary"}:
            continue
        attrs = {kw.arg: kw.value for kw in call.keywords}
        if call.args or len(attrs) != len(call.keywords) or attrs.keys() - _KNOWN_ATTRS:
            raise ValueError(
                f"unsupported BUILD attributes: {sorted(str(k) for k in attrs.keys() - _KNOWN_ATTRS)}"
            )
        name = ast.literal_eval(attrs["name"])
        key = f"//{package}:{name}"
        if key in rules:
            raise ValueError(f"duplicate BUILD rule: {key}")
        sources = strings(attrs["srcs"]) if "srcs" in attrs else []
        deps = strings(attrs["deps"], benchmark=True) if "deps" in attrs else []
        if key == "//test/util:test_util":
            # Only test_util_impl.cc is replaced by the benchmark-free adapter.
            # Any other selected rule needing Benchmark must still fail to link.
            deps = [d for d in deps if d != "@com_google_benchmark//:benchmark"]
        malloc = ast.literal_eval(attrs["malloc"]) if "malloc" in attrs else None
        if malloc not in (None, "//test/util:errno_safe_allocator"):
            raise ValueError(f"unsupported malloc: {malloc}")
        unsupported = tuple(k for k in ("data", "linkopts", "copts") if k in attrs)
        if "defines" in attrs and strings(attrs["defines"]) != ["__linux__"]:
            raise ValueError(f"unsupported defines: {key}")
        rules[key] = Rule(
            call.func.id[3:],
            tuple(f"{package}/{p}" for p in sources),
            tuple(f"//{package}{d}" if d.startswith(":") else d for d in deps),
            malloc is not None,
            unsupported,
        )
    return rules


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    native = sub.add_parser("native")
    native.add_argument("--root", type=Path, default=BUILD)
    native.add_argument("--case-timeout", type=float, default=CASE_TIMEOUT_S)
    native.add_argument("--listing-timeout", type=float, default=LISTING_TIMEOUT_S)
    native.add_argument("--mode", choices=("batched", "per-case"), default="per-case")
    record = sub.add_parser("record")
    record.add_argument("artifact", type=Path)
    record.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "record":
        args.output.write_text(format_native(native_outcomes(args.artifact)))
        return 0
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "AMD64"):
        parser.error("native reference execution requires x86-64 Linux")
    binaries, runs = read_tests(args.root), {}
    environment = {
        **host_identity(),
        "mode": args.mode,
        "netns": "per-case" if args.mode == "per-case" else "shared",
        "zig_version": json.loads((args.root / "build-report.json").read_text())["zig_version"],
    }
    started = time.monotonic()

    def save():
        report = report_data(binaries, runs, time.monotonic() - started, environment=environment)
        (args.root / "native-report.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n"
        )
        return report

    save()
    try:
        for name in binaries:
            print(f"gvisor native: {name}", flush=True)
            runs[name] = run_binary(
                args.root,
                name,
                logs=args.root / "native",
                timeout_s=args.case_timeout,
                listing_timeout_s=args.listing_timeout,
                mode=args.mode,
            )
            report = save()
            for error in runs[name].errors:
                print(f"gvisor native: {name}: {error}", file=sys.stderr, flush=True)
            print(json.dumps(report["summary"]), flush=True)
    finally:
        report = save()
        print(
            "unmeasured binaries: " + (", ".join(report["summary"]["unmeasured"]) or "none"),
            flush=True,
        )
    return int(
        any(
            r.errors
            or r.not_run
            or any(o in {Outcome.TIMEOUT, Outcome.ERROR} for o in r.final_results.values())
            for r in runs.values()
        )
    )


if __name__ == "__main__":
    raise SystemExit(main())
