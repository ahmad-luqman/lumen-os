//! Pure `no_std + alloc` POSIX pipe ring buffer and semantics.
//!
//! Models the byte buffer, capacity rules (`F_SETPIPE_SZ`), `PIPE_BUF` write
//! atomicity, partial writes, EOF and broken-pipe (`-EPIPE`/`SIGPIPE`)
//! decisions, `FIONREAD`, and `pipe_poll`-compatible readiness predicates for
//! Linux/POSIX pipes (`fs/pipe.c`, `pipe(7)`).
//!
//! No kernel locks, wait queues, file descriptors, or signal delivery live
//! here; the kernel wraps [`Pipe`] behind a lock and wait queues.
//!
//! # Allocation guarantee
//!
//! Heap allocation occurs **only** when creating a [`Pipe`] ([`Pipe::new`],
//! [`Pipe::with_capacity`]) or when [`Pipe::set_capacity`] changes the buffer
//! size. [`Pipe::read`], [`Pipe::write`], [`WriteProgress::step`],
//! [`Pipe::fionread`], and all readiness predicates never allocate.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::boxed::Box;
use core::fmt;

/// Page size in bytes (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// POSIX / Linux atomic pipe write guarantee (`PIPE_BUF`, 4 KiB).
///
/// Writes of at most `PIPE_BUF` bytes are all-or-nothing.
pub const PIPE_BUF: usize = 4096;

/// Minimum pipe buffer capacity (one page, 4 KiB, matching Linux `pipe_min_size`).
pub const MIN_CAPACITY: usize = PAGE_SIZE;

/// Default pipe buffer capacity (16 pages = 64 KiB, matching Linux
/// `PIPE_DEF_BUFFERS * PAGE_SIZE`).
pub const DEFAULT_CAPACITY: usize = 16 * PAGE_SIZE;

/// Default maximum pipe buffer capacity (1 MiB, matching Linux's default
/// `/proc/sys/fs/pipe-max-size`).
pub const DEFAULT_MAX_CAPACITY: usize = 1024 * 1024;

const _: () = {
    assert!(PAGE_SIZE.is_power_of_two());
    assert!(PIPE_BUF == PAGE_SIZE);
    assert!(MIN_CAPACITY == PAGE_SIZE);
    assert!(DEFAULT_CAPACITY.is_power_of_two());
    assert!(DEFAULT_CAPACITY >= MIN_CAPACITY);
    assert!(DEFAULT_MAX_CAPACITY.is_power_of_two());
    assert!(DEFAULT_MAX_CAPACITY >= DEFAULT_CAPACITY);
};

/// Round `bytes` up to a power-of-two multiple of [`PAGE_SIZE`], at least
/// [`MIN_CAPACITY`] (one page), matching Linux `fs/pipe.c` `round_pipe_size`.
///
/// Returns `None` if rounding up overflows `usize`.
pub const fn round_capacity(bytes: usize) -> Option<usize> {
    if bytes <= MIN_CAPACITY {
        Some(MIN_CAPACITY)
    } else {
        // Because MIN_CAPACITY (= PAGE_SIZE) is a power of two, any power of
        // two >= PAGE_SIZE is automatically a power-of-two multiple of pages.
        bytes.checked_next_power_of_two()
    }
}

/// Outcome of a single [`Pipe::write`] attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `n` bytes were copied into the pipe buffer.
    ///
    /// - For `data.is_empty()`, returns `Wrote(0)`.
    /// - For `data.len() <= PIPE_BUF`, `n` is always `data.len()` (atomic).
    /// - For `data.len() > PIPE_BUF`, `1 <= n <= data.len()` (partial if less
    ///   free space was available than `data.len()`).
    Wrote(usize),
    /// The write could not make progress right now:
    ///
    /// - For `data.len() <= PIPE_BUF`, fewer than `data.len()` bytes of free
    ///   space are available (writing a prefix would violate `PIPE_BUF`
    ///   atomicity).
    /// - For `data.len() > PIPE_BUF`, the buffer is completely full (`0` bytes
    ///   free).
    ///
    /// A non-blocking caller returns `-EAGAIN`; a blocking caller waits for
    /// space (or reader close) and retries.
    WouldBlock,
    /// No open readers remain. The kernel must deliver `SIGPIPE` and return
    /// `-EPIPE`.
    Broken,
}

/// Outcome of a single [`Pipe::read`] attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `n` bytes were copied out of the pipe buffer (`n <= buf.len()`, and
    /// never more than was queued). For `buf.is_empty()`, returns `Read(0)`.
    Read(usize),
    /// The buffer is empty and no open writers remain; the kernel returns `0`.
    Eof,
    /// The buffer is empty and at least one writer remains open.
    ///
    /// A non-blocking caller returns `-EAGAIN`; a blocking caller waits until
    /// [`Pipe::readable`] becomes true and retries.
    WouldBlock,
}

/// Error returned by [`Pipe::set_capacity`] and [`Pipe::with_capacity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetCapacityError {
    /// The rounded capacity exceeds `max_bytes` or overflows `usize`
    /// (`-EPERM` / `-EINVAL`).
    TooLarge,
    /// The rounded capacity is smaller than the bytes currently queued in the
    /// pipe (`-EBUSY`).
    Busy,
}

impl fmt::Display for SetCapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetCapacityError::TooLarge => f.write_str("requested pipe capacity exceeds maximum"),
            SetCapacityError::Busy => f.write_str("pipe holds more bytes than requested capacity"),
        }
    }
}

impl core::error::Error for SetCapacityError {}

/// Outcome of one iteration of [`WriteProgress::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteStep {
    /// All bytes have been written; contains the total byte count.
    Done(usize),
    /// More bytes remain and the pipe currently lacks room to continue.
    ///
    /// `wrote_now` is how many bytes were copied in this step (non-zero when
    /// a `> PIPE_BUF` write made partial progress). Wake waiting readers if
    /// `wrote_now > 0`, then wait until [`WriteProgress::can_progress`] is
    /// true or a signal interrupts the wait.
    NeedSpace { wrote_now: usize },
    /// No open readers remain (`SIGPIPE`).
    ///
    /// `written` is the number of bytes already transferred in earlier steps.
    /// Per Linux `fs/pipe.c` (`pipe_write`), the kernel delivers `SIGPIPE` and
    /// returns `written` if `written > 0`, or `-EPIPE` if `written == 0`.
    Broken { written: usize },
}

/// Outcome when a blocking write tracked by [`WriteProgress`] is interrupted
/// by a signal while sleeping for buffer space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptedWrite {
    /// At least one byte was written before the signal arrived; return this
    /// partial byte count to userspace.
    Partial(usize),
    /// Nothing was written yet; return `-EINTR` (or `-ERESTARTSYS` for
    /// `SA_RESTART`).
    Restart,
}

