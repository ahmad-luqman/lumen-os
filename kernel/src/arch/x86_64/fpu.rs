//! User-thread FP/SSE/AVX state (`FXSAVE64` baseline, optional `XSAVE64`)
//! and `IA32_FS_BASE` thread-local storage helpers.
//!
//! Kernel code runs with SSE/SSE2/x87 disabled (`make audit` proves no vector
//! registers appear anywhere in either kernel image, and allows `fxsave64`/
//! `xsave64` only in `lumen_fpu_save` and `fxrstor64`/`xrstor64` only in
//! `lumen_fpu_restore`). User threads therefore save and restore their FP/SIMD
//! state only across thread switches and signal delivery, never on syscalls or
//! interrupts.
//!
//! See `docs/ARCHITECTURE.md` §6 ("User FP/SSE/AVX state" and "Thread-Local
//! Storage").

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, handle_alloc_error};
use core::arch::{asm, global_asm};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use log::info;

use super::cpu::{cpuid, cpuid_count, rdmsr, read_cr0, read_cr4, write_cr0, write_cr4, wrmsr};

/// CR0.MP (bit 1): monitor coprocessor (`WAIT`/`FWAIT` honour `CR0.TS`).
pub const CR0_MP: u64 = 1 << 1;
/// CR0.EM (bit 2): x87 FPU emulation (cleared so x87/SSE execute natively).
pub const CR0_EM: u64 = 1 << 2;
/// CR0.TS (bit 3): task switched (cleared; we save/restore eagerly on switch).
pub const CR0_TS: u64 = 1 << 3;
/// CR0.NE (bit 5): native numeric error (unmasked x87 errors raise `#MF`
/// vector 16 instead of legacy IRQ 13).
pub const CR0_NE: u64 = 1 << 5;

/// CR4.OSFXSR (bit 9): enable `FXSAVE`/`FXRSTOR` and SSE instructions.
pub const CR4_OSFXSR: u64 = 1 << 9;
/// CR4.OSXMMEXCPT (bit 10): unmasked SIMD FP exceptions raise `#XM` (vector 19).
pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
/// CR4.FSGSBASE (bit 16): kept clear so ring-3 cannot execute `wrfsbase`/
/// `wrgsbase` and bypass `arch_prctl` or kernel `SWAPGS` invariants.
pub const CR4_FSGSBASE: u64 = 1 << 16;
/// CR4.OSXSAVE (bit 18): enable `XSAVE`/`XRSTOR` and `XGETBV`/`XSETBV`.
pub const CR4_OSXSAVE: u64 = 1 << 18;

/// `CPUID(1).ecx` bit 26: CPU supports `XSAVE`/`XRSTOR`/`XGETBV`/`XSETBV`.
const CPUID_ECX_XSAVE: u32 = 1 << 26;

/// `XCR0` bit 0: x87 FPU state (always set when `XSAVE` is active).
const XCR0_X87: u64 = 1 << 0;
/// `XCR0` bit 1: SSE (`XMM0..15` + `MXCSR`).
const XCR0_SSE: u64 = 1 << 1;
/// `XCR0` bit 2: AVX (`YMM_Hi128`).
const XCR0_AVX: u64 = 1 << 2;

/// Legacy `FXSAVE64` buffer size in bytes.
pub const FXSAVE_SIZE: usize = 512;
/// Byte offset of the 64-byte `XSAVE` header (`XSTATE_BV`, `XCOMP_BV`, reserved).
pub const XSAVE_HEADER_OFFSET: usize = 512;
/// Length of the `XSAVE` header in bytes.
pub const XSAVE_HEADER_SIZE: usize = 64;
/// Minimum valid `XSAVE64` save-area size in bytes (512-byte legacy region + 64-byte header).
pub const XSAVE_MIN_SIZE: usize = XSAVE_HEADER_OFFSET + XSAVE_HEADER_SIZE;
/// Buffer alignment in bytes: 64 bytes satisfies both `FXSAVE64` (16-byte)
/// and `XSAVE64`/`XRSTOR64` (64-byte).
pub const FPU_ALIGN: usize = 64;
/// Upper sanity bound on `CPUID(0xD, 0).ebx` for `XCR0` limited to x87+SSE+AVX
/// (576 bytes without AVX, 832 bytes with AVX).
const MAX_XSAVE_SIZE: usize = 16 * 1024;

