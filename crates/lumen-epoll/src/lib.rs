//! Pure `no_std + alloc` `epoll(7)` interest-set, ready-list, and nested-set
//! loop-check logic for the Lumen kernel.
//!
//! # Two-lock split (ARCHITECTURE §9 & §10, Linux `fs/eventpoll.c`)
//!
//! Linux splits an epoll instance across two locks so readiness callbacks can
//! run in atomic context without blocking or allocating while `vfs_poll` runs
//! outside the spinlock:
//!
//! - **`ep->mtx` (sleep mutex) guards [`EpollSet<K, T>`]:** owns the interest
//!   table (`key -> slot`), per-slot payload `T` (in B5: the target file
//!   description and its `PollGuard`), event masks, user `data`, and slot
//!   generations. Operations here may allocate (`add` uses fallible
//!   `try_reserve` + [`ReadyQueue::prepare_grow`], returning [`Errno::NoMem`]
//!   with the set unchanged on failure).
//! - **Level-10e leaf spinlock (`ep->lock`, nested inside a `PollHead`'s
//!   subscriber lock) guards [`ReadyQueue`]:** holds only the slab of
//!   per-slot intrusive ready-list links, `queued` / `armed` /
//!   `woke_during_scan` flags, slot generations, and a copy of each slot's
//!   watch mask, indexed by generational [`Handle`]. Every method on
//!   [`ReadyQueue`] except [`ReadyQueue::prepare_grow`] (which is called
//!   *outside* the spinlock) performs **zero heap allocations and zero
//!   deallocations**.
//!
//! ## Kernel syscall protocols (B5)
//!
//! ### `EPOLL_CTL_ADD` (`ep_insert`)
//!
//! 1. **Errno precedence (Linux `fs/eventpoll.c`, `do_epoll_ctl`):**
//!    - `EINVAL`: `epfd == fd` (`f.file == tf.file`), `EPOLLEXCLUSIVE` on an
//!      epoll target (`is_file_epoll(tf.file)`), or `EPOLLEXCLUSIVE` combined
//!      with bits outside [`EPOLLEXCLUSIVE_OK_BITS`] (checked by
//!      [`EpollSet::add`]).
//!    - `ELOOP`: for an epoll target, [`LoopCheck::add`] rejects cycles or
//!      nesting deeper than [`EP_MAX_NESTS`] (`4`). Keep strict 1-to-1 pairing:
//!      one [`LoopCheck::add`] per successfully added epoll-target interest and
//!      one [`LoopCheck::remove`] on teardown; if [`EpollSet::add`] fails after
//!      [`LoopCheck::add`] (e.g. `EEXIST` or `ENOMEM`), immediately undo it
//!      with [`LoopCheck::remove`].
//!    - `EEXIST` / `ENOMEM`: checked by [`EpollSet::add`].
//! 2. Holding `ep->mtx` (no spinlock), call [`EpollSet::add`] to obtain
//!    [`Added`].
//! 3. Briefly lock [`ReadyQueue`], call [`ReadyQueue::install`] if
//!    `added.growth` is `Some` (binding the returned old [`Storage`]), call
//!    `rq.set_mask(added.handle, added.watch_mask, true)`, unlock
//!    [`ReadyQueue`], and drop any old [`Storage`] *after* unlocking.
//! 4. **Post-add subscribe, poll, and wake (mandatory):** while still holding
//!    `ep->mtx` (no spinlock), construct the per-interest `PollWaker` carrying
//!    `added.handle`, call `Pollable::poll(mode, Some(&waker))` to get
//!    `(initial_mask, guard)`, store `guard` via [`EpollSet::payload_mut`],
//!    and call `rq.wake(added.handle, initial_mask.bits() as u32)` under the
//!    spinlock (matching Linux `ep_insert`, which calls `ep_item_poll` after
//!    linking).
//!
//! ### `EPOLL_CTL_MOD` (`ep_modify`)
//!
//! 1. Holding `ep->mtx`, call [`EpollSet::modify`] (checks `EINVAL` for
//!    `EPOLLEXCLUSIVE`, then `ENOENT`, then `EINVAL` on an existing
//!    `EPOLLEXCLUSIVE` entry).
//! 2. Briefly lock [`ReadyQueue`] and call
//!    `rq.set_mask(modified.handle, modified.watch_mask, true)` (allocation-free,
//!    re-arms a disarmed `EPOLLONESHOT`).
//! 3. **Post-modify poll and wake (mandatory):** holding only `ep->mtx`, call
//!    `Pollable::poll(mode, None)` and pass the current mask to
//!    [`ReadyQueue::wake`] under the spinlock (matching Linux `ep_modify`, so
//!    an already-ready descriptor or freshly re-armed `EPOLLONESHOT` is queued).
//!
//! ### `EPOLL_CTL_DEL` (`ep_remove`) and description/instance teardown
//!
//! - Single key: under `ep->mtx`, `let (handle, payload) = set.remove(key)?;`,
//!   then `rq.unlink(handle)` under the spinlock, then unlock `ep->mtx` and
//!   drop `payload` (`PollGuard` + `Arc<File>`) outside both locks.
//! - File-description close (`eventpoll_release_file`): under `ep->mtx`, remove
//!   all keys for that description with [`EpollSet::remove_range`], unlink each
//!   returned [`Handle`] under the spinlock, and drop the payloads outside both
//!   locks.
//! - Epoll instance close: [`EpollSet::drain`] yields every `(K, Handle, T)` so
//!   payloads drop outside locks and [`LoopCheck::remove_set`] clears incident
//!   nesting edges.
//!
//! ### Two-phase `epoll_wait` (`ep_start_scan` / `ep_send_events` / `ep_done_scan`)
//!
//! 1. Lock `ep->mtx`, then briefly lock [`ReadyQueue`] and call
//!    [`ReadyQueue::begin_scan`] with a caller-owned handle buffer (zero alloc).
//!    Unlock [`ReadyQueue`].
//! 2. Holding only `ep->mtx`, poll each handle in [`Scan::handles`] via
//!    [`EpollSet::get`] (which is safe to acquire pipe/tty `SleepMutex`es and
//!    nested epoll `ep->mtx`es).
//! 3. Re-acquire [`ReadyQueue`] and call [`ReadyQueue::finish_scan`], collecting
//!    [`Event`]s into a kernel buffer. Unlock [`ReadyQueue`] and `ep->mtx`.
//! 4. Copy events to user memory. If copying faults at index `i` (`-EFAULT`),
//!    lock [`ReadyQueue`] and call [`ReadyQueue::unreport`] with the uncopied
//!    tail's handles so no `EPOLLONESHOT` or `EPOLLET` event is lost.
//!
//! ### Nested-epoll readiness (`__ep_eventpoll_poll` / `ep_read_events_proc`)
//!
//! When an outer epoll polls an inner epoll file, the inner epoll runs
//! [`ReadyQueue::begin_scan`], polls queued handles under its `ep->mtx`, and
//! calls [`ReadyQueue::finish_peek`]. It reports ready (`EPOLLIN | EPOLLRDNORM`)
//! only if an entry is genuinely ready, without disarming `EPOLLONESHOT` or
//! dropping ready `EPOLLET`/LT entries, while pruning stale (`revents == 0`)
//! entries whose pipes/sockets were drained.
//!
//! # Event bits and documented boundaries
//!
//! - [`EPOLLERR`] and [`EPOLLHUP`] are always implicitly monitored and reported
//!   while an interest is armed (Linux `fs/eventpoll.c`, `do_epoll_ctl`).
//! - [`EPOLLWAKEUP`] is accepted and ignored (stripped on `add` and `modify`):
//!   Lumen has no system suspend / `CAP_BLOCK_SUSPEND` wakeup sources, and
//!   Linux likewise clears `EPOLLWAKEUP` for callers without
//!   `CAP_BLOCK_SUSPEND`.
//! - [`EP_PRIVATE_BITS`] ([`EPOLLET`], [`EPOLLONESHOT`], [`EPOLLWAKEUP`],
//!   [`EPOLLEXCLUSIVE`]) are never matched against readiness masks and never
//!   reported in collected `revents`.
//! - `EPOLLEXCLUSIVE` wake-one semantics are **not** modelled here (Lumen's
//!   `PollHead::notify` wakes all subscribers).
//! - Linux's `reverse_path_check` (wakeup-path fan-out limits returning
//!   `EINVAL`) is **not** modelled here (left to B5 or omitted).
//! - `epfd == fd` (`EINVAL`) and `EPOLLEXCLUSIVE` on an epoll target
//!   (`is_file_epoll`, `EINVAL`) are B5's checks.
//! - [`Wake`] is **not** Linux `ep_poll_callback`'s integer return value
//!   (`1`/`0` for waitqueue traversal); it tells B5 whether an entry newly
//!   entered the ready list so B5 knows when to wake sleepers.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod loop_check;
mod ready;
mod set;

