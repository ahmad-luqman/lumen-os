//! Pure `no_std + alloc` futex wait-queue logic for the Lumen kernel.
//!
//! [`FutexTable`] implements the per-bucket (or global) wait-queue state
//! machine driven by `futex(2)`: [`FutexTable::enqueue`], [`FutexTable::wake`],
//! [`FutexTable::requeue`], cross-bucket [`FutexTable::requeue_into`], and
//! [`FutexTable::cancel`].
//!
//! # Zero-allocation guarantee and cost (ARCHITECTURE §9)
//!
//! In the kernel, each futex bucket will be guarded by a **leaf IRQ-off
//! `SpinLock`** (added to ARCHITECTURE §9 by the kernel `futex(2)` issue).
//! Nothing may allocate or deallocate on the kernel heap, nor acquire any
//! `SleepMutex`, while a bucket lock is held.
//!
//! Every queue operation on [`FutexTable`] performs **zero heap allocations
//! and zero heap deallocations**, provided the caller's closures
//! (`on_requeue`, `is_waiter`) and `K` comparisons do not allocate:
//!
//! - The caller validates the bitset (`NonZeroU32`) and allocates
//!   `Box<FutexNode<K, W>>` *before* taking the bucket lock, then hands it to
//!   [`FutexTable::enqueue`].
//! - [`FutexTable::wake`], [`FutexTable::requeue`], and
//!   [`FutexTable::requeue_into`] splice woken nodes onto an intrusive
//!   [`Woken`] list without allocating.
//! - [`FutexTable::cancel`] returns the unlinked `Box<FutexNode<K, W>>` inside
//!   [`CancelOutcome::Cancelled`].
//! - **Bind and drop results *after* releasing the bucket lock.** Never drop a
//!   [`Woken`] or [`CancelOutcome::Cancelled`] temporary while the lock guard
//!   is still alive: for example, `bucket.wake(..).len()` frees every woken
//!   node on the heap *under* the leaf spinlock and never wakes their threads.
//!
//! [`FutexTable`] is a safe (`#![forbid(unsafe_code)]`) singly-linked list
//! with no tail pointer. Consequently, `enqueue`, `wake`, `requeue` (including
//! appending moved nodes to the tail), `requeue_into`, and `cancel` walk the
//! table's list in `O(len)` under the lock. Hash futex keys across an array of
//! per-bucket `SpinLock<FutexTable<K, W>>` tables in the kernel so each bucket
//! stays short.
//!
//! Within a key, waiters are woken in FIFO order; Linux is FIFO only among
//! non-realtime waiters (its `plist` is priority-ordered for RT tasks), and
//! Lumen has no realtime priorities.
//!
//! # Kernel `futex(2)` protocols
//!
//! User memory must never be faulted in while holding a bucket `SpinLock`
//! (resolving `#PF` takes `AddressSpace::inner`, a level-1 `SleepMutex`).
//!
//! ## `FUTEX_WAIT` / `FUTEX_WAIT_BITSET`
//!
//! 1. Validate alignment of `uaddr` (4-byte aligned, else `-EINVAL`) and
//!    convert `bitset` via `NonZeroU32::new(bitset)` (`None` -> `-EINVAL`
//!    *before* allocating; `FUTEX_WAIT` uses [`FUTEX_BITSET_MATCH_ANY`]).
//! 2. Allocate `Box<FutexNode<K, W>>` *before* taking any bucket lock. `W`
//!    must own a reference to the sleeping thread (e.g. an `Arc` to per-waiter
//!    wake state), because a waker wakes the thread *after* dropping the
//!    bucket lock.
//! 3. Resolve the opaque futex key (`(asid, uaddr)` for private futexes;
//!    backing identity + offset for shared futexes) and compute its bucket
//!    index `b`.
//! 4. Acquire `BUCKETS[b]`'s leaf IRQ-off `SpinLock`.
//! 5. Read the `u32` at `uaddr` **with page faults disabled** (non-faulting
//!    user-copy fixup path).
//!    - On a fault: drop the bucket lock, fault the page in with interrupts
//!      enabled (returning `-EFAULT` if unmapped or unreadable), and retry
//!      from step 3 (recomputing the key for shared futexes in case a CoW or
//!      demand fault changed the backing frame).
//! 6. If `*uaddr != expected`: drop the bucket lock, drop the unused
//!    `Box<FutexNode>` outside the lock, and return `-EAGAIN`.
//! 7. Store `b` in the waiter's atomic bucket index, mark the current thread
//!    as about to sleep *before* dropping the bucket lock (analogous to
//!    Linux's `set_current_state` before `futex_queue`, so a wake arriving
//!    between unlock and block is not lost), call [`FutexTable::enqueue`],
//!    drop the bucket lock, and block with the optional deadline and signal
//!    checks.
//! 8. After **every** return from blocking (wake, timeout, signal, or spurious
//!    wakeup), run the cancel protocol below:
//!    - [`CancelOutcome::AlreadyWoken`] -> return `0` (a concurrent wake
//!      already took the node; no lost wakeup).
//!    - [`CancelOutcome::Cancelled`] -> drop the returned node outside the
//!      lock; return `-ETIMEDOUT` if timed out, `-EINTR` if interrupted by a
//!      signal, or otherwise treat it as a spurious wakeup (retry the wait
//!      with the remaining timeout, or return `0`).
//!
//! ## `FUTEX_WAKE` / `FUTEX_WAKE_BITSET`
//!
//! 1. Validate alignment of `uaddr` (`-EINVAL`) and convert `bitset` via
//!    `NonZeroU32::new(bitset)` (`None` -> `-EINVAL`; `FUTEX_WAKE` uses
//!    [`FUTEX_BITSET_MATCH_ANY`]).
//! 2. Reinterpret the syscall's `val: u32` count as `i32` (`val as i32`),
//!    **never clamped**.
//! 3. Resolve the key and bucket `b`, lock `BUCKETS[b]`, and bind
//!    `let woken = bucket.wake(key, val as i32, bitset);`.
//! 4. Record `let count = woken.len();`, drop the bucket lock, then iterate
//!    `woken` to wake each thread and drop its `Box<FutexNode>`. Return
//!    `count`.
//!
//! ## `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`
//!
//! 1. Validate alignment of `uaddr1` and `uaddr2` (`-EINVAL`), and reinterpret
//!    `val` (`nr_wake`) and `val2` (`nr_requeue`) as `i32` (`as i32`), never
//!    clamped. Negative counts fail with [`FutexError::NegativeCount`]
//!    (`-EINVAL`).
//! 2. Resolve `key1` and `key2` and their bucket indices `b1` and `b2`.
//! 3. Acquire bucket lock(s) in canonical order (`min(b1, b2)` before
//!    `max(b1, b2)`, or a single lock when `b1 == b2`).
//! 4. For `FUTEX_CMP_REQUEUE` only: read the `u32` at `uaddr1` **with page
//!    faults disabled** under the bucket lock(s).
//!    - On a fault: drop all bucket locks, fault in `uaddr1` (returning
//!      `-EFAULT` if invalid), and retry from step 2.
//!    - If `*uaddr1 != expected_val3`: drop the bucket lock(s) and return
//!      `-EAGAIN`.
//! 5. Call [`FutexTable::requeue`] (when `b1 == b2`) or
//!    [`FutexTable::requeue_into`] (when `b1 != b2`), passing an `on_requeue`
//!    callback that stores `b2` into each moved waiter's atomic bucket index
//!    while both bucket locks are held. Bind `let (woken, moved) = ...;`.
//! 6. Compute `let total = woken.len() + moved;` (the syscall return value,
//!    matching Linux `futex_requeue`'s `task_count`), drop the bucket lock(s),
//!    then iterate `woken` to wake each thread and drop its node. Return
//!    `total`.
//!
//! # Finding a requeued waiter's bucket on `cancel` (Linux `lock_ptr` pattern)
//!
//! [`FutexTable::cancel`] matches by waiter identity across the entire table
//! and takes no key, so a waiter requeued within the same bucket is always
//! found and can never report a false [`CancelOutcome::AlreadyWoken`].
//!
//! Across multiple buckets (`[SpinLock<FutexTable<K, W>>; N]`), a concurrent
//! [`FutexTable::requeue_into`] may move a blocked waiter from `b1` to `b2`.
//! To follow it without tearing a multi-word key:
//!
//! - Store a single `AtomicUsize` bucket index in per-waiter state shared with
//!   `W`, initialized to `b1` before unlocking in `FUTEX_WAIT`, and updated to
//!   `b2` by `on_requeue` while both `b1` and `b2` locks are held.
//! - On cancel, loop:
//!   1. Load `b = waiter.bucket.load(Acquire)`.
//!   2. Lock `BUCKETS[b]`.
//!   3. Re-load `waiter.bucket.load(Acquire)`; if it changed before we took
//!      the lock, drop the lock and retry from step 1.
//!   4. Call `let outcome = bucket.cancel(|w| ...);` with a unique identity
//!      predicate (e.g. `Arc::ptr_eq` or `(ThreadId, generation)`).
//!   5. Drop the bucket lock, then handle `outcome` (dropping any
//!      [`CancelOutcome::Cancelled`] node outside the lock).

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

