//! Zero-allocation and zero-deallocation proof for [`lumen_futex::FutexTable`].
//!
//! Runs in its own integration-test binary with a single `#[test]` so no other
//! test thread can allocate concurrently while the counting `#[global_allocator]`
//! observes heap traffic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_futex::{CancelOutcome, FUTEX_BITSET_MATCH_ANY, FutexError, FutexNode, FutexTable};

struct Counting;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards directly to `System`, upholding `GlobalAlloc`'s
// contract; the atomics only count calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: caller's `GlobalAlloc` contract forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: caller's `GlobalAlloc` contract forwarded to `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn nz(v: u32) -> NonZeroU32 {
    NonZeroU32::new(v).unwrap()
}

fn assert_zero_heap_traffic<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let allocs_before = ALLOC_COUNT.load(Ordering::SeqCst);
    let deallocs_before = DEALLOC_COUNT.load(Ordering::SeqCst);
    let result = f();
    let allocs_after = ALLOC_COUNT.load(Ordering::SeqCst);
    let deallocs_after = DEALLOC_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        allocs_after - allocs_before,
        0,
        "{label} performed {} heap allocation(s)",
        allocs_after - allocs_before
    );
    assert_eq!(
        deallocs_after - deallocs_before,
        0,
        "{label} performed {} heap deallocation(s)",
        deallocs_after - deallocs_before
    );
    result
}

#[test]
fn enqueue_wake_requeue_cancel_perform_zero_allocations() {
    // Negative control: verify the counting allocator observes a real Box alloc/dealloc.
    let a0 = ALLOC_COUNT.load(Ordering::SeqCst);
    let d0 = DEALLOC_COUNT.load(Ordering::SeqCst);
    let probe = std::hint::black_box(Box::new([0u64; 8]));
    assert!(ALLOC_COUNT.load(Ordering::SeqCst) > a0, "counting allocator must observe alloc");
    drop(probe);
    assert!(DEALLOC_COUNT.load(Ordering::SeqCst) > d0, "counting allocator must observe dealloc");

    // Pre-allocate all nodes and per-waiter bucket mirrors outside the measurement window.
    // Assign bitsets by `(i / 4) % 4` so each of the 4 keys receives a mix of all 4 bits.
    let mut pool: Vec<Box<FutexNode<u64, u32>>> =
        (0..64u32).map(|i| Box::new(FutexNode::new(i, nz(1u32 << ((i / 4) % 4))))).collect();
    let mut cancel_node = Some(Box::new(FutexNode::new(64, FUTEX_BITSET_MATCH_ANY)));
    let waiter_bucket: [AtomicUsize; 65] = std::array::from_fn(|_| AtomicUsize::new(0));
    let mut table = FutexTable::<u64, u32>::new();
    let mut other_bucket = FutexTable::<u64, u32>::new();

    // 1. `enqueue` allocates and frees nothing.
    assert_zero_heap_traffic("enqueue(32 nodes across 4 keys)", || {
        for i in 0..32u64 {
            let n = pool.pop().unwrap();
            table.enqueue(i % 4, n);
        }
    });
    assert_eq!(table.len(), 32);

    // 2. `wake` (matching bitset subset, nr == 0, negative nr) allocates and frees nothing.
    // Key 0 has 8 nodes with bitsets spanning 0b0001..=0b1000 (4 match 0b0011).
    assert_zero_heap_traffic("wake(bitset subset)", || {
        let woken = table.wake(0, 3, nz(0b0011));
        assert_eq!(woken.len(), 3);
        pool.extend(woken);
    });

    assert_zero_heap_traffic("wake(nr == 0 and nr < 0)", || {
        let w0 = table.wake(1, 0, FUTEX_BITSET_MATCH_ANY);
        assert_eq!(w0.len(), 1);
        pool.extend(w0);

        let wneg = table.wake(1, -1, FUTEX_BITSET_MATCH_ANY);
        assert_eq!(wneg.len(), 1);
        pool.extend(wneg);
    });

    // 3. `requeue` and `requeue_into` allocate and free nothing.
    assert_zero_heap_traffic("requeue(negative count)", || {
        assert_eq!(table.requeue(2, 3, -1, 1, |_, _| {}).unwrap_err(), FutexError::NegativeCount);
    });

    assert_zero_heap_traffic("requeue(same table, from != to)", || {
        let (woken, moved) = table
            .requeue(2, 3, 2, 4, |w, _| {
                waiter_bucket[*w as usize].store(0, Ordering::Release);
            })
            .unwrap();
        assert_eq!(woken.len(), 2);
        assert_eq!(moved, 4);
        pool.extend(woken);
    });

    assert_zero_heap_traffic("requeue(same table, from == to)", || {
        let (woken, moved) = table.requeue(3, 3, 1, 3, |_, _| {}).unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(moved, 3);
        pool.extend(woken);
    });

    assert_zero_heap_traffic("requeue_into(across buckets)", || {
        let (woken, moved) = table
            .requeue_into(3, &mut other_bucket, 10, 1, 5, |w, _| {
                waiter_bucket[*w as usize].store(1, Ordering::Release);
            })
            .unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(moved, 5);
        pool.extend(woken);
    });

    // 4. `cancel` (both `Cancelled` and `AlreadyWoken`) allocates and frees nothing.
    assert_zero_heap_traffic("cancel(Cancelled and AlreadyWoken)", || {
        let n = cancel_node.take().unwrap();
        other_bucket.enqueue(10, n);

        match other_bucket.cancel(|w| *w == 64) {
            CancelOutcome::Cancelled(returned) => {
                assert_eq!(*returned.waiter(), 64);
                assert_eq!(returned.key(), Some(10));
                cancel_node = Some(returned);
            }
            CancelOutcome::AlreadyWoken => panic!("expected Cancelled"),
        }
        assert!(other_bucket.cancel(|w| *w == 64).is_already_woken());
    });

    // 5. Sustained interleaved churn recycling the pre-allocated node pool: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("interleaved churn (200 rounds)", || {
        for round in 0..200u32 {
            let k1 = (round % 4) as u64;
            let k2 = ((round + 1) % 4) as u64;
            if let Some(n) = pool.pop() {
                table.enqueue(k1, n);
            }
            if round % 3 == 0 {
                let (woken, _) = table.requeue(k1, k2, 1, 2, |_, _| {}).unwrap();
                pool.extend(woken);
            }
            if round % 5 == 0
                && let CancelOutcome::Cancelled(n) = table.cancel(|w| *w == round % 64)
            {
                pool.push(n);
            }
            if round % 2 == 0 {
                let woken = table.wake(k2, 2, FUTEX_BITSET_MATCH_ANY);
                pool.extend(woken);
            }
        }
    });

    // Drop everything outside the zero-heap measurement window.
    drop(cancel_node);
    drop(table);
    drop(other_bucket);
    drop(pool);
}
