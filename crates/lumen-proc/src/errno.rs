//! Errors: the Linux x86-64 `errno` values a syscall returns ([`Errno`]),
//! and the kernel bugs that are no user's error ([`Bug`]), together as
//! [`Error`].

use core::fmt;

/// An error a syscall returns to the user, as its `errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    /// Operation not permitted (`EPERM` = 1).
    Perm,
    /// No such process (`ESRCH` = 3).
    Srch,
    /// No child processes (`ECHILD` = 10).
    Child,
    /// Try again (`EAGAIN` = 11): no pid is free, or (from
    /// [`crate::ProcTree::spawn_init`]) pid 1 is still some process's
    /// group or session id.
    Again,
    /// Out of memory (`ENOMEM` = 12): a fallible reservation failed and the
    /// tree is unchanged.
    NoMem,
    /// Permission denied (`EACCES` = 13).
    Acces,
    /// Invalid argument (`EINVAL` = 22).
    Inval,
    /// Inappropriate ioctl for device (`ENOTTY` = 25).
    NoTty,
}

impl Errno {
    /// The positive Linux x86-64 `errno` code (a syscall returns its negation).
    pub const fn to_errno(self) -> u32 {
        match self {
            Self::Perm => 1,
            Self::Srch => 3,
            Self::Child => 10,
            Self::Again => 11,
            Self::NoMem => 12,
            Self::Acces => 13,
            Self::Inval => 22,
            Self::NoTty => 25,
        }
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Perm => "EPERM",
            Self::Srch => "ESRCH",
            Self::Child => "ECHILD",
            Self::Again => "EAGAIN",
            Self::NoMem => "ENOMEM",
            Self::Acces => "EACCES",
            Self::Inval => "EINVAL",
            Self::NoTty => "ENOTTY",
        })
    }
}

/// A call the kernel should never make. The tree is unchanged, and no
/// `errno` stands for it: the kernel must handle it itself (log it, panic)
/// rather than hand it to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// [`crate::ProcTree::spawn_init`] while a process 1 (live or zombie)
    /// is in the tree.
    InitExists,
    /// A pid the operation needs live (a caller, a parent to fork, a
    /// process to exit or exec) is a zombie or not in the tree.
    NotLive {
        /// The pid.
        pid: u32,
    },
    /// [`crate::ProcTree::exit`]'s reaper isn't live, or is the exiting
    /// process or one of its descendants.
    BadReaper {
        /// The exiting process.
        pid: u32,
        /// The reaper the kernel named.
        reaper: u32,
    },
    /// [`crate::ProcTree::exit`] without a reaper of a process that has
    /// children (live or zombie).
    NoReaper {
        /// The exiting process.
        pid: u32,
    },
    /// [`crate::ProcTree::release`] of a pid that isn't a zombie (a live
    /// process, or none).
    NotAZombie {
        /// The pid.
        pid: u32,
    },
    /// [`crate::ProcTree::release`] of a zombie whose `rusage_slot` isn't
    /// the one the kernel passed: the pid was reused since.
    WrongSlot {
        /// The pid.
        pid: u32,
        /// The slot the kernel passed.
        expected: u64,
        /// The slot the zombie holds.
        found: u64,
    },
}

/// What a fallible process-tree operation returns: an error for the user,
/// or a kernel bug. There is deliberately no mapping from it to an `errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The syscall fails with this `errno`.
    User(Errno),
    /// The kernel called wrongly: no `errno` stands for it.
    Bug(Bug),
}

impl From<Errno> for Error {
    fn from(e: Errno) -> Self {
        Self::User(e)
    }
}

impl From<Bug> for Error {
    fn from(b: Bug) -> Self {
        Self::Bug(b)
    }
}
