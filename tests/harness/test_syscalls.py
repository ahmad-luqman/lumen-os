"""The M8 syscall census (lumen_harness.syscalls, #337): strace traces into
the census, the census file, and the report against the kernel's dispatch."""

import re
import tomllib
from pathlib import Path

import pytest
from lumen_harness import syscalls
from lumen_harness.syscalls import (
    X86_64,
    Call,
    CensusError,
    census,
    classify,
    key_values,
    parse_trace,
    render_toml,
    render_value,
    report,
    split_args,
)

ROOT = Path(__file__).resolve().parents[2]
CENSUS = ROOT / "tests/linux/m8-census.toml"
WORKLOADS_SH = ROOT / "tools/census/workloads.sh"
CENSUS_SH = ROOT / "tools/syscall-census.sh"

# Lines in strace's shapes (from the arm64 trial run, renumbered): two pids
# interleaving unfinished and resumed calls, plus the `+++`/`---` lines that
# `-qq -e signal=none` suppresses, to check the parser skips them. The futex
# at the end never returns.
TRACE = """\
12    execve("/bin/sh", [...], 0xfffff69dc000 /* 6 vars */) = 0
12    rt_sigaction(17, {sa_handler=0xaaaab49d7590, sa_mask=~[32 33 34], sa_flags=0x4000000, \
sa_restorer=0xffffaf227a74}, NULL, 8) = 0
12    clone(child_stack=NULL, flags=0x4100|17 <unfinished ...>
13    set_tid_address(0xffffaf298e80 <unfinished ...>
12    <... clone resumed>)              = 13
13    <... set_tid_address resumed>)    = 13
13    newfstatat(-100, "/usr/local/bin/cat",  <unfinished ...>
12    wait4(-1,  <unfinished ...>
13    <... newfstatat resumed>0xfffffded4390, 0) = -1 ENOENT (No such file or directory)
13    ioctl(1, 0x5413, 0xffffd15dbbe8)  = -1 ENOTTY (Not a tty)
13    exit_group(1)                     = ?
13    +++ exited with 1 +++
12    --- SIGCHLD {si_signo=SIGCHLD, si_code=CLD_EXITED, si_pid=13} ---
12    <... wait4 resumed>[{WIFEXITED(s) && WEXITSTATUS(s) == 1}], 0, NULL) = 13
12    futex(0x7f00, 0x80, 2, NULL <unfinished ...>
"""


def test_split_args_keeps_brackets_braces_quotes_and_comments_whole():
    text = '{a=1, b=[2, 3]}, "x, \\"y", 0x5 /* a, b */, NULL'
    assert split_args(text) == ["{a=1, b=[2, 3]}", '"x, \\"y"', "0x5 /* a, b */", "NULL"]
    assert split_args("") == []
    assert split_args("7") == ["7"]


def test_parse_trace_joins_resumed_calls_per_pid_and_skips_the_rest():
    calls = parse_trace(TRACE.splitlines())
    assert [c.name for c in calls] == [
        "execve",
        "rt_sigaction",
        "clone",
        "set_tid_address",
        "newfstatat",
        "ioctl",
        "exit_group",
        "wait4",
        "futex",
    ]
    newfstatat = calls[4]
    assert newfstatat.args == ["-100", '"/usr/local/bin/cat"', "0xfffffded4390", "0"]
    assert (newfstatat.ret, newfstatat.errno) == ("-1", "ENOENT")
    assert calls[6].ret == "?" and calls[6].errno is None
    assert calls[7].args[0] == "-1" and calls[7].ret == "13"


def test_a_call_unfinished_when_the_trace_ends_is_kept_with_no_return():
    calls = parse_trace([*TRACE.splitlines(), "14    wait4(-1,  <unfinished ...>"])
    assert [(c.name, c.args, c.ret, c.errno) for c in calls[-2:]] == [
        ("futex", ["0x7f00", "0x80", "2", "NULL"], "?", None),
        ("wait4", ["-1"], "?", None),
    ]
    assert key_values(calls[-2]) == {"op": "0x80"}


def test_a_resumption_cut_off_by_the_threads_end_is_kept_with_no_return():
    lines = [
        "146   rt_sigtimedwait([32],  <unfinished ...>",
        "146   <... rt_sigtimedwait resumed> <unfinished ...>) = ?",
    ]
    [call] = parse_trace(lines)
    assert (call.name, call.args, call.ret, call.errno) == ("rt_sigtimedwait", ["[32]"], "?", None)


@pytest.mark.parametrize(
    ("lines", "match"),
    [
        (
            ["7    <... mmap resumed>0x3, 0x22, -1, 0) = 0x1000"],
            "mmap resumed with nothing pending",
        ),
        (
            ["7    wait4(-1,  <unfinished ...>", "7    read(3,  <unfinished ...>"],
            "a new call while wait4 was pending",
        ),
        (
            ["7    wait4(-1,  <unfinished ...>", "7    getpid() = 7"],
            "a new call while wait4 was pending",
        ),
        (
            ["5    read(3,  <unfinished ...>", "5    <... write resumed>) = 1"],
            "write resumed while read was pending",
        ),
    ],
)
def test_calls_out_of_order_on_one_pid_are_refused(lines, match):
    with pytest.raises(CensusError, match=match):
        parse_trace(lines)


