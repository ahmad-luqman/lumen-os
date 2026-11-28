use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::{
    AcceptOutcome, Ancillary, ByteStorage, ConnectOutcome, DEFAULT_INITIAL_BUF, DEFAULT_MAX_DGRAM_QLEN, DEFAULT_SNDBUF,
    DGRAM_MAX_MSG_OVERHEAD, Datagram, DgramPayload, DgramPeer, DgramQueue, DgramRecvOutcome, DgramSendOutcome,
    DgramSlotStorage, DgramTargetError, End, ListenError, Listener, ListenerStorage, MIN_SNDBUF, MIN_STREAM_SNDBUF,
    Readiness, RecvAncillary, RecvValue, SCM_MAX_FD, SEGMENT_OVERHEAD, SHUT_RD, SHUT_RDWR, SHUT_WR, SOMAXCONN,
    SegmentStorage, SendBudget, SendBudgetError, SetSndbufError, ShutdownHow, StreamPair, StreamRecvOutcome,
    StreamSendOutcome, Ucred, clamp_backlog,
};

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        (self.next_u64() as usize) % upper
    }
}

/// Helper: send on `StreamPair`, executing the lock-free pre-allocation swap
/// protocol on `NeedBuffer` / `NeedSegment`.
fn stream_send_growing<F>(
    pair: &mut StreamPair<F>,
    end: End,
    data: &[u8],
    sender: Ucred,
    mut anc: Option<Ancillary<F>>,
) -> StreamSendOutcome<F> {
    for _ in 0..8 {
        match pair.send(end, data, sender, anc) {
            StreamSendOutcome::NeedBuffer(ret) => {
                let cap = pair.next_buffer_capacity(end, data.len());
                let spare = ByteStorage::try_with_capacity(cap).unwrap();
                let _old = pair.grow_buffer(end, spare);
                anc = ret;
            }
            StreamSendOutcome::NeedSegment(ret) => {
                let cap = pair.next_segment_capacity(end);
                let spare = SegmentStorage::try_with_capacity(cap).unwrap();
                let _old = pair.grow_segments(end, spare);
                anc = ret;
            }
            other => return other,
        }
    }
    panic!("stream_send_growing did not converge");
}

fn mk_dgram<F, A, S>(
    data: &[u8],
    addr: Option<A>,
    sender: S,
    cred: Ucred,
    ancillary: Option<Ancillary<F>>,
) -> Datagram<F, A, S> {
    Datagram::new(DgramPayload::try_from_slice(data).unwrap(), addr, sender, cred, ancillary)
}

#[test]
fn linux_constants_pinned_to_literals() {
    // Linux v6.12 include/linux/net.h: SHUT_RD(0), SHUT_WR(1), SHUT_RDWR(2).
    assert_eq!(SHUT_RD, 0);
    assert_eq!(SHUT_WR, 1);
    assert_eq!(SHUT_RDWR, 2);
    assert_eq!(ShutdownHow::from_raw(0), Some(ShutdownHow::Read));
    assert_eq!(ShutdownHow::from_raw(1), Some(ShutdownHow::Write));
    assert_eq!(ShutdownHow::from_raw(2), Some(ShutdownHow::Both));
    assert_eq!(ShutdownHow::from_raw(-1), None);
    assert_eq!(ShutdownHow::from_raw(3), None);
    assert_eq!(ShutdownHow::Read.as_raw(), 0);
    assert_eq!(ShutdownHow::Write.as_raw(), 1);
    assert_eq!(ShutdownHow::Both.as_raw(), 2);

    // Linux v6.12 include/linux/socket.h: SOMAXCONN = 4096.
    assert_eq!(SOMAXCONN, 4096);
    // Linux v6.12 include/net/scm.h: #define SCM_MAX_FD 253.
    assert_eq!(SCM_MAX_FD, 253);
    // Linux v6.12 net/core/sock.c sysctl_wmem_default = 212992; include/net/sock.h SOCK_MIN_SNDBUF = 4608.
    assert_eq!(DEFAULT_SNDBUF, 212_992);
    assert_eq!(MIN_SNDBUF, 4_608);
    // Linux v6.12 net/unix/af_unix.c:2015 `if (len > sk->sk_sndbuf - 32)` -> -EMSGSIZE.
    assert_eq!(DGRAM_MAX_MSG_OVERHEAD, 32);
    // Linux unix(7) default net.unix.max_dgram_qlen = 10.
    assert_eq!(DEFAULT_MAX_DGRAM_QLEN, 10);
    // Lumen fixed per-segment overhead and lazy initial stream ring size.
    assert_eq!(SEGMENT_OVERHEAD, 64);
    assert_eq!(MIN_STREAM_SNDBUF, 65);
    assert_eq!(DEFAULT_INITIAL_BUF, 4096);
}

