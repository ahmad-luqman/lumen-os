//! Futexes and sleeps (M8's cluster, ARCHITECTURE section 8, "B0's
//! stubs"): `futex`, `set_robust_list`, `get_robust_list`, `nanosleep`,
//! `clock_nanosleep`.

use super::at::UserPtr;
use super::not_implemented;
use crate::linux::general;

/// `futex(uaddr, op, val, timeout_or_val2, uaddr2, val3)` (202): M8 stub, replaced by C4 (#383).
pub(super) fn sys_futex(
    _uaddr: UserPtr,
    _op: u32,
    _val: u32,
    _timeout_or_val2: u64,
    _uaddr2: UserPtr,
    _val3: u32,
) -> u64 {
    not_implemented(general::__NR_futex)
}

/// `set_robust_list(head, len)` (273): M8 stub, replaced by C4 (#383).
pub(super) fn sys_set_robust_list(_head: UserPtr, _len: u64) -> u64 {
    not_implemented(general::__NR_set_robust_list)
}

/// `get_robust_list(pid, head_ptr, len_ptr)` (274): M8 stub, replaced by C4 (#383).
pub(super) fn sys_get_robust_list(_pid: i32, _head_ptr: UserPtr, _len_ptr: UserPtr) -> u64 {
    not_implemented(general::__NR_get_robust_list)
}

/// `nanosleep(req, rem)` (35): M8 stub, replaced by C4 (#383).
pub(super) fn sys_nanosleep(_req: UserPtr, _rem: UserPtr) -> u64 {
    not_implemented(general::__NR_nanosleep)
}

/// `clock_nanosleep(clockid, flags, req, rem)` (230): M8 stub, replaced by C4 (#383).
pub(super) fn sys_clock_nanosleep(_clockid: i32, _flags: u32, _req: UserPtr, _rem: UserPtr) -> u64 {
    not_implemented(general::__NR_clock_nanosleep)
}
