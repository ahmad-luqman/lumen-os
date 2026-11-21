//! Linux x86-64 30-syscall dispatcher (M6, `docs/ARCHITECTURE.md` §8).
//!
//! All user pointers are copied via `.lumen_fixup`-protected `copy_from_user` /
//! `copy_to_user` with interrupts enabled and no `SpinLock` held. Unknown
//! syscalls return `-ENOSYS` and log once per syscall number per boot at
//! `debug` level.

use alloc::vec;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process::{ConsoleIo, FdKind, KernelSigaction, record_exit, with_current, with_current_mut};
use crate::linux::{ARCH_GET_FS, ARCH_SET_FS, Utsname, default_utsname, errno, general, ioctl};
use crate::mm::user_vm::prot_from_linux;
use crate::sched;
use crate::sync::SpinLock;
use crate::user::{CopyError, USER_MAX, UserExit, copy_from_user, copy_to_user, user_range_valid};

/// Maximum `iovec` count accepted by `readv`/`writev` (POSIX `UIO_MAXIOV`).
const UIO_MAXIOV: isize = 1024;

/// Maximum bytes transferred in one `write`/`getrandom` loop chunk (matches `shell::CHUNK`).
const IO_CHUNK: usize = 512;

/// Cap total bytes processed in a single `write`/`writev` call so one syscall
/// cannot monopolize a CPU indefinitely.
const MAX_SINGLE_IO: usize = 1024 * 1024;

/// Cap bytes returned by a single `getrandom(2)` call to avoid issuing
/// thousands of synchronous `virtio-rng` requests in one syscall.
const MAX_GETRANDOM: usize = 4096;

/// Default stack rlimit advertised by M6 `prlimit64` (8 MiB, matching `USER_STACK_SIZE`).
const DEFAULT_RLIMIT_STACK: u64 = 8 * 1024 * 1024;

/// Default open-file rlimit advertised by M6 `prlimit64`.
const DEFAULT_RLIMIT_NOFILE: u64 = 1024;

/// Mask of unblockable signals (`SIGKILL` = 9, `SIGSTOP` = 19, 1-indexed).
const UNBLOCKABLE_SIGS: u64 = (1u64 << (general::SIGKILL - 1)) | (1u64 << (general::SIGSTOP - 1));

/// Raw Linux x86-64 `struct iovec` layout (16 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawIovec {
    base: u64,
    len: u64,
}

/// Raw Linux x86-64 `struct pollfd` layout (8 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

// Compile-time zero-padding verification for every POD ABI struct copied to/from user space.
const _: () = {
    assert!(size_of::<RawIovec>() == size_of::<general::iovec>() && size_of::<RawIovec>() == 16);
    assert!(size_of::<RawPollFd>() == size_of::<general::pollfd>() && size_of::<RawPollFd>() == 8);
    assert!(size_of::<general::stat>() == 144);
    assert!(size_of::<general::termios>() == 36);
    assert!(size_of::<general::winsize>() == 8);
    assert!(size_of::<general::timespec>() == 16);
    assert!(size_of::<general::rlimit64>() == 16);
    assert!(size_of::<Utsname>() == 390);
    assert!(size_of::<KernelSigaction>() == 32);
};

#[inline]
const fn neg_errno(code: u32) -> u64 {
    -(code as i64) as u64
}

