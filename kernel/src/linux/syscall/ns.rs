//! Namespace changes (M7): `mkdir`, `mkdirat`, `rmdir`, `unlink`,
//! `unlinkat`, `link`, `linkat`, `symlink`, `symlinkat`, `readlink`,
//! `readlinkat`, `rename`, `renameat`, `renameat2`.
//!
//! Each legacy call is its `*at` form from `Dirfd::Cwd`, and each `*at` form
//! reaches lumen-vfs only through `at::start_dir`'s `Start`. As Linux does,
//! a flags argument (and `readlinkat`'s `bufsiz`) is checked before any path
//! is copied in, so a bad flag is `-EINVAL` even with a bad pointer.
//!
//! The errnos lumen-vfs gives where Linux's differ or vary (ARCHITECTURE
//! section 8's table):
//! - `/` as the name to remove or rename is `-EBUSY` for `rmdir`, `rename`
//!   and `unlink` alike (Linux's `unlink("/")` is `-EISDIR`); `mkdir("/")`
//!   and `link(_, "/")` are `-EEXIST`.
//! - `linkat`'s `AT_EMPTY_PATH` links the descriptor's own file, as Linux
//!   does for a caller with `CAP_DAC_READ_SEARCH` (every process here runs
//!   as root); a file with no links left is `-ENOENT`.
//! - `readlinkat` of an empty path names the descriptor's file (or the
//!   cwd): one that isn't a symlink is `-ENOENT`, as Linux's
//!   `do_readlinkat` says for an empty name (`-EINVAL` for any other).
//! - A symlink target that isn't UTF-8 is `-EINVAL` (#268).

use super::at::{self, Dirfd, Mode, Start, UserPath, UserPtr};
use super::finish;
use crate::linux::errno;
use crate::user::{CopyError, copy_to_user};
use lumen_vfs::{AtFlags, RenameFlags, VfsError};

/// `mkdir(path, mode)` (83): `mkdirat(AT_FDCWD, path, mode)`.
pub(super) fn sys_mkdir(path: UserPtr, mode: Mode) -> u64 {
    finish(mkdirat(Dirfd::Cwd, path, mode))
}

/// `mkdirat(dirfd, path, mode)` (258).
pub(super) fn sys_mkdirat(dirfd: Dirfd, path: UserPtr, mode: Mode) -> u64 {
    finish(mkdirat(dirfd, path, mode))
}

/// `mode`'s directory bits (`Mode::dir_perm`: never setuid or setgid), less
/// the process's umask: `Start::mkdir` applies both.
fn mkdirat(dirfd: Dirfd, path: UserPtr, mode: Mode) -> Result<u64, u32> {
    let path = UserPath::copy_in(path, false)?;
    let start = at::start_dir(dirfd, &path)?;
    start.mkdir(&path, mode)?;
    Ok(0)
}

/// `rmdir(path)` (84): `unlinkat(AT_FDCWD, path, AT_REMOVEDIR)`.
pub(super) fn sys_rmdir(path: UserPtr) -> u64 {
    finish(unlinkat(Dirfd::Cwd, path, AtFlags(AtFlags::AT_REMOVEDIR)))
}

/// `unlink(path)` (87): `unlinkat(AT_FDCWD, path, 0)`.
pub(super) fn sys_unlink(path: UserPtr) -> u64 {
    finish(unlinkat(Dirfd::Cwd, path, AtFlags(0)))
}

/// `unlinkat(dirfd, path, flags)` (263).
pub(super) fn sys_unlinkat(dirfd: Dirfd, path: UserPtr, flags: AtFlags) -> u64 {
    finish(unlinkat(dirfd, path, flags))
}

/// `AT_REMOVEDIR` removes an empty directory, else a non-directory; any
/// other flag is `-EINVAL`.
fn unlinkat(dirfd: Dirfd, path: UserPtr, flags: AtFlags) -> Result<u64, u32> {
    let flags = flags.validate(AtFlags::AT_REMOVEDIR).map_err(VfsError::to_errno)?;
    let path = UserPath::copy_in(path, false)?;
    let start = at::start_dir(dirfd, &path)?;
    start.unlink(&path, flags.has_removedir())?;
    Ok(0)
}

/// `link(old, new)` (86): `linkat(AT_FDCWD, old, AT_FDCWD, new, 0)`.
pub(super) fn sys_link(old: UserPtr, new: UserPtr) -> u64 {
    finish(linkat(Dirfd::Cwd, old, Dirfd::Cwd, new, AtFlags(0)))
}

/// `linkat(olddirfd, old, newdirfd, new, flags)` (265).
pub(super) fn sys_linkat(olddirfd: Dirfd, old: UserPtr, newdirfd: Dirfd, new: UserPtr, flags: AtFlags) -> u64 {
    finish(linkat(olddirfd, old, newdirfd, new, flags))
}

/// `AT_SYMLINK_FOLLOW` links what a final symlink names, not the link;
/// `AT_EMPTY_PATH` lets `old` be empty, naming `olddirfd`'s file. `new` is
/// never empty.
fn linkat(olddirfd: Dirfd, old: UserPtr, newdirfd: Dirfd, new: UserPtr, flags: AtFlags) -> Result<u64, u32> {
    let flags = flags.validate(AtFlags::AT_SYMLINK_FOLLOW | AtFlags::AT_EMPTY_PATH).map_err(VfsError::to_errno)?;
    let old = UserPath::copy_in(old, flags.has_empty_path())?;
    let new = UserPath::copy_in(new, false)?;
    let old_start = at::start_dir(olddirfd, &old)?;
    let new_start = at::start_dir(newdirfd, &new)?;
    Start::link(&old_start, &old, &new_start, &new, flags.has_symlink_follow(), flags.has_empty_path())?;
    Ok(0)
}

/// `symlink(target, linkpath)` (88): `symlinkat(target, AT_FDCWD, linkpath)`.
pub(super) fn sys_symlink(target: UserPtr, linkpath: UserPtr) -> u64 {
    finish(symlinkat(target, Dirfd::Cwd, linkpath))
}

/// `symlinkat(target, newdirfd, linkpath)` (266).
pub(super) fn sys_symlinkat(target: UserPtr, newdirfd: Dirfd, linkpath: UserPtr) -> u64 {
    finish(symlinkat(target, newdirfd, linkpath))
}

/// `target` is stored as given (never resolved; empty is `-ENOENT`).
fn symlinkat(target: UserPtr, newdirfd: Dirfd, linkpath: UserPtr) -> Result<u64, u32> {
    let target = UserPath::copy_in(target, false)?;
    let linkpath = UserPath::copy_in(linkpath, false)?;
    let start = at::start_dir(newdirfd, &linkpath)?;
    start.symlink(target.as_bytes(), &linkpath)?;
    Ok(0)
}

/// `readlink(path, buf, bufsiz)` (89): `readlinkat(AT_FDCWD, path, buf, bufsiz)`.
pub(super) fn sys_readlink(path: UserPtr, buf: UserPtr, bufsiz: i32) -> u64 {
    finish(readlinkat(Dirfd::Cwd, path, buf, bufsiz))
}

/// `readlinkat(dirfd, path, buf, bufsiz)` (267).
pub(super) fn sys_readlinkat(dirfd: Dirfd, path: UserPtr, buf: UserPtr, bufsiz: i32) -> u64 {
    finish(readlinkat(dirfd, path, buf, bufsiz))
}