#[cfg(test)]
mod tests;

use core::fmt;
use linux_raw_sys::{errno, general};

pub use loop_check::{EP_MAX_NESTS, LoopCheck};
pub use ready::{ReadyQueue, Scan, Storage, Wake};
pub use set::{Added, Drain, EntryRef, EpollSet, Modified, RemoveRange};

/// Data other than high-priority data may be read without blocking.
pub const EPOLLIN: u32 = general::EPOLLIN;
/// Exceptional condition / urgent data available.
pub const EPOLLPRI: u32 = general::EPOLLPRI;
/// Writing is now possible without blocking.
pub const EPOLLOUT: u32 = general::EPOLLOUT;
/// Error condition (always reported while armed).
pub const EPOLLERR: u32 = general::EPOLLERR;
/// Hang up (always reported while armed).
pub const EPOLLHUP: u32 = general::EPOLLHUP;
/// Invalid descriptor (`POLLNVAL` equivalent).
pub const EPOLLNVAL: u32 = general::EPOLLNVAL;
/// Normal data may be read without blocking.
pub const EPOLLRDNORM: u32 = general::EPOLLRDNORM;
/// Priority band data may be read without blocking.
pub const EPOLLRDBAND: u32 = general::EPOLLRDBAND;
/// Normal data may be written without blocking.
pub const EPOLLWRNORM: u32 = general::EPOLLWRNORM;
/// Priority band data may be written without blocking.
pub const EPOLLWRBAND: u32 = general::EPOLLWRBAND;
/// Unused Linux message bit.
pub const EPOLLMSG: u32 = general::EPOLLMSG;
/// Stream socket peer closed connection or shut down writing half.
pub const EPOLLRDHUP: u32 = general::EPOLLRDHUP;

