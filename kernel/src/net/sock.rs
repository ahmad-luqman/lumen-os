//! Generic socket file object, `sockfs` (dev `0:8`), `InFlightFile`, and
//! `block_on` (M9 B1, #472; ARCHITECTURE sections 8, 9 and 10).
//!
//! Every socket family implements [`SockOps`], attached once to a
//! [`KernelFile`] via `FileObj::Sock(Arc<dyn SockOps>)` and reached through
//! [`KernelFile::pollable`] and `KernelFile::release_end`. All socket
//! syscalls in `linux::syscall::socket` dispatch generically over [`SockOps`].

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_sockabi::{Family, OptVal, SockAddr, SockOpt, Ucred};
use lumen_vfs::{
    AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, PollMask, PollWaker, Pollable,
    SetTime, Timespec, VfsError, VfsInode, mode_after_chown, next_lock_id,
};

use crate::linux::process::{self, FileRef, KernelFile};
use crate::linux::signal::ERESTARTSYS;
use crate::linux::{errno, general, net};
use crate::sched;
use crate::sync::SleepMutex;
use crate::time;
use crate::user::{copy_from_user, copy_to_user_partial};

/// Device identifier for the internal `sockfs` (`0:8`, never mounted).
pub const SOCK_DEV: DevId = DevId::makedev(0, 8);

/// Maximum byte count processed in a single read/write/send/recv transfer
/// (Linux v6.12 `include/linux/fs.h` `MAX_RW_COUNT`).
pub const MAX_RW_COUNT: usize = 0x7fff_f000;

/// Fallback polling step when a buggy [`Pollable`] returns no [`PollGuard`]
/// in [`block_on`] (10 ms).
const NO_GUARD_FALLBACK_NS: u64 = 10_000_000;

/// Monotonic inode allocator for `sockfs` (`socket:[ino]`).
static NEXT_SOCK_INO: AtomicU64 = AtomicU64::new(1);

/// Socket transport kind (low bits of Linux `SOCK_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SockKind {
    Stream,
    Dgram,
    Seqpacket,
    Raw,
}

impl SockKind {
    /// Raw Linux `SOCK_*` constant (`SOCK_STREAM`, `SOCK_DGRAM`,
    /// `SOCK_SEQPACKET`, or `SOCK_RAW`).
    #[allow(dead_code)] // used by family `SO_TYPE` implementations (C1/C3/C4/C6)
    pub const fn to_raw(self) -> i32 {
        match self {
            Self::Stream => net::SOCK_STREAM as i32,
            Self::Dgram => net::SOCK_DGRAM as i32,
            Self::Seqpacket => net::SOCK_SEQPACKET as i32,
            Self::Raw => net::SOCK_RAW as i32,
        }
    }
}

/// Direction(s) passed to `shutdown(2)` (`SHUT_RD`, `SHUT_WR`, `SHUT_RDWR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShutdownHow {
    /// Disallow further receptions (`SHUT_RD` = 0).
    Read,
    /// Disallow further transmissions (`SHUT_WR` = 1).
    Write,
    /// Disallow both receptions and transmissions (`SHUT_RDWR` = 2).
    Both,
}

const _: () = {
    assert!(net::SHUT_RD == 0);
    assert!(net::SHUT_WR == 1);
    assert!(net::SHUT_RDWR == 2);
};

impl ShutdownHow {
    /// Decode a raw Linux `how` argument (`0..=2`), returning `None` for any
    /// other value (`-EINVAL`).
    pub const fn from_raw(how: i32) -> Option<Self> {
        match how {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Both),
            _ => None,
        }
    }
}

/// One validated user-memory span (`struct iovec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSpan {
    pub base: u64,
    pub len: usize,
}

/// Borrowed slice of user-memory spans passed to [`SockOps::send`] and
/// [`SockOps::recv`].
///
/// Families copy to and from user memory only while holding their own Lock 0
/// socket I/O lock (never under `NetStack::inner` or any spinlock).
#[derive(Debug, Clone, Copy)]
pub struct UserIovec<'a> {
    spans: &'a [UserSpan],
    total: usize,
}

impl<'a> UserIovec<'a> {
    /// Wrap a validated slice of spans, truncating total length to
    /// [`MAX_RW_COUNT`] (Linux v6.12 `lib/iov_iter.c` `import_iovec`).
    pub fn new(spans: &'a [UserSpan]) -> Self {
        let total = spans.iter().fold(0usize, |acc, s| acc.saturating_add(s.len)).min(MAX_RW_COUNT);
        Self { spans, total }
    }

    /// Underlying user spans.
    #[allow(dead_code)] // used by family implementations
    pub const fn spans(&self) -> &'a [UserSpan] {
        self.spans
    }

    /// Total byte length across all spans (capped at [`MAX_RW_COUNT`]).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by family implementations (C1/C3/C4/C6)
    pub const fn total_len(&self) -> usize {
        self.total
    }

    /// True if all spans together have zero bytes.
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Copy up to `dst.len()` bytes from user space starting at byte `offset`
    /// across the spans into `dst`.
    ///
    /// Returns the number of bytes copied (`min(dst.len(), total_len - offset)`),
    /// or `Err(EFAULT)` if any touched user address faults or overflows.
    #[allow(dead_code)] // used by family implementations and ktests
    pub fn copy_from_user(&self, mut offset: usize, dst: &mut [u8]) -> Result<usize, u32> {
        let limit = dst.len().min(self.total.saturating_sub(offset));
        if limit == 0 {
            return Ok(0);
        }
        let mut dst = &mut dst[..limit];
        let mut copied = 0usize;
        for span in self.spans {
            if offset >= span.len {
                offset -= span.len;
                continue;
            }
            let n = dst.len().min(span.len - offset);
            let addr = span.base.checked_add(offset as u64).ok_or(errno::EFAULT)?;
            copy_from_user(&mut dst[..n], addr).map_err(|_| errno::EFAULT)?;
            copied += n;
            dst = &mut dst[n..];
            if dst.is_empty() {
                break;
            }
            offset = 0;
        }
        Ok(copied)
    }

    /// Copy as much of `src` as fits into user space starting at byte `offset`
    /// across the spans, returning the exact prefix length written before end
    /// of vector or a page fault.
    #[allow(dead_code)] // used by family implementations and ktests
    pub fn copy_to_user_partial(&self, mut offset: usize, src: &[u8]) -> usize {
        let limit = src.len().min(self.total.saturating_sub(offset));
        if limit == 0 {
            return 0;
        }
        let mut src = &src[..limit];
        let mut copied = 0usize;
        for span in self.spans {
            if offset >= span.len {
                offset -= span.len;
                continue;
            }
            let n = src.len().min(span.len - offset);
            let Some(addr) = span.base.checked_add(offset as u64) else {
                return copied;
            };
            let moved = copy_to_user_partial(addr, &src[..n]);
            copied += moved;
            if moved < n {
                return copied;
            }
            src = &src[n..];
            if src.is_empty() {
                break;
            }
            offset = 0;
        }
        copied
    }

    /// Copy up to `src.len()` bytes into user space starting at byte `offset`,
    /// returning `Err(EFAULT)` if any byte fails to copy.
    #[allow(dead_code)] // used by family implementations and ktests
    pub fn copy_to_user(&self, offset: usize, src: &[u8]) -> Result<usize, u32> {
        let want = src.len().min(self.total.saturating_sub(offset));
        if want == 0 {
            return Ok(0);
        }
        let moved = self.copy_to_user_partial(offset, &src[..want]);
        if moved < want { Err(errno::EFAULT) } else { Ok(moved) }
    }
}

/// A descriptor in flight in `SCM_RIGHTS`: its end reference is counted
/// (`inc_fd_ref`) while it lives, and `Drop` runs `release_inflight_file`
/// (releasing `flock` and `end_refs` on the last reference, as Linux's final
/// `fput`), so it is dropped only outside every lock (ARCHITECTURE section 9's
/// end-release rule).
pub struct InFlightFile(ManuallyDrop<FileRef>);

impl InFlightFile {
    /// Resolve `fd` in the current process's FD table into an [`InFlightFile`],
    /// raising its counted FD/end reference under `Files::table`.
    pub fn from_fd(fd: i32) -> Result<Self, u32> {
        process::clone_current_fd_ref(fd).map(|f| Self(ManuallyDrop::new(f))).ok_or(errno::EBADF)
    }

    /// Wrap a freshly created, uninstalled [`KernelFile`] (whose initial
    /// `fd_refs` and `end_refs` are already 1) for installation into a
    /// `ReservedFd`.
    pub(crate) fn from_fresh(file: KernelFile) -> Self {
        Self(ManuallyDrop::new(Arc::new(file)))
    }

    /// Borrow the underlying open file description.
    #[allow(dead_code)] // used by C2 (`unix_gc`) and ktests
    pub fn file(&self) -> &KernelFile {
        &self.0
    }

    /// Move this in-flight object's counted FD/end reference out for
    /// installation via `ReservedFd::install_inflight` without touching any
    /// reference counter.
    pub(crate) fn into_file_ref(self) -> FileRef {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: `this` is wrapped in `ManuallyDrop`, so `InFlightFile::drop`
        // will not run; ownership of the counted `FileRef` moves to the caller.
        unsafe { ManuallyDrop::take(&mut this.0) }
    }
}

impl Clone for InFlightFile {
    fn clone(&self) -> Self {
        self.0.inc_fd_ref();
        Self(ManuallyDrop::new(Arc::clone(&self.0)))
    }
}

// Dropping runs `release_inflight_file`, which may release `flock` (Lock 4)
// and trigger `release_end` (Lock 00/0): never drop an `InFlightFile` while
// holding any lock 00–10.
impl Drop for InFlightFile {
    fn drop(&mut self) {
        // SAFETY: `self.0` is never read again after `Drop` (`into_file_ref`
        // suppresses `Drop` via `ManuallyDrop::new(self)`).
        let file = unsafe { ManuallyDrop::take(&mut self.0) };
        process::release_inflight_file(file);
    }
}

/// One send's user side, decoded by the handler: spans checked
/// (`UIO_MAXIOV`), any non-null destination copied and length-checked
/// (`named == true` when `msg_namelen != 0`; decoded into `to` for datagram
/// and raw sockets only), and the control buffer (at most 131072 bytes,
/// Linux's `optmem_max` since 6.8; `-ENOBUFS` past it) walked with `CmsgIter`:
/// `SCM_RIGHTS` resolved to `InFlightFile`s, `SCM_CREDENTIALS` decoded,
/// messages at levels other than `SOL_SOCKET` skipped, an unknown `SOL_SOCKET`
/// type `-EINVAL`.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by family SockOps implementations (C1/C3/C4/C6)
pub struct SendIo<'a> {
    pub iov: UserIovec<'a>,
    pub to: Option<SockAddr>,
    pub named: bool,
    pub rights: Vec<InFlightFile>,
    pub creds: Option<Ucred>,
    pub flags: u32,
    pub nonblock: bool,
}

/// One receive's user side: spans and flags.
///
/// Families return every descriptor carried by the consumed message in
/// [`RecvDone::rights`] (cloning under `MSG_PEEK`); the handler installs what
/// fits into `msg_control` and `RLIMIT_NOFILE`, sets `MSG_CTRUNC` if
/// truncated, and drops the rest outside every lock (`scm_detach_fds`).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by family SockOps implementations (C1/C3/C4/C6)
pub struct RecvIo<'a> {
    pub iov: UserIovec<'a>,
    pub flags: u32,
    pub nonblock: bool,
}

/// Result of [`SockOps::recv`]: the handler installs `rights`, encodes them
/// and `creds` with `CmsgWriter`, and copies the control buffer out.
pub struct RecvDone {
    pub bytes: usize,
    pub msg_flags: u32,
    pub from: Option<SockAddr>,
    pub rights: Vec<InFlightFile>,
    pub creds: Option<Ucred>,
}

