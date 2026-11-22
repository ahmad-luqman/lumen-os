//! What the M7 file syscalls share (#254's clusters): decoding their
//! register arguments, copying a path argument in, resolving a `dirfd` into
//! the directory a path starts from, and filling Linux's `struct stat` and
//! `struct statx` from VFS metadata.
//!
//! `dispatch` decodes each register into the handler's parameter type with
//! [`reg`], as Linux reads it: an `int`, `mode_t` or `unsigned int` is the
//! register's low 32 bits, since callers may leave anything in the upper
//! half (`musl` sign-extends an `int`; a raw `syscall` may not). A sizes or
//! offset argument stays a whole `u64`.
//!
//! **Turning a [`Dirfd`] into the directory a path starts from** is
//! [`start_dir`], the one place it happens, and its [`Start`] is the only
//! way a handler reaches lumen-vfs's path operations (`Start::open`,
//! `resolve`, `mkdir`, `unlink`, `readlink`, `symlink`, and `Start::link`
//! and `Start::rename` across two starts): a handler never hands lumen-vfs
//! a raw `start_dir`. Its traps:
//! - lumen-vfs's `resolve_path(start_dir: None, ..)` starts at the global
//!   root, not the working directory: `Dirfd::Cwd` resolves to the
//!   process's cwd inode, a concrete `Arc`, never `None`;
//! - Linux ignores the dirfd when the path is absolute, even a bad one:
//!   `start_dir` takes the `&UserPath` and looks at the dirfd only for a
//!   relative or empty path (`-EBADF` then for a bad one);
//! - with `AT_EMPTY_PATH`, `Dirfd::Fd` names the file itself, which need
//!   not be a directory (`-ENOTDIR` only for a relative path through it).

use alloc::sync::Arc;
use alloc::vec::Vec;

use lumen_vfs::{
    AccessMask, AtFlags, InodeType, Metadata, OpenFlags, Opened, RenameFlags, ResolveFlags, ResolvedPath, Timespec,
    VfsError, VfsInode, link_at, mkdir_at, open_inode_guarded, readlink_at, rename_at2, resolve_path, symlink_at,
    unlink_at,
};

use crate::vfs::tables;

use crate::linux::process::{self, FileRef};
use crate::linux::{errno, general};
use crate::user::{CopyError, copy_cstr_from_user};

/// A syscall argument decoded from its register as Linux reads it.
pub(super) trait FromReg {
    fn from_reg(reg: u64) -> Self;
}

/// Decode register `r` into the parameter type `T` (`dispatch`'s arms).
#[inline]
pub(super) fn reg<T: FromReg>(r: u64) -> T {
    T::from_reg(r)
}

/// A whole register: a size, an offset, an address or an `arg` whose
/// meaning depends on another argument.
impl FromReg for u64 {
    fn from_reg(reg: u64) -> Self {
        reg
    }
}

/// An `unsigned int` (`uid_t`, `gid_t`, `lseek`'s `whence`, `fcntl`'s and
/// `flock`'s `cmd`, `getdents64`'s `count`, `statx`'s `mask`).
impl FromReg for u32 {
    fn from_reg(reg: u64) -> Self {
        reg as u32
    }
}

/// An `int` (`readlink`'s `bufsiz`, `msync`'s `flags`).
impl FromReg for i32 {
    fn from_reg(reg: u64) -> Self {
        reg as u32 as i32
    }
}

/// A file descriptor argument (`int fd`; Linux's `unsigned int fd` ones
/// decode alike: a huge one is negative here and `-EBADF` either way).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fd(i32);

impl Fd {
    /// The descriptor, as lumen-vfs's `FdTable` takes it (negative: `-EBADF`).
    pub(super) fn raw(self) -> i32 {
        self.0
    }

    /// The current process's open file on this descriptor (a clone), or
    /// `EBADF`: every handler's descriptor lookup.
    pub(super) fn file(self) -> Result<FileRef, u32> {
        process::current_file(self.0).ok_or(errno::EBADF)
    }
}

impl FromReg for Fd {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32 as i32)
    }
}

/// A `*at` syscall's directory argument (`int dirfd`): `AT_FDCWD` for the
/// working directory, else a descriptor (maybe a bad one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dirfd {
    Cwd,
    Fd(Fd),
}

impl FromReg for Dirfd {
    fn from_reg(reg: u64) -> Self {
        let fd = Fd::from_reg(reg);
        if fd.0 == general::AT_FDCWD { Self::Cwd } else { Self::Fd(fd) }
    }
}

/// A mode argument (`umode_t mode`; `umask`'s `int mask`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mode(u32);

impl Mode {
    /// What `open(O_CREAT)` and `chmod` keep: the permission and special
    /// bits (`mode & 0o7777`), as lumen-vfs takes them. Linux reads a 16-bit
    /// `umode_t`, which this mask makes no different from the low 32 bits.
    pub(super) fn perm(self) -> u16 {
        (self.0 & 0o7777) as u16
    }

    /// What `mkdir` keeps (`mode & 0o1777`, Linux's `vfs_mkdir`): the
    /// permission bits and sticky, never setuid or setgid.
    pub(super) fn dir_perm(self) -> u16 {
        (self.0 & 0o1777) as u16
    }

    /// The low 32 bits as given (`umask` keeps `& 0o777` of them).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `umask` (wave 3) calls it; the ktests until then
    pub(super) fn bits(self) -> u32 {
        self.0
    }
}

impl FromReg for Mode {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32)
    }
}

/// A `*at` flags argument (`int flags`); `AtFlags::validate` rejects the
/// bits a syscall doesn't take.
impl FromReg for AtFlags {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32)
    }
}

/// `open`/`openat`'s flags (`int flags`).
impl FromReg for OpenFlags {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32)
    }
}

