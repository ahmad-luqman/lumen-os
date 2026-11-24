//! Linux x86-64 rt_sigframe construction and rt_sigreturn validation.
//!
//! Buffers are snapshots, not user pointers. The kernel must copy them safely,
//! enforce mappings and apply its final return-to-user checks. XSAVE uses the
//! standard (non-compacted), CPU-sized layout; CPUID component layout and actual
//! FPU instructions remain the kernel's responsibility.

use alloc::vec::Vec;
use core::mem::offset_of;
use linux_raw_sys::general;

use crate::SigSet;
use crate::abi::*;
use crate::altstack::{AltStack, StackError};
use crate::pending::SigInfo;

/// Exclusive limit from Lumen ARCHITECTURE §5/§6; the top user page is reserved.
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;
/// Linux arch/x86/include/asm/sighandling.h FIX_EFLAGS: AC, OF, DF, TF, SF,
/// ZF, AF, PF, CF and RF. Other bits come from trusted current flags. Lumen's
/// final return-to-user sanitizer may additionally clear TF/RF (ARCHITECTURE §6).
pub const FIX_EFLAGS: u64 = 0x0005_0dd5;

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
    pub mask: SigSet,
    pub handler: Handler,
    pub alt_stack: AltStack,
}

#[derive(Debug)]
pub struct FrameImage {
    /// Copy these bytes to new_rsp, including padding and FP state.
    pub bytes: Vec<u8>,
    pub new_rsp: u64,
    pub entry: Registers,
    pub alt_stack_during_handler: AltStack,
    fp_offset: usize,
}

impl FrameImage {
    /// The FP snapshot to pass to restore alongside the frame bytes, or None
    /// if public image fields were shortened or changed to a wrapping address.
    pub fn fp_snapshot(&self) -> Option<FpSnapshot<'_>> {
        Some(FpSnapshot {
            address: self.new_rsp.checked_add(self.fp_offset as u64)?,
            bytes: self.bytes.get(self.fp_offset..)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    MissingRestorer,
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
    Magic,
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
            rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip, rflags, err, trapno, cr2)
    };
}

fn encode_registers(bytes: &mut [u8], regs: Registers) {
    macro_rules! encode {
        ($($field:ident),*) => { $(write64(bytes, MCONTEXT + offset_of!(SigContext, $field), regs.$field);)* };
    }
    register_words!(encode);
    write16(bytes, MCONTEXT + offset_of!(SigContext, cs), regs.cs);
    write16(bytes, MCONTEXT + offset_of!(SigContext, ss), regs.ss);
}

fn check_xheader(bytes: &[u8], features: u64) -> Result<(), FpError> {
    if read64(bytes, FXSAVE_SIZE)? & !features != 0 {
        return Err(FpError::Features);
    }
    let reserved = bytes.get(FXSAVE_SIZE + 8..FXSAVE_SIZE + XHEADER_SIZE).ok_or(FpError::Truncated)?;
    // XCOMP_BV must also be zero: user ABI accepts only standard XSAVE images.
    if reserved.iter().any(|&byte| byte != 0) {
        return Err(FpError::Reserved);
    }
    Ok(())
}

