//! Descriptor I/O: `read`, `write`, `close`, `fstat`, `poll`, `ioctl`,
//! `readv`, `writev` (M6, through the process's FD table since #254), and
//! M7's `open`, `openat`, `lseek`.
//!
//! Every descriptor is an open file description in the process's
//! `FdTable`. A terminal's (`VfsInode::is_tty`: `/dev/console`, which fds 0,
//! 1 and 2 start on, and `/dev/tty`) reads and writes the process's console
//! as M6 did (`read_into_user`, `write_from_user`), takes the tty `ioctl`s
//! and polls on the console's readiness; any other goes to its inode at the
//! file's offset, through a bounce buffer on the stack. No VFS lock, nor
//! the offset's (Lock 2), is held across a copy to or from user memory.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use super::at::{self, Dirfd, Fd, Mode, UserPath, UserPtr};
use super::{
    DEFAULT_RLIMIT_NOFILE, MAX_SINGLE_IO, copy_iovecs, copy_pod_from_user, copy_pod_to_user, finish, neg_errno,
    read_into_user, write_from_user,
};
use crate::linux::process::{self, ConsoleIo, FileRef, KernelFile, with_current, with_current_mut};
use crate::linux::{errno, general, ioctl};
use crate::sched;
use crate::user::{CopyError, copy_from_user, copy_to_user};
use lumen_vfs::{InodeType, OpenFile, OpenFlags, SeekFrom, VfsError, VfsLock, open_inode_guarded};

/// Bytes moved per step between a file and user memory: the bounce buffer,
/// on the stack (no heap, so a run's frame count is unchanged).
const FILE_CHUNK: usize = 4096;

/// Raw Linux x86-64 `struct pollfd` layout (8 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// A VFS error as the positive errno a handler returns.
fn vfs(e: VfsError) -> u32 {
    e.to_errno()
}

/// The open file on `fd`, or `EBADF`.
fn file(fd: Fd) -> Result<FileRef, u32> {
    process::current_file(fd.raw()).ok_or(errno::EBADF)
}

fn current_io() -> ConsoleIo {
    with_current(|p| p.io)
}

/// `n` bytes moved before an error: the count if any, else the error.
fn partial(done: usize, err: u32) -> Result<u64, u32> {
    if done > 0 { Ok(done as u64) } else { Err(err) }
}

/// Read up to `count` bytes (at most `MAX_SINGLE_IO`) from `file` at its
/// offset into user memory at `user_buf`, a chunk at a time until EOF or a
/// short read. The offset's lock is held across each chunk's `read_at` and
/// released before its copy out; a chunk the copy refuses is given back
/// (the offset moves only by what reached the caller).
pub(super) fn read_file(file: &KernelFile, user_buf: u64, count: u64) -> Result<u64, u32> {
    if file.inode.inode_type() == InodeType::Dir {
        return Err(errno::EISDIR);
    }
    let limit = count.min(MAX_SINGLE_IO as u64) as usize;
    let mut kbuf = [0u8; FILE_CHUNK];
    let mut done = 0usize;
    while done < limit {
        let want = (limit - done).min(FILE_CHUNK);
        let Some(addr) = user_buf.checked_add(done as u64) else {
            return partial(done, errno::EFAULT);
        };
        let (pos, end) = {
            let mut off = file.offset.lock();
            let pos = *off;
            let n = match file.inode.read_at(pos, &mut kbuf[..want]) {
                Ok(n) => n,
                Err(e) => return partial(done, vfs(e)),
            };
            let end = pos.saturating_add(n as u64);
            *off = end;
            (pos, end)
        };
        let n = (end - pos) as usize;
        if n == 0 {
            break;
        }
        if copy_to_user(addr, &kbuf[..n]).is_err() {
            let mut off = file.offset.lock();
            if *off == end {
                *off = pos;
            }
            return partial(done, errno::EFAULT);
        }
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done as u64)
}

/// Write up to `count` bytes (at most `MAX_SINGLE_IO`) from user memory at
/// `user_buf` to `file` at its offset (at its end with `O_APPEND`), a chunk
/// at a time: each copied in first, then written under the offset's lock.
pub(super) fn write_file(file: &KernelFile, user_buf: u64, count: u64) -> Result<u64, u32> {
    let limit = count.min(MAX_SINGLE_IO as u64) as usize;
    let append = file.status_flags() & OpenFlags::O_APPEND != 0;
    let mut kbuf = [0u8; FILE_CHUNK];
    let mut done = 0usize;
    while done < limit {
        let want = (limit - done).min(FILE_CHUNK);
        let Some(addr) = user_buf.checked_add(done as u64) else {
            return partial(done, errno::EFAULT);
        };
        if copy_from_user(&mut kbuf[..want], addr).is_err() {
            return partial(done, errno::EFAULT);
        }
        let n = {
            let mut off = file.offset.lock();
            let pos = if append {
                match file.inode.stat() {
                    Ok(m) => m.size,
                    Err(e) => return partial(done, vfs(e)),
                }
            } else {
                *off
            };
            let n = match file.inode.write_at(pos, &kbuf[..want]) {
                Ok(n) => n,
                Err(e) => return partial(done, vfs(e)),
            };
            *off = pos.saturating_add(n as u64);
            n
        };
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done as u64)
}

pub(super) fn sys_read(fd: Fd, buf: UserPtr, count: u64) -> u64 {
    finish(read(fd, buf, count))
}

