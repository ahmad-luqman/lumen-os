//! Linux x86-64 syscall dispatcher (M6 built, M7 numbers routed;
//! `docs/ARCHITECTURE.md` §8).
//!
//! `dispatch` maps each syscall number to its handler, one `match` arm per
//! number. The handlers live in one module per cluster, so the PRs that fill
//! in M7 (#254) each touch only their own module, never `dispatch`:
//!
//! - `fd`: descriptor I/O (`read`, `write`, `close`, `fstat`, `poll`,
//!   `ioctl`, `readv`, `writev`; M7: `open`, `openat`, `lseek`).
//! - `stat`: `stat`, `lstat`, `newfstatat`, `statx`, `access`, `faccessat`,
//!   `faccessat2` (M7).
//! - `ns`: namespace changes (`mkdir*`, `rmdir`, `unlink*`, `link*`,
//!   `symlink*`, `readlink*`, `rename*`; M7).
//! - `dir`: `getcwd` (M6); `getdents64`, `chdir`, `fchdir` (M7).
//! - `meta`: `chmod*`, `chown*`, `umask`, `utimensat`, `truncate`,
//!   `ftruncate` (M7).
//! - `io`: `pread64`, `pwrite64`, `fsync`, `fdatasync`, `sync`, `msync` (M7).
//! - `lock`: `fcntl`, `flock` (M7).
//! - `mem`: `mmap`, `mprotect`, `munmap`, `brk` (M6).
//! - `proc`: signals, `arch_prctl`, `prlimit64`, `uname`, `clock_gettime`,
//!   `getrandom` (M6).
//!
//! An M7 syscall not implemented yet has a stub that logs once per number
//! (as an unknown one does) and returns `-ENOSYS`.
//!
//! All user pointers are copied via `.lumen_fixup`-protected `copy_from_user` /
//! `copy_to_user` with interrupts enabled and no `SpinLock` held. Unknown
//! syscalls return `-ENOSYS` and log once per syscall number per boot at
//! `debug` level.

mod dir;
mod fd;
mod io;
mod lock;
mod mem;
mod meta;
mod ns;
mod proc;
mod stat;

use alloc::vec;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process::{ConsoleIo, KernelSigaction, record_exit, with_current, with_current_mut};
use crate::linux::{Utsname, errno, general};
use crate::user::{CopyError, UserExit, copy_from_user, copy_to_user, user_range_valid};

/// Maximum `iovec` count accepted by `readv`/`writev` (POSIX `UIO_MAXIOV`).
const UIO_MAXIOV: isize = 1024;

/// Maximum bytes transferred in one `write`/`getrandom` loop chunk (matches `shell::CHUNK`).
const IO_CHUNK: usize = 512;

/// Cap total bytes processed in a single `write`/`writev` call so one syscall
/// cannot monopolize a CPU indefinitely.
const MAX_SINGLE_IO: usize = 1024 * 1024;

/// Default open-file rlimit advertised by M6 `prlimit64`.
const DEFAULT_RLIMIT_NOFILE: u64 = 1024;

/// Raw Linux x86-64 `struct iovec` layout (16 bytes, zero padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawIovec {
    base: u64,
    len: u64,
}

// Compile-time zero-padding verification for every POD ABI struct copied to/from user space.
const _: () = {
    assert!(size_of::<RawIovec>() == size_of::<general::iovec>() && size_of::<RawIovec>() == 16);
    assert!(size_of::<fd::RawPollFd>() == size_of::<general::pollfd>() && size_of::<fd::RawPollFd>() == 8);
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

/// Whether this is the first time this boot that syscall `nr` is reported
/// as unimplemented (and so worth a log line); records it either way.
fn first_report(nr: u64) -> bool {
    if nr < 1024 {
        let idx = (nr / 64) as usize;
        let bit = 1u64 << (nr % 64);
        return (UNKNOWN_LOW_BITS[idx].fetch_or(bit, Ordering::Relaxed) & bit) == 0;
    }
    for slot in &UNKNOWN_HIGH_SLOTS {
        let cur = slot.load(Ordering::Acquire);
        if cur == nr {
            return false;
        }
        if cur == 0 {
            match slot.compare_exchange(0, nr, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) if actual == nr => return false,
                Err(_) => continue,
            }
        }
    }
    false
}

