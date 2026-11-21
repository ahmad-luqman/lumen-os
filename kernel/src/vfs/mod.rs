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
        [sub, path, len_str] if sub == "read" => {
            let Ok(len) = len_str.parse::<usize>() else {
                return ctx.fail("usage: vfstest read <path> <bytes>");
            };
            if len > 64 * 1024 {
                return ctx.fail("read: length exceeds 64 KiB");
            }
            match open_inode(
                &TABLES,
                None,
                path.as_bytes(),
                OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY),
                0,
                0,
            ) {
                Ok((inode, _)) => {
                    let mut buf = vec![0xA5u8; len];
                    match inode.read_at(0, &mut buf) {
                        Ok(n) => {
                            let _ = writeln!(ctx.out, "read: {n}");
                        }
                        Err(e) => ctx.fail(format_args!("read: {e:?}")),
                    }
                }
                Err(e) => ctx.fail(format_args!("read: {e:?}")),
            }
        }
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
        _ => ctx.fail("usage: vfstest <stat|cat|read|write|write-pattern|verify-pattern|mkdir|symlink|readlink|chmod|truncate|unlink|rmdir|rename|ls|dev-tmp-proc> ..."),
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

        // 2. Inline symlink <= 59B, 60B, and >= 60B block symlink + unlink inline symlink
        //    + hard-linked inline symlink unlink preserves surviving link.
        let target60 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";
        kassert_eq!(target60.len(), 60);
        let _ = symlink_at(&tables, target60.as_bytes(), None, b"/sym60").map_err(|_| "sym60")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym60").map_err(|_| "readlink sym60")?, target60.as_bytes());
        let _ = symlink_at(&tables, b"/tail.bin", None, b"/sym_short").map_err(|_| "sym_short")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym_short").map_err(|_| "readlink sym_short")?, b"/tail.bin");
        unlink_at(&tables, None, b"/sym_short", false).map_err(|_| "unlink inline symlink")?;

        let _ = symlink_at(&tables, b"/tail.bin", None, b"/sym_hl_1").map_err(|_| "sym_hl_1")?;
        lumen_vfs::link_at(&tables, None, b"/sym_hl_1", None, b"/sym_hl_2", false, false)
            .map_err(|_| "link inline symlink")?;
        unlink_at(&tables, None, b"/sym_hl_1", false).map_err(|_| "unlink first inline symlink link")?;
        kassert_eq!(
            readlink_at(&tables, None, b"/sym_hl_2").map_err(|_| "readlink surviving inline symlink")?,
            b"/tail.bin"
        );
        unlink_at(&tables, None, b"/sym_hl_2", false).map_err(|_| "unlink second inline symlink link")?;
        kassert!(matches!(readlink_at(&tables, None, b"/sym_hl_2"), Err(VfsError::NoEnt)));

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

        // Stale unlinked inode with open_count == 0 returns NoEnt (not freed disk reuse).
        let dead_dir = mkdir_at(&tables, None, b"/dead_dir", 0o755, 0).map_err(|_| "mkdir dead_dir")?;
        unlink_at(&tables, None, b"/dead_dir", true).map_err(|_| "rmdir dead_dir")?;
        kassert!(matches!(dead_dir.create("child", 0o644), Err(VfsError::NoEnt)));
        kassert!(matches!(dead_dir.lookup("child"), Err(VfsError::NoEnt)));
        let mut dead_entries = Vec::new();
        kassert!(matches!(dead_dir.readdir(0, &mut dead_entries, 16), Err(VfsError::NoEnt)));

        // Malformed 64-bit superblock with desc_size != 64 returns Err(Inval) without panic.
        let mut bad_img = build_minimal_ext4_image_blocks(64);
        bad_img[1024 + 0xfe..1024 + 0x100].copy_from_slice(&32u16.to_le_bytes());
        let bad_dev = MemBlkDev::new(bad_img, true);
        let bad_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(bad_dev, 16).map_err(|_| "bad bcache")?));
        kassert!(matches!(ext4::Ext4FileSystem::mount(bad_bcache, DevId::makedev(0, 12), true), Err(VfsError::Inval)));

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

        // Regression: write_at -> reclaim_ext4_to_target(0) (no sync) -> OpenGuard + unlink_at -> read_at.
        let (victim, _) = open_inode(
            &TABLES,
            None,
            b"/tmp/reclaim_mnt/victim.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create victim.bin")?;
        let victim_payload = vec![0x5Au8; 2 * 4096];
        kassert_eq!(victim.write_at(0, &victim_payload).map_err(|_| "write victim")?, victim_payload.len());
        page_cache::reclaim_ext4_to_target(0);
        kassert_eq!(page_cache::ext4_cached_pages(), 0);

        let guard = OpenGuard::new(Arc::clone(&victim));
        unlink_at(&TABLES, None, b"/tmp/reclaim_mnt/victim.bin", false).map_err(|_| "unlink victim")?;
        kassert!(victim.is_detached());
        let got_victim = read_all_inode(&victim).map_err(|_| "read detached victim")?;
        kassert_eq!(got_victim, victim_payload);
        drop(guard);
        drop(victim);

        let _ = TABLES.umount(mp_dev, mp_ino).map_err(|_| "umount")?;
        EXT4_FILESYSTEMS.lock().retain(|e| e.dev_id() != dev);
        unlink_at(&TABLES, None, b"/tmp/reclaim_mnt", true).map_err(|_| "rmdir mnt")?;
        Ok(())
    }
);

crate::ktest!(
    fn vfs_iteration1_regressions_tmpfs_maps_and_procfs() {
        use crate::{kassert, kassert_eq};

        // 1. tmpfs multi-page -ENOSPC short-write + truncate extend does not leak unwritten data.
        let tfs = tmpfs::Tmpfs::new(DevId::makedev(0, 120), page_cache::PAGE_SIZE);
        let root = tfs.root_inode();
        let f = root.create("short.bin", 0o644).map_err(|_| "tmpfs create short.bin")?;
        let two_pages = vec![0xA5u8; 2 * page_cache::PAGE_SIZE];
        kassert_eq!(f.write_at(0, &two_pages).map_err(|_| "short write")?, page_cache::PAGE_SIZE);
        kassert_eq!(f.stat().map_err(|_| "stat short")?.size, page_cache::PAGE_SIZE as u64);
        kassert!(matches!(f.write_at(page_cache::PAGE_SIZE as u64, &[0xBBu8; 64]), Err(VfsError::NoSpc)));

        // Free quota by truncating to 0, then extend past 2 pages: must read all zeroes.
        f.truncate(0).map_err(|_| "truncate 0")?;
        f.truncate((2 * page_cache::PAGE_SIZE) as u64).map_err(|_| "truncate extend")?;
        let extended = read_all_inode(&f).map_err(|_| "read extended")?;
        kassert_eq!(extended.len(), 2 * page_cache::PAGE_SIZE);
        kassert!(extended.iter().all(|&b| b == 0));

        // 2. tmpfs rmdir (open_count == 0) blocks subsequent create/mkdir/symlink with NoEnt.
        let d = root.mkdir("gone_dir", 0o755).map_err(|_| "mkdir gone_dir")?;
        kassert_eq!(d.open_count(), 0);
        root.rmdir("gone_dir").map_err(|_| "rmdir gone_dir")?;
        kassert!(matches!(d.create("f", 0o644), Err(VfsError::NoEnt)));
        kassert!(matches!(d.mkdir("sub", 0o755), Err(VfsError::NoEnt)));
        kassert!(matches!(d.symlink("sym", "/x"), Err(VfsError::NoEnt)));
        kassert!(matches!(d.lookup("f"), Err(VfsError::NoEnt)));

        // 3. AddressSpace::format_maps coalesced .bss + brk emits [heap] at brk_init.
        let elf_img = crate::mm::user_vm::build_relro_test_elf();
        let (space, prog) = crate::mm::user_vm::AddressSpace::from_elf(&elf_img, &[b"/t"], &[], b"/t", [0; 16])
            .map_err(|_| "from_elf")?;
        kassert_eq!(prog.initial_brk, 0x40_4000);
        kassert_eq!(space.brk(0x40_5000), 0x40_5000);
        let maps = space.format_maps();
        kassert!(maps.contains("00402000-00404000 rw-p 00000000 00:00 0\n"));
        kassert!(maps.contains("00404000-00405000 rw-p 00000000 00:00 0  [heap]\n"));
        space.teardown();

        // 4. procfs pid_ino bit-32 collision fix and canonical numeric parsing.
        kassert_ne_u64(procfs::pid_ino(1, 0), procfs::pid_ino(65537, 0))?;
        let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
        kassert!(matches!(resolve_path(&TABLES, None, b"/proc/+1", rf), Err(VfsError::NoEnt)));
        kassert!(matches!(resolve_path(&TABLES, None, b"/proc/01", rf), Err(VfsError::NoEnt)));
        Ok(())
    }
);

#[cfg(feature = "ktest")]
struct FaultInjectBlockCache {
    inner: SleepMutex<lumen_bcache::BlockCache<MemBlkDev>>,
    fail_reads: core::sync::atomic::AtomicBool,
    fail_writes: core::sync::atomic::AtomicBool,
    fail_write_on_call: core::sync::atomic::AtomicUsize,
    fail_write_on_call2: core::sync::atomic::AtomicUsize,
    fail_write_from_call: core::sync::atomic::AtomicUsize,
    write_calls: core::sync::atomic::AtomicUsize,
}

#[cfg(feature = "ktest")]
impl FaultInjectBlockCache {
    fn new(inner: lumen_bcache::BlockCache<MemBlkDev>) -> Self {
        Self {
            inner: SleepMutex::new(inner),
            fail_reads: core::sync::atomic::AtomicBool::new(false),
            fail_writes: core::sync::atomic::AtomicBool::new(false),
            fail_write_on_call: core::sync::atomic::AtomicUsize::new(0),
            fail_write_on_call2: core::sync::atomic::AtomicUsize::new(0),
            fail_write_from_call: core::sync::atomic::AtomicUsize::new(0),
            write_calls: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn arm_write_fail_on(&self, call_num: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(call_num, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(0, core::sync::atomic::Ordering::Release);
    }

    fn arm_write_fail_pair(&self, call_1: usize, call_2: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(call_1, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(call_2, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(0, core::sync::atomic::Ordering::Release);
    }

    fn arm_write_fail_from(&self, from_call: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(from_call, core::sync::atomic::Ordering::Release);
    }
}

#[cfg(feature = "ktest")]
impl ext4::BlockCacheBackend for FaultInjectBlockCache {
    fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_reads.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().read_at(byte_off, dst)
    }

    fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_writes.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        let call_idx = self.write_calls.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1;
        let target = self.fail_write_on_call.load(core::sync::atomic::Ordering::Acquire);
        let target2 = self.fail_write_on_call2.load(core::sync::atomic::Ordering::Acquire);
        let from_call = self.fail_write_from_call.load(core::sync::atomic::Ordering::Acquire);
        if (target != 0 && call_idx == target)
            || (target2 != 0 && call_idx == target2)
            || (from_call != 0 && call_idx >= from_call)
        {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().write_at(byte_off, src)
    }

    fn sync(&self) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_writes.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().sync()
    }
}

crate::ktest!(
    fn ext4_flush_and_reclaim_preserve_dirty_on_io_error() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering;
        use lumen_bcache::BlockCache;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache_raw = BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?;
        let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
        let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
        let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, 13), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. Create a file and write 2 dirty pages in PageCache without syncing yet.
        let (f, _) = open_inode(
            &tables,
            None,
            b"/dirty_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create dirty_err.bin")?;
        let mut payload = vec![0u8; 2 * 4096];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = ((i * 37) ^ 0xA5) as u8;
        }
        kassert_eq!(f.write_at(0, &payload).map_err(|_| "write dirty_err")?, payload.len());
        let ext4_inode = fs.lookup_live_by_lock_id(f.lock_id()).ok_or("lookup live inode")?;

        // 2. Fail Inode::read during flush_to_ext4 (sync) and reclaim_dirty_pages:
        //    both must preserve page dirty state so clean-page reclamation cannot drop
        //    modified pages and a retry sync writes them to disk.
        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(f.sync(false), Err(VfsError::Io)));
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 0);
        kassert_eq!(ext4_inode.reclaim_dirty_pages(16), 0);
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 0);

        // 3. Recover I/O: retry sync must succeed and persist both modified pages.
        fault_cache.fail_reads.store(false, Ordering::Release);
        f.sync(false).map_err(|_| "retry sync after read error")?;
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 2);
        kassert_eq!(read_all_inode(&f).map_err(|_| "re-fault dirty_err")?, payload);
        drop(ext4_inode);
        drop(f);

        // 4. Also verify write-side failure during writeback preserves dirty state.
        let (w_f, _) = open_inode(
            &tables,
            None,
            b"/write_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create write_err.bin")?;
        let old_data = vec![0x11u8; 4096];
        let new_data = vec![0x22u8; 4096];
        kassert_eq!(w_f.write_at(0, &old_data).map_err(|_| "write old_data")?, old_data.len());
        w_f.sync(false).map_err(|_| "initial sync write_err")?;
        kassert_eq!(w_f.write_at(0, &new_data).map_err(|_| "write new_data")?, new_data.len());

        let w_ext4 = fs.lookup_live_by_lock_id(w_f.lock_id()).ok_or("lookup live write_err")?;
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(w_f.sync(false), Err(VfsError::Io)));
        kassert_eq!(w_ext4.reclaim_clean_pages(16), 0);
        fault_cache.fail_writes.store(false, Ordering::Release);
        w_f.sync(false).map_err(|_| "retry sync after write error")?;
        kassert_eq!(w_ext4.reclaim_clean_pages(16), 1);
        kassert_eq!(read_all_inode(&w_f).map_err(|_| "re-fault write_err")?, new_data);
        drop(w_ext4);
        drop(w_f);

        // 5. Verify persisted bytes on a fresh read-only mount of the backing image.
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 14), true).map_err(|_| "ro mount")?;
        let ro_root = ro_fs.root_inode();
        let ro_dirty = ro_root.lookup("dirty_err.bin").map_err(|_| "ro lookup dirty_err")?;
        kassert_eq!(read_all_inode(&ro_dirty).map_err(|_| "ro read dirty_err")?, payload);
        let ro_write = ro_root.lookup("write_err.bin").map_err(|_| "ro lookup write_err")?;
        kassert_eq!(read_all_inode(&ro_write).map_err(|_| "ro read write_err")?, new_data);

        Ok(())
    }
);

