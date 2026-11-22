//! Core POSIX VFS value types, bitflags, and device/inode identifiers.

use alloc::string::String;
use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

use crate::error::VfsError;

/// Inode number within a mounted filesystem.
pub type Ino = u64;

/// Canonical Linux device identifier (`makedev(major, minor)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DevId(pub u64);

impl DevId {
    /// Root filesystem (`0:1`, ext4 or fallback tmpfs).
    pub const ROOT: Self = Self::makedev(0, 1);
    /// `/tmp` (`0:2`, tmpfs).
    pub const TMPFS: Self = Self::makedev(0, 2);
    /// `/dev` (`0:3`, devfs).
    pub const DEVFS: Self = Self::makedev(0, 3);
    /// `/proc` (`0:4`, procfs).
    pub const PROCFS: Self = Self::makedev(0, 4);

    /// Encode `(major, minor)` using Linux's canonical `gnu_dev_makedev` layout.
    pub const fn makedev(major: u32, minor: u32) -> Self {
        let maj = major as u64;
        let min = minor as u64;
        Self(
            ((maj & 0xffff_f000) << 32)
                | ((maj & 0x0000_0fff) << 8)
                | ((min & 0xffff_ff00) << 12)
                | (min & 0x0000_00ff),
        )
    }

    /// Extract the 32-bit major device number.
    pub const fn major(self) -> u32 {
        (((self.0 >> 8) & 0x0000_0fff) | ((self.0 >> 32) & 0xffff_f000)) as u32
    }

    /// Extract the 32-bit minor device number.
    pub const fn minor(self) -> u32 {
        ((self.0 & 0x0000_00ff) | ((self.0 >> 12) & 0xffff_ff00)) as u32
    }

    /// Raw `dev_t` value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DevId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major(), self.minor())
    }
}

/// POSIX file type encoded in `st_mode` and `d_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InodeType {
    /// Regular file (`S_IFREG`).
    File,
    /// Directory (`S_IFDIR`).
    Dir,
    /// Symbolic link (`S_IFLNK`).
    Symlink,
    /// Character device (`S_IFCHR`).
    CharDevice,
    /// Block device (`S_IFBLK`).
    BlockDevice,
    /// Named pipe (`S_IFIFO`).
    Fifo,
    /// Unix domain socket (`S_IFSOCK`).
    Socket,
}

impl InodeType {
    /// `S_IFMT` mode bits matching Linux `st_mode`.
    pub const fn to_mode_bits(self) -> u32 {
        match self {
            Self::Fifo => 0o010000,
            Self::CharDevice => 0o020000,
            Self::Dir => 0o040000,
            Self::BlockDevice => 0o060000,
            Self::File => 0o100000,
            Self::Symlink => 0o120000,
            Self::Socket => 0o140000,
        }
    }

    /// A special file (character or block device, FIFO, socket), as
    /// Linux's `special_file()`: its data isn't stored on its filesystem,
    /// so a read-only mount doesn't refuse writing to it (`sb_permission`).
    pub const fn is_special(self) -> bool {
        matches!(self, Self::CharDevice | Self::BlockDevice | Self::Fifo | Self::Socket)
    }

    /// `d_type` byte for `linux_dirent64`.
    pub const fn to_dt_type(self) -> u8 {
        match self {
            Self::Fifo => 1,
            Self::CharDevice => 2,
            Self::Dir => 4,
            Self::BlockDevice => 6,
            Self::File => 8,
            Self::Symlink => 10,
            Self::Socket => 12,
        }
    }
}

/// Timestamp (`struct timespec`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    /// Validate `0 <= nsec < 1_000_000_000`.
    pub const fn is_valid(self) -> bool {
        self.nsec >= 0 && self.nsec < 1_000_000_000
    }
}

/// Timestamp update specification (`utimensat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetTime {
    /// `UTIME_OMIT`: leave timestamp unchanged.
    Omit,
    /// `UTIME_NOW`: set timestamp to current realtime clock.
    Now,
    /// Explicit timestamp.
    Set(Timespec),
}

impl SetTime {
    /// Special `tv_nsec` sentinel meaning "set to current time".
    pub const UTIME_NOW: i64 = 0x3fff_ffff;
    /// Special `tv_nsec` sentinel meaning "do not change".
    pub const UTIME_OMIT: i64 = 0x3fff_fffe;

    /// Decode a Linux `timespec` `(tv_sec, tv_nsec)` pair.
    pub const fn from_raw(sec: i64, nsec: i64) -> Result<Self, VfsError> {
        if nsec == Self::UTIME_NOW {
            Ok(Self::Now)
        } else if nsec == Self::UTIME_OMIT {
            Ok(Self::Omit)
        } else if nsec >= 0 && nsec < 1_000_000_000 {
            Ok(Self::Set(Timespec { sec, nsec }))
        } else {
            Err(VfsError::Inval)
        }
    }
}

