//! `epoll` syscalls (M9 B0 stubs, #471; replaced by B5 (#476), ARCHITECTURE
//! sections 8 and 10).

use super::at::{Fd, UserPtr};
use super::not_implemented;
use crate::linux::general;

/// `epoll_create(size)` (213): M9 stub, replaced by B5 (#476).
pub(super) fn sys_epoll_create(_size: i32) -> u64 {
    not_implemented(general::__NR_epoll_create)
}

/// `epoll_wait(epfd, events, maxevents, timeout)` (232): M9 stub, replaced by
/// B5 (#476).
pub(super) fn sys_epoll_wait(_epfd: Fd, _events: UserPtr, _maxevents: i32, _timeout: i32) -> u64 {
    not_implemented(general::__NR_epoll_wait)
}

/// `epoll_ctl(epfd, op, fd, event)` (233): M9 stub, replaced by B5 (#476).
pub(super) fn sys_epoll_ctl(_epfd: Fd, _op: i32, _fd: Fd, _event: UserPtr) -> u64 {
    not_implemented(general::__NR_epoll_ctl)
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)` (281):
/// M9 stub, replaced by B5 (#476).
pub(super) fn sys_epoll_pwait(
    _epfd: Fd,
    _events: UserPtr,
    _maxevents: i32,
    _timeout: i32,
    _sigmask: UserPtr,
    _sigsetsize: u64,
) -> u64 {
    not_implemented(general::__NR_epoll_pwait)
}

/// `epoll_create1(flags)` (291): M9 stub, replaced by B5 (#476).
pub(super) fn sys_epoll_create1(_flags: i32) -> u64 {
    not_implemented(general::__NR_epoll_create1)
}

/// `epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)` (441):
/// M9 stub, replaced by B5 (#476).
pub(super) fn sys_epoll_pwait2(
    _epfd: Fd,
    _events: UserPtr,
    _maxevents: i32,
    _timeout: UserPtr,
    _sigmask: UserPtr,
    _sigsetsize: u64,
) -> u64 {
    not_implemented(general::__NR_epoll_pwait2)
}
