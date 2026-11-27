use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec;
use alloc::vec::Vec;
use linux_raw_sys::{errno as raw_errno, general as raw_general};

use super::*;

type Key = (u64, u32);

fn key(desc: u64, fd: u32) -> Key {
    (desc, fd)
}

/// Deterministic SplitMix64 PRNG for model tests.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }
}

/// Test helper pairing `EpollSet` (ep->mtx) with `ReadyQueue` (level-10e spin).
struct Pair<T = u32> {
    set: EpollSet<Key, T>,
    rq: ReadyQueue,
}

impl<T> Pair<T> {
    fn new() -> Self {
        Self { set: EpollSet::new(), rq: ReadyQueue::new() }
    }

    fn add(&mut self, k: Key, events: u32, data: u64, payload: T) -> Result<Handle, Errno> {
        let added = self.set.add(k, events, data, payload)?;
        let old = added.growth.map(|s| self.rq.install(s));
        self.rq.set_mask(added.handle, added.watch_mask, true);
        drop(old);
        Ok(added.handle)
    }

    fn modify(&mut self, k: Key, events: u32, data: u64) -> Result<Handle, Errno> {
        let modified = self.set.modify(k, events, data)?;
        self.rq.set_mask(modified.handle, modified.watch_mask, true);
        Ok(modified.handle)
    }

    fn remove(&mut self, k: Key) -> Result<(Handle, T), Errno> {
        let (h, payload) = self.set.remove(k)?;
        self.rq.unlink(h);
        Ok((h, payload))
    }

    /// Run a complete two-phase scan with an optional hook executed between
    /// `begin_scan` and `finish_scan` (after polling, simulating concurrent
    /// wakes while the spinlock is dropped).
    fn scan_with(
        &mut self,
        max: usize,
        mut poll: impl FnMut(Key, &T) -> u32,
        mid_scan: impl FnOnce(&mut ReadyQueue),
    ) -> Vec<Event> {
        let mut hbuf = vec![Handle::new(0, 0); self.rq.ready_len().max(1)];
        let scan = self.rq.begin_scan(&mut hbuf);
        let mut polled = Vec::with_capacity(scan.len());
        for &h in scan.handles() {
            let e = self.set.get(h).expect("handle in scan must be live in set");
            polled.push(poll(e.key, e.payload));
        }
        mid_scan(&mut self.rq);
        let mut out = Vec::new();
        let n = self.rq.finish_scan(&self.set, scan, &polled, max, &mut out);
        assert_eq!(n, out.len());
        out
    }

    fn scan(&mut self, max: usize, poll: impl FnMut(Key, &T) -> u32) -> Vec<Event> {
        self.scan_with(max, poll, |_| {})
    }

    fn assert_invariants(&self) {
        self.set.assert_invariants();
        self.rq.assert_invariants();
    }
}

#[test]
fn constants_and_errnos_match_linux_raw_sys() {
    // Linux `include/uapi/linux/eventpoll.h` and `fs/eventpoll.c`.
    assert_eq!(EPOLLIN, raw_general::EPOLLIN);
    assert_eq!(EPOLLPRI, raw_general::EPOLLPRI);
    assert_eq!(EPOLLOUT, raw_general::EPOLLOUT);
    assert_eq!(EPOLLERR, raw_general::EPOLLERR);
    assert_eq!(EPOLLHUP, raw_general::EPOLLHUP);
    assert_eq!(EPOLLNVAL, raw_general::EPOLLNVAL);
    assert_eq!(EPOLLRDNORM, raw_general::EPOLLRDNORM);
    assert_eq!(EPOLLRDBAND, raw_general::EPOLLRDBAND);
    assert_eq!(EPOLLWRNORM, raw_general::EPOLLWRNORM);
    assert_eq!(EPOLLWRBAND, raw_general::EPOLLWRBAND);
    assert_eq!(EPOLLMSG, raw_general::EPOLLMSG);
    assert_eq!(EPOLLRDHUP, raw_general::EPOLLRDHUP);
    assert_eq!(EPOLLEXCLUSIVE, raw_general::EPOLLEXCLUSIVE);
    assert_eq!(EPOLLWAKEUP, raw_general::EPOLLWAKEUP);
    assert_eq!(EPOLLONESHOT, raw_general::EPOLLONESHOT);
    assert_eq!(EPOLLET, raw_general::EPOLLET);
    assert_eq!(EPOLL_CLOEXEC, raw_general::EPOLL_CLOEXEC);
    assert_eq!(EPOLL_CTL_ADD, raw_general::EPOLL_CTL_ADD);
    assert_eq!(EPOLL_CTL_DEL, raw_general::EPOLL_CTL_DEL);
    assert_eq!(EPOLL_CTL_MOD, raw_general::EPOLL_CTL_MOD);

    // Pin composite bitmasks to exact Linux x86-64 / UAPI literals.
    assert_eq!(EP_PRIVATE_BITS, 0xF000_0000);
    assert_eq!(EP_ALWAYS_EVENTS, 0x0000_0018);
    assert_eq!(EPOLLEXCLUSIVE_OK_BITS, 0xB000_001D);

    // Linux `fs/eventpoll.c`: `#define EP_MAX_NESTS 4`.
    assert_eq!(EP_MAX_NESTS, 4);

    assert_eq!(Errno::NoEnt.to_errno(), raw_errno::ENOENT);
    assert_eq!(Errno::NoMem.to_errno(), raw_errno::ENOMEM);
    assert_eq!(Errno::Exist.to_errno(), raw_errno::EEXIST);
    assert_eq!(Errno::Inval.to_errno(), raw_errno::EINVAL);
    assert_eq!(Errno::Loop.to_errno(), raw_errno::ELOOP);
    assert_eq!(alloc::format!("{}", Errno::NoEnt), "ENOENT");
    assert_eq!(alloc::format!("{}", Errno::NoMem), "ENOMEM");
    assert_eq!(alloc::format!("{}", Errno::Exist), "EEXIST");
    assert_eq!(alloc::format!("{}", Errno::Inval), "EINVAL");
    assert_eq!(alloc::format!("{}", Errno::Loop), "ELOOP");
}

