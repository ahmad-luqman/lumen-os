//! Linux x86-64 UAPI constants, syscall dispatcher, process state, boot module
//! loader, and the `linux` shell command (M6–M10).
//!
//! Backed by the pinned `linux-raw-sys` crate (`=0.12.1`, elected license: MIT,
//! zero transitive dependencies; see `docs/ARCHITECTURE.md` sections 2 and 8).
//! Re-exports the UAPI modules so ring-3 syscall and ELF loader modules share
//! one canonical ABI surface.

pub mod modules;
pub mod process;
pub mod syscall;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

#[allow(unused_imports)]
pub use linux_raw_sys::{auxvec, ctypes, errno, general, ioctl, prctl, system};

use crate::arch;
use crate::linux::process::ConsoleIo;
use crate::mm::frame;
use crate::mm::user_vm::AddressSpace;
use crate::shell::Ctx;
use crate::term::Place;
use crate::user::{UserExit, run_elf_process};

/// Linux x86-64 `struct new_utsname` written by `uname(2)` (syscall 63).
pub type Utsname = system::new_utsname;

/// `arch_prctl(2)` code to set the 64-bit base for the `FS` register.
#[allow(unused_imports)]
pub use general::ARCH_SET_FS;

/// `arch_prctl(2)` code to read the 64-bit base for the `FS` register.
///
/// `linux-raw-sys` 0.12.1 exports `ARCH_SET_FS` (`0x1002`) from `<asm/prctl.h>`
/// but omits `ARCH_GET_FS` (`0x1003`), so we define it here beside `ARCH_SET_FS`.
pub const ARCH_GET_FS: u32 = 0x1003;

/// The 30 initial M6 Linux x86-64 syscalls (`docs/ARCHITECTURE.md` section 8).
pub const M6_SYSCALLS: &[(u32, &str)] = &[
    (general::__NR_read, "read"),
    (general::__NR_write, "write"),
    (general::__NR_close, "close"),
    (general::__NR_fstat, "fstat"),
    (general::__NR_mmap, "mmap"),
    (general::__NR_mprotect, "mprotect"),
    (general::__NR_munmap, "munmap"),
    (general::__NR_brk, "brk"),
    (general::__NR_rt_sigaction, "rt_sigaction"),
    (general::__NR_rt_sigprocmask, "rt_sigprocmask"),
    (general::__NR_ioctl, "ioctl"),
    (general::__NR_readv, "readv"),
    (general::__NR_writev, "writev"),
    (general::__NR_getpid, "getpid"),
    (general::__NR_exit, "exit"),
    (general::__NR_uname, "uname"),
    (general::__NR_getcwd, "getcwd"),
    (general::__NR_getuid, "getuid"),
    (general::__NR_getgid, "getgid"),
    (general::__NR_geteuid, "geteuid"),
    (general::__NR_getegid, "getegid"),
    (general::__NR_getppid, "getppid"),
    (general::__NR_getpgrp, "getpgrp"),
    (general::__NR_arch_prctl, "arch_prctl"),
    (general::__NR_gettid, "gettid"),
    (general::__NR_set_tid_address, "set_tid_address"),
    (general::__NR_clock_gettime, "clock_gettime"),
    (general::__NR_exit_group, "exit_group"),
    (general::__NR_prlimit64, "prlimit64"),
    (general::__NR_getrandom, "getrandom"),
];