#[test]
fn take_fds_truncation_next_delivered_and_drain_orders() {
    let cred = Ucred::new(42, 1000, 1000);

    // Partial truncation: max < len delivers first `max`, flags MSG_CTRUNC,
    // and returns the rest for the kernel to close (net/core/scm.c scm_detach_fds).
    let mut anc = Ancillary::new(vec![10u32, 20, 30, 40, 50], Some(cred));
    let mut taken = anc.take_fds(2);
    assert_eq!(anc.cred, Some(cred));
    assert!(anc.fds.is_empty());
    assert!(taken.truncated());
    assert_eq!(taken.delivered_len(), 2);
    assert_eq!(taken.dropped_len(), 3);
    assert_eq!(taken.delivered(), &[10, 20]);
    assert_eq!(taken.dropped(), &[30, 40, 50]);

    let delivered: Vec<u32> = taken.drain_delivered().collect();
    let dropped: Vec<u32> = taken.drain_dropped().collect();
    assert_eq!(delivered, vec![10, 20]);
    assert_eq!(dropped, vec![30, 40, 50]);
    assert!(taken.truncated());
    assert_eq!(taken.drain_delivered().count(), 0);
    assert_eq!(taken.drain_dropped().count(), 0);

    // Pop-style `next_delivered()` leaves un-popped delivered items reachable
    // for `drain_all()` when fd installation fails partway (e.g. -EMFILE).
    let mut anc = Ancillary::from_fds(vec![100u32, 200, 300, 400, 500]);
    let mut taken = anc.take_fds(3);
    assert!(taken.truncated());
    assert_eq!(taken.next_delivered(), Some(100));
    assert_eq!(taken.delivered(), &[200, 300]);
    assert_eq!(taken.dropped(), &[400, 500]);
    assert_eq!(taken.drain_all().collect::<Vec<_>>(), vec![200, 300, 400, 500]);
    assert_eq!(taken.next_delivered(), None);
    assert_eq!(taken.delivered_len(), 0);
    assert_eq!(taken.dropped_len(), 0);

    // Popping every delivered item stops at `deliver_len`, leaving excess in `drain_dropped()`.
    let mut anc = Ancillary::from_fds(vec![1u32, 2, 3, 4]);
    let mut taken = anc.take_fds(2);
    let mut popped = Vec::new();
    while let Some(fd) = taken.next_delivered() {
        popped.push(fd);
    }
    assert_eq!(popped, vec![1, 2]);
    assert_eq!(taken.drain_dropped().collect::<Vec<_>>(), vec![3, 4]);

    // Reverse drain order (dropped first, then delivered) yields identical parts.
    let mut anc = Ancillary::from_fds(vec![1u32, 2, 3, 4]);
    let mut taken = anc.take_fds(3);
    assert!(taken.truncated());
    let dropped: Vec<u32> = taken.drain_dropped().collect();
    assert!(taken.truncated());
    let delivered: Vec<u32> = taken.drain_delivered().collect();
    assert_eq!(dropped, vec![4]);
    assert_eq!(delivered, vec![1, 2, 3]);

    // max == 0: everything is dropped and truncated is true when non-empty.
    let mut anc = Ancillary::from_fds(vec![7u32, 8]);
    let mut taken = anc.take_fds(0);
    assert!(taken.truncated());
    assert_eq!(taken.next_delivered(), None);
    assert_eq!(taken.drain_dropped().collect::<Vec<_>>(), vec![7, 8]);

    // max == len and max > len: not truncated, dropped is empty.
    for max in [3usize, 10] {
        let mut anc = Ancillary::from_fds(vec![1u32, 2, 3]);
        let mut taken = anc.take_fds(max);
        assert!(!taken.truncated());
        assert_eq!(taken.drain_delivered().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(taken.drain_dropped().collect::<Vec<_>>(), vec![]);
    }

    // Empty fds with max == 0 is NOT truncated.
    let mut anc = Ancillary::<u32>::from_cred(cred);
    let mut taken = anc.take_fds(0);
    assert!(!taken.truncated());
    assert_eq!(taken.drain_all().count(), 0);

    // Ancillary::drain_fds hands all fds back without touching cred.
    let mut anc = Ancillary::new(vec![5u32, 6], Some(cred));
    assert_eq!(anc.drain_fds().collect::<Vec<_>>(), vec![5, 6]);
    assert_eq!(anc.cred, Some(cred));
    assert!(!anc.has_fds());
}

#[test]
fn stream_reads_follow_linux_af_unix_fd_and_cred_boundaries() {
    // Linux v6.12 net/unix/af_unix.c unix_stream_read_generic (~2866-2931):
    // - continues INTO an skb carrying fds, takes/peeks its fds, then breaks
    //   AFTER it (`if (scm.fp) break;`);
    // - never coalesces a later write into an skb that carries fds;
    // - splits on a writer identity change ONLY when `check_creds` (SO_PASSCRED) is set;
    // - reports the starting segment's writer identity on every read (including
    //   subsequent partial reads of the same segment).
    let c1 = Ucred::new(10, 1000, 1000);
    let c2 = Ucred::new(20, 2000, 2000);
    let mut pair = StreamPair::<u32>::try_new(4096).unwrap();
    let mut buf = [0u8; 64];

    // Probe 1: "abc" (no fds) + "def"/fds -> ONE read returning "abcdef" + fds.
    assert_eq!(pair.send(End::A, b"abc", c1, None), StreamSendOutcome::Sent(3));
    assert_eq!(pair.send(End::A, b"def", c1, Some(Ancillary::from_fds(vec![1, 2]))), StreamSendOutcome::Sent(3));
    assert_eq!(pair.in_flight_fds(End::A), 2);

    // MSG_PEEK reports segment 1's fds (unix_peek_fds), not just segment 0's.
    let StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer } = pair.recv(End::B, &mut buf, true, false)
    else {
        panic!("expected peek across plain+fds");
    };
    assert_eq!(&buf[..bytes], b"abcdef");
    assert_eq!(sender, c1);
    assert!(!wake_writer);
    let peeked = ancillary.expect("MSG_PEEK must report segment 1's fds");
    assert!(peeked.is_peeked());
    assert_eq!(peeked.fds(), &[1, 2]);
    assert_eq!(pair.in_flight_fds(End::A), 2);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected plain+fds merged read");
    };
    assert_eq!(&buf[..bytes], b"abcdef");
    assert_eq!(sender, c1);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![1, 2])));
    assert_eq!(pair.in_flight_fds(End::A), 0);

    // Probe 2: "hi"/fd + "!!" -> TWO reads: "hi" + fd, then "!!".
    assert_eq!(pair.send(End::A, b"hi", c1, Some(Ancillary::from_fds(vec![30]))), StreamSendOutcome::Sent(2));
    assert_eq!(pair.send(End::A, b"!!", c1, None), StreamSendOutcome::Sent(2));
    assert_eq!(pair.segment_count(End::A), 2);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected fd segment alone");
    };
    assert_eq!(&buf[..bytes], b"hi");
    assert_eq!(sender, c1);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![30])));

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected trailing plain segment");
    };
    assert_eq!(&buf[..bytes], b"!!");
    assert_eq!(sender, c1);
    assert!(ancillary.is_none());

    // Probe 3: Two back-to-back SCM_RIGHTS writes are never merged.
    assert_eq!(pair.send(End::A, b"11", c1, Some(Ancillary::from_fds(vec![10]))), StreamSendOutcome::Sent(2));
    assert_eq!(pair.send(End::A, b"22", c1, Some(Ancillary::from_fds(vec![20]))), StreamSendOutcome::Sent(2));
    assert_eq!(pair.in_flight_fds(End::A), 2);

    let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected first SCM_RIGHTS");
    };
    assert_eq!(&buf[..bytes], b"11");
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![10])));
    assert_eq!(pair.in_flight_fds(End::A), 1);

    let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected second SCM_RIGHTS");
    };
    assert_eq!(&buf[..bytes], b"22");
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![20])));
    assert_eq!(pair.in_flight_fds(End::A), 0);

    // Probe 4: Same writer with no fds coalesces; different writers split ONLY under check_creds.
    assert_eq!(pair.send(End::A, b"AA", c1, None), StreamSendOutcome::Sent(2));
    assert_eq!(pair.send(End::A, b"BB", c1, None), StreamSendOutcome::Sent(2));
    assert_eq!(pair.segment_count(End::A), 1);
    assert_eq!(pair.send(End::A, b"CC", c2, None), StreamSendOutcome::Sent(2));
    assert_eq!(pair.segment_count(End::A), 2);

    let StreamRecvOutcome::Data { bytes, sender, .. } = pair.recv(End::B, &mut buf, true, false) else {
        panic!("expected unsplit peek");
    };
    assert_eq!(&buf[..bytes], b"AABBCC");
    assert_eq!(sender, c1);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected c1 part");
    };
    assert_eq!(&buf[..bytes], b"AABB");
    assert_eq!(sender, c1);
    assert!(ancillary.is_none());

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected c2 part");
    };
    assert_eq!(&buf[..bytes], b"CC");
    assert_eq!(sender, c2);
    assert!(ancillary.is_none());

    // Probe 5: Plain c1 + fds c2 splits before c2 under check_creds, merges without it.
    assert_eq!(pair.send(End::A, b"xx", c1, None), StreamSendOutcome::Sent(2));
    assert_eq!(pair.send(End::A, b"yy", c2, Some(Ancillary::from_fds(vec![44]))), StreamSendOutcome::Sent(2));
    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, true, false) else {
        panic!("expected merged peek without check_creds");
    };
    assert_eq!(&buf[..bytes], b"xxyy");
    assert_eq!(sender, c1);
    assert_eq!(ancillary.unwrap().fds(), &[44]);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected split under check_creds");
    };
    assert_eq!(&buf[..bytes], b"xx");
    assert_eq!(sender, c1);
    assert!(ancillary.is_none());
    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected c2 fds");
    };
    assert_eq!(&buf[..bytes], b"yy");
    assert_eq!(sender, c2);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![44])));

    // Probe 6: Partial read reports writer identity on every read, detaches fds once.
    assert_eq!(pair.send(End::A, b"wxyz", c1, Some(Ancillary::from_fds(vec![99]))), StreamSendOutcome::Sent(4));
    assert_eq!(pair.send(End::A, b"PQ", c2, None), StreamSendOutcome::Sent(2));

    let mut small = [0u8; 2];
    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut small, false, true) else {
        panic!("expected partial first read");
    };
    assert_eq!(&small[..bytes], b"wx");
    assert_eq!(sender, c1);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![99])));
    assert_eq!(pair.in_flight_fds(End::A), 0);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected tail read");
    };
    assert_eq!(&buf[..bytes], b"yz");
    assert_eq!(sender, c1);
    assert!(ancillary.is_none());

    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut buf, false, true) else {
        panic!("expected c2 read");
    };
    assert_eq!(&buf[..bytes], b"PQ");
    assert_eq!(sender, c2);
    assert!(ancillary.is_none());
}

#[test]
fn stream_zero_length_read_vs_recv() {
    // Linux v6.12:
    // - read(2) with count == 0 returns 0 immediately (`ZeroBuffer`) without
    //   inspecting the queue, detaching fds, or clearing a pending reset.
    // - recv(2)/recvmsg(2) with len == 0 enters unix_stream_read_generic with
    //   sock_rcvlowat target = 1 (include/net/sock.h): on an empty queue it
    //   returns Reset (clearing sk_err), then Eof after shutdown, else WouldBlock;
    //   on a queued head segment carrying fds, it detaches (or peeks) those fds
    //   and returns 0 bytes while leaving the payload bytes queued.
    let c1 = Ucred::new(11, 100, 100);
    let mut pair = StreamPair::<u32>::try_new(512).unwrap();

    // Empty open queue: read(&mut []) -> ZeroBuffer, recv(&mut []) -> WouldBlock.
    assert_eq!(pair.read(End::B, &mut [], false), StreamRecvOutcome::ZeroBuffer);
    assert_eq!(pair.recv(End::B, &mut [], false, false), StreamRecvOutcome::WouldBlock);
    assert_eq!(pair.recv(End::B, &mut [], true, false), StreamRecvOutcome::WouldBlock);

    // Queued segment with fds: read(&mut []) leaves fds untouched; recv(&mut [], true)
    // peeks fds; recv(&mut [], false) detaches fds with 0 bytes and leaves bytes queued!
    assert_eq!(pair.send(End::A, b"hi", c1, Some(Ancillary::from_fds(vec![7, 8]))), StreamSendOutcome::Sent(2));
    assert_eq!(pair.read(End::B, &mut [], false), StreamRecvOutcome::ZeroBuffer);
    assert_eq!(pair.in_flight_fds(End::A), 2);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer } = pair.recv(End::B, &mut [], true, false)
    else {
        panic!("expected 0-byte peek with fds");
    };
    assert_eq!(bytes, 0);
    assert_eq!(sender, c1);
    assert!(!wake_writer);
    assert_eq!(ancillary.unwrap().fds(), &[7, 8]);
    assert_eq!(pair.in_flight_fds(End::A), 2);

    let StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer } = pair.recv(End::B, &mut [], false, false)
    else {
        panic!("expected 0-byte recv detaching fds");
    };
    assert_eq!(bytes, 0);
    assert_eq!(sender, c1);
    assert!(!wake_writer);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![7, 8])));
    assert_eq!(pair.in_flight_fds(End::A), 0);
    assert_eq!(pair.readable_bytes(End::B), 2);

    // Bytes are still queued and read out next without fds.
    let mut out = [0u8; 8];
    let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut out, false, false) else {
        panic!("expected remaining bytes");
    };
    assert_eq!(&out[..bytes], b"hi");
    assert!(ancillary.is_none());

    // Empty queue with Reset pending: read(&mut []) -> ZeroBuffer (keeps Reset),
    // recv(&mut []) -> Reset once (clears sk_err), then Eof!
    let _ = pair.close_reset(End::A);
    assert_eq!(pair.read(End::B, &mut [], false), StreamRecvOutcome::ZeroBuffer);
    assert!(pair.err(End::B));
    assert_eq!(pair.recv(End::B, &mut [], false, false), StreamRecvOutcome::Reset);
    assert!(!pair.err(End::B));
    assert_eq!(pair.recv(End::B, &mut [], false, false), StreamRecvOutcome::Eof);
}

