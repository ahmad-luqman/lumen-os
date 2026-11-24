use linux_raw_sys::general as g;
use lumen_signal::abi;
use lumen_signal::altstack::{AltStack, StackError};
use lumen_signal::frame::*;
use lumen_signal::pending::SigInfo;
use lumen_signal::{SigSet, Signal};

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
    put32(&mut bytes, 24, 0x1f80);
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
        mask: SigSet(0x1234_5678_9abc_def0),
        handler: Handler { address: 0x402000, flags: g::SA_RESTORER as u64, restorer: 0x403000 },
        alt_stack: AltStack::DISABLED,
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
fn restore_image(image: &FrameImage) -> Result<Restored<'_>, RestoreError> {
    restore(&image.bytes, Some(image.fp_snapshot().unwrap()), config())
}
fn assert_error(image: &FrameImage, expected: RestoreError) {
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
    assert_eq!(word(&image.bytes, 216), args.mask.0);
    assert_eq!(word(&image.bytes, 224), 0xbad000);
    assert_eq!(word(&image.bytes, 232), image.fp_snapshot().unwrap().address);
    assert_eq!(&image.bytes[240..304], &[0; 64]);
    assert_eq!(word(&image.bytes, 304), args.mask.0);
    assert_eq!(&image.bytes[312..440], args.info.bytes());
    let fp_offset = (image.fp_snapshot().unwrap().address - image.new_rsp) as usize;
    assert_eq!(fp_offset, 456);
    assert!(image.bytes[440..fp_offset].iter().all(|&b| b == 0));
    assert_eq!(image.bytes.len(), 456 + 512);
    assert_eq!(image.fp_snapshot().unwrap().bytes, raw);
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
                    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, 0).unwrap();
                    args.handler.flags |= g::SA_ONSTACK as u64;
                }
                let image = build(args).unwrap();
                let fp = image.fp_snapshot().unwrap();
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
                assert_eq!(restore_image(&image).unwrap().registers, args.registers);
                assert_eq!(restore_image(&image).unwrap().alt_stack, args.alt_stack);
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
    assert_eq!(dword(&image.bytes, 32), g::SS_ONSTACK);
    assert_eq!(restore_image(&image).unwrap().alt_stack, args.alt_stack);
    args.alt_stack = AltStack::DISABLED.configure(0, 0x900000, 8192, g::SS_AUTODISARM).unwrap();
    args.registers.rsp = 0x800000;
    let outer = build(args).unwrap();
    assert_eq!(outer.alt_stack_during_handler, AltStack::DISABLED);
    assert_eq!(dword(&outer.bytes, 32), g::SS_AUTODISARM);
    assert_eq!(restore_image(&outer).unwrap().alt_stack, args.alt_stack);
    args.registers = outer.entry;
    args.alt_stack = outer.alt_stack_during_handler;
    let nested = build(args).unwrap();
    assert!(nested.new_rsp + nested.bytes.len() as u64 <= outer.new_rsp - 128);
    assert_eq!(dword(&nested.bytes, 32), g::SS_DISABLE);
    assert_eq!(restore_image(&nested).unwrap().alt_stack, AltStack::DISABLED);
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
    assert_eq!(&image.fp_snapshot().unwrap().bytes[464..512], &[0; 48]);
    assert_eq!(image.entry.rflags, 0x202);
    assert_eq!(restore_image(&image).unwrap().registers.rflags, args.registers.rflags);
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
            args.mask = SigSet::ALL;
            let mut image = build(args).unwrap();
            // uc_sigmask is authoritative; the legacy oldmask must be ignored.
            put64(&mut image.bytes, 216, 0);
            let mut cfg = config();
            cfg.current_rflags = current;
            cfg.user_cs = 0x33;
            cfg.user_ss = 0x2b;
            let restored = restore(&image.bytes, Some(image.fp_snapshot().unwrap()), cfg).unwrap();
            assert_eq!(restored.registers.rflags, (user & expected_mask) | (current & !expected_mask));
            assert_eq!(restored.registers.cs, 0x33);
            assert_eq!(restored.registers.ss, 0x2b);
            assert_eq!(restored.mask.0, !(1 << 8) & !(1 << 18));
        }
    }
}

#[test]
fn sigreturn_refuses_truncation_and_invalid_rip() {
    let raw = fp_data(false);
    let image = build(input(&raw, false)).unwrap();
    for length in 0..440 {
        assert_eq!(
            restore(&image.bytes[..length], Some(image.fp_snapshot().unwrap()), config()).unwrap_err(),
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
        put64(&mut bytes, 176, rip);
        assert_eq!(restore(&bytes, Some(image.fp_snapshot().unwrap()), config()).unwrap_err(), expected);
    }
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, 176, USER_MAX - 1);
    assert!(restore(&bytes, Some(image.fp_snapshot().unwrap()), config()).is_ok());
    let mut truncated = build(input(&raw, false)).unwrap();
    truncated.bytes.truncate(455);
    assert!(truncated.fp_snapshot().is_none());
    let mut wrapping = build(input(&raw, false)).unwrap();
    wrapping.new_rsp = u64::MAX;
    assert!(wrapping.fp_snapshot().is_none());
}

#[test]
fn sigreturn_validates_fp_pointer_mxcsr_and_null_reset() {
    let raw = fp_data(false);
    let image = build(input(&raw, false)).unwrap();
    let original = image.fp_snapshot().unwrap();
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
        put64(&mut bytes, 232, address);
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
        put32(&mut fp_bytes, 24, 1 << bit);
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
    put32(&mut zero, 24, 0);
    assert!(restore(&image.bytes, Some(FpSnapshot { bytes: &zero, ..original }), cfg).is_ok());
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, 232, 0);
    assert!(restore(&bytes, None, config()).unwrap().fp.is_none());
    // A separately copied FP area is legal: pointer need not follow the frame.
    put64(&mut bytes, 232, 0x600000);
    assert!(restore(&bytes, Some(FpSnapshot { address: 0x600000, ..original }), config()).is_ok());
}

