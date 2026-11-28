"""#454: gtest transcripts, strict xfails, bounded runners and the Linux reference."""

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import time
from dataclasses import FrozenInstanceError
from pathlib import Path
from types import SimpleNamespace

import pytest
from lumen_harness import REPO_ROOT, gvisor, libctest
from lumen_harness.gvisor import Outcome

# googletest v1.18.0 googletest/src/gtest.cc:3547,3575-3594,3628-3646:
# RUN/end records, optional parameter comments before duration, and repeated
# FAILED names in the summary. These fixtures model the printer, not Linux.
SAMPLE = """\
[==========] Running 3 tests from 2 test suites.
[ RUN      ] EventfdTest.Nonblock
[       OK ] EventfdTest.Nonblock (0 ms)
[ RUN      ] All/SocketTest.Send/IPv4
socket.cc:123: Failure
Expected: 0
[  FAILED  ] All/SocketTest.Send/IPv4, where GetParam() = IPv4 (10 ms)
[ RUN      ] EventfdTest.IllegalSeek
skipped on native Linux
[  SKIPPED ] EventfdTest.IllegalSeek (0 ms)
[==========] 3 tests from 2 test suites ran. (10 ms total)
[  PASSED  ] 1 test.
[  SKIPPED ] 1 test, listed below:
[  SKIPPED ] EventfdTest.IllegalSeek
[  FAILED  ] 1 test, listed below:
[  FAILED  ] All/SocketTest.Send/IPv4, where GetParam() = IPv4
"""


def _compare(binary, run, expected, xfail):
    # Legacy parser fixtures express a pass-only baseline as a set.
    return gvisor.compare(binary, run, dict.fromkeys(expected, "pass"), xfail)


NATIVE_ENV = {
    "uid": 0,
    "euid": 0,
    "capabilities": {"CapEff": "0000000000000000"},
    "mode": "per-case",
    "netns": "per-case",
    "zig_version": "0.15.2",
    "uname_r": "fixture",
}


@pytest.mark.parametrize("color", [False, True])
def test_gtest_output_parses_each_case(color):
    text = SAMPLE.replace("[ RUN      ]", "\x1b[32m[ RUN      ]\x1b[0m") if color else SAMPLE
    run = gvisor.parse(text, exit_code=1)
    assert run.errors == ()
    assert run.results == {
        "EventfdTest.Nonblock": Outcome.PASS,
        "All/SocketTest.Send/IPv4": Outcome.FAIL,
        "EventfdTest.IllegalSeek": Outcome.SKIP,
    }


@pytest.mark.parametrize(
    "text,code,error",
    [
        ("[  PASSED  ] 123 tests.\n", 0, "incomplete run"),
        (SAMPLE, 0, "exit 0 disagrees with verdicts (expected 1)"),
        (SAMPLE, 139, "exit 139 disagrees with verdicts (expected 1)"),
        (SAMPLE.replace("[ RUN      ] EventfdTest.Nonblock\n", ""), 1, "does not match RUN None"),
        (
            SAMPLE.replace("EventfdTest.Nonblock (0 ms)", "EventfdTest.Other (0 ms)"),
            1,
            "terminal case EventfdTest.Other",
        ),
        (
            SAMPLE.replace("[       OK ] EventfdTest.Nonblock (0 ms)\n", ""),
            1,
            "out-of-order RUN: All/SocketTest.Send/IPv4",
        ),
        (SAMPLE.replace("Running 3 tests", "Running 4 tests"), 1, "incomplete run: header=4"),
        (
            SAMPLE.replace("3 tests from 2 test suites ran.", "4 tests from 2 test suites ran."),
            1,
            "footer=4",
        ),
        (SAMPLE.split("[==========] 3 tests")[0], 1, "footer=None"),
        (SAMPLE + SAMPLE, 1, "duplicate run header"),
        (
            SAMPLE.replace("EventfdTest.IllegalSeek", "EventfdTest.Nonblock"),
            1,
            "duplicate case: EventfdTest.Nonblock",
        ),
        (SAMPLE.replace(" (0 ms)", ""), 1, "malformed gtest record"),
        (
            SAMPLE + "SetUpTestSuite or TearDownTestSuite failed\n",
            1,
            "suite failure outside a case",
        ),
        ("file.cc:42: Failure\n" + SAMPLE, 1, "assertion outside a case: file.cc:42: Failure"),
    ],
)
def test_broken_runs_never_become_expected_failures(text, code, error):
    run = gvisor.parse(text, exit_code=code)
    assert any(error in e for e in run.errors), run.errors
    expected = {f"socket_test:{case}" for case in run.results}
    xfail = {case: gvisor.Xfail("fixture", 454) for case in expected}
    assert any(error in e for e in _compare("socket_test", run, expected, xfail))


@pytest.mark.parametrize(
    "entry",
    [
        "eventfd_test:E.A",
        "eventfd_test:E.A # reason",
        "eventfd_test:E.A # #454",
        "eventfd_test:E.A # reason #0",
        "eventfd_test:E.A # reason #x",
        "eventfd_test:E.* # reason #454",
        "../eventfd_test:E.A # reason #454",
        "eventfd_test:E.A # reason #454\neventfd_test:E.A # again #455",
    ],
)
def test_xfail_entries_need_a_reason_and_issue(entry, tmp_path):
    with pytest.raises(ValueError, match="line"):
        gvisor.parse_xfail(entry)
    path = tmp_path / "xfail.txt"
    path.write_text("# comment\n\nsocket_test:All/Suite.Case/0 # unsupported option #454\n")
    assert gvisor.load_xfail(path) == {
        "socket_test:All/Suite.Case/0": gvisor.Xfail("unsupported option", 454)
    }


@pytest.mark.parametrize("outcome", list(Outcome))
def test_xfail_is_strict_and_missing_extra_stale_cases_fail(outcome):
    test = "eventfd_test:E.A"
    run = gvisor.Run(
        {"E.A": outcome}, retries={"E.A": outcome} if outcome is Outcome.TIMEOUT else {}
    )
    xfail = {test: gvisor.Xfail("fixture", 454)}
    assert bool(_compare("eventfd_test", run, {test}, xfail)) == (outcome is not Outcome.FAIL)
    assert bool(_compare("eventfd_test", run, {test}, {})) == (outcome is not Outcome.PASS)
    assert _compare("eventfd_test", run, {test, "eventfd_test:E.B"}, {})
    assert _compare("eventfd_test", run, set(), {})
    assert _compare("eventfd_test", run, set(), xfail)


