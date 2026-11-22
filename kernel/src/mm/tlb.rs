//! Retiring kernel-window pages from every CPU's TLB before their addresses
//! are used again (#300).
//!
//! Unmapping a page of the heap or stack windows ([`super::paging::unmap`])
//! flushes it from this CPU's TLB only; another CPU that touched the page
//! (a spawner writing a new thread's first frame, a `sched::scope` thread
//! borrowing from its caller's stack) may keep the old translation. That
//! is harmless while nothing uses the address, and a hazard once it is
//! mapped again to another frame: that CPU would read and write the old
//! frame, by then someone else's. So whoever unmaps pages it will hand out
//! again [`retire`]s them, which numbers a [`Generation`], and hands them out
//! only once every CPU has flushed through it ([`flushed_through`], the one
//! maker of a [`Flushed`]): each CPU records the newest generation it has
//! flushed through ([`catch_up`], from its timer and `WAKEUP` interrupts and
//! once at the start of [`sync`]), and [`sync`] sends `WAKEUP` IPIs to the
//! CPUs behind and waits for them where it can. The kernel stacks
//! ([`super::stack`]) are the one user.
//!
//! A CPU counts from the start (the boot CPU) or from the first thing it
//! does on the kernel's tables and its own stack (an AP, [`join`] at the
//! top of `smp::ap_main`). Until then an AP uses no translation of a
//! retired page: it has run only `smp::ap_entry`'s switch onto its own main
//! stack, which is never retired, and [`join`] flushes whatever its walker
//! may have cached meanwhile. A flush is a CR3 reload
//! (`arch::paging::flush_local`): every kernel-window translation is
//! non-global.

