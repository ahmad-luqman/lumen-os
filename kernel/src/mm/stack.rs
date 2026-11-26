//! Kernel stacks from the allocator, each with an unmapped guard page below
//! it (hard rule 4).
//!
//! [`KernelStack::new`] takes addresses from the stack window
//! ([`paging::STACKS_BASE`] up to [`paging::STACKS_LIMIT`]; the layout is
//! `lumen_mm::stacks`, host-tested), maps fresh frames there and registers
//! the stack in [`stacks::REGISTRY`], which the crash reporter reads to walk
//! backtraces and to name guard-page hits (an overflow, or a stray access). Dropping a stack retracts it
//! from the registry, then unmaps it and frees its frames; each CPU's main
//! and IST stacks are [`leak`]ed so they can't be dropped, and a thread's
//! stack is dropped only once the thread is dead and its CPU has switched
//! away from it.
//!
//! A dropped stack's addresses are taken again (#300), so steady-state
//! spawning and exiting maps no new page tables: [`super::tlb`] retires its
//! pages when it is dropped, and they are handed out again only once every
//! CPU has flushed them from its TLB (`new` waits for that with
//! [`super::tlb::sync`] where it can; where it can't, it takes other
//! addresses and counts why, [`fallbacks`]).
//!
//! Nothing here uses the heap, so stacks can be set up right after the
//! kernel's page tables.
//!
//! [`leak`]: KernelStack::leak

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, compiler_fence};

use log::{error, warn};
use lumen_mm::stacks::{Handle, Lost, StackKind, StackRange, StackSlot, StackWindow};

use super::{paging, tlb, window};
use crate::arch::stacks;
use crate::sync::SpinLock;

/// Size of each IST stack (#DF, NMI, #MC, #DB) and each thread's stack.
pub const STACK_SIZE: u64 = 64 * 1024;

/// The boot CPU's kernel stack, from `kmain_on_kernel_stack` on (the size
/// Limine was asked for, so nothing gets less room than before).
pub const MAIN_STACK_SIZE: u64 = 128 * 1024;

/// The stack window ([`paging::claim_stack_window`], claimed by [`init`]),
/// which hands each stack its own sub-window and takes freed ones back. Its
/// lock is held only around the window's own (pure) calls, never across
/// the page tables, the frame allocator or [`tlb::sync`].
static WINDOW: SpinLock<StackWindow> = SpinLock::new(StackWindow::empty());

/// Claim the stack window. Called by `mm::init`, after the kernel's page
/// tables; making a stack before that panics.
pub fn init() {
    WINDOW.lock().claim(paging::claim_stack_window());
}

/// Why a new stack skipped the lowest freed range that fits, because not
/// every CPU had flushed it yet, and took other addresses (a higher freed
/// range already flushed, else the untouched rest of the window, which may
/// map a page table): [`fallbacks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// `sync` couldn't wait (interrupts off, a critical section open, or
    /// the clocks not calibrated yet).
    NoWait,
    /// `sync` didn't wait: CPUs are still behind a wait that timed out.
    GaveUp,
    /// `sync` waited 2 s and gave up.
    TimedOut,
    /// The lowest fitting range changed while `sync` ran (or before, with
    /// nothing to wait for): a stack freed meanwhile joined it at a newer
    /// generation, or left a new lower one not flushed yet.
    Raced,
}

impl Fallback {
    pub const ALL: [Fallback; 4] = [Fallback::NoWait, Fallback::GaveUp, Fallback::TimedOut, Fallback::Raced];

    pub fn name(self) -> &'static str {
        match self {
            Fallback::NoWait => "no-wait",
            Fallback::GaveUp => "gave-up",
            Fallback::TimedOut => "timed-out",
            Fallback::Raced => "raced",
        }
    }

    /// Its place in [`Fallback::ALL`] (and [`FALLBACKS`]).
    const fn index(self) -> usize {
        match self {
            Fallback::NoWait => 0,
            Fallback::GaveUp => 1,
            Fallback::TimedOut => 2,
            Fallback::Raced => 3,
        }
    }
}

