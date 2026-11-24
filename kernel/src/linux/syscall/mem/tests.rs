//! The ktests of `mmap` of descriptors, `msync` and `/proc/<pid>/maps`'
//! file columns, each run as a process (`in_test_process`), on `/tmp`
//! (tmpfs, device 0:2) and on fresh ext4 volumes
//! (`vfs::page_cache::ktest_files`, not mounted: their files get a
//! descriptor through `fd::ktest_install`).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use lumen_vfs::{OpenFlags, VfsInode};
use lumen_vm::PAGE_SIZE;

use super::super::at::Fd;
use super::super::fd::{KtestTmpFile, ktest_failed_at, ktest_install, ktest_open, ktest_unlink, sys_close};
use super::super::neg_errno;
use super::{sys_mmap, sys_mprotect, sys_msync, sys_munmap};
use crate::linux::process::{KTEST_USER_LEN, KTEST_USER_VA, in_test_process};
use crate::linux::{errno, general};
use crate::user::{copy_from_user, copy_to_user};
use crate::vfs::ktest_maps_line as named;
use crate::vfs::page_cache::ktest_files::{self, Ext4Volume, file, read};
use crate::{kassert, kassert_eq};

const P: u64 = PAGE_SIZE;
const READ: u64 = general::PROT_READ as u64;
const RW: u64 = (general::PROT_READ | general::PROT_WRITE) as u64;
const SHARED: u64 = general::MAP_SHARED as u64;
const PRIVATE: u64 = general::MAP_PRIVATE as u64;
const FIXED: u64 = general::MAP_FIXED as u64;
const NOREPLACE: u64 = general::MAP_FIXED_NOREPLACE as u64;
const MS_SYNC: i32 = general::MS_SYNC as i32;
const MS_ASYNC: i32 = general::MS_ASYNC as i32;
const MS_INVALIDATE: i32 = general::MS_INVALIDATE as i32;

type Inode = Arc<dyn VfsInode>;

/// `-errno` as a syscall returns it.
fn e(n: u32) -> u64 {
    neg_errno(n)
}

/// A descriptor as `mmap`'s `fd` register holds it.
fn raw(fd: Fd) -> u64 {
    fd.raw() as u32 as u64
}

/// `mmap(addr, len, prot, flags, fd, off)`'s return as an address, or why not.
fn map_at(addr: u64, len: u64, prot: u64, flags: u64, fd: Fd, off: u64) -> Result<u64, &'static str> {
    let va = sys_mmap(addr, len, prot, flags, raw(fd), off);
    if va > u64::MAX - 4095 {
        crate::ktest::diag(format_args!("mmap gave -{}", va.wrapping_neg()));
        return Err("mmap failed");
    }
    Ok(va)
}

/// [`map_at`] wherever the kernel places it.
fn map(len: u64, prot: u64, flags: u64, fd: Fd, off: u64) -> Result<u64, &'static str> {
    map_at(0, len, prot, flags, fd, off)
}

/// The inode descriptor `fd` has open.
fn inode_of(fd: Fd) -> Result<Inode, &'static str> {
    Ok(Arc::clone(fd.file().map_err(|_| "fd")?.inode()))
}

/// Two pages of `ktest_files::pattern` (no zero byte).
fn pattern(seed: u8) -> Vec<u8> {
    ktest_files::pattern(2 * P as usize, seed)
}

/// The current process's `/proc/self/maps`, read through the VFS.
fn maps() -> Result<String, &'static str> {
    let maps = crate::vfs::resolve_path_from_root("/proc/self/maps").map_err(|_| "resolve maps")?;
    String::from_utf8(read(&maps, 0, 64 * 1024)?).map_err(|_| "maps not UTF-8")
}

/// The line of `maps` for the mapping at `va`.
fn line(maps: &str, va: u64) -> Result<&str, &'static str> {
    let start = format!("{va:08x}-");
    maps.lines().find(|l| l.starts_with(&start)).ok_or("no maps line at the mapping")
}

/// The bytes of `name` in the root of a read-only mount of what `vol`'s
/// disk holds now (device `0:minor`), `len` of them.
fn on_disk(vol: &Ext4Volume, minor: u32, name: &str, len: usize) -> Result<Vec<u8>, &'static str> {
    let root = vol.mount_device_copy_ro(minor)?;
    read(&root.lookup(name.as_bytes()).map_err(|_| "lookup on the disk")?, 0, len)
}

