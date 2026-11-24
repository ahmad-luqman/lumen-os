"""The M8 syscall census (#337): which Linux syscalls the M8 gate workloads make.

`tools/syscall-census.sh` runs the workloads under `strace -f -X raw` on stock
Alpine (x86-64) and calls `build` here on the traces, inside the container: so
this module uses the standard library only (no uv, no harness dependencies,
Python 3.12) and imports nothing else from `lumen_harness`. The result,
`tests/linux/m8-census.toml`, is checked in. `report` compares it with the
syscalls the kernel dispatches and the milestones the roadmap plans them for.

    python3 syscalls.py build --traces DIR --meta DIR -o m8-census.toml
    PYTHONPATH=tools/harness uv run python -m lumen_harness.syscalls report [--verbose]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# x86-64 syscall numbers, generated from linux-raw-sys 0.12.1's
# src/x86_64/general.rs (the crate the kernel takes its numbers from).
# fmt: off
X86_64: dict[str, int] = {
    "read": 0, "write": 1, "open": 2, "close": 3, "stat": 4, "fstat": 5, "lstat": 6, "poll": 7,
    "lseek": 8, "mmap": 9, "mprotect": 10, "munmap": 11, "brk": 12, "rt_sigaction": 13,
    "rt_sigprocmask": 14, "rt_sigreturn": 15, "ioctl": 16, "pread64": 17, "pwrite64": 18,
    "readv": 19, "writev": 20, "access": 21, "pipe": 22, "select": 23, "sched_yield": 24,
    "mremap": 25, "msync": 26, "mincore": 27, "madvise": 28, "shmget": 29, "shmat": 30,
    "shmctl": 31, "dup": 32, "dup2": 33, "pause": 34, "nanosleep": 35, "getitimer": 36,
    "alarm": 37, "setitimer": 38, "getpid": 39, "sendfile": 40, "socket": 41, "connect": 42,
    "accept": 43, "sendto": 44, "recvfrom": 45, "sendmsg": 46, "recvmsg": 47, "shutdown": 48,
    "bind": 49, "listen": 50, "getsockname": 51, "getpeername": 52, "socketpair": 53,
    "setsockopt": 54, "getsockopt": 55, "clone": 56, "fork": 57, "vfork": 58, "execve": 59,
    "exit": 60, "wait4": 61, "kill": 62, "uname": 63, "semget": 64, "semop": 65, "semctl": 66,
    "shmdt": 67, "msgget": 68, "msgsnd": 69, "msgrcv": 70, "msgctl": 71, "fcntl": 72, "flock": 73,
    "fsync": 74, "fdatasync": 75, "truncate": 76, "ftruncate": 77, "getdents": 78, "getcwd": 79,
    "chdir": 80, "fchdir": 81, "rename": 82, "mkdir": 83, "rmdir": 84, "creat": 85, "link": 86,
    "unlink": 87, "symlink": 88, "readlink": 89, "chmod": 90, "fchmod": 91, "chown": 92,
    "fchown": 93, "lchown": 94, "umask": 95, "gettimeofday": 96, "getrlimit": 97, "getrusage": 98,
    "sysinfo": 99, "times": 100, "ptrace": 101, "getuid": 102, "syslog": 103, "getgid": 104,
    "setuid": 105, "setgid": 106, "geteuid": 107, "getegid": 108, "setpgid": 109, "getppid": 110,
    "getpgrp": 111, "setsid": 112, "setreuid": 113, "setregid": 114, "getgroups": 115,
    "setgroups": 116, "setresuid": 117, "getresuid": 118, "setresgid": 119, "getresgid": 120,
    "getpgid": 121, "setfsuid": 122, "setfsgid": 123, "getsid": 124, "capget": 125, "capset": 126,
    "rt_sigpending": 127, "rt_sigtimedwait": 128, "rt_sigqueueinfo": 129, "rt_sigsuspend": 130,
    "sigaltstack": 131, "utime": 132, "mknod": 133, "uselib": 134, "personality": 135,
    "ustat": 136, "statfs": 137, "fstatfs": 138, "sysfs": 139, "getpriority": 140,
    "setpriority": 141, "sched_setparam": 142, "sched_getparam": 143, "sched_setscheduler": 144,
    "sched_getscheduler": 145, "sched_get_priority_max": 146, "sched_get_priority_min": 147,
    "sched_rr_get_interval": 148, "mlock": 149, "munlock": 150, "mlockall": 151, "munlockall": 152,
    "vhangup": 153, "modify_ldt": 154, "pivot_root": 155, "_sysctl": 156, "prctl": 157,
    "arch_prctl": 158, "adjtimex": 159, "setrlimit": 160, "chroot": 161, "sync": 162, "acct": 163,
    "settimeofday": 164, "mount": 165, "umount2": 166, "swapon": 167, "swapoff": 168,
    "reboot": 169, "sethostname": 170, "setdomainname": 171, "iopl": 172, "ioperm": 173,
    "create_module": 174, "init_module": 175, "delete_module": 176, "get_kernel_syms": 177,
    "query_module": 178, "quotactl": 179, "nfsservctl": 180, "getpmsg": 181, "putpmsg": 182,
    "afs_syscall": 183, "tuxcall": 184, "security": 185, "gettid": 186, "readahead": 187,
    "setxattr": 188, "lsetxattr": 189, "fsetxattr": 190, "getxattr": 191, "lgetxattr": 192,
    "fgetxattr": 193, "listxattr": 194, "llistxattr": 195, "flistxattr": 196, "removexattr": 197,
    "lremovexattr": 198, "fremovexattr": 199, "tkill": 200, "time": 201, "futex": 202,
    "sched_setaffinity": 203, "sched_getaffinity": 204, "set_thread_area": 205, "io_setup": 206,
    "io_destroy": 207, "io_getevents": 208, "io_submit": 209, "io_cancel": 210,
    "get_thread_area": 211, "lookup_dcookie": 212, "epoll_create": 213, "epoll_ctl_old": 214,
    "epoll_wait_old": 215, "remap_file_pages": 216, "getdents64": 217, "set_tid_address": 218,
    "restart_syscall": 219, "semtimedop": 220, "fadvise64": 221, "timer_create": 222,
    "timer_settime": 223, "timer_gettime": 224, "timer_getoverrun": 225, "timer_delete": 226,
    "clock_settime": 227, "clock_gettime": 228, "clock_getres": 229, "clock_nanosleep": 230,
    "exit_group": 231, "epoll_wait": 232, "epoll_ctl": 233, "tgkill": 234, "utimes": 235,
    "vserver": 236, "mbind": 237, "set_mempolicy": 238, "get_mempolicy": 239, "mq_open": 240,
    "mq_unlink": 241, "mq_timedsend": 242, "mq_timedreceive": 243, "mq_notify": 244,
    "mq_getsetattr": 245, "kexec_load": 246, "waitid": 247, "add_key": 248, "request_key": 249,
    "keyctl": 250, "ioprio_set": 251, "ioprio_get": 252, "inotify_init": 253,
    "inotify_add_watch": 254, "inotify_rm_watch": 255, "migrate_pages": 256, "openat": 257,
    "mkdirat": 258, "mknodat": 259, "fchownat": 260, "futimesat": 261, "newfstatat": 262,
    "unlinkat": 263, "renameat": 264, "linkat": 265, "symlinkat": 266, "readlinkat": 267,
    "fchmodat": 268, "faccessat": 269, "pselect6": 270, "ppoll": 271, "unshare": 272,
    "set_robust_list": 273, "get_robust_list": 274, "splice": 275, "tee": 276,
    "sync_file_range": 277, "vmsplice": 278, "move_pages": 279, "utimensat": 280,
    "epoll_pwait": 281, "signalfd": 282, "timerfd_create": 283, "eventfd": 284, "fallocate": 285,
    "timerfd_settime": 286, "timerfd_gettime": 287, "accept4": 288, "signalfd4": 289,
    "eventfd2": 290, "epoll_create1": 291, "dup3": 292, "pipe2": 293, "inotify_init1": 294,
    "preadv": 295, "pwritev": 296, "rt_tgsigqueueinfo": 297, "perf_event_open": 298,
    "recvmmsg": 299, "fanotify_init": 300, "fanotify_mark": 301, "prlimit64": 302,
    "name_to_handle_at": 303, "open_by_handle_at": 304, "clock_adjtime": 305, "syncfs": 306,
    "sendmmsg": 307, "setns": 308, "getcpu": 309, "process_vm_readv": 310,
    "process_vm_writev": 311, "kcmp": 312, "finit_module": 313, "sched_setattr": 314,
    "sched_getattr": 315, "renameat2": 316, "seccomp": 317, "getrandom": 318, "memfd_create": 319,
    "kexec_file_load": 320, "bpf": 321, "execveat": 322, "userfaultfd": 323, "membarrier": 324,
    "mlock2": 325, "copy_file_range": 326, "preadv2": 327, "pwritev2": 328, "pkey_mprotect": 329,
    "pkey_alloc": 330, "pkey_free": 331, "statx": 332, "io_pgetevents": 333, "rseq": 334,
    "uretprobe": 335, "pidfd_send_signal": 424, "io_uring_setup": 425, "io_uring_enter": 426,
    "io_uring_register": 427, "open_tree": 428, "move_mount": 429, "fsopen": 430, "fsconfig": 431,
    "fsmount": 432, "fspick": 433, "pidfd_open": 434, "clone3": 435, "close_range": 436,
    "openat2": 437, "pidfd_getfd": 438, "faccessat2": 439, "process_madvise": 440,
    "epoll_pwait2": 441, "mount_setattr": 442, "quotactl_fd": 443, "landlock_create_ruleset": 444,
    "landlock_add_rule": 445, "landlock_restrict_self": 446, "memfd_secret": 447,
    "process_mrelease": 448, "futex_waitv": 449, "set_mempolicy_home_node": 450, "cachestat": 451,
    "fchmodat2": 452, "map_shadow_stack": 453, "futex_wake": 454, "futex_wait": 455,
    "futex_requeue": 456, "statmount": 457, "listmount": 458, "lsm_get_self_attr": 459,
    "lsm_set_self_attr": 460, "lsm_list_modules": 461, "mseal": 462, "setxattrat": 463,
    "getxattrat": 464, "listxattrat": 465, "removexattrat": 466, "open_tree_attr": 467,
    "file_getattr": 468, "file_setattr": 469,
}
# fmt: on
NAMES: dict[int, str] = {n: s for s, n in X86_64.items()}

# The arguments recorded per syscall, as (label, position, field):
# - position alone: that argument's value (`futex`'s op);
# - field alone: a named argument anywhere (`clone(child_stack=NULL, flags=17)`);
# - both: a field inside that argument (`rt_sigaction`'s new action's sa_flags,
#   never the old action's).
KEY_ARGS: dict[str, tuple[tuple[str, int | None, str | None], ...]] = {
    "arch_prctl": (("code", 0, None),),
    "clock_getres": (("clock", 0, None),),
    "clock_gettime": (("clock", 0, None),),
    "clock_nanosleep": (("clock", 0, None), ("flags", 1, None)),
    "clone": (("flags", None, "flags"),),
    "clone3": (("flags", 0, "flags"),),
    "dup3": (("flags", 2, None),),
    "faccessat2": (("flags", 3, None),),
    "fcntl": (("cmd", 1, None),),
    "flock": (("operation", 1, None),),
    "futex": (("op", 1, None),),
    "getrandom": (("flags", 2, None),),
    "ioctl": (("request", 1, None),),
    "kill": (("signal", 1, None),),
    "lseek": (("whence", 2, None),),
    "madvise": (("advice", 2, None),),
    "membarrier": (("cmd", 0, None),),
    "mmap": (("prot", 2, None), ("flags", 3, None)),
    "mprotect": (("prot", 2, None),),
    "mremap": (("flags", 3, None),),
    "msync": (("flags", 2, None),),
    "newfstatat": (("flags", 3, None),),
    "open": (("flags", 1, None),),
    "openat": (("flags", 2, None),),
    "personality": (("persona", 0, None),),
    "pipe2": (("flags", 1, None),),
    "prctl": (("option", 0, None),),
    "prlimit64": (("resource", 1, None),),
    "renameat2": (("flags", 4, None),),
    "rt_sigaction": (("signal", 0, None), ("sa_flags", 1, "sa_flags")),
    "rt_sigprocmask": (("how", 0, None),),
    "rt_sigqueueinfo": (("signal", 1, None),),
    "socket": (("domain", 0, None), ("type", 1, None)),
    "statx": (("flags", 2, None),),
    "tgkill": (("signal", 2, None),),
    "tkill": (("signal", 1, None),),
    "unlinkat": (("flags", 2, None),),
    "utimensat": (("flags", 3, None),),
    "wait4": (("options", 2, None),),
    "waitid": (("idtype", 0, None), ("options", 3, None)),
}

# Labels shown in decimal; every other value (flags, ops, requests) in hex.
DECIMAL_LABELS = frozenset({"signal", "clock", "resource", "domain", "idtype", "whence", "code"})

# The milestones that plan each syscall: M8 as the M8 epic (#335) scopes it
# (ROADMAP M8's rows plus the plan's waitid, getpgid, getsid, priorities and
# the signal waits), M9 and M10 from ROADMAP's rows.
# fmt: off
PLANNED: dict[str, frozenset[str]] = {
    "M8": frozenset({
        "fork", "vfork", "clone", "execve", "wait4", "waitid", "futex", "nanosleep",
        "clock_nanosleep", "clock_getres", "getrusage", "prctl", "statfs", "fstatfs",
        "rt_sigreturn", "sigaltstack", "kill", "tkill", "tgkill", "rt_sigpending", "rt_sigsuspend",
        "rt_sigtimedwait", "pipe", "pipe2", "dup", "dup2", "dup3", "poll", "ppoll", "select",
        "pselect6", "setsid", "setpgid", "getpgid", "getsid", "sched_yield", "sched_getaffinity",
        "sched_setaffinity", "getpriority", "setpriority",
    }),
    "M9": frozenset({
        "socket", "socketpair", "bind", "listen", "accept", "accept4", "connect", "sendto",
        "recvfrom", "sendmsg", "recvmsg", "shutdown", "getsockname", "getpeername", "setsockopt",
        "getsockopt", "epoll_create", "epoll_create1", "epoll_ctl", "epoll_wait", "epoll_pwait",
        "epoll_pwait2", "eventfd", "eventfd2", "timerfd_create", "timerfd_settime",
        "timerfd_gettime",
    }),
    "M10": frozenset({
        "mremap", "madvise", "memfd_create", "inotify_init", "inotify_init1", "inotify_add_watch",
        "inotify_rm_watch", "sysinfo", "rseq", "io_uring_setup", "io_uring_enter",
        "io_uring_register",
    }),
}
# fmt: on

# Calls Linux answers from the vDSO, so strace never sees them; Lumen maps no
# vDSO, so musl makes them as syscalls there. The report lists them apart.
VDSO = ("clock_gettime", "gettimeofday", "time", "getcpu")


class CensusError(Exception):
    """A trace this census can't stand behind (an unknown syscall name, such
    as the `syscall_0x...` lines strace prints under Rosetta's translation)."""


