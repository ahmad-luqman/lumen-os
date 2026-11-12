//! x86-64 architecture support.
//!
//! Everything CPU- or platform-specific lives under `arch/`. The rest of the
//! kernel uses only what [`crate::arch`] re-exports, which is the start of the
//! hardware abstraction layer the ARM64 port (M10) will implement.

pub mod apic;
pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod irq;
pub mod paging;
pub mod pic;
pub mod pit;
pub mod port;
pub mod qemu;
pub mod serial;
pub mod stacks;

/// The earliest setup: serial out, then GDT/TSS and IDT so that any fault from
/// here on produces a crash report.
pub fn init_early() {
    serial::init();
    gdt::init();
    idt::init();
}

/// Make the double-fault, NMI and machine-check handlers run on their own
/// stacks: TSS first, then the IDT, so no gate ever names an empty slot (an
/// NMI or #MC can arrive even with interrupts off).
///
/// # Safety
/// Each top is the top of a mapped stack that is never freed and nothing
/// else uses; one CPU.
pub unsafe fn install_ist_stacks(double_fault: u64, nmi: u64, machine_check: u64) {
    // SAFETY: upheld by the caller.
    unsafe { gdt::set_ist_stacks(double_fault, nmi, machine_check) };
    // The two are plain stores to different statics: keep the compiler from
    // putting the IDT's before the TSS's.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // SAFETY: the slots are filled (just above).
    unsafe { idt::use_ist_stacks() };
}
