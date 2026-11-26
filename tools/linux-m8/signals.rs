//! Ring-3 signal checks, including handler recovery, user-edited frames,
//! send errors and asynchronous delivery without syscalls.
use crate::common::{self, R, ensure, eq, ok, sys};
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};

const USR1: u64 = 10;
const SEGV: u64 = 11;
const INFO: u64 = 4;
const RESTORER: u64 = 0x04000000;
const ONSTACK: u64 = 0x08000000;
const RESTART: u64 = 0x10000000;
const NODEFER: u64 = 0x40000000;
const RESETHAND: u64 = 0x80000000;

const NORMAL: u64 = 0;
const NESTED: u64 = 2;
const FAULT: u64 = 3;
const FAILED_FRAME: u64 = 4;
const BAD_MXCSR: u64 = 10;
const BAD_XSTATE: u64 = 11;
const BAD_XCOMP: u64 = 12;
const BAD_CS: u64 = 13;
const BAD_RIP: u64 = 14;
const FX_FALLBACK: u64 = 15;
const BAD_SS: u64 = 16;
const RESTART_RAX: u64 = 17;
const WAIT: u64 = 18;
const RESET_HAND: u64 = 19;
const UC_MCONTEXT: usize = 40;
const MC_RAX: usize = 104;
const MC_RSP: usize = 120;
const MC_RIP: usize = 128;
const MC_CS: usize = 144;
const MC_SS: usize = 150;
const MC_ERR: usize = 152;
const MC_TRAPNO: usize = 160;
const MC_CR2: usize = 176;
const MC_FPSTATE: usize = 184;

static MODE: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);
static SIGNO: AtomicU64 = AtomicU64::new(0);
static CODE: AtomicU64 = AtomicU64::new(0);
static ADDR: AtomicU64 = AtomicU64::new(0);
static MASK: AtomicU64 = AtomicU64::new(0);
static HANDLER_SP: AtomicU64 = AtomicU64::new(0);
static STACK_STATUS: AtomicU64 = AtomicU64::new(0);
static STACK_CHANGE: AtomicU64 = AtomicU64::new(0);
static SAVED_RIP: AtomicU64 = AtomicU64::new(0);
static ERR: AtomicU64 = AtomicU64::new(0);
static CR2: AtomicU64 = AtomicU64::new(0);
static TRAP: AtomicU64 = AtomicU64::new(0);
static VALUE: AtomicU64 = AtomicU64::new(0);
static RESET_ACTION: AtomicU64 = AtomicU64::new(u64::MAX);
#[unsafe(no_mangle)]
static C5_RESUME: AtomicU64 = AtomicU64::new(0);
#[unsafe(no_mangle)]
static C5_STACK: AtomicU64 = AtomicU64::new(0);
#[unsafe(no_mangle)]
static C5_MXCSR: AtomicU64 = AtomicU64::new(0);

// SAFETY: these C ABI functions are implemented below or provided by musl.
unsafe extern "C" {
    fn c5_restorer();
    fn c5_de();
    fn c5_ud();
    fn c5_bp();
    fn c5_gp();
    fn c5_ss();
    fn c5_xm();
    fn c5_db();
    fn c5_load(addr: u64);
    fn c5_bad_stack(tid: u64);
    fn c5_fp(tid: u64, output: *mut u8, avx: u64);
    fn raise(sig: i32) -> i32;
}