const _: () = {
    let mut i = 0;
    while i < Fallback::ALL.len() {
        assert!(Fallback::ALL[i].index() == i, "Fallback::ALL is in index order");
        i += 1;
    }
};

/// [`Fallback`]s so far, by reason ([`Fallback::index`]).
static FALLBACKS: [AtomicU64; Fallback::ALL.len()] = [const { AtomicU64::new(0) }; Fallback::ALL.len()];

/// How many new stacks took other addresses for `why` so far (the shell's
/// `mem`).
pub fn fallback_count(why: Fallback) -> u64 {
    FALLBACKS[why.index()].load(Ordering::Relaxed)
}

/// Addresses for a stack of `size` bytes: the lowest freed ones that fit,
/// once every CPU has flushed them ([`tlb::sync`], when they haven't yet);
/// if that can't be waited for, a higher freed range already flushed, else
/// fresh ones ([`Fallback`]).
fn take(size: u64) -> Option<StackSlot> {
    let wait = {
        let w = WINDOW.lock();
        assert!(w.is_claimed(), "mm::init claims the stack window");
        w.wait_for(size, tlb::flushed_through())
    };
    // Outside the lock: the other CPUs' interrupts answer this.
    let synced = wait.map(tlb::sync);
    let (skipped, slot) = {
        let mut w = WINDOW.lock();
        let flushed = tlb::flushed_through();
        (w.wait_for(size, flushed).is_some(), w.take(size, flushed))
    };
    if skipped {
        let why = match synced {
            Some(tlb::Synced::NoWait) => Fallback::NoWait,
            Some(tlb::Synced::GaveUp) => Fallback::GaveUp,
            Some(tlb::Synced::TimedOut) => Fallback::TimedOut,
            Some(tlb::Synced::Done) | None => Fallback::Raced,
        };
        FALLBACKS[why.index()].fetch_add(1, Ordering::Relaxed);
    }
    slot
}

/// Hand `slot` back for reuse, retired at a new generation.
///
/// # Safety
/// Every page of `slot`'s stack is unmapped (by this CPU, before this call).
unsafe fn give_back(slot: StackSlot) {
    let bottom = slot.range().bottom;
    // SAFETY: the caller unmapped every page given back with it.
    let retired = unsafe { tlb::retire() };
    // The lock dropped before logging.
    let kept = WINDOW.lock().give_back(slot, retired);
    match kept {
        Ok(()) => {}
        Err(Lost::Full) => {
            warn!(target: "mm", "too many free stack ranges: the stack at {bottom:#x} is never reused")
        }
        Err(Lost::Changed) => {
            error!(target: "mm", "the stack at {bottom:#x} came back with its window changed: never reused")
        }
    }
}

/// Why [`KernelStack::new`] failed (nothing is left mapped or registered).
/// A mapping failure other than running out is a kernel bug and panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// No room left in the stack window (or only in freed ranges not
    /// every CPU has flushed yet, past its 1 TiB).
    WindowFull,
    /// No frame for the stack or for a page table.
    OutOfMemory,
    /// Every registry slot is taken.
    RegistryFull,
}

impl fmt::Display for StackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StackError::WindowFull => "stack window full",
            StackError::OutOfMemory => "out of memory",
            StackError::RegistryFull => "stack registry full",
        })
    }
}

/// A mapped, registered kernel stack with an unmapped guard page below it.
#[derive(Debug)]
pub struct KernelStack {
    /// The stack's addresses: its own sub-window (its pages alone, not the
    /// guard page) is what lets it map and unmap them. `None` only while
    /// dropping.
    slot: Option<StackSlot>,
    /// `None` only while dropping.
    handle: Option<Handle>,
}

impl KernelStack {
    /// A new stack of `size` bytes (whole pages), registered as `kind`.
    pub fn new(size: u64, kind: StackKind) -> Result<KernelStack, StackError> {
        Self::allocate(size, kind, false)
    }

    /// A user thread's guarded stack, preserving the registry's kernel reserve.
    pub fn new_user(size: u64) -> Result<KernelStack, StackError> {
        Self::allocate(size, StackKind::Thread, true)
    }