/// Default x87 floating-point control word (64-bit precision, all exceptions masked).
pub const DEFAULT_FCW: u16 = 0x037F;
/// Default SSE `MXCSR` (round-to-nearest, all SIMD exceptions masked).
pub const DEFAULT_MXCSR: u32 = 0x1F80;

/// Byte offset of `FCW` in the legacy FXSAVE / XSAVE area.
const FCW_OFFSET: usize = 0;
/// Byte offset of `MXCSR` in the legacy FXSAVE / XSAVE area.
const MXCSR_OFFSET: usize = 24;

/// `IA32_FS_BASE` MSR: user thread-local storage base address.
const IA32_FS_BASE: u32 = 0xC000_0100;
/// Exclusive upper bound of the user virtual address space (`docs/ARCHITECTURE.md` §5/§6;
/// the top canonical page `0x0000_7fff_ffff_f000` remains unmapped as a boundary guard).
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;

/// Detected FPU save/restore mechanism on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpuMode {
    /// Baseline x86-64 `FXSAVE64`/`FXRSTOR64` (512-byte area).
    FxSave,
    /// `XSAVE64`/`XRSTOR64` with CPUID-reported area size and active `XCR0`.
    XSave { size: usize, xcr0: u64 },
}

impl FpuMode {
    /// Save-area size in bytes.
    pub const fn area_size(self) -> usize {
        match self {
            FpuMode::FxSave => FXSAVE_SIZE,
            FpuMode::XSave { size, .. } => size,
        }
    }

    /// `XCR0` feature mask to pass to the assembly stubs (`0` selects `FXSAVE64`/`FXRSTOR64`).
    const fn stub_xcr0(self) -> u64 {
        match self {
            FpuMode::FxSave => 0,
            FpuMode::XSave { xcr0, .. } => xcr0,
        }
    }
}

/// Published mode state: 0 = uninitialized, 1 = `FxSave`, 2 = `XSave`.
static MODE_STATE: AtomicU8 = AtomicU8::new(0);
static XSAVE_SIZE: AtomicUsize = AtomicUsize::new(FXSAVE_SIZE);
static XSAVE_XCR0: AtomicU64 = AtomicU64::new(0);

// Raw ELF assembly stubs for user FPU state save/restore.
//
// Defined in `global_asm!` so they lower to module assembly with no LLVM IR
// `define`: `audit.py`'s IR checks remain 100% strict (`-sse,-sse2`), LLVM
// cannot inline them into callers, and `llvm-objdump -d` sees the exact symbol
// names `lumen_fpu_save` and `lumen_fpu_restore`.
//
// Calling convention (System V AMD64):
//   rdi = pointer to 64-byte-aligned save buffer
//   rsi = XCR0 mask (0 => FXSAVE64 / FXRSTOR64; nonzero => XSAVE64 / XRSTOR64)
//
// Neither stub touches any vector/x87/mask register (`%xmm`/`%ymm`/`%zmm`/
// `%mm`/`%k`) or the red zone.
global_asm!(
    r#"
    .pushsection .text.lumen_fpu, "ax", @progbits
    .globl lumen_fpu_save
    .type lumen_fpu_save, @function
    .balign 16
lumen_fpu_save:
    testq %rsi, %rsi
    jz 1f
    movl %esi, %eax
    movq %rsi, %rdx
    shrq $32, %rdx
    xsave64 (%rdi)
    ret
1:
    fxsave64 (%rdi)
    ret
    .size lumen_fpu_save, . - lumen_fpu_save

    .globl lumen_fpu_restore
    .type lumen_fpu_restore, @function
    .balign 16
lumen_fpu_restore:
    testq %rsi, %rsi
    jz 1f
    movl %esi, %eax
    movq %rsi, %rdx
    shrq $32, %rdx
    xrstor64 (%rdi)
    ret
1:
    fxrstor64 (%rdi)
    ret
    .size lumen_fpu_restore, . - lumen_fpu_restore
    .popsection
"#,
    options(att_syntax)
);

