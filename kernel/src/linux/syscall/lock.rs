//! Descriptor control and advisory locks (M7, #254): `fcntl`, `flock`.
//!
//! The locks are lumen-vfs's `AdvisoryLocks`, one table per inode behind
//! its Lock 4 (`VfsInode::with_locks_mut`): `flock` locks keyed by the open
//! file description (so `dup`'d descriptors share them), POSIX record locks
//! keyed by the process's pid. A blocking request (`F_SETLKW`, `flock`
//! without `LOCK_NB`) that conflicts waits in
//! `process::wait_for_lock_release` and retries after any release: one
//! kernel-wide wait queue, woken by every call here that gave a lock up
//! (lumen-vfs says which: `released`) and by `close`, exit and `dup2`
//! displacement giving locks up (`process::release_fd`). `F_SETLKW` refuses
//! a wait that would close a cycle of waiting processes (`EDEADLK`, from
//! lumen-vfs's `PosixWaitGraph` in `POSIX_WAITERS`); a blocking `flock`
//! gives up the lock its description holds before it waits, as Linux. No
//! signal ends a wait (M7 delivers none, so no `EINTR`).

use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use super::at::{self, Fd, UserPtr};
use super::fd::vfs;
use super::finish;
use crate::linux::process::{self, FileRef, with_current};
use crate::linux::{errno, general};
use crate::sync::SpinLock;
use crate::user::{copy_from_user, copy_to_user};
use lumen_vfs::locks::{MAX_DEADLOCK_CHAIN, fcntl_const};
use lumen_vfs::{
    AdvisoryLocks, Escaped, FlockOp, FlockOutcome, FlockSpec, LockContext, PosixWaitGraph, RecordLockOutcome, VfsError,
    VfsLock, WaitSearch,
};

/// The size of Linux x86-64's `struct flock` (its offsets pinned by
/// `m7_abi_constants_match_spec`): `l_type` (`i16`) at 0, `l_whence`
/// (`i16`) at 2, `l_start` (`i64`) at 8, `l_len` (`i64`) at 16, `l_pid`
/// (`i32`) at 24, and padding at 4 and 28.
const FLOCK_SIZE: usize = size_of::<general::flock>();

/// A `struct flock` copied in: its bytes as they came (padding included,
/// so a copy back changes only the fields `F_GETLK` writes) and its fields.
struct UserFlock {
    raw: [u8; FLOCK_SIZE],
    spec: FlockSpec,
}

impl UserFlock {
    fn copy_in(at: UserPtr) -> Result<Self, u32> {
        let mut raw = [0u8; FLOCK_SIZE];
        copy_from_user(&mut raw, at.addr()).map_err(|e| e.to_errno())?;
        let i16_at = |o: usize| i16::from_le_bytes([raw[o], raw[o + 1]]);
        let i64_at = |o: usize| i64::from_le_bytes(raw[o..o + 8].try_into().expect("8 bytes"));
        let spec = FlockSpec {
            l_type: i16_at(0),
            l_whence: i16_at(2),
            l_start: i64_at(8),
            l_len: i64_at(16),
            l_pid: i32::from_le_bytes(raw[24..28].try_into().expect("4 bytes")),
        };
        Ok(Self { raw, spec })
    }

    fn copy_out(mut self, at: UserPtr) -> Result<(), u32> {
        let s = self.spec;
        self.raw[0..2].copy_from_slice(&s.l_type.to_le_bytes());
        self.raw[2..4].copy_from_slice(&s.l_whence.to_le_bytes());
        self.raw[8..16].copy_from_slice(&s.l_start.to_le_bytes());
        self.raw[16..24].copy_from_slice(&s.l_len.to_le_bytes());
        self.raw[24..28].copy_from_slice(&s.l_pid.to_le_bytes());
        copy_to_user(at.addr(), &self.raw).map_err(|e| e.to_errno())
    }
}

/// Where `spec`'s range is measured from, for lumen-vfs: `file`'s offset
/// (Lock 2, released before the inode's Lock 4 is taken) and, for
/// `SEEK_END` only, its size (`stat`, which takes Lock 4 itself). Read once
/// per call: a `F_SETLKW` keeps the range it was given while it waits.
fn lock_context(file: &FileRef, spec: &FlockSpec) -> Result<LockContext, u32> {
    let cur_offset = *file.offset.lock();
    let file_size = if spec.l_whence == fcntl_const::SEEK_END { file.inode.stat().map_err(vfs)?.size } else { 0 };
    Ok(LockContext { cur_offset, file_size, readable: file.is_readable(), writable: file.is_writable() })
}

/// Run `f` on `file`'s inode's lock table (its Lock 4): what `f` returned,
/// or the filesystem's refusal. `VfsInode::with_locks_mut` is `Err` only
/// if `f` didn't run; if `f` ran, its result is used whatever the call
/// said (a release it made must still wake the waiters), and a filesystem
/// breaking that contract is logged. One that says yes without running `f`
/// is a bug in it too: logged, and `EIO`.
fn with_locks<T>(file: &FileRef, mut f: impl FnMut(&mut AdvisoryLocks) -> Result<T, VfsError>) -> Result<T, u32> {
    let mut out = None;
    let called = file.inode.with_locks_mut(&mut |locks| out = Some(f(locks)));
    match (out, called) {
        (Some(out), Ok(())) => out.map_err(vfs),
        (Some(out), Err(e)) => {
            log::error!(target: "linux", "{}: with_locks_mut ran its closure but returned {e:?}", Escaped(&file.path));
            out.map_err(vfs)
        }
        (None, Err(e)) => Err(vfs(e)),
        (None, Ok(())) => {
            log::error!(target: "linux", "{}: with_locks_mut returned without running its closure", Escaped(&file.path));
            Err(errno::EIO)
        }
    }
}

/// One try at a lock, under the inode's lock.
enum Step<H> {
    /// Taken, changed or given up; `released`: the try gave up or weakened
    /// a lock (lumen-vfs's `released`), so the waiters must retry.
    Done { released: bool },
    /// Must wait, for `holder` (`F_SETLKW`'s pid; `()` for `flock`);
    /// `released` as for `Done` (a blocking `flock` drops its lock first).
    Wait { released: bool, holder: H },
}

/// Take a lock with `attempt`, waiting for a release and trying again while
/// it says `Wait`. `before_wait(holder, seen)` runs outside the inode's lock
/// before each wait and may refuse it (`EDEADLK`). The release count
/// (`seen`) is read before each try, so a release between a try and the
/// wait ends the wait at once. A try that released something wakes the
/// waiters, which retry (and wait again if they still conflict).
fn lock_or_wait<H: Copy>(
    file: &FileRef,
    mut attempt: impl FnMut(&mut AdvisoryLocks) -> Result<Step<H>, VfsError>,
    mut before_wait: impl FnMut(H, u64) -> Result<(), u32>,
) -> Result<(), u32> {
    loop {
        let seen = process::lock_releases();
        let step = with_locks(file, &mut attempt)?;
        #[cfg(feature = "ktest")]
        if matches!(step, Step::Wait { .. }) {
            ktest_window::hold();
        }
        let (Step::Done { released } | Step::Wait { released, .. }) = step;
        if released {
            process::locks_released();
        }
        match step {
            Step::Done { .. } => return Ok(()),
            Step::Wait { holder, .. } => {
                before_wait(holder, seen)?;
                process::wait_for_lock_release(seen);
            }
        }
    }
}

