use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::num::NonZeroU32;
use std::sync::Mutex;

use super::*;

fn nz(v: u32) -> NonZeroU32 {
    NonZeroU32::new(v).expect("test bitset must be non-zero")
}

fn node(waiter: u32, bitset: u32) -> Box<FutexNode<u64, u32>> {
    Box::new(FutexNode::new(waiter, nz(bitset)))
}

fn node_any(waiter: u32) -> Box<FutexNode<u64, u32>> {
    Box::new(FutexNode::new_match_any(waiter))
}

fn ids<K>(woken: Woken<K, u32>) -> Vec<u32> {
    woken.map(|n| n.into_waiter()).collect()
}

fn drain(table: &mut FutexTable<u64, u32>, key: u64) -> Vec<u32> {
    ids(table.wake(key, i32::MAX, FUTEX_BITSET_MATCH_ANY))
}

/// Fixed-capacity, non-allocating recorder for `on_requeue` callbacks in tests.
struct Moves<const N: usize> {
    buf: [(u32, u64); N],
    len: usize,
}

impl<const N: usize> Moves<N> {
    const fn new() -> Self {
        Self { buf: [(0, 0); N], len: 0 }
    }

    fn record(&mut self, waiter: &u32, key: u64) {
        self.buf[self.len] = (*waiter, key);
        self.len += 1;
    }

    fn as_slice(&self) -> &[(u32, u64)] {
        &self.buf[..self.len]
    }
}

#[test]
fn check_requeue_counts_rejects_negative() {
    assert_eq!(check_requeue_counts(0, 0), Ok(()));
    assert_eq!(check_requeue_counts(1, 0), Ok(()));
    assert_eq!(check_requeue_counts(0, 1), Ok(()));
    assert_eq!(check_requeue_counts(i32::MAX, i32::MAX), Ok(()));

    assert_eq!(check_requeue_counts(-1, 0), Err(FutexError::NegativeCount));
    assert_eq!(check_requeue_counts(0, -1), Err(FutexError::NegativeCount));
    assert_eq!(check_requeue_counts(-1, -1), Err(FutexError::NegativeCount));
    assert_eq!(check_requeue_counts(i32::MIN, 0), Err(FutexError::NegativeCount));
    assert_eq!(check_requeue_counts(0, i32::MIN), Err(FutexError::NegativeCount));
}

#[test]
fn bitset_match_any_and_high_bit() {
    assert_eq!(FUTEX_BITSET_MATCH_ANY.get(), u32::MAX);
    assert_eq!(NonZeroU32::new(0), None);

    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(1, node(10, 1 << 0));
    table.enqueue(1, node(11, 1 << 31));
    table.enqueue(1, node_any(12));

    // Waking with only bit 31 set must match both the bit-31 waiter and the
    // `FUTEX_BITSET_MATCH_ANY` waiter, while leaving bit 0 queued.
    assert_eq!(ids(table.wake(1, i32::MAX, nz(1 << 31))), [11, 12]);
    assert_eq!(drain(&mut table, 1), [10]);
    assert!(table.is_empty());
}

#[test]
fn wake_fifo_order_and_key_isolation() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(1, node_any(10));
    table.enqueue(2, node_any(20));
    table.enqueue(1, node_any(11));
    table.enqueue(3, node_any(30));
    table.enqueue(1, node_any(12));
    table.enqueue(2, node_any(21));

    assert_eq!(table.len(), 6);
    assert_eq!(table.count_for_key(1), 3);
    assert_eq!(table.count_for_key(2), 2);
    assert_eq!(table.count_for_key(3), 1);

    assert_eq!(ids(table.wake(1, 2, FUTEX_BITSET_MATCH_ANY)), [10, 11]);
    assert_eq!(table.count_for_key(1), 1);
    assert_eq!(table.count_for_key(2), 2);

    assert_eq!(drain(&mut table, 1), [12]);
    assert_eq!(table.count_for_key(1), 0);
    assert_eq!(drain(&mut table, 2), [20, 21]);
    assert_eq!(drain(&mut table, 3), [30]);
    assert!(table.is_empty());
}

