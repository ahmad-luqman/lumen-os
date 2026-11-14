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
//! while a lock is held (`sched::schedule` asserts it), so the holder is
//! this CPU's own code, or the code an exception or NMI interrupted, and
//! neither runs again until the second `lock` returns.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;

/// A test-and-set spinlock protecting a `T`. Interrupts are off while it is
/// held (and while waiting for it).
pub struct SpinLock<T> {
    /// [`FREE`], or the holder's [`owner_id`]. Only the holder stores
    /// anything but `FREE` here, so a CPU reads its own id exactly while it
    /// holds the lock.
    owner: AtomicU32,
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
        Self { owner: AtomicU32::new(FREE), value: UnsafeCell::new(value) }
    }

    /// Take the lock, spinning while another CPU holds it.
    ///
    /// # Panics
    /// If this CPU already holds it (it would wait for itself forever), or
    /// leaked a guard of it. The panic names the lock's address and type
    /// and this CPU, and its location is the caller's (`track_caller`).
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Interrupts off first: no timer switch or maskable handler on this
        // CPU between reading the owner and taking the lock.
        let irq = arch::irq::push_off();
        let me = owner_id();
        loop {
            match self.owner.compare_exchange_weak(FREE, me, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return SpinLockGuard { lock: self, _irq: irq },
                Err(owner) if owner == me => {
                    relocked(core::ptr::from_ref(self).addr(), core::any::type_name::<T>(), me - 1)
                }
                // Held elsewhere (or a spurious failure): wait until it looks free.
                Err(_) => {
                    while self.owner.load(Ordering::Relaxed) != FREE {
                        core::hint::spin_loop();
                    }
                }
            }
        }
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

    /// Whether this CPU holds the lock (for the ktests).
    #[cfg(feature = "ktest")]
    fn held_here(&self) -> bool {
        let _irq = arch::irq::push_off();
        self.owner.load(Ordering::Relaxed) == owner_id()
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
/// stays small). Takes no lock, and neither does the crash reporter (it
/// reads locked state through `try_lock` and `force_get`), so re-locking
/// any lock (the heap's, the log's, the page tables') still gets a full
/// report.
#[cold]
#[inline(never)]
#[track_caller]
fn relocked(addr: usize, ty: &str, cpu: u32) -> ! {
    panic!("SpinLock<{ty}> at {addr:#x} re-locked on cpu {cpu}, which already holds it")
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
        // CPU 1 takes the lock and holds it for a while: there it is held,
        // here it isn't, and `lock` here waits for CPU 1 to let go instead
        // of reporting a re-lock.
        use crate::sched::yield_until;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU8};
        const NOT_TAKEN: u8 = 0;
        const HELD_THERE: u8 = 1; // CPU 1 holds it and saw it held there
        const HELD_UNSEEN: u8 = 2; // CPU 1 holds it but didn't see that
        /// How long CPU 1 holds it: far longer than this CPU takes to see
        /// that it is held and call `lock`.
        const HOLD_NS: u64 = 100_000_000;
        static SHARED: SpinLock<()> = SpinLock::new(());
        static STEP: AtomicU8 = AtomicU8::new(NOT_TAKEN);
        /// Set by CPU 1 just before it lets go.
        static RELEASING: AtomicBool = AtomicBool::new(false);
        fn on_cpu_1() {
            let held = SHARED.lock();
            STEP.store(if SHARED.held_here() { HELD_THERE } else { HELD_UNSEEN }, Ordering::Release);
            let until = crate::time::now_ns() + HOLD_NS;
            while crate::time::now_ns() < until {
                core::hint::spin_loop();
            }
            RELEASING.store(true, Ordering::Relaxed); // published by the release
            drop(held);
        }
        crate::smp::run_on(1, on_cpu_1).map_err(|e| {
            crate::ktest::diag(format_args!("run_on cpu 1: {e}"));
            "run_on failed"
        })?;
        kassert!(yield_until(|| STEP.load(Ordering::Acquire) != NOT_TAKEN));
        let seen = (STEP.load(Ordering::Acquire), SHARED.held_here(), SHARED.try_lock().is_none());
        let contended = !RELEASING.load(Ordering::Relaxed);
        let here = SHARED.lock();
        let after = (RELEASING.load(Ordering::Relaxed), SHARED.held_here());
        drop(here);
        // Still held when `lock` was called (so `seen` is from while CPU 1
        // held it, and `lock` had to wait), and taken only once CPU 1 let go.
        kassert!(contended);
        kassert_eq!(seen, (HELD_THERE, false, true));
        kassert_eq!(after, (true, true));
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
