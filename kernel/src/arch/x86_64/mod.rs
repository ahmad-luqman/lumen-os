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
pub mod percpu;
pub mod pic;
pub mod pit;
pub mod port;
pub mod qemu;
pub mod serial;
pub mod stacks;

/// The earliest setup on the boot CPU: serial out, then its GDT/TSS, its
/// per-CPU record (GS) and the IDT, so that any fault from here on produces a
/// crash report.
pub fn init_early() {
    serial::init();
    // SAFETY: once, on the boot CPU (index 0), interrupts off.
    unsafe {
        gdt::load(0);
        percpu::set_current(0);
    }
    idt::init();
}

/// The first setup on an application processor, CPU `cpu`: its GDT/TSS, its
/// per-CPU record (GS) and the IDT, in that order, as on the boot CPU.
///
/// # Safety
/// Once, on that CPU, with interrupts off; `cpu` is its own index (no other
/// CPU has it) and [`set_ap_ist_stacks`] has filled its IST slots.
pub unsafe fn init_ap(cpu: usize) {
    // SAFETY: upheld by the caller.
    unsafe {
        gdt::load(cpu);
        percpu::set_current(cpu);
    }
    idt::load();
}

/// Point CPU `cpu`'s IST slots (an application processor's) at its own
/// double-fault, NMI and machine-check stacks, before it starts.
///
/// # Safety
/// Each top is the top of a mapped stack that is never freed and nothing
/// else uses; CPU `cpu` isn't running kernel code yet.
pub unsafe fn set_ap_ist_stacks(cpu: usize, double_fault: u64, nmi: u64, machine_check: u64) {
    assert!(cpu != 0, "the boot CPU's IST stacks go through install_ist_stacks");
    // SAFETY: upheld by the caller.
    unsafe { gdt::set_ist_stacks(cpu, double_fault, nmi, machine_check) };
}

/// Make the double-fault, NMI and machine-check handlers run on their own
/// stacks: TSS first, then the IDT, so no gate ever names an empty slot (an
/// NMI or #MC can arrive even with interrupts off).
///
/// # Safety
/// Each top is the top of a mapped stack that is never freed and nothing
/// else uses; called once, on the boot CPU, before any AP starts.
pub unsafe fn install_ist_stacks(double_fault: u64, nmi: u64, machine_check: u64) {
    // SAFETY: upheld by the caller.
    unsafe { gdt::set_ist_stacks(percpu::index(), double_fault, nmi, machine_check) };
    // The two are plain stores to different statics: keep the compiler from
    // putting the IDT's before the TSS's.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    // SAFETY: the slots are filled (just above).
    unsafe { idt::use_ist_stacks() };
}
