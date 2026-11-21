//! Ring-3 user-mode support: fault-safe user memory copies (`.lumen_fixup`),
//! user exception / syscall exit handling, and the M6 ring-3 test runner.
//!
//! See `docs/ARCHITECTURE.md` §5 and §6 ("Ring 3 entry, `syscall`/`sysretq`,
//! user CPU state and safe copies").

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use log::warn;
use lumen_elf::loader::{
    EM_X86_64, ET_EXEC, EV_CURRENT, ElfLoadError, PF_R, PF_W, PF_X, PHDR_MIN_SIZE, PT_INTERP, PT_LOAD,
};
use lumen_mm::FRAME_SIZE;
use lumen_vm::Prot;

use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::arch::{self, TrapFrame};
use crate::linux::process::{ConsoleIo, record_exit, register_current, take_current};
use crate::linux::{ARCH_GET_FS, ARCH_SET_FS, errno, general, ioctl, syscall as linux_syscall};
#[cfg(feature = "ktest")]
use crate::mm::paging;
use crate::mm::user_vm::{AddressSpace, USER_STACK_BOTTOM, UserVmError};
use crate::mm::{frame, heap};
use crate::sched;
use crate::shell::Ctx;
use crate::sync::SpinLock;

const ELF_EHDR_SIZE: usize = 64;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Lowest valid user virtual address (first 64 KiB is the unmapped null guard).
pub const USER_MIN: u64 = 0x0000_0000_0001_0000;

/// Exclusive upper bound of the user virtual address space (the top canonical
/// page `0x0000_7fff_ffff_f000` stays unmapped as a boundary guard).
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;

const _: () = assert!(USER_MAX == arch::fpu::USER_MAX);

/// One `(fault_ip, fixup_ip)` pair placed in the read-only `.lumen_fixup`
/// linker section (`kernel/linker.ld`).
#[repr(C)]
struct FixupEntry {
    fault_ip: u64,
    fixup_ip: u64,
}

unsafe extern "C" {
    static __lumen_fixup_start: [FixupEntry; 0];
    static __lumen_fixup_end: [FixupEntry; 0];
}

/// Look up `rip` in the read-only `.lumen_fixup` table. Returns the fixup
/// recovery address if `rip` is a registered user-copy faulting instruction.
pub fn lookup_fixup(rip: u64) -> Option<u64> {
    let start = (&raw const __lumen_fixup_start).cast::<FixupEntry>();
    let end = (&raw const __lumen_fixup_end).cast::<FixupEntry>();
    let bytes = (end as usize).saturating_sub(start as usize);
    debug_assert!(start as usize <= end as usize && bytes.is_multiple_of(core::mem::size_of::<FixupEntry>()));
    let count = bytes / core::mem::size_of::<FixupEntry>();
    // SAFETY: `.lumen_fixup` is a read-only linker section of initialized
    // `FixupEntry` pairs between `__lumen_fixup_start` and `__lumen_fixup_end`.
    let entries = unsafe { core::slice::from_raw_parts(start, count) };
    entries.iter().find_map(|e| (e.fault_ip == rip).then_some(e.fixup_ip))
}

// Fault-safe user memory copy routines.
//
// Each faulting load/store instruction is registered in `.lumen_fixup`
// (`"a", @progbits`, placed inside `:rodata` in `kernel/linker.ld`). When
// `LUMEN_SMAP_ENABLED != 0`, accesses are bracketed with `stac`/`clac`; on
// fault, the `#PF` handler redirects `rip` to the fixup label and clears
// `RFLAGS.AC` in `frame.rflags`. Zero SIMD, zero red-zone use.
global_asm!(
    r#"
    .pushsection .text.lumen_uaccess, "ax", @progbits
    .globl lumen_user_copy_raw
    .type lumen_user_copy_raw, @function
    .balign 16
lumen_user_copy_raw:
    # System V AMD64: rdi = dst, rsi = src, rdx = len.
    # Returns 0 on success, 1 on user page fault.
    movq %rdx, %rcx
    cld
    movb LUMEN_SMAP_ENABLED(%rip), %r8b
    testb %r8b, %r8b
    jz 1f
    stac
1:
2:
    rep movsb
    testb %r8b, %r8b
    jz 3f
    clac
3:
    xorl %eax, %eax
    ret
4:
    testb %r8b, %r8b
    jz 5f
    clac
5:
    movl $1, %eax
    ret
    .size lumen_user_copy_raw, . - lumen_user_copy_raw

    .pushsection .lumen_fixup, "a", @progbits
    .balign 8
    .quad 2b
    .quad 4b
    .popsection

    .globl lumen_user_copy_cstr_raw
    .type lumen_user_copy_cstr_raw, @function
    .balign 16
lumen_user_copy_cstr_raw:
    # System V AMD64: rdi = kernel dst, rsi = user src, rdx = max bytes (> 0).
    # Returns:
    #   >= 0: length up to NUL (NUL was found at src[rax], rax < rdx)
    #   -1:   page fault on user load
    #   -2:   no NUL found within rdx bytes (all rdx bytes copied into dst)
    xorl %ecx, %ecx
    movb LUMEN_SMAP_ENABLED(%rip), %r8b
    testb %r8b, %r8b
    jz 1f
    stac
1:
    cmpq %rdx, %rcx
    jae 3f
2:
    movzbl (%rsi, %rcx, 1), %eax
    testl %eax, %eax
    jz 4f
    movb %al, (%rdi, %rcx, 1)
    incq %rcx
    jmp 1b
3:
    testb %r8b, %r8b
    jz 31f
    clac
31:
    movq $-2, %rax
    ret
4:
    testb %r8b, %r8b
    jz 41f
    clac
41:
    movq %rcx, %rax
    ret
5:
    testb %r8b, %r8b
    jz 51f
    clac
51:
    movq $-1, %rax
    ret
    .size lumen_user_copy_cstr_raw, . - lumen_user_copy_cstr_raw

    .pushsection .lumen_fixup, "a", @progbits
    .balign 8
    .quad 2b
    .quad 5b
    .popsection
    .popsection
"#,
    options(att_syntax)
);

unsafe extern "C" {
    fn lumen_user_copy_raw(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn lumen_user_copy_cstr_raw(dst: *mut u8, src: *const u8, max_len: usize) -> i64;
}

/// Failure modes for [`copy_from_user`], [`copy_to_user`], and [`copy_cstr_from_user`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyError {
    /// Invalid or unmapped user address, overflow wraparound, or insufficient
    /// permissions (Linux `-EFAULT`).
    Fault,
    /// C string exceeded `max_len` bytes without a NUL terminator (Linux
    /// `-ENAMETOOLONG`).
    TooLong,
}

impl CopyError {
    /// Corresponding Linux `errno` value.
    #[allow(dead_code)] // used by M6 syscall implementations
    pub const fn to_errno(self) -> u32 {
        match self {
            CopyError::Fault => errno::EFAULT,
            CopyError::TooLong => errno::ENAMETOOLONG,
        }
    }
}

/// Check that `[start, start + len)` is a valid user-space range (empty ranges
/// are always valid; non-empty ranges must satisfy `start >= USER_MIN` and
/// `start + len <= USER_MAX` without `u64` overflow).
#[inline]
pub const fn user_range_valid(start: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if start < USER_MIN {
        return false;
    }
    match start.checked_add(len as u64) {
        Some(end) => end <= USER_MAX,
        None => false,
    }
}

/// Fault-safely copy `dst.len()` bytes from user virtual address `user_src`
/// into kernel slice `dst`.
///
/// Must be called with interrupts enabled (unless `irq::disabled_for_good()`)
/// and no `SpinLock` held
/// (`debug_assert!((irq::enabled() || irq::disabled_for_good()) && irq::depth() == 0)`).
pub fn copy_from_user(dst: &mut [u8], user_src: u64) -> Result<(), CopyError> {
    debug_assert!(
        (arch::irq::enabled() || arch::irq::disabled_for_good()) && arch::irq::depth() == 0,
        "copy_from_user requires interrupts enabled and no SpinLock held"
    );
    if dst.is_empty() {
        return Ok(());
    }
    if !user_range_valid(user_src, dst.len()) {
        return Err(CopyError::Fault);
    }
    // SAFETY: `dst` is writable kernel memory for `dst.len()` bytes;
    // `[user_src, user_src + dst.len())` is validated within `USER_MIN..USER_MAX`,
    // and any `#PF` on the user load is caught by `.lumen_fixup`.
    let rc = unsafe { lumen_user_copy_raw(dst.as_mut_ptr(), user_src as *const u8, dst.len()) };
    if rc == 0 { Ok(()) } else { Err(CopyError::Fault) }
}

/// Fault-safely copy `src` from kernel memory to user virtual address `user_dst`.
///
/// Must be called with interrupts enabled (unless `irq::disabled_for_good()`)
/// and no `SpinLock` held
/// (`debug_assert!((irq::enabled() || irq::disabled_for_good()) && irq::depth() == 0)`).
#[allow(dead_code)] // HAL/kernel surface used by ktests and M6 syscalls
pub fn copy_to_user(user_dst: u64, src: &[u8]) -> Result<(), CopyError> {
    debug_assert!(
        (arch::irq::enabled() || arch::irq::disabled_for_good()) && arch::irq::depth() == 0,
        "copy_to_user requires interrupts enabled and no SpinLock held"
    );
    if src.is_empty() {
        return Ok(());
    }
    if !user_range_valid(user_dst, src.len()) {
        return Err(CopyError::Fault);
    }
    // SAFETY: `src` is readable kernel memory for `src.len()` bytes;
    // `[user_dst, user_dst + src.len())` is validated within `USER_MIN..USER_MAX`,
    // and any `#PF` on the user store (unmapped or read-only with CR0.WP=1) is
    // caught by `.lumen_fixup`.
    let rc = unsafe { lumen_user_copy_raw(user_dst as *mut u8, src.as_ptr(), src.len()) };
    if rc == 0 { Ok(()) } else { Err(CopyError::Fault) }
}