    fn allocate(size: u64, kind: StackKind, user: bool) -> Result<KernelStack, StackError> {
        let mut slot = take(size).ok_or(StackError::WindowFull)?;
        let range = slot.range();
        // Hard rule 4 holds for a reused range too: whatever was mapped
        // there was unmapped before it was given back.
        assert!(guard_unmapped(range), "mm: the guard page at {:#x} is mapped", range.guard());
        match window::map_fresh(slot.window(), range.span()) {
            Ok(()) => {}
            Err(e) if e.is_exhaustion() => {
                // SAFETY: `map_fresh` unmapped whatever it had mapped.
                unsafe { give_back(slot) };
                return Err(StackError::OutOfMemory);
            }
            Err(e) => panic!("mm: mapping a {} stack at {:#x} failed: {e}", kind.name(), range.bottom),
        }
        // SAFETY: mapped just above; it stays mapped until `drop` retracts it.
        let handle = unsafe {
            if user { stacks::REGISTRY.register_user(range) } else { stacks::REGISTRY.register(kind, range) }
        };
        let Some(handle) = handle else {
            // SAFETY: mapped just above; nothing has used it.
            unsafe { window::unmap_and_free(slot.window(), range.span()) };
            // SAFETY: unmapped just above.
            unsafe { give_back(slot) };
            return Err(StackError::RegistryFull);
        };
        Ok(KernelStack { slot: Some(slot), handle: Some(handle) })
    }

    pub fn range(&self) -> StackRange {
        self.slot.as_ref().expect("not dropped").range()
    }

    /// Keep the stack mapped and registered forever: for stacks the CPU
    /// points at (the TSS, `rsp`), which must never be dropped. Returns its
    /// range (the type doesn't record that it is leaked); hand only its `top`
    /// to the CPU.
    pub fn leak(self) -> StackRange {
        let range = self.range();
        core::mem::forget(self);
        range
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let handle = self.handle.take().expect("dropped once");
        let mut slot = self.slot.take().expect("dropped once");
        // Retract first: the crash reporter must not follow a frame pointer
        // into memory that is going away. It reads the registry only after
        // stopping the other CPUs, so a crash during this, on any CPU, sees
        // the stack either registered and mapped, or retracted (if every CPU
        // acknowledged the stop; one that didn't is named in the report).
        stacks::REGISTRY.unregister(handle);
        // Keep the compiler from moving the unmapping above the retraction.
        compiler_fence(Ordering::SeqCst);
        let span = slot.range().span();
        // SAFETY: the stack is ours, and (not leaked) nothing runs on it or
        // points at it. Other CPUs may still cache its translations: its
        // addresses are reused only once every CPU has flushed through the
        // generation `give_back` retires it at (`mm::tlb`).
        unsafe { window::unmap_and_free(slot.window(), span) };
        // SAFETY: unmapped just above, on this CPU.
        unsafe { give_back(slot) };
    }
}

/// Is `range`'s guard page unmapped (one walk of the kernel's tables)?
fn guard_unmapped(range: StackRange) -> bool {
    mapping(range.guard()).is_none()
}

/// The page-table walk of `virt` (the guard check, and the stack tests).
fn mapping(virt: u64) -> Option<lumen_mm::paging::Mapping> {
    paging::walk(lumen_mm::VirtAddr::new(virt).expect("canonical")).mapping()
}

/// Is `range` a stack with an unmapped guard page below it, every page a
/// private data page (`paging::is_private_data_page`), and
/// registered as `kind`? `Err` says what is wrong.
#[cfg(feature = "ktest")]
pub fn check_guarded(range: StackRange, kind: StackKind) -> Result<(), &'static str> {
    use lumen_mm::stacks::{Hit, Registered};
    if mapping(range.guard()).is_some() {
        return Err("guard page mapped");
    }
    let private = |page| mapping(page).is_some_and(|m| paging::is_private_data_page(&m));
    if !range.span().step_by(lumen_mm::FRAME_SIZE as usize).all(private) {
        return Err("a stack page is not mapped as a private RW, NX, WB page");
    }
    let registered = Registered { kind, range };
    if stacks::REGISTRY.find(range.bottom) != Some(Hit::Stack(registered)) {
        return Err("stack not registered");
    }
    if stacks::guard_hit(range.guard()) != Some(registered) {
        return Err("guard page not recognized");
    }
    Ok(())
}

