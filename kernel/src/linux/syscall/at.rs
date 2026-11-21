//! What the M7 file syscalls share (#254's clusters): decoding their
//! register arguments, copying a path argument in, and filling a Linux
//! `struct stat` from VFS metadata.
//!
//! Each handler decodes its registers on its first line, with
//! [`FromReg::from_reg`], into the type Linux reads: an `int`, `mode_t` or
//! `unsigned int` is the register's low 32 bits, whatever the upper half
//! holds (`musl` passes a 32-bit `int` with garbage or a sign extension
//! above it). Comparing a whole register with `AT_FDCWD` would miss one.
//!
//! Turning a [`Dirfd`] into the directory a path starts from needs the FD
//! table, so #254's FD-table PR adds it here.

// Wave 3's handlers are the callers; until then only the ktests use these.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::vec::Vec;

use lumen_vfs::{AtFlags, Metadata, OpenFlags, RenameFlags};

use crate::linux::{errno, general};
use crate::user::{CopyError, copy_cstr_from_user};

/// A syscall argument decoded from its register as Linux reads it.
pub(super) trait FromReg {
    fn from_reg(reg: u64) -> Self;
}

/// A file descriptor argument (`int fd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fd(pub(super) i32);

impl Fd {
    /// The FD table index, or `None` for a negative descriptor (`-EBADF`
    /// to the caller).
    pub(super) fn index(self) -> Option<usize> {
        usize::try_from(self.0).ok()
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

/// A mode argument (`mode_t`/`umode_t mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mode(pub(super) u32);

impl Mode {
    /// The permission and special bits (`mode & 0o7777`), all that `open`,
    /// `mkdir` and `chmod` keep. Linux reads a 16-bit `umode_t`, which this
    /// mask makes no different from the low 32 bits.
    pub(super) fn perm(self) -> u32 {
        self.0 & 0o7777
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

/// A user pointer argument: the whole register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UserPtr(pub(super) u64);

impl UserPtr {
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
    /// Copy the NUL-terminated path at `ptr`: `-EFAULT` for a bad pointer
    /// (`NULL` too: a caller for which `NULL` means something, as
    /// `utimensat`'s path, checks first), `-ENAMETOOLONG` past `PATH_MAX`
    /// bytes with its NUL, `-ENOENT` if empty unless `allow_empty`
    /// (`AT_EMPTY_PATH`). Copies with interrupts enabled and no lock held.
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
    pub(super) fn as_str(&self) -> Result<&str, u32> {
        core::str::from_utf8(&self.0).map_err(|_| errno::ENOENT)
    }
}

/// The Linux x86-64 `struct stat` for `m` (`stat`, `lstat`, `newfstatat`,
/// `fstat`). `st_dev` and `st_rdev` are glibc's `makedev` encoding, which
/// `DevId` holds and `major()`/`minor()` read back.
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
        // A file's size and blocks fit `loff_t`; saturate rather than wrap
        // should a filesystem ever report more.
        st_size: i64::try_from(m.size).unwrap_or(i64::MAX),
        st_blksize: i64::from(m.blksize),
        st_blocks: i64::try_from(m.blocks).unwrap_or(i64::MAX),
        st_atime: m.atime.sec as u64,
        st_atime_nsec: m.atime.nsec as u64,
        st_mtime: m.mtime.sec as u64,
        st_mtime_nsec: m.mtime.nsec as u64,
        st_ctime: m.ctime.sec as u64,
        st_ctime_nsec: m.ctime.nsec as u64,
        __unused: [0; 3],
    }
}

crate::ktest!(
    fn at_decoders_read_the_low_32_bits() {
        // `AT_FDCWD` as musl passes it (sign-extended), zero-extended, and
        // with junk above it: all the working directory.
        for reg in [0xffff_ffff_ffff_ff9c, 0x0000_0000_ffff_ff9c, 0x1234_5678_ffff_ff9c] {
            crate::kassert_eq!(Dirfd::from_reg(reg), Dirfd::Cwd);
        }
        // -100 is the only such number: -101 and 100 are descriptors.
        crate::kassert_eq!(Dirfd::from_reg(0xffff_ffff_ffff_ff9b), Dirfd::Fd(Fd(-101)));
        crate::kassert_eq!(Dirfd::from_reg(100), Dirfd::Fd(Fd(100)));
        // A descriptor is the low 32 bits: junk above 3 is fd 3; a high-bit
        // fd is negative, no table index.
        crate::kassert_eq!(Fd::from_reg(0xdead_beef_0000_0003), Fd(3));
        crate::kassert_eq!(Fd::from_reg(0xdead_beef_0000_0003).index(), Some(3));
        crate::kassert_eq!(Fd::from_reg(0x8000_0000), Fd(i32::MIN));
        crate::kassert_eq!(Fd::from_reg(0x8000_0000).index(), None);
        crate::kassert_eq!(Fd::from_reg(u64::MAX).index(), None);
        crate::kassert_eq!(Fd::from_reg(0x7fff_ffff).index(), Some(0x7fff_ffff));
        // A mode keeps its permission and special bits only.
        crate::kassert_eq!(Mode::from_reg(0xffff_ffff_0000_81ed).perm(), 0o755);
        crate::kassert_eq!(Mode::from_reg(0o107_777).perm(), 0o7777);
        crate::kassert_eq!(Mode::from_reg(0o170_000).perm(), 0);
        // Flags are the low 32 bits.
        crate::kassert_eq!(AtFlags::from_reg(0xffff_ffff_0000_0100), AtFlags(general::AT_SYMLINK_NOFOLLOW));
        crate::kassert_eq!(OpenFlags::from_reg(0x1_0001_0000), OpenFlags(general::O_DIRECTORY));
        crate::kassert_eq!(RenameFlags::from_reg(0x1_0000_0001), RenameFlags(general::RENAME_NOREPLACE));
        crate::kassert!(UserPtr::from_reg(0).is_null());
        crate::kassert_eq!(UserPtr::from_reg(0xdead_beef_0000_1000).addr(), 0xdead_beef_0000_1000);
        // lumen-vfs's copies of the flags the decoders hand it agree with
        // linux-raw-sys's.
        crate::kassert_eq!(AtFlags::AT_FDCWD, general::AT_FDCWD);
        crate::kassert_eq!(AtFlags::AT_SYMLINK_NOFOLLOW, general::AT_SYMLINK_NOFOLLOW);
        crate::kassert_eq!(AtFlags::AT_REMOVEDIR, general::AT_REMOVEDIR);
        crate::kassert_eq!(AtFlags::AT_EACCESS, general::AT_EACCESS);
        crate::kassert_eq!(AtFlags::AT_SYMLINK_FOLLOW, general::AT_SYMLINK_FOLLOW);
        crate::kassert_eq!(AtFlags::AT_NO_AUTOMOUNT, general::AT_NO_AUTOMOUNT);
        crate::kassert_eq!(AtFlags::AT_EMPTY_PATH, general::AT_EMPTY_PATH);
        crate::kassert_eq!(OpenFlags::O_ACCMODE, general::O_ACCMODE);
        crate::kassert_eq!(OpenFlags::O_DIRECTORY, general::O_DIRECTORY);
        crate::kassert_eq!(OpenFlags::O_NOFOLLOW, general::O_NOFOLLOW);
        crate::kassert_eq!(OpenFlags::O_CLOEXEC, general::O_CLOEXEC);
        crate::kassert_eq!(OpenFlags::O_PATH, general::O_PATH);
        crate::kassert_eq!(OpenFlags::O_TMPFILE, general::O_TMPFILE);
        crate::kassert_eq!(OpenFlags::O_CREAT, general::O_CREAT);
        crate::kassert_eq!(OpenFlags::O_EXCL, general::O_EXCL);
        crate::kassert_eq!(OpenFlags::O_TRUNC, general::O_TRUNC);
        crate::kassert_eq!(OpenFlags::O_APPEND, general::O_APPEND);
        crate::kassert_eq!(OpenFlags::O_NONBLOCK, general::O_NONBLOCK);
        crate::kassert_eq!(RenameFlags::NOREPLACE, general::RENAME_NOREPLACE);
        crate::kassert_eq!(RenameFlags::EXCHANGE, general::RENAME_EXCHANGE);
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
    fn at_stat_from_metadata_places_each_field() {
        use lumen_vfs::{DevId, InodeType, Timespec};

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
        // A directory's type bits, and a size past `i64::MAX` saturating.
        let dir = Metadata { inode_type: InodeType::Dir, mode: 0o1777, size: u64::MAX, ..m };
        let st = stat_from_metadata(core::hint::black_box(&dir));
        crate::kassert_eq!(st.st_mode, 0o041_777);
        crate::kassert_eq!(st.st_size, i64::MAX);
        Ok(())
    }
);
