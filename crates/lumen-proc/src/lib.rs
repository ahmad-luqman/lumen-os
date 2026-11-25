//! POSIX process-tree rules, as pure `no_std` + `alloc` logic tested on the
//! host (`cargo test -p lumen-proc`).
//!
//! [`ProcTree`] holds every process the kernel knows by pid: its parent, its
//! children in Linux's children-list order, its process group and session,
//! its wait state ([`ProcState`]), whether it has exec'd, and the kernel's
//! token for its resource usage ([`RusageSlot`], owned by the tree until
//! the process leaves it). On it:
//!
//! - **`wait4`/`waitid`**: [`ProcTree::wait4_request`] and
//!   [`ProcTree::waitid_request`] check a call's arguments in Linux's order
//!   ([`WaitRequest`]); [`ProcTree::wait`] picks the child and says what it
//!   reports ([`WaitReady`], [`Report`]), with the status words and
//!   `siginfo` codes of [`WaitStatus`];
//! - **exit and reparenting** ([`ProcTree::exit`]): the zombie, its children
//!   moved to the reaper the kernel names (or made roots, without one), the
//!   zombies nobody will wait for removed, and what the kernel must then do
//!   ([`ExitOutcome`]: notify parents, free tokens, hang up orphaned process
//!   groups with a stopped member, whose members [`ProcTree::pgrp_members`]
//!   lists; no further tree change, but the recipients are numbers, resolved
//!   in the same lock hold);
//! - **stops and continues** ([`ProcTree::stop`], [`ProcTree::cont`]), each
//!   saying whom to tell ([`StateChange`]);
//! - **sessions and groups**: [`ProcTree::setsid`], [`ProcTree::setpgid`],
//!   [`ProcTree::getpgid`], [`ProcTree::getsid`], [`ProcTree::is_orphaned_pgrp`]
//!   and the checks of `tcsetpgrp` ([`ProcTree::tcsetpgrp`]);
//! - **pids**: from 2 up to the tree's maximum, then wrapping, never a
//!   number still used as a pid, a pgid, a sid or a tid
//!   ([`ProcTree::is_busy`]);
//! - **tids**: a thread's number from the same allocator
//!   ([`ProcTree::add_thread`], [`ProcTree::remove_thread`]; the leader's
//!   tid is the pid), so no tid is ever a live pid, pgid or sid; a process's
//!   exit frees any it still has.
//!
//! Ids are plain `u32`s; arguments a syscall passes signed (`wait4`'s `pid`,
//! `waitid`'s `idtype` and `id`, `setpgid`'s two, `getpgid`'s, `getsid`'s
//! and `tcsetpgrp`'s `pgrp`) are taken as `i32` so their negative values get
//! Linux's errors. Signals are `lumen_signal::Signal`s.
//!
//! Errors are per call. The calls made on behalf of a user (`fork`,
//! `add_thread`, `remove_thread`, `wait*`, `setsid`, `setpgid`, `getpgid`,
//! `getsid`, `tcsetpgrp`) return
//! [`Error`]: an [`Errno`] for the user, or a [`Bug`], a wrong or stale pid
//! from the kernel, which maps to no `errno`. The others say only what can
//! happen to them: [`ProcTree::exit`] an [`ExitError`], [`ProcTree::exec`]
//! a [`Bug`], [`ProcTree::spawn_init`] a [`SpawnInitError`],
//! [`ProcTree::spawn_root`] [`NoFreePid`], [`ProcTree::stop`] and
//! [`ProcTree::cont`] `ESRCH`.
//!
//! Threads are modelled only as numbers: a process's tids, for the pid
//! space (`getpgid`, `setpgid` and the other calls look processes up by
//! pid, so a non-leader tid names none). Not modelled: what a thread does,
//! signal delivery and dispositions (the kernel sends what an
//! operation returns, and says through `exit`'s `autoreap` which parents
//! ignore `SIGCHLD`), `rusage` accounting (the tree only carries each
//! process's token), controlling-terminal acquisition and hangup,
//! `CLONE_PARENT`, ptrace, child subreapers, and `PR_SET_PDEATHSIG` (the
//! kernel reads [`ProcTree::children`] before [`ProcTree::exit`] to signal
//! them).
//!
//! # Allocation
//!
//! The kernel decides what may run under a spin lock; this is what each
//! operation does with the global allocator.
//!
//! - **Fallible allocation only:** [`ProcTree::exit`] reserves room in the
//!   reaper's children list and in the exiting process's old children list,
//!   which its [`ExitOutcome`] keeps for the actions (room means the list
//!   holds their words: two for a zombie, four for one released, one for a
//!   hangup; without a reaper, after the children themselves). A failed
//!   reservation is [`ExitError::NoMem`] with the tree unchanged, the
//!   exiting process's tids included. It also frees the nodes of the
//!   zombies that leave, and the index entries of any tids the exiting
//!   process still has.
//! - **Infallible allocation (a known gap):** stable `BTreeMap`/`BTreeSet`
//!   have no fallible insert. A new process takes three B-tree inserts (its
//!   node and its two index entries; [`ProcTree::spawn_init`],
//!   [`ProcTree::spawn_root`], [`ProcTree::fork`]), a new thread one (its
//!   tid's index entry; [`ProcTree::add_thread`]), and [`ProcTree::setsid`]
//!   and [`ProcTree::setpgid`] move index entries; any of them can abort on
//!   out-of-memory, and the kernel issue must settle it. A review counted up
//!   to 8 allocations in a `fork` (node splits), 7 in a `spawn_root`, 4 in a
//!   `setsid` and 2 in a `setpgid`; only `fork`'s reservation in the
//!   parent's children list and `add_thread`'s in the process's thread list
//!   are fallible (`ENOMEM`, the tree unchanged). These calls can also free.
//! - **Freeing only:** [`ProcTree::wait`] when it reaps frees the zombie's
//!   node and index entries, and [`ProcTree::remove_thread`] the tid's.
//!   Dropping an [`ExitOutcome`]'s actions frees their list. That a reaping
//!   `wait` and `remove_thread` never allocate rests on liballoc's current
//!   `BTreeMap::remove` and `Vec::retain`; `tests/alloc.rs` checks it with a
//!   counting allocator.
//! - **Neither:** the lookups (including [`ProcTree::pgrp_members`]), the
//!   request builders, a `wait` that doesn't reap, a refused
//!   [`ProcTree::remove_thread`], [`ProcTree::exec`],
//!   [`ProcTree::stop`], [`ProcTree::cont`], [`ProcTree::getpgid`],
//!   [`ProcTree::getsid`] and [`ProcTree::tcsetpgrp`]; `tests/alloc.rs`
//!   checks that they neither allocate nor free.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod errno;
mod exit;
mod pid;
mod session;
mod slot;
mod status;
mod tree;
mod wait;

#[cfg(test)]
mod tests;

pub use errno::{Bug, Errno, Error, ExitError, NoFreePid, Role, SpawnInitError};
pub use exit::{ExitAction, ExitActions, ExitOutcome};
pub use pid::{FIRST_PID, INIT_PID};
pub use session::{Controlling, Sigttou, Tcsetpgrp};
pub use slot::{RusageSlot, SlotRef};
pub use status::{CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED, ChildInfo, ExitStatus, WaitStatus};
pub use tree::{ProcState, ProcTree, StateChange};
pub use wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, Report, WCONTINUED, WEXITED, WNOHANG, WNOWAIT,
    WSTOPPED, WUNTRACED, WaitOptions, WaitReady, WaitRequest,
};