def test_a_return_value_in_a_string_argument_is_not_the_calls():
    [call] = parse_trace(['7    open("a) = b", 0) = -1 ENOENT (No such file or directory)'])
    assert (call.args, call.ret, call.errno) == (['"a) = b"', "0"], "-1", "ENOENT")


def test_a_success_strace_annotates_is_kept_and_has_no_errno():
    lines = [
        "7    fcntl(3, 0x3)                     = 0x8800 (flags O_RDONLY|O_NONBLOCK|O_LARGEFILE)",
        "7    pselect6(4, [3], NULL, NULL, {tv_sec=0, tv_nsec=0}, NULL) = 0 (Timeout)",
        "7    poll([{fd=0, events=0x1}], 1, -1) = 1 ([{fd=0, revents=0x1}])",
        "7    rt_sigsuspend([], 8)              = ? ERESTARTNOHAND (To be restarted if no handler)",
        "7    exit_group(0)                     = ?",
    ]
    calls = parse_trace(lines)
    assert [(c.name, c.ret, c.errno) for c in calls] == [
        ("fcntl", "0x8800", None),
        ("pselect6", "0", None),
        ("poll", "1", None),
        ("rt_sigsuspend", "?", "ERESTARTNOHAND"),
        ("exit_group", "?", None),
    ]
    assert key_values(calls[0]) == {"cmd": "0x3"}


def test_an_interrupted_call_keeps_its_restart_code_and_rt_sigreturn_has_no_errno():
    # The errno with an underscore is read whole. The handler's return restores
    # the interrupted nanosleep's -EINTR: nanosleep's result, not an error of
    # rt_sigreturn's.
    lines = [
        (
            "9    nanosleep({tv_sec=5, tv_nsec=0}, {tv_sec=3, tv_nsec=999979558}) "
            "= ? ERESTART_RESTARTBLOCK (Interrupted by signal)"
        ),
        "9    rt_sigreturn({mask=[]})           = -1 EINTR (Interrupted system call)",
    ]
    calls = parse_trace(lines)
    assert [(c.name, c.ret, c.errno) for c in calls] == [
        ("nanosleep", "?", "ERESTART_RESTARTBLOCK"),
        ("rt_sigreturn", "-1", None),
    ]
    seen = census({"signals": calls})
    assert seen["nanosleep"].errors == {"ERESTART_RESTARTBLOCK": 1}
    assert seen["rt_sigreturn"].errors == {}


@pytest.mark.parametrize(
    ("line", "match"),
    [
        ('open("/x", 0) = 3', "not an strace -f line"),
        ("7    write(1, ..., 4", "not a completed syscall"),
        ('7    write(1, "", 4) = ', "not a completed syscall"),
        ('7    open("/x", 0) = -1', "no errno after -1"),
        ('7    open("/x", 0) = -1 (No such file or directory)', "no errno after -1"),
        ("7    exit_group(0) = ? <unavailable>", "not an errno after \\?"),
    ],
)
def test_a_line_the_parser_cant_read_is_refused(line, match):
    with pytest.raises(CensusError, match=match):
        parse_trace([line])


