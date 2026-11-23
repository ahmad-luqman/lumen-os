//! Virtual filesystem integration (M7+).
//!
//! Provides unified [`page_cache::PageCache`], writable [`ext4::Ext4FileSystem`]
//! over [`lumen_bcache::BlockCache`], [`tmpfs::Tmpfs`], [`devfs::Devfs`],
//! [`procfs::Procfs`], Lock-9 [`KernelVfsTables`] (lumen-vfs's `VfsTables`
//! over `SleepMutex`: `MountTable -> DentryCache -> InodeCache`), boot-time
//! [`init()`] mounting, shell commands (`mount`, `sync`, `vfstest`), and the
//! `#248` `ext4plus` smoke check (`exercise_ext4_smoke`;
//! its ktest, `ext4_crate_smoke`, is in `vfs/tests.rs` with the others of
//! this file's code).

pub mod devfs;
pub mod ext4;
pub mod page_cache;
pub mod procfs;
#[cfg(feature = "ktest")]
mod tests;
pub mod tmpfs;

#[allow(unused_imports)] // the file-backed VM's way in (#255); until then only ktests use it
pub(crate) use page_cache::{MappedPage, mmap_page};

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt::{self, Write as _};
use core::time::Duration;

use ext4plus::dir::Dir;
use ext4plus::error::Ext4Error;
use ext4plus::file::{truncate, write_at};
use ext4plus::inode::{Inode, InodeCreationOptions, InodeFlags, InodeMode};
use ext4plus::path::PathBuf;
use ext4plus::{DirEntryName, Ext4, Ext4Read, Ext4Write, FileType, IncompatibleFeatures};
use log::{debug, error, info, warn};
use lumen_bcache::BlockDevice;
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_vfs::{
    DentryCache, DevId, Escaped, FileSystem, InodeCache, InodeType, LockFamily, MountInfo, MountTable, OpenFile,
    OpenFlags, RenameFlags, ResolveFlags, VfsError, VfsInode, VfsLock, VfsTables, mkdir_at, open_inode,
    open_inode_guarded, readlink_at, rename_at2, resolve_path, symlink_at, unlink_at,
};

use crate::shell::{Ctx, sysinfo, write_in_chunks};
use crate::sync::{SleepMutex, SleepMutexGuard};

/// Kernel `VfsLock` backed by [`SleepMutex`]: Lock 2, and Lock 9 through
/// [`KernelLocks`].
pub struct KernelVfsLock<T>(SleepMutex<T>);

impl<T> VfsLock<T> for KernelVfsLock<T> {
    type Guard<'a>
        = SleepMutexGuard<'a, T>
    where
        T: 'a;

    fn new(val: T) -> Self {
        Self(SleepMutex::new(val))
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.0.lock()
    }

    fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

impl<T> KernelVfsLock<T> {
    /// [`VfsLock::new`] as a `const fn`, for [`TABLES`] only (a trait
    /// method can't be `const` on stable); everything else uses
    /// `VfsLock::new`.
    const fn new_const(val: T) -> Self {
        Self(SleepMutex::new(val))
    }
}

/// The kernel's [`LockFamily`]: [`KernelVfsLock`] (`SleepMutex`).
pub struct KernelLocks;

impl LockFamily for KernelLocks {
    type Lock<T> = KernelVfsLock<T>;
}

/// The kernel's Lock-9 VFS lookup tables: lumen-vfs's [`VfsTables`] (the
/// ones its host tests run) over `SleepMutex`.
pub type KernelVfsTables = VfsTables<KernelLocks>;

static TABLES: KernelVfsTables = KernelVfsTables::from_locks(
    KernelVfsLock::new_const(MountTable::new()),
    KernelVfsLock::new_const(DentryCache::with_default_capacity()),
    KernelVfsLock::new_const(InodeCache::new()),
);
static EXT4_FILESYSTEMS: SleepMutex<Vec<Arc<ext4::Ext4FileSystem>>> = SleepMutex::new(Vec::new());

/// Global kernel VFS tables (`VfsCacheOps`).
pub fn tables() -> &'static KernelVfsTables {
    &TABLES
}

/// Snapshot all active mounts (`VfsTables::list_mounts`).
pub fn list_mounts() -> Vec<MountInfo> {
    TABLES.list_mounts()
}

/// Resolve an absolute path from global `/` without holding Lock 9 across Locks 4–8.
pub fn resolve_path_from_root(path: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
    let resolved = resolve_path(
        &TABLES,
        None,
        path.as_bytes(),
        ResolveFlags { follow_final: true, empty_path: false, require_dir: false },
    )?;
    Ok(resolved.inode)
}

#[cfg(feature = "ktest")]
pub(crate) fn snapshot_cached_inodes() -> Vec<Arc<dyn VfsInode>> {
    TABLES.snapshot_inodes()
}

/// Every live inode of every `ext4` filesystem in `EXT4_FILESYSTEMS` (the
/// root volume, and the ktests' volumes), for reclaim: the
/// filesystems' list is copied first, then each one's live inodes under its
/// own Lock 6, held only while copying (`Ext4FileSystem::snapshot_live`).
pub(crate) fn snapshot_live_ext4_inodes() -> Vec<Arc<ext4::Ext4Inode>> {
    let filesystems = EXT4_FILESYSTEMS.lock().clone();
    filesystems.iter().flat_map(|fs| fs.snapshot_live()).collect()
}

/// Flush all dirty `PageCache` pages, metadata, mounted filesystems, and
/// `BlockCache`s to durable storage (`Lock 9 snapshot -> Lock 4 -> 5 -> 6 -> 7 -> 8`).
///
/// Each mounted filesystem's `sync` flushes its own live inodes and then its
/// block cache (ext4: `Ext4FileSystem::sync`; tmpfs, devfs and procfs have
/// nothing to write), so the inodes aren't visited a second time here. A
/// failure doesn't stop the rest: every other inode and filesystem is still
/// flushed, each failure is logged once (by the filesystem, naming the inode
/// or its block cache), and the first error is returned.
pub fn sync_all() -> Result<(), VfsError> {
    sync_tables(&TABLES)
}