global_asm!(
    r#"
.global c5_restorer
c5_restorer:
    mov rax, 15
    syscall
    ud2
.macro fault_start name
.global \name
\name:
    lea rax, [rip + 9f]
    mov [rip + C5_RESUME], rax
    mov [rip + C5_STACK], rsp
.endm
fault_start c5_de
    xor edx, edx
    mov eax, 1
    xor ecx, ecx
    div rcx
9: ret
fault_start c5_ud
    ud2
9: ret
fault_start c5_bp
    int3
9: ret
fault_start c5_gp
    hlt
9: ret
fault_start c5_ss
    mov rsp, 0x8000000000000000
    push rax
9: ret
fault_start c5_load
    mov al, [rdi]
9: ret
fault_start c5_xm
    ldmxcsr [rip + c5_unmask]
    pxor xmm0, xmm0
    movss xmm1, [rip + c5_one]
    divss xmm1, xmm0
9:  stmxcsr [rip + C5_MXCSR]
    ldmxcsr [rip + c5_default]
    ret
fault_start c5_db
    pushfq
    or qword ptr [rsp], 0x100
    popfq
    nop
1:  jmp 1b
9:  ret
fault_start c5_bad_stack
    mov rsp, 0
    mov esi, 10
    mov eax, 200
    syscall
9: ret
.global c5_fp
c5_fp:
    push rbx
    push r12
    mov rbx, rsi
    mov r12, rdx
    movdqu xmm0, [rip + c5_pattern]
    test r12, r12
    jz 1f
    vmovdqu ymm0, [rip + c5_pattern]
1:  ldmxcsr [rip + c5_round]
    mov eax, 200
    mov esi, 10
    syscall
    movdqu [rbx], xmm0
    stmxcsr [rbx + 32]
    test r12, r12
    jz 2f
    vmovdqu [rbx], ymm0
2:  ldmxcsr [rip + c5_default]
    pop r12
    pop rbx
    ret
.section .rodata
.balign 32
c5_pattern: .quad 0x1122334455667788, 0x99aabbccddeeff00, 0x123456789abcdef0, 0xfedcba9876543210
c5_unmask: .long 0x1d80
c5_default: .long 0x1f80
c5_round: .long 0x3f80
c5_one: .long 0x3f800000
.text
"#
);