/// The final symlink's target, never followed, cut to `bufsiz` bytes with
/// no NUL added; returns how many were copied. `bufsiz <= 0` is `-EINVAL`
/// before the path is looked at. An empty path names `dirfd`'s file (an
/// `O_PATH | O_NOFOLLOW` symlink), as Linux always allows here; not a
/// symlink is then `-ENOENT`, not `-EINVAL`. The target is a copy, so no
/// VFS lock is held across the copy out.
fn readlinkat(dirfd: Dirfd, path: UserPtr, buf: UserPtr, bufsiz: i32) -> Result<u64, u32> {
    if bufsiz <= 0 {
        return Err(errno::EINVAL);
    }
    let path = UserPath::copy_in(path, true)?;
    let start = at::start_dir(dirfd, &path)?;
    let target = start.readlink(&path).map_err(|e| match e {
        errno::EINVAL if path.is_empty() => errno::ENOENT,
        e => e,
    })?;
    let n = target.len().min(bufsiz as usize);
    copy_to_user(buf.addr(), &target[..n]).map_err(CopyError::to_errno)?;
    Ok(n as u64)
}

/// `rename(old, new)` (82): `renameat2(AT_FDCWD, old, AT_FDCWD, new, 0)`.
pub(super) fn sys_rename(old: UserPtr, new: UserPtr) -> u64 {
    finish(renameat2(Dirfd::Cwd, old, Dirfd::Cwd, new, RenameFlags(0)))
}

/// `renameat(olddirfd, old, newdirfd, new)` (264): `renameat2(.., 0)`.
pub(super) fn sys_renameat(olddirfd: Dirfd, old: UserPtr, newdirfd: Dirfd, new: UserPtr) -> u64 {
    finish(renameat2(olddirfd, old, newdirfd, new, RenameFlags(0)))
}

/// `renameat2(olddirfd, old, newdirfd, new, flags)` (316).
pub(super) fn sys_renameat2(olddirfd: Dirfd, old: UserPtr, newdirfd: Dirfd, new: UserPtr, flags: RenameFlags) -> u64 {
    finish(renameat2(olddirfd, old, newdirfd, new, flags))
}

/// `RENAME_NOREPLACE` or `RENAME_EXCHANGE`; `RENAME_WHITEOUT`, any other
/// bit, or both together are `-EINVAL`, before the paths are looked at.
fn renameat2(olddirfd: Dirfd, old: UserPtr, newdirfd: Dirfd, new: UserPtr, flags: RenameFlags) -> Result<u64, u32> {
    let flags = flags.validate().map_err(VfsError::to_errno)?;
    let old = UserPath::copy_in(old, false)?;
    let new = UserPath::copy_in(new, false)?;
    let old_start = at::start_dir(olddirfd, &old)?;
    let new_start = at::start_dir(newdirfd, &new)?;
    Start::rename(&old_start, &old, &new_start, &new, flags)?;
    Ok(0)
}

/// The ktests' helpers: paths and buffers in the test process's user
/// memory, files made and looked at through lumen-vfs directly (so a check
/// never leans on the handler it checks), and each suite run on a fresh
/// directory in `/tmp` (tmpfs) and on an ext4 volume mounted under it.
#[cfg(feature = "ktest")]
mod kt {
    use alloc::format;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use lumen_vfs::{FileSystem, InodeType, OpenFlags, OpenGuard, ResolveFlags, VfsError};

    use super::super::at::{self, Fd, Mode, UserPtr};
    use super::super::fd;
    use crate::linux::process;
    use crate::user::{copy_from_user, copy_to_user};
    use crate::vfs::tables;

    pub(super) type R = crate::ktest::TestResult;

    /// A user buffer clear of `fd::ktest_user_path`'s path slots (16 of 256
    /// bytes in the first page).
    pub(super) const BUF: u64 = process::KTEST_USER_VA + 8192;
    /// An address no user copy may touch: `-EFAULT`.
    pub(super) const BAD: u64 = 0xffff_8000_0000_0000;

    /// `path` and a NUL in user memory, in path slot `slot`.
    pub(super) fn up(slot: u64, path: &str) -> Result<UserPtr, &'static str> {
        fd::ktest_user_path(slot, path.as_bytes())
    }

    pub(super) fn ptr(addr: u64) -> UserPtr {
        at::reg(addr)
    }

    pub(super) fn mode(bits: u64) -> Mode {
        at::reg(bits)
    }

    pub(super) fn e(code: u32) -> u64 {
        super::super::neg_errno(code)
    }

    /// Create (or truncate) the file at `path` holding `data`.
    pub(super) fn file(path: &str, data: &[u8]) -> R {
        let flags = OpenFlags::from_bits(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
        let (inode, _) =
            lumen_vfs::open_inode(tables(), None, path.as_bytes(), flags, 0o644, 0).map_err(|_| "create a file")?;
        crate::kassert_eq!(inode.write_at(0, data), Ok(data.len()));
        Ok(())
    }

    pub(super) fn mkdir(path: &str) -> R {
        lumen_vfs::mkdir_at(tables(), None, path.as_bytes(), 0o755, 0).map(drop).map_err(|_| "mkdir")
    }

    pub(super) fn symlink(target: &str, path: &str) -> R {
        lumen_vfs::symlink_at(tables(), target.as_bytes(), None, path.as_bytes()).map(drop).map_err(|_| "symlink")
    }

    /// The first 64 bytes of the file at `path`, symlinks followed.
    pub(super) fn read(path: &str) -> Result<Vec<u8>, VfsError> {
        let inode = crate::vfs::resolve_path_from_root(path)?;
        let mut buf = [0u8; 64];
        let n = inode.read_at(0, &mut buf)?;
        Ok(buf[..n].to_vec())
    }

    /// `path`'s (type, mode, nlink, ino), a final symlink not followed.
    pub(super) fn lstat(path: &str) -> Result<(InodeType, u16, u32, u64), VfsError> {
        let flags = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
        let m = lumen_vfs::resolve_path(tables(), None, path.as_bytes(), flags)?.inode.stat()?;
        Ok((m.inode_type, m.mode, m.nlink, m.ino))
    }

    /// `open(path, flags)` through the syscall, in path slot `slot`.
    pub(super) fn open(slot: u64, path: &str, flags: u32) -> Result<Fd, &'static str> {
        let ret = fd::sys_open(up(slot, path)?, OpenFlags::from_bits(flags), mode(0));
        if ret < lumen_vfs::MAX_FDS as u64 { Ok(at::reg(ret)) } else { Err("an open failed") }
    }

    /// What `read(fd)` gives, at most 64 bytes.
    pub(super) fn fd_read(fd: Fd) -> Result<Vec<u8>, &'static str> {
        let at = BUF + 1024;
        let n = fd::sys_read(fd, ptr(at), 64);
        if n > 64 {
            return Err("a read failed");
        }
        user(at, n as usize)
    }

    /// `n` bytes of user memory at `at`.
    pub(super) fn user(at: u64, n: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = alloc::vec![0u8; n];
        copy_from_user(&mut out, at).map_err(|_| "copy from user memory")?;
        Ok(out)
    }

    /// Fill the first 64 bytes of `BUF` with `0xaa`, to see what a copy out
    /// left alone.
    pub(super) fn smudge() -> R {
        copy_to_user(BUF, &[0xaa; 64]).map_err(|_| "fill the buffer")
    }

    /// Move the test process's working directory to `path`.
    pub(super) fn set_cwd(path: &str) -> R {
        let dir = crate::vfs::resolve_path_from_root(path).map_err(|_| "resolve the new cwd")?;
        let guard = OpenGuard::try_new(dir).map_err(|_| "guard the new cwd")?;
        process::set_current_cwd(process::Cwd::new(guard, path).map_err(|_| "the cwd")?);
        Ok(())
    }

    /// Remove each path in turn (directories empty by then).
    pub(super) fn remove(paths: &[&str]) -> R {
        for path in paths {
            let dir = matches!(lstat(path), Ok((InodeType::Dir, ..)));
            if lumen_vfs::unlink_at(tables(), None, path.as_bytes(), dir).is_err() {
                crate::ktest::diag(format_args!("removing {path}"));
                return Err("clean up");
            }
        }
        Ok(())
    }

    fn close_all_but_stdio() {
        for fd in 3..64u64 {
            let _ = fd::sys_close(at::reg(fd));
        }
    }

    /// Run `body` on a fresh directory in `/tmp` (tmpfs), which it must
    /// leave empty, then on the root of a fresh ext4 volume mounted at
    /// `/tmp/ns-ktest-ext4-<name>` (block device minor `minor`), which
    /// `rmdir` can't remove while mounted (`-EBUSY`). Every fd it left open
    /// is closed after each.
    pub(super) fn on_tmpfs_and_ext4(name: &str, minor: u32, body: fn(&str) -> R) -> R {
        let tmp = format!("/tmp/ns-ktest-{name}");
        mkdir(&tmp)?;
        let ran = body(&tmp);
        close_all_but_stdio();
        if ran.is_err() {
            crate::ktest::diag(format_args!("on tmpfs, in {tmp}"));
        }
        ran?;
        lumen_vfs::unlink_at(tables(), None, tmp.as_bytes(), true).map_err(|_| "the tmpfs run left files behind")?;

        let fs = crate::vfs::ktest_ext4(minor)?;
        let mnt = format!("/tmp/ns-ktest-ext4-{name}");
        let mp = lumen_vfs::mkdir_at(tables(), None, mnt.as_bytes(), 0o755, 0).map_err(|_| "mkdir the mountpoint")?;
        let (mp_dev, mp_ino) = (mp.dev_id(), mp.ino());
        if tables().mount(mp, &mnt, fs as Arc<dyn FileSystem>).is_err() {
            let _ = lumen_vfs::unlink_at(tables(), None, mnt.as_bytes(), true);
            return Err("mount the ext4 volume");
        }
        let ran = body(&mnt);
        close_all_but_stdio();
        if ran.is_err() {
            crate::ktest::diag(format_args!("on ext4, in {mnt}"));
        }
        let busy = up(15, &mnt).map(super::sys_rmdir);
        let umounted = tables().umount(mp_dev, mp_ino).map(drop).map_err(|_| "umount");
        let removed = lumen_vfs::unlink_at(tables(), None, mnt.as_bytes(), true).map_err(|_| "rmdir the mountpoint");
        ran?;
        crate::kassert_eq!(busy, Ok(e(crate::linux::errno::EBUSY)));
        umounted?;
        removed
    }
}