/// Bitset matching any non-zero waiter bitset (`FUTEX_BITSET_MATCH_ANY` in
/// Linux `include/uapi/linux/futex.h`, `0xffff_ffff`).
pub const FUTEX_BITSET_MATCH_ANY: NonZeroU32 = NonZeroU32::MAX;

/// Maximum number of queued keys printed by `Debug` on [`FutexTable`] and
/// [`Woken`], keeping formatting bounded and O(1) in stack space.
const DEBUG_KEY_PREVIEW: usize = 8;

type Link<K, W> = Option<Box<FutexNode<K, W>>>;

/// Errors returned by [`FutexTable::requeue`] and [`FutexTable::requeue_into`],
/// mapping to `-EINVAL` in `futex(2)`.
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

/// Caller-allocated wait-queue node carrying one waiter `W`, its non-zero
/// match bitset, and the futex key it is currently queued under.
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

/// Outcome of [`FutexTable::cancel`].
///
/// Bind the outcome to a variable and drop any [`CancelOutcome::Cancelled`]
/// node *after* releasing the bucket spinlock.
#[derive(Debug)]
#[must_use = "drop a Cancelled node outside the lock; return 0 on AlreadyWoken"]
pub enum CancelOutcome<K, W> {
    /// The waiter was still queued in this table and has been unlinked; the
    /// returned node carries its current [`FutexNode::key`]. Drop the `Box`
    /// *after* releasing the bucket lock.
    Cancelled(Box<FutexNode<K, W>>),
    /// The waiter is not queued in this table: a concurrent wake already took
    /// it. Return `0` from `futex(2)` so no wakeup is lost.
    AlreadyWoken,
}

