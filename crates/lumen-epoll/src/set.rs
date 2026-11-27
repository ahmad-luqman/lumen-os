//! Interest table ([`EpollSet<K, T>`]), guarded by B5's sleep mutex
//! (Linux `fs/eventpoll.c`, `ep->mtx`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;
use core::ops::{Bound, RangeBounds};

use crate::{
    EP_ALWAYS_EVENTS, EP_PRIVATE_BITS, EPOLLEXCLUSIVE, EPOLLEXCLUSIVE_OK_BITS, EPOLLWAKEUP, Errno, Handle, ReadyQueue,
    Storage,
};

/// Initial [`ReadyQueue`] slot capacity prepared on the first [`EpollSet::add`].
const INITIAL_READY_CAPACITY: usize = 16;

/// Normalize caller-supplied event bits: strip ignored [`EPOLLWAKEUP`] and
/// force [`EPOLLERR`] | [`EPOLLHUP`] on.
const fn normalize_events(events: u32) -> u32 {
    (events & !EPOLLWAKEUP) | EP_ALWAYS_EVENTS
}

/// Bump a slot generation on free, skipping `0` (reserved for inactive slots in
/// [`ReadyQueue`]).
const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

/// Result of [`EpollSet::add`].
#[derive(Debug)]
#[must_use = "install any growth Storage and call ReadyQueue::set_mask under the spinlock"]
pub struct Added {
    /// Generational handle for the new interest.
    pub handle: Handle,
    /// Effective watch mask (`(events & !EP_PRIVATE_BITS) | EPOLLERR | EPOLLHUP`)
    /// to register via [`ReadyQueue::set_mask`].
    pub watch_mask: u32,
    /// Pre-allocated backing buffer when the paired [`ReadyQueue`] needs to
    /// grow. Pass to [`ReadyQueue::install`] under the spinlock and drop the
    /// returned old [`Storage`] *after* unlocking.
    pub growth: Option<Storage>,
}

/// Result of [`EpollSet::modify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "call ReadyQueue::set_mask under the spinlock"]
pub struct Modified {
    /// Generational handle of the modified interest.
    pub handle: Handle,
    /// Updated effective watch mask to pass to [`ReadyQueue::set_mask`].
    pub watch_mask: u32,
}

/// Borrowed view of one live interest returned by [`EpollSet::get`].
#[derive(Debug)]
pub struct EntryRef<'a, K, T> {
    /// Interest key (in B5: `(open_description_id, fd)`).
    pub key: K,
    /// Generational handle of this interest.
    pub handle: Handle,
    /// Normalized event mask (with `EPOLLWAKEUP` stripped and
    /// `EPOLLERR | EPOLLHUP` forced on).
    pub events: u32,
    /// Opaque user `data`.
    pub data: u64,
    /// Caller payload (in B5: target file description and `PollGuard`).
    pub payload: &'a T,
}

impl<K, T> EntryRef<'_, K, T> {
    /// Effective watch mask (excluding [`EP_PRIVATE_BITS`]).
    pub const fn watch_mask(&self) -> u32 {
        self.events & !EP_PRIVATE_BITS
    }
}

struct SetEntry<K, T> {
    key: K,
    generation: u32,
    events: u32,
    data: u64,
    payload: T,
}

impl<K, T> SetEntry<K, T> {
    const fn watch_mask(&self) -> u32 {
        self.events & !EP_PRIVATE_BITS
    }

    const fn is_exclusive(&self) -> bool {
        (self.events & EPOLLEXCLUSIVE) != 0
    }
}

enum SetSlot<K, T> {
    Occupied(SetEntry<K, T>),
    Vacant { next_free: Option<u32>, next_gen: u32 },
}

/// Interest table for a single `epoll` instance, keyed by `K` (in the kernel:
/// `(open_description_id, fd)`) with per-interest payload `T` (in the kernel:
/// target description + `PollGuard`).
///
/// Designed to sit behind B5's sleep mutex (`ep->mtx`), paired with a
/// [`ReadyQueue`] behind the level-10e leaf spinlock (`ep->lock`).
pub struct EpollSet<K: Ord + Copy, T = ()> {
    by_key: BTreeMap<K, u32>,
    slots: Vec<SetSlot<K, T>>,
    free_head: Option<u32>,
    ready_capacity: usize,
}

