//! Positional I/O and syncing (M7): `pread64`, `pwrite64`, `fsync`,
//! `fdatasync`, `sync` (#254), and `msync` (#255, with file-backed `mmap`).
//!
//! `pread64` and `pwrite64` go to the inode at the position they're given,
//! a chunk at a time through a bounce buffer on the stack as `fd.rs`'s
//! `read`/`write` do (the write side through its `write_chunks_from_user`),
//! and never read or move the open file's offset, so they take no offset
//! lock (Lock 2); no VFS lock is held across a copy to or from user memory.
//! `fsync` and `fdatasync` are the inode's own `VfsInode::sync` (ext4: its
//! dirty pages and metadata, then the block cache and the device's flush);
//! `sync` is `vfs::sync_all`.

use super::at::{Fd, UserPtr};
use super::fd::{FILE_CHUNK, partial, vfs, write_chunks_from_user};
use super::{MAX_SINGLE_IO, finish, not_implemented};
use crate::linux::process::FileRef;
use crate::linux::{errno, general};
use crate::user::{copy_to_user, user_range_valid};
use lumen_vfs::{InodeType, OpenFlags, VfsInode};

// A positional transfer's checks come in Linux's order (`ksys_pread64`,
// `vfs_read`; `pwrite64` the same, without the last):
// 1. a negative offset: `EINVAL`, before the fd is looked up;
// 2. the fd not open, or `O_PATH`: `EBADF`;
// 3. no position (a terminal, FIFO or socket): `ESPIPE`;
// 4. the wrong access mode: `EBADF`;
// 5. the buffer leaving user space: `EFAULT`;
// 6. the end past `i64::MAX`: `EINVAL`;
// 7. a directory (`pread64`): `EISDIR`, even for 0 bytes.

/// `EINVAL` for an offset negative as an `loff_t`: checked before the fd.
fn nonnegative(offset: u64) -> Result<u64, u32> {
    if offset > i64::MAX as u64 { Err(errno::EINVAL) } else { Ok(offset) }
}

/// The open file on `fd` for a positional transfer: `EBADF` if it isn't
/// open or is `O_PATH`, `ESPIPE` if it has no position (a terminal; a FIFO
/// or socket node, which ext4 can hold). Before the access mode's check, so
/// a `pread64` on the write-only fd 1 is `ESPIPE`, not `EBADF`.
fn positional_file(fd: Fd) -> Result<FileRef, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    if file.inode.is_tty() || matches!(file.inode.inode_type(), InodeType::Fifo | InodeType::Socket) {
        return Err(errno::ESPIPE);
    }
    Ok(file)
}

/// How much of `count` bytes at `buf`, from `pos`, to move.
///
/// `EFAULT` if the buffer isn't wholly inside user space (`user_range_valid`:
/// Linux's `access_ok`, which also refuses addresses below `USER_MIN`). Inside
/// it, a chunk (4 KiB of the transfer) whose copy faults ends the transfer
/// there, the chunks before it counted. `EINVAL` if the transfer would end
/// past `i64::MAX` (`rw_verify_area`, on the whole count). Then `count`
/// capped at `MAX_SINGLE_IO`, as `read` and `write` cap it.
fn transfer_len(buf: UserPtr, pos: u64, count: u64) -> Result<usize, u32> {
    if !user_range_valid(buf.addr(), usize::try_from(count).unwrap_or(usize::MAX)) {
        return Err(errno::EFAULT);
    }
    if pos.checked_add(count).is_none_or(|end| end > i64::MAX as u64) {
        return Err(errno::EINVAL);
    }
    Ok(count.min(MAX_SINGLE_IO as u64) as usize)
}

/// `pread64(fd, buf, count, offset)` (17).
pub(super) fn sys_pread64(fd: Fd, buf: UserPtr, count: u64, offset: u64) -> u64 {
    finish(pread64(fd, buf, count, offset))
}

fn pread64(fd: Fd, buf: UserPtr, count: u64, offset: u64) -> Result<u64, u32> {
    let pos = nonnegative(offset)?;
    let file = positional_file(fd)?;
    file.require_readable().map_err(vfs)?;
    let limit = transfer_len(buf, pos, count)?;
    if file.inode.inode_type() == InodeType::Dir {
        return Err(errno::EISDIR);
    }
    read_at_into_user(&*file.inode, pos, buf.addr(), limit)
}

