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
//!
//! User address spaces use a separate synchronous shootdown IPI (#376, vector
//! `0x3e`, [`arch::idt::TLB_SHOOTDOWN`]): each [`AddressSpace`] tracks the
//! CPUs that have its root loaded in `active` (maintained with
//! `PerCpu::loaded_space` by `AddressSpace::switch_loaded`), and
//! [`shoot_down`] serializes requests through `SHOOTDOWN` (Lock 1a), publishes
//! the root and range (or full flush above [`FULL_FLUSH_PAGES`]) on a
//! lock-free seqlock board, sends vector `0x3e` to each other active CPU, and
//! spins with interrupts enabled until every target has acknowledged in
//! [`on_shootdown_ipi`] or cleared its bit in `active` by switching away.

use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use log::{info, warn};
use lumen_mm::stacks::{Flushed, Generation};
use lumen_mm::{FRAME_SIZE, VirtAddr};
use lumen_vm::USER_MAX;

use super::user_vm::AddressSpace;
use crate::arch::{self, MAX_CPUS};
use crate::sync::SleepMutex;
use crate::{smp, time};

const _: () = assert!(MAX_CPUS <= 64);

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

// ---------------------------------------------------------------------------
// Cross-CPU user address-space TLB shootdowns (M8, B5: vector `0x3e`)
// ---------------------------------------------------------------------------

/// Above this many pages, [`shoot_down`] reloads `CR3` rather than issuing
/// per-page `invlpg` instructions (about Linux's `tlb_single_page_flush_ceiling`
/// of 33).
pub(in crate::mm) const FULL_FLUSH_PAGES: u64 = 32;

/// Interval at which [`shoot_down`] and `AddressSpace::teardown` warn while
/// waiting on a stalled CPU.
pub(in crate::mm) const STALL_WARN_NS: u64 = 2_000_000_000;

/// Tracks elapsed wait time for [`STALL_WARN_NS`] stall warnings, starting on
/// the first nonzero `time::now_ns()` reading (if a wait begins before clock
/// calibration).
pub(in crate::mm) struct WaitTimer {
    start: u64,
    next_warn: u64,
    warned: bool,
}

impl WaitTimer {
    pub(in crate::mm) fn new() -> Self {
        let start = time::now_ns();
        let next_warn = if start == 0 { 0 } else { start.saturating_add(STALL_WARN_NS) };
        Self { start, next_warn, warned: false }
    }

    /// Return `Some((is_first_warning, elapsed_secs))` whenever the next
    /// [`STALL_WARN_NS`] deadline elapses, advancing the deadline.
    pub(in crate::mm) fn poll(&mut self) -> Option<(bool, u64)> {
        let now = time::now_ns();
        if self.start == 0 {
            if now != 0 {
                self.start = now;
                self.next_warn = now.saturating_add(STALL_WARN_NS);
            }
            return None;
        }
        if now < self.next_warn {
            return None;
        }
        let first = !self.warned;
        self.warned = true;
        self.next_warn = now.saturating_add(STALL_WARN_NS);
        Some((first, now.saturating_sub(self.start) / 1_000_000_000))
    }
}

/// What translations [`shoot_down`] retires in an [`AddressSpace`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::mm) enum Shoot {
    /// Page-aligned user virtual address range `[start, end)`.
    Pages(Range<u64>),
    /// Every non-global translation of the address space (`CR3` reload).
    Full,
}

/// Normalized flush payload shared by [`shoot_down`] and [`on_shootdown_ipi`].
#[derive(Clone, Copy)]
enum Flush {
    Pages { start: u64, end: u64 },
    All,
}

impl Flush {
    /// Validate and normalize `what` once at the caller, returning `None` for
    /// an empty page range and promoting ranges past [`FULL_FLUSH_PAGES`] to
    /// [`Flush::All`].
    fn from_shoot(what: Shoot) -> Option<Self> {
        match what {
            Shoot::Full => Some(Self::All),
            Shoot::Pages(range) => {
                debug_assert!(range.start <= range.end, "tlb::shoot_down inverted range {range:#x?}");
                assert!(range.end <= USER_MAX, "tlb::shoot_down range {range:#x?} exceeds USER_MAX");
                if range.start >= range.end {
                    return None;
                }
                debug_assert!(
                    range.start.is_multiple_of(FRAME_SIZE) && range.end.is_multiple_of(FRAME_SIZE),
                    "tlb::shoot_down range {range:#x?} is not page-aligned"
                );
                if range.end - range.start > FULL_FLUSH_PAGES * FRAME_SIZE {
                    Some(Self::All)
                } else {
                    Some(Self::Pages { start: range.start, end: range.end })
                }
            }
        }
    }

