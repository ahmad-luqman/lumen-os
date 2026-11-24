"""musl libc-test (#345): its output read into pass, fail and error; the
expected-failure list's format; a run judged against that list; and, where
`make libc-test` has built it, the build's linkage and the native reference
run on CI's Linux host."""

from __future__ import annotations

import os
import platform
import struct
import subprocess
import sys
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, libctest
from lumen_harness.libctest import Outcome

# A run of four tests, hand-written in runtest's format: the test's t_error
# lines, then `FAIL <path> [<why>]`, each test closed by the driver's EXIT
# line. The t_error line is a real one, from CI's native run of the pinned
# libc-test (mntent-static fails natively too).
SAMPLE = """\
EXIT src/functional/argv-static.exe 0
src/functional/mntent.c:45: !strcmp(m->mnt_opts, "defaults") failed
FAIL src/functional/mntent-static.exe [status 1]
EXIT src/functional/mntent-static.exe 1
FAIL src/regression/sigaltstack.exe [signal Segmentation fault]
EXIT src/regression/sigaltstack.exe 1
FAIL src/functional/pthread_cond.exe [timed out]
EXIT src/functional/pthread_cond.exe 1
"""


def test_the_parser_reads_pass_fail_signal_and_timeout():
    run = libctest.parse(SAMPLE)
    r = run.results
    assert set(r) == {
        "src/functional/argv-static.exe",
        "src/functional/mntent-static.exe",
        "src/regression/sigaltstack.exe",
        "src/functional/pthread_cond.exe",
    }
    assert r["src/functional/argv-static.exe"].outcome is Outcome.PASS
    failed = r["src/functional/mntent-static.exe"]
    assert (failed.outcome, failed.detail) == (Outcome.FAIL, "status 1")
    assert failed.output == ('src/functional/mntent.c:45: !strcmp(m->mnt_opts, "defaults") failed',)
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
        # runtest's own error lines (strings of the built runtest-static.exe,
        # t_error's `src/common/runtest.c:<line>: ` first), each with the
        # ending runtest gives it: the child's execv failing (the test, or
        # the loader as its wrapper) ends `[status 1]`; a sigtimedwait
        # failure kills the test, `[signal Killed]`; a kill failure, anything.
        (
            [
                "src/common/runtest.c:29: src/functional/a.exe exec failed: Exec format error",
                "FAIL src/functional/a.exe [status 1]",
            ],
            "runner trouble",
        ),
        (
            [
                (
                    "src/common/runtest.c:29: /lib/ld-musl-x86_64.so.1 exec failed: "
                    "No such file or directory"
                ),
                "FAIL src/functional/a.exe [status 1]",
            ],
            "runner trouble",
        ),
        (
            [
                (
                    "src/common/runtest.c:80: src/functional/a.exe sigtimedwait failed: "
                    "Interrupted system call"
                ),
                "FAIL src/functional/a.exe [signal Killed]",
            ],
            "runner trouble",
        ),
        (
            [
                "src/common/runtest.c:82: src/functional/a.exe kill failed: No such process",
                "FAIL src/functional/a.exe [timed out]",
            ],
            "runner trouble",
        ),
        (
            [
                "src/common/runtest.c:82: src/functional/a.exe kill failed: No such process",
                "FAIL src/functional/a.exe [status 1]",
            ],
            "runner trouble",
        ),
        # fork and waitpid failures end `FAIL <path> [internal]` and exit 255.
        (
            [
                (
                    "src/common/runtest.c:72: src/functional/a.exe fork failed: "
                    "Resource temporarily unavailable"
                ),
                "FAIL src/functional/a.exe [internal]",
                "EXIT255",
            ],
            "exited 255 after FAIL [internal]",
        ),
        (
            [
                "src/common/runtest.c:85: src/functional/a.exe waitpid failed: No child processes",
                "FAIL src/functional/a.exe [internal]",
                "EXIT255",
            ],
            "exited 255 after FAIL [internal]",
        ),
        # musl 1.2.5's loader, as the wrapper, failing to start the test and
        # exiting 1 (ldso/dynlink.c's own text).
        (
            [
                (
                    "/lib/ld-musl-x86_64.so.1: cannot load src/functional/a.exe: "
                    "No such file or directory"
                ),
                "FAIL src/functional/a.exe [status 1]",
            ],
            "runner trouble",
        ),
        (
            [
                "/lib/ld-musl-x86_64.so.1: src/functional/a.exe: Not a valid dynamic program",
                "FAIL src/functional/a.exe [status 1]",
            ],
            "runner trouble",
        ),
        # The program never ran: not executable, or musl's loader failing to
        # load a library (`_exit(127)`, its message as dynlink.c prints it).
        (["FAIL src/functional/a.exe [status 126]"], "never ran"),
        (
            [
                (
                    "Error loading shared library src/functional/a_dso.so: "
                    "No such file or directory (needed by src/functional/a.exe)"
                ),
                "FAIL src/functional/a.exe [status 127]",
            ],
            "never ran",
        ),
    ],
)
def test_runner_trouble_is_an_error_not_a_failure(lines, why):
    """Each of these as an ERROR, which no expected-failure entry covers."""
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


