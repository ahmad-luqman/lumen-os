//! Virtual filesystem integration (M7+).
//!
//! Provides unified [`page_cache::PageCache`], writable [`ext4::Ext4FileSystem`]
//! over [`lumen_bcache::BlockCache`], [`tmpfs::Tmpfs`], [`devfs::Devfs`],
//! [`procfs::Procfs`], Lock-9 [`KernelVfsTables`] (`MountTable -> DentryCache ->
//! InodeCache`), boot-time [`init()`] mounting, shell commands (`mount`, `sync`,
//! `vfstest`), and the `#248` `ext4_crate_smoke` check.

pub mod devfs;
pub mod ext4;
pub mod page_cache;
pub mod procfs;
pub mod tmpfs;

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
use log::{debug, info};
use lumen_bcache::BlockDevice;
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_vfs::{
    DentryCache, DentryTarget, DevId, FileSystem, Ino, InodeCache, InodeType, MountInfo, MountTable, OpenFlags,
    OpenGuard, RenameFlags, ResolveFlags, VfsCacheOps, VfsError, VfsInode, VfsLock, mkdir_at, open_inode, readlink_at,
    rename_at2, resolve_path, symlink_at, unlink_at,
};

use crate::shell::{Ctx, sysinfo, write_in_chunks};
use crate::sync::{SleepMutex, SleepMutexGuard};

/// Kernel `VfsLock` backed by [`SleepMutex`] (Lock 2).
#[allow(dead_code)] // public OpenFile lock adapter for M7 syscalls (#254)
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

/// Lock-9 VFS lookup tables enforcing strict sub-order
/// `MountTable -> DentryCache -> InodeCache`.
///
/// Never holds any Lock-9 mutex across `FileSystem::root_inode()`, Locks 1–8,
/// or `Arc` drops after `umount`.
pub struct KernelVfsTables {
    mounts: SleepMutex<MountTable>,
    dentries: SleepMutex<DentryCache>,
    inodes: SleepMutex<InodeCache>,
}

impl KernelVfsTables {
    pub const fn new() -> Self {
        Self {
            mounts: SleepMutex::new(MountTable::new()),
            dentries: SleepMutex::new(DentryCache::new(DentryCache::DEFAULT_CAPACITY)),
            inodes: SleepMutex::new(InodeCache::new()),
        }
    }

    pub fn mount_root(&self, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let root = fs.root_inode();
        let root_ino = root.ino();
        self.mounts.lock().mount_root_with_ino(fs, root_ino)?;
        let _ = self.inodes.lock().get_or_insert(root);
        Ok(())
    }

    pub fn mount(&self, mountpoint: Arc<dyn VfsInode>, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let mp_dev = mountpoint.dev_id();
        let mp_ino = mountpoint.ino();
        let root = fs.root_inode();
        let root_ino = root.ino();
        let mut mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        mounts.mount_with_ino(mountpoint, path, fs, root_ino)?;
        dentries.invalidate_dir(mp_dev, mp_ino);
        let _ = inodes.get_or_insert(root);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn umount(&self, mountpoint_dev: DevId, mountpoint_ino: Ino) -> Result<Arc<dyn FileSystem>, VfsError> {
        let (fs, evicted) = {
            let mut mounts = self.mounts.lock();
            let mut dentries = self.dentries.lock();
            let mut inodes = self.inodes.lock();
            let fs = mounts.umount(mountpoint_dev, mountpoint_ino)?;
            let dev = fs.dev_id();
            dentries.invalidate_fs(dev);
            dentries.invalidate_dir(mountpoint_dev, mountpoint_ino);
            let evicted = inodes.evict_fs(dev);
            (fs, evicted)
        };
        drop(evicted);
        Ok(fs)
    }

    pub fn snapshot_filesystems(&self) -> Vec<Arc<dyn FileSystem>> {
        self.mounts.lock().snapshot_filesystems()
    }

    pub fn snapshot_inodes(&self) -> Vec<Arc<dyn VfsInode>> {
        self.inodes.lock().snapshot_all()
    }

    pub fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts.lock().list_mounts()
    }
}

