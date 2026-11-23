//! Physical component-by-component path & symlink resolution, `cwd` path
//! reconstruction, and `access(2)` permission checks.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::error::VfsError;
use crate::fs::VfsInode;
use crate::mount::VfsCacheOps;
use crate::types::{AccessMask, Escaped, InodeType};
use crate::{MAX_SYMLINK_DEPTH, NAME_MAX, PATH_MAX};

/// Upper bound on bounded `InodeCache` race retries.
pub(crate) const MAX_INODE_RACE_RETRIES: usize = 64;

/// Upper bound on bounded `compute_cwd_path` retries across concurrent `rename` races.
const MAX_CWD_RENAME_RETRIES: usize = 64;

/// Options controlling [`resolve_path`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolveFlags {
    /// Follow a symbolic link if it is the final component (note: a trailing
    /// `/` always forces following the symlink even when `follow_final == false`).
    pub follow_final: bool,
    /// Allow empty `path == b""` to resolve directly to `start_dir` (`AT_EMPTY_PATH`).
    pub empty_path: bool,
    /// Require the resolved target to be a directory (`-ENOTDIR` otherwise).
    pub require_dir: bool,
}

/// Output of [`resolve_path`].
#[derive(Clone)]
pub struct ResolvedPath {
    pub inode: Arc<dyn VfsInode>,
    pub has_trailing_slash: bool,
}

impl fmt::Debug for ResolvedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedPath")
            .field("dev", &self.inode.dev_id())
            .field("ino", &self.inode.ino())
            .field("type", &self.inode.inode_type())
            .field("has_trailing_slash", &self.has_trailing_slash)
            .finish()
    }
}

/// Output of [`resolve_parent`]: the directory a namespace operation works
/// in and the path's last component, borrowed from the path and handed out
/// as a [`Final`] by [`ParentTarget::final_component`] (#309).
#[derive(Clone)]
pub struct ParentTarget<'a> {
    pub parent: Arc<dyn VfsInode>,
    /// Private, so a `ParentTarget` comes only from [`resolve_parent`],
    /// which classifies the component, and a name's bytes come only out of
    /// a pattern on [`ParentTarget::final_component`]'s [`Final`].
    last: Final<'a>,
    pub has_trailing_slash: bool,
}

/// A path's last component, as [`ParentTarget::final_component`] gives it.
///
/// Only [`Final::Name`] is an entry a filesystem can create, remove or
/// rename; for the other three each namespace operation has its own errno
/// (`mkdirat`'s `-EEXIST`; `rmdir`'s `-EBUSY`, `-EINVAL` and `-ENOTEMPTY`;
/// ...). A name's bytes come only out of a pattern on it, so the other
/// three always get some answer. `ops.rs` reads it only by a `match` on
/// `final_component()`, which doesn't compile if it leaves a case out;
/// clippy refuses a `_` or catch-all arm there
/// (`clippy::wildcard_enum_match_arm`, denied in that file), and a host
/// test, `ops_reads_final_only_through_a_direct_match`, refuses any other
/// way of reading one there (`let ... else`, `if let`, `matches!`, a
/// tuple, a method path). Neither sees a helper whose own `match` answers
/// the three alike for several operations; review does, and code outside
/// `ops.rs` is held to all of it by convention only.
///
/// Stable rustdoc doesn't check a `compile_fail` example's error code, so
/// each one is followed by a twin that is the same but for the part meant
/// to fail, and compiles: it fails for that reason, not another.
///
/// ```compile_fail
/// use lumen_vfs::resolve::{Final, ParentTarget};
///
/// fn name<'a>(target: &ParentTarget<'a>) -> Option<&'a [u8]> {
///     match target.final_component() {
///         Final::Root | Final::Dot => None,
///         Final::Name(name) => Some(name),
///     }
/// }
/// ```
///
/// ```
/// use lumen_vfs::resolve::{Final, ParentTarget};
///
/// fn name<'a>(target: &ParentTarget<'a>) -> Option<&'a [u8]> {
///     match target.final_component() {
///         Final::Root | Final::Dot | Final::DotDot => None,
///         Final::Name(name) => Some(name),
///     }
/// }
/// ```
///
/// Nor is there a field to take it from instead: the component's own is
/// private (this would compile were it `pub`)...
///
/// ```compile_fail
/// use lumen_vfs::resolve::{Final, ParentTarget};
///
/// fn last<'a>(target: &ParentTarget<'a>) -> Final<'a> {
///     target.last
/// }
/// ```
///
/// ```
/// use lumen_vfs::resolve::{Final, ParentTarget};
///
/// fn last<'a>(target: &ParentTarget<'a>) -> Final<'a> {
///     target.final_component()
/// }
/// ```
///
/// ...and the name's bytes have none (this would compile were they a
/// public `name: Vec<u8>` or `&[u8]` again):
///
/// ```compile_fail
/// fn is_empty(target: &lumen_vfs::resolve::ParentTarget<'_>) -> bool {
///     target.name.is_empty()
/// }
/// ```
///
/// ```
/// fn is_empty(target: &lumen_vfs::resolve::ParentTarget<'_>) -> bool {
///     !target.has_trailing_slash
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Final<'a> {
    /// The path was only slashes (`/`, `//`): the parent is the global root.
    Root,
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// A name in the parent. As [`resolve_parent`] makes it (the variant
    /// itself checks nothing): never empty, `.` or `..`, at most
    /// `NAME_MAX` bytes, and holding neither `/` nor NUL; any other bytes,
    /// UTF-8 or not (#268).
    Name(&'a [u8]),
}

