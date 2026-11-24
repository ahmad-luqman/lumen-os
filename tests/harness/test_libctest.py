"""musl libc-test (#345): its output read into pass, fail and error; the
expected-failure list's format; a run judged against that list; and, where
`make libc-test` has built it, the build's linkage and the native reference
run on CI's Linux host."""

from __future__ import annotations

import os
import platform
import struct
import sys

import pytest
from lumen_harness import BUILD_DIR, libctest
from lumen_harness.libctest import Outcome

# What a run of three tests prints: libc-test's runtest output (t_error lines,
# then `FAIL <path> [<why>]`), each test closed by the driver's EXIT line.
# The message lines are the format libc-test's t_error prints (file:line: ...).
RECORDED = """\
EXIT src/functional/argv-static.exe 0
src/functional/string.c:68: strlen("abc") returned 2, want 3
FAIL src/functional/string-static.exe [status 1]
EXIT src/functional/string-static.exe 1
FAIL src/regression/sigaltstack.exe [signal Segmentation fault]
EXIT src/regression/sigaltstack.exe 1
FAIL src/functional/pthread_cond.exe [timed out]
EXIT src/functional/pthread_cond.exe 1
"""


def test_the_parser_reads_pass_fail_signal_and_timeout():
    run = libctest.parse(RECORDED)
    r = run.results
    assert set(r) == {
        "src/functional/argv-static.exe",
        "src/functional/string-static.exe",
        "src/regression/sigaltstack.exe",
        "src/functional/pthread_cond.exe",
    }
    assert r["src/functional/argv-static.exe"].outcome is Outcome.PASS
    failed = r["src/functional/string-static.exe"]
    assert (failed.outcome, failed.detail) == (Outcome.FAIL, "status 1")
    assert failed.output == ('src/functional/string.c:68: strlen("abc") returned 2, want 3',)
    killed = r["src/regression/sigaltstack.exe"]
    assert (killed.outcome, killed.detail) == (Outcome.FAIL, "signal Segmentation fault")
    timed_out = r["src/functional/pthread_cond.exe"]
    assert (timed_out.outcome, timed_out.detail) == (Outcome.FAIL, "timed out")
    assert run.trailing == []


def test_a_test_s_messages_belong_to_the_next_exit_line_only():
    run = libctest.parse(
        "src/functional/a.c:1: first\nFAIL src/functional/a.exe [status 1]\n"
        "EXIT src/functional/a.exe 1\nEXIT src/functional/b.exe 0\n"
    )
    assert run.results["src/functional/a.exe"].output == ("src/functional/a.c:1: first",)
    assert run.results["src/functional/b.exe"].output == ()


def test_a_leading_dot_slash_and_surrounding_blanks_are_tolerated():
    run = libctest.parse(
        "EXIT ./src/functional/a-static.exe 0\r\n"
        "  FAIL src/functional/b-static.exe [signal Aborted]\r\n EXIT src/functional/b-static.exe 1\n"
    )
    assert run.results["src/functional/a-static.exe"].outcome is Outcome.PASS
    b = run.results["src/functional/b-static.exe"]
    assert (b.outcome, b.detail, b.output) == (Outcome.FAIL, "signal Aborted", ())


