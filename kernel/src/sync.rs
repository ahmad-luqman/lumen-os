//! Minimal synchronization primitives.
//!
//! [`SpinLock`] turns this CPU's interrupts off while it is held (nested, see
//! `arch::irq`), so a maskable interrupt's handler that takes a lock can
//! never spin on one the code it interrupted holds; other CPUs spin until it
//! is released (interrupt nesting is per CPU). A guard can't be sent to
//! another thread (it holds this CPU's critical section).
//!
//! The lock word is the holder: free, or the CPU holding it. A CPU that
//! takes a lock it already holds would spin forever with interrupts off (a
//! silent hang); [`SpinLock::lock`] panics instead, so the crash report
//! names the lock, the CPU and the second caller. That is always a
//! deadlock, never another thread waiting its turn: no thread switches
//! while a lock is held (`sched::schedule` asserts that no `push_off`
//! section is open, and every guard holds one), so the holder is
//! this CPU's own code, or the code an exception or NMI interrupted, and
//! neither runs again until the second `lock` returns.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;
use crate::sched::WaitQueue;

/// A spinlock protecting a `T`, whose word is its owner (a compare-and-swap
/// takes it). Interrupts are off while it is held (and while waiting for it).
pub struct SpinLock<T> {
    /// [`FREE`], or the holder's [`owner_id`]. Only the holder stores
    /// anything but `FREE` here, and a CPU never reads a value older than
    /// its own last store (coherence), so it reads its own id exactly while
    /// it holds the lock, even with `Relaxed` loads.
    owner: AtomicU32,
    /// Contended [`lock`](Self::lock) calls that have recorded their wait's
    /// start TSC and entered the spin loop (ktest seam for deterministic
    /// lock-wait tests).
    #[cfg(feature = "ktest")]
    waits_started: AtomicU32,
    value: UnsafeCell<T>,
}

/// [`SpinLock::owner`] of a free lock (0: a zeroed lock is free).
const FREE: u32 = 0;