crate::ktest!(
    fn ext4_truncate_and_open_unlink_preserve_state_on_io_error() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering;
        use lumen_bcache::BlockCache;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache_raw = BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?;
        let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
        let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
        let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, 15), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. truncate shrink under fail_reads and fail_writes preserves meta.size and dirty PageCache bytes.
        let (tf, _) = open_inode(
            &tables,
            None,
            b"/trunc_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create trunc_err.bin")?;
        let mut payload_1 = vec![0u8; 2 * 4096];
        let mut payload_2 = vec![0u8; 2 * 4096];
        for i in 0..payload_1.len() {
            payload_1[i] = ((i * 19) ^ 0x5A) as u8;
            payload_2[i] = ((i * 41) ^ 0xC3) as u8;
        }
        kassert_eq!(tf.write_at(0, &payload_1).map_err(|_| "write payload_1")?, payload_1.len());

        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after read err")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after trunc read err")?, payload_1);
        fault_cache.fail_reads.store(false, Ordering::Release);

        tf.sync(false).map_err(|_| "sync payload_1")?;
        kassert_eq!(tf.write_at(0, &payload_2).map_err(|_| "write payload_2")?, payload_2.len());
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after write err")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after trunc write err")?, payload_2);
        fault_cache.fail_writes.store(false, Ordering::Release);

        tf.truncate(100).map_err(|_| "retry truncate shrink")?;
        kassert_eq!(tf.stat().map_err(|_| "stat after shrink")?.size, 100);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after shrink")?, payload_2[..100]);

        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(200), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after extend err")?.size, 100);
        fault_cache.fail_reads.store(false, Ordering::Release);

        tf.truncate(200).map_err(|_| "retry truncate extend")?;
        let extended = read_all_inode(&tf).map_err(|_| "read after extend")?;
        kassert_eq!(extended.len(), 200);
        kassert_eq!(&extended[..100], &payload_2[..100]);
        kassert!(extended[100..200].iter().all(|&b| b == 0));
        tf.truncate(100).map_err(|_| "final shrink 100")?;
        tf.sync(false).map_err(|_| "sync trunc_err")?;
        drop(tf);

        // 2. unlink on open file (open_count > 0, nlink == 1) under disk I/O error preserves
        //    nlink == 1, live_inodes/path lookup, meta_dirty/dirty contents, and DETACHED_QUOTA.
        let (uf, _) = open_inode(
            &tables,
            None,
            b"/open_unlink_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create open_unlink_err.bin")?;
        kassert_eq!(uf.write_at(0, &payload_1).map_err(|_| "write u_payload")?, payload_1.len());
        uf.sync(false).map_err(|_| "sync u_payload")?;
        let u_ext4 = fs.lookup_live_by_lock_id(uf.lock_id()).ok_or("lookup live u_ext4")?;
        kassert_eq!(u_ext4.reclaim_clean_pages(16), 2);
        kassert_eq!(uf.write_at(8192, b"EXTRA").map_err(|_| "write extra tail")?, 5);

        let ug = OpenGuard::new(Arc::clone(&uf));
        let orig_ino = uf.ino();
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();
        let detached_before = page_cache::detached_cached_pages();
        let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(unlink_at(&tables, None, b"/open_unlink_err.bin", false), Err(VfsError::Io)));
        kassert!(!uf.is_detached());
        kassert_eq!(uf.ino(), orig_ino);
        kassert_eq!(uf.stat().map_err(|_| "stat after unlink err")?.nlink, 1);
        kassert!(fs.lookup_live_by_lock_id(uf.lock_id()).is_some());
        kassert_eq!(
            resolve_path(&tables, None, b"/open_unlink_err.bin", rf).map_err(|_| "still linked")?.inode.ino(),
            orig_ino
        );
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        kassert_eq!(page_cache::detached_cached_pages(), detached_before);
        kassert_eq!(u_ext4.reclaim_clean_pages(16), 0);
        let mut expected_u = payload_1.clone();
        expected_u.extend_from_slice(b"EXTRA");
        kassert_eq!(read_all_inode(&uf).map_err(|_| "read after failed unlink")?, expected_u);
        fault_cache.fail_writes.store(false, Ordering::Release);

        // meta_dirty must still be true so fdatasync (sync(true)) writes page 2 and updates size.
        uf.sync(true).map_err(|_| "sync(true) preserves meta_dirty")?;
        unlink_at(&tables, None, b"/open_unlink_err.bin", false).map_err(|_| "retry open unlink")?;
        kassert!(uf.is_detached());
        kassert_ne_u64(uf.ino(), orig_ino)?;
        kassert_eq!(uf.stat().map_err(|_| "stat detached")?.nlink, 0);
        kassert!(fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
        kassert!(matches!(resolve_path(&tables, None, b"/open_unlink_err.bin", rf), Err(VfsError::NoEnt)));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before + 3);
        kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached after unlink")?, expected_u);
        drop(u_ext4);
        drop(ug);
        drop(uf);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        kassert_eq!(page_cache::detached_cached_pages(), detached_before);

        // 3. rmdir and rename overwrite on open targets under fail_writes preserve live state.
        let dir_ino = mkdir_at(&tables, None, b"/open_rmdir", 0o755, 0).map_err(|_| "mkdir open_rmdir")?;
        let dg = OpenGuard::new(Arc::clone(&dir_ino));
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(unlink_at(&tables, None, b"/open_rmdir", true), Err(VfsError::Io)));
        kassert!(!dir_ino.is_detached());
        kassert_eq!(dir_ino.stat().map_err(|_| "stat open_rmdir")?.nlink, 2);
        kassert!(fs.lookup_live_by_lock_id(dir_ino.lock_id()).is_some());
        fault_cache.fail_writes.store(false, Ordering::Release);
        unlink_at(&tables, None, b"/open_rmdir", true).map_err(|_| "retry open rmdir")?;
        kassert!(dir_ino.is_detached());
        kassert_eq!(dir_ino.stat().map_err(|_| "stat detached rmdir")?.nlink, 0);
        drop(dg);
        drop(dir_ino);

        let (rsrc, _) = open_inode(
            &tables,
            None,
            b"/ren_src.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create ren_src")?;
        kassert_eq!(rsrc.write_at(0, b"NEW").map_err(|_| "write ren_src")?, 3);
        drop(rsrc);
        let (rdst, _) = open_inode(
            &tables,
            None,
            b"/ren_dst.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create ren_dst")?;
        kassert_eq!(rdst.write_at(0, b"OLD").map_err(|_| "write ren_dst")?, 3);
        let rg = OpenGuard::new(Arc::clone(&rdst));
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(
            rename_at2(&tables, None, b"/ren_src.bin", None, b"/ren_dst.bin", RenameFlags::default()),
            Err(VfsError::Io)
        ));
        kassert!(!rdst.is_detached());
        kassert_eq!(rdst.stat().map_err(|_| "stat ren_dst")?.nlink, 1);
        kassert!(fs.lookup_live_by_lock_id(rdst.lock_id()).is_some());
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        fault_cache.fail_writes.store(false, Ordering::Release);
        rename_at2(&tables, None, b"/ren_src.bin", None, b"/ren_dst.bin", RenameFlags::default())
            .map_err(|_| "retry rename overwrite")?;
        kassert!(rdst.is_detached());
        kassert_eq!(rdst.stat().map_err(|_| "stat detached ren_dst")?.nlink, 0);
        kassert_eq!(read_all_inode(&rdst).map_err(|_| "read detached ren_dst")?, b"OLD");
        drop(rg);
        drop(rdst);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);

        // 4. Verify persisted truncated prefix and renamed file on fresh RO mount.
        fs.sync().map_err(|_| "final fs.sync")?;
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 16), true).map_err(|_| "ro mount")?;
        let ro_root = ro_fs.root_inode();
        let ro_trunc = ro_root.lookup("trunc_err.bin").map_err(|_| "ro lookup trunc_err")?;
        kassert_eq!(read_all_inode(&ro_trunc).map_err(|_| "ro read trunc_err")?, payload_2[..100]);
        let ro_ren = ro_root.lookup("ren_dst.bin").map_err(|_| "ro lookup ren_dst")?;
        kassert_eq!(read_all_inode(&ro_ren).map_err(|_| "ro read ren_dst")?, b"NEW");

        Ok(())
    }
);

#[cfg(feature = "ktest")]
struct TestExt4Mount {
    mem_dev: MemBlkDev,
    fault_cache: Arc<FaultInjectBlockCache>,
    fs: Arc<ext4::Ext4FileSystem>,
    tables: KernelVfsTables,
}

#[cfg(feature = "ktest")]
fn mount_test_ext4(img: Vec<u8>, dev_minor: u32) -> Result<TestExt4Mount, &'static str> {
    use lumen_bcache::BlockCache;
    let mem_dev = MemBlkDev::new(img, false);
    let bcache_raw = BlockCache::new(mem_dev.clone(), 32).map_err(|_| "bcache init")?;
    let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
    let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
    let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, dev_minor), false).map_err(|_| "ext4 mount")?;
    let tables = KernelVfsTables::new();
    tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;
    Ok(TestExt4Mount { mem_dev, fault_cache, fs, tables })
}