unsafe extern "C" {
    /// Save x87/SSE(/AVX) state into `buf` (64-byte aligned, sized for [`mode`]).
    /// `xcr0 == 0` uses `FXSAVE64`; nonzero `xcr0` uses `XSAVE64` with `EDX:EAX = xcr0`.
    fn lumen_fpu_save(buf: *mut u8, xcr0: u64);
    /// Restore x87/SSE(/AVX) state from `buf` (64-byte aligned, sized for [`mode`]).
    /// `xcr0 == 0` uses `FXRSTOR64`; nonzero `xcr0` uses `XRSTOR64` with `EDX:EAX = xcr0`.
    fn lumen_fpu_restore(buf: *const u8, xcr0: u64);
}

/// Write extended control register `index` (`0` = `XCR0`).
///
/// # Safety
/// `CR4.OSXSAVE` must be set and `value` must enable only feature bits
/// supported by this CPU (with bit 0 set).
unsafe fn xsetbv(index: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    // SAFETY: delegated to the caller.
    unsafe {
        asm!(
            "xsetbv",
            in("ecx") index,
            in("eax") lo,
            in("edx") hi,
            options(nostack, preserves_flags),
        );
    }
}

/// Configure `CR0`, `CR4` (and `XCR0` when `XSAVE` is supported) on the
/// current CPU, returning the active [`FpuMode`].
fn configure_registers() -> FpuMode {
    let cr0 = (read_cr0() & !(CR0_EM | CR0_TS)) | CR0_MP | CR0_NE;
    // SAFETY: keeps paging/protection/caching bits unchanged; enables native
    // x87/SSE execution and `#MF` delivery on unmasked x87 errors.
    unsafe { write_cr0(cr0) };

    let base_cr4 = (read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT) & !(CR4_FSGSBASE | CR4_OSXSAVE);
    let has_xsave = cpuid(1).2 & CPUID_ECX_XSAVE != 0 && cpuid(0).0 >= 0xD;
    if !has_xsave {
        // SAFETY: enables `FXSAVE`/`FXRSTOR` and `#XM` while keeping `FSGSBASE`
        // and `OSXSAVE` disabled.
        unsafe { write_cr4(base_cr4) };
        return FpuMode::FxSave;
    }

    // SAFETY: `CPUID(1).ecx[26]` (`XSAVE`) is set, so `CR4.OSXSAVE` is valid.
    unsafe { write_cr4(base_cr4 | CR4_OSXSAVE) };

    let supported_xcr0 = cpuid_count(0xD, 0).0;
    if supported_xcr0 & 0x3 != 0x3 {
        // Defensive fallback if a non-standard CPUID leaf omits x87/SSE bits.
        // SAFETY: `base_cr4` keeps `OSFXSR` and `OSXMMEXCPT` enabled and clears `OSXSAVE`.
        unsafe { write_cr4(base_cr4) };
        return FpuMode::FxSave;
    }

    let xcr0 = (XCR0_X87 | XCR0_SSE) | (u64::from(supported_xcr0) & XCR0_AVX);
    // SAFETY: `CR4.OSXSAVE` is set, `supported_xcr0 & 0x3 == 0x3` was verified
    // above, and bit 2 (`AVX`) is masked by `CPUID(0xD, 0).eax`.
    unsafe { xsetbv(0, xcr0) };

    let size = cpuid_count(0xD, 0).1 as usize;
    if (XSAVE_MIN_SIZE..=MAX_XSAVE_SIZE).contains(&size) && size.is_multiple_of(FPU_ALIGN) {
        FpuMode::XSave { size, xcr0 }
    } else {
        // Unexpected CPUID save-area size: disable OSXSAVE and fall back safely.
        // SAFETY: `base_cr4` leaves `OSFXSR` and `OSXMMEXCPT` enabled and clears `OSXSAVE`.
        unsafe { write_cr4(base_cr4) };
        FpuMode::FxSave
    }
}

