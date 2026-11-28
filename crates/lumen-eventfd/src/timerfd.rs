//! Linux `timerfd` arming, expiry accounting, and remaining-time logic
//! (`fs/timerfd.c`).
//!
//! # Timebase and worker-queue contract
//!
//! [`TimerCount`] stores deadlines as `u64` nanoseconds on the kernel timer
//! worker's monotonic clock (`CLOCK_BOOTTIME` is treated as monotonic because
//! Lumen has no suspend). Callers pass monotonic `now` to [`TimerCount::arm`],
//! [`TimerCount::on_expiry`], [`TimerCount::read`], [`TimerCount::read_bytes`],
//! [`TimerCount::readable`], and [`TimerCount::get`]; `CLOCK_REALTIME` absolute
//! deadlines are converted onto the monotonic clock by
//! [`TimerCount::arm_realtime`] (or [`TimerCount::settime`]), preserving
//! pre-boot phase.
//!
//! A deadline of `u64::MAX` means "never": it does not fire, returns `None`
//! from [`TimerCount::next_wakeup`], and stays armed for [`TimerCount::get`].
//! A periodic forward that lands on or past `u64::MAX` is unrepresentable and
//! disarms (`deadline = None`).
//!
//! Every mutating operation (`arm`, `arm_realtime`, `settime`, `disarm`,
//! `on_expiry`, `read`, `read_bytes`) returns a [`Requeue`] carrying the exact
//! previously queued worker key to remove and the new key to insert. Callers
//! should replace their worker entry after every one of those calls so no timer
//! is stranded; a stale worker entry that fires before the current deadline is
//! harmless because [`TimerCount::on_expiry`] is a no-op before the deadline.
//!
//! No clock is read inside this module, no operation allocates, and every
//! operation is panic-free.

use core::fmt;

use crate::{IO_SIZE, InvalidFlags, ReadBytesError, WouldBlock};

/// `TFD_NONBLOCK` (`include/uapi/linux/timerfd.h` -> `O_NONBLOCK`, `0o4000`).
pub const TFD_NONBLOCK: u32 = 0x800;

/// `TFD_CLOEXEC` (`include/uapi/linux/timerfd.h` -> `O_CLOEXEC`, `0o2000000`).
pub const TFD_CLOEXEC: u32 = 0x8_0000;

/// `TFD_TIMER_ABSTIME` (`include/uapi/linux/timerfd.h`): interpret `it_value`
/// as an absolute timestamp on the timer's clock.
pub const TFD_TIMER_ABSTIME: u32 = 0x1;

/// `TFD_TIMER_CANCEL_ON_SET` (`include/uapi/linux/timerfd.h`): accepted by
/// [`TimerfdSettimeFlags::from_bits`] and never cancels, because Lumen cannot
/// set `CLOCK_REALTIME` (#450).
pub const TFD_TIMER_CANCEL_ON_SET: u32 = 0x2;

/// `CLOCK_REALTIME` (`include/uapi/linux/time.h`).
pub const CLOCK_REALTIME: i32 = 0;

/// `CLOCK_MONOTONIC` (`include/uapi/linux/time.h`).
pub const CLOCK_MONOTONIC: i32 = 1;

/// `CLOCK_BOOTTIME` (`include/uapi/linux/time.h`); treated as monotonic on
/// Lumen (no suspend).
pub const CLOCK_BOOTTIME: i32 = 7;

/// `CLOCK_REALTIME_ALARM` (`include/uapi/linux/time.h`).
pub const CLOCK_REALTIME_ALARM: i32 = 8;

/// `CLOCK_BOOTTIME_ALARM` (`include/uapi/linux/time.h`).
pub const CLOCK_BOOTTIME_ALARM: i32 = 9;

/// Maximum `ktime_t` in nanoseconds (`i64::MAX`, Linux `include/linux/ktime.h`
/// `KTIME_MAX`).
pub const KTIME_MAX_NS: u64 = i64::MAX as u64;

const NSEC_PER_SEC: u64 = 1_000_000_000;
/// `KTIME_SEC_MAX = KTIME_MAX / NSEC_PER_SEC` (`include/linux/ktime.h`).
const KTIME_SEC_MAX: i64 = (KTIME_MAX_NS / NSEC_PER_SEC) as i64;
/// Sentinel monotonic deadline meaning "never fires".
const NEVER: u64 = u64::MAX;

const TFD_CREATE_KNOWN_FLAGS: u32 = TFD_NONBLOCK | TFD_CLOEXEC;
const TFD_SETTIME_KNOWN_FLAGS: u32 = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;

/// Invalid `timespec` field (`tv_sec < 0`, `tv_nsec < 0`, or
/// `tv_nsec >= 1_000_000_000`; `-EINVAL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid;

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid timespec")
    }
}

impl core::error::Error for Invalid {}

/// Validate a `struct timespec` component of an `itimerspec` and convert it to
/// nanoseconds, clamping as Linux `include/linux/ktime.h` `ktime_set`.
///
/// Matches Linux `include/linux/time64.h` `timespec64_valid` /
/// `itimerspec64_valid` (`sec < 0`, `nsec < 0`, or `nsec >= 1_000_000_000` is
/// `-EINVAL`) and `ktime_set` (`sec >= KTIME_SEC_MAX` (`9_223_372_036`) returns
/// [`KTIME_MAX_NS`] regardless of `nsec`).
pub const fn itimerspec_to_ns(sec: i64, nsec: i64) -> Result<u64, Invalid> {
    if sec < 0 || nsec < 0 || nsec >= NSEC_PER_SEC as i64 {
        return Err(Invalid);
    }
    if sec >= KTIME_SEC_MAX {
        return Ok(KTIME_MAX_NS);
    }
    Ok((sec as u64) * NSEC_PER_SEC + (nsec as u64))
}

/// Error returned by [`TimerClock::from_clockid`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockError {
    /// `CLOCK_REALTIME_ALARM` (8) or `CLOCK_BOOTTIME_ALARM` (9): Linux
    /// `fs/timerfd.c` `timerfd_create` requires `CAP_WAKE_ALARM` and returns
    /// `-EPERM` without it.
    AlarmNotPermitted,
    /// Unrecognized or unsupported `clockid_t` (`-EINVAL`).
    Invalid,
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlarmNotPermitted => f.write_str("alarm clock requires CAP_WAKE_ALARM"),
            Self::Invalid => f.write_str("unsupported timerfd clock id"),
        }
    }
}

impl core::error::Error for ClockError {}

