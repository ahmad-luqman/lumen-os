//! Verifies that `Pipe::try_new`, `Pipe::fifo`, `Pipe::with_capacity`, and
//! `Pipe::set_capacity` return their `NoMemory` error instead of aborting when
//! the heap refuses an allocation, and that a failed `set_capacity` leaves the
//! pipe's existing capacity and queued data intact (standalone binary with a
//! refusing global allocator, as in `crates/lumen-view/tests/oom.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_pipe::{
    CreateError, DEFAULT_CAPACITY, DEFAULT_MAX_CAPACITY, NoMemory, PAGE_SIZE, Pipe, ReadOutcome, SetCapacityError,
    WriteOutcome,
};

/// Refuse every allocation of at least this many bytes (0 = allow all).
static REFUSE_FROM: AtomicUsize = AtomicUsize::new(0);

struct Refusing;

// SAFETY: Every allowed call is forwarded to `System`, which upholds
// `GlobalAlloc`'s contract; returning a null pointer on refusal is explicitly
// permitted by `GlobalAlloc`.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let from = REFUSE_FROM.load(Ordering::SeqCst);
        if from != 0 && layout.size() >= from {
            return std::ptr::null_mut();
        }
        // SAFETY: Caller upholds `GlobalAlloc::alloc` preconditions.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Caller upholds `GlobalAlloc::dealloc` preconditions.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Refusing = Refusing;

fn with_refusal<T>(from: usize, f: impl FnOnce() -> T) -> T {
    REFUSE_FROM.store(from, Ordering::SeqCst);
    let out = f();
    REFUSE_FROM.store(0, Ordering::SeqCst);
    out
}

#[test]
fn refusing_heap_returns_no_memory_and_preserves_pipe() {
    // Pipe::try_new and Pipe::fifo refuse cleanly on OOM.
    assert_eq!(with_refusal(DEFAULT_CAPACITY, Pipe::try_new).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(DEFAULT_CAPACITY, Pipe::fifo).unwrap_err(), NoMemory);

    // Pipe::with_capacity refuses cleanly on OOM.
    assert_eq!(
        with_refusal(PAGE_SIZE, || Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY)).unwrap_err(),
        CreateError::NoMemory
    );

    // Pipe::set_capacity refuses cleanly on OOM and preserves capacity & data.
    let mut pipe = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
    let payload = b"preserved across oom";
    assert_eq!(pipe.write(payload), WriteOutcome::Wrote(payload.len()));

    assert_eq!(
        with_refusal(DEFAULT_MAX_CAPACITY, || pipe.set_capacity(DEFAULT_MAX_CAPACITY, DEFAULT_MAX_CAPACITY)),
        Err(SetCapacityError::NoMemory)
    );
    assert_eq!(pipe.capacity(), PAGE_SIZE);
    assert_eq!(pipe.fionread(), payload.len());

    let mut out = [0u8; 32];
    assert_eq!(pipe.read(&mut out), ReadOutcome::Read(payload.len()));
    assert_eq!(&out[..payload.len()], payload);
}
