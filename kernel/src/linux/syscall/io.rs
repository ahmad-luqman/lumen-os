//! Positional I/O and syncing (M7): `pread64`, `pwrite64`, `fsync`,
//! `fdatasync`, `sync` (#254), and `msync` (#255, with file-backed `mmap`).

use super::at::{Fd, UserPtr};
use super::not_implemented;
use crate::linux::general;

/// `pread64(fd, buf, count, offset)` (17): M7 stub.
pub(super) fn sys_pread64(_fd: Fd, _buf: UserPtr, _count: u64, _offset: u64) -> u64 {
    not_implemented(general::__NR_pread64)
}

/// `pwrite64(fd, buf, count, offset)` (18): M7 stub.
pub(super) fn sys_pwrite64(_fd: Fd, _buf: UserPtr, _count: u64, _offset: u64) -> u64 {
    not_implemented(general::__NR_pwrite64)
}

/// `fsync(fd)` (74): M7 stub.
pub(super) fn sys_fsync(_fd: Fd) -> u64 {
    not_implemented(general::__NR_fsync)
}

/// `fdatasync(fd)` (75): M7 stub.
pub(super) fn sys_fdatasync(_fd: Fd) -> u64 {
    not_implemented(general::__NR_fdatasync)
}

/// `sync()` (162): M7 stub.
pub(super) fn sys_sync() -> u64 {
    not_implemented(general::__NR_sync)
}

crate::ktest!(
    fn io_m7_stubs_return_enosys() {
        super::check_stubs(&[
            general::__NR_pread64,
            general::__NR_pwrite64,
            general::__NR_fsync,
            general::__NR_fdatasync,
            general::__NR_sync,
        ])
    }
);
