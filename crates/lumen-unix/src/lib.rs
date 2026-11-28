//! Pure `no_std + alloc` Unix domain socket (`AF_UNIX`) data-path logic.
//!
//! Models Linux v6.12 `net/unix/af_unix.c` and `net/core/sock.c` queue and
//! state semantics without kernel locks, wait queues, file descriptors, VFS
//! paths, or socket-cycle GC:
//!
//! - [`Ancillary`] and [`TakenFds`]: `SCM_RIGHTS` file references (bounded in
//!   the kernel by [`SCM_MAX_FD`]) and optional `SCM_CREDENTIALS` (`Ucred`),
//!   zero-allocation `MSG_CTRUNC` splitting via [`Ancillary::take_fds`] with
//!   partial-install-safe [`TakenFds::next_delivered`], and explicit draining.
//! - [`StreamPair`]: bidirectional `SOCK_STREAM` byte rings with lazy buffer
//!   growth, `sk_sndbuf` + [`SEGMENT_OVERHEAD`] accounting, Linux
//!   `unix_stream_read_generic` (~2866–2931) fd/credential boundary semantics,
//!   zero-length `read` vs `recv` target-1 semantics, `MSG_PEEK`
//!   (`unix_peek_fds`; clears a pending `Reset` per `sock_error`),
//!   `SHUT_RD`/`SHUT_WR`/`SHUT_RDWR`, `close` / `close_reset` (`ECONNRESET` /
//!   `POLLERR`), `purge_recv` (`unix_collect_skb`), Linux quarter-rule
//!   `unix_writable`, per-direction `in_flight_fds`, and borrowing GC hooks
//!   ([`StreamPair::for_each_queued_fd`]).
//! - [`DgramQueue`], [`Datagram`], [`DgramPayload`], [`SendBudget`], and
//!   [`DgramPeer`]: `SOCK_DGRAM` / `SOCK_SEQPACKET` message queues with
//!   preserved boundaries, per-message payloads allocated outside any lock (no
//!   receiver-side byte cap), required writer credentials (`cred: Ucred`),
//!   `MSG_TRUNC` full-length reporting, `max_dgram_qlen` enforcement
//!   (`unix_recvq_full_lockless`, skipped for mutually connected / self-sending
//!   peers via `peer_is_sender`), Linux overshoot admission in [`SendBudget`]
//!   (`sock_alloc_send_pskb`) with `-EMSGSIZE` at `sndbuf - 32`
//!   (`af_unix.c:2015`), separate sender handle `S` for budget refunds,
//!   `purge` / `set_reset` / `take_error` / `send_shutdown`, `sendto`
//!   destination routing without `-EISCONN`, `unix_may_send`
//!   ([`DgramPeer::may_accept_from`]), and [`DgramQueue::for_each_queued_fd`].
//! - [`Listener`]: `SOCK_STREAM` accept backlog with Linux `somaxconn`
//!   clamping (`__sys_listen_socket`), FIFO `accept`, full-backlog
//!   `WouldBlock`, non-allocating growth via [`ListenerStorage`], borrowing GC
//!   hook [`Listener::pending`], and draining unaccepted (`embrion`)
//!   connections on `close`.
//!
//! # Locking and allocation contract
//!
//! The kernel may guard a [`StreamPair`], [`DgramQueue`], or [`Listener`] with
//! a spinlock or sleep mutex. To keep critical sections safe and bounded:
//!
//! - **Allocating operations** (complete list — every one is fallible via
//!   `try_reserve_exact`, returning [`NoMemory`] or an error containing it,
//!   never panicking on OOM; call outside any socket spinlock):
//!   - [`StreamPair::try_new`]
//!   - [`DgramQueue::try_new`]
//!   - [`Listener::try_new`]
//!   - [`Listener::try_new_with_somaxconn`]
//!   - [`ByteStorage::try_with_capacity`]
//!   - [`SegmentStorage::try_with_capacity`]
//!   - [`DgramPayload::try_from_slice`] (zero-alloc when empty)
//!   - [`DgramPayload::try_with_capacity`]
//!   - [`DgramSlotStorage::try_with_capacity`]
//!   - [`ListenerStorage::try_for_backlog`]
//!   - [`ListenerStorage::try_with_capacity`]
//!   - [`Ancillary::try_with_fd_capacity`]
//! - **Strictly non-allocating operations** (safe while holding a spinlock):
//!   - [`StreamPair::send`] (returns `NeedBuffer` / `NeedSegment` instead of
//!     allocating), [`StreamPair::read`], [`StreamPair::recv`],
//!     [`StreamPair::set_sndbuf`], [`StreamPair::grow_buffer`],
//!     [`StreamPair::grow_segments`], [`StreamPair::shutdown`],
//!     [`StreamPair::purge_recv`], [`StreamPair::close`],
//!     [`StreamPair::close_reset`], [`StreamPair::take_error`],
//!     [`StreamPair::for_each_queued_fd`], and all readiness/accounting queries.
//!   - [`DgramQueue::send`] and [`DgramQueue::send_charged`] (returns
//!     `NeedSlot` instead of allocating), [`DgramQueue::recv`],
//!     [`DgramQueue::grow_slots`], [`DgramQueue::shutdown`],
//!     [`DgramQueue::purge`], [`DgramQueue::close`],
//!     [`DgramQueue::set_reset`], [`DgramQueue::take_error`],
//!     [`DgramQueue::for_each_queued_fd`], [`SendBudget`] methods, [`DgramPeer`]
//!     methods, and all readiness/accounting queries.
//!   - [`Listener::listen`] (returns `NeedStorage` instead of allocating),
//!     [`Listener::grow_storage`], [`Listener::connect`], [`Listener::accept`],
//!     [`Listener::close`], [`Listener::pending`], and all readiness queries.
//!   - [`Ancillary::take_fds`], [`TakenFds::next_delivered`], and all drain
//!     iterators.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod ancillary;
pub mod dgram;
pub mod listener;
pub mod ring;
pub mod stream;