/// Who waits for whom among the processes blocked in `F_SETLKW` (keyed by
/// pid: from M8, two threads of one process would overwrite each other's
/// record). Not a leaf lock: a record's insert and removal allocate and
/// free, so the heap's locks nest inside it (ARCHITECTURE section 9, level
/// 10). Never taken under an inode's lock or another spinlock.
static POSIX_WAITERS: SpinLock<PosixWaitGraph> = SpinLock::new(PosixWaitGraph::new());

/// Record that `pid` is about to wait for `holder`'s POSIX lock, its try
/// having failed with the release count at `seen`, unless that closes a
/// cycle of waiting processes: `EDEADLK`, as Linux. lumen-vfs's
/// `PosixWaitGraph::wait_for` has the rules: one holder per waiter, as
/// Linux's single `fl_blocker`; a record older than the last release is
/// never followed; a caller whose own `seen` is stale is neither checked
/// nor recorded (`WaitSearch::StaleCaller`: its wait ends at once and it
/// retries). The count is read under `POSIX_WAITERS`, so of two processes
/// about to wait for each other with a current view, the second is refused.
fn record_wait_unless_deadlock(pid: u32, holder: u32, seen: u64) -> Result<WaitSearch, u32> {
    let mut graph = POSIX_WAITERS.lock();
    let now = process::lock_releases();
    graph.wait_for(pid, holder, seen, now).map_err(|_| errno::EDEADLK)
}

/// Whether a deadlock search has been cut off at `MAX_DEADLOCK_CHAIN`
/// (warned about once a boot).
static WARNED_TRUNCATED: AtomicBool = AtomicBool::new(false);

/// What `F_SETLKW`'s wait decision says in the log: a debug line once the
/// wait is decided (not before: an `EDEADLK` isn't a wait), the first time
/// and whenever the holder changes; `EDEADLK` at debug too; and a warning,
/// once a boot, when the cycle search was cut off (a longer cycle would
/// hang every process in it, with no signal to end it).
fn log_wait_decision(
    pid: u32,
    file: &FileRef,
    holder: u32,
    decided: Result<WaitSearch, u32>,
    logged: &mut Option<u32>,
) -> Result<(), u32> {
    match decided {
        Ok(WaitSearch::StaleCaller) => Ok(()),
        Ok(search) => {
            if search == WaitSearch::Truncated && !WARNED_TRUNCATED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    target: "linux",
                    "pid {pid}: F_SETLKW's deadlock search stopped after {MAX_DEADLOCK_CHAIN} waiting processes; \
                     a longer cycle would never be broken"
                );
            }
            if *logged != Some(holder) {
                log::debug!(target: "linux", "pid {pid}: F_SETLKW on {} waits for pid {holder}", Escaped(&file.path));
                *logged = Some(holder);
            }
            Ok(())
        }
        Err(e) => {
            log::debug!(target: "linux", "pid {pid}: F_SETLKW on {}: EDEADLK (pid {holder} waits for it)", Escaped(&file.path));
            Err(e)
        }
    }
}

/// `fcntl(fd, cmd, arg)` (72).
pub(super) fn sys_fcntl(fd: Fd, cmd: u32, arg: u64) -> u64 {
    finish(fcntl(fd, cmd, arg))
}

fn fcntl(fd: Fd, cmd: u32, arg: u64) -> Result<u64, u32> {
    let file = fd.file()?;
    // An `int` argument (`F_DUPFD`'s minimum, the flags): the low 32 bits.
    let argi: i32 = at::reg(arg);
    // On an `O_PATH` descriptor only these work; every other command, an
    // unknown one too, is `-EBADF`, as on Linux.
    let path_ok = matches!(
        cmd,
        general::F_DUPFD | general::F_DUPFD_CLOEXEC | general::F_GETFD | general::F_SETFD | general::F_GETFL
    );
    if file.is_path() && !path_ok {
        return Err(errno::EBADF);
    }
    match cmd {
        general::F_DUPFD | general::F_DUPFD_CLOEXEC => {
            let cloexec = cmd == general::F_DUPFD_CLOEXEC;
            process::dup_current_fd_from(fd.raw(), argi, cloexec).map(|new| new as u64).map_err(vfs)
        }
        general::F_GETFD => {
            let cloexec = process::current_cloexec(fd.raw()).map_err(vfs)?;
            Ok(if cloexec { u64::from(general::FD_CLOEXEC) } else { 0 })
        }
        general::F_SETFD => {
            let cloexec = argi as u32 & general::FD_CLOEXEC != 0;
            process::set_current_cloexec(fd.raw(), cloexec).map(|()| 0).map_err(vfs)
        }
        general::F_GETFL => Ok(u64::from(file.get_fl())),
        // Only `O_APPEND` and `O_NONBLOCK` change; the access mode and the
        // creation flags are ignored, as on Linux, and so are `O_ASYNC`,
        // `O_DIRECT` and `O_NOATIME`, which Linux would change and Lumen
        // doesn't track.
        general::F_SETFL => file.set_status_flags(argi as u32).map(|()| 0).map_err(vfs),
        general::F_GETLK => getlk(&file, at::reg(arg)),
        general::F_SETLK => setlk(&file, at::reg(arg), false),
        general::F_SETLKW => setlk(&file, at::reg(arg), true),
        _ => Err(errno::EINVAL),
    }
}

/// `F_GETLK`: the first lock another process holds that conflicts with the
/// one described, written back over it (`l_whence` `SEEK_SET`, `l_len` 0
/// for a lock to the end, the owner's pid), or `l_type` set to `F_UNLCK`
/// and the rest left as given. The access mode isn't checked (Linux
/// doesn't); an `F_UNLCK` request is `-EINVAL` (checked first, as Linux),
/// a range past the largest offset `-EOVERFLOW`.
fn getlk(file: &FileRef, at: UserPtr) -> Result<u64, u32> {
    let mut flock = UserFlock::copy_in(at)?;
    let ctx = lock_context(file, &flock.spec)?;
    let pid = with_current(|p| p.pid);
    let mut spec = flock.spec;
    with_locks(file, |locks| locks.getlk(pid, &mut spec, ctx.cur_offset, ctx.file_size))?;
    flock.spec = spec;
    flock.copy_out(at)?;
    Ok(0)
}

/// `F_SETLK` (`-EAGAIN` on a conflict) and `F_SETLKW` (waits for it to
/// go; `-EDEADLK` if the holder is waiting, directly or through other
/// waiters, for this process): take, change or give up (`F_UNLCK`) the
/// current process's lock on the range. Checked in Linux's order: the range
/// (`-EINVAL`, `-EOVERFLOW` past the largest offset), then the type
/// (`-EINVAL`), then the access mode: `F_RDLCK` needs a readable descriptor
/// and `F_WRLCK` a writable one (`-EBADF`).
fn setlk(file: &FileRef, at: UserPtr, blocking: bool) -> Result<u64, u32> {
    let spec = UserFlock::copy_in(at)?.spec;
    let ctx = lock_context(file, &spec)?;
    let pid = with_current(|p| p.pid);
    let mut logged = None;
    let result = lock_or_wait(
        file,
        |locks| {
            Ok(match locks.setlk(pid, spec, ctx, blocking)? {
                RecordLockOutcome::Granted { released } => Step::Done { released },
                RecordLockOutcome::WouldBlock { holder } => Step::Wait { released: false, holder },
            })
        },
        |holder, seen| {
            let decided = record_wait_unless_deadlock(pid, holder, seen);
            log_wait_decision(pid, file, holder, decided, &mut logged)
        },
    );
    if blocking {
        // Its record, if any, goes once the call returns (a record left
        // between waits is stale after the release that woke it anyway).
        POSIX_WAITERS.lock().done(pid);
    }
    result.map(|()| 0)
}

