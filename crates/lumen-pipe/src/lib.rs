//! Pure `no_std + alloc` POSIX pipe ring buffer and semantics.
//!
//! Models the byte buffer, capacity rules (`F_SETPIPE_SZ`), `PIPE_BUF` write
//! atomicity, partial writes, EOF and broken-pipe (`-EPIPE`/`SIGPIPE`)
//! decisions, `FIONREAD`, borrow-guarded `peek`/`commit`, readiness predicates,
//! and explicit wake signals for Linux/POSIX pipes (`fs/pipe.c`, `pipe(7)`).
//!
//! No kernel locks, wait queues, file descriptors, or signal delivery live
//! here; see [`Pipe`] for the locking contract and [`WriteProgress`] for the
//! kernel read/write loops and wake rules.
//!
//! # Allocation guarantee
//!
//! Heap allocation occurs **only** via fallible `try_reserve_exact` when
//! creating a [`Pipe`] ([`Pipe::try_new`], [`Pipe::fifo`],
//! [`Pipe::with_capacity`]) or when [`Pipe::set_capacity`] changes the buffer
//! size. [`Pipe::read`], [`Pipe::peek`], [`Peeked::commit`],
//! [`Peeked::commit_prefix`], [`Pipe::write`], [`WriteProgress::step`],
//! [`Pipe::fionread`], and all readiness predicates never allocate.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroUsize;
use core::ops::Range;

/// Page size in bytes (4 KiB on x86-64).
pub const PAGE_SIZE: usize = 4096;

/// Linux atomic pipe write guarantee (`PIPE_BUF`, 4 KiB; POSIX's minimum is
/// 512 bytes, and `PAGE_SIZE == PIPE_BUF` holds on x86-64).
///
/// Writes of at most `PIPE_BUF` bytes are all-or-nothing.
pub const PIPE_BUF: usize = 4096;

/// Minimum pipe buffer capacity (one page, 4 KiB, enforced by Linux
/// `fs/pipe.c` `round_pipe_size`).
pub const MIN_CAPACITY: usize = PAGE_SIZE;

/// Default pipe buffer capacity (16 pages = 64 KiB, matching Linux
/// `PIPE_DEF_BUFFERS * PAGE_SIZE`).
pub const DEFAULT_CAPACITY: usize = 16 * PAGE_SIZE;

/// Default maximum pipe buffer capacity (1 MiB, matching Linux's default
/// `/proc/sys/fs/pipe-max-size`).
pub const DEFAULT_MAX_CAPACITY: usize = 1024 * 1024;

/// Maximum input accepted by Linux `fs/pipe.c` `round_pipe_size` (`1 << 31`).
const MAX_ROUND_INPUT: usize = 1usize << 31;

// Load-bearing: an atomic write of `PIPE_BUF` bytes must always be able to fit
// once the pipe drains, even at minimum capacity.
const _: () = assert!(MIN_CAPACITY >= PIPE_BUF);

/// Round `bytes` up to a power-of-two multiple of [`PAGE_SIZE`], at least
/// [`MIN_CAPACITY`] (one page), matching Linux `fs/pipe.c` `round_pipe_size`.
///
/// Returns `None` if `bytes > (1 << 31)` (Linux returns `0` / `-EINVAL`).
pub const fn round_capacity(bytes: usize) -> Option<usize> {
    if bytes > MAX_ROUND_INPUT {
        None
    } else if bytes <= MIN_CAPACITY {
        Some(MIN_CAPACITY)
    } else {
        bytes.checked_next_power_of_two()
    }
}

/// Fallible buffer allocation failed (`-ENOMEM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMemory;

/// Error returned by [`Pipe::with_capacity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError {
    /// Requested size exceeds `1 << 31` (Linux `round_pipe_size` ceiling, `-EINVAL`).
    Invalid,
    /// Rounded capacity exceeds `max_bytes` (`-EPERM`).
    OverLimit,
    /// Fallible heap allocation for the pipe buffer failed (`-ENOMEM`).
    NoMemory,
}

impl From<NoMemory> for CreateError {
    fn from(_: NoMemory) -> Self {
        Self::NoMemory
    }
}

/// Error returned by [`Pipe::set_capacity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetCapacityError {
    /// Requested size exceeds `1 << 31` (Linux `round_pipe_size` ceiling, `-EINVAL`).
    Invalid,
    /// Rounded capacity exceeds `max_bytes` while growing (`-EPERM`).
    OverLimit,
    /// Rounded capacity is smaller than the bytes currently queued (`-EBUSY`).
    Busy,
    /// Fallible heap allocation for the pipe buffer failed (`-ENOMEM`).
    NoMemory,
}

impl From<NoMemory> for SetCapacityError {
    fn from(_: NoMemory) -> Self {
        Self::NoMemory
    }
}

/// Result of a successful [`Pipe::set_capacity`] call.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resized {
    /// Actual new buffer capacity in bytes (power-of-two multiple of [`PAGE_SIZE`]).
    pub capacity: usize,
    /// `true` if capacity grew; the kernel must wake waiting writers and pollers.
    pub wake_writers: bool,
}

/// Result of [`Pipe::release_reader`] or [`Pipe::release_writer`].
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Released {
    /// Closed the last open file description on this end (count reached `0`).
    /// The kernel must wake **all** waiters on the opposite end and all pollers
    /// (`wake_up_interruptible_all`, matching Linux `pipe_release`).
    Last,
    /// At least one open file description still remains on this end.
    Remaining(NonZeroUsize),
    /// Count was already `0` (caller bug: double release). Does not panic; the
    /// kernel should log an error.
    Underflow,
}

/// Outcome of a single non-blocking [`Pipe::write`] attempt.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `n` bytes were copied into the pipe buffer.
    Wrote(usize),
    /// Not enough room right now (`-EAGAIN`).
    WouldBlock,
    /// No open readers remain (`SIGPIPE` and `-EPIPE`).
    Broken,
}

/// Outcome of a single non-blocking [`Pipe::read`] attempt.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `n` bytes were copied out of the pipe buffer.
    Read(usize),
    /// Buffer is empty and no open writers remain (return `0`).
    Eof,
    /// Buffer is empty and at least one writer remains open (`-EAGAIN`).
    WouldBlock,
}

/// Outcome of [`Pipe::peek`].
#[must_use]
#[derive(Debug)]
pub enum PeekOutcome<'a> {
    /// Bytes were copied into the caller's destination buffer and stay queued
    /// until committed via [`Peeked::commit`] or [`Peeked::commit_prefix`].
    /// A zero-length destination buffer always yields `Data` with `len() == 0`.
    Data(Peeked<'a>),
    /// Buffer is empty and no open writers remain (return `0`).
    Eof,
    /// Buffer is empty and at least one writer remains open (`-EAGAIN`).
    WouldBlock,
}

/// Result of committing a [`Peeked`] read.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Consumed {
    /// Number of bytes removed from the pipe.
    pub n: usize,
    /// `true` if `n > 0` and the pipe was not [`Pipe::writable`] before the
    /// read; the kernel must wake a waiting writer (exclusive wake).
    pub wake_writers: bool,
}

/// Borrow guard returned by [`Pipe::peek`], holding `&mut Pipe` so no other
/// operation can interleave before the read is committed or abandoned.
///
/// Dropping a `Peeked` without committing leaves every byte queued in the pipe
/// (the `-EFAULT` path when `copy_to_user` faults). Intentionally neither
/// `Clone` nor `Copy`:
///
/// ```compile_fail
/// use lumen_pipe::{PeekOutcome, Pipe};
/// let mut pipe = Pipe::try_new().unwrap();
/// let mut dst = [0u8; 4];
/// if let PeekOutcome::Data(p) = pipe.peek(&mut dst) {
///     let _dup = p.clone();
/// }
/// ```
#[must_use]
#[derive(Debug)]
pub struct Peeked<'a> {
    pipe: &'a mut Pipe,
    len: usize,
}

impl Peeked<'_> {
    /// Number of bytes copied into the destination slice by [`Pipe::peek`].
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether zero bytes were peeked (only when the caller passed an empty slice).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remove all [`Peeked::len`] peeked bytes from the pipe.
    pub fn commit(self) -> Consumed {
        let n = self.len;
        self.commit_prefix(n)
    }

    /// Remove the first `k.min(self.len())` peeked bytes from the pipe.
    ///
    /// Never consumes more bytes than were peeked and never panics.
    pub fn commit_prefix(self, k: usize) -> Consumed {
        let n = k.min(self.len);
        let was_writable = self.pipe.writable();
        if n > 0 {
            let cap = self.pipe.buf.len();
            self.pipe.head = (self.pipe.head + n) % cap;
            self.pipe.len -= n;
            if self.pipe.len == 0 {
                self.pipe.head = 0;
            }
        }
        Consumed { n, wake_writers: n > 0 && !was_writable }
    }
}

/// Error returned by [`WriteProgress::step`] when the supplied chunk violates
/// the write's contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkError {
    /// `next` was empty while bytes still remained to be written.
    Empty,
    /// `next.len()` exceeded the bytes remaining in the write.
    TooLong,
    /// An atomic write (`total <= PIPE_BUF`) was given a partial slice; atomic
    /// writes must be supplied as the full remainder so they commit
    /// all-or-nothing.
    SplitAtomic,
}

/// Outcome of one iteration of [`WriteProgress::step`].
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteStep {
    /// Every byte of the write has been placed into the pipe.
    Done {
        /// Total bytes written.
        total: usize,
        /// `true` if `total > 0` and any buffer space remains free; the kernel
        /// must wake the next exclusive waiting writer (Linux `wake_next_writer`).
        wake_next_writer: bool,
    },
    /// Consumed `wrote_now` bytes from `next` (all or a prefix of `next`);
    /// more bytes remain and [`WriteProgress::can_progress`] is still `true`,
    /// so the caller may immediately fetch [`WriteProgress::next_chunk`] and
    /// step again without sleeping.
    Continue { wrote_now: usize },
    /// Consumed `wrote_now` bytes from `next` (`0` if the pipe lacked the
    /// required room); more bytes remain and the caller must sleep until
    /// [`WriteProgress::can_progress`] holds or a signal interrupts the wait.
    NeedSpace { wrote_now: usize },
    /// No open readers remain (`SIGPIPE`).
    ///
    /// `written` is the number of bytes already transferred in earlier steps.
    /// Per Linux `fs/pipe.c` (`anon_pipe_write`), the kernel delivers `SIGPIPE`
    /// and returns `written` if `written > 0`, or `-EPIPE` if `written == 0`.
    Broken { written: usize },
}

/// Outcome when a blocking write tracked by [`WriteProgress`] is interrupted
/// by a signal while sleeping for buffer space.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptedWrite {
    /// At least one byte was written before the signal arrived; return this
    /// partial byte count to userspace.
    Partial(usize),
    /// Nothing was written yet; return `-ERESTARTSYS` (which signal delivery
    /// turns into `-EINTR` or a syscall restart when `SA_RESTART` is set).
    Restart,
}

/// Outcome when `copy_from_user` faults while preparing a chunk for
/// [`WriteProgress`].
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultedWrite {
    /// At least one byte was already written before the fault; return this
    /// partial byte count to userspace (matching Linux `anon_pipe_write`).
    Partial(usize),
    /// Nothing was written yet; return `-EFAULT`.
    Fault,
}

