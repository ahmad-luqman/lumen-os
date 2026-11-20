//! x86-64 `syscall`/`sysretq` setup, SMEP/SMAP detection, ring-3 entry, and
//! clean kernel unwind.
//!
//! See `docs/ARCHITECTURE.md` §6 ("Ring 3 entry, `syscall`/`sysretq`, user
//! CPU state and safe copies").
//!
//! - `IA32_STAR = (0x0010 << 48) | (0x0008 << 32)`:
//!   - `syscall` loads `CS = 0x08` (`KERNEL_CODE`), `SS = 0x10` (`KERNEL_DATA`).
//!   - `sysretq` loads `SS = (0x10 + 8) | 3 = 0x1b` (`USER_DATA`),
//!     `CS = (0x10 + 16) | 3 = 0x23` (`USER_CODE`).
//! - `IA32_LSTAR = lumen_syscall_entry`.
//! - `IA32_FMASK = 0x0005_7700` (`TF | IF | DF | IOPL | NT | RF | AC`).
//! - `IA32_KERNEL_GS_BASE = 0` while in ring 0 (swapped with `&CPUS[cpu]`
//!   across `swapgs` when entering/leaving ring 3).
//! - `CR4.FSGSBASE = 0` so ring 3 cannot write `GS_BASE` directly.
//! - `SMEP` (`CPUID(7,0).ebx[7]`) and `SMAP` (`CPUID(7,0).ebx[20]`) are
//!   runtime-detected and enabled in `CR4` when advertised; `LUMEN_SMAP_ENABLED`
//!   gates `stac`/`clac` in assembly so CPUs without SMAP (such as QEMU's
//!   default `-cpu qemu64`) never execute `#UD`.

use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use log::info;

use super::cpu::{cpuid, cpuid_count, rdmsr, read_cr4, write_cr4, wrmsr};
use super::fpu::{CR4_FSGSBASE, USER_MAX};
use super::gdt::{self, KERNEL_CODE, KERNEL_DATA, USER_CODE, USER_DATA};
use super::paging::EFER_NXE;
use super::{irq, percpu};

const IA32_EFER: u32 = 0xc000_0080;
const IA32_STAR: u32 = 0xc000_0081;
const IA32_LSTAR: u32 = 0xc000_0082;
const IA32_FMASK: u32 = 0xc000_0084;

/// `IA32_EFER.SCE` (bit 0): System Call Extensions (`syscall`/`sysretq`).
pub const EFER_SCE: u64 = 1 << 0;

/// `IA32_STAR` value matching Lumen's GDT layout.
pub const STAR_VALUE: u64 = (0x0010u64 << 48) | (0x0008u64 << 32);

/// `IA32_FMASK` value: clears `TF | IF | DF | IOPL | NT | RF | AC` on `syscall`.
pub const FMASK_VALUE: u64 = 0x0005_7700;

/// User-safe `RFLAGS` bits preserved across `sysretq` and `iretq`:
/// `CF | PF | AF | ZF | SF | DF | OF | AC | ID`.
pub const RFLAGS_USER_MASK: u64 = 0x0024_0cd5;

/// Mandatory `RFLAGS` bits forced on every ring-3 return: `IF` (bit 9) and
/// architectural reserved bit 1.
pub const RFLAGS_USER_FIXED: u64 = 0x0202;

/// `CR4.SMEP` (bit 20): Supervisor Mode Execution Prevention.
pub const CR4_SMEP: u64 = 1 << 20;
/// `CR4.SMAP` (bit 21): Supervisor Mode Access Prevention.
pub const CR4_SMAP: u64 = 1 << 21;

const CPUID7_EBX_SMEP: u32 = 1 << 7;
const CPUID7_EBX_SMAP: u32 = 1 << 20;

