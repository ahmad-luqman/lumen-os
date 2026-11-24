//! What a `wait4` or `waitid` asks for ([`WaitTarget`], [`WaitOptions`]) and
//! what it gets ([`WaitResult`]).

use crate::{Errno, ExitStatus, Signal, WaitStatus};

/// Return at once if no child is ready.
pub const WNOHANG: u32 = 0x1;
/// Report stopped children too (`wait4`'s name for [`WSTOPPED`]).
pub const WUNTRACED: u32 = 0x2;
/// Report stopped children too (`waitid`'s name for [`WUNTRACED`]).
pub const WSTOPPED: u32 = 0x2;
/// Report children that ended (`waitid`; `wait4` always does).
pub const WEXITED: u32 = 0x4;
/// Report stopped children continued by `SIGCONT`.
pub const WCONTINUED: u32 = 0x8;
/// Report without consuming: a zombie stays, a stop or continue stays
/// unreported (`waitid` only).
pub const WNOWAIT: u32 = 0x0100_0000;
/// Wait only for the caller's own children, not its threads' (accepted; one
/// thread per process here, so it changes nothing).
pub const __WNOTHREAD: u32 = 0x2000_0000;
/// Wait for every child, whatever signal its exit sends.
pub const __WALL: u32 = 0x4000_0000;
/// Wait only for "clone" children, whose exit sends a signal other than
/// `SIGCHLD` (unless [`__WALL`] is set too).
pub const __WCLONE: u32 = 0x8000_0000;

/// `waitid`'s `idtype`: any child.
pub const P_ALL: i32 = 0;
/// `waitid`'s `idtype`: the child with pid `id`.
pub const P_PID: i32 = 1;
/// `waitid`'s `idtype`: any child in process group `id` (0: the caller's).
pub const P_PGID: i32 = 2;
/// `waitid`'s `idtype`: the child a pidfd names (refused: no pidfds yet).
pub const P_PIDFD: i32 = 3;

const WAIT4_ALLOWED: u32 = WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WCLONE | __WALL;
const WAITID_ALLOWED: u32 = WNOHANG | WNOWAIT | WEXITED | WSTOPPED | WCONTINUED | __WNOTHREAD | __WCLONE | __WALL;

/// Which children a wait considers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitTarget {
    /// The child with this pid.
    Pid(u32),
    /// Any child in this process group.
    Pgid(u32),
    /// Any child.
    Any,
}

impl WaitTarget {
    /// `wait4`'s `pid`: `n > 0` the child `n`, `0` any child in the caller's
    /// group (`caller_pgid`), `-1` any child, `n < -1` any child in group
    /// `-n`. `i32::MIN` (whose negation doesn't exist) is `ESRCH`, as in
    /// Linux. Linux checks the options first: call
    /// [`WaitOptions::check`] before this to get its error order.
    pub const fn from_wait4_pid(pid: i32, caller_pgid: u32) -> Result<Self, Errno> {
        match pid {
            i32::MIN => Err(Errno::Srch),
            -1 => Ok(Self::Any),
            0 => Ok(Self::Pgid(caller_pgid)),
            n if n > 0 => Ok(Self::Pid(n as u32)),
            n => Ok(Self::Pgid(n.unsigned_abs())),
        }
    }

    /// `waitid`'s `idtype` and `id`: [`P_ALL`] any child (`id` ignored);
    /// [`P_PID`] the child `id`, `EINVAL` unless `id > 0`; [`P_PGID`] group
    /// `id`, or the caller's group (`caller_pgid`) for `0`, `EINVAL` for a
    /// negative `id`. [`P_PIDFD`] is refused with `EINVAL` until pidfds
    /// exist, as is any other `idtype`.
    pub const fn from_waitid(idtype: i32, id: i32, caller_pgid: u32) -> Result<Self, Errno> {
        match idtype {
            P_ALL => Ok(Self::Any),
            P_PID if id > 0 => Ok(Self::Pid(id as u32)),
            P_PGID if id == 0 => Ok(Self::Pgid(caller_pgid)),
            P_PGID if id > 0 => Ok(Self::Pgid(id as u32)),
            _ => Err(Errno::Inval),
        }
    }

    pub(crate) fn matches(self, pid: u32, pgid: u32) -> bool {
        match self {
            Self::Pid(n) => pid == n,
            Self::Pgid(g) => pgid == g,
            Self::Any => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Wait4,
    Waitid,
}

/// A wait's options word, with the call it came from: `wait4` and `waitid`
/// accept different flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitOptions {
    bits: u32,
    call: Call,
}

impl WaitOptions {
    /// `wait4`'s options. Valid: [`WNOHANG`], [`WUNTRACED`], [`WCONTINUED`],
    /// [`__WNOTHREAD`], [`__WCLONE`], [`__WALL`]; `wait4` always reports
    /// children that ended, as if [`WEXITED`] were set.
    pub const fn wait4(bits: u32) -> Self {
        Self { bits, call: Call::Wait4 }
    }

