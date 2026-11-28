//! Linux `eventfd` and `timerfd` kernel objects (M9 B6, #477; ARCHITECTURE
//! sections 8, 9 and 10).
//!
//! Wraps `lumen-eventfd`'s `no_std` state machines ([`EventCounter`] and
//! [`TimerCount`]) behind anonymous inodes (`anon_inode:[eventfd]` and
//! `anon_inode:[timerfd]`), with kernel wait queues, readiness
//! ([`KernelPollHead`]), and M8's `TIMERS` worker for timerfd expiries.
//!
//! Lock ordering (ARCHITECTURE section 9, level 10c):
//! - `EventFd::counter` is a level-10c leaf `SpinLock` (never held across user
//!   copies, blocking waits, or `PollHead::notify` / `WaitQueue::wake_all`).
//! - `TimerFd::count` is a level-10c `SpinLock` that nests only `TIMERS` (also
//!   10c) when applying a [`Requeue`] via [`TimerFd::apply`]. Neither lock is
//!   held across `timer::wake_worker`, `WaitQueue::wake_all`, or
//!   `PollHead::notify`. The `timers` worker moves expired entries out of
//!   `TIMERS` before taking `TimerFd::count`, and wakes readers / notifies
//!   `PollHead` only after releasing `TimerFd::count`.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use lumen_eventfd::{EventCounter, IO_SIZE, ITimer, Requeue, TimerClock, TimerCount, TimerfdSettimeFlags, WriteError};
use lumen_vfs::{AccessMode, PollGuard, PollMask, PollWaker, Pollable};

use crate::linux::signal::ERESTARTSYS;
use crate::linux::{errno, timer};
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::time;
use crate::user::copy_from_user;
use crate::vfs::KernelPollHead;

/// Monotonic instance counter for `TimerFd` entries in `TIMERS`, keyed
/// `(deadline, TIMERFD_OWNER, instance)`.
static NEXT_TIMERFD_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// Block interruptibly on `wq` until `seq` moves past `seen`, returning
/// `-ERESTARTSYS` on signal interruption.
fn wait_seq(wq: &WaitQueue, seq: &AtomicU64, seen: u64) -> Result<(), u32> {
    if wq.wait_interruptible(None, || seq.load(Ordering::Acquire) != seen) == sched::Wake::Interrupted {
        Err(ERESTARTSYS)
    } else {
        Ok(())
    }
}

/// Kernel object backing an `anon_inode:[eventfd]` description (`fs/eventfd.c`).
pub struct EventFd {
    /// Level-10c leaf lock protecting the counter state machine.
    counter: SpinLock<EventCounter>,
    /// Bumped under `counter` whenever a non-zero write increases the count,
    /// paired with [`Self::readers`].
    readers_seq: AtomicU64,
    /// Bumped under `counter` whenever a read lowers the count, paired with
    /// [`Self::writers`].
    writers_seq: AtomicU64,
    readers: WaitQueue,
    writers: WaitQueue,
    poll: KernelPollHead,
}

impl EventFd {
    /// Create a new `EventFd` with initial value `initval` and optional
    /// `EFD_SEMAPHORE` mode.
    pub fn new(initval: u32, semaphore: bool) -> Arc<Self> {
        Arc::new(Self {
            counter: SpinLock::new(EventCounter::new(initval, semaphore)),
            readers_seq: AtomicU64::new(0),
            writers_seq: AtomicU64::new(0),
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            poll: KernelPollHead::new(),
        })
    }

    /// Consume one value (whole counter, or `1` in semaphore mode), blocking
    /// interruptibly (`-ERESTARTSYS`) unless `nonblock` is set (`-EAGAIN`).
    ///
    /// Wakes blocked writers and notifies pollers with `POLLOUT` after
    /// releasing the counter lock.
    pub fn read_u64(&self, nonblock: bool) -> Result<u64, u32> {
        loop {
            let seen = {
                let mut counter = self.counter.lock();
                match counter.read() {
                    Ok(val) => {
                        self.writers_seq.fetch_add(1, Ordering::Release);
                        drop(counter);
                        self.writers.wake_all();
                        self.poll.notify(PollMask::OUT);
                        return Ok(val);
                    }
                    Err(_) => {
                        if nonblock {
                            return Err(errno::EAGAIN);
                        }
                        self.readers_seq.load(Ordering::Acquire)
                    }
                }
            };
            wait_seq(&self.readers, &self.readers_seq, seen)?;
        }
    }