#[cfg(feature = "ktest")]
fn mount_ro_snapshot(m: &TestExt4Mount, dev_minor: u32) -> Result<Arc<ext4::Ext4FileSystem>, &'static str> {
    use ext4::BlockCacheBackend;
    use lumen_bcache::BlockCache;
    m.fault_cache.sync().map_err(|_| "fault_cache sync")?;
    let ro_dev = MemBlkDev::new(m.mem_dev.0.data.lock().clone(), true);
    let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
        Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
    ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, dev_minor), true).map_err(|_| "ro mount")
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_rmdir_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 20)?;
    let d = mkdir_at(&m.tables, None, b"/victim_dir", 0o755, 0).map_err(|_| "mkdir victim_dir")?;
    drop(d);
    m.fs.sync().map_err(|_| "sync victim_dir")?;
    let snap = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/victim_dir", true).map_err(|_| "measure rmdir")?;
    let total_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(total_writes >= 10);
    drop(m);

    for &open in &[true, false] {
        for fail_at in 1..=total_writes {
            let m = mount_test_ext4(snap.clone(), 21)?;
            let d = resolve_path(&m.tables, None, b"/victim_dir", rf).map_err(|_| "resolve victim_dir")?.inode;
            let orig_ino = d.ino();
            let guard = if open { Some(OpenGuard::new(Arc::clone(&d))) } else { None };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = unlink_at(&m.tables, None, b"/victim_dir", true);
            m.fault_cache.arm_write_fail_on(0);

            match res {
                Ok(()) => {
                    kassert!(matches!(resolve_path(&m.tables, None, b"/victim_dir", rf), Err(VfsError::NoEnt)));
                    if open {
                        kassert!(d.is_detached());
                        kassert_ne_u64(d.ino(), orig_ino)?;
                    }
                    kassert_eq!(d.stat().map_err(|_| "stat ok")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_none());
                    let (nf, _) = open_inode(
                        &m.tables,
                        None,
                        b"/new_after_rmdir",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create after ok rmdir")?;
                    kassert_eq!(nf.inode_type(), InodeType::File);
                }
                Err(VfsError::Io) => {
                    if resolve_path(&m.tables, None, b"/victim_dir", rf).is_ok() {
                        kassert!(!d.is_detached());
                        kassert_eq!(d.stat().map_err(|_| "stat still live")?.nlink, 2);
                        kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_some());
                        unlink_at(&m.tables, None, b"/victim_dir", true).map_err(|_| "retry rmdir")?;
                        if open {
                            kassert!(d.is_detached());
                        }
                        kassert_eq!(d.stat().map_err(|_| "stat retried")?.nlink, 0);
                    } else {
                        if open {
                            kassert!(d.is_detached());
                            kassert_ne_u64(d.ino(), orig_ino)?;
                        }
                        kassert_eq!(d.stat().map_err(|_| "stat removed")?.nlink, 0);
                        kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_none());
                        let (nf, _) = open_inode(
                            &m.tables,
                            None,
                            b"/new_after_rmdir",
                            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                            0o644,
                            0,
                        )
                        .map_err(|_| "create after partial rmdir")?;
                        kassert_eq!(nf.inode_type(), InodeType::File);
                    }
                }
                Err(_) => return Err("unexpected error in rmdir sweep"),
            }
            drop(guard);
            drop(d);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }
    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_rename_dir_overwrite_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 22)?;
    let s = mkdir_at(&m.tables, None, b"/src_d", 0o755, 0).map_err(|_| "mkdir src_d")?;
    let d = mkdir_at(&m.tables, None, b"/dst_d", 0o755, 0).map_err(|_| "mkdir dst_d")?;
    drop((s, d));
    m.fs.sync().map_err(|_| "sync rename dir")?;
    let snap = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default()).map_err(|_| "measure ren dir")?;
    let total_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(total_writes >= 12);
    drop(m);

    for fail_at in 1..=total_writes {
        let m = mount_test_ext4(snap.clone(), 23)?;
        let dst = resolve_path(&m.tables, None, b"/dst_d", rf).map_err(|_| "resolve dst_d")?.inode;
        let dst_orig_ino = dst.ino();
        let guard = OpenGuard::new(Arc::clone(&dst));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(dst.is_detached());
                kassert_eq!(dst.stat().map_err(|_| "stat dst ok")?.nlink, 0);
                kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                let dst_cur = resolve_path(&m.tables, None, b"/dst_d", rf);
                if matches!(dst_cur, Ok(ref r) if r.inode.ino() == dst_orig_ino) {
                    kassert!(!dst.is_detached());
                    kassert_eq!(dst.stat().map_err(|_| "stat dst live")?.nlink, 2);
                    kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_some());
                    rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default())
                        .map_err(|_| "retry ren dir")?;
                    kassert!(dst.is_detached());
                } else {
                    kassert!(dst.is_detached());
                    kassert_ne_u64(dst.ino(), dst_orig_ino)?;
                    kassert_eq!(dst.stat().map_err(|_| "stat dst detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_none());
                    let (nf, _) = open_inode(
                        &m.tables,
                        None,
                        b"/new_after_ren_dir",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create after partial ren dir")?;
                    kassert_eq!(nf.inode_type(), InodeType::File);
                }
            }
            Err(_) => return Err("unexpected error in rename dir overwrite sweep"),
        }
        drop(guard);
        drop(dst);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_inline_symlink_and_file_unlink_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Inline symlink unlink (open & closed) + rename overwrite
    let m = mount_test_ext4(base_img.to_vec(), 24)?;
    let s = symlink_at(&m.tables, b"/real/target", None, b"/sym_i").map_err(|_| "symlink sym_i")?;
    let s_src = symlink_at(&m.tables, b"/other/target", None, b"/sym_s").map_err(|_| "symlink sym_s")?;
    drop((s, s_src));
    m.fs.sync().map_err(|_| "sync syms")?;
    let snap_sym = m.mem_dev.0.data.lock().clone();

    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/sym_i", false).map_err(|_| "measure sym unlink")?;
    let sym_unlink_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(sym_unlink_writes >= 8);
    drop(m);

    for &open in &[true, false] {
        for fail_at in 1..=sym_unlink_writes {
            let m = mount_test_ext4(snap_sym.clone(), 25)?;
            let sym = resolve_path(&m.tables, None, b"/sym_i", rf).map_err(|_| "resolve sym_i")?.inode;
            let guard = if open { Some(OpenGuard::new(Arc::clone(&sym))) } else { None };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = unlink_at(&m.tables, None, b"/sym_i", false);
            m.fault_cache.arm_write_fail_on(0);

            match res {
                Ok(()) => {
                    kassert!(matches!(resolve_path(&m.tables, None, b"/sym_i", rf), Err(VfsError::NoEnt)));
                    if open {
                        kassert!(sym.is_detached());
                        kassert_eq!(sym.readlink().map_err(|_| "sym readlink ok")?, b"/real/target");
                    }
                    kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                }
                Err(VfsError::Io) => {
                    if resolve_path(&m.tables, None, b"/sym_i", rf).is_ok() {
                        kassert!(!sym.is_detached());
                        kassert_eq!(sym.readlink().map_err(|_| "sym readlink live")?, b"/real/target");
                        kassert_eq!(
                            readlink_at(&m.tables, None, b"/sym_i").map_err(|_| "path readlink")?,
                            b"/real/target"
                        );
                        unlink_at(&m.tables, None, b"/sym_i", false).map_err(|_| "retry sym unlink")?;
                        if open {
                            kassert!(sym.is_detached());
                            kassert_eq!(sym.readlink().map_err(|_| "sym readlink detached")?, b"/real/target");
                        }
                    } else {
                        if open {
                            kassert!(sym.is_detached());
                            kassert_eq!(sym.readlink().map_err(|_| "sym readlink detached2")?, b"/real/target");
                        }
                        kassert_eq!(sym.stat().map_err(|_| "sym stat removed")?.nlink, 0);
                        kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                        let nd = mkdir_at(&m.tables, None, b"/new_dir_sym", 0o755, 0).map_err(|_| "mkdir after sym")?;
                        kassert_eq!(nd.inode_type(), InodeType::Dir);
                    }
                }
                Err(_) => return Err("unexpected error in sym unlink sweep"),
            }
            drop(guard);
            drop(sym);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }

    // Inline symlink rename overwrite
    let m_ren = mount_test_ext4(snap_sym.clone(), 26)?;
    m_ren.fault_cache.arm_write_fail_on(0);
    rename_at2(&m_ren.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default())
        .map_err(|_| "measure sym rename")?;
    let sym_ren_writes = m_ren.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(sym_ren_writes >= sym_unlink_writes);
    drop(m_ren);

    for fail_at in 1..=sym_ren_writes {
        let m = mount_test_ext4(snap_sym.clone(), 27)?;
        let sym = resolve_path(&m.tables, None, b"/sym_i", rf).map_err(|_| "resolve sym_i ren")?.inode;
        let sym_orig_ino = sym.ino();
        let guard = OpenGuard::new(Arc::clone(&sym));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(sym.is_detached());
                kassert_eq!(sym.readlink().map_err(|_| "sym ren ok readlink")?, b"/real/target");
                kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                let cur = resolve_path(&m.tables, None, b"/sym_i", rf);
                if matches!(cur, Ok(ref r) if r.inode.ino() == sym_orig_ino) {
                    kassert!(!sym.is_detached());
                    kassert_eq!(sym.readlink().map_err(|_| "sym ren live readlink")?, b"/real/target");
                    rename_at2(&m.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default())
                        .map_err(|_| "retry sym rename")?;
                    kassert!(sym.is_detached());
                } else {
                    kassert!(sym.is_detached());
                    kassert_eq!(sym.stat().map_err(|_| "sym ren detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                    let nd =
                        mkdir_at(&m.tables, None, b"/new_dir_sym2", 0o755, 0).map_err(|_| "mkdir after sym ren")?;
                    kassert_eq!(nd.inode_type(), InodeType::Dir);
                }
            }
            Err(_) => return Err("unexpected error in sym rename sweep"),
        }
        drop(guard);
        drop(sym);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Regular file unlink (open)
    let m = mount_test_ext4(base_img.to_vec(), 28)?;
    let (uf, _) = open_inode(
        &m.tables,
        None,
        b"/reg.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create reg.bin")?;
    let pattern = vec![0x5Au8; 4096];
    kassert_eq!(uf.write_at(0, &pattern).map_err(|_| "write reg.bin")?, 4096);
    uf.sync(false).map_err(|_| "sync reg.bin")?;
    drop(uf);
    m.fs.sync().map_err(|_| "sync reg fs")?;
    let snap_reg = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/reg.bin", false).map_err(|_| "measure reg unlink")?;
    let reg_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(reg_writes >= 9);
    drop(m);

    for fail_at in 1..=reg_writes {
        let m = mount_test_ext4(snap_reg.clone(), 29)?;
        let uf = resolve_path(&m.tables, None, b"/reg.bin", rf).map_err(|_| "resolve reg.bin")?.inode;
        let guard = OpenGuard::new(Arc::clone(&uf));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = unlink_at(&m.tables, None, b"/reg.bin", false);
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(uf.is_detached());
                kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached reg ok")?, pattern);
                kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                if resolve_path(&m.tables, None, b"/reg.bin", rf).is_ok() {
                    kassert!(!uf.is_detached());
                    kassert_eq!(uf.stat().map_err(|_| "stat reg live")?.nlink, 1);
                    kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_some());
                    unlink_at(&m.tables, None, b"/reg.bin", false).map_err(|_| "retry reg unlink")?;
                    kassert!(uf.is_detached());
                } else {
                    kassert!(uf.is_detached());
                    kassert_eq!(uf.stat().map_err(|_| "stat reg detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
                    kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached reg eio")?, pattern);
                    let nd = mkdir_at(&m.tables, None, b"/new_dir_reg", 0o755, 0).map_err(|_| "mkdir after reg")?;
                    kassert_eq!(nd.inode_type(), InodeType::Dir);
                }
            }
            Err(_) => return Err("unexpected error in reg unlink sweep"),
        }
        drop(guard);
        drop(uf);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_rename_step2_and_exchange_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Cross-dir file move step-2 sweep
    let m = mount_test_ext4(base_img.to_vec(), 30)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd")?;
    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/sd/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/f")?;
    kassert_eq!(sf.write_at(0, b"FILE_DATA").map_err(|_| "write /sd/f")?, 9);
    drop((sd, dd, sf));
    m.fs.sync().map_err(|_| "sync step2 file")?;
    let snap_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default()).map_err(|_| "measure ren file")?;
    let file_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(file_ren_writes >= 4);
    drop(m);

    for fail_at in 1..=file_ren_writes {
        let m = mount_test_ext4(snap_f.clone(), 31)?;
        let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve /sd/f")?.inode;
        let orig_ino = f.ino();
        let guard = OpenGuard::new(Arc::clone(&f));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        kassert!(res.is_err() || res.is_ok());

        let old_res = resolve_path(&m.tables, None, b"/sd/f", rf);
        let new_res = resolve_path(&m.tables, None, b"/dd/f", rf);
        match (old_res, new_res) {
            (Ok(_), Err(VfsError::NoEnt)) => {
                kassert!(!f.is_detached());
                kassert_eq!(f.stat().map_err(|_| "stat rolled back f")?.nlink, 1);
                kassert!(m.fs.lookup_live_by_lock_id(f.lock_id()).is_some());
                unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink rolled back f")?;
                kassert!(f.is_detached());
            }
            (Err(VfsError::NoEnt), Ok(n)) => {
                kassert!(!f.is_detached());
                kassert_eq!(n.inode.ino(), orig_ino);
                kassert_eq!(n.inode.lock_id(), f.lock_id());
                kassert_eq!(n.inode.stat().map_err(|_| "stat moved f")?.nlink, 1);
                unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink moved f")?;
                kassert!(f.is_detached());
            }
            (Ok(_), Ok(n)) => {
                kassert!(!f.is_detached());
                kassert_eq!(n.inode.lock_id(), f.lock_id());
                kassert!(f.stat().map_err(|_| "stat dual f")?.nlink >= 2);
                unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink dual f1")?;
                kassert!(!f.is_detached());
                let n2 = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "still at /dd/f")?.inode;
                kassert_eq!(n2.lock_id(), f.lock_id());
                unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink dual f2")?;
                kassert!(f.is_detached());
            }
            _ => return Err("unexpected file rename step2 state"),
        }
        kassert_eq!(read_all_inode(&f).map_err(|_| "read f detached")?, b"FILE_DATA");
        drop(guard);
        drop(f);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Cross-dir directory move step-2 sweep
    let m = mount_test_ext4(base_img.to_vec(), 32)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd2")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd2")?;
    let sub = mkdir_at(&m.tables, None, b"/sd/sub", 0o755, 0).map_err(|_| "mkdir sub")?;
    drop((sd, dd, sub));
    m.fs.sync().map_err(|_| "sync step2 dir")?;
    let snap_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default()).map_err(|_| "measure ren sub")?;
    let dir_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(dir_ren_writes >= 6);
    drop(m);

    for fail_at in 1..=dir_ren_writes {
        let m = mount_test_ext4(snap_d.clone(), 33)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd")?.inode;
        let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 34)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro lookup sd")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro lookup dd")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        match (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf)) {
            (Ok(_), Err(VfsError::NoEnt)) => {
                unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir rolled back sub")?;
            }
            (Err(VfsError::NoEnt), Ok(_)) => {
                let parent_via_dotdot =
                    resolve_path(&m.tables, None, b"/dd/sub/..", rf).map_err(|_| "resolve /dd/sub/..")?.inode;
                let mut post_mismatches = 0usize;
                if sd.stat().map_err(|_| "stat sd moved")?.nlink != 2 {
                    post_mismatches += 1;
                }
                if parent_via_dotdot.ino() != dd.ino() {
                    post_mismatches += 1;
                }
                if res.is_ok() {
                    kassert_eq!(post_mismatches, 0);
                } else {
                    kassert!(post_mismatches <= 1);
                }
                unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir moved sub")?;
            }
            (Ok(_), Ok(_)) => {
                kassert!(sub.stat().map_err(|_| "stat dual sub")?.nlink >= 3);
            }
            _ => return Err("unexpected dir rename step2 state"),
        }
        drop((sd, dd, sub, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (3) RENAME_EXCHANGE cross-dir dir <-> inline symlink sweep
    let m = mount_test_ext4(base_img.to_vec(), 35)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd3")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd3")?;
    let da = mkdir_at(&m.tables, None, b"/sd/dir_a", 0o755, 0).map_err(|_| "mkdir dir_a")?;
    let sb = symlink_at(&m.tables, b"/exch/target", None, b"/dd/sym_b").map_err(|_| "symlink sym_b")?;
    drop((sd, dd, da, sb));
    m.fs.sync().map_err(|_| "sync exch")?;
    let snap_ex = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure exchange")?;
    let exch_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_writes >= 8);
    drop(m);

    for fail_at in 1..=exch_writes {
        let m = mount_test_ext4(snap_ex.clone(), 36)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd ex")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd ex")?.inode;
        let da = resolve_path(&m.tables, None, b"/sd/dir_a", rf).map_err(|_| "resolve dir_a")?.inode;
        let sb = resolve_path(&m.tables, None, b"/dd/sym_b", rf).map_err(|_| "resolve sym_b")?.inode;
        let ga = OpenGuard::new(Arc::clone(&da));
        let gb = OpenGuard::new(Arc::clone(&sb));

        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        kassert!(!da.is_detached());
        kassert!(!sb.is_detached());
        kassert_eq!(sb.readlink().map_err(|_| "sym_b readlink ex")?, b"/exch/target");

        let ro_fs = mount_ro_snapshot(&m, 37)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro sd ex")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro dd ex")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd ex")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd ex")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd ex")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd ex")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        let a_at_sd = resolve_path(&m.tables, None, b"/sd/dir_a", rf).is_ok_and(|r| r.inode.ino() == da.ino());
        let b_at_dd = resolve_path(&m.tables, None, b"/dd/sym_b", rf).is_ok_and(|r| r.inode.ino() == sb.ino());
        let a_at_dd = resolve_path(&m.tables, None, b"/dd/sym_b", rf).is_ok_and(|r| r.inode.ino() == da.ino());
        let b_at_sd = resolve_path(&m.tables, None, b"/sd/dir_a", rf).is_ok_and(|r| r.inode.ino() == sb.ino());
        if res.is_ok() {
            kassert!(a_at_dd && b_at_sd);
        } else {
            kassert!((a_at_sd && b_at_dd) || (a_at_dd && b_at_sd));
            if a_at_sd && b_at_dd {
                rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE))
                    .map_err(|_| "retry exchange")?;
            }
        }
        drop((ga, gb, sd, dd, da, sb, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_truncate_shrink_io_recovery(base_img: &[u8]) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 38)?;
    let (tf, _) = open_inode(
        &m.tables,
        None,
        b"/t8k.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create t8k.bin")?;
    let aa_8k = vec![0xAAu8; 2 * 4096];
    kassert_eq!(tf.write_at(0, &aa_8k).map_err(|_| "write aa_8k")?, aa_8k.len());
    tf.sync(false).map_err(|_| "sync aa_8k")?;
    drop(tf);

    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/t200.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create t200.bin")?;
    let bb_200 = vec![0xBBu8; 200];
    kassert_eq!(sf.write_at(0, &bb_200).map_err(|_| "write bb_200")?, bb_200.len());
    sf.sync(false).map_err(|_| "sync bb_200")?;
    drop(sf);
    m.fs.sync().map_err(|_| "sync trunc setup")?;
    let snap = m.mem_dev.0.data.lock().clone();

    // Measure clean truncate write counts for 8192 -> 100 and 200 -> 100.
    let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k")?.inode;
    m.fault_cache.arm_write_fail_on(0);
    tf.truncate(100).map_err(|_| "measure t8k")?;
    let t8k_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(t8k_writes >= 7);
    drop(tf);

    let sf = resolve_path(&m.tables, None, b"/t200.bin", rf).map_err(|_| "resolve t200")?.inode;
    m.fault_cache.arm_write_fail_on(0);
    sf.truncate(100).map_err(|_| "measure t200")?;
    let t200_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(t200_writes >= 3);
    drop((sf, m));

    // Sweep 1..=t8k_writes (hitting ext4_write_at at #1/#2 and ext4_truncate at #7).
    let mut saw_err_at_last_8k = false;
    for fail_at in 1..=t8k_writes {
        let m = mount_test_ext4(snap.clone(), 39)?;
        let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k sweep")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = tf.truncate(100);
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 40)?;
        let ro_tf = ro_fs.root_inode().lookup("t8k.bin").map_err(|_| "ro t8k")?;
        match res {
            Ok(()) => {
                kassert_eq!(tf.stat().map_err(|_| "stat t8k ok")?.size, 100);
                kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k ok")?, aa_8k[..100]);
                kassert_eq!(read_all_inode(&ro_tf).map_err(|_| "ro read t8k ok")?, aa_8k[..100]);
            }
            Err(VfsError::Io) => {
                if fail_at == t8k_writes {
                    saw_err_at_last_8k = true;
                }
                kassert_eq!(tf.stat().map_err(|_| "stat t8k eio")?.size, 8192);
                kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k eio")?, aa_8k);
                kassert_eq!(ro_tf.stat().map_err(|_| "ro stat t8k eio")?.size, 8192);
                kassert_eq!(read_all_inode(&ro_tf).map_err(|_| "ro read t8k eio")?, aa_8k);
            }
            Err(_) => return Err("unexpected error in t8k truncate sweep"),
        }
        drop((ro_fs, tf, m));
    }
    kassert!(saw_err_at_last_8k);

    // Dirty tail in PageCache must also be preserved on ext4_write_at / ext4_truncate EIO.
    for &fail_at in &[1usize, 2usize, t8k_writes] {
        let m = mount_test_ext4(snap.clone(), 43)?;
        let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k dirty")?.inode;
        let cc = [0xCCu8; 32];
        kassert_eq!(tf.write_at(150, &cc).map_err(|_| "dirty write tail")?, cc.len());
        let mut expected = aa_8k.clone();
        expected[150..182].copy_from_slice(&cc);
        m.fault_cache.arm_write_fail_on(fail_at);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        m.fault_cache.arm_write_fail_on(0);
        kassert_eq!(tf.stat().map_err(|_| "stat t8k dirty eio")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k dirty eio")?, expected);
        drop((tf, m));
    }

    // Sweep 1..=t200_writes (hitting ext4_write_at at #1/#2 and ext4_truncate at #3).
    let mut saw_err_at_last_200 = false;
    for fail_at in 1..=t200_writes {
        let m = mount_test_ext4(snap.clone(), 41)?;
        let sf = resolve_path(&m.tables, None, b"/t200.bin", rf).map_err(|_| "resolve t200 sweep")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = sf.truncate(100);
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 42)?;
        let ro_sf = ro_fs.root_inode().lookup("t200.bin").map_err(|_| "ro t200")?;
        match res {
            Ok(()) => {
                kassert_eq!(sf.stat().map_err(|_| "stat t200 ok")?.size, 100);
                kassert_eq!(read_all_inode(&sf).map_err(|_| "read t200 ok")?, bb_200[..100]);
                kassert_eq!(read_all_inode(&ro_sf).map_err(|_| "ro read t200 ok")?, bb_200[..100]);
            }
            Err(VfsError::Io) => {
                if fail_at == t200_writes {
                    saw_err_at_last_200 = true;
                }
                kassert_eq!(sf.stat().map_err(|_| "stat t200 eio")?.size, 200);
                kassert_eq!(read_all_inode(&sf).map_err(|_| "read t200 eio")?, bb_200);
                kassert_eq!(ro_sf.stat().map_err(|_| "ro stat t200 eio")?.size, 200);
                kassert_eq!(read_all_inode(&ro_sf).map_err(|_| "ro read t200 eio")?, bb_200);
            }
            Err(_) => return Err("unexpected error in t200 truncate sweep"),
        }
        drop((ro_fs, sf, m));
    }
    kassert!(saw_err_at_last_200);
    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_cascading_eio_and_post_res_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;
    use lumen_vfs::link_at;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Hardlinked file unlink write-cutoff (From(1..=hl_writes+1), including From(2))
    let m = mount_test_ext4(base_img.to_vec(), 44)?;
    let (hl1, _) = open_inode(
        &m.tables,
        None,
        b"/hl1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /hl1")?;
    kassert_eq!(hl1.write_at(0, b"HL_DATA").map_err(|_| "write /hl1")?, 7);
    hl1.sync(false).map_err(|_| "sync /hl1")?;
    drop(hl1);
    link_at(&m.tables, None, b"/hl1", None, b"/hl2", false, false).map_err(|_| "link /hl2")?;
    m.fs.sync().map_err(|_| "sync hl")?;
    let snap_hl = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/hl1", false).map_err(|_| "measure hl unlink")?;
    let hl_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(hl_writes >= 2);
    drop(m);

    for from_k in 1..=(hl_writes + 1) {
        let m = mount_test_ext4(snap_hl.clone(), 45)?;
        let hl = resolve_path(&m.tables, None, b"/hl1", rf).map_err(|_| "resolve /hl1")?.inode;
        let orig_ino = hl.ino();
        let guard = OpenGuard::new(Arc::clone(&hl));

        m.fault_cache.arm_write_fail_from(from_k);
        let res = unlink_at(&m.tables, None, b"/hl1", false);
        m.fault_cache.arm_write_fail_on(0);
        if from_k <= hl_writes {
            kassert!(res.is_err());
        }

        if resolve_path(&m.tables, None, b"/hl1", rf).is_ok() {
            kassert!(!hl.is_detached());
            kassert_eq!(hl.stat().map_err(|_| "stat hl live")?.nlink, 2);
            unlink_at(&m.tables, None, b"/hl1", false).map_err(|_| "unlink /hl1 post cutoff")?;
        }
        kassert!(!hl.is_detached());
        kassert_eq!(hl.stat().map_err(|_| "stat hl remaining")?.nlink, 1);
        let hl2 = resolve_path(&m.tables, None, b"/hl2", rf).map_err(|_| "resolve /hl2 post unlink")?.inode;
        kassert_eq!(hl2.ino(), orig_ino);
        kassert_eq!(hl2.lock_id(), hl.lock_id());
        kassert_eq!(read_all_inode(&hl2).map_err(|_| "read /hl2")?, b"HL_DATA");
        drop(hl2);
        unlink_at(&m.tables, None, b"/hl2", false).map_err(|_| "unlink /hl2")?;
        kassert!(hl.is_detached());
        kassert_eq!(read_all_inode(&hl).map_err(|_| "read detached hl")?, b"HL_DATA");
        drop(guard);
        drop(hl);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Rename step-2 file move cascading From(k) (incl. From(4)) and Pair(c1, c2) (incl. (3,4)..(4,7))
    let m = mount_test_ext4(base_img.to_vec(), 46)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd casc f")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd casc f")?;
    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/sd/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/f casc")?;
    kassert_eq!(sf.write_at(0, b"FILE_DATA").map_err(|_| "write /sd/f casc")?, 9);
    drop((sd, dd, sf));
    m.fs.sync().map_err(|_| "sync casc f")?;
    let snap_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default()).map_err(|_| "measure casc f")?;
    let file_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(file_ren_writes >= 4);
    drop(m);

    let check_file_step2_post_fault =
        |m: TestExt4Mount, f: Arc<dyn VfsInode>, orig_ino: u64| -> Result<(), &'static str> {
            let guard = OpenGuard::new(Arc::clone(&f));
            let old_res = resolve_path(&m.tables, None, b"/sd/f", rf);
            let new_res = resolve_path(&m.tables, None, b"/dd/f", rf);
            match (old_res, new_res) {
                (Ok(_), Err(VfsError::NoEnt)) => {
                    kassert!(!f.is_detached());
                    let nlink = f.stat().map_err(|_| "stat f rb")?.nlink;
                    kassert!(nlink == 1 || nlink == 2);
                    unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink f rb")?;
                    kassert!(matches!(resolve_path(&m.tables, None, b"/sd/f", rf), Err(VfsError::NoEnt)));
                    if nlink == 1 {
                        kassert!(f.is_detached());
                    }
                }
                (Err(VfsError::NoEnt), Ok(n)) => {
                    kassert!(!f.is_detached());
                    kassert_eq!(n.inode.ino(), orig_ino);
                    unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink f mv")?;
                    kassert!(f.is_detached());
                }
                (Ok(_), Ok(n)) => {
                    kassert!(!f.is_detached());
                    kassert_eq!(n.inode.lock_id(), f.lock_id());
                    kassert!(f.stat().map_err(|_| "stat dual f casc")?.nlink >= 2);
                    unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink /sd/f dual casc")?;
                    kassert!(!f.is_detached());
                    kassert_eq!(f.stat().map_err(|_| "stat /dd/f surviving")?.nlink, 1);
                    let dd_f = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "lookup /dd/f surviving")?.inode;
                    kassert_eq!(dd_f.ino(), orig_ino);
                    kassert_eq!(read_all_inode(&dd_f).map_err(|_| "read /dd/f surviving")?, b"FILE_DATA");
                    let (canary, _) = open_inode(
                        &m.tables,
                        None,
                        b"/canary_f",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create canary_f")?;
                    kassert_ne_u64(canary.ino(), orig_ino)?;
                    drop((dd_f, canary));
                    unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink /dd/f final")?;
                    kassert!(f.is_detached());
                }
                _ => return Err("unexpected cascading file rename state"),
            }
            kassert_eq!(read_all_inode(&f).map_err(|_| "read f final")?, b"FILE_DATA");
            drop(guard);
            drop(f);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            Ok(())
        };

    for from_k in 1..=(file_ren_writes + 1) {
        let m = mount_test_ext4(snap_f.clone(), 47)?;
        let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve f from")?.inode;
        let orig_ino = f.ino();
        m.fault_cache.arm_write_fail_from(from_k);
        let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        check_file_step2_post_fault(m, f, orig_ino)?;
    }

    for c1 in 1..=file_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_f.clone(), 48)?;
            let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve f pair")?.inode;
            let orig_ino = f.ino();
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            check_file_step2_post_fault(m, f, orig_ino)?;
        }
    }

    // (3) Rename step-2 dir move cascading Pair(c1, c2) (incl. (4,5)..(5,8)) and From(k)
    let m = mount_test_ext4(base_img.to_vec(), 49)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd casc d")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd casc d")?;
    let sub = mkdir_at(&m.tables, None, b"/sd/sub", 0o755, 0).map_err(|_| "mkdir sub casc")?;
    drop((sd, dd, sub));
    m.fs.sync().map_err(|_| "sync casc d")?;
    let snap_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default()).map_err(|_| "measure casc d")?;
    let dir_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(dir_ren_writes >= 6);
    drop(m);

    let check_dir_step2_post_fault =
        |m: TestExt4Mount, sub: Arc<dyn VfsInode>, orig_ino: u64| -> Result<(), &'static str> {
            match (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf)) {
                (Ok(_), Err(VfsError::NoEnt)) => {
                    unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir sub rb")?;
                }
                (Err(VfsError::NoEnt), Ok(_)) => {
                    unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir sub mv")?;
                }
                (Ok(_), Ok(n)) => {
                    kassert_eq!(n.inode.lock_id(), sub.lock_id());
                    kassert!(sub.stat().map_err(|_| "stat dual sub casc")?.nlink >= 3);
                    unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir /sd/sub dual")?;
                    kassert!(!sub.is_detached());
                    kassert_eq!(sub.stat().map_err(|_| "stat surviving sub")?.nlink, 2);
                    let dd_sub = resolve_path(&m.tables, None, b"/dd/sub", rf).map_err(|_| "lookup /dd/sub")?.inode;
                    kassert_eq!(dd_sub.ino(), orig_ino);
                    let canary_d = mkdir_at(&m.tables, None, b"/canary_d", 0o755, 0).map_err(|_| "mkdir canary_d")?;
                    kassert_ne_u64(canary_d.ino(), orig_ino)?;
                    drop((dd_sub, canary_d));
                    unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir /dd/sub final")?;
                }
                _ => return Err("unexpected cascading dir rename state"),
            }
            drop((sub, m));
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            Ok(())
        };

    for from_k in 1..=(dir_ren_writes + 1) {
        let m = mount_test_ext4(snap_d.clone(), 50)?;
        let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub from")?.inode;
        let orig_ino = sub.ino();
        m.fault_cache.arm_write_fail_from(from_k);
        let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        check_dir_step2_post_fault(m, sub, orig_ino)?;
    }

    for c1 in 1..=dir_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_d.clone(), 51)?;
            let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub pair")?.inode;
            let orig_ino = sub.ino();
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            check_dir_step2_post_fault(m, sub, orig_ino)?;
        }
    }

    // (4) RENAME_EXCHANGE file<->file single-write post_res checks + cascading Pair(c1, c2) (incl. (9,10)..(10,12))
    let m = mount_test_ext4(base_img.to_vec(), 52)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd ex f")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd ex f")?;
    let (fa, _) = open_inode(
        &m.tables,
        None,
        b"/sd/fa",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/fa")?;
    let (fb, _) = open_inode(
        &m.tables,
        None,
        b"/dd/fb",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /dd/fb")?;
    kassert_eq!(fa.write_at(0, b"FA_DATA").map_err(|_| "write /sd/fa")?, 7);
    kassert_eq!(fb.write_at(0, b"FB_DATA").map_err(|_| "write /dd/fb")?, 7);
    drop((sd, dd, fa, fb));
    m.fs.sync().map_err(|_| "sync ex f")?;
    let snap_ex_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure ex f")?;
    let exch_file_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_file_writes >= 10);
    drop(m);

    // Single-write post_res verification on file<->file RENAME_EXCHANGE
    for fail_at in 1..=exch_file_writes {
        let m = mount_test_ext4(snap_ex_f.clone(), 53)?;
        let fa = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "resolve fa single")?.inode;
        let fb = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "resolve fb single")?.inode;
        let ga = OpenGuard::new(Arc::clone(&fa));
        let gb = OpenGuard::new(Arc::clone(&fb));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        let fa_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fa.ino());
        let fb_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fb.ino());
        if fa_at_dd && fb_at_sd {
            // Post-commit swap: at most one of old_disk.write / new_disk.write could have failed.
            let fa_nlink = fa.stat().map_err(|_| "stat fa post")?.nlink;
            let fb_nlink = fb.stat().map_err(|_| "stat fb post")?.nlink;
            if res.is_ok() {
                kassert_eq!(fa_nlink, 1);
                kassert_eq!(fb_nlink, 1);
            } else {
                kassert!(fa_nlink == 1 || fb_nlink == 1);
                if fb_nlink == 1 {
                    unlink_at(&m.tables, None, b"/sd/fa", false).map_err(|_| "unlink swapped fb")?;
                    kassert!(fb.is_detached());
                } else {
                    unlink_at(&m.tables, None, b"/dd/fb", false).map_err(|_| "unlink swapped fa")?;
                    kassert!(fa.is_detached());
                }
            }
        }
        drop((ga, gb, fa, fb, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // Cascading Pair(c1, c2) on file<->file RENAME_EXCHANGE (covers pair(9,10)..(10,12))
    for c1 in 1..=exch_file_writes {
        for c2 in (c1 + 1)..=(c1 + 6) {
            let m = mount_test_ext4(snap_ex_f.clone(), 54)?;
            let fa = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "resolve fa pair")?.inode;
            let fb = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "resolve fb pair")?.inode;
            let fa_orig_ino = fa.ino();
            let fb_orig_ino = fb.ino();
            let ga = OpenGuard::new(Arc::clone(&fa));
            let gb = OpenGuard::new(Arc::clone(&fb));

            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE));
            m.fault_cache.arm_write_fail_on(0);

            kassert!(!fa.is_detached());
            kassert!(!fb.is_detached());
            let fa_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fa_orig_ino);
            let fa_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fa_orig_ino);
            let fb_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fb_orig_ino);
            let fb_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fb_orig_ino);

            if fa_at_sd || fa_at_dd {
                kassert_eq!(read_all_inode(&fa).map_err(|_| "read fa pair")?, b"FA_DATA");
            }
            if fb_at_dd || fb_at_sd {
                kassert_eq!(read_all_inode(&fb).map_err(|_| "read fb pair")?, b"FB_DATA");
            }

            if fa_at_sd && fa_at_dd {
                kassert!(fa.stat().map_err(|_| "stat dual fa")?.nlink >= 2);
                unlink_at(&m.tables, None, b"/sd/fa", false).map_err(|_| "unlink dual /sd/fa")?;
                kassert!(!fa.is_detached());
                let surv = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "lookup surviving /dd/fb")?.inode;
                kassert_eq!(surv.ino(), fa_orig_ino);
                kassert_eq!(read_all_inode(&surv).map_err(|_| "read surviving /dd/fb")?, b"FA_DATA");
                let (canary, _) = open_inode(
                    &m.tables,
                    None,
                    b"/canary_ex",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "create canary_ex")?;
                kassert_ne_u64(canary.ino(), fa_orig_ino)?;
                drop((surv, canary));
                unlink_at(&m.tables, None, b"/dd/fb", false).map_err(|_| "unlink surviving /dd/fb")?;
                kassert!(fa.is_detached());
            } else if fb_at_dd && fb_at_sd {
                kassert!(fb.stat().map_err(|_| "stat dual fb")?.nlink >= 2);
                unlink_at(&m.tables, None, b"/dd/fb", false).map_err(|_| "unlink dual /dd/fb")?;
                kassert!(!fb.is_detached());
                let surv = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "lookup surviving /sd/fa")?.inode;
                kassert_eq!(surv.ino(), fb_orig_ino);
                kassert_eq!(read_all_inode(&surv).map_err(|_| "read surviving /sd/fa")?, b"FB_DATA");
                drop(surv);
                unlink_at(&m.tables, None, b"/sd/fa", false).map_err(|_| "unlink surviving /sd/fa")?;
                kassert!(fb.is_detached());
            }
            drop((ga, gb, fa, fb, m));
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }

    // (5) RENAME_EXCHANGE dir<->dir single-write post_res verification across 1..=total_writes
    let m = mount_test_ext4(base_img.to_vec(), 55)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd ex d")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd ex d")?;
    let da = mkdir_at(&m.tables, None, b"/sd/da", 0o755, 0).map_err(|_| "mkdir /sd/da")?;
    let db = mkdir_at(&m.tables, None, b"/dd/db", 0o755, 0).map_err(|_| "mkdir /dd/db")?;
    drop((sd, dd, da, db));
    m.fs.sync().map_err(|_| "sync ex d")?;
    let snap_ex_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/da", None, b"/dd/db", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure ex d")?;
    let exch_dir_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_dir_writes >= 12);
    drop(m);

    for fail_at in 1..=exch_dir_writes {
        let m = mount_test_ext4(snap_ex_d.clone(), 56)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd ex d")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd ex d")?.inode;
        let da = resolve_path(&m.tables, None, b"/sd/da", rf).map_err(|_| "resolve da ex d")?.inode;
        let db = resolve_path(&m.tables, None, b"/dd/db", rf).map_err(|_| "resolve db ex d")?.inode;

        let orig_da_ino = da.ino();
        let orig_db_ino = db.ino();
        let ga = OpenGuard::new(Arc::clone(&da));
        let gb = OpenGuard::new(Arc::clone(&db));

        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/da", None, b"/dd/db", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 57)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro sd ex d")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro dd ex d")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd ex d")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd ex d")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd ex d")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd ex d")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        let da_at_dd = resolve_path(&m.tables, None, b"/dd/db", rf).is_ok_and(|r| r.inode.ino() == orig_da_ino);
        let db_at_sd = resolve_path(&m.tables, None, b"/sd/da", rf).is_ok_and(|r| r.inode.ino() == orig_db_ino);
        if da_at_dd && db_at_sd {
            let da_dotdot = resolve_path(&m.tables, None, b"/dd/db/..", rf).map_err(|_| "resolve da ..")?.inode;
            let db_dotdot = resolve_path(&m.tables, None, b"/sd/da/..", rf).map_err(|_| "resolve db ..")?.inode;
            let mut dotdot_mismatches = 0usize;
            kassert_eq!(da.stat().map_err(|_| "stat da post")?.nlink, 2);
            kassert_eq!(db.stat().map_err(|_| "stat db post")?.nlink, 2);
            kassert_eq!(sd.stat().map_err(|_| "stat sd post")?.nlink, 3);
            kassert_eq!(dd.stat().map_err(|_| "stat dd post")?.nlink, 3);
            if da_dotdot.ino() != dd.ino() {
                dotdot_mismatches += 1;
            }
            if db_dotdot.ino() != sd.ino() {
                dotdot_mismatches += 1;
            }
            if res.is_ok() {
                kassert_eq!(dotdot_mismatches, 0);
            } else {
                kassert!(dotdot_mismatches <= 1);
            }
            drop((da_dotdot, db_dotdot));
            unlink_at(&m.tables, None, b"/dd/db", true).map_err(|_| "rmdir swapped da")?;
            unlink_at(&m.tables, None, b"/sd/da", true).map_err(|_| "rmdir swapped db")?;
            kassert!(da.is_detached());
            kassert!(db.is_detached());
            kassert_eq!(da.stat().map_err(|_| "stat da detached")?.nlink, 0);
            kassert_eq!(db.stat().map_err(|_| "stat db detached")?.nlink, 0);
            kassert!(m.fs.lookup_live_by_lock_id(da.lock_id()).is_none());
            kassert!(m.fs.lookup_live_by_lock_id(db.lock_id()).is_none());
            drop((ga, gb, da, db));
            let reuse_a = mkdir_at(&m.tables, None, b"/reuse_ex_a", 0o755, 0).map_err(|_| "mkdir reuse_ex_a")?.ino();
            let reuse_b = mkdir_at(&m.tables, None, b"/reuse_ex_b", 0o755, 0).map_err(|_| "mkdir reuse_ex_b")?.ino();
            kassert!(
                (reuse_a == orig_da_ino && reuse_b == orig_db_ino)
                    || (reuse_a == orig_db_ino && reuse_b == orig_da_ino)
            );
            drop((sd, dd, m));
        } else {
            drop((ga, gb, sd, dd, da, db, m));
        }
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6) Cold-cache (open_count == 0, NO OpenGuard / no prior resolve_path) Pair(2, 3) & From(2)
    //     regression tests across hardlinked files, hardlinked inline symlinks, hardlink
    //     rename overwrites, dual-linked directories, and uncached step-2 file rename.

    // (6a) Cold-cache hardlinked regular file unlink_at("/h1")
    let m = mount_test_ext4(base_img.to_vec(), 58)?;
    let (h1, _) = open_inode(
        &m.tables,
        None,
        b"/h1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold /h1")?;
    let cold_hl_orig_ino = h1.ino();
    kassert_eq!(h1.write_at(0, b"COLD_HL_DATA").map_err(|_| "write cold /h1")?, 12);
    h1.sync(false).map_err(|_| "sync cold /h1")?;
    drop(h1);
    link_at(&m.tables, None, b"/h1", None, b"/h2", false, false).map_err(|_| "link cold /h2")?;
    m.fs.sync().map_err(|_| "sync cold hl")?;
    let snap_cold_hl = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_hl.clone(), 59)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/h1", false);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/h1", false).map_err(|_| "retry cold /h1 unlink")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/h1", rf), Err(VfsError::NoEnt)));
        let h2 = resolve_path(&m.tables, None, b"/h2", rf).map_err(|_| "resolve cold /h2")?.inode;
        kassert_eq!(h2.ino(), cold_hl_orig_ino);
        kassert_eq!(h2.stat().map_err(|_| "stat cold /h2")?.nlink, 1);
        kassert_eq!(read_all_inode(&h2).map_err(|_| "read cold /h2")?, b"COLD_HL_DATA");
        let (canary, _) = open_inode(
            &m.tables,
            None,
            b"/canary",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create cold /canary")?;
        kassert_ne_u64(canary.ino(), cold_hl_orig_ino)?;
        drop((h2, canary));
        unlink_at(&m.tables, None, b"/h2", false).map_err(|_| "cleanup cold /h2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6b) Cold-cache hardlinked inline symlink unlink_at("/sym1")
    let m = mount_test_ext4(base_img.to_vec(), 60)?;
    let sym1 = symlink_at(&m.tables, b"/cold/sym/target", None, b"/sym1").map_err(|_| "symlink cold /sym1")?;
    let cold_sym_orig_ino = sym1.ino();
    drop(sym1);
    link_at(&m.tables, None, b"/sym1", None, b"/sym2", false, false).map_err(|_| "link cold /sym2")?;
    m.fs.sync().map_err(|_| "sync cold sym")?;
    let snap_cold_sym = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_sym.clone(), 61)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/sym1", false);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/sym1", false).map_err(|_| "retry cold /sym1 unlink")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/sym1", rf), Err(VfsError::NoEnt)));
        let sym2 = resolve_path(&m.tables, None, b"/sym2", rf).map_err(|_| "resolve cold /sym2")?.inode;
        kassert_eq!(sym2.ino(), cold_sym_orig_ino);
        kassert_eq!(sym2.stat().map_err(|_| "stat cold /sym2")?.nlink, 1);
        kassert_eq!(sym2.readlink().map_err(|_| "readlink cold /sym2")?, b"/cold/sym/target");
        kassert_eq!(readlink_at(&m.tables, None, b"/sym2").map_err(|_| "readlink_at cold /sym2")?, b"/cold/sym/target");
        let canary_sym = symlink_at(&m.tables, b"/canary/target", None, b"/canary_sym").map_err(|_| "canary_sym")?;
        kassert_ne_u64(canary_sym.ino(), cold_sym_orig_ino)?;
        drop((sym2, canary_sym));
        unlink_at(&m.tables, None, b"/sym2", false).map_err(|_| "cleanup cold /sym2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6c) Cold-cache hardlink overwrite rename_at2("/src", "/h1")
    let m = mount_test_ext4(base_img.to_vec(), 62)?;
    let (ow_h1, _) = open_inode(
        &m.tables,
        None,
        b"/h1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold ow /h1")?;
    let cold_ow_orig_ino = ow_h1.ino();
    kassert_eq!(ow_h1.write_at(0, b"OLD_HL_OW").map_err(|_| "write cold ow /h1")?, 9);
    ow_h1.sync(false).map_err(|_| "sync cold ow /h1")?;
    drop(ow_h1);
    link_at(&m.tables, None, b"/h1", None, b"/h2", false, false).map_err(|_| "link cold ow /h2")?;
    let (ow_src, _) = open_inode(
        &m.tables,
        None,
        b"/src",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold ow /src")?;
    let ow_src_ino = ow_src.ino();
    kassert_eq!(ow_src.write_at(0, b"NEW_SRC_OW").map_err(|_| "write cold ow /src")?, 10);
    ow_src.sync(false).map_err(|_| "sync cold ow /src")?;
    drop(ow_src);
    m.fs.sync().map_err(|_| "sync cold ow")?;
    let snap_cold_ow = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_ow.clone(), 63)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = rename_at2(&m.tables, None, b"/src", None, b"/h1", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        rename_at2(&m.tables, None, b"/src", None, b"/h1", RenameFlags::default())
            .map_err(|_| "retry cold ow rename")?;
        let new_h1 = resolve_path(&m.tables, None, b"/h1", rf).map_err(|_| "resolve new cold ow /h1")?.inode;
        kassert_eq!(new_h1.ino(), ow_src_ino);
        kassert_eq!(read_all_inode(&new_h1).map_err(|_| "read new cold ow /h1")?, b"NEW_SRC_OW");
        let h2 = resolve_path(&m.tables, None, b"/h2", rf).map_err(|_| "resolve cold ow /h2")?.inode;
        kassert_eq!(h2.ino(), cold_ow_orig_ino);
        kassert_eq!(h2.stat().map_err(|_| "stat cold ow /h2")?.nlink, 1);
        kassert_eq!(read_all_inode(&h2).map_err(|_| "read cold ow /h2")?, b"OLD_HL_OW");
        let (canary_ow, _) = open_inode(
            &m.tables,
            None,
            b"/canary_ow",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create cold /canary_ow")?;
        kassert_ne_u64(canary_ow.ino(), cold_ow_orig_ino)?;
        drop((new_h1, h2, canary_ow));
        unlink_at(&m.tables, None, b"/h2", false).map_err(|_| "cleanup cold ow /h2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6d) Cold-cache dual-linked empty directory unlink_at("/sd/sub", true)
    let mut snap_dual_dir_opt: Option<(Vec<u8>, u64)> = None;
    'find_dual: for c1 in 1..=dir_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_d.clone(), 64)?;
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            if let (Ok(sd_r), Ok(dd_r)) =
                (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf))
            {
                let ino = sd_r.inode.ino();
                if dd_r.inode.ino() == ino {
                    drop((sd_r, dd_r));
                    let ro_fs = mount_ro_snapshot(&m, 65)?;
                    let ro_sub = ro_fs
                        .root_inode()
                        .lookup("dd")
                        .and_then(|d| d.lookup("sub"))
                        .map_err(|_| "ro lookup dual sub")?;
                    if ro_sub.stat().map_err(|_| "ro stat dual sub")?.nlink >= 3 {
                        drop(ro_sub);
                        drop(ro_fs);
                        m.fs.sync().map_err(|_| "sync dual dir snap")?;
                        snap_dual_dir_opt = Some((m.mem_dev.0.data.lock().clone(), ino));
                        drop(m);
                        break 'find_dual;
                    }
                }
            }
            drop(m);
        }
    }
    let (snap_dual_dir, dual_dir_orig_ino) = snap_dual_dir_opt.ok_or("missing dual dir snapshot")?;

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_dual_dir.clone(), 66)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/sd/sub", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "retry cold dual rmdir")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/sd/sub", rf), Err(VfsError::NoEnt)));
        let dd_sub = resolve_path(&m.tables, None, b"/dd/sub", rf).map_err(|_| "resolve cold /dd/sub")?.inode;
        kassert_eq!(dd_sub.ino(), dual_dir_orig_ino);
        kassert_eq!(dd_sub.stat().map_err(|_| "stat cold /dd/sub")?.nlink, 2);
        let canary_dir = mkdir_at(&m.tables, None, b"/canary_dir", 0o755, 0).map_err(|_| "mkdir cold /canary_dir")?;
        kassert_ne_u64(canary_dir.ino(), dual_dir_orig_ino)?;
        drop((dd_sub, canary_dir));
        unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "cleanup cold /dd/sub")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6e) Cold-cache step-2 file rename From(4) / Pair(3, 4) without prior resolve_path
    let snap_f_orig_ino = {
        let ro_dev = MemBlkDev::new(snap_f.clone(), true);
        let ro_bc: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(ro_dev, 32).map_err(|_| "ro bc snap_f")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bc, DevId::makedev(0, 67), true).map_err(|_| "ro snap_f")?;
        ro_fs.root_inode().lookup("sd").and_then(|d| d.lookup("f")).map_err(|_| "ro /sd/f")?.ino()
    };
    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_f.clone(), 68)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(4);
        } else {
            m.fault_cache.arm_write_fail_pair(3, 4);
        }
        let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        if resolve_path(&m.tables, None, b"/sd/f", rf).is_ok() && resolve_path(&m.tables, None, b"/dd/f", rf).is_ok() {
            unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "cold s2 unlink /sd/f")?;
            let dd_f = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "cold s2 resolve /dd/f")?.inode;
            kassert_eq!(dd_f.ino(), snap_f_orig_ino);
            kassert_eq!(read_all_inode(&dd_f).map_err(|_| "cold s2 read /dd/f")?, b"FILE_DATA");
            let (canary_s2, _) = open_inode(
                &m.tables,
                None,
                b"/canary_s2",
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create canary_s2")?;
            kassert_ne_u64(canary_s2.ino(), snap_f_orig_ino)?;
            drop((dd_f, canary_s2));
            unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "cleanup cold s2 /dd/f")?;
        }
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    Ok(())
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn sweep_upward_drift_and_dir_floor_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let ro_ino_at = |img: &[u8], dir_name: Option<&str>, name: &str, minor: u32| -> Result<u64, &'static str> {
        let ro_dev = MemBlkDev::new(img.to_vec(), true);
        let ro_bc: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(ro_dev, 32).map_err(|_| "ro bc")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bc, DevId::makedev(0, minor), true).map_err(|_| "ro mount")?;
        let dir = match dir_name {
            Some(d) => ro_fs.root_inode().lookup(d).map_err(|_| "ro lookup parent")?,
            None => ro_fs.root_inode(),
        };
        Ok(dir.lookup(name).map_err(|_| "ro lookup child")?.ino())
    };

    // (A) rename_at2 step-2 Pair(2,3)/From(2) (and Pair(3,4)/From(3) for dir), cold-cache & pinned,
    //     cross-dir & same-dir, across regular file, inline symlink, and directory.
    #[derive(Clone, Copy)]
    enum EntryKind {
        File,
        Symlink,
        Dir,
    }
    for &kind in &[EntryKind::File, EntryKind::Symlink, EntryKind::Dir] {
        for &same_dir in &[false, true] {
            let m = mount_test_ext4(base_img.to_vec(), 70)?;
            mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd A")?;
            if !same_dir {
                mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd A")?;
            }
            let src_path: &[u8] = b"/sd/item";
            let dst_path: &[u8] = if same_dir { b"/sd/item2" } else { b"/dd/item" };
            match kind {
                EntryKind::File => {
                    let (f, _) = open_inode(
                        &m.tables,
                        None,
                        src_path,
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create src file A")?;
                    kassert_eq!(f.write_at(0, b"UP_FILE").map_err(|_| "write src file A")?, 7);
                }
                EntryKind::Symlink => {
                    symlink_at(&m.tables, b"/up/sym/target", None, src_path).map_err(|_| "symlink src A")?;
                }
                EntryKind::Dir => {
                    mkdir_at(&m.tables, None, src_path, 0o755, 0).map_err(|_| "mkdir src dir A")?;
                }
            }
            m.fs.sync().map_err(|_| "sync A")?;
            let snap = m.mem_dev.0.data.lock().clone();
            let orig_ino = ro_ino_at(&snap, Some("sd"), "item", 71)?;
            drop(m);

            let faults: &[(usize, usize)] = match kind {
                EntryKind::Dir => &[(2, 3), (3, 4), (2, 0), (3, 0)],
                _ => &[(2, 3), (2, 0)],
            };
            for &pinned in &[false, true] {
                for &(c1, c2) in faults {
                    let m = mount_test_ext4(snap.clone(), 72)?;
                    let pinned_inode = if pinned {
                        Some(resolve_path(&m.tables, None, src_path, rf).map_err(|_| "pin src A")?.inode)
                    } else {
                        None
                    };
                    let pinned_guard = pinned_inode.as_ref().map(|n| OpenGuard::new(Arc::clone(n)));

                    if c2 == 0 {
                        m.fault_cache.arm_write_fail_from(c1);
                    } else {
                        m.fault_cache.arm_write_fail_pair(c1, c2);
                    }
                    let res = rename_at2(&m.tables, None, src_path, None, dst_path, RenameFlags::default());
                    m.fault_cache.arm_write_fail_on(0);
                    kassert!(matches!(res, Err(VfsError::Io)));

                    let src_ok = resolve_path(&m.tables, None, src_path, rf).ok().map(|r| r.inode);
                    let dst_ok = resolve_path(&m.tables, None, dst_path, rf).ok().map(|r| r.inode);
                    let target = src_ok.as_ref().or(dst_ok.as_ref()).ok_or("missing item A")?;
                    kassert_eq!(target.ino(), orig_ino);
                    let g = OpenGuard::new(Arc::clone(target));
                    let is_dir = matches!(kind, EntryKind::Dir);
                    if src_ok.is_some() && dst_ok.is_some() {
                        unlink_at(&m.tables, None, src_path, is_dir).map_err(|_| "unlink src dual A")?;
                        kassert!(!target.is_detached());
                        unlink_at(&m.tables, None, dst_path, is_dir).map_err(|_| "unlink dst dual A")?;
                    } else if src_ok.is_some() {
                        unlink_at(&m.tables, None, src_path, is_dir).map_err(|_| "unlink src A")?;
                    } else {
                        unlink_at(&m.tables, None, dst_path, is_dir).map_err(|_| "unlink dst A")?;
                    }
                    kassert!(target.is_detached());
                    kassert_eq!(target.stat().map_err(|_| "stat detached A")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(target.lock_id()).is_none());
                    let target_clone = Arc::clone(target);
                    drop((g, src_ok, dst_ok, pinned_guard, pinned_inode));
                    drop(target_clone);

                    let reuse_ino = match kind {
                        EntryKind::File => open_inode(
                            &m.tables,
                            None,
                            b"/reuse_a",
                            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                            0o644,
                            0,
                        )
                        .map_err(|_| "reuse file A")?
                        .0
                        .ino(),
                        EntryKind::Symlink => {
                            symlink_at(&m.tables, b"/t", None, b"/reuse_a").map_err(|_| "reuse sym A")?.ino()
                        }
                        EntryKind::Dir => {
                            mkdir_at(&m.tables, None, b"/reuse_a", 0o755, 0).map_err(|_| "reuse dir A")?.ino()
                        }
                    };
                    kassert_eq!(reuse_ino, orig_ino);
                    drop(m);
                    kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                }
            }
        }
    }

    // (B) RENAME_EXCHANGE Pair(2,3)/From(2), cold-cache & pinned, cross-dir & same-dir,
    //     verifying detach (is_detached, nlink == 0, live_inodes cleared) and exact inode reuse.
    struct ExCase {
        same_dir: bool,
        a_kind: EntryKind,
        b_kind: EntryKind,
    }
    let ex_cases = [
        ExCase { same_dir: false, a_kind: EntryKind::File, b_kind: EntryKind::File },
        ExCase { same_dir: false, a_kind: EntryKind::Dir, b_kind: EntryKind::Symlink },
        ExCase { same_dir: false, a_kind: EntryKind::Dir, b_kind: EntryKind::Dir },
        ExCase { same_dir: true, a_kind: EntryKind::File, b_kind: EntryKind::File },
        ExCase { same_dir: true, a_kind: EntryKind::Dir, b_kind: EntryKind::File },
    ];
    for case in &ex_cases {
        let m = mount_test_ext4(base_img.to_vec(), 73)?;
        mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd B")?;
        if !case.same_dir {
            mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd B")?;
        }
        let path_a: &[u8] = b"/sd/ea";
        let path_b: &[u8] = if case.same_dir { b"/sd/eb" } else { b"/dd/eb" };
        let make_entry = |path: &[u8], kind: EntryKind| -> Result<(), &'static str> {
            match kind {
                EntryKind::File => {
                    let (f, _) = open_inode(
                        &m.tables,
                        None,
                        path,
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create ex B")?;
                    kassert_eq!(f.write_at(0, b"EX_DATA").map_err(|_| "write ex B")?, 7);
                }
                EntryKind::Symlink => {
                    symlink_at(&m.tables, b"/ex/sym/target", None, path).map_err(|_| "sym ex B")?;
                }
                EntryKind::Dir => {
                    mkdir_at(&m.tables, None, path, 0o755, 0).map_err(|_| "mkdir ex B")?;
                }
            }
            Ok(())
        };
        make_entry(path_a, case.a_kind)?;
        make_entry(path_b, case.b_kind)?;
        m.fs.sync().map_err(|_| "sync B")?;
        let snap = m.mem_dev.0.data.lock().clone();
        let orig_a_ino = ro_ino_at(&snap, Some("sd"), "ea", 74)?;
        let orig_b_ino = ro_ino_at(&snap, Some(if case.same_dir { "sd" } else { "dd" }), "eb", 75)?;
        drop(m);

        for &pinned in &[false, true] {
            for &use_from in &[false, true] {
                let m = mount_test_ext4(snap.clone(), 76)?;
                let pin_pair = if pinned {
                    let na = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "pin ea")?.inode;
                    let nb = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "pin eb")?.inode;
                    let ga = OpenGuard::new(Arc::clone(&na));
                    let gb = OpenGuard::new(Arc::clone(&nb));
                    Some((na, nb, ga, gb))
                } else {
                    None
                };

                if use_from {
                    m.fault_cache.arm_write_fail_from(2);
                } else {
                    m.fault_cache.arm_write_fail_pair(2, 3);
                }
                let res = rename_at2(&m.tables, None, path_a, None, path_b, RenameFlags(RenameFlags::EXCHANGE));
                m.fault_cache.arm_write_fail_on(0);
                kassert!(matches!(res, Err(VfsError::Io)));

                let cur_a = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "resolve post ea")?.inode;
                let cur_b = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "resolve post eb")?.inode;
                let ga = OpenGuard::new(Arc::clone(&cur_a));
                let gb = OpenGuard::new(Arc::clone(&cur_b));
                let cur_a_is_dir = cur_a.inode_type() == InodeType::Dir;
                let cur_b_is_dir = cur_b.inode_type() == InodeType::Dir;
                unlink_at(&m.tables, None, path_a, cur_a_is_dir).map_err(|_| "unlink post ea")?;
                unlink_at(&m.tables, None, path_b, cur_b_is_dir).map_err(|_| "unlink post eb")?;
                kassert!(cur_a.is_detached());
                kassert!(cur_b.is_detached());
                kassert_eq!(cur_a.stat().map_err(|_| "stat detached ea")?.nlink, 0);
                kassert_eq!(cur_b.stat().map_err(|_| "stat detached eb")?.nlink, 0);
                kassert!(m.fs.lookup_live_by_lock_id(cur_a.lock_id()).is_none());
                kassert!(m.fs.lookup_live_by_lock_id(cur_b.lock_id()).is_none());
                drop((ga, gb, cur_a, cur_b, pin_pair));

                let (ra, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_b1",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse b1")?;
                let (rb, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_b2",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse b2")?;
                let r1 = ra.ino();
                let r2 = rb.ino();
                kassert!((r1 == orig_a_ino && r2 == orig_b_ino) || (r1 == orig_b_ino && r2 == orig_a_ino));
                drop((ra, rb, m));
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            }
        }
    }

    // (C) Cold-cache directories carrying a pre-existing nlink_floor entering rename step-2
    //     (!same_parent & same_parent) and RENAME_EXCHANGE, plus nlink_ceiling same_parent & !same_parent.
    // (C1) !same_parent rename step-2 & RENAME_EXCHANGE with new_dir carrying nlink_floor = 2 (disk = 1)
    let m = mount_test_ext4(base_img.to_vec(), 77)?;
    let fd = mkdir_at(&m.tables, None, b"/floor_d", 0o755, 0).map_err(|_| "mkdir floor_d")?;
    let fd_orig_ino = fd.ino();
    drop(fd);
    mkdir_at(&m.tables, None, b"/other", 0o755, 0).map_err(|_| "mkdir other")?;
    mkdir_at(&m.tables, None, b"/other/mv_sub", 0o755, 0).map_err(|_| "mkdir mv_sub")?;
    m.fs.sync().map_err(|_| "sync C1")?;
    let snap_c1 = m.mem_dev.0.data.lock().clone();
    drop(m);

    // Arm Pair(2, 3) on rmdir("/floor_d") in cold cache -> /floor_d has disk links_count == 1, nlink_floor == 2.
    {
        let m = mount_test_ext4(snap_c1.clone(), 78)?;
        m.fault_cache.arm_write_fail_pair(2, 3);
        let res = unlink_at(&m.tables, None, b"/floor_d", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res, Err(VfsError::Io)));

        rename_at2(&m.tables, None, b"/other/mv_sub", None, b"/floor_d/mv_sub", RenameFlags::default())
            .map_err(|_| "rename into floor_d")?;
        let ro_fs = mount_ro_snapshot(&m, 79)?;
        let ro_fd = ro_fs.root_inode().lookup("floor_d").map_err(|_| "ro floor_d")?;
        kassert_eq!(ro_fd.stat().map_err(|_| "ro stat floor_d")?.nlink, 3);
        drop(ro_fs);

        let floor_d = resolve_path(&m.tables, None, b"/floor_d", rf).map_err(|_| "resolve floor_d")?.inode;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d")?.nlink, 3);
        unlink_at(&m.tables, None, b"/floor_d/mv_sub", true).map_err(|_| "rmdir mv_sub")?;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d empty")?.nlink, 2);
        let g = OpenGuard::new(Arc::clone(&floor_d));
        unlink_at(&m.tables, None, b"/floor_d", true).map_err(|_| "rmdir floor_d")?;
        kassert!(floor_d.is_detached());
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d det")?.nlink, 0);
        drop((g, floor_d));
        let reuse = mkdir_at(&m.tables, None, b"/reuse_fd", 0o755, 0).map_err(|_| "reuse fd")?.ino();
        kassert_eq!(reuse, fd_orig_ino);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (C2) same_parent rename step-2 with nlink_floor = 2 (disk = 1) & RENAME_EXCHANGE with new_dir nlink_floor = 2 (disk = 1)
    {
        let m = mount_test_ext4(snap_c1.clone(), 80)?;
        m.fault_cache.arm_write_fail_pair(2, 3);
        let res = unlink_at(&m.tables, None, b"/floor_d", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res, Err(VfsError::Io)));

        let (f1, _) = open_inode(
            &m.tables,
            None,
            b"/floor_d/f1",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create floor_d/f1")?;
        drop(f1);
        rename_at2(&m.tables, None, b"/floor_d/f1", None, b"/floor_d/f2", RenameFlags::default())
            .map_err(|_| "same_parent file rename in floor_d")?;
        rename_at2(&m.tables, None, b"/other/mv_sub", None, b"/floor_d/f2", RenameFlags(RenameFlags::EXCHANGE))
            .map_err(|_| "exchange into floor_d")?;
        let ro_fs = mount_ro_snapshot(&m, 81)?;
        let ro_fd = ro_fs.root_inode().lookup("floor_d").map_err(|_| "ro floor_d ex")?;
        kassert_eq!(ro_fd.stat().map_err(|_| "ro stat floor_d ex")?.nlink, 3);
        drop(ro_fs);

        let floor_d = resolve_path(&m.tables, None, b"/floor_d", rf).map_err(|_| "resolve floor_d ex")?.inode;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d ex")?.nlink, 3);
        unlink_at(&m.tables, None, b"/floor_d/f2", true).map_err(|_| "rmdir floor_d/f2")?;
        let g = OpenGuard::new(Arc::clone(&floor_d));
        unlink_at(&m.tables, None, b"/floor_d", true).map_err(|_| "rmdir floor_d ex")?;
        kassert!(floor_d.is_detached());
        drop((g, floor_d));
        let reuse = mkdir_at(&m.tables, None, b"/reuse_fd2", 0o755, 0).map_err(|_| "reuse fd2")?.ino();
        kassert_eq!(reuse, fd_orig_ino);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (C3) Cold-cache directory carrying pre-existing nlink_ceiling (disk = 4, ceiling = 3)
    //      entering same_parent rename step-2 and RENAME_EXCHANGE.
    let m = mount_test_ext4(base_img.to_vec(), 82)?;
    let cp = mkdir_at(&m.tables, None, b"/ceil_p", 0o755, 0).map_err(|_| "mkdir ceil_p")?;
    let cp_orig_ino = cp.ino();
    drop(cp);
    mkdir_at(&m.tables, None, b"/ceil_p/sub1", 0o755, 0).map_err(|_| "mkdir sub1")?;
    mkdir_at(&m.tables, None, b"/ceil_p/sub2", 0o755, 0).map_err(|_| "mkdir sub2")?;
    mkdir_at(&m.tables, None, b"/other", 0o755, 0).map_err(|_| "mkdir other C3")?;
    let (of, _) = open_inode(
        &m.tables,
        None,
        b"/other/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create other/f")?;
    drop(of);
    m.fs.sync().map_err(|_| "sync C3")?;
    let snap_c3 = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/ceil_p/sub1", None, b"/other/f", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure C3")?;
    let c3_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    drop(m);

    let mut tested_ceil_same_parent = false;
    for fail_at in 1..=c3_writes {
        let m = mount_test_ext4(snap_c3.clone(), 83)?;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/ceil_p/sub1", None, b"/other/f", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);
        if res.is_err() {
            let ro_fs = mount_ro_snapshot(&m, 84)?;
            let ro_cp = ro_fs.root_inode().lookup("ceil_p").map_err(|_| "ro ceil_p")?;
            let ro_nlink = ro_cp.stat().map_err(|_| "ro stat ceil_p")?.nlink;
            let ro_swapped = ro_fs
                .root_inode()
                .lookup("other")
                .and_then(|o| o.lookup("f"))
                .is_ok_and(|f| f.inode_type() == InodeType::Dir);
            drop((ro_cp, ro_fs));
            if ro_swapped && ro_nlink == 4 {
                tested_ceil_same_parent = true;
                // Cold-cache same_parent dir rename on ceil_p (disk=4, nlink_ceiling=3).
                rename_at2(&m.tables, None, b"/ceil_p/sub2", None, b"/ceil_p/sub2_mv", RenameFlags::default())
                    .map_err(|_| "same_parent rename on ceil_p")?;
                let ro_fs2 = mount_ro_snapshot(&m, 85)?;
                let ro_cp2 = ro_fs2.root_inode().lookup("ceil_p").map_err(|_| "ro ceil_p 2")?;
                kassert_eq!(ro_cp2.stat().map_err(|_| "ro stat ceil_p 2")?.nlink, 3);
                drop(ro_fs2);

                // same_parent RENAME_EXCHANGE on ceil_p (sub2_mv <-> sub1 which is file 'f').
                rename_at2(
                    &m.tables,
                    None,
                    b"/ceil_p/sub2_mv",
                    None,
                    b"/ceil_p/sub1",
                    RenameFlags(RenameFlags::EXCHANGE),
                )
                .map_err(|_| "same_parent exchange on ceil_p")?;
                let ceil_p = resolve_path(&m.tables, None, b"/ceil_p", rf).map_err(|_| "resolve ceil_p")?.inode;
                kassert_eq!(ceil_p.stat().map_err(|_| "stat ceil_p after")?.nlink, 3);
                unlink_at(&m.tables, None, b"/ceil_p/sub1", true).map_err(|_| "rmdir moved sub2_mv")?;
                unlink_at(&m.tables, None, b"/ceil_p/sub2_mv", false).map_err(|_| "unlink swapped f")?;
                kassert_eq!(ceil_p.stat().map_err(|_| "stat ceil_p empty")?.nlink, 2);
                let g = OpenGuard::new(Arc::clone(&ceil_p));
                unlink_at(&m.tables, None, b"/ceil_p", true).map_err(|_| "rmdir ceil_p")?;
                kassert!(ceil_p.is_detached());
                drop((g, ceil_p));
                let reuse = mkdir_at(&m.tables, None, b"/reuse_cp", 0o755, 0).map_err(|_| "reuse cp")?.ino();
                kassert_eq!(reuse, cp_orig_ino);
                drop(m);
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                break;
            }
        }
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    kassert!(tested_ceil_same_parent);

    Ok(())
}

crate::ktest!(
    fn ext4_partial_phase2_io_error_recovery() {
        let base_img = build_minimal_ext4_image_blocks(256);
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();

        sweep_rmdir_io_recovery(&base_img, quota_before)?;
        sweep_rename_dir_overwrite_io_recovery(&base_img, quota_before)?;
        sweep_inline_symlink_and_file_unlink_io_recovery(&base_img, quota_before)?;
        sweep_rename_step2_and_exchange_io_recovery(&base_img, quota_before)?;
        sweep_truncate_shrink_io_recovery(&base_img)?;
        sweep_cascading_eio_and_post_res_recovery(&base_img, quota_before)?;
        sweep_upward_drift_and_dir_floor_recovery(&base_img, quota_before)?;
        crate::kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);

        Ok(())
    }
);
