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
        let h = added.handle();
        let old = self.rq.activate(&mut self.set, added);
        drop(old);
        Ok(h)
    }

    fn modify(&mut self, k: Key, events: u32, data: u64) -> Result<Handle, Errno> {
        let modified = self.set.modify(k, events, data)?;
        let h = modified.handle();
        self.rq.apply(modified);
        Ok(h)
    }

    fn remove(&mut self, k: Key) -> Result<(Handle, T), Errno> {
        let (h, payload) = self.set.remove(k)?;
        self.rq.unlink(h);
        Ok((h, payload))
    }

    /// Run a complete two-phase scan sizing the buffer from `set.len()`, with
    /// an optional hook between `begin_scan` and `finish_scan`.
    fn scan_with(
        &mut self,
        max: usize,
        mut poll: impl FnMut(Key, &T) -> u32,
        mid_scan: impl FnOnce(&mut ReadyQueue),
    ) -> (Vec<Event>, ScanDone) {
        let mut hbuf = vec![Handle::INVALID; self.set.len()];
        let scan = self.rq.begin_scan(&mut hbuf);
        let mut polled = Vec::with_capacity(scan.len());
        for &h in scan.handles() {
            let e = self.set.get(h).expect("handle in scan must be live in set");
            polled.push(poll(e.key, e.payload));
        }
        mid_scan(&mut self.rq);
        let mut out = Vec::new();
        let done = self.rq.finish_scan(&self.set, scan, &polled, max, &mut out);
        assert_eq!(done.reported, out.len());
        assert_eq!(done.wake_waiters, self.rq.has_ready());
        (out, done)
    }

    fn scan(&mut self, max: usize, poll: impl FnMut(Key, &T) -> u32) -> Vec<Event> {
        self.scan_with(max, poll, |_| {}).0
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

    // Modifying an existing EPOLLEXCLUSIVE interest returns EINVAL.
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

    // Interest with events == 0 still monitors EPOLLERR and EPOLLHUP.
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
    let (out, done) = p.scan_with(0, |_, _| EPOLLIN, |_| {});
    assert!(out.is_empty());
    assert!(done.wake_waiters);
    assert_eq!(p.rq.ready_len(), 5);

    // Round-robin with max = 2 across 6 calls:
    // [0,1], [2,3], [4,0], [1,2], [3,4], [0,1].
    let expected_batches: &[&[u64]] = &[&[0, 1], &[2, 3], &[4, 0], &[1, 2], &[3, 4], &[0, 1]];
    for &expected in expected_batches {
        let (out, done) = p.scan_with(2, |_, _| EPOLLIN, |_| {});
        assert!(done.wake_waiters);
        let got: Vec<u64> = out.iter().map(|e| e.data).collect();
        assert_eq!(got, expected);
        p.assert_invariants();
    }

    // Large max reports each ready LT item exactly once per scan (no LT duplicates).
    let out = p.scan(100, |_, _| EPOLLIN);
    let got: Vec<u64> = out.iter().map(|e| e.data).collect();
    assert_eq!(got, vec![2, 3, 4, 0, 1]);

    // Stale entry ahead of ready entry in a full-list scan with max = 1 still
    // skips the stale entry and reports the ready entry (no missed fd).
    let out = p.scan(1, |k, _| if k.1 == 2 { 0 } else { EPOLLIN });
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), vec![3]);
    assert_eq!(p.rq.ready_len(), 4); // [4, 0, 1, 3]

    // When multiple LT items become unready, remaining items stay fair.
    let out = p.scan(2, |k, _| if k.1 == 4 { 0 } else { EPOLLIN });
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(p.rq.ready_len(), 3); // [3, 0, 1]
    p.assert_invariants();
}

#[test]
fn et_reported_once_per_wake() {
    let mut p = Pair::<u32>::new();
    let h = p.add(key(10, 3), EPOLLIN | EPOLLET, 77, 1).unwrap();

    assert!(p.scan(4, |_, _| EPOLLIN).is_empty());
    assert_eq!(p.rq.wake(h, EPOLLOUT), Wake::Filtered);
    assert!(!p.rq.has_ready());

    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    assert!(Wake::Queued.should_wake_waiters());
    assert!(Wake::Queued.should_notify_subscribers());
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::AlreadyQueued);
    assert!(Wake::AlreadyQueued.should_wake_waiters());
    assert!(Wake::AlreadyQueued.should_notify_subscribers());
    assert!(!Wake::Filtered.should_wake_waiters());
    assert!(!Wake::Filtered.should_notify_subscribers());
    assert!(!Wake::Stale.should_wake_waiters());
    assert!(!Wake::Stale.should_notify_subscribers());
    assert_eq!(p.rq.ready_len(), 1);

    let (out, done) = p.scan_with(4, |_, _| EPOLLIN, |_| {});
    assert_eq!(out, vec![Event { handle: h, events: EPOLLIN, data: 77 }]);
    assert!(!done.wake_waiters);
    assert!(!p.rq.has_ready());

    assert!(p.scan(4, |_, _| EPOLLIN).is_empty());

    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    let out = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h, events: EPOLLIN, data: 77 }]);
    assert!(!p.rq.has_ready());

    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    let (out, done) = p.scan_with(4, |_, _| 0, |_| {});
    assert!(out.is_empty());
    assert!(!done.wake_waiters);
    p.assert_invariants();
}

