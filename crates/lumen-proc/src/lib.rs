//! POSIX process-tree rules, as pure `no_std` + `alloc` logic tested on the
//! host (`cargo test -p lumen-proc`).
//!
//! [`ProcTree`] holds every process the kernel knows by pid: its parent, its
//! children in Linux's children-list order, its process group and session,
//! its wait state ([`ProcState`]) and whether it has exec'd. On it:
//!
//! - **`wait4`/`waitid`**: [`ProcTree::wait4_request`] and
//!   [`ProcTree::waitid_request`] check a call's arguments in Linux's order
//!   ([`WaitRequest`]); [`ProcTree::wait`] picks the child and says what it
//!   reports ([`WaitReady`], [`Report`]), with the status words and
//!   `siginfo` codes of [`WaitStatus`];
//! - **exit and reparenting** ([`ProcTree::exit`]): the zombie, its children
//!   moved to the reaper the kernel names, and what the kernel must then do
//!   ([`ExitOutcome::actions`]: tell or release the zombies, hang up
//!   orphaned process groups with a stopped member);
//! - **stops and continues** ([`ProcTree::stop`], [`ProcTree::cont`]), each
//!   naming the parent to tell ([`StateChanged`]);
//! - **sessions and groups**: [`ProcTree::setsid`], [`ProcTree::setpgid`],
//!   [`ProcTree::getpgid`], [`ProcTree::getsid`], [`ProcTree::is_orphaned_pgrp`]
//!   and the checks of `tcsetpgrp` ([`ProcTree::tcsetpgrp`]);
//! - **pids**: from 2 up to the tree's maximum, then wrapping, never a
//!   number still used as a pid, a pgid or a sid ([`ProcTree::is_busy`]).
//!
//! Ids are plain `u32`s; arguments a syscall passes signed (`wait4`'s `pid`,
//! `waitid`'s `idtype` and `id`, `setpgid`'s two, `getpgid`'s, `getsid`'s
//! and `tcsetpgrp`'s `pgrp`) are taken as `i32` so their negative values get
//! Linux's errors. Errors are [`Error`]s: an [`Errno`] for the user, or a
//! [`Bug`], a call the kernel should never make, which maps to no `errno`.
//!
//! Not modelled: threads (every process is one thread group with one
//! thread), signal delivery and dispositions (the kernel sends what an
//! operation returns), `rusage` accounting (each process carries an opaque
//! `rusage_slot` token instead), controlling-terminal acquisition and
//! hangup, `CLONE_PARENT`, ptrace and child subreapers.
//!
//! # Allocation
//!
//! The kernel decides what may run under a spin lock; this is what each
//! operation does with the global allocator.
//!
//! - **Fallible allocation only:** [`ProcTree::exit`] reserves room in the
//!   reaper's children list and in the exiting process's old children list,
//!   which its [`ExitOutcome`] keeps for the actions. A failed reservation
//!   is `ENOMEM` with the tree unchanged.
//! - **Infallible allocation (a known gap):** stable `BTreeMap`/`BTreeSet`
//!   have no fallible insert. The node a new process takes
//!   ([`ProcTree::spawn_init`], [`ProcTree::spawn_root`], [`ProcTree::fork`])
//!   and the index entries [`ProcTree::setsid`] and [`ProcTree::setpgid`]
//!   move can still abort on out-of-memory; the kernel issue must settle it.
//!   `fork` reserves room in the parent's children list fallibly first
//!   (`ENOMEM`, the tree unchanged). These calls can also free (an index
//!   entry moved out of a node).
//! - **Freeing only:** [`ProcTree::wait`] when it reaps, and
//!   [`ProcTree::release`], free the zombie's node and index entries.
//!   Dropping an [`ExitOutcome`] frees its list. That `wait` and `release`
//!   never allocate rests on liballoc's current `BTreeMap::remove` and
//!   `Vec::retain`; `tests/alloc.rs` checks it with a counting allocator.
//! - **Neither:** the lookups, the request builders, [`ProcTree::exec`],
//!   [`ProcTree::stop`], [`ProcTree::cont`], [`ProcTree::getpgid`],
//!   [`ProcTree::getsid`] and [`ProcTree::tcsetpgrp`].

#![no_std]

extern crate alloc;

mod errno;
mod exit;
mod pid;
mod session;
mod status;
mod tree;
mod wait;

#[cfg(test)]
mod tests;

pub use errno::{Bug, Errno, Error};
pub use exit::{ExitAction, ExitOutcome};
pub use pid::{FIRST_PID, INIT_PID};
pub use session::{Controlling, Sigttou, Tcsetpgrp};
pub use status::{
    CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED, ChildInfo, ExitStatus, Signal, WaitStatus,
};
pub use tree::{ProcState, ProcTree, StateChanged};
pub use wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, Report, WCONTINUED, WEXITED, WNOHANG, WNOWAIT,
    WSTOPPED, WUNTRACED, WaitOptions, WaitReady, WaitRequest, WaitTarget,
};
