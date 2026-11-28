//! Zero-allocation and zero-deallocation proof for `lumen-eventfd`.
//!
//! Runs in its own integration-test binary with a single `#[test]` so no other
//! test thread allocates concurrently while the counting `#[global_allocator]`
//! observes heap traffic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_eventfd::{
    ClockError, EventCounter, EventfdFlags, ITimer, Invalid, ReadBytesError, Requeue, TimerClock, TimerCount,
    TimerfdCreateFlags, TimerfdSettimeFlags, WouldBlock, WriteError, itimerspec_to_ns,
};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every method delegates directly to `System`, upholding
// `GlobalAlloc`'s safety contract; the atomics only observe calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::alloc` preconditions.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::alloc_zeroed` preconditions.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::realloc` preconditions.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Caller upholds `GlobalAlloc::dealloc` preconditions.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn all_eventfd_and_timerfd_operations_perform_zero_allocations() {
    // Positive control: confirm the counting allocator observes a real Box alloc/dealloc.
    let a0 = ALLOCS.load(Ordering::SeqCst);
    let d0 = DEALLOCS.load(Ordering::SeqCst);
    let probe = std::hint::black_box(Box::new([0u64; 4]));
    assert!(ALLOCS.load(Ordering::SeqCst) > a0, "counting allocator must observe alloc");
    drop(probe);
    assert!(DEALLOCS.load(Ordering::SeqCst) > d0, "counting allocator must observe dealloc");

    let allocs_before = ALLOCS.load(Ordering::SeqCst);
    let deallocs_before = DEALLOCS.load(Ordering::SeqCst);

    // Exercise EventCounter in normal and semaphore modes, including error paths and bytes.
    assert!(EventfdFlags::from_bits(0x80801).is_ok());
    assert!(EventfdFlags::from_bits(0x4).is_err());

    let mut ev = EventCounter::new(0, false);
    assert!(!ev.readable() && ev.writable());
    assert_eq!(ev.write(u64::MAX), Err(WriteError::Invalid));
    assert_eq!(ev.write(0xffff_ffff_ffff_fffe), Ok(()));
    assert!(!ev.writable() && ev.readable());
    assert_eq!(ev.write(1), Err(WriteError::WouldBlock));
    assert_eq!(ev.read(), Ok(0xffff_ffff_ffff_fffe));
    assert_eq!(ev.read(), Err(WouldBlock));

    let mut sem = EventCounter::new(2, true);
    let mut buf = [0u8; 8];
    assert_eq!(sem.read_bytes(&mut buf[..7]), Err(ReadBytesError::Invalid));
    assert_eq!(sem.read_bytes(&mut buf), Ok(8));
    assert_eq!(u64::from_ne_bytes(buf), 1);
    assert_eq!(sem.write_bytes(&[0u8; 7]), Err(WriteError::Invalid));
    assert_eq!(sem.write_bytes(&5u64.to_ne_bytes()), Ok(8));

    // Exercise TimerCount across arm, arm_realtime, settime, on_expiry, get, read,
    // disarm, next_wakeup, itimerspec_to_ns, flags, and clocks.
    assert_eq!(TimerClock::from_clockid(1), Ok(TimerClock::Monotonic));
    assert_eq!(TimerClock::from_clockid(8), Err(ClockError::AlarmNotPermitted));
    assert_eq!(TimerClock::from_clockid(99), Err(ClockError::Invalid));
    assert!(TimerfdCreateFlags::from_bits(0x80800).is_ok());
    let abs_flags = TimerfdSettimeFlags::from_bits(0x1).unwrap();
    assert_eq!(itimerspec_to_ns(1, 500), Ok(1_000_000_500));
    assert_eq!(itimerspec_to_ns(-1, 0), Err(Invalid));

    let mut tm = TimerCount::new();
    assert_eq!(
        tm.arm(100, ITimer::new(20, 10), false),
        (ITimer::new(0, 0), Requeue { remove: None, insert: Some(120) })
    );
    assert_eq!(tm.next_wakeup(), Some(120));
    assert_eq!(tm.get(110), ITimer::new(10, 10));
    assert_eq!(tm.on_expiry(120), (1, Requeue { remove: Some(120), insert: None }));
    assert_eq!(tm.next_wakeup(), None);
    assert!(tm.readable(155));
    assert_eq!(tm.read_bytes(&mut buf[..7], 155), Err(ReadBytesError::Invalid));
    assert_eq!(tm.read_bytes(&mut buf, 155), Ok((8, Requeue { remove: None, insert: Some(160) })));
    assert_eq!(u64::from_ne_bytes(buf), 4);
    assert_eq!(tm.next_wakeup(), Some(160));
    assert_eq!(tm.read(155), Err(WouldBlock));
    assert_eq!(
        tm.settime(10, 100, TimerClock::Realtime, abs_flags, ITimer::new(80, 3)),
        (ITimer::new(150, 10), Requeue { remove: Some(160), insert: Some(0) })
    );
    assert_eq!(tm.read(10), Ok((7, Requeue { remove: Some(0), insert: Some(11) })));
    assert_eq!(tm.disarm(), Requeue { remove: Some(11), insert: None });
    assert_eq!(tm.get(200), ITimer::new(0, 0));

    let allocs_after = ALLOCS.load(Ordering::SeqCst);
    let deallocs_after = DEALLOCS.load(Ordering::SeqCst);
    assert_eq!(allocs_after - allocs_before, 0, "expected zero heap allocations");
    assert_eq!(deallocs_after - deallocs_before, 0, "expected zero heap deallocations");
}
