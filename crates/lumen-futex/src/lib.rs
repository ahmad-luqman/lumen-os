//! Pure `no_std + alloc` futex wait-queue logic for the Lumen kernel.
//!
//! [`FutexTable`] implements the per-bucket (or global) wait-queue state
//! machine driven by `futex(2)`: [`FutexTable::enqueue`], [`FutexTable::wake`],
//! [`FutexTable::requeue`], cross-bucket [`FutexTable::requeue_into`], and
//! [`FutexTable::cancel`]. [`BucketIndex`] implements the multi-bucket atomic
//! recheck protocol (Linux's `q->lock_ptr` pattern).
//!
//! # Zero-allocation guarantee and cost (ARCHITECTURE §9)
//!
//! In the kernel, each futex bucket will be guarded by an **IRQ-off `SpinLock`
//! that is a leaf, except that one other bucket may be nested under it in
//! ascending index order** (added to ARCHITECTURE §9 by the kernel `futex(2)`
//! issue). Nothing may allocate or deallocate on the kernel heap, nor acquire
//! any `SleepMutex`, while a bucket lock is held.
//!
//! Every queue operation on [`FutexTable`] (and [`BucketIndex::cancel_in`])
//! performs **zero heap allocations and zero heap deallocations**, provided the
//! caller's closures (`on_requeue`, `is_waiter`, `lock`) and `K` comparisons do
//! not allocate:
//!
//! - The caller validates the bitset (`NonZeroU32`) and allocates
//!   `Box<FutexNode<K, W>>` *before* taking the bucket lock, then hands it to
//!   [`FutexTable::enqueue`].
//! - [`FutexTable::wake`], [`FutexTable::requeue`], and
//!   [`FutexTable::requeue_into`] splice woken nodes onto an intrusive
//!   [`Woken`] list without allocating.
//! - [`FutexTable::cancel`] returns the unlinked `Box<FutexNode<K, W>>` inside
//!   [`CancelOutcome::Cancelled`].
//! - **Bind results and drop the lock *before* draining or dropping them.**
//!   Locals drop in reverse declaration order, so a `Woken` declared after its
//!   lock guard drops *before* the guard, freeing nodes under the leaf spinlock
//!   and silently losing every wakeup (which panics under debug assertions):
//!
//!   ```ignore
//!   // WRONG: `woken` drops before `guard` (reverse declaration order),
//!   // freeing nodes under the spinlock and never waking their threads.
//!   let mut guard = bucket.lock();
//!   let woken = guard.wake(key, nr, bitset);
//!
//!   // WRONG: temporary `Woken` drops before `guard` is released.
//!   let n = guard.wake(key, nr, bitset).len();
//!
//!   // RIGHT: release the lock guard before draining `woken`.
//!   let woken = {
//!       let mut guard = bucket.lock();
//!       guard.wake(key, nr, bitset)
//!   };
//!   let count = woken.len();
//!   for node in woken {
//!       wake_thread(node.into_waiter());
//!   }
//!   ```
//!
//!   [`BucketIndex::cancel_in`] drops the bucket guard internally before
//!   returning [`CancelOutcome`], preventing the same trap on `cancel`.
//!
//! [`FutexTable`] is a safe (`#![forbid(unsafe_code)]`) singly-linked list
//! with no tail pointer. Consequently, `enqueue`, `wake`, `requeue` (including
//! appending moved nodes to the tail), and `cancel` walk the table's list in
//! `O(len)` under the lock, and `requeue_into` walks both `self`'s list and
//! `dst`'s list. Hash futex keys across an array of per-bucket
//! `SpinLock<FutexTable<K, W>>` tables in the kernel so each bucket stays
//! short.
//!
//! Within a key, waiters are woken in FIFO order; Linux is FIFO only among
//! non-realtime waiters (its `plist` is priority-ordered for RT tasks), and
//! Lumen has no realtime priorities.
//!
//! # Kernel `futex(2)` protocols
//!
//! User memory must never be faulted in while holding a bucket `SpinLock`
//! (resolving `#PF` takes Lock 1, `AddressSpace::inner`, a `SleepMutex`).
//!
//! ## `FUTEX_WAIT` / `FUTEX_WAIT_BITSET`
//!
//! 1. Validate that `uaddr` is a user address (`-EFAULT` if not) and 4-byte
//!    aligned (`-EINVAL` if not), and convert `bitset` via
//!    `NonZeroU32::new(bitset)` (`None` -> `-EINVAL` *before* allocating;
//!    `FUTEX_WAIT` uses [`FUTEX_BITSET_MATCH_ANY`]).
//! 2. Allocate a fresh per-wait `Arc` holding a [`BucketIndex`], a `woken`
//!    flag (`AtomicBool`), and a handle to the sleeping thread's wait queue,
//!    and allocate `Box<FutexNode<K, W>>` *before* taking any bucket lock. `W`
//!    must own a reference to that `Arc` because a waker wakes the thread
//!    *after* dropping the bucket lock.
//! 3. Resolve the opaque futex key (`(asid, uaddr)` for private futexes;
//!    backing identity + offset for shared futexes) and compute its bucket
//!    index `b`.
//! 4. Acquire `BUCKETS[b]`'s IRQ-off `SpinLock`.
//! 5. Read the `u32` at `uaddr` **with page faults disabled** (non-faulting
//!    user-copy fixup path).
//!    - On a fault: drop the bucket lock, fault the page in with interrupts
//!      enabled (returning `-EFAULT` if unmapped or unreadable), and retry
//!      from step 3 (recomputing the key for shared futexes in case a CoW or
//!      demand fault changed the backing frame).
//! 6. If `*uaddr != expected`: drop the bucket lock, drop the unused
//!    `Box<FutexNode>` outside the lock, and return `-EAGAIN` (Linux's
//!    `-EWOULDBLOCK`).
//! 7. Call `waiter.bucket.set(b)` and [`FutexTable::enqueue`], drop the bucket
//!    lock, and block with Lumen's
//!    `sched::WaitQueue::wait_until_or_deadline(deadline, || waiter.woken.load(Acquire))`.
//!    Each waker sets `waiter.woken.store(true, Release)` before calling
//!    `wake_one` on the thread's wait queue, so a wake landing between unlock
//!    and block is observed immediately by `ready` (analogous to Linux's
//!    `set_current_state` before `futex_queue`). A waker's late `wake_one` can
//!    still land on the thread's next block, and the retry absorbs it.
//! 8. After **every** return from blocking (wake, timeout, signal, or spurious
//!    wakeup), call [`BucketIndex::cancel_in`] (the load, lock, and recheck
//!    protocol below):
//!    - [`CancelOutcome::AlreadyWoken`] (meaning not in the waiter's rechecked
//!      bucket, so a concurrent wake claimed it) -> return `0`.
//!    - [`CancelOutcome::Cancelled`] (`node` returned with the lock already
//!      dropped):
//!      - **Timeout** (checked first): drop `node` and return `-ETIMEDOUT`.
//!      - **Pending signal**: drop `node` and return `-ERESTARTSYS` (restarted
//!        under `SA_RESTART`, otherwise `-EINTR`).
//!      - **Spurious wakeup** (neither timeout nor signal): keep `node` and
//!        retry from step 3 with the same absolute deadline, re-reading the
//!        user word as Linux's `goto retry` does (which may return `-EAGAIN`).
//!        Returning `0` instead is allowed as a documented deviation from
//!        Linux.
//!
//! ## `FUTEX_WAKE` / `FUTEX_WAKE_BITSET`
//!
//! 1. Validate that `uaddr` is a user address (`-EFAULT`) and 4-byte aligned
//!    (`-EINVAL`), and convert `bitset` via `NonZeroU32::new(bitset)`
//!    (`None` -> `-EINVAL`; `FUTEX_WAKE` uses [`FUTEX_BITSET_MATCH_ANY`]).
//! 2. Reinterpret the syscall's `val: u32` count as `i32` (`val as i32`),
//!    **never clamped**.
//! 3. Resolve the key and bucket `b`, lock `BUCKETS[b]`, and bind
//!    `let woken = bucket.wake(key, val as i32, bitset);` in a scope that
//!    drops the lock guard before `woken`.
//! 4. Record `let count = woken.len();`, iterate `woken` outside the lock to
//!    mark each waiter woken, wake its thread, and drop its `Box<FutexNode>`,
//!    and return `count`.
//!
//! ## `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`
//!
//! 1. Validate that `uaddr1` and `uaddr2` are user addresses (`-EFAULT`) and
//!    4-byte aligned (`-EINVAL`), reinterpret `val` (`nr_wake`) as `i32` and
//!    `val2` (passed in the syscall's `timeout` register slot, truncated to
//!    `u32`, then `as i32`, never clamped) as `nr_requeue`, and call
//!    [`check_requeue_counts`] **before** key lookup, faulting, or `cmpval`
//!    (`-EINVAL` on [`FutexError::NegativeCount`], matching Linux
//!    `kernel/futex/requeue.c:400`).
//! 2. Resolve `key1` and `key2` and their bucket indices `b1` and `b2`.
//! 3. Acquire bucket lock(s) in canonical order (`min(b1, b2)` before
//!    `max(b1, b2)`, or a single lock when `b1 == b2`).
//! 4. For `FUTEX_CMP_REQUEUE` only: read the `u32` at `uaddr1` **with page
//!    faults disabled** under the bucket lock(s).
//!    - On a fault: drop the bucket lock(s), fault in `uaddr1` (returning
//!      `-EFAULT` if invalid), and retry from step 2.
//!    - If `*uaddr1 != expected_val3`: drop the bucket lock(s) and return
//!      `-EAGAIN` (Linux's `-EWOULDBLOCK`).
//! 5. In a scope that drops the bucket lock(s) before `woken`, call
//!    [`FutexTable::requeue`] (when `b1 == b2`) or
//!    [`FutexTable::requeue_into`] (when `b1 != b2`), passing an `on_requeue`
//!    callback that calls `waiter.bucket.set(b2)` while the bucket lock(s) are
//!    held. Bind `let (woken, moved) = ...;`.
//! 6. Compute `let total = woken.len() + moved;` (the syscall return value,
//!    matching Linux `futex_requeue`'s `task_count`), iterate `woken` outside
//!    the lock(s) to wake each thread and drop its node, and return `total`.
//!
//! # Finding a requeued waiter's bucket on `cancel` (Linux `lock_ptr` pattern)
//!
//! [`FutexTable::cancel`] matches by waiter identity across the entire table
//! without taking a key, so a waiter requeued within the same bucket is always
//! found. Across multiple buckets, store a [`BucketIndex`] in per-waiter state
//! shared with `W`, update it via [`BucketIndex::set`] inside `on_requeue`
//! while the bucket lock(s) are held, and cancel via [`BucketIndex::cancel_in`].

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
use core::num::NonZeroU32;
use core::ops::DerefMut;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Bitset matching any non-zero waiter bitset (`FUTEX_BITSET_MATCH_ANY` in
/// Linux `include/uapi/linux/futex.h`, `0xffff_ffff`).
pub const FUTEX_BITSET_MATCH_ANY: NonZeroU32 = NonZeroU32::MAX;

/// Maximum number of queued keys printed by `Debug` on [`FutexTable`] and
/// [`Woken`], keeping formatting bounded and O(1) in stack space.
const DEBUG_KEY_PREVIEW: usize = 8;

type Link<K, W> = Option<Box<FutexNode<K, W>>>;

/// Errors returned by [`check_requeue_counts`], [`FutexTable::requeue`], and
/// [`FutexTable::requeue_into`], mapping to `-EINVAL` in `futex(2)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FutexError {
    /// A negative `nr_wake` or `nr_requeue` count was supplied
    /// (`FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`).
    NegativeCount,
}