#[cfg(feature = "ktest")]
fn ktest_mkdir_and_rmdir(base: &str) -> kt::R {
    use super::at::reg;
    use crate::linux::{errno, process};
    use crate::{kassert, kassert_eq};
    use kt::{BAD, e, lstat, mode, open, ptr, set_cwd, up};
    use lumen_vfs::{InodeType, OpenFlags as O, VfsError};
    let p = |s: &str| alloc::format!("{base}/{s}");
    let rmdir_flag = AtFlags(AtFlags::AT_REMOVEDIR);

    // The default umask (0o022) comes off the mode.
    let d = p("d");
    kassert_eq!(sys_mkdir(up(0, &d)?, mode(0o777)), 0);
    kassert_eq!(lstat(&d).map(|s| (s.0, s.1)), Ok((InodeType::Dir, 0o755)));
    // Another umask applies as it is; setuid and setgid go, sticky stays.
    let umask = process::with_current_mut(|p| core::mem::replace(&mut p.umask, 0o027));
    let made = sys_mkdir(up(1, &p("d/s"))?, mode(0o7777));
    process::with_current_mut(|p| p.umask = umask);
    kassert_eq!(made, 0);
    kassert_eq!(lstat(&p("d/s")).map(|s| s.1), Ok(0o1750));
    // From a dirfd (an `O_PATH` one); an absolute path ignores even a bad one.
    let dfd = open(2, &d, O::O_PATH | O::O_DIRECTORY)?;
    kassert_eq!(sys_mkdirat(Dirfd::Fd(dfd), up(3, "r")?, mode(0o700)), 0);
    kassert_eq!(lstat(&p("d/r")).map(|s| s.1), Ok(0o700));
    kassert_eq!(sys_mkdirat(Dirfd::Fd(reg(99)), up(4, &p("d/abs"))?, mode(0o755)), 0);
    // From the cwd, by `mkdir` and `mkdirat(AT_FDCWD)`.
    set_cwd(&d)?;
    let from_cwd = (sys_mkdir(up(5, "c1")?, mode(0o755)), sys_mkdirat(Dirfd::Cwd, up(6, "c2")?, mode(0o755)));
    set_cwd("/")?;
    kassert_eq!(from_cwd, (0, 0));
    kassert!(lstat(&p("d/c1")).is_ok() && lstat(&p("d/c2")).is_ok());
    // EEXIST: a directory, a file, `/`.
    kt::file(&p("f"), b"x")?;
    kassert_eq!(sys_mkdir(up(0, &d)?, mode(0o755)), e(errno::EEXIST));
    kassert_eq!(sys_mkdir(up(0, &p("f"))?, mode(0o755)), e(errno::EEXIST));
    kassert_eq!(sys_mkdir(up(0, "/")?, mode(0o755)), e(errno::EEXIST));
    // ENOENT: a missing parent, an empty path.
    kassert_eq!(sys_mkdir(up(0, &p("none/x"))?, mode(0o755)), e(errno::ENOENT));
    kassert_eq!(sys_mkdir(up(0, "")?, mode(0o755)), e(errno::ENOENT));
    // ENOTDIR: a file as the parent, or as the dirfd.
    kassert_eq!(sys_mkdir(up(0, &p("f/x"))?, mode(0o755)), e(errno::ENOTDIR));
    let ffd = open(7, &p("f"), O::O_RDONLY)?;
    kassert_eq!(sys_mkdirat(Dirfd::Fd(ffd), up(0, "x")?, mode(0o755)), e(errno::ENOTDIR));
    // EBADF: a relative path from a closed dirfd. EFAULT: bad pointers.
    kassert_eq!(sys_mkdirat(Dirfd::Fd(reg(99)), up(0, "x")?, mode(0o755)), e(errno::EBADF));
    kassert_eq!(sys_mkdir(ptr(BAD), mode(0o755)), e(errno::EFAULT));
    kassert_eq!(sys_mkdir(ptr(0), mode(0o755)), e(errno::EFAULT));
    // ELOOP: through a symlink to itself.
    kt::symlink("loop", &p("loop"))?;
    kassert_eq!(sys_mkdir(up(0, &p("loop/x"))?, mode(0o755)), e(errno::ELOOP));

    // `rmdir`: ENOTEMPTY (and for `..`), ENOTDIR on a file, EINVAL for `.`,
    // EBUSY for `/`, ENOENT, EFAULT, ELOOP.
    kassert_eq!(sys_rmdir(up(0, &d)?), e(errno::ENOTEMPTY));
    kassert_eq!(sys_rmdir(up(0, &p("d/s/.."))?), e(errno::ENOTEMPTY));
    kassert_eq!(sys_rmdir(up(0, &p("f"))?), e(errno::ENOTDIR));
    kassert_eq!(sys_rmdir(up(0, &p("d/s/."))?), e(errno::EINVAL));
    kassert_eq!(sys_unlinkat(Dirfd::Fd(dfd), up(0, ".")?, rmdir_flag), e(errno::EINVAL));
    kassert_eq!(sys_rmdir(up(0, "/")?), e(errno::EBUSY));
    kassert_eq!(sys_rmdir(up(0, &p("none"))?), e(errno::ENOENT));
    kassert_eq!(sys_rmdir(ptr(BAD)), e(errno::EFAULT));
    kassert_eq!(sys_rmdir(up(0, &p("loop/x"))?), e(errno::ELOOP));
    // The empty ones go: by path, from a dirfd, from the cwd.
    kassert_eq!(sys_rmdir(up(0, &p("d/s"))?), 0);
    kassert_eq!(lstat(&p("d/s")).map(drop), Err(VfsError::NoEnt));
    kassert_eq!(sys_unlinkat(Dirfd::Fd(dfd), up(0, "r")?, rmdir_flag), 0);
    set_cwd(&d)?;
    let from_cwd = (sys_rmdir(up(0, "c1")?), sys_unlinkat(Dirfd::Cwd, up(1, "c2")?, rmdir_flag));
    set_cwd("/")?;
    kassert_eq!(from_cwd, (0, 0));
    kassert_eq!(sys_rmdir(up(0, &p("d/abs"))?), 0);
    kassert_eq!(super::fd::sys_close(dfd), 0);
    kassert_eq!(super::fd::sys_close(ffd), 0);
    kassert_eq!(sys_rmdir(up(0, &d)?), 0);
    kassert_eq!(lstat(&d).map(drop), Err(VfsError::NoEnt));
    kt::remove(&[&p("f"), &p("loop")])
}