// SAFETY: access to `value` is serialized by `owner`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the value; `T: Send` makes that sound.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            owner: AtomicU32::new(FREE),
            #[cfg(feature = "ktest")]
            waits_started: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Take the lock, spinning while another CPU holds it.
    ///
    /// # Panics
    /// If this CPU already holds it (it would wait for itself forever), or
    /// leaked a guard of it. The panic names the lock's address and type
    /// and this CPU, and its location is the caller's (`track_caller`).
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Interrupts off first: the lock is never held with them on (no
        // handler here can take it from under us), and nothing switches this
        // thread between `owner_id()` and the compare-and-swap.
        let irq = arch::irq::push_off();
        let me = owner_id();
        // Looks that found the lock held elsewhere, and the TSC at the first.
        let mut spins = 0u64;
        let mut since = 0;
        loop {
            match self.owner.compare_exchange_weak(FREE, me, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => {
                    if spins > 0 {
                        self.waited(spins, since);
                    }
                    return SpinLockGuard { lock: self, _irq: irq };
                }
                Err(owner) if owner == me => relocked(core::ptr::from_ref(self).addr(), core::any::type_name::<T>()),
                // Held elsewhere (or a spurious failure, `FREE`): wait until
                // it looks free. The failed exchange's own look counts.
                Err(mut owner) => {
                    while owner != FREE {
                        if spins == 0 {
                            since = arch::rdtsc();
                            #[cfg(feature = "ktest")]
                            self.waits_started.fetch_add(1, Ordering::Release);
                        }
                        spins += 1;
                        core::hint::spin_loop();
                        owner = self.owner.load(Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Record that this lock waited (`spins` looks found it held, from TSC
    /// `since`) in this CPU's X-ray ring: its address and the caller's line
    /// (`#[track_caller]` through `lock`; the file isn't recorded). Out of
    /// line, so the uncontended `lock` stays small. Called holding the lock,
    /// with interrupts off: it takes no lock (recording is lock-free),
    /// allocates nothing and logs nothing, so it can't come back here.
    #[cold]
    #[inline(never)]
    #[track_caller]
    fn waited(&self, spins: u64, since: u64) {
        crate::trace::record_lock_wait(lumen_trace::kind::LockWait {
            lock: core::ptr::from_ref(self).addr() as u64,
            spins,
            waited: arch::rdtsc().saturating_sub(since),
            line: core::panic::Location::caller().line(),
        });
    }

    /// Take the lock if it is free; `None` if any CPU holds it (this one
    /// included).
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let irq = arch::irq::push_off();
        if self.owner.compare_exchange(FREE, owner_id(), Ordering::Acquire, Ordering::Relaxed).is_err() {
            // No guard: dropping one would unlock the holder's lock (an
            // eager `then_some` did). Dropping `irq` closes the section.
            return None;
        }
        Some(SpinLockGuard { lock: self, _irq: irq })
    }

    /// The value, through exclusive access to the lock (`Drop`, say): no
    /// one can hold it, so nothing is taken.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Whether this CPU holds the lock (for the ktests).
    #[cfg(feature = "ktest")]
    fn held_here(&self) -> bool {
        let _irq = arch::irq::push_off();
        self.owner.load(Ordering::Relaxed) == owner_id()
    }

    /// How many contended [`lock`](Self::lock) calls have recorded their
    /// wait's start TSC and entered the spin loop (for the ktests).
    #[cfg(feature = "ktest")]
    pub(crate) fn waits_started(&self) -> u32 {
        self.waits_started.load(Ordering::Acquire)
    }

    /// Access the value even if the lock is held.
    ///
    /// # Safety
    /// For the crash reporter, which must print even when the crash happened
    /// while the lock was held, and for tests that hold the lock without
    /// touching the value. The caller guarantees no other code uses the value
    /// while the reference lives (on the crash path: interrupts off for good,
    /// and the interrupted holder never resumes; a CPU the report names as
    /// not stopped is the one exception, and the crash path only reads or
    /// draws through it).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn force_get(&self) -> &mut T {
        // SAFETY: upheld by the caller (see above).
        unsafe { &mut *self.value.get() }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// Dropped after `drop` releases the lock: interrupts come back on (if
    /// they were on) only once it is free.
    _irq: arch::irq::IrqOff,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(FREE, Ordering::Release);
    }
}

/// This CPU's id in [`SpinLock::owner`]: its index + 1 (never [`FREE`]).
fn owner_id() -> u32 {
    const _: () = assert!(arch::MAX_CPUS < u32::MAX as usize);
    arch::cpu_index() as u32 + 1
}

/// The panic of a CPU taking a lock it already holds (out of line: `lock`
/// stays small; interrupts are still off, so this is the holding CPU).
/// Takes no lock, and the crash reporter never waits for one (it reads
/// locked state through `try_lock`, falling back to `force_get`), so
/// re-locking any lock (the heap's, the log's, the page tables') still
/// gets a full report.
#[cold]
#[inline(never)]
#[track_caller]
fn relocked(addr: usize, ty: &str) -> ! {
    let cpu = arch::cpu_index();
    panic!("SpinLock<{ty}> at {addr:#x} re-locked on cpu {cpu}, which already holds it")
}

/// A sleeping, interrupt-enabled mutex protecting a `T`.
///
/// Unlike [`SpinLock`], holding a [`SleepMutex`] keeps interrupts enabled so
/// callers may block (e.g. demand-paging I/O or cross-CPU shootdowns) and
/// be preempted by the timer without holding an IRQ-off critical section.
/// Contended waiters sleep on an internal [`WaitQueue`].
///
/// Lock order: never acquire a [`SleepMutex`] while holding a [`SpinLock`] or
/// with interrupts disabled (`lock()` asserts
/// `(irq::enabled() || irq::disabled_for_good_but_not_here()) && irq::depth() == 0`).
pub struct SleepMutex<T> {
    locked: SpinLock<bool>,
    waiters: WaitQueue,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for SleepMutex<T> {}
// SAFETY: moving the mutex moves `value`; `T: Send` makes that sound.
unsafe impl<T: Send> Send for SleepMutex<T> {}

impl<T> SleepMutex<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: SpinLock::new(false), waiters: WaitQueue::new(), value: UnsafeCell::new(value) }
    }

    /// Acquire the mutex, sleeping with interrupts enabled if another thread
    /// currently holds it.
    ///
    /// # Panics
    /// If interrupts are disabled (unless another CPU disabled them for good
    /// during a crash report or reboot) or a `SpinLock` critical section is
    /// open (`irq::depth() != 0`).
    #[track_caller]
    pub fn lock(&self) -> SleepMutexGuard<'_, T> {
        assert!(
            (arch::irq::enabled() || arch::irq::disabled_for_good_but_not_here()) && arch::irq::depth() == 0,
            "SleepMutex::lock requires interrupts enabled and no SpinLock held"
        );
        // Fast path: uncontended acquire without touching the wait queue.
        {
            let mut held = self.locked.lock();
            if !*held {
                *held = true;
                return SleepMutexGuard { lock: self, _not_send: PhantomData };
            }
        }
        // Slow path: sleep on `waiters` until we observe `*held == false` and
        // claim it under the wait queue's lock (lock order: `waiters` -> `locked`;
        // `Drop` releases `locked` before calling `wake_one()`, preventing AB-BA).
        self.waiters.wait_until(|| {
            let mut held = self.locked.lock();
            if !*held {
                *held = true;
                true
            } else {
                false
            }
        });
        SleepMutexGuard { lock: self, _not_send: PhantomData }
    }

    /// Acquire the mutex if it is currently unlocked without sleeping.
    #[allow(dead_code)] // general synchronization surface
    pub fn try_lock(&self) -> Option<SleepMutexGuard<'_, T>> {
        let mut held = self.locked.lock();
        if *held {
            return None;
        }
        *held = true;
        Some(SleepMutexGuard { lock: self, _not_send: PhantomData })
    }

    /// Mutable access to the inner value when the caller has exclusive access
    /// to the mutex (e.g. inside `Drop`, where taking a sleeping lock may be
    /// disallowed because interrupts are off).
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

/// Guard returned by [`SleepMutex::lock`] / [`SleepMutex::try_lock`].
/// Interrupts remain enabled while held.
pub struct SleepMutexGuard<'a, T> {
    lock: &'a SleepMutex<T>,
    /// Not `Send`: keeps unlock paired with the acquiring thread.
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for SleepMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we exclusively hold `self.lock`.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SleepMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we exclusively hold `self.lock`.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SleepMutexGuard<'_, T> {
    fn drop(&mut self) {
        {
            let mut held = self.lock.locked.lock();
            debug_assert!(*held, "SleepMutex unlocked while not held");
            *held = false;
        }
        self.lock.waiters.wake_one();
    }
}

