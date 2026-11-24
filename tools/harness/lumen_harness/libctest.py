"""musl's libc-test: its results read back, and compared with the expected failures.

tools/build-libc-test.sh builds the tests into build/libc-test/ with libc-test's
own layout: `src/functional/<name>-static.exe` (static) and
`src/functional/<name>.exe` (dynamic, against /lib/ld-musl-x86_64.so.1), the
same under `src/regression/`, and `src/common/runtest-static.exe`, the runner.
`tests.txt` lists the tests that built, one path per line, relative to that
root; a test's path is its id everywhere (libc-test's `FAIL` lines, the
expected-failure list).

A run is a transcript: for each test, whatever `runtest` printed while running
it (the test's own `t_error` lines, then libc-test's `FAIL <path> [<why>]`
line if it failed), then one line the driver adds with runtest's exit code:

    EXIT <path> <code>

(`runtest -t 30 src/functional/x-static.exe; echo "EXIT src/functional/x-static.exe $?"`;
30 s is this harness's choice, runtest's own default is 5 s).
A passing test prints nothing, so a pass is only ever read from its EXIT line,
and a test without one counts as not run, never as passed.

Each test is a pass, a fail or an error:

- pass: runtest exited 0 and printed no FAIL line;
- fail: the test ran and failed: exit 1 with one `FAIL <path>` line saying
  `[status N]`, `[signal <name>]` or `[timed out]` (these are the test's
  verdict, so an expected-failure entry may cover them);
- error: anything else: runtest's own failures (`[internal]`, `[unknown]`,
  exit 255), an exit code runtest never returns, a FAIL line naming another
  test, or one that disagrees with the exit code. runtest's `fork` and
  `waitpid` failures are among these (`FAIL <path> [internal]`, exit 255).
  Also runner trouble that runtest reports as a status or a signal:
  - one of runtest's own error lines in the output, which t_error prefixes
    with runtest's source location: `src/common/runtest.c:29: <argv0> exec
    failed: ...` (the test, or the loader given as runtest's wrapper,
    couldn't be executed: then `[status 1]`), `src/common/runtest.c:80:
    <argv0> sigtimedwait failed: ...` (then runtest kills the test:
    `[signal Killed]`) and `src/common/runtest.c:82: <argv0> kill failed:
    ...` (whatever follows);
  - musl's loader, as runtest's wrapper, failing to start the test (then
    `[status 1]`): `<ldname>: cannot load <path>: <error>` and `<ldname>:
    <path>: Not a valid dynamic program` (ldso/dynlink.c), naming the test;
  - `[status 126]` or `[status 127]`: the test never ran. runtest calls
    execv itself, so there is no shell; a 127 is musl's loader failing to
    load a library or relocate (`Error loading shared library ... (needed by
    ...)`, `Error relocating ...`), and a 126 can come only from the test
    or the wrapper itself.
  The tests' own t_error lines start with `src/functional/` or
  `src/regression/`, never runtest's file, so they stay the test's verdict.
  An error is a broken run, not a verdict, and no expected-failure entry
  covers it.

tests/conformance/libc-test-xfail.txt lists the tests expected to fail, one
per line: the test's path, then `# <reason> #<issue>`, the issue that will fix
it. Lines that are empty or start with `#` are comments. A listed test that
passes fails the comparison too, so the list never keeps a fixed test.
"""

from __future__ import annotations

import enum
import os
import re
import signal
import struct
import subprocess
import sys
import tempfile
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

from . import BUILD_DIR, REPO_ROOT

BUILD = BUILD_DIR / "libc-test"
XFAIL_FILE = REPO_ROOT / "tests" / "conformance" / "libc-test-xfail.txt"
TESTS_FILE = "tests.txt"
RUNTEST = "src/common/runtest-static.exe"
MUSL_LOADER = "/lib/ld-musl-x86_64.so.1"