// Pin GDT selectors and PerCpu offsets assumed by the assembly stubs below.
const _: () = {
    assert!(KERNEL_CODE == 0x08);
    assert!(KERNEL_DATA == 0x10);
    assert!(USER_DATA == 0x1b);
    assert!(USER_CODE == 0x23);
    assert!(percpu::OFFSET_KERNEL_RSP == 8);
    assert!(percpu::OFFSET_USER_RSP_SCRATCH == 16);
    assert!(percpu::OFFSET_USER_EXIT_RSP == 24);
};

/// Published SMAP state readable from assembly (`cmpb $0, LUMEN_SMAP_ENABLED(%rip)`):
/// `1` if `CR4.SMAP` is active, `0` otherwise.
#[unsafe(no_mangle)]
pub static LUMEN_SMAP_ENABLED: AtomicU8 = AtomicU8::new(0);

static SMEP_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether Supervisor Mode Execution Prevention (`CR4.SMEP`) is active.
#[allow(dead_code)] // HAL surface / ktests
pub fn smep_enabled() -> bool {
    SMEP_ENABLED.load(Ordering::Acquire)
}

/// Whether Supervisor Mode Access Prevention (`CR4.SMAP`) is active.
#[allow(dead_code)] // HAL surface / ktests
pub fn smap_enabled() -> bool {
    LUMEN_SMAP_ENABLED.load(Ordering::Acquire) != 0
}

/// Sanitize a user `RFLAGS` value before returning to ring 3 via `sysretq` or
/// `iretq`: retain only user-safe flags, force `IF = 1` and reserved bit 1,
/// and unconditionally clear `TF`, `IOPL`, `NT`, `RF`, `VM`, `VIF`, `VIP`,
/// and all reserved bits.
#[inline]
pub const fn sanitize_user_rflags(rflags: u64) -> u64 {
    (rflags & RFLAGS_USER_MASK) | RFLAGS_USER_FIXED
}

/// Full register frame saved on the kernel stack by `lumen_syscall_entry`.
///
/// The tail (`rip`, `cs`, `rflags`, `rsp`, `ss`) matches the hardware `iretq`
/// frame layout so both `sysretq` (normal syscalls) and `iretq` (rewritten
/// frames) exit cleanly with zero red-zone use and 16-byte stack alignment
/// (`size_of::<SyscallFrame>() == 160`).
#[repr(C)]
#[derive(Debug, Clone)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // Hardware-compatible `iretq` frame:
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

const _: () = {
    assert!(size_of::<SyscallFrame>() == 160);
    assert!(offset_of!(SyscallFrame, r15) == 0);
    assert!(offset_of!(SyscallFrame, rax) == 112);
    assert!(offset_of!(SyscallFrame, rip) == 120);
    assert!(offset_of!(SyscallFrame, rsp) == 144);
    assert!(offset_of!(SyscallFrame, ss) == 152);
};

/// How a completed syscall returns to ring 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyscallReturn {
    /// Normal canonical return via `swapgs; sysretq`.
    Sysret,
    /// Rewritten-frame return via `swapgs; iretq` (restores all 15 GPRs).
    Iret,
}

