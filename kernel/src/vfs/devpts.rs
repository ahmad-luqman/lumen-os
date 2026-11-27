//! The live PTY slave namespace, mounted at `/dev/pts` (device 0:5).
//!
//! Registry entries retain slaves until master close. Lookups bypass the VFS
//! caches, and old inode references never resolve to a newly allocated pair.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_vfs::{
    AccessMode, AdvisoryLocks, DevId, DirEntry, FileSystem, InodeType, Metadata, OpenFlags, PollGuard, PollMask,
    PollWaker, Pollable, SetTime, VfsError, VfsInode, next_lock_id,
};

use crate::linux::errno;
use crate::linux::process::{self, KernelFile};
use crate::sync::{SleepMutex, SpinLock};
use crate::tty::Tty;

pub const PTY_MAX: usize = 256;
static SLAVES: SpinLock<[Option<Arc<PtyInode>>; PTY_MAX]> = SpinLock::new([const { None }; PTY_MAX]);
static NEXT_INO: AtomicU64 = AtomicU64::new(2);

pub struct Devpts {
    root: Arc<PtyInode>,
}

impl Devpts {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { root: PtyInode::new(None, false, 1) })
    }
}

impl FileSystem for Devpts {
    fn dev_id(&self) -> DevId {
        DevId::makedev(0, 5)
    }
    fn name(&self) -> &'static str {
        "devpts"
    }
    fn root_inode(&self) -> Arc<dyn VfsInode> {
        self.root.clone()
    }
    fn read_only(&self) -> bool {
        false
    }
    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }
    fn statfs(&self) -> Result<lumen_vfs::fs::StatFs, VfsError> {
        Ok(lumen_vfs::fs::StatFs { magic: 0x1cd1, ..Default::default() })
    }
}

/// An endpoint's single open-description lifetime (dup and fork share it).
pub struct TtyEnd {
    pub(crate) tty: Arc<Tty>,
    pub(crate) master: bool,
    live: AtomicBool,
}

impl TtyEnd {
    fn new(tty: Arc<Tty>, master: bool) -> Self {
        Self { tty, master, live: AtomicBool::new(true) }
    }
    /// Release after the last descriptor and in-flight I/O reference, in thread context.
    pub(crate) fn release(&self) {
        if self.live.swap(false, Ordering::AcqRel) {
            self.tty.close(self.master);
        }
    }
    pub(crate) fn read(&self, addr: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        self.tty.read(self.master, addr, count, nonblock)
    }
    pub(crate) fn write(&self, addr: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        self.tty.write(self.master, addr, count, nonblock)
    }
    pub(crate) fn ioctl(&self, cmd: u32, arg: u64) -> Result<u64, u32> {
        self.tty.ioctl(self.master, cmd, arg)
    }
}

impl Drop for TtyEnd {
    fn drop(&mut self) {
        self.release();
    }
}

/// Perform terminal-specific open work after ordinary VFS and flag checks.
pub(crate) fn open(file: KernelFile, flags: OpenFlags) -> Result<KernelFile, u32> {
    if flags.is_path() {
        return Ok(file);
    }
    let kind = super::devfs::terminal_open(file.inode().as_ref());
    let (inode, tty, master): (Arc<dyn VfsInode>, Arc<Tty>, bool) = match kind {
        Some(super::devfs::TerminalOpen::Ptmx) => {
            let (master, tty) = allocate_master()?;
            (master, tty, true)
        }
        Some(super::devfs::TerminalOpen::Alias) => {
            let tty = process::current_process().controlling_tty().ok_or(errno::ENXIO)?;
            if tty.is_console() {
                return console_file(file, tty);
            }
            let inode = SLAVES.lock()[tty.number as usize]
                .as_ref()
                .filter(|node| node.tty.as_ref().is_some_and(|held| Arc::ptr_eq(held, &tty)))
                .cloned()
                .ok_or(errno::ENXIO)?;
            (inode, tty, false)
        }
        Some(super::devfs::TerminalOpen::Console) => {
            let Some(tty) = process::try_current_io().and_then(crate::tty::console) else {
                return Ok(file);
            };
            return console_file(file, tty);
        }
        None => {
            let Some(node) = file.inode().as_any().and_then(|any| any.downcast_ref::<PtyInode>()) else {
                return Ok(file);
            };
            let Some(tty) = node.tty.as_ref() else { return Ok(file) };
            if node.master {
                // A reopened ptmx device runs the allocator again, as Linux's
                // ptmx_open does; only dup/fork share the existing master.
                let (master, tty) = allocate_master()?;
                (master, tty, true)
            } else {
                (Arc::clone(file.inode()), Arc::clone(tty), false)
            }
        }
    };
    if !master {
        tty.open_slave(flags.bits() & crate::linux::general::O_NOCTTY == 0 && file.access_mode().can_read())?;
    }
    let end = TtyEnd::new(tty, master);
    KernelFile::new_tty_end(inode, Arc::clone(&file.path), flags, end).map_err(|error| error.to_errno())
}