impl fmt::Debug for Final<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Final::Root => f.write_str("Root"),
            Final::Dot => f.write_str("Dot"),
            Final::DotDot => f.write_str("DotDot"),
            Final::Name(name) => f.debug_tuple("Name").field(&Escaped(name)).finish(),
        }
    }
}

impl<'a> ParentTarget<'a> {
    /// The path's last component.
    pub fn final_component(&self) -> Final<'a> {
        self.last
    }
}

impl fmt::Debug for ParentTarget<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParentTarget")
            .field("parent_dev", &self.parent.dev_id())
            .field("parent_ino", &self.parent.ino())
            .field("final", &self.last)
            .field("has_trailing_slash", &self.has_trailing_slash)
            .finish()
    }
}

/// Cross downward through any stacked mounts at `cur` without holding Lock 9
/// across `FileSystem::root_inode()` or leaking unmounted root inodes.
pub(crate) fn cross_mounts_down(env: &dyn VfsCacheOps, mut cur: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
    for _ in 0..MAX_INODE_RACE_RETRIES {
        let Some((fs, mount_id)) = env.mount_at_with_id(cur.dev_id(), cur.ino()) else {
            break;
        };
        let raw_root = fs.root_inode();
        let Some(next) = env.inode_get_or_insert_if_mounted(cur.dev_id(), cur.ino(), mount_id, raw_root) else {
            continue;
        };
        if next.dev_id() == cur.dev_id() && next.ino() == cur.ino() {
            break;
        }
        cur = next;
    }
    cur
}

/// Resolve the global root `/`, unify it in `InodeCache` if fresh, and cross
/// any mounts stacked directly on `/`.
pub(crate) fn resolve_global_root(env: &dyn VfsCacheOps) -> Result<Arc<dyn VfsInode>, VfsError> {
    let mut first_root: Option<Arc<dyn VfsInode>> = None;
    for _ in 0..=MAX_INODE_RACE_RETRIES {
        let (_, since_gen) = env.cache_gens();
        let root = env.root_inode()?;
        match env.inode_get_or_insert_if_fresh(root, since_gen) {
            Ok(unified) => return Ok(cross_mounts_down(env, unified)),
            Err(stale) => {
                if first_root.is_none() {
                    first_root = Some(stale);
                }
            }
        }
    }
    Ok(cross_mounts_down(env, first_root.expect("loop ran at least once")))
}