/// Fault-safely copy a NUL-terminated byte string of up to `max_len` bytes
/// (excluding the terminating NUL) from `user_src`.
///
/// Reads in bounded chunks and stops at the first NUL byte so valid strings
/// adjacent to an unmapped page or `USER_MAX` succeed. Must be called with
/// interrupts enabled and no `SpinLock` held.
#[allow(dead_code)] // HAL/kernel surface used by ktests and M6 syscalls
pub fn copy_cstr_from_user(user_src: u64, max_len: usize) -> Result<Vec<u8>, CopyError> {
    debug_assert!(
        (arch::irq::enabled() || arch::irq::disabled_for_good()) && arch::irq::depth() == 0,
        "copy_cstr_from_user requires interrupts enabled and no SpinLock held"
    );
    if !(USER_MIN..USER_MAX).contains(&user_src) {
        return Err(CopyError::Fault);
    }
    let scan_limit = max_len.saturating_add(1);
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    let mut offset = 0usize;

    loop {
        let addr = user_src.checked_add(offset as u64).ok_or(CopyError::Fault)?;
        if addr >= USER_MAX {
            return Err(CopyError::Fault);
        }
        let remaining = scan_limit.saturating_sub(offset);
        if remaining == 0 {
            return Err(CopyError::TooLong);
        }
        let avail_user = (USER_MAX - addr).min(usize::MAX as u64) as usize;
        let chunk = remaining.min(avail_user).min(buf.len());
        // SAFETY: `buf` is valid writable kernel memory for `chunk` bytes;
        // `[addr, addr + chunk)` lies inside `USER_MIN..USER_MAX`, and any `#PF`
        // on the user load is caught by `.lumen_fixup`.
        let rc = unsafe { lumen_user_copy_cstr_raw(buf.as_mut_ptr(), addr as *const u8, chunk) };
        if rc == -1 {
            return Err(CopyError::Fault);
        }
        if rc >= 0 {
            let n = rc as usize;
            out.extend_from_slice(&buf[..n]);
            return Ok(out);
        }
        // rc == -2: no NUL in `chunk` bytes.
        if offset.saturating_add(chunk) > max_len {
            return Err(CopyError::TooLong);
        }
        out.extend_from_slice(&buf[..chunk]);
        offset += chunk;
        if user_src.checked_add(offset as u64).is_none_or(|a| a >= USER_MAX) {
            return Err(CopyError::Fault);
        }
    }
}

/// How a ring-3 execution terminated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserExit {
    /// Clean exit via `exit(2)` / `exit_group(2)`.
    Exited(i32),
    /// Fatal ring-3 CPU exception (or deferred `#DB` at `LSTAR`).
    Fault { vector: u64, error_code: u64, rip: u64, cr2: u64 },
    /// Kernel refused to execute `sysretq`/`iretq` to a non-canonical or
    /// kernel-half return `rip`.
    NonCanonicalRip(u64),
}

/// Synchronization state for the concurrent same-CPU `sse_tls` preemption test.
struct SseTlsSync {
    ready_mask: AtomicU8,
    aborted: AtomicBool,
    base_switches: AtomicU64,
    target_cpu: AtomicUsize,
}

static SSE_TLS_SYNC: SseTlsSync = SseTlsSync {
    ready_mask: AtomicU8::new(0),
    aborted: AtomicBool::new(false),
    base_switches: AtomicU64::new(0),
    target_cpu: AtomicUsize::new(0),
};

// Test-only syscall numbers used by `usertest` to exercise return-path
// validation (canonical check on `sysretq`/`iretq`, rewritten-frame `iretq`,
// and deterministic scheduler-switch counting for `sse_tls`). Never exposed to
// normal `linux` processes (`allow_test_syscalls == false`).
const SYS_TEST_NONCANON_SYSRET: u64 = 0x1000_0001;
const SYS_TEST_NONCANON_IRET: u64 = 0x1000_0002;
const SYS_TEST_IRET_RETURN: u64 = 0x1000_0003;
const SYS_TEST_PREEMPT_COUNT: u64 = 0x1000_0004;

/// Abort any waiting `sse_tls` peer if a user process exits non-zero.
pub(crate) fn note_exit_status(status: i32) {
    if status != 0 {
        SSE_TLS_SYNC.aborted.store(true, Ordering::Release);
    }
}

/// Handle a ring-3 exception (`(frame.cs & 3) != 0`) from `lumen_exception_dispatch`.
///
/// Logs one `WARN` naming pid, vector, `rip`, and `cr2`, and unwinds cleanly
/// back to the kernel ring-3 runner—never calling `crash::exception`.
pub fn handle_user_exception(frame: &TrapFrame, cr2: u64) -> ! {
    SSE_TLS_SYNC.aborted.store(true, Ordering::Release);
    let exit = UserExit::Fault { vector: frame.vector, error_code: frame.error_code, rip: frame.rip, cr2 };
    let pid = record_exit(exit);
    warn!(
        target: "user",
        "pid {pid}: fatal user exception {} (vector {}, error {:#x}, rip {:#x}, cr2 {:#x})",
        arch::idt::exception_name(frame.vector),
        frame.vector,
        frame.error_code,
        frame.rip,
        cr2
    );
    // SAFETY: called on the CPU running the active ring-3 context; lock guard
    // from `record_exit` is already dropped (`irq::depth() == 0`).
    unsafe { arch::syscall::unwind_to_kernel() }
}

/// Handle a deferred `#DB` trap caught at `LSTAR` (CVE-2018-8897 handoff),
/// running on the thread's normal kernel stack inside `lumen_syscall_dispatch`.
pub fn handle_deferred_debug_trap(frame: &SyscallFrame) -> ! {
    SSE_TLS_SYNC.aborted.store(true, Ordering::Release);
    let exit = UserExit::Fault { vector: 1, error_code: 0, rip: frame.rip, cr2: 0 };
    let pid = record_exit(exit);
    warn!(
        target: "user",
        "pid {pid}: fatal deferred #DB at LSTAR (vector 1, rip {:#x}, cr2 0x0)",
        frame.rip
    );
    // SAFETY: called on the CPU running the active ring-3 context, no lock held.
    unsafe { arch::syscall::unwind_to_kernel() }
}

/// Terminate the active user context when the return `rip` is non-canonical or
/// `>= USER_MAX`, before `swapgs` or `sysretq`/`iretq` ever runs.
pub fn handle_non_canonical_return(rip: u64) -> ! {
    SSE_TLS_SYNC.aborted.store(true, Ordering::Release);
    let pid = record_exit(UserExit::NonCanonicalRip(rip));
    warn!(target: "user", "pid {pid}: non-canonical user return rip {rip:#x}; terminating process");
    // SAFETY: called on the CPU running the active ring-3 context, no lock held.
    unsafe { arch::syscall::unwind_to_kernel() }
}

/// Dispatch a Linux x86-64 ring-3 syscall through `crate::linux::syscall`.
pub fn dispatch_syscall(frame: &mut SyscallFrame) -> SyscallReturn {
    linux_syscall::dispatch(frame)
}

/// Test-only syscall hooks invoked ONLY when `ProcessState::allow_test_syscalls`
/// is set (`usertest` runs). Normal `linux` binaries never reach these hooks.
pub(crate) fn dispatch_test_syscall(frame: &mut SyscallFrame) -> Option<SyscallReturn> {
    match frame.rax {
        SYS_TEST_NONCANON_SYSRET => {
            // Non-canonical boundary address: bit 47 = 1, bits 48..63 = 0.
            frame.rip = 0x0000_8000_0000_0000;
            Some(SyscallReturn::Sysret)
        }
        SYS_TEST_NONCANON_IRET => {
            frame.rip = 0x0000_8000_0000_0000;
            Some(SyscallReturn::Iret)
        }
        SYS_TEST_IRET_RETURN => {
            frame.rax = 0x1337;
            Some(SyscallReturn::Iret)
        }
        SYS_TEST_PREEMPT_COUNT => {
            if SSE_TLS_SYNC.aborted.load(Ordering::Acquire) {
                frame.rax = u64::MAX;
                return Some(SyscallReturn::Sysret);
            }
            let bit = (frame.rdi & 3) as u8;
            let now_mask = SSE_TLS_SYNC.ready_mask.fetch_or(bit, Ordering::AcqRel) | bit;
            if now_mask != 3 {
                frame.rax = 0;
            } else {
                let cpu = SSE_TLS_SYNC.target_cpu.load(Ordering::Acquire);
                let cur = sched::switch_count(cpu);
                let existing = SSE_TLS_SYNC.base_switches.load(Ordering::Acquire);
                let base = if existing == 0 {
                    let anchor = cur.max(1);
                    match SSE_TLS_SYNC.base_switches.compare_exchange(0, anchor, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => anchor,
                        Err(val) => val,
                    }
                } else {
                    existing
                };
                frame.rax = cur.saturating_sub(base);
            }
            Some(SyscallReturn::Sysret)
        }
        _ => None,
    }
}

/// Virtual layout for raw ring-3 test programs.
const TEST_CODE_VA: u64 = 0x0000_0000_0040_0000;
const TEST_STACK_VA: u64 = 0x0000_0000_0050_0000;
const TEST_DATA_VA: u64 = 0x0000_0000_0060_0000;

/// Result of running one ring-3 user process.
struct RunResult {
    exit: UserExit,
    stdout: Vec<u8>,
    deferred_lstar_db: u64,
    deferred_stub_db: u64,
}

/// RAII guard for an armed `DR0` execution breakpoint used by the deferred
/// `#DB` test modes; always disarms `DR7`/`DR0`/`DR6` on drop.
struct Dr0Guard(bool);

impl Dr0Guard {
    fn arm(target: Option<u64>) -> Self {
        if let Some(addr) = target {
            arch::write_dr6(0);
            arch::write_dr0(addr);
            arch::write_dr7(0x0000_0001); // L0 = 1, execute breakpoint (RW0=00, LEN0=00)
            Self(true)
        } else {
            Self(false)
        }
    }
}

impl Drop for Dr0Guard {
    fn drop(&mut self) {
        if self.0 {
            arch::write_dr7(0);
            arch::write_dr0(0);
            arch::write_dr6(0);
        }
    }
}

type ResultSlot = Arc<SpinLock<Option<RunResult>>>;

