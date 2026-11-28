//! Pure `no_std` Linux `eventfd` counter and `timerfd` expiry-count logic.
//!
//! Models the counter state machine of `fs/eventfd.c` ([`EventCounter`]) and
//! the arming, expiry accounting, overrun forwarding, and remaining-time
//! queries of `fs/timerfd.c` ([`TimerCount`]). No kernel locks, file
//! descriptors, wait queues, or clock reads live here. See [`timerfd`] for the
//! monotonic timebase contract, worker [`Requeue`] rules, and `u64::MAX`
//! ("never") sentinel.
//!
//! # Allocation and panic guarantee
//!
//! **No operation in this crate allocates, and every operation is panic-free.**
//! `lumen-eventfd` does not depend on `alloc`; every type is `Copy` and
//! stack-sized, and all invalid inputs return typed errors rather than
//! panicking.
//!
//! # `TFD_TIMER_CANCEL_ON_SET` (#450)
//!
//! [`TimerfdSettimeFlags::from_bits`] accepts `TFD_TIMER_CANCEL_ON_SET` and
//! [`TimerCount`] never cancels on clock discontinuity: Lumen cannot set
//! `CLOCK_REALTIME`, so per #450 the flag is accepted as a no-op and never
//! produces `ECANCELED`.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod eventfd;
pub mod timerfd;

use core::fmt;

pub use eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, EventCounter, EventfdFlags, MAX_COUNTER, WriteError};
pub use timerfd::{
    CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_REALTIME, CLOCK_REALTIME_ALARM, ClockError, ITimer,
    Invalid, KTIME_MAX_NS, Requeue, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME, TFD_TIMER_CANCEL_ON_SET, TimerClock,
    TimerCount, TimerfdCreateFlags, TimerfdSettimeFlags, itimerspec_to_ns,
};

/// Size in bytes of an `eventfd` or `timerfd` `read(2)`/`write(2)` transfer
/// (`sizeof(__u64)` in Linux `fs/eventfd.c` and `fs/timerfd.c`).
pub const IO_SIZE: usize = core::mem::size_of::<u64>();

/// The operation cannot complete immediately (`-EAGAIN`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WouldBlock;

impl fmt::Display for WouldBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation would block")
    }
}

impl core::error::Error for WouldBlock {}

/// Error returned by byte-slice `read_bytes` helpers on [`EventCounter`] and
/// [`TimerCount`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadBytesError {
    /// Destination buffer is shorter than [`IO_SIZE`] (8 bytes; `-EINVAL`).
    Invalid,
    /// Counter or expiry count is zero (`-EAGAIN`).
    WouldBlock,
}

impl From<WouldBlock> for ReadBytesError {
    fn from(_: WouldBlock) -> Self {
        Self::WouldBlock
    }
}

impl fmt::Display for ReadBytesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("buffer shorter than 8 bytes"),
            Self::WouldBlock => f.write_str("operation would block"),
        }
    }
}

impl core::error::Error for ReadBytesError {}

/// Unrecognized bits were set in a Linux flags bitmask (`-EINVAL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidFlags;

impl fmt::Display for InvalidFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unrecognized flag bits")
    }
}

impl core::error::Error for InvalidFlags {}
