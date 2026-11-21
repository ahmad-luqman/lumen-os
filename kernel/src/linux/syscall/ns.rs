//! Namespace changes (M7): `mkdir`, `mkdirat`, `rmdir`, `unlink`,
//! `unlinkat`, `link`, `linkat`, `symlink`, `symlinkat`, `readlink`,
//! `readlinkat`, `rename`, `renameat`, `renameat2` (stubs until #254
//! implements them).

use super::not_implemented;
use crate::linux::general;

/// `mkdir(path, mode)` (83): M7 stub.
pub(super) fn sys_mkdir(_path: u64, _mode: u64) -> u64 {
    not_implemented(general::__NR_mkdir)
}

/// `mkdirat(dirfd, path, mode)` (258): M7 stub.
pub(super) fn sys_mkdirat(_dirfd: u64, _path: u64, _mode: u64) -> u64 {
    not_implemented(general::__NR_mkdirat)
}

/// `rmdir(path)` (84): M7 stub.
pub(super) fn sys_rmdir(_path: u64) -> u64 {
    not_implemented(general::__NR_rmdir)
}

/// `unlink(path)` (87): M7 stub.
pub(super) fn sys_unlink(_path: u64) -> u64 {
    not_implemented(general::__NR_unlink)
}

/// `unlinkat(dirfd, path, flags)` (263): M7 stub.
pub(super) fn sys_unlinkat(_dirfd: u64, _path: u64, _flags: u64) -> u64 {
    not_implemented(general::__NR_unlinkat)
}

/// `link(old, new)` (86): M7 stub.
pub(super) fn sys_link(_old: u64, _new: u64) -> u64 {
    not_implemented(general::__NR_link)
}

/// `linkat(olddirfd, old, newdirfd, new, flags)` (265): M7 stub.
pub(super) fn sys_linkat(_olddirfd: u64, _old: u64, _newdirfd: u64, _new: u64, _flags: u64) -> u64 {
    not_implemented(general::__NR_linkat)
}

/// `symlink(target, linkpath)` (88): M7 stub.
pub(super) fn sys_symlink(_target: u64, _linkpath: u64) -> u64 {
    not_implemented(general::__NR_symlink)
}

/// `symlinkat(target, newdirfd, linkpath)` (266): M7 stub.
pub(super) fn sys_symlinkat(_target: u64, _newdirfd: u64, _linkpath: u64) -> u64 {
    not_implemented(general::__NR_symlinkat)
}

/// `readlink(path, buf, bufsiz)` (89): M7 stub.
pub(super) fn sys_readlink(_path: u64, _buf: u64, _bufsiz: u64) -> u64 {
    not_implemented(general::__NR_readlink)
}

/// `readlinkat(dirfd, path, buf, bufsiz)` (267): M7 stub.
pub(super) fn sys_readlinkat(_dirfd: u64, _path: u64, _buf: u64, _bufsiz: u64) -> u64 {
    not_implemented(general::__NR_readlinkat)
}

/// `rename(old, new)` (82): M7 stub.
pub(super) fn sys_rename(_old: u64, _new: u64) -> u64 {
    not_implemented(general::__NR_rename)
}

/// `renameat(olddirfd, old, newdirfd, new)` (264): M7 stub.
pub(super) fn sys_renameat(_olddirfd: u64, _old: u64, _newdirfd: u64, _new: u64) -> u64 {
    not_implemented(general::__NR_renameat)
}

/// `renameat2(olddirfd, old, newdirfd, new, flags)` (316): M7 stub.
pub(super) fn sys_renameat2(_olddirfd: u64, _old: u64, _newdirfd: u64, _new: u64, _flags: u64) -> u64 {
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