/// A terminal's read: M6's console read (blocking for the first byte, then
/// what is queued), except that with `O_NONBLOCK` (`FIONBIO`) and no input
/// waiting it is `-EAGAIN` instead of a wait.
fn read_tty(file: &KernelFile, user_buf: u64, count: u64) -> Result<u64, u32> {
    let io = current_io();
    if count != 0 && file.status_flags() & OpenFlags::O_NONBLOCK != 0 && !process::stdin_ready(io) {
        return Err(errno::EAGAIN);
    }
    read_into_user(io, user_buf, count).map(|n| n as u64)
}

fn read(fd: Fd, buf: UserPtr, count: u64) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_readable().map_err(vfs)?;
    if file.inode.is_tty() {
        return read_tty(&file, buf.addr(), count);
    }
    read_file(&file, buf.addr(), count)
}

pub(super) fn sys_write(fd: Fd, buf: UserPtr, count: u64) -> u64 {
    finish(write(fd, buf, count))
}

fn write(fd: Fd, buf: UserPtr, count: u64) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_writable().map_err(vfs)?;
    if file.inode.is_tty() {
        return write_from_user(current_io(), buf.addr(), count).map(|n| n as u64);
    }
    write_file(&file, buf.addr(), count)
}

pub(super) fn sys_close(fd: Fd) -> u64 {
    finish(process::close_current_fd(fd.raw()).map(|()| 0).map_err(vfs))
}

pub(super) fn sys_fstat(fd: Fd, statbuf: UserPtr) -> u64 {
    finish(fstat(fd, statbuf))
}

fn fstat(fd: Fd, statbuf: UserPtr) -> Result<u64, u32> {
    let file = file(fd)?;
    let st = at::stat_from_metadata(&file.inode.stat().map_err(vfs)?);
    drop(file);
    // SAFETY: `general::stat` is 144 bytes with explicit `__pad0` and zero implicit padding.
    unsafe { copy_pod_to_user(statbuf.addr(), &st) }.map_err(CopyError::to_errno)?;
    Ok(0)
}

/// How `poll` treats a descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollClass {
    /// Not open (or `O_PATH`, which Linux's `poll` doesn't see): `POLLNVAL`.
    Invalid,
    /// A terminal: `POLLOUT` if writable at once, `POLLIN` once the console
    /// has input (if readable), as M6's console fds.
    Tty { readable: bool, writable: bool },
    /// Anything else (a file, a directory, `/proc`, `/dev/null`...): always
    /// ready both ways, as Linux's `DEFAULT_POLLMASK`.
    Ready,
}

fn poll_class(file: Option<&FileRef>) -> PollClass {
    match file {
        None => PollClass::Invalid,
        Some(f) if f.is_path() => PollClass::Invalid,
        Some(f) if f.inode.is_tty() => PollClass::Tty { readable: f.is_readable(), writable: f.is_writable() },
        Some(_) => PollClass::Ready,
    }
}

/// `poll(2)`: always-ready files at once; terminals readiness-driven, with
/// a timeout-aware wait for console input (Rust `std`'s
/// `sanitize_standard_fds`, `poll([0, 1, 2], 3, 0)`, and `busybox`
/// `lineedit`'s stdin waits). Pipes and PTYs arrive in M8.
pub(super) fn sys_poll(fds_ptr: u64, nfds: u64, timeout_raw: u64) -> u64 {
    if nfds > DEFAULT_RLIMIT_NOFILE {
        return neg_errno(errno::EINVAL);
    }
    let timeout_ms = timeout_raw as i32;
    let cnt = nfds as usize;
    if cnt == 0 {
        if timeout_ms > 0 {
            let _ = sched::sleep((timeout_ms as u64).saturating_mul(1_000_000));
        }
        return 0;
    }
    let byte_len = cnt * size_of::<RawPollFd>();
    let mut raw = vec![0u8; byte_len];
    if let Err(e) = copy_from_user(&mut raw, fds_ptr) {
        return neg_errno(e.to_errno());
    }
    let fd_at = |chunk: &[u8; 8]| i32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes"));
    let events_at = |chunk: &[u8; 8]| i16::from_le_bytes(chunk[4..6].try_into().expect("2 bytes"));

    // The polled files, cloned out in one hold of `ACTIVE_PROCS` and
    // classified (and dropped) after it.
    let (io, files): (ConsoleIo, Vec<Option<FileRef>>) = with_current(|p| {
        let files = raw.as_chunks::<8>().0.iter().map(|c| p.fds.get(fd_at(c)).map(|e| Arc::clone(&e.file))).collect();
        (p.io, files)
    });
    let classes: Vec<PollClass> = files.iter().map(|f| poll_class(f.as_ref())).collect();
    drop(files);

    let deadline = if timeout_ms > 0 {
        Some(crate::time::now_ns().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    } else {
        None
    };

    // Only touch stdin readiness if at least one entry actually asks for
    // POLLIN on a readable terminal.
    let pollin = general::POLLIN as i16;
    let pollout = general::POLLOUT as i16;
    let needs_stdin = raw.as_chunks::<8>().0.iter().zip(&classes).any(|(chunk, class)| {
        fd_at(chunk) >= 0 && matches!(class, PollClass::Tty { readable: true, .. }) && (events_at(chunk) & pollin) != 0
    });
    let always = (general::POLLIN | general::POLLOUT | general::POLLRDNORM | general::POLLWRNORM) as i16;

    let ready = loop {
        let mut ready = 0u64;
        let mut waiting_stdin = false;
        let stdin_avail = if needs_stdin { process::stdin_ready(io) } else { false };

        for (chunk, class) in raw.as_chunks_mut::<8>().0.iter_mut().zip(&classes) {
            let fd = fd_at(chunk);
            let events = events_at(chunk);
            let revents: i16 = if fd < 0 {
                0
            } else {
                match *class {
                    PollClass::Invalid => general::POLLNVAL as i16,
                    PollClass::Ready => events & always,
                    PollClass::Tty { readable, writable } => {
                        let mut r = if writable { events & pollout } else { 0 };
                        if readable && (events & pollin) != 0 {
                            if stdin_avail {
                                r |= pollin;
                            } else {
                                waiting_stdin = true;
                            }
                        }
                        r
                    }
                }
            };
            if revents != 0 {
                ready += 1;
            }
            chunk[6..8].copy_from_slice(&revents.to_le_bytes());
        }

        if ready > 0 || timeout_ms == 0 {
            break ready;
        }
        if !waiting_stdin {
            if let Some(dl) = deadline {
                let rem = dl.saturating_sub(crate::time::now_ns());
                if rem > 0 {
                    let _ = sched::sleep(rem);
                }
            }
            break 0;
        }
        if let Some(dl) = deadline
            && crate::time::now_ns() >= dl
        {
            break 0;
        }
        process::wait_stdin_step(io, deadline);
    };

    if let Err(e) = copy_to_user(fds_ptr, &raw) {
        return neg_errno(e.to_errno());
    }
    ready
}

pub(super) fn sys_ioctl(fd: Fd, req: u32, arg: u64) -> u64 {
    finish(ioctl(fd, req, arg))
}

fn ioctl(fd: Fd, cmd: u32, arg: u64) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_non_path().map_err(vfs)?;
    match cmd {
        ioctl::FIOCLEX | ioctl::FIONCLEX => {
            let cloexec = cmd == ioctl::FIOCLEX;
            with_current_mut(|p| p.fds.set_cloexec(fd.raw(), cloexec)).map_err(vfs)?;
            Ok(0)
        }
        ioctl::FIONBIO => {
            // SAFETY: an `int`: every bit pattern is a valid `i32`.
            let on = unsafe { copy_pod_from_user::<i32>(arg) }.map_err(CopyError::to_errno)?;
            file.set_nonblock(on != 0).map_err(vfs)?;
            Ok(0)
        }
        _ if file.inode.is_tty() => tty_ioctl(cmd, arg),
        _ => Err(errno::ENOTTY),
    }
}

