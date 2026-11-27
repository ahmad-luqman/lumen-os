"""The M8 (#337) and M9 (#451) syscall censuses: which Linux syscalls the gate
workloads make.

`tools/syscall-census.sh` runs the workloads under `strace -f -X raw` on stock
Alpine (x86-64) and calls `build` here on the traces, inside the container: so
this module uses the standard library only (no uv, no harness dependencies,
Python 3.12) and imports nothing else from `lumen_harness`. The results,
`tests/linux/m8-census.toml` and `tests/linux/m9-census.toml`, are checked in.
`report` compares a census with the syscalls the kernel dispatches (M8) or the
M6–M8 kernel tables (M9) and the milestones the roadmap plans them for.

    python3 syscalls.py build --traces DIR --meta DIR -o m8-census.toml
    python3 syscalls.py build --milestone M9 --traces DIR --meta DIR -o m9-census.toml
    PYTHONPATH=tools/harness uv run python -m lumen_harness.syscalls report [--verbose]
    PYTHONPATH=tools/harness uv run python -m lumen_harness.syscalls report --milestone M9 [--verbose]
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from collections import Counter, defaultdict
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

Milestone = Literal["M8", "M9"]

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

# The arguments recorded per syscall, as (label, position, field):
# - no field: that argument's value (`futex`'s op);
# - a field: `field=value` inside that argument (`clone(child_stack=NULL,
#   flags=17)`'s flags; `rt_sigaction`'s new action's sa_flags, never the old
#   action's). An argument of `NULL` has no field and records nothing; so does
#   `epoll_ctl(.., EPOLL_CTL_DEL, .., event)`'s undecoded bare-address pointer,
#   which Linux ignores (see `_undecoded_ignored_struct`).
# `setsockopt` and `getsockopt` fold their `level` and `optname` into one
# combined `option = "<level>:<optname>"` label in `key_values`.
KEY_ARGS: dict[str, tuple[tuple[str, int, str | None], ...]] = {
    "accept4": (("flags", 3, None),),
    "arch_prctl": (("code", 0, None),),
    "clock_getres": (("clock", 0, None),),
    "clock_gettime": (("clock", 0, None),),
    "clock_nanosleep": (("clock", 0, None), ("flags", 1, None)),
    "clone": (("flags", 1, "flags"),),
    "clone3": (("flags", 0, "flags"),),
    "dup3": (("flags", 2, None),),
    "epoll_create1": (("flags", 0, None),),
    "epoll_ctl": (("op", 1, None), ("events", 3, "events")),
    "eventfd2": (("flags", 1, None),),
    "faccessat2": (("flags", 3, None),),
    "fcntl": (("cmd", 1, None),),
    "flock": (("operation", 1, None),),
    "futex": (("op", 1, None),),
    "getrandom": (("flags", 2, None),),
    "getsockopt": (("level", 1, None), ("optname", 2, None)),
    "inotify_init1": (("flags", 0, None),),
    "ioctl": (("request", 1, None),),
    "kill": (("signal", 1, None),),
    "lseek": (("whence", 2, None),),
    "madvise": (("advice", 2, None),),
    "membarrier": (("cmd", 0, None),),
    "memfd_create": (("flags", 1, None),),
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
    "prlimit64": (("resource", 1, None), ("new_limit", 2, None)),
    "recvfrom": (("flags", 3, None),),
    "recvmmsg": (("flags", 3, None),),
    "recvmsg": (("flags", 2, None),),
    "renameat2": (("flags", 4, None),),
    "rt_sigaction": (("signal", 0, None), ("sa_flags", 1, "sa_flags")),
    "rt_sigprocmask": (("how", 0, None),),
    "rt_sigqueueinfo": (("signal", 1, None),),
    "sendmmsg": (("flags", 3, None),),
    "sendmsg": (("flags", 2, None),),
    "sendto": (("flags", 3, None),),
    "setsockopt": (("level", 1, None), ("optname", 2, None)),
    "shutdown": (("how", 1, None),),
    "signalfd4": (("flags", 3, None),),
    "socket": (("domain", 0, None), ("type", 1, None), ("protocol", 2, None)),
    "socketpair": (("domain", 0, None), ("type", 1, None), ("protocol", 2, None)),
    "statx": (("flags", 2, None),),
    "tgkill": (("signal", 2, None),),
    "timer_create": (("clock", 0, None),),
    "timerfd_create": (("clock", 0, None), ("flags", 1, None)),
    "timerfd_settime": (("flags", 1, None),),
    "tkill": (("signal", 1, None),),
    "unlinkat": (("flags", 2, None),),
    "utimensat": (("flags", 3, None),),
    "wait4": (("options", 2, None),),
    "waitid": (("idtype", 0, None), ("options", 3, None)),
}

# Labels shown in decimal; every other number (flags, ops, requests) in hex.
DECIMAL_LABELS = frozenset(
    {
        "signal",
        "clock",
        "resource",
        "domain",
        "protocol",
        "level",
        "optname",
        "idtype",
        "whence",
        "code",
    }
)
# Pointer labels recorded only as `NULL` or `set` (whether `prlimit64` writes).
PRESENCE_LABELS = frozenset({"new_limit"})

# The milestones that plan each syscall. M8: ROADMAP M8's rows, plus the calls
# the M8 issues name that those rows don't (waitid in #336; tkill, waitid and
# pselect6 in #337) and the rest of their families (getpgid, getsid,
# sched_setaffinity, the priorities, the signal waits), and what the design
# (#336) added from the census: sysinfo (from M10), the timers, mknod, the
# fsuid calls, getcpu, the robust lists, the sigqueueinfo calls and
# restart_syscall. Since B0 (#371) it is exactly the kernel's M8_SYSCALLS
# (test_m8_table_covers_the_census). M9 and M10: ROADMAP's rows.
# fmt: off
PLANNED: dict[str, frozenset[str]] = {
    "M8": frozenset({
        "fork", "vfork", "clone", "execve", "wait4", "waitid", "futex", "nanosleep",
        "clock_nanosleep", "clock_getres", "getrusage", "prctl", "statfs", "fstatfs",
        "rt_sigreturn", "sigaltstack", "kill", "tkill", "tgkill", "rt_sigpending", "rt_sigsuspend",
        "rt_sigtimedwait", "pipe", "pipe2", "dup", "dup2", "dup3", "poll", "ppoll", "select",
        "pselect6", "setsid", "setpgid", "getpgid", "getsid", "sched_yield", "sched_getaffinity",
        "sched_setaffinity", "getpriority", "setpriority",
        "sysinfo", "pause", "alarm", "getitimer", "setitimer", "timer_create", "timer_settime",
        "timer_gettime", "timer_getoverrun", "timer_delete", "mknod", "mknodat", "setfsuid",
        "setfsgid", "getcpu", "set_robust_list", "get_robust_list", "rt_sigqueueinfo",
        "rt_tgsigqueueinfo", "restart_syscall",
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
        "inotify_rm_watch", "rseq", "io_uring_setup", "io_uring_enter",
        "io_uring_register",
    }),
}
# fmt: on

# The syscalls M8 leaves -ENOSYS, each by a decision ARCHITECTURE section 8
# records ("Stays `-ENOSYS` in M8, and why"): sendfile and membarrier (the
# callers fall back), clone3 and execveat, futex_waitv, the scheduler-policy
# calls (no real-time policies), signalfd and signalfd4, and times.
# fmt: off
ENOSYS_BY_DECISION: frozenset[str] = frozenset({
    "sendfile", "membarrier", "clone3", "execveat", "futex_waitv", "sched_setparam",
    "sched_getparam", "sched_setscheduler", "sched_getscheduler", "sched_get_priority_max",
    "sched_get_priority_min", "sched_rr_get_interval", "signalfd", "signalfd4", "times",
})
# fmt: on

# Calls Linux normally answers from the vDSO, so strace doesn't see them. Lumen maps no
# vDSO, so musl falls back to syscalls there: clock_gettime (which its
# gettimeofday and time call too) and getcpu. The report lists them apart.
VDSO = ("clock_gettime", "gettimeofday", "time", "getcpu")


class CensusError(Exception):
    """Input this census can't stand behind: no traces, a trace line it can't
    read or calls out of order on one pid, a recorded argument that is missing
    or not a number, a syscall name x86-64 doesn't have (the `syscall_0x...`
    lines strace prints under Rosetta's translation), missing container facts,
    or a dispatch table it can't read."""