fn sync_tables(tables: &KernelVfsTables) -> Result<(), VfsError> {
    let mut first: Option<VfsError> = None;
    for fs in tables.snapshot_filesystems() {
        if let Err(e) = fs.sync() {
            log::debug!(target: "vfs", "sync: {} filesystem {} failed: {e:?}", fs.name(), fs.dev_id());
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

fn ensure_mountpoint(
    root: &Arc<dyn VfsInode>,
    name: &[u8],
    mode: u16,
    root_ro: bool,
) -> Result<Arc<dyn VfsInode>, VfsError> {
    match root.lookup(name) {
        Ok(dir) => Ok(dir),
        Err(VfsError::NoEnt) if !root_ro => root.mkdir(name, mode),
        Err(e) => Err(e),
    }
}

/// Whether the bytes at offset 1024 of `cache` carry the ext4 superblock
/// magic (`Ok(false)` for a blank or foreign disk).
fn has_ext4_magic(cache: &dyn ext4::BlockCacheBackend) -> Result<bool, lumen_bcache::BlockError> {
    let mut magic = [0u8; 2];
    cache.read_at(1024 + 0x38, &mut magic)?;
    Ok(u16::from_le_bytes(magic) == 0xef53)
}

/// Initialize the kernel VFS and mount `/`, `/tmp`, `/dev`, and `/proc`.
///
/// Called from `kmain_on_kernel_stack` right after `drivers::probe_all()` and
/// `rng::init()`. Scans bound `virtio-blk` devices in PCI address order and
/// mounts the first valid `ext4` volume at `/`; if none is found, falls back
/// to `tmpfs` at `/`. A disk without the ext4 magic (a blank scratch disk in
/// `test_blk`) is skipped with only a `debug` line; a disk whose superblock
/// can't be read, or that has the magic but won't mount, is a `WARN`, so a
/// lost rootfs is never silent.
/// Missing `/tmp`, `/dev` and `/proc` directories are created on any writable
/// root (a writable ext4 disk included); the closing `INFO` line names only
/// the mounts that succeeded.
pub fn init() {
    let mut devs = crate::blk::devices();
    devs.sort_by_key(|e| e.dev.addr());

    let mut mounted_ext4: Option<(usize, Arc<ext4::Ext4FileSystem>)> = None;
    for entry in devs {
        if entry.dev.is_failed() {
            continue;
        }
        let hw_ro = entry.dev.info().read_only;
        let cache: Arc<dyn ext4::BlockCacheBackend> = entry.cache;
        match has_ext4_magic(&*cache) {
            Ok(true) => {}
            Ok(false) => {
                debug!(target: "vfs", "blk{}: no ext4 superblock", entry.id);
                continue;
            }
            Err(err) => {
                warn!(target: "vfs", "blk{}: can't read the superblock: {err:?}", entry.id);
                continue;
            }
        }
        match ext4::Ext4FileSystem::mount(cache, DevId::ROOT, hw_ro) {
            Ok(fs) => {
                EXT4_FILESYSTEMS.lock().push(Arc::clone(&fs));
                mounted_ext4 = Some((entry.id, fs));
                break;
            }
            Err(err) => {
                warn!(target: "vfs", "blk{}: has an ext4 superblock but won't mount: {err:?}", entry.id);
            }
        }
    }

    let root_fs: Arc<dyn FileSystem> = match mounted_ext4 {
        Some((id, fs)) => {
            debug!(target: "vfs", "mounted ext4 from blk{id} at / ({})", if fs.read_only() { "ro" } else { "rw" });
            fs
        }
        None => {
            debug!(target: "vfs", "no ext4 volume found; mounting tmpfs fallback at /");
            tmpfs::Tmpfs::new(DevId::ROOT, page_cache::TMPFS_DEFAULT_QUOTA_PAGES * page_cache::PAGE_SIZE)
        }
    };

    if let Err(e) = TABLES.mount_root(Arc::clone(&root_fs)) {
        error!(target: "vfs", "can't mount / ({}): {e:?}; no filesystem is mounted", root_fs.name());
        return;
    }

    let root_inode = root_fs.root_inode();
    let root_ro = root_fs.read_only();

    let tmp_fs = tmpfs::Tmpfs::new(DevId::TMPFS, page_cache::TMPFS_DEFAULT_QUOTA_PAGES * page_cache::PAGE_SIZE);
    // `/tmp` shows the mounted root, not the mountpoint under it: world
    // writable with the sticky bit, as on Linux.
    if let Err(e) = tmp_fs.root_inode().set_mode(0o1777) {
        warn!(target: "vfs", "can't make /tmp 1777: {e:?}");
    }
    let dev_fs = devfs::Devfs::new(DevId::DEVFS);
    let proc_fs = procfs::Procfs::new(DevId::PROCFS);

    let mut mounted = String::new();
    for (name, path, mode, fs) in [
        ("tmp", "/tmp", 0o1777u16, tmp_fs as Arc<dyn FileSystem>),
        ("dev", "/dev", 0o755u16, dev_fs as Arc<dyn FileSystem>),
        ("proc", "/proc", 0o555u16, proc_fs as Arc<dyn FileSystem>),
    ] {
        match ensure_mountpoint(&root_inode, name.as_bytes(), mode, root_ro) {
            Ok(mp) => match TABLES.mount(mp, path, fs) {
                Ok(()) => {
                    mounted.push(' ');
                    mounted.push_str(path);
                }
                Err(e) => warn!(target: "vfs", "can't mount {path}: {e:?}"),
            },
            Err(e) => warn!(target: "vfs", "no mountpoint for {path}: {e:?}"),
        }
    }

    let others = if mounted.is_empty() { String::new() } else { format!(" +{mounted}") };
    info!(target: "vfs", "mounted / ({}, {}){others}", root_fs.name(), if root_ro { "ro" } else { "rw" });
}

/// Build `mount`'s records table for shell rendering and pipelines: each
/// mount's target, filesystem, device and mode, and a tmpfs mount's quota
/// (`used` and `limit`, in bytes; `-` for any other filesystem), so an
/// `ENOSPC` on `/tmp` can be told apart from a leaked charge (#324).
pub fn mount_table() -> pipe::Table {
    const COLUMNS: [Column; 6] = [
        Column::new("target", Kind::Text, Align::Left, 6),
        Column::new("fs", Kind::Text, Align::Left, 6),
        Column::new("dev", Kind::Text, Align::Left, 5),
        Column::new("mode", Kind::Text, Align::Left, 4),
        Column::new("used", Kind::Int, Align::Right, 4),
        Column::new("limit", Kind::Int, Align::Right, 5),
    ];
    // The mounts' filesystems, matched to `list_mounts`' rows by device
    // (each mount's is its own); both are snapshots, taken under Lock 9
    // and used after it is released.
    let filesystems = TABLES.snapshot_filesystems();
    let mut t = pipe::Table::new(&COLUMNS);
    for m in list_mounts() {
        let quota = filesystems.iter().find(|fs| fs.dev_id() == m.dev_id).and_then(|fs| tmpfs::quota_use(fs.as_ref()));
        t.push(vec![
            Value::Text(m.mount_path),
            sysinfo::text(m.fs_name),
            Value::Text(format!("{}", m.dev_id)),
            sysinfo::text(if m.read_only { "ro" } else { "rw" }),
            quota.map_or(sysinfo::DASH, |q| sysinfo::int(q.used_bytes)),
            quota.map_or(sysinfo::DASH, |q| sysinfo::int(q.limit_bytes)),
        ]);
    }
    t
}

/// Pipeline records provider for `mount`.
pub fn mount_records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    args.is_empty().then(|| Ok(mount_table()))
}

/// `mount` shell command handler.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn mount_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: mount");
    }
    let t = mount_table();
    let mounts = list_mounts();
    let mut out = String::new();
    let s = if mounts.len() == 1 { "" } else { "s" };
    let _ = writeln!(out, "mount: {} mount{s}", mounts.len());
    t.render(&mut out);
    let _ = ctx.out.write_str(&out);
}

/// `sync` shell command handler.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn sync_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: sync");
    }
    match sync_all() {
        Ok(()) => {
            let _ = ctx.out.write_str("sync: ok\n");
        }
        Err(e) => ctx.fail(format_args!("sync: {e:?} (each failing inode or filesystem is in the log)")),
    }
}

fn parse_octal(s: &str) -> Option<u16> {
    u16::from_str_radix(s, 8).ok()
}

/// The longest `vfstest write-pattern` buffer: the whole pattern is built on
/// the heap first, and a length from the shell must not panic the kernel.
const WRITE_PATTERN_MAX: usize = 16 << 20;

fn vfs_pattern_byte(idx: usize) -> u8 {
    ((idx.wrapping_mul(131)) ^ (idx >> 8).wrapping_mul(17) ^ 0x5a) as u8
}

/// The most `read_all_inode` reads (the same bound as `vfstest
/// write-pattern`): `vfstest cat` and `verify-pattern` read a file of any size
/// from the shell, and growing one buffer until the heap refuses would panic
/// the kernel.
const READ_ALL_MAX: usize = WRITE_PATTERN_MAX;

/// A whole file's bytes, refused with `EFBIG` past [`READ_ALL_MAX`] (by its
/// size before reading, and again if it grows while read) and with `ENOMEM`
/// when the heap can't hold them: the buffer is reserved from the file's
/// size (nothing for one that reports 0, as procfs's do) and grown fallibly.
fn read_all_inode(inode: &Arc<dyn VfsInode>) -> Result<Vec<u8>, VfsError> {
    let size = inode.stat()?.size;
    if size > READ_ALL_MAX as u64 {
        return Err(VfsError::FBig);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(size as usize).map_err(|_| VfsError::NoMem)?;
    let mut off = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        let n = inode.read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() + n > READ_ALL_MAX {
            return Err(VfsError::FBig);
        }
        out.try_reserve(n).map_err(|_| VfsError::NoMem)?;
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    Ok(out)
}

/// An executable file's bytes by absolute path from `/` (symlinks
/// followed), for `linux /path` and the interpreter its binary names:
/// `EACCES` for anything but a regular file with an execute bit (as
/// `execve(2)`, where root too needs one), `EFBIG` past [`READ_ALL_MAX`],
/// `ENOMEM` when the heap can't hold it.
/// The inode comes back with the bytes: the loader names the image's
/// `/proc/<pid>/maps` lines after it (#255).
pub(crate) fn read_executable(path: &str) -> Result<(Vec<u8>, Arc<dyn VfsInode>), VfsError> {
    let inode = resolve_path_from_root(path)?;
    if inode.inode_type() != InodeType::File || inode.stat()?.mode & 0o111 == 0 {
        return Err(VfsError::Acces);
    }
    Ok((read_all_inode(&inode)?, inode))
}

/// Makes `run_dev_tmp_proc_checks`'s process fail its first check, for the
/// ktest that it still unregisters the process.
#[cfg(feature = "ktest")]
static FAIL_PROC_CHECKS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `fields` and `name` as a `/proc/<pid>/maps` line puts them (with its
/// `\n`): the name after 73 characters, where Linux pads it, the width
/// written out (72, then a space) rather than taken from `lumen_vm::maps`,
/// so a ktest comparing against it checks that width too.
#[cfg(feature = "ktest")]
pub(crate) fn ktest_maps_line(fields: &str, name: &str) -> String {
    format!("{:<72} {name}\n", format!("{fields} "))
}

/// Whether `line` has the shape of a `/proc/<pid>/maps` line:
/// `start-end perms offset major:minor inode`, then nothing or a name
/// (`[heap]`, `[stack]` or an absolute path) after 73 characters, where Linux pads
/// it (two spaces after the inode when the fields are wider). Addresses are
/// 8 to 16 lowercase hex digits, as is the offset; the device numbers
/// at least 2 each.
fn is_maps_line(line: &str) -> bool {
    fn hex(s: &str, min: usize, max: usize) -> bool {
        (min..=max).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }
    let (fields, label) = match line.split_once("  ") {
        Some((f, l)) => (f, Some(l.trim_start_matches(' '))),
        None => (line, None),
    };
    if let Some(label) = label {
        let named = matches!(label, "[heap]" | "[stack]") || label.starts_with('/');
        if !named || line.len() - label.len() != fields.len().max(lumen_vm::maps::NAME_PAD - 1) + 2 {
            return false;
        }
    }
    let parts: Vec<&str> = fields.split(' ').collect();
    let [range, perms, offset, devno, ino] = parts[..] else {
        return false;
    };
    let range_ok = range.split_once('-').is_some_and(|(a, b)| hex(a, 8, 16) && hex(b, 8, 16));
    let p = perms.as_bytes();
    let perms_ok = p.len() == 4
        && matches!(p[0], b'r' | b'-')
        && matches!(p[1], b'w' | b'-')
        && matches!(p[2], b'x' | b'-')
        && matches!(p[3], b'p' | b's');
    let dev_ok = devno.split_once(':').is_some_and(|(a, b)| hex(a, 2, 8) && hex(b, 2, 8));
    let ino_ok = !ino.is_empty() && ino.bytes().all(|b| b.is_ascii_digit());
    range_ok && perms_ok && hex(offset, 8, 16) && dev_ok && ino_ok
}

/// Check that `dir`'s listing agrees with lookup: each entry named in `names`
/// (all of them if `None`) has the inode number and type that looking its
/// name up and `stat`ing it gives, as `ls -i` and `find` expect. Every name in
/// `names` must be listed.
fn check_listing_agrees_with_lookup(dir: &Arc<dyn VfsInode>, what: &str, names: Option<&[&str]>) -> Result<(), String> {
    let mut entries = Vec::new();
    dir.readdir(0, &mut entries, 1024).map_err(|e| format!("readdir {what}: {e:?}"))?;
    if let Some(names) = names {
        for name in names {
            if !entries.iter().any(|e| e.name == name.as_bytes()) {
                return Err(format!("{what} doesn't list {name:?}"));
            }
        }
    }
    for entry in entries.iter().filter(|e| names.is_none_or(|n| n.iter().any(|n| n.as_bytes() == e.name))) {
        let st = dir
            .lookup(&entry.name)
            .and_then(|n| n.stat())
            .map_err(|e| format!("lookup and stat {what}/{}: {e:?}", Escaped(&entry.name)))?;
        if (st.ino, st.inode_type) != (entry.ino, entry.inode_type) {
            return Err(format!(
                "{what}/{} listed as ({}, {:?}) but stat says ({}, {:?})",
                Escaped(&entry.name),
                entry.ino,
                entry.inode_type,
                st.ino,
                st.inode_type
            ));
        }
    }
    Ok(())
}

/// `what: got`, unless `got` is `want`.
fn expect_eq<T: PartialEq + fmt::Debug>(what: &str, got: T, want: T) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("{what}: {got:?}, expected {want:?}")) }
}