/// A terminal's `ioctl`s (M6's): `TCGETS`, `TCSETS*`, `TIOCGWINSZ`.
fn tty_ioctl(cmd: u32, arg: u64) -> Result<u64, u32> {
    if cmd == ioctl::TCGETS {
        let t = with_current(|p| p.termios);
        // SAFETY: `general::termios` is 36 bytes with zero implicit padding.
        unsafe { copy_pod_to_user(arg, &t) }.map_err(CopyError::to_errno)?;
        return Ok(0);
    }
    if cmd == ioctl::TCSETS || cmd == ioctl::TCSETSW || cmd == ioctl::TCSETSF {
        // SAFETY: `general::termios` is 36 integer bytes; all bit patterns are valid.
        let t = unsafe { copy_pod_from_user::<general::termios>(arg) }.map_err(CopyError::to_errno)?;
        with_current_mut(|p| p.termios = t);
        return Ok(0);
    }
    if cmd == ioctl::TIOCGWINSZ {
        let (ws_row, ws_col) = match current_io() {
            ConsoleIo::Vt(_) => crate::term::status()
                .map(|t| {
                    let rows = u16::try_from(t.rows).ok().filter(|&n| n > 0).unwrap_or(25);
                    let cols = u16::try_from(t.cols).ok().filter(|&n| n > 0).unwrap_or(80);
                    (rows, cols)
                })
                .unwrap_or((25, 80)),
            ConsoleIo::Serial | ConsoleIo::Capture => (25, 80),
        };
        let ws = general::winsize { ws_row, ws_col, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `general::winsize` is 4 `u16`s (8 bytes) with zero padding.
        unsafe { copy_pod_to_user(arg, &ws) }.map_err(CopyError::to_errno)?;
        return Ok(0);
    }
    Err(errno::ENOTTY)
}

pub(super) fn sys_readv(fd: Fd, iov: UserPtr, iovcnt: u64) -> u64 {
    finish(readv(fd, iov, iovcnt))
}

fn readv(fd: Fd, iov: UserPtr, iovcnt: u64) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_readable().map_err(vfs)?;
    let iovs = copy_iovecs(iov.addr(), iovcnt)?;
    if file.inode.is_tty() {
        // Standard tty short-read semantics: return as soon as the first
        // non-empty span receives data (or EOF/error) so musl
        // `__stdio_read` never hangs trying to fill trailing iovecs.
        return match iovs.iter().find(|span| span.len != 0) {
            Some(span) => read_tty(&file, span.base, span.len),
            None => Ok(0),
        };
    }
    // Any other file: every span in turn, stopping at EOF (a short span).
    let mut total = 0usize;
    for span in iovs {
        if span.len == 0 {
            continue;
        }
        let room = MAX_SINGLE_IO - total;
        if room == 0 {
            break;
        }
        let want = span.len.min(room as u64);
        match read_file(&file, span.base, want) {
            Ok(n) => {
                total += n as usize;
                if n < want {
                    break;
                }
            }
            Err(e) => return partial(total, e),
        }
    }
    Ok(total as u64)
}

pub(super) fn sys_writev(fd: Fd, iov: UserPtr, iovcnt: u64) -> u64 {
    finish(writev(fd, iov, iovcnt))
}

