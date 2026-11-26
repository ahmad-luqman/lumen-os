//! Signal delivery on syscall, fork-entry and non-IST interrupt returns. Queue transitions
//! are serialized by the process signal lock; frames and user copies happen
//! after releasing it. Restart context is consumed once, never after sigreturn.

use crate::linux::{general, process};
use lumen_signal::restart::{Handling, RESTART_SYSCALL, RestartCode, RestartDecision, SyscallExit, decide};
use lumen_signal::{Signal, pending::SigInfo};

use crate::arch;
use crate::arch::percpu::DebugCode;
use crate::arch::syscall::SyscallFrame;
use crate::linux::errno;
use crate::sched;
use crate::user::{UserFault, vector_signal};

pub mod frame;

/// What `restart_syscall` resumes (Linux's `struct restart_block`): set by
/// a handler that returns `-ERESTART_RESTARTBLOCK` (a nanosleep's, a timed
/// futex wait's, a `poll`'s: C4, B3), kept by the syscall-exit hook while
/// it turns that code into `restart_syscall`, and taken by
/// `restart_syscall`, which calls `resume` with it. Lives in the thread's
/// scheduler state (stored through [`interrupted_with`]).
#[derive(Clone, Copy, Debug)]
pub struct RestartBlock {
    /// Resumes the interrupted call: its result (`rax`), handed this block.
    pub resume: fn(&RestartBlock) -> u64,
    /// The interrupted call's deadline (`time::now_ns`), if it had one: a
    /// restart resumes toward the same deadline, never a fresh one.
    pub deadline: Option<u64>,
    /// The rest of its arguments, as its `resume` reads them.
    pub args: [u64; 4],
}

/// A restart code as the handler's `rax` (its negated value).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests
pub const fn restart_result(code: RestartCode) -> u64 {
    (-(code as i64)) as u64
}

/// `-ERESTARTSYS` as a handler's `u32` errno for `syscall::finish`
/// (negated once there): an interrupted wait that restarts unless a handler
/// without `SA_RESTART` is delivered.
pub const ERESTARTSYS: u32 = RestartCode::Sys as u32;

/// The syscall-exit hook, at the end of each syscall (`syscall` the
/// syscall's context: `SyscallExit::new` of its number), with interrupts
/// on and no lock held. `None` is passed on later passes of the loop and
/// after `rt_sigreturn` (C5). Non-IST interrupt returns use
/// `exit_to_user_from_interrupt`; IST vectors are excluded. Resolves a
/// restart code in `frame.rax` only with a syscall context. Without one,
/// registers stay unchanged, but a restart-block result drops the stored
/// block. Dequeues pending signals with the fatal check first, builds handler
/// frames outside the signal lock, and clears sigpending when none remain.
/// Callers loop while `sigpending` is set again by the time interrupts are
/// off, passing `None` after the first pass, so the restart runs once.
/// Returns true when a handler rewrote the frame, requiring IRET.
pub fn exit_to_user(frame: &mut SyscallFrame, mut syscall: Option<SyscallExit>) -> bool {
    debug_assert!((arch::irq::enabled() || arch::irq::disabled_for_good()) && arch::irq::depth() == 0);
    let mut rewritten = false;
    loop {
        let next = match process::try_current_thread() {
            Some(t) => t.process().next_signal(t.tid()),
            None => {
                sched::clear_current_sigpending();
                process::Delivery::None
            }
        };
        match next {
            process::Delivery::None => {
                resolve_restart(frame, decide(syscall, frame.rax, Handling::None));
                return rewritten;
            }
            process::Delivery::Fatal(sig, fault) => terminate(sig, fault),
            process::Delivery::Handler(handler) => match self::frame::deliver(frame, handler, syscall) {
                Ok(()) => {
                    rewritten = true;
                    syscall = None;
                }
                Err(failure) => force_sigsegv(Some(failure.signal), failure.reason),
            },
        }
    }
}

