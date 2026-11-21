//! Descriptor control and advisory locks (M7): `fcntl`, `flock`.

use super::not_implemented;
use crate::linux::general;

/// `fcntl(fd, cmd, arg)` (72): M7 stub.
pub(super) fn sys_fcntl(_fd: u64, _cmd: u64, _arg: u64) -> u64 {
    not_implemented(general::__NR_fcntl)
}

/// `flock(fd, operation)` (73): M7 stub.
pub(super) fn sys_flock(_fd: u64, _operation: u64) -> u64 {
    not_implemented(general::__NR_flock)
}

crate::ktest!(
    fn lock_m7_stubs_return_enosys() {
        super::check_stubs(&[general::__NR_fcntl, general::__NR_flock])
    }
);