def test_listing_keeps_parameters_and_excludes_only_disabled_cases():
    assert gvisor.parse_list(
        "All/Suite.\n  Case/0  # GetParam() = 123\n  Case/1\n  DISABLED_Off/0\n"
        "Typed/0.  # TypeParam = int\n  Case\nDISABLED_Suite.\n  Case\n"
    ) == {"All/Suite.Case/0", "All/Suite.Case/1", "Typed/0.Case"}
    for bad in ("", "Suite.\n  Case\n  Case\n", "garbage\n", "  Orphan\n"):
        with pytest.raises(ValueError):
            gvisor.parse_list(bad)


def _fake(root: Path, body: str):
    binary = root / "cmake/eventfd_test"
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_text("#!/bin/sh\n" + body)
    binary.chmod(0o755)


GOOD = """\
[==========] Running 1 test from 1 test suite.
[ RUN      ] E.A
[       OK ] E.A (0 ms)
[==========] 1 test from 1 test suite ran. (0 ms total)
[  PASSED  ] 1 test.
"""
LISTING = 'case "$*" in *--gtest_list_tests*) printf "E.\\n  A\\n"; exit 0;; esac\n'


def test_native_runner_checks_listing_exit_and_cleans_environment(tmp_path, monkeypatch):
    for key in ("GTEST_FILTER", "TEST_ON_GVISOR", "GVISOR_PLATFORM"):
        monkeypatch.setenv(key, "bad")
    _fake(
        tmp_path,
        LISTING
        + '[ -z "$GTEST_FILTER$TEST_ON_GVISOR$GVISOR_PLATFORM" ] || exit 5\n'
        + '[ "$PWD" = "$TEST_TMPDIR" ] || exit 6\n'
        + "cat <<'EOF'\n"
        + GOOD
        + "EOF\n",
    )
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs")
    assert run == gvisor.Run({"E.A": Outcome.PASS}, listed={"E.A"})
    assert (tmp_path / "logs/eventfd_test.1.log").read_text() == GOOD
    assert not (tmp_path / "logs/eventfd_test.2.log").exists()


@pytest.mark.parametrize("filtered", [False, True])
def test_native_runner_resumes_after_timeout_without_replacing_verdicts(
    tmp_path, monkeypatch, filtered
):
    args = tmp_path / "args"
    resumed = "E.C" if filtered else "-E.A:E.B"
    initial = " --gtest_filter=E.A:E.B:E.C" if filtered else ""
    _fake(
        tmp_path,
        f'echo "$*" >> "{args}"\n'
        + 'case "$*" in *--gtest_list_tests*) printf "E.\\n  A\\n  B\\n  C\\n"; exit 0;; esac\n'
        + f"case \"$*\" in *--gtest_filter={resumed}) cat <<'EOF'\n"
        + GOOD.replace("E.A", "E.C")
        + "EOF\nexit 0;; esac\n"
        + "case \"$*\" in *--gtest_filter=E.B) cat <<'EOF'\n"
        + GOOD.replace("E.A", "E.B")
        + "EOF\nexit 0;; esac\n"
        + "cat <<'EOF'\n[==========] Running 3 tests from 1 test suite.\n"
        + "[ RUN      ] E.A\n[       OK ] E.A (0 ms)\n[ RUN      ] E.B\nEOF\nsleep 30\n",
    )
    execute = gvisor._execute

    def short_hanging_attempt(argv, log, timeout_s, **kwargs):
        return execute(argv, log, 0.5 if log.name == "eventfd_test.1.log" else timeout_s, **kwargs)

    monkeypatch.setattr(gvisor, "_execute", short_hanging_attempt)
    filters = {"eventfd_test": {"E.A", "E.B", "E.C"}} if filtered else None
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "logs", timeout_s=5, filters=filters
    )
    assert run.results == {"E.A": Outcome.PASS, "E.B": Outcome.TIMEOUT, "E.C": Outcome.PASS}
    assert run.final_results == dict.fromkeys(("E.A", "E.B", "E.C"), Outcome.PASS)
    assert run.errors == () and not run.not_run
    assert args.read_text().splitlines() == [
        "--gtest_color=no" + initial + " --gtest_list_tests",
        "--gtest_color=no" + initial,
        "--gtest_color=no --gtest_filter=E.B",
        "--gtest_color=no --gtest_filter=" + resumed,
    ]
    # A misbehaving resumed binary cannot overwrite A's original pass.
    _fake(
        tmp_path,
        (tmp_path / "cmake/eventfd_test")
        .read_text()
        .split("\n", 1)[1]
        .replace(GOOD.replace("E.A", "E.C"), GOOD.replace("[       OK ]", "[  FAILED  ]")),
    )
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "bad", timeout_s=5, filters=filters
    )
    assert run.results["E.A"] == Outcome.PASS
    assert "resumed case repeated: E.A" in run.errors
    assert run.not_run == {"E.C"}


@pytest.mark.parametrize("prefix", ["", GOOD, GOOD.split("[==========] 1 test")[0]])
def test_native_runner_watchdog_outside_case_is_error(tmp_path, prefix):
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + prefix + "EOF\nsleep 30\n")
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", timeout_s=0.5)
    assert "watchdog expired outside a case" in run.errors
    assert not any("exit " in e for e in run.errors)
    assert not (tmp_path / "logs/eventfd_test.2.log").exists()


@pytest.mark.parametrize(
    "tail,error", [("exit 1", "listing failed: exit 1"), ("sleep 30", "listing timed out")]
)
def test_native_runner_rejects_partial_listing(tmp_path, tail, error):
    _fake(tmp_path, 'printf "E.\\n  A\\n"\n' + tail + "\n")
    run = gvisor.run_binary(
        tmp_path,
        "eventfd_test",
        logs=tmp_path / "logs",
        listing_timeout_s=0.5 if tail == "sleep 30" else 5,
    )
    assert run.errors == (error,)
    assert run.results == {}
    assert not (tmp_path / "logs/eventfd_test.1.log").exists()


def test_native_runner_listing_has_its_own_timeout(tmp_path, monkeypatch):
    _fake(
        tmp_path,
        'case "$*" in *--gtest_list_tests*) sleep 0.7; printf "E.\\n  A\\n"; exit 0;; esac\n'
        + "cat <<'EOF'\n"
        + GOOD
        + "EOF\n",
    )
    execute, timeouts = gvisor._execute, []

    def record_timeouts(argv, log, timeout_s, **kwargs):
        timeouts.append(timeout_s)
        return execute(argv, log, timeout_s, **kwargs)

    monkeypatch.setattr(gvisor, "_execute", record_timeouts)
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", timeout_s=5)
    assert run.results == {"E.A": Outcome.PASS} and not run.errors
    assert timeouts == [gvisor.LISTING_TIMEOUT_S, 5]


def test_native_runner_rejects_missing_binaries_and_filtered_or_missing_cases(tmp_path):
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "missing")
    assert "No such file or directory" in run.errors[0]
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD.replace("E.A", "E.B") + "EOF\n")
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "mismatch")
    assert any("listing mismatch" in e for e in run.errors)
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "filter", filters={"eventfd_test": {"E.B"}}
    )
    assert run.errors == ("listing does not match the exact requested filter",)