/// Supported clocks for `timerfd_create(2)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerClock {
    /// `CLOCK_REALTIME` (0).
    Realtime,
    /// `CLOCK_MONOTONIC` (1).
    Monotonic,
    /// `CLOCK_BOOTTIME` (7), treated as monotonic on Lumen (no suspend).
    Boottime,
}

impl TimerClock {
    /// Validate a Linux `clockid_t` for `timerfd_create(2)` (`fs/timerfd.c`
    /// `SYSCALL_DEFINE2(timerfd_create, ...)`).
    ///
    /// Linux checks flags (`-EINVAL`) and unknown clock ids (`-EINVAL`) before
    /// the alarm-clock capability check (`-EPERM`), so callers should validate
    /// [`TimerfdCreateFlags::from_bits`] first, then map
    /// [`ClockError::Invalid`] to `-EINVAL` and
    /// [`ClockError::AlarmNotPermitted`] to `-EPERM`.
    pub const fn from_clockid(clockid: i32) -> Result<Self, ClockError> {
        match clockid {
            CLOCK_REALTIME => Ok(Self::Realtime),
            CLOCK_MONOTONIC => Ok(Self::Monotonic),
            CLOCK_BOOTTIME => Ok(Self::Boottime),
            CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM => Err(ClockError::AlarmNotPermitted),
            _ => Err(ClockError::Invalid),
        }
    }

    /// Return the Linux `clockid_t` constant for this clock.
    pub const fn clockid(self) -> i32 {
        match self {
            Self::Realtime => CLOCK_REALTIME,
            Self::Monotonic => CLOCK_MONOTONIC,
            Self::Boottime => CLOCK_BOOTTIME,
        }
    }
}

/// Validated `timerfd_create(2)` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerfdCreateFlags {
    /// `TFD_NONBLOCK`: non-blocking descriptor.
    pub nonblock: bool,
    /// `TFD_CLOEXEC`: close-on-exec descriptor flag.
    pub cloexec: bool,
}

impl TimerfdCreateFlags {
    /// Validate and decode a raw Linux `timerfd_create(2)` flags bitmask.
    pub const fn from_bits(bits: u32) -> Result<Self, InvalidFlags> {
        if bits & !TFD_CREATE_KNOWN_FLAGS != 0 {
            return Err(InvalidFlags);
        }
        Ok(Self { nonblock: bits & TFD_NONBLOCK != 0, cloexec: bits & TFD_CLOEXEC != 0 })
    }

    /// Re-encode these flags as a Linux `timerfd_create(2)` bitmask.
    pub const fn bits(self) -> u32 {
        let mut out = 0u32;
        if self.nonblock {
            out |= TFD_NONBLOCK;
        }
        if self.cloexec {
            out |= TFD_CLOEXEC;
        }
        out
    }
}

/// Validated `timerfd_settime(2)` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerfdSettimeFlags {
    /// `TFD_TIMER_ABSTIME`: absolute deadline on the timer's clock.
    pub abstime: bool,
    /// `TFD_TIMER_CANCEL_ON_SET`: accepted and never cancels because Lumen
    /// cannot set `CLOCK_REALTIME` (#450).
    pub cancel_on_set: bool,
}

impl TimerfdSettimeFlags {
    /// Validate and decode a raw Linux `timerfd_settime(2)` flags bitmask.
    pub const fn from_bits(bits: u32) -> Result<Self, InvalidFlags> {
        if bits & !TFD_SETTIME_KNOWN_FLAGS != 0 {
            return Err(InvalidFlags);
        }
        Ok(Self { abstime: bits & TFD_TIMER_ABSTIME != 0, cancel_on_set: bits & TFD_TIMER_CANCEL_ON_SET != 0 })
    }

    /// Re-encode these flags as a Linux `timerfd_settime(2)` bitmask.
    pub const fn bits(self) -> u32 {
        let mut out = 0u32;
        if self.abstime {
            out |= TFD_TIMER_ABSTIME;
        }
        if self.cancel_on_set {
            out |= TFD_TIMER_CANCEL_ON_SET;
        }
        out
    }
}

/// Nanosecond timer setting (`it_value`, `it_interval`) for [`TimerCount::arm`],
/// [`TimerCount::arm_realtime`], [`TimerCount::settime`], and
/// [`TimerCount::get`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ITimer {
    /// Initial expiration (or remaining time until next expiration) in
    /// nanoseconds; `0` means disarmed.
    pub value: u64,
    /// Repeat interval in nanoseconds; `0` means one-shot.
    pub interval: u64,
}

impl ITimer {
    /// Construct an [`ITimer`] with `value` and `interval` in nanoseconds.
    pub const fn new(value: u64, interval: u64) -> Self {
        Self { value, interval }
    }
}

/// Worker-queue update returned by every [`TimerCount`] operation that can
/// change the scheduled wakeup.
///
/// Apply under the timer lock then the global worker queue lock: remove
/// `remove` (if `Some`), then insert `insert` (if `Some`). The `remove` key is
/// always the exact wakeup previously queued by `TimerCount`, never a moved
/// `deadline()`. A stale worker entry that fires before the current deadline is
/// harmless because [`TimerCount::on_expiry`] is a no-op before the deadline,
/// and callers should replace their entry after every mutating call so no timer
/// is stranded.
#[must_use = "apply the worker queue update (remove then insert) so no timer is stranded"]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Requeue {
    /// Previously queued monotonic deadline to remove from the timer worker, if any.
    pub remove: Option<u64>,
    /// New monotonic deadline to insert into the timer worker, if any.
    pub insert: Option<u64>,
}

impl Requeue {
    /// No worker-queue change is required.
    pub const NONE: Self = Self { remove: None, insert: None };

    /// Whether neither a removal nor an insertion is required.
    pub const fn is_none(self) -> bool {
        self.remove.is_none() && self.insert.is_none()
    }
}

/// Arming state and unread-expiry counter for a Linux `timerfd` (`fs/timerfd.c`).
///
/// See the [module documentation](self) for the monotonic timebase, `u64::MAX`
/// ("never") sentinel, and [`Requeue`] contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerCount {
    deadline: Option<u64>,
    interval: u64,
    ticks: u64,
    before_boot: u64,
    queued: Option<u64>,
}

impl TimerCount {
    /// Create a disarmed `TimerCount` with zero pending expirations.
    pub const fn new() -> Self {
        Self { deadline: None, interval: 0, ticks: 0, before_boot: 0, queued: None }
    }

    /// Return the monotonic deadline if the timer is due at `now` (armed, not
    /// [`NEVER`], and `deadline <= now`).
    const fn due(&self, now: u64) -> Option<u64> {
        match self.deadline {
            Some(d) if d != NEVER && d <= now => Some(d),
            _ => None,
        }
    }

