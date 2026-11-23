//! Inode metadata and size (M7): `chmod`, `fchmod`, `fchmodat`,
//! `fchmodat2`, `chown`, `fchown`, `lchown`, `fchownat`, `umask`,
//! `utimensat`, `truncate`, `ftruncate`.
//!
//! Each change goes through the inode's own setter (`VfsInode::set_mode`,
//! `set_owner`, `set_times`, `truncate`), which updates `ctime` (and, for a
//! truncate, `mtime`) under the inode's lock. A path is resolved through
//! `at::lookup`; a descriptor must be open, and not `O_PATH`
//! (`-EBADF`). A change on a read-only mount is `-EROFS`, checked here
//! against the mount table before the setter is asked. devfs is a writable
//! mount, as Linux's devtmpfs (#301), so `chmod`, `chown` and `utimensat`
//! there reach devfs's setters, which keep a node's new mode, owner and
//! times in memory and refuse `/dev` itself (#293).
//!
//! A truncate then unmaps the current process's mapped pages of the file
//! past its new end (`AddressSpace::invalidate_file_from`), so a later
//! access there is a bus error. Other processes' mappings aren't reached
//! until M8 (issue #255).

use alloc::sync::Arc;

use lumen_vfs::{AtFlags, InodeType, SetTime, VfsCacheOps, VfsInode};

use super::at::{self, Dirfd, Fd, Mode, UserPath, UserPtr};
use super::fd::vfs;
use super::finish;
use crate::linux::errno;
use crate::linux::process::{FileRef, with_current_mut};
use crate::user::{CopyError, copy_from_user};
use crate::vfs::tables;

/// The flags `fchmodat2`, `fchownat` and `utimensat` (with a path) take.
const PATH_FLAGS: u32 = AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH;

/// The inode `path` names from `dirfd`: `flags` checked first (anything
/// but `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` is `-EINVAL`), then the
/// path copied in (empty only with `AT_EMPTY_PATH`) and resolved by
/// `at::lookup`.
fn lookup(dirfd: Dirfd, path: UserPtr, flags: AtFlags) -> Result<Arc<dyn VfsInode>, u32> {
    let flags = flags.validate(PATH_FLAGS).map_err(vfs)?;
    at::lookup(dirfd, &UserPath::copy_in(path, flags.has_empty_path())?, flags)
}

/// The open file on `fd` for a change through it: `-EBADF` if it isn't
/// open or is `O_PATH` (Linux's `fdget`). Held for the change, so the
/// inode stays open meanwhile.
fn open_file(fd: Fd) -> Result<FileRef, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    Ok(file)
}

/// `-EROFS` if `inode` is on a read-only mount (Linux's `mnt_want_write`),
/// checked before the setter, whatever the inode's type: a special file on
/// a read-only mount is `-EROFS` here, as on Linux. devfs's mounts are
/// writable (#301), its setters deciding. A device not in the mount table
/// (a ktest's scratch volume) counts as writable: ext4's setters still
/// refuse on a read-only volume.
fn writable_mount(inode: &dyn VfsInode) -> Result<(), u32> {
    if tables().fs_read_only(inode.dev_id()) { Err(errno::EROFS) } else { Ok(()) }
}

/// Set `inode`'s permission and special bits (`mode & 0o7777`): a symlink's
/// mode can't change (`-EOPNOTSUPP`, as Linux 6.6's `notify_change` says).
fn chmod(inode: &dyn VfsInode, mode: Mode) -> Result<u64, u32> {
    writable_mount(inode)?;
    if inode.inode_type() == InodeType::Symlink {
        return Err(errno::EOPNOTSUPP);
    }
    inode.set_mode(mode.perm()).map_err(vfs)?;
    Ok(0)
}

fn fchmodat(dirfd: Dirfd, path: UserPtr, mode: Mode, flags: AtFlags) -> Result<u64, u32> {
    chmod(&*lookup(dirfd, path, flags)?, mode)
}

/// `chmod(path, mode)` (90): `fchmodat(AT_FDCWD, path, mode)`.
pub(super) fn sys_chmod(path: UserPtr, mode: Mode) -> u64 {
    finish(fchmodat(Dirfd::Cwd, path, mode, AtFlags(0)))
}

/// `fchmod(fd, mode)` (91).
pub(super) fn sys_fchmod(fd: Fd, mode: Mode) -> u64 {
    finish(open_file(fd).and_then(|file| chmod(&*file.inode, mode)))
}

/// `fchmodat(dirfd, path, mode)` (268): following symlinks. Three arguments
/// on Linux x86-64: `r10` is never passed (`musl` leaves it uninitialized).
pub(super) fn sys_fchmodat(dirfd: Dirfd, path: UserPtr, mode: Mode) -> u64 {
    finish(fchmodat(dirfd, path, mode, AtFlags(0)))
}

/// `fchmodat2(dirfd, path, mode, flags)` (452): `AT_SYMLINK_NOFOLLOW`
/// (a symlink then `-EOPNOTSUPP`) and `AT_EMPTY_PATH`; any other flag
/// `-EINVAL`.
pub(super) fn sys_fchmodat2(dirfd: Dirfd, path: UserPtr, mode: Mode, flags: AtFlags) -> u64 {
    finish(fchmodat(dirfd, path, mode, flags))
}

/// A `uid_t`/`gid_t` argument: `-1` leaves the id as it is.
fn id(raw: u32) -> Option<u32> {
    (raw != u32::MAX).then_some(raw)
}

/// Set `inode`'s owner and group. The process is root, so any id goes.
/// `VfsInode::set_owner` also drops the set-id bits a chown drops
/// (`lumen_vfs::mode_after_chown`), in the same hold of the inode's lock
/// and the same disk write: a failed chown changes nothing.
fn chown(inode: &dyn VfsInode, uid: u32, gid: u32) -> Result<u64, u32> {
    writable_mount(inode)?;
    inode.set_owner(id(uid), id(gid)).map_err(vfs)?;
    Ok(0)
}

fn fchownat(dirfd: Dirfd, path: UserPtr, uid: u32, gid: u32, flags: AtFlags) -> Result<u64, u32> {
    chown(&*lookup(dirfd, path, flags)?, uid, gid)
}

/// `chown(path, uid, gid)` (92): following symlinks.
pub(super) fn sys_chown(path: UserPtr, uid: u32, gid: u32) -> u64 {
    finish(fchownat(Dirfd::Cwd, path, uid, gid, AtFlags(0)))
}

/// `fchown(fd, uid, gid)` (93).
pub(super) fn sys_fchown(fd: Fd, uid: u32, gid: u32) -> u64 {
    finish(open_file(fd).and_then(|file| chown(&*file.inode, uid, gid)))
}

/// `lchown(path, uid, gid)` (94): the symlink itself.
pub(super) fn sys_lchown(path: UserPtr, uid: u32, gid: u32) -> u64 {
    finish(fchownat(Dirfd::Cwd, path, uid, gid, AtFlags(AtFlags::AT_SYMLINK_NOFOLLOW)))
}

/// `fchownat(dirfd, path, uid, gid, flags)` (260): `AT_SYMLINK_NOFOLLOW`
/// and `AT_EMPTY_PATH`; any other flag `-EINVAL`.
pub(super) fn sys_fchownat(dirfd: Dirfd, path: UserPtr, uid: u32, gid: u32, flags: AtFlags) -> u64 {
    finish(fchownat(dirfd, path, uid, gid, flags))
}

/// `umask(mask)` (95): the process's mask becomes `mask & 0o777`; the old
/// one is returned. Never fails.
pub(super) fn sys_umask(mask: Mode) -> u64 {
    let new = (mask.bits() & 0o777) as u16;
    u64::from(with_current_mut(|p| core::mem::replace(&mut p.umask, new)))
}

/// The two `struct timespec`s (`{ i64 tv_sec; i64 tv_nsec; }`, atime then
/// mtime) at `times`, as `(sec, nsec)` pairs.
fn copy_times(times: UserPtr) -> Result<[(i64, i64); 2], u32> {
    let mut raw = [0u8; 32];
    copy_from_user(&mut raw, times.addr()).map_err(CopyError::to_errno)?;
    let word = |i: usize| i64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
    Ok([(word(0), word(1)), (word(2), word(3))])
}

