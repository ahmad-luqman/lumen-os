//! Kernel stacks from the allocator, each with an unmapped guard page below
//! it (hard rule 4).
//!
//! [`KernelStack::new`] takes addresses from the stack window
//! ([`paging::STACKS_BASE`] up to [`paging::STACKS_LIMIT`]; the layout is
//! `lumen_mm::stacks`, host-tested), maps fresh frames there and registers
//! the stack in [`stacks::REGISTRY`], which the crash reporter reads to walk
//! backtraces and to name guard-page hits (an overflow, or a stray access). Dropping a stack retracts it
//! from the registry, then unmaps it and frees its frames; stacks the CPU
//! points at (the TSS's IST entries, the running stack) are [`leak`]ed so
//! they can't be dropped. Addresses are never reused.
//!
//! Nothing here uses the heap, so stacks can be set up right after the
//! kernel's page tables.
//!
//! [`leak`]: KernelStack::leak

use core::fmt;
use core::sync::atomic::{Ordering, compiler_fence};

use lumen_mm::stacks::{Handle, StackKind, StackRange, StackWindow};
use lumen_mm::window::Window;

use super::{paging, window};
use crate::arch::stacks;
use crate::sync::SpinLock;

/// Size of each IST stack (#DF, NMI, #MC).
pub const STACK_SIZE: u64 = 64 * 1024;

/// The boot CPU's kernel stack, from `kmain_on_kernel_stack` on (the size
/// Limine was asked for, so nothing gets less room than before).
pub const MAIN_STACK_SIZE: u64 = 128 * 1024;

/// The stack window ([`paging::claim_stack_window`], claimed by [`init`]),
/// which hands each stack its own sub-window.
static WINDOW: SpinLock<Option<StackWindow>> = SpinLock::new(None);

/// Claim the stack window. Called by `mm::init`, after the kernel's page
/// tables; making a stack before that panics.
pub fn init() {
    *WINDOW.lock() = Some(StackWindow::new(paging::claim_stack_window()));
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
    /// The stack's own sub-window (its pages alone, not the guard page):
    /// what lets it map and unmap them.
    window: Window,
    /// `None` only while dropping.
    handle: Option<Handle>,
}

