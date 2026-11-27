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

    // Linux `fs/eventpoll.c`: `#define EP_MAX_NESTS 4`.
    assert_eq!(EP_MAX_NESTS, 4);

    assert_eq!(Errno::NoEnt.to_errno(), raw_errno::ENOENT);
    assert_eq!(Errno::Exist.to_errno(), raw_errno::EEXIST);
    assert_eq!(Errno::Inval.to_errno(), raw_errno::EINVAL);
    assert_eq!(Errno::Loop.to_errno(), raw_errno::ELOOP);
    assert_eq!(alloc::format!("{}", Errno::NoEnt), "ENOENT");
    assert_eq!(alloc::format!("{}", Errno::Exist), "EEXIST");
    assert_eq!(alloc::format!("{}", Errno::Inval), "EINVAL");
    assert_eq!(alloc::format!("{}", Errno::Loop), "ELOOP");
}

#[test]
fn eexist_enoent_and_einval_cases() {
    let mut set = EpollSet::<Key>::new();
    let k1 = key(1, 3);
    let k2 = key(2, 4);

    // ENOENT on missing modify and remove (Linux `fs/eventpoll.c` `do_epoll_ctl`).
    assert_eq!(set.modify(k1, EPOLLIN, 1), Err(Errno::NoEnt));
    assert_eq!(set.remove(k1), Err(Errno::NoEnt));

    // EINVAL for EPOLLEXCLUSIVE + EPOLLONESHOT (Linux `fs/eventpoll.c`, `EPOLLEXCLUSIVE_OK_BITS`).
    assert_eq!(set.add(k1, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 1), Err(Errno::Inval));
    // EINVAL for EPOLLEXCLUSIVE combined with event bits outside EPOLLEXCLUSIVE_OK_BITS.
    assert_eq!(set.add(k1, EPOLLPRI | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));
    assert_eq!(set.add(k1, EPOLLRDHUP | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));
    assert_eq!(set.add(k1, EPOLLRDNORM | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));
    assert!(!set.contains(k1));

    // Normal add succeeds; duplicate returns EEXIST.
    assert_eq!(set.add(k1, EPOLLIN, 10), Ok(()));
    assert_eq!(set.add(k1, EPOLLOUT, 20), Err(Errno::Exist));

    // Linux `do_epoll_ctl` checks EPOLLEXCLUSIVE validity before duplicate lookup:
    // invalid flags on an existing key return EINVAL, not EEXIST.
    assert_eq!(set.add(k1, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 30), Err(Errno::Inval));

    // Linux `do_epoll_ctl` rejects EPOLLEXCLUSIVE in EPOLL_CTL_MOD before lookup:
    // EINVAL even when key is missing, and EINVAL when key exists.
    assert_eq!(set.modify(k2, EPOLLIN | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));
    assert_eq!(set.modify(k1, EPOLLIN | EPOLLEXCLUSIVE, 1), Err(Errno::Inval));

    // Valid EPOLLEXCLUSIVE add with all EPOLLEXCLUSIVE_OK_BITS succeeds.
    assert_eq!(set.add(k2, EPOLLEXCLUSIVE_OK_BITS, 42), Ok(()));
    // Modifying an existing EPOLLEXCLUSIVE interest returns EINVAL
    // (Linux `fs/eventpoll.c` `do_epoll_ctl`: `if (epi->event.events & EPOLLEXCLUSIVE) error = -EINVAL`).
    assert_eq!(set.modify(k2, EPOLLIN, 43), Err(Errno::Inval));
    assert_eq!(set.modify(k2, EPOLLOUT | EPOLLET, 44), Err(Errno::Inval));

    // Removing an EPOLLEXCLUSIVE interest succeeds, and it can then be re-added normally.
    assert_eq!(set.remove(k2), Ok(()));
    assert_eq!(set.remove(k2), Err(Errno::NoEnt));
    assert_eq!(set.add(k2, EPOLLIN, 50), Ok(()));
    assert_eq!(set.modify(k2, EPOLLOUT, 51), Ok(()));
    set.assert_invariants();
}

