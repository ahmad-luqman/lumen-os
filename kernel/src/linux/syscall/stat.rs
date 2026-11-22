//! File status and access checks (M7): `stat`, `lstat`, `newfstatat`,
//! `statx`, `access`, `faccessat`, `faccessat2`.
//!
//! Each resolves its path once, through `at::start_dir` and
//! `Start::resolve` (following the final symlink unless
//! `AT_SYMLINK_NOFOLLOW`), and takes the inode's `Metadata` by value: no VFS
//! lock is held, nor the inode kept, across the copy to user memory.
//! `stat`, `lstat`, `access` and `faccessat` are shims over `newfstatat` and
//! `faccessat2` with `AT_FDCWD` and fixed flags.
//!
//! Every Lumen process runs as uid and gid 0, so `access` follows Linux's
//! rules for root (`lumen_vfs::check_access`): `R_OK` and `W_OK` pass (but
//! `W_OK` on a read-only mount is `-EROFS`, a devfs device node excepted),
//! `X_OK` needs an execute bit or a directory, `F_OK` is existence alone.
//! With the real and effective ids the same, `AT_EACCESS` changes nothing.

use super::at::{Dirfd, UserPath, UserPtr, start_dir, stat_from_metadata, statx_from_metadata};
use super::{copy_pod_to_user, finish};
use crate::linux::general;
use crate::user::CopyError;
use lumen_vfs::{AccessMask, AtFlags, Metadata, ResolveFlags, VfsError, check_access};

/// The uid and gid every Lumen process runs as (no credentials yet).
const ROOT_ID: u32 = 0;

/// The flags `newfstatat` and `statx` take, as Linux's `vfs_statx` does.
/// `AT_NO_AUTOMOUNT` (Lumen has no automounts; GNU `stat` passes it) and the
/// sync type (every filesystem's metadata is current in memory) are
/// accepted and ignored.
const STAT_FLAGS: u32 =
    general::AT_SYMLINK_NOFOLLOW | general::AT_EMPTY_PATH | general::AT_NO_AUTOMOUNT | general::AT_STATX_SYNC_TYPE;

/// The flags `faccessat2` takes, as Linux's (`AT_EMPTY_PATH` since 5.8).
const FACCESSAT_FLAGS: u32 = general::AT_EACCESS | general::AT_SYMLINK_NOFOLLOW | general::AT_EMPTY_PATH;

/// `stat(path, buf)` (4): `newfstatat(AT_FDCWD, path, buf, 0)`.
pub(super) fn sys_stat(path: UserPtr, buf: UserPtr) -> u64 {
    sys_newfstatat(Dirfd::Cwd, path, buf, AtFlags(0))
}

/// `lstat(path, buf)` (6): `newfstatat(AT_FDCWD, path, buf,
/// AT_SYMLINK_NOFOLLOW)`.
pub(super) fn sys_lstat(path: UserPtr, buf: UserPtr) -> u64 {
    sys_newfstatat(Dirfd::Cwd, path, buf, AtFlags(general::AT_SYMLINK_NOFOLLOW))
}

/// `newfstatat(dirfd, path, buf, flags)` (262): the 144-byte `struct stat`.
pub(super) fn sys_newfstatat(dirfd: Dirfd, path: UserPtr, buf: UserPtr, flags: AtFlags) -> u64 {
    finish(newfstatat(dirfd, path, buf, flags))
}

fn newfstatat(dirfd: Dirfd, path: UserPtr, buf: UserPtr, flags: AtFlags) -> Result<u64, u32> {
    let flags = flags.validate(STAT_FLAGS).map_err(VfsError::to_errno)?;
    let st = stat_from_metadata(&metadata_at(dirfd, path, flags)?);
    // SAFETY: `general::stat` is 144 bytes of integers with an explicit
    // `__pad0` and no implicit padding, all initialized.
    unsafe { copy_pod_to_user(buf.addr(), &st) }.map_err(CopyError::to_errno)?;
    Ok(0)
}

/// `statx(dirfd, path, flags, mask, buf)` (332): the 256-byte `struct
/// statx`. Whatever `mask` asks for, the basic fields are filled, and
/// `stx_mask` says so (`STATX_BASIC_STATS`), as Linux may answer.
pub(super) fn sys_statx(dirfd: Dirfd, path: UserPtr, flags: AtFlags, mask: u32, buf: UserPtr) -> u64 {
    finish(statx(dirfd, path, flags, mask, buf))
}

fn statx(dirfd: Dirfd, path: UserPtr, flags: AtFlags, mask: u32, buf: UserPtr) -> Result<u64, u32> {
    let flags = flags.validate(STAT_FLAGS).map_err(VfsError::to_errno)?;
    // Both sync bits at once ask for two things (`statx`'s `-EINVAL`, not
    // `newfstatat`'s), as does the bit reserved for extending `struct statx`.
    if flags.0 & general::AT_STATX_SYNC_TYPE == general::AT_STATX_SYNC_TYPE || mask & general::STATX__RESERVED != 0 {
        return Err(VfsError::Inval.to_errno());
    }
    let stx = statx_from_metadata(&metadata_at(dirfd, path, flags)?);
    // SAFETY: `general::statx` is 256 bytes of integers with explicit spare
    // fields and no implicit padding, all initialized.
    unsafe { copy_pod_to_user(buf.addr(), &stx) }.map_err(CopyError::to_errno)?;
    Ok(0)
}