/// The FPU save/restore mode detected on the boot CPU.
///
/// Panics if called before [`init_cpu`] has run on the boot CPU.
pub fn mode() -> FpuMode {
    match MODE_STATE.load(Ordering::Acquire) {
        1 => FpuMode::FxSave,
        2 => FpuMode::XSave { size: XSAVE_SIZE.load(Ordering::Relaxed), xcr0: XSAVE_XCR0.load(Ordering::Relaxed) },
        _ => panic!("fpu::mode called before fpu::init_cpu"),
    }
}

/// Initialize FPU/SSE/AVX hardware state on the current CPU.
///
/// Called once on the boot CPU (at the start of `kmain_on_kernel_stack`, once
/// the heap, guarded main stack, and IST stacks are active) and once on each AP
/// (in `smp::ap_main`, once on kernel page tables and its guarded stack). The
/// boot CPU detects and publishes the active [`FpuMode`] and verifies the
/// `lumen_fpu_restore` / `lumen_fpu_save` round-trip; each AP applies the
/// identical `CR0`/`CR4`/`XCR0` setup and loads a clean baseline state.
pub fn init_cpu() {
    let detected = configure_registers();
    if super::percpu::index() == 0 {
        match detected {
            FpuMode::FxSave => {
                XSAVE_SIZE.store(FXSAVE_SIZE, Ordering::Relaxed);
                XSAVE_XCR0.store(0, Ordering::Relaxed);
                MODE_STATE.store(1, Ordering::Release);
                info!(target: "fpu", "FXSAVE64 ({FXSAVE_SIZE} bytes)");
            }
            FpuMode::XSave { size, xcr0 } => {
                XSAVE_SIZE.store(size, Ordering::Relaxed);
                XSAVE_XCR0.store(xcr0, Ordering::Relaxed);
                MODE_STATE.store(2, Ordering::Release);
                info!(target: "fpu", "XSAVE64 ({size} bytes, XCR0 {xcr0:#x})");
            }
        }
        // Load clean initial FPU state on the boot CPU and verify both raw
        // assembly stubs work end-to-end (also keeps both symbols live in both
        // kernels so `make audit` scans them in `lumen-kernel` as well).
        let mut state = UserFpuState::new();
        state.restore();
        state.save();
        assert_eq!(state.fcw(), DEFAULT_FCW, "FPU init round-trip did not preserve FCW");
        assert_eq!(state.mxcsr(), DEFAULT_MXCSR, "FPU init round-trip did not preserve MXCSR");
    } else {
        let expected = mode();
        assert_eq!(detected, expected, "AP FPU mode {detected:?} disagrees with boot CPU {expected:?}");
        UserFpuState::new().restore();
    }
}

/// Standard-format AVX component layout; XCR0 enables no features above AVX.
pub fn signal_xfeature_layout() -> [lumen_signal::frame::XFeature; 1] {
    let (size, offset, _, _) = cpuid_count(0xd, 2);
    [lumen_signal::frame::XFeature { bit: 2, offset: offset as usize, size: size as usize }]
}

/// A user thread's 64-byte-aligned x87/SSE/AVX save area.
///
/// Initialized with clean x86-64 architectural defaults (`FCW = 0x037F`,
/// `MXCSR = 0x1F80`, all registers and extended headers zeroed).
pub struct UserFpuState {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: `UserFpuState` uniquely owns its heap buffer; moving or sharing
// immutable references across CPUs (e.g. thread migration) is sound.
unsafe impl Send for UserFpuState {}
// SAFETY: `&self` only reads the owned buffer; mutation requires `&mut self`.
unsafe impl Sync for UserFpuState {}

impl UserFpuState {
    /// Allocate a fresh save buffer initialized to clean x86-64 defaults.
    pub fn new() -> Self {
        let size = mode().area_size();
        let layout = Layout::from_size_align(size, FPU_ALIGN).expect("valid FPU state layout");
        // SAFETY: `size >= 512` (nonzero) and `FPU_ALIGN == 64` (power of two).
        let raw = unsafe { alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout);
        };
        let mut state = Self { ptr, layout };
        state.write_defaults();
        state
    }

    /// Reset this save buffer to clean x86-64 defaults (e.g. on `execve`).
    #[allow(dead_code)] // HAL surface for M6 execve
    pub fn reset(&mut self) {
        // SAFETY: `self.ptr` is valid for `self.layout.size()` writable bytes.
        unsafe { core::ptr::write_bytes(self.ptr.as_ptr(), 0, self.layout.size()) };
        self.write_defaults();
    }