crate::ktest!(
    fn guard_page_unmapped() {
        // A new stack: guard page unmapped, every page mapped and usable;
        // dropping it retracts it and returns its frames. A stack of the
        // same size comes back at the addresses the last one had (#300), so
        // with its page tables already there its whole life costs no frame.
        use crate::{kassert, kassert_eq};
        let size = 4 * lumen_mm::FRAME_SIZE;
        let last = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?.range();
        let free0 = super::frame::free_frames();
        let stack = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = stack.range();
        kassert_eq!(range, last);
        kassert_eq!(range.size(), size);
        check_guarded(range, StackKind::Test)?;
        // Usable: write the top and bottom words and read them back.
        let (low, high) = (range.bottom as *mut u64, (range.top - 8) as *mut u64);
        // SAFETY: both words are inside the stack, which this test owns and
        // nothing runs on.
        unsafe {
            low.write_volatile(0x1111);
            high.write_volatile(0x2222);
            kassert_eq!((low.read_volatile(), high.read_volatile()), (0x1111, 0x2222));
        }
        kassert!(stacks::contains(range.bottom, range.size()));
        kassert!(!stacks::contains(range.guard(), 8));
        drop(stack);
        kassert!(mapping(range.bottom).is_none() && mapping(range.top - 8).is_none());
        kassert!(stacks::REGISTRY.find(range.bottom).is_none() && stacks::guard_hit(range.guard()).is_none());
        kassert_eq!(super::frame::free_frames(), free0);
        Ok(())
    }
);

crate::ktest!(
    fn kernel_runs_on_a_guarded_main_stack() {
        // After boot the kernel runs on the allocated main stack, not the
        // one Limine gave it.
        use crate::{kassert, kassert_eq};
        let rsp = crate::arch::read_rsp();
        let main = stacks::REGISTRY.iter().find(|r| r.kind == StackKind::Main).ok_or("no main stack registered")?;
        kassert!(main.range.span().contains(&rsp));
        kassert_eq!(main.range.size(), MAIN_STACK_SIZE);
        check_guarded(main.range, StackKind::Main)?;
        let (boot_lo, boot_hi) = stacks::boot_stack();
        kassert!(!(boot_lo..boot_hi).contains(&rsp));
        Ok(())
    }
);

crate::ktest!(
    fn ist_stacks_are_guarded() {
        // The #DB, #DF, NMI and #MC gates switch to IST stacks, and each slot
        // holds the top of an allocated, registered stack with an unmapped
        // guard page below it.
        use crate::arch::{gdt, idt};
        use crate::{kassert, kassert_eq};
        let ists = [
            (1, gdt::IST_DEBUG, StackKind::Debug),
            (2, gdt::IST_NMI, StackKind::Nmi),
            (8, gdt::IST_DOUBLE_FAULT, StackKind::DoubleFault),
            (18, gdt::IST_MACHINE_CHECK, StackKind::MachineCheck),
        ];
        for (vector, slot, kind) in ists {
            kassert_eq!(idt::ist_index(vector), slot);
            let top = gdt::ist_top(slot);
            let r = stacks::REGISTRY.iter().find(|r| r.range.top == top).ok_or("IST slot is not a registered stack")?;
            kassert_eq!(r.kind, kind);
            kassert_eq!(r.range.size(), STACK_SIZE);
            kassert!(top.is_multiple_of(16));
            check_guarded(r.range, kind)?;
        }
        // No other vector switches stacks.
        kassert!((0..256).filter(|v| ![1, 2, 8, 18].contains(v)).all(|v| idt::ist_index(v) == 0));
        Ok(())
    }
);

