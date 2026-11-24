//! Pure `no_std + alloc` futex wait-queue logic for the Lumen kernel.
//!
//! [`FutexTable`] implements the per-bucket (or global) wait-queue state
//! machine driven by `futex(2)`: [`FutexTable::enqueue`], [`FutexTable::wake`],
//! [`FutexTable::requeue`] (plus cross-bucket [`FutexTable::requeue_into`]),
//! and [`FutexTable::cancel`].
//!
//! # Zero-allocation guarantee (ARCHITECTURE §9)
//!
//! In the kernel, each futex bucket is guarded by a **leaf IRQ-off `SpinLock`**
//! (lock ordering level 10). Nothing may allocate or deallocate on the kernel
//! heap, nor acquire any `SleepMutex`, while a bucket lock is held.
//!
//! Accordingly, every queue operation on [`FutexTable`] performs **zero heap
//! allocations and zero heap deallocations**:
//!
//! - The caller allocates `Box<FutexNode<K, W>>` *before* taking the bucket
//!   lock and hands it to [`FutexTable::enqueue`].
//! - On a refused enqueue (`bitset == 0`), [`EnqueueError`] returns the `Box`
//!   intact so the caller drops it *after* unlocking.
//! - [`FutexTable::wake`], [`FutexTable::requeue`], and
//!   [`FutexTable::requeue_into`] splice woken nodes onto an intrusive
//!   [`Woken`] list without allocating; the caller releases the bucket lock(s)
//!   before waking the threads and dropping the nodes.
//! - [`FutexTable::cancel`] returns the removed `Box<FutexNode<K, W>>` inside
//!   [`CancelOutcome::Cancelled`] for the caller to drop *after* unlocking.
//!
//! # Kernel `futex(2)` value-check protocol
//!
//! User memory must never be faulted in while holding a bucket `SpinLock`
//! (resolving `#PF` takes `AddressSpace::inner`, a level-1 `SleepMutex`).
//!
//! ## `FUTEX_WAIT` / `FUTEX_WAIT_BITSET`
//!
//! 1. Validate alignment of `uaddr` (4-byte aligned, else `-EINVAL`) and
//!    `bitset != 0` (else `-EINVAL`; `FUTEX_WAIT` uses [`FUTEX_BITSET_MATCH_ANY`]).
//! 2. Allocate `Box<FutexNode<K, W>>` *before* taking any bucket lock.
//! 3. Resolve the opaque futex key (`(asid, uaddr)` for private futexes;
//!    backing identity + offset for shared futexes) and pick its bucket.
//! 4. Acquire the bucket's leaf IRQ-off `SpinLock`.
//! 5. Read the `u32` at `uaddr` **with page faults disabled** (non-faulting
//!    user-copy fixup path).
//!    - On a fault: drop the bucket lock, fault the page in with interrupts
//!      enabled (returning `-EFAULT` if unmapped or unreadable), and retry
//!      from step 3 (recomputing the key for shared futexes in case a CoW or
//!      demand fault changed the backing frame).
//! 6. If `*uaddr != expected`: drop the bucket lock, drop the unused
//!    `Box<FutexNode>` outside the lock, and return `-EAGAIN`.
//! 7. Call [`FutexTable::enqueue`], drop the bucket lock, and block the thread
//!    until woken, timed out, or interrupted by a signal.
//!
//! ## `FUTEX_CMP_REQUEUE`
//!
//! 1. Validate `nr_wake >= 0 && nr_requeue >= 0` (`-EINVAL`) and alignment of
//!    `uaddr1` and `uaddr2`.
//! 2. Resolve `key1` and `key2` and their buckets `b1` and `b2`.
//! 3. Acquire bucket lock(s) in canonical order (`min(b1, b2)` before
//!    `max(b1, b2)`, or a single lock when `b1 == b2`).
//! 4. Read the `u32` at `uaddr1` **with page faults disabled** under the
//!    bucket lock(s).
//!    - On a fault: drop all bucket locks, fault in `uaddr1` (returning
//!      `-EFAULT` if invalid), and retry from step 2.
//! 5. If `*uaddr1 != expected_val3`: drop the bucket lock(s) and return
//!    `-EAGAIN`.
//! 6. Call [`FutexTable::requeue_with`] (same bucket) or
//!    [`FutexTable::requeue_into_with`] (distinct buckets), drop the bucket
//!    lock(s), then wake every waiter in the returned [`Woken`] and drop their
//!    nodes outside the lock.
//!
//! # Finding a requeued waiter's current key on `cancel`
//!
//! While a thread is blocked, a concurrent `FUTEX_REQUEUE` or
//! `FUTEX_CMP_REQUEUE` may move its node to a different key (and possibly a
//! different hash bucket). Every [`FutexNode`] stores its current `key: K`,
//! updated in place by `requeue` / `requeue_into`:
//!
//! - **Single-table kernel** (one `SpinLock<FutexTable<K, W>>`): under the
//!   lock, call [`FutexTable::key_of`] to read the waiter's current key from
//!   its node, then [`FutexTable::cancel`] at that key (or simply use
//!   `requeue_with` as below).
//! - **Multi-bucket kernel** (`[SpinLock<FutexTable<K, W>>; N]`): mirror the
//!   waiter's current key in per-waiter state shared with `W`, initialized on
//!   enqueue and updated by the `on_requeue` closure passed to
//!   [`FutexTable::requeue_with`] / [`FutexTable::requeue_into_with`] while
//!   both bucket locks are held (analogous to Linux's `q->key` and
//!   `q->lock_ptr`). When a timeout or signal fires, the thread loops:
//!   1. Read its mirrored `key`.
//!   2. Lock `BUCKETS[bucket(key)]`.
//!   3. Re-read its mirrored `key`; if it changed while waiting for the lock,
//!      drop the lock and retry from step 1.
//!   4. Call `bucket.cancel(key, &waiter)`.
//!   5. Drop the bucket lock. On [`CancelOutcome::Cancelled`], drop the
//!      returned node outside the lock and return `-ETIMEDOUT` / `-EINTR`. On
//!      [`CancelOutcome::AlreadyWoken`], a concurrent wake already claimed the
//!      node, so return `0` (no lost wakeup).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use core::fmt;
use core::iter::FusedIterator;