/// Whether `flock(LOCK_MAND)` has been seen (reported once a boot).
static SEEN_LOCK_MAND: AtomicBool = AtomicBool::new(false);

/// `flock(fd, operation)` (73): `LOCK_SH`, `LOCK_EX` or `LOCK_UN`, maybe
/// with `LOCK_NB`, on the descriptor's open file description; a held lock
/// is converted in place when nothing conflicts. A conflict with `LOCK_NB`
/// is `-EWOULDBLOCK`, the held lock kept (a deviation: Linux drops it). A
/// blocking request that conflicts first gives up the lock this
/// description holds, as Linux does, so two holders of `LOCK_SH` both
/// asking for `LOCK_EX` don't wait for each other for good: one gets it.
///
/// As Linux (since 6.0): `LOCK_MAND` is ignored (0, whatever the fd;
/// reported once a boot, as Linux's `pr_warn_once`, but at info: the
/// ktests use it on purpose), and the operation is checked (`-EINVAL`)
/// before the descriptor (`-EBADF`; `O_PATH` too). No deadlock is detected
/// (Linux finds none for `flock` either).
pub(super) fn sys_flock(fd: Fd, operation: u32) -> u64 {
    finish(flock(fd, operation))
}

fn flock(fd: Fd, operation: u32) -> Result<u64, u32> {
    if operation & general::LOCK_MAND != 0 {
        if !SEEN_LOCK_MAND.swap(true, Ordering::Relaxed) {
            let pid = with_current(|p| p.pid);
            log::info!(target: "linux", "pid {pid}: flock with LOCK_MAND is ignored (no mandatory locks)");
        }
        return Ok(0);
    }
    let op = FlockOp::from_raw(operation as i32).map_err(vfs)?;
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    let id = file.id;
    let mut logged = false;
    lock_or_wait(
        &file,
        |locks| {
            Ok(match locks.flock(id, op)? {
                FlockOutcome::Granted { released } => Step::Done { released },
                FlockOutcome::WouldBlock { released } => Step::Wait { released, holder: () },
            })
        },
        |(), _| {
            if !logged {
                let pid = with_current(|p| p.pid);
                log::debug!(target: "linux", "pid {pid}: flock on {} waits", Escaped(&file.path));
                logged = true;
            }
            Ok(())
        },
    )
    .map(|()| 0)
}

/// A ktest seam in `lock_or_wait`: an armed window holds the next try that
/// must wait right after it (before anything reads the release count
/// again), until the test lets it go or a thread is blocked in a lock wait.
/// The lost-wakeup and stale-view ktests release a lock inside it.
#[cfg(feature = "ktest")]
mod ktest_window {
    use core::sync::atomic::{AtomicBool, Ordering};

    static ARMED: AtomicBool = AtomicBool::new(false);
    static HELD: AtomicBool = AtomicBool::new(false);
    static GO: AtomicBool = AtomicBool::new(false);

    /// Hold the next try that must wait.
    pub(super) fn arm() {
        GO.store(false, Ordering::SeqCst);
        ARMED.store(true, Ordering::SeqCst);
    }

    /// Hold nothing more (after the armed call, whatever it did).
    pub(super) fn disarm() {
        ARMED.store(false, Ordering::SeqCst);
    }

    /// Whether a try is held in the window now.
    pub(super) fn held() -> bool {
        HELD.load(Ordering::SeqCst)
    }

    /// Let the held try go on.
    pub(super) fn go() {
        GO.store(true, Ordering::SeqCst);
    }

    /// `lock_or_wait`'s side: if armed, wait (sleeping a millisecond at a
    /// time) for `go` or for a thread blocked in a lock wait.
    pub(super) fn hold() {
        if !ARMED.swap(false, Ordering::SeqCst) {
            return;
        }
        HELD.store(true, Ordering::SeqCst);
        while !GO.load(Ordering::SeqCst) && crate::linux::process::lock_waiters() == 0 {
            crate::time::sleep(1_000_000);
        }
        HELD.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- ktests

#[cfg(feature = "ktest")]
use {
    super::fd::{ktest_fd, ktest_open, ktest_unlink, sys_close, sys_lseek, sys_write},
    super::neg_errno,
    crate::ktest::TestResult,
    crate::sched,
    alloc::sync::Arc,
    core::sync::atomic::AtomicU32,
    lumen_vfs::OpenFlags,
};

/// Where the lock ktests keep a `struct flock` in the test process's
/// user memory (after the path slots).
#[cfg(feature = "ktest")]
const KTEST_FLOCK_AT: u64 = process::KTEST_USER_VA + 8192;

/// Where the lock ktests keep bytes to write.
#[cfg(feature = "ktest")]
const KTEST_BYTES_AT: u64 = process::KTEST_USER_VA + 12288;

/// The `l_pid` a ktest's `struct flock` goes in with, to see that a
/// lookup that finds no conflict leaves it alone.
#[cfg(feature = "ktest")]
const KTEST_PID_SENTINEL: i32 = -7;

/// How long a holder sleeps before checking that a blocked locker is still
/// blocked: a floor (it must stay blocked at least this long), never a
/// ceiling.
#[cfg(feature = "ktest")]
const KTEST_FLOOR_NS: u64 = 20_000_000;

/// Write `bytes` at `fd`'s offset (through user memory): `write`'s result.
#[cfg(feature = "ktest")]
fn ktest_write(fd: Fd, bytes: &[u8]) -> Result<u64, &'static str> {
    copy_to_user(KTEST_BYTES_AT, bytes).map_err(|_| "copy the bytes")?;
    Ok(sys_write(fd, super::at::reg(KTEST_BYTES_AT), bytes.len() as u64))
}

/// `fcntl(fd, cmd, &flock)` with a `struct flock` of these fields (padding
/// and `l_pid` filled with markers): the result.
#[cfg(feature = "ktest")]
fn ktest_lk(fd: Fd, cmd: u32, l_type: u32, whence: u32, start: i64, len: i64) -> Result<u64, &'static str> {
    let mut raw = [0x5au8; FLOCK_SIZE];
    raw[0..2].copy_from_slice(&(l_type as i16).to_le_bytes());
    raw[2..4].copy_from_slice(&(whence as i16).to_le_bytes());
    raw[8..16].copy_from_slice(&start.to_le_bytes());
    raw[16..24].copy_from_slice(&len.to_le_bytes());
    raw[24..28].copy_from_slice(&KTEST_PID_SENTINEL.to_le_bytes());
    copy_to_user(KTEST_FLOCK_AT, &raw).map_err(|_| "copy the struct flock")?;
    Ok(sys_fcntl(fd, cmd, KTEST_FLOCK_AT))
}