impl VfsCacheOps for KernelVfsTables {
    fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
        let root_fs = self.mounts.lock().root_fs().ok_or(VfsError::NoEnt)?;
        Ok(root_fs.root_inode())
    }

    fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.lock().is_global_root(dev, ino)
    }

    fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
        self.mounts.lock().mount_at(dev, ino)
    }

    fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
        self.mounts.lock().mount_at_with_id(dev, ino)
    }

    fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
        self.mounts.lock().parent_mount(mounted_dev)
    }

    fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.mounts.lock().parent_mount_if_root(dev, ino)
    }

    fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.lock().is_mountpoint(dev, ino)
    }

    fn fs_read_only(&self, dev: DevId) -> bool {
        self.mounts.lock().fs_by_dev(dev).is_some_and(|fs| fs.read_only())
    }

    fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
        self.mounts.lock().fs_by_dev(dev)
    }

    fn cache_gens(&self) -> (u64, u64) {
        let dentries = self.dentries.lock();
        let inodes = self.inodes.lock();
        (dentries.invalidation_gen(), inodes.removal_gen())
    }

    fn dentry_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
    ) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
        let dentries = self.dentries.lock();
        let inodes = self.inodes.lock();
        let (target_opt, dentry_gen) = dentries.get_with_gen(parent_dev, parent_ino, name);
        let inode_gen = inodes.removal_gen();
        let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
        (inode, dentry_gen, inode_gen)
    }

    fn publish_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
        if !mounts.is_mountpoint(parent_dev, parent_ino) {
            let _ = dentries.insert_if_gen(
                parent_dev,
                parent_ino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }

    fn publish_created_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
        let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
        if !was_cached && !mounts.is_mountpoint(parent_dev, parent_ino) {
            let _ = dentries.insert_if_gen(
                parent_dev,
                parent_ino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }

    fn dentry_gen(&self) -> u64 {
        self.dentries.lock().invalidation_gen()
    }

    fn dentry_get(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> Option<DentryTarget> {
        self.dentries.lock().get(parent_dev, parent_ino, name)
    }

    fn dentry_get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
        self.dentries.lock().get_with_gen(parent_dev, parent_ino, name)
    }

    fn dentry_insert(&self, parent_dev: DevId, parent_ino: Ino, name: &str, target: DentryTarget) {
        self.dentries.lock().insert(parent_dev, parent_ino, name, target);
    }

    fn dentry_insert_if_gen(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        if mounts.is_mountpoint(parent_dev, parent_ino) {
            return false;
        }
        dentries.insert_if_gen(parent_dev, parent_ino, name, target, expected_gen)
    }

    fn dentry_invalidate(&self, parent_dev: DevId, parent_ino: Ino, name: &str) {
        self.dentries.lock().invalidate(parent_dev, parent_ino, name);
    }

    fn dentry_invalidate_dir(&self, parent_dev: DevId, parent_ino: Ino) {
        self.dentries.lock().invalidate_dir(parent_dev, parent_ino);
    }

    fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().get(dev, ino)
    }

    fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().get_if_fresh(dev, ino, since_gen)
    }

    fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        self.inodes.lock().get_or_insert(inode)
    }

    fn inode_get_or_insert_if_mounted(
        &self,
        mp_dev: DevId,
        mp_ino: Ino,
        mount_id: u64,
        inode: Arc<dyn VfsInode>,
    ) -> Option<Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.has_mount_id(mp_dev, mp_ino, mount_id) {
            return None;
        }
        Some(inodes.get_or_insert(inode))
    }

    fn inode_get_or_insert_if_fresh(
        &self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        inodes.get_or_insert_if_fresh(inode, since_gen)
    }

    fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().remove(dev, ino)
    }

    fn inode_remove_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
        self.inodes.lock().remove_if_fresh(dev, ino, since_gen)
    }

    fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
        self.inodes.lock().begin_removal_if_fresh(dev, ino, since_gen)
    }

    fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.lock().end_removal(dev, ino)
    }

    fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
        self.inodes.lock().evict_detached(dev, ino)
    }

    fn inode_remove_if_same(&self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().remove_if_same(dev, ino, expected)
    }

    fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.lock().record_removal(dev, ino)
    }

    fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
        self.inodes.lock().record_removal_if_absent(dev, ino)
    }
}

static TABLES: KernelVfsTables = KernelVfsTables::new();
static EXT4_FILESYSTEMS: SleepMutex<Vec<Arc<ext4::Ext4FileSystem>>> = SleepMutex::new(Vec::new());

/// Global kernel VFS tables (`VfsCacheOps`).
#[allow(dead_code)] // public kernel VFS table surface for M7 syscalls (#254)
pub fn tables() -> &'static KernelVfsTables {
    &TABLES
}

/// Snapshot all active mounts (`MountTable::list_mounts`).
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

pub(crate) fn snapshot_cached_inodes() -> Vec<Arc<dyn VfsInode>> {
    TABLES.snapshot_inodes()
}

fn find_ext4_inode(inode: &Arc<dyn VfsInode>) -> Option<Arc<ext4::Ext4Inode>> {
    let dev = inode.dev_id();
    let lock_id = inode.lock_id();
    let fs = {
        let guard = EXT4_FILESYSTEMS.lock();
        guard.iter().find(|f| f.dev_id() == dev).cloned()
    }?;
    fs.lookup_live_by_lock_id(lock_id)
}

pub(crate) fn reclaim_ext4_inode_clean(inode: &Arc<dyn VfsInode>, max_evict: usize) {
    if let Some(ext4_inode) = find_ext4_inode(inode) {
        let _ = ext4_inode.reclaim_clean_pages(max_evict);
    }
}