    /// `write(2)` on an eventfd (Linux v6.12 `fs/eventfd.c` `eventfd_write`):
    /// buffers shorter than 8 bytes or value `u64::MAX` return `-EINVAL`;
    /// copies the `u64` from user memory before taking the counter lock;
    /// overflow blocks interruptibly (`-ERESTARTSYS`) or returns `-EAGAIN`
    /// when non-blocking.
    pub fn write(&self, user_buf: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        if count < IO_SIZE as u64 {
            return Err(errno::EINVAL);
        }
        let mut raw = [0u8; IO_SIZE];
        copy_from_user(&mut raw, user_buf).map_err(|_| errno::EFAULT)?;
        let add = u64::from_ne_bytes(raw);
        if add == u64::MAX {
            return Err(errno::EINVAL);
        }
        loop {
            let seen = {
                let mut counter = self.counter.lock();
                match counter.write(add) {
                    Ok(()) => {
                        if add > 0 {
                            self.readers_seq.fetch_add(1, Ordering::Release);
                        }
                        drop(counter);
                        if add > 0 {
                            self.readers.wake_all();
                            self.poll.notify(PollMask::IN);
                        }
                        return Ok(IO_SIZE as u64);
                    }
                    Err(WriteError::Invalid) => return Err(errno::EINVAL),
                    Err(WriteError::WouldBlock) => {
                        if nonblock {
                            return Err(errno::EAGAIN);
                        }
                        self.writers_seq.load(Ordering::Acquire)
                    }
                }
            };
            wait_seq(&self.writers, &self.writers_seq, seen)?;
        }
    }
}

impl Pollable for EventFd {
    fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        // Subscribe before reading state under `counter` so no wake is lost.
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let counter = self.counter.lock();
        let mut mask = PollMask::EMPTY;
        if counter.readable() {
            mask |= PollMask::IN;
        }
        if counter.writable() {
            mask |= PollMask::OUT;
        }
        (mask, guard)
    }
}

/// Kernel object backing an `anon_inode:[timerfd]` description (`fs/timerfd.c`).
pub struct TimerFd {
    /// Level-10c lock protecting the timer state; nests only `TIMERS` (10c).
    count: SpinLock<TimerCount>,
    clock: TimerClock,
    /// Bumped under `count` whenever the worker accounts new expirations.
    seq: AtomicU64,
    wq: WaitQueue,
    poll: KernelPollHead,
    /// Unique per-object key component in `TIMERS` `(deadline, TIMERFD_OWNER,
    /// instance)`. Safe to keep constant across re-arms because
    /// [`Requeue::remove`] always carries the exact previously queued deadline,
    /// never a recomputed one.
    instance: u64,
}