impl fmt::Display for FutexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeCount => f.write_str("futex requeue count must be non-negative"),
        }
    }
}

impl core::error::Error for FutexError {}

/// Validate Linux `futex_requeue` counts (`nr_wake < 0 || nr_requeue < 0` ->
/// `-EINVAL` in Linux `kernel/futex/requeue.c:400`, `futex_requeue()`).
///
/// Call this in `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE` *before* key resolution,
/// user-memory faults, or `cmpval` comparison so negative counts always return
/// `-EINVAL` rather than `-EFAULT` or `-EAGAIN`. [`FutexTable::requeue`] and
/// [`FutexTable::requeue_into`] also run this check as a backstop.
pub const fn check_requeue_counts(nr_wake: i32, nr_requeue: i32) -> Result<(), FutexError> {
    if nr_wake < 0 || nr_requeue < 0 {
        return Err(FutexError::NegativeCount);
    }
    Ok(())
}

/// Atomic bucket index implementing Linux's `q->lock_ptr` recheck protocol
/// across an array of per-bucket [`FutexTable`] locks.
///
/// Store one `BucketIndex` in per-waiter state shared with `W`:
/// - call [`BucketIndex::set`] with the initial bucket index before unlocking
///   after [`FutexTable::enqueue`];
/// - call [`BucketIndex::set`] with the destination bucket index inside
///   [`FutexTable::requeue`] / [`FutexTable::requeue_into`]'s `on_requeue`
///   callback while the bucket lock(s) are held;
/// - call [`BucketIndex::cancel_in`] after any return from blocking to cancel
///   the waiter in whichever bucket currently owns it.
#[derive(Debug)]
pub struct BucketIndex(AtomicUsize);