/// What `F_GETLK` says of a lock of `l_type` over `start`, `len` from
/// `whence`: `(l_type, l_whence, l_start, l_len, l_pid)` as written back,
/// with the padding checked unchanged.
#[cfg(feature = "ktest")]
fn ktest_getlk(
    fd: Fd,
    l_type: u32,
    whence: u32,
    start: i64,
    len: i64,
) -> Result<(u32, u32, i64, i64, i32), &'static str> {
    if ktest_lk(fd, general::F_GETLK, l_type, whence, start, len)? != 0 {
        return Err("F_GETLK failed");
    }
    let mut raw = [0u8; FLOCK_SIZE];
    copy_from_user(&mut raw, KTEST_FLOCK_AT).map_err(|_| "copy the struct flock back")?;
    if raw[4..8] != [0x5a; 4] || raw[28..32] != [0x5a; 4] {
        return Err("F_GETLK wrote over the struct's padding");
    }
    let i64_at = |o: usize| i64::from_le_bytes(raw[o..o + 8].try_into().expect("8 bytes"));
    Ok((
        i16::from_le_bytes([raw[0], raw[1]]) as u32,
        i16::from_le_bytes([raw[2], raw[3]]) as u32,
        i64_at(8),
        i64_at(16),
        i32::from_le_bytes(raw[24..28].try_into().expect("4 bytes")),
    ))
}

/// A whole file's bytes, read through its inode (ktests).
#[cfg(feature = "ktest")]
fn ktest_contents(fd: Fd) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let file = fd.file().map_err(|_| "the fd isn't open")?;
    let mut buf = alloc::vec![0u8; 64];
    let n = file.inode.read_at(0, &mut buf).map_err(|_| "read the file")?;
    buf.truncate(n);
    Ok(buf)
}

crate::ktest!(
    fn lock_fcntl_dupfd_fd_flags_and_status_flags() {
        use crate::{kassert, kassert_eq};
        const PATH: &[u8] = b"/tmp/lock-ktest-fcntl";
        let ret = process::in_test_process(|_| -> TestResult {
            let rdwr = OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC;
            let fd = ktest_open(0, PATH, rdwr, 0o644)?;
            kassert_eq!(fd.raw(), 3);
            // F_DUPFD: the lowest free slot at or above the minimum.
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 0), 4);
            kassert_eq!(sys_close(at::reg::<Fd>(1)), 0);
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 0), 1);
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 10), 10);
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 10), 11);
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 7), 7);
            // The minimum is an `int`: the upper half of the register is
            // ignored.
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, 0x1_0000_0014), 20);
            // A minimum out of range: `-EINVAL`; nothing free at or above
            // it: `-EMFILE`.
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, u64::MAX), neg_errno(errno::EINVAL));
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, lumen_vfs::MAX_FDS as u64), neg_errno(errno::EINVAL));
            let last = lumen_vfs::MAX_FDS as u64 - 1;
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, last), last);
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD, last), neg_errno(errno::EMFILE));
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD_CLOEXEC, last), neg_errno(errno::EMFILE));

            // Close-on-exec: F_DUPFD_CLOEXEC sets it, F_DUPFD doesn't;
            // F_SETFD takes only the FD_CLOEXEC bit, per descriptor.
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD_CLOEXEC, 5), 5);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(5), general::F_GETFD, 0), u64::from(general::FD_CLOEXEC));
            kassert_eq!(sys_fcntl(at::reg::<Fd>(10), general::F_GETFD, 0), 0);
            kassert_eq!(sys_fcntl(fd, general::F_GETFD, 0), 0);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(10), general::F_SETFD, u64::from(general::FD_CLOEXEC)), 0);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(10), general::F_GETFD, 0), u64::from(general::FD_CLOEXEC));
            kassert_eq!(sys_fcntl(at::reg::<Fd>(11), general::F_GETFD, 0), 0);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(10), general::F_SETFD, 0xfe), 0);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(10), general::F_GETFD, 0), 0);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(5), general::F_SETFD, 0), 0);
            kassert_eq!(process::current_cloexec(5), Ok(false));

            // F_GETFL/F_SETFL: the access mode and O_APPEND, shared by the
            // dup'd descriptors (one open file description).
            kassert_eq!(sys_fcntl(fd, general::F_GETFL, 0), u64::from(OpenFlags::O_RDWR));
            kassert_eq!(ktest_write(fd, b"abc")?, 3);
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_SET), 0);
            kassert_eq!(sys_fcntl(fd, general::F_SETFL, u64::from(OpenFlags::O_APPEND)), 0);
            let append = u64::from(OpenFlags::O_RDWR | OpenFlags::O_APPEND);
            kassert_eq!(sys_fcntl(fd, general::F_GETFL, 0), append);
            kassert_eq!(sys_fcntl(at::reg::<Fd>(4), general::F_GETFL, 0), append);
            // With O_APPEND the write lands at the end, not at offset 0.
            kassert_eq!(ktest_write(fd, b"Z")?, 1);
            kassert_eq!(ktest_contents(fd)?, b"abcZ");
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_CUR), 4);
            // The access mode and creation flags are ignored (no
            // truncation, still read-write); O_APPEND clears, O_NONBLOCK
            // sets.
            let ignored = OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC | OpenFlags::O_NONBLOCK;
            kassert_eq!(sys_fcntl(at::reg::<Fd>(4), general::F_SETFL, u64::from(ignored)), 0);
            kassert_eq!(sys_fcntl(fd, general::F_GETFL, 0), u64::from(OpenFlags::O_RDWR | OpenFlags::O_NONBLOCK));
            kassert_eq!(sys_lseek(fd, 0, general::SEEK_SET), 0);
            kassert_eq!(ktest_write(fd, b"Y")?, 1);
            kassert_eq!(ktest_contents(fd)?, b"YbcZ");

            // A bad descriptor: `-EBADF`; an unknown command: `-EINVAL`.
            for cmd in [general::F_DUPFD, general::F_GETFD, general::F_SETFD, general::F_GETFL, general::F_SETFL] {
                kassert_eq!(sys_fcntl(at::reg::<Fd>(77), cmd, 0), neg_errno(errno::EBADF));
            }
            kassert_eq!(sys_fcntl(at::reg::<Fd>(77), 99, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_fcntl(fd, 99, 0), neg_errno(errno::EINVAL));
            kassert_eq!(sys_fcntl(fd, general::F_DUPFD_CLOEXEC + 50, 0), neg_errno(errno::EINVAL));

            // O_PATH: the descriptor commands and F_GETFL work; F_SETFL,
            // the locks, an unknown command and flock are `-EBADF` (flock
            // only once its operation is valid).
            let path_fd = ktest_open(1, b"/tmp", OpenFlags::O_PATH, 0o644)?;
            kassert_eq!(sys_fcntl(path_fd, general::F_GETFL, 0), u64::from(OpenFlags::O_PATH));
            kassert_eq!(sys_fcntl(path_fd, general::F_SETFD, u64::from(general::FD_CLOEXEC)), 0);
            kassert_eq!(sys_fcntl(path_fd, general::F_GETFD, 0), u64::from(general::FD_CLOEXEC));
            kassert_eq!(sys_fcntl(path_fd, general::F_DUPFD, 30), 30);
            kassert_eq!(sys_fcntl(path_fd, general::F_DUPFD_CLOEXEC, 30), 31);
            kassert_eq!(process::current_cloexec(31), Ok(true));
            kassert_eq!(sys_fcntl(path_fd, general::F_SETFL, 0), neg_errno(errno::EBADF));
            kassert_eq!(sys_fcntl(path_fd, 99, 0), neg_errno(errno::EBADF));
            kassert_eq!(
                ktest_lk(path_fd, general::F_GETLK, general::F_RDLCK, general::SEEK_SET, 0, 0)?,
                neg_errno(errno::EBADF)
            );
            kassert_eq!(
                ktest_lk(path_fd, general::F_SETLK, general::F_RDLCK, general::SEEK_SET, 0, 0)?,
                neg_errno(errno::EBADF)
            );
            kassert_eq!(sys_flock(path_fd, general::LOCK_SH), neg_errno(errno::EBADF));
            kassert_eq!(sys_flock(path_fd, 0), neg_errno(errno::EINVAL));
            kassert!(sys_close(path_fd) == 0);
            Ok(())
        });
        let _ = ktest_unlink(PATH);
        ret?
    }
);