fn writev(fd: Fd, iov: UserPtr, iovcnt: u64) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_writable().map_err(vfs)?;
    let iovs = copy_iovecs(iov.addr(), iovcnt)?;
    let tty = file.inode.is_tty().then(current_io);
    let mut total = 0usize;
    for span in iovs {
        if span.len == 0 {
            continue;
        }
        let room = MAX_SINGLE_IO - total;
        if room == 0 {
            break;
        }
        let want = span.len.min(room as u64);
        let wrote = match tty {
            Some(io) => write_from_user(io, span.base, want).map(|n| n as u64),
            None => write_file(&file, span.base, want),
        };
        match wrote {
            Ok(n) => {
                total += n as usize;
                if n < span.len {
                    break;
                }
            }
            Err(e) => return partial(total, e),
        }
    }
    Ok(total as u64)
}

/// `open(path, flags, mode)` (2): `openat(AT_FDCWD, ...)`.
pub(super) fn sys_open(path: UserPtr, flags: OpenFlags, mode: Mode) -> u64 {
    finish(openat(Dirfd::Cwd, path, flags, mode))
}

/// `openat(dirfd, path, flags, mode)` (257).
pub(super) fn sys_openat(dirfd: Dirfd, path: UserPtr, flags: OpenFlags, mode: Mode) -> u64 {
    finish(openat(dirfd, path, flags, mode))
}

fn openat(dirfd: Dirfd, path: UserPtr, flags: OpenFlags, mode: Mode) -> Result<u64, u32> {
    let path = UserPath::copy_in(path, false)?;
    // Linux takes the slot right after copying the path in: a full table
    // refuses before the dirfd is looked at or anything is opened or created.
    if !process::current_has_free_fd() {
        return Err(errno::EMFILE);
    }
    let start = at::start_dir(dirfd, &path, false)?;
    let umask = process::current_umask();
    let opened = open_inode_guarded(crate::vfs::tables(), start.dir(), path.as_bytes(), flags, mode.perm(), umask)
        .map_err(vfs)?;
    let mut file = OpenFile::from_opened(opened).map_err(vfs)?;
    if let Some(full) = start.display_path(&file.path) {
        file.path = full;
    }
    let fd = process::install_current_fd(Arc::new(file), flags.has_cloexec()).map_err(vfs)?;
    Ok(fd as u64)
}

/// The offset `whence` and `delta` name from `cur` (and the file's `size`,
/// asked only for `SEEK_END`): `EINVAL` if it is negative or past
/// `i64::MAX` (an `loff_t`), or `whence` is none of the three.
fn seek_target(whence: u32, delta: i64, cur: u64, size: impl FnOnce() -> Result<u64, u32>) -> Result<u64, u32> {
    let whence = i32::try_from(whence).map_err(|_| errno::EINVAL)?;
    let base = match SeekFrom::from_raw(whence, delta).map_err(vfs)? {
        SeekFrom::Start(_) => 0,
        SeekFrom::Current(_) => cur,
        SeekFrom::End(_) => size()?,
    };
    let base = i64::try_from(base).map_err(|_| errno::EINVAL)?;
    match base.checked_add(delta) {
        Some(new) if new >= 0 => Ok(new as u64),
        _ => Err(errno::EINVAL),
    }
}

/// `lseek(fd, offset, whence)` (8).
pub(super) fn sys_lseek(fd: Fd, offset: u64, whence: u32) -> u64 {
    finish(lseek(fd, offset as i64, whence))
}

fn lseek(fd: Fd, delta: i64, whence: u32) -> Result<u64, u32> {
    let file = file(fd)?;
    file.require_non_path().map_err(vfs)?;
    if file.inode.is_tty() {
        return Err(errno::ESPIPE);
    }
    match file.inode.inode_type() {
        // A directory's position is its cursor (`rewinddir`, `seekdir`);
        // its end means nothing.
        InodeType::Dir => {
            let mut cursor = file.dir_cursor.lock();
            let new = seek_target(whence, delta, *cursor, || Err(errno::EINVAL))?;
            *cursor = new;
            Ok(new)
        }
        // `/dev/null`, `/dev/zero`, `/dev/random`, `/dev/urandom`: always
        // at 0, whatever is asked (Linux's `null_lseek`; their offset is
        // never read).
        InodeType::CharDevice => Ok(0),
        _ => {
            let mut off = file.offset.lock();
            let new = seek_target(whence, delta, *off, || file.inode.stat().map(|m| m.size).map_err(vfs))?;
            *off = new;
            Ok(new)
        }
    }
}

/// Copy `path` and a NUL into the test process's user memory, in slot
/// `slot` (256 bytes each), for a handler's path argument.
#[cfg(feature = "ktest")]
pub(super) fn ktest_user_path(slot: u64, path: &[u8]) -> Result<UserPtr, &'static str> {
    let addr = process::KTEST_USER_VA + slot * 256;
    let mut bytes = path.to_vec();
    bytes.push(0);
    copy_to_user(addr, &bytes).map_err(|_| "copy a path into user memory")?;
    Ok(at::reg(addr))
}

#[cfg(feature = "ktest")]
fn ktest_flags(bits: u32) -> OpenFlags {
    OpenFlags::from_bits(bits)
}

/// A handler's return value as the fd it opened.
#[cfg(feature = "ktest")]
fn ktest_fd(ret: u64) -> Result<Fd, &'static str> {
    if ret < lumen_vfs::MAX_FDS as u64 { Ok(at::reg(ret)) } else { Err("the open failed") }
}

