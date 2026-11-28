//! `eventfd` and `timerfd` syscalls (M9 B0 stubs, #471; replaced by B6,
//! ARCHITECTURE sections 8 and 10).

use super::at::{Fd, UserPtr};
use super::not_implemented;
use crate::linux::general;

/// `timerfd_create(clockid, flags)` (283): M9 stub, replaced by B6.
pub(super) fn sys_timerfd_create(_clockid: i32, _flags: i32) -> u64 {
    not_implemented(general::__NR_timerfd_create)
}

/// `eventfd(initval)` (284): M9 stub, replaced by B6.
pub(super) fn sys_eventfd(_initval: u32) -> u64 {
    not_implemented(general::__NR_eventfd)
}

/// `timerfd_settime(fd, flags, new, old)` (286): M9 stub, replaced by B6.
pub(super) fn sys_timerfd_settime(_fd: Fd, _flags: i32, _new: UserPtr, _old: UserPtr) -> u64 {
    not_implemented(general::__NR_timerfd_settime)
}

/// `timerfd_gettime(fd, cur)` (287): M9 stub, replaced by B6.
pub(super) fn sys_timerfd_gettime(_fd: Fd, _cur: UserPtr) -> u64 {
    not_implemented(general::__NR_timerfd_gettime)
}

/// `eventfd2(initval, flags)` (290): M9 stub, replaced by B6.
pub(super) fn sys_eventfd2(_initval: u32, _flags: i32) -> u64 {
    not_implemented(general::__NR_eventfd2)
}
