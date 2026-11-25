//! Process and system calls: `uname`, `arch_prctl`, `clock_gettime`,
//! `prlimit64`, `getrandom` (M6). M6's `rt_sigaction` and `rt_sigprocmask`
//! are in `signal` since #371.

use super::{DEFAULT_RLIMIT_NOFILE, IO_CHUNK, copy_pod_from_user, copy_pod_to_user, neg_errno};
use crate::arch;
use crate::linux::process::current_process;
use crate::linux::{ARCH_GET_FS, ARCH_SET_FS, default_utsname, errno, general};
use crate::sched;
use crate::user::{USER_MAX, copy_to_user, user_range_valid};

/// Cap bytes returned by a single `getrandom(2)` call to avoid issuing
/// thousands of synchronous `virtio-rng` requests in one syscall.
const MAX_GETRANDOM: usize = 4096;

/// Default stack rlimit advertised by M6 `prlimit64`: the initial user
/// stack's size (8 MiB).
const DEFAULT_RLIMIT_STACK: u64 = lumen_vm::USER_STACK_SIZE;

pub(super) fn sys_uname(buf: u64) -> u64 {
    let uts = default_utsname();
    // SAFETY: `Utsname` (`system::new_utsname`) is 6 x `[c_char; 65]` (390 bytes), zero padding.
    match unsafe { copy_pod_to_user(buf, &uts) } {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

pub(super) fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
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

pub(super) fn sys_clock_gettime(clockid: u64, tp: u64) -> u64 {
    let (sec, nsec) =
        if clockid == u64::from(general::CLOCK_REALTIME) || clockid == u64::from(general::CLOCK_REALTIME_COARSE) {
            crate::time::realtime_sec_nsec()
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

pub(super) fn sys_prlimit64(pid: u64, resource: u64, new_limit: u64, old_limit: u64) -> u64 {
    let current_pid = current_process().pid();
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

pub(super) fn sys_getrandom(buf: u64, buflen: u64, flags: u64) -> u64 {
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