/// The 47 Linux x86-64 syscalls M7 adds (`docs/ARCHITECTURE.md` section 8's
/// M7 table, less `mmap` (9) and `getcwd` (79), which M6 already has and M7
/// extends). Each has its own arm in `syscall::dispatch`; one not yet
/// implemented is a stub returning `-ENOSYS`.
pub const M7_SYSCALLS: &[(u32, &str)] = &[
    (general::__NR_open, "open"),
    (general::__NR_stat, "stat"),
    (general::__NR_lstat, "lstat"),
    (general::__NR_lseek, "lseek"),
    (general::__NR_pread64, "pread64"),
    (general::__NR_pwrite64, "pwrite64"),
    (general::__NR_access, "access"),
    (general::__NR_msync, "msync"),
    (general::__NR_fcntl, "fcntl"),
    (general::__NR_flock, "flock"),
    (general::__NR_fsync, "fsync"),
    (general::__NR_fdatasync, "fdatasync"),
    (general::__NR_truncate, "truncate"),
    (general::__NR_ftruncate, "ftruncate"),
    (general::__NR_chdir, "chdir"),
    (general::__NR_fchdir, "fchdir"),
    (general::__NR_rename, "rename"),
    (general::__NR_mkdir, "mkdir"),
    (general::__NR_rmdir, "rmdir"),
    (general::__NR_link, "link"),
    (general::__NR_unlink, "unlink"),
    (general::__NR_symlink, "symlink"),
    (general::__NR_readlink, "readlink"),
    (general::__NR_chmod, "chmod"),
    (general::__NR_fchmod, "fchmod"),
    (general::__NR_chown, "chown"),
    (general::__NR_fchown, "fchown"),
    (general::__NR_lchown, "lchown"),
    (general::__NR_umask, "umask"),
    (general::__NR_sync, "sync"),
    (general::__NR_getdents64, "getdents64"),
    (general::__NR_openat, "openat"),
    (general::__NR_mkdirat, "mkdirat"),
    (general::__NR_fchownat, "fchownat"),
    (general::__NR_newfstatat, "newfstatat"),
    (general::__NR_unlinkat, "unlinkat"),
    (general::__NR_renameat, "renameat"),
    (general::__NR_linkat, "linkat"),
    (general::__NR_symlinkat, "symlinkat"),
    (general::__NR_readlinkat, "readlinkat"),
    (general::__NR_fchmodat, "fchmodat"),
    (general::__NR_faccessat, "faccessat"),
    (general::__NR_utimensat, "utimensat"),
    (general::__NR_renameat2, "renameat2"),
    (general::__NR_statx, "statx"),
    (general::__NR_faccessat2, "faccessat2"),
    (general::__NR_fchmodat2, "fchmodat2"),
];

/// Look up the name of an M6 or M7 Linux x86-64 syscall by number.
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    M6_SYSCALLS.iter().chain(M7_SYSCALLS).find_map(|&(num, name)| (num == nr).then_some(name))
}

/// Copy an ASCII string into a fixed-size NUL-terminated `c_char` field,
/// zeroing all trailing bytes.
fn write_uts_field(dst: &mut [ctypes::c_char; 65], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(dst.len() - 1);
    for (slot, &b) in dst.iter_mut().zip(&bytes[..len]) {
        *slot = b as ctypes::c_char;
    }
    dst[len..].fill(0);
}

/// Read the NUL-terminated UTF-8 prefix of a `Utsname` field.
fn read_uts_field(src: &[ctypes::c_char; 65]) -> &str {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    let bytes = zerocopy::IntoBytes::as_bytes(&src[..len]);
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Default `uname(2)` payload advertised to Linux userspace (`ARCHITECTURE.md` §8).
#[inline(never)]
pub fn default_utsname() -> Utsname {
    let mut uts = Utsname {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
        domainname: [0; 65],
    };
    write_uts_field(&mut uts.sysname, "Linux");
    write_uts_field(&mut uts.nodename, "lumen");
    write_uts_field(&mut uts.release, "6.6.0-lumen");
    write_uts_field(&mut uts.version, "#1 SMP Lumen");
    write_uts_field(&mut uts.machine, "x86_64");
    write_uts_field(&mut uts.domainname, "(none)");
    uts
}

/// Log a debug summary of the linked Linux x86-64 ABI table at boot.
///
/// Called from `kmain_on_kernel_stack` so `linux-raw-sys` types and constants
/// are genuinely linked into both `lumen-kernel` and `lumen-kernel-ktest` and
/// covered by `make audit`.
pub fn log_abi_summary() {
    let uts = default_utsname();
    let uname_nr = general::__NR_uname;
    let uname_name = syscall_name(uname_nr).unwrap_or("?");
    let mods = modules::list();
    log::debug!(
        target: "linux",
        "abi: {} M6 syscalls, {} M7 (#{uname_nr} {uname_name}: {} {} {}, -ENOSYS={}), {} boot module(s)",
        M6_SYSCALLS.len(),
        M7_SYSCALLS.len(),
        read_uts_field(&uts.sysname),
        read_uts_field(&uts.release),
        read_uts_field(&uts.machine),
        -(errno::ENOSYS as i64),
        mods.len(),
    );
}

/// Build 16 random bytes for `AT_RANDOM`, using `crate::rng::fill` when an
/// entropy source is present and falling back to TSC/time mixing otherwise.
fn at_random_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    if crate::rng::fill(&mut out).is_err() {
        let a = crate::time::now_ns() ^ arch::rdtsc().rotate_left(13);
        let b = arch::rdtsc().wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ a.rotate_right(29);
        out[0..8].copy_from_slice(&a.to_le_bytes());
        out[8..16].copy_from_slice(&b.to_le_bytes());
    }
    out
}