crate::ktest!(
    fn ns_mkdir_rmdir_with_the_umask_dirfds_and_errnos() {
        crate::linux::process::in_test_process(|_| kt::on_tmpfs_and_ext4("mkdir", 150, ktest_mkdir_and_rmdir))?
    }
);

#[cfg(feature = "ktest")]
fn ktest_unlink(base: &str) -> kt::R {
    use super::at::reg;
    use crate::linux::errno;
    use crate::{kassert, kassert_eq};
    use kt::{BAD, e, lstat, open, ptr, read, up};
    use lumen_vfs::{OpenFlags as O, VfsError};
    let p = |s: &str| alloc::format!("{base}/{s}");

    // Unlinked while open: the name goes, the open file reads on.
    kt::file(&p("f"), b"data")?;
    let fd = open(0, &p("f"), O::O_RDONLY)?;
    kassert_eq!(sys_unlink(up(1, &p("f"))?), 0);
    kassert_eq!(lstat(&p("f")).map(drop), Err(VfsError::NoEnt));
    kassert_eq!(kt::fd_read(fd)?, b"data".to_vec());
    kassert_eq!(sys_unlink(up(1, &p("f"))?), e(errno::ENOENT));
    // From a dirfd.
    kt::mkdir(&p("d"))?;
    kt::file(&p("d/g"), b"g")?;
    let dfd = open(2, &p("d"), O::O_RDONLY | O::O_DIRECTORY)?;
    kassert_eq!(sys_unlinkat(Dirfd::Fd(dfd), up(3, "g")?, AtFlags(0)), 0);
    kassert_eq!(lstat(&p("d/g")).map(drop), Err(VfsError::NoEnt));
    // A symlink goes, not what it names.
    kt::file(&p("t"), b"t")?;
    kt::symlink("t", &p("s"))?;
    kassert_eq!(sys_unlink(up(1, &p("s"))?), 0);
    kassert_eq!(lstat(&p("s")).map(drop), Err(VfsError::NoEnt));
    kassert_eq!(read(&p("t")), Ok(b"t".to_vec()));
    // EISDIR: a directory without AT_REMOVEDIR. ENOTDIR: a file with it, a
    // file with a trailing slash, a file as a parent.
    kassert_eq!(sys_unlink(up(1, &p("d"))?), e(errno::EISDIR));
    kassert_eq!(sys_unlinkat(Dirfd::Cwd, up(1, &p("d"))?, AtFlags(0)), e(errno::EISDIR));
    kassert_eq!(sys_unlinkat(Dirfd::Cwd, up(1, &p("t"))?, AtFlags(AtFlags::AT_REMOVEDIR)), e(errno::ENOTDIR));
    kassert_eq!(sys_unlink(up(1, &p("t/"))?), e(errno::ENOTDIR));
    kassert_eq!(sys_unlink(up(1, &p("t/x"))?), e(errno::ENOTDIR));
    // EINVAL: any flag but AT_REMOVEDIR, before the path is read.
    for bad in [AtFlags::AT_SYMLINK_NOFOLLOW, AtFlags::AT_EMPTY_PATH, AtFlags::AT_REMOVEDIR | 1, 0x8000_0000] {
        kassert_eq!(sys_unlinkat(Dirfd::Cwd, up(1, &p("t"))?, AtFlags(bad)), e(errno::EINVAL));
        kassert_eq!(sys_unlinkat(Dirfd::Cwd, ptr(0), AtFlags(bad)), e(errno::EINVAL));
    }
    kassert!(lstat(&p("t")).is_ok());
    // `/` is EBUSY (Linux says EISDIR); EBADF, EFAULT, ENOENT for an empty
    // path, ELOOP through a looping symlink (which itself goes).
    kassert_eq!(sys_unlink(up(1, "/")?), e(errno::EBUSY));
    kassert_eq!(sys_unlinkat(Dirfd::Fd(reg(99)), up(1, "t")?, AtFlags(0)), e(errno::EBADF));
    kassert_eq!(sys_unlink(ptr(BAD)), e(errno::EFAULT));
    kassert_eq!(sys_unlink(up(1, "")?), e(errno::ENOENT));
    kt::symlink("loop", &p("loop"))?;
    kassert_eq!(sys_unlink(up(1, &p("loop/x"))?), e(errno::ELOOP));
    kassert_eq!(sys_unlink(up(1, &p("loop"))?), 0);
    kassert_eq!(super::fd::sys_close(fd), 0);
    kassert_eq!(super::fd::sys_close(dfd), 0);
    kassert_eq!(sys_unlink(up(1, &p("t"))?), 0);
    kassert_eq!(sys_rmdir(up(1, &p("d"))?), 0);
    Ok(())
}

crate::ktest!(
    fn ns_unlink_unlinkat_and_their_errnos() {
        crate::linux::process::in_test_process(|_| kt::on_tmpfs_and_ext4("unlink", 151, ktest_unlink))?
    }
);

