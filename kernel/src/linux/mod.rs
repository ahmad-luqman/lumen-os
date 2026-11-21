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

use core::fmt;

use lumen_elf::loader::LoadedElf;
use lumen_vfs::VfsError;
use lumen_vm::PIE_BASE;

use crate::arch;
use crate::linux::process::ConsoleIo;
use crate::mm::frame;
use crate::mm::user_vm::{AddressSpace, UserVmError};
use crate::shell::Ctx;
use crate::term::Place;
use crate::user::{UserExit, run_elf_process};
use crate::vfs;

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

/// Why `linux` could not load a binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecError {
    /// The interpreter its `PT_INTERP` names could not be read (`ENOENT`
    /// when it is absent).
    Interp(VfsError),
    /// The binary, or its interpreter (`ELIBBAD`), could not be loaded.
    Load(UserVmError),
}

impl ExecError {
    /// The Linux `errno` an `execve(2)` failing this way returns.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    const fn to_errno(self) -> u32 {
        match self {
            ExecError::Interp(e) => e.to_errno(),
            ExecError::Load(e) => e.to_errno(),
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Interp(e) => write!(f, "cannot read its interpreter: {e}"),
            ExecError::Load(e) => write!(f, "{e}"),
        }
    }
}

/// The interpreter file `image`'s `PT_INTERP` names, read through the VFS
/// (`None` for a static binary), so that `linux` reads every file before
/// counting frames.
fn read_interp(image: &[u8]) -> Result<Option<Vec<u8>>, ExecError> {
    let main = LoadedElf::parse_with_pie_base(image, PIE_BASE).map_err(|e| ExecError::Load(UserVmError::Elf(e)))?;
    let Some(path) = main.interp_path(image) else {
        return Ok(None);
    };
    // The VFS resolves UTF-8 paths only; no other path can name a file in it.
    let path = core::str::from_utf8(path).map_err(|_| ExecError::Interp(VfsError::NoEnt))?;
    vfs::read_executable(path).map(Some).map_err(ExecError::Interp)
}

