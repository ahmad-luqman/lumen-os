//! Memory: `mmap` (anonymous, M6; of a file descriptor, #255), `mprotect`,
//! `munmap`, `brk` (M6), and `msync` (#255).
//!
//! **`mmap` of a descriptor** checks what Linux's `ksys_mmap_pgoff` and
//! `do_mmap` check, mostly in their order (placement apart, below): the
//! offset page-aligned (`EINVAL`), the descriptor open and not `O_PATH`
//! (`EBADF`), no `MAP_HUGETLB` (`EINVAL`: no file is on hugetlbfs), a
//! non-zero length (`EINVAL`) whose page rounding fits (`ENOMEM`), an end
//! in the file the inode can have (`EOVERFLOW`: at most `i64::MAX`, the
//! largest `loff_t`, for a regular file, below 2^64 otherwise, Linux's
//! `ULONG_MAX` for a character device: `file_mmap_ok`), then
//! the type in `flags & MAP_TYPE`:
//! - `MAP_SHARED`: flags Linux doesn't know are ignored;
//!   `MAP_SHARED_VALIDATE` refuses them (`EOPNOTSUPP`), and `MAP_SYNC` and
//!   `MAP_FIXED_NOREPLACE` with them (Linux's `LEGACY_MAP_MASK` holds
//!   neither; no file here is on DAX). Either way `PROT_WRITE` needs the
//!   descriptor open for writing (`EACCES`), which is also what lets a
//!   later `mprotect` add it (`VM_MAYWRITE`: `O_RDWR`, recorded from the
//!   description itself by `user_vm::MappedFile::of`), on a volume still
//!   writable (an `ext4` one errored read-only since the open, #349:
//!   `EROFS` for `PROT_WRITE`, and no `VM_MAYWRITE`);
//! - `MAP_PRIVATE`: copy-on-write, writable whatever the descriptor's mode;
//! - anything else: `EINVAL`.
//!
//! Then the descriptor must be open for reading (`EACCES`), the file must
//! have pages to map (`ENODEV`), and `MAP_GROWSDOWN` is `EINVAL`. What maps:
//! a regular `ext4` or `tmpfs` file, or `/dev/zero` (any character device
//! numbered 1:5, as Linux's `mmap_zero` is that device's; its `stat` error,
//! if any, returned), which maps anonymous memory as `MAP_ANONYMOUS` would,
//! shared or private as asked. Directories, the console, `/dev/null` and
//! the rest are `ENODEV`. The advisory flags (`MAP_DENYWRITE`,
//! `MAP_EXECUTABLE`, `MAP_NORESERVE`, `MAP_POPULATE`, `MAP_NONBLOCK`,
//! `MAP_STACK`, `MAP_LOCKED`, `MAP_UNINITIALIZED`) are taken and ignored.
//! Placement (`MAP_FIXED`, `MAP_FIXED_NOREPLACE`, a hint) is the anonymous
//! path's (`user_vm::map_place`). The mapping holds its own open reference
//! on the file, so it outlives the descriptor's `close` and the name's
//! `unlink`.
//!
//! **Not as Linux:**
//! - Placement's errors (`MAP_FIXED_NOREPLACE`'s `EEXIST`, no room's
//!   `ENOMEM`) come after every check above; Linux finds the address (in
//!   `get_unmapped_area`) before `EOVERFLOW`, the map type and every check
//!   after it.
//! - `MAP_32BIT` and `MAP_ABOVE4G` are taken and ignored: the mapping is
//!   placed as without them.
//! - A shared `/dev/zero` mapping is plain anonymous memory: its maps line
//!   is an unnamed `s` one (Linux names it `/dev/zero (deleted)`; M10's
//!   shmem backing will supply that name),
//!   and one made through a read-only descriptor can later be made
//!   writable with `mprotect` (Linux refuses, `EACCES`): anonymous VMAs
//!   carry no `may_write`. This is no longer harmless: `fork_copy` shares
//!   the anonymous pages, so the upgrade allows writes visible to the child.
//! - `/proc/<pid>/maps`' file columns differ in a few ways, each listed
//!   on `user_vm::KeptFile` (a file mapping's: the descriptor's path, not
//!   normalised nor following a rename; the first mapping's for every
//!   mapping of a file; ` (deleted)` only once the file's last link is
//!   gone, then with the synthetic inode number a detached ext4 inode gets)
//!   and `user_vm::ImageFile` (an executable's: the path it was run by, a
//!   symlink in it unresolved; never ` (deleted)`; the inode number from
//!   load time).