@dataclass
class Call:
    """One syscall from a trace; `ret` is `?` for one that never returned."""

    name: str
    args: list[str]
    ret: str
    errno: str | None


@dataclass
class Seen:
    """What the census keeps per syscall."""

    calls: int = 0
    workloads: set[str] = field(default_factory=set)
    args: defaultdict[str, set[str]] = field(default_factory=lambda: defaultdict(set))
    errors: Counter[str] = field(default_factory=Counter)


_LINE = re.compile(r"^(\d+)\s+(.*)$")
_UNFINISHED = re.compile(r"^(\w+)\((.*) <unfinished \.\.\.>$")
_RESUMED = re.compile(r"^<\.\.\. (\w+) resumed>(.*)$")
# A resumption cut off by the thread's end: `<... read resumed> <unfinished ...>) = ?`.
_CUT_OFF = re.compile(r"^\s*<unfinished \.\.\.>(?=\))")
# A completed call: its arguments, its return value, then whatever strace adds
# after it: an errno (`-1 ENOENT (...)`, `? ERESTARTNOHAND (...)`) or a note on
# a success (`0x8800 (flags O_RDONLY|O_NONBLOCK|O_LARGEFILE)`, `0 (Timeout)`,
# `1 ([{fd=0, ...}])`).
_COMPLETE = re.compile(r"^(\w+)\((.*)\)\s+=\s+(\S+)(?:\s+(.*))?$")
# An errno name, underscores included (`ERESTART_RESTARTBLOCK`).
_ERRNO = re.compile(r"^(E[A-Z0-9_]+)\b")


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


def _trimmed(partial: str) -> str:
    """The arguments of a call strace printed only in part (`wait4(-1, `),
    without the comma it left for the rest."""
    return partial.rstrip().removesuffix(",")