/// `renameat2`'s flags (`unsigned int flags`).
impl FromReg for RenameFlags {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32)
    }
}

/// `access`/`faccessat*`'s mode (`int mode`); `AccessMask::validate`
/// rejects unknown bits.
impl FromReg for AccessMask {
    fn from_reg(reg: u64) -> Self {
        Self(reg as u32)
    }
}

/// A user pointer argument: the whole register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UserPtr(u64);

impl UserPtr {
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `utimensat` (wave 3) calls it; the ktests until then
    pub(super) fn is_null(self) -> bool {
        self.0 == 0
    }

    pub(super) fn addr(self) -> u64 {
        self.0
    }
}

impl FromReg for UserPtr {
    fn from_reg(reg: u64) -> Self {
        Self(reg)
    }
}

/// A path argument copied in from user space: at most `PATH_MAX - 1` bytes,
/// no NUL, empty only where the caller allowed it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct UserPath(Vec<u8>);

impl UserPath {
    /// Copy the NUL-terminated path at `ptr`: `-EFAULT` for a bad pointer,
    /// `-ENAMETOOLONG` past `PATH_MAX` bytes with its NUL, `-ENOENT` if empty
    /// unless `allow_empty` (`AT_EMPTY_PATH`, or `readlinkat`, which takes an
    /// empty path without it).
    ///
    /// `NULL` is `-EFAULT` too. A caller for which `NULL` means something
    /// checks first: `utimensat`'s path, and, from Linux 6.11, `fstatat`'s and
    /// `statx`'s with `AT_EMPTY_PATH` (the same as `""`).
    ///
    /// Must be called with interrupts enabled and no lock held.
    pub(super) fn copy_in(ptr: UserPtr, allow_empty: bool) -> Result<Self, u32> {
        let max_len = general::PATH_MAX as usize - 1;
        let bytes = copy_cstr_from_user(ptr.addr(), max_len).map_err(CopyError::to_errno)?;
        Self::from_bytes(bytes, allow_empty)
    }

    fn from_bytes(bytes: Vec<u8>, allow_empty: bool) -> Result<Self, u32> {
        if bytes.is_empty() && !allow_empty {
            return Err(errno::ENOENT);
        }
        Ok(Self(bytes))
    }

    /// The path's bytes, as the VFS's resolution takes them.
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The path as text, for the calls that take a `&str`. The one place the
    /// rule lives: a path that isn't UTF-8 names nothing the VFS can hold
    /// yet, so `-ENOENT` (#268 makes VFS names byte strings).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // for wave 3's `&str` VFS calls; the ktests until then
    pub(super) fn as_str(&self) -> Result<&str, u32> {
        core::str::from_utf8(&self.0).map_err(|_| errno::ENOENT)
    }
}

/// Where a path argument starts, as [`start_dir`] resolved its `dirfd`.
pub(super) struct Start {
    /// The directory a relative path starts from (with an empty path, which
    /// `AT_EMPTY_PATH` or `readlinkat` allows, the file itself); `None` only
    /// for an absolute path, which lumen-vfs starts at the global root
    /// whatever it is handed.
    dir: Option<Arc<dyn VfsInode>>,
    /// The path `dir` was opened or set by (the cwd's, or the dirfd's open
    /// file's), for the path a relative open shows in `/proc/<pid>/fd`.
    base: Option<Arc<str>>,
}

impl Start {
    /// lumen-vfs's `start_dir` for this start: private, so it reaches
    /// lumen-vfs only through the operations below.
    fn dir(&self) -> Option<&Arc<dyn VfsInode>> {
        self.dir.as_ref()
    }

    /// `open_inode_guarded` from this start (`openat`): `mode`'s permission
    /// bits, less the process's umask, for a file it creates.
    pub(super) fn open(&self, path: &UserPath, flags: OpenFlags, mode: Mode) -> Result<Opened, u32> {
        let umask = process::current_umask();
        open_inode_guarded(tables(), self.dir(), path.as_bytes(), flags, mode.perm(), umask).map_err(VfsError::to_errno)
    }

    /// `resolve_path` from this start (the `stat` family, `access`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // wave 3's `newfstatat`/`statx`/`faccessat2` call it
    pub(super) fn resolve(&self, path: &UserPath, flags: ResolveFlags) -> Result<ResolvedPath, u32> {
        resolve_path(tables(), self.dir(), path.as_bytes(), flags).map_err(VfsError::to_errno)
    }

    /// `mkdir_at` from this start (`mkdirat`): `mode`'s directory bits, less
    /// the process's umask.
    pub(super) fn mkdir(&self, path: &UserPath, mode: Mode) -> Result<Arc<dyn VfsInode>, u32> {
        let umask = process::current_umask();
        mkdir_at(tables(), self.dir(), path.as_bytes(), mode.dir_perm(), umask).map_err(VfsError::to_errno)
    }

    /// `unlink_at` from this start (`unlinkat`; `remove_dir` for
    /// `AT_REMOVEDIR`).
    pub(super) fn unlink(&self, path: &UserPath, remove_dir: bool) -> Result<(), u32> {
        unlink_at(tables(), self.dir(), path.as_bytes(), remove_dir).map_err(VfsError::to_errno)
    }

    /// `readlink_at` from this start (`readlinkat`).
    pub(super) fn readlink(&self, path: &UserPath) -> Result<Vec<u8>, u32> {
        readlink_at(tables(), self.dir(), path.as_bytes()).map_err(VfsError::to_errno)
    }

    /// `symlink_at` at `linkpath` from this start (`symlinkat`): `target` is
    /// stored as given, never resolved, so it takes no start.
    pub(super) fn symlink(&self, target: &[u8], linkpath: &UserPath) -> Result<Arc<dyn VfsInode>, u32> {
        symlink_at(tables(), target, self.dir(), linkpath.as_bytes()).map_err(VfsError::to_errno)
    }