// Raw assembly entry/exit stubs for `syscall`/`sysretq`, initial ring-3
// `iretq` entry, and clean unwind back to the kernel caller.
//
// Placed in `.text.lumen_syscall` (covered by `*(.text .text.*)` in
// `linker.ld`), with zero SIMD instructions and zero red-zone accesses.
global_asm!(
    r#"
    .pushsection .text.lumen_syscall, "ax", @progbits
    .globl lumen_syscall_entry
    .type lumen_syscall_entry, @function
    .balign 16
lumen_syscall_entry:
    # Entered with IF=0, DF=0, TF=0, AC=0 (cleared by IA32_FMASK).
    # GS_BASE is user GS; RSP is untrusted user RSP.
    swapgs
    movq %rsp, %gs:16           # PerCpu::user_rsp_scratch
    movq %gs:8, %rsp            # PerCpu::kernel_rsp (16-byte aligned)
    # Build hardware-compatible iretq tail (40 bytes) + 15 GPRs (120 bytes) = 160 bytes.
    pushq $0x1b                 # SS = USER_DATA
    pushq %gs:16                # RSP = user_rsp
    pushq %r11                  # RFLAGS (saved by syscall)
    pushq $0x23                 # CS = USER_CODE
    pushq %rcx                  # RIP (saved by syscall)
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    cld
    movq %rsp, %rdi             # arg1: &mut SyscallFrame (rsp is 16-byte aligned)
    xorl %ebp, %ebp             # terminate kernel backtraces at syscall entry
    call lumen_syscall_dispatch
    # Returns with interrupts OFF, frame.rflags sanitized, frame.rip < USER_MAX.
    # rax == 0 => sysretq; rax != 0 => iretq.
    testq %rax, %rax
    jnz 1f
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11                   # sanitized user RFLAGS
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx                   # validated canonical user RIP (< USER_MAX)
    popq %rbx
    popq %rax                   # syscall return value
    movq 24(%rsp), %rsp         # load user RSP from SyscallFrame::rsp (no red zone)
    swapgs
    sysretq
1:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    swapgs
    iretq
    .size lumen_syscall_entry, . - lumen_syscall_entry

    .globl lumen_enter_ring3_call
    .type lumen_enter_ring3_call, @function
    .balign 16
lumen_enter_ring3_call:
    # System V AMD64: rdi = validated rip, rsi = user_rsp, rdx = sanitized rflags.
    # Called with interrupts OFF and IA32_KERNEL_GS_BASE == 0.
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $8, %rsp               # %rsp is now 16-byte aligned
    movq %rdi, %r12             # preserve rip
    movq %rsi, %r13             # preserve user_rsp
    movq %rdx, %r14             # preserve rflags
    movq %rsp, %rdi             # arg1: saved kernel stack top for TSS.rsp0 & PerCpu
    call lumen_set_ring3_stack
    pushq $0x1b                 # SS = USER_DATA
    pushq %r13                  # RSP = user_rsp
    pushq %r14                  # RFLAGS = sanitized rflags
    pushq $0x23                 # CS = USER_CODE
    pushq %r12                  # RIP = validated canonical rip
    # Zero all GPRs so no kernel values leak to ring 3.
    xorl %eax, %eax
    xorl %ebx, %ebx
    xorl %ecx, %ecx
    xorl %edx, %edx
    xorl %esi, %esi
    xorl %edi, %edi
    xorl %ebp, %ebp
    xorl %r8d, %r8d
    xorl %r9d, %r9d
    xorl %r10d, %r10d
    xorl %r11d, %r11d
    xorl %r12d, %r12d
    xorl %r13d, %r13d
    xorl %r14d, %r14d
    xorl %r15d, %r15d
    swapgs
    iretq
    .size lumen_enter_ring3_call, . - lumen_enter_ring3_call

    .globl lumen_user_unwind
    .type lumen_user_unwind, @function
    .balign 16
lumen_user_unwind:
    # Restores the kernel stack and callee-saved registers captured by
    # `lumen_enter_ring3_call` and returns cleanly to its Rust caller.
    cli
    cld
    movq %gs:24, %rsp           # PerCpu::user_exit_rsp
    movq $0, %gs:24             # clear active ring-3 unwind target
    addq $8, %rsp               # undo alignment slot
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    ret
    .size lumen_user_unwind, . - lumen_user_unwind
    .popsection
"#,
    options(att_syntax)
);

unsafe extern "C" {
    fn lumen_syscall_entry();
    fn lumen_enter_ring3_call(rip: u64, user_rsp: u64, rflags: u64);
    fn lumen_user_unwind() -> !;
}

/// Address of `lumen_syscall_entry` (`IA32_LSTAR`).
#[inline]
pub fn entry_addr() -> u64 {
    lumen_syscall_entry as *const () as u64
}

