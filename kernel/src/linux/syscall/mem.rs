//! Memory: `mmap` (anonymous, M6; of a file descriptor, #255), `mprotect`,
//! `munmap`, `brk` (M6), and `msync` (#255).
//!
//! **`mmap` of a descriptor** checks as Linux's `ksys_mmap_pgoff` and
//! `do_mmap` do, in their order: the offset page-aligned (`EINVAL`), the
//! descriptor open and not `O_PATH` (`EBADF`), no `MAP_HUGETLB` (`EINVAL`:
//! no file is on hugetlbfs), a non-zero length (`EINVAL`) whose page
//! rounding fits (`ENOMEM`) and whose end in the file is at most
//! `i64::MAX`, the largest `loff_t` (`EOVERFLOW`, Linux's `file_mmap_ok`
//! for a regular file), then the type in `flags & MAP_TYPE`:
//! - `MAP_SHARED`: flags Linux doesn't know are ignored;
//!   `MAP_SHARED_VALIDATE` refuses them (`EOPNOTSUPP`), and `MAP_SYNC`
//!   with them (no file here is on DAX). Either way `PROT_WRITE` needs the
//!   descriptor open for writing (`EACCES`), which is also what lets a
//!   later `mprotect` add it (`may_write`, `VM_MAYWRITE`);
//! - `MAP_PRIVATE`: copy-on-write, writable whatever the descriptor's mode;
//! - anything else: `EINVAL`.
//!
//! Then the descriptor must be open for reading (`EACCES`), and the file
//! must have pages to map (`ENODEV`): a regular `ext4` or `tmpfs` file, or
//! `/dev/zero` (any character device numbered 1:5, as Linux's `mmap_zero`
//! is that device's), which maps anonymous memory as `MAP_ANONYMOUS` would,
//! shared or private as asked. Directories, the console, `/dev/null` and
//! the rest are `ENODEV`. The advisory flags (`MAP_DENYWRITE`,
//! `MAP_EXECUTABLE`, `MAP_NORESERVE`, `MAP_POPULATE`, `MAP_NONBLOCK`,
//! `MAP_STACK`, `MAP_LOCKED`, `MAP_GROWSDOWN`, `MAP_UNINITIALIZED`,
//! `MAP_32BIT`, `MAP_ABOVE4G`) are taken and ignored. Placement
//! (`MAP_FIXED`, `MAP_FIXED_NOREPLACE`, a hint) is the anonymous path's
//! (`user_vm::map_place`). The mapping holds its own open reference on the
//! file, so it outlives the descriptor's `close` and the name's `unlink`.
//!
//! Not as Linux: a shared `/dev/zero` mapping made through a read-only
//! descriptor can later be made writable with `mprotect` (Linux refuses,
//! `EACCES`): anonymous VMAs carry no `may_write`.

use alloc::sync::Arc;

use lumen_vfs::{AccessMode, DevId, InodeType, OpenGuard};

use super::at::{Fd, reg};
use super::neg_errno;
use crate::linux::{errno, general};
use crate::mm::user_vm::{MappedFile, Sharing, map_place, prot_from_linux};
use crate::sched;

const PAGE_SIZE: u64 = 4096;

/// `/dev/zero`'s device number (Linux's `mem` major, minor 5).
const DEV_ZERO: DevId = DevId::makedev(1, 5);

/// The flags `MAP_SHARED_VALIDATE` takes: Linux's `LEGACY_MAP_MASK`, and
/// `MAP_FIXED_NOREPLACE` (placement this kernel supports). `MAP_SYNC` is
/// not among them: Linux takes it only for a file on DAX.
const VALIDATED_FLAGS: u32 = general::MAP_SHARED
    | general::MAP_PRIVATE
    | general::MAP_FIXED
    | general::MAP_ANONYMOUS
    | general::MAP_DENYWRITE
    | general::MAP_EXECUTABLE
    | general::MAP_UNINITIALIZED
    | general::MAP_GROWSDOWN
    | general::MAP_LOCKED
    | general::MAP_NORESERVE
    | general::MAP_POPULATE
    | general::MAP_NONBLOCK
    | general::MAP_STACK
    | general::MAP_HUGETLB
    | general::MAP_32BIT
    | general::MAP_ABOVE4G
    | general::MAP_HUGE_2MB
    | general::MAP_HUGE_1GB
    | general::MAP_FIXED_NOREPLACE;