/// The read side's chunk loop: up to `limit` bytes of `inode` from `pos`
/// into user memory at `user_buf`, a chunk at a time until EOF or a short
/// read. A read error or a copy out that faults ends it: the bytes moved
/// by then if any, else the errno.
fn read_at_into_user(inode: &dyn VfsInode, pos: u64, user_buf: u64, limit: usize) -> Result<u64, u32> {
    let mut kbuf = [0u8; FILE_CHUNK];
    let mut done = 0usize;
    while done < limit {
        let want = (limit - done).min(FILE_CHUNK);
        let Some(addr) = user_buf.checked_add(done as u64) else {
            return partial(done, errno::EFAULT);
        };
        let n = match inode.read_at(pos + done as u64, &mut kbuf[..want]) {
            Ok(n) => n,
            Err(e) => return partial(done, vfs(e)),
        };
        // As `write_chunks_from_user`: never more than asked for.
        debug_assert!(n <= want, "read {n} into a {want}-byte chunk");
        let n = n.min(want);
        if n == 0 {
            break;
        }
        if copy_to_user(addr, &kbuf[..n]).is_err() {
            return partial(done, errno::EFAULT);
        }
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done as u64)
}

/// `pwrite64(fd, buf, count, offset)` (18).
///
/// On an `O_APPEND` file every chunk goes to the end through
/// `VfsInode::append` and `offset` is ignored, as Linux does (`pwrite(2)`,
/// BUGS); the open file's offset stays where it was either way.
pub(super) fn sys_pwrite64(fd: Fd, buf: UserPtr, count: u64, offset: u64) -> u64 {
    finish(pwrite64(fd, buf, count, offset))
}

fn pwrite64(fd: Fd, buf: UserPtr, count: u64, offset: u64) -> Result<u64, u32> {
    let pos = nonnegative(offset)?;
    let file = positional_file(fd)?;
    file.require_writable().map_err(vfs)?;
    let limit = transfer_len(buf, pos, count)?;
    let append = file.status_flags() & OpenFlags::O_APPEND != 0;
    write_chunks_from_user(buf.addr(), limit, |done, chunk| {
        if append { file.inode.append(chunk).map(|(_, n)| n) } else { file.inode.write_at(pos + done as u64, chunk) }
    })
}

/// Whether `fsync` can sync `inode`: a regular file or directory. A
/// terminal, a character-device node, a FIFO, a socket or a block-device
/// node is `EINVAL`, whatever its filesystem's `VfsInode::sync` would say
/// (devfs's says `Ok`). Linux has no `fsync` for the first four; a block
/// device's syncs the device there (`blkdev_fsync`), which M7 can't open.
/// Anything on procfs is refused by procfs's own `sync`.
fn syncable(inode: &dyn VfsInode) -> bool {
    !inode.is_tty() && matches!(inode.inode_type(), InodeType::File | InodeType::Dir)
}

/// `fsync(fd)` and `fdatasync(fd)`: the inode's `VfsInode::sync`, its data
/// alone (and the metadata needed to read it back) if `data_only`; ext4
/// still writes all of an inode's dirty metadata either way. Any open file
/// but `O_PATH` (`EBADF`), whatever its access mode.
///
/// A failure comes back as the filesystem gave it (`EIO` for a device
/// error). On ext4 a page whose writeback failed stays dirty in the page
/// cache, and a block cache whose sync failed keeps its blocks dirty: the
/// next sync writes either.
fn fsync(fd: Fd, data_only: bool) -> Result<u64, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    if !syncable(&*file.inode) {
        return Err(errno::EINVAL);
    }
    file.inode.sync(data_only).map_err(vfs)?;
    Ok(0)
}

/// `fsync(fd)` (74).
pub(super) fn sys_fsync(fd: Fd) -> u64 {
    finish(fsync(fd, false))
}

/// `fdatasync(fd)` (75).
pub(super) fn sys_fdatasync(fd: Fd) -> u64 {
    finish(fsync(fd, true))
}

/// `sync()` (162): every mounted filesystem flushed (`vfs::sync_all`).
/// Always 0, as Linux's: `sync_all` keeps going past a failure, and each
/// filesystem logs its own (`FileSystem::sync`).
pub(super) fn sys_sync() -> u64 {
    let _ = crate::vfs::sync_all();
    0
}

/// `msync(addr, length, flags)` (26): M7 stub (#255).
pub(super) fn sys_msync(_addr: u64, _length: u64, _flags: i32) -> u64 {
    not_implemented(general::__NR_msync)
}

