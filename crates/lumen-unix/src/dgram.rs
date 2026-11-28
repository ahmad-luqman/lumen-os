//! `AF_UNIX` `SOCK_DGRAM` (and `SOCK_SEQPACKET`) message queue, sender
//! accounting, resets/purges, and peer routing.
//!
//! Models Linux v6.12 `net/unix/af_unix.c` and `net/core/sock.c`:
//! - `unix_dgram_sendmsg` & `unix_dgram_recvmsg`: preserved message boundaries
//!   (including zero-length datagrams), per-message payloads allocated outside
//!   any socket lock ([`DgramPayload`], no receiver-side byte cap), required
//!   writer credentials (`cred: Ucred`, overridden by explicit
//!   `SCM_CREDENTIALS` per `unix_scm_to_skb`), `MSG_TRUNC` full-length
//!   reporting, `MSG_PEEK`, optional sender address `A`, separate sender
//!   handle `S` for [`SendBudget`] refunds, `-EMSGSIZE` when
//!   `len > sndbuf - 32` (`af_unix.c:2015`), and skipping `max_dgram_qlen`
//!   (`unix_recvq_full_lockless`) when the receiver is connected back to the
//!   sender or is the sender (`other != sk && unix_peer(other) != sk`).
//! - `sock_alloc_send_pskb`: [`SendBudget`] admits whenever `used < limit` and
//!   lets the charge overshoot `limit` (guaranteeing that whenever
//!   `writable()` holds, any datagram up to `max_msg_len()` is admitted).
//! - `DgramPeer`: `sendto` with an explicit destination on a connected
//!   datagram socket uses the destination given (no `-EISCONN`);
//!   [`DgramPeer::may_accept_from`] mirrors `unix_may_send` (`-EPERM` when a
//!   receiver is connected to someone else).
//! - `unix_dgram_poll` & `unix_writable`: `SendBudget::writable()` uses
//!   Linux's quarter rule (`used << 2 <= limit`), `accepts` withholds
//!   `POLLOUT` only for a full non-mutual queue (`closed`/`recv_shutdown`
//!   still report `accepts == true` so the sender wakes to see the error), and
//!   `DgramQueue::hup()` reports `POLLHUP` on `SHUT_RDWR` / close.
//! - `unix_dgram_disconnected` (C1 reconnect / disconnect sequence):
//!   1. On `connect(new_peer)` or `connect(AF_UNSPEC)` while connected to
//!      `old_peer`, detach `old_peer` from `DgramPeer`.
//!   2. Call `self.queue.purge()` (empties `self`'s receive queue without
//!      closing or shutting it down).
//!   3. If `purged.had_dropped()` and `old_peer` was connected back to `self`,
//!      call `old_peer.queue.set_reset()` (sets `POLLERR` + one `Reset`
//!      consumed on next `recv`/`take_error`).
//!   4. Outside locks, refund each dropped datagram's `SendBudget` via
//!      `item.sender` + `item.len()`, close its `Ancillary` fds, and drop its
//!      `payload`.
//!   5. Repeat the disconnect + purge step when a connected `send` observes
//!      [`DgramSendOutcome::PeerClosed`] (`-ECONNREFUSED`).

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::ring::{grow_deque, try_deque};
use crate::{Ancillary, DEFAULT_SNDBUF, NoMemory, RecvAncillary, ShutdownHow, Ucred};

/// Linux kernel default `net.unix.max_dgram_qlen` (`unix(7)`).
///
/// Linux v6.12 `net/unix/af_unix.c` `unix_recvq_full_lockless` checks
/// `skb_queue_len_lockless(&sk->sk_receive_queue) > READ_ONCE(sk->sk_max_ack_backlog)`,
/// so a `max_dgram_qlen` of `N` queues up to `N + 1` datagrams before refusing
/// non-mutual senders with `WouldBlock`.
pub const DEFAULT_MAX_DGRAM_QLEN: usize = 10;

/// Headroom subtracted from `sk_sndbuf` for the `-EMSGSIZE` limit
/// (`len > sk->sk_sndbuf - 32`, Linux v6.12 `net/unix/af_unix.c:2015`).
///
/// Lumen also charges this fixed 32 B overhead per queued datagram in
/// [`SendBudget`] (a deliberate Lumen choice; Linux charges `skb->truesize`)
/// so 0-byte datagrams on a mutually connected pair remain bounded by
/// `sk_sndbuf`.
pub const DGRAM_MAX_MSG_OVERHEAD: usize = 32;

/// Minimum slot capacity selected when growing a datagram slot ring via
/// [`DgramQueue::next_slot_capacity`].
const MIN_GROW_SLOT_CAPACITY: usize = 16;

/// Error from [`DgramPeer::resolve_send_target`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DgramTargetError {
    /// Unconnected datagram socket called `send` without a destination address
    /// (Linux v6.12 `net/unix/af_unix.c` `unix_dgram_sendmsg`: `-ENOTCONN`).
    NotConnected,
}

/// Connected-peer state for an `AF_UNIX` `SOCK_DGRAM` socket.
///
/// Tracks `connect(peer)`, `connect(AF_UNSPEC)` (`disconnect`), Linux's
/// `unix_dgram_sendmsg` destination selection (an explicit `sendto`
/// destination is used even on an already-connected socket — no `-EISCONN`),
/// and `unix_may_send` ([`DgramPeer::may_accept_from`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DgramPeer<P> {
    peer: Option<P>,
}

impl<P> Default for DgramPeer<P> {
    fn default() -> Self {
        Self { peer: None }
    }
}