#[test]
fn woken_len_and_size_hint_track_partial_drain() {
    let mut table = FutexTable::<u64, u32>::new();
    let expected = [10u32, 20, 30];
    for id in expected {
        table.enqueue(1, node_any(id));
    }

    let mut woken = table.wake(1, i32::MAX, FUTEX_BITSET_MATCH_ANY);
    for (idx, &id) in expected.iter().enumerate() {
        let rem = expected.len() - idx;
        assert!(!woken.is_empty());
        assert_eq!(woken.len(), rem);
        assert_eq!(ExactSizeIterator::len(&woken), rem);
        assert_eq!(woken.size_hint(), (rem, Some(rem)));
        assert_eq!(woken.next().unwrap().into_waiter(), id);
    }

    // Exhausted and fused: subsequent `next()` returns `None` and keeps `len == 0`.
    for _ in 0..2 {
        assert!(woken.is_empty());
        assert_eq!(woken.len(), 0);
        assert_eq!(ExactSizeIterator::len(&woken), 0);
        assert_eq!(woken.size_hint(), (0, Some(0)));
        assert!(woken.next().is_none());
    }
}

#[test]
fn reenqueued_woken_node_is_found_at_new_key() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(1, node(42, 0b0101));

    let mut woken = table.wake(1, 1, nz(0b0001));
    let reused = woken.next().unwrap();
    assert_eq!(reused.key(), Some(1));
    assert_eq!(reused.bitset(), nz(0b0101));
    assert!(woken.next().is_none());

    // Re-enqueue the returned node under a different key; `wake` must find it
    // only under the new key with `key() == Some(2)`.
    table.enqueue(2, reused);
    assert!(table.wake(1, 10, FUTEX_BITSET_MATCH_ANY).is_empty());
    let mut woken2 = table.wake(2, 10, nz(0b0100));
    let n = woken2.next().unwrap();
    assert_eq!(n.key(), Some(2));
    assert_eq!(n.into_waiter(), 42);
    assert!(woken2.next().is_none());
    assert!(table.is_empty());
}

#[test]
fn wake_bitset_selection() {
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(7, node(1, 0b0001));
    table.enqueue(7, node(2, 0b0010));
    table.enqueue(7, node(3, 0b0101));
    table.enqueue(7, node(4, 0b1000));
    table.enqueue(7, node(5, 0b0011));

    // Mask `0b0010` matches waiters 2 and 5; asking for 1 skips waiter 1 and
    // takes waiter 2, leaving waiter 5.
    assert_eq!(ids(table.wake(7, 1, nz(0b0010))), [2]);

    // Mask `0b0100` matches only waiter 3.
    assert_eq!(ids(table.wake(7, 10, nz(0b0100))), [3]);

    // Mask `0b0001` matches remaining waiters 1 and 5 in FIFO order.
    assert_eq!(ids(table.wake(7, 10, nz(0b0001))), [1, 5]);

    // Mask `0b0111` matches nothing left (only waiter 4 with `0b1000` remains).
    assert!(table.wake(7, 10, nz(0b0111)).is_empty());
    assert_eq!(table.len(), 1);

    assert_eq!(drain(&mut table, 7), [4]);
    assert!(table.is_empty());
}

#[test]
fn wake_nr_edge_cases_match_linux() {
    let mut table = FutexTable::<u64, u32>::new();

    // Empty table: nr == 0 and negative nr wake nothing.
    assert!(table.wake(1, 0, FUTEX_BITSET_MATCH_ANY).is_empty());
    assert!(table.wake(1, -1, FUTEX_BITSET_MATCH_ANY).is_empty());
    assert!(table.wake(1, i32::MIN, FUTEX_BITSET_MATCH_ANY).is_empty());

    for id in 1..=6 {
        table.enqueue(1, node(id, if id == 1 { 0b0010 } else { 0b0001 }));
    }

    // Non-matching bitset with nr == 0 / negative still wakes 0.
    assert!(table.wake(1, 0, nz(0b1000)).is_empty());
    assert!(table.wake(1, -5, nz(0b1000)).is_empty());
    assert_eq!(table.len(), 6);

    // Linux `futex_wake` checks `if (++ret >= nr_wake) break;` *after* waking
    // a matching waiter, so `nr == 0` wakes 1 matching waiter (skipping
    // non-matching waiter 1 to wake waiter 2).
    assert_eq!(ids(table.wake(1, 0, nz(0b0001))), [2]);

    // `nr == -1` also wakes 1 matching waiter (waiter 3).
    assert_eq!(ids(table.wake(1, -1, nz(0b0001))), [3]);

    // `nr == i32::MIN` also wakes 1 matching waiter (waiter 4).
    assert_eq!(ids(table.wake(1, i32::MIN, nz(0b0001))), [4]);

    // `nr == i32::MAX` wakes all remaining matching waiters in FIFO order.
    assert_eq!(drain(&mut table, 1), [1, 5, 6]);
    assert!(table.is_empty());
}

