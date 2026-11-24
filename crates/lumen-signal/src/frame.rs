//! Linux x86-64 rt_sigframe construction and rt_sigreturn validation.
//!
//! Buffers are snapshots, not user pointers. The kernel must copy them safely,
//! enforce mappings and apply its final return-to-user checks. XSAVE uses the
//! standard (non-compacted), CPU-sized layout; CPUID component layout and actual
//! FPU instructions remain the kernel's responsibility.

use alloc::vec::Vec;
use core::mem::offset_of;
use linux_raw_sys::general;

use crate::abi::*;
use crate::altstack::AltStack;
use crate::pending::SigInfo;
use crate::restart::{self, Handling, SyscallExit};
use crate::{Blocked, SigSet};

/// Exclusive limit from Lumen ARCHITECTURE §5/§6; the top user page is reserved.
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;
/// Named x86 RFLAGS bits used by signal entry and return.
pub mod eflags {
    pub const CF: u64 = 1 << 0;
    pub const PF: u64 = 1 << 2;
    pub const AF: u64 = 1 << 4;
    pub const ZF: u64 = 1 << 6;
    pub const SF: u64 = 1 << 7;
    pub const TF: u64 = 1 << 8;
    pub const DF: u64 = 1 << 10;
    pub const OF: u64 = 1 << 11;
    pub const RF: u64 = 1 << 16;
    pub const AC: u64 = 1 << 18;
}
/// Linux arch/x86/include/asm/sighandling.h FIX_EFLAGS: AC, OF, DF, TF, SF,
/// ZF, AF, PF, CF and RF. Other bits come from the user's R11 at syscall entry.
/// Lumen's final return sanitizer always clears TF/RF (ARCHITECTURE §6).
pub const FIX_EFLAGS: u64 = eflags::AC
    | eflags::OF
    | eflags::DF
    | eflags::TF
    | eflags::SF
    | eflags::ZF
    | eflags::AF
    | eflags::PF
    | eflags::CF
    | eflags::RF;