    fn write_defaults(&mut self) {
        // In `XSAVE` mode, leaving bytes 512..576 zeroed sets `XSTATE_BV = 0`
        // and `XCOMP_BV = 0` (standard, non-compacted format), so `XRSTOR64`
        // initializes x87/XMM/YMM registers to clean architectural defaults
        // while loading `MXCSR` from offset 24.
        let bytes = self.as_bytes_mut_internal();
        bytes[FCW_OFFSET..FCW_OFFSET + 2].copy_from_slice(&DEFAULT_FCW.to_le_bytes());
        bytes[MXCSR_OFFSET..MXCSR_OFFSET + 4].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
    }

    /// Save the current CPU's x87/SSE(/AVX) state into this buffer.
    pub fn save(&mut self) {
        let m = mode();
        debug_assert!(self.layout.size() >= m.area_size());
        // SAFETY: `self.ptr` is 64-byte aligned, writable, and at least
        // `m.area_size()` bytes; `init_cpu` configured `CR0`/`CR4`/`XCR0`.
        unsafe { lumen_fpu_save(self.ptr.as_ptr(), m.stub_xcr0()) };
    }

    /// Restore the current CPU's x87/SSE(/AVX) state from this buffer.
    pub fn restore(&self) {
        let m = mode();
        debug_assert!(self.layout.size() >= m.area_size());
        // SAFETY: `self.ptr` is 64-byte aligned, readable, and holds valid
        // state produced by `new`/`reset`/`save`; `init_cpu` configured
        // `CR0`/`CR4`/`XCR0`.
        unsafe { lumen_fpu_restore(self.ptr.as_ptr(), m.stub_xcr0()) };
    }

    /// Install only frame::restore's validated image, initializing every
    /// enabled feature absent from it. The next restore cannot consume stale
    /// extended state from the signal handler.
    pub fn install_signal(&mut self, fp: Option<lumen_signal::frame::ValidatedFp<'_>>) {
        use lumen_signal::frame::FpKind;
        self.reset();
        if let Some(fp) = fp {
            let bytes = self.as_bytes_mut_internal();
            bytes[..fp.bytes().len()].copy_from_slice(fp.bytes());
            if let FpuMode::XSave { .. } = mode() {
                let bv = match fp.kind() {
                    FpKind::Fxsave => 3,
                    FpKind::Xsave { xfeatures } => {
                        u64::from_le_bytes(
                            bytes[lumen_signal::abi::XSTATE_BV..lumen_signal::abi::XCOMP_BV].try_into().unwrap(),
                        ) & xfeatures
                    }
                };
                bytes[lumen_signal::abi::XSTATE_BV..lumen_signal::abi::XCOMP_BV].copy_from_slice(&bv.to_le_bytes());
                bytes[lumen_signal::abi::XCOMP_BV..lumen_signal::abi::XHEADER_END].fill(0);
            }
        }
        self.restore();
    }

    /// x87 floating-point control word (`FCW`, offset 0).
    pub fn fcw(&self) -> u16 {
        let bytes = self.as_bytes();
        u16::from_le_bytes([bytes[FCW_OFFSET], bytes[FCW_OFFSET + 1]])
    }

    /// x87 status word, including pending exception bits.
    pub fn fsw(&self) -> u16 {
        u16::from_le_bytes(
            self.as_bytes()[core::mem::offset_of!(lumen_signal::abi::FxSave, swd)
                ..core::mem::offset_of!(lumen_signal::abi::FxSave, twd)]
                .try_into()
                .unwrap(),
        )
    }

    /// SSE control/status register (`MXCSR`, offset 24).
    pub fn mxcsr(&self) -> u32 {
        let bytes = self.as_bytes();
        u32::from_le_bytes([
            bytes[MXCSR_OFFSET],
            bytes[MXCSR_OFFSET + 1],
            bytes[MXCSR_OFFSET + 2],
            bytes[MXCSR_OFFSET + 3],
        ])
    }

