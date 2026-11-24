//! Fixed Linux x86-64 wire layouts, independent of the host architecture.
//!
//! Sources: arch/x86/include/uapi/asm/sigcontext.h and ucontext.h,
//! include/uapi/asm-generic/ucontext.h, and arch/x86/include/asm/sigframe.h.
//! linux-raw-sys 0.12.1 exposes stack_t and siginfo_t, but not x86 sigcontext,
//! ucontext, rt_sigframe or the FP structs. Assertions below pin every field of
//! those layouts against transcribed Linux header literals. XSaveHeader follows
//! arch/x86/include/asm/fpu/types.h (struct xstate_header). Serialization never
//! reads Rust padding.

use core::mem::{offset_of, size_of};

#[repr(C)]
pub struct Stack {
    pub sp: u64,
    pub flags: u32,
    pub size: u64,
}

#[repr(C)]
pub struct SigContext {
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
    pub gs: u16,
    pub fs: u16,
    pub ss: u16,
    pub err: u64,
    pub trapno: u64,
    pub oldmask: u64,
    pub cr2: u64,
    pub fpstate: u64,
    pub reserved: [u64; 8],
}

#[repr(C)]
pub struct UContext {
    pub flags: u64,
    pub link: u64,
    pub stack: Stack,
    pub mcontext: SigContext,
    pub sigmask: u64,
}

#[repr(C)]
pub struct RtSigFrame {
    pub pretcode: u64,
    pub uc: UContext,
    pub info: [u8; 128],
}

#[repr(C)]
pub struct FpxSwBytes {
    pub magic1: u32,
    pub extended_size: u32,
    pub xfeatures: u64,
    pub xstate_size: u32,
    pub padding: [u32; 7],
}

#[repr(C)]
pub struct FxSave {
    pub cwd: u16,
    pub swd: u16,
    pub twd: u16,
    pub fop: u16,
    pub rip: u64,
    pub rdp: u64,
    pub mxcsr: u32,
    pub mxcsr_mask: u32,
    pub st: [u32; 32],
    pub xmm: [u32; 64],
    pub reserved: [u32; 12],
    pub sw: FpxSwBytes,
}

#[repr(C)]
pub struct XSaveHeader {
    pub xstate_bv: u64,
    pub xcomp_bv: u64,
    pub reserved: [u64; 6],
}

#[repr(C)]
pub struct XSavePrefix {
    pub legacy: FxSave,
    pub header: XSaveHeader,
}

pub const UCONTEXT: usize = offset_of!(RtSigFrame, uc);
pub const SIGINFO: usize = offset_of!(RtSigFrame, info);
pub const MCONTEXT: usize = offset_of!(RtSigFrame, uc.mcontext);
pub const SIGMASK: usize = offset_of!(RtSigFrame, uc.sigmask);
pub const STACK: usize = offset_of!(RtSigFrame, uc.stack);
pub const FRAME_SIZE: usize = size_of::<RtSigFrame>();
pub const FXSAVE_SIZE: usize = size_of::<FxSave>();
pub const XHEADER_SIZE: usize = size_of::<XSaveHeader>();
pub const SW_BYTES: usize = offset_of!(FxSave, sw);
pub const MXCSR: usize = offset_of!(FxSave, mxcsr);
pub const SW_MAGIC1: usize = offset_of!(FxSave, sw.magic1);
pub const SW_EXTENDED_SIZE: usize = offset_of!(FxSave, sw.extended_size);
pub const SW_XFEATURES: usize = offset_of!(FxSave, sw.xfeatures);
pub const SW_XSTATE_SIZE: usize = offset_of!(FxSave, sw.xstate_size);
pub const SW_PADDING: usize = offset_of!(FxSave, sw.padding);
pub const XSTATE_BV: usize = offset_of!(XSavePrefix, header.xstate_bv);
pub const XCOMP_BV: usize = offset_of!(XSavePrefix, header.xcomp_bv);
pub const XRESERVED: usize = offset_of!(XSavePrefix, header.reserved);
pub const XHEADER_END: usize = size_of::<XSavePrefix>();
pub const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
pub const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
pub const UC_FP_XSTATE: u64 = 1;
pub const UC_SIGCONTEXT_SS: u64 = 2;
pub const UC_STRICT_RESTORE_SS: u64 = 4;

macro_rules! offsets {
    ($ty:ty, $size:expr; $($field:ident: $offset:expr),+ $(,)?) => {
        const _: () = {
            assert!(size_of::<$ty>() == $size);
            $(assert!(offset_of!($ty, $field) == $offset);)+
        };
    };
}

offsets!(Stack, 24; sp: 0, flags: 8, size: 16);
offsets!(SigContext, 256;
    r8: 0, r9: 8, r10: 16, r11: 24, r12: 32, r13: 40, r14: 48, r15: 56,
    rdi: 64, rsi: 72, rbp: 80, rbx: 88, rdx: 96, rax: 104, rcx: 112, rsp: 120,
    rip: 128, rflags: 136, cs: 144, gs: 146, fs: 148, ss: 150,
    err: 152, trapno: 160, oldmask: 168, cr2: 176, fpstate: 184, reserved: 192,
);
offsets!(UContext, 304; flags: 0, link: 8, stack: 16, mcontext: 40, sigmask: 296);
offsets!(RtSigFrame, 440; pretcode: 0, uc: 8, info: 312);
offsets!(FpxSwBytes, 48; magic1: 0, extended_size: 4, xfeatures: 8, xstate_size: 16, padding: 20);
offsets!(FxSave, 512;
    cwd: 0, swd: 2, twd: 4, fop: 6, rip: 8, rdp: 16, mxcsr: 24, mxcsr_mask: 28,
    st: 32, xmm: 160, reserved: 416, sw: 464,
);
offsets!(XSavePrefix, 576; legacy: 0, header: 512);
offsets!(XSaveHeader, 64; xstate_bv: 0, xcomp_bv: 8, reserved: 16);

#[cfg(target_pointer_width = "64")]
const _: () = {
    use linux_raw_sys::general::stack_t;
    assert!(size_of::<Stack>() == size_of::<stack_t>());
    assert!(offset_of!(Stack, sp) == offset_of!(stack_t, ss_sp));
    assert!(offset_of!(Stack, flags) == offset_of!(stack_t, ss_flags));
    assert!(offset_of!(Stack, size) == offset_of!(stack_t, ss_size));
};
