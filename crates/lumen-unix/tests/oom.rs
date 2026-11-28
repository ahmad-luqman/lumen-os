//! Verifies with a refusing global allocator that every fallible constructor
//! and pre-allocation helper returns `NoMemory` (never panicking or aborting)
//! and leaves existing socket state intact on OOM.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_unix::{
    AcceptOutcome, Ancillary, ByteStorage, ConnectOutcome, DEFAULT_INITIAL_BUF, DEFAULT_SNDBUF, DgramPayload,
    DgramQueue, DgramSlotStorage, End, Listener, ListenerStorage, NoMemory, SOMAXCONN, SegmentStorage, SetSndbufError,
    StreamPair, StreamRecvOutcome, StreamSendOutcome, Ucred,
};

static REFUSE_FROM: AtomicUsize = AtomicUsize::new(0);

struct Refusing;

// SAFETY: Every allowed call delegates to `System`; returning null on refusal
// is explicitly permitted by `GlobalAlloc`.
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
fn refusing_heap_returns_no_memory_and_preserves_state() {
    // Every fallible constructor and pre-allocation helper refuses cleanly on OOM.
    assert_eq!(
        with_refusal(DEFAULT_INITIAL_BUF, || StreamPair::<u32>::try_new(DEFAULT_SNDBUF)).unwrap_err(),
        SetSndbufError::NoMemory
    );
    assert_eq!(with_refusal(1024, || DgramQueue::<u32, u32, u32>::try_new(256)).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(1024, || Listener::<u32>::try_new(1024)).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(1024, || ListenerStorage::<u32>::try_for_backlog(1024, SOMAXCONN)).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(1024, || SegmentStorage::<u32>::try_with_capacity(1024)).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(4096, || ByteStorage::try_with_capacity(4096)).unwrap_err(), NoMemory);
    assert_eq!(
        with_refusal(1024, || DgramSlotStorage::<u32, u32, u32>::try_with_capacity(1024)).unwrap_err(),
        NoMemory
    );
    assert_eq!(with_refusal(4096, || DgramPayload::try_with_capacity(4096)).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(4096, || DgramPayload::try_from_slice(&[0u8; 4096])).unwrap_err(), NoMemory);
    assert_eq!(with_refusal(1024, || Ancillary::<u32>::try_with_fd_capacity(1024, None)).unwrap_err(), NoMemory);

    // Existing StreamPair state is untouched when a pre-allocation fails on OOM.
    let c1 = Ucred::new(1, 0, 0);
    let mut pair = StreamPair::<u32>::try_new(256).unwrap();
    assert_eq!(pair.send(End::A, b"preserved", c1, None), StreamSendOutcome::Sent(9));
    assert_eq!(with_refusal(8192, || ByteStorage::try_with_capacity(8192)).unwrap_err(), NoMemory);
    let mut out = [0u8; 16];
    let StreamRecvOutcome::Data { bytes, .. } = pair.recv(End::B, &mut out, false, false) else {
        panic!("expected preserved stream bytes");
    };
    assert_eq!(&out[..bytes], b"preserved");

    // Existing Listener state is untouched when ListenerStorage pre-allocation fails on OOM.
    let mut listener = Listener::<u32>::try_new(2).unwrap();
    assert_eq!(listener.connect(42), ConnectOutcome::Queued);
    assert_eq!(with_refusal(4096, || ListenerStorage::<u32>::try_for_backlog(4096, SOMAXCONN)).unwrap_err(), NoMemory);
    assert_eq!(listener.backlog(), 2);
    assert_eq!(listener.accept(), AcceptOutcome::Accepted { conn: 42, wake_connecters: false });
}