impl TimerFd {
    /// Create a new disarmed `TimerFd` on `clock`.
    pub fn new(clock: TimerClock) -> Arc<Self> {
        let instance = NEXT_TIMERFD_INSTANCE.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            count: SpinLock::new(TimerCount::new()),
            clock,
            seq: AtomicU64::new(0),
            wq: WaitQueue::new(),
            poll: KernelPollHead::new(),
            instance,
        })
    }

    /// Apply a `TimerCount` [`Requeue`] to `TIMERS` while the caller holds
    /// `self.count`. Returns whether the caller must wake the `timers` worker
    /// after releasing `self.count`.
    #[must_use = "wake the timers worker after releasing self.count when true"]
    fn apply(self: &Arc<Self>, requeue: Requeue) -> bool {
        timer::requeue_timerfd(&Arc::downgrade(self), self.instance, requeue)
    }

    /// Arm, re-arm, or disarm this timer (`timerfd_settime(2)`), applying the
    /// returned [`Requeue`] to `TIMERS` under `self.count` and waking the
    /// worker outside the lock if the new deadline is already due.
    ///
    /// Samples `CLOCK_REALTIME` before taking `self.count` so `REALTIME_ANCHOR`
    /// never nests inside the counter lock.
    pub fn settime(self: &Arc<Self>, flags: TimerfdSettimeFlags, new: ITimer) -> ITimer {
        let rt_sample = (self.clock == TimerClock::Realtime && flags.abstime).then(|| {
            let (sec, nsec) = time::realtime_sec_nsec();
            let mono = time::now_ns();
            let rt = sec.saturating_mul(1_000_000_000).saturating_add(nsec);
            (mono, rt)
        });
        let (old, wake) = {
            let mut count = self.count.lock();
            let now_mono = time::now_ns();
            let now_rt = rt_sample
                .map_or(0, |(sample_mono, sample_rt)| sample_rt.saturating_add(now_mono.saturating_sub(sample_mono)));
            let (old, requeue) = count.settime(now_mono, now_rt, self.clock, flags, new);
            (old, self.apply(requeue))
        };
        if wake {
            timer::wake_worker();
        }
        old
    }

    /// Current remaining time and repeat interval (`timerfd_gettime(2)`).
    ///
    /// Deviation from Linux v6.12 `fs/timerfd.c` `do_timerfd_gettime`: projects
    /// remaining time via [`TimerCount::get`] without mutating or forwarding
    /// buffered `ticks` on an expired interval timer (pending expiries are
    /// still accounted on `on_expiry` and `read`).
    pub fn gettime(&self) -> ITimer {
        let count = self.count.lock();
        count.get(time::now_ns())
    }

    /// Consume pending expirations, accounting any already due at `now` and
    /// re-queuing an interval timer's next deadline in `TIMERS`.
    pub fn read_u64(self: &Arc<Self>, nonblock: bool) -> Result<u64, u32> {
        loop {
            let seen = {
                let mut count = self.count.lock();
                match count.read(time::now_ns()) {
                    Ok((expiries, requeue)) => {
                        let wake = self.apply(requeue);
                        drop(count);
                        if wake {
                            timer::wake_worker();
                        }
                        return Ok(expiries);
                    }
                    Err(_) => {
                        if nonblock {
                            return Err(errno::EAGAIN);
                        }
                        self.seq.load(Ordering::Acquire)
                    }
                }
            };
            wait_seq(&self.wq, &self.seq, seen)?;
        }
    }

    /// Called by `linux::timer`'s worker thread after popping an expired entry
    /// out of `TIMERS` (outside every lock).
    pub(crate) fn expire(self: &Arc<Self>, now: u64) {
        let (added, wake) = {
            let mut count = self.count.lock();
            let (added, requeue) = count.on_expiry(now);
            let wake = self.apply(requeue);
            if added > 0 {
                self.seq.fetch_add(1, Ordering::Release);
            }
            (added, wake)
        };
        if wake {
            timer::wake_worker();
        }
        if added > 0 {
            self.wq.wake_all();
            self.poll.notify(PollMask::IN);
        }
    }

    /// Disarm on teardown (`AnonObj::release`), idempotently removing any
    /// queued deadline from `TIMERS`.
    pub fn disarm(self: &Arc<Self>) {
        let wake = {
            let mut count = self.count.lock();
            let requeue = count.disarm();
            self.apply(requeue)
        };
        debug_assert!(!wake, "disarm never inserts a deadline");
    }

    #[cfg(feature = "ktest")]
    pub(crate) fn instance(&self) -> u64 {
        self.instance
    }

    #[cfg(feature = "ktest")]
    pub(crate) fn ticks_snapshot(&self) -> u64 {
        self.count.lock().ticks()
    }
}

impl Pollable for TimerFd {
    fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        // Subscribe before reading state under `count` so no worker expiry wake is lost.
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let count = self.count.lock();
        let mask = if count.readable(time::now_ns()) { PollMask::IN } else { PollMask::EMPTY };
        (mask, guard)
    }
}

