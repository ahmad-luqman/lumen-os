//! Bidirectional `AF_UNIX` `SOCK_STREAM` queues (`StreamPair`).
//!
//! Models Linux v6.12 `net/unix/af_unix.c`:
//! - `unix_stream_sendmsg`: partial writes against the sender's `sk_sndbuf`
//!   (charging payload bytes plus [`SEGMENT_OVERHEAD`] per non-coalesced
//!   segment — a deliberate deviation from Linux's per-skb `sndbuf / 2 - 64`
//!   chunking with overshoot, kept non-spinning by partial writes), lazy ring
//!   buffer growth via [`StreamSendOutcome::NeedBuffer`] / [`ByteStorage`],
//!   segment-ring growth via [`StreamSendOutcome::NeedSegment`] /
//!   [`SegmentStorage`], `WouldBlock` when full, and `BrokenPipe` (`-EPIPE` +
//!   `SIGPIPE` unless `MSG_NOSIGNAL`) after send shutdown or peer read
//!   shutdown / close.
//! - `unix_stream_read_generic` (~2866–2931): reads continue **into** a
//!   segment carrying `SCM_RIGHTS` fds, detach (or peek via `unix_peek_fds`)
//!   its fds, and stop **after** it (`if (scm.fp) break;`); a later write
//!   never coalesces into a segment with fds; when `check_creds`
//!   (`SO_PASSCRED`) is enabled, reads stop on a writer identity change; every
//!   read reports the starting segment's writer identity (`Ucred`). A
//!   zero-length `recv` runs once with target 1 (reporting `Reset`/`Eof`/
//!   `WouldBlock` on empty, or detaching/peeking head fds with 0 bytes), while
//!   `read(2)` returns [`StreamRecvOutcome::ZeroBuffer`] immediately.
//! - `unix_shutdown` & `unix_release_sock`: propagates `SHUT_RD` to peer's
//!   `SEND_SHUTDOWN` and `SHUT_WR` to peer's `RCV_SHUTDOWN`; closing an end
//!   calls `shutdown(Both)`, purges its own receive queue (leaving the peer's
//!   receive queue readable), reports [`UndeliveredAncillary::had_unread_bytes`],
//!   and sets the peer's `ECONNRESET` (`StreamRecvOutcome::Reset` once — even
//!   on `MSG_PEEK`, matching `sock_error`'s `xchg` — after draining queued
//!   bytes, then `Eof`) when unread bytes were discarded or on
//!   [`StreamPair::close_reset`] (unaccepted `embrion`).
//! - `unix_poll` & `unix_writable`: `readable` (`!empty || rcv_shutdown`),
//!   `writable` (`wmem << 2 <= sndbuf` only), `rdhup`, `hup`, and `err`
//!   (`POLLERR`).

use alloc::collections::VecDeque;
use core::fmt;
use core::mem;

use crate::ring::{ByteRing, ByteStorage, grow_deque, try_deque};
use crate::{Ancillary, NoMemory, Readiness, RecvAncillary, ShutdownHow, Ucred};

/// Default pre-allocated segment-metadata slots per direction.
///
/// Same-writer writes without fds coalesce into a single segment, so 16 slots
/// cover 16 distinct ancillary or cross-writer segments in flight before
/// needing to grow via [`SegmentStorage`].
pub const DEFAULT_SEGMENT_CAPACITY: usize = 16;

/// Lazy initial byte-ring capacity per direction (4 KiB), avoiding upfront
/// allocation of 2 × [`DEFAULT_SNDBUF`](crate::DEFAULT_SNDBUF) on every
/// stream pair. Directions grow toward `sndbuf` on demand via
/// [`StreamSendOutcome::NeedBuffer`] + [`StreamPair::grow_buffer`].
pub const DEFAULT_INITIAL_BUF: usize = 4096;

/// Fixed per-segment byte overhead charged against `sk_sndbuf` (Lumen's
/// strict-cap counterpart to Linux's `skb->truesize` metadata accounting),
/// preventing 1-byte non-coalescing writes from building megabytes of segment
/// metadata.
pub const SEGMENT_OVERHEAD: usize = 64;

/// Minimum valid stream `sndbuf` (`SEGMENT_OVERHEAD + 1`), ensuring at least a
/// 1-byte write can fit in an empty queue.
pub const MIN_STREAM_SNDBUF: usize = SEGMENT_OVERHEAD + 1;

const fn max_payload(sndbuf: usize) -> usize {
    sndbuf.saturating_sub(SEGMENT_OVERHEAD)
}

const fn max_segments(sndbuf: usize) -> usize {
    sndbuf / MIN_STREAM_SNDBUF
}

/// Which peer of a connected [`StreamPair`] is acting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum End {
    /// First socket of the pair (e.g. `sv[0]` or the connecting client).
    A,
    /// Second socket of the pair (e.g. `sv[1]` or the accepted peer).
    B,
}