/// State machine driving a blocking `write(2)` of any size (including writes
/// larger than [`PIPE_BUF`]) across lock/wait/retry iterations.
///
/// Once a write's original total length exceeds [`PIPE_BUF`], POSIX and Linux
/// (`fs/pipe.c` `pipe_write`) treat the entire write as non-atomic: each
/// iteration writes whatever fits (even once the remaining tail drops below
/// [`PIPE_BUF`]), wakes waiting readers, and sleeps until more room appears,
/// all readers close, or a signal arrives. Writes whose original total length
/// is at most [`PIPE_BUF`] remain strictly atomic (all-or-nothing).
///
/// # Kernel loop
///
/// The kernel runs blocking `write(2)` roughly as follows:
///
/// ```ignore
/// let mut progress = WriteProgress::new(data.len());
/// loop {
///     let step = {
///         let mut pipe = inode.lock();
///         progress.step(&mut pipe, data)
///     };
///     match step {
///         WriteStep::Done(n) => {
///             if n > 0 {
///                 inode.wake_readers();
///             }
///             return Ok(n);
///         }
///         WriteStep::NeedSpace { wrote_now } => {
///             if wrote_now > 0 {
///                 inode.wake_readers();
///             }
///             // Sleep until `progress.can_progress(&pipe)` or a signal is pending.
///             if wait_for_write_progress(&inode, &progress).is_interrupted() {
///                 return match progress.interrupted() {
///                     InterruptedWrite::Partial(n) => Ok(n),
///                     InterruptedWrite::Restart => Err(EINTR_OR_ERESTARTSYS),
///                 };
///             }
///         }
///         WriteStep::Broken { written } => {
///             deliver_sigpipe_to_current();
///             return if written > 0 { Ok(written) } else { Err(EPIPE) };
///         }
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteProgress {
    total: usize,
    written: usize,
}

impl WriteProgress {
    /// Start tracking a blocking write of `total` bytes.
    pub const fn new(total: usize) -> Self {
        Self { total, written: 0 }
    }

    /// Total bytes requested by the write call.
    pub const fn total(&self) -> usize {
        self.total
    }

    /// Bytes already written into the pipe in completed steps.
    pub const fn written(&self) -> usize {
        self.written
    }

    /// Bytes still remaining to be written.
    pub const fn remaining(&self) -> usize {
        self.total - self.written
    }

    /// Whether this write requires `PIPE_BUF` atomicity (`total <= PIPE_BUF`).
    pub const fn is_atomic(&self) -> bool {
        self.total <= PIPE_BUF
    }

    /// Whether every byte has been written.
    pub const fn is_done(&self) -> bool {
        self.written == self.total
    }

    /// Whether calling [`WriteProgress::step`] on `pipe` will either finish,
    /// copy at least one byte, or observe [`WriteStep::Broken`].
    ///
    /// Use this as the wake/sleep predicate for the blocking write loop so a
    /// writer does not stall on a small pipe waiting for a full [`PIPE_BUF`]
    /// when fewer bytes suffice.
    pub const fn can_progress(&self, pipe: &Pipe) -> bool {
        if self.written == self.total || pipe.readers() == 0 {
            return true;
        }
        if self.is_atomic() { pipe.free_space() >= self.total } else { pipe.free_space() > 0 }
    }

    /// Execute one non-sleeping iteration of the blocking write against `pipe`.
    ///
    /// `data` must be the full original write slice (`data.len() == self.total()`).
    /// Never allocates.
    pub fn step(&mut self, pipe: &mut Pipe, data: &[u8]) -> WriteStep {
        assert_eq!(data.len(), self.total, "WriteProgress::step: data.len() must equal total");
        if self.written == self.total {
            return WriteStep::Done(self.total);
        }
        if pipe.readers == 0 {
            return WriteStep::Broken { written: self.written };
        }
        let rem = &data[self.written..];
        let free = pipe.free_space();
        if self.is_atomic() {
            if free >= rem.len() {
                pipe.push_bytes(rem);
                self.written = self.total;
                WriteStep::Done(self.total)
            } else {
                WriteStep::NeedSpace { wrote_now: 0 }
            }
        } else if free == 0 {
            WriteStep::NeedSpace { wrote_now: 0 }
        } else {
            let n = rem.len().min(free);
            pipe.push_bytes(&rem[..n]);
            self.written += n;
            if self.written == self.total { WriteStep::Done(self.total) } else { WriteStep::NeedSpace { wrote_now: n } }
        }
    }

    /// Decide the return value when a signal interrupts a wait between steps.
    ///
    /// Returns [`InterruptedWrite::Partial`] if any bytes were already
    /// written, or [`InterruptedWrite::Restart`] if nothing was written yet.
    pub const fn interrupted(&self) -> InterruptedWrite {
        if self.written > 0 { InterruptedWrite::Partial(self.written) } else { InterruptedWrite::Restart }
    }
}

/// A POSIX pipe byte ring buffer with open reader/writer reference counts.
#[derive(Debug)]
pub struct Pipe {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
    readers: usize,
    writers: usize,
}

impl Pipe {
    /// Create a pipe with [`DEFAULT_CAPACITY`] (16 pages = 64 KiB), one open
    /// reader, and one open writer.
    pub fn new() -> Self {
        Self { buf: alloc::vec![0u8; DEFAULT_CAPACITY].into_boxed_slice(), head: 0, len: 0, readers: 1, writers: 1 }
    }

    /// Create a pipe whose capacity is `bytes` rounded up per [`round_capacity`],
    /// bounded by `max_bytes`, with one open reader and one open writer.
    pub fn with_capacity(bytes: usize, max_bytes: usize) -> Result<Self, SetCapacityError> {
        let cap = round_capacity(bytes).ok_or(SetCapacityError::TooLarge)?;
        if cap > max_bytes {
            return Err(SetCapacityError::TooLarge);
        }
        Ok(Self { buf: alloc::vec![0u8; cap].into_boxed_slice(), head: 0, len: 0, readers: 1, writers: 1 })
    }