crate::ktest!(
    fn a_held_lock_keeps_interrupts_off() {
        use crate::kassert_eq;
        // Tests run with interrupts on, outside any lock: "restored" means on.
        kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (true, 0));
        let lock = SpinLock::new(0u32);
        {
            let mut g = lock.lock();
            *g += 1;
            kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (false, 1));
        }
        kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (true, 0));
        Ok(())
    }
);

crate::ktest!(
    fn guards_dropped_out_of_order_keep_interrupts_off_until_the_last() {
        use crate::{kassert, kassert_eq};
        kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (true, 0));
        let (a, b) = (SpinLock::new(()), SpinLock::new(()));
        let ga = a.lock();
        let gb = b.lock();
        drop(ga);
        kassert!(!arch::irq::enabled()); // `b` is still held
        drop(gb);
        kassert!(arch::irq::enabled());
        Ok(())
    }
);

crate::ktest!(
    fn a_lock_records_the_cpu_holding_it() {
        // `lock` and `try_lock` record this CPU, the guard's drop clears it,
        // and a failed `try_lock` (this CPU holds it) leaves it recorded.
        use crate::kassert;
        let lock = SpinLock::new(());
        kassert!(!lock.held_here());
        let g = lock.lock();
        kassert!(lock.held_here());
        kassert!(lock.try_lock().is_none() && lock.held_here());
        drop(g);
        kassert!(!lock.held_here());
        let g = lock.try_lock();
        kassert!(g.is_some() && lock.held_here());
        drop(g);
        kassert!(!lock.held_here());
        Ok(())
    }
);

