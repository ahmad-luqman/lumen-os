//! Read-only `/proc` virtual filesystem (`procfs`, `dev = 0:4`).
//!
//! All regular virtual files report `st_size == 0` in `stat()` and generate
//! fresh content dynamically on `read_at()`. Magic symlinks (`/proc/<pid>/fd/*`)
//! implement `resolve_magic_link()` while keeping `readlink()` textual.
//! Exe links expand their stored path in the caller's bounded symlink walk;
//! they never start a nested resolver with a fresh depth counter.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::pseudo::fill_readdir;
use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, SetTime, Timespec, VfsError, VfsInode,
    next_lock_id,
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
use log::warn;
use lumen_vfs::procfs::{ProcFd, ProcKind, parse_canonical_u32};

/// A `readdir` entry for `kind`, named `name`.
/// `/proc`'s `readdir` cursor for pid 0: a process's directory is keyed
/// by this plus its pid, past the fixed entries' 0 to 6.
const PROC_PID_CURSOR: u64 = 0x100;

/// `/proc/<pid>/fd`'s `readdir` cursor for fd 0, past `.` and `..`.
const PROC_FD_CURSOR: u64 = 2;

/// A listed entry: its inode number, type and name.
type Listed = (u64, InodeType, Vec<u8>);

fn dir_entry(kind: ProcKind, name: impl Into<Vec<u8>>) -> Listed {
    (kind.ino(), kind.inode_type(), name.into())
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
    format_mounts_of(super::list_mounts())
}

