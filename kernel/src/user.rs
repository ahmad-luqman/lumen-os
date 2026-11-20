//! Ring-3 user-mode support: fault-safe user memory copies (`.lumen_fixup`),
//! user exception / syscall exit handling, and the M6 ring-3 test runner.
//!
//! See `docs/ARCHITECTURE.md` §5 and §6 ("Ring 3 entry, `syscall`/`sysretq`,
//! user CPU state and safe copies").

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, Ordering};

use log::warn;
use lumen_mm::paging::Attrs;
use lumen_mm::{FRAME_SIZE, PhysFrame, VirtAddr};

use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::arch::{self, TrapFrame};
use crate::linux::{errno, general};
use crate::mm::{self, frame, paging};
use crate::shell::Ctx;
use crate::sync::SpinLock;

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

/// Active ring-3 test-runner slot on one CPU.
struct RunSlot {
    pid: u32,
    exit: Option<UserExit>,
    stdout: Vec<u8>,
}

static ACTIVE_RUNS: [SpinLock<Option<RunSlot>>; arch::MAX_CPUS] = [const { SpinLock::new(None) }; arch::MAX_CPUS];
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

// Test-only syscall numbers used by `usertest` to exercise return-path
// validation (canonical check on `sysretq`/`iretq` and rewritten-frame `iretq`).
const SYS_TEST_NONCANON_SYSRET: u64 = 0x1000_0001;
const SYS_TEST_NONCANON_IRET: u64 = 0x1000_0002;
const SYS_TEST_IRET_RETURN: u64 = 0x1000_0003;

/// Record `exit` for the active ring-3 context on this CPU and return its pid.
fn record_exit(exit: UserExit) -> u32 {
    let cpu = arch::cpu_index();
    let mut guard = ACTIVE_RUNS[cpu].lock();
    let slot = guard.as_mut().expect("user exit with no active ring-3 run");
    slot.exit = Some(exit);
    slot.pid
}

/// Handle a ring-3 exception (`(frame.cs & 3) != 0`) from `lumen_exception_dispatch`.
///
/// Logs one `WARN` naming pid, vector, `rip`, and `cr2`, and unwinds cleanly
/// back to the kernel ring-3 runner—never calling `crash::exception`.
pub fn handle_user_exception(frame: &TrapFrame, cr2: u64) -> ! {
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
    let pid = record_exit(UserExit::NonCanonicalRip(rip));
    warn!(target: "user", "pid {pid}: non-canonical user return rip {rip:#x}; terminating process");
    // SAFETY: called on the CPU running the active ring-3 context, no lock held.
    unsafe { arch::syscall::unwind_to_kernel() }
}

/// Minimal M6 wave-2 syscall dispatcher for ring-3 validation.
pub fn dispatch_syscall(frame: &mut SyscallFrame) -> SyscallReturn {
    let nr = frame.rax;
    match nr {
        n if n == u64::from(general::__NR_write) => {
            let fd = frame.rdi;
            let buf = frame.rsi;
            let count = frame.rdx;
            if fd != 1 && fd != 2 {
                frame.rax = -(errno::EBADF as i64) as u64;
                return SyscallReturn::Sysret;
            }
            let n = count.min(4096) as usize;
            let mut kbuf = [0u8; 4096];
            if copy_from_user(&mut kbuf[..n], buf).is_err() {
                frame.rax = -(errno::EFAULT as i64) as u64;
                return SyscallReturn::Sysret;
            }
            let cpu = arch::cpu_index();
            if let Some(slot) = ACTIVE_RUNS[cpu].lock().as_mut() {
                let room = 4096usize.saturating_sub(slot.stdout.len());
                slot.stdout.extend_from_slice(&kbuf[..n.min(room)]);
            }
            frame.rax = n as u64;
            SyscallReturn::Sysret
        }
        n if n == u64::from(general::__NR_exit) || n == u64::from(general::__NR_exit_group) => {
            let status = (frame.rdi & 0xff) as i32;
            let _ = record_exit(UserExit::Exited(status));
            // SAFETY: unwinds cleanly back to `enter_ring3`, no lock held.
            unsafe { arch::syscall::unwind_to_kernel() }
        }
        SYS_TEST_NONCANON_SYSRET => {
            // Non-canonical boundary address: bit 47 = 1, bits 48..63 = 0.
            frame.rip = 0x0000_8000_0000_0000;
            SyscallReturn::Sysret
        }
        SYS_TEST_NONCANON_IRET => {
            frame.rip = 0x0000_8000_0000_0000;
            SyscallReturn::Iret
        }
        SYS_TEST_IRET_RETURN => {
            frame.rax = 0x1337;
            SyscallReturn::Iret
        }
        _ => {
            frame.rax = -(errno::ENOSYS as i64) as u64;
            SyscallReturn::Sysret
        }
    }
}

