//! The syscall-exit hook (M8's B2, ARCHITECTURE sections 6 and 8, "One
//! interruptible, killable blocking wait"), which every return to ring 3
//! runs, and the restart block `restart_syscall` resumes.
//!
//! A blocking syscall's interruptible wait ends `Interrupted` when its
//! thread has a signal to look at (`sched::Thread::sigpending`), and the
//! handler returns a restart code (`-ERESTARTSYS`, `-ERESTARTNOINTR`,
//! `-ERESTARTNOHAND`, `-ERESTART_RESTARTBLOCK`). None reaches user space:
//! the hook resolves each, at the end of the syscall, with
//! `lumen_signal::restart::decide`. Until C5 nothing is delivered, so every
//! code is resolved as with no handler (`Handling::None`): a restart (`rax`
//! the syscall's number, `rip` back over the 2-byte `syscall`), or for
//! `-ERESTART_RESTARTBLOCK` `restart_syscall` (219), which resumes from the
//! thread's [`RestartBlock`] (`-EINTR` with none, Linux's
//! `do_no_restart_syscall`). Then, nothing being deliverable, the hook
//! clears `sigpending` (under the process's signal lock). C5 adds delivery
//! to the same hook: the restart decision then runs once, through
//! `frame::build` when a handler is delivered and through `decide` when
//! none is.
//!
//! The ktests raise a signal through [`raise_for_test`] (the flags and a
//! kick, nothing queued: C5 adds the pending queues).

use lumen_signal::restart::{Handling, RestartCode, RestartDecision, SyscallExit, decide};

use crate::arch;
use crate::arch::syscall::SyscallFrame;
use crate::linux::errno;
use crate::sched;

/// What `restart_syscall` resumes (Linux's `struct restart_block`): set by
/// a handler that returns `-ERESTART_RESTARTBLOCK` (a nanosleep's, a timed
/// futex wait's, a `poll`'s: C4, B3), kept by the syscall-exit hook while
/// it turns that code into `restart_syscall`, and taken by
/// `restart_syscall`, which calls `resume` with it. Lives in the thread's
/// scheduler state (`sched::set_restart_block`).
#[derive(Clone, Copy, Debug)]
pub struct RestartBlock {
    /// Resumes the interrupted call: its result (`rax`), handed this block.
    pub resume: fn(&RestartBlock) -> u64,
    /// The interrupted call's deadline (`time::now_ns`), if it had one: a
    /// restart resumes toward the same deadline, never a fresh one.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by C4's and B3's resumes
    pub deadline: Option<u64>,
    /// The rest of its arguments, as its `resume` reads them.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by C4's and B3's resumes
    pub args: [u64; 4],
}

/// A restart code as the handler's `rax` (its negated value).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests; C4's and B3's handlers next
pub const fn restart_result(code: RestartCode) -> u64 {
    (-(code as i64)) as u64
}

/// `-ERESTARTSYS` as a handler's `u32` errno for `syscall::finish`
/// (negated once there): an interrupted wait that restarts unless a handler
/// without `SA_RESTART` is delivered.
pub const ERESTARTSYS: u32 = RestartCode::Sys as u32;

/// The syscall-exit hook, at the end of each syscall (`syscall` the
/// syscall's context: `SyscallExit::new` of its number) or on a return to
/// ring 3 with no syscall to resolve (`None`), with interrupts on and no
/// lock held. Resolves a restart code in `frame.rax` (only with a syscall
/// context: a user `rax` that looks like one is left alone), then, if the
/// thread has `sigpending`, clears it (nothing is deliverable before C5).
/// Callers loop while `sigpending` is set again by the time interrupts are
/// off, passing `None` after the first pass, so the restart runs once.
pub fn exit_to_user(frame: &mut SyscallFrame, syscall: Option<SyscallExit>) {
    debug_assert!(
        (arch::irq::enabled() || arch::irq::disabled_for_good()) && arch::irq::depth() == 0,
        "the syscall-exit hook runs with interrupts on and no lock held"
    );
    resolve_restart(frame, decide(syscall, frame.rax, Handling::None));
    settle_pending();
}

/// The interrupt and exception return to ring 3's hook (`lumen_isr_common`,
/// the timer's preemption included), called with interrupts off, from a
/// non-IST vector: if the thread has `sigpending`, the hook with interrupts
/// on, as often as it is set again before interrupts are off. No syscall
/// context, so no register changes (C5 delivers here too).
pub fn exit_to_user_from_interrupt() {
    while sched::current_sigpending() {
        arch::irq::restore(true);
        settle_pending();
        let _ = arch::irq::disable();
    }
}

/// Apply a restart decision to the frame: a restart sets `rax` and moves
/// `rip` back over the 2-byte `syscall` instruction (a `rip` below 2,
/// which no `syscall` returns to, gets `-EINTR`), `ReturnEintr` sets
/// `-EINTR`.
fn resolve_restart(frame: &mut SyscallFrame, decision: RestartDecision) {
    let eintr = (-(errno::EINTR as i64)) as u64;
    match decision {
        RestartDecision::Unchanged => {}
        RestartDecision::ReturnEintr => frame.rax = eintr,
        RestartDecision::Restart { rax } => match frame.rip.checked_sub(2) {
            Some(rip) => {
                frame.rax = rax;
                frame.rip = rip;
            }
            None => frame.rax = eintr,
        },
    }
}

/// The hook's last step: with nothing deliverable (always, before C5), the
/// running thread's `sigpending` cleared, under its process's signal lock
/// if it is a Linux thread (where C5's senders set it). `fatal_pending` is
/// never cleared (C5 and C1 end such a thread here).
fn settle_pending() {
    if !sched::current_sigpending() {
        return;
    }
    match crate::linux::process::try_current_thread() {
        Some(thread) => thread.process().settle_sigpending(),
        None => {
            let _ = sched::with_current(sched::Thread::clear_sigpending);
        }
    }
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

/// Ktests: raise `sig` on `thread` as a sender would before C5 queues
/// anything: `sigpending` set (and `fatal_pending` for `SIGKILL`), then
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