_TEST = re.compile(r"^src/(?:functional|regression)/[A-Za-z0-9_.+-]+\.exe$")
_EXIT = re.compile(r"^EXIT (\S+) (\d+)$")
_FAIL = re.compile(r"^FAIL (\S+) \[(.*)\]$")
_STATUS = re.compile(r"^status ([1-9]\d*)$")
# runtest's own error lines: t_error("%s exec failed: %s\n", argv[0], ...)
# prints `src/common/runtest.c:<line>: ` first (test.h's T_LOC1), so the
# file tells them from a test's own t_error lines.
_RUNNER_TROUBLE = re.compile(
    r"^src/common/runtest\.c:\d+: \S+ (exec|fork|sigtimedwait|kill|waitpid) failed: "
)
# musl 1.2.5's loader failing to start the program it was given, before
# exiting 1 (ldso/dynlink.c): `%s: cannot load %s: %s` and `%s: %s: Not a
# valid dynamic program`, ldname then the program's path.
_LOADER_CANNOT_LOAD = re.compile(r"^\S+: cannot load (\S+): ")
_LOADER_NOT_DYNAMIC = re.compile(r"^\S+: (\S+): Not a valid dynamic program$")
# Exit statuses meaning the program never ran: 126 (found, not executable)
# and 127 (not found; musl's loader for a missing library or a failed
# relocation, ldso/dynlink.c's `_exit(127)`).
_NOT_RUN_STATUSES = {"status 126", "status 127"}


def _loader_failure(test: str, line: str) -> bool:
    """Whether `line` is musl's loader failing to start `test`."""
    m = _LOADER_CANNOT_LOAD.match(line) or _LOADER_NOT_DYNAMIC.match(line)
    return m is not None and m.group(1).removeprefix("./") == test


_XFAIL = re.compile(r"^(?P<test>\S+)\s+#\s*(?P<reason>\S.*?)\s+#(?P<issue>[1-9]\d*)$")


class Outcome(enum.Enum):
    PASS = "pass"
    FAIL = "fail"
    ERROR = "error"


@dataclass(frozen=True)
class Result:
    test: str
    outcome: Outcome
    # "" for a pass; libc-test's bracket for a fail ("status 1", "signal
    # Segmentation fault", "timed out"); why it is an error otherwise.
    detail: str
    # What the test printed besides the FAIL line (its t_error lines).
    output: tuple[str, ...] = ()


@dataclass
class Run:
    results: dict[str, Result] = field(default_factory=dict)
    # Lines after the last EXIT line: a test that started and never ended.
    trailing: list[str] = field(default_factory=list)


class ParseError(ValueError):
    pass


def is_static(test: str) -> bool:
    return test.endswith("-static.exe")


def _classify(test: str, code: int, lines: list[str]) -> Result:
    fails = [(m.group(1), m.group(2)) for m in map(_FAIL.match, lines) if m]
    output = tuple(line for line in lines if not _FAIL.match(line))

    def error(why: str) -> Result:
        return Result(test, Outcome.ERROR, why, output)

    if code > 255:
        return error(f"exit code {code} is not an exit status")
    others = sorted({path for path, _ in fails if path != test})
    if others:
        return error(f"a FAIL line for {', '.join(others)} in its output")
    if len(fails) > 1:
        return error(f"{len(fails)} FAIL lines")
    why = fails[0][1] if fails else None
    if code == 0:
        if why is not None:
            return error(f"runtest exited 0 after FAIL [{why}]")
        return Result(test, Outcome.PASS, "", output)
    if why is None:
        return error(f"runtest exited {code} without a FAIL line")
    if code != 1:
        return error(f"runtest exited {code} after FAIL [{why}]")
    trouble = [
        line for line in output if _RUNNER_TROUBLE.match(line) or _loader_failure(test, line)
    ]
    if trouble:
        return error(f"runner trouble, not the test's verdict: {trouble[0]} (FAIL [{why}])")
    if why in _NOT_RUN_STATUSES:
        return error(f"FAIL [{why}]: the program never ran (not executable, or not found)")
    if _STATUS.match(why) or why == "timed out" or why.startswith("signal "):
        return Result(test, Outcome.FAIL, why, output)
    return error(f"runtest could not run it: FAIL [{why}]")