/// `vfstest dev-tmp-proc`'s checks of `/tmp`, `/dev` and `/proc`, a
/// section at a time, the last from a user process's thread: its summary.
fn run_dev_tmp_proc_checks() -> Result<String, String> {
    check_tmp()?;
    check_dev()?;
    check_proc_static()?;
    check_proc_of_a_process().map(|summary| format!("{summary} quota_ok"))
}

/// `/tmp`: mode 1777, write, read, open-unlinked, and 32 MiB quota (-ENOSPC).
fn check_tmp() -> Result<(), String> {
    let tmp_mode = resolve_path_from_root("/tmp").and_then(|t| t.stat()).map_err(|e| format!("stat /tmp: {e:?}"))?.mode;
    if tmp_mode & 0o7777 != 0o1777 {
        return Err(format!("/tmp mode {tmp_mode:o}, expected 1777"));
    }
    let tmp_of = OpenFile::<KernelVfsLock<u64>>::from_opened(
        open_inode_guarded(
            &TABLES,
            None,
            b"/tmp/hello_tmp.txt",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0o022,
        )
        .map_err(|e| format!("open /tmp/hello_tmp.txt: {e:?}"))?,
    )
    .map_err(|e| format!("open file /tmp/hello_tmp.txt: {e:?}"))?;
    let tmp_f = &tmp_of.inode;
    tmp_f.write_at(0, b"tmpfs-payload").map_err(|e| format!("write /tmp: {e:?}"))?;
    unlink_at(&TABLES, None, b"/tmp/hello_tmp.txt", false).map_err(|e| format!("unlink /tmp: {e:?}"))?;
    if !tmp_f.is_detached() || tmp_f.stat().map_err(|e| format!("stat detached: {e:?}"))?.nlink != 0 {
        return Err(String::from("/tmp open-unlinked did not detach"));
    }
    let got_tmp = read_all_inode(tmp_f).map_err(|e| format!("read detached /tmp: {e:?}"))?;
    if got_tmp != b"tmpfs-payload" {
        return Err(String::from("/tmp open-unlinked payload mismatch"));
    }
    drop(tmp_of);

    // Test 32 MiB /tmp quota enforcement (-ENOSPC) and release on unlink.
    {
        let (q_f, _) = open_inode(
            &TABLES,
            None,
            b"/tmp/quota.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o600,
            0,
        )
        .map_err(|e| format!("open /tmp/quota.bin: {e:?}"))?;
        let chunk = [0x42u8; 4096];
        let mut hit_nospc = false;
        for i in 0..(page_cache::TMPFS_DEFAULT_QUOTA_PAGES + 16) {
            match q_f.write_at((i * 4096) as u64, &chunk) {
                Ok(_) => {}
                Err(VfsError::NoSpc) => {
                    hit_nospc = true;
                    break;
                }
                Err(e) => return Err(format!("unexpected quota err at page {i}: {e:?}")),
            }
        }
        if !hit_nospc {
            return Err(String::from("/tmp did not enforce 32 MiB -ENOSPC quota"));
        }
        drop(q_f);
        unlink_at(&TABLES, None, b"/tmp/quota.bin", false).map_err(|e| format!("unlink quota.bin: {e:?}"))?;

        // The unlink gave the quota back: a full page fits again.
        let (after, _) = open_inode(
            &TABLES,
            None,
            b"/tmp/after_quota.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o600,
            0,
        )
        .map_err(|e| format!("open /tmp/after_quota.bin: {e:?}"))?;
        let wrote = after.write_at(0, &chunk).map_err(|e| format!("write after quota unlink: {e:?}"))?;
        if wrote != chunk.len() {
            return Err(format!("write after quota unlink was short: {wrote}"));
        }
        drop(after);
        unlink_at(&TABLES, None, b"/tmp/after_quota.bin", false)
            .map_err(|e| format!("unlink after_quota.bin: {e:?}"))?;
    }
    Ok(())
}

/// `/dev`: a writable mount whose names never change, and its 6 nodes.
fn check_dev() -> Result<(), String> {
    check_dev_namespace()?;
    // Each node is still there after the refusals. The type and number
    // only: a node's mode and owner are boot values a `chmod` or `chown`
    // may have changed since (#293; devfs's
    // `devfs_nodes_start_with_their_boot_metadata` checks the boot modes
    // and owners).
    for (path, maj, min) in [
        ("/dev/null", 1u32, 3u32),
        ("/dev/zero", 1, 5),
        ("/dev/random", 1, 8),
        ("/dev/urandom", 1, 9),
        ("/dev/tty", 5, 0),
        ("/dev/console", 5, 1),
    ] {
        let n = resolve_path_from_root(path).map_err(|e| format!("{path}: {e:?}"))?;
        let st = n.stat().map_err(|e| format!("{path} stat: {e:?}"))?;
        if st.rdev != DevId::makedev(maj, min) || st.inode_type != InodeType::CharDevice {
            return Err(format!("{path} metadata mismatch: {st:?}"));
        }
    }
    let dev_null = resolve_path_from_root("/dev/null").map_err(|e| format!("/dev/null: {e:?}"))?;
    if dev_null.write_at(0, b"discard").map_err(|e| format!("/dev/null write: {e:?}"))? != 7 {
        return Err(String::from("/dev/null write failed"));
    }
    let mut nb = [0xAAu8; 8];
    if dev_null.read_at(0, &mut nb).map_err(|e| format!("/dev/null read: {e:?}"))? != 0 {
        return Err(String::from("/dev/null read non-empty"));
    }

    let dev_zero = resolve_path_from_root("/dev/zero").map_err(|e| format!("/dev/zero: {e:?}"))?;
    let mut zb = [0xFFu8; 16];
    if dev_zero.read_at(0, &mut zb).map_err(|e| format!("/dev/zero read: {e:?}"))? != 16 || zb != [0u8; 16] {
        return Err(String::from("/dev/zero read mismatch"));
    }

    let dev_rand = resolve_path_from_root("/dev/random").map_err(|e| format!("/dev/random: {e:?}"))?;
    let dev_urand = resolve_path_from_root("/dev/urandom").map_err(|e| format!("/dev/urandom: {e:?}"))?;
    let mut rbuf = [0u8; 32];
    let mut ubuf1 = [0u8; 32];
    let mut ubuf2 = [0u8; 32];
    dev_rand.read_at(0, &mut rbuf).map_err(|e| format!("/dev/random read: {e:?}"))?;
    dev_urand.read_at(0, &mut ubuf1).map_err(|e| format!("/dev/urandom read 1: {e:?}"))?;
    dev_urand.read_at(0, &mut ubuf2).map_err(|e| format!("/dev/urandom read 2: {e:?}"))?;
    if ubuf1 == [0u8; 32] || ubuf1 == ubuf2 {
        return Err(String::from("/dev/urandom output invalid when seeded"));
    }
    if dev_urand.read_at(0, &mut []).map_err(|e| format!("/dev/urandom empty read: {e:?}"))? != 0 {
        return Err(String::from("/dev/urandom empty read non-zero"));
    }
    Ok(())
}