@pytest.mark.parametrize(
    "options,error",
    [
        ({"timeout_s": 0}, "timeout_s"),
        ({"timeout_s": -1}, "timeout_s"),
        ({"timeout_s": float("nan")}, "timeout_s"),
        ({"timeout_s": float("inf")}, "timeout_s"),
        ({"listing_timeout_s": 0}, "listing_timeout_s"),
        ({"listing_timeout_s": float("inf")}, "listing_timeout_s"),
        ({"filters": {}}, "filter must"),
        ({"filters": {"eventfd_test": set()}}, "filter must"),
        ({"filters": {"eventfd_test": {"E.*"}}}, "filter must"),
    ],
)
def test_native_runner_validates_inputs(tmp_path, options, error):
    with pytest.raises(ValueError, match=error):
        gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", **options)


def test_native_runner_passes_exact_filter(tmp_path):
    args = tmp_path / "args"
    _fake(tmp_path, f'echo "$*" >> "{args}"\n' + LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "logs", filters={"eventfd_test": {"E.A"}}
    )
    assert not run.errors
    assert all("--gtest_filter=E.A" in line for line in args.read_text().splitlines())


def test_build_manifest_cannot_be_empty_duplicate_or_escape_root(tmp_path):
    for text in ("", "../eventfd_test\n", "eventfd_test\neventfd_test\n"):
        (tmp_path / "tests.txt").write_text(text)
        with pytest.raises(ValueError):
            gvisor.read_tests(tmp_path)
    (tmp_path / "tests.txt").write_text("eventfd_test\nepoll_test\n")
    (tmp_path / "build-report.json").write_text(json.dumps({"zig_version": "0.15.2"}))
    assert gvisor.read_tests(tmp_path) == ["eventfd_test", "epoll_test"]


def test_the_native_reference_run_passes_the_recorded_set():
    baseline = REPO_ROOT / "tests/conformance/gvisor-native.txt"
    if not os.environ.get("LUMEN_GVISOR_REFERENCE"):
        pytest.skip("full native reference is gated in gvisor-reference.yml")
    recorded = gvisor.read_native(baseline)
    measured = gvisor.native_outcomes(gvisor.BUILD)
    assert measured == recorded, gvisor.native_diff(recorded, measured)


def test_cross_build_contains_the_recorded_targets():
    if not (gvisor.BUILD / "build-report.json").exists():
        if os.environ.get("LUMEN_GVISOR_REFERENCE"):
            pytest.fail("missing build-report.json")
        pytest.skip("no gVisor cross-build")
    report = json.loads((gvisor.BUILD / "build-report.json").read_text())
    assert report["zig_version"] == "0.15.2"
    assert "uid" in report and "capabilities" in report
    assert report["uname_r"]
    assert report["cmake_version"].startswith("cmake version ")
    assert report["ninja_version"]
    assert len(gvisor.read_tests()) == 34
    assert (gvisor.BUILD / "not-built.txt").read_text() == ""
    assert set(report["binaries"]) == set(gvisor.read_tests())
    for name, metadata in report["binaries"].items():
        data = (gvisor.BUILD / "cmake" / name).read_bytes()
        elf = libctest.read_elf(gvisor.BUILD / "cmake" / name)
        assert elf.interp is None and not elf.dynamic
        assert elf.stack_size == 0
        assert metadata == {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
        assert str(REPO_ROOT).encode() not in data


def test_native_runner_kills_children_after_a_successful_parent(tmp_path):
    pidfile = tmp_path / "child.pid"
    _fake(
        tmp_path,
        LISTING + f'sleep 30 &\necho $! > "{pidfile}"\n' + "cat <<'EOF'\n" + GOOD + "EOF\n",
    )
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs")
    assert run == gvisor.Run({"E.A": Outcome.PASS}, listed={"E.A"})
    pid = int(pidfile.read_text())

    def running():
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        # Linux containers may leave an orphan zombie for PID 1 to reap.
        stat = Path(f"/proc/{pid}/stat")
        try:
            return stat.read_text().rsplit(")", 1)[1].split()[0] != "Z"
        except FileNotFoundError:
            return sys.platform != "linux"

    for _ in range(100):
        if not running():
            break
        time.sleep(0.01)
    assert not running(), f"child {pid} survived the runner"


def test_native_cli_records_case_results_and_returns_failure(tmp_path, monkeypatch, fake_netns):
    monkeypatch.setattr(gvisor.sys, "platform", "linux")
    monkeypatch.setattr(gvisor.platform, "machine", lambda: "x86_64")
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    (tmp_path / "build-report.json").write_text(json.dumps({"zig_version": "0.15.2"}))
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    assert gvisor.main(["native", "--root", str(tmp_path)]) == 0
    report = json.loads((tmp_path / "native-report.json").read_text())
    assert report["binaries"]["eventfd_test"]["cases"] == {"E.A": "pass"}
    assert report["environment"] == {
        **gvisor.host_identity(),
        "mode": "per-case",
        "netns": "per-case",
        "zig_version": "0.15.2",
    }
    assert report["summary"] == {
        "pass": 1,
        "fail": 0,
        "skip": 0,
        "timeout": 0,
        "error": 0,
        "listed": 1,
        "not_run": 0,
        "errors": 0,
        "unmeasured": [],
    }
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\nexit 1\n")
    assert gvisor.main(["native", "--root", str(tmp_path)]) == 1
    report = json.loads((tmp_path / "native-report.json").read_text())
    assert report["binaries"]["eventfd_test"]["errors"] == [
        "exit 1 disagrees with verdicts (expected 0)"
    ]
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD.split("[       OK ]")[0] + "EOF\nexit 17\n")
    assert gvisor.main(["native", "--root", str(tmp_path)]) == 1
    report = json.loads((tmp_path / "native-report.json").read_text())
    assert report["binaries"]["eventfd_test"]["cases"] == {"E.A": "error"}
    assert report["summary"]["error"] == 1


