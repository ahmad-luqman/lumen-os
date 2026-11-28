//! Pipes and FIFOs (M8, C7; ARCHITECTURE section 10).
//!
//! Wires `lumen-pipe`'s byte ring and state machine into kernel wait queues,
//! readiness (`Pollable` / `KernelPollHead`), anonymous `pipefs` inodes
//! (dev `0:6`), and the `FifoSlot` embedded on tmpfs and ext4 FIFO inodes.
//!
//! Lock ordering (ARCHITECTURE section 9):
//! - `PipeObject::pipe` is Lock 0 (`SleepMutex`), held across user copies
//!   (`peek` / `copy_to_user_partial` / `commit_prefix`; non-blocking chunked writes), never
//!   taken while holding locks 1–10.
//! - `PipePoll::end` and `FifoSlot::inner` are level-10c leaf spinlocks
//!   (no allocation under either; detached ends and `Arc<PipeObject>`s dropped
//!   after releasing them).
//! - `PipeObject::poll` (`KernelPollHead`) is level 10e.
//! - `PipeObject::readers` / `writers` (`WaitQueue`) are level 10f.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use lumen_pipe::{
    DEFAULT_MAX_CAPACITY, InterruptedWrite, PIPE_BUF, PeekOutcome, Pipe, Released, SetCapacityError, WriteOutcome,
    WriteProgress, WriteStep,
};
use lumen_signal::Signal;
use lumen_vfs::{
    AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, PollGuard, PollMask, PollWaker,
    Pollable, SetTime, Timespec, VfsError, VfsInode, mode_after_chown, next_lock_id,
};

use crate::linux::process::{self, KernelFile};
use crate::linux::signal::ERESTARTSYS;
use crate::linux::syscall::MAX_SINGLE_IO;
use crate::linux::{errno, general};
use crate::sched::{self, WaitQueue};
use crate::sync::{SleepMutex, SpinLock};
use crate::time;
use crate::user::{copy_from_user, copy_to_user_partial};
use crate::vfs::KernelPollHead;
use crate::vfs::ext4::Ext4Inode;
use crate::vfs::tmpfs::TmpfsInode;

static NEXT_PIPE_INO: AtomicU64 = AtomicU64::new(1);

/// Shared kernel pipe state backing an anonymous pipe or an open FIFO.
pub struct PipeObject {
    /// Lock 0: protects the ring buffer and reader/writer open counts.
    pipe: SleepMutex<Pipe>,
    /// Bumped under `pipe` whenever reader-visible state changes (bytes written,
    /// writer opened, or last writer released).
    read_seq: AtomicU64,
    /// Bumped under `pipe` whenever writer-visible state changes (bytes read,
    /// capacity grown, reader opened, or last reader released).
    write_seq: AtomicU64,
    /// Monotonic count of reader opens (for blocking `fifo_open(O_WRONLY)`).
    r_opens: AtomicU64,
    /// Monotonic count of writer opens (for blocking `fifo_open(O_RDONLY)`).
    w_opens: AtomicU64,
    readers: WaitQueue,
    writers: WaitQueue,
    poll: KernelPollHead,
    #[cfg(feature = "ktest")]
    waiting_readers: AtomicU32,
    #[cfg(feature = "ktest")]
    read_lock_acquisitions: AtomicU64,
    #[cfg(feature = "ktest")]
    waiting_writers: AtomicU32,
}

impl PipeObject {
    /// Create a fresh anonymous pipe (1 reader, 1 writer, 64 KiB buffer).
    pub fn new_pipe() -> Result<Arc<Self>, u32> {
        let pipe = Pipe::try_new().map_err(|_| errno::ENOMEM)?;
        Ok(Self::from_pipe(pipe, 1))
    }

    /// Create a fresh FIFO pipe buffer (0 readers, 0 writers, 64 KiB buffer).
    pub fn new_fifo() -> Result<Arc<Self>, u32> {
        Ok(Self::from_pipe(Pipe::fifo().map_err(|_| errno::ENOMEM)?, 0))
    }