@pytest.mark.parametrize(
    "line",
    [
        # The test's own t_error lines name its file, not runtest's.
        "src/functional/spawn.c:40: /bin/true exec failed: No such file or directory",
        "src/regression/x.c:12: child kill failed: No such process",
        # A loader message about another program is the test's business.
        "/lib/ld-musl-x86_64.so.1: cannot load src/functional/other.exe: No such file",
        "/lib/ld-musl-x86_64.so.1: ./helper: Not a valid dynamic program",
    ],
)
def test_a_test_s_own_lines_about_failed_programs_are_still_its_verdict(line):
    run = libctest.parse(
        f"{line}\nFAIL src/functional/spawn.exe [status 1]\nEXIT src/functional/spawn.exe 1\n"
    )
    r = run.results["src/functional/spawn.exe"]
    assert (r.outcome, r.detail) == (Outcome.FAIL, "status 1")


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
        "src/functional/a-static.exe # fails#400",  # no space before the issue
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


def test_the_summary_names_every_kind_of_trouble():
    run = libctest.parse(
        "src/regression/c.c:9: lost a signal\n"
        "FAIL src/regression/c.exe [internal]\nEXIT src/regression/c.exe 255\n"
        "EXIT src/functional/z-static.exe 0\n"
        "src/functional/b.c:3: started, never ended\n"
    )
    v = libctest.compare(run, _xfail("src/regression/gone.exe"), [A, B, C])
    assert not v.ok
    lines = v.summary().splitlines()
    assert lines[0] == (
        "0 passed, 0 failed as expected, 0 failed unexpectedly, 0 passed unexpectedly, "
        "1 errors, 2 not run"
    )
    assert lines[1:] == [
        f"  ERROR {C}: runtest exited 255 after FAIL [internal]",
        "    src/regression/c.c:9: lost a signal",
        f"  NOT RUN {A}",
        f"  NOT RUN {B}",
        "  RAN BUT NOT A BUILT TEST src/functional/z-static.exe",
        "  LISTED BUT NOT BUILT (line 1 of the expected-failure list) src/regression/gone.exe",
        "  OUTPUT AFTER THE LAST EXIT LINE (a test that never ended)",
        "    src/functional/b.c:3: started, never ended",
    ]


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
    elf32 = tmp_path / "elf32"
    elf32.write_bytes(_patched(_elf(None), 4, b"\x01"))
    big = tmp_path / "big"
    big.write_bytes(_patched(_elf(None), 5, b"\x02"))
    for path in (arm, text, elf32, big):
        with pytest.raises(ValueError):
            libctest.read_elf(path)


def _patched(data: bytes, at: int, new: bytes) -> bytes:
    return data[:at] + new + data[at + len(new) :]


def test_read_elf_refuses_a_section_name_table_past_the_sections(tmp_path):
    data = _elf(None, sections=(".text",))
    (shnum,) = struct.unpack_from("<H", data, 60)
    bad = tmp_path / "bad"
    bad.write_bytes(_patched(data, 62, struct.pack("<H", shnum)))  # e_shstrndx == e_shnum
    with pytest.raises(ValueError, match="e_shstrndx"):
        libctest.read_elf(bad)


