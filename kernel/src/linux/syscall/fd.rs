//! Descriptor I/O: `read`, `write`, `close`, `fstat`, `poll`, `ioctl`,
//! `readv`, `writev` (M6), and M7's `open`, `openat`, `lseek`.

use alloc::vec;
use core::mem::size_of;

use super::at::{Dirfd, Fd, Mode, UserPtr};
use super::{
    DEFAULT_RLIMIT_NOFILE, MAX_SINGLE_IO, copy_iovecs, copy_pod_from_user, copy_pod_to_user, neg_errno,
    not_implemented, read_into_user, write_from_user,
};
use crate::linux::process::{ConsoleIo, FdKind, with_current, with_current_mut};
use crate::linux::{errno, general, ioctl};
use crate::sched;
use crate::user::{copy_from_user, copy_to_user};
use lumen_vfs::OpenFlags;

/// Raw Linux x86-64 `struct pollfd` layout (8 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

pub(super) fn sys_read(fd: u64, buf: u64, count: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_readable() {
        return neg_errno(errno::EBADF);
    }
    match read_into_user(io, buf, count) {
        Ok(n) => n as u64,
        Err(e) => neg_errno(e),
    }
}

pub(super) fn sys_write(fd: u64, buf: u64, count: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_writable() {
        return neg_errno(errno::EBADF);
    }
    match write_from_user(io, buf, count) {
        Ok(n) => n as u64,
        Err(e) => neg_errno(e),
    }
}

pub(super) fn sys_close(fd: u64) -> u64 {
    with_current_mut(|p| {
        if p.fd_kind(fd).is_open() {
            p.fds[fd as usize] = FdKind::Closed;
            0
        } else {
            neg_errno(errno::EBADF)
        }
    })
}