impl BucketIndex {
    /// Create a bucket index initialized to `bucket`.
    pub const fn new(bucket: usize) -> Self {
        Self(AtomicUsize::new(bucket))
    }

    /// Record the waiter's current bucket index (must be called while holding
    /// the bucket lock(s) covering the enqueue or requeue).
    pub fn set(&self, bucket: usize) {
        self.0.store(bucket, Ordering::Release);
    }

    /// Read the currently recorded bucket index.
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    /// Cancel the waiter in whichever bucket currently holds it, retrying if a
    /// concurrent [`FutexTable::requeue_into`] moves the waiter between
    /// loading the bucket index and acquiring that bucket's lock.
    ///
    /// Drops the bucket lock guard *before* returning [`CancelOutcome`], so
    /// the caller can safely drop any [`CancelOutcome::Cancelled`] node. An
    /// `AlreadyWoken` returned here proves the waiter was absent from its
    /// rechecked bucket and therefore already claimed by a concurrent wake.
    ///
    /// The pre-lock load uses `Relaxed` ordering; an ABA move away from `b`
    /// and back to `b` before `lock(b)` is harmless because once `BUCKETS[b]`
    /// is locked and `Acquire`-rechecking sees `b`, no concurrent requeue can
    /// move the waiter out of `b` until the lock drops.
    ///
    /// `is_waiter` must identify at most one queued waiter by comparing
    /// against something the canceller keeps alive (e.g. `Arc::ptr_eq` or a
    /// `(ThreadId, generation)` value, never a freed node's address), and
    /// neither `lock` nor `is_waiter` may allocate or block.
    pub fn cancel_in<K, W, G>(
        &self,
        mut lock: impl FnMut(usize) -> G,
        mut is_waiter: impl FnMut(&W) -> bool,
    ) -> CancelOutcome<K, W>
    where
        K: Ord + Copy,
        G: DerefMut<Target = FutexTable<K, W>>,
    {
        loop {
            let b = self.0.load(Ordering::Relaxed);
            let mut guard = lock(b);
            if self.0.load(Ordering::Acquire) != b {
                drop(guard);
                continue;
            }
            let outcome = guard.cancel(&mut is_waiter);
            drop(guard);
            return outcome;
        }
    }
}