def test_linkage_problems_name_each_binary_built_unlike_libc_tests_build(tmp_path):
    loader = b"/lib/ld-musl-x86_64.so.1"
    binaries = {
        # The runner is checked as a test is: here every way at once.
        libctest.RUNTEST: _elf(loader, stack=16 << 20, sections=(".text", ".debug_info")),
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
    # The tests' libraries: no interpreter (they're libraries), and no
    # stack-size check, but the runtime and debug checks apply.
    libraries = {
        "src/functional/good_dso.so": _elf(None, stack=16 << 20, sections=(".text",)),
        "src/functional/debug_dso.so": _elf(None, sections=(".text", ".debug_line")),
        "src/regression/ubsan_dso.so": _elf(None, tail=b"\0 cannot be represented in type \0"),
    }
    for rel, data in {**binaries, **libraries}.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_bytes(data)
    tests = [t for t in binaries if t != libctest.RUNTEST] + ["src/regression/missing.exe"]
    (tmp_path / libctest.TESTS_FILE).write_text("\n".join(tests) + "\n")
    problems = libctest.linkage_problems(tmp_path)
    assert [p.split(": ", 1)[0] for p in problems] == [
        libctest.RUNTEST,
        libctest.RUNTEST,
        libctest.RUNTEST,
        "src/functional/a-static.exe",
        "src/functional/a.exe",
        "src/functional/b.exe",
        "src/regression/c-static.exe",
        "src/regression/d.exe",
        "src/regression/e-static.exe",
        "src/regression/missing.exe",
        "src/functional/debug_dso.so",
        "src/regression/ubsan_dso.so",
    ], problems
    assert "keeps debug sections (.debug_info)" in problems[0]
    assert "16777216-byte stack" in problems[1]
    assert "static by name" in problems[2]
    assert "static by name" in problems[3]
    assert "dynamic by name" in problems[4] and "dynamic by name" in problems[5]
    assert "16777216-byte stack" in problems[6]
    assert "UBSan" in problems[7]
    assert "keeps debug sections (.debug_info)" in problems[8]
    assert "keeps debug sections (.debug_line)" in problems[10]
    assert "UBSan" in problems[11]


def _tree(root, binaries: dict[str, bytes]) -> None:
    for rel, data in binaries.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_bytes(data)
    tests = [t for t in binaries if t != libctest.RUNTEST]
    (root / libctest.TESTS_FILE).write_text("".join(f"{t}\n" for t in tests))


def test_main_s_linkage_command_exits_by_what_it_found(tmp_path, capsys):
    good, bad = tmp_path / "good", tmp_path / "bad"
    _tree(good, {libctest.RUNTEST: _elf(None), "src/functional/a-static.exe": _elf(None)})
    _tree(bad, {libctest.RUNTEST: _elf(None), "src/functional/a.exe": _elf(None)})
    assert libctest.main(["linkage", str(good)]) == 0
    assert capsys.readouterr().err == ""
    assert libctest.main(["linkage", str(bad)]) == 1
    assert "libc-test linkage: src/functional/a.exe: dynamic by name" in capsys.readouterr().err
    assert libctest.main([]) == 2
    assert libctest.main(["link", str(good)]) == 2
    assert "usage:" in capsys.readouterr().err


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
  *fail*) echo "$last.c:1: wrong"; echo "$last.c:2: on stderr" >&2; echo "FAIL $last [status 1]"; exit 1 ;;
  *segv*) ulimit -c 0; kill -SEGV $$ ;;
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
        "src/regression/segv-static.exe",
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
        "ok -t 7 src/regression/segv-static.exe",
    ]
    assert [line for line in transcript.splitlines() if line.startswith("EXIT")] == [
        "EXIT src/functional/pass-static.exe 0",
        "EXIT src/functional/fail.exe 1",
        "EXIT src/regression/broken-static.exe 3",
        # runtest itself killed by SIGSEGV: 128 + 11, as a shell reports it.
        "EXIT src/regression/segv-static.exe 139",
    ]
    r = libctest.parse(transcript).results
    assert r["src/functional/pass-static.exe"].outcome is Outcome.PASS
    fail = r["src/functional/fail.exe"]
    assert (fail.outcome, fail.detail, fail.output) == (
        Outcome.FAIL,
        "status 1",
        # stderr reaches the transcript too.
        ("src/functional/fail.exe.c:1: wrong", "src/functional/fail.exe.c:2: on stderr"),
    )
    assert r["src/regression/broken-static.exe"].outcome is Outcome.ERROR
    segv = r["src/regression/segv-static.exe"]
    assert (segv.outcome, segv.detail) == (Outcome.ERROR, "runtest exited 139 without a FAIL line")


