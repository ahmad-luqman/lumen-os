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

/// The original syscall number, supplied by the kernel. Restart status is read
/// from the current saved RAX, never cached here. The kernel must pass None
/// after rt_sigreturn (Linux clears orig_ax and resets the restart block).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyscallExit {
    orig_rax: u64,
}

impl SyscallExit {
    /// Refuses Linux's sentinel so None is the only non-syscall encoding.
    pub const fn new(orig_rax: u64) -> Option<Self> {
        if orig_rax == u64::MAX { None } else { Some(Self { orig_rax }) }
    }
    pub const fn orig_rax(self) -> u64 {
        self.orig_rax
    }
}

/// Whether this return-to-user path delivers a handler, including SA_RESTART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handling {
    None,
    Handler { sa_restart: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    Unchanged,
    Restart { rax: u64 },
    ReturnEintr,
}

/// No syscall context means no register change, even if user RAX resembles an
/// internal restart code. RESTARTBLOCK without a handler uses restart_syscall
/// (219); the kernel must retain its associated restart-block state.
#[must_use]
pub const fn decide(syscall: Option<SyscallExit>, rax: u64, handling: Handling) -> RestartDecision {
    use RestartCode::*;
    use RestartDecision::*;
    let Some(exit) = syscall else { return Unchanged };
    let Some(code) = RestartCode::from_kernel_result(rax as i64) else { return Unchanged };
    match (code, handling) {
        (NoIntr, _) | (Sys, Handling::None | Handling::Handler { sa_restart: true }) | (NoHand, Handling::None) => {
            Restart { rax: exit.orig_rax }
        }
        (RestartBlock, Handling::None) => Restart { rax: RESTART_SYSCALL },
        _ => ReturnEintr,
    }
}

impl RestartDecision {
    /// Apply atomically. A restart with RIP below 2 is refused; otherwise RIP
    /// is rewound by the two-byte x86-64 syscall instruction.
    pub fn apply(self, registers: &mut Registers) -> Result<(), RestartUnderflow> {
        match self {
            Self::Unchanged => {}
            Self::ReturnEintr => registers.rax = (-(linux_raw_sys::errno::EINTR as i64)) as u64,
            Self::Restart { rax } => {
                let rip = registers.rip.checked_sub(2).ok_or(RestartUnderflow)?;
                registers.rax = rax;
                registers.rip = rip;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartUnderflow;

/// x86-64 __NR_restart_syscall, independent of the host architecture.
pub const RESTART_SYSCALL: u64 = 219;
#[cfg(target_arch = "x86_64")]
const _: () = assert!(linux_raw_sys::general::__NR_restart_syscall as u64 == RESTART_SYSCALL);