impl End {
    /// The opposite peer of this stream pair.
    pub const fn peer(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Error returned by [`StreamPair::try_new`] and [`StreamPair::set_sndbuf`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetSndbufError {
    /// Requested `sndbuf` is smaller than [`MIN_STREAM_SNDBUF`] (`-EINVAL`).
    Invalid,
    /// Fallible heap allocation failed during construction (`-ENOMEM`).
    NoMemory,
}

impl From<NoMemory> for SetSndbufError {
    fn from(_: NoMemory) -> Self {
        Self::NoMemory
    }
}

/// Outcome of [`StreamPair::send`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum StreamSendOutcome<F> {
    /// `n` bytes were copied into the send queue (`n < data.len()` on a partial
    /// write). Any non-empty `Ancillary` was attached to this segment.
    Sent(usize),
    /// A zero-length stream send carried non-empty `Ancillary`; on
    /// `SOCK_STREAM`, ancillary data requires at least one payload byte, so it
    /// is returned untouched for the kernel to handle/close.
    EmptyWithAncillary(Ancillary<F>),
    /// Send buffer (`sk_sndbuf`) has no room for this write right now
    /// (`-EAGAIN`); supplied ancillary data is returned untouched.
    WouldBlock(Option<Ancillary<F>>),
    /// This end's write side or the peer's read side is shut down / closed
    /// (`-EPIPE`, `SIGPIPE` unless `MSG_NOSIGNAL`); supplied ancillary data is
    /// returned untouched.
    BrokenPipe(Option<Ancillary<F>>),
    /// The lazy byte ring's free space is below the bytes this write would
    /// transfer (`ring.free() < data.len().min(available)`) while `sk_sndbuf`
    /// has room. Supplied ancillary data is returned untouched and zero bytes
    /// were written.
    ///
    /// Kernel protocol: read [`StreamPair::next_buffer_capacity`], release the
    /// lock, allocate [`ByteStorage::try_with_capacity`], retake the lock,
    /// swap in via [`StreamPair::grow_buffer`], retry `send`, and drop the old
    /// [`ByteStorage`] outside the lock.
    NeedBuffer(Option<Ancillary<F>>),
    /// The segment metadata ring is at capacity and needs a larger
    /// [`SegmentStorage`] before this write can be queued without allocating
    /// under lock. Supplied ancillary data is returned untouched.
    ///
    /// Kernel protocol: read [`StreamPair::next_segment_capacity`], release
    /// the lock, allocate [`SegmentStorage::try_with_capacity`], retake the
    /// lock, swap in via [`StreamPair::grow_segments`], retry `send`, and drop
    /// the old [`SegmentStorage`] outside the lock.
    NeedSegment(Option<Ancillary<F>>),
}

/// Outcome of [`StreamPair::recv`] and [`StreamPair::read`].
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum StreamRecvOutcome<'a, F> {
    /// `bytes` were copied into `buf` (`0` only for a zero-length `recv` that
    /// observed a queued head segment, detaching/peeking its fds if any).
    Data {
        /// Number of bytes copied into `buf`.
        bytes: usize,
        /// Writer identity of the segment this read started on (reported on
        /// every read, including subsequent partial reads of a segment; used
        /// to emit `SCM_CREDENTIALS` when `SO_PASSCRED` is set).
        sender: Ucred,
        /// `SCM_RIGHTS` ancillary data detached (or peeked) from the last
        /// segment touched by this read, if it carried file references.
        ancillary: Option<RecvAncillary<'a, F>>,
        /// `true` if `peek == false` and the peer transitioned from
        /// `!writable` to `writable` under Linux's quarter watermark
        /// (`wmem << 2 <= sndbuf`; wake waiting writers and pollers).
        wake_writer: bool,
    },
    /// Returned only by [`StreamPair::read`] (`read(2)` / `readv(2)`) for an
    /// empty destination slice (`return 0` immediately without inspecting the
    /// queue, detaching fds, or clearing a pending reset).
    ZeroBuffer,
    /// Receive queue is empty and the peer reset the connection (`-ECONNRESET`,
    /// cleared and reported once regardless of `peek`, matching Linux
    /// `sock_error`; subsequent receives see `Eof`).
    Reset,
    /// Receive queue is empty and no more data can arrive (`return 0`).
    Eof,
    /// Receive queue is empty and the peer may still send (`-EAGAIN`).
    WouldBlock,
}

#[derive(Debug)]
struct Segment<F> {
    /// Positive byte count in the direction's ring buffer.
    len: usize,
    /// Writer identity recorded on `send` (`UNIXCB(skb)` credentials).
    sender: Ucred,
    /// Present only when this segment carries `SCM_RIGHTS` fds (`!anc.fds.is_empty()`).
    ancillary: Option<Ancillary<F>>,
}

/// Pre-allocated segment-ring storage for the non-allocating growth protocol
/// ([`StreamPair::grow_segments`]).
pub struct SegmentStorage<F> {
    deque: VecDeque<Segment<F>>,
}

impl<F> fmt::Debug for SegmentStorage<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentStorage")
            .field("len", &self.deque.len())
            .field("capacity", &self.deque.capacity())
            .finish()
    }
}

impl<F> SegmentStorage<F> {
    /// Allocate empty segment storage with capacity for at least `cap`
    /// segments. Intended to be called **outside** any socket lock.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_with_capacity(cap: usize) -> Result<Self, NoMemory> {
        Ok(Self { deque: try_deque(cap)? })
    }

    /// Usable segment capacity of this storage.
    pub fn capacity(&self) -> usize {
        self.deque.capacity()
    }
}