#[cfg(feature = "ktest")]
fn ktest_link(base: &str) -> kt::R {
    use super::at::reg;
    use crate::linux::errno;
    use crate::{kassert, kassert_eq};
    use kt::{BAD, e, lstat, open, ptr, read, up};
    use lumen_vfs::{InodeType, OpenFlags as O};
    let p = |s: &str| alloc::format!("{base}/{s}");
    let cwd = Dirfd::Cwd;

    // A second name for the same inode, the link count up.
    kt::file(&p("f"), b"data")?;
    let ino = lstat(&p("f")).map_err(|_| "stat f")?.3;
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, &p("h"))?), 0);
    kassert_eq!(lstat(&p("h")).map(|s| (s.0, s.3, s.2)), Ok((InodeType::File, ino, 2)));
    kassert_eq!(read(&p("h")), Ok(b"data".to_vec()));
    // From dirfds.
    let dfd = open(2, base, O::O_PATH | O::O_DIRECTORY)?;
    kassert_eq!(sys_linkat(Dirfd::Fd(dfd), up(3, "f")?, Dirfd::Fd(dfd), up(4, "i")?, AtFlags(0)), 0);
    kassert_eq!(lstat(&p("f")).map(|s| s.2), Ok(3));
    // A final symlink: linked itself, or with AT_SYMLINK_FOLLOW what it names.
    kt::symlink("f", &p("s"))?;
    kassert_eq!(sys_linkat(cwd, up(0, &p("s"))?, cwd, up(1, &p("s-link"))?, AtFlags(0)), 0);
    kassert_eq!(lstat(&p("s-link")).map(|s| s.0), Ok(InodeType::Symlink));
    let follow = AtFlags(AtFlags::AT_SYMLINK_FOLLOW);
    kassert_eq!(sys_linkat(cwd, up(0, &p("s"))?, cwd, up(1, &p("s-follow"))?, follow), 0);
    kassert_eq!(lstat(&p("s-follow")).map(|s| (s.0, s.3)), Ok((InodeType::File, ino)));
    // AT_EMPTY_PATH: the dirfd's own file (as for a caller with
    // CAP_DAC_READ_SEARCH). Without it an empty path is ENOENT, and the new
    // path is never empty.
    let empty = AtFlags(AtFlags::AT_EMPTY_PATH);
    let ffd = open(5, &p("f"), O::O_RDONLY)?;
    kassert_eq!(sys_linkat(Dirfd::Fd(ffd), up(0, "")?, cwd, up(1, &p("e"))?, empty), 0);
    kassert_eq!(lstat(&p("e")).map(|s| s.3), Ok(ino));
    kassert_eq!(sys_linkat(Dirfd::Fd(ffd), up(0, "")?, cwd, up(1, &p("e2"))?, AtFlags(0)), e(errno::ENOENT));
    kassert_eq!(sys_linkat(Dirfd::Fd(ffd), up(0, "")?, cwd, up(1, "")?, empty), e(errno::ENOENT));
    // ... and a file with no names left can't get one back: ENOENT.
    kt::file(&p("gone"), b"g")?;
    let gfd = open(6, &p("gone"), O::O_RDONLY)?;
    kt::remove(&[&p("gone")])?;
    kassert_eq!(sys_linkat(Dirfd::Fd(gfd), up(0, "")?, cwd, up(1, &p("back"))?, empty), e(errno::ENOENT));
    // EPERM: a directory, by path or as the dirfd's file.
    kt::mkdir(&p("d"))?;
    kassert_eq!(sys_link(up(0, &p("d"))?, up(1, &p("dl"))?), e(errno::EPERM));
    kassert_eq!(sys_linkat(Dirfd::Fd(dfd), up(0, "")?, cwd, up(1, &p("dl"))?, empty), e(errno::EPERM));
    // EEXIST: a file, a directory, `/`.
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, &p("h"))?), e(errno::EEXIST));
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, &p("d"))?), e(errno::EEXIST));
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, "/")?), e(errno::EEXIST));
    // EXDEV: onto the root filesystem.
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, "/ns-ktest-xdev")?), e(errno::EXDEV));
    // ENOENT, ENOTDIR.
    kassert_eq!(sys_link(up(0, &p("none"))?, up(1, &p("x"))?), e(errno::ENOENT));
    kassert_eq!(sys_link(up(0, &p("f"))?, up(1, &p("none/x"))?), e(errno::ENOENT));
    kassert_eq!(sys_link(up(0, &p("f/x"))?, up(1, &p("x"))?), e(errno::ENOTDIR));
    // EINVAL: a flag linkat doesn't take, before the paths are read.
    for bad in [AtFlags::AT_SYMLINK_NOFOLLOW, AtFlags::AT_REMOVEDIR, 0x8000_0000] {
        kassert_eq!(sys_linkat(cwd, up(0, &p("f"))?, cwd, up(1, &p("x"))?, AtFlags(bad)), e(errno::EINVAL));
        kassert_eq!(sys_linkat(cwd, ptr(0), cwd, ptr(0), AtFlags(bad)), e(errno::EINVAL));
    }
    // EFAULT, EBADF (either dirfd), ELOOP.
    kassert_eq!(sys_link(ptr(BAD), up(1, &p("x"))?), e(errno::EFAULT));
    kassert_eq!(sys_link(up(0, &p("f"))?, ptr(BAD)), e(errno::EFAULT));
    kassert_eq!(sys_linkat(Dirfd::Fd(reg(99)), up(0, "f")?, cwd, up(1, &p("x"))?, AtFlags(0)), e(errno::EBADF));
    kassert_eq!(sys_linkat(cwd, up(0, &p("f"))?, Dirfd::Fd(reg(99)), up(1, "x")?, AtFlags(0)), e(errno::EBADF));
    kt::symlink("loop", &p("loop"))?;
    kassert_eq!(sys_linkat(cwd, up(0, &p("loop"))?, cwd, up(1, &p("x"))?, follow), e(errno::ELOOP));
    kassert_eq!(sys_link(up(0, &p("loop/x"))?, up(1, &p("x"))?), e(errno::ELOOP));
    kassert!(lstat(&p("x")).is_err() && lstat(&p("dl")).is_err() && lstat(&p("e2")).is_err());
    // Each name removed takes one off the count.
    kassert_eq!(lstat(&p("f")).map(|s| s.2), Ok(5));
    kt::remove(&[&p("f"), &p("h"), &p("i"), &p("s-follow")])?;
    kassert_eq!(lstat(&p("e")).map(|s| (s.3, s.2)), Ok((ino, 1)));
    kassert_eq!(read(&p("e")), Ok(b"data".to_vec()));
    kt::remove(&[&p("e"), &p("s"), &p("s-link"), &p("d"), &p("loop")])
}

crate::ktest!(
    fn ns_link_linkat_follow_empty_path_and_errnos() {
        crate::linux::process::in_test_process(|_| kt::on_tmpfs_and_ext4("link", 152, ktest_link))?
    }
);