/// State machine driving a blocking `write(2)` of any size across chunked
/// copies from userspace and lock/wait/retry iterations.
///
/// POSIX permits splitting writes larger than [`PIPE_BUF`]; Linux's
/// `anon_pipe_write` merges a sub-page remainder into the last buffer first and
/// then fills page slots one at a time, sleeping for a free slot. Here, a
/// non-atomic write (`total > PIPE_BUF`) steps only when at least
/// `min(remaining, PIPE_BUF)` bytes are free — the byte-ring counterpart of a
/// free page slot — so a large writer cannot refill every freed byte and
/// starve small atomic writers or thrash on tiny reads. Writes of at most
/// [`PIPE_BUF`] bytes remain strictly atomic and require only their own length
/// in free space.
///
/// Intentionally neither `Copy` nor `Clone`: stepping a stale copy would
/// duplicate bytes into the pipe:
///
/// ```compile_fail
/// use lumen_pipe::WriteProgress;
/// let p = WriteProgress::new(16);
/// let _dup = p.clone();
/// ```
///
/// # Wake rules (writer and reader wait queues)
///
/// Writer waits are **exclusive** (matching Linux `wr_wait`), so each wake
/// unblocks one waiting writer; any waiter whose [`WriteProgress::can_progress`]
/// turns true is woken by Rule 1:
///
/// - **Writer wake rule 1:** a read (`commit` / `commit_prefix`) that frees
///   `>= 1` byte from a pipe that was not [`Pipe::writable`] before the read
///   returns [`Consumed::wake_writers`] `== true` -> wake one waiting writer
///   (and pollers when `writable()` becomes true).
/// - **Writer wake rule 2:** a growing [`Pipe::set_capacity`] returns
///   [`Resized::wake_writers`] `== true` -> wake waiting writers and pollers.
/// - **Writer wake rule 3:** a writer that leaves after having slept, by
///   **any** exit (`Done`, `Broken`, copy fault, or interrupted), wakes the
///   next waiting writer if any space is free (`pipe.fionread() < pipe.capacity()`,
///   provided directly as [`WriteStep::Done::wake_next_writer`] on `Done`).
/// - **Reader wake rules:** a write that copies `>= 1` byte wakes waiting
///   readers; a reader that leaves data queued (`!pipe.is_empty()` after
///   `commit` / `commit_prefix`) wakes the next reader (Linux `wake_next_reader`);
///   [`Released::Last`] on either end wakes **all** waiters on the opposite end
///   and all pollers (`wake_up_interruptible_all`).
///
/// # Blocking kernel write loop
///
/// Check readers **before** the first `copy_from_user` so a bad pointer with no
/// readers returns `SIGPIPE` + `-EPIPE` rather than `-EFAULT`. On an
/// interrupted wait, retake the lock and step once more before checking the
/// signal, returning [`WriteProgress::interrupted`] only if that step is
/// [`WriteStep::NeedSpace`]. A [`ChunkError`] indicates a kernel bug: log it
/// and return `-EIO` (never `.expect()`).
///
/// ```ignore
/// if user_len == 0 {
///     return Ok(0);
/// }
/// if inode.lock().writer_error() {
///     deliver_sigpipe_to_current();
///     return Err(EPIPE);
/// }
/// let mut progress = WriteProgress::new(user_len);
/// let mut kbuf = [0u8; PIPE_BUF];
/// let mut slept = false;
/// let mut interrupted = false;
/// loop {
///     let range = progress.next_chunk(kbuf.len());
///     if copy_from_user(&mut kbuf[..range.len()], user_ptr + range.start).is_err() {
///         if slept {
///             let pipe = inode.lock();
///             if pipe.fionread() < pipe.capacity() {
///                 drop(pipe);
///                 inode.wake_next_writer();
///             }
///         }
///         return match progress.faulted() {
///             FaultedWrite::Partial(n) => Ok(n),
///             FaultedWrite::Fault => Err(EFAULT),
///         };
///     }
///     let step = {
///         let mut pipe = inode.lock();
///         match progress.step(&mut pipe, &kbuf[..range.len()]) {
///             Ok(step) => step,
///             Err(err) => {
///                 log::error!("pipe: invalid write chunk: {err:?}");
///                 let wake = slept && pipe.fionread() < pipe.capacity();
///                 drop(pipe);
///                 if wake {
///                     inode.wake_next_writer();
///                 }
///                 return Err(EIO);
///             }
///         }
///     };
///     match step {
///         WriteStep::Done { total, wake_next_writer } => {
///             if total > 0 {
///                 inode.wake_readers();
///             }
///             if wake_next_writer {
///                 inode.wake_next_writer();
///             }
///             return Ok(total);
///         }
///         WriteStep::Continue { wrote_now } => {
///             if wrote_now > 0 {
///                 inode.wake_readers();
///             }
///         }
///         WriteStep::NeedSpace { wrote_now } => {
///             if wrote_now > 0 {
///                 inode.wake_readers();
///             }
///             if interrupted {
///                 if slept {
///                     let pipe = inode.lock();
///                     if pipe.fionread() < pipe.capacity() {
///                         drop(pipe);
///                         inode.wake_next_writer();
///                     }
///                 }
///                 return match progress.interrupted() {
///                     InterruptedWrite::Partial(n) => Ok(n),
///                     InterruptedWrite::Restart => Err(ERESTARTSYS),
///                 };
///             }
///             slept = true;
///             interrupted = wait_for_write_progress(&inode, &progress).is_interrupted();
///         }
///         WriteStep::Broken { written } => {
///             if slept {
///                 let pipe = inode.lock();
///                 if pipe.fionread() < pipe.capacity() {
///                     drop(pipe);
///                     inode.wake_next_writer();
///                 }
///             }
///             deliver_sigpipe_to_current();
///             return if written > 0 { Ok(written) } else { Err(EPIPE) };
///         }
///     }
/// }
/// ```
///
/// # Non-blocking (`O_NONBLOCK`) kernel write loop
///
/// For `user_len <= PIPE_BUF`, check `writer_error()` first, copy all
/// `user_len` bytes into `kbuf`, and call [`Pipe::write`] once. For
/// `user_len > PIPE_BUF` with chunked copies, loop under the pipe `SleepMutex`
/// so partial writes up to `free` bytes never trip the `<= PIPE_BUF` atomic
/// rule on a small chunk:
///
/// ```ignore
/// if user_len == 0 {
///     return Ok(0);
/// }
/// let mut written = 0usize;
/// let mut kbuf = [0u8; PIPE_BUF];
/// let mut pipe = inode.lock();
/// while written < user_len {
///     if pipe.writer_error() {
///         deliver_sigpipe_to_current();
///         return if written > 0 { Ok(written) } else { Err(EPIPE) };
///     }
///     let avail = pipe.capacity() - pipe.fionread();
///     let chunk_len = if user_len <= PIPE_BUF {
///         if avail < user_len {
///             return Err(EAGAIN);
///         }
///         user_len
///     } else {
///         if avail == 0 {
///             break;
///         }
///         (user_len - written).min(avail).min(kbuf.len())
///     };
///     if copy_from_user(&mut kbuf[..chunk_len], user_ptr + written).is_err() {
///         return if written > 0 { Ok(written) } else { Err(EFAULT) };
///     }
///     match pipe.write(&kbuf[..chunk_len]) {
///         WriteOutcome::Wrote(n) => written += n,
///         WriteOutcome::WouldBlock => break,
///         WriteOutcome::Broken => {
///             deliver_sigpipe_to_current();
///             return if written > 0 { Ok(written) } else { Err(EPIPE) };
///         }
///     }
/// }
/// if written > 0 {
///     inode.wake_readers();
///     Ok(written)
/// } else {
///     Err(EAGAIN)
/// }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct WriteProgress {
    total: usize,
    written: usize,
}

impl WriteProgress {
    /// Start tracking a blocking write of `total` bytes.
    pub const fn new(total: usize) -> Self {
        Self { total, written: 0 }
    }

    const fn is_atomic(&self) -> bool {
        self.total <= PIPE_BUF
    }

    const fn remaining(&self) -> usize {
        self.total - self.written
    }

    /// Byte range within the caller's write buffer to supply to the next
    /// [`WriteProgress::step`] call, capped at `max` bytes for non-atomic
    /// writes.
    ///
    /// For atomic writes (`total <= PIPE_BUF`), always returns the entire
    /// remainder (`0..total`) regardless of `max` so the write can commit
    /// all-or-nothing. Returns an empty range (`total..total`) once complete.
    pub const fn next_chunk(&self, max: usize) -> Range<usize> {
        let rem = self.remaining();
        let len = if self.is_atomic() || rem <= max { rem } else { max };
        self.written..(self.written + len)
    }

    /// Whether calling [`WriteProgress::step`] on `pipe` will either finish,
    /// copy at least one byte, or observe [`WriteStep::Broken`] (at the time
    /// of the check).
    pub fn can_progress(&self, pipe: &Pipe) -> bool {
        let rem = self.remaining();
        rem == 0 || pipe.readers == 0 || pipe.allowed(rem, true) > 0
    }

    /// Execute one non-sleeping iteration of the blocking write against `pipe`,
    /// consuming a prefix of `next`.
    ///
    /// Check order (matching Linux `fs/pipe.c` `anon_pipe_write`):
    /// 1. Validate `next` against the remaining byte count.
    /// 2. If nothing remains (including `WriteProgress::new(0)` with `&[]`),
    ///    return [`WriteStep::Done`] before checking readers.
    /// 3. If no readers remain, return [`WriteStep::Broken`] before writing
    ///    any byte.
    /// 4. Otherwise write up to `next.len()` bytes if the pipe meets the
    ///    progress threshold (`min(remaining, PIPE_BUF)`).
    ///
    /// # Errors
    ///
    /// Returns [`ChunkError::TooLong`] if `next.len()` exceeds remaining bytes,
    /// [`ChunkError::Empty`] if `next` is empty while bytes remain, or
    /// [`ChunkError::SplitAtomic`] if an atomic write (`total <= PIPE_BUF`) is
    /// given fewer bytes than its full remainder.
    ///
    /// Never allocates.
    pub fn step(&mut self, pipe: &mut Pipe, next: &[u8]) -> Result<WriteStep, ChunkError> {
        let rem = self.remaining();
        if next.len() > rem {
            return Err(ChunkError::TooLong);
        }
        if rem == 0 {
            return Ok(WriteStep::Done { total: self.total, wake_next_writer: false });
        }
        if next.is_empty() {
            return Err(ChunkError::Empty);
        }
        if self.is_atomic() && next.len() != rem {
            return Err(ChunkError::SplitAtomic);
        }
        if pipe.readers == 0 {
            return Ok(WriteStep::Broken { written: self.written });
        }

        let allowed = pipe.allowed(rem, true);
        if allowed == 0 {
            return Ok(WriteStep::NeedSpace { wrote_now: 0 });
        }
        let n = allowed.min(next.len());
        pipe.push_bytes(&next[..n]);
        self.written += n;
        if self.remaining() == 0 {
            Ok(WriteStep::Done { total: self.total, wake_next_writer: pipe.free_space() > 0 })
        } else if self.can_progress(pipe) {
            Ok(WriteStep::Continue { wrote_now: n })
        } else {
            Ok(WriteStep::NeedSpace { wrote_now: n })
        }
    }

    /// Decide the return value when a signal interrupts a wait and a
    /// subsequent [`WriteProgress::step`] still returns [`WriteStep::NeedSpace`].
    pub const fn interrupted(&self) -> InterruptedWrite {
        if self.written > 0 { InterruptedWrite::Partial(self.written) } else { InterruptedWrite::Restart }
    }

    /// Decide the return value when `copy_from_user` faults while fetching the
    /// next chunk.
    pub const fn faulted(&self) -> FaultedWrite {
        if self.written > 0 { FaultedWrite::Partial(self.written) } else { FaultedWrite::Fault }
    }
}