impl<K: Ord + Copy + fmt::Debug, T> fmt::Debug for EpollSet<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EpollSet")
            .field("len", &self.by_key.len())
            .field("slots", &self.slots.len())
            .field("ready_capacity", &self.ready_capacity)
            .finish()
    }
}

impl<K: Ord + Copy, T> Default for EpollSet<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Copy, T> EpollSet<K, T> {
    /// Create an empty interest table.
    pub const fn new() -> Self {
        Self { by_key: BTreeMap::new(), slots: Vec::new(), free_head: None, ready_capacity: 0 }
    }

    /// Number of interests currently registered in this set.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Returns `true` if no interests are registered in this set.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Returns `true` if `key` is registered in this set.
    pub fn contains(&self, key: K) -> bool {
        self.by_key.contains_key(&key)
    }

    /// Look up the live [`Handle`] for `key`, if registered.
    pub fn lookup(&self, key: K) -> Option<Handle> {
        let &idx = self.by_key.get(&key)?;
        match &self.slots[idx as usize] {
            SetSlot::Occupied(e) => Some(Handle::new(idx, e.generation)),
            SetSlot::Vacant { .. } => unreachable!("by_key pointed to vacant slot"),
        }
    }

    /// Look up a live interest by [`Handle`] in O(1), rejecting stale
    /// generations.
    pub fn get(&self, handle: Handle) -> Option<EntryRef<'_, K, T>> {
        let slot = self.slots.get(handle.index() as usize)?;
        match slot {
            SetSlot::Occupied(e) if e.generation == handle.generation() => {
                Some(EntryRef { key: e.key, handle, events: e.events, data: e.data, payload: &e.payload })
            }
            _ => None,
        }
    }

    /// Mutably borrow the payload `T` of a live interest by [`Handle`] in O(1)
    /// (for example, to store a `PollGuard` after subscribing), rejecting stale
    /// generations.
    pub fn payload_mut(&mut self, handle: Handle) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.index() as usize)?;
        match slot {
            SetSlot::Occupied(e) if e.generation == handle.generation() => Some(&mut e.payload),
            _ => None,
        }
    }

    /// Internal O(1) metadata lookup used by [`ReadyQueue::finish_scan`].
    pub(crate) fn meta(&self, handle: Handle) -> Option<(u32, u64)> {
        let slot = self.slots.get(handle.index() as usize)?;
        match slot {
            SetSlot::Occupied(e) if e.generation == handle.generation() => Some((e.events, e.data)),
            _ => None,
        }
    }

    /// Register `key` with event mask `events`, user `data`, and payload `T`
    /// (`EPOLL_CTL_ADD`, Linux `fs/eventpoll.c` `do_epoll_ctl` / `ep_insert`).
    ///
    /// - Refuses [`EPOLLEXCLUSIVE`] combined with any bit outside
    ///   [`EPOLLEXCLUSIVE_OK_BITS`] (including [`EPOLLONESHOT`]) with
    ///   [`Errno::Inval`], checked before duplicate lookup as in Linux.
    /// - Refuses a duplicate `key` with [`Errno::Exist`].
    /// - Uses fallible reservation (`try_reserve` + [`ReadyQueue::prepare_grow`])
    ///   so out-of-memory returns [`Errno::NoMem`] with the set unchanged.
    /// - Accepts and ignores [`EPOLLWAKEUP`]; implicitly includes
    ///   [`EPOLLERR`] and [`EPOLLHUP`].
    ///
    /// After `add`, the caller must install any returned [`Added::growth`] and
    /// call [`ReadyQueue::set_mask`] under the spinlock, then subscribe, poll
    /// the target, and call [`ReadyQueue::wake`] with the initial mask.
    pub fn add(&mut self, key: K, events: u32, data: u64, payload: T) -> Result<Added, Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 && (events & !EPOLLEXCLUSIVE_OK_BITS) != 0 {
            return Err(Errno::Inval);
        }
        if self.by_key.contains_key(&key) {
            return Err(Errno::Exist);
        }

        let norm = normalize_events(events);
        let watch_mask = norm & !EP_PRIVATE_BITS;

        if let Some(idx) = self.free_head {
            let (next_free, generation) = match self.slots[idx as usize] {
                SetSlot::Vacant { next_free, next_gen } => (next_free, next_gen),
                SetSlot::Occupied(_) => unreachable!("free list pointed to occupied slot"),
            };
            self.free_head = next_free;
            self.slots[idx as usize] = SetSlot::Occupied(SetEntry { key, generation, events: norm, data, payload });
            self.by_key.insert(key, idx);
            return Ok(Added { handle: Handle::new(idx, generation), watch_mask, growth: None });
        }

        if self.slots.len() >= u32::MAX as usize {
            return Err(Errno::NoMem);
        }

        // Fallible reservations first so `self` is unchanged on `Errno::NoMem`.
        self.slots.try_reserve(1).map_err(|_| Errno::NoMem)?;
        let needed = self.slots.len() + 1;
        let growth = if needed > self.ready_capacity {
            let new_cap =
                self.ready_capacity.saturating_mul(2).max(INITIAL_READY_CAPACITY).max(needed).min(u32::MAX as usize);
            Some(ReadyQueue::prepare_grow(new_cap)?)
        } else {
            None
        };

        if let Some(ref storage) = growth {
            self.ready_capacity = storage.capacity();
        }
        let idx = self.slots.len() as u32;
        let generation = 1u32;
        self.slots.push(SetSlot::Occupied(SetEntry { key, generation, events: norm, data, payload }));
        self.by_key.insert(key, idx);
        Ok(Added { handle: Handle::new(idx, generation), watch_mask, growth })
    }

    /// Update `key`'s event mask and user `data` (`EPOLL_CTL_MOD`, Linux
    /// `fs/eventpoll.c` `do_epoll_ctl` / `ep_modify`).
    ///
    /// - Refuses [`EPOLLEXCLUSIVE`] in `events` with [`Errno::Inval`] before
    ///   looking up `key`, matching Linux `do_epoll_ctl`.
    /// - Returns [`Errno::NoEnt`] if `key` is not in the set.
    /// - Refuses modifying an existing [`EPOLLEXCLUSIVE`] interest with
    ///   [`Errno::Inval`].
    /// - Performs zero heap allocations and zero deallocations.
    ///
    /// After `modify`, the caller must call
    /// `rq.set_mask(modified.handle, modified.watch_mask, true)` under the
    /// spinlock (re-arming any disarmed `EPOLLONESHOT`), then re-poll the
    /// target and call [`ReadyQueue::wake`] with the current readiness mask.
    pub fn modify(&mut self, key: K, events: u32, data: u64) -> Result<Modified, Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        let Some(&idx) = self.by_key.get(&key) else {
            return Err(Errno::NoEnt);
        };
        let entry = match &mut self.slots[idx as usize] {
            SetSlot::Occupied(e) => e,
            SetSlot::Vacant { .. } => unreachable!("by_key pointed to vacant slot"),
        };
        if entry.is_exclusive() {
            return Err(Errno::Inval);
        }
        entry.events = normalize_events(events);
        entry.data = data;
        Ok(Modified { handle: Handle::new(idx, entry.generation), watch_mask: entry.watch_mask() })
    }

    /// Remove `key` from the interest set, returning its `(Handle, T)` so the
    /// caller can call [`ReadyQueue::unlink`] under the spinlock and drop `T`
    /// after releasing locks (`EPOLL_CTL_DEL`, Linux `fs/eventpoll.c`
    /// `ep_remove`).
    ///
    /// Returns [`Errno::NoEnt`] if `key` is not present.
    pub fn remove(&mut self, key: K) -> Result<(Handle, T), Errno> {
        let Some(idx) = self.by_key.remove(&key) else {
            return Err(Errno::NoEnt);
        };
        Ok(self.free_slot(idx))
    }

    /// Remove every interest whose key falls within `range` (for example, all
    /// `(open_description_id, fd)` entries of one closing description),
    /// yielding each `(K, Handle, T)` without allocating.
    ///
    /// Dropping the returned iterator before exhaustion still removes the
    /// remaining keys in `range`.
    pub fn remove_range<R: RangeBounds<K>>(&mut self, range: R) -> RemoveRange<'_, K, T> {
        let bounds = (range.start_bound().cloned(), range.end_bound().cloned());
        RemoveRange { set: self, bounds }
    }

    /// Remove every interest from the set (for tearing down a closing epoll
    /// instance), yielding each `(K, Handle, T)` without allocating.
    ///
    /// Dropping the returned iterator before exhaustion still empties the set.
    pub fn drain(&mut self) -> Drain<'_, K, T> {
        Drain { set: self }
    }

    fn free_slot(&mut self, idx: u32) -> (Handle, T) {
        let old = core::mem::replace(
            &mut self.slots[idx as usize],
            SetSlot::Vacant { next_free: self.free_head, next_gen: 1 },
        );
        let SetSlot::Occupied(entry) = old else {
            unreachable!("free_slot called on vacant slot");
        };
        let handle = Handle::new(idx, entry.generation);
        self.slots[idx as usize] =
            SetSlot::Vacant { next_free: self.free_head, next_gen: next_generation(entry.generation) };
        self.free_head = Some(idx);
        (handle, entry.payload)
    }

    /// Structural invariant check for tests.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        let mut occupied = 0usize;
        for (idx, slot) in self.slots.iter().enumerate() {
            match slot {
                SetSlot::Occupied(e) => {
                    occupied += 1;
                    assert_ne!(e.generation, 0, "live slot must have non-zero generation");
                    assert_eq!(self.by_key.get(&e.key), Some(&(idx as u32)));
                }
                SetSlot::Vacant { next_gen, .. } => {
                    assert_ne!(*next_gen, 0, "vacant slot next_gen must be non-zero");
                }
            }
        }
        assert_eq!(occupied, self.by_key.len());
        assert!(self.slots.len() <= self.ready_capacity || self.slots.is_empty());

        let mut vacant_seen = 0usize;
        let mut cur = self.free_head;
        while let Some(idx) = cur {
            vacant_seen += 1;
            assert!(vacant_seen <= self.slots.len(), "cycle in EpollSet free list");
            match self.slots[idx as usize] {
                SetSlot::Vacant { next_free, .. } => cur = next_free,
                SetSlot::Occupied(_) => panic!("EpollSet free list hit occupied slot {idx}"),
            }
        }
        assert_eq!(occupied + vacant_seen, self.slots.len());
    }
}