/// `/dev`'s namespace. devfs's mount is writable, as Linux's devtmpfs
/// (#301), so lumen-vfs's own checks answer first, as on any writable mount
/// (a missing name to unlink or rename, a node that is no directory, a name
/// in use under `RENAME_NOREPLACE`, a link from another mount; a rename onto
/// itself does nothing), and a change that reaches devfs is `-EROFS` (a
/// divergence: devtmpfs takes these from root), `mkdir` and `symlink` onto a
/// name in use too (Linux: `-EEXIST`). Every check runs and every mismatch
/// is reported, until a change goes through (`Ok` where a refusal was
/// expected): the checks stop there.
fn check_dev_namespace() -> Result<(), String> {
    let creat = OpenFlags::from_bits_truncate(OpenFlags::O_CREAT | OpenFlags::O_WRONLY);
    let none = RenameFlags::default();
    let noreplace = RenameFlags(RenameFlags::NOREPLACE);
    let exchange = RenameFlags(RenameFlags::EXCHANGE);
    let rofs = Err(VfsError::RoFs);
    let noent = Err(VfsError::NoEnt);
    let dev = resolve_path_from_root("/dev").map_err(|e| format!("/dev: {e:?}"))?;
    create_truncated("/tmp/dev_link_src.txt").map_err(|e| format!("open /tmp/dev_link_src.txt: {e:?}"))?;
    let mut mismatches = Vec::new();
    // A change that went through (`Ok` where a refusal was expected) stops
    // the checks: the ones after it would run on a changed `/dev` and bury
    // it under their own mismatches.
    let mut changed = false;
    let mut skipped = 0usize;
    let mut check = |what: &str, op: &dyn Fn() -> Result<(), VfsError>, want: Result<(), VfsError>| {
        if changed {
            skipped += 1;
            return;
        }
        let got = op();
        changed = got.is_ok() && want.is_err();
        if let Err(mismatch) = expect_eq(&format!("/dev {what}"), got, want) {
            mismatches.push(mismatch);
        }
    };
    // Changes that reach devfs.
    check("create", &|| open_inode(&TABLES, None, b"/dev/forbidden", creat, 0o644, 0).map(drop), rofs);
    check("mkdir", &|| mkdir_at(&TABLES, None, b"/dev/forbidden", 0o755, 0).map(drop), rofs);
    check("mkdir over a node", &|| mkdir_at(&TABLES, None, b"/dev/null", 0o755, 0).map(drop), rofs);
    check("symlink", &|| symlink_at(&TABLES, b"null", None, b"/dev/forbidden").map(drop), rofs);
    check("symlink over a node", &|| symlink_at(&TABLES, b"zero", None, b"/dev/null").map(drop), rofs);
    check("link", &|| lumen_vfs::link_at(&TABLES, None, b"/dev/null", None, b"/dev/forbidden", false, false), rofs);
    check("unlink", &|| unlink_at(&TABLES, None, b"/dev/null", false), rofs);
    check("rename", &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/forbidden", none), rofs);
    check("rename over", &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/zero", none), rofs);
    check("rename exchange", &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/zero", exchange), rofs);
    check("rmdir (devfs's own)", &|| dev.rmdir(b"null"), rofs);
    // lumen-vfs's own answers, before devfs is asked.
    check("unlink of a missing name", &|| unlink_at(&TABLES, None, b"/dev/missing", false), noent);
    check(
        "rename of a missing name",
        &|| rename_at2(&TABLES, None, b"/dev/missing", None, b"/dev/forbidden", none),
        noent,
    );
    check(
        "rename exchange with a missing name",
        &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/missing", exchange),
        noent,
    );
    check("rmdir of a node", &|| unlink_at(&TABLES, None, b"/dev/null", true), Err(VfsError::NotDir));
    check(
        "RENAME_NOREPLACE over a node",
        &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/zero", noreplace),
        Err(VfsError::Exist),
    );
    check("rename onto itself", &|| rename_at2(&TABLES, None, b"/dev/null", None, b"/dev/null", none), Ok(()));
    check(
        "link from /tmp",
        &|| lumen_vfs::link_at(&TABLES, None, b"/tmp/dev_link_src.txt", None, b"/dev/forbidden", false, false),
        Err(VfsError::XDev),
    );
    check("forbidden after the refusals", &|| resolve_path_from_root("/dev/forbidden").map(drop), noent);
    if skipped > 0 {
        let s = if skipped == 1 { "" } else { "s" };
        mismatches.push(format!("/dev: a change went through, so the {skipped} check{s} after it didn't run"));
    }
    // A `/dev/forbidden` that a change left (reported above) is removed, or
    // the next run's `create` would open it and stop there, blaming the
    // wrong check: a removal that fails is reported too. A device there is
    // a boot node a rename moved: it is left, not unlinked for good.
    match resolve_path_from_root("/dev/forbidden").and_then(|n| n.stat()) {
        Ok(st) if st.inode_type == InodeType::CharDevice => {
            mismatches.push(format!("/dev/forbidden is a moved boot node (rdev {}), left there", st.rdev));
        }
        Ok(_) => {
            if let Err(e) = unlink_at(&TABLES, None, b"/dev/forbidden", false)
                .or_else(|_| unlink_at(&TABLES, None, b"/dev/forbidden", true))
            {
                mismatches.push(format!("couldn't remove the /dev/forbidden left behind: {e:?}"));
            }
        }
        Err(_) => {}
    }
    if let Err(e) = unlink_at(&TABLES, None, b"/tmp/dev_link_src.txt", false) {
        mismatches.push(format!("unlink /tmp/dev_link_src.txt: {e:?}"));
    }
    if mismatches.is_empty() { Ok(()) } else { Err(mismatches.join("; ")) }
}

/// `/proc`'s static virtual views (st_size == 0). This kernel thread is no
/// Linux process, so `/proc/self` dangles and no `/proc/<pid>` is invented.
fn check_proc_static() -> Result<(), String> {
    use crate::linux::process;

    expect_eq("/proc/self from a kernel thread", readlink_at(&TABLES, None, b"/proc/self"), Err(VfsError::NoEnt))?;
    if process::active_pids().is_empty() {
        let mut names = Vec::new();
        resolve_path_from_root("/proc")
            .and_then(|d| d.readdir(0, &mut names, 64))
            .map_err(|e| format!("readdir /proc: {e:?}"))?;
        if names.iter().any(|e| e.name.iter().all(u8::is_ascii_digit)) {
            return Err(String::from("/proc lists a pid while no process runs"));
        }
    }
    for path in ["/proc/cpuinfo", "/proc/meminfo", "/proc/mounts", "/proc/version"] {
        let inode = resolve_path_from_root(path).map_err(|e| format!("{path}: {e:?}"))?;
        let st = inode.stat().map_err(|e| format!("{path} stat: {e:?}"))?;
        if st.size != 0 {
            return Err(format!("{path} st_size != 0: {}", st.size));
        }
        let body = read_all_inode(&inode).map_err(|e| format!("read {path}: {e:?}"))?;
        if body.is_empty() {
            return Err(format!("{path} empty"));
        }
    }
    Ok(())
}

/// `/proc/self`, `/proc/self/exe`, the `/proc/self/fd/*` magic links (a
/// deleted file's too), `/proc/self/maps`, and what `/dev/tty` and
/// `/dev/console` write (`ConsoleIo::Capture`), from a user thread with a
/// real `AddressSpace` and `ProcessState`: the run's summary
/// (`pid=<pid> maps_ok tty_ok`).
fn check_proc_of_a_process() -> Result<String, String> {
    use crate::linux::process::{self, ConsoleIo};
    use crate::mm::user_vm::AddressSpace;
    use crate::sched;
    use lumen_vm::Prot;

    let space = Arc::new(AddressSpace::new().map_err(|e| format!("AddressSpace::new: {e}"))?);
    let _anon = space
        .mmap_anon(
            0,
            4096,
            Prot::READ | Prot::WRITE,
            u64::from(crate::linux::general::MAP_PRIVATE | crate::linux::general::MAP_ANONYMOUS),
        )
        .map_err(|e| format!("mmap_anon: {e}"))?;
    let space_for_thread = Arc::clone(&space);

    let fd_file = process::open_kernel_file(
        b"/tmp/proc_fd_test.txt",
        OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC,
        0o644,
    )
    .map_err(|e| format!("open /tmp/proc_fd_test.txt: {e:?}"))?;
    fd_file.inode.write_at(0, b"magic-fd-bytes").map_err(|e| format!("write fd_file: {e:?}"))?;
    let stdio = process::Stdio::open().map_err(|e| format!("open the process's stdio: {e:?}"))?;

    let result: Arc<SleepMutex<Option<Result<String, String>>>> = Arc::new(SleepMutex::new(None));
    let res_clone = Arc::clone(&result);

    let handle = sched::spawn_user_on(0, "vfstest-proc", space_for_thread, move || {
        let pid = process::register_current(ConsoleIo::Capture, false, Vec::new(), None, stdio);
        // `/proc/<pid>` and `/proc/<pid>/fd/3` as resolved while they exist,
        // for checking what they say once the process and fd are gone.
        let mut proc_nodes: Option<(Arc<dyn VfsInode>, Arc<dyn VfsInode>)> = None;
        let checks = (|| -> Result<String, String> {
            #[cfg(feature = "ktest")]
            if FAIL_PROC_CHECKS.load(core::sync::atomic::Ordering::Acquire) {
                return Err(String::from("injected failure"));
            }
            // No executable path recorded yet: `exe` has no target.
            expect_eq(
                "/proc/self/exe without a path",
                readlink_at(&TABLES, None, b"/proc/self/exe"),
                Err(VfsError::NoEnt),
            )?;
            if read_all_inode(&resolve_path_from_root("/proc/self/mounts").map_err(|e| format!("self/mounts: {e:?}"))?)
                .map_err(|e| format!("read /proc/self/mounts: {e:?}"))?
                .is_empty()
            {
                return Err(String::from("/proc/self/mounts empty"));
            }
            process::set_current_exe_path("/tmp");
            process::insert_current_fd(3, fd_file).map_err(|e| format!("install fd 3: {e:?}"))?;
            // procfs builds an fd's inode number from it: out-of-range fds
            // are refused.
            for bad in [-1, lumen_vfs::MAX_FDS as i32] {
                let null = process::open_kernel_file(b"/dev/null", OpenFlags::O_RDONLY, 0)
                    .map_err(|e| format!("open /dev/null: {e:?}"))?;
                if process::insert_current_fd(bad, null).is_ok() {
                    return Err(format!("fd {bad} was installed"));
                }
            }

            let self_target =
                readlink_at(&TABLES, None, b"/proc/self").map_err(|e| format!("readlink /proc/self: {e:?}"))?;
            if self_target != format!("{pid}").as_bytes() {
                return Err(format!("/proc/self mismatch: {self_target:?}"));
            }
            let exe_target =
                readlink_at(&TABLES, None, b"/proc/self/exe").map_err(|e| format!("readlink /proc/self/exe: {e:?}"))?;
            if exe_target != b"/tmp" {
                return Err(format!("/proc/self/exe mismatch: {exe_target:?}"));
            }
            let fd_target_live =
                readlink_at(&TABLES, None, b"/proc/self/fd/3").map_err(|e| format!("readlink fd/3: {e:?}"))?;
            if fd_target_live != b"/tmp/proc_fd_test.txt" {
                return Err(format!("fd/3 live mismatch: {fd_target_live:?}"));
            }
            // getdents' inode numbers and types agree with stat's: this
            // process's entry in `/proc`, everything in `/proc/<pid>`, and
            // `/proc/<pid>/fd`'s `.`, `..` and `3`.
            let pid_name = format!("{pid}");
            let proc_root = resolve_path_from_root("/proc").map_err(|e| format!("resolve /proc: {e:?}"))?;
            check_listing_agrees_with_lookup(&proc_root, "/proc", Some(&[pid_name.as_str()]))?;
            let pid_path = format!("/proc/{pid}");
            let pid_listed = resolve_path_from_root(&pid_path).map_err(|e| format!("resolve {pid_path}: {e:?}"))?;
            check_listing_agrees_with_lookup(&pid_listed, &pid_path, None)?;
            let fd_path = format!("/proc/{pid}/fd");
            let fd_dir = resolve_path_from_root(&fd_path).map_err(|e| format!("resolve {fd_path}: {e:?}"))?;
            check_listing_agrees_with_lookup(&fd_dir, &fd_path, Some(&[".", "..", "3"]))?;

            // Magic symlink traversal reads the live inode directly even after unlink!
            unlink_at(&TABLES, None, b"/tmp/proc_fd_test.txt", false).map_err(|e| format!("unlink fd_test: {e:?}"))?;
            let fd_target_del =
                readlink_at(&TABLES, None, b"/proc/self/fd/3").map_err(|e| format!("readlink deleted fd/3: {e:?}"))?;
            if fd_target_del != b"/tmp/proc_fd_test.txt (deleted)" {
                return Err(format!("fd/3 deleted mismatch: {fd_target_del:?}"));
            }
            let magic_inode = resolve_path_from_root("/proc/self/fd/3").map_err(|e| format!("resolve fd/3: {e:?}"))?;
            if read_all_inode(&magic_inode).map_err(|e| format!("read magic fd/3: {e:?}"))? != b"magic-fd-bytes" {
                return Err(String::from("magic fd/3 content mismatch"));
            }
            let nofollow = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
            let pid_dir = resolve_path(&TABLES, None, format!("/proc/{pid}").as_bytes(), nofollow)
                .map_err(|e| format!("resolve /proc/{pid}: {e:?}"))?
                .inode;
            let fd_link = resolve_path(&TABLES, None, b"/proc/self/fd/3", nofollow)
                .map_err(|e| format!("resolve fd/3 link: {e:?}"))?
                .inode;
            proc_nodes = Some((pid_dir, fd_link));

            // /proc/self/maps
            let maps_inode = resolve_path_from_root("/proc/self/maps").map_err(|e| format!("maps: {e:?}"))?;
            let maps_bytes = read_all_inode(&maps_inode).map_err(|e| format!("read maps: {e:?}"))?;
            let maps_str = core::str::from_utf8(&maps_bytes).map_err(|_| String::from("maps utf8"))?;
            if !maps_str.contains("rw-p 00000000 00:00 0\n") || !maps_str.lines().all(is_maps_line) {
                return Err(format!("unexpected maps format:\n{maps_str}"));
            }

            // /dev/tty & /dev/console ConsoleIo::Capture
            let tty = resolve_path_from_root("/dev/tty").map_err(|e| format!("tty: {e:?}"))?;
            let con = resolve_path_from_root("/dev/console").map_err(|e| format!("con: {e:?}"))?;
            tty.write_at(0, b"tty-ok ").map_err(|e| format!("tty write: {e:?}"))?;
            con.write_at(0, b"con-ok").map_err(|e| format!("con write: {e:?}"))?;

            Ok(format!("pid={pid} maps_ok tty_ok"))
        })();
        // Always unregister, a failed check included: a `ProcessState` left in
        // `ACTIVE_PROCS` would stay in `/proc` for good and pin its address space.
        // Not open if a check failed before fd 3 was: nothing to close then.
        let _ = process::close_current_fd(3);
        let fd_link_after_close = proc_nodes.as_ref().map(|(_, fd)| fd.stat().err());
        let proc_state = process::take_current();
        // These nodes were resolved while the process lived and are still
        // held (the VFS no longer caches them, #263), so procfs's `stat` must
        // say they are gone.
        let pid_dir_after_exit = proc_nodes.as_ref().map(|(dir, _)| dir.stat().err());
        drop(proc_nodes);
        let out = checks.and_then(|summary| {
            if proc_state.stdout != b"tty-ok con-ok" {
                Err(format!("console capture mismatch: {:?}", proc_state.stdout))
            } else if fd_link_after_close != Some(Some(VfsError::NoEnt))
                || pid_dir_after_exit != Some(Some(VfsError::NoEnt))
            {
                Err(format!(
                    "stale procfs nodes: fd/3 after close {fd_link_after_close:?}, /proc/{pid} after exit \
                     {pid_dir_after_exit:?}"
                ))
            } else {
                Ok(summary)
            }
        });
        drop(proc_state);
        if let Some(detached) = sched::detach_user_space()
            && Arc::strong_count(&detached) == 1
        {
            detached.teardown();
        }
        *res_clone.lock() = Some(out);
    })
    .map_err(|e| format!("spawn_user_on: {e:?}"))?;

    handle.join();
    drop(space);
    let inner_res = result.lock().take().ok_or_else(|| String::from("missing thread result"))??;
    Ok(inner_res)
}

/// `vfstest` shell command handler: exercises real VFS paths, `PageCache`,
/// `ext4`, `tmpfs`, `devfs`, and `procfs` operations, a function per
/// subcommand.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn vfstest_command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [sub, path] if sub == "stat" => vfstest_stat(ctx, path),
        [sub, path] if sub == "cat" => vfstest_cat(ctx, path),
        [sub, path, len] if sub == "read" => vfstest_read(ctx, path, len),
        [sub, path, text @ ..] if sub == "write" && !text.is_empty() => vfstest_write(ctx, path, text),
        [sub, path, len] if sub == "write-pattern" => vfstest_write_pattern(ctx, path, len),
        [sub, path, len] if sub == "verify-pattern" => vfstest_verify_pattern(ctx, path, len),
        [sub, path, rest @ ..] if sub == "mkdir" && rest.len() <= 1 => vfstest_mkdir(ctx, path, rest.first()),
        [sub, linkpath, target] if sub == "symlink" => vfstest_symlink(ctx, linkpath, target),
        [sub, path] if sub == "readlink" => vfstest_readlink(ctx, path),
        [sub, path, mode] if sub == "chmod" => vfstest_chmod(ctx, path, mode),
        [sub, path, size] if sub == "truncate" => vfstest_truncate(ctx, path, size),
        [sub, path] if sub == "unlink" => vfstest_unlink(ctx, path, false),
        [sub, path] if sub == "rmdir" => vfstest_unlink(ctx, path, true),
        [sub, old, new] if sub == "rename" => vfstest_rename(ctx, old, new),
        [sub, path] if sub == "ls" => vfstest_ls(ctx, path),
        [sub] if sub == "dev-tmp-proc" => vfstest_dev_tmp_proc(ctx),
        _ => ctx.fail("usage: vfstest <stat|cat|read|write|write-pattern|verify-pattern|mkdir|symlink|readlink|chmod|truncate|unlink|rmdir|rename|ls|dev-tmp-proc> ..."),
    }
}

/// `vfstest stat <path>`: the final symlink not followed.
fn vfstest_stat(ctx: &mut Ctx<'_>, path: &str) {
    let nofollow = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
    match resolve_path(&TABLES, None, path.as_bytes(), nofollow).and_then(|res| res.inode.stat()) {
        Ok(st) => {
            let kind = match st.inode_type {
                InodeType::File => "file",
                InodeType::Dir => "dir",
                InodeType::Symlink => "symlink",
                InodeType::CharDevice => "char",
                InodeType::BlockDevice => "block",
                InodeType::Fifo => "fifo",
                InodeType::Socket => "socket",
            };
            let _ = writeln!(
                ctx.out,
                "dev={} ino={} type={kind} mode={:04o} nlink={} size={} rdev={} mtime={}.{:09}",
                st.dev, st.ino, st.mode, st.nlink, st.size, st.rdev, st.mtime.sec, st.mtime.nsec
            );
        }
        Err(e) => ctx.fail(format_args!("stat: {e:?}")),
    }
}

/// `vfstest cat <path>`: the whole file as text.
fn vfstest_cat(ctx: &mut Ctx<'_>, path: &str) {
    let rdonly = OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY);
    match open_inode(&TABLES, None, path.as_bytes(), rdonly, 0, 0).and_then(|(inode, _)| read_all_inode(&inode)) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            write_in_chunks(ctx.out, &text);
            if !text.ends_with('\n') {
                let _ = ctx.out.write_str("\n");
            }
        }
        Err(e) => ctx.fail(format_args!("cat: {e:?}")),
    }
}