def parse(text: str) -> Run:
    """Read a transcript (see the module's doc) into one result per test.

    Raises ParseError for a test with two EXIT lines or an EXIT line naming
    something that isn't a test path."""
    run = Run()
    pending: list[str] = []
    for number, raw in enumerate(text.splitlines(), 1):
        # Surrounding blanks don't count (a console's \r, a message written
        # without its newline just before runtest's FAIL line).
        line = raw.strip()
        m = _EXIT.match(line)
        if m is None:
            pending.append(line)
            continue
        test = m.group(1).removeprefix("./")
        if not _TEST.match(test):
            raise ParseError(f"line {number}: {test!r} is not a libc-test test path")
        if test in run.results:
            raise ParseError(f"line {number}: a second EXIT line for {test}")
        run.results[test] = _classify(test, int(m.group(2)), pending)
        pending = []
    run.trailing = [line for line in pending if line]
    return run


@dataclass(frozen=True)
class Xfail:
    test: str
    reason: str
    issue: int
    line: int


class XfailError(ValueError):
    pass


def parse_xfail(text: str) -> dict[str, Xfail]:
    """The expected-failure list: `<test path> # <reason> #<issue>` per line.

    Raises XfailError naming the line for an entry without a reason or an
    issue, a path that isn't a libc-test test's, or a test listed twice."""
    entries: dict[str, Xfail] = {}
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        m = _XFAIL.match(line)
        if m is None:
            raise XfailError(
                f"line {number}: {line!r}: an entry is a test path, then '# <reason> #<issue>'"
            )
        test = m.group("test")
        if not _TEST.match(test):
            raise XfailError(
                f"line {number}: {test!r} is not a test path "
                "(src/functional/ or src/regression/<name>.exe or <name>-static.exe)"
            )
        if test in entries:
            raise XfailError(
                f"line {number}: {test} is listed twice (first on line {entries[test].line})"
            )
        entries[test] = Xfail(test, m.group("reason"), int(m.group("issue")), number)
    return entries


def load_xfail(path: Path = XFAIL_FILE) -> dict[str, Xfail]:
    return parse_xfail(path.read_text())


@dataclass
class Verdict:
    expected_failures: list[Result] = field(default_factory=list)
    unexpected_failures: list[Result] = field(default_factory=list)
    unexpected_passes: list[Result] = field(default_factory=list)
    errors: list[Result] = field(default_factory=list)
    not_run: list[str] = field(default_factory=list)
    unknown: list[str] = field(default_factory=list)
    stale: list[Xfail] = field(default_factory=list)
    passed: list[str] = field(default_factory=list)
    # The run's output after its last EXIT line (Run.trailing): shown, as the
    # test it belongs to is already counted not run.
    trailing: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not (
            self.unexpected_failures
            or self.unexpected_passes
            or self.errors
            or self.not_run
            or self.unknown
            or self.stale
        )

    def summary(self) -> str:
        lines = [
            (
                f"{len(self.passed)} passed, {len(self.expected_failures)} failed as expected, "
                f"{len(self.unexpected_failures)} failed unexpectedly, "
                f"{len(self.unexpected_passes)} passed unexpectedly, {len(self.errors)} errors, "
                f"{len(self.not_run)} not run"
            )
        ]
        for r in self.unexpected_failures:
            lines.append(f"  FAIL (not in the expected-failure list) {r.test} [{r.detail}]")
            lines += [f"    {o}" for o in r.output]
        for r in self.unexpected_passes:
            lines.append(f"  PASS (listed as expected to fail: remove its entry) {r.test}")
        for r in self.errors:
            lines.append(f"  ERROR {r.test}: {r.detail}")
            lines += [f"    {o}" for o in r.output]
        lines += [f"  NOT RUN {t}" for t in self.not_run]
        lines += [f"  RAN BUT NOT A BUILT TEST {t}" for t in self.unknown]
        lines += [
            f"  LISTED BUT NOT BUILT (line {x.line} of the expected-failure list) {x.test}"
            for x in self.stale
        ]
        if self.trailing:
            lines.append("  OUTPUT AFTER THE LAST EXIT LINE (a test that never ended)")
            lines += [f"    {o}" for o in self.trailing]
        return "\n".join(lines)