    fn from_pipe(pipe: Pipe, opens: u64) -> Arc<Self> {
        Arc::new(Self {
            pipe: SleepMutex::new(pipe),
            read_seq: AtomicU64::new(0),
            write_seq: AtomicU64::new(0),
            r_opens: AtomicU64::new(opens),
            w_opens: AtomicU64::new(opens),
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            poll: KernelPollHead::new(),
            #[cfg(feature = "ktest")]
            waiting_readers: AtomicU32::new(0),
            #[cfg(feature = "ktest")]
            read_lock_acquisitions: AtomicU64::new(0),
            #[cfg(feature = "ktest")]
            waiting_writers: AtomicU32::new(0),
        })
    }

    /// Readiness mask for one open description (`fs/pipe.c` `pipe_poll`).
    fn poll_mask(pipe: &Pipe, mode: AccessMode, writer_seen: bool) -> PollMask {
        let mut mask = PollMask::EMPTY;
        if mode.can_read() {
            if !pipe.is_empty() {
                mask |= PollMask::IN | PollMask::RDNORM;
            }
            if pipe.reader_hung_up() && writer_seen {
                mask |= PollMask::HUP;
            }
        }
        if mode.can_write() {
            if pipe.writable() {
                mask |= PollMask::OUT | PollMask::WRNORM;
            }
            if pipe.writer_error() {
                mask |= PollMask::ERR;
            }
        }
        mask
    }

    fn notify_readers(&self) {
        self.readers.wake_all();
        self.poll.notify(PollMask::IN | PollMask::RDNORM);
    }

    fn has_space(pipe: &Pipe) -> bool {
        pipe.fionread() < pipe.capacity()
    }

    fn wake_writers_if_space(&self) {
        let has_space = {
            let pipe = self.pipe.lock();
            Self::has_space(&pipe)
        };
        if has_space {
            self.writers.wake_all();
        }
    }

    /// Read up to `count` bytes (capped at `MAX_SINGLE_IO`) into user memory.
    ///
    /// Returns bytes transferred (`0` on EOF or `count == 0`), `-EAGAIN` with
    /// `nonblock` when empty with writers open, `-ERESTARTSYS` when interrupted
    /// before transferring any byte, or `-EFAULT` if the first byte's copy out
    /// faults (leaving the uncopied bytes queued).
    pub fn read(&self, user_buf: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        self.read_with(count as usize, nonblock, |offset, buf| {
            user_buf.checked_add(offset as u64).map_or(0, |addr| copy_to_user_partial(addr, buf))
        })
    }

    /// One logical read across all spans: hold Lock 0 across every chunk and
    /// copy, dropping it only to wait before any progress or to finish. `fill`
    /// returns the copied prefix length; a short copy faults and commits only
    /// that prefix. It must not take locks preceding Lock 0.
    pub fn read_with(
        &self,
        count: usize,
        nonblock: bool,
        mut fill: impl FnMut(usize, &[u8]) -> usize,
    ) -> Result<u64, u32> {
        let limit = count.min(MAX_SINGLE_IO);
        if limit == 0 {
            return Ok(0);
        }
        let mut kbuf = [0u8; PIPE_BUF];
        let mut done = 0usize;
        let mut wake_writers = false;

        loop {
            let mut pipe = self.pipe.lock();
            #[cfg(feature = "ktest")]
            self.read_lock_acquisitions.fetch_add(1, Ordering::SeqCst);
            let mut faulted = false;
            let mut saw_eof = false;

            while done < limit {
                let want = (limit - done).min(kbuf.len());
                match pipe.peek(&mut kbuf[..want]) {
                    PeekOutcome::Data(peeked) => {
                        let n = peeked.len();
                        let copied = fill(done, &kbuf[..n]);
                        debug_assert!(copied <= n, "read copy exceeded the peeked bytes");
                        let consumed = peeked.commit_prefix(copied);
                        faulted = copied < n;
                        done += consumed.n;
                        wake_writers |= consumed.wake_writers;
                        if faulted {
                            break;
                        }
                    }
                    PeekOutcome::Eof => {
                        saw_eof = true;
                        break;
                    }
                    PeekOutcome::WouldBlock => break,
                }
            }

            if done > 0 || saw_eof || faulted {
                let more_data = !pipe.is_empty();
                let now_writable = pipe.writable();
                if done > 0 {
                    self.write_seq.fetch_add(1, Ordering::Release);
                }
                drop(pipe);
                if wake_writers {
                    self.writers.wake_all();
                }
                if wake_writers && now_writable {
                    self.poll.notify(PollMask::OUT | PollMask::WRNORM);
                }
                if done > 0 && more_data {
                    self.readers.wake_one();
                }
                if faulted {
                    return if done > 0 { Ok(done as u64) } else { Err(errno::EFAULT) };
                }
                return Ok(done as u64);
            }

            if nonblock {
                return Err(errno::EAGAIN);
            }
            let seen = self.read_seq.load(Ordering::Acquire);
            drop(pipe);

            #[cfg(feature = "ktest")]
            self.waiting_readers.fetch_add(1, Ordering::SeqCst);
            let wake = self.readers.wait_interruptible(None, || self.read_seq.load(Ordering::Acquire) != seen);
            #[cfg(feature = "ktest")]
            self.waiting_readers.fetch_sub(1, Ordering::SeqCst);
            if wake == sched::Wake::Interrupted {
                return Err(ERESTARTSYS);
            }
        }
    }