pub(super) fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    let open = with_current(|p| p.fd_kind(fd).is_open());
    if !open {
        return neg_errno(errno::EBADF);
    }
    let st = general::stat {
        st_dev: 0,
        st_ino: fd + 1,
        st_nlink: 1,
        st_mode: general::S_IFCHR | 0o620,
        st_uid: 0,
        st_gid: 0,
        __pad0: 0,
        st_rdev: (5 << 8) | 1,
        st_size: 0,
        st_blksize: 4096,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        __unused: [0; 3],
    };
    // SAFETY: `general::stat` is 144 bytes with explicit `__pad0` and zero implicit padding.
    match unsafe { copy_pod_to_user(statbuf, &st) } {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

/// Console `poll(2)` supporting Rust `std` startup's `sanitize_standard_fds`
/// (`poll([0, 1, 2], 3, 0)`) and `busybox` `lineedit` stdin readiness waits.
/// Full multi-fd blocking `poll`/`ppoll` with pipes/PTYs arrives in M8.
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

    let (io, fds) = with_current(|p| (p.io, p.fds));
    let fd_kind_of =
        |fd: i32| -> FdKind { if fd >= 0 && (fd as usize) < fds.len() { fds[fd as usize] } else { FdKind::Closed } };
    let deadline = if timeout_ms > 0 {
        Some(crate::time::now_ns().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    } else {
        None
    };

    // Only touch stdin readiness if at least one entry actually asks for POLLIN on Stdin.
    let needs_stdin = raw.as_chunks::<8>().0.iter().any(|chunk| {
        let fd = i32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes"));
        let events = i16::from_le_bytes(chunk[4..6].try_into().expect("2 bytes"));
        fd >= 0 && matches!(fd_kind_of(fd), FdKind::Stdin) && (events & (general::POLLIN as i16)) != 0
    });

    let ready = loop {
        let mut ready = 0u64;
        let mut waiting_stdin = false;
        let stdin_avail = if needs_stdin { crate::linux::process::stdin_ready(io) } else { false };

        for chunk in raw.as_chunks_mut::<8>().0 {
            let fd = i32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes"));
            let events = i16::from_le_bytes(chunk[4..6].try_into().expect("2 bytes"));
            let revents: i16 = if fd < 0 {
                0
            } else {
                match fd_kind_of(fd) {
                    FdKind::Closed => general::POLLNVAL as i16,
                    FdKind::Stdout | FdKind::Stderr => events & (general::POLLOUT as i16),
                    FdKind::Stdin => {
                        if (events & (general::POLLIN as i16)) != 0 {
                            if stdin_avail {
                                general::POLLIN as i16
                            } else {
                                waiting_stdin = true;
                                0
                            }
                        } else {
                            0
                        }
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
        crate::linux::process::wait_stdin_step(io, deadline);
    };

    if let Err(e) = copy_to_user(fds_ptr, &raw) {
        return neg_errno(e.to_errno());
    }
    ready
}

pub(super) fn sys_ioctl(fd: u64, req: u64, arg: u64) -> u64 {
    let (open, io) = with_current(|p| (p.fd_kind(fd).is_open(), p.io));
    if !open {
        return neg_errno(errno::EBADF);
    }
    let cmd = req as u32;
    if cmd == ioctl::TCGETS {
        let t = with_current(|p| p.termios);
        // SAFETY: `general::termios` is 36 bytes with zero implicit padding.
        return match unsafe { copy_pod_to_user(arg, &t) } {
            Ok(()) => 0,
            Err(e) => neg_errno(e.to_errno()),
        };
    }
    if cmd == ioctl::TCSETS || cmd == ioctl::TCSETSW || cmd == ioctl::TCSETSF {
        // SAFETY: `general::termios` is 36 integer bytes; all bit patterns are valid.
        return match unsafe { copy_pod_from_user::<general::termios>(arg) } {
            Ok(t) => {
                with_current_mut(|p| p.termios = t);
                0
            }
            Err(e) => neg_errno(e.to_errno()),
        };
    }
    if cmd == ioctl::TIOCGWINSZ {
        let (ws_row, ws_col) = match io {
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
        return match unsafe { copy_pod_to_user(arg, &ws) } {
            Ok(()) => 0,
            Err(e) => neg_errno(e.to_errno()),
        };
    }
    neg_errno(errno::ENOTTY)
}

pub(super) fn sys_readv(fd: u64, iov: u64, iovcnt: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_readable() {
        return neg_errno(errno::EBADF);
    }
    let iovs = match copy_iovecs(iov, iovcnt) {
        Ok(v) => v,
        Err(e) => return neg_errno(e),
    };
    for span in iovs {
        if span.len == 0 {
            continue;
        }
        // Standard tty short-read semantics: return as soon as the first non-empty
        // span receives data (or EOF/error) so musl `__stdio_read` never hangs
        // trying to fill trailing iovecs.
        return match read_into_user(io, span.base, span.len) {
            Ok(n) => n as u64,
            Err(e) => neg_errno(e),
        };
    }
    0
}

pub(super) fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_writable() {
        return neg_errno(errno::EBADF);
    }
    let iovs = match copy_iovecs(iov, iovcnt) {
        Ok(v) => v,
        Err(e) => return neg_errno(e),
    };
    let mut remaining = MAX_SINGLE_IO;
    let mut total = 0usize;
    for span in iovs {
        if span.len == 0 {
            continue;
        }
        if remaining == 0 {
            break;
        }
        let want = span.len.min(remaining as u64);
        match write_from_user(io, span.base, want) {
            Ok(n) => {
                total = total.saturating_add(n);
                remaining = remaining.saturating_sub(n);
                if (n as u64) < want || (n as u64) < span.len {
                    break;
                }
            }
            Err(e) => return if total == 0 { neg_errno(e) } else { total as u64 },
        }
    }
    total as u64
}

/// `open(path, flags, mode)` (2): M7 stub.
pub(super) fn sys_open(_path: UserPtr, _flags: OpenFlags, _mode: Mode) -> u64 {
    not_implemented(general::__NR_open)
}

/// `openat(dirfd, path, flags, mode)` (257): M7 stub.
pub(super) fn sys_openat(_dirfd: Dirfd, _path: UserPtr, _flags: OpenFlags, _mode: Mode) -> u64 {
    not_implemented(general::__NR_openat)
}

/// `lseek(fd, offset, whence)` (8): M7 stub.
pub(super) fn sys_lseek(_fd: Fd, _offset: u64, _whence: u32) -> u64 {
    not_implemented(general::__NR_lseek)
}

crate::ktest!(
    fn fd_m7_stubs_return_enosys() {
        super::check_stubs(&[general::__NR_open, general::__NR_openat, general::__NR_lseek])
    }
);
