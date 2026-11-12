//! x86-64 architecture support.
//!
//! Everything CPU- or platform-specific lives under `arch/`. The rest of the
//! kernel uses only what [`crate::arch`] re-exports, which is the start of the
//! hardware abstraction layer the ARM64 port (M10) will implement.

pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod paging;
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
