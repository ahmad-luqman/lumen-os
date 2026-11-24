use linux_raw_sys::general as g;
use lumen_signal::abi;
use lumen_signal::altstack::AltStack;
use lumen_signal::frame::*;
use lumen_signal::pending::SigInfo;
use lumen_signal::restart::{Handling, SyscallExit};
use lumen_signal::{Blocked, SigSet, Signal};

// Transcribed Linux wire offsets used by tests, independent of abi's offset_of!.
const UC_FLAGS: usize = 8;
const STACK_SP: usize = 24;
const STACK_FLAGS: usize = 32;
const STACK_SIZE: usize = 40;
const RIP: usize = 176;
const RAX: usize = 152;
const OLD_MASK: usize = 216;
const FP_PTR: usize = 232;
const MASK: usize = 304;
const INFO: usize = 312;
const FRAME_END: usize = 440;
const MXCSR: usize = 24;
const MAGIC1: usize = 464;
const EXT_SIZE: usize = 468;
const FEATURES: usize = 472;
const STATE_SIZE: usize = 480;
const SW_PADDING: usize = 484;
const XSTATE_BV: usize = 512;
const XCOMP_BV: usize = 520;
const HEADER_END: usize = 576;
const XSIZE: usize = 832;

// Mutable copies model user edits; the real FrameImage remains opaque.
#[derive(Debug)]
struct CopiedFrame {
    bytes: Vec<u8>,
    new_rsp: u64,
    entry: Registers,
    alt_stack_during_handler: AltStack,
    fp_offset: usize,
}
impl CopiedFrame {
    fn fp_snapshot(&self) -> FpSnapshot<'_> {
        FpSnapshot { address: self.new_rsp + self.fp_offset as u64, bytes: &self.bytes[self.fp_offset..] }
    }
}
fn build(args: FrameInput<'_>) -> Result<CopiedFrame, FrameError> {
    let image = lumen_signal::frame::build(args)?;
    Ok(CopiedFrame {
        bytes: image.bytes().to_vec(),
        new_rsp: image.new_rsp(),
        entry: image.entry(),
        alt_stack_during_handler: image.alt_stack_during_handler(),
        fp_offset: (image.fp_snapshot().address - image.new_rsp()) as usize,
    })
}
fn restore<'a>(bytes: &[u8], fp: Option<FpSnapshot<'a>>, cfg: RestoreConfig) -> Result<Restored<'a>, RestoreError> {
    lumen_signal::frame::restore(bytes, fp, cfg, AltStack::DISABLED, 0)
}
fn without_exception(mut registers: Registers) -> Registers {
    registers.err = 0;
    registers.trapno = 0;
    registers.cr2 = 0;
    registers
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn fp_data(xsave: bool) -> Vec<u8> {
    let mut bytes = vec![0; if xsave { 832 } else { 512 }];
    bytes[0..2].copy_from_slice(&0x37fu16.to_le_bytes());
    put32(&mut bytes, MXCSR, 0x1f80);
    if xsave {
        put64(&mut bytes, 512, 7);
    }
    bytes
}
fn input(raw: &[u8], xsave: bool) -> FrameInput<'_> {
    FrameInput {
        registers: Registers {
            r8: 1,
            r9: 2,
            r10: 3,
            r11: 4,
            r12: 5,
            r13: 6,
            r14: 7,
            r15: 8,
            rdi: 9,
            rsi: 10,
            rbp: 11,
            rbx: 12,
            rdx: 13,
            rax: 14,
            rcx: 15,
            rsp: 0x800008,
            rip: 0x401234,
            rflags: 0x202,
            cs: 0x23,
            ss: 0x1b,
            err: 0x123,
            trapno: 14,
            cr2: 0xbad000,
        },
        fp: if xsave { FpState::Xsave { bytes: raw, xfeatures: 7 } } else { FpState::Fxsave(raw) },
        info: SigInfo::fault(Signal::new(11).unwrap(), 2, 0xbad000),
        mask: Blocked::new(SigSet(0x1234_5678_9abc_def0)),
        handler: Handler { address: 0x402000, flags: g::SA_RESTORER as u64, restorer: 0x403000 },
        alt_stack: AltStack::DISABLED,
        syscall: None,
        user_cs: 0x23,
        user_ss: 0x1b,
    }
}
fn config() -> RestoreConfig {
    RestoreConfig {
        current_rflags: 0x202,
        user_cs: 0x23,
        user_ss: 0x1b,
        mxcsr_mask: 0xffff,
        enabled_xfeatures: 7,
        xsave_size: Some(832),
    }
}
fn restore_image(image: &CopiedFrame) -> Result<Restored<'_>, RestoreError> {
    restore(&image.bytes, Some(image.fp_snapshot()), config())
}
fn assert_error(image: &CopiedFrame, expected: RestoreError) {
    assert_eq!(restore_image(image).unwrap_err(), expected);
}

