//! Pure `no_std + alloc` POSIX pipe ring buffer and semantics.
//!
//! Models the byte buffer, capacity rules (`F_SETPIPE_SZ`), `PIPE_BUF` write
//! atomicity, partial writes, EOF and broken-pipe (`-EPIPE`/`SIGPIPE`)
//! decisions, `FIONREAD`, non-destructive `peek`/`consume`, and readiness
//! predicates for Linux/POSIX pipes (`fs/pipe.c`, `pipe(7)`). Note that
//! [`Pipe::readable`] expresses the non-blocking-read / `select(2)` rule (data
//! or no writers), whereas Linux `pipe_poll` sets `EPOLLIN` only when data is
//! queued (`!is_empty()`) and `EPOLLHUP` via [`Pipe::reader_hung_up`].
//!
//! No kernel locks, wait queues, file descriptors, or signal delivery live
//! here; the kernel wraps [`Pipe`] behind a **`SleepMutex`** (not an IRQ-off
//! `SpinLock`, because [`Pipe::set_capacity`] allocates and copies under the
//! pipe lock) plus read/write wait queues.
//!
//! # Allocation guarantee
//!
//! Heap allocation occurs **only** via fallible `try_reserve_exact` when
//! creating a [`Pipe`] ([`Pipe::try_new`], [`Pipe::with_capacity`]) or when
//! [`Pipe::set_capacity`] changes the buffer size. [`Pipe::read`],
//! [`Pipe::peek`], [`Pipe::consume`], [`Pipe::write`],
//! [`WriteProgress::step`], [`Pipe::fionread`], and all readiness predicates
//! never allocate.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::vec::Vec;
use core::fmt;

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

/// Error returned by [`Pipe::try_new`], [`Pipe::with_capacity`], and
/// [`Pipe::set_capacity`].
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
    /// Every byte of the write has been placed into the pipe; contains the
    /// total byte count.
    Done(usize),
    /// Consumed `wrote_now` bytes from `next`; more bytes remain and the pipe
    /// still has enough room for the next chunk without sleeping.
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

/// State machine driving a blocking `write(2)` of any size across chunked
/// copies from userspace and lock/wait/retry iterations.
///
/// POSIX permits splitting writes larger than [`PIPE_BUF`]; Linux's
/// `anon_pipe_write` fills a page slot at a time and sleeps for a free slot.
/// Here, a non-atomic write (`total > PIPE_BUF`) steps only when at least
/// `min(remaining, PIPE_BUF)` bytes are free — the byte-ring counterpart of a
/// free page slot — so a large writer cannot refill every freed byte and
/// starve small atomic writers or thrash on tiny reads. Writes of at most
/// [`PIPE_BUF`] bytes remain strictly atomic and require only their own length
/// in free space.
///
/// Intentionally neither `Copy` nor `Clone`: stepping a stale copy would
/// duplicate bytes into the pipe.
///
/// # Kernel loop and wake rules
///
/// The kernel holds the pipe behind a `SleepMutex` and follows three writer
/// wake rules:
///
/// 1. A read (`read` or `consume`) wakes waiting writers when
///    [`Pipe::writable`] transitions from `false` to `true`.
/// 2. A growing [`Pipe::set_capacity`] wakes waiting writers.
/// 3. A writer that finishes with room left (`pipe.writable()`) wakes the next
///    waiting writer (matching Linux's `wake_next_writer`).
///
/// On an interrupted wait, the kernel retakes the lock and calls
/// [`WriteProgress::step`] once more before checking the signal (matching
/// Linux `anon_pipe_write`, which rechecks readers and room first); it returns
/// [`WriteProgress::interrupted`] only if that re-step yields
/// [`WriteStep::NeedSpace`]:
///
/// ```ignore
/// let mut progress = WriteProgress::new(user_len);
/// let mut offset = 0usize;
/// let mut kbuf = [0u8; PIPE_BUF];
/// let mut interrupted = false;
/// loop {
///     let chunk_len = (user_len - offset).min(kbuf.len());
///     if chunk_len > 0 {
///         copy_from_user(&mut kbuf[..chunk_len], user_ptr + offset)?;
///     }
///     let step = {
///         let mut pipe = inode.lock();
///         progress.step(&mut pipe, &kbuf[..chunk_len]).expect("valid chunk")
///     };
///     match step {
///         WriteStep::Done(n) => {
///             if n > 0 {
///                 inode.wake_readers();
///             }
///             if inode.lock().writable() {
///                 inode.wake_next_writer();
///             }
///             return Ok(n);
///         }
///         WriteStep::Continue { wrote_now } => {
///             offset += wrote_now;
///             if wrote_now > 0 {
///                 inode.wake_readers();
///             }
///         }
///         WriteStep::NeedSpace { wrote_now } => {
///             offset += wrote_now;
///             if wrote_now > 0 {
///                 inode.wake_readers();
///             }
///             if interrupted {
///                 return match progress.interrupted() {
///                     InterruptedWrite::Partial(n) => Ok(n),
///                     InterruptedWrite::Restart => Err(ERESTARTSYS),
///                 };
///             }
///             // Sleep until `progress.can_progress(&pipe)` or a signal is pending;
///             // if a signal woke us, loop once more to recheck readers and room.
///             interrupted = wait_for_write_progress(&inode, &progress).is_interrupted();
///         }
///         WriteStep::Broken { written } => {
///             deliver_sigpipe_to_current();
///             return if written > 0 { Ok(written) } else { Err(EPIPE) };
///         }
///     }
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

    /// Whether this write requires `PIPE_BUF` atomicity (`total <= PIPE_BUF`).
    ///
    /// Atomic writes must pass the entire write slice to [`WriteProgress::step`].
    pub const fn is_atomic(&self) -> bool {
        self.total <= PIPE_BUF
    }

    const fn remaining(&self) -> usize {
        self.total - self.written
    }

    /// Whether calling [`WriteProgress::step`] on `pipe` will either finish,
    /// copy at least one byte, or observe [`WriteStep::Broken`] (at the time
    /// of the check).
    pub const fn can_progress(&self, pipe: &Pipe) -> bool {
        let rem = self.remaining();
        rem == 0 || pipe.readers == 0 || pipe.allowed(rem, true) > 0
    }

    /// Execute one non-sleeping iteration of the blocking write against `pipe`,
    /// consuming a prefix of `next`.
    ///
    /// Check order (matching Linux `fs/pipe.c` `anon_pipe_write`):
    /// 1. Validate `next` against the remaining byte count (returning
    ///    [`ChunkError`] without panicking if empty when bytes remain, longer
    ///    than remaining, or a partial slice for an atomic write).
    /// 2. If nothing remains (including `WriteProgress::new(0)` with `&[]`),
    ///    return [`WriteStep::Done`] before checking readers.
    /// 3. If no readers remain, return [`WriteStep::Broken`] before writing
    ///    any byte.
    /// 4. Otherwise write up to `next.len()` bytes if the pipe meets the
    ///    progress threshold (`min(remaining, PIPE_BUF)`).
    ///
    /// Never allocates.
    pub fn step(&mut self, pipe: &mut Pipe, next: &[u8]) -> Result<WriteStep, ChunkError> {
        let rem = self.remaining();
        if next.len() > rem {
            return Err(ChunkError::TooLong);
        }
        if rem == 0 {
            return Ok(WriteStep::Done(self.total));
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
            Ok(WriteStep::Done(self.total))
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
}

/// A POSIX pipe byte ring buffer with open reader/writer file-description counts.
///
/// The kernel must protect a `Pipe` with a `SleepMutex` (never an IRQ-off
/// `SpinLock`), since [`Pipe::set_capacity`] allocates and copies under it.
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

fn checked_capacity(bytes: usize, current_cap: usize, max_bytes: usize) -> Result<usize, SetCapacityError> {
    let new_cap = round_capacity(bytes).ok_or(SetCapacityError::Invalid)?;
    if new_cap > current_cap && new_cap > max_bytes {
        return Err(SetCapacityError::OverLimit);
    }
    Ok(new_cap)
}

fn alloc_buf(cap: usize) -> Result<Vec<u8>, SetCapacityError> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(cap).map_err(|_| SetCapacityError::NoMemory)?;
    buf.resize(cap, 0);
    Ok(buf)
}