    /// Write from user memory, enforcing `PIPE_BUF` atomicity and delivering
    /// `SIGPIPE` + `EPIPE` before a user copy when no readers remain.
    /// Blocking writes may sleep and are not capped at `MAX_SINGLE_IO`.
    pub fn write(&self, user_buf: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        self.write_with(count as usize, nonblock, |offset, buf| {
            let addr = user_buf.checked_add(offset as u64).ok_or(errno::EFAULT)?;
            copy_from_user(buf, addr).map_err(|_| errno::EFAULT)
        })
    }

    /// One logical write, including all iovecs. `fill` must support retries at
    /// the same offset after a wait. An atomic write fills its entire buffer
    /// before queuing anything, so a later span's fault cannot tear it.
    pub fn write_with(
        &self,
        count: usize,
        nonblock: bool,
        mut fill: impl FnMut(usize, &mut [u8]) -> Result<(), u32>,
    ) -> Result<u64, u32> {
        let limit = if nonblock { count.min(MAX_SINGLE_IO) } else { count };
        if limit == 0 {
            return Ok(0);
        }
        if nonblock { self.write_nonblock(limit, &mut fill) } else { self.write_blocking(limit, &mut fill) }
    }

    fn write_nonblock(
        &self,
        limit: usize,
        fill: &mut impl FnMut(usize, &mut [u8]) -> Result<(), u32>,
    ) -> Result<u64, u32> {
        let mut written = 0usize;
        let mut kbuf = [0u8; PIPE_BUF];
        let mut pipe = self.pipe.lock();

        while written < limit {
            if pipe.writer_error() {
                drop(pipe);
                if written > 0 {
                    self.notify_readers();
                }
                deliver_sigpipe();
                return if written > 0 { Ok(written as u64) } else { Err(errno::EPIPE) };
            }
            let avail = pipe.capacity() - pipe.fionread();
            let chunk_len = if limit <= PIPE_BUF {
                if avail < limit {
                    drop(pipe);
                    return Err(errno::EAGAIN);
                }
                limit
            } else {
                if avail == 0 {
                    break;
                }
                (limit - written).min(avail).min(kbuf.len())
            };
            if fill(written, &mut kbuf[..chunk_len]).is_err() {
                drop(pipe);
                if written > 0 {
                    self.notify_readers();
                    return Ok(written as u64);
                }
                return Err(errno::EFAULT);
            }
            match pipe.write(&kbuf[..chunk_len]) {
                WriteOutcome::Wrote(n) => {
                    written += n;
                    if n > 0 {
                        self.read_seq.fetch_add(1, Ordering::Release);
                    }
                }
                WriteOutcome::WouldBlock => break,
                WriteOutcome::Broken => {
                    drop(pipe);
                    if written > 0 {
                        self.notify_readers();
                    }
                    deliver_sigpipe();
                    return if written > 0 { Ok(written as u64) } else { Err(errno::EPIPE) };
                }
            }
        }

        drop(pipe);
        if written > 0 {
            self.notify_readers();
            Ok(written as u64)
        } else {
            Err(errno::EAGAIN)
        }
    }