// SAFETY: the kernel calls with readable siginfo and a writable Linux x86-64
// ucontext on this thread's stack. Modes alter only fields owned by that frame.
extern "C" fn handler(sig: u64, info: *const u8, uc: *mut u8) {
    let count = COUNT.fetch_add(1, SeqCst);
    let mut sp: u64;
    // SAFETY: reading rsp changes no memory or machine state.
    unsafe {
        asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    HANDLER_SP.store(sp, SeqCst);
    SIGNO.store(sig, SeqCst);
    // SAFETY: SA_SIGINFO provides 128 readable bytes, offsets are Linux ABI.
    unsafe {
        CODE.store(core::ptr::read_unaligned(info.add(8).cast::<i32>()) as i64 as u64, SeqCst);
        ADDR.store(core::ptr::read_unaligned(info.add(16).cast::<u64>()), SeqCst);
        VALUE.store(core::ptr::read_unaligned(info.add(24).cast::<u64>()), SeqCst);
    }
    let mut mask = 0u64;
    let _ = sys(14, [0, 0, &mut mask as *mut _ as u64, 8, 0, 0]);
    MASK.store(mask, SeqCst);
    let mut old = [0u64; 3];
    let _ = sys(131, [0, old.as_mut_ptr() as u64, 0, 0, 0, 0]);
    STACK_STATUS.store(old[1], SeqCst);
    let disable = [0u64, 2, 0];
    STACK_CHANGE.store(sys(131, [disable.as_ptr() as u64, 0, 0, 0, 0, 0]) as u64, SeqCst);
    let mode = MODE.load(SeqCst);
    if mode == WAIT {
        say!("signal-wait: handler");
    }
    if mode == RESET_HAND {
        let mut old = [0u64; 4];
        let _ = sys(13, [USR1, 0, old.as_mut_ptr() as u64, 8, 0, 0]);
        RESET_ACTION.store(old[0], SeqCst);
    }
    if mode == NESTED && count == 0 {
        let _ = sys(200, [tid(), USR1, 0, 0, 0, 0]);
    }
    // SAFETY: all offsets are within sigcontext or its advertised FP area.
    // Corruptions deliberately exercise sigreturn; no corrupt value is used
    // as a pointer by this program.
    unsafe {
        let mc = uc.add(UC_MCONTEXT);
        TRAP.store(core::ptr::read_unaligned(mc.add(MC_TRAPNO).cast::<u64>()), SeqCst);
        SAVED_RIP.store(core::ptr::read_unaligned(mc.add(MC_RIP).cast::<u64>()), SeqCst);
        ERR.store(core::ptr::read_unaligned(mc.add(MC_ERR).cast::<u64>()), SeqCst);
        CR2.store(core::ptr::read_unaligned(mc.add(MC_CR2).cast::<u64>()), SeqCst);
        let fp = core::ptr::read_unaligned(mc.add(MC_FPSTATE).cast::<u64>()) as *mut u8;
        match mode {
            FAULT | FAILED_FRAME => {
                if mode == FAILED_FRAME && sig != SEGV {
                    common::_exit(91);
                }
                core::ptr::write_unaligned(mc.add(MC_RIP).cast::<u64>(), C5_RESUME.load(SeqCst));
                core::ptr::write_unaligned(mc.add(MC_RSP).cast::<u64>(), C5_STACK.load(SeqCst));
            }
            BAD_MXCSR => core::ptr::write_unaligned(fp.add(24).cast::<u32>(), 0x80000000),
            BAD_XSTATE => core::ptr::write_unaligned(fp.add(512).cast::<u64>(), 1 << 63),
            BAD_XCOMP => core::ptr::write_unaligned(fp.add(520).cast::<u64>(), 1 << 63),
            BAD_CS => core::ptr::write_unaligned(mc.add(MC_CS).cast::<u16>(), 8),
            BAD_RIP => core::ptr::write_unaligned(mc.add(MC_RIP).cast::<u64>(), 1 << 63),
            FX_FALLBACK => core::ptr::write_unaligned(fp.add(464).cast::<u32>(), 0),
            BAD_SS => core::ptr::write_unaligned(mc.add(MC_SS).cast::<u16>(), 0xffff),
            RESTART_RAX => core::ptr::write_unaligned(mc.add(MC_RAX).cast::<u64>(), (-512i64) as u64),
            _ => {}
        }
        // Clobber actual hardware state. Sigreturn must recover the saved
        // legacy/extended image, not the handler's registers.
        asm!("pxor xmm0, xmm0", out("xmm0") _, options(nostack));
    }
}

fn tid() -> u64 {
    sys(186, [0; 6]) as u64
}
fn action(sig: u64, flags: u64, mask: u64) -> R {
    let a = [handler as *const () as u64, flags | RESTORER, c5_restorer as *const () as u64, mask];
    eq("sigaction", sys(13, [sig, a.as_ptr() as u64, 0, 8, 0, 0]), 0)
}
fn altstack() -> Result<u64, common::Fail> {
    let base = ok("mmap altstack", sys(9, [0, 32768, 3, 0x22, u64::MAX, 0]))? as u64;
    let stack = [base, 0, 32768];
    eq("sigaltstack", sys(131, [stack.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    Ok(base)
}
fn reset(mode: u64) {
    MODE.store(mode, SeqCst);
    COUNT.store(0, SeqCst);
}
fn send(nr: u64, args: [u64; 6]) -> R {
    eq("send", sys(nr, args), 0)
}
fn avx() -> bool {
    // SAFETY: CPUID is available on x86-64; OSXSAVE/AVX are checked together.
    unsafe { core::arch::x86_64::__cpuid(1).ecx & (3 << 27) == (3 << 27) }
}

pub fn signals() -> i32 {
    common::run_checks(&[
        ("senders_and_masks", &|| {
            reset(NORMAL);
            action(USR1, INFO, 1 << 11)?;
            for (nr, args, code) in [
                (62, [tid(), USR1, 0, 0, 0, 0], 0),
                (200, [tid(), USR1, 0, 0, 0, 0], -6),
                (234, [tid(), tid(), USR1, 0, 0, 0], -6),
            ] {
                send(nr, args)?;
                eq("si_code", CODE.load(SeqCst) as i64, code)?;
                eq("si_pid", ADDR.load(SeqCst) as i64, tid() as i64)?;
            }
            eq("deliveries", COUNT.load(SeqCst) as i64, 3)?;
            eq("handler mask", MASK.load(SeqCst) as i64, (1 << 9) | (1 << 11))?;
            let mut after = 9u64;
            eq("get mask", sys(14, [0, 0, &mut after as *mut _ as u64, 8, 0, 0]), 0)?;
            eq("restored mask", after as i64, 0)?;
            eq("wrong tgid", sys(234, [tid() + 1, tid(), USR1, 0, 0, 0]), -3)?;
            eq("bad tid", sys(200, [0, USR1, 0, 0, 0, 0]), -22)?;
            eq("missing pid", sys(62, [32767, 0, 0, 0, 0, 0]), -3)?;
            for (nr, args) in
                [(62, [32767, 65, 0, 0, 0, 0]), (200, [32767, 65, 0, 0, 0, 0]), (234, [32767, 32767, 65, 0, 0, 0])]
            {
                eq("missing target before invalid signal", sys(nr, args), -3)?;
            }
            let mut info = [0u64; 16];
            info[1] = (-1i32) as u32 as u64;
            eq("queue missing target before invalid signal", sys(129, [32767, 65, info.as_ptr() as u64, 0, 0, 0]), -3)?;
            eq("queue copy before invalid ids", sys(297, [0, 0, 65, 1, 0, 0]), -14)?;
            eq("queue invalid ids after valid copy", sys(297, [0, 0, 65, info.as_ptr() as u64, 0, 0]), -22)?;
            eq("signal zero", sys(62, [tid(), 0, 0, 0, 0, 0]), 0)
        }),
        ("raise_without_siginfo", &|| {
            reset(NORMAL);
            action(USR1, 0, 0)?;
            // SAFETY: musl raise uses this process's valid signal number.
            eq("raise", unsafe { raise(USR1 as i32) } as i64, 0)?;
            eq("raised handler", COUNT.load(SeqCst) as i64, 1)
        }),
        ("blocked_pending_and_sigqueue", &|| {
            reset(NORMAL);
            action(USR1, INFO, 0)?;
            let mask = 1u64 << 9;
            eq("block", sys(14, [0, &mask as *const _ as u64, 0, 8, 0, 0]), 0)?;
            let mut info = [0u64; 16];
            info[1] = (-1i32) as u32 as u64;
            info[3] = 0xabc;
            send(129, [tid(), 0, info.as_ptr() as u64, 0, 0, 0])?;
            send(297, [tid(), tid(), 0, info.as_ptr() as u64, 0, 0])?;
            eq("signal zero queues nothing", COUNT.load(SeqCst) as i64, 0)?;
            send(129, [tid(), USR1, info.as_ptr() as u64, 0, 0, 0])?;
            eq("blocked", COUNT.load(SeqCst) as i64, 0)?;
            eq("unblock", sys(14, [1, &mask as *const _ as u64, 0, 8, 0, 0]), 0)?;
            eq("delivered on unblock", COUNT.load(SeqCst) as i64, 1)?;
            eq("queued value", VALUE.load(SeqCst) as i64, 0xabc)?;
            send(297, [tid(), tid(), USR1, info.as_ptr() as u64, 0, 0])?;
            eq("thread queue", COUNT.load(SeqCst) as i64, 2)
        }),
        ("nodefer_and_resethand", &|| {
            reset(NESTED);
            action(USR1, INFO | NODEFER, 0)?;
            send(200, [tid(), USR1, 0, 0, 0, 0])?;
            eq("nested handler", COUNT.load(SeqCst) as i64, 2)?;
            eq("nodefer mask", MASK.load(SeqCst) as i64, 0)?;
            reset(RESET_HAND);
            action(USR1, INFO | RESETHAND, 0)?;
            send(200, [tid(), USR1, 0, 0, 0, 0])?;
            eq("reset before handler", RESET_ACTION.load(SeqCst) as i64, 0)?;
            let mut old = [0u64; 4];
            eq("read reset action", sys(13, [USR1, 0, old.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
            eq("reset before return", old[0] as i64, 0)
        }),
        ("alternate_stack", &|| {
            let base = altstack()?;
            reset(NORMAL);
            action(USR1, INFO | ONSTACK, 0)?;
            send(200, [tid(), USR1, 0, 0, 0, 0])?;
            ensure(
                "handler stack",
                (base..base + 32768).contains(&HANDLER_SP.load(SeqCst)),
                HANDLER_SP.load(SeqCst) as i64,
            )?;
            eq("SS_ONSTACK", STACK_STATUS.load(SeqCst) as i64, 1)?;
            eq("onstack change EPERM", STACK_CHANGE.load(SeqCst) as i64, -1)
        }),
        ("fp_and_mxcsr", &|| fp_check(NORMAL)),
        ("sigreturn_ss_sanitized", &|| fp_check(BAD_SS)),
        ("sigreturn_rax_is_not_restart", &|| {
            reset(RESTART_RAX);
            action(USR1, INFO, 0)?;
            eq("restored rax", sys(200, [tid(), USR1, 0, 0, 0, 0]), -512)
        }),
        ("force_sigsegv", &|| {
            let _ = altstack()?;
            reset(FAILED_FRAME);
            action(USR1, INFO, 0)?;
            action(SEGV, INFO | ONSTACK, 0)?;
            // SAFETY: assembly saves a valid recovery stack before setting rsp=0.
            unsafe {
                c5_bad_stack(tid());
            }
            eq("caught failed frame", SIGNO.load(SeqCst) as i64, 11)?;
            eq("one SEGV", COUNT.load(SeqCst) as i64, 1)
        }),
    ])
}
fn fp_check(mode: u64) -> R {
    reset(mode);
    action(USR1, INFO, 0)?;
    let mut output = [0u8; 40];
    // SAFETY: output has room for YMM0 plus MXCSR; AVX is CPUID gated.
    unsafe {
        c5_fp(tid(), output.as_mut_ptr(), u64::from(avx()));
    }
    let expected = [0x1122334455667788u64, 0x99aabbccddeeff00, 0x123456789abcdef0, 0xfedcba9876543210];
    for (i, want) in expected.iter().enumerate().take(if avx() { 4 } else { 2 }) {
        let got = u64::from_le_bytes(output[i * 8..i * 8 + 8].try_into().unwrap());
        eq("FP register", got as i64, if mode == FX_FALLBACK && i >= 2 { 0 } else { *want as i64 })?;
    }
    eq("MXCSR", u32::from_le_bytes(output[32..36].try_into().unwrap()) as i64, 0x3f80)
}

pub fn faults() -> i32 {
    common::run_checks(&[
        ("de", &|| fault(c5_de, 8, 1, None, 0)),
        ("ud", &|| fault(c5_ud, 4, 2, None, 6)),
        ("bp", &|| fault(c5_bp, 5, 128, Some(0), 3)),
        ("gp", &|| fault(c5_gp, 11, 128, Some(0), 13)),
        ("ss", &|| {
            let _ = altstack()?;
            reset(FAULT);
            action(7, INFO | ONSTACK, 0)?;
            action(11, INFO | ONSTACK, 0)?;
            // SAFETY: assembly records the good stack for recovery.
            unsafe {
                c5_ss();
            }
            // QEMU TCG reports #GP for this noncanonical push (qemu.log
            // vector 13). On hardware that raises #SS, require SIGBUS.
            let vector = TRAP.load(SeqCst);
            ensure("stack fault vector", vector == 12 || vector == 13, vector as i64)?;
            if vector == 13 {
                say!("note ss: emulator raised GP");
            }
            check_fault(if vector == 12 { 7 } else { 11 }, 128, Some(0))?;
            metadata(vector, 0, 0)
        }),
        ("xm", &|| {
            reset(FAULT);
            action(8, INFO | ONSTACK, 0)?;
            // SAFETY: the assembly records a recovery point and restores MXCSR.
            unsafe {
                c5_xm();
            }
            if COUNT.load(SeqCst) == 0 {
                // TCG may set the exception status without delivering #XM.
                // Require proof that division by zero occurred unmasked.
                eq("unmasked SIMD divide status", C5_MXCSR.load(SeqCst) as i64, 0x1d84)?;
                say!("note xm: emulator set unmasked divide status without a trap");
                Ok(())
            } else {
                check_fault(8, 3, None)?;
                metadata(19, 0, 0)
            }
        }),
        ("maperr", &|| memory_fault(0x12345000, 11, 1)),
        ("accerr", &|| {
            let p = ok("mmap PROT_NONE", sys(9, [0, 4096, 0, 0x22, u64::MAX, 0]))?;
            memory_fault(p as u64, 11, 2)
        }),
        ("bus", &|| {
            let path = b"/tmp/c5-bus\0";
            let fd = ok("open bus", sys(257, [(-100i64) as u64, path.as_ptr() as u64, 0x242, 0o600, 0, 0]))?;
            eq("truncate", sys(77, [fd as u64, 4096, 0, 0, 0, 0]), 0)?;
            let p = ok("mmap file", sys(9, [0, 8192, 1, 1, fd as u64, 0]))?;
            memory_fault(p as u64 + 4096, 7, 2)
        }),
        ("db", &|| {
            reset(FAULT);
            action(5, INFO, 0)?;
            // SAFETY: clears TF through the kernel's IST handoff; normal stack.
            unsafe {
                c5_db();
            }
            // The trap and the deferred IPI both point at the one-instruction
            // spin loop; the handler resumes at the saved return label.
            eq("debug handler", COUNT.load(SeqCst) as i64, 1)?;
            check_fault(5, 2, None)?;
            metadata(1, 0, 0)
        }),
    ])
}
fn fault(f: unsafe extern "C" fn(), sig: u64, code: i64, addr: Option<u64>, vector: u64) -> R {
    reset(FAULT);
    action(sig, INFO | ONSTACK, 0)?;
    // SAFETY: the assembly records a recovery address and stack for the handler.
    unsafe {
        f();
    }
    check_fault(sig, code, addr)?;
    metadata(vector, 0, 0)
}
fn check_fault(sig: u64, code: i64, addr: Option<u64>) -> R {
    eq("fault handler", COUNT.load(SeqCst) as i64, 1)?;
    eq("fault signal", SIGNO.load(SeqCst) as i64, sig as i64)?;
    eq("fault code", CODE.load(SeqCst) as i64, code)?;
    if let Some(addr) = addr {
        eq("fault address", ADDR.load(SeqCst) as i64, addr as i64)?;
    } else {
        eq("fault instruction address", ADDR.load(SeqCst) as i64, SAVED_RIP.load(SeqCst) as i64)?;
    }
    Ok(())
}
fn memory_fault(addr: u64, sig: u64, code: i64) -> R {
    reset(FAULT);
    action(sig, INFO | ONSTACK, 0)?;
    // SAFETY: deliberately faulting load; handler resumes at the assembly label.
    unsafe {
        c5_load(addr);
    }
    check_fault(sig, code, Some(addr))?;
    metadata(14, 4, addr)
}
fn metadata(vector: u64, error: u64, cr2: u64) -> R {
    eq("REG_TRAPNO", TRAP.load(SeqCst) as i64, vector as i64)?;
    eq("REG_ERR", ERR.load(SeqCst) as i64, error as i64)?;
    eq("REG_CR2", CR2.load(SeqCst) as i64, cr2 as i64)
}

pub fn bad(kind: &[u8]) -> i32 {
    if kind == b"segvframe" {
        // A SEGV frame that cannot be built must reset the SEGV action and
        // terminate, rather than retrying the same unusable handler forever.
        let broken = [handler as *const () as u64, INFO, 0, 0];
        let _ = sys(13, [SEGV, broken.as_ptr() as u64, 0, 8, 0, 0]);
        // SAFETY: deliberate unmapped load; no handler can return from it.
        unsafe {
            c5_load(0x12345000);
        }
        return 99;
    }
    let mode = match kind {
        b"mxcsr" => BAD_MXCSR,
        b"xstate" => BAD_XSTATE,
        b"xcomp" => BAD_XCOMP,
        b"cs" => BAD_CS,
        b"rip" => BAD_RIP,
        b"magic" => FX_FALLBACK,
        _ => return 64,
    };
    if mode == FX_FALLBACK {
        return common::run_checks(&[("fx_fallback", &|| fp_check(FX_FALLBACK))]);
    }
    reset(mode);
    if action(USR1, INFO, 0).is_err() {
        return 90;
    }
    let _ = sys(200, [tid(), USR1, 0, 0, 0, 0]);
    say!("FAIL malformed sigreturn survived");
    99
}
pub fn fatal(kind: &[u8]) -> i32 {
    if action(SEGV, INFO, 0).is_err() {
        return 90;
    }
    if kind == b"blocked" {
        let mask = 1u64 << 10;
        let _ = sys(14, [0, &mask as *const _ as u64, 0, 8, 0, 0]);
    } else {
        let ignore = [1u64, 0, 0, 0];
        let _ = sys(13, [SEGV, ignore.as_ptr() as u64, 0, 8, 0, 0]);
    }
    // SAFETY: intended unmapped load must terminate before returning.
    unsafe {
        c5_load(0x12345000);
    }
    99
}
pub fn hold() -> i32 {
    let fd = match lock_file() {
        Ok(fd) => fd,
        Err(_) => return 90,
    };
    let lock = [1u64, 0, 0, 0]; // F_WRLCK, SEEK_SET, through EOF
    if sys(72, [fd, 6, lock.as_ptr() as u64, 0, 0, 0]) != 0 {
        return 91;
    }
    say!("signal-hold: pid={} ready", tid());
    let mut byte = 0u8;
    let _ = sys(0, [0, &mut byte as *mut _ as u64, 1, 0, 0, 0]);
    say!("signal-hold: done");
    0
}
fn lock_file() -> Result<u64, common::Fail> {
    let path = b"/tmp/c5-restart\0";
    ok("open lock file", sys(257, [(-100i64) as u64, path.as_ptr() as u64, 0x42, 0o600, 0, 0])).map(|fd| fd as u64)
}
pub fn wait(restart: bool) -> i32 {
    common::run_checks(&[(if restart { "sa_restart" } else { "eintr" }, &|| {
        reset(WAIT);
        action(USR1, INFO | if restart { RESTART } else { 0 }, 0)?;
        let fd = lock_file()?;
        let lock = [1u64, 0, 0, 0];
        say!("signal-wait: pid={} ready", tid());
        let result = sys(72, [fd, 7, lock.as_ptr() as u64, 0, 0, 0]);
        eq("interrupted lock result", result, if restart { 0 } else { -4 })?;
        eq("handler ran", COUNT.load(SeqCst) as i64, 1)
    })])
}
pub fn kill(arg: &[u8]) -> i32 {
    let pid = parse_pid(arg);
    common::run_checks(&[("send_usr1", &|| send(62, [pid, USR1, 0, 0, 0, 0]))])
}
pub fn probe(arg: &[u8]) -> i32 {
    let pid = parse_pid(arg);
    common::run_checks(&[("sigqueue_permission", &|| {
        let mut info = [0u64; 16];
        info[1] = 128;
        eq("positive code to another process", sys(129, [pid, USR1, info.as_ptr() as u64, 0, 0, 0]), -1)?;
        info[1] = (-6i32) as u32 as u64;
        eq("SI_TKILL to another process", sys(129, [pid, USR1, info.as_ptr() as u64, 0, 0, 0]), -1)
    })])
}

fn parse_pid(arg: &[u8]) -> u64 {
    arg.iter().fold(0u64, |n, b| n * 10 + u64::from(b - b'0'))
}
pub fn spin() -> i32 {
    common::run_checks(&[("async_delivery", &|| {
        reset(NORMAL);
        action(USR1, INFO, 0)?;
        say!("signal-spin: pid={} ready", tid());
        // No syscalls: only an interrupt return can deliver.
        while COUNT.load(SeqCst) == 0 {
            core::hint::spin_loop();
        }
        eq("handler ran", COUNT.load(SeqCst) as i64, 1)
    })])
}