def parse_trace(lines: Iterable[str]) -> list[Call]:
    """Every syscall in an `strace -f -o FILE` trace, an unfinished call joined
    to its resumption (both on the same pid's lines). A call that never
    returned is kept with `?` for its return value: one still unfinished when
    the trace ends, or one resumed only to be cut off by its thread's end
    (`<... read resumed> <unfinished ...>) = ?`). Signal, exit and `+++`/`---`
    lines are skipped. Anything else is refused: a line it can't read, a
    resumption with nothing pending or of another call, and a pid starting a
    call while one is pending. A census that skipped what it couldn't read
    would understate the workloads.

    `rt_sigreturn`'s return value is the register it restores (the
    interrupted code's result, `-1 EINTR` after a call a handler cut short),
    never an error of its own, so it gets no errno."""
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
        if r := _RESUMED.match(rest):
            if pid not in pending:
                raise CensusError(f"pid {pid}: {r.group(1)} resumed with nothing pending")
            name, partial = pending.pop(pid)
            if name != r.group(1):
                raise CensusError(f"pid {pid}: {r.group(1)} resumed while {name} was pending")
            tail = r.group(2)
            if cut := _CUT_OFF.match(tail):
                partial, tail = _trimmed(partial), tail[cut.end() :]
            rest = f"{name}({partial}{tail}"
        elif pid in pending:
            raise CensusError(
                f"pid {pid}: a new call while {pending[pid][0]} was pending: {rest!r}"
            )
        elif u := _UNFINISHED.match(rest):
            pending[pid] = (u.group(1), u.group(2))
            continue
        c = _COMPLETE.match(rest)
        if not c:
            raise CensusError(f"pid {pid}: not a completed syscall: {rest!r}")
        name, args, ret, note = c.groups()
        errno = None
        if ret in ("-1", "?"):
            # -1 always carries an errno; ? carries one or nothing (exit_group).
            if e := _ERRNO.match(note or ""):
                errno = e.group(1)
            elif ret == "-1":
                raise CensusError(f"pid {pid}: no errno after -1: {rest!r}")
            elif note:
                raise CensusError(f"pid {pid}: not an errno after ?: {rest!r}")
        if name == "rt_sigreturn":
            errno = None
        calls.append(Call(name, split_args(args), ret, errno))
    for name, partial in pending.values():
        calls.append(Call(name, split_args(_trimmed(partial)), "?", None))
    return calls


_HTONS = re.compile(r"^htons\(([^)]+)\)$")


def render_value(label: str, value: str) -> str:
    """A recorded argument as the census writes it: in hex, or in decimal for
    the labels in `DECIMAL_LABELS`, or as `NULL` or `set` for those in
    `PRESENCE_LABELS`. A `|`-joined value (`clone`'s flags with its exit
    signal, `0x4100|17`) is one number, its parts OR'd. `htons(n)` is handled
    as a defensive guess for non-raw `AF_PACKET` protocols (real `strace -X
    raw` traces print raw integers and never print `htons(n)`). Anything else
    is a `CensusError`: `-X raw` prints every recorded argument as a number."""
    if label in PRESENCE_LABELS:
        return "NULL" if value == "NULL" else "set"
    try:
        if m := _HTONS.match(value):
            v = int(m.group(1), 0)
            if not 0 <= v <= 0xFFFF:
                raise ValueError
            n = ((v & 0xFF) << 8) | ((v >> 8) & 0xFF)
        else:
            n = 0
            for part in value.split("|"):
                n |= int(part, 0)
    except ValueError:
        raise CensusError(f"{label}={value!r}: not a number (a trace without -X raw?)") from None
    if label in DECIMAL_LABELS or n < 0:
        return str(n)
    return hex(n)


def _undecoded_ignored_struct(call: Call, pos: int, value: str) -> bool:
    """`epoll_ctl(.., EPOLL_CTL_DEL, .., event)`: Linux ignores the event
    (fs/eventpoll.c, ep_op_has_event()), so strace prints its bare address."""
    return (
        call.name == "epoll_ctl"
        and pos == 3
        and call.args[1] in ("0x2", "EPOLL_CTL_DEL")
        and re.fullmatch(r"0x[0-9a-f]+", value) is not None
    )


def key_values(call: Call) -> dict[str, str]:
    """The recorded arguments of one call (`KEY_ARGS`). An argument strace
    didn't print is skipped only in a call that never returned (printed in
    part), and a field only in a `NULL` argument or one the kernel ignores;
    anything else missing is a `CensusError`. For `setsockopt` and
    `getsockopt`, `level` and `optname` are combined into `option` as
    `"<level>:<optname>"`."""
    out: dict[str, str] = {}
    for label, pos, fld in KEY_ARGS.get(call.name, ()):
        if pos >= len(call.args):
            if call.ret == "?":
                continue
            raise CensusError(f"{call.name}: no argument {pos} for its {label}")
        value = call.args[pos]
        if fld is not None:
            m = re.search(rf"(?:^|[{{,\s]){fld}=([^,}}\s]+)", value)
            if not m:
                if value == "NULL" or _undecoded_ignored_struct(call, pos, value):
                    continue
                raise CensusError(f"{call.name}: no {fld}= in argument {pos}: {value!r}")
            value = m.group(1)
        out[label] = render_value(label, value)
    if call.name in ("setsockopt", "getsockopt"):
        level = out.pop("level", None)
        optname = out.pop("optname", None)
        if level is not None and optname is not None:
            out["option"] = f"{level}:{optname}"
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
            try:
                values = key_values(call)
            except CensusError as e:
                raise CensusError(f"{workload}: {e}") from None
            for label, value in values.items():
                s.args[label].add(value)
            if call.errno:
                s.errors[call.errno] += 1
    return seen


