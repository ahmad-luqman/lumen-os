//! High-level POSIX namespace operations (`open`, `mkdir`, `unlink`, `rmdir`,
//! `link`, `symlink`, `readlink`, `renameat2`) coordinating `resolve_path`,
//! Lock-9 cache invalidation, Lock-3 `rename_lock`, and Gate 6 detach rules.

// A last component (`resolve::Final`) is read here only as the scrutinee of
// a `match` on `final_component()` whose arms are `Final::` patterns (no
// `let ... else`, `if let`, `matches!`, tuple, or `Final` held in a
// variable: the host test `ops_reads_final_only_through_a_direct_match`
// refuses those), so an operation that leaves a case out doesn't compile,
// and `make lint` refuses a `_` or catch-all arm answering them all alike
// (#309). The deny covers every enum matched in this file, not only
// `Final`: name the cases of the others too, or allow it on one match with
// the reason.
#![deny(clippy::wildcard_enum_match_arm)]

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::cache::DentryTarget;
use crate::error::VfsError;
use crate::fs::{OpenGuard, Refused, VfsInode};
use crate::mount::VfsCacheOps;
use crate::resolve::{
    Final, MAX_INODE_RACE_RETRIES, ResolveFlags, cross_mounts_down, resolve_and_unify_magic_target, resolve_parent,
    resolve_parent_with_depth, resolve_path, resolve_path_with_depth,
};
use crate::types::{DevId, Ino, InodeType, OpenFlags, RenameFlags};
use crate::{MAX_SYMLINK_DEPTH, PATH_MAX};

/// Upper bound on each of non-exclusive `O_CREAT`'s two race retries: a
/// create that keeps finding the name taken, and a found file that keeps
/// being freed before the open's reference goes up. Running out of either
/// fails the open with `EEXIST` (the name kept coming back).
pub(crate) const MAX_CREAT_RACE_RETRIES: usize = 64;

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
    name: &[u8],
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
    name: &[u8],
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

/// What [`open_inode_guarded`] opened: the open reference it took, the path
/// and the flags the open checked, for
/// [`OpenFile::from_opened`](crate::fd::OpenFile::from_opened). The flags
/// can't be changed between the two, so a descriptor's access mode is always
/// the one the open checked (`EROFS`, `EISDIR`, `O_PATH`).
#[must_use = "dropping an Opened gives its open reference back"]
pub struct Opened {
    guard: OpenGuard,
    path: Arc<[u8]>,
    flags: OpenFlags,
}

impl Opened {
    /// The opened inode.
    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        self.guard.inode()
    }

    /// The path as given, its bytes (#268).
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The flags the open checked.
    pub fn flags(&self) -> OpenFlags {
        self.flags
    }

    pub(crate) fn into_parts(self) -> (OpenGuard, Arc<[u8]>, OpenFlags) {
        (self.guard, self.path, self.flags)
    }
}

/// Open or create a file/directory/device inode according to POSIX/`openat(2)`
/// semantics, returning `(inode, normalized_path_string)` with no open
/// reference held: [`open_inode_guarded`] with its reference given back.
///
/// The inode was live when the open checked it, but an `unlink` can free it
/// once this returns. An open file description built from it with
/// [`OpenFile::new_unchecked_flags`](crate::fd::OpenFile::new_unchecked_flags)
/// then fails with `ENOENT` (that call checks again); an
/// [`OpenGuard::new_unchecked`] on it can hold a freed inode. File
/// descriptors use [`open_inode_guarded`] and
/// [`OpenFile::from_opened`](crate::fd::OpenFile::from_opened). The brief
/// reference it holds while opening can make a racing last-link unlink
/// detach the file instead of freeing it, which on ext4 copies it into
/// detached memory and can fail that unlink with `ENOSPC` over the quota.
pub fn open_inode(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: OpenFlags,
    mode: u16,
    umask: u16,
) -> Result<(Arc<dyn VfsInode>, Vec<u8>), VfsError> {
    let (guard, path_str, _) = open_inode_guarded(env, start_dir, path, flags, mode, umask)?.into_parts();
    Ok((Arc::clone(guard.inode()), path_str.to_vec()))
}

