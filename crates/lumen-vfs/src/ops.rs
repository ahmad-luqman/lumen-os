//! High-level POSIX namespace operations (`open`, `mkdir`, `unlink`, `rmdir`,
//! `link`, `symlink`, `readlink`, `renameat2`) coordinating `resolve_path`,
//! Lock-9 cache invalidation, Lock-3 `rename_lock`, and Gate 6 detach rules.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::cache::DentryTarget;
use crate::error::VfsError;
use crate::fs::VfsInode;
use crate::mount::VfsCacheOps;
use crate::resolve::{
    ResolveFlags, cross_mounts_down, resolve_parent, resolve_parent_with_depth, resolve_path, resolve_path_with_depth,
};
use crate::types::{InodeType, OpenFlags, RenameFlags};
use crate::{MAX_SYMLINK_DEPTH, PATH_MAX};

/// Open or create a file/directory/device inode according to POSIX/`openat(2)`
/// semantics.
///
/// Returns `(inode, normalized_path_string)` ready for constructing an [`OpenFile`](crate::fd::OpenFile).
pub fn open_inode(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: OpenFlags,
    mode: u16,
    umask: u16,
) -> Result<(Arc<dyn VfsInode>, String), VfsError> {
    if flags.has_tmpfile() {
        return Err(VfsError::OpNotSupp);
    }
    let _ = flags.access_mode()?;

    let path_str = core::str::from_utf8(path).map(String::from).unwrap_or_default();

    if flags.is_path() {
        let resolved = resolve_path(
            env,
            start_dir,
            path,
            ResolveFlags { follow_final: !flags.has_nofollow(), empty_path: false, require_dir: flags.has_directory() },
        )?;
        return Ok((resolved.inode, path_str));
    }

    let inode = if flags.has_creat() {
        let mut symlink_depth = 0usize;
        let mut first_hop = true;
        let mut cur_start_dir: Option<Arc<dyn VfsInode>> = start_dir.map(Arc::clone);
        let mut cur_path: Vec<u8> = path.to_vec();

        loop {
            let target = resolve_parent_with_depth(env, cur_start_dir.as_ref(), &cur_path, &mut symlink_depth)?;
            if target.name == "/" {
                if flags.has_excl() {
                    return Err(VfsError::Exist);
                }
                break cross_mounts_down(env, Arc::clone(&target.parent));
            }
            if target.name == "." || target.name == ".." {
                if flags.has_excl() {
                    return Err(VfsError::Exist);
                }
                break resolve_path_with_depth(
                    env,
                    Some(&target.parent),
                    target.name.as_bytes(),
                    ResolveFlags { follow_final: !flags.has_nofollow(), empty_path: false, require_dir: true },
                    &mut symlink_depth,
                )?
                .inode;
            }
            if first_hop && target.has_trailing_slash {
                return Err(VfsError::IsDir);
            }

            match target.parent.lookup(&target.name) {
                Ok(existing) => {
                    if flags.has_excl() {
                        return Err(VfsError::Exist);
                    }
                    let existing = env.inode_get_or_insert(existing);
                    if existing.inode_type() == InodeType::Symlink {
                        if flags.has_nofollow() && !target.has_trailing_slash {
                            return Err(VfsError::Loop);
                        }
                        symlink_depth += 1;
                        if symlink_depth > MAX_SYMLINK_DEPTH {
                            return Err(VfsError::Loop);
                        }
                        if let Some(magic_target) = existing.resolve_magic_link() {
                            let magic_target = env.inode_get_or_insert(magic_target);
                            let resolved = cross_mounts_down(env, magic_target);
                            if target.has_trailing_slash && resolved.inode_type() != InodeType::Dir {
                                return Err(VfsError::NotDir);
                            }
                            break resolved;
                        }
                        let mut link_target = existing.readlink()?;
                        if link_target.is_empty() {
                            return Err(VfsError::NoEnt);
                        }
                        if link_target.contains(&0) {
                            return Err(VfsError::Inval);
                        }
                        if link_target.len() >= PATH_MAX {
                            return Err(VfsError::NameTooLong);
                        }
                        if target.has_trailing_slash && !link_target.ends_with(b"/") {
                            if link_target.len() + 1 >= PATH_MAX {
                                return Err(VfsError::NameTooLong);
                            }
                            link_target.push(b'/');
                        }
                        first_hop = false;
                        cur_start_dir =
                            if link_target.starts_with(b"/") { None } else { Some(Arc::clone(&target.parent)) };
                        cur_path = link_target;
                    } else {
                        // Follow stacked mountpoints if `existing` is a mountpoint directory.
                        let existing = cross_mounts_down(env, existing);
                        if target.has_trailing_slash && existing.inode_type() != InodeType::Dir {
                            return Err(VfsError::NotDir);
                        }
                        break existing;
                    }
                }
                Err(VfsError::NoEnt) => {
                    if target.has_trailing_slash {
                        return Err(VfsError::IsDir);
                    }
                    if flags.has_directory() {
                        return Err(VfsError::NotDir);
                    }
                    if env.fs_read_only(target.parent.dev_id()) {
                        return Err(VfsError::RoFs);
                    }
                    let eff_mode = (mode & 0o7777) & !umask;
                    let created = target.parent.create(&target.name, eff_mode)?;
                    let created = env.inode_get_or_insert(created);
                    env.dentry_insert(
                        target.parent.dev_id(),
                        target.parent.ino(),
                        &target.name,
                        DentryTarget { dev: created.dev_id(), ino: created.ino() },
                    );
                    return Ok((created, path_str));
                }
                Err(e) => return Err(e),
            }
        }
    } else {
        resolve_path(
            env,
            start_dir,
            path,
            ResolveFlags { follow_final: !flags.has_nofollow(), empty_path: false, require_dir: flags.has_directory() },
        )?
        .inode
    };

    if inode.inode_type() == InodeType::Symlink {
        return Err(VfsError::Loop);
    }
    if flags.has_directory() && inode.inode_type() != InodeType::Dir {
        return Err(VfsError::NotDir);
    }
    if inode.inode_type() == InodeType::Dir && (flags.is_writable() || flags.has_trunc() || flags.has_creat()) {
        return Err(VfsError::IsDir);
    }
    if !inode.exempt_from_rofs() && env.fs_read_only(inode.dev_id()) && (flags.is_writable() || flags.has_trunc()) {
        return Err(VfsError::RoFs);
    }
    if flags.has_trunc() && inode.inode_type() == InodeType::File {
        inode.truncate(0)?;
    }

    Ok((inode, path_str))
}