/// `mmap(addr, len, prot, flags, fd, off)`: anonymous with `MAP_ANONYMOUS`
/// (`fd` and `off` ignored, as in M6), else of descriptor `fd` from byte
/// `off` (the module's notes).
pub(super) fn sys_mmap(addr: u64, len: u64, prot_raw: u64, flags: u64, fd: u64, off: u64) -> u64 {
    let fd: Fd = reg(fd);
    let Some(prot) = prot_from_linux(prot_raw) else {
        return neg_errno(errno::EINVAL);
    };
    if flags & u64::from(general::MAP_ANONYMOUS) == 0 {
        return match mmap_fd(addr, len, prot, flags, fd, off) {
            Ok(va) => va,
            Err(e) => neg_errno(e),
        };
    }
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::ENOMEM);
    };
    match space.mmap_anon(addr, len, prot, flags) {
        Ok(va) => va,
        Err(e) => neg_errno(e.to_errno()),
    }
}

/// `mmap` of descriptor `fd`: the mapping's address, or the errno.
fn mmap_fd(addr: u64, len: u64, prot: lumen_vm::Prot, flags: u64, fd: Fd, off: u64) -> Result<u64, u32> {
    if !off.is_multiple_of(PAGE_SIZE) {
        return Err(errno::EINVAL);
    }
    let file = fd.file()?;
    if file.is_path() {
        return Err(errno::EBADF);
    }
    if flags & u64::from(general::MAP_HUGETLB) != 0 || len == 0 {
        return Err(errno::EINVAL);
    }
    let aligned_len = lumen_vm::align_up_page(len).ok_or(errno::ENOMEM)?;
    if off.checked_add(aligned_len).is_none_or(|end| end > i64::MAX as u64) {
        return Err(errno::EOVERFLOW);
    }
    let mode = file.access_mode();
    let sharing = match (flags & u64::from(general::MAP_TYPE)) as u32 {
        general::MAP_SHARED | general::MAP_SHARED_VALIDATE => {
            let validate = flags & u64::from(general::MAP_TYPE) == u64::from(general::MAP_SHARED_VALIDATE);
            if validate && flags & !u64::from(VALIDATED_FLAGS) != 0 {
                return Err(errno::EOPNOTSUPP);
            }
            if prot.write() && mode == AccessMode::ReadOnly {
                return Err(errno::EACCES);
            }
            Sharing::Shared { may_write: mode == AccessMode::ReadWrite }
        }
        general::MAP_PRIVATE => Sharing::Private,
        _ => return Err(errno::EINVAL),
    };
    if mode == AccessMode::WriteOnly {
        return Err(errno::EACCES);
    }
    let space = sched::current_address_space().ok_or(errno::ENOMEM)?;
    let inode = &file.inode;
    if inode.inode_type() == InodeType::CharDevice && inode.stat().is_ok_and(|m| m.rdev == DEV_ZERO) {
        // `/dev/zero`: anonymous memory, placed and shared as asked.
        let share = match sharing {
            Sharing::Private => general::MAP_PRIVATE,
            Sharing::Shared { .. } => general::MAP_SHARED,
        };
        let keep = u64::from(general::MAP_FIXED | general::MAP_FIXED_NOREPLACE);
        let anon = (flags & keep) | u64::from(general::MAP_ANONYMOUS | share);
        return space.mmap_anon(addr, len, prot, anon).map_err(|e| e.to_errno());
    }
    // A clone of the descriptor's reference: never on a freed inode.
    let mapped = MappedFile { guard: OpenGuard::new_unchecked(Arc::clone(inode)), path: Arc::clone(&file.path) };
    space.mmap_file(map_place(addr, flags), len, prot, &mapped, off, sharing).map_err(|e| e.to_errno())
}

pub(super) fn sys_mprotect(addr: u64, len: u64, prot_raw: u64) -> u64 {
    let Some(prot) = prot_from_linux(prot_raw) else {
        return neg_errno(errno::EINVAL);
    };
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::ENOMEM);
    };
    match space.mprotect(addr, len, prot) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_mprotect_errno()),
    }
}

pub(super) fn sys_munmap(addr: u64, len: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::EINVAL);
    };
    match space.munmap(addr, len) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

pub(super) fn sys_brk(addr: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return 0;
    };
    space.brk(addr)
}

