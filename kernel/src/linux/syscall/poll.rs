//! Readiness syscalls (`poll`, `ppoll`, `select`, `pselect6`; M8's B3, #374,
//! ARCHITECTURE sections 8 and 10).
//!
//! All four syscalls share [`wait_ready`]: a zero or already-expired timeout
//! runs one unsubscribed pass and returns immediately. Otherwise pass 1
//! subscribes a [`PollWait`] (holding `fired` and a per-call `WaitQueue`) to
//! every [`lumen_vfs::Pollable`] source while reading its mask; if nothing is
//! ready, the thread waits with `WaitQueue::wait_interruptible`, clears
//! `fired` before re-polling every source without re-subscribing, and keeps
//! every [`PollGuard`] until the call returns on any path (ready, timed out,
//! or interrupted).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "ktest")]
use lumen_signal::restart::RestartCode;
use lumen_signal::{Blocked, SigSet};
use lumen_vfs::{MAX_FDS, PollGuard, PollMask, PollWaker};

use super::at::{FromReg, UserPtr, reg};
use super::fd::current_io;
use super::{DEFAULT_RLIMIT_NOFILE, copy_pod_from_user, copy_pod_to_user, finish};
use crate::linux::process::{self, ConsoleIo, IoFile, KernelFile};
use crate::linux::signal::{RestartBlock, interrupted_with};
use crate::linux::{errno, general};
use crate::sched;
use crate::user::{CopyError, copy_from_user, copy_to_user};

#[cfg(feature = "ktest")]
use super::time::RawTimespec;
use super::time::{ERESTARTNOHAND, NS_PER_SEC, read_timespec, write_timespec};

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
struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
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
    assert!(size_of::<RawPollFd>() == size_of::<general::pollfd>() && size_of::<RawPollFd>() == 8);
    assert!(offset_of!(RawPollFd, fd) == offset_of!(general::pollfd, fd) && offset_of!(RawPollFd, fd) == 0);
    assert!(offset_of!(RawPollFd, events) == offset_of!(general::pollfd, events) && offset_of!(RawPollFd, events) == 4);
    assert!(
        offset_of!(RawPollFd, revents) == offset_of!(general::pollfd, revents) && offset_of!(RawPollFd, revents) == 6
    );
    assert!(size_of::<RawTimeval>() == size_of::<general::timeval>() && size_of::<RawTimeval>() == 16);
    assert!(offset_of!(RawTimeval, tv_sec) == offset_of!(general::timeval, tv_sec));
    assert!(offset_of!(RawTimeval, tv_usec) == offset_of!(general::timeval, tv_usec));
    assert!(size_of::<RawPselectSig>() == 16);
};

/// How long a readiness syscall may wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitTimeout {
    /// Non-blocking single pass (do not subscribe; do not write back remaining time).
    Zero,
    /// Block until monotonic `time::now_ns() >= dl`.
    Deadline(u64),
    /// Block indefinitely until ready or interrupted.
    Forever,
}

impl WaitTimeout {
    fn after_ns(dur: u64) -> Self {
        if dur == 0 { Self::Zero } else { Self::Deadline(crate::time::now_ns().saturating_add(dur)) }
    }

    fn from_deadline(dl: Option<u64>) -> Self {
        match dl {
            Some(dl) => Self::Deadline(dl),
            None => Self::Forever,
        }
    }
}

/// Outcome of [`wait_ready`]. `Ready(n)` guarantees `n > 0`.
enum WaitResult {
    Ready(u64),
    TimedOut,
    Interrupted,
}

/// Per-call [`PollWaker`]: sets `fired` and wakes its calling thread's
/// per-call `WaitQueue`. Never allocates or blocks in [`PollWaker::wake`].
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

/// Optional console recheck step for [`wait_ready`]: caps each sleep at
/// `step_ns` and flags IRQ 4's 100 ms safety-net step.
#[derive(Clone, Copy)]
struct ConsoleStep {
    step_ns: u64,
    serial_irq_safety: bool,
}

/// How the previous `wait_interruptible` in [`wait_ready`] ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastWait {
    None,
    Woken,
    TimedOut,
    SafetyStep,
}

/// Shared readiness wait loop for `poll`, `ppoll`, `select` and `pselect6`
/// (ARCHITECTURE section 10).
///
/// Zero and already-expired timeouts perform one unsubscribed pass and return
/// `Ready(n)` (`n > 0`) or `TimedOut`. Otherwise pass 1 passes `Some(&waker)`
/// so every [`lumen_vfs::Pollable`] subscribes no later than it reads its
/// mask; subsequent passes pass `None` while the returned [`PollGuard`]s stay
/// held, and all guards drop on every exit path.
fn wait_ready(
    timeout: WaitTimeout,
    console_step: Option<ConsoleStep>,
    mut pass: impl FnMut(Option<&Arc<dyn PollWaker>>, &mut Vec<PollGuard>) -> u64,
) -> WaitResult {
    let mut guards = Vec::new();
    let deadline = match timeout {
        WaitTimeout::Deadline(dl) if crate::time::now_ns() < dl => Some(dl),
        WaitTimeout::Zero | WaitTimeout::Deadline(_) => {
            let ready = pass(None, &mut guards);
            return if ready > 0 { WaitResult::Ready(ready) } else { WaitResult::TimedOut };
        }
        WaitTimeout::Forever => None,
    };

    let waiter = PollWait::new();
    let mut subscribe: Option<Arc<dyn PollWaker>> = Some(Arc::clone(&waiter) as Arc<dyn PollWaker>);
    let mut last_wait = LastWait::None;

    loop {
        let sub = subscribe.take();
        let ready = pass(sub.as_ref(), &mut guards);
        if ready > 0 {
            match last_wait {
                LastWait::SafetyStep => {
                    crate::serial_in::note_poll_safety_step(|| waiter.fired.load(Ordering::SeqCst));
                }
                LastWait::Woken if console_step.is_some_and(|cs| cs.serial_irq_safety) => {
                    crate::serial_in::note_poll_woken();
                }
                _ => {}
            }
            return WaitResult::Ready(ready);
        }

        if let Some(dl) = deadline
            && crate::time::now_ns() >= dl
        {
            return WaitResult::TimedOut;
        }
        let (step_dl, is_safety_step) = match (deadline, console_step) {
            (Some(dl), Some(cs)) => {
                let cap = crate::time::now_ns().saturating_add(cs.step_ns);
                (Some(dl.min(cap)), cs.serial_irq_safety && cap < dl)
            }
            (None, Some(cs)) => (Some(crate::time::now_ns().saturating_add(cs.step_ns)), cs.serial_irq_safety),
            (dl, None) => (dl, false),
        };
        match waiter.queue.wait_interruptible(step_dl, || waiter.fired.load(Ordering::SeqCst)) {
            sched::Wake::Woken => {
                // Clear `fired` BEFORE re-polling at the top of the loop so any
                // readiness change landing during or after the re-poll sets
                // `fired` again and wakes the next `wait_interruptible`
                // immediately.
                waiter.fired.store(false, Ordering::SeqCst);
                last_wait = LastWait::Woken;
                #[cfg(feature = "ktest")]
                recheck_window::pause_if_armed();
            }
            sched::Wake::TimedOut => {
                last_wait = if is_safety_step { LastWait::SafetyStep } else { LastWait::TimedOut };
            }
            sched::Wake::Interrupted => {
                return WaitResult::Interrupted;
            }
        }
    }
}

