//! Socket syscalls (M9 B0 stubs, #471; B1 (#472) writes generic `SockOps`
//! dispatch for every handler here, and each family issue implements its
//! behaviour, ARCHITECTURE section 8).

use super::at::{Fd, UserPtr};
use super::not_implemented;
use crate::linux::general;

/// `socket(domain, type, protocol)` (41): M9 stub, replaced by B1 (#472),
/// C1 (#478), C3 (#480), C4 (#481) and C6 (#483).
pub(super) fn sys_socket(_domain: i32, _ty: i32, _protocol: i32) -> u64 {
    not_implemented(general::__NR_socket)
}

/// `connect(fd, addr, addrlen)` (42): M9 stub, replaced by C1 (#478),
/// C3 (#480) and C4 (#481).
pub(super) fn sys_connect(_fd: Fd, _addr: UserPtr, _addrlen: i32) -> u64 {
    not_implemented(general::__NR_connect)
}

/// `accept(fd, addr, addrlen)` (43): M9 stub, replaced by C1 (#478) and
/// C4 (#481).
pub(super) fn sys_accept(_fd: Fd, _addr: UserPtr, _addrlen: UserPtr) -> u64 {
    not_implemented(general::__NR_accept)
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` (44): M9 stub, replaced by
/// C1 (#478), C3 (#480) and C4 (#481).
pub(super) fn sys_sendto(_fd: Fd, _buf: UserPtr, _len: u64, _flags: u32, _addr: UserPtr, _addrlen: i32) -> u64 {
    not_implemented(general::__NR_sendto)
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)` (45): M9 stub, replaced by
/// C1 (#478), C3 (#480) and C4 (#481).
pub(super) fn sys_recvfrom(_fd: Fd, _buf: UserPtr, _len: u64, _flags: u32, _addr: UserPtr, _addrlen: UserPtr) -> u64 {
    not_implemented(general::__NR_recvfrom)
}

/// `sendmsg(fd, msg, flags)` (46): M9 stub, replaced by C1 (#478), C2 (#479),
/// C3 (#480), C4 (#481) and C5 (#482).
pub(super) fn sys_sendmsg(_fd: Fd, _msg: UserPtr, _flags: u32) -> u64 {
    not_implemented(general::__NR_sendmsg)
}

/// `recvmsg(fd, msg, flags)` (47): M9 stub, replaced by C1 (#478), C2 (#479),
/// C3 (#480), C4 (#481) and C5 (#482).
pub(super) fn sys_recvmsg(_fd: Fd, _msg: UserPtr, _flags: u32) -> u64 {
    not_implemented(general::__NR_recvmsg)
}

/// `shutdown(fd, how)` (48): M9 stub, replaced by C1 (#478) and C4 (#481).
pub(super) fn sys_shutdown(_fd: Fd, _how: i32) -> u64 {
    not_implemented(general::__NR_shutdown)
}

/// `bind(fd, addr, addrlen)` (49): M9 stub, replaced by C1 (#478), C3 (#480)
/// and C4 (#481).
pub(super) fn sys_bind(_fd: Fd, _addr: UserPtr, _addrlen: i32) -> u64 {
    not_implemented(general::__NR_bind)
}

/// `listen(fd, backlog)` (50): M9 stub, replaced by C1 (#478) and C4 (#481).
pub(super) fn sys_listen(_fd: Fd, _backlog: i32) -> u64 {
    not_implemented(general::__NR_listen)
}

/// `getsockname(fd, addr, addrlen)` (51): M9 stub, replaced by C5 (#482).
pub(super) fn sys_getsockname(_fd: Fd, _addr: UserPtr, _addrlen: UserPtr) -> u64 {
    not_implemented(general::__NR_getsockname)
}

/// `getpeername(fd, addr, addrlen)` (52): M9 stub, replaced by C5 (#482).
pub(super) fn sys_getpeername(_fd: Fd, _addr: UserPtr, _addrlen: UserPtr) -> u64 {
    not_implemented(general::__NR_getpeername)
}

/// `socketpair(domain, type, protocol, sv)` (53): M9 stub, replaced by
/// B1 (#472) and C1 (#478).
pub(super) fn sys_socketpair(_domain: i32, _ty: i32, _protocol: i32, _sv: UserPtr) -> u64 {
    not_implemented(general::__NR_socketpair)
}

/// `setsockopt(fd, level, name, val, len)` (54): M9 stub, replaced by
/// C5 (#482).
pub(super) fn sys_setsockopt(_fd: Fd, _level: i32, _name: i32, _val: UserPtr, _len: i32) -> u64 {
    not_implemented(general::__NR_setsockopt)
}

/// `getsockopt(fd, level, name, val, len)` (55): M9 stub, replaced by
/// C5 (#482).
pub(super) fn sys_getsockopt(_fd: Fd, _level: i32, _name: i32, _val: UserPtr, _len: UserPtr) -> u64 {
    not_implemented(general::__NR_getsockopt)
}

/// `accept4(fd, addr, addrlen, flags)` (288): M9 stub, replaced by C1 (#478)
/// and C4 (#481).
pub(super) fn sys_accept4(_fd: Fd, _addr: UserPtr, _addrlen: UserPtr, _flags: i32) -> u64 {
    not_implemented(general::__NR_accept4)
}
