use alloc::boxed::Box;
use alloc::vec::Vec;
use std::collections::BTreeMap;

use super::*;

fn node(waiter: u32, bitset: u32) -> Box<FutexNode<u64, u32>> {
    Box::new(FutexNode::new(waiter, bitset))
}

fn node_any(waiter: u32) -> Box<FutexNode<u64, u32>> {
    Box::new(FutexNode::new_match_any(waiter))
}

#[test]
fn enqueue_rejects_zero_bitset_and_returns_node() {
    let mut table = FutexTable::<u64, u32>::new();
    let err = table.enqueue(0x1000, node(42, 0)).unwrap_err();
    assert_eq!(err.kind, FutexError::ZeroBitset);
    assert_eq!(err.node.bitset(), 0);
    assert_eq!(err.node.key(), None);
    assert_eq!(err.node.into_waiter(), 42);
    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
}

#[test]
fn wake_fifo_order_and_key_isolation() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(1, node_any(10)).unwrap();
    table.enqueue(2, node_any(20)).unwrap();
    table.enqueue(1, node_any(11)).unwrap();
    table.enqueue(3, node_any(30)).unwrap();
    table.enqueue(1, node_any(12)).unwrap();
    table.enqueue(2, node_any(21)).unwrap();

    assert_eq!(table.len(), 6);
    assert_eq!(table.count_for_key(1), 3);
    assert_eq!(table.count_for_key(2), 2);
    assert_eq!(table.count_for_key(3), 1);

    let woken: Vec<u32> = table.wake(1, 2, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(woken, [10, 11]);
    assert_eq!(table.count_for_key(1), 1);
    assert_eq!(table.count_for_key(2), 2);

    let rest_k1: Vec<u32> = table.wake(1, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(rest_k1, [12]);
    assert_eq!(table.count_for_key(1), 0);

    let all_k2: Vec<u32> = table.wake(2, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(all_k2, [20, 21]);

    let all_k3: Vec<u32> = table.wake(3, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(all_k3, [30]);
    assert!(table.is_empty());
}

#[test]
fn wake_bitset_selection() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(7, node(1, 0b0001)).unwrap();
    table.enqueue(7, node(2, 0b0010)).unwrap();
    table.enqueue(7, node(3, 0b0101)).unwrap();
    table.enqueue(7, node(4, 0b1000)).unwrap();
    table.enqueue(7, node(5, 0b0011)).unwrap();

    // Zero bitset is refused before touching the queue, regardless of `nr`.
    assert_eq!(table.wake(7, 1, 0).unwrap_err(), FutexError::ZeroBitset);
    assert_eq!(table.wake(7, 0, 0).unwrap_err(), FutexError::ZeroBitset);
    assert_eq!(table.wake(7, -1, 0).unwrap_err(), FutexError::ZeroBitset);
    assert_eq!(table.len(), 5);

    // Mask `0b0010` matches waiters 2 and 5; asking for 1 skips waiter 1 and
    // takes waiter 2, leaving waiter 5.
    let woken: Vec<u32> = table.wake(7, 1, 0b0010).unwrap().into_waiters().collect();
    assert_eq!(woken, [2]);

    // Mask `0b0100` matches only waiter 3.
    let woken: Vec<u32> = table.wake(7, 10, 0b0100).unwrap().into_waiters().collect();
    assert_eq!(woken, [3]);

    // Mask `0b0001` matches remaining waiters 1 and 5 in FIFO order.
    let woken: Vec<u32> = table.wake(7, 10, 0b0001).unwrap().into_waiters().collect();
    assert_eq!(woken, [1, 5]);

    // Mask `0b0111` matches nothing left (only waiter 4 with `0b1000` remains).
    let woken = table.wake(7, 10, 0b0111).unwrap();
    assert!(woken.is_empty());
    assert_eq!(table.len(), 1);

    let woken: Vec<u32> = table.wake(7, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(woken, [4]);
    assert!(table.is_empty());
}

#[test]
fn wake_nr_edge_cases_match_linux() {
    let mut table = FutexTable::<u64, u32>::new();

    // Empty table: nr == 0 and negative nr wake nothing.
    assert!(table.wake(1, 0, FUTEX_BITSET_MATCH_ANY).unwrap().is_empty());
    assert!(table.wake(1, -1, FUTEX_BITSET_MATCH_ANY).unwrap().is_empty());
    assert!(table.wake(1, i32::MIN, FUTEX_BITSET_MATCH_ANY).unwrap().is_empty());

    for id in 1..=6 {
        table.enqueue(1, node(id, if id == 1 { 0b0010 } else { 0b0001 })).unwrap();
    }

    // Non-matching bitset with nr == 0 / negative still wakes 0.
    assert!(table.wake(1, 0, 0b1000).unwrap().is_empty());
    assert!(table.wake(1, -5, 0b1000).unwrap().is_empty());
    assert_eq!(table.len(), 6);

    // Linux `futex_wake` checks `if (++ret >= nr_wake) break;` *after* waking
    // a matching waiter, so `nr == 0` wakes 1 matching waiter (skipping
    // non-matching waiter 1 to wake waiter 2).
    let woken: Vec<u32> = table.wake(1, 0, 0b0001).unwrap().into_waiters().collect();
    assert_eq!(woken, [2]);

    // `nr == -1` also wakes 1 matching waiter (waiter 3).
    let woken: Vec<u32> = table.wake(1, -1, 0b0001).unwrap().into_waiters().collect();
    assert_eq!(woken, [3]);

    // `nr == i32::MIN` also wakes 1 matching waiter (waiter 4).
    let woken: Vec<u32> = table.wake(1, i32::MIN, 0b0001).unwrap().into_waiters().collect();
    assert_eq!(woken, [4]);

    // `nr == i32::MAX` wakes all remaining matching waiters in FIFO order.
    let woken: Vec<u32> = table.wake(1, i32::MAX, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(woken, [1, 5, 6]);
    assert!(table.is_empty());
}

#[test]
fn requeue_keeps_order_and_preserves_bitset() {
    let mut table = FutexTable::<u64, u32>::new();
    // Existing waiters on `to` (key 20) and bystander key 30 interleaved with `from` (key 10).
    table.enqueue(10, node(1, 0b0001)).unwrap();
    table.enqueue(20, node(100, FUTEX_BITSET_MATCH_ANY)).unwrap();
    table.enqueue(10, node(2, 0b0010)).unwrap();
    table.enqueue(30, node(200, FUTEX_BITSET_MATCH_ANY)).unwrap();
    table.enqueue(10, node(3, 0b0100)).unwrap();
    table.enqueue(20, node(101, FUTEX_BITSET_MATCH_ANY)).unwrap();
    table.enqueue(10, node(4, 0b1000)).unwrap();

    // Wake 1 from key 10 (waiter 1), move 2 (waiters 2, 3) to key 20's tail,
    // leaving waiter 4 on key 10.
    let (woken, moved) = table.requeue(10, 20, 1, 2).unwrap();
    assert_eq!(moved, 2);
    let woken_nodes: Vec<_> = woken.collect();
    assert_eq!(woken_nodes.len(), 1);
    assert_eq!(woken_nodes[0].key(), Some(10));
    assert_eq!(*woken_nodes[0].waiter(), 1);

    assert_eq!(table.count_for_key(10), 1);
    assert_eq!(table.count_for_key(20), 4);
    assert_eq!(table.count_for_key(30), 1);

    // Original bitsets survive requeue: waking `0b0100` on key 20 wakes 100,
    // 101 (MATCH_ANY) and 3 (`0b0100`), skipping 2 (`0b0010`), in FIFO order.
    let woken: Vec<_> = table.wake(20, 10, 0b0100).unwrap().collect();
    assert!(woken.iter().all(|n| n.key() == Some(20)));
    let ids: Vec<u32> = woken.into_iter().map(|n| n.into_waiter()).collect();
    assert_eq!(ids, [100, 101, 3]);

    // Waiter 2 is still on key 20 with bitset `0b0010`.
    let woken: Vec<u32> = table.wake(20, 10, 0b0010).unwrap().into_waiters().collect();
    assert_eq!(woken, [2]);

    // Waiter 4 stayed on key 10, waiter 200 stayed on key 30.
    assert_eq!(table.wake(10, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [4]);
    assert_eq!(table.wake(30, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [200]);
    assert!(table.is_empty());
}

#[test]
fn requeue_nr_edge_cases_match_linux() {
    let mut table = FutexTable::<u64, u32>::new();
    for id in 1..=4 {
        table.enqueue(1, node_any(id)).unwrap();
    }

    // Negative counts return `NegativeCount` (`-EINVAL`) without mutating state.
    assert_eq!(table.requeue(1, 2, -1, 0).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.requeue(1, 2, 0, -1).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.requeue(1, 2, i32::MIN, i32::MIN).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.len(), 4);

    // `(0, 0)` wakes 0 and moves 0 (unlike `wake` with `nr == 0`).
    let (woken, moved) = table.requeue(1, 2, 0, 0).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 4);
    assert_eq!(table.count_for_key(2), 0);

    // `(0, 2)` wakes 0 and moves 2.
    let (woken, moved) = table.requeue(1, 2, 0, 2).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);
    assert_eq!(table.count_for_key(1), 2);
    assert_eq!(table.count_for_key(2), 2);

    // `(1, 0)` wakes 1 and moves 0.
    let (woken, moved) = table.requeue(1, 2, 1, 0).unwrap();
    assert_eq!(woken.into_waiters().collect::<Vec<_>>(), [3]);
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 1);

    // `nr_wake` past remaining count wakes what exists and moves 0.
    let (woken, moved) = table.requeue(1, 2, 5, 10).unwrap();
    assert_eq!(woken.into_waiters().collect::<Vec<_>>(), [4]);
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 0);

    // `from == to`: wakes up to `nr_wake` and keeps remaining FIFO order
    // without rotating or double-counting when `nr_requeue > remaining`.
    table.enqueue(2, node_any(5)).unwrap();
    table.enqueue(2, node_any(6)).unwrap();
    // Key 2 now has [1, 2, 5, 6].
    let (woken, moved) = table.requeue(2, 2, 1, 2).unwrap();
    assert_eq!(woken.into_waiters().collect::<Vec<_>>(), [1]);
    assert_eq!(moved, 2);
    // Remaining order must stay [2, 5, 6] (not rotated to [6, 2, 5]).
    let (woken, moved) = table.requeue(2, 2, 0, 100).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 3);
    assert_eq!(table.wake(2, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [2, 5, 6]);
    assert!(table.is_empty());
}

#[test]
fn requeue_into_across_tables() {
    let mut src = FutexTable::<u64, u32>::new();
    let mut dst = FutexTable::<u64, u32>::new();

    src.enqueue(1, node_any(10)).unwrap();
    src.enqueue(1, node_any(11)).unwrap();
    src.enqueue(1, node_any(12)).unwrap();
    src.enqueue(1, node_any(13)).unwrap();
    dst.enqueue(2, node_any(20)).unwrap();

    assert_eq!(src.requeue_into(1, &mut dst, 2, -1, 1).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(src.requeue_into(1, &mut dst, 2, 1, -1).unwrap_err(), FutexError::NegativeCount);

    let mut notified = Vec::new();
    let (woken, moved) = src.requeue_into_with(1, &mut dst, 2, 1, 2, |w| notified.push(*w)).unwrap();
    assert_eq!(woken.into_waiters().collect::<Vec<_>>(), [10]);
    assert_eq!(moved, 2);
    assert_eq!(notified, [11, 12]);
    assert_eq!(src.len(), 1);
    assert_eq!(dst.len(), 3);

    // Waiter 11 can now be cancelled at key 2 in `dst`, not in `src`.
    assert!(src.cancel(1, &11).is_already_woken());
    let cancelled = dst.cancel(2, &11).into_node().unwrap();
    assert_eq!(cancelled.key(), Some(2));
    assert_eq!(cancelled.into_waiter(), 11);

    // `dst` still has [20, 12] in FIFO order; `src` still has [13].
    assert_eq!(dst.wake(2, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [20, 12]);
    assert_eq!(src.wake(1, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [13]);
    assert!(src.is_empty());
    assert!(dst.is_empty());
}

#[test]
fn cancel_after_wake_reports_already_woken() {
    let mut table = FutexTable::<u64, u32>::new();
    assert!(table.cancel(1, &99).is_already_woken());

    table.enqueue(1, node_any(1)).unwrap();
    table.enqueue(1, node_any(2)).unwrap();
    table.enqueue(1, node_any(3)).unwrap();

    // Wake waiter 1.
    let woken: Vec<u32> = table.wake(1, 1, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(woken, [1]);

    // Cancelling waiter 1 after wake must report `AlreadyWoken`.
    assert!(table.cancel(1, &1).is_already_woken());

    // Cancelling waiter 2 while still queued returns `Cancelled` and removes it.
    let cancelled = table.cancel(1, &2);
    assert!(cancelled.is_cancelled());
    let cancelled_node = cancelled.into_node().unwrap();
    assert_eq!(cancelled_node.key(), Some(1));
    assert_eq!(cancelled_node.into_waiter(), 2);

    // Cancelling waiter 2 a second time reports `AlreadyWoken`.
    assert!(table.cancel(1, &2).is_already_woken());

    // Only waiter 3 remains.
    let rest: Vec<u32> = table.wake(1, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect();
    assert_eq!(rest, [3]);
    assert!(table.is_empty());
}

#[test]
fn cancel_after_requeue_finds_waiter_at_new_key() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(10, node_any(1)).unwrap();
    table.enqueue(10, node_any(2)).unwrap();
    table.enqueue(10, node_any(3)).unwrap();

    // Track current key per waiter via `requeue_with`, as the kernel does.
    let mut current_key = BTreeMap::from([(1u32, 10u64), (2, 10), (3, 10)]);
    let (woken, moved) = table
        .requeue_with(10, 20, 0, 2, |w| {
            current_key.insert(*w, 20);
        })
        .unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);

    // Both `key_of` and the mirrored key report the new key 20 for waiters 1 & 2.
    assert_eq!(table.key_of(&1), Some(20));
    assert_eq!(table.key_of(&2), Some(20));
    assert_eq!(table.key_of(&3), Some(10));
    assert_eq!(current_key[&1], 20);
    assert_eq!(current_key[&2], 20);

    // Cancelling waiter 1 at its old key does not remove it from key 20.
    assert!(table.cancel(10, &1).is_already_woken());
    assert_eq!(table.count_for_key(20), 2);

    // Cancelling waiter 1 at its new key succeeds and returns its node.
    let cancelled = table.cancel(current_key[&1], &1).into_node().unwrap();
    assert_eq!(cancelled.key(), Some(20));
    assert_eq!(cancelled.into_waiter(), 1);
    assert_eq!(table.key_of(&1), None);

    // Waiter 2 remains on key 20, waiter 3 remains on key 10.
    assert_eq!(table.wake(20, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [2]);
    assert_eq!(table.wake(10, 10, FUTEX_BITSET_MATCH_ANY).unwrap().into_waiters().collect::<Vec<_>>(), [3]);
    assert!(table.is_empty());
}

/// Deterministic SplitMix64 PRNG for randomized model tests.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }
}

/// Naive `Vec`-backed reference oracle for [`FutexTable`].
#[derive(Default)]
struct VecModel {
    entries: Vec<(u64, u32, u32)>, // (key, bitset, waiter_id)
}

impl VecModel {
    fn enqueue(&mut self, key: u64, waiter: u32, bitset: u32) -> Result<(), FutexError> {
        if bitset == 0 {
            return Err(FutexError::ZeroBitset);
        }
        self.entries.push((key, bitset, waiter));
        Ok(())
    }

    fn wake(&mut self, key: u64, nr: i32, bitset: u32) -> Result<Vec<u32>, FutexError> {
        if bitset == 0 {
            return Err(FutexError::ZeroBitset);
        }
        let limit = if nr <= 0 { 1 } else { nr as usize };
        let mut woken = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if woken.len() < limit && entry.0 == key && (entry.1 & bitset) != 0 {
                woken.push(entry.2);
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
        Ok(woken)
    }

    fn requeue(&mut self, from: u64, to: u64, nr_wake: i32, nr_requeue: i32) -> Result<(Vec<u32>, usize), FutexError> {
        if nr_wake < 0 || nr_requeue < 0 {
            return Err(FutexError::NegativeCount);
        }
        let wake_limit = nr_wake as usize;
        let requeue_limit = nr_requeue as usize;

        let mut woken = Vec::new();
        let mut moved_entries = Vec::new();
        let mut moved = 0usize;
        let mut kept = Vec::with_capacity(self.entries.len());

        for entry in self.entries.drain(..) {
            if entry.0 == from && woken.len() < wake_limit {
                woken.push(entry.2);
            } else if entry.0 == from && moved < requeue_limit {
                moved += 1;
                if from == to {
                    kept.push(entry);
                } else {
                    moved_entries.push((to, entry.1, entry.2));
                }
            } else {
                kept.push(entry);
            }
        }
        kept.extend(moved_entries);
        self.entries = kept;
        Ok((woken, moved))
    }

    fn cancel(&mut self, key: u64, waiter: u32) -> Option<u32> {
        let pos = self.entries.iter().position(|&(k, _, w)| k == key && w == waiter)?;
        let (_, _, w) = self.entries.remove(pos);
        Some(w)
    }

    fn key_of(&self, waiter: u32) -> Option<u64> {
        self.entries.iter().find(|&&(_, _, w)| w == waiter).map(|&(k, _, _)| k)
    }

    fn count_for_key(&self, key: u64) -> usize {
        self.entries.iter().filter(|&&(k, _, _)| k == key).count()
    }
}

fn random_bitset(rng: &mut Rng) -> u32 {
    match rng.below(10) {
        0 => 0,
        1 => FUTEX_BITSET_MATCH_ANY,
        _ => 1u32 << rng.below(4) | (if rng.below(2) == 0 { 1u32 << rng.below(4) } else { 0 }),
    }
}

fn random_nr(rng: &mut Rng) -> i32 {
    match rng.below(10) {
        0 => i32::MIN,
        1 => -1,
        2 => 0,
        3 => 1,
        4 => 2,
        5 => 3,
        6 => 5,
        7 => i32::MAX,
        _ => (rng.below(8)) as i32,
    }
}

#[test]
fn randomized_model_matches_vec_oracle() {
    for (seed, num_keys, steps) in [(0xdead_beef_cafe_0001u64, 4usize, 5_000usize), (0x1234_5678_9abc_def0, 12, 5_000)]
    {
        let mut rng = Rng(seed);
        let mut table = FutexTable::<u64, u32>::new();
        let mut model = VecModel::default();
        let mut known_waiters: Vec<(u32, u64)> = Vec::new();
        let mut next_waiter: u32 = 1;

        for step in 0..steps {
            match rng.below(10) {
                // Enqueue (40%)
                0..=3 => {
                    let key = rng.below(num_keys) as u64;
                    let bitset = random_bitset(&mut rng);
                    let id = next_waiter;
                    next_waiter += 1;

                    let actual = table.enqueue(key, node(id, bitset)).map_err(|e| e.kind);
                    let expected = model.enqueue(key, id, bitset);
                    assert_eq!(actual, expected, "seed {seed:#x} step {step} enqueue");
                    if actual.is_ok() {
                        known_waiters.push((id, key));
                    }
                }
                // Wake (25%)
                4..=5 => {
                    let key = rng.below(num_keys) as u64;
                    let nr = random_nr(&mut rng);
                    let bitset = random_bitset(&mut rng);

                    let actual = table.wake(key, nr, bitset).map(|w| w.into_waiters().collect::<Vec<_>>());
                    let expected = model.wake(key, nr, bitset);
                    assert_eq!(actual, expected, "seed {seed:#x} step {step} wake({key}, {nr}, {bitset:#x})");
                }
                // Requeue (20%)
                6..=7 => {
                    let from = rng.below(num_keys) as u64;
                    let to = rng.below(num_keys) as u64;
                    let nr_wake = random_nr(&mut rng);
                    let nr_requeue = random_nr(&mut rng);

                    let mut moved_ids = Vec::new();
                    let actual = table
                        .requeue_with(from, to, nr_wake, nr_requeue, |w| moved_ids.push(*w))
                        .map(|(w, m)| (w.into_waiters().collect::<Vec<_>>(), m));
                    let expected = model.requeue(from, to, nr_wake, nr_requeue);
                    assert_eq!(
                        actual, expected,
                        "seed {seed:#x} step {step} requeue({from}, {to}, {nr_wake}, {nr_requeue})"
                    );
                    if let Ok((_, moved)) = actual {
                        assert_eq!(moved_ids.len(), moved);
                        for id in moved_ids {
                            if let Some(slot) = known_waiters.iter_mut().find(|(w, _)| *w == id) {
                                slot.1 = to;
                            }
                        }
                    }
                }
                // Cancel (20%): mix current key, stale key, and already-woken waiters.
                _ => {
                    if known_waiters.is_empty() {
                        continue;
                    }
                    let idx = rng.below(known_waiters.len());
                    let (id, tracked_key) = known_waiters[idx];
                    // Sometimes probe a wrong/stale key to check it doesn't remove from the real key.
                    let probe_key = if rng.below(4) == 0 { rng.below(num_keys) as u64 } else { tracked_key };

                    assert_eq!(table.key_of(&id), model.key_of(id), "seed {seed:#x} step {step} key_of({id})");
                    let actual = table.cancel(probe_key, &id).into_node().map(|n| n.into_waiter());
                    let expected = model.cancel(probe_key, id);
                    assert_eq!(actual, expected, "seed {seed:#x} step {step} cancel({probe_key}, {id})");
                }
            }

            assert_eq!(table.len(), model.entries.len(), "seed {seed:#x} step {step} len");
            for k in 0..num_keys as u64 {
                assert_eq!(
                    table.count_for_key(k),
                    model.count_for_key(k),
                    "seed {seed:#x} step {step} count_for_key({k})"
                );
            }
        }
    }
}

#[test]
fn randomized_multi_bucket_requeue_into_matches_vec_oracle() {
    const BUCKETS: usize = 4;
    let bucket_of = |key: u64| (key as usize) % BUCKETS;

    let mut rng = Rng(0x51a7_cafe_babe_2026);
    let mut buckets: [FutexTable<u64, u32>; BUCKETS] = core::array::from_fn(|_| FutexTable::new());
    let mut model = VecModel::default();
    let mut tracked_key = BTreeMap::<u32, u64>::new();
    let mut known_ids = Vec::<u32>::new();
    let mut next_waiter: u32 = 1;

    for step in 0..4_000 {
        match rng.below(4) {
            0 => {
                let key = rng.below(8) as u64;
                let bitset = random_bitset(&mut rng);
                let id = next_waiter;
                next_waiter += 1;

                let actual = buckets[bucket_of(key)].enqueue(key, node(id, bitset)).map_err(|e| e.kind);
                let expected = model.enqueue(key, id, bitset);
                assert_eq!(actual, expected, "step {step} enqueue");
                if actual.is_ok() {
                    tracked_key.insert(id, key);
                    known_ids.push(id);
                }
            }
            1 => {
                let key = rng.below(8) as u64;
                let nr = random_nr(&mut rng);
                let bitset = random_bitset(&mut rng);

                let actual =
                    buckets[bucket_of(key)].wake(key, nr, bitset).map(|w| w.into_waiters().collect::<Vec<_>>());
                let expected = model.wake(key, nr, bitset);
                assert_eq!(actual, expected, "step {step} wake");
            }
            2 => {
                let from = rng.below(8) as u64;
                let to = rng.below(8) as u64;
                let nr_wake = random_nr(&mut rng);
                let nr_requeue = random_nr(&mut rng);
                let b1 = bucket_of(from);
                let b2 = bucket_of(to);

                let actual = if b1 == b2 {
                    buckets[b1]
                        .requeue_with(from, to, nr_wake, nr_requeue, |w| {
                            tracked_key.insert(*w, to);
                        })
                        .map(|(w, m)| (w.into_waiters().collect::<Vec<_>>(), m))
                } else if b1 < b2 {
                    let (left, right) = buckets.split_at_mut(b2);
                    left[b1]
                        .requeue_into_with(from, &mut right[0], to, nr_wake, nr_requeue, |w| {
                            tracked_key.insert(*w, to);
                        })
                        .map(|(w, m)| (w.into_waiters().collect::<Vec<_>>(), m))
                } else {
                    let (left, right) = buckets.split_at_mut(b1);
                    right[0]
                        .requeue_into_with(from, &mut left[b2], to, nr_wake, nr_requeue, |w| {
                            tracked_key.insert(*w, to);
                        })
                        .map(|(w, m)| (w.into_waiters().collect::<Vec<_>>(), m))
                };
                let expected = model.requeue(from, to, nr_wake, nr_requeue);
                assert_eq!(actual, expected, "step {step} requeue_into({from}->{to})");
            }
            _ => {
                if known_ids.is_empty() {
                    continue;
                }
                let id = known_ids[rng.below(known_ids.len())];
                let key = tracked_key[&id];
                let actual = buckets[bucket_of(key)].cancel(key, &id).into_node().map(|n| n.into_waiter());
                let expected = model.cancel(key, id);
                assert_eq!(actual, expected, "step {step} cancel({key}, {id})");
            }
        }
        let total_len: usize = buckets.iter().map(FutexTable::len).sum();
        assert_eq!(total_len, model.entries.len(), "step {step} total len");
    }
}

#[test]
fn drop_deep_table_and_woken_without_stack_overflow() {
    // Run on a small 256 KiB stack so a recursive `Drop` over 10,000 nodes
    // would overflow immediately, proving iterative `Drop` uses O(1) stack.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            const N: u32 = 10_000;

            let mut table = FutexTable::<u64, u32>::new();
            for i in 0..N {
                table.enqueue(1, node_any(i)).unwrap();
            }
            drop(table);

            let mut table = FutexTable::<u64, u32>::new();
            for i in 0..N {
                table.enqueue(1, node_any(i)).unwrap();
            }
            let woken = table.wake(1, i32::MAX, FUTEX_BITSET_MATCH_ANY).unwrap();
            assert_eq!(woken.len(), N as usize);
            drop(woken);
        })
        .unwrap();
    handle.join().unwrap();
}