# One line per syscall in KEY_ARGS, in `strace -X raw`'s shapes: from the
# x86-64 traces for the 18 the census has, the rest written the same way.
KEY_ARG_LINES = [
    (
        (
            "clone(child_stack=0x78b82aec5af8, flags=0x7d0f00, parent_tid=[158], "
            "tls=0x78b82aec5b38, child_tidptr=0x78b82af6af90) = 158"
        ),
        {"flags": "0x7d0f00"},
    ),
    ("ioctl(0, 0x540b, 0)               = 0", {"request": "0x540b"}),
    (
        (
            "rt_sigaction(17, {sa_handler=0x5784e90091f8, sa_mask=~[32 33 34], sa_flags=0x4000000, "
            "sa_restorer=0x7ebf4256b57e}, NULL, 8) = 0"
        ),
        {"signal": "17", "sa_flags": "0x4000000"},
    ),
    (
        "prlimit64(0, 0x3, {rlim_cur=67108864, rlim_max=18446744073709551615}, NULL) = 0",
        {"resource": "3", "new_limit": "set"},
    ),
    ("futex(0x7ffda3c64ff4, 0x81, 1)    = 1", {"op": "0x81"}),
    (
        "mmap(0x578506282000, 4096, 0, 0x32, -1, 0) = 0x578506282000",
        {"prot": "0x0", "flags": "0x32"},
    ),
    ('open("ash.sh", 0x88000)           = 3', {"flags": "0x88000"}),
    ("lseek(3, 376, 0)                  = 376", {"whence": "0"}),
    ("mprotect(0x7ebf425ae000, 4096, 0x1) = 0", {"prot": "0x1"}),
    ("rt_sigprocmask(0x1, [33 34], NULL, 8) = 0", {"how": "0x1"}),
    (
        "prlimit64(0, 0x3, NULL, {rlim_cur=8388608, rlim_max=18446744073709551615}) = 0",
        {"resource": "3", "new_limit": "NULL"},
    ),
    ("arch_prctl(0x1002, 0x7ebf425b1b28) = 0", {"code": "4098"}),
    ("kill(19, 2)                       = 0", {"signal": "2"}),
    ("tkill(171, 10)                    = 0", {"signal": "10"}),
    (
        'faccessat2(-100, "/usr/local/sbin/gcc", 0x1, 0x200) = -1 ENOENT (No such file)',
        {"flags": "0x200"},
    ),
    ("pipe2([...], 0x80000)             = 0", {"flags": "0x80000"}),
    ("fcntl(3, 0x2, 0x1)                = 0", {"cmd": "0x2"}),
    ("wait4(-1, 0x7ffe34ace10c, 0x1, NULL) = -1 ECHILD (No child process)", {"options": "0x1"}),
    ("membarrier(0x10, 0)               = 0", {"cmd": "0x10"}),
    # Written, not traced:
    (
        "clone3({flags=0x3d0f00, exit_signal=0, stack=0x7f, stack_size=0x1000}, 88) = 5",
        {"flags": "0x3d0f00"},
    ),
    ("clock_getres(1, 0x7ffe0000)       = 0", {"clock": "1"}),
    ("clock_gettime(7, 0x7ffe0000)      = 0", {"clock": "7"}),
    ("clock_nanosleep(1, 0x1, {tv_sec=1, tv_nsec=0}, NULL) = 0", {"clock": "1", "flags": "0x1"}),
    ("dup3(3, 4, 0x80000)               = 4", {"flags": "0x80000"}),
    ("flock(3, 0x6)                     = 0", {"operation": "0x6"}),
    ('getrandom(""..., 16, 0x1)         = 16', {"flags": "0x1"}),
    ("madvise(0x7f0000, 4096, 0x4)      = 0", {"advice": "0x4"}),
    ("mremap(0x7f0000, 4096, 8192, 0x1) = 0x7f1000", {"flags": "0x1"}),
    ("msync(0x7f0000, 4096, 0x4)        = 0", {"flags": "0x4"}),
    ('newfstatat(-100, "/x", 0x7ffe0000, 0x100) = 0', {"flags": "0x100"}),
    ('openat(-100, "/x", 0x80000)       = 3', {"flags": "0x80000"}),
    ("personality(0xffffffff)           = 0", {"persona": "0xffffffff"}),
    ("prctl(0xf, 0x7ffe0000)            = 0", {"option": "0xf"}),
    ('renameat2(-100, "/a", -100, "/b", 0x1) = 0', {"flags": "0x1"}),
    ("rt_sigqueueinfo(5, 10, {si_signo=10, si_code=-1}) = 0", {"signal": "10"}),
    ("socket(0x1, 0x80001, 0)           = 3", {"domain": "1", "type": "0x80001"}),
    ('statx(-100, "/x", 0x100, 0xfff, 0x7ffe0000) = 0', {"flags": "0x100"}),
    ("tgkill(5, 6, 6)                   = 0", {"signal": "6"}),
    ('unlinkat(-100, "/x", 0x200)       = 0', {"flags": "0x200"}),
    ('utimensat(-100, "/x", NULL, 0x100) = 0', {"flags": "0x100"}),
    ("waitid(0x1, 5, {si_signo=17}, 0x4, NULL) = 0", {"idtype": "1", "options": "0x4"}),
]


@pytest.mark.parametrize(("line", "expected"), KEY_ARG_LINES)
def test_key_values_of_each_syscall_with_recorded_arguments(line, expected):
    [call] = parse_trace([f"7    {line}"])
    assert key_values(call) == expected


def test_every_key_args_syscall_has_a_line():
    assert {line.split("(", 1)[0] for line, _ in KEY_ARG_LINES} == set(syscalls.KEY_ARGS)


@pytest.mark.parametrize(
    ("call", "match"),
    [
        (Call("futex", ["0x7f00"], "0", None), "futex: no argument 1 for its op"),
        (Call("clone", ["child_stack=NULL", "0x11"], "5", None), "clone: no flags= in argument 1"),
        (Call("ioctl", ["1", "TCGETS", "0x7f00"], "0", None), "request='TCGETS': not a number"),
    ],
)
def test_a_recorded_argument_missing_or_not_a_number_is_refused(call, match):
    with pytest.raises(CensusError, match=match):
        key_values(call)
    with pytest.raises(CensusError, match="make: "):
        census({"make": [call]})


def test_key_args_and_decimal_labels_agree():
    labels = {label for spec in syscalls.KEY_ARGS.values() for label, _, _ in spec}
    assert syscalls.DECIMAL_LABELS <= labels
    for name, spec in syscalls.KEY_ARGS.items():
        for label, pos, fld in spec:
            assert isinstance(pos, int) and pos >= 0, (name, label)
            assert fld is None or re.fullmatch(r"\w+", fld), (name, label)


def test_render_value_folds_or_and_picks_the_base_by_label():
    assert render_value("flags", "0x4100|17") == "0x4111"
    assert render_value("flags", "16") == "0x10"
    for label in ("signal", "clock", "resource", "domain", "idtype", "whence", "code"):
        assert render_value(label, "0x1") == "1", label
    assert render_value("flags", "-1") == "-1"
    assert render_value("new_limit", "NULL") == "NULL"
    assert render_value("new_limit", "{rlim_cur=8388608, rlim_max=8388608}") == "set"
    with pytest.raises(CensusError, match="not a number"):
        render_value("flags", "NULL")