#[test]
fn requeue_keeps_order_and_preserves_bitset() {
    // 1. Pin exact relative order of moved waiters appended to `to`'s tail
    //    (catches any reversal of the moved chain).
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(10, node_any(1));
    table.enqueue(20, node_any(100));
    table.enqueue(10, node_any(2));
    table.enqueue(30, node_any(200));
    table.enqueue(10, node_any(3));
    table.enqueue(20, node_any(101));
    table.enqueue(10, node_any(4));

    let mut moves = Moves::<2>::new();
    let (woken, moved) = table.requeue(10, 20, 1, 2, |w, k| moves.record(w, k)).unwrap();
    assert_eq!(moved, 2);
    assert_eq!(moves.as_slice(), [(2, 20), (3, 20)]);

    let woken_nodes: Vec<_> = woken.collect();
    assert_eq!(woken_nodes.len(), 1);
    assert_eq!(woken_nodes[0].key(), Some(10));
    assert_eq!(*woken_nodes[0].waiter(), 1);

    assert_eq!(table.count_for_key(10), 1);
    assert_eq!(table.count_for_key(20), 4);
    assert_eq!(table.count_for_key(30), 1);

    // Moved waiters [2, 3] must follow existing waiters [100, 101] in order.
    let key20_nodes: Vec<_> = table.wake(20, i32::MAX, FUTEX_BITSET_MATCH_ANY).collect();
    assert!(key20_nodes.iter().all(|n| n.key() == Some(20)));
    let key20_ids: Vec<u32> = key20_nodes.into_iter().map(|n| n.into_waiter()).collect();
    assert_eq!(key20_ids, [100, 101, 2, 3]);
    assert_eq!(drain(&mut table, 10), [4]);
    assert_eq!(drain(&mut table, 30), [200]);
    assert!(table.is_empty());

    // 2. Verify original bitsets survive requeue intact.
    table.enqueue(10, node(1, 0b0010));
    table.enqueue(10, node(2, 0b0100));
    let (woken, moved) = table.requeue(10, 20, 0, 2, |_, _| {}).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);
    assert_eq!(ids(table.wake(20, 10, nz(0b0100))), [2]);
    assert_eq!(ids(table.wake(20, 10, nz(0b0010))), [1]);
    assert!(table.is_empty());
}

#[test]
fn requeue_nr_edge_cases_match_linux() {
    let mut table = FutexTable::<u64, u32>::new();
    for id in 1..=4 {
        table.enqueue(1, node_any(id));
    }

    // Negative counts return `NegativeCount` (`-EINVAL`) without mutating state.
    assert_eq!(table.requeue(1, 2, -1, 0, |_, _| {}).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.requeue(1, 2, 0, -1, |_, _| {}).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.requeue(1, 2, i32::MIN, i32::MIN, |_, _| {}).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(table.len(), 4);

    // `(0, 0)` wakes 0 and moves 0 (unlike `wake` with `nr == 0`).
    let (woken, moved) = table.requeue(1, 2, 0, 0, |_, _| {}).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 4);
    assert_eq!(table.count_for_key(2), 0);

    // `(0, 2)` wakes 0 and moves 2.
    let (woken, moved) = table.requeue(1, 2, 0, 2, |_, _| {}).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);
    assert_eq!(table.count_for_key(1), 2);
    assert_eq!(table.count_for_key(2), 2);

    // `(1, 0)` wakes 1 and moves 0.
    let (woken, moved) = table.requeue(1, 2, 1, 0, |_, _| {}).unwrap();
    assert_eq!(ids(woken), [3]);
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 1);

    // `nr_wake` past remaining count wakes what exists and moves 0.
    let (woken, moved) = table.requeue(1, 2, 5, 10, |_, _| {}).unwrap();
    assert_eq!(ids(woken), [4]);
    assert_eq!(moved, 0);
    assert_eq!(table.count_for_key(1), 0);

    // `from == to` with a bystander key (99) interleaved: wakes up to `nr_wake`,
    // keeps remaining FIFO order without rotating or double-counting, still
    // counts them in `moved` and invokes `on_requeue`, and leaves the bystander
    // untouched and unnotified.
    table.enqueue(99, node_any(900));
    table.enqueue(2, node_any(5));
    table.enqueue(99, node_any(901));
    table.enqueue(2, node_any(6));
    // Key 2 now has [1, 2, 5, 6]; key 99 has [900, 901].
    let mut moves = Moves::<4>::new();
    let (woken, moved) = table.requeue(2, 2, 1, 2, |w, k| moves.record(w, k)).unwrap();
    assert_eq!(ids(woken), [1]);
    assert_eq!(moved, 2);
    assert_eq!(moves.as_slice(), [(2, 2), (5, 2)]);

    // Remaining order on key 2 must stay [2, 5, 6]; bystander key 99 stays [900, 901].
    let (woken, moved) = table.requeue(2, 2, 0, 100, |_, _| {}).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 3);
    assert_eq!(drain(&mut table, 2), [2, 5, 6]);
    assert_eq!(drain(&mut table, 99), [900, 901]);
    assert!(table.is_empty());
}

