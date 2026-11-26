//! Shared Linux timespec copies and restart constants for blocking syscalls.

use super::{copy_pod_from_user, copy_pod_to_user};
use crate::linux::{errno, general};
use crate::user::CopyError;

/// Positive restart errno for calls that restart only without a handler.
pub(super) const ERESTARTNOHAND: u32 = lumen_signal::restart::RestartCode::NoHand as u32;
/// Nanoseconds per second.
pub(super) const NS_PER_SEC: u64 = 1_000_000_000;

/// Linux x86-64 timespec, with every bit initialized and no padding.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct RawTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

const _: () = {
    assert!(size_of::<RawTimespec>() == size_of::<general::timespec>());
    assert!(core::mem::offset_of!(RawTimespec, tv_sec) == core::mem::offset_of!(general::timespec, tv_sec));
    assert!(core::mem::offset_of!(RawTimespec, tv_nsec) == core::mem::offset_of!(general::timespec, tv_nsec));
};

/// Copy and validate a non-null timespec, saturating nanosecond overflow.
pub(super) fn read_timespec(addr: u64) -> Result<u64, u32> {
    // SAFETY: RawTimespec is two i64s; every bit pattern is valid.
    let ts = unsafe { copy_pod_from_user::<RawTimespec>(addr) }.map_err(CopyError::to_errno)?;
    if ts.tv_sec < 0 || !(0..NS_PER_SEC as i64).contains(&ts.tv_nsec) {
        return Err(errno::EINVAL);
    }
    Ok((ts.tv_sec as u64).saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec as u64))
}

/// Write a normalized timespec containing exactly the supplied nanoseconds.
pub(super) fn write_timespec(addr: u64, ns: u64) -> Result<(), u32> {
    let ts = RawTimespec { tv_sec: (ns / NS_PER_SEC) as i64, tv_nsec: (ns % NS_PER_SEC) as i64 };
    // SAFETY: RawTimespec is initialized and has no padding.
    unsafe { copy_pod_to_user(addr, &ts) }.map_err(CopyError::to_errno)
}
