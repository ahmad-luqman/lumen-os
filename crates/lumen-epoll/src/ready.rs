//! Leaf-spinlock ready list ([`ReadyQueue`]), two-phase scan
//! (`begin_scan` / `finish_scan` / `finish_peek`), and copy-out rollback
//! (`unreport`), corresponding to Linux `fs/eventpoll.c` `ep->lock`,
//! `ep_poll_callback`, `ep_start_scan`, `ep_send_events`, `ep_done_scan`, and
//! `ep_read_events_proc`.

use alloc::vec::Vec;
use core::fmt;

use crate::{EP_ALWAYS_EVENTS, EP_PRIVATE_BITS, EPOLLET, EPOLLONESHOT, EpollSet, Errno, Event, Handle};

/// Outcome of [`ReadyQueue::wake`].
///
/// This is **not** Linux `ep_poll_callback`'s integer return value (`1`/`0` for
/// waitqueue traversal). It tells the caller whether the interest newly entered
/// the ready list (`Queued`) so B5 knows when to wake threads sleeping in
/// `epoll_wait`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Wake {
    /// The interest was armed, matched `ready`, and was newly appended to the
    /// ready list.
    Queued,
    /// The interest was armed and matched `ready`, and was already on the ready
    /// list or detached in an active scan (if in a scan, `woke_during_scan` was
    /// set so the scan will not lose the edge).
    AlreadyQueued,
    /// The interest is live, but either disarmed (`EPOLLONESHOT`) or its watch
    /// mask did not intersect `ready`.
    Filtered,
    /// `handle` is out of bounds or its generation no longer matches (the
    /// interest was removed or its slot was reused).
    Stale,
}

#[derive(Clone, Copy, Debug)]
struct ReadySlot {
    /// Non-zero generation when active, `0` when vacant/unlinked.
    generation: u32,
    /// Copy of the interest's monitored event bits (excludes `EP_PRIVATE_BITS`,
    /// always includes `EPOLLERR | EPOLLHUP` while active).
    watch_mask: u32,
    /// `false` while disarmed after an `EPOLLONESHOT` report.
    armed: bool,
    /// Currently linked in `ReadyQueue::ready`.
    in_ready: bool,
    /// Currently detached in an active [`Scan`] batch.
    in_scan: bool,
    /// A matching [`ReadyQueue::wake`] arrived while `in_scan` was `true`
    /// (stands in for Linux's `ep->ovflist`).
    woke_during_scan: bool,
    prev: Option<u32>,
    next: Option<u32>,
}

impl ReadySlot {
    const fn vacant() -> Self {
        Self {
            generation: 0,
            watch_mask: 0,
            armed: false,
            in_ready: false,
            in_scan: false,
            woke_during_scan: false,
            prev: None,
            next: None,
        }
    }
}

/// Pre-allocated backing buffer for [`ReadyQueue`], created outside the
/// spinlock by [`ReadyQueue::prepare_grow`] and swapped in under the spinlock
/// by [`ReadyQueue::install`].
///
/// Drop the old `Storage` returned by [`ReadyQueue::install`] *after* releasing
/// the spinlock so no heap deallocation occurs under level-10e.
#[must_use = "install into ReadyQueue or drop outside the spinlock"]
pub struct Storage {
    slots: Vec<ReadySlot>,
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Storage").field("capacity", &self.slots.len()).finish()
    }
}

impl Storage {
    /// Number of slots backed by this storage buffer.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
}

struct ReadyList {
    head: Option<u32>,
    tail: Option<u32>,
    len: usize,
}

impl ReadyList {
    const fn new() -> Self {
        Self { head: None, tail: None, len: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Detached batch of ready handles produced by [`ReadyQueue::begin_scan`].
///
/// While a `Scan` is in flight, its handles remain marked in-scan inside the
/// [`ReadyQueue`]: concurrent [`ReadyQueue::wake`] calls on them record
/// `woke_during_scan` without touching the ready list, while wakes on other
/// handles append to the ready list normally.
///
/// Pass every `Scan` back to [`ReadyQueue::finish_scan`] or
/// [`ReadyQueue::finish_peek`].
#[derive(Debug)]
#[must_use = "pass Scan back to ReadyQueue::finish_scan or ReadyQueue::finish_peek"]
pub struct Scan<'a> {
    handles: &'a [Handle],
    finished: bool,
}

impl<'a> Scan<'a> {
    /// Ordered slice of detached handles to poll outside the spinlock.
    pub const fn handles(&self) -> &'a [Handle] {
        self.handles
    }

    /// Number of handles in this scan batch.
    pub const fn len(&self) -> usize {
        self.handles.len()
    }

    /// Returns `true` if no handles were detached into this batch.
    pub const fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

impl Drop for Scan<'_> {
    fn drop(&mut self) {
        debug_assert!(
            self.finished || self.handles.is_empty(),
            "Scan dropped with {} unfinished handle(s); call finish_scan or finish_peek",
            self.handles.len()
        );
    }
}

/// Ready list and per-slot wake state guarded by the level-10e leaf spinlock
/// (Linux `fs/eventpoll.c`, `ep->lock`).
///
/// Every method except [`Self::prepare_grow`] (called without any spinlock
/// held) performs **zero heap allocations and zero heap deallocations**.
pub struct ReadyQueue {
    slots: Vec<ReadySlot>,
    ready: ReadyList,
    scanning: bool,
}

impl fmt::Debug for ReadyQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyQueue")
            .field("capacity", &self.slots.len())
            .field("ready_len", &self.ready.len)
            .field("scanning", &self.scanning)
            .finish()
    }
}