crate::ktest!(
    fn io_m7_stubs_return_enosys() {
        super::check_stubs(&[general::__NR_msync])
    }
);

#[cfg(feature = "ktest")]
use super::at::{self, Mode};
#[cfg(feature = "ktest")]
use super::fd::{ktest_fd, ktest_flags, ktest_user_path, sys_close, sys_lseek, sys_open, sys_write};
#[cfg(feature = "ktest")]
use super::neg_errno;
#[cfg(feature = "ktest")]
use crate::linux::process;
#[cfg(feature = "ktest")]
use crate::user::copy_from_user;

/// `len` bytes of user memory at `addr` (ktests).
#[cfg(feature = "ktest")]
fn ktest_user_bytes(addr: u64, len: usize) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let mut out = alloc::vec![0u8; len];
    copy_from_user(&mut out, addr).map_err(|_| "copy out of user memory")?;
    Ok(out)
}

/// A recognisable `len`-byte pattern, never zero (so a hole can't pass for it).
#[cfg(feature = "ktest")]
fn ktest_pattern(len: usize, seed: u8) -> alloc::vec::Vec<u8> {
    (0..len).map(|i| ((i * 7 + usize::from(seed)) % 251) as u8 | 1).collect()
}

/// Open `inode` (on a volume no path reaches) on a new descriptor of the
/// current process, with access `flags` (ktests).
#[cfg(feature = "ktest")]
fn ktest_install(inode: &alloc::sync::Arc<dyn VfsInode>, flags: u32) -> Result<Fd, &'static str> {
    let file = process::KernelFile::new_unchecked_flags(
        alloc::sync::Arc::clone(inode),
        "/io-ktest-volume/file".into(),
        ktest_flags(flags),
    )
    .map_err(|_| "open the volume's file")?;
    let raw = process::install_current_fd(file, false).map_err(|_| "install the fd")?;
    Ok(at::reg(u64::from(raw as u32)))
}

/// The file `name` at the root of a read-only mount of `vol`'s device bytes
/// (`Ext4Volume::mount_device_copy_ro` on device `0:minor`): `None` if the
/// device doesn't hold it yet (ktests).
#[cfg(feature = "ktest")]
fn ktest_on_device(
    vol: &crate::vfs::page_cache::ktest_files::Ext4Volume,
    minor: u32,
    name: &str,
) -> Result<Option<alloc::vec::Vec<u8>>, &'static str> {
    let root = vol.mount_device_copy_ro(minor)?;
    let f = match root.lookup(name) {
        Ok(f) => f,
        Err(lumen_vfs::VfsError::NoEnt) => return Ok(None),
        Err(_) => return Err("lookup on the device copy"),
    };
    let size = f.stat().map_err(|_| "stat on the device copy")?.size as usize;
    let mut data = alloc::vec![0u8; size];
    crate::kassert_eq!(f.read_at(0, &mut data), Ok(size));
    Ok(Some(data))
}

