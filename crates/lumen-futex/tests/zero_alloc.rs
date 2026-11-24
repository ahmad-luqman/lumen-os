//! Zero-allocation and zero-deallocation proof for [`lumen_futex::FutexTable`].
//!
//! Runs in its own integration-test binary with a single `#[test]` so no other
//! test thread can allocate concurrently while the counting `#[global_allocator]`
//! observes heap traffic.

use std::alloc::{GlobalAlloc, Layout, System};
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

    // Pre-allocate nodes and a holding Vec outside the lock / measurement window.
    let mut pool: Vec<Box<FutexNode<u64, u32>>> =
        (0..64u32).map(|i| Box::new(FutexNode::new(i, 1u32 << (i % 4)))).collect();
    let mut zero_bitset_node = Box::new(FutexNode::new(999, 0));
    let mut table = FutexTable::<u64, u32>::new();
    let mut other_bucket = FutexTable::<u64, u32>::new();

    // 1. `enqueue` (both normal and refused zero-bitset) allocates and frees nothing.
    zero_bitset_node = assert_zero_heap_traffic("enqueue(zero bitset)", || {
        let err = table.enqueue(1, zero_bitset_node).unwrap_err();
        assert_eq!(err.kind, FutexError::ZeroBitset);
        err.node
    });

    assert_zero_heap_traffic("enqueue(32 nodes across 4 keys)", || {
        for i in 0..32u64 {
            let n = pool.pop().unwrap();
            table.enqueue(i % 4, n).unwrap();
        }
    });
    assert_eq!(table.len(), 32);

    // 2. `wake` (matching subset, nr == 0, negative nr, zero-bitset error) allocates and frees nothing.
    assert_zero_heap_traffic("wake(zero bitset)", || {
        assert_eq!(table.wake(0, 1, 0).unwrap_err(), FutexError::ZeroBitset);
    });

    assert_zero_heap_traffic("wake(bitset subset)", || {
        let mut woken = table.wake(0, 3, 0b0011).unwrap();
        while let Some(n) = woken.pop() {
            pool.push(n);
        }
    });

    assert_zero_heap_traffic("wake(nr == 0 and nr < 0)", || {
        let mut w0 = table.wake(1, 0, FUTEX_BITSET_MATCH_ANY).unwrap();
        assert_eq!(w0.len(), 1);
        while let Some(n) = w0.pop() {
            pool.push(n);
        }
        let mut wneg = table.wake(1, -1, FUTEX_BITSET_MATCH_ANY).unwrap();
        assert_eq!(wneg.len(), 1);
        while let Some(n) = wneg.pop() {
            pool.push(n);
        }
    });

    // 3. `requeue` and `requeue_into` allocate and free nothing.
    assert_zero_heap_traffic("requeue(negative count)", || {
        assert_eq!(table.requeue(2, 3, -1, 1).unwrap_err(), FutexError::NegativeCount);
    });

    assert_zero_heap_traffic("requeue(same table, from != to)", || {
        let (mut woken, moved) = table.requeue(2, 3, 2, 4).unwrap();
        assert_eq!(woken.len(), 2);
        assert_eq!(moved, 4);
        while let Some(n) = woken.pop() {
            pool.push(n);
        }
    });

    assert_zero_heap_traffic("requeue(same table, from == to)", || {
        let (mut woken, moved) = table.requeue(3, 3, 1, 3).unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(moved, 3);
        while let Some(n) = woken.pop() {
            pool.push(n);
        }
    });

    assert_zero_heap_traffic("requeue_into(across buckets)", || {
        let (mut woken, moved) = table.requeue_into(3, &mut other_bucket, 10, 1, 5).unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(moved, 5);
        while let Some(n) = woken.pop() {
            pool.push(n);
        }
    });

    // 4. `cancel` (both `Cancelled` and `AlreadyWoken`) allocates and frees nothing.
    let target_waiter = assert_zero_heap_traffic("cancel(Cancelled and AlreadyWoken)", || {
        // Enqueue a fresh known waiter on key 10 in `other_bucket`, cancel it,
        // then cancel again to hit `AlreadyWoken`.
        let mut n = pool.pop().unwrap();
        n.reset(777, FUTEX_BITSET_MATCH_ANY);
        other_bucket.enqueue(10, n).unwrap();

        match other_bucket.cancel(10, &777) {
            CancelOutcome::Cancelled(returned) => {
                assert_eq!(*returned.waiter(), 777);
                pool.push(returned);
            }
            CancelOutcome::AlreadyWoken => panic!("expected Cancelled"),
        }
        assert!(other_bucket.cancel(10, &777).is_already_woken());
        777u32
    });
    assert_eq!(target_waiter, 777);

    // 5. Sustained interleaved churn recycling the pre-allocated node pool: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("interleaved churn (200 rounds)", || {
        for round in 0..200u32 {
            let k1 = (round % 4) as u64;
            let k2 = ((round + 1) % 4) as u64;
            if let Some(mut n) = pool.pop() {
                n.reset(round, 1u32 << (round % 4));
                table.enqueue(k1, n).unwrap();
            }
            if round % 3 == 0 {
                let (mut woken, _) = table.requeue(k1, k2, 1, 2).unwrap();
                while let Some(n) = woken.pop() {
                    pool.push(n);
                }
            }
            if round % 5 == 0
                && let CancelOutcome::Cancelled(n) = table.cancel(k2, &round)
            {
                pool.push(n);
            }
            if round % 2 == 0 {
                let mut woken = table.wake(k2, 2, FUTEX_BITSET_MATCH_ANY).unwrap();
                while let Some(n) = woken.pop() {
                    pool.push(n);
                }
            }
        }
    });

    // Drop everything outside the zero-heap measurement window.
    drop(zero_bitset_node);
    drop(table);
    drop(other_bucket);
    drop(pool);
}