#[cfg(feature = "ktest")]
fn ktest_symlink_and_readlink(base: &str) -> kt::R {
    use super::at::reg;
    use crate::linux::errno;
    use crate::{kassert, kassert_eq};
    use kt::{BAD, BUF, e, lstat, open, ptr, smudge, up, user};
    use lumen_vfs::{InodeType, OpenFlags as O};
    let p = |s: &str| alloc::format!("{base}/{s}");
    let cwd = Dirfd::Cwd;

    // A target stored as given (dangling here), read back whole with no NUL.
    kassert_eq!(sys_symlink(up(0, "abcdef")?, up(1, &p("s"))?), 0);
    kassert_eq!(lstat(&p("s")).map(|s| s.0), Ok(InodeType::Symlink));
    smudge()?;
    kassert_eq!(sys_readlink(up(1, &p("s"))?, ptr(BUF), 64), 6);
    kassert_eq!(user(BUF, 8)?, b"abcdef\xaa\xaa".to_vec());
    // Cut to `bufsiz`, the rest of the buffer untouched.
    smudge()?;
    kassert_eq!(sys_readlink(up(1, &p("s"))?, ptr(BUF), 3), 3);
    kassert_eq!(user(BUF, 4)?, b"abc\xaa".to_vec());
    kassert_eq!(sys_readlink(up(1, &p("s"))?, ptr(BUF), 6), 6);
    // `bufsiz <= 0`: EINVAL, before the path is read.
    for bad in [0, -1, i32::MIN] {
        kassert_eq!(sys_readlink(up(1, &p("s"))?, ptr(BUF), bad), e(errno::EINVAL));
        kassert_eq!(sys_readlinkat(cwd, ptr(0), ptr(BUF), bad), e(errno::EINVAL));
    }
    // From a dirfd, both ways.
    kt::mkdir(&p("d"))?;
    let dfd = open(2, &p("d"), O::O_PATH | O::O_DIRECTORY)?;
    kassert_eq!(sys_symlinkat(up(3, "../t")?, Dirfd::Fd(dfd), up(4, "rel")?), 0);
    kassert_eq!(lstat(&p("d/rel")).map(|s| s.0), Ok(InodeType::Symlink));
    kassert_eq!(sys_readlinkat(Dirfd::Fd(dfd), up(4, "rel")?, ptr(BUF), 64), 4);
    kassert_eq!(user(BUF, 4)?, b"../t".to_vec());
    // An empty path names the dirfd's own file: an `O_PATH | O_NOFOLLOW`
    // symlink reads; anything else (a directory dirfd, a file's, the cwd)
    // is ENOENT, as on Linux.
    let sfd = open(5, &p("s"), O::O_PATH | O::O_NOFOLLOW)?;
    smudge()?;
    kassert_eq!(sys_readlinkat(Dirfd::Fd(sfd), up(0, "")?, ptr(BUF), 64), 6);
    kassert_eq!(user(BUF, 6)?, b"abcdef".to_vec());
    kassert_eq!(sys_readlinkat(Dirfd::Fd(dfd), up(0, "")?, ptr(BUF), 64), e(errno::ENOENT));
    kassert_eq!(sys_readlink(up(0, "")?, ptr(BUF), 64), e(errno::ENOENT));
    // Not a symlink: EINVAL. A symlink to a symlink: the final one is read.
    kt::file(&p("f"), b"x")?;
    let ffd = open(6, &p("f"), O::O_RDONLY)?;
    kassert_eq!(sys_readlinkat(Dirfd::Fd(ffd), up(0, "")?, ptr(BUF), 64), e(errno::ENOENT));
    kassert_eq!(sys_readlink(up(0, &p("f"))?, ptr(BUF), 64), e(errno::EINVAL));
    kassert_eq!(sys_readlink(up(0, &p("d"))?, ptr(BUF), 64), e(errno::EINVAL));
    kt::symlink("s", &p("s2"))?;
    kassert_eq!(sys_readlink(up(0, &p("s2"))?, ptr(BUF), 64), 1);
    // ENOENT, ENOTDIR, EBADF, EFAULT (the path, the buffer).
    kassert_eq!(sys_readlink(up(0, &p("none"))?, ptr(BUF), 64), e(errno::ENOENT));
    kassert_eq!(sys_readlink(up(0, &p("f/x"))?, ptr(BUF), 64), e(errno::ENOTDIR));
    kassert_eq!(sys_readlinkat(Dirfd::Fd(reg(99)), up(0, "s")?, ptr(BUF), 64), e(errno::EBADF));
    kassert_eq!(sys_readlink(ptr(BAD), ptr(BUF), 64), e(errno::EFAULT));
    kassert_eq!(sys_readlink(up(0, &p("s"))?, ptr(BAD), 64), e(errno::EFAULT));
    // `symlink`: ENOENT for an empty target or a missing parent; EEXIST
    // (over a symlink, a file); EFAULT (either pointer); EBADF.
    kassert_eq!(sys_symlink(up(0, "")?, up(1, &p("x"))?), e(errno::ENOENT));
    kassert_eq!(sys_symlink(up(0, "t")?, up(1, &p("none/x"))?), e(errno::ENOENT));
    kassert_eq!(sys_symlink(up(0, "t")?, up(1, &p("s"))?), e(errno::EEXIST));
    kassert_eq!(sys_symlink(up(0, "t")?, up(1, &p("f"))?), e(errno::EEXIST));
    kassert_eq!(sys_symlink(ptr(BAD), up(1, &p("x"))?), e(errno::EFAULT));
    kassert_eq!(sys_symlink(up(0, "t")?, ptr(BAD)), e(errno::EFAULT));
    kassert_eq!(sys_symlinkat(up(0, "t")?, Dirfd::Fd(reg(99)), up(1, "x")?), e(errno::EBADF));
    // ELOOP: a looping symlink reads as itself, but nothing resolves through it.
    kt::symlink("loop", &p("loop"))?;
    kassert_eq!(sys_readlink(up(0, &p("loop"))?, ptr(BUF), 64), 4);
    kassert_eq!(sys_readlink(up(0, &p("loop/x"))?, ptr(BUF), 64), e(errno::ELOOP));
    kassert_eq!(sys_symlink(up(0, "t")?, up(1, &p("loop/x"))?), e(errno::ELOOP));
    kassert!(lstat(&p("x")).is_err());
    kassert_eq!(super::fd::sys_close(dfd), 0);
    kassert_eq!(super::fd::sys_close(sfd), 0);
    kassert_eq!(super::fd::sys_close(ffd), 0);
    kt::remove(&[&p("s"), &p("s2"), &p("d/rel"), &p("d"), &p("f"), &p("loop")])
}

crate::ktest!(
    fn ns_symlink_readlink_truncation_proc_links_and_errnos() {
        use crate::kassert_eq;
        use crate::linux::process;
        use kt::{BUF, open, ptr, smudge, up, user};
        process::in_test_process(|pid| {
            kt::on_tmpfs_and_ext4("symlink", 153, ktest_symlink_and_readlink)?;
            // procfs's magic links: `/proc/self/exe` (cut too), and an fd's
            // path by `/proc/<pid>/fd/N` and `/proc/self/fd/N`.
            let exe = "/bin/ns-ktest-exe";
            process::set_current_exe_path(exe);
            smudge()?;
            kassert_eq!(sys_readlink(up(0, "/proc/self/exe")?, ptr(BUF), 64), exe.len() as u64);
            kassert_eq!(user(BUF, exe.len() + 1)?, [exe.as_bytes(), &[0xaa]].concat());
            smudge()?;
            kassert_eq!(sys_readlink(up(0, "/proc/self/exe")?, ptr(BUF), 4), 4);
            kassert_eq!(user(BUF, 5)?, b"/bin\xaa".to_vec());
            let fd = open(1, "/dev/null", lumen_vfs::OpenFlags::O_RDONLY)?;
            for path in [alloc::format!("/proc/{pid}/fd/{}", fd.raw()), alloc::format!("/proc/self/fd/{}", fd.raw())] {
                smudge()?;
                kassert_eq!(sys_readlink(up(2, &path)?, ptr(BUF), 64), 9);
                kassert_eq!(user(BUF, 10)?, b"/dev/null\xaa".to_vec());
            }
            Ok(())
        })?
    }
);