crate::ktest!(
    fn io_pread_pwrite_at_offsets_leave_the_offset_alone() {
        // Transfers starting mid-page and over 4 KiB (crossing a page and a
        // bounce chunk), a hole past EOF reading as zeros, the fd's offset
        // unchanged throughout, `EINVAL` offsets and a fault mid-transfer.
        use crate::{kassert, kassert_eq};
        const LEN: usize = 6000;
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA;
            let back = base + 8192;
            let last_page = base + process::KTEST_USER_LEN - 4096;
            let path = ktest_user_path(0, b"/tmp/io-ktest-rw")?;
            let flags = ktest_flags(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
            let fd = ktest_fd(sys_open(path, flags, at::reg(0o644)))?;
            copy_to_user(base, b"0123456789").map_err(|_| "copy data")?;
            kassert_eq!(sys_write(fd, at::reg(base), 10), 10);
            // 6000 bytes at 4000: past EOF (10), across the page at 4096
            // and the next chunk's edge at 8096.
            let pattern = ktest_pattern(LEN, 3);
            copy_to_user(base, &pattern).map_err(|_| "copy the pattern")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), LEN as u64, 4000), LEN as u64);
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 10);
            let size = || crate::vfs::resolve_path_from_root("/tmp/io-ktest-rw").and_then(|i| i.stat()).map(|m| m.size);
            kassert_eq!(size(), Ok(10_000));
            // Read back whole, and the hole between 10 and 4000 is zeros.
            copy_to_user(back, &[0xaau8; LEN]).map_err(|_| "fill the buffer")?;
            kassert_eq!(sys_pread64(fd, at::reg(back), LEN as u64, 4000), LEN as u64);
            kassert!(ktest_user_bytes(back, LEN)? == pattern);
            kassert_eq!(sys_pread64(fd, at::reg(back), 4000, 0), 4000);
            let head = ktest_user_bytes(back, 4000)?;
            kassert_eq!(&head[..10], b"0123456789");
            kassert!(head[10..].iter().all(|&b| b == 0));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 10);
            // A write far past EOF: the file grows to it, a hole before it.
            copy_to_user(base, b"XYZ").map_err(|_| "copy data")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 3, 20_000), 3);
            kassert_eq!(size(), Ok(20_003));
            kassert_eq!(sys_pread64(fd, at::reg(back), 5000, 15_003), 5000);
            let tail = ktest_user_bytes(back, 5000)?;
            kassert!(tail[..4997].iter().all(|&b| b == 0));
            kassert_eq!(&tail[4997..], b"XYZ");
            // At or past EOF: 0; a count of 0: 0.
            kassert_eq!(sys_pread64(fd, at::reg(back), 10, 20_003), 0);
            kassert_eq!(sys_pread64(fd, at::reg(back), 10, 1 << 40), 0);
            kassert_eq!(sys_pread64(fd, at::reg(back), 0, 4000), 0);
            // The offset never moved: `read` starts at 10.
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 10);
            kassert_eq!(super::fd::sys_read(fd, at::reg(back), 3), 3);
            kassert_eq!(ktest_user_bytes(back, 3)?, b"\0\0\0");
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 13);
            // A negative offset: `EINVAL`, even on a closed fd (checked
            // first, as Linux).
            let neg = -1i64 as u64;
            let max = i64::MAX as u64;
            let kernel_half = 0xffff_8000_0000_0000u64;
            kassert_eq!(sys_pread64(fd, at::reg(back), 1, neg), neg_errno(errno::EINVAL));
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 1, neg), neg_errno(errno::EINVAL));
            kassert_eq!(sys_pread64(at::reg(99), at::reg(back), 1, neg), neg_errno(errno::EINVAL));
            // An end past `i64::MAX`: `EINVAL`, but after the fd's checks
            // and the buffer's range (a closed fd `EBADF`, a buffer out of
            // user space `EFAULT`); 0 bytes at `i64::MAX` are fine.
            kassert_eq!(sys_pread64(fd, at::reg(back), 1, max), neg_errno(errno::EINVAL));
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 1, max), neg_errno(errno::EINVAL));
            kassert_eq!(sys_pread64(at::reg(99), at::reg(back), 1, max), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(fd, at::reg(kernel_half), 1, max), neg_errno(errno::EFAULT));
            kassert_eq!(sys_pread64(fd, at::reg(back), 1 << 62, 1 << 62), neg_errno(errno::EFAULT));
            kassert_eq!(sys_pread64(fd, at::reg(back), 0, max), 0);
            kassert_eq!(size(), Ok(20_003));
            // The last mapped page, then nothing: the first chunk moves, the
            // second faults; the offset stays put.
            kassert_eq!(sys_pread64(fd, at::reg(last_page), 8192, 4000), 4096);
            kassert!(ktest_user_bytes(last_page, 4096)?[..] == pattern[..4096]);
            kassert_eq!(sys_pwrite64(fd, at::reg(last_page), 8192, 30_000), 4096);
            kassert_eq!(size(), Ok(34_096));
            kassert_eq!(sys_pread64(fd, at::reg(back), 4096, 30_000), 4096);
            kassert!(ktest_user_bytes(back, 4096)?[..] == pattern[..4096]);
            // `EFAULT`, nothing moved: a first chunk whose copy faults (an
            // unmapped page in user space), or a buffer not wholly in user
            // space (in the kernel half, or below `USER_MIN`).
            let unmapped = last_page + 4096;
            for bad in [unmapped, kernel_half, 0] {
                kassert_eq!(sys_pread64(fd, at::reg(bad), 4, 4000), neg_errno(errno::EFAULT));
                kassert_eq!(sys_pwrite64(fd, at::reg(bad), 4, 40_000), neg_errno(errno::EFAULT));
            }
            kassert_eq!(size(), Ok(34_096));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 13);
            kassert_eq!(sys_close(fd), 0);
            kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/io-ktest-rw", false).is_ok());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn io_pwrite_appends_on_o_append_and_positional_errnos() {
        // `O_APPEND`: the given offset ignored, every write at the end, the
        // fd's offset unchanged. Then each refusal, in Linux's order.
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA + 2048;
            let out = process::KTEST_USER_VA + 8192;
            let kernel_half = 0xffff_8000_0000_0000u64;
            let max = i64::MAX as u64;
            let mode: Mode = at::reg(0o644);
            let path = ktest_user_path(0, b"/tmp/io-ktest-append")?;
            let creat = ktest_flags(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
            let w = ktest_fd(sys_open(path, creat, mode))?;
            copy_to_user(base, b"hello").map_err(|_| "copy data")?;
            kassert_eq!(sys_write(w, at::reg(base), 5), 5);
            let app = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_RDWR | OpenFlags::O_APPEND), mode))?;
            copy_to_user(base, b"AB").map_err(|_| "copy data")?;
            kassert_eq!(sys_pwrite64(app, at::reg(base), 2, 0), 2);
            kassert_eq!(sys_lseek(app, 0, general::SEEK_CUR), 0);
            kassert_eq!(sys_pwrite64(app, at::reg(base + 1), 1, 1), 1);
            kassert_eq!(sys_lseek(app, 0, general::SEEK_CUR), 0);
            kassert_eq!(sys_pread64(app, at::reg(out), 16, 0), 8);
            kassert_eq!(ktest_user_bytes(out, 8)?, b"helloABB");
            // A terminal has no position: `ESPIPE`, before the access mode
            // (fd 0 is read-only, 1 write-only), on `/dev/tty` too.
            for fd in [0u64, 1] {
                kassert_eq!(sys_pread64(at::reg(fd), at::reg(out), 1, 0), neg_errno(errno::ESPIPE));
                kassert_eq!(sys_pwrite64(at::reg(fd), at::reg(base), 1, 0), neg_errno(errno::ESPIPE));
            }
            let tty = ktest_user_path(1, b"/dev/tty")?;
            let tty_fd = ktest_fd(sys_open(tty, ktest_flags(OpenFlags::O_RDWR), at::reg(0)))?;
            kassert_eq!(sys_pread64(tty_fd, at::reg(out), 1, 0), neg_errno(errno::ESPIPE));
            kassert_eq!(sys_pwrite64(tty_fd, at::reg(base), 1, 0), neg_errno(errno::ESPIPE));
            // The wrong direction, `O_PATH`, or not open: `EBADF`.
            let rd = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_RDONLY), at::reg(0)))?;
            let opath = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_PATH), at::reg(0)))?;
            kassert_eq!(sys_pwrite64(rd, at::reg(base), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(w, at::reg(out), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(opath, at::reg(out), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pwrite64(opath, at::reg(base), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(at::reg(99), at::reg(out), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pwrite64(at::reg(99), at::reg(base), 1, 0), neg_errno(errno::EBADF));
            // ... before the buffer's range and the end past `i64::MAX`.
            kassert_eq!(sys_pwrite64(rd, at::reg(kernel_half), 1, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(w, at::reg(out), 1, max), neg_errno(errno::EBADF));
            kassert_eq!(sys_pread64(rd, at::reg(out), 16, 5), 3);
            kassert_eq!(ktest_user_bytes(out, 3)?, b"ABB");
            // A directory: `EISDIR`, even for 0 bytes, but after the
            // buffer's range (`EFAULT`) and the end (`EINVAL`).
            let tmp = ktest_user_path(2, b"/tmp")?;
            let dir = ktest_fd(sys_open(tmp, ktest_flags(OpenFlags::O_RDONLY), at::reg(0)))?;
            kassert_eq!(sys_pread64(dir, at::reg(out), 1, 0), neg_errno(errno::EISDIR));
            kassert_eq!(sys_pread64(dir, at::reg(out), 0, 0), neg_errno(errno::EISDIR));
            kassert_eq!(sys_pread64(dir, at::reg(kernel_half), 1, 0), neg_errno(errno::EFAULT));
            kassert_eq!(sys_pread64(dir, at::reg(out), 1, max), neg_errno(errno::EINVAL));
            // Through `dispatch`, each argument from its own register (the
            // offset from r10, the fd from rdi).
            let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64| {
                let mut frame = crate::arch::syscall::SyscallFrame {
                    rax: u64::from(nr),
                    rdi,
                    rsi,
                    rdx,
                    r10,
                    r8: 0x5a5a,
                    r9: 0x5a5a,
                    ..Default::default()
                };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            let raw = |fd: Fd| u64::from(fd.raw() as u32);
            kassert_eq!(call(general::__NR_pread64, raw(rd), out, 2, 6), 2);
            kassert_eq!(ktest_user_bytes(out, 2)?, b"BB");
            copy_to_user(base, b"Q").map_err(|_| "copy data")?;
            kassert_eq!(call(general::__NR_pwrite64, raw(w), base, 1, 7), 1);
            copy_to_user(base, b"C").map_err(|_| "copy data")?;
            kassert_eq!(call(general::__NR_pwrite64, raw(app), base, 1, 3), 1);
            kassert_eq!(call(general::__NR_fsync, raw(app), 0, 0, 0), 0);
            kassert_eq!(call(general::__NR_fsync, 99, 0, 0, 0), neg_errno(errno::EBADF));
            kassert_eq!(call(general::__NR_fdatasync, raw(w), 0, 0, 0), 0);
            kassert_eq!(call(general::__NR_fdatasync, 99, 0, 0, 0), neg_errno(errno::EBADF));
            kassert_eq!(call(general::__NR_sync, 0, 0, 0, 0), 0);
            kassert_eq!(sys_pread64(rd, at::reg(out), 16, 0), 9);
            kassert_eq!(ktest_user_bytes(out, 9)?, b"helloABQC");
            // An append longer than a chunk: every chunk at the end, in
            // order, read back from the old end.
            let big = ktest_pattern(6000, 5);
            copy_to_user(base, &big).map_err(|_| "copy the pattern")?;
            kassert_eq!(sys_pwrite64(app, at::reg(base), 6000, 0), 6000);
            kassert_eq!(sys_lseek(app, 0, general::SEEK_CUR), 0);
            kassert_eq!(sys_pread64(rd, at::reg(out), 8000, 9), 6000);
            kassert!(ktest_user_bytes(out, 6000)? == big);
            kassert_eq!(sys_pread64(rd, at::reg(out), 9, 0), 9);
            kassert_eq!(ktest_user_bytes(out, 9)?, b"helloABQC");
            kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/io-ktest-append", false).is_ok());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn io_pread_pwrite_report_filesystem_errors() {
        // A disk read failing: `EIO`, not a 0 that reads as EOF. A tmpfs out
        // of room: the bytes written before it, then `ENOSPC`.
        use crate::vfs::page_cache::ktest_files::{Ext4Volume, TmpfsVolume};
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA;
            let vol = Ext4Volume::new(193)?;
            let inode = vol.root.create("f", 0o644).map_err(|_| "ext4 create")?;
            let pattern = ktest_pattern(8192, 1);
            kassert_eq!(inode.write_at(0, &pattern), Ok(8192));
            inode.sync(false).map_err(|_| "sync the ext4 file")?;
            // Read through a fresh read-only mount, so the pages come from
            // the disk: page 1 (offset 4096) with every disk read failing.
            let ro = vol.remount_ro(194)?.lookup("f").map_err(|_| "lookup on the read-only mount")?;
            let fd = ktest_install(&ro, OpenFlags::O_RDONLY)?;
            vol.fail_reads(true);
            let failed = sys_pread64(fd, at::reg(base), 4, 4096);
            vol.fail_reads(false);
            kassert_eq!(failed, neg_errno(errno::EIO));
            kassert_eq!(sys_pread64(fd, at::reg(base), 4, 4096), 4);
            kassert!(ktest_user_bytes(base, 4)?[..] == pattern[4096..4100]);
            kassert_eq!(sys_close(fd), 0);
            // A tmpfs of two pages: 12000 bytes write the first 8192 (two
            // whole chunks), then nothing fits.
            let tmpfs = TmpfsVolume::new(195, 2 * 4096);
            let small = tmpfs.root.create("f", 0o644).map_err(|_| "tmpfs create")?;
            let fd = ktest_install(&small, OpenFlags::O_RDWR)?;
            let big = ktest_pattern(12_000, 2);
            copy_to_user(base, &big).map_err(|_| "copy the pattern")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 12_000, 0), 8192);
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 100, 8192), neg_errno(errno::ENOSPC));
            kassert_eq!(small.stat().map(|m| m.size), Ok(8192));
            kassert_eq!(sys_pread64(fd, at::reg(base + 12_288), 4096, 4096), 4096);
            kassert!(ktest_user_bytes(base + 12_288, 4096)?[..] == big[4096..8192]);
            kassert_eq!(sys_close(fd), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn io_fsync_writes_an_ext4_file_back_and_reports_eio() {
        // On a fresh ext4 volume: what a write left in the page cache is not
        // on the device until `fsync`, and is after it (read back through a
        // read-only mount of a copy of the device's bytes, under the block
        // cache). Writes into the block cache failing, or its sync alone:
        // `EIO`, the device unchanged, and the next sync writes it all. An
        // open file unlinked since: `fsync` is 0 (nothing to write).
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        use crate::{kassert, kassert_eq};
        const LEN: usize = 6000;
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA;
            let vol = Ext4Volume::new(186)?;
            let inode = vol.root.create("f", 0o644).map_err(|_| "ext4 create")?;
            let fd = ktest_install(&inode, OpenFlags::O_RDWR)?;
            let on_device = |minor: u32| ktest_on_device(&vol, minor, "f");
            let pattern = ktest_pattern(LEN, 9);
            copy_to_user(base, &pattern).map_err(|_| "copy the pattern")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), LEN as u64, 0), LEN as u64);
            kassert!(on_device(187)?.is_none_or(|d| d.is_empty()));
            kassert_eq!(sys_fsync(fd), 0);
            kassert_eq!(on_device(188)?, Some(pattern.clone()));
            // Overwrite the start with writes failing: `EIO` from both, the
            // device unchanged; once they work, `fdatasync` writes it.
            copy_to_user(base, b"ZZZZ").map_err(|_| "copy data")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 4, 0), 4);
            vol.fail_writes(true);
            let failed = (sys_fsync(fd), sys_fdatasync(fd));
            vol.fail_writes(false);
            kassert_eq!(failed, (neg_errno(errno::EIO), neg_errno(errno::EIO)));
            kassert_eq!(on_device(189)?, Some(pattern.clone()));
            kassert_eq!(sys_fdatasync(fd), 0);
            let mut want = pattern.clone();
            want[..4].copy_from_slice(b"ZZZZ");
            kassert_eq!(on_device(190)?, Some(want.clone()));
            // Again with only the block cache's sync failing: the page goes
            // into the cache, which keeps it; the next `fsync` flushes it.
            copy_to_user(base, b"YY").map_err(|_| "copy data")?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 2, 4096), 2);
            vol.fail_syncs(true);
            let failed = sys_fsync(fd);
            vol.fail_syncs(false);
            kassert_eq!(failed, neg_errno(errno::EIO));
            kassert_eq!(on_device(191)?, Some(want.clone()));
            kassert_eq!(sys_fsync(fd), 0);
            want[4096..4098].copy_from_slice(b"YY");
            kassert_eq!(on_device(192)?, Some(want));
            kassert_eq!(sys_close(fd), 0);
            // Open, written, then unlinked: detached, so `fsync` has nothing
            // to write and is 0 (ARCHITECTURE section 8, `fsync`).
            let gone = vol.root.create("g", 0o644).map_err(|_| "ext4 create g")?;
            let fd = ktest_install(&gone, OpenFlags::O_RDWR)?;
            kassert_eq!(sys_pwrite64(fd, at::reg(base), 100, 0), 100);
            vol.root.unlink("g").map_err(|_| "unlink g")?;
            kassert_eq!(sys_fsync(fd), 0);
            kassert_eq!(sys_fdatasync(fd), 0);
            kassert_eq!(sys_close(fd), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn io_fsync_refuses_terminals_devices_and_procfs() {
        // A file or directory on tmpfs syncs (`0`, read-only too); a
        // terminal, a device node, a procfs file and a procfs directory are
        // `EINVAL`; `O_PATH` and a closed fd `EBADF`.
        use crate::kassert_eq;
        process::in_test_process(|_| {
            let rd = ktest_flags(OpenFlags::O_RDONLY);
            let path = ktest_user_path(0, b"/tmp/io-ktest-fsync")?;
            let creat = ktest_flags(OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
            let w = ktest_fd(sys_open(path, creat, at::reg(0o644)))?;
            let r = ktest_fd(sys_open(path, rd, at::reg(0)))?;
            let opath = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_PATH), at::reg(0)))?;
            let tmp = ktest_user_path(1, b"/tmp")?;
            let dir = ktest_fd(sys_open(tmp, rd, at::reg(0)))?;
            let null = ktest_user_path(2, b"/dev/null")?;
            let null_fd = ktest_fd(sys_open(null, ktest_flags(OpenFlags::O_RDWR), at::reg(0)))?;
            let version = ktest_user_path(3, b"/proc/version")?;
            let proc_fd = ktest_fd(sys_open(version, rd, at::reg(0)))?;
            let proc_dir = ktest_user_path(4, b"/proc")?;
            let proc_dir_fd = ktest_fd(sys_open(proc_dir, rd, at::reg(0)))?;
            for fd in [w, r, dir] {
                kassert_eq!(sys_fsync(fd), 0);
                kassert_eq!(sys_fdatasync(fd), 0);
            }
            for fd in [at::reg(0), at::reg(1), null_fd, proc_fd, proc_dir_fd] {
                kassert_eq!(sys_fsync(fd), neg_errno(errno::EINVAL));
                kassert_eq!(sys_fdatasync(fd), neg_errno(errno::EINVAL));
            }
            for fd in [opath, at::reg(99)] {
                kassert_eq!(sys_fsync(fd), neg_errno(errno::EBADF));
                kassert_eq!(sys_fdatasync(fd), neg_errno(errno::EBADF));
            }
            crate::kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/io-ktest-fsync", false).is_ok());
            Ok(())
        })?
    }
);