#[test]
fn eexist_enoent_and_einval_cases() {
    let mut p = Pair::<u32>::new();
    let k1 = key(1, 3);
    let k2 = key(2, 4);
    let k3 = key(3, 5);
    let k4 = key(4, 6);

    // ENOENT on missing modify and remove (Linux `fs/eventpoll.c` `do_epoll_ctl`).
    assert_eq!(p.modify(k1, EPOLLIN, 1), Err(Errno::NoEnt));
    assert_eq!(p.remove(k1), Err(Errno::NoEnt));

    // EINVAL for EPOLLEXCLUSIVE + EPOLLONESHOT and disallowed event bits
    // (Linux `fs/eventpoll.c`, `EPOLLEXCLUSIVE_OK_BITS`).
    assert_eq!(p.add(k1, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 1, 100), Err(Errno::Inval));
    assert_eq!(p.add(k1, EPOLLPRI | EPOLLEXCLUSIVE, 1, 100), Err(Errno::Inval));
    assert_eq!(p.add(k1, EPOLLRDHUP | EPOLLEXCLUSIVE, 1, 100), Err(Errno::Inval));
    assert_eq!(p.add(k1, EPOLLRDNORM | EPOLLEXCLUSIVE, 1, 100), Err(Errno::Inval));
    assert!(!p.set.contains(k1));

    // Positive literal EPOLLEXCLUSIVE combinations allowed by Linux.
    assert!(p.add(k2, EPOLLIN | EPOLLET | EPOLLEXCLUSIVE, 20, 200).is_ok());
    assert!(p.add(k3, EPOLLOUT | EPOLLEXCLUSIVE, 30, 300).is_ok());
    assert!(p.add(k4, EPOLLIN | EPOLLWAKEUP | EPOLLEXCLUSIVE, 40, 400).is_ok());

    // Normal add succeeds; duplicate returns EEXIST.
    let h1 = p.add(k1, EPOLLIN, 10, 101).unwrap();
    assert_eq!(p.set.lookup(k1), Some(h1));
    assert_eq!(p.add(k1, EPOLLOUT, 20, 102), Err(Errno::Exist));

    // Linux `do_epoll_ctl` checks EPOLLEXCLUSIVE validity before duplicate lookup:
    // invalid flags on an existing key return EINVAL, not EEXIST.
    assert_eq!(p.add(k1, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 30, 103), Err(Errno::Inval));

    // Linux `do_epoll_ctl` rejects EPOLLEXCLUSIVE in EPOLL_CTL_MOD before lookup:
    // EINVAL even when key is missing, and EINVAL when key exists.
    assert_eq!(p.modify(key(99, 99), EPOLLIN | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));
    assert_eq!(p.modify(k1, EPOLLIN | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));

    // Modifying an existing EPOLLEXCLUSIVE interest returns EINVAL
    // (Linux `fs/eventpoll.c` `do_epoll_ctl`: `if (epi->event.events & EPOLLEXCLUSIVE) error = -EINVAL`).
    assert_eq!(p.modify(k2, EPOLLIN, 43), Err(Errno::Inval));
    assert_eq!(p.modify(k3, EPOLLOUT | EPOLLET, 44), Err(Errno::Inval));

    // Removing returns `(Handle, payload)` and allows re-adding as non-exclusive.
    let (_, payload) = p.remove(k2).unwrap();
    assert_eq!(payload, 200);
    assert_eq!(p.remove(k2), Err(Errno::NoEnt));
    assert!(p.add(k2, EPOLLIN, 50, 201).is_ok());
    assert!(p.modify(k2, EPOLLOUT, 51).is_ok());
    p.assert_invariants();
}

#[test]
fn err_and_hup_always_reported_and_wakeup_ignored() {
    let mut p = Pair::<u32>::new();
    let k_empty = key(1, 0);
    let k_wakeup = key(2, 1);

    // Interest with events == 0 still monitors EPOLLERR and EPOLLHUP
    // (Linux `fs/eventpoll.c` `do_epoll_ctl`: `epds->events |= EPOLLERR | EPOLLHUP`).
    let h_empty = p.add(k_empty, 0, 100, 1).unwrap();
    assert_eq!(p.rq.wake(h_empty, EPOLLIN | EPOLLOUT), Wake::Filtered);
    assert!(!p.rq.has_ready());
    assert_eq!(p.rq.wake(h_empty, EPOLLERR), Wake::Queued);
    assert!(p.rq.has_ready());

    let out = p.scan(4, |_, _| EPOLLIN | EPOLLERR | EPOLLHUP | EPOLLET | EPOLLWAKEUP);
    assert_eq!(out, vec![Event { handle: h_empty, events: EPOLLERR | EPOLLHUP, data: 100 }]);

    // EPOLLWAKEUP is accepted and ignored; private modifier bits never wake or appear in revents.
    let h_wakeup = p.add(k_wakeup, EPOLLIN | EPOLLWAKEUP, 200, 2).unwrap();
    assert_eq!(p.rq.wake(h_wakeup, EPOLLWAKEUP), Wake::Filtered);
    assert_eq!(p.rq.wake(h_wakeup, EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE), Wake::Filtered);
    assert_eq!(p.rq.wake(h_wakeup, EPOLLIN), Wake::Queued);

    let out = p.scan(4, |k, _| if k == k_wakeup { EPOLLIN | EPOLLWAKEUP | EPOLLET } else { 0 });
    assert_eq!(out, vec![Event { handle: h_wakeup, events: EPOLLIN, data: 200 }]);
    p.assert_invariants();
}

#[test]
fn lt_requeue_fairness_round_robin() {
    // Linux `fs/eventpoll.c` `ep_send_events`: LT items are requeued at the
    // tail of `rdllist`, and unvisited items left in `txlist` when `max` is
    // reached are spliced back to the *front* of `rdllist`.
    let mut p = Pair::<u32>::new();
    for i in 0..5u32 {
        let h = p.add(key(u64::from(i), i), EPOLLIN, u64::from(i), i).unwrap();
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    // max == 0: reports nothing, preserves exact ready-list order.
    let out = p.scan(0, |_, _| EPOLLIN);
    assert!(out.is_empty());
    assert_eq!(p.rq.ready_len(), 5);

    // Round-robin with max = 2 across 6 calls:
    // [0,1], [2,3], [4,0], [1,2], [3,4], [0,1].
    let expected_batches: &[&[u64]] = &[&[0, 1], &[2, 3], &[4, 0], &[1, 2], &[3, 4], &[0, 1]];
    for &expected in expected_batches {
        let out = p.scan(2, |_, _| EPOLLIN);
        let got: Vec<u64> = out.iter().map(|e| e.data).collect();
        assert_eq!(got, expected);
        p.assert_invariants();
    }

    // Large max reports each ready LT item exactly once per scan.
    let out = p.scan(100, |_, _| EPOLLIN);
    let got: Vec<u64> = out.iter().map(|e| e.data).collect();
    assert_eq!(got, vec![2, 3, 4, 0, 1]);

    // When an LT item becomes unready (poll returns 0), it is dropped from the
    // ready list without starving remaining items.
    let out = p.scan(2, |k, _| if k.1 == 2 || k.1 == 3 { 0 } else { EPOLLIN });
    let got: Vec<u64> = out.iter().map(|e| e.data).collect();
    assert_eq!(got, vec![4, 0]);
    assert_eq!(p.rq.ready_len(), 3); // [1, 4, 0]

    let out = p.scan(1, |_, _| EPOLLIN);
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), vec![1]);
    p.assert_invariants();
}

#[test]
fn et_reported_once_per_wake() {
    // Linux `fs/eventpoll.c` `ep_send_events`: items with EPOLLET are not
    // re-added to `rdllist` after reporting; only a subsequent wake queues
    // them again.
    let mut p = Pair::<u32>::new();
    let h = p.add(key(10, 3), EPOLLIN | EPOLLET, 77, 1).unwrap();

    // Not woken yet: scan reports nothing even if poll would return EPOLLIN.
    assert!(p.scan(4, |_, _| EPOLLIN).is_empty());

    // Non-matching wake does not queue.
    assert_eq!(p.rq.wake(h, EPOLLOUT), Wake::Filtered);
    assert!(!p.rq.has_ready());

    // Duplicate wakes before scan collapse to one entry.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::AlreadyQueued);
    assert_eq!(p.rq.ready_len(), 1);

    // Reported once, then dropped from ready list.
    let out = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h, events: EPOLLIN, data: 77 }]);
    assert!(!p.rq.has_ready());

    // Next scan reports nothing even though the underlying resource is still ready.
    assert!(p.scan(4, |_, _| EPOLLIN).is_empty());

    // Another matching wake triggers exactly one more report.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    let out = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h, events: EPOLLIN, data: 77 }]);
    assert!(!p.rq.has_ready());

    // Spurious wake where poll returns 0 reports nothing and drops from ready list.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    assert!(p.scan(4, |_, _| 0).is_empty());
    assert!(!p.rq.has_ready());
    p.assert_invariants();
}