#[test]
fn frame_offsets_sizes_and_register_wire_image() {
    assert_eq!((abi::UCONTEXT, abi::MCONTEXT, abi::SIGMASK, abi::SIGINFO, abi::FRAME_SIZE), (8, 48, 304, 312, 440));
    assert_eq!((abi::FXSAVE_SIZE, abi::XHEADER_SIZE, abi::SW_BYTES, abi::MXCSR), (512, 64, 464, 24));
    let raw = fp_data(false);
    let args = input(&raw, false);
    let image = build(args).unwrap();
    assert_eq!(word(&image.bytes, 0), 0x403000);
    assert_eq!(word(&image.bytes, 8), 6);
    assert_eq!(word(&image.bytes, 16), 0);
    assert_eq!(word(&image.bytes, 24), 0);
    assert_eq!(dword(&image.bytes, 32), 2);
    assert_eq!(&image.bytes[36..40], &[0; 4]);
    assert_eq!(word(&image.bytes, 40), 0);
    // Independent Linux sigcontext word order, not the crate's encoding helpers.
    let expected = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0x800008, 0x401234, 0x202];
    for (index, value) in expected.into_iter().enumerate() {
        assert_eq!(word(&image.bytes, 48 + 8 * index), value, "register {index}");
    }
    assert_eq!(&image.bytes[192..200], &[0x23, 0, 0, 0, 0, 0, 0x1b, 0]);
    assert_eq!(word(&image.bytes, 200), 0x123);
    assert_eq!(word(&image.bytes, 208), 14);
    assert_eq!(word(&image.bytes, 216), args.mask.set().0);
    assert_eq!(word(&image.bytes, 224), 0xbad000);
    assert_eq!(word(&image.bytes, 232), image.fp_snapshot().address);
    assert_eq!(&image.bytes[240..304], &[0; 64]);
    assert_eq!(word(&image.bytes, 304), args.mask.set().0);
    assert_eq!(&image.bytes[312..440], args.info.bytes());
    let fp_offset = (image.fp_snapshot().address - image.new_rsp) as usize;
    assert_eq!(fp_offset, 456);
    assert!(image.bytes[440..fp_offset].iter().all(|&b| b == 0));
    assert_eq!(image.bytes.len(), 456 + 512);
    assert_eq!(image.fp_snapshot().bytes, raw);
    let mut entry = args.registers;
    entry.rip = 0x402000;
    entry.rsp = image.new_rsp;
    entry.rdi = 11;
    entry.rsi = image.new_rsp + 312;
    entry.rdx = image.new_rsp + 8;
    entry.rax = 0;
    assert_eq!(image.entry, entry);
}

#[test]
fn fxsave_xsave_alignment_red_zone_and_altstack_placement() {
    for xsave in [false, true] {
        let raw = fp_data(xsave);
        for skew in 0..64 {
            let mut args = input(&raw, xsave);
            args.registers.rsp += skew;
            // A configured stack is used only when SA_ONSTACK requests it.
            args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
            for use_alt in [false, true] {
                if use_alt {
                    args.handler.flags |= g::SA_ONSTACK as u64;
                }
                let image = build(args).unwrap();
                let fp = image.fp_snapshot();
                let limit = if use_alt { 0x902000 } else { args.registers.rsp - 128 };
                let fp_size = if xsave { 836 } else { 512 };
                let expected_fp = (limit - fp_size) & !63;
                let expected_rsp = ((expected_fp - 440) & !15) - 8;
                assert_eq!(fp.address, expected_fp);
                assert_eq!(image.new_rsp, expected_rsp);
                assert_eq!((image.new_rsp + 8) % 16, 0);
                assert_eq!(fp.address % 64, 0);
                assert!(image.new_rsp + image.bytes.len() as u64 <= limit);
                assert!(image.new_rsp + 440 <= fp.address);
                if use_alt {
                    assert!(image.new_rsp >= 0x900000);
                    assert_eq!(word(&image.bytes, 24), 0x900000);
                    assert_eq!(word(&image.bytes, 40), 8192);
                    assert_eq!(dword(&image.bytes, 32), 0);
                }
                if xsave {
                    assert_eq!(word(&image.bytes, 8), 7);
                    assert_eq!(dword(fp.bytes, 464), 0x4650_5853);
                    assert_eq!(dword(fp.bytes, 468), 836);
                    assert_eq!(word(fp.bytes, 472), 7);
                    assert_eq!(dword(fp.bytes, 480), 832);
                    assert_eq!(&fp.bytes[484..512], &[0; 28]);
                    assert_eq!(dword(fp.bytes, 832), 0x4650_5845);
                } else {
                    assert_eq!(word(&image.bytes, 8), 6);
                    assert_eq!(fp.bytes.len(), 512);
                }
                assert_eq!(restore_image(&image).unwrap().registers(), without_exception(args.registers));
                assert_eq!(restore_image(&image).unwrap().alt_stack(), args.alt_stack);
            }
        }
    }
}