/// The metadata of what `path` names from `dirfd` (`flags` already
/// validated): the file itself for an empty path with `AT_EMPTY_PATH`, the
/// final symlink itself with `AT_SYMLINK_NOFOLLOW`. The resolved inode is
/// dropped before this returns.
fn metadata_at(dirfd: Dirfd, path: UserPtr, flags: AtFlags) -> Result<Metadata, u32> {
    let path = UserPath::copy_in(path, flags.has_empty_path())?;
    let start = start_dir(dirfd, &path)?;
    let how = ResolveFlags {
        follow_final: !flags.has_symlink_nofollow(),
        empty_path: flags.has_empty_path(),
        require_dir: false,
    };
    let found = start.resolve(&path, how)?;
    found.inode.stat().map_err(VfsError::to_errno)
}

/// `access(path, mode)` (21): `faccessat2(AT_FDCWD, path, mode, 0)`.
pub(super) fn sys_access(path: UserPtr, mode: AccessMask) -> u64 {
    sys_faccessat2(Dirfd::Cwd, path, mode, AtFlags(0))
}

/// `faccessat(dirfd, path, mode)` (269): `faccessat2(dirfd, path, mode, 0)`.
/// Three arguments on Linux x86-64: `r10` is never passed (`musl` leaves it
/// uninitialized).
pub(super) fn sys_faccessat(dirfd: Dirfd, path: UserPtr, mode: AccessMask) -> u64 {
    sys_faccessat2(dirfd, path, mode, AtFlags(0))
}

/// `faccessat2(dirfd, path, mode, flags)` (439).
pub(super) fn sys_faccessat2(dirfd: Dirfd, path: UserPtr, mode: AccessMask, flags: AtFlags) -> u64 {
    finish(faccessat2(dirfd, path, mode, flags))
}

fn faccessat2(dirfd: Dirfd, path: UserPtr, mode: AccessMask, flags: AtFlags) -> Result<u64, u32> {
    // The mode and flags first: an invalid one is `-EINVAL` whatever the path.
    let mode = mode.validate().map_err(VfsError::to_errno)?;
    let flags = flags.validate(FACCESSAT_FLAGS).map_err(VfsError::to_errno)?;
    let path = UserPath::copy_in(path, flags.has_empty_path())?;
    let start = start_dir(dirfd, &path)?;
    let how = ResolveFlags {
        follow_final: !flags.has_symlink_nofollow(),
        empty_path: flags.has_empty_path(),
        require_dir: false,
    };
    let found = start.resolve(&path, how)?;
    check_access(crate::vfs::tables(), &found.inode, mode, ROOT_ID, ROOT_ID).map_err(VfsError::to_errno)?;
    Ok(0)
}