/// Shell command: `linux` (list available Limine Linux binaries) or
/// `linux <name> [args...]` (load and run a static Linux x86-64 ELF64 binary).
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    if args.is_empty() {
        let mods = modules::list();
        if mods.is_empty() {
            let _ = writeln!(ctx.out, "linux: no boot modules loaded");
            return;
        }
        let _ = writeln!(ctx.out, "linux: {} boot module(s):", mods.len());
        for m in mods {
            let kib = m.bytes.len().div_ceil(1024);
            let _ = writeln!(ctx.out, "  {} ({kib} KiB)", m.name);
        }
        return;
    }

    if ctx.piped() {
        return ctx.fail("linux: not in a pipeline");
    }

    let io = match ctx.out.place() {
        Place::Anywhere => ConsoleIo::Serial,
        Place::Vt(vt) => ConsoleIo::Vt(vt),
        Place::Window(_) => return ctx.fail("linux: run from serial or a virtual terminal"),
    };

    let name = &args[0];
    let Some(module) = modules::find(name) else {
        return ctx.fail(format_args!("linux: unknown binary '{name}' (run 'linux' to list)"));
    };

    let argv: Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();
    let envp: [&[u8]; 3] = [b"PATH=/bin:/usr/bin", b"HOME=/", b"TERM=vt100"];
    let execfn = name.as_bytes();
    let random = at_random_bytes();

    // Pre-warm kernel heap so process/VMA/stack metadata reuses mapped heap
    // pages and physical frame accounting reflects only user-space frames.
    let _warm: Vec<u8> = vec![0u8; 32 * 1024];
    drop(_warm);

    let frames_before = frame::free_frames();
    let (space, prog) = match AddressSpace::from_elf(module.bytes, &argv, &envp, execfn, random) {
        Ok(loaded) => loaded,
        Err(e) => return ctx.fail(format_args!("linux: failed to load '{name}': {e}")),
    };
    let space = Arc::new(space);
    let exit = match run_elf_process(module.name, space, prog.entry, prog.rsp, io) {
        Ok(exit) => exit,
        Err(e) => return ctx.fail(format_args!("linux: failed to run '{name}': {e}")),
    };
    let frames_after = frame::free_frames();
    let leaked_frames = frames_before as i64 - frames_after as i64;

    match exit {
        UserExit::Exited(code) => {
            let _ = writeln!(ctx.out, "linux: {name} exit={code} leaked_frames={leaked_frames}");
        }
        UserExit::Fault { vector, error_code, rip, cr2 } => {
            let _ = writeln!(
                ctx.out,
                "linux: {name} fault vector={vector} ({}) error={error_code:#x} rip={rip:#x} cr2={cr2:#x} leaked_frames={leaked_frames}",
                arch::idt::exception_name(vector),
            );
        }
        UserExit::NonCanonicalRip(rip) => {
            let _ = writeln!(ctx.out, "linux: {name} non-canonical rip={rip:#x} leaked_frames={leaked_frames}");
        }
    }
}

