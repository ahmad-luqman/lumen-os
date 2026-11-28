//! `epoll` syscalls: `epoll_create` (213), `epoll_wait` (232), `epoll_ctl`
//! (233), `epoll_pwait` (281), `epoll_create1` (291), and `epoll_pwait2` (441)
//! (M9 B5, #476; ARCHITECTURE sections 8 and 10).

use lumen_epoll::{EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD};
use lumen_signal::Blocked;
use lumen_vfs::{OpenFlags, VfsError};

use super::at::{Fd, UserPtr};
use super::finish;
use super::poll::{WaitTimeout, copy_sigmask, copy_timespec};
use crate::linux::epoll::{EPOLL_EVENT_SIZE, Epoll, decode_event};
use crate::linux::errno;
use crate::linux::process::{self, AnonObj, KernelFile};
use crate::user::{CopyError, copy_from_user, user_range_valid};

/// Nanoseconds per millisecond.
const NS_PER_MS: u64 = 1_000_000;

/// Maximum `maxevents` accepted by `epoll_wait*` (Linux v6.12 `fs/eventpoll.c`:
/// `EP_MAX_EVENTS = INT_MAX / sizeof(struct epoll_event)`).
const EP_MAX_EVENTS: i32 = i32::MAX / (EPOLL_EVENT_SIZE as i32);

/// Copy one packed 12-byte x86-64 `struct epoll_event` from user space.
fn copy_event_from_user(ptr: UserPtr) -> Result<(u32, u64), u32> {
    let mut raw = [0u8; EPOLL_EVENT_SIZE];
    copy_from_user(&mut raw, ptr.addr()).map_err(CopyError::to_errno)?;
    Ok(decode_event(&raw))
}

/// Decode a `timeout` argument in milliseconds (`epoll_wait`, `epoll_pwait`).
fn ms_timeout(timeout_ms: i32) -> WaitTimeout {
    if timeout_ms < 0 {
        WaitTimeout::Forever
    } else {
        WaitTimeout::after_ns((timeout_ms as u64).saturating_mul(NS_PER_MS))
    }
}

/// Core `do_epoll_wait` (Linux v6.12 `fs/eventpoll.c`): validates `maxevents`,
/// checks `events` lies in user space (`access_ok`), looks up `epfd`, and
/// runs [`Epoll::wait`].
fn do_epoll_wait(epfd: Fd, events: UserPtr, maxevents: i32, timeout: WaitTimeout) -> Result<u64, u32> {
    if maxevents <= 0 || maxevents > EP_MAX_EVENTS {
        return Err(errno::EINVAL);
    }
    let byte_len = (maxevents as usize) * EPOLL_EVENT_SIZE;
    if !user_range_valid(events.addr(), byte_len) {
        return Err(errno::EFAULT);
    }
    let ep_io = epfd.file()?;
    if ep_io.is_path() {
        return Err(errno::EBADF);
    }
    let Some(ep) = ep_io.epoll() else {
        return Err(errno::EINVAL);
    };
    ep.wait(events.addr(), maxevents as usize, timeout)
}

/// Core `do_epoll_pwait` (Linux v6.12 `fs/eventpoll.c`): installs an optional
/// temporary signal mask for the duration of `do_epoll_wait` and restores it
/// unless interrupted (`-EINTR`, in which case `exit_to_user` delivers under
/// the temporary mask and restores the saved mask on return).
fn do_epoll_pwait(
    epfd: Fd,
    events: UserPtr,
    maxevents: i32,
    timeout: WaitTimeout,
    sigmask: UserPtr,
    sigsetsize: u64,
) -> Result<u64, u32> {
    let mask = copy_sigmask(sigmask, sigsetsize)?;
    let thread = process::current_thread();
    if let Some(mask) = mask {
        thread.install_temporary_mask(Blocked::new(mask));
    }
    let result = do_epoll_wait(epfd, events, maxevents, timeout);
    thread.restore_saved_sigmask_unless(result == Err(errno::EINTR));
    result
}

