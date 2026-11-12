//! Per-CPU data: which CPU is this?
//!
//! Every CPU's GS base points at its own [`PerCpu`] record, so
//! [`index`] is one GS-relative load: no lock, no `cpuid`, usable from
//! interrupt and crash paths. Index 0 is the boot CPU; the application
//! processors get 1, 2, ... in the order `smp` starts them. State that
//! belongs to one CPU (its GDT and TSS, interrupt nesting, trace ring) lives
//! in arrays indexed by it.
//!
//! The GS base is set in `arch::init_early` (boot CPU) and `arch::init_ap`,
//! right after the GDT is loaded (loading a segment selector can reset the
//! base) and before the IDT: from then on locks, logging and the crash
//! reporter may run, and all of them ask which CPU they are on.

use core::arch::asm;
use core::mem::offset_of;

use super::cpu::wrmsr;

/// The most CPUs the kernel runs on. More are left parked in the bootloader
/// (with a warning).
pub const MAX_CPUS: usize = 8;

const IA32_GS_BASE: u32 = 0xc000_0101;

/// What GS points at on each CPU.
#[repr(C)]
pub struct PerCpu {
    index: u32,
}

static CPUS: [PerCpu; MAX_CPUS] = {
    let mut cpus = [const { PerCpu { index: 0 } }; MAX_CPUS];
    let mut i = 0;
    while i < MAX_CPUS {
        cpus[i].index = i as u32;
        i += 1;
    }
    cpus
};

/// Make this CPU CPU `index`: point its GS base at `CPUS[index]`.
///
/// # Safety
/// Once per CPU, with a distinct `index` on each (two CPUs claiming one
/// index would share its interrupt nesting and TSS), and after this CPU's
/// GDT is loaded (a later segment load could clear the base).
pub(super) unsafe fn set_current(index: usize) {
    assert!(index < MAX_CPUS, "CPU index {index} out of range");
    // SAFETY: IA32_GS_BASE exists on every x86-64 CPU; the record is a
    // static that lives forever and is only ever read.
    unsafe { wrmsr(IA32_GS_BASE, &raw const CPUS[index] as u64) };
}

/// This CPU's index (0: the boot CPU).
#[inline]
pub fn index() -> usize {
    let index: u32;
    // SAFETY: GS points at this CPU's `PerCpu` (set in `init_early` or
    // `init_ap` before anything that can call this); the load reads its
    // `index` field.
    unsafe {
        asm!(
            "mov {:e}, gs:[{off}]",
            out(reg) index,
            off = const offset_of!(PerCpu, index),
            options(nostack, preserves_flags, readonly)
        )
    };
    index as usize
}

/// This CPU's GS base (for the ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn gs_base() -> u64 {
    // SAFETY: IA32_GS_BASE exists on every x86-64 CPU.
    unsafe { super::cpu::rdmsr(IA32_GS_BASE) }
}

/// Where CPU `index`'s record is (for the ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn record_of(index: usize) -> u64 {
    &raw const CPUS[index] as u64
}

crate::ktest!(
    fn the_boot_cpu_is_cpu_0() {
        crate::kassert_eq!(index(), 0);
        crate::kassert_eq!(gs_base(), record_of(0));
        Ok(())
    }
);
