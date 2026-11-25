//! Scheduling and system information (M8's cluster, ARCHITECTURE section 8,
//! "B0's stubs"): `sched_yield`, `sched_setaffinity`, `sched_getaffinity`,
//! `getpriority`, `setpriority`, `getrusage`, `sysinfo`, `prctl`, `statfs`,
//! `fstatfs`, `clock_getres`, `getcpu`, `setfsuid`, `setfsgid`.

use super::at::{Fd, UserPtr};
use super::not_implemented;
use crate::linux::general;

/// `sched_yield()` (24): M8 stub, replaced by C12 (#391).
pub(super) fn sys_sched_yield() -> u64 {
    not_implemented(general::__NR_sched_yield)
}

/// `sched_setaffinity(pid, len, mask)` (203): M8 stub, replaced by C12 (#391).
pub(super) fn sys_sched_setaffinity(_pid: i32, _len: u32, _mask: UserPtr) -> u64 {
    not_implemented(general::__NR_sched_setaffinity)
}

/// `sched_getaffinity(pid, len, mask)` (204): M8 stub, replaced by C12 (#391).
pub(super) fn sys_sched_getaffinity(_pid: i32, _len: u32, _mask: UserPtr) -> u64 {
    not_implemented(general::__NR_sched_getaffinity)
}

/// `getpriority(which, who)` (140): M8 stub, replaced by C12 (#391).
pub(super) fn sys_getpriority(_which: i32, _who: i32) -> u64 {
    not_implemented(general::__NR_getpriority)
}

/// `setpriority(which, who, prio)` (141): M8 stub, replaced by C12 (#391).
pub(super) fn sys_setpriority(_which: i32, _who: i32, _prio: i32) -> u64 {
    not_implemented(general::__NR_setpriority)
}

/// `getrusage(who, usage)` (98): M8 stub, replaced by C12 (#391).
pub(super) fn sys_getrusage(_who: i32, _usage: UserPtr) -> u64 {
    not_implemented(general::__NR_getrusage)
}

/// `sysinfo(info)` (99): M8 stub, replaced by C12 (#391).
pub(super) fn sys_sysinfo(_info: UserPtr) -> u64 {
    not_implemented(general::__NR_sysinfo)
}

/// `prctl(option, arg2, arg3, arg4, arg5)` (157): M8 stub, replaced by C12 (#391).
pub(super) fn sys_prctl(_option: i32, _arg2: u64, _arg3: u64, _arg4: u64, _arg5: u64) -> u64 {
    not_implemented(general::__NR_prctl)
}

/// `statfs(path, buf)` (137): M8 stub, replaced by C12 (#391).
pub(super) fn sys_statfs(_path: UserPtr, _buf: UserPtr) -> u64 {
    not_implemented(general::__NR_statfs)
}

/// `fstatfs(fd, buf)` (138): M8 stub, replaced by C12 (#391).
pub(super) fn sys_fstatfs(_fd: Fd, _buf: UserPtr) -> u64 {
    not_implemented(general::__NR_fstatfs)
}

/// `clock_getres(clockid, res)` (229): M8 stub, replaced by C12 (#391).
pub(super) fn sys_clock_getres(_clockid: i32, _res: UserPtr) -> u64 {
    not_implemented(general::__NR_clock_getres)
}

/// `getcpu(cpu, node, cache)` (309): M8 stub, replaced by C12 (#391).
pub(super) fn sys_getcpu(_cpu: UserPtr, _node: UserPtr, _cache: UserPtr) -> u64 {
    not_implemented(general::__NR_getcpu)
}

/// `setfsuid(uid)` (122): M8 stub, replaced by C12 (#391).
pub(super) fn sys_setfsuid(_uid: u32) -> u64 {
    not_implemented(general::__NR_setfsuid)
}

/// `setfsgid(gid)` (123): M8 stub, replaced by C12 (#391).
pub(super) fn sys_setfsgid(_gid: u32) -> u64 {
    not_implemented(general::__NR_setfsgid)
}
