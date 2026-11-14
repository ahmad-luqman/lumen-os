//! Hardware abstraction layer (HAL) boundary.
//!
//! Code outside `arch/` must only use the items re-exported here. Adding a new
//! architecture (M10: ARM64) means providing the same items for it.

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)] // the HAL surface is wider than the kernel uses yet
pub use self::x86_64::{
    apic, cmos, context,
    cpu::{
        apic_id, enter_kernel_space, halt_forever, rdtsc, read_cr0, read_cr2, read_cr3, read_cr4, read_rbp, read_rsp,
        switch_stack,
    },
    gdt, i8042, idt,
    idt::TrapFrame,
    init_ap, init_early, install_ist_stacks, irq, paging, percpu,
    percpu::{MAX_CPUS, index as cpu_index},
    pic, pit, qemu, serial, set_ap_ist_stacks, stacks,
};