@dataclass
class Call:
    """One completed syscall from a trace."""

    name: str
    args: list[str]
    ret: str
    errno: str | None


@dataclass
class Seen:
    """What the census keeps per syscall."""

    calls: int = 0
    workloads: set[str] = field(default_factory=set)
    args: dict[str, set[str]] = field(default_factory=dict)
    errors: dict[str, int] = field(default_factory=dict)


_LINE = re.compile(r"^(\d+)\s+(.*)$")
_UNFINISHED = re.compile(r"^(\w+)\((.*) <unfinished \.\.\.>$")
_RESUMED = re.compile(r"^<\.\.\. (\w+) resumed>(.*)$")
# A completed call: its arguments, its return value, then whatever strace adds
# after it: an errno (`-1 ENOENT (...)`, `? ERESTARTNOHAND (...)`) or a note
# on a success (`0x8000 (flags 0x8000)`, `0 (Timeout)`, `1 ([{fd=0, ...}])`).
_COMPLETE = re.compile(r"^(\w+)\((.*)\)\s+=\s+(\S+)(?:\s+(.*))?$")
_ERRNO = re.compile(r"^(E[A-Z0-9]+)\b")


def split_args(text: str) -> list[str]:
    """Split strace's argument text at the commas outside brackets, braces,
    quotes and `/* */` comments."""
    parts: list[str] = []
    depth = 0
    start = 0
    i = 0
    while i < len(text):
        c = text[i]
        if c == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            i = len(text) if end < 0 else end + 1
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append(text[start:i].strip())
            start = i + 1
        i += 1
    tail = text[start:].strip()
    if tail or parts:
        parts.append(tail)
    return parts


