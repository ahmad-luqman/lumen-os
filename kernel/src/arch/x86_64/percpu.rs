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
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, Ordering};

use crate::mm::user_vm::AddressSpace;

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
    _pad: [u8; 4],
    kernel_rsp: AtomicU64,
    user_rsp_scratch: AtomicU64,
    user_exit_rsp: AtomicU64,
    pending_debug_trap: AtomicDebugTrap,
    /// User [`AddressSpace`] whose page-table root this CPU currently has
    /// loaded in `CR3`, or null when running on `kernel_root()`. Kept alive by
    /// the running (or outgoing, during a switch) thread's `UserThreadState`.
    loaded_space: AtomicPtr<AddressSpace>,
}

/// Pinned byte offsets into [`PerCpu`] for assembly entry/exit stubs.
pub const OFFSET_INDEX: usize = offset_of!(PerCpu, index);
pub const OFFSET_KERNEL_RSP: usize = offset_of!(PerCpu, kernel_rsp);
pub const OFFSET_USER_RSP_SCRATCH: usize = offset_of!(PerCpu, user_rsp_scratch);
pub const OFFSET_USER_EXIT_RSP: usize = offset_of!(PerCpu, user_exit_rsp);

const _: () = {
    assert!(OFFSET_INDEX == 0);
    assert!(OFFSET_KERNEL_RSP == 8);
    assert!(OFFSET_USER_RSP_SCRATCH == 16);
    assert!(OFFSET_USER_EXIT_RSP == 24);
};

static CPUS: [PerCpu; MAX_CPUS] = {
    let mut cpus = [const {
        PerCpu {
            index: 0,
            _pad: [0; 4],
            kernel_rsp: AtomicU64::new(0),
            user_rsp_scratch: AtomicU64::new(0),
            user_exit_rsp: AtomicU64::new(0),
            pending_debug_trap: AtomicDebugTrap::new(),
            loaded_space: AtomicPtr::new(core::ptr::null_mut()),
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
#[inline]
pub fn kernel_rsp() -> u64 {
    CPUS[index()].kernel_rsp.load(Ordering::Relaxed)
}

/// Linux debug si_codes emitted by x86 debug exceptions.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum DebugCode {
    Breakpoint = 1,
    Trace = 2,
}
impl DebugCode {
    pub const fn to_raw(self) -> u8 {
        self as u8
    }
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => None,
            1 => Some(Self::Breakpoint),
            2 => Some(Self::Trace),
            _ => panic!("invalid debug si_code"),
        }
    }
}

/// Where a deferred debug exception was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DebugOrigin {
    /// A ring-3 instruction, waiting for delivery on the thread's kernel stack.
    User = 0,
    /// Kernel syscall entry, before the syscall has executed.
    Lstar = 0x80,
}

/// A deferred debug trap's original address, Linux si_code and origin.
#[derive(Clone, Copy, Debug)]
pub struct DebugTrap {
    pub code: DebugCode,
    pub rip: u64,
    pub origin: DebugOrigin,
}

/// A debug record accessed with interrupts off by its owning CPU/thread.
/// The pair is not a general concurrently writable atomic snapshot.
pub struct AtomicDebugTrap {
    // Bit 7 identifies LSTAR; low bits hold si_code. Zero means no trap.
    code: AtomicU8,
    rip: AtomicU64,
}
impl AtomicDebugTrap {
    pub const fn new() -> Self {
        Self { code: AtomicU8::new(0), rip: AtomicU64::new(0) }
    }
    pub fn store(&self, trap: Option<DebugTrap>) {
        self.rip.store(trap.map_or(0, |t| t.rip), Ordering::Relaxed);
        self.code.store(trap.map_or(0, |t| t.code.to_raw() | t.origin as u8), Ordering::Release);
    }
    pub fn take(&self) -> Option<DebugTrap> {
        let raw = self.code.swap(0, Ordering::AcqRel);
        let origin = if raw & DebugOrigin::Lstar as u8 != 0 { DebugOrigin::Lstar } else { DebugOrigin::User };
        DebugCode::from_raw(raw & !(DebugOrigin::Lstar as u8)).map(|code| DebugTrap {
            code,
            rip: self.rip.load(Ordering::Relaxed),
            origin,
        })
    }
}

/// Record or clear a deferred #DB, with interrupts off on its owning CPU.
#[inline]
pub fn set_pending_debug_trap(trap: Option<DebugTrap>) {
    CPUS[index()].pending_debug_trap.store(trap);
}

/// Take the original RIP, si_code and origin, with interrupts off on the owning CPU.
#[inline]
pub fn take_pending_debug_trap() -> Option<DebugTrap> {
    CPUS[index()].pending_debug_trap.take()
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

/// Read the user [`AddressSpace`] currently loaded in this CPU's `CR3`
/// (null when running on `kernel_root()`). Call with interrupts disabled.
#[inline]
pub(crate) fn loaded_space() -> *const AddressSpace {
    CPUS[index()].loaded_space.load(Ordering::Relaxed).cast_const()
}

/// Record the user [`AddressSpace`] loaded in this CPU's `CR3` (or null for
/// `kernel_root()`).
///
/// # Safety
/// Only `AddressSpace::switch_loaded` may call this, with interrupts disabled,
/// immediately after the matching `CR3` write. Whenever `space` is non-null, it
/// must point to a live `AddressSpace` that stays alive until the next
/// `AddressSpace::switch_loaded` clears or replaces it on this CPU.
#[inline]
pub(crate) unsafe fn set_loaded_space(space: *const AddressSpace) {
    CPUS[index()].loaded_space.store(space.cast_mut(), Ordering::Relaxed);
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