#[test]
fn err_and_hup_always_reported_and_wakeup_ignored() {
    let mut set = EpollSet::<Key>::new();
    let k_empty = key(1, 0);
    let k_wakeup = key(2, 1);

    // Interest with events == 0 still monitors EPOLLERR and EPOLLHUP
    // (Linux `fs/eventpoll.c` `do_epoll_ctl`: `epds->events |= EPOLLERR | EPOLLHUP`).
    set.add(k_empty, 0, 100).unwrap();
    assert!(!set.wake(k_empty, EPOLLIN | EPOLLOUT));
    assert!(!set.has_ready());
    assert!(set.wake(k_empty, EPOLLERR));
    assert!(set.has_ready());

    let mut out = Vec::new();
    let n = set.collect(4, |_| EPOLLIN | EPOLLERR | EPOLLHUP | EPOLLET | EPOLLWAKEUP, &mut out);
    assert_eq!(n, 1);
    assert_eq!(out, vec![(EPOLLERR | EPOLLHUP, 100)]);

    // EPOLLWAKEUP is accepted and ignored; private modifier bits never wake or appear in revents.
    set.add(k_wakeup, EPOLLIN | EPOLLWAKEUP, 200).unwrap();
    assert!(!set.wake(k_wakeup, EPOLLWAKEUP));
    assert!(!set.wake(k_wakeup, EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE));
    assert!(set.wake(k_wakeup, EPOLLIN));

    out.clear();
    let n = set.collect(4, |k| if k == k_wakeup { EPOLLIN | EPOLLWAKEUP | EPOLLET } else { 0 }, &mut out);
    assert_eq!(n, 1);
    assert_eq!(out, vec![(EPOLLIN, 200)]);
    set.assert_invariants();
}

#[test]
fn lt_requeue_fairness_round_robin() {
    // Linux `fs/eventpoll.c` `ep_send_events`: LT items are requeued at the
    // tail of `rdllist`, and any unvisited items left in `txlist` when `max`
    // is reached are spliced back to the *front* of `rdllist`.
    let mut set = EpollSet::<Key>::new();
    for i in 0..5u32 {
        let k = key(u64::from(i), i);
        set.add(k, EPOLLIN, u64::from(i)).unwrap();
        assert!(set.wake(k, EPOLLIN));
    }

    // max == 0: polls nothing, preserves ready-list order.
    let mut polled = 0usize;
    let mut out = Vec::new();
    assert_eq!(
        set.collect(
            0,
            |_| {
                polled += 1;
                EPOLLIN
            },
            &mut out,
        ),
        0
    );
    assert_eq!(polled, 0);
    assert!(out.is_empty());
    assert_eq!(set.ready_len(), 5);

    // Round-robin with max = 2 across 6 calls:
    // [0,1], [2,3], [4,0], [1,2], [3,4], [0,1].
    let expected_batches: &[&[u64]] = &[&[0, 1], &[2, 3], &[4, 0], &[1, 2], &[3, 4], &[0, 1]];
    for &expected in expected_batches {
        out.clear();
        let n = set.collect(2, |_| EPOLLIN, &mut out);
        assert_eq!(n, expected.len());
        let got: Vec<u64> = out.iter().map(|&(_, d)| d).collect();
        assert_eq!(got, expected);
        set.assert_invariants();
    }

    // Large max reports each ready LT item exactly once per collect (no infinite loop).
    out.clear();
    let n = set.collect(100, |_| EPOLLIN, &mut out);
    assert_eq!(n, 5);
    let got: Vec<u64> = out.iter().map(|&(_, d)| d).collect();
    assert_eq!(got, vec![2, 3, 4, 0, 1]);

    // When an LT item becomes unready (poll returns 0), it is dropped from the
    // ready list without starving remaining items.
    out.clear();
    let n = set.collect(2, |k| if k.1 == 2 || k.1 == 3 { 0 } else { EPOLLIN }, &mut out);
    assert_eq!(n, 2);
    let got: Vec<u64> = out.iter().map(|&(_, d)| d).collect();
    assert_eq!(got, vec![4, 0]);
    assert_eq!(set.ready_len(), 3); // [1, 4, 0]

    out.clear();
    let n = set.collect(1, |_| EPOLLIN, &mut out);
    assert_eq!(n, 1);
    assert_eq!(out, vec![(EPOLLIN, 1)]);
    set.assert_invariants();
}