def test_native_cli_lists_unmeasured_binaries(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(gvisor.sys, "platform", "linux")
    monkeypatch.setattr(gvisor.platform, "machine", lambda: "x86_64")
    (tmp_path / "tests.txt").write_text("eventfd_test\nepoll_test\n")
    (tmp_path / "build-report.json").write_text(json.dumps({"zig_version": "0.15.2"}))

    def interrupted(*args, **kwargs):
        raise KeyboardInterrupt

    monkeypatch.setattr(gvisor, "run_binary", interrupted)
    with pytest.raises(KeyboardInterrupt):
        gvisor.main(["native", "--root", str(tmp_path)])
    report = json.loads((tmp_path / "native-report.json").read_text())
    assert report["summary"]["unmeasured"] == ["epoll_test", "eventfd_test"]
    assert "unmeasured binaries: epoll_test, eventfd_test" in capsys.readouterr().out


def test_fetch_checks_pins_and_fsck_without_network(tmp_path):
    tools = tmp_path / "tools"
    tools.mkdir()
    shutil.copyfile(REPO_ROOT / "tools/fetch-gvisor-tests.sh", tools / "fetch-gvisor-tests.sh")
    sources = tmp_path / "build/gvisor-sources"
    for name in ("gvisor", "googletest", "abseil"):
        (sources / name / ".git").mkdir(parents=True)
    # Stub git records every invocation; the real network path is never used.
    git = tools / "git"
    git.write_text(
        "#!/bin/sh\n"
        + f'echo "$*" >> "{tmp_path}/git.args"\n'
        + 'case "$*" in *rev-parse*) case "$2" in *googletest) echo 063de7e9578f82b369302001269680b4b1553359;; *abseil) echo 2065f4ded0558c6f89fee67c8e5228feb4eb960e;; *) echo cc5b7474bf134432a4f41a47d37022fc4f05ede6;; esac;; *fsck*) exit "${FSCK_EXIT:-0}";; esac\n'
    )
    git.chmod(0o755)
    env = {**os.environ, "PATH": f"{tools}:{os.environ['PATH']}"}
    argv = ["bash", str(tools / "fetch-gvisor-tests.sh"), "--check"]
    good = subprocess.run(argv, env=env, capture_output=True, text=True, check=False)
    assert good.returncode == 0, good.stderr
    args = (tmp_path / "git.args").read_text()
    assert args.count("fsck --full --no-dangling") == 3
    assert "fetch " not in args
    bad = subprocess.run(
        argv, env={**env, "FSCK_EXIT": "1"}, capture_output=True, text=True, check=False
    )
    assert bad.returncode == 1
    assert "gvisor missing, changed, or corrupt" in bad.stderr


def test_xfail_stale_entries_are_scoped_per_binary():
    entries = gvisor.parse_xfail("epoll_test:E.A # fixture #454\neventfd_test:E.B # stale #454")
    assert _compare(
        "eventfd_test", gvisor.Run({"E.A": Outcome.PASS}), {"eventfd_test:E.A"}, entries
    ) == ["stale xfail: eventfd_test:E.B"]


@pytest.mark.parametrize(
    "kind,allowed",
    [
        ("fail", {Outcome.FAIL}),
        ("skip", {Outcome.SKIP}),
        ("timeout", {Outcome.TIMEOUT}),
        ("flaky", {Outcome.PASS, Outcome.FAIL, Outcome.TIMEOUT}),
    ],
)
def test_xfail_kinds_allow_only_their_outcomes(kind, allowed):
    entries = gvisor.parse_xfail(f"eventfd_test:E.A {kind} # fixture #454")
    for outcome in Outcome:
        problems = _compare(
            "eventfd_test",
            gvisor.Run(
                {"E.A": outcome},
                retries={"E.A": outcome}
                if outcome is Outcome.TIMEOUT or (kind == "flaky" and outcome is Outcome.FAIL)
                else {},
            ),
            {"eventfd_test:E.A"},
            entries,
        )
        assert bool(problems) == (outcome not in allowed)


def test_flaky_failure_retries_once_and_uses_final_verdict(tmp_path):
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    marker = tmp_path / "marker"
    _fake(
        tmp_path,
        LISTING
        + f"if [ -e \"{marker}\" ]; then\ncat <<'EOF'\n"
        + GOOD
        + f"EOF\nexit 0\nfi\ntouch \"{marker}\"\ncat <<'EOF'\n"
        + GOOD.replace("[       OK ]", "[  FAILED  ]")
        + "EOF\nexit 1\n",
    )
    xfail = gvisor.parse_xfail("eventfd_test:E.A flaky # fixture #454")
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", xfail=xfail)
    assert run.results == {"E.A": Outcome.FAIL}
    assert run.retries == {"E.A": Outcome.PASS}
    assert run.final_results == {"E.A": Outcome.PASS}
    assert not _compare("eventfd_test", run, {"eventfd_test:E.A"}, xfail)
    assert len(list((tmp_path / "logs").glob("*.retry-*.log"))) == 1


def test_run_is_frozen_and_parse_checks_listing():
    run = gvisor.parse(GOOD, exit_code=0, listed={"E.A", "E.B"})
    assert any("listing mismatch" in e for e in run.errors)
    assert run.not_run == {"E.B"}
    with pytest.raises(FrozenInstanceError):
        run.errors = ()
    with pytest.raises(TypeError):
        run.results["E.A"] = Outcome.FAIL


def test_record_roundtrips_full_set_and_rejects_incomplete_reports(tmp_path):
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    (tmp_path / "build-report.json").write_text(json.dumps({"zig_version": "0.15.2"}))
    (tmp_path / "targets.txt").write_text("eventfd_test\n")
    (tmp_path / "not-built.txt").write_text("")
    report_file = tmp_path / "native-report.json"
    run = gvisor.Run(
        {"E.A": Outcome.PASS, "E.B": Outcome.SKIP, "E.C": Outcome.FAIL},
        listed={"E.A", "E.B", "E.C"},
    )
    report = gvisor.report_data(
        ["eventfd_test"], {"eventfd_test": run}, 1.0, environment=NATIVE_ENV
    )
    report_file.write_text(json.dumps(report))
    output = tmp_path / "baseline.txt"
    assert gvisor.main(["record", str(tmp_path), "--output", str(output)]) == 0
    assert gvisor.read_native(output) == {
        "eventfd_test:E.A": "pass",
        "eventfd_test:E.B": "skip",
        "eventfd_test:E.C": "fail",
    }
    assert "# skip: eventfd_test:E.B" in output.read_text()
    for broken in (
        gvisor.Run({"E.A": Outcome.PASS}, listed={"E.A", "E.B"}),
        gvisor.Run({"E.A": Outcome.TIMEOUT}, listed={"E.A"}),
        gvisor.Run({"E.A": Outcome.ERROR}, listed={"E.A"}),
        gvisor.Run({"E.A": Outcome.PASS}, errors=("exit disagrees",), listed={"E.A"}),
    ):
        report_file.write_text(
            json.dumps(
                gvisor.report_data(
                    ["eventfd_test"], {"eventfd_test": broken}, 1, environment=NATIVE_ENV
                )
            )
        )
        with pytest.raises(ValueError):
            gvisor.native_outcomes(tmp_path)
    report_file.write_text(json.dumps(gvisor.report_data(["eventfd_test"], {}, 1)))
    with pytest.raises(ValueError, match="unmeasured"):
        gvisor.native_outcomes(tmp_path)


def test_build_parser_preserves_one_line_lists_and_rejects_unknown_syntax():
    rule = gvisor.build_rules('cc_binary(name="a", srcs=["a.cc"], deps=[":b"])', "test")["//test:a"]
    assert rule.sources == ("test/a.cc",) and rule.deps == ("//test:b",)
    benchmark = gvisor.build_rules('cc_binary(name="a", deps=[gbenchmark])', "test")["//test:a"]
    assert benchmark.deps == ("@com_google_benchmark//:benchmark",)
    for attr in (
        "deps=unknown()",
        'srcs=glob(["*.cc"])',
        "deps=[unknown]",
        "srcs=[gbenchmark]",
        'srcs=select_arch(arm64=["a.cc"])',
        "mystery=[]",
    ):
        with pytest.raises(ValueError, match="unsupported BUILD"):
            gvisor.build_rules(f'cc_binary(name="a", {attr})', "test")


def test_elf_dynamic_segment_without_interpreter_is_detected(tmp_path):
    # Minimal ELF64 with a single PT_DYNAMIC, no interpreter or sections.
    data = bytearray(120)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, 62)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    struct.pack_into("<I", data, 64, 2)
    binary = tmp_path / "dynamic"
    binary.write_bytes(data)
    elf = libctest.read_elf(binary)
    assert elf.interp is None and elf.dynamic