impl<P> DgramPeer<P> {
    /// Create an unconnected datagram endpoint.
    pub const fn unconnected() -> Self {
        Self { peer: None }
    }

    /// Create a datagram endpoint already connected to `peer` (e.g.
    /// `socketpair(AF_UNIX, SOCK_DGRAM)`).
    pub const fn connected(peer: P) -> Self {
        Self { peer: Some(peer) }
    }

    /// Connect (or reconnect) to `peer`, returning any previously connected
    /// peer (Linux `net/unix/af_unix.c` `unix_dgram_connect`).
    pub fn connect(&mut self, peer: P) -> Option<P> {
        self.peer.replace(peer)
    }

    /// Disconnect from any connected peer (`connect(AF_UNSPEC)`), returning it.
    pub fn disconnect(&mut self) -> Option<P> {
        self.peer.take()
    }

    /// Currently connected peer, if any.
    pub const fn peer(&self) -> Option<&P> {
        self.peer.as_ref()
    }

    /// `true` if currently connected to a peer.
    pub const fn is_connected(&self) -> bool {
        self.peer.is_some()
    }

    /// Resolve the destination peer for `send` (`dest == None`) vs `sendto`
    /// (`dest == Some(target)`), matching Linux `net/unix/af_unix.c`
    /// `unix_dgram_sendmsg`:
    ///
    /// - If `dest == Some(d)`, returns `Ok(d)` (even when already connected).
    /// - If `dest == None`, returns the connected peer or
    ///   `Err(DgramTargetError::NotConnected)` (`-ENOTCONN`).
    pub fn resolve_send_target<'a>(&'a self, dest: Option<&'a P>) -> Result<&'a P, DgramTargetError> {
        match dest {
            Some(d) => Ok(d),
            None => self.peer.as_ref().ok_or(DgramTargetError::NotConnected),
        }
    }

    /// Linux `net/unix/af_unix.c` `unix_may_send` check on the **receiving**
    /// socket: returns `true` if this receiver is unconnected or connected to
    /// `sender`; returns `false` (`-EPERM`) if connected to a different peer.
    pub fn may_accept_from(&self, sender: &P) -> bool
    where
        P: PartialEq,
    {
        self.peer.as_ref().is_none_or(|p| p == sender)
    }
}

/// Error from [`SendBudget::try_charge`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendBudgetError {
    /// Datagram payload exceeds `limit - DGRAM_MAX_MSG_OVERHEAD` (`-EMSGSIZE`,
    /// Linux v6.12 `net/unix/af_unix.c:2015`).
    MessageTooLong,
    /// Sender's `sk_sndbuf` is already at or above its limit (`used >= limit`,
    /// `-EAGAIN`).
    WouldBlock,
}

/// Per-sender send-buffer byte accounting (`sk_wmem_alloc` against `sk_sndbuf`).
///
/// Follows Linux v6.12 `net/core/sock.c` `sock_alloc_send_pskb` and
/// `net/unix/af_unix.c:2015`:
/// - Refuses payloads above `limit - 32` with `-EMSGSIZE` first.
/// - Admits whenever `used < limit` and lets the charge overshoot `limit`
///   (so whenever [`SendBudget::writable`] holds, any datagram up to
///   [`SendBudget::max_msg_len`] is guaranteed admitted).
/// - Charges `payload_len + DGRAM_MAX_MSG_OVERHEAD` (32 B, a Lumen choice;
///   Linux charges `skb->truesize`).
/// - Reports `POLLOUT` via Linux's quarter rule (`used << 2 <= limit`).
///
/// Never allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendBudget {
    limit: usize,
    used: usize,
}

impl Default for SendBudget {
    fn default() -> Self {
        Self::new(DEFAULT_SNDBUF)
    }
}

impl SendBudget {
    /// Create a send-buffer budget with `limit` (`sk_sndbuf`) bytes.
    pub const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Configured `sk_sndbuf` byte limit.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Maximum single-datagram payload accepted without `-EMSGSIZE`
    /// (`limit.saturating_sub(DGRAM_MAX_MSG_OVERHEAD)`, Linux `af_unix.c:2015`).
    pub const fn max_msg_len(&self) -> usize {
        self.limit.saturating_sub(DGRAM_MAX_MSG_OVERHEAD)
    }

    /// Total `sk_wmem_alloc` bytes currently charged and in flight (may
    /// temporarily exceed `limit` by up to one admitted datagram, matching
    /// Linux `sock_alloc_send_pskb`).
    pub const fn used(&self) -> usize {
        self.used
    }

