//! Scheduling, accounting, filesystem capacity and system information.

use super::at::{Dirfd, Fd, UserPath, UserPtr, lookup};
use super::{finish, neg_errno};
use crate::linux::{errno, general, process};
use crate::{sched, user};
use alloc::sync::Arc;
use lumen_vfs::{AtFlags, VfsCacheOps, VfsError};

pub(super) fn sys_sched_yield() -> u64 {
    sched::yield_now();
    0
}

fn thread(tid: i32) -> Result<Arc<sched::Thread>, u32> {
    let user = if tid == 0 { Some(process::current_thread()) } else { process::lookup_thread(tid as u32) };
    user.and_then(|t| t.scheduler_thread()).ok_or(errno::ESRCH)
}

pub(super) fn sys_sched_setaffinity(tid: i32, len: u32, mask: UserPtr) -> u64 {
    finish((|| {
        let mut bytes = [0; 8];
        // Linux's get_user_cpu_mask precedes the task lookup, including len == 0.
        if len != 0 {
            user::copy_from_user(&mut bytes[..(len as usize).min(8)], mask.addr()).map_err(|_| errno::EFAULT)?;
        }
        thread(tid)?.set_affinity(u64::from_ne_bytes(bytes)).map_err(|_| errno::EINVAL)?;
        Ok(0)
    })())
}

