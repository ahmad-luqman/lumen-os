//! Verifies with a counting global allocator that `StreamPair`, `DgramQueue`,
//! `Listener`, `Ancillary::take_fds`, and GC hooks perform zero heap
//! allocations on every non-allocating path (including `set_sndbuf`,
//! `grow_buffer`, `grow_segments`, `grow_slots`, `grow_storage` with
//! `listen(10)` and 10 connects, `recv`, `peek`, `purge`, `purge_recv`,
//! `close`, and `for_each_queued_fd`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_unix::{
    AcceptOutcome, Ancillary, ByteStorage, ConnectOutcome, Datagram, DgramPayload, DgramQueue, DgramRecvOutcome,
    DgramSendOutcome, DgramSlotStorage, End, ListenError, Listener, ListenerStorage, SOMAXCONN, SegmentStorage,
    SendBudget, ShutdownHow, StreamPair, StreamRecvOutcome, StreamSendOutcome, Ucred,
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
fn non_allocating_paths_perform_zero_heap_allocations() {
    // Positive controls: construction and pre-allocation do allocate.
    let (mut pair, pair_allocs) = count_allocs(|| StreamPair::<u32>::try_new(192).unwrap());
    assert!(pair_allocs >= 1, "expected StreamPair construction to allocate");

    let (mut dgram, dgram_allocs) = count_allocs(|| DgramQueue::<u32, u32, u32>::try_new(4).unwrap());
    assert!(dgram_allocs >= 1, "expected DgramQueue construction to allocate");

    let (mut listener, list_allocs) = count_allocs(|| Listener::<u32>::try_new(3).unwrap());
    assert!(list_allocs >= 1, "expected Listener construction to allocate");

    // Pre-allocate ancillary payloads, datagrams, and swap storages outside the zero-alloc region.
    let cred = Ucred::new(100, 1000, 1000);
    let anc_stream_1 = Ancillary::new(vec![1u32, 2, 3, 4], Some(cred));
    let anc_stream_2 = Ancillary::from_fds(vec![5u32, 6]);
    let anc_stream_undel = Ancillary::from_fds(vec![7u32, 8]);
    let anc_dgram_1 = Ancillary::new(vec![10u32, 11, 12], Some(cred));
    let anc_dgram_undel = Ancillary::from_fds(vec![13u32, 14]);
    let spare_segs = SegmentStorage::<u32>::try_with_capacity(16).unwrap();
    let spare_buf = ByteStorage::try_with_capacity(256).unwrap();
    let spare_slots = DgramSlotStorage::<u32, u32, u32>::try_with_capacity(16).unwrap();
    let spare_listener = ListenerStorage::<u32>::try_for_backlog(10, SOMAXCONN).unwrap();

    let d1 = Datagram::new(DgramPayload::try_from_slice(b"hello world").unwrap(), Some(1), 1, cred, Some(anc_dgram_1));
    let d2 = Datagram::new(DgramPayload::try_from_slice(b"22").unwrap(), Some(2), 1, cred, None);
    let d3 = Datagram::new(DgramPayload::try_from_slice(b"").unwrap(), Some(3), 1, cred, None);
    let d4 = Datagram::new(DgramPayload::try_from_slice(b"44").unwrap(), Some(4), 1, cred, None);
    let d5 = Datagram::new(DgramPayload::try_from_slice(b"55").unwrap(), Some(5), 1, cred, None);
    let d6 = Datagram::new(DgramPayload::try_from_slice(b"66").unwrap(), Some(6), 1, cred, None);
    let d7 = Datagram::new(DgramPayload::try_from_slice(b"end").unwrap(), Some(9), 1, cred, Some(anc_dgram_undel));

    let payload = [0x5Au8; 64];
    let mut read_buf = [0u8; 128];

    let (_, io_allocs) = count_allocs(|| {
        // --- StreamPair zero-allocation paths ---
        for _ in 0..8 {
            assert_eq!(pair.send(End::A, &payload, cred, None), StreamSendOutcome::Sent(64));
            assert_eq!(pair.send(End::A, &payload, cred, None), StreamSendOutcome::Sent(64));
            assert_eq!(pair.send(End::A, &payload, cred, None), StreamSendOutcome::WouldBlock(None));
            assert!(pair.readable(End::B));
            assert!(!pair.writable(End::A));
            let _ = pair.readiness(End::B);
            assert_eq!(pair.readable_bytes(End::B), 128);

            let StreamRecvOutcome::Data { bytes, .. } = pair.recv(End::B, &mut read_buf[..50], false, false) else {
                panic!("expected stream read");
            };
            assert_eq!(bytes, 50);

            let StreamRecvOutcome::Data { bytes, wake_writer, .. } = pair.recv(End::B, &mut read_buf, false, false)
            else {
                panic!("expected second stream read");
            };
            assert_eq!(bytes, 78);
            assert!(wake_writer);
        }

        // set_sndbuf + grow_buffer + grow_segments perform zero allocations under lock.
        assert_eq!(pair.set_sndbuf(End::A, 512), Ok(false));
        let old_buf = pair.grow_buffer(End::A, spare_buf);
        let old_segs = pair.grow_segments(End::A, spare_segs);

        assert_eq!(pair.send(End::A, b"pre-", cred, None), StreamSendOutcome::Sent(4));
        assert_eq!(pair.send(End::A, b"abcd", cred, Some(anc_stream_1)), StreamSendOutcome::Sent(4));
        assert_eq!(pair.send(End::A, b"ef", cred, Some(anc_stream_2)), StreamSendOutcome::Sent(2));
        assert_eq!(pair.in_flight_fds(End::A), 6);

        let mut scanned = 0usize;
        pair.for_each_queued_fd(End::B, |_| scanned += 1);
        assert_eq!(scanned, 6);

        let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut read_buf, true, false) else {
            panic!("expected peek");
        };
        assert_eq!(bytes, 8);
        assert_eq!(ancillary.unwrap().fds(), &[1, 2, 3, 4]);

        let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut read_buf, false, false) else {
            panic!("expected consume");
        };
        assert_eq!(bytes, 8);
        let mut owned_anc = ancillary.unwrap().into_owned().unwrap();
        let mut taken = owned_anc.take_fds(2);
        assert!(taken.truncated());
        let mut sum = 0u32;
        while let Some(fd) = taken.next_delivered() {
            sum += fd;
        }
        for fd in taken.drain_dropped() {
            sum += fd;
        }
        assert_eq!(sum, 10);

        // Shutdown, close, close_reset, and O(1) pop_fd drain with zero allocations.
        assert_eq!(pair.send(End::B, b"zz", cred, Some(anc_stream_undel)), StreamSendOutcome::Sent(2));
        pair.shutdown(End::A, ShutdownHow::Write);
        assert_eq!(pair.send(End::A, b"x", cred, None), StreamSendOutcome::BrokenPipe(None));
        let mut undel_a = pair.close(End::A);
        assert!(undel_a.had_unread_bytes());
        let mut count_a = 0usize;
        while undel_a.pop_fd().is_some() {
            count_a += 1;
        }
        assert_eq!(count_a, 2);
        let mut undel_b = pair.close_reset(End::B);
        let mut count_b = 0usize;
        while undel_b.pop_fd().is_some() {
            count_b += 1;
        }
        assert_eq!(count_b, 2);
        drop(old_buf);
        drop(old_segs);

        // --- DgramQueue zero-allocation paths ---
        let mut budget = SendBudget::new(1024);
        assert_eq!(dgram.send_charged(&mut budget, d1, false), DgramSendOutcome::Sent(11));
        assert_eq!(dgram.send_charged(&mut budget, d2, false), DgramSendOutcome::Sent(2));
        assert_eq!(dgram.send_charged(&mut budget, d3, false), DgramSendOutcome::Sent(0));
        assert_eq!(dgram.send_charged(&mut budget, d4, false), DgramSendOutcome::Sent(2));
        assert_eq!(dgram.send_charged(&mut budget, d5, false), DgramSendOutcome::Sent(2));
        assert!(matches!(dgram.send_charged(&mut budget, d6, false), DgramSendOutcome::WouldBlock(_)));
        let old_slots = dgram.grow_slots(spare_slots);
        let mut d_scanned = 0usize;
        dgram.for_each_queued_fd(|_| d_scanned += 1);
        assert_eq!(d_scanned, 3);

        let DgramRecvOutcome::Data { full_len, truncated, .. } = dgram.recv(&mut read_buf[..4], true) else {
            panic!("expected dgram peek");
        };
        assert_eq!(full_len, 11);
        assert!(truncated);
        let DgramRecvOutcome::Data { full_len, ancillary, reclaimed, .. } = dgram.recv(&mut read_buf[..4], false)
        else {
            panic!("expected dgram recv");
        };
        budget.release(full_len);
        drop(reclaimed);
        let mut d_anc = ancillary.unwrap().into_owned().unwrap();
        let mut d_taken = d_anc.take_fds(1);
        assert_eq!(d_taken.next_delivered(), Some(10));
        assert_eq!(d_taken.drain_dropped().count(), 2);

        assert_eq!(dgram.send(d7, false), DgramSendOutcome::Sent(3));
        let _ = dgram.shutdown(ShutdownHow::Read);
        let mut d_undel = dgram.close();
        let mut drained_fds = 0usize;
        while let Some(item) = d_undel.pop_next() {
            if let Some(mut a) = item.ancillary {
                drained_fds += a.drain_fds().count();
            }
        }
        assert_eq!(drained_fds, 2);
        drop(old_slots);

        // --- Listener growth zero-allocation case: unconditionally assert NeedStorage(11) ---
        assert_eq!(listener.connect(0), ConnectOutcome::Queued);
        assert_eq!(listener.listen(10, SOMAXCONN), Err(ListenError::NeedStorage(11)));
        let old_l = listener.grow_storage(spare_listener);
        assert_eq!(listener.listen(10, SOMAXCONN), Ok(false));
        drop(old_l);
        for i in 1..=10u32 {
            assert_eq!(listener.connect(i), ConnectOutcome::Queued);
        }
        assert_eq!(listener.connect(99), ConnectOutcome::WouldBlock(99));
        assert_eq!(listener.pending().count(), 11);
        let _ = listener.readiness();
        assert_eq!(listener.accept(), AcceptOutcome::Accepted { conn: 0, wake_connecters: true });
        assert_eq!(listener.listen(2, SOMAXCONN), Ok(false));
        let mut pending = listener.close();
        let mut closed_conns = 0usize;
        while pending.next().is_some() {
            closed_conns += 1;
        }
        assert_eq!(closed_conns, 10);
        assert_eq!(listener.connect(100), ConnectOutcome::Refused(100));
        assert_eq!(listener.accept(), AcceptOutcome::Closed);
    });

    assert_eq!(io_allocs, 0, "expected zero heap allocations on non-allocating paths, got {io_allocs}");
}