#[test]
fn stream_peek_does_not_consume() {
    let c1 = Ucred::new(7, 0, 0);
    let mut pair = StreamPair::<u32>::try_new(1024).unwrap();
    assert_eq!(pair.send(End::A, b"pre-", c1, None), StreamSendOutcome::Sent(4));
    assert_eq!(pair.send(End::A, b"hello", c1, Some(Ancillary::from_fds(vec![5, 6]))), StreamSendOutcome::Sent(5));
    assert_eq!(pair.send(End::A, b"world", c1, Some(Ancillary::from_fds(vec![7]))), StreamSendOutcome::Sent(5));

    let mut p1 = [0u8; 16];
    let mut p2 = [0u8; 16];
    for dst in [&mut p1, &mut p2] {
        let StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer } = pair.recv(End::B, dst, true, false)
        else {
            panic!("expected peek data");
        };
        assert_eq!(&dst[..bytes], b"pre-hello");
        assert_eq!(sender, c1);
        assert!(!wake_writer);
        let anc = ancillary.expect("expected peeked ancillary from segment 1");
        assert!(anc.is_peeked());
        assert_eq!(anc.fds(), &[5, 6]);
    }

    assert_eq!(pair.readable_bytes(End::B), 14);
    assert_eq!(pair.bytes_in_flight(End::A), 14);
    assert_eq!(pair.in_flight_fds(End::A), 3);

    let mut out = [0u8; 16];
    let StreamRecvOutcome::Data { bytes, sender, ancillary, .. } = pair.recv(End::B, &mut out, false, false) else {
        panic!("expected consuming data");
    };
    assert_eq!(&out[..bytes], b"pre-hello");
    assert_eq!(sender, c1);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![5, 6])));
    assert_eq!(pair.readable_bytes(End::B), 5);
    assert_eq!(pair.in_flight_fds(End::A), 1);

    let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut out, false, false) else {
        panic!("expected second segment");
    };
    assert_eq!(&out[..bytes], b"world");
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![7])));
    assert_eq!(pair.in_flight_fds(End::A), 0);
}

#[test]
fn stream_shutdown_eof_epipe_quarter_writable_and_readiness() {
    // Linux v6.12 net/unix/af_unix.c unix_shutdown, unix_poll, unix_writable:
    // - unix_writable: `(wmem << 2) <= sndbuf` ONLY (no `send_shutdown ||` term);
    // - readable: `!skb_queue_empty || (sk_shutdown & RCV_SHUTDOWN)` (no `|| err`);
    // - SHUT_WR on A sets A's SEND_SHUTDOWN and B's RCV_SHUTDOWN (POLLRDHUP | POLLIN, !POLLHUP);
    // - SHUT_RD on B sets B's RCV_SHUTDOWN and A's SEND_SHUTDOWN (EPIPE on A);
    // - SHUT_RDWR or both sides SHUT_WR sets SHUTDOWN_MASK (POLLHUP).
    let c1 = Ucred::new(1, 0, 0);
    let mut pair = StreamPair::<u32>::try_new(400).unwrap();
    assert_eq!(
        pair.readiness(End::A),
        Readiness { readable: false, writable: true, rdhup: false, hup: false, err: false }
    );

    let mut buf = [0u8; 256];
    assert_eq!(pair.recv(End::B, &mut buf, false, false), StreamRecvOutcome::WouldBlock);

    // Send 30 bytes -> wmem = 64 + 30 = 94 <= 100 -> writable.
    assert_eq!(pair.send(End::A, &[b'a'; 30], c1, None), StreamSendOutcome::Sent(30));
    assert_eq!(pair.wmem(End::A), 94);
    assert!(pair.writable(End::A));

    // Send 10 more bytes -> wmem = 104 > 100 -> NOT writable.
    assert_eq!(pair.send(End::A, &[b'b'; 10], c1, None), StreamSendOutcome::Sent(10));
    assert_eq!(pair.wmem(End::A), 104);
    assert!(!pair.writable(End::A));

    // Reading 3 bytes leaves wmem = 101 > 100 -> wake_writer == false.
    let StreamRecvOutcome::Data { bytes, wake_writer, .. } = pair.recv(End::B, &mut buf[..3], false, false) else {
        panic!("expected 3-byte read");
    };
    assert_eq!(bytes, 3);
    assert!(!wake_writer);
    assert!(!pair.writable(End::A));

    // Reading 1 more byte drops wmem = 100 <= 100 -> wake_writer == true.
    let StreamRecvOutcome::Data { bytes, wake_writer, .. } = pair.recv(End::B, &mut buf[..1], false, false) else {
        panic!("expected 1-byte read");
    };
    assert_eq!(bytes, 1);
    assert!(wake_writer);
    assert!(pair.writable(End::A));

    // set_sndbuf wake across quarter watermark.
    assert_eq!(pair.send(End::A, &[b'c'; 20], c1, None), StreamSendOutcome::Sent(20));
    assert_eq!(pair.wmem(End::A), 120);
    assert!(!pair.writable(End::A));
    assert_eq!(pair.set_sndbuf(End::A, 476), Ok(false));
    assert!(!pair.writable(End::A));
    assert_eq!(pair.set_sndbuf(End::A, 480), Ok(true));
    assert!(pair.writable(End::A));

    let StreamRecvOutcome::Data { bytes, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected drain");
    };
    assert_eq!(bytes, 56);

    // Queue bytes A -> B, then SHUT_WR on A.
    assert_eq!(pair.send(End::A, b"last", c1, None), StreamSendOutcome::Sent(4));
    pair.shutdown(End::A, ShutdownHow::Write);

    assert_eq!(
        pair.send(End::A, b"nope", c1, Some(Ancillary::from_fds(vec![42]))),
        StreamSendOutcome::BrokenPipe(Some(Ancillary::from_fds(vec![42])))
    );
    assert_eq!(pair.send(End::A, b"", c1, None), StreamSendOutcome::BrokenPipe(None));

    assert_eq!(
        pair.readiness(End::B),
        Readiness { readable: true, writable: true, rdhup: true, hup: false, err: false }
    );
    assert_eq!(pair.send(End::B, b"reply", c1, None), StreamSendOutcome::Sent(5));

    let StreamRecvOutcome::Data { bytes, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected queued data before EOF");
    };
    assert_eq!(&buf[..bytes], b"last");
    assert_eq!(pair.recv(End::B, &mut buf, false, false), StreamRecvOutcome::Eof);
    assert!(pair.readable(End::B));
    assert!(pair.rdhup(End::B));
    assert!(!pair.hup(End::B));

    // Both ends SHUT_WR -> POLLHUP on both.
    pair.shutdown(End::B, ShutdownHow::Write);
    assert!(pair.hup(End::A));
    assert!(pair.hup(End::B));
    assert_eq!(pair.send(End::B, b"x", c1, None), StreamSendOutcome::BrokenPipe(None));

    let StreamRecvOutcome::Data { bytes, .. } = pair.recv(End::A, &mut buf, false, false) else {
        panic!("expected reply before EOF");
    };
    assert_eq!(&buf[..bytes], b"reply");
    assert_eq!(pair.recv(End::A, &mut buf, false, false), StreamRecvOutcome::Eof);

    // Linux unix_poll / unix_writable: SHUT_RD on B does NOT make A writable
    // while A's send buffer is above the quarter watermark! Draining A->B wakes A.
    let mut pair2 = StreamPair::<u32>::try_new(100).unwrap();
    assert_eq!(pair2.send(End::A, &[0xAA; 36], c1, None), StreamSendOutcome::Sent(36));
    assert!(!pair2.writable(End::A));
    pair2.shutdown(End::B, ShutdownHow::Read);
    assert!(!pair2.writable(End::A), "unix_writable depends only on wmem << 2 <= sndbuf");
    assert_eq!(pair2.send(End::A, b"z", c1, None), StreamSendOutcome::BrokenPipe(None));
    let StreamRecvOutcome::Data { bytes, wake_writer, .. } = pair2.recv(End::B, &mut buf, false, false) else {
        panic!("expected pre-shutdown bytes");
    };
    assert_eq!(bytes, 36);
    assert!(wake_writer, "draining wmem below sndbuf/4 triggers wake_writer even after send_shutdown");
    assert!(pair2.writable(End::A));
    assert_eq!(pair2.recv(End::B, &mut buf, false, false), StreamRecvOutcome::Eof);

    // SHUT_RDWR sets hup and rdhup on both ends.
    let mut pair3 = StreamPair::<u32>::try_new(128).unwrap();
    pair3.shutdown(End::A, ShutdownHow::Both);
    assert!(pair3.hup(End::A));
    assert!(pair3.hup(End::B));
    assert!(pair3.rdhup(End::A));
    assert!(pair3.rdhup(End::B));
}

