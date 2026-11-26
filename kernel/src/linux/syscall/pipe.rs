//! Pipes, `dup*` and `mknod*` (M8 C7, ARCHITECTURE sections 8 and 10):
//! `pipe`, `pipe2`, `dup`, `dup2`, `dup3`, `mknod`, `mknodat`.

use super::at::{self, Dirfd, Fd, Mode, UserPath, UserPtr};
use super::fd::vfs;
use super::{copy_pod_to_user, finish};
use crate::linux::{errno, general, pipe, process};
use crate::user::CopyError;
use lumen_vfs::{DevId, InodeType, OpenFlags};

/// `pipe(fds)` (22): `pipe2(fds, 0)`.
pub(super) fn sys_pipe(fds: UserPtr) -> u64 {
    finish(pipe2(fds, OpenFlags::from_bits(0)))
}

/// `pipe2(fds, flags)` (293): create an anonymous pipe (`O_CLOEXEC` and
/// `O_NONBLOCK` accepted; `O_DIRECT` and any other flag `-EINVAL`).
/// Deviation: Linux supports O_DIRECT packet-mode pipes; Lumen does not.
pub(super) fn sys_pipe2(fds: UserPtr, flags: OpenFlags) -> u64 {
    finish(pipe2(fds, flags))
}

fn pipe2(fds: UserPtr, flags: OpenFlags) -> Result<u64, u32> {
    let allowed = OpenFlags::O_CLOEXEC | OpenFlags::O_NONBLOCK;
    if flags.0 & !allowed != 0 {
        return Err(errno::EINVAL);
    }
    let r_slot = process::reserve_current_fd(0).map_err(vfs)?;
    let w_slot = process::reserve_current_fd(0).map_err(vfs)?;
    let (read_file, write_file) = pipe::new_pipe(flags.has_nonblock())?;
    let pair = [r_slot.fd(), w_slot.fd()];
    // Copy the pair to user space before installing either descriptor so a
    // faulting pointer leaves no descriptor visible in the table.
    // SAFETY: `[i32; 2]` is 8 bytes with no padding.
    unsafe { copy_pod_to_user(fds.addr(), &pair) }.map_err(CopyError::to_errno)?;
    let cloexec = flags.has_cloexec();
    let _ = r_slot.install(read_file, cloexec);
    let _ = w_slot.install(write_file, cloexec);
    Ok(0)
}

/// `dup(oldfd)` (32): lowest free descriptor, `FD_CLOEXEC` clear.
pub(super) fn sys_dup(oldfd: Fd) -> u64 {
    finish(process::dup_current_fd_from(oldfd.raw(), 0, false).map(|new| new as u64).map_err(vfs))
}

/// `dup2(oldfd, newfd)` (33): duplicate onto `newfd` (`FD_CLOEXEC` clear;
/// `oldfd == newfd` is a no-op if open; reserved `newfd` is `-EBUSY`).
pub(super) fn sys_dup2(oldfd: Fd, newfd: Fd) -> u64 {
    finish(process::dup_current_fd(oldfd.raw(), newfd.raw(), false).map(|new| new as u64).map_err(vfs))
}

/// `dup3(oldfd, newfd, flags)` (292): `dup2` with `O_CLOEXEC` from `flags`,
/// rejecting unknown flags or `oldfd == newfd` with `-EINVAL`.
pub(super) fn sys_dup3(oldfd: Fd, newfd: Fd, flags: OpenFlags) -> u64 {
    finish(dup3(oldfd, newfd, flags))
}

fn dup3(oldfd: Fd, newfd: Fd, flags: OpenFlags) -> Result<u64, u32> {
    if flags.0 & !OpenFlags::O_CLOEXEC != 0 {
        return Err(errno::EINVAL);
    }
    if oldfd.raw() == newfd.raw() {
        return Err(errno::EINVAL);
    }
    process::dup_current_fd(oldfd.raw(), newfd.raw(), flags.has_cloexec()).map(|new| new as u64).map_err(vfs)
}

/// `mknod(path, mode, dev)` (133): `mknodat(AT_FDCWD, path, mode, dev)`.
pub(super) fn sys_mknod(path: UserPtr, mode: Mode, dev: u32) -> u64 {
    finish(mknodat(Dirfd::Cwd, path, mode, dev))
}

/// `mknodat(dirfd, path, mode, dev)` (259): create a FIFO (`S_IFIFO`) or
/// regular file (`S_IFREG` or 0), checking the file type before path lookup.
/// Deviation: S_IFCHR/S_IFBLK/S_IFSOCK return EPERM even for root, whereas
/// Linux allows any user to create socket nodes (subject to directory access);
/// only character and block device creation requires CAP_MKNOD.
pub(super) fn sys_mknodat(dirfd: Dirfd, path: UserPtr, mode: Mode, dev: u32) -> u64 {
    finish(mknodat(dirfd, path, mode, dev))
}

fn mknodat(dirfd: Dirfd, path: UserPtr, mode: Mode, dev: u32) -> Result<u64, u32> {
    let path = UserPath::copy_in(path, false)?;
    // Type checked before lookup (Linux's `may_mknod`, ARCHITECTURE §8).
    let inode_type = match mode.bits() & general::S_IFMT {
        0 | general::S_IFREG => InodeType::File,
        general::S_IFIFO => InodeType::Fifo,
        general::S_IFDIR | general::S_IFCHR | general::S_IFBLK | general::S_IFSOCK => return Err(errno::EPERM),
        _ => return Err(errno::EINVAL),
    };
    let start = at::start_dir(dirfd, &path)?;
    start.mknod(&path, inode_type, mode, DevId(u64::from(dev)))?;
    Ok(0)
}