/// `mkdirat(2)`: create a directory with `(mode & 0o7777) & !umask`.
pub fn mkdir_at(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    mode: u16,
    umask: u16,
) -> Result<Arc<dyn VfsInode>, VfsError> {
    let target = resolve_parent(env, start_dir, path)?;
    if target.name == "/" || target.name == "." || target.name == ".." {
        return Err(VfsError::Exist);
    }
    if env.fs_read_only(target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }
    let eff_mode = (mode & 0o7777) & !umask;
    let dir = target.parent.mkdir(&target.name, eff_mode)?;
    let dir = env.inode_get_or_insert(dir);
    env.dentry_insert(
        target.parent.dev_id(),
        target.parent.ino(),
        &target.name,
        DentryTarget { dev: dir.dev_id(), ino: dir.ino() },
    );
    Ok(dir)
}

/// `unlinkat(2)`: unlink a non-directory (`remove_dir == false`) or remove an
/// empty directory (`remove_dir == true`).
///
/// Invalidates `DentryCache` immediately and removes the target from
/// `InodeCache` before backend removal when removing the last link so
/// open-unlinked inodes never alias reused inode numbers.
pub fn unlink_at(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    remove_dir: bool,
) -> Result<(), VfsError> {
    let target = resolve_parent(env, start_dir, path)?;
    if target.name == "/" {
        return Err(VfsError::Busy);
    }
    if target.name == "." {
        return Err(VfsError::Inval);
    }
    if target.name == ".." {
        return Err(VfsError::NotEmpty);
    }
    if env.fs_read_only(target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }

    let child = target.parent.lookup(&target.name)?;
    let child = env.inode_get_or_insert(child);

    if !remove_dir {
        if child.inode_type() == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if target.has_trailing_slash {
            if child.inode_type() == InodeType::Symlink {
                let followed = resolve_path(
                    env,
                    start_dir,
                    path,
                    ResolveFlags { follow_final: true, empty_path: false, require_dir: false },
                )?;
                if followed.inode.inode_type() == InodeType::Dir {
                    return Err(VfsError::IsDir);
                }
            }
            return Err(VfsError::NotDir);
        }

        let old_dev = child.dev_id();
        let old_ino = child.ino();
        let removing_last_link = child.stat()?.nlink <= 1;

        env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), &target.name);
        if removing_last_link {
            let _ = env.inode_remove(old_dev, old_ino);
        }

        if let Err(err) = target.parent.unlink(&target.name) {
            if removing_last_link && !child.is_detached() && child.ino() == old_ino {
                let _ = env.inode_get_or_insert(child);
            }
            return Err(err);
        }
        env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), &target.name);
        Ok(())
    } else {
        if child.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if env.is_mountpoint(child.dev_id(), child.ino()) {
            return Err(VfsError::Busy);
        }

        let old_dev = child.dev_id();
        let old_ino = child.ino();
        env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), &target.name);
        env.dentry_invalidate_dir(old_dev, old_ino);
        let _ = env.inode_remove(old_dev, old_ino);

        if let Err(err) = target.parent.rmdir(&target.name) {
            if !child.is_detached() && child.ino() == old_ino {
                let _ = env.inode_get_or_insert(child);
            }
            return Err(err);
        }
        env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), &target.name);
        env.dentry_invalidate_dir(old_dev, old_ino);
        Ok(())
    }
}

