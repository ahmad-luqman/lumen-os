//! Working directory and directory reads: `getcwd` (M6), and M7's
//! `getdents64`, `chdir`, `fchdir` (stubs until #254 implements them).

use super::{neg_errno, not_implemented};
use crate::linux::{errno, general};
use crate::user::copy_to_user;

pub(super) fn sys_getcwd(buf: u64, size: u64) -> u64 {
    if size < 2 {
        return neg_errno(errno::ERANGE);
    }
    match copy_to_user(buf, b"/\0") {
        Ok(()) => 2,
        Err(e) => neg_errno(e.to_errno()),
    }
}

/// `getdents64(fd, dirp, count)` (217): M7 stub.
pub(super) fn sys_getdents64(_fd: u64, _dirp: u64, _count: u64) -> u64 {
    not_implemented(general::__NR_getdents64)
}

/// `chdir(path)` (80): M7 stub.
pub(super) fn sys_chdir(_path: u64) -> u64 {
    not_implemented(general::__NR_chdir)
}

/// `fchdir(fd)` (81): M7 stub.
pub(super) fn sys_fchdir(_fd: u64) -> u64 {
    not_implemented(general::__NR_fchdir)
}

crate::ktest!(
    fn dir_m7_stubs_return_enosys() {
        super::check_stubs(&[general::__NR_getdents64, general::__NR_chdir, general::__NR_fchdir])
    }
);
