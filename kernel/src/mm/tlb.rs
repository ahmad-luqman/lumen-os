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
//! again [`retire`]s them, which numbers a generation, and before handing
//! them out makes every CPU flush through it ([`sync`]): each CPU records
//! the newest generation it has flushed through ([`catch_up`], from its
//! timer and `WAKEUP` interrupts), and [`sync`] sends `WAKEUP` IPIs to the
//! CPUs behind and waits for them. The kernel stacks ([`super::stack`]) are
//! the one user.
//!
//! A CPU counts from the moment it first loads the kernel's tables: the
//! boot CPU from the start, an AP from just before it goes online
//! ([`join`]); until then it holds none of these translations (the AP flushed
//! everything when it loaded the tables, and [`join`] flushes again after
//! reading the generation). A flush is a CR3 reload
//! (`arch::paging::flush_local`): every kernel-window translation is
//! non-global.

use core::sync::atomic::{AtomicU64, Ordering};

use log::warn;

use crate::arch::{self, MAX_CPUS};
use crate::{smp, time};

/// The newest generation retired.
static RETIRED: AtomicU64 = AtomicU64::new(0);

/// A CPU that hasn't loaded the kernel's tables, so holds none of their
/// translations ([`FLUSHED`]).
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
/// reuse the pages).
const SYNC_TIMEOUT_NS: u64 = 2_000_000_000;

/// Number the pages just unmapped (on this CPU, by the caller) as a new
/// generation, which [`sync`] makes every CPU flush through. Call it after
/// the unmapping.
pub fn retire() -> u64 {
    RETIRED.fetch_add(1, Ordering::SeqCst) + 1
}

/// The newest generation every counted CPU has flushed through: pages
/// retired at it or before are in no CPU's TLB.
pub fn flushed_through() -> u64 {
    FLUSHED.iter().map(|f| f.load(Ordering::SeqCst)).min().unwrap_or(0).min(RETIRED.load(Ordering::SeqCst))
}

/// Flush this CPU through the newest generation, if it is behind (and
/// counted). From the timer and `WAKEUP` interrupts, and [`sync`]'s waits;
/// interrupts off, so the CPU index holds.
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

/// Start counting this AP, just before it goes online (it has loaded the
/// kernel's tables): from here [`sync`] waits for it, and its interrupts
/// [`catch_up`]. Interrupts off.
pub fn join() {
    let flushed = &FLUSHED[arch::cpu_index()];
    // Counted first, as behind everything: a `sync` that sees it from here
    // on waits for it; one that saw it not counted retired its pages
    // before the generation read below, so the flush after it covers them.
    flushed.store(0, Ordering::SeqCst);
    let newest = RETIRED.load(Ordering::SeqCst);
    arch::paging::flush_local();
    flushed.store(newest, Ordering::SeqCst);
}

/// Make every counted CPU flush through generation `generation`: this one
/// at once, the others at their next interrupt, sent a `WAKEUP` IPI now.
/// Waits for them only from a thread outside every critical section with
/// interrupts on (a CPU spinning on a lock this caller held, interrupts
/// off, could never answer), at most [`SYNC_TIMEOUT_NS`] (warned about);
/// whether they all got there.
pub fn sync(generation: u64) -> bool {
    {
        let _off = arch::irq::push_off();
        catch_up();
    }
    let here = arch::cpu_index();
    let mut woken = 0u64;
    let lagging = kick(generation, here, &mut woken);
    let free = arch::irq::depth() == 0 && arch::irq::enabled();
    // Before the clocks are calibrated there is no deadline to wait for.
    let start = time::now_ns();
    if lagging == 0 || !free || start == 0 {
        return lagging == 0;
    }
    let deadline = start.saturating_add(SYNC_TIMEOUT_NS);
    loop {
        let lagging = kick(generation, here, &mut woken);
        if lagging == 0 {
            return true;
        }
        if time::now_ns() >= deadline {
            warn!(target: "mm", "cpus {lagging:#x} didn't flush their TLBs in time: freed stack addresses aren't reused");
            return false;
        }
        core::hint::spin_loop();
    }
}

/// The CPUs (a bit each) not yet flushed through `generation`; each online
/// one but `here` that isn't in `woken` yet is sent a `WAKEUP` IPI and
/// added to it (one that couldn't be sent is left to its next tick).
fn kick(generation: u64, here: usize, woken: &mut u64) -> u64 {
    let mut lagging = 0;
    for i in (0..MAX_CPUS).filter(|&i| FLUSHED[i].load(Ordering::SeqCst) < generation) {
        lagging |= 1 << i;
        if i != here && *woken & 1 << i == 0 && smp::online_lapic_id(i).is_some() && smp::wake(i) {
            *woken |= 1 << i;
        }
    }
    lagging
}

crate::ktest!(
    fn every_cpu_flushes_through_a_retired_generation() {
        // `sync` brings every online CPU's count to the generation it was
        // given, the idle ones woken by IPI; a later `retire` puts them all
        // behind again.
        use crate::{kassert, kassert_eq};
        let generation = retire();
        kassert!(flushed_through() < generation);
        kassert!(sync(generation));
        kassert!(flushed_through() >= generation);
        for (i, flushed) in FLUSHED.iter().enumerate() {
            let f = flushed.load(Ordering::SeqCst);
            kassert_eq!(f == NOT_COUNTED, i != 0 && smp::online_lapic_id(i).is_none());
            kassert!(f >= generation);
        }
        Ok(())
    }
);