    /// Next scheduled expiry timestamp on the monotonic clock, or `None` if
    /// disarmed.
    ///
    /// A deadline of `Some(u64::MAX)` means "never" (never fires, stays armed
    /// for [`Self::get`]).
    pub const fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    /// Next deadline the kernel timer worker should wait for, or `None` if the
    /// worker should not be scheduled.
    ///
    /// Returns `None` whenever unread expirations are already pending
    /// (`ticks > 0`, matching Linux `fs/timerfd.c` `timerfd_tmrproc` returning
    /// `HRTIMER_NORESTART`), when disarmed, or when the deadline is `u64::MAX`
    /// ("never").
    pub const fn next_wakeup(&self) -> Option<u64> {
        if self.ticks > 0 {
            return None;
        }
        match self.deadline {
            Some(d) if d != NEVER => Some(d),
            _ => None,
        }
    }

    /// Configured repeat interval in nanoseconds (`0` for one-shot).
    pub const fn interval(&self) -> u64 {
        self.interval
    }

    /// Number of unread expirations currently buffered (before accounting any
    /// further expirations due at a later `now`).
    pub const fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Whether a `read(now)` would succeed immediately without mutating `self`.
    ///
    /// This is a superset of Linux's `fs/timerfd.c` `timerfd_poll` (which only
    /// checks `ctx->ticks`): it also reports `true` once `deadline <= now` even
    /// before the worker has run [`Self::on_expiry`]. B6 uses `readable(now)`
    /// for `POLLIN`.
    pub const fn readable(&self, now: u64) -> bool {
        self.ticks > 0 || self.due(now).is_some()
    }

    fn sync_wakeup(&mut self) -> Requeue {
        let next = self.next_wakeup();
        if self.queued == next {
            return Requeue::NONE;
        }
        let remove = self.queued;
        self.queued = next;
        Requeue { remove, insert: next }
    }

    fn replace(&mut self, now: u64, deadline: Option<u64>, interval: u64, before_boot: u64) -> (ITimer, Requeue) {
        let old = self.get(now);
        self.ticks = 0;
        self.deadline = deadline;
        self.interval = interval;
        self.before_boot = before_boot;
        (old, self.sync_wakeup())
    }

    /// Arm or re-arm the timer on the monotonic clock (`fs/timerfd.c`
    /// `do_timerfd_settime` / `timerfd_setup`).
    ///
    /// Returns `(old, requeue)` where `old` is the previous [`ITimer`] snapshot
    /// as of `now`. Always resets unread expirations (`ticks`) and pre-boot
    /// phase history to `0`.
    ///
    /// - If `it.value == 0`, the timer is disarmed regardless of `absolute`
    ///   (matching Linux `fs/timerfd.c` `if (texp != 0)`), and `it.interval` is
    ///   stored as in Linux `timerfd_setup`.
    /// - If `it.value != 0` and `absolute` is `true`, the initial deadline is
    ///   `Some(it.value)`.
    /// - If `it.value != 0` and `absolute` is `false`, the initial deadline is
    ///   `Some(now.saturating_add(it.value))` (saturating to `u64::MAX` "never").
    #[must_use = "apply the returned Requeue to the timer worker"]
    pub fn arm(&mut self, now: u64, it: ITimer, absolute: bool) -> (ITimer, Requeue) {
        let deadline = if it.value == 0 {
            None
        } else if absolute {
            Some(it.value)
        } else {
            Some(now.saturating_add(it.value))
        };
        self.replace(now, deadline, it.interval, 0)
    }

    /// Arm or re-arm a `CLOCK_REALTIME` absolute (`TFD_TIMER_ABSTIME`) timer by
    /// converting `it.value` onto the worker's monotonic clock.
    ///
    /// Mirrors `lumen_signal::timer::Setting::realtime`'s `i128` conversion and
    /// pre-boot phase tracking with two `timerfd`-specific differences:
    /// `it.value == 0` keeps `it.interval` (matching `fs/timerfd.c`
    /// `timerfd_setup`), and a converted deadline clamped to `u64::MAX` never
    /// fires.
    ///
    /// Returns `(old, requeue)` as of `now_mono`. Relative `CLOCK_REALTIME`
    /// timers do not depend on wall-clock offset and should use [`Self::arm`]
    /// with `now_mono` and `absolute = false` (or [`Self::settime`]).
    #[must_use = "apply the returned Requeue to the timer worker"]
    pub fn arm_realtime(&mut self, now_mono: u64, now_rt: u64, it: ITimer) -> (ITimer, Requeue) {
        if it.value == 0 {
            return self.replace(now_mono, None, it.interval, 0);
        }
        let deadline = i128::from(now_mono) + i128::from(it.value) - i128::from(now_rt);
        let clamped = deadline.clamp(0, i128::from(NEVER)) as u64;
        let before_boot = (-deadline).max(0) as u64;
        self.replace(now_mono, Some(clamped), it.interval, before_boot)
    }

    /// Dispatch `timerfd_settime(2)` to [`Self::arm_realtime`] (for
    /// `CLOCK_REALTIME` with `TFD_TIMER_ABSTIME`) or [`Self::arm`] (for
    /// `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`, or relative `CLOCK_REALTIME`).
    #[must_use = "apply the returned Requeue to the timer worker"]
    pub fn settime(
        &mut self,
        now_mono: u64,
        now_rt: u64,
        clock: TimerClock,
        flags: TimerfdSettimeFlags,
        it: ITimer,
    ) -> (ITimer, Requeue) {
        if clock == TimerClock::Realtime && flags.abstime {
            self.arm_realtime(now_mono, now_rt, it)
        } else {
            self.arm(now_mono, it, flags.abstime)
        }
    }

    /// Reset all timer state for teardown (clearing deadline, interval,
    /// pre-boot offset, and unread expirations) and return the [`Requeue`] to
    /// remove any queued worker entry.
    ///
    /// For `timerfd_settime(2)` with `it_value == 0`, call [`Self::arm`] (or
    /// [`Self::settime`]) with `value = 0` instead so `interval` is preserved
    /// and the previous [`ITimer`] is returned, matching Linux `fs/timerfd.c`
    /// `timerfd_setup`.
    pub fn disarm(&mut self) -> Requeue {
        let remove = self.queued;
        *self = Self::new();
        Requeue { remove, insert: None }
    }