crate::ktest!(
    fn abi_constants_match_spec() {
        use core::mem::size_of;

        // 1. All 30 M6 Linux x86-64 syscall numbers (ARCHITECTURE.md §8).
        let expected_syscalls: &[(u32, u32, &str)] = &[
            (general::__NR_read, 0, "read"),
            (general::__NR_write, 1, "write"),
            (general::__NR_close, 3, "close"),
            (general::__NR_fstat, 5, "fstat"),
            (general::__NR_mmap, 9, "mmap"),
            (general::__NR_mprotect, 10, "mprotect"),
            (general::__NR_munmap, 11, "munmap"),
            (general::__NR_brk, 12, "brk"),
            (general::__NR_rt_sigaction, 13, "rt_sigaction"),
            (general::__NR_rt_sigprocmask, 14, "rt_sigprocmask"),
            (general::__NR_ioctl, 16, "ioctl"),
            (general::__NR_readv, 19, "readv"),
            (general::__NR_writev, 20, "writev"),
            (general::__NR_getpid, 39, "getpid"),
            (general::__NR_exit, 60, "exit"),
            (general::__NR_uname, 63, "uname"),
            (general::__NR_getcwd, 79, "getcwd"),
            (general::__NR_getuid, 102, "getuid"),
            (general::__NR_getgid, 104, "getgid"),
            (general::__NR_geteuid, 107, "geteuid"),
            (general::__NR_getegid, 108, "getegid"),
            (general::__NR_getppid, 110, "getppid"),
            (general::__NR_getpgrp, 111, "getpgrp"),
            (general::__NR_arch_prctl, 158, "arch_prctl"),
            (general::__NR_gettid, 186, "gettid"),
            (general::__NR_set_tid_address, 218, "set_tid_address"),
            (general::__NR_clock_gettime, 228, "clock_gettime"),
            (general::__NR_exit_group, 231, "exit_group"),
            (general::__NR_prlimit64, 302, "prlimit64"),
            (general::__NR_getrandom, 318, "getrandom"),
        ];
        crate::kassert_eq!(M6_SYSCALLS.len(), 30);
        crate::kassert_eq!(expected_syscalls.len(), 30);
        for &(actual, spec_nr, name) in expected_syscalls {
            crate::kassert_eq!(actual, spec_nr);
            crate::kassert_eq!(syscall_name(spec_nr), Some(name));
        }
        crate::kassert_eq!(syscall_name(u32::MAX), None);

        // 2. Core Linux errno values.
        crate::kassert_eq!(errno::EPERM, 1);
        crate::kassert_eq!(errno::ENOENT, 2);
        crate::kassert_eq!(errno::ESRCH, 3);
        crate::kassert_eq!(errno::EINTR, 4);
        crate::kassert_eq!(errno::EBADF, 9);
        crate::kassert_eq!(errno::EAGAIN, 11);
        crate::kassert_eq!(errno::ENOMEM, 12);
        crate::kassert_eq!(errno::EFAULT, 14);
        crate::kassert_eq!(errno::EEXIST, 17);
        crate::kassert_eq!(errno::EINVAL, 22);
        crate::kassert_eq!(errno::ENOTTY, 25);
        crate::kassert_eq!(errno::ERANGE, 34);
        crate::kassert_eq!(errno::ENOSYS, 38);

        // 3. Terminal ioctl requests.
        crate::kassert_eq!(ioctl::TCGETS, 0x5401);
        crate::kassert_eq!(ioctl::TCSETS, 0x5402);
        crate::kassert_eq!(ioctl::TCSETSW, 0x5403);
        crate::kassert_eq!(ioctl::TCSETSF, 0x5404);
        crate::kassert_eq!(ioctl::TIOCGWINSZ, 0x5413);

        // 4. arch_prctl FS base codes.
        crate::kassert_eq!(ARCH_SET_FS, 0x1002);
        crate::kassert_eq!(ARCH_GET_FS, 0x1003);

        // 5. CLOCK_* IDs supported in M6.
        crate::kassert_eq!(general::CLOCK_REALTIME, 0);
        crate::kassert_eq!(general::CLOCK_MONOTONIC, 1);
        crate::kassert_eq!(general::CLOCK_MONOTONIC_RAW, 4);
        crate::kassert_eq!(general::CLOCK_REALTIME_COARSE, 5);
        crate::kassert_eq!(general::CLOCK_MONOTONIC_COARSE, 6);
        crate::kassert_eq!(general::CLOCK_BOOTTIME, 7);

        // 6. Memory protection / mapping and rlimit constants used in M6.
        crate::kassert_eq!(general::PROT_NONE, 0);
        crate::kassert_eq!(general::PROT_READ, 1);
        crate::kassert_eq!(general::PROT_WRITE, 2);
        crate::kassert_eq!(general::PROT_EXEC, 4);
        crate::kassert_eq!(general::MAP_SHARED, 1);
        crate::kassert_eq!(general::MAP_PRIVATE, 2);
        crate::kassert_eq!(general::MAP_FIXED, 0x10);
        crate::kassert_eq!(general::MAP_ANONYMOUS, 0x20);
        crate::kassert_eq!(general::MAP_FIXED_NOREPLACE, 0x10_0000);
        crate::kassert_eq!(general::RLIMIT_STACK, 3);
        crate::kassert_eq!(general::RLIMIT_NOFILE, 7);
        crate::kassert_eq!(general::RLIM_NLIMITS, 16);

        // 7. Exact Linux x86-64 ABI struct sizes and key field offsets.
        crate::kassert_eq!(size_of::<Utsname>(), 390);
        crate::kassert_eq!(size_of::<general::timespec>(), 16);
        crate::kassert_eq!(size_of::<general::__kernel_timespec>(), 16);
        crate::kassert_eq!(size_of::<general::iovec>(), 16);
        crate::kassert_eq!(size_of::<general::stat>(), 144);
        crate::kassert_eq!(size_of::<general::termios>(), 36);
        crate::kassert_eq!(size_of::<general::winsize>(), 8);
        crate::kassert_eq!(size_of::<general::rlimit64>(), 16);
        crate::kassert_eq!(size_of::<general::sigaction>(), 32);
        crate::kassert_eq!(size_of::<general::kernel_sigaction>(), 32);
        crate::kassert_eq!(core::mem::offset_of!(general::stat, st_mode), 24);
        crate::kassert_eq!(core::mem::offset_of!(general::stat, st_size), 48);
        crate::kassert_eq!(core::mem::offset_of!(general::kernel_sigaction, sa_mask), 24);
        crate::kassert_eq!(core::mem::offset_of!(Utsname, release), 130);

        // 8. Default utsname payload matches ARCHITECTURE.md §8.
        let uts = core::hint::black_box(default_utsname());
        crate::kassert_eq!(read_uts_field(&uts.sysname), "Linux");
        crate::kassert_eq!(read_uts_field(&uts.release), "6.6.0-lumen");
        crate::kassert_eq!(read_uts_field(&uts.machine), "x86_64");

        Ok(())
    }
);

