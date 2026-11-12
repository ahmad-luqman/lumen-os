//! Minimal synchronization primitives.
//!
//! [`SpinLock`] turns this CPU's interrupts off while it is held (nested, see
//! `arch::irq`), so a maskable interrupt's handler that takes a lock can
//! never spin on one the code it interrupted holds; other CPUs spin until it
//! is released (interrupt nesting is per CPU). A guard can't be sent to
//! another thread (it holds this CPU's critical section).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

/// A test-and-set spinlock protecting a `T`. Interrupts are off while it is
/// held (and while waiting for it).
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the value; `T: Send` makes that sound.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let irq = arch::irq::push_off();
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self, _irq: irq }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let irq = arch::irq::push_off();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // No guard: dropping one would unlock the holder's lock (an
            // eager `then_some` did). Dropping `irq` closes the section.
            return None;
        }
        Some(SpinLockGuard { lock: self, _irq: irq })
    }

    /// Access the value even if the lock is held.
    ///
    /// # Safety
    /// For the crash reporter, which must print even when the crash happened
    /// while the lock was held, and for tests that hold the lock without
    /// touching the value. The caller guarantees no other code uses the value
    /// while the reference lives (on the crash path: interrupts off for good,
    /// every other online CPU stopped or named as not stopped, and the
    /// interrupted holder never resumes).
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
        self.lock.locked.store(false, Ordering::Release);
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