#[test]
fn oneshot_disarm_and_rearm_by_modify() {
    // Linux `fs/eventpoll.c` `ep_send_events` and `ep_poll_callback`:
    // reporting an EPOLLONESHOT item disarms it (suppressing even EPOLLERR and
    // EPOLLHUP) until `EPOLL_CTL_MOD` re-arms it, and B5 must poll + `wake`
    // after `modify` for an already-ready descriptor to be queued.
    let mut p = Pair::<u32>::new();
    let k = key(20, 5);
    let k_other = key(21, 6);
    let h = p.add(k, EPOLLIN | EPOLLONESHOT, 111, 1).unwrap();
    let h_other = p.add(k_other, EPOLLIN, 222, 2).unwrap();

    // Spurious wake (poll == 0) must not disarm the one-shot.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    assert!(p.scan(4, |_, _| 0).is_empty());

    // Queue `k_other` ahead of `k` (`ready` = [k_other, k]). Stopping at max = 1
    // reports only `k_other` and leaves `k` armed and queued at the front.
    assert_eq!(p.rq.wake(h_other, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    let out = p.scan(1, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h_other, events: EPOLLIN, data: 222 }]);
    assert_eq!(p.rq.ready_len(), 2); // [k, k_other]

    // Next scan visits `k`, reports it, and disarms it.
    let out = p.scan(1, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h, events: EPOLLIN, data: 111 }]);

    // While disarmed, wakes for EPOLLIN, EPOLLERR, and EPOLLHUP are all filtered.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Filtered);
    assert_eq!(p.rq.wake(h, EPOLLERR), Wake::Filtered);
    assert_eq!(p.rq.wake(h, EPOLLHUP), Wake::Filtered);
    assert!(p.scan(4, |key, _| if key == k { EPOLLIN | EPOLLERR | EPOLLHUP } else { 0 }).is_empty());

    // `modify` re-arms the one-shot, but without a subsequent `wake` (B5's
    // post-modify poll+wake), scanning reports nothing.
    let h_mod = p.modify(k, EPOLLOUT | EPOLLONESHOT, 333).unwrap();
    assert_eq!(h_mod, h);
    assert!(p.scan(4, |_, _| EPOLLOUT).is_empty());

    // Once `wake` fires (e.g. from B5's post-modify poll or a later edge), it reports and disarms.
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Filtered);
    assert_eq!(p.rq.wake(h, EPOLLOUT), Wake::Queued);
    let out = p.scan(4, |_, _| EPOLLOUT);
    assert_eq!(out, vec![Event { handle: h, events: EPOLLOUT, data: 333 }]);
    assert_eq!(p.rq.wake(h, EPOLLOUT), Wake::Filtered);

    // `modify` without EPOLLONESHOT converts it back to level-triggered.
    p.modify(k, EPOLLIN, 444).unwrap();
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    assert_eq!(p.scan(4, |_, _| EPOLLIN), vec![Event { handle: h, events: EPOLLIN, data: 444 }]);
    assert_eq!(p.scan(4, |_, _| EPOLLIN), vec![Event { handle: h, events: EPOLLIN, data: 444 }]);
    p.assert_invariants();
}

#[test]
fn wakes_between_begin_scan_and_finish_scan() {
    let mut p = Pair::<u32>::new();
    let h_et = p.add(key(1, 1), EPOLLIN | EPOLLET, 1, 1).unwrap();
    let h_zero = p.add(key(2, 2), EPOLLIN | EPOLLET, 2, 2).unwrap();
    let h_os = p.add(key(3, 3), EPOLLIN | EPOLLONESHOT, 3, 3).unwrap();
    let h_unvisited = p.add(key(4, 4), EPOLLIN, 4, 4).unwrap();
    let h_outside = p.add(key(5, 5), EPOLLIN | EPOLLET, 5, 5).unwrap();

    // Queue [h_et, h_zero, h_os, h_unvisited].
    for h in [h_et, h_zero, h_os, h_unvisited] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    // Run a scan with max = 2:
    // - h_et reports EPOLLIN, AND gets woken mid-scan -> must be requeued!
    // - h_zero polls as 0, AND gets woken mid-scan -> must be requeued!
    // - h_os reports EPOLLIN (hitting max = 2), AND gets woken mid-scan -> STILL disarmed!
    // - h_unvisited is not visited (max = 2 reached), AND gets woken mid-scan -> spliced once to front!
    // - h_outside was not in the scan, gets woken mid-scan -> Wake::Queued onto ready list!
    let out = p.scan_with(
        2,
        |k, _| if k.1 == 2 { 0 } else { EPOLLIN },
        |rq| {
            // Entries inside the scan stay logically queued -> Wake::AlreadyQueued.
            assert_eq!(rq.wake(h_et, EPOLLIN), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h_zero, EPOLLIN), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h_os, EPOLLIN), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h_unvisited, EPOLLIN), Wake::AlreadyQueued);
            // Non-matching mask mid-scan is still filtered.
            assert_eq!(rq.wake(h_et, EPOLLOUT), Wake::Filtered);
            // Entry outside the scan enters `ready` immediately -> Wake::Queued.
            assert_eq!(rq.wake(h_outside, EPOLLIN), Wake::Queued);
            assert_eq!(rq.wake(h_outside, EPOLLIN), Wake::AlreadyQueued);
        },
    );
    assert_eq!(
        out,
        vec![Event { handle: h_et, events: EPOLLIN, data: 1 }, Event { handle: h_os, events: EPOLLIN, data: 3 }]
    );

    // Expected ready queue order after finish_scan:
    // - unvisited remainder [h_unvisited] spliced to FRONT,
    // - then [h_outside] (woken into ready during scan),
    // - then [h_et, h_zero] (requeued at tail due to woke_during_scan),
    // - h_os is DISARMED and NOT queued.
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);
    let next = p.scan(10, |_, _| EPOLLIN);
    let ids: Vec<u64> = next.iter().map(|e| e.data).collect();
    assert_eq!(ids, vec![4, 5, 1, 2]);
    // After that scan: h_unvisited (LT) stays queued; ET entries [5, 1, 2] were dropped.
    assert_eq!(p.rq.ready_len(), 1);
    p.assert_invariants();
}

