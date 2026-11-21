//! File status and access checks (M7): `stat`, `lstat`, `newfstatat`,
//! `statx`, `access`, `faccessat`, `faccessat2`.

use super::at::{Dirfd, UserPtr};
use super::not_implemented;
use crate::linux::general;
use lumen_vfs::{AccessMask, AtFlags};

/// `stat(path, buf)` (4): M7 stub.
pub(super) fn sys_stat(_path: UserPtr, _buf: UserPtr) -> u64 {
    not_implemented(general::__NR_stat)
}

/// `lstat(path, buf)` (6): M7 stub.
pub(super) fn sys_lstat(_path: UserPtr, _buf: UserPtr) -> u64 {
    not_implemented(general::__NR_lstat)
}

/// `newfstatat(dirfd, path, buf, flags)` (262): M7 stub.
pub(super) fn sys_newfstatat(_dirfd: Dirfd, _path: UserPtr, _buf: UserPtr, _flags: AtFlags) -> u64 {
    not_implemented(general::__NR_newfstatat)
}

/// `statx(dirfd, path, flags, mask, buf)` (332): M7 stub.
pub(super) fn sys_statx(_dirfd: Dirfd, _path: UserPtr, _flags: AtFlags, _mask: u32, _buf: UserPtr) -> u64 {
    not_implemented(general::__NR_statx)
}

/// `access(path, mode)` (21): M7 stub.
pub(super) fn sys_access(_path: UserPtr, _mode: AccessMask) -> u64 {
    not_implemented(general::__NR_access)
}

/// `faccessat(dirfd, path, mode)` (269): M7 stub. Three arguments on Linux
/// x86-64: `r10` is never passed (`musl` leaves it uninitialized).
pub(super) fn sys_faccessat(_dirfd: Dirfd, _path: UserPtr, _mode: AccessMask) -> u64 {
    not_implemented(general::__NR_faccessat)
}

/// `faccessat2(dirfd, path, mode, flags)` (439): M7 stub.
pub(super) fn sys_faccessat2(_dirfd: Dirfd, _path: UserPtr, _mode: AccessMask, _flags: AtFlags) -> u64 {
    not_implemented(general::__NR_faccessat2)
}

crate::ktest!(
    fn stat_m7_stubs_return_enosys() {
        super::check_stubs(&[
            general::__NR_stat,
            general::__NR_lstat,
            general::__NR_newfstatat,
            general::__NR_statx,
            general::__NR_access,
            general::__NR_faccessat,
            general::__NR_faccessat2,
        ])
    }
);