#[test]
fn requeue_into_across_tables() {
    let mut src = FutexTable::<u64, u32>::new();
    let mut dst = FutexTable::<u64, u32>::new();

    src.enqueue(1, node_any(10));
    src.enqueue(1, node_any(11));
    src.enqueue(1, node_any(12));
    src.enqueue(1, node_any(13));
    src.enqueue(1, node_any(14));
    dst.enqueue(2, node_any(20));

    assert_eq!(src.requeue_into(1, &mut dst, 2, -1, 1, |_, _| {}).unwrap_err(), FutexError::NegativeCount);
    assert_eq!(src.requeue_into(1, &mut dst, 2, 1, -1, |_, _| {}).unwrap_err(), FutexError::NegativeCount);

    // `(0, 1)` wakes 0 and moves waiter 10 to `dst`'s tail.
    let mut moves = Moves::<4>::new();
    let (woken, moved) = src.requeue_into(1, &mut dst, 2, 0, 1, |w, k| moves.record(w, k)).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 1);
    assert_eq!(moves.as_slice(), [(10, 2)]);
    assert_eq!(src.len(), 4);
    assert_eq!(dst.len(), 2);

    // `(1, 2)` wakes waiter 11 and moves waiters 12, 13 to `dst`'s tail.
    let mut moves = Moves::<4>::new();
    let (woken, moved) = src.requeue_into(1, &mut dst, 2, 1, 2, |w, k| moves.record(w, k)).unwrap();
    assert_eq!(ids(woken), [11]);
    assert_eq!(moved, 2);
    assert_eq!(moves.as_slice(), [(12, 2), (13, 2)]);
    assert_eq!(src.len(), 1);
    assert_eq!(dst.len(), 4);

    // Waiter 12 is absent from `src` (`AlreadyWoken` here only means not in
    // `src`, not that waiter 12 was woken) and cancels cleanly in `dst`.
    assert!(matches!(src.cancel(|w| *w == 12), CancelOutcome::AlreadyWoken));
    let cancelled = dst.cancel(|w| *w == 12).into_node().unwrap();
    assert_eq!(cancelled.key(), Some(2));
    assert_eq!(cancelled.into_waiter(), 12);

    // `dst` has [20, 10, 13] in FIFO order; `src` still has [14].
    assert_eq!(drain(&mut dst, 2), [20, 10, 13]);
    assert_eq!(drain(&mut src, 1), [14]);
    assert!(src.is_empty());
    assert!(dst.is_empty());
}

#[test]
fn cancel_after_wake_reports_already_woken() {
    let mut table = FutexTable::<u64, u32>::new();
    assert!(matches!(table.cancel(|w| *w == 99), CancelOutcome::AlreadyWoken));

    table.enqueue(1, node_any(1));
    table.enqueue(1, node_any(2));
    table.enqueue(1, node_any(3));

    // Wake waiter 1.
    assert_eq!(ids(table.wake(1, 1, FUTEX_BITSET_MATCH_ANY)), [1]);

    // Cancelling waiter 1 after wake reports `AlreadyWoken`.
    assert!(matches!(table.cancel(|w| *w == 1), CancelOutcome::AlreadyWoken));

    // Cancelling waiter 2 while still queued returns `Cancelled` and removes it.
    let cancelled_node = match table.cancel(|w| *w == 2) {
        CancelOutcome::Cancelled(node) => node,
        CancelOutcome::AlreadyWoken => panic!("expected Cancelled"),
    };
    assert_eq!(cancelled_node.key(), Some(1));
    assert_eq!(cancelled_node.into_waiter(), 2);

    // Cancelling waiter 2 a second time reports `AlreadyWoken`.
    assert!(matches!(table.cancel(|w| *w == 2), CancelOutcome::AlreadyWoken));

    // Only waiter 3 remains.
    assert_eq!(drain(&mut table, 1), [3]);
    assert!(table.is_empty());
}