    /// `link_at` from `old`'s start to `new`'s (`linkat`; `follow` for
    /// `AT_SYMLINK_FOLLOW`, `empty` for `AT_EMPTY_PATH`).
    pub(super) fn link(
        old: &Start,
        old_path: &UserPath,
        new: &Start,
        new_path: &UserPath,
        follow: bool,
        empty: bool,
    ) -> Result<(), u32> {
        link_at(tables(), old.dir(), old_path.as_bytes(), new.dir(), new_path.as_bytes(), follow, empty)
            .map_err(VfsError::to_errno)
    }

    /// `rename_at2` from `old`'s start to `new`'s (`renameat2`).
    pub(super) fn rename(
        old: &Start,
        old_path: &UserPath,
        new: &Start,
        new_path: &UserPath,
        flags: RenameFlags,
    ) -> Result<(), u32> {
        rename_at2(tables(), old.dir(), old_path.as_bytes(), new.dir(), new_path.as_bytes(), flags)
            .map_err(VfsError::to_errno)
    }

    /// `path` (as given, relative) joined to the path the start was opened
    /// by, for `/proc/<pid>/fd`; `None` for an absolute or empty one, which
    /// stands as it is. Lexical: a `..` or a symlink in it stays, and the
    /// base is the path as it was when the cwd was set or the dirfd opened,
    /// so a rename since then isn't followed (Linux's `d_path` would be;
    /// `getcwd` is, computing the cwd's path from its inode).
    pub(super) fn display_path(&self, path: &str) -> Option<Arc<str>> {
        let base = self.base.as_deref()?;
        if path.is_empty() || path.starts_with('/') {
            return None;
        }
        let sep = if base.ends_with('/') { "" } else { "/" };
        Some(Arc::from(alloc::format!("{base}{sep}{path}").as_str()))
    }
}

/// The directory `path` starts from for `dirfd`: the one rule every `*at`
/// syscall shares (and the legacy ones, as `Dirfd::Cwd`).
///
/// - An absolute path starts at the root whatever `dirfd` is: the dirfd is
///   never looked at (a bad one is no error).
/// - An empty path (`UserPath::copy_in` let one through only with
///   `AT_EMPTY_PATH`, or for `readlinkat`) names the dirfd's file itself, a
///   directory or not, or the cwd.
/// - A relative path starts at the cwd's inode for `Dirfd::Cwd`, or at the
///   dirfd's file: `-EBADF` if it isn't open, `-ENOTDIR` if it isn't a
///   directory (an `O_PATH` one is fine).
///
/// Takes `ACTIVE_PROCS` briefly; the clones it takes drop after it.
pub(super) fn start_dir(dirfd: Dirfd, path: &UserPath) -> Result<Start, u32> {
    if path.as_bytes().starts_with(b"/") {
        return Ok(Start { dir: None, base: None });
    }
    let (inode, base) = match dirfd {
        Dirfd::Cwd => process::current_cwd(),
        Dirfd::Fd(fd) => {
            let file = fd.file()?;
            (Arc::clone(&file.inode), Arc::clone(&file.path))
        }
    };
    if !path.is_empty() && inode.inode_type() != InodeType::Dir {
        return Err(errno::ENOTDIR);
    }
    Ok(Start { dir: Some(inode), base: Some(base) })
}

/// A timestamp's nanoseconds, within `0..1_000_000_000` whatever a
/// filesystem reported (a negative one would reach user space as a huge
/// unsigned number).
fn nsec(t: Timespec) -> u32 {
    debug_assert!((0..1_000_000_000).contains(&t.nsec), "timestamp nsec {} out of range", t.nsec);
    t.nsec.clamp(0, 999_999_999) as u32
}