use core::sync::atomic::{AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::stacks::{Flushed, Generation};

use crate::arch::{self, MAX_CPUS};
use crate::{smp, time};

/// The newest generation retired.
static RETIRED: AtomicU64 = AtomicU64::new(0);

/// A CPU not counted yet: one that hasn't loaded the kernel's tables, or
/// an AP that has but hasn't reached [`join`] yet ([`FLUSHED`]).
const NOT_COUNTED: u64 = u64::MAX;

/// The newest generation each CPU has flushed through, or [`NOT_COUNTED`]
/// (the boot CPU, index 0, counts from the start).
static FLUSHED: [AtomicU64; MAX_CPUS] = {
    let mut f = [const { AtomicU64::new(NOT_COUNTED) }; MAX_CPUS];
    f[0] = AtomicU64::new(0);
    f
};

/// How long [`sync`] waits for the other CPUs before giving up (a CPU that
/// keeps its interrupts off this long is stuck; the caller then doesn't
/// reuse the pages yet).
const SYNC_TIMEOUT_NS: u64 = 2_000_000_000;

/// How often a [`sync`] that doesn't wait, because CPUs are still behind a
/// wait that gave up, warns again.
const STILL_STUCK_NS: u64 = 10_000_000_000;

/// The generation the last timed-out [`sync`] waited for (0 if none, or
/// once every CPU has got past it).
static GAVE_UP: AtomicU64 = AtomicU64::new(0);
/// When a CPU's being stuck was last warned about (`time::now_ns`).
static STUCK_WARNED: AtomicU64 = AtomicU64::new(0);

/// Number the pages just unmapped as a new generation, which [`sync`]
/// makes every CPU flush through.
///
/// # Safety
/// Every page given back with the result is already unmapped.
pub unsafe fn retire() -> Generation {
    let n = RETIRED.fetch_add(1, Ordering::SeqCst) + 1;
    // SAFETY: the caller unmapped the pages it gives back with this before
    // the count above, and `catch_up` reads the count before it flushes.
    unsafe { Generation::retired(n) }
}

/// The newest generation every counted CPU has flushed through: pages
/// retired at it or before are in no CPU's TLB.
pub fn flushed_through() -> Flushed {
    let n = FLUSHED.iter().map(|f| f.load(Ordering::SeqCst)).min().unwrap_or(0).min(RETIRED.load(Ordering::SeqCst));
    // SAFETY: each counted CPU stored a generation in `FLUSHED` only after
    // a CR3 reload that came after it read that generation (`catch_up`,
    // `join`), and every translation is non-global. A CPU not counted
    // either hasn't loaded the kernel's tables, or is an AP between loading
    // them and `join` (the top of `ap_main`): it runs only on its own main
    // stack, never retired, so it uses no translation of a retired page,
    // and `join`, which counts it before reading the generation, flushes
    // whatever it cached. So no CPU will use a translation of a page
    // retired at `n` or before.
    unsafe { Flushed::vouch(n) }
}

/// Flush this CPU through the newest generation, if it is behind (and
/// counted). From the `WAKEUP` interrupt, the timer's ([`on_tick`]) and
/// [`sync`]'s start; interrupts off, so the CPU index holds.
pub fn catch_up() {
    let flushed = &FLUSHED[arch::cpu_index()];
    let mine = flushed.load(Ordering::SeqCst);
    // Read before the flush: everything retired by then is unmapped.
    let newest = RETIRED.load(Ordering::SeqCst);
    if mine < newest && mine != NOT_COUNTED {
        arch::paging::flush_local();
        flushed.store(newest, Ordering::SeqCst);
    }
}

/// The ktests' switch: off, the timer doesn't [`catch_up`], so only a
/// `WAKEUP` IPI (such as [`sync`]'s) flushes the other CPUs
/// ([`TickFlushesOff`]).
#[cfg(feature = "ktest")]
static TICK_FLUSHES: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// The timer interrupt's part: [`catch_up`].
pub fn on_tick() {
    #[cfg(feature = "ktest")]
    if !TICK_FLUSHES.load(Ordering::SeqCst) {
        return;
    }
    catch_up();
}

/// While held, the CPUs' timer interrupts don't [`catch_up`] (ktests).
#[cfg(feature = "ktest")]
pub(crate) struct TickFlushesOff(());

#[cfg(feature = "ktest")]
impl TickFlushesOff {
    pub(crate) fn new() -> TickFlushesOff {
        TICK_FLUSHES.store(false, Ordering::SeqCst);
        TickFlushesOff(())
    }
}

#[cfg(feature = "ktest")]
impl Drop for TickFlushesOff {
    fn drop(&mut self) {
        TICK_FLUSHES.store(true, Ordering::SeqCst);
    }
}

/// Start counting this AP, first thing on the kernel's tables and its own
/// stack (the top of `smp::ap_main`): from here [`sync`] waits for it, and
/// its interrupts [`catch_up`]. Interrupts off.
pub fn join() {
    let flushed = &FLUSHED[arch::cpu_index()];
    // Counted first, as behind everything: a `flushed_through` that sees it
    // from here on waits for it; one that saw it not counted vouched only
    // for generations retired before that, so before the read below, and
    // the flush after that read covers their pages.
    flushed.store(0, Ordering::SeqCst);
    let newest = RETIRED.load(Ordering::SeqCst);
    arch::paging::flush_local();
    flushed.store(newest, Ordering::SeqCst);
}

/// What [`sync`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Synced {
    /// Every counted CPU has flushed through the generation.
    Done,
    /// It couldn't wait (interrupts off, a critical section open, or the
    /// clocks not calibrated yet): left to the other CPUs' ticks.
    NoWait,
    /// It didn't wait: CPUs are still behind a wait that timed out.
    GaveUp,
    /// It waited [`SYNC_TIMEOUT_NS`] and they didn't all get there.
    TimedOut,
}