#[test]
fn cancel_after_requeue_without_key_and_across_buckets() {
    // 1. Same-bucket requeue: `cancel` takes no key and finds the requeued
    //    waiter regardless of its new key, returning `Cancelled` with the new
    //    key (never a false `AlreadyWoken`).
    let mut table = FutexTable::<u64, u32>::new();
    table.enqueue(10, node_any(1));
    table.enqueue(10, node_any(2));
    table.enqueue(10, node_any(3));

    let (woken, moved) = table.requeue(10, 20, 0, 2, |_, _| {}).unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);
    assert_eq!(table.count_for_key(20), 2);
    assert_eq!(table.count_for_key(10), 1);

    let cancelled = table.cancel(|w| *w == 1).into_node().unwrap();
    assert_eq!(cancelled.key(), Some(20));
    assert_eq!(cancelled.into_waiter(), 1);
    assert!(matches!(table.cancel(|w| *w == 1), CancelOutcome::AlreadyWoken));

    assert_eq!(drain(&mut table, 20), [2]);
    assert_eq!(drain(&mut table, 10), [3]);
    assert!(table.is_empty());

    // 2. Single-threaded multi-bucket requeue (no recheck race; the race is
    //    tested with `std::sync::Mutex` in `bucket_index_cancel_in_retries_across_mutex_move`).
    const BUCKETS: usize = 2;
    let bucket_of = |key: u64| (key as usize) % BUCKETS;
    let buckets: [RefCell<FutexTable<u64, u32>>; BUCKETS] = core::array::from_fn(|_| RefCell::new(FutexTable::new()));
    let waiter_bucket: [BucketIndex; 3] = core::array::from_fn(|_| BucketIndex::new(usize::MAX));

    for id in 0..3u32 {
        let key = 10u64; // bucket 0
        waiter_bucket[id as usize].set(bucket_of(key));
        buckets[bucket_of(key)].borrow_mut().enqueue(key, node_any(id));
    }

    // Requeue waiters 0 and 1 from key 10 (bucket 0) to key 11 (bucket 1).
    let (woken, moved) = buckets[0]
        .borrow_mut()
        .requeue_into(10, &mut buckets[1].borrow_mut(), 11, 0, 2, |w, new_key| {
            waiter_bucket[*w as usize].set(bucket_of(new_key));
        })
        .unwrap();
    assert!(woken.is_empty());
    assert_eq!(moved, 2);

    // Waiter 0 cancels via `BucketIndex::cancel_in` -> finds bucket 1 and key 11.
    assert_eq!(waiter_bucket[0].get(), 1);
    let cancelled = waiter_bucket[0].cancel_in(|b| buckets[b].borrow_mut(), |w| *w == 0).into_node().unwrap();
    assert_eq!(cancelled.key(), Some(11));
    assert_eq!(cancelled.into_waiter(), 0);

    // Waiter 1 is absent from `buckets[0]` (`AlreadyWoken` here means only
    // "not in bucket 0", not that waiter 1 was woken).
    assert!(matches!(buckets[0].borrow_mut().cancel(|w| *w == 1), CancelOutcome::AlreadyWoken));
    assert_eq!(drain(&mut buckets[1].borrow_mut(), 11), [1]);
    assert_eq!(drain(&mut buckets[0].borrow_mut(), 10), [2]);
}