/// Copy a zero-padding POD ABI struct `T` to `user_dst`.
///
/// # Safety
/// `T` must have no uninitialized padding bytes so viewing `&T` as `[u8; size_of::<T>()]`
/// is sound.
unsafe fn copy_pod_to_user<T: Copy>(user_dst: u64, val: &T) -> Result<(), CopyError> {
    // SAFETY: caller guarantees `T` has no padding bytes and `val` is initialized.
    let bytes = unsafe { core::slice::from_raw_parts((val as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user(user_dst, bytes)
}

/// Copy a zero-padding POD ABI struct `T` from `user_src`.
///
/// # Safety
/// Every bit pattern of length `size_of::<T>()` must be a valid value of `T`.
unsafe fn copy_pod_from_user<T: Copy>(user_src: u64) -> Result<T, CopyError> {
    let mut raw = MaybeUninit::<T>::zeroed();
    // SAFETY: `raw` is writable stack storage of `size_of::<T>()` bytes.
    let dst = unsafe { core::slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    copy_from_user(dst, user_src)?;
    // SAFETY: `copy_from_user` initialized all `size_of::<T>()` bytes and caller
    // guarantees any bit pattern is valid for `T`.
    Ok(unsafe { raw.assume_init() })
}

// Lock-free deduplication for unimplemented syscall debug logs (once per syscall number per boot).
static UNKNOWN_LOW_BITS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16]; // syscall numbers 0..1024
static UNKNOWN_HIGH_SLOTS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8]; // up to 8 distinct numbers >= 1024

fn log_unknown_once(nr: u64) {
    if nr < 1024 {
        let idx = (nr / 64) as usize;
        let bit = 1u64 << (nr % 64);
        if (UNKNOWN_LOW_BITS[idx].fetch_or(bit, Ordering::Relaxed) & bit) == 0 {
            log::debug!(target: "linux", "unimplemented syscall #{nr}");
        }
        return;
    }
    for slot in &UNKNOWN_HIGH_SLOTS {
        let cur = slot.load(Ordering::Acquire);
        if cur == nr {
            return;
        }
        if cur == 0 {
            match slot.compare_exchange(0, nr, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    log::debug!(target: "linux", "unimplemented syscall #{nr}");
                    return;
                }
                Err(actual) if actual == nr => return,
                Err(_) => continue,
            }
        }
    }
}

/// Cached `(unix_epoch_seconds, monotonic_anchor_ns)` sampled once from CMOS RTC.
static REALTIME_ANCHOR: SpinLock<Option<Option<(u64, u64)>>> = SpinLock::new(None);

fn realtime_sec_nsec() -> (u64, u64) {
    let anchor = {
        let guard = REALTIME_ANCHOR.lock();
        *guard
    };
    let anchor = match anchor {
        Some(cached) => cached,
        None => {
            // Read RTC outside the lock (`rtc::now_quiet` polls CMOS ports).
            let sampled = crate::rtc::now_quiet()
                .ok()
                .and_then(|dt| dt.unix_seconds())
                .map(|unix_sec| (unix_sec, crate::time::now_ns()));
            let mut guard = REALTIME_ANCHOR.lock();
            *guard.get_or_insert(sampled)
        }
    };
    let now_ns = crate::time::now_ns();
    match anchor {
        Some((base_sec, base_ns)) => {
            let delta = now_ns.saturating_sub(base_ns);
            (base_sec.saturating_add(delta / 1_000_000_000), delta % 1_000_000_000)
        }
        None => (now_ns / 1_000_000_000, now_ns % 1_000_000_000),
    }
}

/// Dispatch one Linux x86-64 syscall (`rax` = nr, `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9` = args).
pub fn dispatch(frame: &mut SyscallFrame) -> SyscallReturn {
    let nr = frame.rax;
    match nr {
        n if n == u64::from(general::__NR_read) => {
            frame.rax = sys_read(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_write) => {
            frame.rax = sys_write(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_close) => {
            frame.rax = sys_close(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fstat) => {
            frame.rax = sys_fstat(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_poll) => {
            frame.rax = sys_poll(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mmap) => {
            frame.rax = sys_mmap(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mprotect) => {
            frame.rax = sys_mprotect(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_munmap) => {
            frame.rax = sys_munmap(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_brk) => {
            frame.rax = sys_brk(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rt_sigaction) => {
            frame.rax = sys_rt_sigaction(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rt_sigprocmask) => {
            frame.rax = sys_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_ioctl) => {
            frame.rax = sys_ioctl(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_readv) => {
            frame.rax = sys_readv(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_writev) => {
            frame.rax = sys_writev(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getpid) => {
            frame.rax = u64::from(with_current(|p| p.pid));
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_exit) || n == u64::from(general::__NR_exit_group) => {
            let status = (frame.rdi & 0xff) as i32;
            let (_, is_test) = record_exit(UserExit::Exited(status));
            if is_test {
                crate::user::note_exit_status(status);
            }
            // SAFETY: unwinds cleanly back to `enter_ring3`; no lock held and
            // no live Rust `Drop` values on this stack frame.
            unsafe { arch::syscall::unwind_to_kernel() }
        }
        n if n == u64::from(general::__NR_uname) => {
            frame.rax = sys_uname(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getcwd) => {
            frame.rax = sys_getcwd(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getuid)
            || n == u64::from(general::__NR_getgid)
            || n == u64::from(general::__NR_geteuid)
            || n == u64::from(general::__NR_getegid) =>
        {
            frame.rax = 0;
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getppid) => {
            frame.rax = u64::from(with_current(|p| p.ppid));
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getpgrp) => {
            frame.rax = u64::from(with_current(|p| p.pid));
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_arch_prctl) => {
            frame.rax = sys_arch_prctl(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_gettid) => {
            frame.rax = u64::from(with_current(|p| p.tid));
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_set_tid_address) => {
            let tidptr = frame.rdi;
            frame.rax = u64::from(with_current_mut(|p| {
                p.clear_child_tid = tidptr;
                p.tid
            }));
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_clock_gettime) => {
            frame.rax = sys_clock_gettime(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_prlimit64) => {
            frame.rax = sys_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getrandom) => {
            frame.rax = sys_getrandom(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        _ => {
            if with_current(|p| p.allow_test_syscalls)
                && let Some(ret) = crate::user::dispatch_test_syscall(frame)
            {
                return ret;
            }
            log_unknown_once(nr);
            frame.rax = neg_errno(errno::ENOSYS);
            SyscallReturn::Sysret
        }
    }
}

fn read_into_user(io: ConsoleIo, user_buf: u64, count: u64) -> Result<usize, u32> {
    if count == 0 {
        return Ok(0);
    }
    let want = count.min(IO_CHUNK as u64) as usize;
    // Validate the target chunk range before blocking on stdin.
    if !user_range_valid(user_buf, want) {
        return Err(errno::EFAULT);
    }
    let mut kbuf = [0u8; IO_CHUNK];
    let n = crate::linux::process::read_console(io, &mut kbuf[..want]);
    if n == 0 {
        return Ok(0);
    }
    copy_to_user(user_buf, &kbuf[..n]).map_err(CopyError::to_errno)?;
    Ok(n)
}

fn write_from_user(io: ConsoleIo, user_buf: u64, count: u64) -> Result<usize, u32> {
    if count == 0 {
        return Ok(0);
    }
    let limit = count.min(MAX_SINGLE_IO as u64) as usize;
    let mut kbuf = [0u8; IO_CHUNK];
    let mut written = 0usize;
    while written < limit {
        let chunk = (limit - written).min(IO_CHUNK);
        let addr = match user_buf.checked_add(written as u64) {
            Some(a) => a,
            None => return if written == 0 { Err(errno::EFAULT) } else { Ok(written) },
        };
        if copy_from_user(&mut kbuf[..chunk], addr).is_err() {
            return if written == 0 { Err(errno::EFAULT) } else { Ok(written) };
        }
        crate::linux::process::write_console(io, &kbuf[..chunk]);
        written += chunk;
    }
    Ok(written)
}

fn sys_read(fd: u64, buf: u64, count: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_readable() {
        return neg_errno(errno::EBADF);
    }
    match read_into_user(io, buf, count) {
        Ok(n) => n as u64,
        Err(e) => neg_errno(e),
    }
}

fn sys_write(fd: u64, buf: u64, count: u64) -> u64 {
    let (kind, io) = with_current(|p| (p.fd_kind(fd), p.io));
    if !kind.is_writable() {
        return neg_errno(errno::EBADF);
    }
    match write_from_user(io, buf, count) {
        Ok(n) => n as u64,
        Err(e) => neg_errno(e),
    }
}

fn sys_close(fd: u64) -> u64 {
    with_current_mut(|p| {
        if p.fd_kind(fd).is_open() {
            p.fds[fd as usize] = FdKind::Closed;
            0
        } else {
            neg_errno(errno::EBADF)
        }
    })
}

fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
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
fn sys_poll(fds_ptr: u64, nfds: u64, timeout_raw: u64) -> u64 {
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

fn sys_mmap(addr: u64, len: u64, prot_raw: u64, flags: u64) -> u64 {
    let Some(prot) = prot_from_linux(prot_raw) else {
        return neg_errno(errno::EINVAL);
    };
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::ENOMEM);
    };
    match space.mmap_anon(addr, len, prot, flags) {
        Ok(va) => va,
        Err(e) => neg_errno(e.to_errno()),
    }
}

fn sys_mprotect(addr: u64, len: u64, prot_raw: u64) -> u64 {
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

fn sys_munmap(addr: u64, len: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::EINVAL);
    };
    match space.munmap(addr, len) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

fn sys_brk(addr: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return 0;
    };
    space.brk(addr)
}

fn sys_rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 || !(1..=64).contains(&sig) {
        return neg_errno(errno::EINVAL);
    }
    if act != 0 && (sig == u64::from(general::SIGKILL) || sig == u64::from(general::SIGSTOP)) {
        return neg_errno(errno::EINVAL);
    }
    let new_action = if act != 0 {
        // SAFETY: `KernelSigaction` is 4 `u64`s (32 bytes); all bit patterns are valid.
        match unsafe { copy_pod_from_user::<KernelSigaction>(act) } {
            Ok(a) => Some(a),
            Err(e) => return neg_errno(e.to_errno()),
        }
    } else {
        None
    };

    let idx = (sig - 1) as usize;
    let prev = with_current(|p| p.sigactions[idx]);
    if oldact != 0 {
        // Write `oldact` BEFORE mutating the table so a faulting `oldact` leaves state intact.
        // SAFETY: `KernelSigaction` has zero padding bytes.
        if let Err(e) = unsafe { copy_pod_to_user(oldact, &prev) } {
            return neg_errno(e.to_errno());
        }
    }
    if let Some(mut a) = new_action {
        a.mask &= !UNBLOCKABLE_SIGS;
        with_current_mut(|p| p.sigactions[idx] = a);
    }
    0
}

fn sys_rt_sigprocmask(how: u64, set: u64, oldset: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 {
        return neg_errno(errno::EINVAL);
    }
    let new_mask = if set != 0 {
        if how != u64::from(general::SIG_BLOCK)
            && how != u64::from(general::SIG_UNBLOCK)
            && how != u64::from(general::SIG_SETMASK)
        {
            return neg_errno(errno::EINVAL);
        }
        let mut raw = [0u8; 8];
        if let Err(e) = copy_from_user(&mut raw, set) {
            return neg_errno(e.to_errno());
        }
        Some(u64::from_le_bytes(raw))
    } else {
        None
    };

    let prev = with_current(|p| p.sigmask);
    if oldset != 0
        && let Err(e) = copy_to_user(oldset, &prev.to_le_bytes())
    {
        return neg_errno(e.to_errno());
    }
    if let Some(mask) = new_mask {
        let updated = if how == u64::from(general::SIG_BLOCK) {
            prev | mask
        } else if how == u64::from(general::SIG_UNBLOCK) {
            prev & !mask
        } else {
            mask
        };
        with_current_mut(|p| p.sigmask = updated & !UNBLOCKABLE_SIGS);
    }
    0
}

fn sys_ioctl(fd: u64, req: u64, arg: u64) -> u64 {
    let open = with_current(|p| p.fd_kind(fd).is_open());
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
        let ws = general::winsize { ws_row: 25, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `general::winsize` is 4 `u16`s (8 bytes) with zero padding.
        return match unsafe { copy_pod_to_user(arg, &ws) } {
            Ok(()) => 0,
            Err(e) => neg_errno(e.to_errno()),
        };
    }
    neg_errno(errno::ENOTTY)
}

fn copy_iovecs(user_iov: u64, iovcnt: u64) -> Result<alloc::vec::Vec<RawIovec>, u32> {
    let cnt = iovcnt as isize;
    if !(0..=UIO_MAXIOV).contains(&cnt) {
        return Err(errno::EINVAL);
    }
    let cnt = cnt as usize;
    if cnt == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let byte_len = cnt * size_of::<RawIovec>();
    let mut raw = vec![0u8; byte_len];
    copy_from_user(&mut raw, user_iov).map_err(CopyError::to_errno)?;
    let mut iovs = alloc::vec::Vec::with_capacity(cnt);
    let mut total_len = 0usize;
    for chunk in raw.as_chunks::<16>().0 {
        let base = u64::from_le_bytes(chunk[0..8].try_into().expect("8 bytes"));
        let len = u64::from_le_bytes(chunk[8..16].try_into().expect("8 bytes"));
        if (len as isize) < 0 {
            return Err(errno::EINVAL);
        }
        total_len = total_len.checked_add(len as usize).filter(|&t| t <= isize::MAX as usize).ok_or(errno::EINVAL)?;
        iovs.push(RawIovec { base, len });
    }
    Ok(iovs)
}

fn sys_readv(fd: u64, iov: u64, iovcnt: u64) -> u64 {
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

fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> u64 {
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

fn sys_uname(buf: u64) -> u64 {
    let uts = default_utsname();
    // SAFETY: `Utsname` (`system::new_utsname`) is 6 x `[c_char; 65]` (390 bytes), zero padding.
    match unsafe { copy_pod_to_user(buf, &uts) } {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

fn sys_getcwd(buf: u64, size: u64) -> u64 {
    if size < 2 {
        return neg_errno(errno::ERANGE);
    }
    match copy_to_user(buf, b"/\0") {
        Ok(()) => 2,
        Err(e) => neg_errno(e.to_errno()),
    }
}

fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    if code == u64::from(ARCH_SET_FS) {
        if addr >= USER_MAX {
            neg_errno(errno::EINVAL)
        } else {
            sched::set_current_fs_base(addr);
            0
        }
    } else if code == u64::from(ARCH_GET_FS) {
        let fs = sched::current_fs_base();
        match copy_to_user(addr, &fs.to_le_bytes()) {
            Ok(()) => 0,
            Err(e) => neg_errno(e.to_errno()),
        }
    } else {
        neg_errno(errno::EINVAL)
    }
}

fn sys_clock_gettime(clockid: u64, tp: u64) -> u64 {
    let (sec, nsec) =
        if clockid == u64::from(general::CLOCK_REALTIME) || clockid == u64::from(general::CLOCK_REALTIME_COARSE) {
            realtime_sec_nsec()
        } else if clockid == u64::from(general::CLOCK_MONOTONIC)
            || clockid == u64::from(general::CLOCK_MONOTONIC_RAW)
            || clockid == u64::from(general::CLOCK_MONOTONIC_COARSE)
            || clockid == u64::from(general::CLOCK_BOOTTIME)
        {
            let ns = crate::time::now_ns();
            (ns / 1_000_000_000, ns % 1_000_000_000)
        } else {
            return neg_errno(errno::EINVAL);
        };

    let ts = general::timespec { tv_sec: sec as i64, tv_nsec: nsec as i64 };
    // SAFETY: `general::timespec` is two `i64`s (16 bytes) with zero padding.
    match unsafe { copy_pod_to_user(tp, &ts) } {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

fn sys_prlimit64(pid: u64, resource: u64, new_limit: u64, old_limit: u64) -> u64 {
    let current_pid = with_current(|p| p.pid);
    if pid != 0 && pid != u64::from(current_pid) {
        return neg_errno(errno::ESRCH);
    }
    if resource >= u64::from(general::RLIM_NLIMITS) {
        return neg_errno(errno::EINVAL);
    }
    if new_limit != 0 {
        // Validate user pointer and `rlim_cur <= rlim_max` even though M6 limits are read-only.
        // SAFETY: `general::rlimit64` is two `u64`s (16 bytes); all bit patterns valid.
        match unsafe { copy_pod_from_user::<general::rlimit64>(new_limit) } {
            Ok(rlim) if rlim.rlim_cur > rlim.rlim_max => return neg_errno(errno::EINVAL),
            Ok(_) => {}
            Err(e) => return neg_errno(e.to_errno()),
        }
    }
    if old_limit != 0 {
        let limit = match resource as u32 {
            general::RLIMIT_STACK => DEFAULT_RLIMIT_STACK,
            general::RLIMIT_NOFILE => DEFAULT_RLIMIT_NOFILE,
            _ => u64::MAX,
        };
        let rlim = general::rlimit64 { rlim_cur: limit, rlim_max: limit };
        // SAFETY: `general::rlimit64` is two `u64`s (16 bytes) with zero padding.
        if let Err(e) = unsafe { copy_pod_to_user(old_limit, &rlim) } {
            return neg_errno(e.to_errno());
        }
    }
    0
}

fn fill_insecure_fallback(state: &mut u64, dst: &mut [u8]) {
    for chunk in dst.chunks_mut(8) {
        // SplitMix64 mixing step (non-cryptographic fallback only when GRND_INSECURE
        // is requested on a machine without hardware/virtio RNG).
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let bytes = z.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

fn sys_getrandom(buf: u64, buflen: u64, flags: u64) -> u64 {
    let allowed = u64::from(general::GRND_NONBLOCK | general::GRND_RANDOM | general::GRND_INSECURE);
    let incompatible = u64::from(general::GRND_RANDOM | general::GRND_INSECURE);
    if (flags & !allowed) != 0 || (flags & incompatible) == incompatible {
        return neg_errno(errno::EINVAL);
    }
    if buflen == 0 {
        return 0;
    }
    let total = buflen.min(MAX_GETRANDOM as u64) as usize;
    if !user_range_valid(buf, 1) {
        return neg_errno(errno::EFAULT);
    }
    let mut kbuf = [0u8; IO_CHUNK];
    let mut fallback_state = crate::time::now_ns() ^ arch::rdtsc().rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    let mut written = 0usize;
    let res = loop {
        if written >= total {
            break written as u64;
        }
        let chunk = (total - written).min(IO_CHUNK);
        let addr = match buf.checked_add(written as u64) {
            Some(a) => a,
            None => break if written == 0 { neg_errno(errno::EFAULT) } else { written as u64 },
        };
        if crate::rng::fill(&mut kbuf[..chunk]).is_err() {
            if (flags & u64::from(general::GRND_INSECURE)) != 0 {
                fill_insecure_fallback(&mut fallback_state, &mut kbuf[..chunk]);
            } else {
                break if written == 0 { neg_errno(errno::EAGAIN) } else { written as u64 };
            }
        }
        if copy_to_user(addr, &kbuf[..chunk]).is_err() {
            break if written == 0 { neg_errno(errno::EFAULT) } else { written as u64 };
        }
        written += chunk;
    };
    lumen_rand::clear(&mut kbuf);
    res
}