/// Called from `lumen_enter_ring3_call` (interrupts off) to point `TSS.rsp0`,
/// `PerCpu.kernel_rsp`, and `PerCpu.user_exit_rsp` at the saved kernel stack.
#[unsafe(no_mangle)]
extern "C" fn lumen_set_ring3_stack(rsp: u64) {
    let cpu = percpu::index();
    // SAFETY: called on `cpu` with interrupts disabled right before entering
    // ring 3; `rsp` is 16-byte aligned on this thread's live kernel stack.
    unsafe { gdt::set_rsp0(cpu, rsp) };
    percpu::set_kernel_rsp(rsp);
    percpu::set_user_exit_rsp(rsp);
}

/// Rust syscall dispatcher called by `lumen_syscall_entry` with interrupts off.
///
/// Re-enables interrupts via `irq::restore(true)` (respecting
/// `irq::disabled_for_good()`), checks for a deferred `#DB` at `LSTAR`,
/// dispatches the syscall, disables interrupts, sanitizes `RFLAGS`, and
/// validates `rip < USER_MAX` before returning to the `sysretq`/`iretq`
/// assembly epilogue.
#[unsafe(no_mangle)]
extern "C" fn lumen_syscall_dispatch(frame: &mut SyscallFrame) -> u64 {
    let pending_db = percpu::take_pending_debug_trap();
    irq::restore(true);

    if pending_db {
        crate::user::handle_deferred_debug_trap(frame);
    }

    let mode = crate::user::dispatch_syscall(frame);

    let _ = irq::disable();
    frame.rflags = sanitize_user_rflags(frame.rflags);

    // Canonical RIP check BEFORE `swapgs` or `sysretq`/`iretq` (both raise
    // ring-0 `#GP(0)` with user GS loaded if RIP is non-canonical).
    if frame.rip >= USER_MAX {
        crate::user::handle_non_canonical_return(frame.rip);
    }

    match mode {
        SyscallReturn::Sysret => {
            frame.rcx = frame.rip;
            frame.r11 = frame.rflags;
            0
        }
        SyscallReturn::Iret => {
            frame.cs = u64::from(USER_CODE);
            frame.ss = u64::from(USER_DATA);
            1
        }
    }
}

/// Detect `(smep, smap)` support via `CPUID(7, 0).ebx`.
fn detect_smep_smap() -> (bool, bool) {
    if cpuid(0).0 < 7 {
        return (false, false);
    }
    let ebx = cpuid_count(7, 0).1;
    (ebx & CPUID7_EBX_SMEP != 0, ebx & CPUID7_EBX_SMAP != 0)
}

/// Clear `RFLAGS.AC` (only when `CR4.SMAP` is enabled).
///
/// # Safety
/// The CPU must support SMAP (`CPUID(7, 0).ebx[20] == 1`).
unsafe fn clac() {
    // SAFETY: upheld by the caller.
    unsafe { asm!("clac", options(nomem, nostack)) };
}

