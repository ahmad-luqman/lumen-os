//! Per-CPU data: CPU index, syscall stack slots, and ring-3 unwind state.
//!
//! Every CPU's `IA32_GS_BASE` points at its own [`PerCpu`] record while in
//! ring 0, so [`index`] is one GS-relative load: no lock, no `cpuid`, usable
//! from interrupt, syscall, and crash paths. Index 0 is the boot CPU; the
//! application processors get 1, 2, ... in the order `smp` starts them.
//!
//! Because [`PerCpu`] holds atomic fields (`kernel_rsp`, `user_rsp_scratch`,
//! `pending_debug_trap`, `user_exit_rsp`), [`CPUS`] lives in writable `.data`
//! rather than `.rodata`. Field offsets read from assembly (`syscall.rs`) are
//! pinned with compile-time `offset_of!` assertions.
//!
//! In ring 3, `IA32_GS_BASE` holds the user GS base (`0` initially) and
//! `IA32_KERNEL_GS_BASE` holds `&CPUS[index]`. Every entry from ring 3
//! (`lumen_syscall_entry`, `lumen_isr_common`, and `lumen_isr_ist_common`)
//! executes `swapgs` before calling into Rust or [`index`].

use core::arch::asm;
use core::mem::offset_of;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::cpu::{rdmsr, wrmsr};

/// The most CPUs the kernel runs on. More are halted for good in
/// `smp::park` (with a warning).
pub const MAX_CPUS: usize = 8;

pub const IA32_GS_BASE: u32 = 0xc000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// What `IA32_GS_BASE` points at on each CPU while in ring 0.
#[repr(C)]
pub struct PerCpu {
    index: u32,
    pending_debug_trap: AtomicU8,
    _pad: [u8; 3],
    kernel_rsp: AtomicU64,
    user_rsp_scratch: AtomicU64,
    user_exit_rsp: AtomicU64,
}

/// Pinned byte offsets into [`PerCpu`] for assembly entry/exit stubs.
pub const OFFSET_INDEX: usize = offset_of!(PerCpu, index);
pub const OFFSET_PENDING_DEBUG_TRAP: usize = offset_of!(PerCpu, pending_debug_trap);
pub const OFFSET_KERNEL_RSP: usize = offset_of!(PerCpu, kernel_rsp);
pub const OFFSET_USER_RSP_SCRATCH: usize = offset_of!(PerCpu, user_rsp_scratch);
pub const OFFSET_USER_EXIT_RSP: usize = offset_of!(PerCpu, user_exit_rsp);

const _: () = {
    assert!(OFFSET_INDEX == 0);
    assert!(OFFSET_PENDING_DEBUG_TRAP == 4);
    assert!(OFFSET_KERNEL_RSP == 8);
    assert!(OFFSET_USER_RSP_SCRATCH == 16);
    assert!(OFFSET_USER_EXIT_RSP == 24);
};