@pytest.mark.parametrize(
    "lines, why",
    [
        # runtest's own failures: never a verdict on the test.
        (["FAIL src/functional/a.exe [internal]"], "could not run it"),
        (["FAIL src/functional/a.exe [unknown]"], "could not run it"),
        (["FAIL src/functional/a.exe [internal]", "EXIT255"], "exited 255"),
        # Exit codes that disagree with the output.
        (["FAIL src/functional/a.exe [status 1]", "EXIT0"], "exited 0 after FAIL"),
        (["EXIT1"], "without a FAIL line"),
        (["EXIT127"], "without a FAIL line"),
        (["FAIL src/functional/a.exe [status 3]", "EXIT2"], "exited 2 after FAIL"),
        (["EXIT300"], "not an exit status"),
        # A FAIL line naming another test, or two of them.
        (["FAIL src/functional/b.exe [status 1]"], "FAIL line for src/functional/b.exe"),
        (
            ["FAIL src/functional/a.exe [status 1]", "FAIL src/functional/a.exe [status 1]"],
            "2 FAIL lines",
        ),
        # A bracket runtest doesn't print.
        (["FAIL src/functional/a.exe [status 0]"], "could not run it"),
        (["FAIL src/functional/a.exe [exploded]"], "could not run it"),
    ],
)
def test_runner_trouble_is_an_error_not_a_failure(lines, why):
    code = "1"
    body = []
    for line in lines:
        if line.startswith("EXIT"):
            code = line.removeprefix("EXIT")
        else:
            body.append(line)
    text = "\n".join([*body, f"EXIT src/functional/a.exe {code}"]) + "\n"
    r = libctest.parse(text).results["src/functional/a.exe"]
    assert r.outcome is Outcome.ERROR, r
    assert why in r.detail


def test_a_truncated_run_leaves_its_last_test_not_run():
    # The run stopped inside the second test: its FAIL line came, its EXIT didn't.
    run = libctest.parse(
        "EXIT src/functional/a-static.exe 0\nFAIL src/functional/b-static.exe [status 1]\n"
    )
    assert set(run.results) == {"src/functional/a-static.exe"}
    assert run.trailing == ["FAIL src/functional/b-static.exe [status 1]"]
    v = libctest.compare(run, {}, ["src/functional/a-static.exe", "src/functional/b-static.exe"])
    assert not v.ok
    assert v.not_run == ["src/functional/b-static.exe"]


def test_an_empty_run_passes_nothing():
    v = libctest.compare(libctest.parse(""), {}, ["src/functional/a-static.exe"])
    assert not v.ok and v.not_run == ["src/functional/a-static.exe"] and v.passed == []


@pytest.mark.parametrize(
    "text",
    [
        "EXIT src/functional/a.exe 0\nEXIT src/functional/a.exe 0\n",
        "EXIT /abs/src/functional/a.exe 0\n",
        "EXIT src/math/sin.exe 0\n",
        "EXIT src/functional/a 0\n",
    ],
)
def test_an_exit_line_names_each_test_once_by_its_path(text):
    with pytest.raises(libctest.ParseError):
        libctest.parse(text)


# The expected-failure list's format.

GOOD_XFAIL = """\
# a comment

src/functional/pthread_cancel-points-static.exe # cancellation points not delivered #400
   src/regression/raise-race.exe   #  SIGCHLD lost; see #12 too   #401
"""


def test_xfail_entries_carry_a_reason_and_an_issue():
    entries = libctest.parse_xfail(GOOD_XFAIL)
    assert set(entries) == {
        "src/functional/pthread_cancel-points-static.exe",
        "src/regression/raise-race.exe",
    }
    a = entries["src/functional/pthread_cancel-points-static.exe"]
    assert (a.reason, a.issue, a.line) == ("cancellation points not delivered", 400, 3)
    b = entries["src/regression/raise-race.exe"]
    assert (b.reason, b.issue, b.line) == ("SIGCHLD lost; see #12 too", 401, 4)


@pytest.mark.parametrize(
    "line",
    [
        "src/functional/a-static.exe",  # no reason, no issue
        "src/functional/a-static.exe # fails",  # no issue
        "src/functional/a-static.exe # #400",  # no reason
        "src/functional/a-static.exe #400",  # no reason
        "src/functional/a-static.exe # fails #0",  # not an issue number
        "src/functional/a-static.exe # fails 400",  # issue without '#'
        "src/functional/a-static.exe # fails #400 later",  # the issue comes last
        "src/functional/a-static.exe fails # #400",  # text before the comment
    ],
)
def test_an_xfail_entry_without_a_reason_and_an_issue_is_refused(line):
    with pytest.raises(libctest.XfailError, match="line 2"):
        libctest.parse_xfail(f"# header\n{line}\n")