#[test]
fn oneshot_disarm_and_rearm_by_modify() {
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
    let (out, done) = p.scan_with(1, |_, _| EPOLLIN, |_| {});
    assert_eq!(out, vec![Event { handle: h_other, events: EPOLLIN, data: 222 }]);
    assert!(done.wake_waiters);
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
fn post_scan_order_matches_linux_ep_done_scan() {
    // Pins Linux `fs/eventpoll.c` `ep_done_scan` ordering:
    // 1. unvisited remainder spliced at the very front,
    // 2. entries woken mid-scan placed at the head (ahead of LT requeues),
    // 3. LT requeues at the tail.
    let mut p = Pair::<u32>::new();
    let h_lt = p.add(key(1, 1), EPOLLIN, 1, 1).unwrap();
    let h_zero = p.add(key(2, 2), EPOLLIN | EPOLLET, 2, 2).unwrap();
    let h_et = p.add(key(3, 3), EPOLLIN | EPOLLET, 3, 3).unwrap();
    let h_os = p.add(key(4, 4), EPOLLIN | EPOLLONESHOT, 4, 4).unwrap();
    let h_unvisited = p.add(key(5, 5), EPOLLIN, 5, 5).unwrap();
    let h_outside = p.add(key(6, 6), EPOLLIN | EPOLLET, 6, 6).unwrap();

    // Initial ready list: [h_lt, h_zero, h_et, h_os, h_unvisited].
    for h in [h_lt, h_zero, h_et, h_os, h_unvisited] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    // Scan with max = 3:
    // - h_lt: reported (#1), LT -> tail requeue;
    // - h_zero: polled 0, woken mid-scan -> head (woken_requeue);
    // - h_et: reported (#2), ET, woken mid-scan -> head (woken_requeue);
    // - h_os: reported (#3 == max), ONESHOT, woken mid-scan -> STILL disarmed;
    // - h_unvisited: unvisited -> spliced in front of mid-scan wakes;
    // - h_outside: not in scan, woken mid-scan -> Wake::Queued, ahead of LT tail.
    let (out, done) = p.scan_with(
        3,
        |k, _| if k.1 == 2 { 0 } else { EPOLLIN },
        |rq| {
            // Mid-scan wake on entries in the scan returns Wake::AlreadyQueued
            // (still wakes waiters and notifies subscribers, and records woke_during_scan).
            assert_eq!(rq.wake(h_zero, EPOLLIN), Wake::AlreadyQueued);
            assert!(Wake::AlreadyQueued.should_wake_waiters());
            assert!(Wake::AlreadyQueued.should_notify_subscribers());
            assert_eq!(rq.wake(h_et, EPOLLIN), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h_os, EPOLLIN), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h_unvisited, EPOLLIN), Wake::AlreadyQueued);
            // Non-matching mask mid-scan is still filtered.
            assert_eq!(rq.wake(h_et, EPOLLOUT), Wake::Filtered);
            // Entry outside scan enters `ready` immediately -> Wake::Queued.
            assert_eq!(rq.wake(h_outside, EPOLLIN), Wake::Queued);
            assert_eq!(rq.wake(h_outside, EPOLLIN), Wake::AlreadyQueued);
        },
    );
    assert!(done.wake_waiters);
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), vec![1, 3, 4]);
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);

    // Exact resulting order:
    // [h_unvisited (5), h_zero (2), h_et (3), h_outside (6), h_lt (1)].
    let next = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(next.iter().map(|e| e.data).collect::<Vec<_>>(), vec![5, 2, 3, 6, 1]);
    p.assert_invariants();
}

#[test]
fn two_waiters_events_available_and_scan_done_wakeup() {
    // Reproduces the two-waiter lost-wakeup scenario:
    // 1. Waiter 1 starts a scan, detaching [h1, h2]. `has_ready()` becomes false,
    //    but `events_available()` stays true so Waiter 2 will not sleep while
    //    events are in transit.
    // 2. Waiter 1 finishes with max = 1: `ScanDone.has_ready` is true, telling
    //    Waiter 1 to wake `ep->wq` so Waiter 2 wakes and collects h2.
    let mut p = Pair::<u32>::new();
    let h1 = p.add(key(1, 1), EPOLLIN | EPOLLET, 10, 1).unwrap();
    let h2 = p.add(key(2, 2), EPOLLIN | EPOLLET, 20, 2).unwrap();
    assert!(!p.rq.events_available());

    assert_eq!(p.rq.wake(h1, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h2, EPOLLIN), Wake::Queued);
    assert!(p.rq.events_available());

    let mut hbuf = vec![Handle::INVALID; p.set.len()];
    let scan = p.rq.begin_scan(&mut hbuf);
    assert!(!p.rq.has_ready());
    assert!(p.rq.events_available(), "waiter 2 must see active scan as events_available()");

    // Waiter 1 finishes with max = 1 (taking only h1), leaving h2 queued.
    let mut out1 = Vec::new();
    let done1 = p.rq.finish_scan(&p.set, scan, &[EPOLLIN, EPOLLIN], 1, &mut out1);
    assert_eq!(done1, ScanDone { reported: 1, wake_waiters: true });
    assert_eq!(out1, vec![Event { handle: h1, events: EPOLLIN, data: 10 }]);
    assert!(p.rq.events_available());

    // Woken by `done1.wake_waiters`, Waiter 2 scans and receives h2.
    let (out2, done2) = p.scan_with(1, |_, _| EPOLLIN, |_| {});
    assert_eq!(done2, ScanDone { reported: 1, wake_waiters: false });
    assert_eq!(out2, vec![Event { handle: h2, events: EPOLLIN, data: 20 }]);
    assert!(!p.rq.events_available());
    p.assert_invariants();
}

#[test]
#[should_panic(expected = "shorter than ready_len")]
fn begin_scan_refuses_short_buffer() {
    let mut p = Pair::<u32>::new();
    let h1 = p.add(key(1, 1), EPOLLIN, 1, 1).unwrap();
    let h2 = p.add(key(2, 2), EPOLLIN, 2, 2).unwrap();
    assert_eq!(p.rq.wake(h1, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h2, EPOLLIN), Wake::Queued);
    let mut too_short = [Handle::INVALID; 1];
    let _scan = p.rq.begin_scan(&mut too_short);
}

