//! Verifies that `Pipe::read`, `Pipe::peek`, `Peeked::commit`,
//! `Peeked::commit_prefix`, `Pipe::write`, `WriteProgress::step`, `fionread`,
//! readiness predicates, and no-op/failing `set_capacity` perform zero heap
//! allocations, using a counting global allocator in its own test binary (as
//! in `crates/lumen-app-api/tests/decode_peak.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_pipe::{
    ChunkError, Consumed, DEFAULT_MAX_CAPACITY, PAGE_SIZE, PIPE_BUF, PeekOutcome, Pipe, ReadOutcome, Released, Resized,
    SetCapacityError, WriteOutcome, WriteProgress, WriteStep,
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
    let (mut pipe, init_allocs) = count_allocs(|| Pipe::try_new().unwrap());
    assert!(init_allocs >= 1, "expected Pipe::try_new() to allocate its ring buffer");

    let (resized, resize_allocs) = count_allocs(|| pipe.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY));
    assert_eq!(resized, Ok(Resized { capacity: 2 * PAGE_SIZE, wake_writers: false }));
    assert!(resize_allocs >= 1, "expected changing capacity to allocate");

    // Pre-allocate scratch buffers outside the zero-allocation region.
    let atomic_buf = [0xABu8; PIPE_BUF];
    let large_buf = [0xCDu8; PIPE_BUF + 1024];
    let mut read_buf = [0u8; 3000];

    let (_, io_allocs) = count_allocs(|| {
        // Keep the pipe non-empty so head advances across the 8 KiB ring wrap
        // boundary rather than resetting to 0.
        assert_eq!(pipe.write(&[0x11; 3500]), WriteOutcome::Wrote(3500));
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::Read(3000));
        // 500 bytes at [3000..3500]; next 4096-byte writes and reads wrap past 8192 in most iterations.
        for _ in 0..32 {
            assert_eq!(pipe.write(&atomic_buf), WriteOutcome::Wrote(PIPE_BUF));
            assert!(pipe.readable());
            assert!(!pipe.writable());
            assert!(!pipe.reader_hung_up());
            assert!(!pipe.writer_error());
            assert_eq!(pipe.fionread(), 500 + PIPE_BUF);

            // Partial non-blocking write fills the remaining 3596 bytes across the wrap.
            assert_eq!(pipe.write(&large_buf), WriteOutcome::Wrote(2 * PAGE_SIZE - 500 - PIPE_BUF));
            assert_eq!(pipe.write(&atomic_buf), WriteOutcome::WouldBlock);
            assert_eq!(pipe.write(&large_buf), WriteOutcome::WouldBlock);

            // Same-size and failing set_capacity must also allocate nothing.
            assert_eq!(
                pipe.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY),
                Ok(Resized { capacity: 2 * PAGE_SIZE, wake_writers: false })
            );
            assert_eq!(pipe.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Busy));
            assert_eq!(
                pipe.set_capacity(DEFAULT_MAX_CAPACITY + 1, DEFAULT_MAX_CAPACITY),
                Err(SetCapacityError::OverLimit)
            );
            assert_eq!(pipe.set_capacity((1usize << 31) + 1, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Invalid));

            // Drain back down to 500 bytes (bounded loop, never resetting head to 0).
            let mut drained = 0usize;
            let target = 2 * PAGE_SIZE - 500;
            for _ in 0..16 {
                let want = (target - drained).min(read_buf.len());
                if want == 0 {
                    break;
                }
                match pipe.read(&mut read_buf[..want]) {
                    ReadOutcome::Read(n) => drained += n,
                    other => panic!("unexpected read: {other:?}"),
                }
            }
            assert_eq!(drained, target);
            assert_eq!(pipe.fionread(), 500);
        }

        // Position head at 8000 (500 bytes spanning [8000..8192] and [0..308])
        // so peek and commit_prefix/commit also execute their wrap-around path.
        assert_eq!(pipe.write(&atomic_buf), WriteOutcome::Wrote(PIPE_BUF));
        assert_eq!(pipe.write(&[0x22; 520]), WriteOutcome::Wrote(520));
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::Read(3000));
        assert_eq!(pipe.read(&mut read_buf[..1616]), ReadOutcome::Read(1616));
        assert_eq!(pipe.fionread(), 500);
        let PeekOutcome::Data(peeked) = pipe.peek(&mut read_buf) else {
            panic!("expected wrapped Data");
        };
        assert_eq!(peeked.len(), 500);
        assert_eq!(peeked.commit_prefix(200), Consumed { n: 200, wake_writers: false });
        let PeekOutcome::Data(rest) = pipe.peek(&mut read_buf) else {
            panic!("expected rest Data");
        };
        assert_eq!(rest.commit(), Consumed { n: 300, wake_writers: false });
        assert_eq!(pipe.fionread(), 0);
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::WouldBlock);

        // Exercise WriteProgress::step across Continue, NeedSpace (partial and
        // zero), ChunkError, and Done with zero allocations.
        let mut progress = WriteProgress::new(3 * PIPE_BUF);
        assert_eq!(progress.step(&mut pipe, &[]), Err(ChunkError::Empty));
        assert_eq!(progress.step(&mut pipe, &atomic_buf), Ok(WriteStep::Continue { wrote_now: PIPE_BUF }));
        assert_eq!(progress.step(&mut pipe, &atomic_buf), Ok(WriteStep::NeedSpace { wrote_now: PIPE_BUF }));
        assert_eq!(progress.step(&mut pipe, &atomic_buf), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::Read(3000));
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::Read(3000));
        assert_eq!(
            progress.step(&mut pipe, &atomic_buf),
            Ok(WriteStep::Done { total: 3 * PIPE_BUF, wake_next_writer: true })
        );

        // Drain the rest (bounded).
        for _ in 0..16 {
            if pipe.is_empty() {
                break;
            }
            let _ = pipe.read(&mut read_buf);
        }
        assert!(pipe.is_empty());

        // Open/release ends, EOF, and Broken paths.
        pipe.open_writer();
        assert_eq!(pipe.release_writer(), Released::Remaining(NonZeroUsize::new(1).unwrap()));
        assert_eq!(pipe.release_writer(), Released::Last);
        assert_eq!(pipe.release_writer(), Released::Underflow);
        assert!(pipe.reader_hung_up());
        assert_eq!(pipe.read(&mut read_buf), ReadOutcome::Eof);

        assert_eq!(pipe.release_reader(), Released::Last);
        assert!(pipe.writer_error());
        assert_eq!(pipe.write(&atomic_buf), WriteOutcome::Broken);
        let mut broken_prog = WriteProgress::new(PIPE_BUF);
        assert_eq!(broken_prog.step(&mut pipe, &atomic_buf), Ok(WriteStep::Broken { written: 0 }));
    });

    assert_eq!(io_allocs, 0, "expected zero heap allocations during pipe I/O, got {io_allocs}");
}