/// `linkat(2)`: create a hard link, rejecting directories (`-EPERM`) and
/// cross-mount targets (`-EXDEV`).
pub fn link_at(
    env: &dyn VfsCacheOps,
    old_dir: Option<&Arc<dyn VfsInode>>,
    old_path: &[u8],
    new_dir: Option<&Arc<dyn VfsInode>>,
    new_path: &[u8],
    follow_symlinks: bool,
    empty_path: bool,
) -> Result<(), VfsError> {
    let old = resolve_path(
        env,
        old_dir,
        old_path,
        ResolveFlags { follow_final: follow_symlinks, empty_path, require_dir: false },
    )?;

    if old.inode.inode_type() == InodeType::Dir {
        return Err(VfsError::Perm);
    }

    let new_target = resolve_parent(env, new_dir, new_path)?;
    if new_target.name == "/" || new_target.name == "." || new_target.name == ".." {
        return Err(VfsError::Exist);
    }
    if new_target.has_trailing_slash {
        return Err(VfsError::NoEnt);
    }
    if old.inode.dev_id() != new_target.parent.dev_id() {
        return Err(VfsError::XDev);
    }
    if env.fs_read_only(new_target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }
    if old.inode.is_detached() || old.inode.stat()?.nlink == 0 {
        return Err(VfsError::NoEnt);
    }

    new_target.parent.link(&new_target.name, &old.inode)?;
    env.dentry_insert(
        new_target.parent.dev_id(),
        new_target.parent.ino(),
        &new_target.name,
        DentryTarget { dev: old.inode.dev_id(), ino: old.inode.ino() },
    );
    Ok(())
}