#[test]
#[should_panic(expected = "Scan dropped")]
fn dropping_unfinished_scan_panics() {
    let mut p = Pair::<u32>::new();
    let h1 = p.add(key(1, 1), EPOLLIN, 1, 1).unwrap();
    assert_eq!(p.rq.wake(h1, EPOLLIN), Wake::Queued);
    let mut buf = [Handle::INVALID; 1];
    let _scan = p.rq.begin_scan(&mut buf);
}

#[test]
fn abort_scan_restores_batch_to_front_and_signals_ready() {
    let mut p = Pair::<u32>::new();
    let h1 = p.add(key(1, 1), EPOLLIN | EPOLLONESHOT, 10, 1).unwrap();
    let h2 = p.add(key(2, 2), EPOLLIN | EPOLLET, 20, 2).unwrap();
    let h3 = p.add(key(3, 3), EPOLLIN, 30, 3).unwrap();
    assert_eq!(p.rq.wake(h1, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h2, EPOLLIN), Wake::Queued);

    let mut buf = [Handle::INVALID; 3];
    let scan = p.rq.begin_scan(&mut buf);
    assert_eq!(p.rq.wake(h3, EPOLLIN), Wake::Queued);
    assert!(p.rq.abort_scan(scan));
    assert_eq!(p.rq.ready_len(), 3);

    // Order is [h1, h2, h3]; h1 still armed and h2 still queued.
    let out = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), vec![10, 20, 30]);
    p.assert_invariants();
}

#[test]
fn set_is_authority_on_forgotten_unlink_and_dropped_added() {
    // Reproduces scratch_r2's `partial_remove_range_drop_leaks_rq_slot_and_kills_next_interest`
    // for both unqueued and queued un-unlinked entries, plus a dropped `Added`.
    let mut set = EpollSet::<Key, u32>::new();
    let mut rq = ReadyQueue::new();

    // 1. Dropped `Added` before any `activate` must NOT desync confirmed ready_capacity!
    let dropped_first = set.add(key(0, 0), EPOLLIN, 0, 0).unwrap();
    drop(dropped_first);
    assert_eq!(rq.capacity(), 0);
    // Remove slot 0 so the next `add` reuses slot 0 via free_head: must still request growth!
    let _ = set.remove(key(0, 0)).unwrap();
    let added = set.add(key(9, 1), EPOLLIN, 1, 1).unwrap();
    let h1 = added.handle();
    let old = rq.activate(&mut set, added);
    drop(old);
    assert!(rq.capacity() >= 16);

    let added2 = set.add(key(9, 2), EPOLLIN, 2, 2).unwrap();
    let h2 = added2.handle();
    drop(rq.activate(&mut set, added2));
    // Queue h2 so we also exercise taking over a slot with `in_ready == true`!
    assert_eq!(rq.wake(h2, EPOLLIN), Wake::Queued);

    // Partially consume RemoveRange and drop it without unlinking (9, 2).
    {
        let mut it = set.remove_range((9, 0)..=(9, u32::MAX));
        let first = it.next().unwrap();
        assert_eq!(first.1, h1);
        rq.unlink(first.1);
    }
    // (9, 2) was removed from `set` but never unlinked from `rq` (still in_ready!).
    assert_eq!(rq.ready_len(), 1);

    // Next add reuses (9, 2)'s slot: `activate` takes over and unlinks the stale occupant!
    let added3 = set.add(key(5, 5), EPOLLIN, 55, 5).unwrap();
    let h3 = added3.handle();
    assert_eq!(h3.index(), h2.index());
    drop(rq.activate(&mut set, added3));
    assert_eq!(rq.ready_len(), 0);
    assert_eq!(rq.wake(h2, EPOLLIN), Wake::Stale);
    assert_eq!(rq.wake(h3, EPOLLIN), Wake::Queued);

    // `apply` also takes over if `activate` had been skipped on an older slot.
    let m = set.modify(key(5, 5), EPOLLOUT, 56).unwrap();
    rq.apply(m);
    assert_eq!(rq.wake(h3, EPOLLOUT), Wake::AlreadyQueued);
    set.assert_invariants();
    rq.assert_invariants();
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

    // After remove, old handle and Handle::INVALID are stale everywhere.
    for stale in [h_old, Handle::INVALID, Handle::new(9999, 1)] {
        assert_eq!(p.rq.wake(stale, EPOLLIN | EPOLLERR), Wake::Stale);
        assert!(p.set.get(stale).is_none());
        assert!(p.set.payload_mut(stale).is_none());
        p.rq.unlink(stale);
        assert!(!p.rq.unreport([stale]));
    }

    // Adding a new key reuses the same slot index with a bumped generation.
    let h_new = p.add(key(2, 2), EPOLLIN, 20, 200).unwrap();
    assert_eq!(h_new.index(), h_old.index());
    assert_ne!(h_new.generation(), h_old.generation());

    // Old handle is still rejected and cannot perturb the new occupant.
    assert_eq!(p.rq.wake(h_old, EPOLLIN), Wake::Stale);
    assert!(p.set.get(h_old).is_none());
    p.rq.unlink(h_old);
    assert!(!p.rq.unreport([h_old]));

    assert_eq!(p.rq.wake(h_new, EPOLLIN), Wake::Queued);
    let out = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(out, vec![Event { handle: h_new, events: EPOLLIN, data: 20 }]);
    p.assert_invariants();
}