#[test]
fn stale_handles_rejected_after_remove_and_slot_reuse() {
    let mut p = Pair::<u32>::new();
    let h_old = p.add(key(1, 1), EPOLLIN, 10, 100).unwrap();
    assert_eq!(p.set.get(h_old).unwrap().payload, &100);
    *p.set.payload_mut(h_old).unwrap() = 101;
    assert_eq!(p.set.get(h_old).unwrap().payload, &101);

    let (removed_h, payload) = p.remove(key(1, 1)).unwrap();
    assert_eq!(removed_h, h_old);
    assert_eq!(payload, 101);

    // After remove, old handle is stale everywhere.
    assert_eq!(p.rq.wake(h_old, EPOLLIN | EPOLLERR), Wake::Stale);
    assert!(p.set.get(h_old).is_none());
    assert!(p.set.payload_mut(h_old).is_none());
    p.rq.unlink(h_old); // no-op
    p.rq.unreport([h_old]); // no-op
    assert!(!p.rq.has_ready());

    // Adding a new key reuses the same slot index with a bumped generation.
    let h_new = p.add(key(2, 2), EPOLLIN, 20, 200).unwrap();
    assert_eq!(h_new.index(), h_old.index());
    assert_ne!(h_new.generation(), h_old.generation());

    // Old handle is STILL rejected and cannot perturb the new occupant!
    assert_eq!(p.rq.wake(h_old, EPOLLIN), Wake::Stale);
    assert!(p.set.get(h_old).is_none());
    assert!(p.set.payload_mut(h_old).is_none());
    p.rq.unlink(h_old);
    p.rq.unreport([h_old]);
    assert!(!p.rq.has_ready());

    // New handle works normally.
    assert_eq!(p.rq.wake(h_new, EPOLLIN), Wake::Queued);
    let out = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h_new, events: EPOLLIN, data: 20 }]);

    // Out-of-bounds handle is also rejected.
    let oob = Handle::new(9999, 1);
    assert_eq!(p.rq.wake(oob, EPOLLIN), Wake::Stale);
    assert!(p.set.get(oob).is_none());
    p.rq.unlink(oob);
    p.rq.unreport([oob]);
    p.assert_invariants();
}

#[test]
fn growth_via_prepare_grow_and_install_preserves_state_and_handles_nomem() {
    let mut set = EpollSet::<Key, u32>::new();
    let mut rq = ReadyQueue::new();
    assert_eq!(rq.capacity(), 0);

    // prepare_grow rejects impossible sizes cleanly with Errno::NoMem.
    assert_eq!(ReadyQueue::prepare_grow(usize::MAX).unwrap_err(), Errno::NoMem);
    assert_eq!(ReadyQueue::prepare_grow((u32::MAX as usize) + 1).unwrap_err(), Errno::NoMem);

    // Add 40 interests (crossing initial 16 -> 32 -> 64 capacity boundaries),
    // queueing half of them BEFORE each growth to verify links and flags survive.
    let mut handles = Vec::new();
    let mut growth_count = 0usize;
    for i in 0..40u32 {
        let added = set.add(key(u64::from(i), i), EPOLLIN, u64::from(i), i).unwrap();
        if let Some(storage) = added.growth {
            growth_count += 1;
            let old_cap = rq.capacity();
            let old = rq.install(storage);
            assert_eq!(old.capacity(), old_cap);
            assert!(rq.capacity() > old_cap);
            drop(old);
        }
        rq.set_mask(added.handle, added.watch_mask, true);
        if i % 2 == 0 {
            assert_eq!(rq.wake(added.handle, EPOLLIN), Wake::Queued);
        }
        handles.push(added.handle);
    }
    assert!(growth_count >= 3, "expected multiple ReadyQueue growths, got {growth_count}");
    assert_eq!(rq.ready_len(), 20);

    // Installing a smaller-or-equal storage is a safe no-op returning it back.
    let small = ReadyQueue::prepare_grow(4).unwrap();
    let cap_before = rq.capacity();
    let returned = rq.install(small);
    assert_eq!(returned.capacity(), 4);
    assert_eq!(rq.capacity(), cap_before);

    // Every queued entry survived all growths in exact FIFO order.
    let mut hbuf = vec![Handle::new(0, 0); 40];
    let scan = rq.begin_scan(&mut hbuf);
    let polled = vec![EPOLLIN; scan.len()];
    let mut out = Vec::new();
    let n = rq.finish_scan(&set, scan, &polled, 40, &mut out);
    assert_eq!(n, 20);
    let expected: Vec<u64> = (0..40u64).step_by(2).collect();
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), expected);
    set.assert_invariants();
    rq.assert_invariants();
}

#[test]
fn unreport_on_copy_out_failure_loses_no_oneshot_or_et() {
    // Linux `fs/eventpoll.c` `ep_send_events` `-EFAULT` path: rolling back the
    // uncopied tail re-arms EPOLLONESHOT and requeues ET/ONESHOT/LT at the front
    // in their original order.
    let mut p = Pair::<u32>::new();
    let h_lt0 = p.add(key(1, 1), EPOLLIN, 10, 1).unwrap();
    let h_os = p.add(key(2, 2), EPOLLIN | EPOLLONESHOT, 20, 2).unwrap();
    let h_et = p.add(key(3, 3), EPOLLIN | EPOLLET, 30, 3).unwrap();
    let h_lt1 = p.add(key(4, 4), EPOLLIN, 40, 4).unwrap();
    let h_unvisited = p.add(key(5, 5), EPOLLIN, 50, 5).unwrap();

    for h in [h_lt0, h_os, h_et, h_lt1, h_unvisited] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    // Collect 4 events: [h_lt0, h_os, h_et, h_lt1], leaving h_unvisited at front.
    let events = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(events.iter().map(|e| e.data).collect::<Vec<_>>(), vec![10, 20, 30, 40]);
    // At this point: h_os is disarmed, h_et is unqueued, ready = [h_unvisited, h_lt0, h_lt1].
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);

    // Simulate copy_to_user succeeding on events[0] (h_lt0) and faulting on events[1] (h_os).
    // B5 rolls back the uncopied tail events[1..] = [h_os, h_et, h_lt1].
    p.rq.unreport(events[1..].iter().map(|e| e.handle));

    // Next scan must deliver [h_os, h_et, h_lt1, h_unvisited, h_lt0] in that exact order!
    let retry = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(retry.iter().map(|e| e.data).collect::<Vec<_>>(), vec![20, 30, 40, 50, 10]);

    // And now h_os is disarmed again after actually being delivered.
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);

    // Full rollback at index 0 (EFAULT on very first event) also works for ONESHOT + ET.
    p.modify(key(2, 2), EPOLLIN | EPOLLONESHOT, 21).unwrap();
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h_et, EPOLLIN), Wake::Queued);
    let batch = p.scan(10, |k, _| if k.1 == 2 || k.1 == 3 { EPOLLIN } else { 0 });
    assert_eq!(batch.iter().map(|e| e.data).collect::<Vec<_>>(), vec![21, 30]);
    p.rq.unreport(batch.iter().map(|e| e.handle));
    let retry2 = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(retry2.iter().map(|e| e.data).collect::<Vec<_>>(), vec![21, 30]);
    p.assert_invariants();
}