pub(super) fn sys_sched_getaffinity(tid: i32, len: u32, mask: UserPtr) -> u64 {
    finish((|| {
        if len < 8 || !len.is_multiple_of(8) {
            return Err(errno::EINVAL);
        }
        let mask_bits = thread(tid)?.affinity() & sched::online_mask();
        user::copy_to_user(mask.addr(), &mask_bits.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        Ok(8)
    })())
}

pub(super) fn sys_getpriority(which: i32, who: i32) -> u64 {
    finish(process::priority(which, who, None))
}

pub(super) fn sys_setpriority(which: i32, who: i32, prio: i32) -> u64 {
    finish(process::priority(which, who, Some(prio)).map(|_| 0))
}

fn write_words<const N: usize>(ptr: UserPtr, words: [u64; N]) -> Result<(), u32> {
    // Integer serialization avoids exposing padding in Linux's ABI structures.
    for (i, word) in words.into_iter().enumerate() {
        let addr = ptr.addr().checked_add((i * 8) as u64).ok_or(errno::EFAULT)?;
        user::copy_to_user(addr, &word.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
    }
    Ok(())
}

/// Write only measured CPU times; all other rusage fields are zero.
pub(super) fn write_usage(ptr: UserPtr, ticks: [u64; 2]) -> Result<(), u32> {
    let mut words = [0; 18];
    for (i, ticks) in ticks.into_iter().enumerate() {
        words[i * 2] = ticks / 1000;
        words[i * 2 + 1] = (ticks % 1000) * 1000;
    }
    write_words(ptr, words)
}

pub(super) fn sys_getrusage(who: i32, usage: UserPtr) -> u64 {
    finish((|| {
        let ticks = match who {
            0 => process::process_time(process::current_process().pid()),
            -1 => process::current_process().children_time(),
            1 => thread(0)?.cpu_time(),
            _ => return Err(errno::EINVAL),
        };
        write_usage(usage, ticks)?;
        Ok(0)
    })())
}

fn uptime_seconds(ns: u64) -> u64 {
    ns.div_ceil(1_000_000_000)
}

pub(super) fn sys_sysinfo(info: UserPtr) -> u64 {
    let frames = crate::mm::frame::counts();
    let mut words = [0; 14];
    words[0] = uptime_seconds(crate::time::now_ns());
    words[4] = frames.owned as u64 * 4096;
    words[5] = frames.free as u64 * 4096;
    words[7] = crate::vfs::page_cache::total_cached_pages() as u64 * 4096;
    words[10] = process::live_process_count().min(u16::MAX as usize) as u64;
    words[13] = 1; // mem_unit, with its tail padding zero.
    finish(write_words(info, words).map(|_| 0))
}

pub(super) fn sys_prctl(option: i32, arg2: u64, _arg3: u64, _arg4: u64, _arg5: u64) -> u64 {
    finish((|| {
        match option {
            15 => {
                let mut bytes = [0; 16];
                for (i, byte) in bytes[..15].iter_mut().enumerate() {
                    let addr = arg2.checked_add(i as u64).ok_or(errno::EFAULT)?;
                    user::copy_from_user(core::slice::from_mut(byte), addr).map_err(|_| errno::EFAULT)?;
                    if *byte == 0 {
                        break;
                    }
                }
                bytes[15] = 0;
                sched::set_current_comm(bytes);
            }
            16 => user::copy_to_user(arg2, &thread(0)?.comm()).map_err(|_| errno::EFAULT)?,
            _ => return Err(errno::EINVAL),
        }
        Ok(0)
    })())
}

fn statfs(dev: lumen_vfs::DevId, buf: UserPtr) -> Result<u64, u32> {
    let fs = crate::vfs::tables().fs_by_dev(dev).ok_or(errno::ENODEV)?;
    let stats = fs.statfs().map_err(VfsError::to_errno)?;
    write_words(buf, stats.words(dev, fs.read_only()))?;
    Ok(0)
}

pub(super) fn sys_statfs(path: UserPtr, buf: UserPtr) -> u64 {
    finish((|| {
        let path = UserPath::copy_in(path, false)?;
        let inode = lookup(Dirfd::Cwd, &path, AtFlags(0))?;
        statfs(inode.dev_id(), buf)
    })())
}

pub(super) fn sys_fstatfs(fd: Fd, buf: UserPtr) -> u64 {
    finish(fd.file().and_then(|file| statfs(file.inode().dev_id(), buf)))
}

pub(super) fn sys_clock_getres(clockid: i32, res: UserPtr) -> u64 {
    match clockid as u32 {
        general::CLOCK_REALTIME
        | general::CLOCK_REALTIME_COARSE
        | general::CLOCK_MONOTONIC
        | general::CLOCK_MONOTONIC_RAW
        | general::CLOCK_MONOTONIC_COARSE
        | general::CLOCK_BOOTTIME => {
            if res.is_null() {
                0
            } else {
                finish(write_words(res, [0, 1]).map(|_| 0))
            }
        }
        _ => neg_errno(errno::EINVAL),
    }
}

pub(super) fn sys_getcpu(cpu: UserPtr, node: UserPtr, _cache: UserPtr) -> u64 {
    finish((|| {
        let index = crate::arch::cpu_index() as u32;
        if !cpu.is_null() {
            user::copy_to_user(cpu.addr(), &index.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        }
        if !node.is_null() {
            user::copy_to_user(node.addr(), &0u32.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        }
        Ok(0)
    })())
}

pub(super) fn sys_setfsuid(_uid: u32) -> u64 {
    0
}
pub(super) fn sys_setfsgid(_gid: u32) -> u64 {
    0
}

const _: () = {
    assert!(core::mem::size_of::<general::rusage>() == 18 * 8);
    assert!(core::mem::size_of::<general::statfs>() == 15 * 8);
    assert!(core::mem::size_of::<crate::linux::system::sysinfo>() == 14 * 8);
};

#[cfg(feature = "ktest")]
mod tests {
    use super::super::at::reg;
    use super::*;
    use crate::{kassert, kassert_eq};
    use process::{KTEST_USER_VA, in_test_process};

    crate::ktest!(
        fn sysinfo_uptime_rounds_up() {
            for (ns, seconds) in
                [(0, 0), (1, 1), (999_999_999, 1), (1_000_000_000, 1), (1_000_000_001, 2), (u64::MAX, 18_446_744_074)]
            {
                kassert_eq!(uptime_seconds(ns), seconds);
            }
            Ok(())
        }
    );

    crate::ktest!(
        fn sysinfo_reports_free_frames_and_live_processes() {
            in_test_process(|_| {
                let ptr = reg(KTEST_USER_VA);
                kassert_eq!(sys_sysinfo(ptr), 0);
                // Only init and this test process exist; APs are idle and
                // the runner is blocked. The output pages are eager-mapped.
                kassert_eq!(words::<14>()?[10], 2);
                let free = crate::mm::frame::counts().free as u64 * 4096;
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[5], free);
                let frame = crate::mm::frame::alloc_owned().ok_or("sysinfo test frame")?;
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[5], free - 4096);
                drop(frame);
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[5], free);
                in_test_process(|_| {
                    kassert_eq!(sys_sysinfo(reg(KTEST_USER_VA)), 0);
                    kassert_eq!(words::<14>()?[10], 3);
                    Ok(())
                })??;
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[10], 2);
                Ok(())
            })?
        }
    );

    fn words<const N: usize>() -> Result<[u64; N], &'static str> {
        let mut out = [0; N];
        for (i, word) in out.iter_mut().enumerate() {
            let mut bytes = [0; 8];
            user::copy_from_user(&mut bytes, KTEST_USER_VA + i as u64 * 8).map_err(|_| "copy words")?;
            *word = u64::from_ne_bytes(bytes);
        }
        Ok(out)
    }

    crate::ktest!(
        fn small_syscalls_copyout_and_errors() {
            in_test_process(|pid| {
                let ptr = reg(KTEST_USER_VA);
                kassert_eq!(sys_sched_yield(), 0);
                kassert_eq!(sys_getpriority(0, 0), 20);
                kassert_eq!(sys_setpriority(0, pid as i32, -50), 0);
                kassert_eq!(sys_getpriority(0, 0), 40);
                kassert_eq!(sys_setpriority(0, 0, 99), 0);
                kassert_eq!(sys_getpriority(0, pid as i32), 1);
                kassert_eq!(sys_getpriority(3, 0), neg_errno(errno::EINVAL));
                kassert_eq!(sys_setpriority(2, 123, 0), neg_errno(errno::ESRCH));
                kassert_eq!(sys_getpriority(0, -1), neg_errno(errno::ESRCH));
                kassert_eq!(sys_sched_getaffinity(0, 7, ptr), neg_errno(errno::EINVAL));
                kassert_eq!(sys_sched_getaffinity(0, 9, ptr), neg_errno(errno::EINVAL));
                kassert_eq!(sys_sched_getaffinity(-1, 8, ptr), neg_errno(errno::ESRCH));
                kassert_eq!(sys_sched_getaffinity(0, 8, reg(1)), neg_errno(errno::EFAULT));
                kassert_eq!(sys_sched_getaffinity(0, 16, ptr), 8);
                kassert_eq!(words::<1>()?[0], thread(0).map_err(|_| "thread")?.affinity() & sched::online_mask());
                // The test thread is permanently pinned: even its existing mask is refused.
                kassert_eq!(sys_sched_setaffinity(0, 8, ptr), neg_errno(errno::EINVAL));
                kassert_eq!(sys_sched_setaffinity(0, 8, reg(1)), neg_errno(errno::EFAULT));
                for id in [0, 1, 4, 5, 6, 7] {
                    kassert_eq!(sys_clock_getres(id, reg(0)), 0);
                    kassert_eq!(sys_clock_getres(id, ptr), 0);
                    kassert_eq!(words::<2>()?, [0, 1]);
                }
                kassert_eq!(sys_clock_getres(2, ptr), neg_errno(errno::EINVAL));
                kassert_eq!(sys_clock_getres(0, reg(1)), neg_errno(errno::EFAULT));
                kassert_eq!(sys_getcpu(reg(0), reg(0), reg(1)), 0);
                kassert_eq!(sys_getcpu(reg(1), reg(0), reg(0)), neg_errno(errno::EFAULT));
                kassert_eq!(sys_getcpu(reg(0), reg(1), reg(0)), neg_errno(errno::EFAULT));
                kassert_eq!(sys_getrusage(2, ptr), neg_errno(errno::EINVAL));
                kassert_eq!(sys_getrusage(0, reg(1)), neg_errno(errno::EFAULT));
                write_usage(ptr, [1234, 5678]).map_err(|_| "usage copy")?;
                let usage = words::<18>()?;
                kassert_eq!(&usage[..4], &[1, 234000, 5, 678000]);
                kassert!(usage[4..].iter().all(|&v| v == 0));
                kassert_eq!(sys_sysinfo(reg(1)), neg_errno(errno::EFAULT));
                let uptime = crate::time::now_ns() / 1_000_000_000;
                kassert_eq!(sys_sysinfo(ptr), 0);
                let info = words::<14>()?;
                kassert!(info[0] >= uptime);
                kassert_eq!(info[4], crate::mm::frame::counts().owned as u64 * 4096);
                kassert!(info[5] <= info[4]);
                kassert!(info[10] >= 2);
                kassert_eq!(info[13], 1);
                for i in [1, 2, 3, 6, 8, 9, 11, 12] {
                    kassert_eq!(info[i], 0);
                }
                kassert_eq!(sys_setfsuid(u32::MAX), 0);
                kassert_eq!(sys_setfsgid(u32::MAX), 0);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn small_syscalls_names_and_stack_limits() {
            in_test_process(|_| {
                let ptr = reg(KTEST_USER_VA);
                let bytes =
                    [b'a', 0xff, b'\n', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l', b'm', b'n'];
                user::copy_to_user(KTEST_USER_VA, &bytes).map_err(|_| "name")?;
                kassert_eq!(sys_prctl(15, KTEST_USER_VA, 0, 0, 0), 0);
                kassert_eq!(sys_prctl(16, KTEST_USER_VA, 0, 0, 0), 0);
                let mut got = [0; 16];
                user::copy_from_user(&mut got, KTEST_USER_VA).map_err(|_| "name read")?;
                kassert_eq!(&got[..15], &bytes[..15]);
                kassert_eq!(got[15], 0);
                kassert_eq!(sys_prctl(15, 1, 0, 0, 0), neg_errno(errno::EFAULT));
                kassert_eq!(sys_prctl(16, 1, 0, 0, 0), neg_errno(errno::EFAULT));
                kassert_eq!(sys_prctl(-1, 0, 0, 0, 0), neg_errno(errno::EINVAL));
                // Exec's setter updates the same comm owner.
                sched::set_current_name(sched::TaskName::from_comm(b"after-exec"));
                kassert_eq!(sys_prctl(16, KTEST_USER_VA, 0, 0, 0), 0);
                user::copy_from_user(&mut got, KTEST_USER_VA).map_err(|_| "exec name")?;
                kassert_eq!(&got, b"after-exec\0\0\0\0\0\0");
                let limits = [16 * 1024 * 1024, 64 * 1024 * 1024];
                write_words(ptr, limits).map_err(|_| "limits")?;
                kassert_eq!(super::super::proc::sys_prlimit64(0, 3, KTEST_USER_VA, KTEST_USER_VA + 16), 0);
                kassert_eq!(
                    process::current_process().rlimit(process::RlimitKind::Stack, None),
                    Ok(process::Rlimit::new(limits[0], limits[1]).unwrap())
                );
                kassert_eq!(super::super::proc::sys_prlimit64(0, 3, 0, KTEST_USER_VA), 0);
                kassert_eq!(words::<2>()?, limits);
                write_words(ptr, [2, 1]).map_err(|_| "bad limits")?;
                kassert_eq!(super::super::proc::sys_prlimit64(0, 3, KTEST_USER_VA, 0), neg_errno(errno::EINVAL));
                kassert_eq!(
                    process::current_process().rlimit(process::RlimitKind::Stack, None),
                    Ok(process::Rlimit::new(limits[0], limits[1]).unwrap())
                );
                kassert_eq!(super::super::proc::sys_prlimit64(0, 3, 1, 0), neg_errno(errno::EFAULT));
                kassert_eq!(super::super::proc::sys_prlimit64(u32::MAX as u64, 3, 0, 0), neg_errno(errno::ESRCH));
                kassert_eq!(super::super::proc::sys_prlimit64(0, 16, 0, 0), neg_errno(errno::EINVAL));
                Ok(())
            })?
        }
    );
    crate::ktest!(
        fn priority_and_usage_include_nonleader_threads() {
            use core::sync::atomic::{AtomicBool, Ordering};
            in_test_process(|pid| {
                let other = process::add_ktest_thread(&process::current_process())?;
                let bound = Arc::clone(&other);
                let done = Arc::new(AtomicBool::new(false));
                let queue = Arc::new(sched::WaitQueue::new());
                let (end, wait) = (Arc::clone(&done), Arc::clone(&queue));
                let handle = match sched::spawn("nice-target", move || {
                    bound.bind_kernel_thread(&sched::current().unwrap());
                    while sched::current().unwrap().cpu_time()[1] == 0 {
                        core::hint::spin_loop();
                    }
                    wait.wait_until(|| end.load(Ordering::Acquire));
                }) {
                    Ok(handle) => handle,
                    Err(_) => {
                        process::remove_ktest_thread(&other);
                        return Err("spawn priority target");
                    }
                };
                let result = (|| {
                    kassert!(sched::yield_until_blocked(
                        core::iter::once(&handle),
                        Some(crate::time::now_ns() + 10_000_000_000)
                    ));
                    kassert_eq!(sys_setpriority(0, other.tid() as i32, -7), 0);
                    kassert_eq!(sys_getpriority(0, other.tid() as i32), 27);
                    kassert_eq!(sys_getpriority(0, pid as i32), 20);
                    kassert_eq!(sys_getpriority(1, 0), 27);
                    kassert_eq!(sys_getpriority(2, 0), 27);
                    kassert_eq!(sys_setpriority(1, 0, 4), 0);
                    kassert_eq!(handle.thread().nice(), 4);
                    kassert_eq!(sys_getpriority(0, 0), 16);
                    kassert_eq!(sys_setpriority(2, 0, 9), 0);
                    kassert_eq!(handle.thread().nice(), 9);
                    kassert_eq!(sys_getpriority(0, 0), 11);
                    {
                        let _irq = crate::arch::irq::push_off();
                        let me = sched::current().unwrap().cpu_time();
                        let extra = handle.thread().cpu_time();
                        kassert!(extra[1] > 0);
                        kassert_eq!(process::process_time(pid), [me[0] + extra[0], me[1] + extra[1]]);
                    }
                    Ok(())
                })();
                // The worker remains blocked while its charges move to dead-thread totals.
                let transfer = {
                    let _irq = crate::arch::irq::push_off();
                    let before = process::process_time(pid);
                    process::remove_ktest_thread(&other);
                    process::process_time(pid) == before
                };
                done.store(true, Ordering::Release);
                queue.wake_all();
                handle.join();
                result?;
                kassert!(transfer);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn statfs_tmpfs_tracks_quota_consumption() {
            use lumen_vfs::FileSystem;
            let fs = crate::vfs::tmpfs::Tmpfs::new(lumen_vfs::DevId::makedev(0, 391), 64 * 1024);
            let before = fs.statfs().map_err(|_| "statfs")?;
            let root = fs.root_inode();
            let file = root.create(b"statfs", 0o600).map_err(|_| "create")?;
            file.write_at(0, &[0; 4096]).map_err(|_| "write")?;
            let written = fs.statfs().map_err(|_| "statfs after write")?;
            kassert_eq!(written.blocks, before.blocks);
            kassert!(written.free_blocks < before.free_blocks);
            kassert!(written.free_files < before.free_files);
            root.unlink(b"statfs").map_err(|_| "unlink")?;
            drop(file);
            kassert_eq!(fs.statfs().map_err(|_| "statfs after unlink")?, before);
            Ok(())
        }
    );
    crate::ktest!(
        fn sysinfo_reports_populated_page_cache() {
            use lumen_vfs::FileSystem;
            in_test_process(|_| {
                let ptr = reg(KTEST_USER_VA);
                kassert_eq!(sys_sysinfo(ptr), 0);
                let before = words::<14>()?[7];
                let fs = crate::vfs::tmpfs::Tmpfs::new(lumen_vfs::DevId::makedev(0, 392), 64 * 1024);
                let root = fs.root_inode();
                let file = root.create(b"cached", 0o600).map_err(|_| "create cache file")?;
                for index in 0..3 {
                    kassert_eq!(file.write_at(index * 4096, &[0x5a; 4096]).map_err(|_| "populate cache")?, 4096);
                }
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[7], before + 3 * 4096);
                drop((file, root, fs));
                kassert_eq!(sys_sysinfo(ptr), 0);
                kassert_eq!(words::<14>()?[7], before);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn prlimit_nofile_enforces_each_allocation_path() {
            use super::super::{at, fd, lock, pipe, poll, proc};
            use lumen_vfs::OpenFlags;
            in_test_process(|_| {
                let ptr = reg(KTEST_USER_VA + 2048);
                let out_va = KTEST_USER_VA + 2048 + 16;
                let pipe_va = KTEST_USER_VA + 2048 + 32;
                let read_pair = |va: u64| -> Result<[u64; 2], &'static str> {
                    let mut out = [0u64; 2];
                    for (i, word) in out.iter_mut().enumerate() {
                        let mut bytes = [0u8; 8];
                        user::copy_from_user(&mut bytes, va + i as u64 * 8).map_err(|_| "read rlimit")?;
                        *word = u64::from_ne_bytes(bytes);
                    }
                    Ok(out)
                };
                let max_fds = lumen_vfs::MAX_FDS as u64;

                // Default RLIMIT_NOFILE is [MAX_FDS, MAX_FDS].
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), 0, out_va), 0);
                kassert_eq!(read_pair(out_va)?, [max_fds, max_fds]);

                // soft > hard -> EINVAL (precedes EPERM even above MAX_FDS); > MAX_FDS -> EPERM.
                write_words(ptr, [max_fds + 2, max_fds + 1]).map_err(|_| "write bad")?;
                kassert_eq!(
                    proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0),
                    neg_errno(errno::EINVAL)
                );
                write_words(ptr, [max_fds, max_fds + 1]).map_err(|_| "write high")?;
                kassert_eq!(
                    proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0),
                    neg_errno(errno::EPERM)
                );
                write_words(ptr, [u64::MAX, u64::MAX]).map_err(|_| "write inf")?;
                kassert_eq!(
                    proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0),
                    neg_errno(errno::EPERM)
                );
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), 0, out_va), 0);
                kassert_eq!(read_pair(out_va)?, [max_fds, max_fds]);

                // Lower soft limit to 5, hard to 10; old_limit returns previous [1024, 1024].
                write_words(ptr, [5, 10]).map_err(|_| "write [5, 10]")?;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), out_va), 0);
                kassert_eq!(read_pair(out_va)?, [max_fds, max_fds]);
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), 0, out_va), 0);
                kassert_eq!(read_pair(out_va)?, [5, 10]);

                // Stdio occupies 0, 1, 2: open gets 3, openat gets 4, next fails EMFILE without creating a file.
                let null = fd::ktest_user_path(0, b"/dev/null")?;
                let creat_path = fd::ktest_user_path(1, b"/tmp/nofile-ktest-emfile")?;
                let rd = OpenFlags::from_bits(OpenFlags::O_RDONLY);
                let creat = OpenFlags::from_bits(OpenFlags::O_WRONLY | OpenFlags::O_CREAT);
                kassert_eq!(fd::sys_open(null, rd, at::reg(0)), 3);
                kassert_eq!(fd::sys_openat(Dirfd::Cwd, null, rd, at::reg(0)), 4);
                kassert_eq!(fd::sys_open(null, rd, at::reg(0)), neg_errno(errno::EMFILE));
                kassert_eq!(fd::sys_openat(Dirfd::Cwd, null, rd, at::reg(0)), neg_errno(errno::EMFILE));
                kassert_eq!(fd::sys_open(creat_path, creat, at::reg(0o644)), neg_errno(errno::EMFILE));
                kassert!(matches!(
                    crate::vfs::resolve_path_from_root("/tmp/nofile-ktest-emfile"),
                    Err(VfsError::NoEnt)
                ));

                // dup, fcntl(F_DUPFD*), dup2, dup3 at limit 5.
                kassert_eq!(pipe::sys_dup(at::reg(1)), neg_errno(errno::EMFILE));
                kassert_eq!(lock::sys_fcntl(at::reg(1), general::F_DUPFD, 0), neg_errno(errno::EMFILE));
                kassert_eq!(lock::sys_fcntl(at::reg(1), general::F_DUPFD_CLOEXEC, 3), neg_errno(errno::EMFILE));
                kassert_eq!(lock::sys_fcntl(at::reg(1), general::F_DUPFD, 5), neg_errno(errno::EINVAL));
                kassert_eq!(lock::sys_fcntl(at::reg(1), general::F_DUPFD_CLOEXEC, 5), neg_errno(errno::EINVAL));
                kassert_eq!(pipe::sys_dup2(at::reg(1), at::reg(5)), neg_errno(errno::EBADF));
                kassert_eq!(
                    pipe::sys_dup3(at::reg(1), at::reg(5), OpenFlags::from_bits(OpenFlags::O_CLOEXEC)),
                    neg_errno(errno::EBADF)
                );
                // dup2 displacing an existing slot below limit still succeeds when full.
                kassert_eq!(pipe::sys_dup2(at::reg(1), at::reg(4)), 4);
                // poll checks nfds against the live soft RLIMIT_NOFILE.
                kassert_eq!(poll::sys_poll(ptr.addr(), 6, 0), neg_errno(errno::EINVAL));

                // pipe / pipe2 require 2 free slots; with 1 free slot (fd 4) they return EMFILE without leaking it.
                kassert_eq!(fd::sys_close(at::reg(4)), 0);
                kassert_eq!(pipe::sys_pipe(reg(pipe_va)), neg_errno(errno::EMFILE));
                kassert_eq!(
                    pipe::sys_pipe2(reg(pipe_va), OpenFlags::from_bits(OpenFlags::O_CLOEXEC)),
                    neg_errno(errno::EMFILE)
                );
                kassert_eq!(fd::sys_open(null, rd, at::reg(0)), 4);

                // With 2 free slots (3, 4), pipe2 succeeds and next pipe returns EMFILE.
                kassert_eq!(fd::sys_close(at::reg(3)), 0);
                kassert_eq!(fd::sys_close(at::reg(4)), 0);
                kassert_eq!(pipe::sys_pipe2(reg(pipe_va), OpenFlags::from_bits(OpenFlags::O_CLOEXEC)), 0);
                let mut pair = [0u8; 8];
                user::copy_from_user(&mut pair, pipe_va).map_err(|_| "pipe fds")?;
                kassert_eq!(
                    [
                        i32::from_ne_bytes(pair[0..4].try_into().unwrap()),
                        i32::from_ne_bytes(pair[4..8].try_into().unwrap())
                    ],
                    [3, 4]
                );
                kassert_eq!(pipe::sys_pipe(reg(pipe_va)), neg_errno(errno::EMFILE));

                // Lowering soft limit below open fds keeps them usable (including dup2(fd, fd))
                // while refusing new allocations.
                write_words(ptr, [3, 10]).map_err(|_| "write [3, 10]")?;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0), 0);
                kassert_eq!(lock::sys_fcntl(at::reg(4), general::F_GETFD, 0), u64::from(general::FD_CLOEXEC));
                kassert_eq!(pipe::sys_dup2(at::reg(4), at::reg(4)), 4);
                kassert_eq!(pipe::sys_dup3(at::reg(4), at::reg(4), OpenFlags::from_bits(0)), neg_errno(errno::EINVAL));
                kassert_eq!(pipe::sys_dup(at::reg(4)), neg_errno(errno::EMFILE));
                kassert_eq!(fd::sys_close(at::reg(4)), 0);
                kassert_eq!(fd::sys_close(at::reg(3)), 0);

                // Soft limit 0: open/pipe/dup return EMFILE; fcntl(F_DUPFD, 0) returns EINVAL;
                // dup2(0, 1)/dup3(0, 1, 0) return EBADF; dup2(0, 0) still returns 0.
                write_words(ptr, [0, 10]).map_err(|_| "write [0, 10]")?;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0), 0);
                kassert_eq!(fd::sys_open(null, rd, at::reg(0)), neg_errno(errno::EMFILE));
                kassert_eq!(fd::sys_openat(Dirfd::Cwd, null, rd, at::reg(0)), neg_errno(errno::EMFILE));
                kassert_eq!(pipe::sys_pipe(reg(pipe_va)), neg_errno(errno::EMFILE));
                kassert_eq!(pipe::sys_pipe2(reg(pipe_va), OpenFlags::from_bits(0)), neg_errno(errno::EMFILE));
                kassert_eq!(pipe::sys_dup(at::reg(0)), neg_errno(errno::EMFILE));
                kassert_eq!(lock::sys_fcntl(at::reg(0), general::F_DUPFD, 0), neg_errno(errno::EINVAL));
                kassert_eq!(lock::sys_fcntl(at::reg(0), general::F_DUPFD_CLOEXEC, 0), neg_errno(errno::EINVAL));
                kassert_eq!(pipe::sys_dup2(at::reg(0), at::reg(1)), neg_errno(errno::EBADF));
                kassert_eq!(pipe::sys_dup3(at::reg(0), at::reg(1), OpenFlags::from_bits(0)), neg_errno(errno::EBADF));
                kassert_eq!(pipe::sys_dup2(at::reg(0), at::reg(0)), 0);
                kassert_eq!(poll::sys_poll(ptr.addr(), 1, 0), neg_errno(errno::EINVAL));
                kassert_eq!(poll::sys_poll(0, 0, 0), 0);

                // Raising back to MAX_FDS succeeds and allows allocation again.
                write_words(ptr, [max_fds, max_fds]).map_err(|_| "write max")?;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NOFILE), ptr.addr(), 0), 0);
                kassert_eq!(fd::sys_open(null, rd, at::reg(0)), 3);
                kassert_eq!(fd::sys_close(at::reg(3)), 0);
                Ok(())
            })?
        }
    );

    crate::ktest!(
        fn prlimit_nproc_read_write_and_validation() {
            use super::super::proc;
            in_test_process(|_| {
                let ptr = reg(KTEST_USER_VA);
                let out_va = KTEST_USER_VA + 16;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NPROC), 0, KTEST_USER_VA), 0);
                kassert_eq!(words::<2>()?, [u64::MAX, u64::MAX]);

                write_words(ptr, [5, 4]).map_err(|_| "bad nproc")?;
                kassert_eq!(
                    proc::sys_prlimit64(0, u64::from(general::RLIMIT_NPROC), KTEST_USER_VA, 0),
                    neg_errno(errno::EINVAL)
                );
                kassert_eq!(
                    process::current_process().rlimit(process::RlimitKind::Nproc, None),
                    Ok(process::Rlimit::INFINITY)
                );

                write_words(ptr, [0, 3]).map_err(|_| "set nproc")?;
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NPROC), KTEST_USER_VA, out_va), 0);
                kassert_eq!(
                    process::current_process().rlimit(process::RlimitKind::Nproc, None),
                    Ok(process::Rlimit::new(0, 3).unwrap())
                );
                kassert_eq!(proc::sys_prlimit64(0, u64::from(general::RLIMIT_NPROC), 0, KTEST_USER_VA), 0);
                kassert_eq!(words::<2>()?, [0, 3]);
                Ok(())
            })?
        }
    );
}
