//! Pure `no_std + alloc` `epoll(7)` interest-set, ready-list, and nested-set
//! loop-check logic for the Lumen kernel.
//!
//! # Two-lock split (ARCHITECTURE §9 & §10, Linux `fs/eventpoll.c`)
//!
//! Linux splits an epoll instance across two locks so readiness callbacks can
//! run in atomic context without blocking or allocating while `Pollable::poll`
//! runs outside the spinlock:
//!
//! - **`ep->mtx` (sleep mutex) guards [`EpollSet<K, T>`]:** owns the interest
//!   table (`key -> slot`), per-slot payload `T` (in B5: target description and
//!   `Option<PollGuard>`, initialized with `None` until the guard exists),
//!   event masks, user `data`, and slot generations. `EpollSet::add` uses
//!   fallible reservation (`Vec::try_reserve` + [`ReadyQueue::prepare_grow`])
//!   for its slot and ready-queue storage (`Errno::NoMem`, leaving the set
//!   unchanged on failure); note that `BTreeMap::insert` in stable Rust has no
//!   fallible API and can abort on OOM.
//! - **Level-10e leaf spinlock (`ep->lock`, nested inside a `PollHead`'s
//!   subscriber lock) guards [`ReadyQueue`]:** holds only the slab of per-slot
//!   intrusive ready-list links, `in_ready` / `in_scan` / `armed` /
//!   `woke_during_scan` flags, slot generations, and a copy of each slot's
//!   watch mask, indexed by generational [`Handle`]. Every method on
//!   [`ReadyQueue`] except [`ReadyQueue::prepare_grow`] (called *outside* the
//!   spinlock) performs **zero heap allocations and zero deallocations**.
//!
//! Each [`Handle`] and [`Scan`] belongs exclusively to the paired
//! `(`[`EpollSet`]`, `[`ReadyQueue`]`)` instance that created it and must never
//! be passed to a different epoll instance.
//!
//! ## Kernel syscall protocols (B5)
//!
//! ### `EPOLL_CTL_ADD` (`ep_insert`)
//!
//! 1. **Errno precedence (Linux `fs/eventpoll.c`, `do_epoll_ctl`):**
//!    - `EPERM`: target file does not support polling (`!file_can_poll`).
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
//! 2. Holding `ep->mtx` (no spinlock), call [`EpollSet::add`] with a payload
//!    holding a placeholder `None` for the `PollGuard` to obtain [`Added`].
//! 3. Record `let handle = added.handle();`, briefly lock [`ReadyQueue`], call
//!    `let old = rq.activate(&mut set, added);` (installs any growth, confirms
//!    capacity back to `set`, and activates the slot in one step), unlock
//!    [`ReadyQueue`], and drop `old` *after* unlocking.
//! 4. **Post-add subscribe, poll, and wake (mandatory):** while still holding
//!    `ep->mtx` (no spinlock), construct the per-interest `PollWaker` carrying
//!    `handle`, call `Pollable::poll(mode, Some(&waker))` to obtain
//!    `(initial_mask, guard)`, store `Some(guard)` via
//!    [`EpollSet::payload_mut`], and call
//!    `rq.wake(handle, initial_mask.bits() as u32)` under the spinlock
//!    (matching Linux `ep_insert`, which calls `ep_item_poll` after linking).
//!    If `wake` returns [`Wake::Queued`] or [`Wake::DuringScan`]:
//!    - wake this epoll's `epoll_wait` waiters (`ep->wq`, [`Wake::should_wake_waiters`]), **and**
//!    - notify this epoll file's `PollHead` subscribers (`ep->poll_wait`,
//!      [`Wake::should_notify_subscribers`]: outer epolls and `poll`/`select`).
//!
//! ### `EPOLL_CTL_MOD` (`ep_modify`)
//!
//! 1. Holding `ep->mtx`, call [`EpollSet::modify`] (checks `EINVAL` for
//!    `EPOLLEXCLUSIVE`, then `ENOENT`, then `EINVAL` on an existing
//!    `EPOLLEXCLUSIVE` entry) to obtain [`Modified`].
//! 2. Briefly lock [`ReadyQueue`] and call `rq.apply(modified)` (allocation-free,
//!    updates watch mask and re-arms any disarmed `EPOLLONESHOT`).
//! 3. **Post-modify poll and wake (mandatory):** holding only `ep->mtx`, call
//!    `Pollable::poll(mode, None)` on the payload and pass the current mask to
//!    [`ReadyQueue::wake`] under the spinlock (matching Linux `ep_modify`, so
//!    an already-ready descriptor or freshly re-armed `EPOLLONESHOT` is queued).
//!    If `wake` returns [`Wake::Queued`] or [`Wake::DuringScan`], wake this
//!    epoll's `ep->wq` waiters (`should_wake_waiters`) **and** notify this epoll
//!    file's `PollHead` subscribers (`should_notify_subscribers`).
//!
//! ### `EPOLL_CTL_DEL` (`ep_remove`), description close, and instance close
//!
//! - **Single key (`EPOLL_CTL_DEL`):** under `ep->mtx`, call
//!   `let (handle, payload) = set.remove(key)?;`, then `rq.unlink(handle)`
//!   under the spinlock; if the target was an epoll instance, call
//!   [`LoopCheck::remove`]. Unlock `ep->mtx` and drop `payload` (`PollGuard` +
//!   `Arc<File>`) outside both locks.
//! - **File-description close (`eventpoll_release_file`):** under `ep->mtx`,
//!   collect `set.remove_range(desc_range)` into a local `Vec` (yielding every
//!   `(K, Handle, T)`), call `rq.unlink(handle)` for each under the spinlock
//!   (and [`LoopCheck::remove`] for epoll targets), unlock `ep->mtx`, and drop
//!   the collected payloads outside both locks.
//! - **Epoll instance close:** under `ep->mtx`, collect `set.drain()`, call
//!   `rq.unlink(handle)` for each under the spinlock and
//!   [`LoopCheck::remove_set`] for the closing epoll id, unlock `ep->mtx`, and
//!   drop all collected payloads outside both locks.
//!
//! ### Two-phase `epoll_wait` (`ep_start_scan` / `ep_send_events` / `ep_done_scan`)
//!
//! 1. Before sleeping, check [`ReadyQueue::events_available`] under the
//!    spinlock (`in_ready` non-empty **or** a scan currently in flight,
//!    matching Linux `ep_events_available`).
//! 2. Lock `ep->mtx`, reserve/size a handle buffer to at least `set.len()`
//!    (reusable buffer kept alongside `EpollSet` under `ep->mtx`), then briefly
//!    lock [`ReadyQueue`] and call [`ReadyQueue::begin_scan`] (detaches the
//!    entire ready list; panics if `buf.len() < ready_len()`). Unlock
//!    [`ReadyQueue`].
//! 3. Holding only `ep->mtx`, iterate [`Scan::handles`], look up each interest
//!    via [`EpollSet::get`] (`get` takes no locks), and call
//!    `Pollable::poll(mode, None)` on the payload (which may take pipe/tty or
//!    nested-epoll sleep mutexes). If an error occurs between phases, call
//!    `let wake_waiters = rq.abort_scan(scan);` under the spinlock, then wake
//!    **only** this epoll's `ep->wq` waiters if `wake_waiters` is `true`
//!    (never notify `PollHead` subscribers).
//! 4. Briefly lock [`ReadyQueue`] and call [`ReadyQueue::finish_scan`],
//!    collecting [`Event`]s into a kernel buffer. Unlock [`ReadyQueue`] (keep
//!    `ep->mtx` held!).
//! 5. **Copy-out under `ep->mtx` (Linux `ep_send_events`):** copy collected
//!    events to user memory while still holding `ep->mtx` (a sleep mutex above
//!    Lock 1). If copying faults at index `i`: briefly lock [`ReadyQueue`],
//!    call `let wake_waiters = rq.unreport(events[i..].iter().map(|e| e.handle));`,
//!    unlock [`ReadyQueue`] and `ep->mtx`, wake **only** this epoll's `ep->wq`
//!    waiters if `wake_waiters` is `true` (never notify `PollHead`
//!    subscribers), and return `Ok(i)` if `i > 0` else `-EFAULT`.
//! 6. Otherwise unlock `ep->mtx`; if [`ScanDone::wake_waiters`] is `true`, wake
//!    **only** this epoll's `ep->wq` waiters (Linux `ep_done_scan`'s
//!    `wake_up(&ep->wq)` — **never** notify `PollHead` subscribers); return
//!    `Ok(done.reported)`.
//!
//! ### Nested-epoll readiness (`__ep_eventpoll_poll` / `ep_read_events_proc`)
//!
//! When an outer epoll polls an inner epoll file, the inner epoll locks its
//! `ep->mtx`, sizes its handle buffer to `set.len()`, calls
//! [`ReadyQueue::begin_scan`] under its spinlock (detaching the entire ready
//! list), polls handles under `ep->mtx` via `Pollable::poll(mode, None)` on
//! each payload, and calls [`ReadyQueue::finish_peek`] under its spinlock.
//! If [`PeekDone::wake_waiters`] is `true`, wake **only** the inner epoll's own
//! `ep->wq` waiters — **never** notify the inner epoll's `PollHead` subscribers
//! (doing so while an outer `EPOLLET` epoll is scanning the inner handle would
//! trigger `Wake::DuringScan` on the outer handle and cause an infinite
//! self-requeue loop; real source wakes arriving mid-peek already notify
//! subscribers via [`Wake::should_notify_subscribers`]). Return
//! `EPOLLIN | EPOLLRDNORM` iff [`PeekDone::ready`] is `true`.
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
//! - `!file_can_poll` (`EPERM`), `epfd == fd` (`EINVAL`), and `EPOLLEXCLUSIVE`
//!   on an epoll target (`is_file_epoll`, `EINVAL`) are B5's checks.
//! - **Notification split (Linux `ep->wq` vs `ep->poll_wait`):** [`Wake`] is
//!   **not** Linux `ep_poll_callback`'s integer return value (`1`/`0` for
//!   waitqueue traversal); only a real source wake returning [`Wake::Queued`]
//!   or [`Wake::DuringScan`] both wakes this epoll's `ep->wq` waiters
//!   ([`Wake::should_wake_waiters`]) and notifies this epoll file's `PollHead`
//!   subscribers ([`Wake::should_notify_subscribers`]). Scan/rollback results
//!   ([`ScanDone::wake_waiters`], [`PeekDone::wake_waiters`],
//!   [`ReadyQueue::unreport`], [`ReadyQueue::abort_scan`]) mean wake `ep->wq`
//!   waiters **only**, never `PollHead` subscribers.

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
pub use ready::{PeekDone, ReadyQueue, Scan, ScanDone, Storage, Wake};
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