crate::ktest!(
    fn mem_mmap_shared_fd_stores_reach_the_disk_through_msync() {
        // `MAP_SHARED` of a descriptor open for reading and writing: stores
        // through the mapping are what the VFS's read path reads, and
        // `msync(MS_SYNC)` writes them to the disk itself (read back from
        // the device's bytes, not through the block cache), where after
        // `MS_ASYNC` and `MS_INVALIDATE` it still holds the old bytes. The
        // third page is a hole (no block on the disk until the writeback
        // allocates one); a second file is mapped right after the first,
        // and one range over both, starting a page below them, writes both
        // back and is `ENOMEM` for that page.
        let vol = Arc::new(Ext4Volume::new(230)?);
        let mut original = pattern(3);
        let f = file(&vol.root, "shared.bin", &original)?;
        f.truncate(3 * P).map_err(|_| "truncate")?;
        let g = file(&vol.root, "other.bin", &pattern(4)[..P as usize])?;
        f.sync(false).and_then(|()| g.sync(false)).map_err(|_| "sync")?;
        original.resize(3 * P as usize, 0);
        let mut expected = original.clone();
        expected[10..24].copy_from_slice(b"through-memory");
        expected[P as usize + 7..P as usize + 18].copy_from_slice(b"second-page");
        expected[2 * P as usize + 1..2 * P as usize + 12].copy_from_slice(b"in-the-hole");
        let mut other = pattern(4)[..P as usize].to_vec();
        other[..5].copy_from_slice(b"other");
        let (vol2, f2, g2) = (Arc::clone(&vol), Arc::clone(&f), Arc::clone(&g));
        in_test_process(move |_| {
            let fd = ktest_install(&f2, b"/ext4/shared.bin", OpenFlags::O_RDWR)?;
            let gd = ktest_install(&g2, b"/ext4/other.bin", OpenFlags::O_RDWR)?;
            // Four pages: three of the file, one of the other right after.
            let at = sys_mmap(0, 4 * P, 0, PRIVATE | general::MAP_ANONYMOUS as u64, 0, 0);
            kassert_eq!(map_at(at, 3 * P, RW, SHARED | FIXED, fd, 0)?, at);
            kassert_eq!(map_at(at + 3 * P, P, RW, SHARED | FIXED, gd, 0)?, at + 3 * P);
            copy_to_user(at + 10, b"through-memory").map_err(|_| "store")?;
            copy_to_user(at + P + 7, b"second-page").map_err(|_| "store 2")?;
            copy_to_user(at + 2 * P + 1, b"in-the-hole").map_err(|_| "store 3")?;
            copy_to_user(at + 3 * P, b"other").map_err(|_| "store 4")?;
            kassert!(read(&f2, 0, 3 * P as usize)? == expected);
            kassert_eq!(sys_msync(at, 4 * P, MS_ASYNC), 0);
            kassert_eq!(sys_msync(at, 4 * P, MS_INVALIDATE), 0);
            kassert!(on_disk(&vol2, 231, "shared.bin", 3 * P as usize)? == original);
            kassert_eq!(sys_msync(at - P, 5 * P, MS_SYNC | MS_INVALIDATE), e(errno::ENOMEM));
            kassert!(on_disk(&vol2, 232, "shared.bin", 3 * P as usize)? == expected);
            kassert!(on_disk(&vol2, 233, "other.bin", P as usize)? == other);
            kassert_eq!(sys_munmap(at, 4 * P), 0);
            Ok::<(), &'static str>(())
        })??;

        // On `/tmp` (tmpfs: nothing to write back; `msync` is still 0), from
        // a file offset of one page.
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-shared", &pattern(9))?;
            let va = map(P, RW, SHARED, tmp.fd(), P)?;
            copy_to_user(va, b"tmpfs-store").map_err(|_| "store")?;
            kassert_eq!(sys_msync(va, P, MS_SYNC), 0);
            kassert_eq!(read(tmp.inode(), P, 11)?, b"tmpfs-store");
            kassert_eq!(sys_munmap(va, P), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_msync_returns_a_writeback_error_before_enomem() {
        // With the disk's writes failing, `msync(MS_SYNC)` over a mapping
        // and a hole below it returns the writeback's `EIO`, not `ENOMEM`.
        // The failed write made the volume read-only (`errors=remount-ro`,
        // #349): with the disk's writes working again, `msync` still writes
        // nothing (`EIO`) and the disk keeps its old bytes, and no new
        // mapping of the file can write it.
        let vol = Arc::new(Ext4Volume::new(236)?);
        let f = file(&vol.root, "failing.bin", &pattern(6)[..P as usize])?;
        f.sync(false).map_err(|_| "sync")?;
        let expected = pattern(6)[..P as usize].to_vec();
        let (vol2, f2) = (Arc::clone(&vol), Arc::clone(&f));
        let (failed, retried) = in_test_process(move |_| {
            let fd = ktest_install(&f2, b"/ext4/failing.bin", OpenFlags::O_RDWR)?;
            let va = map(P, RW, SHARED, fd, 0)?;
            copy_to_user(va + 20, b"retried!").map_err(|_| "store")?;
            vol2.fail_writes(true);
            let failed = sys_msync(va - P, 2 * P, MS_SYNC);
            vol2.fail_writes(false);
            let retried = sys_msync(va, P, MS_SYNC);
            // On the read-only volume, through the same `O_RDWR` descriptor:
            // a new shared writable mapping is `EROFS`; a shared read-only
            // one maps but `mprotect` can't make it writable (`EACCES`, no
            // `VM_MAYWRITE`); a private one is writable as ever.
            kassert_eq!(sys_mmap(0, P, RW, SHARED, raw(fd), 0), e(errno::EROFS));
            let shared_ro = map(P, READ, SHARED, fd, 0)?;
            kassert_eq!(sys_mprotect(shared_ro, P, RW), e(errno::EACCES));
            kassert_eq!(sys_munmap(shared_ro, P), 0);
            let private = map(P, RW, PRIVATE, fd, 0)?;
            copy_to_user(private, b"private").map_err(|_| "private store")?;
            kassert_eq!(sys_munmap(private, P), 0);
            // Nor does `mprotect` refuse write access to a private one.
            let private_ro = map(P, READ, PRIVATE, fd, 0)?;
            kassert_eq!(sys_mprotect(private_ro, P, RW), 0);
            copy_to_user(private_ro, b"private").map_err(|_| "private store after mprotect")?;
            kassert_eq!(sys_munmap(private_ro, P), 0);
            kassert_eq!(sys_munmap(va, P), 0);
            Ok::<_, &'static str>((failed, retried))
        })??;
        kassert_eq!(failed, e(errno::EIO));
        kassert_eq!(retried, e(errno::EIO));
        kassert!(on_disk(&vol, 237, "failing.bin", P as usize)? == expected);
        Ok(())
    }
);

crate::ktest!(
    fn mem_msync_writes_nothing_back_for_a_shared_mapping_made_read_only() {
        // A `MAP_SHARED` mapping through a descriptor open only for reading
        // can't store to the file (Linux clears its `VM_SHARED`), so
        // `msync(MS_SYNC)` writes nothing back for it: another writer's
        // dirty page in the file, which a writeback would fail on here (the
        // disk's writes failing), doesn't fail this `msync`.
        let vol = Arc::new(Ext4Volume::new(238)?);
        let f = file(&vol.root, "read-only.bin", &pattern(4)[..P as usize])?;
        f.sync(false).map_err(|_| "sync")?;
        let (vol2, f2) = (Arc::clone(&vol), Arc::clone(&f));
        let synced = in_test_process(move |_| {
            let fd = ktest_install(&f2, b"/ext4/read-only.bin", OpenFlags::O_RDONLY)?;
            let va = map(P, READ, SHARED, fd, 0)?;
            f2.write_at(5, b"another writer").map_err(|_| "write")?;
            vol2.fail_writes(true);
            let synced = sys_msync(va, P, MS_SYNC);
            vol2.fail_writes(false);
            kassert_eq!(sys_munmap(va, P), 0);
            Ok::<_, &'static str>(synced)
        })??;
        kassert_eq!(synced, 0);
        Ok(())
    }
);

crate::ktest!(
    fn mem_mmap_private_fd_copies_on_write_and_leaves_the_file_alone() {
        // `MAP_PRIVATE` of a descriptor open only for reading may still be
        // written: the store lands in a private copy, the file keeps its
        // bytes (an `msync(MS_SYNC)` of the range writes nothing either,
        // to the cache or the disk), and a shared mapping of the same page
        // doesn't see it.
        let vol = Ext4Volume::new(234)?;
        let original = pattern(5);
        let f = file(&vol.root, "private.bin", &original)?;
        let mapped = Arc::clone(&f);
        let copy = original.clone();
        in_test_process(move |_| {
            let fd = ktest_install(&mapped, b"/ext4/private.bin", OpenFlags::O_RDONLY)?;
            let va = map(2 * P, RW, PRIVATE, fd, 0)?;
            let shared = map(P, READ, SHARED, fd, 0)?;
            copy_to_user(va + 3, b"private").map_err(|_| "store")?;
            let mut seen = [0u8; 7];
            copy_from_user(&mut seen, va + 3).map_err(|_| "load")?;
            kassert_eq!(&seen, b"private");
            kassert_eq!(sys_msync(va, 2 * P, MS_SYNC), 0);
            kassert_eq!(read(&mapped, 0, copy.len())?, copy);
            copy_from_user(&mut seen, shared + 3).map_err(|_| "load shared")?;
            kassert_eq!(&seen[..], &copy[3..10]);
            kassert_eq!(sys_munmap(va, 2 * P), 0);
            kassert_eq!(sys_munmap(shared, P), 0);
            Ok::<(), &'static str>(())
        })??;
        f.sync(false).map_err(|_| "sync")?;
        kassert!(on_disk(&vol, 235, "private.bin", original.len())? == original);
        Ok(())
    }
);

crate::ktest!(
    fn mem_open_trunc_unmaps_the_callers_pages() {
        // `open(O_TRUNC)` of a file the caller maps, shared and private,
        // both faulted in (the private one copied on write): the truncate
        // drops the cache's pages, and the open unmaps the caller's, so a
        // later touch is a bus error (`EFAULT` for a kernel copy), not the
        // old bytes or a store to a page the file no longer has. A reopen
        // without `O_TRUNC` first unmaps nothing: the private copy keeps
        // its bytes and the shared page still reads the file.
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-trunc", &pattern(8))?;
            let (path, fd) = (tmp.path(), tmp.fd());
            let shared = map(2 * P, RW, SHARED, fd, 0)?;
            let private = map(2 * P, RW, PRIVATE, fd, 0)?;
            let mut seen = [0u8; 4];
            copy_from_user(&mut seen, shared + P).map_err(|_| "fault in")?;
            copy_to_user(private + 8, b"copy").map_err(|_| "copy on write")?;
            let _reopened = ktest_open(3, path, OpenFlags::O_RDWR)?;
            copy_from_user(&mut seen, private + 8).map_err(|_| "the private page is gone")?;
            kassert_eq!(&seen, b"copy");
            copy_from_user(&mut seen, shared + P).map_err(|_| "the shared page is gone")?;
            kassert_eq!(&seen[..], &pattern(8)[P as usize..P as usize + 4]);
            let again = ktest_open(3, path, OpenFlags::O_RDWR | OpenFlags::O_TRUNC)?;
            kassert!(copy_from_user(&mut seen, shared + P).is_err());
            kassert!(copy_to_user(shared, b"lost").is_err());
            kassert!(copy_from_user(&mut seen, private + 8).is_err());
            kassert_eq!(inode_of(again)?.stat().map(|m| m.size), Ok(0));
            kassert_eq!(sys_munmap(shared, 2 * P), 0);
            kassert_eq!(sys_munmap(private, 2 * P), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_open_path_with_o_trunc_leaves_the_file_and_its_mappings_alone() {
        // `open(O_PATH | O_TRUNC)` doesn't truncate (`O_PATH` opens no
        // file for I/O), so it unmaps nothing: a private copy-on-write page
        // keeps its bytes, a shared page stays mapped, and the file its size.
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-path-trunc", &pattern(9))?;
            let (path, fd, inode) = (tmp.path(), tmp.fd(), tmp.inode());
            let private = map(2 * P, RW, PRIVATE, fd, 0)?;
            let shared = map(2 * P, READ, SHARED, fd, 0)?;
            copy_to_user(private + 8, b"private!").map_err(|_| "copy on write")?;
            let mut seen = [0u8; 8];
            copy_from_user(&mut seen, shared + P).map_err(|_| "fault in")?;
            let _path_fd = ktest_open(3, path, OpenFlags::O_PATH | OpenFlags::O_TRUNC)?;
            kassert_eq!(inode.stat().map(|m| m.size), Ok(2 * P));
            copy_from_user(&mut seen, private + 8).map_err(|_| "the private page is gone")?;
            kassert_eq!(&seen, b"private!");
            copy_from_user(&mut seen, shared + P).map_err(|_| "the shared page is gone")?;
            kassert_eq!(&seen[..], &pattern(9)[P as usize..P as usize + 8]);
            kassert_eq!(sys_munmap(private, 2 * P), 0);
            kassert_eq!(sys_munmap(shared, 2 * P), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_fd_outlives_close_and_unlink() {
        // The mapping holds the file open: with its descriptor closed and its
        // name unlinked, stores and loads through it still reach the file,
        // `/proc/self/maps` shows its path ` (deleted)`, and `munmap` gives
        // the last reference back.
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-gone", &pattern(7))?;
            let (path, fd, inode) = (tmp.path(), tmp.fd(), Arc::clone(tmp.inode()));
            let va = map(P, RW, SHARED, fd, 0)?;
            kassert_eq!(sys_close(fd), 0);
            kassert_eq!(inode.open_count(), 1); // the mapping's
            ktest_unlink(path).map_err(|e| ktest_failed_at("unlink", path, e))?;
            kassert!(crate::vfs::resolve_path_from_root("/tmp/mem-ktest-gone").is_err());
            copy_to_user(va + 100, b"after-unlink").map_err(|_| "store")?;
            let mut seen = [0u8; 12];
            copy_from_user(&mut seen, va + 100).map_err(|_| "load")?;
            kassert_eq!(&seen, b"after-unlink");
            kassert_eq!(read(&inode, 100, 12)?, b"after-unlink");
            let maps = maps()?;
            kassert!(line(&maps, va)?.ends_with(" /tmp/mem-ktest-gone (deleted)"));
            kassert_eq!(sys_munmap(va, P), 0);
            drop(tmp);
            kassert_eq!(inode.open_count(), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_fd_refuses_with_linuxs_errnos() {
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-errnos", &pattern(1))?;
            let (path, rw) = (tmp.path(), tmp.fd());
            let ro = ktest_open(3, path, OpenFlags::O_RDONLY)?;
            let wo = ktest_open(3, path, OpenFlags::O_WRONLY)?;
            // EBADF: not open, or O_PATH.
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, 99, 0), e(errno::EBADF));
            let opath = ktest_open(3, path, OpenFlags::O_PATH)?;
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, raw(opath), 0), e(errno::EBADF));
            // EINVAL: an offset not page-aligned (before the descriptor is
            // looked at), no length, MAP_HUGETLB, a map type other than
            // shared or private.
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, 99, 0x123), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, 0, READ, PRIVATE, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE | general::MAP_HUGETLB as u64, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, P, READ, 0, raw(ro), 0), e(errno::EINVAL));
            kassert_eq!(sys_mmap(0, P, READ, 0xf, raw(ro), 0), e(errno::EINVAL));
            // ENOMEM: a length whose page rounding overflows; EOVERFLOW: an
            // end in the file past i64::MAX (and past 2^64).
            kassert_eq!(sys_mmap(0, u64::MAX, READ, PRIVATE, raw(ro), 0), e(errno::ENOMEM));
            kassert_eq!(sys_mmap(0, 2 * P, READ, PRIVATE, raw(ro), !0xfff_u64), e(errno::EOVERFLOW));
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, raw(ro), 1 << 63), e(errno::EOVERFLOW));
            kassert_eq!(sys_mmap(0, 2 * P, READ, PRIVATE, raw(ro), (1 << 63) - P), e(errno::EOVERFLOW));
            // MAP_SHARED_VALIDATE refuses flags it doesn't know, MAP_SYNC
            // and MAP_FIXED_NOREPLACE (outside Linux's LEGACY_MAP_MASK);
            // MAP_SHARED ignores them.
            let validate = general::MAP_SHARED_VALIDATE as u64;
            let unknown = 0x20_0000;
            kassert_eq!(sys_mmap(0, P, READ, validate | unknown, raw(ro), 0), e(errno::EOPNOTSUPP));
            kassert_eq!(sys_mmap(0, P, READ, validate | general::MAP_SYNC as u64, raw(ro), 0), e(errno::EOPNOTSUPP));
            kassert_eq!(sys_mmap(0x7000_0000_0000, P, READ, validate | NOREPLACE, raw(ro), 0), e(errno::EOPNOTSUPP));
            let ignored = map(P, READ, SHARED | unknown | general::MAP_SYNC as u64, ro, 0)?;
            kassert_eq!(sys_munmap(ignored, P), 0);
            let validated = map(P, READ, validate | general::MAP_POPULATE as u64, ro, 0)?;
            kassert_eq!(sys_munmap(validated, P), 0);
            // EACCES: a descriptor not open for reading, and a shared
            // writable mapping of one not open for writing; `mprotect`
            // then can't add PROT_WRITE to a shared read-only one. A
            // private one can be written whatever the descriptor.
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, raw(wo), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, P, RW, SHARED, raw(wo), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, P, RW, SHARED, raw(ro), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, P, RW, validate, raw(ro), 0), e(errno::EACCES));
            let shared_ro = map(P, READ, SHARED, ro, 0)?;
            kassert_eq!(sys_mprotect(shared_ro, P, RW), e(errno::EACCES));
            kassert_eq!(sys_munmap(shared_ro, P), 0);
            let shared_rw = map(P, READ, SHARED, rw, 0)?;
            kassert_eq!(sys_mprotect(shared_rw, P, RW), 0);
            kassert_eq!(sys_munmap(shared_rw, P), 0);
            let private = map(P, RW, PRIVATE, ro, 0)?;
            kassert_eq!(sys_munmap(private, P), 0);
            // ENODEV: a directory, the console (fd 0), /dev/null.
            let dir = ktest_open(3, b"/tmp", OpenFlags::O_RDONLY)?;
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, raw(dir), 0), e(errno::ENODEV));
            // ... after the access checks, as Linux's order has them.
            kassert_eq!(sys_mmap(0, P, RW, SHARED, raw(dir), 0), e(errno::EACCES));
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE, 0, 0), e(errno::ENODEV));
            let null = ktest_open(3, b"/dev/null", OpenFlags::O_RDWR)?;
            kassert_eq!(sys_mmap(0, P, READ, SHARED, raw(null), 0), e(errno::ENODEV));
            // MAP_GROWSDOWN on a file: EINVAL (Linux's `vma_flags_can_grow`).
            kassert_eq!(sys_mmap(0, P, READ, PRIVATE | general::MAP_GROWSDOWN as u64, raw(ro), 0), e(errno::EINVAL));
            // Placement as the anonymous path's: MAP_FIXED_NOREPLACE over a
            // mapping is EEXIST, MAP_FIXED and a free MAP_FIXED_NOREPLACE
            // take the address.
            kassert_eq!(sys_mmap(KTEST_USER_VA, P, READ, PRIVATE | NOREPLACE, raw(ro), 0), e(errno::EEXIST));
            let fixed_at = 0x7000_0000_0000;
            kassert_eq!(sys_mmap(fixed_at, P, READ, PRIVATE | FIXED, raw(ro), P), fixed_at);
            kassert_eq!(sys_mmap(fixed_at + P, P, READ, PRIVATE | NOREPLACE, raw(ro), 0), fixed_at + P);
            kassert_eq!(sys_munmap(fixed_at, 2 * P), 0);
            // The anonymous path is as it was: `fd` and `off` ignored.
            let va = sys_mmap(0, P, RW, PRIVATE | general::MAP_ANONYMOUS as u64, 99, 0x123);
            kassert!(va < u64::MAX - 4095);
            kassert_eq!(sys_munmap(va, P), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_mmap_dev_zero_is_anonymous_memory() {
        // `/dev/zero` maps zero-filled anonymous memory, private or shared,
        // writable through a read-only descriptor when private, placed as
        // asked, at an offset past i64::MAX (below 2^64, as a device's may
        // be); its maps line names no file.
        in_test_process(|_| {
            let zero = ktest_open(3, b"/dev/zero", OpenFlags::O_RDONLY)?;
            let va = map(2 * P, RW, PRIVATE, zero, 0)?;
            let mut seen = [0xffu8; 16];
            copy_from_user(&mut seen, va + P).map_err(|_| "load")?;
            kassert_eq!(seen, [0u8; 16]);
            copy_to_user(va, b"zero-page").map_err(|_| "store")?;
            let maps = maps()?;
            kassert!(line(&maps, va)?.ends_with(" rw-p 00000000 00:00 0"));
            kassert_eq!(sys_munmap(va, 2 * P), 0);
            // Placement: over the test's own memory, MAP_FIXED_NOREPLACE is
            // EEXIST; MAP_FIXED takes the address it is given.
            kassert_eq!(sys_mmap(KTEST_USER_VA, P, RW, PRIVATE | NOREPLACE, raw(zero), 0), e(errno::EEXIST));
            let at = 0x7000_0000_0000;
            kassert_eq!(sys_mmap(at, P, RW, PRIVATE | FIXED, raw(zero), 0), at);
            kassert_eq!(sys_munmap(at, P), 0);
            // A device's offset may pass i64::MAX (not 2^64).
            let high = map(P, READ, PRIVATE, zero, 1 << 63)?;
            kassert_eq!(sys_munmap(high, P), 0);
            // Shared and writable needs the descriptor open for writing.
            kassert_eq!(sys_mmap(0, P, RW, SHARED, raw(zero), 0), e(errno::EACCES));
            let zero_rw = ktest_open(3, b"/dev/zero", OpenFlags::O_RDWR)?;
            let shared = map(P, RW, SHARED, zero_rw, 0)?;
            copy_to_user(shared, b"shared-zero").map_err(|_| "store shared")?;
            kassert_eq!(sys_munmap(shared, P), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_msync_checks_its_arguments_and_the_range() {
        in_test_process(|_| {
            let (at, len) = (KTEST_USER_VA, KTEST_USER_LEN);
            let unmapped = 0x7100_0000_0000;
            // EINVAL: an unknown flag, an address not page-aligned, MS_ASYNC with MS_SYNC.
            kassert_eq!(sys_msync(at, len, 8), e(errno::EINVAL));
            kassert_eq!(sys_msync(at + 1, len, MS_SYNC), e(errno::EINVAL));
            kassert_eq!(sys_msync(at, len, MS_ASYNC | MS_SYNC), e(errno::EINVAL));
            // ENOMEM: an end past 2^64; then a range wholly or partly unmapped.
            kassert_eq!(sys_msync(!0xfff_u64, 2 * P, MS_SYNC), e(errno::ENOMEM));
            kassert_eq!(sys_msync(unmapped, P, MS_SYNC), e(errno::ENOMEM));
            kassert_eq!(sys_msync(at, len + P, MS_SYNC), e(errno::ENOMEM));
            kassert_eq!(sys_msync(at - P, 2 * P, MS_ASYNC), e(errno::ENOMEM));
            // An empty range is 0, mapped or not; an anonymous one is 0,
            // its length rounded up to a page.
            kassert_eq!(sys_msync(unmapped, 0, MS_SYNC), 0);
            kassert_eq!(sys_msync(unmapped, u64::MAX, MS_SYNC), 0); // rounds to 0, as Linux's
            kassert_eq!(sys_msync(at, len - 100, MS_SYNC), 0);
            kassert_eq!(sys_msync(at, len, MS_ASYNC), 0);
            kassert_eq!(sys_msync(at, len, 0), 0);
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_dispatch_takes_mmap_and_msync_registers_in_order() {
        // Through `dispatch`, each argument in its own register (garbage
        // above an `int`'s 32 bits where Linux reads one), so an arm
        // passing them out of order, or not dispatching them, can't pass:
        // `mmap(addr, len, prot, flags, fd, off)` from rdi, rsi, rdx, r10,
        // r8, r9 places the file's second page where asked; `msync(addr,
        // len, flags)` from rdi, rsi, rdx writes it back (swapped, the
        // flags would be an unaligned address or the address an unknown
        // flag: `EINVAL`).
        use crate::arch::syscall::SyscallFrame;
        in_test_process(|_| {
            let call = |nr: u32, args: [u64; 6]| {
                let [rdi, rsi, rdx, r10, r8, r9] = args;
                let mut frame =
                    SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, r8, r9, ..SyscallFrame::default() };
                let _ = super::super::dispatch(&mut frame);
                frame.rax
            };
            let high = 0xdead_beef_0000_0000u64;
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-dispatch", &pattern(2))?;
            let (fd, inode) = (tmp.fd(), tmp.inode());
            let at = 0x7000_0000_0000;
            kassert_eq!(call(general::__NR_mmap, [at, P, RW, SHARED | FIXED, high | raw(fd), P]), at);
            let mut seen = [0u8; 8];
            copy_from_user(&mut seen, at).map_err(|_| "load")?;
            kassert_eq!(&seen[..], &pattern(2)[P as usize..P as usize + 8]);
            copy_to_user(at, b"syscall!").map_err(|_| "store")?;
            kassert_eq!(call(general::__NR_msync, [at, P, high | u64::from(general::MS_SYNC), 0, 0, 0]), 0);
            kassert_eq!(read(inode, P, 8)?, b"syscall!");
            kassert_eq!(call(general::__NR_msync, [at, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_msync, [at + 1, P, 0, 0, 0, 0]), e(errno::EINVAL));
            kassert_eq!(call(general::__NR_munmap, [at, P, 0, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_msync, [at, P, u64::from(general::MS_SYNC), 0, 0, 0]), e(errno::ENOMEM));
            Ok::<(), &'static str>(())
        })?
    }
);

crate::ktest!(
    fn mem_proc_maps_names_mapped_files_and_the_executable() {
        // A file mapping's line shows the file's device (major:minor, hex),
        // inode number and path after 73 characters; an executable loaded
        // from a file names its segments' lines after it, at their file
        // offsets, its `.bss` past the file's bytes anonymous.
        in_test_process(|_| {
            let tmp = KtestTmpFile::new(b"/tmp/mem-ktest-maps", &pattern(2))?;
            let ino = tmp.inode().stat().map_err(|_| "stat")?.ino;
            let va = map(P, RW, SHARED, tmp.fd(), P)?;
            let maps = maps()?;
            let line = line(&maps, va)?;
            let fields = format!("{va:08x}-{:08x} rw-s 00001000 00:02 {ino}", va + P);
            kassert_eq!(format!("{line}\n"), named(&fields, "/tmp/mem-ktest-maps"));
            kassert_eq!(line.find('/'), Some(73));
            kassert_eq!(sys_munmap(va, P), 0);
            Ok::<(), &'static str>(())
        })??;

        // `build_relro_test_elf`'s text at 0x401000 (file offset 0x1000),
        // its rodata and data sharing page 0x402000 (file offset 0x2000),
        // and a .bss page at 0x403000. Anonymous memory mapped over the text
        // (`MAP_FIXED`) isn't named after the file; the data page beside it
        // still is, until it is unmapped: memory mapped there again
        // (`MAP_FIXED_NOREPLACE`, replacing nothing) isn't named either.
        let relro = load_and_list("mem-ktest-exe", &crate::mm::user_vm::build_relro_test_elf(), |space| {
            let anon = u64::from(general::MAP_ANONYMOUS | general::MAP_PRIVATE | general::MAP_FIXED);
            let replaced = space.mmap_anon(0x40_1000, P, lumen_vm::Prot::READ, anon);
            let after = space.format_maps_text();
            let noreplace = u64::from(general::MAP_ANONYMOUS | general::MAP_PRIVATE | general::MAP_FIXED_NOREPLACE);
            let unmapped = space.munmap(0x40_2000, P);
            // Read and exec, so it coalesces with neither neighbour.
            let again = space.mmap_anon(0x40_2000, P, lumen_vm::Prot::READ | lumen_vm::Prot::EXEC, noreplace);
            (replaced, after, unmapped.and(again), space.format_maps_text())
        })?;
        let (maps, ino, (replaced, after, again, last)) = relro;
        let at = |fields: &str| named(&format!("{fields} 00:02 {ino}"), "/tmp/mem-ktest-exe");
        let text = at("00401000-00402000 r-xp 00001000");
        let data = at("00402000-00403000 rw-p 00002000");
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
        kassert_eq!(again, Ok(0x40_2000));
        if !last.contains("00402000-00403000 r-xp 00000000 00:00 0\n") {
            crate::ktest::diag(format_args!("maps after munmap and a new mapping over the data:\n{last}"));
            return Err("a mapping where the executable's data was unmapped is named after it");
        }

        // A segment starting mid-page, its first page its own: its line
        // shows the offset of that page in the file (0x2000), not the
        // segment's (0x2100).
        use lumen_elf::loader::{ET_EXEC, PF_R, PF_W, PF_X, PT_LOAD};
        let mid = crate::user::build_elf_with_phdrs(
            ET_EXEC,
            0x40_1000,
            &[
                (PT_LOAD, PF_R | PF_X, 0x1000, 0x40_1000, 0x20, 0x20),
                (PT_LOAD, PF_R | PF_W, 0x2100, 0x40_3100, 0x100, 0x100),
            ],
            0x3000,
            &[(0x1000, &[0x90, 0x90, 0x90, 0xc3])],
        );
        let (maps, ino, ()) = load_and_list("mem-ktest-mid", &mid, |_| ())?;
        let data = named(&format!("00403000-00404000 rw-p 00002000 00:02 {ino}"), "/tmp/mem-ktest-mid");
        if !maps.contains(&data) {
            crate::ktest::diag(format_args!("maps:\n{maps}"));
            return Err("a mid-page segment's line has the wrong offset");
        }
        Ok(())
    }
);

/// Write `image` to `/tmp/<name>` (mode 0o755), load it with
/// `Exec::from_path`, and hand back its maps listing, its inode number and
/// what `then` makes of the loaded space (torn down after); the file is
/// gone again either way.
fn load_and_list<T>(
    name: &str,
    image: &[u8],
    then: impl FnOnce(&crate::mm::user_vm::AddressSpace) -> T,
) -> Result<(String, u64, T), &'static str> {
    let tmp = crate::vfs::resolve_path_from_root("/tmp").map_err(|_| "/tmp")?;
    let exe = tmp.create(name.as_bytes(), 0o755).map_err(|_| "create")?;
    let written = exe.write_at(0, image);
    let loaded = crate::linux::exec::Exec::from_path(&format!("/tmp/{name}"))
        .map_err(|_| "Exec::from_path")
        .and_then(|e| e.load(&[b"/x"], &[], b"/x", [0; 16]).map_err(|_| "load"));
    let ino = exe.ino();
    let listed = loaded.map(|(space, _)| {
        let maps = space.format_maps_text();
        let made = then(&space);
        space.teardown();
        (maps, made)
    });
    let unlinked = tmp.unlink(name.as_bytes());
    kassert_eq!(written, Ok(image.len()));
    kassert!(unlinked.is_ok());
    let (maps, made) = listed?;
    Ok((maps, ino, made))
}