#[test]
fn sigreturn_checks_every_xsave_metadata_and_header_field() {
    let raw = fp_data(true);
    let image = build(input(&raw, true)).unwrap();
    let snapshot = image.fp_snapshot().unwrap();
    let fp_offset = (snapshot.address - image.new_rsp) as usize;
    let cases = [
        (464, 0, FpError::Magic),
        (464, 0x4650_5852, FpError::Magic),
        (468, 832, FpError::Size),
        (468, u32::MAX, FpError::Size),
        (480, 512, FpError::Size),
        (480, 831, FpError::Size),
        (480, u32::MAX, FpError::Size),
        (832, 0, FpError::Magic),
    ];
    for (offset, value, expected) in cases {
        let mut broken = build(input(&raw, true)).unwrap();
        put32(&mut broken.bytes, fp_offset + offset, value);
        assert_error(&broken, RestoreError::BadFpState(expected));
    }
    for offset in [472, 512] {
        for bit in 3..64 {
            let mut broken = build(input(&raw, true)).unwrap();
            put64(&mut broken.bytes, fp_offset + offset, 1 << bit);
            assert_error(&broken, RestoreError::BadFpState(FpError::Features));
        }
    }
    // Header bits must also be included in the frame's advertised features.
    let mut broken = build(input(&raw, true)).unwrap();
    put64(&mut broken.bytes, fp_offset + 472, 3);
    assert_error(&broken, RestoreError::BadFpState(FpError::Features));
    for offset in (484..512).chain(520..576) {
        let mut broken = build(input(&raw, true)).unwrap();
        broken.bytes[fp_offset + offset] = 1;
        assert_error(&broken, RestoreError::BadFpState(FpError::Reserved));
    }
    for length in 0..836 {
        let fp = FpSnapshot { bytes: &snapshot.bytes[..length], ..snapshot };
        assert_eq!(
            restore(&image.bytes, Some(fp), config()).unwrap_err(),
            RestoreError::BadFpState(FpError::Truncated)
        );
    }
    for size in [None, Some(576), Some(831), Some(833)] {
        let cfg = RestoreConfig { xsave_size: size, ..config() };
        assert_eq!(restore(&image.bytes, Some(snapshot), cfg).unwrap_err(), RestoreError::BadFpState(FpError::Size));
    }
    let cfg = RestoreConfig { enabled_xfeatures: 3, ..config() };
    assert_eq!(restore(&image.bytes, Some(snapshot), cfg).unwrap_err(), RestoreError::BadFpState(FpError::Features));
    let mut bytes = image.bytes.clone();
    put64(&mut bytes, 232, snapshot.address + 16);
    assert_eq!(
        restore(&bytes, Some(FpSnapshot { address: snapshot.address + 16, ..snapshot }), config()).unwrap_err(),
        RestoreError::BadFpState(FpError::Alignment)
    );
    // Metadata itself claims XSAVE even if a program clears UC_FP_XSTATE.
    put64(&mut bytes, 232, snapshot.address);
    put64(&mut bytes, 8, 6);
    assert!(matches!(restore(&bytes, Some(snapshot), config()).unwrap().fp, Some(FpState::Xsave { .. })));
    // Conversely UC_FP_XSTATE with an ordinary FXSAVE image lacks magic1.
    let fx = fp_data(false);
    let mut legacy = build(input(&fx, false)).unwrap();
    put64(&mut legacy.bytes, 8, 7);
    assert_error(&legacy, RestoreError::BadFpState(FpError::Magic));
}

#[test]
fn sigreturn_rejects_invalid_saved_altstack() {
    let raw = fp_data(false);
    for (sp, size, flags, error) in [
        (0x900000, 2047, 0, StackError::TooSmall),
        (u64::MAX, 4096, 0, StackError::Invalid),
        (0x900000, 4096, 4, StackError::Invalid),
    ] {
        let mut image = build(input(&raw, false)).unwrap();
        put64(&mut image.bytes, 24, sp);
        put32(&mut image.bytes, 32, flags);
        put64(&mut image.bytes, 40, size);
        assert_error(&image, RestoreError::BadAltStack(error));
    }
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
        put32(&mut raw, 24, mxcsr);
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
        args.mask = SigSet(random(&mut seed));
        if iteration % 3 != 0 {
            args.alt_stack = AltStack::DISABLED
                .configure(0, 0x900000, 8192, if iteration % 3 == 1 { 0 } else { g::SS_AUTODISARM })
                .unwrap();
            args.handler.flags |= g::SA_ONSTACK as u64;
        }
        let cfg = RestoreConfig { current_rflags: random(&mut seed), ..config() };
        let image = build(args).unwrap();
        let restored = restore(&image.bytes, Some(image.fp_snapshot().unwrap()), cfg).unwrap();
        let mut expected = args.registers;
        expected.cs = 0x23;
        expected.ss = 0x1b;
        expected.rflags = (expected.rflags & 0x50dd5) | (cfg.current_rflags & !0x50dd5);
        assert_eq!(restored.registers, expected);
        assert_eq!(restored.mask.0, args.mask.0 & !(1 << 8) & !(1 << 18));
        assert_eq!(restored.alt_stack, args.alt_stack);
        let fp = restored.fp.unwrap().bytes();
        assert_eq!(&fp[..464], &raw[..464]);
        assert_eq!(&fp[512..], &raw[512..]);
        assert_eq!(fp.len(), raw.len());
    }
}