crate::ktest!(
    fn lock_fcntl_record_locks_check_modes_and_arguments() {
        use crate::kassert_eq;
        const PATH: &[u8] = b"/tmp/lock-ktest-modes";
        let ret = process::in_test_process(|_| -> TestResult {
            let (set, get) = (general::F_SETLK, general::F_GETLK);
            let (rd, wr, un) = (general::F_RDLCK, general::F_WRLCK, general::F_UNLCK);
            let seek_set = general::SEEK_SET;
            let rdwr = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
            let rdonly = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
            let wronly = ktest_open(0, PATH, OpenFlags::O_WRONLY, 0o644)?;
            // The lock type must match the access mode ...
            kassert_eq!(ktest_lk(rdonly, set, wr, seek_set, 0, 10)?, neg_errno(errno::EBADF));
            kassert_eq!(ktest_lk(wronly, set, rd, seek_set, 0, 10)?, neg_errno(errno::EBADF));
            // ... checked after the type and the range, as on Linux.
            kassert_eq!(ktest_lk(rdonly, set, wr, 3, 0, 10)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdonly, set, wr, seek_set, -1, 10)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdonly, set, wr, seek_set, i64::MAX, 2)?, neg_errno(errno::EOVERFLOW));
            kassert_eq!(ktest_lk(wronly, set, 3, seek_set, 0, 10)?, neg_errno(errno::EINVAL));
            // The range comes before the type, as Linux (`assign_type` last).
            kassert_eq!(ktest_lk(wronly, set, 3, seek_set, i64::MAX, 2)?, neg_errno(errno::EOVERFLOW));
            kassert_eq!(ktest_lk(wronly, set, 3, 7, 0, 10)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdonly, set, rd, seek_set, 0, 10)?, 0);
            kassert_eq!(ktest_lk(wronly, general::F_SETLKW, wr, seek_set, 5, 10)?, 0);
            // A process's own locks never conflict with each other, nor
            // show in F_GETLK (which doesn't check the access mode).
            kassert_eq!(ktest_lk(rdwr, set, wr, seek_set, 0, 0)?, 0);
            kassert_eq!(ktest_getlk(rdonly, wr, seek_set, 0, 0)?, (un, seek_set, 0, 0, KTEST_PID_SENTINEL));
            // Unlocking what isn't locked succeeds.
            kassert_eq!(ktest_lk(rdwr, set, un, seek_set, 0, 0)?, 0);
            kassert_eq!(ktest_lk(rdwr, set, un, seek_set, 100, 5)?, 0);
            // Bad arguments: `-EINVAL`; a range past the largest offset:
            // `-EOVERFLOW`.
            kassert_eq!(ktest_lk(rdwr, set, 3, seek_set, 0, 1)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdwr, set, wr, 3, 0, 1)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdwr, set, wr, seek_set, -1, 1)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdwr, set, wr, seek_set, 10, -20)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdwr, set, wr, seek_set, i64::MAX, 2)?, neg_errno(errno::EOVERFLOW));
            kassert_eq!(ktest_lk(rdwr, set, wr, seek_set, i64::MAX, 1)?, 0);
            kassert_eq!(ktest_lk(rdwr, get, rd, seek_set, i64::MAX - 1, 3)?, neg_errno(errno::EOVERFLOW));
            kassert_eq!(ktest_lk(rdwr, get, un, seek_set, 0, 1)?, neg_errno(errno::EINVAL));
            kassert_eq!(ktest_lk(rdwr, get, 3, seek_set, 0, 1)?, neg_errno(errno::EINVAL));
            // A bad pointer: `-EFAULT`; a bad descriptor: `-EBADF`.
            for cmd in [get, set, general::F_SETLKW] {
                kassert_eq!(sys_fcntl(rdwr, cmd, 0), neg_errno(errno::EFAULT));
                kassert_eq!(sys_fcntl(at::reg::<Fd>(77), cmd, KTEST_FLOCK_AT), neg_errno(errno::EBADF));
            }
            Ok(())
        });
        let _ = ktest_unlink(PATH);
        ret?
    }
);

crate::ktest!(
    fn lock_flock_belongs_to_the_open_file_description() {
        use crate::kassert_eq;
        const PATH: &[u8] = b"/tmp/lock-ktest-flock";
        let ret = process::in_test_process(|_| -> TestResult {
            let (sh, ex, nb, un) = (general::LOCK_SH, general::LOCK_EX, general::LOCK_NB, general::LOCK_UN);
            let busy = neg_errno(errno::EWOULDBLOCK);
            let a = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
            // Another open of the same file, read-only (flock doesn't
            // care), and a dup of the first.
            let b = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
            let a2 = ktest_fd(sys_fcntl(a, general::F_DUPFD, 10))?;
            kassert_eq!(a2.raw(), 10);
            kassert_eq!(sys_flock(a, ex), 0);
            // Another open file description conflicts, even in the same
            // process; the dup is the same lock's holder.
            kassert_eq!(sys_flock(b, ex | nb), busy);
            kassert_eq!(sys_flock(b, sh | nb), busy);
            kassert_eq!(sys_flock(a2, ex | nb), 0);
            // Converted in place through the dup: shared now.
            kassert_eq!(sys_flock(a2, sh), 0);
            kassert_eq!(sys_flock(b, sh | nb), 0);
            kassert_eq!(sys_flock(a, ex | nb), busy);
            kassert_eq!(sys_flock(b, un), 0);
            kassert_eq!(sys_flock(a, ex | nb), 0);
            // Closing one of the two descriptors keeps the lock; closing
            // the last releases it.
            kassert_eq!(sys_close(a), 0);
            kassert_eq!(sys_flock(b, sh | nb), busy);
            kassert_eq!(sys_close(a2), 0);
            kassert_eq!(sys_flock(b, ex | nb), 0);
            // POSIX record locks are another namespace: b's flock doesn't
            // stop this process's record lock.
            kassert_eq!(ktest_lk(b, general::F_SETLK, general::F_RDLCK, general::SEEK_SET, 0, 0)?, 0);
            // Bad operations: `-EINVAL`, checked before the descriptor (a
            // bad one too); LOCK_UN may carry LOCK_NB.
            for op in [0, sh | ex, nb, 16, sh | 16, un | ex, 1 << 31] {
                kassert_eq!(sys_flock(b, op), neg_errno(errno::EINVAL));
                kassert_eq!(sys_flock(at::reg::<Fd>(77), op), neg_errno(errno::EINVAL));
            }
            kassert_eq!(sys_flock(b, un | nb), 0);
            kassert_eq!(sys_flock(at::reg::<Fd>(77), sh), neg_errno(errno::EBADF));
            // LOCK_MAND is ignored, whatever else is set and whatever the
            // descriptor: 0, and nothing locked.
            let mand = general::LOCK_MAND;
            for (fd, op) in [(b, mand), (b, mand | ex), (at::reg::<Fd>(77), mand | sh), (b, mand | 16)] {
                kassert_eq!(sys_flock(fd, op), 0);
            }
            let c = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
            kassert_eq!(sys_flock(c, ex | nb), 0);
            // Reported once a boot (Linux's `pr_warn_once`), however often
            // it comes: one log line for the four calls above (no other
            // ktest uses LOCK_MAND).
            let mut log = alloc::vec![0u8; crate::klog::RING_BYTES];
            let (n, _) = crate::klog::copy_recent(&mut log);
            let said = log[..n].split(|&b| b == b'\n').filter(|line| {
                line.windows(b"flock with LOCK_MAND is ignored".len()).any(|w| w == b"flock with LOCK_MAND is ignored")
            });
            kassert_eq!(said.count(), 1);
            Ok(())
        });
        let _ = ktest_unlink(PATH);
        ret?
    }
);

