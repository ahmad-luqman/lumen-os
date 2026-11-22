//! POSIX VFS error codes mapped to Linux x86-64 `errno` values.

use core::fmt;

/// Error returned by VFS operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsError {
    /// Operation not permitted (`EPERM` = 1).
    Perm,
    /// No such file or directory (`ENOENT` = 2).
    NoEnt,
    /// I/O error (`EIO` = 5).
    Io,
    /// Bad file descriptor (`EBADF` = 9).
    BadF,
    /// Try again / operation would block (`EAGAIN` / `EWOULDBLOCK` = 11).
    Again,
    /// Out of memory (`ENOMEM` = 12).
    NoMem,
    /// Permission denied (`EACCES` = 13).
    Acces,
    /// Bad address (`EFAULT` = 14).
    Fault,
    /// Device or resource busy (`EBUSY` = 16).
    Busy,
    /// File exists (`EEXIST` = 17).
    Exist,
    /// Invalid cross-device link (`EXDEV` = 18).
    XDev,
    /// No such device (`ENODEV` = 19).
    NoDev,
    /// Not a directory (`ENOTDIR` = 20).
    NotDir,
    /// Is a directory (`EISDIR` = 21).
    IsDir,
    /// Invalid argument (`EINVAL` = 22).
    Inval,
    /// Too many open files (`EMFILE` = 24).
    MFile,
    /// Inappropriate ioctl for device (`ENOTTY` = 25).
    NoTty,
    /// File too large (`EFBIG` = 27).
    FBig,
    /// No space left on device (`ENOSPC` = 28).
    NoSpc,
    /// Illegal seek (`ESPIPE` = 29).
    SPipe,
    /// Read-only file system (`EROFS` = 30).
    RoFs,
    /// Result too large (`ERANGE` = 34).
    Range,
    /// Resource deadlock avoided (`EDEADLK` = 35).
    DeadLk,
    /// File name too long (`ENAMETOOLONG` = 36).
    NameTooLong,
    /// No locks available (`ENOLCK` = 37).
    NoLck,
    /// Function not implemented (`ENOSYS` = 38).
    NoSys,
    /// Directory not empty (`ENOTEMPTY` = 39).
    NotEmpty,
    /// Too many levels of symbolic links (`ELOOP` = 40).
    Loop,
    /// Value too large for defined data type (`EOVERFLOW` = 75): a lock
    /// range past the largest offset.
    Overflow,
    /// Operation not supported (`EOPNOTSUPP` = 95).
    OpNotSupp,
}

impl VfsError {
    /// Convert to the positive Linux x86-64 `errno` code.
    pub const fn to_errno(self) -> u32 {
        match self {
            Self::Perm => 1,
            Self::NoEnt => 2,
            Self::Io => 5,
            Self::BadF => 9,
            Self::Again => 11,
            Self::NoMem => 12,
            Self::Acces => 13,
            Self::Fault => 14,
            Self::Busy => 16,
            Self::Exist => 17,
            Self::XDev => 18,
            Self::NoDev => 19,
            Self::NotDir => 20,
            Self::IsDir => 21,
            Self::Inval => 22,
            Self::MFile => 24,
            Self::NoTty => 25,
            Self::FBig => 27,
            Self::NoSpc => 28,
            Self::SPipe => 29,
            Self::RoFs => 30,
            Self::Range => 34,
            Self::DeadLk => 35,
            Self::NameTooLong => 36,
            Self::NoLck => 37,
            Self::NoSys => 38,
            Self::NotEmpty => 39,
            Self::Loop => 40,
            Self::Overflow => 75,
            Self::OpNotSupp => 95,
        }
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Perm => "EPERM",
            Self::NoEnt => "ENOENT",
            Self::Io => "EIO",
            Self::BadF => "EBADF",
            Self::Again => "EAGAIN",
            Self::NoMem => "ENOMEM",
            Self::Acces => "EACCES",
            Self::Fault => "EFAULT",
            Self::Busy => "EBUSY",
            Self::Exist => "EEXIST",
            Self::XDev => "EXDEV",
            Self::NoDev => "ENODEV",
            Self::NotDir => "ENOTDIR",
            Self::IsDir => "EISDIR",
            Self::Inval => "EINVAL",
            Self::MFile => "EMFILE",
            Self::NoTty => "ENOTTY",
            Self::FBig => "EFBIG",
            Self::NoSpc => "ENOSPC",
            Self::SPipe => "ESPIPE",
            Self::RoFs => "EROFS",
            Self::Range => "ERANGE",
            Self::DeadLk => "EDEADLK",
            Self::NameTooLong => "ENAMETOOLONG",
            Self::NoLck => "ENOLCK",
            Self::NoSys => "ENOSYS",
            Self::NotEmpty => "ENOTEMPTY",
            Self::Loop => "ELOOP",
            Self::Overflow => "EOVERFLOW",
            Self::OpNotSupp => "EOPNOTSUPP",
        };
        write!(f, "{name} ({})", self.to_errno())
    }
}