def parse_trace(lines) -> list[Call]:
    """The completed calls in an `strace -f -o FILE` trace, an unfinished call
    joined to its resumption (both on the same pid's lines). Signal, exit and
    `+++`/`---` lines are skipped; a call still unfinished at the end (its
    process killed) is dropped. Any other line is refused: a census that
    skipped lines it couldn't read would understate the workloads."""
    pending: dict[str, tuple[str, str]] = {}
    calls: list[Call] = []
    for raw in lines:
        line = raw.rstrip("\n")
        if not line.strip():
            continue
        m = _LINE.match(line)
        if not m:
            raise CensusError(f"not an strace -f line: {line!r}")
        pid, rest = m.groups()
        if rest.startswith(("+++", "---")):
            continue
        if u := _UNFINISHED.match(rest):
            pending[pid] = (u.group(1), u.group(2))
            continue
        if r := _RESUMED.match(rest):
            name, partial = pending.pop(pid, (r.group(1), ""))
            if name != r.group(1):
                raise CensusError(f"pid {pid}: {r.group(1)} resumed while {name} was pending")
            rest = f"{name}({partial}{r.group(2)}"
        c = _COMPLETE.match(rest)
        if not c:
            raise CensusError(f"pid {pid}: not a completed syscall: {rest!r}")
        name, args, ret, note = c.groups()
        e = _ERRNO.match(note or "") if ret in ("-1", "?") else None
        calls.append(Call(name, split_args(args), ret, e.group(1) if e else None))
    return calls