/// Mount `vol` at `path` (a new directory) in the global tables for `f`,
/// then unmount it and remove the directory, whatever `f` returned
/// (ktests).
#[cfg(feature = "ktest")]
fn ktest_with_mounted(
    vol: &crate::vfs::page_cache::ktest_files::Ext4Volume,
    path: &str,
    f: impl FnOnce() -> crate::ktest::TestResult,
) -> crate::ktest::TestResult {
    use lumen_vfs::{FileSystem, mkdir_at, unlink_at};
    let tables = crate::vfs::tables();
    let mp = mkdir_at(tables, None, path.as_bytes(), 0o755, 0).map_err(|_| "mkdir the mountpoint")?;
    let (mp_dev, mp_ino) = (mp.dev_id(), mp.ino());
    let fs = vol.fs() as alloc::sync::Arc<dyn FileSystem>;
    if tables.mount(mp, path, fs).is_err() {
        let _ = unlink_at(tables, None, path.as_bytes(), true);
        return Err("mount the volume");
    }
    let result = f();
    let unmounted = tables.umount(mp_dev, mp_ino).map(drop).map_err(|_| "unmount the volume");
    let removed = unlink_at(tables, None, path.as_bytes(), true).map_err(|_| "rmdir the mountpoint");
    result.and(unmounted).and(removed)
}

