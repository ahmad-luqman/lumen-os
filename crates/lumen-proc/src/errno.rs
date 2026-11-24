//! The Linux x86-64 `errno` values the process-tree rules return.

use core::fmt;

/// An error from a process-tree operation, as the syscall's `errno`.
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
            Self::Acces => "EACCES",
            Self::Inval => "EINVAL",
            Self::NoTty => "ENOTTY",
        })
    }
}