/// Undelivered [`Ancillary`] items (and discarded unread byte count) detached
/// from a direction's receive queue on [`StreamPair::close`],
/// [`StreamPair::close_reset`], or [`StreamPair::purge_recv`].
///
/// Returned without allocating so the kernel can release its socket lock before
/// iterating and closing every passed file reference (and before freeing the
/// underlying segment-ring memory when this value drops).
#[must_use]
pub struct UndeliveredAncillary<F> {
    segments: VecDeque<Segment<F>>,
    ancillary_count: usize,
    fd_count: usize,
    unread_bytes: usize,
    /// `true` when the head segment's `ancillary.fds` has been reversed for
    /// O(1) FIFO popping via `pop_fd`.
    front_reversed: bool,
}

impl<F> fmt::Debug for UndeliveredAncillary<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UndeliveredAncillary")
            .field("ancillary_items", &self.ancillary_count)
            .field("fds", &self.fd_count)
            .field("unread_bytes", &self.unread_bytes)
            .finish()
    }
}

impl<F> UndeliveredAncillary<F> {
    const fn empty() -> Self {
        Self { segments: VecDeque::new(), ancillary_count: 0, fd_count: 0, unread_bytes: 0, front_reversed: false }
    }

    /// `true` if the drained receive queue held unread payload bytes (which
    /// triggers `ECONNRESET` on the peer during `close`, per Linux
    /// `net/unix/af_unix.c` `unix_release_sock`).
    pub const fn had_unread_bytes(&self) -> bool {
        self.unread_bytes > 0
    }

    /// Number of unread payload bytes discarded.
    pub const fn unread_bytes(&self) -> usize {
        self.unread_bytes
    }

    /// Total number of undelivered file references currently held.
    pub const fn fd_count(&self) -> usize {
        self.fd_count
    }

    /// `true` if no undelivered [`Ancillary`] items remain to pop (even if
    /// plain unread bytes without fds were discarded; check
    /// [`UndeliveredAncillary::had_unread_bytes`] for that).
    pub const fn is_empty(&self) -> bool {
        self.ancillary_count == 0
    }

    /// Pop the next undelivered [`Ancillary`] in FIFO order. Never allocates.
    pub fn pop_next(&mut self) -> Option<Ancillary<F>> {
        while let Some(mut seg) = self.segments.pop_front() {
            let was_reversed = mem::replace(&mut self.front_reversed, false);
            if let Some(mut anc) = seg.ancillary.take() {
                if was_reversed {
                    anc.fds.reverse();
                }
                self.ancillary_count -= 1;
                self.fd_count -= anc.fd_count();
                return Some(anc);
            }
        }
        None
    }

    /// Pop the next undelivered file reference in FIFO order across all
    /// segments in O(1) amortized time (reverses each segment's fd vec once
    /// and pops from the back rather than shifting on `remove(0)`). Never
    /// allocates.
    pub fn pop_fd(&mut self) -> Option<F> {
        while let Some(front) = self.segments.front_mut() {
            if let Some(anc) = front.ancillary.as_mut()
                && !anc.fds.is_empty()
            {
                if !self.front_reversed {
                    anc.fds.reverse();
                    self.front_reversed = true;
                }
                self.fd_count -= 1;
                let fd = anc.fds.pop().expect("checked non-empty");
                if anc.fds.is_empty() {
                    self.ancillary_count -= 1;
                    front.ancillary = None;
                    self.front_reversed = false;
                }
                return Some(fd);
            }
            self.segments.pop_front();
            self.front_reversed = false;
        }
        None
    }
}

impl<F> Iterator for UndeliveredAncillary<F> {
    type Item = Ancillary<F>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop_next()
    }
}

struct ReadPlan {
    total_bytes: usize,
    full_segs: usize,
    partial_last: usize,
    fds_seg_idx: Option<usize>,
}

struct StreamDirection<F> {
    ring: ByteRing,
    sndbuf: usize,
    segments: VecDeque<Segment<F>>,
    in_flight_fds: usize,
}

impl<F> StreamDirection<F> {
    fn try_new(sndbuf: usize) -> Result<Self, NoMemory> {
        let init_buf = max_payload(sndbuf).clamp(1, DEFAULT_INITIAL_BUF);
        let init_segs = DEFAULT_SEGMENT_CAPACITY.min(max_segments(sndbuf).max(1));

        let ring = ByteRing::try_new(init_buf)?;
        let segments = try_deque(init_segs)?;
        Ok(Self { ring, sndbuf, segments, in_flight_fds: 0 })
    }

    fn wmem(&self) -> usize {
        self.ring.len() + self.segments.len() * SEGMENT_OVERHEAD
    }

    fn send_space(&self) -> usize {
        self.sndbuf.saturating_sub(self.wmem())
    }

    /// Linux v6.12 `net/unix/af_unix.c` `unix_writable`: `(wmem << 2) <= sndbuf`.
    fn is_writable(&self) -> bool {
        self.wmem() <= self.sndbuf / 4
    }