/// Bind a console description to its terminal without acquiring a controlling tty.
pub(crate) fn console_file(file: KernelFile, tty: Arc<Tty>) -> Result<KernelFile, u32> {
    tty.open_slave(false)?;
    let flags = OpenFlags::from_bits(file.get_fl());
    let end = TtyEnd::new(tty, false);
    KernelFile::new_tty_end(Arc::clone(file.inode()), Arc::clone(&file.path), flags, end)
        .map_err(|error| error.to_errno())
}

fn allocate_master() -> Result<(Arc<PtyInode>, Arc<Tty>), u32> {
    let mut slots = SLAVES.lock();
    let number = slots.iter().position(Option::is_none).ok_or(errno::ENOSPC)?;
    let tty = Tty::new(number as u32)?;
    let ino = NEXT_INO.fetch_add(2, Ordering::Relaxed);
    let slave = PtyInode::new(Some(Arc::clone(&tty)), false, ino);
    let master = PtyInode::new(Some(Arc::clone(&tty)), true, ino + 1);
    slots[number] = Some(slave);
    Ok((master, tty))
}

pub(crate) fn remove(tty: &Tty) {
    let removed = {
        let mut slots = SLAVES.lock();
        let slot = &mut slots[tty.number as usize];
        if slot.as_ref().is_some_and(|node| node.tty.as_ref().is_some_and(|held| core::ptr::eq(held.as_ref(), tty))) {
            slot.take()
        } else {
            None
        }
    };
    drop(removed);
}

struct PtyInode {
    tty: Option<Arc<Tty>>,
    master: bool,
    ino: u64,
    lock_id: u64,
    opens: AtomicU32,
    meta: SleepMutex<Metadata>,
    locks: SleepMutex<AdvisoryLocks>,
    myself: Weak<Self>,
}

impl PtyInode {
    fn new(tty: Option<Arc<Tty>>, master: bool, ino: u64) -> Arc<Self> {
        let dir = tty.is_none();
        let now = crate::time::realtime_timespec();
        let rdev = tty.as_ref().map_or(DevId::makedev(0, 0), |tty| {
            if master { DevId::makedev(5, 2) } else { DevId::makedev(136, tty.number) }
        });
        Arc::new_cyclic(|myself| Self {
            tty,
            master,
            ino,
            lock_id: next_lock_id(),
            opens: AtomicU32::new(0),
            myself: myself.clone(),
            locks: SleepMutex::new(AdvisoryLocks::new()),
            meta: SleepMutex::new(Metadata {
                dev: DevId::makedev(0, 5),
                ino,
                inode_type: if dir { InodeType::Dir } else { InodeType::CharDevice },
                mode: if dir {
                    0o755
                } else if master {
                    0o666
                } else {
                    0o620
                },
                nlink: if dir { 2 } else { 1 },
                uid: 0,
                gid: 0,
                rdev,
                size: 0,
                blksize: 4096,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
            }),
        })
    }
}

impl VfsInode for PtyInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }
    fn dev_id(&self) -> DevId {
        DevId::makedev(0, 5)
    }
    fn ino(&self) -> u64 {
        self.ino
    }
    fn inode_type(&self) -> InodeType {
        if self.tty.is_none() { InodeType::Dir } else { InodeType::CharDevice }
    }
    fn open_count(&self) -> u32 {
        self.opens.load(Ordering::Acquire)
    }
    fn inc_open_count(&self) {
        self.opens.fetch_add(1, Ordering::AcqRel);
    }
    fn dec_open_count(&self) -> u32 {
        self.opens.fetch_sub(1, Ordering::AcqRel) - 1
    }
    fn cacheable(&self) -> bool {
        false
    }
    fn is_tty(&self) -> bool {
        self.tty.is_some()
    }
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
    fn stat(&self) -> Result<Metadata, VfsError> {
        Ok(*self.meta.lock())
    }
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.tty.is_some() {
            return Err(VfsError::NotDir);
        }
        if name == b"." || name == b".." {
            return self.myself.upgrade().map(|node| node as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        let number = lumen_vfs::procfs::parse_canonical_u32(name)
            .filter(|&number| number < PTY_MAX as u32)
            .ok_or(VfsError::NoEnt)? as usize;
        SLAVES.lock()[number].as_ref().map(|node| Arc::clone(node) as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }
    lumen_vfs::fixed_namespace_ops!();
    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Inval)
    }
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.tty.is_some() {
            return Err(VfsError::NotDir);
        }
        let mut entries = alloc::vec![
            (0, (1, InodeType::Dir, alloc::string::String::from("."))),
            (1, (1, InodeType::Dir, alloc::string::String::from("..")))
        ];
        {
            let slots = SLAVES.lock();
            for (number, slot) in slots.iter().enumerate() {
                if let Some(node) = slot {
                    entries.push((number as u64 + 2, (node.ino, InodeType::CharDevice, alloc::format!("{number}"))));
                }
            }
        }
        lumen_vfs::pseudo::fill_readdir(
            out,
            cursor,
            max_entries,
            entries.iter().map(|(key, (ino, kind, name))| (*key, (*ino, *kind, name.as_str()))),
        );
        Ok(())
    }
    fn truncate(&self, _: u64) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }
    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        meta.mode = mode & 0o7777;
        meta.ctime = crate::time::realtime_timespec();
        Ok(())
    }
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        if let Some(uid) = uid {
            meta.uid = uid;
        }
        if let Some(gid) = gid {
            meta.gid = gid;
        }
        meta.mode = lumen_vfs::mode_after_chown(meta.inode_type, meta.mode);
        meta.ctime = crate::time::realtime_timespec();
        Ok(())
    }
    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        let now = crate::time::realtime_timespec();
        meta.atime = atime.resolve(meta.atime, now);
        meta.mtime = mtime.resolve(meta.mtime, now);
        meta.ctime = now;
        Ok(())
    }
    fn read_at(&self, _: u64, _: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::Io)
    }
    fn write_at(&self, _: u64, _: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Io)
    }
    fn read_page(&self, _: u64, _: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }
    fn write_page(&self, _: u64, _: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }
    fn sync(&self, _: bool) -> Result<(), VfsError> {
        Ok(())
    }
    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        f(&mut self.locks.lock());
        Ok(())
    }
    fn pollable(&self) -> Option<&dyn Pollable> {
        self.tty.as_ref().map(|_| self as &dyn Pollable)
    }
}