#[test]
fn nested_altstack_and_autodisarm_frames() {
    let raw = fp_data(false);
    let mut args = input(&raw, false);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
    args.handler.flags |= g::SA_ONSTACK as u64;
    args.registers.rsp = 0x901800;
    let image = build(args).unwrap();
    assert!(image.new_rsp + image.bytes.len() as u64 <= 0x901800 - 128);
    assert_eq!(dword(&image.bytes, 32), 0);
    assert_eq!(restore_image(&image).unwrap().alt_stack(), args.alt_stack);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, g::SS_AUTODISARM).unwrap();
    args.registers.rsp = 0x800000;
    let outer = build(args).unwrap();
    assert_eq!(outer.alt_stack_during_handler, AltStack::DISABLED);
    assert_eq!(dword(&outer.bytes, 32), g::SS_AUTODISARM);
    assert_eq!(restore_image(&outer).unwrap().alt_stack(), args.alt_stack);
    args.registers = outer.entry;
    args.alt_stack = outer.alt_stack_during_handler;
    let nested = build(args).unwrap();
    assert!(nested.new_rsp + nested.bytes.len() as u64 <= outer.new_rsp - 128);
    assert_eq!(dword(&nested.bytes, 32), g::SS_DISABLE);
    assert_eq!(restore_image(&nested).unwrap().alt_stack(), AltStack::DISABLED);
}

#[test]
fn frame_refuses_missing_restorer_bad_addresses_and_stack_wrap() {
    let raw = fp_data(false);
    let valid = input(&raw, false);
    for flags in [0, g::SA_ONSTACK as u64] {
        let mut args = valid;
        args.handler.flags = flags;
        assert_eq!(build(args).unwrap_err(), FrameError::MissingRestorer);
    }
    let mut args = valid;
    args.handler.restorer = 0;
    assert_eq!(build(args).unwrap_err(), FrameError::MissingRestorer);
    for address in [USER_MAX, 0x8000_0000_0000, u64::MAX] {
        let mut args = valid;
        args.handler.restorer = address;
        assert_eq!(build(args).unwrap_err(), FrameError::InvalidAddress);
        args = valid;
        args.handler.address = address;
        assert_eq!(build(args).unwrap_err(), FrameError::InvalidAddress);
        args = valid;
        args.registers.rsp = address.saturating_add(1);
        assert_eq!(build(args).unwrap_err(), FrameError::InvalidAddress);
    }
    for rsp in [0, 1, 8, 127, 128, 511, 512, 640, 1087] {
        let mut args = valid;
        args.registers.rsp = rsp;
        assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow, "rsp {rsp}");
    }
    // Check both first-entry overflow and nested overflow, with/without ONSTACK.
    let large = vec![0; 4096];
    let mut args = input(&large, true);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 2048, 0).unwrap();
    args.handler.flags |= g::SA_ONSTACK as u64;
    assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    for flags in [g::SA_RESTORER, g::SA_RESTORER | g::SA_ONSTACK] {
        let mut args = valid;
        args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
        args.registers.rsp = 0x900200;
        args.handler.flags = flags as u64;
        assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    }
}

#[test]
fn frame_rejects_bad_fp_inputs_and_clears_software_padding() {
    for size in [0, 511, 513] {
        let raw = vec![0; size];
        assert_eq!(build(input(&raw, false)).unwrap_err(), FrameError::BadFpState(FpError::Size));
    }
    for size in [0, 512, 575] {
        let raw = vec![0; size];
        assert_eq!(build(input(&raw, true)).unwrap_err(), FrameError::BadFpState(FpError::Size));
    }
    let mut raw = fp_data(true);
    put64(&mut raw, 512, 8);
    assert_eq!(build(input(&raw, true)).unwrap_err(), FrameError::BadFpState(FpError::Features));
    put64(&mut raw, 512, 7);
    for index in 520..576 {
        raw[index] = 1;
        assert_eq!(build(input(&raw, true)).unwrap_err(), FrameError::BadFpState(FpError::Reserved));
        raw[index] = 0;
    }
    let mut raw = fp_data(false);
    raw[464..512].fill(0xff);
    let mut args = input(&raw, false);
    args.registers.rflags = 0x202 | 0x100 | 0x400 | 0x10000;
    let image = build(args).unwrap();
    assert_eq!(&image.fp_snapshot().bytes[464..512], &[0; 48]);
    assert_eq!(image.entry.rflags, 0x202);
    assert_eq!(restore_image(&image).unwrap().registers().rflags, args.registers.rflags);
}