impl Default for ReadyQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadyQueue {
    /// Create an empty ready queue with zero initial slot capacity.
    pub const fn new() -> Self {
        Self { slots: Vec::new(), ready: ReadyList::new(), scanning: false }
    }

    /// Current number of slots backed by this queue.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Number of entries currently linked on the ready list (excluding entries
    /// temporarily detached in an in-flight [`Scan`]).
    pub const fn ready_len(&self) -> usize {
        self.ready.len
    }

    /// Returns `true` if at least one entry is currently on the ready list.
    pub const fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Pre-allocate backing [`Storage`] for at least `min_slots` slots outside
    /// the spinlock (fallible: returns [`Errno::NoMem`] on allocation failure).
    pub fn prepare_grow(min_slots: usize) -> Result<Storage, Errno> {
        if min_slots > u32::MAX as usize {
            return Err(Errno::NoMem);
        }
        let mut slots = Vec::new();
        slots.try_reserve_exact(min_slots).map_err(|_| Errno::NoMem)?;
        slots.resize(min_slots, ReadySlot::vacant());
        Ok(Storage { slots })
    }

    /// Copy existing slot state into `storage` and swap it in under the
    /// spinlock, returning the old [`Storage`] to be dropped *after* releasing
    /// the spinlock.
    ///
    /// If `storage` is no larger than the current capacity, returns `storage`
    /// unchanged. Performs **zero heap allocations and zero deallocations**.
    pub fn install(&mut self, mut storage: Storage) -> Storage {
        if storage.slots.len() <= self.slots.len() {
            return storage;
        }
        storage.slots[..self.slots.len()].copy_from_slice(&self.slots);
        core::mem::swap(&mut self.slots, &mut storage.slots);
        storage
    }

    /// Activate a newly added interest or update an existing interest's watch
    /// mask and armed state under the spinlock (called after [`EpollSet::add`]
    /// or [`EpollSet::modify`]).
    ///
    /// Preserves ready-list membership on an existing armed interest. Performs
    /// **zero heap allocations and zero deallocations**.
    ///
    /// # Panics
    ///
    /// Panics if `handle.index()` is out of bounds (caller must [`Self::install`]
    /// any [`crate::Added::growth`] returned by [`EpollSet::add`] first).
    pub fn set_mask(&mut self, handle: Handle, watch_mask: u32, armed: bool) {
        let idx = handle.index();
        let slot = &mut self.slots[idx as usize];
        let effective_mask = (watch_mask & !EP_PRIVATE_BITS) | EP_ALWAYS_EVENTS;
        if slot.generation == 0 {
            debug_assert!(!slot.in_ready && !slot.in_scan);
            slot.generation = handle.generation();
            slot.watch_mask = effective_mask;
            slot.armed = armed;
            slot.woke_during_scan = false;
        } else if slot.generation == handle.generation() {
            slot.watch_mask = effective_mask;
            slot.armed = armed;
            if !armed && slot.in_ready {
                self.unlink_ready(idx);
            }
        } else {
            debug_assert!(false, "ReadyQueue::set_mask called with stale Handle");
        }
    }

    /// Remove `handle` from the ready queue and invalidate its generation under
    /// the spinlock (called after [`EpollSet::remove`] /
    /// [`EpollSet::remove_range`]).
    ///
    /// Safely ignores stale or out-of-bounds handles. Performs **zero heap
    /// allocations and zero deallocations**.
    pub fn unlink(&mut self, handle: Handle) {
        let idx = handle.index();
        let Some(slot) = self.slots.get(idx as usize) else {
            return;
        };
        if slot.generation != handle.generation() {
            return;
        }
        if slot.in_ready {
            self.unlink_ready(idx);
        }
        self.slots[idx as usize] = ReadySlot::vacant();
    }