/// One-page stacks until the registry refuses one: the stacks, and the
/// refusal (an error if it isn't `RegistryFull`, or never comes).
#[cfg(feature = "ktest")]
fn fill_registry() -> Result<alloc::vec::Vec<KernelStack>, &'static str> {
    let mut held = alloc::vec::Vec::with_capacity(lumen_mm::stacks::MAX_STACKS);
    loop {
        match KernelStack::new(lumen_mm::FRAME_SIZE, StackKind::Test) {
            Ok(s) => held.push(s),
            Err(StackError::RegistryFull) => return Ok(held),
            Err(_) => return Err("a stack failed before the registry was full"),
        }
        if held.len() > lumen_mm::stacks::MAX_STACKS {
            return Err("more stacks than registry slots");
        }
    }
}

crate::ktest!(
    fn a_full_registry_leaves_nothing_behind() {
        // Fill the registry with one-page stacks: the next one fails with
        // RegistryFull and leaves nothing mapped or allocated, and dropping
        // them all returns every frame. Done three times: the first settles
        // the free ranges (it may carve them differently from what earlier
        // tests left), and the third time the stacks come back at the
        // second time's addresses (#300), so not even a page table is left
        // behind.
        use crate::{kassert, kassert_eq};
        drop(fill_registry()?);
        let second: alloc::vec::Vec<StackRange> = fill_registry()?.iter().map(KernelStack::range).collect();
        let free0 = super::frame::free_frames();
        let held = fill_registry()?;
        let free1 = super::frame::free_frames();
        kassert!(!held.is_empty());
        kassert!(held.iter().map(KernelStack::range).eq(second.iter().copied()));
        kassert_eq!(free1 + held.len(), free0);
        drop(held);
        kassert_eq!(super::frame::free_frames(), free0);
        // All of them unmapped again.
        kassert!(second.iter().all(|r| mapping(r.bottom).is_none()));
        Ok(())
    }
);

crate::ktest!(
    fn out_of_frames_is_an_error_that_leaves_nothing_behind() {
        // With fewer free frames than a stack needs, `new` fails with
        // OutOfMemory: nothing registered, and every frame it took returned
        // (its page tables are there already: a stack of the size had its
        // addresses just before).
        use crate::kassert_eq;
        use alloc::vec::Vec;
        let size = 4 * lumen_mm::FRAME_SIZE;
        drop(KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?);
        let mut held = Vec::with_capacity(super::frame::free_frames());
        // Two frames: fewer than the four-page stack below needs.
        while super::frame::free_frames() > 2 {
            held.push(super::frame::alloc_owned().ok_or("frames ran out")?);
        }
        let (registered, free) = (stacks::REGISTRY.iter().count(), super::frame::free_frames());
        let result = KernelStack::new(size, StackKind::Test).map(|s| s.range());
        let (registered_after, free_after) = (stacks::REGISTRY.iter().count(), super::frame::free_frames());
        drop(held); // frees them
        kassert_eq!(result, Err(StackError::OutOfMemory));
        kassert_eq!(registered_after, registered);
        kassert_eq!(free_after, free);
        Ok(())
    }
);