#[cfg(feature = "ktest")]
fn ktest_rename(base: &str) -> kt::R {
    use super::at::reg;
    use crate::linux::{errno, general};
    use crate::{kassert, kassert_eq};
    use kt::{BAD, e, lstat, open, ptr, read, up};
    use lumen_vfs::{InodeType, OpenFlags as O, VfsError};
    let p = |s: &str| alloc::format!("{base}/{s}");
    let cwd = Dirfd::Cwd;
    let gone = |s: &str| lstat(&p(s)).map(drop) == Err(VfsError::NoEnt);
    let noreplace = RenameFlags(RenameFlags::NOREPLACE);
    let exchange = RenameFlags(RenameFlags::EXCHANGE);

    // Over an existing file: atomic, and the old target's open fd still
    // reads the old data.
    kt::file(&p("a"), b"AAA")?;
    kt::file(&p("b"), b"BBB")?;
    let bfd = open(0, &p("b"), O::O_RDONLY)?;
    kassert_eq!(sys_rename(up(1, &p("a"))?, up(2, &p("b"))?), 0);
    kassert!(gone("a"));
    kassert_eq!(read(&p("b")), Ok(b"AAA".to_vec()));
    kassert_eq!(kt::fd_read(bfd)?, b"BBB".to_vec());
    // From dirfds.
    let dfd = open(3, base, O::O_PATH | O::O_DIRECTORY)?;
    kassert_eq!(sys_renameat(Dirfd::Fd(dfd), up(4, "b")?, Dirfd::Fd(dfd), up(5, "c")?), 0);
    kassert!(gone("b"));
    kassert_eq!(read(&p("c")), Ok(b"AAA".to_vec()));
    // RENAME_NOREPLACE: EEXIST onto a name in use (nothing moves); a free
    // one renames.
    kt::file(&p("n"), b"NNN")?;
    kassert_eq!(sys_renameat2(cwd, up(1, &p("c"))?, cwd, up(2, &p("n"))?, noreplace), e(errno::EEXIST));
    kassert_eq!((read(&p("c")), read(&p("n"))), (Ok(b"AAA".to_vec()), Ok(b"NNN".to_vec())));
    kassert_eq!(sys_renameat2(cwd, up(1, &p("c"))?, cwd, up(2, &p("c2"))?, noreplace), 0);
    kassert!(gone("c"));
    // The same file both sides: nothing happens (EEXIST with NOREPLACE).
    kassert_eq!(sys_rename(up(1, &p("c2"))?, up(2, &p("c2"))?), 0);
    kassert_eq!(sys_renameat2(cwd, up(1, &p("c2"))?, cwd, up(2, &p("c2"))?, noreplace), e(errno::EEXIST));
    kassert_eq!(read(&p("c2")), Ok(b"AAA".to_vec()));
    // RENAME_EXCHANGE: two files swap, then a file and a directory; ENOENT
    // if either side is missing.
    kassert_eq!(sys_renameat2(cwd, up(1, &p("c2"))?, cwd, up(2, &p("n"))?, exchange), 0);
    kassert_eq!((read(&p("c2")), read(&p("n"))), (Ok(b"NNN".to_vec()), Ok(b"AAA".to_vec())));
    kt::mkdir(&p("dd"))?;
    kt::file(&p("dd/in"), b"I")?;
    kassert_eq!(sys_renameat2(cwd, up(1, &p("n"))?, cwd, up(2, &p("dd"))?, exchange), 0);
    kassert_eq!(lstat(&p("n")).map(|s| s.0), Ok(InodeType::Dir));
    kassert_eq!((read(&p("n/in")), read(&p("dd"))), (Ok(b"I".to_vec()), Ok(b"AAA".to_vec())));
    kassert_eq!(sys_renameat2(cwd, up(1, &p("c2"))?, cwd, up(2, &p("none"))?, exchange), e(errno::ENOENT));
    kassert_eq!(sys_renameat2(cwd, up(1, &p("none"))?, cwd, up(2, &p("c2"))?, exchange), e(errno::ENOENT));
    // EINVAL: RENAME_WHITEOUT, NOREPLACE with EXCHANGE, an unknown bit,
    // before the paths are read.
    let both = RenameFlags::NOREPLACE | RenameFlags::EXCHANGE;
    for bad in [general::RENAME_WHITEOUT, both, 8, 0x8000_0000] {
        kassert_eq!(sys_renameat2(cwd, up(1, &p("c2"))?, cwd, up(2, &p("x"))?, RenameFlags(bad)), e(errno::EINVAL));
        kassert_eq!(sys_renameat2(cwd, ptr(0), cwd, ptr(0), RenameFlags(bad)), e(errno::EINVAL));
    }
    kassert!(gone("x"));
    kassert_eq!(read(&p("c2")), Ok(b"NNN".to_vec()));
    // Directories: EINVAL into its own subtree, ENOTEMPTY onto a non-empty
    // one; an empty one is replaced.
    kt::mkdir(&p("p"))?;
    kt::mkdir(&p("p/q"))?;
    kassert_eq!(sys_rename(up(1, &p("p"))?, up(2, &p("p/q/r"))?), e(errno::EINVAL));
    kassert_eq!(sys_rename(up(1, &p("p"))?, up(2, &p("p/r"))?), e(errno::EINVAL));
    kassert_eq!(sys_rename(up(1, &p("p"))?, up(2, &p("n"))?), e(errno::ENOTEMPTY));
    kt::mkdir(&p("empty"))?;
    kassert_eq!(sys_rename(up(1, &p("p"))?, up(2, &p("empty"))?), 0);
    kassert!(gone("p") && lstat(&p("empty/q")).is_ok());
    // EISDIR: a file onto a directory; ENOTDIR: a directory onto a file.
    kassert_eq!(sys_rename(up(1, &p("c2"))?, up(2, &p("empty"))?), e(errno::EISDIR));
    kassert_eq!(sys_rename(up(1, &p("empty"))?, up(2, &p("c2"))?), e(errno::ENOTDIR));
    // EXDEV: onto the root filesystem. EBUSY: `/` either side.
    kassert_eq!(sys_rename(up(1, &p("c2"))?, up(2, "/ns-ktest-xdev")?), e(errno::EXDEV));
    kassert_eq!(sys_rename(up(1, "/")?, up(2, &p("x"))?), e(errno::EBUSY));
    kassert_eq!(sys_rename(up(1, &p("c2"))?, up(2, "/")?), e(errno::EBUSY));
    // ENOENT, ENOTDIR, EFAULT (either pointer), EBADF (either dirfd), ELOOP.
    kassert_eq!(sys_rename(up(1, &p("none"))?, up(2, &p("x"))?), e(errno::ENOENT));
    kassert_eq!(sys_rename(up(1, &p("c2/x"))?, up(2, &p("x"))?), e(errno::ENOTDIR));
    kassert_eq!(sys_rename(ptr(BAD), up(2, &p("x"))?), e(errno::EFAULT));
    kassert_eq!(sys_rename(up(1, &p("c2"))?, ptr(BAD)), e(errno::EFAULT));
    kassert_eq!(sys_renameat(Dirfd::Fd(reg(99)), up(1, "c2")?, cwd, up(2, &p("x"))?), e(errno::EBADF));
    kassert_eq!(sys_renameat(cwd, up(1, &p("c2"))?, Dirfd::Fd(reg(99)), up(2, "x")?), e(errno::EBADF));
    kt::symlink("loop", &p("loop"))?;
    kassert_eq!(sys_rename(up(1, &p("loop/x"))?, up(2, &p("x"))?), e(errno::ELOOP));
    kassert!(gone("x"));
    kassert_eq!(super::fd::sys_close(bfd), 0);
    kassert_eq!(super::fd::sys_close(dfd), 0);
    kt::remove(&[&p("n/in"), &p("n"), &p("dd"), &p("empty/q"), &p("empty"), &p("c2"), &p("loop")])
}