/// Read the current readiness of an open, non-`O_PATH` descriptor, subscribing
/// `waker` when provided and pushing any returned [`PollGuard`] onto `guards`.
fn poll_file(file: &KernelFile, waker: Option<&Arc<dyn PollWaker>>, guards: &mut Vec<PollGuard>) -> PollMask {
    static WARNED_NO_GUARD: AtomicBool = AtomicBool::new(false);
    if let Some(pipe) = file.pipe() {
        let (mask, guard) = pipe.poll_open(file.access_mode(), file.writer_generation(), waker);
        if let Some(guard) = guard {
            guards.push(guard);
        }
        return mask;
    }
    match file.inode().pollable() {
        Some(pollable) => {
            let (mask, guard) = pollable.poll(file.access_mode(), waker);
            match guard {
                Some(g) => guards.push(g),
                None if waker.is_some()
                    && !mask.intersects(PollMask::ERR | PollMask::HUP | PollMask::NVAL)
                    && !file.inode().is_tty() =>
                {
                    if !WARNED_NO_GUARD.swap(true, Ordering::Relaxed) {
                        log::warn!(
                            target: "poll",
                            "pollable inode {:?}:{} returned mask {:#x} without a PollGuard",
                            file.inode().dev_id(),
                            file.inode().ino(),
                            mask.bits(),
                        );
                    }
                    // `debug-assertions = true` in every Lumen profile, so this
                    // panics in release builds too (a `Pollable` without a wake
                    // source that is not re-polled via `console_step` would
                    // otherwise sleep forever).
                    debug_assert!(false, "pollable returned non-terminal mask without a PollGuard");
                }
                None => {}
            }
            mask
        }
        None => PollMask::ALWAYS_READY,
    }
}

/// Optional step bound for consoles that lack an interrupt-driven wake (VT,
/// polled serial) or keep a 100 ms safety-net recheck (IRQ 4 serial).
fn console_step(has_readable_tty: bool) -> Option<ConsoleStep> {
    if !has_readable_tty {
        return None;
    }
    match current_io() {
        ConsoleIo::Capture => None,
        ConsoleIo::Serial => {
            if crate::serial_in::by_interrupt() {
                Some(ConsoleStep { step_ns: SERIAL_SAFETY_STEP_NS, serial_irq_safety: true })
            } else {
                Some(ConsoleStep { step_ns: CONSOLE_POLL_STEP_NS, serial_irq_safety: false })
            }
        }
        ConsoleIo::Vt(_) => Some(ConsoleStep { step_ns: CONSOLE_POLL_STEP_NS, serial_irq_safety: false }),
    }
}

fn copy_timespec(ptr: UserPtr) -> Result<WaitTimeout, u32> {
    if ptr.is_null() {
        return Ok(WaitTimeout::Forever);
    }
    Ok(WaitTimeout::after_ns(read_timespec(ptr.addr())?))
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
    let dur =
        (tv.tv_sec as u64).saturating_mul(NS_PER_SEC).saturating_add((tv.tv_usec as u64).saturating_mul(NS_PER_US));
    Ok(WaitTimeout::after_ns(dur))
}