    /// Current buffer capacity in bytes (always a power-of-two multiple of
    /// [`PAGE_SIZE`]).
    pub const fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of unread bytes currently queued in the pipe.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the pipe holds zero unread bytes.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Free buffer space in bytes (`capacity() - len()`).
    pub const fn free_space(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Number of unread bytes queued, for the `FIONREAD` ioctl (Linux
    /// `fs/pipe.c` `pipe_ioctl`).
    pub const fn fionread(&self) -> usize {
        self.len
    }

    /// Number of open read ends.
    pub const fn readers(&self) -> usize {
        self.readers
    }

    /// Number of open write ends.
    pub const fn writers(&self) -> usize {
        self.writers
    }

    /// Record an additional open read end (e.g. `dup`/`fork`).
    pub fn add_reader(&mut self) {
        self.readers = self.readers.checked_add(1).expect("pipe reader count overflow");
    }

    /// Record an additional open write end (e.g. `dup`/`fork`).
    pub fn add_writer(&mut self) {
        self.writers = self.writers.checked_add(1).expect("pipe writer count overflow");
    }

    /// Close one read end and return the remaining reader count.
    ///
    /// Closing when `readers() == 0` is a no-op returning `0`.
    pub fn close_reader(&mut self) -> usize {
        self.readers = self.readers.saturating_sub(1);
        self.readers
    }

    /// Close one write end and return the remaining writer count.
    ///
    /// Closing when `writers() == 0` is a no-op returning `0`.
    pub fn close_writer(&mut self) -> usize {
        self.writers = self.writers.saturating_sub(1);
        self.writers
    }

    /// Resize the pipe buffer following Linux `F_SETPIPE_SZ` (`fs/pipe.c`
    /// `pipe_set_size` / `round_pipe_size`).
    ///
    /// `bytes` is rounded up to a power-of-two multiple of [`PAGE_SIZE`], at
    /// least one page ([`MIN_CAPACITY`]). Returns [`SetCapacityError::TooLarge`]
    /// if the rounded size exceeds `max_bytes` (or overflows), or
    /// [`SetCapacityError::Busy`] (`-EBUSY`) if the pipe currently holds more
    /// bytes than the rounded capacity.
    ///
    /// On success, preserves all queued bytes in FIFO order and returns the
    /// actual new capacity in bytes. If the rounded capacity equals the
    /// current capacity, returns `Ok(cap)` without allocating.
    pub fn set_capacity(&mut self, bytes: usize, max_bytes: usize) -> Result<usize, SetCapacityError> {
        let new_cap = round_capacity(bytes).ok_or(SetCapacityError::TooLarge)?;
        if new_cap > max_bytes {
            return Err(SetCapacityError::TooLarge);
        }
        if self.len > new_cap {
            return Err(SetCapacityError::Busy);
        }
        if new_cap == self.buf.len() {
            return Ok(new_cap);
        }

        let mut new_buf = alloc::vec![0u8; new_cap].into_boxed_slice();
        if self.len > 0 {
            let cap = self.buf.len();
            let first = self.len.min(cap - self.head);
            new_buf[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
            let second = self.len - first;
            if second > 0 {
                new_buf[first..self.len].copy_from_slice(&self.buf[..second]);
            }
        }
        self.buf = new_buf;
        self.head = 0;
        Ok(new_cap)
    }

    /// Attempt to write `data` into the pipe (Linux `fs/pipe.c` `pipe_write`,
    /// `pipe(7)`).
    ///
    /// Rules guaranteed:
    /// - An empty write (`data.is_empty()`) succeeds immediately with
    ///   [`WriteOutcome::Wrote(0)`], even when the buffer is full or no
    ///   readers remain (matching Linux `pipe_write`'s early null-write check).
    /// - For any non-empty write, if no readers remain, returns
    ///   [`WriteOutcome::Broken`] immediately — taking precedence over
    ///   [`WriteOutcome::WouldBlock`] even when the buffer is full.
    /// - Writes of at most [`PIPE_BUF`] bytes are atomic: if fewer than
    ///   `data.len()` bytes are free, writes nothing and returns
    ///   [`WriteOutcome::WouldBlock`].
    /// - Writes larger than [`PIPE_BUF`] may be partial: writes
    ///   `data.len().min(free_space())` bytes if any room exists, or returns
    ///   [`WriteOutcome::WouldBlock`] only when `free_space() == 0`.
    ///
    /// Both blocking (`nonblocking == false`) and non-blocking
    /// (`nonblocking == true`) single-step attempts produce the same
    /// [`WriteOutcome`]: on `WouldBlock` (or after a partial write of a
    /// `> PIPE_BUF` buffer), a non-blocking caller returns immediately while a
    /// blocking caller sleeps and loops (see [`WriteProgress`]).
    ///
    /// Never allocates.
    pub fn write(&mut self, data: &[u8], nonblocking: bool) -> WriteOutcome {
        let _ = nonblocking;
        if data.is_empty() {
            return WriteOutcome::Wrote(0);
        }
        if self.readers == 0 {
            return WriteOutcome::Broken;
        }
        let free = self.free_space();
        if data.len() <= PIPE_BUF {
            if free < data.len() {
                return WriteOutcome::WouldBlock;
            }
            self.push_bytes(data);
            WriteOutcome::Wrote(data.len())
        } else {
            if free == 0 {
                return WriteOutcome::WouldBlock;
            }
            let n = data.len().min(free);
            self.push_bytes(&data[..n]);
            WriteOutcome::Wrote(n)
        }
    }

    /// Attempt to read bytes from the pipe into `buf` (Linux `fs/pipe.c`
    /// `pipe_read`, `pipe(7)`).
    ///
    /// Rules guaranteed:
    /// - An empty buffer (`buf.is_empty()`) returns [`ReadOutcome::Read(0)`]
    ///   immediately.
    /// - If data is queued (`len() > 0`), copies `buf.len().min(len())` bytes
    ///   in FIFO order and returns [`ReadOutcome::Read`], even when no writers
    ///   remain. Never returns more than is currently queued.
    /// - If empty and `writers() == 0`, returns [`ReadOutcome::Eof`].
    /// - If empty and `writers() > 0`, returns [`ReadOutcome::WouldBlock`]
    ///   (non-blocking callers return `-EAGAIN`; blocking callers wait until
    ///   [`Pipe::readable`] and retry).
    ///
    /// Never allocates.
    pub fn read(&mut self, buf: &mut [u8], nonblocking: bool) -> ReadOutcome {
        let _ = nonblocking;
        if buf.is_empty() {
            return ReadOutcome::Read(0);
        }
        if self.len > 0 {
            let n = self.pop_bytes(buf);
            return ReadOutcome::Read(n);
        }
        if self.writers == 0 { ReadOutcome::Eof } else { ReadOutcome::WouldBlock }
    }

    /// Read-end readiness: `true` if data is queued or no writers remain (so a
    /// `read(2)` will not block).
    ///
    /// Note: when constructing a Linux `poll(2)` mask from `fs/pipe.c`
    /// `pipe_poll`, `POLLIN | POLLRDNORM` corresponds to `!pipe.is_empty()`
    /// while `POLLHUP` corresponds to [`Pipe::reader_hung_up`]; `select(2)`
    /// and blocking `read(2)` treat `readable()` as ready.
    pub const fn readable(&self) -> bool {
        self.len > 0 || self.writers == 0
    }

    /// Write-end room readiness, matching Linux `fs/pipe.c` `pipe_poll`.
    ///
    /// In Linux `fs/pipe.c`, `pipe_poll` sets `EPOLLOUT | EPOLLWRNORM` iff
    /// `!pipe_full(head, tail, pipe->max_usage)` — meaning at least one full
    /// `PAGE_SIZE` (`== PIPE_BUF` = 4096 bytes) slot is free, so an atomic
    /// `PIPE_BUF` write is guaranteed not to block (POSIX `poll`/`select`
    /// contract). For a flat byte ring of capacity `M * PAGE_SIZE`,
    /// `ceil(len / PAGE_SIZE) < M` is algebraically identical to
    /// `capacity - len >= PIPE_BUF`.
    ///
    /// As in `fs/pipe.c` `pipe_poll`, this depends solely on buffer room and
    /// is independent of `readers()`; a missing reader is signalled separately
    /// by [`Pipe::writer_error`] (`POLLERR`).
    pub const fn writable(&self) -> bool {
        self.free_space() >= PIPE_BUF
    }

    /// `POLLHUP` for the read end: `true` when no writers remain open
    /// (Linux `fs/pipe.c` `pipe_poll`: `if (!pipe->writers) mask |= EPOLLHUP`),
    /// regardless of whether unread bytes are still queued.
    pub const fn reader_hung_up(&self) -> bool {
        self.writers == 0
    }

    /// `POLLERR` for the write end: `true` when no readers remain open
    /// (Linux `fs/pipe.c` `pipe_poll`: `if (!pipe->readers) mask |= EPOLLERR`),
    /// regardless of buffer occupancy.
    pub const fn writer_error(&self) -> bool {
        self.readers == 0
    }

    fn push_bytes(&mut self, src: &[u8]) {
        let n = src.len();
        debug_assert!(n <= self.free_space());
        if n == 0 {
            return;
        }
        let cap = self.buf.len();
        let tail = if self.len < cap - self.head { self.head + self.len } else { self.len - (cap - self.head) };
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        let second = n - first;
        if second > 0 {
            self.buf[..second].copy_from_slice(&src[first..]);
        }
        self.len += n;
    }

    fn pop_bytes(&mut self, dst: &mut [u8]) -> usize {
        let n = dst.len().min(self.len);
        if n == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        let second = n - first;
        if second > 0 {
            dst[first..n].copy_from_slice(&self.buf[..second]);
            self.head = second;
        } else {
            let next_head = self.head + first;
            self.head = if next_head == cap { 0 } else { next_head };
        }
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
        n
    }
}

impl Default for Pipe {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::vec;
    use std::vec::Vec;

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

    #[test]
    fn default_pipe_geometry_and_counts() {
        let mut p = Pipe::new();
        assert_eq!(p.capacity(), 65_536);
        assert_eq!(p.capacity(), 16 * PAGE_SIZE);
        assert_eq!(p.len(), 0);
        assert!(p.is_empty());
        assert_eq!(p.free_space(), 65_536);
        assert_eq!(p.fionread(), 0);
        assert_eq!(p.readers(), 1);
        assert_eq!(p.writers(), 1);

        p.add_reader();
        p.add_writer();
        assert_eq!(p.readers(), 2);
        assert_eq!(p.writers(), 2);
        assert_eq!(p.close_reader(), 1);
        assert_eq!(p.close_reader(), 0);
        assert_eq!(p.close_reader(), 0);
        assert_eq!(p.close_writer(), 1);
        assert_eq!(p.close_writer(), 0);
        assert_eq!(p.close_writer(), 0);
    }

    #[test]
    fn atomic_writes_at_pipe_buf_and_pipe_buf_plus_one() {
        for cap in [PAGE_SIZE, DEFAULT_CAPACITY] {
            let mut p = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
            // Fill so exactly PIPE_BUF - 1 bytes remain free.
            let prefill = cap - (PIPE_BUF - 1);
            let filler = vec![0xAA; prefill];
            assert_eq!(p.write(&filler, false), WriteOutcome::Wrote(prefill));
            assert_eq!(p.free_space(), PIPE_BUF - 1);

            let atomic = vec![0x11; PIPE_BUF];
            // Neither blocking nor non-blocking may split a PIPE_BUF write.
            assert_eq!(p.write(&atomic, true), WriteOutcome::WouldBlock);
            assert_eq!(p.write(&atomic, false), WriteOutcome::WouldBlock);
            assert_eq!(p.len(), prefill);

            // A smaller atomic write that does fit succeeds atomically, while
            // one byte past available free space does not.
            let exact_fit = vec![0x22; PIPE_BUF - 1];
            let mut small_check = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
            assert_eq!(small_check.write(&filler, true), WriteOutcome::Wrote(prefill));
            assert_eq!(small_check.write(&exact_fit, true), WriteOutcome::Wrote(PIPE_BUF - 1));
            assert_eq!(small_check.free_space(), 0);

            // Free 1 byte so exactly PIPE_BUF bytes are free: atomic write now succeeds.
            let mut one = [0u8; 1];
            assert_eq!(p.read(&mut one, false), ReadOutcome::Read(1));
            assert_eq!(one, [0xAA]);
            assert_eq!(p.free_space(), PIPE_BUF);
            assert_eq!(p.write(&atomic, true), WriteOutcome::Wrote(PIPE_BUF));
            assert_eq!(p.free_space(), 0);

            // Drain and re-verify for blocking mode at exact PIPE_BUF boundary.
            let mut drained = vec![0u8; cap];
            assert_eq!(p.read(&mut drained, false), ReadOutcome::Read(cap));
            assert_eq!(&drained[..prefill - 1], &vec![0xAA; prefill - 1][..]);
            assert_eq!(&drained[prefill - 1..], &atomic[..]);

            assert_eq!(p.write(&filler[..prefill - 1], false), WriteOutcome::Wrote(prefill - 1));
            assert_eq!(p.free_space(), PIPE_BUF);
            assert_eq!(p.write(&atomic, false), WriteOutcome::Wrote(PIPE_BUF));
            assert_eq!(p.free_space(), 0);

            // Now test PIPE_BUF + 1 with PIPE_BUF - 1 bytes free: both modes may
            // write partially (non-atomic once > PIPE_BUF).
            for nonblocking in [true, false] {
                let mut q = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
                assert_eq!(q.write(&filler, nonblocking), WriteOutcome::Wrote(prefill));
                assert_eq!(q.free_space(), PIPE_BUF - 1);

                let over: Vec<u8> = (0..PIPE_BUF + 1).map(|i| (i & 0xFF) as u8).collect();
                assert_eq!(q.write(&over, nonblocking), WriteOutcome::Wrote(PIPE_BUF - 1));
                assert_eq!(q.free_space(), 0);
                // When completely full, PIPE_BUF + 1 returns WouldBlock.
                assert_eq!(q.write(&over, nonblocking), WriteOutcome::WouldBlock);

                let mut out = vec![0u8; cap];
                assert_eq!(q.read(&mut out, nonblocking), ReadOutcome::Read(cap));
                assert_eq!(&out[..prefill], &filler[..]);
                assert_eq!(&out[prefill..], &over[..PIPE_BUF - 1]);
            }
        }
    }

    #[test]
    fn partial_nonblocking_writes() {
        let mut p = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let seed = vec![0x55; 2 * PAGE_SIZE - 1000];
        assert_eq!(p.write(&seed, true), WriteOutcome::Wrote(seed.len()));
        assert_eq!(p.free_space(), 1000);

        // Non-blocking write > PIPE_BUF writes exactly the 1000 bytes that fit.
        let payload: Vec<u8> = (0..PIPE_BUF + 500).map(|i| (i.wrapping_mul(3) & 0xFF) as u8).collect();
        assert_eq!(p.write(&payload, true), WriteOutcome::Wrote(1000));
        assert_eq!(p.free_space(), 0);

        // Now completely full: non-blocking write returns WouldBlock.
        assert_eq!(p.write(&payload[1000..], true), WriteOutcome::WouldBlock);

        // Zero-length write succeeds even when full.
        assert_eq!(p.write(&[], true), WriteOutcome::Wrote(0));

        // Verify exact prefix was written.
        let mut out = vec![0u8; 2 * PAGE_SIZE];
        assert_eq!(p.read(&mut out, true), ReadOutcome::Read(2 * PAGE_SIZE));
        assert_eq!(&out[..seed.len()], &seed[..]);
        assert_eq!(&out[seed.len()..], &payload[..1000]);

        // Writing more than the entire pipe capacity into an empty pipe writes
        // exactly `capacity()` bytes.
        let huge: Vec<u8> = (0..3 * PAGE_SIZE).map(|i| (i & 0xFF) as u8).collect();
        assert_eq!(p.write(&huge, true), WriteOutcome::Wrote(2 * PAGE_SIZE));
        assert_eq!(p.write(&huge[2 * PAGE_SIZE..], true), WriteOutcome::WouldBlock);
        let mut out2 = vec![0u8; 2 * PAGE_SIZE];
        assert_eq!(p.read(&mut out2, true), ReadOutcome::Read(2 * PAGE_SIZE));
        assert_eq!(&out2[..], &huge[..2 * PAGE_SIZE]);
    }

    #[test]
    fn broken_pipe_takes_precedence() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        p.add_reader();
        // Fill completely so any non-empty write would otherwise WouldBlock.
        let full = vec![0xCC; PAGE_SIZE];
        assert_eq!(p.write(&full, false), WriteOutcome::Wrote(PAGE_SIZE));
        assert_eq!(p.write(b"x", true), WriteOutcome::WouldBlock);

        // Closing one of two readers still leaves a live reader -> WouldBlock.
        assert_eq!(p.close_reader(), 1);
        assert_eq!(p.write(b"x", true), WriteOutcome::WouldBlock);

        // Closing the final reader makes Broken take precedence over WouldBlock
        // for atomic, large, blocking, and non-blocking writes alike.
        assert_eq!(p.close_reader(), 0);
        let big = vec![0x77; PIPE_BUF + 10];
        let atomic = vec![0x88; PIPE_BUF];
        for nonblocking in [true, false] {
            assert_eq!(p.write(b"x", nonblocking), WriteOutcome::Broken);
            assert_eq!(p.write(&atomic, nonblocking), WriteOutcome::Broken);
            assert_eq!(p.write(&big, nonblocking), WriteOutcome::Broken);
            // Null write still succeeds with Wrote(0) (Linux fs/pipe.c pipe_write).
            assert_eq!(p.write(&[], nonblocking), WriteOutcome::Wrote(0));
        }
        // Buffer contents untouched.
        assert_eq!(p.len(), PAGE_SIZE);

        // Broken also wins on a nearly-full pipe and an empty pipe.
        let mut progress = WriteProgress::new(big.len());
        assert!(progress.can_progress(&p));
        assert_eq!(progress.step(&mut p, &big), WriteStep::Broken { written: 0 });

        let mut empty = Pipe::new();
        assert_eq!(empty.close_reader(), 0);
        assert_eq!(empty.write(b"hi", false), WriteOutcome::Broken);
        assert_eq!(empty.write(b"hi", true), WriteOutcome::Broken);
    }

    #[test]
    fn eof_only_when_no_writers_and_buffer_empty() {
        let mut p = Pipe::new();
        p.add_writer();
        assert_eq!(p.write(b"hello world", false), WriteOutcome::Wrote(11));

        // Closing one of two writers: data still readable, empty gives WouldBlock.
        assert_eq!(p.close_writer(), 1);
        let mut buf = [0xFFu8; 20];
        assert_eq!(p.read(&mut buf[..5], false), ReadOutcome::Read(5));
        assert_eq!(&buf[..5], b"hello");

        // Closing the last writer while data remains: must drain all data first.
        assert_eq!(p.close_writer(), 0);
        let mut buf2 = [0xEEu8; 20];
        assert_eq!(p.read(&mut buf2, false), ReadOutcome::Read(6));
        assert_eq!(&buf2[..6], b" world");
        // Bytes beyond what was queued must not be touched.
        assert!(buf2[6..].iter().all(|&b| b == 0xEE));

        // Now empty with zero writers: both blocking and non-blocking return Eof.
        assert_eq!(p.read(&mut buf2, false), ReadOutcome::Eof);
        assert_eq!(p.read(&mut buf2, true), ReadOutcome::Eof);

        // Zero-length read returns Read(0) in every state.
        assert_eq!(p.read(&mut [], false), ReadOutcome::Read(0));
        assert_eq!(p.read(&mut [], true), ReadOutcome::Read(0));

        // Empty with a writer open: WouldBlock (not Eof).
        let mut q = Pipe::new();
        assert_eq!(q.read(&mut buf2, false), ReadOutcome::WouldBlock);
        assert_eq!(q.read(&mut buf2, true), ReadOutcome::WouldBlock);
        assert_eq!(q.read(&mut [], true), ReadOutcome::Read(0));
    }

    #[test]
    fn readiness_predicates_match_pipe_poll() {
        // Test both a multi-page (16-page) pipe and a minimum 1-page pipe.
        for cap in [PAGE_SIZE, DEFAULT_CAPACITY] {
            let occupancies = if cap == PAGE_SIZE {
                vec![0, 1, PAGE_SIZE - 1, PAGE_SIZE]
            } else {
                vec![0, 1, cap - PIPE_BUF, cap - PIPE_BUF + 1, cap - 1, cap]
            };

            for &occ in &occupancies {
                for &readers in &[1usize, 0] {
                    for &writers in &[1usize, 0] {
                        let mut p = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
                        if occ > 0 {
                            let data = vec![0x42; occ];
                            assert_eq!(p.write(&data, false), WriteOutcome::Wrote(occ));
                        }
                        if readers == 0 {
                            p.close_reader();
                        }
                        if writers == 0 {
                            p.close_writer();
                        }

                        assert_eq!(
                            p.readable(),
                            occ > 0 || writers == 0,
                            "readable mismatch for cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.writable(),
                            cap - occ >= PIPE_BUF,
                            "writable mismatch for cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.reader_hung_up(),
                            writers == 0,
                            "reader_hung_up mismatch for cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.writer_error(),
                            readers == 0,
                            "writer_error mismatch for cap={cap} occ={occ} r={readers} w={writers}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn set_capacity_rounding_bounds_and_ebusy() {
        // Direct round_capacity checks.
        assert_eq!(round_capacity(0), Some(PAGE_SIZE));
        assert_eq!(round_capacity(1), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE - 1), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE + 1), Some(2 * PAGE_SIZE));
        assert_eq!(round_capacity(2 * PAGE_SIZE), Some(2 * PAGE_SIZE));
        assert_eq!(round_capacity(3 * PAGE_SIZE), Some(4 * PAGE_SIZE));
        assert_eq!(round_capacity(DEFAULT_CAPACITY), Some(DEFAULT_CAPACITY));
        assert_eq!(round_capacity(DEFAULT_MAX_CAPACITY), Some(DEFAULT_MAX_CAPACITY));
        assert_eq!(round_capacity((1usize << (usize::BITS - 1)) + 1), None);
        assert_eq!(round_capacity(usize::MAX), None);

        let mut p = Pipe::new();
        // Minimum: 0 and 1 round up to 1 page.
        assert_eq!(p.set_capacity(0, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));
        assert_eq!(p.capacity(), PAGE_SIZE);
        assert_eq!(p.set_capacity(1, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));
        assert_eq!(p.capacity(), PAGE_SIZE);

        // Rounding up to power-of-two pages.
        assert_eq!(p.set_capacity(PAGE_SIZE + 1, DEFAULT_MAX_CAPACITY), Ok(2 * PAGE_SIZE));
        assert_eq!(p.capacity(), 2 * PAGE_SIZE);
        assert_eq!(p.set_capacity(3 * PAGE_SIZE - 10, DEFAULT_MAX_CAPACITY), Ok(4 * PAGE_SIZE));
        assert_eq!(p.capacity(), 4 * PAGE_SIZE);

        // Maximum checks (default 1 MiB and custom smaller max).
        assert_eq!(p.set_capacity(DEFAULT_MAX_CAPACITY, DEFAULT_MAX_CAPACITY), Ok(DEFAULT_MAX_CAPACITY));
        assert_eq!(p.capacity(), DEFAULT_MAX_CAPACITY);
        assert_eq!(p.set_capacity(DEFAULT_MAX_CAPACITY + 1, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::TooLarge));
        assert_eq!(p.set_capacity(usize::MAX, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::TooLarge));
        assert_eq!(p.set_capacity(PAGE_SIZE, PAGE_SIZE - 1), Err(SetCapacityError::TooLarge));
        assert_eq!(p.set_capacity(4 * PAGE_SIZE + 1, 4 * PAGE_SIZE), Err(SetCapacityError::TooLarge));
        assert_eq!(p.capacity(), DEFAULT_MAX_CAPACITY);

        // Shrink with wrapped data: EBUSY if contents wouldn't fit, exact
        // preservation when they do.
        assert_eq!(p.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(2 * PAGE_SIZE));
        // Advance head to create a wrap-around.
        let dummy = vec![0x99; 6000];
        assert_eq!(p.write(&dummy, false), WriteOutcome::Wrote(6000));
        let mut discard = vec![0u8; 5500];
        assert_eq!(p.read(&mut discard, false), ReadOutcome::Read(5500));
        // 500 bytes at [5500..6000]; now write 4500 bytes that wrap past 8192.
        let wrapped: Vec<u8> = (0..4500).map(|i| (i & 0xFF) as u8).collect();
        assert_eq!(p.write(&wrapped, false), WriteOutcome::Wrote(4500));
        assert_eq!(p.len(), 5000);

        // 4096 < 5000 -> Busy, capacity and contents unchanged.
        assert_eq!(p.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Busy));
        assert_eq!(p.capacity(), 2 * PAGE_SIZE);
        assert_eq!(p.len(), 5000);

        // Growing from 2 pages to 4 pages preserves wrapped bytes.
        assert_eq!(p.set_capacity(3 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(4 * PAGE_SIZE));
        assert_eq!(p.capacity(), 4 * PAGE_SIZE);
        assert_eq!(p.len(), 5000);

        // Same capacity is a no-op.
        assert_eq!(p.set_capacity(4 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(4 * PAGE_SIZE));

        // Drain 904 bytes so exactly 4096 remain, then shrink to 1 page.
        let mut first = vec![0u8; 904];
        assert_eq!(p.read(&mut first, false), ReadOutcome::Read(904));
        assert_eq!(&first[..500], &vec![0x99; 500][..]);
        assert_eq!(&first[500..], &wrapped[..404]);
        assert_eq!(p.len(), PAGE_SIZE);

        assert_eq!(p.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));
        assert_eq!(p.capacity(), PAGE_SIZE);
        let mut rest = vec![0u8; PAGE_SIZE];
        assert_eq!(p.read(&mut rest, false), ReadOutcome::Read(PAGE_SIZE));
        assert_eq!(&rest[..], &wrapped[404..]);
        assert!(p.is_empty());

        // with_capacity enforces max_bytes as well.
        assert_eq!(Pipe::with_capacity(8192, 4096).unwrap_err(), SetCapacityError::TooLarge);
    }

    #[test]
    fn fionread_tracks_queued_bytes() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(p.fionread(), 0);

        assert_eq!(p.write(b"abcdef", false), WriteOutcome::Wrote(6));
        assert_eq!(p.fionread(), 6);

        let fill = vec![1u8; PAGE_SIZE - 10];
        assert_eq!(p.write(&fill, false), WriteOutcome::Wrote(PAGE_SIZE - 10));
        assert_eq!(p.fionread(), PAGE_SIZE - 4);

        // Failed WouldBlock write does not change FIONREAD.
        assert_eq!(p.write(&[2u8; 5], true), WriteOutcome::WouldBlock);
        assert_eq!(p.fionread(), PAGE_SIZE - 4);

        // Partial read reduces FIONREAD accurately.
        let mut out = [0u8; 100];
        assert_eq!(p.read(&mut out, false), ReadOutcome::Read(100));
        assert_eq!(p.fionread(), PAGE_SIZE - 104);

        // Closing writer leaves queued bytes visible to FIONREAD until drained.
        p.close_writer();
        assert_eq!(p.fionread(), PAGE_SIZE - 104);

        let mut rest = vec![0u8; PAGE_SIZE];
        assert_eq!(p.read(&mut rest, false), ReadOutcome::Read(PAGE_SIZE - 104));
        assert_eq!(p.fionread(), 0);
    }

    #[test]
    fn ring_wrap_around_preserves_order() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let mut next_in: u64 = 0;
        let mut next_out: u64 = 0;

        // Prime-sized interleaved writes and reads so head/tail wrap around the
        // 4096-byte buffer dozens of times without ever emptying the ring.
        assert_eq!(p.write(&[0u8; 1], false), WriteOutcome::Wrote(1));
        next_in += 1;

        let write_sizes = [307, 509, 1013, 199, 701, 419];
        let read_sizes = [173, 401, 613, 251, 521, 353];
        for round in 0..80 {
            let wlen = write_sizes[round % write_sizes.len()];
            if p.free_space() >= wlen {
                let data: Vec<u8> = (0..wlen).map(|i| ((next_in + i as u64) & 0xFF) as u8).collect();
                assert_eq!(p.write(&data, false), WriteOutcome::Wrote(wlen));
                next_in += wlen as u64;
            }
            let rlen = read_sizes[round % read_sizes.len()].min(p.len().saturating_sub(1));
            if rlen > 0 {
                let mut buf = vec![0u8; rlen];
                assert_eq!(p.read(&mut buf, false), ReadOutcome::Read(rlen));
                for (i, &b) in buf.iter().enumerate() {
                    assert_eq!(b, ((next_out + i as u64) & 0xFF) as u8, "round {round} byte {i}");
                }
                next_out += rlen as u64;
            }
            assert_eq!(p.fionread(), (next_in - next_out) as usize);
        }

        // Exact boundary wrap: leave head at cap - 1, write across the wrap,
        // fill to exact capacity, and drain completely.
        let mut drain = vec![0u8; p.len()];
        assert_eq!(p.read(&mut drain, false), ReadOutcome::Read(drain.len()));
        for (i, &b) in drain.iter().enumerate() {
            assert_eq!(b, ((next_out + i as u64) & 0xFF) as u8);
        }

        let setup = vec![0xA5; PAGE_SIZE - 1];
        assert_eq!(p.write(&setup, false), WriteOutcome::Wrote(PAGE_SIZE - 1));
        let mut scratch = vec![0u8; PAGE_SIZE - 1];
        assert_eq!(p.read(&mut scratch[..PAGE_SIZE - 2], false), ReadOutcome::Read(PAGE_SIZE - 2));
        // Now 1 byte sits at offset PAGE_SIZE - 2; write PAGE_SIZE - 1 bytes to
        // fill the ring completely across the boundary.
        let span: Vec<u8> = (0..PAGE_SIZE - 1).map(|i| (i & 0xFF) as u8).collect();
        assert_eq!(p.write(&span, false), WriteOutcome::Wrote(PAGE_SIZE - 1));
        assert_eq!(p.len(), PAGE_SIZE);
        let mut full_out = vec![0u8; PAGE_SIZE];
        assert_eq!(p.read(&mut full_out, false), ReadOutcome::Read(PAGE_SIZE));
        assert_eq!(full_out[0], 0xA5);
        assert_eq!(&full_out[1..], &span[..]);
    }

    #[test]
    fn write_progress_interleaves_large_blocking_write_with_reads() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let total: usize = 20_000;
        let payload: Vec<u8> = (0..total).map(|i| (i.wrapping_mul(17) & 0xFF) as u8).collect();
        let mut progress = WriteProgress::new(total);
        assert!(!progress.is_atomic());
        assert_eq!(progress.total(), total);
        assert_eq!(progress.written(), 0);
        assert_eq!(progress.remaining(), total);

        let mut received = Vec::with_capacity(total);
        // Varying read chunk sizes including tiny 13-byte reads so the tail
        // (< PIPE_BUF remaining) proves it continues without stalling for an
        // atomic chunk.
        let read_chunks = [1000, 13, 4096, 37, 2500, 7, 1500, 999];
        let mut step_idx = 0usize;

        loop {
            assert!(progress.can_progress(&p));
            match progress.step(&mut p, &payload) {
                WriteStep::Done(n) => {
                    assert_eq!(n, total);
                    assert!(progress.is_done());
                    assert_eq!(progress.remaining(), 0);
                    break;
                }
                WriteStep::NeedSpace { wrote_now } => {
                    assert!(wrote_now > 0, "step made zero progress despite can_progress");
                    assert!(!progress.can_progress(&p));
                    // Verify calling step while still full yields NeedSpace { wrote_now: 0 }.
                    assert_eq!(progress.step(&mut p, &payload), WriteStep::NeedSpace { wrote_now: 0 });
                    // Interleave a read.
                    let want = read_chunks[step_idx % read_chunks.len()];
                    step_idx += 1;
                    let mut buf = vec![0u8; want];
                    match p.read(&mut buf, false) {
                        ReadOutcome::Read(r) => {
                            assert!(r > 0);
                            received.extend_from_slice(&buf[..r]);
                        }
                        other => panic!("unexpected read outcome: {other:?}"),
                    }
                }
                WriteStep::Broken { .. } => panic!("unexpected broken pipe"),
            }
        }

        // Drain whatever remains in the pipe.
        let mut tail = vec![0u8; PAGE_SIZE];
        while !p.is_empty() {
            match p.read(&mut tail, false) {
                ReadOutcome::Read(r) => received.extend_from_slice(&tail[..r]),
                other => panic!("unexpected tail read: {other:?}"),
            }
        }
        assert_eq!(received, payload);

        // Signal interruption before any progress -> Restart; after partial -> Partial(n).
        let mut p2 = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let fill = vec![0x10; PAGE_SIZE];
        assert_eq!(p2.write(&fill, false), WriteOutcome::Wrote(PAGE_SIZE));
        let mut prog_blocked = WriteProgress::new(5000);
        let data5k = vec![0x20; 5000];
        assert!(!prog_blocked.can_progress(&p2));
        assert_eq!(prog_blocked.step(&mut p2, &data5k), WriteStep::NeedSpace { wrote_now: 0 });
        assert_eq!(prog_blocked.interrupted(), InterruptedWrite::Restart);

        // Free 300 bytes -> partial write of 300 bytes, then interrupted -> Partial(300).
        let mut sink = [0u8; 300];
        assert_eq!(p2.read(&mut sink, false), ReadOutcome::Read(300));
        assert!(prog_blocked.can_progress(&p2));
        assert_eq!(prog_blocked.step(&mut p2, &data5k), WriteStep::NeedSpace { wrote_now: 300 });
        assert_eq!(prog_blocked.interrupted(), InterruptedWrite::Partial(300));

        // Closing the reader after partial progress yields Broken { written: 300 }.
        p2.close_reader();
        assert!(prog_blocked.can_progress(&p2));
        assert_eq!(prog_blocked.step(&mut p2, &data5k), WriteStep::Broken { written: 300 });

        // Atomic WriteProgress (== PIPE_BUF): waits until full PIPE_BUF is free.
        let mut p3 = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(p3.write(&[0x33; 10], false), WriteOutcome::Wrote(10));
        let atomic = vec![0x44; PIPE_BUF];
        let mut prog_atomic = WriteProgress::new(PIPE_BUF);
        assert!(prog_atomic.is_atomic());
        assert!(!prog_atomic.can_progress(&p3));
        assert_eq!(prog_atomic.step(&mut p3, &atomic), WriteStep::NeedSpace { wrote_now: 0 });
        assert_eq!(prog_atomic.interrupted(), InterruptedWrite::Restart);
        let mut small = [0u8; 10];
        assert_eq!(p3.read(&mut small, false), ReadOutcome::Read(10));
        assert!(prog_atomic.can_progress(&p3));
        assert_eq!(prog_atomic.step(&mut p3, &atomic), WriteStep::Done(PIPE_BUF));
        assert_eq!(prog_atomic.step(&mut p3, &atomic), WriteStep::Done(PIPE_BUF));
    }

    #[test]
    fn randomized_model_matches_vecdeque() {
        let mut rng = SplitMix64(0xDEAD_BEEF_CAFE_0342);
        let max_cap = 8 * PAGE_SIZE;

        for initial_cap in [PAGE_SIZE, 2 * PAGE_SIZE, 4 * PAGE_SIZE] {
            let mut pipe = Pipe::with_capacity(initial_cap, max_cap).unwrap();
            let mut model: VecDeque<u8> = VecDeque::new();
            let mut model_cap = initial_cap;
            let mut model_readers: usize = 1;
            let mut model_writers: usize = 1;
            let mut byte_seq: u8 = 0;

            for step in 0..4_000 {
                let op = rng.gen_range(100);
                if op < 40 {
                    // Write (mix of 0, small, around PIPE_BUF, and large).
                    let len = match rng.gen_range(8) {
                        0 => 0,
                        1 => 1 + rng.gen_range(64),
                        2 => PIPE_BUF - 1,
                        3 => PIPE_BUF,
                        4 => PIPE_BUF + 1,
                        5 => 1 + rng.gen_range(PIPE_BUF),
                        6 => PIPE_BUF + 1 + rng.gen_range(2 * PAGE_SIZE),
                        _ => model_cap.saturating_sub(model.len()),
                    };
                    let nonblocking = rng.gen_range(2) == 0;
                    let mut data = Vec::with_capacity(len);
                    for _ in 0..len {
                        data.push(byte_seq);
                        byte_seq = byte_seq.wrapping_add(1);
                    }

                    let expected = if data.is_empty() {
                        WriteOutcome::Wrote(0)
                    } else if model_readers == 0 {
                        WriteOutcome::Broken
                    } else {
                        let free = model_cap - model.len();
                        if data.len() <= PIPE_BUF {
                            if free < data.len() {
                                WriteOutcome::WouldBlock
                            } else {
                                model.extend(&data);
                                WriteOutcome::Wrote(data.len())
                            }
                        } else if free == 0 {
                            WriteOutcome::WouldBlock
                        } else {
                            let n = data.len().min(free);
                            model.extend(&data[..n]);
                            WriteOutcome::Wrote(n)
                        }
                    };

                    let got = pipe.write(&data, nonblocking);
                    assert_eq!(got, expected, "step {step}: write len={len} nb={nonblocking}");
                    // No <= PIPE_BUF write may ever be split.
                    if data.len() <= PIPE_BUF
                        && let WriteOutcome::Wrote(n) = got
                    {
                        assert_eq!(n, data.len(), "step {step}: split atomic write");
                    }
                } else if op < 80 {
                    // Read.
                    let len = match rng.gen_range(5) {
                        0 => 0,
                        1 => 1 + rng.gen_range(64),
                        2 => 1 + rng.gen_range(PIPE_BUF),
                        3 => PIPE_BUF + rng.gen_range(PAGE_SIZE),
                        _ => model.len().max(1),
                    };
                    let nonblocking = rng.gen_range(2) == 0;
                    let mut buf = vec![0xEE; len];
                    let got = pipe.read(&mut buf, nonblocking);

                    let expected = if len == 0 {
                        ReadOutcome::Read(0)
                    } else if !model.is_empty() {
                        let n = len.min(model.len());
                        let expected_bytes: Vec<u8> = model.drain(..n).collect();
                        assert_eq!(&buf[..n], &expected_bytes[..], "step {step}: data mismatch");
                        assert!(buf[n..].iter().all(|&b| b == 0xEE), "step {step}: overwrote past read len");
                        ReadOutcome::Read(n)
                    } else if model_writers == 0 {
                        ReadOutcome::Eof
                    } else {
                        ReadOutcome::WouldBlock
                    };
                    assert_eq!(got, expected, "step {step}: read len={len} nb={nonblocking}");
                } else if op < 90 {
                    // set_capacity.
                    let req = match rng.gen_range(6) {
                        0 => 0,
                        1 => rng.gen_range(PAGE_SIZE + 1),
                        2 => PAGE_SIZE + rng.gen_range(max_cap),
                        3 => max_cap,
                        4 => max_cap + 1,
                        _ => usize::MAX,
                    };
                    let expected = match round_capacity(req) {
                        None => Err(SetCapacityError::TooLarge),
                        Some(r) if r > max_cap => Err(SetCapacityError::TooLarge),
                        Some(r) if model.len() > r => Err(SetCapacityError::Busy),
                        Some(r) => {
                            model_cap = r;
                            Ok(r)
                        }
                    };
                    assert_eq!(pipe.set_capacity(req, max_cap), expected, "step {step}: set_capacity({req})");
                } else {
                    // Open/close readers and writers (occasionally hitting 0, then
                    // re-adding so the simulation continues exercising everything).
                    match rng.gen_range(4) {
                        0 => {
                            if model_readers < 3 {
                                pipe.add_reader();
                                model_readers += 1;
                            }
                        }
                        1 => {
                            let rem = pipe.close_reader();
                            model_readers = model_readers.saturating_sub(1);
                            assert_eq!(rem, model_readers);
                        }
                        2 => {
                            if model_writers < 3 {
                                pipe.add_writer();
                                model_writers += 1;
                            }
                        }
                        _ => {
                            let rem = pipe.close_writer();
                            model_writers = model_writers.saturating_sub(1);
                            assert_eq!(rem, model_writers);
                        }
                    }
                }

                // Check all invariants & readiness predicates after every step.
                assert_eq!(pipe.capacity(), model_cap, "step {step}: capacity");
                assert_eq!(pipe.len(), model.len(), "step {step}: len");
                assert_eq!(pipe.fionread(), model.len(), "step {step}: fionread");
                assert_eq!(pipe.is_empty(), model.is_empty(), "step {step}: is_empty");
                assert_eq!(pipe.free_space(), model_cap - model.len(), "step {step}: free_space");
                assert_eq!(pipe.readers(), model_readers, "step {step}: readers");
                assert_eq!(pipe.writers(), model_writers, "step {step}: writers");
                assert_eq!(pipe.readable(), !model.is_empty() || model_writers == 0, "step {step}: readable");
                assert_eq!(pipe.writable(), model_cap - model.len() >= PIPE_BUF, "step {step}: writable");
                assert_eq!(pipe.reader_hung_up(), model_writers == 0, "step {step}: reader_hung_up");
                assert_eq!(pipe.writer_error(), model_readers == 0, "step {step}: writer_error");
            }
        }
    }
}