/// `msync(addr, length, flags)` (26), as Linux's:
/// - `EINVAL` for a flag other than `MS_ASYNC`, `MS_INVALIDATE` and
///   `MS_SYNC`, `addr` not page-aligned, or `MS_ASYNC` with `MS_SYNC`;
///   `ENOMEM` if `addr + length` rounded up to a page overflows; 0 for an
///   empty range, or a `length` within a page of 2^64 (Linux's rounding
///   wraps it to 0);
///   empty range (no mapping needed).
/// - `MS_SYNC` writes back every file a `MAP_SHARED` VMA in the range maps
///   and waits (`VfsInode::sync` with `data_only`, Linux's
///   `vfs_fsync_range(.., 1)`, over the whole file rather than the range;
///   its error, `EIO`, returned). The files are found under the address
///   space's lock and written back after it is released
///   (`AddressSpace::msync_files`). A writable shared page is dirty for as
///   long as it is mapped writable, so stores made through the mapping
///   reach the disk without the PTEs' dirty bits being read.
/// - `MS_ASYNC` writes nothing: Linux's has started no I/O since 2.6.19
///   either; the pages are already dirty in the page cache, which a
///   writeback (`fsync`, `sync`, reclaim) writes. `MS_INVALIDATE` is
///   accepted and does nothing: a `MAP_SHARED` mapping is the page cache's
///   own pages, so nothing is stale.
/// - Private and anonymous ranges have nothing to write.
/// - Part of the range mapped by no VMA is `ENOMEM`, after the mapped
///   parts are written back (with `MS_SYNC`).
pub(super) fn sys_msync(addr: u64, length: u64, flags: i32) -> u64 {
    match msync(addr, length, flags) {
        Ok(()) => 0,
        Err(e) => neg_errno(e),
    }
}

fn msync(addr: u64, length: u64, flags: i32) -> Result<(), u32> {
    let known = (general::MS_ASYNC | general::MS_INVALIDATE | general::MS_SYNC) as i32;
    let both = (general::MS_ASYNC | general::MS_SYNC) as i32;
    if flags & !known != 0 || !addr.is_multiple_of(PAGE_SIZE) || flags & both == both {
        return Err(errno::EINVAL);
    }
    // Linux's `(len + ~PAGE_MASK) & PAGE_MASK`, wrapping.
    let len = length.wrapping_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let end = addr.checked_add(len).ok_or(errno::ENOMEM)?;
    if end == addr {
        return Ok(());
    }
    let space = sched::current_address_space().ok_or(errno::ENOMEM)?;
    let found = space.msync_files(addr, end);
    if flags & general::MS_SYNC as i32 != 0 {
        for file in &found.shared {
            file.inode().sync(true).map_err(|e| e.to_errno())?;
        }
    }
    if found.hole { Err(errno::ENOMEM) } else { Ok(()) }
}

// The ktests: `mmap` of descriptors and `msync`, run as a process
// (`in_test_process`), on `/tmp` (tmpfs, device 0:2) and on a fresh ext4
// volume (`vfs::page_cache::ktest_files`, not mounted: its files get a
// descriptor through `OpenFile::new_unchecked_flags`).

#[cfg(feature = "ktest")]
const RW: u64 = (general::PROT_READ | general::PROT_WRITE) as u64;
#[cfg(feature = "ktest")]
const SHARED: u64 = general::MAP_SHARED as u64;
#[cfg(feature = "ktest")]
const PRIVATE: u64 = general::MAP_PRIVATE as u64;

/// `mmap(0, len, prot, flags, fd, off)`'s return as an address, or why not.
#[cfg(feature = "ktest")]
fn ktest_map(len: u64, prot: u64, flags: u64, fd: Fd, off: u64) -> Result<u64, &'static str> {
    let va = sys_mmap(0, len, prot, flags, fd.raw() as u32 as u64, off);
    if va > u64::MAX - 4095 {
        crate::ktest::diag(format_args!("mmap gave -{}", va.wrapping_neg()));
        return Err("mmap failed");
    }
    Ok(va)
}

/// Open `path` (from `/`) with `flags` (creating it `0o644`) as a
/// descriptor of the current process.
#[cfg(feature = "ktest")]
fn ktest_open(path: &[u8], flags: u32) -> Result<Fd, &'static str> {
    let user = super::fd::ktest_user_path(3, path)?;
    let fd = super::fd::sys_open(user, lumen_vfs::OpenFlags::from_bits(flags), reg(0o644));
    if fd < lumen_vfs::MAX_FDS as u64 { Ok(reg(fd)) } else { Err("open failed") }
}

/// A descriptor of the current process on `inode` (opened by `path`) with
/// `flags`: how the ktests reach an ext4 volume no mount holds.
#[cfg(feature = "ktest")]
fn ktest_fd_on(inode: &Arc<dyn lumen_vfs::VfsInode>, path: &str, flags: u32) -> Result<Fd, &'static str> {
    let file = lumen_vfs::OpenFile::new_unchecked_flags(
        Arc::clone(inode),
        Arc::from(path),
        lumen_vfs::OpenFlags::from_bits(flags),
    )
    .map_err(|_| "open the inode")?;
    let fd = crate::linux::process::install_current_fd(file, false).map_err(|_| "install the fd")?;
    Ok(reg(fd as u32 as u64))
}