impl<K, W> CancelOutcome<K, W> {
    /// Returns `true` if the waiter was still queued and removed.
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    /// Returns `true` if the waiter was not present in the table.
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
/// Bind `Woken` to a variable and release the futex bucket lock *before*
/// iterating or dropping it; dropping a temporary under the lock frees nodes
/// on the heap while the leaf spinlock is held and skips waking their threads.
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

impl<K, W> ExactSizeIterator for Woken<K, W> {
    fn len(&self) -> usize {
        self.len
    }
}

impl<K, W> FusedIterator for Woken<K, W> {}

impl<K, W> Drop for Woken<K, W> {
    fn drop(&mut self) {
        drop_chain(self.head.take());
        self.len = 0;
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
        self.len = 0;
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
    /// `nr == 0` and negative `nr` wake **at most 1** matching waiter,
    /// matching Linux `kernel/futex/waitwake.c`, `futex_wake()`, which checks
    /// `if (++ret >= nr_wake) break;` *after* waking a matching waiter
    /// (`FLAGS_STRICT`, which changes only `nr == 0`, is not set by `futex(2)`).
    pub fn wake(&mut self, key: K, nr: i32, bitset: NonZeroU32) -> Woken<K, W> {
        // Linux `kernel/futex/waitwake.c`, `futex_wake()`:
        //     this->wake(&wake_q, this);
        //     if (++ret >= nr_wake)
        //         break;
        // Because `++ret >= nr_wake` runs *after* waking a matching waiter
        // (and legacy `futex(2)` does not set `FLAGS_STRICT`, which affects
        // only `nr_wake == 0`), any `nr <= 0` wakes 1 matching waiter if one
        // exists, or 0 if none matches.
        let limit: usize = if nr <= 0 { 1 } else { nr as usize };
        let mask = bitset.get();

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
    /// with `Err(FutexError::NegativeCount)`, matching
    /// `if (nr_wake < 0 || nr_requeue < 0) return -EINVAL;` in Linux
    /// `kernel/futex/requeue.c`, `futex_requeue()`.
    ///
    /// Linux's `requeue_futex()` skips list relinking whenever both keys hash
    /// to the same bucket (`&hb1->chain != &hb2->chain`). This method appends
    /// moved nodes to `to`'s tail whenever `from != to` (matching Linux's
    /// cross-bucket behaviour) and leaves them in place in FIFO order when
    /// `from == to`.
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
    /// Lock both buckets in canonical order before calling this method.
    /// Invokes `on_requeue(&waiter, to)` for every waiter moved into `dst`
    /// while both bucket locks are held (e.g. to update each waiter's atomic
    /// bucket index). `on_requeue` must not allocate, acquire locks, or block.
    /// Semantics otherwise match [`FutexTable::requeue`].
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

    /// Cancel a waiter anywhere in this table by identity predicate (on
    /// timeout, signal, or spurious wakeup).
    ///
    /// `is_waiter` must identify at most one queued waiter (for example,
    /// `Arc::ptr_eq` or a `(ThreadId, generation)` pair, rather than value
    /// equality on `Arc<T>`), and must not allocate, acquire locks, or block.
    ///
    /// Because matching spans the entire table without taking a key, a waiter
    /// requeued within this bucket is still found and returned in
    /// [`CancelOutcome::Cancelled`] with its current [`FutexNode::key`].
    /// [`CancelOutcome::AlreadyWoken`] strictly means the waiter is not in
    /// this table (a concurrent wake already removed it).
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

    /// Shared list walker for `wake`, `requeue` (including `from == to`), and
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

        // Linux `kernel/futex/requeue.c`, `futex_requeue()`:
        //     if (task_count - nr_wake >= nr_requeue)
        //         break;
        //     ...
        //     if (!requeue_pi) {
        //         if (++task_count <= nr_wake) { ... }
        //     }
        // Unlike `futex_wake`, `nr_wake == 0` wakes 0 waiters and moves up to
        // `nr_requeue` waiters directly.
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

fn unlink<K, W>(cursor: &mut Link<K, W>) -> Box<FutexNode<K, W>> {
    let mut node = cursor.take().unwrap();
    *cursor = node.next.take();
    node
}

fn push_back<K, W>(tail: &mut Link<K, W>, node: Box<FutexNode<K, W>>) -> &mut Link<K, W> {
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
            let mut cur = self.head.as_deref();
            let mut shown = 0usize;
            while let Some(node) = cur {
                if shown == DEBUG_KEY_PREVIEW {
                    break;
                }
                if let Some(key) = &node.key {
                    list.entry(key);
                }
                cur = node.next.as_deref();
                shown += 1;
            }
            if self.len > shown { list.finish_non_exhaustive() } else { list.finish() }
        }
    }

    f.debug_struct(name).field("len", &len).field("keys", &KeysPreview { head, len }).finish()
}

/// Validate Linux `futex_requeue` counts (`nr_wake < 0 || nr_requeue < 0` ->
/// `-EINVAL` in `kernel/futex/requeue.c`, `futex_requeue()`).
fn validate_requeue_counts(nr_wake: i32, nr_requeue: i32) -> Result<(usize, usize), FutexError> {
    if nr_wake < 0 || nr_requeue < 0 {
        return Err(FutexError::NegativeCount);
    }
    Ok((nr_wake as usize, nr_requeue as usize))
}