def test_run_native_counts_a_runtest_that_never_ends_as_an_error(tmp_path, monkeypatch):
    (tmp_path / "src").mkdir()
    calls = []

    def hang(argv, *, timeout, **kwargs):
        calls.append(timeout)
        # What runtest printed before it was killed comes with the exception.
        raise subprocess.TimeoutExpired(argv, timeout, output=b"src/functional/a.c:3: stuck\n")

    monkeypatch.setattr(libctest.subprocess, "run", hang)
    transcript = libctest.run_native(
        tmp_path, ["src/functional/a-static.exe"], loader=None, timeout_s=5, grace_s=2
    )
    assert calls == [7]
    assert transcript == (
        "src/functional/a.c:3: stuck\n"
        "runtest did not end within 7 s\nEXIT src/functional/a-static.exe 124\n"
    )
    r = libctest.parse(transcript).results["src/functional/a-static.exe"]
    assert r.outcome is Outcome.ERROR
    assert r.output == ("src/functional/a.c:3: stuck", "runtest did not end within 7 s")


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
# and libc-test: 274 of the 284, the same in two CI runs. The other 10 fail
# natively too, all `[status 1]`, static and dynamic alike, in libc code:
# mntent, strptime, wordexp, regex-backref-oob and regexec-backref (most
# likely the libc-test tip checks musl fixes newer than 1.2.5). So one of
# those failing in Lumen is no evidence of a Lumen bug. The set includes
# timing-sensitive tests (pthread_cond-smasher, raise-race, pthread_cancel*,
# sem_close-unmap, pthread_once-deadlock) under a 30 s per-test timeout, so a
# badly stalled runner or a new runner image can upset it. Re-record when a
# pin changes: a mismatch's message lists how each other test failed and
# prints the set to paste here.
NATIVE_PASS: frozenset[str] = frozenset(
    {
        "src/functional/argv-static.exe",
        "src/functional/argv.exe",
        "src/functional/basename-static.exe",
        "src/functional/basename.exe",
        "src/functional/clocale_mbfuncs-static.exe",
        "src/functional/clocale_mbfuncs.exe",
        "src/functional/clock_gettime-static.exe",
        "src/functional/clock_gettime.exe",
        "src/functional/crypt-static.exe",
        "src/functional/crypt.exe",
        "src/functional/dirname-static.exe",
        "src/functional/dirname.exe",
        "src/functional/dlopen.exe",
        "src/functional/env-static.exe",
        "src/functional/env.exe",
        "src/functional/fcntl-static.exe",
        "src/functional/fcntl.exe",
        "src/functional/fdopen-static.exe",
        "src/functional/fdopen.exe",
        "src/functional/fnmatch-static.exe",
        "src/functional/fnmatch.exe",
        "src/functional/fscanf-static.exe",
        "src/functional/fscanf.exe",
        "src/functional/fwscanf-static.exe",
        "src/functional/fwscanf.exe",
        "src/functional/iconv_open-static.exe",
        "src/functional/iconv_open.exe",
        "src/functional/inet_pton-static.exe",
        "src/functional/inet_pton.exe",
        "src/functional/ipc_msg-static.exe",
        "src/functional/ipc_msg.exe",
        "src/functional/ipc_sem-static.exe",
        "src/functional/ipc_sem.exe",
        "src/functional/ipc_shm-static.exe",
        "src/functional/ipc_shm.exe",
        "src/functional/mbc-static.exe",
        "src/functional/mbc.exe",
        "src/functional/memstream-static.exe",
        "src/functional/memstream.exe",
        "src/functional/popen-static.exe",
        "src/functional/popen.exe",
        "src/functional/pthread_cancel-points-static.exe",
        "src/functional/pthread_cancel-points.exe",
        "src/functional/pthread_cancel-static.exe",
        "src/functional/pthread_cancel.exe",
        "src/functional/pthread_cond-static.exe",
        "src/functional/pthread_cond.exe",
        "src/functional/pthread_mutex-static.exe",
        "src/functional/pthread_mutex.exe",
        "src/functional/pthread_mutex_pi-static.exe",
        "src/functional/pthread_mutex_pi.exe",
        "src/functional/pthread_robust-static.exe",
        "src/functional/pthread_robust.exe",
        "src/functional/pthread_tsd-static.exe",
        "src/functional/pthread_tsd.exe",
        "src/functional/qsort-static.exe",
        "src/functional/qsort.exe",
        "src/functional/random-static.exe",
        "src/functional/random.exe",
        "src/functional/search_hsearch-static.exe",
        "src/functional/search_hsearch.exe",
        "src/functional/search_insque-static.exe",
        "src/functional/search_insque.exe",
        "src/functional/search_lsearch-static.exe",
        "src/functional/search_lsearch.exe",
        "src/functional/search_tsearch-static.exe",
        "src/functional/search_tsearch.exe",
        "src/functional/sem_init-static.exe",
        "src/functional/sem_init.exe",
        "src/functional/sem_open-static.exe",
        "src/functional/sem_open.exe",
        "src/functional/setjmp-static.exe",
        "src/functional/setjmp.exe",
        "src/functional/snprintf-static.exe",
        "src/functional/snprintf.exe",
        "src/functional/socket-static.exe",
        "src/functional/socket.exe",
        "src/functional/spawn-static.exe",
        "src/functional/spawn.exe",
        "src/functional/sscanf-static.exe",
        "src/functional/sscanf.exe",
        "src/functional/sscanf_long-static.exe",
        "src/functional/sscanf_long.exe",
        "src/functional/stat-static.exe",
        "src/functional/stat.exe",
        "src/functional/strftime-static.exe",
        "src/functional/strftime.exe",
        "src/functional/string-static.exe",
        "src/functional/string.exe",
        "src/functional/string_memcpy-static.exe",
        "src/functional/string_memcpy.exe",
        "src/functional/string_memmem-static.exe",
        "src/functional/string_memmem.exe",
        "src/functional/string_memset-static.exe",
        "src/functional/string_memset.exe",
        "src/functional/string_strchr-static.exe",
        "src/functional/string_strchr.exe",
        "src/functional/string_strcspn-static.exe",
        "src/functional/string_strcspn.exe",
        "src/functional/string_strstr-static.exe",
        "src/functional/string_strstr.exe",
        "src/functional/strtod-static.exe",
        "src/functional/strtod.exe",
        "src/functional/strtod_long-static.exe",
        "src/functional/strtod_long.exe",
        "src/functional/strtod_simple-static.exe",
        "src/functional/strtod_simple.exe",
        "src/functional/strtof-static.exe",
        "src/functional/strtof.exe",
        "src/functional/strtol-static.exe",
        "src/functional/strtol.exe",
        "src/functional/strtold-static.exe",
        "src/functional/strtold.exe",
        "src/functional/swprintf-static.exe",
        "src/functional/swprintf.exe",
        "src/functional/tgmath-static.exe",
        "src/functional/tgmath.exe",
        "src/functional/time-static.exe",
        "src/functional/time.exe",
        "src/functional/tls_align-static.exe",
        "src/functional/tls_align.exe",
        "src/functional/tls_align_dlopen.exe",
        "src/functional/tls_init-static.exe",
        "src/functional/tls_init.exe",
        "src/functional/tls_init_dlopen.exe",
        "src/functional/tls_local_exec-static.exe",
        "src/functional/tls_local_exec.exe",
        "src/functional/udiv-static.exe",
        "src/functional/udiv.exe",
        "src/functional/ungetc-static.exe",
        "src/functional/ungetc.exe",
        "src/functional/utime-static.exe",
        "src/functional/utime.exe",
        "src/functional/vfork-static.exe",
        "src/functional/vfork.exe",
        "src/functional/wcsstr-static.exe",
        "src/functional/wcsstr.exe",
        "src/functional/wcstol-static.exe",
        "src/functional/wcstol.exe",
        "src/regression/daemon-failure-static.exe",
        "src/regression/daemon-failure.exe",
        "src/regression/dn_expand-empty-static.exe",
        "src/regression/dn_expand-empty.exe",
        "src/regression/dn_expand-ptr-0-static.exe",
        "src/regression/dn_expand-ptr-0.exe",
        "src/regression/execle-env-static.exe",
        "src/regression/execle-env.exe",
        "src/regression/fflush-exit-static.exe",
        "src/regression/fflush-exit.exe",
        "src/regression/fgets-eof-static.exe",
        "src/regression/fgets-eof.exe",
        "src/regression/fgetwc-buffering-static.exe",
        "src/regression/fgetwc-buffering.exe",
        "src/regression/flockfile-list-static.exe",
        "src/regression/flockfile-list.exe",
        "src/regression/fpclassify-invalid-ld80-static.exe",
        "src/regression/fpclassify-invalid-ld80.exe",
        "src/regression/ftello-unflushed-append-static.exe",
        "src/regression/ftello-unflushed-append.exe",
        "src/regression/getpwnam_r-crash-static.exe",
        "src/regression/getpwnam_r-crash.exe",
        "src/regression/getpwnam_r-errno-static.exe",
        "src/regression/getpwnam_r-errno.exe",
        "src/regression/iconv-roundtrips-static.exe",
        "src/regression/iconv-roundtrips.exe",
        "src/regression/inet_ntop-v4mapped-static.exe",
        "src/regression/inet_ntop-v4mapped.exe",
        "src/regression/inet_pton-empty-last-field-static.exe",
        "src/regression/inet_pton-empty-last-field.exe",
        "src/regression/iswspace-null-static.exe",
        "src/regression/iswspace-null.exe",
        "src/regression/lrand48-signextend-static.exe",
        "src/regression/lrand48-signextend.exe",
        "src/regression/lseek-large-static.exe",
        "src/regression/lseek-large.exe",
        "src/regression/malloc-0-static.exe",
        "src/regression/malloc-0.exe",
        "src/regression/malloc-brk-fail-static.exe",
        "src/regression/malloc-brk-fail.exe",
        "src/regression/malloc-oom-static.exe",
        "src/regression/malloc-oom.exe",
        "src/regression/mbsrtowcs-overflow-static.exe",
        "src/regression/mbsrtowcs-overflow.exe",
        "src/regression/memmem-oob-read-static.exe",
        "src/regression/memmem-oob-read.exe",
        "src/regression/memmem-oob-static.exe",
        "src/regression/memmem-oob.exe",
        "src/regression/mkdtemp-failure-static.exe",
        "src/regression/mkdtemp-failure.exe",
        "src/regression/mkstemp-failure-static.exe",
        "src/regression/mkstemp-failure.exe",
        "src/regression/printf-1e9-oob-static.exe",
        "src/regression/printf-1e9-oob.exe",
        "src/regression/printf-fmt-g-round-static.exe",
        "src/regression/printf-fmt-g-round.exe",
        "src/regression/printf-fmt-g-zeros-static.exe",
        "src/regression/printf-fmt-g-zeros.exe",
        "src/regression/printf-fmt-n-static.exe",
        "src/regression/printf-fmt-n.exe",
        "src/regression/pthread-robust-detach-static.exe",
        "src/regression/pthread-robust-detach.exe",
        "src/regression/pthread_atfork-errno-clobber-static.exe",
        "src/regression/pthread_atfork-errno-clobber.exe",
        "src/regression/pthread_cancel-sem_wait-static.exe",
        "src/regression/pthread_cancel-sem_wait.exe",
        "src/regression/pthread_cond-smasher-static.exe",
        "src/regression/pthread_cond-smasher.exe",
        "src/regression/pthread_cond_wait-cancel_ignored-static.exe",
        "src/regression/pthread_cond_wait-cancel_ignored.exe",
        "src/regression/pthread_condattr_setclock-static.exe",
        "src/regression/pthread_condattr_setclock.exe",
        "src/regression/pthread_create-oom-static.exe",
        "src/regression/pthread_create-oom.exe",
        "src/regression/pthread_exit-cancel-static.exe",
        "src/regression/pthread_exit-cancel.exe",
        "src/regression/pthread_exit-dtor-static.exe",
        "src/regression/pthread_exit-dtor.exe",
        "src/regression/pthread_once-deadlock-static.exe",
        "src/regression/pthread_once-deadlock.exe",
        "src/regression/pthread_rwlock-ebusy-static.exe",
        "src/regression/pthread_rwlock-ebusy.exe",
        "src/regression/putenv-doublefree-static.exe",
        "src/regression/putenv-doublefree.exe",
        "src/regression/raise-race-static.exe",
        "src/regression/raise-race.exe",
        "src/regression/regex-backref-0-static.exe",
        "src/regression/regex-backref-0.exe",
        "src/regression/regex-bracket-icase-static.exe",
        "src/regression/regex-bracket-icase.exe",
        "src/regression/regex-ere-backref-static.exe",
        "src/regression/regex-ere-backref.exe",
        "src/regression/regex-escaped-high-byte-static.exe",
        "src/regression/regex-escaped-high-byte.exe",
        "src/regression/regex-negated-range-static.exe",
        "src/regression/regex-negated-range.exe",
        "src/regression/regexec-nosub-static.exe",
        "src/regression/regexec-nosub.exe",
        "src/regression/rewind-clear-error-static.exe",
        "src/regression/rewind-clear-error.exe",
        "src/regression/rlimit-open-files-static.exe",
        "src/regression/rlimit-open-files.exe",
        "src/regression/scanf-bytes-consumed-static.exe",
        "src/regression/scanf-bytes-consumed.exe",
        "src/regression/scanf-match-literal-eof-static.exe",
        "src/regression/scanf-match-literal-eof.exe",
        "src/regression/scanf-nullbyte-char-static.exe",
        "src/regression/scanf-nullbyte-char.exe",
        "src/regression/sem_close-unmap-static.exe",
        "src/regression/sem_close-unmap.exe",
        "src/regression/setenv-oom-static.exe",
        "src/regression/setenv-oom.exe",
        "src/regression/setvbuf-unget-static.exe",
        "src/regression/setvbuf-unget.exe",
        "src/regression/sigaltstack-static.exe",
        "src/regression/sigaltstack.exe",
        "src/regression/sigprocmask-internal-static.exe",
        "src/regression/sigprocmask-internal.exe",
        "src/regression/sigreturn-static.exe",
        "src/regression/sigreturn.exe",
        "src/regression/sscanf-eof-static.exe",
        "src/regression/sscanf-eof.exe",
        "src/regression/statvfs-static.exe",
        "src/regression/statvfs.exe",
        "src/regression/strverscmp-static.exe",
        "src/regression/strverscmp.exe",
        "src/regression/syscall-sign-extend-static.exe",
        "src/regression/syscall-sign-extend.exe",
        "src/regression/tls_get_new-dtv.exe",
        "src/regression/uselocale-0-static.exe",
        "src/regression/uselocale-0.exe",
        "src/regression/wcsncpy-read-overflow-static.exe",
        "src/regression/wcsncpy-read-overflow.exe",
        "src/regression/wcsstr-false-negative-static.exe",
        "src/regression/wcsstr-false-negative.exe",
    }
)


