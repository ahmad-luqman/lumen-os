//! Read-only `/proc` virtual filesystem (`procfs`, `dev = 0:4`).
//!
//! All regular virtual files report `st_size == 0` in `stat()` and generate
//! fresh content dynamically on `read_at()`. Magic symlinks (`/proc/<pid>/fd/*`
//! and `/proc/<pid>/exe`) implement `resolve_magic_link()` while keeping
//! `readlink()` textual.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, next_lock_id,
};

use crate::arch;
use crate::linux::{self, process};
use crate::mm::frame;
use crate::smp;
use crate::sync::SleepMutex;
use crate::time;

use super::page_cache;

// The node kinds, their inode numbers and numeric names are host-tested in
// `lumen_vfs::procfs`; what needs the live processes stays here.
use lumen_vfs::procfs::{ProcFd, ProcKind, parse_canonical_u32};

/// A `readdir` entry for `kind`, named `name`.
fn dir_entry(kind: ProcKind, name: String) -> (u64, InodeType, String) {
    (kind.ino(), kind.inode_type(), name)
}

/// Whether the process (and, for an fd link, the fd) `kind` shows still
/// exists. The fixed nodes always do.
fn is_live(kind: ProcKind) -> bool {
    match kind {
        ProcKind::PidDir(p)
        | ProcKind::PidExe(p)
        | ProcKind::PidMaps(p)
        | ProcKind::PidFdDir(p)
        | ProcKind::PidMounts(p) => process::has_pid(p),
        ProcKind::PidFdLink(p, fd) => process::process_fd_target(p, fd.get()).is_some(),
        ProcKind::Root
        | ProcKind::Cpuinfo
        | ProcKind::Meminfo
        | ProcKind::Mounts
        | ProcKind::Version
        | ProcKind::SelfLink => true,
    }
}

/// Pseudo-filesystem mounted at `/proc` (`0:4`).
pub struct Procfs {
    dev: DevId,
    root: Arc<ProcfsInode>,
}

impl Procfs {
    pub fn new(dev: DevId) -> Arc<Self> {
        let now = time::realtime_timespec();
        let root = Arc::new_cyclic(|w| ProcfsInode {
            lock_id: next_lock_id(),
            dev,
            kind: ProcKind::Root,
            ctime: now,
            root_weak: Weak::clone(w),
            open_count: AtomicU32::new(0),
            locks: SleepMutex::new(AdvisoryLocks::new()),
        });
        Arc::new(Self { dev, root })
    }
}

impl FileSystem for Procfs {
    fn dev_id(&self) -> DevId {
        self.dev
    }

    fn name(&self) -> &'static str {
        "procfs"
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        Arc::clone(&self.root) as Arc<dyn VfsInode>
    }

    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read_only(&self) -> bool {
        true
    }
}

pub struct ProcfsInode {
    lock_id: u64,
    dev: DevId,
    kind: ProcKind,
    ctime: Timespec,
    root_weak: Weak<ProcfsInode>,
    open_count: AtomicU32,
    locks: SleepMutex<AdvisoryLocks>,
}

impl ProcfsInode {
    fn spawn(&self, kind: ProcKind) -> Arc<Self> {
        Arc::new(Self {
            lock_id: next_lock_id(),
            dev: self.dev,
            kind,
            ctime: self.ctime,
            root_weak: Weak::clone(&self.root_weak),
            open_count: AtomicU32::new(0),
            locks: SleepMutex::new(AdvisoryLocks::new()),
        })
    }

    /// Whether `/proc/<pid>` exists: only for a registered Linux process
    /// (none is invented for the shell's kernel threads).
    fn pid_exists(pid: u32) -> bool {
        process::has_pid(pid)
    }
}