#[test]
fn sigreturn_forces_selectors_masks_flags_and_unblockables() {
    let raw = fp_data(false);
    // Exhaustively check every bit independently for user and current flags.
    let expected_mask =
        (1 << 0) | (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 10) | (1 << 11) | (1 << 16) | (1 << 18);
    assert_eq!(FIX_EFLAGS, expected_mask);
    for bit in 0..64 {
        for (user, current) in [(1 << bit, 0), (0, 1 << bit), (u64::MAX, 0x202)] {
            let mut args = input(&raw, false);
            args.registers.rflags = user;
            args.registers.cs = 0;
            args.registers.ss = 0xffff;
            args.mask = Blocked::new(SigSet::ALL);
            let mut image = build(args).unwrap();
            // uc_sigmask is authoritative; the legacy oldmask must be ignored.
            put64(&mut image.bytes, OLD_MASK, 0);
            // This is hostile user memory, not a mask sanitized by FrameInput.
            put64(&mut image.bytes, MASK, u64::MAX);
            let mut cfg = config();
            cfg.current_rflags = current;
            cfg.user_cs = 0x33;
            cfg.user_ss = 0x2b;
            let restored = restore(&image.bytes, Some(image.fp_snapshot()), cfg).unwrap();
            assert_eq!(restored.registers().rflags, (user & expected_mask) | (current & !expected_mask));
            assert_eq!(restored.registers().cs, 0x33);
            assert_eq!(restored.registers().ss, 0x2b);
            assert_eq!(restored.mask().set().0, !(1 << 8) & !(1 << 18));
        }
    }
}

#[test]
fn sigreturn_refuses_truncation_and_invalid_rip() {
    let raw = fp_data(false);
    let image = build(input(&raw, false)).unwrap();
    for length in 0..440 {
        assert_eq!(
            restore(&image.bytes[..length], Some(image.fp_snapshot()), config()).unwrap_err(),
            RestoreError::TruncatedFrame
        );
    }
    for (rip, expected) in [
        (0x0000_8000_0000_0000, RestoreError::NonCanonicalRip),
        (0xffff_7fff_ffff_ffff, RestoreError::NonCanonicalRip),
        (0xffff_8000_0000_0000, RestoreError::NonUserRip),
        (u64::MAX, RestoreError::NonUserRip),
        (USER_MAX, RestoreError::NonUserRip),
    ] {
        let mut bytes = image.bytes.clone();
        put64(&mut bytes, RIP, rip);
        assert_eq!(restore(&bytes, Some(image.fp_snapshot()), config()).unwrap_err(), expected);
    }
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, RIP, USER_MAX - 1);
    assert!(restore(&bytes, Some(image.fp_snapshot()), config()).is_ok());
}