#[test]
fn growth_via_prepare_grow_and_install_preserves_state_and_handles_nomem() {
    let mut set = EpollSet::<Key, u32>::new();
    let mut rq = ReadyQueue::new();
    assert_eq!(rq.capacity(), 0);

    assert_eq!(ReadyQueue::prepare_grow(usize::MAX).unwrap_err(), Errno::NoMem);
    assert_eq!(ReadyQueue::prepare_grow((u32::MAX as usize) + 1).unwrap_err(), Errno::NoMem);

    // Add 40 interests (crossing 0 -> 16 -> 32 -> 64 capacity boundaries),
    // queueing half before each growth to verify links and flags survive.
    let mut growth_count = 0usize;
    for i in 0..40u32 {
        let added = set.add(key(u64::from(i), i), EPOLLIN, u64::from(i), i).unwrap();
        let h = added.handle();
        let old_cap = rq.capacity();
        if let Some(old) = rq.activate(&mut set, added) {
            growth_count += 1;
            assert_eq!(old.capacity(), old_cap);
            assert!(rq.capacity() > old_cap);
            drop(old);
        }
        if i % 2 == 0 {
            assert_eq!(rq.wake(h, EPOLLIN), Wake::Queued);
        }
    }
    assert!(growth_count >= 3, "expected multiple ReadyQueue growths, got {growth_count}");
    assert_eq!(rq.ready_len(), 20);

    // Installing a smaller-or-equal storage is a safe no-op returning it back.
    let small = ReadyQueue::prepare_grow(4).unwrap();
    let cap_before = rq.capacity();
    let returned = rq.install(small);
    assert_eq!(returned.capacity(), 4);
    assert_eq!(rq.capacity(), cap_before);

    let mut hbuf = vec![Handle::INVALID; set.len()];
    let scan = rq.begin_scan(&mut hbuf);
    let polled = vec![EPOLLIN; scan.len()];
    let mut out = Vec::new();
    let done = rq.finish_scan(&set, scan, &polled, 40, &mut out);
    assert_eq!(done, ScanDone { reported: 20, wake_waiters: true });
    let expected: Vec<u64> = (0..40u64).step_by(2).collect();
    assert_eq!(out.iter().map(|e| e.data).collect::<Vec<_>>(), expected);
    set.assert_invariants();
    rq.assert_invariants();
}

#[test]
fn unreport_on_copy_out_failure_loses_no_oneshot_or_et() {
    let mut p = Pair::<u32>::new();
    let h_lt0 = p.add(key(1, 1), EPOLLIN, 10, 1).unwrap();
    let h_os = p.add(key(2, 2), EPOLLIN | EPOLLONESHOT, 20, 2).unwrap();
    let h_et = p.add(key(3, 3), EPOLLIN | EPOLLET, 30, 3).unwrap();
    let h_lt1 = p.add(key(4, 4), EPOLLIN, 40, 4).unwrap();
    let h_unvisited = p.add(key(5, 5), EPOLLIN, 50, 5).unwrap();

    for h in [h_lt0, h_os, h_et, h_lt1, h_unvisited] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

    let events = p.scan(4, |_, _| EPOLLIN);
    assert_eq!(events.iter().map(|e| e.data).collect::<Vec<_>>(), vec![10, 20, 30, 40]);
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);

    // Partial copy fault at index 1: roll back [h_os, h_et, h_lt1].
    assert!(p.rq.unreport(events[1..].iter().map(|e| e.handle)));

    let retry = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(retry.iter().map(|e| e.data).collect::<Vec<_>>(), vec![20, 30, 40, 50, 10]);
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Filtered);

    // Full rollback at index 0 (-EFAULT).
    p.modify(key(2, 2), EPOLLIN | EPOLLONESHOT, 21).unwrap();
    assert_eq!(p.rq.wake(h_os, EPOLLIN), Wake::Queued);
    assert_eq!(p.rq.wake(h_et, EPOLLIN), Wake::Queued);
    let batch = p.scan(10, |k, _| if k.1 == 2 || k.1 == 3 { EPOLLIN } else { 0 });
    assert_eq!(batch.iter().map(|e| e.data).collect::<Vec<_>>(), vec![21, 30]);
    assert!(p.rq.unreport(batch.iter().map(|e| e.handle)));
    let retry2 = p.scan(10, |_, _| EPOLLIN);
    assert_eq!(retry2.iter().map(|e| e.data).collect::<Vec<_>>(), vec![21, 30]);
    p.assert_invariants();
}

#[test]
#[should_panic(expected = "unreport called while handle is in an active scan")]
fn unreport_panics_if_handle_is_in_active_scan() {
    let mut p = Pair::<u32>::new();
    let h = p.add(key(1, 1), EPOLLIN, 10, 1).unwrap();
    assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    let mut buf = [Handle::INVALID; 1];
    let scan = p.rq.begin_scan(&mut buf);
    // Abort on unwind not needed; we intentionally panic inside `unreport` before drop.
    // Mark `scan` finished via abort only if unreport didn't panic (it must panic).
    let _ = p.rq.unreport([h]);
    let _ = p.rq.abort_scan(scan);
}