fn log_unknown_once(nr: u64) {
    if first_report(nr) {
        log::debug!(target: "linux", "unimplemented syscall #{nr}");
    }
}

/// The last number an M7 stub was called with, for the stub ktests to tell
/// an arm calling its own stub from one calling another (`u32::MAX`: none).
#[cfg(feature = "ktest")]
static LAST_STUB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);

/// The body of an M7 syscall's stub until its cluster's PR implements it:
/// log once per number, as an unknown syscall does but with its name, and
/// return `-ENOSYS`.
fn not_implemented(nr: u32) -> u64 {
    #[cfg(feature = "ktest")]
    LAST_STUB.store(nr, Ordering::Relaxed);
    if first_report(u64::from(nr)) {
        let name = crate::linux::syscall_name(nr).unwrap_or("?");
        log::debug!(target: "linux", "unimplemented syscall #{nr} ({name})");
    }
    neg_errno(errno::ENOSYS)
}

/// Dispatch one Linux x86-64 syscall (`rax` = nr, `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9` = args).
pub fn dispatch(frame: &mut SyscallFrame) -> SyscallReturn {
    let nr = frame.rax;
    match nr {
        // M6.
        n if n == u64::from(general::__NR_read) => {
            frame.rax = fd::sys_read(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_write) => {
            frame.rax = fd::sys_write(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_close) => {
            frame.rax = fd::sys_close(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fstat) => {
            frame.rax = fd::sys_fstat(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_poll) => {
            frame.rax = fd::sys_poll(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mmap) => {
            frame.rax = mem::sys_mmap(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mprotect) => {
            frame.rax = mem::sys_mprotect(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_munmap) => {
            frame.rax = mem::sys_munmap(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_brk) => {
            frame.rax = mem::sys_brk(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rt_sigaction) => {
            frame.rax = proc::sys_rt_sigaction(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rt_sigprocmask) => {
            frame.rax = proc::sys_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_ioctl) => {
            frame.rax = fd::sys_ioctl(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_readv) => {
            frame.rax = fd::sys_readv(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_writev) => {
            frame.rax = fd::sys_writev(frame.rdi, frame.rsi, frame.rdx);
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
            frame.rax = proc::sys_uname(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getcwd) => {
            frame.rax = dir::sys_getcwd(frame.rdi, frame.rsi);
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
            frame.rax = proc::sys_arch_prctl(frame.rdi, frame.rsi);
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
            frame.rax = proc::sys_clock_gettime(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_prlimit64) => {
            frame.rax = proc::sys_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getrandom) => {
            frame.rax = proc::sys_getrandom(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }

        // M7 (`crate::linux::M7_SYSCALLS`): each arm passes the arguments of
        // the syscall's Linux x86-64 signature, and no more (`faccessat` and
        // `fchmodat` take 3, never `r10`).
        n if n == u64::from(general::__NR_open) => {
            frame.rax = fd::sys_open(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_openat) => {
            frame.rax = fd::sys_openat(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_lseek) => {
            frame.rax = fd::sys_lseek(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_stat) => {
            frame.rax = stat::sys_stat(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_lstat) => {
            frame.rax = stat::sys_lstat(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_newfstatat) => {
            frame.rax = stat::sys_newfstatat(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_statx) => {
            frame.rax = stat::sys_statx(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_access) => {
            frame.rax = stat::sys_access(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_faccessat) => {
            frame.rax = stat::sys_faccessat(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_faccessat2) => {
            frame.rax = stat::sys_faccessat2(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mkdir) => {
            frame.rax = ns::sys_mkdir(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_mkdirat) => {
            frame.rax = ns::sys_mkdirat(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rmdir) => {
            frame.rax = ns::sys_rmdir(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_unlink) => {
            frame.rax = ns::sys_unlink(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_unlinkat) => {
            frame.rax = ns::sys_unlinkat(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_link) => {
            frame.rax = ns::sys_link(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_linkat) => {
            frame.rax = ns::sys_linkat(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_symlink) => {
            frame.rax = ns::sys_symlink(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_symlinkat) => {
            frame.rax = ns::sys_symlinkat(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_readlink) => {
            frame.rax = ns::sys_readlink(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_readlinkat) => {
            frame.rax = ns::sys_readlinkat(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_rename) => {
            frame.rax = ns::sys_rename(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_renameat) => {
            frame.rax = ns::sys_renameat(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_renameat2) => {
            frame.rax = ns::sys_renameat2(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_getdents64) => {
            frame.rax = dir::sys_getdents64(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_chdir) => {
            frame.rax = dir::sys_chdir(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchdir) => {
            frame.rax = dir::sys_fchdir(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_chmod) => {
            frame.rax = meta::sys_chmod(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchmod) => {
            frame.rax = meta::sys_fchmod(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchmodat) => {
            frame.rax = meta::sys_fchmodat(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchmodat2) => {
            frame.rax = meta::sys_fchmodat2(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_chown) => {
            frame.rax = meta::sys_chown(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchown) => {
            frame.rax = meta::sys_fchown(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_lchown) => {
            frame.rax = meta::sys_lchown(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fchownat) => {
            frame.rax = meta::sys_fchownat(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_umask) => {
            frame.rax = meta::sys_umask(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_utimensat) => {
            frame.rax = meta::sys_utimensat(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_truncate) => {
            frame.rax = meta::sys_truncate(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_ftruncate) => {
            frame.rax = meta::sys_ftruncate(frame.rdi, frame.rsi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_pread64) => {
            frame.rax = io::sys_pread64(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_pwrite64) => {
            frame.rax = io::sys_pwrite64(frame.rdi, frame.rsi, frame.rdx, frame.r10);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fsync) => {
            frame.rax = io::sys_fsync(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fdatasync) => {
            frame.rax = io::sys_fdatasync(frame.rdi);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_sync) => {
            frame.rax = io::sys_sync();
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_msync) => {
            frame.rax = io::sys_msync(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_fcntl) => {
            frame.rax = lock::sys_fcntl(frame.rdi, frame.rsi, frame.rdx);
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_flock) => {
            frame.rax = lock::sys_flock(frame.rdi, frame.rsi);
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

/// Dispatch each of `numbers` with zeroed arguments on a thread with no
/// `ProcessState` and check that it reached its own stub (`LAST_STUB`) and
/// came back as `-ENOSYS` by `sysret`: the per-cluster stub ktests' check.
///
/// A number without its own `dispatch` arm can't pass: the fallthrough arm
/// asks `with_current` for the process, which panics with none (the `diag`
/// line before each dispatch names the number in that crash's output).
/// A number this check is the first to report is forgotten again, so a
/// later call in the same boot still logs.
#[cfg(feature = "ktest")]
fn check_stubs(numbers: &[u32]) -> crate::ktest::TestResult {
    for &nr in numbers {
        let name = crate::linux::syscall_name(nr).unwrap_or("?");
        crate::ktest::diag(format_args!("dispatching syscall #{nr} ({name})"));
        crate::kassert!(nr < 1024);
        let (idx, bit) = ((nr / 64) as usize, 1u64 << (nr % 64));
        let reported_before = UNKNOWN_LOW_BITS[idx].load(Ordering::Relaxed) & bit != 0;
        LAST_STUB.store(u32::MAX, Ordering::Relaxed);
        let mut frame = SyscallFrame { rax: u64::from(nr), ..SyscallFrame::default() };
        let ret = dispatch(core::hint::black_box(&mut frame));
        if !reported_before {
            UNKNOWN_LOW_BITS[idx].fetch_and(!bit, Ordering::Relaxed);
        }
        crate::kassert_eq!(LAST_STUB.load(Ordering::Relaxed), nr);
        crate::kassert_eq!(ret, SyscallReturn::Sysret);
        crate::kassert_eq!(frame.rax, neg_errno(errno::ENOSYS));
    }
    Ok(())
}