    /// Read-only view of the raw save buffer.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self.ptr` is valid for `self.layout.size()` initialized bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    /// Backing buffer pointer (always 64-byte aligned).
    #[allow(dead_code)] // HAL surface / ktests
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Save-area size in bytes.
    #[allow(dead_code)] // HAL surface / ktests
    pub fn len(&self) -> usize {
        self.layout.size()
    }

    /// Always false (the save area is at least 512 bytes).
    #[allow(dead_code)] // pairs with `len`
    pub fn is_empty(&self) -> bool {
        self.layout.size() == 0
    }

    fn as_bytes_mut_internal(&mut self) -> &mut [u8] {
        // SAFETY: `self.ptr` is uniquely owned and valid for `self.layout.size()` bytes.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Default for UserFpuState {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for UserFpuState {
    fn clone(&self) -> Self {
        // SAFETY: `self.layout` is the valid nonzero layout used to allocate `self`;
        // every byte is immediately initialized by `copy_nonoverlapping` below.
        let raw = unsafe { alloc(self.layout) };
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(self.layout);
        };
        // SAFETY: `self.ptr` and `ptr` are non-overlapping allocations of `self.layout.size()` bytes.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), ptr.as_ptr(), self.layout.size()) };
        Self { ptr, layout: self.layout }
    }
}