/// Exclusive wakeup mode (cannot be combined with `EPOLLONESHOT` or non-allowed
/// event bits, and cannot be set or altered via `modify`).
pub const EPOLLEXCLUSIVE: u32 = general::EPOLLEXCLUSIVE;
/// System-suspend wakeup hint; accepted and ignored in Lumen.
pub const EPOLLWAKEUP: u32 = general::EPOLLWAKEUP;
/// One-shot mode: disarms after delivering an event until re-armed by `modify`.
pub const EPOLLONESHOT: u32 = general::EPOLLONESHOT;
/// Edge-triggered mode: dropped from the ready list after collection rather
/// than requeued.
pub const EPOLLET: u32 = general::EPOLLET;

/// `epoll_create1` close-on-exec flag.
pub const EPOLL_CLOEXEC: u32 = general::EPOLL_CLOEXEC;
/// `epoll_ctl` operation: add a target descriptor.
pub const EPOLL_CTL_ADD: u32 = general::EPOLL_CTL_ADD;
/// `epoll_ctl` operation: remove a target descriptor.
pub const EPOLL_CTL_DEL: u32 = general::EPOLL_CTL_DEL;
/// `epoll_ctl` operation: modify a registered descriptor.
pub const EPOLL_CTL_MOD: u32 = general::EPOLL_CTL_MOD;