/// Resolve a magic symlink on `link_inode`, capturing `cache_gens()` before
/// `resolve_magic_link()` so concurrent unlinks/`ino` reuse are detected, and
/// unify the live target in `InodeCache` with bounded retries.
pub(crate) fn resolve_and_unify_magic_target(
    env: &dyn VfsCacheOps,
    link_inode: &Arc<dyn VfsInode>,
) -> Option<Arc<dyn VfsInode>> {
    let mut first_target: Option<Arc<dyn VfsInode>> = None;
    for attempt in 0..=MAX_INODE_RACE_RETRIES {
        let (_, since_gen) = env.cache_gens();
        let Some(magic_target) = link_inode.resolve_magic_link() else {
            return if attempt > 0 { first_target } else { None };
        };
        if magic_target.is_detached() {
            return Some(first_target.unwrap_or(magic_target));
        }
        match env.inode_get_or_insert_if_fresh(magic_target, since_gen) {
            Ok(unified) => return Some(unified),
            Err(stale) => {
                if stale.is_detached() {
                    return Some(first_target.unwrap_or(stale));
                }
                if first_target.is_none() {
                    first_target = Some(stale);
                }
            }
        }
    }
    first_target
}

/// Look up `".."` on `cur` and unify it in `InodeCache` when fresh, with
/// bounded retries so concurrent unlinks/`ino` reuse never alias a wrong inode.
fn lookup_dotdot(env: &dyn VfsCacheOps, cur: &Arc<dyn VfsInode>) -> Result<Arc<dyn VfsInode>, VfsError> {
    let mut first_parent: Option<Arc<dyn VfsInode>> = None;
    for attempt in 0..=MAX_INODE_RACE_RETRIES {
        let (_, since_gen) = env.cache_gens();
        let parent = match cur.lookup(b"..") {
            Ok(p) => p,
            Err(VfsError::NoEnt) if attempt > 0 => {
                return Ok(first_parent.expect("first_parent set before retry"));
            }
            Err(e) => return Err(e),
        };
        match env.inode_get_or_insert_if_fresh(parent, since_gen) {
            Ok(unified) => return Ok(unified),
            Err(stale) => {
                if first_parent.is_none() {
                    first_parent = Some(stale);
                }
            }
        }
    }
    Ok(first_parent.expect("loop ran at least once"))
}

/// Cross upward across stacked mount roots before `..` lookup.
fn cross_mounts_up(env: &dyn VfsCacheOps, mut cur: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
    while let Some(mp) = env.parent_mount_if_root(cur.dev_id(), cur.ino()) {
        if mp.dev_id() == cur.dev_id() && mp.ino() == cur.ino() {
            break;
        }
        cur = mp;
    }
    cur
}

/// Resolve one normal component using atomic Lock-9 probes (`DentryCache ->
/// InodeCache`) followed by Lock-4 `VfsInode::lookup` only on cache miss, with
/// bounded retry and `first_child` fallback on concurrent invalidation races.
fn lookup_component(
    env: &dyn VfsCacheOps,
    dir: &Arc<dyn VfsInode>,
    name: &[u8],
) -> Result<Arc<dyn VfsInode>, VfsError> {
    let pdev = dir.dev_id();
    let pino = dir.ino();
    let mut first_child: Option<Arc<dyn VfsInode>> = None;

    for attempt in 0..=MAX_INODE_RACE_RETRIES {
        let (cached_inode, dentry_gen, inode_gen) = env.dentry_lookup_inode(pdev, pino, name);
        if let Some(inode) = cached_inode {
            return Ok(inode);
        }

        let child = match dir.lookup(name) {
            Ok(c) => c,
            Err(VfsError::NoEnt) if attempt > 0 => {
                return Ok(first_child.expect("first_child set before retry"));
            }
            Err(e) => return Err(e),
        };
        // A cached child of a non-cacheable directory would leave a dentry
        // keyed by its parent that outlives the parent (`VfsInode::cacheable`).
        debug_assert!(
            dir.cacheable() || !child.cacheable(),
            "{:?}: a cacheable child of a non-cacheable directory",
            Escaped(name)
        );

        let child_ino = child.ino();
        match env.publish_lookup_inode(pdev, pino, name, Arc::clone(&child), dentry_gen, inode_gen) {
            Ok(unified) => {
                if !Arc::ptr_eq(&unified, &child)
                    && (unified.is_detached()
                        || unified.ino() != child_ino
                        || matches!(unified.stat(), Ok(m) if m.nlink == 0)
                        || matches!(unified.stat(), Err(VfsError::NoEnt)))
                {
                    env.dentry_invalidate(pdev, pino, name);
                    let _ = env.inode_remove_if_same(unified.dev_id(), child_ino, &unified);
                    if first_child.is_none() {
                        first_child = Some(child);
                    }
                    continue;
                }
                return Ok(unified);
            }
            Err(stale) => {
                if first_child.is_none() {
                    first_child = Some(stale);
                }
            }
        }
    }

    Ok(first_child.expect("loop ran at least once"))
}