#[cfg(feature = "ktest")]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::size_of;

    use super::*;
    use crate::linux::errno;
    use crate::linux::process::{self, KTEST_USER_LEN, KTEST_USER_VA};
    use crate::linux::syscall::at::{Fd, reg};
    use crate::linux::syscall::fd::ktest_user_path;
    use crate::linux::syscall::neg_errno;
    use crate::user::{copy_from_user, copy_to_user};
    use crate::{kassert, kassert_eq};
    use lumen_vfs::{OpenFlags, resolve_path, symlink_at, unlink_at};

    /// The user buffers the tests copy into: past the path slots (the first
    /// page), one `struct stat` or `struct statx` each.
    const BUF_A: u64 = KTEST_USER_VA + 4096;
    const BUF_B: u64 = KTEST_USER_VA + 4096 + 512;
    /// Where a path of `PATH_MAX` bytes goes: the last two pages.
    const LONG_PATH: u64 = KTEST_USER_VA + 2 * 4096;

    /// A POD ABI struct's bytes.
    fn bytes_of<T: Copy>(v: &T) -> Vec<u8> {
        // SAFETY: called only on `general::stat` and `general::statx`, plain
        // integers with explicit padding fields and no implicit padding.
        unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
    }

    /// `len` bytes of user memory at `at`.
    fn user_bytes(at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut buf = vec![0u8; len];
        copy_from_user(&mut buf, at).map_err(|_| "copy from user memory")?;
        Ok(buf)
    }

    /// Fill `len` bytes at `at` with a pattern, so a test can tell a written
    /// buffer from one left alone.
    fn scribble(at: u64, len: usize) -> Result<(), &'static str> {
        copy_to_user(at, &vec![0xa5; len]).map_err(|_| "scribble")
    }

    /// The metadata of the absolute `path`, resolved kernel-side.
    fn meta(path: &[u8], follow: bool) -> Result<Metadata, &'static str> {
        let how = ResolveFlags { follow_final: follow, empty_path: false, require_dir: false };
        let found = resolve_path(crate::vfs::tables(), None, path, how).map_err(|_| "resolve kernel-side")?;
        found.inode.stat().map_err(|_| "stat kernel-side")
    }

    /// The `struct stat` Linux would write for `path`.
    fn want_stat(path: &[u8], follow: bool) -> Result<Vec<u8>, &'static str> {
        Ok(bytes_of(&stat_from_metadata(&meta(path, follow)?)))
    }

    fn want_statx(path: &[u8], follow: bool) -> Result<Vec<u8>, &'static str> {
        Ok(bytes_of(&statx_from_metadata(&meta(path, follow)?)))
    }

    fn stat_at(at: u64) -> Result<Vec<u8>, &'static str> {
        user_bytes(at, size_of::<general::stat>())
    }

    fn statx_at(at: u64) -> Result<Vec<u8>, &'static str> {
        user_bytes(at, size_of::<general::statx>())
    }

    /// Create the file `path` with exactly `mode` (no umask).
    fn create(path: &[u8], mode: u16) -> Result<(), &'static str> {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_EXCL;
        process::open_kernel_file(path, flags, mode).map(drop).map_err(|_| "create a file")
    }

    /// An open descriptor on `path` with `flags`.
    fn open_fd(path: &[u8], flags: u32) -> Result<Fd, &'static str> {
        let file = process::open_kernel_file(path, flags, 0).map_err(|_| "open")?;
        process::install_current_fd(file, false).map(|n| reg::<Fd>(n as u64)).map_err(|_| "install the fd")
    }

    /// Remove what a test made, the deepest first; whatever is missing is
    /// skipped (a failed test may not have made it).
    fn clean(files: &[&[u8]], dirs: &[&[u8]]) {
        let tables = crate::vfs::tables();
        for f in files {
            let _ = unlink_at(tables, None, f, false);
        }
        for d in dirs {
            let _ = unlink_at(tables, None, d, true);
        }
    }

    fn at(flags: u32) -> AtFlags {
        reg(u64::from(flags))
    }

    fn mode(bits: u32) -> AccessMask {
        reg(u64::from(bits))
    }

    crate::ktest!(
        fn stat_lstat_and_newfstatat_on_a_file_a_directory_and_a_symlink() {
            const DIR: &[u8] = b"/tmp/stat-ktest-kinds";
            const FILE: &[u8] = b"/tmp/stat-ktest-kinds/f";
            const LINK: &[u8] = b"/tmp/stat-ktest-kinds/l";
            const DIR_LINK: &[u8] = b"/tmp/stat-ktest-kinds/dl";
            let tables = crate::vfs::tables();
            let result = process::in_test_process(move |_| {
                lumen_vfs::mkdir_at(tables, None, DIR, 0o750, 0).map_err(|_| "mkdir")?;
                create(FILE, 0o640)?;
                {
                    let file = process::open_kernel_file(FILE, OpenFlags::O_WRONLY, 0).map_err(|_| "open")?;
                    file.inode.write_at(0, b"twelve bytes").map_err(|_| "write")?;
                }
                symlink_at(tables, b"f", None, LINK).map_err(|_| "symlink")?;
                symlink_at(tables, DIR, None, DIR_LINK).map_err(|_| "symlink")?;
                let file = ktest_user_path(0, FILE)?;
                let dir = ktest_user_path(1, DIR)?;
                let link = ktest_user_path(2, LINK)?;
                let dir_link_slash = ktest_user_path(3, b"/tmp/stat-ktest-kinds/dl/")?;

                // A regular file: `stat` and `lstat` alike, its type, mode and size.
                kassert_eq!(sys_stat(file, reg(BUF_A)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                let st = meta(FILE, true)?;
                kassert_eq!(st.st_mode(), 0o100_640);
                kassert_eq!(st.size, 12);
                kassert_eq!(sys_lstat(file, reg(BUF_B)), 0);
                kassert_eq!(stat_at(BUF_B)?, stat_at(BUF_A)?);
                // A directory.
                kassert_eq!(sys_stat(dir, reg(BUF_A)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(DIR, true)?);
                kassert_eq!(meta(DIR, true)?.st_mode(), 0o040_750);
                // A symlink: `stat` follows it to the file, `lstat` doesn't.
                kassert_eq!(sys_stat(link, reg(BUF_A)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                kassert_eq!(sys_lstat(link, reg(BUF_B)), 0);
                kassert_eq!(stat_at(BUF_B)?, want_stat(LINK, false)?);
                kassert_eq!(meta(LINK, false)?.st_mode() & 0o170_000, 0o120_000);
                kassert!(stat_at(BUF_A)? != stat_at(BUF_B)?);
                // `newfstatat` alike, `AT_SYMLINK_NOFOLLOW` being `lstat`.
                kassert_eq!(sys_newfstatat(Dirfd::Cwd, link, reg(BUF_A), at(0)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                kassert_eq!(sys_newfstatat(Dirfd::Cwd, link, reg(BUF_A), at(general::AT_SYMLINK_NOFOLLOW)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LINK, false)?);
                // A trailing slash follows even `lstat`'s final symlink.
                kassert_eq!(sys_lstat(dir_link_slash, reg(BUF_A)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(DIR, true)?);

                // Relative to a directory fd, names that mean nothing from `/`.
                let dir_fd = Dirfd::Fd(open_fd(DIR, OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?);
                let rel_link = ktest_user_path(4, b"l")?;
                kassert_eq!(sys_newfstatat(dir_fd, rel_link, reg(BUF_A), at(0)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                kassert_eq!(sys_newfstatat(dir_fd, rel_link, reg(BUF_A), at(general::AT_SYMLINK_NOFOLLOW)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LINK, false)?);
                kassert_eq!(sys_stat(rel_link, reg(BUF_A)), neg_errno(errno::ENOENT));
                // ... and relative to the cwd, once it is that directory.
                let guard = lumen_vfs::OpenGuard::try_new(
                    crate::vfs::resolve_path_from_root("/tmp/stat-ktest-kinds").map_err(|_| "resolve")?,
                )
                .map_err(|_| "guard")?;
                process::set_current_cwd(process::Cwd::new(guard, "/tmp/stat-ktest-kinds").map_err(|_| "cwd")?);
                kassert_eq!(sys_stat(rel_link, reg(BUF_A)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);

                // `AT_EMPTY_PATH` with an empty path: the dirfd's file itself,
                // a directory, a regular file, or the cwd.
                let empty = ktest_user_path(5, b"")?;
                kassert_eq!(sys_newfstatat(dir_fd, empty, reg(BUF_A), at(general::AT_EMPTY_PATH)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(DIR, true)?);
                let file_fd = Dirfd::Fd(open_fd(FILE, OpenFlags::O_RDONLY)?);
                kassert_eq!(sys_newfstatat(file_fd, empty, reg(BUF_A), at(general::AT_EMPTY_PATH)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                kassert_eq!(sys_newfstatat(Dirfd::Cwd, empty, reg(BUF_A), at(general::AT_EMPTY_PATH)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(DIR, true)?);
                // An `O_PATH` fd on the symlink itself, with both flags: the link.
                let link_fd = Dirfd::Fd(open_fd(LINK, OpenFlags::O_PATH | OpenFlags::O_NOFOLLOW)?);
                let both = general::AT_EMPTY_PATH | general::AT_SYMLINK_NOFOLLOW;
                kassert_eq!(sys_newfstatat(link_fd, empty, reg(BUF_A), at(both)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LINK, false)?);
                // `AT_EMPTY_PATH` with a path that isn't empty changes nothing.
                kassert_eq!(sys_newfstatat(dir_fd, rel_link, reg(BUF_A), at(general::AT_EMPTY_PATH)), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                Ok(())
            });
            clean(&[LINK, DIR_LINK, FILE], &[DIR]);
            result?
        }
    );

    crate::ktest!(
        fn stat_statx_mask_and_fields_agree_with_stat() {
            const DIR: &[u8] = b"/tmp/stat-ktest-statx";
            const FILE: &[u8] = b"/tmp/stat-ktest-statx/f";
            const LINK: &[u8] = b"/tmp/stat-ktest-statx/l";
            let tables = crate::vfs::tables();
            let result = process::in_test_process(move |_| {
                lumen_vfs::mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
                create(FILE, 0o604)?;
                {
                    let file = process::open_kernel_file(FILE, OpenFlags::O_WRONLY, 0).map_err(|_| "open")?;
                    file.inode.write_at(0, &[7u8; 5000]).map_err(|_| "write")?;
                }
                symlink_at(tables, b"f", None, LINK).map_err(|_| "symlink")?;
                let file = ktest_user_path(0, FILE)?;
                let link = ktest_user_path(1, LINK)?;
                let dir = ktest_user_path(2, DIR)?;
                let empty = ktest_user_path(3, b"")?;

                // Whatever the mask asks (nothing, a field, everything): the
                // basic fields, said so in `stx_mask`.
                for mask in [0, general::STATX_SIZE, general::STATX_ALL, 0x7fff_ffff] {
                    scribble(BUF_B, 256)?;
                    kassert_eq!(sys_statx(Dirfd::Cwd, file, at(0), mask, reg(BUF_B)), 0);
                    kassert_eq!(statx_at(BUF_B)?, want_statx(FILE, true)?);
                }
                // Field by field against `stat`'s answer.
                kassert_eq!(sys_stat(file, reg(BUF_A)), 0);
                let st = stat_at(BUF_A)?;
                let stx = statx_at(BUF_B)?;
                let u64_of = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().expect("8 bytes"));
                let u32_of = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"));
                let u16_of = |b: &[u8], o: usize| u16::from_le_bytes(b[o..o + 2].try_into().expect("2 bytes"));
                kassert_eq!(u32_of(&stx, 0), general::STATX_BASIC_STATS); // stx_mask
                kassert_eq!(u64::from(u32_of(&stx, 4)), u64_of(&st, 56)); // blksize
                kassert_eq!(u64::from(u32_of(&stx, 16)), u64_of(&st, 16)); // nlink
                kassert_eq!(u32_of(&stx, 20), u32_of(&st, 28)); // uid
                kassert_eq!(u32_of(&stx, 24), u32_of(&st, 32)); // gid
                kassert_eq!(u32::from(u16_of(&stx, 28)), u32_of(&st, 24)); // mode
                kassert_eq!(u32_of(&st, 24), 0o100_604);
                kassert_eq!(u64_of(&stx, 32), u64_of(&st, 8)); // ino
                kassert_eq!(u64_of(&stx, 40), u64_of(&st, 48)); // size
                kassert_eq!(u64_of(&st, 48), 5000);
                kassert_eq!(u64_of(&stx, 48), u64_of(&st, 64)); // blocks
                kassert_eq!(u64_of(&stx, 64), u64_of(&st, 72)); // atime
                kassert_eq!(u64::from(u32_of(&stx, 72)), u64_of(&st, 80));
                kassert_eq!(u64_of(&stx, 96), u64_of(&st, 104)); // ctime
                kassert_eq!(u64::from(u32_of(&stx, 104)), u64_of(&st, 112));
                kassert_eq!(u64_of(&stx, 112), u64_of(&st, 88)); // mtime
                kassert_eq!(u64::from(u32_of(&stx, 120)), u64_of(&st, 96));
                let dev = lumen_vfs::DevId::makedev(u32_of(&stx, 136), u32_of(&stx, 140));
                kassert_eq!(dev.raw(), u64_of(&st, 0)); // dev
                let rdev = lumen_vfs::DevId::makedev(u32_of(&stx, 128), u32_of(&stx, 132));
                kassert_eq!(rdev.raw(), u64_of(&st, 40)); // rdev

                // A symlink followed, or not; a directory; `AT_EMPTY_PATH` on an fd.
                kassert_eq!(sys_statx(Dirfd::Cwd, link, at(0), 0, reg(BUF_B)), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(FILE, true)?);
                kassert_eq!(sys_statx(Dirfd::Cwd, link, at(general::AT_SYMLINK_NOFOLLOW), 0, reg(BUF_B)), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(LINK, false)?);
                kassert_eq!(sys_statx(Dirfd::Cwd, dir, at(0), 0, reg(BUF_B)), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(DIR, true)?);
                let dir_fd = Dirfd::Fd(open_fd(DIR, OpenFlags::O_RDONLY)?);
                kassert_eq!(sys_statx(dir_fd, empty, at(general::AT_EMPTY_PATH), 0, reg(BUF_B)), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(DIR, true)?);
                let rel = ktest_user_path(4, b"l")?;
                kassert_eq!(sys_statx(dir_fd, rel, at(0), 0, reg(BUF_B)), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(FILE, true)?);

                // `AT_NO_AUTOMOUNT` and either sync type: accepted, ignored.
                for flags in [
                    general::AT_NO_AUTOMOUNT,
                    general::AT_STATX_SYNC_AS_STAT,
                    general::AT_STATX_FORCE_SYNC,
                    general::AT_STATX_DONT_SYNC,
                    general::AT_NO_AUTOMOUNT | general::AT_STATX_DONT_SYNC | general::AT_SYMLINK_NOFOLLOW,
                ] {
                    scribble(BUF_B, 256)?;
                    kassert_eq!(sys_statx(Dirfd::Cwd, link, at(flags), 0, reg(BUF_B)), 0);
                    let follow = flags & general::AT_SYMLINK_NOFOLLOW == 0;
                    kassert_eq!(statx_at(BUF_B)?, want_statx(if follow { FILE } else { LINK }, follow)?);
                }
                // Both sync types, the reserved mask bit or an unknown flag:
                // `-EINVAL`, the buffer untouched.
                scribble(BUF_B, 256)?;
                let einval = neg_errno(errno::EINVAL);
                kassert_eq!(sys_statx(Dirfd::Cwd, file, at(general::AT_STATX_SYNC_TYPE), 0, reg(BUF_B)), einval);
                kassert_eq!(sys_statx(Dirfd::Cwd, file, at(0), general::STATX__RESERVED, reg(BUF_B)), einval);
                kassert_eq!(sys_statx(Dirfd::Cwd, file, at(general::AT_REMOVEDIR), 0, reg(BUF_B)), einval);
                kassert_eq!(sys_statx(Dirfd::Cwd, file, at(0x1), 0, reg(BUF_B)), einval);
                kassert!(statx_at(BUF_B)?.iter().all(|&b| b == 0xa5));
                Ok(())
            });
            clean(&[LINK, FILE], &[DIR]);
            result?
        }
    );

    crate::ktest!(
        fn stat_access_modes_follow_the_root_rules() {
            const DIR: &[u8] = b"/tmp/stat-ktest-access";
            const PLAIN: &[u8] = b"/tmp/stat-ktest-access/plain";
            const EXEC: &[u8] = b"/tmp/stat-ktest-access/exec";
            const OTHER_X: &[u8] = b"/tmp/stat-ktest-access/other-x";
            const NONE: &[u8] = b"/tmp/stat-ktest-access/none";
            const SUBDIR: &[u8] = b"/tmp/stat-ktest-access/sub";
            const LINK: &[u8] = b"/tmp/stat-ktest-access/link";
            const DANGLING: &[u8] = b"/tmp/stat-ktest-access/dangling";
            let tables = crate::vfs::tables();
            let result = process::in_test_process(move |_| {
                lumen_vfs::mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
                create(PLAIN, 0o644)?;
                create(EXEC, 0o755)?;
                create(OTHER_X, 0o001)?;
                create(NONE, 0o000)?;
                lumen_vfs::mkdir_at(tables, None, SUBDIR, 0o600, 0).map_err(|_| "mkdir")?;
                symlink_at(tables, b"plain", None, LINK).map_err(|_| "symlink")?;
                symlink_at(tables, b"missing", None, DANGLING).map_err(|_| "symlink")?;
                let (f, r, w, x) = (general::F_OK, general::R_OK, general::W_OK, general::X_OK);
                let check = |slot: u64, path: &[u8], bits: u32| -> Result<u64, &'static str> {
                    Ok(sys_access(ktest_user_path(slot, path)?, mode(bits)))
                };
                let eacces = neg_errno(errno::EACCES);

                // `F_OK`, `R_OK`, `W_OK` pass on every file, even mode 0.
                for p in [PLAIN, EXEC, OTHER_X, NONE, SUBDIR] {
                    for bits in [f, r, w, r | w] {
                        kassert_eq!(check(0, p, bits)?, 0);
                    }
                }
                // `X_OK`: any execute bit, or a directory whatever its mode.
                kassert_eq!(check(0, PLAIN, x)?, eacces);
                kassert_eq!(check(0, PLAIN, r | w | x)?, eacces);
                kassert_eq!(check(0, NONE, x)?, eacces);
                kassert_eq!(check(0, EXEC, x)?, 0);
                kassert_eq!(check(0, EXEC, r | w | x)?, 0);
                kassert_eq!(check(0, OTHER_X, x)?, 0);
                kassert_eq!(check(0, SUBDIR, x)?, 0);
                // Missing: `-ENOENT` for every mode, `F_OK` included.
                for bits in [f, r, w, x] {
                    kassert_eq!(check(0, b"/tmp/stat-ktest-access/missing", bits)?, neg_errno(errno::ENOENT));
                }
                // An invalid mode: `-EINVAL`, before the path is looked at
                // (a missing one, a bad pointer).
                for bits in [8, 0x10, 0xffff_ffff] {
                    kassert_eq!(check(0, PLAIN, bits)?, neg_errno(errno::EINVAL));
                    kassert_eq!(check(0, b"/tmp/stat-ktest-access/missing", bits)?, neg_errno(errno::EINVAL));
                    kassert_eq!(sys_access(reg(0), mode(bits)), neg_errno(errno::EINVAL));
                }

                // A read-only mount: `W_OK` is `-EROFS` (the check comes
                // before root's pass), `R_OK` passes; a devfs device node is
                // exempt.
                let erofs = neg_errno(errno::EROFS);
                kassert_eq!(check(0, b"/proc/version", r)?, 0);
                kassert_eq!(check(0, b"/proc/version", w)?, erofs);
                kassert_eq!(check(0, b"/proc/version", r | w)?, erofs);
                kassert_eq!(check(0, b"/proc", w)?, erofs);
                kassert_eq!(check(0, b"/dev", w)?, erofs);
                kassert_eq!(check(0, b"/dev", x)?, 0);
                kassert_eq!(check(0, b"/dev/null", w)?, 0);
                kassert_eq!(check(0, b"/dev/null", r | w)?, 0);

                // A symlink is followed: to `plain`, no execute bit; with
                // `AT_SYMLINK_NOFOLLOW` the link itself (0o777).
                let link = ktest_user_path(1, LINK)?;
                let dangling = ktest_user_path(2, DANGLING)?;
                let nofollow = at(general::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_access(link, mode(x)), eacces);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, link, mode(x), at(0)), eacces);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, link, mode(x), nofollow), 0);
                kassert_eq!(sys_access(dangling, mode(f)), neg_errno(errno::ENOENT));
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, dangling, mode(f), nofollow), 0);
                // `AT_EACCESS`: the same answers (real and effective ids are 0).
                let eaccess = at(general::AT_EACCESS);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, link, mode(r | w), eaccess), 0);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, link, mode(x), eaccess), eacces);
                let exec = ktest_user_path(3, EXEC)?;
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, exec, mode(x), eaccess), 0);
                // Any other flag: `-EINVAL`, even on a missing path.
                let missing = ktest_user_path(4, b"/tmp/stat-ktest-access/missing")?;
                for bad in [general::AT_SYMLINK_FOLLOW, general::AT_NO_AUTOMOUNT, general::AT_STATX_DONT_SYNC, 0x1] {
                    kassert_eq!(sys_faccessat2(Dirfd::Cwd, exec, mode(f), at(bad)), neg_errno(errno::EINVAL));
                    kassert_eq!(sys_faccessat2(Dirfd::Cwd, missing, mode(f), at(bad)), neg_errno(errno::EINVAL));
                }

                // Relative to a directory fd, through `faccessat` and `faccessat2`.
                let dir_fd = Dirfd::Fd(open_fd(DIR, OpenFlags::O_RDONLY)?);
                let rel_exec = ktest_user_path(5, b"exec")?;
                let rel_plain = ktest_user_path(6, b"plain")?;
                kassert_eq!(sys_faccessat(dir_fd, rel_exec, mode(x)), 0);
                kassert_eq!(sys_faccessat(dir_fd, rel_plain, mode(x)), eacces);
                kassert_eq!(sys_faccessat(dir_fd, rel_plain, mode(r | w)), 0);
                kassert_eq!(sys_faccessat2(dir_fd, rel_exec, mode(x), at(0)), 0);
                kassert_eq!(sys_faccessat(Dirfd::Cwd, rel_exec, mode(f)), neg_errno(errno::ENOENT));
                kassert_eq!(sys_faccessat(dir_fd, exec, mode(x)), 0);

                // `AT_EMPTY_PATH` with an empty path: the dirfd's file itself
                // (or the cwd); without it, an empty path is `-ENOENT`.
                let empty = ktest_user_path(7, b"")?;
                let empty_path = at(general::AT_EMPTY_PATH);
                let plain_fd = Dirfd::Fd(open_fd(PLAIN, OpenFlags::O_RDONLY)?);
                let exec_fd = Dirfd::Fd(open_fd(EXEC, OpenFlags::O_PATH)?);
                kassert_eq!(sys_faccessat2(plain_fd, empty, mode(r | w), empty_path), 0);
                kassert_eq!(sys_faccessat2(plain_fd, empty, mode(x), empty_path), eacces);
                kassert_eq!(sys_faccessat2(exec_fd, empty, mode(x), empty_path), 0);
                kassert_eq!(sys_faccessat2(dir_fd, empty, mode(x), empty_path), 0);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, empty, mode(x), empty_path), 0);
                kassert_eq!(sys_faccessat2(plain_fd, empty, mode(f), at(0)), neg_errno(errno::ENOENT));
                kassert_eq!(sys_faccessat2(Dirfd::Fd(reg(999)), empty, mode(f), empty_path), neg_errno(errno::EBADF));
                Ok(())
            });
            clean(&[LINK, DANGLING, PLAIN, EXEC, OTHER_X, NONE], &[SUBDIR, DIR]);
            result?
        }
    );

    crate::ktest!(
        fn stat_family_errnos() {
            const DIR: &[u8] = b"/tmp/stat-ktest-errno";
            const FILE: &[u8] = b"/tmp/stat-ktest-errno/f";
            const LOOP_A: &[u8] = b"/tmp/stat-ktest-errno/a";
            const LOOP_B: &[u8] = b"/tmp/stat-ktest-errno/b";
            let tables = crate::vfs::tables();
            let result = process::in_test_process(move |_| {
                lumen_vfs::mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
                create(FILE, 0o644)?;
                symlink_at(tables, b"b", None, LOOP_A).map_err(|_| "symlink")?;
                symlink_at(tables, b"a", None, LOOP_B).map_err(|_| "symlink")?;
                let file = ktest_user_path(0, FILE)?;
                let missing = ktest_user_path(1, b"/tmp/stat-ktest-errno/missing")?;
                let through_file = ktest_user_path(2, b"/tmp/stat-ktest-errno/f/x")?;
                let file_slash = ktest_user_path(3, b"/tmp/stat-ktest-errno/f/")?;
                let looped = ktest_user_path(4, LOOP_A)?;
                let rel = ktest_user_path(5, b"f")?;
                let empty = ktest_user_path(6, b"")?;
                // `PATH_MAX` bytes before the NUL: one too many.
                let mut long = vec![b'a'; general::PATH_MAX as usize + 1];
                long[0] = b'/';
                long[general::PATH_MAX as usize] = 0;
                copy_to_user(LONG_PATH, &long).map_err(|_| "copy the long path")?;
                let long: UserPtr = reg(LONG_PATH);
                // A buffer running off the end of user memory.
                let edge: UserPtr = reg(KTEST_USER_VA + KTEST_USER_LEN - 8);
                let buf: UserPtr = reg(BUF_A);

                // Every call the same way: each of the seven, with `path`.
                let all = |path: UserPtr, buf: UserPtr| -> [u64; 7] {
                    [
                        sys_stat(path, buf),
                        sys_lstat(path, buf),
                        sys_newfstatat(Dirfd::Cwd, path, buf, at(0)),
                        sys_statx(Dirfd::Cwd, path, at(0), 0, buf),
                        sys_access(path, mode(general::F_OK)),
                        sys_faccessat(Dirfd::Cwd, path, mode(general::F_OK)),
                        sys_faccessat2(Dirfd::Cwd, path, mode(general::F_OK), at(0)),
                    ]
                };
                let every = |e: u32| [neg_errno(e); 7];
                kassert_eq!(all(file, buf), [0; 7]);
                kassert_eq!(all(missing, buf), every(errno::ENOENT));
                kassert_eq!(all(empty, buf), every(errno::ENOENT));
                kassert_eq!(all(through_file, buf), every(errno::ENOTDIR));
                kassert_eq!(all(file_slash, buf), every(errno::ENOTDIR));
                kassert_eq!(all(long, buf), every(errno::ENAMETOOLONG));
                kassert_eq!(all(reg(0), buf), every(errno::EFAULT));
                kassert_eq!(all(reg(0xffff_8000_0000_0000), buf), every(errno::EFAULT));
                // A loop: `-ELOOP` wherever the final link is followed;
                // `lstat` and `AT_SYMLINK_NOFOLLOW` stop at the link itself.
                let r = all(looped, buf);
                kassert_eq!([r[0], r[2], r[3], r[4], r[5], r[6]], [neg_errno(errno::ELOOP); 6]);
                kassert_eq!(r[1], 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LOOP_A, false)?);
                let nofollow = at(general::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_newfstatat(Dirfd::Cwd, looped, buf, nofollow), 0);
                kassert_eq!(sys_statx(Dirfd::Cwd, looped, nofollow, 0, buf), 0);
                kassert_eq!(sys_faccessat2(Dirfd::Cwd, looped, mode(general::F_OK), nofollow), 0);
                // Through a loop, a component: `-ELOOP` even for `lstat`.
                let via_loop = ktest_user_path(7, b"/tmp/stat-ktest-errno/a/x")?;
                kassert_eq!(sys_lstat(via_loop, buf), neg_errno(errno::ELOOP));

                // A bad buffer: `-EFAULT` from the stat calls.
                for bad in [reg(0), edge, reg(0xffff_8000_0000_0000)] {
                    kassert_eq!(sys_stat(file, bad), neg_errno(errno::EFAULT));
                    kassert_eq!(sys_lstat(file, bad), neg_errno(errno::EFAULT));
                    kassert_eq!(sys_newfstatat(Dirfd::Cwd, file, bad, at(0)), neg_errno(errno::EFAULT));
                    kassert_eq!(sys_statx(Dirfd::Cwd, file, at(0), 0, bad), neg_errno(errno::EFAULT));
                }
                // ... but a path's error comes first.
                kassert_eq!(sys_stat(missing, reg(0)), neg_errno(errno::ENOENT));

                // A bad dirfd with a relative path, or with `AT_EMPTY_PATH`:
                // `-EBADF`; with an absolute path it isn't looked at.
                for fd in [999u64, u64::MAX, 0x8000_0000] {
                    let bad = Dirfd::Fd(reg(fd));
                    kassert_eq!(sys_newfstatat(bad, rel, buf, at(0)), neg_errno(errno::EBADF));
                    kassert_eq!(sys_statx(bad, rel, at(0), 0, buf), neg_errno(errno::EBADF));
                    kassert_eq!(sys_faccessat(bad, rel, mode(general::F_OK)), neg_errno(errno::EBADF));
                    kassert_eq!(sys_faccessat2(bad, rel, mode(general::F_OK), at(0)), neg_errno(errno::EBADF));
                    let empty_path = at(general::AT_EMPTY_PATH);
                    kassert_eq!(sys_newfstatat(bad, empty, buf, empty_path), neg_errno(errno::EBADF));
                    kassert_eq!(sys_statx(bad, empty, empty_path, 0, buf), neg_errno(errno::EBADF));
                    kassert_eq!(sys_newfstatat(bad, file, buf, at(0)), 0);
                    kassert_eq!(sys_faccessat(bad, file, mode(general::F_OK)), 0);
                }
                // A relative path through a regular file's fd: `-ENOTDIR`.
                let file_fd = Dirfd::Fd(open_fd(FILE, OpenFlags::O_RDONLY)?);
                kassert_eq!(sys_newfstatat(file_fd, rel, buf, at(0)), neg_errno(errno::ENOTDIR));
                kassert_eq!(sys_statx(file_fd, rel, at(0), 0, buf), neg_errno(errno::ENOTDIR));
                kassert_eq!(sys_faccessat(file_fd, rel, mode(general::F_OK)), neg_errno(errno::ENOTDIR));

                // `AT_NO_AUTOMOUNT` and the sync bits, both of them too (only
                // `statx` refuses those together): accepted, ignored.
                for ignored in [
                    general::AT_NO_AUTOMOUNT,
                    general::AT_STATX_FORCE_SYNC,
                    general::AT_STATX_DONT_SYNC,
                    general::AT_STATX_SYNC_TYPE,
                ] {
                    scribble(BUF_A, 144)?;
                    kassert_eq!(sys_newfstatat(Dirfd::Cwd, file, buf, at(ignored)), 0);
                    kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                }
                // `newfstatat`'s other flags: `-EINVAL`, before the path.
                for bad in [general::AT_REMOVEDIR, general::AT_SYMLINK_FOLLOW, 0x1, 0x8000_0000] {
                    kassert_eq!(sys_newfstatat(Dirfd::Cwd, file, buf, at(bad)), neg_errno(errno::EINVAL));
                    kassert_eq!(sys_newfstatat(Dirfd::Cwd, missing, buf, at(bad)), neg_errno(errno::EINVAL));
                    kassert_eq!(sys_statx(Dirfd::Cwd, missing, at(bad), 0, buf), neg_errno(errno::EINVAL));
                }
                // Both sync types, or the reserved mask bit: `-EINVAL` before
                // the path too.
                let sync_both = at(general::AT_STATX_SYNC_TYPE);
                kassert_eq!(sys_statx(Dirfd::Cwd, missing, sync_both, 0, buf), neg_errno(errno::EINVAL));
                let reserved = general::STATX__RESERVED;
                kassert_eq!(sys_statx(Dirfd::Cwd, missing, at(0), reserved, buf), neg_errno(errno::EINVAL));
                Ok(())
            });
            clean(&[LOOP_A, LOOP_B, FILE], &[DIR]);
            result?
        }
    );

    crate::ktest!(
        fn stat_handlers_take_their_registers_in_order() {
            // Through `dispatch`, each argument in its own register with
            // distinct values (garbage above an `int`'s 32 bits too), so an
            // arm passing them out of order can't pass.
            use crate::arch::syscall::SyscallFrame;
            const DIR: &[u8] = b"/tmp/stat-ktest-regs";
            const FILE: &[u8] = b"/tmp/stat-ktest-regs/f";
            const LINK: &[u8] = b"/tmp/stat-ktest-regs/l";
            let tables = crate::vfs::tables();
            let result = process::in_test_process(move |_| {
                lumen_vfs::mkdir_at(tables, None, DIR, 0o755, 0).map_err(|_| "mkdir")?;
                create(FILE, 0o644)?;
                symlink_at(tables, b"f", None, LINK).map_err(|_| "symlink")?;
                let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64| {
                    let mut frame =
                        SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, r8, ..SyscallFrame::default() };
                    let _ = crate::linux::syscall::dispatch(&mut frame);
                    frame.rax
                };
                let high = 0xdead_beef_0000_0000u64;
                let dir_fd = open_fd(DIR, OpenFlags::O_RDONLY)?.raw() as u64;
                let link = ktest_user_path(0, LINK)?.addr();
                let rel = ktest_user_path(1, b"l")?.addr();
                let rel_file = ktest_user_path(2, b"f")?.addr();
                let nofollow = u64::from(general::AT_SYMLINK_NOFOLLOW);

                // stat(path, buf) and lstat(path, buf).
                kassert_eq!(call(general::__NR_stat, link, BUF_A, high, high, high), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(FILE, true)?);
                kassert_eq!(call(general::__NR_lstat, link, BUF_A, high, high, high), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LINK, false)?);
                // newfstatat(dirfd, path, buf, flags).
                scribble(BUF_A, 144)?;
                kassert_eq!(call(general::__NR_newfstatat, high | dir_fd, rel, BUF_A, high | nofollow, high), 0);
                kassert_eq!(stat_at(BUF_A)?, want_stat(LINK, false)?);
                // statx(dirfd, path, flags, mask, buf).
                let mask = high | u64::from(general::STATX_ALL);
                kassert_eq!(call(general::__NR_statx, high | dir_fd, rel, high | nofollow, mask, BUF_B), 0);
                kassert_eq!(statx_at(BUF_B)?, want_statx(LINK, false)?);
                // access(path, mode): `X_OK` on the link's target is refused.
                let x_ok = high | u64::from(general::X_OK);
                let r_ok = high | u64::from(general::R_OK);
                kassert_eq!(call(general::__NR_access, link, x_ok, high, high, high), neg_errno(errno::EACCES));
                kassert_eq!(call(general::__NR_access, link, r_ok, high, high, high), 0);
                // faccessat(dirfd, path, mode): `r10` is never read, so
                // garbage there (an invalid flag) changes nothing ...
                let bad_flags = u64::from(general::AT_EMPTY_PATH | 0x1);
                kassert_eq!(call(general::__NR_faccessat, high | dir_fd, rel_file, r_ok, bad_flags, high), 0);
                kassert_eq!(
                    call(general::__NR_faccessat, high | dir_fd, rel_file, x_ok, bad_flags, high),
                    neg_errno(errno::EACCES)
                );
                // ... where faccessat2(dirfd, path, mode, flags) reads it.
                kassert_eq!(
                    call(general::__NR_faccessat2, high | dir_fd, rel_file, r_ok, bad_flags, high),
                    neg_errno(errno::EINVAL)
                );
                kassert_eq!(call(general::__NR_faccessat2, high | dir_fd, rel, x_ok, high | nofollow, high), 0);
                kassert_eq!(
                    call(general::__NR_faccessat2, high | dir_fd, rel, x_ok, high, high),
                    neg_errno(errno::EACCES)
                );
                Ok(())
            });
            clean(&[LINK, FILE], &[DIR]);
            result?
        }
    );
}