pub(crate) fn reclaim_ext4_inode_dirty(inode: &Arc<dyn VfsInode>, max_evict: usize) {
    if let Some(ext4_inode) = find_ext4_inode(inode) {
        let _ = ext4_inode.reclaim_dirty_pages(max_evict);
    }
}

/// Flush all dirty `PageCache` pages, metadata, mounted filesystems, and
/// `BlockCache`s to durable storage (`Lock 9 snapshot -> Lock 4 -> 5 -> 6 -> 7 -> 8`).
pub fn sync_all() -> Result<(), VfsError> {
    let inodes = TABLES.snapshot_inodes();
    for inode in inodes {
        if !inode.is_detached() {
            inode.sync(false)?;
        }
    }
    let filesystems = TABLES.snapshot_filesystems();
    for fs in filesystems {
        fs.sync()?;
    }
    Ok(())
}

fn ensure_mountpoint(
    root: &Arc<dyn VfsInode>,
    name: &str,
    mode: u16,
    root_ro: bool,
) -> Result<Arc<dyn VfsInode>, VfsError> {
    match root.lookup(name) {
        Ok(dir) => Ok(dir),
        Err(VfsError::NoEnt) if !root_ro => root.mkdir(name, mode),
        Err(e) => Err(e),
    }
}

/// Initialize the kernel VFS and mount `/`, `/tmp`, `/dev`, and `/proc`.
///
/// Called from `kmain_on_kernel_stack` right after `drivers::probe_all()` and
/// `rng::init()`. Scans bound `virtio-blk` devices in PCI address order and
/// mounts the first valid `ext4` volume at `/`; if none is found (including
/// blank scratch disks in `test_blk`), silently falls back to `tmpfs` at `/`.
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
        match ext4::Ext4FileSystem::mount(cache, DevId::ROOT, hw_ro) {
            Ok(fs) => {
                EXT4_FILESYSTEMS.lock().push(Arc::clone(&fs));
                mounted_ext4 = Some((entry.id, fs));
                break;
            }
            Err(err) => {
                debug!(target: "vfs", "blk{}: not ext4 ({err:?})", entry.id);
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
        debug!(target: "vfs", "mount_root failed: {e:?}");
        return;
    }

    let root_inode = root_fs.root_inode();
    let root_ro = root_fs.read_only();

    let tmp_fs = tmpfs::Tmpfs::new(DevId::TMPFS, page_cache::TMPFS_DEFAULT_QUOTA_PAGES * page_cache::PAGE_SIZE);
    let dev_fs = devfs::Devfs::new(DevId::DEVFS);
    let proc_fs = procfs::Procfs::new(DevId::PROCFS);

    for (name, path, mode, fs) in [
        ("tmp", "/tmp", 0o1777u16, tmp_fs as Arc<dyn FileSystem>),
        ("dev", "/dev", 0o755u16, dev_fs as Arc<dyn FileSystem>),
        ("proc", "/proc", 0o555u16, proc_fs as Arc<dyn FileSystem>),
    ] {
        match ensure_mountpoint(&root_inode, name, mode, root_ro) {
            Ok(mp) => {
                if let Err(e) = TABLES.mount(mp, path, fs) {
                    debug!(target: "vfs", "mount {path} failed: {e:?}");
                }
            }
            Err(e) => {
                debug!(target: "vfs", "mountpoint {path} unavailable: {e:?}");
            }
        }
    }

    info!(
        target: "vfs",
        "mounted / ({}, {}) + /tmp /dev /proc",
        root_fs.name(),
        if root_ro { "ro" } else { "rw" }
    );
}

