//! `AF_UNIX` listening backlog and connection hand-off (`Listener`).
//!
//! Models Linux v6.12 `net/socket.c` `__sys_listen_socket` and
//! `net/unix/af_unix.c` `unix_listen`, `unix_stream_connect`, `unix_accept`,
//! `unix_poll`, and `unix_release_sock`:
//! - `backlog` clamped as Linux's `somaxconn` (`(unsigned int)backlog > somaxconn`);
//! - `unix_recvq_full_lockless`: `pending > backlog` (so `backlog = N` accepts
//!   `N + 1` pending connections, and `listen(fd, 0)` permits 1 pending
//!   connection);
//! - FIFO `accept`;
//! - full backlog returns `WouldBlock` (`-EAGAIN` if nonblocking, or sleep on
//!   `unix_wait_for_peer`), closed listener returns `Refused` (`-ECONNREFUSED`);
//! - growing the backlog uses a non-allocating swap API ([`ListenerStorage`] +
//!   [`Listener::grow_storage`]) so `listen` never allocates under a socket lock;
//! - `close` hands back every unaccepted (`embrion`) connection in FIFO order
//!   without allocating so the kernel resets each server end via
//!   [`StreamPair::close_reset`](crate::StreamPair::close_reset) and closes any
//!   queued fds outside the lock.

use alloc::collections::VecDeque;
use alloc::collections::vec_deque::Iter;
use core::fmt;
use core::mem;

use crate::ring::{grow_deque, try_deque};
use crate::{NoMemory, Readiness};

/// Linux default `net.core.somaxconn` (`include/linux/socket.h` since Linux 5.4).
pub const SOMAXCONN: usize = 4096;

/// Clamp a `listen(2)` `backlog` argument against `somaxconn`, matching Linux
/// v6.12 `net/socket.c` `__sys_listen_socket`:
///
/// ```c
/// if ((unsigned int)backlog > somaxconn)
///     backlog = somaxconn;
/// ```
///
/// Because `backlog` is cast to `unsigned int` before comparing, any negative
/// `backlog` (e.g. `listen(fd, -1)`) clamps to `somaxconn`, while `0` remains
/// `0`.
pub const fn clamp_backlog(backlog: i32, somaxconn: usize) -> usize {
    let raw = backlog.cast_unsigned() as usize;
    if raw > somaxconn { somaxconn } else { raw }
}

/// Error returned by [`Listener::listen`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenError {
    /// Listener has already been closed (`-EINVAL`).
    Closed,
    /// Raising `backlog` requires more queue slots than currently allocated;
    /// allocate [`ListenerStorage::try_for_backlog`] (with capacity at least
    /// `needed`) outside the lock, install via [`Listener::grow_storage`],
    /// retry `listen`, and drop the old storage outside the lock.
    NeedStorage(usize),
    /// Clamped `backlog + 1` overflowed `usize`.
    Overflow,
}

/// Pre-allocated backlog storage for [`Listener::grow_storage`], allocated
/// outside any socket lock.
pub struct ListenerStorage<C> {
    deque: VecDeque<C>,
}

impl<C> fmt::Debug for ListenerStorage<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListenerStorage")
            .field("len", &self.deque.len())
            .field("capacity", &self.deque.capacity())
            .finish()
    }
}

impl<C> ListenerStorage<C> {
    /// Fallibly allocate empty backlog storage sized for `backlog` clamped by
    /// `somaxconn` (`clamped + 1` slots). Call **outside** any socket lock.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if `clamped + 1` overflows or allocation fails.
    pub fn try_for_backlog(backlog: i32, somaxconn: usize) -> Result<Self, NoMemory> {
        let clamped = clamp_backlog(backlog, somaxconn);
        let cap = clamped.checked_add(1).ok_or(NoMemory)?;
        Self::try_with_capacity(cap)
    }

    /// Fallibly allocate empty backlog storage with at least `cap` slots.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_with_capacity(cap: usize) -> Result<Self, NoMemory> {
        Ok(Self { deque: try_deque(cap)? })
    }

    /// Usable slot capacity of this storage.
    pub fn capacity(&self) -> usize {
        self.deque.capacity()
    }
}

/// Outcome of [`Listener::connect`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectOutcome<C> {
    /// Connection was enqueued on the listener's accept queue; wake waiting
    /// acceptors and pollers (`POLLIN`).
    Queued,
    /// Backlog is full (`pending_len() > backlog()`); connection is returned
    /// untouched (`-EAGAIN` if nonblocking, else sleep on `unix_wait_for_peer`).
    WouldBlock(C),
    /// Listener has been closed (`-ECONNREFUSED`); connection is returned
    /// untouched.
    Refused(C),
}