/// Shell command: `linux` (list available Limine Linux binaries),
/// `linux <name> [args...]` (load and run a Linux x86-64 ELF64 boot module),
/// or `linux /path [args...]` (load and run a static or dynamically linked
/// ELF64 binary from the VFS, its `PT_INTERP` interpreter too).
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
    let file: Vec<u8>;
    let (image, thread_name, exe_path): (&[u8], &'static str, Option<String>) = if name.starts_with('/') {
        file = match vfs::read_executable(name) {
            Ok(bytes) => bytes,
            Err(e) => return ctx.fail(format_args!("linux: cannot read '{name}': {e}")),
        };
        (&file, "linux", Some(name.clone()))
    } else {
        let Some(module) = modules::find(name) else {
            return ctx.fail(format_args!("linux: unknown binary '{name}' (run 'linux' to list)"));
        };
        (module.bytes, module.name, None)
    };
    let interp = match read_interp(image) {
        Ok(interp) => interp,
        Err(e) => return ctx.fail(format_args!("linux: failed to load '{name}': {e}")),
    };

    let argv: Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();
    let envp: [&[u8]; 3] = [b"PATH=/bin:/usr/bin", b"HOME=/", b"TERM=vt100"];
    let execfn = name.as_bytes();
    let random = at_random_bytes();

    // Pre-warm kernel heap so process/VMA/stack metadata reuses mapped heap
    // pages and physical frame accounting reflects only user-space frames.
    // Every file was read above, so the page cache's frames are not counted.
    let _warm: Vec<u8> = vec![0u8; 32 * 1024];
    drop(_warm);

    let frames_before = frame::free_frames();
    let loaded = AddressSpace::from_elf_with_interp(image, interp.as_deref(), &argv, &envp, execfn, random);
    let (space, prog) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => return ctx.fail(format_args!("linux: failed to load '{name}': {e}")),
    };
    let space = Arc::new(space);
    let exit = match run_elf_process(thread_name, space, prog.entry, prog.rsp, io, exe_path) {
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
        crate::kassert_eq!(general::O_RDONLY, 0);
        crate::kassert_eq!(general::O_WRONLY, 1);
        crate::kassert_eq!(general::O_RDWR, 2);
        crate::kassert_eq!(general::O_ACCMODE, 3);
        crate::kassert_eq!(general::O_NOCTTY, 0o400);
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
        crate::kassert_eq!(general::F_RDLCK, 0);
        crate::kassert_eq!(general::F_WRLCK, 1);
        crate::kassert_eq!(general::F_UNLCK, 2);
        crate::kassert_eq!(general::LOCK_SH, 1);
        crate::kassert_eq!(general::LOCK_EX, 2);
        crate::kassert_eq!(general::LOCK_NB, 4);
        crate::kassert_eq!(general::LOCK_UN, 8);
        //    `utimensat`'s `tv_nsec` sentinels. (lumen-vfs's copies of these
        //    and the other flags are checked against linux-raw-sys's at
        //    compile time, in `syscall/at.rs`.)
        crate::kassert_eq!(general::UTIME_NOW, 0x3fff_ffff);
        crate::kassert_eq!(general::UTIME_OMIT, 0x3fff_fffe);
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

/// One program header for the ktests' crafted ELF64 images.
#[cfg(feature = "ktest")]
#[derive(Clone, Copy)]
struct TestPhdr {
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// A little-endian x86-64 ELF64 image of `len` bytes: the header, `phdrs`
/// right after it, then each `(offset, bytes)` patch.
#[cfg(feature = "ktest")]
fn build_test_elf(e_type: u16, entry: u64, phdrs: &[TestPhdr], len: usize, patches: &[(usize, &[u8])]) -> Vec<u8> {
    use lumen_elf::loader::{EM_X86_64, EV_CURRENT, PHDR_MIN_SIZE};
    const EHDR_SIZE: usize = 64;
    let mut img = vec![0u8; len];
    img[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    img[4] = 2; // ELFCLASS64
    img[5] = 1; // ELFDATA2LSB
    img[6] = EV_CURRENT;
    img[0x10..0x12].copy_from_slice(&e_type.to_le_bytes());
    img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
    img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
    img[0x18..0x20].copy_from_slice(&entry.to_le_bytes());
    img[0x20..0x28].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    img[0x34..0x36].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
    img[0x38..0x3a].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
    for (i, ph) in phdrs.iter().enumerate() {
        let o = EHDR_SIZE + i * usize::from(PHDR_MIN_SIZE);
        img[o..o + 4].copy_from_slice(&ph.p_type.to_le_bytes());
        img[o + 4..o + 8].copy_from_slice(&ph.flags.to_le_bytes());
        img[o + 8..o + 16].copy_from_slice(&ph.offset.to_le_bytes());
        img[o + 16..o + 24].copy_from_slice(&ph.vaddr.to_le_bytes());
        img[o + 24..o + 32].copy_from_slice(&ph.vaddr.to_le_bytes());
        img[o + 32..o + 40].copy_from_slice(&ph.filesz.to_le_bytes());
        img[o + 40..o + 48].copy_from_slice(&ph.memsz.to_le_bytes());
        img[o + 48..o + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    }
    for &(off, bytes) in patches {
        img[off..off + bytes.len()].copy_from_slice(bytes);
    }
    img
}

/// A dynamically linked main image naming `interp`: `PT_PHDR`, `PT_INTERP`,
/// an `R--` page at `vbase` holding the headers and an `R-X` page above it
/// starting `MAIN-TXT`, with its entry at `vbase + 0x1010` (an `ET_DYN`
/// gets `PIE_BASE` added).
#[cfg(feature = "ktest")]
fn test_dynamic_main(e_type: u16, vbase: u64, interp: &[u8]) -> Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
    let mut path = Vec::from(interp);
    path.push(0);
    let phdrs = [
        TestPhdr { p_type: PT_PHDR, flags: PF_R, offset: 0x40, vaddr: vbase + 0x40, filesz: 4 * 56, memsz: 4 * 56 },
        TestPhdr {
            p_type: PT_INTERP,
            flags: PF_R,
            offset: 0x200,
            vaddr: vbase + 0x200,
            filesz: path.len() as u64,
            memsz: path.len() as u64,
        },
        TestPhdr { p_type: PT_LOAD, flags: PF_R, offset: 0, vaddr: vbase, filesz: 0x1000, memsz: 0x1000 },
        TestPhdr {
            p_type: PT_LOAD,
            flags: PF_R | PF_X,
            offset: 0x1000,
            vaddr: vbase + 0x1000,
            filesz: 0x100,
            memsz: 0x100,
        },
    ];
    build_test_elf(e_type, vbase + 0x1010, &phdrs, 0x2000, &[(0x200, &path), (0x1000, b"MAIN-TXT")])
}

/// An interpreter: an `R--` page at `vbase` and an `R-X` segment of
/// `text_memsz` bytes above it starting `INTERP!!`, its entry at
/// `vbase + 0x1020`, plus `extra` program headers.
#[cfg(feature = "ktest")]
fn test_interp(e_type: u16, vbase: u64, text_memsz: u64, extra: &[TestPhdr]) -> Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_LOAD};
    let mut phdrs = vec![
        TestPhdr { p_type: PT_LOAD, flags: PF_R, offset: 0, vaddr: vbase, filesz: 0x400, memsz: 0x1000 },
        TestPhdr {
            p_type: PT_LOAD,
            flags: PF_R | PF_X,
            offset: 0x1000,
            vaddr: vbase + 0x1000,
            filesz: 0x100,
            memsz: text_memsz,
        },
    ];
    phdrs.extend_from_slice(extra);
    build_test_elf(e_type, vbase + 0x1020, &phdrs, 0x2000, &[(0x1000, b"INTERP!!")])
}

/// The auxiliary vector on a freshly built initial stack at `rsp` (past
/// `argc`, `argv` and `envp`), read through the current address space.
#[cfg(feature = "ktest")]
fn read_test_auxv(rsp: u64) -> Result<Vec<(u64, u64)>, &'static str> {
    use crate::user::copy_from_user;
    let word = |va: u64| -> Result<u64, &'static str> {
        let mut b = [0u8; 8];
        copy_from_user(&mut b, va).map_err(|_| "initial stack unreadable")?;
        Ok(u64::from_le_bytes(b))
    };
    let argc = word(rsp)?;
    let mut va = rsp + 8 * (argc + 2); // argc, argv[..], NULL
    while word(va)? != 0 {
        va += 8; // envp[..]
    }
    va += 8;
    let mut auxv = Vec::new();
    loop {
        let (key, val) = (word(va)?, word(va + 8)?);
        if key == lumen_elf::stack::AT_NULL {
            return Ok(auxv);
        }
        auxv.push((key, val));
        va += 16;
        if auxv.len() > 64 {
            return Err("auxv not terminated");
        }
    }
}