/// Caller-allocated wait-queue node carrying one waiter `W`, its non-zero
/// match bitset, and the futex key it was most recently enqueued or requeued
/// under.
///
/// Allocate `Box<FutexNode<K, W>>` before acquiring the bucket spinlock and
/// drop it only after releasing the lock.
pub struct FutexNode<K, W> {
    key: Option<K>,
    bitset: NonZeroU32,
    waiter: W,
    next: Link<K, W>,
}

impl<K: fmt::Debug, W: fmt::Debug> fmt::Debug for FutexNode<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FutexNode")
            .field("key", &self.key)
            .field("bitset", &self.bitset)
            .field("waiter", &self.waiter)
            .finish()
    }
}

impl<K, W> FutexNode<K, W> {
    /// Create a new unqueued node with the given waiter and non-zero match
    /// bitset.
    pub const fn new(waiter: W, bitset: NonZeroU32) -> Self {
        Self { key: None, bitset, waiter, next: None }
    }

    /// Create a new unqueued node matching any wake bitset
    /// ([`FUTEX_BITSET_MATCH_ANY`]), as used by `FUTEX_WAIT`.
    pub const fn new_match_any(waiter: W) -> Self {
        Self::new(waiter, FUTEX_BITSET_MATCH_ANY)
    }

    /// The non-zero match bitset stored in this node.
    pub const fn bitset(&self) -> NonZeroU32 {
        self.bitset
    }