    /// Compute the exact read extent and optional fd-carrying segment index in
    /// one loop, matching Linux v6.12 `net/unix/af_unix.c`
    /// `unix_stream_read_generic` (~2866–2931):
    /// - if `check_creds` (`SO_PASSCRED`), stop on a writer identity change;
    /// - continue into a segment carrying fds, record it as `fds_seg_idx`, and
    ///   break immediately after it (`if (scm.fp) break;`).
    fn plan_read(&self, max_bytes: usize, check_creds: bool) -> ReadPlan {
        let Some(front) = self.segments.front() else {
            return ReadPlan { total_bytes: 0, full_segs: 0, partial_last: 0, fds_seg_idx: None };
        };
        let first_sender = front.sender;

        // Zero-length `recv`/`recvmsg` enters unix_stream_read_generic once
        // with target 1: touches only the head segment, copying 0 bytes and
        // detaching/peeking its fds if present.
        if max_bytes == 0 {
            let fds_seg_idx = front.ancillary.as_ref().is_some_and(Ancillary::has_fds).then_some(0);
            return ReadPlan { total_bytes: 0, full_segs: 0, partial_last: 0, fds_seg_idx };
        }

        let mut remaining = max_bytes;
        let mut total_bytes = 0usize;
        let mut full_segs = 0usize;
        let mut partial_last = 0usize;
        let mut fds_seg_idx = None;

        for (idx, seg) in self.segments.iter().enumerate() {
            if remaining == 0 {
                break;
            }
            if check_creds && seg.sender != first_sender {
                break;
            }
            let has_fds = seg.ancillary.as_ref().is_some_and(Ancillary::has_fds);
            if seg.len <= remaining {
                total_bytes += seg.len;
                remaining -= seg.len;
                full_segs += 1;
            } else {
                total_bytes += remaining;
                partial_last = remaining;
                remaining = 0;
            }
            if has_fds {
                fds_seg_idx = Some(idx);
                break;
            }
        }

        ReadPlan { total_bytes, full_segs, partial_last, fds_seg_idx }
    }

    fn next_buffer_capacity(&self, write_len: usize) -> usize {
        let cur = self.ring.capacity();
        let cap_limit = max_payload(self.sndbuf).max(1);
        let need = self.ring.len().saturating_add(write_len).min(cap_limit);
        let doubled = cur.saturating_mul(2).max(DEFAULT_INITIAL_BUF);
        doubled.max(need).min(cap_limit).max(cur.saturating_add(1))
    }

    fn next_segment_capacity(&self) -> usize {
        let cur = self.segments.capacity();
        let cap_limit = max_segments(self.sndbuf).max(1);
        let doubled = cur.saturating_mul(2).max(DEFAULT_SEGMENT_CAPACITY);
        doubled.min(cap_limit).max(cur.saturating_add(1))
    }

    fn grow_segments(&mut self, mut spare: SegmentStorage<F>) -> SegmentStorage<F> {
        grow_deque(&mut self.segments, &mut spare.deque);
        spare
    }

    fn take_all_undelivered(&mut self) -> UndeliveredAncillary<F> {
        let unread_bytes = self.ring.len();
        if self.segments.is_empty() && unread_bytes == 0 {
            return UndeliveredAncillary::empty();
        }
        let segments = mem::take(&mut self.segments);
        let ancillary_count = segments.iter().filter(|s| s.ancillary.is_some()).count();
        let fd_count = self.in_flight_fds;
        self.ring.clear();
        self.in_flight_fds = 0;
        UndeliveredAncillary { segments, ancillary_count, fd_count, unread_bytes, front_reversed: false }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct EndState {
    recv_shutdown: bool,
    send_shutdown: bool,
    closed: bool,
    reset_pending: bool,
}

/// Connected bidirectional `AF_UNIX` `SOCK_STREAM` socket pair.
///
/// Direction `A -> B` is charged against `A`'s `sndbuf` and read by `B`;
/// direction `B -> A` is charged against `B`'s `sndbuf` and read by `A`.
pub struct StreamPair<F> {
    a_to_b: StreamDirection<F>,
    b_to_a: StreamDirection<F>,
    end_a: EndState,
    end_b: EndState,
}

impl<F> fmt::Debug for StreamPair<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamPair")
            .field("a_to_b_len", &self.a_to_b.ring.len())
            .field("a_to_b_sndbuf", &self.a_to_b.sndbuf)
            .field("a_to_b_fds", &self.a_to_b.in_flight_fds)
            .field("b_to_a_len", &self.b_to_a.ring.len())
            .field("b_to_a_sndbuf", &self.b_to_a.sndbuf)
            .field("b_to_a_fds", &self.b_to_a.in_flight_fds)
            .field("end_a", &self.end_a)
            .field("end_b", &self.end_b)
            .finish()
    }
}

impl<F> StreamPair<F> {
    /// Create a connected stream pair with send buffer limit `sndbuf` per
    /// direction (pass [`DEFAULT_SNDBUF`](crate::DEFAULT_SNDBUF) for Linux's
    /// default 208 KiB).
    ///
    /// Backing byte rings start lazily at
    /// `(sndbuf - SEGMENT_OVERHEAD).min(DEFAULT_INITIAL_BUF)` bytes and grow
    /// toward `sndbuf` on demand via [`StreamSendOutcome::NeedBuffer`] +
    /// [`StreamPair::grow_buffer`].
    ///
    /// # Errors
    ///
    /// - [`SetSndbufError::Invalid`] if `sndbuf < MIN_STREAM_SNDBUF`.
    /// - [`SetSndbufError::NoMemory`] if initial buffer or segment-ring
    ///   allocation fails.
    pub fn try_new(sndbuf: usize) -> Result<Self, SetSndbufError> {
        if sndbuf < MIN_STREAM_SNDBUF {
            return Err(SetSndbufError::Invalid);
        }
        let a_to_b = StreamDirection::try_new(sndbuf)?;
        let b_to_a = StreamDirection::try_new(sndbuf)?;
        Ok(Self { a_to_b, b_to_a, end_a: EndState::default(), end_b: EndState::default() })
    }