/// All x86-64 GPRs and the exception metadata preserved in a signal context.
/// FS/GS base state is not part of sigcontext and remains owned by the kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Registers {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u16,
    pub ss: u16,
    pub err: u64,
    pub trapno: u64,
    pub cr2: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum FpState<'a> {
    Fxsave(&'a [u8]),
    Xsave { bytes: &'a [u8], xfeatures: u64 },
}

impl<'a> FpState<'a> {
    pub const fn bytes(self) -> &'a [u8] {
        match self {
            Self::Fxsave(bytes) | Self::Xsave { bytes, .. } => bytes,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Handler {
    pub address: u64,
    pub flags: u64,
    pub restorer: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    pub registers: Registers,
    pub fp: FpState<'a>,
    pub info: SigInfo,
    pub mask: Blocked,
    pub handler: Handler,
    pub alt_stack: AltStack,
    /// Trusted kernel syscall context; None for asynchronous user execution.
    pub syscall: Option<SyscallExit>,
    /// Kernel-owned ring-3 selectors for handler entry.
    pub user_cs: u16,
    pub user_ss: u16,
}

/// A complete delivery image. The kernel must copy bytes to new_rsp first,
/// and only after a successful copy install alt_stack_during_handler (Linux
/// signal_delivered). It must also block sa_mask and, unless SA_NODEFER, the
/// signal; apply SA_RESETHAND; and reset user FP state (fpu__clear_user_states).
///
/// The validated delivery image cannot be edited in place:
/// ```compile_fail
/// use lumen_signal::frame::FrameImage;
/// fn corrupt(image: &mut FrameImage) { image.bytes.clear(); }
/// ```
#[must_use]
#[derive(Debug)]
pub struct FrameImage {
    bytes: Vec<u8>,
    new_rsp: u64,
    entry: Registers,
    alt_stack_during_handler: AltStack,
    fp_offset: usize,
}

impl FrameImage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub const fn new_rsp(&self) -> u64 {
        self.new_rsp
    }
    pub const fn entry(&self) -> Registers {
        self.entry
    }
    pub const fn alt_stack_during_handler(&self) -> AltStack {
        self.alt_stack_during_handler
    }
    pub fn fp_snapshot(&self) -> FpSnapshot<'_> {
        FpSnapshot { address: self.new_rsp + self.fp_offset as u64, bytes: &self.bytes[self.fp_offset..] }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    MissingRestorer,
    RestartUnderflow,
    InvalidAddress,
    StackOverflow,
    BadFpState(FpError),
    NoMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpError {
    Truncated,
    Alignment,
    Address,
    Mxcsr,
    Size,
    Features,
    Reserved,
}

fn write16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn write32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn read<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}
fn read32(bytes: &[u8], offset: usize) -> Result<u32, FpError> {
    read(bytes, offset).map(u32::from_le_bytes).ok_or(FpError::Truncated)
}
fn read64(bytes: &[u8], offset: usize) -> Result<u64, FpError> {
    read(bytes, offset).map(u64::from_le_bytes).ok_or(FpError::Truncated)
}

macro_rules! register_words {
    ($mac:ident $(, $args:expr)*) => {
        $mac!($($args,)* r8, r9, r10, r11, r12, r13, r14, r15,
            rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip, rflags)
    };
}

fn encode_registers(bytes: &mut [u8], regs: Registers) {
    macro_rules! encode {
        ($($field:ident),*) => { $(write64(bytes, offset_of!(RtSigFrame, uc.mcontext.$field), regs.$field);)* };
    }
    register_words!(encode);
    encode!(err, trapno, cr2);
    write16(bytes, offset_of!(RtSigFrame, uc.mcontext.cs), regs.cs);
    write16(bytes, offset_of!(RtSigFrame, uc.mcontext.ss), regs.ss);
}

fn check_xheader(bytes: &[u8], features: u64) -> Result<(), FpError> {
    if read64(bytes, XSTATE_BV)? & !features != 0 {
        return Err(FpError::Features);
    }
    let reserved = bytes.get(XCOMP_BV..XHEADER_END).ok_or(FpError::Truncated)?;
    // XCOMP_BV must also be zero: user ABI accepts only standard XSAVE images.
    if reserved.iter().any(|&byte| byte != 0) {
        return Err(FpError::Reserved);
    }
    Ok(())
}

/// Build a frame below the interrupted red zone, or from the alternate stack's
/// top on first SA_ONSTACK entry. All stack arithmetic is checked, including
/// nested alternate-stack overflow. The image's FP address is 64-byte aligned
/// and the handler RSP satisfies (RSP + 8) % 16 == 0. Handler addresses 0 and 1
/// are SIG_DFL/SIG_IGN, not callable handlers. Restorer non-null and USER_MAX
/// address checks are intentionally stricter than Linux. Restart processing is
/// applied to the saved registers before encoding the frame.
pub fn build(input: FrameInput<'_>) -> Result<FrameImage, FrameError> {
    let FrameInput { mut registers, fp, info, mask, handler, alt_stack, syscall, user_cs, user_ss } = input;
    restart::decide(syscall, Handling::Handler { sa_restart: handler.flags & general::SA_RESTART as u64 != 0 })
        .apply(&mut registers)
        .map_err(|_| FrameError::RestartUnderflow)?;
    if handler.flags & general::SA_RESTORER as u64 == 0 || handler.restorer == 0 {
        return Err(FrameError::MissingRestorer);
    }
    if handler.address <= 1 || handler.address >= USER_MAX || handler.restorer >= USER_MAX {
        return Err(FrameError::InvalidAddress);
    }
    let raw = fp.bytes();
    let extra = match fp {
        FpState::Fxsave(_) => {
            if raw.len() != FXSAVE_SIZE {
                return Err(FrameError::BadFpState(FpError::Size));
            }
            0
        }
        FpState::Xsave { xfeatures, .. } => {
            if raw.len() < FXSAVE_SIZE + XHEADER_SIZE || raw.len() > u32::MAX as usize - 4 {
                return Err(FrameError::BadFpState(FpError::Size));
            }
            check_xheader(raw, xfeatures).map_err(FrameError::BadFpState)?;
            4
        }
    };
    let fp_len = raw.len().checked_add(extra).ok_or(FrameError::BadFpState(FpError::Size))?;
    let sp = registers.rsp.checked_sub(128).ok_or(FrameError::StackOverflow)?;
    let nested_alt = alt_stack.on_stack(registers.rsp);
    let entering_alt =
        handler.flags & general::SA_ONSTACK as u64 != 0 && alt_stack.enabled() && !alt_stack.on_stack(sp);
    let top = if entering_alt {
        alt_stack.top()
    } else {
        if registers.rsp > USER_MAX {
            return Err(FrameError::InvalidAddress);
        }
        sp
    };
    let fp_address = top.checked_sub(fp_len as u64).ok_or(FrameError::StackOverflow)? & !63;
    let new_rsp = (fp_address.checked_sub(FRAME_SIZE as u64).ok_or(FrameError::StackOverflow)? & !15)
        .checked_sub(8)
        .ok_or(FrameError::StackOverflow)?;
    if (entering_alt || nested_alt) && !alt_stack.contains(new_rsp) {
        return Err(FrameError::StackOverflow);
    }
    let fp_offset = usize::try_from(fp_address - new_rsp).map_err(|_| FrameError::StackOverflow)?;
    let len = fp_offset.checked_add(fp_len).ok_or(FrameError::StackOverflow)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| FrameError::NoMemory)?;
    bytes.resize(len, 0);
    write64(&mut bytes, 0, handler.restorer);
    let flags =
        UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS | if matches!(fp, FpState::Xsave { .. }) { UC_FP_XSTATE } else { 0 };
    write64(&mut bytes, UCONTEXT, flags);
    write64(&mut bytes, offset_of!(RtSigFrame, uc.stack.sp), alt_stack.sp());
    write32(&mut bytes, offset_of!(RtSigFrame, uc.stack.flags), alt_stack.flags());
    write64(&mut bytes, offset_of!(RtSigFrame, uc.stack.size), alt_stack.size());
    encode_registers(&mut bytes, registers);
    write64(&mut bytes, offset_of!(RtSigFrame, uc.mcontext.oldmask), mask.set().0);
    write64(&mut bytes, offset_of!(RtSigFrame, uc.mcontext.fpstate), fp_address);
    write64(&mut bytes, SIGMASK, mask.set().0);
    bytes[SIGINFO..SIGINFO + 128].copy_from_slice(info.bytes());
    let fp_bytes = &mut bytes[fp_offset..];
    fp_bytes[..raw.len()].copy_from_slice(raw);
    // Software metadata is kernel-owned; never expose stale FXSAVE padding.
    fp_bytes[SW_BYTES..FXSAVE_SIZE].fill(0);
    if let FpState::Xsave { xfeatures, .. } = fp {
        write32(fp_bytes, SW_MAGIC1, FP_XSTATE_MAGIC1);
        write32(fp_bytes, SW_EXTENDED_SIZE, fp_len as u32);
        write64(fp_bytes, SW_XFEATURES, xfeatures);
        write32(fp_bytes, SW_XSTATE_SIZE, raw.len() as u32);
        write32(fp_bytes, raw.len(), FP_XSTATE_MAGIC2);
        // Linux save_xstate_epilog ensures handler edits to FP/SSE are restored.
        let xstate_bv = read64(raw, XSTATE_BV).map_err(FrameError::BadFpState)?;
        write64(fp_bytes, XSTATE_BV, xstate_bv | 0b11);
    }
    let mut entry = registers;
    entry.rsp = new_rsp;
    entry.rip = handler.address;
    entry.rdi = info.signal().number() as u64;
    entry.rsi = new_rsp + SIGINFO as u64;
    entry.rdx = new_rsp + UCONTEXT as u64;
    entry.rax = 0;
    // Linux handle_signal clears DF/RF/TF for handler execution.
    entry.rflags &= !(eflags::DF | eflags::RF | eflags::TF);
    entry.cs = user_cs;
    entry.ss = user_ss;
    Ok(FrameImage { bytes, new_rsp, entry, alt_stack_during_handler: alt_stack.during_handler(), fp_offset })
}

/// A separate user-copy snapshot permits an edited fpstate pointer to refer
/// outside the frame. Its address must equal sigcontext.fpstate.
#[derive(Clone, Copy, Debug)]
pub struct FpSnapshot<'a> {
    pub address: u64,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct RestoreConfig {
    /// The user's R11 at rt_sigreturn syscall entry, not kernel execution flags.
    pub current_rflags: u64,
    pub user_cs: u16,
    pub user_ss: u16,
    pub mxcsr_mask: u32,
    pub enabled_xfeatures: u64,
    /// Standard size for the features XCR0 enables (CPUID.(0Dh,0):EBX),
    /// or None for FXSAVE-only CPUs.
    pub xsave_size: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    TruncatedFrame,
    NonCanonicalRip,
    NonUserRip,
    BadFpState(FpError),
}

/// A validated FP restore mode. Xsave's feature mask is the user-requested
/// software mask intersected with CPU-enabled features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpKind {
    Fxsave,
    Xsave { xfeatures: u64 },
}

