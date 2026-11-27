//! Zero-allocation and zero-deallocation proof for every leaf-spinlock
//! [`lumen_epoll::ReadyQueue`] operation (`wake`, `begin_scan`, `finish_scan`,
//! `finish_peek`, `abort_scan`, `activate`, `apply`, `unlink`, `unreport`, and
//! `install`) as well as [`lumen_epoll::EpollSet::modify`].
//!
//! Runs in its own integration-test binary with a single `#[test]` so no other
//! test thread can allocate concurrently while the counting `#[global_allocator]`
//! observes heap traffic (as in `crates/lumen-pipe/tests/zero_alloc.rs` and
//! `crates/lumen-futex/tests/zero_alloc.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_epoll::{
    EPOLLERR, EPOLLET, EPOLLEXCLUSIVE, EPOLLHUP, EPOLLIN, EPOLLONESHOT, EPOLLOUT, EPOLLWAKEUP, EpollSet, Errno, Event,
    Handle, ReadyQueue, Wake,
};

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

/// Stack-allocated fixed-capacity `Extend<Event>` sink so `finish_scan` output
/// itself never touches the heap.
struct StackSink<const N: usize> {
    buf: [Option<Event>; N],
    len: usize,
}

impl<const N: usize> StackSink<N> {
    const fn new() -> Self {
        Self { buf: [None; N], len: 0 }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn slice(&self) -> &[Option<Event>] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> Extend<Event> for StackSink<N> {
    fn extend<T: IntoIterator<Item = Event>>(&mut self, iter: T) {
        for item in iter {
            assert!(self.len < N, "StackSink overflow");
            self.buf[self.len] = Some(item);
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
fn spinlock_ready_queue_ops_and_modify_perform_zero_allocations() {
    // Positive control: Box alloc/dealloc, EpollSet::add, and ReadyQueue::prepare_grow allocate.
    let a0 = ALLOC_COUNT.load(Ordering::SeqCst);
    let d0 = DEALLOC_COUNT.load(Ordering::SeqCst);
    let probe = std::hint::black_box(Box::new([0u64; 8]));
    assert!(ALLOC_COUNT.load(Ordering::SeqCst) > a0, "counting allocator must observe alloc");
    drop(probe);
    assert!(DEALLOC_COUNT.load(Ordering::SeqCst) > d0, "counting allocator must observe dealloc");

    let mut set = EpollSet::<(u64, u32), u32>::new();
    let mut rq = ReadyQueue::new();
    let mut handles = Vec::with_capacity(33);

    let add_before = ALLOC_COUNT.load(Ordering::SeqCst);
    for i in 0..32u32 {
        let events = match i % 4 {
            0 => EPOLLIN,
            1 => EPOLLIN | EPOLLET,
            2 => EPOLLIN | EPOLLONESHOT,
            _ => EPOLLOUT | EPOLLWAKEUP,
        };
        let added = set.add((u64::from(i), i), events, u64::from(i), i).unwrap();
        let h = added.handle();
        // `activate` installs growth + activates slot with 0 allocs and 0 deallocs;
        // any old Storage is dropped outside the measurement window.
        let old = assert_zero_heap_traffic("ReadyQueue::activate", || rq.activate(&mut set, added));
        drop(old);
        handles.push(h);
    }
    // Add one EPOLLEXCLUSIVE interest for EINVAL modify coverage.
    let excl = set.add((100, 100), EPOLLIN | EPOLLEXCLUSIVE, 100, 100).unwrap();
    let excl_h = excl.handle();
    let old = assert_zero_heap_traffic("ReadyQueue::activate (excl)", || rq.activate(&mut set, excl));
    drop(old);
    assert!(ALLOC_COUNT.load(Ordering::SeqCst) > add_before, "expected EpollSet::add + prepare_grow to allocate");

    // Pre-allocate one extra growth Storage outside the lock to test explicit prepare_grow + install.
    let pg_before = ALLOC_COUNT.load(Ordering::SeqCst);
    let extra_storage = ReadyQueue::prepare_grow(128).unwrap();
    assert!(ALLOC_COUNT.load(Ordering::SeqCst) > pg_before, "expected prepare_grow to allocate");
    let old_storage = assert_zero_heap_traffic("ReadyQueue::install (explicit 128)", || rq.install(extra_storage));
    drop(old_storage);

    // Buffer sized >= set.len() (33) so begin_scan detaches the full ready list.
    let mut hbuf = [Handle::INVALID; 40];
    let mut pbuf = [0u32; 40];
    let mut sink = StackSink::<40>::new();

    // 1. `wake` across matching, duplicate, filtered, and stale handles: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("wake(matching, duplicate, filtered, stale)", || {
        for &h in &handles {
            assert_eq!(rq.wake(h, EPOLLIN | EPOLLOUT), Wake::Queued);
            assert_eq!(rq.wake(h, EPOLLIN | EPOLLOUT), Wake::AlreadyQueued);
            assert_eq!(rq.wake(h, 0), Wake::Filtered);
        }
        assert_eq!(rq.wake(excl_h, EPOLLIN), Wake::Queued);
        assert_eq!(rq.wake(excl_h, EPOLLOUT), Wake::Filtered);
        assert_eq!(rq.wake(Handle::INVALID, EPOLLIN), Wake::Stale);
    });
    assert_eq!(rq.ready_len(), 33);

    // 2. `begin_scan`, mid-scan `wake` (`DuringScan`), `finish_scan`, `unreport`,
    //    `finish_peek`, and `abort_scan`: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic(
        "begin_scan + DuringScan wake + finish_scan + unreport + finish_peek + abort_scan",
        || {
            let scan = rq.begin_scan(&mut hbuf);
            let len = scan.len();
            assert_eq!(len, 33);
            assert!(rq.events_available());
            for (i, &h) in scan.handles().iter().enumerate() {
                let e = set.get(h).unwrap();
                pbuf[i] = if e.key.1.is_multiple_of(7) { 0 } else { EPOLLIN | EPOLLOUT };
            }
            assert_eq!(rq.wake(handles[1], EPOLLIN), Wake::DuringScan);
            sink.clear();
            let done = rq.finish_scan(&set, scan, &pbuf[..len], 7, &mut sink);
            assert_eq!(done.reported, 7);
            assert!(done.has_ready);

            // Roll back the tail via `unreport` (zero alloc).
            assert!(rq.unreport(sink.slice()[3..done.reported].iter().map(|e| e.unwrap().handle)));

            // Nested-epoll non-consuming `begin_scan` + `finish_peek`.
            let pscan = rq.begin_scan(&mut hbuf);
            let plen = pscan.len();
            for p in &mut pbuf[..plen] {
                *p = EPOLLIN | EPOLLOUT;
            }
            let pdone = rq.finish_peek(pscan, &pbuf[..plen]);
            assert!(pdone.ready && pdone.has_ready);

            // Error-path `abort_scan`.
            let ascan = rq.begin_scan(&mut hbuf);
            assert!(rq.abort_scan(ascan));
        },
    );

    // 3. `EpollSet::modify` + `ReadyQueue::apply` (updates, re-arms, ENOENT, and EINVAL): 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("modify + apply (update, re-arm, ENOENT, EINVAL)", || {
        for i in (2..32u32).step_by(4) {
            let m = set.modify((u64::from(i), i), EPOLLIN | EPOLLONESHOT, u64::from(i) + 1000).unwrap();
            rq.apply(m);
        }
        assert_eq!(set.modify((999, 999), EPOLLIN, 0), Err(Errno::NoEnt));
        assert_eq!(set.modify((999, 999), EPOLLIN | EPOLLEXCLUSIVE, 0), Err(Errno::Inval));
        assert_eq!(set.modify((100, 100), EPOLLIN, 0), Err(Errno::Inval));
    });