    const fn end_state(&self, end: End) -> &EndState {
        match end {
            End::A => &self.end_a,
            End::B => &self.end_b,
        }
    }

    const fn end_state_mut(&mut self, end: End) -> &mut EndState {
        match end {
            End::A => &mut self.end_a,
            End::B => &mut self.end_b,
        }
    }

    const fn send_dir(&self, sender: End) -> &StreamDirection<F> {
        match sender {
            End::A => &self.a_to_b,
            End::B => &self.b_to_a,
        }
    }

    const fn send_dir_mut(&mut self, sender: End) -> &mut StreamDirection<F> {
        match sender {
            End::A => &mut self.a_to_b,
            End::B => &mut self.b_to_a,
        }
    }

    const fn recv_dir(&self, reader: End) -> &StreamDirection<F> {
        self.send_dir(reader.peer())
    }

    const fn recv_dir_mut(&mut self, reader: End) -> &mut StreamDirection<F> {
        self.send_dir_mut(reader.peer())
    }

    /// Configured send buffer limit (`sk_sndbuf`) for writes initiated by `sender`.
    pub const fn sndbuf(&self, sender: End) -> usize {
        self.send_dir(sender).sndbuf
    }

    /// Current allocated backing byte-ring capacity for `sender`'s outbound
    /// direction (starts lazily at `<= DEFAULT_INITIAL_BUF` and grows via
    /// [`StreamPair::grow_buffer`]).
    pub const fn buffer_capacity(&self, sender: End) -> usize {
        self.send_dir(sender).ring.capacity()
    }

    /// Update `sender`'s logical send buffer limit (`SO_SNDBUF`).
    ///
    /// Per Linux `net/core/sock.c` `__sock_set_sndbuf`, the kernel should
    /// double the userspace `SO_SNDBUF` value and clamp it to
    /// `[MIN_SNDBUF, 2 * wmem_max]` before calling this method.
    ///
    /// Never allocates: shrinking keeps already-queued bytes readable, and
    /// growing lets the backing ring expand lazily via
    /// [`StreamSendOutcome::NeedBuffer`] + [`StreamPair::grow_buffer`] (or
    /// proactively via [`StreamPair::grow_buffer`]).
    ///
    /// Returns `Ok(true)` if `sender` transitioned from `!writable` to
    /// `writable` under Linux's quarter watermark (`wmem << 2 <= sndbuf`; wake
    /// waiting writers and pollers).
    ///
    /// # Errors
    ///
    /// Returns [`SetSndbufError::Invalid`] if `new_sndbuf < MIN_STREAM_SNDBUF`.
    pub fn set_sndbuf(&mut self, sender: End, new_sndbuf: usize) -> Result<bool, SetSndbufError> {
        if new_sndbuf < MIN_STREAM_SNDBUF {
            return Err(SetSndbufError::Invalid);
        }
        let dir = self.send_dir_mut(sender);
        let was_writable = dir.is_writable();
        dir.sndbuf = new_sndbuf;
        Ok(!was_writable && dir.is_writable())
    }

    /// Unread payload bytes currently in flight sent by `sender`.
    pub const fn bytes_in_flight(&self, sender: End) -> usize {
        self.send_dir(sender).ring.len()
    }

    /// Total `sk_wmem_alloc` charged for `sender`: payload bytes plus
    /// `segment_count(sender) * SEGMENT_OVERHEAD`.
    pub fn wmem(&self, sender: End) -> usize {
        self.send_dir(sender).wmem()
    }

    /// Number of queued segments currently in flight sent by `sender`.
    pub fn segment_count(&self, sender: End) -> usize {
        self.send_dir(sender).segments.len()
    }

    /// Unread payload bytes currently queued for `reader` (`FIONREAD` / `SIOCINQ`).
    pub const fn readable_bytes(&self, reader: End) -> usize {
        self.recv_dir(reader).ring.len()
    }

    /// Remaining `wmem` headroom in `sender`'s send buffer (`sndbuf - wmem`).
    pub fn send_space(&self, sender: End) -> usize {
        self.send_dir(sender).send_space()
    }

    /// Undelivered `SCM_RIGHTS` file references currently queued in the
    /// direction sent by `sender`.
    pub const fn in_flight_fds(&self, sender: End) -> usize {
        self.send_dir(sender).in_flight_fds
    }

    /// Undelivered `SCM_RIGHTS` file references currently queued for `reader`.
    pub const fn recv_in_flight_fds(&self, reader: End) -> usize {
        self.recv_dir(reader).in_flight_fds
    }

    /// Total undelivered `SCM_RIGHTS` file references across both directions.
    pub const fn total_in_flight_fds(&self) -> usize {
        self.a_to_b.in_flight_fds + self.b_to_a.in_flight_fds
    }