crate::ktest!(
    fn m7_abi_constants_match_spec() {
        use core::mem::{offset_of, size_of};

        // 1. All 49 rows of ARCHITECTURE.md §8's M7 syscall table, by number.
        //    Two are M6's (`mmap` 9, `getcwd` 79: extended in M7, still in
        //    `M6_SYSCALLS`); the other 47 are `M7_SYSCALLS`, whose
        //    `linux-raw-sys` constants `syscall_name` checks against these.
        let m6_rows: &[(u32, &str)] = &[(9, "mmap"), (79, "getcwd")];
        let m7_rows: &[(u32, &str)] = &[
            (2, "open"),
            (4, "stat"),
            (6, "lstat"),
            (8, "lseek"),
            (17, "pread64"),
            (18, "pwrite64"),
            (21, "access"),
            (26, "msync"),
            (72, "fcntl"),
            (73, "flock"),
            (74, "fsync"),
            (75, "fdatasync"),
            (76, "truncate"),
            (77, "ftruncate"),
            (80, "chdir"),
            (81, "fchdir"),
            (82, "rename"),
            (83, "mkdir"),
            (84, "rmdir"),
            (86, "link"),
            (87, "unlink"),
            (88, "symlink"),
            (89, "readlink"),
            (90, "chmod"),
            (91, "fchmod"),
            (92, "chown"),
            (93, "fchown"),
            (94, "lchown"),
            (95, "umask"),
            (162, "sync"),
            (217, "getdents64"),
            (257, "openat"),
            (258, "mkdirat"),
            (260, "fchownat"),
            (262, "newfstatat"),
            (263, "unlinkat"),
            (264, "renameat"),
            (265, "linkat"),
            (266, "symlinkat"),
            (267, "readlinkat"),
            (268, "fchmodat"),
            (269, "faccessat"),
            (280, "utimensat"),
            (316, "renameat2"),
            (332, "statx"),
            (439, "faccessat2"),
            (452, "fchmodat2"),
        ];
        crate::kassert_eq!(m6_rows.len() + m7_rows.len(), 49);
        crate::kassert_eq!(M7_SYSCALLS.len(), 47);
        crate::kassert_eq!(m7_rows.len(), M7_SYSCALLS.len());
        let in_m6 = |nr: u32| M6_SYSCALLS.iter().any(|&(n, _)| n == nr);
        let in_m7 = |nr: u32| M7_SYSCALLS.iter().any(|&(n, _)| n == nr);
        for &(nr, name) in m6_rows {
            crate::kassert!(in_m6(nr) && !in_m7(nr));
            crate::kassert_eq!(syscall_name(nr), Some(name));
        }
        for &(nr, name) in m7_rows {
            crate::kassert!(in_m7(nr) && !in_m6(nr));
            crate::kassert_eq!(syscall_name(nr), Some(name));
        }
        // No number twice: with the lengths above, `M7_SYSCALLS` is exactly
        // the 47 rows.
        for (i, &(nr, _)) in M7_SYSCALLS.iter().enumerate() {
            crate::kassert!(M7_SYSCALLS[i + 1..].iter().all(|&(other, _)| other != nr));
        }

        // 2. The flags and commands the M7 syscalls take (Linux x86-64 values).
        //    `*at` flags: `AT_EACCESS` (faccessat2) and `AT_REMOVEDIR`
        //    (unlinkat) share a bit, so each syscall reads only its own.
        crate::kassert_eq!(general::AT_FDCWD, -100);
        crate::kassert_eq!(general::AT_SYMLINK_NOFOLLOW, 0x100);
        crate::kassert_eq!(general::AT_EACCESS, 0x200);
        crate::kassert_eq!(general::AT_REMOVEDIR, 0x200);
        crate::kassert_eq!(general::AT_SYMLINK_FOLLOW, 0x400);
        crate::kassert_eq!(general::AT_NO_AUTOMOUNT, 0x800);
        crate::kassert_eq!(general::AT_EMPTY_PATH, 0x1000);
        crate::kassert_eq!(general::AT_STATX_SYNC_TYPE, 0x6000);
        crate::kassert_eq!(general::AT_STATX_SYNC_AS_STAT, 0);
        crate::kassert_eq!(general::AT_STATX_FORCE_SYNC, 0x2000);
        crate::kassert_eq!(general::AT_STATX_DONT_SYNC, 0x4000);
        crate::kassert_eq!(general::STATX_BASIC_STATS, 0x7ff);
        crate::kassert_eq!(general::PATH_MAX, 4096);
        //    `open` flags: `O_TMPFILE` includes `O_DIRECTORY`, so a check for
        //    `O_DIRECTORY` alone also matches it.
        crate::kassert_eq!(general::O_ACCMODE, 3);
        crate::kassert_eq!(general::O_DIRECTORY, 0o200_000);
        crate::kassert_eq!(general::O_NOFOLLOW, 0o400_000);
        crate::kassert_eq!(general::O_CLOEXEC, 0o2_000_000);
        crate::kassert_eq!(general::O_PATH, 0o10_000_000);
        crate::kassert_eq!(general::O_TMPFILE, 0o20_000_000 | general::O_DIRECTORY);
        //    `access` modes.
        crate::kassert_eq!(general::F_OK, 0);
        crate::kassert_eq!(general::X_OK, 1);
        crate::kassert_eq!(general::W_OK, 2);
        crate::kassert_eq!(general::R_OK, 4);
        //    `fcntl` commands and `flock` operations.
        crate::kassert_eq!(general::F_DUPFD, 0);
        crate::kassert_eq!(general::F_GETFD, 1);
        crate::kassert_eq!(general::F_SETFD, 2);
        crate::kassert_eq!(general::F_GETFL, 3);
        crate::kassert_eq!(general::F_SETFL, 4);
        crate::kassert_eq!(general::F_GETLK, 5);
        crate::kassert_eq!(general::F_SETLK, 6);
        crate::kassert_eq!(general::F_SETLKW, 7);
        crate::kassert_eq!(general::F_DUPFD_CLOEXEC, 1030);
        crate::kassert_eq!(general::FD_CLOEXEC, 1);
        crate::kassert_eq!(general::LOCK_SH, 1);
        crate::kassert_eq!(general::LOCK_EX, 2);
        crate::kassert_eq!(general::LOCK_NB, 4);
        crate::kassert_eq!(general::LOCK_UN, 8);
        //    `utimensat`'s `tv_nsec` sentinels (and lumen-vfs's copies).
        crate::kassert_eq!(general::UTIME_NOW, 0x3fff_ffff);
        crate::kassert_eq!(general::UTIME_OMIT, 0x3fff_fffe);
        crate::kassert_eq!(i64::from(general::UTIME_NOW), lumen_vfs::SetTime::UTIME_NOW);
        crate::kassert_eq!(i64::from(general::UTIME_OMIT), lumen_vfs::SetTime::UTIME_OMIT);
        //    `renameat2` flags: `RENAME_WHITEOUT` is one M7 rejects (`-EINVAL`).
        crate::kassert_eq!(general::RENAME_NOREPLACE, 1);
        crate::kassert_eq!(general::RENAME_EXCHANGE, 2);
        crate::kassert_eq!(general::RENAME_WHITEOUT, 4);
        //    `lseek` whences and `msync` flags.
        crate::kassert_eq!(general::SEEK_SET, 0);
        crate::kassert_eq!(general::SEEK_CUR, 1);
        crate::kassert_eq!(general::SEEK_END, 2);
        crate::kassert_eq!(general::SEEK_DATA, 3);
        crate::kassert_eq!(general::SEEK_HOLE, 4);
        crate::kassert_eq!(general::MS_ASYNC, 1);
        crate::kassert_eq!(general::MS_INVALIDATE, 2);
        crate::kassert_eq!(general::MS_SYNC, 4);

        // 3. The user-visible M7 structs' Linux x86-64 sizes and key offsets.
        //    `stat`: x86-64 puts `st_nlink` (8 bytes) before `st_mode`.
        crate::kassert_eq!(size_of::<general::stat>(), 144);
        crate::kassert_eq!(offset_of!(general::stat, st_nlink), 16);
        crate::kassert_eq!(offset_of!(general::stat, st_mode), 24);
        crate::kassert_eq!(offset_of!(general::stat, st_rdev), 40);
        crate::kassert_eq!(offset_of!(general::stat, st_size), 48);
        crate::kassert_eq!(offset_of!(general::stat, st_blocks), 64);
        crate::kassert_eq!(offset_of!(general::stat, st_atime), 72);
        crate::kassert_eq!(offset_of!(general::stat, st_mtime), 88);
        crate::kassert_eq!(offset_of!(general::stat, st_ctime), 104);
        //    `statx`.
        crate::kassert_eq!(size_of::<general::statx>(), 256);
        crate::kassert_eq!(size_of::<general::statx_timestamp>(), 16);
        crate::kassert_eq!(offset_of!(general::statx, stx_mode), 28);
        crate::kassert_eq!(offset_of!(general::statx, stx_ino), 32);
        crate::kassert_eq!(offset_of!(general::statx, stx_size), 40);
        crate::kassert_eq!(offset_of!(general::statx, stx_blocks), 48);
        crate::kassert_eq!(offset_of!(general::statx, stx_atime), 64);
        crate::kassert_eq!(offset_of!(general::statx, stx_mtime), 112);
        crate::kassert_eq!(offset_of!(general::statx, stx_rdev_major), 128);
        //    `linux_dirent64`: a 19-byte header (`d_ino`, `d_off`, `d_reclen`,
        //    `d_type`), then the NUL-terminated name. `size_of` says 24 (the
        //    header padded to 8), which is not a record's length: a record is
        //    the header plus the name and its NUL, rounded up to 8.
        crate::kassert_eq!(offset_of!(general::linux_dirent64, d_off), 8);
        crate::kassert_eq!(offset_of!(general::linux_dirent64, d_reclen), 16);
        crate::kassert_eq!(offset_of!(general::linux_dirent64, d_type), 18);
        crate::kassert_eq!(offset_of!(general::linux_dirent64, d_name), 19);
        crate::kassert_eq!(size_of::<general::linux_dirent64>(), 24);
        //    `struct flock` (`fcntl`'s `F_GETLK`/`F_SETLK`/`F_SETLKW`).
        crate::kassert_eq!(size_of::<general::flock>(), 32);
        crate::kassert_eq!(offset_of!(general::flock, l_whence), 2);
        crate::kassert_eq!(offset_of!(general::flock, l_start), 8);
        crate::kassert_eq!(offset_of!(general::flock, l_len), 16);
        crate::kassert_eq!(offset_of!(general::flock, l_pid), 24);

        Ok(())
    }
);