    // 4. `unlink` (active + stale handle): 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("unlink (active + stale)", || {
        rq.unlink(excl_h);
        rq.unlink(excl_h);
        assert_eq!(rq.wake(excl_h, EPOLLIN | EPOLLERR | EPOLLHUP), Wake::Stale);
    });

    // 5. Sustained interleaved wake + begin_scan + finish_scan + unreport + modify churn: 0 allocs, 0 deallocs.
    assert_zero_heap_traffic("interleaved churn (200 rounds)", || {
        for round in 0..200u32 {
            let idx = (round % 32) as usize;
            let ready = if round % 5 == 0 { EPOLLERR } else { EPOLLIN | EPOLLOUT };
            let _ = rq.wake(handles[idx], ready);
            if round % 3 == 0 {
                let target = ((round / 3) % 8) * 4 + 2;
                let m = set.modify((u64::from(target), target), EPOLLIN | EPOLLONESHOT, u64::from(round)).unwrap();
                rq.apply(m);
            }
            let scan = rq.begin_scan(&mut hbuf);
            let slen = scan.len();
            for (i, &h) in scan.handles().iter().enumerate() {
                let e = set.get(h).unwrap();
                pbuf[i] = if (e.key.1 + round).is_multiple_of(4) { 0 } else { EPOLLIN | EPOLLOUT };
            }
            let _ = rq.wake(handles[(idx + 1) % 32], EPOLLIN);
            sink.clear();
            let max = ((round % 5) + 1) as usize;
            let done = rq.finish_scan(&set, scan, &pbuf[..slen], max, &mut sink);
            assert!(done.reported <= max);
            if done.reported > 1 && round % 7 == 0 {
                let _ = rq.unreport(sink.slice()[1..done.reported].iter().map(|e| e.unwrap().handle));
            }
        }
    });

    // Drop everything outside the zero-heap measurement window.
    drop(handles);
    drop(rq);
    drop(set);
}