/// Configure `syscall`/`sysretq` MSRs and optional `SMEP`/`SMAP` on this CPU.
///
/// Called once on the boot CPU (`kmain_on_kernel_stack`, alongside
/// `fpu::init_cpu()`) and once on each AP (`smp::ap_main`).
pub fn init_cpu() {
    let (smep, smap) = detect_smep_smap();

    // Enable EFER.SCE (bit 0) while preserving EFER.NXE (bit 11) and other bits.
    // SAFETY: IA32_EFER exists on every x86-64 CPU; SCE enables `syscall`/`sysretq`.
    unsafe {
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer | EFER_SCE | EFER_NXE);
    }

    // Keep CR4.FSGSBASE = 0; enable SMEP/SMAP when advertised by CPUID.
    let mut cr4 = read_cr4() & !CR4_FSGSBASE;
    if smep {
        cr4 |= CR4_SMEP;
    }
    if smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: only sets CR4.SMEP/CR4.SMAP when CPUID(7,0).ebx advertises them,
    // and keeps CR4.FSGSBASE cleared while preserving all paging/FPU bits.
    unsafe { write_cr4(cr4) };
    if smap {
        // SAFETY: `smap` was verified via CPUID(7,0).ebx[20] just above.
        unsafe { clac() };
    }

    // Program syscall MSRs and initialize IA32_KERNEL_GS_BASE = 0 (user GS).
    // SAFETY: STAR, LSTAR, FMASK, and KERNEL_GS_BASE exist on every x86-64 CPU;
    // `entry_addr()` is our canonical kernel-text syscall entry point.
    unsafe {
        wrmsr(IA32_STAR, STAR_VALUE);
        wrmsr(IA32_LSTAR, entry_addr());
        wrmsr(IA32_FMASK, FMASK_VALUE);
        wrmsr(percpu::IA32_KERNEL_GS_BASE, 0);
    }

    if percpu::index() == 0 {
        SMEP_ENABLED.store(smep, Ordering::Release);
        LUMEN_SMAP_ENABLED.store(u8::from(smap), Ordering::Release);
        info!(target: "syscall", "syscall/sysretq ready (SMEP={smep}, SMAP={smap})");
    } else {
        assert_eq!(smep, smep_enabled(), "AP SMEP support disagrees with boot CPU");
        assert_eq!(smap, smap_enabled(), "AP SMAP support disagrees with boot CPU");
    }
}

/// Enter ring 3 at `(rip, user_rsp)` with sanitized `rflags`, returning when
/// ring 3 exits via [`unwind_to_kernel`].
///
/// # Safety
/// - The active page tables (`CR3`) must map `rip` (user-executable) and
///   `user_rsp` (user-writable) as well as the full kernel upper half.
/// - Interrupts must be enabled and `irq::depth() == 0` (no `SpinLock` held).
/// - No other ring-3 context may already be active on this CPU.
pub unsafe fn enter_ring3(rip: u64, user_rsp: u64, rflags: u64) {
    assert!(rip < USER_MAX, "ring-3 entry rip {rip:#x} is not below USER_MAX");
    assert!(irq::enabled() && irq::depth() == 0, "enter_ring3 requires interrupts on and no SpinLock held");
    assert_eq!(percpu::user_exit_rsp(), 0, "nested enter_ring3 on CPU {}", percpu::index());

    let sanitized = sanitize_user_rflags(rflags);
    let were_on = irq::disable();
    percpu::set_pending_debug_trap(false);
    // SAFETY: `IA32_KERNEL_GS_BASE` exists on every x86-64 CPU; resetting it
    // to 0 ensures ring 3 starts with a clean user GS base after `swapgs`.
    unsafe { wrmsr(percpu::IA32_KERNEL_GS_BASE, 0) };

    // SAFETY: `rip < USER_MAX`, `sanitized` has user-safe RFLAGS with IF=1,
    // interrupts are off, and the caller guarantees valid user CR3 mappings.
    unsafe { lumen_enter_ring3_call(rip, user_rsp, sanitized) };

    // Returned via `lumen_user_unwind` with interrupts off and `user_exit_rsp == 0`.
    let cpu = percpu::index();
    // SAFETY: interrupts are off on `cpu` and no ring-3 context is active.
    unsafe { gdt::set_rsp0(cpu, 0) };
    percpu::set_kernel_rsp(0);
    percpu::set_pending_debug_trap(false);
    irq::restore(were_on);
}

/// Unwind directly back to the caller of [`enter_ring3`] on this CPU.
///
/// # Safety
/// Must be called on the CPU currently executing a ring-3 context started by
/// [`enter_ring3`], with kernel `IA32_GS_BASE` active and no open `SpinLock`
/// critical sections (`irq::depth() == 0`).
pub unsafe fn unwind_to_kernel() -> ! {
    assert_eq!(irq::depth(), 0, "unwind_to_kernel with open critical section");
    assert_ne!(percpu::user_exit_rsp(), 0, "unwind_to_kernel with no active ring-3 context");
    // SAFETY: `user_exit_rsp` holds the saved frame from `lumen_enter_ring3_call`.
    unsafe { lumen_user_unwind() }
}