/// Allocation-free draining iterator returned by [`EpollSet::remove_range`].
pub struct RemoveRange<'a, K: Ord + Copy, T> {
    set: &'a mut EpollSet<K, T>,
    bounds: (Bound<K>, Bound<K>),
}

impl<K: Ord + Copy, T> Iterator for RemoveRange<'_, K, T> {
    type Item = (K, Handle, T);

    fn next(&mut self) -> Option<Self::Item> {
        let key = *self.set.by_key.range(self.bounds).next()?.0;
        let idx = self.set.by_key.remove(&key).expect("key just found in range");
        let (handle, payload) = self.set.free_slot(idx);
        Some((key, handle, payload))
    }
}

impl<K: Ord + Copy, T> Drop for RemoveRange<'_, K, T> {
    fn drop(&mut self) {
        while self.next().is_some() {}
    }
}

/// Allocation-free draining iterator returned by [`EpollSet::drain`].
pub struct Drain<'a, K: Ord + Copy, T> {
    set: &'a mut EpollSet<K, T>,
}

impl<K: Ord + Copy, T> Iterator for Drain<'_, K, T> {
    type Item = (K, Handle, T);

    fn next(&mut self) -> Option<Self::Item> {
        let (key, idx) = self.set.by_key.pop_first()?;
        let (handle, payload) = self.set.free_slot(idx);
        Some((key, handle, payload))
    }
}

impl<K: Ord + Copy, T> Drop for Drain<'_, K, T> {
    fn drop(&mut self) {
        while self.next().is_some() {}
    }
}