/// [`open_inode`] returning the open reference it took, as an [`Opened`] for
/// [`OpenFile::from_opened`](crate::fd::OpenFile::from_opened).
///
/// The reference is taken ([`OpenGuard::try_new`]) before the last check
/// that the inode is live and before `O_TRUNC`'s truncate, so an `unlink`
/// racing the open either detaches the inode (Gate 6 sees the reference;
/// the data stays readable) or freed it first (`ENOENT`, as if the lookup
/// had lost the race). Under `O_CREAT` a lost race opens again instead
/// (POSIX: the old file or a new one, never `ENOENT`); after
/// `MAX_CREAT_RACE_RETRIES` more lost races it fails with `EEXIST`, as the
/// create loop does when the name keeps being taken. Only a freed inode is
/// retried: a `stat()` error passes through unchanged. A file this open
/// creates takes an unchecked reference ([`OpenGuard::new_unchecked`]) under
/// its filesystem's Lock 3, by design: on a filesystem with a rename lock
/// (ext4, tmpfs) no unlink can come between, and the open must succeed even
/// if one follows at once. Every error gives the reference back.
pub fn open_inode_guarded(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: OpenFlags,
    mode: u16,
    umask: u16,
) -> Result<Opened, VfsError> {
    for _ in 0..=MAX_CREAT_RACE_RETRIES {
        if let Some((guard, path)) = open_once(env, start_dir, path, flags, mode, umask)? {
            return Ok(Opened { guard, path, flags });
        }
    }
    Err(VfsError::Exist)
}

/// What one attempt of [`open_inode_guarded`] opened: the reference and the path.
type OpenedOnce = (OpenGuard, Arc<[u8]>);