crate::ktest!(
    fn a_lock_held_by_another_cpu_is_waited_for() {
        // CPU 1 takes the lock and holds it until this CPU is about to call
        // `lock` (and a while after, so `lock` most likely spins): there it
        // is held, here it isn't, and `lock` here returns only once CPU 1 let
        // go, instead of reporting a re-lock. A handshake, not a fixed hold:
        // a host can stall this vCPU past any fixed hold.
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU8};
        const NOT_TAKEN: u8 = 0;
        const HELD_THERE: u8 = 1; // CPU 1 holds it and saw it held there
        const HELD_UNSEEN: u8 = 2; // CPU 1 holds it but didn't see that
        /// How long CPU 1 holds it after this CPU sets `LOCKING`: long enough
        /// that `lock`, called right after, finds it still held unless this
        /// vCPU stalls longer than that in between. Nothing checked depends
        /// on it.
        const HOLD_NS: u64 = 100_000_000;
        /// How long CPU 1 waits for `LOCKING` before going on with its hold
        /// anyway (a stall of this vCPU that long before it reads
        /// `RELEASING` fails the test: `contended` is false).
        const WAIT_NS: u64 = 5_000_000_000;
        static SHARED: SpinLock<()> = SpinLock::new(());
        static STEP: AtomicU8 = AtomicU8::new(NOT_TAKEN);
        /// Set by this CPU just before it calls `lock`.
        static LOCKING: AtomicBool = AtomicBool::new(false);
        /// Set by CPU 1 if it waited WAIT_NS for `LOCKING` and went on.
        static WAITED_OUT: AtomicBool = AtomicBool::new(false);
        /// Set by CPU 1 just before it lets go.
        static RELEASING: AtomicBool = AtomicBool::new(false);
        fn on_cpu_1() {
            let held = SHARED.lock();
            STEP.store(if SHARED.held_here() { HELD_THERE } else { HELD_UNSEEN }, Ordering::Release);
            if !crate::smp::spin_until(crate::time::now_ns() + WAIT_NS, || LOCKING.load(Ordering::Acquire)) {
                WAITED_OUT.store(true, Ordering::Relaxed);
            }
            crate::time::spin(HOLD_NS);
            RELEASING.store(true, Ordering::Relaxed); // published by the release
            drop(held);
        }
        crate::smp::run_on_test(1, on_cpu_1)?;
        kassert!(crate::smp::job_reached(1, || STEP.load(Ordering::Acquire) != NOT_TAKEN));
        let seen = (STEP.load(Ordering::Acquire), SHARED.held_here(), SHARED.try_lock().is_none());
        let contended = !RELEASING.load(Ordering::Relaxed);
        LOCKING.store(true, Ordering::Release);
        let here = SHARED.lock();
        let after = (RELEASING.load(Ordering::Relaxed), SHARED.held_here());
        drop(here);
        // Still held just before `lock` was called (so `seen` is from while
        // CPU 1 held it, and `lock` most likely had to wait), and taken only
        // once CPU 1 let go.
        if !contended {
            crate::ktest::diag(format_args!(
                "cpu 1 let go before this cpu said it was locking (cpu 1 waited it out: {})",
                WAITED_OUT.load(Ordering::Relaxed)
            ));
        }
        kassert!(contended);
        kassert_eq!(seen, (HELD_THERE, false, true));
        kassert_eq!(after, (true, true));
        Ok(())
    }
);