    /// Execute this flush on the current CPU.
    fn apply_local(self) {
        match self {
            Self::All => arch::paging::flush_local(),
            Self::Pages { start, end } => {
                let mut addr = start;
                while addr < end {
                    arch::paging::invalidate(VirtAddr::new(addr).expect("user shootdown page"));
                    addr += FRAME_SIZE;
                }
            }
        }
    }
}

/// Exclusive writer token for the global shootdown board (ARCHITECTURE §9
/// Lock 1a: taken only under Lock 1, with no lock below it held (2 through 10,
/// 9a included)).
struct Board {
    /// Last published even generation; incremented by 1 (odd) while updating
    /// the request fields, then by 1 again (even) once published.
    generation: u64,
}

impl Board {
    fn publish(&mut self, root: u64, flush: Flush) -> u64 {
        debug_assert!(self.generation & 1 == 0, "shootdown board generation must be even between publishes");
        let (start, end, full) = match flush {
            Flush::All => (0, 0, true),
            Flush::Pages { start, end } => (start, end, false),
        };
        let odd = self.generation + 1;
        // Mark update in progress so any concurrent `on_shootdown_ipi` (such as
        // a stale IPI from an earlier shootdown) waits for the coherent fields.
        // The Release fence orders the odd store before the payload stores.
        REQUEST_GEN.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        REQUEST_ROOT.store(root, Ordering::Relaxed);
        REQUEST_START.store(start, Ordering::Relaxed);
        REQUEST_END.store(end, Ordering::Relaxed);
        REQUEST_FULL.store(full, Ordering::Relaxed);
        let published = odd + 1;
        REQUEST_GEN.store(published, Ordering::SeqCst);
        self.generation = published;
        published
    }
}

/// Serializes cross-CPU user TLB shootdowns kernel-wide (Lock 1a).
static SHOOTDOWN: SleepMutex<Board> = SleepMutex::new(Board { generation: 0 });

/// Lock-free request fields read by [`on_shootdown_ipi`]. Odd `REQUEST_GEN`
/// indicates a writer with interrupts off is updating the fields.
static REQUEST_GEN: AtomicU64 = AtomicU64::new(0);
static REQUEST_ROOT: AtomicU64 = AtomicU64::new(0);
static REQUEST_START: AtomicU64 = AtomicU64::new(0);
static REQUEST_END: AtomicU64 = AtomicU64::new(0);
static REQUEST_FULL: AtomicBool = AtomicBool::new(false);

/// Newest shootdown generation each CPU has acknowledged in [`on_shootdown_ipi`].
static ACKED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Full-flush shootdown requests acknowledged across all CPUs (for ktests).
#[cfg(feature = "ktest")]
static FULL_FLUSH_IPIS: AtomicU64 = AtomicU64::new(0);

/// Flush `flush` on this CPU if `space` is loaded here (unless already flushed
/// on this CPU) and return `(here, other_active_cpus)`. Must be called with
/// interrupts off.
fn flush_here(space: &AddressSpace, flush: Flush, already_flushed_cpu: Option<usize>) -> (usize, u64) {
    let here = arch::cpu_index();
    if already_flushed_cpu != Some(here) && arch::paging::active_root() == space.root() {
        flush.apply_local();
    }
    // Order prior PTE stores globally before reading `space.active` (on
    // x86-64 a `SeqCst` load alone does not drain the store buffer).
    fence(Ordering::SeqCst);
    let targets = space.active_mask() & !(1u64 << here);
    (here, targets)
}

/// Send vector `0x3e` ([`arch::idt::TLB_SHOOTDOWN`]) to active target `cpu`,
/// panicking if that CPU is not online.
fn send_shootdown(cpu: usize) -> bool {
    let lapic = smp::online_lapic_id(cpu)
        .unwrap_or_else(|| panic!("tlb::shoot_down target cpu {cpu} is active but not online"));
    arch::apic::send_ipi(lapic, arch::idt::TLB_SHOOTDOWN)
}