pub use ancillary::{Ancillary, RecvAncillary, SCM_MAX_FD, TakenFds, Ucred};
pub use dgram::{
    DEFAULT_MAX_DGRAM_QLEN, DGRAM_MAX_MSG_OVERHEAD, Datagram, DgramPayload, DgramPeer, DgramQueue, DgramRecvOutcome,
    DgramSendOutcome, DgramSlotStorage, DgramTargetError, DgramUndelivered, RecvValue, SendBudget, SendBudgetError,
    UndeliveredDgram,
};
pub use listener::{
    AcceptOutcome, ConnectOutcome, ListenError, Listener, ListenerStorage, PendingConnections, SOMAXCONN, clamp_backlog,
};
pub use ring::ByteStorage;
pub use stream::{
    DEFAULT_INITIAL_BUF, DEFAULT_SEGMENT_CAPACITY, End, MIN_STREAM_SNDBUF, SEGMENT_OVERHEAD, SegmentStorage,
    SetSndbufError, StreamPair, StreamRecvOutcome, StreamSendOutcome, UndeliveredAncillary,
};

use alloc::vec::Vec;

/// Linux `SHUT_RD` (`include/linux/net.h`).
pub const SHUT_RD: i32 = 0;

/// Linux `SHUT_WR` (`include/linux/net.h`).
pub const SHUT_WR: i32 = 1;

/// Linux `SHUT_RDWR` (`include/linux/net.h`).
pub const SHUT_RDWR: i32 = 2;

/// Linux default socket send buffer size (`net/core/sock.c` `sysctl_wmem_default`,
/// 208 KiB = 212,992 bytes).
pub const DEFAULT_SNDBUF: usize = 212_992;

/// Linux minimum socket send buffer size (`include/net/sock.h` `SOCK_MIN_SNDBUF`,
/// 4,608 bytes).
///
/// The kernel clamps doubled `SO_SNDBUF` values to at least `MIN_SNDBUF`
/// before calling [`StreamPair::set_sndbuf`] or [`SendBudget::set_limit`].
pub const MIN_SNDBUF: usize = 4_608;

/// Fallible heap allocation failed (`-ENOMEM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMemory;

/// Direction(s) passed to `shutdown(2)` (`SHUT_RD`, `SHUT_WR`, `SHUT_RDWR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShutdownHow {
    /// Disallow further receptions (`SHUT_RD` = 0).
    Read,
    /// Disallow further transmissions (`SHUT_WR` = 1).
    Write,
    /// Disallow both receptions and transmissions (`SHUT_RDWR` = 2).
    Both,
}

impl ShutdownHow {
    /// Convert a raw Linux `shutdown(2)` `how` argument (`0`, `1`, or `2`).
    /// Returns `None` (`-EINVAL`) for any other value.
    pub const fn from_raw(how: i32) -> Option<Self> {
        match how {
            SHUT_RD => Some(Self::Read),
            SHUT_WR => Some(Self::Write),
            SHUT_RDWR => Some(Self::Both),
            _ => None,
        }
    }

    /// Raw Linux `shutdown(2)` `how` value.
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Read => SHUT_RD,
            Self::Write => SHUT_WR,
            Self::Both => SHUT_RDWR,
        }
    }

    pub(crate) const fn shuts_read(self) -> bool {
        matches!(self, Self::Read | Self::Both)
    }

    pub(crate) const fn shuts_write(self) -> bool {
        matches!(self, Self::Write | Self::Both)
    }
}

/// Snapshot of Linux `poll(2)` / `epoll(7)` readiness booleans for a socket
/// endpoint (`net/unix/af_unix.c` `unix_poll`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readiness {
    /// `POLLIN | POLLRDNORM`: data/connection queued or read side shut down.
    pub readable: bool,
    /// `POLLOUT | POLLWRNORM`: send buffer below quarter watermark (`wmem << 2 <= sndbuf`).
    pub writable: bool,
    /// `POLLRDHUP`: peer shut down writing (or closed), or local read side shut down.
    pub rdhup: bool,
    /// `POLLHUP`: both directions shut down (or peer/listener closed).
    pub hup: bool,
    /// `POLLERR`: pending socket error (`ECONNRESET`).
    pub err: bool,
}

pub(crate) fn alloc_zeroed_buf(cap: usize) -> Result<Vec<u8>, NoMemory> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(cap).map_err(|_| NoMemory)?;
    buf.resize(cap, 0);
    Ok(buf)
}

#[cfg(test)]
mod tests;