def test_watchdog_has_no_whole_binary_ceiling(tmp_path):
    _fake(
        tmp_path,
        'case "$*" in *--gtest_list_tests*) printf "E.\n  A\n  B\n  C\n"; exit 0;; esac\n'
        + 'echo "[==========] Running 3 tests from 1 test suite."\n'
        + "for name in A B C; do\n"
        + 'echo "[ RUN      ] E.$name"; sleep 1; echo "[       OK ] E.$name (1000 ms)"\n'
        + 'done\necho "[==========] 3 tests from 1 test suite ran. (3000 ms total)"\n',
    )
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", timeout_s=2.5)
    assert run.results == dict.fromkeys(("E.A", "E.B", "E.C"), Outcome.PASS)
    assert not run.errors
    assert not (tmp_path / "logs/eventfd_test.2.log").exists()


def test_native_runner_kills_group_before_reaping(tmp_path, monkeypatch):
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    killed = set()
    kill_group, wait = gvisor.kill_group, subprocess.Popen.wait

    def kill(pgid):
        killed.add(pgid)
        kill_group(pgid)

    def reap(proc, *args, **kwargs):
        assert proc.pid in killed
        return wait(proc, *args, **kwargs)

    monkeypatch.setattr(gvisor, "kill_group", kill)
    monkeypatch.setattr(subprocess.Popen, "wait", reap)
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs")
    assert not run.errors and len(killed) == 2


def test_native_gate_rejects_drift_and_missing_baseline_in_ci(tmp_path, monkeypatch):
    monkeypatch.setattr(sys.modules[__name__], "REPO_ROOT", tmp_path)
    monkeypatch.setenv("CI", "true")
    monkeypatch.setenv("LUMEN_GVISOR_REFERENCE", "1")
    with pytest.raises(ValueError, match="missing gvisor-native.txt") as missing:
        test_the_native_reference_run_passes_the_recorded_set()
    assert gvisor.RECORD_COMMAND in str(missing.value)
    baseline = tmp_path / "tests/conformance/gvisor-native.txt"
    baseline.parent.mkdir(parents=True)
    expected = {"eventfd_test:E.A": "pass", "eventfd_test:E.B": "skip"}
    baseline.write_text(gvisor.format_native(expected))
    monkeypatch.setattr(gvisor, "native_outcomes", lambda _: expected.copy())
    test_the_native_reference_run_passes_the_recorded_set()
    for changed in (
        {"eventfd_test:E.A": "fail", "eventfd_test:E.B": "skip"},
        {"eventfd_test:E.A": "pass", "eventfd_test:E.B": "fail"},
        {"eventfd_test:E.A": "pass"},
    ):
        monkeypatch.setattr(gvisor, "native_outcomes", lambda _, result=changed: result)
        with pytest.raises(AssertionError, match="Re-record after reviewing drift"):
            test_the_native_reference_run_passes_the_recorded_set()


@pytest.mark.parametrize("mode", ["batched", "per-case"])
@pytest.mark.parametrize(
    "last", [Outcome.PASS, Outcome.FAIL, Outcome.SKIP, Outcome.TIMEOUT, Outcome.ERROR, None]
)
def test_a_timed_out_case_is_run_once_more(tmp_path, monkeypatch, mode, last):
    prefix = GOOD.split("[       OK ]")[0]
    if last is Outcome.TIMEOUT:
        final = (prefix, -9, True)
    elif last is Outcome.ERROR:
        final = (prefix, -11, False)
    elif last is None:
        final = ("", 0, False)
    else:
        terminal = {
            Outcome.PASS: "[       OK ]",
            Outcome.FAIL: "[  FAILED  ]",
            Outcome.SKIP: "[  SKIPPED ]",
        }[last]
        final = (GOOD.replace("[       OK ]", terminal), int(last is Outcome.FAIL), False)
    outputs = iter([("E.\n  A\n", 0, False), (prefix, -9, True), final])
    calls = []

    def execute(argv, log, timeout_s, **kwargs):
        calls.append(argv)
        return next(outputs)

    monkeypatch.setattr(gvisor, "_execute", execute)
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", mode=mode)
    assert len(calls) == 3 and calls[-1][-1] == "--gtest_filter=E.A"
    assert run.results == {"E.A": Outcome.TIMEOUT}
    assert run.final_results == {"E.A": last or Outcome.ERROR}
    assert not run.errors if last is not None else run.errors
    report = gvisor.report_data(["eventfd_test"], {"eventfd_test": run}, 1, environment=NATIVE_ENV)
    assert report["binaries"]["eventfd_test"]["cases"] == {"E.A": (last or Outcome.ERROR).value}
    assert report["binaries"]["eventfd_test"]["first_attempts"] == {"E.A": "timeout"}
    baseline = {"eventfd_test:E.A": "pass"}
    assert bool(gvisor.compare("eventfd_test", run, baseline, {})) == (last is not Outcome.PASS)
    timeout = gvisor.parse_xfail("eventfd_test:E.A timeout # deterministic hang #454")
    assert bool(gvisor.compare("eventfd_test", run, baseline, timeout)) == (
        last is not Outcome.TIMEOUT
    )
    if last in (Outcome.PASS, Outcome.FAIL, Outcome.SKIP):
        (tmp_path / "tests.txt").write_text("eventfd_test\n")
        (tmp_path / "targets.txt").write_text("eventfd_test\n")
        (tmp_path / "not-built.txt").write_text("")
        (tmp_path / "native-report.json").write_text(json.dumps(report))
        assert gvisor.native_outcomes(tmp_path) == {"eventfd_test:E.A": last.value}