#[test]
fn sigreturn_validates_fp_pointer_mxcsr_and_null_reset() {
    let raw = fp_data(false);
    let image = build(input(&raw, false)).unwrap();
    let original = image.fp_snapshot();
    assert_eq!(restore(&image.bytes, None, config()).unwrap_err(), RestoreError::BadFpState(FpError::Address));
    for address in [original.address + 16, 0] {
        assert_eq!(
            restore(&image.bytes, Some(FpSnapshot { address, ..original }), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Address)
        );
    }
    for (address, expected) in
        [(0x900001, FpError::Alignment), (u64::MAX - 15, FpError::Address), (USER_MAX - 16, FpError::Address)]
    {
        let mut bytes = image.bytes.clone();
        put64(&mut bytes, FP_PTR, address);
        assert_eq!(
            restore(&bytes, Some(FpSnapshot { address, ..original }), config()).unwrap_err(),
            RestoreError::BadFpState(expected)
        );
    }
    for length in 0..512 {
        let fp = FpSnapshot { bytes: &original.bytes[..length], ..original };
        assert_eq!(
            restore(&image.bytes, Some(fp), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Truncated)
        );
    }
    for bit in 16..32 {
        let mut fp_bytes = original.bytes.to_vec();
        put32(&mut fp_bytes, MXCSR, 1 << bit);
        // The frame's own MXCSR_MASK must never override the trusted CPU mask.
        put32(&mut fp_bytes, 28, u32::MAX);
        assert_eq!(
            restore(&image.bytes, Some(FpSnapshot { bytes: &fp_bytes, ..original }), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Mxcsr)
        );
    }
    let mut cfg = config();
    cfg.mxcsr_mask = 0;
    assert_eq!(restore(&image.bytes, Some(original), cfg).unwrap_err(), RestoreError::BadFpState(FpError::Mxcsr));
    let mut zero = fp_data(false);
    put32(&mut zero, MXCSR, 0);
    assert!(restore(&image.bytes, Some(FpSnapshot { bytes: &zero, ..original }), cfg).is_ok());
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, FP_PTR, 0);
    assert!(restore(&bytes, None, config()).unwrap().fp().is_none());
    // A separately copied FP area is legal: pointer need not follow the frame.
    put64(&mut bytes, FP_PTR, 0x600000);
    assert!(restore(&bytes, Some(FpSnapshot { address: 0x600000, ..original }), config()).is_ok());
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn randomized_register_and_fp_round_trip() {
    let mut seed = 0x3751_30ab_998f_e701;
    for iteration in 0..512 {
        let xsave = iteration % 2 != 0;
        let mut raw = fp_data(xsave);
        for byte in &mut raw {
            *byte = random(&mut seed) as u8;
        }
        let mxcsr = random(&mut seed) as u32 & 0xffff;
        put32(&mut raw, MXCSR, mxcsr);
        if xsave {
            raw[512..576].fill(0);
            put64(&mut raw, 512, random(&mut seed) & 7);
        }
        let mut args = input(&raw, xsave);
        args.registers = Registers {
            r8: random(&mut seed),
            r9: random(&mut seed),
            r10: random(&mut seed),
            r11: random(&mut seed),
            r12: random(&mut seed),
            r13: random(&mut seed),
            r14: random(&mut seed),
            r15: random(&mut seed),
            rdi: random(&mut seed),
            rsi: random(&mut seed),
            rbp: random(&mut seed),
            rbx: random(&mut seed),
            rdx: random(&mut seed),
            rax: random(&mut seed),
            rcx: random(&mut seed),
            rsp: 0x800000 + (random(&mut seed) & 0xffff),
            rip: random(&mut seed) & 0x7fff_ffff,
            rflags: random(&mut seed),
            cs: random(&mut seed) as u16,
            ss: random(&mut seed) as u16,
            err: random(&mut seed),
            trapno: random(&mut seed),
            cr2: random(&mut seed),
        };
        args.mask = Blocked::new(SigSet(random(&mut seed)));
        if iteration % 3 != 0 {
            args.alt_stack = AltStack::DISABLED
                .configure(0, 0x900000, 8192, if iteration % 3 == 1 { 0 } else { g::SS_AUTODISARM })
                .unwrap();
            args.handler.flags |= g::SA_ONSTACK as u64;
        }
        let signal = Signal::new((random(&mut seed) % 64 + 1) as u32).unwrap();
        let mut info_bytes = [0; 128];
        for byte in &mut info_bytes {
            *byte = random(&mut seed) as u8;
        }
        args.info = SigInfo::from_user(signal, info_bytes);
        let cfg = RestoreConfig { current_rflags: random(&mut seed), ..config() };
        let image = build(args).unwrap();
        let restored = restore(&image.bytes, Some(image.fp_snapshot()), cfg).unwrap();
        let mut expected = args.registers;
        expected = without_exception(expected);
        expected.cs = 0x23;
        expected.ss = 0x1b;
        expected.rflags = (expected.rflags & 0x50dd5) | (cfg.current_rflags & !0x50dd5);
        assert_eq!(restored.registers(), expected);
        assert_eq!(restored.mask().set().0, args.mask.set().0 & !(1 << 8) & !(1 << 18));
        assert_eq!(restored.alt_stack(), args.alt_stack);
        let fp = restored.fp().unwrap().bytes();
        assert_eq!(&fp[..464], &raw[..464]);
        if xsave {
            assert_eq!(word(fp, XSTATE_BV), word(&raw, XSTATE_BV) | 3);
            assert_eq!(&fp[XCOMP_BV..], &raw[XCOMP_BV..]);
        }
        assert_eq!(&image.bytes[INFO..FRAME_END], args.info.bytes());
        let mut entry = args.registers;
        entry.rip = args.handler.address;
        entry.rsp = image.new_rsp;
        entry.rdi = signal.number() as u64;
        entry.rsi = image.new_rsp + INFO as u64;
        entry.rdx = image.new_rsp + UC_FLAGS as u64;
        entry.rax = 0;
        entry.cs = args.user_cs;
        entry.ss = args.user_ss;
        entry.rflags &= !(0x400 | 0x10000 | 0x100);
        assert_eq!(image.entry, entry);
        assert_eq!(fp.len(), raw.len());
    }
}