impl Drop for UserFpuState {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was allocated with the global allocator using `self.layout`.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// Read the current CPU's `IA32_FS_BASE` MSR (`0xC000_0100`).
#[allow(dead_code)] // HAL surface for M6 TLS / context switch
pub fn read_fs_base() -> u64 {
    // SAFETY: `IA32_FS_BASE` exists on every x86-64 CPU.
    unsafe { rdmsr(IA32_FS_BASE) }
}

/// Write the current CPU's `IA32_FS_BASE` MSR (`0xC000_0100`).
///
/// Panics if `addr >= USER_MAX` (`0x0000_7fff_ffff_f000`, per
/// `docs/ARCHITECTURE.md` §5/§6; `0` is allowed).
///
/// # Safety
/// The caller must ensure changing `FS` base is valid for the currently
/// running thread / user context.
#[allow(dead_code)] // HAL surface for M6 TLS / context switch
pub unsafe fn write_fs_base(addr: u64) {
    assert!(addr < USER_MAX, "IA32_FS_BASE {addr:#x} is not below USER_MAX ({USER_MAX:#x})");
    // SAFETY: `IA32_FS_BASE` exists on every x86-64 CPU, and `addr < USER_MAX`
    // guarantees a canonical lower-half address so `wrmsr` cannot raise `#GP(0)`.
    unsafe { wrmsr(IA32_FS_BASE, addr) };
}

#[cfg(feature = "ktest")]
fn check_cr0_cr4(cr0: u64, cr4: u64, active: FpuMode) -> crate::ktest::TestResult {
    crate::kassert!(cr0 & CR0_NE != 0);
    crate::kassert_eq!(cr0 & CR0_EM, 0);
    crate::kassert_eq!(cr0 & CR0_TS, 0);
    crate::kassert!(cr0 & CR0_MP != 0);
    crate::kassert!(cr4 & CR4_OSFXSR != 0);
    crate::kassert!(cr4 & CR4_OSXMMEXCPT != 0);
    crate::kassert_eq!(cr4 & CR4_FSGSBASE, 0);
    match active {
        FpuMode::FxSave => crate::kassert_eq!(cr4 & CR4_OSXSAVE, 0),
        FpuMode::XSave { .. } => crate::kassert!(cr4 & CR4_OSXSAVE != 0),
    }
    Ok(())
}

crate::ktest!(
    fn save_restore_roundtrip_preserves_control_words() {
        use core::sync::atomic::AtomicBool;

        let active = mode();
        match active {
            FpuMode::FxSave => {
                crate::kassert_eq!(active.area_size(), FXSAVE_SIZE);
            }
            FpuMode::XSave { size, xcr0 } => {
                crate::kassert!(size >= XSAVE_MIN_SIZE && size.is_multiple_of(FPU_ALIGN));
                crate::kassert!(xcr0 & (XCR0_X87 | XCR0_SSE) == (XCR0_X87 | XCR0_SSE));
            }
        }

        // Verify CR0/CR4 on the boot CPU and across every online AP.
        check_cr0_cr4(read_cr0(), read_cr4(), active)?;
        static AP_CR0: AtomicU64 = AtomicU64::new(0);
        static AP_CR4: AtomicU64 = AtomicU64::new(0);
        static AP_DONE: AtomicBool = AtomicBool::new(false);
        let mut checked_aps = 0usize;
        for cpu in 1..super::percpu::MAX_CPUS {
            if crate::smp::online_lapic_id(cpu).is_none() {
                continue;
            }
            AP_DONE.store(false, Ordering::Release);
            crate::smp::run_on_test(cpu, || {
                AP_CR0.store(read_cr0(), Ordering::Relaxed);
                AP_CR4.store(read_cr4(), Ordering::Relaxed);
                AP_DONE.store(true, Ordering::Release);
            })?;
            crate::kassert!(crate::smp::job_reached(cpu, || AP_DONE.load(Ordering::Acquire)));
            check_cr0_cr4(AP_CR0.load(Ordering::Relaxed), AP_CR4.load(Ordering::Relaxed), active)?;
            checked_aps += 1;
        }
        crate::kassert!(checked_aps > 0);

        let initial = UserFpuState::new();
        crate::kassert!((initial.as_ptr() as usize).is_multiple_of(FPU_ALIGN));
        crate::kassert_eq!(initial.len(), active.area_size());
        crate::kassert_eq!(initial.fcw(), DEFAULT_FCW);
        crate::kassert_eq!(initial.mxcsr(), DEFAULT_MXCSR);

        // Round-trip non-default valid control words so a broken/no-op restore
        // cannot pass by relying on hardware reset defaults.
        const CUSTOM_FCW: u16 = 0x0F7F;
        const CUSTOM_MXCSR: u32 = 0x7F80;
        let mut custom = UserFpuState::new();
        {
            let bytes = custom.as_bytes_mut_internal();
            bytes[FCW_OFFSET..FCW_OFFSET + 2].copy_from_slice(&CUSTOM_FCW.to_le_bytes());
            bytes[MXCSR_OFFSET..MXCSR_OFFSET + 4].copy_from_slice(&CUSTOM_MXCSR.to_le_bytes());
            if matches!(active, FpuMode::XSave { .. }) {
                // Mark x87 + SSE present in XSTATE_BV so XRSTOR64 loads FCW
                // from offset 0 instead of initializing x87 to 0x037F.
                let xstate_bv = XCR0_X87 | XCR0_SSE;
                bytes[XSAVE_HEADER_OFFSET..XSAVE_HEADER_OFFSET + 8].copy_from_slice(&xstate_bv.to_le_bytes());
            }
        }
        custom.restore();

        let mut saved = UserFpuState::new();
        saved.as_bytes_mut_internal().fill(0);
        crate::kassert_eq!(saved.fcw(), 0);
        crate::kassert_eq!(saved.mxcsr(), 0);
        saved.save();
        crate::kassert_eq!(saved.fcw(), CUSTOM_FCW);
        crate::kassert_eq!(saved.mxcsr(), CUSTOM_MXCSR);

        // Restore clean defaults (XSTATE_BV = 0 in XSAVE mode) and verify
        // hardware returns to DEFAULT_FCW / DEFAULT_MXCSR.
        initial.restore();
        saved.as_bytes_mut_internal().fill(0);
        saved.save();
        crate::kassert_eq!(saved.fcw(), DEFAULT_FCW);
        crate::kassert_eq!(saved.mxcsr(), DEFAULT_MXCSR);

        // Verify `IA32_FS_BASE` round-trip with a valid user address (< USER_MAX),
        // restoring 0 afterward.
        let sample_tls = 0x0000_7fff_1234_5678u64;
        // SAFETY: no user thread is running during ktest; we restore 0 right after.
        unsafe { write_fs_base(sample_tls) };
        crate::kassert_eq!(read_fs_base(), sample_tls);
        // SAFETY: restoring the kernel default FS base of 0.
        unsafe { write_fs_base(0) };
        crate::kassert_eq!(read_fs_base(), 0);

        Ok(())
    }
);