fn cstr_i8(buf: &[i8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let bytes: &[u8] = zerocopy::IntoBytes::as_bytes(&buf[..len]);
    core::str::from_utf8(bytes).unwrap_or("")
}

fn format_cpuinfo() -> String {
    let mut out = String::new();
    let (max_leaf, b0, c0, d0) = arch::cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&b0.to_le_bytes());
    vendor[4..8].copy_from_slice(&d0.to_le_bytes());
    vendor[8..12].copy_from_slice(&c0.to_le_bytes());
    let vendor_str = core::str::from_utf8(&vendor).unwrap_or("Unknown");

    let (eax1, _, ecx1, edx1) = if max_leaf >= 1 { arch::cpuid(1) } else { (0, 0, 0, 0) };
    let stepping = eax1 & 0xf;
    let base_model = (eax1 >> 4) & 0xf;
    let base_family = (eax1 >> 8) & 0xf;
    let ext_model = (eax1 >> 16) & 0xf;
    let ext_family = (eax1 >> 20) & 0xff;
    let family = if base_family == 0xf { base_family + ext_family } else { base_family };
    let model = if base_family == 6 || base_family == 0xf { (ext_model << 4) | base_model } else { base_model };

    let (max_ext, _, _, _) = arch::cpuid(0x8000_0000);
    let mut brand = [0u8; 48];
    let brand_str = if max_ext >= 0x8000_0004 {
        for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let (a, b, c, d) = arch::cpuid(leaf);
            let off = i * 16;
            brand[off..off + 4].copy_from_slice(&a.to_le_bytes());
            brand[off + 4..off + 8].copy_from_slice(&b.to_le_bytes());
            brand[off + 8..off + 12].copy_from_slice(&c.to_le_bytes());
            brand[off + 12..off + 16].copy_from_slice(&d.to_le_bytes());
        }
        let end = brand.iter().position(|&b| b == 0).unwrap_or(brand.len());
        core::str::from_utf8(&brand[..end]).unwrap_or("Lumen x86_64").trim()
    } else {
        "Lumen x86_64"
    };
    let (_, _, _, edx_ext) = if max_ext >= 0x8000_0001 { arch::cpuid(0x8000_0001) } else { (0, 0, 0, 0) };

    let mut flags = String::new();
    let std_edx = [
        (0, "fpu"),
        (3, "pse"),
        (4, "tsc"),
        (5, "msr"),
        (6, "pae"),
        (8, "cx8"),
        (9, "apic"),
        (15, "cmov"),
        (16, "pat"),
        (23, "mmx"),
        (24, "fxsr"),
        (25, "sse"),
        (26, "sse2"),
    ];
    for (bit, name) in std_edx {
        if (edx1 & (1 << bit)) != 0 {
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str(name);
        }
    }
    if (ecx1 & (1 << 0)) != 0 {
        flags.push_str(" sse3");
    }
    if (ecx1 & (1 << 31)) != 0 {
        flags.push_str(" hypervisor");
    }
    if (edx_ext & (1 << 20)) != 0 {
        flags.push_str(" nx");
    }
    if (edx_ext & (1 << 29)) != 0 {
        flags.push_str(" lm");
    }

    let mhz = time::tsc().map_or(0, |c| c.hz() / 1_000_000);
    for cpu_idx in 0..arch::MAX_CPUS {
        let Some(lapic) = smp::online_lapic_id(cpu_idx) else {
            continue;
        };
        let _ = writeln!(out, "processor\t: {cpu_idx}");
        let _ = writeln!(out, "vendor_id\t: {vendor_str}");
        let _ = writeln!(out, "cpu family\t: {family}");
        let _ = writeln!(out, "model\t\t: {model}");
        let _ = writeln!(out, "model name\t: {brand_str}");
        let _ = writeln!(out, "stepping\t: {stepping}");
        let _ = writeln!(out, "cpu MHz\t\t: {mhz}.000");
        let _ = writeln!(out, "apicid\t\t: {lapic}");
        let _ = writeln!(out, "flags\t\t: {flags}");
        out.push('\n');
    }
    out
}