def _sort_key(value: str) -> tuple[int, tuple[int, ...] | str]:
    if ":" in value:
        try:
            return (0, tuple(int(p, 0) for p in value.split(":")))
        except ValueError:
            return (1, value)
    try:
        return (0, (int(value, 0),))
    except ValueError:
        return (1, value)


def _toml_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _toml_list(items: Iterable[str]) -> str:
    return "[" + ", ".join(_toml_str(i) for i in items) + "]"


def render_toml(
    meta: dict[str, str | list[str]], seen: dict[str, Seen], milestone: Milestone = "M8"
) -> str:
    """The census file: `[meta]`, then one `[syscalls.<name>]` per syscall in
    number order, with its `args` and `errors` subtables when not empty."""
    if milestone == "M8":
        header = [
            "# The M8 syscall census (#337), written by tools/syscall-census.sh: do not edit.",
            "# Regenerate it by running that script on an x86-64 Linux Docker or Podman host.",
        ]
    elif milestone == "M9":
        header = [
            "# The M9 syscall census (#451), written by tools/syscall-census.sh: do not edit.",
            "# Regenerate it via .github/workflows/syscall-census.yml or tools/syscall-census.sh m9.",
        ]
    else:
        raise CensusError(f"unknown milestone {milestone!r} (expected 'M8' or 'M9')")
    out = [*header, "", "[meta]"]
    for key in sorted(meta):
        value = meta[key]
        out.append(f"{key} = {_toml_list(value) if isinstance(value, list) else _toml_str(value)}")
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


def read_meta(directory: Path) -> dict[str, str | list[str]]:
    """The container's facts the census header records, from files the census
    scripts wrote: `image` (tools/syscall-census.sh, on the host), and
    `alpine-release`, `kernel` and `packages` (workloads.sh, in the container).
    A missing or empty one is a `CensusError`: the header would misstate where
    the census came from."""

    def text(name: str) -> str:
        p = directory / name
        content = p.read_text().strip() if p.exists() else ""
        if not content:
            raise CensusError(f"{p}: missing or empty")
        return content

    return {
        "alpine": text("alpine-release"),
        "image": text("image"),
        "kernel": text("kernel"),
        "packages": sorted(text("packages").split()),
    }


def build(traces_dir: Path, meta_dir: Path, milestone: Milestone = "M8") -> str:
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
    return render_toml(meta, census(traces), milestone=milestone)


# --- the report ---

# A dispatch arm, on one line or as rustfmt wraps it, with or without a guard:
# `Ok(general::__NR_exit | general::__NR_exit_group) => {`, or
# `Ok(\n    general::__NR_getuid\n    | general::__NR_getgid,\n) if x =>`.
_ARM = re.compile(
    r"\bOk\(\s*(general::__NR_\w+(?:\s*\|\s*general::__NR_\w+)*)\s*,?\s*\)\s*(?:if\b.*?)?=>",
    re.DOTALL,
)


def dispatched(dispatch_rs: str) -> set[str]:
    """The syscalls `kernel/src/linux/syscall/mod.rs` dispatches: the names in
    its `Ok(general::__NR_<name>) =>` arms; a mention in a comment doesn't
    count. A `general::__NR_` anywhere else, or no arms at all, is a
    `CensusError`: the arms changed shape, and the report would quietly call
    implemented syscalls unplanned."""
    names = {n for arm in dispatch_arms(dispatch_rs) for n in arm.names}
    code = re.sub(r"//[^\n]*", "", dispatch_rs)
    if stray := sorted(set(re.findall(r"general::__NR_(\w+)", _ARM.sub("", code)))):
        raise CensusError(f"general::__NR_ outside an arm the report can read: {', '.join(stray)}")
    if not names:
        raise CensusError("no `Ok(general::__NR_<name>) =>` arms in the dispatch table")
    return names


# The registers a syscall's arguments arrive in, in order (the x86-64 ABI).
ARG_REGISTERS = ("rdi", "rsi", "rdx", "r10", "r8", "r9")

# How many arguments each of M8_SYSCALLS takes: Linux's x86-64 signatures
# (`SYSCALL_DEFINE<n>`), so its arm reads that many registers.
# fmt: off
M8_ARGS: dict[str, int] = {
    # task
    "clone": 5, "fork": 0, "vfork": 0, "execve": 3, "wait4": 4, "waitid": 5, "setpgid": 2,
    "setsid": 0, "getpgid": 1, "getsid": 1,
    # signal
    "rt_sigreturn": 0, "rt_sigpending": 2, "rt_sigtimedwait": 4, "rt_sigqueueinfo": 3,
    "rt_tgsigqueueinfo": 4, "rt_sigsuspend": 2, "sigaltstack": 2, "kill": 2, "tkill": 2,
    "tgkill": 3, "pause": 0, "restart_syscall": 0,
    # futex
    "futex": 6, "set_robust_list": 2, "get_robust_list": 3, "nanosleep": 2,
    "clock_nanosleep": 4,
    # poll
    "poll": 3, "select": 5, "pselect6": 6, "ppoll": 5,
    # pipe
    "pipe": 1, "pipe2": 2, "dup": 1, "dup2": 2, "dup3": 3, "mknod": 3, "mknodat": 4,
    # timer
    "alarm": 1, "getitimer": 2, "setitimer": 3, "timer_create": 3, "timer_settime": 4,
    "timer_gettime": 2, "timer_getoverrun": 1, "timer_delete": 1,
    # sys
    "sched_yield": 0, "sched_setaffinity": 3, "sched_getaffinity": 3, "getpriority": 2,
    "setpriority": 3, "getrusage": 2, "sysinfo": 1, "prctl": 5, "statfs": 2, "fstatfs": 2,
    "clock_getres": 2, "getcpu": 3, "setfsuid": 1, "setfsgid": 1,
}
# fmt: on