/// What two test processes running at once share: a step each side moves
/// forward and the other waits for, who has ended, A's pid, and the blocked
/// side's progress.
#[cfg(feature = "ktest")]
#[derive(Default)]
struct Baton {
    step: AtomicU32,
    a_ended: AtomicBool,
    b_ended: AtomicBool,
    a_pid: AtomicU32,
    /// A is about to give its lock up (set just before).
    releasing: AtomicBool,
    /// B's blocking call has returned.
    b_done: AtomicBool,
}

#[cfg(feature = "ktest")]
impl Baton {
    fn go(&self, step: u32) {
        self.step.store(step, Ordering::SeqCst);
    }

    /// Wait (polling, sleeping a millisecond at a time) until the step is at
    /// least `step`, or fail once the other side (`peer_ended`) has ended
    /// without getting there.
    fn wait_for(&self, step: u32, peer_ended: &AtomicBool) -> TestResult {
        loop {
            if self.step.load(Ordering::SeqCst) >= step {
                return Ok(());
            }
            if peer_ended.load(Ordering::SeqCst) {
                return Err("the other test process ended early");
            }
            crate::time::sleep(1_000_000);
        }
    }

    /// A's wait for B to be blocked in a lock call (or to have come back
    /// from it, or ended: the checks after then fail).
    fn wait_for_b_blocked(&self) {
        while process::lock_waiters() == 0
            && !self.b_done.load(Ordering::SeqCst)
            && !self.b_ended.load(Ordering::SeqCst)
        {
            crate::time::sleep(1_000_000);
        }
    }

    /// A's check that B is still blocked a while (at least
    /// `KTEST_FLOOR_NS`) after it was seen blocked.
    fn b_stays_blocked(&self) -> TestResult {
        crate::time::sleep(KTEST_FLOOR_NS);
        crate::kassert!(!self.b_done.load(Ordering::SeqCst));
        crate::kassert!(!self.b_ended.load(Ordering::SeqCst));
        Ok(())
    }
}

/// Run `a` and `b` as two test processes at once (each `in_test_process`
/// on its own kernel thread, both on this CPU), handed a shared [`Baton`]
/// (A's pid in it): A's failure if it failed (B's then given as a `diag`
/// line, if it failed too), else B's result. Either side ending marks it
/// in the baton, so the other's waits stop; A ending releases its locks
/// (exit closes its descriptors), so B can't be left blocked by a failed A.
#[cfg(feature = "ktest")]
fn two_processes(
    a: impl FnOnce(&Baton) -> TestResult + Send + 'static,
    b: impl FnOnce(&Baton) -> TestResult + Send + 'static,
) -> TestResult {
    let baton = Arc::new(Baton::default());
    let mut a_result: TestResult = Err("process A didn't run");
    let mut b_result: TestResult = Err("process B didn't run");
    let cpu = crate::arch::cpu_index();
    let (b_slot, a_slot, b_baton) = (&mut b_result, &mut a_result, Arc::clone(&baton));
    sched::scope(|s| {
        let spawned = s.spawn_on(cpu, "lock-ktest-b", move || {
            let for_b = Arc::clone(&b_baton);
            *b_slot = process::in_test_process(move |_| {
                let r = b(&for_b);
                for_b.b_ended.store(true, Ordering::SeqCst);
                r
            })
            .and_then(|r| r);
            // Also when the process never ran.
            b_baton.b_ended.store(true, Ordering::SeqCst);
        });
        if spawned.is_err() {
            *a_slot = Err("spawn process B's thread");
            return;
        }
        let for_a = Arc::clone(&baton);
        *a_slot = process::in_test_process(move |pid| {
            for_a.a_pid.store(pid, Ordering::SeqCst);
            let r = a(&for_a);
            for_a.a_ended.store(true, Ordering::SeqCst);
            r
        })
        .and_then(|r| r);
        baton.a_ended.store(true, Ordering::SeqCst);
    })
    .map_err(|_| "start the two test processes")?;
    if let (Err(_), Err(b)) = (a_result, b_result) {
        crate::ktest::diag(format_args!("process B failed too: {b}"));
    }
    a_result?;
    b_result
}

/// How the holder gives its lock up in the blocking ktests.
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug)]
enum Release {
    /// `F_UNLCK`, or `LOCK_UN` through a dup of the locking descriptor.
    Unlock,
    /// Closing a descriptor: for POSIX locks another open of the file (any
    /// close releases them), for `flock` the last of the dup'd pair.
    Close,
    /// The process ending with the lock held.
    Exit,
}

/// A holds a write lock on `[0, 100)`; B finds it (`-EAGAIN`, then A's pid
/// and range through F_GETLK), blocks in F_SETLKW, and gets the lock only
/// once A gives it up as `release` says.
#[cfg(feature = "ktest")]
fn ktest_setlkw_waits_for(release: Release) -> TestResult {
    const PATH: &[u8] = b"/tmp/lock-ktest-setlkw";
    let (set, wr, rd, un) = (general::F_SETLK, general::F_WRLCK, general::F_RDLCK, general::F_UNLCK);
    let seek_set = general::SEEK_SET;
    let ret = two_processes(
        move |baton| {
            let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
            let other = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
            crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 100)?, 0);
            baton.go(1);
            baton.wait_for(2, &baton.b_ended)?;
            baton.wait_for_b_blocked();
            baton.b_stays_blocked()?;
            baton.releasing.store(true, Ordering::SeqCst);
            match release {
                Release::Unlock => crate::kassert_eq!(ktest_lk(fd, set, un, seek_set, 0, 0)?, 0),
                Release::Close => crate::kassert_eq!(sys_close(other), 0),
                Release::Exit => {}
            }
            Ok(())
        },
        move |baton| {
            baton.wait_for(1, &baton.a_ended)?;
            let a_pid = baton.a_pid.load(Ordering::SeqCst) as i32;
            let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
            crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 50, 10)?, neg_errno(errno::EAGAIN));
            crate::kassert_eq!(ktest_lk(fd, set, rd, seek_set, 99, 1)?, neg_errno(errno::EAGAIN));
            // Past A's range: no conflict.
            crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 100, 0)?, 0);
            crate::kassert_eq!(ktest_getlk(fd, rd, seek_set, 10, 1)?, (wr, seek_set, 0, 100, a_pid));
            baton.go(2);
            crate::kassert_eq!(ktest_lk(fd, general::F_SETLKW, wr, seek_set, 0, 100)?, 0);
            baton.b_done.store(true, Ordering::SeqCst);
            // It returned only after A began giving its lock up.
            crate::kassert!(baton.releasing.load(Ordering::SeqCst));
            Ok(())
        },
    );
    let _ = ktest_unlink(PATH);
    if ret.is_err() {
        crate::ktest::diag(format_args!("releasing by {release:?}"));
    }
    ret
}

