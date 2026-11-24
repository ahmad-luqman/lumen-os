//! POSIX process-tree rules, as pure `no_std` + `alloc` logic tested on the
//! host (`cargo test -p lumen-proc`).
//!
//! [`ProcTree`] holds every process the kernel knows by pid: its parent, its
//! children in Linux's children-list order, its process group and session,
//! its wait state ([`ProcState`]) and whether it has exec'd. On it:
//!
//! - **`wait4`/`waitid`** ([`ProcTree::wait`]): which child a wait picks
//!   ([`WaitTarget`], [`WaitOptions`]) and what it reports ([`WaitResult`]),
//!   with the status words and `siginfo` codes of [`WaitStatus`];
//! - **exit and reparenting** ([`ProcTree::exit`]): the zombie, its children
//!   moved to the reaper the kernel names, and what the kernel must signal
//!   ([`ExitOutcome`]: the parent's `SIGCHLD`, the adopted zombies, the
//!   process groups newly orphaned with a stopped member);
//! - **sessions and groups**: [`ProcTree::setsid`], [`ProcTree::setpgid`],
//!   [`ProcTree::getpgid`], [`ProcTree::getsid`], [`ProcTree::is_orphaned_pgrp`]
//!   and the checks of `tcsetpgrp` ([`ProcTree::tcsetpgrp`]);
//! - **pids** ([`PidAllocator`]): from 2 up to a maximum, then wrapping,
//!   never a number still used as a pid, a pgid or a sid.
//!
//! Ids are plain `u32`s; arguments a syscall passes signed (`wait4`'s `pid`,
//! `setpgid`'s two, `getpgid`'s and `getsid`'s) are taken as `i32` so their
//! negative values get Linux's errors. Errors are [`Errno`]s.
//!
//! Not modelled: threads (every process is one thread group with one
//! thread), signal delivery and dispositions (the kernel sends what an
//! operation returns), `rusage` accounting, controlling-terminal acquisition
//! and hangup, `CLONE_PARENT`, ptrace and child subreapers.
//!
//! # Allocation
//!
//! Operations that can allocate (call the global allocator for memory):
//! [`ProcTree::spawn_init`], [`ProcTree::spawn_root`], [`ProcTree::fork`],
//! [`ProcTree::exit`], [`ProcTree::setsid`] and [`ProcTree::setpgid`] (tree
//! nodes and vectors grow). Operations that can free memory but never
//! allocate: [`ProcTree::wait`] (when it reaps) and [`ProcTree::release`].
//! Everything else (lookups, [`ProcTree::exec`], [`ProcTree::stop`],
//! [`ProcTree::cont`], [`ProcTree::tcsetpgrp`], [`PidAllocator::alloc`])
//! neither allocates nor frees. Whether any of them may run under a spin
//! lock is the kernel's decision.

#![no_std]

extern crate alloc;

mod errno;
mod pid;
mod session;
mod status;
mod tree;
mod wait;

#[cfg(test)]
mod tests;

pub use errno::Errno;
pub use pid::{FIRST_PID, INIT_PID, PidAllocator};
pub use session::{Sigttou, Tcsetpgrp, Terminal};
pub use status::{
    CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED, ChildInfo, ExitStatus, Signal, WaitStatus,
};
pub use tree::{ExitOutcome, ProcState, ProcTree};
pub use wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WSTOPPED,
    WUNTRACED, WaitOptions, WaitResult, WaitTarget,
};
