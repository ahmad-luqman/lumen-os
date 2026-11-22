//! Working directory and directory reads: `getcwd` (M6), and M7's
//! `getdents64`, `chdir`, `fchdir`.

use alloc::vec::Vec;

use super::at::{Fd, UserPtr};
use super::{finish, not_implemented};
use crate::linux::process;
use crate::linux::{errno, general};
use crate::user::{CopyError, copy_to_user};

/// `getcwd(buf, size)` (79): the working directory's path, computed from
/// its inode (so a rename of it or an ancestor shows), with its NUL; its
/// length with the NUL. `-ENOENT` once the directory is unlinked (or the
/// process has none), `-ERANGE` if `size` can't hold it.
pub(super) fn sys_getcwd(buf: UserPtr, size: u64) -> u64 {
    finish(getcwd(buf, size))
}

fn getcwd(buf: UserPtr, size: u64) -> Result<u64, u32> {
    let (cwd, _) = process::current_cwd().ok_or(errno::ENOENT)?;
    let path = lumen_vfs::compute_cwd_path(crate::vfs::tables(), &cwd).map_err(lumen_vfs::VfsError::to_errno)?;
    drop(cwd);
    let mut bytes: Vec<u8> = path.into_bytes();
    bytes.push(0);
    if (bytes.len() as u64) > size {
        return Err(errno::ERANGE);
    }
    copy_to_user(buf.addr(), &bytes).map_err(CopyError::to_errno)?;
    Ok(bytes.len() as u64)
}

/// `getdents64(fd, dirp, count)` (217): M7 stub.
pub(super) fn sys_getdents64(_fd: Fd, _dirp: UserPtr, _count: u32) -> u64 {
    not_implemented(general::__NR_getdents64)
}

/// `chdir(path)` (80): M7 stub.
pub(super) fn sys_chdir(_path: UserPtr) -> u64 {
    not_implemented(general::__NR_chdir)
}

/// `fchdir(fd)` (81): M7 stub.
pub(super) fn sys_fchdir(_fd: Fd) -> u64 {
    not_implemented(general::__NR_fchdir)
}

crate::ktest!(
    fn dir_m7_stubs_return_enosys() {
        super::check_stubs(&[general::__NR_getdents64, general::__NR_chdir, general::__NR_fchdir])
    }
);

crate::ktest!(
    fn dir_getcwd_follows_the_cwd_inode_and_fails_once_unlinked() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenGuard, mkdir_at, rename_at2, unlink_at};

        let tables = crate::vfs::tables();
        process::in_test_process(move |_| {
            let buf = process::KTEST_USER_VA;
            let getcwd = |size: u64| sys_getcwd(super::at::reg(buf), size);
            let read_back = |len: usize| {
                let mut got = alloc::vec![0u8; len];
                crate::user::copy_from_user(&mut got, buf).map(|()| got).map_err(|_| "copy out")
            };
            // A new process's cwd: "/", two bytes with the NUL.
            kassert_eq!(getcwd(64), 2);
            kassert_eq!(read_back(2)?, b"/\0");
            kassert_eq!(getcwd(1), super::neg_errno(errno::ERANGE));
            kassert_eq!(sys_getcwd(super::at::reg(0), 64), super::neg_errno(errno::EFAULT));

            // A directory made the cwd: its path, which `size` must hold
            // with the NUL.
            let dir = mkdir_at(tables, None, b"/tmp/dir-ktest-cwd", 0o755, 0).map_err(|_| "mkdir")?;
            let guard = OpenGuard::try_new(dir).map_err(|_| "guard")?;
            process::set_current_cwd(process::Cwd::new(guard, alloc::string::String::from("/tmp/dir-ktest-cwd")));
            kassert_eq!(getcwd(64), 19);
            kassert_eq!(read_back(19)?, b"/tmp/dir-ktest-cwd\0");
            kassert_eq!(getcwd(18), super::neg_errno(errno::ERANGE));
            kassert_eq!(getcwd(19), 19);
            // Renamed: the new name, computed from the inode, not the
            // path the cwd was set by.
            let renamed = rename_at2(
                tables,
                None,
                b"/tmp/dir-ktest-cwd",
                None,
                b"/tmp/dir-ktest-moved",
                lumen_vfs::RenameFlags(0),
            );
            kassert!(renamed.is_ok());
            kassert_eq!(getcwd(64), 21);
            kassert_eq!(read_back(21)?, b"/tmp/dir-ktest-moved\0");
            // Removed while it is the cwd: `-ENOENT`, whatever the size.
            kassert!(unlink_at(tables, None, b"/tmp/dir-ktest-moved", true).is_ok());
            kassert_eq!(getcwd(64), super::neg_errno(errno::ENOENT));
            kassert_eq!(getcwd(1), super::neg_errno(errno::ENOENT));
            Ok(())
        })?
    }
);