    /// Forward a due deadline (`deadline != NEVER && deadline <= now`) without
    /// mutating `self`, returning `(expiries_added, next_deadline)`.
    ///
    /// Uses the same constant-time phase-preserving rule as
    /// `lumen_signal::timer::Setting::forward` and Linux `hrtimer_forward`:
    /// - One-shot (`interval == 0`): `(1, None)`.
    /// - Periodic (`interval > 0`):
    ///   `elapsed = (now - deadline) + before_boot`,
    ///   `count = elapsed / interval + 1`,
    ///   `next = deadline + count * interval - before_boot`.
    ///   If `next >= u64::MAX`, the next deadline is unrepresentable (including
    ///   landing exactly on the [`NEVER`] sentinel) and disarms (`None`).
    fn forward_due(&self, deadline: u64, now: u64) -> (u64, Option<u64>) {
        if self.interval == 0 {
            return (1, None);
        }
        let elapsed = u128::from(now - deadline) + u128::from(self.before_boot);
        let interval = u128::from(self.interval);
        let count = elapsed / interval + 1;
        let next = u128::from(deadline) + count * interval - u128::from(self.before_boot);
        let added = count.min(u128::from(u64::MAX)) as u64;
        let next_deadline = (next < u128::from(NEVER)).then_some(next as u64);
        (added, next_deadline)
    }

    fn advance_due(&mut self, now: u64) -> u64 {
        let Some(deadline) = self.due(now) else {
            return 0;
        };
        let (added, next) = self.forward_due(deadline, now);
        self.before_boot = 0;
        self.deadline = next;
        self.ticks = self.ticks.saturating_add(added);
        added
    }

    /// Account all expirations due at or before `now` and return
    /// `(added, requeue)`.
    ///
    /// A call before the deadline (e.g. from a stale worker entry) is a no-op
    /// returning `(0, Requeue::NONE)`.
    #[must_use = "apply the returned Requeue to the timer worker"]
    pub fn on_expiry(&mut self, now: u64) -> (u64, Requeue) {
        let added = self.advance_due(now);
        (added, self.sync_wakeup())
    }

    /// Account any expirations due at or before `now`, then consume and return
    /// `(count, requeue)` (`fs/timerfd.c` `timerfd_read_iter` /
    /// `timerfd_restart` adding `hrtimer_forward_now - 1` at read time).
    ///
    /// Unlike Linux (where `timerfd_tmrproc` sets `ctx->expired` before
    /// `timerfd_restart` forwards), `read` also accounts due ticks even before
    /// the worker fires—a harmless deviation that avoids stale zero reads.
    ///
    /// Returns `Err(WouldBlock)` if zero expirations are pending after
    /// accounting (leaving the queued worker entry untouched).
    pub fn read(&mut self, now: u64) -> Result<(u64, Requeue), WouldBlock> {
        self.advance_due(now);
        if self.ticks == 0 {
            return Err(WouldBlock);
        }
        let count = self.ticks;
        self.ticks = 0;
        Ok((count, self.sync_wakeup()))
    }

    /// Consume pending expirations via [`Self::read`] and write the
    /// native-endian `u64` into the first 8 bytes of `buf`, returning
    /// `(IO_SIZE, requeue)`.
    ///
    /// Buffers shorter than [`IO_SIZE`] (8 bytes) return
    /// `Err(ReadBytesError::Invalid)` before checking or mutating timer state,
    /// matching Linux `fs/timerfd.c` `timerfd_read_iter`.
    pub fn read_bytes(&mut self, buf: &mut [u8], now: u64) -> Result<(usize, Requeue), ReadBytesError> {
        let Some(head) = buf.first_chunk_mut::<IO_SIZE>() else {
            return Err(ReadBytesError::Invalid);
        };
        let (count, requeue) = self.read(now)?;
        *head = count.to_ne_bytes();
        Ok((IO_SIZE, requeue))
    }