fn format_meminfo() -> String {
    let counts = frame::counts();
    let total_kb = (counts.owned + counts.free).saturating_mul(4);
    let free_kb = counts.free.saturating_mul(4);
    let cached_kb = page_cache::total_cached_pages().saturating_mul(4);
    let reclaimable_kb = page_cache::ext4_cached_pages().saturating_mul(4);
    let avail_kb = free_kb.saturating_add(reclaimable_kb).min(total_kb);
    let mut out = String::new();
    let _ = writeln!(out, "MemTotal:       {total_kb:8} kB");
    let _ = writeln!(out, "MemFree:        {free_kb:8} kB");
    let _ = writeln!(out, "MemAvailable:   {avail_kb:8} kB");
    let _ = writeln!(out, "Buffers:               0 kB");
    let _ = writeln!(out, "Cached:         {cached_kb:8} kB");
    out
}

fn format_mounts() -> String {
    let mounts = super::list_mounts();
    let mut out = String::new();
    for m in mounts {
        // Outside the ktests the only ext4 mount is the root, always the first
        // virtio-blk disk `vfs::init` found; `/dev` has no block nodes yet, so
        // the name follows Linux's (a ktest's ext4 mount is labelled the same).
        let source = if m.fs_name == "ext4" { "/dev/vda" } else { m.fs_name };
        let mode = if m.read_only { "ro" } else { "rw" };
        let _ = writeln!(out, "{source} {} {} {mode} 0 0", m.mount_path, m.fs_name);
    }
    out
}

fn format_version() -> String {
    let uts = linux::default_utsname();
    format!(
        "{} version {} (lumen@lumen-os) (rustc) {}\n",
        cstr_i8(&uts.sysname),
        cstr_i8(&uts.release),
        cstr_i8(&uts.version),
    )
}

fn slice_read(data: &[u8], offset: u64, dst: &mut [u8]) -> usize {
    let Ok(off) = usize::try_from(offset) else {
        return 0;
    };
    if off >= data.len() || dst.is_empty() {
        return 0;
    }
    let n = (data.len() - off).min(dst.len());
    dst[..n].copy_from_slice(&data[off..off + n]);
    n
}