#[test]
fn nested_epoll_peek_drained_and_ready_pipes() {
    // Models Linux `fs/eventpoll.c` `__ep_eventpoll_poll` / `ep_read_events_proc`:
    // `outer` watches `inner`; `inner` watches pipes.
    let mut inner = Pair::<u32>::new();
    let mut outer = Pair::<u32>::new();

    let h_pipe_et = inner.add(key(1, 10), EPOLLIN | EPOLLET, 100, 10).unwrap();
    let h_pipe_os = inner.add(key(2, 11), EPOLLIN | EPOLLONESHOT, 101, 11).unwrap();
    let h_inner_in_outer = outer.add(key(99, 50), EPOLLIN, 999, 50).unwrap();

    // Helper implementing inner epoll's `Pollable::poll` via `begin_scan` + `finish_peek`.
    let poll_inner = |inner: &mut Pair<u32>, pipe_state: &[u32; 2], mid_peek: &mut dyn FnMut(&mut ReadyQueue)| -> u32 {
        let mut hbuf = [Handle::new(0, 0); 8];
        let scan = inner.rq.begin_scan(&mut hbuf);
        let mut polled = Vec::with_capacity(scan.len());
        for &h in scan.handles() {
            let e = inner.set.get(h).unwrap();
            let idx = (e.key.1 - 10) as usize;
            let mask = pipe_state[idx];
            polled.push(mask);
            if (mask & e.watch_mask()) != 0 {
                break;
            }
        }
        mid_peek(&mut inner.rq);
        if inner.rq.finish_peek(scan, &polled) { EPOLLIN | EPOLLRDNORM } else { 0 }
    };

    // Scenario 1 (drained pipe): pipe_os was woken -> inner notified outer -> pipe_os drained before outer polls!
    assert_eq!(inner.rq.wake(h_pipe_os, EPOLLIN), Wake::Queued);
    assert_eq!(outer.rq.wake(h_inner_in_outer, EPOLLIN), Wake::Queued);
    let drained = [0, 0];
    let outer_events = outer.scan(4, |_, _| poll_inner(&mut inner, &drained, &mut |_| {}));
    assert!(outer_events.is_empty(), "outer must not spuriously report drained inner pipe");
    assert!(!inner.rq.has_ready(), "drained entry pruned from inner ready list");
    // Pruning on revents == 0 must NOT disarm EPOLLONESHOT!
    assert_eq!(inner.rq.wake(h_pipe_os, EPOLLIN), Wake::Queued);

    // Scenario 2 (ready pipes: ET + ONESHOT): outer polls inner -> sees EPOLLIN;
    // inner must NOT drop ET or disarm ONESHOT!
    assert_eq!(inner.rq.wake(h_pipe_et, EPOLLIN), Wake::Queued);
    // inner ready list is [h_pipe_os, h_pipe_et].
    assert_eq!(outer.rq.wake(h_inner_in_outer, EPOLLIN), Wake::Queued);
    let both_ready = [EPOLLIN, EPOLLIN];
    let outer_events = outer.scan(4, |_, _| poll_inner(&mut inner, &both_ready, &mut |_| {}));
    assert_eq!(outer_events, vec![Event { handle: h_inner_in_outer, events: EPOLLIN, data: 999 }]);
    assert_eq!(inner.rq.ready_len(), 2);

    // Actual `epoll_wait` on inner still delivers BOTH ONESHOT and ET in exact order!
    let inner_events = inner.scan(4, |_, _| EPOLLIN);
    assert_eq!(
        inner_events,
        vec![
            Event { handle: h_pipe_os, events: EPOLLIN, data: 101 },
            Event { handle: h_pipe_et, events: EPOLLIN, data: 100 },
        ]
    );

    // Scenario 3 (stale-then-ready): first entry is drained, second is ready -> prunes first, keeps second, returns ready.
    inner.modify(key(2, 11), EPOLLIN | EPOLLONESHOT, 102).unwrap();
    assert_eq!(inner.rq.wake(h_pipe_et, EPOLLIN), Wake::Queued); // idx 0 (drained)
    assert_eq!(inner.rq.wake(h_pipe_os, EPOLLIN), Wake::Queued); // idx 1 (ready)
    let only_os_ready = [0, EPOLLIN];
    assert_eq!(poll_inner(&mut inner, &only_os_ready, &mut |_| {}), EPOLLIN | EPOLLRDNORM);
    assert_eq!(inner.rq.ready_len(), 1);
    let inner_events = inner.scan(4, |_, _| EPOLLIN);
    assert_eq!(inner_events, vec![Event { handle: h_pipe_os, events: EPOLLIN, data: 102 }]);

    // Scenario 4 (mid-peek wake on drained entry): pipe was 0 when polled, then woken before finish_peek -> requeued!
    assert_eq!(inner.rq.wake(h_pipe_et, EPOLLIN), Wake::Queued);
    let mut mid_woke = false;
    let res = poll_inner(&mut inner, &drained, &mut |rq| {
        assert_eq!(rq.wake(h_pipe_et, EPOLLIN), Wake::AlreadyQueued);
        mid_woke = true;
    });
    assert!(mid_woke);
    assert_eq!(res, 0);
    assert!(inner.rq.has_ready(), "mid-peek wake must prevent losing wakeup");
    inner.assert_invariants();
    outer.assert_invariants();
}

#[test]
fn remove_range_and_drain_return_payloads_and_bump_generations() {
    let mut p = Pair::<u32>::new();
    // Description 7 registered under fds 3, 5, 9; description 8 under fd 4.
    let h7_3 = p.add(key(7, 3), EPOLLIN, 73, 703).unwrap();
    let h7_5 = p.add(key(7, 5), EPOLLIN, 75, 705).unwrap();
    let h7_9 = p.add(key(7, 9), EPOLLIN, 79, 709).unwrap();
    let h8_4 = p.add(key(8, 4), EPOLLIN, 84, 804).unwrap();
    for h in [h7_3, h7_5, h7_9, h8_4] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    // Remove all keys belonging to description 7.
    let removed: Vec<_> = p.set.remove_range((7, 0)..=(7, u32::MAX)).collect();
    assert_eq!(removed.len(), 3);
    assert_eq!(
        removed.iter().map(|&(k, _, pl)| (k, pl)).collect::<Vec<_>>(),
        vec![(key(7, 3), 703), (key(7, 5), 705), (key(7, 9), 709),]
    );
    for &(_, h, _) in &removed {
        p.rq.unlink(h);
    }
    assert_eq!(p.set.len(), 1);
    assert!(p.set.contains(key(8, 4)));
    assert_eq!(p.rq.ready_len(), 1);
    for &(_, h, _) in &removed {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Stale);
    }

    // Dropping a partial RemoveRange still removes the rest of the range.
    let _ = p.add(key(9, 1), EPOLLIN, 91, 901).unwrap();
    let _ = p.add(key(9, 2), EPOLLIN, 92, 902).unwrap();
    {
        let mut it = p.set.remove_range((9, 0)..=(9, u32::MAX));
        let first = it.next().unwrap();
        assert_eq!(first.0, key(9, 1));
        p.rq.unlink(first.1);
        // drop `it` without consuming (9, 2)
    }
    assert!(!p.set.contains(key(9, 2)));
    assert_eq!(p.set.len(), 1);

    // `drain()` yields remaining entries, empties set, and bumps generations.
    let drained: Vec<_> = p.set.drain().collect();
    assert_eq!(drained, vec![(key(8, 4), h8_4, 804)]);
    p.rq.unlink(h8_4);
    assert!(p.set.is_empty());
    assert_eq!(p.rq.wake(h8_4, EPOLLIN), Wake::Stale);

    // Re-adding reuses a slot with a bumped generation.
    let h_fresh = p.add(key(8, 4), EPOLLIN, 85, 805).unwrap();
    assert_ne!(h_fresh, h8_4);
    p.assert_invariants();
}

// =============================================================================
// Naive reference model for randomized differential testing of EpollSet + ReadyQueue
//
// NOTE: This reference model uses its own literal bit constants and checks the
// data-structure implementation (slab reuse, generational handles, intrusive
// ready list, two-phase scan with interleaved wakes, unreport, peek, growth,
// remove_range, and drain). The hand-written unit tests above pin the Linux
// UAPI constants and `fs/eventpoll.c` semantics directly.
// =============================================================================