# The arms written inline in `dispatch`, calling no handler (M6's), by their
# first syscall.
INLINE_ARMS = frozenset({"getuid", "getpgrp"})


@dataclass(frozen=True)
class Arm:
    """One `dispatch` arm: the syscalls its pattern names, in order, the
    `sys_*` functions its body calls, and the argument registers it reads, in
    the order it reads them (the whole frame passed isn't a register read)."""

    names: tuple[str, ...]
    handlers: tuple[str, ...]
    registers: tuple[str, ...]


def dispatch_arms(dispatch_rs: str) -> list[Arm]:
    """`kernel/src/linux/syscall/mod.rs`'s arms, in order, each body running
    to the next arm (the last one's to the `_ =>` fallback); comments left
    out."""
    code = re.sub(r"//[^\n]*", "", dispatch_rs)
    found = list(_ARM.finditer(code))
    regs = "|".join(ARG_REGISTERS)
    arms = []
    for i, m in enumerate(found):
        if i + 1 < len(found):
            body = code[m.end() : found[i + 1].start()]
        else:
            body = re.split(r"\n\s*_\s*=>", code[m.end() :])[0]
        arms.append(
            Arm(
                names=tuple(re.findall(r"__NR_(\w+)", m.group(1))),
                handlers=tuple(re.findall(r"\b(sys_\w+)\(", body)),
                registers=tuple(re.findall(rf"\bframe\.({regs})\b", body)),
            )
        )
    return arms


def arm_problems(arms: Iterable[Arm]) -> list[str]:
    """What's wrong with the arms (ARCHITECTURE section 8: each arm passes its
    signature's registers, in order, to its handler):
    - one calling a handler not named after the first syscall of its pattern
      (`sys_<name>`), or more than one, or none (but `INLINE_ARMS`);
    - one reading argument registers other than the first few in order;
    - an M8 syscall's (`M8_ARGS`) reading other than its argument count, or
      sharing its arm."""
    problems = []
    for arm in arms:
        first = arm.names[0]
        if arm.handlers != (f"sys_{first}",) and not (not arm.handlers and first in INLINE_ARMS):
            problems.append(f"{first}: calls {', '.join(arm.handlers) or 'no handler'}")
        if arm.registers != ARG_REGISTERS[: len(arm.registers)]:
            problems.append(f"{first}: reads {', '.join(arm.registers)}")
        elif first in M8_ARGS and len(arm.registers) != M8_ARGS[first]:
            problems.append(f"{first}: reads {len(arm.registers)} registers, not {M8_ARGS[first]}")
        if len(arm.names) > 1 and set(arm.names) & M8_ARGS.keys():
            problems.append(f"{first}: shares its arm with {', '.join(arm.names[1:])}")
    return problems


# A handler in a cluster file (kernel/src/linux/syscall/*.rs): its doc lines,
# its name and its body, which ends at the first `}` in column 0.
_HANDLER = re.compile(
    r"((?:^[ \t]*///[^\n]*\n)*)^(?:pub(?:\([\w ]+\))? )?fn (sys_\w+)\(.*?\)[^{;]*\{\n(.*?)^\}",
    re.DOTALL | re.MULTILINE,
)
# What marks a handler as an M8 stub: its doc, "M8 stub, replaced by C7 (#386)."
STUB_MARK = "M8 stub"
# The issues a stub's doc names: "replaced by C1 (#380) and C3 (#382)."
_REPLACED_BY = re.compile(r"replaced by ([^.]*)\.")
_ISSUE = re.compile(r"\b([BCD]\d+) \(#(\d+)\)")


@dataclass(frozen=True)
class Stub:
    """An M8 stub (its doc says `STUB_MARK`): its doc's text without the `///`
    markers, and its body, whitespace collapsed."""

    doc: str
    body: str


def stubs(cluster_sources: Iterable[str]) -> dict[str, Stub]:
    """The M8 stubs in the cluster files' sources, by syscall. A `fn sys_*`
    this can't read (a one-line body, say) is a `CensusError`: a stub could
    hide there."""
    found = {}
    for source in cluster_sources:
        read = _HANDLER.findall(source)
        if len(read) != len(re.findall(r"\bfn sys_\w+", source)):
            names = sorted(set(re.findall(r"\bfn (sys_\w+)", source)) - {fn for _, fn, _ in read})
            raise CensusError(f"a handler of a shape this can't read: {', '.join(names) or '?'}")
        for doc, fn, body in read:
            text = " ".join(re.sub(r"^[ \t]*///", "", doc, flags=re.MULTILINE).split())
            if STUB_MARK in text:
                found[fn.removeprefix("sys_")] = Stub(text, " ".join(body.split()))
    return found