/// Make every counted CPU flush through generation `generation` (a
/// number, as `StackWindow::wait_for` gives it): this one at once, the
/// others at their next interrupt (a tick, or a `WAKEUP`). Only from a
/// thread outside every critical section with interrupts on, and once the
/// clocks are calibrated, does it send the ones behind a `WAKEUP` IPI and
/// wait for them (a CPU spinning on a lock this caller held, interrupts
/// off, could never answer), at most [`SYNC_TIMEOUT_NS`] (warned about).
/// After a wait gave up, none waits again until every CPU has got past that
/// one's generation: a `sync` meanwhile warns again at most every
/// [`STILL_STUCK_NS`], and the first one after they have logs it.
pub fn sync(generation: u64) -> Synced {
    {
        let _off = arch::irq::push_off();
        catch_up();
    }
    let now = time::now_ns();
    if still_stuck(now) {
        return if lagging(generation) == 0 { Synced::Done } else { Synced::GaveUp };
    }
    if lagging(generation) == 0 {
        return Synced::Done;
    }
    if arch::irq::depth() != 0 || !arch::irq::enabled() || now == 0 {
        return Synced::NoWait;
    }
    let here = arch::cpu_index();
    let deadline = now.saturating_add(SYNC_TIMEOUT_NS);
    let mut woken = 0u64;
    loop {
        let lagging = lagging(generation);
        if lagging == 0 {
            return Synced::Done;
        }
        // Each online one behind, until its IPI is sent (one that couldn't
        // be is tried again on the next pass): including one that came
        // online meanwhile.
        let unwoken = lagging & !woken & !(1 << here);
        for i in (0..MAX_CPUS).filter(|&i| unwoken & 1 << i != 0) {
            if smp::online_lapic_id(i).is_some() && smp::wake(i) {
                woken |= 1 << i;
            }
        }
        let now = time::now_ns();
        if now >= deadline {
            GAVE_UP.store(generation, Ordering::SeqCst);
            STUCK_WARNED.store(now, Ordering::SeqCst);
            warn!(target: "mm", "cpus {lagging:#x} didn't flush their TLBs within 2 s: freed stack addresses wait for them");
            return Synced::TimedOut;
        }
        core::hint::spin_loop();
    }
}

/// Are CPUs still behind the generation a [`sync`] gave up on? Warns about
/// them at most every [`STILL_STUCK_NS`], and logs once when they aren't.
fn still_stuck(now: u64) -> bool {
    let gave_up = GAVE_UP.load(Ordering::SeqCst);
    if gave_up == 0 {
        return false;
    }
    let stuck = lagging(gave_up);
    if stuck == 0 {
        if GAVE_UP.compare_exchange(gave_up, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            info!(target: "mm", "every cpu flushed its TLB at last: freed stack addresses are reused again");
        }
        return false;
    }
    let last = STUCK_WARNED.load(Ordering::SeqCst);
    if now.saturating_sub(last) >= STILL_STUCK_NS
        && STUCK_WARNED.compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    {
        warn!(target: "mm", "cpus {stuck:#x} still haven't flushed their TLBs: freed stack addresses wait for them");
    }
    true
}

/// The counted CPUs (a bit each) not yet flushed through `generation`.
fn lagging(generation: u64) -> u64 {
    (0..MAX_CPUS).filter(|&i| FLUSHED[i].load(Ordering::SeqCst) < generation).fold(0, |m, i| m | 1 << i)
}

/// The counted CPUs (a bit each) not yet flushed through the newest
/// generation retired.
pub fn behind() -> u64 {
    lagging(RETIRED.load(Ordering::SeqCst))
}

crate::ktest!(
    fn every_cpu_flushes_through_a_retired_generation() {
        // With the timer's flushes off, `sync` brings every online CPU's
        // count to the generation it was given by IPI alone.
        use crate::{kassert, kassert_eq};
        let _ticks_off = TickFlushesOff::new();
        // SAFETY: given back with nothing.
        let generation = unsafe { retire() };
        kassert_eq!(sync(generation.get()), Synced::Done);
        kassert!(lagging(generation.get()) == 0);
        for (i, flushed) in FLUSHED.iter().enumerate() {
            let f = flushed.load(Ordering::SeqCst);
            kassert_eq!(f == NOT_COUNTED, i != 0 && smp::online_lapic_id(i).is_none());
            kassert!(f >= generation.get());
        }
        Ok(())
    }
);