impl VfsInode for ProcfsInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        self.dev
    }

    fn ino(&self) -> Ino {
        self.kind.ino()
    }

    fn inode_type(&self) -> InodeType {
        self.kind.inode_type()
    }

    fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    fn inc_open_count(&self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
    }

    fn dec_open_count(&self) -> u32 {
        self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    fn cacheable(&self) -> bool {
        self.kind.cacheable()
    }

    fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
        #[cfg(feature = "ktest")]
        ASKED_MAGIC.fetch_add(1, Ordering::Relaxed);
        match self.kind {
            ProcKind::PidExe(pid) => {
                let path = process::process_exe_path(pid)?;
                super::resolve_path_from_root(&path).ok()
            }
            ProcKind::PidFdLink(pid, fd) => process::process_fd_target(pid, fd.get()).and_then(|(inode, _)| inode),
            _ => None,
        }
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        // The VFS never caches a per-process node (`cacheable`, #263), but
        // one already resolved can still be held (an open fd, a cwd): once
        // its process or fd is gone, `stat` says so.
        if !is_live(self.kind) {
            return Err(VfsError::NoEnt);
        }
        let itype = self.inode_type();
        Ok(Metadata {
            dev: self.dev,
            ino: self.ino(),
            inode_type: itype,
            mode: self.kind.mode(),
            nlink: if itype == InodeType::Dir { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: DevId::default(),
            size: 0,
            blksize: 4096,
            blocks: 0,
            atime: self.ctime,
            mtime: self.ctime,
            ctime: self.ctime,
        })
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        #[cfg(feature = "ktest")]
        ASKED_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        let root_arc = self.root_weak.upgrade().ok_or(VfsError::NoEnt)?;
        match self.kind {
            ProcKind::Root => match name {
                "." | ".." => Ok(root_arc as Arc<dyn VfsInode>),
                "cpuinfo" => Ok(self.spawn(ProcKind::Cpuinfo) as Arc<dyn VfsInode>),
                "meminfo" => Ok(self.spawn(ProcKind::Meminfo) as Arc<dyn VfsInode>),
                "mounts" => Ok(self.spawn(ProcKind::Mounts) as Arc<dyn VfsInode>),
                "version" => Ok(self.spawn(ProcKind::Version) as Arc<dyn VfsInode>),
                "self" => Ok(self.spawn(ProcKind::SelfLink) as Arc<dyn VfsInode>),
                other => {
                    let pid: u32 = parse_canonical_u32(other).ok_or(VfsError::NoEnt)?;
                    if pid == 0 || !Self::pid_exists(pid) {
                        return Err(VfsError::NoEnt);
                    }
                    Ok(self.spawn(ProcKind::PidDir(pid)) as Arc<dyn VfsInode>)
                }
            },
            ProcKind::PidDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                match name {
                    "." => Ok(self.spawn(ProcKind::PidDir(pid)) as Arc<dyn VfsInode>),
                    ".." => Ok(root_arc as Arc<dyn VfsInode>),
                    "exe" => Ok(self.spawn(ProcKind::PidExe(pid)) as Arc<dyn VfsInode>),
                    "maps" => Ok(self.spawn(ProcKind::PidMaps(pid)) as Arc<dyn VfsInode>),
                    "fd" => Ok(self.spawn(ProcKind::PidFdDir(pid)) as Arc<dyn VfsInode>),
                    "mounts" => Ok(self.spawn(ProcKind::PidMounts(pid)) as Arc<dyn VfsInode>),
                    _ => Err(VfsError::NoEnt),
                }
            }
            ProcKind::PidFdDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                match name {
                    "." => Ok(self.spawn(ProcKind::PidFdDir(pid)) as Arc<dyn VfsInode>),
                    ".." => Ok(self.spawn(ProcKind::PidDir(pid)) as Arc<dyn VfsInode>),
                    other => {
                        let fd_u32 = parse_canonical_u32(other).ok_or(VfsError::NoEnt)?;
                        let fd = i32::try_from(fd_u32).map_err(|_| VfsError::NoEnt)?;
                        let fd = ProcFd::try_from(fd).map_err(|_| VfsError::NoEnt)?;
                        if process::process_fd_target(pid, fd.get()).is_none() {
                            return Err(VfsError::NoEnt);
                        }
                        Ok(self.spawn(ProcKind::PidFdLink(pid, fd)) as Arc<dyn VfsInode>)
                    }
                }
            }
            _ => Err(VfsError::NotDir),
        }
    }

    fn create(&self, _name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::RoFs)
    }

    fn mkdir(&self, _name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::RoFs)
    }

    fn unlink(&self, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn rmdir(&self, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn link(&self, _name: &str, _target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::RoFs)
    }

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        match self.kind {
            // A kernel thread (the shell) has no pid: `/proc/self` dangles.
            ProcKind::SelfLink => Ok(format!("{}", process::current_pid().ok_or(VfsError::NoEnt)?).into_bytes()),
            // No path recorded (a process not loaded from the VFS): no target,
            // the same answer `resolve_magic_link` gives.
            ProcKind::PidExe(pid) => process::process_exe_path(pid).map(String::into_bytes).ok_or(VfsError::NoEnt),
            ProcKind::PidFdLink(pid, fd) => {
                let (_, path) = process::process_fd_target(pid, fd.get()).ok_or(VfsError::NoEnt)?;
                Ok(path.into_bytes())
            }
            _ => Err(VfsError::Inval),
        }
    }

    fn rename(
        &self,
        _old_name: &str,
        _new_dir: &Arc<dyn VfsInode>,
        _new_name: &str,
        _flags: RenameFlags,
    ) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if max_entries == 0 {
            return Ok(());
        }
        let mut entries: Vec<(u64, InodeType, String)> = Vec::new();
        match self.kind {
            ProcKind::Root => {
                entries.push(dir_entry(ProcKind::Root, String::from(".")));
                entries.push(dir_entry(ProcKind::Root, String::from("..")));
                entries.push(dir_entry(ProcKind::Cpuinfo, String::from("cpuinfo")));
                entries.push(dir_entry(ProcKind::Meminfo, String::from("meminfo")));
                entries.push(dir_entry(ProcKind::Mounts, String::from("mounts")));
                entries.push(dir_entry(ProcKind::SelfLink, String::from("self")));
                entries.push(dir_entry(ProcKind::Version, String::from("version")));
                for pid in process::active_pids() {
                    entries.push(dir_entry(ProcKind::PidDir(pid), format!("{pid}")));
                }
            }
            ProcKind::PidDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                entries.push(dir_entry(ProcKind::PidDir(pid), String::from(".")));
                entries.push(dir_entry(ProcKind::Root, String::from("..")));
                entries.push(dir_entry(ProcKind::PidExe(pid), String::from("exe")));
                entries.push(dir_entry(ProcKind::PidFdDir(pid), String::from("fd")));
                entries.push(dir_entry(ProcKind::PidMaps(pid), String::from("maps")));
                entries.push(dir_entry(ProcKind::PidMounts(pid), String::from("mounts")));
            }
            ProcKind::PidFdDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                entries.push(dir_entry(ProcKind::PidFdDir(pid), String::from(".")));
                entries.push(dir_entry(ProcKind::PidDir(pid), String::from("..")));
                for fd in process::process_open_fds(pid) {
                    // The fd table keeps fds in `0..MAX_FDS`; one outside it
                    // couldn't be looked up either, so it isn't listed.
                    let Ok(proc_fd) = ProcFd::try_from(fd) else { continue };
                    entries.push(dir_entry(ProcKind::PidFdLink(pid, proc_fd), format!("{fd}")));
                }
            }
            _ => return Err(VfsError::NotDir),
        }

        let start = usize::try_from(cursor).unwrap_or(entries.len());
        for (idx, (ino, itype, name)) in entries.into_iter().enumerate().skip(start) {
            if out.len() >= max_entries {
                break;
            }
            out.push(DirEntry { ino, inode_type: itype, name, next_cursor: (idx + 1) as u64 });
        }
        Ok(())
    }

    fn truncate(&self, _size: u64) -> Result<(), VfsError> {
        if self.inode_type() == InodeType::Dir { Err(VfsError::IsDir) } else { Err(VfsError::RoFs) }
    }

    fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.inode_type() == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type() != InodeType::File {
            return Err(VfsError::Inval);
        }
        let text = match self.kind {
            ProcKind::Cpuinfo => format_cpuinfo(),
            ProcKind::Meminfo => format_meminfo(),
            ProcKind::Mounts | ProcKind::PidMounts(_) => format_mounts(),
            ProcKind::Version => format_version(),
            ProcKind::PidMaps(pid) => match process::process_address_space(pid) {
                Some(aspace) => aspace.format_maps(),
                None => return Err(VfsError::NoEnt),
            },
            _ => return Err(VfsError::Inval),
        };
        Ok(slice_read(text.as_bytes(), offset, buf))
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::RoFs)
    }

    fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut locks = self.locks.lock();
        f(&mut locks);
        Ok(())
    }
}