    /// The futex key this node was most recently enqueued or requeued under,
    /// or `None` if it has never been enqueued.
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

    /// Consume this unqueued node and return the inner waiter `W`.
    pub fn into_waiter(self) -> W {
        debug_assert!(self.next.is_none());
        self.waiter
    }
}

/// Outcome of [`FutexTable::cancel`] and [`BucketIndex::cancel_in`].
///
/// Bind the outcome to a variable and drop any [`CancelOutcome::Cancelled`]
/// node *after* releasing the bucket spinlock (handled automatically by
/// [`BucketIndex::cancel_in`]).
#[derive(Debug)]
#[must_use = "drop a Cancelled node outside the lock; return 0 on AlreadyWoken"]
pub enum CancelOutcome<K, W> {
    /// The waiter was still queued in this table and has been unlinked; the
    /// returned node carries its current [`FutexNode::key`]. Drop the `Box`
    /// *after* releasing the bucket lock.
    Cancelled(Box<FutexNode<K, W>>),
    /// The waiter is not queued in *this* table.
    ///
    /// All a single table knows is that the waiter is absent here. That
    /// implies a concurrent wake already took it (so `futex(2)` returns `0`
    /// with no lost wakeup) only when this is the bucket named by the waiter's
    /// [`BucketIndex`], rechecked under its lock (as [`BucketIndex::cancel_in`]
    /// enforces).
    AlreadyWoken,
}

impl<K, W> CancelOutcome<K, W> {
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
/// Bind `Woken` in a scope that releases the bucket lock guard *before*
/// iterating `Woken`, and drain every node out of it: dropping a non-empty
/// `Woken` loses wakeups silently (and triggers a `debug_assert!` panic).
#[must_use = "woken waiters must be notified and their nodes dropped outside the lock"]
pub struct Woken<K, W> {
    head: Link<K, W>,
    len: usize,
}

impl<K: fmt::Debug, W> fmt::Debug for Woken<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_chain_debug("Woken", &self.head, self.len, f)
    }
}

impl<K, W> Woken<K, W> {
    const fn empty() -> Self {
        Self { head: None, len: 0 }
    }

