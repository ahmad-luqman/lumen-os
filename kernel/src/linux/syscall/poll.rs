//! Readiness syscalls (`poll`, `ppoll`, `select`, `pselect6`; M8's B3, #374,
//! ARCHITECTURE sections 8 and 10).
//!
//! All four syscalls share [`wait_ready`]: pass 1 subscribes a [`PollWait`]
//! waker to every [`lumen_vfs::Pollable`] source while reading its mask; if
//! nothing is ready and the timeout is non-zero, the thread waits with
//! `WaitQueue::wait_interruptible`, clears `fired` before re-polling every
//! source without re-subscribing, and keeps every [`PollGuard`] until the call
//! returns on any path (ready, timed out, or interrupted).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

use lumen_signal::restart::RestartCode;
use lumen_signal::{Blocked, SigSet};
use lumen_vfs::{MAX_FDS, PollGuard, PollMask, PollWaker};

use super::at::UserPtr;
use super::fd::current_io;
use super::{DEFAULT_RLIMIT_NOFILE, copy_pod_from_user, copy_pod_to_user, finish};
use crate::linux::process::{self, ConsoleIo, FileRef};
use crate::linux::signal::{RestartBlock, interrupted_with};
use crate::linux::{errno, general};
use crate::sched;
use crate::user::{CopyError, copy_from_user, copy_to_user};

/// `-ERESTARTNOHAND` as a positive errno for [`finish`]: restarted unless a
/// signal handler is delivered (Linux's `select`, `pselect6`, `ppoll`).
const ERESTARTNOHAND: u32 = RestartCode::NoHand as u32;

/// `timespec` / `timeval` nanoseconds per second.
const NS_PER_SEC: u64 = 1_000_000_000;
/// Microseconds per second.
const US_PER_SEC: u64 = 1_000_000;
/// Nanoseconds per microsecond.
const NS_PER_US: u64 = 1_000;
/// Nanoseconds per millisecond.
const NS_PER_MS: u64 = 1_000_000;

/// Serial console safety-net recheck interval when IRQ 4 is active (100 ms).
const SERIAL_SAFETY_STEP_NS: u64 = 100 * NS_PER_MS;
/// Short recheck step when polling a VT or non-IRQ serial console (5 ms).
const CONSOLE_POLL_STEP_NS: u64 = 5 * NS_PER_MS;

/// Raw Linux x86-64 `struct pollfd` layout (8 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// Raw Linux x86-64 `struct timespec` layout (16 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Raw Linux x86-64 `struct timeval` layout (16 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// Raw Linux x86-64 `pselect6` 6th-argument struct `{ const sigset_t *ss; size_t ss_len }`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawPselectSig {
    ss: u64,
    ss_len: u64,
}

const _: () = {
    assert!(size_of::<RawTimespec>() == size_of::<general::timespec>() && size_of::<RawTimespec>() == 16);
    assert!(offset_of!(RawTimespec, tv_sec) == offset_of!(general::timespec, tv_sec));
    assert!(offset_of!(RawTimespec, tv_nsec) == offset_of!(general::timespec, tv_nsec));
    assert!(size_of::<RawTimeval>() == size_of::<general::timeval>() && size_of::<RawTimeval>() == 16);
    assert!(offset_of!(RawTimeval, tv_sec) == offset_of!(general::timeval, tv_sec));
    assert!(offset_of!(RawTimeval, tv_usec) == offset_of!(general::timeval, tv_usec));
    assert!(size_of::<RawPselectSig>() == 16);
};

/// How long a readiness syscall may wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitTimeout {
    /// Non-blocking single pass (do not write back remaining time).
    Zero,
    /// Block until monotonic `time::now_ns() >= dl`.
    Deadline(u64),
    /// Block indefinitely until ready or interrupted.
    Forever,
}

impl WaitTimeout {
    fn deadline(self) -> Option<u64> {
        match self {
            Self::Deadline(dl) => Some(dl),
            Self::Zero | Self::Forever => None,
        }
    }
}

/// Outcome of [`wait_ready`].
enum WaitResult {
    Ready(u64),
    TimedOut,
    Interrupted,
}

/// Per-call [`PollWaker`]: sets `fired` and wakes its calling thread's
/// `WaitQueue`. Never allocates or blocks in [`PollWaker::wake`].
struct PollWait {
    fired: AtomicBool,
    queue: sched::WaitQueue,
}

impl PollWait {
    fn new() -> Arc<Self> {
        Arc::new(Self { fired: AtomicBool::new(false), queue: sched::WaitQueue::new() })
    }
}

impl PollWaker for PollWait {
    fn wake(&self, _ready: PollMask) {
        self.fired.store(true, Ordering::SeqCst);
        let _ = self.queue.wake_one();
    }
}

/// Temporarily replaces the calling thread's signal mask for `ppoll` /
/// `pselect6`, restoring it on drop on every exit path.
///
/// Until C5/C6 add signal delivery and `saved_mask` (`restore_saved_sigmask_unless`),
/// no user handler runs on syscall exit, so restoring here is exact.
struct MaskGuard {
    prev: Option<Blocked>,
}

impl MaskGuard {
    fn install(mask: Option<SigSet>) -> Self {
        let prev = mask.map(|set| {
            let old = process::current_blocked();
            process::set_current_blocked(Blocked::new(set));
            old
        });
        Self { prev }
    }
}

impl Drop for MaskGuard {
    fn drop(&mut self) {
        if let Some(old) = self.prev.take() {
            process::set_current_blocked(old);
        }
    }
}

/// Shared readiness wait loop for `poll`, `ppoll`, `select` and `pselect6`
/// (ARCHITECTURE section 10).
///
/// Pass 1 passes `Some(&waker)` so every [`lumen_vfs::Pollable`] subscribes in
/// the same hold as it reads its mask. Subsequent passes pass `None` while the
/// returned [`PollGuard`]s remain held; all guards drop on every exit path.
fn wait_ready(
    timeout: WaitTimeout,
    poll_step_ns: Option<u64>,
    mut pass: impl FnMut(Option<&Arc<dyn PollWaker>>, &mut Vec<PollGuard>) -> u64,
) -> WaitResult {
    let mut guards = Vec::new();
    let deadline = match timeout {
        WaitTimeout::Zero => return WaitResult::Ready(pass(None, &mut guards)),
        WaitTimeout::Deadline(dl) if crate::time::now_ns() >= dl => {
            let ready = pass(None, &mut guards);
            return if ready > 0 { WaitResult::Ready(ready) } else { WaitResult::TimedOut };
        }
        WaitTimeout::Deadline(dl) => Some(dl),
        WaitTimeout::Forever => None,
    };

    let waiter = PollWait::new();
    let waker: Arc<dyn PollWaker> = Arc::clone(&waiter) as Arc<dyn PollWaker>;

    // Pass 1: subscribe to every Pollable while reading its readiness.
    let ready = pass(Some(&waker), &mut guards);
    if ready > 0 {
        return WaitResult::Ready(ready);
    }

    loop {
        if let Some(dl) = deadline
            && crate::time::now_ns() >= dl
        {
            return WaitResult::TimedOut;
        }
        let step_dl = match (deadline, poll_step_ns) {
            (Some(dl), Some(step)) => Some(dl.min(crate::time::now_ns().saturating_add(step))),
            (None, Some(step)) => Some(crate::time::now_ns().saturating_add(step)),
            (dl, None) => dl,
        };
        match waiter.queue.wait_interruptible(step_dl, || waiter.fired.load(Ordering::SeqCst)) {
            sched::Wake::Woken => {
                // Clear `fired` BEFORE re-polling so any readiness change
                // landing during or after the re-poll sets `fired` again and
                // wakes the next `wait_interruptible` immediately.
                waiter.fired.store(false, Ordering::SeqCst);
                #[cfg(feature = "ktest")]
                recheck_window::pause_if_armed();
            }
            sched::Wake::TimedOut => {
                if let Some(dl) = deadline
                    && crate::time::now_ns() >= dl
                {
                    let ready = pass(None, &mut guards);
                    return if ready > 0 { WaitResult::Ready(ready) } else { WaitResult::TimedOut };
                }
            }
            sched::Wake::Interrupted => {
                return WaitResult::Interrupted;
            }
        }
        let ready = pass(None, &mut guards);
        if ready > 0 {
            return WaitResult::Ready(ready);
        }
    }
}

