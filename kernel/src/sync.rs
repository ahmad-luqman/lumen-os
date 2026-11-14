//! Minimal synchronization primitives.
//!
//! [`SpinLock`] turns this CPU's interrupts off while it is held (nested, see
//! `arch::irq`), so a maskable interrupt's handler that takes a lock can
//! never spin on one the code it interrupted holds; other CPUs spin until it
//! is released (interrupt nesting is per CPU). A guard can't be sent to
//! another thread (it holds this CPU's critical section).
//!
//! A lock also records which CPU holds it. A CPU that takes a lock it
//! already holds would spin forever with interrupts off (a silent hang);
//! [`SpinLock::lock`] panics instead, so the crash report names the lock,
//! the CPU and the second caller. Only that CPU's code can be holding it:
//! no thread switches while a lock is held (`sched`), so the holder is the
//! code this CPU was running, or the code an exception or NMI interrupted.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch;

/// A test-and-set spinlock protecting a `T`. Interrupts are off while it is
/// held (and while waiting for it).
pub struct SpinLock<T> {
    locked: AtomicBool,
    /// The index of the CPU holding the lock, or [`NO_HOLDER`]: set right
    /// after taking it, cleared right before releasing it (so a CPU that
    /// takes it next can't have its index overwritten). A CPU compares it
    /// only with its own index, and always reads its own last write or a
    /// later one (coherence of a single location): it reads its own index
    /// exactly while it holds the lock, so `Relaxed` is enough. (An NMI or
    /// exception landing between the `locked` update and the `holder` one
    /// that takes the same lock still spins: the check misses that window.)
    holder: AtomicU32,
    value: UnsafeCell<T>,
}

/// [`SpinLock::holder`] of a free lock.
const NO_HOLDER: u32 = u32::MAX;

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the value; `T: Send` makes that sound.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), holder: AtomicU32::new(NO_HOLDER), value: UnsafeCell::new(value) }
    }

    /// Take the lock, spinning while another CPU holds it.
    ///
    /// # Panics
    /// If this CPU already holds it (it would wait for itself forever). The
    /// panic names the lock's address and type and this CPU, and its
    /// location is the caller's (`track_caller`).
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Interrupts off first: from here this code stays on this CPU.
        let irq = arch::irq::push_off();
        let me = this_cpu();
        if self.holder.load(Ordering::Relaxed) == me {
            relocked(core::ptr::from_ref(self).addr(), core::any::type_name::<T>(), me);
        }
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        self.holder.store(me, Ordering::Relaxed);
        SpinLockGuard { lock: self, _irq: irq }
    }

    /// Take the lock if it is free; `None` if any CPU holds it (this one
    /// included).
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let irq = arch::irq::push_off();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // No guard: dropping one would unlock the holder's lock (an
            // eager `then_some` did). Dropping `irq` closes the section.
            return None;
        }
        self.holder.store(this_cpu(), Ordering::Relaxed);
        Some(SpinLockGuard { lock: self, _irq: irq })
    }

    /// Whether this CPU holds the lock (for the ktests).
    #[cfg(feature = "ktest")]
    fn held_here(&self) -> bool {
        let _irq = arch::irq::push_off();
        self.holder.load(Ordering::Relaxed) == this_cpu()
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
        // Before the release: once it is free, another CPU may take it and
        // record itself.
        self.lock.holder.store(NO_HOLDER, Ordering::Relaxed);
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// This CPU's index, as [`SpinLock::holder`] stores it.
fn this_cpu() -> u32 {
    // `MAX_CPUS` is far below `u32::MAX` (`NO_HOLDER`).
    arch::cpu_index() as u32
}

/// The panic of a CPU taking a lock it already holds (out of line: `lock`
/// stays small). Nothing here takes a lock; the crash reporter reads what
/// it needs without this one (`force_get` rules).
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
    fn a_lock_held_by_another_cpu_is_not_held_here() {
        // CPU 1 takes the lock and holds it: there it is held, here it isn't
        // (so `lock` here waits instead of reporting a re-lock), and once CPU
        // 1 lets go, `lock` here takes it.
        use crate::kassert;
        use core::sync::atomic::AtomicU8;
        static SHARED: SpinLock<()> = SpinLock::new(());
        // 0: not taken yet, 1: CPU 1 holds it and saw it held there, 2: it
        // holds it but didn't see that, 3: released.
        static STEP: AtomicU8 = AtomicU8::new(0);
        static LET_GO: AtomicBool = AtomicBool::new(false);
        fn on_cpu_1() {
            let held = SHARED.lock();
            STEP.store(if SHARED.held_here() { 1 } else { 2 }, Ordering::Release);
            // Bounded: a failed check here must not leave the lock held.
            let deadline = crate::time::now_ns() + 5_000_000_000;
            while !LET_GO.load(Ordering::Acquire) && crate::time::now_ns() < deadline {
                core::hint::spin_loop();
            }
            drop(held);
            STEP.store(3, Ordering::Release);
        }
        let wait_step = |done: fn(u8) -> bool| {
            let deadline = crate::time::now_ns() + 5_000_000_000;
            while !done(STEP.load(Ordering::Acquire)) && crate::time::now_ns() < deadline {
                crate::sched::yield_now();
            }
            done(STEP.load(Ordering::Acquire))
        };
        kassert!(crate::smp::online_lapic_id(1).is_some()); // the ktests boot 4 CPUs
        crate::smp::run_on(1, on_cpu_1).map_err(|e| {
            crate::ktest::diag(format_args!("run_on cpu 1: {e}"));
            "run_on failed"
        })?;
        let taken = wait_step(|s| s != 0);
        let seen = (STEP.load(Ordering::Acquire), SHARED.held_here(), SHARED.try_lock().is_none());
        LET_GO.store(true, Ordering::Release);
        kassert!(taken);
        kassert!(seen == (1, false, true));
        let here = SHARED.lock(); // waits for CPU 1 to let go
        kassert!(SHARED.held_here());
        drop(here);
        kassert!(wait_step(|s| s == 3));
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