fn write_remaining_timespec(ptr: UserPtr, timeout: WaitTimeout, timed_out: bool) -> Result<(), u32> {
    let WaitTimeout::Deadline(dl) = timeout else {
        return Ok(());
    };
    if ptr.is_null() {
        return Ok(());
    }
    let rem_ns = if timed_out { 0 } else { dl.saturating_sub(crate::time::now_ns()) };
    write_timespec(ptr.addr(), rem_ns)
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

/// Apply Linux's `poll_select_finish` write-back rule for `ppoll`, `select`,
/// and `pselect6`: writes the remaining time back on every outcome (ready,
/// timed out, or any error after the timeout was read). A failed write-back
/// keeps a ready/timeout count or non-restart error, and turns
/// `-ERESTARTNOHAND` into `-EINTR` so the call cannot restart with an
/// un-updated timeout.
fn finish_timed(result: Result<u64, u32>, write_back: impl FnOnce(bool) -> Result<(), u32>) -> Result<u64, u32> {
    let wb = write_back(matches!(result, Ok(0)));
    match result {
        Ok(ready) => Ok(ready),
        Err(ERESTARTNOHAND) if wb.is_err() => Err(errno::EINTR),
        Err(e) => Err(e),
    }
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

/// Encode an interrupted `poll(2)`'s arguments and deadline into a
/// [`RestartBlock`] (paired with [`decode_poll_restart`]).
fn encode_poll_restart(fds_ptr: u64, nfds: u32, timeout: WaitTimeout) -> RestartBlock {
    let deadline = match timeout {
        WaitTimeout::Deadline(dl) => Some(dl),
        WaitTimeout::Forever => None,
        WaitTimeout::Zero => unreachable!("a zero-timeout poll never blocks or restarts"),
    };
    RestartBlock { resume: resume_poll, deadline, args: [fds_ptr, u64::from(nfds), 0, 0] }
}

/// Decode a `poll(2)` [`RestartBlock`] (paired with [`encode_poll_restart`]).
fn decode_poll_restart(block: &RestartBlock) -> (u64, u32, WaitTimeout) {
    (block.args[0], block.args[1] as u32, WaitTimeout::from_deadline(block.deadline))
}

/// Write back only the 2-byte `revents` field of every `struct pollfd`,
/// leaving `fd` and `events` untouched (as Linux's `do_sys_poll` does).
fn write_revents(fds_ptr: u64, entries: &[RawPollFd]) -> Result<(), u32> {
    let stride = size_of::<RawPollFd>() as u64;
    let revents_off = offset_of!(RawPollFd, revents) as u64;
    for (i, entry) in entries.iter().enumerate() {
        let addr = fds_ptr + (i as u64) * stride + revents_off;
        copy_to_user(addr, &entry.revents.to_le_bytes()).map_err(CopyError::to_errno)?;
    }
    Ok(())
}

/// Core `poll`/`ppoll` body, shared with [`resume_poll`].
///
/// Returns `Ok(ready)` (`0` on timeout), or `Err(errno)` (including
/// `ERESTART_RESTARTBLOCK` when `restartable` is true and `ERESTARTNOHAND` when
/// false). Writes back each entry's `revents` even on signal interruption,
/// returning `EFAULT` if the user array is unwritable.
fn run_poll(fds_ptr: u64, nfds: u32, timeout: WaitTimeout, restartable: bool) -> Result<u64, u32> {
    if u64::from(nfds) > DEFAULT_RLIMIT_NOFILE {
        return Err(errno::EINVAL);
    }
    let cnt = nfds as usize;
    let mut raw = vec![0u8; cnt * size_of::<RawPollFd>()];
    copy_from_user(&mut raw, fds_ptr).map_err(CopyError::to_errno)?;
    let mut entries: Vec<RawPollFd> = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| RawPollFd {
            fd: i32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes")),
            events: i16::from_le_bytes(chunk[4..6].try_into().expect("2 bytes")),
            revents: 0,
        })
        .collect();

    let polled_fds: Vec<i32> = entries.iter().map(|e| e.fd).collect();
    let files = process::current_io_files(&polled_fds);
    let has_readable_tty = entries.iter().zip(&files).any(|(e, file)| {
        e.fd >= 0 && file.as_ref().is_some_and(|f| f.is_readable() && f.inode().is_tty() && f.tty().is_none())
    });
    let step = console_step(has_readable_tty);

    let outcome = wait_ready(timeout, step, |waker, guards| {
        let mut ready = 0u64;
        for (entry, file) in entries.iter_mut().zip(&files) {
            let revents: u16 = if entry.fd < 0 {
                0
            } else {
                match file {
                    None => PollMask::NVAL.bits(),
                    Some(f) if f.is_path() => PollMask::NVAL.bits(),
                    Some(f) => {
                        let events = PollMask::from_bits(entry.events as u16);
                        poll_file(f, waker, guards).filter_events(events).bits()
                    }
                }
            };
            entry.revents = revents as i16;
            if revents != 0 {
                ready += 1;
            }
        }
        ready
    });
    drop(files);

    write_revents(fds_ptr, &entries)?;
    match outcome {
        WaitResult::Ready(ready) => Ok(ready),
        WaitResult::TimedOut => Ok(0),
        WaitResult::Interrupted => {
            if restartable {
                Err(interrupted_with(encode_poll_restart(fds_ptr, nfds, timeout)))
            } else {
                Err(ERESTARTNOHAND)
            }
        }
    }
}

/// Resumes an interrupted `poll(2)` via `restart_syscall(2)` toward its
/// original deadline.
fn resume_poll(block: &RestartBlock) -> u64 {
    let (fds_ptr, nfds, timeout) = decode_poll_restart(block);
    finish(run_poll(fds_ptr, nfds, timeout, true))
}

/// `poll(fds, nfds, timeout_ms)` (7).
pub(super) fn sys_poll(fds_ptr: u64, nfds: u64, timeout_raw: u64) -> u64 {
    let fds = reg::<UserPtr>(fds_ptr);
    let nfds = reg::<u32>(nfds);
    let timeout_ms = reg::<i32>(timeout_raw);
    let timeout = if timeout_ms < 0 {
        WaitTimeout::Forever
    } else {
        WaitTimeout::after_ns((timeout_ms as u64).saturating_mul(NS_PER_MS))
    };
    finish(run_poll(fds.addr(), nfds, timeout, true))
}

/// `ppoll(fds, nfds, tsp, sigmask, sigsetsize)` (271).
pub(super) fn sys_ppoll(fds: UserPtr, nfds: u32, tsp: UserPtr, sigmask: UserPtr, sigsetsize: u64) -> u64 {
    finish((|| {
        let timeout = copy_timespec(tsp)?;
        let mask = copy_sigmask(sigmask, sigsetsize)?;
        let thread = process::current_thread();
        if let Some(mask) = mask {
            thread.install_temporary_mask(Blocked::new(mask));
        }
        let result = run_poll(fds.addr(), nfds, timeout, false);
        thread.restore_saved_sigmask_unless(result == Err(ERESTARTNOHAND));
        finish_timed(result, |timed_out| write_remaining_timespec(tsp, timeout, timed_out))
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
/// Returns `Ok(ready_bits)` (`0` on timeout), or `Err(errno)` (`ERESTARTNOHAND`
/// on signal interruption). Only a closed descriptor returns `EBADF`; an
/// `O_PATH` descriptor answers `PollMask::NVAL` (matching Linux's
/// `select_poll_one`).
fn run_select(
    n: i32,
    readfds: UserPtr,
    writefds: UserPtr,
    exceptfds: UserPtr,
    timeout: WaitTimeout,
) -> Result<u64, u32> {
    if n < 0 {
        return Err(errno::EINVAL);
    }
    let n_capped = (n as usize).min(MAX_FDS);
    let words = fds_words(n_capped);

    let read_in = copy_fd_set_in(readfds, n_capped)?;
    let write_in = copy_fd_set_in(writefds, n_capped)?;
    let except_in = copy_fd_set_in(exceptfds, n_capped)?;

    struct SelectEntry {
        fd: usize,
        want_r: bool,
        want_w: bool,
        want_x: bool,
        file: IoFile,
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

    let raw_files = process::current_io_files(&wanted_fds);
    let mut entries = Vec::with_capacity(wanted_fds.len());
    let mut has_readable_tty = false;
    for ((fd, (want_r, want_w, want_x)), file) in wanted_fds.into_iter().zip(flags).zip(raw_files) {
        let Some(file) = file else {
            return Err(errno::EBADF);
        };
        if want_r && file.is_readable() && file.inode().is_tty() && file.tty().is_none() {
            has_readable_tty = true;
        }
        entries.push(SelectEntry { fd: fd as usize, want_r, want_w, want_x, file });
    }

    let step = console_step(has_readable_tty);
    let mut read_out = vec![0u64; words];
    let mut write_out = vec![0u64; words];
    let mut except_out = vec![0u64; words];

    let outcome = wait_ready(timeout, step, |waker, guards| {
        read_out.fill(0);
        write_out.fill(0);
        except_out.fill(0);
        let mut ready = 0u64;
        for entry in &entries {
            let mask = if entry.file.is_path() { PollMask::NVAL } else { poll_file(&entry.file, waker, guards) };
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

    let ready = match outcome {
        WaitResult::Ready(ready) => ready,
        WaitResult::TimedOut => 0,
        WaitResult::Interrupted => return Err(ERESTARTNOHAND),
    };
    copy_fd_set_out(readfds, &read_out)?;
    copy_fd_set_out(writefds, &write_out)?;
    copy_fd_set_out(exceptfds, &except_out)?;
    Ok(ready)
}

/// `select(n, readfds, writefds, exceptfds, tv)` (23).
pub(super) fn sys_select(n: i32, readfds: UserPtr, writefds: UserPtr, exceptfds: UserPtr, tv: UserPtr) -> u64 {
    finish((|| {
        let timeout = copy_timeval(tv)?;
        finish_timed(run_select(n, readfds, writefds, exceptfds, timeout), |timed_out| {
            write_remaining_timeval(tv, timeout, timed_out)
        })
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
        // Read `sig`'s `{ ss, ss_len }` argpack before `tsp` (Linux's
        // `SYSCALL_DEFINE6(pselect6)`), then `tsp`, then `copy_sigmask` (as
        // `do_pselect` orders them).
        let sig_arg = if sig.is_null() {
            None
        } else {
            // SAFETY: `RawPselectSig` is two `u64`s (16 bytes, zero padding); every bit pattern is valid.
            Some(unsafe { copy_pod_from_user::<RawPselectSig>(sig.addr()) }.map_err(CopyError::to_errno)?)
        };
        let timeout = copy_timespec(tsp)?;
        let mask = match sig_arg {
            Some(arg) => copy_sigmask(UserPtr::from_reg(arg.ss), arg.ss_len)?,
            None => None,
        };
        let thread = process::current_thread();
        if let Some(mask) = mask {
            thread.install_temporary_mask(Blocked::new(mask));
        }
        let result = run_select(n, readfds, writefds, exceptfds, timeout);
        thread.restore_saved_sigmask_unless(result == Err(ERESTARTNOHAND));
        finish_timed(result, |timed_out| write_remaining_timespec(tsp, timeout, timed_out))
    })())
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
pub(super) mod ktest_pollable {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

    use lumen_vfs::{
        AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, OpenGuard, PollGuard,
        PollMask, PollWaker, Pollable, SetTime, Timespec, VfsError, VfsInode, next_lock_id,
    };

    use super::{FromReg, RawTimespec, RawTimeval, UserPtr, copy_pod_to_user, sys_ppoll, sys_pselect6, sys_select};
    use crate::linux::process::{self, KernelFile, KtestThread, open_kernel_file};
    use crate::linux::signal::raise_for_test;
    use crate::sync::SpinLock;
    use crate::user::{copy_from_user, copy_to_user};
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
        /// Counts calls to [`Pollable::poll`] since the last [`Self::set_after_read_hook`].
        poll_calls: AtomicU32,
        /// Optional one-shot `(target_call, callback)` invoked inside
        /// [`Pollable::poll`] after reading `mask` when `poll_calls` reaches
        /// `target_call`.
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

        pub(super) fn set_after_read_hook(&self, on_call: u32, f: PollHook) {
            self.poll_calls.store(0, Ordering::SeqCst);
            *self.hook.lock() = Some((on_call, f));
        }

        pub(super) fn open_file(self: &Arc<Self>, flags: u32) -> Result<KernelFile, VfsError> {
            let inode: Arc<dyn VfsInode> = Arc::clone(self) as Arc<dyn VfsInode>;
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

    pub(crate) fn write_pollfds(va: u64, entries: &[(i32, u16)]) -> Result<(), &'static str> {
        let mut buf = Vec::with_capacity(entries.len() * 8);
        for &(fd, ev) in entries {
            buf.extend_from_slice(&fd.to_le_bytes());
            buf.extend_from_slice(&(ev as i16).to_le_bytes());
            buf.extend_from_slice(&0x5a5ai16.to_le_bytes());
        }
        copy_to_user(va, &buf).map_err(|_| "write pollfds")
    }

    pub(crate) fn read_revents(va: u64, count: usize) -> Result<Vec<u16>, &'static str> {
        let mut buf = vec![0u8; count * 8];
        copy_from_user(&mut buf, va).map_err(|_| "read pollfds")?;
        Ok(buf.as_chunks::<8>().0.iter().map(|c| u16::from_le_bytes(c[6..8].try_into().expect("2 bytes"))).collect())
    }

    pub(super) fn write_fd_set(va: u64, fds: &[i32]) -> Result<(), &'static str> {
        let mut words = [0u64; 16];
        for &fd in fds {
            words[(fd as usize) / 64] |= 1u64 << ((fd as usize) % 64);
        }
        let mut bytes = [0u8; 128];
        for (i, w) in words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        copy_to_user(va, &bytes).map_err(|_| "write fd_set")
    }

    pub(super) fn read_fd_set_has(va: u64, fd: i32) -> Result<bool, &'static str> {
        let mut bytes = [0u8; 128];
        copy_from_user(&mut bytes, va).map_err(|_| "read fd_set")?;
        let idx = (fd as usize) / 64;
        let word = u64::from_le_bytes(bytes[idx * 8..idx * 8 + 8].try_into().expect("8 bytes"));
        Ok((word & (1u64 << ((fd as usize) % 64))) != 0)
    }

    pub(super) fn write_timespec(va: u64, sec: i64, nsec: i64) -> Result<(), &'static str> {
        // SAFETY: `RawTimespec` has zero padding bytes.
        unsafe { copy_pod_to_user(va, &RawTimespec { tv_sec: sec, tv_nsec: nsec }) }.map_err(|_| "write timespec")
    }

    pub(super) fn write_timeval(va: u64, sec: i64, usec: i64) -> Result<(), &'static str> {
        // SAFETY: `RawTimeval` has zero padding bytes.
        unsafe { copy_pod_to_user(va, &RawTimeval { tv_sec: sec, tv_usec: usec }) }.map_err(|_| "write timeval")
    }

    /// Poll `cond` every 1 ms until true, or fail after 5 seconds.
    pub(crate) fn wait_for(mut cond: impl FnMut() -> bool) -> crate::ktest::TestResult {
        let dl = crate::time::now_ns() + 5_000_000_000;
        while !cond() {
            crate::kassert!(crate::time::now_ns() < dl);
            crate::time::sleep(1_000_000);
        }
        Ok(())
    }

    pub(super) fn ppoll_at(fds: u64, nfds: u32, tsp: u64, sigmask: u64, sigsetsize: u64) -> u64 {
        sys_ppoll(UserPtr::from_reg(fds), nfds, UserPtr::from_reg(tsp), UserPtr::from_reg(sigmask), sigsetsize)
    }

    pub(super) fn select_at(n: i32, readfds: u64, writefds: u64, exceptfds: u64, tv: u64) -> u64 {
        sys_select(
            n,
            UserPtr::from_reg(readfds),
            UserPtr::from_reg(writefds),
            UserPtr::from_reg(exceptfds),
            UserPtr::from_reg(tv),
        )
    }

    pub(super) fn pselect6_at(n: i32, readfds: u64, writefds: u64, exceptfds: u64, tsp: u64, sig: u64) -> u64 {
        sys_pselect6(
            n,
            UserPtr::from_reg(readfds),
            UserPtr::from_reg(writefds),
            UserPtr::from_reg(exceptfds),
            UserPtr::from_reg(tsp),
            UserPtr::from_reg(sig),
        )
    }

    pub(super) fn raise_usr1_on_self() {
        if let Some(me) = crate::sched::current() {
            raise_for_test(&me, lumen_signal::Signal::USR1);
        }
    }

    /// Spawn a waiter running `call`, wait until subscribed and blocked on
    /// `inode`, raise `SIGUSR1`, and verify return value `want`, no restart
    /// block, and `subscriber_count() == 0`.
    pub(super) fn expect_interrupted(
        inode: &Arc<TestPollableInode>,
        want: u64,
        call: impl FnOnce() -> u64 + Send + 'static,
    ) -> crate::ktest::TestResult {
        inode.set_mask_quiet(PollMask::EMPTY);
        let t: KtestThread<(u64, bool)> = process::spawn_test_thread(move |_| {
            let r = call();
            let no_block = crate::sched::restart_block().is_none();
            crate::sched::clear_current_sigpending();
            (r, no_block)
        })?;
        wait_for(|| inode.subscriber_count() == 1 && t.blocked())?;
        raise_for_test(t.thread(), lumen_signal::Signal::USR1);
        let (ret, no_block) = t.join()?;
        crate::kassert_eq!(ret, want);
        crate::kassert!(no_block);
        crate::kassert_eq!(inode.subscriber_count(), 0);
        Ok(())
    }
}

crate::ktest!(
    fn poll_masks_of_each_file_kind() {
        use super::at;
        use super::fd::{ktest_install_file, ktest_open, ktest_unlink};
        use super::mem::sys_mprotect;
        use super::neg_errno;
        use crate::{kassert, kassert_eq};
        use ktest_pollable::{
            TestPollableInode, open_tmp_regular, ppoll_at, pselect6_at, read_fd_set_has, read_revents, select_at,
            write_fd_set, write_pollfds, write_timespec, write_timeval,
        };
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
            let ro_page = base + 3 * 4096;
            let unmapped = process::KTEST_USER_VA + process::KTEST_USER_LEN;

            // Open every M7 file kind: regular tmpfs file, directory, procfs
            // file and directory, devfs always-ready devices, consoles, and
            // O_PATH.
            let reg_fd = ktest_install_file(open_tmp_regular(TMP_FILE).map_err(|_| "open tmp file")?)?;
            let dir_fd = ktest_open(0, b"/tmp", OpenFlags::O_RDONLY)?;
            let proc_file_fd = ktest_open(0, b"/proc/version", OpenFlags::O_RDONLY)?;
            let proc_dir_fd = ktest_open(0, b"/proc", OpenFlags::O_RDONLY)?;
            let null_fd = ktest_open(0, b"/dev/null", OpenFlags::O_RDWR)?;
            let zero_fd = ktest_open(0, b"/dev/zero", OpenFlags::O_RDWR)?;
            let urandom_fd = ktest_open(0, b"/dev/urandom", OpenFlags::O_RDONLY)?;
            let console_rw_fd = ktest_open(0, b"/dev/console", OpenFlags::O_RDWR)?;
            let tty_rw_fd = ktest_open(0, b"/dev/console", OpenFlags::O_RDWR)?;
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
                // Stdio fds 0/1/2 and /dev/console by AccessMode (Capture mode).
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
            write_pollfds(fds_va, &req)?;
            kassert_eq!(sys_poll(fds_va, req.len() as u64, 0), (cases.len() - 1) as u64);
            let got = read_revents(fds_va, cases.len())?;
            for (i, (_, _, want)) in cases.iter().enumerate() {
                kassert_eq!((i, got[i]), (i, *want));
            }

            // C7: pipe readiness rows (empty -> OUT only, data -> IN | OUT,
            // closed write end with data -> IN | HUP, drained -> HUP, closed
            // read end -> OUT | ERR).
            let pipe_fds_va = base + 1536;
            kassert_eq!(super::pipe::sys_pipe(UserPtr::from_reg(pipe_fds_va)), 0);
            // SAFETY: `[i32; 2]` is 8 bytes with no padding.
            let p1 = unsafe { copy_pod_from_user::<[i32; 2]>(pipe_fds_va) }.map_err(|_| "read pipe1")?;
            let (pr1, pw1) = (p1[0], p1[1]);
            write_pollfds(fds_va, &[(pr1, all_ev), (pw1, all_ev)])?;
            kassert_eq!(sys_poll(fds_va, 2, 0), 1);
            kassert_eq!(read_revents(fds_va, 2)?, alloc::vec![0, out_only]);

            copy_to_user(base + 1568, b"ping").map_err(|_| "write pipe payload")?;
            kassert_eq!(super::fd::sys_write(at::reg(pw1 as u64), UserPtr::from_reg(base + 1568), 4), 4);
            write_pollfds(fds_va, &[(pr1, all_ev), (pw1, all_ev)])?;
            kassert_eq!(sys_poll(fds_va, 2, 0), 2);
            kassert_eq!(read_revents(fds_va, 2)?, alloc::vec![in_only, out_only]);

            kassert_eq!(super::fd::sys_close(at::reg(pw1 as u64)), 0);
            write_pollfds(fds_va, &[(pr1, all_ev)])?;
            kassert_eq!(sys_poll(fds_va, 1, 0), 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], in_only | PollMask::HUP.bits());

            kassert_eq!(super::fd::sys_read(at::reg(pr1 as u64), UserPtr::from_reg(base + 1568), 4), 4);
            write_pollfds(fds_va, &[(pr1, all_ev)])?;
            kassert_eq!(sys_poll(fds_va, 1, 0), 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::HUP.bits());
            kassert_eq!(super::fd::sys_close(at::reg(pr1 as u64)), 0);

            kassert_eq!(super::pipe::sys_pipe(UserPtr::from_reg(pipe_fds_va)), 0);
            // SAFETY: `[i32; 2]` is 8 bytes with no padding.
            let p2 = unsafe { copy_pod_from_user::<[i32; 2]>(pipe_fds_va) }.map_err(|_| "read pipe2")?;
            kassert_eq!(super::fd::sys_close(at::reg(p2[0] as u64)), 0);
            write_pollfds(fds_va, &[(p2[1], all_ev)])?;
            kassert_eq!(sys_poll(fds_va, 1, 0), 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], out_only | PollMask::ERR.bits());
            kassert_eq!(super::fd::sys_close(at::reg(p2[1] as u64)), 0);

            // `sys_poll` decodes `nfds` from the low 32 bits (upper bits ignored).
            write_pollfds(fds_va, &[(reg_fd.raw(), PollMask::IN.bits())])?;
            kassert_eq!(sys_poll(fds_va, (1u64 << 32) | 1, 0), 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::IN.bits());

            // Controllable `TestPollable`: verify `AccessMode` propagation,
            // `POLLERR`/`POLLHUP` always reported, and `select`'s
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
                write_pollfds(fds_va, &[(fd, PollMask::IN.bits())])?;
                kassert_eq!(sys_poll(fds_va, 1, 0), 0);
                kassert_eq!(test_inode.last_mode(), Some(want_mode));
            }

            write_timeval(tv_va, 0, 0)?;
            write_timespec(ts_va, 0, 0)?;
            let max_n = tp_rw.raw() + 1;

            let select_table: &[(PollMask, u64, bool, bool, bool)] = &[
                (PollMask::EMPTY, 0, false, false, false),
                (PollMask::IN | PollMask::RDNORM, 1, true, false, false),
                (PollMask::OUT | PollMask::WRNORM, 1, false, true, false),
                (PollMask::PRI, 1, false, false, true),
                (PollMask::ERR, 2, true, true, false),
                (PollMask::HUP, 1, true, false, false),
            ];
            for &(mask, want_ret, want_r, want_w, want_x) in select_table {
                test_inode.set_mask_quiet(mask);
                write_fd_set(rset_va, &[tp_rw.raw()])?;
                write_fd_set(wset_va, &[tp_rw.raw()])?;
                write_fd_set(xset_va, &[tp_rw.raw()])?;
                kassert_eq!(select_at(max_n, rset_va, wset_va, xset_va, tv_va), want_ret);
                kassert_eq!(read_fd_set_has(rset_va, tp_rw.raw())?, want_r);
                kassert_eq!(read_fd_set_has(wset_va, tp_rw.raw())?, want_w);
                kassert_eq!(read_fd_set_has(xset_va, tp_rw.raw())?, want_x);
            }

            // POLLERR and POLLHUP are reported by `poll` even with `events == 0`.
            for mask in [PollMask::ERR, PollMask::HUP] {
                test_inode.set_mask_quiet(mask);
                write_pollfds(fds_va, &[(tp_rw.raw(), 0)])?;
                kassert_eq!(sys_poll(fds_va, 1, 0), 1);
                kassert_eq!(read_revents(fds_va, 1)?[0], mask.bits());
            }

            // `select`/`pselect6` on an `O_PATH` fd: Linux's `select_poll_one`
            // returns `POLLNVAL`, which sits in all three of `POLLIN_SET`,
            // `POLLOUT_SET` and `POLLEX_SET` (not `-EBADF`).
            write_fd_set(rset_va, &[path_fd.raw()])?;
            write_fd_set(wset_va, &[path_fd.raw()])?;
            write_fd_set(xset_va, &[path_fd.raw()])?;
            kassert_eq!(select_at(path_fd.raw() + 1, rset_va, wset_va, xset_va, tv_va), 3);
            kassert!(read_fd_set_has(rset_va, path_fd.raw())?);
            kassert!(read_fd_set_has(wset_va, path_fd.raw())?);
            kassert!(read_fd_set_has(xset_va, path_fd.raw())?);

            // Error cases across poll, ppoll, select, pselect6.
            kassert_eq!(sys_poll(fds_va, DEFAULT_RLIMIT_NOFILE + 1, 0), neg_errno(errno::EINVAL));
            kassert_eq!(sys_poll(unmapped, 1, 0), neg_errno(errno::EFAULT));
            kassert_eq!(sys_poll(0, 0, 0), 0);

            kassert_eq!(select_at(-1, 0, 0, 0, 0), neg_errno(errno::EINVAL));
            // Closed fd in select -> EBADF, and remaining time is written back.
            write_fd_set(rset_va, &[99])?;
            write_timeval(tv_va, 5, 0)?;
            kassert_eq!(select_at(100, rset_va, 0, 0, tv_va), neg_errno(errno::EBADF));
            // SAFETY: `RawTimeval` is two `i64`s; every bit pattern is valid.
            let tv_after_ebadf = unsafe { copy_pod_from_user::<RawTimeval>(tv_va) }.map_err(|_| "read tv")?;
            let rem_ebadf_us = (tv_after_ebadf.tv_sec as u64) * US_PER_SEC + tv_after_ebadf.tv_usec as u64;
            kassert!(rem_ebadf_us < 5 * US_PER_SEC);
            // Negative timeval field -> EINVAL; bad pointers -> EFAULT.
            write_timeval(tv_va, -1, 0)?;
            kassert_eq!(select_at(0, 0, 0, 0, tv_va), neg_errno(errno::EINVAL));
            write_timeval(tv_va, 0, -5)?;
            kassert_eq!(select_at(0, 0, 0, 0, tv_va), neg_errno(errno::EINVAL));
            kassert_eq!(select_at(1, unmapped, 0, 0, 0), neg_errno(errno::EFAULT));
            kassert_eq!(select_at(0, 0, 0, 0, unmapped), neg_errno(errno::EFAULT));

            // `pselect6` reads `sig`'s argpack before `tsp` (bad `sig` beats
            // bad `tsp`), then `tsp` before `copy_sigmask` (bad `tsp` beats
            // bad `sig.ss_len`, as Linux's `do_pselect` orders them).
            write_timespec(ts_va, 0, NS_PER_SEC as i64)?;
            kassert_eq!(pselect6_at(0, 0, 0, 0, ts_va, unmapped), neg_errno(errno::EFAULT));
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 4 }) }.map_err(|_| "write sig")?;
            kassert_eq!(pselect6_at(0, 0, 0, 0, unmapped, sig_va), neg_errno(errno::EFAULT));
            kassert_eq!(ppoll_at(fds_va, 0, ts_va, 0, 8), neg_errno(errno::EINVAL));
            kassert_eq!(pselect6_at(0, 0, 0, 0, ts_va, 0), neg_errno(errno::EINVAL));
            write_timespec(ts_va, 0, 0)?;
            let initial_mask = Blocked::new(SigSet::of(&[Signal::USR1]));
            process::set_current_blocked(initial_mask);
            copy_to_user(mask_va, &SigSet::of(&[Signal::USR2]).0.to_le_bytes()).map_err(|_| "write mask")?;
            kassert_eq!(ppoll_at(fds_va, 0, ts_va, mask_va, 4), neg_errno(errno::EINVAL));
            kassert_eq!(process::current_blocked(), initial_mask);
            kassert_eq!(ppoll_at(fds_va, 0, ts_va, mask_va, 8), 0);
            kassert_eq!(process::current_blocked(), initial_mask);
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 4 }) }.map_err(|_| "write sig")?;
            kassert_eq!(pselect6_at(0, 0, 0, 0, ts_va, sig_va), neg_errno(errno::EINVAL));
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: 0, ss_len: 99 }) }.map_err(|_| "write sig")?;
            kassert_eq!(pselect6_at(0, 0, 0, 0, ts_va, sig_va), 0);
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 8 }) }.map_err(|_| "write sig")?;
            kassert_eq!(pselect6_at(0, 0, 0, 0, ts_va, sig_va), 0);
            kassert_eq!(process::current_blocked(), initial_mask);

            // Failed timeout write-back on a read-only page keeps the ready
            // count on success, `0` on timeout, and non-restart errors such as
            // `-EBADF` (Linux's `poll_select_finish`).
            write_timespec(ro_page, 5, 0)?;
            write_timeval(ro_page + 32, 5, 0)?;
            write_timespec(ro_page + 64, 0, 1)?;
            write_timeval(ro_page + 96, 0, 1)?;
            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ)), 0);
            write_pollfds(fds_va, &[(reg_fd.raw(), PollMask::IN.bits())])?;
            kassert_eq!(ppoll_at(fds_va, 1, ro_page, 0, 8), 1);
            write_fd_set(rset_va, &[reg_fd.raw()])?;
            kassert_eq!(select_at(reg_fd.raw() + 1, rset_va, 0, 0, ro_page + 32), 1);
            write_fd_set(rset_va, &[reg_fd.raw()])?;
            kassert_eq!(pselect6_at(reg_fd.raw() + 1, rset_va, 0, 0, ro_page, 0), 1);
            // Timed out with read-only timeout -> 0, not `-EFAULT`.
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfds(fds_va, &[(tp_rw.raw(), PollMask::IN.bits())])?;
            kassert_eq!(ppoll_at(fds_va, 1, ro_page + 64, 0, 8), 0);
            write_fd_set(rset_va, &[tp_rw.raw()])?;
            kassert_eq!(select_at(max_n, rset_va, 0, 0, ro_page + 96), 0);
            write_fd_set(rset_va, &[tp_rw.raw()])?;
            kassert_eq!(pselect6_at(max_n, rset_va, 0, 0, ro_page + 64, 0), 0);
            // Non-restart error with read-only timeout keeps `-EBADF`.
            write_fd_set(rset_va, &[99])?;
            kassert_eq!(select_at(100, rset_va, 0, 0, ro_page + 32), neg_errno(errno::EBADF));
            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ | general::PROT_WRITE)), 0);

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
        use super::mem::sys_mprotect;
        use super::neg_errno;
        use crate::linux::signal::{raise_for_test, restart_result, restart_syscall};
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicU64;
        use ktest_pollable::{
            TestPollableInode, expect_interrupted, ppoll_at, pselect6_at, raise_usr1_on_self, read_revents, select_at,
            wait_for, write_fd_set, write_pollfds, write_timespec, write_timeval,
        };
        use lumen_signal::Signal;
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let fds_va = base + 512;
            let rset_va = base + 768;
            let tv_va = base + 1024;
            let ts_va = base + 1056;
            let ro_page = base + 3 * 4096;

            let test_inode = TestPollableInode::new(PollMask::EMPTY);
            let fd = ktest_install_file(test_inode.open_file(OpenFlags::O_RDWR).map_err(|_| "open test")?)?;
            let fd_num = fd.raw();

            // 1. Delay knob between clearing `fired` and the re-poll: a
            //    spurious wake wakes the waiter, it clears `fired` and pauses
            //    at the knob; a readiness change landing there must still end
            //    the wait with POLLIN.
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            recheck_window::arm();
            let waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, 5_000))?;
            wait_for(|| test_inode.subscriber_count() == 1 && waiter.blocked())?;
            test_inode.notify(PollMask::OUT);
            kassert!(recheck_window::wait_until_paused(crate::time::now_ns() + 5_000_000_000));
            test_inode.set_mask(PollMask::IN | PollMask::RDNORM);
            recheck_window::release();
            let got = waiter.join()?;
            recheck_window::reset();
            kassert_eq!(got, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 2. A change landing right after the re-poll reads EMPTY (while a
            //    timed wait is in progress) still ends with POLLIN.
            fn flip_after_empty_read(inode: &TestPollableInode) {
                inode.set_mask(PollMask::IN | PollMask::RDNORM);
            }
            test_inode.set_mask_quiet(PollMask::EMPTY);
            test_inode.set_after_read_hook(2, flip_after_empty_read);
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            let waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, 500))?;
            wait_for(|| test_inode.subscriber_count() == 1 && waiter.blocked())?;
            test_inode.notify(PollMask::OUT);
            kassert_eq!(waiter.join()?, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 2b. Kills the mutant that clears `fired` AFTER the re-poll: with
            //     timeout = -1 (no deadline final pass to hide a lost wake),
            //     the re-poll reads EMPTY, then sets IN (notifying `fired`)
            //     and raises USR1 on the waiter. Clearing `fired` before the
            //     re-poll keeps `fired` set so `wait_interruptible` checks
            //     `ready()` before signals and returns 1; clearing after wipes
            //     `fired`, so the wait sees only USR1 and returns
            //     `-ERESTART_RESTARTBLOCK`.
            fn flip_then_raise(inode: &TestPollableInode) {
                inode.set_mask(PollMask::IN | PollMask::RDNORM);
                raise_usr1_on_self();
            }
            test_inode.set_mask_quiet(PollMask::EMPTY);
            test_inode.set_after_read_hook(2, flip_then_raise);
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            let waiter = process::spawn_test_thread(move |_| {
                let r = sys_poll(fds_va, 1, u64::MAX);
                sched::clear_current_sigpending();
                let _ = sched::take_restart_block();
                r
            })?;
            wait_for(|| test_inode.subscriber_count() == 1 && waiter.blocked())?;
            test_inode.notify(PollMask::OUT);
            kassert_eq!(waiter.join()?, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::IN.bits());
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

            // 4a. `poll` interrupted -> writes back zeroed `revents`, drops its
            //     PollGuard, returns `-ERESTART_RESTARTBLOCK` with a deadline
            //     in the RestartBlock, and resumes via an in-kernel call to
            //     `restart_syscall()` (ring-3 `sysret` restart is B2's test).
            test_inode.set_mask_quiet(PollMask::EMPTY);
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            let inode_for_resume = Arc::clone(&test_inode);
            let t = process::spawn_test_thread(move |_| {
                let r = sys_poll(fds_va, 1, 5_000);
                let revents_on_int = read_revents(fds_va, 1).ok().map(|v| v[0]);
                let has_block = sched::restart_block().and_then(|b| b.deadline).is_some();
                sched::clear_current_sigpending();
                inode_for_resume.set_mask(PollMask::IN | PollMask::RDNORM);
                let resumed = restart_syscall();
                (r, revents_on_int, has_block, resumed)
            })?;
            wait_for(|| test_inode.subscriber_count() == 1 && t.blocked())?;
            raise_for_test(t.thread(), Signal::USR1);
            let (ret, revents_on_int, has_block, resumed) = t.join()?;
            kassert_eq!(ret, restart_result(RestartCode::RestartBlock));
            kassert_eq!(revents_on_int, Some(0));
            kassert!(has_block);
            kassert_eq!(resumed, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::IN.bits());
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 4a-ro. Interrupted `poll` with read-only `fds` returns `-EFAULT`
            //        (without storing a RestartBlock), as Linux's `do_sys_poll`.
            write_pollfds(ro_page, &[(fd_num, PollMask::IN.bits())])?;
            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ)), 0);
            let ro_res = expect_interrupted(&test_inode, neg_errno(errno::EFAULT), move || sys_poll(ro_page, 1, 5_000));
            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ | general::PROT_WRITE)), 0);
            ro_res?;

            // 4b. `poll(NULL, 0, 5000)` interrupted -> `-ERESTART_RESTARTBLOCK` with deadline.
            let t = process::spawn_test_thread(move |_| {
                let r = sys_poll(0, 0, 5_000);
                let has_block = sched::take_restart_block().and_then(|b| b.deadline).is_some();
                sched::clear_current_sigpending();
                (r, has_block)
            })?;
            wait_for(|| t.blocked())?;
            raise_for_test(t.thread(), Signal::USR1);
            let (ret, has_block) = t.join()?;
            kassert_eq!(ret, restart_result(RestartCode::RestartBlock));
            kassert!(has_block);

            // 4c/4d. `ppoll`, `select`, and `pselect6` interrupted -> `-ERESTARTNOHAND`,
            //        PollGuards dropped.
            let nohand = neg_errno(ERESTARTNOHAND);
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            write_timespec(ts_va, 5, 0)?;
            expect_interrupted(&test_inode, nohand, move || ppoll_at(fds_va, 1, ts_va, 0, 8))?;

            write_fd_set(rset_va, &[fd_num])?;
            write_timeval(tv_va, 5, 0)?;
            expect_interrupted(&test_inode, nohand, move || select_at(fd_num + 1, rset_va, 0, 0, tv_va))?;

            write_fd_set(rset_va, &[fd_num])?;
            write_timespec(ts_va, 5, 0)?;
            expect_interrupted(&test_inode, nohand, move || pselect6_at(fd_num + 1, rset_va, 0, 0, ts_va, 0))?;

            Ok(())
        })?
    }
);