    /// Number of woken waiters remaining in this batch.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no woken waiters remain.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<K, W> Iterator for Woken<K, W> {
    type Item = Box<FutexNode<K, W>>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut node = self.head.take()?;
        self.head = node.next.take();
        self.len -= 1;
        Some(node)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<K, W> ExactSizeIterator for Woken<K, W> {}

impl<K, W> FusedIterator for Woken<K, W> {}

impl<K, W> Drop for Woken<K, W> {
    fn drop(&mut self) {
        debug_assert!(
            self.head.is_none(),
            "Woken dropped with {} unnotified waiter(s); drain every waiter after unlocking",
            self.len
        );
        drop_chain(self.head.take());
    }
}

/// Futex wait-queue table keyed by opaque `K`, holding caller-allocated
/// [`FutexNode`]s in FIFO order per key.
///
/// See the crate-level documentation for the zero-allocation guarantee,
/// `O(len)` list-walk cost, and kernel `futex(2)` locking protocols.
pub struct FutexTable<K, W> {
    head: Link<K, W>,
    len: usize,
}

impl<K: fmt::Debug, W> fmt::Debug for FutexTable<K, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_chain_debug("FutexTable", &self.head, self.len, f)
    }
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
        drop_chain(self.head.take());
    }
}

impl<K: Ord + Copy, W> FutexTable<K, W> {
    /// Enqueue `node` at the tail of `key`'s wait queue.
    ///
    /// Callers must not enqueue two nodes with the same waiter identity at the
    /// same time (no runtime duplicate scan is performed).
    pub fn enqueue(&mut self, key: K, mut node: Box<FutexNode<K, W>>) {
        debug_assert!(node.next.is_none());
        node.key = Some(key);
        self.append_chain(node, 1);
    }

    /// Wake waiters queued on `key` whose `node.bitset & bitset != 0`, in FIFO
    /// order (matching Linux for non-realtime waiters; Lumen has no RT
    /// priorities).
    ///
    /// Matches Linux `kernel/futex/waitwake.c`, `futex_wake()`, which calls
    /// `this->wake(&wake_q, this);` before checking
    /// `if (++ret >= nr_wake) break;`. Because legacy `futex(2)` does not set
    /// `FLAGS_STRICT` (which affects only `nr_wake == 0`), `nr == 0` and any
    /// negative `nr` wake **at most 1** matching waiter (0 if none matches).
    pub fn wake(&mut self, key: K, nr: i32, bitset: NonZeroU32) -> Woken<K, W> {
        let limit = nr.max(1) as usize;
        let mask = bitset.get();

        // With `requeue_limit = 0`, the walker never enters the requeue
        // branches, so `to`, `detach_moved`, and `on_requeue` are unused.
        self.drain_matching(
            limit,
            0,
            key,
            false,
            |node| node.key == Some(key) && (node.bitset.get() & mask) != 0,
            |_, _| {},
        )
        .woken
    }

    /// Wake up to `nr_wake` waiters from `from` (in FIFO order, ignoring
    /// bitsets), then move up to `nr_requeue` remaining waiters from `from` to
    /// `to`'s tail, keeping their relative order and original bitsets.
    ///
    /// `to` must belong to this same bucket table (not checked at runtime);
    /// use [`FutexTable::requeue_into`] when `from` and `to` hash to different
    /// buckets.
    ///
    /// Invokes `on_requeue(&waiter, to)` for every moved waiter while the lock
    /// is held. `on_requeue` must not allocate, acquire locks, or block.
    ///
    /// Returns `(woken, moved)`. Refuses `nr_wake < 0` or `nr_requeue < 0`
    /// with `Err(FutexError::NegativeCount)` as a backstop to
    /// [`check_requeue_counts`].
    ///
    /// Linux's `requeue_futex()` relinks only when the two keys hash to
    /// different buckets (`&hb1->chain != &hb2->chain`); within one bucket it
    /// only rewrites the key so the waiter keeps its place. This method
    /// appends moved nodes to `to`'s tail whenever `from != to` (matching
    /// Linux's cross-bucket behaviour) and leaves them in place in FIFO order
    /// when `from == to`; waiters left in place when `from == to` are still
    /// counted in `moved` and passed to `on_requeue`, matching Linux's
    /// `task_count`.
    ///
    /// `FUTEX_CMP_REQUEUE`'s user-word check is performed by the kernel before
    /// calling this method; see the crate-level protocol documentation.
    pub fn requeue(
        &mut self,
        from: K,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
        on_requeue: impl FnMut(&W, K),
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        let (wake_limit, requeue_limit) = validate_requeue_counts(nr_wake, nr_requeue)?;
        let extracted =
            self.drain_matching(wake_limit, requeue_limit, to, from != to, |n| n.key == Some(from), on_requeue);
        if let Some(chain) = extracted.moved_head {
            self.append_chain(chain, extracted.moved);
        }
        Ok((extracted.woken, extracted.moved))
    }