/// Whether `ino` is one of the nodes [`ProcKind::cacheable`] keeps out of
/// the VFS caches: `self`, or a per-process node (every one numbered from
/// `/proc/0`'s up, above all the fixed nodes).
#[cfg(feature = "ktest")]
fn uncacheable_ino(ino: Ino) -> bool {
    ino == ProcKind::SelfLink.ino() || ino >= ProcKind::PidDir(0).ino()
}

/// Every `lookup` procfs answered (the ktest: one per uncached step).
#[cfg(feature = "ktest")]
static ASKED_LOOKUPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Every `resolve_magic_link` procfs answered.
#[cfg(feature = "ktest")]
static ASKED_MAGIC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How often the VFS has asked procfs: `(lookups, magic link resolutions)`.
#[cfg(feature = "ktest")]
fn asked() -> (u64, u64) {
    (ASKED_LOOKUPS.load(Ordering::Relaxed), ASKED_MAGIC.load(Ordering::Relaxed))
}

/// `procfs` inodes in the VFS's `InodeCache`: all of them, and the ones
/// [`uncacheable_ino`] says must never be there.
#[cfg(feature = "ktest")]
fn cached_procfs_inodes() -> (usize, usize) {
    let cached = super::snapshot_cached_inodes();
    let procfs = cached.iter().filter(|i| i.dev_id() == DevId::PROCFS);
    let all = procfs.clone().count();
    (all, procfs.filter(|i| uncacheable_ino(i.ino())).count())
}