crate::ktest!(
    fn lock_fcntl_setlkw_blocks_until_unlock_close_or_exit() {
        for release in [Release::Unlock, Release::Close, Release::Exit] {
            ktest_setlkw_waits_for(release)?;
        }
        Ok(())
    }
);

/// A holds an exclusive `flock` through a dup'd pair of descriptors; B's
/// `LOCK_NB` attempts are `-EWOULDBLOCK`, its blocking `LOCK_EX` waits
/// through A closing one of the pair, and gets the lock only once A gives
/// it up as `release` says.
#[cfg(feature = "ktest")]
fn ktest_flock_waits_for(release: Release) -> TestResult {
    const PATH: &[u8] = b"/tmp/lock-ktest-flock-wait";
    let (sh, ex, nb, un) = (general::LOCK_SH, general::LOCK_EX, general::LOCK_NB, general::LOCK_UN);
    let ret = two_processes(
        move |baton| {
            let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
            crate::kassert_eq!(sys_flock(fd, ex), 0);
            let dup = ktest_fd(sys_fcntl(fd, general::F_DUPFD, 10))?;
            crate::kassert_eq!(dup.raw(), 10);
            baton.go(1);
            baton.wait_for(2, &baton.b_ended)?;
            baton.wait_for_b_blocked();
            baton.b_stays_blocked()?;
            // Closing one of the pair keeps the lock: B stays blocked.
            crate::kassert_eq!(sys_close(fd), 0);
            baton.b_stays_blocked()?;
            baton.releasing.store(true, Ordering::SeqCst);
            match release {
                Release::Unlock => crate::kassert_eq!(sys_flock(dup, un), 0),
                Release::Close => crate::kassert_eq!(sys_close(dup), 0),
                Release::Exit => {}
            }
            Ok(())
        },
        move |baton| {
            baton.wait_for(1, &baton.a_ended)?;
            let fd = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
            crate::kassert_eq!(sys_flock(fd, ex | nb), neg_errno(errno::EWOULDBLOCK));
            crate::kassert_eq!(sys_flock(fd, sh | nb), neg_errno(errno::EWOULDBLOCK));
            baton.go(2);
            crate::kassert_eq!(sys_flock(fd, ex), 0);
            baton.b_done.store(true, Ordering::SeqCst);
            crate::kassert!(baton.releasing.load(Ordering::SeqCst));
            Ok(())
        },
    );
    let _ = ktest_unlink(PATH);
    if ret.is_err() {
        crate::ktest::diag(format_args!("releasing by {release:?}"));
    }
    ret
}

crate::ktest!(
    fn lock_flock_blocks_until_unlock_last_close_or_exit() {
        for release in [Release::Unlock, Release::Close, Release::Exit] {
            ktest_flock_waits_for(release)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn lock_flock_blocking_upgrade_gives_up_the_shared_lock_first() {
        // Both hold LOCK_SH and both ask for LOCK_EX: B, asking first,
        // gives its shared lock up and waits, so A's upgrade goes through
        // (rather than each waiting for the other for good), and B gets
        // the lock once A lets go.
        const PATH: &[u8] = b"/tmp/lock-ktest-flock-upgrade";
        let (sh, ex, un) = (general::LOCK_SH, general::LOCK_EX, general::LOCK_UN);
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(sys_flock(fd, sh), 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                baton.wait_for_b_blocked();
                baton.b_stays_blocked()?;
                crate::kassert_eq!(sys_flock(fd, ex), 0);
                baton.b_stays_blocked()?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(sys_flock(fd, un), 0);
                Ok(())
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
                crate::kassert_eq!(sys_flock(fd, sh), 0);
                baton.go(2);
                crate::kassert_eq!(sys_flock(fd, ex), 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

crate::ktest!(
    fn lock_fcntl_setlkw_refuses_a_deadlock() {
        // A holds [0, 10) and B [10, 20); B waits for A's range; A asking
        // to wait for B's would close the cycle: `-EDEADLK` (F_SETLK still
        // `-EAGAIN`), and B gets A's range once A lets go; A then waiting
        // for B's range (B's record from before the unlock notwithstanding)
        // is no deadlock.
        const PATH: &[u8] = b"/tmp/lock-ktest-deadlock";
        let (set, setw, wr, un) = (general::F_SETLK, general::F_SETLKW, general::F_WRLCK, general::F_UNLCK);
        let seek_set = general::SEEK_SET;
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 10)?, 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                baton.wait_for_b_blocked();
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 10, 10)?, neg_errno(errno::EDEADLK));
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 15, 1)?, neg_errno(errno::EDEADLK));
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 10, 10)?, neg_errno(errno::EAGAIN));
                // A range nobody else holds is granted at once (no wait, so
                // no cycle check).
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 20, 10)?, 0);
                baton.b_stays_blocked()?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(ktest_lk(fd, set, un, seek_set, 0, 10)?, 0);
                // B, just woken, is about to take [0, 10), not waiting for
                // A any more: waiting for B's range now is no deadlock (it
                // waits until B's exit lets go of it). B has usually not run
                // yet, so its record is still there, older than the unlock:
                // it must not be followed. (Whether B ran first depends on
                // the scheduler; the host test
                // `posix_wait_graph_never_follows_a_record_older_than_the_last_release`
                // pins the rule itself.)
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 10, 10)?, 0);
                crate::kassert!(baton.b_done.load(Ordering::SeqCst));
                Ok(())
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 10, 10)?, 0);
                baton.go(2);
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 0, 10)?, 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