#[test]
fn outer_et_watching_inner_lt_does_not_feedback_loop() {
    // Reproduces the Round 3 feedback loop:
    // An outer epoll watches an inner epoll with EPOLLET; the inner epoll
    // watches a level-triggered pipe. Notifications follow the B5 protocol:
    // - Only a real source wake returning `should_notify_subscribers()` calls
    //   `outer.rq.wake(h_inner, EPOLLIN | EPOLLRDNORM)`.
    // - `finish_peek` / `finish_scan`'s `wake_waiters` wakes only the inner
    //   epoll's own `ep->wq` waiters and NEVER notifies `outer`.
    let mut inner = Pair::<u32>::new();
    let mut outer = Pair::<u32>::new();

    let h_pipe = inner.add(key(1, 10), EPOLLIN, 100, 10).unwrap();
    let h_inner = outer.add(key(99, 50), EPOLLIN | EPOLLET, 999, 50).unwrap();
    let mut inner_waiter_wakes = 0usize;

    // Protocol-faithful real source wake on the pipe -> forwards to outer iff
    // `Wake::should_notify_subscribers()`.
    let wake_pipe = |inner: &mut Pair<u32>, outer: &mut Pair<u32>, mask: u32| -> (Wake, Option<Wake>) {
        let w = inner.rq.wake(h_pipe, mask);
        let outer_w =
            if w.should_notify_subscribers() { Some(outer.rq.wake(h_inner, EPOLLIN | EPOLLRDNORM)) } else { None };
        (w, outer_w)
    };

    // Protocol-faithful inner `Pollable::poll`: wakes ONLY inner's `ep->wq`
    // waiters on `done.wake_waiters`, never notifies `outer`.
    let poll_inner = |inner: &mut Pair<u32>, pipe_mask: u32, wakes: &mut usize| -> u32 {
        let mut hbuf = vec![Handle::INVALID; inner.set.len()];
        let scan = inner.rq.begin_scan(&mut hbuf);
        let polled = vec![pipe_mask; scan.len()];
        let done = inner.rq.finish_peek(scan, &polled);
        if done.wake_waiters {
            *wakes += 1;
        }
        if done.ready { EPOLLIN | EPOLLRDNORM } else { 0 }
    };

    // 1. Real source wake on pipe -> queues inner -> notifies outer subscriber.
    assert_eq!(wake_pipe(&mut inner, &mut outer, EPOLLIN), (Wake::Queued, Some(Wake::Queued)));

    // 2. First outer wait reports inner once and consumes the ET edge.
    let (out1, done1) = outer.scan_with(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes), |_| {});
    assert_eq!(out1, vec![Event { handle: h_inner, events: EPOLLIN, data: 999 }]);
    assert!(!done1.wake_waiters, "outer ET interest must not self-requeue on peek alone");
    assert!(!outer.rq.has_ready());
    assert_eq!(inner_waiter_wakes, 1);

    // 3. Repeated outer waits with no new source wake report nothing even though
    //    inner's LT pipe is still ready and inner's own scans requeue it!
    for _ in 0..3 {
        let (out_again, done_again) =
            outer.scan_with(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes), |_| {});
        assert!(out_again.is_empty());
        assert!(!done_again.wake_waiters);
    }
    // Inner's own epoll_wait requeues the LT pipe (`wake_waiters == true`),
    // which wakes only inner's waiters, never outer.
    let (inner_out, inner_done) = inner.scan_with(4, |_, _| EPOLLIN, |_| {});
    assert_eq!(inner_out, vec![Event { handle: h_pipe, events: EPOLLIN, data: 100 }]);
    assert!(inner_done.wake_waiters);
    assert!(!outer.rq.has_ready());

    // 4. Codex's Round 4 scenario: pipe is drained by the application WITHOUT
    //    any inner scan pruning `h_pipe` (so `h_pipe` is STILL queued on
    //    `inner.rq`), then a new real write arrives on the pipe!
    //    `inner.rq.wake` returns `Wake::AlreadyQueued`, which MUST still notify
    //    subscribers so `outer` does not miss the new edge!
    assert!(inner.rq.has_ready(), "h_pipe is still queued inside inner");
    assert_eq!(
        wake_pipe(&mut inner, &mut outer, EPOLLIN),
        (Wake::AlreadyQueued, Some(Wake::Queued)),
        "source wake on already-queued inner entry must notify outer ET subscriber"
    );
    let (out2, done2) = outer.scan_with(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes), |_| {});
    assert_eq!(out2, vec![Event { handle: h_inner, events: EPOLLIN, data: 999 }]);
    assert!(!done2.wake_waiters, "outer ET interest must still not feedback-loop after peek");
    assert!(outer.scan(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes)).is_empty());

    // 5. Also check full drain + inner prune + fresh source wake -> outer reports once again.
    assert!(inner.scan(4, |_, _| 0).is_empty());
    assert!(!inner.rq.has_ready());
    assert_eq!(wake_pipe(&mut inner, &mut outer, EPOLLIN), (Wake::Queued, Some(Wake::Queued)));

    let (out3, done3) = outer.scan_with(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes), |_| {});
    assert_eq!(out3, vec![Event { handle: h_inner, events: EPOLLIN, data: 999 }]);
    assert!(!done3.wake_waiters);
    assert!(outer.scan(4, |_, _| poll_inner(&mut inner, EPOLLIN, &mut inner_waiter_wakes)).is_empty());
    inner.assert_invariants();
    outer.assert_invariants();
}