/// A size or block count as Linux's signed field: these fit `loff_t`;
/// saturate rather than wrap should a filesystem ever report more.
fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The Linux x86-64 `struct stat` for `m` (`stat`, `lstat`, `newfstatat`,
/// and `fstat` once it serves VFS files). `st_dev` and `st_rdev` are glibc's
/// `makedev` encoding, which `DevId` holds and `major()`/`minor()` read back.
pub(super) fn stat_from_metadata(m: &Metadata) -> general::stat {
    general::stat {
        st_dev: m.dev.raw(),
        st_ino: m.ino,
        st_nlink: u64::from(m.nlink),
        st_mode: m.st_mode(),
        st_uid: m.uid,
        st_gid: m.gid,
        __pad0: 0,
        st_rdev: m.rdev.raw(),
        st_size: signed(m.size),
        st_blksize: i64::from(m.blksize),
        st_blocks: signed(m.blocks),
        st_atime: m.atime.sec as u64,
        st_atime_nsec: u64::from(nsec(m.atime)),
        st_mtime: m.mtime.sec as u64,
        st_mtime_nsec: u64::from(nsec(m.mtime)),
        st_ctime: m.ctime.sec as u64,
        st_ctime_nsec: u64::from(nsec(m.ctime)),
        __unused: [0; 3],
    }
}

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `statx` (wave 3) calls it; the ktests until then
fn statx_time(t: Timespec) -> general::statx_timestamp {
    general::statx_timestamp { tv_sec: t.sec, tv_nsec: nsec(t), __reserved: 0 }
}

/// The Linux `struct statx` for `m` (`statx`): the basic fields
/// (`STATX_BASIC_STATS` in `stx_mask`, whatever was asked, as Linux may),
/// no birth time, no mount id, no attributes.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `statx` (wave 3) calls it; the ktests until then
pub(super) fn statx_from_metadata(m: &Metadata) -> general::statx {
    let zero_time = general::statx_timestamp { tv_sec: 0, tv_nsec: 0, __reserved: 0 };
    general::statx {
        stx_mask: general::STATX_BASIC_STATS,
        stx_blksize: m.blksize,
        stx_attributes: 0,
        stx_nlink: m.nlink,
        stx_uid: m.uid,
        stx_gid: m.gid,
        stx_mode: m.st_mode() as u16,
        __spare0: [0],
        stx_ino: m.ino,
        stx_size: m.size,
        stx_blocks: m.blocks,
        stx_attributes_mask: 0,
        stx_atime: statx_time(m.atime),
        stx_btime: zero_time,
        stx_ctime: statx_time(m.ctime),
        stx_mtime: statx_time(m.mtime),
        stx_rdev_major: m.rdev.major(),
        stx_rdev_minor: m.rdev.minor(),
        stx_dev_major: m.dev.major(),
        stx_dev_minor: m.dev.minor(),
        stx_mnt_id: 0,
        stx_dio_mem_align: 0,
        stx_dio_offset_align: 0,
        stx_subvol: 0,
        stx_atomic_write_unit_min: 0,
        stx_atomic_write_unit_max: 0,
        stx_atomic_write_segments_max: 0,
        stx_dio_read_offset_align: 0,
        stx_atomic_write_unit_max_opt: 0,
        __spare2: [0],
        __spare3: [0; 8],
    }
}

// lumen-vfs's copies of the Linux constants the decoded arguments are
// checked against agree with linux-raw-sys's (a drift fails the build).
const _: () = {
    assert!(AtFlags::AT_FDCWD == general::AT_FDCWD);
    assert!(AtFlags::AT_SYMLINK_NOFOLLOW == general::AT_SYMLINK_NOFOLLOW);
    assert!(AtFlags::AT_REMOVEDIR == general::AT_REMOVEDIR);
    assert!(AtFlags::AT_EACCESS == general::AT_EACCESS);
    assert!(AtFlags::AT_SYMLINK_FOLLOW == general::AT_SYMLINK_FOLLOW);
    assert!(AtFlags::AT_NO_AUTOMOUNT == general::AT_NO_AUTOMOUNT);
    assert!(AtFlags::AT_EMPTY_PATH == general::AT_EMPTY_PATH);
    assert!(OpenFlags::O_RDONLY == general::O_RDONLY);
    assert!(OpenFlags::O_WRONLY == general::O_WRONLY);
    assert!(OpenFlags::O_RDWR == general::O_RDWR);
    assert!(OpenFlags::O_ACCMODE == general::O_ACCMODE);
    assert!(OpenFlags::O_CREAT == general::O_CREAT);
    assert!(OpenFlags::O_EXCL == general::O_EXCL);
    assert!(OpenFlags::O_NOCTTY == general::O_NOCTTY);
    assert!(OpenFlags::O_TRUNC == general::O_TRUNC);
    assert!(OpenFlags::O_APPEND == general::O_APPEND);
    assert!(OpenFlags::O_NONBLOCK == general::O_NONBLOCK);
    assert!(OpenFlags::O_DIRECTORY == general::O_DIRECTORY);
    assert!(OpenFlags::O_NOFOLLOW == general::O_NOFOLLOW);
    assert!(OpenFlags::O_CLOEXEC == general::O_CLOEXEC);
    assert!(OpenFlags::O_PATH == general::O_PATH);
    assert!(OpenFlags::O_TMPFILE == general::O_TMPFILE);
    assert!(RenameFlags::NOREPLACE == general::RENAME_NOREPLACE);
    assert!(RenameFlags::EXCHANGE == general::RENAME_EXCHANGE);
    assert!(AccessMask::F_OK == general::F_OK);
    assert!(AccessMask::X_OK == general::X_OK);
    assert!(AccessMask::W_OK == general::W_OK);
    assert!(AccessMask::R_OK == general::R_OK);
    assert!(lumen_vfs::SetTime::UTIME_NOW == general::UTIME_NOW as i64);
    assert!(lumen_vfs::SetTime::UTIME_OMIT == general::UTIME_OMIT as i64);
};

const _: () = {
    use lumen_vfs::locks::{fcntl_const as f, flock_const as l};
    assert!(l::LOCK_SH == general::LOCK_SH as i32);
    assert!(l::LOCK_EX == general::LOCK_EX as i32);
    assert!(l::LOCK_NB == general::LOCK_NB as i32);
    assert!(l::LOCK_UN == general::LOCK_UN as i32);
    assert!(f::F_DUPFD == general::F_DUPFD as i32);
    assert!(f::F_GETFD == general::F_GETFD as i32);
    assert!(f::F_SETFD == general::F_SETFD as i32);
    assert!(f::F_GETFL == general::F_GETFL as i32);
    assert!(f::F_SETFL == general::F_SETFL as i32);
    assert!(f::F_GETLK == general::F_GETLK as i32);
    assert!(f::F_SETLK == general::F_SETLK as i32);
    assert!(f::F_SETLKW == general::F_SETLKW as i32);
    assert!(f::F_DUPFD_CLOEXEC == general::F_DUPFD_CLOEXEC as i32);
    assert!(f::FD_CLOEXEC == general::FD_CLOEXEC);
    assert!(f::F_RDLCK == general::F_RDLCK as i16);
    assert!(f::F_WRLCK == general::F_WRLCK as i16);
    assert!(f::F_UNLCK == general::F_UNLCK as i16);
    assert!(f::SEEK_SET == general::SEEK_SET as i16);
    assert!(f::SEEK_CUR == general::SEEK_CUR as i16);
    assert!(f::SEEK_END == general::SEEK_END as i16);
};

crate::ktest!(
    fn at_decoders_read_the_low_32_bits() {
        // `AT_FDCWD` as musl passes it (sign-extended), zero-extended, and
        // with anything above it: all the working directory.
        for r in [0xffff_ffff_ffff_ff9c, 0x0000_0000_ffff_ff9c, 0x1234_5678_ffff_ff9c] {
            crate::kassert_eq!(reg::<Dirfd>(r), Dirfd::Cwd);
        }
        // -100 is the only such number: -101 and 100 are descriptors.
        crate::kassert_eq!(reg::<Dirfd>(0xffff_ffff_ffff_ff9b), Dirfd::Fd(Fd(-101)));
        crate::kassert_eq!(reg::<Dirfd>(100), Dirfd::Fd(Fd(100)));
        // A descriptor is the low 32 bits: anything above 3 is fd 3; a
        // high-bit fd is negative.
        crate::kassert_eq!(reg::<Fd>(0xdead_beef_0000_0003).raw(), 3);
        crate::kassert_eq!(reg::<Fd>(0x8000_0000).raw(), i32::MIN);
        crate::kassert_eq!(reg::<Fd>(u64::MAX).raw(), -1);
        crate::kassert_eq!(reg::<Fd>(0x7fff_ffff).raw(), i32::MAX);
        // A mode keeps its permission and special bits; a directory's drops
        // setuid and setgid; the bits as given are the low 32.
        crate::kassert_eq!(reg::<Mode>(0xffff_ffff_0000_81ed).perm(), 0o755);
        crate::kassert_eq!(reg::<Mode>(0o107_777).perm(), 0o7777);
        crate::kassert_eq!(reg::<Mode>(0o170_000).perm(), 0);
        crate::kassert_eq!(reg::<Mode>(0o107_777).dir_perm(), 0o1777);
        crate::kassert_eq!(reg::<Mode>(0o6755).dir_perm(), 0o755);
        crate::kassert_eq!(reg::<Mode>(0x1_0000_0012).bits(), 0o22);
        // Flags and `int`/`unsigned int` arguments are the low 32 bits; a
        // size, offset or pointer is the whole register.
        crate::kassert_eq!(reg::<AtFlags>(0xffff_ffff_0000_0100), AtFlags(general::AT_SYMLINK_NOFOLLOW));
        crate::kassert_eq!(reg::<OpenFlags>(0x1_0001_0000), OpenFlags(general::O_DIRECTORY));
        crate::kassert_eq!(reg::<RenameFlags>(0x1_0000_0001), RenameFlags(general::RENAME_NOREPLACE));
        crate::kassert_eq!(reg::<AccessMask>(0x1_0000_0004), AccessMask(general::R_OK));
        crate::kassert_eq!(reg::<u32>(0xffff_ffff_0000_0007), 7);
        crate::kassert_eq!(reg::<i32>(0xffff_ffff_ffff_ffff), -1);
        crate::kassert_eq!(reg::<i32>(0x1_0000_0010), 16);
        crate::kassert_eq!(reg::<u64>(0xffff_ffff_0000_0007), 0xffff_ffff_0000_0007);
        crate::kassert!(reg::<UserPtr>(0).is_null());
        crate::kassert_eq!(reg::<UserPtr>(0xdead_beef_0000_1000).addr(), 0xdead_beef_0000_1000);
        Ok(())
    }
);

crate::ktest!(
    fn at_user_path_rules() {
        use alloc::vec;

        // Empty: `-ENOENT`, unless the caller allows it (`AT_EMPTY_PATH`).
        crate::kassert_eq!(UserPath::from_bytes(Vec::new(), false), Err(errno::ENOENT));
        crate::kassert!(UserPath::from_bytes(Vec::new(), true).is_ok_and(|p| p.is_empty()));
        // Text: UTF-8 reads as itself; anything else names nothing (#268).
        let p = UserPath::from_bytes(b"/tmp/a".to_vec(), false);
        crate::kassert_eq!(p.as_ref().map(|p| p.as_bytes()), Ok(&b"/tmp/a"[..]));
        crate::kassert_eq!(p.as_ref().map(|p| p.as_str()), Ok(Ok("/tmp/a")));
        let p = UserPath::from_bytes(vec![b'/', 0xff, b'x'], false);
        crate::kassert_eq!(p.as_ref().map(|p| p.as_str()), Ok(Err(errno::ENOENT)));
        // Pointers: `NULL` and a kernel address are `-EFAULT`, never read.
        crate::kassert_eq!(UserPath::copy_in(UserPtr(0), false), Err(errno::EFAULT));
        crate::kassert_eq!(UserPath::copy_in(UserPtr(0), true), Err(errno::EFAULT));
        crate::kassert_eq!(UserPath::copy_in(UserPtr(0xffff_8000_0000_0000), false), Err(errno::EFAULT));
        Ok(())
    }
);

crate::ktest!(
    fn at_user_path_copy_in_from_user_memory() {
        use alloc::sync::Arc;
        use alloc::vec;
        use lumen_mm::FRAME_SIZE;
        use lumen_vm::Prot;

        use crate::mm::user_vm::AddressSpace;
        use crate::user::copy_to_user;

        // Two mapped pages, then an unmapped one, in a user address space of
        // our own (no process needed), as `user.rs`'s copy ktest does.
        let base: u64 = 0x0000_0000_0060_0000;
        let space = Arc::new(AddressSpace::new().map_err(|_| "out of frames for AddressSpace")?);
        space.map_eager_with_bytes(base, 2 * FRAME_SIZE, Prot::READ | Prot::WRITE, &[]).map_err(|_| "map")?;
        let result = (|| {
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let max = general::PATH_MAX as usize - 1;
            // The longest path: 4095 bytes and its NUL.
            let mut long = vec![b'a'; max + 1];
            long[max] = 0;
            crate::kassert_eq!(copy_to_user(base, &long), Ok(()));
            let p = UserPath::copy_in(UserPtr(base), false);
            crate::kassert_eq!(p.map(|p| p.as_bytes().len()), Ok(max));
            // One byte more: `-ENAMETOOLONG`.
            crate::kassert_eq!(copy_to_user(base + max as u64, b"a\0"), Ok(()));
            crate::kassert_eq!(UserPath::copy_in(UserPtr(base), false), Err(errno::ENAMETOOLONG));
            crate::kassert_eq!(UserPath::copy_in(UserPtr(base), true), Err(errno::ENAMETOOLONG));
            // An empty string: `-ENOENT`, or an empty path with `AT_EMPTY_PATH`.
            let empty = base + FRAME_SIZE + 16;
            crate::kassert_eq!(copy_to_user(empty, b"\0"), Ok(()));
            crate::kassert_eq!(UserPath::copy_in(UserPtr(empty), false), Err(errno::ENOENT));
            crate::kassert!(UserPath::copy_in(UserPtr(empty), true).is_ok_and(|p| p.is_empty()));
            // Bytes that aren't UTF-8 copy in; as text they name nothing.
            let odd = base + FRAME_SIZE + 32;
            crate::kassert_eq!(copy_to_user(odd, b"/\xff\xfe\0"), Ok(()));
            let p = UserPath::copy_in(UserPtr(odd), false);
            crate::kassert_eq!(p.as_ref().map(|p| p.as_bytes()), Ok(&b"/\xff\xfe"[..]));
            crate::kassert_eq!(p.as_ref().map(|p| p.as_str()), Ok(Err(errno::ENOENT)));
            // A string running into the unmapped page: `-EFAULT`.
            let edge = base + 2 * FRAME_SIZE - 4;
            crate::kassert_eq!(copy_to_user(edge, b"ABCD"), Ok(()));
            crate::kassert_eq!(UserPath::copy_in(UserPtr(edge), false), Err(errno::EFAULT));
            Ok(())
        })();
        space.teardown();
        result
    }
);

crate::ktest!(
    fn at_stat_from_metadata_places_each_field() {
        use lumen_vfs::{DevId, InodeType};

        let m = Metadata {
            dev: DevId::makedev(0x123, 0x4567),
            ino: 0x1122_3344_5566_7788,
            inode_type: InodeType::File,
            mode: 0o4755,
            nlink: 3,
            uid: 1000,
            gid: 2000,
            rdev: DevId::makedev(5, 1),
            size: 0x0102_0304_0506,
            blksize: 4096,
            blocks: 24,
            atime: Timespec { sec: 1_700_000_001, nsec: 11 },
            mtime: Timespec { sec: 1_700_000_002, nsec: 22 },
            ctime: Timespec { sec: -1, nsec: 33 },
        };
        let st = stat_from_metadata(core::hint::black_box(&m));
        // SAFETY: `general::stat` is 144 bytes of integers with an explicit
        // `__pad0` and no implicit padding, all initialized.
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                (&st as *const general::stat).cast::<u8>(),
                core::mem::size_of::<general::stat>(),
            )
        };
        let u64_at = |off: usize| u64::from_le_bytes(bytes[off..off + 8].try_into().expect("8 bytes"));
        let u32_at = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().expect("4 bytes"));
        // Where Linux x86-64 userspace reads each field.
        crate::kassert_eq!(u64_at(0), DevId::makedev(0x123, 0x4567).raw()); // st_dev
        crate::kassert_eq!(u64_at(8), 0x1122_3344_5566_7788); // st_ino
        crate::kassert_eq!(u64_at(16), 3); // st_nlink
        crate::kassert_eq!(u32_at(24), 0o104_755); // st_mode: S_IFREG | 0o4755
        crate::kassert_eq!(u32_at(28), 1000); // st_uid
        crate::kassert_eq!(u32_at(32), 2000); // st_gid
        crate::kassert_eq!(u32_at(36), 0); // __pad0
        crate::kassert_eq!(u64_at(40), (5 << 8) | 1); // st_rdev: 5:1
        crate::kassert_eq!(u64_at(48), 0x0102_0304_0506); // st_size
        crate::kassert_eq!(u64_at(56), 4096); // st_blksize
        crate::kassert_eq!(u64_at(64), 24); // st_blocks
        crate::kassert_eq!(u64_at(72), 1_700_000_001); // st_atime
        crate::kassert_eq!(u64_at(80), 11); // st_atime_nsec
        crate::kassert_eq!(u64_at(88), 1_700_000_002); // st_mtime
        crate::kassert_eq!(u64_at(96), 22); // st_mtime_nsec
        crate::kassert_eq!(u64_at(104) as i64, -1); // st_ctime, before 1970
        crate::kassert_eq!(u64_at(112), 33); // st_ctime_nsec
        crate::kassert!(bytes[120..144].iter().all(|&b| b == 0)); // __unused
        // A directory's type bits, and a size and block count past
        // `i64::MAX` saturating.
        let dir = Metadata { inode_type: InodeType::Dir, mode: 0o1777, size: u64::MAX, blocks: u64::MAX, ..m };
        let st = stat_from_metadata(core::hint::black_box(&dir));
        crate::kassert_eq!(st.st_mode, 0o041_777);
        crate::kassert_eq!(st.st_size, i64::MAX);
        crate::kassert_eq!(st.st_blocks, i64::MAX);
        Ok(())
    }
);