crate::ktest!(
    fn fd_openat_takes_the_lowest_free_slot_and_refuses_past_1024() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let null = ktest_user_path(0, b"/dev/null")?;
            let rd = ktest_flags(OpenFlags::O_RDONLY);
            let mode: Mode = at::reg(0);
            kassert_eq!(sys_openat(Dirfd::Cwd, null, rd, mode), 3);
            kassert_eq!(sys_open(null, rd, mode), 4);
            kassert_eq!(sys_close(at::reg(3)), 0);
            kassert_eq!(sys_openat(Dirfd::Cwd, null, rd, mode), 3);
            // A console fd's slot is as free as any once closed.
            kassert_eq!(sys_close(at::reg(1)), 0);
            kassert_eq!(sys_open(null, rd, mode), 1);
            for want in 5..lumen_vfs::MAX_FDS as u64 {
                let got = sys_open(null, rd, mode);
                if got != want {
                    crate::ktest::diag(format_args!("open #{want} gave {got:#x}"));
                    return Err("an open didn't take the lowest free slot");
                }
            }
            // Full: `-EMFILE`, and a full table creates nothing.
            kassert_eq!(sys_open(null, rd, mode), neg_errno(errno::EMFILE));
            let new = ktest_user_path(1, b"/tmp/fd-ktest-emfile")?;
            let creat = ktest_flags(OpenFlags::O_WRONLY | OpenFlags::O_CREAT);
            kassert_eq!(sys_open(new, creat, at::reg(0o644)), neg_errno(errno::EMFILE));
            kassert!(matches!(crate::vfs::resolve_path_from_root("/tmp/fd-ktest-emfile"), Err(VfsError::NoEnt)));
            // ... before the dirfd is looked at (a bad one is `-EMFILE` too).
            let rel = ktest_user_path(2, b"null")?;
            kassert_eq!(sys_openat(Dirfd::Fd(at::reg(999)), rel, rd, mode), neg_errno(errno::EMFILE));
            // A slot closed in the middle is the next one taken.
            kassert_eq!(sys_close(at::reg(700)), 0);
            kassert_eq!(sys_open(null, rd, mode), 700);
            // Closing what isn't open: `-EBADF`.
            kassert_eq!(sys_close(at::reg(700)), 0);
            kassert_eq!(sys_close(at::reg(700)), neg_errno(errno::EBADF));
            kassert_eq!(sys_close(at::reg(u64::MAX)), neg_errno(errno::EBADF));
            kassert_eq!(sys_close(at::reg(lumen_vfs::MAX_FDS as u64)), neg_errno(errno::EBADF));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fd_cloexec_follows_o_cloexec_and_fioclex() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let null = ktest_user_path(0, b"/dev/null")?;
            let mode: Mode = at::reg(0);
            let cloexec = |fd: Fd| with_current(|p| p.fds.get_cloexec(fd.raw()));
            let a = ktest_fd(sys_open(null, ktest_flags(OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC), mode))?;
            let b = ktest_fd(sys_open(null, ktest_flags(OpenFlags::O_RDONLY), mode))?;
            kassert_eq!(cloexec(a), Ok(true));
            kassert_eq!(cloexec(b), Ok(false));
            kassert_eq!(sys_ioctl(b, ioctl::FIOCLEX, 0), 0);
            kassert_eq!(sys_ioctl(a, ioctl::FIONCLEX, 0), 0);
            kassert_eq!(cloexec(a), Ok(false));
            kassert_eq!(cloexec(b), Ok(true));
            // `FIONBIO` sets and clears `O_NONBLOCK` from the `int` it points at.
            let int_at = process::KTEST_USER_VA + 1024;
            let nonblock = |fd: Fd| current_file_flags(fd).map(|f| f & OpenFlags::O_NONBLOCK != 0);
            copy_to_user(int_at, &1i32.to_le_bytes()).map_err(|_| "copy the int")?;
            kassert_eq!(sys_ioctl(a, ioctl::FIONBIO, int_at), 0);
            kassert_eq!(nonblock(a), Some(true));
            copy_to_user(int_at, &0i32.to_le_bytes()).map_err(|_| "copy the int")?;
            kassert_eq!(sys_ioctl(a, ioctl::FIONBIO, int_at), 0);
            kassert_eq!(nonblock(a), Some(false));
            kassert_eq!(sys_ioctl(a, ioctl::FIONBIO, 0), neg_errno(errno::EFAULT));
            // On the console (serial here, with no input waiting), a read
            // with `O_NONBLOCK` is `-EAGAIN`, not a wait.
            let io = with_current(|p| p.io);
            with_current_mut(|p| p.io = ConsoleIo::Serial);
            copy_to_user(int_at, &1i32.to_le_bytes()).map_err(|_| "copy the int")?;
            let set = sys_ioctl(at::reg(0), ioctl::FIONBIO, int_at);
            let pending = process::stdin_ready(ConsoleIo::Serial);
            let read = sys_read(at::reg(0), at::reg(process::KTEST_USER_VA + 2048), 4);
            with_current_mut(|p| p.io = io);
            kassert_eq!(set, 0);
            if !pending {
                kassert_eq!(read, neg_errno(errno::EAGAIN));
            }
            // Not open, or `O_PATH`: `-EBADF`.
            kassert_eq!(sys_ioctl(at::reg(77), ioctl::FIOCLEX, 0), neg_errno(errno::EBADF));
            let tmp = ktest_user_path(1, b"/tmp")?;
            let path_fd = ktest_fd(sys_open(tmp, ktest_flags(OpenFlags::O_PATH), mode))?;
            kassert_eq!(sys_ioctl(path_fd, ioctl::FIOCLEX, 0), neg_errno(errno::EBADF));
            kassert_eq!(cloexec(path_fd), Ok(false));
            // The tty requests: only on a terminal (fd 0, `/dev/tty`).
            let out = process::KTEST_USER_VA + 2048;
            kassert_eq!(sys_ioctl(a, ioctl::TCGETS, out), neg_errno(errno::ENOTTY));
            kassert_eq!(sys_ioctl(a, ioctl::TIOCGWINSZ, out), neg_errno(errno::ENOTTY));
            kassert_eq!(sys_ioctl(at::reg(0), ioctl::TCGETS, out), 0);
            let tty = ktest_user_path(2, b"/dev/tty")?;
            let tty_fd = ktest_fd(sys_open(tty, ktest_flags(OpenFlags::O_RDWR), mode))?;
            kassert_eq!(sys_ioctl(tty_fd, ioctl::TIOCGWINSZ, out), 0);
            kassert!(current_file(tty_fd).is_some_and(|f| f.inode.is_tty()));
            Ok(())
        })?
    }
);

