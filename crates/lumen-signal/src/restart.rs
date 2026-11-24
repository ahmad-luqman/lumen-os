//! Linux arch/x86/kernel/signal.c handle_signal / arch_do_signal_or_restart.

use crate::frame::Registers;

/// Kernel-internal Linux restart codes (include/linux/errno.h). A syscall result
/// contains their negative value; these numbers must never reach user space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum RestartCode {
    Sys = 512,
    NoIntr = 513,
    NoHand = 514,
    RestartBlock = 516,
}

impl RestartCode {
    pub const fn from_kernel_result(result: i64) -> Option<Self> {
        match result {
            -512 => Some(Self::Sys),
            -513 => Some(Self::NoIntr),
            -514 => Some(Self::NoHand),
            -516 => Some(Self::RestartBlock),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartSyscall {
    Original,
    /// Linux uses __NR_restart_syscall, not orig_rax, for RESTARTBLOCK without
    /// a handler. The kernel must retain the associated restart-block state.
    RestartBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    Restart(RestartSyscall),
    ReturnEintr,
}

/// `handler` is None when no handler runs, or Some(SA_RESTART is set).
pub const fn decide(code: RestartCode, handler: Option<bool>) -> RestartDecision {
    use RestartCode::*;
    use RestartDecision::*;
    match (code, handler) {
        (NoIntr, _) | (Sys, None | Some(true)) | (NoHand, None) => Restart(RestartSyscall::Original),
        (RestartBlock, None) => Restart(RestartSyscall::RestartBlock),
        _ => ReturnEintr,
    }
}

impl RestartDecision {
    /// Apply the decision atomically. A restart with RIP below 2 is refused;
    /// otherwise RIP is rewound by the two-byte x86-64 syscall instruction.
    pub fn apply(self, registers: &mut Registers, orig_rax: u64) -> Result<(), RestartUnderflow> {
        match self {
            Self::ReturnEintr => registers.rax = (-(linux_raw_sys::errno::EINTR as i64)) as u64,
            Self::Restart(kind) => {
                let rip = registers.rip.checked_sub(2).ok_or(RestartUnderflow)?;
                registers.rax = match kind {
                    RestartSyscall::Original => orig_rax,
                    // x86-64 unistd_64.h; the host's syscall number may differ.
                    RestartSyscall::RestartBlock => 219,
                };
                registers.rip = rip;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartUnderflow;

#[cfg(target_arch = "x86_64")]
const _: () = assert!(linux_raw_sys::general::__NR_restart_syscall == 219);