/// Modifier bits stored alongside event bits in `epoll_event::events`
/// (Linux `fs/eventpoll.c` `EP_PRIVATE_BITS`). Never reported in `revents`.
pub const EP_PRIVATE_BITS: u32 = EPOLLWAKEUP | EPOLLONESHOT | EPOLLET | EPOLLEXCLUSIVE;

/// Event bits implicitly monitored on every armed interest
/// (Linux `fs/eventpoll.c`, `do_epoll_ctl`).
pub const EP_ALWAYS_EVENTS: u32 = EPOLLERR | EPOLLHUP;

/// Event and modifier bits permitted when [`EPOLLEXCLUSIVE`] is set in
/// `EPOLL_CTL_ADD` (Linux `fs/eventpoll.c`, `EPOLLEXCLUSIVE_OK_BITS`).
pub const EPOLLEXCLUSIVE_OK_BITS: u32 =
    EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP | EPOLLWAKEUP | EPOLLET | EPOLLEXCLUSIVE;

/// Generational slot handle returned by [`EpollSet::add`] and stored in B5's
/// per-interest `PollWaker` for O(1) lock-separated [`ReadyQueue`] operations.
///
/// Reusing a freed slot bumps `gen`, so any stale `Handle` held by an in-flight
/// waker or unreport path is rejected safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle {
    idx: u32,
    generation: u32,
}

impl Handle {
    pub(crate) const fn new(idx: u32, generation: u32) -> Self {
        Self { idx, generation }
    }

    /// Slot index within the paired [`EpollSet`] and [`ReadyQueue`].
    pub const fn index(self) -> u32 {
        self.idx
    }

    /// Non-zero slot generation distinguishing reused slots from stale handles.
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// One event collected by [`ReadyQueue::finish_scan`].
///
/// Carries the originating [`Handle`] alongside Linux's `(events, data)` pair
/// so that if copying the batch to user memory faults at index `i`, B5 can
/// pass the uncopied tail's handles to [`ReadyQueue::unreport`] without losing
/// any `EPOLLONESHOT` or `EPOLLET` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Interest handle that produced this event (for [`ReadyQueue::unreport`]).
    pub handle: Handle,
    /// Reported readiness bits (masked to monitored events, never containing
    /// [`EP_PRIVATE_BITS`]).
    pub events: u32,
    /// Opaque user `data` stored on the interest.
    pub data: u64,
}

/// Linux `errno` values returned by [`EpollSet`], [`ReadyQueue::prepare_grow`],
/// and [`LoopCheck`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    /// Entry not found (`ENOENT` = 2): `modify` or `remove` on a key not
    /// present in the interest set.
    NoEnt,
    /// Out of memory (`ENOMEM` = 12): fallible slot/storage reservation failed
    /// and state is unchanged.
    NoMem,
    /// Entry already exists (`EEXIST` = 17): `add` on a key already present
    /// in the interest set.
    Exist,
    /// Invalid argument (`EINVAL` = 22): invalid event-flag combination (such
    /// as `EPOLLEXCLUSIVE` with `EPOLLONESHOT` or other disallowed bits,
    /// `EPOLLEXCLUSIVE` in `modify`, or modifying an `EPOLLEXCLUSIVE`
    /// interest).
    Inval,
    /// Circular nesting or nesting depth past [`EP_MAX_NESTS`] (`ELOOP` = 40).
    Loop,
}

impl Errno {
    /// The positive Linux `errno` code (a syscall returns its negation).
    pub const fn to_errno(self) -> u32 {
        match self {
            Self::NoEnt => errno::ENOENT,
            Self::NoMem => errno::ENOMEM,
            Self::Exist => errno::EEXIST,
            Self::Inval => errno::EINVAL,
            Self::Loop => errno::ELOOP,
        }
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoEnt => "ENOENT",
            Self::NoMem => "ENOMEM",
            Self::Exist => "EEXIST",
            Self::Inval => "EINVAL",
            Self::Loop => "ELOOP",
        })
    }
}

impl core::error::Error for Errno {}