/// RAII temporary user address space (lower half `L4[0..256]` + shared kernel
/// upper half `L4[256..512]`). Dropping always switches `CR3` back to
/// `kernel_root()` and frees all user leaf and page-table frames.
///
/// Wave-2 preemption note: until #237 wires per-thread `CR3` switching into
/// `sched::schedule`, timer preemption while `TempUserSpace` is active may run
/// kernel threads under this `CR3`. Because `L4[256..512]` shares all kernel
/// mappings (including the 6 pre-allocated dynamic L3 slots for heap, stacks,
/// and MMIO) and no other thread enters ring 3 concurrently on this CPU, that
/// is sound for the M6 wave-2 test runner.
struct TempUserSpace {
    root: PhysFrame,
    active: bool,
}

impl TempUserSpace {
    fn new() -> Option<Self> {
        let root = paging::new_user_root()?;
        Some(Self { root, active: false })
    }

    fn map_page(&mut self, virt: u64, attrs: Attrs) -> Option<PhysFrame> {
        let va = VirtAddr::new(virt)?;
        let phys = frame::alloc_zeroed()?;
        // SAFETY: `self.root` was allocated by `new_user_root` (distinct from
        // `kernel_root()`), `self` has exclusive access to its lower half, and
        // `phys` is a freshly allocated frame owned by `self` until teardown.
        if unsafe { paging::map_user_page(self.root, va, phys, attrs) }.is_err() {
            // SAFETY: freshly allocated frame, not mapped anywhere.
            unsafe { frame::free(phys) }.expect("freeing unmapped user frame");
            return None;
        }
        Some(phys)
    }

    fn activate(&mut self) {
        if !self.active {
            // SAFETY: `self.root` copies the kernel upper half `L4[256..512]`
            // identically from `kernel_root()`.
            unsafe { arch::paging::write_cr3(self.root.as_u64()) };
            self.active = true;
        }
    }

    fn deactivate(&mut self) {
        if self.active {
            // SAFETY: restoring the kernel's own page-table root.
            unsafe { arch::paging::write_cr3(paging::kernel_root().as_u64()) };
            self.active = false;
        }
    }
}

impl Drop for TempUserSpace {
    fn drop(&mut self) {
        self.deactivate();
        // SAFETY: `self.root` is no longer active in `CR3`, and every mapped
        // lower-half leaf and table belongs exclusively to `self`.
        unsafe { paging::destroy_user_root(self.root) };
    }
}

/// Write `bytes` into physical frame `frame` at `offset` through the HHDM.
fn write_frame_bytes(frame: PhysFrame, offset: usize, bytes: &[u8]) {
    assert!(offset + bytes.len() <= FRAME_SIZE as usize);
    let ptr = mm::phys_to_virt(frame.start()).as_mut_ptr::<u8>();
    // SAFETY: `frame` is a newly allocated usable RAM frame mapped read-write
    // through the HHDM, and `offset + bytes.len() <= FRAME_SIZE`.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), bytes.len()) };
}

/// Virtual layout for temporary ring-3 test programs.
const TEST_CODE_VA: u64 = 0x0000_0000_0040_0000;
const TEST_STACK_VA: u64 = 0x0000_0000_0050_0000;
const TEST_DATA_VA: u64 = 0x0000_0000_0060_0000;

/// Result of running one temporary ring-3 test program.
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