/// `vfstest read <path> <bytes>`: one read from offset 0, at most 64 KiB.
fn vfstest_read(ctx: &mut Ctx<'_>, path: &str, len: &str) {
    let Ok(len) = len.parse::<usize>() else {
        return ctx.fail("usage: vfstest read <path> <bytes>");
    };
    if len > 64 * 1024 {
        return ctx.fail("read: length exceeds 64 KiB");
    }
    let rdonly = OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY);
    let read = open_inode(&TABLES, None, path.as_bytes(), rdonly, 0, 0).and_then(|(inode, _)| {
        let mut buf = vec![0xA5u8; len];
        inode.read_at(0, &mut buf)
    });
    match read {
        Ok(n) => {
            let _ = writeln!(ctx.out, "read: {n}");
        }
        Err(e) => ctx.fail(format_args!("read: {e:?}")),
    }
}

/// `path` opened for writing, as `vfstest write` and `write-pattern` and
/// `check_dev_namespace`'s link source want it: created (0o644 under umask
/// 0o022) or truncated.
fn create_truncated(path: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
    let flags = OpenFlags::from_bits_truncate(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
    open_inode(&TABLES, None, path.as_bytes(), flags, 0o644, 0o022).map(|(inode, _)| inode)
}

/// `<name>: ok`, or `r`'s error as the command's failure.
fn vfstest_ok<T>(ctx: &mut Ctx<'_>, name: &str, r: Result<T, VfsError>) {
    match r {
        Ok(_) => {
            let _ = writeln!(ctx.out, "{name}: ok");
        }
        Err(e) => ctx.fail(format_args!("{name}: {e:?}")),
    }
}

/// `wrote <n>`, or `r`'s error as the command's failure.
fn vfstest_wrote(ctx: &mut Ctx<'_>, name: &str, r: Result<usize, VfsError>) {
    match r {
        Ok(n) => {
            let _ = writeln!(ctx.out, "wrote {n}");
        }
        Err(e) => ctx.fail(format_args!("{name}: {e:?}")),
    }
}

/// `vfstest write <path> <text>...`: the words joined by spaces, from
/// offset 0 of the file created or truncated.
fn vfstest_write(ctx: &mut Ctx<'_>, path: &str, text: &[String]) {
    let payload = text.join(" ");
    vfstest_wrote(ctx, "write", create_truncated(path).and_then(|inode| inode.write_at(0, payload.as_bytes())));
}

/// `vfstest write-pattern <path> <bytes>`: [`vfs_pattern_byte`]'s bytes,
/// at most [`WRITE_PATTERN_MAX`].
fn vfstest_write_pattern(ctx: &mut Ctx<'_>, path: &str, len: &str) {
    let Ok(len) = len.parse::<usize>() else {
        return ctx.fail("usage: vfstest write-pattern <path> <bytes>");
    };
    if len > WRITE_PATTERN_MAX {
        return ctx.fail("write-pattern: length exceeds 16 MiB");
    }
    let wrote = create_truncated(path).and_then(|inode| {
        let buf: Vec<u8> = (0..len).map(vfs_pattern_byte).collect();
        inode.write_at(0, &buf)
    });
    vfstest_wrote(ctx, "write-pattern", wrote);
}

/// `vfstest verify-pattern <path> <bytes>`: the file is `write-pattern`'s
/// bytes, exactly that long.
fn vfstest_verify_pattern(ctx: &mut Ctx<'_>, path: &str, len: &str) {
    let Ok(len) = len.parse::<usize>() else {
        return ctx.fail("usage: vfstest verify-pattern <path> <bytes>");
    };
    match resolve_path_from_root(path).and_then(|i| read_all_inode(&i)) {
        Ok(bytes) => {
            if bytes.len() != len {
                return ctx.fail(format_args!("verify-pattern: len {} != {len}", bytes.len()));
            }
            for (i, &b) in bytes.iter().enumerate() {
                let exp = vfs_pattern_byte(i);
                if b != exp {
                    return ctx.fail(format_args!("verify-pattern: mismatch at {i}: {b:#x} != {exp:#x}"));
                }
            }
            let _ = writeln!(ctx.out, "verify-pattern: ok ({len})");
        }
        Err(e) => ctx.fail(format_args!("verify-pattern: {e:?}")),
    }
}

/// `vfstest mkdir <path> [octal_mode]` (0o755 by default, no umask).
fn vfstest_mkdir(ctx: &mut Ctx<'_>, path: &str, mode: Option<&String>) {
    let mode = match mode {
        Some(m) => match parse_octal(m) {
            Some(v) => v,
            None => return ctx.fail("usage: vfstest mkdir <path> [octal_mode]"),
        },
        None => 0o755,
    };
    vfstest_ok(ctx, "mkdir", mkdir_at(&TABLES, None, path.as_bytes(), mode, 0));
}

/// `vfstest symlink <linkpath> <target>`.
fn vfstest_symlink(ctx: &mut Ctx<'_>, linkpath: &str, target: &str) {
    vfstest_ok(ctx, "symlink", symlink_at(&TABLES, target.as_bytes(), None, linkpath.as_bytes()));
}

/// `vfstest readlink <path>`: the target, escaped.
fn vfstest_readlink(ctx: &mut Ctx<'_>, path: &str) {
    match readlink_at(&TABLES, None, path.as_bytes()) {
        Ok(bytes) => {
            let _ = writeln!(ctx.out, "{}", Escaped(&bytes));
        }
        Err(e) => ctx.fail(format_args!("readlink: {e:?}")),
    }
}

/// `vfstest chmod <path> <octal_mode>`: the inode's setter, called directly
/// (no mount check).
fn vfstest_chmod(ctx: &mut Ctx<'_>, path: &str, mode: &str) {
    let Some(mode) = parse_octal(mode) else {
        return ctx.fail("usage: vfstest chmod <path> <octal_mode>");
    };
    vfstest_ok(ctx, "chmod", resolve_path_from_root(path).and_then(|i| i.set_mode(mode)));
}

/// `vfstest truncate <path> <size>`: the inode's `truncate`, called directly.
fn vfstest_truncate(ctx: &mut Ctx<'_>, path: &str, size: &str) {
    let Ok(sz) = size.parse::<u64>() else {
        return ctx.fail("usage: vfstest truncate <path> <size>");
    };
    vfstest_ok(ctx, "truncate", resolve_path_from_root(path).and_then(|i| i.truncate(sz)));
}

/// `vfstest unlink <path>`, or `vfstest rmdir <path>` with `remove_dir`.
fn vfstest_unlink(ctx: &mut Ctx<'_>, path: &str, remove_dir: bool) {
    let name = if remove_dir { "rmdir" } else { "unlink" };
    vfstest_ok(ctx, name, unlink_at(&TABLES, None, path.as_bytes(), remove_dir));
}

/// `vfstest rename <old> <new>`: no flags.
fn vfstest_rename(ctx: &mut Ctx<'_>, old: &str, new: &str) {
    vfstest_ok(ctx, "rename", rename_at2(&TABLES, None, old.as_bytes(), None, new.as_bytes(), RenameFlags::default()));
}

/// `vfstest ls <path>`: the first 256 names, escaped, `.` and `..` too.
fn vfstest_ls(ctx: &mut Ctx<'_>, path: &str) {
    let mut entries = Vec::new();
    match resolve_path_from_root(path).and_then(|dir| dir.readdir(0, &mut entries, 256)) {
        Ok(()) => {
            for e in entries {
                let _ = writeln!(ctx.out, "{}", Escaped(&e.name));
            }
        }
        Err(e) => ctx.fail(format_args!("ls: {e:?}")),
    }
}

/// `vfstest dev-tmp-proc`: [`run_dev_tmp_proc_checks`].
fn vfstest_dev_tmp_proc(ctx: &mut Ctx<'_>) {
    match run_dev_tmp_proc_checks() {
        Ok(summary) => {
            let _ = writeln!(ctx.out, "dev-tmp-proc: ok ({summary})");
        }
        Err(err) => ctx.fail(format_args!("dev-tmp-proc: {err}")),
    }
}

// ---------------------------------------------------------------------------
// #248 ext4plus smoke check (`exercise_ext4_smoke`; its ktest,
// `ext4_crate_smoke`, is in `vfs/tests.rs`)
// ---------------------------------------------------------------------------

const BLOCK_SIZE: usize = 4096;
const IMAGE_BLOCKS: usize = 64;
const INODE_SIZE: usize = 256;
const INODES_PER_GROUP: u32 = 32;
const USED_BLOCKS: u32 = 7;
const FREE_BLOCKS: u32 = IMAGE_BLOCKS as u32 - USED_BLOCKS;
const RESERVED_INODES: u32 = 10;
const FREE_INODES: u32 = INODES_PER_GROUP - RESERVED_INODES;

#[derive(Debug)]
struct MemIoErr;

impl fmt::Display for MemIoErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("in-memory ext4 I/O out of bounds")
    }
}