    fn write_blocking(
        &self,
        limit: usize,
        fill: &mut impl FnMut(usize, &mut [u8]) -> Result<(), u32>,
    ) -> Result<u64, u32> {
        // Check readers before the first copy_from_user so a bad pointer with
        // no readers returns SIGPIPE + EPIPE rather than EFAULT.
        if self.pipe.lock().writer_error() {
            deliver_sigpipe();
            return Err(errno::EPIPE);
        }

        let mut progress = WriteProgress::new(limit);
        let mut kbuf = [0u8; PIPE_BUF];
        let mut slept = false;
        let mut interrupted = false;

        loop {
            let range = progress.next_chunk(kbuf.len());
            if fill(range.start, &mut kbuf[..range.len()]).is_err() {
                if slept {
                    self.wake_writers_if_space();
                }
                return match progress.faulted() {
                    lumen_pipe::FaultedWrite::Partial(n) => Ok(n as u64),
                    lumen_pipe::FaultedWrite::Fault => Err(errno::EFAULT),
                };
            }

            let mut pipe = self.pipe.lock();
            let step = match progress.step(&mut pipe, &kbuf[..range.len()]) {
                Ok(step) => step,
                Err(err) => {
                    log::error!(target: "pipe", "invalid write chunk: {err:?}");
                    let wake = slept && Self::has_space(&pipe);
                    drop(pipe);
                    if wake {
                        self.writers.wake_all();
                    }
                    return Err(errno::EIO);
                }
            };

            match step {
                WriteStep::Done { total, wake_next_writer } => {
                    if total > 0 {
                        self.read_seq.fetch_add(1, Ordering::Release);
                    }
                    drop(pipe);
                    if total > 0 {
                        self.notify_readers();
                    }
                    if wake_next_writer {
                        self.writers.wake_all();
                    }
                    return Ok(total as u64);
                }
                WriteStep::Continue { wrote_now } => {
                    if wrote_now > 0 {
                        self.read_seq.fetch_add(1, Ordering::Release);
                    }
                    drop(pipe);
                    if wrote_now > 0 {
                        self.notify_readers();
                    }
                }
                WriteStep::NeedSpace { wrote_now } => {
                    if wrote_now > 0 {
                        self.read_seq.fetch_add(1, Ordering::Release);
                    }
                    let seen = self.write_seq.load(Ordering::Acquire);
                    drop(pipe);
                    if wrote_now > 0 {
                        self.notify_readers();
                    }
                    if interrupted {
                        if slept {
                            self.wake_writers_if_space();
                        }
                        return match progress.interrupted() {
                            InterruptedWrite::Partial(n) => Ok(n as u64),
                            InterruptedWrite::Restart => Err(ERESTARTSYS),
                        };
                    }
                    slept = true;
                    #[cfg(feature = "ktest")]
                    self.waiting_writers.fetch_add(1, Ordering::SeqCst);
                    let wake = self.writers.wait_interruptible(None, || self.write_seq.load(Ordering::Acquire) != seen);
                    #[cfg(feature = "ktest")]
                    self.waiting_writers.fetch_sub(1, Ordering::SeqCst);
                    interrupted = wake == sched::Wake::Interrupted;
                }
                WriteStep::Broken { written } => {
                    let wake = slept && Self::has_space(&pipe);
                    drop(pipe);
                    if wake {
                        self.writers.wake_all();
                    }
                    deliver_sigpipe();
                    return if written > 0 { Ok(written as u64) } else { Err(errno::EPIPE) };
                }
            }
        }
    }

    /// Number of unread bytes currently queued (`ioctl(FIONREAD)`).
    pub fn fionread(&self) -> usize {
        self.pipe.lock().fionread()
    }

    /// Current pipe capacity in bytes (`fcntl(F_GETPIPE_SZ)`).
    pub fn capacity(&self) -> usize {
        self.pipe.lock().capacity()
    }

    /// Resize the pipe buffer (`fcntl(F_SETPIPE_SZ)`).
    pub fn set_capacity(&self, bytes: u32) -> Result<usize, u32> {
        let mut pipe = self.pipe.lock();
        let resized = pipe.set_capacity(bytes as usize, DEFAULT_MAX_CAPACITY).map_err(|e| match e {
            SetCapacityError::Invalid => errno::EINVAL,
            SetCapacityError::OverLimit => errno::EPERM,
            SetCapacityError::Busy => errno::EBUSY,
            SetCapacityError::NoMemory => errno::ENOMEM,
        })?;
        let now_writable = pipe.writable();
        if resized.wake_writers {
            self.write_seq.fetch_add(1, Ordering::Release);
        }
        drop(pipe);
        if resized.wake_writers {
            self.writers.wake_all();
            if now_writable {
                self.poll.notify(PollMask::OUT | PollMask::WRNORM);
            }
        }
        Ok(resized.capacity)
    }