/// One short Linux process on a user thread (as `run_dev_tmp_proc_checks`
/// spawns one). It opens fd 3 on `/dev/null`, resolves its `/proc` nodes (a
/// plain lookup, `..` out of `fd`, `self`, the fd's magic link), resolves
/// them again asking procfs exactly once per uncached step, and hands
/// `/proc/<pid>` to the tables' other insert paths, checks that neither
/// cache kept any of them, then closes fd 3 and checks that `fd/3` is gone
/// while `/proc/<pid>` is not. Returns its pid.
#[cfg(feature = "ktest")]
fn short_process_resolving_its_proc_nodes() -> Result<u32, String> {
    use lumen_vfs::{ResolveFlags, VfsCacheOps, resolve_path};

    use crate::linux::process::{self, ConsoleIo};
    use crate::mm::user_vm::AddressSpace;
    use crate::sched;

    let space = Arc::new(AddressSpace::new().map_err(|e| format!("AddressSpace::new: {e}"))?);
    let dev_null = super::resolve_path_from_root("/dev/null").map_err(|e| format!("/dev/null: {e:?}"))?;
    let result: Arc<SleepMutex<Option<Result<u32, String>>>> = Arc::new(SleepMutex::new(None));
    let res_clone = Arc::clone(&result);

    let handle = sched::spawn_user_on(0, "procfs-uncached", Arc::clone(&space), move || {
        let pid = process::register_current(ConsoleIo::Capture, false, Vec::new());
        let checks = (|| -> Result<(), String> {
            let tables = super::tables();
            let name = format!("{pid}");
            let nofollow = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
            let follow = ResolveFlags { follow_final: true, ..nofollow };
            let resolve = |path: &str, flags| resolve_path(tables, None, path.as_bytes(), flags).map(|r| r.inode);
            let fd3 = format!("/proc/{pid}/fd/3");

            process::register_current_vfs_fd(3, dev_null, String::from("/dev/null"))
                .map_err(|e| format!("register fd 3: {e:?}"))?;
            for (path, flags) in [
                (format!("/proc/{pid}"), nofollow),
                (format!("/proc/{pid}/fd"), nofollow),
                (format!("/proc/{pid}/fd/.."), nofollow),
                (fd3.clone(), nofollow),
                (fd3.clone(), follow),
                (format!("/proc/{pid}/exe"), nofollow),
                (format!("/proc/{pid}/maps"), nofollow),
                (format!("/proc/{pid}/mounts"), nofollow),
                (String::from("/proc/self"), nofollow),
                (String::from("/proc/self/fd"), nofollow),
            ] {
                resolve(&path, flags).map_err(|e| format!("resolve {path} while pid {pid} lives: {e:?}"))?;
            }
            // Again: procfs is asked exactly once per uncached step (a step
            // the tables refused as stale, `Err`, would be retried). `self`
            // is followed through `resolve_magic_link` (none) and `readlink`.
            for (path, flags, want) in [
                (format!("/proc/{pid}"), nofollow, (1, 0)),
                (format!("/proc/{pid}/fd"), nofollow, (2, 0)),
                (format!("/proc/{pid}/fd/.."), nofollow, (3, 0)),
                (fd3.clone(), nofollow, (3, 0)),
                (fd3.clone(), follow, (3, 1)),
                (String::from("/proc/self/fd"), nofollow, (3, 1)),
            ] {
                let (lookups, magic) = asked();
                resolve(&path, flags).map_err(|e| format!("resolve {path} again: {e:?}"))?;
                let (lookups_after, magic_after) = asked();
                let got = (lookups_after - lookups, magic_after - magic);
                if got != want {
                    return Err(format!("resolving {path} asked procfs {got:?} (lookups, magic links), not {want:?}"));
                }
            }

            // The tables' other insert paths hand `/proc/<pid>` back uncached.
            let proc_root = resolve("/proc", follow).map_err(|e| format!("/proc: {e:?}"))?;
            let pid_dir = proc_root.lookup(&name).map_err(|e| format!("lookup {pid}: {e:?}"))?;
            let (dentry_gen, inode_gen) = tables.cache_gens();
            let created = tables.publish_created_inode(
                DevId::PROCFS,
                ProcKind::Root.ino(),
                &name,
                Arc::clone(&pid_dir),
                dentry_gen,
                inode_gen,
            );
            let mp = tables.parent_mount(DevId::PROCFS).ok_or_else(|| String::from("/proc's mountpoint"))?;
            let (_, mount_id) =
                tables.mount_at_with_id(mp.dev_id(), mp.ino()).ok_or_else(|| String::from("/proc's mount id"))?;
            let mounted = tables.inode_get_or_insert_if_mounted(mp.dev_id(), mp.ino(), mount_id, Arc::clone(&pid_dir));
            let inserted = tables.inode_get_or_insert(Arc::clone(&pid_dir));
            if !created.is_ok_and(|c| Arc::ptr_eq(&c, &pid_dir))
                || !mounted.is_some_and(|m| Arc::ptr_eq(&m, &pid_dir))
                || !Arc::ptr_eq(&inserted, &pid_dir)
            {
                return Err(format!("/proc/{pid} was unified with a cached inode"));
            }

            if let Some(target) = tables.dentry_get(DevId::PROCFS, ProcKind::Root.ino(), &name) {
                return Err(format!("/proc/{pid} is in the dentry cache: {target:?}"));
            }
            let (_, uncacheable) = cached_procfs_inodes();
            if uncacheable != 0 {
                return Err(format!("{uncacheable} per-process procfs inodes in the inode cache"));
            }

            // fd 3 closes while the process lives: only `fd/3` goes.
            process::unregister_current_vfs_fd(3);
            for flags in [nofollow, follow] {
                let res = resolve(&fd3, flags);
                if !matches!(res, Err(VfsError::NoEnt)) {
                    return Err(format!("/proc/{pid}/fd/3 after fd 3 closed: {res:?}"));
                }
            }
            for path in [format!("/proc/{pid}"), format!("/proc/{pid}/fd")] {
                resolve(&path, nofollow).map_err(|e| format!("resolve {path} after fd 3 closed: {e:?}"))?;
            }
            Ok(())
        })();
        // Always end the process, a failed check included: one left in
        // `ACTIVE_PROCS` stays in `/proc` for good.
        drop(process::take_current());
        if let Some(detached) = sched::detach_user_space()
            && Arc::strong_count(&detached) == 1
        {
            detached.teardown();
        }
        *res_clone.lock() = Some(checks.map(|()| pid));
    })
    .map_err(|e| format!("spawn_user_on: {e:?}"))?;

    handle.join();
    drop(space);
    result.lock().take().ok_or_else(|| String::from("missing thread result"))?
}

