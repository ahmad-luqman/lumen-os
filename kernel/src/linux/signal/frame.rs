//! User copies and CPU state for Linux signal frames. Validation and frame
//! layout belong to lumen-signal; user memory is never read under siglock.

use crate::arch::{fpu, gdt, syscall::SyscallFrame};
use crate::linux::process;
use crate::{sched, user};
use alloc::vec;
use core::mem::offset_of;
use lumen_signal::{abi, frame as wire};

const _: () = assert!(wire::USER_MAX == user::USER_MAX);
macro_rules! copy_gprs { ($a:ident,$b:ident; $($f:ident),*) => { $($a.$f = $b.$f;)* }; }
macro_rules! registers {
    ($m:ident, $a:ident, $b:ident) => {
        $m!($a, $b; r8,r9,r10,r11,r12,r13,r14,r15,rdi,rsi,rbp,rbx,rdx,rax,rcx,rsp,rip,rflags);
    };
}
pub fn registers(frame: &SyscallFrame) -> wire::Registers {
    let mut out = wire::Registers { cs: frame.cs as u16, ss: frame.ss as u16, ..Default::default() };
    registers!(copy_gprs, out, frame);
    out
}
pub fn install(frame: &mut SyscallFrame, regs: wire::Registers) {
    registers!(copy_gprs, frame, regs);
    frame.cs = u64::from(regs.cs);
    frame.ss = u64::from(regs.ss);
}
pub fn from_trap(trap: &crate::arch::idt::TrapFrame) -> SyscallFrame {
    let mut out = SyscallFrame::default();
    registers!(copy_gprs, out, trap);
    out.cs = trap.cs;
    out.ss = trap.ss;
    out
}
pub fn to_trap(trap: &mut crate::arch::idt::TrapFrame, frame: &SyscallFrame) {
    registers!(copy_gprs, trap, frame);
    trap.cs = frame.cs;
    trap.ss = frame.ss;
}

/// Copy a selected handler frame with no signal lock held. Only a successful
/// copy installs its mask, alternate stack and initial FP state.
pub fn deliver(
    frame: &mut SyscallFrame,
    delivery: process::HandlerDelivery,
    syscall: Option<lumen_signal::restart::SyscallExit>,
) -> Result<(), lumen_signal::Signal> {
    let process::HandlerDelivery { info, action, blocked, stack, fault } = delivery;
    let sig = info.signal();
    let fp = sched::snapshot_user_fpu().expect("signal on user thread");
    let raw = match fpu::mode() {
        fpu::FpuMode::FxSave => wire::FpState::Fxsave(fp.as_bytes()),
        fpu::FpuMode::XSave { size, xcr0 } => {
            wire::FpState::Xsave { bytes: fp.as_bytes(), xfeatures: xcr0, configured_size: size }
        }
    };
    let mut regs = registers(frame);
    if let Some(fault) = fault {
        (regs.trapno, regs.err, regs.cr2) = fault.trap_metadata();
    }
    let image = wire::build(wire::FrameInput {
        registers: regs,
        fp: raw,
        info,
        saved_mask: blocked,
        blocked,
        handler: wire::Handler {
            address: action.handler,
            flags: action.flags,
            restorer: action.restorer,
            sa_mask: lumen_signal::SigSet(action.mask),
        },
        alt_stack: stack,
        syscall,
        user_cs: gdt::USER_CODE,
        user_ss: gdt::USER_DATA,
    })
    .map_err(|error| {
        match error {
            wire::FrameError::BadFpState(_) | wire::FrameError::NoMemory | wire::FrameError::RestartUnderflow => {
                log::warn!(target: "signal", "signal frame build: {error:?}")
            }
            _ => log::debug!(target: "signal", "signal frame build: {error:?}"),
        }
        sig
    })?;
    user::copy_to_user(image.new_rsp(), image.bytes()).map_err(|error| {
        log::debug!(target: "signal", "signal frame copyout: {error:?}");
        sig
    })?;
    process::current_thread().install_signal_context(image.blocked_during_handler(), image.alt_stack_during_handler());
    sched::restore_signal_fpu(None);
    install(frame, image.entry());
    let _ = sched::take_restart_block();
    Ok(())
}

fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Restore a user-edited frame, returning failure before any CPU state is
/// installed if a copy or validation fails.
pub fn restore(frame: &mut SyscallFrame) -> Result<(), ()> {
    let base = frame.rsp.checked_sub(8).ok_or_else(|| {
        log::debug!(target: "signal", "sigreturn stack underflow");
    })?;
    let mut bytes = [0; abi::FRAME_SIZE];
    user::copy_from_user(&mut bytes, base).map_err(|error| {
        log::debug!(target: "signal", "sigreturn copy: {error:?}");
    })?;
    let fp_address = word(&bytes, offset_of!(abi::RtSigFrame, uc.mcontext.fpstate));
    let live = sched::snapshot_user_fpu().ok_or(())?;
    let mxcsr_mask = dword(live.as_bytes(), 28);
    let (enabled_xfeatures, xsave_size) = match fpu::mode() {
        fpu::FpuMode::FxSave => (3, None),
        fpu::FpuMode::XSave { xcr0, size } => (xcr0, Some(size)),
    };
    let mut fp = vec![0; abi::FXSAVE_SIZE];
    if fp_address != 0 {
        user::copy_from_user(&mut fp, fp_address).map_err(|error| {
            log::debug!(target: "signal", "sigreturn copy: {error:?}");
        })?;
        let size = dword(&fp, abi::SW_XSTATE_SIZE) as usize;
        if dword(&fp, abi::SW_MAGIC1) == abi::FP_XSTATE_MAGIC1
            && xsave_size.is_some_and(|max| (576..=max).contains(&size))
        {
            fp.resize(size + 4, 0);
            user::copy_from_user(&mut fp, fp_address).map_err(|error| {
                log::debug!(target: "signal", "sigreturn copy: {error:?}");
            })?;
        }
    }
    let layout = fpu::signal_xfeature_layout();
    let me = process::current_thread();
    let restored = wire::restore(
        &bytes,
        (fp_address != 0).then_some(wire::FpSnapshot { address: fp_address, bytes: &fp }),
        wire::RestoreConfig {
            current_rflags: frame.r11,
            user_cs: gdt::USER_CODE,
            user_ss: gdt::USER_DATA,
            mxcsr_mask: if mxcsr_mask == 0 { 0xffbf } else { mxcsr_mask },
            enabled_xfeatures,
            xsave_size,
            xfeature_layout: &layout,
        },
        me.alt_stack(),
        frame.rsp,
    )
    .map_err(|error| {
        log::debug!(target: "signal", "sigreturn validation: {error:?}");
    })?;
    me.install_signal_context(restored.mask(), restored.alt_stack());
    sched::restore_signal_fpu(restored.fp());
    install(frame, restored.registers());
    let _ = sched::take_restart_block();
    Ok(())
}

