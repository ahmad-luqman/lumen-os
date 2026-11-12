//! Kernel stacks from the allocator, each with an unmapped guard page below
//! it (hard rule 4).
//!
//! [`KernelStack::new`] takes addresses from the stack window
//! ([`paging::STACKS_BASE`] up to [`paging::STACKS_LIMIT`]; the layout is
//! `lumen_mm::stacks`, host-tested), maps fresh frames there and registers
//! the stack in [`stacks::REGISTRY`], which the crash reporter reads to walk
//! backtraces and to name a stack overflow. Dropping a stack retracts it
//! from the registry, then unmaps it and frees its frames; stacks the CPU
//! points at (the TSS's IST entries, the running stack) are [`leak`]ed so
//! they can't be dropped. Addresses are never reused.
//!
//! Nothing here uses the heap, so stacks can be set up right after the
//! kernel's page tables.
//!
//! [`leak`]: KernelStack::leak

use core::fmt;
use core::ops::Range;

use lumen_mm::stacks::{Handle, StackKind, StackRange, StackWindow};

use super::{paging, window};
use crate::arch::stacks;
use crate::sync::SpinLock;

/// The default stack size: the IST stacks (and the documented size in
/// ARCHITECTURE.md section 5).
pub const STACK_SIZE: u64 = 64 * 1024;

/// The boot CPU's kernel stack, `kmain` onward (the size Limine was asked
/// for, so nothing gets less room than before).
pub const MAIN_STACK_SIZE: u64 = 128 * 1024;

/// What the stack module owns: the stack window minus the ktests' page.
const OWNED: Range<u64> = paging::STACKS_BASE..paging::STACKS_LIMIT;

static WINDOW: SpinLock<StackWindow> = SpinLock::new(StackWindow::new(OWNED.start, OWNED.end));

/// Why [`KernelStack::new`] failed (nothing is left mapped or registered).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// No room left below [`paging::STACKS_LIMIT`].
    WindowFull,
    /// Mapping the stack failed (no frames, or no frame for a page table).
    Map(window::MapError),
    /// Every registry slot is taken.
    RegistryFull,
}

impl fmt::Display for StackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StackError::WindowFull => f.write_str("stack window full"),
            StackError::Map(e) => write!(f, "{e}"),
            StackError::RegistryFull => f.write_str("stack registry full"),
        }
    }
}

/// A mapped, registered kernel stack with an unmapped guard page below it.
#[derive(Debug)]
pub struct KernelStack {
    range: StackRange,
    /// `None` only while dropping.
    handle: Option<Handle>,
}

impl KernelStack {
    /// A new stack of `size` bytes (whole pages), registered as `kind`.
    pub fn new(size: u64, kind: StackKind) -> Result<KernelStack, StackError> {
        let range = WINDOW.lock().take(size).ok_or(StackError::WindowFull)?;
        // SAFETY: this module owns `OWNED`, and the window hands out each
        // range once, so nothing in it is mapped or in use.
        unsafe { window::map_fresh(&OWNED, range.bottom..range.top) }.map_err(StackError::Map)?;
        match stacks::REGISTRY.register(kind, range) {
            Some(handle) => Ok(KernelStack { range, handle: Some(handle) }),
            None => {
                // SAFETY: mapped just above; nothing has used it.
                unsafe { window::unmap_and_free(&OWNED, range.bottom..range.top, None) };
                Err(StackError::RegistryFull)
            }
        }
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; threads (M2)
    pub fn range(&self) -> StackRange {
        self.range
    }

    /// Keep the stack mapped and registered forever: for stacks the CPU
    /// points at (the TSS, `rsp`), which must never be dropped.
    pub fn leak(self) -> StackRange {
        let range = self.range;
        core::mem::forget(self);
        range
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let handle = self.handle.take().expect("dropped once");
        // Retract first: the crash reporter must not follow a frame pointer
        // into memory that is going away.
        stacks::REGISTRY.unregister(handle);
        // SAFETY: the stack is ours, and (not leaked) nothing runs on it or
        // points at it.
        unsafe { window::unmap_and_free(&OWNED, self.range.bottom..self.range.top, None) };
    }
}

/// The page-table walk of `virt`, as the stack tests check it.
#[cfg(feature = "ktest")]
fn mapping(virt: u64) -> Option<lumen_mm::paging::Mapping> {
    paging::walk(lumen_mm::VirtAddr::new(virt).expect("canonical")).mapping()
}

/// Is `range` a stack with an unmapped guard page below it, every page
/// mapped 4 KiB, read-write, no-execute and write-back to an allocated frame,
/// and registered as `kind`? `Err` says what is wrong.
#[cfg(feature = "ktest")]
pub fn check_guarded(range: StackRange, kind: StackKind) -> Result<(), &'static str> {
    use lumen_mm::paging::{Cache, PageSize};
    use lumen_mm::stacks::{Hit, Registered};
    if mapping(range.guard()).is_some() {
        return Err("guard page mapped");
    }
    let ok = |page| {
        mapping(page).is_some_and(|m| {
            m.size == PageSize::Size4K
                && m.writable
                && !m.executable
                && m.cache() == Some(Cache::WriteBack)
                && super::frame::is_allocated(m.phys)
        })
    };
    if !(range.bottom..range.top).step_by(lumen_mm::FRAME_SIZE as usize).all(ok) {
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
        // dropping it retracts it and returns its frames (page tables created
        // on the way stay).
        use crate::{kassert, kassert_eq};
        let tables = || paging::for_each_active_table(|_| {}).map_err(|_| "unreadable table");
        let (free0, tables0) = (super::frame::free_frames(), tables()?);
        let stack = KernelStack::new(4 * lumen_mm::FRAME_SIZE, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = stack.range();
        kassert_eq!(range.size(), 4 * lumen_mm::FRAME_SIZE);
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
        let (free1, tables1) = (super::frame::free_frames(), tables()?);
        kassert_eq!(free1 + (tables1 - tables0), free0);
        Ok(())
    }
);

crate::ktest!(
    fn kernel_runs_on_a_guarded_main_stack() {
        // After boot the kernel runs on the allocated main stack, not the
        // one Limine gave it.
        use crate::kassert;
        let rsp = crate::arch::read_rsp();
        let main = stacks::REGISTRY.iter().find(|r| r.kind == StackKind::Main).ok_or("no main stack registered")?;
        kassert!((main.range.bottom..main.range.top).contains(&rsp));
        kassert!(main.range.size() == MAIN_STACK_SIZE);
        check_guarded(main.range, StackKind::Main)?;
        let (boot_lo, boot_hi) = stacks::boot_stack();
        kassert!(!(boot_lo..boot_hi).contains(&rsp));
        Ok(())
    }
);

crate::ktest!(
    fn ist_stacks_are_guarded() {
        // The #DF, NMI and #MC gates switch to IST stacks, and each slot holds
        // the top of an allocated, registered stack with an unmapped guard
        // page below it.
        use crate::arch::{gdt, idt};
        use crate::{kassert, kassert_eq};
        let ists = [
            (8, gdt::IST_DOUBLE_FAULT, StackKind::DoubleFault),
            (2, gdt::IST_NMI, StackKind::Nmi),
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
        kassert!((0..256).filter(|v| ![2, 8, 18].contains(v)).all(|v| idt::ist_index(v) == 0));
        Ok(())
    }
);