/// Inode metadata returned by `VfsInode::stat()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub dev: DevId,
    pub ino: Ino,
    pub inode_type: InodeType,
    /// Permission and special mode bits (`0o7777`).
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: DevId,
    pub size: u64,
    pub blksize: u32,
    /// Allocated 512-byte blocks (`st_blocks`).
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
}

impl Metadata {
    /// Combined `S_IFMT | (mode & 0o7777)` value for `stat.st_mode`.
    pub const fn st_mode(&self) -> u32 {
        self.inode_type.to_mode_bits() | (self.mode as u32 & 0o7777)
    }
}

/// The mode a change of owner leaves on an inode of type `inode_type`
/// ([`VfsInode::set_owner`](crate::VfsInode::set_owner)'s rule): on anything
/// but a directory, setuid goes, and setgid too when group-execute is set
/// (a setgid file without it marks mandatory locking and keeps the bit); a
/// directory keeps both; a `-1, -1` chown too. This is Linux's
/// `chown_common` for a caller with `CAP_FSETID`. Every Lumen process is
/// root, so Lumen applies it to every chown; a non-root caller would also
/// need `setattr_should_drop_sgid`'s group check (dropping setgid without
/// group-execute when not in the file's group).
pub const fn mode_after_chown(inode_type: InodeType, mode: u16) -> u16 {
    const S_ISUID: u16 = 0o4000;
    const S_ISGID: u16 = 0o2000;
    const S_IXGRP: u16 = 0o010;
    if matches!(inode_type, InodeType::Dir) {
        return mode;
    }
    let mut kept = mode & !S_ISUID;
    if mode & (S_ISGID | S_IXGRP) == S_ISGID | S_IXGRP {
        kept &= !S_ISGID;
    }
    kept
}

/// Directory entry yielded by `VfsInode::readdir()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: Ino,
    pub inode_type: InodeType,
    pub name: String,
    /// The cursor just past this entry: `readdir` from it resumes with the
    /// next entry. Opaque, greater than the cursor the listing started at,
    /// and stable as other entries come and go (the contract on
    /// [`VfsInode::readdir`](crate::VfsInode::readdir)). `getdents64` hands
    /// it to user space as `d_off`.
    pub next_cursor: u64,
}

/// File access mode (`O_RDONLY`, `O_WRONLY`, `O_RDWR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl AccessMode {
    /// Convert back to Linux `O_ACCMODE` bits.
    pub const fn to_raw(self) -> u32 {
        match self {
            Self::ReadOnly => OpenFlags::O_RDONLY,
            Self::WriteOnly => OpenFlags::O_WRONLY,
            Self::ReadWrite => OpenFlags::O_RDWR,
        }
    }
}

/// Linux x86-64 `open` / `openat` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags(pub u32);

impl OpenFlags {
    pub const O_RDONLY: u32 = 0o0;
    pub const O_WRONLY: u32 = 0o1;
    pub const O_RDWR: u32 = 0o2;
    pub const O_ACCMODE: u32 = 0o3;
    pub const O_CREAT: u32 = 0o100;
    pub const O_EXCL: u32 = 0o200;
    pub const O_NOCTTY: u32 = 0o400;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_NONBLOCK: u32 = 0o4000;
    pub const O_DIRECTORY: u32 = 0o200000;
    pub const O_NOFOLLOW: u32 = 0o400000;
    pub const O_CLOEXEC: u32 = 0o2000000;
    pub const O_PATH: u32 = 0o10000000;
    pub const O_TMPFILE: u32 = 0o20200000;

    /// Construct from raw bits, kept as given (unknown bits included).
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The same as [`Self::from_bits`]: it truncates nothing, despite its
    /// name. Kept until its callers in `kernel/src/vfs` move to `from_bits`.
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self::from_bits(bits)
    }

    /// Raw bitmask.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// True if `O_PATH` is set.
    pub const fn is_path(self) -> bool {
        (self.0 & Self::O_PATH) != 0
    }

    /// Decode the access mode, returning `Err(Inval)` for `O_ACCMODE == 3` when `!O_PATH`.
    pub const fn access_mode(self) -> Result<AccessMode, VfsError> {
        if self.is_path() {
            return Ok(AccessMode::ReadOnly);
        }
        match self.0 & Self::O_ACCMODE {
            Self::O_RDONLY => Ok(AccessMode::ReadOnly),
            Self::O_WRONLY => Ok(AccessMode::WriteOnly),
            Self::O_RDWR => Ok(AccessMode::ReadWrite),
            _ => Err(VfsError::Inval),
        }
    }

    /// True if readable (`O_RDONLY` or `O_RDWR`, and not `O_PATH`).
    pub const fn is_readable(self) -> bool {
        !self.is_path() && matches!(self.0 & Self::O_ACCMODE, Self::O_RDONLY | Self::O_RDWR)
    }

    /// True if writable (`O_WRONLY` or `O_RDWR`, and not `O_PATH`).
    pub const fn is_writable(self) -> bool {
        !self.is_path() && matches!(self.0 & Self::O_ACCMODE, Self::O_WRONLY | Self::O_RDWR)
    }

    pub const fn has_creat(self) -> bool {
        (self.0 & Self::O_CREAT) != 0
    }

    pub const fn has_excl(self) -> bool {
        (self.0 & Self::O_EXCL) != 0
    }

    pub const fn has_trunc(self) -> bool {
        (self.0 & Self::O_TRUNC) != 0
    }

    pub const fn has_append(self) -> bool {
        (self.0 & Self::O_APPEND) != 0
    }

    pub const fn has_nonblock(self) -> bool {
        (self.0 & Self::O_NONBLOCK) != 0
    }

    pub const fn has_directory(self) -> bool {
        (self.0 & Self::O_DIRECTORY) != 0
    }

    pub const fn has_nofollow(self) -> bool {
        (self.0 & Self::O_NOFOLLOW) != 0
    }

    pub const fn has_cloexec(self) -> bool {
        (self.0 & Self::O_CLOEXEC) != 0
    }

    pub const fn has_tmpfile(self) -> bool {
        (self.0 & Self::O_TMPFILE) == Self::O_TMPFILE
    }

    /// Mutable status flags (`O_APPEND | O_NONBLOCK`).
    pub const fn status_bits(self) -> u32 {
        self.0 & (Self::O_APPEND | Self::O_NONBLOCK)
    }
}