def test_key_values_reads_positions_named_args_and_fields():
    calls = {c.name: c for c in parse_trace(TRACE.splitlines())}
    # Named argument, its OR'd exit signal folded in: CLONE_VM|CLONE_VFORK|SIGCHLD.
    assert key_values(calls["clone"]) == {"flags": "0x4111"}
    assert key_values(calls["ioctl"]) == {"request": "0x5413"}
    assert key_values(calls["wait4"]) == {"options": "0x0"}
    # Signals are decimal; sa_flags only from the new action (argument 1).
    act = Call(
        "rt_sigaction",
        ["2", "{sa_handler=0x1, sa_flags=0x14000000}", "{sa_handler=0, sa_flags=0x4000000}", "8"],
        "0",
        None,
    )
    assert key_values(act) == {"signal": "2", "sa_flags": "0x14000000"}
    old_only = Call("rt_sigaction", ["2", "NULL", "{sa_flags=0x4000000}", "8"], "0", None)
    assert key_values(old_only) == {"signal": "2"}
    clone3 = Call("clone3", ["{flags=0x3d0f00, exit_signal=0, stack=0x7f}", "88"], "5", None)
    assert key_values(clone3) == {"flags": "0x3d0f00"}
    # A position past the arguments strace printed records nothing, in a call
    # that never returned (printed in part).
    assert key_values(Call("futex", ["0x7f00"], "?", None)) == {}
    assert key_values(Call("getpid", [], "1", None)) == {}


def test_census_counts_calls_workloads_arguments_and_errors():
    seen = census({"ash": parse_trace(TRACE.splitlines()), "make": parse_trace(TRACE.splitlines())})
    ioctl = seen["ioctl"]
    assert ioctl.calls == 2
    assert ioctl.workloads == {"ash", "make"}
    assert ioctl.args == {"request": {"0x5413"}}
    assert ioctl.errors == {"ENOTTY": 2}
    assert seen["futex"].calls == 2 and seen["futex"].errors == {}


def test_the_census_file_sorts_workloads_and_values_numerically():
    calls = [
        Call("ioctl", ["1", "0x10", "0"], "0", None),
        Call("ioctl", ["1", "0x9", "0"], "-1", "ENOTTY"),
    ]
    # Five workloads, so a set's order is almost never the sorted one.
    seen = census(
        {"vim": calls[:1], "make": calls, "git": calls[:1], "gcc": calls[:1], "ash": calls[:1]}
    )
    meta = {"alpine": "3.21.10", "image": "img", "kernel": "6.6", "packages": [], "workloads": []}
    entry = tomllib.loads(render_toml(meta, seen))["syscalls"]["ioctl"]
    assert entry["calls"] == 6
    assert entry["workloads"] == ["ash", "gcc", "git", "make", "vim"]
    assert entry["args"]["request"] == ["0x9", "0x10"]
    assert entry["errors"] == {"ENOTTY": 1}


def test_census_refuses_a_translated_trace():
    rosetta = ["15    syscall_0xf000205372f0(0xf000205372f0, 0x1) = 0x8000000a0000"]
    with pytest.raises(CensusError, match="not an x86-64 trace"):
        census({"ash": parse_trace(rosetta)})


def test_the_census_file_round_trips_through_tomllib_in_number_order():
    seen = census({"ash": parse_trace(TRACE.splitlines())})
    meta = {"alpine": "3.21.10", "image": "img", "kernel": "6.6", "packages": ["a-1"]}
    meta["workloads"] = ["ash"]
    data = tomllib.loads(render_toml(meta, seen))
    assert data["meta"] == meta
    assert list(data["syscalls"]) == sorted(data["syscalls"], key=X86_64.__getitem__)
    clone = data["syscalls"]["clone"]
    assert clone == {"nr": 56, "calls": 1, "workloads": ["ash"], "args": {"flags": ["0x4111"]}}
    assert data["syscalls"]["newfstatat"]["errors"] == {"ENOENT": 1}


IMAGE = "docker.io/library/alpine:3.21@sha256:" + "0" * 64


def _write_meta(directory: Path) -> None:
    (directory / "alpine-release").write_text("3.21.10\n")
    (directory / "kernel").write_text("6.6.87.2-microsoft-standard-WSL2\n")
    (directory / "image").write_text(IMAGE + "\n")
    (directory / "packages").write_text("musl-1.2.5-r9\nbusybox-1.37.0-r14\n")


def test_build_reads_every_trace_and_the_containers_facts(tmp_path):
    (tmp_path / "ash.strace").write_text(TRACE)
    (tmp_path / "vim-pty.strace").write_text("7    getpid()  = 7\n")
    _write_meta(tmp_path)
    data = tomllib.loads(syscalls.build(tmp_path, tmp_path))
    assert data["meta"] == {
        "alpine": "3.21.10",
        "image": IMAGE,
        "kernel": "6.6.87.2-microsoft-standard-WSL2",
        "packages": ["busybox-1.37.0-r14", "musl-1.2.5-r9"],
        "workloads": ["ash", "vim-pty"],
    }
    assert data["syscalls"]["getpid"]["workloads"] == ["vim-pty"]
    with pytest.raises(CensusError, match="no \\*.strace files"):
        syscalls.build(tmp_path / "missing", tmp_path)


@pytest.mark.parametrize("fact", ["alpine-release", "kernel", "image", "packages"])
def test_build_refuses_a_missing_or_empty_fact(tmp_path, fact):
    (tmp_path / "ash.strace").write_text(TRACE)
    _write_meta(tmp_path)
    (tmp_path / fact).write_text("\n")
    with pytest.raises(CensusError, match=f"{fact}: missing or empty"):
        syscalls.build(tmp_path, tmp_path)
    (tmp_path / fact).unlink()
    with pytest.raises(CensusError, match=f"{fact}: missing or empty"):
        syscalls.build(tmp_path, tmp_path)