crate::ktest!(
    fn poll_resume_honours_the_saved_deadline() {
        use super::fd::ktest_install_file;
        use crate::kassert_eq;
        use ktest_pollable::{TestPollableInode, raise_usr1_on_self, write_pollfds};
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let fds_va = process::KTEST_USER_VA + 512;
            let test_inode = TestPollableInode::new(PollMask::EMPTY);
            let fd = ktest_install_file(test_inode.open_file(OpenFlags::O_RDWR).map_err(|_| "open test")?)?;
            write_pollfds(fds_va, &[(fd.raw(), PollMask::IN.bits())])?;
            // A restart block whose deadline has passed, a signal pending, the
            // source never ready: `resume_poll` must time out immediately (0)
            // rather than block (which only the signal would end). Kills the
            // mutant that drops the saved deadline into `Forever`.
            let got = process::spawn_test_thread(move |_| {
                raise_usr1_on_self();
                let past = crate::time::now_ns().saturating_sub(1);
                let block = encode_poll_restart(fds_va, 1, WaitTimeout::Deadline(past));
                let r = resume_poll(&block);
                sched::clear_current_sigpending();
                let _ = sched::take_restart_block();
                r
            })?
            .join()?;
            kassert_eq!(got, 0);
            kassert_eq!(test_inode.subscriber_count(), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn ppoll_installs_its_mask_and_writes_back_when_interrupted() {
        use super::fd::ktest_install_file;
        use super::mem::sys_mprotect;
        use super::neg_errno;
        use crate::sync::SpinLock;
        use crate::{kassert, kassert_eq};
        use ktest_pollable::{
            TestPollableInode, ppoll_at, pselect6_at, raise_usr1_on_self, select_at, write_fd_set, write_pollfds,
            write_timespec, write_timeval,
        };
        use lumen_signal::{Blocked, SigSet, Signal};
        use lumen_vfs::OpenFlags;

        static SEEN: SpinLock<Option<Blocked>> = SpinLock::new(None);
        fn record_then_raise(_inode: &TestPollableInode) {
            *SEEN.lock() = Some(process::current_blocked());
            raise_usr1_on_self();
        }

        fn interrupted(
            inode: &Arc<TestPollableInode>,
            initial: Option<Blocked>,
            call: impl FnOnce() -> u64 + Send + 'static,
        ) -> Result<(u64, Option<Blocked>, Blocked, u64), &'static str> {
            inode.set_after_read_hook(1, record_then_raise);
            let (ret, after, elapsed) = process::spawn_test_thread(move |_| {
                if let Some(mask) = initial {
                    process::set_current_blocked(mask);
                }
                let t0 = crate::time::now_ns();
                let r = call();
                let elapsed = crate::time::now_ns() - t0;
                // No queued signal in this fixture: exercise the exit hook's
                // no-handler restoration after the synthetic interruption.
                let mut frame = crate::arch::syscall::SyscallFrame { rax: r, rip: 0x400002, ..Default::default() };
                crate::linux::signal::exit_to_user(&mut frame, lumen_signal::restart::SyscallExit::new(271));
                let after = process::current_blocked();
                sched::clear_current_sigpending();
                (r, after, elapsed)
            })?
            .join()?;
            Ok((ret, SEEN.lock().take(), after, elapsed))
        }

        fn check_rem(rem: u64, asked: u64, elapsed: u64) -> crate::ktest::TestResult {
            kassert!(rem < asked);
            kassert!(rem + elapsed >= asked);
            Ok(())
        }

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let fds_va = base + 512;
            let rset_va = base + 768;
            let ts_va = base + 1056;
            let tv_va = base + 1088;
            let sig_va = base + 1472;
            let mask_va = base + 1504;
            let ro_page = base + 3 * 4096;

            let test_inode = TestPollableInode::new(PollMask::EMPTY);
            let fd = ktest_install_file(test_inode.open_file(OpenFlags::O_RDWR).map_err(|_| "open test")?)?;
            let fd_num = fd.raw();
            let initial = Blocked::new(SigSet::of(&[Signal::USR1]));
            let temp = Blocked::new(SigSet::of(&[Signal::USR2]));
            copy_to_user(mask_va, &temp.set().0.to_le_bytes()).map_err(|_| "write mask")?;
            // SAFETY: `RawPselectSig` has zero padding bytes.
            unsafe { copy_pod_to_user(sig_va, &RawPselectSig { ss: mask_va, ss_len: 8 }) }.map_err(|_| "write sig")?;

            // 1. `ppoll`: installs temporary mask, restores at the no-handler exit hook,
            //    and writes remaining `timespec` back.
            write_pollfds(fds_va, &[(fd_num, PollMask::IN.bits())])?;
            write_timespec(ts_va, 5, 0)?;
            let (ret, seen, after, elapsed) =
                interrupted(&test_inode, Some(initial), move || ppoll_at(fds_va, 1, ts_va, mask_va, 8))?;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            kassert_eq!(seen, Some(temp));
            kassert_eq!(after, initial);
            // SAFETY: `RawTimespec` is two `i64`s; every bit pattern is valid.
            let ts = unsafe { copy_pod_from_user::<RawTimespec>(ts_va) }.map_err(|_| "read ts")?;
            check_rem((ts.tv_sec as u64) * NS_PER_SEC + ts.tv_nsec as u64, 5 * NS_PER_SEC, elapsed)?;
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 2. `pselect6`: installs temporary mask, restores at the no-handler exit hook,
            //    and writes remaining `timespec` back.
            write_fd_set(rset_va, &[fd_num])?;
            write_timespec(ts_va, 5, 0)?;
            let (ret, seen, after, elapsed) =
                interrupted(&test_inode, Some(initial), move || pselect6_at(fd_num + 1, rset_va, 0, 0, ts_va, sig_va))?;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            kassert_eq!(seen, Some(temp));
            kassert_eq!(after, initial);
            // SAFETY: `RawTimespec` is two `i64`s; every bit pattern is valid.
            let ts = unsafe { copy_pod_from_user::<RawTimespec>(ts_va) }.map_err(|_| "read ts")?;
            check_rem((ts.tv_sec as u64) * NS_PER_SEC + ts.tv_nsec as u64, 5 * NS_PER_SEC, elapsed)?;
            kassert_eq!(test_inode.subscriber_count(), 0);

            // 3. `select`: writes remaining `timeval` back on `-ERESTARTNOHAND`.
            write_fd_set(rset_va, &[fd_num])?;
            write_timeval(tv_va, 5, 0)?;
            let (ret, _, _, elapsed) =
                interrupted(&test_inode, None, move || select_at(fd_num + 1, rset_va, 0, 0, tv_va))?;
            kassert_eq!(ret, neg_errno(ERESTARTNOHAND));
            // SAFETY: `RawTimeval` is two `i64`s; every bit pattern is valid.
            let tv = unsafe { copy_pod_from_user::<RawTimeval>(tv_va) }.map_err(|_| "read tv")?;
            check_rem(
                (tv.tv_sec as u64) * US_PER_SEC + tv.tv_usec as u64,
                5 * US_PER_SEC,
                elapsed.div_ceil(NS_PER_US),
            )?;

            // 4. Read-only timeout pointer on `-ERESTARTNOHAND` turns into
            //    `-EINTR` (Linux's `poll_select_finish`), for `ppoll`, `select`,
            //    and `pselect6`.
            write_timespec(ro_page, 5, 0)?;
            write_timeval(ro_page + 32, 5, 0)?;
            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ)), 0);

            let (r_ppoll, _, _, _) = interrupted(&test_inode, None, move || ppoll_at(fds_va, 1, ro_page, 0, 8))?;
            kassert_eq!(r_ppoll, neg_errno(errno::EINTR));

            write_fd_set(rset_va, &[fd_num])?;
            let (r_select, _, _, _) =
                interrupted(&test_inode, None, move || select_at(fd_num + 1, rset_va, 0, 0, ro_page + 32))?;
            kassert_eq!(r_select, neg_errno(errno::EINTR));

            write_fd_set(rset_va, &[fd_num])?;
            let (r_pselect, _, _, _) =
                interrupted(&test_inode, None, move || pselect6_at(fd_num + 1, rset_va, 0, 0, ro_page, 0))?;
            kassert_eq!(r_pselect, neg_errno(errno::EINTR));

            kassert_eq!(sys_mprotect(ro_page, 4096, u64::from(general::PROT_READ | general::PROT_WRITE)), 0);
            kassert_eq!(test_inode.subscriber_count(), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn poll_wakes_on_a_pipe_write() {
        use super::at;
        use crate::kassert_eq;
        use ktest_pollable::{read_revents, wait_for, write_pollfds};

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let pair_va = base + 512;
            let fds_va = base + 544;
            let buf_va = base + 608;

            kassert_eq!(super::pipe::sys_pipe(UserPtr::from_reg(pair_va)), 0);
            // SAFETY: `[i32; 2]` is 8 bytes with no padding.
            let pair = unsafe { copy_pod_from_user::<[i32; 2]>(pair_va) }.map_err(|_| "read pipe fds")?;
            let (r_fd, w_fd) = (pair[0], pair[1]);
            let pipe = process::current_file(r_fd).and_then(|f| f.pipe().cloned()).ok_or("pipe object missing")?;

            // 1. Blocked `poll` on an empty read end wakes when the write end writes.
            write_pollfds(fds_va, &[(r_fd, (PollMask::IN | PollMask::RDNORM).bits())])?;
            let waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, (-1i64) as u64))?;
            wait_for(|| pipe.subscriber_count() == 1 && waiter.blocked())?;

            copy_to_user(buf_va, b"wake").map_err(|_| "write payload")?;
            kassert_eq!(super::fd::sys_write(at::reg(w_fd as u64), UserPtr::from_reg(buf_va), 4), 4);
            kassert_eq!(waiter.join()?, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], (PollMask::IN | PollMask::RDNORM).bits());
            kassert_eq!(pipe.subscriber_count(), 0);

            // Drain so the pipe is empty again.
            kassert_eq!(super::fd::sys_read(at::reg(r_fd as u64), UserPtr::from_reg(buf_va), 4), 4);

            // 2. Blocked `poll` on an empty read end wakes with `POLLHUP` when
            //    the last write end closes.
            write_pollfds(fds_va, &[(r_fd, PollMask::IN.bits())])?;
            let hup_waiter = process::spawn_test_thread(move |_| sys_poll(fds_va, 1, (-1i64) as u64))?;
            wait_for(|| pipe.subscriber_count() == 1 && hup_waiter.blocked())?;

            kassert_eq!(super::fd::sys_close(at::reg(w_fd as u64)), 0);
            kassert_eq!(hup_waiter.join()?, 1);
            kassert_eq!(read_revents(fds_va, 1)?[0], PollMask::HUP.bits());
            kassert_eq!(pipe.subscriber_count(), 0);

            kassert_eq!(super::fd::sys_close(at::reg(r_fd as u64)), 0);
            Ok(())
        })?
    }
);