@pytest.mark.parametrize(
    "path",
    [
        "src/math/sin.exe",
        "src/api/main.exe",
        "functional/a-static.exe",
        "/build/libc-test/src/functional/a-static.exe",
        "src/functional/a-static",
        "src/functional/a.c",
        "src/functional/*.exe",
    ],
)
def test_an_xfail_entry_names_a_built_test_path(path):
    with pytest.raises(libctest.XfailError, match="not a test path"):
        libctest.parse_xfail(f"{path} # fails #400\n")


def test_an_xfail_entry_is_listed_once():
    text = "src/functional/a.exe # one #1\n\nsrc/functional/a.exe # two #2\n"
    with pytest.raises(libctest.XfailError, match="line 3: .* twice .*line 1"):
        libctest.parse_xfail(text)


def test_the_checked_in_xfail_list_is_well_formed():
    libctest.load_xfail()


# A run judged against the list.

A = "src/functional/a-static.exe"
B = "src/functional/b-static.exe"
C = "src/regression/c.exe"


def _xfail(*tests: str) -> dict[str, libctest.Xfail]:
    return libctest.parse_xfail("".join(f"{t} # known #{i + 1}\n" for i, t in enumerate(tests)))


def _run(**outcomes: str) -> libctest.Run:
    """A run from {test letter: "pass" | "fail" | "signal" | "timeout" | "internal"}."""
    lines = []
    for letter, outcome in outcomes.items():
        test = {"A": A, "B": B, "C": C}[letter]
        bracket = {
            "fail": "status 1",
            "signal": "signal Aborted",
            "timeout": "timed out",
            "internal": "internal",
        }.get(outcome)
        if bracket is None:
            lines.append(f"EXIT {test} 0")
        else:
            lines += [
                f"FAIL {test} [{bracket}]",
                f"EXIT {test} {255 if outcome == 'internal' else 1}",
            ]
    return libctest.parse("\n".join(lines) + "\n")


def test_all_passing_and_nothing_listed_passes():
    v = libctest.compare(_run(A="pass", B="pass", C="pass"), {}, [A, B, C])
    assert v.ok, v.summary()
    assert v.passed == [A, B, C]


def test_an_unexpected_failure_fails_the_comparison():
    v = libctest.compare(_run(A="pass", B="fail", C="pass"), {}, [A, B, C])
    assert not v.ok
    assert [r.test for r in v.unexpected_failures] == [B]
    assert f"FAIL (not in the expected-failure list) {B} [status 1]" in v.summary()


def test_a_listed_test_that_passes_fails_the_comparison():
    v = libctest.compare(_run(A="pass", B="pass", C="pass"), _xfail(B), [A, B, C])
    assert not v.ok
    assert [r.test for r in v.unexpected_passes] == [B]
    assert "remove its entry" in v.summary()


@pytest.mark.parametrize("how", ["fail", "signal", "timeout"])
def test_a_listed_failure_passes_the_comparison(how):
    v = libctest.compare(_run(A="pass", B=how, C="pass"), _xfail(B), [A, B, C])
    assert v.ok, v.summary()
    assert [r.test for r in v.expected_failures] == [B]
    assert v.passed == [A, C]


def test_an_error_fails_the_comparison_even_when_listed():
    v = libctest.compare(_run(A="pass", B="internal", C="pass"), _xfail(B), [A, B, C])
    assert not v.ok
    assert [r.test for r in v.errors] == [B]


def test_a_listed_test_that_was_not_built_fails_the_comparison():
    v = libctest.compare(_run(A="pass", B="pass"), _xfail(C), [A, B])
    assert not v.ok
    assert [x.test for x in v.stale] == [C]