impl BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for OpenFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for OpenFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for OpenFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

/// `*at` syscall constants and flag helpers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtFlags(pub u32);

impl AtFlags {
    pub const AT_FDCWD: i32 = -100;
    pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    pub const AT_REMOVEDIR: u32 = 0x200;
    pub const AT_EACCESS: u32 = 0x200;
    pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
    pub const AT_NO_AUTOMOUNT: u32 = 0x800;
    pub const AT_EMPTY_PATH: u32 = 0x1000;

    /// Validate that only bits in `allowed` are set.
    pub const fn validate(self, allowed: u32) -> Result<Self, VfsError> {
        if self.0 & !allowed == 0 { Ok(self) } else { Err(VfsError::Inval) }
    }

    pub const fn has_symlink_nofollow(self) -> bool {
        (self.0 & Self::AT_SYMLINK_NOFOLLOW) != 0
    }

    pub const fn has_removedir(self) -> bool {
        (self.0 & Self::AT_REMOVEDIR) != 0
    }

    pub const fn has_symlink_follow(self) -> bool {
        (self.0 & Self::AT_SYMLINK_FOLLOW) != 0
    }

    pub const fn has_empty_path(self) -> bool {
        (self.0 & Self::AT_EMPTY_PATH) != 0
    }
}

/// `renameat2(2)` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameFlags(pub u32);

impl RenameFlags {
    pub const NOREPLACE: u32 = 1 << 0;
    pub const EXCHANGE: u32 = 1 << 1;

    /// Validate flag combinations: only `0`, `NOREPLACE`, or `EXCHANGE` are legal.
    pub const fn validate(self) -> Result<Self, VfsError> {
        let allowed = Self::NOREPLACE | Self::EXCHANGE;
        if (self.0 & !allowed) != 0 {
            return Err(VfsError::Inval);
        }
        if self.has_noreplace() && self.has_exchange() {
            return Err(VfsError::Inval);
        }
        Ok(self)
    }

    pub const fn has_noreplace(self) -> bool {
        (self.0 & Self::NOREPLACE) != 0
    }

    pub const fn has_exchange(self) -> bool {
        (self.0 & Self::EXCHANGE) != 0
    }
}

/// `lseek(2)` whence argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekFrom {
    Start(i64),
    Current(i64),
    End(i64),
}

impl SeekFrom {
    pub const SEEK_SET: i32 = 0;
    pub const SEEK_CUR: i32 = 1;
    pub const SEEK_END: i32 = 2;

    /// Decode `(whence, offset)` from syscall arguments.
    pub const fn from_raw(whence: i32, offset: i64) -> Result<Self, VfsError> {
        match whence {
            Self::SEEK_SET => Ok(Self::Start(offset)),
            Self::SEEK_CUR => Ok(Self::Current(offset)),
            Self::SEEK_END => Ok(Self::End(offset)),
            _ => Err(VfsError::Inval),
        }
    }
}

/// `access(2)` / `faccessat2(2)` mode bits (`F_OK`, `R_OK`, `W_OK`, `X_OK`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessMask(pub u32);

impl AccessMask {
    pub const F_OK: u32 = 0;
    pub const X_OK: u32 = 1;
    pub const W_OK: u32 = 2;
    pub const R_OK: u32 = 4;

    pub const fn validate(self) -> Result<Self, VfsError> {
        let allowed = Self::X_OK | Self::W_OK | Self::R_OK;
        if self.0 & !allowed == 0 { Ok(self) } else { Err(VfsError::Inval) }
    }

    pub const fn wants_read(self) -> bool {
        (self.0 & Self::R_OK) != 0
    }

    pub const fn wants_write(self) -> bool {
        (self.0 & Self::W_OK) != 0
    }

    pub const fn wants_exec(self) -> bool {
        (self.0 & Self::X_OK) != 0
    }
}
