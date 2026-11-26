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
pub mod timer;

use linux_raw_sys::general;

/// A Linux signal number in 1..=64. The kernel real-time range starts at 32;
/// musl reserves signals 32–34 internally, without changing their kernel ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signal(u8);

impl Signal {
    pub const HUP: Self = Self(general::SIGHUP as u8);
    pub const INT: Self = Self(general::SIGINT as u8);
    pub const QUIT: Self = Self(general::SIGQUIT as u8);
    pub const ILL: Self = Self(general::SIGILL as u8);
    pub const TRAP: Self = Self(general::SIGTRAP as u8);
    pub const ABRT: Self = Self(general::SIGABRT as u8);
    pub const BUS: Self = Self(general::SIGBUS as u8);
    pub const FPE: Self = Self(general::SIGFPE as u8);
    pub const KILL: Self = Self(general::SIGKILL as u8);
    pub const USR1: Self = Self(general::SIGUSR1 as u8);
    pub const SEGV: Self = Self(general::SIGSEGV as u8);
    pub const USR2: Self = Self(general::SIGUSR2 as u8);
    pub const PIPE: Self = Self(general::SIGPIPE as u8);
    pub const ALRM: Self = Self(general::SIGALRM as u8);
    pub const TERM: Self = Self(general::SIGTERM as u8);
    pub const STKFLT: Self = Self(general::SIGSTKFLT as u8);
    pub const CHLD: Self = Self(general::SIGCHLD as u8);
    pub const CONT: Self = Self(general::SIGCONT as u8);
    pub const STOP: Self = Self(general::SIGSTOP as u8);
    pub const TSTP: Self = Self(general::SIGTSTP as u8);
    pub const TTIN: Self = Self(general::SIGTTIN as u8);
    pub const TTOU: Self = Self(general::SIGTTOU as u8);
    pub const URG: Self = Self(general::SIGURG as u8);
    pub const XCPU: Self = Self(general::SIGXCPU as u8);
    pub const XFSZ: Self = Self(general::SIGXFSZ as u8);
    pub const VTALRM: Self = Self(general::SIGVTALRM as u8);
    pub const PROF: Self = Self(general::SIGPROF as u8);
    pub const WINCH: Self = Self(general::SIGWINCH as u8);
    pub const IO: Self = Self(general::SIGIO as u8);
    pub const PWR: Self = Self(general::SIGPWR as u8);
    pub const SYS: Self = Self(general::SIGSYS as u8);
    pub const RTMIN: Self = Self(32);
    pub const RTMAX: Self = Self(64);

    pub const fn index(self) -> usize {
        self.0 as usize - 1
    }
    pub const fn bit(self) -> u64 {
        1 << self.index()
    }

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

    pub const fn of(signals: &[Signal]) -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < signals.len() {
            bits |= signals[index].bit();
            index += 1;
        }
        Self(bits)
    }

    pub const fn contains(self, signal: Signal) -> bool {
        self.0 & signal.bit() != 0
    }

    pub fn insert(&mut self, signal: Signal) {
        self.0 |= signal.bit();
    }

    pub fn remove(&mut self, signal: Signal) {
        self.0 &= !signal.bit();
    }

    #[must_use]
    pub const fn unblockable_stripped(self) -> Self {
        Self(self.0 & !(Signal::KILL.bit() | Signal::STOP.bit()))
    }

    /// Apply a Linux SIG_BLOCK, SIG_UNBLOCK or SIG_SETMASK operation. Invalid
    /// operations return -EINVAL; successful masks never block KILL or STOP.
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

/// A thread's blocked mask. Every constructor and mask operation removes KILL
/// and STOP. User interest sets (sigwait) and pending sets remain [`SigSet`].
/// ```compile_fail
/// use lumen_signal::{Blocked, SigSet};
/// fn corrupt(mask: &mut Blocked) { mask.0 = SigSet::ALL; }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blocked(SigSet);

impl Blocked {
    pub const EMPTY: Self = Self(SigSet::EMPTY);
    #[must_use]
    pub const fn new(set: SigSet) -> Self {
        Self(set.unblockable_stripped())
    }
    pub const fn set(self) -> SigSet {
        self.0
    }
    pub const fn contains(self, signal: Signal) -> bool {
        self.0.contains(signal)
    }
    pub const fn apply(self, how: u32, set: SigSet) -> Result<Self, i32> {
        match self.0.apply(how, set) {
            Ok(set) => Ok(Self::new(set)),
            Err(error) => Err(error),
        }
    }
}

/// Linux's stop-signal numbers, for process-wide stop/continue cancellation.
pub const STOP_LIST: [Signal; 4] = [Signal::STOP, Signal::TSTP, Signal::TTIN, Signal::TTOU];
/// Set form of STOP_LIST for membership tests.
pub const STOP_SIGNALS: SigSet = SigSet::of(&STOP_LIST);