def test_a_result_for_a_test_that_was_not_built_fails_the_comparison():
    v = libctest.compare(_run(A="pass", B="pass", C="pass"), {}, [A, B])
    assert not v.ok
    assert v.unknown == [C]


# The ELF check behind the build's linkage check.


def _elf(
    interp: bytes | None,
    *,
    stack: int | None = None,
    machine: int = 62,
    tail: bytes = b"",
    sections: tuple[str, ...] | None = None,
) -> bytes:
    """A minimal ELF64 little-endian file: PT_INTERP naming `interp` (or a
    PT_LOAD for a static file), then PT_GNU_STACK with `stack` as its size if
    given, then `tail`; with `sections`, a section header table naming them
    (after the null section, and before `.shstrtab`)."""
    phoff, phentsize, shentsize = 64, 56, 64
    phnum = 1 if stack is None else 2
    payload = phoff + phnum * phentsize
    data = interp + b"\0" if interp is not None else b""
    ptype = 3 if interp is not None else 1
    phdrs = struct.pack("<IIQQQQQQ", ptype, 4, payload, 0, 0, len(data), len(data), 1)
    if stack is not None:
        phdrs += struct.pack("<IIQQQQQQ", 0x6474E551, 6, 0, 0, 0, 0, stack, 16)
    body = phdrs + data + tail
    shoff = shnum = shstrndx = 0
    if sections is not None:
        names = ["", *sections, ".shstrtab"]
        strtab, offsets = b"", []
        for name in names:
            offsets.append(len(strtab))
            strtab += name.encode() + b"\0"
        strtab_at = phoff + len(body)
        body += strtab
        shoff, shnum, shstrndx = phoff + len(body), len(names), len(names) - 1
        for i, off in enumerate(offsets):
            at, size = (strtab_at, len(strtab)) if i == shstrndx else (0, 0)
            body += struct.pack(
                "<IIQQQQIIQQ", off, 3 if i == shstrndx else 1, 0, 0, at, size, 0, 0, 1, 0
            )
    header = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header += struct.pack(
        "<HHIQQQIHHHHHH",
        2,
        machine,
        1,
        0,
        phoff,
        shoff,
        0,
        64,
        phentsize,
        phnum,
        shentsize,
        shnum,
        shstrndx,
    )
    return header + body


def test_read_elf_reads_the_interpreter_and_the_stack_size(tmp_path):
    dyn = tmp_path / "dyn"
    dyn.write_bytes(_elf(b"/lib/ld-musl-x86_64.so.1", stack=0))
    static = tmp_path / "static"
    static.write_bytes(_elf(None, stack=16 << 20))
    bare = tmp_path / "bare"
    bare.write_bytes(_elf(None))
    named = tmp_path / "named"
    named.write_bytes(_elf(None, stack=0, sections=(".text", ".debug_line")))
    assert libctest.read_elf(dyn) == libctest.Elf("/lib/ld-musl-x86_64.so.1", 0)
    assert libctest.read_elf(static) == libctest.Elf(None, 16 << 20)
    assert libctest.read_elf(bare) == libctest.Elf(None, 0)
    assert libctest.read_elf(named) == libctest.Elf(
        None, 0, ("", ".text", ".debug_line", ".shstrtab")
    )


def test_read_elf_refuses_other_machines_and_non_elf_files(tmp_path):
    arm = tmp_path / "arm"
    arm.write_bytes(_elf(None, machine=183))
    text = tmp_path / "text"
    text.write_bytes(b"#!/bin/sh\n" + bytes(64))
    for path in (arm, text):
        with pytest.raises(ValueError):
            libctest.read_elf(path)