def compare(run: Run, xfail: dict[str, Xfail], expected: Iterable[str]) -> Verdict:
    """Judge a run against the expected failures. `expected` is every test
    that should have run (the build's tests.txt): one without a result fails
    the comparison, as does a result for a test not in it, an error, a failure
    not listed, a listed test that passed, and a listed test not in
    `expected`. A listed test that failed (status, signal or timeout) passes."""
    want = set(expected)
    v = Verdict(trailing=list(run.trailing))
    v.not_run = sorted(want - run.results.keys())
    v.unknown = sorted(run.results.keys() - want)
    v.stale = sorted((x for t, x in xfail.items() if t not in want), key=lambda x: x.line)
    for test in sorted(run.results.keys() & want):
        r = run.results[test]
        if r.outcome is Outcome.ERROR:
            v.errors.append(r)
        elif r.outcome is Outcome.FAIL:
            (v.expected_failures if test in xfail else v.unexpected_failures).append(r)
        elif test in xfail:
            v.unexpected_passes.append(r)
        else:
            v.passed.append(test)
    return v


def read_tests(root: Path = BUILD) -> list[str]:
    """The tests the build made (its tests.txt), in order."""
    return [line for line in (root / TESTS_FILE).read_text().splitlines() if line]


def run_native(
    root: Path,
    tests: Iterable[str],
    *,
    loader: Path | None,
    timeout_s: int = 30,
    grace_s: int = 30,
) -> str:
    """Run each test on this host, one at a time, through the built runtest
    (`-t timeout_s`; 30 s is this harness's choice, runtest's default is 5 s),
    and return the transcript `parse` reads. A runtest still running
    `grace_s` after its own timeout is killed and closed with exit code 124,
    an error. Each runtest runs in its own process group, killed whole once
    it ends or times out, so a test (or its children) that outlives runtest
    can't run on into the next test. Dynamic tests
    run through `loader`, a musl dynamic loader (Alpine's), as runtest's
    wrapper: this host needn't have musl installed.

    The tests run in a fresh directory whose `src` links to `root`'s, so
    their paths (and libc-test's FAIL lines) read `src/...` and whatever they
    leave behind is deleted afterwards."""
    root = root.resolve()
    out: list[str] = []
    with tempfile.TemporaryDirectory(prefix="libc-test-") as tmp:
        cwd = Path(tmp)
        (cwd / "src").symlink_to(root / "src", target_is_directory=True)
        for test in tests:
            argv = [str(root / RUNTEST), "-t", str(timeout_s)]
            if not is_static(test):
                if loader is None:
                    raise ValueError(f"{test} is dynamic and no musl loader was given")
                argv += ["-w", str(loader)]
            argv.append(test)
            proc = subprocess.Popen(
                argv,
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                stdout, _ = proc.communicate(timeout=timeout_s + grace_s)
                out += stdout.decode(errors="replace").splitlines()
                code = proc.returncode if proc.returncode >= 0 else 128 - proc.returncode
            except subprocess.TimeoutExpired:
                # runtest itself hung: an error (its exit code isn't 0 or 1),
                # after whatever it printed before it was killed.
                _kill_group(proc.pid)
                stdout, _ = proc.communicate()
                out += stdout.decode(errors="replace").splitlines()
                out.append(f"runtest did not end within {timeout_s + grace_s} s")
                code = 124
            _kill_group(proc.pid)
            out.append(f"EXIT {test} {code}")
    return "\n".join(out) + "\n"


def _kill_group(pgid: int) -> None:
    """SIGKILL every process left in runtest's process group, if any."""
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass


_PT_INTERP = 3
_PT_GNU_STACK = 0x6474E551
_EM_X86_64 = 62


@dataclass(frozen=True)
class Elf:
    # PT_INTERP's path; None for a static program.
    interp: str | None
    # PT_GNU_STACK's p_memsz (0 without one). musl takes a nonzero size as
    # the default thread stack size (capped at 8 MiB); GNU ld writes 0.
    stack_size: int
    # The section names, in order (none without a section header table).
    sections: tuple[str, ...] = ()


def read_elf(path: Path) -> Elf:
    """The program headers that matter here, and the section names, from an
    x86-64 ELF64 little-endian file. Raises ValueError for anything else."""
    data = path.read_bytes()
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise ValueError(f"{path} is not a little-endian ELF64 file")
    (machine,) = struct.unpack_from("<H", data, 18)
    if machine != _EM_X86_64:
        raise ValueError(f"{path} is not x86-64 (e_machine {machine})")
    (phoff,) = struct.unpack_from("<Q", data, 32)
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    interp, stack_size = None, 0
    for i in range(phnum):
        p_type, _, p_offset, _, _, p_filesz, p_memsz = struct.unpack_from(
            "<IIQQQQQ", data, phoff + i * phentsize
        )
        if p_type == _PT_INTERP:
            interp = data[p_offset : p_offset + p_filesz].rstrip(b"\0").decode()
        elif p_type == _PT_GNU_STACK:
            stack_size = p_memsz
    (shoff,) = struct.unpack_from("<Q", data, 40)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 58)
    headers = [struct.unpack_from("<IIQQQQ", data, shoff + i * shentsize) for i in range(shnum)]
    sections: tuple[str, ...] = ()
    if headers:
        if shstrndx >= shnum:
            raise ValueError(f"{path}: e_shstrndx {shstrndx} is past its {shnum} sections")
        names = headers[shstrndx][4]
        sections = tuple(
            data[names + h[0] : data.index(b"\0", names + h[0])].decode() for h in headers
        )
    return Elf(interp, stack_size, sections)