def stub_problems(
    found: dict[str, Stub], design: dict[str, tuple[str, ...]], issues: dict[str, int]
) -> list[str]:
    """What's wrong with the stubs: a body that does more than return
    `not_implemented(general::__NR_<its name>)` (as `frame.rax` for one that
    returns through the frame), or a doc not naming exactly the issues the
    design's table names for it (`design`, from `design_issues`), each with
    its number (`issues`: label to GitHub issue)."""
    problems = []
    for name, stub in found.items():
        call = f"not_implemented(general::__NR_{name})"
        if stub.body not in (call, f"frame.rax = {call}; SyscallReturn::Sysret"):
            problems.append(f"{name}: body {stub.body!r}")
        clause = _REPLACED_BY.search(stub.doc)
        named = tuple(_ISSUE.findall(clause.group(1))) if clause else ()
        want = tuple((label, str(issues.get(label, "?"))) for label in design.get(name, ()))
        if not named or named != want:
            said = ", ".join(f"{label} (#{n})" for label, n in named) or "none"
            problems.append(f"{name}: its doc names {said}, the design {want or 'no row'}")
    return problems


def cluster_sources(syscall_dir: Path) -> list[str]:
    """The sources of `kernel/src/linux/syscall/`'s files."""
    return [f.read_text() for f in sorted(syscall_dir.glob("*.rs"))]


# A row of ARCHITECTURE section 8's M8 syscall table: `| 122, 123 |
# \`setfsuid(uid)\`, \`setfsgid(gid)\` | <its rules> | C12 |`, a `|` inside a
# cell escaped (`\|`).
_DESIGN_ROW = re.compile(r"^\| *\d+(?:, *\d+)* *\|.*\| *$", re.MULTILINE)


def design_issues(architecture_md: str) -> dict[str, tuple[str, ...]]:
    """The issues ARCHITECTURE section 8's M8 syscall table names for each of
    its syscalls (its last column; `—` none), by syscall. A row whose numbers
    and names disagree is a `CensusError`."""
    start = architecture_md.find("#### M8 processes")
    if start < 0:
        raise CensusError("no M8 syscall table in ARCHITECTURE.md")
    end = architecture_md.find("\n#### ", start + 1)
    rows: dict[str, tuple[str, ...]] = {}
    for line in _DESIGN_ROW.findall(architecture_md[start:end]):
        cells = [c.strip() for c in re.split(r"(?<!\\)\|", line)[1:-1]]
        nrs, cell, labels = cells[0], cells[1], cells[-1]
        names = re.findall(r"`(\w+)[(`]", cell)
        numbers = [int(n) for n in nrs.split(",")]
        if [X86_64.get(n) for n in names] != numbers:
            raise CensusError(f"M8 table: row {nrs} names {', '.join(names)}")
        for name in names:
            rows[name] = tuple(re.findall(r"[BCD]\d+", labels))
    if not rows:
        raise CensusError("no rows in ARCHITECTURE.md's M8 syscall table")
    return rows


# The kernel's syscall tables (kernel/src/linux/mod.rs): `pub const M6_SYSCALLS:
# &[(u32, &str)] = &[ ... ];` and its M7 and M8 siblings, each row
# `(general::__NR_<name>, "<name>"),`.
_TABLE = re.compile(r"pub const (M\d+)_SYSCALLS: &\[\(u32, &str\)\] = &\[(.*?)\];", re.DOTALL)
_ROW = re.compile(r'\(general::__NR_(\w+), "(\w+)"\),?')


def kernel_tables(linux_mod_rs: str) -> dict[str, list[str]]:
    """The syscall names in each of `kernel/src/linux/mod.rs`'s tables, by
    milestone (`M6`, `M7`, `M8`), in their order (a name twice is kept, for
    the caller to refuse). A row whose constant and name differ, a name x86-64
    doesn't have, anything in a table that isn't a row, a table declared in a
    shape this can't read, or no tables at all is a `CensusError`."""
    code = re.sub(r"//[^\n]*", "", linux_mod_rs)
    tables: dict[str, list[str]] = {}
    for milestone, body in _TABLE.findall(code):
        names = []
        for const, name in _ROW.findall(body):
            if const != name:
                raise CensusError(f"{milestone}_SYSCALLS: __NR_{const} named {name!r}")
            if name not in X86_64:
                raise CensusError(f"{milestone}_SYSCALLS: {name!r} isn't an x86-64 syscall")
            names.append(name)
        if rest := _ROW.sub("", body).strip():
            raise CensusError(f"{milestone}_SYSCALLS: not a row: {rest.split()[0]!r}")
        tables[milestone] = names
    if unread := sorted(set(re.findall(r"pub const (M\d+)_SYSCALLS\b", code)) - set(tables)):
        raise CensusError(f"a table of a shape this can't read: {', '.join(unread)}_SYSCALLS")
    if not tables:
        raise CensusError("no `pub const M<n>_SYSCALLS` tables")
    return tables