/// Build `mount`'s records table for shell rendering and pipelines.
pub fn mount_table() -> pipe::Table {
    const COLUMNS: [Column; 4] = [
        Column::new("target", Kind::Text, Align::Left, 6),
        Column::new("fs", Kind::Text, Align::Left, 6),
        Column::new("dev", Kind::Text, Align::Left, 5),
        Column::new("mode", Kind::Text, Align::Left, 4),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for m in list_mounts() {
        t.push(vec![
            Value::Text(m.mount_path),
            sysinfo::text(m.fs_name),
            Value::Text(format!("{}", m.dev_id)),
            sysinfo::text(if m.read_only { "ro" } else { "rw" }),
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
        Err(e) => ctx.fail(format_args!("sync: {e:?}")),
    }
}

#[cfg_attr(feature = "ktest", allow(dead_code))]
fn parse_octal(s: &str) -> Option<u16> {
    u16::from_str_radix(s, 8).ok()
}

#[cfg_attr(feature = "ktest", allow(dead_code))]
fn vfs_pattern_byte(idx: usize) -> u8 {
    ((idx.wrapping_mul(131)) ^ (idx >> 8).wrapping_mul(17) ^ 0x5a) as u8
}

#[cfg_attr(feature = "ktest", allow(dead_code))]
fn read_all_inode(inode: &Arc<dyn VfsInode>) -> Result<Vec<u8>, VfsError> {
    let mut out = Vec::new();
    let mut off = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        let n = inode.read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    Ok(out)
}

#[cfg_attr(feature = "ktest", allow(dead_code))]
fn run_dev_tmp_proc_checks() -> Result<String, String> {
    use crate::linux::process::{self, ConsoleIo};
    use crate::mm::user_vm::AddressSpace;
    use crate::sched;
    use lumen_vm::Prot;

    // 1. /tmp write, read, open-unlinked, and 32 MiB quota (-ENOSPC).
    let (tmp_f, _) = open_inode(
        &TABLES,
        None,
        b"/tmp/hello_tmp.txt",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0o022,
    )
    .map_err(|e| format!("open /tmp/hello_tmp.txt: {e:?}"))?;
    let _guard_tmp = OpenGuard::new(Arc::clone(&tmp_f));
    tmp_f.write_at(0, b"tmpfs-payload").map_err(|e| format!("write /tmp: {e:?}"))?;
    unlink_at(&TABLES, None, b"/tmp/hello_tmp.txt", false).map_err(|e| format!("unlink /tmp: {e:?}"))?;
    if !tmp_f.is_detached() || tmp_f.stat().map_err(|e| format!("stat detached: {e:?}"))?.nlink != 0 {
        return Err(String::from("/tmp open-unlinked did not detach"));
    }
    let got_tmp = read_all_inode(&tmp_f).map_err(|e| format!("read detached /tmp: {e:?}"))?;
    if got_tmp != b"tmpfs-payload" {
        return Err(String::from("/tmp open-unlinked payload mismatch"));
    }
    drop(_guard_tmp);
    drop(tmp_f);

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
    }

    // 2. /dev namespace ROFS vs char dev exemptions + 6 nodes.
    if !matches!(
        open_inode(
            &TABLES,
            None,
            b"/dev/forbidden",
            OpenFlags::from_bits_truncate(OpenFlags::O_CREAT | OpenFlags::O_WRONLY),
            0o644,
            0
        ),
        Err(VfsError::RoFs)
    ) {
        return Err(String::from("/dev create did not return -EROFS"));
    }
    let dev_null = resolve_path_from_root("/dev/null").map_err(|e| format!("/dev/null: {e:?}"))?;
    if !dev_null.exempt_from_rofs() || dev_null.stat().map_err(|e| format!("{e:?}"))?.rdev != DevId::makedev(1, 3) {
        return Err(String::from("/dev/null metadata mismatch"));
    }
    if dev_null.write_at(0, b"discard").map_err(|e| format!("{e:?}"))? != 7 {
        return Err(String::from("/dev/null write failed"));
    }
    let mut nb = [0xAAu8; 8];
    if dev_null.read_at(0, &mut nb).map_err(|e| format!("{e:?}"))? != 0 {
        return Err(String::from("/dev/null read non-empty"));
    }

    let dev_zero = resolve_path_from_root("/dev/zero").map_err(|e| format!("/dev/zero: {e:?}"))?;
    if dev_zero.stat().map_err(|e| format!("{e:?}"))?.rdev != DevId::makedev(1, 5) {
        return Err(String::from("/dev/zero rdev mismatch"));
    }
    let mut zb = [0xFFu8; 16];
    if dev_zero.read_at(0, &mut zb).map_err(|e| format!("{e:?}"))? != 16 || zb != [0u8; 16] {
        return Err(String::from("/dev/zero read mismatch"));
    }

    for (path, maj, min, mode) in [
        ("/dev/random", 1u32, 8u32, 0o666u16),
        ("/dev/urandom", 1, 9, 0o666),
        ("/dev/tty", 5, 0, 0o666),
        ("/dev/console", 5, 1, 0o620),
    ] {
        let n = resolve_path_from_root(path).map_err(|e| format!("{path}: {e:?}"))?;
        let st = n.stat().map_err(|e| format!("{path} stat: {e:?}"))?;
        if st.rdev != DevId::makedev(maj, min) || st.mode != mode || !n.exempt_from_rofs() {
            return Err(format!("{path} metadata mismatch: {st:?}"));
        }
    }
    let dev_rand = resolve_path_from_root("/dev/random").map_err(|e| format!("{e:?}"))?;
    let dev_urand = resolve_path_from_root("/dev/urandom").map_err(|e| format!("{e:?}"))?;
    let mut rbuf = [0u8; 32];
    dev_rand.read_at(0, &mut rbuf).map_err(|e| format!("/dev/random read: {e:?}"))?;
    dev_urand.read_at(0, &mut rbuf).map_err(|e| format!("/dev/urandom read: {e:?}"))?;
    if rbuf == [0u8; 32] {
        return Err(String::from("/dev/urandom returned all zeroes"));
    }

    // 3. /proc static virtual views (st_size == 0).
    for path in ["/proc/cpuinfo", "/proc/meminfo", "/proc/mounts", "/proc/version", "/proc/self/mounts"] {
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

    // 4. Spawn a user thread with real AddressSpace + ProcessState to test
    // /proc/self, /proc/self/exe, /proc/self/fd/* magic symlinks (incl. deleted),
    // /proc/self/maps format, and /dev/tty + /dev/console ConsoleIo::Capture.
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

    let (fd_file, _) = open_inode(
        &TABLES,
        None,
        b"/tmp/proc_fd_test.txt",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|e| format!("open /tmp/proc_fd_test.txt: {e:?}"))?;
    let fd_guard = OpenGuard::new(Arc::clone(&fd_file));
    fd_file.write_at(0, b"magic-fd-bytes").map_err(|e| format!("write fd_file: {e:?}"))?;

    let result: Arc<SleepMutex<Option<Result<String, String>>>> = Arc::new(SleepMutex::new(None));
    let res_clone = Arc::clone(&result);

    let handle = sched::spawn_user_on(0, "vfstest-proc", space_for_thread, move || {
        let out = (|| -> Result<String, String> {
            let pid = process::register_current(ConsoleIo::Capture, false, Vec::new());
            process::set_current_exe_path("/tmp");
            process::register_current_vfs_fd(3, Arc::clone(&fd_file), String::from("/tmp/proc_fd_test.txt"));

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

            // /proc/self/maps
            let maps_inode = resolve_path_from_root("/proc/self/maps").map_err(|e| format!("maps: {e:?}"))?;
            let maps_bytes = read_all_inode(&maps_inode).map_err(|e| format!("read maps: {e:?}"))?;
            let maps_str = core::str::from_utf8(&maps_bytes).map_err(|_| String::from("maps utf8"))?;
            if !maps_str.contains("rw-p 00000000 00:00 0\n") {
                return Err(format!("unexpected maps format:\n{maps_str}"));
            }

            // /dev/tty & /dev/console ConsoleIo::Capture
            let tty = resolve_path_from_root("/dev/tty").map_err(|e| format!("tty: {e:?}"))?;
            let con = resolve_path_from_root("/dev/console").map_err(|e| format!("con: {e:?}"))?;
            tty.write_at(0, b"tty-ok ").map_err(|e| format!("tty write: {e:?}"))?;
            con.write_at(0, b"con-ok").map_err(|e| format!("con write: {e:?}"))?;

            process::unregister_current_vfs_fd(3);
            let proc_state = process::take_current();
            if proc_state.stdout != b"tty-ok con-ok" {
                return Err(format!("console capture mismatch: {:?}", proc_state.stdout));
            }
            drop(proc_state);
            if let Some(detached) = sched::detach_user_space()
                && Arc::strong_count(&detached) == 1
            {
                detached.teardown();
            }
            Ok(format!("pid={pid} maps_ok tty_ok quota_ok"))
        })();
        *res_clone.lock() = Some(out);
    })
    .map_err(|e| format!("spawn_user_on: {e:?}"))?;

    handle.join();
    drop(fd_guard);
    drop(space);
    let inner_res = result.lock().take().ok_or_else(|| String::from("missing thread result"))??;
    Ok(inner_res)
}

/// `vfstest` shell command handler: exercises real VFS paths, `PageCache`,
/// `ext4`, `tmpfs`, `devfs`, and `procfs` operations.
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn vfstest_command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [sub, path] if sub == "stat" => match resolve_path(
            &TABLES,
            None,
            path.as_bytes(),
            ResolveFlags { follow_final: false, empty_path: false, require_dir: false },
        ) {
            Ok(res) => match res.inode.stat() {
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
                        "dev={} ino={} type={kind} mode={:04o} nlink={} size={} rdev={}",
                        st.dev, st.ino, st.mode, st.nlink, st.size, st.rdev
                    );
                }
                Err(e) => ctx.fail(format_args!("stat: {e:?}")),
            },
            Err(e) => ctx.fail(format_args!("stat: {e:?}")),
        },
        [sub, path] if sub == "cat" => match open_inode(
            &TABLES,
            None,
            path.as_bytes(),
            OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY),
            0,
            0,
        ) {
            Ok((inode, _)) => match read_all_inode(&inode) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    write_in_chunks(ctx.out, &text);
                    if !text.ends_with('\n') {
                        let _ = ctx.out.write_str("\n");
                    }
                }
                Err(e) => ctx.fail(format_args!("cat: {e:?}")),
            },
            Err(e) => ctx.fail(format_args!("cat: {e:?}")),
        },
        [sub, path, text @ ..] if sub == "write" && !text.is_empty() => {
            let payload = text.join(" ");
            match open_inode(
                &TABLES,
                None,
                path.as_bytes(),
                OpenFlags::from_bits_truncate(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0o022,
            ) {
                Ok((inode, _)) => match inode.write_at(0, payload.as_bytes()) {
                    Ok(n) => {
                        let _ = writeln!(ctx.out, "wrote {n}");
                    }
                    Err(e) => ctx.fail(format_args!("write: {e:?}")),
                },
                Err(e) => ctx.fail(format_args!("write: {e:?}")),
            }
        }
        [sub, path, len_str] if sub == "write-pattern" => {
            let Ok(len) = len_str.parse::<usize>() else {
                return ctx.fail("usage: vfstest write-pattern <path> <bytes>");
            };
            match open_inode(
                &TABLES,
                None,
                path.as_bytes(),
                OpenFlags::from_bits_truncate(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0o022,
            ) {
                Ok((inode, _)) => {
                    let mut buf = vec![0u8; len];
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = vfs_pattern_byte(i);
                    }
                    match inode.write_at(0, &buf) {
                        Ok(n) => {
                            let _ = writeln!(ctx.out, "wrote {n}");
                        }
                        Err(e) => ctx.fail(format_args!("write-pattern: {e:?}")),
                    }
                }
                Err(e) => ctx.fail(format_args!("write-pattern: {e:?}")),
            }
        }
        [sub, path, len_str] if sub == "verify-pattern" => {
            let Ok(len) = len_str.parse::<usize>() else {
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
        [sub, path, rest @ ..] if sub == "mkdir" && rest.len() <= 1 => {
            let mode = match rest.first() {
                Some(m) => match parse_octal(m) {
                    Some(v) => v,
                    None => return ctx.fail("usage: vfstest mkdir <path> [octal_mode]"),
                },
                None => 0o755,
            };
            match mkdir_at(&TABLES, None, path.as_bytes(), mode, 0) {
                Ok(_) => {
                    let _ = ctx.out.write_str("mkdir: ok\n");
                }
                Err(e) => ctx.fail(format_args!("mkdir: {e:?}")),
            }
        }
        [sub, linkpath, target] if sub == "symlink" => {
            match symlink_at(&TABLES, target.as_bytes(), None, linkpath.as_bytes()) {
                Ok(_) => {
                    let _ = ctx.out.write_str("symlink: ok\n");
                }
                Err(e) => ctx.fail(format_args!("symlink: {e:?}")),
            }
        }
        [sub, path] if sub == "readlink" => match readlink_at(&TABLES, None, path.as_bytes()) {
            Ok(bytes) => {
                let _ = writeln!(ctx.out, "{}", String::from_utf8_lossy(&bytes));
            }
            Err(e) => ctx.fail(format_args!("readlink: {e:?}")),
        },
        [sub, path, mode_str] if sub == "chmod" => {
            let Some(mode) = parse_octal(mode_str) else {
                return ctx.fail("usage: vfstest chmod <path> <octal_mode>");
            };
            match resolve_path_from_root(path).and_then(|i| i.set_mode(mode)) {
                Ok(()) => {
                    let _ = ctx.out.write_str("chmod: ok\n");
                }
                Err(e) => ctx.fail(format_args!("chmod: {e:?}")),
            }
        }
        [sub, path, size_str] if sub == "truncate" => {
            let Ok(sz) = size_str.parse::<u64>() else {
                return ctx.fail("usage: vfstest truncate <path> <size>");
            };
            match resolve_path_from_root(path).and_then(|i| i.truncate(sz)) {
                Ok(()) => {
                    let _ = ctx.out.write_str("truncate: ok\n");
                }
                Err(e) => ctx.fail(format_args!("truncate: {e:?}")),
            }
        }
        [sub, path] if sub == "unlink" => match unlink_at(&TABLES, None, path.as_bytes(), false) {
            Ok(()) => {
                let _ = ctx.out.write_str("unlink: ok\n");
            }
            Err(e) => ctx.fail(format_args!("unlink: {e:?}")),
        },
        [sub, path] if sub == "rmdir" => match unlink_at(&TABLES, None, path.as_bytes(), true) {
            Ok(()) => {
                let _ = ctx.out.write_str("rmdir: ok\n");
            }
            Err(e) => ctx.fail(format_args!("rmdir: {e:?}")),
        },
        [sub, old, new] if sub == "rename" => {
            match rename_at2(&TABLES, None, old.as_bytes(), None, new.as_bytes(), RenameFlags::default()) {
                Ok(()) => {
                    let _ = ctx.out.write_str("rename: ok\n");
                }
                Err(e) => ctx.fail(format_args!("rename: {e:?}")),
            }
        }
        [sub, path] if sub == "ls" => match resolve_path_from_root(path) {
            Ok(dir) => {
                let mut entries = Vec::new();
                if let Err(e) = dir.readdir(0, &mut entries, 256) {
                    return ctx.fail(format_args!("ls: {e:?}"));
                }
                for e in entries {
                    let _ = writeln!(ctx.out, "{}", e.name);
                }
            }
            Err(e) => ctx.fail(format_args!("ls: {e:?}")),
        },
        [sub] if sub == "dev-tmp-proc" => match run_dev_tmp_proc_checks() {
            Ok(summary) => {
                let _ = writeln!(ctx.out, "dev-tmp-proc: ok ({summary})");
            }
            Err(err) => ctx.fail(format_args!("dev-tmp-proc: {err}")),
        },
        _ => ctx.fail("usage: vfstest <stat|cat|write|write-pattern|verify-pattern|mkdir|symlink|readlink|chmod|truncate|unlink|rmdir|rename|ls|dev-tmp-proc> ..."),
    }
}

// ---------------------------------------------------------------------------
// #248 ext4plus smoke check (preserved for tests/features/fs.toml & make audit)
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

crate::ktest!(
    fn ext4_crate_smoke() {
        let stats = exercise_ext4_smoke()?;
        crate::kassert_eq!(stats.blocks_count, IMAGE_BLOCKS as u64);
        crate::kassert_eq!(stats.free_blocks_after, u64::from(FREE_BLOCKS));
        crate::kassert_eq!(stats.free_inodes_after, FREE_INODES - 1);
        Ok(())
    }
);

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

crate::ktest!(
    fn ext4_adapter_pagecache_quirks_and_open_unlinked() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;
        use lumen_vfs::OpenFile;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?));
        let fs =
            ext4::Ext4FileSystem::mount(Arc::clone(&bcache), DevId::makedev(0, 10), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. Write-back PageCache + OpenFile<KernelVfsLock<u64>> + truncate shrink tail zeroing + sync.
        let (f, path_str) = open_inode(
            &tables,
            None,
            b"/tail.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create tail.bin")?;
        let of = OpenFile::<KernelVfsLock<u64>>::new(
            Arc::clone(&f),
            Arc::from(path_str.as_str()),
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR),
        )
        .map_err(|_| "OpenFile::new")?;
        *of.offset.lock() = 123;
        kassert_eq!(*of.offset.lock(), 123);
        drop(of);

        let mut payload = vec![0xAAu8; 5000];
        payload[..10].copy_from_slice(b"0123456789");
        f.write_at(0, &payload).map_err(|_| "write tail.bin")?;
        f.sync(false).map_err(|_| "sync tail.bin")?;
        kassert!(mem_dev.0.flushed.load(core::sync::atomic::Ordering::Acquire));

        // Shrink to 100 (partial page 0) -> tail [100..4096) zeroed on disk too.
        f.truncate(100).map_err(|_| "truncate shrink")?;
        f.truncate(200).map_err(|_| "truncate extend")?;
        let mut check = [0xFFu8; 200];
        kassert_eq!(f.read_at(0, &mut check).map_err(|_| "read after extend")?, 200);
        kassert_eq!(&check[..10], b"0123456789");
        kassert!(check[100..200].iter().all(|&b| b == 0));

        // 2. Inline symlink <= 59B, 60B, and >= 60B block symlink + unlink inline symlink.
        let target60 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";
        kassert_eq!(target60.len(), 60);
        let _ = symlink_at(&tables, target60.as_bytes(), None, b"/sym60").map_err(|_| "sym60")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym60").map_err(|_| "readlink sym60")?, target60.as_bytes());
        let _ = symlink_at(&tables, b"/tail.bin", None, b"/sym_short").map_err(|_| "sym_short")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym_short").map_err(|_| "readlink sym_short")?, b"/tail.bin");
        unlink_at(&tables, None, b"/sym_short", false).map_err(|_| "unlink inline symlink")?;

        // 3. Gate 6 open-unlinked contract on ext4 file & directory.
        let g = OpenGuard::new(Arc::clone(&f));
        let orig_ino = f.ino();
        unlink_at(&tables, None, b"/tail.bin", false).map_err(|_| "open unlink tail.bin")?;
        kassert!(f.is_detached());
        kassert_ne_u64(f.ino(), orig_ino)?;
        kassert_eq!(f.stat().map_err(|_| "stat detached")?.nlink, 0);
        let mut after_unlink = [0u8; 10];
        kassert_eq!(f.read_at(0, &mut after_unlink).map_err(|_| "read detached")?, 10);
        kassert_eq!(&after_unlink, b"0123456789");
        f.sync(false).map_err(|_| "sync detached")?;
        drop(g);
        drop(f);

        // 4. Cross-dir mkdir + rename (0, NOREPLACE, EXCHANGE) updating `..`.
        let _ = mkdir_at(&tables, None, b"/d1", 0o755, 0).map_err(|_| "mkdir d1")?;
        let _ = mkdir_at(&tables, None, b"/d2", 0o755, 0).map_err(|_| "mkdir d2")?;
        let _ = mkdir_at(&tables, None, b"/d1/sub", 0o755, 0).map_err(|_| "mkdir d1/sub")?;
        let _ = open_inode(
            &tables,
            None,
            b"/d1/sub/file.txt",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT),
            0o644,
            0,
        )
        .map_err(|_| "create d1/sub/file.txt")?;
        rename_at2(&tables, None, b"/d1/sub", None, b"/d2/sub_moved", RenameFlags::default())
            .map_err(|_| "rename cross-dir")?;
        let parent_via_dotdot = resolve_path(
            &tables,
            None,
            b"/d2/sub_moved/..",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: true },
        )
        .map_err(|_| "resolve ..")?
        .inode;
        let d2_inode = resolve_path(
            &tables,
            None,
            b"/d2",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: true },
        )
        .map_err(|_| "resolve d2")?
        .inode;
        kassert_eq!(parent_via_dotdot.ino(), d2_inode.ino());

        // EXCHANGE between /d2/sub_moved and a file in /d1.
        let (ex_f, _) = open_inode(
            &tables,
            None,
            b"/d1/ex.txt",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT),
            0o644,
            0,
        )
        .map_err(|_| "create ex.txt")?;
        ex_f.write_at(0, b"ex").map_err(|_| "write ex")?;
        drop(ex_f);
        rename_at2(&tables, None, b"/d2/sub_moved", None, b"/d1/ex.txt", RenameFlags(RenameFlags::EXCHANGE))
            .map_err(|_| "rename EXCHANGE")?;
        let moved_back = resolve_path(
            &tables,
            None,
            b"/d1/ex.txt/file.txt",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: false },
        )
        .map_err(|_| "resolve swapped dir")?;
        kassert_eq!(moved_back.inode.inode_type(), InodeType::File);

        // Read-only mount check.
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 11), true).map_err(|_| "ro mount")?;
        kassert!(ro_fs.read_only());

        Ok(())
    }
);