/// Spawn a pinned user thread on `cpu` executing `(entry, rsp)` inside `space`,
/// returning `(JoinHandle, result_slot)`.
#[allow(clippy::too_many_arguments)]
fn spawn_ring3_on(
    cpu: usize,
    name: &'static str,
    space: Arc<AddressSpace>,
    entry: u64,
    rsp: u64,
    dr0_target: Option<u64>,
    io: ConsoleIo,
    allow_test_syscalls: bool,
    stdin: Vec<u8>,
) -> Result<(sched::JoinHandle, ResultSlot), &'static str> {
    let result_slot: ResultSlot = Arc::new(SpinLock::new(None));
    let out = result_slot.clone();
    let handle = sched::spawn_user_on(cpu, name, space, move || {
        let _pid = register_current(io, allow_test_syscalls, stdin);

        let lstar_before = arch::idt::DEFERRED_LSTAR_DB.load(Ordering::Relaxed);
        let stub_before = arch::idt::DEFERRED_STUB_DB.load(Ordering::Relaxed);
        let _dr0 = Dr0Guard::arm(dr0_target);
        // SAFETY: `sched::spawn_user_on` loaded `space.root()` into CR3,
        // `entry` and `rsp` are mapped inside `space`, and interrupts are on.
        unsafe { arch::syscall::enter_ring3(entry, rsp, 0x0202) };
        drop(_dr0);
        let lstar_after = arch::idt::DEFERRED_LSTAR_DB.load(Ordering::Relaxed);
        let stub_after = arch::idt::DEFERRED_STUB_DB.load(Ordering::Relaxed);

        let proc = take_current();
        let exit = proc.exit.expect("ring-3 exited without recording UserExit");
        *out.lock() = Some(RunResult {
            exit,
            stdout: proc.stdout,
            deferred_lstar_db: lstar_after.saturating_sub(lstar_before),
            deferred_stub_db: stub_after.saturating_sub(stub_before),
        });
        // Detach and tear down user mappings with interrupts ON before exiting.
        if let Some(detached) = sched::detach_user_space()
            && Arc::strong_count(&detached) == 1
        {
            detached.teardown();
        }
    })
    .map_err(|_| "failed to spawn user thread")?;
    Ok((handle, result_slot))
}

/// Run a loaded Linux ELF64 process in ring 3 on the current CPU with the
/// given interactive console target (`allow_test_syscalls == false`).
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub fn run_elf_process(
    name: &'static str,
    space: Arc<AddressSpace>,
    entry: u64,
    rsp: u64,
    io: ConsoleIo,
) -> Result<UserExit, &'static str> {
    let (handle, slot) = spawn_ring3_on(arch::cpu_index(), name, space, entry, rsp, None, io, false, Vec::new())?;
    handle.join();
    let res = slot.lock().take().expect("linux process finished without RunResult");
    Ok(res.exit)
}

/// Run raw x86-64 user machine code `code` (with optional `data` mapped at
/// `TEST_DATA_VA`) in ring 3 inside a real [`AddressSpace`] on a user thread.
fn run_ring3_program(
    code: &[u8],
    data: &[u8],
    dr0_target: Option<u64>,
    stdin: Vec<u8>,
) -> Result<RunResult, &'static str> {
    let space = Arc::new(AddressSpace::new().map_err(|_| "out of frames for AddressSpace")?);
    let code_len = (code.len() as u64).max(FRAME_SIZE);
    let data_len = (data.len() as u64).max(FRAME_SIZE);
    space.map_eager_with_bytes(TEST_CODE_VA, code_len, Prot::READ | Prot::EXEC, code).map_err(|_| "map code")?;
    space.map_eager_with_bytes(TEST_STACK_VA, FRAME_SIZE, Prot::READ | Prot::WRITE, &[]).map_err(|_| "map stack")?;
    space.map_eager_with_bytes(TEST_DATA_VA, data_len, Prot::READ | Prot::WRITE, data).map_err(|_| "map data")?;

    let (handle, slot) = spawn_ring3_on(
        arch::cpu_index(),
        "usertest",
        space,
        TEST_CODE_VA,
        TEST_STACK_VA + FRAME_SIZE,
        dr0_target,
        ConsoleIo::Capture,
        true,
        stdin,
    )?;
    handle.join();
    Ok(slot.lock().take().expect("user thread finished without RunResult"))
}

/// Tiny x86-64 machine-code builder for ring-3 test programs.
struct CodeBuf(Vec<u8>);