    /// Release one open reader and/or writer description on this pipe.
    ///
    /// Called in thread context with no lock held when the description's last
    /// descriptor and last in-flight syscall reference both reach zero, or
    /// when rolling back an open that was never installed.
    fn release_end(&self, reader: bool, writer: bool) {
        let (wake_writers, wake_readers) = {
            let mut pipe = self.pipe.lock();
            let mut ww = false;
            let mut wr = false;
            if reader {
                match pipe.release_reader() {
                    Released::Last => {
                        ww = true;
                        self.write_seq.fetch_add(1, Ordering::Release);
                    }
                    Released::Remaining(_) => {}
                    Released::Underflow => log::error!(target: "pipe", "pipe reader count underflow"),
                }
            }
            if writer {
                match pipe.release_writer() {
                    Released::Last => {
                        wr = true;
                        self.read_seq.fetch_add(1, Ordering::Release);
                    }
                    Released::Remaining(_) => {}
                    Released::Underflow => log::error!(target: "pipe", "pipe writer count underflow"),
                }
            }
            (ww, wr)
        };
        if wake_writers {
            self.writers.wake_all();
            self.poll.notify(PollMask::ERR);
        }
        if wake_readers {
            self.readers.wake_all();
            self.poll.notify(PollMask::HUP);
        }
    }

    /// Description-aware anonymous pipe and FIFO readiness. Subscribe before
    /// reading the generation or counts under Lock 0, preserving the wake contract.
    pub fn poll_open(
        &self,
        mode: AccessMode,
        writer_generation: u64,
        waker: Option<&Arc<dyn PollWaker>>,
    ) -> (PollMask, Option<PollGuard>) {
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let pipe = self.pipe.lock();
        let writer_seen = self.w_opens.load(Ordering::Acquire) != writer_generation;
        (Self::poll_mask(&pipe, mode, writer_seen), guard)
    }

    /// Reader and writer description counts, for interrupted-open ktests.
    #[cfg(feature = "ktest")]
    pub fn end_counts(&self) -> (usize, usize) {
        let pipe = self.pipe.lock();
        (pipe.readers(), pipe.writers())
    }

    /// Actual read-loop Lock 0 acquisitions, including retries after waits.
    /// Pins readv's single critical section independently of reader scheduling.
    #[cfg(feature = "ktest")]
    pub fn read_lock_acquisitions(&self) -> u64 {
        self.read_lock_acquisitions.load(Ordering::SeqCst)
    }

    /// Number of threads currently blocked in a read wait (ktests).
    #[cfg(feature = "ktest")]
    pub fn waiting_readers(&self) -> u32 {
        self.waiting_readers.load(Ordering::SeqCst)
    }

    /// Number of threads currently blocked in a write wait (ktests).
    #[cfg(feature = "ktest")]
    pub fn waiting_writers(&self) -> u32 {
        self.waiting_writers.load(Ordering::SeqCst)
    }

    /// Number of active poll subscribers on this pipe (ktests).
    #[cfg(feature = "ktest")]
    pub fn subscriber_count(&self) -> usize {
        self.poll.subscriber_count()
    }
}

impl Pollable for PipeObject {
    fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        // Subscribe before locking the pipe so no wake between reading the
        // mask and returning can be missed.
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let pipe = self.pipe.lock();
        (Self::poll_mask(&pipe, mode, true), guard)
    }
}

/// Deliver `SIGPIPE` to the calling thread (Linux `send_sig(SIGPIPE, current, 0)`).
pub fn deliver_sigpipe() {
    process::send_current_signal(Signal::PIPE);
}

/// Per-FIFO-inode slot holding the attached `PipeObject` while open
/// descriptions exist (ARCHITECTURE section 10, level-10c leaf lock).
pub struct FifoSlot {
    inner: SpinLock<FifoSlotInner>,
}

struct FifoSlotInner {
    pipe: Option<Arc<PipeObject>>,
    opens: usize,
}

impl FifoSlot {
    pub const fn new() -> Self {
        Self { inner: SpinLock::new(FifoSlotInner { pipe: None, opens: 0 }) }
    }