impl Pollable for PtyInode {
    fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        self.tty.as_ref().expect("terminal inode").poll(self.master, mode, waker)
    }
}

#[cfg(feature = "ktest")]
fn test_open(path: &[u8], flags: u32) -> Result<KernelFile, u32> {
    let file = process::open_kernel_file(path, flags, 0).map_err(|e| e.to_errno())?;
    open(file, OpenFlags::from_bits(flags))
}

crate::ktest!(
    fn devpts_numbers_and_end_lifetimes() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let flags = OpenFlags::O_RDWR | crate::linux::general::O_NOCTTY;
            let mut masters = Vec::new();
            for number in 0..PTY_MAX {
                let master = test_open(b"/dev/ptmx", flags).map_err(|_| "allocate master")?;
                kassert_eq!(master.tty().ok_or("master endpoint")?.tty.number, number as u32);
                masters.push(Some(master));
            }
            kassert_eq!(test_open(b"/dev/ptmx", flags).err(), Some(errno::ENOSPC));
            let root = Devpts::new().root_inode();
            for name in [b"".as_slice(), b"00", b"037", b"+37", b"-1", b"256", b"4294967296", b"\xff"] {
                kassert_eq!(root.lookup(name).err(), Some(VfsError::NoEnt));
            }
            kassert!(root.lookup(b"0").is_ok());
            kassert!(root.lookup(b"255").is_ok());
            let old = root.lookup(b"37").map_err(|_| "old slave")?;
            let old_ino = old.ino();
            drop(masters[37].take());
            kassert_eq!(root.lookup(b"37").err(), Some(VfsError::NoEnt));
            let replacement = test_open(b"/dev/ptmx", flags).map_err(|_| "reuse number")?;
            kassert_eq!(replacement.tty().ok_or("replacement endpoint")?.tty.number, 37);
            kassert!(root.lookup(b"37").map_err(|_| "new slave")?.ino() != old_ino);
            drop(replacement);
            drop(masters);

            let master = test_open(b"/dev/ptmx", flags).map_err(|_| "master")?;
            let reopened = KernelFile::new_unchecked_flags(
                master.inode().clone(),
                master.path.clone(),
                OpenFlags::from_bits(flags),
            )
            .map_err(|_| "reopen inode")?;
            let reopened = open(reopened, OpenFlags::from_bits(flags)).map_err(|_| "reopen master")?;
            kassert_eq!(reopened.tty().ok_or("reopened endpoint")?.tty.number, 1);
            drop(reopened);
            let base = process::KTEST_USER_VA;
            crate::user::copy_to_user(base, &0i32.to_ne_bytes()).map_err(|_| "unlock arg")?;
            master
                .tty()
                .ok_or("master endpoint")?
                .ioctl(crate::linux::ioctl::TIOCSPTLCK, base)
                .map_err(|_| "unlock")?;
            let slave = test_open(b"/dev/pts/0", flags).map_err(|_| "slave")?;
            let tty = slave.tty().ok_or("slave endpoint")?.tty.clone();
            process::insert_current_fd(3, master).map_err(|_| "install master")?;
            process::insert_current_fd(4, slave).map_err(|_| "install slave")?;
            let observer = process::current_file(3).ok_or("observer")?;
            let active_io = process::current_io_file(3).ok_or("io reference")?;
            process::close_current_fd(3).map_err(|_| "close master fd")?;
            kassert!(!tty.poll(false, AccessMode::ReadOnly, None).0.intersects(PollMask::HUP));
            kassert!(root.lookup(b"0").is_ok());
            drop(active_io);
            // A procfs-style Arc observer must not defer the last close.
            kassert!(tty.poll(false, AccessMode::ReadOnly, None).0.intersects(PollMask::HUP));
            kassert_eq!(root.lookup(b"0").err(), Some(VfsError::NoEnt));
            drop(observer);
            process::close_current_fd(4).map_err(|_| "close slave")?;
            Ok(())
        })?
    }
);
