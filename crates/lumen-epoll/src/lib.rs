//! Pure `no_std + alloc` `epoll(7)` interest-set, ready-list, and nested-set
//! loop-check logic for the Lumen kernel.
//!
//! [`EpollSet`] maintains the interest set and intrusive ready list for one
//! epoll instance, following Linux `fs/eventpoll.c` (`ep_insert`, `ep_modify`,
//! `ep_remove`, `ep_poll_callback`, and `ep_send_events`). [`LoopCheck`]
//! tracks nested epoll-watches-epoll edges across instances to reject cycles
//! and nesting deeper than [`EP_MAX_NESTS`] (`4`) with [`Errno::Loop`],
//! matching Linux's `ep_loop_check`.
//!
//! # Zero-allocation guarantee (ARCHITECTURE §9, level 10e)
//!
//! In the kernel, an epoll instance's ready list nests inside a `PollHead`'s
//! subscriber spinlock (section 9, level 10e), so waking an interest from a
//! `PollWaker::wake` callback must never allocate, deallocate, or block:
//!
//! - [`EpollSet::add`] pre-allocates the interest's slab slot and map entry.
//! - [`EpollSet::wake`] performs **zero heap allocations and zero
//!   deallocations**: it looks up the interest in the existing map and links
//!   its pre-allocated slot onto the ready list in O(1).
//! - [`EpollSet::collect`] performs **zero heap allocations and zero
//!   deallocations** (provided the caller's `poll` closure and `out` sink do
//!   not allocate): it splices the ready list into a local list, re-polls each
//!   queued interest, requeues level-triggered entries at the tail, drops
//!   edge-triggered entries, disarms one-shots, and splices any unprocessed
//!   remainder back to the front of the ready list for round-robin fairness.
//! - [`EpollSet::modify`] also performs **zero heap allocations and zero
//!   deallocations**.
//!
//! # Event-bit rules
//!
//! - [`EPOLLERR`] and [`EPOLLHUP`] are always implicitly monitored and
//!   reported while an interest is armed (Linux `fs/eventpoll.c`,
//!   `do_epoll_ctl`).
//! - [`EPOLLWAKEUP`] is accepted and ignored (stripped on `add` and `modify`):
//!   Lumen has no system suspend / `CAP_BLOCK_SUSPEND` wakeup sources, and
//!   Linux likewise clears `EPOLLWAKEUP` for callers without
//!   `CAP_BLOCK_SUSPEND`.
//! - Private modifier bits ([`EP_PRIVATE_BITS`]: [`EPOLLET`],
//!   [`EPOLLONESHOT`], [`EPOLLWAKEUP`], [`EPOLLEXCLUSIVE`]) are never matched
//!   against readiness masks and never reported in collected `revents`.
//! - Unlike Linux's untyped `wake_up()` (which passes a NULL/0 poll key that
//!   conservatively queues the item), Lumen's typed `PollWaker::wake` always
//!   delivers an explicit readiness mask, so [`EpollSet::wake`] queues an
//!   armed interest only when `ready` intersects its monitored event bits.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod loop_check;
mod set;

#[cfg(test)]
mod tests;

use core::fmt;
use linux_raw_sys::{errno, general};

pub use loop_check::{EP_MAX_NESTS, LoopCheck};
pub use set::EpollSet;

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

/// Linux `errno` values returned by [`EpollSet`] and [`LoopCheck`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    /// Entry not found (`ENOENT` = 2): `modify` or `remove` on a key not
    /// present in the interest set.
    NoEnt,
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
            Self::Exist => "EEXIST",
            Self::Inval => "EINVAL",
            Self::Loop => "ELOOP",
        })
    }
}

impl core::error::Error for Errno {}