def render_value(label: str, value: str) -> str:
    """A recorded argument as the census writes it: numbers in hex, or in
    decimal for the labels in `DECIMAL_LABELS`; anything else (`NULL`) as is.
    A `|`-joined value (`clone`'s flags with its exit signal, `0x4100|17`) is
    one number, its parts OR'd."""
    try:
        n = 0
        for part in value.split("|"):
            n |= int(part, 0)
    except ValueError:
        return value
    if label in DECIMAL_LABELS or n < 0:
        return str(n)
    return hex(n)


def key_values(call: Call) -> dict[str, str]:
    """The recorded arguments of one call (`KEY_ARGS`)."""
    out: dict[str, str] = {}
    for label, pos, fld in KEY_ARGS.get(call.name, ()):
        if pos is not None and pos >= len(call.args):
            continue
        texts = [call.args[pos]] if pos is not None else call.args
        value = None
        for text in texts:
            if fld is None:
                value = text
            elif m := re.search(rf"(?:^|[{{,\s]){fld}=([^,}}\s]+)", text):
                value = m.group(1)
            if value is not None:
                break
        if value is not None:
            out[label] = render_value(label, value)
    return out


def census(traces: dict[str, list[Call]]) -> dict[str, Seen]:
    """Every syscall the traces made, by name, with its workloads, recorded
    arguments and errors. Raises `CensusError` on a name x86-64 doesn't have."""
    seen: dict[str, Seen] = {}
    for workload, calls in traces.items():
        for call in calls:
            if call.name not in X86_64:
                raise CensusError(
                    f"{workload}: unknown syscall {call.name!r}: not an x86-64 trace "
                    "(Rosetta or qemu-user translation?); run on a real x86-64 Linux kernel"
                )
            s = seen.setdefault(call.name, Seen())
            s.calls += 1
            s.workloads.add(workload)
            for label, value in key_values(call).items():
                s.args.setdefault(label, set()).add(value)
            if call.errno:
                s.errors[call.errno] = s.errors.get(call.errno, 0) + 1
    return seen


