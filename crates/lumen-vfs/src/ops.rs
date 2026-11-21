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
    MAX_INODE_RACE_RETRIES, ResolveFlags, cross_mounts_down, resolve_and_unify_magic_target, resolve_parent,
    resolve_parent_with_depth, resolve_path, resolve_path_with_depth,
};
use crate::types::{DevId, Ino, InodeType, OpenFlags, RenameFlags};
use crate::{MAX_SYMLINK_DEPTH, PATH_MAX};

/// Upper bound on non-exclusive `O_CREAT` lookup-vs-create race retries.
const MAX_CREAT_RACE_RETRIES: usize = 64;

struct InodeRemovalGuard<'a> {
    env: &'a dyn VfsCacheOps,
    dev: DevId,
    ino: Ino,
    active: bool,
}

impl<'a> InodeRemovalGuard<'a> {
    fn begin(env: &'a dyn VfsCacheOps, dev: DevId, ino: Ino, since_gen: u64) -> (Self, Option<Arc<dyn VfsInode>>) {
        let (removed, _) = env.inode_begin_removal(dev, ino, since_gen);
        (Self { env, dev, ino, active: true }, removed)
    }

    fn finish(mut self) -> u64 {
        self.active = false;
        self.env.inode_end_removal(self.dev, self.ino)
    }
}

impl Drop for InodeRemovalGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.env.inode_end_removal(self.dev, self.ino);
        }
    }
}

pub(crate) fn is_inode_dead(inode: &Arc<dyn VfsInode>) -> bool {
    inode.is_detached() || matches!(inode.stat(), Ok(m) if m.nlink == 0) || matches!(inode.stat(), Err(VfsError::NoEnt))
}

/// Evict any cached dead inode for `(dev, ino)` without recording a redundant
/// removal if none is cached (used right after `InodeRemovalGuard::finish`).
fn evict_if_cached_zombie(env: &dyn VfsCacheOps, dev: DevId, ino: Ino) {
    let _ = env.inode_evict_detached(dev, ino);
    if let Some(cached) = env.inode_get(dev, ino)
        && (is_inode_dead(&cached) || cached.ino() != ino)
    {
        let _ = env.inode_remove_if_same(dev, ino, &cached);
    }
}

/// Evict any cached dead inode for `(dev, ino)` AND atomically guarantee a
/// removal generation is recorded on `InodeCache` miss (closes `PROBE 1D-2`
/// and `PROBE 1D-4` `evict_zombie_inode` Lock-9 gap).
fn evict_zombie_inode(env: &dyn VfsCacheOps, dev: DevId, ino: Ino) {
    for _ in 0..MAX_INODE_RACE_RETRIES {
        match env.inode_record_removal_if_absent(dev, ino) {
            Ok(_) => return,
            Err(cached) => {
                if is_inode_dead(&cached) || cached.ino() != ino {
                    if env.inode_remove_if_same(dev, ino, &cached).is_some() {
                        return;
                    }
                    continue;
                }
                let _ = env.inode_record_removal(dev, ino);
                return;
            }
        }
    }
}