static CPUS: [PerCpu; MAX_CPUS] = {
    let mut cpus = [const {
        PerCpu {
            index: 0,
            pending_debug_trap: AtomicU8::new(0),
            _pad: [0; 3],
            kernel_rsp: AtomicU64::new(0),
            user_rsp_scratch: AtomicU64::new(0),
            user_exit_rsp: AtomicU64::new(0),
        }
    }; MAX_CPUS];
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
/// index would share its interrupt nesting, syscall slots, and TSS), and
/// after this CPU's GDT is loaded (a later segment load could clear the base).
pub(super) unsafe fn set_current(index: usize) {
    assert!(index < MAX_CPUS, "CPU index {index} out of range");
    // SAFETY: IA32_GS_BASE exists on every x86-64 CPU; `CPUS[index]` is a
    // static in writable `.data` that lives forever.
    unsafe { wrmsr(IA32_GS_BASE, &raw const CPUS[index] as u64) };
}

/// This CPU's index (0: the boot CPU).
#[inline]
pub fn index() -> usize {
    let index: u32;
    // SAFETY: GS points at this CPU's `PerCpu` (set in `init_early` or
    // `init_ap` before anything that can call this); the load reads its
    // `index` field. `readonly`, not `pure`: once threads can move between
    // CPUs, two calls may differ, so they must not be merged.
    unsafe {
        asm!(
            "mov {:e}, gs:[{off}]",
            out(reg) index,
            off = const OFFSET_INDEX,
            options(nostack, preserves_flags, readonly)
        )
    };
    index as usize
}

/// Update this CPU's ring-0 syscall kernel stack pointer (`PerCpu::kernel_rsp`).
#[inline]
pub fn set_kernel_rsp(rsp: u64) {
    CPUS[index()].kernel_rsp.store(rsp, Ordering::Relaxed);
}

/// Read this CPU's ring-0 syscall kernel stack pointer (`PerCpu::kernel_rsp`).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
#[inline]
pub fn kernel_rsp() -> u64 {
    CPUS[index()].kernel_rsp.load(Ordering::Relaxed)
}

/// Record or clear a deferred `#DB` trap at `LSTAR` (CVE-2018-8897 handoff).
#[inline]
pub fn set_pending_debug_trap(pending: bool) {
    CPUS[index()].pending_debug_trap.store(u8::from(pending), Ordering::Release);
}

/// Atomically take and clear the deferred `#DB` trap flag for this CPU.
#[inline]
pub fn take_pending_debug_trap() -> bool {
    CPUS[index()].pending_debug_trap.swap(0, Ordering::AcqRel) != 0
}

/// Set the saved kernel stack pointer used to unwind cleanly from ring 3 back
/// to the kernel caller.
#[inline]
pub fn set_user_exit_rsp(rsp: u64) {
    CPUS[index()].user_exit_rsp.store(rsp, Ordering::Relaxed);
}

/// Read the saved kernel unwind stack pointer for ring 3 (0 if not active).
#[inline]
pub fn user_exit_rsp() -> u64 {
    CPUS[index()].user_exit_rsp.load(Ordering::Relaxed)
}

/// This CPU's `IA32_GS_BASE` (for ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn gs_base() -> u64 {
    // SAFETY: IA32_GS_BASE exists on every x86-64 CPU.
    unsafe { rdmsr(IA32_GS_BASE) }
}

/// This CPU's `IA32_KERNEL_GS_BASE` (for ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn kernel_gs_base() -> u64 {
    // SAFETY: IA32_KERNEL_GS_BASE exists on every x86-64 CPU.
    unsafe { rdmsr(IA32_KERNEL_GS_BASE) }
}

/// Where CPU `index`'s record is (for ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn record_of(index: usize) -> u64 {
    &raw const CPUS[index] as u64
}

crate::ktest!(
    fn the_boot_cpu_is_cpu_0() {
        crate::kassert_eq!(index(), 0);
        crate::kassert_eq!(gs_base(), record_of(0));
        crate::kassert_eq!(kernel_gs_base(), 0);
        // Prove CPUS lives in writable `.data` (not `.rodata`) by writing and
        // restoring `kernel_rsp` via this CPU's record.
        let prev = kernel_rsp();
        set_kernel_rsp(0x1234_5678_9abc_def0);
        crate::kassert_eq!(kernel_rsp(), 0x1234_5678_9abc_def0);
        set_kernel_rsp(prev);
        // Also verify TSS.rsp0 round-trip on CPU 0 (interrupts off while writing).
        let _irq = super::irq::push_off();
        let prev_rsp0 = super::gdt::rsp0();
        // SAFETY: interrupts are off on CPU 0 and no ring-3 context is active;
        // we restore `prev_rsp0` immediately.
        unsafe {
            super::gdt::set_rsp0(0, 0xffff_d000_0001_0000);
            crate::kassert_eq!(super::gdt::rsp0(), 0xffff_d000_0001_0000);
            super::gdt::set_rsp0(0, prev_rsp0);
        }
        Ok(())
    }
);