/// `symlinkat(2)`: create a symbolic link with verbatim non-empty `target`.
pub fn symlink_at(
    env: &dyn VfsCacheOps,
    target: &[u8],
    new_dir: Option<&Arc<dyn VfsInode>>,
    linkpath: &[u8],
) -> Result<Arc<dyn VfsInode>, VfsError> {
    if target.is_empty() {
        return Err(VfsError::NoEnt);
    }
    if target.len() >= PATH_MAX {
        return Err(VfsError::NameTooLong);
    }
    if target.contains(&0) {
        return Err(VfsError::Inval);
    }
    let target_str = core::str::from_utf8(target).map_err(|_| VfsError::Inval)?;

    let parent = resolve_parent(env, new_dir, linkpath)?;
    if parent.name == "/" || parent.name == "." || parent.name == ".." {
        return Err(VfsError::Exist);
    }
    if parent.has_trailing_slash {
        return match parent.parent.lookup(&parent.name) {
            Ok(_) => Err(VfsError::Exist),
            Err(e) => Err(e),
        };
    }
    if env.fs_read_only(parent.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }

    let sym = parent.parent.symlink(&parent.name, target_str)?;
    let sym = env.inode_get_or_insert(sym);
    env.dentry_insert(
        parent.parent.dev_id(),
        parent.parent.ino(),
        &parent.name,
        DentryTarget { dev: sym.dev_id(), ino: sym.ino() },
    );
    Ok(sym)
}

/// `readlinkat(2)`: read symlink target without following the final component.
/// Supports `path == b""` on an `O_PATH | O_NOFOLLOW` symlink descriptor.
pub fn readlink_at(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
) -> Result<Vec<u8>, VfsError> {
    let resolved = resolve_path(
        env,
        start_dir,
        path,
        ResolveFlags { follow_final: false, empty_path: path.is_empty(), require_dir: false },
    )?;
    if resolved.inode.inode_type() != InodeType::Symlink {
        return Err(VfsError::Inval);
    }
    resolved.inode.readlink()
}

/// Return true if `candidate` equals `ancestor` or is a descendant of `ancestor`
/// within the same filesystem.
fn is_same_or_descendant(ancestor: &Arc<dyn VfsInode>, candidate: &Arc<dyn VfsInode>) -> Result<bool, VfsError> {
    let dev = ancestor.dev_id();
    let target_ino = ancestor.ino();
    let mut cur = Arc::clone(candidate);

    for _ in 0..PATH_MAX {
        if cur.dev_id() != dev {
            return Ok(false);
        }
        if cur.ino() == target_ino {
            return Ok(true);
        }
        let parent = cur.lookup("..")?;
        if parent.dev_id() == cur.dev_id() && parent.ino() == cur.ino() {
            return Ok(false);
        }
        cur = parent;
    }
    Err(VfsError::Loop)
}