use lumen_vfs::{AccessMode, DevId, InodeType};
use lumen_vm::PAGE_SIZE;

use super::at::{Fd, reg};
use super::{neg_errno, not_implemented};
use crate::linux::{errno, general};
use crate::mm::user_vm::{MappedFile, Sharing, map_place, prot_from_linux};
use crate::sched;
use crate::vfs::page_cache;

/// `/dev/zero`'s device number (Linux's `mem` major, minor 5).
const DEV_ZERO: DevId = DevId::makedev(1, 5);

/// The flags `MAP_SHARED_VALIDATE` takes: Linux's `LEGACY_MAP_MASK`.
/// `MAP_SYNC` is not among them (Linux takes it only for a file on DAX),
/// nor `MAP_FIXED_NOREPLACE`.
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
    | general::MAP_HUGE_1GB;

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
    let inode = file.inode();
    let max_end = if inode.inode_type() == InodeType::File { i64::MAX as u64 } else { u64::MAX };
    if off.checked_add(aligned_len).is_none_or(|end| end > max_end) {
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
            Sharing::Shared
        }
        general::MAP_PRIVATE => Sharing::Private,
        _ => return Err(errno::EINVAL),
    };
    if mode == AccessMode::WriteOnly {
        return Err(errno::EACCES);
    }
    let dev_zero =
        inode.inode_type() == InodeType::CharDevice && inode.stat().map_err(|e| e.to_errno())?.rdev == DEV_ZERO;
    if !dev_zero && !page_cache::mappable(&**inode) {
        return Err(errno::ENODEV);
    }
    // An `ext4` volume gone read-only since the descriptor was opened
    // (`errors=remount-ro`, #349) takes no new shared writable mapping, nor
    // one `mprotect` could make writable later.
    let mount_writable = dev_zero || !page_cache::mapped_read_only(&**inode);
    if sharing == Sharing::Shared && prot.write() && !mount_writable {
        return Err(errno::EROFS);
    }
    if flags & u64::from(general::MAP_GROWSDOWN) != 0 {
        return Err(errno::EINVAL); // Linux's `vma_flags_can_grow`
    }
    let space = sched::current_address_space().ok_or(errno::ENOMEM)?;
    let place = map_place(addr, flags);
    if dev_zero {
        // Anonymous memory, with the requested placement and sharing.
        return space.map_anon_at(place, len, prot, sharing).map_err(|e| e.to_errno());
    }
    space.mmap_file(place, len, prot, MappedFile::of(&file, mount_writable), off, sharing).map_err(|e| e.to_errno())
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
///   empty range (no mapping needed), or a `length` within a page of 2^64
///   (Linux's rounding wraps it to 0).
/// - `MS_SYNC` writes back every file a `MAP_SHARED` VMA in the range maps
///   and waits (`VfsInode::sync` with `data_only`, Linux's
///   `vfs_fsync_range(.., 1)`, over the whole file rather than the range;
///   its error (such as `EIO`) returned, before any `ENOMEM`). The files are found under the address
///   space's lock and written back after it is released
///   (`AddressSpace::msync_files`). A writable shared page is dirty for as
///   long as it is mapped writable, so stores made through the mapping
///   reach the disk without the PTEs' dirty bits being read.
/// - `MS_ASYNC` writes nothing: Linux's has done nothing since 2.6.19 (no
///   I/O since 2.5.67)
///   either; the pages are already dirty in the page cache, which a
///   writeback (`fsync`, `sync`, reclaim) writes. `MS_INVALIDATE` is
///   accepted and does nothing: a `MAP_SHARED` mapping is the page cache's
///   own pages, so nothing is stale.
/// - Private and anonymous ranges have nothing to write, nor a shared one
///   made through a read-only descriptor (Linux's `do_mmap` clears its
///   `VM_SHARED`): another writer's dirty pages in that file are left to
///   its own writeback, so its error can't fail this `msync`.
/// - Part of the range mapped by no VMA is `ENOMEM` (with `MS_SYNC`, after
///   the mapped parts are written back).
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

/// `madvise(addr, len, advice)` (28): M9 stub, replaced by C7 (#484).
pub(super) fn sys_madvise(_addr: u64, _len: u64, _advice: i32) -> u64 {
    not_implemented(general::__NR_madvise)
}

#[cfg(feature = "ktest")]
mod tests;
