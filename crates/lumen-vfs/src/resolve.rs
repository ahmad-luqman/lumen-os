//! Physical component-by-component path & symlink resolution, `cwd` path
//! reconstruction, and `access(2)` permission checks.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::cache::DentryTarget;
use crate::error::VfsError;
use crate::fs::VfsInode;
use crate::mount::VfsCacheOps;
use crate::types::{AccessMask, InodeType};
use crate::{MAX_SYMLINK_DEPTH, NAME_MAX, PATH_MAX};

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

/// Output of [`resolve_parent`].
#[derive(Clone)]
pub struct ParentTarget {
    pub parent: Arc<dyn VfsInode>,
    pub name: String,
    pub has_trailing_slash: bool,
}

impl fmt::Debug for ParentTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParentTarget")
            .field("parent_dev", &self.parent.dev_id())
            .field("parent_ino", &self.parent.ino())
            .field("name", &self.name)
            .field("has_trailing_slash", &self.has_trailing_slash)
            .finish()
    }
}

/// Cross downward through any stacked mounts at `cur`.
pub(crate) fn cross_mounts_down(env: &dyn VfsCacheOps, mut cur: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
    while let Some(fs) = env.mount_at(cur.dev_id(), cur.ino()) {
        let next = env.inode_get_or_insert(fs.root_inode());
        if next.dev_id() == cur.dev_id() && next.ino() == cur.ino() {
            break;
        }
        cur = next;
    }
    cur
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

/// Resolve one normal component using brief Lock-9 probes (`DentryCache` +
/// `InodeCache`) followed by Lock-4 `VfsInode::lookup` only on cache miss.
fn lookup_component(env: &dyn VfsCacheOps, dir: &Arc<dyn VfsInode>, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
    let pdev = dir.dev_id();
    let pino = dir.ino();

    let (cached, inv_gen) = env.dentry_get_with_gen(pdev, pino, name);
    if let Some(target) = cached
        && let Some(inode) = env.inode_get(target.dev, target.ino)
    {
        return Ok(inode);
    }

    let child = dir.lookup(name)?;
    if env.dentry_gen() != inv_gen {
        return Ok(env.inode_get(child.dev_id(), child.ino()).unwrap_or(child));
    }
    let child = env.inode_get_or_insert(child);
    let _ = env.dentry_insert_if_gen(pdev, pino, name, DentryTarget { dev: child.dev_id(), ino: child.ino() }, inv_gen);
    Ok(child)
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
            None => cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?)),
        };
        if flags.require_dir && target.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        return Ok(ResolvedPath { inode: target, has_trailing_slash: false });
    }

    let mut has_trailing_slash = path.ends_with(b"/");
    let mut cur = if path.starts_with(b"/") {
        cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?))
    } else {
        match start_dir {
            Some(d) => Arc::clone(d),
            None => cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?)),
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
                cur = env.inode_get_or_insert(cur.lookup("..")?);
            }
            cur = cross_mounts_down(env, cur);
            cursor = tail_start;
            continue;
        }

        let name = core::str::from_utf8(comp).map_err(|_| VfsError::NoEnt)?;
        let child = lookup_component(env, &cur, name)?;

        let should_follow =
            child.inode_type() == InodeType::Symlink && (!is_last || flags.follow_final || trailing_after_comp);

        if should_follow {
            *symlink_depth += 1;
            if *symlink_depth > MAX_SYMLINK_DEPTH {
                return Err(VfsError::Loop);
            }

            if let Some(magic_target) = child.resolve_magic_link() {
                cur = cross_mounts_down(env, env.inode_get_or_insert(magic_target));
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
                cur = cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?));
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

/// Resolve the parent directory and final component name for namespace-mutating
/// operations (`open(O_CREAT)`, `mkdir`, `unlink`, `rmdir`, `link`, `symlink`, `rename`).
pub fn resolve_parent(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
) -> Result<ParentTarget, VfsError> {
    let mut symlink_depth = 0usize;
    resolve_parent_with_depth(env, start_dir, path, &mut symlink_depth)
}

pub(crate) fn resolve_parent_with_depth(
    env: &dyn VfsCacheOps,
    start_dir: Option<&Arc<dyn VfsInode>>,
    path: &[u8],
    symlink_depth: &mut usize,
) -> Result<ParentTarget, VfsError> {
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
        let root = cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?));
        return Ok(ParentTarget { parent: root, name: String::from("/"), has_trailing_slash: true });
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
                None => cross_mounts_down(env, env.inode_get_or_insert(env.root_inode()?)),
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
    let name = core::str::from_utf8(name_bytes).map_err(|_| VfsError::Inval)?;

    Ok(ParentTarget { parent, name: String::from(name), has_trailing_slash })
}

/// Reconstruct the canonical path of `cwd` by walking `".."` and crossing
/// mountpoints up to the global root `/`.
///
/// Returns `Err(NoEnt)` if `cwd` was unlinked (`st_nlink == 0` or detached).
pub fn compute_cwd_path(env: &dyn VfsCacheOps, cwd: &Arc<dyn VfsInode>) -> Result<String, VfsError> {
    if cwd.inode_type() != InodeType::Dir {
        return Err(VfsError::NotDir);
    }
    if cwd.is_detached() || cwd.stat()?.nlink == 0 {
        return Err(VfsError::NoEnt);
    }

    let mut components: Vec<String> = Vec::new();
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
        let parent = env.inode_get_or_insert(cur.lookup("..")?);
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
        return Ok(String::from("/"));
    }

    let mut out = String::new();
    for comp in components.iter().rev() {
        out.push('/');
        out.push_str(comp);
    }
    if out.len() >= PATH_MAX {
        return Err(VfsError::NameTooLong);
    }
    Ok(out)
}

fn find_child_name_in_dir(parent: &Arc<dyn VfsInode>, target_ino: u64) -> Result<String, VfsError> {
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
            if entry.name != "." && entry.name != ".." && entry.ino == target_ino {
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

    if mask.wants_write() && !inode.exempt_from_rofs() && env.fs_read_only(inode.dev_id()) {
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