impl Error for MemIoErr {}

#[derive(Clone)]
struct MemExt4Device(Arc<SleepMutex<Vec<u8>>>);

impl MemExt4Device {
    fn new(data: Vec<u8>) -> Self {
        Self(Arc::new(SleepMutex::new(data)))
    }
}

impl Ext4Read for MemExt4Device {
    fn read(&self, start_byte: u64, dst: &mut [u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        let guard = self.0.lock();
        let start = usize::try_from(start_byte).map_err(|_| Box::new(MemIoErr))?;
        let end = start.checked_add(dst.len()).ok_or_else(|| Box::new(MemIoErr))?;
        let slice = guard.get(start..end).ok_or_else(|| Box::new(MemIoErr))?;
        dst.copy_from_slice(slice);
        Ok(())
    }
}

impl Ext4Write for MemExt4Device {
    fn write(&self, start_byte: u64, src: &[u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        let mut guard = self.0.lock();
        let start = usize::try_from(start_byte).map_err(|_| Box::new(MemIoErr))?;
        let end = start.checked_add(src.len()).ok_or_else(|| Box::new(MemIoErr))?;
        let slice = guard.get_mut(start..end).ok_or_else(|| Box::new(MemIoErr))?;
        slice.copy_from_slice(src);
        Ok(())
    }
}

fn write_u16le(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32le(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn build_minimal_ext4_image_blocks(total_blocks: usize) -> Vec<u8> {
    let mut img = vec![0u8; total_blocks * BLOCK_SIZE];
    let free_blocks = (total_blocks as u32) - USED_BLOCKS;

    let sb = &mut img[1024..2048];
    write_u32le(sb, 0x00, INODES_PER_GROUP);
    write_u32le(sb, 0x04, total_blocks as u32);
    write_u32le(sb, 0x0c, free_blocks);
    write_u32le(sb, 0x10, FREE_INODES);
    write_u32le(sb, 0x14, 0);
    write_u32le(sb, 0x18, 2);
    write_u32le(sb, 0x1c, 2);
    write_u32le(sb, 0x20, total_blocks as u32);
    write_u32le(sb, 0x24, total_blocks as u32);
    write_u32le(sb, 0x28, INODES_PER_GROUP);
    write_u16le(sb, 0x36, u16::MAX);
    write_u16le(sb, 0x38, 0xef53);
    write_u16le(sb, 0x3a, 1);
    write_u16le(sb, 0x3c, 1);
    write_u32le(sb, 0x4c, 1);
    write_u32le(sb, 0x54, RESERVED_INODES + 1);
    write_u16le(sb, 0x58, INODE_SIZE as u16);
    write_u32le(sb, 0x5c, 0x28);
    write_u32le(sb, 0x60, 0x2c2);
    write_u32le(sb, 0x64, 0x6b);
    sb[0x68..0x78].copy_from_slice(&[
        0x4c, 0x75, 0x6d, 0x65, 0x6e, 0x4f, 0x53, 0x2d, 0x4d, 0x37, 0x2d, 0x45, 0x78, 0x74, 0x34, 0x01,
    ]);
    sb[0x78..0x83].copy_from_slice(b"lumen-smoke");
    write_u16le(sb, 0xfe, 64);
    write_u16le(sb, 0x15c, 32);
    write_u16le(sb, 0x15e, 32);
    // s_flags: EXT2_FLAGS_SIGNED_HASH, as mke2fs writes where `char` is
    // signed. With `dir_index` (compat 0x20) and neither hash flag, the
    // adapter mounts read-only (#308).
    write_u32le(sb, 0x160, 0x1);

    let bgd = &mut img[BLOCK_SIZE..BLOCK_SIZE + 64];
    write_u32le(bgd, 0x00, 2);
    write_u32le(bgd, 0x04, 3);
    write_u32le(bgd, 0x08, 4);
    write_u16le(bgd, 0x0c, free_blocks as u16);
    write_u16le(bgd, 0x0e, FREE_INODES as u16);
    write_u16le(bgd, 0x10, 1);
    write_u16le(bgd, 0x12, 0x0004);
    write_u16le(bgd, 0x1c, 0);

    let bmap = &mut img[2 * BLOCK_SIZE..3 * BLOCK_SIZE];
    bmap[0] = 0x7f;
    bmap[total_blocks / 8..].fill(0xff);

    let imap = &mut img[3 * BLOCK_SIZE..4 * BLOCK_SIZE];
    imap[0] = 0xff;
    imap[1] = 0x03;
    imap[(INODES_PER_GROUP as usize) / 8..].fill(0xff);

    let root_off = 4 * BLOCK_SIZE + INODE_SIZE;
    let root_inode = &mut img[root_off..root_off + INODE_SIZE];
    write_u16le(root_inode, 0x00, 0o040755);
    write_u32le(root_inode, 0x04, BLOCK_SIZE as u32);
    write_u16le(root_inode, 0x1a, 2);
    write_u32le(root_inode, 0x1c, (BLOCK_SIZE / 512) as u32);
    write_u32le(root_inode, 0x20, 0x0008_0000);
    write_u16le(root_inode, 0x28, 0xf30a);
    write_u16le(root_inode, 0x2a, 1);
    write_u16le(root_inode, 0x2c, 4);
    write_u16le(root_inode, 0x2e, 0);
    write_u32le(root_inode, 0x34, 0);
    write_u16le(root_inode, 0x38, 1);
    write_u16le(root_inode, 0x3a, 0);
    write_u32le(root_inode, 0x3c, 6);
    write_u16le(root_inode, 0x80, 32);

    let root_dir = &mut img[6 * BLOCK_SIZE..7 * BLOCK_SIZE];
    write_u32le(root_dir, 0, 2);
    write_u16le(root_dir, 4, 12);
    root_dir[6] = 1;
    root_dir[7] = 2;
    root_dir[8] = b'.';

    write_u32le(root_dir, 12, 2);
    write_u16le(root_dir, 16, (BLOCK_SIZE - 12) as u16);
    root_dir[18] = 2;
    root_dir[19] = 2;
    root_dir[20..22].copy_from_slice(b"..");

    img
}

fn build_minimal_ext4_image() -> Vec<u8> {
    build_minimal_ext4_image_blocks(IMAGE_BLOCKS)
}

struct SmokeStats {
    blocks_count: u64,
    free_blocks_after: u64,
    free_inodes_after: u32,
}

fn exercise_ext4_smoke() -> Result<SmokeStats, &'static str> {
    let image = build_minimal_ext4_image();
    {
        let ro_dev = MemExt4Device::new(image.clone());
        let ro_fs = Ext4::load_with_writer(Box::new(ro_dev), None).map_err(|_| "ext4 load ro failed")?;
        let ro_err = ro_fs.create_inode(InodeCreationOptions {
            file_type: FileType::Regular,
            mode: InodeMode::S_IFREG | InodeMode::S_IRUSR | InodeMode::S_IWUSR,
            uid: 0,
            gid: 0,
            time: Duration::ZERO,
            flags: InodeFlags::empty(),
        });
        if !matches!(ro_err, Err(Ext4Error::Readonly)) {
            return Err("read-only mount did not return Ext4Error::Readonly");
        }
    }

    let dev = MemExt4Device::new(image);
    let fs = Ext4::load_with_writer(Box::new(dev.clone()), Some(Box::new(dev))).map_err(|_| "ext4 load rw failed")?;
    if fs.superblock().features().incompatible().contains(IncompatibleFeatures::RECOVERY) {
        return Err("unexpected RECOVERY bit on clean image");
    }

    let root = fs.read_root_inode().map_err(|_| "read_root_inode failed")?;
    if root.index.get() != 2 || !root.file_type().is_dir() || root.links_count() != 2 {
        return Err("unexpected root inode metadata");
    }
    let root_ino = root.index;
    let mut root_dir = Dir::open_inode(&fs, root).map_err(|_| "Dir::open_inode failed")?;

    let mut file_inode = fs
        .create_inode(InodeCreationOptions {
            file_type: FileType::Regular,
            mode: InodeMode::S_IFREG
                | InodeMode::S_IRUSR
                | InodeMode::S_IWUSR
                | InodeMode::S_IRGRP
                | InodeMode::S_IROTH,
            uid: 0,
            gid: 0,
            time: Duration::from_secs(1),
            flags: InodeFlags::empty(),
        })
        .map_err(|_| "create_inode regular failed")?;
    let file_ino = file_inode.index;
    let hello_name = DirEntryName::try_from(b"hello.txt".as_slice()).map_err(|_| "DirEntryName failed")?;
    root_dir.link(hello_name, &mut file_inode).map_err(|_| "Dir::link file failed")?;
    if file_inode.links_count() != 1 {
        return Err("file links_count != 1 after link");
    }

    let payload = b"hello ext4plus!!";
    let written = write_at(&fs, &mut file_inode, payload, 0).map_err(|_| "write_at failed")?;
    if written != payload.len() {
        return Err("short write_at");
    }
    let data = fs.read("/hello.txt").map_err(|_| "Ext4::read failed")?;
    if data != payload {
        return Err("read payload mismatch");
    }

    truncate(&fs, &mut file_inode, 5).map_err(|_| "truncate failed")?;
    let truncated = fs.read("/hello.txt").map_err(|_| "Ext4::read after truncate failed")?;
    if truncated != b"hello" {
        return Err("truncated payload mismatch");
    }

    let sym_name = DirEntryName::try_from(b"sym".as_slice()).map_err(|_| "sym DirEntryName failed")?;
    let _sym_inode = fs
        .symlink(&mut root_dir, sym_name, PathBuf::new("/hello.txt"), 0, 0, Duration::from_secs(1))
        .map_err(|_| "Ext4::symlink failed")?;
    let target = fs.read_link("/sym").map_err(|_| "Ext4::read_link failed")?;
    if target.as_path().to_str().ok() != Some("/hello.txt") {
        return Err("read_link target mismatch");
    }
    let via_sym = fs.read("/sym").map_err(|_| "read via symlink failed")?;
    if via_sym != b"hello" {
        return Err("read via symlink payload mismatch");
    }

    let mut file_inode = Inode::read(&fs, file_ino).map_err(|_| "Inode::read before hardlink failed")?;
    let hard_name = DirEntryName::try_from(b"hello_hard.txt".as_slice()).map_err(|_| "hard DirEntryName failed")?;
    root_dir.link(hard_name, &mut file_inode).map_err(|_| "Dir::link hardlink failed")?;
    if file_inode.links_count() != 2 {
        return Err("hardlink links_count != 2");
    }
    let remaining = root_dir
        .unlink(hello_name, file_inode)
        .map_err(|_| "unlink first hardlink failed")?
        .ok_or("file unexpectedly deleted at links_count == 2")?;
    if remaining.links_count() != 1 {
        return Err("remaining hardlink links_count != 1");
    }
    if fs.read("/hello_hard.txt").map_err(|_| "read remaining hardlink failed")? != b"hello" {
        return Err("remaining hardlink content mismatch");
    }

    let sub_raw = fs
        .create_inode(InodeCreationOptions {
            file_type: FileType::Directory,
            mode: InodeMode::S_IFDIR
                | InodeMode::S_IRUSR
                | InodeMode::S_IWUSR
                | InodeMode::S_IXUSR
                | InodeMode::S_IRGRP
                | InodeMode::S_IXGRP
                | InodeMode::S_IROTH
                | InodeMode::S_IXOTH,
            uid: 0,
            gid: 0,
            time: Duration::from_secs(1),
            flags: InodeFlags::empty(),
        })
        .map_err(|_| "create_inode dir failed")?;
    let sub_dir = Dir::init(fs.clone(), sub_raw, root_ino).map_err(|_| "Dir::init failed")?;
    let sub_ino = sub_dir.inode().index;
    drop(sub_dir);
    let mut sub_inode = Inode::read(&fs, sub_ino).map_err(|_| "Inode::read subdir failed")?;
    let sub_name = DirEntryName::try_from(b"sub".as_slice()).map_err(|_| "sub DirEntryName failed")?;
    root_dir.link(sub_name, &mut sub_inode).map_err(|_| "Dir::link subdir failed")?;
    if sub_inode.links_count() != 2 || root_dir.inode().links_count() != 3 {
        return Err("mkdir link counts mismatch");
    }

    sub_inode.set_links_count(1);
    sub_inode.write(&fs).map_err(|_| "subdir set_links_count failed")?;
    if root_dir.unlink(sub_name, sub_inode).map_err(|_| "rmdir unlink failed")?.is_some() {
        return Err("subdir not freed during rmdir");
    }
    let parent_links = root_dir.inode().links_count();
    root_dir.inode_mut().set_links_count(parent_links.saturating_sub(1));
    root_dir.inode_mut().write(&fs).map_err(|_| "parent write after rmdir failed")?;
    if root_dir.inode().links_count() != 2 || fs.exists("/sub").map_err(|_| "exists(/sub) failed")? {
        return Err("rmdir post-state mismatch");
    }

    let file_inode = Inode::read(&fs, file_ino).map_err(|_| "Inode::read before final unlink failed")?;
    if file_inode.links_count() != 1 {
        return Err("expected links_count == 1 before final unlink");
    }
    if root_dir.unlink(hard_name, file_inode).map_err(|_| "final unlink failed")?.is_some() {
        return Err("file not freed on final unlink");
    }
    if fs.exists("/hello_hard.txt").map_err(|_| "exists(/hello_hard.txt) failed")? {
        return Err("unlinked file still exists");
    }

    let sb = fs.superblock();
    let stats = SmokeStats {
        blocks_count: sb.blocks_count(),
        free_blocks_after: sb.free_blocks_count(),
        free_inodes_after: sb.free_inodes_count(),
    };
    if stats.blocks_count != IMAGE_BLOCKS as u64
        || stats.free_blocks_after != u64::from(FREE_BLOCKS)
        || stats.free_inodes_after != FREE_INODES - 1
    {
        return Err("free block/inode accounting leaked after unlink/rmdir");
    }
    Ok(stats)
}

/// Probe and log a debug summary of the pinned `ext4plus` crate at boot.
pub fn log_ext4_crate_summary() {
    match exercise_ext4_smoke() {
        Ok(stats) => {
            log::debug!(
                target: "vfs",
                "ext4plus =0.1.0-rc.2 (MIT): smoke ok (blocks={}, free_blocks={}, free_inodes={})",
                stats.blocks_count,
                stats.free_blocks_after,
                stats.free_inodes_after,
            );
        }
        Err(err) => {
            log::debug!(target: "vfs", "ext4plus =0.1.0-rc.2: smoke failed: {err}");
        }
    }
}

/// A fresh, writable in-memory ext4 filesystem (a 256-block image) for
/// ktests outside `vfs` (the FD layer's), on device `0:dev_minor`.
#[cfg(feature = "ktest")]
pub(crate) fn ktest_ext4(dev_minor: u32) -> Result<Arc<ext4::Ext4FileSystem>, &'static str> {
    let mem_dev = MemBlkDev::new(build_minimal_ext4_image_blocks(256), false);
    let bcache: Arc<dyn ext4::BlockCacheBackend> =
        Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(mem_dev, 64).map_err(|_| "bcache init")?));
    ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, dev_minor), false).map_err(|_| "ext4 mount")
}