/// Read the current readiness of an open, non-`O_PATH` descriptor, subscribing
/// `waker` when provided and pushing any returned [`PollGuard`] onto `guards`.
fn poll_file(file: &FileRef, waker: Option<&Arc<dyn PollWaker>>, guards: &mut Vec<PollGuard>) -> PollMask {
    match file.inode().pollable() {
        Some(pollable) => {
            let (mask, guard) = pollable.poll(file.access_mode(), waker);
            if let Some(g) = guard {
                guards.push(g);
            }
            mask
        }
        None => PollMask::ALWAYS_READY,
    }
}

/// Optional step bound for consoles that lack an interrupt-driven wake (VT,
/// polled serial) or keep a safety-net recheck (IRQ 4 serial).
fn console_step_ns(has_readable_tty: bool) -> Option<u64> {
    if !has_readable_tty {
        return None;
    }
    match current_io() {
        ConsoleIo::Capture => None,
        ConsoleIo::Serial => {
            Some(if crate::serial_in::by_interrupt() { SERIAL_SAFETY_STEP_NS } else { CONSOLE_POLL_STEP_NS })
        }
        ConsoleIo::Vt(_) => Some(CONSOLE_POLL_STEP_NS),
    }
}

fn copy_timespec(ptr: UserPtr) -> Result<WaitTimeout, u32> {
    if ptr.is_null() {
        return Ok(WaitTimeout::Forever);
    }
    // SAFETY: `RawTimespec` is two `i64`s (16 bytes, zero padding); every bit pattern is valid.
    let ts = unsafe { copy_pod_from_user::<RawTimespec>(ptr.addr()) }.map_err(CopyError::to_errno)?;
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= NS_PER_SEC as i64 {
        return Err(errno::EINVAL);
    }
    if ts.tv_sec == 0 && ts.tv_nsec == 0 {
        return Ok(WaitTimeout::Zero);
    }
    let dur = (ts.tv_sec as u64).saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec as u64);
    Ok(WaitTimeout::Deadline(crate::time::now_ns().saturating_add(dur)))
}

fn copy_timeval(ptr: UserPtr) -> Result<WaitTimeout, u32> {
    if ptr.is_null() {
        return Ok(WaitTimeout::Forever);
    }
    // SAFETY: `RawTimeval` is two `i64`s (16 bytes, zero padding); every bit pattern is valid.
    let tv = unsafe { copy_pod_from_user::<RawTimeval>(ptr.addr()) }.map_err(CopyError::to_errno)?;
    if tv.tv_sec < 0 || tv.tv_usec < 0 {
        return Err(errno::EINVAL);
    }
    if tv.tv_sec == 0 && tv.tv_usec == 0 {
        return Ok(WaitTimeout::Zero);
    }
    let dur =
        (tv.tv_sec as u64).saturating_mul(NS_PER_SEC).saturating_add((tv.tv_usec as u64).saturating_mul(NS_PER_US));
    Ok(WaitTimeout::Deadline(crate::time::now_ns().saturating_add(dur)))
}

fn write_remaining_timespec(ptr: UserPtr, timeout: WaitTimeout, timed_out: bool) -> Result<(), u32> {
    let WaitTimeout::Deadline(dl) = timeout else {
        return Ok(());
    };
    if ptr.is_null() {
        return Ok(());
    }
    let rem_ns = if timed_out { 0 } else { dl.saturating_sub(crate::time::now_ns()) };
    let ts = RawTimespec { tv_sec: (rem_ns / NS_PER_SEC) as i64, tv_nsec: (rem_ns % NS_PER_SEC) as i64 };
    // SAFETY: `RawTimespec` has zero padding bytes.
    unsafe { copy_pod_to_user(ptr.addr(), &ts) }.map_err(CopyError::to_errno)
}

fn write_remaining_timeval(ptr: UserPtr, timeout: WaitTimeout, timed_out: bool) -> Result<(), u32> {
    let WaitTimeout::Deadline(dl) = timeout else {
        return Ok(());
    };
    if ptr.is_null() {
        return Ok(());
    }
    let rem_ns = if timed_out { 0 } else { dl.saturating_sub(crate::time::now_ns()) };
    let rem_usec = rem_ns / NS_PER_US;
    let tv = RawTimeval { tv_sec: (rem_usec / US_PER_SEC) as i64, tv_usec: (rem_usec % US_PER_SEC) as i64 };
    // SAFETY: `RawTimeval` has zero padding bytes.
    unsafe { copy_pod_to_user(ptr.addr(), &tv) }.map_err(CopyError::to_errno)
}

