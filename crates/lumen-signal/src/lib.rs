//! Linux x86-64 signal policy and byte images, independent of kernel memory and
//! scheduling. Callers serialize pending-state operations and perform user copies.
//! No function dereferences a user address or executes a CPU restore instruction.
#![no_std]

extern crate alloc;

pub mod abi;
pub mod altstack;
pub mod frame;
pub mod pending;
pub mod restart;

use linux_raw_sys::general;

/// A Linux signal number in 1..=64. The kernel real-time range starts at 32;
/// musl reserves signals 32–34 internally, without changing their kernel ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signal(u8);

impl Signal {
    pub const fn new(number: u32) -> Option<Self> {
        if number >= 1 && number <= 64 { Some(Self(number as u8)) } else { None }
    }

    pub const fn number(self) -> u32 {
        self.0 as u32
    }

    pub const fn is_realtime(self) -> bool {
        self.number() >= general::SIGRTMIN
    }

    pub const fn is_unblockable(self) -> bool {
        matches!(self.number(), general::SIGKILL | general::SIGSTOP)
    }

    /// Linux x86-64 defaults from signal(7), including Term for signals 32–64.
    pub const fn default_action(self) -> DefaultAction {
        use DefaultAction::*;
        match self.number() {
            general::SIGQUIT
            | general::SIGILL
            | general::SIGTRAP
            | general::SIGABRT
            | general::SIGBUS
            | general::SIGFPE
            | general::SIGSEGV
            | general::SIGXCPU
            | general::SIGXFSZ
            | general::SIGSYS => Core,
            general::SIGCHLD | general::SIGURG | general::SIGWINCH => Ign,
            general::SIGSTOP | general::SIGTSTP | general::SIGTTIN | general::SIGTTOU => Stop,
            general::SIGCONT => Cont,
            _ => Term,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAction {
    Term,
    Core,
    Ign,
    Stop,
    Cont,
}

/// Linux's 64-bit kernel sigset: bit n-1 represents signal n.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigSet(pub u64);

impl SigSet {
    pub const EMPTY: Self = Self(0);
    pub const ALL: Self = Self(u64::MAX);

    pub const fn contains(self, signal: Signal) -> bool {
        self.0 & (1 << (signal.0 - 1)) != 0
    }

    pub fn insert(&mut self, signal: Signal) {
        self.0 |= 1 << (signal.0 - 1);
    }

    pub fn remove(&mut self, signal: Signal) {
        self.0 &= !(1 << (signal.0 - 1));
    }

    pub const fn unblockable_stripped(self) -> Self {
        Self(self.0 & !((1 << (general::SIGKILL - 1)) | (1 << (general::SIGSTOP - 1))))
    }

    /// Apply a Linux SIG_BLOCK, SIG_UNBLOCK or SIG_SETMASK operation. Invalid
    /// operations return EINVAL; successful masks never block KILL or STOP.
    pub const fn apply(self, how: u32, set: Self) -> Result<Self, i32> {
        let bits = match how {
            general::SIG_BLOCK => self.0 | set.0,
            general::SIG_UNBLOCK => self.0 & !set.0,
            general::SIG_SETMASK => set.0,
            _ => return Err(-(linux_raw_sys::errno::EINVAL as i32)),
        };
        Ok(Self(bits).unblockable_stripped())
    }
}