crate::ktest!(
    fn at_statx_from_metadata_places_each_field() {
        use lumen_vfs::{DevId, InodeType};

        let m = Metadata {
            dev: DevId::makedev(0x123, 0x4567),
            ino: 0x1122_3344_5566_7788,
            inode_type: InodeType::Symlink,
            mode: 0o777,
            nlink: 3,
            uid: 1000,
            gid: 2000,
            rdev: DevId::makedev(5, 1),
            size: u64::MAX,
            blksize: 4096,
            blocks: 24,
            atime: Timespec { sec: 1_700_000_001, nsec: 11 },
            mtime: Timespec { sec: 1_700_000_002, nsec: 22 },
            ctime: Timespec { sec: -1, nsec: 33 },
        };
        let stx = statx_from_metadata(core::hint::black_box(&m));
        // SAFETY: `general::statx` is 256 bytes of integers with explicit
        // spare fields and no implicit padding, all initialized.
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                (&stx as *const general::statx).cast::<u8>(),
                core::mem::size_of::<general::statx>(),
            )
        };
        let u64_at = |off: usize| u64::from_le_bytes(bytes[off..off + 8].try_into().expect("8 bytes"));
        let u32_at = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().expect("4 bytes"));
        let u16_at = |off: usize| u16::from_le_bytes(bytes[off..off + 2].try_into().expect("2 bytes"));
        crate::kassert_eq!(u32_at(0), general::STATX_BASIC_STATS); // stx_mask
        crate::kassert_eq!(u32_at(4), 4096); // stx_blksize
        crate::kassert_eq!(u64_at(8), 0); // stx_attributes
        crate::kassert_eq!(u32_at(16), 3); // stx_nlink
        crate::kassert_eq!(u32_at(20), 1000); // stx_uid
        crate::kassert_eq!(u32_at(24), 2000); // stx_gid
        crate::kassert_eq!(u16_at(28), 0o120_777); // stx_mode: S_IFLNK | 0o777
        crate::kassert_eq!(u64_at(32), 0x1122_3344_5566_7788); // stx_ino
        crate::kassert_eq!(u64_at(40), u64::MAX); // stx_size: unsigned, whole
        crate::kassert_eq!(u64_at(48), 24); // stx_blocks
        crate::kassert_eq!(u64_at(64), 1_700_000_001); // stx_atime.tv_sec
        crate::kassert_eq!(u32_at(72), 11); // stx_atime.tv_nsec
        crate::kassert_eq!(u64_at(80), 0); // stx_btime: not reported
        crate::kassert_eq!(u64_at(96) as i64, -1); // stx_ctime.tv_sec
        crate::kassert_eq!(u32_at(104), 33); // stx_ctime.tv_nsec
        crate::kassert_eq!(u64_at(112), 1_700_000_002); // stx_mtime.tv_sec
        crate::kassert_eq!(u32_at(120), 22); // stx_mtime.tv_nsec
        crate::kassert_eq!(u32_at(128), 5); // stx_rdev_major
        crate::kassert_eq!(u32_at(132), 1); // stx_rdev_minor
        crate::kassert_eq!(u32_at(136), 0x123); // stx_dev_major
        crate::kassert_eq!(u32_at(140), 0x4567); // stx_dev_minor
        crate::kassert!(bytes[144..256].iter().all(|&b| b == 0)); // mnt_id onwards
        Ok(())
    }
);