#[test]
fn bucket_index_cancel_in_retries_across_mutex_move() {
    #[derive(Debug)]
    struct Waiter {
        id: u32,
        bucket: BucketIndex,
    }

    let buckets: [Mutex<FutexTable<u64, Arc<Waiter>>>; 2] = core::array::from_fn(|_| Mutex::new(FutexTable::new()));

    // 1. Deterministic race: move waiter from bucket 0 -> bucket 1 *between*
    //    `cancel_in`'s pre-lock load of `0` and its lock acquisition of bucket 0.
    let w0 = Arc::new(Waiter { id: 7, bucket: BucketIndex::new(0) });
    buckets[0].lock().unwrap().enqueue(10, Box::new(FutexNode::new_match_any(Arc::clone(&w0))));

    let mut lock_calls = Vec::new();
    let mut moved_once = false;
    let outcome = w0.bucket.cancel_in(
        |b| {
            lock_calls.push(b);
            if b == 0 && !moved_once {
                moved_once = true;
                // Simulate a concurrent `requeue_into` locking buckets 0 and 1
                // in ascending order before `cancel_in` gets bucket 0's lock.
                let mut g0 = buckets[0].lock().unwrap();
                let mut g1 = buckets[1].lock().unwrap();
                let (woken, moved) = g0.requeue_into(10, &mut g1, 11, 0, 1, |w, _| w.bucket.set(1)).unwrap();
                assert!(woken.is_empty());
                assert_eq!(moved, 1);
            }
            buckets[b].lock().unwrap()
        },
        |w| Arc::ptr_eq(w, &w0),
    );

    // `cancel_in` must have tried bucket 0, detected the move, retried bucket 1,
    // cancelled the waiter at key 11, and dropped both mutex guards before returning.
    assert_eq!(lock_calls, [0, 1]);
    assert!(buckets[0].try_lock().is_ok());
    assert!(buckets[1].try_lock().is_ok());
    let cancelled = outcome.into_node().unwrap();
    assert_eq!(cancelled.key(), Some(11));
    assert_eq!(cancelled.waiter().id, 7);

    // 2. Concurrent wake before `cancel_in` -> `AlreadyWoken`, locks released.
    let w1 = Arc::new(Waiter { id: 8, bucket: BucketIndex::new(1) });
    buckets[1].lock().unwrap().enqueue(11, Box::new(FutexNode::new_match_any(Arc::clone(&w1))));
    let woken = {
        let mut g1 = buckets[1].lock().unwrap();
        g1.wake(11, 1, FUTEX_BITSET_MATCH_ANY)
    };
    assert_eq!(woken.count(), 1);
    let outcome = w1.bucket.cancel_in(|b| buckets[b].lock().unwrap(), |w| Arc::ptr_eq(w, &w1));
    assert!(matches!(outcome, CancelOutcome::AlreadyWoken));
    assert!(buckets[1].try_lock().is_ok());

    // 3. Multi-threaded stress: one thread repeatedly requeues waiters between
    //    two `Mutex` buckets (and wakes some) while another cancels via `cancel_in`.
    //    Every waiter must be either woken or cancelled exactly once.
    const COUNT: usize = 64;
    let shared_buckets = Arc::new(buckets);
    let waiters: Vec<Arc<Waiter>> = (0..COUNT as u32)
        .map(|id| {
            let b = (id as usize) % 2;
            let key = if b == 0 { 10 } else { 11 };
            let w = Arc::new(Waiter { id, bucket: BucketIndex::new(b) });
            shared_buckets[b].lock().unwrap().enqueue(key, Box::new(FutexNode::new_match_any(Arc::clone(&w))));
            w
        })
        .collect();

    let requeuer = {
        let buckets = Arc::clone(&shared_buckets);
        std::thread::spawn(move || {
            let mut woken_ids = Vec::new();
            for round in 0..200u32 {
                let nr_wake = if round % 7 == 0 { 1 } else { 0 };
                let (from_b, to_b, from_k, to_k) = if round % 2 == 0 { (0, 1, 10, 11) } else { (1, 0, 11, 10) };
                let woken = {
                    // Always acquire bucket locks in ascending index order (0 then 1).
                    let mut g0 = buckets[0].lock().unwrap();
                    let mut g1 = buckets[1].lock().unwrap();
                    let (src, dst) = if from_b == 0 { (&mut *g0, &mut *g1) } else { (&mut *g1, &mut *g0) };
                    let (woken, _) =
                        src.requeue_into(from_k, dst, to_k, nr_wake, 4, |w, _| w.bucket.set(to_b)).unwrap();
                    woken
                };
                for node in woken {
                    woken_ids.push(node.waiter().id);
                }
            }
            woken_ids
        })
    };

    let mut cancelled_ids = Vec::new();
    for w in &waiters {
        if let CancelOutcome::Cancelled(node) =
            w.bucket.cancel_in(|b| shared_buckets[b].lock().unwrap(), |cand| Arc::ptr_eq(cand, w))
        {
            cancelled_ids.push(node.waiter().id);
        }
    }

    let mut all_ids = requeuer.join().unwrap();
    all_ids.extend(cancelled_ids);
    all_ids.sort_unstable();
    let expected_ids: Vec<u32> = (0..COUNT as u32).collect();
    assert_eq!(all_ids, expected_ids);
    assert!(shared_buckets[0].lock().unwrap().is_empty());
    assert!(shared_buckets[1].lock().unwrap().is_empty());
}