/// `utimensat`, in Linux's order: the times copied in (`-EFAULT`); both
/// `UTIME_OMIT` is done before even the path is looked at; then a `NULL`
/// path with a descriptor is `futimens` on it (any flag `-EINVAL`), any
/// other a path (unknown flags `-EINVAL`, `NULL` `-EFAULT`); then an
/// invalid `tv_nsec` is `-EINVAL` (`vfs_utimes`), and a read-only mount
/// `-EROFS`.
fn utimensat(dirfd: Dirfd, path: UserPtr, times: UserPtr, flags: AtFlags) -> Result<u64, u32> {
    let raw = if times.is_null() { None } else { Some(copy_times(times)?) };
    if let Some([(_, a), (_, m)]) = raw
        && a == SetTime::UTIME_OMIT
        && m == SetTime::UTIME_OMIT
    {
        return Ok(0);
    }
    // The open file, for `futimens`, held while its inode changes.
    let (inode, _file) = match dirfd {
        Dirfd::Fd(fd) if path.is_null() => {
            if flags.0 != 0 {
                return Err(errno::EINVAL);
            }
            let file = open_file(fd)?;
            (Arc::clone(&file.inode), Some(file))
        }
        _ => (lookup(dirfd, path, flags)?, None),
    };
    let (atime, mtime) = match raw {
        None => (SetTime::Now, SetTime::Now),
        Some([(asec, ansec), (msec, mnsec)]) => {
            (SetTime::from_raw(asec, ansec).map_err(vfs)?, SetTime::from_raw(msec, mnsec).map_err(vfs)?)
        }
    };
    writable_mount(&*inode)?;
    inode.set_times(atime, mtime).map_err(vfs)?;
    Ok(0)
}

/// `utimensat(dirfd, path, times, flags)` (280): a `NULL` `times` sets both
/// to now; `UTIME_NOW` and `UTIME_OMIT` per timestamp; `AT_SYMLINK_NOFOLLOW`
/// and `AT_EMPTY_PATH`.
pub(super) fn sys_utimensat(dirfd: Dirfd, path: UserPtr, times: UserPtr, flags: AtFlags) -> u64 {
    finish(utimensat(dirfd, path, times, flags))
}

/// A `loff_t` length: `-EINVAL` if negative.
fn length(raw: u64) -> Result<u64, u32> {
    if (raw as i64) < 0 { Err(errno::EINVAL) } else { Ok(raw) }
}

/// Resize `inode` (a regular file) on a writable mount (`-EROFS`) to `len`,
/// then unmap the current process's pages of it past the new end
/// (`super::unmap_past`). A grow finds nothing to unmap: a fault refuses a
/// page wholly past the old end, and every shrink of the caller's
/// (`truncate`, `ftruncate`, `openat(O_TRUNC)`) unmapped its own pages
/// past the end it made; any leaf past the new end is cleared all the same.
fn resize(inode: &dyn VfsInode, len: u64) -> Result<u64, u32> {
    writable_mount(inode)?;
    let space = super::shrinking_space();
    inode.truncate(len).map_err(vfs)?;
    super::unmap_past(&space, inode, len);
    Ok(0)
}

fn truncate(path: UserPtr, len: u64) -> Result<u64, u32> {
    let len = length(len)?;
    let inode = lookup(Dirfd::Cwd, path, AtFlags(0))?;
    match inode.inode_type() {
        InodeType::File => {}
        InodeType::Dir => return Err(errno::EISDIR),
        _ => return Err(errno::EINVAL),
    }
    resize(&*inode, len)
}

fn ftruncate(fd: Fd, len: u64) -> Result<u64, u32> {
    let len = length(len)?;
    let file = open_file(fd)?;
    // Not open for writing, or not a regular file (a directory too):
    // `-EINVAL`, as Linux's `do_sys_ftruncate`.
    if !file.is_writable() || file.inode.inode_type() != InodeType::File {
        return Err(errno::EINVAL);
    }
    resize(&*file.inode, len)
}

/// `truncate(path, length)` (76): following symlinks. `-EINVAL` for a
/// negative length or anything but a regular file, `-EISDIR` for a
/// directory, `-EROFS` on a read-only mount, `-EFBIG` past the
/// filesystem's largest file.
pub(super) fn sys_truncate(path: UserPtr, length: u64) -> u64 {
    finish(truncate(path, length))
}

/// `ftruncate(fd, length)` (77): `-EBADF` if `fd` isn't open (or is
/// `O_PATH`), `-EINVAL` for a negative length, a descriptor not open for
/// writing or anything but a regular file, `-EROFS` on a read-only mount,
/// `-EFBIG` past the filesystem's largest file.
pub(super) fn sys_ftruncate(fd: Fd, length: u64) -> u64 {
    finish(ftruncate(fd, length))
}