impl CodeBuf {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn mov_eax(&mut self, imm: u32) {
        self.0.push(0xb8);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_ebx(&mut self, imm: u32) {
        self.0.push(0xbb);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_edx(&mut self, imm: u32) {
        self.0.push(0xba);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_esi(&mut self, imm: u32) {
        self.0.push(0xbe);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_edi(&mut self, imm: u32) {
        self.0.push(0xbf);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_ebp(&mut self, imm: u32) {
        self.0.push(0xbd);
        self.0.extend_from_slice(&imm.to_le_bytes());
    }
    fn mov_r12d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xbc]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r13d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xbd]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r14d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xbe]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r15d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xbf]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r10d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xba]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r8d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xb8]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r9d(&mut self, imm: u32) {
        self.bytes(&[0x41, 0xb9]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_rcx_u64(&mut self, imm: u64) {
        self.bytes(&[0x48, 0xb9]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_rdi_u64(&mut self, imm: u64) {
        self.bytes(&[0x48, 0xbf]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_rsi_u64(&mut self, imm: u64) {
        self.bytes(&[0x48, 0xbe]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_rdx_u64(&mut self, imm: u64) {
        self.bytes(&[0x48, 0xba]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_r10_u64(&mut self, imm: u64) {
        self.bytes(&[0x49, 0xba]);
        self.bytes(&imm.to_le_bytes());
    }
    fn mov_rbp_u64(&mut self, imm: u64) {
        self.bytes(&[0x48, 0xbd]);
        self.bytes(&imm.to_le_bytes());
    }
    fn exit_group_imm(&mut self, code: u32) {
        self.mov_edi(code);
        self.mov_eax(general::__NR_exit_group);
        self.syscall();
    }
    /// Emit `je +12; exit_group(code)` (skips the 12-byte exit sequence when ZF=1).
    fn fail_if_ne(&mut self, code: u32) {
        self.bytes(&[0x74, 0x0c]); // je +12
        self.exit_group_imm(code);
    }
    /// Emit `test rax, rax; jz +12; exit_group(code)`.
    fn fail_if_rax_nonzero(&mut self, code: u32) {
        self.bytes(&[0x48, 0x85, 0xc0]); // test rax, rax
        self.fail_if_ne(code);
    }
    /// Emit `cmp rax, rcx; je +12; exit_group(code)`.
    fn fail_if_rax_ne_rcx(&mut self, code: u32) {
        self.bytes(&[0x48, 0x39, 0xc8]); // cmp rax, rcx
        self.fail_if_ne(code);
    }
    fn syscall(&mut self) {
        self.bytes(&[0x0f, 0x05]);
    }
    fn ud2(&mut self) {
        self.bytes(&[0x0f, 0x0b]);
    }
}

/// Ring-3 program: writes `"hello from ring3\n"` via `SYS_write`, verifies
/// `rax == 17` and all 6 callee-saved GPRs survive `syscall`/`sysretq`, and
/// exits with status 42 (or 99 if any register was clobbered).
fn program_ok() -> (Vec<u8>, &'static [u8]) {
    let msg = b"hello from ring3\n";
    let mut c = CodeBuf::new();
    // Load callee-saved sentinels.
    c.mov_ebx(0x1111_2222);
    c.mov_ebp(0x3333_4444);
    c.mov_r12d(0x5555_6666);
    c.mov_r13d(0x7777_8888);
    c.mov_r14d(0x9999_aaaa);
    c.mov_r15d(0xbbbb_cccc);
    // write(1, TEST_DATA_VA, msg.len())
    c.mov_eax(general::__NR_write);
    c.mov_edi(1);
    c.mov_esi(TEST_DATA_VA as u32);
    c.mov_edx(msg.len() as u32);
    c.syscall();
    // Verify rax == msg.len() and all callee-saved registers match sentinels.
    c.bytes(&[0x35]); // xor eax, imm32
    c.bytes(&(msg.len() as u32).to_le_bytes());
    c.bytes(&[0x81, 0xf3]); // xor ebx, imm32
    c.bytes(&0x1111_2222u32.to_le_bytes());
    c.bytes(&[0x09, 0xd8]); // or eax, ebx
    c.bytes(&[0x81, 0xf5]); // xor ebp, imm32
    c.bytes(&0x3333_4444u32.to_le_bytes());
    c.bytes(&[0x09, 0xe8]); // or eax, ebp
    c.bytes(&[0x41, 0x81, 0xf4]); // xor r12d, imm32
    c.bytes(&0x5555_6666u32.to_le_bytes());
    c.bytes(&[0x44, 0x09, 0xe0]); // or eax, r12d
    c.bytes(&[0x41, 0x81, 0xf5]); // xor r13d, imm32
    c.bytes(&0x7777_8888u32.to_le_bytes());
    c.bytes(&[0x44, 0x09, 0xe8]); // or eax, r13d
    c.bytes(&[0x41, 0x81, 0xf6]); // xor r14d, imm32
    c.bytes(&0x9999_aaaau32.to_le_bytes());
    c.bytes(&[0x44, 0x09, 0xf0]); // or eax, r14d
    c.bytes(&[0x41, 0x81, 0xf7]); // xor r15d, imm32
    c.bytes(&0xbbbb_ccccu32.to_le_bytes());
    c.bytes(&[0x44, 0x09, 0xf8]); // or eax, r15d
    // edi = (eax == 0) ? 42 : 99
    c.bytes(&[0x85, 0xc0]); // test eax, eax
    c.mov_edi(42);
    c.mov_esi(99);
    c.bytes(&[0x0f, 0x45, 0xfe]); // cmovne edi, esi
    c.mov_eax(general::__NR_exit_group);
    c.syscall();
    c.ud2();
    (c.0, msg)
}

/// Ring-3 program exercising the `iretq` syscall return path (`SYS_TEST_IRET_RETURN`).
fn program_iret() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.mov_eax(SYS_TEST_IRET_RETURN as u32);
    c.syscall();
    // edi = (eax == 0x1337) ? 7 : 98
    c.bytes(&[0x35]); // xor eax, 0x1337
    c.bytes(&0x1337u32.to_le_bytes());
    c.bytes(&[0x85, 0xc0]); // test eax, eax
    c.mov_edi(7);
    c.mov_esi(98);
    c.bytes(&[0x0f, 0x45, 0xfe]); // cmovne edi, esi
    c.mov_eax(general::__NR_exit);
    c.syscall();
    c.ud2();
    c.0
}

/// Ring-3 `#PF`: read from unmapped user address `0x0000_0000_dead_b000`.
fn program_pf() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.mov_eax(0xdead_b000);
    c.bytes(&[0x8b, 0x00]); // mov eax, [rax]
    c.ud2();
    c.0
}

/// Ring-3 `#UD`: execute `ud2`.
fn program_ud() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.ud2();
    c.0
}

/// Ring-3 `#GP`: execute privileged `cli` (with `IOPL = 0`).
fn program_gp() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.bytes(&[0xfa]); // cli
    c.ud2();
    c.0
}

/// Ring-3 `#BP`: execute `int3` (must kill user process, not resume kernel ktest).
fn program_bp() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.bytes(&[0xcc]); // int3
    c.ud2();
    c.0
}

/// Ring-3 `#DB`: set `RFLAGS.TF = 1` and single-step a `nop`.
fn program_db() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.bytes(&[0x9c]); // pushfq
    c.bytes(&[0x81, 0x0c, 0x24, 0x00, 0x01, 0x00, 0x00]); // or dword ptr [rsp], 0x100 (TF)
    c.bytes(&[0x9d]); // popfq
    c.bytes(&[0x90, 0x90]); // nop; nop
    c.ud2();
    c.0
}

/// CVE-2018-8897: `MOV SS; SYSCALL` with `TF = 1`, deferring `#DB` onto
/// `LSTAR` (`lumen_syscall_entry`) on `IST_DEBUG`.
fn program_cve_db() -> Vec<u8> {
    let mut c = CodeBuf::new();
    // If the syscall were erroneously executed instead of aborted by the
    // pending debug trap, the process would exit(77) rather than fault(1).
    c.mov_edi(77);
    c.mov_eax(general::__NR_exit);
    c.bytes(&[0x8c, 0xd1]); // mov cx, ss
    c.bytes(&[0x9c]); // pushfq
    c.bytes(&[0x81, 0x0c, 0x24, 0x00, 0x01, 0x00, 0x00]); // or dword ptr [rsp], 0x100 (TF)
    c.bytes(&[0x9d]); // popfq
    c.bytes(&[0x8e, 0xd1]); // mov ss, cx (suppresses #DB for 1 instruction)
    c.syscall(); // #DB delivered at LSTAR (CS = KERNEL_CODE, RSP = user RSP)
    c.ud2();
    c.0
}

/// CVE-2018-8897: `MOV SS; INT3` with `TF = 1`, deferring `#DB` onto IDT
/// vector 3's stub entry before `lumen_isr_common`'s `swapgs`.
fn program_movss_bp() -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.bytes(&[0x8c, 0xd1]); // mov cx, ss
    c.bytes(&[0x9c]); // pushfq
    c.bytes(&[0x81, 0x0c, 0x24, 0x00, 0x01, 0x00, 0x00]); // or dword ptr [rsp], 0x100 (TF)
    c.bytes(&[0x9d]); // popfq
    c.bytes(&[0x8e, 0xd1]); // mov ss, cx (suppresses #DB for 1 instruction)
    c.bytes(&[0xcc]); // int3 (#DB delivered at IDT stub 3 before `swapgs`)
    c.ud2();
    c.0
}

/// Ring-3 test triggering a non-canonical `rip` (`0x0000_8000_0000_0000`) on
/// the `sysretq` or `iretq` return path.
fn program_noncanon(iret_path: bool) -> Vec<u8> {
    let mut c = CodeBuf::new();
    c.mov_eax(if iret_path { SYS_TEST_NONCANON_IRET as u32 } else { SYS_TEST_NONCANON_SYSRET as u32 });
    c.syscall();
    c.ud2();
    c.0
}

/// Build a minimal static ELF64 (`ET_EXEC`) with custom program headers.
fn build_elf_with_phdrs(
    entry: u64,
    phdrs: &[(u32, u32, u64, u64, u64, u64)],
    payload_at_page: Option<&[u8]>,
) -> Vec<u8> {
    let total_len = if payload_at_page.is_some() { 0x2000 } else { 0x1000 };
    let mut img = vec![0u8; total_len];
    img[0..4].copy_from_slice(&ELF_MAGIC);
    img[4] = ELFCLASS64;
    img[5] = ELFDATA2LSB;
    img[6] = EV_CURRENT;
    img[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
    img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
    img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
    img[0x18..0x20].copy_from_slice(&entry.to_le_bytes());
    img[0x20..0x28].copy_from_slice(&(ELF_EHDR_SIZE as u64).to_le_bytes());
    img[0x34..0x36].copy_from_slice(&(ELF_EHDR_SIZE as u16).to_le_bytes());
    img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
    img[0x38..0x3a].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());

    for (idx, &(p_type, flags, off, va, filesz, memsz)) in phdrs.iter().enumerate() {
        let o = ELF_EHDR_SIZE + idx * usize::from(PHDR_MIN_SIZE);
        img[o..o + 4].copy_from_slice(&p_type.to_le_bytes());
        img[o + 4..o + 8].copy_from_slice(&flags.to_le_bytes());
        img[o + 8..o + 16].copy_from_slice(&off.to_le_bytes());
        img[o + 16..o + 24].copy_from_slice(&va.to_le_bytes());
        img[o + 24..o + 32].copy_from_slice(&va.to_le_bytes());
        img[o + 32..o + 40].copy_from_slice(&filesz.to_le_bytes());
        img[o + 40..o + 48].copy_from_slice(&memsz.to_le_bytes());
        img[o + 48..o + 56].copy_from_slice(&FRAME_SIZE.to_le_bytes());
    }
    if let Some(code) = payload_at_page {
        img[0x1000..0x1000 + code.len()].copy_from_slice(code);
    }
    img
}

/// Exercise malformed/W+X/overlapping/kernel-half/PT_INTERP/RELRO-W+X ELF
/// rejections through `AddressSpace::from_elf`, verifying zero frame and heap
/// leaks across all rejections.
fn run_bad_elf_tests(ctx: &mut Ctx<'_>) {
    // Pre-build all bad ELF images before measuring frames/heap so test fixture
    // allocation itself isn't counted as a loader leak.
    let bad_truncated = b"\x7fELF\x02\x01\x01".to_vec();
    let bad_wx = build_elf_with_phdrs(0x40_1000, &[(PT_LOAD, PF_R | PF_W | PF_X, 0x100, 0x40_1000, 0x10, 0x10)], None);
    let bad_overlap = build_elf_with_phdrs(
        0x40_1000,
        &[(PT_LOAD, PF_R | PF_X, 0x100, 0x40_1000, 0x200, 0x200), (PT_LOAD, PF_R, 0x200, 0x40_1100, 0x200, 0x200)],
        None,
    );
    let bad_kernel_half = build_elf_with_phdrs(
        0xffff_8000_0000_1000,
        &[(PT_LOAD, PF_R | PF_X, 0x100, 0xffff_8000_0000_1000, 0x10, 0x10)],
        None,
    );
    let bad_interp = build_elf_with_phdrs(
        0x40_1000,
        &[(PT_INTERP, PF_R, 0x100, 0x40_0100, 0x20, 0x20), (PT_LOAD, PF_R | PF_X, 0x100, 0x40_1000, 0x10, 0x10)],
        None,
    );
    let bad_shared_wx = build_elf_with_phdrs(
        0x40_1000,
        &[
            (PT_LOAD, PF_R | PF_X, 0x100, 0x40_1000, 0x400, 0x400),
            (PT_LOAD, PF_R | PF_W, 0x500, 0x40_1400, 0x400, 0x400),
        ],
        None,
    );
    // Passes `LoadedElf::parse` but places PT_LOAD inside the reserved stack +
    // guard region, exercising `commit_loaded_elf` rejection + AddressSpace::drop.
    let bad_stack_overlap =
        build_elf_with_phdrs(USER_STACK_BOTTOM, &[(PT_LOAD, PF_R | PF_X, 0x100, USER_STACK_BOTTOM, 0x10, 0x10)], None);

    type BadElfCase<'a> = (&'static str, &'a [u8], fn(&UserVmError) -> bool);
    let cases: [BadElfCase<'_>; 7] = [
        ("truncated", &bad_truncated, |e| matches!(e, UserVmError::Elf(ElfLoadError::Truncated))),
        ("wx", &bad_wx, |e| matches!(e, UserVmError::Elf(ElfLoadError::WritableAndExecutable { .. }))),
        ("overlap", &bad_overlap, |e| matches!(e, UserVmError::Elf(ElfLoadError::OverlappingSegments { .. }))),
        ("kernel_half", &bad_kernel_half, |e| matches!(e, UserVmError::Elf(ElfLoadError::AddressOutOfRange { .. }))),
        ("pt_interp", &bad_interp, |e| matches!(e, UserVmError::Elf(ElfLoadError::DynamicInterpreterUnsupported))),
        ("shared_wx", &bad_shared_wx, |e| {
            matches!(e, UserVmError::Elf(ElfLoadError::SharedPageWritableAndExecutable { .. }))
        }),
        ("stack_overlap", &bad_stack_overlap, |e| matches!(e, UserVmError::Vm(lumen_vm::VmError::Exists))),
    ];

    // Pre-warm kernel heap before measuring so first-time heap expansion is
    // not counted as a loader leak.
    let _warm: Vec<u8> = vec![0u8; 8192];
    drop(_warm);

    let frames_before = frame::free_frames();
    let heap_before = heap::stats().used;
    let mut rejected = 0usize;
    for (name, img, check) in cases {
        match AddressSpace::from_elf(img, &[b"/bad"], &[], b"/bad", [0; 16]) {
            Err(err) if check(&err) => rejected += 1,
            Err(err) => {
                return ctx.fail(format_args!("usertest: bad_elf {name}: unexpected error {err}"));
            }
            Ok((space, _)) => {
                space.teardown();
                return ctx.fail(format_args!("usertest: bad_elf {name}: unexpectedly accepted"));
            }
        }
    }
    let frames_after = frame::free_frames();
    let heap_after = heap::stats().used;
    let leaked_frames = frames_before as i64 - frames_after as i64;
    let leaked_heap = heap_after as i64 - heap_before as i64;
    let _ = writeln!(
        ctx.out,
        "usertest: bad_elf rejected={rejected} leaked_frames={leaked_frames} leaked_heap={leaked_heap}"
    );
}

/// Ring-3 machine code (loaded as a real static ELF64) exercising:
/// `mmap` (2 pages lazy RW), demand-paged `#PF` zero-fill, `mprotect(PROT_NONE)`
/// -> `mprotect(PROT_READ|PROT_WRITE)` preserving data, byte-granular `brk`
/// grow/shrink/invalid, and `munmap` freeing frames. Exits 0 on success.
fn program_mmap_brk() -> Vec<u8> {
    const SENT0: u64 = 0x1122_3344_5566_7788;
    const SENT1: u64 = 0xaabb_ccdd_eeff_0011;
    const BRK0: u64 = 0xdead_beef_cafe_babe;
    const BRK1: u64 = 0x55aa_55aa_55aa_55aa;

    let mut c = CodeBuf::new();
    // 1. rbx = mmap(0, 2*4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    c.mov_eax(general::__NR_mmap);
    c.mov_edi(0);
    c.mov_esi(2 * FRAME_SIZE as u32);
    c.mov_edx(general::PROT_READ | general::PROT_WRITE);
    c.mov_r10d(general::MAP_PRIVATE | general::MAP_ANONYMOUS);
    c.mov_r8d(u32::MAX);
    c.mov_r9d(0);
    c.syscall();
    // Check rax > 0 (not negative errno): test rax, rax; jns +12; exit_group(1)
    c.bytes(&[0x48, 0x85, 0xc0]);
    c.bytes(&[0x79, 0x0c]);
    c.exit_group_imm(1);
    c.bytes(&[0x48, 0x89, 0xc3]); // mov rbx, rax

    // 2. Verify demand-paged zero-fill on page 0 and page 1.
    c.bytes(&[0x48, 0x8b, 0x03]); // mov rax, [rbx]
    c.fail_if_rax_nonzero(2);
    c.bytes(&[0x48, 0x8b, 0x83]); // mov rax, [rbx + 4096]
    c.bytes(&(FRAME_SIZE as u32).to_le_bytes());
    c.fail_if_rax_nonzero(3);

    // 3. Write sentinels into page 0 and page 1.
    c.mov_rcx_u64(SENT0);
    c.bytes(&[0x48, 0x89, 0x0b]); // mov [rbx], rcx
    c.mov_rcx_u64(SENT1);
    c.bytes(&[0x48, 0x89, 0x8b]); // mov [rbx + 4096], rcx
    c.bytes(&(FRAME_SIZE as u32).to_le_bytes());

    // 4. mprotect(rbx, 4096, PROT_NONE) -> 0
    c.mov_eax(general::__NR_mprotect);
    c.bytes(&[0x48, 0x89, 0xdf]); // mov rdi, rbx
    c.mov_esi(FRAME_SIZE as u32);
    c.mov_edx(general::PROT_NONE);
    c.syscall();
    c.fail_if_rax_nonzero(4);

    // 5. mprotect(rbx, 4096, PROT_READ|PROT_WRITE) -> 0
    c.mov_eax(general::__NR_mprotect);
    c.bytes(&[0x48, 0x89, 0xdf]); // mov rdi, rbx
    c.mov_esi(FRAME_SIZE as u32);
    c.mov_edx(general::PROT_READ | general::PROT_WRITE);
    c.syscall();
    c.fail_if_rax_nonzero(5);

    // 6. Verify page 0 sentinel survived PROT_NONE round-trip and page 1 is intact.
    c.bytes(&[0x48, 0x8b, 0x03]); // mov rax, [rbx]
    c.mov_rcx_u64(SENT0);
    c.fail_if_rax_ne_rcx(6);
    c.bytes(&[0x48, 0x8b, 0x83]); // mov rax, [rbx + 4096]
    c.bytes(&(FRAME_SIZE as u32).to_le_bytes());
    c.mov_rcx_u64(SENT1);
    c.fail_if_rax_ne_rcx(7);

    // 7. rbp = brk(0) (initial_brk)
    c.mov_eax(general::__NR_brk);
    c.mov_edi(0);
    c.syscall();
    c.bytes(&[0x48, 0x89, 0xc5]); // mov rbp, rax

    // 8. brk(rbp + 137) -> exact rbp + 137
    c.mov_eax(general::__NR_brk);
    c.bytes(&[0x48, 0x8d, 0xbd]); // lea rdi, [rbp + 137]
    c.bytes(&137u32.to_le_bytes());
    c.syscall();
    c.bytes(&[0x48, 0x89, 0xf9]); // mov rcx, rdi (rdi preserved across syscall; rcx clobbered)
    c.fail_if_rax_ne_rcx(8);

    // 9. Verify brk page 0 demand-paged zero-fill, write BRK0.
    c.bytes(&[0x48, 0x8b, 0x85]); // mov rax, [rbp + 0]
    c.bytes(&0u32.to_le_bytes());
    c.fail_if_rax_nonzero(9);
    c.mov_rcx_u64(BRK0);
    c.bytes(&[0x48, 0x89, 0x8d]); // mov [rbp + 0], rcx
    c.bytes(&0u32.to_le_bytes());

    // 10. brk(rbp + 4096 + 500) -> exact, verify zero-fill on page 1, write BRK1.
    c.mov_eax(general::__NR_brk);
    c.bytes(&[0x48, 0x8d, 0xbd]); // lea rdi, [rbp + 4596]
    c.bytes(&(FRAME_SIZE as u32 + 500).to_le_bytes());
    c.syscall();
    c.bytes(&[0x48, 0x89, 0xf9]); // mov rcx, rdi
    c.fail_if_rax_ne_rcx(10);
    c.bytes(&[0x48, 0x8b, 0x85]); // mov rax, [rbp + 4096]
    c.bytes(&(FRAME_SIZE as u32).to_le_bytes());
    c.fail_if_rax_nonzero(11);
    c.mov_rcx_u64(BRK1);
    c.bytes(&[0x48, 0x89, 0x8d]); // mov [rbp + 4096], rcx
    c.bytes(&(FRAME_SIZE as u32).to_le_bytes());

    // 11. Shrink brk back to rbp + 137 -> exact, page 0 data still intact.
    c.mov_eax(general::__NR_brk);
    c.bytes(&[0x48, 0x8d, 0xbd]); // lea rdi, [rbp + 137]
    c.bytes(&137u32.to_le_bytes());
    c.syscall();
    c.bytes(&[0x48, 0x89, 0xf9]); // mov rcx, rdi
    c.fail_if_rax_ne_rcx(12);
    c.bytes(&[0x48, 0x8b, 0x85]); // mov rax, [rbp + 0]
    c.bytes(&0u32.to_le_bytes());
    c.mov_rcx_u64(BRK0);
    c.fail_if_rax_ne_rcx(13);

    // 12. Invalid brk(rbp - 16) returns unchanged rbp + 137.
    c.mov_eax(general::__NR_brk);
    c.bytes(&[0x48, 0x8d, 0xbd]); // lea rdi, [rbp - 16]
    c.bytes(&(-16i32 as u32).to_le_bytes());
    c.syscall();
    c.bytes(&[0x48, 0x8d, 0x8d]); // lea rcx, [rbp + 137]
    c.bytes(&137u32.to_le_bytes());
    c.fail_if_rax_ne_rcx(14);

    // 13. munmap(rbx, 2*4096) -> 0
    c.mov_eax(general::__NR_munmap);
    c.bytes(&[0x48, 0x89, 0xdf]); // mov rdi, rbx
    c.mov_esi(2 * FRAME_SIZE as u32);
    c.syscall();
    c.fail_if_rax_nonzero(15);

    // 14. Clean exit(0).
    c.exit_group_imm(0);
    c.ud2();
    c.0
}

fn run_mmap_brk_test(ctx: &mut Ctx<'_>) {
    let code = program_mmap_brk();
    let elf = build_elf_with_phdrs(
        0x40_1000,
        &[(PT_LOAD, PF_R | PF_X, 0x1000, 0x40_1000, code.len() as u64, code.len() as u64)],
        Some(&code),
    );
    // Pre-warm heap so thread/slot metadata reuses existing heap pages.
    let _warm: Vec<u8> = vec![0u8; 8192];
    drop(_warm);

    let frames_before = frame::free_frames();
    let (space, prog) = match AddressSpace::from_elf(&elf, &[b"/mmap_brk"], &[], b"/mmap_brk", [0x11; 16]) {
        Ok(loaded) => loaded,
        Err(e) => return ctx.fail(format_args!("usertest: mmap_brk load failed: {e}")),
    };
    let space = Arc::new(space);
    let (handle, slot) = match spawn_ring3_on(
        arch::cpu_index(),
        "mmap-brk",
        space,
        prog.entry,
        prog.rsp,
        None,
        ConsoleIo::Capture,
        true,
        Vec::new(),
    ) {
        Ok(pair) => pair,
        Err(e) => return ctx.fail(format_args!("usertest: mmap_brk spawn failed: {e}")),
    };
    handle.join();
    let result = slot.lock().take().expect("mmap-brk finished without RunResult");
    drop(slot);
    let frames_after = frame::free_frames();
    let leaked_frames = frames_before as i64 - frames_after as i64;
    match result.exit {
        UserExit::Exited(status) => {
            let _ = writeln!(ctx.out, "usertest: mmap_brk exit={status} leaked_frames={leaked_frames}");
        }
        other => {
            ctx.fail(format_args!("usertest: mmap_brk unexpected exit {other:?} leaked_frames={leaked_frames}"));
        }
    }
}

/// Ring-3 machine code + data for one worker in the concurrent same-CPU
/// SSE (`xmm0`–`xmm7`) + TLS (`IA32_FS_BASE` / `%fs:0`) preemption test.
fn program_sse_tls(worker_bit: u32, tls_offset: u64, seed: u8, min_preempts: u8) -> (Vec<u8>, Vec<u8>) {
    // Data page layout at TEST_DATA_VA:
    //   [0x000..0x080]: 8 x 16-byte distinct expected patterns for xmm0..xmm7
    //   [0x080..0x088]: scratch u64 for arch_prctl(ARCH_GET_FS)
    //   [tls_offset..tls_offset+24]: TLS block (fs:0 = sentinel0, fs:8 = sentinel1, fs:16 = iter)
    let mut data = vec![0u8; 0x300];
    for reg in 0..8usize {
        for b in 0..16usize {
            data[reg * 16 + b] = seed.wrapping_add((reg as u8).wrapping_mul(17)).wrapping_add(b as u8);
        }
    }
    let tls_addr = TEST_DATA_VA + tls_offset;
    let getfs_scratch = TEST_DATA_VA + 0x80;
    let tls_sent0 = u64::from_le_bytes([seed; 8]) ^ 0x1357_2468_9bdf_0246;
    let tls_sent1 = !tls_sent0;

    let mut c = CodeBuf::new();
    // 1. arch_prctl(ARCH_SET_FS, tls_addr) -> 0
    c.mov_eax(general::__NR_arch_prctl);
    c.mov_edi(ARCH_SET_FS);
    c.mov_rsi_u64(tls_addr);
    c.syscall();
    c.fail_if_rax_nonzero(1);

    // 2. arch_prctl(ARCH_GET_FS, getfs_scratch) -> 0 and *getfs_scratch == tls_addr
    c.mov_eax(general::__NR_arch_prctl);
    c.mov_edi(ARCH_GET_FS);
    c.mov_rsi_u64(getfs_scratch);
    c.syscall();
    c.fail_if_rax_nonzero(2);
    c.mov_rbp_u64(TEST_DATA_VA);
    c.bytes(&[0x48, 0x8b, 0x85]); // mov rax, [rbp + 0x80]
    c.bytes(&0x80u32.to_le_bytes());
    c.mov_rcx_u64(tls_addr);
    c.fail_if_rax_ne_rcx(3);

    // 3. rbx = 0; initialize %fs:0, %fs:8, %fs:16.
    c.bytes(&[0x31, 0xdb]); // xor ebx, ebx
    c.mov_rcx_u64(tls_sent0);
    c.bytes(&[0x64, 0x48, 0x89, 0x4b, 0x00]); // mov fs:[rbx + 0], rcx
    c.mov_rcx_u64(tls_sent1);
    c.bytes(&[0x64, 0x48, 0x89, 0x4b, 0x08]); // mov fs:[rbx + 8], rcx
    c.bytes(&[0x48, 0x31, 0xc9]); // xor rcx, rcx
    c.bytes(&[0x64, 0x48, 0x89, 0x4b, 0x10]); // mov fs:[rbx + 16], rcx
    c.bytes(&[0x4d, 0x31, 0xe4]); // xor r12, r12 (expected local counter)

    // 4. Load xmm0..xmm7 ONCE from [rbp + k*16]; never reloaded inside loop.
    for k in 0..8u8 {
        c.bytes(&[0xf3, 0x0f, 0x6f, 0x45 | (k << 3), k * 16]); // movdqu xmmK, [rbp + k*16]
    }

    // 5. Loop verifying xmm0..xmm7 and %fs:0/%fs:8/%fs:16 across >= min_preempts switches.
    let loop_top = c.0.len();
    for k in 0..8u8 {
        // movdqu xmm8, [rbp + k*16]
        c.bytes(&[0xf3, 0x44, 0x0f, 0x6f, 0x45, k * 16]);
        // pcmpeqb xmm8, xmmK
        c.bytes(&[0x66, 0x44, 0x0f, 0x74, 0xc0 | k]);
        // pmovmskb eax, xmm8
        c.bytes(&[0x66, 0x41, 0x0f, 0xd7, 0xc0]);
        // cmp eax, 0xffff; je +12; exit_group(10 + k)
        c.bytes(&[0x3d, 0xff, 0xff, 0x00, 0x00]);
        c.fail_if_ne(10 + u32::from(k));
    }

    // Verify %fs:0 == tls_sent0, %fs:8 == tls_sent1, %fs:16 == r12, then increment both.
    c.bytes(&[0x64, 0x48, 0x8b, 0x43, 0x00]); // mov rax, fs:[rbx + 0]
    c.mov_rcx_u64(tls_sent0);
    c.fail_if_rax_ne_rcx(20);
    c.bytes(&[0x64, 0x48, 0x8b, 0x43, 0x08]); // mov rax, fs:[rbx + 8]
    c.mov_rcx_u64(tls_sent1);
    c.fail_if_rax_ne_rcx(21);
    c.bytes(&[0x64, 0x48, 0x8b, 0x43, 0x10]); // mov rax, fs:[rbx + 16]
    c.bytes(&[0x4c, 0x39, 0xe0]); // cmp rax, r12
    c.fail_if_ne(22);
    c.bytes(&[0x49, 0xff, 0xc4]); // inc r12
    c.bytes(&[0x64, 0x4c, 0x89, 0x63, 0x10]); // mov fs:[rbx + 16], r12

    // Short busy spin while holding live xmm0..xmm7 and %fs so timer ticks fire in ring 3.
    c.bytes(&[0xb9]); // mov ecx, 2000
    c.bytes(&2000u32.to_le_bytes());
    c.bytes(&[0xf3, 0x90]); // pause
    c.bytes(&[0xff, 0xc9]); // dec ecx
    c.bytes(&[0x75, 0xfa]); // jnz -6

    // Check how many real scheduler switches have occurred since both workers entered ring 3.
    c.mov_eax(SYS_TEST_PREEMPT_COUNT as u32);
    c.mov_edi(worker_bit);
    c.syscall();
    c.bytes(&[0x48, 0x83, 0xf8, min_preempts]); // cmp rax, min_preempts
    let disp = (loop_top as i64 - (c.0.len() as i64 + 6)) as i32;
    c.bytes(&[0x0f, 0x82]); // jb loop_top
    c.bytes(&disp.to_le_bytes());

    c.exit_group_imm(0);
    c.ud2();
    (c.0, data)
}

fn run_sse_tls_test(ctx: &mut Ctx<'_>) {
    const MIN_PREEMPTS: u8 = 6;
    // Pin BOTH user processes to the same CPU (prefer AP 1 if online so serial
    // shell I/O never competes; otherwise CPU 0).
    let target_cpu = if sched::running_on(1) { 1 } else { arch::cpu_index() };

    let (code_a, data_a) = program_sse_tls(1, 0x100, 0x21, MIN_PREEMPTS);
    let (code_b, data_b) = program_sse_tls(2, 0x200, 0x97, MIN_PREEMPTS);

    let _warm: Vec<u8> = vec![0u8; 8192];
    drop(_warm);

    let frames_before = frame::free_frames();
    SSE_TLS_SYNC.ready_mask.store(0, Ordering::Release);
    SSE_TLS_SYNC.aborted.store(false, Ordering::Release);
    SSE_TLS_SYNC.base_switches.store(0, Ordering::Release);
    SSE_TLS_SYNC.target_cpu.store(target_cpu, Ordering::Release);

    let make_space = |code: &[u8], data: &[u8]| -> Result<Arc<AddressSpace>, &'static str> {
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        space.map_eager_with_bytes(TEST_CODE_VA, FRAME_SIZE, Prot::READ | Prot::EXEC, code).map_err(|_| "map code")?;
        space
            .map_eager_with_bytes(TEST_STACK_VA, FRAME_SIZE, Prot::READ | Prot::WRITE, &[])
            .map_err(|_| "map stack")?;
        space.map_eager_with_bytes(TEST_DATA_VA, FRAME_SIZE, Prot::READ | Prot::WRITE, data).map_err(|_| "map data")?;
        Ok(space)
    };

    let space_a = match make_space(&code_a, &data_a) {
        Ok(s) => s,
        Err(e) => return ctx.fail(format_args!("usertest: sse_tls space_a: {e}")),
    };
    let space_b = match make_space(&code_b, &data_b) {
        Ok(s) => s,
        Err(e) => return ctx.fail(format_args!("usertest: sse_tls space_b: {e}")),
    };

    let (ha, slot_a) = match spawn_ring3_on(
        target_cpu,
        "sse-tls-a",
        space_a,
        TEST_CODE_VA,
        TEST_STACK_VA + FRAME_SIZE,
        None,
        ConsoleIo::Capture,
        true,
        Vec::new(),
    ) {
        Ok(p) => p,
        Err(e) => return ctx.fail(format_args!("usertest: sse_tls spawn A: {e}")),
    };
    let (hb, slot_b) = match spawn_ring3_on(
        target_cpu,
        "sse-tls-b",
        space_b,
        TEST_CODE_VA,
        TEST_STACK_VA + FRAME_SIZE,
        None,
        ConsoleIo::Capture,
        true,
        Vec::new(),
    ) {
        Ok(p) => p,
        Err(e) => {
            SSE_TLS_SYNC.aborted.store(true, Ordering::Release);
            ha.join();
            return ctx.fail(format_args!("usertest: sse_tls spawn B: {e}"));
        }
    };

    ha.join();
    hb.join();

    let base = SSE_TLS_SYNC.base_switches.load(Ordering::Acquire);
    let switches = if base > 0 { sched::switch_count(target_cpu).saturating_sub(base) } else { 0 };
    let res_a = slot_a.lock().take().expect("sse-tls-a finished without RunResult");
    let res_b = slot_b.lock().take().expect("sse-tls-b finished without RunResult");
    drop(slot_a);
    drop(slot_b);

    let frames_after = frame::free_frames();
    let leaked_frames = frames_before as i64 - frames_after as i64;

    match (res_a.exit, res_b.exit) {
        (UserExit::Exited(ea), UserExit::Exited(eb)) => {
            let _ = writeln!(
                ctx.out,
                "usertest: sse_tls cpu={target_cpu} exit_a={ea} exit_b={eb} switches={switches} leaked_frames={leaked_frames}"
            );
        }
        (ea, eb) => {
            ctx.fail(format_args!(
                "usertest: sse_tls unexpected exit a={ea:?} b={eb:?} switches={switches} leaked_frames={leaked_frames}"
            ));
        }
    }
}

/// Ring-3 program exercising every pointer-taking M6 syscall with null,
/// kernel-half, and unmapped user pointers (`-EFAULT`), plus unknown syscall
/// (`-ENOSYS`), closed-fd operations (`-EBADF`), invalid arguments
/// (`-EINVAL`/`-ERANGE`/`-ESRCH`/`-ENOTTY`), and zero-length iovec skipping.
fn program_bad_ptrs() -> (Vec<u8>, Vec<u8>) {
    const NULL_VA: u64 = 0;
    const LOW_BAD_VA: u64 = 0x1000;
    const KERNEL_VA: u64 = 0xffff_8000_0000_0000;
    const UNMAPPED_VA: u64 = TEST_DATA_VA + FRAME_SIZE;

    let valid_buf = TEST_DATA_VA;
    let iov_bad_unmapped = TEST_DATA_VA + 0x20;
    let iov_bad_kernel = TEST_DATA_VA + 0x30;
    let iov_skip_zero = TEST_DATA_VA + 0x40;
    let valid_out = TEST_DATA_VA + 0x100;

    let mut data = vec![0u8; 0x300];
    data[0..4].copy_from_slice(b"ok!\n");
    // iov_bad_unmapped: { base = UNMAPPED_VA, len = 4 }
    data[0x20..0x28].copy_from_slice(&UNMAPPED_VA.to_le_bytes());
    data[0x28..0x30].copy_from_slice(&4u64.to_le_bytes());
    // iov_bad_kernel: { base = KERNEL_VA, len = 4 }
    data[0x30..0x38].copy_from_slice(&KERNEL_VA.to_le_bytes());
    data[0x38..0x40].copy_from_slice(&4u64.to_le_bytes());
    // iov_skip_zero: [ { base = 0, len = 0 }, { base = valid_buf, len = 4 } ]
    data[0x40..0x48].copy_from_slice(&0u64.to_le_bytes());
    data[0x48..0x50].copy_from_slice(&0u64.to_le_bytes());
    data[0x50..0x58].copy_from_slice(&valid_buf.to_le_bytes());
    data[0x58..0x60].copy_from_slice(&4u64.to_le_bytes());

    let efault = -(errno::EFAULT as i64) as u64;
    let enosys = -(errno::ENOSYS as i64) as u64;
    let ebadf = -(errno::EBADF as i64) as u64;
    let einval = -(errno::EINVAL as i64) as u64;
    let erange = -(errno::ERANGE as i64) as u64;
    let esrch = -(errno::ESRCH as i64) as u64;
    let enotty = -(errno::ENOTTY as i64) as u64;

    let mut c = CodeBuf::new();
    let mut step = 1u32;
    let mut check_syscall = |c: &mut CodeBuf, nr: u32, a1: u64, a2: u64, a3: u64, a4: u64, expect: u64| {
        c.mov_eax(nr);
        c.mov_rdi_u64(a1);
        c.mov_rsi_u64(a2);
        c.mov_rdx_u64(a3);
        c.mov_r10_u64(a4);
        c.syscall();
        c.mov_rcx_u64(expect);
        c.fail_if_rax_ne_rcx(step);
        step += 1;
    };

    for bad in [NULL_VA, KERNEL_VA, UNMAPPED_VA] {
        // 1. read / write
        check_syscall(&mut c, general::__NR_read, 0, bad, 4, 0, efault);
        check_syscall(&mut c, general::__NR_write, 1, bad, 4, 0, efault);
        // 2. readv / writev (bad iov array pointer)
        check_syscall(&mut c, general::__NR_readv, 0, bad, 1, 0, efault);
        check_syscall(&mut c, general::__NR_writev, 1, bad, 1, 0, efault);
        // 3. fstat
        check_syscall(&mut c, general::__NR_fstat, 1, bad, 0, 0, efault);
        // 4. ioctl TCGETS / TCSETS / TIOCGWINSZ
        check_syscall(&mut c, general::__NR_ioctl, 1, u64::from(ioctl::TCGETS), bad, 0, efault);
        check_syscall(&mut c, general::__NR_ioctl, 1, u64::from(ioctl::TCSETS), bad, 0, efault);
        check_syscall(&mut c, general::__NR_ioctl, 1, u64::from(ioctl::TIOCGWINSZ), bad, 0, efault);
        // 5. uname / getcwd
        check_syscall(&mut c, general::__NR_uname, bad, 0, 0, 0, efault);
        check_syscall(&mut c, general::__NR_getcwd, bad, 64, 0, 0, efault);
        // 6. arch_prctl(ARCH_GET_FS)
        check_syscall(&mut c, general::__NR_arch_prctl, u64::from(ARCH_GET_FS), bad, 0, 0, efault);
        // 7. clock_gettime
        check_syscall(&mut c, general::__NR_clock_gettime, u64::from(general::CLOCK_MONOTONIC), bad, 0, 0, efault);
        // 8. getrandom / poll
        check_syscall(&mut c, general::__NR_getrandom, bad, 8, 0, 0, efault);
        check_syscall(&mut c, general::__NR_poll, bad, 1, 0, 0, efault);
    }

    // Optional-pointer syscalls (where NULL means "omitted"): test non-null
    // invalid addresses below USER_MIN, in the kernel half, and unmapped user VA.
    for bad in [LOW_BAD_VA, KERNEL_VA, UNMAPPED_VA] {
        // 9. rt_sigaction (act and oldact)
        check_syscall(&mut c, general::__NR_rt_sigaction, 15, bad, 0, 8, efault);
        check_syscall(&mut c, general::__NR_rt_sigaction, 15, 0, bad, 8, efault);
        // 10. rt_sigprocmask (set and oldset)
        check_syscall(&mut c, general::__NR_rt_sigprocmask, u64::from(general::SIG_SETMASK), bad, 0, 8, efault);
        check_syscall(&mut c, general::__NR_rt_sigprocmask, u64::from(general::SIG_SETMASK), 0, bad, 8, efault);
        // 11. prlimit64 (new and old)
        check_syscall(&mut c, general::__NR_prlimit64, 0, u64::from(general::RLIMIT_STACK), bad, 0, efault);
        check_syscall(&mut c, general::__NR_prlimit64, 0, u64::from(general::RLIMIT_STACK), 0, bad, efault);
    }

    // Valid iov array with bad iov_base (both unmapped user page and kernel address).
    check_syscall(&mut c, general::__NR_readv, 0, iov_bad_unmapped, 1, 0, efault);
    check_syscall(&mut c, general::__NR_readv, 0, iov_bad_kernel, 1, 0, efault);
    check_syscall(&mut c, general::__NR_writev, 1, iov_bad_unmapped, 1, 0, efault);
    check_syscall(&mut c, general::__NR_writev, 1, iov_bad_kernel, 1, 0, efault);

    // Zero-length iovec with null iov_base must be skipped cleanly.
    check_syscall(&mut c, general::__NR_writev, 1, iov_skip_zero, 2, 0, 4);
    check_syscall(&mut c, general::__NR_readv, 0, iov_skip_zero, 2, 0, 4);

    // Unknown syscall returns -ENOSYS (-38).
    check_syscall(&mut c, 999, 0, 0, 0, 0, enosys);

    // Invalid argument checks.
    check_syscall(&mut c, general::__NR_getcwd, valid_out, 1, 0, 0, erange);
    check_syscall(&mut c, general::__NR_getcwd, valid_out, 64, 0, 0, 2);
    check_syscall(&mut c, general::__NR_ioctl, 1, 0x9999, valid_out, 0, enotty);
    check_syscall(&mut c, general::__NR_clock_gettime, 99, valid_out, 0, 0, einval);
    check_syscall(&mut c, general::__NR_clock_gettime, u64::from(general::CLOCK_REALTIME), valid_out, 0, 0, 0);
    check_syscall(&mut c, general::__NR_getrandom, valid_out, 8, 0x8000, 0, einval);
    check_syscall(&mut c, general::__NR_getrandom, valid_out, 8, 0, 0, 8);
    check_syscall(&mut c, general::__NR_prlimit64, 999_999, u64::from(general::RLIMIT_STACK), 0, valid_out, esrch);
    check_syscall(&mut c, general::__NR_prlimit64, 0, u64::from(general::RLIM_NLIMITS), 0, valid_out, einval);
    check_syscall(&mut c, general::__NR_prlimit64, 0, u64::from(general::RLIMIT_STACK), 0, valid_out, 0);
    check_syscall(&mut c, general::__NR_rt_sigaction, u64::from(general::SIGKILL), valid_out, 0, 8, einval);

    // Closed-fd checks: close(2) -> 0, then write/fstat/ioctl/read/close(2) -> -EBADF.
    check_syscall(&mut c, general::__NR_close, 2, 0, 0, 0, 0);
    check_syscall(&mut c, general::__NR_close, 2, 0, 0, 0, ebadf);
    check_syscall(&mut c, general::__NR_write, 2, valid_buf, 4, 0, ebadf);
    check_syscall(&mut c, general::__NR_fstat, 2, valid_out, 0, 0, ebadf);
    check_syscall(&mut c, general::__NR_ioctl, 2, u64::from(ioctl::TCGETS), valid_out, 0, ebadf);
    check_syscall(&mut c, general::__NR_read, 2, valid_out, 4, 0, ebadf);

    // close(0) -> 0, then read/readv(0) -> -EBADF.
    check_syscall(&mut c, general::__NR_close, 0, 0, 0, 0, 0);
    check_syscall(&mut c, general::__NR_read, 0, valid_out, 4, 0, ebadf);
    check_syscall(&mut c, general::__NR_readv, 0, iov_skip_zero, 2, 0, ebadf);

    c.exit_group_imm(0);
    c.ud2();
    (c.0, data)
}

fn run_bad_ptrs_test(ctx: &mut Ctx<'_>) {
    let (code, data) = program_bad_ptrs();
    let _warm: Vec<u8> = vec![0u8; 16 * 1024];
    drop(_warm);

    let frames_before = frame::free_frames();
    // Pre-seed stdin so read/readv bad-pointer cases attempt copy_to_user and
    // exercise .lumen_fixup even on unmapped user pages.
    let result = match run_ring3_program(&code, &data, None, vec![b'A'; 64]) {
        Ok(r) => r,
        Err(e) => return ctx.fail(format_args!("usertest: bad_ptrs {e}")),
    };
    let exit = result.exit;
    drop(result);
    let frames_after = frame::free_frames();
    let leaked_frames = frames_before as i64 - frames_after as i64;
    match exit {
        UserExit::Exited(status) => {
            let _ = writeln!(ctx.out, "usertest: bad_ptrs exit={status} leaked_frames={leaked_frames}");
        }
        other => {
            ctx.fail(format_args!("usertest: bad_ptrs unexpected exit {other:?} leaked_frames={leaked_frames}"));
        }
    }
}

/// Shell command: `usertest <ok|iret|pf|ud|gp|bp|db|cve_db|lstar_db|movss_bp|stub_db|noncanon|noncanon-iret|bad_elf|mmap_brk|sse_tls|bad_ptrs>`.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: usertest <ok|iret|pf|ud|gp|bp|db|cve_db|lstar_db|movss_bp|stub_db|noncanon|noncanon-iret|bad_elf|mmap_brk|sse_tls|bad_ptrs>";
    let [kind] = args else {
        return ctx.fail(USAGE);
    };
    match kind.as_str() {
        "bad_elf" => return run_bad_elf_tests(ctx),
        "mmap_brk" => return run_mmap_brk_test(ctx),
        "sse_tls" => return run_sse_tls_test(ctx),
        "bad_ptrs" => return run_bad_ptrs_test(ctx),
        _ => {}
    }
    let (code, data, dr0_target): (Vec<u8>, &[u8], Option<u64>) = match kind.as_str() {
        "ok" => {
            let (code, data) = program_ok();
            (code, data, None)
        }
        "iret" => (program_iret(), &[], None),
        "pf" => (program_pf(), &[], None),
        "ud" => (program_ud(), &[], None),
        "gp" => (program_gp(), &[], None),
        "bp" => (program_bp(), &[], None),
        "db" => (program_db(), &[], None),
        "cve_db" => (program_cve_db(), &[], None),
        "lstar_db" => (program_cve_db(), &[], Some(arch::syscall::entry_addr())),
        "movss_bp" => (program_movss_bp(), &[], None),
        "stub_db" => (program_movss_bp(), &[], Some(arch::idt::stub_entry_addr(3))),
        "noncanon" => (program_noncanon(false), &[], None),
        "noncanon-iret" => (program_noncanon(true), &[], None),
        _ => return ctx.fail(USAGE),
    };
    let result = match run_ring3_program(&code, data, dr0_target, Vec::new()) {
        Ok(r) => r,
        Err(e) => return ctx.fail(format_args!("usertest: {e}")),
    };
    match result.exit {
        UserExit::Exited(status) => {
            let text = core::str::from_utf8(&result.stdout).unwrap_or("<non-utf8>").trim_end_matches('\n');
            let _ = writeln!(ctx.out, "usertest: exit={status} stdout=\"{text}\"");
        }
        UserExit::Fault { vector, error_code, rip, cr2 } => {
            let _ = writeln!(
                ctx.out,
                "usertest: fault vector={vector} ({}) error={error_code:#x} rip={rip:#x} cr2={cr2:#x} deferred_lstar={} deferred_stub={}",
                arch::idt::exception_name(vector),
                result.deferred_lstar_db,
                result.deferred_stub_db
            );
        }
        UserExit::NonCanonicalRip(rip) => {
            let _ = writeln!(ctx.out, "usertest: non-canonical rip={rip:#x}");
        }
    }
}