crate::ktest!(
    fn procfs_per_process_nodes_are_never_cached() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{DentryCache, ResolveFlags, VfsCacheOps, resolve_path};

        let tables = super::tables();
        let nofollow = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
        let run = || {
            short_process_resolving_its_proc_nodes().map_err(|err| {
                log::error!(target: "vfs", "procfs short process: {err}");
                "a short process's /proc checks failed (its reason is logged above)"
            })
        };
        // The first process caches the fixed names it passes (`/proc`,
        // `/dev/null`); every later one must leave both caches as it found
        // them.
        run()?;
        // Every fixed node is still cached: the root (by the mount), and each
        // file by name and inode.
        kassert!(tables.inode_get(DevId::PROCFS, ProcKind::Root.ino()).is_some());
        for (name, kind) in [
            ("cpuinfo", ProcKind::Cpuinfo),
            ("meminfo", ProcKind::Meminfo),
            ("mounts", ProcKind::Mounts),
            ("version", ProcKind::Version),
        ] {
            let path = format!("/proc/{name}");
            let inode =
                resolve_path(tables, None, path.as_bytes(), nofollow).map_err(|_| "resolve a fixed node")?.inode;
            kassert!(tables.dentry_get(DevId::PROCFS, ProcKind::Root.ino(), name).is_some_and(|t| t.ino == kind.ino()));
            kassert!(tables.inode_get(DevId::PROCFS, kind.ino()).is_some_and(|i| Arc::ptr_eq(&i, &inode)));
        }

        let (inodes_before, _) = cached_procfs_inodes();
        let dentries_before = tables.dentry_count();
        // A full `DentryCache` evicts as it inserts, keeping its size flat
        // whatever goes in: the check below needs room.
        kassert!(dentries_before < DentryCache::DEFAULT_CAPACITY);
        for _ in 0..32 {
            let pid = run()?;
            // Gone with its process: the resolve itself says so, not just
            // the node's `stat`.
            for path in [format!("/proc/{pid}"), format!("/proc/{pid}/fd"), format!("/proc/{pid}/fd/3")] {
                let res = resolve_path(tables, None, path.as_bytes(), nofollow);
                if !matches!(res, Err(VfsError::NoEnt)) {
                    log::error!(target: "vfs", "{path} after its process ended: {:?}", res.map(|r| r.inode));
                    return Err("a /proc/<pid> path resolved after its process ended");
                }
            }
        }
        let (inodes_after, uncacheable) = cached_procfs_inodes();
        kassert_eq!(uncacheable, 0);
        kassert_eq!(inodes_after, inodes_before);
        kassert_eq!(tables.dentry_count(), dentries_before);
        Ok(())
    }
);