@pytest.mark.parametrize("first", [Outcome.FAIL, Outcome.TIMEOUT])
@pytest.mark.parametrize("last", [*Outcome, None])
def test_a_flaky_entry_covers_a_fail_or_timeout_never_an_error(first, last):
    run = gvisor.Run({"E.A": first}, retries={"E.A": last} if last else {})
    entry = gvisor.parse_xfail("eventfd_test:E.A flaky # fixture #454")
    problems = gvisor.compare("eventfd_test", run, {"eventfd_test:E.A": "pass"}, entry)
    assert bool(problems) == (last not in {Outcome.PASS, Outcome.FAIL, Outcome.TIMEOUT})
    # An ERROR cannot be turned into a pass through an unauthorized retry.
    error = gvisor.Run({"E.A": Outcome.ERROR}, retries={"E.A": Outcome.PASS})
    assert "unexpected retry: eventfd_test:E.A" in gvisor.compare(
        "eventfd_test", error, {"eventfd_test:E.A": "pass"}, entry
    )


@pytest.mark.parametrize("code", [-11, 0, 1])
def test_crashed_or_unfinished_case_is_error_and_batch_resumes(tmp_path, monkeypatch, code):
    outputs = iter(
        [
            ("E.\n  A\n  B\n  C\n", 0, False),
            (
                "[==========] Running 3 tests from 1 test suite.\n[ RUN      ] E.A\n[       OK ] E.A (0 ms)\n[ RUN      ] E.B\n",
                code,
                False,
            ),
            (GOOD.replace("E.A", "E.C"), 0, False),
        ]
    )
    calls = []

    def execute(argv, log, timeout_s, **kwargs):
        calls.append(argv)
        return next(outputs)

    monkeypatch.setattr(gvisor, "_execute", execute)
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs")
    assert run.results == {"E.A": Outcome.PASS, "E.B": Outcome.ERROR, "E.C": Outcome.PASS}
    assert not run.errors and not run.not_run and not run.retries
    assert calls[-1][-1] == "--gtest_filter=-E.A:E.B"
    problems = _compare("eventfd_test", run, {"eventfd_test:" + c for c in run.results}, {})
    assert problems == ["eventfd_test:E.B: expected pass, got error"]


def test_per_case_mode_launches_each_exact_filter(tmp_path, fake_netns):
    args = tmp_path / "args"
    _fake(
        tmp_path,
        f'echo "$*" >> "{args}"\n'
        + 'case "$*" in *--gtest_list_tests*) printf "E.\n  A\n  B\n"; exit 0;; esac\n'
        + 'case "$*" in *--gtest_filter=E.A) name=A;; *--gtest_filter=E.B) name=B;; *) exit 7;; esac\n'
        + 'echo "[==========] Running 1 test from 1 test suite."\n'
        + 'echo "[ RUN      ] E.$name"\necho "[       OK ] E.$name (0 ms)"\n'
        + 'echo "[==========] 1 test from 1 test suite ran. (0 ms total)"\n',
    )
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "logs", mode="per-case", timeout_s=5
    )
    assert not run.errors and run.final_results == {"E.A": Outcome.PASS, "E.B": Outcome.PASS}
    assert args.read_text().splitlines() == [
        "--gtest_color=no --gtest_list_tests",
        "--gtest_color=no --gtest_filter=E.A",
        "--gtest_color=no --gtest_filter=E.B",
    ]

    assert fake_netns.read_text().splitlines() == [
        "unshare 1",
        "ip 1 link set dev lo up",
        "unshare 2",
        "ip 2 link set dev lo up",
    ]


def test_non_utf8_output_does_not_discard_verdicts(tmp_path, fake_netns):
    before, after = GOOD.split("[       OK ]", 1)
    _fake(
        tmp_path,
        LISTING
        + "cat <<'EOF'\n"
        + before
        + "EOF\nprintf '\\377\\376\\n'\ncat <<'EOF'\n[       OK ]"
        + after
        + "EOF\n",
    )
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "logs", mode="per-case", timeout_s=5
    )
    assert not run.errors and run.final_results == {"E.A": Outcome.PASS}
    assert b"\xff\xfe" in (tmp_path / "logs/eventfd_test.1.log").read_bytes()


def test_compare_reads_native_skips_and_checks_all_xfail_binaries(tmp_path):
    baseline = tmp_path / "native.txt"
    baseline.write_text("eventfd_test:E.A\n# skip: eventfd_test:E.B\n# fail: eventfd_test:E.C\n")
    run = gvisor.Run({"E.A": Outcome.PASS, "E.B": Outcome.SKIP, "E.C": Outcome.FAIL})
    assert not gvisor.compare("eventfd_test", run, baseline, {})
    changed = gvisor.Run({"E.A": Outcome.SKIP, "E.B": Outcome.PASS, "E.C": Outcome.PASS})
    assert gvisor.compare("eventfd_test", changed, baseline, {}) == [
        "eventfd_test:E.A: expected pass, got skip"
    ]
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    entries = gvisor.parse_xfail("eventfd_test:E.A # failure #454\nepoll_test:E.A # typo #454")
    with pytest.raises(ValueError, match="xfail binaries absent from tests.txt: epoll_test"):
        gvisor.check_xfail_binaries(tmp_path, entries)
    with pytest.raises(ValueError, match="xfail binaries absent"):
        gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", xfail=entries)
    gvisor.check_xfail_binaries(tmp_path, {"eventfd_test:E.A": entries["eventfd_test:E.A"]})


@pytest.mark.parametrize("attr", ["copts", "linkopts", "data"])
def test_selected_rule_keeps_unsupported_attributes(attr):
    rule = gvisor.build_rules(f'cc_binary(name="a", srcs=["a.cc"], {attr}=["value"])', "test")[
        "//test:a"
    ]
    assert rule.unsupported == (attr,)


def test_select_system_defaults_to_linux_define():
    rule = gvisor.build_rules(
        'cc_library(name="a", srcs=select_system(), defines=select_system())', "test"
    )["//test:a"]
    assert rule.sources == ("test/__linux__",)


@pytest.mark.parametrize(
    "text,error",
    [
        (GOOD.replace("E.A", "E.C").replace("Running 1 test", "Running 2 tests"), "header=2"),
        (GOOD.replace("E.A", "E.D"), "extra=['E.D']"),
    ],
)
def test_resumed_attempt_rejects_header_and_extra_cases(tmp_path, monkeypatch, text, error):
    outputs = iter(
        [
            ("E.\n  A\n  B\n  C\n", 0, False),
            (
                "[==========] Running 3 tests from 1 test suite.\n[ RUN      ] E.A\n[       OK ] E.A (0 ms)\n[ RUN      ] E.B\n",
                -11,
                False,
            ),
            (text, 0, False),
        ]
    )
    monkeypatch.setattr(gvisor, "_execute", lambda *args, **kwargs: next(outputs))
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs")
    assert any(error in e for e in run.errors)