#[test]
fn et_reported_once_per_wake() {
    // Linux `fs/eventpoll.c` `ep_send_events`: items with EPOLLET are NOT
    // re-added to `rdllist` after reporting; only a subsequent `ep_poll_callback`
    // (`wake`) queues them again.
    let mut set = EpollSet::<Key>::new();
    let k = key(10, 3);
    set.add(k, EPOLLIN | EPOLLET, 77).unwrap();

    // Not woken yet: collect reports nothing even if poll would return EPOLLIN.
    let mut out = Vec::new();
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 0);

    // Non-matching wake does not queue.
    assert!(!set.wake(k, EPOLLOUT));
    assert!(!set.has_ready());

    // Duplicate wakes before collect collapse to one entry.
    assert!(set.wake(k, EPOLLIN));
    assert!(set.wake(k, EPOLLIN));
    assert_eq!(set.ready_len(), 1);

    // Reported once, then dropped from ready list.
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 77)]);
    assert!(!set.has_ready());

    // Next collect reports nothing even though the underlying resource is still ready.
    out.clear();
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 0);
    assert!(out.is_empty());

    // Another matching wake triggers exactly one more report.
    assert!(set.wake(k, EPOLLIN));
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 77)]);
    assert!(!set.has_ready());

    // Spurious wake where poll returns 0 reports nothing and drops from ready list.
    assert!(set.wake(k, EPOLLIN));
    out.clear();
    assert_eq!(set.collect(4, |_| 0, &mut out), 0);
    assert!(!set.has_ready());
    set.assert_invariants();
}

#[test]
fn oneshot_disarm_and_rearm_by_modify() {
    // Linux `fs/eventpoll.c` `ep_send_events` and `ep_poll_callback`:
    // reporting an EPOLLONESHOT item clears its event mask (`&= EP_PRIVATE_BITS`),
    // suppressing even EPOLLERR/EPOLLHUP until `EPOLL_CTL_MOD` re-arms it.
    let mut set = EpollSet::<Key>::new();
    let k = key(20, 5);
    let k_other = key(21, 6);
    set.add(k, EPOLLIN | EPOLLONESHOT, 111).unwrap();
    set.add(k_other, EPOLLIN, 222).unwrap();

    // Spurious wake (poll == 0) must NOT disarm the one-shot.
    assert!(set.wake(k, EPOLLIN));
    let mut out = Vec::new();
    assert_eq!(set.collect(4, |_| 0, &mut out), 0);
    assert!(set.wake(k, EPOLLIN));

    // If collect stops at `max` before visiting the one-shot, it stays armed and queued.
    assert!(set.wake(k_other, EPOLLIN));
    // Ready list is [k, k_other]; let's test the reverse order too:
    set.remove(k).unwrap();
    set.add(k, EPOLLIN | EPOLLONESHOT, 111).unwrap();
    assert!(set.wake(k, EPOLLIN)); // Ready list is [k_other, k]
    out.clear();
    assert_eq!(set.collect(1, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 222)]);
    assert_eq!(set.ready_len(), 2); // [k, k_other]

    // Now collect visits k, reports it, and disarms it.
    out.clear();
    assert_eq!(set.collect(1, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 111)]);

    // While disarmed, wakes for EPOLLIN, EPOLLERR, and EPOLLHUP are all ignored.
    assert!(!set.wake(k, EPOLLIN));
    assert!(!set.wake(k, EPOLLERR));
    assert!(!set.wake(k, EPOLLHUP));
    out.clear();
    assert_eq!(set.collect(4, |key| if key == k { EPOLLIN | EPOLLERR | EPOLLHUP } else { 0 }, &mut out), 0);

    // `modify` re-arms the one-shot with updated events and data.
    set.modify(k, EPOLLOUT | EPOLLONESHOT, 333).unwrap();
    assert!(!set.wake(k, EPOLLIN)); // no longer watching EPOLLIN
    assert!(set.wake(k, EPOLLOUT));
    out.clear();
    assert_eq!(set.collect(4, |_| EPOLLOUT, &mut out), 1);
    assert_eq!(out, vec![(EPOLLOUT, 333)]);

    // Disarmed again after reporting.
    assert!(!set.wake(k, EPOLLOUT));

    // `modify` without EPOLLONESHOT re-arms it as level-triggered.
    set.modify(k, EPOLLIN, 444).unwrap();
    assert!(set.wake(k, EPOLLIN));
    out.clear();
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 444)]);
    // Still queued (LT) and reports again without another wake.
    out.clear();
    assert_eq!(set.collect(4, |_| EPOLLIN, &mut out), 1);
    assert_eq!(out, vec![(EPOLLIN, 444)]);
    set.assert_invariants();
}