impl Pipe {
    const fn with_buf(buf: Vec<u8>) -> Self {
        Self { buf, head: 0, len: 0, readers: 1, writers: 1 }
    }

    /// Create a pipe with [`DEFAULT_CAPACITY`] (16 pages = 64 KiB), one open
    /// read file description, and one open write file description.
    ///
    /// Returns [`SetCapacityError::NoMemory`] if the buffer allocation fails.
    pub fn try_new() -> Result<Self, SetCapacityError> {
        Ok(Self::with_buf(alloc_buf(DEFAULT_CAPACITY)?))
    }

    /// Create a pipe whose capacity is `bytes` rounded up per [`round_capacity`],
    /// bounded by `max_bytes`, with one open read description and one open
    /// write description.
    pub fn with_capacity(bytes: usize, max_bytes: usize) -> Result<Self, SetCapacityError> {
        let cap = checked_capacity(bytes, 0, max_bytes)?;
        Ok(Self::with_buf(alloc_buf(cap)?))
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

    pub(crate) const fn free_space(&self) -> usize {
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

    /// Record an additional open read file description (e.g. a FIFO `open`).
    ///
    /// Do **not** call on `dup`/`fork`: duplicated descriptors share one open
    /// file description and release once when its last reference drops.
    ///
    /// # Panics
    ///
    /// Panics on `usize` overflow, or in debug builds if called when
    /// `readers() == 0` (an anonymous pipe's closed read end cannot be reopened).
    pub fn open_reader(&mut self) {
        debug_assert!(self.readers > 0, "open_reader called on a pipe with no open readers");
        self.readers = self.readers.checked_add(1).expect("pipe reader count overflow");
    }

    /// Record an additional open write file description (e.g. a FIFO `open`).
    ///
    /// Do **not** call on `dup`/`fork`: duplicated descriptors share one open
    /// file description and release once when its last reference drops.
    ///
    /// # Panics
    ///
    /// Panics on `usize` overflow, or in debug builds if called when
    /// `writers() == 0` (an anonymous pipe's closed write end cannot be reopened).
    pub fn open_writer(&mut self) {
        debug_assert!(self.writers > 0, "open_writer called on a pipe with no open writers");
        self.writers = self.writers.checked_add(1).expect("pipe writer count overflow");
    }

    /// Release one open read file description (when its last reference is
    /// dropped) and return the remaining reader count.
    ///
    /// When the returned count is `0`, wake waiting writers so they observe
    /// broken pipe (`SIGPIPE`/`-EPIPE`).
    ///
    /// # Panics
    ///
    /// Panics in debug builds if called when `readers() == 0`.
    #[must_use]
    pub fn release_reader(&mut self) -> usize {
        debug_assert!(self.readers > 0, "release_reader called with no open readers");
        self.readers = self.readers.saturating_sub(1);
        self.readers
    }

    /// Release one open write file description (when its last reference is
    /// dropped) and return the remaining writer count.
    ///
    /// When the returned count is `0`, wake waiting readers so they observe
    /// EOF / `POLLHUP`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if called when `writers() == 0`.
    #[must_use]
    pub fn release_writer(&mut self) -> usize {
        debug_assert!(self.writers > 0, "release_writer called with no open writers");
        self.writers = self.writers.saturating_sub(1);
        self.writers
    }

    /// Resize the pipe buffer following Linux `F_SETPIPE_SZ` (`fs/pipe.c`
    /// `pipe_set_size` / `round_pipe_size`).
    ///
    /// - Rounds `bytes` up to a power-of-two multiple of [`PAGE_SIZE`] (at
    ///   least one page; returns [`SetCapacityError::Invalid`] above `1 << 31`).
    /// - Enforces `max_bytes` against the *rounded* size only when growing
    ///   (`new_cap > capacity()`); shrinking is always permitted unless queued
    ///   bytes would not fit.
    /// - Returns [`SetCapacityError::Busy`] (`-EBUSY`) if `fionread() > new_cap`.
    ///   Note that Linux checks occupied page slots rather than exact byte
    ///   count, so Linux can refuse some shrinks that this byte ring accepts.
    /// - Allocates fallibly via `try_reserve_exact`, returning
    ///   [`SetCapacityError::NoMemory`] (`-ENOMEM`) with the existing buffer
    ///   and contents intact on OOM.
    /// - If the rounded capacity equals `capacity()`, returns `Ok(cap)`
    ///   without allocating. When growing succeeds, the kernel should wake
    ///   waiting writers.
    pub fn set_capacity(&mut self, bytes: usize, max_bytes: usize) -> Result<usize, SetCapacityError> {
        let new_cap = checked_capacity(bytes, self.capacity(), max_bytes)?;
        if self.len > new_cap {
            return Err(SetCapacityError::Busy);
        }
        if new_cap == self.capacity() {
            return Ok(new_cap);
        }

        let mut new_buf = alloc_buf(new_cap)?;
        let saved = self.len;
        let copied = self.pop_bytes(&mut new_buf[..saved]);
        debug_assert_eq!(copied, saved);
        self.buf = new_buf;
        self.head = 0;
        self.len = saved;
        Ok(new_cap)
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

    /// Copy up to `dst.len()` queued bytes into `dst` in FIFO order without
    /// removing them from the pipe, returning the byte count copied.
    ///
    /// The kernel's `read(2)` path should hold the pipe `SleepMutex`, call
    /// `peek` into a scratch buffer, copy to userspace, and call
    /// [`Pipe::consume`] only after `copy_to_user` succeeds — so a faulting
    /// user pointer returns `-EFAULT` with the bytes still queued.
    ///
    /// Never allocates.
    pub fn peek(&self, dst: &mut [u8]) -> usize {
        let n = dst.len().min(self.len);
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        let second = n - first;
        dst[first..n].copy_from_slice(&self.buf[..second]);
        n
    }

    /// Remove the oldest `n` bytes from the pipe (after a successful
    /// [`Pipe::peek`] and copy to userspace).
    ///
    /// If [`Pipe::writable`] transitions from `false` to `true` across this
    /// call, the kernel must wake waiting writers.
    ///
    /// # Panics
    ///
    /// Panics if `n > self.fionread()`.
    ///
    /// Never allocates.
    pub fn consume(&mut self, n: usize) {
        assert!(n <= self.len, "Pipe::consume: requested {n} bytes, only {} queued", self.len);
        let cap = self.buf.len();
        self.head = (self.head + n) % cap;
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
    }

    /// Attempt one non-blocking read into `buf` (Linux `fs/pipe.c`
    /// `anon_pipe_read`, `pipe(7)`), composed of [`Pipe::peek`] and
    /// [`Pipe::consume`].
    ///
    /// Kernel `read(2)` paths that copy to fallible user memory should use
    /// [`Pipe::peek`] + [`Pipe::consume`] directly instead of `read` so a copy
    /// fault does not drop data.
    ///
    /// Rules guaranteed:
    /// - An empty `buf` returns [`ReadOutcome::Read`] with `0` immediately.
    /// - If data is queued, copies `buf.len().min(fionread())` bytes in FIFO
    ///   order even when no writers remain; never returns more than is queued.
    /// - If empty and no writers remain, returns [`ReadOutcome::Eof`].
    /// - If empty and writers remain, returns [`ReadOutcome::WouldBlock`]
    ///   (blocking callers wait until [`Pipe::readable`] and retry).
    ///
    /// Never allocates.
    pub fn read(&mut self, buf: &mut [u8]) -> ReadOutcome {
        if buf.is_empty() {
            return ReadOutcome::Read(0);
        }
        if self.len > 0 {
            return ReadOutcome::Read(self.pop_bytes(buf));
        }
        if self.writers == 0 { ReadOutcome::Eof } else { ReadOutcome::WouldBlock }
    }

    /// Read-end readiness at the time of the check: `true` if data is queued
    /// or no writers remain (so a `read(2)` will not block now).
    ///
    /// Note: this is the `select(2)` / non-blocking-read rule. When building a
    /// Linux `poll(2)` bitmask from `fs/pipe.c` `pipe_poll`, `EPOLLIN |
    /// EPOLLRDNORM` is set only when `!pipe.is_empty()`, while `EPOLLHUP` is
    /// given by [`Pipe::reader_hung_up`].
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
    /// `if (!pipe->writers && filp->f_version != pipe->w_counter) mask |= EPOLLHUP;`.
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

    /// Single `PIPE_BUF` / progress-threshold rule shared by [`Pipe::write`],
    /// [`WriteProgress::step`], and [`WriteProgress::can_progress`].
    ///
    /// - Blocking (`WriteProgress`): requires `len.min(PIPE_BUF)` bytes free
    ///   (exact length for atomic writes or non-atomic tails `< PIPE_BUF`; a
    ///   full `PIPE_BUF` for larger non-atomic steps).
    /// - Non-blocking (`Pipe::write`): atomic writes (`len <= PIPE_BUF`)
    ///   require `len` bytes free; larger writes take any available space.
    const fn allowed(&self, len: usize, blocking: bool) -> usize {
        let free = self.free_space();
        let need = if blocking || len <= PIPE_BUF { if len < PIPE_BUF { len } else { PIPE_BUF } } else { 1 };
        if free < need {
            0
        } else if len < free {
            len
        } else {
            free
        }
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

    fn pop_bytes(&mut self, dst: &mut [u8]) -> usize {
        let n = self.peek(dst);
        self.consume(n);
        n
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

    #[test]
    fn default_pipe_geometry_counts_and_debug() {
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

        p.open_reader();
        p.open_writer();
        assert_eq!(p.readers(), 2);
        assert_eq!(p.writers(), 2);
        assert_eq!(p.release_reader(), 1);
        assert_eq!(p.release_reader(), 0);
        assert_eq!(p.release_writer(), 1);
        assert_eq!(p.release_writer(), 0);
    }

    #[test]
    fn atomic_writes_at_pipe_buf_and_pipe_buf_plus_one() {
        // Table of (capacity, free_space, write_len, expected_nonblocking, expected_blocking_step_kind)
        // checking exact boundaries around PIPE_BUF and small atomic writes.
        let cases = [
            (PAGE_SIZE, PIPE_BUF - 1, PIPE_BUF, WriteOutcome::WouldBlock, false),
            (PAGE_SIZE, PIPE_BUF, PIPE_BUF, WriteOutcome::Wrote(PIPE_BUF), true),
            (PAGE_SIZE, PIPE_BUF - 1, PIPE_BUF - 1, WriteOutcome::Wrote(PIPE_BUF - 1), true),
            (PAGE_SIZE, 100, 100, WriteOutcome::Wrote(100), true),
            (PAGE_SIZE, 99, 100, WriteOutcome::WouldBlock, false),
            (PAGE_SIZE, PIPE_BUF - 1, PIPE_BUF + 1, WriteOutcome::Wrote(PIPE_BUF - 1), false),
            (PAGE_SIZE, 0, PIPE_BUF + 1, WriteOutcome::WouldBlock, false),
            (DEFAULT_CAPACITY, PIPE_BUF - 1, PIPE_BUF, WriteOutcome::WouldBlock, false),
            (DEFAULT_CAPACITY, PIPE_BUF, PIPE_BUF, WriteOutcome::Wrote(PIPE_BUF), true),
            (DEFAULT_CAPACITY, PIPE_BUF - 1, PIPE_BUF + 1, WriteOutcome::Wrote(PIPE_BUF - 1), false),
            (DEFAULT_CAPACITY, PIPE_BUF, PIPE_BUF + 1, WriteOutcome::Wrote(PIPE_BUF), true),
            (DEFAULT_CAPACITY, 0, PIPE_BUF + 1, WriteOutcome::WouldBlock, false),
        ];

        for (cap, free, write_len, expected_nb, blocking_should_write) in cases {
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

            // Blocking WriteProgress::step.
            let mut q = Pipe::with_capacity(cap, DEFAULT_MAX_CAPACITY).unwrap();
            if prefill > 0 {
                assert_eq!(q.write(&filler), WriteOutcome::Wrote(prefill));
            }
            let mut prog = WriteProgress::new(write_len);
            let chunk_len = if prog.is_atomic() { write_len } else { write_len.min(PIPE_BUF) };
            assert_eq!(
                prog.can_progress(&q),
                blocking_should_write,
                "can_progress cap={cap} free={free} len={write_len}"
            );
            let step = prog.step(&mut q, &payload[..chunk_len]).unwrap();
            if blocking_should_write {
                if write_len <= PIPE_BUF {
                    assert_eq!(step, WriteStep::Done(write_len));
                } else {
                    assert_eq!(step, WriteStep::NeedSpace { wrote_now: chunk_len });
                }
            } else {
                assert_eq!(step, WriteStep::NeedSpace { wrote_now: 0 });
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
        assert_eq!(p.release_reader(), 1);
        assert_eq!(p.write(b"x"), WriteOutcome::WouldBlock);

        // Closing the final reader makes Broken take precedence over WouldBlock
        // on a full pipe for small, atomic, and large writes alike.
        assert_eq!(p.release_reader(), 0);
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

        // WriteProgress::step check order (Item 3):
        // 1) Empty pipe (plenty of room!) after release_reader -> Broken before writing anything.
        let mut empty = Pipe::try_new().unwrap();
        assert_eq!(empty.release_reader(), 0);
        assert_eq!(empty.write(b"hi"), WriteOutcome::Broken);
        let mut empty_large = WriteProgress::new(PIPE_BUF + 100);
        assert!(empty_large.can_progress(&empty));
        assert_eq!(empty_large.step(&mut empty, &big[..PIPE_BUF]), Ok(WriteStep::Broken { written: 0 }));
        assert_eq!(empty.fionread(), 0);

        // 2) Atomic WriteProgress on an empty pipe after release_reader -> Broken.
        let mut empty_atomic = WriteProgress::new(100);
        assert!(empty_atomic.can_progress(&empty));
        assert_eq!(empty_atomic.step(&mut empty, &atomic[..100]), Ok(WriteStep::Broken { written: 0 }));
        assert_eq!(empty.fionread(), 0);

        // 3) Zero-length WriteProgress with no readers -> Done(0) before readers check.
        let mut zero_prog = WriteProgress::new(0);
        assert!(zero_prog.can_progress(&empty));
        assert_eq!(zero_prog.step(&mut empty, &[]), Ok(WriteStep::Done(0)));

        // 4) After partial progress, reader drains space and then closes: next
        // step MUST return Broken { written } even though free space exists.
        let mut mid = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let data = ramp(0, 10_000);
        let mut mid_prog = WriteProgress::new(data.len());
        assert_eq!(mid_prog.step(&mut mid, &data[..PIPE_BUF]), Ok(WriteStep::Continue { wrote_now: PIPE_BUF }));
        assert_eq!(mid.free_space(), PIPE_BUF);
        assert_eq!(mid.release_reader(), 0);
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
        assert_eq!(p.release_writer(), 1);
        let mut buf = [0xFFu8; 20];
        assert_eq!(p.read(&mut buf[..5]), ReadOutcome::Read(5));
        assert_eq!(&buf[..5], b"hello");

        // Closing the last writer while data remains: must drain all data first.
        assert_eq!(p.release_writer(), 0);
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
    fn peek_and_consume_preserve_bytes_across_faults_and_wraps() {
        let mut p = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        // Advance head near the end of the ring so peek/consume wrap around.
        let lead = vec![0x11; PAGE_SIZE - 5];
        assert_eq!(p.write(&lead), WriteOutcome::Wrote(lead.len()));
        let mut scratch = vec![0u8; PAGE_SIZE - 5];
        assert_eq!(p.read(&mut scratch), ReadOutcome::Read(lead.len()));
        // Re-advance without emptying so head stays at PAGE_SIZE - 5.
        assert_eq!(p.write(&vec![0x22; PAGE_SIZE - 5]), WriteOutcome::Wrote(PAGE_SIZE - 5));
        assert_eq!(p.write(b"ABCDE"), WriteOutcome::Wrote(5));
        assert_eq!(p.read(&mut vec![0u8; PAGE_SIZE - 5]), ReadOutcome::Read(PAGE_SIZE - 5));
        // Now 5 bytes "ABCDE" sit at [PAGE_SIZE - 5..PAGE_SIZE]; write 5 more at [0..5].
        assert_eq!(p.write(b"FGHIJ"), WriteOutcome::Wrote(5));
        assert_eq!(p.fionread(), 10);

        // Simulated copy_to_user fault: peek twice without calling consume;
        // both peeks observe the exact same wrapped bytes and fionread is unchanged.
        let mut dst1 = [0xFFu8; 16];
        let mut dst2 = [0xEEu8; 16];
        assert_eq!(p.peek(&mut dst1), 10);
        assert_eq!(&dst1[..10], b"ABCDEFGHIJ");
        assert!(dst1[10..].iter().all(|&b| b == 0xFF));
        assert_eq!(p.fionread(), 10);

        assert_eq!(p.peek(&mut dst2[..7]), 7);
        assert_eq!(&dst2[..7], b"ABCDEFG");
        assert_eq!(p.fionread(), 10);

        // Consuming 7 leaves "HIJ" readable.
        p.consume(7);
        assert_eq!(p.fionread(), 3);
        let mut tail = [0u8; 8];
        assert_eq!(p.peek(&mut tail), 3);
        assert_eq!(&tail[..3], b"HIJ");
        p.consume(3);
        assert!(p.is_empty());
        assert_eq!(p.peek(&mut tail), 0);
        p.consume(0);
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
                            assert_eq!(p.release_reader(), 0);
                        }
                        if writers == 0 {
                            assert_eq!(p.release_writer(), 0);
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

        // with_capacity rounding and error checks.
        assert_eq!(Pipe::with_capacity(0, DEFAULT_MAX_CAPACITY).unwrap().capacity(), PAGE_SIZE);
        assert_eq!(Pipe::with_capacity(5000, DEFAULT_MAX_CAPACITY).unwrap().capacity(), 2 * PAGE_SIZE);
        assert_eq!(Pipe::with_capacity(8192, 4096).unwrap_err(), SetCapacityError::OverLimit);
        assert_eq!(Pipe::with_capacity((1usize << 31) + 1, usize::MAX).unwrap_err(), SetCapacityError::Invalid);

        let mut p = Pipe::try_new().unwrap();
        // Minimum: 0 and 1 round up to 1 page.
        assert_eq!(p.set_capacity(0, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));
        assert_eq!(p.capacity(), PAGE_SIZE);
        assert_eq!(p.set_capacity(1, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));

        // Limit checked against the *rounded* size (3*PAGE rounds to 4*PAGE > 3*PAGE).
        assert_eq!(p.set_capacity(3 * PAGE_SIZE, 3 * PAGE_SIZE), Err(SetCapacityError::OverLimit));
        assert_eq!(p.capacity(), PAGE_SIZE);

        // Rounding up to power-of-two pages.
        assert_eq!(p.set_capacity(PAGE_SIZE + 1, DEFAULT_MAX_CAPACITY), Ok(2 * PAGE_SIZE));
        assert_eq!(p.set_capacity(3 * PAGE_SIZE - 10, DEFAULT_MAX_CAPACITY), Ok(4 * PAGE_SIZE));

        // Maximum and Invalid checks.
        assert_eq!(p.set_capacity(DEFAULT_MAX_CAPACITY, DEFAULT_MAX_CAPACITY), Ok(DEFAULT_MAX_CAPACITY));
        assert_eq!(p.set_capacity(DEFAULT_MAX_CAPACITY + 1, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::OverLimit));
        assert_eq!(p.set_capacity((1usize << 31) + 1, usize::MAX), Err(SetCapacityError::Invalid));
        assert_eq!(p.set_capacity(usize::MAX, DEFAULT_MAX_CAPACITY), Err(SetCapacityError::Invalid));
        assert_eq!(p.capacity(), DEFAULT_MAX_CAPACITY);

        // Shrinking is always permitted even when current and new capacities
        // both exceed max_bytes; growing above max_bytes is refused.
        assert_eq!(p.set_capacity(4 * PAGE_SIZE, 2 * PAGE_SIZE), Ok(4 * PAGE_SIZE));
        assert_eq!(p.set_capacity(4 * PAGE_SIZE, 2 * PAGE_SIZE), Ok(4 * PAGE_SIZE));
        assert_eq!(p.set_capacity(8 * PAGE_SIZE, 2 * PAGE_SIZE), Err(SetCapacityError::OverLimit));
        assert_eq!(p.capacity(), 4 * PAGE_SIZE);

        // Wrapped grow, -EBUSY, and wrapped shrink.
        assert_eq!(p.set_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(2 * PAGE_SIZE));
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

        // Drain 1000 bytes so 4000 bytes remain, STILL wrapped across offset 8192
        // (head is at 6500, tail is at 2308), then shrink directly to 1 page!
        let mut prefix = vec![0u8; 1000];
        assert_eq!(p.read(&mut prefix), ReadOutcome::Read(1000));
        assert_eq!(&prefix[..500], &vec![0x99; 500][..]);
        assert_eq!(&prefix[500..], &wrapped[..500]);
        assert_eq!(p.fionread(), 4000);

        assert_eq!(p.set_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(PAGE_SIZE));
        assert_eq!(p.capacity(), PAGE_SIZE);
        let mut after_shrink = vec![0u8; 4000];
        assert_eq!(p.read(&mut after_shrink), ReadOutcome::Read(4000));
        assert_eq!(&after_shrink[..], &wrapped[500..]);
        assert!(p.is_empty());

        // Wrapped grow from 1 page to 4 pages preserves wrapped bytes.
        assert_eq!(p.write(&vec![0x77; 3500]), WriteOutcome::Wrote(3500));
        assert_eq!(p.read(&mut vec![0u8; 3000]), ReadOutcome::Read(3000));
        let wrapped_grow = ramp(3, 3000);
        assert_eq!(p.write(&wrapped_grow), WriteOutcome::Wrote(3000));
        assert_eq!(p.set_capacity(3 * PAGE_SIZE, DEFAULT_MAX_CAPACITY), Ok(4 * PAGE_SIZE));
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
        assert_eq!(p.release_writer(), 0);
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
        let mut tail = vec![0u8; 100];
        assert_eq!(r.read(&mut tail), ReadOutcome::Read(100));
        assert_eq!(tail, wrapped);
        assert!(r.is_empty());
    }

    #[test]
    fn write_progress_chunked_interleaving_threshold_and_interrupts() {
        // 1) Large chunked blocking write (20_000 bytes in 4 KiB chunks) on a
        // 2-page pipe interleaved with reads; bounded loops prevent runaway.
        let mut p = Pipe::with_capacity(2 * PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        let total: usize = 20_000;
        let payload = ramp(17, total);
        let mut progress = WriteProgress::new(total);
        assert!(!progress.is_atomic());

        let mut offset = 0usize;
        let mut received = Vec::with_capacity(total);
        let read_chunks = [1000, 3500, 4096, 2000, 4096, 1500, 3000];
        let mut read_idx = 0usize;
        let mut finished = false;
        let mut saw_continue = false;
        let mut saw_need_space = false;

        for _ in 0..MAX_LOOP_STEPS {
            let chunk_len = (total - offset).min(PIPE_BUF);
            let chunk = &payload[offset..offset + chunk_len];
            match progress.step(&mut p, chunk).unwrap() {
                WriteStep::Done(n) => {
                    assert_eq!(n, total);
                    finished = true;
                    break;
                }
                WriteStep::Continue { wrote_now } => {
                    assert!(wrote_now > 0);
                    saw_continue = true;
                    offset += wrote_now;
                }
                WriteStep::NeedSpace { wrote_now } => {
                    saw_need_space = true;
                    offset += wrote_now;
                    assert!(!progress.can_progress(&p));
                    // Stepping again before space is freed makes zero progress.
                    let retry_len = (total - offset).min(PIPE_BUF);
                    assert_eq!(
                        progress.step(&mut p, &payload[offset..offset + retry_len]),
                        Ok(WriteStep::NeedSpace { wrote_now: 0 })
                    );
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

        // Bounded tail drain.
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

        // 2) Non-atomic threshold (Item 4): does NOT progress when
        // 0 < free < min(remaining, PIPE_BUF), so a small atomic writer is not starved.
        let mut q = Pipe::with_capacity(PAGE_SIZE, DEFAULT_MAX_CAPACITY).unwrap();
        assert_eq!(q.write(&vec![0x10; PAGE_SIZE]), WriteOutcome::Wrote(PAGE_SIZE));
        let data5k = ramp(0, 5000);
        let mut large = WriteProgress::new(data5k.len());
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
        assert_eq!(large.interrupted(), InterruptedWrite::Restart);

        // Free 300 bytes (< PIPE_BUF): large writer still cannot progress, while
        // a 300-byte atomic writer CAN progress (needs only its own length, not PIPE_BUF).
        assert_eq!(q.read(&mut [0u8; 300]), ReadOutcome::Read(300));
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));

        let small_data = ramp(9, 300);
        let mut small_atomic = WriteProgress::new(300);
        assert!(small_atomic.is_atomic());
        assert!(small_atomic.can_progress(&q));
        assert_eq!(small_atomic.step(&mut q, &small_data), Ok(WriteStep::Done(300)));

        // Drain the full page so free == PIPE_BUF: large writer now writes 4096 bytes.
        assert_eq!(q.read(&mut vec![0u8; PAGE_SIZE]), ReadOutcome::Read(PAGE_SIZE));
        assert!(large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[..PIPE_BUF]), Ok(WriteStep::NeedSpace { wrote_now: PIPE_BUF }));
        assert_eq!(large.interrupted(), InterruptedWrite::Partial(PIPE_BUF));

        // Tail has 904 bytes remaining (< PIPE_BUF): needs 904 free (not 4096).
        assert_eq!(q.read(&mut [0u8; 903]), ReadOutcome::Read(903));
        assert!(!large.can_progress(&q));
        assert_eq!(large.step(&mut q, &data5k[PIPE_BUF..]), Ok(WriteStep::NeedSpace { wrote_now: 0 }));
        assert_eq!(q.read(&mut [0u8; 1]), ReadOutcome::Read(1));
        assert!(large.can_progress(&q));
        // Simulates interrupted wait re-step succeeding with Done instead of returning Partial.
        assert_eq!(large.step(&mut q, &data5k[PIPE_BUF..]), Ok(WriteStep::Done(5000)));
        assert_eq!(large.step(&mut q, &[]), Ok(WriteStep::Done(5000)));

        // 3) ChunkError cases (no panics on bad chunks).
        let mut err_pipe = Pipe::try_new().unwrap();
        let mut p_atomic = WriteProgress::new(100);
        assert_eq!(p_atomic.step(&mut err_pipe, &[]), Err(ChunkError::Empty));
        assert_eq!(p_atomic.step(&mut err_pipe, &[0u8; 50]), Err(ChunkError::SplitAtomic));
        assert_eq!(p_atomic.step(&mut err_pipe, &[0u8; 101]), Err(ChunkError::TooLong));
        assert_eq!(p_atomic.step(&mut err_pipe, &[0u8; 100]), Ok(WriteStep::Done(100)));
        assert_eq!(p_atomic.step(&mut err_pipe, &[0u8; 1]), Err(ChunkError::TooLong));

        let mut p_zero = WriteProgress::new(0);
        assert_eq!(p_zero.step(&mut err_pipe, &[0u8; 1]), Err(ChunkError::TooLong));
        assert_eq!(p_zero.step(&mut err_pipe, &[]), Ok(WriteStep::Done(0)));

        let mut p_large = WriteProgress::new(5000);
        assert_eq!(p_large.step(&mut err_pipe, &[]), Err(ChunkError::Empty));
        assert_eq!(p_large.step(&mut err_pipe, &vec![0u8; 5001]), Err(ChunkError::TooLong));
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

            // Optional in-flight blocking WriteProgress interleaved with other ops.
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
                        _ => model_cap.saturating_sub(model.len()),
                    };
                    let mut data = Vec::with_capacity(len);
                    for _ in 0..len {
                        data.push(byte_seq);
                        byte_seq = byte_seq.wrapping_add(1);
                    }
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
                                    // Law: atomic write is never split.
                                    assert_eq!(n, data.len(), "step {step}: split atomic write");
                                    assert!(free_before >= data.len(), "step {step}");
                                } else {
                                    // Law: large non-blocking write writes min(len, free).
                                    assert_eq!(n, data.len().min(free_before), "step {step}");
                                }
                                model.extend(&data[..n]);
                            }
                            assert_eq!(pipe.fionread(), len_before + n, "step {step}");
                        }
                        WriteOutcome::WouldBlock => {
                            assert!(!data.is_empty() && model_readers > 0, "step {step}");
                            // Law: WouldBlock implies no room (for atomic: < len; for large: == 0).
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
                    // Interleaved chunked WriteProgress step.
                    if inflight.is_none() {
                        let total = match rng.gen_range(5) {
                            0 => 0,
                            1 => 1 + rng.gen_range(200),
                            2 => PIPE_BUF,
                            3 => PIPE_BUF + 1,
                            _ => PIPE_BUF + 1 + rng.gen_range(2 * PAGE_SIZE),
                        };
                        let mut data = Vec::with_capacity(total);
                        for _ in 0..total {
                            data.push(byte_seq);
                            byte_seq = byte_seq.wrapping_add(1);
                        }
                        inflight = Some((WriteProgress::new(total), data, 0));
                    }
                    let (mut prog, data, mut off) = inflight.take().unwrap();
                    let rem = data.len() - off;
                    let chunk_len = if rem == 0 {
                        0
                    } else if prog.is_atomic() {
                        rem
                    } else {
                        let c = 1 + rng.gen_range(PIPE_BUF);
                        c.min(rem)
                    };
                    let free_before = model_cap - model.len();
                    let can_before = prog.can_progress(&pipe);
                    let res = prog.step(&mut pipe, &data[off..off + chunk_len]).unwrap();

                    match res {
                        WriteStep::Done(n) => {
                            assert!(can_before, "step {step}");
                            assert_eq!(n, data.len(), "step {step}");
                            if rem > 0 {
                                assert!(model_readers > 0, "step {step}");
                                assert_eq!(chunk_len, rem, "step {step}");
                                assert!(free_before >= rem, "step {step}");
                                model.extend(&data[off..]);
                            }
                        }
                        WriteStep::Continue { wrote_now } => {
                            assert!(can_before && model_readers > 0, "step {step}");
                            assert!(wrote_now > 0 && wrote_now <= chunk_len, "step {step}");
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
                                assert!(!prog.is_atomic(), "step {step}: split atomic WriteProgress");
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
                    // Read or peek+consume.
                    let len = match rng.gen_range(5) {
                        0 => 0,
                        1 => 1 + rng.gen_range(64),
                        2 => 1 + rng.gen_range(PIPE_BUF),
                        3 => PIPE_BUF + rng.gen_range(PAGE_SIZE),
                        _ => model.len().max(1),
                    };
                    let mut buf = vec![0xEE; len];
                    if rng.gen_range(4) == 0 {
                        // Exercise non-destructive peek + consume.
                        let n = pipe.peek(&mut buf);
                        assert_eq!(n, len.min(model.len()), "step {step}: peek len");
                        let expected_bytes: Vec<u8> = model.iter().take(n).copied().collect();
                        assert_eq!(&buf[..n], &expected_bytes[..], "step {step}: peek data");
                        assert!(buf[n..].iter().all(|&b| b == 0xEE), "step {step}: peek overwrote");
                        if rng.gen_range(2) == 0 {
                            let take = if n == 0 { 0 } else { 1 + rng.gen_range(n) };
                            pipe.consume(take);
                            model.drain(..take);
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
                            model_cap = r;
                            Ok(r)
                        }
                    };
                    assert_eq!(pipe.set_capacity(req, lim), expected, "step {step}: set_capacity({req}, {lim})");
                } else {
                    // Open/release ends while respecting anonymous-pipe invariants
                    // (keep >= 1 until the final steps, where we may close to 0).
                    let allow_zero = step >= 2_800;
                    match rng.gen_range(4) {
                        0 if model_readers > 0 && model_readers < 3 => {
                            pipe.open_reader();
                            model_readers += 1;
                        }
                        1 if model_readers > usize::from(!allow_zero) => {
                            let rem = pipe.release_reader();
                            model_readers -= 1;
                            assert_eq!(rem, model_readers);
                        }
                        2 if model_writers > 0 && model_writers < 3 => {
                            pipe.open_writer();
                            model_writers += 1;
                        }
                        3 if model_writers > usize::from(!allow_zero) => {
                            let rem = pipe.release_writer();
                            model_writers -= 1;
                            assert_eq!(rem, model_writers);
                        }
                        _ => {}
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
