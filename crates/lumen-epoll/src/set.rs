//! Interest set and zero-allocation intrusive ready list ([`EpollSet`]).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use crate::{
    EP_ALWAYS_EVENTS, EP_PRIVATE_BITS, EPOLLET, EPOLLEXCLUSIVE, EPOLLEXCLUSIVE_OK_BITS, EPOLLONESHOT, EPOLLWAKEUP,
    Errno,
};

/// Normalize caller-supplied event bits: strip ignored [`EPOLLWAKEUP`] and
/// force [`EPOLLERR`] | [`EPOLLHUP`] on.
const fn normalize_events(events: u32) -> u32 {
    (events & !EPOLLWAKEUP) | EP_ALWAYS_EVENTS
}

struct Entry<K> {
    key: K,
    events: u32,
    data: u64,
    armed: bool,
    in_ready: bool,
    prev: Option<usize>,
    next: Option<usize>,
}

impl<K> Entry<K> {
    const fn watch_mask(&self) -> u32 {
        self.events & !EP_PRIVATE_BITS
    }

    const fn is_oneshot(&self) -> bool {
        (self.events & EPOLLONESHOT) != 0
    }

    const fn is_edge_triggered(&self) -> bool {
        (self.events & EPOLLET) != 0
    }

    const fn is_exclusive(&self) -> bool {
        (self.events & EPOLLEXCLUSIVE) != 0
    }
}

enum Slot<K> {
    Occupied(Entry<K>),
    Vacant { next_free: Option<usize> },
}

#[derive(Default)]
struct ReadyList {
    head: Option<usize>,
    tail: Option<usize>,
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

/// Interest set and ready list for a single `epoll` instance, keyed by `K`
/// (in the kernel: `(open_description_id, fd)`).
///
/// Every registered interest occupies a pre-allocated slab slot carrying
/// intrusive ready-list links so [`Self::wake`], [`Self::collect`], and
/// [`Self::modify`] perform zero heap allocations and zero deallocations.
pub struct EpollSet<K: Ord + Copy> {
    by_key: BTreeMap<K, usize>,
    slots: Vec<Slot<K>>,
    free_head: Option<usize>,
    ready: ReadyList,
}

impl<K: Ord + Copy + fmt::Debug> fmt::Debug for EpollSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EpollSet").field("len", &self.by_key.len()).field("ready_len", &self.ready.len).finish()
    }
}

impl<K: Ord + Copy> Default for EpollSet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Copy> EpollSet<K> {
    /// Create an empty interest set.
    pub const fn new() -> Self {
        Self { by_key: BTreeMap::new(), slots: Vec::new(), free_head: None, ready: ReadyList::new() }
    }

    /// Number of interests currently registered in this set.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Returns `true` if no interests are registered in this set.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Number of interests currently queued on the ready list.
    pub const fn ready_len(&self) -> usize {
        self.ready.len
    }

    /// Returns `true` if at least one interest is currently queued on the
    /// ready list.
    pub const fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Returns `true` if `key` is registered in this set.
    pub fn contains(&self, key: K) -> bool {
        self.by_key.contains_key(&key)
    }

    /// Register `key` with event mask `events` and opaque user `data`
    /// (`EPOLL_CTL_ADD`, Linux `fs/eventpoll.c` `do_epoll_ctl` / `ep_insert`).
    ///
    /// - Refuses [`EPOLLEXCLUSIVE`] combined with any bit outside
    ///   [`EPOLLEXCLUSIVE_OK_BITS`] (including [`EPOLLONESHOT`]) with
    ///   [`Errno::Inval`], checked before duplicate lookup as in Linux.
    /// - Refuses a duplicate `key` with [`Errno::Exist`].
    /// - Accepts and ignores [`EPOLLWAKEUP`]; implicitly includes
    ///   [`EPOLLERR`] and [`EPOLLHUP`].
    ///
    /// Pre-allocates the interest's slot so subsequent [`Self::wake`] calls
    /// never allocate. Callers that observe initial readiness when subscribing
    /// should call [`Self::wake`] after `add`.
    pub fn add(&mut self, key: K, events: u32, data: u64) -> Result<(), Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 && (events & !EPOLLEXCLUSIVE_OK_BITS) != 0 {
            return Err(Errno::Inval);
        }
        if self.by_key.contains_key(&key) {
            return Err(Errno::Exist);
        }

