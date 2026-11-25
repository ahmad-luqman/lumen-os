//! Pipes, `dup*` and `mknod*` (M8's cluster, ARCHITECTURE section 8,
//! "B0's stubs"): `pipe`, `pipe2`, `dup`, `dup2`, `dup3`, `mknod`, `mknodat`.

use super::at::{Dirfd, Fd, Mode, UserPtr};
use super::not_implemented;
use crate::linux::general;
use lumen_vfs::OpenFlags;

/// `pipe(fds)` (22): M8 stub, replaced by C7 (#386).
pub(super) fn sys_pipe(_fds: UserPtr) -> u64 {
    not_implemented(general::__NR_pipe)
}

/// `pipe2(fds, flags)` (293): M8 stub, replaced by C7 (#386).
pub(super) fn sys_pipe2(_fds: UserPtr, _flags: OpenFlags) -> u64 {
    not_implemented(general::__NR_pipe2)
}

/// `dup(oldfd)` (32): M8 stub, replaced by C7 (#386).
pub(super) fn sys_dup(_oldfd: Fd) -> u64 {
    not_implemented(general::__NR_dup)
}

/// `dup2(oldfd, newfd)` (33): M8 stub, replaced by C7 (#386).
pub(super) fn sys_dup2(_oldfd: Fd, _newfd: Fd) -> u64 {
    not_implemented(general::__NR_dup2)
}

/// `dup3(oldfd, newfd, flags)` (292): M8 stub, replaced by C7 (#386).
pub(super) fn sys_dup3(_oldfd: Fd, _newfd: Fd, _flags: OpenFlags) -> u64 {
    not_implemented(general::__NR_dup3)
}

/// `mknod(path, mode, dev)` (133): M8 stub, replaced by C7 (#386).
pub(super) fn sys_mknod(_path: UserPtr, _mode: Mode, _dev: u32) -> u64 {
    not_implemented(general::__NR_mknod)
}

/// `mknodat(dirfd, path, mode, dev)` (259): M8 stub, replaced by C7 (#386).
pub(super) fn sys_mknodat(_dirfd: Dirfd, _path: UserPtr, _mode: Mode, _dev: u32) -> u64 {
    not_implemented(general::__NR_mknodat)
}