crate::ktest!(
    fn timerfd_expiries_count_on_the_timers_worker() {
        use crate::{kassert, kassert_eq};
        const MS: u64 = 1_000_000;
        const HOUR: u64 = 3_600_000 * MS;

        let tfd = TimerFd::new(TimerClock::Monotonic);
        kassert!(!timer::has_timerfd_entry(tfd.instance()));
        kassert_eq!(tfd.read_u64(true), Err(errno::EAGAIN));

        // Deterministic presence checks using a 1 h interval so the worker
        // cannot race-pop the entry between `settime`/`read_u64` and `TIMERS`
        // inspection:
        // 1. Arming inserts an entry.
        let old = tfd.settime(TimerfdSettimeFlags::default(), ITimer::new(HOUR, HOUR));
        kassert_eq!(old, ITimer::new(0, 0));
        kassert!(timer::has_timerfd_entry(tfd.instance()));

        // 2. Reading an expired periodic timer re-enters the next wakeup ~1 h
        //    ahead in `TIMERS`.
        let abs = TimerfdSettimeFlags { abstime: true, cancel_on_set: false };
        let _ = tfd.settime(abs, ITimer::new(1, HOUR));
        let past_count = tfd.read_u64(true).map_err(|_| "read past-due 1h periodic")?;
        kassert!(past_count >= 1);
        kassert!(timer::has_timerfd_entry(tfd.instance()));

        // 3. Disarming via `settime(0)` removes the entry; sleeping produces
        //    no expiries and non-blocking read returns `-EAGAIN`.
        let prev = tfd.settime(TimerfdSettimeFlags::default(), ITimer::new(0, 0));
        kassert_eq!(prev.interval, HOUR);
        kassert!(!timer::has_timerfd_entry(tfd.instance()));
        kassert!(sched::sleep(30 * MS));
        kassert_eq!(tfd.ticks_snapshot(), 0);
        kassert_eq!(tfd.poll(lumen_vfs::AccessMode::ReadWrite, None).0, PollMask::EMPTY);
        kassert_eq!(tfd.read_u64(true), Err(errno::EAGAIN));

        // 4. Arm a 10 ms periodic timer for the worker delivery + count floor
        //    check (no racy `TIMERS` presence check on the short interval).
        let interval = 10 * MS;
        let _ = tfd.settime(TimerfdSettimeFlags::default(), ITimer::new(interval, interval));
        let start = time::now_ns();
        kassert!(sched::sleep(45 * MS));
        // Confirm the background `timers` worker actually delivered at least one
        // expiry (not just `TimerCount::read`'s own overdue forwarding). No
        // artificial deadline: `test_ktest`'s own timeout catches a hang.
        while tfd.ticks_snapshot() == 0 {
            kassert!(sched::sleep(5 * MS));
        }
        kassert!(tfd.ticks_snapshot() >= 1);
        kassert!(time::now_ns().saturating_sub(start) >= 3 * interval);

        let count = tfd.read_u64(true).map_err(|_| "read after three intervals")?;
        kassert!(count >= 3);

        // 5. Disarm after periodic run reports nothing; teardown `disarm()`
        //    removes a 1 h entry idempotently.
        let prev = tfd.settime(TimerfdSettimeFlags::default(), ITimer::new(0, 0));
        kassert_eq!(prev.interval, interval);
        kassert!(!timer::has_timerfd_entry(tfd.instance()));
        kassert!(sched::sleep(30 * MS));
        kassert_eq!(tfd.ticks_snapshot(), 0);
        kassert_eq!(tfd.read_u64(true), Err(errno::EAGAIN));

        let _ = tfd.settime(TimerfdSettimeFlags::default(), ITimer::new(HOUR, 0));
        kassert!(timer::has_timerfd_entry(tfd.instance()));
        tfd.disarm();
        kassert!(!timer::has_timerfd_entry(tfd.instance()));
        tfd.disarm();
        kassert!(!timer::has_timerfd_entry(tfd.instance()));
        Ok(())
    }
);