    /// Requeue from `from` in `self` into `to` in a distinct bucket table
    /// `dst`.
    ///
    /// Lock both buckets in ascending index order before calling this method.
    /// Invokes `on_requeue(&waiter, to)` for every waiter moved into `dst`
    /// while both bucket locks are held (e.g. calling [`BucketIndex::set`]).
    /// `on_requeue` must not allocate, acquire locks, or block. Semantics
    /// otherwise match [`FutexTable::requeue`].
    pub fn requeue_into(
        &mut self,
        from: K,
        dst: &mut Self,
        to: K,
        nr_wake: i32,
        nr_requeue: i32,
        on_requeue: impl FnMut(&W, K),
    ) -> Result<(Woken<K, W>, usize), FutexError> {
        let (wake_limit, requeue_limit) = validate_requeue_counts(nr_wake, nr_requeue)?;
        let extracted = self.drain_matching(wake_limit, requeue_limit, to, true, |n| n.key == Some(from), on_requeue);
        if let Some(chain) = extracted.moved_head {
            dst.append_chain(chain, extracted.moved);
        }
        Ok((extracted.woken, extracted.moved))
    }

    /// Cancel a waiter anywhere in this table by identity predicate after any
    /// return from blocking.
    ///
    /// `is_waiter` must identify at most one queued waiter by comparing
    /// against something the canceller keeps alive (for example,
    /// `Arc::ptr_eq` or a `(ThreadId, generation)` value — never value
    /// equality on `Arc<T>` or a freed node's address), and must not allocate,
    /// acquire locks, or block.
    ///
    /// Because matching spans the entire table without taking a key, a waiter
    /// requeued within this bucket is still found and returned in
    /// [`CancelOutcome::Cancelled`] with its current [`FutexNode::key`].
    /// [`CancelOutcome::AlreadyWoken`] means only that the waiter is not in
    /// *this* table; in a multi-bucket kernel, call [`BucketIndex::cancel_in`]
    /// so `AlreadyWoken` is reached only after rechecking the waiter's bucket
    /// under its lock.
    pub fn cancel(&mut self, mut is_waiter: impl FnMut(&W) -> bool) -> CancelOutcome<K, W> {
        let mut cursor = &mut self.head;
        while let Some(node) = cursor.as_deref() {
            if is_waiter(&node.waiter) {
                let removed = unlink(cursor);
                self.len -= 1;
                return CancelOutcome::Cancelled(removed);
            }
            cursor = &mut cursor.as_mut().unwrap().next;
        }
        CancelOutcome::AlreadyWoken
    }

    /// Count how many waiters are currently queued under `key`.
    pub fn count_for_key(&self, key: K) -> usize {
        nodes(&self.head).filter(|node| node.key == Some(key)).count()
    }