#[test]
fn nested_epoll_peek_drained_and_ready_pipes() {
    let mut inner = Pair::<u32>::new();
    let mut outer = Pair::<u32>::new();

    let h_pipe_et = inner.add(key(1, 10), EPOLLIN | EPOLLET, 100, 10).unwrap();
    let h_pipe_os = inner.add(key(2, 11), EPOLLIN | EPOLLONESHOT, 101, 11).unwrap();
    let h_inner_in_outer = outer.add(key(99, 50), EPOLLIN, 999, 50).unwrap();

    // Forward source wakes on `inner` through `outer.rq` whenever the protocol
    // says `should_notify_subscribers()`.
    let wake_inner = |inner: &mut Pair<u32>, outer_rq: &mut ReadyQueue, h: Handle, mask: u32| -> Wake {
        let w = inner.rq.wake(h, mask);
        if w.should_notify_subscribers() {
            let _ = outer_rq.wake(h_inner_in_outer, EPOLLIN | EPOLLRDNORM);
        }
        w
    };

    // Inner epoll's `Pollable::poll`: sizes buffer from `set.len()`, runs
    // `begin_scan` + `finish_peek`; `PeekDone.wake_waiters` wakes only inner's
    // `ep->wq` waiters and never forwards to `outer`.
    let poll_inner = |inner: &mut Pair<u32>,
                      outer_rq: &mut ReadyQueue,
                      pipe_state: &[u32; 2],
                      mid_peek: &mut dyn FnMut(&mut ReadyQueue, &mut ReadyQueue)|
     -> (u32, PeekDone) {
        let mut hbuf = vec![Handle::INVALID; inner.set.len()];
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
        mid_peek(&mut inner.rq, outer_rq);
        let done = inner.rq.finish_peek(scan, &polled);
        (if done.ready { EPOLLIN | EPOLLRDNORM } else { 0 }, done)
    };

    // Scenario 1 (drained pipe): source wake notifies outer via protocol, then
    // pipe drains before outer scans -> outer gets 0, inner prunes drained entry
    // without disarming ONESHOT.
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_os, EPOLLIN), Wake::Queued);
    assert!(outer.rq.has_ready());
    let drained = [0, 0];
    let mut obuf = vec![Handle::INVALID; outer.set.len()];
    let oscan = outer.rq.begin_scan(&mut obuf);
    let (imask, idone) = poll_inner(&mut inner, &mut outer.rq, &drained, &mut |_, _| {});
    assert_eq!(imask, 0);
    assert_eq!(idone, PeekDone { ready: false, wake_waiters: false });
    let mut oout = Vec::new();
    let odone = outer.rq.finish_scan(&outer.set, oscan, &[imask], 4, &mut oout);
    assert!(oout.is_empty(), "outer must not spuriously report drained inner pipe");
    assert!(!odone.wake_waiters);
    assert!(!inner.rq.has_ready());

    // Scenario 2 (ready pipes: ET + ONESHOT): source wakes forward to outer;
    // outer polls inner -> sees EPOLLIN; inner drops/disarms nothing.
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_os, EPOLLIN), Wake::Queued);
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_et, EPOLLIN), Wake::Queued);
    let both_ready = [EPOLLIN, EPOLLIN];
    let oscan = outer.rq.begin_scan(&mut obuf);
    let (imask, idone) = poll_inner(&mut inner, &mut outer.rq, &both_ready, &mut |_, _| {});
    assert_eq!(idone, PeekDone { ready: true, wake_waiters: true });
    let mut oout = Vec::new();
    let _ = outer.rq.finish_scan(&outer.set, oscan, &[imask], 4, &mut oout);
    assert_eq!(oout, vec![Event { handle: h_inner_in_outer, events: EPOLLIN, data: 999 }]);
    assert_eq!(inner.rq.ready_len(), 2);

    let inner_events = inner.scan(4, |_, _| EPOLLIN);
    assert_eq!(
        inner_events,
        vec![
            Event { handle: h_pipe_os, events: EPOLLIN, data: 101 },
            Event { handle: h_pipe_et, events: EPOLLIN, data: 100 },
        ]
    );

    // Scenario 3 (stale-then-ready): prunes stale, stops at ready, returns ready = true.
    inner.modify(key(2, 11), EPOLLIN | EPOLLONESHOT, 102).unwrap();
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_et, EPOLLIN), Wake::Queued);
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_os, EPOLLIN), Wake::Queued);
    let only_os_ready = [0, EPOLLIN];
    let (mask, done) = poll_inner(&mut inner, &mut outer.rq, &only_os_ready, &mut |_, _| {});
    assert_eq!(mask, EPOLLIN | EPOLLRDNORM);
    assert_eq!(done, PeekDone { ready: true, wake_waiters: true });
    assert_eq!(inner.rq.ready_len(), 1);
    let inner_events = inner.scan(4, |_, _| EPOLLIN);
    assert_eq!(inner_events, vec![Event { handle: h_pipe_os, events: EPOLLIN, data: 102 }]);

    // Scenario 4 (mid-peek real source wake on drained entry):
    // During outer's scan -> poll_inner -> after poll(pipe) == 0 -> a real
    // source wake fires on inner (`Wake::AlreadyQueued`, `in_scan == true`),
    // which forwards to outer (`Wake::AlreadyQueued` on h_inner_in_outer).
    // `finish_peek` returns `PeekDone { ready: false, wake_waiters: true }`
    // (waking inner's waiters only), and outer's `finish_scan` requeues
    // h_inner_in_outer because of the forwarded source wake so the next outer
    // wait delivers the new data!
    let _ = outer.scan(4, |_, _| 0);
    assert_eq!(wake_inner(&mut inner, &mut outer.rq, h_pipe_et, EPOLLIN), Wake::Queued);
    let oscan = outer.rq.begin_scan(&mut obuf);
    let (mask, done) = poll_inner(&mut inner, &mut outer.rq, &drained, &mut |inner_rq, outer_rq| {
        let w = inner_rq.wake(h_pipe_et, EPOLLIN);
        assert_eq!(w, Wake::AlreadyQueued);
        assert!(w.should_notify_subscribers());
        // Protocol forwards real source wake to outer subscriber mid-scan!
        assert_eq!(outer_rq.wake(h_inner_in_outer, EPOLLIN | EPOLLRDNORM), Wake::AlreadyQueued);
    });
    assert_eq!(mask, 0);
    assert_eq!(done, PeekDone { ready: false, wake_waiters: true });
    let mut oout = Vec::new();
    let odone = outer.rq.finish_scan(&outer.set, oscan, &[mask], 4, &mut oout);
    assert!(oout.is_empty());
    assert!(odone.wake_waiters, "outer must stay ready due to forwarded mid-peek source wake");
    assert!(outer.rq.has_ready());
    assert!(inner.rq.has_ready());

    // Next outer scan polls inner (pipe now readable) and delivers the event!
    let oscan2 = outer.rq.begin_scan(&mut obuf);
    let (mask2, done2) = poll_inner(&mut inner, &mut outer.rq, &[EPOLLIN, 0], &mut |_, _| {});
    assert_eq!(mask2, EPOLLIN | EPOLLRDNORM);
    assert_eq!(done2, PeekDone { ready: true, wake_waiters: true });
    let mut oout2 = Vec::new();
    let _ = outer.rq.finish_scan(&outer.set, oscan2, &[mask2], 4, &mut oout2);
    assert_eq!(oout2, vec![Event { handle: h_inner_in_outer, events: EPOLLIN, data: 999 }]);
    inner.assert_invariants();
    outer.assert_invariants();
}