def test_the_syscall_table_is_linux_x86_64s():
    known = {"read": 0, "write": 1, "poll": 7, "clone": 56, "fork": 57, "vfork": 58,
             "execve": 59, "wait4": 61, "futex": 202, "pselect6": 270, "pipe2": 293,
             "clone3": 435}  # fmt: skip
    assert {name: X86_64[name] for name in known} == known
    assert len(set(X86_64.values())) == len(X86_64)
    for planned in syscalls.PLANNED.values():
        assert planned <= X86_64.keys()
    assert set(syscalls.KEY_ARGS) <= X86_64.keys()
    assert set(syscalls.VDSO) <= X86_64.keys()
    assert syscalls.ENOSYS_BY_DECISION <= X86_64.keys()


DISPATCH_RS = """
    match nr {
        Ok(general::__NR_read) => {
            // A mention in a comment, such as general::__NR_clone, is not dispatched.
        }
        Ok(general::__NR_poll) => {}
        Ok(general::__NR_exit | general::__NR_exit_group) => {}
        // The shape rustfmt wraps a long arm into, and a guarded arm:
        Ok(
            general::__NR_getuid
            | general::__NR_getgid
            | general::__NR_geteuid,
        ) => 1,
        Ok(general::__NR_wait4) if nr > 0 => {}
        _ => {}
    }
"""


def test_dispatched_reads_the_arms_only():
    assert syscalls.dispatched(DISPATCH_RS) == {
        "read", "poll", "exit", "exit_group", "getuid", "getgid", "geteuid", "wait4"
    }  # fmt: skip
    with pytest.raises(CensusError, match="no `Ok"):
        syscalls.dispatched("fn dispatch() {}")
    stray = DISPATCH_RS.replace("_ => {}", "Some(general::__NR_kill) => {}\n        _ => {}")
    with pytest.raises(CensusError, match="outside an arm the report can read: kill"):
        syscalls.dispatched(stray)


def test_the_planned_milestones_are_disjoint():
    milestones = [*syscalls.PLANNED.values(), syscalls.ENOSYS_BY_DECISION]
    for i, a in enumerate(milestones):
        for b in milestones[i + 1 :]:
            assert not a & b, a & b


def test_report_classifies_against_the_dispatch_table():
    implemented = syscalls.dispatched(DISPATCH_RS)
    assert classify("read", implemented) == "implemented"
    # Implemented wins over planned: poll is in M8's plan and dispatched.
    assert "poll" in syscalls.PLANNED["M8"] and classify("poll", implemented) == "implemented"
    assert classify("clone", implemented) == "M8"
    assert classify("socket", implemented) == "M9"
    assert classify("mremap", implemented) == "M10"
    assert classify("membarrier", implemented) == "ENOSYS"
    assert classify("ptrace", implemented) == "unplanned"
    data = {
        "meta": {"workloads": ["ash", "make"], "alpine": "3.21.10", "kernel": "6.6"},
        "syscalls": {
            "read": {"nr": 0, "calls": 3, "workloads": ["ash", "make"], "errors": {"EINTR": 2}},
            "clone": {"nr": 56, "calls": 1, "workloads": ["make"]},
            "fork": {"nr": 57, "calls": 1, "workloads": ["ash"]},
            "socket": {"nr": 41, "calls": 1, "workloads": ["make"]},
            "mremap": {"nr": 25, "calls": 1, "workloads": ["make"]},
            "membarrier": {"nr": 324, "calls": 1, "workloads": ["ash"], "args": {"cmd": ["0x0"]}},
            "ptrace": {"nr": 101, "calls": 1, "workloads": ["make"]},
        },
    }
    text = report(data, implemented, verbose=True)
    assert "Census of ash, make on Alpine 3.21.10 (kernel 6.6): 7 syscalls." in text
    headers = [
        "unplanned (1):",
        "ENOSYS (1):",
        "M8 (2):",
        "M9 (1):",
        "M10 (1):",
        "implemented (1):",
    ]
    at = [text.index(h) for h in headers]
    assert at == sorted(at)
    rows = [line for line in text.splitlines() if re.match(r"  +\d+ \w", line)]
    assert len(rows) == len(data["syscalls"])
    assert "    0 read                       3  ash, make" in rows
    assert "        cmd: 0x0" in text
    assert "        errors: EINTR=2" in text
    assert "clock_gettime, gettimeofday, time, getcpu" in text
    quiet = report(data, implemented)
    assert "        cmd:" not in quiet and "        errors:" not in quiet


def test_main_reports_the_checked_in_census_by_default(capsys):
    assert syscalls.main(["report", "--verbose"]) == 0
    out = capsys.readouterr().out
    count = len(tomllib.loads(CENSUS.read_text())["syscalls"])
    assert f": {count} syscalls." in out
    assert len([line for line in out.splitlines() if re.match(r"  +\d+ \w", line)]) == count
    assert "implemented (" in out and "        errors: " in out


def test_main_reports_the_census_it_is_given(tmp_path, capsys):
    census_file = tmp_path / "census.toml"
    census_file.write_text(
        '[meta]\nalpine = "3.21.10"\nkernel = "6.6"\nworkloads = ["ash"]\n\n'
        '[syscalls.getpid]\nnr = 39\ncalls = 2\nworkloads = ["ash"]\n'
    )
    assert syscalls.main(["report", "--census", str(census_file)]) == 0
    out = capsys.readouterr().out
    assert "Census of ash on Alpine 3.21.10 (kernel 6.6): 1 syscalls." in out
    assert "   39 getpid                     2  ash" in out