def _native_host_or_skip() -> Path:
    """The musl loader for dynamic tests, on an x86-64 Linux host with the
    build and `make deps` (failing rather than skipping on CI's)."""
    if sys.platform != "linux" or platform.machine() not in ("x86_64", "AMD64"):
        pytest.skip(
            f"running the built tests needs an x86-64 Linux host (CI's), not "
            f"{sys.platform} {platform.machine()}"
        )
    _require_build()
    loader = BUILD_DIR / "deps" / "alpine-minirootfs" / "lib" / "ld-musl-x86_64.so.1"
    if not loader.is_file():
        if ON_CI_LINUX:
            pytest.fail(f"{loader} is missing: CI runs `make deps` first")
        pytest.skip(f"no {loader} for the dynamic tests (run `make deps`)")
    return loader


def test_the_real_runner_s_trouble_on_a_missing_test_is_an_error():
    """The built runtest (and musl's loader as its wrapper) on test paths
    that don't exist: their real error lines, read as runner trouble."""
    loader = _native_host_or_skip()
    static, dynamic = "src/functional/no-such-test-static.exe", "src/functional/no-such-test.exe"
    assert not (BUILD / static).exists() and not (BUILD / dynamic).exists()
    transcript = libctest.run_native(BUILD, [static, dynamic], loader=loader, timeout_s=10)
    r = libctest.parse(transcript).results
    assert set(r) == {static, dynamic}, transcript
    for test in (static, dynamic):
        assert r[test].outcome is Outcome.ERROR, transcript
        assert "runner trouble" in r[test].detail, transcript
    # runtest's own execv failing, and musl's loader unable to load the test.
    assert f"src/common/runtest.c:29: {static} exec failed: " in r[static].detail
    assert f": cannot load {dynamic}: " in r[dynamic].detail


def test_the_native_reference_run_passes_the_recorded_set():
    loader = _native_host_or_skip()
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