crate::ktest!(
    fn dynamic_elf_maps_interp_and_sets_auxv() {
        use crate::user::copy_from_user;
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::ET_DYN;
        use lumen_elf::stack::{AT_BASE, AT_ENTRY, AT_PHDR, AT_PHNUM};
        use lumen_vm::INTERP_BASE;

        const NAME: &str = "ld-lumen-ktest.so";
        // The interpreter is linked at 0x10000, so its load bias (AT_BASE)
        // is INTERP_BASE - 0x10000 while its pages start at INTERP_BASE.
        let interp_img = test_interp(ET_DYN, 0x10000, 0x100, &[]);
        let main = test_dynamic_main(ET_DYN, 0, b"/tmp/ld-lumen-ktest.so");

        // Write the interpreter to tmpfs and read it back the way `linux`
        // does, then without its execute bits (EACCES, as execve(2)), and
        // remove it before checking anything or counting frames (tmpfs
        // pages hold frames).
        let tmp = vfs::resolve_path_from_root("/tmp").map_err(|_| "no /tmp")?;
        let file = tmp.create(NAME, 0o755).map_err(|_| "create interpreter")?;
        let written = file.write_at(0, &interp_img);
        let read = read_interp(&main);
        let mode = file.set_mode(0o644);
        let not_executable = read_interp(&main);
        drop(file);
        tmp.unlink(NAME).map_err(|_| "unlink interpreter")?;
        kassert_eq!(written, Ok(interp_img.len()));
        kassert_eq!(mode, Ok(()));
        kassert_eq!(not_executable.map_err(ExecError::to_errno), Err(errno::EACCES));
        let interp = read.map_err(|_| "read_interp failed")?.ok_or("no interpreter read")?;
        kassert_eq!(interp, interp_img);

        let _warm: Vec<u8> = vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();
        {
            let (space, prog) =
                AddressSpace::from_elf_with_interp(&main, Some(&interp), &[b"/t"], &[b"K=V"], b"/t", [7; 16])
                    .map_err(|_| "dynamic load failed")?;
            kassert_eq!(prog.entry, INTERP_BASE + 0x1020);
            kassert_eq!(prog.initial_brk, PIE_BASE + 0x2000);
            let space = Arc::new(space);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let auxv = read_test_auxv(prog.rsp)?;
            let aux = |key: u64| auxv.iter().find(|&&(k, _)| k == key).map(|&(_, v)| v);
            kassert_eq!(aux(AT_BASE), Some(INTERP_BASE - 0x10000));
            kassert_eq!(aux(AT_ENTRY), Some(PIE_BASE + 0x1010));
            kassert_eq!(aux(AT_PHDR), Some(PIE_BASE + 0x40));
            kassert_eq!(aux(AT_PHNUM), Some(4));
            // Both images are mapped where their headers say.
            let mut text = [0u8; 8];
            kassert_eq!(copy_from_user(&mut text, PIE_BASE + 0x1000), Ok(()));
            kassert_eq!(&text, b"MAIN-TXT");
            kassert_eq!(copy_from_user(&mut text, INTERP_BASE + 0x1000), Ok(()));
            kassert_eq!(&text, b"INTERP!!");
            drop(_active);
            space.teardown();
        }
        kassert_eq!(frame::free_frames(), free_before);

        // A static image through the same call: its own entry, no AT_BASE.
        {
            let elf = crate::mm::user_vm::build_relro_test_elf();
            let (space, prog) = AddressSpace::from_elf_with_interp(&elf, None, &[b"/s"], &[], b"/s", [7; 16])
                .map_err(|_| "static load failed")?;
            kassert_eq!(prog.entry, 0x40_1000);
            kassert_eq!(prog.initial_brk, 0x40_4000);
            let space = Arc::new(space);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let auxv = read_test_auxv(prog.rsp)?;
            kassert!(auxv.iter().all(|&(k, _)| k != AT_BASE));
            kassert_eq!(auxv.iter().find(|&&(k, _)| k == AT_ENTRY).map(|&(_, v)| v), Some(0x40_1000));
            drop(_active);
            space.teardown();

            // An interpreter must be given exactly when the image names one.
            let mismatch = AddressSpace::from_elf_with_interp(&elf, Some(&interp), &[], &[], b"/s", [7; 16]);
            kassert!(matches!(mismatch, Err(UserVmError::InvalidArg)));
            let mismatch = AddressSpace::from_elf_with_interp(&main, None, &[], &[], b"/t", [7; 16]);
            kassert!(matches!(mismatch, Err(UserVmError::InvalidArg)));
        }
        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn dynamic_elf_layout_rejected() {
        use crate::mm::user_vm::USER_STACK_BOTTOM;
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::{ET_DYN, ET_EXEC, ElfLoadError, PF_R, PT_INTERP};
        use lumen_vm::{INTERP_BASE, USER_MAX, VmError};

        let main = test_dynamic_main(ET_DYN, 0, b"/lib/ld-musl-x86_64.so.1");
        let interp = test_interp(ET_DYN, 0, 0x100, &[]);
        let load = |main: &[u8], interp: &[u8]| {
            AddressSpace::from_elf_with_interp(main, Some(interp), &[b"/t"], &[], b"/t", [0; 16]).map(|_| ())
        };

        let _warm: Vec<u8> = vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();

        // An ET_EXEC main image placed over the interpreter's pages.
        let over = test_dynamic_main(ET_EXEC, INTERP_BASE, b"/lib/ld-musl-x86_64.so.1");
        kassert_eq!(load(&over, &interp), Err(UserVmError::Vm(VmError::Exists)));
        // An interpreter reaching the stack's guard page (its last page is
        // the guard page; refused before anything is mapped).
        let long = test_interp(ET_DYN, 0, USER_STACK_BOTTOM - INTERP_BASE - 0x1000, &[]);
        kassert_eq!(load(&main, &long), Err(UserVmError::Vm(VmError::Exists)));
        // An interpreter past the top of user space.
        let past = load(&main, &test_interp(ET_DYN, 0, USER_MAX - INTERP_BASE, &[]));
        kassert!(matches!(past, Err(UserVmError::BadInterp(ElfLoadError::AddressOutOfRange { .. }))));
        kassert_eq!(past.map_err(UserVmError::to_errno), Err(errno::ELIBBAD));
        // An interpreter with its own interpreter, or not an ET_DYN.
        let nested_ph = TestPhdr { p_type: PT_INTERP, flags: PF_R, offset: 0x400, vaddr: 0x400, filesz: 2, memsz: 2 };
        let nested = test_interp(ET_DYN, 0, 0x100, &[nested_ph]);
        kassert_eq!(load(&main, &nested), Err(UserVmError::BadInterp(ElfLoadError::NestedInterp)));
        let exec = test_interp(ET_EXEC, 0x40_0000, 0x100, &[]);
        kassert_eq!(load(&main, &exec), Err(UserVmError::BadInterp(ElfLoadError::InterpreterNotDynamic(ET_EXEC))));

        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn missing_interpreter_is_enoent() {
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::ET_DYN;

        let missing = test_dynamic_main(ET_DYN, 0, b"/no/such/ld-lumen.so");
        let err = read_interp(&missing);
        kassert_eq!(err, Err(ExecError::Interp(VfsError::NoEnt)));
        kassert_eq!(err.map_err(ExecError::to_errno), Err(errno::ENOENT));
        // A directory is not an interpreter.
        let dir = test_dynamic_main(ET_DYN, 0, b"/");
        kassert_eq!(read_interp(&dir).map_err(ExecError::to_errno), Err(errno::EACCES));
        // A static image names none; a malformed one is refused as ENOEXEC.
        let elf = crate::mm::user_vm::build_relro_test_elf();
        kassert_eq!(read_interp(&elf), Ok(None));
        kassert!(matches!(read_interp(b"not an elf"), Err(ExecError::Load(UserVmError::Elf(_)))));
        kassert_eq!(read_interp(b"not an elf").map_err(ExecError::to_errno), Err(errno::ENOEXEC));
        Ok(())
    }
);