    /// Notify the queue that `handle` may have become ready with bits `ready`
    /// (called from `PollWaker::wake` under the level-10e spinlock, matching
    /// Linux `fs/eventpoll.c` `ep_poll_callback`).
    ///
    /// - Rejects out-of-bounds or stale-generation handles with [`Wake::Stale`].
    /// - Ignores disarmed (`EPOLLONESHOT`) interests or non-matching `ready`
    ///   masks with [`Wake::Filtered`].
    /// - If the entry is currently detached in an active [`Scan`], records
    ///   `woke_during_scan` without mutating the ready list and returns
    ///   [`Wake::AlreadyQueued`].
    /// - If already on the ready list, returns [`Wake::AlreadyQueued`].
    /// - Otherwise appends to the tail of the ready list and returns
    ///   [`Wake::Queued`].
    ///
    /// Performs **zero heap allocations and zero deallocations**.
    pub fn wake(&mut self, handle: Handle, ready: u32) -> Wake {
        let idx = handle.index();
        let Some(slot) = self.slots.get_mut(idx as usize) else {
            return Wake::Stale;
        };
        if slot.generation == 0 || slot.generation != handle.generation() {
            return Wake::Stale;
        }
        if !slot.armed || (slot.watch_mask & ready) == 0 {
            return Wake::Filtered;
        }
        if slot.in_scan {
            slot.woke_during_scan = true;
            return Wake::AlreadyQueued;
        }
        if slot.in_ready {
            return Wake::AlreadyQueued;
        }
        self.push_back_ready(idx);
        Wake::Queued
    }