#[cfg(feature = "ktest")]
fn check_cpu_msrs(efer: u64, star: u64, lstar: u64, fmask: u64, kgs: u64, cr4: u64) -> crate::ktest::TestResult {
    crate::kassert!(efer & EFER_SCE != 0);
    crate::kassert!(efer & EFER_NXE != 0);
    crate::kassert_eq!(star, STAR_VALUE);
    crate::kassert_eq!(lstar, entry_addr());
    crate::kassert_eq!(fmask, FMASK_VALUE);
    crate::kassert_eq!(kgs, 0);
    crate::kassert_eq!(cr4 & CR4_FSGSBASE, 0);
    crate::kassert_eq!(cr4 & CR4_SMEP != 0, smep_enabled());
    crate::kassert_eq!(cr4 & CR4_SMAP != 0, smap_enabled());
    Ok(())
}

crate::ktest!(
    fn syscall_msrs_are_programmed_on_all_cpus() {
        use core::sync::atomic::{AtomicBool, AtomicU64};

        let read_all = || {
            // SAFETY: EFER, STAR, LSTAR, FMASK, KERNEL_GS_BASE exist on every x86-64 CPU.
            unsafe {
                (
                    rdmsr(IA32_EFER),
                    rdmsr(IA32_STAR),
                    rdmsr(IA32_LSTAR),
                    rdmsr(IA32_FMASK),
                    rdmsr(percpu::IA32_KERNEL_GS_BASE),
                    read_cr4(),
                )
            }
        };
        let (efer, star, lstar, fmask, kgs, cr4) = read_all();
        check_cpu_msrs(efer, star, lstar, fmask, kgs, cr4)?;

        // Sanitization checks: IOPL, TF, NT, RF, VM, VIF, VIP stripped; IF & bit 1 forced.
        crate::kassert_eq!(sanitize_user_rflags(0), 0x0202);
        crate::kassert_eq!(sanitize_user_rflags(u64::MAX), RFLAGS_USER_MASK | RFLAGS_USER_FIXED);
        crate::kassert_eq!(sanitize_user_rflags(0x3100), 0x0202); // IOPL=3 | TF=1 cleared

        static AP_VALS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
        static AP_DONE: AtomicBool = AtomicBool::new(false);
        let mut checked_aps = 0usize;
        for cpu in 1..percpu::MAX_CPUS {
            if crate::smp::online_lapic_id(cpu).is_none() {
                continue;
            }
            AP_DONE.store(false, Ordering::Release);
            crate::smp::run_on_test(cpu, || {
                // SAFETY: architectural x86-64 MSRs.
                let vals = unsafe {
                    [
                        rdmsr(IA32_EFER),
                        rdmsr(IA32_STAR),
                        rdmsr(IA32_LSTAR),
                        rdmsr(IA32_FMASK),
                        rdmsr(percpu::IA32_KERNEL_GS_BASE),
                        read_cr4(),
                    ]
                };
                for (slot, val) in AP_VALS.iter().zip(vals) {
                    slot.store(val, Ordering::Relaxed);
                }
                AP_DONE.store(true, Ordering::Release);
            })?;
            crate::kassert!(crate::smp::job_reached(cpu, || AP_DONE.load(Ordering::Acquire)));
            check_cpu_msrs(
                AP_VALS[0].load(Ordering::Relaxed),
                AP_VALS[1].load(Ordering::Relaxed),
                AP_VALS[2].load(Ordering::Relaxed),
                AP_VALS[3].load(Ordering::Relaxed),
                AP_VALS[4].load(Ordering::Relaxed),
                AP_VALS[5].load(Ordering::Relaxed),
            )?;
            checked_aps += 1;
        }
        crate::kassert!(checked_aps > 0);
        Ok(())
    }
);