#[cfg(feature = "ktest")]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use lumen_vfs::{Metadata, OpenFlags, ResolveFlags, Timespec, VfsInode};
    use lumen_vm::{MapPlace, Prot};

    use super::super::at::{self, Dirfd, Fd, UserPtr};
    use super::super::fd::{
        ktest_failed_at, ktest_fd_of, ktest_install, ktest_open, ktest_unlink, ktest_user_path, sys_close, sys_open,
        sys_openat,
    };
    use super::super::neg_errno;
    use super::*;
    use crate::linux::general;
    use crate::linux::process;
    use crate::mm::user_vm::{FaultResult, Sharing};
    use crate::user::copy_to_user;
    use crate::{kassert, kassert_eq};

    const P: u64 = 4096;

    /// `path`'s metadata, its last symlink not followed.
    fn meta_of(path: &str) -> Result<Metadata, &'static str> {
        let start = crate::vfs::tables();
        let flags = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
        let inode = lumen_vfs::resolve_path(start, None, path.as_bytes(), flags).map_err(|_| "resolve")?.inode;
        inode.stat().map_err(|_| "stat")
    }

    fn inode_of(path: &str) -> Result<Arc<dyn VfsInode>, &'static str> {
        crate::vfs::resolve_path_from_root(path).map_err(|_| "resolve")
    }

    fn ts(t: Timespec) -> (i64, i64) {
        (t.sec, t.nsec)
    }

    /// Spin until the realtime clock reads later than `t`, so a timestamp
    /// set from now is strictly later than `t` (a floor, never a ceiling).
    fn wait_past(t: Timespec) -> Result<(), &'static str> {
        for _ in 0..10_000_000 {
            if ts(crate::time::realtime_timespec()) > ts(t) {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("the realtime clock didn't move")
    }

    /// Create (or truncate) `path` through `open` (mode 0o644, less the
    /// umask), write `data` to it and close it: the path in user memory.
    fn make_file(slot: u64, path: &[u8], data: &[u8]) -> Result<UserPtr, &'static str> {
        let p = ktest_user_path(slot, path)?;
        let flags = OpenFlags::from_bits(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
        let fd = ktest_fd_of(sys_open(p, flags, at::reg(0o644)), "open", path)?;
        let path = core::str::from_utf8(path).map_err(|_| "utf-8")?;
        if inode_of(path)?.write_at(0, data).map_err(|_| "write_at")? != data.len() {
            return Err("short write");
        }
        kassert_eq!(sys_close(fd), 0);
        Ok(p)
    }

    /// Open `path` (already in user memory) with `flags`: the descriptor.
    /// A failure's `ktest::diag` line names the path, read back from user
    /// memory, and the flags.
    fn open(path: UserPtr, flags: u32) -> Result<Fd, &'static str> {
        let ret = sys_open(path, OpenFlags::from_bits(flags), at::reg(0));
        if ret < lumen_vfs::MAX_FDS as u64 {
            return Ok(at::reg(ret));
        }
        let name = at::UserPath::copy_in(path, true).map(|p| p.as_bytes().to_vec());
        let call = alloc::format!("open({flags:#o})");
        ktest_fd_of(ret, &call, name.as_deref().unwrap_or(b"<unreadable path>"))
    }

    fn symlink(target: &[u8], at: &[u8]) -> Result<(), &'static str> {
        lumen_vfs::symlink_at(crate::vfs::tables(), target, None, at).map(|_| ()).map_err(|_| "symlink")
    }

    /// Remove each of `paths`. The tests call it before making their
    /// files (an earlier run's, if it crashed) and after, with names a test
    /// may not have made (its `-link`s), so `NoEnt` is expected; any other
    /// failure (a directory left at the name, say) gets a `ktest::diag`
    /// line naming the path, without failing the test.
    fn remove(paths: &[&[u8]]) {
        for p in paths {
            if let Err(e) = ktest_unlink(p)
                && e != lumen_vfs::VfsError::NoEnt
            {
                ktest_failed_at("remove a test's file", p, e);
            }
        }
    }

    fn flags(bits: u32) -> AtFlags {
        AtFlags(bits)
    }

    /// `utimensat`'s two timespecs written at `addr` in user memory.
    fn put_times(addr: u64, times: [(i64, i64); 2]) -> Result<UserPtr, &'static str> {
        let mut raw = [0u8; 32];
        for (i, (sec, nsec)) in times.iter().enumerate() {
            raw[i * 16..i * 16 + 8].copy_from_slice(&sec.to_le_bytes());
            raw[i * 16 + 8..i * 16 + 16].copy_from_slice(&nsec.to_le_bytes());
        }
        copy_to_user(addr, &raw).map_err(|_| "copy the times")?;
        Ok(at::reg(addr))
    }

    crate::ktest!(
        fn meta_chmod_family_sets_the_permission_and_special_bits() {
            process::in_test_process(|_| {
                let path = make_file(0, b"/tmp/meta-ktest-chmod", b"x")?;
                symlink(b"meta-ktest-chmod", b"/tmp/meta-ktest-chmod-link")?;
                let link = ktest_user_path(1, b"/tmp/meta-ktest-chmod-link")?;
                let link_mode = meta_of("/tmp/meta-ktest-chmod-link")?.mode;
                // chmod: only `0o7777` changes (file-type bits ignored), and
                // ctime moves past what it was.
                let before = meta_of("/tmp/meta-ktest-chmod")?;
                wait_past(before.ctime)?;
                kassert_eq!(sys_chmod(path, at::reg(0o170_000 | 0o4751)), 0);
                let after = meta_of("/tmp/meta-ktest-chmod")?;
                kassert_eq!(after.mode, 0o4751);
                kassert_eq!(after.inode_type, InodeType::File);
                kassert!(ts(after.ctime) > ts(before.ctime));
                // chmod through a symlink changes the target, not the link.
                kassert_eq!(sys_chmod(link, at::reg(0o7777)), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o7777);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod-link")?.mode, link_mode);
                // fchmod on an open descriptor; `O_PATH` or closed `-EBADF`.
                let fd = open(path, OpenFlags::O_RDONLY)?;
                kassert_eq!(sys_fchmod(fd, at::reg(0o640)), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o640);
                let opath = open(path, OpenFlags::O_PATH)?;
                kassert_eq!(sys_fchmod(opath, at::reg(0o600)), neg_errno(errno::EBADF));
                kassert_eq!(sys_fchmod(at::reg(900), at::reg(0o600)), neg_errno(errno::EBADF));
                // fchmodat relative to a directory descriptor.
                let tmp = ktest_open(2, b"/tmp", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
                let rel = ktest_user_path(3, b"meta-ktest-chmod")?;
                kassert_eq!(sys_fchmodat(Dirfd::Fd(tmp), rel, at::reg(0o604)), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o604);
                // fchmodat2: AT_SYMLINK_NOFOLLOW on a regular file works, on
                // a symlink is `-EOPNOTSUPP` (the link left as it was);
                // AT_EMPTY_PATH names the descriptor's file, `O_PATH` too.
                let nofollow = flags(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_fchmodat2(Dirfd::Cwd, path, at::reg(0o611), nofollow), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o611);
                kassert_eq!(sys_fchmodat2(Dirfd::Cwd, link, at::reg(0o600), nofollow), neg_errno(errno::EOPNOTSUPP));
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod-link")?.mode, link_mode);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o611);
                kassert_eq!(sys_fchmodat2(Dirfd::Cwd, link, at::reg(0o612), flags(0)), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o612);
                let empty = ktest_user_path(4, b"")?;
                let empty_path = flags(AtFlags::AT_EMPTY_PATH);
                kassert_eq!(sys_fchmodat2(Dirfd::Fd(opath), empty, at::reg(0o613), empty_path), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o613);
                kassert_eq!(sys_fchmodat2(Dirfd::Fd(opath), empty, at::reg(0o613), flags(0)), neg_errno(errno::ENOENT));
                // Unknown flags: `-EINVAL` (AT_REMOVEDIR, AT_SYMLINK_FOLLOW).
                for bad in [AtFlags::AT_REMOVEDIR, AtFlags::AT_SYMLINK_FOLLOW, 1 << 30] {
                    kassert_eq!(sys_fchmodat2(Dirfd::Cwd, path, at::reg(0o600), flags(bad)), neg_errno(errno::EINVAL));
                }
                kassert_eq!(meta_of("/tmp/meta-ktest-chmod")?.mode, 0o613);
                // A missing file `-ENOENT`; a read-only mount `-EROFS`.
                let missing = ktest_user_path(5, b"/tmp/meta-ktest-no-such")?;
                kassert_eq!(sys_chmod(missing, at::reg(0o600)), neg_errno(errno::ENOENT));
                let ro = ktest_user_path(6, b"/proc/version")?;
                kassert_eq!(sys_chmod(ro, at::reg(0o600)), neg_errno(errno::EROFS));
                kassert_eq!(sys_fchmodat(Dirfd::Cwd, ro, at::reg(0o600)), neg_errno(errno::EROFS));
                // The mount table's check comes first: procfs's `self` is a
                // symlink, so without it this would be `-EOPNOTSUPP`.
                let proc_self = ktest_user_path(7, b"/proc/self")?;
                let nofollow = flags(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_fchmodat2(Dirfd::Cwd, proc_self, at::reg(0o600), nofollow), neg_errno(errno::EROFS));
                remove(&[b"/tmp/meta-ktest-chmod", b"/tmp/meta-ktest-chmod-link"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_chown_family_sets_owner_and_group_with_minus_one_unchanged() {
            process::in_test_process(|_| {
                let path = make_file(0, b"/tmp/meta-ktest-chown", b"x")?;
                symlink(b"/tmp/meta-ktest-chown", b"/tmp/meta-ktest-chown-link")?;
                let link = ktest_user_path(1, b"/tmp/meta-ktest-chown-link")?;
                let owner = |p: &str| meta_of(p).map(|m| (m.uid, m.gid));
                let before = meta_of("/tmp/meta-ktest-chown")?;
                kassert_eq!((before.uid, before.gid), (0, 0));
                wait_past(before.ctime)?;
                // chown: uid only (-1 keeps the gid), then gid only; ctime moves.
                kassert_eq!(sys_chown(path, 1000, u32::MAX), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (1000, 0));
                kassert!(ts(meta_of("/tmp/meta-ktest-chown")?.ctime) > ts(before.ctime));
                kassert_eq!(sys_chown(path, u32::MAX, 2000), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (1000, 2000));
                kassert_eq!(sys_chown(path, u32::MAX, u32::MAX), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (1000, 2000));
                // On a regular file, chown (even with -1, -1) drops setuid,
                // and setgid only with group-execute; a directory keeps both.
                let mode = || meta_of("/tmp/meta-ktest-chown").map(|m| m.mode);
                for (before, after) in [(0o6755, 0o0755), (0o6745, 0o2745), (0o2711, 0o0711), (0o1644, 0o1644)] {
                    kassert_eq!(sys_chmod(path, at::reg(before)), 0);
                    kassert_eq!(sys_chown(path, u32::MAX, u32::MAX), 0);
                    kassert_eq!(mode()?, after);
                }
                let dir = ktest_user_path(7, b"/tmp/meta-ktest-chown-dir")?;
                lumen_vfs::mkdir_at(crate::vfs::tables(), None, b"/tmp/meta-ktest-chown-dir", 0o755, 0)
                    .map_err(|_| "mkdir")?;
                kassert_eq!(sys_chmod(dir, at::reg(0o6775)), 0);
                kassert_eq!(sys_chown(dir, 1, 1), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-chown-dir")?.mode, 0o6775);
                let _ = lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/meta-ktest-chown-dir", true);
                // chown follows the symlink; lchown and fchownat with
                // AT_SYMLINK_NOFOLLOW change the link itself.
                kassert_eq!(sys_chown(link, 3, 4), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (3, 4));
                kassert_eq!(owner("/tmp/meta-ktest-chown-link")?, (0, 0));
                kassert_eq!(sys_lchown(link, 5, 6), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown-link")?, (5, 6));
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (3, 4));
                let nofollow = flags(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_fchownat(Dirfd::Cwd, link, 7, u32::MAX, nofollow), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown-link")?, (7, 6));
                kassert_eq!(sys_fchownat(Dirfd::Cwd, link, u32::MAX, 8, flags(0)), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (3, 8));
                // fchownat relative to a dirfd, and AT_EMPTY_PATH on its file.
                let tmp = ktest_open(2, b"/tmp", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
                let rel = ktest_user_path(3, b"meta-ktest-chown")?;
                kassert_eq!(sys_fchownat(Dirfd::Fd(tmp), rel, 9, 10, flags(0)), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (9, 10));
                let opath = open(path, OpenFlags::O_PATH)?;
                let empty = ktest_user_path(4, b"")?;
                kassert_eq!(sys_fchownat(Dirfd::Fd(opath), empty, 11, 12, flags(AtFlags::AT_EMPTY_PATH)), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (11, 12));
                // fchown on an open descriptor; `O_PATH` or closed `-EBADF`.
                let fd = open(path, OpenFlags::O_RDONLY)?;
                kassert_eq!(sys_fchown(fd, 13, u32::MAX), 0);
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (13, 12));
                kassert_eq!(sys_fchown(opath, 0, 0), neg_errno(errno::EBADF));
                kassert_eq!(sys_fchown(at::reg(900), 0, 0), neg_errno(errno::EBADF));
                // Unknown flags `-EINVAL`; missing `-ENOENT`; read-only `-EROFS`.
                kassert_eq!(
                    sys_fchownat(Dirfd::Cwd, path, 0, 0, flags(AtFlags::AT_REMOVEDIR)),
                    neg_errno(errno::EINVAL)
                );
                kassert_eq!(owner("/tmp/meta-ktest-chown")?, (13, 12));
                let missing = ktest_user_path(5, b"/tmp/meta-ktest-no-such")?;
                kassert_eq!(sys_chown(missing, 0, 0), neg_errno(errno::ENOENT));
                let ro = ktest_user_path(6, b"/proc/version")?;
                kassert_eq!(sys_chown(ro, 0, 0), neg_errno(errno::EROFS));
                kassert_eq!(sys_lchown(ro, 0, 0), neg_errno(errno::EROFS));
                remove(&[b"/tmp/meta-ktest-chown", b"/tmp/meta-ktest-chown-link"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_umask_returns_the_old_mask_and_applies_to_openat_o_creat() {
            process::in_test_process(|_| {
                // The default 0o022 comes back; only `& 0o777` of the new one
                // is kept, whatever is above it.
                kassert_eq!(sys_umask(at::reg(0xffff_0000_0000_0000 | 0o7077)), 0o022);
                kassert_eq!(sys_umask(at::reg(0o077)), 0o077);
                kassert_eq!(process::current_umask(), 0o077);
                // A file `openat(O_CREAT)` makes gets `mode & !umask`.
                let rel = ktest_user_path(0, b"/tmp/meta-ktest-umask")?;
                let creat = OpenFlags::from_bits(OpenFlags::O_WRONLY | OpenFlags::O_CREAT);
                let fd = sys_openat(Dirfd::Cwd, rel, creat, at::reg(0o4777));
                kassert!(fd < lumen_vfs::MAX_FDS as u64);
                kassert_eq!(meta_of("/tmp/meta-ktest-umask")?.mode, 0o4700);
                remove(&[b"/tmp/meta-ktest-umask"]);
                kassert_eq!(sys_umask(at::reg(0)), 0o077);
                let fd = sys_openat(Dirfd::Cwd, rel, creat, at::reg(0o666));
                kassert!(fd < lumen_vfs::MAX_FDS as u64);
                kassert_eq!(meta_of("/tmp/meta-ktest-umask")?.mode, 0o666);
                remove(&[b"/tmp/meta-ktest-umask"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_utimensat_sets_omits_and_nows_by_path_fd_and_link() {
            process::in_test_process(|_| {
                let path = make_file(0, b"/tmp/meta-ktest-utime", b"x")?;
                symlink(b"meta-ktest-utime", b"/tmp/meta-ktest-utime-link")?;
                let link = ktest_user_path(1, b"/tmp/meta-ktest-utime-link")?;
                let times_at = process::KTEST_USER_VA + 2048;
                let times = |p: &str| meta_of(p).map(|m| (ts(m.atime), ts(m.mtime)));
                let omit = SetTime::UTIME_OMIT;
                let now = SetTime::UTIME_NOW;
                // Explicit times, and ctime moving past what it was.
                let before = meta_of("/tmp/meta-ktest-utime")?;
                wait_past(before.ctime)?;
                let t = put_times(times_at, [(1000, 5), (2000, 7)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((1000, 5), (2000, 7)));
                let after = meta_of("/tmp/meta-ktest-utime")?;
                kassert!(ts(after.ctime) > ts(before.ctime));
                // UTIME_OMIT keeps one, UTIME_NOW sets one to now.
                let t = put_times(times_at, [(3000, omit), (4000, 9)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((1000, 5), (4000, 9)));
                let floor = crate::time::realtime_timespec();
                let t = put_times(times_at, [(0, now), (0, omit)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                let (a, m) = times("/tmp/meta-ktest-utime")?;
                kassert!(a >= ts(floor));
                kassert_eq!(m, (4000, 9));
                // A NULL `times` sets both to now.
                let t = put_times(times_at, [(10, 0), (20, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                let floor = crate::time::realtime_timespec();
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, at::reg(0), flags(0)), 0);
                let (a, m) = times("/tmp/meta-ktest-utime")?;
                kassert!(a >= ts(floor) && m >= ts(floor));
                // A NULL path with a descriptor: `futimens` on it (an
                // `O_PATH` one `-EBADF`, any flag `-EINVAL`); with
                // AT_FDCWD, `-EFAULT`.
                let fd = open(path, OpenFlags::O_RDONLY)?;
                let null = at::reg(0);
                let t = put_times(times_at, [(30, 1), (40, 2)])?;
                kassert_eq!(sys_utimensat(Dirfd::Fd(fd), null, t, flags(0)), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((30, 1), (40, 2)));
                let opath = open(path, OpenFlags::O_PATH)?;
                kassert_eq!(sys_utimensat(Dirfd::Fd(opath), null, t, flags(0)), neg_errno(errno::EBADF));
                kassert_eq!(sys_utimensat(Dirfd::Fd(at::reg(900)), null, t, flags(0)), neg_errno(errno::EBADF));
                let nofollow = flags(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_utimensat(Dirfd::Fd(fd), null, t, nofollow), neg_errno(errno::EINVAL));
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, t, flags(0)), neg_errno(errno::EFAULT));
                // AT_EMPTY_PATH on an `O_PATH` descriptor's file.
                let empty = ktest_user_path(2, b"")?;
                let t = put_times(times_at, [(50, 3), (60, 4)])?;
                kassert_eq!(sys_utimensat(Dirfd::Fd(opath), empty, t, flags(AtFlags::AT_EMPTY_PATH)), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((50, 3), (60, 4)));
                // AT_SYMLINK_NOFOLLOW sets the link's own times; without it,
                // the target's.
                let t = put_times(times_at, [(70, 5), (80, 6)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, link, t, nofollow), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime-link")?, ((70, 5), (80, 6)));
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((50, 3), (60, 4)));
                let t = put_times(times_at, [(90, 7), (100, 8)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, link, t, flags(0)), 0);
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((90, 7), (100, 8)));
                kassert_eq!(times("/tmp/meta-ktest-utime-link")?, ((70, 5), (80, 6)));
                // An invalid `tv_nsec` in either: `-EINVAL`, nothing changed.
                for bad in [[(1, 1_000_000_000), (2, 0)], [(1, 0), (2, -1)], [(1, now + 1), (2, 0)]] {
                    let t = put_times(times_at, bad)?;
                    kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), neg_errno(errno::EINVAL));
                }
                kassert_eq!(times("/tmp/meta-ktest-utime")?, ((90, 7), (100, 8)));
                // ... checked after the path and descriptor, as Linux does.
                let t = put_times(times_at, [(1, 1_000_000_000), (2, 0)])?;
                let gone = ktest_user_path(5, b"/tmp/meta-ktest-no-such")?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, gone, t, flags(0)), neg_errno(errno::ENOENT));
                kassert_eq!(sys_utimensat(Dirfd::Fd(at::reg(900)), null, t, flags(0)), neg_errno(errno::EBADF));
                // Unknown flags `-EINVAL`; a bad `times` pointer `-EFAULT`.
                let t = put_times(times_at, [(1, 0), (2, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(AtFlags::AT_REMOVEDIR)), neg_errno(errno::EINVAL));
                let bad_ptr = at::reg(0xffff_8000_0000_0000);
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, bad_ptr, flags(0)), neg_errno(errno::EFAULT));
                // Both UTIME_OMIT: done before the path is looked at (a
                // missing one too), and ctime left alone.
                let ctime = meta_of("/tmp/meta-ktest-utime")?.ctime;
                wait_past(ctime)?;
                let t = put_times(times_at, [(0, omit), (0, omit)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-utime")?.ctime, ctime);
                let missing = ktest_user_path(3, b"/tmp/meta-ktest-no-such")?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, missing, t, flags(0)), 0);
                let t = put_times(times_at, [(1, 0), (2, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, missing, t, flags(0)), neg_errno(errno::ENOENT));
                // A read-only mount: `-EROFS`.
                let ro = ktest_user_path(4, b"/proc/version")?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, ro, at::reg(0), flags(0)), neg_errno(errno::EROFS));
                // ... after an invalid `tv_nsec` (`-EINVAL` first, as Linux).
                let t = put_times(times_at, [(1, -5), (2, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, ro, t, flags(0)), neg_errno(errno::EINVAL));
                remove(&[b"/tmp/meta-ktest-utime", b"/tmp/meta-ktest-utime-link"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_truncate_resizes_zeroing_the_tail_and_updates_mtime_and_ctime() {
            process::in_test_process(|_| {
                let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8 | 1).collect();
                let path = make_file(0, b"/tmp/meta-ktest-trunc", &data)?;
                let f = inode_of("/tmp/meta-ktest-trunc")?;
                let read = |len: usize| -> Result<Vec<u8>, &'static str> {
                    let mut buf = vec![0u8; len];
                    let n = f.read_at(0, &mut buf).map_err(|_| "read_at")?;
                    buf.truncate(n);
                    Ok(buf)
                };
                // Old times, so the truncate's mtime and ctime are later.
                let t = put_times(process::KTEST_USER_VA + 2048, [(1000, 0), (1000, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                let before = meta_of("/tmp/meta-ktest-trunc")?;
                wait_past(before.ctime)?;
                // Shrink into page 0 by path, then grow again by descriptor:
                // what was past the cut reads back as zeros, in the shrunk
                // page and in the page that was dropped.
                kassert_eq!(sys_truncate(path, 100), 0);
                let after = meta_of("/tmp/meta-ktest-trunc")?;
                kassert_eq!(after.size, 100);
                kassert!(ts(after.mtime) > (1000, 0) && ts(after.ctime) > ts(before.ctime));
                kassert!(read(200)? == data[..100]);
                let fd = open(path, OpenFlags::O_RDWR)?;
                kassert_eq!(sys_ftruncate(fd, 2 * P + 5), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-trunc")?.size, 2 * P + 5);
                let back = read(3 * P as usize)?;
                kassert_eq!(back.len(), 2 * P as usize + 5);
                kassert!(back[..100] == data[..100]);
                kassert!(back[100..].iter().all(|&b| b == 0));
                // ftruncate's own mtime: past an old one set again.
                let t = put_times(process::KTEST_USER_VA + 2048, [(1000, 0), (1000, 0)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, path, t, flags(0)), 0);
                kassert_eq!(sys_ftruncate(fd, 7), 0);
                kassert!(ts(meta_of("/tmp/meta-ktest-trunc")?.mtime) > (1000, 0));
                // truncate follows a symlink.
                symlink(b"meta-ktest-trunc", b"/tmp/meta-ktest-trunc-link")?;
                let link = ktest_user_path(1, b"/tmp/meta-ktest-trunc-link")?;
                kassert_eq!(sys_truncate(link, 3), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-trunc")?.size, 3);
                remove(&[b"/tmp/meta-ktest-trunc", b"/tmp/meta-ktest-trunc-link"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_truncate_errnos() {
            use crate::vfs::page_cache::ktest_files::Ext4Volume;
            process::in_test_process(|_| {
                let path = make_file(0, b"/tmp/meta-ktest-terr", b"0123456789")?;
                let size = || meta_of("/tmp/meta-ktest-terr").map(|m| m.size);
                // A negative length: `-EINVAL`, before the path or fd is
                // looked at.
                let neg = (-1i64) as u64;
                kassert_eq!(sys_truncate(path, neg), neg_errno(errno::EINVAL));
                kassert_eq!(sys_truncate(path, i64::MIN as u64), neg_errno(errno::EINVAL));
                let missing = ktest_user_path(1, b"/tmp/meta-ktest-no-such")?;
                kassert_eq!(sys_truncate(missing, neg), neg_errno(errno::EINVAL));
                kassert_eq!(sys_truncate(missing, 0), neg_errno(errno::ENOENT));
                let rdwr = open(path, OpenFlags::O_RDWR)?;
                kassert_eq!(sys_ftruncate(rdwr, neg), neg_errno(errno::EINVAL));
                kassert_eq!(sys_ftruncate(at::reg(900), neg), neg_errno(errno::EINVAL));
                kassert_eq!(size()?, 10);
                // A directory: `-EISDIR` by path, `-EINVAL` by descriptor
                // (never open for writing); a device `-EINVAL`.
                let tmp = ktest_user_path(2, b"/tmp")?;
                kassert_eq!(sys_truncate(tmp, 0), neg_errno(errno::EISDIR));
                let dir_fd = open(tmp, OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
                kassert_eq!(sys_ftruncate(dir_fd, 0), neg_errno(errno::EINVAL));
                let null = ktest_user_path(3, b"/dev/null")?;
                kassert_eq!(sys_truncate(null, 0), neg_errno(errno::EINVAL));
                let null_fd = open(null, OpenFlags::O_RDWR)?;
                kassert_eq!(sys_ftruncate(null_fd, 0), neg_errno(errno::EINVAL));
                // Not open for writing `-EINVAL`; `O_PATH` or closed `-EBADF`.
                let rd = open(path, OpenFlags::O_RDONLY)?;
                kassert_eq!(sys_ftruncate(rd, 0), neg_errno(errno::EINVAL));
                let opath = open(path, OpenFlags::O_PATH)?;
                kassert_eq!(sys_ftruncate(opath, 0), neg_errno(errno::EBADF));
                kassert_eq!(sys_ftruncate(at::reg(900), 0), neg_errno(errno::EBADF));
                kassert_eq!(size()?, 10);
                // Write-only is enough.
                let wr = open(path, OpenFlags::O_WRONLY)?;
                kassert_eq!(sys_ftruncate(wr, 4), 0);
                kassert_eq!(size()?, 4);
                // A read-only mount: `-EROFS` by path (procfs).
                let ro = ktest_user_path(4, b"/proc/version")?;
                kassert_eq!(sys_truncate(ro, 0), neg_errno(errno::EROFS));
                // On ext4: past the largest file `-EFBIG`, through a
                // descriptor installed on a scratch volume's file; on its
                // read-only mount, `-EROFS`.
                let vol = Ext4Volume::new(200)?;
                // Each inode a temporary, dropped at the end of its install's
                // statement, so closing `fd` drops the inode's last reference.
                let fd = ktest_install(
                    &vol.root.create(b"big", 0o644).map_err(|_| "create on ext4")?,
                    b"/big-rw",
                    OpenFlags::O_RDWR,
                )?;
                kassert_eq!(sys_ftruncate(fd, i64::MAX as u64), neg_errno(errno::EFBIG));
                kassert_eq!(sys_ftruncate(fd, 5000), 0);
                kassert_eq!(fd.file().map(|f| f.inode.stat().map(|m| m.size)), Ok(Ok(5000)));
                kassert_eq!(sys_close(fd), 0);
                let ro_root = vol.remount_ro(201)?;
                let fd = ktest_install(
                    &ro_root.lookup(b"big").map_err(|_| "lookup on the ro mount")?,
                    b"/big-ro",
                    OpenFlags::O_RDWR,
                )?;
                kassert_eq!(sys_ftruncate(fd, 0), neg_errno(errno::EROFS));
                kassert_eq!(sys_close(fd), 0);
                remove(&[b"/tmp/meta-ktest-terr"]);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_chown_on_ext4_drops_set_id_bits_in_its_one_write_and_a_failed_one_changes_nothing() {
            // ext4's `set_owner` changes the owner and drops setuid (and
            // group-exec setgid) in one disk write: with the second write
            // armed to fail, the chown still succeeds, both applied, one
            // write made (a two-step chown fails there, owner changed and
            // setuid kept), and a read-only remount sees both. With every
            // write failing, the chown is `-EIO` and changes nothing, in
            // memory or on disk.
            use crate::vfs::ext4::ktest_util::remount_ro;
            use crate::vfs::page_cache::ktest_files::fault_ext4;
            use core::sync::atomic::Ordering;
            use lumen_vfs::FileSystem;
            process::in_test_process(|_| {
                let m = fault_ext4(203)?;
                let f = m.root.create(b"suid", 0o644).map_err(|_| "create on ext4")?;
                f.set_mode(0o4755).map_err(|_| "set_mode")?;
                let fd = ktest_install(&f, b"/suid", OpenFlags::O_RDWR)?;
                let disk = |minor| {
                    remount_ro(&m, minor)?.root_inode().lookup(b"suid").and_then(|i| i.stat()).map_err(|_| "remount")
                };
                m.cache.arm_write(2);
                let chown = sys_fchown(fd, 5, u32::MAX);
                let writes = m.cache.writes.load(Ordering::Acquire);
                m.cache.clear();
                kassert_eq!(chown, 0);
                kassert_eq!(writes, 1);
                let st = f.stat().map_err(|_| "stat")?;
                kassert_eq!((st.uid, st.gid, st.mode), (5, 0, 0o0755));
                let d = disk(204)?;
                kassert_eq!((d.uid, d.gid, d.mode), (5, 0, 0o0755));
                // Every write failing: `-EIO`, and nothing changed.
                f.set_mode(0o4755).map_err(|_| "set_mode")?;
                let before = f.stat().map_err(|_| "stat")?;
                m.cache.fail_writes.store(true, Ordering::Release);
                let failed = sys_fchown(fd, 7, 8);
                m.cache.clear();
                kassert_eq!(failed, neg_errno(errno::EIO));
                let after = f.stat().map_err(|_| "stat")?;
                kassert_eq!((after.uid, after.gid, after.mode), (5, 0, 0o4755));
                kassert_eq!(after.ctime, before.ctime);
                let d = disk(205)?;
                kassert_eq!((d.uid, d.gid, d.mode), (5, 0, 0o4755));
                kassert_eq!(sys_close(fd), 0);
                Ok(())
            })?
        }
    );

    /// Map `f` (3 pages) shared and writable in the current process, fault
    /// every page in, then run `shrink` (to `P + 10`) and check that the
    /// pages wholly past the new end are gone from the page tables (a copy
    /// there `-EFAULT`s, a fault is a bus error) while the page holding the
    /// end reads back, its tail zero through the mapping; a later grow maps
    /// a fresh zero page.
    fn shrink_unmaps(f: &Arc<dyn VfsInode>, shrink: impl FnOnce() -> u64) -> Result<(), &'static str> {
        use crate::user::copy_from_user;
        let space = crate::sched::current_address_space().ok_or("no address space")?;
        let file = crate::mm::user_vm::file_vm_open(f)?;
        let rw = Prot::READ | Prot::WRITE;
        let va = space.mmap_file(MapPlace::Hint(None), 3 * P, rw, file, 0, Sharing::Shared).map_err(|_| "mmap_file")?;
        let mut byte = [0u8; 1];
        for i in 0..3 {
            kassert_eq!(copy_from_user(&mut byte, va + i * P + 200), Ok(()));
            kassert!(byte[0] != 0);
        }
        kassert_eq!(copy_to_user(va + 2 * P, b"w"), Ok(())); // writable, counted
        kassert_eq!(shrink(), 0);
        kassert_eq!(f.stat().map(|m| m.size), Ok(P + 10));
        kassert_eq!(copy_from_user(&mut byte, va + 200), Ok(()));
        kassert!(byte[0] != 0);
        kassert_eq!(copy_from_user(&mut byte, va + P + 9), Ok(()));
        kassert!(byte[0] != 0);
        kassert_eq!(copy_from_user(&mut byte, va + P + 200), Ok(()));
        kassert_eq!(byte[0], 0);
        kassert_eq!(copy_from_user(&mut byte, va + 2 * P + 200), Err(CopyError::Fault));
        kassert_eq!(space.handle_page_fault(va + 2 * P, 0), FaultResult::BusError);
        f.truncate(3 * P).map_err(|_| "grow")?;
        kassert_eq!(copy_from_user(&mut byte, va + 2 * P), Ok(()));
        kassert_eq!(byte[0], 0);
        space.munmap(va, 3 * P).map_err(|_| "munmap")?;
        Ok(())
    }

    crate::ktest!(
        fn meta_truncate_shrink_unmaps_mapped_pages_past_the_new_end() {
            use crate::vfs::page_cache::ktest_files::Ext4Volume;
            process::in_test_process(|_| {
                let data: Vec<u8> = (0..3 * P as usize).map(|i| (i % 251) as u8 | 1).collect();
                // tmpfs, by path (`truncate`).
                let path = make_file(0, b"/tmp/meta-ktest-map", &data)?;
                let f = inode_of("/tmp/meta-ktest-map")?;
                shrink_unmaps(&f, || sys_truncate(path, P + 10))
                    .inspect_err(|_| crate::ktest::diag(format_args!("truncate on tmpfs")))?;
                remove(&[b"/tmp/meta-ktest-map"]);
                // ext4, by descriptor (`ftruncate`).
                let vol = Ext4Volume::new(202)?;
                let f = vol.root.create(b"map", 0o644).map_err(|_| "create on ext4")?;
                kassert_eq!(f.write_at(0, &data), Ok(data.len()));
                let fd = ktest_install(&f, b"/map", OpenFlags::O_RDWR)?;
                shrink_unmaps(&f, || sys_ftruncate(fd, P + 10))
                    .inspect_err(|_| crate::ktest::diag(format_args!("ftruncate on ext4")))?;
                kassert_eq!(sys_close(fd), 0);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_handlers_take_their_registers_in_order() {
            // Through `dispatch`, each argument in its own register with
            // distinct values (garbage above an `int`'s 32 bits, and in
            // `fchmodat`'s unused `r10`), so an arm passing them out of order
            // can't pass.
            use crate::arch::syscall::SyscallFrame;
            process::in_test_process(|_| {
                let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64| {
                    let mut frame =
                        SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, r8, ..SyscallFrame::default() };
                    let _ = super::super::dispatch(&mut frame);
                    frame.rax
                };
                let high = 0xdead_beef_0000_0000u64;
                let path = make_file(0, b"/tmp/meta-ktest-regs", b"0123456789")?.addr();
                let tmp = ktest_open(1, b"/tmp", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
                let rel = ktest_user_path(2, b"meta-ktest-regs")?.addr();
                let fd = open(at::reg(path), OpenFlags::O_RDWR)?;
                let fdr = fd.raw() as u64;
                let dirfd = tmp.raw() as u64;
                let mode = || meta_of("/tmp/meta-ktest-regs").map(|m| m.mode);
                let owner = || meta_of("/tmp/meta-ktest-regs").map(|m| (m.uid, m.gid));
                kassert_eq!(call(general::__NR_chmod, path, high | 0o601, 0, 0, 0), 0);
                kassert_eq!(mode()?, 0o601);
                kassert_eq!(call(general::__NR_fchmod, high | fdr, high | 0o602, 0, 0, 0), 0);
                kassert_eq!(mode()?, 0o602);
                // r10 holds AT_REMOVEDIR: never read by the 3-argument call.
                let removedir = u64::from(AtFlags::AT_REMOVEDIR);
                kassert_eq!(call(general::__NR_fchmodat, high | dirfd, rel, high | 0o603, removedir, 0), 0);
                kassert_eq!(mode()?, 0o603);
                let nofollow = u64::from(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(call(general::__NR_fchmodat2, high | dirfd, rel, high | 0o604, high | nofollow, 0), 0);
                kassert_eq!(mode()?, 0o604);
                kassert_eq!(call(general::__NR_fchmodat2, dirfd, rel, 0o605, removedir, 0), neg_errno(errno::EINVAL));
                kassert_eq!(call(general::__NR_chown, path, high | 11, high | 12, 0, 0), 0);
                kassert_eq!(owner()?, (11, 12));
                kassert_eq!(call(general::__NR_fchown, high | fdr, 13, 14, 0, 0), 0);
                kassert_eq!(owner()?, (13, 14));
                kassert_eq!(call(general::__NR_lchown, path, 15, 16, 0, 0), 0);
                kassert_eq!(owner()?, (15, 16));
                kassert_eq!(call(general::__NR_fchownat, high | dirfd, rel, high | 17, high | 18, high | nofollow), 0);
                kassert_eq!(owner()?, (17, 18));
                kassert_eq!(call(general::__NR_fchownat, dirfd, rel, 19, 20, removedir), neg_errno(errno::EINVAL));
                kassert_eq!(call(general::__NR_umask, high | 0o027, 0, 0, 0, 0), 0o022);
                kassert_eq!(call(general::__NR_umask, 0o022, 0, 0, 0, 0), 0o027);
                let t = put_times(process::KTEST_USER_VA + 2048, [(21, 22), (23, 24)])?.addr();
                kassert_eq!(call(general::__NR_utimensat, high | dirfd, rel, t, high | nofollow, 0), 0);
                let m = meta_of("/tmp/meta-ktest-regs")?;
                kassert_eq!((ts(m.atime), ts(m.mtime)), ((21, 22), (23, 24)));
                kassert_eq!(call(general::__NR_truncate, path, 7, 0, 0, 0), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-regs")?.size, 7);
                kassert_eq!(call(general::__NR_ftruncate, high | fdr, 3, 0, 0, 0), 0);
                kassert_eq!(meta_of("/tmp/meta-ktest-regs")?.size, 3);
                remove(&[b"/tmp/meta-ktest-regs"]);
                Ok(())
            })?
        }
    );

    /// `/dev/null`'s mode, owner and times as a ktest found them, put back
    /// by `restore` (which each ktest asserts at its end), or by the drop
    /// if the ktest returned early, so other tests and the boot checks see
    /// the node as devfs made it. Its ctime stays moved.
    struct DevNullRestore {
        inode: Arc<dyn VfsInode>,
        meta: Metadata,
        restored: bool,
    }

    impl DevNullRestore {
        fn take() -> Result<Self, &'static str> {
            let inode = inode_of("/dev/null")?;
            let meta = inode.stat().map_err(|_| "stat")?;
            Ok(Self { inode, meta, restored: false })
        }

        /// Put the metadata back, owner first (a chown's
        /// `mode_after_chown` can't strip the restored mode), then check
        /// `stat` shows it.
        fn restore(&mut self) -> Result<(), &'static str> {
            let m = &self.meta;
            self.inode.set_owner(Some(m.uid), Some(m.gid)).map_err(|_| "restore the owner")?;
            self.inode.set_mode(m.mode).map_err(|_| "restore the mode")?;
            self.inode.set_times(SetTime::Set(m.atime), SetTime::Set(m.mtime)).map_err(|_| "restore the times")?;
            let now = self.inode.stat().map_err(|_| "stat after the restore")?;
            if (now.mode, now.uid, now.gid, now.atime, now.mtime) != (m.mode, m.uid, m.gid, m.atime, m.mtime) {
                return Err("/dev/null's metadata not as it was");
            }
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for DevNullRestore {
        /// The backstop for a ktest that returned early (its failure is
        /// already reported): a failed restore here is only warned about.
        fn drop(&mut self) {
            if !self.restored
                && let Err(e) = self.restore()
            {
                log::warn!(target: "ktest", "/dev/null's metadata not restored: {e}");
            }
        }
    }

    crate::ktest!(
        fn meta_chmod_on_dev_null_is_kept_as_devtmpfs_does() {
            process::in_test_process(|_| {
                let mut restore = DevNullRestore::take()?;
                let null = ktest_user_path(0, b"/dev/null")?;
                // devfs is a writable mount (#301): the new mode is kept,
                // set-id and sticky bits too, `stat` shows it, ctime moves.
                let before = meta_of("/dev/null")?;
                kassert_eq!(before.mode, 0o666);
                wait_past(before.ctime)?;
                kassert_eq!(sys_chmod(null, at::reg(0o600)), 0);
                let after = meta_of("/dev/null")?;
                kassert_eq!(after.mode, 0o600);
                kassert_eq!(after.inode_type, InodeType::CharDevice);
                kassert!(ts(after.ctime) > ts(before.ctime));
                // fchmod on a descriptor, fchmodat2 with AT_SYMLINK_NOFOLLOW.
                let fd = open(null, OpenFlags::O_WRONLY)?;
                kassert_eq!(sys_fchmod(fd, at::reg(0o640)), 0);
                kassert_eq!(meta_of("/dev/null")?.mode, 0o640);
                let nofollow = flags(AtFlags::AT_SYMLINK_NOFOLLOW);
                kassert_eq!(sys_fchmodat2(Dirfd::Cwd, null, at::reg(0o604), nofollow), 0);
                kassert_eq!(meta_of("/dev/null")?.mode, 0o604);
                kassert_eq!(sys_chmod(null, at::reg(0o7777)), 0);
                kassert_eq!(meta_of("/dev/null")?.mode, 0o7777);
                kassert_eq!(sys_close(fd), 0);
                // `/dev` itself: `-EROFS` (devfs's setter refuses it; a
                // direct call too: `devfs_nodes_start_with_their_boot_metadata`).
                let dev = ktest_user_path(1, b"/dev")?;
                kassert_eq!(sys_chmod(dev, at::reg(0o700)), neg_errno(errno::EROFS));
                kassert_eq!(meta_of("/dev")?.mode, 0o755);
                kassert_eq!(restore.restore(), Ok(()));
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_chown_on_dev_null_is_kept_as_devtmpfs_does() {
            process::in_test_process(|_| {
                let mut restore = DevNullRestore::take()?;
                let null = ktest_user_path(0, b"/dev/null")?;
                let owner = || meta_of("/dev/null").map(|m| (m.uid, m.gid));
                let before = meta_of("/dev/null")?;
                kassert_eq!((before.uid, before.gid), (0, 0));
                wait_past(before.ctime)?;
                kassert_eq!(sys_chown(null, 1000, 2000), 0);
                kassert_eq!(owner()?, (1000, 2000));
                kassert!(ts(meta_of("/dev/null")?.ctime) > ts(before.ctime));
                // -1 leaves an id as it is; a chown drops setuid on a
                // non-directory (`mode_after_chown`), in devfs too.
                kassert_eq!(sys_chmod(null, at::reg(0o4755)), 0);
                kassert_eq!(meta_of("/dev/null")?.mode, 0o4755);
                kassert_eq!(sys_chown(null, u32::MAX, 3000), 0);
                kassert_eq!(owner()?, (1000, 3000));
                kassert_eq!(meta_of("/dev/null")?.mode, 0o755);
                // fchown on a descriptor, lchown on the path.
                let fd = open(null, OpenFlags::O_RDONLY)?;
                kassert_eq!(sys_fchown(fd, 4000, u32::MAX), 0);
                kassert_eq!(owner()?, (4000, 3000));
                kassert_eq!(sys_close(fd), 0);
                kassert_eq!(sys_lchown(null, 5000, 6000), 0);
                kassert_eq!(owner()?, (5000, 6000));
                // `/dev` itself: `-EROFS`.
                let dev = ktest_user_path(1, b"/dev")?;
                kassert_eq!(sys_chown(dev, 1, 1), neg_errno(errno::EROFS));
                kassert_eq!(meta_of("/dev").map(|m| (m.uid, m.gid))?, (0, 0));
                kassert_eq!(restore.restore(), Ok(()));
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn meta_utimensat_on_dev_null_is_kept_as_devtmpfs_does() {
            process::in_test_process(|_| {
                let mut restore = DevNullRestore::take()?;
                let null = ktest_user_path(0, b"/dev/null")?;
                let times_at = process::KTEST_USER_VA + 2048;
                let times = |p: &str| meta_of(p).map(|m| (ts(m.atime), ts(m.mtime)));
                let before = meta_of("/dev/null")?;
                wait_past(before.ctime)?;
                let t = put_times(times_at, [(1000, 5), (2000, 7)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, t, flags(0)), 0);
                kassert_eq!(times("/dev/null")?, ((1000, 5), (2000, 7)));
                kassert!(ts(meta_of("/dev/null")?.ctime) > ts(before.ctime));
                // UTIME_OMIT keeps one; a NULL `times` (touch) sets both to now.
                let t = put_times(times_at, [(0, SetTime::UTIME_OMIT), (4000, 9)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, t, flags(0)), 0);
                kassert_eq!(times("/dev/null")?, ((1000, 5), (4000, 9)));
                let t = put_times(times_at, [(5000, 1), (0, SetTime::UTIME_OMIT)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, t, flags(0)), 0);
                kassert_eq!(times("/dev/null")?, ((5000, 1), (4000, 9)));
                let floor = crate::time::realtime_timespec();
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, at::reg(0), flags(0)), 0);
                let (a, m) = times("/dev/null")?;
                kassert!(a >= ts(floor) && m >= ts(floor));
                // futimens: a NULL path with a descriptor.
                let fd = open(null, OpenFlags::O_WRONLY)?;
                let t = put_times(times_at, [(30, 1), (40, 2)])?;
                kassert_eq!(sys_utimensat(Dirfd::Fd(fd), at::reg(0), t, flags(0)), 0);
                kassert_eq!(times("/dev/null")?, ((30, 1), (40, 2)));
                kassert_eq!(sys_close(fd), 0);
                // `/dev` itself: `-EROFS`, its times as they were.
                let dev = ktest_user_path(1, b"/dev")?;
                let dev_times = times("/dev")?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, dev, t, flags(0)), neg_errno(errno::EROFS));
                kassert_eq!(times("/dev")?, dev_times);
                kassert_eq!(restore.restore(), Ok(()));
                Ok(())
            })?
        }
    );

    /// The wrapped filesystem mounted read-only, with its inodes and device
    /// number: a read-only filesystem holding special files (none of
    /// Lumen's boot mounts is one since devfs's mounts are writable, #301).
    /// It forwards only `FileSystem`'s required methods: `with_rename_lock`
    /// keeps the default, so wrap only a filesystem that keeps it too.
    struct ReadOnlyMount(Arc<dyn lumen_vfs::FileSystem>);

    impl lumen_vfs::FileSystem for ReadOnlyMount {
        fn dev_id(&self) -> lumen_vfs::DevId {
            self.0.dev_id()
        }

        fn name(&self) -> &'static str {
            self.0.name()
        }

        fn root_inode(&self) -> Arc<dyn VfsInode> {
            self.0.root_inode()
        }

        fn sync(&self) -> Result<(), lumen_vfs::VfsError> {
            self.0.sync()
        }

        fn read_only(&self) -> bool {
            true
        }
    }

    /// Mount `fs` on `mnt`, a new directory (under `/tmp`), run `f` in a
    /// test process, then unmount `fs` and remove `mnt`, whatever `f`
    /// returned.
    fn in_a_mount(
        mnt: &str,
        fs: Arc<dyn lumen_vfs::FileSystem>,
        f: impl FnOnce() -> Result<(), &'static str> + Send + 'static,
    ) -> Result<(), &'static str> {
        let rmdir = || lumen_vfs::unlink_at(tables(), None, mnt.as_bytes(), true);
        let mp = lumen_vfs::mkdir_at(tables(), None, mnt.as_bytes(), 0o755, 0).map_err(|_| "mkdir")?;
        let (mp_dev, mp_ino) = (mp.dev_id(), mp.ino());
        if let Err(e) = tables().mount(mp, mnt, fs) {
            log::warn!(target: "ktest", "{mnt}: mount failed: {e:?}");
            if let Err(e) = rmdir() {
                log::warn!(target: "ktest", "{mnt}: rmdir failed: {e:?}");
            }
            return Err("mount");
        }
        let ran = process::in_test_process(|_| f());
        let umounted = tables().umount(mp_dev, mp_ino).map(drop);
        let removed = rmdir();
        // Only the first failure is returned: every cleanup failure is
        // logged with its errno, so none is lost (the mount may stay at
        // `mnt`).
        for (step, r) in [("umount", umounted), ("rmdir", removed)] {
            if let Err(e) = r {
                log::warn!(target: "ktest", "{mnt}: {step} failed: {e:?}");
            }
        }
        ran??;
        umounted.map_err(|_| "umount")?;
        removed.map_err(|_| "rmdir")
    }

    crate::ktest!(
        fn meta_a_special_file_on_another_read_only_mount_stays_erofs() {
            // A read-only mount whose char nodes' setters take a change
            // (checked first): only `writable_mount` refuses it here.
            const NULL: &str = "/tmp/meta-ktest-ro-dev/null";
            let devfs = crate::vfs::devfs::Devfs::new(lumen_vfs::DevId::makedev(0, 133));
            in_a_mount("/tmp/meta-ktest-ro-dev", Arc::new(ReadOnlyMount(devfs)), || {
                let null = ktest_user_path(0, NULL.as_bytes())?;
                let before = meta_of(NULL)?;
                kassert_eq!(inode_of(NULL)?.set_mode(before.mode), Ok(()));
                kassert_eq!(before.inode_type, InodeType::CharDevice);
                kassert!(tables().fs_read_only(before.dev));
                kassert_eq!(sys_chmod(null, at::reg(0o600)), neg_errno(errno::EROFS));
                kassert_eq!(sys_chown(null, 1000, 1000), neg_errno(errno::EROFS));
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, at::reg(0), flags(0)), neg_errno(errno::EROFS));
                // A write open is still fine: a special file (#291).
                let fd = open(null, OpenFlags::O_WRONLY)?;
                kassert_eq!(sys_fchmod(fd, at::reg(0o600)), neg_errno(errno::EROFS));
                kassert_eq!(sys_close(fd), 0);
                let after = meta_of(NULL)?;
                kassert_eq!((after.mode, after.uid, after.gid), (before.mode, before.uid, before.gid));
                kassert_eq!((ts(after.atime), ts(after.mtime)), (ts(before.atime), ts(before.mtime)));
                Ok(())
            })
        }
    );

    crate::ktest!(
        fn meta_a_second_devfs_mount_takes_chmod_chown_and_utimensat() {
            // Every devfs mount is writable, as every devtmpfs mount on
            // Linux (#301), not only `/dev`'s: a second instance's nodes
            // take the changes, by path (and `fchmod` by descriptor), and
            // `/dev`'s own nodes keep theirs.
            const MNT: &str = "/tmp/meta-ktest-dev2";
            const NULL: &str = "/tmp/meta-ktest-dev2/null";
            let devfs = crate::vfs::devfs::Devfs::new(lumen_vfs::DevId::makedev(0, 135));
            in_a_mount(MNT, devfs, || {
                let null = ktest_user_path(0, NULL.as_bytes())?;
                let before = meta_of(NULL)?;
                let dev_null = meta_of("/dev/null")?;
                kassert!(!tables().fs_read_only(before.dev));
                kassert!(crate::vfs::list_mounts().iter().any(|m| m.mount_path == MNT && !m.read_only));
                kassert_eq!(sys_chmod(null, at::reg(0o600)), 0);
                kassert_eq!(meta_of(NULL)?.mode, 0o600);
                kassert_eq!(sys_chown(null, 1000, 2000), 0);
                let t = put_times(process::KTEST_USER_VA + 2048, [(1000, 5), (2000, 7)])?;
                kassert_eq!(sys_utimensat(Dirfd::Cwd, null, t, flags(0)), 0);
                let fd = open(null, OpenFlags::O_WRONLY)?;
                kassert_eq!(sys_fchmod(fd, at::reg(0o640)), 0);
                kassert_eq!(sys_close(fd), 0);
                let after = meta_of(NULL)?;
                kassert_eq!((after.mode, after.uid, after.gid), (0o640, 1000, 2000));
                kassert_eq!((ts(after.atime), ts(after.mtime)), ((1000, 5), (2000, 7)));
                let dev_null_after = meta_of("/dev/null")?;
                kassert_eq!(
                    (dev_null_after.mode, dev_null_after.uid, dev_null_after.gid),
                    (dev_null.mode, dev_null.uid, dev_null.gid)
                );
                // Its root refuses, as `/dev`'s does.
                let root = ktest_user_path(1, MNT.as_bytes())?;
                kassert_eq!(sys_chmod(root, at::reg(0o700)), neg_errno(errno::EROFS));
                Ok(())
            })
        }
    );
}