/// A read-only in-memory ext4 filesystem (devices `0:minor` for the
/// writable mount it is made on and `0:minor+1`) whose root holds, after
/// `.` and `..`, the names `a0` to `a5` (hard links to one file) with `a3`
/// renamed on disk to the bytes `ff fe`, not UTF-8, which `readdir` lists
/// as any other name (#268), for the ktests (ext4's and the FD layer's).
#[cfg(feature = "ktest")]
pub(crate) fn ktest_ext4_non_utf8(minor: u32) -> Result<Arc<ext4::Ext4FileSystem>, &'static str> {
    use ext4::ktest_util::{ROOT_DIR_BLOCK_4K, image, mount, mount_ro_image};
    let m = mount(build_minimal_ext4_image_blocks(256), minor)?;
    let file = m.root.create(b"a0", 0o644).map_err(|_| "create a0")?;
    for name in ["a1", "a2", "a3", "a4", "a5"] {
        m.root.link(name.as_bytes(), &file).map_err(|_| "link")?;
    }
    let mut img = image(&m)?;
    let dir = &mut img[(ROOT_DIR_BLOCK_4K as usize) * BLOCK_SIZE..][..BLOCK_SIZE];
    let at = dir.windows(2).position(|w| w == b"a3").ok_or("a3's entry")?;
    dir[at] = 0xff;
    dir[at + 1] = 0xfe;
    mount_ro_image(img, minor + 1)
}

