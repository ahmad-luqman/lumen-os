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

/// An interrupted syscall, supplied by the kernel rather than inferred from RAX.
/// None represents Linux's orig_ax == -1, including every successful sigreturn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyscallExit {
    pub result: i64,
    pub orig_rax: u64,
}

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
pub const fn decide(syscall: Option<SyscallExit>, handling: Handling) -> RestartDecision {
    use RestartCode::*;
    use RestartDecision::*;
    let Some(exit) = syscall else { return Unchanged };
    if exit.orig_rax == u64::MAX {
        return Unchanged;
    }
    let Some(code) = RestartCode::from_kernel_result(exit.result) else { return Unchanged };
    match (code, handling) {
        (NoIntr, _) | (Sys, Handling::None | Handling::Handler { sa_restart: true }) | (NoHand, Handling::None) => {
            Restart { rax: exit.orig_rax }
        }
        (RestartBlock, Handling::None) => Restart { rax: 219 },
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

#[cfg(target_arch = "x86_64")]
const _: () = assert!(linux_raw_sys::general::__NR_restart_syscall == 219);