    /// `waitid`'s options. Valid: [`WNOHANG`], [`WNOWAIT`], [`WEXITED`],
    /// [`WSTOPPED`], [`WCONTINUED`], [`__WNOTHREAD`], [`__WCLONE`],
    /// [`__WALL`], with at least one of `WEXITED`, `WSTOPPED`, `WCONTINUED`.
    pub const fn waitid(bits: u32) -> Self {
        Self { bits, call: Call::Waitid }
    }

    /// `EINVAL` for a flag the call doesn't accept, or a `waitid` asking for
    /// none of exits, stops and continues; [`crate::ProcTree::wait`] answers
    /// [`WaitResult::Invalid`] then.
    pub const fn check(self) -> Result<(), Errno> {
        let ok = match self.call {
            Call::Wait4 => self.bits & !WAIT4_ALLOWED == 0,
            Call::Waitid => self.bits & !WAITID_ALLOWED == 0 && self.bits & (WEXITED | WSTOPPED | WCONTINUED) != 0,
        };
        if ok { Ok(()) } else { Err(Errno::Inval) }
    }

    /// Whether [`WNOHANG`] is set: [`WaitResult::NoneReady`] then returns 0
    /// rather than blocking.
    pub const fn nohang(self) -> bool {
        self.bits & WNOHANG != 0
    }

    /// The flags as the wait sees them, [`WEXITED`] added for `wait4`.
    pub(crate) const fn effective(self) -> u32 {
        match self.call {
            Call::Wait4 => self.bits | WEXITED,
            Call::Waitid => self.bits,
        }
    }

    /// Whether a child whose exit sends `SIGCHLD` (every child here) is
    /// eligible: not when only "clone" children are asked for.
    pub(crate) const fn takes_sigchld_children(self) -> bool {
        self.bits & __WCLONE == 0 || self.bits & __WALL != 0
    }
}

/// What a wait found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitResult {
    /// A zombie child, now removed from the tree: its pid is free again
    /// unless a group or session still uses the number.
    Reaped {
        /// The child's pid.
        pid: u32,
        /// How it ended.
        status: ExitStatus,
        /// The token the kernel gave the child when it was created.
        rusage_slot: u64,
    },
    /// A zombie child reported under [`WNOWAIT`] and left in place.
    Zombie {
        /// The child's pid.
        pid: u32,
        /// How it ended.
        status: ExitStatus,
        /// The token the kernel gave the child when it was created.
        rusage_slot: u64,
    },
    /// A stopped child not reported before (now reported, unless [`WNOWAIT`]).
    Stopped {
        /// The child's pid.
        pid: u32,
        /// The signal that stopped it.
        signal: Signal,
    },
    /// A continued child not reported before (now reported, unless [`WNOWAIT`]).
    Continued {
        /// The child's pid.
        pid: u32,
    },
    /// Some child matches but none is ready: block, or return 0 with
    /// [`WNOHANG`].
    NoneReady,
    /// No child matches (`ECHILD`).
    NoChildren,
    /// The options are invalid (`EINVAL`).
    Invalid,
}

impl WaitResult {
    /// The child reported, if any.
    pub const fn pid(&self) -> Option<u32> {
        match *self {
            Self::Reaped { pid, .. }
            | Self::Zombie { pid, .. }
            | Self::Stopped { pid, .. }
            | Self::Continued { pid } => Some(pid),
            Self::NoneReady | Self::NoChildren | Self::Invalid => None,
        }
    }

    /// What is reported about the child, if any: its status word and
    /// `siginfo` come from it.
    pub fn status(&self) -> Option<WaitStatus> {
        match *self {
            Self::Reaped { status, .. } | Self::Zombie { status, .. } => Some(status.into()),
            Self::Stopped { signal, .. } => Some(WaitStatus::Stopped(signal)),
            Self::Continued { .. } => Some(WaitStatus::Continued),
            Self::NoneReady | Self::NoChildren | Self::Invalid => None,
        }
    }

    /// The error the syscall returns: `ECHILD` or `EINVAL`.
    pub const fn errno(&self) -> Option<Errno> {
        match self {
            Self::NoChildren => Some(Errno::Child),
            Self::Invalid => Some(Errno::Inval),
            _ => None,
        }
    }
}
