//! Ancillary control data (`SCM_RIGHTS` and `SCM_CREDENTIALS`) for `AF_UNIX`.

use alloc::vec::{Drain, Vec};
use core::mem;

use crate::NoMemory;

/// Maximum number of file descriptors in a single `SCM_RIGHTS` control message
/// (Linux v6.12 `include/net/scm.h`: `#define SCM_MAX_FD 253`).
///
/// The kernel validates incoming `SCM_RIGHTS` cmsgs against `SCM_MAX_FD`
/// (returning `-EINVAL` above it) before constructing an [`Ancillary`].
pub const SCM_MAX_FD: usize = 253;

/// Unix socket credentials (`struct ucred` in Linux v6.12 `include/linux/socket.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ucred {
    /// Process ID (`pid_t`).
    pub pid: i32,
    /// User ID (`uid_t`).
    pub uid: u32,
    /// Group ID (`gid_t`).
    pub gid: u32,
}

impl Ucred {
    /// Construct a credential triple.
    pub const fn new(pid: i32, uid: u32, gid: u32) -> Self {
        Self { pid, uid, gid }
    }
}

/// Control data carried alongside a stream write or datagram: passed file
/// references (`SCM_RIGHTS`) and optional explicit `SCM_CREDENTIALS`.
///
/// Undelivered `Ancillary` values should be explicitly drained via
/// [`Ancillary::drain_fds`] (or returned from [`StreamPair::close`](crate::StreamPair::close) /
/// [`DgramQueue::close`](crate::DgramQueue::close)) so the kernel can close
/// every file reference outside any socket lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ancillary<F> {
    /// Passed file references (`SCM_RIGHTS`).
    pub fds: Vec<F>,
    /// Optional explicit `SCM_CREDENTIALS` override, if supplied on `sendmsg`.
    pub cred: Option<Ucred>,
}

impl<F> Default for Ancillary<F> {
    fn default() -> Self {
        Self { fds: Vec::new(), cred: None }
    }
}

impl<F> Ancillary<F> {
    /// Construct ancillary data from a vector of file references and optional
    /// credentials. Never allocates.
    pub const fn new(fds: Vec<F>, cred: Option<Ucred>) -> Self {
        Self { fds, cred }
    }

    /// Construct ancillary data carrying only file references (`SCM_RIGHTS`).
    /// Never allocates.
    pub const fn from_fds(fds: Vec<F>) -> Self {
        Self { fds, cred: None }
    }

    /// Construct ancillary data carrying only credentials (`SCM_CREDENTIALS`).
    /// Never allocates.
    pub const fn from_cred(cred: Ucred) -> Self {
        Self { fds: Vec::new(), cred: Some(cred) }
    }

    /// Fallibly allocate an empty `Ancillary` with exact capacity for `fd_cap`
    /// file references so subsequent pushes up to `fd_cap` cannot fail or
    /// reallocate.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if `try_reserve_exact(fd_cap)` fails.
    pub fn try_with_fd_capacity(fd_cap: usize, cred: Option<Ucred>) -> Result<Self, NoMemory> {
        let mut fds = Vec::new();
        fds.try_reserve_exact(fd_cap).map_err(|_| NoMemory)?;
        Ok(Self { fds, cred })
    }

    /// Whether neither file references nor credentials are present.
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty() && self.cred.is_none()
    }

    /// Whether at least one `SCM_RIGHTS` file reference is present.
    pub fn has_fds(&self) -> bool {
        !self.fds.is_empty()
    }

    /// Number of `SCM_RIGHTS` file references carried.
    pub fn fd_count(&self) -> usize {
        self.fds.len()
    }

    /// Drain all carried file references in FIFO order so the kernel can close
    /// them. Never allocates.
    pub fn drain_fds(&mut self) -> Drain<'_, F> {
        self.fds.drain(..)
    }

    /// Split carried file references at `max`: the first `max.min(fd_count)`
    /// fit into the receiver's `cmsg` buffer and the remainder must be closed
    /// and flagged with `MSG_CTRUNC` (matching Linux `net/core/scm.c`
    /// `scm_detach_fds`).
    ///
    /// Leaves `self.cred` intact and empties `self.fds`. Performs **zero**
    /// heap allocations.
    pub fn take_fds(&mut self, max: usize) -> TakenFds<F> {
        let fds = mem::take(&mut self.fds);
        let deliver_len = max.min(fds.len());
        let truncated = deliver_len < fds.len();
        TakenFds { fds, deliver_len, truncated }
    }
}