crate::ktest!(
    fn at_start_dir_resolves_dirfd_with_its_traps() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenGuard, mkdir_at};

        const DIR: &[u8] = b"/tmp/at-ktest";
        const FILE: &[u8] = b"/tmp/at-ktest/f";
        let tables = crate::vfs::tables();
        process::in_test_process(move |_| {
            let dir = mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
            let open = |path: &[u8], bits: u32| process::open_kernel_file(path, bits, 0o644).map_err(|_| "open");
            let file = open(FILE, OpenFlags::O_RDWR | OpenFlags::O_CREAT)?;
            let file_inode = Arc::clone(&file.inode);
            let dir_fd = process::install_current_fd(open(DIR, OpenFlags::O_RDONLY)?, false).map_err(|_| "fd")?;
            let path_fd = process::install_current_fd(open(DIR, OpenFlags::O_PATH)?, false).map_err(|_| "fd")?;
            let file_fd = process::install_current_fd(file, false).map_err(|_| "fd")?;
            let fd = |n: i32| Dirfd::Fd(Fd(n));
            // As a handler calls it: the path copied in (an empty one only
            // with `AT_EMPTY_PATH`), then resolved.
            let start =
                |d: Dirfd, p: &[u8], allow_empty: bool| start_dir(d, &UserPath::from_bytes(p.to_vec(), allow_empty)?);
            let must = |r: Result<Start, u32>| r.map_err(|_| "start_dir refused");
            let is = |s: &Start, inode: &Arc<dyn VfsInode>| s.dir().is_some_and(|d| Arc::ptr_eq(d, inode));

            // `AT_FDCWD` with a relative path: the cwd's inode itself, never
            // `None` (lumen-vfs's global root). With the cwd moved to DIR,
            // "f" resolves there, where from the root it names nothing.
            let root = crate::vfs::resolve_path_from_root("/").map_err(|_| "resolve /")?;
            let s = must(start(Dirfd::Cwd, b"f", false))?;
            kassert!(is(&s, &root));
            let guard = OpenGuard::try_new(Arc::clone(&dir)).map_err(|_| "guard")?;
            process::set_current_cwd(process::Cwd::new(guard, "/tmp/at-ktest").map_err(|_| "cwd")?);
            let s = must(start(Dirfd::Cwd, b"f", false))?;
            kassert!(is(&s, &dir));
            let follow = ResolveFlags { follow_final: true, empty_path: false, require_dir: false };
            let f = UserPath::from_bytes(b"f".to_vec(), false).map_err(|_| "path")?;
            let found = s.resolve(&f, follow).map_err(|_| "resolve f from the cwd")?;
            kassert!(Arc::ptr_eq(&found.inode, &file_inode));
            kassert_eq!(s.display_path("f").as_deref(), Some("/tmp/at-ktest/f"));
            let from_root = resolve_path(tables, None, b"f", follow).err();
            kassert_eq!(from_root, Some(lumen_vfs::VfsError::NoEnt));
            // An empty path with `AT_EMPTY_PATH`: the cwd.
            kassert!(is(&must(start(Dirfd::Cwd, b"", true))?, &dir));

            // A directory fd (an `O_PATH` one too) with a relative path.
            kassert!(is(&must(start(fd(dir_fd), b"f", false))?, &dir));
            kassert!(is(&must(start(fd(path_fd), b"f", false))?, &dir));
            kassert_eq!(must(start(fd(dir_fd), b"f", false))?.display_path("f").as_deref(), Some("/tmp/at-ktest/f"));

            // An absolute path ignores the dirfd: a closed, negative or
            // non-directory one is no error, and it starts at the root.
            for d in [fd(999), fd(-5), fd(file_fd), fd(dir_fd), Dirfd::Cwd] {
                let s = must(start(d, b"/tmp", false))?;
                kassert!(s.dir().is_none());
                kassert!(s.display_path("/tmp").is_none());
            }

            // A relative or empty path through a bad fd: `-EBADF`.
            for (p, allow_empty) in [(&b"f"[..], false), (&b""[..], true)] {
                kassert_eq!(start(fd(999), p, allow_empty).err(), Some(errno::EBADF));
                kassert_eq!(start(fd(-1), p, allow_empty).err(), Some(errno::EBADF));
            }
            // A relative path through a non-directory: `-ENOTDIR`; but with
            // `AT_EMPTY_PATH` and an empty path, the file itself.
            kassert_eq!(start(fd(file_fd), b"x", false).err(), Some(errno::ENOTDIR));
            kassert!(is(&must(start(fd(file_fd), b"", true))?, &file_inode));
            // An empty path without `AT_EMPTY_PATH`: `-ENOENT` (copying it in).
            kassert_eq!(start(fd(dir_fd), b"", false).err(), Some(errno::ENOENT));
            kassert_eq!(start(Dirfd::Cwd, b"", false).err(), Some(errno::ENOENT));
            // A closed fd: `-EBADF` again.
            process::close_current_fd(dir_fd).map_err(|_| "close")?;
            kassert_eq!(start(fd(dir_fd), b"f", false).err(), Some(errno::EBADF));
            Ok(())
        })??;
        kassert!(lumen_vfs::unlink_at(tables, None, FILE, false).is_ok());
        kassert!(lumen_vfs::unlink_at(tables, None, DIR, true).is_ok());
        Ok(())
    }
);