#[test]
fn modify_and_remove_update_queued_entries_and_reuse_slots() {
    let mut set = EpollSet::<Key>::new();
    let k1 = key(1, 1);
    let k2 = key(2, 2);
    let k3 = key(3, 3);
    set.add(k1, EPOLLIN, 1).unwrap();
    set.add(k2, EPOLLIN, 2).unwrap();
    set.add(k3, EPOLLIN, 3).unwrap();
    assert!(set.wake(k1, EPOLLIN));
    assert!(set.wake(k2, EPOLLIN));
    assert!(set.wake(k3, EPOLLIN));

    // Modify queued middle item: keeps its queue position, updates events & data.
    set.modify(k2, EPOLLOUT, 20).unwrap();
    assert_eq!(set.ready_len(), 3);

    // Remove queued head (k1) and tail (k3), leaving only k2.
    set.remove(k1).unwrap();
    set.remove(k3).unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(set.ready_len(), 1);
    set.assert_invariants();

    let mut out = Vec::new();
    assert_eq!(set.collect(4, |_| EPOLLOUT, &mut out), 1);
    assert_eq!(out, vec![(EPOLLOUT, 20)]);

    // Removing the sole queued item empties the ready list cleanly.
    set.remove(k2).unwrap();
    assert!(set.is_empty());
    assert!(!set.has_ready());
    set.assert_invariants();
}

// =============================================================================
// Naive reference model for randomized differential testing of EpollSet
// =============================================================================

struct ModelEntry {
    events: u32,
    data: u64,
    armed: bool,
}

struct NaiveEpoll<K: Ord + Copy> {
    entries: BTreeMap<K, ModelEntry>,
    ready: VecDeque<K>,
}

impl<K: Ord + Copy> NaiveEpoll<K> {
    fn new() -> Self {
        Self { entries: BTreeMap::new(), ready: VecDeque::new() }
    }

    fn add(&mut self, key: K, events: u32, data: u64) -> Result<(), Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 && (events & !EPOLLEXCLUSIVE_OK_BITS) != 0 {
            return Err(Errno::Inval);
        }
        if self.entries.contains_key(&key) {
            return Err(Errno::Exist);
        }
        self.entries.insert(key, ModelEntry { events: (events & !EPOLLWAKEUP) | EP_ALWAYS_EVENTS, data, armed: true });
        Ok(())
    }

    fn modify(&mut self, key: K, events: u32, data: u64) -> Result<(), Errno> {
        if (events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        let Some(e) = self.entries.get_mut(&key) else {
            return Err(Errno::NoEnt);
        };
        if (e.events & EPOLLEXCLUSIVE) != 0 {
            return Err(Errno::Inval);
        }
        e.events = (events & !EPOLLWAKEUP) | EP_ALWAYS_EVENTS;
        e.data = data;
        e.armed = true;
        Ok(())
    }

    fn remove(&mut self, key: K) -> Result<(), Errno> {
        if self.entries.remove(&key).is_none() {
            return Err(Errno::NoEnt);
        }
        self.ready.retain(|&k| k != key);
        Ok(())
    }

    fn wake(&mut self, key: K, ready: u32) -> bool {
        let Some(e) = self.entries.get(&key) else {
            return false;
        };
        if !e.armed {
            return false;
        }
        let watch = e.events & !EP_PRIVATE_BITS;
        if (watch & ready) == 0 {
            return false;
        }
        if !self.ready.contains(&key) {
            self.ready.push_back(key);
        }
        true
    }

    fn collect(&mut self, max: usize, mut poll: impl FnMut(K) -> u32, out: &mut Vec<(u32, u64)>) -> usize {
        if max == 0 || self.ready.is_empty() {
            return 0;
        }
        let mut tx = core::mem::take(&mut self.ready);
        let mut reported = 0usize;
        while reported < max {
            let Some(k) = tx.pop_front() else {
                break;
            };
            let e = self.entries.get_mut(&k).unwrap();
            let watch = e.events & !EP_PRIVATE_BITS;
            let revents = poll(k) & watch;
            if revents == 0 {
                continue;
            }
            out.push((revents, e.data));
            reported += 1;
            if (e.events & EPOLLONESHOT) != 0 {
                e.armed = false;
            } else if (e.events & EPOLLET) == 0 {
                self.ready.push_back(k);
            }
        }
        // Splice remaining tx to front of self.ready.
        while let Some(k) = tx.pop_back() {
            self.ready.push_front(k);
        }
        reported
    }
}