#[test]
fn stream_lazy_buffer_segment_overhead_and_growth_protocols() {
    let c1 = Ucred::new(1, 0, 0);
    let c2 = Ucred::new(2, 0, 0);

    // Default-sized StreamPair starts lazily at DEFAULT_INITIAL_BUF (4096), not 212_992.
    let mut big = StreamPair::<u32>::try_new(DEFAULT_SNDBUF).unwrap();
    assert_eq!(big.sndbuf(End::A), DEFAULT_SNDBUF);
    assert_eq!(big.buffer_capacity(End::A), DEFAULT_INITIAL_BUF);

    // Fill the lazy 4096-byte ring; next write hits NeedBuffer (not WouldBlock).
    let chunk = vec![0x55u8; DEFAULT_INITIAL_BUF];
    assert_eq!(big.send(End::A, &chunk, c1, None), StreamSendOutcome::Sent(DEFAULT_INITIAL_BUF));
    assert_eq!(big.send(End::A, b"more", c1, None), StreamSendOutcome::NeedBuffer(None));

    let next_buf = big.next_buffer_capacity(End::A, 4);
    assert!(next_buf > DEFAULT_INITIAL_BUF);
    let spare = ByteStorage::try_with_capacity(next_buf).unwrap();
    let old = big.grow_buffer(End::A, spare);
    assert_eq!(old.capacity(), DEFAULT_INITIAL_BUF);
    assert_eq!(big.send(End::A, b"more", c1, None), StreamSendOutcome::Sent(4));
    assert_eq!(big.readable_bytes(End::B), DEFAULT_INITIAL_BUF + 4);

    // Per-segment overhead bounds 1-byte non-coalescing writes:
    // sndbuf = 200 -> 3 segments * 65 = 195, 4th -> WouldBlock.
    let mut ov = StreamPair::<u32>::try_new(200).unwrap();
    assert_eq!(ov.send(End::A, b"a", c1, None), StreamSendOutcome::Sent(1));
    assert_eq!(ov.send(End::A, b"b", c2, None), StreamSendOutcome::Sent(1));
    assert_eq!(ov.send(End::A, b"c", c1, None), StreamSendOutcome::Sent(1));
    assert_eq!(ov.wmem(End::A), 3 * (SEGMENT_OVERHEAD + 1));
    assert_eq!(
        ov.send(End::A, b"d", c2, Some(Ancillary::from_fds(vec![9]))),
        StreamSendOutcome::WouldBlock(Some(Ancillary::from_fds(vec![9])))
    );

    // Trigger NeedSegment when segment ring fills.
    let mut segp = StreamPair::<u32>::try_new(4096).unwrap();
    let mut pushed = 0usize;
    let mut hit_need = false;
    for i in 0..40u32 {
        match segp.send(End::A, &[b'a' + (i as u8 % 26)], c1, Some(Ancillary::from_fds(vec![i]))) {
            StreamSendOutcome::Sent(1) => pushed += 1,
            StreamSendOutcome::NeedSegment(Some(anc)) => {
                assert_eq!(anc, Ancillary::from_fds(vec![i]));
                let next_cap = segp.next_segment_capacity(End::A);
                assert!(next_cap > pushed);
                let spare = SegmentStorage::try_with_capacity(next_cap).unwrap();
                let _old = segp.grow_segments(End::A, spare);
                assert_eq!(segp.send(End::A, &[b'a' + (i as u8 % 26)], c1, Some(anc)), StreamSendOutcome::Sent(1));
                pushed += 1;
                hit_need = true;
                break;
            }
            other => panic!("unexpected send outcome: {other:?}"),
        }
    }
    assert!(hit_need, "expected NeedSegment once initial segment capacity filled");
    assert_eq!(segp.in_flight_fds(End::A), pushed);

    // Empty write without ancillary -> Sent(0); empty write with ancillary -> EmptyWithAncillary.
    let mut p = StreamPair::<u32>::try_new(128).unwrap();
    assert_eq!(p.send(End::A, b"", c1, None), StreamSendOutcome::Sent(0));
    assert_eq!(
        p.send(End::A, b"", c1, Some(Ancillary::from_fds(vec![1]))),
        StreamSendOutcome::EmptyWithAncillary(Ancillary::from_fds(vec![1]))
    );
    assert_eq!(p.send(End::A, b"", c1, Some(Ancillary::default())), StreamSendOutcome::Sent(0));

    // Invalid sndbuf (< MIN_STREAM_SNDBUF) rejected cleanly.
    assert_eq!(StreamPair::<u32>::try_new(0).unwrap_err(), SetSndbufError::Invalid);
    assert_eq!(StreamPair::<u32>::try_new(SEGMENT_OVERHEAD).unwrap_err(), SetSndbufError::Invalid);
    assert_eq!(p.set_sndbuf(End::A, SEGMENT_OVERHEAD), Err(SetSndbufError::Invalid));

    // Grow buffer while ring bytes are wrapped around the end.
    let mut wrap = StreamPair::<u32>::try_new(80).unwrap();
    assert_eq!(wrap.buffer_capacity(End::A), 16);
    assert_eq!(wrap.send(End::A, b"0123456789AB", c1, None), StreamSendOutcome::Sent(12));
    let mut out = [0u8; 32];
    let StreamRecvOutcome::Data { bytes, .. } = wrap.recv(End::B, &mut out[..10], false, false) else {
        panic!("expected advance read");
    };
    assert_eq!(bytes, 10);
    assert_eq!(wrap.send(End::A, b"CDEFGHIJKL", c1, None), StreamSendOutcome::Sent(10));
    // Raise sndbuf past 4 * wmem (4 * 76 = 304 -> wake = true), then grow buffer via swap while wrapped.
    assert_eq!(wrap.set_sndbuf(End::A, 320), Ok(true));
    let _old = wrap.grow_buffer(End::A, ByteStorage::try_with_capacity(64).unwrap());
    let StreamRecvOutcome::Data { bytes, .. } = wrap.recv(End::B, &mut out, false, false) else {
        panic!("expected linearized wrapped bytes");
    };
    assert_eq!(&out[..bytes], b"ABCDEFGHIJKL");
}

#[test]
fn stream_close_resets_purge_recv_gc_hooks_and_undelivered_fds() {
    // Linux v6.12 net/unix/af_unix.c unix_release_sock & unix_stream_read_generic:
    // - closing an end with unread bytes sets peer's ECONNRESET;
    // - peer drains its queue first, then sock_error() clears and returns Reset
    //   ONCE regardless of MSG_PEEK, then Eof;
    // - StreamPair::purge_recv empties an end's receive queue without shutdown,
    //   closed, or reset state (unix_gc / unix_collect_skb).
    let c1 = Ucred::new(1, 0, 0);
    let mut pair = StreamPair::<u32>::try_new(1024).unwrap();
    assert_eq!(pair.send(End::A, b"for-b", c1, Some(Ancillary::from_fds(vec![1, 2]))), StreamSendOutcome::Sent(5));
    assert_eq!(pair.send(End::B, b"for-a1", c1, Some(Ancillary::from_fds(vec![10]))), StreamSendOutcome::Sent(6));
    assert_eq!(pair.send(End::B, b"for-a2", c1, Some(Ancillary::from_fds(vec![20, 30]))), StreamSendOutcome::Sent(6));
    assert_eq!(pair.total_in_flight_fds(), 5);

    // Borrowing GC scan visits queued fds per reader end without draining.
    let mut b_queued = Vec::new();
    pair.for_each_queued_fd(End::B, |&fd| b_queued.push(fd));
    assert_eq!(b_queued, vec![1, 2]);
    let mut a_queued = Vec::new();
    pair.for_each_queued_fd(End::A, |&fd| a_queued.push(fd));
    assert_eq!(a_queued, vec![10, 20, 30]);
    assert_eq!(pair.total_in_flight_fds(), 5);

    // Manual Debug must not dump queued user payloads.
    let dbg = format!("{pair:?}");
    assert!(dbg.contains("a_to_b_fds: 2"), "{dbg}");
    assert!(dbg.contains("b_to_a_fds: 3"), "{dbg}");
    assert!(!dbg.contains("for-b"), "Debug leaked payload: {dbg}");

    // Closing A while A has unread bytes from B -> had_unread_bytes() == true,
    // hands back [10, 20, 30] in FIFO order (O(1) pop_fd), and sets Reset on B!
    let mut undel_a = pair.close(End::A);
    assert!(undel_a.had_unread_bytes());
    assert_eq!(undel_a.unread_bytes(), 12);
    assert_eq!(undel_a.fd_count(), 3);
    assert!(!undel_a.is_empty());
    let mut closed_fds = Vec::new();
    while let Some(fd) = undel_a.pop_fd() {
        closed_fds.push(fd);
    }
    assert_eq!(closed_fds, vec![10, 20, 30]);
    assert!(undel_a.is_empty());
    assert!(pair.close(End::A).is_empty());

    // B sees err == true, hup == true, rdhup == true, and drains "for-b" FIRST!
    assert!(pair.err(End::B));
    assert!(pair.readiness(End::B).err);
    assert!(pair.hup(End::B));
    assert_eq!(pair.send(End::B, b"z", c1, None), StreamSendOutcome::BrokenPipe(None));

    let mut buf = [0u8; 16];
    let StreamRecvOutcome::Data { bytes, ancillary, .. } = pair.recv(End::B, &mut buf, false, false) else {
        panic!("expected B to drain queued bytes before Reset");
    };
    assert_eq!(&buf[..bytes], b"for-b");
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![1, 2])));

    // Linux sock_error(sk) xchg runs before MSG_PEEK: a peek clears Reset once,
    // so the subsequent recv observes Eof!
    assert_eq!(pair.recv(End::B, &mut buf, true, false), StreamRecvOutcome::Reset);
    assert!(!pair.err(End::B));
    assert_eq!(pair.recv(End::B, &mut buf, false, false), StreamRecvOutcome::Eof);

    // Closing A with NO unread bytes -> had_unread_bytes() == false, no Reset on B.
    let mut clean = StreamPair::<u32>::try_new(256).unwrap();
    assert_eq!(clean.send(End::A, b"ok", c1, None), StreamSendOutcome::Sent(2));
    let undel = clean.close(End::A);
    assert!(!undel.had_unread_bytes());
    assert!(undel.is_empty());
    assert!(!clean.err(End::B));
    let StreamRecvOutcome::Data { bytes, .. } = clean.recv(End::B, &mut buf, false, false) else {
        panic!("expected ok");
    };
    assert_eq!(&buf[..bytes], b"ok");
    assert_eq!(clean.recv(End::B, &mut buf, false, false), StreamRecvOutcome::Eof);

    // Closing A with unread PLAIN bytes -> had_unread_bytes() == true, is_empty() == true, Reset on B.
    let mut plain_unread = StreamPair::<u32>::try_new(256).unwrap();
    assert_eq!(plain_unread.send(End::B, b"unread-plain", c1, None), StreamSendOutcome::Sent(12));
    let undel = plain_unread.close(End::A);
    assert!(undel.had_unread_bytes());
    assert_eq!(undel.unread_bytes(), 12);
    assert!(undel.is_empty(), "plain bytes have no ancillary items");
    assert!(plain_unread.err(End::B));
    assert!(plain_unread.take_error(End::B));
    assert!(!plain_unread.take_error(End::B));
    assert!(!plain_unread.err(End::B));
    assert_eq!(plain_unread.recv(End::B, &mut buf, false, false), StreamRecvOutcome::Eof);

    // `close_reset` (unaccepted embrion on closed listener): sets Reset even with 0 unread bytes.
    let mut embrion = StreamPair::<u32>::try_new(256).unwrap();
    let undel = embrion.close_reset(End::B);
    assert!(!undel.had_unread_bytes());
    assert!(embrion.err(End::A));
    assert_eq!(embrion.recv(End::A, &mut buf, false, false), StreamRecvOutcome::Reset);
    assert_eq!(embrion.recv(End::A, &mut buf, false, false), StreamRecvOutcome::Eof);

    // `StreamPair::purge_recv` (GC unix_collect_skb): empties receive queue without
    // setting shutdown, closed, or reset state; peer can keep sending afterwards.
    let mut gcp = StreamPair::<u32>::try_new(512).unwrap();
    assert_eq!(gcp.send(End::A, b"cyc1", c1, Some(Ancillary::from_fds(vec![1, 2, 3]))), StreamSendOutcome::Sent(4));
    assert_eq!(gcp.send(End::A, b"cyc2", c1, Some(Ancillary::from_fds(vec![4]))), StreamSendOutcome::Sent(4));
    let mut purged = gcp.purge_recv(End::B);
    assert!(purged.had_unread_bytes());
    assert_eq!(purged.unread_bytes(), 8);
    assert_eq!(purged.fd_count(), 4);
    // Mix pop_fd and pop_next to verify FIFO preservation after internal reverse!
    assert_eq!(purged.pop_fd(), Some(1));
    assert_eq!(purged.pop_next(), Some(Ancillary::from_fds(vec![2, 3])));
    assert_eq!(purged.pop_fd(), Some(4));
    assert!(purged.is_empty());

    assert!(!gcp.is_closed(End::A) && !gcp.is_closed(End::B));
    assert!(!gcp.rdhup(End::A) && !gcp.rdhup(End::B));
    assert!(!gcp.hup(End::A) && !gcp.hup(End::B));
    assert!(!gcp.err(End::A) && !gcp.err(End::B));
    assert_eq!(gcp.readable_bytes(End::B), 0);
    assert_eq!(gcp.in_flight_fds(End::A), 0);
    assert_eq!(stream_send_growing(&mut gcp, End::A, b"after-purge", c1, None), StreamSendOutcome::Sent(11));
    let StreamRecvOutcome::Data { bytes, .. } = gcp.recv(End::B, &mut buf, false, false) else {
        panic!("expected post-purge data");
    };
    assert_eq!(&buf[..bytes], b"after-purge");
}