/// Outcome of [`Listener::accept`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum AcceptOutcome<C> {
    /// Popped the oldest pending connection in FIFO order.
    Accepted {
        /// The accepted pending connection.
        conn: C,
        /// `true` if the backlog transitioned from full to accepting new
        /// connections (wake waiting `connect`ers).
        wake_connecters: bool,
    },
    /// No pending connections queued right now (`-EAGAIN` if nonblocking, else
    /// wait for a connection or close).
    WouldBlock,
    /// Listener has been closed (`-EINVAL`).
    Closed,
}

/// Every unaccepted (`embrion`) pending connection detached from a [`Listener`]
/// by [`Listener::close`].
///
/// Returned without allocating so the kernel can release the listener's lock,
/// then call [`StreamPair::close_reset`](crate::StreamPair::close_reset) on
/// each connection's server end — resetting the waiting client peer
/// (`ECONNRESET`) and reclaiming any `SCM_RIGHTS` fds the client already sent
/// before `accept`.
#[must_use]
pub struct PendingConnections<C> {
    queue: VecDeque<C>,
}

impl<C> fmt::Debug for PendingConnections<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingConnections").field("len", &self.queue.len()).finish()
    }
}

impl<C> PendingConnections<C> {
    const fn empty() -> Self {
        Self { queue: VecDeque::new() }
    }

    /// Number of pending connections remaining.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// `true` if no pending connections remain.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl<C> Iterator for PendingConnections<C> {
    type Item = C;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop_front()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.queue.len();
        (len, Some(len))
    }
}

impl<C> ExactSizeIterator for PendingConnections<C> {}

impl<C> DoubleEndedIterator for PendingConnections<C> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.queue.pop_back()
    }
}

/// `AF_UNIX` listening socket accept queue.
///
/// Pre-allocates its backlog storage at creation and grows via
/// [`ListenerStorage`] + [`Listener::grow_storage`], so [`Listener::listen`],
/// [`Listener::connect`], [`Listener::accept`], [`Listener::close`],
/// [`Listener::pending`], and all readiness checks perform **zero** heap
/// allocations under lock.
pub struct Listener<C> {
    queue: VecDeque<C>,
    backlog: usize,
    closed: bool,
}

impl<C> fmt::Debug for Listener<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("pending", &self.queue.len())
            .field("backlog", &self.backlog)
            .field("closed", &self.closed)
            .finish()
    }
}

impl<C> Listener<C> {
    /// Start listening with `backlog` clamped against [`SOMAXCONN`].
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if reserving the backlog queue fails.
    pub fn try_new(backlog: i32) -> Result<Self, NoMemory> {
        Self::try_new_with_somaxconn(backlog, SOMAXCONN)
    }

    /// Start listening with `backlog` clamped against a custom `somaxconn`.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if `clamped + 1` overflows or allocation fails.
    pub fn try_new_with_somaxconn(backlog: i32, somaxconn: usize) -> Result<Self, NoMemory> {
        let clamped = clamp_backlog(backlog, somaxconn);
        let storage = ListenerStorage::try_for_backlog(backlog, somaxconn)?;
        Ok(Self { queue: storage.deque, backlog: clamped, closed: false })
    }

    /// Install a pre-allocated [`ListenerStorage`] without allocating or
    /// freeing under lock, moving all pending connections in FIFO order and
    /// returning the previous (now empty) storage — or `spare` unchanged if
    /// not larger — to drop outside the lock.
    pub fn grow_storage(&mut self, mut spare: ListenerStorage<C>) -> ListenerStorage<C> {
        grow_deque(&mut self.queue, &mut spare.deque);
        spare
    }

    /// Update the listen backlog (`listen(2)` called on an already-listening
    /// socket), clamped against `somaxconn`. Never allocates.
    ///
    /// - If raising `backlog` requires more slots than currently reserved,
    ///   returns [`ListenError::NeedStorage`] with the required capacity so
    ///   the kernel can allocate [`ListenerStorage::try_for_backlog`] outside
    ///   the lock, swap it in via [`Listener::grow_storage`], and retry.
    /// - Shrinking never drops already-queued pending connections; new
    ///   connects wait until `pending_len() <= new_backlog`.
    /// - Returns `Ok(true)` if the backlog transitioned from full to accepting
    ///   new connections (wake waiting connecters).
    ///
    /// # Errors
    ///
    /// - [`ListenError::Closed`] if the listener is already closed.
    /// - [`ListenError::NeedStorage`] if storage growth is required.
    /// - [`ListenError::Overflow`] if `clamped + 1` overflows `usize`.
    pub fn listen(&mut self, backlog: i32, somaxconn: usize) -> Result<bool, ListenError> {
        if self.closed {
            return Err(ListenError::Closed);
        }
        let was_full = self.is_full();
        let clamped = clamp_backlog(backlog, somaxconn);
        let needed_cap = clamped.checked_add(1).ok_or(ListenError::Overflow)?;
        if needed_cap > self.queue.capacity() {
            return Err(ListenError::NeedStorage(needed_cap));
        }
        self.backlog = clamped;
        Ok(was_full && !self.is_full())
    }