const M_IN: u32 = 0x0000_0001;
const M_PRI: u32 = 0x0000_0002;
const M_OUT: u32 = 0x0000_0004;
const M_ERR: u32 = 0x0000_0008;
const M_HUP: u32 = 0x0000_0010;
const M_RDHUP: u32 = 0x0000_2000;
const M_EXCLUSIVE: u32 = 0x1000_0000;
const M_WAKEUP: u32 = 0x2000_0000;
const M_ONESHOT: u32 = 0x4000_0000;
const M_ET: u32 = 0x8000_0000;
const M_PRIVATE: u32 = 0xF000_0000;
const M_ALWAYS: u32 = 0x0000_0018;
const M_EXCLUSIVE_OK: u32 = 0xB000_001D;

struct ModelEntry {
    handle: Handle,
    events: u32,
    data: u64,
    armed: bool,
}

struct NaiveEpoll {
    by_key: BTreeMap<Key, ModelEntry>,
    by_handle: BTreeMap<Handle, Key>,
    ready: VecDeque<Handle>,
    in_scan: BTreeSet<Handle>,
    woke_during_scan: BTreeSet<Handle>,
}

impl NaiveEpoll {
    fn new() -> Self {
        Self {
            by_key: BTreeMap::new(),
            by_handle: BTreeMap::new(),
            ready: VecDeque::new(),
            in_scan: BTreeSet::new(),
            woke_during_scan: BTreeSet::new(),
        }
    }

    fn add(&mut self, key: Key, events: u32, data: u64, handle: Option<Handle>) -> Result<(), Errno> {
        if (events & M_EXCLUSIVE) != 0 && (events & !M_EXCLUSIVE_OK) != 0 {
            return Err(Errno::Inval);
        }
        if self.by_key.contains_key(&key) {
            return Err(Errno::Exist);
        }
        let h = handle.expect("expected handle when model add succeeds");
        self.by_key.insert(key, ModelEntry { handle: h, events: (events & !M_WAKEUP) | M_ALWAYS, data, armed: true });
        self.by_handle.insert(h, key);
        Ok(())
    }

    fn modify(&mut self, key: Key, events: u32, data: u64) -> Result<Handle, Errno> {
        if (events & M_EXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        let Some(e) = self.by_key.get_mut(&key) else {
            return Err(Errno::NoEnt);
        };
        if (e.events & M_EXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        e.events = (events & !M_WAKEUP) | M_ALWAYS;
        e.data = data;
        e.armed = true;
        Ok(e.handle)
    }

    fn remove(&mut self, key: Key) -> Result<Handle, Errno> {
        let Some(e) = self.by_key.remove(&key) else {
            return Err(Errno::NoEnt);
        };
        self.by_handle.remove(&e.handle);
        self.ready.retain(|&h| h != e.handle);
        self.in_scan.remove(&e.handle);
        self.woke_during_scan.remove(&e.handle);
        Ok(e.handle)
    }

    fn wake(&mut self, h: Handle, ready: u32) -> Wake {
        let Some(&key) = self.by_handle.get(&h) else {
            return Wake::Stale;
        };
        let e = &self.by_key[&key];
        let watch = e.events & !M_PRIVATE;
        if !e.armed || (watch & ready) == 0 {
            return Wake::Filtered;
        }
        if self.in_scan.contains(&h) {
            self.woke_during_scan.insert(h);
            return Wake::AlreadyQueued;
        }
        if self.ready.contains(&h) {
            return Wake::AlreadyQueued;
        }
        self.ready.push_back(h);
        Wake::Queued
    }

    fn begin_scan(&mut self, cap: usize) -> Vec<Handle> {
        let mut batch = Vec::new();
        while batch.len() < cap {
            let Some(h) = self.ready.pop_front() else {
                break;
            };
            self.in_scan.insert(h);
            batch.push(h);
        }
        batch
    }

    fn finish_scan(&mut self, batch: &[Handle], polled: &[u32], max: usize) -> Vec<Event> {
        let visit_limit = batch.len().min(polled.len());
        let mut visited = 0usize;
        let mut out = Vec::new();
        while visited < visit_limit && out.len() < max {
            let h = batch[visited];
            let ready_bits = polled[visited];
            visited += 1;
            self.in_scan.remove(&h);
            let woke = self.woke_during_scan.remove(&h);
            let Some(&key) = self.by_handle.get(&h) else {
                continue;
            };
            let e = self.by_key.get_mut(&key).unwrap();
            let revents = ready_bits & (e.events & !M_PRIVATE);
            if revents == 0 {
                if woke {
                    self.ready.push_back(h);
                }
                continue;
            }
            out.push(Event { handle: h, events: revents, data: e.data });
            if (e.events & M_ONESHOT) != 0 {
                e.armed = false;
            } else if (e.events & M_ET) == 0 || woke {
                self.ready.push_back(h);
            }
        }
        for &h in batch[visited..].iter().rev() {
            self.in_scan.remove(&h);
            self.woke_during_scan.remove(&h);
            if self.by_handle.contains_key(&h) && !self.ready.contains(&h) {
                self.ready.push_front(h);
            }
        }
        out
    }

    fn finish_peek(&mut self, batch: &[Handle], polled: &[u32]) -> bool {
        let visit_limit = batch.len().min(polled.len());
        let mut visited = 0usize;
        let mut found = false;
        while visited < visit_limit {
            let h = batch[visited];
            let ready_bits = polled[visited];
            let Some(&key) = self.by_handle.get(&h) else {
                visited += 1;
                continue;
            };
            let e = &self.by_key[&key];
            if (ready_bits & (e.events & !M_PRIVATE)) != 0 {
                found = true;
                break;
            }
            visited += 1;
            self.in_scan.remove(&h);
            let woke = self.woke_during_scan.remove(&h);
            if woke && !self.ready.contains(&h) {
                self.ready.push_back(h);
            }
        }
        for &h in batch[visited..].iter().rev() {
            self.in_scan.remove(&h);
            self.woke_during_scan.remove(&h);
            if self.by_handle.contains_key(&h) && !self.ready.contains(&h) {
                self.ready.push_front(h);
            }
        }
        found
    }

    fn unreport(&mut self, handles: &[Handle]) {
        for &h in handles.iter().rev() {
            let Some(&key) = self.by_handle.get(&h) else {
                continue;
            };
            let e = self.by_key.get_mut(&key).unwrap();
            e.armed = true;
            if self.in_scan.contains(&h) {
                self.woke_during_scan.insert(h);
                continue;
            }
            self.ready.retain(|&x| x != h);
            self.ready.push_front(h);
        }
    }
}

fn random_events(rng: &mut Rng) -> u32 {
    const BASE_BITS: &[u32] = &[M_IN, M_OUT, M_PRI, M_RDHUP, M_ERR, M_HUP];
    const MOD_BITS: &[u32] = &[M_ET, M_ONESHOT, M_WAKEUP, M_EXCLUSIVE];
    let mut ev = 0u32;
    for &b in BASE_BITS {
        if rng.below(3) == 0 {
            ev |= b;
        }
    }
    for &m in MOD_BITS {
        if rng.below(5) == 0 {
            ev |= m;
        }
    }
    ev
}

fn random_ready(rng: &mut Rng) -> u32 {
    const BITS: &[u32] = &[M_IN, M_OUT, M_PRI, M_RDHUP, M_ERR, M_HUP, M_ET, M_WAKEUP];
    let mut r = 0u32;
    for &b in BITS {
        if rng.below(4) == 0 {
            r |= b;
        }
    }
    r
}

#[test]
fn randomized_model_matches_naive_epoll() {
    for seed in 1..=8u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x517c_c1b7_2722_0a95));
        let mut p = Pair::<u32>::new();
        let mut model = NaiveEpoll::new();
        let mut live_state = [0u32; 24];
        let mut known_handles: Vec<Handle> = Vec::new();