/// Result of [`Ancillary::take_fds`], separating delivered file references from
/// truncated excess references without allocating.
///
/// For fallible fd installation (where installing the `k`-th descriptor can
/// fail with `-EMFILE`), pop one item at a time with [`TakenFds::next_delivered`]:
/// breaking out early leaves all remaining delivered and truncated references
/// intact for [`TakenFds::drain_all`] to reclaim and close. When installation
/// cannot exit early, [`TakenFds::drain_delivered`] and [`TakenFds::drain_dropped`]
/// may be called in either order (note that dropping a `Drain` early drops any
/// uniterated items in its range). [`TakenFds::truncated`] remembers whether
/// `take_fds` truncated (`MSG_CTRUNC`) even after items are drained.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct TakenFds<F> {
    fds: Vec<F>,
    deliver_len: usize,
    truncated: bool,
}

impl<F> TakenFds<F> {
    /// `true` if more file references arrived than `max` allowed (`MSG_CTRUNC`).
    /// Preserved even after draining.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Number of file references remaining to deliver.
    pub const fn delivered_len(&self) -> usize {
        self.deliver_len
    }

    /// Number of truncated excess file references remaining to close.
    pub fn dropped_len(&self) -> usize {
        self.fds.len() - self.deliver_len
    }

    /// Slice of file references remaining to deliver.
    pub fn delivered(&self) -> &[F] {
        &self.fds[..self.deliver_len]
    }

    /// Slice of truncated excess file references remaining to close.
    pub fn dropped(&self) -> &[F] {
        &self.fds[self.deliver_len..]
    }

    /// Pop the next file reference to deliver in FIFO order.
    ///
    /// Unlike [`TakenFds::drain_delivered`], breaking out of a `next_delivered`
    /// loop partway through (e.g. on `-EMFILE`) leaves all un-popped delivered
    /// and truncated references reachable for [`TakenFds::drain_all`]. When
    /// breaking out early on `-EMFILE`, the kernel must set `MSG_CTRUNC` in
    /// `msg_flags` itself (matching Linux `net/core/scm.c` `scm_detach_fds`),
    /// even if [`TakenFds::truncated`] is `false`. Never allocates.
    pub fn next_delivered(&mut self) -> Option<F> {
        if self.deliver_len == 0 {
            return None;
        }
        self.deliver_len -= 1;
        Some(self.fds.remove(0))
    }

    /// Drain all remaining file references to deliver in FIFO order.
    ///
    /// Dropping the returned `Drain` early drops any uniterated items in the
    /// delivered slice; use [`TakenFds::next_delivered`] if installation can
    /// fail partway through. Never allocates.
    pub fn drain_delivered(&mut self) -> Drain<'_, F> {
        let n = self.deliver_len;
        self.deliver_len = 0;
        self.fds.drain(..n)
    }

    /// Drain the truncated excess file references in FIFO order so the kernel
    /// can close them. Never allocates.
    pub fn drain_dropped(&mut self) -> Drain<'_, F> {
        self.fds.drain(self.deliver_len..)
    }

    /// Drain every remaining file reference (both undelivered and dropped) in
    /// FIFO order. Never allocates.
    pub fn drain_all(&mut self) -> Drain<'_, F> {
        self.deliver_len = 0;
        self.fds.drain(..)
    }
}

/// Ancillary data returned by a receive operation.
///
/// - A consuming receive (`peek == false`) transfers ownership via
///   [`RecvAncillary::Owned`] without allocating.
/// - A non-consuming receive (`peek == true`) borrows the still-queued
///   [`Ancillary`] via [`RecvAncillary::Peeked`] without cloning or allocating;
///   the kernel inspects `cred` and clones up to `max` file references into the
///   receiver's fd table.
#[derive(Debug, PartialEq, Eq)]
pub enum RecvAncillary<'a, F> {
    /// Ownership transferred out of the queue (`peek == false`).
    Owned(Ancillary<F>),
    /// Borrowed from a still-queued entry (`peek == true`).
    Peeked(&'a Ancillary<F>),
}

impl<'a, F> RecvAncillary<'a, F> {
    /// Shared view of the ancillary data regardless of peek vs consume.
    pub fn as_ancillary(&self) -> &Ancillary<F> {
        match self {
            Self::Owned(a) => a,
            Self::Peeked(a) => a,
        }
    }

    /// Borrowed slice of carried `SCM_RIGHTS` file references.
    pub fn fds(&self) -> &[F] {
        &self.as_ancillary().fds
    }

    /// Carried explicit `SCM_CREDENTIALS`, if any.
    pub fn cred(&self) -> Option<Ucred> {
        self.as_ancillary().cred
    }

    /// `true` if returned from a non-consuming peek.
    pub const fn is_peeked(&self) -> bool {
        matches!(self, Self::Peeked(_))
    }

    /// Extract the owned [`Ancillary`] from a consuming receive, or `None` if
    /// this came from a peek.
    pub fn into_owned(self) -> Option<Ancillary<F>> {
        match self {
            Self::Owned(a) => Some(a),
            Self::Peeked(_) => None,
        }
    }
}