crate::ktest!(
    fn at_start_operations_run_from_their_start() {
        // Each of `Start`'s lumen-vfs operations, from a cwd off `/` and from
        // a dirfd, with relative paths that name nothing from `/`: `mkdir`
        // and `open` take `Mode` and apply the umask themselves.
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenGuard, mkdir_at};

        const DIR: &[u8] = b"/tmp/at-ktest-ops";
        let tables = crate::vfs::tables();
        process::in_test_process(move |_| {
            let base = mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
            let guard = OpenGuard::try_new(Arc::clone(&base)).map_err(|_| "guard")?;
            process::set_current_cwd(process::Cwd::new(guard, "/tmp/at-ktest-ops").map_err(|_| "cwd")?);
            let path = |p: &[u8]| UserPath::from_bytes(p.to_vec(), false).map_err(|_| "path");
            let start = |d: Dirfd, p: &UserPath| start_dir(d, p).map_err(|_| "start_dir");
            let mode = |m: u64| reg::<Mode>(m);
            let mode_of = |p: &str| crate::vfs::resolve_path_from_root(p).and_then(|i| i.stat()).map(|m| m.mode);

            let d = path(b"d")?;
            let cwd = start(Dirfd::Cwd, &d)?;
            kassert!(cwd.mkdir(&d, mode(0o6777)).is_ok());
            kassert_eq!(mode_of("/tmp/at-ktest-ops/d"), Ok(0o755));
            let f = path(b"d/f")?;
            let flags = OpenFlags(OpenFlags::O_WRONLY | OpenFlags::O_CREAT);
            kassert!(cwd.open(&f, flags, mode(0o666)).is_ok());
            kassert_eq!(mode_of("/tmp/at-ktest-ops/d/f"), Ok(0o644));
            let s = path(b"d/s")?;
            kassert!(cwd.symlink(b"f", &s).is_ok());
            kassert_eq!(cwd.readlink(&s), Ok(b"f".to_vec()));
            let h = path(b"d/h")?;
            kassert_eq!(Start::link(&cwd, &f, &cwd, &h, false, false), Ok(()));
            kassert_eq!(mode_of("/tmp/at-ktest-ops/d/h"), Ok(0o644));
            // Across two starts: the cwd's "d/h" to "r" in a dirfd on "d".
            let d_file =
                process::open_kernel_file(b"/tmp/at-ktest-ops/d", OpenFlags::O_RDONLY, 0).map_err(|_| "open d")?;
            let d_fd = process::install_current_fd(d_file, false).map_err(|_| "fd")?;
            let r = path(b"r")?;
            let in_d = start(Dirfd::Fd(Fd(d_fd)), &r)?;
            kassert_eq!(Start::rename(&cwd, &h, &in_d, &r, RenameFlags(0)), Ok(()));
            kassert!(mode_of("/tmp/at-ktest-ops/d/r").is_ok());
            kassert!(mode_of("/tmp/at-ktest-ops/d/h").is_err());
            kassert_eq!(in_d.unlink(&r, false), Ok(()));
            kassert_eq!(cwd.unlink(&s, false), Ok(()));
            kassert_eq!(cwd.unlink(&f, false), Ok(()));
            kassert_eq!(cwd.unlink(&d, true), Ok(()));
            kassert!(mode_of("/tmp/at-ktest-ops/d").is_err());
            Ok(())
        })??;
        kassert!(lumen_vfs::unlink_at(tables, None, DIR, true).is_ok());
        Ok(())
    }
);