#[test]
fn sigreturn_xsave_metadata_falls_back_or_is_ignored() {
    let raw = fp_data(true);
    for (offset, value) in [
        (MAGIC1, 0),
        (MAGIC1, 0x46505852),
        (EXT_SIZE, 832),
        (EXT_SIZE, u32::MAX),
        (STATE_SIZE, 512),
        (STATE_SIZE, 831),
        (STATE_SIZE, u32::MAX),
        (XSIZE, 0),
    ] {
        let mut image = build(input(&raw, true)).unwrap();
        put32(&mut image.bytes, image.fp_offset + offset, value);
        let restored = restore_image(&image).unwrap();
        let fp = restored.fp().unwrap();
        assert_eq!(fp.kind(), FpKind::Fxsave, "offset {offset}");
        assert_eq!(fp.bytes(), &image.fp_snapshot().bytes[..512]);
    }
    let mut image = build(input(&raw, true)).unwrap();
    for offset in SW_PADDING..XSTATE_BV {
        image.bytes[image.fp_offset + offset] = 0xff;
    }
    put64(&mut image.bytes, image.fp_offset + FEATURES, u64::MAX);
    for flags in [0, 6, 7, u64::MAX] {
        put64(&mut image.bytes, UC_FLAGS, flags);
        assert_eq!(restore_image(&image).unwrap().fp().unwrap().kind(), FpKind::Xsave { xfeatures: 7 });
    }
    put64(&mut image.bytes, image.fp_offset + FEATURES, 3);
    assert_eq!(restore_image(&image).unwrap().fp().unwrap().kind(), FpKind::Xsave { xfeatures: 3 });
    for size in [None, Some(576), Some(831), Some(833)] {
        let cfg = RestoreConfig { xsave_size: size, ..config() };
        assert_eq!(restore(&image.bytes, Some(image.fp_snapshot()), cfg).unwrap().fp().unwrap().kind(), FpKind::Fxsave);
    }
    // FXSAVE-only CPUs ignore even nonsensical software metadata and xheaders.
    image.bytes[image.fp_offset + MAGIC1..].fill(0xff);
    let cfg = RestoreConfig { xsave_size: None, ..config() };
    let fp = restore(&image.bytes, Some(image.fp_snapshot()), cfg).unwrap().fp().unwrap();
    assert_eq!(fp.kind(), FpKind::Fxsave);
    assert_eq!(fp.bytes().len(), 512);
    let legacy = fp_data(false);
    let mut image = build(input(&legacy, false)).unwrap();
    put64(&mut image.bytes, UC_FLAGS, 7);
    assert_eq!(restore_image(&image).unwrap().fp().unwrap().kind(), FpKind::Fxsave);
}

#[test]
fn sigreturn_checks_xsave_hardware_safety() {
    let raw = fp_data(true);
    for bit in 3..64 {
        let mut image = build(input(&raw, true)).unwrap();
        put64(&mut image.bytes, image.fp_offset + XSTATE_BV, 1 << bit);
        assert_error(&image, RestoreError::BadFpState(FpError::Features));
    }
    for offset in XCOMP_BV..HEADER_END {
        let mut image = build(input(&raw, true)).unwrap();
        image.bytes[image.fp_offset + offset] = 1;
        assert_error(&image, RestoreError::BadFpState(FpError::Reserved));
    }
    let image = build(input(&raw, true)).unwrap();
    let snapshot = image.fp_snapshot();
    for length in 0..836 {
        let fp = FpSnapshot { bytes: &snapshot.bytes[..length], ..snapshot };
        assert_eq!(
            restore(&image.bytes, Some(fp), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Truncated)
        );
    }
    let cfg = RestoreConfig { enabled_xfeatures: 3, ..config() };
    assert_eq!(restore(&image.bytes, Some(snapshot), cfg).unwrap_err(), RestoreError::BadFpState(FpError::Features));
    for address in [0x900008, 0x900020, 0x900010] {
        let mut bytes = image.bytes.clone();
        put64(&mut bytes, FP_PTR, address);
        assert_eq!(
            restore(&bytes, Some(FpSnapshot { address, ..snapshot }), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Alignment)
        );
    }
}