/// Only restore can construct this verdict. The kernel must copy only bytes()
/// into its aligned restore buffer and use kind()'s mode/feature mask. On an
/// FX-only fallback it must initialize the extended state, as Linux does.
/// With XSAVE it must also initialize enabled features outside the returned
/// feature mask, rather than retaining state from the handler.
///
/// A build-side image cannot be labelled validated by a caller:
/// ```compile_fail
/// use lumen_signal::frame::{ValidatedFp, FpKind};
/// let raw = [0xff; 512];
/// let forged = ValidatedFp { bytes: &raw, kind: FpKind::Fxsave };
/// ```
/// Nor can a caller replace an already validated image's bytes:
/// ```compile_fail
/// use lumen_signal::frame::ValidatedFp;
/// fn replace(fp: &mut ValidatedFp<'static>) { fp.bytes = &[0xff; 512]; }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ValidatedFp<'a> {
    bytes: &'a [u8],
    kind: FpKind,
}
impl<'a> ValidatedFp<'a> {
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    pub const fn kind(self) -> FpKind {
        self.kind
    }
}

/// An opaque successful sigreturn verdict. Exception metadata is not restored
/// from user memory: registers().err/trapno/cr2 are zero. None FP means reset to
/// clean initial state. Linux clears orig_ax, represented by syscall() == None.
///
/// A caller cannot manufacture a successful sigreturn verdict:
/// ```compile_fail
/// use lumen_signal::{Blocked, altstack::AltStack, frame::{Restored, Registers}};
/// let forged = Restored { registers: Registers::default(), mask: Blocked::EMPTY,
///     alt_stack: AltStack::DISABLED, fp: None };
/// ```
/// Validated registers cannot be edited through the verdict:
/// ```compile_fail
/// use lumen_signal::frame::Restored;
/// fn corrupt(state: &mut Restored<'_>) { state.registers.rip = u64::MAX; }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Restored<'a> {
    registers: Registers,
    mask: Blocked,
    alt_stack: AltStack,
    fp: Option<ValidatedFp<'a>>,
}
impl<'a> Restored<'a> {
    pub const fn registers(self) -> Registers {
        self.registers
    }
    pub const fn mask(self) -> Blocked {
        self.mask
    }
    pub const fn alt_stack(self) -> AltStack {
        self.alt_stack
    }
    pub const fn fp(self) -> Option<ValidatedFp<'a>> {
        self.fp
    }
    pub const fn syscall(self) -> Option<SyscallExit> {
        None
    }
}