    /// Visit every in-flight `SCM_RIGHTS` file reference currently queued in
    /// `reader`'s receive queue in FIFO order, without draining or allocating.
    ///
    /// Intended for the kernel's `AF_UNIX` garbage collector (`unix_gc`).
    pub fn for_each_queued_fd(&self, reader: End, mut f: impl FnMut(&F)) {
        for seg in &self.recv_dir(reader).segments {
            if let Some(ref anc) = seg.ancillary {
                for fd in &anc.fds {
                    f(fd);
                }
            }
        }
    }

    /// Recommended capacity for [`ByteStorage::try_with_capacity`] after
    /// [`StreamPair::send`] of `write_len` bytes returned
    /// [`StreamSendOutcome::NeedBuffer`]. Grows at least by doubling and in
    /// one step for large writes, capped at `sndbuf - SEGMENT_OVERHEAD`.
    /// Never allocates.
    pub fn next_buffer_capacity(&self, sender: End, write_len: usize) -> usize {
        self.send_dir(sender).next_buffer_capacity(write_len)
    }

    /// Install a pre-allocated [`ByteStorage`] for `sender`'s outbound ring
    /// buffer without allocating or freeing under lock, returning the previous
    /// buffer (or `spare` unchanged if not larger) to drop outside the lock.
    pub fn grow_buffer(&mut self, sender: End, spare: ByteStorage) -> ByteStorage {
        self.send_dir_mut(sender).ring.grow_storage(spare)
    }

    /// Recommended capacity for [`SegmentStorage::try_with_capacity`] after
    /// [`StreamPair::send`] returned [`StreamSendOutcome::NeedSegment`].
    /// Never allocates.
    pub fn next_segment_capacity(&self, sender: End) -> usize {
        self.send_dir(sender).next_segment_capacity()
    }

    /// Install a pre-allocated [`SegmentStorage`] for `sender`'s outbound
    /// queue without allocating or freeing under lock, returning the previous
    /// (now empty) storage — or `spare` unchanged if not larger — to drop
    /// outside the lock.
    pub fn grow_segments(&mut self, sender: End, spare: SegmentStorage<F>) -> SegmentStorage<F> {
        self.send_dir_mut(sender).grow_segments(spare)
    }

    /// Attempt a non-blocking send from `sender` to `sender.peer()` (Linux
    /// v6.12 `net/unix/af_unix.c` `unix_stream_sendmsg`).
    ///
    /// - `cred` is the calling writer's required identity (`UNIXCB(skb)`). If
    ///   `ancillary` supplies an explicit `SCM_CREDENTIALS` override, it
    ///   replaces `cred` on the segment (matching Linux `scm_send`).
    /// - Writes from the same effective writer with no fds coalesce into the
    ///   tail segment when that segment carries no fds; a later write **never**
    ///   coalesces into a segment that carries fds.
    /// - Admission enforces a strict cap (`wmem <= sndbuf`, charging fixed
    ///   [`SEGMENT_OVERHEAD`] per non-coalesced segment); this is a deliberate
    ///   deviation from Linux's per-skb `sndbuf / 2 - 64` chunking with
    ///   overshoot, kept non-spinning on `POLLOUT` by partial writes.
    /// - Never allocates: returns [`StreamSendOutcome::NeedBuffer`] or
    ///   [`StreamSendOutcome::NeedSegment`] if backing storage needs growth.
    pub fn send(
        &mut self,
        sender: End,
        data: &[u8],
        cred: Ucred,
        ancillary: Option<Ancillary<F>>,
    ) -> StreamSendOutcome<F> {
        let ancillary = ancillary.filter(|a| !a.is_empty());

        if self.end_state(sender).send_shutdown {
            return StreamSendOutcome::BrokenPipe(ancillary);
        }
        if data.is_empty() {
            return match ancillary {
                Some(anc) => StreamSendOutcome::EmptyWithAncillary(anc),
                None => StreamSendOutcome::Sent(0),
            };
        }

        let effective_cred = ancillary.as_ref().and_then(|a| a.cred).unwrap_or(cred);
        let has_fds = ancillary.as_ref().is_some_and(Ancillary::has_fds);

        let dir = self.send_dir_mut(sender);
        let can_coalesce =
            !has_fds && dir.segments.back().is_some_and(|b| b.sender == effective_cred && b.ancillary.is_none());

        let available = if can_coalesce {
            dir.send_space()
        } else {
            dir.sndbuf.saturating_sub(dir.wmem().saturating_add(SEGMENT_OVERHEAD))
        };
        if available == 0 {
            return StreamSendOutcome::WouldBlock(ancillary);
        }
        if !can_coalesce && dir.segments.len() == dir.segments.capacity() {
            return StreamSendOutcome::NeedSegment(ancillary);
        }
        let want = data.len().min(available);
        if dir.ring.free() < want {
            return StreamSendOutcome::NeedBuffer(ancillary);
        }

        dir.ring.push_bytes(&data[..want]);

        if can_coalesce {
            if let Some(back) = dir.segments.back_mut() {
                back.len += want;
            }
        } else {
            let seg_anc = if has_fds { ancillary } else { None };
            let fds = seg_anc.as_ref().map_or(0, Ancillary::fd_count);
            dir.in_flight_fds += fds;
            dir.segments.push_back(Segment { len: want, sender: effective_cred, ancillary: seg_anc });
        }

        StreamSendOutcome::Sent(want)
    }

