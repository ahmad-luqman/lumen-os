//! Errors: the Linux x86-64 `errno` values a syscall returns ([`Errno`]),
//! the kernel bugs that are no user's error ([`Bug`]), together as
//! [`Error`], and the errors of the calls no user makes.

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
    /// Try again (`EAGAIN` = 11): no pid is free.
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

/// A call the kernel should never make (a stale or wrong pid): the tree is
/// unchanged, and no `errno` stands for it, so the kernel handles it itself
/// (log it, panic) rather than hand it to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// A pid the operation needs live is a zombie or not in the tree.
    NotLive {
        /// The pid.
        pid: u32,
        /// What the operation needed it for.
        role: Role,
    },
    /// [`crate::ProcTree::exit`]'s reaper isn't live, or is the exiting
    /// process or one of its descendants.
    BadReaper {
        /// The exiting process.
        pid: u32,
        /// The reaper the kernel named.
        reaper: u32,
    },
}

/// What a pid named in a [`Bug::NotLive`] was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The process making the call (`wait*`, `setsid`, `setpgid`,
    /// `getpgid`, `getsid`, `tcsetpgrp`, `exec`).
    Caller,
    /// The parent [`crate::ProcTree::fork`] forks from.
    Parent,
    /// The process [`crate::ProcTree::exit`] ends.
    Exiting,
}

impl fmt::Display for Bug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NotLive { pid, role } => write!(f, "{role:?} {pid} isn't live"),
            Self::BadReaper { pid, reaper } => write!(f, "{reaper} can't reap the children of {pid}"),
        }
    }
}

/// What a fallible call made on behalf of a user returns: an error for the
/// user, or a kernel bug. There is deliberately no mapping from it to an
/// `errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The syscall fails with this `errno`.
    User(Errno),
    /// The kernel called wrongly: no `errno` stands for it.
    Bug(Bug),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(e) => write!(f, "{e}"),
            Self::Bug(b) => write!(f, "kernel bug: {b}"),
        }
    }
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

/// Why [`crate::ProcTree::exit`] failed. The tree is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitError {
    /// A fallible reservation failed. The process is past the point of no
    /// return: the kernel must not return to user space, but retry the
    /// exit after reclaiming memory.
    NoMem,
    /// The kernel called wrongly.
    Bug(Bug),
}

impl From<Bug> for ExitError {
    fn from(b: Bug) -> Self {
        Self::Bug(b)
    }
}

/// Why [`crate::ProcTree::spawn_init`] failed. The tree is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnInitError {
    /// A process 1 (live or zombie) is in the tree.
    Exists,
    /// Number 1 is still some process's pgid or sid; it succeeds once 1 is
    /// free.
    NumberBusy,
}

/// [`crate::ProcTree::spawn_root`] found no free pid. The tree is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoFreePid;