crate::ktest!(
    fn a_stack_maps_only_its_own_pages() {
        // A stack's own window is its pages alone: not its guard page (which
        // the window keeps with the slot, never handed out) and not the page
        // above it (the next guard).
        use crate::kassert_eq;
        use lumen_mm::VirtAddr;
        use lumen_mm::paging::Attrs;
        let mut stack =
            KernelStack::new(2 * lumen_mm::FRAME_SIZE, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = stack.range();
        let window = stack.slot.as_mut().ok_or("no slot")?.window();
        kassert_eq!(window.range(), range.span());
        let v = |a| VirtAddr::new(a).expect("canonical");
        let mut f = super::frame::alloc_owned().ok_or("out of frames")?;
        for page in [range.guard(), range.top] {
            let (got, back) = paging::map_refused(window, v(page), f, Attrs::DATA, "outside the stack's window")?;
            kassert_eq!(got, paging::VmError::NotOwner(v(page)));
            f = back;
        }
        Ok(())
    }
);

crate::ktest!(
    fn spawned_threads_reuse_stacks_and_leak_no_frames() {
        // #300: 100 threads spawned and joined, alternately on this CPU and
        // another, leak no frame (each stack's addresses used to be new, so
        // about every 30th mapped a page table that was never freed), and
        // their stacks all come from the same one or two places.
        use crate::{kassert, kassert_eq};
        use alloc::collections::BTreeSet;
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicU64;
        let here = crate::arch::cpu_index();
        let other = crate::sched::running_cpus().into_iter().find(|&c| c != here).unwrap_or(here);
        let mut bottoms = BTreeSet::new();
        let mut cycle = |cpu: usize| -> crate::ktest::TestResult {
            let bottom = Arc::new(AtomicU64::new(0));
            let b = bottom.clone();
            let h = crate::sched::spawn_on(cpu, "stack-cycle", move || {
                let r = stacks::stack_of(crate::arch::read_rsp()).map_or(0, |r| r.range.bottom);
                b.store(r, Ordering::Relaxed);
            })
            .map_err(|_| "spawn failed")?;
            crate::sched::join_bounded(h)?;
            bottoms.insert(bottom.load(Ordering::Relaxed));
            Ok(())
        };
        // Warm up: the heap's and the thread list's room, the slots' tables.
        for _ in 0..4 {
            cycle(here)?;
            cycle(other)?;
        }
        let free0 = super::frame::free_frames();
        for i in 0..100 {
            cycle(if i % 2 == 0 { here } else { other })?;
        }
        let free1 = super::frame::free_frames();
        crate::ktest::diag(format_args!("{} stack places: {bottoms:x?}", bottoms.len()));
        kassert_eq!(free1, free0);
        kassert!(!bottoms.contains(&0));
        kassert!(bottoms.len() <= 2);
        Ok(())
    }
);

crate::ktest!(
    fn a_reused_stack_is_flushed_from_every_cpu() {
        // #300: another CPU reads a stack's bottom word, caching the page's
        // translation; the stack is freed here (flushing only this CPU's
        // TLB) and its addresses taken again for a new stack on another
        // frame. The other CPU then reads the new frame's word, not the old
        // one's: every CPU flushed the freed pages before they were reused,
        // woken by `sync`'s IPIs alone (the timer's flushes are off here).
        use crate::{kassert, kassert_eq};
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicU8, AtomicU64};
        const OLD: u64 = 0x01d0_01d0_01d0_01d0;
        const NEW: u64 = 0x0e30_0e30_0e30_0e30;
        let here = crate::arch::cpu_index();
        let Some(other) = crate::sched::running_cpus().into_iter().find(|&c| c != here) else {
            crate::ktest::diag(format_args!("one CPU: nothing to flush elsewhere"));
            return Ok(());
        };
        let size = 37 * lumen_mm::FRAME_SIZE;
        // One stack of the size first, untested: its range takes in any
        // smaller free range just below, so `first` comes from the bottom of
        // that and its drop can only join upward. And no other stack may be
        // taken between `first`'s drop and `second` (none is in the ktest
        // kernel): first fit would carve it from these addresses.
        drop(KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?);
        // The timer doesn't flush the other CPUs meanwhile (a `WAKEUP` does).
        let _ticks_off = super::tlb::TickFlushesOff::new();
        let first = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = first.range();
        let word = range.bottom as *mut u64;
        // SAFETY: inside the stack, which this test owns and nothing runs on.
        unsafe { word.write_volatile(OLD) };
        // (phase, first read, second read)
        let shared = Arc::new((AtomicU8::new(0), AtomicU64::new(0), AtomicU64::new(0)));
        let s = shared.clone();
        let reader = crate::sched::spawn_on(other, "tlb-reader", move || {
            let at = range.bottom as *const u64;
            // SAFETY: the test keeps `first` mapped until phase 1 is seen.
            s.1.store(unsafe { at.read_volatile() }, Ordering::SeqCst);
            s.0.store(1, Ordering::SeqCst);
            let deadline = crate::time::now_ns() + 10_000_000_000;
            while s.0.load(Ordering::SeqCst) == 1 && crate::time::now_ns() < deadline {
                core::hint::spin_loop();
            }
            if s.0.load(Ordering::SeqCst) == 2 {
                // SAFETY: in phase 2 the test holds `second` mapped there.
                s.2.store(unsafe { at.read_volatile() }, Ordering::SeqCst);
            }
        })
        .map_err(|_| "spawn failed")?;
        kassert!(crate::sched::yield_until(|| shared.0.load(Ordering::SeqCst) == 1));
        kassert_eq!(shared.1.load(Ordering::SeqCst), OLD);
        let old_frame = mapping(range.bottom).ok_or("the stack's bottom isn't mapped")?.phys;
        drop(first);
        // The old page's frame, kept out of the new stack (frames taken until
        // it comes, if it does), so a stale translation would read it, not
        // the new frame.
        let mut held = alloc::vec::Vec::new();
        while held.len() < 256 {
            let f = super::frame::alloc_owned().ok_or("out of frames")?;
            let found = f.frame() == old_frame;
            held.push(f);
            if found {
                break;
            }
        }
        let second = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?;
        let new_frame = mapping(range.bottom).map(|m| m.phys);
        let reused = second.range() == range;
        if reused {
            // SAFETY: inside the new stack, which this test owns.
            unsafe { word.write_volatile(NEW) };
        }
        // Phase 3: not reused, nothing to read.
        shared.0.store(if reused { 2 } else { 3 }, Ordering::SeqCst);
        crate::sched::join_bounded(reader)?;
        drop(held);
        kassert_eq!(second.range(), range);
        kassert!(new_frame.is_some_and(|f| f != old_frame));
        kassert_eq!(shared.2.load(Ordering::SeqCst), NEW);
        Ok(())
    }
);