def test_main_builds_the_census_file(tmp_path):
    (tmp_path / "ash.strace").write_text(TRACE)
    _write_meta(tmp_path)
    out = tmp_path / "out.toml"
    assert (
        syscalls.main(["build", "--traces", str(tmp_path), "--meta", str(tmp_path), "-o", str(out)])
        == 0
    )
    assert out.read_text() == syscalls.build(tmp_path, tmp_path)


def test_main_fails_with_a_message_on_a_census_error(tmp_path, capsys):
    out = tmp_path / "out.toml"
    assert (
        syscalls.main(["build", "--traces", str(tmp_path), "--meta", str(tmp_path), "-o", str(out)])
        == 1
    )
    assert "no *.strace files" in capsys.readouterr().err
    assert not out.exists()
    rs = tmp_path / "mod.rs"
    rs.write_text("fn dispatch() {}\n")
    assert syscalls.main(["report", "--dispatch", str(rs)]) == 1
    assert "no `Ok(general::__NR_<name>) =>` arms" in capsys.readouterr().err


def _script_workloads() -> set[str]:
    script = WORKLOADS_SH.read_text()
    names = set(re.findall(r"^trace (\S+) ", script, re.MULTILINE))
    names |= set(re.findall(r'"\$OUT/([\w-]+)\.strace"', script))
    return names


def test_the_checked_in_census_matches_the_table_and_the_script():
    data = tomllib.loads(CENSUS.read_text())
    meta = data["meta"]
    assert meta["alpine"].startswith("3.21.")
    pin = re.search(r"^IMAGE=(\S+)$", CENSUS_SH.read_text(), re.MULTILINE)
    assert pin and meta["image"] == pin.group(1)
    assert meta["kernel"] and meta["packages"]
    assert set(meta["workloads"]) == _script_workloads()
    names = list(data["syscalls"])
    assert names == sorted(names, key=X86_64.__getitem__)
    for name, entry in data["syscalls"].items():
        assert entry["nr"] == X86_64[name], name
        assert entry["calls"] > 0, name
        assert set(entry["workloads"]) <= set(meta["workloads"]), name


LINUX_MOD_RS = ROOT / "kernel/src/linux/mod.rs"
DISPATCH_FILE = ROOT / "kernel/src/linux/syscall/mod.rs"

TABLES_RS = """
pub const M6_SYSCALLS: &[(u32, &str)] = &[
    (general::__NR_read, "read"),
    // a comment: (general::__NR_write, "write"),
    (general::__NR_exit, "exit"),
];
pub const M8_SYSCALLS: &[(u32, &str)] = &[(general::__NR_fork, "fork"), (general::__NR_pipe, "pipe")];
"""


def test_kernel_tables_reads_each_milestones_rows():
    assert syscalls.kernel_tables(TABLES_RS) == {"M6": ["read", "exit"], "M8": ["fork", "pipe"]}
    with pytest.raises(CensusError, match="no `pub const"):
        syscalls.kernel_tables("fn main() {}")
    with pytest.raises(CensusError, match="a shape this can't read: M9_SYSCALLS"):
        syscalls.kernel_tables(TABLES_RS + "pub const M9_SYSCALLS: &[(u32, &'static str)] = &[];")
    with pytest.raises(CensusError, match="__NR_fork named 'vfork'"):
        syscalls.kernel_tables(TABLES_RS.replace('"fork"', '"vfork"'))
    with pytest.raises(CensusError, match="not a row: 'general::__NR_pipe'"):
        syscalls.kernel_tables(
            TABLES_RS.replace('(general::__NR_pipe, "pipe")', "general::__NR_pipe")
        )
    bogus = TABLES_RS.replace('__NR_fork, "fork"', '__NR_frok, "frok"')
    with pytest.raises(CensusError, match="'frok' isn't an x86-64 syscall"):
        syscalls.kernel_tables(bogus)


def test_m8_table_covers_the_census():
    """ARCHITECTURE section 8: every syscall the M8 census (#337) saw is in
    M6_SYSCALLS, M7_SYSCALLS or M8_SYSCALLS, or -ENOSYS by a recorded
    decision (ENOSYS_BY_DECISION); M8's table is the plan's (PLANNED["M8"]),
    each of its numbers dispatched, and no decided -ENOSYS one dispatched or
    in a table."""
    tables = syscalls.kernel_tables(LINUX_MOD_RS.read_text())
    assert set(tables) == {"M6", "M7", "M8"}
    rows = [name for names in tables.values() for name in names]
    assert len(rows) == len(set(rows)), "a syscall in two tables, or twice in one"
    assert (len(tables["M6"]), len(tables["M7"]), len(tables["M8"])) == (30, 47, 60)

    census_names = tomllib.loads(CENSUS.read_text())["syscalls"].keys()
    assert syscalls.uncovered(census_names, tables) == []
    # The check could have failed: a census syscall in no table and no
    # decision is reported, and so is one a table lost.
    assert syscalls.uncovered([*census_names, "socket", "mremap"], tables) == ["mremap", "socket"]
    without_pipe = {**tables, "M8": [n for n in tables["M8"] if n != "pipe"]}
    assert syscalls.uncovered(census_names, without_pipe) == ["pipe"]

    assert set(tables["M8"]) == syscalls.PLANNED["M8"] == syscalls.M8_ARGS.keys()
    assert not syscalls.ENOSYS_BY_DECISION & set(rows)
    implemented = syscalls.dispatched(DISPATCH_FILE.read_text())
    assert set(tables["M8"]) <= implemented
    assert not syscalls.ENOSYS_BY_DECISION & implemented
    # The two the census saw are among the decisions, which are section 8's
    # "Stays `-ENOSYS` in M8" list.
    assert {"sendfile", "membarrier"} <= census_names & syscalls.ENOSYS_BY_DECISION
    assert syscalls.ENOSYS_BY_DECISION == {
        "sendfile", "membarrier", "clone3", "execveat", "futex_waitv", "sched_setparam",
        "sched_getparam", "sched_setscheduler", "sched_getscheduler", "sched_get_priority_max",
        "sched_get_priority_min", "sched_rr_get_interval", "signalfd", "signalfd4", "times",
    }  # fmt: skip