/// Bitset matching any non-zero waiter bitset (`FUTEX_BITSET_MATCH_ANY` in
/// Linux `include/uapi/linux/futex.h`).
pub const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

/// Errors returned by [`FutexTable`] operations, each mapping to `-EINVAL` in
/// `futex(2)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FutexError {
    /// A zero bitset was supplied to `enqueue` (`FUTEX_WAIT_BITSET`) or `wake`
    /// (`FUTEX_WAKE_BITSET`).
    ZeroBitset,
    /// A negative `nr_wake` or `nr_requeue` count was supplied to `requeue` or
    /// `requeue_into` (`FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`).
    NegativeCount,
}

impl fmt::Display for FutexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBitset => f.write_str("futex bitset must be non-zero"),
            Self::NegativeCount => f.write_str("futex requeue count must be non-negative"),
        }
    }
}

impl core::error::Error for FutexError {}

/// Failure from [`FutexTable::enqueue`], returning the caller's node without
/// dropping it so no deallocation occurs while the bucket lock is held.
#[derive(Debug)]
pub struct EnqueueError<K, W> {
    /// Why the enqueue was refused.
    pub kind: FutexError,
    /// The untouched caller-allocated node.
    pub node: Box<FutexNode<K, W>>,
}

impl<K, W> fmt::Display for EnqueueError<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl<K: fmt::Debug, W: fmt::Debug> core::error::Error for EnqueueError<K, W> {}

