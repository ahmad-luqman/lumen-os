//! Hardware abstraction layer (HAL) boundary.
//!
//! Code outside `arch/` must only use the items re-exported here. Adding a new
//! architecture (M10: ARM64) means providing the same items for it.

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)] // the HAL surface is wider than M0 uses
pub use self::x86_64::{
    cpu::{apic_id, halt_forever, rdtsc, read_cr0, read_cr2, read_cr3, read_cr4, read_rbp, read_rsp, switch_stack},
    gdt, idt,
    idt::TrapFrame,
    init_early, install_ist_stacks, paging, qemu, serial, stacks,
};