#[test]
fn remove_range_and_drain_return_payloads_and_bump_generations() {
    let mut p = Pair::<u32>::new();
    let h7_3 = p.add(key(7, 3), EPOLLIN, 73, 703).unwrap();
    let h7_5 = p.add(key(7, 5), EPOLLIN, 75, 705).unwrap();
    let h7_9 = p.add(key(7, 9), EPOLLIN, 79, 709).unwrap();
    let h8_4 = p.add(key(8, 4), EPOLLIN, 84, 804).unwrap();
    for h in [h7_3, h7_5, h7_9, h8_4] {
        assert_eq!(p.rq.wake(h, EPOLLIN), Wake::Queued);
    }

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

    let drained: Vec<_> = p.set.drain().collect();
    assert_eq!(drained, vec![(key(8, 4), h8_4, 804)]);
    p.rq.unlink(h8_4);
    assert!(p.set.is_empty());
    assert_eq!(p.rq.wake(h8_4, EPOLLIN), Wake::Stale);

    let h_fresh = p.add(key(8, 4), EPOLLIN, 85, 805).unwrap();
    assert_ne!(h_fresh, h8_4);
    p.assert_invariants();
}

// =============================================================================
// Naive reference model for randomized differential testing of EpollSet + ReadyQueue
//
// NOTE: This reference model uses its own literal bit constants and checks the
// data-structure implementation (slab reuse, generational handles, intrusive
// ready list, full-list two-phase scan with interleaved wakes, ep_done_scan
// ordering, unreport, peek, abort_scan, growth, remove_range, and drain). The
// hand-written unit tests above pin the Linux UAPI constants and `fs/eventpoll.c`
// semantics directly.
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

    fn begin_scan(&mut self) -> Vec<Handle> {
        let mut batch = Vec::with_capacity(self.ready.len());
        while let Some(h) = self.ready.pop_front() {
            self.in_scan.insert(h);
            batch.push(h);
        }
        batch
    }

    fn finish_scan(&mut self, batch: &[Handle], polled: &[u32], max: usize) -> (Vec<Event>, ScanDone) {
        let visit_limit = batch.len().min(polled.len());
        let mut visited = 0usize;
        let mut out = Vec::new();
        let mut woken_requeue = Vec::new();
        let mut lt_requeue = Vec::new();
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
                    woken_requeue.push(h);
                }
                continue;
            }
            out.push(Event { handle: h, events: revents, data: e.data });
            if (e.events & M_ONESHOT) != 0 {
                e.armed = false;
            } else if (e.events & M_ET) == 0 {
                lt_requeue.push(h);
            } else if woke {
                woken_requeue.push(h);
            }
        }
        for h in woken_requeue.into_iter().rev() {
            self.ready.push_front(h);
        }
        for h in lt_requeue {
            self.ready.push_back(h);
        }
        for &h in batch[visited..].iter().rev() {
            self.in_scan.remove(&h);
            self.woke_during_scan.remove(&h);
            if self.by_handle.contains_key(&h) {
                self.ready.push_front(h);
            }
        }
        let done = ScanDone { reported: out.len(), wake_waiters: !self.ready.is_empty() };
        (out, done)
    }

    fn finish_peek(&mut self, batch: &[Handle], polled: &[u32]) -> PeekDone {
        let visit_limit = batch.len().min(polled.len());
        let mut visited = 0usize;
        let mut found = false;
        let mut woken_requeue = Vec::new();
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
            if woke {
                woken_requeue.push(h);
            }
        }
        for h in woken_requeue.into_iter().rev() {
            self.ready.push_front(h);
        }
        for &h in batch[visited..].iter().rev() {
            self.in_scan.remove(&h);
            self.woke_during_scan.remove(&h);
            if self.by_handle.contains_key(&h) {
                self.ready.push_front(h);
            }
        }
        PeekDone { ready: found, wake_waiters: !self.ready.is_empty() }
    }

    fn abort_scan(&mut self, batch: &[Handle]) -> bool {
        for &h in batch.iter().rev() {
            self.in_scan.remove(&h);
            self.woke_during_scan.remove(&h);
            if self.by_handle.contains_key(&h) {
                self.ready.push_front(h);
            }
        }
        !self.ready.is_empty()
    }

    fn unreport(&mut self, handles: &[Handle]) -> bool {
        for &h in handles.iter().rev() {
            let Some(&key) = self.by_handle.get(&h) else {
                continue;
            };
            let e = self.by_key.get_mut(&key).unwrap();
            e.armed = true;
            self.ready.retain(|&x| x != h);
            self.ready.push_front(h);
        }
        !self.ready.is_empty()
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
            match rng.below(15) {
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
                    let ready = random_ready(&mut rng);
                    live_state[idx] = ready;
                    let h = if rng.below(4) == 0 && !known_handles.is_empty() {
                        known_handles[rng.below(known_handles.len())]
                    } else if let Some(h) = p.set.lookup(k) {
                        h
                    } else {
                        Handle::INVALID
                    };
                    assert_eq!(p.rq.wake(h, ready), model.wake(h, ready), "seed={seed} step={step} wake");
                }
                9..=11 => {
                    if rng.below(3) == 0 {
                        let flip = rng.below(live_state.len());
                        live_state[flip] = random_ready(&mut rng);
                    }
                    let mut hbuf = vec![Handle::INVALID; p.set.len()];
                    let scan = p.rq.begin_scan(&mut hbuf);
                    let mbatch = model.begin_scan();
                    assert_eq!(scan.handles(), mbatch.as_slice(), "seed={seed} step={step} begin_scan");
                    assert_eq!(p.rq.events_available(), !model.ready.is_empty() || !model.in_scan.is_empty());

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
                    let done_p = p.rq.finish_scan(&p.set, scan, &polled, max, &mut out_p);
                    let (out_m, done_m) = model.finish_scan(&mbatch, &polled, max);
                    assert_eq!(done_p, done_m, "seed={seed} step={step} finish_scan done");
                    assert_eq!(out_p, out_m, "seed={seed} step={step} finish_scan events");

                    if !out_p.is_empty() && rng.below(4) == 0 {
                        let fail_at = rng.below(out_p.len());
                        let tail: Vec<Handle> = out_p[fail_at..].iter().map(|e| e.handle).collect();
                        assert_eq!(
                            p.rq.unreport(tail.iter().copied()),
                            model.unreport(&tail),
                            "seed={seed} step={step} unreport"
                        );
                    }
                }
                12 => {
                    // Two-phase nested-epoll peek with interleaved mid-peek wakes!
                    let mut hbuf = vec![Handle::INVALID; p.set.len()];
                    let scan = p.rq.begin_scan(&mut hbuf);
                    let mbatch = model.begin_scan();
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
                13 => {
                    // abort_scan with mid-scan wakes!
                    let mut hbuf = vec![Handle::INVALID; p.set.len()];
                    let scan = p.rq.begin_scan(&mut hbuf);
                    let mbatch = model.begin_scan();
                    assert_eq!(scan.handles(), mbatch.as_slice());
                    for _ in 0..rng.below(3) {
                        let widx = rng.below(live_state.len());
                        let wk = key((widx / 3) as u64, widx as u32);
                        let wready = random_ready(&mut rng);
                        if let Some(wh) = p.set.lookup(wk) {
                            assert_eq!(p.rq.wake(wh, wready), model.wake(wh, wready));
                        }
                    }
                    assert_eq!(p.rq.abort_scan(scan), model.abort_scan(&mbatch), "seed={seed} step={step} abort_scan");
                }
                _ => {
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
            assert_eq!(p.rq.events_available(), !model.ready.is_empty());
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
    let mut lc = LoopCheck::<u64>::new();
    assert_eq!(lc.add(0, 0), Err(Errno::Loop));
    assert!(lc.is_empty());

    assert_eq!(lc.add(0, 1), Ok(()));
    assert_eq!(lc.add(1, 0), Err(Errno::Loop));
    assert!(lc.contains(0, 1));
    assert!(!lc.contains(1, 0));

    assert_eq!(lc.add(1, 2), Ok(()));
    assert_eq!(lc.add(2, 0), Err(Errno::Loop));

    assert_eq!(lc.add(2, 3), Ok(()));
    assert_eq!(lc.add(3, 4), Ok(()));
    assert_eq!(lc.add(4, 0), Err(Errno::Loop));
    // Extending at the leaf (top-down construction: 0->1->2->3->4, then 4->5) -> ELOOP.
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));
    // Prepending at the root (bottom-up construction: 99->0 onto 0->1->2->3->4) -> ELOOP.
    assert_eq!(lc.add(99, 0), Err(Errno::Loop));
    lc.assert_invariants();

    assert!(lc.remove(0, 1));
    assert!(!lc.remove(0, 1));
    assert_eq!(lc.add(4, 5), Ok(()));
    assert_eq!(lc.add(0, 1), Err(Errno::Loop));
    lc.assert_invariants();

    let mut mid = LoopCheck::<u64>::new();
    mid.add(0, 1).unwrap();
    mid.add(1, 2).unwrap();
    mid.add(3, 4).unwrap();
    mid.add(4, 5).unwrap();
    assert_eq!(mid.add(2, 3), Err(Errno::Loop));
    assert!(mid.remove(4, 5));
    assert_eq!(mid.add(2, 3), Ok(()));
    mid.assert_invariants();
}

#[test]
fn loop_check_diamonds_refcounts_and_remove_set() {
    let mut lc = LoopCheck::<u64>::new();

    lc.add(0, 3).unwrap();
    lc.add(0, 1).unwrap();
    lc.add(1, 2).unwrap();
    lc.add(2, 3).unwrap();
    assert_eq!(lc.add(3, 4), Ok(()));
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));

    assert_eq!(lc.add(1, 2), Ok(()));
    assert_eq!(lc.edge_count(1, 2), 2);
    assert!(lc.remove(1, 2));
    assert_eq!(lc.edge_count(1, 2), 1);
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));
    assert!(lc.remove(1, 2));
    assert_eq!(lc.edge_count(1, 2), 0);
    assert_eq!(lc.add(4, 5), Ok(()));
    assert_eq!(lc.add(5, 6), Ok(()));
    assert_eq!(lc.add(6, 7), Err(Errno::Loop));

    lc.remove_set(4);
    assert!(!lc.contains(3, 4));
    assert!(!lc.contains(4, 5));
    assert_eq!(lc.add(6, 7), Ok(()));
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