crate::ktest!(
    fn a_contended_lock_excludes_and_knows_its_holder() {
        // CPUs 0 and 1 hammer one lock together with `lock` and `try_lock`:
        // they really contend (some `try_lock` fails), every critical
        // section sees the lock held by its own CPU, and no increment is
        // lost (mutual exclusion). Each counts its `ROUNDS` rounds from when
        // either first sees a `try_lock` fail, so both are hammering when
        // they start: under KVM the host can stop one vCPU for longer than
        // the other's whole run (#85), and counted from the start they
        // needn't overlap (or overlap in a handful of rounds, leaving the
        // exclusion checks little to catch). A vCPU stopped after that
        // still leaves the other to finish alone: the test then passes on
        // fewer overlapping rounds rather than failing. Waiting for that first contention is bounded by
        // `CONTENTION_NS`, well inside the 10 s `DONE` wait below
        // (`sched::TEST_DEADLINE_NS`).
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU64};
        const ROUNDS: u64 = 20_000;
        const CONTENTION_NS: u64 = 2_000_000_000;
        static HOT: SpinLock<u64> = SpinLock::new(0);
        static MISSES: AtomicU64 = AtomicU64::new(0);
        static ENTERED_THERE: AtomicU64 = AtomicU64::new(0);
        static ROUNDS_THERE: AtomicU64 = AtomicU64::new(0);
        /// Some `try_lock` found the lock held, on either CPU.
        static CONTENDED: AtomicBool = AtomicBool::new(false);
        /// CPU 1 is in its job.
        static READY: AtomicBool = AtomicBool::new(false);
        /// 0 until CPU 0 gives the start signal, which is the `now_ns`
        /// past which neither waits any longer for contention.
        static GO_UNTIL: AtomicU64 = AtomicU64::new(0);
        static DONE: AtomicBool = AtomicBool::new(false);
        /// The rounds this CPU entered the lock in, and all its rounds.
        fn hammer(until: u64) -> (u64, u64) {
            let mut entered = 0;
            let mut rounds = 0;
            // Rounds since contention was first seen.
            let mut together = 0;
            while together < ROUNDS {
                if CONTENDED.load(Ordering::Relaxed) {
                    together += 1;
                } else if crate::time::now_ns() >= until {
                    break;
                }
                let guard = if rounds % 2 == 0 { Some(HOT.lock()) } else { HOT.try_lock() };
                match guard {
                    Some(mut g) => {
                        if !HOT.held_here() {
                            MISSES.fetch_add(1, Ordering::Relaxed);
                        }
                        *g += 1;
                        entered += 1;
                    }
                    None => CONTENDED.store(true, Ordering::Relaxed),
                }
                rounds += 1;
            }
            (entered, rounds)
        }
        fn on_cpu_1() {
            READY.store(true, Ordering::Release);
            // Bounded: if CPU 0 gave up, don't stay here.
            let deadline = crate::time::now_ns() + 10_000_000_000;
            let until = loop {
                match GO_UNTIL.load(Ordering::Acquire) {
                    0 if crate::time::now_ns() > deadline => return,
                    0 => core::hint::spin_loop(),
                    until => break until,
                }
            };
            let (entered, rounds) = hammer(until);
            ENTERED_THERE.store(entered, Ordering::Relaxed);
            ROUNDS_THERE.store(rounds, Ordering::Relaxed);
            DONE.store(true, Ordering::Release);
        }
        crate::smp::run_on_test(1, on_cpu_1)?;
        kassert!(crate::smp::job_reached(1, || READY.load(Ordering::Acquire)));
        let start = crate::time::now_ns();
        let until = start + CONTENTION_NS;
        GO_UNTIL.store(until, Ordering::Release);
        let (entered_here, rounds_here) = hammer(until);
        let ran_us = (crate::time::now_ns() - start) / 1000;
        let done = crate::smp::job_reached(1, || DONE.load(Ordering::Acquire));
        // Before any verdict. Without contention, a CPU with few rounds was
        // stopped (or came after the deadline); both with millions would
        // be a broken lock.
        crate::ktest::diag(format_args!(
            "cpu 0: {rounds_here} rounds in {ran_us} us; cpu 1: {} rounds{}",
            ROUNDS_THERE.load(Ordering::Relaxed),
            if done { "" } else { " (not done)" }
        ));
        kassert!(done);
        let there = ENTERED_THERE.load(Ordering::Relaxed);
        kassert!(CONTENDED.load(Ordering::Relaxed)); // some `try_lock` found it held
        kassert_eq!(MISSES.load(Ordering::Relaxed), 0);
        kassert_eq!(*HOT.lock(), entered_here + there);
        Ok(())
    }
);