fn validate_fp(snapshot: FpSnapshot<'_>, config: RestoreConfig) -> Result<ValidatedFp<'_>, FpError> {
    let FpSnapshot { address, bytes } = snapshot;
    if address >= USER_MAX || address.checked_add(bytes.len() as u64).is_none_or(|end| end > USER_MAX) {
        return Err(FpError::Address);
    }
    if address & 15 != 0 {
        return Err(FpError::Alignment);
    }
    let legacy = bytes.get(..FXSAVE_SIZE).ok_or(FpError::Truncated)?;
    if read32(bytes, MXCSR)? & !config.mxcsr_mask != 0 {
        return Err(FpError::Mxcsr);
    }
    let fx = ValidatedFp { bytes: legacy, kind: FpKind::Fxsave };
    // check_xstate_in_sigframe fallback: UC_FP_XSTATE and sw padding are not
    // validation inputs. FXSAVE-only CPUs ignore all software metadata.
    let Some(cpu_size) = config.xsave_size else { return Ok(fx) };
    if read32(bytes, SW_MAGIC1)? != FP_XSTATE_MAGIC1 {
        return Ok(fx);
    }
    let extended = read32(bytes, SW_EXTENDED_SIZE)? as usize;
    let size = read32(bytes, SW_XSTATE_SIZE)? as usize;
    if size < XHEADER_END || size != cpu_size || size.checked_add(4) != Some(extended) {
        return Ok(fx);
    }
    let area = bytes.get(..size).ok_or(FpError::Truncated)?;
    if read32(bytes, extended - 4)? != FP_XSTATE_MAGIC2 {
        return Ok(fx);
    }
    if address & 63 != 0 {
        return Err(FpError::Alignment);
    }
    check_xheader(area, config.enabled_xfeatures)?;
    let features = read64(bytes, SW_XFEATURES)? & config.enabled_xfeatures;
    Ok(ValidatedFp { bytes: area, kind: FpKind::Xsave { xfeatures: features } })
}

/// Validate a copied x86-64 frame starting at pretcode (restorer syscall RSP is
/// normally frame_address + 8). current_stack/current_rsp are the live alternate
/// stack configuration and user RSP at sigreturn entry, for do_sigaltstack's
/// -EPERM check. Invalid saved stack changes are ignored, as Linux does; safe
/// user-copy faults must already have been handled by the caller.
///
/// Forces selectors, restores Blocked, and merges FIX_EFLAGS with user R11 from
/// syscall entry. Non-canonical RIP has a distinct verdict; kernel/top-guard
/// addresses are refused too. The kernel must apply its final address/RFLAGS
/// checks (which always clear TF/RF) before any return instruction.
pub fn restore<'a>(
    bytes: &[u8],
    fp: Option<FpSnapshot<'a>>,
    config: RestoreConfig,
    current_stack: AltStack,
    current_rsp: u64,
) -> Result<Restored<'a>, RestoreError> {
    let bytes = bytes.first_chunk::<FRAME_SIZE>().ok_or(RestoreError::TruncatedFrame)?;
    // Every caller below supplies an offset statically pinned inside RtSigFrame.
    let word = |offset: usize| u64::from_le_bytes(core::array::from_fn(|i| bytes[offset + i]));
    let dword = |offset: usize| u32::from_le_bytes(core::array::from_fn(|i| bytes[offset + i]));
    macro_rules! decode {
        ($($field:ident),*) => {
            Registers { $($field: word(offset_of!(RtSigFrame, uc.mcontext.$field)),)*
                cs: config.user_cs, ss: config.user_ss, err: 0, trapno: 0, cr2: 0 }
        };
    }
    let mut registers = register_words!(decode);
    if ((registers.rip << 16) as i64 >> 16) as u64 != registers.rip {
        return Err(RestoreError::NonCanonicalRip);
    }
    if registers.rip >= USER_MAX {
        return Err(RestoreError::NonUserRip);
    }
    registers.rflags = (registers.rflags & FIX_EFLAGS) | (config.current_rflags & !FIX_EFLAGS);
    let mask = Blocked::new(SigSet(word(SIGMASK)));
    let pointer = word(offset_of!(RtSigFrame, uc.mcontext.fpstate));
    let fp = if pointer == 0 {
        None
    } else {
        let snapshot = fp.filter(|fp| fp.address == pointer).ok_or(RestoreError::BadFpState(FpError::Address))?;
        Some(validate_fp(snapshot, config).map_err(RestoreError::BadFpState)?)
    };
    let alt_stack = current_stack
        .configure(
            current_rsp,
            word(offset_of!(RtSigFrame, uc.stack.sp)),
            word(offset_of!(RtSigFrame, uc.stack.size)),
            dword(offset_of!(RtSigFrame, uc.stack.flags)),
        )
        .unwrap_or(current_stack);
    Ok(Restored { registers, mask, alt_stack, fp })
}