/// Publish a newly created inode into `InodeCache` and `DentryCache`. If an
/// earlier unlink/eviction for the same `(dev, ino)` raced before `create`/
/// `mkdir`/`symlink`, verify via `parent.lookup(name)` and `is_inode_dead`
/// before retrying with `publish_lookup_inode` (which respects
/// `pending_removals`) so live inodes unify without adopting an unlinked or
/// replacement inode.
fn publish_created_inode(
    env: &dyn VfsCacheOps,
    parent: &Arc<dyn VfsInode>,
    name: &str,
    created: Arc<dyn VfsInode>,
    dentry_gen: u64,
    inode_gen: u64,
) -> Arc<dyn VfsInode> {
    let pdev = parent.dev_id();
    let pino = parent.ino();
    let target_dev = created.dev_id();
    let target_ino = created.ino();
    let target_type = created.inode_type();

    if let Ok(unified) = env.publish_created_inode(pdev, pino, name, Arc::clone(&created), dentry_gen, inode_gen) {
        if Arc::ptr_eq(&unified, &created) {
            if is_inode_dead(&created) || created.ino() != target_ino {
                env.dentry_invalidate(pdev, pino, name);
                let _ = env.inode_remove_if_same(target_dev, target_ino, &created);
            }
            return created;
        }
        if is_inode_dead(&created) || created.ino() != target_ino {
            return created;
        }
        if !is_inode_dead(&unified) && unified.ino() == target_ino {
            let _ = env.dentry_insert_if_gen(
                pdev,
                pino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                dentry_gen,
            );
            return unified;
        }
        env.dentry_invalidate(pdev, pino, name);
        let _ = env.inode_remove_if_same(unified.dev_id(), target_ino, &unified);
    }

    for _ in 0..MAX_INODE_RACE_RETRIES {
        if is_inode_dead(&created) || created.ino() != target_ino {
            return created;
        }
        let (retry_dentry_gen, retry_inode_gen) = env.cache_gens();
        if is_inode_dead(&created) || created.ino() != target_ino {
            return created;
        }
        let Ok(live) = parent.lookup(name) else {
            return created;
        };
        if live.dev_id() != target_dev || live.ino() != target_ino || live.inode_type() != target_type {
            return created;
        }
        match env.publish_lookup_inode(pdev, pino, name, Arc::clone(&created), retry_dentry_gen, retry_inode_gen) {
            Ok(unified) => {
                if Arc::ptr_eq(&unified, &created) {
                    if is_inode_dead(&created) || created.ino() != target_ino {
                        env.dentry_invalidate(pdev, pino, name);
                        let _ = env.inode_remove_if_same(target_dev, target_ino, &created);
                    }
                    return created;
                }
                if is_inode_dead(&created) || created.ino() != target_ino {
                    return created;
                }
                if is_inode_dead(&unified) || unified.ino() != target_ino {
                    env.dentry_invalidate(pdev, pino, name);
                    let _ = env.inode_remove_if_same(unified.dev_id(), target_ino, &unified);
                    continue;
                }
                return unified;
            }
            Err(_) => continue,
        }
    }

    created
}