crate::ktest!(
    fn a_failed_try_lock_changes_nothing() {
        use crate::{kassert, kassert_eq};
        kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (true, 0));
        let lock = SpinLock::new(());
        let held = lock.lock();
        kassert!(lock.try_lock().is_none());
        // Still held: the failed attempt didn't release the holder's lock.
        kassert!(lock.try_lock().is_none());
        kassert_eq!((arch::irq::enabled(), arch::irq::depth()), (false, 1));
        drop(held);
        let again = lock.try_lock();
        kassert!(again.is_some() && !arch::irq::enabled());
        drop(again);
        kassert!(arch::irq::enabled());
        Ok(())
    }
);

crate::ktest!(
    fn a_lock_taken_with_interrupts_off_leaves_them_off() {
        // As in an interrupt handler: interrupts were already off, so
        // releasing the lock must not turn them on.
        use crate::{kassert, kassert_eq};
        kassert_eq!(arch::irq::depth(), 0);
        // SAFETY: masks interrupts on this CPU; turned back on below.
        unsafe { core::arch::asm!("cli") };
        let lock = SpinLock::new(());
        drop(lock.lock());
        let stayed_off = !arch::irq::enabled();
        arch::irq::enable();
        kassert!(stayed_off);
        Ok(())
    }
);

crate::ktest!(
    fn sleep_mutex_keeps_irqs_on_and_wakes_waiter() {
        use crate::{kassert, kassert_eq};
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let mutex = Arc::new(SleepMutex::new(0u64));
        let entered = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        // Holding a SleepMutex leaves interrupts ON and irq::depth() == 0.
        let mut g = mutex.lock();
        kassert!(arch::irq::enabled());
        kassert_eq!(arch::irq::depth(), 0);
        kassert!(mutex.try_lock().is_none());
        *g = 10;

        let m2 = mutex.clone();
        let e2 = entered.clone();
        let d2 = done.clone();
        let handle = crate::sched::spawn("sleep-mtx-wait", move || {
            e2.store(true, Ordering::Release);
            let mut guard = m2.lock();
            assert!(arch::irq::enabled() && arch::irq::depth() == 0);
            *guard += 32;
            d2.store(true, Ordering::Release);
        })
        .map_err(|_| "spawn failed")?;

        // Let the waiter run and block on the mutex while we still hold it.
        kassert!(crate::sched::yield_until_blocked(core::iter::once(&handle), None));
        kassert!(entered.load(Ordering::Acquire));
        kassert!(!done.load(Ordering::Acquire));

        drop(g);
        handle.join();
        kassert!(done.load(Ordering::Acquire));
        kassert_eq!(*mutex.lock(), 42);
        Ok(())
    }
);