ARMS_RS = """
    match nr {
        Ok(general::__NR_read) => {
            frame.rax = fd::sys_read(reg(frame.rdi), reg(frame.rsi), reg(frame.rdx));
            SyscallReturn::Sysret
        }
        Ok(general::__NR_exit | general::__NR_exit_group) => task::sys_exit(frame),
        Ok(general::__NR_getuid | general::__NR_getgid) => {
            frame.rax = 0;
            SyscallReturn::Sysret
        }
        Ok(general::__NR_execve) => task::sys_execve(reg(frame.rdi), reg(frame.rsi), reg(frame.rdx), frame),
        Ok(general::__NR_clone) => {
            // The frame's registers: frame.r9 in a comment isn't read.
            frame.rax = task::sys_clone(reg(frame.rdi), reg(frame.rsi), reg(frame.rdx), reg(frame.r10), reg(frame.r8), frame);
            SyscallReturn::Sysret
        }
        _ => {
            log_unknown_once(frame.r9);
        }
    }
"""


def test_dispatch_arms_reads_each_arms_handlers_and_registers():
    five = ("rdi", "rsi", "rdx", "r10", "r8")
    assert syscalls.dispatch_arms(ARMS_RS) == [
        syscalls.Arm(("read",), ("sys_read",), ("rdi", "rsi", "rdx")),
        syscalls.Arm(("exit", "exit_group"), ("sys_exit",), ()),
        syscalls.Arm(("getuid", "getgid"), (), ()),
        syscalls.Arm(("execve",), ("sys_execve",), ("rdi", "rsi", "rdx")),
        syscalls.Arm(("clone",), ("sys_clone",), five),
    ]
    assert syscalls.arm_problems(syscalls.dispatch_arms(ARMS_RS)) == []


EXECVE_ARM = "task::sys_execve(reg(frame.rdi), reg(frame.rsi), reg(frame.rdx), frame),"


@pytest.mark.parametrize(
    ("old", "new", "problems"),
    [
        ("fd::sys_read(", "fd::sys_write(", ["read: calls sys_write"]),
        ("reg(frame.rsi), reg(frame.rdx));", "reg(frame.rdx), reg(frame.rsi));", ["read: reads rdi, rdx, rsi"]),
        (", reg(frame.r8), frame)", ", frame)", ["clone: reads 4 registers, not 5"]),
        (", reg(frame.r8), frame)", ", reg(frame.r8), reg(frame.r9), frame)", ["clone: reads 6 registers, not 5"]),
        (EXECVE_ARM, "{ frame.rax = 0; SyscallReturn::Sysret }", ["execve: calls no handler", "execve: reads 0 registers, not 3"]),
        ("__NR_execve)", "__NR_execve | general::__NR_execveat)", ["execve: shares its arm with execveat"]),
        ("frame.rax = 0;", "frame.rax = proc::sys_uname(frame.rdi);", ["getuid: calls sys_uname"]),
    ],
)  # fmt: skip
def test_arm_problems_catch_a_wrong_handler_or_register(old, new, problems):
    assert old in ARMS_RS
    assert syscalls.arm_problems(syscalls.dispatch_arms(ARMS_RS.replace(old, new, 1))) == problems


def test_each_arm_calls_its_own_handler_with_its_registers_in_order():
    """ARCHITECTURE section 8: each dispatch arm passes the registers of its
    syscall's Linux signature, in order, to the handler named after it (but
    M6's two inline arms); an M8 syscall's arm is its own, and reads Linux's
    argument count."""
    assert syscalls.arm_problems(syscalls.dispatch_arms(DISPATCH_FILE.read_text())) == []


STUBS_RS = """
/// `pipe2(fds, flags)` (293): M8 stub, replaced by C7 (#386).
pub(super) fn sys_pipe2(_fds: UserPtr, _flags: OpenFlags) -> u64 {
    not_implemented(general::__NR_pipe2)
}

/// `execve(path, argv, envp)` (59): M8 stub, replaced by C2 (#381). It
/// returns through the frame.
pub(super) fn sys_execve(
    _path: UserPtr,
    frame: &mut SyscallFrame,
) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_execve);
    SyscallReturn::Sysret
}

/// `getpid()` (39): not a stub.
pub(super) fn sys_getpid(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = 1;
    SyscallReturn::Sysret
}
"""
STUBS_DESIGN = {"pipe2": ("C7",), "execve": ("C2",), "getpid": ("B1",)}
STUBS_ISSUES = {"C7": 386, "C2": 381, "B1": 372}