/// Resolve `path` relative to `start_dir` (or global root when `start_dir` is `None`).
///
/// Enforces:
/// - `PATH_MAX = 4096`, `NAME_MAX = 255` (`-ENAMETOOLONG`)
/// - empty path `-ENOENT` unless `flags.empty_path`
/// - physical component-by-component `.` and `..` traversal (no lexical collapse)
/// - `MAX_SYMLINK_DEPTH = 40` (`-ELOOP`)
/// - relative symlinks resolved relative to the symlink's parent directory
/// - trailing `/` forcing symlink traversal and requiring a directory (`-ENOTDIR`)
/// - magic symlinks via [`VfsInode::resolve_magic_link`]
pub fn resolve_path(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: ResolveFlags,
) -> Result<ResolvedPath, VfsError> {
    let mut symlink_depth = 0usize;
    resolve_path_with_depth(env, start_dir, path, flags, &mut symlink_depth)
}

pub(crate) fn resolve_path_with_depth(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    flags: ResolveFlags,
    symlink_depth: &mut usize,
) -> Result<ResolvedPath, VfsError> {
    if path.len() >= PATH_MAX {
        return Err(VfsError::NameTooLong);
    }
    if path.contains(&0) {
        return Err(VfsError::Inval);
    }

    if path.is_empty() {
        if !flags.empty_path {
            return Err(VfsError::NoEnt);
        }
        let target = match start_dir {
            Some(d) => Arc::clone(d),
            None => resolve_global_root(env)?,
        };
        if flags.require_dir && target.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        return Ok(ResolvedPath { inode: target, has_trailing_slash: false });
    }

    let mut has_trailing_slash = path.ends_with(b"/");
    let mut cur = if path.starts_with(b"/") {
        resolve_global_root(env)?
    } else {
        match start_dir {
            Some(d) => Arc::clone(d),
            None => resolve_global_root(env)?,
        }
    };

    let mut rest_buf: Vec<u8> = path.to_vec();
    let mut cursor = 0usize;

    loop {
        while cursor < rest_buf.len() && rest_buf[cursor] == b'/' {
            cursor += 1;
        }
        if cursor >= rest_buf.len() {
            break;
        }

        if cur.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }

        let rem = &rest_buf[cursor..];
        let (comp_len, tail_start) = match rem.iter().position(|&b| b == b'/') {
            Some(pos) => (pos, cursor + pos),
            None => (rem.len(), rest_buf.len()),
        };

        let comp = &rest_buf[cursor..cursor + comp_len];
        if comp.len() > NAME_MAX {
            return Err(VfsError::NameTooLong);
        }

        let tail = &rest_buf[tail_start..];
        let is_last = tail.iter().all(|&b| b == b'/');
        let trailing_after_comp = !tail.is_empty();

        if comp == b"." {
            cursor = tail_start;
            continue;
        }

        if comp == b".." {
            cur = cross_mounts_up(env, cur);
            if !env.is_global_root(cur.dev_id(), cur.ino()) {
                cur = lookup_dotdot(env, &cur)?;
            }
            cur = cross_mounts_down(env, cur);
            cursor = tail_start;
            continue;
        }

        let child = lookup_component(env, &cur, comp)?;

        let should_follow =
            child.inode_type() == InodeType::Symlink && (!is_last || flags.follow_final || trailing_after_comp);

        if should_follow {
            *symlink_depth += 1;
            if *symlink_depth > MAX_SYMLINK_DEPTH {
                return Err(VfsError::Loop);
            }

            if let Some(magic_target) = resolve_and_unify_magic_target(env, &child) {
                cur = cross_mounts_down(env, magic_target);
                cursor = tail_start;
                continue;
            }

            let target = child.readlink()?;
            if target.is_empty() {
                return Err(VfsError::NoEnt);
            }
            if target.contains(&0) {
                return Err(VfsError::Inval);
            }
            let total_len = target.len().checked_add(tail.len()).ok_or(VfsError::NameTooLong)?;
            if total_len >= PATH_MAX {
                return Err(VfsError::NameTooLong);
            }

            let mut next_buf = Vec::with_capacity(total_len);
            next_buf.extend_from_slice(&target);
            next_buf.extend_from_slice(tail);
            has_trailing_slash = next_buf.ends_with(b"/");

            if target.starts_with(b"/") {
                cur = resolve_global_root(env)?;
            }
            rest_buf = next_buf;
            cursor = 0;
            continue;
        }

        cur = cross_mounts_down(env, child);
        if trailing_after_comp && is_last && cur.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        cursor = tail_start;
    }

    if (has_trailing_slash || flags.require_dir) && cur.inode_type() != InodeType::Dir {
        return Err(VfsError::NotDir);
    }

    Ok(ResolvedPath { inode: cur, has_trailing_slash })
}