/// Caller-allocated wait-queue node carrying one waiter `W`, its match bitset,
/// and the futex key it is currently queued under.
///
/// Allocate `Box<FutexNode<K, W>>` before acquiring the bucket spinlock and
/// drop it only after releasing the lock.
#[derive(Debug)]
pub struct FutexNode<K, W> {
    key: Option<K>,
    bitset: u32,
    waiter: W,
    next: Option<Box<FutexNode<K, W>>>,
}

impl<K, W> FutexNode<K, W> {
    /// Create a new unqueued node with the given waiter and match bitset.
    pub const fn new(waiter: W, bitset: u32) -> Self {
        Self { key: None, bitset, waiter, next: None }
    }

    /// Create a new unqueued node matching any wake bitset
    /// ([`FUTEX_BITSET_MATCH_ANY`]), as used by `FUTEX_WAIT`.
    pub const fn new_match_any(waiter: W) -> Self {
        Self::new(waiter, FUTEX_BITSET_MATCH_ANY)
    }

    /// The match bitset stored in this node.
    pub const fn bitset(&self) -> u32 {
        self.bitset
    }

    /// The futex key this node was most recently enqueued or requeued under,
    /// or `None` if it has never been enqueued (or was reset).
    pub const fn key(&self) -> Option<K>
    where
        K: Copy,
    {
        self.key
    }

    /// Borrow the waiter stored in this node.
    pub const fn waiter(&self) -> &W {
        &self.waiter
    }

    /// Mutably borrow the waiter stored in this node.
    pub const fn waiter_mut(&mut self) -> &mut W {
        &mut self.waiter
    }

    /// Consume this unqueued node and return the inner waiter `W`.
    pub fn into_waiter(self) -> W {
        debug_assert!(self.next.is_none());
        self.waiter
    }

    /// Replace the waiter and bitset on an unqueued node for zero-allocation
    /// reuse, clearing its recorded key and returning the previous waiter.
    pub fn reset(&mut self, waiter: W, bitset: u32) -> W {
        debug_assert!(self.next.is_none());
        self.key = None;
        self.bitset = bitset;
        core::mem::replace(&mut self.waiter, waiter)
    }
}

/// Outcome of [`FutexTable::cancel`] / [`FutexTable::cancel_where`].
#[derive(Debug)]
#[must_use = "drop a Cancelled node outside the lock; return 0 on AlreadyWoken"]
pub enum CancelOutcome<K, W> {
    /// The waiter was still queued at `key` and has been unlinked. Drop the
    /// returned `Box` *after* releasing the bucket lock and return
    /// `-ETIMEDOUT` or `-EINTR`.
    Cancelled(Box<FutexNode<K, W>>),
    /// The waiter was not queued at `key`: a concurrent wake already took it.
    /// Return `0` from `futex(2)` so no wakeup is lost.
    AlreadyWoken,
}

impl<K, W> CancelOutcome<K, W> {
    /// Returns `true` if the waiter was still queued and removed.
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    /// Returns `true` if the waiter had already been woken.
    pub const fn is_already_woken(&self) -> bool {
        matches!(self, Self::AlreadyWoken)
    }

    /// Extract the cancelled node, if any.
    pub fn into_node(self) -> Option<Box<FutexNode<K, W>>> {
        match self {
            Self::Cancelled(node) => Some(node),
            Self::AlreadyWoken => None,
        }
    }
}

/// Intrusive FIFO list of nodes removed by [`FutexTable::wake`],
/// [`FutexTable::requeue`], or [`FutexTable::requeue_into`].
///
/// Constructing and draining `Woken` performs zero allocations and zero
/// deallocations when popping nodes with [`Woken::pop`]. Release the futex
/// bucket lock *before* iterating or dropping `Woken`.
#[derive(Debug)]
#[must_use = "woken waiters must be notified and their nodes dropped outside the lock"]
pub struct Woken<K, W> {
    head: Option<Box<FutexNode<K, W>>>,
    len: usize,
}

impl<K, W> Woken<K, W> {
    const fn empty() -> Self {
        Self { head: None, len: 0 }
    }