def test_linkage_problems_name_each_binary_built_unlike_libc_tests_build(tmp_path):
    loader = b"/lib/ld-musl-x86_64.so.1"
    binaries = {
        libctest.RUNTEST: _elf(None, stack=0),
        "src/functional/good-static.exe": _elf(None, stack=0, sections=(".text", ".comment")),
        "src/functional/good.exe": _elf(loader, stack=0),
        "src/functional/a-static.exe": _elf(loader, stack=0),
        "src/functional/a.exe": _elf(None, stack=0),
        "src/functional/b.exe": _elf(b"/lib/ld-linux-x86-64.so.2", stack=0),
        "src/regression/c-static.exe": _elf(None, stack=16 << 20),
        "src/regression/d.exe": _elf(
            loader, tail=b"\0 integer overflow:  +  cannot be represented in type \0"
        ),
        "src/regression/e-static.exe": _elf(None, sections=(".text", ".debug_info", ".symtab")),
    }
    for rel, data in binaries.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_bytes(data)
    tests = [t for t in binaries if t != libctest.RUNTEST] + ["src/regression/missing.exe"]
    (tmp_path / libctest.TESTS_FILE).write_text("\n".join(tests) + "\n")
    problems = libctest.linkage_problems(tmp_path)
    assert [p.split(": ", 1)[0] for p in problems] == [
        "src/functional/a-static.exe",
        "src/functional/a.exe",
        "src/functional/b.exe",
        "src/regression/c-static.exe",
        "src/regression/d.exe",
        "src/regression/e-static.exe",
        "src/regression/missing.exe",
    ], problems
    assert "static by name" in problems[0]
    assert "dynamic by name" in problems[1] and "dynamic by name" in problems[2]
    assert "16777216-byte stack" in problems[3]
    assert "UBSan" in problems[4]
    assert "keeps debug sections (.debug_info)" in problems[5]


# The native runner's driver, over a stand-in runtest (a shell script) that
# logs its arguments and where it ran, and answers as libc-test's would.

FAKE_RUNTEST = """\
#!/bin/sh
last=""
for a in "$@"; do last=$a; done
[ -L src ] && [ -e "$last" ] && where=ok || where=bad
echo "$where $*" >> {log}
case "$last" in
  *pass*) exit 0 ;;
  *fail*) echo "$last.c:1: wrong"; echo "FAIL $last [status 1]"; exit 1 ;;
  *) exit 3 ;;
esac
"""


def test_run_native_wraps_only_dynamic_tests_and_closes_each_with_its_exit_code(tmp_path):
    root, log = tmp_path / "build", tmp_path / "args.log"
    runtest = root / libctest.RUNTEST
    runtest.parent.mkdir(parents=True)
    runtest.write_text(FAKE_RUNTEST.format(log=log))
    runtest.chmod(0o755)
    tests = [
        "src/functional/pass-static.exe",
        "src/functional/fail.exe",
        "src/regression/broken-static.exe",
    ]
    for t in tests:
        (root / t).parent.mkdir(parents=True, exist_ok=True)
        (root / t).write_bytes(b"")
    loader = tmp_path / "ld-musl-x86_64.so.1"

    transcript = libctest.run_native(root, tests, loader=loader, timeout_s=7)

    assert log.read_text().splitlines() == [
        "ok -t 7 src/functional/pass-static.exe",
        f"ok -t 7 -w {loader} src/functional/fail.exe",
        "ok -t 7 src/regression/broken-static.exe",
    ]
    assert [line for line in transcript.splitlines() if line.startswith("EXIT")] == [
        "EXIT src/functional/pass-static.exe 0",
        "EXIT src/functional/fail.exe 1",
        "EXIT src/regression/broken-static.exe 3",
    ]
    r = libctest.parse(transcript).results
    assert r["src/functional/pass-static.exe"].outcome is Outcome.PASS
    fail = r["src/functional/fail.exe"]
    assert (fail.outcome, fail.detail, fail.output) == (
        Outcome.FAIL,
        "status 1",
        ("src/functional/fail.exe.c:1: wrong",),
    )
    assert r["src/regression/broken-static.exe"].outcome is Outcome.ERROR