def _sort_key(value: str) -> tuple[int, int | str]:
    try:
        return (0, int(value, 0))
    except ValueError:
        return (1, value)


def _toml_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _toml_list(items) -> str:
    return "[" + ", ".join(_toml_str(i) for i in items) + "]"


def render_toml(meta: dict[str, object], seen: dict[str, Seen]) -> str:
    """The census file: `[meta]`, then one `[syscalls.<name>]` per syscall in
    number order, with its `args` and `errors` subtables when not empty."""
    out = [
        "# The M8 syscall census (#337), written by tools/syscall-census.sh: do not edit.",
        "# Regenerate it by running that script on an x86-64 Linux Docker or Podman host.",
        "",
        "[meta]",
    ]
    for key, value in meta.items():
        out.append(
            f"{key} = {_toml_list(value) if isinstance(value, list) else _toml_str(str(value))}"
        )
    for name in sorted(seen, key=X86_64.__getitem__):
        s = seen[name]
        out += [
            "",
            f"[syscalls.{name}]",
            f"nr = {X86_64[name]}",
            f"calls = {s.calls}",
            f"workloads = {_toml_list(sorted(s.workloads))}",
        ]
        if s.args:
            out += ["", f"[syscalls.{name}.args]"]
            for label in sorted(s.args):
                out.append(f"{label} = {_toml_list(sorted(s.args[label], key=_sort_key))}")
        if s.errors:
            out += ["", f"[syscalls.{name}.errors]"]
            out += [f"{e} = {n}" for e, n in sorted(s.errors.items())]
    return "\n".join(out) + "\n"


