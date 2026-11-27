//! Zero-allocation and zero-deallocation proof for [`lumen_epoll::EpollSet::wake`],
//! [`lumen_epoll::EpollSet::collect`], and [`lumen_epoll::EpollSet::modify`].
//!
//! Runs in its own integration-test binary with a single `#[test]` so no other
//! test thread can allocate concurrently while the counting `#[global_allocator]`
//! observes heap traffic (as in `crates/lumen-pipe/tests/zero_alloc.rs` and
//! `crates/lumen-futex/tests/zero_alloc.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_epoll::{EPOLLERR, EPOLLET, EPOLLHUP, EPOLLIN, EPOLLONESHOT, EPOLLOUT, EPOLLWAKEUP, EpollSet, Errno};

struct Counting;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every method delegates directly to `System`, upholding
// `GlobalAlloc`'s safety contract; the atomics only count calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::alloc` preconditions.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::alloc_zeroed` preconditions.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::realloc` preconditions.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::dealloc` preconditions.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Stack-allocated fixed-capacity `Extend<(u32, u64)>` sink so `collect`
/// output itself never touches the heap.
struct StackSink<const N: usize> {
    buf: [(u32, u64); N],
    len: usize,
}

impl<const N: usize> StackSink<N> {
    const fn new() -> Self {
        Self { buf: [(0, 0); N], len: 0 }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn as_slice(&self) -> &[(u32, u64)] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> Extend<(u32, u64)> for StackSink<N> {
    fn extend<T: IntoIterator<Item = (u32, u64)>>(&mut self, iter: T) {
        for item in iter {
            assert!(self.len < N, "StackSink overflow");
            self.buf[self.len] = item;
            self.len += 1;
        }
    }
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
fn wake_collect_and_modify_perform_zero_allocations() {
    // Positive control: constructing and populating an EpollSet allocates, and
    // dropping a Box deallocates, proving the counting allocator observes traffic.
    let a0 = ALLOC_COUNT.load(Ordering::SeqCst);
    let d0 = DEALLOC_COUNT.load(Ordering::SeqCst);
    let probe = std::hint::black_box(Box::new([0u64; 8]));
    assert!(ALLOC_COUNT.load(Ordering::SeqCst) > a0, "counting allocator must observe alloc");
    drop(probe);
    assert!(DEALLOC_COUNT.load(Ordering::SeqCst) > d0, "counting allocator must observe dealloc");

    let mut set = EpollSet::<(u64, u32)>::new();
    let add_before = ALLOC_COUNT.load(Ordering::SeqCst);
    for i in 0..32u32 {
        let events = match i % 4 {
            0 => EPOLLIN,
            1 => EPOLLIN | EPOLLET,
            2 => EPOLLIN | EPOLLONESHOT,
            _ => EPOLLOUT | EPOLLWAKEUP,
        };
        set.add((u64::from(i), i), events, u64::from(i)).unwrap();
    }
    assert!(
        ALLOC_COUNT.load(Ordering::SeqCst) > add_before,
        "expected EpollSet::add to allocate slots and map entries"
    );

    let mut sink = StackSink::<32>::new();

    // 1. `wake` across matching, duplicate, non-matching, and missing keys: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("wake(matching, duplicate, non-matching, missing)", || {
        for i in 0..32u32 {
            let k = (u64::from(i), i);
            assert!(set.wake(k, EPOLLIN | EPOLLOUT));
            assert!(set.wake(k, EPOLLIN | EPOLLOUT)); // duplicate wake
            assert!(!set.wake(k, 0)); // empty mask
        }
        assert!(!set.wake((999, 999), EPOLLIN | EPOLLERR | EPOLLHUP));
    });
    assert_eq!(set.ready_len(), 32);

    // 2. `collect` with partial max (exercising LT requeue, ET drop, one-shot disarm,
    //    and txlist prepend-to-front) plus max == 0: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("collect(partial, full, empty, max=0)", || {
        sink.clear();
        assert_eq!(set.collect(0, |_| EPOLLIN | EPOLLOUT, &mut sink), 0);

        for _ in 0..5 {
            sink.clear();
            let n = set.collect(7, |(_, fd)| if fd % 7 == 0 { 0 } else { EPOLLIN | EPOLLOUT }, &mut sink);
            assert!(n <= 7);
            assert_eq!(sink.as_slice().len(), n);
        }
    });

    // 3. `modify` (re-arming one-shots, updating events/data, ENOENT/EINVAL): 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("modify(re-arm, update, errors)", || {
        for i in (2..32u32).step_by(4) {
            let k = (u64::from(i), i);
            set.modify(k, EPOLLIN | EPOLLONESHOT, u64::from(i) + 1000).unwrap();
        }
        assert_eq!(set.modify((999, 999), EPOLLIN, 0), Err(Errno::NoEnt));
    });

    // 4. Sustained interleaved wake + collect + modify churn: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("interleaved churn (200 rounds)", || {
        for round in 0..200u32 {
            let fd = round % 32;
            let k = (u64::from(fd), fd);
            let ready = if round % 5 == 0 { EPOLLERR } else { EPOLLIN | EPOLLOUT };
            let _ = set.wake(k, ready);
            if round % 3 == 0 {
                let target_fd = ((round / 3) % 8) * 4 + 2;
                let _ = set.modify((u64::from(target_fd), target_fd), EPOLLIN | EPOLLONESHOT, u64::from(round));
            }
            sink.clear();
            let max = ((round % 5) + 1) as usize;
            let n = set.collect(max, |(_, f)| if (f + round) % 4 == 0 { 0 } else { EPOLLIN | EPOLLOUT }, &mut sink);
            assert!(n <= max);
            assert_eq!(sink.as_slice().len(), n);
        }
    });

    // Drop outside the zero-heap measurement window.
    drop(set);
}
