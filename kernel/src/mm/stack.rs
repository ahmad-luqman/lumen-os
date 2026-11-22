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
//! pages when it is dropped, and before they are handed out again every CPU
//! flushes them from its TLB ([`super::tlb::sync`], waited for by `new`
//! where it can wait; where it can't, `new` takes other addresses).
//!
//! Nothing here uses the heap, so stacks can be set up right after the
//! kernel's page tables.
//!
//! [`leak`]: KernelStack::leak

use core::fmt;
use core::sync::atomic::{Ordering, compiler_fence};

use log::warn;
use lumen_mm::stacks::{Handle, StackKind, StackRange, StackSlot, StackWindow};

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

/// Addresses for a stack of `size` bytes: the lowest freed ones that fit,
/// once every CPU has flushed them ([`tlb::sync`]), else fresh ones.
fn take(size: u64) -> Option<StackSlot> {
    // A few rounds: a stack freed next to the range while this waited joins
    // it, and its newer generation is waited for too.
    for _ in 0..4 {
        let mut w = WINDOW.lock();
        assert!(w.is_claimed(), "mm::init claims the stack window");
        let flushed = tlb::flushed_through();
        let Some(generation) = w.wait_for(size, flushed) else { return w.take(size, flushed) };
        drop(w);
        // Outside the lock: the other CPUs' interrupts answer this.
        if !tlb::sync(generation) {
            break;
        }
    }
    WINDOW.lock().take(size, tlb::flushed_through())
}

/// Hand `slot` back for reuse, its pages all unmapped (by this CPU, just
/// now): retired at a new generation.
fn give_back(slot: StackSlot) {
    let bottom = slot.range().bottom;
    let retired = tlb::retire();
    if !WINDOW.lock().give_back(slot, retired) {
        warn!(target: "mm", "too many free stack ranges: the stack at {bottom:#x} is never reused");
    }
}

/// Why [`KernelStack::new`] failed (nothing is left mapped or registered).
/// A mapping failure other than running out is a kernel bug and panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// No room left in the stack window.
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
    range: StackRange,
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
        let mut slot = take(size).ok_or(StackError::WindowFull)?;
        let range = slot.range();
        match window::map_fresh(slot.window(), range.span()) {
            Ok(()) => {}
            Err(e) if e.is_exhaustion() => {
                // `map_fresh` unmapped whatever it had mapped.
                give_back(slot);
                return Err(StackError::OutOfMemory);
            }
            Err(e) => panic!("mm: mapping a {} stack at {:#x} failed: {e}", kind.name(), range.bottom),
        }
        // SAFETY: mapped just above; it stays mapped until `drop` retracts it.
        let Some(handle) = (unsafe { stacks::REGISTRY.register(kind, range) }) else {
            // SAFETY: mapped just above; nothing has used it.
            unsafe { window::unmap_and_free(slot.window(), range.span()) };
            give_back(slot);
            return Err(StackError::RegistryFull);
        };
        Ok(KernelStack { range, slot: Some(slot), handle: Some(handle) })
    }

    pub fn range(&self) -> StackRange {
        self.range
    }

    /// Keep the stack mapped and registered forever: for stacks the CPU
    /// points at (the TSS, `rsp`), which must never be dropped. Returns its
    /// range (the type doesn't record that it is leaked); hand only its `top`
    /// to the CPU.
    pub fn leak(self) -> StackRange {
        let range = self.range;
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
        // SAFETY: the stack is ours, and (not leaked) nothing runs on it or
        // points at it.
        unsafe { window::unmap_and_free(slot.window(), self.range.span()) };
        // Unmapped (just above, on this CPU) before it is retired.
        give_back(slot);
    }
}

/// The page-table walk of `virt`, as the stack tests check it.
#[cfg(feature = "ktest")]
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
        // them all returns every frame. Done twice: the second time the
        // stacks come back at the first time's addresses (#300), so not even
        // a page table is left behind.
        use crate::{kassert, kassert_eq};
        let first: alloc::vec::Vec<StackRange> = fill_registry()?.iter().map(KernelStack::range).collect();
        let free0 = super::frame::free_frames();
        let held = fill_registry()?;
        let free1 = super::frame::free_frames();
        kassert!(!held.is_empty());
        kassert!(held.iter().map(KernelStack::range).eq(first.iter().copied()));
        kassert_eq!(free1 + held.len(), free0);
        drop(held);
        kassert_eq!(super::frame::free_frames(), free0);
        // All of them unmapped again.
        kassert!(first.iter().all(|r| mapping(r.bottom).is_none()));
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
        let range = stack.range;
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
        // one's: every CPU flushed the freed pages before they were reused.
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
        // An odd size, so no other stack takes these addresses meanwhile.
        let size = 37 * lumen_mm::FRAME_SIZE;
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