// Reference model for `StreamPair`:
// - Recv loop (`unix_stream_read_generic` ~2866-2931, incl. 0-byte target 1,
//   stop-after-fd, `check_creds` writer split, and `sock_error` clearing Reset
//   even on `MSG_PEEK`), shutdown/close/reset, and `unix_poll` / `unix_writable`
//   readiness are derived from Linux v6.12 `net/unix/af_unix.c`.
// - Each accepted send is pushed as its own unmerged `LinuxSkb` (Linux never
//   coalesces skbs on send); send admission is driven by `StreamPair`'s strict
//   cap + coalescing so the test verifies observational equivalence of bytes,
//   boundaries, creds, fds, resets, and readiness.
#[derive(Debug)]
struct ModelRecv {
    bytes: Vec<u8>,
    sender: Ucred,
    fds: Vec<u32>,
}

#[derive(Clone, Debug)]
struct LinuxSkb {
    bytes: VecDeque<u8>,
    sender: Ucred,
    fds: Vec<u32>,
}

#[derive(Default)]
struct ModelEnd {
    rcv_shut: bool,
    snd_shut: bool,
    closed: bool,
    err: bool,
}

struct LinuxDir {
    skbs: VecDeque<LinuxSkb>,
}

impl LinuxDir {
    fn new() -> Self {
        Self { skbs: VecDeque::new() }
    }

    fn payload_bytes(&self) -> usize {
        self.skbs.iter().map(|s| s.bytes.len()).sum()
    }

    fn in_flight_fds(&self) -> usize {
        self.skbs.iter().map(|s| s.fds.len()).sum()
    }
}

struct LinuxStreamModel {
    a_to_b: LinuxDir,
    b_to_a: LinuxDir,
    end_a: ModelEnd,
    end_b: ModelEnd,
}

impl LinuxStreamModel {
    fn new() -> Self {
        Self {
            a_to_b: LinuxDir::new(),
            b_to_a: LinuxDir::new(),
            end_a: ModelEnd::default(),
            end_b: ModelEnd::default(),
        }
    }

    fn end(&self, e: End) -> &ModelEnd {
        match e {
            End::A => &self.end_a,
            End::B => &self.end_b,
        }
    }

    fn end_mut(&mut self, e: End) -> &mut ModelEnd {
        match e {
            End::A => &mut self.end_a,
            End::B => &mut self.end_b,
        }
    }

    fn push_skb(&mut self, end: End, data: &[u8], sender: Ucred, anc: Option<Ancillary<u32>>) {
        let anc = anc.filter(|a| !a.is_empty());
        let effective_sender = anc.as_ref().and_then(|a| a.cred).unwrap_or(sender);
        let fds = anc.map(|a| a.fds).unwrap_or_default();
        let dir = match end {
            End::A => &mut self.a_to_b,
            End::B => &mut self.b_to_a,
        };
        // Linux never merges skbs on send: always one skb per send.
        dir.skbs.push_back(LinuxSkb { bytes: data.iter().copied().collect(), sender: effective_sender, fds });
    }

    // Linux v6.12 net/unix/af_unix.c unix_stream_read_generic (~2866-2931):
    // - empty queue: sock_error() clears and returns Reset regardless of peek,
    //   then Eof on RCV_SHUTDOWN, else WouldBlock;
    // - max == 0 runs once with target 1: detaches/peeks head skb's fds with 0 bytes;
    // - max > 0 loops over skbs, breaks on writer change under check_creds, and
    //   breaks immediately after an skb carrying fds (`if (scm.fp) break;`).
    fn recv(&mut self, end: End, max: usize, peek: bool, check_creds: bool) -> Result<ModelRecv, &'static str> {
        let (st, dir) = match end {
            End::A => (&mut self.end_a, &mut self.b_to_a),
            End::B => (&mut self.end_b, &mut self.a_to_b),
        };
        if dir.skbs.is_empty() {
            if st.err {
                st.err = false;
                return Err("reset");
            }
            return if st.rcv_shut { Err("eof") } else { Err("wouldblock") };
        }

        let start_sender = dir.skbs.front().unwrap().sender;
        if max == 0 {
            let got_fds = if peek {
                dir.skbs.front().unwrap().fds.clone()
            } else {
                core::mem::take(&mut dir.skbs.front_mut().unwrap().fds)
            };
            return Ok(ModelRecv { bytes: Vec::new(), sender: start_sender, fds: got_fds });
        }

        let mut out = Vec::new();
        let mut got_fds = Vec::new();
        let mut idx = 0usize;

        while out.len() < max {
            let skb = if peek {
                match dir.skbs.get(idx) {
                    Some(s) => s,
                    None => break,
                }
            } else {
                match dir.skbs.front() {
                    Some(s) => s,
                    None => break,
                }
            };
            if check_creds && !out.is_empty() && skb.sender != start_sender {
                break;
            }
            let take = (max - out.len()).min(skb.bytes.len());
            if peek {
                out.extend(skb.bytes.iter().take(take).copied());
                if !skb.fds.is_empty() {
                    got_fds = skb.fds.clone();
                    break;
                }
                idx += 1;
            } else {
                let front = dir.skbs.front_mut().unwrap();
                for _ in 0..take {
                    out.push(front.bytes.pop_front().unwrap());
                }
                let had_fds = !front.fds.is_empty();
                if had_fds {
                    got_fds = core::mem::take(&mut front.fds);
                }
                if front.bytes.is_empty() {
                    dir.skbs.pop_front();
                }
                if had_fds {
                    break;
                }
            }
        }

        Ok(ModelRecv { bytes: out, sender: start_sender, fds: got_fds })
    }

    fn shutdown(&mut self, end: End, how: ShutdownHow) {
        if how.shuts_read() {
            self.end_mut(end).rcv_shut = true;
            self.end_mut(end.peer()).snd_shut = true;
        }
        if how.shuts_write() {
            self.end_mut(end).snd_shut = true;
            self.end_mut(end.peer()).rcv_shut = true;
        }
    }

    fn close(&mut self, end: End, force_reset: bool) -> (Vec<u32>, bool) {
        let already = self.end(end).closed;
        self.end_mut(end).closed = true;
        self.shutdown(end, ShutdownHow::Both);
        if already {
            return (Vec::new(), false);
        }
        let (dir, peer_st) = match end {
            End::A => (&mut self.b_to_a, &mut self.end_b),
            End::B => (&mut self.a_to_b, &mut self.end_a),
        };
        let had_unread = dir.payload_bytes() > 0;
        if had_unread || force_reset {
            peer_st.err = true;
        }
        let mut fds = Vec::new();
        while let Some(skb) = dir.skbs.pop_front() {
            fds.extend(skb.fds);
        }
        (fds, had_unread)
    }

    // Derived directly from Linux v6.12 net/unix/af_unix.c unix_poll + unix_writable.
    fn readiness(&self, end: End, wmem: usize, sndbuf: usize) -> Readiness {
        let st = self.end(end);
        let rdir = match end {
            End::A => &self.b_to_a,
            End::B => &self.a_to_b,
        };
        Readiness {
            readable: rdir.payload_bytes() > 0 || st.rcv_shut,
            writable: wmem <= sndbuf / 4,
            rdhup: st.rcv_shut,
            hup: st.rcv_shut && st.snd_shut,
            err: st.err,
        }
    }
}