fn random_events(rng: &mut Rng) -> u32 {
    const BASE_BITS: &[u32] = &[EPOLLIN, EPOLLOUT, EPOLLPRI, EPOLLRDHUP, EPOLLERR, EPOLLHUP];
    const MOD_BITS: &[u32] = &[EPOLLET, EPOLLONESHOT, EPOLLWAKEUP, EPOLLEXCLUSIVE];
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
    const BITS: &[u32] = &[EPOLLIN, EPOLLOUT, EPOLLPRI, EPOLLRDHUP, EPOLLERR, EPOLLHUP, EPOLLET, EPOLLWAKEUP];
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
        let mut set = EpollSet::<Key>::new();
        let mut model = NaiveEpoll::<Key>::new();
        let mut live_state = [0u32; 16];

        for step in 0..1500usize {
            let idx = rng.below(live_state.len());
            let k = key((idx / 2) as u64, idx as u32);
            match rng.below(10) {
                0 | 1 => {
                    let ev = random_events(&mut rng);
                    let data = rng.next_u64();
                    assert_eq!(set.add(k, ev, data), model.add(k, ev, data), "seed={seed} step={step} add");
                }
                2 | 3 => {
                    let ev = random_events(&mut rng);
                    let data = rng.next_u64();
                    assert_eq!(set.modify(k, ev, data), model.modify(k, ev, data), "seed={seed} step={step} modify");
                }
                4 => {
                    assert_eq!(set.remove(k), model.remove(k), "seed={seed} step={step} remove");
                }
                5..=7 => {
                    let ready = random_ready(&mut rng);
                    live_state[idx] = ready;
                    assert_eq!(set.wake(k, ready), model.wake(k, ready), "seed={seed} step={step} wake");
                }
                _ => {
                    // Occasionally flip some underlying readiness bits without waking
                    // to exercise re-polling stale / vanished readiness.
                    if rng.below(3) == 0 {
                        let flip = rng.below(live_state.len());
                        live_state[flip] = random_ready(&mut rng);
                    }
                    let max = match rng.below(6) {
                        0 => 0,
                        1 => 1,
                        2 => 2,
                        3 => 3,
                        4 => 5,
                        _ => 32,
                    };
                    let mut out_set = Vec::new();
                    let mut out_model = Vec::new();
                    let n_set = set.collect(max, |k| live_state[k.1 as usize], &mut out_set);
                    let n_model = model.collect(max, |k| live_state[k.1 as usize], &mut out_model);
                    assert_eq!(n_set, n_model, "seed={seed} step={step} collect count");
                    assert_eq!(out_set, out_model, "seed={seed} step={step} collect events/order");
                }
            }
            assert_eq!(set.len(), model.entries.len());
            assert_eq!(set.ready_len(), model.ready.len());
            assert_eq!(set.has_ready(), !model.ready.is_empty());
            set.assert_invariants();
        }
    }
}

// =============================================================================
// LoopCheck tests (cycles, EP_MAX_NESTS = 4, 5-deep chains, DAGs, refcounts)
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
    // Extending to a 5-deep chain (5 edges > EP_MAX_NESTS): 4 -> 5 fails with ELOOP (top-down).
    assert_eq!(lc.add(4, 5), Err(Errno::Loop));
    // Prepending to make a 5-deep chain: 99 -> 0 fails with ELOOP (bottom-up).
    assert_eq!(lc.add(99, 0), Err(Errno::Loop));
    lc.assert_invariants();

    // Removing an edge drops depth to 3 so extending succeeds.
    assert!(lc.remove(0, 1));
    assert!(!lc.remove(0, 1)); // already gone
    assert_eq!(lc.add(4, 5), Ok(())); // 1 -> 2 -> 3 -> 4 -> 5 (4 edges)
    assert_eq!(lc.add(0, 1), Err(Errno::Loop)); // would make 5 edges again
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