/// Resolve the parent directory and final component for namespace-mutating
/// operations (`open(O_CREAT)`, `mkdir`, `unlink`, `rmdir`, `link`, `symlink`, `rename`).
/// A [`Final::Name`] borrows its bytes from `path`, UTF-8 or not (#268); a
/// path of only slashes is [`Final::Root`], with `has_trailing_slash` set.
pub fn resolve_parent<'a>(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &'a [u8],
) -> Result<ParentTarget<'a>, VfsError> {
    let mut symlink_depth = 0usize;
    resolve_parent_with_depth(env, start_dir, path, &mut symlink_depth)
}

pub(crate) fn resolve_parent_with_depth<'a>(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &'a [u8],
    symlink_depth: &mut usize,
) -> Result<ParentTarget<'a>, VfsError> {
    if path.len() >= PATH_MAX {
        return Err(VfsError::NameTooLong);
    }
    if path.contains(&0) {
        return Err(VfsError::Inval);
    }
    if path.is_empty() {
        return Err(VfsError::NoEnt);
    }

    let has_trailing_slash = path.ends_with(b"/");
    let mut trimmed_end = path.len();
    while trimmed_end > 0 && path[trimmed_end - 1] == b'/' {
        trimmed_end -= 1;
    }
    if trimmed_end == 0 {
        let root = resolve_global_root(env)?;
        return Ok(ParentTarget { parent: root, last: Final::Root, has_trailing_slash: true });
    }

    let trimmed = &path[..trimmed_end];
    let (parent, name_bytes) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(pos) => {
            let dir_part = if pos == 0 { b"/" as &[u8] } else { &trimmed[..pos] };
            let resolved = resolve_path_with_depth(
                env,
                start_dir,
                dir_part,
                ResolveFlags { follow_final: true, empty_path: false, require_dir: true },
                symlink_depth,
            )?;
            (resolved.inode, &trimmed[pos + 1..])
        }
        None => {
            let dir = match start_dir {
                Some(d) => Arc::clone(d),
                None => resolve_global_root(env)?,
            };
            if dir.inode_type() != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            (dir, trimmed)
        }
    };

    if name_bytes.len() > NAME_MAX {
        return Err(VfsError::NameTooLong);
    }
    let last = match name_bytes {
        b"." => Final::Dot,
        b".." => Final::DotDot,
        name => Final::Name(name),
    };
    Ok(ParentTarget { parent, last, has_trailing_slash })
}