/// Handle vector `0x3e` ([`arch::idt::TLB_SHOOTDOWN`]), lock-free with
/// interrupts off: if this CPU's `CR3` matches the published request, flush
/// its range (or reload `CR3`), then store the generation in `ACKED[cpu]`.
pub fn on_shootdown_ipi() {
    let cpu = arch::cpu_index();
    loop {
        let g1 = REQUEST_GEN.load(Ordering::Acquire);
        if g1 == 0 {
            return;
        }
        if g1 & 1 != 0 {
            // The writer runs with interrupts off for a few atomic stores.
            core::hint::spin_loop();
            continue;
        }
        let root = REQUEST_ROOT.load(Ordering::Relaxed);
        let flush = if REQUEST_FULL.load(Ordering::Relaxed) {
            Flush::All
        } else {
            Flush::Pages { start: REQUEST_START.load(Ordering::Relaxed), end: REQUEST_END.load(Ordering::Relaxed) }
        };
        fence(Ordering::Acquire);
        let g2 = REQUEST_GEN.load(Ordering::Relaxed);
        if g1 != g2 {
            continue;
        }
        if ACKED[cpu].load(Ordering::Relaxed) < g1 {
            if arch::paging::active_root().as_u64() == root {
                flush.apply_local();
            }
            #[cfg(feature = "ktest")]
            if matches!(flush, Flush::All) {
                FULL_FLUSH_IPIS.fetch_add(1, Ordering::Relaxed);
            }
            ACKED[cpu].store(g1, Ordering::Release);
        }
        return;
    }
}

