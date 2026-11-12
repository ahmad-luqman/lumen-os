//! Small CPU helpers.

use core::arch::asm;

/// Halt this CPU forever with interrupts disabled.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: cli/hlt only stop this CPU.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

macro_rules! read_cr {
    ($name:ident, $reg:literal) => {
        pub fn $name() -> u64 {
            let v: u64;
            // SAFETY: reading a control register has no side effects in ring 0.
            unsafe { asm!(concat!("mov {}, ", $reg), out(reg) v, options(nomem, nostack, preserves_flags)) }
            v
        }
    };
}
read_cr!(read_cr0, "cr0");
read_cr!(read_cr2, "cr2");
read_cr!(read_cr3, "cr3");
read_cr!(read_cr4, "cr4");

/// Current frame pointer (valid because we build with frame pointers forced).
#[inline(always)]
pub fn read_rbp() -> u64 {
    let v: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rbp", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

#[allow(dead_code)] // HAL surface
#[inline(always)]
pub fn read_rsp() -> u64 {
    let v: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rsp", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

/// Time-stamp counter. Uncalibrated in M0 (calibration is an M1 timer task).
#[allow(dead_code)] // HAL surface; boot timeline in M1
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: rdtsc is unprivileged and side-effect free.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Read a model-specific register.
///
/// # Safety
/// `msr` must exist on this CPU (reading an unknown MSR raises #GP).
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: the caller guarantees the MSR exists; rdmsr has no other effect.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Write a model-specific register.
///
/// # Safety
/// `msr` must exist and `value` must be valid for it; the effect is whatever
/// that MSR controls.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: delegated to the caller.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nostack, preserves_flags))
    }
}

/// `cpuid` leaf `leaf`, subleaf 0, as `(eax, ebx, ecx, edx)`.
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(leaf, 0);
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// This CPU's APIC id, as the MADT lists it: the 32-bit x2APIC id (`cpuid`
/// leaf 0xB) where the CPU reports one, else the 8-bit initial APIC id
/// (leaf 1).
pub fn apic_id() -> u32 {
    if cpuid(0).0 >= 0xb && cpuid(0xb).1 != 0 { cpuid(0xb).3 } else { cpuid(1).1 >> 24 }
}

macro_rules! write_cr {
    ($name:ident, $reg:literal) => {
        /// Write the control register.
        ///
        /// # Safety
        /// The new value must keep the CPU in a state the kernel can run in
        /// (paging, protection and caching bits).
        pub unsafe fn $name(v: u64) {
            // SAFETY: delegated to the caller. Not `nomem`: changing paging
            // or protection changes what memory accesses mean.
            unsafe { asm!(concat!("mov ", $reg, ", {}"), in(reg) v, options(nostack, preserves_flags)) }
        }
    };
}
write_cr!(write_cr0, "cr0");
write_cr!(write_cr3, "cr3");
write_cr!(write_cr4, "cr4");

/// Continue on another stack: point `rsp` at `top` and call `entry`, which
/// never returns. A frame record (the caller's `rbp` and return address) is
/// planted at the top of the new stack, so the frame-pointer chain runs from
/// `entry` back into the caller's frames on the old stack and backtraces stay
/// whole.
///
/// # Safety
/// `top` is the 16-byte-aligned top of a mapped stack that nothing else uses
/// and that is never freed; the old stack is never reused while that chain
/// can still be walked.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_stack(top: u64, entry: extern "C" fn() -> !) -> ! {
    core::arch::naked_asm!(
        "mov rax, [rsp]", // our return address, into the caller
        "mov rsp, rdi",
        "push rax", // frame record: return address ...
        "push rbp", // ... and the caller's frame pointer
        "mov rbp, rsp",
        "call rsi", // rsp is 16-byte aligned here, as calls require
        "ud2",
    )
}