/// An in-memory block device for ktests. It stays here, not in
/// `vfs/tests.rs`, because the `vfs/ext4.rs` and `vfs/page_cache.rs` ktests use it too.
#[cfg(feature = "ktest")]
#[derive(Clone)]
struct MemBlkDev(Arc<MemBlkDevInner>);

#[cfg(feature = "ktest")]
struct MemBlkDevInner {
    data: SleepMutex<Vec<u8>>,
    read_only: bool,
    flushed: core::sync::atomic::AtomicBool,
}

#[cfg(feature = "ktest")]
impl MemBlkDev {
    fn new(data: Vec<u8>, read_only: bool) -> Self {
        Self(Arc::new(MemBlkDevInner {
            data: SleepMutex::new(data),
            read_only,
            flushed: core::sync::atomic::AtomicBool::new(false),
        }))
    }
}

#[cfg(feature = "ktest")]
impl lumen_bcache::BlockDevice for MemBlkDev {
    fn info(&self) -> lumen_bcache::BlockInfo {
        let len = self.0.data.lock().len() as u64;
        lumen_bcache::BlockInfo {
            sector_size: 512,
            sector_count: len / 512,
            read_only: self.0.read_only,
            flush_supported: true,
        }
    }

    fn read_sectors(&self, lba: u64, dst: &mut [u8]) -> Result<(), lumen_bcache::BlockError> {
        let off = (lba * 512) as usize;
        let guard = self.0.data.lock();
        let slice = guard.get(off..off + dst.len()).ok_or(lumen_bcache::BlockError::OutOfRange)?;
        dst.copy_from_slice(slice);
        Ok(())
    }

    fn write_sectors(&self, lba: u64, src: &[u8]) -> Result<(), lumen_bcache::BlockError> {
        if self.0.read_only {
            return Err(lumen_bcache::BlockError::ReadOnly);
        }
        let off = (lba * 512) as usize;
        let mut guard = self.0.data.lock();
        let slice = guard.get_mut(off..off + src.len()).ok_or(lumen_bcache::BlockError::OutOfRange)?;
        slice.copy_from_slice(src);
        Ok(())
    }

    fn flush(&self) -> Result<(), lumen_bcache::BlockError> {
        self.0.flushed.store(true, core::sync::atomic::Ordering::Release);
        Ok(())
    }
}