/// Reconstruct the canonical path of `cwd` by walking `".."` and crossing
/// mountpoints up to the global root `/`.
///
/// Retries from `cwd` up to [`MAX_CWD_RENAME_RETRIES`] times if a concurrent
/// `rename` moves `cwd` or an ancestor between `".."` lookup and parent
/// `readdir`, provided `cwd` itself remains live (`!is_detached() && nlink > 0`).
///
/// Returns `Err(NoEnt)` if `cwd` was unlinked (`st_nlink == 0` or detached).
/// The path is bytes, each component the name `readdir` lists (#268).
pub fn compute_cwd_path(env: &dyn VfsCacheOps, cwd: &Arc<dyn VfsInode>) -> Result<Vec<u8>, VfsError> {
    if cwd.inode_type() != InodeType::Dir {
        return Err(VfsError::NotDir);
    }
    if cwd.is_detached() || cwd.stat()?.nlink == 0 {
        return Err(VfsError::NoEnt);
    }

    let mut components: Vec<Vec<u8>> = Vec::new();
    for attempt in 0..=MAX_CWD_RENAME_RETRIES {
        match compute_cwd_path_once(env, cwd, &mut components) {
            Ok(path) => return Ok(path),
            Err(VfsError::NoEnt) => {
                if cwd.is_detached() || cwd.stat()?.nlink == 0 {
                    return Err(VfsError::NoEnt);
                }
                if attempt == MAX_CWD_RENAME_RETRIES {
                    return Err(VfsError::NoEnt);
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(VfsError::NoEnt)
}

fn compute_cwd_path_once(
    env: &dyn VfsCacheOps,
    cwd: &Arc<dyn VfsInode>,
    components: &mut Vec<Vec<u8>>,
) -> Result<Vec<u8>, VfsError> {
    components.clear();
    let mut cur = Arc::clone(cwd);

    for _ in 0..PATH_MAX {
        if env.is_global_root(cur.dev_id(), cur.ino()) {
            break;
        }
        cur = cross_mounts_up(env, cur);
        if env.is_global_root(cur.dev_id(), cur.ino()) {
            break;
        }

        let cur_dev = cur.dev_id();
        let cur_ino = cur.ino();
        let parent = lookup_dotdot(env, &cur)?;
        if parent.dev_id() == cur_dev && parent.ino() == cur_ino {
            return Err(VfsError::NoEnt);
        }

        let found_name = find_child_name_in_dir(&parent, cur_ino)?;
        components.push(found_name);
        cur = parent;
    }

    if !env.is_global_root(cur.dev_id(), cur.ino()) {
        return Err(VfsError::NameTooLong);
    }

    if components.is_empty() {
        return Ok(b"/".to_vec());
    }

    let mut out = Vec::new();
    for comp in components.iter().rev() {
        out.push(b'/');
        out.extend_from_slice(comp);
    }
    if out.len() >= PATH_MAX {
        return Err(VfsError::NameTooLong);
    }
    Ok(out)
}

fn find_child_name_in_dir(parent: &Arc<dyn VfsInode>, target_ino: u64) -> Result<Vec<u8>, VfsError> {
    let mut cursor = 0u64;
    let mut entries = Vec::new();
    loop {
        entries.clear();
        parent.readdir(cursor, &mut entries, 64)?;
        if entries.is_empty() {
            return Err(VfsError::NoEnt);
        }
        let mut next_cursor = cursor;
        for entry in &entries {
            next_cursor = entry.next_cursor;
            if entry.name != b"." && entry.name != b".." && entry.ino == target_ino {
                return Ok(entry.name.clone());
            }
        }
        if next_cursor == cursor {
            return Err(VfsError::NoEnt);
        }
        cursor = next_cursor;
    }
}

/// Check `access(2)` / `faccessat2(2)` permissions against `inode`.
pub fn check_access(
    env: &dyn VfsCacheOps,
    inode: &Arc<dyn VfsInode>,
    mask: AccessMask,
    uid: u32,
    gid: u32,
) -> Result<(), VfsError> {
    let mask = mask.validate()?;
    let meta = inode.stat()?;

    if mask.0 == AccessMask::F_OK {
        return Ok(());
    }

    // A special file passes on a read-only mount, as `open_inode` agrees
    // (`InodeType::is_special`, as Linux's `do_faccessat`).
    if mask.wants_write() && !inode.inode_type().is_special() && env.fs_read_only(inode.dev_id()) {
        return Err(VfsError::RoFs);
    }

    if uid == 0 {
        if mask.wants_exec() && inode.inode_type() != InodeType::Dir && (meta.mode & 0o111) == 0 {
            return Err(VfsError::Acces);
        }
        return Ok(());
    }

    let triplet = if uid == meta.uid {
        (meta.mode >> 6) & 0o7
    } else if gid == meta.gid {
        (meta.mode >> 3) & 0o7
    } else {
        meta.mode & 0o7
    };

    if (mask.wants_read() && (triplet & 0o4) == 0)
        || (mask.wants_write() && (triplet & 0o2) == 0)
        || (mask.wants_exec() && (triplet & 0o1) == 0)
    {
        return Err(VfsError::Acces);
    }

    Ok(())
}