/// Build a frame below the interrupted red zone, or from the alternate stack's
/// top on first SA_ONSTACK entry. All stack arithmetic is checked, including
/// nested alternate-stack overflow. The image's FP address is 64-byte aligned
/// and the handler RSP satisfies (RSP + 8) % 16 == 0.
pub fn build(input: FrameInput<'_>) -> Result<FrameImage, FrameError> {
    let FrameInput { registers, fp, info, mask, handler, alt_stack } = input;
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
    let entering_alt =
        handler.flags & general::SA_ONSTACK as u64 != 0 && alt_stack.enabled() && !alt_stack.on_stack(registers.rsp);
    let top = if entering_alt {
        alt_stack.top()
    } else {
        if registers.rsp > USER_MAX {
            return Err(FrameError::InvalidAddress);
        }
        registers.rsp.checked_sub(128).ok_or(FrameError::StackOverflow)?
    };
    let fp_address = top.checked_sub(fp_len as u64).ok_or(FrameError::StackOverflow)? & !63;
    let new_rsp = (fp_address.checked_sub(FRAME_SIZE as u64).ok_or(FrameError::StackOverflow)? & !15)
        .checked_sub(8)
        .ok_or(FrameError::StackOverflow)?;
    if (entering_alt || alt_stack.contains(registers.rsp)) && new_rsp < alt_stack.sp() {
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
    write64(&mut bytes, STACK + offset_of!(Stack, sp), alt_stack.sp());
    write32(&mut bytes, STACK + offset_of!(Stack, flags), alt_stack.status(registers.rsp));
    write64(&mut bytes, STACK + offset_of!(Stack, size), alt_stack.size());
    encode_registers(&mut bytes, registers);
    write64(&mut bytes, MCONTEXT + offset_of!(SigContext, oldmask), mask.0);
    write64(&mut bytes, MCONTEXT + offset_of!(SigContext, fpstate), fp_address);
    write64(&mut bytes, SIGMASK, mask.0);
    bytes[SIGINFO..SIGINFO + 128].copy_from_slice(info.bytes());
    let fp_bytes = &mut bytes[fp_offset..];
    fp_bytes[..raw.len()].copy_from_slice(raw);
    // Software metadata is kernel-owned; never expose stale FXSAVE padding.
    fp_bytes[SW_BYTES..FXSAVE_SIZE].fill(0);
    if let FpState::Xsave { xfeatures, .. } = fp {
        write32(fp_bytes, SW_BYTES, FP_XSTATE_MAGIC1);
        write32(fp_bytes, SW_BYTES + 4, fp_len as u32);
        write64(fp_bytes, SW_BYTES + 8, xfeatures);
        write32(fp_bytes, SW_BYTES + 16, raw.len() as u32);
        write32(fp_bytes, raw.len(), FP_XSTATE_MAGIC2);
    }
    let mut entry = registers;
    entry.rsp = new_rsp;
    entry.rip = handler.address;
    entry.rdi = info.signal().number() as u64;
    entry.rsi = new_rsp + SIGINFO as u64;
    entry.rdx = new_rsp + UCONTEXT as u64;
    entry.rax = 0;
    // Linux handle_signal clears DF/RF/TF for handler execution.
    entry.rflags &= !(0x400 | 0x10000 | 0x100);
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
    pub current_rflags: u64,
    pub user_cs: u16,
    pub user_ss: u16,
    pub mxcsr_mask: u32,
    pub enabled_xfeatures: u64,
    /// CPU's full standard XSAVE size from CPUID, or None for FXSAVE-only CPUs.
    pub xsave_size: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreError {
    TruncatedFrame,
    NonCanonicalRip,
    NonUserRip,
    BadFpState(FpError),
    BadAltStack(StackError),
}

#[derive(Clone, Copy, Debug)]
pub struct Restored<'a> {
    pub registers: Registers,
    pub mask: SigSet,
    pub alt_stack: AltStack,
    /// None means initialize clean FP state (Linux's null fpstate behavior).
    /// Software metadata is still present in these bytes; the kernel copies
    /// only the indicated FXSAVE/XSAVE area into its aligned restore buffer.
    pub fp: Option<FpState<'a>>,
}

fn validate_fp<'a>(snapshot: FpSnapshot<'a>, uc_flags: u64, config: RestoreConfig) -> Result<FpState<'a>, FpError> {
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
    let magic = read32(bytes, SW_BYTES)?;
    if magic == 0 && uc_flags & UC_FP_XSTATE == 0 {
        return Ok(FpState::Fxsave(legacy));
    }
    if magic != FP_XSTATE_MAGIC1 {
        return Err(FpError::Magic);
    }
    if address & 63 != 0 {
        return Err(FpError::Alignment);
    }
    let extended = read32(bytes, SW_BYTES + 4)? as usize;
    let size = read32(bytes, SW_BYTES + 16)? as usize;
    if size < FXSAVE_SIZE + XHEADER_SIZE || Some(size) != config.xsave_size || size.checked_add(4) != Some(extended) {
        return Err(FpError::Size);
    }
    let area = bytes.get(..size).ok_or(FpError::Truncated)?;
    if read32(bytes, size)? != FP_XSTATE_MAGIC2 {
        return Err(FpError::Magic);
    }
    let features = read64(bytes, SW_BYTES + 8)?;
    if features & !config.enabled_xfeatures != 0 {
        return Err(FpError::Features);
    }
    if bytes[SW_BYTES + 20..FXSAVE_SIZE].iter().any(|&byte| byte != 0) {
        return Err(FpError::Reserved);
    }
    check_xheader(area, features)?;
    Ok(FpState::Xsave { bytes: area, xfeatures: features })
}

/// Validate a copied x86-64 frame beginning at pretcode (the restorer's syscall
/// RSP is normally frame_address + 8). Restores masks without KILL/STOP, forces
/// selectors and merges FIX_EFLAGS with trusted current flags. Non-canonical
/// RIP has its own verdict; canonical kernel/top-guard addresses are also refused.
/// No restore instruction may run until this function succeeds and the kernel
/// has performed its final user-address and RFLAGS checks.
pub fn restore<'a>(
    bytes: &[u8],
    fp: Option<FpSnapshot<'a>>,
    config: RestoreConfig,
) -> Result<Restored<'a>, RestoreError> {
    if bytes.len() < FRAME_SIZE {
        return Err(RestoreError::TruncatedFrame);
    }
    macro_rules! decode {
        ($($field:ident),*) => {
            Registers {
                $($field: u64::from_le_bytes(read(bytes, MCONTEXT + offset_of!(SigContext, $field))
                    .ok_or(RestoreError::TruncatedFrame)?),)*
                cs: config.user_cs,
                ss: config.user_ss,
            }
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
    let word = |offset| read64(bytes, offset).map_err(|_| RestoreError::TruncatedFrame);
    let mask = SigSet(word(SIGMASK)?).unblockable_stripped();
    let pointer = word(MCONTEXT + offset_of!(SigContext, fpstate))?;
    let fp = if pointer == 0 {
        None
    } else {
        let snapshot = fp.filter(|fp| fp.address == pointer).ok_or(RestoreError::BadFpState(FpError::Address))?;
        Some(validate_fp(snapshot, word(UCONTEXT)?, config).map_err(RestoreError::BadFpState)?)
    };
    let stack_flags = read32(bytes, STACK + offset_of!(Stack, flags)).map_err(|_| RestoreError::TruncatedFrame)?;
    let alt_stack = AltStack::DISABLED
        .configure(0, word(STACK + offset_of!(Stack, sp))?, word(STACK + offset_of!(Stack, size))?, stack_flags)
        .map_err(RestoreError::BadAltStack)?;
    Ok(Restored { registers, mask, alt_stack, fp })
}