fn lookup_and_unify_inode(
    env: &dyn VfsCacheOps,
    parent: &Arc<dyn VfsInode>,
    name: &str,
) -> Result<Arc<dyn VfsInode>, VfsError> {
    let mut first_inode: Option<Arc<dyn VfsInode>> = None;
    for attempt in 0..=MAX_INODE_RACE_RETRIES {
        let (_, since_gen) = env.cache_gens();
        let raw = match parent.lookup(name) {
            Ok(i) => i,
            Err(VfsError::NoEnt) if attempt > 0 => return Err(VfsError::NoEnt),
            Err(e) => return Err(e),
        };
        let raw_ino = raw.ino();
        match env.inode_get_or_insert_if_fresh(Arc::clone(&raw), since_gen) {
            Ok(unified) => {
                if !Arc::ptr_eq(&unified, &raw) && (is_inode_dead(&unified) || unified.ino() != raw_ino) {
                    let _ = env.inode_remove_if_same(unified.dev_id(), raw_ino, &unified);
                    if first_inode.is_none() {
                        first_inode = Some(raw);
                    }
                    continue;
                }
                return Ok(unified);
            }
            Err(stale) => {
                if first_inode.is_none() {
                    first_inode = Some(stale);
                }
            }
        }
    }
    Ok(first_inode.expect("loop ran at least once"))
}

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

        'follow: loop {
            let target = resolve_parent_with_depth(env, cur_start_dir.as_ref(), &cur_path, &mut symlink_depth)?;
            if target.name == "/" {
                if flags.has_excl() {
                    return Err(VfsError::Exist);
                }
                break 'follow cross_mounts_down(env, Arc::clone(&target.parent));
            }
            if target.name == "." || target.name == ".." {
                if flags.has_excl() {
                    return Err(VfsError::Exist);
                }
                break 'follow resolve_path_with_depth(
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

            enum CreatStep {
                Done(Arc<dyn VfsInode>),
                FollowSymlink(Arc<dyn VfsInode>),
                Created(Arc<dyn VfsInode>),
            }

            let mut step: Option<CreatStep> = None;
            let fs_opt = env.fs_by_dev(target.parent.dev_id());
            let mut run_creat = || -> Result<(), VfsError> {
                let mut creat_retries = 0usize;
                let mut inode_retries = 0usize;
                loop {
                    let (dentry_gen, inode_gen) = env.cache_gens();
                    match target.parent.lookup(&target.name) {
                        Ok(existing) => {
                            if flags.has_excl() {
                                return Err(VfsError::Exist);
                            }
                            let existing_ino = existing.ino();
                            let existing = match env.publish_lookup_inode(
                                target.parent.dev_id(),
                                target.parent.ino(),
                                &target.name,
                                Arc::clone(&existing),
                                dentry_gen,
                                inode_gen,
                            ) {
                                Ok(unified) => {
                                    if !Arc::ptr_eq(&unified, &existing)
                                        && (is_inode_dead(&unified) || unified.ino() != existing_ino)
                                    {
                                        env.dentry_invalidate(
                                            target.parent.dev_id(),
                                            target.parent.ino(),
                                            &target.name,
                                        );
                                        let _ = env.inode_remove_if_same(unified.dev_id(), existing_ino, &unified);
                                        if inode_retries < MAX_INODE_RACE_RETRIES {
                                            inode_retries += 1;
                                            continue;
                                        }
                                        existing
                                    } else {
                                        unified
                                    }
                                }
                                Err(stale) => {
                                    if inode_retries < MAX_INODE_RACE_RETRIES {
                                        inode_retries += 1;
                                        continue;
                                    }
                                    stale
                                }
                            };
                            if existing.inode_type() == InodeType::Symlink {
                                if flags.has_nofollow() && !target.has_trailing_slash {
                                    return Err(VfsError::Loop);
                                }
                                step = Some(CreatStep::FollowSymlink(existing));
                            } else {
                                step = Some(CreatStep::Done(existing));
                            }
                            return Ok(());
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
                            let (dentry_gen, inode_gen) = env.cache_gens();
                            match target.parent.create(&target.name, eff_mode) {
                                Ok(created) => {
                                    let created = publish_created_inode(
                                        env,
                                        &target.parent,
                                        &target.name,
                                        created,
                                        dentry_gen,
                                        inode_gen,
                                    );
                                    step = Some(CreatStep::Created(created));
                                    return Ok(());
                                }
                                Err(VfsError::Exist) if !flags.has_excl() => {
                                    if creat_retries >= MAX_CREAT_RACE_RETRIES {
                                        return Err(VfsError::Exist);
                                    }
                                    creat_retries += 1;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            };
            match fs_opt {
                Some(fs) => fs.with_rename_lock(&mut run_creat)?,
                None => run_creat()?,
            }
            match step.expect("run_creat returns Ok only after setting step") {
                CreatStep::Created(created) => return Ok((created, path_str)),
                CreatStep::Done(existing) => {
                    // Follow stacked mountpoints if `existing` is a mountpoint directory.
                    let existing = cross_mounts_down(env, existing);
                    if target.has_trailing_slash && existing.inode_type() != InodeType::Dir {
                        return Err(VfsError::NotDir);
                    }
                    break 'follow existing;
                }
                CreatStep::FollowSymlink(existing) => {
                    symlink_depth += 1;
                    if symlink_depth > MAX_SYMLINK_DEPTH {
                        return Err(VfsError::Loop);
                    }
                    if let Some(magic_target) = resolve_and_unify_magic_target(env, &existing) {
                        let resolved = cross_mounts_down(env, magic_target);
                        if target.has_trailing_slash && resolved.inode_type() != InodeType::Dir {
                            return Err(VfsError::NotDir);
                        }
                        break 'follow resolved;
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
                    cur_start_dir = if link_target.starts_with(b"/") { None } else { Some(Arc::clone(&target.parent)) };
                    cur_path = link_target;
                    continue 'follow;
                }
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
    let fs_opt = env.fs_by_dev(target.parent.dev_id());
    let mut created_dir: Option<Arc<dyn VfsInode>> = None;
    let mut run = || -> Result<(), VfsError> {
        let (dentry_gen, inode_gen) = env.cache_gens();
        let dir = target.parent.mkdir(&target.name, eff_mode)?;
        created_dir = Some(publish_created_inode(env, &target.parent, &target.name, dir, dentry_gen, inode_gen));
        Ok(())
    };
    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run)?,
        None => run()?,
    }
    Ok(created_dir.expect("mkdir run returns Ok only after setting created_dir"))
}

/// `unlinkat(2)`: unlink a non-directory (`remove_dir == false`) or remove an
/// empty directory (`remove_dir == true`).
///
/// Serializes under Lock 3 (`FileSystem::with_rename_lock`), invalidates
/// `DentryCache` immediately, removes the target from `InodeCache` safely via
/// `inode_remove_if_fresh`, and performs post-mutation `nlink == 0`/`is_detached`
/// cleanup so open-unlinked inodes never alias reused inode numbers.
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

    let fs_opt = env.fs_by_dev(target.parent.dev_id());
    let mut run = || -> Result<(), VfsError> {
        let (_, since_gen) = env.cache_gens();
        let raw_child = target.parent.lookup(&target.name)?;
        let child = env.inode_get_if_fresh(raw_child.dev_id(), raw_child.ino(), since_gen).unwrap_or(raw_child);

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
            let removal_guard = if removing_last_link {
                Some(InodeRemovalGuard::begin(env, old_dev, old_ino, since_gen))
            } else {
                None
            };

            if let Err(err) = target.parent.unlink(&target.name) {
                if let Some((guard, Some(removed_arc))) = removal_guard {
                    let end_gen = guard.finish();
                    if !removed_arc.is_detached()
                        && removed_arc.ino() == old_ino
                        && matches!(removed_arc.stat(), Ok(m) if m.nlink > 0)
                    {
                        let _ = env.inode_get_or_insert_if_fresh(removed_arc, end_gen);
                    }
                }
                return Err(err);
            }

            if let Some((guard, removed_arc)) = removal_guard {
                let _ = guard.finish();
                if removed_arc.is_none() {
                    evict_if_cached_zombie(env, old_dev, old_ino);
                }
            } else {
                let last_gone = child.is_detached()
                    || child.ino() != old_ino
                    || matches!(child.stat(), Ok(m) if m.nlink == 0)
                    || matches!(child.stat(), Err(VfsError::NoEnt));
                if last_gone {
                    evict_zombie_inode(env, old_dev, old_ino);
                }
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
            let (guard, removed_arc_opt) = InodeRemovalGuard::begin(env, old_dev, old_ino, since_gen);

            if let Err(err) = target.parent.rmdir(&target.name) {
                let end_gen = guard.finish();
                if let Some(removed_arc) = removed_arc_opt
                    && !removed_arc.is_detached()
                    && removed_arc.ino() == old_ino
                    && matches!(removed_arc.stat(), Ok(m) if m.nlink > 0)
                {
                    let _ = env.inode_get_or_insert_if_fresh(removed_arc, end_gen);
                }
                return Err(err);
            }
            let _ = guard.finish();
            if removed_arc_opt.is_none() {
                evict_if_cached_zombie(env, old_dev, old_ino);
            }
            env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), &target.name);
            env.dentry_invalidate_dir(old_dev, old_ino);
            Ok(())
        }
    };

    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run),
        None => run(),
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

    let fs_opt = env.fs_by_dev(new_target.parent.dev_id());
    let mut run = || -> Result<(), VfsError> {
        if old.inode.is_detached() || old.inode.stat()?.nlink == 0 {
            return Err(VfsError::NoEnt);
        }

        let inv_gen = env.dentry_gen();
        new_target.parent.link(&new_target.name, &old.inode)?;
        let _ = env.dentry_insert_if_gen(
            new_target.parent.dev_id(),
            new_target.parent.ino(),
            &new_target.name,
            DentryTarget { dev: old.inode.dev_id(), ino: old.inode.ino() },
            inv_gen,
        );
        Ok(())
    };

    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run),
        None => run(),
    }
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

    let fs_opt = env.fs_by_dev(parent.parent.dev_id());
    let mut created_sym: Option<Arc<dyn VfsInode>> = None;
    let mut run = || -> Result<(), VfsError> {
        let (dentry_gen, inode_gen) = env.cache_gens();
        let sym = parent.parent.symlink(&parent.name, target_str)?;
        created_sym = Some(publish_created_inode(env, &parent.parent, &parent.name, sym, dentry_gen, inode_gen));
        Ok(())
    };
    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run)?,
        None => run()?,
    }
    Ok(created_sym.expect("symlink run returns Ok only after setting created_sym"))
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
        let old_inode = lookup_and_unify_inode(env, &old_target.parent, &old_target.name)?;

        if env.is_mountpoint(old_inode.dev_id(), old_inode.ino()) {
            return Err(VfsError::Busy);
        }
        if old_target.has_trailing_slash && old_inode.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }

        let (_, new_since_gen) = env.cache_gens();
        let new_inode_opt = if flags.has_exchange() {
            match lookup_and_unify_inode(env, &new_target.parent, &new_target.name) {
                Ok(i) => Some(i),
                Err(VfsError::NoEnt) => None,
                Err(e) => return Err(e),
            }
        } else {
            match new_target.parent.lookup(&new_target.name) {
                Ok(i) => Some(env.inode_get_if_fresh(i.dev_id(), i.ino(), new_since_gen).unwrap_or(i)),
                Err(VfsError::NoEnt) => None,
                Err(e) => return Err(e),
            }
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

        struct PendingRenameRemoval<'a> {
            guard: InodeRemovalGuard<'a>,
            removed_arc: Option<Arc<dyn VfsInode>>,
            orig_ino: Ino,
        }
        let mut removal_guard: Option<PendingRenameRemoval<'_>> = None;
        let mut overwritten_dir_key: Option<(DevId, Ino)> = None;
        let last_link_info = if let Some(ref new_inode) = new_inode_opt {
            let orig_ino = new_inode.ino();
            let nlink = new_inode.stat()?.nlink;
            let last_link = if new_inode.inode_type() == InodeType::Dir { nlink <= 2 } else { nlink <= 1 };
            Some((Arc::clone(new_inode), orig_ino, last_link))
        } else {
            None
        };

        env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), &old_target.name);
        env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), &new_target.name);

        if let Some((ref new_inode, orig_ino, last_link)) = last_link_info {
            if new_inode.inode_type() == InodeType::Dir {
                let dir_key = (new_inode.dev_id(), orig_ino);
                env.dentry_invalidate_dir(dir_key.0, dir_key.1);
                overwritten_dir_key = Some(dir_key);
            }
            if last_link {
                let (guard, removed_arc) = InodeRemovalGuard::begin(env, new_inode.dev_id(), orig_ino, new_since_gen);
                removal_guard = Some(PendingRenameRemoval { guard, removed_arc, orig_ino });
            }
        }

        if let Err(err) = old_target.parent.rename(&old_target.name, &new_target.parent, &new_target.name, flags) {
            if let Some(pending) = removal_guard {
                let end_gen = pending.guard.finish();
                if let Some(removed_arc) = pending.removed_arc
                    && !removed_arc.is_detached()
                    && removed_arc.ino() == pending.orig_ino
                    && matches!(removed_arc.stat(), Ok(m) if m.nlink > 0)
                {
                    let _ = env.inode_get_or_insert_if_fresh(removed_arc, end_gen);
                }
            }
            return Err(err);
        }

        if let Some(pending) = removal_guard {
            let removed_was_none = pending.removed_arc.is_none();
            let pending_dev = pending.guard.dev;
            let pending_ino = pending.orig_ino;
            let _ = pending.guard.finish();
            if removed_was_none {
                evict_if_cached_zombie(env, pending_dev, pending_ino);
            }
        }
        if let Some((new_inode, orig_ino, false)) = last_link_info {
            let last_gone = new_inode.is_detached()
                || new_inode.ino() != orig_ino
                || matches!(new_inode.stat(), Ok(m) if m.nlink == 0)
                || matches!(new_inode.stat(), Err(VfsError::NoEnt));
            if last_gone {
                evict_zombie_inode(env, new_inode.dev_id(), orig_ino);
            }
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