fn copy_sigmask(sigmask: UserPtr, sigsetsize: u64) -> Result<Option<SigSet>, u32> {
    if sigmask.is_null() {
        return Ok(None);
    }
    if sigsetsize != 8 {
        return Err(errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    copy_from_user(&mut raw, sigmask.addr()).map_err(CopyError::to_errno)?;
    Ok(Some(SigSet(u64::from_le_bytes(raw))))
}

/// Core `poll`/`ppoll` body, shared with [`resume_poll`].
///
/// Returns `Ok((ready, timed_out))` on completion, or `Err(errno)` (including
/// `ERESTART_RESTARTBLOCK` when `restartable` is true and `ERESTARTNOHAND` when
/// false).
fn run_poll(fds_ptr: u64, nfds: u64, timeout: WaitTimeout, restartable: bool) -> Result<(u64, bool), u32> {
    if nfds > DEFAULT_RLIMIT_NOFILE {
        return Err(errno::EINVAL);
    }
    let cnt = nfds as usize;
    if cnt == 0 {
        return match wait_ready(timeout, None, |_, _| 0) {
            WaitResult::Ready(r) => Ok((r, false)),
            WaitResult::TimedOut => Ok((0, true)),
            WaitResult::Interrupted => Err(poll_interrupted(fds_ptr, 0, timeout, restartable)),
        };
    }

    let byte_len = cnt * size_of::<RawPollFd>();
    let mut raw = vec![0u8; byte_len];
    copy_from_user(&mut raw, fds_ptr).map_err(CopyError::to_errno)?;

    let fd_at = |chunk: &[u8; 8]| i32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes"));
    let events_at = |chunk: &[u8; 8]| i16::from_le_bytes(chunk[4..6].try_into().expect("2 bytes"));

    let polled_fds: Vec<i32> = raw.as_chunks::<8>().0.iter().map(fd_at).collect();
    let files = process::current_files(&polled_fds);
    let has_readable_tty = raw.as_chunks::<8>().0.iter().zip(&files).any(|(chunk, file)| {
        fd_at(chunk) >= 0 && file.as_ref().is_some_and(|f| !f.is_path() && f.is_readable() && f.inode().is_tty())
    });
    let step_ns = console_step_ns(has_readable_tty);

    let outcome = wait_ready(timeout, step_ns, |waker, guards| {
        let mut ready = 0u64;
        for (chunk, file) in raw.as_chunks_mut::<8>().0.iter_mut().zip(&files) {
            let fd = fd_at(chunk);
            let events = PollMask::from_bits(events_at(chunk) as u16);
            let revents: u16 = if fd < 0 {
                0
            } else {
                match file {
                    None => PollMask::NVAL.bits(),
                    Some(f) if f.is_path() => PollMask::NVAL.bits(),
                    Some(f) => {
                        let mask = poll_file(f, waker, guards);
                        mask.filter_events(events).bits()
                    }
                }
            };
            if revents != 0 {
                ready += 1;
            }
            chunk[6..8].copy_from_slice(&revents.to_le_bytes());
        }
        ready
    });
    drop(files);

    match outcome {
        WaitResult::Ready(ready) => {
            copy_to_user(fds_ptr, &raw).map_err(CopyError::to_errno)?;
            Ok((ready, false))
        }
        WaitResult::TimedOut => {
            copy_to_user(fds_ptr, &raw).map_err(CopyError::to_errno)?;
            Ok((0, true))
        }
        WaitResult::Interrupted => Err(poll_interrupted(fds_ptr, nfds, timeout, restartable)),
    }
}

fn poll_interrupted(fds_ptr: u64, nfds: u64, timeout: WaitTimeout, restartable: bool) -> u32 {
    if restartable {
        let block = RestartBlock { resume: resume_poll, deadline: timeout.deadline(), args: [fds_ptr, nfds, 0, 0] };
        interrupted_with(block)
    } else {
        ERESTARTNOHAND
    }
}

/// Resumes an interrupted `poll(2)` via `restart_syscall(2)` toward its
/// original deadline.
fn resume_poll(block: &RestartBlock) -> u64 {
    let fds_ptr = block.args[0];
    let nfds = block.args[1];
    let timeout = match block.deadline {
        Some(dl) => WaitTimeout::Deadline(dl),
        None => WaitTimeout::Forever,
    };
    finish(run_poll(fds_ptr, nfds, timeout, true).map(|(ready, _)| ready))
}

/// `poll(fds, nfds, timeout_ms)` (7).
pub(super) fn sys_poll(fds_ptr: u64, nfds: u64, timeout_raw: u64) -> u64 {
    let timeout_ms = timeout_raw as i32;
    let timeout = if timeout_ms < 0 {
        WaitTimeout::Forever
    } else if timeout_ms == 0 {
        WaitTimeout::Zero
    } else {
        let dur = (timeout_ms as u64).saturating_mul(NS_PER_MS);
        WaitTimeout::Deadline(crate::time::now_ns().saturating_add(dur))
    };
    finish(run_poll(fds_ptr, nfds, timeout, true).map(|(ready, _)| ready))
}

/// `ppoll(fds, nfds, tsp, sigmask, sigsetsize)` (271).
pub(super) fn sys_ppoll(fds: UserPtr, nfds: u32, tsp: UserPtr, sigmask: UserPtr, sigsetsize: u64) -> u64 {
    finish((|| {
        let timeout = copy_timespec(tsp)?;
        let mask = copy_sigmask(sigmask, sigsetsize)?;
        let _mask_guard = MaskGuard::install(mask);
        match run_poll(fds.addr(), u64::from(nfds), timeout, false) {
            Ok((ready, timed_out)) => {
                write_remaining_timespec(tsp, timeout, timed_out)?;
                Ok(ready)
            }
            Err(e) if e == ERESTARTNOHAND => {
                write_remaining_timespec(tsp, timeout, false)?;
                Err(ERESTARTNOHAND)
            }
            Err(e) => Err(e),
        }
    })())
}

/// Number of `u64` words in a Linux x86-64 `fd_set` covering `n` descriptors
/// (`FDS_LONGS(n)` in `fs/select.c`).
const fn fds_words(n: usize) -> usize {
    n.div_ceil(64)
}

fn copy_fd_set_in(ptr: UserPtr, n: usize) -> Result<Option<Vec<u64>>, u32> {
    if ptr.is_null() {
        return Ok(None);
    }
    let words = fds_words(n);
    if words == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut bytes = vec![0u8; words * 8];
    copy_from_user(&mut bytes, ptr.addr()).map_err(CopyError::to_errno)?;
    let mut out: Vec<u64> = bytes.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
    // Ignore bits at or above `n`.
    let rem = n % 64;
    if rem != 0
        && let Some(last) = out.last_mut()
    {
        *last &= (1u64 << rem) - 1;
    }
    Ok(Some(out))
}

fn copy_fd_set_out(ptr: UserPtr, words: &[u64]) -> Result<(), u32> {
    if ptr.is_null() || words.is_empty() {
        return Ok(());
    }
    let mut bytes = Vec::with_capacity(words.len() * 8);
    for &w in words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    copy_to_user(ptr.addr(), &bytes).map_err(CopyError::to_errno)
}

#[inline]
fn fd_bit_set(set: &Option<Vec<u64>>, fd: usize) -> bool {
    set.as_ref().is_some_and(|w| (w[fd / 64] & (1u64 << (fd % 64))) != 0)
}

#[inline]
fn set_fd_bit(words: &mut [u64], fd: usize) {
    words[fd / 64] |= 1u64 << (fd % 64);
}

/// Core `select`/`pselect6` body.
///
/// Returns `Ok((ready_bits, timed_out))`, or `Err(errno)` (`ERESTARTNOHAND` on
/// signal interruption).
fn run_select(
    n: i32,
    readfds: UserPtr,
    writefds: UserPtr,
    exceptfds: UserPtr,
    timeout: WaitTimeout,
) -> Result<(u64, bool), u32> {
    if n < 0 {
        return Err(errno::EINVAL);
    }
    let n_capped = (n as usize).min(MAX_FDS);
    let words = fds_words(n_capped);

    let read_in = copy_fd_set_in(readfds, n_capped)?;
    let write_in = copy_fd_set_in(writefds, n_capped)?;
    let except_in = copy_fd_set_in(exceptfds, n_capped)?;

    // Collect every descriptor named in any of the three sets.
    struct SelectEntry {
        fd: usize,
        want_r: bool,
        want_w: bool,
        want_x: bool,
        file: FileRef,
    }

    let mut wanted_fds: Vec<i32> = Vec::new();
    let mut flags: Vec<(bool, bool, bool)> = Vec::new();
    for fd in 0..n_capped {
        let want_r = fd_bit_set(&read_in, fd);
        let want_w = fd_bit_set(&write_in, fd);
        let want_x = fd_bit_set(&except_in, fd);
        if want_r || want_w || want_x {
            wanted_fds.push(fd as i32);
            flags.push((want_r, want_w, want_x));
        }
    }

    let raw_files = if wanted_fds.is_empty() { Vec::new() } else { process::current_files(&wanted_fds) };
    let mut entries = Vec::with_capacity(wanted_fds.len());
    let mut has_readable_tty = false;
    for ((fd, (want_r, want_w, want_x)), file) in wanted_fds.into_iter().zip(flags).zip(raw_files) {
        let Some(file) = file else {
            return Err(errno::EBADF);
        };
        if file.is_path() {
            return Err(errno::EBADF);
        }
        if want_r && file.is_readable() && file.inode().is_tty() {
            has_readable_tty = true;
        }
        entries.push(SelectEntry { fd: fd as usize, want_r, want_w, want_x, file });
    }

    let step_ns = console_step_ns(has_readable_tty);
    let mut read_out = vec![0u64; words];
    let mut write_out = vec![0u64; words];
    let mut except_out = vec![0u64; words];

    let outcome = wait_ready(timeout, step_ns, |waker, guards| {
        read_out.fill(0);
        write_out.fill(0);
        except_out.fill(0);
        let mut ready = 0u64;
        for entry in &entries {
            let mask = poll_file(&entry.file, waker, guards);
            if entry.want_r && mask.intersects(PollMask::POLLIN_SET) {
                set_fd_bit(&mut read_out, entry.fd);
                ready += 1;
            }
            if entry.want_w && mask.intersects(PollMask::POLLOUT_SET) {
                set_fd_bit(&mut write_out, entry.fd);
                ready += 1;
            }
            if entry.want_x && mask.intersects(PollMask::POLLEX_SET) {
                set_fd_bit(&mut except_out, entry.fd);
                ready += 1;
            }
        }
        ready
    });
    drop(entries);

    match outcome {
        WaitResult::Ready(ready) => {
            copy_fd_set_out(readfds, &read_out)?;
            copy_fd_set_out(writefds, &write_out)?;
            copy_fd_set_out(exceptfds, &except_out)?;
            Ok((ready, false))
        }
        WaitResult::TimedOut => {
            read_out.fill(0);
            write_out.fill(0);
            except_out.fill(0);
            copy_fd_set_out(readfds, &read_out)?;
            copy_fd_set_out(writefds, &write_out)?;
            copy_fd_set_out(exceptfds, &except_out)?;
            Ok((0, true))
        }
        WaitResult::Interrupted => Err(ERESTARTNOHAND),
    }
}

/// `select(n, readfds, writefds, exceptfds, tv)` (23).
pub(super) fn sys_select(n: i32, readfds: UserPtr, writefds: UserPtr, exceptfds: UserPtr, tv: UserPtr) -> u64 {
    finish((|| {
        let timeout = copy_timeval(tv)?;
        match run_select(n, readfds, writefds, exceptfds, timeout) {
            Ok((ready, timed_out)) => {
                write_remaining_timeval(tv, timeout, timed_out)?;
                Ok(ready)
            }
            Err(e) if e == ERESTARTNOHAND => {
                write_remaining_timeval(tv, timeout, false)?;
                Err(ERESTARTNOHAND)
            }
            Err(e) => Err(e),
        }
    })())
}

/// `pselect6(n, readfds, writefds, exceptfds, tsp, sig)` (270).
pub(super) fn sys_pselect6(
    n: i32,
    readfds: UserPtr,
    writefds: UserPtr,
    exceptfds: UserPtr,
    tsp: UserPtr,
    sig: UserPtr,
) -> u64 {
    finish((|| {
        let timeout = copy_timespec(tsp)?;
        let mask = if sig.is_null() {
            None
        } else {
            // SAFETY: `RawPselectSig` is two `u64`s (16 bytes, zero padding); every bit pattern is valid.
            let arg = unsafe { copy_pod_from_user::<RawPselectSig>(sig.addr()) }.map_err(CopyError::to_errno)?;
            copy_sigmask(UserPtr::from(arg.ss), arg.ss_len)?
        };
        let _mask_guard = MaskGuard::install(mask);
        match run_select(n, readfds, writefds, exceptfds, timeout) {
            Ok((ready, timed_out)) => {
                write_remaining_timespec(tsp, timeout, timed_out)?;
                Ok(ready)
            }
            Err(e) if e == ERESTARTNOHAND => {
                write_remaining_timespec(tsp, timeout, false)?;
                Err(ERESTARTNOHAND)
            }
            Err(e) => Err(e),
        }
    })())
}

impl From<u64> for UserPtr {
    fn from(addr: u64) -> Self {
        use super::at::FromReg;
        Self::from_reg(addr)
    }
}

/// Ktest-only delay knob between clearing `fired` and the re-poll in
/// [`wait_ready`]: used by `poll_rechecks_after_a_wake`.
#[cfg(feature = "ktest")]
mod recheck_window {
    use core::sync::atomic::{AtomicU32, Ordering};

    const IDLE: u32 = 0;
    const ARMED: u32 = 1;
    const PAUSED: u32 = 2;
    const RELEASED: u32 = 3;

    static STATE: AtomicU32 = AtomicU32::new(IDLE);

    pub(super) fn arm() {
        STATE.store(ARMED, Ordering::SeqCst);
    }

    pub(super) fn pause_if_armed() {
        if STATE.compare_exchange(ARMED, PAUSED, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            while STATE.load(Ordering::SeqCst) != RELEASED {
                crate::time::sleep(1_000_000);
            }
        }
    }

    pub(super) fn wait_until_paused(deadline_ns: u64) -> bool {
        while STATE.load(Ordering::SeqCst) != PAUSED {
            if crate::time::now_ns() >= deadline_ns {
                return false;
            }
            crate::time::sleep(1_000_000);
        }
        true
    }

    pub(super) fn release() {
        STATE.store(RELEASED, Ordering::SeqCst);
    }

    pub(super) fn reset() {
        STATE.store(IDLE, Ordering::SeqCst);
    }
}

#[cfg(feature = "ktest")]
mod ktest_pollable {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

    use lumen_vfs::{
        AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, OpenGuard, PollGuard,
        PollMask, PollWaker, Pollable, SetTime, Timespec, VfsError, VfsInode, next_lock_id,
    };

    use crate::linux::process::{KernelFile, open_kernel_file};
    use crate::sync::SpinLock;
    use crate::vfs::KernelPollHead;

    type PollHook = fn(&TestPollableInode);

    /// Controllable [`VfsInode`] + [`Pollable`] for B3's ktests.
    pub(super) struct TestPollableInode {
        lock_id: u64,
        ino: Ino,
        open_count: AtomicU32,
        mask: AtomicU16,
        last_mode: SpinLock<Option<AccessMode>>,
        head: KernelPollHead,
        /// Optional one-shot hook called inside `poll()` AFTER reading `mask`
        /// when `poll_count` equals `hook_on_call`.
        poll_calls: AtomicU32,
        hook: SpinLock<Option<(u32, PollHook)>>,
    }

    impl TestPollableInode {
        pub(super) fn new(initial: PollMask) -> Arc<Self> {
            Arc::new(Self {
                lock_id: next_lock_id(),
                ino: lumen_vfs::next_detached_ino(),
                open_count: AtomicU32::new(0),
                mask: AtomicU16::new(initial.bits()),
                last_mode: SpinLock::new(None),
                head: KernelPollHead::new(),
                poll_calls: AtomicU32::new(0),
                hook: SpinLock::new(None),
            })
        }

        pub(super) fn set_mask(&self, mask: PollMask) {
            self.mask.store(mask.bits(), Ordering::SeqCst);
            self.head.notify(mask);
        }

        pub(super) fn set_mask_quiet(&self, mask: PollMask) {
            self.mask.store(mask.bits(), Ordering::SeqCst);
        }

        pub(super) fn notify(&self, mask: PollMask) {
            self.head.notify(mask);
        }

        pub(super) fn subscriber_count(&self) -> usize {
            self.head.subscriber_count()
        }

        pub(super) fn last_mode(&self) -> Option<AccessMode> {
            *self.last_mode.lock()
        }

        pub(super) fn set_after_read_hook(&self, on_call: u32, f: fn(&TestPollableInode)) {
            self.poll_calls.store(0, Ordering::SeqCst);
            *self.hook.lock() = Some((on_call, f));
        }

        pub(super) fn open_file(self: &Arc<Self>, flags: u32) -> Result<KernelFile, VfsError> {
            let inode: Arc<dyn VfsInode> = Arc::clone(self) as Arc<dyn VfsInode>;
            // Verify `OpenGuard::try_new` works on this inode (nlink == 1).
            let _check = OpenGuard::try_new(Arc::clone(&inode))?;
            KernelFile::new_unchecked_flags(inode, Arc::from(&b"/test-pollable"[..]), OpenFlags::from_bits(flags))
        }
    }

    impl Pollable for TestPollableInode {
        fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
            *self.last_mode.lock() = Some(mode);
            let call_num = self.poll_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let guard = waker.map(|w| self.head.subscribe(Arc::clone(w)));
            let current = PollMask::from_bits(self.mask.load(Ordering::SeqCst));
            let hook_fn = {
                let mut slot = self.hook.lock();
                if slot.is_some_and(|(target, _)| target == call_num) { slot.take().map(|(_, f)| f) } else { None }
            };
            if let Some(f) = hook_fn {
                f(self);
            }
            (current, guard)
        }
    }

    impl VfsInode for TestPollableInode {
        fn lock_id(&self) -> u64 {
            self.lock_id
        }
        fn dev_id(&self) -> DevId {
            DevId::makedev(0, 250)
        }
        fn ino(&self) -> Ino {
            self.ino
        }
        fn inode_type(&self) -> InodeType {
            InodeType::CharDevice
        }
        fn open_count(&self) -> u32 {
            self.open_count.load(Ordering::Acquire)
        }
        fn inc_open_count(&self) {
            self.open_count.fetch_add(1, Ordering::AcqRel);
        }
        fn dec_open_count(&self) -> u32 {
            self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
        }
        fn pollable(&self) -> Option<&dyn Pollable> {
            Some(self)
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            Ok(Metadata {
                dev: self.dev_id(),
                ino: self.ino,
                inode_type: InodeType::CharDevice,
                mode: 0o666,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: DevId::makedev(0, 0),
                size: 0,
                blksize: 4096,
                blocks: 0,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            })
        }
        fn lookup(&self, _name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
            Err(VfsError::NotDir)
        }
        lumen_vfs::fixed_namespace_ops!();
        fn readlink(&self) -> Result<Vec<u8>, VfsError> {
            Err(VfsError::Inval)
        }
        fn readdir(&self, _cursor: u64, _out: &mut Vec<DirEntry>, _max_entries: usize) -> Result<(), VfsError> {
            Err(VfsError::NotDir)
        }
        fn truncate(&self, _size: u64) -> Result<(), VfsError> {
            Err(VfsError::Inval)
        }
        fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
            Ok(0)
        }
        fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
            Ok(buf.len())
        }
        fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
            Err(VfsError::Inval)
        }
        fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
            Err(VfsError::Inval)
        }
        fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
            Ok(())
        }
        fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
            let mut locks = AdvisoryLocks::new();
            f(&mut locks);
            Ok(())
        }
    }

    pub(super) fn open_tmp_regular(path: &[u8]) -> Result<KernelFile, VfsError> {
        open_kernel_file(path, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)
    }
}

