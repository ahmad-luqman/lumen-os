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
    report,
    split_args,
)

ROOT = Path(__file__).resolve().parents[2]
CENSUS = ROOT / "tests/linux/m8-census.toml"
WORKLOADS_SH = ROOT / "tools/census/workloads.sh"

# Lines in the shapes `strace -f -qq -X raw -s 0 -o FILE` writes (taken from
# a real trace, renumbered): two pids interleaving unfinished and resumed calls.
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
    ]
    newfstatat = calls[4]
    assert newfstatat.args == ["-100", '"/usr/local/bin/cat"', "0xfffffded4390", "0"]
    assert (newfstatat.ret, newfstatat.errno) == ("-1", "ENOENT")
    assert calls[6].ret == "?" and calls[6].errno is None
    assert calls[7].args[0] == "-1" and calls[7].ret == "13"


def test_a_call_unfinished_when_the_trace_ends_is_dropped():
    calls = parse_trace(TRACE.splitlines())
    assert "futex" not in [c.name for c in calls]


def test_a_resumption_of_another_call_than_the_pending_one_is_refused():
    lines = ["5    read(3,  <unfinished ...>", "5    <... write resumed>) = 1"]
    with pytest.raises(CensusError, match="write resumed while read was pending"):
        parse_trace(lines)


def test_a_success_strace_annotates_is_kept_and_has_no_errno():
    lines = [
        "7    fcntl(3, 0x3)                     = 0x8000 (flags 0x8000)",
        "7    pselect6(4, [3], NULL, NULL, {tv_sec=0, tv_nsec=0}, NULL) = 0 (Timeout)",
        "7    poll([{fd=0, events=0x1}], 1, -1) = 1 ([{fd=0, revents=0x1}])",
        "7    rt_sigsuspend([], 8)              = ? ERESTARTNOHAND (To be restarted if no handler)",
        "7    exit_group(0)                     = ?",
    ]
    calls = parse_trace(lines)
    assert [(c.name, c.ret, c.errno) for c in calls] == [
        ("fcntl", "0x8000", None),
        ("pselect6", "0", None),
        ("poll", "1", None),
        ("rt_sigsuspend", "?", "ERESTARTNOHAND"),
        ("exit_group", "?", None),
    ]
    assert key_values(calls[0]) == {"cmd": "0x3"}


@pytest.mark.parametrize(
    ("line", "match"),
    [
        ('open("/x", 0) = 3', "not an strace -f line"),
        ("7    write(1, ..., 4", "not a completed syscall"),
        ('7    write(1, "", 4) = ', "not a completed syscall"),
    ],
)
def test_a_line_the_parser_cant_read_is_refused(line, match):
    with pytest.raises(CensusError, match=match):
        parse_trace([line])


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
    # A position past the arguments strace printed records nothing.
    assert key_values(Call("futex", ["0x7f00"], "0", None)) == {}
    assert key_values(Call("getpid", [], "1", None)) == {}


def test_census_counts_calls_workloads_arguments_and_errors():
    seen = census({"ash": parse_trace(TRACE.splitlines()), "make": parse_trace(TRACE.splitlines())})
    ioctl = seen["ioctl"]
    assert ioctl.calls == 2
    assert ioctl.workloads == {"ash", "make"}
    assert ioctl.args == {"request": {"0x5413"}}
    assert ioctl.errors == {"ENOTTY": 2}
    assert "futex" not in seen


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


def test_build_reads_every_trace_and_the_containers_facts(tmp_path):
    (tmp_path / "ash.strace").write_text(TRACE)
    (tmp_path / "vim-pty.strace").write_text("7    getpid()  = 7\n")
    (tmp_path / "alpine-release").write_text("3.21.10\n")
    (tmp_path / "kernel").write_text("6.6.87.2-microsoft-standard-WSL2\n")
    (tmp_path / "packages").write_text("musl-1.2.5-r9\nbusybox-1.37.0-r14\n")
    data = tomllib.loads(syscalls.build(tmp_path, tmp_path))
    assert data["meta"]["workloads"] == ["ash", "vim-pty"]
    assert data["meta"]["packages"] == ["busybox-1.37.0-r14", "musl-1.2.5-r9"]
    assert data["meta"]["image"] == "unknown"
    assert data["syscalls"]["getpid"]["workloads"] == ["vim-pty"]
    with pytest.raises(CensusError, match="no \\*.strace files"):
        syscalls.build(tmp_path / "missing", tmp_path)


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


def test_report_classifies_against_the_dispatch_table():
    implemented = syscalls.dispatched("general::__NR_read => .., general::__NR_poll => ..")
    assert implemented == {"read", "poll"}
    assert classify("read", implemented) == "implemented"
    assert classify("clone", implemented) == "M8"
    assert classify("socket", implemented) == "M9"
    assert classify("mremap", implemented) == "M10"
    assert classify("membarrier", implemented) == "unplanned"
    data = {
        "meta": {"workloads": ["ash"], "alpine": "3.21.10", "kernel": "6.6"},
        "syscalls": {
            "read": {"nr": 0, "calls": 3, "workloads": ["ash"]},
            "membarrier": {"nr": 324, "calls": 1, "workloads": ["ash"], "args": {"cmd": ["0x0"]}},
        },
    }
    text = report(data, implemented, verbose=True)
    assert text.index("unplanned (1):") < text.index("implemented (1):")
    assert "cmd: 0x0" in text
    assert "clock_gettime, gettimeofday, time, getcpu" in text


def _script_workloads() -> set[str]:
    script = WORKLOADS_SH.read_text()
    names = set(re.findall(r"^trace (\S+) ", script, re.MULTILINE))
    names |= set(re.findall(r'"\$OUT/([\w-]+)\.strace"', script))
    return names


def test_the_checked_in_census_matches_the_table_and_the_script():
    data = tomllib.loads(CENSUS.read_text())
    meta = data["meta"]
    assert meta["alpine"].startswith("3.21.")
    assert set(meta["workloads"]) == _script_workloads()
    names = list(data["syscalls"])
    assert names == sorted(names, key=X86_64.__getitem__)
    for name, entry in data["syscalls"].items():
        assert entry["nr"] == X86_64[name], name
        assert entry["calls"] > 0, name
        assert set(entry["workloads"]) <= set(meta["workloads"]), name