#[test]
fn debug_format_exact_short_and_truncated() {
    let mut table = FutexTable::<u64, u32>::new();
    assert_eq!(format!("{table:?}"), "FutexTable { len: 0, keys: [] }");

    let unqueued = FutexNode::<u64, u32>::new(42, nz(3));
    assert_eq!(format!("{unqueued:?}"), "FutexNode { key: None, bitset: 3, waiter: 42 }");

    table.enqueue(1, node_any(10));
    table.enqueue(2, node_any(20));
    assert_eq!(format!("{table:?}"), "FutexTable { len: 2, keys: [1, 2] }");

    for k in 3..=8 {
        table.enqueue(k, node_any(k as u32 * 10));
    }
    assert_eq!(format!("{table:?}"), "FutexTable { len: 8, keys: [1, 2, 3, 4, 5, 6, 7, 8] }");

    table.enqueue(9, node_any(90));
    assert_eq!(format!("{table:?}"), "FutexTable { len: 9, keys: [1, 2, 3, 4, 5, 6, 7, 8, ..] }");

    // Same exact-string checks on `Woken` for 0, 2, 8, and 9 nodes.
    let mut w_table = FutexTable::<u64, u32>::new();
    let w0 = w_table.wake(1, 10, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(format!("{w0:?}"), "Woken { len: 0, keys: [] }");
    assert_eq!(w0.count(), 0);

    for (count, expected_fmt) in [
        (2, "Woken { len: 2, keys: [1, 1] }"),
        (8, "Woken { len: 8, keys: [1, 1, 1, 1, 1, 1, 1, 1] }"),
        (9, "Woken { len: 9, keys: [1, 1, 1, 1, 1, 1, 1, 1, ..] }"),
    ] {
        for i in 0..count {
            w_table.enqueue(1, node_any(i));
        }
        let woken = w_table.wake(1, i32::MAX, FUTEX_BITSET_MATCH_ANY);
        assert_eq!(format!("{woken:?}"), expected_fmt);
        assert_eq!(woken.count(), count as usize);
    }
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
    entries: Vec<(u64, NonZeroU32, u32)>, // (key, bitset, waiter_id)
}

impl VecModel {
    fn enqueue(&mut self, key: u64, waiter: u32, bitset: NonZeroU32) {
        self.entries.push((key, bitset, waiter));
    }

    fn wake(&mut self, key: u64, nr: i32, bitset: NonZeroU32) -> Vec<u32> {
        let limit = nr.max(1) as usize;
        let mask = bitset.get();
        let mut woken = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if woken.len() < limit && entry.0 == key && (entry.1.get() & mask) != 0 {
                woken.push(entry.2);
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
        woken
    }

    fn requeue(&mut self, from: u64, to: u64, nr_wake: i32, nr_requeue: i32) -> Result<(Vec<u32>, usize), FutexError> {
        check_requeue_counts(nr_wake, nr_requeue)?;
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

    fn cancel(&mut self, waiter: u32) -> Option<(u64, u32)> {
        let pos = self.entries.iter().position(|&(_, _, w)| w == waiter)?;
        let (k, _, w) = self.entries.remove(pos);
        Some((k, w))
    }

    fn count_for_key(&self, key: u64) -> usize {
        self.entries.iter().filter(|&&(k, _, _)| k == key).count()
    }
}

fn random_bitset(rng: &mut Rng) -> NonZeroU32 {
    let raw = match rng.below(9) {
        0 => u32::MAX,
        1 => 1u32 << 31,
        _ => (1u32 << rng.below(4)) | (if rng.below(2) == 0 { 1u32 << rng.below(4) } else { 0 }),
    };
    nz(raw)
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

fn run_model(seed: u64, num_keys: usize, num_buckets: usize, steps: usize) {
    let bucket_of = |key: u64| (key as usize) % num_buckets;

    let mut rng = Rng(seed);
    let buckets: Vec<RefCell<FutexTable<u64, u32>>> =
        (0..num_buckets).map(|_| RefCell::new(FutexTable::new())).collect();
    let mut model = VecModel::default();
    // Pre-allocated atomic bucket index per waiter ID (zero allocation in `on_requeue`).
    let waiter_bucket: Vec<BucketIndex> = (0..steps).map(|_| BucketIndex::new(usize::MAX)).collect();
    let mut next_waiter: u32 = 0;

    for step in 0..steps {
        match rng.below(10) {
            // Enqueue (40%)
            0..=3 => {
                let key = rng.below(num_keys) as u64;
                let bitset = random_bitset(&mut rng);
                let id = next_waiter;
                next_waiter += 1;

                let b = bucket_of(key);
                waiter_bucket[id as usize].set(b);
                buckets[b].borrow_mut().enqueue(key, Box::new(FutexNode::new(id, bitset)));
                model.enqueue(key, id, bitset);
            }
            // Wake (20%)
            4..=5 => {
                let key = rng.below(num_keys) as u64;
                let nr = random_nr(&mut rng);
                let bitset = random_bitset(&mut rng);

                let woken = buckets[bucket_of(key)].borrow_mut().wake(key, nr, bitset);
                let actual = ids(woken);
                let expected = model.wake(key, nr, bitset);
                assert_eq!(actual, expected, "seed {seed:#x} buckets {num_buckets} step {step} wake({key}, {nr})");
            }
            // Requeue (20%)
            6..=7 => {
                let from = rng.below(num_keys) as u64;
                let to = rng.below(num_keys) as u64;
                let nr_wake = random_nr(&mut rng);
                let nr_requeue = random_nr(&mut rng);
                let b1 = bucket_of(from);
                let b2 = bucket_of(to);

                let mut cb_count = 0usize;
                let mut on_requeue = |w: &u32, new_key: u64| {
                    assert_eq!(new_key, to);
                    waiter_bucket[*w as usize].set(bucket_of(new_key));
                    cb_count += 1;
                };
                let actual = if b1 == b2 {
                    buckets[b1].borrow_mut().requeue(from, to, nr_wake, nr_requeue, &mut on_requeue)
                } else {
                    let mut src = buckets[b1].borrow_mut();
                    let mut dst = buckets[b2].borrow_mut();
                    src.requeue_into(from, &mut dst, to, nr_wake, nr_requeue, &mut on_requeue)
                }
                .map(|(w, m)| (ids(w), m));
                let expected = model.requeue(from, to, nr_wake, nr_requeue);
                assert_eq!(
                    actual, expected,
                    "seed {seed:#x} buckets {num_buckets} step {step} requeue({from}->{to}, {nr_wake}, {nr_requeue})"
                );
                if let Ok((_, moved)) = actual {
                    assert_eq!(cb_count, moved);
                }
            }
            // Cancel (20%): exercises both active and already-woken/already-cancelled waiters.
            _ => {
                if next_waiter == 0 {
                    continue;
                }
                let id = rng.below(next_waiter as usize) as u32;
                let actual = waiter_bucket[id as usize]
                    .cancel_in(|b| buckets[b].borrow_mut(), |w| *w == id)
                    .into_node()
                    .map(|n| (n.key().unwrap(), n.into_waiter()));
                let expected = model.cancel(id);
                assert_eq!(actual, expected, "seed {seed:#x} buckets {num_buckets} step {step} cancel({id})");
            }
        }

        let total_len: usize = buckets.iter().map(|b| b.borrow().len()).sum();
        assert_eq!(total_len, model.entries.len(), "seed {seed:#x} buckets {num_buckets} step {step} len");
        for k in 0..num_keys as u64 {
            assert_eq!(
                buckets[bucket_of(k)].borrow().count_for_key(k),
                model.count_for_key(k),
                "seed {seed:#x} buckets {num_buckets} step {step} count_for_key({k})"
            );
        }
    }
}

#[test]
fn randomized_model_matches_vec_oracle() {
    run_model(0xdead_beef_cafe_0001, 4, 1, 5_000);
    run_model(0x1234_5678_9abc_def0, 12, 1, 5_000);
    run_model(0x51a7_cafe_babe_2026, 8, 4, 5_000);
}

#[test]
fn drop_and_debug_deep_lists_without_stack_overflow() {
    // Run on a 256 KiB stack so any recursive `Drop` or `Debug` over 10,000
    // nodes would overflow immediately, proving both use O(1) stack.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            const N: u32 = 10_000;

            let mut table = FutexTable::<u64, u32>::new();
            for i in 0..N {
                table.enqueue(1, node_any(i));
            }
            let table_dbg = format!("{table:?}");
            assert_eq!(table_dbg, "FutexTable { len: 10000, keys: [1, 1, 1, 1, 1, 1, 1, 1, ..] }");
            drop(table);

            let mut table = FutexTable::<u64, u32>::new();
            for i in 0..N {
                table.enqueue(1, node_any(i));
            }
            let woken = table.wake(1, i32::MAX, FUTEX_BITSET_MATCH_ANY);
            assert_eq!(woken.len(), N as usize);
            let woken_dbg = format!("{woken:?}");
            assert_eq!(woken_dbg, "Woken { len: 10000, keys: [1, 1, 1, 1, 1, 1, 1, 1, ..] }");
            assert_eq!(woken.count(), N as usize);
        })
        .unwrap();
    handle.join().unwrap();
}