/// The status flags of the current process's file on `fd` (ktests).
#[cfg(feature = "ktest")]
fn current_file_flags(fd: Fd) -> Option<u32> {
    process::current_file(fd.raw()).map(|f| f.status_flags())
}

#[cfg(feature = "ktest")]
fn current_file(fd: Fd) -> Option<FileRef> {
    process::current_file(fd.raw())
}

crate::ktest!(
    fn fd_read_write_readv_writev_and_fstat_on_a_file() {
        use crate::user::copy_from_user;
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let base = process::KTEST_USER_VA;
            let data = base + 1024;
            let iov = base + 2048;
            let out = base + 3072;
            let path = ktest_user_path(0, b"/tmp/fd-ktest-rw")?;
            let rdwr = ktest_flags(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
            let fd = ktest_fd(sys_open(path, rdwr, at::reg(0o666)))?;
            copy_to_user(data, b"hello world").map_err(|_| "copy data")?;
            kassert_eq!(sys_write(fd, at::reg(data), 11), 11);
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_SET), 0);
            kassert_eq!(sys_read(fd, at::reg(out), 5), 5);
            let mut got = [0u8; 11];
            copy_from_user(&mut got[..5], out).map_err(|_| "copy out")?;
            kassert_eq!(&got[..5], b"hello");
            // A copy out that faults reads nothing: `-EFAULT`, the offset
            // where it was.
            kassert_eq!(sys_read(fd, at::reg(0xffff_8000_0000_0000), 3), neg_errno(errno::EFAULT));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 5);
            // `readv` fills each span in turn, stopping at EOF.
            let spans: [(u64, u64); 2] = [(out, 2), (out + 2, 100)];
            for (i, (b, l)) in spans.iter().enumerate() {
                let at = iov + 16 * i as u64;
                copy_to_user(at, &b.to_le_bytes()).map_err(|_| "iov")?;
                copy_to_user(at + 8, &l.to_le_bytes()).map_err(|_| "iov")?;
            }
            kassert_eq!(sys_readv(fd, at::reg(iov), 2), 6);
            copy_from_user(&mut got[..6], out).map_err(|_| "copy out")?;
            kassert_eq!(&got[..6], b" world");
            kassert_eq!(sys_read(fd, at::reg(out), 5), 0);
            // `writev` writes every span: "hello world" then "AB" + "CDE".
            copy_to_user(data, b"ABCDE").map_err(|_| "copy data")?;
            let spans: [(u64, u64); 2] = [(data, 2), (data + 2, 3)];
            for (i, (b, l)) in spans.iter().enumerate() {
                let at = iov + 16 * i as u64;
                copy_to_user(at, &b.to_le_bytes()).map_err(|_| "iov")?;
                copy_to_user(at + 8, &l.to_le_bytes()).map_err(|_| "iov")?;
            }
            kassert_eq!(sys_writev(fd, at::reg(iov), 2), 5);
            // `fstat`: a regular file of 16 bytes, its mode less the umask.
            kassert_eq!(sys_fstat(fd, at::reg(out)), 0);
            let mut st = [0u8; 144];
            copy_from_user(&mut st, out).map_err(|_| "copy stat")?;
            let st_mode = u32::from_le_bytes(st[24..28].try_into().expect("4 bytes"));
            let st_size = u64::from_le_bytes(st[48..56].try_into().expect("8 bytes"));
            kassert_eq!(st_mode, general::S_IFREG | 0o644);
            kassert_eq!(st_size, 16);
            // `O_APPEND` writes at the end wherever the offset is.
            let app = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_WRONLY | OpenFlags::O_APPEND), at::reg(0)))?;
            kassert_eq!(sys_write(app, at::reg(data), 1), 1);
            kassert_eq!(sys_lseek(app, 0, general::SEEK_CUR), 17);
            // Wrong direction: `-EBADF`; a directory: `-EISDIR`.
            let rd = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_RDONLY), at::reg(0)))?;
            kassert_eq!(sys_write(rd, at::reg(data), 1), neg_errno(errno::EBADF));
            kassert_eq!(sys_read(app, at::reg(out), 1), neg_errno(errno::EBADF));
            kassert_eq!(sys_read(rd, at::reg(out), 32), 17);
            let mut all = [0u8; 17];
            copy_from_user(&mut all, out).map_err(|_| "copy out")?;
            kassert_eq!(&all, b"hello worldABCDEA");
            let dir = ktest_user_path(1, b"/tmp")?;
            let dir_fd = ktest_fd(sys_open(dir, ktest_flags(OpenFlags::O_RDONLY), at::reg(0)))?;
            kassert_eq!(sys_read(dir_fd, at::reg(out), 1), neg_errno(errno::EISDIR));
            kassert_eq!(sys_read(dir_fd, at::reg(out), 0), neg_errno(errno::EISDIR));
            // A console fd's `fstat`: the console's character device.
            kassert_eq!(sys_fstat(at::reg(1), at::reg(out)), 0);
            copy_from_user(&mut st, out).map_err(|_| "copy stat")?;
            let st_mode = u32::from_le_bytes(st[24..28].try_into().expect("4 bytes"));
            let st_rdev = u64::from_le_bytes(st[40..48].try_into().expect("8 bytes"));
            kassert_eq!(st_mode, general::S_IFCHR | 0o620);
            kassert_eq!(st_rdev, (5 << 8) | 1);
            kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/fd-ktest-rw", false).is_ok());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fd_poll_reports_files_ready_terminals_by_direction_and_the_rest_invalid() {
        use crate::kassert_eq;
        use crate::user::copy_from_user;
        process::in_test_process(|_| {
            let mode: Mode = at::reg(0);
            let null = ktest_user_path(0, b"/dev/null")?;
            let null_fd = ktest_fd(sys_open(null, ktest_flags(OpenFlags::O_RDONLY), mode))?;
            let tmp = ktest_user_path(1, b"/tmp")?;
            let dir_fd = ktest_fd(sys_open(tmp, ktest_flags(OpenFlags::O_RDONLY), mode))?;
            let path_fd = ktest_fd(sys_open(tmp, ktest_flags(OpenFlags::O_PATH), mode))?;
            let both = (general::POLLIN | general::POLLOUT) as i16;
            // (fd, events) -> the revents wanted: a file and a directory
            // ready both ways (only what was asked); the console's fd 0
            // readable (capture input never blocks) but not writable, fd 1
            // writable but not readable; closed and `O_PATH` fds invalid; a
            // negative fd ignored.
            let cases: [(i32, i16, i16); 8] = [
                (null_fd.raw(), both, both),
                (dir_fd.raw(), general::POLLIN as i16, general::POLLIN as i16),
                (0, both, general::POLLIN as i16),
                (1, both, general::POLLOUT as i16),
                (path_fd.raw(), both, general::POLLNVAL as i16),
                (99, both, general::POLLNVAL as i16),
                (lumen_vfs::MAX_FDS as i32, both, general::POLLNVAL as i16),
                (-1, both, 0),
            ];
            let fds = process::KTEST_USER_VA + 1024;
            let mut raw = Vec::new();
            for (fd, events, _) in cases {
                raw.extend_from_slice(&fd.to_le_bytes());
                raw.extend_from_slice(&events.to_le_bytes());
                raw.extend_from_slice(&0x5a5ai16.to_le_bytes());
            }
            copy_to_user(fds, &raw).map_err(|_| "copy the pollfds")?;
            kassert_eq!(sys_poll(fds, cases.len() as u64, 0), 7);
            copy_from_user(&mut raw, fds).map_err(|_| "copy the pollfds back")?;
            for (i, (_, _, want)) in cases.iter().enumerate() {
                let got = i16::from_le_bytes(raw[8 * i + 6..8 * i + 8].try_into().expect("2 bytes"));
                kassert_eq!((i, got), (i, *want));
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fd_lseek_edges() {
        use crate::kassert_eq;
        // The arithmetic alone: each whence, past the end, a negative or
        // overflowing result, an unknown whence.
        let size = || Ok(10);
        kassert_eq!(seek_target(general::SEEK_SET, 3, 5, size), Ok(3));
        kassert_eq!(seek_target(general::SEEK_CUR, 2, 5, size), Ok(7));
        kassert_eq!(seek_target(general::SEEK_CUR, -5, 5, size), Ok(0));
        kassert_eq!(seek_target(general::SEEK_CUR, -6, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_END, 0, 5, size), Ok(10));
        kassert_eq!(seek_target(general::SEEK_END, 5, 5, size), Ok(15));
        kassert_eq!(seek_target(general::SEEK_END, -11, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_SET, -1, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_SET, i64::MAX, 5, size), Ok(i64::MAX as u64));
        kassert_eq!(seek_target(general::SEEK_CUR, 1, i64::MAX as u64, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_CUR, i64::MIN, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_END, i64::MAX, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(general::SEEK_END, 0, 5, || Ok(u64::MAX)), Err(errno::EINVAL));
        kassert_eq!(seek_target(3, 0, 5, size), Err(errno::EINVAL));
        kassert_eq!(seek_target(u32::MAX, 0, 5, size), Err(errno::EINVAL));
        // `SEEK_END`'s size is asked for only by `SEEK_END`.
        kassert_eq!(seek_target(general::SEEK_SET, 4, 5, || Err(errno::EIO)), Ok(4));
        kassert_eq!(seek_target(general::SEEK_END, 4, 5, || Err(errno::EIO)), Err(errno::EIO));

        // Through `lseek`, on each kind of descriptor.
        process::in_test_process(|_| {
            let data = process::KTEST_USER_VA + 1024;
            let path = ktest_user_path(0, b"/tmp/fd-ktest-lseek")?;
            let flags = ktest_flags(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC);
            let fd = ktest_fd(sys_open(path, flags, at::reg(0o644)))?;
            copy_to_user(data, b"0123456789").map_err(|_| "copy data")?;
            kassert_eq!(sys_write(fd, at::reg(data), 10), 10);
            kassert_eq!(sys_lseek(fd, 3, general::SEEK_SET), 3);
            kassert_eq!(sys_lseek(fd, 2, general::SEEK_CUR), 5);
            // A refused seek leaves the offset where it was.
            kassert_eq!(sys_lseek(fd, -6i64 as u64, general::SEEK_CUR), neg_errno(errno::EINVAL));
            kassert_eq!(sys_lseek(fd, 0, 7), neg_errno(errno::EINVAL));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 5);
            kassert_eq!(sys_lseek(fd, -10i64 as u64, general::SEEK_END), 0);
            kassert_eq!(sys_lseek(fd, 5, general::SEEK_END), 15);
            kassert_eq!(sys_lseek(fd, i64::MAX as u64, general::SEEK_SET), i64::MAX as u64);
            kassert_eq!(sys_lseek(fd, 1, general::SEEK_CUR), neg_errno(errno::EINVAL));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), i64::MAX as u64);
            // `read` starts where `lseek` put the offset.
            kassert_eq!(sys_lseek(fd, 7, general::SEEK_SET), 7);
            kassert_eq!(sys_read(fd, at::reg(data), 10), 3);
            let mut got = [0u8; 3];
            crate::user::copy_from_user(&mut got, data).map_err(|_| "copy out")?;
            kassert_eq!(&got, b"789");
            // A terminal can't seek; nor can what isn't open, or `O_PATH`.
            kassert_eq!(sys_lseek(at::reg(0), 0, general::SEEK_SET), neg_errno(errno::ESPIPE));
            kassert_eq!(sys_lseek(at::reg(1), 0, general::SEEK_CUR), neg_errno(errno::ESPIPE));
            kassert_eq!(sys_lseek(at::reg(99), 0, general::SEEK_SET), neg_errno(errno::EBADF));
            let opath = ktest_fd(sys_open(path, ktest_flags(OpenFlags::O_PATH), at::reg(0)))?;
            kassert_eq!(sys_lseek(opath, 0, general::SEEK_SET), neg_errno(errno::EBADF));
            // `/dev/null` is always at 0.
            let null = ktest_user_path(1, b"/dev/null")?;
            let null_fd = ktest_fd(sys_open(null, ktest_flags(OpenFlags::O_RDWR), at::reg(0)))?;
            kassert_eq!(sys_lseek(null_fd, 100, general::SEEK_SET), 0);
            kassert_eq!(sys_lseek(null_fd, 5, general::SEEK_END), 0);
            // A directory seeks its cursor; it has no end.
            let tmp = ktest_user_path(2, b"/tmp")?;
            let dir_fd = ktest_fd(sys_open(tmp, ktest_flags(OpenFlags::O_RDONLY), at::reg(0)))?;
            kassert_eq!(sys_lseek(dir_fd, 2, general::SEEK_SET), 2);
            kassert_eq!(sys_lseek(dir_fd, 1, general::SEEK_CUR), 3);
            kassert_eq!(current_file(dir_fd).map(|f| *f.dir_cursor.lock()), Some(3));
            kassert_eq!(sys_lseek(dir_fd, 0, general::SEEK_END), neg_errno(errno::EINVAL));
            kassert_eq!(sys_lseek(dir_fd, 0, general::SEEK_SET), 0);
            crate::kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/fd-ktest-lseek", false).is_ok());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fd_handlers_take_their_registers_in_order() {
        // Through `dispatch`, each argument in its own register with
        // distinct values (garbage above an `int`'s 32 bits too), so an arm
        // passing them out of order can't pass.
        use crate::arch::syscall::SyscallFrame;
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64| {
                let mut frame = SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, ..SyscallFrame::default() };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            let high = 0xdead_beef_0000_0000u64;
            let tmp = ktest_user_path(0, b"/tmp")?;
            let dirfd = call(general::__NR_open, tmp.addr(), high | u64::from(OpenFlags::O_DIRECTORY), 0, 0);
            kassert_eq!(dirfd, 3);
            // openat(dirfd, "fd-ktest-reg", O_RDWR | O_CREAT, 0o640): relative
            // to fd 3, the mode less the umask (0o022).
            let rel = ktest_user_path(1, b"fd-ktest-reg")?;
            let flags = u64::from(OpenFlags::O_RDWR | OpenFlags::O_CREAT);
            let fd = call(general::__NR_openat, high | dirfd, rel.addr(), high | flags, high | 0o666);
            kassert_eq!(fd, 4);
            let st = crate::vfs::resolve_path_from_root("/tmp/fd-ktest-reg").and_then(|i| i.stat());
            kassert_eq!(st.map(|m| m.mode), Ok(0o644));
            // Its path for `/proc/<pid>/fd`: the dirfd's joined to it.
            kassert_eq!(current_file(at::reg(4)).map(|f| f.path.clone()).as_deref(), Some("/tmp/fd-ktest-reg"));
            // lseek(fd, 7, SEEK_CUR) twice: 7, then 14 (swapped, either is
            // `-EINVAL` or `-EBADF`).
            kassert_eq!(call(general::__NR_lseek, high | fd, 7, high | u64::from(general::SEEK_CUR), 0), 7);
            kassert_eq!(call(general::__NR_lseek, fd, 7, u64::from(general::SEEK_CUR), 0), 14);
            // open(path, O_RDONLY, mode) by its absolute path: fd 5.
            let abs = ktest_user_path(2, b"/tmp/fd-ktest-reg")?;
            kassert_eq!(call(general::__NR_open, abs.addr(), high, high | 0o777, 0), 5);
            kassert_eq!(call(general::__NR_close, high | 5, 0, 0, 0), 0);
            kassert_eq!(call(general::__NR_close, 5, 0, 0, 0), neg_errno(errno::EBADF));
            kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, b"/tmp/fd-ktest-reg", false).is_ok());
            Ok(())
        })?
    }
);