#[test]
fn sigreturn_keeps_current_altstack_on_invalid_or_onstack_edits() {
    let raw = fp_data(false);
    let current = AltStack::DISABLED.configure(0, 0xa00000, 4096, 0).unwrap();
    for (sp, size, flags) in [(0x900000, 2047, 0), (u64::MAX, 4096, 0), (0x900000, 4096, 4)] {
        let mut image = build(input(&raw, false)).unwrap();
        put64(&mut image.bytes, STACK_SP, sp);
        put32(&mut image.bytes, STACK_FLAGS, flags);
        put64(&mut image.bytes, STACK_SIZE, size);
        let restored =
            lumen_signal::frame::restore(&image.bytes, Some(image.fp_snapshot()), config(), current, 0x800000).unwrap();
        assert_eq!(restored.alt_stack(), current);
        assert_eq!(restored.registers().rip, 0x401234);
    }
    let mut args = input(&raw, false);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
    let image = build(args).unwrap();
    for rsp in [current.sp() + 1, current.top()] {
        let restored =
            lumen_signal::frame::restore(&image.bytes, Some(image.fp_snapshot()), config(), current, rsp).unwrap();
        assert_eq!(restored.alt_stack(), current);
    }
    let restored =
        lumen_signal::frame::restore(&image.bytes, Some(image.fp_snapshot()), config(), current, current.top() + 1)
            .unwrap();
    assert_eq!(restored.alt_stack(), args.alt_stack);
    let auto = AltStack::DISABLED.configure(0, current.sp(), current.size(), g::SS_AUTODISARM).unwrap();
    let restored =
        lumen_signal::frame::restore(&image.bytes, Some(image.fp_snapshot()), config(), auto, auto.top()).unwrap();
    assert_eq!(restored.alt_stack(), args.alt_stack);
}

#[test]
fn frame_applies_restart_before_saving_and_sigreturn_clears_syscall_context() {
    let raw = fp_data(false);
    for code in [-512, -513, -514, -516] {
        for sa_restart in [false, true] {
            let mut args = input(&raw, false);
            args.registers.rax = code as u64;
            args.syscall = Some(SyscallExit { result: code, orig_rax: 77 });
            if sa_restart {
                args.handler.flags |= g::SA_RESTART as u64;
            }
            let image = build(args).unwrap();
            let should_restart = code == -513 || (code == -512 && sa_restart);
            assert_eq!(word(&image.bytes, RAX), if should_restart { 77 } else { (-4i64) as u64 });
            assert_eq!(word(&image.bytes, RIP), args.registers.rip - if should_restart { 2 } else { 0 });
            let restored = restore_image(&image).unwrap();
            assert_eq!(restored.syscall(), None);
            // A handler may write an ERESTART-looking value; sigreturn must not
            // turn that ordinary user RAX into a syscall restart on the next signal.
            let mut user_bytes = image.bytes.clone();
            put64(&mut user_bytes, RAX, code as u64);
            let restored = restore(&user_bytes, Some(image.fp_snapshot()), config()).unwrap();
            args.registers = restored.registers();
            args.syscall = restored.syscall();
            let async_image = build(args).unwrap();
            assert_eq!(word(&async_image.bytes, RAX), code as u64);
            assert_eq!(word(&async_image.bytes, RIP), args.registers.rip);
            let decision = lumen_signal::restart::decide(restored.syscall(), Handling::None);
            assert_eq!(decision, lumen_signal::restart::RestartDecision::Unchanged);
        }
    }
    let mut args = input(&raw, false);
    args.registers.rip = 1;
    args.syscall = Some(SyscallExit { result: -513, orig_rax: 1 });
    assert_eq!(build(args).unwrap_err(), FrameError::RestartUnderflow);
}

#[test]
fn xsave_build_forces_fp_sse_and_return_ignores_exception_metadata() {
    for bv in 0..=7 {
        let mut raw = fp_data(true);
        put64(&mut raw, XSTATE_BV, bv);
        let mut image = build(input(&raw, true)).unwrap();
        assert_eq!(word(image.fp_snapshot().bytes, XSTATE_BV), bv | 3);
        for offset in [200, 208, 224] {
            put64(&mut image.bytes, offset, u64::MAX);
        }
        let restored = restore_image(&image).unwrap();
        assert_eq!((restored.registers().err, restored.registers().trapno, restored.registers().cr2), (0, 0, 0));
        assert_eq!(restored.fp().unwrap().kind(), FpKind::Xsave { xfeatures: 7 });
    }
}