def test_large_explicit_resume_filter_only_contains_remaining_cases(tmp_path, monkeypatch):
    names = {f"E.Case{i:04d}" + "x" * 90 for i in range(1000)}
    ordered = sorted(names)
    listing = "E.\n" + "".join("  " + c[2:] + "\n" for c in ordered)
    first = (
        "[==========] Running 1000 tests from 1 test suite.\n"
        + "".join(f"[ RUN      ] {c}\n[       OK ] {c} (0 ms)\n" for c in ordered[:-2])
        + f"[ RUN      ] {ordered[-2]}\n"
    )
    outputs = iter(
        [(listing, 0, False), (first, -11, False), (GOOD.replace("E.A", ordered[-1]), 0, False)]
    )
    calls = []

    def execute(argv, log, timeout_s, **kwargs):
        calls.append(argv)
        return next(outputs)

    monkeypatch.setattr(gvisor, "_execute", execute)
    run = gvisor.run_binary(
        tmp_path, "eventfd_test", logs=tmp_path / "logs", filters={"eventfd_test": names}
    )
    assert not run.errors and not run.not_run
    assert calls[-1][-1] == "--gtest_filter=" + ordered[-1]
    assert all(len(arg.encode()) + 1 <= 131072 for argv in calls for arg in argv)


@pytest.mark.parametrize(
    "annotation",
    [
        "#skip: eventfd_test:E.A",
        "# skip:eventfd_test:E.A",
        "# fail : eventfd_test:E.A",
        "# SKIP: eventfd_test:E.A",
    ],
)
def test_read_native_rejects_mistyped_annotations(tmp_path, annotation):
    baseline = tmp_path / "native.txt"
    baseline.write_text("eventfd_test:E.B\n" + annotation + "\n")
    with pytest.raises(ValueError, match="malformed native annotation"):
        gvisor.read_native(baseline)


def test_native_diff_is_bounded_and_names_downloaded_artifact():
    recorded = {f"eventfd_test:E.Case{i}": "pass" for i in range(10000)}
    summary = gvisor.native_diff(recorded, dict.fromkeys(recorded, "skip"))
    assert "changed=10000" in summary
    assert len(summary.splitlines()) == 13
    assert gvisor.RECORD_COMMAND in summary


@pytest.mark.parametrize(
    "system,probe",
    [
        ("Darwin", "gone"),
        ("Darwin", "zombie"),
        ("Darwin", "live"),
        ("Darwin", "denied"),
        ("Linux", "gone"),
    ],
)
def test_kill_group_only_swallows_darwin_zombie_permission_error(monkeypatch, system, probe):
    calls = []

    def killpg(pgid, sig):
        calls.append((pgid, sig))
        if sig:
            raise PermissionError("denied kill")
        if probe == "gone":
            raise ProcessLookupError
        if probe in {"denied", "zombie"}:
            raise PermissionError("denied probe")

    def getpgid(pid):
        if probe == "zombie":
            raise ProcessLookupError
        return pid

    monkeypatch.setattr(libctest.os, "killpg", killpg)
    monkeypatch.setattr(libctest.os, "getpgid", getpgid)
    monkeypatch.setattr(libctest.os, "uname", lambda: SimpleNamespace(sysname=system))
    if system == "Darwin" and probe in {"gone", "zombie"}:
        libctest.kill_group(123)
    else:
        with pytest.raises(PermissionError):
            libctest.kill_group(123)
    assert len(calls) == (2 if system == "Darwin" else 1)


@pytest.mark.parametrize(
    "field,value",
    [
        ("mode", "batched"),
        ("uid", 1000),
        ("euid", 1000),
        ("capabilities", None),
        ("zig_version", ""),
        ("uname_r", ""),
    ],
)
def test_native_record_requires_per_case_root_identity(tmp_path, field, value):
    for manifest in ("tests.txt", "targets.txt"):
        (tmp_path / manifest).write_text("eventfd_test\n")
    (tmp_path / "not-built.txt").write_text("")
    run = gvisor.Run({"E.A": Outcome.PASS}, listed={"E.A"})
    report = gvisor.report_data(
        ["eventfd_test"], {"eventfd_test": run}, 1, environment={**NATIVE_ENV, field: value}
    )
    (tmp_path / "native-report.json").write_text(json.dumps(report))
    with pytest.raises(ValueError, match="native reference"):
        gvisor.native_outcomes(tmp_path)


def test_build_removes_stale_native_artifacts_before_building(tmp_path):
    tools = tmp_path / "tools"
    tools.mkdir()
    shutil.copyfile(REPO_ROOT / "tools/build-gvisor-tests.sh", tools / "build-gvisor-tests.sh")
    fetch = tools / "fetch-gvisor-tests.sh"
    fetch.write_text("#!/bin/sh\nexit 0\n")
    fetch.chmod(0o755)
    zig = tmp_path / "build/deps/zig/zig"
    zig.parent.mkdir(parents=True)
    zig.write_text("#!/bin/sh\necho 0.15.2\n")
    zig.chmod(0o755)
    for name in ("cmake", "ninja"):
        stub = tools / name
        stub.write_text("#!/bin/sh\nexit 1\n")
        stub.chmod(0o755)
    out = tmp_path / "build/gvisor-tests"
    (out / "native").mkdir(parents=True)
    (out / "native/old.log").write_text("old verdicts")
    (out / "native-report.json").write_text("old reference")
    # No source checkout: extraction fails, but cannot retain an old reference.
    result = subprocess.run(
        ["bash", str(tools / "build-gvisor-tests.sh")],
        env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}"},
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode != 0
    assert not (out / "native").exists()
    assert not (out / "native-report.json").exists()


def test_host_identity_records_linux_capability_masks(tmp_path, monkeypatch):
    status = tmp_path / "status"
    status.write_text(
        "Name:\tpython\nUid:\t0\t0\t0\t0\nCapInh:\t0000000000000000\nCapPrm:\t000001ffffffffff\nCapEff:\t000001ffffffffff\nCapBnd:\t000001ffffffffff\nCapAmb:\t0000000000000000\n"
    )
    monkeypatch.setattr(gvisor, "Path", lambda _: status)
    monkeypatch.setattr(gvisor.os, "getuid", lambda: 0)
    monkeypatch.setattr(gvisor.os, "geteuid", lambda: 0)
    monkeypatch.setattr(gvisor.platform, "release", lambda: "fixture-kernel")
    assert gvisor.host_identity() == {
        "uid": 0,
        "euid": 0,
        "uname_r": "fixture-kernel",
        "capabilities": {
            "CapInh": "0000000000000000",
            "CapPrm": "000001ffffffffff",
            "CapEff": "000001ffffffffff",
            "CapBnd": "000001ffffffffff",
            "CapAmb": "0000000000000000",
        },
    }