/// Generic socket operations trait shared by every socket family
/// (`AF_UNIX`, UDP, TCP, ICMP; ARCHITECTURE section 8).
///
/// Rules for every implementor:
/// - Errors are positive Linux errnos.
/// - User copies through `SendIo::iov` / `RecvIo::iov` happen only under the
///   socket's own Lock 0 I/O lock, never under `NetStack::inner` or a spinlock,
///   and that lock is released around [`block_on`].
/// - Readiness changes notify after the family's locks are released.
/// - No family touches an FD table.
pub trait SockOps: Pollable + Send + Sync {
    fn family(&self) -> Family;
    fn kind(&self) -> SockKind;
    fn bind(&self, addr: &SockAddr) -> Result<(), u32>;
    fn listen(&self, backlog: i32) -> Result<(), u32>;
    fn accept(&self, nonblock: bool) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32>;
    fn connect(&self, addr: &SockAddr, nonblock: bool) -> Result<(), u32>;
    /// Handle `connect(fd, { sa_family: AF_UNSPEC }, >= 2)` per Linux v6.12:
    /// - `AF_UNIX` stream/seqpacket: `-EINVAL` (`unix_validate_addr`);
    /// - TCP: disconnect and return `0` (`__inet_stream_connect` -> `prot->disconnect`;
    ///   note for C4: `lumen-net`'s `Stack::disconnect` is UDP-only, so TCP's
    ///   `AF_UNSPEC` needs an abort-style path);
    /// - UDP, raw, ping and `AF_UNIX` datagram: disconnect and return `0`.
    fn disconnect(&self) -> Result<(), u32>;
    /// Takes what it queues from `io.rights`; what is left goes back to the
    /// handler, which drops it after the call.
    fn send(&self, io: &mut SendIo<'_>) -> Result<usize, u32>;
    fn recv(&self, io: &mut RecvIo<'_>) -> Result<RecvDone, u32>;
    fn shutdown(&self, how: ShutdownHow) -> Result<(), u32>;
    fn local_name(&self) -> Result<SockAddr, u32>;
    fn peer_name(&self) -> Result<SockAddr, u32>;
    fn set_opt(&self, opt: SockOpt, val: &OptVal) -> Result<(), u32>;
    fn get_opt(&self, opt: SockOpt) -> Result<OptVal, u32>;
    fn fionread(&self) -> Result<usize, u32>;
    /// The description's last end reference (ARCHITECTURE section 10).
    fn release(&self);
}

/// Per-wait [`PollWaker`] for [`block_on`]: sets `fired` and wakes its calling
/// thread's per-call `WaitQueue`. Never allocates or blocks in `wake`.
struct SockPollWait {
    fired: AtomicBool,
    queue: sched::WaitQueue,
}

impl SockPollWait {
    fn new() -> Arc<Self> {
        Arc::new(Self { fired: AtomicBool::new(false), queue: sched::WaitQueue::new() })
    }
}

impl PollWaker for SockPollWait {
    fn wake(&self, _ready: PollMask) {
        self.fired.store(true, Ordering::SeqCst);
        let _ = self.queue.wake_one();
    }
}

#[cfg(feature = "ktest")]
mod block_on_window {
    use crate::sync::SpinLock;
    use alloc::boxed::Box;

    static HOOK: SpinLock<Option<Box<dyn FnOnce() + Send>>> = SpinLock::new(None);

    pub struct ArmGuard;

    pub fn arm(f: impl FnOnce() + Send + 'static) -> ArmGuard {
        *HOOK.lock() = Some(Box::new(f));
        ArmGuard
    }

    impl Drop for ArmGuard {
        fn drop(&mut self) {
            *HOOK.lock() = None;
        }
    }

    pub fn fire_if_armed() {
        let hook = HOOK.lock().take();
        if let Some(f) = hook {
            f();
        }
    }
}

/// The one blocking socket wait: subscribes a [`PollWaker`] to `src`'s
/// `PollHead` and sleeps until `ready()` holds or the absolute monotonic
/// `deadline` (from `SO_RCVTIMEO`/`SO_SNDTIMEO`; `None`: forever) passes.
///
/// Follows ARCHITECTURE section 8, "Signals and sockets":
/// - Signal with a timeout (`deadline.is_some()`): `Err(EINTR)`.
/// - Signal without a timeout (`deadline.is_none()`): `Err(ERESTARTSYS)`.
/// - Timeout elapsed without becoming ready: `Err(EAGAIN)`.
#[allow(dead_code)] // used by family implementations (C1, C3, C4, C6) and ktests
pub fn block_on(src: &dyn Pollable, deadline: Option<u64>, mut ready: impl FnMut() -> bool) -> Result<(), u32> {
    if ready() {
        return Ok(());
    }
    if let Some(dl) = deadline
        && time::now_ns() >= dl
    {
        return Err(errno::EAGAIN);
    }

    let waiter = SockPollWait::new();
    let waker: Arc<dyn PollWaker> = waiter.clone();
    // Subscribe before re-checking `ready()` so no wakeup between the check
    // and the sleep can be missed; `guard` stays held until `block_on` exits.
    let (_mask, guard) = src.poll(AccessMode::ReadWrite, Some(&waker));
    debug_assert!(guard.is_some(), "block_on source returned no PollGuard");

    loop {
        // Clear `fired` BEFORE checking `ready()` so any notification arriving
        // during or after `ready()` sets `fired` again and wakes the wait.
        waiter.fired.store(false, Ordering::SeqCst);
        if ready() {
            return Ok(());
        }
        #[cfg(feature = "ktest")]
        block_on_window::fire_if_armed();
        let now = time::now_ns();
        if let Some(dl) = deadline
            && now >= dl
        {
            return Err(errno::EAGAIN);
        }
        let step = if guard.is_none() {
            let cap = now.saturating_add(NO_GUARD_FALLBACK_NS);
            Some(deadline.map_or(cap, |dl| dl.min(cap)))
        } else {
            deadline
        };
        match waiter.queue.wait_interruptible(step, || waiter.fired.load(Ordering::SeqCst)) {
            sched::Wake::Woken => {}
            sched::Wake::TimedOut => {
                if ready() {
                    return Ok(());
                }
                if deadline.is_some_and(|dl| time::now_ns() >= dl) {
                    return Err(errno::EAGAIN);
                }
            }
            sched::Wake::Interrupted => {
                return Err(if deadline.is_some() { errno::EINTR } else { ERESTARTSYS });
            }
        }
    }
}

struct SockMeta {
    mode: u16,
    uid: u32,
    gid: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    locks: AdvisoryLocks,
}

/// Unmounted `sockfs` inode (dev `0:8`, `InodeType::Socket`, never cached).
pub struct SockInode {
    lock_id: u64,
    ino: Ino,
    open_count: AtomicU32,
    meta: SleepMutex<SockMeta>,
}

impl SockInode {
    fn new(ino: Ino) -> Self {
        let now = time::realtime_timespec();
        Self {
            lock_id: next_lock_id(),
            ino,
            open_count: AtomicU32::new(0),
            meta: SleepMutex::new(SockMeta {
                mode: 0o777,
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

impl VfsInode for SockInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        SOCK_DEV
    }

    fn ino(&self) -> Ino {
        self.ino
    }

    fn inode_type(&self) -> InodeType {
        InodeType::Socket
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
            dev: SOCK_DEV,
            ino: self.ino,
            inode_type: InodeType::Socket,
            mode: meta.mode,
            nlink: 1,
            uid: meta.uid,
            gid: meta.gid,
            rdev: DevId::default(),
            size: 0,
            blksize: 4096,
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
        meta.mode = mode_after_chown(InodeType::Socket, meta.mode);
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

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

/// Allocate a fresh `sockfs` open file description (`dev 0:8`, path
/// `socket:[ino]`, `O_RDWR | O_NONBLOCK?`) backed by `ops`. Calls
/// `ops.release()` if constructing the description fails (matching Linux v6.12
/// `sock_alloc_file`).
pub fn new_socket_file(ops: Arc<dyn SockOps>, nonblock: bool) -> Result<KernelFile, u32> {
    let ino = NEXT_SOCK_INO.fetch_add(1, Ordering::Relaxed);
    let inode: Arc<dyn VfsInode> = Arc::new(SockInode::new(ino));
    let path: Arc<[u8]> = Arc::from(format!("socket:[{ino}]").into_bytes());
    let nb = if nonblock { general::O_NONBLOCK } else { 0 };
    let flags = OpenFlags::from_bits(general::O_RDWR | nb);
    match KernelFile::new_sock(inode, path, flags, Arc::clone(&ops)) {
        Ok(file) => Ok(file),
        Err(e) => {
            ops.release();
            Err(e.to_errno())
        }
    }
}

/// A connected pair of sockets created by [`create_socketpair`].
pub type SockPair = (Arc<dyn SockOps>, Arc<dyn SockOps>);

#[cfg(feature = "ktest")]
mod ktest_factory {
    use super::*;
    use crate::sync::SpinLock;

    pub type SingleFactory = fn(Family, SockKind, i32) -> Result<Arc<dyn SockOps>, u32>;
    pub type PairFactory = fn(Family, SockKind, i32) -> Result<SockPair, u32>;

    static SINGLE: SpinLock<Option<SingleFactory>> = SpinLock::new(None);
    static PAIR: SpinLock<Option<PairFactory>> = SpinLock::new(None);

    pub struct FactoryGuard {
        prev_single: Option<SingleFactory>,
        prev_pair: Option<PairFactory>,
    }

    impl FactoryGuard {
        pub fn install(single: Option<SingleFactory>, pair: Option<PairFactory>) -> Self {
            let prev_single = core::mem::replace(&mut *SINGLE.lock(), single);
            let prev_pair = core::mem::replace(&mut *PAIR.lock(), pair);
            Self { prev_single, prev_pair }
        }
    }

    impl Drop for FactoryGuard {
        fn drop(&mut self) {
            *SINGLE.lock() = self.prev_single;
            *PAIR.lock() = self.prev_pair;
        }
    }

    pub fn try_single(family: Family, kind: SockKind, protocol: i32) -> Option<Result<Arc<dyn SockOps>, u32>> {
        let f = *SINGLE.lock();
        f.map(|f| f(family, kind, protocol))
    }

    pub fn try_pair(family: Family, kind: SockKind, protocol: i32) -> Option<Result<SockPair, u32>> {
        let f = *PAIR.lock();
        f.map(|f| f(family, kind, protocol))
    }
}

/// Create a socket of validated `(family, kind, protocol)`.
///
/// Families (C1 `AF_UNIX`, C3 UDP, C4 TCP, C6 ICMP) plug their constructors
/// in here. Until a family lands, it returns `-EAFNOSUPPORT`.
pub fn create_socket(family: Family, kind: SockKind, protocol: i32) -> Result<Arc<dyn SockOps>, u32> {
    #[cfg(feature = "ktest")]
    if let Some(res) = ktest_factory::try_single(family, kind, protocol) {
        return res;
    }
    let _ = (family, kind, protocol);
    Err(errno::EAFNOSUPPORT)
}

/// Create a connected socket pair of validated `(family, kind, protocol)`.
///
/// Only `AF_UNIX` supports `socketpair` (C1 plugs its constructor in here).
/// Until C1 lands, it returns `-EAFNOSUPPORT`.
pub fn create_socketpair(family: Family, kind: SockKind, protocol: i32) -> Result<SockPair, u32> {
    #[cfg(feature = "ktest")]
    if let Some(res) = ktest_factory::try_pair(family, kind, protocol) {
        return res;
    }
    let _ = (family, kind, protocol);
    Err(errno::EAFNOSUPPORT)
}

#[cfg(feature = "ktest")]
mod ktest_support {
    use super::*;
    use alloc::collections::VecDeque;
    use core::sync::atomic::AtomicUsize;

    use lumen_vfs::PollGuard;

    use crate::arch::syscall::SyscallFrame;
    use crate::linux::syscall::dispatch;
    use crate::sync::SpinLock;
    use crate::vfs::KernelPollHead;

    pub fn syscall(nr: u32, a: [u64; 6]) -> u64 {
        let mut frame = SyscallFrame {
            rax: u64::from(nr),
            rdi: a[0],
            rsi: a[1],
            rdx: a[2],
            r10: a[3],
            r8: a[4],
            r9: a[5],
            ..SyscallFrame::default()
        };
        let _ = dispatch(&mut frame);
        frame.rax
    }

    pub const fn neg(code: u32) -> u64 {
        -(code as i64) as u64
    }

    pub struct MockState {
        pub buf: VecDeque<u8>,
        pub queued_rights: Vec<InFlightFile>,
        pub queued_creds: Option<Ucred>,
        pub queued_from: Option<SockAddr>,
        pub pending_accept: Option<(Arc<dyn SockOps>, Option<SockAddr>)>,
        pub local: Option<SockAddr>,
        pub peer: Option<SockAddr>,
        pub mask: PollMask,
        pub last_bind: Option<SockAddr>,
        pub last_listen: Option<i32>,
        pub last_connect: Option<(SockAddr, bool)>,
        pub disconnects: usize,
        pub last_send_flags: Option<u32>,
        pub last_send_nonblock: Option<bool>,
        pub last_send_to: Option<SockAddr>,
        pub last_send_named: Option<bool>,
        pub last_send_creds: Option<Ucred>,
        pub fail_next_send: Option<u32>,
        pub last_recv_flags: Option<u32>,
        pub last_recv_nonblock: Option<bool>,
        pub last_shutdown: Option<ShutdownHow>,
        pub last_opt_set: Option<(SockOpt, OptVal)>,
        pub opt_reuseaddr: i32,
        pub opt_linger: lumen_sockabi::Linger,
        pub opt_rcvtimeo: lumen_sockabi::Timeout,
        pub send_calls: usize,
        pub recv_calls: usize,
        pub accept_calls: usize,
        pub fionread_calls: usize,
    }

    impl MockState {
        fn new() -> Self {
            Self {
                buf: VecDeque::new(),
                queued_rights: Vec::new(),
                queued_creds: None,
                queued_from: None,
                pending_accept: None,
                local: None,
                peer: None,
                mask: PollMask::OUT | PollMask::WRNORM,
                last_bind: None,
                last_listen: None,
                last_connect: None,
                disconnects: 0,
                last_send_flags: None,
                last_send_nonblock: None,
                last_send_to: None,
                last_send_named: None,
                last_send_creds: None,
                fail_next_send: None,
                last_recv_flags: None,
                last_recv_nonblock: None,
                last_shutdown: None,
                last_opt_set: None,
                opt_reuseaddr: 0,
                opt_linger: lumen_sockabi::Linger::default(),
                opt_rcvtimeo: lumen_sockabi::Timeout::Infinite,
                send_calls: 0,
                recv_calls: 0,
                accept_calls: 0,
                fionread_calls: 0,
            }
        }
    }

    pub struct MockSock {
        family: Family,
        kind: SockKind,
        protocol: i32,
        pub poll_head: KernelPollHead,
        pub releases: AtomicUsize,
        pub state: SpinLock<MockState>,
    }

    impl MockSock {
        pub fn new(family: Family, kind: SockKind, protocol: i32) -> Arc<Self> {
            Arc::new(Self {
                family,
                kind,
                protocol,
                poll_head: KernelPollHead::new(),
                releases: AtomicUsize::new(0),
                state: SpinLock::new(MockState::new()),
            })
        }

        pub fn set_mask_and_notify(&self, mask: PollMask) {
            self.state.lock().mask = mask;
            self.poll_head.notify(mask);
        }
    }

    impl Pollable for MockSock {
        fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
            let guard = waker.map(|w| self.poll_head.subscribe(Arc::clone(w)));
            let mask = self.state.lock().mask;
            (mask, guard)
        }
    }

    impl SockOps for MockSock {
        fn family(&self) -> Family {
            self.family
        }

        fn kind(&self) -> SockKind {
            self.kind
        }

        fn bind(&self, addr: &SockAddr) -> Result<(), u32> {
            let mut st = self.state.lock();
            st.last_bind = Some(*addr);
            st.local = Some(*addr);
            Ok(())
        }

        fn listen(&self, backlog: i32) -> Result<(), u32> {
            self.state.lock().last_listen = Some(backlog);
            Ok(())
        }

        fn accept(&self, _nonblock: bool) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32> {
            let mut st = self.state.lock();
            st.accept_calls += 1;
            st.pending_accept.take().ok_or(errno::EAGAIN)
        }

        fn connect(&self, addr: &SockAddr, nonblock: bool) -> Result<(), u32> {
            let mut st = self.state.lock();
            st.last_connect = Some((*addr, nonblock));
            st.peer = Some(*addr);
            Ok(())
        }

        fn disconnect(&self) -> Result<(), u32> {
            let mut st = self.state.lock();
            st.disconnects += 1;
            st.peer = None;
            Ok(())
        }

        fn send(&self, io: &mut SendIo<'_>) -> Result<usize, u32> {
            // Copy from user space outside the spinlock (SockOps contract).
            let total = io.iov.total_len();
            let mut data = alloc::vec![0u8; total];
            let n = io.iov.copy_from_user(0, &mut data)?;
            data.truncate(n);

            let old_rights = {
                let mut st = self.state.lock();
                st.send_calls += 1;
                st.last_send_flags = Some(io.flags);
                st.last_send_nonblock = Some(io.nonblock);
                st.last_send_to = io.to;
                st.last_send_named = Some(io.named);
                st.last_send_creds = io.creds;
                if let Some(err) = st.fail_next_send.take() {
                    return Err(err);
                }
                st.buf.extend(&data);
                if io.creds.is_some() {
                    st.queued_creds = io.creds;
                }
                if !io.rights.is_empty() {
                    let prev = core::mem::take(&mut st.queued_rights);
                    st.queued_rights = core::mem::take(&mut io.rights);
                    prev
                } else {
                    Vec::new()
                }
            };
            // Drop any displaced rights outside the spinlock.
            drop(old_rights);
            Ok(n)
        }

        fn recv(&self, io: &mut RecvIo<'_>) -> Result<RecvDone, u32> {
            let want = io.iov.total_len();
            let (chunk, rights, creds, from) = {
                let mut st = self.state.lock();
                st.recv_calls += 1;
                st.last_recv_flags = Some(io.flags);
                st.last_recv_nonblock = Some(io.nonblock);
                let take = want.min(st.buf.len());
                let chunk: Vec<u8> = st.buf.drain(..take).collect();
                let rights = core::mem::take(&mut st.queued_rights);
                let creds = st.queued_creds.take();
                let from = st.queued_from.or(st.peer);
                (chunk, rights, creds, from)
            };
            // Copy to user space outside the spinlock (SockOps contract).
            let copied = io.iov.copy_to_user(0, &chunk)?;
            Ok(RecvDone { bytes: copied, msg_flags: 0, from, rights, creds })
        }

        fn shutdown(&self, how: ShutdownHow) -> Result<(), u32> {
            self.state.lock().last_shutdown = Some(how);
            Ok(())
        }

        fn local_name(&self) -> Result<SockAddr, u32> {
            self.state.lock().local.ok_or(errno::EINVAL)
        }

        fn peer_name(&self) -> Result<SockAddr, u32> {
            self.state.lock().peer.ok_or(errno::ENOTCONN)
        }

        fn set_opt(&self, opt: SockOpt, val: &OptVal) -> Result<(), u32> {
            let mut st = self.state.lock();
            st.last_opt_set = Some((opt, *val));
            match (opt, *val) {
                (SockOpt::ReuseAddr, OptVal::Int(v)) => st.opt_reuseaddr = i32::from(v != 0),
                (SockOpt::Linger, OptVal::Linger(l)) => st.opt_linger = l,
                (SockOpt::RcvTimeo, OptVal::Timeout(t)) => st.opt_rcvtimeo = t,
                _ => return Err(errno::ENOPROTOOPT),
            }
            Ok(())
        }

        fn get_opt(&self, opt: SockOpt) -> Result<OptVal, u32> {
            let st = self.state.lock();
            match opt {
                SockOpt::ReuseAddr => Ok(OptVal::Int(st.opt_reuseaddr)),
                SockOpt::Linger => Ok(OptVal::Linger(st.opt_linger)),
                SockOpt::RcvTimeo => Ok(OptVal::Timeout(st.opt_rcvtimeo)),
                SockOpt::Type => Ok(OptVal::Int(self.kind.to_raw())),
                SockOpt::Domain => Ok(OptVal::Int(i32::from(self.family.to_raw()))),
                SockOpt::Protocol => Ok(OptVal::Int(self.protocol)),
                _ => Err(errno::ENOPROTOOPT),
            }
        }

        fn fionread(&self) -> Result<usize, u32> {
            let mut st = self.state.lock();
            st.fionread_calls += 1;
            Ok(st.buf.len())
        }

        fn release(&self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub static FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);
    pub static FACTORY_RELEASES: AtomicUsize = AtomicUsize::new(0);
    pub static LAST_CREATED: SpinLock<Option<(Family, SockKind, i32)>> = SpinLock::new(None);

    pub struct CountingSock {
        family: Family,
        kind: SockKind,
        poll_head: KernelPollHead,
    }

    impl Pollable for CountingSock {
        fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
            (PollMask::EMPTY, waker.map(|w| self.poll_head.subscribe(Arc::clone(w))))
        }
    }

    impl SockOps for CountingSock {
        fn family(&self) -> Family {
            self.family
        }
        fn kind(&self) -> SockKind {
            self.kind
        }
        fn bind(&self, _: &SockAddr) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn listen(&self, _: i32) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn accept(&self, _: bool) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn connect(&self, _: &SockAddr, _: bool) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn disconnect(&self) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn send(&self, _: &mut SendIo<'_>) -> Result<usize, u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn recv(&self, _: &mut RecvIo<'_>) -> Result<RecvDone, u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn shutdown(&self, _: ShutdownHow) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn local_name(&self) -> Result<SockAddr, u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn peer_name(&self) -> Result<SockAddr, u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn set_opt(&self, _: SockOpt, _: &OptVal) -> Result<(), u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn get_opt(&self, _: SockOpt) -> Result<OptVal, u32> {
            Err(errno::EOPNOTSUPP)
        }
        fn fionread(&self) -> Result<usize, u32> {
            Ok(0)
        }
        fn release(&self) {
            FACTORY_RELEASES.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn counting_single(family: Family, kind: SockKind, protocol: i32) -> Result<Arc<dyn SockOps>, u32> {
        FACTORY_CALLS.fetch_add(1, Ordering::SeqCst);
        *LAST_CREATED.lock() = Some((family, kind, protocol));
        Ok(Arc::new(CountingSock { family, kind, poll_head: KernelPollHead::new() }))
    }

    pub fn counting_pair(family: Family, kind: SockKind, protocol: i32) -> Result<SockPair, u32> {
        FACTORY_CALLS.fetch_add(1, Ordering::SeqCst);
        *LAST_CREATED.lock() = Some((family, kind, protocol));
        Ok((
            Arc::new(CountingSock { family, kind, poll_head: KernelPollHead::new() }),
            Arc::new(CountingSock { family, kind, poll_head: KernelPollHead::new() }),
        ))
    }
}

crate::ktest!(
    fn socket_and_socketpair_dispatch_by_family() {
        use core::mem::{offset_of, size_of};
        use core::sync::atomic::Ordering;

        use crate::linux::process::{Rlimit, RlimitKind};
        use crate::linux::{errno, general, ioctl, net};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use ktest_support::{
            FACTORY_CALLS, FACTORY_RELEASES, LAST_CREATED, counting_pair, counting_single, neg, syscall,
        };

        process::in_test_process(|pid| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let sv_va = base + 64;
            let stat_va = base + 128;
            let path_va = base + 512;
            let link_va = base + 768;
            let int_va = base + 1024;
            let sock_nonblock = u64::from(general::O_NONBLOCK);
            let sock_cloexec = u64::from(general::O_CLOEXEC);

            // 1. Unsupported families -> -EAFNOSUPPORT; socketpair on inet -> -EOPNOTSUPP.
            for bad_dom in [
                u64::from(net::AF_UNSPEC),
                u64::from(net::AF_NETLINK),
                u64::from(net::AF_PACKET),
                99u64,
                (-1i32) as u32 as u64,
            ] {
                kassert_eq!(
                    syscall(general::__NR_socket, [bad_dom, u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                    neg(errno::EAFNOSUPPORT)
                );
                kassert_eq!(
                    syscall(general::__NR_socketpair, [bad_dom, u64::from(net::SOCK_STREAM), 0, sv_va, 0, 0]),
                    neg(errno::EAFNOSUPPORT)
                );
            }
            for inet_dom in [u64::from(net::AF_INET), u64::from(net::AF_INET6)] {
                kassert_eq!(
                    syscall(general::__NR_socketpair, [inet_dom, u64::from(net::SOCK_STREAM), 0, sv_va, 0, 0]),
                    neg(errno::EOPNOTSUPP)
                );
            }

            // 2. Linux v6.12 `__sock_create` (net/socket.c:1515-1564) and
            //    `unix_create` (net/unix/af_unix.c:1068-1085) / `inet_create`
            //    (net/ipv4/af_inet.c:263-289) errno order:
            //    - bad type flags -> -EINVAL (net/socket.c:1665);
            //    - family < 0 || family >= NPROTO (46) -> -EAFNOSUPPORT before type >= SOCK_MAX (11);
            //    - type >= SOCK_MAX (11) -> -EINVAL before unknown registered family (-EAFNOSUPPORT);
            //    - SOCK_PACKET (10) < SOCK_MAX (11): AF_NETLINK -> -EAFNOSUPPORT, AF_UNIX -> -ESOCKTNOSUPPORT,
            //      (AF_INET, SOCK_PACKET) -> -EAFNOSUPPORT (net/socket.c:1525);
            //    - AF_UNIX checks protocol before type (af_unix.c:1068 before 1076).
            for bad_flags in [0x100u64, 0x400, 0x100_000, sock_nonblock | 0x20] {
                kassert_eq!(
                    syscall(
                        general::__NR_socket,
                        [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM) | bad_flags, 0, 0, 0, 0]
                    ),
                    neg(errno::EINVAL)
                );
                kassert_eq!(
                    syscall(
                        general::__NR_socketpair,
                        [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM) | bad_flags, 0, sv_va, 0, 0]
                    ),
                    neg(errno::EINVAL)
                );
            }
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_NETLINK), 11, 0, 0, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_NETLINK), 10, 0, 0, 0, 0]),
                neg(errno::EAFNOSUPPORT)
            );
            kassert_eq!(syscall(general::__NR_socket, [46, 11, 0, 0, 0, 0]), neg(errno::EAFNOSUPPORT));
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET), 10, 0, 0, 0, 0]),
                neg(errno::EAFNOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 10, 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 11, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 15, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 4, 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            // Protocol checked before type in `unix_create` (af_unix.c:1068): bad protocol wins over bad type 4.
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 4, 99, 0, 0, 0]),
                neg(errno::EPROTONOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET), u64::from(net::SOCK_SEQPACKET), 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET6), u64::from(net::SOCK_SEQPACKET), 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socket,
                    [u64::from(net::AF_INET), u64::from(net::SOCK_STREAM), net::IPPROTO_UDP as u64, 0, 0, 0]
                ),
                neg(errno::EPROTONOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET), u64::from(net::SOCK_RAW), 0, 0, 0, 0]),
                neg(errno::EPROTONOSUPPORT)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socket,
                    [u64::from(net::AF_INET6), u64::from(net::SOCK_RAW), net::IPPROTO_ICMP as u64, 0, 0, 0]
                ),
                neg(errno::EPROTONOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET), u64::from(net::SOCK_STREAM), 256, 0, 0, 0]),
                neg(errno::EINVAL)
            );
            // Linux v6.12 `__sys_socketpair` (net/socket.c:1779 before 1796): type/protocol errors before inet -EOPNOTSUPP.
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_INET), u64::from(net::SOCK_SEQPACKET), 0, sv_va, 0, 0]
                ),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_INET), u64::from(net::SOCK_STREAM), net::IPPROTO_UDP as u64, sv_va, 0, 0]
                ),
                neg(errno::EPROTONOSUPPORT)
            );

            // Without a family implementation registered yet, supported families answer -EAFNOSUPPORT.
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                neg(errno::EAFNOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET), u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                neg(errno::EAFNOSUPPORT)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, sv_va, 0, 0]
                ),
                neg(errno::EAFNOSUPPORT)
            );

            // 3. Install ktest factory to verify slot reservation, flags, fstat S_IFSOCK,
            //    /proc/<pid>/fd socket:[ino] + reopen -ENXIO, FIONBIO, fcntl, lseek, mmap, and end release.
            FACTORY_CALLS.store(0, Ordering::SeqCst);
            FACTORY_RELEASES.store(0, Ordering::SeqCst);
            let _factory = ktest_factory::FactoryGuard::install(Some(counting_single), Some(counting_pair));

            // Linux v6.12 `unix_create` (net/unix/af_unix.c:1068): protocol == PF_UNIX (1) is accepted.
            let unix_p1 = syscall(
                general::__NR_socket,
                [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), u64::from(net::AF_UNIX), 0, 0, 0],
            );
            kassert!((unix_p1 as i64) >= 3);
            kassert_eq!(*LAST_CREATED.lock(), Some((Family::Unix, SockKind::Stream, 1)));
            kassert_eq!(syscall(general::__NR_close, [unix_p1, 0, 0, 0, 0, 0]), 0);

            let fd = syscall(
                general::__NR_socket,
                [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM) | sock_nonblock | sock_cloexec, 0, 0, 0, 0],
            );
            kassert!((fd as i64) >= 3);
            kassert_eq!(*LAST_CREATED.lock(), Some((Family::Unix, SockKind::Stream, 0)));

            // SOCK_CLOEXEC and SOCK_NONBLOCK applied to descriptor and open description.
            kassert_eq!(
                syscall(general::__NR_fcntl, [fd, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                u64::from(general::FD_CLOEXEC)
            );
            let fl = syscall(general::__NR_fcntl, [fd, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
            kassert_eq!(fl & general::O_ACCMODE, general::O_RDWR);
            kassert!(fl & general::O_NONBLOCK != 0);

            // FIONBIO clears O_NONBLOCK; fcntl F_SETFL restores it.
            copy_to_user(int_va, &0i32.to_le_bytes()).map_err(|_| "copy 0")?;
            kassert_eq!(syscall(general::__NR_ioctl, [fd, u64::from(ioctl::FIONBIO), int_va, 0, 0, 0]), 0);
            let fl2 = syscall(general::__NR_fcntl, [fd, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
            kassert_eq!(fl2 & general::O_NONBLOCK, 0);
            kassert_eq!(syscall(general::__NR_fcntl, [fd, u64::from(general::F_SETFL), sock_nonblock, 0, 0, 0]), 0);
            let fl3 = syscall(general::__NR_fcntl, [fd, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
            kassert!(fl3 & general::O_NONBLOCK != 0);

            // fstat reports sockfs dev 0:8, S_IFSOCK, and non-zero unique st_ino.
            kassert_eq!(syscall(general::__NR_fstat, [fd, stat_va, 0, 0, 0, 0]), 0);
            let mut raw_stat = [0u8; size_of::<general::stat>()];
            copy_from_user(&mut raw_stat, stat_va).map_err(|_| "copy stat")?;
            let le_u64 = |off: usize| u64::from_le_bytes(raw_stat[off..off + 8].try_into().expect("8 bytes"));
            let st_dev = le_u64(offset_of!(general::stat, st_dev));
            let st_ino = le_u64(offset_of!(general::stat, st_ino));
            let st_mode = le_u64(offset_of!(general::stat, st_mode)) as u32;
            kassert_eq!(st_dev, SOCK_DEV.raw());
            kassert_eq!((DevId(st_dev).major(), DevId(st_dev).minor()), (0, 8));
            kassert!(st_ino > 0);
            kassert_eq!(st_mode & general::S_IFMT, general::S_IFSOCK);

            // /proc/<pid>/fd/<fd> reports `socket:[<st_ino>]` and refuses reopen with -ENXIO
            // unless O_PATH (Linux v6.12 `sock_no_open`, net/socket.c:1303).
            let expected_target = format!("socket:[{st_ino}]");
            let proc_path = format!("/proc/{pid}/fd/{fd}\0");
            copy_to_user(path_va, proc_path.as_bytes()).map_err(|_| "copy proc path")?;
            let n = syscall(general::__NR_readlinkat, [general::AT_FDCWD as u64, path_va, link_va, 64, 0, 0]);
            kassert_eq!(n, expected_target.len() as u64);
            let mut got = alloc::vec![0u8; expected_target.len()];
            copy_from_user(&mut got, link_va).map_err(|_| "copy readlink")?;
            kassert_eq!(got.as_slice(), expected_target.as_bytes());
            let target = process::process_fd_target(pid, fd as i32).ok_or("proc fd target missing")?;
            kassert_eq!(target.1.as_slice(), expected_target.as_bytes());

            kassert_eq!(
                syscall(general::__NR_openat, [general::AT_FDCWD as u64, path_va, u64::from(general::O_RDWR), 0, 0, 0]),
                neg(errno::ENXIO)
            );
            let opath_fd =
                syscall(general::__NR_openat, [general::AT_FDCWD as u64, path_va, u64::from(general::O_PATH), 0, 0, 0]);
            kassert!((opath_fd as i64) >= 0);
            kassert_eq!(syscall(general::__NR_close, [opath_fd, 0, 0, 0, 0, 0]), 0);

            // lseek / pread64 / pwrite64 -> -ESPIPE; mmap -> -ENODEV.
            kassert_eq!(
                syscall(general::__NR_lseek, [fd, 0, u64::from(general::SEEK_SET), 0, 0, 0]),
                neg(errno::ESPIPE)
            );
            kassert_eq!(syscall(general::__NR_pread64, [fd, link_va, 4, 0, 0, 0]), neg(errno::ESPIPE));
            kassert_eq!(syscall(general::__NR_pwrite64, [fd, link_va, 4, 0, 0, 0]), neg(errno::ESPIPE));
            kassert_eq!(
                syscall(
                    general::__NR_mmap,
                    [0, 4096, u64::from(general::PROT_READ), u64::from(general::MAP_PRIVATE), fd, 0]
                ),
                neg(errno::ENODEV)
            );

            // close calls release() only at the last end reference (not while a dup exists).
            let dup_fd = syscall(general::__NR_dup, [fd, 0, 0, 0, 0, 0]);
            kassert!((dup_fd as i64) >= 0);
            let rel_before = FACTORY_RELEASES.load(Ordering::SeqCst);
            kassert_eq!(syscall(general::__NR_close, [fd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(FACTORY_RELEASES.load(Ordering::SeqCst), rel_before);
            kassert_eq!(syscall(general::__NR_close, [dup_fd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(FACTORY_RELEASES.load(Ordering::SeqCst), rel_before + 1);

            // AF_UNIX SOCK_RAW maps to SockKind::Dgram, and without flags neither CLOEXEC nor NONBLOCK is set.
            let raw_unix =
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), u64::from(net::SOCK_RAW), 0, 0, 0, 0]);
            kassert!((raw_unix as i64) >= 0);
            kassert_eq!(*LAST_CREATED.lock(), Some((Family::Unix, SockKind::Dgram, 0)));
            kassert_eq!(syscall(general::__NR_fcntl, [raw_unix, u64::from(general::F_GETFD), 0, 0, 0, 0]), 0);
            kassert_eq!(
                (syscall(general::__NR_fcntl, [raw_unix, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32)
                    & general::O_NONBLOCK,
                0
            );
            kassert_eq!(syscall(general::__NR_close, [raw_unix, 0, 0, 0, 0, 0]), 0);

            // AF_INET / AF_INET6 dispatch for TCP, UDP, and ICMP raw/dgram.
            for (dom, ty, proto, exp_fam, exp_kind) in [
                (net::AF_INET, net::SOCK_STREAM, net::IPPROTO_TCP as u32, Family::Inet, SockKind::Stream),
                (net::AF_INET6, net::SOCK_DGRAM, net::IPPROTO_ICMPV6, Family::Inet6, SockKind::Dgram),
                (net::AF_INET, net::SOCK_RAW, net::IPPROTO_ICMP as u32, Family::Inet, SockKind::Raw),
            ] {
                let s = syscall(general::__NR_socket, [u64::from(dom), u64::from(ty), u64::from(proto), 0, 0, 0]);
                kassert!((s as i64) >= 0);
                kassert_eq!(*LAST_CREATED.lock(), Some((exp_fam, exp_kind, proto as i32)));
                kassert_eq!(syscall(general::__NR_close, [s, 0, 0, 0, 0, 0]), 0);
            }

            // socketpair: flags, distinct socket:[ino], and Linux v6.12 `__sys_socketpair` order
            // (net/socket.c:1744-1801): flags -> reserve both fds -> put_user(sv) -> create.
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [
                        u64::from(net::AF_UNIX),
                        u64::from(net::SOCK_SEQPACKET) | sock_nonblock | sock_cloexec,
                        0,
                        sv_va,
                        0,
                        0
                    ]
                ),
                0
            );
            kassert_eq!(*LAST_CREATED.lock(), Some((Family::Unix, SockKind::Seqpacket, 0)));
            let mut sv_raw = [0u8; 8];
            copy_from_user(&mut sv_raw, sv_va).map_err(|_| "copy sv")?;
            let sv0 = i32::from_le_bytes(sv_raw[0..4].try_into().expect("4 bytes")) as u64;
            let sv1 = i32::from_le_bytes(sv_raw[4..8].try_into().expect("4 bytes")) as u64;
            kassert!(sv0 != sv1);
            for s in [sv0, sv1] {
                kassert_eq!(
                    syscall(general::__NR_fcntl, [s, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                    u64::from(general::FD_CLOEXEC)
                );
                let sfl = syscall(general::__NR_fcntl, [s, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
                kassert!(sfl & general::O_NONBLOCK != 0);
            }
            let t0 = process::process_fd_target(pid, sv0 as i32).ok_or("sv0 target")?;
            let t1 = process::process_fd_target(pid, sv1 as i32).ok_or("sv1 target")?;
            kassert!(t0.1.starts_with(b"socket:["));
            kassert!(t1.1.starts_with(b"socket:["));
            kassert!(t0.1 != t1.1);
            kassert_eq!(syscall(general::__NR_close, [sv0, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [sv1, 0, 0, 0, 0, 0]), 0);

            // Bad `sv` faults before family validation/creation (net/socket.c:1766 before 1779):
            // 0 factory calls, 0 releases, no leaked fd reservations, -EFAULT beats bad domain 99.
            let open_before = process::process_open_fds(pid).len();
            let calls_before_fault = FACTORY_CALLS.load(Ordering::SeqCst);
            let rel_before_fault = FACTORY_RELEASES.load(Ordering::SeqCst);
            kassert_eq!(
                syscall(general::__NR_socketpair, [99, u64::from(net::SOCK_DGRAM), 0, 0, 0, 0]),
                neg(errno::EFAULT)
            );
            kassert_eq!(process::process_open_fds(pid).len(), open_before);
            kassert_eq!(FACTORY_CALLS.load(Ordering::SeqCst), calls_before_fault);
            kassert_eq!(FACTORY_RELEASES.load(Ordering::SeqCst), rel_before_fault);

            // Slot reservation and errno order under RLIMIT_NOFILE:
            // - socketpair with 1 slot free -> -EMFILE, 0 factory calls;
            // - socketpair with 0 slots free: bad flags -> -EINVAL before -EMFILE, bad sv -> -EMFILE before -EFAULT;
            // - accept4 (net/socket.c:2020 -> 1982 -> 1988 -> 1925): -EBADF -> flags -EINVAL -> -EMFILE -> -ENOTSOCK.
            let proc = process::current_process();
            let open_now = process::process_open_fds(pid).len() as u64;
            let orig_rlim = proc
                .rlimit(RlimitKind::Nofile, Some(Rlimit::new(open_now + 1, 1024).ok_or("rlimit+1")?))
                .map_err(|_| "set rlimit+1")?;
            let calls_before = FACTORY_CALLS.load(Ordering::SeqCst);
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, sv_va, 0, 0]
                ),
                neg(errno::EMFILE)
            );
            kassert_eq!(FACTORY_CALLS.load(Ordering::SeqCst), calls_before);

            let _ = proc
                .rlimit(RlimitKind::Nofile, Some(Rlimit::new(open_now, 1024).ok_or("rlimit")?))
                .map_err(|_| "set rlimit")?;
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                neg(errno::EMFILE)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM) | 0x100, 0, 0, 0, 0]
                ),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                syscall(general::__NR_socketpair, [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                neg(errno::EMFILE)
            );
            kassert_eq!(FACTORY_CALLS.load(Ordering::SeqCst), calls_before);

            // accept4 order with full fd table: -EBADF (fd 99) -> flags -EINVAL (fd 0) -> -EMFILE (fd 0) -> -ENOTSOCK.
            kassert_eq!(syscall(general::__NR_accept4, [99, 0, 0, 0x100, 0, 0]), neg(errno::EBADF));
            kassert_eq!(syscall(general::__NR_accept4, [0, 0, 0, 0x100, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(syscall(general::__NR_accept4, [0, 0, 0, 0, 0, 0]), neg(errno::EMFILE));
            let _ = proc.rlimit(RlimitKind::Nofile, Some(orig_rlim));
            kassert_eq!(syscall(general::__NR_accept4, [0, 0, 0, 0, 0, 0]), neg(errno::ENOTSOCK));

            // connect order (Linux v6.12 net/socket.c:2082-2088 `__sys_connect`):
            // -EBADF -> move_addr_to_kernel (-EINVAL / -EFAULT) -> -ENOTSOCK.
            kassert_eq!(
                syscall(general::__NR_connect, [99, stat_va, (-1i32) as u32 as u64, 0, 0, 0]),
                neg(errno::EBADF)
            );
            kassert_eq!(
                syscall(general::__NR_connect, [0, stat_va, (-1i32) as u32 as u64, 0, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(syscall(general::__NR_connect, [0, 0, 16, 0, 0, 0]), neg(errno::EFAULT));
            copy_to_user(stat_va, &[0u8; 16]).map_err(|_| "copy zero addr")?;
            kassert_eq!(syscall(general::__NR_connect, [0, stat_va, 16, 0, 0, 0]), neg(errno::ENOTSOCK));

            // sendto / recvfrom order (Linux v6.12 net/socket.c:2191, 2256):
            // `import_ubuf` (-EFAULT) before `sockfd_lookup_light` (-EBADF).
            let kernel_ptr = 0xffff_8000_0000_0000u64;
            kassert_eq!(syscall(general::__NR_sendto, [99, kernel_ptr, 4, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(syscall(general::__NR_recvfrom, [99, kernel_ptr, 4, 0, 0, 0]), neg(errno::EFAULT));

            // Non-socket fd -> -ENOTSOCK; closed fd -> -EBADF.
            kassert_eq!(syscall(general::__NR_listen, [0, 8, 0, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_shutdown, [0, 0, 0, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_getsockname, [0, stat_va, int_va, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_listen, [99, 8, 0, 0, 0, 0]), neg(errno::EBADF));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn socket_io_and_poll_reach_sockops() {
        use core::sync::atomic::{AtomicBool, Ordering};
        use core::time::Duration;

        use crate::linux::process::{KernelSigaction, Rlimit, RlimitKind};
        use crate::linux::signal::raise_for_test;
        use crate::linux::{errno, general, ioctl, net};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use ktest_support::{MockSock, neg, syscall};
        use lumen_signal::Signal;
        use lumen_sockabi::abi::Msghdr;
        use lumen_sockabi::{
            AF_UNIX, AF_UNSPEC, CmsgIter, CmsgWriter, Ipv4Addr, Linger, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT,
            MSG_EOR, MSG_ERRQUEUE, MSG_OOB, SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET, SocketAddrV4, Timeout, Timeval,
            UnixName, cmsg_len, cmsg_space,
        };

        process::in_test_process(|pid| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let buf_va = base + 64;
            let out_va = base + 256;
            let iov_va = base + 512;
            let addr_va = base + 768;
            let len_va = base + 900;
            let poll_va = base + 960;
            let msg_va = base + 1024;
            let ctl_va = base + 1280;
            let opt_va = base + 1792;
            let path_va = base + 1920;

            // Install stream, datagram, and seqpacket MockSocks.
            let stream = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            let stream_file = new_socket_file(Arc::clone(&stream) as Arc<dyn SockOps>, false).map_err(|_| "stream")?;
            let sfd_id = stream_file.id();
            let sfd = process::install_current_fd(stream_file, false).map_err(|_| "install sfd")? as u64;

            let dgram = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let dgram_file = new_socket_file(Arc::clone(&dgram) as Arc<dyn SockOps>, false).map_err(|_| "dgram")?;
            let dfd = process::install_current_fd(dgram_file, false).map_err(|_| "install dfd")? as u64;

            let seq = MockSock::new(Family::Unix, SockKind::Seqpacket, 0);
            let seq_file = new_socket_file(Arc::clone(&seq) as Arc<dyn SockOps>, false).map_err(|_| "seq")?;
            let qfd = process::install_current_fd(seq_file, false).map_err(|_| "install qfd")? as u64;

            // 1. write -> SockOps::send(flags=0), FIONREAD -> SockOps::fionread, read -> SockOps::recv(flags=0),
            //    zero-length read/readv bypass recv (Linux v6.12 net/socket.c:1140 `sock_read_iter`),
            //    seqpacket write/writev set MSG_EOR (net/socket.c:1162 `sock_write_iter`), and nonblock propagation.
            copy_to_user(buf_va, b"hello").map_err(|_| "copy hello")?;
            kassert_eq!(syscall(general::__NR_write, [sfd, buf_va, 5, 0, 0, 0]), 5);
            kassert_eq!(stream.state.lock().send_calls, 1);
            kassert_eq!(stream.state.lock().last_send_flags, Some(0));
            kassert_eq!(stream.state.lock().last_send_nonblock, Some(false));

            kassert_eq!(syscall(general::__NR_ioctl, [sfd, u64::from(ioctl::FIONREAD), len_va, 0, 0, 0]), 0);
            let mut n_raw = [0u8; 4];
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read fionread")?;
            kassert_eq!(i32::from_le_bytes(n_raw), 5);
            kassert_eq!(stream.state.lock().fionread_calls, 1);
            // Unknown ioctl on a socket returns -ENOTTY.
            kassert_eq!(
                syscall(general::__NR_ioctl, [sfd, u64::from(ioctl::TCGETS), len_va, 0, 0, 0]),
                neg(errno::ENOTTY)
            );

            // Zero-length read returns 0 without calling SockOps::recv (sock_read_iter, net/socket.c:1140).
            kassert_eq!(syscall(general::__NR_read, [sfd, out_va, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().recv_calls, 0);
            // Faulting non-zero read still gives -EFAULT before recv.
            kassert_eq!(syscall(general::__NR_read, [sfd, 0xffff_8000_0000_0000u64, 4, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(stream.state.lock().recv_calls, 0);

            kassert_eq!(syscall(general::__NR_read, [sfd, out_va, 5, 0, 0, 0]), 5);
            let mut got5 = [0u8; 5];
            copy_from_user(&mut got5, out_va).map_err(|_| "read out5")?;
            kassert_eq!(&got5, b"hello");
            kassert_eq!(stream.state.lock().recv_calls, 1);
            kassert_eq!(stream.state.lock().last_recv_flags, Some(0));
            kassert_eq!(stream.state.lock().last_recv_nonblock, Some(false));

            // Zero-length readv returns 0 without calling SockOps::recv (sock_read_iter, net/socket.c:1140).
            let mut zero_iovs = [0u8; 32];
            zero_iovs[0..8].copy_from_slice(&out_va.to_le_bytes());
            zero_iovs[16..24].copy_from_slice(&out_va.to_le_bytes());
            copy_to_user(iov_va, &zero_iovs).map_err(|_| "copy zero iovs")?;
            kassert_eq!(syscall(general::__NR_readv, [sfd, iov_va, 2, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().recv_calls, 1);

            // Seqpacket write/writev set MSG_EOR (Linux v6.12 net/socket.c:1162 `sock_write_iter`).
            kassert_eq!(syscall(general::__NR_write, [qfd, buf_va, 2, 0, 0, 0]), 2);
            kassert_eq!(seq.state.lock().last_send_flags, Some(MSG_EOR));

            // 2. writev / readv reach SockOps::send / SockOps::recv with gather/scatter spans.
            copy_to_user(buf_va, b"ab").map_err(|_| "copy ab")?;
            copy_to_user(buf_va + 16, b"cde").map_err(|_| "copy cde")?;
            let mut raw_iovs = [0u8; 48];
            raw_iovs[0..8].copy_from_slice(&buf_va.to_le_bytes());
            raw_iovs[8..16].copy_from_slice(&2u64.to_le_bytes());
            raw_iovs[16..24].copy_from_slice(&buf_va.to_le_bytes());
            raw_iovs[24..32].copy_from_slice(&0u64.to_le_bytes());
            raw_iovs[32..40].copy_from_slice(&(buf_va + 16).to_le_bytes());
            raw_iovs[40..48].copy_from_slice(&3u64.to_le_bytes());
            copy_to_user(iov_va, &raw_iovs).map_err(|_| "copy writev iovs")?;
            kassert_eq!(syscall(general::__NR_writev, [sfd, iov_va, 3, 0, 0, 0]), 5);
            kassert_eq!(stream.state.lock().last_send_flags, Some(0));
            kassert_eq!(syscall(general::__NR_writev, [qfd, iov_va, 3, 0, 0, 0]), 5);
            kassert_eq!(seq.state.lock().last_send_flags, Some(MSG_EOR));

            raw_iovs[0..8].copy_from_slice(&out_va.to_le_bytes());
            raw_iovs[8..16].copy_from_slice(&2u64.to_le_bytes());
            raw_iovs[16..24].copy_from_slice(&out_va.to_le_bytes());
            raw_iovs[24..32].copy_from_slice(&0u64.to_le_bytes());
            raw_iovs[32..40].copy_from_slice(&(out_va + 16).to_le_bytes());
            raw_iovs[40..48].copy_from_slice(&3u64.to_le_bytes());
            copy_to_user(iov_va, &raw_iovs).map_err(|_| "copy readv iovs")?;
            kassert_eq!(syscall(general::__NR_readv, [sfd, iov_va, 3, 0, 0, 0]), 5);
            let mut p1 = [0u8; 2];
            let mut p2 = [0u8; 3];
            copy_from_user(&mut p1, out_va).map_err(|_| "read p1")?;
            copy_from_user(&mut p2, out_va + 16).map_err(|_| "read p2")?;
            kassert_eq!(&p1, b"ab");
            kassert_eq!(&p2, b"cde");

            // Nonblock propagation on read, write, sendmsg, recvmsg (O_NONBLOCK) and recvfrom(MSG_DONTWAIT).
            kassert_eq!(
                syscall(
                    general::__NR_fcntl,
                    [sfd, u64::from(general::F_SETFL), u64::from(general::O_NONBLOCK), 0, 0, 0]
                ),
                0
            );
            kassert_eq!(syscall(general::__NR_write, [sfd, buf_va, 2, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_send_nonblock, Some(true));
            kassert_eq!(syscall(general::__NR_read, [sfd, out_va, 2, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_recv_nonblock, Some(true));
            let mut one_iov = [0u8; 16];
            one_iov[0..8].copy_from_slice(&buf_va.to_le_bytes());
            one_iov[8..16].copy_from_slice(&2u64.to_le_bytes());
            copy_to_user(iov_va, &one_iov).map_err(|_| "copy nb iov")?;
            let nb_hdr = Msghdr { msg_iov: iov_va, msg_iovlen: 1, ..Default::default() };
            copy_to_user(msg_va, &nb_hdr.to_bytes()).map_err(|_| "copy nb hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [sfd, msg_va, 0, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_send_nonblock, Some(true));
            kassert_eq!(syscall(general::__NR_recvmsg, [sfd, msg_va, 0, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_recv_nonblock, Some(true));
            kassert_eq!(syscall(general::__NR_fcntl, [sfd, u64::from(general::F_SETFL), 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_recvfrom, [sfd, out_va, 1, u64::from(MSG_DONTWAIT), 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_recv_nonblock, Some(true));
            kassert_eq!(syscall(general::__NR_recvfrom, [sfd, out_va, 1, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_recv_nonblock, Some(false));

            // 3. poll reaches SockOps::poll both immediately and via blocking wakeup (KtestThread::settle).
            stream.set_mask_and_notify(PollMask::EMPTY);
            let mut pfd = [0u8; 8];
            pfd[0..4].copy_from_slice(&(sfd as i32).to_le_bytes());
            pfd[4..6].copy_from_slice(&(PollMask::IN.bits() as i16).to_le_bytes());
            copy_to_user(poll_va, &pfd).map_err(|_| "copy pfd")?;
            kassert_eq!(syscall(general::__NR_poll, [poll_va, 1, 0, 0, 0, 0]), 0);

            let waiter = process::spawn_test_thread(move |_| {
                syscall(general::__NR_poll, [poll_va, 1, (-1i64) as u64, 0, 0, 0])
            })?;
            kassert!(waiter.settle());
            kassert_eq!(stream.poll_head.subscriber_count(), 1);
            stream.set_mask_and_notify(PollMask::IN | PollMask::RDNORM);
            kassert_eq!(waiter.join()?, 1);
            copy_from_user(&mut pfd, poll_va).map_err(|_| "read revents")?;
            kassert_eq!(u16::from_le_bytes([pfd[6], pfd[7]]), PollMask::IN.bits());

            // 4. bind, listen (with SOMAXCONN clamp + seqpacket), accept/accept4, connect (and AF_UNSPEC),
            //    Call::Bind vs Call::Connect (unix autobind), shutdown, getsockname, getpeername (with canary).
            let inet_addr = SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080));
            let mut enc = [0u8; 128];
            let enc_len = inet_addr.encode(&mut enc);
            copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy inet_addr")?;
            kassert_eq!(syscall(general::__NR_bind, [sfd, addr_va, enc_len as u64, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_bind, Some(inet_addr));

            // Call::Bind vs Call::Connect: AF_UNIX with addrlen == 2 autobinds (UnixName::Unnamed) on bind,
            // but fails -EINVAL on connect (lumen-sockabi / Linux v6.12 `unix_mkname`).
            copy_to_user(addr_va, &AF_UNIX.to_le_bytes()).map_err(|_| "copy AF_UNIX")?;
            kassert_eq!(syscall(general::__NR_bind, [dfd, addr_va, 2, 0, 0, 0]), 0);
            kassert_eq!(dgram.state.lock().last_bind, Some(SockAddr::Unix(UnixName::Unnamed)));
            kassert_eq!(syscall(general::__NR_connect, [dfd, addr_va, 2, 0, 0, 0]), neg(errno::EINVAL));

            // listen clamps negative / > SOMAXCONN backlogs to 4096 (Linux v6.12 `__sys_listen_socket`, net/socket.c:1888).
            kassert_eq!(syscall(general::__NR_listen, [sfd, 16, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_listen, Some(16));
            kassert_eq!(syscall(general::__NR_listen, [sfd, (-1i32) as u32 as u64, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_listen, Some(4096));
            kassert_eq!(syscall(general::__NR_listen, [sfd, 100_000, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_listen, Some(4096));
            // listen succeeds on seqpacket, -EOPNOTSUPP on datagram.
            kassert_eq!(syscall(general::__NR_listen, [qfd, 8, 0, 0, 0, 0]), 0);
            kassert_eq!(seq.state.lock().last_listen, Some(8));
            kassert_eq!(syscall(general::__NR_listen, [dfd, 16, 0, 0, 0, 0]), neg(errno::EOPNOTSUPP));

            // getsockname: truncated copy-out writes full length back without clobbering canary byte past *addrlen.
            copy_to_user(out_va, &[0xA5u8; 16]).map_err(|_| "fill canary")?;
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy len 4")?;
            kassert_eq!(syscall(general::__NR_getsockname, [sfd, out_va, len_va, 0, 0, 0]), 0);
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read out len")?;
            kassert_eq!(i32::from_le_bytes(n_raw), enc_len as i32);
            let mut got_addr = [0u8; 16];
            copy_from_user(&mut got_addr, out_va).map_err(|_| "read trunc sockname")?;
            kassert_eq!(&got_addr[..4], &enc[..4]);
            kassert_eq!(got_addr[4], 0xA5);

            // getpeername before connect -> -ENOTCONN; after connect -> truncated copy-out + canary; AF_UNSPEC -> disconnect.
            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            kassert_eq!(syscall(general::__NR_getpeername, [sfd, out_va, len_va, 0, 0, 0]), neg(errno::ENOTCONN));
            copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy inet_addr")?;
            kassert_eq!(syscall(general::__NR_connect, [sfd, addr_va, enc_len as u64, 0, 0, 0]), 0);
            copy_to_user(out_va, &[0xA5u8; 16]).map_err(|_| "fill peer canary")?;
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy len 4")?;
            kassert_eq!(syscall(general::__NR_getpeername, [sfd, out_va, len_va, 0, 0, 0]), 0);
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read peer len")?;
            kassert_eq!(i32::from_le_bytes(n_raw), enc_len as i32);
            copy_from_user(&mut got_addr, out_va).map_err(|_| "read trunc peername")?;
            kassert_eq!(&got_addr[..4], &enc[..4]);
            kassert_eq!(got_addr[4], 0xA5);

            copy_to_user(addr_va, &AF_UNSPEC.to_le_bytes()).map_err(|_| "copy AF_UNSPEC")?;
            kassert_eq!(syscall(general::__NR_connect, [sfd, addr_va, 2, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().disconnects, 1);

            // accept4 / accept: EMFILE reserves before accept; bad flags; SOCK_NONBLOCK & peer address bytes;
            // syscall 43 (__NR_accept); faulting addr/addrlen installs no fd and releases child.
            let peer_sock = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            stream.state.lock().pending_accept = Some((Arc::clone(&peer_sock) as Arc<dyn SockOps>, Some(inet_addr)));
            let proc = process::current_process();
            let open_now = process::process_open_fds(pid).len() as u64;
            let orig_rlim = proc
                .rlimit(RlimitKind::Nofile, Some(Rlimit::new(open_now, 1024).ok_or("rlim")?))
                .map_err(|_| "set rlim")?;
            kassert_eq!(syscall(general::__NR_accept4, [sfd, 0, 0, 0, 0, 0]), neg(errno::EMFILE));
            kassert_eq!(stream.state.lock().accept_calls, 0);
            let _ = proc.rlimit(RlimitKind::Nofile, Some(orig_rlim));
            kassert_eq!(syscall(general::__NR_accept4, [sfd, 0, 0, 0x100, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(stream.state.lock().accept_calls, 0);

            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            let afd = syscall(
                general::__NR_accept4,
                [sfd, out_va, len_va, u64::from(general::O_NONBLOCK | general::O_CLOEXEC), 0, 0],
            );
            kassert!((afd as i64) >= 0);
            kassert_eq!(
                syscall(general::__NR_fcntl, [afd, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                u64::from(general::FD_CLOEXEC)
            );
            let afl = syscall(general::__NR_fcntl, [afd, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
            kassert!(afl & general::O_NONBLOCK != 0);
            copy_from_user(&mut got_addr, out_va).map_err(|_| "read accept4 peer")?;
            kassert_eq!(&got_addr[..enc_len], &enc[..enc_len]);
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read accept4 len")?;
            kassert_eq!(i32::from_le_bytes(n_raw), enc_len as i32);
            kassert_eq!(syscall(general::__NR_close, [afd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(peer_sock.releases.load(Ordering::SeqCst), 1);

            // Syscall 43 (__NR_accept): neither FD_CLOEXEC nor O_NONBLOCK.
            let peer_sock2 = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            stream.state.lock().pending_accept = Some((Arc::clone(&peer_sock2) as Arc<dyn SockOps>, Some(inet_addr)));
            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            let afd2 = syscall(general::__NR_accept, [sfd, out_va, len_va, 0, 0, 0]);
            kassert!((afd2 as i64) >= 0);
            kassert_eq!(syscall(general::__NR_fcntl, [afd2, u64::from(general::F_GETFD), 0, 0, 0, 0]), 0);
            let afl2 = syscall(general::__NR_fcntl, [afd2, u64::from(general::F_GETFL), 0, 0, 0, 0]) as u32;
            kassert_eq!(afl2 & general::O_NONBLOCK, 0);
            kassert_eq!(syscall(general::__NR_close, [afd2, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(peer_sock2.releases.load(Ordering::SeqCst), 1);

            // accept4 with faulting addr or faulting addrlen installs no fd and releases the accepted child.
            let open_before_accept = process::process_open_fds(pid).len();
            let fault_child1 = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            stream.state.lock().pending_accept = Some((Arc::clone(&fault_child1) as Arc<dyn SockOps>, Some(inet_addr)));
            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            kassert_eq!(syscall(general::__NR_accept4, [sfd, 0x1000, len_va, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(process::process_open_fds(pid).len(), open_before_accept);
            kassert_eq!(fault_child1.releases.load(Ordering::SeqCst), 1);

            let fault_child2 = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            stream.state.lock().pending_accept = Some((Arc::clone(&fault_child2) as Arc<dyn SockOps>, Some(inet_addr)));
            kassert_eq!(syscall(general::__NR_accept4, [sfd, out_va, 0x1000, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(process::process_open_fds(pid).len(), open_before_accept);
            kassert_eq!(fault_child2.releases.load(Ordering::SeqCst), 1);

            // shutdown valid and invalid.
            kassert_eq!(syscall(general::__NR_shutdown, [sfd, u64::from(net::SHUT_WR), 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_shutdown, Some(ShutdownHow::Write));
            kassert_eq!(syscall(general::__NR_shutdown, [sfd, 3, 0, 0, 0, 0]), neg(errno::EINVAL));

            // 5. setsockopt / getsockopt (including SO_RCVTIMEO round-trip and unix non-SOL_SOCKET getsockopt).
            copy_to_user(opt_va, &1i32.to_le_bytes()).map_err(|_| "copy opt 1")?;
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_REUSEADDR), opt_va, 4, 0]
                ),
                0
            );
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy len 4")?;
            kassert_eq!(
                syscall(
                    general::__NR_getsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_REUSEADDR), out_va, len_va, 0]
                ),
                0
            );
            copy_from_user(&mut n_raw, out_va).map_err(|_| "read reuseaddr")?;
            kassert_eq!(i32::from_le_bytes(n_raw), 1);
            // Get-only SO_TYPE fails on setsockopt (-ENOPROTOOPT), succeeds on getsockopt.
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_TYPE), opt_va, 4, 0]
                ),
                neg(errno::ENOPROTOOPT)
            );
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_REUSEADDR), 0, 2, 0]
                ),
                neg(errno::EINVAL)
            );
            copy_to_user(len_va, &(-1i32).to_le_bytes()).map_err(|_| "copy -1")?;
            kassert_eq!(
                syscall(
                    general::__NR_getsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_TYPE), out_va, len_va, 0]
                ),
                neg(errno::EINVAL)
            );
            // Linger, SO_RCVTIMEO finite round-trip, and non-SOL_SOCKET on AF_UNIX (-EOPNOTSUPP for both set and get).
            let linger = Linger { onoff: 1, linger: 5 };
            copy_to_user(opt_va, &linger.to_bytes()).map_err(|_| "copy linger")?;
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_LINGER), opt_va, 8, 0]
                ),
                0
            );
            kassert_eq!(stream.state.lock().opt_linger, linger);

            let tv = Timeval { tv_sec: 2, tv_usec: 500_000 };
            copy_to_user(opt_va, &tv.to_bytes()).map_err(|_| "copy tv")?;
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_RCVTIMEO_OLD), opt_va, 16, 0]
                ),
                0
            );
            kassert_eq!(stream.state.lock().opt_rcvtimeo, Timeout::Duration(Duration::from_millis(2_500)));
            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            kassert_eq!(
                syscall(
                    general::__NR_getsockopt,
                    [sfd, u64::from(net::SOL_SOCKET), u64::from(net::SO_RCVTIMEO_OLD), out_va, len_va, 0]
                ),
                0
            );
            let mut got_tv = [0u8; 16];
            copy_from_user(&mut got_tv, out_va).map_err(|_| "read tv")?;
            kassert_eq!(Timeval::from_bytes(&got_tv), tv);

            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [dfd, u64::from(net::SOL_TCP), u64::from(net::TCP_NODELAY), opt_va, 4, 0]
                ),
                neg(errno::EOPNOTSUPP)
            );
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy len 4")?;
            kassert_eq!(
                syscall(
                    general::__NR_getsockopt,
                    [dfd, u64::from(net::SOL_TCP), u64::from(net::TCP_NODELAY), out_va, len_va, 0]
                ),
                neg(errno::EOPNOTSUPP)
            );

            // 6. Destination names on stream/seqpacket (`move_addr_to_kernel`, Linux v6.12 net/socket.c:2203, 2510):
            //    always copied and length-checked; `named` set; `to` decoded for datagram/raw only.
            kassert_eq!(syscall(general::__NR_sendto, [sfd, buf_va, 2, 0, 0x1000, 16]), neg(errno::EFAULT));
            kassert_eq!(
                syscall(general::__NR_sendto, [sfd, buf_va, 2, 0, addr_va, (-1i32) as u32 as u64]),
                neg(errno::EINVAL)
            );
            copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy inet_addr")?;
            kassert_eq!(syscall(general::__NR_sendto, [sfd, buf_va, 2, 0, addr_va, enc_len as u64]), 2);
            kassert_eq!(stream.state.lock().last_send_named, Some(true));
            kassert_eq!(stream.state.lock().last_send_to, None);
            kassert_eq!(syscall(general::__NR_sendto, [sfd, buf_va, 2, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_send_named, Some(false));
            kassert_eq!(stream.state.lock().last_send_to, None);
            kassert_eq!(syscall(general::__NR_sendto, [qfd, buf_va, 2, 0, addr_va, enc_len as u64]), 2);
            kassert_eq!(seq.state.lock().last_send_named, Some(true));
            kassert_eq!(seq.state.lock().last_send_to, None);

            let stream_bad_name_hdr =
                Msghdr { msg_name: 0x1000, msg_namelen: 16, msg_iov: iov_va, msg_iovlen: 1, ..Default::default() };
            copy_to_user(msg_va, &stream_bad_name_hdr.to_bytes()).map_err(|_| "copy bad name hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [sfd, msg_va, 0, 0, 0, 0]), neg(errno::EFAULT));
            let stream_named_hdr = Msghdr {
                msg_name: addr_va,
                msg_namelen: enc_len as i32,
                msg_iov: iov_va,
                msg_iovlen: 1,
                ..Default::default()
            };
            copy_to_user(msg_va, &stream_named_hdr.to_bytes()).map_err(|_| "copy stream named hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [sfd, msg_va, 0, 0, 0, 0]), 2);
            kassert_eq!(stream.state.lock().last_send_named, Some(true));
            kassert_eq!(stream.state.lock().last_send_to, None);

            // 7. Plain AF_INET on AF_INET6 routed for UDP/dgram only (Linux v6.12
            //    `__ip6_datagram_connect` net/ipv6/datagram.c:154, `tcp_v6_connect` net/ipv6/tcp_ipv6.c:152-156).
            let v6_udp = MockSock::new(Family::Inet6, SockKind::Dgram, net::IPPROTO_UDP as i32);
            let v6_udp_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&v6_udp) as Arc<dyn SockOps>, false).map_err(|_| "v6udp")?,
                false,
            )
            .map_err(|_| "v6udp fd")? as u64;
            let v6_tcp = MockSock::new(Family::Inet6, SockKind::Stream, net::IPPROTO_TCP as i32);
            let v6_tcp_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&v6_tcp) as Arc<dyn SockOps>, false).map_err(|_| "v6tcp")?,
                false,
            )
            .map_err(|_| "v6tcp fd")? as u64;
            let v6_raw = MockSock::new(Family::Inet6, SockKind::Raw, net::IPPROTO_ICMPV6 as i32);
            let v6_raw_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&v6_raw) as Arc<dyn SockOps>, false).map_err(|_| "v6raw")?,
                false,
            )
            .map_err(|_| "v6raw fd")? as u64;
            let v6_ping = MockSock::new(Family::Inet6, SockKind::Dgram, net::IPPROTO_ICMPV6 as i32);
            let v6_ping_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&v6_ping) as Arc<dyn SockOps>, false).map_err(|_| "v6ping")?,
                false,
            )
            .map_err(|_| "v6ping fd")? as u64;

            copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy v4 addr")?;
            kassert_eq!(syscall(general::__NR_connect, [v6_udp_fd, addr_va, enc_len as u64, 0, 0, 0]), 0);
            kassert_eq!(v6_udp.state.lock().last_connect, Some((inet_addr, false)));
            kassert_eq!(syscall(general::__NR_sendto, [v6_udp_fd, buf_va, 2, 0, addr_va, enc_len as u64]), 2);
            kassert_eq!(v6_udp.state.lock().last_send_to, Some(inet_addr));
            kassert_eq!(syscall(general::__NR_sendmsg, [v6_udp_fd, msg_va, 0, 0, 0, 0]), 2);
            kassert_eq!(v6_udp.state.lock().last_send_to, Some(inet_addr));

            // 16-byte AF_INET on TCP/raw/ping -> -EINVAL; 28-byte AF_INET -> -EAFNOSUPPORT.
            let mut v4_padded = [0u8; 28];
            v4_padded[..enc_len].copy_from_slice(&enc[..enc_len]);
            for bad_v6 in [v6_tcp_fd, v6_raw_fd, v6_ping_fd] {
                copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy 16B v4")?;
                kassert_eq!(syscall(general::__NR_connect, [bad_v6, addr_va, 16, 0, 0, 0]), neg(errno::EINVAL));
                copy_to_user(addr_va, &v4_padded).map_err(|_| "copy 28B v4")?;
                kassert_eq!(syscall(general::__NR_connect, [bad_v6, addr_va, 28, 0, 0, 0]), neg(errno::EAFNOSUPPORT));
            }
            for non_udp_v6 in [v6_raw_fd, v6_ping_fd] {
                copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy 16B v4")?;
                kassert_eq!(syscall(general::__NR_sendto, [non_udp_v6, buf_va, 2, 0, addr_va, 16]), neg(errno::EINVAL));
                copy_to_user(addr_va, &v4_padded).map_err(|_| "copy 28B v4")?;
                kassert_eq!(
                    syscall(general::__NR_sendto, [non_udp_v6, buf_va, 2, 0, addr_va, 28]),
                    neg(errno::EAFNOSUPPORT)
                );
            }
            for f in [v6_udp_fd, v6_tcp_fd, v6_raw_fd, v6_ping_fd] {
                kassert_eq!(syscall(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }

            // 8. sendto / recvfrom flags, destination decode, and truncated peer address canary on recvfrom & recvmsg.
            let unix_dst = SockAddr::Unix(UnixName::abstract_name(b"srv").map_err(|_| "unix name")?);
            let ulen = unix_dst.encode(&mut enc);
            copy_to_user(addr_va, &enc[..ulen]).map_err(|_| "copy unix dst")?;
            copy_to_user(buf_va, b"ping").map_err(|_| "copy ping")?;
            kassert_eq!(
                syscall(general::__NR_sendto, [dfd, buf_va, 4, u64::from(MSG_DONTWAIT), addr_va, ulen as u64]),
                4
            );
            kassert_eq!(dgram.state.lock().last_send_to, Some(unix_dst));
            kassert_eq!(dgram.state.lock().last_send_named, Some(true));
            kassert_eq!(dgram.state.lock().last_send_nonblock, Some(true));
            kassert_eq!(
                syscall(general::__NR_sendto, [dfd, buf_va, 4, u64::from(MSG_OOB), 0, 0]),
                neg(errno::EOPNOTSUPP)
            );
            kassert_eq!(
                syscall(general::__NR_recvfrom, [dfd, out_va, 4, u64::from(MSG_ERRQUEUE), 0, 0]),
                neg(errno::EAGAIN)
            );
            kassert_eq!(
                syscall(general::__NR_recvfrom, [dfd, out_va, 4, u64::from(MSG_OOB), 0, 0]),
                neg(errno::EINVAL)
            );
            dgram.state.lock().queued_from = Some(unix_dst);
            copy_to_user(addr_va, &[0xA5u8; 16]).map_err(|_| "fill recvfrom canary")?;
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy 4")?;
            kassert_eq!(syscall(general::__NR_recvfrom, [dfd, out_va, 4, 0, addr_va, len_va]), 4);
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read recvfrom len")?;
            kassert_eq!(i32::from_le_bytes(n_raw), ulen as i32);
            copy_from_user(&mut got_addr, addr_va).map_err(|_| "read recvfrom trunc")?;
            kassert_eq!(&got_addr[..4], &enc[..4]);
            kassert_eq!(got_addr[4], 0xA5);

            // Truncated `msg_name` canary on recvmsg.
            kassert_eq!(syscall(general::__NR_sendto, [dfd, buf_va, 4, 0, 0, 0]), 4);
            copy_to_user(addr_va, &[0xA5u8; 16]).map_err(|_| "fill recvmsg canary")?;
            let mut rname_iov = [0u8; 16];
            rname_iov[0..8].copy_from_slice(&out_va.to_le_bytes());
            rname_iov[8..16].copy_from_slice(&4u64.to_le_bytes());
            copy_to_user(iov_va, &rname_iov).map_err(|_| "copy rname iov")?;
            let rname_hdr =
                Msghdr { msg_name: addr_va, msg_namelen: 4, msg_iov: iov_va, msg_iovlen: 1, ..Default::default() };
            copy_to_user(msg_va, &rname_hdr.to_bytes()).map_err(|_| "copy rname hdr")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), 4);
            let mut out_hdr_raw = [0u8; 56];
            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read rname out hdr")?;
            let rname_out = Msghdr::from_bytes(&out_hdr_raw);
            kassert_eq!(rname_out.msg_namelen, ulen as i32);
            copy_from_user(&mut got_addr, addr_va).map_err(|_| "read recvmsg trunc")?;
            kassert_eq!(&got_addr[..4], &enc[..4]);
            kassert_eq!(got_addr[4], 0xA5);

            // 9. sendmsg / recvmsg `__copy_msghdr` errno order (Linux v6.12 net/socket.c:2483-2548):
            //    bad msg ptr (-EFAULT) before flags; negative msg_namelen (-EINVAL) before UIO_MAXIOV (-EMSGSIZE);
            //    UIO_MAXIOV (-EMSGSIZE) before MSG_OOB/MSG_ERRQUEUE; optmem_max+1 -> -ENOBUFS.
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, 0, u64::from(MSG_OOB), 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, 0, u64::from(MSG_ERRQUEUE), 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, 0, u64::from(MSG_OOB), 0, 0, 0]), neg(errno::EFAULT));

            let neg_name_and_big_iov = Msghdr {
                msg_name: addr_va,
                msg_namelen: -1,
                msg_iov: iov_va,
                msg_iovlen: u64::from(general::UIO_MAXIOV) + 1,
                ..Default::default()
            };
            copy_to_user(msg_va, &neg_name_and_big_iov.to_bytes()).map_err(|_| "copy neg_name")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EINVAL));

            let bad_iov_hdr =
                Msghdr { msg_iov: iov_va, msg_iovlen: u64::from(general::UIO_MAXIOV) + 1, ..Default::default() };
            copy_to_user(msg_va, &bad_iov_hdr.to_bytes()).map_err(|_| "copy bad_iov_hdr")?;
            kassert_eq!(
                syscall(general::__NR_sendmsg, [dfd, msg_va, u64::from(MSG_OOB), 0, 0, 0]),
                neg(errno::EMSGSIZE)
            );
            kassert_eq!(
                syscall(general::__NR_recvmsg, [dfd, msg_va, u64::from(MSG_ERRQUEUE), 0, 0, 0]),
                neg(errno::EMSGSIZE)
            );
            kassert_eq!(
                syscall(general::__NR_recvmsg, [dfd, msg_va, u64::from(MSG_OOB), 0, 0, 0]),
                neg(errno::EMSGSIZE)
            );

            // Duplicate flag guards on sendmsg/recvmsg with valid msghdr.
            let empty_hdr = Msghdr::default();
            copy_to_user(msg_va, &empty_hdr.to_bytes()).map_err(|_| "copy empty_hdr")?;
            kassert_eq!(
                syscall(general::__NR_sendmsg, [dfd, msg_va, u64::from(MSG_OOB), 0, 0, 0]),
                neg(errno::EOPNOTSUPP)
            );
            kassert_eq!(
                syscall(general::__NR_recvmsg, [dfd, msg_va, u64::from(MSG_ERRQUEUE), 0, 0, 0]),
                neg(errno::EAGAIN)
            );
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, u64::from(MSG_OOB), 0, 0, 0]), neg(errno::EINVAL));

            let bad_ctl_hdr = Msghdr { msg_control: ctl_va, msg_controllen: 131_073, ..Default::default() };
            copy_to_user(msg_va, &bad_ctl_hdr.to_bytes()).map_err(|_| "copy bad_ctl_hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::ENOBUFS));

            // Bad SCM_CREDENTIALS pid -> -ESRCH.
            let mut cbuf = [0u8; 64];
            let mut cw = CmsgWriter::new(&mut cbuf);
            kassert!(cw.push_creds(Ucred { pid: 30_000, uid: 0, gid: 0 }));
            let (clen, _) = cw.finish();
            copy_to_user(ctl_va, &cbuf[..clen]).map_err(|_| "copy bad creds")?;
            let cred_hdr = Msghdr { msg_control: ctl_va, msg_controllen: clen as u64, ..Default::default() };
            copy_to_user(msg_va, &cred_hdr.to_bytes()).map_err(|_| "copy cred hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::ESRCH));

            // sendmsg cmsg edge cases: [good, bad] -> -EBADF with good released; >253 fds -> -EINVAL;
            // unknown SOL_SOCKET type -> -EINVAL; non-SOL_SOCKET cmsg skipped; send() failing with rights.
            let good_sock = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let good_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&good_sock) as Arc<dyn SockOps>, false).map_err(|_| "good")?,
                false,
            )
            .map_err(|_| "good fd")?;

            let mut cbuf2 = [0u8; 64];
            let mut cw2 = CmsgWriter::new(&mut cbuf2);
            kassert_eq!(cw2.push_rights(&[good_fd, 99], 2), 2);
            let (clen2, _) = cw2.finish();
            copy_to_user(ctl_va, &cbuf2[..clen2]).map_err(|_| "copy [good, 99]")?;
            let bad_fd_hdr = Msghdr { msg_control: ctl_va, msg_controllen: clen2 as u64, ..Default::default() };
            copy_to_user(msg_va, &bad_fd_hdr.to_bytes()).map_err(|_| "copy bad_fd_hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EBADF));

            // >253 fds (254 copies of good_fd) -> -EINVAL.
            let many_len = cmsg_space(254 * 4).ok_or("cmsg_space 254")?;
            let mut many_buf = alloc::vec![0u8; many_len];
            let mut cw_many = CmsgWriter::new(&mut many_buf);
            let many_fds = alloc::vec![good_fd; 254];
            kassert_eq!(cw_many.push_rights(&many_fds, 254), 254);
            let (many_written, _) = cw_many.finish();
            let big_ctl_va = base + 2048;
            copy_to_user(big_ctl_va, &many_buf[..many_written]).map_err(|_| "copy 254 fds")?;
            let many_hdr =
                Msghdr { msg_control: big_ctl_va, msg_controllen: many_written as u64, ..Default::default() };
            copy_to_user(msg_va, &many_hdr.to_bytes()).map_err(|_| "copy many_hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EINVAL));

            // Unknown SOL_SOCKET cmsg type -> -EINVAL.
            let mut unk_buf = [0u8; 32];
            let mut cw_unk = CmsgWriter::new(&mut unk_buf);
            kassert!(cw_unk.push_raw(SOL_SOCKET, 99, &[0u8; 4]));
            let (unk_len, _) = cw_unk.finish();
            copy_to_user(ctl_va, &unk_buf[..unk_len]).map_err(|_| "copy unk cmsg")?;
            let unk_hdr = Msghdr { msg_control: ctl_va, msg_controllen: unk_len as u64, ..Default::default() };
            copy_to_user(msg_va, &unk_hdr.to_bytes()).map_err(|_| "copy unk hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EINVAL));

            // Non-SOL_SOCKET cmsg skipped cleanly.
            let mut skip_buf = [0u8; 32];
            let mut cw_skip = CmsgWriter::new(&mut skip_buf);
            kassert!(cw_skip.push_raw(net::IPPROTO_IP as i32, 1, &[1u8; 4]));
            let (skip_len, _) = cw_skip.finish();
            copy_to_user(ctl_va, &skip_buf[..skip_len]).map_err(|_| "copy skip cmsg")?;
            copy_to_user(buf_va, b"z").map_err(|_| "copy z")?;
            one_iov[0..8].copy_from_slice(&buf_va.to_le_bytes());
            one_iov[8..16].copy_from_slice(&1u64.to_le_bytes());
            copy_to_user(iov_va, &one_iov).map_err(|_| "copy one_iov")?;
            let skip_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: skip_len as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &skip_hdr.to_bytes()).map_err(|_| "copy skip hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            // Drain the byte sent above.
            kassert_eq!(syscall(general::__NR_recvfrom, [dfd, out_va, 1, 0, 0, 0]), 1);

            // send() returning Err with SCM_RIGHTS drops unconsumed rights cleanly.
            let mut one_r = [0u8; 32];
            let mut cw_one = CmsgWriter::new(&mut one_r);
            kassert_eq!(cw_one.push_rights(&[good_fd], 1), 1);
            let (one_r_len, _) = cw_one.finish();
            copy_to_user(ctl_va, &one_r[..one_r_len]).map_err(|_| "copy one_r")?;
            let fail_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: one_r_len as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &fail_hdr.to_bytes()).map_err(|_| "copy fail_hdr")?;
            dgram.state.lock().fail_next_send = Some(errno::EAGAIN);
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EAGAIN));
            // Closing good_fd now fires its release immediately (proving neither [good, 99], >253, nor failed send leaked a ref).
            kassert_eq!(syscall(general::__NR_close, [good_fd as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(good_sock.releases.load(Ordering::SeqCst), 1);

            // 10. SCM_RIGHTS + SCM_CREDENTIALS: in-flight lifetime, EMFILE MSG_CTRUNC, MSG_CMSG_CLOEXEC echo,
            //     short control buffer (2 rights into cmsg_space(4)), msg_control == NULL, faulting msg_control
            //     (returns bytes + MSG_CTRUNC, no EFAULT), faulting msg_name (returns -EFAULT).
            let passed_a = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let passed_b = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let fa = new_socket_file(Arc::clone(&passed_a) as Arc<dyn SockOps>, false).map_err(|_| "fa")?;
            let fb = new_socket_file(Arc::clone(&passed_b) as Arc<dyn SockOps>, false).map_err(|_| "fb")?;
            let id_a = fa.id();
            let fd_a = process::install_current_fd(fa, false).map_err(|_| "install fa")?;
            let fd_b = process::install_current_fd(fb, false).map_err(|_| "install fb")?;

            let mut cbuf = [0u8; 128];
            let mut cw = CmsgWriter::new(&mut cbuf);
            let send_cred = Ucred { pid: pid as i32, uid: 0, gid: 0 };
            kassert!(cw.push_creds(send_cred));
            kassert_eq!(cw.push_rights(&[fd_a, fd_b], 2), 2);
            let (clen, _) = cw.finish();
            copy_to_user(ctl_va, &cbuf[..clen]).map_err(|_| "copy scm")?;
            copy_to_user(buf_va, b"x").map_err(|_| "copy x")?;
            let send_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: clen as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &send_hdr.to_bytes()).map_err(|_| "copy send_hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            kassert_eq!(dgram.state.lock().last_send_creds, Some(send_cred));

            // Close both originals: InFlightFile keeps end_refs > 0, so release() has NOT run.
            kassert_eq!(syscall(general::__NR_close, [fd_a as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [fd_b as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 0);
            kassert_eq!(passed_b.releases.load(Ordering::SeqCst), 0);

            // Lower RLIMIT_NOFILE so only 1 of the 2 rights fits: installs 1 with FD_CLOEXEC,
            // echoes MSG_CMSG_CLOEXEC in msg_flags (Linux v6.12 net/socket.c:2815, 2839),
            // sets MSG_CTRUNC, and drops the second right (firing passed_b.release()).
            let open_now = process::process_open_fds(pid).len() as u64;
            let orig_rlim = proc
                .rlimit(RlimitKind::Nofile, Some(Rlimit::new(open_now + 1, 1024).ok_or("rlim+1")?))
                .map_err(|_| "set rlim+1")?;
            let recv_cap = cmsg_space(12).unwrap() + cmsg_space(8).unwrap();
            let recv_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: recv_cap as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &recv_hdr.to_bytes()).map_err(|_| "copy recv_hdr")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, u64::from(MSG_CMSG_CLOEXEC), 0, 0, 0]), 1);
            let _ = proc.rlimit(RlimitKind::Nofile, Some(orig_rlim));

            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read out hdr")?;
            let out_hdr = Msghdr::from_bytes(&out_hdr_raw);
            kassert!(out_hdr.msg_flags & MSG_CTRUNC != 0);
            kassert!(out_hdr.msg_flags & MSG_CMSG_CLOEXEC != 0);
            let expected_ctl_len = cmsg_space(12).unwrap() + cmsg_space(4).unwrap();
            kassert_eq!(out_hdr.msg_controllen, expected_ctl_len as u64);
            let mut got_ctl = alloc::vec![0u8; out_hdr.msg_controllen as usize];
            copy_from_user(&mut got_ctl, ctl_va).map_err(|_| "read got ctl")?;
            let mut got_creds = None;
            let mut got_fds = Vec::new();
            for item in CmsgIter::new(&got_ctl) {
                let cmsg = item.map_err(|_| "parse recv cmsg")?;
                if cmsg.level == SOL_SOCKET && cmsg.cmsg_type == SCM_CREDENTIALS {
                    got_creds = Some(cmsg.decode_creds().map_err(|_| "decode recv creds")?);
                } else if cmsg.level == SOL_SOCKET && cmsg.cmsg_type == SCM_RIGHTS {
                    got_fds.extend(cmsg.rights_fds().map_err(|_| "decode recv rights")?);
                }
            }
            kassert_eq!(got_creds, Some(send_cred));
            kassert_eq!(got_fds.len(), 1);
            let recv_fd = got_fds[0];
            kassert_eq!(
                syscall(general::__NR_fcntl, [recv_fd as u64, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                u64::from(general::FD_CLOEXEC)
            );
            let rf = process::current_io_file(recv_fd).ok_or("recv_fd missing")?;
            kassert_eq!(rf.id(), id_a);
            drop(rf);
            kassert_eq!(passed_b.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 0);
            kassert_eq!(syscall(general::__NR_close, [recv_fd as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 1);

            // Short control buffer: on x86-64 `CMSG_SPACE(4)` is 24 bytes (which holds 2 fds per
            // Linux v6.12 `scm_max_fds`, net/core/scm.c:317: `(24 - 16) / 4 == 2`), while `CMSG_LEN(4)`
            // is 20 bytes (`(20 - 16) / 4 == 1`). Receiving 2 rights into `cmsg_len(4)` installs 1
            // (with FD_CLOEXEC clear when MSG_CMSG_CLOEXEC is unset), sets MSG_CTRUNC, reports exact
            // `msg_controllen == cmsg_len(4)`, and drops/releases the 2nd right.
            let short_a = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let short_b = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let sfa = process::install_current_fd(
                new_socket_file(Arc::clone(&short_a) as Arc<dyn SockOps>, false).map_err(|_| "sfa")?,
                false,
            )
            .map_err(|_| "sfa fd")?;
            let sfb = process::install_current_fd(
                new_socket_file(Arc::clone(&short_b) as Arc<dyn SockOps>, false).map_err(|_| "sfb")?,
                false,
            )
            .map_err(|_| "sfb fd")?;
            let mut cw_short = CmsgWriter::new(&mut cbuf2);
            kassert_eq!(cw_short.push_rights(&[sfa, sfb], 2), 2);
            let (short_send_len, _) = cw_short.finish();
            copy_to_user(ctl_va, &cbuf2[..short_send_len]).map_err(|_| "copy short send")?;
            let short_send_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: short_send_len as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &short_send_hdr.to_bytes()).map_err(|_| "copy short send hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            kassert_eq!(syscall(general::__NR_close, [sfa as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [sfb as u64, 0, 0, 0, 0, 0]), 0);

            let one_fd_cap = cmsg_len(4).unwrap();
            let short_recv_hdr = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: one_fd_cap as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &short_recv_hdr.to_bytes()).map_err(|_| "copy short recv hdr")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read short out hdr")?;
            let short_out = Msghdr::from_bytes(&out_hdr_raw);
            kassert!(short_out.msg_flags & MSG_CTRUNC != 0);
            kassert_eq!(short_out.msg_flags & MSG_CMSG_CLOEXEC, 0);
            kassert_eq!(short_out.msg_controllen, one_fd_cap as u64);
            let mut short_got = alloc::vec![0u8; one_fd_cap];
            copy_from_user(&mut short_got, ctl_va).map_err(|_| "read short ctl")?;
            let mut short_fds = Vec::new();
            for item in CmsgIter::new(&short_got) {
                let cmsg = item.map_err(|_| "parse short cmsg")?;
                if cmsg.level == SOL_SOCKET && cmsg.cmsg_type == SCM_RIGHTS {
                    short_fds.extend(cmsg.rights_fds().map_err(|_| "decode short fds")?);
                }
            }
            kassert_eq!(short_fds.len(), 1);
            kassert_eq!(
                syscall(general::__NR_fcntl, [short_fds[0] as u64, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                0
            );
            kassert_eq!(short_b.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(short_a.releases.load(Ordering::SeqCst), 0);
            kassert_eq!(syscall(general::__NR_close, [short_fds[0] as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(short_a.releases.load(Ordering::SeqCst), 1);

            // msg_control == NULL with rights pending: MSG_CTRUNC, msg_controllen == 0, rights released.
            let null_ctl_sock = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let nc_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&null_ctl_sock) as Arc<dyn SockOps>, false).map_err(|_| "nc")?,
                false,
            )
            .map_err(|_| "nc fd")?;
            let mut cw_nc = CmsgWriter::new(&mut one_r);
            kassert_eq!(cw_nc.push_rights(&[nc_fd], 1), 1);
            let (nc_len, _) = cw_nc.finish();
            copy_to_user(ctl_va, &one_r[..nc_len]).map_err(|_| "copy nc")?;
            let nc_send = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: ctl_va,
                msg_controllen: nc_len as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &nc_send.to_bytes()).map_err(|_| "copy nc send")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            kassert_eq!(syscall(general::__NR_close, [nc_fd as u64, 0, 0, 0, 0, 0]), 0);
            let nc_recv = Msghdr { msg_iov: iov_va, msg_iovlen: 1, ..Default::default() };
            copy_to_user(msg_va, &nc_recv.to_bytes()).map_err(|_| "copy nc recv")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read nc out")?;
            let nc_out = Msghdr::from_bytes(&out_hdr_raw);
            kassert!(nc_out.msg_flags & MSG_CTRUNC != 0);
            kassert_eq!(nc_out.msg_controllen, 0);
            kassert_eq!(null_ctl_sock.releases.load(Ordering::SeqCst), 1);

            // Faulting non-NULL msg_control does NOT fail recvmsg (Linux v6.12 `____sys_recvmsg`
            // net/socket.c:2800, `put_cmsg` net/core/scm.c:231): sets MSG_CTRUNC, msg_controllen == 0,
            // drops reserved slots & rights, returns byte count. Bad msg_name still gives -EFAULT (net/socket.c:2832).
            let fault_ctl_sock = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let fc_fd = process::install_current_fd(
                new_socket_file(Arc::clone(&fault_ctl_sock) as Arc<dyn SockOps>, false).map_err(|_| "fc")?,
                false,
            )
            .map_err(|_| "fc fd")?;
            let mut cw_fc = CmsgWriter::new(&mut one_r);
            kassert_eq!(cw_fc.push_rights(&[fc_fd], 1), 1);
            let (fc_len, _) = cw_fc.finish();
            copy_to_user(ctl_va, &one_r[..fc_len]).map_err(|_| "copy fc")?;
            copy_to_user(msg_va, &nc_send.to_bytes()).map_err(|_| "copy fc send")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            kassert_eq!(syscall(general::__NR_close, [fc_fd as u64, 0, 0, 0, 0, 0]), 0);
            let open_before_fc = process::process_open_fds(pid).len();
            let fc_recv = Msghdr {
                msg_iov: iov_va,
                msg_iovlen: 1,
                msg_control: 0x1000,
                msg_controllen: one_fd_cap as u64,
                ..Default::default()
            };
            copy_to_user(msg_va, &fc_recv.to_bytes()).map_err(|_| "copy fc recv")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read fc out")?;
            let fc_out = Msghdr::from_bytes(&out_hdr_raw);
            kassert!(fc_out.msg_flags & MSG_CTRUNC != 0);
            kassert_eq!(fc_out.msg_controllen, 0);
            kassert_eq!(process::process_open_fds(pid).len(), open_before_fc);
            kassert_eq!(fault_ctl_sock.releases.load(Ordering::SeqCst), 1);

            kassert_eq!(syscall(general::__NR_sendto, [dfd, buf_va, 1, 0, 0, 0]), 1);
            let bad_rname_hdr =
                Msghdr { msg_name: 0x1000, msg_namelen: 16, msg_iov: iov_va, msg_iovlen: 1, ..Default::default() };
            copy_to_user(msg_va, &bad_rname_hdr.to_bytes()).map_err(|_| "copy bad rname")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EFAULT));

            // 11. flock leak regression (Linux v6.12 fs/file_table.c `__fput` -> fs/locks.c `locks_remove_file`):
            //     flock(LOCK_EX), send via SCM_RIGHTS, close original, drop right uninstalled, then
            //     flock(LOCK_EX | LOCK_NB) on another open description of the same inode succeeds.
            let flock_path = b"/tmp/sock-flock-ktest\0";
            copy_to_user(path_va, flock_path).map_err(|_| "copy flock path")?;
            let f1 = syscall(
                general::__NR_openat,
                [
                    general::AT_FDCWD as u64,
                    path_va,
                    u64::from(general::O_CREAT | general::O_RDWR | general::O_TRUNC),
                    0o600,
                    0,
                    0,
                ],
            );
            kassert!((f1 as i64) >= 0);
            let f2 =
                syscall(general::__NR_openat, [general::AT_FDCWD as u64, path_va, u64::from(general::O_RDWR), 0, 0, 0]);
            kassert!((f2 as i64) >= 0);
            kassert_eq!(syscall(general::__NR_unlinkat, [general::AT_FDCWD as u64, path_va, 0, 0, 0, 0]), 0);

            let lock_ex = u64::from(general::LOCK_EX);
            let lock_nb = u64::from(general::LOCK_NB);
            kassert_eq!(syscall(general::__NR_flock, [f1, lock_ex, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_flock, [f2, lock_ex | lock_nb, 0, 0, 0, 0]), neg(errno::EWOULDBLOCK));

            let mut cw_fl = CmsgWriter::new(&mut one_r);
            kassert_eq!(cw_fl.push_rights(&[f1 as i32], 1), 1);
            let (fl_len, _) = cw_fl.finish();
            copy_to_user(ctl_va, &one_r[..fl_len]).map_err(|_| "copy fl cmsg")?;
            copy_to_user(msg_va, &nc_send.to_bytes()).map_err(|_| "copy fl send")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            kassert_eq!(syscall(general::__NR_close, [f1, 0, 0, 0, 0, 0]), 0);
            // Still held by the in-flight InFlightFile until dropped uninstalled.
            kassert_eq!(syscall(general::__NR_flock, [f2, lock_ex | lock_nb, 0, 0, 0, 0]), neg(errno::EWOULDBLOCK));
            copy_to_user(msg_va, &nc_recv.to_bytes()).map_err(|_| "copy fl recv")?;
            kassert_eq!(syscall(general::__NR_recvmsg, [dfd, msg_va, 0, 0, 0, 0]), 1);
            // Uninstalled right was dropped -> final fput released flock -> LOCK_EX|LOCK_NB now succeeds!
            kassert_eq!(syscall(general::__NR_flock, [f2, lock_ex | lock_nb, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [f2, 0, 0, 0, 0, 0]), 0);

            // 12. UserIovec::new truncates total to MAX_RW_COUNT (Linux v6.12 `import_iovec`),
            //     and InFlightFile::file() exposes `&KernelFile`.
            let big_spans = [UserSpan { base: 0x1000, len: MAX_RW_COUNT }, UserSpan { base: 0x2000, len: 4096 }];
            kassert_eq!(UserIovec::new(&big_spans).total_len(), MAX_RW_COUNT);
            let inflight_check = InFlightFile::from_fd(sfd as i32).map_err(|_| "inflight_check")?;
            kassert_eq!(inflight_check.file().id(), sfd_id);
            drop(inflight_check);

            // 13. block_on: deterministic lost-wake ordering seam, wakeup via KtestThread::settle,
            //     timeout (-EAGAIN), signal with timeout (-EINTR), signal without (-ERESTARTSYS).
            let lw_ready = Arc::new(AtomicBool::new(false));
            let lw_ready_hook = Arc::clone(&lw_ready);
            let lw_stream_hook = Arc::clone(&stream);
            {
                let _arm = block_on_window::arm(move || {
                    lw_ready_hook.store(true, Ordering::SeqCst);
                    lw_stream_hook.poll_head.notify(PollMask::IN);
                });
                kassert_eq!(block_on(&*stream, None, || lw_ready.load(Ordering::SeqCst)), Ok(()));
            }
            kassert_eq!(stream.poll_head.subscriber_count(), 0);

            let ready_flag = Arc::new(AtomicBool::new(false));
            let ready_w = Arc::clone(&ready_flag);
            let stream_w = Arc::clone(&stream);
            let blocker = process::spawn_test_thread(move |_| {
                let r = block_on(&*stream_w, None, || ready_w.load(Ordering::SeqCst));
                if r.is_ok() { 0 } else { 1 }
            })?;
            kassert!(blocker.settle());
            kassert_eq!(stream.poll_head.subscriber_count(), 1);
            ready_flag.store(true, Ordering::SeqCst);
            stream.poll_head.notify(PollMask::IN);
            kassert_eq!(blocker.join()?, 0);
            kassert_eq!(stream.poll_head.subscriber_count(), 0);

            // Timeout -> -EAGAIN.
            let dl = time::now_ns().saturating_add(2_000_000);
            kassert_eq!(block_on(&*stream, Some(dl), || false), Err(errno::EAGAIN));

            // Signal with timeout -> -EINTR; without timeout -> -ERESTARTSYS.
            let me = sched::current().ok_or("current thread")?;
            process::set_current_action(Signal::USR1, KernelSigaction { handler: 1, ..KernelSigaction::default() });
            raise_for_test(&me, Signal::USR1);
            let far_dl = time::now_ns().saturating_add(10_000_000_000);
            kassert_eq!(block_on(&*stream, Some(far_dl), || false), Err(errno::EINTR));
            kassert_eq!(block_on(&*stream, None, || false), Err(ERESTARTSYS));

            // Clear test-raised pending signal before process exit.
            let mut dummy = crate::arch::syscall::SyscallFrame::default();
            crate::linux::signal::exit_to_user(&mut dummy, None);

            kassert_eq!(syscall(general::__NR_close, [sfd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [dfd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [qfd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(dgram.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(seq.releases.load(Ordering::SeqCst), 1);
            Ok(())
        })?
    }
);