    /// Attach an open description to this FIFO's pipe, allocating a fresh
    /// `PipeObject::new_fifo` outside the spinlock if none is attached.
    fn attach(&self) -> Result<Arc<PipeObject>, u32> {
        {
            let mut slot = self.inner.lock();
            if let Some(pipe) = slot.pipe.as_ref().map(Arc::clone) {
                slot.opens += 1;
                return Ok(pipe);
            }
        }
        let fresh = PipeObject::new_fifo()?;
        let mut drop_extra = None;
        let attached = {
            let mut slot = self.inner.lock();
            slot.opens += 1;
            if let Some(existing) = slot.pipe.as_ref().map(Arc::clone) {
                drop_extra = Some(fresh);
                existing
            } else {
                slot.pipe = Some(Arc::clone(&fresh));
                fresh
            }
        };
        drop(drop_extra);
        Ok(attached)
    }

    /// Detach one open description; the last open reaching `0` clears the slot
    /// and drops its `PipeObject` outside the spinlock.
    fn detach(&self) {
        let detached = {
            let mut slot = self.inner.lock();
            if slot.opens == 0 {
                log::error!(target: "pipe", "FIFO slot open count underflow");
            }
            slot.opens = slot.opens.saturating_sub(1);
            if slot.opens == 0 { slot.pipe.take() } else { None }
        };
        drop(detached);
    }

    /// Current attached pipe, if any (for `Pollable` and ktests).
    pub fn pipe(&self) -> Option<Arc<PipeObject>> {
        self.inner.lock().pipe.as_ref().map(Arc::clone)
    }
}

impl Pollable for FifoSlot {
    fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        match self.pipe() {
            Some(pipe) => pipe.poll(mode, waker),
            // An inode-only poll (including an O_PATH inode reference) can
            // reach an unattached slot. No live ends or wake source exist,
            // so report terminal HUP|ERR. Descriptor polling rejects O_PATH
            // and otherwise uses KernelFile's attached pipe and generation.
            None => (PollMask::HUP | PollMask::ERR, None),
        }
    }
}

/// Look up the [`FifoSlot`] of a tmpfs or ext4 FIFO inode.
pub fn fifo_slot_of(inode: &dyn VfsInode) -> Option<&FifoSlot> {
    let any = inode.as_any()?;
    if let Some(t) = any.downcast_ref::<TmpfsInode>() {
        return t.fifo_slot();
    }
    if let Some(e) = any.downcast_ref::<Ext4Inode>() {
        return e.fifo_slot();
    }
    None
}

/// Ownership of one open pipe or FIFO end, released once when its description's
/// combined `end_refs` reaches zero (or an uninstalled open is dropped).
#[must_use = "an opened end must be attached or released"]
pub struct PipeEnd {
    writer_generation: u64,
    pipe: Arc<PipeObject>,
    reader: bool,
    writer: bool,
    fifo_inode: Option<Arc<dyn VfsInode>>,
}

impl PipeEnd {
    fn new(pipe: Arc<PipeObject>, reader: bool, writer: bool, fifo_inode: Option<Arc<dyn VfsInode>>) -> Self {
        Self { pipe, reader, writer, fifo_inode, writer_generation: 0 }
    }

    /// Zero except for a nonblocking FIFO reader opened with no writer,
    /// which snapshots w_opens. Otherwise w_opens is at least 1 by the time
    /// open returns, so zero makes a departed writer report HUP immediately.
    /// `PipePoll` copies this value to poll without taking its `end` lock.
    pub fn writer_generation(&self) -> u64 {
        self.writer_generation
    }

    /// Borrow the underlying [`PipeObject`]; `PipePoll` keeps an Arc copy so
    /// I/O and polling do not need its `end` ownership lock.
    pub fn pipe(&self) -> &Arc<PipeObject> {
        &self.pipe
    }

    /// Release this open end (and detach the FIFO slot if it was the last open).
    pub fn release(self) {
        self.pipe.release_end(self.reader, self.writer);
        if let Some(inode) = self.fifo_inode
            && let Some(slot) = fifo_slot_of(&*inode)
        {
            slot.detach();
        }
    }
}

/// RAII rollback guard while `open_fifo` is waiting for a peer.
struct FifoOpenGuard {
    pipe: Arc<PipeObject>,
    fifo_inode: Option<Arc<dyn VfsInode>>,
    opened_reader: bool,
    opened_writer: bool,
    committed: bool,
    writer_generation: u64,
}

