//! Signals, exit statuses, `wait4`'s status words and `waitid`'s
//! `si_code`/`si_status`.

/// `waitid`'s `si_code`: the child called `exit`.
pub const CLD_EXITED: i32 = 1;
/// `waitid`'s `si_code`: the child was killed by a signal.
pub const CLD_KILLED: i32 = 2;
/// `waitid`'s `si_code`: the child was killed by a signal and dumped core.
pub const CLD_DUMPED: i32 = 3;
/// `waitid`'s `si_code`: the child was stopped by a signal.
pub const CLD_STOPPED: i32 = 5;
/// `waitid`'s `si_code`: the stopped child was continued by `SIGCONT`.
pub const CLD_CONTINUED: i32 = 6;

/// A signal number, always in `1..=64` (Linux x86-64's signals).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Signal(u8);

impl Signal {
    /// The highest signal number (`SIGRTMAX`).
    pub const MAX: u8 = 64;
    /// Hangup (1): sent to a newly orphaned process group with a stopped member.
    pub const SIGHUP: Self = Self(1);
    /// Kill (9).
    pub const SIGKILL: Self = Self(9);
    /// Child status changed (17).
    pub const SIGCHLD: Self = Self(17);
    /// Continue (18): sent after `SIGHUP` to a newly orphaned group.
    pub const SIGCONT: Self = Self(18);
    /// Stop (19).
    pub const SIGSTOP: Self = Self(19);
    /// Terminal stop (20).
    pub const SIGTSTP: Self = Self(20);
    /// Background read from the terminal (21).
    pub const SIGTTIN: Self = Self(21);
    /// Background write or terminal control (22).
    pub const SIGTTOU: Self = Self(22);

    /// The signal numbered `n`, or `None` outside `1..=64`.
    pub const fn new(n: u8) -> Option<Self> {
        if n >= 1 && n <= Self::MAX { Some(Self(n)) } else { None }
    }

    /// The signal's number, in `1..=64`.
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// How a process ended: what a zombie holds until it is reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// It called `exit`/`exit_group` with this code (the low 8 bits).
    Exited(u8),
    /// A signal killed it, dumping core if `core`.
    Killed {
        /// The signal that killed it.
        signal: Signal,
        /// Whether it dumped core.
        core: bool,
    },
}

impl ExitStatus {
    /// The status of `exit(code)`: only the low 8 bits are kept, as Linux does.
    pub const fn exited(code: i32) -> Self {
        Self::Exited(code as u8)
    }
}

/// What a wait reports about a child: its end, a stop or a continue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    /// It exited with this code.
    Exited(u8),
    /// A signal killed it, dumping core if `core`.
    Killed {
        /// The signal that killed it.
        signal: Signal,
        /// Whether it dumped core.
        core: bool,
    },
    /// It was stopped by this signal.
    Stopped(Signal),
    /// It was continued by `SIGCONT`.
    Continued,
}

/// `waitid`'s `siginfo_t` fields for a child: `si_code` and `si_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildInfo {
    /// `si_code`: one of the `CLD_*` constants.
    pub code: i32,
    /// `si_status`: the exit code, or the signal that killed, stopped or
    /// continued the child.
    pub status: i32,
}

impl From<ExitStatus> for WaitStatus {
    fn from(status: ExitStatus) -> Self {
        match status {
            ExitStatus::Exited(code) => Self::Exited(code),
            ExitStatus::Killed { signal, core } => Self::Killed { signal, core },
        }
    }
}

impl WaitStatus {
    /// `wait4`'s status word, which the `WIF*`/`W*` macros of glibc and musl
    /// decode: exited `code << 8`; killed `signal`, `| 0x80` with a core
    /// dump; stopped `(signal << 8) | 0x7f`; continued `0xffff`.
    pub const fn word(self) -> i32 {
        match self {
            Self::Exited(code) => (code as i32) << 8,
            Self::Killed { signal, core } => signal.0 as i32 | if core { 0x80 } else { 0 },
            Self::Stopped(signal) => ((signal.0 as i32) << 8) | 0x7f,
            Self::Continued => 0xffff,
        }
    }

    /// `waitid`'s `si_code` and `si_status`: `CLD_EXITED` with the exit code,
    /// `CLD_KILLED` or `CLD_DUMPED` with the signal, `CLD_STOPPED` with the
    /// stop signal, `CLD_CONTINUED` with `SIGCONT`.
    pub const fn siginfo(self) -> ChildInfo {
        let (code, status) = match self {
            Self::Exited(code) => (CLD_EXITED, code as i32),
            Self::Killed { signal, core: false } => (CLD_KILLED, signal.0 as i32),
            Self::Killed { signal, core: true } => (CLD_DUMPED, signal.0 as i32),
            Self::Stopped(signal) => (CLD_STOPPED, signal.0 as i32),
            Self::Continued => (CLD_CONTINUED, Signal::SIGCONT.0 as i32),
        };
        ChildInfo { code, status }
    }
}