#[test]
fn randomized_stream_pair_matches_linux_model() {
    let creds = [Ucred::new(1, 100, 100), Ucred::new(2, 200, 200), Ucred::new(3, 0, 0)];
    let configs = [(80usize, 400usize, true), (160, 800, false), (512, 1200, false), (200, 300, true)];

    for (run, &(init_sndbuf, steps, early_shutdown)) in configs.iter().enumerate() {
        let mut rng = SplitMix64(0xCAFE_BABE_0000_0001 ^ (run as u64).wrapping_mul(0x9E37_79B9));
        let mut pair = StreamPair::<u32>::try_new(init_sndbuf).unwrap();
        let mut model = LinuxStreamModel::new();
        let mut next_fd = 1u32;
        let mut sent_fds = Vec::new();
        let mut received_fds = Vec::new();
        let mut closed_fds = Vec::new();
        let mut byte_seq = 0u8;

        for step in 0..steps {
            let end = if rng.gen_range(2) == 0 { End::A } else { End::B };
            let op = rng.gen_range(100);

            if op < 42 {
                let len = rng.gen_range(40);
                let mut payload = Vec::with_capacity(len);
                for _ in 0..len {
                    payload.push(byte_seq);
                    byte_seq = byte_seq.wrapping_add(1);
                }
                let sender = creds[rng.gen_range(creds.len())];
                let anc = match rng.gen_range(5) {
                    0 | 1 => None,
                    2 => {
                        let k = 1 + rng.gen_range(3);
                        let fds: Vec<u32> = (0..k)
                            .map(|_| {
                                let id = next_fd;
                                next_fd += 1;
                                id
                            })
                            .collect();
                        Some(Ancillary::from_fds(fds))
                    }
                    3 => {
                        let k = 1 + rng.gen_range(2);
                        let fds: Vec<u32> = (0..k)
                            .map(|_| {
                                let id = next_fd;
                                next_fd += 1;
                                id
                            })
                            .collect();
                        Some(Ancillary::new(fds, Some(creds[rng.gen_range(creds.len())])))
                    }
                    _ => Some(Ancillary::from_cred(creds[rng.gen_range(creds.len())])),
                };

                let snd_shut = model.end(end).snd_shut;
                let p_res = stream_send_growing(&mut pair, end, &payload, sender, anc.clone());
                match p_res {
                    StreamSendOutcome::Sent(pn) => {
                        assert!(!snd_shut, "run {run} step {step}: Sent after send_shutdown");
                        if pn > 0 {
                            if let Some(ref a) = anc {
                                sent_fds.extend(&a.fds);
                            }
                            model.push_skb(end, &payload[..pn], sender, anc);
                        } else {
                            assert!(payload.is_empty());
                            assert!(anc.as_ref().is_none_or(Ancillary::is_empty));
                        }
                    }
                    StreamSendOutcome::BrokenPipe(ret) => {
                        assert!(snd_shut, "run {run} step {step}: unexpected BrokenPipe");
                        assert_eq!(ret, anc.filter(|a| !a.is_empty()));
                    }
                    StreamSendOutcome::WouldBlock(ret) => {
                        assert!(!snd_shut && !payload.is_empty());
                        assert_eq!(ret, anc.filter(|a| !a.is_empty()));
                    }
                    StreamSendOutcome::EmptyWithAncillary(ret) => {
                        assert!(!snd_shut && payload.is_empty());
                        assert_eq!(Some(ret), anc.filter(|a| !a.is_empty()));
                    }
                    other => panic!("run {run} step {step}: unexpected send outcome {other:?}"),
                }
            } else if op < 88 {
                let buf_len = rng.gen_range(48);
                let check_creds = rng.gen_range(2) == 0;
                let mut pbuf = vec![0u8; buf_len];

                let m_peek = model.recv(end, buf_len, true, check_creds);
                let p_peek = pair.recv(end, &mut pbuf, true, check_creds);
                match (&m_peek, p_peek) {
                    (
                        Ok(ModelRecv { bytes: mb, sender: ms, fds: mfds }),
                        StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer },
                    ) => {
                        assert!(!wake_writer);
                        assert_eq!(&pbuf[..bytes], mb.as_slice(), "run {run} step {step}: peek bytes");
                        assert_eq!(sender, *ms, "run {run} step {step}: peek sender");
                        let pfds: &[u32] = ancillary.as_ref().map_or(&[], RecvAncillary::fds);
                        assert_eq!(pfds, mfds.as_slice(), "run {run} step {step}: peek fds");
                    }
                    (Err("reset"), StreamRecvOutcome::Reset)
                    | (Err("eof"), StreamRecvOutcome::Eof)
                    | (Err("wouldblock"), StreamRecvOutcome::WouldBlock) => {}
                    (m, p) => panic!("run {run} step {step}: peek mismatch model={m:?} pair={p:?}"),
                }

                if rng.gen_range(4) != 0 {
                    let peer = end.peer();
                    let was_w = pair.wmem(peer) <= pair.sndbuf(peer) / 4;
                    let mut rbuf = vec![0u8; buf_len];
                    let m_recv = model.recv(end, buf_len, false, check_creds);
                    let p_recv = pair.recv(end, &mut rbuf, false, check_creds);
                    let wake_seen = match (m_recv, p_recv) {
                        (
                            Ok(ModelRecv { bytes: mb, sender: ms, fds: mfds }),
                            StreamRecvOutcome::Data { bytes, sender, ancillary, wake_writer },
                        ) => {
                            assert_eq!(&rbuf[..bytes], mb.as_slice(), "run {run} step {step}: recv bytes");
                            assert_eq!(sender, ms, "run {run} step {step}: recv sender");
                            let owned = ancillary.and_then(RecvAncillary::into_owned);
                            let pfds = owned.map(|a| a.fds).unwrap_or_default();
                            assert_eq!(pfds, mfds, "run {run} step {step}: recv fds");
                            received_fds.extend(pfds);
                            Some(wake_writer)
                        }
                        (Err("reset"), StreamRecvOutcome::Reset)
                        | (Err("eof"), StreamRecvOutcome::Eof)
                        | (Err("wouldblock"), StreamRecvOutcome::WouldBlock) => None,
                        (m, p) => panic!("run {run} step {step}: recv mismatch model={m:?} pair={p:?}"),
                    };
                    if let Some(wake_writer) = wake_seen {
                        let now_w = pair.wmem(peer) <= pair.sndbuf(peer) / 4;
                        assert_eq!(wake_writer, !was_w && now_w, "run {run} step {step}: recv wake_writer");
                    }
                }
            } else if op < 95 {
                let new_sz = match rng.gen_range(5) {
                    0 => 0,
                    1 => MIN_STREAM_SNDBUF,
                    2 => 128,
                    3 => 320,
                    _ => 1024,
                };
                let was_w = pair.wmem(end) <= pair.sndbuf(end) / 4;
                let p_res = pair.set_sndbuf(end, new_sz);
                if new_sz < MIN_STREAM_SNDBUF {
                    assert_eq!(p_res, Err(SetSndbufError::Invalid));
                } else {
                    let now_w = pair.wmem(end) <= new_sz / 4;
                    assert_eq!(p_res, Ok(!was_w && now_w), "run {run} step {step}: set_sndbuf wake");
                }
            } else if op < 98 && (early_shutdown || step > steps - 80) {
                let how = match rng.gen_range(3) {
                    0 => ShutdownHow::Read,
                    1 => ShutdownHow::Write,
                    _ => ShutdownHow::Both,
                };
                model.shutdown(end, how);
                pair.shutdown(end, how);
            } else if early_shutdown || step > steps - 40 {
                let force_reset = rng.gen_range(2) == 0;
                let (mfds, m_unread) = model.close(end, force_reset);
                let undel = if force_reset { pair.close_reset(end) } else { pair.close(end) };
                assert_eq!(undel.had_unread_bytes(), m_unread, "run {run} step {step}: had_unread_bytes");
                let pfds: Vec<u32> = undel.flat_map(|mut a| a.drain_fds().collect::<Vec<_>>()).collect();
                assert_eq!(pfds, mfds, "run {run} step {step}: close fds");
                closed_fds.extend(pfds);
            }

            // Cross-check readiness, payload bytes, wmem formula, coalescing bound, and in-flight fds.
            for e in [End::A, End::B] {
                let expected_r = model.readiness(e, pair.wmem(e), pair.sndbuf(e));
                assert_eq!(pair.readiness(e), expected_r, "run {run} step {step}: readiness({e:?})");
                let m_dir = match e {
                    End::A => &model.a_to_b,
                    End::B => &model.b_to_a,
                };
                assert_eq!(pair.bytes_in_flight(e), m_dir.payload_bytes(), "run {run} step {step}: bytes({e:?})");
                assert_eq!(pair.in_flight_fds(e), m_dir.in_flight_fds(), "run {run} step {step}: fds({e:?})");
                assert_eq!(pair.wmem(e), pair.bytes_in_flight(e) + pair.segment_count(e) * SEGMENT_OVERHEAD);
                assert!(pair.segment_count(e) <= m_dir.skbs.len());
            }
        }

        for e in [End::A, End::B] {
            let (mfds, _) = model.close(e, false);
            let pfds: Vec<u32> = pair.close(e).flat_map(|mut a| a.drain_fds().collect::<Vec<_>>()).collect();
            assert_eq!(pfds, mfds);
            closed_fds.extend(pfds);
        }
        assert_eq!(pair.total_in_flight_fds(), 0);
        let mut accounted: Vec<u32> = received_fds.into_iter().chain(closed_fds).collect();
        accounted.sort_unstable();
        sent_fds.sort_unstable();
        assert_eq!(accounted, sent_fds, "run {run}: fd conservation failed");
    }
}