        for step in 0..1200usize {
            let idx = rng.below(live_state.len());
            let k = key((idx / 3) as u64, idx as u32);
            match rng.below(14) {
                0..=2 => {
                    let ev = random_events(&mut rng);
                    let data = rng.next_u64();
                    let res = p.add(k, ev, data, idx as u32);
                    match res {
                        Ok(h) => {
                            assert_eq!(model.add(k, ev, data, Some(h)), Ok(()), "seed={seed} step={step} add");
                            known_handles.push(h);
                        }
                        Err(e) => {
                            assert_eq!(model.add(k, ev, data, None), Err(e), "seed={seed} step={step} add err");
                        }
                    }
                }
                3 | 4 => {
                    let ev = random_events(&mut rng);
                    let data = rng.next_u64();
                    assert_eq!(p.modify(k, ev, data), model.modify(k, ev, data), "seed={seed} step={step} modify");
                }
                5 => {
                    let res_p = p.remove(k).map(|(h, _)| h);
                    let res_m = model.remove(k);
                    assert_eq!(res_p, res_m, "seed={seed} step={step} remove");
                }
                6..=8 => {
                    // Wake either a live key's handle or a possibly-stale historical handle.
                    let ready = random_ready(&mut rng);
                    live_state[idx] = ready;
                    let h = if rng.below(4) == 0 && !known_handles.is_empty() {
                        known_handles[rng.below(known_handles.len())]
                    } else if let Some(h) = p.set.lookup(k) {
                        h
                    } else {
                        Handle::new(idx as u32, 999)
                    };
                    assert_eq!(p.rq.wake(h, ready), model.wake(h, ready), "seed={seed} step={step} wake");
                }
                9..=11 => {
                    // Two-phase scan with interleaved mid-scan wakes and optional copy-out unreport!
                    if rng.below(3) == 0 {
                        let flip = rng.below(live_state.len());
                        live_state[flip] = random_ready(&mut rng);
                    }
                    let buf_cap = [0, 1, 2, 4, 8, 32][rng.below(6)];
                    let mut hbuf = vec![Handle::new(0, 0); buf_cap];
                    let scan = p.rq.begin_scan(&mut hbuf);
                    let mbatch = model.begin_scan(buf_cap);
                    assert_eq!(scan.handles(), mbatch.as_slice(), "seed={seed} step={step} begin_scan");

                    let polled_len = if scan.is_empty() { 0 } else { rng.below(scan.len()) + 1 };
                    let mut polled = Vec::with_capacity(polled_len);
                    for &h in &scan.handles()[..polled_len] {
                        let e = p.set.get(h).unwrap();
                        polled.push(live_state[e.key.1 as usize]);
                    }

                    // Interleave 0..3 wakes BETWEEN begin_scan and finish_scan!
                    for _ in 0..rng.below(4) {
                        let widx = rng.below(live_state.len());
                        let wk = key((widx / 3) as u64, widx as u32);
                        let wready = random_ready(&mut rng);
                        live_state[widx] = wready;
                        if let Some(wh) = p.set.lookup(wk) {
                            assert_eq!(
                                p.rq.wake(wh, wready),
                                model.wake(wh, wready),
                                "seed={seed} step={step} mid-scan wake"
                            );
                        }
                    }

                    let max = [0, 1, 2, 3, 5, 32][rng.below(6)];
                    let mut out_p = Vec::new();
                    let n_p = p.rq.finish_scan(&p.set, scan, &polled, max, &mut out_p);
                    let out_m = model.finish_scan(&mbatch, &polled, max);
                    assert_eq!(n_p, out_p.len());
                    assert_eq!(out_p, out_m, "seed={seed} step={step} finish_scan");

                    // Occasionally simulate a copy-out fault on a suffix of reported events.
                    if !out_p.is_empty() && rng.below(4) == 0 {
                        let fail_at = rng.below(out_p.len());
                        let tail: Vec<Handle> = out_p[fail_at..].iter().map(|e| e.handle).collect();
                        p.rq.unreport(tail.iter().copied());
                        model.unreport(&tail);
                    }
                }
                12 => {
                    // Two-phase nested-epoll peek with interleaved mid-peek wakes!
                    let buf_cap = [0, 1, 3, 8, 32][rng.below(5)];
                    let mut hbuf = vec![Handle::new(0, 0); buf_cap];
                    let scan = p.rq.begin_scan(&mut hbuf);
                    let mbatch = model.begin_scan(buf_cap);
                    assert_eq!(scan.handles(), mbatch.as_slice());

                    let polled_len = if scan.is_empty() { 0 } else { rng.below(scan.len()) + 1 };
                    let mut polled = Vec::with_capacity(polled_len);
                    for &h in &scan.handles()[..polled_len] {
                        let e = p.set.get(h).unwrap();
                        polled.push(live_state[e.key.1 as usize]);
                    }
                    for _ in 0..rng.below(3) {
                        let widx = rng.below(live_state.len());
                        let wk = key((widx / 3) as u64, widx as u32);
                        let wready = random_ready(&mut rng);
                        if let Some(wh) = p.set.lookup(wk) {
                            assert_eq!(p.rq.wake(wh, wready), model.wake(wh, wready));
                        }
                    }
                    assert_eq!(
                        p.rq.finish_peek(scan, &polled),
                        model.finish_peek(&mbatch, &polled),
                        "seed={seed} step={step} finish_peek"
                    );
                }
                _ => {
                    // remove_range on one description id.
                    let desc = (idx / 3) as u64;
                    let removed: Vec<_> = p.set.remove_range((desc, 0)..=(desc, u32::MAX)).collect();
                    for &(rk, rh, _) in &removed {
                        p.rq.unlink(rh);
                        assert_eq!(model.remove(rk), Ok(rh));
                    }
                }
            }
            assert_eq!(p.set.len(), model.by_key.len());
            assert_eq!(p.rq.ready_len(), model.ready.len());
            assert_eq!(p.rq.has_ready(), !model.ready.is_empty());
            p.assert_invariants();
        }
    }
}

// =============================================================================
// LoopCheck tests (cycles, EP_MAX_NESTS = 4, 5-deep chains, DAGs, refcounts)
// Matches Linux v6.17+ `fs/eventpoll.c` `ep_loop_check` (`up + 1 + down > 4`).
// =============================================================================

