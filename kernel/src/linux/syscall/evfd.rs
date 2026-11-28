//! `eventfd`/`eventfd2` and `timerfd_create`/`timerfd_settime`/`timerfd_gettime`
//! syscalls (M9 B6, #477; ARCHITECTURE sections 8 and 10).
//!
//! Validation and errno order match Linux v6.12 `fs/eventfd.c` and
//! `fs/timerfd.c`. All user copies happen outside every kernel lock.

use core::mem::{offset_of, size_of};

use lumen_eventfd::{
    ClockError, EventfdFlags, ITimer, TimerClock, TimerfdCreateFlags, TimerfdSettimeFlags, itimerspec_to_ns,
};
use lumen_vfs::OpenFlags;

use super::at::{Fd, UserPtr};
use super::fd::vfs;
use super::time::RawTimespec;
use super::timer::write_setting;
use super::{copy_pod_from_user, finish};
use crate::linux::evfd::{EventFd, TimerFd};
use crate::linux::process::{self, AnonObj, KernelFile};
use crate::linux::{errno, general};
use crate::user::CopyError;

/// Linux x86-64 `struct itimerspec` (32 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawItimerspec {
    it_interval: RawTimespec,
    it_value: RawTimespec,
}

const _: () = {
    assert!(size_of::<RawItimerspec>() == size_of::<general::itimerspec>() && size_of::<RawItimerspec>() == 32);
    assert!(offset_of!(RawItimerspec, it_interval) == offset_of!(general::itimerspec, it_interval));
    assert!(offset_of!(RawItimerspec, it_value) == offset_of!(general::itimerspec, it_value));
};

fn read_itimerspec(ptr: UserPtr) -> Result<RawItimerspec, u32> {
    // SAFETY: `RawItimerspec` is four `i64`s with no padding; every bit pattern is valid.
    unsafe { copy_pod_from_user::<RawItimerspec>(ptr.addr()) }.map_err(CopyError::to_errno)
}

fn anon_open_flags(nonblock: bool) -> OpenFlags {
    let nb = if nonblock { OpenFlags::O_NONBLOCK } else { 0 };
    OpenFlags::from_bits(OpenFlags::O_RDWR | nb)
}

/// `timerfd_create(clockid, flags)` (283).
pub(super) fn sys_timerfd_create(clockid: i32, flags: i32) -> u64 {
    finish(timerfd_create(clockid, flags))
}

fn timerfd_create(clockid: i32, flags: i32) -> Result<u64, u32> {
    // Linux v6.12 `fs/timerfd.c` `SYSCALL_DEFINE2(timerfd_create, ...)`:
    // validate flags (-EINVAL), then clockid (-EINVAL for unknown; alarm clocks
    // are always -EPERM because Lumen models no `CAP_WAKE_ALARM`), then
    // allocate fd.
    let flags = TimerfdCreateFlags::from_bits(flags as u32).map_err(|_| errno::EINVAL)?;
    let clock = TimerClock::from_clockid(clockid).map_err(|e| match e {
        ClockError::AlarmNotPermitted => errno::EPERM,
        ClockError::Invalid => errno::EINVAL,
    })?;
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let tfd = TimerFd::new(clock);
    let file = KernelFile::new_anon(anon_open_flags(flags.nonblock), AnonObj::TimerFd(tfd)).map_err(vfs)?;
    Ok(slot.install(file, flags.cloexec) as u64)
}

/// `eventfd(initval)` (284): `eventfd2(initval, 0)`.
pub(super) fn sys_eventfd(initval: u32) -> u64 {
    finish(eventfd2(initval, 0))
}

/// `eventfd2(initval, flags)` (290).
pub(super) fn sys_eventfd2(initval: u32, flags: i32) -> u64 {
    finish(eventfd2(initval, flags))
}

fn eventfd2(initval: u32, flags: i32) -> Result<u64, u32> {
    // Linux v6.12 `fs/eventfd.c` `do_eventfd`: validate flags first, then
    // allocate descriptor and anonymous inode.
    let flags = EventfdFlags::from_bits(flags as u32).map_err(|_| errno::EINVAL)?;
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let evfd = EventFd::new(initval, flags.semaphore);
    let file = KernelFile::new_anon(anon_open_flags(flags.nonblock), AnonObj::EventFd(evfd)).map_err(vfs)?;
    Ok(slot.install(file, flags.cloexec) as u64)
}

/// `timerfd_settime(fd, flags, new, old)` (286).
pub(super) fn sys_timerfd_settime(fd: Fd, flags: i32, new: UserPtr, old: UserPtr) -> u64 {
    finish(timerfd_settime(fd, flags, new, old))
}

fn timerfd_settime(fd: Fd, flags: i32, new: UserPtr, old: UserPtr) -> Result<u64, u32> {
    // Linux v6.12 `fs/timerfd.c` `SYSCALL_DEFINE4(timerfd_settime, ...)` and
    // `do_timerfd_settime`: copy `new` (-EFAULT), validate flags and timespecs
    // (-EINVAL), look up fd (-EBADF if closed or O_PATH; -EINVAL if not a
    // timerfd), apply setting, then copy `old` out if non-NULL (-EFAULT).
    let raw = read_itimerspec(new)?;
    let flags = TimerfdSettimeFlags::from_bits(flags as u32).map_err(|_| errno::EINVAL)?;
    let interval = itimerspec_to_ns(raw.it_interval.tv_sec, raw.it_interval.tv_nsec).map_err(|_| errno::EINVAL)?;
    let value = itimerspec_to_ns(raw.it_value.tv_sec, raw.it_value.tv_nsec).map_err(|_| errno::EINVAL)?;
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    let Some(AnonObj::TimerFd(tfd)) = file.anon() else {
        return Err(errno::EINVAL);
    };
    let prev = tfd.settime(flags, ITimer::new(value, interval));
    drop(file);
    if old.addr() != 0 {
        write_setting(old, (prev.interval, prev.value), false)?;
    }
    Ok(0)
}

/// `timerfd_gettime(fd, cur)` (287).
pub(super) fn sys_timerfd_gettime(fd: Fd, cur: UserPtr) -> u64 {
    finish(timerfd_gettime(fd, cur))
}

fn timerfd_gettime(fd: Fd, cur: UserPtr) -> Result<u64, u32> {
    // Linux v6.12 `fs/timerfd.c` `do_timerfd_gettime` / `SYSCALL_DEFINE2`:
    // look up timerfd (-EBADF / -EINVAL), snapshot remaining/interval, copy out.
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    let Some(AnonObj::TimerFd(tfd)) = file.anon() else {
        return Err(errno::EINVAL);
    };
    let cur_it = tfd.gettime();
    drop(file);
    write_setting(cur, (cur_it.interval, cur_it.value), false)
}