/// Retire stale translations of `space` from this CPU and every other CPU that
/// currently has `space` loaded, returning only once every target has flushed
/// (or switched away from `space`).
///
/// Must be called after the updated PTEs are written while `space`'s Lock 1
/// (`AddressSpace::inner`) is still held, with no lock below it held
/// (2 through 10, 9a included) and interrupts enabled (`irq::depth() == 0`).
pub(in crate::mm) fn shoot_down(space: &AddressSpace, what: Shoot) {
    let Some(flush) = Flush::from_shoot(what) else {
        return;
    };
    crate::crash::halt_if_stopping_elsewhere();
    debug_assert!(
        arch::irq::enabled() && arch::irq::depth() == 0,
        "tlb::shoot_down called with interrupts off or a SpinLock held"
    );

    // Fast path: flush this CPU and check whether any other CPU has `space`
    // active, avoiding the global `SHOOTDOWN` board on single-CPU spaces.
    let (flushed_cpu, fast_targets) = {
        let _irq = arch::irq::push_off();
        flush_here(space, flush, None)
    };
    if fast_targets == 0 {
        return;
    }

    let mut board = SHOOTDOWN.lock();
    // Step (1): interrupts off so this thread cannot migrate between flushing
    // itself, reading `active`, publishing the request, and sending the IPIs.
    let (generation, targets, mut sent) = {
        let _irq = arch::irq::push_off();
        let (_, targets) = flush_here(space, flush, Some(flushed_cpu));
        if targets == 0 {
            return;
        }
        let generation = board.publish(space.root().as_u64(), flush);
        let mut sent = 0u64;
        for i in (0..MAX_CPUS).filter(|&i| targets & (1u64 << i) != 0) {
            if send_shootdown(i) {
                sent |= 1u64 << i;
            }
        }
        (generation, targets, sent)
    };

    // Step (2): spin with interrupts enabled so this CPU keeps answering
    // interrupts, shootdowns, and `WAKEUP` IPIs while waiting for targets.
    crate::crash::halt_if_stopping_elsewhere();
    debug_assert!(
        arch::irq::enabled() && arch::irq::depth() == 0,
        "tlb::shoot_down waiting with interrupts off or a SpinLock held"
    );
    let mut timer = WaitTimer::new();
    loop {
        let active = space.active_mask();
        let mut remaining = 0u64;
        for i in (0..MAX_CPUS).filter(|&i| targets & (1u64 << i) != 0) {
            let bit = 1u64 << i;
            if active & bit == 0 || ACKED[i].load(Ordering::Acquire) >= generation {
                continue;
            }
            remaining |= bit;
            if sent & bit == 0 && send_shootdown(i) {
                sent |= bit;
            }
        }
        if remaining == 0 {
            return;
        }
        if let Some((first, secs)) = timer.poll() {
            let unsent = remaining & !sent;
            if first {
                warn!(
                    target: "mm",
                    "cpus {remaining:#x} haven't answered TLB shootdown within 2 s (unsent {unsent:#x}, still waiting)"
                );
            } else {
                warn!(
                    target: "mm",
                    "cpus {remaining:#x} still haven't answered TLB shootdown after {secs} s (unsent {unsent:#x})"
                );
            }
        }
        core::hint::spin_loop();
    }
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

crate::ktest!(
    fn shootdown_across_cpus() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;
        use core::sync::atomic::AtomicU32;
        use lumen_vm::{PAGE_SIZE, Prot};

        use super::{frame, paging};
        use crate::sync::SpinLock;
        use crate::user::{copy_to_user, read_u32_nofault};
        use crate::{kassert, kassert_eq, sched};

        /// Anti-wedge bound for helper spin loops with interrupts off (well
        /// above any host stall, safely below `sched::TEST_DEADLINE_NS` of
        /// 10 s). A spin that hits this bound sets `wedged` so the section
        /// reports an explicit anti-wedge failure rather than a misleading
        /// protocol error.
        const ANTI_WEDGE_NS: u64 = 7_000_000_000;

        fn spin_or_wedge(wedged: &AtomicBool, mut done: impl FnMut() -> bool) -> bool {
            let deadline = time::now_ns().saturating_add(ANTI_WEDGE_NS);
            while !done() {
                if time::now_ns() >= deadline {
                    wedged.store(true, Ordering::Release);
                    return false;
                }
                core::hint::spin_loop();
            }
            true
        }

        fn check_not_wedged(wedged: &AtomicBool) -> crate::ktest::TestResult {
            if wedged.load(Ordering::Acquire) {
                crate::ktest::diag(format_args!("anti-wedge expired: host stalled > 7 s"));
                return Err("anti-wedge expired: host stalled > 7 s");
            }
            Ok(())
        }

        /// Owns a section's release flags and spawned helper threads.
        /// On drop (early `?` return or partial spawn failure), sets every
        /// release flag first and then joins every spawned thread. On the
        /// happy path, [`Self::join`] joins without setting flags.
        struct Helpers {
            flags: Vec<Arc<AtomicBool>>,
            handles: Vec<sched::JoinHandle>,
        }

        impl Helpers {
            fn new(flags: impl IntoIterator<Item = Arc<AtomicBool>>) -> Self {
                Self { flags: flags.into_iter().collect(), handles: Vec::new() }
            }

            fn add(&mut self, handle: sched::JoinHandle) {
                self.handles.push(handle);
            }

            fn join(mut self) {
                for h in self.handles.drain(..) {
                    h.join();
                }
            }
        }

        impl Drop for Helpers {
            fn drop(&mut self) {
                if !self.handles.is_empty() {
                    for f in &self.flags {
                        f.store(true, Ordering::Release);
                    }
                    for h in self.handles.drain(..) {
                        h.join();
                    }
                }
            }
        }

        struct ProbeState {
            primed: [AtomicBool; 3],
            after: AtomicBool,
            stop: Arc<AtomicBool>,
            faults_after: [AtomicU64; 3],
            stale_after: [AtomicU64; 3],
            bad_val: [AtomicBool; 3],
        }

        /// Three pinned probe threads on `targets` probing `page_count` pages
        /// starting at `base_va`, cleaned up via [`Helpers`] on drop.
        struct Probes {
            space: Arc<AddressSpace>,
            target_bits: u64,
            state: Arc<ProbeState>,
            helpers: Helpers,
        }

        impl Probes {
            fn start(
                targets: [usize; 3],
                space: Arc<AddressSpace>,
                name: &'static str,
                base_va: u64,
                page_count: u64,
                sentinel: u32,
            ) -> Result<Self, &'static str> {
                let target_bits = (1u64 << targets[0]) | (1u64 << targets[1]) | (1u64 << targets[2]);
                let stop = Arc::new(AtomicBool::new(false));
                let state = Arc::new(ProbeState {
                    primed: [const { AtomicBool::new(false) }; 3],
                    after: AtomicBool::new(false),
                    stop: stop.clone(),
                    faults_after: [const { AtomicU64::new(0) }; 3],
                    stale_after: [const { AtomicU64::new(0) }; 3],
                    bad_val: [const { AtomicBool::new(false) }; 3],
                });
                let mut helpers = Helpers::new([stop]);
                for (idx, &cpu) in targets.iter().enumerate() {
                    let state = state.clone();
                    let h = sched::spawn_user_pinned_on(cpu, name, space.clone(), move || {
                        for p in 0..page_count {
                            if read_u32_nofault(base_va + p * PAGE_SIZE) != Ok(sentinel ^ (p as u32)) {
                                state.bad_val[idx].store(true, Ordering::Release);
                            }
                        }
                        state.primed[idx].store(true, Ordering::Release);
                        let mut p = 0u64;
                        while !state.stop.load(Ordering::Acquire) {
                            let retired = state.after.load(Ordering::Acquire);
                            let va = base_va + p * PAGE_SIZE;
                            match read_u32_nofault(va) {
                                Ok(v) => {
                                    if v != (sentinel ^ (p as u32)) {
                                        state.bad_val[idx].store(true, Ordering::Release);
                                    }
                                    if retired {
                                        state.stale_after[idx].fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(_) => {
                                    if retired {
                                        state.faults_after[idx].fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            p = (p + 1) % page_count;
                            core::hint::spin_loop();
                        }
                    })
                    .map_err(|_| "spawn probe thread")?;
                    helpers.add(h);
                }
                Ok(Self { space, target_bits, state, helpers })
            }

            fn wait_primed(&self) -> crate::ktest::TestResult {
                kassert!(sched::yield_until(|| (0..3).all(|i| self.state.primed[i].load(Ordering::Acquire))));
                kassert!(sched::yield_until(|| (self.space.active_mask() & self.target_bits) == self.target_bits));
                Ok(())
            }

            fn stop_and_join(self) {
                self.state.stop.store(true, Ordering::Release);
                self.helpers.join();
            }

            fn finish_and_check(self, min_faults: u64) -> crate::ktest::TestResult {
                let state = self.state.clone();
                state.after.store(true, Ordering::Release);
                kassert!(sched::yield_until(|| {
                    (0..3).all(|i| state.faults_after[i].load(Ordering::Relaxed) >= min_faults)
                        || (0..3).any(|i| state.stale_after[i].load(Ordering::Relaxed) > 0)
                }));
                self.stop_and_join();
                for i in 0..3 {
                    kassert!(!state.bad_val[i].load(Ordering::Acquire));
                    kassert_eq!(state.stale_after[i].load(Ordering::Relaxed), 0);
                    kassert!(state.faults_after[i].load(Ordering::Relaxed) >= min_faults);
                }
                Ok(())
            }
        }

        let here = arch::cpu_index();
        let mut targets = [0usize; 3];
        let mut found = 0usize;
        for cpu in 0..MAX_CPUS {
            if cpu != here && smp::online_lapic_id(cpu).is_some() {
                targets[found] = cpu;
                found += 1;
                if found == 3 {
                    break;
                }
            }
        }
        if found < 3 {
            return Err("shootdown_across_cpus requires at least 4 online CPUs");
        }
        let [t0, t1, _] = targets;

        const SENTINEL: u32 = 0x5a5a_c3c3;
        const SINGLE_VA: u64 = 0x2000_0000;
        const BULK_VA: u64 = 0x2010_0000;
        const BULK_PAGES: u64 = FULL_FLUSH_PAGES + 2; // 34 pages (> 32)
        const MPROT_VA: u64 = 0x2018_0000;
        const LOCK_VA: u64 = 0x2020_0000;
        const COW_VA: u64 = 0x2028_0000;
        const SWITCH_VA: u64 = 0x2030_0000;
        const TD_VA: u64 = 0x2040_0000;

        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);

        // 1. Single-page unmap shootdown across three pinned target CPUs.
        {
            space
                .map_eager_with_bytes(SINGLE_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &SENTINEL.to_ne_bytes())
                .map_err(|_| "map single page")?;
            let probes = Probes::start(targets, space.clone(), "tlb-probe", SINGLE_VA, 1, SENTINEL)?;
            probes.wait_primed()?;
            space.munmap(SINGLE_VA, PAGE_SIZE).map_err(|_| "munmap single page")?;
            probes.finish_and_check(64)?;
        }

        // 2. Full flush past `FULL_FLUSH_PAGES` (32) pages across three target CPUs.
        {
            let mut bulk = alloc::vec![0u8; (BULK_PAGES * PAGE_SIZE) as usize];
            for p in 0..BULK_PAGES {
                let off = (p * PAGE_SIZE) as usize;
                bulk[off..off + 4].copy_from_slice(&(SENTINEL ^ (p as u32)).to_ne_bytes());
            }
            space
                .map_eager_with_bytes(BULK_VA, BULK_PAGES * PAGE_SIZE, Prot::READ | Prot::WRITE, &bulk)
                .map_err(|_| "map bulk pages")?;

            let probes = Probes::start(targets, space.clone(), "tlb-bulk", BULK_VA, BULK_PAGES, SENTINEL)?;
            probes.wait_primed()?;
            let full_before = FULL_FLUSH_IPIS.load(Ordering::Relaxed);
            space.munmap(BULK_VA, BULK_PAGES * PAGE_SIZE).map_err(|_| "munmap bulk pages")?;
            // `FULL_FLUSH_IPIS` counts full-flush requests acknowledged in
            // `on_shootdown_ipi`; only assert the full count of 3 if all three
            // still have the space loaded afterwards.
            if (space.active_mask() & probes.target_bits) == probes.target_bits {
                kassert!(FULL_FLUSH_IPIS.load(Ordering::Relaxed) >= full_before + 3);
            }
            probes.finish_and_check(2 * BULK_PAGES)?;
        }

        // 3. `mprotect` narrowing (`RW -> PROT_NONE`) shoots down across all three target CPUs.
        {
            space
                .map_eager_with_bytes(MPROT_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &SENTINEL.to_ne_bytes())
                .map_err(|_| "map mprotect page")?;
            let probes = Probes::start(targets, space.clone(), "tlb-mprot", MPROT_VA, 1, SENTINEL)?;
            probes.wait_primed()?;
            space.mprotect(MPROT_VA, PAGE_SIZE, Prot::NONE).map_err(|_| "mprotect PROT_NONE")?;
            probes.finish_and_check(64)?;
            space.munmap(MPROT_VA, PAGE_SIZE).map_err(|_| "munmap mprotect page")?;
        }

        // 4. Target spinning with an IRQ-off `SpinLock` held defers ack and
        //    frame release until it lets go.
        {
            space
                .map_eager_with_bytes(LOCK_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &SENTINEL.to_ne_bytes())
                .map_err(|_| "map lock page")?;
            let lock_phys = paging::walk_user(space.root(), VirtAddr::new(LOCK_VA).unwrap())
                .mapping()
                .ok_or("lock page not mapped")?
                .phys;
            kassert_eq!(frame::user_count(lock_phys), 1);

            let lock_held = Arc::new(AtomicBool::new(false));
            let release_lock = Arc::new(AtomicBool::new(false));
            let abort = Arc::new(AtomicBool::new(false));
            let wedged = Arc::new(AtomicBool::new(false));
            let unmap_done = Arc::new(AtomicBool::new(false));
            let probe_ok_before = Arc::new(AtomicBool::new(false));
            let probe_fault_after = Arc::new(AtomicBool::new(false));

            // `Helpers` releases both flags before joining any started thread on error.
            let mut helpers = Helpers::new([release_lock.clone(), abort.clone()]);
            helpers.add({
                let lock_held = lock_held.clone();
                let release_lock = release_lock.clone();
                let abort = abort.clone();
                let wedged = wedged.clone();
                let unmap_done = unmap_done.clone();
                let probe_ok_before = probe_ok_before.clone();
                let probe_fault_after = probe_fault_after.clone();
                sched::spawn_user_pinned_on(t0, "tlb-spinlock", space.clone(), move || {
                    probe_ok_before.store(read_u32_nofault(LOCK_VA) == Ok(SENTINEL), Ordering::Release);
                    let guard_lock = SpinLock::new(());
                    {
                        let _g = guard_lock.lock();
                        lock_held.store(true, Ordering::Release);
                        spin_or_wedge(&wedged, || release_lock.load(Ordering::Acquire));
                    }
                    // Interrupts are now back on; the pending `0x3e` IPI runs immediately.
                    spin_or_wedge(&wedged, || unmap_done.load(Ordering::Acquire) || abort.load(Ordering::Acquire));
                    probe_fault_after.store(read_u32_nofault(LOCK_VA).is_err(), Ordering::Release);
                })
                .map_err(|_| "spawn spinlock target")?
            });

            kassert!(sched::yield_until(|| lock_held.load(Ordering::Acquire)));
            let gen_before = REQUEST_GEN.load(Ordering::Acquire);
            helpers.add({
                let space = space.clone();
                let unmap_done = unmap_done.clone();
                sched::spawn_on(t1, "tlb-unmap-lock", move || {
                    let _ = space.munmap(LOCK_VA, PAGE_SIZE);
                    unmap_done.store(true, Ordering::Release);
                })
                .map_err(|_| "spawn unmap thread")?
            });

            // Wait until the shootdown request is published while `t0` still holds the lock.
            kassert!(sched::yield_until(|| {
                let g = REQUEST_GEN.load(Ordering::Acquire);
                g >= gen_before + 2 && (g & 1) == 0
            }));
            let published = REQUEST_GEN.load(Ordering::Acquire);
            kassert!(ACKED[t0].load(Ordering::Acquire) < published);
            kassert!(!unmap_done.load(Ordering::Acquire));
            // The unmapped frame must stay referenced until after `shoot_down` completes.
            kassert!(frame::user_count(lock_phys) >= 1);

            release_lock.store(true, Ordering::Release);
            helpers.join();
            check_not_wedged(&wedged)?;
            kassert_eq!(frame::user_count(lock_phys), 0);
            kassert!(probe_ok_before.load(Ordering::Acquire));
            kassert!(probe_fault_after.load(Ordering::Acquire));
            kassert!(unmap_done.load(Ordering::Acquire));
            kassert!(ACKED[t0].load(Ordering::Acquire) >= published);
        }

        // 5. Break-before-make copy-on-write split: the old read-only leaf is
        //    cleared and shot down BEFORE the writable copy is mapped or the
        //    old frame's count is decremented.
        {
            const OLD_VAL: u32 = 0x1122_3344;
            const NEW_VAL: u32 = 0x5566_7788;
            let parent = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new cow")?);
            parent
                .map_eager_with_bytes(COW_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &OLD_VAL.to_ne_bytes())
                .map_err(|_| "map cow page")?;
            let old_phys = paging::walk_user(parent.root(), VirtAddr::new(COW_VA).unwrap())
                .mapping()
                .ok_or("cow page not mapped")?
                .phys;
            let child = parent.fork_copy().map_err(|_| "fork_copy cow")?;
            kassert_eq!(frame::user_count(old_phys), 2);

            let lock_held = Arc::new(AtomicBool::new(false));
            let release_lock = Arc::new(AtomicBool::new(false));
            let abort = Arc::new(AtomicBool::new(false));
            let wedged = Arc::new(AtomicBool::new(false));
            let split_done = Arc::new(AtomicBool::new(false));
            let split_ok = Arc::new(AtomicBool::new(false));
            let read_before = Arc::new(AtomicU32::new(0));
            let read_after = Arc::new(AtomicU32::new(0));

            let mut helpers = Helpers::new([release_lock.clone(), abort.clone()]);
            helpers.add({
                let lock_held = lock_held.clone();
                let release_lock = release_lock.clone();
                let abort = abort.clone();
                let wedged = wedged.clone();
                let split_done = split_done.clone();
                let read_before = read_before.clone();
                let read_after = read_after.clone();
                sched::spawn_user_pinned_on(t0, "tlb-cow-read", parent.clone(), move || {
                    read_before.store(read_u32_nofault(COW_VA).unwrap_or(0), Ordering::Release);
                    let guard_lock = SpinLock::new(());
                    {
                        let _g = guard_lock.lock();
                        lock_held.store(true, Ordering::Release);
                        spin_or_wedge(&wedged, || release_lock.load(Ordering::Acquire));
                    }
                    spin_or_wedge(&wedged, || split_done.load(Ordering::Acquire) || abort.load(Ordering::Acquire));
                    read_after.store(read_u32_nofault(COW_VA).unwrap_or(0), Ordering::Release);
                })
                .map_err(|_| "spawn cow reader")?
            });

            kassert!(sched::yield_until(|| lock_held.load(Ordering::Acquire)));
            let gen_before = REQUEST_GEN.load(Ordering::Acquire);
            helpers.add({
                let split_done = split_done.clone();
                let split_ok = split_ok.clone();
                sched::spawn_user_pinned_on(t1, "tlb-cow-write", parent.clone(), move || {
                    let ok = copy_to_user(COW_VA, &NEW_VAL.to_ne_bytes()).is_ok();
                    split_ok.store(ok, Ordering::Release);
                    split_done.store(true, Ordering::Release);
                })
                .map_err(|_| "spawn cow writer")?
            });

            // Wait until the CoW split's shootdown is published while `t0` still holds its lock.
            kassert!(sched::yield_until(|| {
                let g = REQUEST_GEN.load(Ordering::Acquire);
                g >= gen_before + 2 && (g & 1) == 0
            }));
            let published = REQUEST_GEN.load(Ordering::Acquire);
            kassert!(ACKED[t0].load(Ordering::Acquire) < published);
            kassert!(!split_done.load(Ordering::Acquire));
            // Break-before-make: the leaf must be unmapped (not yet replaced with
            // the writable copy) and the old frame's count still 2 during shootdown.
            kassert!(paging::walk_user(parent.root(), VirtAddr::new(COW_VA).unwrap()).mapping().is_none());
            kassert_eq!(frame::user_count(old_phys), 2);

            release_lock.store(true, Ordering::Release);
            helpers.join();
            check_not_wedged(&wedged)?;
            kassert!(split_ok.load(Ordering::Acquire));
            kassert_eq!(read_before.load(Ordering::Acquire), OLD_VAL);
            kassert_eq!(read_after.load(Ordering::Acquire), NEW_VAL);
            kassert_eq!(frame::user_count(old_phys), 1);
            kassert!(ACKED[t0].load(Ordering::Acquire) >= published);

            child.teardown();
            parent.teardown();
            kassert_eq!(child.active_mask(), 0);
            kassert_eq!(parent.active_mask(), 0);
        }

        // 6. Target that switches away answers through `active` while `ACKED` is still behind.
        {
            space
                .map_eager_with_bytes(SWITCH_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &SENTINEL.to_ne_bytes())
                .map_err(|_| "map switch page")?;
            let ready = Arc::new(AtomicBool::new(false));
            let abort = Arc::new(AtomicBool::new(false));
            let wedged = Arc::new(AtomicBool::new(false));
            let unmap_done = Arc::new(AtomicBool::new(false));
            let answered_via_active = Arc::new(AtomicBool::new(false));

            let mut helpers = Helpers::new([abort.clone()]);
            helpers.add({
                let ready = ready.clone();
                let abort = abort.clone();
                let wedged = wedged.clone();
                let unmap_done = unmap_done.clone();
                let answered_via_active = answered_via_active.clone();
                sched::spawn_user_pinned_on(t0, "tlb-switch", space.clone(), move || {
                    if read_u32_nofault(SWITCH_VA) != Ok(SENTINEL) {
                        return;
                    }
                    // Disable interrupts so `0x3e` stays pending (cannot update `ACKED[t0]`),
                    // wait for the request to be published, then detach off `space` to clear
                    // `t0`'s bit in `space.active` and verify `shoot_down` returns through `active`.
                    let were_on = arch::irq::disable();
                    let gen_before = REQUEST_GEN.load(Ordering::Acquire);
                    ready.store(true, Ordering::Release);
                    let mut published = 0u64;
                    spin_or_wedge(&wedged, || {
                        if abort.load(Ordering::Acquire) {
                            return true;
                        }
                        let g = REQUEST_GEN.load(Ordering::Acquire);
                        if g >= gen_before + 2 && (g & 1) == 0 {
                            published = g;
                            return true;
                        }
                        false
                    });
                    let detached = sched::detach_user_space();
                    spin_or_wedge(&wedged, || unmap_done.load(Ordering::Acquire) || abort.load(Ordering::Acquire));
                    let ok = published != 0
                        && unmap_done.load(Ordering::Acquire)
                        && ACKED[t0].load(Ordering::Acquire) < published;
                    answered_via_active.store(ok, Ordering::Release);
                    arch::irq::restore(were_on);
                    drop(detached);
                })
                .map_err(|_| "spawn switch target")?
            });

            kassert!(sched::yield_until(|| ready.load(Ordering::Acquire)));
            space.munmap(SWITCH_VA, PAGE_SIZE).map_err(|_| "munmap switch page")?;
            unmap_done.store(true, Ordering::Release);
            helpers.join();
            check_not_wedged(&wedged)?;
            kassert!(answered_via_active.load(Ordering::Acquire));
        }

        // 7. `teardown` waits for `active` to empty while a pinned thread's switch is delayed
        //    by `allow_switch`.
        {
            let td_space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new td")?);
            td_space
                .map_eager_with_bytes(TD_VA, PAGE_SIZE, Prot::READ | Prot::WRITE, &SENTINEL.to_ne_bytes())
                .map_err(|_| "map td page")?;
            let loaded = Arc::new(AtomicBool::new(false));
            let allow_switch = Arc::new(AtomicBool::new(false));
            let teardown_entered = Arc::new(AtomicBool::new(false));
            let teardown_done = Arc::new(AtomicBool::new(false));

            let mut helpers = Helpers::new([allow_switch.clone()]);
            helpers.add({
                let loaded = loaded.clone();
                let allow_switch = allow_switch.clone();
                sched::spawn_user_pinned_on(t0, "tlb-td-hold", td_space.clone(), move || {
                    let _ = read_u32_nofault(TD_VA);
                    loaded.store(true, Ordering::Release);
                    while !allow_switch.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    // Returning from `spawn_user_pinned_on` detaches `td_space` and switches away.
                })
                .map_err(|_| "spawn teardown holder")?
            });

            kassert!(sched::yield_until(|| loaded.load(Ordering::Acquire)));
            kassert!(sched::yield_until(|| (td_space.active_mask() & (1u64 << t0)) == (1u64 << t0)));

            helpers.add({
                let td_space = td_space.clone();
                let teardown_entered = teardown_entered.clone();
                let teardown_done = teardown_done.clone();
                sched::spawn_on(t1, "tlb-teardown", move || {
                    teardown_entered.store(true, Ordering::Release);
                    td_space.teardown();
                    teardown_done.store(true, Ordering::Release);
                })
                .map_err(|_| "spawn teardown runner")?
            });

            kassert!(sched::yield_until(|| teardown_entered.load(Ordering::Acquire)));
            let _ = sched::sleep(20_000_000);
            kassert_eq!(td_space.active_mask() & (1u64 << t0), 1u64 << t0);
            kassert!(!teardown_done.load(Ordering::Acquire));

            allow_switch.store(true, Ordering::Release);
            helpers.join();
            kassert!(teardown_done.load(Ordering::Acquire));
            kassert_eq!(td_space.active_mask(), 0);
        }

        space.teardown();
        kassert_eq!(space.active_mask(), 0);
        Ok(())
    }
);
