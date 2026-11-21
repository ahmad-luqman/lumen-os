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
use crate::sched;
use crate::smp;
use crate::sync::SleepMutex;
use crate::time;

use super::page_cache;

const INO_ROOT: u64 = 1;
const INO_CPUINFO: u64 = 2;
const INO_MEMINFO: u64 = 3;
const INO_MOUNTS: u64 = 4;
const INO_VERSION: u64 = 5;
const INO_SELF: u64 = 6;

const TAG_PID_DIR: u64 = 0;
const TAG_PID_EXE: u64 = 1;
const TAG_PID_MAPS: u64 = 2;
const TAG_PID_FD_DIR: u64 = 3;
const TAG_PID_MOUNTS: u64 = 4;
const TAG_PID_FD_BASE: u64 = 0x1000;

pub(crate) const fn pid_ino(pid: u32, tag: u64) -> u64 {
    (1u64 << 48) | ((pid as u64) << 16) | tag
}

fn parse_canonical_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcKind {
    Root,
    Cpuinfo,
    Meminfo,
    Mounts,
    Version,
    SelfLink,
    PidDir(u32),
    PidExe(u32),
    PidMaps(u32),
    PidFdDir(u32),
    PidMounts(u32),
    PidFdLink(u32, i32),
}

impl ProcKind {
    const fn ino(self) -> u64 {
        match self {
            Self::Root => INO_ROOT,
            Self::Cpuinfo => INO_CPUINFO,
            Self::Meminfo => INO_MEMINFO,
            Self::Mounts => INO_MOUNTS,
            Self::Version => INO_VERSION,
            Self::SelfLink => INO_SELF,
            Self::PidDir(p) => pid_ino(p, TAG_PID_DIR),
            Self::PidExe(p) => pid_ino(p, TAG_PID_EXE),
            Self::PidMaps(p) => pid_ino(p, TAG_PID_MAPS),
            Self::PidFdDir(p) => pid_ino(p, TAG_PID_FD_DIR),
            Self::PidMounts(p) => pid_ino(p, TAG_PID_MOUNTS),
            Self::PidFdLink(p, fd) => pid_ino(p, TAG_PID_FD_BASE + fd as u64),
        }
    }

    const fn inode_type(self) -> InodeType {
        match self {
            Self::Root | Self::PidDir(_) | Self::PidFdDir(_) => InodeType::Dir,
            Self::SelfLink | Self::PidExe(_) | Self::PidFdLink(_, _) => InodeType::Symlink,
            Self::Cpuinfo | Self::Meminfo | Self::Mounts | Self::Version | Self::PidMaps(_) | Self::PidMounts(_) => {
                InodeType::File
            }
        }
    }

    const fn mode(self) -> u16 {
        match self.inode_type() {
            InodeType::Dir => 0o555,
            InodeType::Symlink => 0o777,
            _ => 0o444,
        }
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

    fn pid_exists(pid: u32) -> bool {
        if process::has_pid(pid) {
            return true;
        }
        pid == 1 && process::active_pids().is_empty()
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

    fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
        match self.kind {
            ProcKind::PidExe(pid) => {
                let path = process::process_exe_path(pid)?;
                super::resolve_path_from_root(&path).ok()
            }
            ProcKind::PidFdLink(pid, fd) => process::process_fd_target(pid, fd).and_then(|(inode, _)| inode),
            _ => None,
        }
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
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
                        if fd >= lumen_vfs::MAX_FDS as i32 {
                            return Err(VfsError::NoEnt);
                        }
                        if process::process_fd_target(pid, fd).is_none() {
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
            ProcKind::SelfLink => {
                let pid = process::current_pid().unwrap_or(1);
                Ok(format!("{pid}").into_bytes())
            }
            ProcKind::PidExe(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                let path = process::process_exe_path(pid).unwrap_or_else(|| String::from("/bin/lumen"));
                Ok(path.into_bytes())
            }
            ProcKind::PidFdLink(pid, fd) => {
                let (_, path) = process::process_fd_target(pid, fd).ok_or(VfsError::NoEnt)?;
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
                entries.push((INO_ROOT, InodeType::Dir, String::from(".")));
                entries.push((INO_ROOT, InodeType::Dir, String::from("..")));
                entries.push((INO_CPUINFO, InodeType::File, String::from("cpuinfo")));
                entries.push((INO_MEMINFO, InodeType::File, String::from("meminfo")));
                entries.push((INO_MOUNTS, InodeType::File, String::from("mounts")));
                entries.push((INO_SELF, InodeType::Symlink, String::from("self")));
                entries.push((INO_VERSION, InodeType::File, String::from("version")));
                let pids = process::active_pids();
                if pids.is_empty() {
                    entries.push((pid_ino(1, TAG_PID_DIR), InodeType::Dir, String::from("1")));
                } else {
                    for pid in pids {
                        entries.push((pid_ino(pid, TAG_PID_DIR), InodeType::Dir, format!("{pid}")));
                    }
                }
            }
            ProcKind::PidDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                entries.push((pid_ino(pid, TAG_PID_DIR), InodeType::Dir, String::from(".")));
                entries.push((INO_ROOT, InodeType::Dir, String::from("..")));
                entries.push((pid_ino(pid, TAG_PID_EXE), InodeType::Symlink, String::from("exe")));
                entries.push((pid_ino(pid, TAG_PID_FD_DIR), InodeType::Dir, String::from("fd")));
                entries.push((pid_ino(pid, TAG_PID_MAPS), InodeType::File, String::from("maps")));
                entries.push((pid_ino(pid, TAG_PID_MOUNTS), InodeType::File, String::from("mounts")));
            }
            ProcKind::PidFdDir(pid) => {
                if !Self::pid_exists(pid) {
                    return Err(VfsError::NoEnt);
                }
                entries.push((pid_ino(pid, TAG_PID_FD_DIR), InodeType::Dir, String::from(".")));
                entries.push((pid_ino(pid, TAG_PID_DIR), InodeType::Dir, String::from("..")));
                for fd in process::process_open_fds(pid) {
                    entries.push((pid_ino(pid, TAG_PID_FD_BASE + fd as u64), InodeType::Symlink, format!("{fd}")));
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
            ProcKind::PidMaps(pid) => {
                let space = process::process_address_space(pid).or_else(|| {
                    (pid == 1 && process::active_pids().is_empty()).then(sched::current_address_space).flatten()
                });
                match space {
                    Some(aspace) => aspace.format_maps(),
                    None => return Err(VfsError::NoEnt),
                }
            }
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