/// Run raw x86-64 user machine code `code` (with optional `data` mapped at
/// `TEST_DATA_VA`) in ring 3 inside a fresh temporary address space.
///
/// When `dr0_target` is `Some(addr)`, arms a hardware execution breakpoint
/// (`DR0 = addr`) before entering ring 3 and disarms it on return,
/// deterministically exercising the `IST_DEBUG` pre-`swapgs` `#DB` handoff at
/// `LSTAR` or an IDT exception stub under both QEMU TCG and KVM.
fn run_ring3_program(code: &[u8], data: &[u8], dr0_target: Option<u64>) -> Result<RunResult, &'static str> {
    let mut space = TempUserSpace::new().ok_or("out of frames for user page table")?;
    let code_frame = space.map_page(TEST_CODE_VA, Attrs::USER_RX).ok_or("out of frames for user code")?;
    let _stack_frame = space.map_page(TEST_STACK_VA, Attrs::USER_RW).ok_or("out of frames for user stack")?;
    let data_frame = space.map_page(TEST_DATA_VA, Attrs::USER_RW).ok_or("out of frames for user data")?;

    write_frame_bytes(code_frame, 0, code);
    write_frame_bytes(data_frame, 0, data);

    let cpu = arch::cpu_index();
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = ACTIVE_RUNS[cpu].lock();
        if guard.is_some() {
            return Err("ring-3 runner already active on this CPU");
        }
        *guard = Some(RunSlot { pid, exit: None, stdout: Vec::new() });
    }

    let lstar_before = arch::idt::DEFERRED_LSTAR_DB.load(Ordering::Relaxed);
    let stub_before = arch::idt::DEFERRED_STUB_DB.load(Ordering::Relaxed);
    space.activate();
    let _dr0 = Dr0Guard::arm(dr0_target);
    // SAFETY: `space` maps `TEST_CODE_VA` (USER_RX), `TEST_STACK_VA` (USER_RW),
    // and the full kernel upper half; `enter_ring3` checks interrupts/locks.
    unsafe { arch::syscall::enter_ring3(TEST_CODE_VA, TEST_STACK_VA + FRAME_SIZE, 0x0202) };
    drop(_dr0);
    drop(space);
    let lstar_after = arch::idt::DEFERRED_LSTAR_DB.load(Ordering::Relaxed);
    let stub_after = arch::idt::DEFERRED_STUB_DB.load(Ordering::Relaxed);

    let slot = ACTIVE_RUNS[cpu].lock().take().expect("active run slot missing after ring-3 exit");
    let exit = slot.exit.expect("ring-3 exited without recording UserExit");
    Ok(RunResult {
        exit,
        stdout: slot.stdout,
        deferred_lstar_db: lstar_after.saturating_sub(lstar_before),
        deferred_stub_db: stub_after.saturating_sub(stub_before),
    })
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

/// Shell command: `usertest <ok|iret|pf|ud|gp|bp|db|cve_db|lstar_db|movss_bp|stub_db|noncanon|noncanon-iret>`.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str =
        "usage: usertest <ok|iret|pf|ud|gp|bp|db|cve_db|lstar_db|movss_bp|stub_db|noncanon|noncanon-iret>";
    let [kind] = args else {
        return ctx.fail(USAGE);
    };
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
    let result = match run_ring3_program(&code, data, dr0_target) {
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

        // 3. Live user page table tests exercising `.lumen_fixup` on real `#PF`s.
        let free_before = frame::free_frames();
        {
            let mut space = TempUserSpace::new().ok_or("out of frames for TempUserSpace")?;
            let rw_va = TEST_DATA_VA;
            let unmapped_va = TEST_DATA_VA + FRAME_SIZE;
            let ro_va = TEST_DATA_VA + 2 * FRAME_SIZE;

            let _rw_frame = space.map_page(rw_va, Attrs::USER_RW).ok_or("map USER_RW")?;
            let ro_frame = space.map_page(ro_va, Attrs::USER_RO).ok_or("map USER_RO")?;
            write_frame_bytes(ro_frame, 0, b"readonly-ok\0");
            // Place a NUL-terminated string right at the end of the RO page
            // (next page is unmapped) and an unterminated string right after.
            write_frame_bytes(ro_frame, FRAME_SIZE as usize - 5, b"edge\0");

            space.activate();

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
        }

        // Kernel root restored and zero leaked frames/tables.
        kassert_eq!(arch::paging::active_root(), paging::kernel_root());
        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);