crate::ktest!(
    fn signal_frames_restore_registers_masks_and_restart_once() {
        use crate::linux::{general, signal};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet, Signal, restart::SyscallExit};
        process::in_test_process(|_| {
            let me = process::current_thread();
            for restart in [false, true] {
                let action = process::KernelSigaction {
                    handler: 0x400000,
                    restorer: 0x400100,
                    flags: u64::from(general::SA_RESTORER | if restart { general::SA_RESTART } else { 0 }),
                    mask: Signal::USR2.bit(),
                };
                process::set_current_action(Signal::USR1, action);
                let mut before = SyscallFrame {
                    r8: 8,
                    r9: 9,
                    r10: 10,
                    r11: 11,
                    r12: 12,
                    r13: 13,
                    r14: 14,
                    r15: 15,
                    rdi: 17,
                    rsi: 18,
                    rbp: 19,
                    rbx: 20,
                    rdx: 21,
                    rcx: 22,
                    rsp: process::KTEST_USER_VA + process::KTEST_USER_LEN,
                    rip: 0x400202,
                    cs: u64::from(gdt::USER_CODE),
                    ss: u64::from(gdt::USER_DATA),
                    rflags: 0x202,
                    rax: (-512i64) as u64,
                };
                me.set_blocked(Blocked::new(SigSet::of(&[Signal::ALRM])));
                process::send_thread(
                    None,
                    me.tid(),
                    Some(lumen_signal::pending::SigInfo::new(Signal::USR1, 0)),
                    lumen_signal::pending::SenderClass::Other,
                )
                .map_err(|_| "send")?;
                let mut frame = before.clone();
                kassert!(signal::exit_to_user(&mut frame, SyscallExit::new(77)));
                kassert_eq!(frame.rip, action.handler);
                kassert_eq!(frame.rdi, u64::from(Signal::USR1.number()));
                kassert_eq!(me.blocked().set(), SigSet::of(&[Signal::ALRM, Signal::USR1, Signal::USR2]));
                frame.rsp += 8;
                frame.r11 = 0x202;
                restore(&mut frame).map_err(|_| "restore")?;
                before.rax = if restart { 77 } else { (-4i64) as u64 };
                if restart {
                    before.rip -= 2;
                }
                kassert_eq!(registers(&frame), registers(&before));
                kassert_eq!(me.blocked().set(), SigSet::of(&[Signal::ALRM]));
                frame.rax = (-512i64) as u64;
                let rip = frame.rip;
                signal::exit_to_user(&mut frame, None);
                kassert_eq!((frame.rax, frame.rip), ((-512i64) as u64, rip));
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn stack_and_simd_exceptions_map_to_signals() {
        use crate::kassert_eq;
        use lumen_signal::{Signal, pending::SigInfo};
        process::in_test_process(|_| {
            let me = process::current_thread();
            process::set_current_action(
                Signal::BUS,
                process::KernelSigaction { handler: 0x400000, ..Default::default() },
            );
            let f = SyscallFrame { rip: 0x400100, rsp: 1 << 63, ..Default::default() };
            let r = registers(&f);
            let mut trap = crate::arch::idt::TrapFrame {
                r8: r.r8,
                r9: r.r9,
                r10: r.r10,
                r11: r.r11,
                r12: r.r12,
                r13: r.r13,
                r14: r.r14,
                r15: r.r15,
                rdi: r.rdi,
                rsi: r.rsi,
                rbp: r.rbp,
                rbx: r.rbx,
                rdx: r.rdx,
                rax: r.rax,
                rcx: r.rcx,
                rsp: r.rsp,
                rip: r.rip,
                rflags: r.rflags,
                cs: 0x23,
                ss: 0x1b,
                vector: 12,
                error_code: 0,
            };
            crate::linux::signal::exception(&trap, 0, false, false);
            let process::Delivery::Handler(process::HandlerDelivery { info, .. }) = me.process().next_signal(me.tid())
            else {
                return Err("SS did not select BUS handler");
            };
            kassert_eq!(info, SigInfo::fault(Signal::BUS, 128, 0));
            // Install a user-edited FP image through the real sigreturn path:
            // divide-by-zero status set, its mask clear. This exercises the
            // #XM mapping even on TCG versions that do not raise #XM.
            process::set_current_action(
                Signal::USR1,
                process::KernelSigaction {
                    handler: 0x400000,
                    restorer: 0x400200,
                    flags: u64::from(crate::linux::general::SA_RESTORER),
                    ..Default::default()
                },
            );
            process::send_thread(
                None,
                me.tid(),
                Some(SigInfo::new(Signal::USR1, 0)),
                lumen_signal::pending::SenderClass::Other,
            )
            .map_err(|_| "send")?;
            let mut frame = SyscallFrame {
                rip: 0x400100,
                rsp: process::KTEST_USER_VA + process::KTEST_USER_LEN,
                cs: u64::from(gdt::USER_CODE),
                ss: u64::from(gdt::USER_DATA),
                rflags: 0x202,
                ..Default::default()
            };
            super::exit_to_user(&mut frame, None);
            let mut bytes = [0; abi::FRAME_SIZE];
            user::copy_from_user(&mut bytes, frame.rsp).map_err(|_| "read frame")?;
            let fp = word(&bytes, offset_of!(abi::RtSigFrame, uc.mcontext.fpstate));
            user::copy_to_user(fp + 24, &0x1d84u32.to_le_bytes()).map_err(|_| "set SIMD status")?;
            frame.rsp += 8;
            frame.r11 = 0x202;
            restore(&mut frame).map_err(|_| "restore SIMD status")?;
            process::set_current_action(
                Signal::FPE,
                process::KernelSigaction { handler: 0x400000, ..Default::default() },
            );
            trap.vector = 19;
            crate::linux::signal::exception(&trap, 0, false, false);
            let process::Delivery::Handler(process::HandlerDelivery { info, .. }) = me.process().next_signal(me.tid())
            else {
                return Err("XM did not select FPE handler");
            };
            kassert_eq!(info, SigInfo::fault(Signal::FPE, 3, trap.rip));
            sched::restore_signal_fpu(None);
            // Spurious x87/SIMD exceptions with no unmasked status send nothing.
            for vector in [16, 19] {
                trap.vector = vector;
                crate::linux::signal::exception(&trap, 0, false, false);
                crate::kassert!(matches!(me.process().next_signal(me.tid()), process::Delivery::None));
            }
            Ok(())
        })?
    }
);