crate::ktest!(
    fn poll_masks_of_each_file_kind() {
        use super::fd::{ktest_install_file, ktest_open, ktest_unlink};
        use super::neg_errno;
        use crate::{kassert, kassert_eq};
        use ktest_pollable::{TestPollableInode, open_tmp_regular};
        use lumen_signal::{Blocked, SigSet, Signal};
        use lumen_vfs::{AccessMode, OpenFlags};

        const TMP_FILE: &[u8] = b"/tmp/poll-ktest-regular";

        let res = process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let fds_va = base + 512;
            let rset_va = base + 1024;
            let wset_va = base + 1152;
            let xset_va = base + 1280;
            let tv_va = base + 1408;
            let ts_va = base + 1440;
            let sig_va = base + 1472;
            let mask_va = base + 1504;
            let unmapped = process::KTEST_USER_VA + process::KTEST_USER_LEN;

            let write_pollfds = |entries: &[(i32, u16)]| -> Result<(), &'static str> {
                let mut buf = Vec::with_capacity(entries.len() * 8);
                for &(fd, ev) in entries {
                    buf.extend_from_slice(&fd.to_le_bytes());
                    buf.extend_from_slice(&(ev as i16).to_le_bytes());
                    buf.extend_from_slice(&0x5a5ai16.to_le_bytes());
                }
                copy_to_user(fds_va, &buf).map_err(|_| "write pollfds")
            };
            let read_revents = |count: usize| -> Result<Vec<u16>, &'static str> {
                let mut buf = vec![0u8; count * 8];
                copy_from_user(&mut buf, fds_va).map_err(|_| "read pollfds")?;
                Ok(buf
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(c[6..8].try_into().expect("2 bytes")))
                    .collect())
            };
            let write_set = |va: u64, fds: &[i32]| -> Result<(), &'static str> {
                let mut words = [0u64; 16];
                for &fd in fds {
                    words[(fd as usize) / 64] |= 1u64 << ((fd as usize) % 64);
                }
                let mut bytes = [0u8; 128];
                for (i, w) in words.iter().enumerate() {
                    bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
                }
                copy_to_user(va, &bytes).map_err(|_| "write fd_set")
            };
            let read_set_has = |va: u64, fd: i32| -> Result<bool, &'static str> {
                let mut bytes = [0u8; 128];
                copy_from_user(&mut bytes, va).map_err(|_| "read fd_set")?;
                let idx = (fd as usize) / 64;
                let word = u64::from_le_bytes(bytes[idx * 8..idx * 8 + 8].try_into().expect("8 bytes"));
                Ok((word & (1u64 << ((fd as usize) % 64))) != 0)
            };

            // Open every M7 file kind: regular tmpfs file, directory, procfs
            // file and directory, devfs always-ready devices, console/tty, and
            // O_PATH.
            let reg_fd = ktest_install_file(open_tmp_regular(TMP_FILE).map_err(|_| "open tmp file")?)?;
            let dir_fd = ktest_open(0, b"/tmp", OpenFlags::O_RDONLY)?;
            let proc_file_fd = ktest_open(0, b"/proc/version", OpenFlags::O_RDONLY)?;
            let proc_dir_fd = ktest_open(0, b"/proc", OpenFlags::O_RDONLY)?;
            let null_fd = ktest_open(0, b"/dev/null", OpenFlags::O_RDWR)?;
            let zero_fd = ktest_open(0, b"/dev/zero", OpenFlags::O_RDWR)?;
            let urandom_fd = ktest_open(0, b"/dev/urandom", OpenFlags::O_RDONLY)?;
            let console_rw_fd = ktest_open(0, b"/dev/console", OpenFlags::O_RDWR)?;
            let tty_rw_fd = ktest_open(0, b"/dev/tty", OpenFlags::O_RDWR)?;
            let path_fd = ktest_open(0, b"/tmp", OpenFlags::O_PATH)?;

            // Every M7 always-ready kind reports `POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM`
            // (never POLLPRI), filtered by requested events.
            let all_ev = (PollMask::IN | PollMask::PRI | PollMask::OUT | PollMask::RDNORM | PollMask::WRNORM).bits();
            let always = PollMask::ALWAYS_READY.bits();
            let in_only = (PollMask::IN | PollMask::RDNORM).bits();
            let out_only = (PollMask::OUT | PollMask::WRNORM).bits();
            let cases: &[(i32, u16, u16)] = &[
                (reg_fd.raw(), all_ev, always),
                (dir_fd.raw(), all_ev, always),
                (proc_file_fd.raw(), all_ev, always),
                (proc_dir_fd.raw(), all_ev, always),
                (null_fd.raw(), all_ev, always),
                (zero_fd.raw(), all_ev, always),
                (urandom_fd.raw(), all_ev, always),
                // Stdio fds 0/1/2 and /dev/console, /dev/tty by AccessMode (Capture mode).
                (0, all_ev, in_only),
                (1, all_ev, out_only),
                (console_rw_fd.raw(), all_ev, always),
                (tty_rw_fd.raw(), all_ev, always),
                // O_PATH and closed fds -> POLLNVAL; negative fd -> 0.
                (path_fd.raw(), all_ev, PollMask::NVAL.bits()),
                (99, all_ev, PollMask::NVAL.bits()),
                (-1, all_ev, 0),
            ];
            let req: Vec<(i32, u16)> = cases.iter().map(|&(fd, ev, _)| (fd, ev)).collect();
            write_pollfds(&req)?;
            kassert_eq!(sys_poll(fds_va, req.len() as u64, 0), (cases.len() - 1) as u64);
            let got = read_revents(cases.len())?;
            for (i, (_, _, want)) in cases.iter().enumerate() {
                kassert_eq!((i, got[i]), (i, *want));
            }

            // Controllable `TestPollable`: verify every bit, `AccessMode`
            // propagation, `POLLERR`/`POLLHUP` always reported, and `select`'s
            // `POLLIN_SET`/`POLLOUT_SET`/`POLLEX_SET`.
            let test_inode = TestPollableInode::new(PollMask::EMPTY);
            let tp_ro = ktest_install_file(test_inode.open_file(OpenFlags::O_RDONLY).map_err(|_| "open ro")?)?;
            let tp_wo = ktest_install_file(test_inode.open_file(OpenFlags::O_WRONLY).map_err(|_| "open wo")?)?;
            let tp_rw = ktest_install_file(test_inode.open_file(OpenFlags::O_RDWR).map_err(|_| "open rw")?)?;

            for (fd, want_mode) in [
                (tp_ro.raw(), AccessMode::ReadOnly),
                (tp_wo.raw(), AccessMode::WriteOnly),
                (tp_rw.raw(), AccessMode::ReadWrite),
            ] {
                write_pollfds(&[(fd, PollMask::IN.bits())])?;
                kassert_eq!(sys_poll(fds_va, 1, 0), 0);
                kassert_eq!(test_inode.last_mode(), Some(want_mode));
            }

            let zero_tv = RawTimeval { tv_sec: 0, tv_usec: 0 };
            // SAFETY: `RawTimeval` has zero padding bytes.
            unsafe { copy_pod_to_user(tv_va, &zero_tv) }.map_err(|_| "write tv")?;
            let zero_ts = RawTimespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: `RawTimespec` has zero padding bytes.
            unsafe { copy_pod_to_user(ts_va, &zero_ts) }.map_err(|_| "write ts")?;
            let max_n = tp_rw.raw() + 1;

            // EMPTY: neither poll nor select reports ready; select clears sets.
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                0
            );
            kassert!(!read_set_has(rset_va, tp_rw.raw())?);
            kassert!(!read_set_has(wset_va, tp_rw.raw())?);
            kassert!(!read_set_has(xset_va, tp_rw.raw())?);

            // POLLIN | POLLRDNORM: readable only.
            test_inode.set_mask_quiet(PollMask::IN | PollMask::RDNORM);
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                1
            );
            kassert!(read_set_has(rset_va, tp_rw.raw())?);
            kassert!(!read_set_has(wset_va, tp_rw.raw())?);
            kassert!(!read_set_has(xset_va, tp_rw.raw())?);

            // POLLOUT | POLLWRNORM: writable only.
            test_inode.set_mask_quiet(PollMask::OUT | PollMask::WRNORM);
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                1
            );
            kassert!(!read_set_has(rset_va, tp_rw.raw())?);
            kassert!(read_set_has(wset_va, tp_rw.raw())?);
            kassert!(!read_set_has(xset_va, tp_rw.raw())?);

            // POLLPRI: exceptional only (`POLLEX_SET`).
            test_inode.set_mask_quiet(PollMask::PRI);
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                1
            );
            kassert!(!read_set_has(rset_va, tp_rw.raw())?);
            kassert!(!read_set_has(wset_va, tp_rw.raw())?);
            kassert!(read_set_has(xset_va, tp_rw.raw())?);

            // POLLERR: reported by poll even with events=0; in select marks both
            // read and write (not except).
            test_inode.set_mask_quiet(PollMask::ERR);
            write_pollfds(&[(tp_rw.raw(), 0)])?;
            kassert_eq!(sys_poll(fds_va, 1, 0), 1);
            kassert_eq!(read_revents(1)?[0], PollMask::ERR.bits());
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                2
            );
            kassert!(read_set_has(rset_va, tp_rw.raw())?);
            kassert!(read_set_has(wset_va, tp_rw.raw())?);
            kassert!(!read_set_has(xset_va, tp_rw.raw())?);

            // POLLHUP: reported by poll even with events=0; in select marks read
            // only (`POLLIN_SET`, not `POLLOUT_SET`, not `POLLEX_SET`).
            test_inode.set_mask_quiet(PollMask::HUP);
            write_pollfds(&[(tp_rw.raw(), 0)])?;
            kassert_eq!(sys_poll(fds_va, 1, 0), 1);
            kassert_eq!(read_revents(1)?[0], PollMask::HUP.bits());
            write_set(rset_va, &[tp_rw.raw()])?;
            write_set(wset_va, &[tp_rw.raw()])?;
            write_set(xset_va, &[tp_rw.raw()])?;
            kassert_eq!(
                sys_select(
                    max_n,
                    UserPtr::from(rset_va),
                    UserPtr::from(wset_va),
                    UserPtr::from(xset_va),
                    UserPtr::from(tv_va)
                ),
                1
            );
            kassert!(read_set_has(rset_va, tp_rw.raw())?);
            kassert!(!read_set_has(wset_va, tp_rw.raw())?);
            kassert!(!read_set_has(xset_va, tp_rw.raw())?);

            // Error cases across poll, ppoll, select, pselect6.
            kassert_eq!(sys_poll(fds_va, DEFAULT_RLIMIT_NOFILE + 1, 0), neg_errno(errno::EINVAL));
            kassert_eq!(sys_poll(unmapped, 1, 0), neg_errno(errno::EFAULT));
            kassert_eq!(sys_poll(0, 0, 0), 0);

            kassert_eq!(
                sys_select(-1, UserPtr::from(0), UserPtr::from(0), UserPtr::from(0), UserPtr::from(0)),
                neg_errno(errno::EINVAL)
            );
            // Closed fd and O_PATH fd in select -> EBADF.
            write_set(rset_va, &[99])?;
            kassert_eq!(
                sys_select(100, UserPtr::from(rset_va), UserPtr::from(0), UserPtr::from(0), UserPtr::from(tv_va)),
                neg_errno(errno::EBADF)
            );
            write_set(rset_va, &[path_fd.raw()])?;
            kassert_eq!(
                sys_select(
                    path_fd.raw() + 1,
                    UserPtr::from(rset_va),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(tv_va),
                ),
                neg_errno(errno::EBADF)
            );
            // Negative timeval field -> EINVAL; bad pointers -> EFAULT.
            // SAFETY: `RawTimeval` has zero padding bytes.
            unsafe { copy_pod_to_user(tv_va, &RawTimeval { tv_sec: -1, tv_usec: 0 }) }.map_err(|_| "write bad tv")?;
            kassert_eq!(
                sys_select(0, UserPtr::from(0), UserPtr::from(0), UserPtr::from(0), UserPtr::from(tv_va)),
                neg_errno(errno::EINVAL)
            );
            // SAFETY: `RawTimeval` has zero padding bytes.
            unsafe { copy_pod_to_user(tv_va, &RawTimeval { tv_sec: 0, tv_usec: -5 }) }.map_err(|_| "write bad tv")?;
            kassert_eq!(
                sys_select(0, UserPtr::from(0), UserPtr::from(0), UserPtr::from(0), UserPtr::from(tv_va)),
                neg_errno(errno::EINVAL)
            );
            kassert_eq!(
                sys_select(1, UserPtr::from(unmapped), UserPtr::from(0), UserPtr::from(0), UserPtr::from(0)),
                neg_errno(errno::EFAULT)
            );
            kassert_eq!(
                sys_select(0, UserPtr::from(0), UserPtr::from(0), UserPtr::from(0), UserPtr::from(unmapped)),
                neg_errno(errno::EFAULT)
            );

            // ppoll / pselect6 bad timespec & sigsetsize, plus mask restoration.
            // SAFETY: `RawTimespec` has zero padding bytes.
            unsafe { copy_pod_to_user(ts_va, &RawTimespec { tv_sec: 0, tv_nsec: NS_PER_SEC as i64 }) }
                .map_err(|_| "write bad ts")?;
            kassert_eq!(
                sys_ppoll(UserPtr::from(fds_va), 0, UserPtr::from(ts_va), UserPtr::from(0), 8),
                neg_errno(errno::EINVAL)
            );
            kassert_eq!(
                sys_pselect6(
                    0,
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(ts_va),
                    UserPtr::from(0)
                ),
                neg_errno(errno::EINVAL)
            );
            // SAFETY: `RawTimespec` has zero padding bytes.
            unsafe { copy_pod_to_user(ts_va, &zero_ts) }.map_err(|_| "write zero ts")?;
            let initial_mask = Blocked::new(SigSet::of(&[Signal::USR1]));
            process::set_current_blocked(initial_mask);
            copy_to_user(mask_va, &SigSet::of(&[Signal::USR2]).0.to_le_bytes()).map_err(|_| "write mask")?;
            // Bad sigsetsize -> EINVAL, mask unchanged.
            kassert_eq!(
                sys_ppoll(UserPtr::from(fds_va), 0, UserPtr::from(ts_va), UserPtr::from(mask_va), 4),
                neg_errno(errno::EINVAL)
            );
            kassert_eq!(process::current_blocked(), initial_mask);
            // Valid ppoll mask swap restores original mask on return.
            kassert_eq!(sys_ppoll(UserPtr::from(fds_va), 0, UserPtr::from(ts_va), UserPtr::from(mask_va), 8), 0);
            kassert_eq!(process::current_blocked(), initial_mask);
            // pselect6 with ss != NULL, bad ss_len -> EINVAL.
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 4 }) }.map_err(|_| "write sig")?;
            kassert_eq!(
                sys_pselect6(
                    0,
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(ts_va),
                    UserPtr::from(sig_va)
                ),
                neg_errno(errno::EINVAL)
            );
            // pselect6 with ss == NULL ignores ss_len.
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: 0, ss_len: 99 }) }.map_err(|_| "write sig")?;
            kassert_eq!(
                sys_pselect6(
                    0,
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(ts_va),
                    UserPtr::from(sig_va)
                ),
                0
            );
            // pselect6 with valid ss restores original mask on return.
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 8 }) }.map_err(|_| "write sig")?;
            kassert_eq!(
                sys_pselect6(
                    0,
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(ts_va),
                    UserPtr::from(sig_va)
                ),
                0
            );
            kassert_eq!(process::current_blocked(), initial_mask);
            kassert_eq!(test_inode.subscriber_count(), 0);
            Ok(())
        })?;
        let _ = ktest_unlink(TMP_FILE);
        res
    }
);