#[test]
fn altstack_red_zone_boundaries_and_strict_base_check() {
    let raw = fp_data(false);
    let mut args = input(&raw, false);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
    args.handler.flags |= g::SA_ONSTACK as u64;
    for delta in 1..=128 {
        args.registers.rsp = args.alt_stack.top() + delta;
        let image = build(args).unwrap();
        assert!(image.new_rsp + image.bytes.len() as u64 <= args.registers.rsp - 128);
        args.registers.rsp = args.alt_stack.sp() + delta;
        let image = build(args).unwrap();
        assert!(image.new_rsp > args.alt_stack.sp());
        assert_eq!(image.fp_snapshot().address, (args.alt_stack.top() - 512) & !63);
    }
    args.registers.rsp = args.alt_stack.sp() + 129;
    assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    args.registers.rsp = args.alt_stack.top() + 129;
    let image = build(args).unwrap();
    assert_eq!(image.fp_snapshot().address, (args.alt_stack.top() - 512) & !63);
    // An armed AUTODISARM stack is not nested for overflow checking.
    args.handler.flags = g::SA_RESTORER as u64;
    // Nested detection uses the original RSP, even when its red zone crosses
    // below the base and SA_ONSTACK is absent.
    args.registers.rsp = args.alt_stack.sp() + 64;
    assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    args.registers.rsp = args.alt_stack.sp() + 512;
    assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, g::SS_AUTODISARM).unwrap();
    assert!(build(args).unwrap().new_rsp < args.alt_stack.sp());
    // Construct an alternate stack whose exact base equals the resulting RSP.
    let big = vec![0; 4096];
    let mut args = input(&big, true);
    args.handler.flags |= g::SA_ONSTACK as u64;
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 16384, 0).unwrap();
    let image = build(args).unwrap();
    let top = args.alt_stack.top();
    args.alt_stack = AltStack::DISABLED.configure(0, image.new_rsp, top - image.new_rsp, 0).unwrap();
    assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    args.alt_stack = AltStack::DISABLED.configure(0, image.new_rsp - 1, top - image.new_rsp + 1, 0).unwrap();
    assert_eq!(build(args).unwrap().new_rsp, image.new_rsp);
}

#[test]
fn raw_stack_flags_and_handler_entry_selectors() {
    let raw = fp_data(false);
    for flags in [0, g::SS_ONSTACK, g::SS_AUTODISARM, g::SS_DISABLE, g::SS_AUTODISARM | g::SS_DISABLE] {
        let mut args = input(&raw, false);
        args.registers.cs = 0;
        args.registers.ss = 0xffff;
        args.user_cs = 0x33;
        args.user_ss = 0x2b;
        args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, flags).unwrap();
        if args.alt_stack.enabled() {
            args.registers.rsp = args.alt_stack.top();
        }
        let image = build(args).unwrap();
        assert_eq!(dword(&image.bytes, STACK_FLAGS), flags);
        assert_eq!(image.entry.cs, 0x33);
        assert_eq!(image.entry.ss, 0x2b);
        assert_eq!(&image.bytes[192..194], &[0, 0]);
        assert_eq!(&image.bytes[198..200], &[0xff, 0xff]);
        assert_eq!(image.alt_stack_during_handler, args.alt_stack.during_handler());
    }
}

#[test]
fn user_max_and_last_alignment_subtraction_boundaries() {
    assert_eq!(USER_MAX, lumen_vm::USER_MAX);
    let raw = fp_data(false);
    for address in [0, 1] {
        let mut args = input(&raw, false);
        args.handler.address = address;
        assert_eq!(build(args).unwrap_err(), FrameError::InvalidAddress);
    }
    for rsp in [1088, 1151] {
        let mut args = input(&raw, false);
        args.registers.rsp = rsp;
        assert_eq!(build(args).unwrap_err(), FrameError::StackOverflow);
    }
    let mut args = input(&raw, false);
    args.registers.rsp = 1152;
    assert_eq!(build(args).unwrap().new_rsp, 56);
    args.registers.rsp = USER_MAX;
    assert!(build(args).is_ok());
    args.alt_stack = AltStack::DISABLED.configure(0, USER_MAX - 8192, 8192, 0).unwrap();
    args.registers.rsp = 0x800000;
    args.handler.flags |= g::SA_ONSTACK as u64;
    assert!(build(args).is_ok());
    let image = build(input(&raw, false)).unwrap();
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, FP_PTR, USER_MAX - 512);
    let snapshot = FpSnapshot { address: USER_MAX - 512, bytes: &raw };
    assert!(restore(&bytes, Some(snapshot), config()).is_ok());
    put64(&mut bytes, FP_PTR, 0x900008);
    assert_eq!(
        restore(&bytes, Some(FpSnapshot { address: 0x900008, bytes: &raw }), config()).unwrap_err(),
        RestoreError::BadFpState(FpError::Alignment)
    );
}