crate::ktest!(
    fn eventfd_and_timerfd_syscall_edges_and_faults() {
        use crate::arch::syscall::SyscallFrame;
        use crate::linux::process::{self, AnonObj, KTEST_USER_VA as A};
        use crate::linux::{general, syscall};
        use crate::user::copy_to_user;
        use crate::{kassert, kassert_eq};
        use lumen_eventfd::{
            CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_REALTIME, CLOCK_REALTIME_ALARM, EFD_CLOEXEC,
            EFD_NONBLOCK, EFD_SEMAPHORE, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME, TFD_TIMER_CANCEL_ON_SET,
        };

        fn call(nr: u32, a: [u64; 6]) -> u64 {
            let mut frame = SyscallFrame {
                rax: u64::from(nr),
                rdi: a[0],
                rsi: a[1],
                rdx: a[2],
                r10: a[3],
                r8: a[4],
                r9: a[5],
                ..SyscallFrame::default()
            };
            let _ = syscall::dispatch(&mut frame);
            frame.rax
        }
        let neg = |e: u32| -(e as i64) as u64;
        let put_iov = |addr: u64, spans: &[(u64, u64)]| -> Result<(), &'static str> {
            let mut buf = [0u8; 48];
            for (i, &(base, len)) in spans.iter().enumerate() {
                let off = i * 16;
                buf[off..off + 8].copy_from_slice(&base.to_le_bytes());
                buf[off + 8..off + 16].copy_from_slice(&len.to_le_bytes());
            }
            copy_to_user(addr, &buf[..spans.len() * 16]).map_err(|_| "copy iov")
        };

        process::in_test_process(move |_| {
            // eventfd2 invalid flags -> -EINVAL.
            kassert_eq!(call(general::__NR_eventfd2, [0, 0x100, 0, 0, 0, 0]), neg(errno::EINVAL));

            // eventfd read fault after taking value returns -EFAULT with value consumed.
            let efd = call(general::__NR_eventfd2, [7, u64::from(EFD_NONBLOCK | EFD_CLOEXEC), 0, 0, 0, 0]);
            kassert!((efd as i64) >= 0);
            // Short read/write (< 8 bytes) -> -EINVAL (including 0 bytes).
            for len in [0u64, 1, 7] {
                kassert_eq!(call(general::__NR_read, [efd, A, len, 0, 0, 0]), neg(errno::EINVAL));
                kassert_eq!(call(general::__NR_write, [efd, A, len, 0, 0, 0]), neg(errno::EINVAL));
            }
            // Write u64::MAX -> -EINVAL; bad pointer -> -EFAULT.
            copy_to_user(A, &u64::MAX.to_ne_bytes()).map_err(|_| "copy max")?;
            kassert_eq!(call(general::__NR_write, [efd, A, 8, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(call(general::__NR_write, [efd, 0, 8, 0, 0, 0]), neg(errno::EFAULT));
            // Faulting read into NULL consumes the 7 and returns -EFAULT.
            kassert_eq!(call(general::__NR_read, [efd, 0, 8, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(call(general::__NR_read, [efd, A, 8, 0, 0, 0]), neg(errno::EAGAIN));

            // Linux v6.12 `fs/read_write.c` `vfs_writev` / `do_loop_readv_writev`:
            // total length 0 returns 0; a leading zero-length iovec before a
            // non-empty one passes len=0 to `eventfd_write` -> `-EINVAL`; a
            // mid-array zero-length iovec after a write is skipped.
            copy_to_user(A, &3u64.to_ne_bytes()).map_err(|_| "copy 3")?;
            copy_to_user(A + 8, &4u64.to_ne_bytes()).map_err(|_| "copy 4")?;
            put_iov(A + 16, &[(A, 0)])?;
            kassert_eq!(call(general::__NR_writev, [efd, A + 16, 1, 0, 0, 0]), 0);
            put_iov(A + 16, &[(A, 0), (A, 8)])?;
            kassert_eq!(call(general::__NR_writev, [efd, A + 16, 2, 0, 0, 0]), neg(errno::EINVAL));
            put_iov(A + 16, &[(A, 8), (A, 0), (A + 8, 8)])?;
            kassert_eq!(call(general::__NR_writev, [efd, A + 16, 3, 0, 0, 0]), 16);
            kassert_eq!(call(general::__NR_read, [efd, A, 8, 0, 0, 0]), 8);
            let mut sum = [0u8; 8];
            copy_from_user(&mut sum, A).map_err(|_| "read writev sum")?;
            kassert_eq!(u64::from_ne_bytes(sum), 7);

            // Split readv across two 4-byte iovecs reads one 8-byte u64; a
            // partial fault on eventfd (`fs/eventfd.c:242`) returns -EFAULT
            // with the counter consumed.
            copy_to_user(A, &0x0102_0304_0506_0708u64.to_ne_bytes()).map_err(|_| "copy val")?;
            kassert_eq!(call(general::__NR_write, [efd, A, 8, 0, 0, 0]), 8);
            put_iov(A + 16, &[(A + 64, 4), (A + 128, 4)])?;
            kassert_eq!(call(general::__NR_readv, [efd, A + 16, 2, 0, 0, 0]), 8);
            let mut lo = [0u8; 4];
            let mut hi = [0u8; 4];
            copy_from_user(&mut lo, A + 64).map_err(|_| "read lo")?;
            copy_from_user(&mut hi, A + 128).map_err(|_| "read hi")?;
            let mut both = [0u8; 8];
            both[..4].copy_from_slice(&lo);
            both[4..].copy_from_slice(&hi);
            kassert_eq!(u64::from_ne_bytes(both), 0x0102_0304_0506_0708);

            kassert_eq!(call(general::__NR_write, [efd, A, 8, 0, 0, 0]), 8);
            put_iov(A + 16, &[(A + 64, 4), (0, 4)])?;
            kassert_eq!(call(general::__NR_readv, [efd, A + 16, 2, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(call(general::__NR_read, [efd, A, 8, 0, 0, 0]), neg(errno::EAGAIN));
            kassert_eq!(call(general::__NR_close, [efd, 0, 0, 0, 0, 0]), 0);

            // Semaphore mode read fault consumes 1 and leaves remaining count.
            let sfd =
                call(general::__NR_eventfd2, [2, u64::from(EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC), 0, 0, 0, 0]);
            kassert!((sfd as i64) >= 0);
            kassert_eq!(call(general::__NR_read, [sfd, 0, 8, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(call(general::__NR_read, [sfd, A, 8, 0, 0, 0]), 8);
            let mut got = [0u8; 8];
            copy_from_user(&mut got, A).map_err(|_| "read sem")?;
            kassert_eq!(u64::from_ne_bytes(got), 1);
            kassert_eq!(call(general::__NR_read, [sfd, A, 8, 0, 0, 0]), neg(errno::EAGAIN));
            kassert_eq!(call(general::__NR_close, [sfd, 0, 0, 0, 0, 0]), 0);

            // timerfd_create clock and flag error ordering (Linux v6.12
            // `fs/timerfd.c`): invalid flags (-EINVAL) before alarm clocks
            // (always -EPERM: Lumen models no CAP_WAKE_ALARM).
            kassert_eq!(
                call(general::__NR_timerfd_create, [CLOCK_REALTIME_ALARM as u64, 0x100, 0, 0, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                call(general::__NR_timerfd_create, [CLOCK_REALTIME_ALARM as u64, 0, 0, 0, 0, 0]),
                neg(errno::EPERM)
            );
            kassert_eq!(
                call(general::__NR_timerfd_create, [CLOCK_BOOTTIME_ALARM as u64, 0, 0, 0, 0, 0]),
                neg(errno::EPERM)
            );
            kassert_eq!(call(general::__NR_timerfd_create, [2, 0, 0, 0, 0, 0]), neg(errno::EINVAL));

            // Non-timerfd fd -> -EINVAL on settime/gettime.
            copy_to_user(A, &[0u8; 32]).map_err(|_| "zero itimer")?;
            kassert_eq!(call(general::__NR_timerfd_gettime, [0, A, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(call(general::__NR_timerfd_settime, [0, 0, A, 0, 0, 0]), neg(errno::EINVAL));

            // Supported clocks succeed, reject writes before `import_iovec`,
            // and accept `TFD_TIMER_CANCEL_ON_SET` as a no-op.
            put_iov(A + 16, &[(A, 8)])?;
            for clk in [CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME] {
                let tfd =
                    call(general::__NR_timerfd_create, [clk as u64, u64::from(TFD_NONBLOCK | TFD_CLOEXEC), 0, 0, 0, 0]);
                kassert!((tfd as i64) >= 0);
                // Linux v6.12 `fs/read_write.c` `vfs_writev` checks
                // `FMODE_CAN_WRITE` (`-EINVAL`) before `import_iovec`, so both
                // a valid iov and a faulting NULL iov return `-EINVAL`.
                kassert_eq!(call(general::__NR_write, [tfd, A, 8, 0, 0, 0]), neg(errno::EINVAL));
                kassert_eq!(call(general::__NR_writev, [tfd, A + 16, 1, 0, 0, 0]), neg(errno::EINVAL));
                kassert_eq!(call(general::__NR_writev, [tfd, 0, 1, 0, 0, 0]), neg(errno::EINVAL));
                // NULL new in timerfd_settime -> -EFAULT (get_itimerspec64).
                kassert_eq!(call(general::__NR_timerfd_settime, [tfd, 0, 0, 0, 0, 0]), neg(errno::EFAULT));
                // TFD_TIMER_CANCEL_ON_SET accepted as a no-op.
                copy_to_user(A, &[0u8; 32]).map_err(|_| "zero itimer")?;
                kassert_eq!(
                    call(general::__NR_timerfd_settime, [tfd, u64::from(TFD_TIMER_CANCEL_ON_SET), A, 0, 0, 0]),
                    0
                );
                kassert_eq!(call(general::__NR_close, [tfd, 0, 0, 0, 0, 0]), 0);
            }

            // Linux v6.12 `fs/timerfd.c:292` `timerfd_read_iter`: partial
            // copy-out returns the bytes copied (4) with ticks consumed; 0
            // bytes copied returns `-EFAULT` with ticks consumed.
            let ptfd = call(
                general::__NR_timerfd_create,
                [CLOCK_MONOTONIC as u64, u64::from(TFD_NONBLOCK | TFD_CLOEXEC), 0, 0, 0, 0],
            );
            kassert!((ptfd as i64) >= 0);
            let mut past_one = [0u8; 32];
            past_one[24..32].copy_from_slice(&1i64.to_le_bytes()); // it_value = {0, 1 ns} (already past)
            copy_to_user(A, &past_one).map_err(|_| "copy past itimer")?;
            kassert_eq!(call(general::__NR_timerfd_settime, [ptfd, u64::from(TFD_TIMER_ABSTIME), A, 0, 0, 0]), 0);
            put_iov(A + 32, &[(A + 64, 4), (0, 4)])?;
            kassert_eq!(call(general::__NR_readv, [ptfd, A + 32, 2, 0, 0, 0]), 4);
            let mut head = [0u8; 4];
            copy_from_user(&mut head, A + 64).map_err(|_| "read partial timerfd")?;
            kassert_eq!(head, 1u64.to_ne_bytes()[..4]);
            kassert_eq!(call(general::__NR_read, [ptfd, A + 64, 8, 0, 0, 0]), neg(errno::EAGAIN));
            kassert_eq!(call(general::__NR_timerfd_settime, [ptfd, u64::from(TFD_TIMER_ABSTIME), A, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_read, [ptfd, 0, 8, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(call(general::__NR_read, [ptfd, A + 64, 8, 0, 0, 0]), neg(errno::EAGAIN));
            kassert_eq!(call(general::__NR_close, [ptfd, 0, 0, 0, 0, 0]), 0);

            // Closing an armed timerfd through close(2) disarms and removes its
            // entry from `TIMERS`.
            let armed = call(
                general::__NR_timerfd_create,
                [CLOCK_MONOTONIC as u64, u64::from(TFD_NONBLOCK | TFD_CLOEXEC), 0, 0, 0, 0],
            );
            kassert!((armed as i64) >= 0);
            let instance = {
                let file = process::current_io_file(armed as i32).ok_or("armed timerfd missing")?;
                let Some(AnonObj::TimerFd(tf)) = file.anon() else {
                    return Err("expected TimerFd");
                };
                tf.instance()
            };
            let mut one_hour = [0u8; 32];
            one_hour[16..24].copy_from_slice(&3600i64.to_le_bytes());
            copy_to_user(A, &one_hour).map_err(|_| "copy 1h itimer")?;
            kassert_eq!(call(general::__NR_timerfd_settime, [armed, 0, A, 0, 0, 0]), 0);
            kassert!(timer::has_timerfd_entry(instance));
            kassert_eq!(call(general::__NR_close, [armed, 0, 0, 0, 0, 0]), 0);
            kassert!(!timer::has_timerfd_entry(instance));
            Ok(())
        })?
    }
);