/// A POSIX pipe byte ring buffer with open reader/writer file-description counts.
///
/// The kernel must protect a `Pipe` with a `SleepMutex` (never an IRQ-off
/// `SpinLock`), since [`Pipe::set_capacity`] allocates and copies under it and
/// read/write paths may copy to/from user memory while holding it.
pub struct Pipe {
    buf: Vec<u8>,
    head: usize,
    len: usize,
    readers: usize,
    writers: usize,
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe")
            .field("capacity", &self.buf.len())
            .field("len", &self.len)
            .field("readers", &self.readers)
            .field("writers", &self.writers)
            .finish()
    }
}

fn alloc_buf(cap: usize) -> Result<Vec<u8>, NoMemory> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(cap).map_err(|_| NoMemory)?;
    buf.resize(cap, 0);
    Ok(buf)
}

impl Pipe {
    const fn with_buf(buf: Vec<u8>, readers: usize, writers: usize) -> Self {
        Self { buf, head: 0, len: 0, readers, writers }
    }

    /// Create an anonymous pipe with [`DEFAULT_CAPACITY`] (16 pages = 64 KiB),
    /// one open read file description, and one open write file description.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] (`-ENOMEM`) if buffer allocation fails.
    pub fn try_new() -> Result<Self, NoMemory> {
        Ok(Self::with_buf(alloc_buf(DEFAULT_CAPACITY)?, 1, 1))
    }

    /// Create a pipe buffer with [`DEFAULT_CAPACITY`] and `0` open readers /
    /// `0` open writers (for future FIFO support; ends are added with
    /// [`Pipe::open_reader`] / [`Pipe::open_writer`]).
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] (`-ENOMEM`) if buffer allocation fails.
    pub fn fifo() -> Result<Self, NoMemory> {
        Ok(Self::with_buf(alloc_buf(DEFAULT_CAPACITY)?, 0, 0))
    }

    /// Create an anonymous pipe whose capacity is `bytes` rounded up per
    /// [`round_capacity`], bounded by `max_bytes` (typically
    /// [`DEFAULT_MAX_CAPACITY`], or `usize::MAX` with `CAP_SYS_RESOURCE`),
    /// with one open read description and one open write description.
    ///
    /// # Errors
    ///
    /// - [`CreateError::Invalid`]: `bytes > (1 << 31)`.
    /// - [`CreateError::OverLimit`]: rounded capacity exceeds `max_bytes`.
    /// - [`CreateError::NoMemory`]: buffer allocation failed.
    pub fn with_capacity(bytes: usize, max_bytes: usize) -> Result<Self, CreateError> {
        let cap = round_capacity(bytes).ok_or(CreateError::Invalid)?;
        if cap > max_bytes {
            return Err(CreateError::OverLimit);
        }
        Ok(Self::with_buf(alloc_buf(cap)?, 1, 1))
    }

    /// Current buffer capacity in bytes (always a power-of-two multiple of
    /// [`PAGE_SIZE`]).
    pub const fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Whether the pipe holds zero unread bytes.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of unread bytes queued, for the `FIONREAD` ioctl (Linux
    /// `fs/pipe.c` `pipe_ioctl`).
    pub const fn fionread(&self) -> usize {
        self.len
    }

    const fn free_space(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Number of open read file descriptions.
    pub const fn readers(&self) -> usize {
        self.readers
    }

    /// Number of open write file descriptions.
    pub const fn writers(&self) -> usize {
        self.writers
    }

    /// Record an additional open read file description (e.g. a FIFO `open` or
    /// `/proc/<pid>/fd` reopen; opening from `0` is allowed).
    ///
    /// Do **not** call on `dup`/`fork`: duplicated descriptors share one open
    /// file description and release once when its last reference drops.
    ///
    /// # Panics
    ///
    /// Panics on `usize` overflow.
    pub fn open_reader(&mut self) {
        self.readers = self.readers.checked_add(1).expect("pipe reader count overflow");
    }

    /// Record an additional open write file description (e.g. a FIFO `open` or
    /// `/proc/<pid>/fd` reopen; opening from `0` is allowed).
    ///
    /// Do **not** call on `dup`/`fork`: duplicated descriptors share one open
    /// file description and release once when its last reference drops.
    ///
    /// # Panics
    ///
    /// Panics on `usize` overflow.
    pub fn open_writer(&mut self) {
        self.writers = self.writers.checked_add(1).expect("pipe writer count overflow");
    }

    /// Release one open read file description (when its last reference is
    /// dropped).
    ///
    /// On [`Released::Last`], wake **all** waiting writers and pollers
    /// (`wake_up_interruptible_all`, Linux `pipe_release`). On
    /// [`Released::Underflow`], the count was already `0`: does not panic;
    /// log a kernel bug.
    pub fn release_reader(&mut self) -> Released {
        release_end(&mut self.readers)
    }

    /// Release one open write file description (when its last reference is
    /// dropped).
    ///
    /// On [`Released::Last`], wake **all** waiting readers and pollers
    /// (`wake_up_interruptible_all`, Linux `pipe_release`). On
    /// [`Released::Underflow`], the count was already `0`: does not panic;
    /// log a kernel bug.
    pub fn release_writer(&mut self) -> Released {
        release_end(&mut self.writers)
    }

    /// Resize the pipe buffer following Linux `F_SETPIPE_SZ` (`fs/pipe.c`
    /// `pipe_set_size` / `round_pipe_size`).
    ///
    /// Callers handling `fcntl(F_SETPIPE_SZ, arg)` should truncate `arg` to
    /// `u32` first (matching Linux's `unsigned int arg`), and pass
    /// `max_bytes` as `pipe-max-size` (default [`DEFAULT_MAX_CAPACITY`]) or
    /// `usize::MAX` for processes with `CAP_SYS_RESOURCE`.
    ///
    /// - Rounds `bytes` up to a power-of-two multiple of [`PAGE_SIZE`] (at
    ///   least one page).
    /// - Enforces `max_bytes` against the *rounded* size only when growing
    ///   (`new_cap > capacity()`); shrinking is always permitted unless queued
    ///   bytes would not fit.
    /// - If `new_cap == capacity()`, returns `Ok(Resized { capacity, wake_writers: false })`
    ///   without allocating. When growing succeeds, `wake_writers` is `true`.
    ///
    /// # Errors
    ///
    /// - [`SetCapacityError::Invalid`] (`-EINVAL`): `bytes > (1 << 31)`.
    /// - [`SetCapacityError::OverLimit`] (`-EPERM`): growing past `max_bytes`.
    /// - [`SetCapacityError::Busy`] (`-EBUSY`): `fionread() > new_cap` (checked
    ///   by bytes before allocating; Linux compares occupied page slots so it
    ///   can refuse some shrinks this byte ring accepts).
    /// - [`SetCapacityError::NoMemory`] (`-ENOMEM`): allocation failed; the
    ///   existing buffer and queued bytes stay intact.
    pub fn set_capacity(&mut self, bytes: usize, max_bytes: usize) -> Result<Resized, SetCapacityError> {
        let old_cap = self.capacity();
        let new_cap = round_capacity(bytes).ok_or(SetCapacityError::Invalid)?;
        if new_cap > old_cap && new_cap > max_bytes {
            return Err(SetCapacityError::OverLimit);
        }
        if self.len > new_cap {
            return Err(SetCapacityError::Busy);
        }
        if new_cap == old_cap {
            return Ok(Resized { capacity: new_cap, wake_writers: false });
        }

        let mut new_buf = alloc_buf(new_cap)?;
        self.copy_out(&mut new_buf[..self.len]);
        self.buf = new_buf;
        self.head = 0;
        Ok(Resized { capacity: new_cap, wake_writers: new_cap > old_cap })
    }

    /// Attempt one non-blocking write of `data` into the pipe (Linux
    /// `fs/pipe.c` `anon_pipe_write`, `pipe(7)`).
    ///
    /// Blocking writes should use [`WriteProgress`] instead.
    ///
    /// Rules guaranteed:
    /// - An empty write (`data.is_empty()`) returns [`WriteOutcome::Wrote`]
    ///   with `0` immediately, even when full or when no readers remain.
    /// - For any non-empty write, if no readers remain, returns
    ///   [`WriteOutcome::Broken`] before checking buffer space.
    /// - Writes of at most [`PIPE_BUF`] bytes are atomic (all-or-nothing):
    ///   returns [`WriteOutcome::WouldBlock`] if fewer than `data.len()` bytes
    ///   are free.
    /// - Writes larger than [`PIPE_BUF`] write what fits (partial write) and
    ///   return [`WriteOutcome::WouldBlock`] only when zero bytes are free.
    ///
    /// Never allocates.
    pub fn write(&mut self, data: &[u8]) -> WriteOutcome {
        if data.is_empty() {
            return WriteOutcome::Wrote(0);
        }
        if self.readers == 0 {
            return WriteOutcome::Broken;
        }
        let n = self.allowed(data.len(), false);
        if n == 0 {
            return WriteOutcome::WouldBlock;
        }
        self.push_bytes(&data[..n]);
        WriteOutcome::Wrote(n)
    }

    /// Non-destructively copy up to `dst.len()` queued bytes into `dst` in FIFO
    /// order, returning a [`Peeked`] borrow guard that commits or abandons the
    /// read (Linux `fs/pipe.c` `anon_pipe_read`).
    ///
    /// - An empty `dst` yields `PeekOutcome::Data` with `len() == 0`.
    /// - If empty and no writers remain, returns [`PeekOutcome::Eof`].
    /// - If empty and writers remain, returns [`PeekOutcome::WouldBlock`].
    /// - Otherwise copies `dst.len().min(fionread())` bytes and returns
    ///   [`PeekOutcome::Data`].
    ///
    /// For chunked kernel `read(2)` into user memory: peek into a scratch
    /// buffer, `copy_to_user`, and call [`Peeked::commit`] (recording
    /// `wake_writers`) before looping while more data is queued and user space
    /// remains. If `copy_to_user` faults on a chunk, drop [`Peeked`] so those
    /// bytes stay queued, and return the bytes committed so far — or `-EFAULT`
    /// if none were committed yet (Linux `anon_pipe_read`). When returning with
    /// data still queued (`!pipe.is_empty()`), wake the next waiting reader.
    ///
    /// Never allocates.
    pub fn peek(&mut self, dst: &mut [u8]) -> PeekOutcome<'_> {
        if dst.is_empty() {
            return PeekOutcome::Data(Peeked { pipe: self, len: 0 });
        }
        if self.len == 0 {
            return if self.writers == 0 { PeekOutcome::Eof } else { PeekOutcome::WouldBlock };
        }
        let n = self.copy_out(dst);
        PeekOutcome::Data(Peeked { pipe: self, len: n })
    }

    /// Attempt one non-blocking read into `buf`, equivalent to
    /// [`Pipe::peek`] followed by [`Peeked::commit`].
    ///
    /// Kernel `read(2)` paths should use [`Pipe::peek`] + [`Peeked::commit`]
    /// directly to survive `copy_to_user` faults and observe
    /// [`Consumed::wake_writers`].
    ///
    /// Never allocates.
    pub fn read(&mut self, buf: &mut [u8]) -> ReadOutcome {
        match self.peek(buf) {
            PeekOutcome::Data(peeked) => ReadOutcome::Read(peeked.commit().n),
            PeekOutcome::Eof => ReadOutcome::Eof,
            PeekOutcome::WouldBlock => ReadOutcome::WouldBlock,
        }
    }

    /// Read-end readiness at the time of the check: `true` if data is queued
    /// or no writers remain (identical to Linux `pipe_readable()`).
    ///
    /// `select(2)` and blocking `read(2)` treat `readable()` as ready; when
    /// building a Linux `poll(2)` bitmask from `fs/pipe.c` `pipe_poll`,
    /// `EPOLLIN | EPOLLRDNORM` is set only when `!pipe.is_empty()`, while
    /// `EPOLLHUP` is given by [`Pipe::reader_hung_up`].
    pub const fn readable(&self) -> bool {
        self.len > 0 || self.writers == 0
    }

    /// Write-end room readiness at the time of the check: `true` when a write
    /// of up to [`PIPE_BUF`] bytes fits now (`capacity - fionread >= PIPE_BUF`).
    ///
    /// This is the byte-ring counterpart of Linux `fs/pipe.c` `pipe_poll`'s
    /// `!pipe_full(head, tail, pipe->max_usage)`. Because Linux counts page
    /// slots rather than bytes, Linux can report full earlier; this predicate
    /// is true whenever Linux sets `EPOLLOUT`, and sometimes when it does not.
    /// As in `pipe_poll`, it checks room alone and is independent of
    /// `readers()`; a missing reader is reported separately by
    /// [`Pipe::writer_error`] (`EPOLLERR`).
    pub const fn writable(&self) -> bool {
        self.free_space() >= PIPE_BUF
    }

    /// `POLLHUP` for the read end: `true` when no writers remain open.
    ///
    /// In Linux `fs/pipe.c` `pipe_poll`:
    /// `if (!pipe->writers && filp->f_pipe != pipe->w_counter) mask |= EPOLLHUP;`.
    /// The `w_counter` guard distinguishes FIFOs opened before any writer from
    /// pipes whose writers have all closed; for anonymous pipes a writer exists
    /// from creation, so `writers == 0` matches `EPOLLHUP` regardless of
    /// whether unread bytes are still queued.
    pub const fn reader_hung_up(&self) -> bool {
        self.writers == 0
    }

    /// `POLLERR` for the write end: `true` when no readers remain open
    /// (Linux `fs/pipe.c` `pipe_poll`: `if (!pipe->readers) mask |= EPOLLERR`),
    /// regardless of buffer occupancy.
    pub const fn writer_error(&self) -> bool {
        self.readers == 0
    }

    /// Return how many bytes of a `len`-byte write may be copied now: `0` if
    /// `free_space()` is below the progress threshold, otherwise
    /// `len.min(free_space())`.
    ///
    /// - Blocking (`WriteProgress`): threshold is `len.min(PIPE_BUF)`.
    /// - Non-blocking (`Pipe::write`): threshold is `len` for atomic writes
    ///   (`len <= PIPE_BUF`), else `1`.
    fn allowed(&self, len: usize, blocking: bool) -> usize {
        let free = self.free_space();
        let need = if blocking || len <= PIPE_BUF { len.min(PIPE_BUF) } else { 1 };
        if free < need { 0 } else { len.min(free) }
    }

    fn push_bytes(&mut self, src: &[u8]) {
        let n = src.len();
        debug_assert!(n <= self.free_space());
        let cap = self.buf.len();
        let tail = (self.head + self.len) % cap;
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        let second = n - first;
        self.buf[..second].copy_from_slice(&src[first..]);
        self.len += n;
    }

    fn copy_out(&self, dst: &mut [u8]) -> usize {
        let n = dst.len().min(self.len);
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        let second = n - first;
        dst[first..n].copy_from_slice(&self.buf[..second]);
        n
    }
}