/// Deliver pending work on any non-IST return to ring 3, including timer
/// preemption and WAKEUP IPIs. Enters and leaves with interrupts disabled.
pub fn exit_to_user_from_interrupt(trap: &mut arch::idt::TrapFrame) {
    let pending_db = arch::percpu::take_pending_debug_trap();
    if !sched::current_sigpending() && pending_db.is_none() {
        return;
    }
    let mut frame = self::frame::from_trap(trap);
    arch::irq::restore(true);
    if let Some(debug) = pending_db {
        force_debug_trap(debug.code, debug.rip);
    }
    loop {
        exit_to_user(&mut frame, None);
        let _ = arch::irq::disable();
        if !sched::current_sigpending() {
            break;
        }
        arch::irq::restore(true);
    }
    frame.rflags = arch::syscall::sanitize_user_rflags(frame.rflags);
    self::frame::to_trap(trap, &frame);
}

/// Force a synchronous standard signal on the current thread. Blocked or
/// ignored faults become default-fatal; the return hook is always notified.
pub fn force_fault(info: SigInfo, fault: Option<UserFault>) {
    let me = process::current_thread();
    me.process().force_signal(me.tid(), info, false, fault);
}

/// A failed SEGV frame resets its disposition. Other failures may be caught
/// by an unblocked SEGV handler, for example on an alternate stack.
pub fn force_sigsegv(failed: Option<Signal>, reason: frame::FailureReason) {
    process::current_thread().force_frame_failure(failed, reason);
}

fn terminate(sig: Signal, fault: Option<UserFault>) -> ! {
    let exit = fault.map(UserFault::into_exit).unwrap_or(crate::user::UserExit::Signaled(sig));
    let (pid, is_test) = process::record_exit(exit);
    if is_test {
        crate::user::note_exit_status(128 + sig.number() as i32);
    }
    let failure = process::current_thread().take_frame_failure();
    if let Some(failure) = failure.filter(|_| sig == Signal::SEGV) {
        log::info!(target: "signal", "pid {pid}: could not deliver SIG{}: {}", failure.signal.number(), failure.reason);
    } else if let Some(fault) = fault {
        match fault {
            UserFault::Trap { vector, error_code, rip, cr2 } => {
                log::warn!(target: "user", "pid {pid}: killed by signal {}: trap {vector} error={error_code:#x} rip={rip:#x} cr2={cr2:#x}", sig.number())
            }
            UserFault::Bus { rip, cr2, error_code } => {
                log::warn!(target: "user", "pid {pid}: killed by signal 7: file-backed page fault error={error_code:#x} rip={rip:#x} cr2={cr2:#x}")
            }
            UserFault::NonCanonicalRip(rip) => {
                log::warn!(target: "user", "pid {pid}: killed by signal 11: invalid user return rip={rip:#x}")
            }
        }
    }
    // SAFETY: signal delivery runs on the current user's kernel stack; all
    // signal/table guards and owning references have been dropped.
    unsafe { arch::syscall::unwind_to_kernel() }
}

/// Queue a deferred debug signal using the original trap's address.
pub fn force_debug_trap(code: DebugCode, rip: u64) {
    force_fault(
        SigInfo::fault(Signal::TRAP, i32::from(code.to_raw()), rip),
        Some(UserFault::Trap { vector: 1, error_code: 0, rip, cr2: 0 }),
    );
}