def uncovered(census_names: Iterable[str], tables: dict[str, list[str]]) -> list[str]:
    """The census's syscalls that are in none of the kernel's tables and not
    -ENOSYS by a recorded decision (`ENOSYS_BY_DECISION`), in number order:
    M8's table must leave none (ARCHITECTURE section 8)."""
    known = set(ENOSYS_BY_DECISION).union(*tables.values())
    return sorted(set(census_names) - known, key=X86_64.__getitem__)


def classify(name: str, implemented: set[str]) -> str:
    """`implemented`, `ENOSYS` (by decision), the milestone planning it
    (`M8`, `M9`, `M10`), or `unplanned`."""
    if name in implemented:
        return "implemented"
    if name in ENOSYS_BY_DECISION:
        return "ENOSYS"
    for milestone, names in PLANNED.items():
        if name in names:
            return milestone
    return "unplanned"


_VDSO_FOOTER = (
    "",
    "vDSO calls (Linux normally answers these without a syscall, so the census can't see them):",
    f"{', '.join(VDSO)}.",
    "Without a vDSO, musl makes clock_gettime (for gettimeofday and time too) and getcpu",
    "syscalls on Lumen.",
)


def _format_syscall_row(name: str, entry: dict, verbose: bool) -> str:
    line = f"  {entry['nr']:>3} {name:<20} {entry['calls']:>7}  {', '.join(entry['workloads'])}"
    if verbose:
        for label, values in entry.get("args", {}).items():
            line += f"\n        {label}: {' '.join(values)}"
        if entry.get("errors"):
            errs = " ".join(f"{e}={n}" for e, n in entry["errors"].items())
            line += f"\n        errors: {errs}"
    return line


def report(data: dict, implemented: set[str], verbose: bool = False) -> str:
    """The census against the kernel: unplanned syscalls first, then each
    milestone's, then the implemented ones; the vDSO calls listed apart."""
    rows: dict[str, list[str]] = {}
    for name, entry in data["syscalls"].items():
        cls = classify(name, implemented)
        rows.setdefault(cls, []).append(_format_syscall_row(name, entry, verbose))
    meta = data["meta"]
    out = [
        (
            f"Census of {', '.join(meta['workloads'])} on Alpine {meta['alpine']} "
            f"(kernel {meta['kernel']}): {len(data['syscalls'])} syscalls."
        ),
    ]
    for cls in ("unplanned", "ENOSYS", *PLANNED, "implemented"):
        if cls in rows:
            out += ["", f"{cls} ({len(rows[cls])}):", *rows[cls]]
    out += [
        "",
        "implemented: the kernel dispatches the number (an M8 stub returning -ENOSYS until",
        "its issue lands is listed under its milestone); neither the arguments",
        "seen nor the semantics (a thread's exit, each ioctl request) are checked. ENOSYS:",
        "-ENOSYS by a decision ARCHITECTURE section 8 records (ENOSYS_BY_DECISION).",
        "ERESTART* errors: the kernel's restart codes, where a signal interrupted the call;",
        "the program saw EINTR or a restart, by the code, the handler and SA_RESTART.",
        *_VDSO_FOOTER,
    ]
    return "\n".join(out)


# Linux include/uapi/linux/time.h: CLOCK_PROCESS_CPUTIME_ID (2) and
# CLOCK_THREAD_CPUTIME_ID (3); negative clockids are dynamic POSIX CPU clocks
# (clock_getcpuclockid / pthread_getcpuclockid).
CPU_TIME_CLOCK_IDS = frozenset({2, 3})
CLOCK_SYSCALLS = frozenset(
    {"clock_gettime", "clock_getres", "clock_nanosleep", "timer_create", "timerfd_create"}
)
# Linux include/uapi/linux/sockios.h: SIOC*IF* range (SIOCGIFNAME 0x8910 .. 0x893f).
SIOCGIF_MIN = 0x8910
SIOCGIF_MAX = 0x893F
# Linux include/linux/socket.h: AF_NETLINK (16), AF_PACKET (17).
AF_NETLINK = "16"
AF_PACKET = "17"
_NOT_SEEN_NET_PROBE = "not seen (no workload runs udhcpc, getifaddrs or if_nameindex)"


def cpu_time_clocks_seen(data: dict) -> dict[str, list[str]]:
    """CPU-time clock IDs (2, 3, or dynamic negative IDs) recorded on clock
    syscalls in `data`, mapped by syscall name in number order."""
    found: dict[str, list[str]] = {}
    for name in sorted(data["syscalls"], key=X86_64.__getitem__):
        if name not in CLOCK_SYSCALLS:
            continue
        clocks = [
            c
            for c in data["syscalls"][name].get("args", {}).get("clock", ())
            if int(c, 0) in CPU_TIME_CLOCK_IDS or int(c, 0) < 0
        ]
        if clocks:
            found[name] = sorted(clocks, key=_sort_key)
    return found


def _probe_call(syscalls_map: dict, name: str) -> str:
    entry = syscalls_map.get(name)
    if not entry:
        return "none"
    return f"{entry['calls']} calls ({', '.join(entry['workloads'])})"