    /// Number of woken waiters in this batch.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no waiters were woken.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remove and return the next woken node in FIFO order without freeing its
    /// `Box`.
    pub fn pop(&mut self) -> Option<Box<FutexNode<K, W>>> {
        let mut node = self.head.take()?;
        self.head = node.next.take();
        self.len -= 1;
        Some(node)
    }

    /// Convert into an iterator yielding each woken waiter `W` in FIFO order,
    /// freeing each node's `Box` as it is consumed. Call only *after*
    /// releasing the bucket lock.
    pub fn into_waiters(self) -> WokenWaiters<K, W> {
        WokenWaiters(self)
    }
}

impl<K, W> Iterator for Woken<K, W> {
    type Item = Box<FutexNode<K, W>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<K, W> ExactSizeIterator for Woken<K, W> {
    fn len(&self) -> usize {
        self.len
    }
}

impl<K, W> FusedIterator for Woken<K, W> {}

impl<K, W> Drop for Woken<K, W> {
    fn drop(&mut self) {
        // Unlink iteratively so dropping a long list uses O(1) stack space.
        let mut cur = self.head.take();
        while let Some(mut node) = cur {
            cur = node.next.take();
        }
        self.len = 0;
    }
}

/// Iterator over the inner waiters `W` of a [`Woken`] batch in FIFO order.
#[derive(Debug)]
pub struct WokenWaiters<K, W>(Woken<K, W>);

impl<K, W> Iterator for WokenWaiters<K, W> {
    type Item = W;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop().map(|node| node.into_waiter())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K, W> ExactSizeIterator for WokenWaiters<K, W> {
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl<K, W> FusedIterator for WokenWaiters<K, W> {}

/// Futex wait-queue table keyed by opaque `K`, holding caller-allocated
/// [`FutexNode`]s in FIFO order per key.
///
/// Every operation (`enqueue`, `wake`, `requeue`, `requeue_into`, `cancel`)
/// executes in-place over intrusive `Box<FutexNode<K, W>>` links with zero
/// heap allocations and zero heap deallocations.
#[derive(Debug)]
pub struct FutexTable<K, W> {
    head: Option<Box<FutexNode<K, W>>>,
    len: usize,
}

impl<K, W> FutexTable<K, W> {
    /// Create an empty futex wait table.
    pub const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    /// Total number of waiters currently queued across all keys in this table.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no waiters are queued in this table.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<K, W> Default for FutexTable<K, W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, W> Drop for FutexTable<K, W> {
    fn drop(&mut self) {
        // Unlink iteratively so dropping a long list uses O(1) stack space.
        let mut cur = self.head.take();
        while let Some(mut node) = cur {
            cur = node.next.take();
        }
        self.len = 0;
    }
}

impl<K: Ord + Copy, W> FutexTable<K, W> {
    /// Enqueue `node` at the tail of `key`'s wait queue.
    ///
    /// Refuses a node whose `bitset` is `0` (`FutexError::ZeroBitset`, mapping
    /// to `-EINVAL`), returning the node intact inside [`EnqueueError`] so the
    /// caller can drop it after releasing the bucket lock.
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn enqueue(&mut self, key: K, mut node: Box<FutexNode<K, W>>) -> Result<(), EnqueueError<K, W>> {
        if node.bitset == 0 {
            return Err(EnqueueError { kind: FutexError::ZeroBitset, node });
        }
        debug_assert!(node.next.is_none());
        node.key = Some(key);
        node.next = None;

        let mut tail = &mut self.head;
        while let Some(cur) = tail {
            tail = &mut cur.next;
        }
        *tail = Some(node);
        self.len += 1;
        Ok(())
    }

    /// Wake waiters queued on `key` whose `node.bitset & bitset != 0`, in FIFO
    /// order.
    ///
    /// - A zero `bitset` is refused with `Err(FutexError::ZeroBitset)`
    ///   (matching `if (!bitset) return -EINVAL;` in Linux
    ///   `kernel/futex/waitwake.c`, `futex_wake()`).
    /// - `nr == 0` and negative `nr` wake **at most 1** matching waiter,
    ///   matching Linux's `futex_wake()` loop in `kernel/futex/waitwake.c`,
    ///   which marks a matching waiter before checking
    ///   `if (++ret >= nr_wake) break;` (legacy `futex(2)` does not set
    ///   `FLAGS_STRICT`).
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn wake(&mut self, key: K, nr: i32, bitset: u32) -> Result<Woken<K, W>, FutexError> {
        // Linux `kernel/futex/waitwake.c`, `futex_wake()`:
        //     if (!bitset)
        //         return -EINVAL;
        if bitset == 0 {
            return Err(FutexError::ZeroBitset);
        }

        // Linux `kernel/futex/waitwake.c`, `futex_wake()`:
        //     futex_wake_mark(&wake_q, this);
        //     if (++ret >= nr_wake)
        //         break;
        // Because `++ret >= nr_wake` is checked *after* waking a matching
        // waiter (and `futex(2)` does not pass `FLAGS_STRICT`), `nr <= 0`
        // wakes 1 matching waiter if one exists, or 0 if none matches.
        let limit: usize = if nr <= 0 { 1 } else { nr as usize };

        let mut woken = Woken::empty();
        let mut woken_tail = &mut woken.head;
        let mut cursor = &mut self.head;

        while woken.len < limit {
            let matches = match cursor.as_deref() {
                Some(node) => node.key == Some(key) && (node.bitset & bitset) != 0,
                None => break,
            };
            if matches {
                let mut node = cursor.take().unwrap();
                *cursor = node.next.take();
                self.len -= 1;

                let inserted = woken_tail.insert(node);
                woken_tail = &mut inserted.next;
                woken.len += 1;
            } else {
                cursor = &mut cursor.as_mut().unwrap().next;
            }
        }

        Ok(woken)
    }

    /// Wake up to `nr_wake` waiters from `from` (in FIFO order, ignoring
    /// bitsets), then move up to `nr_requeue` remaining waiters from `from` to
    /// `to`'s tail, keeping their relative order and original bitsets.
    ///
    /// Returns `(woken, moved)`. Refuses `nr_wake < 0` or `nr_requeue < 0`
    /// with `Err(FutexError::NegativeCount)`, matching
    /// `if (nr_wake < 0 || nr_requeue < 0) return -EINVAL;` in Linux
    /// `kernel/futex/requeue.c`, `futex_requeue()`.
    ///
    /// When `from == to`, up to `nr_wake` waiters are woken and up to
    /// `nr_requeue` remaining waiters are counted as moved while keeping their
    /// existing FIFO positions (matching `requeue_futex()` in Linux
    /// `kernel/futex/requeue.c`, which skips list relinking when `hb1 == hb2`).
    ///
    /// `FUTEX_CMP_REQUEUE`'s user-word check is performed by the kernel before
    /// calling this method; see the crate-level protocol documentation.
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn requeue(
        &mut self,
        from: K,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        self.requeue_with(from, to, nr_wake, nr_requeue, |_| {})
    }

    /// Like [`FutexTable::requeue`], invoking `on_requeue` on every waiter
    /// moved to `to` while the lock is held (e.g. so a kernel can update a
    /// per-waiter current-key mirror for concurrent [`FutexTable::cancel`]).
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn requeue_with(
        &mut self,
        from: K,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
        mut on_requeue: impl FnMut(&mut W),
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        let (wake_limit, requeue_limit) = validate_requeue_counts(nr_wake, nr_requeue)?;
        if wake_limit == 0 && requeue_limit == 0 {
            return Ok((Woken::empty(), 0));
        }

        if from == to {
            // Linux `kernel/futex/requeue.c`, `requeue_futex()`: when `hb1 == hb2`,
            // the waiter stays in place in FIFO order instead of rotating to the tail.
            let mut woken = Woken::empty();
            let mut woken_tail = &mut woken.head;
            let mut moved = 0usize;
            let mut cursor = &mut self.head;

            while woken.len < wake_limit || moved < requeue_limit {
                let matches = match cursor.as_deref() {
                    Some(node) => node.key == Some(from),
                    None => break,
                };
                if matches {
                    if woken.len < wake_limit {
                        let mut node = cursor.take().unwrap();
                        *cursor = node.next.take();
                        self.len -= 1;

                        let inserted = woken_tail.insert(node);
                        woken_tail = &mut inserted.next;
                        woken.len += 1;
                    } else {
                        let node = cursor.as_mut().unwrap();
                        on_requeue(&mut node.waiter);
                        moved += 1;
                        cursor = &mut node.next;
                    }
                } else {
                    cursor = &mut cursor.as_mut().unwrap().next;
                }
            }
            return Ok((woken, moved));
        }

        let extracted = self.extract_for_requeue(from, to, wake_limit, requeue_limit, &mut on_requeue);
        if let Some(moved_chain) = extracted.moved_head {
            self.append_chain(moved_chain, extracted.moved);
        }
        Ok((extracted.woken, extracted.moved))
    }

    /// Requeue from `from` in `self` into `to` in a distinct bucket table
    /// `dst`.
    ///
    /// For kernels that hash futex keys across multiple `SpinLock<FutexTable>`
    /// buckets: lock both buckets in canonical order before calling this
    /// method. Semantics match [`FutexTable::requeue`], with zero heap
    /// allocations and zero deallocations.
    pub fn requeue_into(
        &mut self,
        from: K,
        dst: &mut Self,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        self.requeue_into_with(from, dst, to, nr_wake, nr_requeue, |_| {})
    }

    /// Like [`FutexTable::requeue_into`], invoking `on_requeue` on every
    /// waiter moved into `dst` so a multi-bucket kernel can update each
    /// waiter's current key while both bucket locks are held.
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn requeue_into_with(
        &mut self,
        from: K,
        dst: &mut Self,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
        mut on_requeue: impl FnMut(&mut W),
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        let (wake_limit, requeue_limit) = validate_requeue_counts(nr_wake, nr_requeue)?;
        if wake_limit == 0 && requeue_limit == 0 {
            return Ok((Woken::empty(), 0));
        }

        let extracted = self.extract_for_requeue(from, to, wake_limit, requeue_limit, &mut on_requeue);
        if let Some(moved_chain) = extracted.moved_head {
            dst.append_chain(moved_chain, extracted.moved);
        }
        Ok((extracted.woken, extracted.moved))
    }

    /// Cancel a waiter queued at `key` (on timeout or signal).
    ///
    /// If the waiter is still queued at `key`, removes and returns its node as
    /// [`CancelOutcome::Cancelled`]. If it is no longer present at `key`
    /// (already woken by a concurrent `wake` or `requeue`), returns
    /// [`CancelOutcome::AlreadyWoken`] so the caller returns `0` without
    /// losing a wakeup.
    ///
    /// After a requeue, the waiter lives under its new key and must be
    /// cancelled there (see the crate-level documentation on tracking a
    /// requeued waiter's key).
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn cancel(&mut self, key: K, waiter: &W) -> CancelOutcome<K, W>
    where
        W: PartialEq,
    {
        self.cancel_where(key, |w| w == waiter)
    }

    /// Predicate variant of [`FutexTable::cancel`] for waiter types that do
    /// not implement `PartialEq` or match by an identifier field.
    ///
    /// Performs zero heap allocations and zero deallocations.
    pub fn cancel_where(&mut self, key: K, mut pred: impl FnMut(&W) -> bool) -> CancelOutcome<K, W> {
        let mut cursor = &mut self.head;
        while let Some(node) = cursor.as_deref() {
            if node.key == Some(key) && pred(&node.waiter) {
                let mut removed = cursor.take().unwrap();
                *cursor = removed.next.take();
                self.len -= 1;
                return CancelOutcome::Cancelled(removed);
            }
            cursor = &mut cursor.as_mut().unwrap().next;
        }
        CancelOutcome::AlreadyWoken
    }

    /// Return the current key of a queued waiter, or `None` if it is not in
    /// this table (e.g. already woken).
    pub fn key_of(&self, waiter: &W) -> Option<K>
    where
        W: PartialEq,
    {
        self.key_of_where(|w| w == waiter)
    }

    /// Predicate variant of [`FutexTable::key_of`].
    pub fn key_of_where(&self, mut pred: impl FnMut(&W) -> bool) -> Option<K> {
        let mut cur = self.head.as_deref();
        while let Some(node) = cur {
            if pred(&node.waiter) {
                return node.key;
            }
            cur = node.next.as_deref();
        }
        None
    }

    /// Count how many waiters are currently queued under `key`.
    pub fn count_for_key(&self, key: K) -> usize {
        let mut count = 0usize;
        let mut cur = self.head.as_deref();
        while let Some(node) = cur {
            if node.key == Some(key) {
                count += 1;
            }
            cur = node.next.as_deref();
        }
        count
    }

    /// Unlink up to `wake_limit` nodes on `from` into `Woken` and up to
    /// `requeue_limit` subsequent nodes on `from` into a detached chain with
    /// their key updated to `to`.
    fn extract_for_requeue(
        &mut self,
        from: K,
        to: K,
        wake_limit: usize,
        requeue_limit: usize,
        on_requeue: &mut impl FnMut(&mut W),
    ) -> ExtractedRequeue<K, W> {
        let mut woken = Woken::empty();
        let mut woken_tail = &mut woken.head;
        let mut moved_head: Option<Box<FutexNode<K, W>>> = None;
        let mut moved_tail = &mut moved_head;
        let mut moved = 0usize;
        let mut cursor = &mut self.head;

        // Linux `kernel/futex/requeue.c`, `futex_requeue()`:
        //     if (task_count - nr_wake >= nr_requeue)
        //         break;
        //     ...
        //     if (++task_count <= nr_wake && !requeue_pi) { ... }
        // Unlike `futex_wake`, `nr_wake == 0` wakes 0 waiters and moves up to
        // `nr_requeue` waiters directly.
        while woken.len < wake_limit || moved < requeue_limit {
            let matches = match cursor.as_deref() {
                Some(node) => node.key == Some(from),
                None => break,
            };
            if matches {
                let mut node = cursor.take().unwrap();
                *cursor = node.next.take();
                self.len -= 1;

                if woken.len < wake_limit {
                    let inserted = woken_tail.insert(node);
                    woken_tail = &mut inserted.next;
                    woken.len += 1;
                } else {
                    node.key = Some(to);
                    on_requeue(&mut node.waiter);
                    let inserted = moved_tail.insert(node);
                    moved_tail = &mut inserted.next;
                    moved += 1;
                }
            } else {
                cursor = &mut cursor.as_mut().unwrap().next;
            }
        }

        ExtractedRequeue { woken, moved_head, moved }
    }

    fn append_chain(&mut self, chain: Box<FutexNode<K, W>>, count: usize) {
        let mut tail = &mut self.head;
        while let Some(cur) = tail {
            tail = &mut cur.next;
        }
        *tail = Some(chain);
        self.len += count;
    }
}

struct ExtractedRequeue<K, W> {
    woken: Woken<K, W>,
    moved_head: Option<Box<FutexNode<K, W>>>,
    moved: usize,
}

/// Validate Linux `futex_requeue` counts (`nr_wake < 0 || nr_requeue < 0` ->
/// `-EINVAL` in `kernel/futex/requeue.c`).
fn validate_requeue_counts(nr_wake: i32, nr_requeue: i32) -> Result<(usize, usize), FutexError> {
    if nr_wake < 0 || nr_requeue < 0 {
        return Err(FutexError::NegativeCount);
    }
    Ok((nr_wake as usize, nr_requeue as usize))
}
