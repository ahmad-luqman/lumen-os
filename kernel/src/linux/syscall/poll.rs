//! Readiness (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): M6's
//! `poll`, moved here from `fd` unchanged; and M8's `select`, `pselect6`,
//! `ppoll`.

use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use super::at::UserPtr;
use super::fd::current_io;
use super::{DEFAULT_RLIMIT_NOFILE, neg_errno, not_implemented};
use crate::linux::process::{self, FileRef};
use crate::linux::{errno, general};
use crate::sched;
use crate::user::{copy_from_user, copy_to_user};

/// Raw Linux x86-64 `struct pollfd` layout (8 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
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
        Some(f) if f.inode().is_tty() => PollClass::Tty { readable: f.is_readable(), writable: f.is_writable() },
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

    // The console's `io`, then the polled files, all cloned out in one more
    // hold of the descriptor table's lock, classified (and dropped) after it.
    let io = current_io();
    let polled: Vec<i32> = raw.as_chunks::<8>().0.iter().map(fd_at).collect();
    let files = process::current_files(&polled);
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

/// `select(n, readfds, writefds, exceptfds, tv)` (23): M8 stub, replaced by B3 (#374).
pub(super) fn sys_select(_n: i32, _readfds: UserPtr, _writefds: UserPtr, _exceptfds: UserPtr, _tv: UserPtr) -> u64 {
    not_implemented(general::__NR_select)
}

/// `pselect6(n, readfds, writefds, exceptfds, tsp, sig)` (270): M8 stub, replaced by B3 (#374) and C6 (#385).
pub(super) fn sys_pselect6(
    _n: i32,
    _readfds: UserPtr,
    _writefds: UserPtr,
    _exceptfds: UserPtr,
    _tsp: UserPtr,
    _sig: UserPtr,
) -> u64 {
    not_implemented(general::__NR_pselect6)
}

/// `ppoll(fds, nfds, tsp, sigmask, sigsetsize)` (271): M8 stub, replaced by B3 (#374) and C6 (#385).
pub(super) fn sys_ppoll(_fds: UserPtr, _nfds: u32, _tsp: UserPtr, _sigmask: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_ppoll)
}