def read_meta(directory: Path) -> dict[str, object]:
    """The container's facts the census header records, from files the
    workload script wrote: `alpine-release`, `kernel`, `image`, `packages`."""

    def text(name: str) -> str:
        p = directory / name
        return p.read_text().strip() if p.exists() else "unknown"

    packages = text("packages")
    return {
        "alpine": text("alpine-release"),
        "image": text("image"),
        "kernel": text("kernel"),
        "packages": sorted(packages.split()) if packages != "unknown" else [],
    }


def build(traces_dir: Path, meta_dir: Path) -> str:
    """The census file's text from every `<workload>.strace` in `traces_dir`."""
    files = sorted(traces_dir.glob("*.strace"))
    if not files:
        raise CensusError(f"no *.strace files in {traces_dir}")
    traces = {}
    for f in files:
        with f.open(errors="replace") as lines:
            traces[f.stem] = parse_trace(lines)
    meta = read_meta(meta_dir)
    meta["workloads"] = sorted(traces)
    return render_toml(meta, census(traces))


# --- the report (host side: Python 3.11+ for tomllib) ---


def dispatched(dispatch_rs: str) -> set[str]:
    """The syscalls `kernel/src/linux/syscall/mod.rs` dispatches (each arm
    names its number as `__NR_<name>`)."""
    return set(re.findall(r"__NR_(\w+)", dispatch_rs))


def classify(name: str, implemented: set[str]) -> str:
    """`implemented`, the milestone planning it (`M8`, `M9`, `M10`), or
    `unplanned`."""
    if name in implemented:
        return "implemented"
    for milestone, names in PLANNED.items():
        if name in names:
            return milestone
    return "unplanned"


def report(data: dict, implemented: set[str], verbose: bool = False) -> str:
    """The census against the kernel: unplanned syscalls first, then each
    milestone's, then the implemented ones; the vDSO calls listed apart."""
    rows: dict[str, list[str]] = {}
    for name, entry in data["syscalls"].items():
        cls = classify(name, implemented)
        line = f"  {entry['nr']:>3} {name:<20} {entry['calls']:>7}  {', '.join(entry['workloads'])}"
        if verbose:
            for label, values in entry.get("args", {}).items():
                line += f"\n        {label}: {' '.join(values)}"
            if entry.get("errors"):
                errs = " ".join(f"{e}={n}" for e, n in entry["errors"].items())
                line += f"\n        errors: {errs}"
        rows.setdefault(cls, []).append(line)
    meta = data["meta"]
    out = [
        (
            f"Census of {', '.join(meta['workloads'])} on Alpine {meta['alpine']} "
            f"(kernel {meta['kernel']}): {len(data['syscalls'])} syscalls."
        ),
    ]
    for cls in ("unplanned", "M8", "M9", "M10", "implemented"):
        if cls in rows:
            out += ["", f"{cls} ({len(rows[cls])}):", *rows[cls]]
    out += [
        "",
        "vDSO calls (Linux answers these without a syscall, so the census can't see them;",
        f"musl makes them as syscalls on Lumen): {', '.join(VDSO)}",
    ]
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build", help="traces into the census file")
    b.add_argument("--traces", type=Path, required=True)
    b.add_argument("--meta", type=Path, required=True)
    b.add_argument("-o", "--output", type=Path, required=True)
    r = sub.add_parser("report", help="the census against the kernel's dispatch table")
    r.add_argument("--census", type=Path, help="default: tests/linux/m8-census.toml")
    r.add_argument("--dispatch", type=Path, help="default: kernel/src/linux/syscall/mod.rs")
    r.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)
    if args.cmd == "build":
        try:
            args.output.write_text(build(args.traces, args.meta))
        except CensusError as e:
            print(f"syscall census: {e}", file=sys.stderr)
            return 1
        return 0
    import tomllib

    # The repo root, three levels above tools/harness/lumen_harness/ (only
    # here: inside the census container this file sits at /harness/syscalls.py).
    root = Path(__file__).resolve().parents[3]
    census_file = args.census or root / "tests/linux/m8-census.toml"
    dispatch_file = args.dispatch or root / "kernel/src/linux/syscall/mod.rs"
    data = tomllib.loads(census_file.read_text())
    print(report(data, dispatched(dispatch_file.read_text()), args.verbose))
    return 0


if __name__ == "__main__":
    sys.exit(main())