crate::ktest!(
    fn io_sync_flushes_every_mounted_filesystem() {
        // Two ext4 volumes mounted in the global tables, a file written on
        // each without `fsync`: `sync` puts the first's on its device, and
        // is 0 though the second's block cache can't sync (its data stays
        // off its device).
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA + 2048;
            let good = Ext4Volume::new(196)?;
            let bad = Ext4Volume::new(197)?;
            ktest_with_mounted(&good, "/tmp/io-sync-good", || {
                ktest_with_mounted(&bad, "/tmp/io-sync-bad", || {
                    let creat = ktest_flags(OpenFlags::O_RDWR | OpenFlags::O_CREAT);
                    let pattern = ktest_pattern(5000, 4);
                    copy_to_user(base, &pattern).map_err(|_| "copy the pattern")?;
                    let mut fds = alloc::vec::Vec::new();
                    for path in [&b"/tmp/io-sync-good/f"[..], b"/tmp/io-sync-bad/f"] {
                        let fd = ktest_fd(sys_open(ktest_user_path(0, path)?, creat, at::reg(0o644)))?;
                        kassert_eq!(sys_pwrite64(fd, at::reg(base), 5000, 0), 5000);
                        fds.push(fd);
                    }
                    kassert!(ktest_on_device(&good, 198, "f")?.is_none_or(|d| d.is_empty()));
                    bad.fail_syncs(true);
                    let synced = sys_sync();
                    bad.fail_syncs(false);
                    kassert_eq!(synced, 0);
                    kassert_eq!(ktest_on_device(&good, 199, "f")?, Some(pattern));
                    kassert!(ktest_on_device(&bad, 200, "f")?.is_none_or(|d| d.is_empty()));
                    for fd in fds {
                        kassert_eq!(sys_close(fd), 0);
                    }
                    Ok(())
                })
            })
        })?
    }
);