    /// Remaining headroom before `used >= limit` (`0` when full or overshot).
    pub const fn available(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// `true` if a datagram up to `max_msg_len()` will be admitted (`used < limit`).
    pub const fn can_admit(&self) -> bool {
        self.used < self.limit
    }

    /// `POLLOUT` readiness under Linux `unix_writable` (`used << 2 <= limit`).
    pub const fn writable(&self) -> bool {
        self.used <= self.limit / 4
    }

    /// Update `sk_sndbuf`. Per Linux `net/core/sock.c` `__sock_set_sndbuf`,
    /// the kernel doubles the user `SO_SNDBUF` value and clamps to
    /// `[MIN_SNDBUF, 2 * wmem_max]` before calling this.
    ///
    /// Returns `true` if [`SendBudget::writable`] transitioned from `false` to
    /// `true`.
    pub fn set_limit(&mut self, new_limit: usize) -> bool {
        let was_writable = self.writable();
        self.limit = new_limit;
        !was_writable && self.writable()
    }

    /// Charge a datagram of `payload_len` bytes (plus the fixed 32 B
    /// [`DGRAM_MAX_MSG_OVERHEAD`]) against this sender's send buffer.
    ///
    /// Checks `MessageTooLong` (`af_unix.c:2015`) before `WouldBlock`, then
    /// admits whenever `used < limit` and allows the charge to overshoot
    /// `limit` (`net/core/sock.c` `sock_alloc_send_pskb`).
    ///
    /// # Errors
    ///
    /// - [`SendBudgetError::MessageTooLong`] if `payload_len > self.max_msg_len()`.
    /// - [`SendBudgetError::WouldBlock`] if `self.used >= self.limit`.
    pub fn try_charge(&mut self, payload_len: usize) -> Result<(), SendBudgetError> {
        if payload_len > self.max_msg_len() {
            return Err(SendBudgetError::MessageTooLong);
        }
        if self.used >= self.limit {
            return Err(SendBudgetError::WouldBlock);
        }
        let cost = payload_len + DGRAM_MAX_MSG_OVERHEAD;
        self.used = self.used.saturating_add(cost);
        Ok(())
    }

    /// Credit back a dequeued or purged datagram of `payload_len` bytes (plus
    /// [`DGRAM_MAX_MSG_OVERHEAD`]).
    ///
    /// Returns `true` if [`SendBudget::writable`] transitioned from `false` to
    /// `true` under Linux's quarter watermark.
    pub fn release(&mut self, payload_len: usize) -> bool {
        let was_writable = self.writable();
        let cost = payload_len.saturating_add(DGRAM_MAX_MSG_OVERHEAD);
        debug_assert!(self.used >= cost, "SendBudget::release underflow: used={} < cost={cost}", self.used);
        self.used = self.used.saturating_sub(cost);
        !was_writable && self.writable()
    }
}

/// Pre-allocated per-datagram payload buffer, allocated outside any socket
/// lock so [`DgramQueue::send`] never allocates under lock and `DgramQueue`
/// imposes no artificial receiver-side byte cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DgramPayload {
    bytes: Vec<u8>,
}