    /// Return the current [`ITimer`] (`value` = remaining, `interval`) as of
    /// `now` (`fs/timerfd.c` `do_timerfd_gettime` / `timerfd_get_remaining`).
    ///
    /// Follows `timerfd_get_remaining` (`remaining < 0 ? 0 : remaining`), not
    /// POSIX `timer_gettime` / `lumen_signal::timer::Setting::remaining`: an
    /// overdue one-shot timer returns `ITimer::new(0, 0)` rather than `1 ns`.
    /// Remaining time is clamped to `KTIME_MAX_NS.saturating_sub(now)`.
    pub fn get(&self, now: u64) -> ITimer {
        let Some(deadline) = self.deadline else {
            return ITimer::new(0, self.interval);
        };
        let raw = if let Some(d) = self.due(now) {
            let (_, next) = self.forward_due(d, now);
            next.map_or(0, |n| n - now)
        } else {
            deadline - now
        };
        ITimer::new(raw.min(KTIME_MAX_NS.saturating_sub(now)), self.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_timerfd_constants_match_uapi_literals() {
        // include/uapi/linux/timerfd.h & include/uapi/linux/time.h (x86-64)
        assert_eq!(TFD_TIMER_ABSTIME, 0x1);
        assert_eq!(TFD_TIMER_CANCEL_ON_SET, 0x2);
        assert_eq!(TFD_NONBLOCK, 0x800);
        assert_eq!(TFD_CLOEXEC, 0x8_0000);
        // M9 census observed timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC|TFD_NONBLOCK)
        // with clock=1, flags=0x80800 and timerfd_settime(flags=0x0).
        assert_eq!(TFD_CLOEXEC | TFD_NONBLOCK, 0x80800);
        assert_eq!(CLOCK_REALTIME, 0);
        assert_eq!(CLOCK_MONOTONIC, 1);
        assert_eq!(CLOCK_BOOTTIME, 7);
        assert_eq!(CLOCK_REALTIME_ALARM, 8);
        assert_eq!(CLOCK_BOOTTIME_ALARM, 9);
        assert_eq!(KTIME_MAX_NS, 9_223_372_036_854_775_807);
    }

    #[test]
    fn clocks_and_flags_validation_accept_known_and_reject_unknown() {
        // fs/timerfd.c timerfd_create: flags (-EINVAL) and unknown clock (-EINVAL)
        // before alarm-clock CAP_WAKE_ALARM check (-EPERM).
        assert_eq!(TimerClock::from_clockid(0), Ok(TimerClock::Realtime));
        assert_eq!(TimerClock::from_clockid(1), Ok(TimerClock::Monotonic));
        assert_eq!(TimerClock::from_clockid(7), Ok(TimerClock::Boottime));
        assert_eq!(TimerClock::Realtime.clockid(), 0);
        assert_eq!(TimerClock::Monotonic.clockid(), 1);
        assert_eq!(TimerClock::Boottime.clockid(), 7);
        assert_eq!(TimerClock::from_clockid(8), Err(ClockError::AlarmNotPermitted));
        assert_eq!(TimerClock::from_clockid(9), Err(ClockError::AlarmNotPermitted));
        for bad in [-1, 2, 3, 4, 5, 6, 10, 11, i32::MIN, i32::MAX] {
            assert_eq!(TimerClock::from_clockid(bad), Err(ClockError::Invalid));
        }

        let create = TimerfdCreateFlags::from_bits(0x80800).unwrap();
        assert!(create.nonblock && create.cloexec);
        assert_eq!(create.bits(), 0x80800);
        assert_eq!(TimerfdCreateFlags::from_bits(0).unwrap().bits(), 0);
        for bit in 0..32u32 {
            let mask = 1u32 << bit;
            if matches!(mask, 0x800 | 0x8_0000) {
                assert!(TimerfdCreateFlags::from_bits(mask).is_ok());
            } else {
                assert_eq!(TimerfdCreateFlags::from_bits(mask), Err(InvalidFlags));
            }
        }

        let set0 = TimerfdSettimeFlags::from_bits(0).unwrap();
        assert!(!set0.abstime && !set0.cancel_on_set);
        assert_eq!(set0.bits(), 0);
        let set_all = TimerfdSettimeFlags::from_bits(0x3).unwrap();
        assert!(set_all.abstime && set_all.cancel_on_set);
        assert_eq!(set_all.bits(), 0x3);
        for bit in 0..32u32 {
            let mask = 1u32 << bit;
            if matches!(mask, 0x1 | 0x2) {
                assert!(TimerfdSettimeFlags::from_bits(mask).is_ok());
            } else {
                assert_eq!(TimerfdSettimeFlags::from_bits(mask), Err(InvalidFlags));
            }
        }
    }

    #[test]
    fn itimerspec_to_ns_validates_and_clamps_as_ktime_set() {
        // include/linux/time64.h timespec64_valid / itimerspec64_valid and
        // include/linux/ktime.h ktime_set (KTIME_SEC_MAX = 9_223_372_036 -> KTIME_MAX).
        const KTIME_MAX: u64 = 9_223_372_036_854_775_807;

        assert_eq!(itimerspec_to_ns(0, 0), Ok(0));
        assert_eq!(itimerspec_to_ns(0, 1), Ok(1));
        assert_eq!(itimerspec_to_ns(0, 999_999_999), Ok(999_999_999));
        assert_eq!(itimerspec_to_ns(1, 500_000_000), Ok(1_500_000_000));
        assert_eq!(itimerspec_to_ns(9_223_372_035, 999_999_999), Ok(9_223_372_035_999_999_999));
        // Linux ktime_set: sec >= KTIME_SEC_MAX (9_223_372_036) clamps to KTIME_MAX whatever nsec is.
        assert_eq!(itimerspec_to_ns(9_223_372_036, 0), Ok(KTIME_MAX));
        assert_eq!(itimerspec_to_ns(9_223_372_036, 854_775_807), Ok(KTIME_MAX));
        assert_eq!(itimerspec_to_ns(9_223_372_036, 854_775_808), Ok(KTIME_MAX));
        assert_eq!(itimerspec_to_ns(9_223_372_037, 0), Ok(KTIME_MAX));
        assert_eq!(itimerspec_to_ns(i64::MAX, 999_999_999), Ok(KTIME_MAX));

        // Negative tv_sec, negative tv_nsec, or tv_nsec >= 1_000_000_000 -> Invalid (-EINVAL),
        // even when sec >= KTIME_SEC_MAX.
        for (sec, nsec) in [
            (-1, 0),
            (i64::MIN, 0),
            (0, -1),
            (0, i64::MIN),
            (-1, -1),
            (0, 1_000_000_000),
            (1, 1_000_000_000),
            (9_223_372_036, 1_000_000_000),
            (0, i64::MAX),
            (-1, 1_000_000_000),
        ] {
            assert_eq!(itimerspec_to_ns(sec, nsec), Err(Invalid), "sec={sec}, nsec={nsec}");
        }
    }

    #[test]
    fn oneshot_arm_expiry_read_and_disarm() {
        let mut t = TimerCount::new();
        assert!(!t.readable(100));
        assert_eq!(t.deadline(), None);
        assert_eq!(t.next_wakeup(), None);
        assert_eq!(t.get(100), ITimer::new(0, 0));
        assert_eq!(t.read(100), Err(WouldBlock));
        assert_eq!(t.on_expiry(100), (0, Requeue::NONE));

        // Relative arm: now=100, value=50 => deadline=150.
        assert_eq!(
            t.arm(100, ITimer::new(50, 0), false),
            (ITimer::new(0, 0), Requeue { remove: None, insert: Some(150) })
        );
        assert_eq!(t.deadline(), Some(150));
        assert_eq!(t.next_wakeup(), Some(150));
        assert!(!t.readable(149));
        assert!(t.readable(150));
        assert_eq!(t.get(120), ITimer::new(30, 0));
        assert_eq!(t.on_expiry(149), (0, Requeue::NONE));
        assert!(!t.readable(149));

        // Fires once at 150, disarms, and removes the queued wakeup.
        assert_eq!(t.on_expiry(150), (1, Requeue { remove: Some(150), insert: None }));
        assert!(t.readable(150));
        assert_eq!(t.deadline(), None);
        assert_eq!(t.next_wakeup(), None);
        assert_eq!(t.get(150), ITimer::new(0, 0));
        assert_eq!(t.on_expiry(500), (0, Requeue::NONE));
        assert_eq!(t.read(500), Ok((1, Requeue::NONE)));
        assert!(!t.readable(500));
        assert_eq!(t.read(500), Err(WouldBlock));
    }

    #[test]
    fn worker_fires_once_then_read_accounts_elapsed_intervals_and_rearms_wakeup() {
        // fs/timerfd.c: timerfd_tmrproc returns HRTIMER_NORESTART (next_wakeup == None
        // while ticks > 0); timerfd_read_iter / timerfd_restart forwards missed
        // intervals at read time (`ctx->ticks += hrtimer_forward_now - 1`) and
        // restarts the hrtimer.
        let mut t = TimerCount::new();
        assert_eq!(
            t.arm(100, ITimer::new(50, 10), false),
            (ITimer::new(0, 0), Requeue { remove: None, insert: Some(150) })
        );
        assert_eq!(t.next_wakeup(), Some(150));

        // Worker runs once at 150: 1 expiry buffered, removes 150 and inserts nothing.
        assert_eq!(t.on_expiry(150), (1, Requeue { remove: Some(150), insert: None }));
        assert_eq!(t.ticks(), 1);
        assert_eq!(t.deadline(), Some(160));
        assert_eq!(t.next_wakeup(), None);
        assert!(t.readable(150));
        assert!(t.readable(185));

        // Read at 185 (worker never ran again): accounts 160, 170, 180 on top of 150 => 4
        // and returns Requeue to insert 190.
        assert_eq!(t.read(185), Ok((4, Requeue { remove: None, insert: Some(190) })));
        assert_eq!(t.ticks(), 0);
        assert_eq!(t.deadline(), Some(190));
        assert_eq!(t.next_wakeup(), Some(190));
        assert!(!t.readable(185));
        assert!(!t.readable(189));
        assert!(t.readable(190));
        assert_eq!(t.get(185), ITimer::new(5, 10));

        // Direct read at 215 without worker running first removes the queued 190
        // (not the moved deadline 220!) and inserts 220.
        assert_eq!(t.read(215), Ok((3, Requeue { remove: Some(190), insert: Some(220) })));
        assert_eq!(t.deadline(), Some(220));
        assert_eq!(t.next_wakeup(), Some(220));
    }

    #[test]
    fn periodic_counts_missed_intervals_and_accumulates_until_read() {
        // fs/timerfd.c: periodic expiry count includes missed intervals (overruns)
        // and accumulates across multiple expiries until read or settime.
        let mut t = TimerCount::new();
        let (_, rq) = t.arm(100, ITimer::new(50, 10), false); // deadline=150, interval=10
        assert_eq!(rq, Requeue { remove: None, insert: Some(150) });
        assert_eq!(t.on_expiry(149), (0, Requeue::NONE));
        assert_eq!(t.on_expiry(185), (4, Requeue { remove: Some(150), insert: None })); // 150, 160, 170, 180
        assert_eq!(t.deadline(), Some(190));
        assert_eq!(t.next_wakeup(), None);
        assert_eq!(t.get(185), ITimer::new(5, 10));
        assert!(t.readable(185));

        // Another expiry before reading accumulates into ticks; still nothing queued.
        assert_eq!(t.on_expiry(205), (2, Requeue::NONE)); // 190, 200
        assert_eq!(t.deadline(), Some(210));
        assert_eq!(t.ticks(), 6);
        assert_eq!(t.read(205), Ok((6, Requeue { remove: None, insert: Some(210) })));
        assert!(!t.readable(205));
        assert_eq!(t.read(205), Err(WouldBlock));
        // Timer stays armed for the next period after read.
        assert_eq!(t.deadline(), Some(210));
        assert_eq!(t.next_wakeup(), Some(210));
        assert_eq!(t.get(205), ITimer::new(5, 10));
    }

    #[test]
    fn settime_and_disarm_reset_pending_count_and_return_old() {
        // fs/timerfd.c timerfd_setup: ctx->ticks = 0 on every timerfd_settime.
        let mut t = TimerCount::new();
        let _ = t.arm(100, ITimer::new(20, 10), false); // deadline=120
        assert_eq!(t.on_expiry(145), (3, Requeue { remove: Some(120), insert: None })); // 120, 130, 140; next=150
        assert!(t.readable(145));
        assert_eq!(t.next_wakeup(), None);

        // Re-arming returns old ITimer, clears unread ticks, and inserts new wakeup.
        let (old, rq) = t.arm(145, ITimer::new(100, 25), false);
        assert_eq!(old, ITimer::new(5, 10));
        assert_eq!(rq, Requeue { remove: None, insert: Some(245) });
        assert!(!t.readable(145));
        assert_eq!(t.read(145), Err(WouldBlock));
        assert_eq!(t.deadline(), Some(245));
        assert_eq!(t.next_wakeup(), Some(245));
        assert_eq!(t.get(145), ITimer::new(100, 25));

        // Passing value == 0 to arm disarms even with absolute=true, clears ticks,
        // removes queued wakeup, and preserves interval as Linux fs/timerfd.c timerfd_setup does.
        assert_eq!(t.on_expiry(270), (2, Requeue { remove: Some(245), insert: None })); // 245, 270; next=295
        assert!(t.readable(270));
        let (old2, rq2) = t.arm(275, ITimer::new(0, 77), true);
        assert_eq!(old2, ITimer::new(20, 25));
        assert_eq!(rq2, Requeue::NONE);
        assert!(!t.readable(275));
        assert_eq!(t.deadline(), None);
        assert_eq!(t.next_wakeup(), None);
        assert_eq!(t.get(275), ITimer::new(0, 77));
        assert_eq!(t.on_expiry(1000), (0, Requeue::NONE));

        // Teardown disarm clears deadline, interval, pre-boot history, and ticks,
        // and returns Requeue removing the queued wakeup.
        let (_, rq_rt) = t.arm_realtime(10, 100, ITimer::new(80, 3));
        assert_eq!(rq_rt, Requeue { remove: None, insert: Some(0) });
        assert_eq!(t.disarm(), Requeue { remove: Some(0), insert: None });
        assert_eq!(t, TimerCount::new());
        assert_eq!(t.get(1020), ITimer::new(0, 0));

        // settime dispatches Realtime+abstime to arm_realtime and all other clocks/modes to arm.
        let abstime = TimerfdSettimeFlags::from_bits(TFD_TIMER_ABSTIME).unwrap();
        let rel = TimerfdSettimeFlags::default();
        let (_, rq_s1) = t.settime(10, 100, TimerClock::Realtime, abstime, ITimer::new(80, 3));
        assert_eq!(rq_s1, Requeue { remove: None, insert: Some(0) });
        assert_eq!(t.deadline(), Some(0));
        let (old_s2, rq_s2) = t.settime(10, 100, TimerClock::Boottime, rel, ITimer::new(50, 0));
        assert_eq!(old_s2, ITimer::new(1, 3)); // -10 + 7*3 = 11 -> rem 1
        assert_eq!(rq_s2, Requeue { remove: Some(0), insert: Some(60) });
        assert_eq!(t.deadline(), Some(60));
    }

    #[test]
    fn absolute_vs_relative_arming_including_past_and_never_deadlines() {
        // Relative arming adds `value` to `now`.
        let mut rel = TimerCount::new();
        let _ = rel.arm(1_000, ITimer::new(250, 100), false);
        assert_eq!(rel.deadline(), Some(1_250));
        assert_eq!(rel.next_wakeup(), Some(1_250));
        assert_eq!(rel.get(1_000), ITimer::new(250, 100));
        assert_eq!(rel.on_expiry(1_000), (0, Requeue::NONE));

        // Absolute arming uses `value` verbatim as the deadline.
        let mut abs_future = TimerCount::new();
        let _ = abs_future.arm(1_000, ITimer::new(1_250, 100), true);
        assert_eq!(abs_future.deadline(), Some(1_250));
        assert_eq!(abs_future.get(1_000), ITimer::new(250, 100));

        // Absolute arming in the past: fires immediately on next on_expiry(now) or read(now),
        // counting all missed periods from `value` through `now`.
        let mut abs_past_periodic = TimerCount::new();
        let _ = abs_past_periodic.arm(135, ITimer::new(100, 10), true);
        assert_eq!(abs_past_periodic.deadline(), Some(100));
        assert_eq!(abs_past_periodic.next_wakeup(), Some(100));
        assert!(abs_past_periodic.readable(135));
        // get(now) projects the next phase without mutating ticks/deadline.
        assert_eq!(abs_past_periodic.get(135), ITimer::new(5, 10));
        assert_eq!(abs_past_periodic.deadline(), Some(100));
        assert_eq!(abs_past_periodic.on_expiry(135), (4, Requeue { remove: Some(100), insert: None })); // 100, 110, 120, 130
        assert_eq!(abs_past_periodic.deadline(), Some(140));
        assert_eq!(abs_past_periodic.get(135), ITimer::new(5, 10));
        assert_eq!(abs_past_periodic.read(135), Ok((4, Requeue { remove: None, insert: Some(140) })));

        // Absolute one-shot in the past fires once and disarms.
        let mut abs_past_oneshot = TimerCount::new();
        let _ = abs_past_oneshot.arm(135, ITimer::new(100, 0), true);
        assert_eq!(abs_past_oneshot.get(135), ITimer::new(0, 0));
        assert!(abs_past_oneshot.readable(135));
        assert_eq!(abs_past_oneshot.read(135), Ok((1, Requeue { remove: Some(100), insert: None })));
        assert_eq!(abs_past_oneshot.deadline(), None);

        // u64::MAX means "never": a deadline at u64::MAX (from relative saturation,
        // absolute u64::MAX, or realtime overflow) does not fire, returns no wakeup,
        // and stays armed for get() (clamped to KTIME_MAX_NS - now).
        let mut never_rel = TimerCount::new();
        let (_, rq) = never_rel.arm(100, ITimer::new(u64::MAX, 0), false);
        assert_eq!(rq, Requeue::NONE);
        assert_eq!(never_rel.deadline(), Some(u64::MAX));
        assert_eq!(never_rel.next_wakeup(), None);
        assert!(!never_rel.readable(u64::MAX));
        assert_eq!(never_rel.on_expiry(u64::MAX), (0, Requeue::NONE));
        assert_eq!(never_rel.read(u64::MAX), Err(WouldBlock));
        assert_eq!(never_rel.deadline(), Some(u64::MAX));
        assert_eq!(never_rel.get(100), ITimer::new(KTIME_MAX_NS - 100, 0));

        let mut never_abs = TimerCount::new();
        let _ = never_abs.arm(0, ITimer::new(u64::MAX, 10), true);
        assert_eq!(never_abs.deadline(), Some(u64::MAX));
        assert_eq!(never_abs.next_wakeup(), None);
        assert_eq!(never_abs.on_expiry(u64::MAX), (0, Requeue::NONE));
        assert_eq!(never_abs.deadline(), Some(u64::MAX));
        assert_eq!(never_abs.get(100), ITimer::new(KTIME_MAX_NS - 100, 10));

        let mut never_rt = TimerCount::new();
        let _ = never_rt.arm_realtime(500, 100, ITimer::new(u64::MAX, 10));
        assert_eq!(never_rt.deadline(), Some(u64::MAX));
        assert_eq!(never_rt.next_wakeup(), None);
        assert_eq!(never_rt.on_expiry(u64::MAX), (0, Requeue::NONE));
        assert_eq!(never_rt.deadline(), Some(u64::MAX));
        assert_eq!(never_rt.get(500), ITimer::new(KTIME_MAX_NS - 500, 10));
    }

    #[test]
    fn arm_realtime_preserves_preboot_phase_and_resets_on_rearm() {
        const S: u64 = 1_000_000_000;
        let mut t = TimerCount::new();
        // Converted deadline: 10s + 80s - 100.5s = -10.5s (pre-boot).
        assert_eq!(
            t.arm_realtime(10 * S, 100 * S + S / 2, ITimer::new(80 * S, 3 * S)),
            (ITimer::new(0, 0), Requeue { remove: None, insert: Some(0) })
        );
        assert_eq!(t.deadline(), Some(0));
        assert_eq!(t.next_wakeup(), Some(0));
        assert!(t.readable(10 * S));
        // Overdue get() projects next phase without mutating ticks/deadline.
        assert_eq!(t.get(10 * S), ITimer::new(S / 2, 3 * S));
        assert_eq!(t.deadline(), Some(0));

        // First forward at 10s accounts 7 expiries and clears before_boot.
        assert_eq!(t.read(10 * S), Ok((7, Requeue { remove: Some(0), insert: Some(10 * S + S / 2) })));
        assert_eq!(t.deadline(), Some(10 * S + S / 2));
        assert_eq!(t.next_wakeup(), Some(10 * S + S / 2));
        assert_eq!(t.get(10 * S), ITimer::new(S / 2, 3 * S));

        // Second forward at 10.5s must account exactly 1 expiry (not 4 from stale before_boot!),
        // advancing deadline to 13.5s, and overdue get() at 14s projects through forward_due cleanly.
        assert_eq!(
            t.read(10 * S + S / 2),
            Ok((1, Requeue { remove: Some(10 * S + S / 2), insert: Some(13 * S + S / 2) }))
        );
        assert_eq!(t.deadline(), Some(13 * S + S / 2));
        assert_eq!(t.get(11 * S), ITimer::new(2 * S + S / 2, 3 * S));
        assert_eq!(t.get(14 * S), ITimer::new(2 * S + S / 2, 3 * S));

        // Re-arming via arm() clears before_boot history completely.
        let _ = t.arm_realtime(10 * S, 100 * S + S / 2, ITimer::new(80 * S, 3 * S));
        let (old, rq) = t.arm(10 * S, ITimer::new(100, 10), true);
        assert_eq!(old, ITimer::new(S / 2, 3 * S));
        assert_eq!(rq, Requeue { remove: Some(0), insert: Some(100) });
        assert_eq!(t.on_expiry(135), (4, Requeue { remove: Some(100), insert: None }));
        assert_eq!(t.deadline(), Some(140));

        // arm_realtime with value == 0 disarms and preserves interval (fs/timerfd.c timerfd_setup).
        let (old2, rq2) = t.arm_realtime(135, 100 * S, ITimer::new(0, 77));
        assert_eq!(old2, ITimer::new(5, 10));
        assert_eq!(rq2, Requeue::NONE);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.next_wakeup(), None);
        assert_eq!(t.get(135), ITimer::new(0, 77));
        assert!(!t.readable(135));
    }

    #[test]
    fn get_remaining_time_before_at_and_after_expiry() {
        // Periodic timer: remaining decreases linearly and wraps modulo interval
        // at exact boundaries (fs/timerfd.c do_timerfd_gettime).
        let mut p = TimerCount::new();
        let _ = p.arm(100, ITimer::new(30, 10), false); // deadline=130, interval=10
        assert_eq!(p.get(100), ITimer::new(30, 10));
        assert_eq!(p.get(129), ITimer::new(1, 10));
        // Overdue before on_expiry: projected remaining at exact boundary is `interval`.
        assert_eq!(p.get(130), ITimer::new(10, 10));
        assert_eq!(p.get(131), ITimer::new(9, 10));
        assert_eq!(p.get(139), ITimer::new(1, 10));
        assert_eq!(p.get(140), ITimer::new(10, 10));
        // Calling get() did not consume or mutate deadline/ticks.
        assert_eq!(p.deadline(), Some(130));
        assert_eq!(p.ticks(), 0);
        // After on_expiry(135), get(135) gives the exact same answer (5, 10).
        let (n, _) = p.on_expiry(135);
        assert_eq!(n, 1);
        assert_eq!(p.get(135), ITimer::new(5, 10));

        // One-shot timer: follows timerfd_get_remaining (0 once overdue, not 1 ns).
        let mut o = TimerCount::new();
        let _ = o.arm(100, ITimer::new(30, 0), false);
        assert_eq!(o.get(100), ITimer::new(30, 0));
        assert_eq!(o.get(129), ITimer::new(1, 0));
        assert_eq!(o.get(130), ITimer::new(0, 0));
        assert_eq!(o.get(200), ITimer::new(0, 0));

        // Remaining is clamped to KTIME_MAX_NS - now even for huge relative deadlines.
        let mut far = TimerCount::new();
        let _ = far.arm(100, ITimer::new(KTIME_MAX_NS, 0), false);
        assert_eq!(far.get(100), ITimer::new(KTIME_MAX_NS - 100, 0));
    }

    #[test]
    fn clock_limit_disarms_computed_never_and_accumulates_to_u64_max() {
        // Maximum span: deadline=1, interval=1, now=u64::MAX => u64::MAX expiries,
        // next deadline = 2^64 (unrepresentable) => disarms.
        let mut t = TimerCount::new();
        let _ = t.arm(0, ITimer::new(1, 1), true);
        assert_eq!(t.on_expiry(u64::MAX), (u64::MAX, Requeue { remove: Some(1), insert: None }));
        assert_eq!(t.deadline(), None);
        assert_eq!(t.get(u64::MAX), ITimer::new(0, 1));
        assert_eq!(t.read(u64::MAX), Ok((u64::MAX, Requeue::NONE)));

        // Deadline=u64::MAX-3, interval=10: next deadline exceeds u64::MAX => disarms.
        let mut edge = TimerCount::new();
        let _ = edge.arm(u64::MAX - 10, ITimer::new(7, 10), false); // deadline = u64::MAX - 3
        assert_eq!(edge.on_expiry(u64::MAX - 3), (1, Requeue { remove: Some(u64::MAX - 3), insert: None }));
        assert_eq!(edge.deadline(), None);

        // Multi-step accumulation where computed next deadline lands exactly on u64::MAX:
        // treated as unrepresentable (disarms to None, not NEVER). Note that across
        // monotonic `now` without a wall clock near 2^64 ns in `arm_realtime`, `ticks`
        // cannot overflow (defensive `saturating_add` is kept in `advance_due`).
        let mut acc = TimerCount::new();
        let _ = acc.arm(0, ITimer::new(1, 1), true);
        assert_eq!(acc.on_expiry(10), (10, Requeue { remove: Some(1), insert: None }));
        assert_eq!(acc.deadline(), Some(11));
        assert_eq!(acc.on_expiry(u64::MAX - 1), (u64::MAX - 11, Requeue::NONE));
        // Computed next = u64::MAX is unrepresentable -> disarms to None.
        assert_eq!(acc.deadline(), None);
        assert_eq!(acc.on_expiry(u64::MAX), (0, Requeue::NONE));
        assert_eq!(acc.read(u64::MAX), Ok((u64::MAX - 1, Requeue::NONE)));
        assert_eq!(acc.next_wakeup(), None);
    }

    #[test]
    fn read_bytes_enforces_8_byte_minimum_before_wouldblock_or_forwarding() {
        // fs/timerfd.c timerfd_read_iter: iov_iter_count(to) < sizeof(ticks) -> -EINVAL
        // before locking, forwarding, or consuming ticks.
        let mut t = TimerCount::new();
        let _ = t.arm(0, ITimer::new(10, 10), true);
        for len in 0..8usize {
            let mut buf = [0xAAu8; 8];
            assert_eq!(t.read_bytes(&mut buf[..len], 35), Err(ReadBytesError::Invalid));
        }
        // Short-buffer error must not have forwarded deadline or consumed ticks.
        assert_eq!(t.deadline(), Some(10));
        assert_eq!(t.ticks(), 0);
        assert_eq!(t.next_wakeup(), Some(10));

        let mut buf = [0x77u8; 12];
        assert_eq!(t.read_bytes(&mut buf, 5), Err(ReadBytesError::WouldBlock));

        // Valid >= 8 byte read at 35 accounts 10, 20, 30 => 3, updates worker queue,
        // and leaves trailing bytes intact.
        assert_eq!(t.read_bytes(&mut buf, 35), Ok((8, Requeue { remove: Some(10), insert: Some(40) })));
        assert_eq!(u64::from_ne_bytes(buf[..8].try_into().unwrap()), 3);
        assert_eq!(&buf[8..], &[0x77; 4]);
        assert_eq!(t.deadline(), Some(40));
        assert_eq!(t.next_wakeup(), Some(40));
        assert_eq!(t.read_bytes(&mut buf, 35), Err(ReadBytesError::WouldBlock));
    }
}