crate::ktest!(
    fn procfs_root_listing_keeps_its_inodes_and_agrees_with_lookup() {
        // `readdir` builds its entries from `ProcKind` (#265): `/proc`'s fixed
        // entries keep their numbers, types and order, and each agrees with
        // what looking the name up and `stat`ing it says.
        use crate::kassert_eq;
        let fs = Procfs::new(DevId::makedev(0, 140));
        let root = fs.root_inode();
        let mut entries = Vec::new();
        root.readdir(0, &mut entries, 64).map_err(|_| "readdir /proc")?;
        let fixed: Vec<(u64, InodeType, &str)> =
            entries.iter().take(7).map(|e| (e.ino, e.inode_type, e.name.as_str())).collect();
        kassert_eq!(
            fixed,
            [
                (1, InodeType::Dir, "."),
                (1, InodeType::Dir, ".."),
                (2, InodeType::File, "cpuinfo"),
                (3, InodeType::File, "meminfo"),
                (4, InodeType::File, "mounts"),
                (6, InodeType::Symlink, "self"),
                (5, InodeType::File, "version"),
            ]
        );
        for (ino, itype, name) in fixed {
            let st = root.lookup(name).and_then(|n| n.stat()).map_err(|_| "lookup and stat a fixed entry")?;
            kassert_eq!((st.ino, st.inode_type), (ino, itype));
        }
        Ok(())
    }
);