impl KernelStack {
    /// A new stack of `size` bytes (whole pages), registered as `kind`.
    pub fn new(size: u64, kind: StackKind) -> Result<KernelStack, StackError> {
        let taken = WINDOW.lock().as_mut().expect("mm::init claims the stack window").take(size);
        let (range, mut window) = taken.ok_or(StackError::WindowFull)?;
        match window::map_fresh(&mut window, range.span()) {
            Ok(()) => {}
            Err(e) if e.is_exhaustion() => return Err(StackError::OutOfMemory),
            Err(e) => panic!("mm: mapping a {} stack at {:#x} failed: {e}", kind.name(), range.bottom),
        }
        // SAFETY: mapped just above; it stays mapped until `drop` retracts it.
        let Some(handle) = (unsafe { stacks::REGISTRY.register(kind, range) }) else {
            // SAFETY: mapped just above; nothing has used it.
            unsafe { window::unmap_and_free(&mut window, range.span()) };
            return Err(StackError::RegistryFull);
        };
        Ok(KernelStack { range, window, handle: Some(handle) })
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; threads (M2)
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
        // Retract first: the crash reporter must not follow a frame pointer
        // into memory that is going away. It reads the registry only after
        // stopping every other CPU, so a crash during this, on any CPU, sees
        // the stack either registered and mapped, or retracted.
        stacks::REGISTRY.unregister(handle);
        // Keep the compiler from moving the unmapping above the retraction.
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the stack is ours, and (not leaked) nothing runs on it or
        // points at it.
        unsafe { window::unmap_and_free(&mut self.window, self.range.span()) };
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
        // dropping it retracts it and returns its frames (page tables created
        // on the way stay).
        use crate::{kassert, kassert_eq};
        use paging::active_table_count;
        let (free0, tables0) = (super::frame::free_frames(), active_table_count()?);
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
        let (free1, tables1) = (super::frame::free_frames(), active_table_count()?);
        kassert_eq!(free1 + (tables1 - tables0), free0);
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

crate::ktest!(
    fn a_full_registry_leaves_nothing_behind() {
        // Fill the registry with one-page stacks: the next one fails with
        // RegistryFull and leaves nothing mapped or allocated (page tables
        // aside), and dropping them all returns every frame.
        use crate::{kassert, kassert_eq};
        use alloc::vec::Vec;
        use paging::active_table_count;
        let mut held = Vec::with_capacity(lumen_mm::stacks::MAX_STACKS);
        let (free0, tables0) = (super::frame::free_frames(), active_table_count()?);
        let refused = loop {
            match KernelStack::new(lumen_mm::FRAME_SIZE, StackKind::Test) {
                Ok(s) => held.push(s),
                Err(e) => break e,
            }
            if held.len() > lumen_mm::stacks::MAX_STACKS {
                return Err("more stacks than registry slots");
            }
        };
        kassert_eq!(refused, StackError::RegistryFull);
        // The refused stack's addresses come right after the last one's: its
        // pages are unmapped again.
        let last = held.last().ok_or("no stack fit")?.range();
        kassert!(mapping(last.top + lumen_mm::FRAME_SIZE).is_none());
        let (free1, tables1) = (super::frame::free_frames(), active_table_count()?);
        kassert_eq!(free1 + (tables1 - tables0) + held.len(), free0);
        drop(held);
        kassert_eq!(super::frame::free_frames() + (active_table_count()? - tables0), free0);
        Ok(())
    }
);

crate::ktest!(
    fn out_of_frames_is_an_error_that_leaves_nothing_behind() {
        // With fewer free frames than a stack needs, `new` fails with
        // OutOfMemory: nothing registered, and every frame it took returned
        // (page tables it created aside).
        use crate::kassert_eq;
        use alloc::vec::Vec;
        use paging::active_table_count;
        let mut held = Vec::with_capacity(super::frame::free_frames());
        // Two frames: fewer than the four-page stack below needs.
        while super::frame::free_frames() > 2 {
            held.push(super::frame::alloc_owned().ok_or("frames ran out")?);
        }
        let (registered, free, tables) =
            (stacks::REGISTRY.iter().count(), super::frame::free_frames(), active_table_count()?);
        let result = KernelStack::new(4 * lumen_mm::FRAME_SIZE, StackKind::Test).map(|s| s.range());
        let (registered_after, free_after, tables_after) =
            (stacks::REGISTRY.iter().count(), super::frame::free_frames(), active_table_count()?);
        drop(held); // frees them
        kassert_eq!(result, Err(StackError::OutOfMemory));
        kassert_eq!(registered_after, registered);
        kassert_eq!(free_after + (tables_after - tables), free);
        Ok(())
    }
);

crate::ktest!(
    fn a_stack_maps_only_its_own_pages() {
        // A stack's own window is its pages alone: not its guard page (which
        // has no token at all) and not the page above it (the next guard).
        use crate::kassert_eq;
        use lumen_mm::VirtAddr;
        use lumen_mm::paging::Attrs;
        let mut stack =
            KernelStack::new(2 * lumen_mm::FRAME_SIZE, StackKind::Test).map_err(|_| "allocating a stack")?;
        let range = stack.range;
        kassert_eq!(stack.window.range(), range.span());
        let v = |a| VirtAddr::new(a).expect("canonical");
        let mut f = super::frame::alloc_owned().ok_or("out of frames")?;
        for page in [range.guard(), range.top] {
            let (got, back) =
                paging::map_refused(&mut stack.window, v(page), f, Attrs::DATA, "outside the stack's window")?;
            kassert_eq!(got, paging::VmError::NotOwner(v(page)));
            f = back;
        }
        Ok(())
    }
);