impl FifoOpenGuard {
    fn commit(mut self) -> PipeEnd {
        self.committed = true;
        let mut end =
            PipeEnd::new(Arc::clone(&self.pipe), self.opened_reader, self.opened_writer, self.fifo_inode.take());
        end.writer_generation = self.writer_generation;
        end
    }
}

impl Drop for FifoOpenGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.opened_reader || self.opened_writer {
            self.pipe.release_end(self.opened_reader, self.opened_writer);
        }
        if let Some(inode) = self.fifo_inode.take()
            && let Some(slot) = fifo_slot_of(&*inode)
        {
            slot.detach();
        }
    }
}

/// Open a FIFO inode (or reopen an anonymous `PipeInode` via `/proc/<pid>/fd`)
/// according to Linux's `fifo_open` rules (ARCHITECTURE section 10).
///
/// Must be called after every VFS lock has been released.
pub fn open_fifo(inode: &Arc<dyn VfsInode>, mode: AccessMode, nonblock: bool) -> Result<PipeEnd, u32> {
    let (pipe, fifo_inode) = if let Some(slot) = fifo_slot_of(&**inode) {
        (slot.attach()?, Some(Arc::clone(inode)))
    } else if let Some(pipe_inode) = inode.as_any().and_then(|a| a.downcast_ref::<PipeInode>()) {
        (Arc::clone(&pipe_inode.pipe), None)
    } else {
        return Err(errno::ENXIO);
    };

    let is_pipe = fifo_inode.is_none();
    let mut guard = FifoOpenGuard {
        pipe,
        fifo_inode,
        opened_reader: false,
        opened_writer: false,
        committed: false,
        writer_generation: 0,
    };

    match mode {
        AccessMode::ReadOnly => {
            let wait_w = {
                let mut p = guard.pipe.pipe.lock();
                // Linux suppresses HUP only for a nonblocking FIFO reader
                // that has not yet seen a writer. Readers joining a writer
                // must observe HUP when that existing writer closes too.
                if !is_pipe && nonblock && p.writers() == 0 {
                    guard.writer_generation = guard.pipe.w_opens.load(Ordering::Acquire);
                }
                p.open_reader();
                guard.opened_reader = true;
                guard.pipe.r_opens.fetch_add(1, Ordering::Release);
                guard.pipe.write_seq.fetch_add(1, Ordering::Release);
                if is_pipe || nonblock || p.writers() > 0 {
                    None
                } else {
                    Some(guard.pipe.w_opens.load(Ordering::Acquire))
                }
            };
            guard.pipe.writers.wake_all();
            guard.pipe.poll.notify(PollMask::OUT | PollMask::WRNORM);
            if let Some(start_w) = wait_w {
                let wake = guard
                    .pipe
                    .readers
                    .wait_interruptible(None, || guard.pipe.w_opens.load(Ordering::Acquire) != start_w);
                if wake == sched::Wake::Interrupted {
                    return Err(ERESTARTSYS);
                }
            }
        }
        AccessMode::WriteOnly => {
            let wait_r = {
                let mut p = guard.pipe.pipe.lock();
                if !is_pipe && nonblock && p.readers() == 0 {
                    return Err(errno::ENXIO);
                }
                p.open_writer();
                guard.opened_writer = true;
                guard.pipe.w_opens.fetch_add(1, Ordering::Release);
                guard.pipe.read_seq.fetch_add(1, Ordering::Release);
                if is_pipe || nonblock || p.readers() > 0 {
                    None
                } else {
                    Some(guard.pipe.r_opens.load(Ordering::Acquire))
                }
            };
            guard.pipe.readers.wake_all();
            guard.pipe.poll.notify(PollMask::IN | PollMask::RDNORM);
            if let Some(start_r) = wait_r {
                let wake = guard
                    .pipe
                    .writers
                    .wait_interruptible(None, || guard.pipe.r_opens.load(Ordering::Acquire) != start_r);
                if wake == sched::Wake::Interrupted {
                    return Err(ERESTARTSYS);
                }
            }
        }
        AccessMode::ReadWrite => {
            {
                let mut p = guard.pipe.pipe.lock();
                p.open_reader();
                p.open_writer();
                guard.opened_reader = true;
                guard.opened_writer = true;
                guard.pipe.r_opens.fetch_add(1, Ordering::Release);
                guard.pipe.w_opens.fetch_add(1, Ordering::Release);
                guard.pipe.read_seq.fetch_add(1, Ordering::Release);
                guard.pipe.write_seq.fetch_add(1, Ordering::Release);
            }
            guard.pipe.readers.wake_all();
            guard.pipe.writers.wake_all();
            guard.pipe.poll.notify(PollMask::IN | PollMask::RDNORM | PollMask::OUT | PollMask::WRNORM);
        }
    }

    Ok(guard.commit())
}