#[test]
fn dgram_boundaries_creds_msg_trunc_peek_zero_len_and_gc_hook() {
    // Linux v6.12 net/unix/af_unix.c unix_dgram_sendmsg, unix_scm_to_skb, unix_dgram_recvmsg:
    // - preserves message boundaries including 0-byte datagrams (with or without SCM_RIGHTS);
    // - records required writer cred Ucred (overridden by explicit SCM_CREDENTIALS) and
    //   reports it on every recv (reused by SOCK_SEQPACKET too);
    // - MSG_TRUNC reports original full_len and sets truncated when full_len > buf.len();
    // - MSG_PEEK | MSG_TRUNC inspects length without consuming.
    let c1 = Ucred::new(10, 1000, 1000);
    let c2 = Ucred::new(20, 2000, 2000);
    let c_override = Ucred::new(99, 0, 0);

    let mut q = DgramQueue::<u32, &'static str, u64>::try_new(8).unwrap();
    assert_eq!(q.fionread(), 0);
    assert_eq!(q.recv(&mut [0u8; 8], false), DgramRecvOutcome::WouldBlock);

    assert_eq!(
        q.send(mk_dgram(b"hello world", Some("s1"), 101, c1, Some(Ancillary::from_fds(vec![1]))), false),
        DgramSendOutcome::Sent(11)
    );
    // Explicit SCM_CREDENTIALS in ancillary overrides writer cred (unix_scm_to_skb).
    assert_eq!(
        q.send(mk_dgram(b"", Some("s2"), 102, c2, Some(Ancillary::new(vec![2], Some(c_override)))), false),
        DgramSendOutcome::Sent(0)
    );
    assert_eq!(q.send(mk_dgram(b"xy", None, 103, c2, None), false), DgramSendOutcome::Sent(2));

    assert_eq!(q.message_count(), 3);
    assert_eq!(q.bytes_in_flight(), 13);
    assert_eq!(q.in_flight_fds(), 2);
    assert_eq!(q.fionread(), 11);

    let mut gc_fds = Vec::new();
    q.for_each_queued_fd(|&fd| gc_fds.push(fd));
    assert_eq!(gc_fds, vec![1, 2]);
    assert_eq!(q.in_flight_fds(), 2);

    // Peek with empty buffer (MSG_PEEK | MSG_TRUNC).
    let DgramRecvOutcome::Data { copied, full_len, truncated, addr, sender, cred, ancillary, wake_writers, .. } =
        q.recv(&mut [], true)
    else {
        panic!("expected peeked datagram");
    };
    assert_eq!((copied, full_len, truncated), (0, 11, true));
    assert_eq!(addr, Some(RecvValue::Peeked(&"s1")));
    assert_eq!(sender, RecvValue::Peeked(&101));
    assert_eq!(cred, c1);
    assert_eq!(ancillary.unwrap().fds(), &[1]);
    assert!(!wake_writers);
    assert_eq!(q.message_count(), 3);

    // Consume into a 5-byte buffer: copies "hello", discards " world", truncated + full_len = 11.
    let mut small = [0u8; 5];
    let DgramRecvOutcome::Data { copied, full_len, truncated, addr, sender, cred, ancillary, wake_writers, reclaimed } =
        q.recv(&mut small, false)
    else {
        panic!("expected truncated datagram");
    };
    assert_eq!(&small[..copied], b"hello");
    assert_eq!((full_len, truncated), (11, true));
    assert_eq!(addr, Some(RecvValue::Owned("s1")));
    assert_eq!(sender, RecvValue::Owned(101));
    assert_eq!(cred, c1);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::from_fds(vec![1])));
    assert!(!wake_writers);
    assert!(reclaimed.is_some());
    assert_eq!(q.in_flight_fds(), 1);
    assert_eq!(q.fionread(), 0);

    // Receive 0-byte datagram with overridden SCM_CREDENTIALS and fd (NOT EOF!).
    let DgramRecvOutcome::Data { copied, full_len, truncated, addr, sender, cred, ancillary, .. } =
        q.recv(&mut small, false)
    else {
        panic!("expected 0-byte datagram");
    };
    assert_eq!((copied, full_len, truncated), (0, 0, false));
    assert_eq!(addr, Some(RecvValue::Owned("s2")));
    assert_eq!(sender, RecvValue::Owned(102));
    assert_eq!(cred, c_override);
    assert_eq!(ancillary.unwrap().into_owned(), Some(Ancillary::new(vec![2], Some(c_override))));

    // Receive unbound third datagram (addr == None, sender handle 103, cred c2).
    let DgramRecvOutcome::Data { copied, full_len, truncated, addr, sender, cred, ancillary, .. } =
        q.recv(&mut small, false)
    else {
        panic!("expected third datagram");
    };
    assert_eq!(&small[..copied], b"xy");
    assert_eq!((full_len, truncated), (2, false));
    assert!(addr.is_none());
    assert_eq!(sender, RecvValue::Owned(103));
    assert_eq!(cred, c2);
    assert!(ancillary.is_none());
    assert!(q.is_empty());
}

#[test]
fn dgram_admission_overshoot_mutual_pair_accepts_resets_and_purge() {
    let c1 = Ucred::new(1, 0, 0);

    // 1. Linux v6.12 net/core/sock.c sock_alloc_send_pskb + af_unix.c:2015:
    //    SendBudget admits whenever `used < limit` and lets the charge overshoot
    //    `limit`, so whenever `writable()` (`used <= limit / 4`) holds, ANY
    //    datagram up to `max_msg_len()` (`limit - 32`) is guaranteed admitted!
    let mut sb = SendBudget::new(200); // quarter = 50, max_msg_len = 168
    assert_eq!(sb.max_msg_len(), 168);
    assert_eq!(sb.try_charge(169), Err(SendBudgetError::MessageTooLong));
    // Charge 18 bytes (cost = 50 == limit / 4) -> writable() STILL holds!
    assert_eq!(sb.try_charge(18), Ok(()));
    assert_eq!(sb.used(), 50);
    assert!(sb.writable());
    // While writable() holds, charging max_msg_len() (168, cost = 200) MUST succeed
    // (overshooting `used` to 250 > 200, matching sock_alloc_send_pskb)!
    assert_eq!(sb.try_charge(168), Ok(()));
    assert_eq!(sb.used(), 250);
    assert!(!sb.writable());
    // Now used >= limit (250 >= 200): even a 0-byte datagram gets WouldBlock,
    // while > max_msg_len() STILL gets MessageTooLong first!
    assert_eq!(sb.try_charge(169), Err(SendBudgetError::MessageTooLong));
    assert_eq!(sb.try_charge(0), Err(SendBudgetError::WouldBlock));
    // Release the 168-byte datagram -> used drops to 50 <= 50 -> wakes writer!
    assert!(sb.release(168));
    assert_eq!(sb.used(), 50);
    assert!(sb.writable());
    assert!(!sb.release(18));
    assert_eq!(sb.used(), 0);

    // Exact `sndbuf - 32` boundary through `DgramQueue::send_charged` (af_unix.c:2015).
    let mut bound_q = DgramQueue::<u32, u32, u32>::try_new(4).unwrap();
    let mut bound_sb = SendBudget::new(200);
    assert!(matches!(
        bound_q.send_charged(&mut bound_sb, mk_dgram(&[0u8; 169], None, 1, c1, None), false),
        DgramSendOutcome::MessageTooLong(_)
    ));
    assert_eq!(bound_sb.used(), 0);
    assert_eq!(
        bound_q.send_charged(&mut bound_sb, mk_dgram(&[0u8; 168], None, 1, c1, None), false),
        DgramSendOutcome::Sent(168)
    );
    assert_eq!(bound_sb.used(), 200);
    // Queue full + oversized payload -> MessageTooLong beats WouldBlock on send_charged!
    let mut full_q = DgramQueue::<u32, u32, u32>::try_new(0).unwrap();
    assert_eq!(full_q.send(mk_dgram(b"a", None, 1, c1, None), false), DgramSendOutcome::Sent(1));
    assert!(full_q.is_full());
    assert!(matches!(
        full_q.send_charged(&mut bound_sb, mk_dgram(&[0u8; 169], None, 1, c1, None), false),
        DgramSendOutcome::MessageTooLong(_)
    ));

    // SendBudget quarter-rule wake on release() and set_limit().
    let mut sb2 = SendBudget::new(400); // quarter = 100
    assert_eq!(sb2.try_charge(40), Ok(())); // used = 72 <= 100 -> writable
    assert!(sb2.writable());
    assert_eq!(sb2.try_charge(40), Ok(())); // used = 144 > 100 -> not writable
    assert!(!sb2.writable());
    assert!(!sb2.release(0)); // used = 112 > 100 -> still not writable
    assert!(sb2.release(40)); // used = 40 <= 100 -> wake!
    assert_eq!(sb2.try_charge(40), Ok(())); // used = 112 > 100
    assert!(!sb2.set_limit(444)); // 444/4 = 111 < 112
    assert!(sb2.set_limit(448)); // 448/4 = 112 >= 112 -> wake!

    // 2. Mutually connected pair (peer_is_sender == true): 20 datagrams past max_dgram_qlen (10),
    //    asserting unconditionally that NeedSlot fires at exact slot capacity before growing!
    let mut rx = DgramQueue::<u32, u32, u32>::try_new(DEFAULT_MAX_DGRAM_QLEN).unwrap();
    let mut budget = SendBudget::new(DEFAULT_SNDBUF);
    let init_cap = rx.slot_capacity();
    assert!((11..20).contains(&init_cap), "expected initial slot_capacity in 11..20, got {init_cap}");

    for i in 0..init_cap {
        let d = mk_dgram(&[i as u8; 32], Some(i as u32), 1, c1, None);
        assert_eq!(rx.send_charged(&mut budget, d, true), DgramSendOutcome::Sent(32));
    }
    // The (init_cap + 1)-th send MUST unconditionally return NeedSlot before growing!
    let d_trigger = mk_dgram(&[init_cap as u8; 32], Some(init_cap as u32), 1, c1, None);
    let DgramSendOutcome::NeedSlot(returned) = rx.send_charged(&mut budget, d_trigger, true) else {
        panic!("expected NeedSlot unconditionally at message #{init_cap}");
    };
    let spare = DgramSlotStorage::try_with_capacity(rx.next_slot_capacity()).unwrap();
    let _old = rx.grow_slots(spare);
    assert_eq!(rx.send_charged(&mut budget, returned, true), DgramSendOutcome::Sent(32));
    for i in (init_cap + 1)..20 {
        let d = mk_dgram(&[i as u8; 32], Some(i as u32), 1, c1, None);
        assert_eq!(rx.send_charged(&mut budget, d, true), DgramSendOutcome::Sent(32));
    }
    assert_eq!(rx.message_count(), 20);
    assert!(rx.is_full());
    assert!(!rx.accepts(false));
    assert!(rx.accepts(true));
    assert!(matches!(rx.send(mk_dgram(b"3rd", None, 99, c1, None), false), DgramSendOutcome::WouldBlock(_)));

    let mut buf = [0u8; 64];
    for i in 0..20u32 {
        let DgramRecvOutcome::Data { copied, full_len, addr, sender, cred, .. } = rx.recv(&mut buf, false) else {
            panic!("expected datagram {i}");
        };
        assert_eq!((copied, full_len), (32, 32));
        assert_eq!(addr, Some(RecvValue::Owned(i)));
        assert_eq!(sender, RecvValue::Owned(1));
        assert_eq!(cred, c1);
        assert!(buf[..32].iter().all(|&b| b == i as u8));
        budget.release(full_len);
    }
    assert_eq!(budget.used(), 0);

    // 3. POLLOUT for datagrams (`unix_dgram_poll`): closed or SHUT_RD peer still
    //    gives `accepts(false) == true` even when full, and wakes waiting writers!
    let mut pq = DgramQueue::<u32, u32, u32>::try_new(1).unwrap();
    assert_eq!(pq.send(mk_dgram(b"1", None, 1, c1, None), false), DgramSendOutcome::Sent(1));
    assert_eq!(pq.send(mk_dgram(b"2", None, 1, c1, None), false), DgramSendOutcome::Sent(1));
    assert!(pq.is_full());
    assert!(!pq.accepts(false));
    assert!(pq.shutdown(ShutdownHow::Read), "SHUT_RD on full queue must wake writers");
    assert!(pq.accepts(false), "SHUT_RD queue must report accepts=true so poll wakes writer for EPIPE");
    assert!(!pq.shutdown(ShutdownHow::Write)); // already accepting

    let mut pq2 = DgramQueue::<u32, u32, u32>::try_new(0).unwrap();
    assert_eq!(pq2.send(mk_dgram(b"1", None, 1, c1, None), false), DgramSendOutcome::Sent(1));
    assert!(!pq2.accepts(false));
    let undel_pq2 = pq2.close();
    assert!(undel_pq2.wake_writers(), "close on full queue must wake writers");
    assert!(pq2.accepts(false), "closed queue must report accepts=true so poll wakes writer for ECONNREFUSED");

    // 4. Datagram purge, reset (set_reset / take_error / err / Reset cleared on peek),
    //    and send_shutdown() (Linux af_unix.c unix_dgram_disconnected & net/core/datagram.c).
    let mut rq = DgramQueue::<u32, u32, u32>::try_new(4).unwrap();
    assert!(!rq.send_shutdown());
    assert_eq!(
        rq.send(mk_dgram(b"old1", Some(7), 11, c1, Some(Ancillary::from_fds(vec![55]))), false),
        DgramSendOutcome::Sent(4)
    );
    let purged = rq.purge();
    assert!(purged.had_dropped());
    assert_eq!(purged.dropped_count(), 1);
    assert_eq!(purged.fd_count(), 1);
    let dropped_items: Vec<_> = purged.collect();
    assert_eq!(dropped_items[0].sender, 11);
    assert_eq!(dropped_items[0].len(), 4);
    assert_eq!(dropped_items[0].ancillary, Some(Ancillary::from_fds(vec![55])));
    // Purge did NOT close or shut down the queue!
    assert!(!rq.is_closed());
    assert!(!rq.rdhup());
    assert!(!rq.send_shutdown());
    assert!(!rq.hup());
    assert!(!rq.err());
    // Empty purge reports had_dropped() == false.
    assert!(!rq.purge().had_dropped());

    // After a non-empty purge, slot storage was swapped out -> NeedSlot -> grow_slots -> Sent!
    let next_d = mk_dgram(b"new", None, 22, c1, None);
    let DgramSendOutcome::NeedSlot(ret) = rq.send(next_d, false) else {
        panic!("expected NeedSlot after non-empty purge");
    };
    let spare = DgramSlotStorage::try_with_capacity(rq.next_slot_capacity()).unwrap();
    let _old = rq.grow_slots(spare);
    assert_eq!(rq.send(ret, false), DgramSendOutcome::Sent(3));

    // set_reset / err / recv Reset (even on peek, matching sock_error xchg) / take_error.
    rq.set_reset();
    assert!(rq.err());
    assert_eq!(rq.recv(&mut buf, true), DgramRecvOutcome::Reset);
    assert!(!rq.err());
    // Queued "new" datagram still delivered after Reset was consumed!
    let DgramRecvOutcome::Data { copied, .. } = rq.recv(&mut buf, false) else {
        panic!("expected queued datagram after Reset");
    };
    assert_eq!(&buf[..copied], b"new");

    rq.set_reset();
    assert!(rq.take_error());
    assert!(!rq.take_error());
    assert!(!rq.err());

    // send_shutdown() tracks local SHUT_WR; SHUT_RDWR sets hup.
    rq.shutdown(ShutdownHow::Write);
    assert!(rq.send_shutdown());
    assert!(!rq.rdhup());
    assert!(!rq.hup());
    rq.shutdown(ShutdownHow::Read);
    assert!(rq.rdhup());
    assert!(rq.hup());
    assert_eq!(rq.recv(&mut buf, false), DgramRecvOutcome::Eof);

    // 5. Connected vs sendto routing (Linux af_unix.c unix_dgram_sendmsg: NO -EISCONN!).
    let mut peer = DgramPeer::<u32>::unconnected();
    assert_eq!(peer.resolve_send_target(None), Err(DgramTargetError::NotConnected));
    assert_eq!(peer.resolve_send_target(Some(&10)), Ok(&10));
    assert!(peer.may_accept_from(&10));
    assert_eq!(peer.connect(20), None);
    assert_eq!(peer.resolve_send_target(None), Ok(&20));
    assert_eq!(peer.resolve_send_target(Some(&10)), Ok(&10));
    assert!(peer.may_accept_from(&20));
    assert!(!peer.may_accept_from(&10));
    assert_eq!(peer.disconnect(), Some(20));
    assert_eq!(peer.resolve_send_target(None), Err(DgramTargetError::NotConnected));
}