crate::ktest!(
    fn bad_user_pointer_returns_efault() {
        use crate::{kassert, kassert_eq};

        // Sanity: .lumen_fixup has entries for both user-copy assembly routines.
        let fixup_start = &raw const __lumen_fixup_start as usize;
        let fixup_end = &raw const __lumen_fixup_end as usize;
        kassert!(fixup_end > fixup_start);
        kassert_eq!((fixup_end - fixup_start) % core::mem::size_of::<FixupEntry>(), 0);

        let mut buf = [0u8; 16];

        // 1. len == 0 returns Ok(()) even with null or kernel pointers (POSIX).
        kassert_eq!(copy_from_user(&mut [], 0), Ok(()));
        kassert_eq!(copy_to_user(0, &[]), Ok(()));
        kassert_eq!(copy_from_user(&mut [], 0xffff_ffff_8000_0000), Ok(()));
        kassert_eq!(copy_to_user(0xffff_ffff_8000_0000, &[]), Ok(()));

        // 2. Bounds rejections before touching memory: null, < USER_MIN,
        //    >= USER_MAX, kernel half, straddling USER_MAX, u64 wraparound.
        let bad_addrs = [
            0u64,
            0x1000,
            USER_MIN - 1,
            USER_MAX,
            USER_MAX - 4,
            0x0000_8000_0000_0000,
            0xffff_8000_0000_0000,
            0xffff_ffff_8000_0000,
            u64::MAX - 4,
            u64::MAX,
        ];
        for addr in bad_addrs {
            kassert_eq!(copy_from_user(&mut buf, addr), Err(CopyError::Fault));
            kassert_eq!(copy_to_user(addr, &buf), Err(CopyError::Fault));
        }
        kassert_eq!(copy_cstr_from_user(0, 16), Err(CopyError::Fault));
        kassert_eq!(copy_cstr_from_user(USER_MIN - 1, 16), Err(CopyError::Fault));
        kassert_eq!(copy_cstr_from_user(USER_MAX, 16), Err(CopyError::Fault));
        kassert_eq!(copy_cstr_from_user(0xffff_ffff_8000_0000, 16), Err(CopyError::Fault));

        // 3. Live user page table tests exercising `.lumen_fixup` on real `#PF`s
        //    through a real `AddressSpace` + `UserSpaceGuard`.
        let _warm: Vec<u8> = vec![0u8; 4096];
        drop(_warm);
        let free_before = frame::free_frames();
        {
            let space = Arc::new(AddressSpace::new().map_err(|_| "out of frames for AddressSpace")?);
            let rw_va = TEST_DATA_VA;
            let unmapped_va = TEST_DATA_VA + FRAME_SIZE;
            let ro_va = TEST_DATA_VA + 2 * FRAME_SIZE;

            space.map_eager_with_bytes(rw_va, FRAME_SIZE, Prot::READ | Prot::WRITE, &[]).map_err(|_| "map USER_RW")?;
            let mut ro_page = vec![0u8; FRAME_SIZE as usize];
            ro_page[..12].copy_from_slice(b"readonly-ok\0");
            // Place a NUL-terminated string right at the end of the RO page
            // (next page is unmapped) and an unterminated string right after.
            ro_page[FRAME_SIZE as usize - 5..].copy_from_slice(b"edge\0");
            space.map_eager_with_bytes(ro_va, FRAME_SIZE, Prot::READ, &ro_page).map_err(|_| "map USER_RO")?;
            drop(ro_page);

            let _active = sched::UserSpaceGuard::enter(space.clone());

            // Valid RW copy_to_user + copy_from_user round-trip.
            let payload = b"hello-lumen-uaccess";
            kassert_eq!(copy_to_user(rw_va, payload), Ok(()));
            let mut read_back = [0u8; 19];
            kassert_eq!(copy_from_user(&mut read_back, rw_va), Ok(()));
            kassert_eq!(&read_back, payload);

            // Unmapped user page -> `#PF` caught by `.lumen_fixup` -> Fault (EFAULT).
            kassert_eq!(copy_from_user(&mut buf, unmapped_va), Err(CopyError::Fault));
            kassert_eq!(copy_to_user(unmapped_va, &buf), Err(CopyError::Fault));

            // Straddling mapped RW page into unmapped page -> Fault via `.lumen_fixup`.
            let straddle = rw_va + FRAME_SIZE - 4;
            kassert_eq!(copy_from_user(&mut buf, straddle), Err(CopyError::Fault));
            kassert_eq!(copy_to_user(straddle, &buf), Err(CopyError::Fault));

            // Read-only user page: read succeeds, write faults via CR0.WP + `.lumen_fixup`.
            let mut ro_read = [0u8; 11];
            kassert_eq!(copy_from_user(&mut ro_read, ro_va), Ok(()));
            kassert_eq!(&ro_read, b"readonly-ok");
            kassert_eq!(copy_to_user(ro_va, b"nope"), Err(CopyError::Fault));

            // copy_cstr_from_user: valid, empty, page-edge, multi-chunk (> 256 bytes), TooLong, unmapped fault.
            kassert_eq!(copy_cstr_from_user(ro_va, 64), Ok(b"readonly-ok".to_vec()));
            kassert_eq!(copy_cstr_from_user(ro_va + 11, 0), Ok(Vec::new()));
            kassert_eq!(copy_cstr_from_user(ro_va + FRAME_SIZE - 5, 64), Ok(b"edge".to_vec()));
            kassert_eq!(copy_cstr_from_user(ro_va, 4), Err(CopyError::TooLong));
            kassert_eq!(copy_cstr_from_user(unmapped_va, 64), Err(CopyError::Fault));
            // Multi-chunk (> 256-byte internal buffer) C string round-trip and TooLong boundary.
            let mut long_src = [b'x'; 601];
            for (i, slot) in long_src[..600].iter_mut().enumerate() {
                *slot = b'a' + (i % 26) as u8;
            }
            long_src[600] = 0;
            kassert_eq!(copy_to_user(rw_va, &long_src), Ok(()));
            kassert_eq!(copy_cstr_from_user(rw_va, 600), Ok(long_src[..600].to_vec()));
            kassert_eq!(copy_cstr_from_user(rw_va, 599), Err(CopyError::TooLong));
            // Unterminated bytes running off the RW page into the unmapped page.
            kassert_eq!(copy_to_user(rw_va + FRAME_SIZE - 4, b"ABCD"), Ok(()));
            kassert_eq!(copy_cstr_from_user(rw_va + FRAME_SIZE - 4, 64), Err(CopyError::Fault));

            drop(_active);
            space.teardown();
        }

        // Kernel root restored and zero leaked frames/tables.
        kassert_eq!(arch::paging::active_root(), paging::kernel_root());
        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);