    /// `read(2)` / `readv(2)` entry point: returns
    /// [`StreamRecvOutcome::ZeroBuffer`] immediately when `buf.is_empty()`
    /// without inspecting the queue, detaching fds, or clearing a pending
    /// reset; otherwise delegates to [`StreamPair::recv`] with `peek = false`.
    /// Never allocates.
    pub fn read<'a>(&'a mut self, reader: End, buf: &mut [u8], check_creds: bool) -> StreamRecvOutcome<'a, F> {
        if buf.is_empty() {
            return StreamRecvOutcome::ZeroBuffer;
        }
        self.recv(reader, buf, false, check_creds)
    }

    /// `recv(2)` / `recvfrom(2)` / `recvmsg(2)` entry point (Linux v6.12
    /// `net/unix/af_unix.c` `unix_stream_read_generic`, ~2866–2931).
    ///
    /// - Pass `check_creds = true` when `SO_PASSCRED` is enabled on `reader`
    ///   so reads split on a writer identity change; when `false`, segments
    ///   from different writers read through without splitting.
    /// - Reads continue **into** a segment carrying `SCM_RIGHTS` fds, detach
    ///   (or peek) its fds, and stop **after** that segment (`if (scm.fp) break;`).
    /// - A zero-length `buf` runs Linux's loop once with `sock_rcvlowat`
    ///   target `1`: on an empty queue it returns a pending `Reset` once, then
    ///   `Eof` after shutdown, else `WouldBlock`; on a non-empty queue it
    ///   returns `Data { bytes: 0, sender, ancillary, wake_writer: false }`,
    ///   detaching (or peeking) any fds on the head segment while leaving its
    ///   payload bytes queued.
    /// - Every `Data` outcome reports the writer identity (`sender: Ucred`) of
    ///   the segment the read started on (including subsequent partial reads
    ///   of the same segment).
    /// - On an empty receive queue, clears and reports a pending peer reset
    ///   once ([`StreamRecvOutcome::Reset`]) regardless of `peek` (Linux
    ///   `sock_error` runs `xchg` before checking `MSG_PEEK`), then returns
    ///   [`StreamRecvOutcome::Eof`] if read-shut-down or
    ///   [`StreamRecvOutcome::WouldBlock`] otherwise.
    ///
    /// Never allocates.
    pub fn recv<'a>(
        &'a mut self,
        reader: End,
        buf: &mut [u8],
        peek: bool,
        check_creds: bool,
    ) -> StreamRecvOutcome<'a, F> {
        let rcv_shutdown = self.end_state(reader).recv_shutdown;
        let reset_pending = self.end_state(reader).reset_pending;

        if self.recv_dir(reader).ring.len() == 0 {
            if reset_pending {
                // Linux sock_error(sk) runs xchg(&sk->sk_err, 0) before MSG_PEEK.
                self.end_state_mut(reader).reset_pending = false;
                return StreamRecvOutcome::Reset;
            }
            return if rcv_shutdown { StreamRecvOutcome::Eof } else { StreamRecvOutcome::WouldBlock };
        }

        let dir = self.recv_dir_mut(reader);
        let plan = dir.plan_read(buf.len(), check_creds);
        dir.ring.copy_out(buf, plan.total_bytes);
        let sender = dir.segments.front().expect("non-empty ring has head segment").sender;

        if peek {
            let ancillary = plan
                .fds_seg_idx
                .and_then(|idx| dir.segments.get(idx))
                .and_then(|s| s.ancillary.as_ref())
                .map(RecvAncillary::Peeked);
            return StreamRecvOutcome::Data { bytes: plan.total_bytes, sender, ancillary, wake_writer: false };
        }

        let was_writable = dir.is_writable();
        let taken_anc = plan.fds_seg_idx.and_then(|idx| dir.segments.get_mut(idx)).and_then(|s| s.ancillary.take());
        if let Some(ref anc) = taken_anc {
            dir.in_flight_fds -= anc.fd_count();
        }

        for _ in 0..plan.full_segs {
            let popped = dir.segments.pop_front();
            debug_assert!(
                popped.as_ref().is_none_or(|s| s.ancillary.is_none()),
                "consumed segment must never drop undelivered fds"
            );
        }

        if plan.partial_last > 0
            && let Some(front) = dir.segments.front_mut()
        {
            debug_assert!(front.ancillary.is_none(), "partial segment fds must already have been detached");
            front.len -= plan.partial_last;
            debug_assert!(front.len > 0);
        }

        dir.ring.consume(plan.total_bytes);
        if dir.ring.len() == 0 {
            debug_assert!(dir.segments.is_empty());
            debug_assert_eq!(dir.in_flight_fds, 0);
        }

        let wake_writer = !was_writable && dir.is_writable();

        StreamRecvOutcome::Data {
            bytes: plan.total_bytes,
            sender,
            ancillary: taken_anc.map(RecvAncillary::Owned),
            wake_writer,
        }
    }

    /// Shut down reading, writing, or both on `end` (Linux v6.12
    /// `net/unix/af_unix.c` `unix_shutdown`).
    ///
    /// Propagates `SHUT_RD` to `end.peer()`'s send side (`BrokenPipe`) and
    /// `SHUT_WR` to `end.peer()`'s receive side (`Eof` once drained, `POLLRDHUP`).
    /// Already-queued bytes remain readable. Never allocates.
    pub fn shutdown(&mut self, end: End, how: ShutdownHow) {
        if how.shuts_read() {
            self.end_state_mut(end).recv_shutdown = true;
            self.end_state_mut(end.peer()).send_shutdown = true;
        }
        if how.shuts_write() {
            self.end_state_mut(end).send_shutdown = true;
            self.end_state_mut(end.peer()).recv_shutdown = true;
        }
    }

    /// Empty `end`'s receive queue without modifying shutdown, closed, or
    /// reset state, returning every undelivered [`Ancillary`] without
    /// allocating so the kernel's `AF_UNIX` garbage collector
    /// (`unix_collect_skb`) can break reference cycles outside any lock.
    ///
    /// Note: when the receive queue was non-empty, its segment ring is swapped
    /// out without allocating; if the pair were ever reused afterwards, the
    /// next send uses the existing `NeedSegment` / `grow_segments` protocol.
    pub fn purge_recv(&mut self, end: End) -> UndeliveredAncillary<F> {
        self.recv_dir_mut(end).take_all_undelivered()
    }

    /// Close `end` (Linux v6.12 `net/unix/af_unix.c` `unix_release_sock`).
    ///
    /// Calls `shutdown(end, ShutdownHow::Both)`, leaves `end.peer()`'s receive
    /// queue intact so the peer can finish reading bytes sent before close,
    /// and purges `end`'s own receive queue — returning every undelivered
    /// [`Ancillary`] without allocating. If `end`'s receive queue held unread
    /// bytes ([`UndeliveredAncillary::had_unread_bytes`]), sets the peer's
    /// pending `ECONNRESET` state (`POLLERR` + one [`StreamRecvOutcome::Reset`]
    /// once its queue drains).
    ///
    /// Idempotent. Never allocates.
    pub fn close(&mut self, end: End) -> UndeliveredAncillary<F> {
        self.close_inner(end, false)
    }

    /// Close `end` and unconditionally reset `end.peer()` (`ECONNRESET`),
    /// matching Linux `unix_release_sock(sk, /* embrion = */ 1)` when closing
    /// a [`Listener`](crate::Listener) with unaccepted pending connections.
    ///
    /// Idempotent. Never allocates.
    pub fn close_reset(&mut self, end: End) -> UndeliveredAncillary<F> {
        self.close_inner(end, true)
    }

    fn close_inner(&mut self, end: End, force_reset: bool) -> UndeliveredAncillary<F> {
        let already_closed = self.end_state(end).closed;
        self.end_state_mut(end).closed = true;
        self.shutdown(end, ShutdownHow::Both);
        if already_closed {
            return UndeliveredAncillary::empty();
        }
        let undel = self.recv_dir_mut(end).take_all_undelivered();
        if force_reset || undel.had_unread_bytes() {
            self.end_state_mut(end.peer()).reset_pending = true;
        }
        undel
    }

    /// Read and clear `end`'s pending `ECONNRESET` error (`getsockopt(SO_ERROR)`
    /// / Linux `sock_error(sk)`). Returns `true` if `ECONNRESET` was pending.
    /// Never allocates.
    pub fn take_error(&mut self, end: End) -> bool {
        mem::take(&mut self.end_state_mut(end).reset_pending)
    }

    /// Whether `end` has been closed via [`StreamPair::close`] or
    /// [`StreamPair::close_reset`].
    pub const fn is_closed(&self, end: End) -> bool {
        self.end_state(end).closed
    }

    /// `POLLIN | POLLRDNORM`: `true` if unread bytes are queued for `end` or
    /// `end`'s read side is shut down (`unix_poll`; errors are reported via
    /// [`StreamPair::err`] / `POLLERR`). Never allocates.
    pub const fn readable(&self, end: End) -> bool {
        self.recv_dir(end).ring.len() > 0 || self.end_state(end).recv_shutdown
    }

    /// `POLLOUT | POLLWRNORM`: `true` iff `wmem << 2 <= sndbuf` (Linux v6.12
    /// `net/unix/af_unix.c` `unix_writable` and `unix_poll`). Never allocates.
    pub fn writable(&self, end: End) -> bool {
        self.send_dir(end).is_writable()
    }

    /// `POLLRDHUP`: `true` if `end`'s read side is shut down (`unix_poll`:
    /// `sk->sk_shutdown & RCV_SHUTDOWN`). Never allocates.
    pub const fn rdhup(&self, end: End) -> bool {
        self.end_state(end).recv_shutdown
    }

    /// `POLLHUP`: `true` if both reading and writing are shut down on `end`
    /// (`unix_poll`: `sk->sk_shutdown == SHUTDOWN_MASK`). Never allocates.
    pub const fn hup(&self, end: End) -> bool {
        let st = self.end_state(end);
        st.recv_shutdown && st.send_shutdown
    }

    /// `POLLERR`: `true` if `end` has a pending `ECONNRESET` error (`unix_poll`:
    /// `sk->sk_err`). Never allocates.
    pub const fn err(&self, end: End) -> bool {
        self.end_state(end).reset_pending
    }

    /// Combined readiness snapshot for `end`. Never allocates.
    pub fn readiness(&self, end: End) -> Readiness {
        Readiness {
            readable: self.readable(end),
            writable: self.writable(end),
            rdhup: self.rdhup(end),
            hup: self.hup(end),
            err: self.err(end),
        }
    }
}