@pytest.fixture
def fake_netns(tmp_path, monkeypatch):
    """Exercise the real wrapper on macOS without changing any host networking."""
    tools = tmp_path / "netns-tools"
    tools.mkdir()
    trace, counter = tmp_path / "netns.trace", tmp_path / "netns.counter"
    unshare = tools / "unshare"
    unshare.write_text(
        '#!/bin/sh\n[ "$1" = --net ] && [ "$2" = -- ] || exit 99\n'
        + f'n=0; if [ -f "{counter}" ]; then n=$(cat "{counter}"); fi\n'
        + f'n=$((n + 1)); echo "$n" > "{counter}"; echo "unshare $n" >> "{trace}"\n'
        + 'if [ "$FAKE_NETNS_DENY" = "$n" ]; then\n'
        + 'echo "unshare: unshare failed: Operation not permitted" >&2; exit 1; fi\n'
        + 'export FAKE_NETNS_TOKEN="$n"\nshift 2\nexec "$@"\n'
    )
    unshare.chmod(0o755)
    ip = tools / "ip"
    ip.write_text(
        '#!/bin/sh\n[ -n "$FAKE_NETNS_TOKEN" ] && [ "$*" = "link set dev lo up" ] || exit 99\n'
        + f'echo "ip $FAKE_NETNS_TOKEN $*" >> "{trace}"\n'
        + '[ "$FAKE_LO_FAIL" != 1 ] || { echo "ip: permission denied" >&2; exit 1; }\n'
    )
    ip.chmod(0o755)
    monkeypatch.setenv("PATH", f"{tools}:{os.environ['PATH']}")
    return trace


@pytest.mark.parametrize("deny_retry", [False, True])
def test_per_case_retry_gets_a_new_network_namespace(tmp_path, fake_netns, monkeypatch, deny_retry):
    # gVisor cc5b7474 test/runner/main.go:249-255 and
    # setup_container/setup_container.cc:101-119 isolate networking and raise lo.
    # socket_util.cc:1201-1219 shrinks the namespace's ephemeral range as root.
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    if deny_retry:
        monkeypatch.setenv("FAKE_NETNS_DENY", "2")
    _fake(
        tmp_path,
        LISTING
        + "if [ \"$FAKE_NETNS_TOKEN\" = 1 ]; then\ncat <<'EOF'\n"
        + GOOD.replace("[       OK ]", "[  FAILED  ]")
        + 'EOF\nexit 1\nfi\n[ "$FAKE_NETNS_TOKEN" = 2 ] || exit 99\n'
        + "cat <<'EOF'\n"
        + GOOD
        + "EOF\n",
    )
    run = gvisor.run_binary(
        tmp_path,
        "eventfd_test",
        logs=tmp_path / "logs",
        mode="per-case",
        xfail=gvisor.parse_xfail("eventfd_test:E.A flaky # fixture #454"),
    )
    assert run.results == {"E.A": Outcome.FAIL}
    if deny_retry:
        assert run.final_results == {"E.A": Outcome.ERROR}
        assert (
            "per-case network namespace setup failed: unshare: unshare failed: Operation not permitted"
            in run.errors
        )
    else:
        assert run.final_results == {"E.A": Outcome.PASS} and not run.errors
    assert fake_netns.read_text().splitlines() == [
        "unshare 1",
        "ip 1 link set dev lo up",
        "unshare 2",
        *([] if deny_retry else ["ip 2 link set dev lo up"]),
    ]


@pytest.mark.parametrize("failure", ["unshare", "loopback"])
def test_per_case_network_setup_failure_is_fatal(tmp_path, fake_netns, monkeypatch, failure):
    monkeypatch.setenv("FAKE_NETNS_DENY" if failure == "unshare" else "FAKE_LO_FAIL", "1")
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", mode="per-case")
    assert not run.results and run.not_run == {"E.A"}
    assert len(run.errors) == 1
    assert "per-case network namespace setup failed" in run.errors[0]
    assert (
        "Operation not permitted" if failure == "unshare" else "cannot bring lo up"
    ) in run.errors[0]
    assert not (tmp_path / "logs/eventfd_test.2.log").exists()


def test_batched_mode_does_not_create_network_namespaces(tmp_path, fake_netns, monkeypatch):
    monkeypatch.setenv("FAKE_NETNS_DENY", "1")
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    run = gvisor.run_binary(tmp_path, "eventfd_test", logs=tmp_path / "logs", mode="batched")
    assert not run.errors and run.results == {"E.A": Outcome.PASS}
    assert not fake_netns.exists()


@pytest.mark.parametrize("netns", [None, "shared"])
def test_record_refuses_a_reference_without_per_case_netns(tmp_path, netns):
    for name in ("tests.txt", "targets.txt"):
        (tmp_path / name).write_text("eventfd_test\n")
    (tmp_path / "not-built.txt").write_text("")
    environment = {k: v for k, v in NATIVE_ENV.items() if k != "netns"}
    if netns is not None:
        environment["netns"] = netns
    report = gvisor.report_data(
        ["eventfd_test"],
        {"eventfd_test": gvisor.Run({"E.A": Outcome.PASS}, listed={"E.A"})},
        1,
        environment=environment,
    )
    (tmp_path / "native-report.json").write_text(json.dumps(report))
    baseline = tmp_path / "baseline.txt"
    baseline.write_text("existing baseline\n")
    with pytest.raises(ValueError, match="native reference requires netns=per-case"):
        gvisor.main(["record", str(tmp_path), "--output", str(baseline)])
    assert baseline.read_text() == "existing baseline\n"


def test_native_cli_reports_network_namespace_failure(tmp_path, fake_netns, monkeypatch, capsys):
    monkeypatch.setattr(gvisor.sys, "platform", "linux")
    monkeypatch.setattr(gvisor.platform, "machine", lambda: "x86_64")
    monkeypatch.setenv("FAKE_NETNS_DENY", "1")
    (tmp_path / "tests.txt").write_text("eventfd_test\n")
    (tmp_path / "build-report.json").write_text(json.dumps({"zig_version": "0.15.2"}))
    _fake(tmp_path, LISTING + "cat <<'EOF'\n" + GOOD + "EOF\n")
    assert gvisor.main(["native", "--root", str(tmp_path)]) == 1
    diagnostic = (
        "per-case network namespace setup failed: unshare: unshare failed: Operation not permitted"
    )
    assert diagnostic in capsys.readouterr().err
    report = json.loads((tmp_path / "native-report.json").read_text())
    assert report["binaries"]["eventfd_test"]["errors"] == [diagnostic]
    assert report["environment"]["netns"] == "per-case"