fn release_end(count: &mut usize) -> Released {
    let Some(next) = count.checked_sub(1) else {
        return Released::Underflow;
    };
    *count = next;
    match NonZeroUsize::new(next) {
        Some(rem) => Released::Remaining(rem),
        None => Released::Last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::format;
    use std::vec;
    use std::vec::Vec;

    const MAX_LOOP_STEPS: usize = 10_000;

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

    fn ramp(start: u64, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((start + i as u64) & 0xFF) as u8).collect()
    }

    fn expect_seq(bytes: &[u8], start: u64, ctx: &str) {
        for (i, &b) in bytes.iter().enumerate() {
            assert_eq!(b, ((start + i as u64) & 0xFF) as u8, "{ctx}: byte {i}");
        }
    }

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn default_pipe_geometry_counts_fifo_and_debug() {
        let mut p = Pipe::try_new().unwrap();
        assert_eq!(p.capacity(), 16 * PAGE_SIZE);
        assert_eq!(p.fionread(), 0);
        assert!(p.is_empty());
        assert_eq!(p.free_space(), DEFAULT_CAPACITY);
        assert_eq!(p.readers(), 1);
        assert_eq!(p.writers(), 1);

        // Manual Debug must report metadata and never dump queued user data.
        assert_eq!(p.write(b"TOP_SECRET_PAYLOAD"), WriteOutcome::Wrote(18));
        let dbg = format!("{p:?}");
        assert!(dbg.contains("capacity: 65536"), "{dbg}");
        assert!(dbg.contains("len: 18"), "{dbg}");
        assert!(dbg.contains("readers: 1"), "{dbg}");
        assert!(dbg.contains("writers: 1"), "{dbg}");
        assert!(!dbg.contains("TOP_SECRET_PAYLOAD"), "Debug leaked payload: {dbg}");

        // Open/release counts, Released::Underflow, and reopening from 0.
        p.open_reader();
        p.open_writer();
        assert_eq!(p.readers(), 2);
        assert_eq!(p.writers(), 2);
        assert_eq!(p.release_reader(), Released::Remaining(nz(1)));
        assert_eq!(p.release_reader(), Released::Last);
        assert_eq!(p.release_reader(), Released::Underflow);
        assert_eq!(p.readers(), 0);
        assert_eq!(p.release_writer(), Released::Remaining(nz(1)));
        assert_eq!(p.release_writer(), Released::Last);
        assert_eq!(p.release_writer(), Released::Underflow);
        assert_eq!(p.writers(), 0);

        // Reopening from 0 (e.g. /proc/<pid>/fd) succeeds without panicking.
        p.open_reader();
        p.open_writer();
        assert_eq!(p.readers(), 1);
        assert_eq!(p.writers(), 1);
        assert_eq!(p.write(b"ok"), WriteOutcome::Wrote(2));

        // Pipe::fifo() starts at 0/0, then supports open -> release -> reopen.
        let mut f = Pipe::fifo().unwrap();
        assert_eq!(f.capacity(), DEFAULT_CAPACITY);
        assert_eq!(f.readers(), 0);
        assert_eq!(f.writers(), 0);
        assert!(f.reader_hung_up());
        assert!(f.writer_error());
        assert_eq!(f.release_reader(), Released::Underflow);
        assert_eq!(f.release_writer(), Released::Underflow);
        f.open_reader();
        f.open_writer();
        assert!(!f.reader_hung_up() && !f.writer_error());
        assert_eq!(f.write(b"fifo"), WriteOutcome::Wrote(4));
        assert_eq!(f.release_writer(), Released::Last);
        let mut out = [0u8; 8];
        assert_eq!(f.read(&mut out), ReadOutcome::Read(4));
        assert_eq!(&out[..4], b"fifo");
        assert_eq!(f.read(&mut out), ReadOutcome::Eof);
        // Reopen writer on the same FIFO after reaching 0.
        f.open_writer();
        assert!(!f.reader_hung_up());
        assert_eq!(f.read(&mut out), ReadOutcome::WouldBlock);
        assert_eq!(f.write(b"again"), WriteOutcome::Wrote(5));
        assert_eq!(f.read(&mut out), ReadOutcome::Read(5));
        assert_eq!(&out[..5], b"again");
    }

    #[test]
    fn atomic_writes_at_pipe_buf_and_pipe_buf_plus_one() {
        // Confirm exact PIPE_BUF atomicity boundary.
        assert!(WriteProgress::new(PIPE_BUF).is_atomic());
        assert!(!WriteProgress::new(PIPE_BUF + 1).is_atomic());

        // Table of (capacity, free_space, write_len, expected_nonblocking, expected_can_progress, expected_step).
        let cases = [
            (PAGE_SIZE, PIPE_BUF - 1, PIPE_BUF, WriteOutcome::WouldBlock, false, WriteStep::NeedSpace { wrote_now: 0 }),
            (
                PAGE_SIZE,
                PIPE_BUF,
                PIPE_BUF,
                WriteOutcome::Wrote(PIPE_BUF),
                true,
                WriteStep::Done { total: PIPE_BUF, wake_next_writer: false },
            ),
            (
                PAGE_SIZE,
                PIPE_BUF - 1,
                PIPE_BUF - 1,
                WriteOutcome::Wrote(PIPE_BUF - 1),
                true,
                WriteStep::Done { total: PIPE_BUF - 1, wake_next_writer: false },
            ),
            (
                PAGE_SIZE,
                100,
                100,
                WriteOutcome::Wrote(100),
                true,
                WriteStep::Done { total: 100, wake_next_writer: false },
            ),
            (
                PAGE_SIZE,
                200,
                100,
                WriteOutcome::Wrote(100),
                true,
                WriteStep::Done { total: 100, wake_next_writer: true },
            ),
            (PAGE_SIZE, 99, 100, WriteOutcome::WouldBlock, false, WriteStep::NeedSpace { wrote_now: 0 }),
            (
                PAGE_SIZE,
                PIPE_BUF - 1,
                PIPE_BUF + 1,
                WriteOutcome::Wrote(PIPE_BUF - 1),
                false,
                WriteStep::NeedSpace { wrote_now: 0 },
            ),
            (PAGE_SIZE, 0, PIPE_BUF + 1, WriteOutcome::WouldBlock, false, WriteStep::NeedSpace { wrote_now: 0 }),
            (
                DEFAULT_CAPACITY,
                PIPE_BUF - 1,
                PIPE_BUF,
                WriteOutcome::WouldBlock,
                false,
                WriteStep::NeedSpace { wrote_now: 0 },
            ),
            (
                DEFAULT_CAPACITY,
                PIPE_BUF,
                PIPE_BUF,
                WriteOutcome::Wrote(PIPE_BUF),
                true,
                WriteStep::Done { total: PIPE_BUF, wake_next_writer: false },
            ),
            (
                DEFAULT_CAPACITY,
                PIPE_BUF - 1,
                PIPE_BUF + 1,
                WriteOutcome::Wrote(PIPE_BUF - 1),
                false,
                WriteStep::NeedSpace { wrote_now: 0 },
            ),
            (
                DEFAULT_CAPACITY,
                PIPE_BUF,
                PIPE_BUF + 1,
                WriteOutcome::Wrote(PIPE_BUF),
                true,
                WriteStep::NeedSpace { wrote_now: PIPE_BUF },
            ),
            (DEFAULT_CAPACITY, 0, PIPE_BUF + 1, WriteOutcome::WouldBlock, false, WriteStep::NeedSpace { wrote_now: 0 }),
        ];

        for (cap, free, write_len, expected_nb, expected_can_progress, expected_step) in cases {
            let prefill = cap - free;
            let filler = vec![0xAA; prefill];
            let payload = ramp(1, write_len);

            // Non-blocking Pipe::write.
            let mut p = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
            if prefill > 0 {
                assert_eq!(p.write(&filler), WriteOutcome::Wrote(prefill));
            }
            assert_eq!(p.free_space(), free);
            let got = p.write(&payload);
            assert_eq!(got, expected_nb, "nb cap={cap} free={free} len={write_len}");
            if let WriteOutcome::Wrote(n) = got {
                let mut out = vec![0u8; prefill + n];
                assert_eq!(p.read(&mut out), ReadOutcome::Read(prefill + n));
                assert_eq!(&out[..prefill], &filler[..]);
                assert_eq!(&out[prefill..], &payload[..n]);
            } else {
                assert_eq!(p.fionread(), prefill);
            }

            // Blocking WriteProgress::step using the spec's `write_len <= PIPE_BUF`
            // (not `prog.is_atomic()`).
            let mut q = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
            if prefill > 0 {
                assert_eq!(q.write(&filler), WriteOutcome::Wrote(prefill));
            }
            let mut prog = WriteProgress::new(write_len);
            let chunk_len = if write_len <= PIPE_BUF { write_len } else { write_len.min(PIPE_BUF) };
            assert_eq!(prog.next_chunk(PIPE_BUF), 0..chunk_len);
            assert_eq!(
                prog.can_progress(&q),
                expected_can_progress,
                "can_progress cap={cap} free={free} len={write_len}"
            );
            assert_eq!(
                prog.step(&mut q, &payload[..chunk_len]),
                Ok(expected_step),
                "step cap={cap} free={free} len={write_len}"
            );
            if !expected_can_progress {
                assert_eq!(q.fionread(), prefill);
            }
        }
    }

    #[test]
    fn partial_nonblocking_writes() {
        let mut p = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let seed = vec![0x55; 2 * PAGE_SIZE - 1000];
        assert_eq!(p.write(&seed), WriteOutcome::Wrote(seed.len()));
        assert_eq!(p.free_space(), 1000);

        let payload = ramp(7, PIPE_BUF + 500);
        assert_eq!(p.write(&payload), WriteOutcome::Wrote(1000));
        assert_eq!(p.free_space(), 0);
        assert_eq!(p.write(&payload[1000..]), WriteOutcome::WouldBlock);
        assert_eq!(p.write(&[]), WriteOutcome::Wrote(0));

        let mut out = vec![0u8; 2 * PAGE_SIZE];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(2 * PAGE_SIZE));
        assert_eq!(&out[..seed.len()], &seed[..]);
        assert_eq!(&out[seed.len()..], &payload[..1000]);

        // Writing more than the full pipe capacity into an empty pipe writes
        // exactly `capacity()` bytes.
        let huge = ramp(13, 3 * PAGE_SIZE);
        assert_eq!(p.write(&huge), WriteOutcome::Wrote(2 * PAGE_SIZE));
        assert_eq!(p.write(&huge[2 * PAGE_SIZE..]), WriteOutcome::WouldBlock);
        let mut out2 = vec![0u8; 2 * PAGE_SIZE];
        assert_eq!(p.read(&mut out2), ReadOutcome::Read(2 * PAGE_SIZE));
        assert_eq!(&out2[..], &huge[..2 * PAGE_SIZE]);
    }

    #[test]
    fn broken_pipe_takes_precedence() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        p.open_reader();
        let full = vec![0xCC; PAGE_SIZE];
        assert_eq!(p.write(&full), WriteOutcome::Wrote(PAGE_SIZE));
        assert_eq!(p.write(b"x"), WriteOutcome::WouldBlock);

        // Closing one of two readers still leaves a live reader -> WouldBlock.
        assert_eq!(p.release_reader(), Released::Remaining(nz(1)));
        assert_eq!(p.write(b"x"), WriteOutcome::WouldBlock);

        // Closing the final reader makes Broken take precedence over WouldBlock
        // on a full pipe for small, atomic, and large writes alike.
        assert_eq!(p.release_reader(), Released::Last);
        let big = vec![0x77; PIPE_BUF + 10];
        let atomic = vec![0x88; PIPE_BUF];
        assert_eq!(p.write(b"x"), WriteOutcome::Broken);
        assert_eq!(p.write(&atomic), WriteOutcome::Broken);
        assert_eq!(p.write(&big), WriteOutcome::Broken);
        assert_eq!(p.write(&[]), WriteOutcome::Wrote(0));
        assert_eq!(p.fionread(), PAGE_SIZE);

        // WriteProgress::step on a full pipe with no readers -> Broken { written: 0 }.
        let mut full_prog = WriteProgress::new(big.len());
        assert!(full_prog.can_progress(&p));
        assert_eq!(full_prog.step(&mut p, &big[..PIPE_BUF]), Ok(WriteStep::Broken { written: 0 }));

        // Malformed chunk errors come BEFORE Broken even when readers == 0.
        let mut bad_atomic = WriteProgress::new(100);
        assert_eq!(bad_atomic.step(&mut p, &[]), Err(ChunkError::Empty));
        assert_eq!(bad_atomic.step(&mut p, &[0u8; 50]), Err(ChunkError::SplitAtomic));
        assert_eq!(bad_atomic.step(&mut p, &[0u8; 101]), Err(ChunkError::TooLong));
        let mut bad_zero = WriteProgress::new(0);
        assert_eq!(bad_zero.step(&mut p, &[0u8; 1]), Err(ChunkError::TooLong));

        // Empty pipe (plenty of room) after release_reader -> Broken before writing anything.
        let mut empty = Pipe::try_new().unwrap();
        assert_eq!(empty.release_reader(), Released::Last);
        assert_eq!(empty.write(b"hi"), WriteOutcome::Broken);
        let mut empty_large = WriteProgress::new(PIPE_BUF + 100);
        assert!(empty_large.can_progress(&empty));
        assert_eq!(empty_large.step(&mut empty, &big[..PIPE_BUF]), Ok(WriteStep::Broken { written: 0 }));
        assert_eq!(empty.fionread(), 0);

        // Atomic WriteProgress on an empty pipe after release_reader -> Broken.
        let mut empty_atomic = WriteProgress::new(100);
        assert!(empty_atomic.can_progress(&empty));
        assert_eq!(empty_atomic.step(&mut empty, &atomic[..100]), Ok(WriteStep::Broken { written: 0 }));
        assert_eq!(empty.fionread(), 0);

        // Zero-length WriteProgress with no readers -> Done before readers check.
        let mut zero_prog = WriteProgress::new(0);
        assert!(zero_prog.can_progress(&empty));
        assert_eq!(zero_prog.step(&mut empty, &[]), Ok(WriteStep::Done { total: 0, wake_next_writer: false }));

        // After partial progress, reader closes while space remains: next step
        // must return Broken { written } without writing more bytes.
        let mut mid = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let data = ramp(0, 10_000);
        let mut mid_prog = WriteProgress::new(data.len());
        assert_eq!(mid_prog.step(&mut mid, &data[..PIPE_BUF]), Ok(WriteStep::Continue { wrote_now: PIPE_BUF }));
        assert_eq!(mid.free_space(), PIPE_BUF);
        assert_eq!(mid.release_reader(), Released::Last);
        assert!(mid_prog.can_progress(&mid));
        assert_eq!(mid_prog.step(&mut mid, &data[PIPE_BUF..2 * PIPE_BUF]), Ok(WriteStep::Broken { written: PIPE_BUF }));
        assert_eq!(mid.fionread(), PIPE_BUF);
    }

    #[test]
    fn eof_only_when_no_writers_and_buffer_empty() {
        let mut p = Pipe::try_new().unwrap();
        p.open_writer();
        assert_eq!(p.write(b"hello world"), WriteOutcome::Wrote(11));

        // Closing one of two writers: data still readable, empty gives WouldBlock.
        assert_eq!(p.release_writer(), Released::Remaining(nz(1)));
        let mut buf = [0xFFu8; 20];
        assert_eq!(p.read(&mut buf[..5]), ReadOutcome::Read(5));
        assert_eq!(&buf[..5], b"hello");

        // Closing the last writer while data remains: must drain all data first.
        assert_eq!(p.release_writer(), Released::Last);
        let mut buf2 = [0xEEu8; 20];
        assert_eq!(p.read(&mut buf2), ReadOutcome::Read(6));
        assert_eq!(&buf2[..6], b" world");
        assert!(buf2[6..].iter().all(|&b| b == 0xEE));

        // Now empty with zero writers: Eof.
        assert_eq!(p.read(&mut buf2), ReadOutcome::Eof);

        // Zero-length read returns Read(0) in every state.
        assert_eq!(p.read(&mut []), ReadOutcome::Read(0));

        // Empty with a writer open: WouldBlock (not Eof).
        let mut q = Pipe::try_new().unwrap();
        assert_eq!(q.read(&mut buf2), ReadOutcome::WouldBlock);
        assert_eq!(q.read(&mut []), ReadOutcome::Read(0));
    }

    #[test]
    fn peeked_guard_preserves_bytes_on_drop_and_reports_wake_signals() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        // Position head at PAGE_SIZE - 5 without emptying so "ABCDEFGHIJ" wraps across PAGE_SIZE.
        assert_eq!(p.write(&vec![0x22; PAGE_SIZE - 5]), WriteOutcome::Wrote(PAGE_SIZE - 5));
        assert_eq!(p.write(b"ABCDE"), WriteOutcome::Wrote(5));
        assert_eq!(p.read(&mut vec![0u8; PAGE_SIZE - 5]), ReadOutcome::Read(PAGE_SIZE - 5));
        assert_eq!(p.write(b"FGHIJ"), WriteOutcome::Wrote(5));
        assert_eq!(p.fionread(), 10);

        // Simulated copy_to_user fault: dropping Peeked leaves every byte queued.
        let mut dst1 = [0xFFu8; 16];
        {
            let PeekOutcome::Data(peeked) = p.peek(&mut dst1) else {
                panic!("expected Data");
            };
            assert_eq!(peeked.len(), 10);
            assert!(!peeked.is_empty());
            assert_eq!(&dst1[..10], b"ABCDEFGHIJ");
            assert!(dst1[10..].iter().all(|&b| b == 0xFF));
            // Drop without committing.
        }
        assert_eq!(p.fionread(), 10);

        // commit_prefix(0) frees nothing -> wake_writers is false.
        let mut dst2 = [0xEEu8; 16];
        let PeekOutcome::Data(peeked) = p.peek(&mut dst2[..7]) else {
            panic!("expected Data");
        };
        assert_eq!(peeked.len(), 7);
        assert_eq!(&dst2[..7], b"ABCDEFG");
        assert_eq!(peeked.commit_prefix(0), Consumed { n: 0, wake_writers: false });
        assert_eq!(p.fionread(), 10);

        // commit_prefix(7) on a 1-page pipe (was not writable!) wakes writers.
        let PeekOutcome::Data(peeked) = p.peek(&mut dst2[..7]) else {
            panic!("expected Data");
        };
        assert_eq!(peeked.commit_prefix(7), Consumed { n: 7, wake_writers: true });
        assert_eq!(p.fionread(), 3);

        // commit_prefix(k > len) clamps cleanly without panicking.
        let mut tail = [0u8; 8];
        let PeekOutcome::Data(peeked) = p.peek(&mut tail) else {
            panic!("expected Data");
        };
        assert_eq!(peeked.len(), 3);
        assert_eq!(&tail[..3], b"HIJ");
        assert_eq!(peeked.commit_prefix(99), Consumed { n: 3, wake_writers: true });
        assert!(p.is_empty());

        // Empty pipe with open writer: WouldBlock for non-empty dst, Data(0) for empty dst.
        assert!(matches!(p.peek(&mut tail), PeekOutcome::WouldBlock));
        let PeekOutcome::Data(zero) = p.peek(&mut []) else {
            panic!("expected Data(0) for empty dst");
        };
        assert!(zero.is_empty());
        assert_eq!(zero.commit(), Consumed { n: 0, wake_writers: false });

        // Empty pipe with no writers: Eof for non-empty dst, Data(0) for empty dst.
        assert_eq!(p.release_writer(), Released::Last);
        assert!(matches!(p.peek(&mut tail), PeekOutcome::Eof));
        let PeekOutcome::Data(zero_eof) = p.peek(&mut []) else {
            panic!("expected Data(0) for empty dst at EOF");
        };
        assert_eq!(zero_eof.commit(), Consumed { n: 0, wake_writers: false });
    }

    #[test]
    fn sub_pipe_buf_waiter_is_woken_by_small_read_and_wake_signals() {
        // Reproduces the round-2 deadlock scenario on a 64 KiB pipe:
        // 1) Pipe holds 65,300 bytes (free = 236 < PIPE_BUF, so !writable()).
        // 2) Child blocks writing 300 bytes (can_progress is false).
        // 3) Parent reads 500 bytes -> free = 736 (still !writable()!), yet
        //    Consumed::wake_writers MUST be true so the 300-byte writer wakes.
        // 4) Child writes 300 bytes -> Done { total: 300, wake_next_writer: true }
        //    because 436 bytes remain free for another small writer.
        let mut p = Pipe::try_new().unwrap();
        let initial = vec![0x42; 65_300];
        assert_eq!(p.write(&initial), WriteOutcome::Wrote(65_300));
        assert!(!p.writable());

        let child_data = ramp(1, 300);
        let mut child = WriteProgress::new(300);
        assert!(!child.can_progress(&p));
        assert_eq!(child.step(&mut p, &child_data), Ok(WriteStep::NeedSpace { wrote_now: 0 }));

        let mut parent_buf = [0u8; 500];
        let PeekOutcome::Data(peeked) = p.peek(&mut parent_buf) else {
            panic!("expected Data");
        };
        let consumed = peeked.commit();
        assert_eq!(consumed, Consumed { n: 500, wake_writers: true });
        assert!(!p.writable(), "pipe must still be below PIPE_BUF free");
        assert!(!p.is_empty(), "data still queued");

        assert!(child.can_progress(&p));
        assert_eq!(child.step(&mut p, &child_data), Ok(WriteStep::Done { total: 300, wake_next_writer: true }));

        // Reading when the pipe was ALREADY writable() does NOT need to wake writers.
        let mut q = Pipe::try_new().unwrap();
        assert_eq!(q.write(b"small"), WriteOutcome::Wrote(5));
        assert!(q.writable());
        let mut out = [0u8; 5];
        let PeekOutcome::Data(peeked) = q.peek(&mut out) else {
            panic!("expected Data");
        };
        assert_eq!(peeked.commit(), Consumed { n: 5, wake_writers: false });
    }

    #[test]
    fn readiness_predicates() {
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
                            assert_eq!(p.write(&data), WriteOutcome::Wrote(occ));
                        }
                        if readers == 0 {
                            assert_eq!(p.release_reader(), Released::Last);
                        }
                        if writers == 0 {
                            assert_eq!(p.release_writer(), Released::Last);
                        }

                        assert_eq!(
                            p.readable(),
                            occ > 0 || writers == 0,
                            "readable cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.writable(),
                            cap - occ >= PIPE_BUF,
                            "writable cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.reader_hung_up(),
                            writers == 0,
                            "reader_hung_up cap={cap} occ={occ} r={readers} w={writers}"
                        );
                        assert_eq!(
                            p.writer_error(),
                            readers == 0,
                            "writer_error cap={cap} occ={occ} r={readers} w={writers}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn set_capacity_rounding_bounds_and_ebusy() {
        // Direct round_capacity checks, including Linux's 1 << 31 ceiling.
        assert_eq!(round_capacity(0), Some(PAGE_SIZE));
        assert_eq!(round_capacity(1), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE - 1), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE), Some(PAGE_SIZE));
        assert_eq!(round_capacity(PAGE_SIZE + 1), Some(2 * PAGE_SIZE));
        assert_eq!(round_capacity(2 * PAGE_SIZE), Some(2 * PAGE_SIZE));
        assert_eq!(round_capacity(3 * PAGE_SIZE), Some(4 * PAGE_SIZE));
        assert_eq!(round_capacity(DEFAULT_CAPACITY), Some(DEFAULT_CAPACITY));
        assert_eq!(round_capacity(DEFAULT_MAX_CAPACITY), Some(DEFAULT_MAX_CAPACITY));
        assert_eq!(round_capacity(1usize << 31), Some(1usize << 31));
        assert_eq!(round_capacity((1usize << 31) + 1), None);
        assert_eq!(round_capacity(usize::MAX), None);

        // with_capacity rounding and CreateError checks.
        assert_eq!(Pipe::with_capacity(0, DEFAULT_MAX_CAPACITY).unwrap().capacity(), PAGE_SIZE);
        assert_eq!(Pipe::with_capacity(5000, DEFAULT_MAX_CAPACITY).unwrap().capacity(), 2 * PAGE_SIZE);
        assert_eq!(Pipe::with_capacity(PAGE_SIZE, PAGE_SIZE - 1).unwrap_err(), CreateError::OverLimit);
        assert_eq!(Pipe::with_capacity(8192, 4096).unwrap_err(), CreateError::OverLimit);
        assert_eq!(Pipe::with_capacity((1usize << 31) + 1, usize::MAX).unwrap_err(), CreateError::Invalid);

        let mut p = Pipe::try_new().unwrap();
        // Minimum: 0 shrinks from 16 pages to 1 page (wake_writers = false); 1 is a no-op.
        assert_eq!(p.set_capacity(0, DEFAULT_MAX_CAPACITY), Ok(Resized { capacity: PAGE_SIZE, wake_writers: false }));
        assert_eq!(p.capacity(), PAGE_SIZE);
        assert_eq!(p.set_capacity(1, DEFAULT_MAX_CAPACITY), Ok(Resized { capacity: PAGE_SIZE, wake_writers: false }));

        // Limit checked against the *rounded* size (3*PAGE rounds to 4*PAGE > 3*PAGE).
        assert_eq!(p.set_capacity(3 * PAGE_SIZE, 3 * PAGE_SIZE), Err(SetCapacityError::OverLimit));
        assert_eq!(p.capacity(), PAGE_SIZE);

        // Growing wakes writers.
        assert_eq!(
            p.set_capacity(PAGE_SIZE + 1, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: 2 * PAGE_SIZE, wake_writers: true })
        );
        assert_eq!(
            p.set_capacity(3 * PAGE_SIZE - 10, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: 4 * PAGE_SIZE, wake_writers: true })
        );

        // Maximum and Invalid checks.
        assert_eq!(
            p.set_capacity(DEFAULT_MAX_CAPACITY, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: DEFAULT_MAX_CAPACITY, wake_writers: true })
        );
        assert_eq!(p.set_capacity(DEFAULT_MAX_CAPACITY + 1, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::OverLimit));
        assert_eq!(p.set_capacity((1usize << 31) + 1, usize::MAX), Err(SetCapacityError::Invalid));
        assert_eq!(p.set_capacity(usize::MAX, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Invalid));
        assert_eq!(p.capacity(), DEFAULT_MAX_CAPACITY);

        // Shrinking and same-size no-ops are permitted even above max_bytes;
        // growing above max_bytes is refused.
        assert_eq!(
            p.set_capacity(4 * PAGE_SIZE, 2 * PAGE_SIZE),
            Ok(Resized { capacity: 4 * PAGE_SIZE, wake_writers: false })
        );
        assert_eq!(
            p.set_capacity(4 * PAGE_SIZE, 2 * PAGE_SIZE),
            Ok(Resized { capacity: 4 * PAGE_SIZE, wake_writers: false })
        );
        assert_eq!(p.set_capacity(8 * PAGE_SIZE, 2 * PAGE_SIZE), Err(SetCapacityError::OverLimit));
        assert_eq!(p.capacity(), 4 * PAGE_SIZE);

        // Wrapped grow, -EBUSY, and wrapped shrink.
        assert_eq!(
            p.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: 2 * PAGE_SIZE, wake_writers: false })
        );
        let dummy = vec![0x99; 6000];
        assert_eq!(p.write(&dummy), WriteOutcome::Wrote(6000));
        assert_eq!(p.read(&mut vec![0u8; 5500]), ReadOutcome::Read(5500));
        // 500 bytes at [5500..6000]; write 4500 bytes that wrap past 8192.
        let wrapped = ramp(0, 4500);
        assert_eq!(p.write(&wrapped), WriteOutcome::Wrote(4500));
        assert_eq!(p.fionread(), 5000);

        // 4096 < 5000 -> Busy, capacity and contents unchanged.
        assert_eq!(p.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Busy));
        assert_eq!(p.capacity(), 2 * PAGE_SIZE);
        assert_eq!(p.fionread(), 5000);

        // Drain 1000 bytes so 4000 bytes remain, still wrapped across offset 8192
        // (head at 6500, tail at 2308), then shrink directly to 1 page.
        let mut prefix = vec![0u8; 1000];
        assert_eq!(p.read(&mut prefix), ReadOutcome::Read(1000));
        assert_eq!(&prefix[..500], &vec![0x99; 500][..]);
        assert_eq!(&prefix[500..], &wrapped[..500]);
        assert_eq!(p.fionread(), 4000);

        assert_eq!(
            p.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: PAGE_SIZE, wake_writers: false })
        );
        assert_eq!(p.capacity(), PAGE_SIZE);
        let mut after_shrink = vec![0u8; 4000];
        assert_eq!(p.read(&mut after_shrink), ReadOutcome::Read(4000));
        assert_eq!(&after_shrink[..], &wrapped[500..]);
        assert!(p.is_empty());

        // Wrapped grow from 1 page to 4 pages preserves wrapped bytes and sets wake_writers.
        assert_eq!(p.write(&vec![0x77; 3500]), WriteOutcome::Wrote(3500));
        assert_eq!(p.read(&mut vec![0u8; 3000]), ReadOutcome::Read(3000));
        let wrapped_grow = ramp(3, 3000);
        assert_eq!(p.write(&wrapped_grow), WriteOutcome::Wrote(3000));
        assert_eq!(
            p.set_capacity(3 * PAGE_SIZE, DEFAULT_MAX_CAPACITY),
            Ok(Resized { capacity: 4 * PAGE_SIZE, wake_writers: true })
        );
        assert_eq!(p.capacity(), 4 * PAGE_SIZE);
        let mut grown_out = vec![0u8; 3500];
        assert_eq!(p.read(&mut grown_out), ReadOutcome::Read(3500));
        assert_eq!(&grown_out[..500], &vec![0x77; 500][..]);
        assert_eq!(&grown_out[500..], &wrapped_grow[..]);
    }

    #[test]
    fn fionread_tracks_queued_bytes() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(p.fionread(), 0);

        assert_eq!(p.write(b"abcdef"), WriteOutcome::Wrote(6));
        assert_eq!(p.fionread(), 6);

        let fill = vec![1u8; PAGE_SIZE - 10];
        assert_eq!(p.write(&fill), WriteOutcome::Wrote(PAGE_SIZE - 10));
        assert_eq!(p.fionread(), PAGE_SIZE - 4);

        // Failed WouldBlock write does not change FIONREAD.
        assert_eq!(p.write(&[2u8; 5]), WriteOutcome::WouldBlock);
        assert_eq!(p.fionread(), PAGE_SIZE - 4);

        // Partial read reduces FIONREAD accurately.
        let mut out = [0u8; 100];
        assert_eq!(p.read(&mut out), ReadOutcome::Read(100));
        assert_eq!(p.fionread(), PAGE_SIZE - 104);

        // Closing writer leaves queued bytes visible to FIONREAD until drained.
        assert_eq!(p.release_writer(), Released::Last);
        assert_eq!(p.fionread(), PAGE_SIZE - 104);

        let mut rest = vec![0u8; PAGE_SIZE];
        assert_eq!(p.read(&mut rest), ReadOutcome::Read(PAGE_SIZE - 104));
        assert_eq!(p.fionread(), 0);
    }

    #[test]
    fn ring_wrap_around_preserves_order() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let mut next_in: u64 = 0;
        let mut next_out: u64 = 0;

        // Prime-sized interleaved writes and reads wrapping the 4096-byte ring
        // roughly eight times without ever emptying it.
        assert_eq!(p.write(&[0u8; 1]), WriteOutcome::Wrote(1));
        next_in += 1;

        let write_sizes = [307, 509, 1013, 199, 701, 419];
        let read_sizes = [173, 401, 613, 251, 521, 353];
        for round in 0..80 {
            let wlen = write_sizes[round % write_sizes.len()];
            if p.free_space() >= wlen {
                let data = ramp(next_in, wlen);
                assert_eq!(p.write(&data), WriteOutcome::Wrote(wlen));
                next_in += wlen as u64;
            }
            let rlen = read_sizes[round % read_sizes.len()].min(p.fionread().saturating_sub(1));
            if rlen > 0 {
                let mut buf = vec![0u8; rlen];
                assert_eq!(p.read(&mut buf), ReadOutcome::Read(rlen));
                expect_seq(&buf, next_out, "prime wrap");
                next_out += rlen as u64;
            }
            assert_eq!(p.fionread(), (next_in - next_out) as usize);
        }

        let mut drain = vec![0u8; p.fionread()];
        assert_eq!(p.read(&mut drain), ReadOutcome::Read(drain.len()));
        expect_seq(&drain, next_out, "drain");
        assert!(p.is_empty());

        // Exact boundary wraps with head at cap - 2, cap - 1, and wrapping to cap (0).
        for head_pos in [PAGE_SIZE - 2, PAGE_SIZE - 1] {
            let mut q = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
            let keep = PAGE_SIZE - head_pos;
            assert_eq!(q.write(&vec![0xA5; PAGE_SIZE]), WriteOutcome::Wrote(PAGE_SIZE));
            assert_eq!(q.read(&mut vec![0u8; head_pos]), ReadOutcome::Read(head_pos));
            // `keep` bytes now sit at [head_pos..PAGE_SIZE]; fill the rest at [0..head_pos].
            let span = ramp(1, head_pos);
            assert_eq!(q.write(&span), WriteOutcome::Wrote(head_pos));
            assert_eq!(q.fionread(), PAGE_SIZE);
            let mut full_out = vec![0u8; PAGE_SIZE];
            assert_eq!(q.read(&mut full_out), ReadOutcome::Read(PAGE_SIZE));
            assert_eq!(&full_out[..keep], &vec![0xA5; keep][..]);
            assert_eq!(&full_out[keep..], &span[..]);
        }

        // Head reaching exact cap wraps cleanly to 0 while data remains queued.
        let mut r = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(r.write(&vec![0xB1; PAGE_SIZE]), WriteOutcome::Wrote(PAGE_SIZE));
        assert_eq!(r.read(&mut [0u8; 100]), ReadOutcome::Read(100));
        let wrapped = ramp(5, 100);
        assert_eq!(r.write(&wrapped), WriteOutcome::Wrote(100));
        // Reading PAGE_SIZE - 100 advances head from 100 to exact PAGE_SIZE -> wraps to 0.
        let mut body = vec![0u8; PAGE_SIZE - 100];
        assert_eq!(r.read(&mut body), ReadOutcome::Read(PAGE_SIZE - 100));
        assert!(body.iter().all(|&b| b == 0xB1));
        let mut tail = [0u8; 100];
        assert_eq!(r.read(&mut tail), ReadOutcome::Read(100));
        assert_eq!(tail[..], wrapped[..]);
        assert!(r.is_empty());
    }

    #[test]
    fn write_progress_chunked_interleaving_threshold_and_interrupts() {
        // 1) Large chunked blocking write driven purely by next_chunk() (no shadow offset)
        // on a 2-page pipe interleaved with reads; bounded loops prevent runaway.
        let mut p = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let total: usize = 20_000;
        let payload = ramp(17, total);
        let mut progress = WriteProgress::new(total);

        let mut received = Vec::with_capacity(total);
        let read_chunks = [1000, 3500, 4096, 2000, 4096, 1500, 3000];
        let mut read_idx = 0usize;
        let mut finished = false;
        let mut saw_continue = false;
        let mut saw_need_space = false;

        for _ in 0..MAX_LOOP_STEPS {
            let range = progress.next_chunk(PIPE_BUF);
            match progress.step(&mut p, &payload[range]).unwrap() {
                WriteStep::Done { total: n, .. } => {
                    assert_eq!(n, total);
                    assert_eq!(progress.next_chunk(PIPE_BUF), total..total);
                    finished = true;
                    break;
                }
                WriteStep::Continue { wrote_now } => {
                    assert!(wrote_now > 0);
                    saw_continue = true;
                }
                WriteStep::NeedSpace { .. } => {
                    saw_need_space = true;
                    assert!(!progress.can_progress(&p));
                    // Stepping again before space is freed makes zero progress.
                    let retry_range = progress.next_chunk(PIPE_BUF);
                    assert_eq!(progress.step(&mut p, &payload[retry_range]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
                    // Read until the writer can progress again (bounded).
                    let mut read_ok = false;
                    for _ in 0..MAX_LOOP_STEPS {
                        let want = read_chunks[read_idx % read_chunks.len()];
                        read_idx += 1;
                        let mut buf = vec![0u8; want];
                        match p.read(&mut buf) {
                            ReadOutcome::Read(r) => {
                                received.extend_from_slice(&buf[..r]);
                                assert!(received.len() <= total);
                            }
                            other => panic!("unexpected read outcome: {other:?}"),
                        }
                        if progress.can_progress(&p) {
                            read_ok = true;
                            break;
                        }
                    }
                    assert!(read_ok, "reader never freed enough room");
                }
                WriteStep::Broken { .. } => panic!("unexpected broken pipe"),
            }
        }
        assert!(finished, "blocking write did not finish within {MAX_LOOP_STEPS} steps");
        assert!(saw_continue && saw_need_space);

        let mut tail = vec![0u8; PAGE_SIZE];
        for _ in 0..MAX_LOOP_STEPS {
            if p.is_empty() {
                break;
            }
            match p.read(&mut tail) {
                ReadOutcome::Read(r) => {
                    received.extend_from_slice(&tail[..r]);
                    assert!(received.len() <= total);
                }
                other => panic!("unexpected tail read: {other:?}"),
            }
        }
        assert!(p.is_empty(), "pipe did not drain within {MAX_LOOP_STEPS} steps");
        assert_eq!(received, payload);

        // 2) Non-atomic chunk larger than free space: verifies allowed.min(next.len())
        // clamps cleanly without overwriting queued bytes.
        let mut clamp_pipe = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let prefill_len = 2 * PAGE_SIZE - (PIPE_BUF + 100);
        let prefill = vec![0x5A; prefill_len];
        assert_eq!(clamp_pipe.write(&prefill), WriteOutcome::Wrote(prefill_len));
        assert_eq!(clamp_pipe.free_space(), PIPE_BUF + 100);
        let big_chunk = ramp(3, 2 * PIPE_BUF);
        let mut clamp_prog = WriteProgress::new(3 * PIPE_BUF);
        assert_eq!(
            clamp_prog.step(&mut clamp_pipe, &big_chunk),
            Ok(WriteStep::NeedSpace { wrote_now: PIPE_BUF + 100 })
        );
        assert_eq!(clamp_prog.next_chunk(2 * PIPE_BUF), (PIPE_BUF + 100)..(3 * PIPE_BUF));
        let mut full_read = vec![0u8; 2 * PAGE_SIZE];
        assert_eq!(clamp_pipe.read(&mut full_read), ReadOutcome::Read(2 * PAGE_SIZE));
        assert_eq!(&full_read[..prefill_len], &prefill[..]);
        assert_eq!(&full_read[prefill_len..], &big_chunk[..PIPE_BUF + 100]);

        // 3) Non-atomic threshold vs small atomic writer, plus interrupted() and faulted().
        let mut q = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(q.write(&vec![0x10; PAGE_SIZE]), WriteOutcome::Wrote(PAGE_SIZE));
        let data5k = ramp(0, 5000);
        let mut large = WriteProgress::new(data5k.len());
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
        assert_eq!(large.interrupted(), InterruptedWrite::Restart);
        assert_eq!(large.faulted(), FaultedWrite::Fault);

        // Free 300 bytes (< PIPE_BUF): large writer still cannot progress, while
        // a 300-byte atomic writer CAN progress (needs only its own length).
        assert_eq!(q.read(&mut [0u8; 300]), ReadOutcome::Read(300));
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));

        let small_data = ramp(9, 300);
        let mut small_atomic = WriteProgress::new(300);
        // Atomic next_chunk always returns the full remainder even when max < total.
        assert_eq!(small_atomic.next_chunk(64), 0..300);
        assert!(small_atomic.can_progress(&q));
        assert_eq!(small_atomic.step(&mut q, &small_data), Ok(WriteStep::Done { total: 300, wake_next_writer: false }));

        // Drain full page: large writer writes 4096 bytes, leaving 904 remaining.
        assert_eq!(q.read(&mut vec![0u8; PAGE_SIZE]), ReadOutcome::Read(PAGE_SIZE));
        assert!(large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: PIPE_BUF }));
        assert_eq!(large.interrupted(), InterruptedWrite::Partial(PIPE_BUF));
        assert_eq!(large.faulted(), FaultedWrite::Partial(PIPE_BUF));
        assert_eq!(large.next_chunk(PIPE_BUF), PIPE_BUF..5000);

        // Tail has 904 bytes remaining (< PIPE_BUF): needs 904 free (not 4096).
        assert_eq!(q.read(&mut [0u8; 903]), ReadOutcome::Read(903));
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[PIPE_BUF..]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
        assert_eq!(q.read(&mut [0u8; 1]), ReadOutcome::Read(1));
        assert!(large.can_progress(&q));
        assert_eq!(
            large.step(&mut q, &data5k[PIPE_BUF..]),
            Ok(WriteStep::Done { total: 5000, wake_next_writer: false })
        );
        assert_eq!(large.step(&mut q, &[]), Ok(WriteStep::Done { total: 5000, wake_next_writer: false }));

        // 4) ChunkError cases including exact PIPE_BUF boundary (no panics on bad chunks).
        let mut err_pipe = Pipe::try_new().unwrap();
        let mut p_pipe_buf = WriteProgress::new(PIPE_BUF);
        assert_eq!(p_pipe_buf.next_chunk(100), 0..PIPE_BUF);
        assert_eq!(p_pipe_buf.step(&mut err_pipe, &vec![0u8; PIPE_BUF - 1]), Err(ChunkError::SplitAtomic));
        assert_eq!(p_pipe_buf.step(&mut err_pipe, &vec![0u8; PIPE_BUF + 1]), Err(ChunkError::TooLong));
        assert_eq!(p_pipe_buf.step(&mut err_pipe, &[]), Err(ChunkError::Empty));
        assert_eq!(
            p_pipe_buf.step(&mut err_pipe, &vec![0u8; PIPE_BUF]),
            Ok(WriteStep::Done { total: PIPE_BUF, wake_next_writer: true })
        );
        assert_eq!(p_pipe_buf.step(&mut err_pipe, &[0u8; 1]), Err(ChunkError::TooLong));

        let mut p_over = WriteProgress::new(PIPE_BUF + 1);
        assert_eq!(p_over.next_chunk(100), 0..100);
        assert_eq!(p_over.step(&mut err_pipe, &[]), Err(ChunkError::Empty));
        assert_eq!(p_over.step(&mut err_pipe, &vec![0u8; PIPE_BUF + 2]), Err(ChunkError::TooLong));

        let mut p_zero = WriteProgress::new(0);
        assert_eq!(p_zero.next_chunk(100), 0..0);
        assert_eq!(p_zero.step(&mut err_pipe, &[0u8; 1]), Err(ChunkError::TooLong));
        assert_eq!(p_zero.step(&mut err_pipe, &[]), Ok(WriteStep::Done { total: 0, wake_next_writer: false }));
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
            let mut byte_seq: u64 = 0;

            let mut inflight: Option<(WriteProgress, Vec<u8>, usize)> = None;

            for step in 0..3_000 {
                let op = rng.gen_range(100);
                if op < 30 {
                    // Non-blocking Pipe::write checked by independent laws.
                    let len = match rng.gen_range(8) {
                        0 => 0,
                        1 => 1 + rng.gen_range(64),
                        2 => PIPE_BUF - 1,
                        3 => PIPE_BUF,
                        4 => PIPE_BUF + 1,
                        5 => 1 + rng.gen_range(PIPE_BUF),
                        6 => PIPE_BUF + 1 + rng.gen_range(2 * PAGE_SIZE),
                        _ => model_cap - model.len(),
                    };
                    let data = ramp(byte_seq, len);
                    byte_seq = byte_seq.wrapping_add(len as u64);
                    let free_before = model_cap - model.len();
                    let len_before = model.len();
                    let got = pipe.write(&data);

                    match got {
                        WriteOutcome::Wrote(n) => {
                            if data.is_empty() {
                                assert_eq!(n, 0, "step {step}");
                            } else {
                                assert!(model_readers > 0, "step {step}: wrote with no readers");
                                assert!(n > 0 && n <= data.len(), "step {step}: bad wrote count {n}");
                                if data.len() <= PIPE_BUF {
                                    assert_eq!(n, data.len(), "step {step}: split atomic write");
                                    assert!(free_before >= data.len(), "step {step}");
                                } else {
                                    assert_eq!(n, data.len().min(free_before), "step {step}");
                                }
                                model.extend(&data[..n]);
                            }
                            assert_eq!(pipe.fionread(), len_before + n, "step {step}");
                        }
                        WriteOutcome::WouldBlock => {
                            assert!(!data.is_empty() && model_readers > 0, "step {step}");
                            if data.len() <= PIPE_BUF {
                                assert!(free_before < data.len(), "step {step}");
                            } else {
                                assert_eq!(free_before, 0, "step {step}");
                            }
                            assert_eq!(pipe.fionread(), len_before, "step {step}");
                        }
                        WriteOutcome::Broken => {
                            assert!(!data.is_empty() && model_readers == 0, "step {step}");
                            assert_eq!(pipe.fionread(), len_before, "step {step}");
                        }
                    }
                } else if op < 50 {
                    // Interleaved chunked WriteProgress step using the spec's `total <= PIPE_BUF`.
                    if inflight.is_none() {
                        let total = match rng.gen_range(5) {
                            0 => 0,
                            1 => 1 + rng.gen_range(200),
                            2 => PIPE_BUF,
                            3 => PIPE_BUF + 1,
                            _ => PIPE_BUF + 1 + rng.gen_range(2 * PAGE_SIZE),
                        };
                        let data = ramp(byte_seq, total);
                        byte_seq = byte_seq.wrapping_add(total as u64);
                        inflight = Some((WriteProgress::new(total), data, 0));
                    }
                    let (mut prog, data, mut off) = inflight.take().unwrap();
                    let total = data.len();
                    let rem = total - off;
                    let is_atomic_spec = total <= PIPE_BUF;
                    let max_chunk = 1 + rng.gen_range(PIPE_BUF + 256);
                    let range = prog.next_chunk(max_chunk);
                    let expected_len = if is_atomic_spec { rem } else { rem.min(max_chunk) };
                    assert_eq!(range, off..(off + expected_len), "step {step}: next_chunk");

                    let free_before = model_cap - model.len();
                    let can_before = prog.can_progress(&pipe);
                    let res = prog.step(&mut pipe, &data[range.clone()]).unwrap();

                    match res {
                        WriteStep::Done { total: n, wake_next_writer } => {
                            assert!(can_before, "step {step}");
                            assert_eq!(n, total, "step {step}");
                            if rem > 0 {
                                assert!(model_readers > 0, "step {step}");
                                assert_eq!(range.len(), rem, "step {step}");
                                assert!(free_before >= rem, "step {step}");
                                model.extend(&data[off..]);
                                assert_eq!(wake_next_writer, model_cap - model.len() > 0, "step {step}");
                            } else {
                                assert!(!wake_next_writer, "step {step}");
                            }
                        }
                        WriteStep::Continue { wrote_now } => {
                            assert!(can_before && model_readers > 0, "step {step}");
                            assert!(wrote_now > 0 && wrote_now <= range.len(), "step {step}");
                            assert!(free_before >= rem.min(PIPE_BUF), "step {step}");
                            model.extend(&data[off..off + wrote_now]);
                            off += wrote_now;
                            assert!(prog.can_progress(&pipe), "step {step}: Continue without progress");
                            inflight = Some((prog, data, off));
                        }
                        WriteStep::NeedSpace { wrote_now } => {
                            assert!(model_readers > 0, "step {step}");
                            assert_eq!(can_before, wrote_now > 0, "step {step}");
                            if wrote_now == 0 {
                                assert!(free_before < rem.min(PIPE_BUF), "step {step}");
                            } else {
                                assert!(!is_atomic_spec, "step {step}: split atomic WriteProgress");
                                assert!(free_before >= rem.min(PIPE_BUF), "step {step}");
                                model.extend(&data[off..off + wrote_now]);
                                off += wrote_now;
                            }
                            assert!(!prog.can_progress(&pipe), "step {step}: NeedSpace while can_progress");
                            inflight = Some((prog, data, off));
                        }
                        WriteStep::Broken { written } => {
                            assert!(can_before && model_readers == 0, "step {step}");
                            assert_eq!(written, off, "step {step}");
                        }
                    }
                } else if op < 82 {
                    // Read or Peeked borrow-guard path.
                    let len = match rng.gen_range(5) {
                        0 => 0,
                        1 => 1 + rng.gen_range(64),
                        2 => 1 + rng.gen_range(PIPE_BUF),
                        3 => PIPE_BUF + rng.gen_range(PAGE_SIZE),
                        _ => model.len().max(1),
                    };
                    let mut buf = vec![0xEE; len];
                    if rng.gen_range(3) == 0 {
                        let was_writable = model_cap - model.len() >= PIPE_BUF;
                        let queued_before = model.len();
                        match pipe.peek(&mut buf) {
                            PeekOutcome::Data(peeked) => {
                                let n = peeked.len();
                                assert_eq!(n, len.min(queued_before), "step {step}: peek len");
                                let expected_bytes: Vec<u8> = model.iter().take(n).copied().collect();
                                assert_eq!(&buf[..n], &expected_bytes[..], "step {step}: peek data");
                                assert!(buf[n..].iter().all(|&b| b == 0xEE), "step {step}: peek overwrote");
                                match rng.gen_range(3) {
                                    0 => {
                                        // Drop without committing (-EFAULT path).
                                        drop(peeked);
                                    }
                                    1 => {
                                        let c = peeked.commit();
                                        assert_eq!(
                                            c,
                                            Consumed { n, wake_writers: n > 0 && !was_writable },
                                            "step {step}: commit"
                                        );
                                        model.drain(..n);
                                    }
                                    _ => {
                                        let k = if n == 0 { 0 } else { rng.gen_range(n + 3) };
                                        let expected_n = k.min(n);
                                        let c = peeked.commit_prefix(k);
                                        assert_eq!(
                                            c,
                                            Consumed { n: expected_n, wake_writers: expected_n > 0 && !was_writable },
                                            "step {step}: commit_prefix"
                                        );
                                        model.drain(..expected_n);
                                    }
                                }
                            }
                            PeekOutcome::Eof => {
                                assert!(len > 0 && queued_before == 0 && model_writers == 0, "step {step}");
                            }
                            PeekOutcome::WouldBlock => {
                                assert!(len > 0 && queued_before == 0 && model_writers > 0, "step {step}");
                            }
                        }
                    } else {
                        let queued_before = model.len();
                        let got = pipe.read(&mut buf);
                        match got {
                            ReadOutcome::Read(n) => {
                                assert_eq!(n, len.min(queued_before), "step {step}: read len");
                                let expected_bytes: Vec<u8> = model.drain(..n).collect();
                                assert_eq!(&buf[..n], &expected_bytes[..], "step {step}: read data");
                                assert!(buf[n..].iter().all(|&b| b == 0xEE), "step {step}: read overwrote");
                            }
                            ReadOutcome::Eof => {
                                assert!(len > 0 && queued_before == 0 && model_writers == 0, "step {step}");
                            }
                            ReadOutcome::WouldBlock => {
                                assert!(len > 0 && queued_before == 0 && model_writers > 0, "step {step}");
                            }
                        }
                    }
                } else if op < 92 {
                    // set_capacity (including shrinking over a smaller limit).
                    let req = match rng.gen_range(7) {
                        0 => 0,
                        1 => rng.gen_range(PAGE_SIZE + 1),
                        2 => PAGE_SIZE + rng.gen_range(max_cap),
                        3 => max_cap,
                        4 => max_cap + 1,
                        5 => (1usize << 31) + 1,
                        _ => usize::MAX,
                    };
                    let lim = if rng.gen_range(4) == 0 { 2 * PAGE_SIZE } else { max_cap };
                    let expected = match round_capacity(req) {
                        None => Err(SetCapacityError::Invalid),
                        Some(r) if r > model_cap && r > lim => Err(SetCapacityError::OverLimit),
                        Some(r) if model.len() > r => Err(SetCapacityError::Busy),
                        Some(r) => {
                            let wake_writers = r > model_cap;
                            model_cap = r;
                            Ok(Resized { capacity: r, wake_writers })
                        }
                    };
                    assert_eq!(pipe.set_capacity(req, lim), expected, "step {step}: set_capacity({req}, {lim})");
                } else {
                    // Open/release ends freely (including down to 0, Underflow, and reopening from 0).
                    match rng.gen_range(4) {
                        0 if model_readers < 3 => {
                            pipe.open_reader();
                            model_readers += 1;
                        }
                        1 => {
                            let got = pipe.release_reader();
                            let expected = match model_readers.checked_sub(1) {
                                None => Released::Underflow,
                                Some(0) => {
                                    model_readers = 0;
                                    Released::Last
                                }
                                Some(n) => {
                                    model_readers = n;
                                    Released::Remaining(nz(n))
                                }
                            };
                            assert_eq!(got, expected, "step {step}: release_reader");
                        }
                        2 if model_writers < 3 => {
                            pipe.open_writer();
                            model_writers += 1;
                        }
                        _ => {
                            let got = pipe.release_writer();
                            let expected = match model_writers.checked_sub(1) {
                                None => Released::Underflow,
                                Some(0) => {
                                    model_writers = 0;
                                    Released::Last
                                }
                                Some(n) => {
                                    model_writers = n;
                                    Released::Remaining(nz(n))
                                }
                            };
                            assert_eq!(got, expected, "step {step}: release_writer");
                        }
                    }
                }

                // Check all invariants and readiness predicates after every step.
                assert_eq!(pipe.capacity(), model_cap, "step {step}: capacity");
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