crate::ktest!(
    fn lock_fcntl_getlk_sees_split_ranges_and_offsets_from_seek_cur_and_end() {
        const PATH: &[u8] = b"/tmp/lock-ktest-split";
        let (set, wr, rd, un) = (general::F_SETLK, general::F_WRLCK, general::F_RDLCK, general::F_UNLCK);
        let (seek_set, seek_cur, seek_end) = (general::SEEK_SET, general::SEEK_CUR, general::SEEK_END);
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC, 0o644)?;
                crate::kassert_eq!(ktest_write(fd, b"12345678")?, 8);
                // [0, 100) written, then its middle [40, 60) unlocked: two
                // pieces left.
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 100)?, 0);
                crate::kassert_eq!(ktest_lk(fd, set, un, seek_set, 40, 20)?, 0);
                // A negative length: [150, 200), read.
                crate::kassert_eq!(ktest_lk(fd, set, rd, seek_set, 200, -50)?, 0);
                // From the offset (300): [300, 310).
                crate::kassert_eq!(sys_lseek(fd, 300, seek_set), 300);
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_cur, 0, 10)?, 0);
                // From the end (8): [1008, end of file).
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_end, 1000, 0)?, 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let a = baton.a_pid.load(Ordering::SeqCst) as i32;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
                let free = |start, len| (un, seek_set, start, len, KTEST_PID_SENTINEL);
                // The first conflict over the whole file is the first piece.
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 0, 0)?, (wr, seek_set, 0, 40, a));
                crate::kassert_eq!(ktest_getlk(fd, rd, seek_set, 39, 2)?, (wr, seek_set, 0, 40, a));
                crate::kassert_eq!(ktest_getlk(fd, rd, seek_set, 59, 2)?, (wr, seek_set, 60, 40, a));
                // The hole: nothing, the rest of the struct as given.
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 40, 20)?, free(40, 20));
                crate::kassert_eq!(ktest_getlk(fd, rd, seek_set, 100, 50)?, free(100, 50));
                // A read lock conflicts only with a write lock.
                crate::kassert_eq!(ktest_getlk(fd, rd, seek_set, 150, 50)?, free(150, 50));
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 199, 1)?, (rd, seek_set, 150, 50, a));
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 200, 1)?, free(200, 1));
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 305, 1)?, (wr, seek_set, 300, 10, a));
                // A lock to the end of the file comes back with l_len 0.
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 5000, 1)?, (wr, seek_set, 1008, 0, a));
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_set, 1007, 1)?, free(1007, 1));
                // B's request from its own offset (41) comes back from the
                // start of the file.
                crate::kassert_eq!(sys_lseek(fd, 41, seek_set), 41);
                crate::kassert_eq!(ktest_getlk(fd, rd, seek_cur, -2, 1)?, (wr, seek_set, 0, 40, a));
                // From the end (8): [150, 151).
                crate::kassert_eq!(ktest_getlk(fd, wr, seek_end, 142, 1)?, (rd, seek_set, 150, 50, a));
                // B can take the hole, not a piece.
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 40, 20)?, 0);
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 60, 1)?, neg_errno(errno::EAGAIN));
                baton.go(2);
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

/// A's wait (no ceiling) for B's blocking call to come back while A still
/// holds its (weakened) lock: B's having returned, or B's failure.
#[cfg(feature = "ktest")]
fn ktest_b_done_while_a_holds(baton: &Baton) -> TestResult {
    while !baton.b_done.load(Ordering::SeqCst) && !baton.b_ended.load(Ordering::SeqCst) {
        crate::time::sleep(1_000_000);
    }
    crate::kassert!(baton.b_done.load(Ordering::SeqCst));
    Ok(())
}

crate::ktest!(
    fn lock_fcntl_setlkw_reader_woken_by_a_downgrade() {
        // A's write lock made a read lock (A keeps holding it) lets B's
        // blocked F_SETLKW for a read lock through.
        const PATH: &[u8] = b"/tmp/lock-ktest-downgrade";
        let (set, wr, rd) = (general::F_SETLK, general::F_WRLCK, general::F_RDLCK);
        let seek_set = general::SEEK_SET;
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 100)?, 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                baton.wait_for_b_blocked();
                baton.b_stays_blocked()?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(ktest_lk(fd, set, rd, seek_set, 0, 100)?, 0);
                ktest_b_done_while_a_holds(baton)
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
                baton.go(2);
                crate::kassert_eq!(ktest_lk(fd, general::F_SETLKW, rd, seek_set, 10, 10)?, 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                crate::kassert!(!baton.a_ended.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

crate::ktest!(
    fn lock_flock_shared_waiter_woken_by_a_downgrade() {
        // A's LOCK_EX made LOCK_SH (A keeps holding it) lets B's blocked
        // LOCK_SH through.
        const PATH: &[u8] = b"/tmp/lock-ktest-flock-downgrade";
        let (sh, ex) = (general::LOCK_SH, general::LOCK_EX);
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(sys_flock(fd, ex), 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                baton.wait_for_b_blocked();
                baton.b_stays_blocked()?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(sys_flock(fd, sh), 0);
                ktest_b_done_while_a_holds(baton)
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDONLY, 0o644)?;
                baton.go(2);
                crate::kassert_eq!(sys_flock(fd, sh), 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                crate::kassert!(!baton.a_ended.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

/// A's wait for B's try to be held in `ktest_window` (or for B to fail).
#[cfg(feature = "ktest")]
fn ktest_wait_for_b_held(baton: &Baton) -> TestResult {
    while !ktest_window::held() && !baton.b_ended.load(Ordering::SeqCst) {
        crate::time::sleep(1_000_000);
    }
    crate::kassert!(ktest_window::held());
    Ok(())
}

crate::ktest!(
    fn lock_fcntl_setlkw_sees_a_release_between_its_try_and_its_wait() {
        // B's try fails (A holds the range) and is held right after it, in
        // `ktest_window`; A unlocks there, waking no one (B isn't waiting
        // yet), and lets B go on. B's wait must still end: the release
        // count it read before its try has moved.
        const PATH: &[u8] = b"/tmp/lock-ktest-lost-wakeup";
        let (set, wr, un) = (general::F_SETLK, general::F_WRLCK, general::F_UNLCK);
        let seek_set = general::SEEK_SET;
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 10)?, 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                ktest_wait_for_b_held(baton)?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(ktest_lk(fd, set, un, seek_set, 0, 10)?, 0);
                crate::kassert_eq!(process::lock_waiters(), 0);
                ktest_window::go();
                Ok(())
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
                ktest_window::arm();
                baton.go(2);
                let got = ktest_lk(fd, general::F_SETLKW, wr, seek_set, 0, 10);
                ktest_window::disarm();
                crate::kassert_eq!(got?, 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);

crate::ktest!(
    fn lock_fcntl_setlkw_retries_on_a_stale_view() {
        // A stale view pinned with `ktest_window`: A holds
        // [0, 10) and B [10, 20). B's F_SETLKW on A's range fails its try
        // and is held before its deadlock check. A unlocks [0, 10) and then
        // waits for B's range (recorded: A waits for B, at the new count).
        // B's view (A holds what it wants) is older than that unlock: it
        // must retry and get the range, not follow A's record back to
        // itself and report EDEADLK. A gets B's range once B exits.
        const PATH: &[u8] = b"/tmp/lock-ktest-stale-view";
        let (set, setw, wr, un) = (general::F_SETLK, general::F_SETLKW, general::F_WRLCK, general::F_UNLCK);
        let seek_set = general::SEEK_SET;
        let ret = two_processes(
            move |baton| {
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 0, 10)?, 0);
                baton.go(1);
                baton.wait_for(2, &baton.b_ended)?;
                ktest_wait_for_b_held(baton)?;
                baton.releasing.store(true, Ordering::SeqCst);
                crate::kassert_eq!(ktest_lk(fd, set, un, seek_set, 0, 10)?, 0);
                // Blocking here lets B's held try go on (`ktest_window`
                // ends once a thread is in a lock wait).
                crate::kassert_eq!(ktest_lk(fd, setw, wr, seek_set, 10, 10)?, 0);
                crate::kassert!(baton.b_done.load(Ordering::SeqCst));
                Ok(())
            },
            move |baton| {
                baton.wait_for(1, &baton.a_ended)?;
                let fd = ktest_open(0, PATH, OpenFlags::O_RDWR, 0o644)?;
                crate::kassert_eq!(ktest_lk(fd, set, wr, seek_set, 10, 10)?, 0);
                ktest_window::arm();
                baton.go(2);
                let got = ktest_lk(fd, setw, wr, seek_set, 0, 10);
                ktest_window::disarm();
                crate::kassert_eq!(got?, 0);
                baton.b_done.store(true, Ordering::SeqCst);
                crate::kassert!(baton.releasing.load(Ordering::SeqCst));
                Ok(())
            },
        );
        let _ = ktest_unlink(PATH);
        ret
    }
);