/// `/proc/mounts`' text for `mounts` (the ktests' own tables' too).
pub(crate) fn format_mounts_of(mounts: Vec<lumen_vfs::MountInfo>) -> String {
    let mut out = String::new();
    for m in mounts {
        // Outside the ktests the only ext4 mount is the root, always the first
        // virtio-blk disk `vfs::init` found; `/dev` has no block nodes yet, so
        // the name follows Linux's (a ktest's ext4 mount is labelled the same).
        let source = if m.fs_name == "ext4" { "/dev/vda" } else { m.fs_name };
        // An ext4 volume errored read-only (#349) notes why, as Linux shows the
        // option that made it so.
        let mode = if m.errored {
            "ro,errors=remount-ro"
        } else if m.read_only {
            "ro"
        } else {
            "rw"
        };
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
            // Exe has only a stored path, not a held inode. Let readlink feed
            // that path into the caller's existing MAX_SYMLINK_DEPTH budget.
            // This also bounds cycles through another exe, fd, or ordinary link.
            ProcKind::PidExe(_) => None,
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

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        #[cfg(feature = "ktest")]
        ASKED_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        let root_arc = self.root_weak.upgrade().ok_or(VfsError::NoEnt)?;
        match self.kind {
            ProcKind::Root => match name {
                b"." | b".." => Ok(root_arc as Arc<dyn VfsInode>),
                b"cpuinfo" => Ok(self.spawn(ProcKind::Cpuinfo) as Arc<dyn VfsInode>),
                b"meminfo" => Ok(self.spawn(ProcKind::Meminfo) as Arc<dyn VfsInode>),
                b"mounts" => Ok(self.spawn(ProcKind::Mounts) as Arc<dyn VfsInode>),
                b"version" => Ok(self.spawn(ProcKind::Version) as Arc<dyn VfsInode>),
                b"self" => Ok(self.spawn(ProcKind::SelfLink) as Arc<dyn VfsInode>),
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
                    b"." => Ok(self.spawn(ProcKind::PidDir(pid)) as Arc<dyn VfsInode>),
                    b".." => Ok(root_arc as Arc<dyn VfsInode>),
                    b"exe" => Ok(self.spawn(ProcKind::PidExe(pid)) as Arc<dyn VfsInode>),
                    b"maps" => Ok(self.spawn(ProcKind::PidMaps(pid)) as Arc<dyn VfsInode>),
                    b"fd" => Ok(self.spawn(ProcKind::PidFdDir(pid)) as Arc<dyn VfsInode>),
                    b"mounts" => Ok(self.spawn(ProcKind::PidMounts(pid)) as Arc<dyn VfsInode>),
                    _ => Err(VfsError::NoEnt),
                }
            }
            ProcKind::PidFdDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                match name {
                    b"." => Ok(self.spawn(ProcKind::PidFdDir(pid)) as Arc<dyn VfsInode>),
                    b".." => Ok(self.spawn(ProcKind::PidDir(pid)) as Arc<dyn VfsInode>),
                    other => {
                        let fd = ProcFd::parse(other).ok_or(VfsError::NoEnt)?;
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

    lumen_vfs::fixed_namespace_ops!();

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        match self.kind {
            // A kernel thread (the shell) has no pid: `/proc/self` dangles.
            ProcKind::SelfLink => Ok(format!("{}", process::try_current_pid().ok_or(VfsError::NoEnt)?).into_bytes()),
            // No path recorded (a process not loaded from the VFS): no target,
            // the same answer `resolve_magic_link` gives.
            ProcKind::PidExe(pid) => process::process_exe_path(pid).map(|path| path.to_vec()).ok_or(VfsError::NoEnt),
            ProcKind::PidFdLink(pid, fd) => {
                process::process_fd_target(pid, fd.get()).map(|(_, path)| path).ok_or(VfsError::NoEnt)
            }
            _ => Err(VfsError::Inval),
        }
    }

    /// Each entry's cursor is a key, not its index in today's listing, so
    /// a listing resumed after processes came and went (`/proc`) or fds were
    /// opened and closed (`/proc/<pid>/fd`) lists every entry that stayed
    /// exactly once (`VfsInode::readdir`'s contract), as Linux's procfs:
    /// `/proc`'s fixed entries 0 to 6, then [`PROC_PID_CURSOR`] plus the pid
    /// (`active_pids` is sorted); `/proc/<pid>/fd`'s `.` and `..` 0 and 1,
    /// then 2 plus the fd (`process_open_fds` ascends); a `/proc/<pid>`'s
    /// fixed entries their index. An entry's `next_cursor` is its key plus 1.
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if max_entries == 0 {
            return Ok(());
        }
        // (key, (ino, type, name)), keys ascending.
        let mut entries: Vec<(u64, Listed)> = Vec::new();
        match self.kind {
            ProcKind::Root => {
                let fixed = [
                    dir_entry(ProcKind::Root, "."),
                    dir_entry(ProcKind::Root, ".."),
                    dir_entry(ProcKind::Cpuinfo, "cpuinfo"),
                    dir_entry(ProcKind::Meminfo, "meminfo"),
                    dir_entry(ProcKind::Mounts, "mounts"),
                    dir_entry(ProcKind::SelfLink, "self"),
                    dir_entry(ProcKind::Version, "version"),
                ];
                debug_assert!(fixed.len() as u64 <= PROC_PID_CURSOR);
                entries.extend((0u64..).zip(fixed));
                for pid in process::active_pids() {
                    entries
                        .push((PROC_PID_CURSOR + u64::from(pid), dir_entry(ProcKind::PidDir(pid), format!("{pid}"))));
                }
            }
            ProcKind::PidDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                let fixed = [
                    dir_entry(ProcKind::PidDir(pid), "."),
                    dir_entry(ProcKind::Root, ".."),
                    dir_entry(ProcKind::PidExe(pid), "exe"),
                    dir_entry(ProcKind::PidFdDir(pid), "fd"),
                    dir_entry(ProcKind::PidMaps(pid), "maps"),
                    dir_entry(ProcKind::PidMounts(pid), "mounts"),
                ];
                entries.extend((0u64..).zip(fixed));
            }
            ProcKind::PidFdDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                entries.push((0, dir_entry(ProcKind::PidFdDir(pid), ".")));
                entries.push((1, dir_entry(ProcKind::PidDir(pid), "..")));
                for fd in process::process_open_fds(pid) {
                    // The fd table keeps fds in `0..MAX_FDS`; one outside it
                    // couldn't be looked up either, so it isn't listed.
                    let Ok(proc_fd) = ProcFd::try_from(fd) else {
                        debug_assert!(false, "pid {pid} holds fd {fd}, outside 0..MAX_FDS");
                        warn!(target: "procfs", "pid {pid} holds fd {fd}, outside 0..MAX_FDS: not listed");
                        continue;
                    };
                    // `try_from` accepted it: `0..MAX_FDS`, not negative.
                    let key = PROC_FD_CURSOR + fd as u64;
                    entries.push((key, dir_entry(ProcKind::PidFdLink(pid, proc_fd), format!("{fd}"))));
                }
            }
            _ => return Err(VfsError::NotDir),
        }
        fill_readdir(out, cursor, max_entries, entries);
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
            ProcKind::Cpuinfo => format_cpuinfo().into_bytes(),
            ProcKind::Meminfo => format_meminfo().into_bytes(),
            ProcKind::Mounts | ProcKind::PidMounts(_) => format_mounts().into_bytes(),
            ProcKind::Version => format_version().into_bytes(),
            ProcKind::PidMaps(pid) => process::process_maps(pid).ok_or(VfsError::NoEnt)?,
            _ => return Err(VfsError::Inval),
        };
        Ok(slice_read(&text, offset, buf))
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

    /// Nothing on procfs has an `fsync` (Linux's has none): `EINVAL`.
    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Err(VfsError::Inval)
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

    use crate::linux::process;
    use crate::mm::user_vm::AddressSpace;
    use crate::sched;

    let space = Arc::new(AddressSpace::new().map_err(|e| format!("AddressSpace::new: {e}"))?);
    let stdio = process::Stdio::open().map_err(|e| format!("open the process's stdio: {e:?}"))?;
    let result: Arc<SleepMutex<Option<Result<u32, String>>>> = Arc::new(SleepMutex::new(None));
    let res_clone = Arc::clone(&result);

    let handle = sched::spawn_user_on(0, "procfs-uncached", Arc::clone(&space), move || {
        let registered = process::RegisteredProcess::register(stdio);
        let pid = registered.pid();
        let checks = (|| -> Result<(), String> {
            let tables = super::tables();
            let name = format!("{pid}");
            let nofollow = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
            let follow = ResolveFlags { follow_final: true, ..nofollow };
            let resolve = |path: &str, flags| resolve_path(tables, None, path.as_bytes(), flags).map(|r| r.inode);
            let fd3 = format!("/proc/{pid}/fd/3");

            let null_file = process::open_kernel_file(b"/dev/null", lumen_vfs::OpenFlags::O_RDWR, 0)
                .map_err(|e| format!("open /dev/null: {e:?}"))?;
            process::insert_current_fd(3, null_file).map_err(|e| format!("install fd 3: {e:?}"))?;
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
            let pid_dir = proc_root.lookup(name.as_bytes()).map_err(|e| format!("lookup {pid}: {e:?}"))?;
            let (dentry_gen, inode_gen) = tables.cache_gens();
            let created = tables.publish_created_inode(
                DevId::PROCFS,
                ProcKind::Root.ino(),
                name.as_bytes(),
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

            if let Some(target) = tables.dentry_get(DevId::PROCFS, ProcKind::Root.ino(), name.as_bytes()) {
                return Err(format!("/proc/{pid} is in the dentry cache: {target:?}"));
            }
            let (_, uncacheable) = cached_procfs_inodes();
            if uncacheable != 0 {
                return Err(format!("{uncacheable} per-process procfs inodes in the inode cache"));
            }

            // fd 3 closes while the process lives: only `fd/3` goes.
            process::close_current_fd(3).map_err(|e| format!("close fd 3: {e:?}"))?;
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
        // Ended, a failed check included: its takedown takes this thread off
        // the space and tears it down.
        drop(registered);
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
            kassert!(
                tables
                    .dentry_get(DevId::PROCFS, ProcKind::Root.ino(), name.as_bytes())
                    .is_some_and(|t| t.ino == kind.ino())
            );
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
        let fixed: Vec<(u64, InodeType, &str)> = entries
            .iter()
            .take(7)
            .map(|e| (e.ino, e.inode_type, core::str::from_utf8(&e.name).unwrap_or("?")))
            .collect();
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
            let st =
                root.lookup(name.as_bytes()).and_then(|n| n.stat()).map_err(|_| "lookup and stat a fixed entry")?;
            kassert_eq!((st.ino, st.inode_type), (ino, itype));
        }
        Ok(())
    }
);

crate::ktest!(
    fn procfs_fd_listing_and_targets_follow_the_fd_table() {
        // `/proc/<pid>/fd` lists the process's FD table (#254): the console
        // fds a process starts with, a file installed, a slot closed; each
        // link's target is the open file's path, its inode the file's.
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenFlags, readlink_at, unlink_at};

        const PATH: &[u8] = b"/tmp/procfs-ktest-fd";
        let tables = super::tables();
        process::in_test_process(move |pid| {
            let listed = || -> Result<Vec<String>, &'static str> {
                let dir = super::resolve_path_from_root(&format!("/proc/{pid}/fd")).map_err(|_| "resolve fd dir")?;
                let mut entries = Vec::new();
                dir.readdir(0, &mut entries, 64).map_err(|_| "readdir fd dir")?;
                Ok(entries
                    .into_iter()
                    .filter(|e| e.name != b"." && e.name != b"..")
                    .map(|e| String::from_utf8_lossy(&e.name).into_owned())
                    .collect())
            };
            let link = |fd: i32| readlink_at(tables, None, format!("/proc/{pid}/fd/{fd}").as_bytes());
            kassert_eq!(listed()?, ["0", "1", "2"]);
            kassert_eq!(process::process_open_fds(pid), [0, 1, 2]);
            for fd in 0..3 {
                kassert_eq!(link(fd), Ok(b"/dev/console".to_vec()));
            }
            let console = super::resolve_path_from_root("/dev/console").map_err(|_| "resolve /dev/console")?;
            let fd0 = super::resolve_path_from_root(&format!("/proc/{pid}/fd/0")).map_err(|_| "resolve fd/0")?;
            kassert!(Arc::ptr_eq(&fd0, &console));

            let file = process::open_kernel_file(PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)
                .map_err(|_| "open the file")?;
            let inode = Arc::clone(file.inode());
            kassert_eq!(process::install_current_fd(file, false), Ok(3));
            process::close_current_fd(1).map_err(|_| "close fd 1")?;
            kassert_eq!(listed()?, ["0", "2", "3"]);
            kassert_eq!(link(1), Err(VfsError::NoEnt));
            kassert_eq!(link(3), Ok(PATH.to_vec()));
            let fd3 = super::resolve_path_from_root(&format!("/proc/{pid}/fd/3")).map_err(|_| "resolve fd/3")?;
            kassert!(Arc::ptr_eq(&fd3, &inode));
            kassert!(unlink_at(tables, None, PATH, false).is_ok());
            kassert_eq!(link(3), Ok(b"/tmp/procfs-ktest-fd (deleted)".to_vec()));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn procfs_exe_cycles_share_the_symlink_limit() {
        use crate::kassert_eq;
        process::in_test_process(|pid| {
            let target = format!("/proc/{pid}/exe");
            process::set_exe_path(Some(Arc::from(target.as_bytes())));
            kassert_eq!(super::resolve_path_from_root("/proc/self/exe").err(), Some(VfsError::Loop));
            // An ordinary symlink in the recorded identity must share the budget too.
            let tmp = super::resolve_path_from_root("/tmp").map_err(|_| "tmp")?;
            tmp.symlink(b"exe-cycle", b"/proc/self/exe").map_err(|_| "symlink")?;
            process::set_exe_path(Some(Arc::from(b"/tmp/exe-cycle".as_slice())));
            let error = super::resolve_path_from_root("/proc/self/exe").err();
            lumen_vfs::unlink_at(super::tables(), None, b"/tmp/exe-cycle", false).map_err(|_| "unlink")?;
            kassert_eq!(error, Some(VfsError::Loop));
            // A normal target still resolves after the cycle was refused.
            process::set_exe_path(Some(Arc::from(b"/dev/null".as_slice())));
            let inode = super::resolve_path_from_root("/proc/self/exe").map_err(|_| "noncyclic exe")?;
            kassert_eq!(inode.inode_type(), InodeType::CharDevice);
            Ok(())
        })?
    }
);