/// `len` bytes of `inode` from `off`, through the VFS's read path.
#[cfg(feature = "ktest")]
fn ktest_read(inode: &Arc<dyn lumen_vfs::VfsInode>, off: u64, len: usize) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let mut buf = alloc::vec![0u8; len];
    let n = inode.read_at(off, &mut buf).map_err(|_| "read_at")?;
    buf.truncate(n);
    Ok(buf)
}

/// A file named `name` in `dir` holding `data`.
#[cfg(feature = "ktest")]
fn ktest_file(
    dir: &Arc<dyn lumen_vfs::VfsInode>,
    name: &str,
    data: &[u8],
) -> Result<Arc<dyn lumen_vfs::VfsInode>, &'static str> {
    let f = dir.create(name, 0o644).map_err(|_| "create")?;
    if f.write_at(0, data).map_err(|_| "write_at")? != data.len() {
        return Err("short write");
    }
    Ok(f)
}

/// Two pages of a pattern with no zero byte.
#[cfg(feature = "ktest")]
fn ktest_pattern(seed: u8) -> alloc::vec::Vec<u8> {
    (0..2 * PAGE_SIZE as usize).map(|i| ((i % 251) as u8).wrapping_add(seed) | 1).collect()
}

/// The current process's `/proc/self/maps`, read through the VFS.
#[cfg(feature = "ktest")]
fn ktest_maps() -> Result<alloc::string::String, &'static str> {
    let maps = crate::vfs::resolve_path_from_root("/proc/self/maps").map_err(|_| "resolve maps")?;
    let bytes = ktest_read(&maps, 0, 64 * 1024)?;
    alloc::string::String::from_utf8(bytes).map_err(|_| "maps not UTF-8")
}

/// The line of `maps` for the mapping at `va`.
#[cfg(feature = "ktest")]
fn ktest_line(maps: &str, va: u64) -> Result<&str, &'static str> {
    let start = alloc::format!("{va:08x}-");
    maps.lines().find(|l| l.starts_with(&start)).ok_or("no maps line at the mapping")
}

