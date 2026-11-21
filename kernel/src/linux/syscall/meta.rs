//! Inode metadata and size (M7): `chmod`, `fchmod`, `fchmodat`,
//! `fchmodat2`, `chown`, `fchown`, `lchown`, `fchownat`, `umask`,
//! `utimensat`, `truncate`, `ftruncate`.

use super::not_implemented;
use crate::linux::general;

/// `chmod(path, mode)` (90): M7 stub.
pub(super) fn sys_chmod(_path: u64, _mode: u64) -> u64 {
    not_implemented(general::__NR_chmod)
}

/// `fchmod(fd, mode)` (91): M7 stub.
pub(super) fn sys_fchmod(_fd: u64, _mode: u64) -> u64 {
    not_implemented(general::__NR_fchmod)
}

/// `fchmodat(dirfd, path, mode)` (268): M7 stub. Three arguments on Linux
/// x86-64: `r10` is never passed (`musl` leaves it uninitialized).
pub(super) fn sys_fchmodat(_dirfd: u64, _path: u64, _mode: u64) -> u64 {
    not_implemented(general::__NR_fchmodat)
}

/// `fchmodat2(dirfd, path, mode, flags)` (452): M7 stub.
pub(super) fn sys_fchmodat2(_dirfd: u64, _path: u64, _mode: u64, _flags: u64) -> u64 {
    not_implemented(general::__NR_fchmodat2)
}

/// `chown(path, uid, gid)` (92): M7 stub.
pub(super) fn sys_chown(_path: u64, _uid: u64, _gid: u64) -> u64 {
    not_implemented(general::__NR_chown)
}

/// `fchown(fd, uid, gid)` (93): M7 stub.
pub(super) fn sys_fchown(_fd: u64, _uid: u64, _gid: u64) -> u64 {
    not_implemented(general::__NR_fchown)
}

/// `lchown(path, uid, gid)` (94): M7 stub.
pub(super) fn sys_lchown(_path: u64, _uid: u64, _gid: u64) -> u64 {
    not_implemented(general::__NR_lchown)
}

/// `fchownat(dirfd, path, uid, gid, flags)` (260): M7 stub.
pub(super) fn sys_fchownat(_dirfd: u64, _path: u64, _uid: u64, _gid: u64, _flags: u64) -> u64 {
    not_implemented(general::__NR_fchownat)
}

/// `umask(mask)` (95): M7 stub.
pub(super) fn sys_umask(_mask: u64) -> u64 {
    not_implemented(general::__NR_umask)
}

/// `utimensat(dirfd, path, times, flags)` (280): M7 stub.
pub(super) fn sys_utimensat(_dirfd: u64, _path: u64, _times: u64, _flags: u64) -> u64 {
    not_implemented(general::__NR_utimensat)
}

/// `truncate(path, length)` (76): M7 stub.
pub(super) fn sys_truncate(_path: u64, _length: u64) -> u64 {
    not_implemented(general::__NR_truncate)
}

/// `ftruncate(fd, length)` (77): M7 stub.
pub(super) fn sys_ftruncate(_fd: u64, _length: u64) -> u64 {
    not_implemented(general::__NR_ftruncate)
}

crate::ktest!(
    fn meta_m7_stubs_return_enosys() {
        super::check_stubs(&[
            general::__NR_chmod,
            general::__NR_fchmod,
            general::__NR_fchmodat,
            general::__NR_fchmodat2,
            general::__NR_chown,
            general::__NR_fchown,
            general::__NR_lchown,
            general::__NR_fchownat,
            general::__NR_umask,
            general::__NR_utimensat,
            general::__NR_truncate,
            general::__NR_ftruncate,
        ])
    }
);