def report_m9(data: dict, tables: dict[str, list[str]], verbose: bool = False) -> str:
    """Compare an M9 census against the M6–M8 kernel tables and ROADMAP's
    M9/M10 lists, surfacing M10 rows and M8 `ENOSYS_BY_DECISION` calls
    (plus CPU-time clocks separately) seen in the census, and #450 design probes."""
    m6_m8: set[str] = set().union(*(tables.get(m, ()) for m in ("M6", "M7", "M8")))
    rows: dict[str, list[str]] = {}
    syscalls_map = data["syscalls"]
    for name in sorted(syscalls_map, key=X86_64.__getitem__):
        entry = syscalls_map[name]
        cls = classify(name, m6_m8)
        rows.setdefault(cls, []).append(_format_syscall_row(name, entry, verbose))

    cpu_clocks = cpu_time_clocks_seen(data)
    meta = data["meta"]
    out = [
        (
            f"M9 census of {', '.join(meta['workloads'])} on Alpine {meta['alpine']} "
            f"(kernel {meta['kernel']}): {len(syscalls_map)} syscalls."
        ),
    ]
    sections = (
        ("unplanned", "unplanned"),
        ("M10", "M10 rows seen in the M9 census"),
        ("ENOSYS", "M8 ENOSYS_BY_DECISION seen in the M9 census"),
        ("M8", "M8 planned"),
        ("M9", "M9 planned"),
        ("implemented", "M6-M8 tables"),
    )
    for key, title in sections:
        if key in rows:
            out += ["", f"{title} ({len(rows[key])}):", *rows[key]]
        if key == "ENOSYS" and cpu_clocks:
            clock_lines = [
                f"  {name}: {', '.join(clocks)} ({', '.join(syscalls_map[name]['workloads'])})"
                for name, clocks in cpu_clocks.items()
            ]
            out += [
                "",
                f"CPU-time clocks seen in the M9 census ({len(clock_lines)}):",
                *clock_lines,
            ]

    sock_domains = set(syscalls_map.get("socket", {}).get("args", {}).get("domain", ()))
    ioctl_reqs = syscalls_map.get("ioctl", {}).get("args", {}).get("request", ())
    siocgif = [r for r in ioctl_reqs if SIOCGIF_MIN <= int(r, 0) <= SIOCGIF_MAX]
    cpu_probe = (
        "; ".join(f"{name}={','.join(clocks)}" for name, clocks in cpu_clocks.items())
        if cpu_clocks
        else "none"
    )
    out += [
        "",
        "M9 design probes (#450):",
        f"  AF_NETLINK (domain 16): {'seen' if AF_NETLINK in sock_domains else _NOT_SEEN_NET_PROBE}",
        f"  AF_PACKET (domain 17): {'seen' if AF_PACKET in sock_domains else _NOT_SEEN_NET_PROBE}",
        f"  SIOC*IF* ioctls (0x8910–0x893f): {', '.join(siocgif) if siocgif else _NOT_SEEN_NET_PROBE}",
        f"  accept: {_probe_call(syscalls_map, 'accept')}; accept4: {_probe_call(syscalls_map, 'accept4')}",
        f"  sendmmsg: {_probe_call(syscalls_map, 'sendmmsg')}; recvmmsg: {_probe_call(syscalls_map, 'recvmmsg')}",
        f"  epoll_pwait2: {_probe_call(syscalls_map, 'epoll_pwait2')}",
        f"  sendfile: {_probe_call(syscalls_map, 'sendfile')}",
        f"  CPU-time clocks: {cpu_probe}",
        *_VDSO_FOOTER,
    ]
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build", help="traces into the census file")
    b.add_argument("--traces", type=Path, required=True)
    b.add_argument("--meta", type=Path, required=True)
    b.add_argument("--milestone", choices=("M8", "M9"), default="M8")
    b.add_argument("-o", "--output", type=Path, required=True)
    r = sub.add_parser("report", help="the census against the kernel's dispatch table")
    r.add_argument("--milestone", choices=("M8", "M9"), default="M8")
    r.add_argument(
        "--census",
        type=Path,
        help="default: tests/linux/m8-census.toml (M8) or tests/linux/m9-census.toml (M9)",
    )
    r.add_argument("--dispatch", type=Path, help="default: kernel/src/linux/syscall/mod.rs")
    r.add_argument("--tables", type=Path, help="default: kernel/src/linux/mod.rs (M9 report)")
    r.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.cmd == "build":
            args.output.write_text(build(args.traces, args.meta, milestone=args.milestone))
            return 0
        # The repo root, three levels above tools/harness/lumen_harness/ (only
        # here: inside the census container this file sits at /harness/syscalls.py).
        root = Path(__file__).resolve().parents[3]
        if args.milestone == "M9":
            census_file = args.census or root / "tests/linux/m9-census.toml"
            tables_file = args.tables or root / "kernel/src/linux/mod.rs"
            data = tomllib.loads(census_file.read_text())
            tables = kernel_tables(tables_file.read_text())
            print(report_m9(data, tables, args.verbose))
            return 0
        census_file = args.census or root / "tests/linux/m8-census.toml"
        dispatch_file = args.dispatch or root / "kernel/src/linux/syscall/mod.rs"
        data = tomllib.loads(census_file.read_text())
        # A stub (`not_implemented`) is dispatched but not implemented.
        stubbed = stubs(cluster_sources(dispatch_file.parent))
        implemented = dispatched(dispatch_file.read_text()) - set(stubbed)
        print(report(data, implemented, args.verbose))
        return 0
    except CensusError as e:
        print(f"syscall census: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