def test_stubs_reads_each_stub_and_its_doc():
    found = syscalls.stubs([STUBS_RS])
    assert set(found) == {"pipe2", "execve"}
    assert found["pipe2"].body == "not_implemented(general::__NR_pipe2)"
    assert found["execve"].doc.endswith("replaced by C2 (#381). It returns through the frame.")
    assert syscalls.stub_problems(found, STUBS_DESIGN, STUBS_ISSUES) == []
    # Any visibility is read; a handler of a shape it can't read is refused.
    assert set(
        syscalls.stubs([STUBS_RS.replace("pub(super) fn sys_pipe2", "pub(crate) fn sys_pipe2")])
    ) == {"pipe2", "execve"}
    one_line = (
        STUBS_RS + "\npub(super) fn sys_pause() -> u64 { not_implemented(general::__NR_pause) }\n"
    )
    with pytest.raises(CensusError, match="a handler of a shape this can't read: sys_pause"):
        syscalls.stubs([one_line])


@pytest.mark.parametrize(
    ("old", "new", "problem"),
    [
        ("not_implemented(general::__NR_pipe2)", "0", "pipe2: body '0'"),
        ("not_implemented(general::__NR_pipe2)", "not_implemented(general::__NR_pipe)", "pipe2: body"),
        ("not_implemented(general::__NR_pipe2)", "neg_errno(errno::ENOSYS)", "pipe2: body"),
        ("frame.rax = not_implemented(general::__NR_execve);", "frame.rax = 0;", "execve: body"),
        ("C7 (#386)", "C13 (#392)", "pipe2: its doc names C13 (#392), the design (('C7', '386'),)"),
        ("C7 (#386)", "C7 (#999)", "pipe2: its doc names C7 (#999)"),
        ("C7 (#386)", "C7 (#386) and C1 (#380)", "pipe2: its doc names C7 (#386), C1 (#380)"),
        (", replaced by C7 (#386).", ".", "pipe2: its doc names none"),
    ],
)  # fmt: skip
def test_stub_problems_catch_a_stub_doing_more_or_naming_other_issues(old, new, problem):
    assert old in STUBS_RS
    found = syscalls.stubs([STUBS_RS.replace(old, new, 1)])
    problems = syscalls.stub_problems(found, STUBS_DESIGN, STUBS_ISSUES)
    assert len(problems) == 1 and problems[0].startswith(problem), problems


ARCHITECTURE_MD = ROOT / "docs/ARCHITECTURE.md"
# M8's issue labels and their GitHub issues (filed from ARCHITECTURE section 8's
# "M8's issues", epic #335).
# fmt: off
M8_ISSUES = {
    "B0": 371, "B1": 372, "B2": 373, "B3": 374, "B4": 375, "B5": 376, "B6": 377, "B7": 378,
    "B8": 379, "C1": 380, "C2": 381, "C3": 382, "C4": 383, "C5": 384, "C6": 385, "C7": 386,
    "C8": 387, "C9": 388, "C10": 389, "C11": 390, "C12": 391, "C13": 392, "D1": 393,
    "D2": 394, "D3": 395, "D4": 396,
}
# fmt: on


def test_design_issues_reads_the_m8_tables_last_column():
    design = syscalls.design_issues(ARCHITECTURE_MD.read_text())
    tables = syscalls.kernel_tables(LINUX_MOD_RS.read_text())
    assert set(tables["M8"]) <= design.keys()
    assert design["clone"] == ("C1", "C3")
    assert design["setfsuid"] == design["setfsgid"] == ("C12",)
    assert design["sendfile"] == ()
    with pytest.raises(CensusError, match="row 57 names vfork"):
        syscalls.design_issues(
            ARCHITECTURE_MD.read_text().replace("| 57 | `fork()`", "| 57 | `vfork()`")
        )
    with pytest.raises(CensusError, match="no M8 syscall table"):
        syscalls.design_issues("# nothing")


def test_each_m8_stub_returns_enosys_and_names_its_issue():
    """ARCHITECTURE section 8, "B0's stubs": a stub (its doc says "M8 stub")
    only returns `not_implemented`'s -ENOSYS for its own number, and its doc
    names the issues the design's table names for it; every stub is an M8
    syscall's."""
    found = syscalls.stubs(syscalls.cluster_sources(DISPATCH_FILE.parent))
    design = syscalls.design_issues(ARCHITECTURE_MD.read_text())
    assert syscalls.stub_problems(found, design, M8_ISSUES) == []
    tables = syscalls.kernel_tables(LINUX_MOD_RS.read_text())
    assert set(found) <= set(tables["M8"])


def test_the_report_lists_a_stub_under_its_milestone(capsys):
    stubbed = set(syscalls.stubs(syscalls.cluster_sources(DISPATCH_FILE.parent)))
    assert syscalls.main(["report"]) == 0
    sections = re.split(r"\n\n", capsys.readouterr().out)
    implemented = next(s for s in sections if s.startswith("implemented ("))
    m8 = next((s for s in sections if s.startswith("M8 (")), "")
    for name in stubbed & tomllib.loads(CENSUS.read_text())["syscalls"].keys():
        assert re.search(rf"^ +\d+ {name} ", m8, re.MULTILINE), name
        assert not re.search(rf"^ +\d+ {name} ", implemented, re.MULTILINE), name