/// One attempt of [`open_inode_guarded`]: `Ok(None)` when an `O_CREAT` open
/// found a file that an unlink freed before the reference went up.
fn open_once(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: OpenFlags,
    mode: u16,
    umask: u16,
) -> Result<Option<OpenedOnce>, VfsError> {
    if flags.has_tmpfile() {
        return Err(VfsError::OpNotSupp);
    }
    let _ = flags.access_mode()?;

    let path_str: Arc<[u8]> = Arc::from(path);

    if flags.is_path() {
        let resolved = resolve_path(
            env,
            start_dir,
            path,
            ResolveFlags { follow_final: !flags.has_nofollow(), empty_path: false, require_dir: flags.has_directory() },
        )?;
        return Ok(Some((OpenGuard::try_new(resolved.inode)?, path_str)));
    }

    let inode = if flags.has_creat() {
        let mut symlink_depth = 0usize;
        let mut first_hop = true;
        let mut cur_start_dir: Option<Arc<dyn VfsInode>> = start_dir.map(Arc::clone);
        let mut cur_path: Vec<u8> = path.to_vec();

        'follow: loop {
            let target = resolve_parent_with_depth(env, cur_start_dir.as_ref(), &cur_path, &mut symlink_depth)?;
            // `.` or `..` (without `O_EXCL`): the directory it names.
            let mut dots_dir = |dots: &[u8]| -> Result<Arc<dyn VfsInode>, VfsError> {
                Ok(resolve_path_with_depth(
                    env,
                    Some(&target.parent),
                    dots,
                    ResolveFlags { follow_final: !flags.has_nofollow(), empty_path: false, require_dir: true },
                    &mut symlink_depth,
                )?
                .inode)
            };
            let name = match target.final_component() {
                Final::Root | Final::Dot | Final::DotDot if flags.has_excl() => return Err(VfsError::Exist),
                Final::Root => break 'follow cross_mounts_down(env, Arc::clone(&target.parent)),
                Final::Dot => break 'follow dots_dir(b".")?,
                Final::DotDot => break 'follow dots_dir(b"..")?,
                Final::Name(name) => name,
            };
            if first_hop && target.has_trailing_slash {
                return Err(VfsError::IsDir);
            }

            enum CreatStep {
                Done(Arc<dyn VfsInode>),
                FollowSymlink(Arc<dyn VfsInode>),
                // The open reference, taken right after the create inside
                // `with_rename_lock`. On a filesystem with a rename lock
                // (ext4, tmpfs; the trait's default takes none) no unlink,
                // which takes it too, can come between, so the file this
                // open made is never freed under it, and the open succeeds
                // even if an unlink follows at once (POSIX).
                Created(OpenGuard),
            }

            let mut step: Option<CreatStep> = None;
            let fs_opt = env.fs_by_dev(target.parent.dev_id());
            let mut run_creat = || -> Result<(), VfsError> {
                let mut creat_retries = 0usize;
                let mut inode_retries = 0usize;
                loop {
                    let (dentry_gen, inode_gen) = env.cache_gens();
                    match target.parent.lookup(name) {
                        Ok(existing) => {
                            if flags.has_excl() {
                                return Err(VfsError::Exist);
                            }
                            let existing_ino = existing.ino();
                            let existing = match env.publish_lookup_inode(
                                target.parent.dev_id(),
                                target.parent.ino(),
                                name,
                                Arc::clone(&existing),
                                dentry_gen,
                                inode_gen,
                            ) {
                                Ok(unified) => {
                                    if !Arc::ptr_eq(&unified, &existing)
                                        && (is_inode_dead(&unified) || unified.ino() != existing_ino)
                                    {
                                        env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), name);
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
                            match target.parent.create(name, eff_mode) {
                                Ok(created) => {
                                    let created = publish_created_inode(
                                        env,
                                        &target.parent,
                                        name,
                                        created,
                                        dentry_gen,
                                        inode_gen,
                                    );
                                    step = Some(CreatStep::Created(OpenGuard::new_unchecked(created)));
                                    return Ok(());
                                }
                                Err(VfsError::Exist) if !flags.has_excl() => {
                                    if creat_retries >= MAX_CREAT_RACE_RETRIES {
                                        // See `MAX_CREAT_RACE_RETRIES`.
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
                CreatStep::Created(guard) => return Ok(Some((guard, path_str))),
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
    // A special file is written through, not on, its filesystem: no `-EROFS`
    // (`InodeType::is_special`, as Linux's `do_dentry_open` skips
    // `mnt_want_write` for one), as `check_access` agrees.
    #[cfg(test)]
    inode.at_rofs_check();
    if !inode.inode_type().is_special()
        && env.fs_read_only(inode.dev_id())
        && (flags.is_writable() || flags.has_trunc())
    {
        return Err(VfsError::RoFs);
    }
    // The open reference, then the last liveness check (inside `try_new`),
    // then the truncate: never a truncate of, or a guard on, a freed inode.
    let guard = match OpenGuard::try_new_or_refused(inode) {
        Ok(guard) => guard,
        // An `O_CREAT` open found a file (an existing name, or a followed
        // symlink's target; a directory failed with `EISDIR` above) that an
        // unlink freed once Lock 3 was released. POSIX gives `O_CREAT` the
        // old file or a new one, never `ENOENT`: open again.
        Err(Refused::Freed) if flags.has_creat() => return Ok(None),
        Err(r) => return Err(r.into()),
    };
    if flags.has_trunc() && guard.inode().inode_type() == InodeType::File {
        guard.inode().truncate(0)?;
    }

    Ok(Some((guard, path_str)))
}

/// `mkdirat(2)`: create a directory with `(mode & 0o7777) & !umask`. The
/// kernel's `mkdirat` hands it `Mode::dir_perm` (`mode & 0o1777`: no setuid
/// or setgid, as Linux's `vfs_mkdir`), so only sticky survives of the
/// special bits there.
pub fn mkdir_at(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    mode: u16,
    umask: u16,
) -> Result<Arc<dyn VfsInode>, VfsError> {
    let target = resolve_parent(env, start_dir, path)?;
    let name = match target.final_component() {
        Final::Root | Final::Dot | Final::DotDot => return Err(VfsError::Exist),
        Final::Name(name) => name,
    };
    if env.fs_read_only(target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }
    let eff_mode = (mode & 0o7777) & !umask;
    let fs_opt = env.fs_by_dev(target.parent.dev_id());
    let mut created_dir: Option<Arc<dyn VfsInode>> = None;
    let mut run = || -> Result<(), VfsError> {
        let (dentry_gen, inode_gen) = env.cache_gens();
        let dir = target.parent.mkdir(name, eff_mode)?;
        created_dir = Some(publish_created_inode(env, &target.parent, name, dir, dentry_gen, inode_gen));
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
    // A last component that isn't a name: Linux's `do_unlinkat` says
    // `-EISDIR` for all three; `do_rmdir` tells them apart.
    let name = match target.final_component() {
        Final::Root => return Err(if remove_dir { VfsError::Busy } else { VfsError::IsDir }),
        Final::Dot => return Err(if remove_dir { VfsError::Inval } else { VfsError::IsDir }),
        Final::DotDot => return Err(if remove_dir { VfsError::NotEmpty } else { VfsError::IsDir }),
        Final::Name(name) => name,
    };
    if env.fs_read_only(target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }

    let fs_opt = env.fs_by_dev(target.parent.dev_id());
    let mut run = || -> Result<(), VfsError> {
        let (_, since_gen) = env.cache_gens();
        let raw_child = target.parent.lookup(name)?;
        let child = env.inode_get_if_fresh(raw_child.dev_id(), raw_child.ino(), since_gen).unwrap_or(raw_child);

        if !remove_dir {
            if child.inode_type() == InodeType::Dir {
                return Err(VfsError::IsDir);
            }
            // A trailing slash on anything but a directory: `-ENOTDIR`, the
            // last component looked at as it is (a symlink to a directory
            // included), as Linux's `do_unlinkat`.
            if target.has_trailing_slash {
                return Err(VfsError::NotDir);
            }

            let old_dev = child.dev_id();
            let old_ino = child.ino();
            let removing_last_link = child.stat()?.nlink <= 1;

            env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), name);
            let removal_guard = if removing_last_link {
                Some(InodeRemovalGuard::begin(env, old_dev, old_ino, since_gen))
            } else {
                None
            };

            if let Err(err) = target.parent.unlink(name) {
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

            env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), name);
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
            env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), name);
            env.dentry_invalidate_dir(old_dev, old_ino);
            let (guard, removed_arc_opt) = InodeRemovalGuard::begin(env, old_dev, old_ino, since_gen);

            if let Err(err) = target.parent.rmdir(name) {
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
            env.dentry_invalidate(target.parent.dev_id(), target.parent.ino(), name);
            env.dentry_invalidate_dir(old_dev, old_ino);
            Ok(())
        }
    };

    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run),
        None => run(),
    }
}

/// `linkat(2)`: create a hard link. A new name in use is `-EEXIST` (with a
/// trailing slash too) and a trailing slash on a free one `-ENOENT`, before
/// a read-only filesystem (`-EROFS`), a cross-mount target (`-EXDEV`) and a
/// directory (`-EPERM`), in Linux's order.
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

    // Linux's order (`do_linkat`, `filename_create`, `vfs_link`): the new
    // name's `-EEXIST` (a name in use, with a trailing slash too) or
    // `-ENOENT` (a trailing slash on a free one), then `-EROFS`, then
    // `-EXDEV`, then a directory's `-EPERM`.
    let new_target = resolve_parent(env, new_dir, new_path)?;
    let new_name = match new_target.final_component() {
        Final::Root | Final::Dot | Final::DotDot => return Err(VfsError::Exist),
        Final::Name(name) => name,
    };
    match new_target.parent.lookup(new_name) {
        Ok(_) => return Err(VfsError::Exist),
        Err(VfsError::NoEnt) if new_target.has_trailing_slash => return Err(VfsError::NoEnt),
        Err(VfsError::NoEnt) => {}
        Err(e) => return Err(e),
    }
    if env.fs_read_only(new_target.parent.dev_id()) {
        return Err(VfsError::RoFs);
    }
    if old.inode.dev_id() != new_target.parent.dev_id() {
        return Err(VfsError::XDev);
    }
    if old.inode.inode_type() == InodeType::Dir {
        return Err(VfsError::Perm);
    }

    let fs_opt = env.fs_by_dev(new_target.parent.dev_id());
    let mut run = || -> Result<(), VfsError> {
        if old.inode.is_detached() || old.inode.stat()?.nlink == 0 {
            return Err(VfsError::NoEnt);
        }

        let inv_gen = env.dentry_gen();
        new_target.parent.link(new_name, &old.inode)?;
        let _ = env.dentry_insert_if_gen(
            new_target.parent.dev_id(),
            new_target.parent.ino(),
            new_name,
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

    let parent = resolve_parent(env, new_dir, linkpath)?;
    let name = match parent.final_component() {
        Final::Root | Final::Dot | Final::DotDot => return Err(VfsError::Exist),
        Final::Name(name) => name,
    };
    if parent.has_trailing_slash {
        return match parent.parent.lookup(name) {
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
        let sym = parent.parent.symlink(name, target)?;
        created_sym = Some(publish_created_inode(env, &parent.parent, name, sym, dentry_gen, inode_gen));
        Ok(())
    };
    match fs_opt {
        Some(fs) => fs.with_rename_lock(&mut run)?,
        None => run()?,
    }
    Ok(created_sym.expect("symlink run returns Ok only after setting created_sym"))
}

/// `readlinkat(2)`: read symlink target without following the final component.
/// Supports `path == b""` on an `O_PATH | O_NOFOLLOW` symlink descriptor. Not
/// a symlink: `-EINVAL`, or `-ENOENT` for an empty path (Linux's
/// `do_readlinkat`, `fs/stat.c`).
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
    // Not a symlink: `-EINVAL`, but `-ENOENT` for an empty path (Linux's
    // `do_readlinkat`).
    if resolved.inode.inode_type() != InodeType::Symlink {
        return Err(if path.is_empty() { VfsError::NoEnt } else { VfsError::Inval });
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
        let parent = cur.lookup(b"..")?;
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

    // Linux's order (`do_renameat2`): `-EXDEV` across mounts, ahead of the
    // `-EBUSY` a last component that isn't a name (`/`, `.`, `..`) gives on
    // the old side; on the new side such a name is `-EEXIST` with
    // `RENAME_NOREPLACE`, else `-EBUSY`.
    let dev = old_target.parent.dev_id();
    if dev != new_target.parent.dev_id() {
        return Err(VfsError::XDev);
    }
    let old_name = match old_target.final_component() {
        Final::Root | Final::Dot | Final::DotDot => return Err(VfsError::Busy),
        Final::Name(name) => name,
    };
    let new_name = match new_target.final_component() {
        Final::Root | Final::Dot | Final::DotDot => {
            return Err(if flags.has_noreplace() { VfsError::Exist } else { VfsError::Busy });
        }
        Final::Name(name) => name,
    };
    if env.fs_read_only(dev) {
        return Err(VfsError::RoFs);
    }

    let fs_opt = env.fs_by_dev(dev);
    let mut run = || -> Result<(), VfsError> {
        // The checks below follow Linux's `do_renameat2` and `vfs_rename`, in
        // their order: the old name's `-ENOENT`; `RENAME_NOREPLACE`'s
        // `-EEXIST`; `RENAME_EXCHANGE`'s missing (`-ENOENT`) or slashed
        // non-directory (`-ENOTDIR`) new name; when the source isn't a
        // directory, a trailing slash on either name (`-ENOTDIR`); the
        // ancestry traps (`lock_rename`); the same inode both sides (a
        // no-op); the types (`may_delete`); a mountpoint (`-EBUSY`).
        let old_inode = lookup_and_unify_inode(env, &old_target.parent, old_name)?;
        let old_is_dir = old_inode.inode_type() == InodeType::Dir;

        let (_, new_since_gen) = env.cache_gens();
        let new_inode_opt = if flags.has_exchange() {
            match lookup_and_unify_inode(env, &new_target.parent, new_name) {
                Ok(i) => Some(i),
                Err(VfsError::NoEnt) => None,
                Err(e) => return Err(e),
            }
        } else {
            match new_target.parent.lookup(new_name) {
                Ok(i) => Some(env.inode_get_if_fresh(i.dev_id(), i.ino(), new_since_gen).unwrap_or(i)),
                Err(VfsError::NoEnt) => None,
                Err(e) => return Err(e),
            }
        };

        if flags.has_noreplace() && new_inode_opt.is_some() {
            return Err(VfsError::Exist);
        }
        if flags.has_exchange() {
            let Some(ref new_inode) = new_inode_opt else {
                return Err(VfsError::NoEnt);
            };
            if new_target.has_trailing_slash && new_inode.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
        }
        // When the source isn't a directory, a trailing slash on its name, or
        // (unless exchanging) on the new name, is `-ENOTDIR`, whatever the
        // new name is.
        if !old_is_dir && (old_target.has_trailing_slash || (!flags.has_exchange() && new_target.has_trailing_slash)) {
            return Err(VfsError::NotDir);
        }
        // The source an ancestor of the new name's directory: `-EINVAL`. The
        // new name an ancestor of the source's directory: `-ENOTEMPTY`
        // (`-EINVAL` when exchanging).
        if old_is_dir && is_same_or_descendant(&old_inode, &new_target.parent)? {
            return Err(VfsError::Inval);
        }
        if let Some(ref new_inode) = new_inode_opt
            && new_inode.inode_type() == InodeType::Dir
            && is_same_or_descendant(new_inode, &old_target.parent)?
        {
            return Err(if flags.has_exchange() { VfsError::Inval } else { VfsError::NotEmpty });
        }
        if let Some(ref new_inode) = new_inode_opt
            && old_inode.dev_id() == new_inode.dev_id()
            && old_inode.ino() == new_inode.ino()
        {
            return Ok(());
        }
        if !flags.has_exchange()
            && let Some(ref new_inode) = new_inode_opt
        {
            let new_is_dir = new_inode.inode_type() == InodeType::Dir;
            if old_is_dir && !new_is_dir {
                return Err(VfsError::NotDir);
            }
            if !old_is_dir && new_is_dir {
                return Err(VfsError::IsDir);
            }
        }
        if env.is_mountpoint(old_inode.dev_id(), old_inode.ino())
            || new_inode_opt.as_ref().is_some_and(|n| env.is_mountpoint(n.dev_id(), n.ino()))
        {
            return Err(VfsError::Busy);
        }

        if flags.has_exchange() {
            env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), old_name);
            env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), new_name);

            old_target.parent.rename(old_name, &new_target.parent, new_name, flags)?;

            env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), old_name);
            env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), new_name);
            return Ok(());
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

        env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), old_name);
        env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), new_name);

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

        if let Err(err) = old_target.parent.rename(old_name, &new_target.parent, new_name, flags) {
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

        env.dentry_invalidate(old_target.parent.dev_id(), old_target.parent.ino(), old_name);
        env.dentry_invalidate(new_target.parent.dev_id(), new_target.parent.ino(), new_name);
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