crate::ktest!(
    fn poll_rechecks_after_a_wake() {
        use super::fd::ktest_install_file;
        use super::neg_errno;
        use crate::linux::signal::{raise_for_test, restart_result, restart_syscall};
        use crate::sync::SpinLock;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicU64;
        use ktest_pollable::TestPollableInode;
        use lumen_signal::Signal;
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let fds_va = base + 512;
            let rset_va = base + 768;
            let tv_va = base + 1024;
            let ts_va = base + 1056;

            let test_inode = TestPollableInode::new(PollMask::EMPTY);
            let fd = ktest_install_file(test_inode.open_file(OpenFlags::O_RDWR).map_err(|_| "open test")?)?;
            let fd_num = fd.raw();

            let write_pollfd = |va: u64, events: u16| -> Result<(), &'static str> {
                let mut raw = [0u8; 8];
                raw[0..4].copy_from_slice(&fd_num.to_le_bytes());
                raw[4..6].copy_from_slice(&(events as i16).to_le_bytes());
                copy_to_user(va, &raw).map_err(|_| "write pollfd")
            };
            let read_revents = |va: u64| -> Result<u16, &'static str> {
                let mut raw = [0u8; 8];
                copy_from_user(&mut raw, va).map_err(|_| "read pollfd")?;
                Ok(u16::from_le_bytes(raw[6..8].try_into().expect("2 bytes")))
            };

            // 1. Delay knob between clearing `fired` and the re-poll: a
            //    spurious wake wakes the waiter, it clears `fired` and pauses
            //    at the knob; a readiness change landing there must still end
            //    the wait with POLLIN.
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfd(fds_va, PollMask::IN.bits())?;
            recheck_window::arm();
            let waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, 5_000))?;
            // Wait until subscribed and blocked.
            let wait_dl = crate::time::now_ns() + 5_000_000_000;
            while test_inode.subscriber_count() == 0 || !waiter.blocked() {
                kassert!(crate::time::now_ns() < wait_dl);
                crate::time::sleep(1_000_000);
            }
            // Spurious wake (mask is still EMPTY, POLLOUT not requested).
            test_inode.notify(PollMask::OUT);
            kassert!(recheck_window::wait_until_paused(crate::time::now_ns() + 5_000_000_000));
            // Readiness change lands while paused between clearing `fired` and re-poll.
            test_inode.set_mask(PollMask::IN | PollMask::RDNORM);
            recheck_window::release();
            let got = waiter.join()?;
            recheck_window::reset();
            kassert_eq!(got, 1);
            kassert_eq!(read_revents(fds_va)?, PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 2. Mutant killer (re-polling before clearing `fired`): on the
            //    re-poll (2nd call to `poll()`), right AFTER reading EMPTY,
            //    the hook flips the source to POLLIN and notifies. Correct code
            //    already cleared `fired` BEFORE the re-poll, so `fired` stays
            //    true and the next wait wakes immediately instead of timing
            //    out.
            fn flip_after_empty_read(inode: &TestPollableInode) {
                inode.set_mask(PollMask::IN | PollMask::RDNORM);
            }
            test_inode.set_mask_quiet(PollMask::EMPTY);
            test_inode.set_after_read_hook(2, flip_after_empty_read);
            write_pollfd(fds_va, PollMask::IN.bits())?;
            let waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, 500))?;
            let wait_dl = crate::time::now_ns() + 5_000_000_000;
            while test_inode.subscriber_count() == 0 || !waiter.blocked() {
                kassert!(crate::time::now_ns() < wait_dl);
                crate::time::sleep(1_000_000);
            }
            // Spurious wake triggers the 2nd poll(), which reads EMPTY then
            // fires `flip_after_empty_read`.
            test_inode.notify(PollMask::OUT);
            let got = waiter.join()?;
            kassert_eq!(got, 1);
            kassert_eq!(read_revents(fds_va)?, PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 3. A waiter whose `ready` reads a `seq` counter wakes for a
            //    change made between its read and its block (never lost).
            let seq = AtomicU64::new(0);
            let queue = sched::WaitQueue::new();
            let seen = seq.load(Ordering::SeqCst);
            seq.fetch_add(1, Ordering::SeqCst);
            let _ = queue.wake_all();
            let dl = crate::time::now_ns() + 5_000_000_000;
            let woke = queue.wait_interruptible(Some(dl), || seq.load(Ordering::SeqCst) != seen);
            kassert_eq!(woke, sched::Wake::Woken);

            // 4. Interrupted waits drop their PollGuards (subscriber_count -> 0)
            //    and return the right restart code (`-ERESTART_RESTARTBLOCK`
            //    with a resumable RestartBlock for `poll`; `-ERESTARTNOHAND`
            //    for `ppoll`, `select`, `pselect6`).
            static WAITER_THREAD: SpinLock<Option<Arc<sched::Thread>>> = SpinLock::new(None);
            let wait_and_raise =
                |inode: &Arc<TestPollableInode>, t: &process::KtestThread<(u64, bool)>| -> crate::ktest::TestResult {
                    let dl = crate::time::now_ns() + 5_000_000_000;
                    while inode.subscriber_count() != 1 || !t.blocked() {
                        kassert!(crate::time::now_ns() < dl);
                        crate::time::sleep(1_000_000);
                    }
                    let thr = WAITER_THREAD.lock().clone().ok_or("waiter thread not published")?;
                    raise_for_test(&thr, Signal::USR1);
                    Ok(())
                };

            // 4a. `poll` interrupted -> `-ERESTART_RESTARTBLOCK`, guards dropped,
            //     RestartBlock kept with original deadline and resumable via `restart_syscall`.
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfd(fds_va, PollMask::IN.bits())?;
            let inode_for_resume = Arc::clone(&test_inode);
            let t = process::spawn_test_thread(move |_| {
                *WAITER_THREAD.lock() = sched::current();
                let r = sys_poll(fds_va, 1, 5_000);
                let has_block = sched::restart_block().and_then(|b| b.deadline).is_some();
                sched::clear_current_sigpending();
                // Now make the source ready and resume via `restart_syscall`.
                inode_for_resume.set_mask(PollMask::IN | PollMask::RDNORM);
                let resumed = restart_syscall();
                (r, has_block && resumed == 1)
            })?;
            wait_and_raise(&test_inode, &t)?;
            let (ret, resumed_ok) = t.join()?;
            *WAITER_THREAD.lock() = None;
            kassert_eq!(ret, restart_result(RestartCode::RestartBlock));
            kassert!(resumed_ok);
            kassert_eq!(read_revents(fds_va)?, PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 4b. `poll(NULL, 0, 5000)` interrupted -> `-ERESTART_RESTARTBLOCK` with deadline.
            let t = process::spawn_test_thread(move |_| {
                *WAITER_THREAD.lock() = sched::current();
                let r = sys_poll(0, 0, 5_000);
                let has_block = sched::take_restart_block().and_then(|b| b.deadline).is_some();
                sched::clear_current_sigpending();
                (r, has_block)
            })?;
            let dl = crate::time::now_ns() + 5_000_000_000;
            while !t.blocked() {
                kassert!(crate::time::now_ns() < dl);
                crate::time::sleep(1_000_000);
            }
            let thr = WAITER_THREAD.lock().clone().ok_or("waiter thread not published")?;
            raise_for_test(&thr, Signal::USR1);
            let (ret, has_block) = t.join()?;
            *WAITER_THREAD.lock() = None;
            kassert_eq!(ret, restart_result(RestartCode::RestartBlock));
            kassert!(has_block);

            // 4c. `ppoll` interrupted -> `-ERESTARTNOHAND`, guards dropped.
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfd(fds_va, PollMask::IN.bits())?;
            // SAFETY: `RawTimespec` has zero padding bytes.
            unsafe { copy_pod_to_user(ts_va, &RawTimespec { tv_sec: 5, tv_nsec: 0 }) }.map_err(|_| "write ts")?;
            let t = process::spawn_test_thread(move |_| {
                *WAITER_THREAD.lock() = sched::current();
                let r = sys_ppoll(UserPtr::from(fds_va), 1, UserPtr::from(ts_va), UserPtr::from(0), 8);
                sched::clear_current_sigpending();
                (r, sched::restart_block().is_none())
            })?;
            wait_and_raise(&test_inode, &t)?;
            let (ret, no_block) = t.join()?;
            *WAITER_THREAD.lock() = None;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            kassert!(no_block);
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 4d. `select` and `pselect6` interrupted -> `-ERESTARTNOHAND`, guards dropped.
            let mut rbytes = [0u8; 128];
            let word_idx = (fd_num as usize) / 64;
            let bit = 1u64 << ((fd_num as usize) % 64);
            rbytes[word_idx * 8..word_idx * 8 + 8].copy_from_slice(&bit.to_le_bytes());
            copy_to_user(rset_va, &rbytes).map_err(|_| "write rset")?;
            // SAFETY: `RawTimeval` has zero padding bytes.
            unsafe { copy_pod_to_user(tv_va, &RawTimeval { tv_sec: 5, tv_usec: 0 }) }.map_err(|_| "write tv")?;
            let t = process::spawn_test_thread(move |_| {
                *WAITER_THREAD.lock() = sched::current();
                let r = sys_select(
                    fd_num + 1,
                    UserPtr::from(rset_va),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(tv_va),
                );
                sched::clear_current_sigpending();
                (r, sched::restart_block().is_none())
            })?;
            wait_and_raise(&test_inode, &t)?;
            let (ret, no_block) = t.join()?;
            *WAITER_THREAD.lock() = None;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            kassert!(no_block);
            kassert_eq!(test_inode.subscriber_count(), 0);

            copy_to_user(rset_va, &rbytes).map_err(|_| "write rset")?;
            // SAFETY: `RawTimespec` has zero padding bytes.
            unsafe { copy_pod_to_user(ts_va, &RawTimespec { tv_sec: 5, tv_nsec: 0 }) }.map_err(|_| "write ts")?;
            let t = process::spawn_test_thread(move |_| {
                *WAITER_THREAD.lock() = sched::current();
                let r = sys_pselect6(
                    fd_num + 1,
                    UserPtr::from(rset_va),
                    UserPtr::from(0),
                    UserPtr::from(0),
                    UserPtr::from(ts_va),
                    UserPtr::from(0),
                );
                sched::clear_current_sigpending();
                (r, sched::restart_block().is_none())
            })?;
            wait_and_raise(&test_inode, &t)?;
            let (ret, no_block) = t.join()?;
            *WAITER_THREAD.lock() = None;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            kassert!(no_block);
            kassert_eq!(test_inode.subscriber_count(), 0);

            Ok(())
        })?
    }
);