#[test]
fn listener_backlog_clamping_swap_growth_pending_iter_and_close() {
    // Linux v6.12 net/socket.c __sys_listen_socket:
    // if ((unsigned int)backlog > somaxconn) backlog = somaxconn;
    assert_eq!(clamp_backlog(0, SOMAXCONN), 0);
    assert_eq!(clamp_backlog(1, SOMAXCONN), 1);
    assert_eq!(clamp_backlog(128, SOMAXCONN), 128);
    assert_eq!(clamp_backlog(4096, SOMAXCONN), 4096);
    assert_eq!(clamp_backlog(10_000, SOMAXCONN), 4096);
    assert_eq!(clamp_backlog(-1, SOMAXCONN), 4096);
    assert_eq!(clamp_backlog(i32::MIN, SOMAXCONN), 4096);

    // Linux v6.12 net/unix/af_unix.c unix_recvq_full_lockless:
    // backlog = 0 permits 1 pending connection; 2nd returns WouldBlock.
    let mut l0 = Listener::<u32>::try_new(0).unwrap();
    assert_eq!(l0.backlog(), 0);
    assert_eq!(l0.max_pending(), 1);
    assert!(!l0.readable());
    assert!(!l0.writable());
    assert!(!l0.hup());
    assert_eq!(l0.accept(), AcceptOutcome::WouldBlock);
    assert_eq!(l0.connect(100), ConnectOutcome::Queued);
    assert!(l0.readable());
    assert!(l0.is_full());
    assert_eq!(l0.connect(101), ConnectOutcome::WouldBlock(101));
    assert_eq!(l0.accept(), AcceptOutcome::Accepted { conn: 100, wake_connecters: true });
    assert!(!l0.is_full());

    // Backlog = 2 accepts 3 connections (FIFO), 4th returns WouldBlock.
    let mut l = Listener::<u32>::try_new(2).unwrap();
    assert_eq!(l.connect(1), ConnectOutcome::Queued);
    assert_eq!(l.connect(2), ConnectOutcome::Queued);
    assert_eq!(l.connect(3), ConnectOutcome::Queued);
    assert_eq!(l.connect(4), ConnectOutcome::WouldBlock(4));

    assert_eq!(l.pending().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(l.pending_len(), 3);

    assert_eq!(l.accept(), AcceptOutcome::Accepted { conn: 1, wake_connecters: true });
    assert_eq!(l.connect(4), ConnectOutcome::Queued);
    assert_eq!(l.connect(5), ConnectOutcome::WouldBlock(5));

    // Raising backlog past capacity unconditionally returns NeedStorage(11),
    // then ListenerStorage + grow_storage + listen succeeds and wakes connecters.
    assert_eq!(l.listen(10, SOMAXCONN), Err(ListenError::NeedStorage(11)));
    let spare = ListenerStorage::try_for_backlog(10, SOMAXCONN).unwrap();
    let _old = l.grow_storage(spare);
    assert_eq!(l.listen(10, SOMAXCONN), Ok(true));
    assert_eq!(l.backlog(), 10);
    assert_eq!(l.connect(5), ConnectOutcome::Queued);
    assert_eq!(l.pending_len(), 4);

    // Lowering backlog preserves already-queued connections and never allocates.
    assert_eq!(l.listen(1, SOMAXCONN), Ok(false));
    assert_eq!(l.pending_len(), 4);
    assert!(l.is_full());
    assert_eq!(l.connect(6), ConnectOutcome::WouldBlock(6));

    assert_eq!(l.accept(), AcceptOutcome::Accepted { conn: 2, wake_connecters: false });

    // Closing hands back every remaining pending connection in FIFO order [3, 4, 5].
    let pending = l.close();
    assert_eq!(pending.len(), 3);
    assert_eq!(pending.collect::<Vec<_>>(), vec![3, 4, 5]);
    assert!(l.is_closed());
    assert!(l.hup());
    assert!(l.readable());
    assert!(!l.writable());
    assert_eq!(l.readiness(), Readiness { readable: true, writable: false, rdhup: true, hup: true, err: false });
    assert_eq!(l.connect(7), ConnectOutcome::Refused(7));
    assert_eq!(l.accept(), AcceptOutcome::Closed);
    assert_eq!(l.listen(2, SOMAXCONN), Err(ListenError::Closed));
    assert!(l.close().is_empty());
}