/// Translate a synchronous CPU fault after VM resolution, on the owning stack.
pub fn exception(trap: &arch::idt::TrapFrame, cr2: u64, bus: bool, oom: bool) {
    if let Some(debug) = arch::percpu::take_pending_debug_trap() {
        force_debug_trap(debug.code, debug.rip);
    }
    let (sig, code, addr, fault) = if oom {
        log::warn!(target: "mm", "pid {}: out of memory resolving a user page fault at cr2 {cr2:#x} (rip {:#x}); ending the process", process::current_process().pid(), trap.rip);
        (Signal::KILL, general::SI_KERNEL as i32, 0, None)
    } else if bus {
        (
            Signal::BUS,
            general::BUS_ADRERR as i32,
            cr2,
            Some(UserFault::Bus { rip: trap.rip, cr2, error_code: trap.error_code }),
        )
    } else {
        let Some((code, addr)) = cpu_fault(trap, cr2) else {
            return;
        };
        (
            vector_signal(trap.vector),
            code,
            addr,
            Some(UserFault::Trap { vector: trap.vector, error_code: trap.error_code, rip: trap.rip, cr2 }),
        )
    };
    force_fault(SigInfo::fault(sig, code, addr), fault);
}
fn cpu_fault(trap: &arch::idt::TrapFrame, cr2: u64) -> Option<(i32, u64)> {
    let (code, addr) = match trap.vector {
        0 => (general::FPE_INTDIV as i32, trap.rip),
        1 => (general::TRAP_TRACE as i32, trap.rip),
        6 => (general::ILL_ILLOPN as i32, trap.rip),
        14 => (
            if sched::current_address_space().is_some_and(|s| s.contains_vma(cr2)) {
                general::SEGV_ACCERR
            } else {
                general::SEGV_MAPERR
            } as i32,
            cr2,
        ),
        16 | 19 => {
            let fp = sched::snapshot_user_fpu().expect("user fault");
            let status = if trap.vector == 19 {
                let csr = fp.mxcsr();
                (csr & !(csr >> 7)) & 0x3f
            } else {
                u32::from(fp.fsw() & !fp.fcw()) & 0x3f
            };
            (fpe_code(status)?, trap.rip)
        }
        17 => (general::BUS_ADRALN as i32, 0),
        _ => (general::SI_KERNEL as i32, 0),
    };
    Some((code, addr))
}
fn fpe_code(status: u32) -> Option<i32> {
    Some(if status & 1 != 0 {
        general::FPE_FLTINV
    } else if status & 4 != 0 {
        general::FPE_FLTDIV
    } else if status & 8 != 0 {
        general::FPE_FLTOVF
    } else if status & 0x12 != 0 {
        general::FPE_FLTUND
    } else if status & 0x20 != 0 {
        general::FPE_FLTRES
    } else {
        return None;
    } as i32)
}

/// Apply a restart decision to the frame: a restart sets `rax` and moves
/// `rip` back over the 2-byte `syscall` instruction. A `rip` below 2 means
/// a corrupt frame: logged and answered with `-EINTR`; `ReturnEintr` also
/// sets `-EINTR`. A restart-block result drops the thread's stored block
/// unless it becomes syscall 219 (Linux's `do_no_restart_syscall` reset).
fn resolve_restart(frame: &mut SyscallFrame, decision: RestartDecision) {
    let was_restart_block = RestartCode::from_kernel_result(frame.rax as i64) == Some(RestartCode::RestartBlock);
    let mut regs = self::frame::registers(frame);
    let applied = decision.apply(&mut regs).is_ok();
    if !applied {
        log::error!(target: "signal", "corrupt syscall restart frame: rip {:#x} is below 2", frame.rip);
        regs.rax = (-(errno::EINTR as i64)) as u64;
    }
    frame.rax = regs.rax;
    frame.rip = regs.rip;
    let keep_block = applied && matches!(decision, RestartDecision::Restart { rax: RESTART_SYSCALL });
    if was_restart_block && !keep_block {
        let _ = sched::take_restart_block();
    }
}

/// Store a handler's restart block and return its positive errno:
/// `return Err(interrupted_with(block))`, negated by `syscall::finish`.
#[must_use]
pub fn interrupted_with(block: RestartBlock) -> u32 {
    debug_assert!(sched::restart_block().is_none(), "a handler replaced an unconsumed restart block");
    sched::set_restart_block(Some(block));
    RestartCode::RestartBlock as u32
}

/// `restart_syscall()` (219): take the thread's restart block and resume
/// it (its `resume`'s result), or `-EINTR` with none (Linux's
/// `do_no_restart_syscall`). A resume that is interrupted again sets a new
/// block and returns `-ERESTART_RESTARTBLOCK` itself.
pub fn restart_syscall() -> u64 {
    match sched::take_restart_block() {
        Some(block) => (block.resume)(&block),
        None => (-(errno::EINTR as i64)) as u64,
    }
}

/// Ktests: simulate a notification without a queued signal: `sigpending` set (and `fatal_pending` for `SIGKILL`), then
/// [`sched::kick`]. Nothing is queued, so the hook finds nothing to
/// deliver: the signal behaves as an ignored one.
#[cfg(feature = "ktest")]
pub fn raise_for_test(thread: &sched::Thread, sig: lumen_signal::Signal) {
    thread.set_sigpending();
    if sig == lumen_signal::Signal::KILL {
        thread.set_fatal_pending();
    }
    sched::kick(thread);
}