struct PipeMeta {
    mode: u16,
    uid: u32,
    gid: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    locks: AdvisoryLocks,
}

/// Anonymous `pipefs` inode (dev `0:6`, `InodeType::Fifo`, never cached).
pub struct PipeInode {
    lock_id: u64,
    ino: Ino,
    pipe: Arc<PipeObject>,
    open_count: AtomicU32,
    meta: SleepMutex<PipeMeta>,
}

impl PipeInode {
    fn new(ino: Ino, pipe: Arc<PipeObject>) -> Self {
        let now = time::realtime_timespec();
        Self {
            lock_id: next_lock_id(),
            ino,
            pipe,
            open_count: AtomicU32::new(0),
            meta: SleepMutex::new(PipeMeta {
                mode: 0o600,
                uid: 0,
                gid: 0,
                atime: now,
                mtime: now,
                ctime: now,
                locks: AdvisoryLocks::new(),
            }),
        }
    }
}

impl VfsInode for PipeInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        DevId::PIPEFS
    }

    fn ino(&self) -> Ino {
        self.ino
    }

    fn inode_type(&self) -> InodeType {
        InodeType::Fifo
    }

    fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    fn inc_open_count(&self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
    }

    fn dec_open_count(&self) -> u32 {
        self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let meta = self.meta.lock();
        Ok(Metadata {
            dev: DevId::PIPEFS,
            ino: self.ino,
            inode_type: InodeType::Fifo,
            mode: meta.mode,
            nlink: 1,
            uid: meta.uid,
            gid: meta.gid,
            rdev: DevId::default(),
            size: 0,
            blksize: PIPE_BUF as u32,
            blocks: 0,
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
        })
    }

    fn lookup(&self, _name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::NotDir)
    }

    lumen_vfs::fixed_namespace_ops!();

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Inval)
    }

    fn readdir(&self, _cursor: u64, _out: &mut Vec<DirEntry>, _max_entries: usize) -> Result<(), VfsError> {
        Err(VfsError::NotDir)
    }

    fn truncate(&self, _size: u64) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        meta.mode = mode & 0o7777;
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        if let Some(u) = uid {
            meta.uid = u;
        }
        if let Some(g) = gid {
            meta.gid = g;
        }
        meta.mode = mode_after_chown(InodeType::Fifo, meta.mode);
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        let now = time::realtime_timespec();
        meta.atime = atime.resolve(meta.atime, now);
        meta.mtime = mtime.resolve(meta.mtime, now);
        meta.ctime = now;
        Ok(())
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        f(&mut meta.locks);
        Ok(())
    }

    fn pollable(&self) -> Option<&dyn Pollable> {
        Some(&*self.pipe as &dyn Pollable)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

/// Create a fresh anonymous pipe pair `(read_end, write_end)` on `pipefs`
/// (dev `0:6`, path `pipe:[ino]`).
pub fn new_pipe(nonblock: bool) -> Result<(KernelFile, KernelFile), u32> {
    let pipe = PipeObject::new_pipe()?;
    let ino = NEXT_PIPE_INO.fetch_add(1, Ordering::Relaxed);
    let inode: Arc<dyn VfsInode> = Arc::new(PipeInode::new(ino, Arc::clone(&pipe)));
    let path: Arc<[u8]> = Arc::from(format!("pipe:[{ino}]").into_bytes());
    let nb = if nonblock { general::O_NONBLOCK } else { 0 };
    let r_flags = OpenFlags::from_bits(general::O_RDONLY | nb);
    let w_flags = OpenFlags::from_bits(general::O_WRONLY | nb);

    let read_file = KernelFile::new_pipe_end(
        Arc::clone(&inode),
        Arc::clone(&path),
        r_flags,
        PipeEnd::new(Arc::clone(&pipe), true, false, None),
    )
    .map_err(VfsError::to_errno)?;
    let write_file = KernelFile::new_pipe_end(inode, path, w_flags, PipeEnd::new(pipe, false, true, None))
        .map_err(VfsError::to_errno)?;
    Ok((read_file, write_file))
}