        let entry =
            Entry { key, events: normalize_events(events), data, armed: true, in_ready: false, prev: None, next: None };
        let idx = self.alloc_slot(entry);
        self.by_key.insert(key, idx);
        Ok(())
    }

    /// Update `key`'s event mask and user `data`, re-arming a disarmed
    /// [`EPOLLONESHOT`] interest (`EPOLL_CTL_MOD`, Linux `fs/eventpoll.c`
    /// `do_epoll_ctl` / `ep_modify`).
    ///
    /// - Refuses [`EPOLLEXCLUSIVE`] in `events` with [`Errno::Inval`] before
    ///   looking up `key`, matching Linux `do_epoll_ctl`.
    /// - Returns [`Errno::NoEnt`] if `key` is not in the set.
    /// - Refuses modifying an existing [`EPOLLEXCLUSIVE`] interest with
    ///   [`Errno::Inval`].
    /// - Performs zero heap allocations and zero deallocations.
    ///
    /// Ready-list membership is preserved if `key` is already queued; callers
    /// that re-poll the target after `modify` should call [`Self::wake`] with
    /// the current readiness mask so an already-ready descriptor (including a
    /// freshly re-armed one-shot) is queued.
    pub fn modify(&mut self, key: K, events: u32, data: u64) -> Result<(), Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        let Some(&idx) = self.by_key.get(&key) else {
            return Err(Errno::NoEnt);
        };
        let entry = self.entry_mut(idx);
        if entry.is_exclusive() {
            return Err(Errno::Inval);
        }
        entry.events = normalize_events(events);
        entry.data = data;
        entry.armed = true;
        Ok(())
    }

    /// Remove `key` from the interest set and unlink it from the ready list if
    /// queued (`EPOLL_CTL_DEL`, Linux `fs/eventpoll.c` `ep_remove`).
    ///
    /// Returns [`Errno::NoEnt`] if `key` is not present.
    pub fn remove(&mut self, key: K) -> Result<(), Errno> {
        let Some(idx) = self.by_key.remove(&key) else {
            return Err(Errno::NoEnt);
        };
        if self.entry(idx).in_ready {
            self.unlink_ready(idx);
        }
        self.free_slot(idx);
        Ok(())
    }

    /// Notify the set that `key` may have become ready with bits `ready`
    /// (called from `PollWaker::wake`, Linux `fs/eventpoll.c`
    /// `ep_poll_callback`).
    ///
    /// Queues `key` at the tail of the ready list if `key` is registered,
    /// armed, and its monitored events (including implicit [`EPOLLERR`] and
    /// [`EPOLLHUP`]) intersect `ready`, without duplicating an already-queued
    /// entry. Returns `true` if `key` matched and is armed (so a caller may
    /// wake threads sleeping in `epoll_wait`), and `false` otherwise.
    ///
    /// Performs **zero heap allocations and zero deallocations**.
    pub fn wake(&mut self, key: K, ready: u32) -> bool {
        let Some(&idx) = self.by_key.get(&key) else {
            return false;
        };
        let (armed, matches, already_queued) = {
            let entry = self.entry(idx);
            (entry.armed, (entry.watch_mask() & ready) != 0, entry.in_ready)
        };
        if !armed || !matches {
            return false;
        }
        if !already_queued {
            self.push_back_ready(idx);
        }
        true
    }

    /// Drain up to `max` ready events into `out`, rechecking each queued
    /// interest's current readiness via `poll` (Linux `fs/eventpoll.c`
    /// `ep_send_events`).
    ///
    /// For each queued interest popped from the snapshot of the current ready
    /// list until `max` events have been emitted:
    /// - Calls `poll(key)` and masks the result with the interest's monitored
    ///   event bits (excluding [`EP_PRIVATE_BITS`], including [`EPOLLERR`] and
    ///   [`EPOLLHUP`]).
    /// - If the masked result is zero, drops the interest from the ready list
    ///   without reporting it and without disarming [`EPOLLONESHOT`].
    /// - Otherwise appends `(revents, data)` to `out`, then:
    ///   - disarms the interest if [`EPOLLONESHOT`] is set;
    ///   - drops it from the ready list if [`EPOLLET`] is set;
    ///   - otherwise (level-triggered) requeues it at the tail of the ready
    ///     list.
    ///
    /// Any queued interests not visited because `max` was reached are spliced
    /// back to the **front** of the ready list (ahead of level-triggered items
    /// requeued during this call), giving round-robin fairness across calls.
    ///
    /// Returns the number of events appended to `out`. Performs **zero heap
    /// allocations and zero deallocations** provided `poll` and `out` do not
    /// allocate.
    pub fn collect(&mut self, max: usize, mut poll: impl FnMut(K) -> u32, out: &mut impl Extend<(u32, u64)>) -> usize {
        if max == 0 || self.ready.is_empty() {
            return 0;
        }

        let mut tx = core::mem::replace(&mut self.ready, ReadyList::new());
        let mut reported = 0usize;

        while reported < max {
            let Some(idx) = self.pop_front(&mut tx) else {
                break;
            };
            let (key, watch_mask, data, armed, is_oneshot, is_et) = {
                let entry = self.entry_mut(idx);
                debug_assert!(entry.in_ready);
                entry.in_ready = false;
                (entry.key, entry.watch_mask(), entry.data, entry.armed, entry.is_oneshot(), entry.is_edge_triggered())
            };
            debug_assert!(armed, "disarmed interest must never reside on ready list");
            if !armed {
                continue;
            }

            let revents = poll(key) & watch_mask;
            if revents == 0 {
                continue;
            }

            out.extend(core::iter::once((revents, data)));
            reported += 1;

            if is_oneshot {
                self.entry_mut(idx).armed = false;
            } else if !is_et {
                self.push_back_ready(idx);
            }
        }

        if !tx.is_empty() {
            self.prepend_ready(tx);
        }

        reported
    }

    fn alloc_slot(&mut self, entry: Entry<K>) -> usize {
        if let Some(idx) = self.free_head {
            let next_free = match self.slots[idx] {
                Slot::Vacant { next_free } => next_free,
                Slot::Occupied(_) => unreachable!("free list pointed to occupied slot"),
            };
            self.free_head = next_free;
            self.slots[idx] = Slot::Occupied(entry);
            idx
        } else {
            let idx = self.slots.len();
            self.slots.push(Slot::Occupied(entry));
            idx
        }
    }

    fn free_slot(&mut self, idx: usize) {
        self.slots[idx] = Slot::Vacant { next_free: self.free_head };
        self.free_head = Some(idx);
    }

    fn entry(&self, idx: usize) -> &Entry<K> {
        match &self.slots[idx] {
            Slot::Occupied(e) => e,
            Slot::Vacant { .. } => unreachable!("index pointed to vacant slot"),
        }
    }

    fn entry_mut(&mut self, idx: usize) -> &mut Entry<K> {
        match &mut self.slots[idx] {
            Slot::Occupied(e) => e,
            Slot::Vacant { .. } => unreachable!("index pointed to vacant slot"),
        }
    }

    fn push_back_ready(&mut self, idx: usize) {
        let old_tail = self.ready.tail;
        {
            let entry = self.entry_mut(idx);
            debug_assert!(!entry.in_ready);
            debug_assert!(entry.prev.is_none());
            debug_assert!(entry.next.is_none());
            entry.in_ready = true;
            entry.prev = old_tail;
            entry.next = None;
        }
        match old_tail {
            Some(t) => self.entry_mut(t).next = Some(idx),
            None => self.ready.head = Some(idx),
        }
        self.ready.tail = Some(idx);
        self.ready.len += 1;
    }

    fn pop_front(&mut self, list: &mut ReadyList) -> Option<usize> {
        let head = list.head?;
        let next = {
            let entry = self.entry_mut(head);
            debug_assert!(entry.prev.is_none());
            entry.next.take()
        };
        list.head = next;
        match next {
            Some(n) => self.entry_mut(n).prev = None,
            None => list.tail = None,
        }
        list.len -= 1;
        Some(head)
    }

    fn unlink_ready(&mut self, idx: usize) {
        let (prev, next) = {
            let entry = self.entry_mut(idx);
            debug_assert!(entry.in_ready);
            entry.in_ready = false;
            (entry.prev.take(), entry.next.take())
        };
        match prev {
            Some(p) => self.entry_mut(p).next = next,
            None => self.ready.head = next,
        }
        match next {
            Some(n) => self.entry_mut(n).prev = prev,
            None => self.ready.tail = prev,
        }
        self.ready.len -= 1;
    }

    /// Splice non-empty `front` ahead of `self.ready` in O(1) (matching Linux
    /// `list_splice(&txlist, &ep->rdllist)` in `ep_send_events`).
    fn prepend_ready(&mut self, front: ReadyList) {
        debug_assert!(!front.is_empty());
        if self.ready.is_empty() {
            self.ready = front;
            return;
        }
        let front_tail = front.tail.expect("non-empty list must have tail");
        let back_head = self.ready.head.expect("non-empty list must have head");
        debug_assert!(self.entry(front_tail).next.is_none());
        debug_assert!(self.entry(back_head).prev.is_none());
        self.entry_mut(front_tail).next = Some(back_head);
        self.entry_mut(back_head).prev = Some(front_tail);
        self.ready.head = front.head;
        self.ready.len += front.len;
    }

    /// Structural invariant check for tests.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        let mut occupied = 0usize;
        let mut in_ready_slots = 0usize;
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Slot::Occupied(e) = slot {
                occupied += 1;
                assert_eq!(self.by_key.get(&e.key), Some(&idx));
                if e.in_ready {
                    assert!(e.armed, "disarmed entry must not be in ready list");
                    in_ready_slots += 1;
                } else {
                    assert!(e.prev.is_none());
                    assert!(e.next.is_none());
                }
            }
        }
        assert_eq!(occupied, self.by_key.len());

        // Check free list visits every vacant slot exactly once.
        let mut vacant_seen = 0usize;
        let mut cur_free = self.free_head;
        while let Some(idx) = cur_free {
            vacant_seen += 1;
            assert!(vacant_seen <= self.slots.len(), "cycle in free list");
            match self.slots[idx] {
                Slot::Vacant { next_free } => cur_free = next_free,
                Slot::Occupied(_) => panic!("free list hit occupied slot {idx}"),
            }
        }
        assert_eq!(occupied + vacant_seen, self.slots.len());

        // Check ready list links, symmetry, head/tail, and count.
        let mut cur = self.ready.head;
        let mut prev = None;
        let mut walked = 0usize;
        while let Some(idx) = cur {
            walked += 1;
            assert!(walked <= self.slots.len(), "cycle in ready list");
            let e = self.entry(idx);
            assert!(e.in_ready);
            assert!(e.armed);
            assert_eq!(e.prev, prev);
            prev = Some(idx);
            cur = e.next;
        }
        assert_eq!(prev, self.ready.tail);
        assert_eq!(walked, self.ready.len);
        assert_eq!(walked, in_ready_slots);
    }
}
