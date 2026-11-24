//! Verifies that `Pipe::read`, `Pipe::write`, `WriteProgress::step`,
//! `fionread`, readiness predicates, and no-op/failing `set_capacity` perform
//! zero heap allocations, using a counting global allocator in its own test
//! binary (as in `crates/lumen-app-api/tests/decode_peak.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_pipe::{
    DEFAULT_MAX_CAPACITY, PAGE_SIZE, PIPE_BUF, Pipe, ReadOutcome, SetCapacityError, WriteOutcome, WriteProgress,
    WriteStep,
};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every method delegates directly to `System`, upholding
// `GlobalAlloc`'s safety contract; the atomic counter only observes calls.
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
        // SAFETY: Caller upholds `GlobalAlloc::dealloc` preconditions.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCS.load(Ordering::SeqCst);
    let out = f();
    let after = ALLOCS.load(Ordering::SeqCst);
    (out, after - before)
}

#[test]
fn read_and_write_perform_zero_allocations() {
    // Positive control: constructing and resizing a Pipe do allocate.
    let (mut pipe, init_allocs) = count_allocs(Pipe::new);
    assert!(init_allocs >= 1, "expected Pipe::new() to allocate its ring buffer");

    let (resized, resize_allocs) = count_allocs(|| pipe.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY));
    assert_eq!(resized, Ok(2 * PAGE_SIZE));
    assert!(resize_allocs >= 1, "expected changing capacity to allocate");

    // Pre-allocate scratch buffers outside the zero-allocation region.
    let atomic_buf = [0xABu8; PIPE_BUF];
    let large_buf = [0xCDu8; PIPE_BUF + 1024];
    let mut read_buf = [0u8; 3000];

    let (_, io_allocs) = count_allocs(|| {
        // Repeatedly write and read across wrap boundaries.
        for _ in 0..64 {
            assert_eq!(pipe.write(&atomic_buf, false), WriteOutcome::Wrote(PIPE_BUF));
            assert!(pipe.readable());
            assert!(pipe.writable());
            assert!(!pipe.reader_hung_up());
            assert!(!pipe.writer_error());
            assert_eq!(pipe.fionread(), PIPE_BUF);

            assert_eq!(pipe.write(&large_buf, true), WriteOutcome::Wrote(PIPE_BUF));
            assert!(!pipe.writable());
            assert_eq!(pipe.write(&atomic_buf, true), WriteOutcome::WouldBlock);
            assert_eq!(pipe.write(&large_buf, true), WriteOutcome::WouldBlock);

            // Same-size and failing set_capacity must also allocate nothing.
            assert_eq!(pipe.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(2 * PAGE_SIZE));
            assert_eq!(pipe.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Busy));
            assert_eq!(
                pipe.set_capacity(DEFAULT_MAX_CAPACITY + 1, DEFAULT_MAX_CAPACITY),
                Err(SetCapacityError::TooLarge)
            );

            // Drain across multiple reads.
            let mut total_read = 0;
            while total_read < 2 * PIPE_BUF {
                match pipe.read(&mut read_buf, false) {
                    ReadOutcome::Read(n) => total_read += n,
                    other => panic!("unexpected read: {other:?}"),
                }
            }
            assert_eq!(pipe.fionread(), 0);
            assert_eq!(pipe.read(&mut read_buf, true), ReadOutcome::WouldBlock);
        }

        // WriteProgress loop with zero allocations.
        let mut progress = WriteProgress::new(large_buf.len());
        assert_eq!(progress.step(&mut pipe, &large_buf), WriteStep::Done(large_buf.len()));
        assert_eq!(pipe.read(&mut read_buf, false), ReadOutcome::Read(3000));
        assert_eq!(pipe.read(&mut read_buf, false), ReadOutcome::Read(large_buf.len() - 3000));

        // Reader/writer counts, EOF, and Broken paths.
        pipe.add_writer();
        assert_eq!(pipe.close_writer(), 1);
        assert_eq!(pipe.close_writer(), 0);
        assert!(pipe.reader_hung_up());
        assert_eq!(pipe.read(&mut read_buf, false), ReadOutcome::Eof);

        assert_eq!(pipe.close_reader(), 0);
        assert!(pipe.writer_error());
        assert_eq!(pipe.write(&atomic_buf, false), WriteOutcome::Broken);
    });

    assert_eq!(io_allocs, 0, "expected zero heap allocations during pipe I/O, got {io_allocs}");
}