impl DgramPayload {
    /// Fallibly allocate an exact-sized copy of `data`. For `data.is_empty()`,
    /// performs zero heap allocations.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_from_slice(data: &[u8]) -> Result<Self, NoMemory> {
        let mut bytes = Vec::new();
        if !data.is_empty() {
            bytes.try_reserve_exact(data.len()).map_err(|_| NoMemory)?;
            bytes.extend_from_slice(data);
        }
        Ok(Self { bytes })
    }

    /// Fallibly allocate an empty payload buffer with exact capacity for `cap`
    /// bytes so the kernel can copy directly from user memory into it outside
    /// any lock.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_with_capacity(cap: usize) -> Result<Self, NoMemory> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(cap).map_err(|_| NoMemory)?;
        Ok(Self { bytes })
    }

    /// Borrow the underlying payload bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable access to the underlying `Vec<u8>` (e.g. to fill pre-reserved
    /// capacity before sending).
    pub fn as_vec_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` if this is a zero-length datagram.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Unwrap the underlying byte vector.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

/// One queued, rejected, or undelivered datagram with its payload, optional
/// sender address `A`, sender budget handle `S`, writer credentials (`Ucred`),
/// and optional `Ancillary<F>`.
#[derive(Debug, PartialEq, Eq)]
pub struct Datagram<F, A, S = ()> {
    /// Pre-allocated payload bytes.
    pub payload: DgramPayload,
    /// Optional bound sender address (for `recvfrom` / `recvmsg` `msg_name`).
    pub addr: Option<A>,
    /// Sender identity handle to locate the [`SendBudget`] to refund.
    pub sender: S,
    /// Writer credentials (`UNIXCB(skb)`, overridden by explicit
    /// `SCM_CREDENTIALS` in `ancillary` when enqueued, per `unix_scm_to_skb`).
    pub cred: Ucred,
    /// Optional `SCM_RIGHTS` (and explicit `SCM_CREDENTIALS`) ancillary data.
    pub ancillary: Option<Ancillary<F>>,
}

impl<F, A, S> Datagram<F, A, S> {
    /// Construct a datagram to send on a [`DgramQueue`]. Never allocates.
    pub const fn new(
        payload: DgramPayload,
        addr: Option<A>,
        sender: S,
        cred: Ucred,
        ancillary: Option<Ancillary<F>>,
    ) -> Self {
        Self { payload, addr, sender, cred, ancillary }
    }

    /// Payload length in bytes (pass to `sender`'s [`SendBudget::release`]).
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// `true` if this is a zero-length datagram.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// Alias for an undelivered [`Datagram`] returned from [`DgramUndelivered`].
pub type UndeliveredDgram<F, A, S = ()> = Datagram<F, A, S>;

/// Value returned by a datagram receive (owned on consume, borrowed on peek)
/// so `MSG_PEEK` never requires `Clone` or heap allocation.
#[derive(Debug, PartialEq, Eq)]
pub enum RecvValue<'a, T> {
    /// Ownership transferred out of the queue (`peek == false`).
    Owned(T),
    /// Borrowed from the still-queued head datagram (`peek == true`).
    Peeked(&'a T),
}

impl<'a, T> RecvValue<'a, T> {
    /// Borrow the underlying value.
    pub const fn as_ref(&self) -> &T {
        match self {
            Self::Owned(v) => v,
            Self::Peeked(v) => v,
        }
    }

    /// Extract ownership from a consuming receive, or `None` if peeked.
    pub fn into_owned(self) -> Option<T> {
        match self {
            Self::Owned(v) => Some(v),
            Self::Peeked(_) => None,
        }
    }
}

/// Outcome of [`DgramQueue::send`] and [`DgramQueue::send_charged`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum DgramSendOutcome<F, A, S = ()> {
    /// The complete datagram of `n` bytes was enqueued.
    Sent(usize),
    /// Sender's [`SendBudget`] is full (`used >= limit`) or receiver queue is
    /// full for a non-mutual sender (`len > max_dgram_qlen`, `-EAGAIN`); the
    /// datagram is returned untouched.
    WouldBlock(Datagram<F, A, S>),
    /// Datagram exceeds the sender's `sndbuf - 32` limit (`-EMSGSIZE`, Linux
    /// `af_unix.c:2015`); the datagram is returned untouched.
    MessageTooLong(Datagram<F, A, S>),
    /// Receiver shut down reading via `SHUT_RD` / `SHUT_RDWR` (`-EPIPE`; only
    /// `SOCK_STREAM` raises `SIGPIPE`); the datagram is returned untouched.
    BrokenPipe(Datagram<F, A, S>),
    /// Receiver socket is closed (`-ECONNREFUSED`; a connected sender should
    /// disconnect its [`DgramPeer`] and purge its queue); the datagram is
    /// returned untouched.
    PeerClosed(Datagram<F, A, S>),
    /// The message-slot ring is at capacity while queueing for a mutually
    /// connected or self-sending peer (`peer_is_sender == true`), or after a
    /// [`DgramQueue::purge`] swapped out a non-empty slot ring.
    ///
    /// Kernel protocol: read [`DgramQueue::next_slot_capacity`], release the
    /// lock, allocate [`DgramSlotStorage::try_with_capacity`], retake the lock,
    /// swap in via [`DgramQueue::grow_slots`], retry `send`, and drop the old
    /// [`DgramSlotStorage`] outside the lock.
    NeedSlot(Datagram<F, A, S>),
}

/// Outcome of [`DgramQueue::recv`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum DgramRecvOutcome<'a, F, A, S = ()> {
    /// Received one datagram.
    Data {
        /// Bytes copied into `buf` (`buf.len().min(full_len)`).
        copied: usize,
        /// Original datagram length in bytes (returned to userspace when
        /// `MSG_TRUNC` is requested, and passed to [`SendBudget::release`] on
        /// the sender identified by `sender`).
        full_len: usize,
        /// `true` if `full_len > buf.len()` (kernel sets `MSG_TRUNC` in
        /// `msg_flags`).
        truncated: bool,
        /// Optional bound sender address (for `recvfrom` / `recvmsg` `msg_name`).
        addr: Option<RecvValue<'a, A>>,
        /// Sender identity handle so the kernel can refund the right
        /// [`SendBudget`] outside the receiver lock.
        sender: RecvValue<'a, S>,
        /// Writer credentials recorded on send (`UNIXCB(skb)`; overridden by
        /// explicit `SCM_CREDENTIALS` if supplied on send, per `unix_scm_to_skb`).
        cred: Ucred,
        /// Ancillary data attached to this datagram, if any.
        ancillary: Option<RecvAncillary<'a, F>>,
        /// `true` if `peek == false` and the queue transitioned from full
        /// (`len > max_dgram_qlen`) to accepting non-mutual senders.
        wake_writers: bool,
        /// Consumed payload buffer returned on `peek == false` so the kernel
        /// can drop (free) it **outside** the socket lock; `None` on `peek`.
        reclaimed: Option<DgramPayload>,
    },
    /// Pending `ECONNRESET` error (set by [`DgramQueue::set_reset`], e.g. from
    /// `unix_dgram_disconnected`), cleared and reported once regardless of
    /// `peek` (Linux `net/core/datagram.c` `__skb_try_recv_datagram` runs
    /// `sock_error`'s `xchg` before checking `MSG_PEEK` or queued skbs).
    Reset,
    /// Queue is empty and the receiver shut down reading.
    ///
    /// Per Linux `net/unix/af_unix.c`:
    /// - For `SOCK_DGRAM`, the kernel maps this by blocking mode (`0` blocking,
    ///   `-EAGAIN` when `O_NONBLOCK` / `MSG_DONTWAIT` is set).
    /// - For `SOCK_SEQPACKET` (which reuses `DgramQueue`), `Eof` returns `0`
    ///   even when non-blocking.
    Eof,
    /// Queue is empty and reading is still open (`-EAGAIN`).
    WouldBlock,
}

/// Pre-allocated message-slot storage for [`DgramQueue::grow_slots`],
/// allocated outside any socket lock when a mutually connected or self-sending
/// peer queues past `max_dgram_qlen` (or after a non-empty [`DgramQueue::purge`]).
pub struct DgramSlotStorage<F, A, S = ()> {
    deque: VecDeque<Datagram<F, A, S>>,
}

impl<F, A, S> fmt::Debug for DgramSlotStorage<F, A, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DgramSlotStorage")
            .field("len", &self.deque.len())
            .field("capacity", &self.deque.capacity())
            .finish()
    }
}

impl<F, A, S> DgramSlotStorage<F, A, S> {
    /// Allocate empty slot storage with capacity for at least `cap` datagrams.
    /// Call **outside** any socket lock.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_with_capacity(cap: usize) -> Result<Self, NoMemory> {
        Ok(Self { deque: try_deque(cap)? })
    }

    /// Usable message-slot capacity of this storage.
    pub fn capacity(&self) -> usize {
        self.deque.capacity()
    }
}

/// Undelivered datagrams detached by [`DgramQueue::close`] or
/// [`DgramQueue::purge`].
///
/// Returned without allocating so the kernel can release its lock before
/// closing undelivered file references, dropping payload buffers, and
/// crediting each sender's [`SendBudget`] via `item.sender` and `item.len()`.
#[must_use]
pub struct DgramUndelivered<F, A, S = ()> {
    msgs: VecDeque<Datagram<F, A, S>>,
    dropped_count: usize,
    fd_count: usize,
    wake_writers: bool,
}

impl<F, A, S> fmt::Debug for DgramUndelivered<F, A, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DgramUndelivered")
            .field("messages", &self.msgs.len())
            .field("dropped_count", &self.dropped_count)
            .field("fds", &self.fd_count)
            .field("wake_writers", &self.wake_writers)
            .finish()
    }
}

impl<F, A, S> DgramUndelivered<F, A, S> {
    const fn empty() -> Self {
        Self { msgs: VecDeque::new(), dropped_count: 0, fd_count: 0, wake_writers: false }
    }

    /// `true` if at least one queued datagram was dropped (preserved even
    /// after iterating, so C1 can decide whether to call `set_reset()` on an
    /// old mutually-connected peer per `unix_dgram_disconnected`).
    pub const fn had_dropped(&self) -> bool {
        self.dropped_count > 0
    }

    /// Total number of datagrams that were dropped when this batch was taken.
    pub const fn dropped_count(&self) -> usize {
        self.dropped_count
    }

    /// `true` if the `close` or `purge` transitioned `accepts(false)` from
    /// `false` to `true` (wake waiting writers / pollers).
    pub const fn wake_writers(&self) -> bool {
        self.wake_writers
    }

    /// Number of undelivered datagrams remaining to pop.
    pub fn len(&self) -> usize {
        self.msgs.len()
    }

    /// `true` if no undelivered datagrams remain to pop.
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// Total number of undelivered file references remaining.
    pub const fn fd_count(&self) -> usize {
        self.fd_count
    }

    /// Pop the next undelivered [`Datagram`] in FIFO order. Never allocates.
    pub fn pop_next(&mut self) -> Option<Datagram<F, A, S>> {
        let dgram = self.msgs.pop_front()?;
        if let Some(ref anc) = dgram.ancillary {
            self.fd_count -= anc.fd_count();
        }
        Some(dgram)
    }
}

impl<F, A, S> Iterator for DgramUndelivered<F, A, S> {
    type Item = Datagram<F, A, S>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop_next()
    }
}

/// Receive queue for an `AF_UNIX` `SOCK_DGRAM` (or `SOCK_SEQPACKET`) socket.
///
/// Allocates only `max_dgram_qlen + 1` message-metadata slots at creation (no
/// upfront 208 KiB byte ring); payloads are pre-allocated per message via
/// [`DgramPayload`] outside any socket lock, and the slot ring grows lazily
/// via [`DgramSendOutcome::NeedSlot`] + [`DgramQueue::grow_slots`] when a
/// mutually connected or self-sending peer queues past `max_dgram_qlen` (or
/// after a non-empty [`DgramQueue::purge`]).
pub struct DgramQueue<F, A, S = ()> {
    msgs: VecDeque<Datagram<F, A, S>>,
    max_dgram_qlen: usize,
    bytes_in_flight: usize,
    in_flight_fds: usize,
    recv_shutdown: bool,
    send_shutdown: bool,
    closed: bool,
    reset_pending: bool,
}

impl<F, A, S> fmt::Debug for DgramQueue<F, A, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DgramQueue")
            .field("messages", &self.msgs.len())
            .field("max_dgram_qlen", &self.max_dgram_qlen)
            .field("bytes_in_flight", &self.bytes_in_flight)
            .field("in_flight_fds", &self.in_flight_fds)
            .field("recv_shutdown", &self.recv_shutdown)
            .field("send_shutdown", &self.send_shutdown)
            .field("closed", &self.closed)
            .field("reset_pending", &self.reset_pending)
            .finish()
    }
}

impl<F, A, S> DgramQueue<F, A, S> {
    /// Create a datagram receive queue with Linux's `max_dgram_qlen` limit
    /// (allowing `max_dgram_qlen + 1` queued messages for non-mutual senders
    /// per `unix_recvq_full_lockless`; pass [`DEFAULT_MAX_DGRAM_QLEN`] for
    /// Linux's default `10`).
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if `max_dgram_qlen == usize::MAX` or slot-ring
    /// allocation fails.
    pub fn try_new(max_dgram_qlen: usize) -> Result<Self, NoMemory> {
        let slot_cap = max_dgram_qlen.checked_add(1).ok_or(NoMemory)?;
        let msgs = try_deque(slot_cap)?;
        Ok(Self {
            msgs,
            max_dgram_qlen,
            bytes_in_flight: 0,
            in_flight_fds: 0,
            recv_shutdown: false,
            send_shutdown: false,
            closed: false,
            reset_pending: false,
        })
    }

    /// Configured `max_dgram_qlen` (`sk_max_ack_backlog`).
    pub const fn max_dgram_qlen(&self) -> usize {
        self.max_dgram_qlen
    }

    /// Maximum number of datagrams accepted from non-mutual senders before
    /// `is_full()` (`max_dgram_qlen + 1`, matching Linux v6.12
    /// `net/unix/af_unix.c` `unix_recvq_full_lockless`:
    /// `skb_queue_len_lockless > sk_max_ack_backlog`).
    pub const fn max_messages(&self) -> usize {
        self.max_dgram_qlen + 1
    }

    /// Current allocated message-slot capacity before [`DgramSendOutcome::NeedSlot`].
    pub fn slot_capacity(&self) -> usize {
        self.msgs.capacity()
    }

    /// Number of queued datagrams.
    pub fn message_count(&self) -> usize {
        self.msgs.len()
    }

    /// `true` if no datagrams are queued.
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// `true` if the queue has reached Linux's `unix_recvq_full_lockless` limit
    /// (`message_count() > max_dgram_qlen()`).
    pub fn is_full(&self) -> bool {
        self.msgs.len() > self.max_dgram_qlen
    }

    /// Total payload bytes currently queued across all datagrams.
    pub const fn bytes_in_flight(&self) -> usize {
        self.bytes_in_flight
    }

    /// Length of the next datagram to be received (`0` if empty), matching
    /// Linux `SIOCINQ` / `FIONREAD` on `SOCK_DGRAM` (`unix_ioctl`).
    pub fn fionread(&self) -> usize {
        self.msgs.front().map_or(0, Datagram::len)
    }

    /// Length of the next datagram, or `None` if the queue is empty.
    pub fn next_len(&self) -> Option<usize> {
        self.msgs.front().map(Datagram::len)
    }

    /// Total undelivered `SCM_RIGHTS` file references currently queued.
    pub const fn in_flight_fds(&self) -> usize {
        self.in_flight_fds
    }

    /// Visit every in-flight `SCM_RIGHTS` file reference currently queued in
    /// FIFO order, without draining or allocating.
    ///
    /// Intended for the kernel's `AF_UNIX` garbage collector (`unix_gc`).
    pub fn for_each_queued_fd(&self, mut f: impl FnMut(&F)) {
        for dgram in &self.msgs {
            if let Some(ref anc) = dgram.ancillary {
                for fd in &anc.fds {
                    f(fd);
                }
            }
        }
    }

    /// Recommended capacity for [`DgramSlotStorage::try_with_capacity`] after
    /// `send` / `send_charged` returned [`DgramSendOutcome::NeedSlot`].
    /// Never allocates.
    pub fn next_slot_capacity(&self) -> usize {
        let base = self.max_dgram_qlen.saturating_add(1).max(MIN_GROW_SLOT_CAPACITY);
        self.msgs.capacity().saturating_mul(2).max(base)
    }

    /// Install a pre-allocated [`DgramSlotStorage`] without allocating or
    /// freeing under lock, returning the previous (now empty) storage — or
    /// `spare` unchanged if not larger — to drop outside the lock.
    pub fn grow_slots(&mut self, mut spare: DgramSlotStorage<F, A, S>) -> DgramSlotStorage<F, A, S> {
        grow_deque(&mut self.msgs, &mut spare.deque);
        spare
    }

    /// Enqueue a pre-allocated datagram under **only** the receiver socket's
    /// lock (recommended lock-order-free protocol):
    ///
    /// 1. Under sender lock: check `sender.queue.send_shutdown()` (`-EPIPE` if
    ///    true), then `sender.budget.try_charge(data.len())?`. Release.
    /// 2. Outside locks: `DgramPayload::try_from_slice(data)?`.
    /// 3. Under receiver lock: `receiver.queue.send(dgram, peer_is_sender)`.
    ///    On any non-`Sent` outcome other than retrying `NeedSlot`, release
    ///    the receiver lock and roll back `sender.budget.release(dgram.len())`
    ///    under the sender lock.
    ///
    /// Set `peer_is_sender = true` when the receiver is the sender itself or
    /// is connected back to the sender (`other == sk || unix_peer(other) == sk`,
    /// Linux `af_unix.c` `unix_dgram_sendmsg`) to bypass `max_dgram_qlen`.
    ///
    /// Never allocates.
    pub fn send(&mut self, dgram: Datagram<F, A, S>, peer_is_sender: bool) -> DgramSendOutcome<F, A, S> {
        self.send_inner(None, dgram, peer_is_sender)
    }

    /// Enqueue a pre-allocated datagram while checking and charging `budget`
    /// atomically in Linux `unix_dgram_sendmsg` error order:
    /// `MessageTooLong` (`af_unix.c:2015`) -> budget `WouldBlock`
    /// (`sock_alloc_send_pskb`) -> receiver `PeerClosed` / `BrokenPipe` /
    /// queue `WouldBlock` / `NeedSlot`, committing the charge only on success
    /// so `budget` is never modified on refusal.
    ///
    /// The kernel checks the sender's own `SHUT_WR` (`sender.queue.send_shutdown()`
    /// -> `-EPIPE`) before calling.
    ///
    /// **Locking note:** if `budget` and `self` live behind separate per-socket
    /// locks, acquire the two socket locks in a canonical order (e.g.
    /// ascending socket address, matching Linux's `unix_state_double_lock`) to
    /// prevent AB-BA deadlock — or prefer the single-lock reserve-then-`send`
    /// protocol documented on [`DgramQueue::send`].
    ///
    /// Never allocates.
    pub fn send_charged(
        &mut self,
        budget: &mut SendBudget,
        dgram: Datagram<F, A, S>,
        peer_is_sender: bool,
    ) -> DgramSendOutcome<F, A, S> {
        self.send_inner(Some(budget), dgram, peer_is_sender)
    }

    fn send_inner(
        &mut self,
        budget: Option<&mut SendBudget>,
        mut dgram: Datagram<F, A, S>,
        peer_is_sender: bool,
    ) -> DgramSendOutcome<F, A, S> {
        dgram.ancillary = dgram.ancillary.filter(|a| !a.is_empty());
        if let Some(explicit) = dgram.ancillary.as_ref().and_then(|a| a.cred) {
            dgram.cred = explicit;
        }

        // Linux v6.12 net/unix/af_unix.c:2015 checks `len > sk->sk_sndbuf - 32`
        // (-EMSGSIZE), then `sock_alloc_send_pskb` checks `wmem < sndbuf`
        // (-EAGAIN), before checking receiver state or queue fullness.
        if let Some(ref b) = budget {
            if dgram.len() > b.max_msg_len() {
                return DgramSendOutcome::MessageTooLong(dgram);
            }
            if !b.can_admit() {
                return DgramSendOutcome::WouldBlock(dgram);
            }
        }
        if self.closed {
            return DgramSendOutcome::PeerClosed(dgram);
        }
        if self.recv_shutdown {
            return DgramSendOutcome::BrokenPipe(dgram);
        }
        if !peer_is_sender && self.is_full() {
            return DgramSendOutcome::WouldBlock(dgram);
        }
        if self.msgs.len() == self.msgs.capacity() {
            return DgramSendOutcome::NeedSlot(dgram);
        }

        let len = dgram.len();
        if let Some(b) = budget {
            let res = b.try_charge(len);
            debug_assert!(res.is_ok(), "pre-checked max_msg_len and can_admit");
        }
        self.bytes_in_flight += len;
        self.in_flight_fds += dgram.ancillary.as_ref().map_or(0, Ancillary::fd_count);
        debug_assert!(self.msgs.len() < self.msgs.capacity());
        self.msgs.push_back(dgram);
        DgramSendOutcome::Sent(len)
    }

    /// Receive or peek the head datagram into `buf` (Linux v6.12
    /// `net/unix/af_unix.c` `unix_dgram_recvmsg` and `net/core/datagram.c`
    /// `__skb_try_recv_datagram`).
    ///
    /// - If `reset_pending` is set, clears and returns [`DgramRecvOutcome::Reset`]
    ///   once regardless of `peek` (`sock_error`'s `xchg` runs before
    ///   `MSG_PEEK` and before dequeuing skbs).
    /// - Preserves message boundaries (including 0-byte datagrams).
    /// - Copies `buf.len().min(full_len)` bytes into `buf`, reports `full_len`
    ///   (for `MSG_TRUNC`), `truncated = full_len > buf.len()`, and `cred`.
    /// - When `peek == false`, consumes the entire datagram and returns its
    ///   `DgramPayload` in `reclaimed` to drop outside the socket lock, plus
    ///   `sender` so the kernel can refund that sender's [`SendBudget`] with
    ///   `full_len`.
    /// - When `peek == true`, leaves the datagram queued and borrows `addr`,
    ///   `sender`, and `ancillary` without allocating.
    ///
    /// Never allocates.
    pub fn recv<'a>(&'a mut self, buf: &mut [u8], peek: bool) -> DgramRecvOutcome<'a, F, A, S> {
        if self.reset_pending {
            self.reset_pending = false;
            return DgramRecvOutcome::Reset;
        }
        if self.msgs.is_empty() {
            return if self.recv_shutdown { DgramRecvOutcome::Eof } else { DgramRecvOutcome::WouldBlock };
        }

        if peek {
            let front = self.msgs.front().expect("checked non-empty above");
            let full_len = front.len();
            let copied = buf.len().min(full_len);
            let truncated = full_len > buf.len();
            if copied > 0 {
                buf[..copied].copy_from_slice(&front.payload.as_slice()[..copied]);
            }
            return DgramRecvOutcome::Data {
                copied,
                full_len,
                truncated,
                addr: front.addr.as_ref().map(RecvValue::Peeked),
                sender: RecvValue::Peeked(&front.sender),
                cred: front.cred,
                ancillary: front.ancillary.as_ref().map(RecvAncillary::Peeked),
                wake_writers: false,
                reclaimed: None,
            };
        }

        let was_full = self.is_full();
        let dgram = self.msgs.pop_front().expect("checked non-empty above");
        let full_len = dgram.len();
        let copied = buf.len().min(full_len);
        let truncated = full_len > buf.len();
        if copied > 0 {
            buf[..copied].copy_from_slice(&dgram.payload.as_slice()[..copied]);
        }
        self.bytes_in_flight -= full_len;
        if let Some(ref anc) = dgram.ancillary {
            self.in_flight_fds -= anc.fd_count();
        }
        let wake_writers = was_full && !self.is_full();

        DgramRecvOutcome::Data {
            copied,
            full_len,
            truncated,
            addr: dgram.addr.map(RecvValue::Owned),
            sender: RecvValue::Owned(dgram.sender),
            cred: dgram.cred,
            ancillary: dgram.ancillary.map(RecvAncillary::Owned),
            wake_writers,
            reclaimed: Some(dgram.payload),
        }
    }

    /// Record a pending `ECONNRESET` error on this datagram socket (e.g. from
    /// Linux `unix_dgram_disconnected` when a mutually connected peer
    /// disconnects or reconnects while dropping unread datagrams).
    /// Never allocates.
    pub fn set_reset(&mut self) {
        self.reset_pending = true;
    }

    /// Read and clear a pending `ECONNRESET` error (`getsockopt(SO_ERROR)` /
    /// Linux `sock_error(sk)`). Returns `true` if a reset was pending.
    /// Never allocates.
    pub fn take_error(&mut self) -> bool {
        mem::take(&mut self.reset_pending)
    }

    /// `POLLERR`: `true` if a reset error is pending. Never allocates.
    pub const fn err(&self) -> bool {
        self.reset_pending
    }

    /// Apply `shutdown(how)` to this datagram socket (`SHUT_RD`, `SHUT_WR`,
    /// `SHUT_RDWR`).
    ///
    /// Returns `true` if `accepts(false)` transitioned from `false` to `true`
    /// (queue was full and `SHUT_RD` / `SHUT_RDWR` just turned on, so waiting
    /// writers / pollers must wake to observe `-EPIPE`).
    ///
    /// `SHUT_RD` / `SHUT_RDWR` stops incoming sends (`BrokenPipe`) while
    /// leaving queued datagrams readable. `SHUT_WR` / `SHUT_RDWR` sets
    /// [`DgramQueue::send_shutdown`]. `SHUT_RDWR` (or both `SHUT_RD` and
    /// `SHUT_WR`) sets [`DgramQueue::hup`] (`POLLHUP`), matching Linux
    /// `net/unix/af_unix.c` `unix_dgram_poll`. Never allocates.
    pub fn shutdown(&mut self, how: ShutdownHow) -> bool {
        let was_accepting = self.accepts(false);
        if how.shuts_read() {
            self.recv_shutdown = true;
        }
        if how.shuts_write() {
            self.send_shutdown = true;
        }
        !was_accepting && self.accepts(false)
    }

    /// Whether this socket has shut down its own sending side (`SHUT_WR` /
    /// `SHUT_RDWR` / `close`); the kernel checks this on the sender before
    /// sending and returns `-EPIPE`. Never allocates.
    pub const fn send_shutdown(&self) -> bool {
        self.send_shutdown
    }

    /// Empty this datagram receive queue without closing or shutting it down,
    /// returning every dropped datagram in [`DgramUndelivered`] without
    /// allocating (used on reconnect / `AF_UNSPEC` disconnect, dead-peer send,
    /// and `unix_gc`).
    ///
    /// Note: when non-empty, swaps out the slot ring without allocating so the
    /// next send uses the existing `NeedSlot` / `grow_slots` protocol; when
    /// already empty, preserves existing slot capacity and returns an empty
    /// [`DgramUndelivered`] with `had_dropped() == false`.
    pub fn purge(&mut self) -> DgramUndelivered<F, A, S> {
        if self.msgs.is_empty() {
            return DgramUndelivered::empty();
        }
        let was_accepting = self.accepts(false);
        let msgs = mem::take(&mut self.msgs);
        let dropped_count = msgs.len();
        let fd_count = self.in_flight_fds;
        self.bytes_in_flight = 0;
        self.in_flight_fds = 0;
        let wake_writers = !was_accepting && self.accepts(false);
        DgramUndelivered { msgs, dropped_count, fd_count, wake_writers }
    }

    /// Close this datagram receive queue, calling `shutdown(ShutdownHow::Both)`
    /// and purging every queued datagram — returning them without allocating
    /// so the kernel can refund budgets, close fds, and drop payloads outside
    /// the lock. [`DgramUndelivered::wake_writers`] is `true` if `accepts(false)`
    /// transitioned from `false` to `true`.
    ///
    /// Idempotent. Never allocates.
    pub fn close(&mut self) -> DgramUndelivered<F, A, S> {
        let already_closed = self.closed;
        let was_accepting = self.accepts(false);
        self.closed = true;
        let _ = self.shutdown(ShutdownHow::Both);
        if already_closed {
            return DgramUndelivered::empty();
        }
        let msgs = mem::take(&mut self.msgs);
        let dropped_count = msgs.len();
        let fd_count = self.in_flight_fds;
        self.bytes_in_flight = 0;
        self.in_flight_fds = 0;
        let wake_writers = !was_accepting && self.accepts(false);
        DgramUndelivered { msgs, dropped_count, fd_count, wake_writers }
    }

    /// Whether [`DgramQueue::close`] has been called.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// `POLLIN | POLLRDNORM`: `true` if a datagram is queued or reading was
    /// shut down (`unix_dgram_poll`). Never allocates.
    pub fn readable(&self) -> bool {
        !self.msgs.is_empty() || self.recv_shutdown
    }

    /// `POLLOUT` peer-queue gate matching Linux v6.12 `net/unix/af_unix.c`
    /// `unix_dgram_poll`: withholds `POLLOUT` **only** when the receiver is
    /// not connected back to the sender, is still open/reading, and its queue
    /// is full (`!peer_is_sender && !closed && !recv_shutdown && is_full()`).
    ///
    /// A closed or `SHUT_RD` peer still returns `true` so a writer polling
    /// `POLLOUT` wakes up and observes `-ECONNREFUSED` / `-EPIPE` on `send`.
    /// A sending datagram socket's `POLLOUT` combines its own
    /// [`SendBudget::writable`] with the connected peer's `accepts(peer_is_sender)`.
    /// Never allocates.
    pub fn accepts(&self, peer_is_sender: bool) -> bool {
        peer_is_sender || self.closed || self.recv_shutdown || !self.is_full()
    }

    /// `POLLRDHUP`: `true` if reading was shut down or closed (`unix_dgram_poll`:
    /// `sk->sk_shutdown & RCV_SHUTDOWN`). Never allocates.
    pub const fn rdhup(&self) -> bool {
        self.recv_shutdown
    }

    /// `POLLHUP`: `true` if both directions were shut down (`SHUT_RDWR`) or
    /// closed (`unix_dgram_poll`: `sk->sk_shutdown == SHUTDOWN_MASK`).
    /// Never allocates.
    pub const fn hup(&self) -> bool {
        self.recv_shutdown && self.send_shutdown
    }
}