# zig cc's Debug mode (no -O flag) compiles UBSan in and links its runtime;
# the build turns it off, and the runtime's report text in a binary means it
# didn't (its handlers' names go with the symbol table the build strips).
_UBSAN_RUNTIME = b"cannot be represented in type"


def _build_problems(root: Path, name: str, elf: Elf) -> list[str]:
    """What every built binary, test or library, must not carry."""
    problems = []
    if _UBSAN_RUNTIME in (root / name).read_bytes():
        problems.append(f"{name}: carries zig's UBSan runtime (built without -fno-sanitize)")
    debug = [s for s in elf.sections if s.startswith(".debug_")]
    if debug:
        problems.append(f"{name}: keeps debug sections ({', '.join(debug)})")
    return problems


def linkage_problems(root: Path = BUILD) -> list[str]:
    """Each built test, and the runner, linked as its name says: `-static.exe`
    with no interpreter, any other `.exe` against musl's loader. None carries
    zig's UBSan runtime or asks for a stack size (zig's linker asks for
    16 MiB unless told otherwise, which would give every thread musl's 8 MiB
    cap instead of the 128 KiB that Alpine's gcc-built programs get), and
    none keeps a `.debug_` section (DWARF names the build's directories, so
    two builds would differ). The tests' libraries (`src/*/*.so`) are
    checked for the runtime and debug sections too."""
    problems = []
    for test in [RUNTEST, *read_tests(root)]:
        try:
            elf = read_elf(root / test)
        except (OSError, ValueError) as e:
            problems.append(f"{test}: {e}")
            continue
        problems += _build_problems(root, test, elf)
        if elf.stack_size:
            problems.append(f"{test}: PT_GNU_STACK asks for a {elf.stack_size}-byte stack")
        if is_static(test) and elf.interp is not None:
            problems.append(f"{test}: static by name, but its interpreter is {elf.interp}")
        elif not is_static(test) and elf.interp != MUSL_LOADER:
            problems.append(f"{test}: dynamic by name, but its interpreter is {elf.interp}")
    for lib in sorted(p.relative_to(root).as_posix() for p in root.glob("src/*/*.so")):
        try:
            elf = read_elf(root / lib)
        except (OSError, ValueError) as e:
            problems.append(f"{lib}: {e}")
            continue
        problems += _build_problems(root, lib, elf)
    return problems


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[0] == "linkage":
        problems = linkage_problems(Path(argv[1]))
        for p in problems:
            print(f"libc-test linkage: {p}", file=sys.stderr)
        return 1 if problems else 0
    print("usage: python -m lumen_harness.libctest linkage <build/libc-test>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
