//! Namespace changes (M7): `mkdir`, `mkdirat`, `rmdir`, `unlink`,
//! `unlinkat`, `link`, `linkat`, `symlink`, `symlinkat`, `readlink`,
//! `readlinkat`, `rename`, `renameat`, `renameat2`.

use super::at::{Dirfd, Mode, UserPtr};
use super::not_implemented;
use crate::linux::general;
use lumen_vfs::{AtFlags, RenameFlags};

/// `mkdir(path, mode)` (83): M7 stub.
pub(super) fn sys_mkdir(_path: UserPtr, _mode: Mode) -> u64 {
    not_implemented(general::__NR_mkdir)
}

/// `mkdirat(dirfd, path, mode)` (258): M7 stub.
pub(super) fn sys_mkdirat(_dirfd: Dirfd, _path: UserPtr, _mode: Mode) -> u64 {
    not_implemented(general::__NR_mkdirat)
}

/// `rmdir(path)` (84): M7 stub.
pub(super) fn sys_rmdir(_path: UserPtr) -> u64 {
    not_implemented(general::__NR_rmdir)
}

/// `unlink(path)` (87): M7 stub.
pub(super) fn sys_unlink(_path: UserPtr) -> u64 {
    not_implemented(general::__NR_unlink)
}

/// `unlinkat(dirfd, path, flags)` (263): M7 stub.
pub(super) fn sys_unlinkat(_dirfd: Dirfd, _path: UserPtr, _flags: AtFlags) -> u64 {
    not_implemented(general::__NR_unlinkat)
}

/// `link(old, new)` (86): M7 stub.
pub(super) fn sys_link(_old: UserPtr, _new: UserPtr) -> u64 {
    not_implemented(general::__NR_link)
}

/// `linkat(olddirfd, old, newdirfd, new, flags)` (265): M7 stub.
pub(super) fn sys_linkat(_olddirfd: Dirfd, _old: UserPtr, _newdirfd: Dirfd, _new: UserPtr, _flags: AtFlags) -> u64 {
    not_implemented(general::__NR_linkat)
}

/// `symlink(target, linkpath)` (88): M7 stub.
pub(super) fn sys_symlink(_target: UserPtr, _linkpath: UserPtr) -> u64 {
    not_implemented(general::__NR_symlink)
}

/// `symlinkat(target, newdirfd, linkpath)` (266): M7 stub.
pub(super) fn sys_symlinkat(_target: UserPtr, _newdirfd: Dirfd, _linkpath: UserPtr) -> u64 {
    not_implemented(general::__NR_symlinkat)
}

/// `readlink(path, buf, bufsiz)` (89): M7 stub.
pub(super) fn sys_readlink(_path: UserPtr, _buf: UserPtr, _bufsiz: i32) -> u64 {
    not_implemented(general::__NR_readlink)
}

/// `readlinkat(dirfd, path, buf, bufsiz)` (267): M7 stub.
pub(super) fn sys_readlinkat(_dirfd: Dirfd, _path: UserPtr, _buf: UserPtr, _bufsiz: i32) -> u64 {
    not_implemented(general::__NR_readlinkat)
}

/// `rename(old, new)` (82): M7 stub.
pub(super) fn sys_rename(_old: UserPtr, _new: UserPtr) -> u64 {
    not_implemented(general::__NR_rename)
}

/// `renameat(olddirfd, old, newdirfd, new)` (264): M7 stub.
pub(super) fn sys_renameat(_olddirfd: Dirfd, _old: UserPtr, _newdirfd: Dirfd, _new: UserPtr) -> u64 {
    not_implemented(general::__NR_renameat)
}

/// `renameat2(olddirfd, old, newdirfd, new, flags)` (316): M7 stub.
pub(super) fn sys_renameat2(
    _olddirfd: Dirfd,
    _old: UserPtr,
    _newdirfd: Dirfd,
    _new: UserPtr,
    _flags: RenameFlags,
) -> u64 {
    not_implemented(general::__NR_renameat2)
}

crate::ktest!(
    fn ns_m7_stubs_return_enosys() {
        super::check_stubs(&[
            general::__NR_mkdir,
            general::__NR_mkdirat,
            general::__NR_rmdir,
            general::__NR_unlink,
            general::__NR_unlinkat,
            general::__NR_link,
            general::__NR_linkat,
            general::__NR_symlink,
            general::__NR_symlinkat,
            general::__NR_readlink,
            general::__NR_readlinkat,
            general::__NR_rename,
            general::__NR_renameat,
            general::__NR_renameat2,
        ])
    }
);