/// `epoll_create(size)` (213): `size <= 0` -> `-EINVAL`, else `epoll_create1(0)`.
pub(super) fn sys_epoll_create(size: i32) -> u64 {
    if size <= 0 {
        return finish(Err(errno::EINVAL));
    }
    sys_epoll_create1(0)
}

/// `epoll_create1(flags)` (291): accepts `EPOLL_CLOEXEC` only, creates an
/// `anon_inode:[eventpoll]` description, and installs it (`-EMFILE` if full).
pub(super) fn sys_epoll_create1(flags: i32) -> u64 {
    finish((|| {
        let flags_u32 = flags as u32;
        if (flags_u32 & !EPOLL_CLOEXEC) != 0 {
            return Err(errno::EINVAL);
        }
        let cloexec = (flags_u32 & EPOLL_CLOEXEC) != 0;
        let reserved = process::reserve_current_fd().map_err(VfsError::to_errno)?;
        let ep = Epoll::new();
        let file = KernelFile::new_anon(OpenFlags::from_bits(OpenFlags::O_RDWR), AnonObj::Epoll(ep))
            .map_err(VfsError::to_errno)?;
        Ok(reserved.install(file, cloexec) as u64)
    })())
}

/// `epoll_ctl(epfd, op, fd, event)` (233): Linux v6.12 `fs/eventpoll.c`
/// `SYSCALL_DEFINE4(epoll_ctl)` / `do_epoll_ctl` error order and operations.
pub(super) fn sys_epoll_ctl(epfd: Fd, op: i32, fd: Fd, event: UserPtr) -> u64 {
    finish((|| {
        let op_u32 = op as u32;
        // Linux `ep_op_has_event(op)`: copy `event` for every op except `DEL`.
        let ev = if op_u32 != EPOLL_CTL_DEL { Some(copy_event_from_user(event)?) } else { None };

        let ep_io = epfd.file()?;
        if ep_io.is_path() {
            return Err(errno::EBADF);
        }
        let target_io = fd.file()?;
        if target_io.is_path() {
            return Err(errno::EBADF);
        }
        let pollable = target_io.pollable().ok_or(errno::EPERM)?;
        if ep_io.id() == target_io.id() {
            return Err(errno::EINVAL);
        }
        let Some(ep) = ep_io.epoll() else {
            return Err(errno::EINVAL);
        };

        match (op_u32, ev) {
            (EPOLL_CTL_ADD, Some((events, data))) => ep.ctl_add(&target_io, pollable, fd.raw(), events, data)?,
            (EPOLL_CTL_MOD, Some((events, data))) => ep.ctl_mod(&target_io, fd.raw(), events, data)?,
            (EPOLL_CTL_DEL, None) => ep.ctl_del(&target_io, fd.raw())?,
            _ => return Err(errno::EINVAL),
        }
        // `target_io` and `ep_io` drop here, outside every epoll lock.
        Ok(0)
    })())
}

/// `epoll_wait(epfd, events, maxevents, timeout)` (232).
pub(super) fn sys_epoll_wait(epfd: Fd, events: UserPtr, maxevents: i32, timeout: i32) -> u64 {
    finish(do_epoll_wait(epfd, events, maxevents, ms_timeout(timeout)))
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)` (281).
pub(super) fn sys_epoll_pwait(
    epfd: Fd,
    events: UserPtr,
    maxevents: i32,
    timeout: i32,
    sigmask: UserPtr,
    sigsetsize: u64,
) -> u64 {
    finish(do_epoll_pwait(epfd, events, maxevents, ms_timeout(timeout), sigmask, sigsetsize))
}

/// `epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)` (441).
pub(super) fn sys_epoll_pwait2(
    epfd: Fd,
    events: UserPtr,
    maxevents: i32,
    timeout: UserPtr,
    sigmask: UserPtr,
    sigsetsize: u64,
) -> u64 {
    finish((|| {
        let wt = copy_timespec(timeout)?;
        do_epoll_pwait(epfd, events, maxevents, wt, sigmask, sigsetsize)
    })())
}