#[test]
fn loop_check_refuses_cycles_and_five_deep_chains() {
    // Self-loop: E0 -> E0.
    let mut lc = LoopCheck::<u64>::new();
    assert_eq!(lc.add(0, 0), Err(Errno::Loop));
    assert!(lc.is_empty());

    // 2-cycle: 0 -> 1 -> 0.
    assert_eq!(lc.add(0, 1), Ok(()));
    assert_eq!(lc.add(1, 0), Err(Errno::Loop));
    assert!(lc.contains(0, 1));
    assert!(!lc.contains(1, 0));

    // 3-cycle: 0 -> 1 -> 2 -> 0.
    assert_eq!(lc.add(1, 2), Ok(()));
    assert_eq!(lc.add(2, 0), Err(Errno::Loop));

    // 4-deep chain (4 edges = EP_MAX_NESTS, 5 sets): 0 -> 1 -> 2 -> 3 -> 4 succeeds.
    assert_eq!(lc.add(2, 3), Ok(()));
    assert_eq!(lc.add(3, 4), Ok(()));
    // Closing the 5-node cycle 4 -> 0 fails with ELOOP.
    assert_eq!(lc.add(4, 0), Err(Errno::Loop));
    // Extending at the leaf (top-down construction: 0->1->2->3->4, then 4->5) -> ELOOP.
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));
    // Prepending at the root (bottom-up construction: 99->0 onto 0->1->2->3->4) -> ELOOP.
    assert_eq!(lc.add(99, 0), Err(Errno::Loop));
    lc.assert_invariants();

    // Removing an edge drops depth to 3 so extending succeeds.
    assert!(lc.remove(0, 1));
    assert!(!lc.remove(0, 1)); // already gone
    assert_eq!(lc.add(4, 5), Ok(())); // 1 -> 2 -> 3 -> 4 -> 5 (4 edges)
    assert_eq!(lc.add(0, 1), Err(Errno::Loop)); // prepending 0->1 would make 5 edges again
    lc.assert_invariants();

    // Middle-join of two 2-edge chains (0->1->2 and 3->4->5): joining 2->3 makes 5 edges -> ELOOP.
    let mut mid = LoopCheck::<u64>::new();
    mid.add(0, 1).unwrap();
    mid.add(1, 2).unwrap();
    mid.add(3, 4).unwrap();
    mid.add(4, 5).unwrap();
    assert_eq!(mid.add(2, 3), Err(Errno::Loop));
    // Dropping 4->5 makes 0->1->2->3->4 (4 edges) legal.
    assert!(mid.remove(4, 5));
    assert_eq!(mid.add(2, 3), Ok(()));
    mid.assert_invariants();
}

#[test]
fn loop_check_diamonds_refcounts_and_remove_set() {
    let mut lc = LoopCheck::<u64>::new();

    // Diamond DAG with asymmetric branch lengths:
    //   0 -> 1 -> 2 -> 3 (length 3)
    //   0 -> 3           (length 1)
    // Must allow 3 -> 4 (longest path 0->1->2->3->4 has length 4),
    // and refuse 4 -> 5 (longest path has length 5, even though 0->3->4->5 is only 3).
    lc.add(0, 3).unwrap();
    lc.add(0, 1).unwrap();
    lc.add(1, 2).unwrap();
    lc.add(2, 3).unwrap();
    assert_eq!(lc.add(3, 4), Ok(()));
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));

    // Duplicate edges (e.g. dup'd epoll fd added twice) are reference-counted.
    assert_eq!(lc.add(1, 2), Ok(()));
    assert_eq!(lc.edge_count(1, 2), 2);
    // Removing once keeps 1 -> 2 active, so 4 -> 5 is still depth 5 -> ELOOP.
    assert!(lc.remove(1, 2));
    assert_eq!(lc.edge_count(1, 2), 1);
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));
    // Removing the second registration drops 1 -> 2; longest path through 4 is now 0->3->4 (depth 2).
    assert!(lc.remove(1, 2));
    assert_eq!(lc.edge_count(1, 2), 0);
    assert_eq!(lc.add(4, 5), Ok(())); // 0 -> 3 -> 4 -> 5 (depth 3)
    assert_eq!(lc.add(5, 6), Ok(())); // 0 -> 3 -> 4 -> 5 -> 6 (depth 4)
    assert_eq!(lc.add(6, 7), Err(Errno::Loop)); // depth 5

    // remove_set clears both incoming and outgoing edges incident to the closed set.
    lc.remove_set(4);
    assert!(!lc.contains(3, 4));
    assert!(!lc.contains(4, 5));
    assert_eq!(lc.add(6, 7), Ok(())); // 5 -> 6 -> 7 (depth 2)
    lc.assert_invariants();
}

/// Brute-force reference graph for differential testing of `LoopCheck`.
struct NaiveGraph {
    edges: BTreeMap<(u64, u64), usize>,
}

impl NaiveGraph {
    fn new() -> Self {
        Self { edges: BTreeMap::new() }
    }

    fn has_cycle_or_too_deep(&self) -> bool {
        let mut nodes = BTreeSet::new();
        let mut adj: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (&(u, v), &c) in &self.edges {
            assert!(c > 0);
            nodes.insert(u);
            nodes.insert(v);
            adj.entry(u).or_default().push(v);
        }

        fn dfs(node: u64, adj: &BTreeMap<u64, Vec<u64>>, on_stack: &mut BTreeSet<u64>, depth: usize) -> bool {
            if depth > EP_MAX_NESTS {
                return true;
            }
            if !on_stack.insert(node) {
                return true;
            }
            if let Some(children) = adj.get(&node) {
                for &c in children {
                    if dfs(c, adj, on_stack, depth + 1) {
                        return true;
                    }
                }
            }
            on_stack.remove(&node);
            false
        }

        for n in nodes {
            let mut on_stack = BTreeSet::new();
            if dfs(n, &adj, &mut on_stack, 0) {
                return true;
            }
        }
        false
    }

    fn add(&mut self, parent: u64, child: u64) -> Result<(), Errno> {
        let entry = self.edges.entry((parent, child)).or_insert(0);
        *entry += 1;
        if *entry == 1 && self.has_cycle_or_too_deep() {
            self.edges.remove(&(parent, child));
            return Err(Errno::Loop);
        }
        Ok(())
    }

    fn remove(&mut self, parent: u64, child: u64) -> bool {
        let Some(c) = self.edges.get_mut(&(parent, child)) else {
            return false;
        };
        if *c > 1 {
            *c -= 1;
        } else {
            self.edges.remove(&(parent, child));
        }
        true
    }

    fn remove_set(&mut self, id: u64) {
        self.edges.retain(|&(u, v), _| u != id && v != id);
    }
}

#[test]
fn randomized_loop_check_matches_naive_graph() {
    for seed in 1..=6u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0xa076_1d64_78bd_642f));
        let mut lc = LoopCheck::<u64>::new();
        let mut model = NaiveGraph::new();

        for step in 0..800usize {
            let u = rng.below(9) as u64;
            let v = rng.below(9) as u64;
            match rng.below(10) {
                0..=5 => {
                    assert_eq!(lc.add(u, v), model.add(u, v), "seed={seed} step={step} add({u},{v})");
                }
                6..=8 => {
                    assert_eq!(lc.remove(u, v), model.remove(u, v), "seed={seed} step={step} remove({u},{v})");
                }
                _ => {
                    lc.remove_set(u);
                    model.remove_set(u);
                }
            }
            assert_eq!(lc.edge_count(u, v), model.edges.get(&(u, v)).copied().unwrap_or(0));
            assert_eq!(lc.is_empty(), model.edges.is_empty());
            lc.assert_invariants();
        }
    }
}