crate::ktest!(
    fn ns_rename_renameat2_over_open_files_exchange_and_errnos() {
        crate::linux::process::in_test_process(|_| kt::on_tmpfs_and_ext4("rename", 154, ktest_rename))?
    }
);

crate::ktest!(
    fn ns_handlers_take_their_registers_in_order() {
        // Through `dispatch`, each argument in its own register with
        // distinct values (garbage above an `int`'s 32 bits too), and two
        // dirfds where a call takes two, so an arm passing them out of
        // order can't pass.
        use crate::arch::syscall::SyscallFrame;
        use crate::kassert_eq;
        use crate::linux::{errno, general};
        use kt::{BUF, e, lstat, open, up, user};
        use lumen_vfs::{InodeType, OpenFlags as O, VfsError};
        crate::linux::process::in_test_process(|_| {
            let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64| {
                let mut frame = SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, r8, ..SyscallFrame::default() };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            let high = 0xdead_beef_0000_0000u64;
            let base = "/tmp/ns-ktest-regs";
            let p = |s: &str| alloc::format!("{base}/{s}");
            let addr = |slot: u64, path: &str| up(slot, path).map(|u| u.addr());
            kt::mkdir(base)?;
            let top = open(0, base, O::O_PATH | O::O_DIRECTORY)?.raw() as u64;
            // mkdirat(top, "d", 0o750), then mkdir(path, 0o700).
            kassert_eq!(call(general::__NR_mkdirat, high | top, addr(1, "d")?, high | 0o750, 0, 0), 0);
            kassert_eq!(lstat(&p("d")).map(|s| s.1), Ok(0o750));
            let sub = open(2, &p("d"), O::O_PATH | O::O_DIRECTORY)?.raw() as u64;
            kassert_eq!(call(general::__NR_mkdir, addr(3, &p("m"))?, high | 0o700, 0, 0, 0), 0);
            kassert_eq!(lstat(&p("m")).map(|s| s.1), Ok(0o700));
            // symlinkat("target", sub, "s"), then symlink("t2", path).
            kassert_eq!(call(general::__NR_symlinkat, addr(4, "target")?, high | sub, addr(5, "s")?, 0, 0), 0);
            kassert_eq!(lstat(&p("d/s")).map(|s| s.0), Ok(InodeType::Symlink));
            kassert_eq!(call(general::__NR_symlink, addr(4, "t2")?, addr(6, &p("s2"))?, 0, 0, 0), 0);
            // readlinkat(sub, "s", buf, 3), then readlink(path, buf, 2).
            kassert_eq!(call(general::__NR_readlinkat, high | sub, addr(5, "s")?, BUF, high | 3, 0), 3);
            kassert_eq!(user(BUF, 3)?, b"tar".to_vec());
            kassert_eq!(call(general::__NR_readlink, addr(6, &p("s2"))?, BUF, high | 2, 0, 0), 2);
            kassert_eq!(user(BUF, 2)?, b"t2".to_vec());
            // linkat(top, "sf", sub, "h", AT_SYMLINK_FOLLOW): `d/h` is the
            // file `sf` names, not a symlink.
            kt::file(&p("f"), b"f")?;
            kt::symlink("f", &p("sf"))?;
            let ino = lstat(&p("f")).map_err(|_| "stat f")?.3;
            let (sf, h) = (addr(7, "sf")?, addr(8, "h")?);
            let follow = u64::from(AtFlags::AT_SYMLINK_FOLLOW);
            kassert_eq!(call(general::__NR_linkat, high | top, sf, high | sub, h, high | follow), 0);
            kassert_eq!(lstat(&p("d/h")).map(|s| (s.0, s.3)), Ok((InodeType::File, ino)));
            // link(old, new).
            kassert_eq!(call(general::__NR_link, addr(9, &p("f"))?, addr(10, &p("l"))?, 0, 0, 0), 0);
            kassert_eq!(lstat(&p("l")).map(|s| s.3), Ok(ino));
            // renameat2(sub, "h", top, "sf", RENAME_NOREPLACE): EEXIST (with
            // the flag lost it would replace `sf`; with the dirfds swapped
            // there is no `h`: ENOENT).
            let noreplace = u64::from(RenameFlags::NOREPLACE);
            kassert_eq!(
                call(general::__NR_renameat2, high | sub, h, high | top, sf, high | noreplace),
                e(errno::EEXIST)
            );
            // renameat2(sub, "h", top, "h2", RENAME_NOREPLACE), then
            // renameat(top, "h2", sub, "h3").
            let (h2, h3) = (addr(11, "h2")?, addr(12, "h3")?);
            kassert_eq!(call(general::__NR_renameat2, high | sub, h, high | top, h2, high | noreplace), 0);
            kassert_eq!(lstat(&p("h2")).map(|s| s.3), Ok(ino));
            kassert_eq!(call(general::__NR_renameat, high | top, h2, high | sub, h3, 0), 0);
            kassert_eq!(lstat(&p("d/h3")).map(|s| s.3), Ok(ino));
            // rename(old, new).
            kassert_eq!(call(general::__NR_rename, addr(9, &p("l"))?, addr(10, &p("l2"))?, 0, 0, 0), 0);
            kassert_eq!(lstat(&p("l2")).map(|s| s.3), Ok(ino));
            // unlinkat(sub, "h3", 0), unlinkat(top, "m", AT_REMOVEDIR), then
            // unlink(path) and rmdir(path).
            kassert_eq!(call(general::__NR_unlinkat, high | sub, h3, high, 0, 0), 0);
            kassert_eq!(lstat(&p("d/h3")).map(drop), Err(VfsError::NoEnt));
            let removedir = u64::from(AtFlags::AT_REMOVEDIR);
            kassert_eq!(call(general::__NR_unlinkat, high | top, addr(13, "m")?, high | removedir, 0, 0), 0);
            kassert_eq!(lstat(&p("m")).map(drop), Err(VfsError::NoEnt));
            kassert_eq!(call(general::__NR_unlink, addr(9, &p("l2"))?, 0, 0, 0, 0), 0);
            kassert_eq!(lstat(&p("l2")).map(drop), Err(VfsError::NoEnt));
            kt::remove(&[&p("d/s")])?;
            kassert_eq!(call(general::__NR_rmdir, addr(9, &p("d"))?, 0, 0, 0, 0), 0);
            kassert_eq!(lstat(&p("d")).map(drop), Err(VfsError::NoEnt));
            kassert_eq!(super::fd::sys_close(super::at::reg(sub)), 0);
            kassert_eq!(super::fd::sys_close(super::at::reg(top)), 0);
            kt::remove(&[&p("s2"), &p("f"), &p("sf"), base])
        })?
    }
);