/// Extract the monitored event bits from a normalized `events` mask (stripping
/// [`EP_PRIVATE_BITS`]).
pub(crate) const fn watch_mask(events: u32) -> u32 {
    events & !EP_PRIVATE_BITS
}

/// Generational slot handle returned by [`EpollSet::add`] (via [`Added::handle`])
/// and stored in B5's per-interest `PollWaker` for O(1) lock-separated
/// [`ReadyQueue`] operations.
///
/// Reusing a freed slot bumps `generation`, so any stale `Handle` held by an
/// in-flight waker is rejected safely. Each `Handle` belongs exclusively to the
/// paired `(`[`EpollSet`]`, `[`ReadyQueue`]`)` instance that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle {
    idx: u32,
    generation: u32,
}

impl Handle {
    /// Sentinel invalid handle (`generation == 0`) for initializing scratch
    /// buffers passed to [`ReadyQueue::begin_scan`]. Always rejected as stale
    /// by every lookup and queue operation.
    pub const INVALID: Self = Self { idx: 0, generation: 0 };

    pub(crate) const fn new(idx: u32, generation: u32) -> Self {
        Self { idx, generation }
    }

    /// Slot index within the paired [`EpollSet`] and [`ReadyQueue`].
    pub const fn index(self) -> u32 {
        self.idx
    }

    /// Non-zero slot generation distinguishing reused slots from stale handles
    /// (`0` only for [`Self::INVALID`]).
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
    /// Out of memory (`ENOMEM` = 12): fallible slot-table (`Vec::try_reserve`)
    /// or [`ReadyQueue::prepare_grow`] storage reservation failed and state is
    /// unchanged. (`BTreeMap::insert` in stable Rust is not fallible.)
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