    /// Shared list walker for `wake` (with `nr <= 0` already mapped to `1` and
    /// `requeue_limit == 0`), `requeue` (including `from == to`), and
    /// `requeue_into`.
    fn drain_matching(
        &mut self,
        wake_limit: usize,
        requeue_limit: usize,
        to: K,
        detach_moved: bool,
        mut matches: impl FnMut(&FutexNode<K, W>) -> bool,
        mut on_requeue: impl FnMut(&W, K),
    ) -> Extracted<K, W> {
        let mut woken = Woken::empty();
        let mut woken_tail = &mut woken.head;
        let mut moved_head: Link<K, W> = None;
        let mut moved_tail = &mut moved_head;
        let mut moved = 0usize;
        let mut cursor = &mut self.head;

        // Linux `kernel/futex/requeue.c:621-625`, `futex_requeue()`:
        //     if (task_count - nr_wake >= nr_requeue)
        //         break;
        //
        //     if (!futex_match(&this->key, &key1))
        //         continue;
        // Followed by `if (!requeue_pi) { if (++task_count <= nr_wake) ... }`.
        // Unlike `futex_wake`, `nr_wake == 0` wakes 0 waiters and moves up to
        // `nr_requeue` waiters directly. This walker also serves `wake` with
        // `nr <= 0` already mapped to `1` and `requeue_limit == 0`.
        while woken.len < wake_limit || moved < requeue_limit {
            let Some(node) = cursor.as_deref() else {
                break;
            };
            if !matches(node) {
                cursor = &mut cursor.as_mut().unwrap().next;
                continue;
            }
            if woken.len < wake_limit {
                let node = unlink(cursor);
                self.len -= 1;
                woken_tail = push_back(woken_tail, node);
                woken.len += 1;
            } else if detach_moved {
                let mut node = unlink(cursor);
                self.len -= 1;
                node.key = Some(to);
                on_requeue(&node.waiter, to);
                moved_tail = push_back(moved_tail, node);
                moved += 1;
            } else {
                let node = cursor.as_mut().unwrap();
                on_requeue(&node.waiter, to);
                moved += 1;
                cursor = &mut node.next;
            }
        }

        Extracted { woken, moved_head, moved }
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

struct Extracted<K, W> {
    woken: Woken<K, W>,
    moved_head: Link<K, W>,
    moved: usize,
}

fn nodes<K, W>(head: &Link<K, W>) -> impl Iterator<Item = &FutexNode<K, W>> {
    core::iter::successors(head.as_deref(), |node| node.next.as_deref())
}

fn unlink<K, W>(cursor: &mut Link<K, W>) -> Box<FutexNode<K, W>> {
    let mut node = cursor.take().unwrap();
    *cursor = node.next.take();
    node
}

fn push_back<K, W>(tail: &mut Link<K, W>, node: Box<FutexNode<K, W>>) -> &mut Link<K, W> {
    debug_assert!(tail.is_none());
    &mut tail.insert(node).next
}

fn drop_chain<K, W>(mut head: Link<K, W>) {
    // Unlink iteratively so dropping a long list uses O(1) stack space.
    while let Some(mut node) = head {
        head = node.next.take();
    }
}

fn fmt_chain_debug<K: fmt::Debug, W>(
    name: &str,
    head: &Link<K, W>,
    len: usize,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    struct KeysPreview<'a, K, W> {
        head: &'a Link<K, W>,
        len: usize,
    }

    impl<K: fmt::Debug, W> fmt::Debug for KeysPreview<'_, K, W> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut list = f.debug_list();
            let mut shown = 0usize;
            for node in nodes(self.head).take(DEBUG_KEY_PREVIEW) {
                if let Some(key) = &node.key {
                    list.entry(key);
                }
                shown += 1;
            }
            if self.len > shown { list.finish_non_exhaustive() } else { list.finish() }
        }
    }

    f.debug_struct(name).field("len", &len).field("keys", &KeysPreview { head, len }).finish()
}

fn validate_requeue_counts(nr_wake: i32, nr_requeue: i32) -> Result<(usize, usize), FutexError> {
    check_requeue_counts(nr_wake, nr_requeue)?;
    Ok((nr_wake as usize, nr_requeue as usize))
}