crate::ktest!(
    fn a_stack_that_cant_wait_takes_other_addresses_and_counts_it() {
        // #300's fallback: a stack made with interrupts off can't wait for
        // the other CPUs to flush a freed range, so it takes other addresses
        // and counts a no-wait fallback; once every CPU's timer has flushed
        // it, the freed range is reused.
        use crate::{kassert, kassert_eq};
        let here = crate::arch::cpu_index();
        if crate::sched::running_cpus().into_iter().all(|c| c == here) {
            crate::ktest::diag(format_args!("one CPU: every flush is this one's"));
            return Ok(());
        }
        let size = 23 * lumen_mm::FRAME_SIZE;
        let counts = || Fallback::ALL.map(fallback_count);
        // One stack of the size first, untested: its range takes in any
        // smaller free range just below, so the tested one comes from the
        // bottom of that and its drop can only join upward.
        drop(KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?);
        let (freed, before, other, behind) = {
            // The other CPUs stay behind until the timer flushes again.
            let _ticks_off = super::tlb::TickFlushesOff::new();
            let freed = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?.range();
            let before = counts();
            let were_on = crate::arch::irq::disable();
            let other = KernelStack::new(size, StackKind::Test).map(|s| s.range());
            crate::arch::irq::restore(were_on);
            (freed, before, other, super::tlb::behind())
        };
        let other = other.map_err(|_| "allocating a stack with interrupts off")?;
        kassert!(other != freed);
        kassert!(behind != 0);
        let mut expected = before;
        expected[Fallback::NoWait.index()] += 1;
        kassert_eq!(counts(), expected);
        // The timer's flushes are back on: every CPU catches up.
        kassert!(crate::sched::yield_until(|| super::tlb::behind() == 0));
        let before = counts();
        let again = KernelStack::new(size, StackKind::Test).map_err(|_| "allocating a stack")?;
        kassert_eq!(counts(), before);
        kassert_eq!(again.range(), freed);
        Ok(())
    }
);

crate::ktest!(
    fn the_guard_check_looks_at_the_guard_page() {
        // `new`'s assert: true for a live stack's range, false for that
        // range shifted up a page, whose "guard" is the stack's mapped
        // bottom page.
        use crate::kassert;
        let stack = KernelStack::new(2 * lumen_mm::FRAME_SIZE, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = stack.range();
        kassert!(guard_unmapped(range));
        kassert!(!guard_unmapped(StackRange { bottom: range.bottom + lumen_mm::FRAME_SIZE, top: range.top }));
        Ok(())
    }
);