    /// Phase 1 of two-phase collection or nested-epoll readiness peeking
    /// (Linux `fs/eventpoll.c` `ep_start_scan`): detach up to `buf.len()`
    /// handles from the front of the ready list into `buf` under the spinlock.
    ///
    /// Detached entries remain logically queued (`in_scan = true`) until
    /// [`Self::finish_scan`] or [`Self::finish_peek`] completes, so concurrent
    /// wakes on them set `woke_during_scan` without duplicating them on the
    /// ready list. Performs **zero heap allocations and zero deallocations**.
    ///
    /// # Panics
    ///
    /// Panics if another non-empty [`Scan`] is already in flight on this queue.
    pub fn begin_scan<'a>(&mut self, buf: &'a mut [Handle]) -> Scan<'a> {
        assert!(!self.scanning, "concurrent scan on ReadyQueue");
        if buf.is_empty() || self.ready.is_empty() {
            return Scan { handles: &[], finished: true };
        }
        let mut count = 0usize;
        while count < buf.len() {
            let Some(idx) = self.pop_front_ready() else {
                break;
            };
            let slot = &mut self.slots[idx as usize];
            slot.in_scan = true;
            slot.woke_during_scan = false;
            buf[count] = Handle::new(idx, slot.generation);
            count += 1;
        }
        self.scanning = true;
        Scan { handles: &buf[..count], finished: false }
    }

    /// Phase 3 of two-phase collection (Linux `fs/eventpoll.c` `ep_send_events`
    /// and `ep_done_scan`): apply the readiness masks in `polled` (parallel to
    /// `scan.handles()`) under the spinlock while `ep->mtx` holds `&set`.
    ///
    /// For each visited handle up to `scan.len().min(polled.len())` while fewer
    /// than `max` events have been emitted:
    /// - Masks `polled[i]` with the slot's watch mask.
    /// - If zero: drops the entry without disarming `EPOLLONESHOT`, unless a
    ///   matching [`Self::wake`] arrived mid-scan (`woke_during_scan`), in
    ///   which case it is requeued at the tail so the new edge is not lost.
    /// - If non-zero: emits [`Event`] to `out`, then:
    ///   - disarms `EPOLLONESHOT` entries (not requeued even if woken
    ///     mid-scan);
    ///   - requeues level-triggered entries, and edge-triggered entries woken
    ///     mid-scan (`woke_during_scan`), at the tail of the ready list;
    ///   - drops edge-triggered entries not woken mid-scan.
    ///
    /// Unvisited handles in `scan` (past `polled.len()` or after hitting `max`)
    /// are spliced back to the **front** of the ready list in their original
    /// order for round-robin fairness.
    ///
    /// Returns the number of events appended to `out`. Performs **zero heap
    /// allocations and zero deallocations** provided `out` does not allocate.
    pub fn finish_scan<K: Ord + Copy, T>(
        &mut self,
        set: &EpollSet<K, T>,
        mut scan: Scan<'_>,
        polled: &[u32],
        max: usize,
        out: &mut impl Extend<Event>,
    ) -> usize {
        let handles = scan.handles;
        scan.finished = true;
        if handles.is_empty() {
            return 0;
        }
        debug_assert!(self.scanning);
        self.scanning = false;

        let visit_limit = handles.len().min(polled.len());
        let mut visited = 0usize;
        let mut reported = 0usize;

        while visited < visit_limit && reported < max {
            let h = handles[visited];
            let ready_bits = polled[visited];
            visited += 1;

            let idx = h.index();
            let (watch_mask, woke_mid_scan) = {
                let Some(slot) = self.slot_mut_if_live(h) else {
                    continue;
                };
                debug_assert!(slot.armed, "disarmed interest must never enter a scan");
                slot.in_scan = false;
                let woke = core::mem::replace(&mut slot.woke_during_scan, false);
                (slot.watch_mask, woke)
            };

            let Some((events, data)) = set.meta(h) else {
                continue;
            };

            let revents = ready_bits & watch_mask;
            if revents == 0 {
                if woke_mid_scan {
                    self.push_back_ready(idx);
                }
                continue;
            }

            out.extend(core::iter::once(Event { handle: h, events: revents, data }));
            reported += 1;

            if (events & EPOLLONESHOT) != 0 {
                self.slots[idx as usize].armed = false;
            } else if (events & EPOLLET) == 0 || woke_mid_scan {
                self.push_back_ready(idx);
            }
        }

        // Splice unvisited remainder back to the front of `self.ready` in
        // original order (matching Linux `list_splice(&txlist, &ep->rdllist)`).
        self.restore_front(&handles[visited..]);

        reported
    }

    /// Complete a non-consuming nested-epoll readiness probe started by
    /// [`Self::begin_scan`] (Linux `fs/eventpoll.c` `__ep_eventpoll_poll` /
    /// `ep_read_events_proc`).
    ///
    /// Scans `polled` (parallel to `scan.handles()`):
    /// - At the first handle with non-zero `polled[i] & watch_mask`, stops and
    ///   returns `true` (`EPOLLIN | EPOLLRDNORM` for the epoll file), restoring
    ///   that handle and every subsequent handle to the front of the ready list
    ///   without disarming `EPOLLONESHOT` or dropping `EPOLLET`.
    /// - Handles with zero masked readiness are pruned from the ready list
    ///   (without disarming `EPOLLONESHOT`) unless a matching wake arrived
    ///   mid-scan (`woke_during_scan`), in which case they are requeued at the
    ///   tail.
    /// - If no polled handle was ready, restores any unpolled tail of `scan` to
    ///   the front and returns `false`.
    ///
    /// Performs **zero heap allocations and zero deallocations**.
    pub fn finish_peek(&mut self, mut scan: Scan<'_>, polled: &[u32]) -> bool {
        let handles = scan.handles;
        scan.finished = true;
        if handles.is_empty() {
            return false;
        }
        debug_assert!(self.scanning);
        self.scanning = false;

        let visit_limit = handles.len().min(polled.len());
        let mut visited = 0usize;
        let mut found_ready = false;

        while visited < visit_limit {
            let h = handles[visited];
            let ready_bits = polled[visited];
            let idx = h.index();
            let Some(slot) = self.slot_mut_if_live(h) else {
                visited += 1;
                continue;
            };
            if (ready_bits & slot.watch_mask) != 0 {
                found_ready = true;
                break;
            }
            visited += 1;
            slot.in_scan = false;
            let woke = core::mem::replace(&mut slot.woke_during_scan, false);
            if woke && !slot.in_ready {
                self.push_back_ready(idx);
            }
        }

        self.restore_front(&handles[visited..]);
        found_ready
    }

    /// Roll back reported events whose copy-out to user memory failed
    /// (Linux `fs/eventpoll.c` `ep_send_events` `-EFAULT` path).
    ///
    /// For each live handle in `handles` (preserving their relative order at
    /// the front of the ready list): re-arms any disarmed `EPOLLONESHOT` entry
    /// and requeues the entry at the front of the ready list (or flags
    /// `woke_during_scan` if another scan already detached it). Safely ignores
    /// stale handles.
    ///
    /// Performs **zero heap allocations and zero deallocations**.
    pub fn unreport<I>(&mut self, handles: I)
    where
        I: IntoIterator<Item = Handle>,
        I::IntoIter: DoubleEndedIterator,
    {
        for h in handles.into_iter().rev() {
            let idx = h.index();
            let Some(slot) = self.slots.get_mut(idx as usize) else {
                continue;
            };
            if slot.generation == 0 || slot.generation != h.generation() {
                continue;
            }
            slot.armed = true;
            if slot.in_scan {
                slot.woke_during_scan = true;
                continue;
            }
            if slot.in_ready {
                self.unlink_ready(idx);
            }
            self.push_front_ready(idx);
        }
    }

    fn slot_mut_if_live(&mut self, handle: Handle) -> Option<&mut ReadySlot> {
        let slot = self.slots.get_mut(handle.index() as usize)?;
        if slot.generation != 0 && slot.generation == handle.generation() { Some(slot) } else { None }
    }

    fn restore_front(&mut self, handles: &[Handle]) {
        for &h in handles.iter().rev() {
            let idx = h.index();
            let Some(slot) = self.slot_mut_if_live(h) else {
                continue;
            };
            slot.in_scan = false;
            slot.woke_during_scan = false;
            if !slot.in_ready {
                self.push_front_ready(idx);
            }
        }
    }

    fn push_back_ready(&mut self, idx: u32) {
        let old_tail = self.ready.tail;
        {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(!slot.in_ready);
            debug_assert!(slot.prev.is_none());
            debug_assert!(slot.next.is_none());
            slot.in_ready = true;
            slot.prev = old_tail;
            slot.next = None;
        }
        match old_tail {
            Some(t) => self.slots[t as usize].next = Some(idx),
            None => self.ready.head = Some(idx),
        }
        self.ready.tail = Some(idx);
        self.ready.len += 1;
    }

    fn push_front_ready(&mut self, idx: u32) {
        let old_head = self.ready.head;
        {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(!slot.in_ready);
            debug_assert!(slot.prev.is_none());
            debug_assert!(slot.next.is_none());
            slot.in_ready = true;
            slot.prev = None;
            slot.next = old_head;
        }
        match old_head {
            Some(h) => self.slots[h as usize].prev = Some(idx),
            None => self.ready.tail = Some(idx),
        }
        self.ready.head = Some(idx);
        self.ready.len += 1;
    }

    fn pop_front_ready(&mut self) -> Option<u32> {
        let head = self.ready.head?;
        let next = {
            let slot = &mut self.slots[head as usize];
            debug_assert!(slot.in_ready);
            debug_assert!(slot.prev.is_none());
            slot.in_ready = false;
            slot.next.take()
        };
        self.ready.head = next;
        match next {
            Some(n) => self.slots[n as usize].prev = None,
            None => self.ready.tail = None,
        }
        self.ready.len -= 1;
        Some(head)
    }

    fn unlink_ready(&mut self, idx: u32) {
        let (prev, next) = {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(slot.in_ready);
            slot.in_ready = false;
            (slot.prev.take(), slot.next.take())
        };
        match prev {
            Some(p) => self.slots[p as usize].next = next,
            None => self.ready.head = next,
        }
        match next {
            Some(n) => self.slots[n as usize].prev = prev,
            None => self.ready.tail = prev,
        }
        self.ready.len -= 1;
    }

    /// Structural invariant check for tests.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        let mut in_ready_slots = 0usize;
        let mut in_scan_slots = 0usize;
        for slot in &self.slots {
            if slot.generation == 0 {
                assert!(!slot.in_ready && !slot.in_scan && !slot.armed && !slot.woke_during_scan);
                assert!(slot.prev.is_none() && slot.next.is_none());
                continue;
            }
            assert!(!(slot.in_ready && slot.in_scan), "slot cannot be both in_ready and in_scan");
            if slot.in_ready {
                assert!(slot.armed, "disarmed slot must not be in ready list");
                in_ready_slots += 1;
            } else {
                assert!(slot.prev.is_none() && slot.next.is_none());
            }
            if slot.in_scan {
                in_scan_slots += 1;
            } else {
                assert!(!slot.woke_during_scan, "woke_during_scan set while not in_scan");
            }
        }
        assert_eq!(self.scanning, in_scan_slots > 0);

        let mut cur = self.ready.head;
        let mut prev = None;
        let mut walked = 0usize;
        while let Some(idx) = cur {
            walked += 1;
            assert!(walked <= self.slots.len(), "cycle in ReadyQueue ready list");
            let slot = &self.slots[idx as usize];
            assert_ne!(slot.generation, 0);
            assert!(slot.in_ready);
            assert!(slot.armed);
            assert_eq!(slot.prev, prev);
            prev = Some(idx);
            cur = slot.next;
        }
        assert_eq!(prev, self.ready.tail);
        assert_eq!(walked, self.ready.len);
        assert_eq!(walked, in_ready_slots);
    }
}