/// `renameat2(2)`: atomic rename supporting flags `0`, `RENAME_NOREPLACE`, and
/// `RENAME_EXCHANGE`.
pub fn rename_at2(
    env: &dyn VfsCacheOps,
    old_dir: Option<&Arc<dyn VfsInode>>,
    old_path: &[u8],
    new_dir: Option<&Arc<dyn VfsInode>>,
    new_path: &[u8],
    flags: RenameFlags,
) -> Result<(), VfsError> {
    let flags = flags.validate()?;

    let old_target = resolve_parent(env, old_dir, old_path)?;
    let new_target = resolve_parent(env, new_dir, new_path)?;

    if old_target.name == "/" || new_target.name == "/" {
        return Err(VfsError::Busy);
    }
    if old_target.name == "." || old_target.name == ".." || new_target.name == "." || new_target.name == ".." {
        return Err(VfsError::Inval);
    }

    let dev = old_target.parent.dev_id();
    if dev != new_target.parent.dev_id() {
        return Err(VfsError::XDev);
    }
    if env.fs_read_only(dev) {
        return Err(VfsError::RoFs);
    }

    let fs_opt = env.fs_by_dev(dev);
    let mut run = || -> Result<(), VfsError> {
        let old_inode = old_target.parent.lookup(&old_target.name)?;
        let old_inode = env.inode_get_or_insert(old_inode);

        if env.is_mountpoint(old_inode.dev_id(), old_inode.ino()) {
            return Err(VfsError::Busy);
        }
        if old_target.has_trailing_slash && old_inode.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }

        let new_inode_opt = match new_target.parent.lookup(&new_target.name) {
            Ok(i) => Some(env.inode_get_or_insert(i)),
            Err(VfsError::NoEnt) => None,
            Err(e) => return Err(e),
        };

        if let Some(ref new_inode) = new_inode_opt
            && old_inode.dev_id() == new_inode.dev_id()
            && old_inode.ino() == new_inode.ino()
        {
            if new_target.has_trailing_slash && old_inode.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            if flags.has_noreplace() {
                return Err(VfsError::Exist);
            }
            return Ok(());
        }

        if flags.has_noreplace() && new_inode_opt.is_some() {
            return Err(VfsError::Exist);
        }

        if flags.has_exchange() {
            let Some(ref new_inode) = new_inode_opt else {
                return Err(VfsError::NoEnt);
            };
            if env.is_mountpoint(new_inode.dev_id(), new_inode.ino()) {
                return Err(VfsError::Busy);
            }
            if new_target.has_trailing_slash && new_inode.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            if old_inode.inode_type() == InodeType::Dir && is_same_or_descendant(&old_inode, &new_target.parent)? {
                return Err(VfsError::Inval);
            }
            if new_inode.inode_type() == InodeType::Dir && is_same_or_descendant(new_inode, &old_target.parent)? {
                return Err(VfsError::Inval);
            }

            env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), &old_target.name);
            env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), &new_target.name);

            old_target.parent.rename(&old_target.name, &new_target.parent, &new_target.name, flags)?;

            env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), &old_target.name);
            env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), &new_target.name);
            return Ok(());
        }

        if let Some(ref new_inode) = new_inode_opt {
            if env.is_mountpoint(new_inode.dev_id(), new_inode.ino()) {
                return Err(VfsError::Busy);
            }
            if old_inode.inode_type() == InodeType::Dir && new_inode.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            if old_inode.inode_type() != InodeType::Dir && new_inode.inode_type() == InodeType::Dir {
                return Err(VfsError::IsDir);
            }
            if new_target.has_trailing_slash && new_inode.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
        } else if new_target.has_trailing_slash && old_inode.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }

        if old_inode.inode_type() == InodeType::Dir && is_same_or_descendant(&old_inode, &new_target.parent)? {
            return Err(VfsError::Inval);
        }

        let mut removed_target_ino: Option<(Arc<dyn VfsInode>, u64)> = None;
        let mut overwritten_dir_key: Option<(crate::types::DevId, crate::types::Ino)> = None;
        if let Some(ref new_inode) = new_inode_opt {
            let nlink = new_inode.stat()?.nlink;
            let last_link = if new_inode.inode_type() == InodeType::Dir { nlink <= 2 } else { nlink <= 1 };
            if last_link {
                let old_ino = new_inode.ino();
                let _ = env.inode_remove(new_inode.dev_id(), old_ino);
                removed_target_ino = Some((Arc::clone(new_inode), old_ino));
            }
            if new_inode.inode_type() == InodeType::Dir {
                let dir_key = (new_inode.dev_id(), new_inode.ino());
                env.dentry_invalidate_dir(dir_key.0, dir_key.1);
                overwritten_dir_key = Some(dir_key);
            }
        }

        env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), &old_target.name);
        env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), &new_target.name);

        if let Err(err) = old_target.parent.rename(&old_target.name, &new_target.parent, &new_target.name, flags) {
            if let Some((inode, old_ino)) = removed_target_ino
                && !inode.is_detached()
                && inode.ino() == old_ino
            {
                let _ = env.inode_get_or_insert(inode);
            }
            return Err(err);
        }

        env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), &old_target.name);
        env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), &new_target.name);
        if let Some((d_dev, d_ino)) = overwritten_dir_key {
            env.dentry_invalidate_dir(d_dev, d_ino);
        }

        Ok(())
    };

    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run),
        None => run(),
    }
}
