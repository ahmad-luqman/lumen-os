//! Generic socket file object, `sockfs` (dev `0:8`), `InFlightFile`, and
//! `block_on` (M9 B1, #472; ARCHITECTURE sections 8, 9 and 10).
//!
//! Every socket family implements [`SockOps`], attached once to a
//! [`KernelFile`] via `FileObj::Sock(Arc<dyn SockOps>)` and reached through
//! [`KernelFile::pollable`] and [`KernelFile::release_end`]. All socket
//! syscalls in `linux::syscall::socket` dispatch generically over [`SockOps`].

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_sockabi::{Family, OptVal, SockAddr, SockOpt, Ucred};
use lumen_vfs::{
    AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, PollGuard, PollMask, PollWaker,
    Pollable, SetTime, Timespec, VfsError, VfsInode, mode_after_chown, next_lock_id,
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
    /// Wrap a validated slice of spans whose total length fits in `usize`.
    pub fn new(spans: &'a [UserSpan]) -> Self {
        let total = spans.iter().fold(0usize, |acc, s| acc.saturating_add(s.len));
        Self { spans, total }
    }

    /// Underlying user spans.
    #[allow(dead_code)] // used by family implementations
    pub const fn spans(&self) -> &'a [UserSpan] {
        self.spans
    }

    /// Total byte length across all spans.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by family implementations (C1/C3/C4/C6)
    pub const fn total_len(&self) -> usize {
        self.total
    }

    /// True if all spans together have zero bytes.
    #[allow(dead_code)] // used by family implementations
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Copy up to `dst.len()` bytes from user space starting at byte `offset`
    /// across the spans into `dst`.
    ///
    /// Returns the number of bytes copied (`min(dst.len(), total_len - offset)`),
    /// or `Err(EFAULT)` if any touched user address faults or overflows.
    #[allow(dead_code)] // used by family implementations and ktests
    pub fn copy_from_user(&self, mut offset: usize, mut dst: &mut [u8]) -> Result<usize, u32> {
        if dst.is_empty() || offset >= self.total {
            return Ok(0);
        }
        let mut copied = 0usize;
        for span in self.spans {
            if span.len == 0 {
                continue;
            }
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
    pub fn copy_to_user_partial(&self, mut offset: usize, mut src: &[u8]) -> usize {
        if src.is_empty() || offset >= self.total {
            return 0;
        }
        let mut copied = 0usize;
        for span in self.spans {
            if span.len == 0 {
                continue;
            }
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
/// (`inc_fd_ref`) while it lives, and `Drop` runs `dec_fd_ref`, so it is
/// dropped only outside every lock (ARCHITECTURE section 9's end-release rule).
pub struct InFlightFile(FileRef);

impl InFlightFile {
    /// Resolve `fd` in the current process's FD table into an [`InFlightFile`],
    /// raising its counted FD/end reference under `Files::table`.
    pub fn from_fd(fd: i32) -> Result<Self, u32> {
        process::clone_current_fd_ref(fd).map(Self).ok_or(errno::EBADF)
    }

    /// Borrow the underlying open file description.
    #[allow(dead_code)] // used by C2 (`unix_gc`) and ktests
    pub fn file(&self) -> &FileRef {
        &self.0
    }

    /// Transfer this in-flight object's counted FD/end reference into a raw
    /// [`FileRef`] for installation via `ReservedFd::install_ref`.
    ///
    /// Raises the reference count before dropping `self` so `dec_fd_ref` cannot
    /// transition `end_refs` to zero during the transfer.
    pub fn into_file_ref(self) -> FileRef {
        let file = Arc::clone(&self.0);
        file.inc_fd_ref();
        drop(self);
        file
    }
}

impl Clone for InFlightFile {
    fn clone(&self) -> Self {
        self.0.inc_fd_ref();
        Self(Arc::clone(&self.0))
    }
}

// Dropping runs `dec_fd_ref`, which may trigger `release_end` and take Lock 00/0:
// never drop an `InFlightFile` while holding any lock 00–10.
impl Drop for InFlightFile {
    fn drop(&mut self) {
        self.0.dec_fd_ref();
    }
}

/// One send's user side, decoded by the handler: spans checked
/// (`UIO_MAXIOV`), a datagram socket's destination decoded by
/// lumen-sockabi's `SockAddr::decode_dest`, and the control buffer (at most
/// 131072 bytes, Linux's `optmem_max` since 6.8; `-ENOBUFS` past it) walked
/// with `CmsgIter`: `SCM_RIGHTS` resolved to `InFlightFile`s,
/// `SCM_CREDENTIALS` decoded, messages at levels other than `SOL_SOCKET`
/// skipped, an unknown `SOL_SOCKET` type `-EINVAL`.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by family SockOps implementations (C1/C3/C4/C6)
pub struct SendIo<'a> {
    pub iov: UserIovec<'a>,
    pub to: Option<SockAddr>,
    pub rights: Vec<InFlightFile>,
    pub creds: Option<Ucred>,
    pub flags: u32,
    pub nonblock: bool,
}

/// One receive's user side: spans, room for descriptors (from `msg_controllen`),
/// flags.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by family SockOps implementations (C1/C3/C4/C6)
pub struct RecvIo<'a> {
    pub iov: UserIovec<'a>,
    pub max_fds: usize,
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
    /// Disconnect a socket (`connect(fd, { sa_family: AF_UNSPEC }, >= 2)`).
    /// Datagram families override this (C1, C3); stream and raw sockets default
    /// to `-EAFNOSUPPORT`.
    fn disconnect(&self) -> Result<(), u32> {
        Err(errno::EAFNOSUPPORT)
    }
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
    let waker: Arc<dyn PollWaker> = Arc::clone(&waiter) as Arc<dyn PollWaker>;
    // Subscribe before re-checking `ready()` so no wakeup between the check
    // and the sleep can be missed; `_guard` stays held until `block_on` exits.
    let (_mask, _guard): (PollMask, Option<PollGuard>) = src.poll(AccessMode::ReadWrite, Some(&waker));

    loop {
        // Clear `fired` BEFORE checking `ready()` so any notification arriving
        // during or after `ready()` sets `fired` again and wakes the wait.
        waiter.fired.store(false, Ordering::SeqCst);
        if ready() {
            return Ok(());
        }
        if let Some(dl) = deadline
            && time::now_ns() >= dl
        {
            return Err(errno::EAGAIN);
        }
        match waiter.queue.wait_interruptible(deadline, || waiter.fired.load(Ordering::SeqCst)) {
            sched::Wake::Woken => {}
            sched::Wake::TimedOut => {
                return if ready() { Ok(()) } else { Err(errno::EAGAIN) };
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
/// `socket:[ino]`, `O_RDWR | O_NONBLOCK?`) backed by `ops`.
pub fn new_socket_file(ops: Arc<dyn SockOps>, nonblock: bool) -> Result<KernelFile, u32> {
    let ino = NEXT_SOCK_INO.fetch_add(1, Ordering::Relaxed);
    let inode: Arc<dyn VfsInode> = Arc::new(SockInode::new(ino));
    let path: Arc<[u8]> = Arc::from(format!("socket:[{ino}]").into_bytes());
    let nb = if nonblock { general::O_NONBLOCK } else { 0 };
    let flags = OpenFlags::from_bits(general::O_RDWR | nb);
    KernelFile::new_sock(inode, path, flags, ops).map_err(VfsError::to_errno)
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
        pub last_send_creds: Option<Ucred>,
        pub last_recv_flags: Option<u32>,
        pub last_recv_nonblock: Option<bool>,
        pub last_recv_max_fds: Option<usize>,
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
                last_send_creds: None,
                last_recv_flags: None,
                last_recv_nonblock: None,
                last_recv_max_fds: None,
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
                st.last_send_creds = io.creds;
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
                st.last_recv_max_fds = Some(io.max_fds);
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

    pub fn wait_for(mut cond: impl FnMut() -> bool) -> Result<(), &'static str> {
        let deadline = time::now_ns().saturating_add(2_000_000_000);
        while !cond() {
            if time::now_ns() >= deadline {
                return Err("timed out waiting for condition");
            }
            sched::yield_now();
        }
        Ok(())
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

            // 2. Bad type flags -> -EINVAL, bad types -> -ESOCKTNOSUPPORT / -EINVAL, bad protocols -> -EPROTONOSUPPORT.
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
                syscall(general::__NR_socket, [u64::from(net::AF_INET), u64::from(net::SOCK_SEQPACKET), 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_INET6), u64::from(net::SOCK_SEQPACKET), 0, 0, 0, 0]),
                neg(errno::ESOCKTNOSUPPORT)
            );
            kassert_eq!(syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 15, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), 4, 0, 0, 0, 0]),
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
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 1, 0, 0, 0]),
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
            //    /proc/<pid>/fd socket:[ino], FIONBIO, fcntl, lseek, mmap, and end release.
            FACTORY_CALLS.store(0, Ordering::SeqCst);
            FACTORY_RELEASES.store(0, Ordering::SeqCst);
            let _factory = ktest_factory::FactoryGuard::install(Some(counting_single), Some(counting_pair));

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

            // /proc/<pid>/fd/<fd> reports `socket:[<st_ino>]` via both procfs readlinkat and process_fd_target.
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

            // socketpair: flags, distinct socket:[ino], and bad sv drops both reservations and releases both sockets.
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

            let open_before = process::process_open_fds(pid).len();
            let rel_before_fault = FACTORY_RELEASES.load(Ordering::SeqCst);
            kassert_eq!(
                syscall(general::__NR_socketpair, [u64::from(net::AF_UNIX), u64::from(net::SOCK_DGRAM), 0, 0, 0, 0]),
                neg(errno::EFAULT)
            );
            kassert_eq!(process::process_open_fds(pid).len(), open_before);
            kassert_eq!(FACTORY_RELEASES.load(Ordering::SeqCst), rel_before_fault + 2);

            // Slot reservation before family creation: -EMFILE never invokes the factory.
            let proc = process::current_process();
            let open_now = process::process_open_fds(pid).len() as u64;
            let orig_rlim = proc
                .rlimit(RlimitKind::Nofile, Some(Rlimit::new(open_now, 1024).ok_or("rlimit")?))
                .map_err(|_| "set rlimit")?;
            let calls_before = FACTORY_CALLS.load(Ordering::SeqCst);
            kassert_eq!(
                syscall(general::__NR_socket, [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, 0, 0, 0]),
                neg(errno::EMFILE)
            );
            kassert_eq!(
                syscall(
                    general::__NR_socketpair,
                    [u64::from(net::AF_UNIX), u64::from(net::SOCK_STREAM), 0, sv_va, 0, 0]
                ),
                neg(errno::EMFILE)
            );
            kassert_eq!(FACTORY_CALLS.load(Ordering::SeqCst), calls_before);
            let _ = proc.rlimit(RlimitKind::Nofile, Some(orig_rlim));

            // Non-socket fd -> -ENOTSOCK; closed fd -> -EBADF.
            kassert_eq!(syscall(general::__NR_listen, [0, 8, 0, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_shutdown, [0, 0, 0, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_getsockname, [0, stat_va, int_va, 0, 0, 0]), neg(errno::ENOTSOCK));
            kassert_eq!(syscall(general::__NR_listen, [99, 8, 0, 0, 0, 0]), neg(errno::EBADF));
            kassert_eq!(syscall(general::__NR_connect, [99, stat_va, 16, 0, 0, 0]), neg(errno::EBADF));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn socket_io_and_poll_reach_sockops() {
        use core::sync::atomic::{AtomicBool, Ordering};

        use crate::linux::process::{KernelSigaction, Rlimit, RlimitKind};
        use crate::linux::signal::raise_for_test;
        use crate::linux::{errno, general, ioctl, net};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use ktest_support::{MockSock, neg, syscall, wait_for};
        use lumen_signal::Signal;
        use lumen_sockabi::abi::Msghdr;
        use lumen_sockabi::{
            AF_UNSPEC, CMSG_HDRLEN, CmsgIter, CmsgWriter, Ipv4Addr, Linger, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT,
            MSG_ERRQUEUE, MSG_OOB, SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET, SocketAddrV4, Timeout, UnixName,
            cmsg_space,
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

            // Install a stream MockSock and a datagram MockSock.
            let stream = MockSock::new(Family::Inet, SockKind::Stream, net::IPPROTO_TCP as i32);
            let stream_file = new_socket_file(Arc::clone(&stream) as Arc<dyn SockOps>, false).map_err(|_| "stream")?;
            let sfd = process::install_current_fd(stream_file, false).map_err(|_| "install sfd")? as u64;

            let dgram = MockSock::new(Family::Unix, SockKind::Dgram, 0);
            let dgram_file = new_socket_file(Arc::clone(&dgram) as Arc<dyn SockOps>, false).map_err(|_| "dgram")?;
            let dfd = process::install_current_fd(dgram_file, false).map_err(|_| "install dfd")? as u64;

            // 1. write -> SockOps::send(flags=0), FIONREAD -> SockOps::fionread, read -> SockOps::recv(flags=0).
            copy_to_user(buf_va, b"hello").map_err(|_| "copy hello")?;
            kassert_eq!(syscall(general::__NR_write, [sfd, buf_va, 5, 0, 0, 0]), 5);
            kassert_eq!(stream.state.lock().send_calls, 1);
            kassert_eq!(stream.state.lock().last_send_flags, Some(0));

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

            kassert_eq!(syscall(general::__NR_read, [sfd, out_va, 5, 0, 0, 0]), 5);
            let mut got5 = [0u8; 5];
            copy_from_user(&mut got5, out_va).map_err(|_| "read out5")?;
            kassert_eq!(&got5, b"hello");
            kassert_eq!(stream.state.lock().recv_calls, 1);
            kassert_eq!(stream.state.lock().last_recv_flags, Some(0));

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

            // 3. poll reaches SockOps::poll both immediately and via blocking wakeup.
            stream.set_mask_and_notify(PollMask::EMPTY);
            let mut pfd = [0u8; 8];
            pfd[0..4].copy_from_slice(&(sfd as i32).to_le_bytes());
            pfd[4..6].copy_from_slice(&(PollMask::IN.bits() as i16).to_le_bytes());
            copy_to_user(poll_va, &pfd).map_err(|_| "copy pfd")?;
            kassert_eq!(syscall(general::__NR_poll, [poll_va, 1, 0, 0, 0, 0]), 0);

            let waiter = process::spawn_test_thread(move |_| {
                syscall(general::__NR_poll, [poll_va, 1, (-1i64) as u64, 0, 0, 0])
            })?;
            wait_for(|| stream.poll_head.subscriber_count() == 1 && waiter.blocked())?;
            stream.set_mask_and_notify(PollMask::IN | PollMask::RDNORM);
            kassert_eq!(waiter.join()?, 1);
            copy_from_user(&mut pfd, poll_va).map_err(|_| "read revents")?;
            kassert_eq!(u16::from_le_bytes([pfd[6], pfd[7]]), PollMask::IN.bits());

            // 4. bind, listen, accept4, connect (and AF_UNSPEC disconnect), shutdown, getsockname, getpeername.
            let inet_addr = SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080));
            let mut enc = [0u8; 128];
            let enc_len = inet_addr.encode(&mut enc);
            copy_to_user(addr_va, &enc[..enc_len]).map_err(|_| "copy inet_addr")?;
            kassert_eq!(syscall(general::__NR_bind, [sfd, addr_va, enc_len as u64, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_bind, Some(inet_addr));

            kassert_eq!(syscall(general::__NR_listen, [sfd, 16, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_listen, Some(16));
            // listen on a datagram socket is -EOPNOTSUPP.
            kassert_eq!(syscall(general::__NR_listen, [dfd, 16, 0, 0, 0, 0]), neg(errno::EOPNOTSUPP));

            // getsockname: truncated buffer writes full length back.
            copy_to_user(len_va, &4i32.to_le_bytes()).map_err(|_| "copy len 4")?;
            kassert_eq!(syscall(general::__NR_getsockname, [sfd, out_va, len_va, 0, 0, 0]), 0);
            copy_from_user(&mut n_raw, len_va).map_err(|_| "read out len")?;
            kassert_eq!(i32::from_le_bytes(n_raw), enc_len as i32);

            // getpeername before connect -> -ENOTCONN; after connect -> peer address; AF_UNSPEC -> disconnect.
            copy_to_user(len_va, &16i32.to_le_bytes()).map_err(|_| "copy len 16")?;
            kassert_eq!(syscall(general::__NR_getpeername, [sfd, out_va, len_va, 0, 0, 0]), neg(errno::ENOTCONN));
            kassert_eq!(syscall(general::__NR_connect, [sfd, addr_va, enc_len as u64, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_getpeername, [sfd, out_va, len_va, 0, 0, 0]), 0);
            copy_to_user(addr_va, &AF_UNSPEC.to_le_bytes()).map_err(|_| "copy AF_UNSPEC")?;
            kassert_eq!(syscall(general::__NR_connect, [sfd, addr_va, 2, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().disconnects, 1);

            // accept4 reserves slot first (-EMFILE leaves pending connection untouched), then installs peer with flags.
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
            kassert_eq!(syscall(general::__NR_close, [afd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(peer_sock.releases.load(Ordering::SeqCst), 1);

            // shutdown valid and invalid.
            kassert_eq!(syscall(general::__NR_shutdown, [sfd, u64::from(net::SHUT_WR), 0, 0, 0, 0]), 0);
            kassert_eq!(stream.state.lock().last_shutdown, Some(ShutdownHow::Write));
            kassert_eq!(syscall(general::__NR_shutdown, [sfd, 3, 0, 0, 0, 0]), neg(errno::EINVAL));

            // 5. setsockopt / getsockopt.
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
            // Linger + non-SOL_SOCKET on AF_UNIX (-EOPNOTSUPP).
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
            kassert_eq!(
                syscall(
                    general::__NR_setsockopt,
                    [dfd, u64::from(net::SOL_TCP), u64::from(net::TCP_NODELAY), opt_va, 4, 0]
                ),
                neg(errno::EOPNOTSUPP)
            );

            // 6. sendto / recvfrom flags and destination decode.
            let unix_dst = SockAddr::Unix(UnixName::abstract_name(b"srv").map_err(|_| "unix name")?);
            let ulen = unix_dst.encode(&mut enc);
            copy_to_user(addr_va, &enc[..ulen]).map_err(|_| "copy unix dst")?;
            copy_to_user(buf_va, b"ping").map_err(|_| "copy ping")?;
            kassert_eq!(
                syscall(general::__NR_sendto, [dfd, buf_va, 4, u64::from(MSG_DONTWAIT), addr_va, ulen as u64]),
                4
            );
            kassert_eq!(dgram.state.lock().last_send_to, Some(unix_dst));
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
            copy_to_user(len_va, &110i32.to_le_bytes()).map_err(|_| "copy 110")?;
            kassert_eq!(syscall(general::__NR_recvfrom, [dfd, out_va, 4, 0, addr_va, len_va]), 4);

            // 7. sendmsg / recvmsg: UIO_MAXIOV+1 -> -EMSGSIZE, optmem_max+1 -> -ENOBUFS,
            //    SCM_RIGHTS (in-flight lifetime + MSG_CMSG_CLOEXEC + EMFILE MSG_CTRUNC), SCM_CREDENTIALS.
            let bad_iov_hdr =
                Msghdr { msg_iov: iov_va, msg_iovlen: u64::from(general::UIO_MAXIOV) + 1, ..Default::default() };
            copy_to_user(msg_va, &bad_iov_hdr.to_bytes()).map_err(|_| "copy bad_iov_hdr")?;
            kassert_eq!(syscall(general::__NR_sendmsg, [dfd, msg_va, 0, 0, 0, 0]), neg(errno::EMSGSIZE));

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

            // Send two socket descriptors in SCM_RIGHTS + valid SCM_CREDENTIALS; close originals while in flight.
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
            let mut one_iov = [0u8; 16];
            one_iov[0..8].copy_from_slice(&buf_va.to_le_bytes());
            one_iov[8..16].copy_from_slice(&1u64.to_le_bytes());
            copy_to_user(iov_va, &one_iov).map_err(|_| "copy one_iov")?;
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

            // Close both originals: InFlightFile keeps end_refs > 0, so release() has NOT run!
            kassert_eq!(syscall(general::__NR_close, [fd_a as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [fd_b as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 0);
            kassert_eq!(passed_b.releases.load(Ordering::SeqCst), 0);

            // Lower RLIMIT_NOFILE so only 1 of the 2 rights fits: installs 1 with FD_CLOEXEC,
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

            let mut out_hdr_raw = [0u8; 56];
            copy_from_user(&mut out_hdr_raw, msg_va).map_err(|_| "read out hdr")?;
            let out_hdr = Msghdr::from_bytes(&out_hdr_raw);
            kassert!(out_hdr.msg_flags & MSG_CTRUNC != 0);
            kassert!(out_hdr.msg_controllen as usize >= CMSG_HDRLEN);
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
            // Second right was dropped on EMFILE, releasing passed_b immediately; passed_a releases on close.
            kassert_eq!(passed_b.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 0);
            kassert_eq!(syscall(general::__NR_close, [recv_fd as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(passed_a.releases.load(Ordering::SeqCst), 1);

            // 8. block_on: wakeup, timeout (-EAGAIN), signal with timeout (-EINTR), signal without (-ERESTARTSYS).
            let ready_flag = Arc::new(AtomicBool::new(false));
            let ready_w = Arc::clone(&ready_flag);
            let stream_w = Arc::clone(&stream);
            let blocker = process::spawn_test_thread(move |_| {
                let r = block_on(&*stream_w, None, || ready_w.load(Ordering::SeqCst));
                if r.is_ok() { 0 } else { 1 }
            })?;
            wait_for(|| stream.poll_head.subscriber_count() == 1 && blocker.blocked())?;
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

            // Verify opt_rcvtimeo round-trip wasn't disturbed.
            kassert_eq!(stream.state.lock().opt_rcvtimeo, Timeout::Infinite);

            kassert_eq!(syscall(general::__NR_close, [sfd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(syscall(general::__NR_close, [dfd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(stream.releases.load(Ordering::SeqCst), 1);
            kassert_eq!(dgram.releases.load(Ordering::SeqCst), 1);
            Ok(())
        })?
    }
);