#[cfg(feature = "ktest")]
fn kassert_ne_u64(a: u64, b: u64) -> Result<(), &'static str> {
    if a == b { Err("expected distinct u64 values") } else { Ok(()) }
}

crate::ktest!(
    fn vfs_boot_mounts_tmp_dev_proc_work() {
        use crate::{kassert, kassert_eq};
        // Boot init mounted `/`, `/tmp`, `/dev`, `/proc`.
        let mounts = list_mounts();
        kassert!(mounts.iter().any(|m| m.mount_path == "/"));
        kassert!(mounts.iter().any(|m| m.mount_path == "/tmp" && m.dev_id == DevId::TMPFS && !m.read_only));
        kassert!(mounts.iter().any(|m| m.mount_path == "/dev" && m.dev_id == DevId::DEVFS && m.read_only));
        kassert!(mounts.iter().any(|m| m.mount_path == "/proc" && m.dev_id == DevId::PROCFS && m.read_only));

        let summary = run_dev_tmp_proc_checks().map_err(|_| "dev_tmp_proc_checks failed")?;
        kassert!(summary.contains("maps_ok"));
        kassert_eq!(mounts.len() >= 4, true);
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_global_reclaim_writes_back_and_evicts() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(mem_dev, 64).map_err(|_| "bcache init")?));
        let dev = DevId::makedev(0, 99);
        let fs = ext4::Ext4FileSystem::mount(Arc::clone(&bcache), dev, false).map_err(|_| "ext4 mount")?;

        let mp = mkdir_at(&TABLES, None, b"/tmp/reclaim_mnt", 0o755, 0).map_err(|_| "mkdir mnt")?;
        let mp_dev = mp.dev_id();
        let mp_ino = mp.ino();
        EXT4_FILESYSTEMS.lock().push(Arc::clone(&fs));
        TABLES.mount(mp, "/tmp/reclaim_mnt", Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount")?;

        let (f, _) = open_inode(
            &TABLES,
            None,
            b"/tmp/reclaim_mnt/big.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create big.bin")?;

        // Write 6 pages (24 KiB) so 6 ext4 PageCache pages are dirty.
        let mut buf = vec![0u8; 6 * 4096];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((i * 73) ^ 0x19) as u8;
        }
        kassert_eq!(f.write_at(0, &buf).map_err(|_| "write big")?, buf.len());
        kassert!(page_cache::ext4_cached_pages() >= 6);

        // Reclaim down to 1 page: dirty pages must be written back before eviction.
        page_cache::reclaim_ext4_to_target(1);
        kassert!(page_cache::ext4_cached_pages() <= 1);

        // Read back after eviction (re-faults from disk) and verify contents match.
        let got = read_all_inode(&f).map_err(|_| "read after reclaim")?;
        kassert_eq!(got, buf);
        drop(f);

        let _ = TABLES.umount(mp_dev, mp_ino).map_err(|_| "umount")?;
        EXT4_FILESYSTEMS.lock().retain(|e| e.dev_id() != dev);
        unlink_at(&TABLES, None, b"/tmp/reclaim_mnt", true).map_err(|_| "rmdir mnt")?;
        Ok(())
    }
);