def test_run_native_refuses_a_dynamic_test_without_a_loader(tmp_path):
    (tmp_path / "src").mkdir()
    with pytest.raises(ValueError, match="no musl loader"):
        libctest.run_native(tmp_path, ["src/functional/a.exe"], loader=None)


# The build itself, where `make libc-test` made it (CI's job builds it before
# the harness tests; there a missing build fails rather than skips, so the
# feature can't pass with these unchecked).

BUILD = libctest.BUILD
ON_CI_LINUX = bool(os.environ.get("CI")) and sys.platform == "linux"

# The tests and libraries the pinned zig and libc-test commit don't build, as
# tools/build-libc-test.sh writes them to not-built.txt (path, then `# why`).
NOT_BUILT: list[str] = []


def _require_build() -> None:
    if (BUILD / libctest.TESTS_FILE).is_file():
        return
    if ON_CI_LINUX:
        pytest.fail("build/libc-test/tests.txt is missing: CI runs `make libc-test` first")
    pytest.skip("no build/libc-test (run `make libc-test`)")


def test_the_build_left_out_only_the_recorded_binaries():
    _require_build()
    got = (BUILD / "not-built.txt").read_text().splitlines()
    assert [line.split(" # ")[0] for line in got] == NOT_BUILT, "\n".join(got)


def test_every_built_binary_is_linked_as_its_name_says():
    _require_build()
    assert libctest.read_tests(), "tests.txt is empty"
    assert libctest.linkage_problems() == []


# The tests that pass when run natively on CI's Linux host (x86-64, its own
# kernel; dynamic tests through Alpine 3.21's musl loader), at the pinned zig
# and libc-test. The rest of tests.txt fails natively too, so its failures
# in Lumen aren't Lumen's own. Re-record when a pin changes: a mismatch's
# message lists how each other test failed and prints the set to paste here.
NATIVE_PASS: frozenset[str] = frozenset()


def test_the_native_reference_run_passes_the_recorded_set():
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "AMD64"):
        pytest.skip(
            f"the native reference run needs an x86-64 Linux host (CI's), not "
            f"{sys.platform} {platform.machine()}"
        )
    _require_build()
    loader = BUILD_DIR / "deps" / "alpine-minirootfs" / "lib" / "ld-musl-x86_64.so.1"
    if not loader.is_file():
        if ON_CI_LINUX:
            pytest.fail(f"{loader} is missing: CI runs `make deps` first")
        pytest.skip(f"no {loader} for the dynamic tests (run `make deps`)")
    tests = libctest.read_tests()
    transcript = libctest.run_native(BUILD, tests, loader=loader)
    run = libctest.parse(transcript)
    assert run.results.keys() == set(tests), transcript
    # An error is runtest failing, not a test: never recorded as environmental.
    errors = [r for r in run.results.values() if r.outcome is Outcome.ERROR]
    assert not errors, "\n".join(f"  {r.test}: {r.detail} {' | '.join(r.output)}" for r in errors)
    passed = {t for t, r in run.results.items() if r.outcome is Outcome.PASS}
    newly_passing = sorted(passed - NATIVE_PASS)
    newly_failing = sorted(NATIVE_PASS - passed)
    # Every test that didn't pass, and how: the environmental failures the PR
    # records alongside the set.
    not_passing = "\n".join(
        f"  {r.test}: {r.outcome.value} [{r.detail}] {' | '.join(r.output)}"
        for r in sorted(run.results.values(), key=lambda r: r.test)
        if r.outcome is not Outcome.PASS
    )
    recorded = "".join(f'        "{t}",\n' for t in sorted(passed))
    assert passed == NATIVE_PASS, (
        f"passing natively now, not recorded: {newly_passing}\n"
        f"recorded as passing, now not: {newly_failing}\n"
        f"not passing natively ({len(tests) - len(passed)} of {len(tests)}):\n{not_passing}\n"
        f"to re-record:\nNATIVE_PASS: frozenset[str] = frozenset(\n    {{\n{recorded}    }}\n)"
    )
