//! What a `wait4` or `waitid` asks for ([`WaitRequest`]: its [`WaitTarget`]
//! and [`WaitOptions`], checked in Linux's order) and what it gets
//! ([`WaitReady`], [`Report`]).

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
/// Don't wait for children of the caller's other threads (accepted; with
/// one thread per process here it changes nothing).
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
    /// Any child in this process group (a `0` in the call already resolved
    /// to the caller's group).
    Pgid(u32),
    /// Any child.
    Any,
}

impl WaitTarget {
    /// `wait4`'s `pid`, after its options were checked: `n > 0` the child
    /// `n`, `0` any child in the caller's group (`caller_pgid`), `-1` any
    /// child, `n < -1` any child in group `-n`. `i32::MIN` (whose negation
    /// doesn't exist) is `ESRCH`, as in Linux.
    pub(crate) const fn from_wait4_pid(pid: i32, caller_pgid: u32) -> Result<Self, Errno> {
        match pid {
            i32::MIN => Err(Errno::Srch),
            -1 => Ok(Self::Any),
            0 => Ok(Self::Pgid(caller_pgid)),
            n if n > 0 => Ok(Self::Pid(n as u32)),
            n => Ok(Self::Pgid(n.unsigned_abs())),
        }
    }

    /// `waitid`'s `idtype` and `id`, after its options were checked (Linux
    /// checks the options first): [`P_ALL`] any child (`id` ignored);
    /// [`P_PID`] the child `id`, `EINVAL` unless `id > 0`; [`P_PGID`] group
    /// `id`, or the caller's group (`caller_pgid`) for `0`, `EINVAL` for a
    /// negative `id`. [`P_PIDFD`] is refused with `EINVAL` until pidfds
    /// exist, as is any other `idtype`.
    pub(crate) const fn from_waitid(idtype: i32, id: i32, caller_pgid: u32) -> Result<Self, Errno> {
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

/// A wait's options, checked: only [`crate::ProcTree::wait4_request`] and
/// [`crate::ProcTree::waitid_request`] build one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitOptions {
    /// The flags as the wait sees them: [`WEXITED`] added for `wait4`.
    bits: u32,
}

impl WaitOptions {
    /// `wait4`'s options: [`WNOHANG`], [`WUNTRACED`], [`WCONTINUED`],
    /// [`__WNOTHREAD`], [`__WCLONE`], [`__WALL`]; `EINVAL` for any other
    /// bit. `wait4` always reports children that ended, as if [`WEXITED`]
    /// were set.
    pub(crate) const fn wait4(bits: u32) -> Result<Self, Errno> {
        if bits & !WAIT4_ALLOWED != 0 {
            return Err(Errno::Inval);
        }
        Ok(Self { bits: bits | WEXITED })
    }

    /// `waitid`'s options: [`WNOHANG`], [`WNOWAIT`], [`WEXITED`],
    /// [`WSTOPPED`], [`WCONTINUED`], [`__WNOTHREAD`], [`__WCLONE`],
    /// [`__WALL`], with at least one of `WEXITED`, `WSTOPPED`,
    /// `WCONTINUED`; `EINVAL` otherwise.
    pub(crate) const fn waitid(bits: u32) -> Result<Self, Errno> {
        if bits & !WAITID_ALLOWED != 0 || bits & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
            return Err(Errno::Inval);
        }
        Ok(Self { bits })
    }

    /// Whether [`WNOHANG`] is set: [`WaitReady::NoneReady`] then returns 0
    /// rather than blocking.
    pub const fn nohang(self) -> bool {
        self.bits & WNOHANG != 0
    }

    /// Whether a report consumes what it reports (no [`WNOWAIT`]).
    pub(crate) const fn consumes(self) -> bool {
        self.bits & WNOWAIT == 0
    }

    pub(crate) const fn exited(self) -> bool {
        self.bits & WEXITED != 0
    }

    pub(crate) const fn stopped(self) -> bool {
        self.bits & WSTOPPED != 0
    }

    pub(crate) const fn continued(self) -> bool {
        self.bits & WCONTINUED != 0
    }

    /// Whether a child whose exit sends `SIGCHLD` (every child here) is
    /// eligible: not when only "clone" children are asked for.
    pub(crate) const fn takes_sigchld_children(self) -> bool {
        self.bits & __WCLONE == 0 || self.bits & __WALL != 0
    }
}

/// A checked `wait4` or `waitid` by one caller: who waits, for which
/// children, with which options. Built once per syscall by
/// [`crate::ProcTree::wait4_request`] or [`crate::ProcTree::waitid_request`]
/// and reused across a blocking wait's retries (a `0` meaning the caller's
/// group is resolved once, at the call, as Linux does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitRequest {
    pub(crate) caller: u32,
    pub(crate) target: WaitTarget,
    pub(crate) options: WaitOptions,
}

impl WaitRequest {
    /// The waiting process.
    pub const fn caller(&self) -> u32 {
        self.caller
    }

    /// Which children it waits for.
    pub const fn target(&self) -> WaitTarget {
        self.target
    }

    /// Its options.
    pub const fn options(&self) -> WaitOptions {
        self.options
    }
}

/// What a wait that found children matching found.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum WaitReady {
    /// A child had something to report.
    Reported(Report),
    /// A live child matches but has nothing to report: block, or return 0
    /// with [`WNOHANG`].
    NoneReady,
}

/// What a wait reports about one child.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    /// A zombie child, now removed from the tree: its pid is free again
    /// unless a group or session still uses the number.
    Reaped {
        /// The child's pid.
        pid: u32,
        /// How it ended.
        status: ExitStatus,
        /// The token the kernel gave the child when it was created, handed
        /// back for the last time: the kernel adds the child's usage to the
        /// parent's totals and frees it.
        rusage_slot: u64,
    },
    /// A zombie child reported under [`WNOWAIT`] and left in place.
    Zombie {
        /// The child's pid.
        pid: u32,
        /// How it ended.
        status: ExitStatus,
        /// The child's token, only lent: the kernel may read it to report
        /// `rusage`, but must neither free it nor add it to the parent's
        /// totals (Linux's `WNOWAIT` path skips that accumulation); a later
        /// reap or release hands it back.
        rusage_slot: u64,
    },
    /// A stopped child not reported before (now reported, unless
    /// [`WNOWAIT`]).
    Stopped {
        /// The child's pid.
        pid: u32,
        /// The signal that stopped it.
        signal: Signal,
    },
    /// A continued child not reported before (now reported, unless
    /// [`WNOWAIT`]).
    Continued {
        /// The child's pid.
        pid: u32,
    },
}

impl Report {
    /// The child reported.
    pub const fn pid(&self) -> u32 {
        match *self {
            Self::Reaped { pid, .. }
            | Self::Zombie { pid, .. }
            | Self::Stopped { pid, .. }
            | Self::Continued { pid } => pid,
        }
    }

    /// What is reported about the child: its status word and `siginfo`
    /// come from it.
    pub fn status(&self) -> WaitStatus {
        match *self {
            Self::Reaped { status, .. } | Self::Zombie { status, .. } => status.into(),
            Self::Stopped { signal, .. } => WaitStatus::Stopped(signal),
            Self::Continued { .. } => WaitStatus::Continued,
        }
    }
}