crate::ktest!(
    fn mem_mmap_shared_fd_stores_reach_the_file_through_msync() {
        // `MAP_SHARED` of a descriptor open for reading and writing: stores
        // through the mapping are what the VFS's read path reads, and
        // `msync(MS_SYNC)` writes them to the disk: a fresh read-only mount
        // of the ext4 volume reads them, where after `MS_ASYNC` it still
        // read the old bytes. The third page is a hole (no block on the
        // disk until the writeback allocates one), and the range synced
        // starts a page below the mapping: `ENOMEM` for that page, after
        // the mapping is written back.
        use crate::kassert_eq;
        use crate::user::copy_to_user;
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        const P: u64 = PAGE_SIZE;
        let vol = Arc::new(Ext4Volume::new(180)?);
        let mut original = ktest_pattern(3);
        let f = ktest_file(&vol.root, "shared.bin", &original)?;
        f.truncate(3 * P).map_err(|_| "truncate")?;
        f.sync(false).map_err(|_| "sync")?;
        original.resize(3 * P as usize, 0);
        let mut expected = original.clone();
        expected[10..24].copy_from_slice(b"through-memory");
        expected[P as usize + 7..P as usize + 18].copy_from_slice(b"second-page");
        expected[2 * P as usize + 1..2 * P as usize + 12].copy_from_slice(b"in-the-hole");
        let (disk, volume) = (Arc::clone(&f), Arc::clone(&vol));
        crate::linux::process::in_test_process(move |_| {
            let on_disk = |minor: u32| -> Result<alloc::vec::Vec<u8>, &'static str> {
                let ro = volume.remount_ro(minor)?.lookup("shared.bin").map_err(|_| "ro lookup")?;
                ktest_read(&ro, 0, 3 * P as usize)
            };
            let fd = ktest_fd_on(&disk, "/ext4/shared.bin", lumen_vfs::OpenFlags::O_RDWR)?;
            let va = ktest_map(3 * P, RW, SHARED, fd, 0)?;
            copy_to_user(va + 10, b"through-memory").map_err(|_| "store")?;
            copy_to_user(va + P + 7, b"second-page").map_err(|_| "store 2")?;
            copy_to_user(va + 2 * P + 1, b"in-the-hole").map_err(|_| "store 3")?;
            crate::kassert!(ktest_read(&disk, 0, 3 * P as usize)? == expected);
            kassert_eq!(sys_msync(va, 3 * P, general::MS_ASYNC as i32), 0);
            crate::kassert!(on_disk(181)? == original);
            let both = (general::MS_SYNC | general::MS_INVALIDATE) as i32;
            kassert_eq!(sys_msync(va - P, 4 * P, both), neg_errno(errno::ENOMEM));
            crate::kassert!(on_disk(182)? == expected);
            kassert_eq!(sys_munmap(va, 3 * P), 0);
            Ok::<(), &'static str>(())
        })??;

        // On `/tmp` (tmpfs: nothing to write back; `msync` is still 0), from
        // a file offset of one page.
        crate::linux::process::in_test_process(|_| {
            let path = b"/tmp/mem-ktest-shared";
            let fd = ktest_open(path, lumen_vfs::OpenFlags::O_RDWR | lumen_vfs::OpenFlags::O_CREAT)?;
            let inode = Arc::clone(&fd.file().map_err(|_| "fd")?.inode);
            inode.write_at(0, &ktest_pattern(9)).map_err(|_| "write")?;
            let va = ktest_map(PAGE_SIZE, RW, SHARED, fd, PAGE_SIZE)?;
            copy_to_user(va, b"tmpfs-store").map_err(|_| "store")?;
            kassert_eq!(sys_msync(va, PAGE_SIZE, general::MS_SYNC as i32), 0);
            kassert_eq!(ktest_read(&inode, PAGE_SIZE, 11)?, b"tmpfs-store");
            kassert_eq!(sys_munmap(va, PAGE_SIZE), 0);
            lumen_vfs::unlink_at(crate::vfs::tables(), None, path, false).map_err(|_| "unlink")?;
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_private_fd_copies_on_write_and_leaves_the_file_alone() {
        // `MAP_PRIVATE` of a descriptor open only for reading may still be
        // written: the store lands in a private copy, the file keeps its
        // bytes (an `msync(MS_SYNC)` of the range writes nothing either,
        // to the cache or the disk), and a shared mapping of the same page
        // doesn't see it.
        use crate::kassert_eq;
        use crate::user::{copy_from_user, copy_to_user};
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        let vol = Ext4Volume::new(184)?;
        let original = ktest_pattern(5);
        let f = ktest_file(&vol.root, "private.bin", &original)?;
        let mapped = Arc::clone(&f);
        crate::linux::process::in_test_process(move |_| {
            let fd = ktest_fd_on(&mapped, "/ext4/private.bin", lumen_vfs::OpenFlags::O_RDONLY)?;
            let va = ktest_map(2 * PAGE_SIZE, RW, PRIVATE, fd, 0)?;
            let shared = ktest_map(PAGE_SIZE, general::PROT_READ as u64, SHARED, fd, 0)?;
            copy_to_user(va + 3, b"private").map_err(|_| "store")?;
            let mut seen = [0u8; 7];
            copy_from_user(&mut seen, va + 3).map_err(|_| "load")?;
            kassert_eq!(&seen, b"private");
            kassert_eq!(sys_msync(va, 2 * PAGE_SIZE, general::MS_SYNC as i32), 0);
            kassert_eq!(ktest_read(&mapped, 0, original.len())?, original);
            copy_from_user(&mut seen, shared + 3).map_err(|_| "load shared")?;
            kassert_eq!(&seen[..], &original[3..10]);
            kassert_eq!(sys_munmap(va, 2 * PAGE_SIZE), 0);
            kassert_eq!(sys_munmap(shared, PAGE_SIZE), 0);
            Ok::<(), &'static str>(())
        })??;
        f.sync(false).map_err(|_| "sync")?;
        let ro = vol.remount_ro(185)?.lookup("private.bin").map_err(|_| "ro lookup")?;
        crate::kassert!(ktest_read(&ro, 0, ktest_pattern(5).len())? == ktest_pattern(5));
        Ok(())
    }
);

crate::ktest!(
    fn mem_mmap_fd_outlives_close_and_unlink() {
        // The mapping holds the file open: with its descriptor closed and its
        // name unlinked, stores and loads through it still reach the file,
        // `/proc/self/maps` shows its path ` (deleted)`, and `munmap` gives
        // the last reference back.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        crate::linux::process::in_test_process(|_| {
            let path = b"/tmp/mem-ktest-gone";
            let fd = ktest_open(path, lumen_vfs::OpenFlags::O_RDWR | lumen_vfs::OpenFlags::O_CREAT)?;
            let inode = Arc::clone(&fd.file().map_err(|_| "fd")?.inode);
            inode.write_at(0, &ktest_pattern(7)).map_err(|_| "write")?;
            let va = ktest_map(PAGE_SIZE, RW, SHARED, fd, 0)?;
            kassert_eq!(super::fd::sys_close(fd), 0);
            kassert_eq!(inode.open_count(), 1); // the mapping's
            lumen_vfs::unlink_at(crate::vfs::tables(), None, path, false).map_err(|_| "unlink")?;
            kassert!(crate::vfs::resolve_path_from_root("/tmp/mem-ktest-gone").is_err());
            copy_to_user(va + 100, b"after-unlink").map_err(|_| "store")?;
            let mut seen = [0u8; 12];
            copy_from_user(&mut seen, va + 100).map_err(|_| "load")?;
            kassert_eq!(&seen, b"after-unlink");
            kassert_eq!(ktest_read(&inode, 100, 12)?, b"after-unlink");
            let maps = ktest_maps()?;
            kassert!(ktest_line(&maps, va)?.ends_with(" /tmp/mem-ktest-gone (deleted)"));
            kassert_eq!(sys_munmap(va, PAGE_SIZE), 0);
            kassert_eq!(inode.open_count(), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_fd_refuses_with_linuxs_errnos() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::OpenFlags;
        crate::linux::process::in_test_process(|_| {
            let e = |n: u32| neg_errno(n);
            let raw = |fd: Fd| fd.raw() as u32 as u64;
            let p = PAGE_SIZE;
            let read = general::PROT_READ as u64;
            let path = b"/tmp/mem-ktest-errnos";
            let rw = ktest_open(path, OpenFlags::O_RDWR | OpenFlags::O_CREAT)?;
            rw.file().map_err(|_| "fd")?.inode.write_at(0, &ktest_pattern(1)).map_err(|_| "write")?;
            let ro = ktest_open(path, OpenFlags::O_RDONLY)?;
            let wo = ktest_open(path, OpenFlags::O_WRONLY)?;
            // EBADF: not open, or O_PATH.
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, 99, 0), e(errno::EBADF));
            let opath = ktest_open(path, OpenFlags::O_PATH)?;
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, raw(opath), 0), e(errno::EBADF));
            // EINVAL: an offset not page-aligned (before the descriptor is
            // looked at), no length, MAP_HUGETLB, a map type other than
            // shared or private.
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, 99, 0x123), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, 0, read, PRIVATE, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, p, read, PRIVATE | general::MAP_HUGETLB as u64, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, p, read, 0, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, p, read, 0xf, raw(ro), 0), e(errno::EINVAL));
            // ENOMEM: a length whose page rounding overflows; EOVERFLOW: an
            // end in the file past i64::MAX (and past 2^64).
            kassert_eq!(sys_mmap(0, u64::MAX, read, PRIVATE, raw(ro), 0), e(errno::ENOMEM));
            kassert_eq!(sys_mmap(0, 2 * p, read, PRIVATE, raw(ro), !0xfff_u64), e(errno::EOVERFLOW));
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, raw(ro), 1 << 63), e(errno::EOVERFLOW));
            kassert_eq!(sys_mmap(0, 2 * p, read, PRIVATE, raw(ro), (1 << 63) - p), e(errno::EOVERFLOW));
            // MAP_SHARED_VALIDATE refuses flags it doesn't know, and
            // MAP_SYNC; MAP_SHARED ignores them.
            let validate = general::MAP_SHARED_VALIDATE as u64;
            let unknown = 0x20_0000;
            kassert_eq!(sys_mmap(0, p, read, validate | unknown, raw(ro), 0), e(errno::EOPNOTSUPP));
            kassert_eq!(sys_mmap(0, p, read, validate | general::MAP_SYNC as u64, raw(ro), 0), e(errno::EOPNOTSUPP));
            let ignored = ktest_map(p, read, SHARED | unknown | general::MAP_SYNC as u64, ro, 0)?;
            kassert_eq!(sys_munmap(ignored, p), 0);
            let validated = ktest_map(p, read, validate | general::MAP_POPULATE as u64, ro, 0)?;
            kassert_eq!(sys_munmap(validated, p), 0);
            // EACCES: a descriptor not open for reading, and a shared
            // writable mapping of one not open for writing; `mprotect`
            // then can't add PROT_WRITE to a shared read-only one. A
            // private one can be written whatever the descriptor.
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, raw(wo), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, p, RW, SHARED, raw(wo), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, p, RW, SHARED, raw(ro), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, p, RW, validate, raw(ro), 0), e(errno::EACCES));
            let shared_ro = ktest_map(p, read, SHARED, ro, 0)?;
            kassert_eq!(sys_mprotect(shared_ro, p, RW), e(errno::EACCES));
            kassert_eq!(sys_munmap(shared_ro, p), 0);
            let shared_rw = ktest_map(p, read, SHARED, rw, 0)?;
            kassert_eq!(sys_mprotect(shared_rw, p, RW), 0);
            kassert_eq!(sys_munmap(shared_rw, p), 0);
            let private = ktest_map(p, RW, PRIVATE, ro, 0)?;
            kassert_eq!(sys_munmap(private, p), 0);
            // ENODEV: a directory, the console (fd 0), /dev/null.
            let dir = ktest_open(b"/tmp", OpenFlags::O_RDONLY)?;
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, raw(dir), 0), e(errno::ENODEV));
            // ... after the access checks, as Linux's order has them.
            kassert_eq!(sys_mmap(0, p, RW, SHARED, raw(dir), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, p, read, PRIVATE, 0, 0), e(errno::ENODEV));
            let null = ktest_open(b"/dev/null", OpenFlags::O_RDWR)?;
            kassert_eq!(sys_mmap(0, p, read, SHARED, raw(null), 0), e(errno::ENODEV));
            // Placement as the anonymous path's: MAP_FIXED_NOREPLACE over a
            // mapping is EEXIST, MAP_FIXED and a free MAP_FIXED_NOREPLACE
            // take the address.
            let taken = crate::linux::process::KTEST_USER_VA;
            let noreplace = PRIVATE | general::MAP_FIXED_NOREPLACE as u64;
            kassert_eq!(sys_mmap(taken, p, read, noreplace, raw(ro), 0), e(errno::EEXIST));
            let fixed_at = 0x7000_0000_0000;
            kassert_eq!(sys_mmap(fixed_at, p, read, PRIVATE | general::MAP_FIXED as u64, raw(ro), p), fixed_at);
            kassert_eq!(sys_mmap(fixed_at + p, p, read, noreplace, raw(ro), 0), fixed_at + p);
            kassert_eq!(sys_munmap(fixed_at, 2 * p), 0);
            lumen_vfs::unlink_at(crate::vfs::tables(), None, path, false).map_err(|_| "unlink")?;
            // The anonymous path is as it was: `fd` and `off` ignored.
            let va = sys_mmap(0, p, RW, PRIVATE | general::MAP_ANONYMOUS as u64, 99, 0x123);
            kassert!(va < u64::MAX - 4095);
            kassert_eq!(sys_munmap(va, p), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_dev_zero_is_anonymous_memory() {
        // `/dev/zero` maps zero-filled anonymous memory, private or shared,
        // writable through a read-only descriptor when private; its maps
        // line names no file.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        crate::linux::process::in_test_process(|_| {
            let zero = ktest_open(b"/dev/zero", lumen_vfs::OpenFlags::O_RDONLY)?;
            let va = ktest_map(2 * PAGE_SIZE, RW, PRIVATE, zero, 0)?;
            let mut seen = [0xffu8; 16];
            copy_from_user(&mut seen, va + PAGE_SIZE).map_err(|_| "load")?;
            kassert_eq!(seen, [0u8; 16]);
            copy_to_user(va, b"zero-page").map_err(|_| "store")?;
            let maps = ktest_maps()?;
            kassert!(ktest_line(&maps, va)?.ends_with(" rw-p 00000000 00:00 0"));
            kassert_eq!(sys_munmap(va, 2 * PAGE_SIZE), 0);
            // Shared and writable needs the descriptor open for writing.
            kassert_eq!(sys_mmap(0, PAGE_SIZE, RW, SHARED, zero.raw() as u64, 0), neg_errno(errno::EACCES));
            let zero_rw = ktest_open(b"/dev/zero", lumen_vfs::OpenFlags::O_RDWR)?;
            let shared = ktest_map(PAGE_SIZE, RW, SHARED, zero_rw, 0)?;
            copy_to_user(shared, b"shared-zero").map_err(|_| "store shared")?;
            kassert_eq!(sys_munmap(shared, PAGE_SIZE), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_msync_checks_its_arguments_and_the_range() {
        use crate::kassert_eq;
        crate::linux::process::in_test_process(|_| {
            let at = crate::linux::process::KTEST_USER_VA;
            let len = crate::linux::process::KTEST_USER_LEN;
            let sync = general::MS_SYNC as i32;
            let unmapped = 0x7100_0000_0000;
            let e = |n: u32| neg_errno(n);
            // EINVAL: an unknown flag, an address not page-aligned, MS_ASYNC with MS_SYNC.
            kassert_eq!(sys_msync(at, len, 8), e(errno::EINVAL));
            kassert_eq!(sys_msync(at + 1, len, sync), e(errno::EINVAL));
            kassert_eq!(sys_msync(at, len, (general::MS_ASYNC | general::MS_SYNC) as i32), e(errno::EINVAL));
            // ENOMEM: an end past 2^64; then a range wholly or partly unmapped.
            kassert_eq!(sys_msync(!0xfff_u64, 2 * PAGE_SIZE, sync), e(errno::ENOMEM));
            kassert_eq!(sys_msync(unmapped, PAGE_SIZE, sync), e(errno::ENOMEM));
            kassert_eq!(sys_msync(at, len + PAGE_SIZE, sync), e(errno::ENOMEM));
            kassert_eq!(sys_msync(at - PAGE_SIZE, 2 * PAGE_SIZE, general::MS_ASYNC as i32), e(errno::ENOMEM));
            // An empty range is 0, mapped or not; an anonymous one is 0,
            // its length rounded up to a page.
            kassert_eq!(sys_msync(unmapped, 0, sync), 0);
            kassert_eq!(sys_msync(unmapped, u64::MAX, sync), 0); // rounds to 0, as Linux's
            kassert_eq!(sys_msync(at, len - 100, sync), 0);
            kassert_eq!(sys_msync(at, len, general::MS_ASYNC as i32), 0);
            kassert_eq!(sys_msync(at, len, 0), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_proc_maps_names_mapped_files_and_the_executable() {
        // A file mapping's line shows the file's device (major:minor, hex),
        // inode number and path at column 73; an executable loaded from a
        // file names its segments' lines after it, at their file offsets,
        // and its `.bss` past the file's bytes stays anonymous.
        use crate::{kassert, kassert_eq};
        crate::linux::process::in_test_process(|_| {
            let path = b"/tmp/mem-ktest-maps";
            let fd = ktest_open(path, lumen_vfs::OpenFlags::O_RDWR | lumen_vfs::OpenFlags::O_CREAT)?;
            let inode = Arc::clone(&fd.file().map_err(|_| "fd")?.inode);
            inode.write_at(0, &ktest_pattern(2)).map_err(|_| "write")?;
            let ino = inode.stat().map_err(|_| "stat")?.ino;
            let va = ktest_map(PAGE_SIZE, RW, SHARED, fd, PAGE_SIZE)?;
            let maps = ktest_maps()?;
            let line = ktest_line(&maps, va)?;
            let fields = alloc::format!("{va:08x}-{:08x} rw-s 00001000 00:02 {ino}", va + PAGE_SIZE);
            kassert_eq!(line, alloc::format!("{:<72} /tmp/mem-ktest-maps", alloc::format!("{fields} ")));
            kassert_eq!(line.find('/'), Some(73));
            kassert_eq!(sys_munmap(va, PAGE_SIZE), 0);
            lumen_vfs::unlink_at(crate::vfs::tables(), None, path, false).map_err(|_| "unlink")?;
            Ok::<(), &'static str>(())
        })??;

        // The executable: `build_relro_test_elf`'s text at 0x401000 (file
        // offset 0x1000), its rodata and data sharing page 0x402000 (file
        // offset 0x2000), and a .bss page at 0x403000.
        let tmp = crate::vfs::resolve_path_from_root("/tmp").map_err(|_| "/tmp")?;
        let exe = tmp.create("mem-ktest-exe", 0o755).map_err(|_| "create")?;
        let image = crate::mm::user_vm::build_relro_test_elf();
        let written = exe.write_at(0, &image);
        let loaded = crate::linux::exec::Exec::from_path("/tmp/mem-ktest-exe")
            .map_err(|_| "Exec::from_path")
            .and_then(|e| e.load(&[b"/x"], &[], b"/x", [0; 16]).map_err(|_| "load"));
        let ino = exe.ino();
        let listed = loaded.map(|(space, _)| {
            let maps = space.format_maps();
            // Anonymous memory mapped over the text (`MAP_FIXED`) isn't named
            // after the file; the data page beside it still is.
            let anon = u64::from(general::MAP_ANONYMOUS | general::MAP_PRIVATE | general::MAP_FIXED);
            let replaced = space.mmap_anon(0x40_1000, PAGE_SIZE, lumen_vm::Prot::READ, anon);
            let after = space.format_maps();
            space.teardown();
            (maps, replaced, after)
        });
        let unlinked = tmp.unlink("mem-ktest-exe");
        kassert_eq!(written, Ok(image.len()));
        kassert!(unlinked.is_ok());
        let (maps, replaced, after) = listed?;
        let named = |fields: alloc::string::String| alloc::format!("{:<72} /tmp/mem-ktest-exe\n", fields + " ");
        let text = named(alloc::format!("00401000-00402000 r-xp 00001000 00:02 {ino}"));
        let data = named(alloc::format!("00402000-00403000 rw-p 00002000 00:02 {ino}"));
        if !(maps.contains(&text) && maps.contains(&data) && maps.contains("00403000-00404000 rw-p 00000000 00:00 0\n"))
        {
            crate::ktest::diag(format_args!("maps:\n{maps}"));
            return Err("the executable's lines don't name it");
        }
        kassert_eq!(replaced, Ok(0x40_1000));
        if !(after.contains("00401000-00402000 r--p 00000000 00:00 0\n") && after.contains(&data)) {
            crate::ktest::diag(format_args!("maps after MAP_FIXED over the text:\n{after}"));
            return Err("a mapping over the executable's text is named after it");
        }
        Ok(())
    }
);