    /// Configured clamped backlog (`sk_max_ack_backlog`).
    pub const fn backlog(&self) -> usize {
        self.backlog
    }

    /// Maximum number of pending connections accepted before `is_full()`
    /// (`backlog + 1`, matching Linux v6.12 `net/unix/af_unix.c`
    /// `unix_recvq_full_lockless`: `skb_queue_len_lockless > sk_max_ack_backlog`).
    pub const fn max_pending(&self) -> usize {
        self.backlog + 1
    }

    /// Number of pending connections currently queued.
    pub fn pending_len(&self) -> usize {
        self.queue.len()
    }

    /// `true` if no pending connections are queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// `true` if the backlog is full (`pending_len() > backlog()`, Linux
    /// `unix_recvq_full_lockless`).
    pub fn is_full(&self) -> bool {
        self.queue.len() > self.backlog
    }

    /// `true` if [`Listener::close`] has been called.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Borrow an iterator over currently queued (unaccepted) pending
    /// connections in FIFO order without draining or allocating.
    ///
    /// Intended for the kernel's `AF_UNIX` garbage collector (`unix_gc`) to
    /// scan in-flight fds inside unaccepted (`embrion`) sockets.
    pub fn pending(&self) -> Iter<'_, C> {
        self.queue.iter()
    }

    /// Enqueue a new incoming connection (`unix_stream_connect`).
    ///
    /// Never allocates.
    pub fn connect(&mut self, conn: C) -> ConnectOutcome<C> {
        if self.closed {
            return ConnectOutcome::Refused(conn);
        }
        if self.is_full() {
            return ConnectOutcome::WouldBlock(conn);
        }
        debug_assert!(self.queue.len() < self.queue.capacity());
        self.queue.push_back(conn);
        ConnectOutcome::Queued
    }

    /// Dequeue the oldest pending connection in FIFO order (`unix_accept`).
    ///
    /// Never allocates.
    pub fn accept(&mut self) -> AcceptOutcome<C> {
        if self.closed {
            return AcceptOutcome::Closed;
        }
        let was_full = self.is_full();
        match self.queue.pop_front() {
            Some(conn) => AcceptOutcome::Accepted { conn, wake_connecters: was_full && !self.is_full() },
            None => AcceptOutcome::WouldBlock,
        }
    }

    /// Close the listener and hand back every unaccepted (`embrion`) pending
    /// connection in FIFO order so the kernel can call
    /// [`StreamPair::close_reset`](crate::StreamPair::close_reset) on each
    /// server end (resetting waiting clients and reclaiming any queued fds)
    /// outside any lock.
    ///
    /// Idempotent. Never allocates.
    pub fn close(&mut self) -> PendingConnections<C> {
        let already_closed = self.closed;
        self.closed = true;
        if already_closed {
            return PendingConnections::empty();
        }
        let queue = mem::take(&mut self.queue);
        PendingConnections { queue }
    }

    /// `POLLIN | POLLRDNORM`: `true` if a connection is ready to `accept` or
    /// the listener has been closed. Never allocates.
    pub fn readable(&self) -> bool {
        !self.queue.is_empty() || self.closed
    }

    /// `POLLOUT`: always `false` for a listening socket (Linux v6.12
    /// `net/unix/af_unix.c` `unix_writable`: `sk->sk_state != TCP_LISTEN`).
    /// Never allocates.
    pub const fn writable(&self) -> bool {
        false
    }

    /// `POLLHUP`: `true` if the listener has been closed. Never allocates.
    pub const fn hup(&self) -> bool {
        self.closed
    }

    /// Combined readiness snapshot for this listener. Never allocates.
    pub fn readiness(&self) -> Readiness {
        Readiness {
            readable: self.readable(),
            writable: self.writable(),
            rdhup: self.closed,
            hup: self.hup(),
            err: false,
        }
    }
}
