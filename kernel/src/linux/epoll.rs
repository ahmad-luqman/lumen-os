//! Linux `epoll(7)` instances, interest sets, deferred nested notifications,
//! and description-release teardown (M9 B5, #476; ARCHITECTURE sections 9 and
//! 10).
//!
//! Each epoll file description holds an `Arc<Epoll>` in
//! `KernelFile::obj` (`FileObj::Anon(AnonObj::Epoll)` on `anon_inode:[eventpoll]`,
//! dev `0:9`). Locking follows section 9:
//! - `EPOLL_LOOPS` (level 00a, `SleepMutex<LoopCheck<u64>>`) protects the global
//!   epoll-in-epoll nesting graph and is acquired before any `Epoll::mtx`.
//! - `Epoll::mtx` (level 00b, `SleepMutex<EpState>`, Linux's `ep->mtx`) guards
//!   `EpollSet` and reusable scan/event buffers across `Pollable::poll` and
//!   `epoll_wait`'s user copy-out. Nested epoll polls take outer `mtx` before
//!   inner `mtx` along an acyclic chain bounded by `EP_MAX_NESTS` (4 edges).
//! - `KernelFile::epitems` (level 10c leaf, `SpinLock<EpWatchers>`) tracks each
//!   epoll instance watching a description without allocating or freeing under
//!   the lock.
//! - `Epoll::rq` (level 10e leaf, `SpinLock<ReadyQueue>`, Linux's `ep->lock`)
//!   and `Epoll::defer` (level 10e leaf, `DeferSlot<KernelLocks>`) each nest
//!   inside a source's `PollHead` lock, never together and never inside another
//!   `PollHead` lock.
//! - `Epoll::wq` (level 10f, `WaitQueue`) is woken via `wake_seq` + `wake_all`
//!   so `wait_interruptible`'s predicate never locks `rq` under 10f.

use alloc::collections::LinkedList;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lumen_epoll::{
    EPOLLET, EPOLLEXCLUSIVE, EPOLLEXCLUSIVE_OK_BITS, EpollSet, Event, Handle, LoopCheck, ReadyQueue, Wake,
};
use lumen_vfs::{AccessMode, DeferLink, DeferSlot, PollDefer, PollDeferred, PollGuard, PollMask, PollWaker, Pollable};

use crate::linux::errno;
use crate::linux::process::{FileRef, IoFile, KernelFile};
use crate::linux::syscall::poll::{ConsoleStep, LastWait, WaitTimeout};
use crate::sched::{self, WaitQueue};
use crate::sync::{SleepMutex, SpinLock};
use crate::user::copy_to_user;
use crate::vfs::{KernelLocks, KernelPollHead};

/// Packed x86-64 `struct epoll_event` wire size (12 bytes: `u32 events`, `u64 data`).
pub(crate) const EPOLL_EVENT_SIZE: usize = 12;

const _: () = {
    assert!(core::mem::size_of::<crate::linux::general::epoll_event>() == EPOLL_EVENT_SIZE);
    assert!(core::mem::offset_of!(crate::linux::general::epoll_event, events) == 0);
    assert!(core::mem::offset_of!(crate::linux::general::epoll_event, data) == 4);
};

/// Monotonic generator for globally unique `Epoll::id` values (graph vertices
/// in [`EPOLL_LOOPS`]).
static NEXT_EPOLL_ID: AtomicU64 = AtomicU64::new(1);

/// Level-00a global nesting graph of epoll-in-epoll interests (Linux v6.17
/// upward `ep_loop_check` via `lumen_epoll::LoopCheck`; v6.12 checks downward only).
static EPOLL_LOOPS: SleepMutex<LoopCheck<u64>> = SleepMutex::new(LoopCheck::new());

/// Interest key within one epoll instance: `(KernelFile::id(), fd)`.
pub type Key = (u64, i32);

/// One epoll instance currently watching a [`KernelFile`], stored in
/// [`EpWatchers`].
pub struct EpWatcher {
    ep_id: u64,
    ep: Weak<Epoll>,
    count: usize,
}

/// Pre-allocated single-node slot for [`EpWatchers::commit`] so `EPOLL_CTL_ADD`
/// never allocates under `KernelFile::epitems`'s level-10c spinlock.
pub struct EpWatcherSlot {
    ep_id: u64,
    node: LinkedList<EpWatcher>,
}

/// Per-[`KernelFile`] list of epoll instances watching that open file
/// description (guarded by `KernelFile::epitems`, level-10c leaf).
///
/// [`Self::reserve`] allocates its list node outside the spinlock;
/// [`Self::commit`], [`Self::remove_one`] and [`Self::take_all`] perform zero
/// heap allocations and zero deallocations under the lock.
pub struct EpWatchers {
    list: LinkedList<EpWatcher>,
}

impl EpWatchers {
    /// Create an empty watcher list.
    pub const fn new() -> Self {
        Self { list: LinkedList::new() }
    }

    /// Pre-allocate a watcher node outside `KernelFile::epitems`'s lock.
    pub fn reserve(ep: &Arc<Epoll>) -> EpWatcherSlot {
        let ep_id = ep.id();
        let mut node = LinkedList::new();
        node.push_back(EpWatcher { ep_id, ep: Arc::downgrade(ep), count: 1 });
        EpWatcherSlot { ep_id, node }
    }

    /// Commit a pre-allocated slot under `KernelFile::epitems`'s lock.
    ///
    /// If `ep` already watches this description, bumps its reference count and
    /// returns `Some(slot)` so the caller drops the unused node outside the
    /// lock. Otherwise splices the node in and returns `None`.
    #[must_use = "drop any unused EpWatcherSlot after releasing the epitems spinlock"]
    pub fn commit(&mut self, mut slot: EpWatcherSlot) -> Option<EpWatcherSlot> {
        for entry in self.list.iter_mut() {
            if entry.ep_id == slot.ep_id {
                entry.count += 1;
                return Some(slot);
            }
        }
        self.list.append(&mut slot.node);
        None
    }

    /// Lower `ep_id`'s watcher count under `KernelFile::epitems`'s lock,
    /// splicing out and returning the removed node (to be dropped outside the
    /// lock) when the count reaches zero.
    #[must_use = "drop any removed node after releasing the epitems spinlock"]
    pub fn remove_one(&mut self, ep_id: u64) -> Option<LinkedList<EpWatcher>> {
        let mut pos = None;
        for (i, entry) in self.list.iter_mut().enumerate() {
            if entry.ep_id == ep_id {
                if entry.count > 1 {
                    entry.count -= 1;
                    return None;
                }
                pos = Some(i);
                break;
            }
        }
        let pos = pos?;
        let mut tail = self.list.split_off(pos);
        let mut rest = tail.split_off(1);
        self.list.append(&mut rest);
        Some(tail)
    }

    /// Detach the entire watcher list in O(1) for description release.
    pub fn take_all(&mut self) -> LinkedList<EpWatcher> {
        core::mem::take(&mut self.list)
    }
}

/// Payload stored per registered interest in `EpollSet<Key, Interest>`.
struct Interest {
    /// Metadata-only reference to the watched description (not counted in
    /// `end_refs`, so an interest never keeps a description open).
    target: FileRef,
    /// Exactly one subscription state, including the temporary ADD state.
    subscription: Subscription,
    /// Last polled watched mask, for edge detection on timer-repolled sources.
    last_polled: u32,
    /// Target's `AccessMode`, captured on `ADD`.
    mode: AccessMode,
    /// Target's `Epoll::id` when `target` is itself an epoll instance.
    nested: Option<u64>,
}

enum Subscription {
    Linking,
    Guard { _guard: PollGuard },
    Repoll,
}

/// State protected by [`Epoll::mtx`] (level 00b).
struct EpState {
    set: EpollSet<Key, Interest>,
    /// Reusable handle scratch buffer for `ReadyQueue::begin_scan`.
    scan_buf: Vec<Handle>,
    /// Reusable readiness-mask buffer parallel to `Scan::handles`.
    polled_buf: Vec<u32>,
    /// Reusable collected-events buffer for `ReadyQueue::finish_scan`.
    events_buf: Vec<Event>,
    /// Live handles, reserved fallibly before ADD commits. Also permits
    /// checking nested epolls whose re-poll requirements change after ADD.
    handles: Vec<Handle>,
    /// Set once [`Epoll::close`] has drained this instance.
    closed: bool,
    #[cfg(feature = "ktest")]
    fail_scan_reserve: bool,
    #[cfg(feature = "ktest")]
    fail_add_reserve: bool,
    /// Ktest kernels do not enable IRQ 4; exercise its shared policy locally.
    #[cfg(feature = "ktest")]
    recheck_override: Option<ConsoleStep>,
}

impl EpState {
    fn new() -> Self {
        Self {
            set: EpollSet::new(),
            scan_buf: Vec::new(),
            polled_buf: Vec::new(),
            events_buf: Vec::new(),
            handles: Vec::new(),
            closed: false,
            #[cfg(feature = "ktest")]
            fail_scan_reserve: false,
            #[cfg(feature = "ktest")]
            fail_add_reserve: false,
            #[cfg(feature = "ktest")]
            recheck_override: None,
        }
    }

    fn prepare_add(&mut self) -> Result<(), u32> {
        #[cfg(feature = "ktest")]
        if core::mem::take(&mut self.fail_add_reserve) {
            return Err(errno::ENOMEM);
        }
        self.handles.try_reserve(1).map_err(|_| errno::ENOMEM)
    }

    /// Reserve every buffer before detaching a ready list. Failure leaves the
    /// queue intact so a subsequent caller can retry.
    fn prepare_scan(&mut self, maxevents: usize) -> Result<(), u32> {
        #[cfg(feature = "ktest")]
        if core::mem::take(&mut self.fail_scan_reserve) {
            return Err(errno::ENOMEM);
        }
        let len = self.set.len();
        if self.scan_buf.len() < len {
            self.scan_buf.try_reserve(len - self.scan_buf.len()).map_err(|_| errno::ENOMEM)?;
            self.scan_buf.resize(len, Handle::INVALID);
        }
        self.polled_buf.clear();
        self.polled_buf.try_reserve(len).map_err(|_| errno::ENOMEM)?;
        self.events_buf.clear();
        self.events_buf.try_reserve(maxevents.min(len)).map_err(|_| errno::ENOMEM)?;
        Ok(())
    }
}

/// Kernel `epoll` instance (`anon_inode:[eventpoll]`).
pub struct Epoll {
    mtx: SleepMutex<EpState>,
    rq: SpinLock<ReadyQueue>,
    wq: WaitQueue,
    poll: KernelPollHead,
    id: u64,
    defer: DeferSlot<KernelLocks>,
    /// Monotonic counter incremented before `wq.wake_all()` so
    /// `wait_interruptible` never locks level-10e `rq` under level-10f `wq`.
    wake_seq: AtomicU64,
    /// Matching source callbacks only, for serial safety-step diagnostics.
    /// Scan completion and timer-repoll wakes must not look like IRQ wakes.
    source_seq: AtomicU64,
    /// A failed peek reservation requests a timed retry from outer pollers.
    peek_retry: AtomicBool,
}

impl Epoll {
    /// Allocate a fresh, empty epoll instance with a globally unique [`Self::id`].
    pub fn new() -> Arc<Self> {
        let id = NEXT_EPOLL_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "epoll id exhausted");
        Arc::new(Self {
            mtx: SleepMutex::new(EpState::new()),
            rq: SpinLock::new(ReadyQueue::new()),
            wq: WaitQueue::new(),
            poll: KernelPollHead::new(),
            id,
            defer: DeferSlot::new(),
            wake_seq: AtomicU64::new(0),
            source_seq: AtomicU64::new(0),
            peek_retry: AtomicBool::new(false),
        })
    }

    /// Globally unique identifier of this epoll instance.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Wake every waiter currently blocked in `epoll_wait` on this instance.
    /// Never notifies `self.poll` subscribers.
    fn wake_waiters(&self) {
        self.wake_seq.fetch_add(1, Ordering::SeqCst);
        self.wq.wake_all();
    }

    /// Shared console and guardless recheck policy, propagated through nested
    /// sets even when the inner set gained its source after the outer ADD.
    /// A guardless target requires the short cadence even when the caller
    /// uses a capture console; removing it removes that recheck requirement.
    pub(crate) fn recheck_step(&self) -> Option<ConsoleStep> {
        let state = self.mtx.lock();
        #[cfg(feature = "ktest")]
        if state.recheck_override.is_some() {
            return state.recheck_override;
        }
        let mut step = self.peek_retry.load(Ordering::Acquire).then_some(ConsoleStep::REPOLL);
        for &h in &state.handles {
            if let Some(entry) = state.set.get(h) {
                if matches!(entry.payload.subscription, Subscription::Repoll) {
                    step = ConsoleStep::merge(step, Some(ConsoleStep::REPOLL));
                }
                step = ConsoleStep::merge(step, ConsoleStep::for_file(&entry.payload.target));
            }
        }
        step
    }

    fn deliver(&self, wake: Wake) {
        if wake.should_wake_waiters() {
            self.wake_waiters();
        }
        if wake.should_notify_subscribers() {
            self.poll.notify(PollMask::IN | PollMask::RDNORM);
        }
    }

    fn unlink_all(&self, handles: &[Handle]) {
        let mut rq = self.rq.lock();
        for &handle in handles {
            rq.unlink(handle);
        }
    }

    /// `EPOLL_CTL_ADD` (Linux v6.12 `fs/eventpoll.c`, `do_epoll_ctl` / `ep_insert`).
    ///
    /// Caller holds `target_io` (`IoFile`) and must drop it only after this
    /// method returns (all locks released), so any concurrent last close runs
    /// [`release_file`] after the interest is linked without deadlocking on
    /// `self.mtx`.
    pub(crate) fn ctl_add(
        self: &Arc<Self>,
        target_io: &IoFile,
        pollable: &dyn Pollable,
        fd: i32,
        events: u32,
        data: u64,
    ) -> Result<(), u32> {
        debug_assert!(target_io.pollable().is_some_and(|p| core::ptr::addr_eq(p, pollable)));
        let target_ep_id = target_io.epoll().map(|e| e.id());

        // Linux v6.12 `do_epoll_ctl`: reject `EPOLLEXCLUSIVE` on an epoll
        // target or with bits outside `EPOLLEXCLUSIVE_OK_BITS` before
        // `ep_loop_check` and duplicate lookup.
        if (events & EPOLLEXCLUSIVE) != 0 && (target_ep_id.is_some() || (events & !EPOLLEXCLUSIVE_OK_BITS) != 0) {
            return Err(errno::EINVAL);
        }

        let watcher_slot = EpWatchers::reserve(self);
        let key: Key = (target_io.id(), fd);
        let mode = target_io.access_mode();

        let (wake, unused_slot, recheck_changed) = {
            let mut loops_guard = target_ep_id.map(|_| EPOLL_LOOPS.lock());
            let mut state = self.mtx.lock();
            if state.closed {
                return Err(errno::ENOENT);
            }

            // Linux v6.12 `do_epoll_ctl`: `ep_loop_check` (-ELOOP) runs before
            // `ep_find` (-EEXIST). On duplicate `(parent, child)`, `LoopCheck`
            // bumps its refcount; duplicate lookup then returns EEXIST and
            // immediately undoes the LoopCheck::add before any reservation.
            if let (Some(loops), Some(child_id)) = (loops_guard.as_deref_mut(), target_ep_id) {
                loops.add(self.id, child_id).map_err(lumen_epoll::Errno::to_errno)?;
            }

            let added = (|| {
                if state.set.contains(key) {
                    return Err(errno::EEXIST);
                }
                // Reserve live-handle storage before committing the interest.
                state.prepare_add()?;
                let placeholder = Interest {
                    target: target_io.file_ref(),
                    subscription: Subscription::Linking,
                    last_polled: 0,
                    mode,
                    nested: target_ep_id,
                };
                state.set.add(key, events, data, placeholder).map_err(lumen_epoll::Errno::to_errno)
            })();
            let added = match added {
                Ok(added) => added,
                Err(err) => {
                    if let (Some(loops), Some(child_id)) = (loops_guard.as_deref_mut(), target_ep_id) {
                        loops.remove(self.id, child_id);
                    }
                    return Err(err);
                }
            };

            // `LoopCheck` + `EpollSet` committed; release global `EPOLL_LOOPS`
            // before polling the target while keeping `self.mtx` held.
            drop(loops_guard);

            let handle = added.handle();
            let watch_mask = added.watch_mask();
            let old_storage = self.rq.lock().activate(&mut state.set, added);
            drop(old_storage);

            // Register this epoll in `target_io.epitems` (10c leaf) before
            // subscribing and while `target_io` pins `end_refs >= 1`.
            let unused_slot = target_io.epitems().lock().commit(watcher_slot);

            let waker: Arc<dyn PollWaker> = Arc::new(EpWaker { ep: Arc::clone(self), handle });
            let (initial_mask, guard) = pollable.poll(mode, Some(&waker));
            let recheck_changed = guard.is_none() || ConsoleStep::for_file(target_io).is_some();
            if let Some(interest) = state.set.payload_mut(handle) {
                interest.subscription = match guard {
                    Some(guard) => Subscription::Guard { _guard: guard },
                    None => Subscription::Repoll,
                };
                interest.last_polled = u32::from(initial_mask.bits()) & watch_mask;
            }
            state.handles.push(handle);

            let wake = self.rq.lock().wake(handle, u32::from(initial_mask.bits()));
            (wake, unused_slot, recheck_changed)
        };

        drop(unused_slot);
        // Existing outer waiters must learn that a previously interrupt-only
        // set now requires a timer, even when the new source is still empty.
        self.deliver(if recheck_changed { Wake::Queued } else { wake });
        Ok(())
    }

    /// `EPOLL_CTL_MOD` (Linux v6.12 `fs/eventpoll.c`, `do_epoll_ctl` / `ep_modify`).
    pub(crate) fn ctl_mod(&self, target_io: &IoFile, fd: i32, events: u32, data: u64) -> Result<(), u32> {
        let key: Key = (target_io.id(), fd);
        let wake = {
            let mut state = self.mtx.lock();
            let modified = state.set.modify(key, events, data).map_err(lumen_epoll::Errno::to_errno)?;
            self.rq.lock().apply(modified);
            let mask = poll_target(target_io, target_io.access_mode());
            if let Some(interest) = state.set.payload_mut(modified.handle()) {
                interest.last_polled = u32::from(mask.bits()) & modified.watch_mask();
            }
            self.rq.lock().wake(modified.handle(), u32::from(mask.bits()))
        };

        self.deliver(wake);
        Ok(())
    }

    /// `EPOLL_CTL_DEL` (Linux v6.12 `fs/eventpoll.c`, `do_epoll_ctl` / `ep_remove`).
    pub(crate) fn ctl_del(&self, target_io: &IoFile, fd: i32) -> Result<(), u32> {
        let key: Key = (target_io.id(), fd);
        let target_is_epoll = target_io.epoll().is_some();

        let (interest, removed_watcher) = {
            let mut loops_guard = target_is_epoll.then(|| EPOLL_LOOPS.lock());
            let mut state = self.mtx.lock();
            let (handle, interest) = state.set.remove(key).map_err(lumen_epoll::Errno::to_errno)?;
            state.handles.retain(|&h| h != handle);
            self.rq.lock().unlink(handle);
            if let (Some(loops), Some(child_id)) = (loops_guard.as_deref_mut(), interest.nested) {
                loops.remove(self.id, child_id);
            }
            let removed_watcher = target_io.epitems().lock().remove_one(self.id);
            (interest, removed_watcher)
        };

        // Drop `PollGuard`, metadata `FileRef`, and watcher node outside locks.
        drop(removed_watcher);
        drop(interest);
        Ok(())
    }

    /// Wait for ready events and copy up to `maxevents` packed `epoll_event`s
    /// to `events_ptr` (Linux v6.12 `fs/eventpoll.c`, `ep_poll` /
    /// `ep_send_events`).
    ///
    /// Caller has already validated `1 <= maxevents <= EP_MAX_EVENTS` and
    /// `user_range_valid(events_ptr, maxevents * 12)`. Never restarts on a
    /// signal (`-EINTR`).
    pub(crate) fn wait(&self, events_ptr: u64, maxevents: usize, timeout: WaitTimeout) -> Result<u64, u32> {
        let deadline = match timeout {
            WaitTimeout::Zero | WaitTimeout::Forever => None,
            WaitTimeout::Deadline(dl) => Some(dl),
        };
        let mut timed_out =
            matches!(timeout, WaitTimeout::Zero) || deadline.is_some_and(|dl| crate::time::now_ns() >= dl);

        let mut last_wait = LastWait::None;
        let mut last_seen = 0;
        loop {
            // Snapshot `wake_seq` BEFORE inspecting `rq.events_available()` so
            // any wake arriving after our check advances `wake_seq` past `seen`.
            let seen = self.wake_seq.load(Ordering::SeqCst);

            // Timer-polled sources are real readiness transitions. Defer
            // subscriber notification until after mtx is released.
            let notify = self.poll_guardless_locked(&mut self.mtx.lock());
            self.deliver(notify);
            let step = self.recheck_step();

            if self.rq.lock().events_available() {
                let reported = self.send_events(events_ptr, maxevents)?;
                if reported > 0 {
                    ConsoleStep::note_ready(step, last_wait, || self.source_seq.load(Ordering::SeqCst) != last_seen);
                    return Ok(reported as u64);
                }
            }

            if !timed_out && deadline.is_some_and(|dl| crate::time::now_ns() >= dl) {
                timed_out = true;
            }
            if timed_out {
                // Linux v6.12 fs/eventpoll.c:2010-2011: timed_out wins
                // over pending signals when no events were delivered.
                return Ok(0);
            }

            let (step_dl, is_safety_step) = ConsoleStep::deadline(step, deadline);
            last_seen = self.source_seq.load(Ordering::SeqCst);

            match self.wq.wait_interruptible(step_dl, || self.wake_seq.load(Ordering::SeqCst) != seen) {
                sched::Wake::Woken => {
                    last_wait = LastWait::Woken;
                }
                sched::Wake::TimedOut => {
                    last_wait = if is_safety_step { LastWait::SafetyStep } else { LastWait::TimedOut };
                    if deadline.is_some_and(|dl| crate::time::now_ns() >= dl) {
                        timed_out = true;
                    }
                }
                sched::Wake::Interrupted => return Err(errno::EINTR),
            }
        }
    }

    /// Two-phase scan and user copy-out under `self.mtx` (Linux v6.12
    /// `ep_start_scan` / `ep_send_events` / `ep_done_scan`).
    fn send_events(&self, events_ptr: u64, maxevents: usize) -> Result<usize, u32> {
        // Linux v6.12 fs/eventpoll.c:1830-1831 (ep_send_events), not the
        // empty/expired-timeout branch in ep_poll.
        if sched::with_current(|t| t.fatal_pending()).unwrap_or(false) {
            return Err(errno::EINTR);
        }
        let (res, wake_waiters) = {
            let mut state = self.mtx.lock();
            let set_len = state.set.len();
            state.prepare_scan(maxevents)?;
            self.peek_retry.store(false, Ordering::Release);
            if set_len == 0 {
                return Ok(0);
            }
            let EpState { set, scan_buf, polled_buf, events_buf, .. } = &mut *state;

            // Phase 1: detach the ready list under level-10e `rq`.
            let scan = self.rq.lock().begin_scan(scan_buf.as_mut_slice());
            if scan.is_empty() {
                return Ok(0);
            }

            // Phase 2: poll detached interests under `self.mtx` (no spinlock).
            // Stop once `maxevents` ready interests are found; `finish_scan`
            // restores the unpolled suffix to the front of `ready`.
            let mut ready_found = 0usize;
            for &h in scan.handles() {
                let (bits, watch) = poll_and_record(set, h);
                polled_buf.push(bits);
                if (bits & watch) != 0 {
                    ready_found += 1;
                    if ready_found >= maxevents {
                        break;
                    }
                }
            }

            // Phase 3: apply polled masks under `rq` (zero allocations because
            // `events_buf` was pre-reserved to `maxevents.min(set_len)`).
            let done = self.rq.lock().finish_scan(set, scan, polled_buf, maxevents, events_buf);

            // Phase 4: copy out under mtx (00b precedes Lock 1).
            let mut copied = 0;
            for ev in events_buf.iter() {
                if copy_to_user(events_ptr + (copied * EPOLL_EVENT_SIZE) as u64, &encode_event(ev.events, ev.data))
                    .is_err()
                {
                    break;
                }
                copied += 1;
                // Linux v6.12 fs/eventpoll.c:1336-1351 queues each matching
                // source wake, including a refill after userspace drains it.
                // A guardless source cannot notify that refill: rearm after
                // reporting, allowing unread data once more on the next wait.
                if let Some(entry) = set.get(ev.handle)
                    && entry.events & EPOLLET != 0
                    && matches!(entry.payload.subscription, Subscription::Repoll)
                {
                    set.payload_mut(ev.handle).expect("reported interest exists under mtx").last_polled = 0;
                }
            }

            if copied < events_buf.len() {
                let wake_after_unreport = self.rq.lock().unreport(events_buf[copied..].iter().map(|e| e.handle));
                let out = if copied > 0 { Ok(copied) } else { Err(errno::EFAULT) };
                (out, done.wake_waiters || wake_after_unreport)
            } else {
                (Ok(done.reported), done.wake_waiters)
            }
        };

        // After a scan or unreport, wake ONLY `self.wq` — NEVER notify
        // `self.poll` subscribers (Linux `ep_done_scan`).
        if wake_waiters {
            self.wake_waiters();
        }
        res
    }

    /// Re-poll timer-driven sources under mtx; the caller delivers the
    /// aggregated wake only after releasing mtx. Guardless ET observes an
    /// edge when its watched mask changes from zero to ready or when the
    /// previous event was reported (a drain/refill can occur between waits).
    fn poll_guardless_locked(&self, state: &mut EpState) -> Wake {
        let mut notify = Wake::Filtered;
        for &handle in &state.handles {
            let Some(entry) = state.set.get(handle) else {
                continue;
            };
            if !matches!(entry.payload.subscription, Subscription::Repoll)
                && ConsoleStep::for_file(&entry.payload.target).is_none()
            {
                continue;
            }
            let bits = u32::from(poll_target(&entry.payload.target, entry.payload.mode).bits()) & entry.watch_mask();
            let edge = entry.events & EPOLLET != 0;
            let previous = entry.payload.last_polled;
            if let Some(interest) = state.set.payload_mut(handle) {
                interest.last_polled = bits;
            }
            if bits != 0 && (!edge || previous == 0) {
                let wake = self.rq.lock().wake(handle, bits);
                if wake.should_notify_subscribers() && previous == 0 {
                    notify = wake;
                }
            }
        }
        notify
    }

    /// Non-consuming readiness probe (Linux v6.12 fs/eventpoll.c:944-979,
    /// __ep_eventpoll_poll). Allocation failure preserves the ready list and
    /// requests a timed retry, including from poll/select and outer epolls.
    fn peek_ready(&self) -> bool {
        let (ready, wake_waiters, notify) = {
            let mut state = self.mtx.lock();
            let notify = self.poll_guardless_locked(&mut state);
            if state.prepare_scan(0).is_err() {
                self.peek_retry.store(true, Ordering::Release);
                drop(state);
                self.deliver(notify);
                return false;
            }
            self.peek_retry.store(false, Ordering::Release);
            let EpState { set, scan_buf, polled_buf, .. } = &mut *state;
            let scan = self.rq.lock().begin_scan(scan_buf.as_mut_slice());
            for &h in scan.handles() {
                let (bits, watch) = poll_and_record(set, h);
                polled_buf.push(bits);
                if bits & watch != 0 {
                    break;
                }
            }
            let done = self.rq.lock().finish_peek(scan, polled_buf);
            (done.ready, done.wake_waiters, notify)
        };
        self.deliver(notify);
        // A scan itself wakes ONLY wq. The notify above is from a source
        // transition detected by re-polling, not from finish_peek.
        if wake_waiters {
            self.wake_waiters();
        }
        ready
    }

    /// Empty this epoll instance's own interest set and remove its graph edges
    /// (idempotent; called from [`release_file`] and [`AnonObj::release`]).
    ///
    /// Must be called with no locks 00–10 held (section 9's end-release rule).
    pub fn close(&self) {
        let mut removed_handles = Vec::new();
        let mut removed_interests = Vec::new();
        {
            let mut loops = EPOLL_LOOPS.lock();
            let mut state = self.mtx.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            let len = state.set.len();
            removed_handles.reserve(len);
            removed_interests.reserve(len);
            for (_key, handle, interest) in state.set.drain() {
                removed_handles.push(handle);
                removed_interests.push(interest);
            }
            self.unlink_all(&removed_handles);
            state.handles.clear();
            loops.remove_set(self.id);
        }

        // Detach from every watched target's `epitems` and drop every
        // `PollGuard` and metadata `FileRef` outside all locks.
        drop(removed_handles);
        for interest in removed_interests {
            let removed_node = interest.target.epitems().lock().remove_one(self.id);
            drop(removed_node);
            drop(interest);
        }
    }

    /// Remove every interest referencing the closing description `file_id`
    /// from this watching epoll instance.
    fn remove_released_file(&self, file_id: u64, released_ep_id: Option<u64>) {
        let mut removed_handles = Vec::new();
        let mut removed_interests = Vec::new();
        {
            let mut loops_guard = released_ep_id.map(|_| EPOLL_LOOPS.lock());
            let mut state = self.mtx.lock();
            let EpState { set, handles, .. } = &mut *state;
            for (_key, handle, interest) in set.remove_range((file_id, i32::MIN)..=(file_id, i32::MAX)) {
                handles.retain(|&h| h != handle);
                if let (Some(loops), Some(child_id)) = (loops_guard.as_deref_mut(), interest.nested) {
                    loops.remove(self.id, child_id);
                }
                removed_handles.push(handle);
                removed_interests.push(interest);
            }
            self.unlink_all(&removed_handles);
        }
        drop(removed_handles);
        drop(removed_interests);
    }
}

impl Pollable for Epoll {
    fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        // Subscribe BEFORE peeking so no source wake falls between the peek
        // and the subscription (same rule as every Pollable).
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let mask = if self.peek_ready() { PollMask::IN | PollMask::RDNORM } else { PollMask::EMPTY };
        // Keep the subscription across changes to guardless targets and OOM
        // recovery. Callers separately recompute recheck_step on every pass.
        (mask, guard)
    }
}

impl PollDeferred for Epoll {
    fn link(&self) -> &dyn DeferLink {
        &self.defer
    }

    fn run(&self) {
        self.poll.notify(PollMask::IN | PollMask::RDNORM);
    }
}

/// Per-interest [`PollWaker`] subscribed on each watched target's `PollHead`.
///
/// Updates `ep.rq` (10e), releases `ep.rq`, wakes `ep.wq` (10f), and defers
/// notifying `ep.poll` through `defer` (10e) so `ep.poll`'s lock never nests
/// inside the source's `PollHead` lock. Performs zero heap allocations.
struct EpWaker {
    ep: Arc<Epoll>,
    handle: Handle,
}

impl PollWaker for EpWaker {
    fn wake(&self, ready: PollMask, defer: &mut PollDefer) {
        let wake = self.ep.rq.lock().wake(self.handle, u32::from(ready.bits()));
        if wake.should_wake_waiters() {
            self.ep.source_seq.fetch_add(1, Ordering::SeqCst);
            self.ep.wake_waiters();
        }
        if wake.should_notify_subscribers() {
            defer.push(&self.ep);
        }
    }
}

/// Target-side description-release hook (Linux v6.12 `eventpoll_release_file`),
/// called from `KernelFile::release_end` when `end_refs` drops `1 -> 0` before
/// the description's own object teardown, with no locks 00–10 held.
pub(crate) fn release_file(file: &KernelFile) {
    let watchers = file.epitems().lock().take_all();

    // If the released file is itself an epoll, empty its own set under
    // `EPOLL_LOOPS` and its own `mtx`, releasing both before locking any
    // watching epoll's `mtx` (ARCHITECTURE section 10).
    let released_ep_id = file.epoll().map(|ep| {
        ep.close();
        ep.id()
    });

    let file_id = file.id();
    for watcher in watchers {
        let Some(ep) = watcher.ep.upgrade() else {
            continue;
        };
        ep.remove_released_file(file_id, released_ep_id);
    }
}

fn poll_target(target: &KernelFile, mode: AccessMode) -> PollMask {
    match target.pollable() {
        Some(pollable) => pollable.poll(mode, None).0,
        None => PollMask::EMPTY,
    }
}

fn poll_and_record(set: &mut EpollSet<Key, Interest>, h: Handle) -> (u32, u32) {
    let (bits, watch) = match set.get(h) {
        Some(entry) => (u32::from(poll_target(&entry.payload.target, entry.payload.mode).bits()), entry.watch_mask()),
        None => (0, 0),
    };
    // A scan can observe zero after the timer probe saw ready. Preserve that
    // observation too, or the next guardless ET transition would be lost.
    if let Some(interest) = set.payload_mut(h) {
        interest.last_polled = bits & watch;
    }
    (bits, watch)
}

pub(crate) fn decode_event(raw: &[u8; EPOLL_EVENT_SIZE]) -> (u32, u64) {
    (
        u32::from_le_bytes(raw[..4].try_into().expect("4 bytes")),
        u64::from_le_bytes(raw[4..].try_into().expect("8 bytes")),
    )
}

fn encode_event(events: u32, data: u64) -> [u8; EPOLL_EVENT_SIZE] {
    let mut raw = [0u8; EPOLL_EVENT_SIZE];
    raw[0..4].copy_from_slice(&events.to_le_bytes());
    raw[4..12].copy_from_slice(&data.to_le_bytes());
    raw
}

#[cfg(feature = "ktest")]
mod ktest_helpers {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

    use lumen_vfs::{
        AccessMode, AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, OpenGuard, PollGuard,
        PollMask, PollWaker, Pollable, SetTime, Timespec, VfsError, VfsInode, next_lock_id,
    };

    use crate::linux::process::KernelFile;
    pub(super) use crate::net::sock::ktest_support::{neg, syscall as call};
    use crate::user::{copy_from_user, copy_to_user};
    use crate::vfs::KernelPollHead;

    use super::EPOLL_EVENT_SIZE;

    pub(super) fn write_ev(va: u64, events: u32, data: u64) -> Result<(), &'static str> {
        let raw = super::encode_event(events, data);
        copy_to_user(va, &raw).map_err(|_| "write epoll_event")
    }

    pub(super) fn read_ev(va: u64, idx: usize) -> Result<(u32, u64), &'static str> {
        let mut raw = [0u8; EPOLL_EVENT_SIZE];
        copy_from_user(&mut raw, va + (idx * EPOLL_EVENT_SIZE) as u64).map_err(|_| "read epoll_event")?;
        Ok(super::decode_event(&raw))
    }

    /// Controllable pollable inode whose `KernelPollHead` is directly
    /// inspectable (including `try_subscriber_count`) for epoll ktests.
    pub(super) struct EpTestSource {
        lock_id: u64,
        ino: Ino,
        open_count: AtomicU32,
        mask: AtomicU16,
        guardless: bool,
        legacy_console: bool,
        clear_after_poll: AtomicBool,
        pub(super) head: KernelPollHead,
    }

    impl EpTestSource {
        pub(super) fn new(initial: PollMask) -> Arc<Self> {
            Self::with_guard(initial, false)
        }

        pub(super) fn guardless(initial: PollMask) -> Arc<Self> {
            Self::with_guard(initial, true)
        }

        fn with_guard(initial: PollMask, guardless: bool) -> Arc<Self> {
            Arc::new(Self {
                lock_id: next_lock_id(),
                ino: lumen_vfs::next_detached_ino(),
                open_count: AtomicU32::new(0),
                mask: AtomicU16::new(initial.bits()),
                guardless,
                legacy_console: false,
                clear_after_poll: AtomicBool::new(false),
                head: KernelPollHead::new(),
            })
        }

        pub(super) fn legacy_console() -> Arc<Self> {
            let mut source = Self::new(PollMask::EMPTY);
            Arc::get_mut(&mut source).expect("unique new source").legacy_console = true;
            source
        }

        pub(super) fn clear_after_next_poll(&self) {
            self.clear_after_poll.store(true, Ordering::SeqCst);
        }

        pub(super) fn set_mask_quiet(&self, mask: PollMask) {
            self.mask.store(mask.bits(), Ordering::SeqCst);
        }

        pub(super) fn set_mask(&self, mask: PollMask) {
            self.mask.store(mask.bits(), Ordering::SeqCst);
            if !self.guardless {
                self.head.notify(mask);
            }
        }

        pub(super) fn open_file(self: &Arc<Self>) -> Result<KernelFile, VfsError> {
            let inode: Arc<dyn VfsInode> = Arc::clone(self) as Arc<dyn VfsInode>;
            let _check = OpenGuard::try_new(Arc::clone(&inode))?;
            KernelFile::new_unchecked_flags(
                inode,
                Arc::from(&b"/ep-test-source"[..]),
                OpenFlags::from_bits(OpenFlags::O_RDWR),
            )
        }
    }

    impl Pollable for EpTestSource {
        fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
            let guard = if self.guardless { None } else { waker.map(|w| self.head.subscribe(Arc::clone(w))) };
            let mask = PollMask::from_bits(self.mask.load(Ordering::SeqCst));
            if self.clear_after_poll.swap(false, Ordering::SeqCst) {
                self.set_mask_quiet(PollMask::EMPTY);
            }
            (mask, guard)
        }
    }

    impl VfsInode for EpTestSource {
        fn is_tty(&self) -> bool {
            self.legacy_console
        }
        fn lock_id(&self) -> u64 {
            self.lock_id
        }
        fn dev_id(&self) -> DevId {
            DevId::makedev(0, 251)
        }
        fn ino(&self) -> Ino {
            self.ino
        }
        fn inode_type(&self) -> InodeType {
            InodeType::CharDevice
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
        fn pollable(&self) -> Option<&dyn Pollable> {
            Some(self)
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            Ok(Metadata {
                dev: self.dev_id(),
                ino: self.ino,
                inode_type: InodeType::CharDevice,
                mode: 0o666,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: DevId::default(),
                size: 0,
                blksize: 4096,
                blocks: 0,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
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
        fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
            Ok(0)
        }
        fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
            Ok(buf.len())
        }
        fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
            Err(VfsError::Inval)
        }
        fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
            Err(VfsError::Inval)
        }
        fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
            Ok(())
        }
        fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
            let mut locks = AdvisoryLocks::new();
            f(&mut locks);
            Ok(())
        }
    }
}

crate::ktest!(
    fn interest_outlives_a_closed_dup_and_goes_with_the_last() {
        use crate::linux::process;
        use crate::linux::{errno, general};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, neg, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLLIN};

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let ev_va = base;
            let out_va = base + 64;

            let src = EpTestSource::new(PollMask::EMPTY);
            let file = src.open_file().map_err(|_| "open source")?;
            let fd1 = process::install_current_fd(file, false).map_err(|_| "install fd1")?;
            let fd2 = process::dup_current_fd_from(fd1, 0, false).map_err(|_| "dup fd1")?;

            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            kassert!((epfd as i64) >= 0);

            write_ev(ev_va, EPOLLIN, 0xCAFE_0001)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), fd1 as u64, ev_va, 0, 0]), 0);
            kassert_eq!(src.head.subscriber_count(), 1);

            // Closing one dup keeps the interest alive on the description.
            kassert_eq!(call(general::__NR_close, [fd1 as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(src.head.subscriber_count(), 1);

            // `EPOLL_CTL_DEL` on the closed fd number is `-EBADF`, as Linux.
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_DEL), fd1 as u64, 0, 0, 0]),
                neg(errno::EBADF)
            );

            // Making the source ready still reports under the old fd's data.
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, out_va, 4, 0, 0, 0]), 1);
            let (ev_bits, ev_data) = read_ev(out_va, 0)?;
            kassert_eq!(ev_bits, EPOLLIN);
            kassert_eq!(ev_data, 0xCAFE_0001);

            // Hold an in-flight end reference (like `SCM_RIGHTS` or an active
            // syscall) and close the last FD: the interest must survive until
            // that final end reference drops.
            let inflight = process::current_io_file(fd2).ok_or("fd2 missing")?;
            kassert_eq!(call(general::__NR_close, [fd2 as u64, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(src.head.subscriber_count(), 1);
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, out_va, 4, 0, 0, 0]), 1);

            // Dropping the last end reference runs `epoll::release_file`,
            // removing the interest and unsubscribing its PollGuard.
            drop(inflight);
            kassert_eq!(src.head.subscriber_count(), 0);
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, out_va, 4, 0, 0, 0]), 0);

            kassert_eq!(call(general::__NR_close, [epfd, 0, 0, 0, 0, 0]), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn nested_notify_is_deferred_past_the_source_lock() {
        use crate::linux::general;
        use crate::linux::process;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicUsize;
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};

        struct LockProbeWaker {
            source_head: KernelPollHead,
            inner_head: Option<KernelPollHead>,
            wakes: AtomicUsize,
            source_lock_free: AtomicBool,
            inner_lock_free: AtomicBool,
        }

        impl PollWaker for LockProbeWaker {
            fn wake(&self, _ready: PollMask, _defer: &mut PollDefer) {
                // `try_subscriber_count` uses `SpinLock::try_lock`, which
                // returns `None` (never hangs or panics) if this CPU still
                // holds the source's `PollHead` lock.
                let src_free = self.source_head.try_subscriber_count().is_some();
                self.source_lock_free.store(src_free, Ordering::SeqCst);
                if let Some(inner) = &self.inner_head {
                    let inner_free = inner.try_subscriber_count().is_some();
                    self.inner_lock_free.store(inner_free, Ordering::SeqCst);
                }
                self.wakes.fetch_add(1, Ordering::SeqCst);
            }
        }

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let ev_va = process::KTEST_USER_VA;
            let src = EpTestSource::new(PollMask::EMPTY);
            let src_fd = process::install_current_fd(src.open_file().map_err(|_| "open source")?, false)
                .map_err(|_| "install source")?;

            let inner_fd = call(general::__NR_epoll_create1, [0; 6]);
            let outer_fd = call(general::__NR_epoll_create1, [0; 6]);
            kassert!((inner_fd as i64) >= 0 && (outer_fd as i64) >= 0);

            write_ev(ev_va, EPOLLIN, 1)?;
            kassert_eq!(
                call(general::__NR_epoll_ctl, [inner_fd, u64::from(EPOLL_CTL_ADD), src_fd as u64, ev_va, 0, 0]),
                0
            );
            write_ev(ev_va, EPOLLIN, 2)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [outer_fd, u64::from(EPOLL_CTL_ADD), inner_fd, ev_va, 0, 0]), 0);

            let inner_file = process::current_io_file(inner_fd as i32).ok_or("inner epoll missing")?;
            let outer_file = process::current_io_file(outer_fd as i32).ok_or("outer epoll missing")?;
            let inner_ep = inner_file.epoll().ok_or("inner not epoll")?;
            let outer_ep = outer_file.epoll().ok_or("outer not epoll")?;

            let inner_probe = Arc::new(LockProbeWaker {
                source_head: src.head.clone(),
                inner_head: None,
                wakes: AtomicUsize::new(0),
                source_lock_free: AtomicBool::new(false),
                inner_lock_free: AtomicBool::new(false),
            });
            let outer_probe = Arc::new(LockProbeWaker {
                source_head: src.head.clone(),
                inner_head: Some(inner_ep.poll.clone()),
                wakes: AtomicUsize::new(0),
                source_lock_free: AtomicBool::new(false),
                inner_lock_free: AtomicBool::new(false),
            });

            let inner_guard = inner_ep.poll.subscribe(Arc::clone(&inner_probe) as Arc<dyn PollWaker>);
            let outer_guard = outer_ep.poll.subscribe(Arc::clone(&outer_probe) as Arc<dyn PollWaker>);

            // Notify source: inner's notify must run ONLY after `src.head` is
            // unlocked, and outer's notify ONLY after both `src.head` and
            // `inner_ep.poll` are unlocked.
            src.set_mask(PollMask::IN);

            kassert_eq!(inner_probe.wakes.load(Ordering::SeqCst), 1);
            kassert!(inner_probe.source_lock_free.load(Ordering::SeqCst));
            kassert_eq!(outer_probe.wakes.load(Ordering::SeqCst), 1);
            kassert!(outer_probe.source_lock_free.load(Ordering::SeqCst));
            kassert!(outer_probe.inner_lock_free.load(Ordering::SeqCst));

            drop((inner_guard, outer_guard, inner_file, outer_file));
            kassert_eq!(call(general::__NR_close, [outer_fd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_close, [inner_fd, 0, 0, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_close, [src_fd as u64, 0, 0, 0, 0, 0]), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn epoll_ctl_errors_in_linux_order() {
        use crate::linux::process;
        use crate::linux::{errno, general};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{call, neg, write_ev};
        use lumen_epoll::{
            EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLEXCLUSIVE, EPOLLIN, EPOLLONESHOT,
        };
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let ev_va = base;
            let pipe_va = base + 64;
            let path_va = base + 128;

            // `epoll_create` / `epoll_create1` flag checks.
            kassert_eq!(call(general::__NR_epoll_create, [0; 6]), neg(errno::EINVAL));
            kassert_eq!(call(general::__NR_epoll_create, [(-1i32) as u32 as u64, 0, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(call(general::__NR_epoll_create1, [1, 0, 0, 0, 0, 0]), neg(errno::EINVAL));
            let ep_c = call(general::__NR_epoll_create1, [u64::from(EPOLL_CLOEXEC), 0, 0, 0, 0, 0]);
            kassert!((ep_c as i64) >= 0);
            kassert_eq!(
                call(general::__NR_fcntl, [ep_c, u64::from(general::F_GETFD), 0, 0, 0, 0]),
                u64::from(general::FD_CLOEXEC)
            );
            kassert_eq!(call(general::__NR_close, [ep_c, 0, 0, 0, 0, 0]), 0);

            let epfd = call(general::__NR_epoll_create, [1, 0, 0, 0, 0, 0]);
            kassert!((epfd as i64) >= 0);
            let ep_dup = process::dup_current_fd_from(epfd as i32, 0, false).map_err(|_| "dup epfd")? as u64;

            // Open a pipe, a regular file, `/dev/null`, and an `O_PATH` fd.
            kassert_eq!(call(general::__NR_pipe, [pipe_va, 0, 0, 0, 0, 0]), 0);
            let mut raw_pipe = [0u8; 8];
            copy_from_user(&mut raw_pipe, pipe_va).map_err(|_| "read pipe fds")?;
            let pr = i32::from_le_bytes(raw_pipe[0..4].try_into().expect("4 bytes")) as u64;
            let pw = i32::from_le_bytes(raw_pipe[4..8].try_into().expect("4 bytes")) as u64;

            let reg_file = process::open_kernel_file(
                b"/tmp/epoll-ctl-order-reg",
                OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC,
                0o644,
            )
            .map_err(|_| "open reg")?;
            let reg_fd = process::install_current_fd(reg_file, false).map_err(|_| "install reg")? as u64;

            let null_file = process::open_kernel_file(b"/dev/null", OpenFlags::O_RDWR, 0).map_err(|_| "open null")?;
            let null_fd = process::install_current_fd(null_file, false).map_err(|_| "install null")? as u64;

            copy_to_user(path_va, b"/tmp\0").map_err(|_| "copy /tmp")?;
            let opath_fd =
                call(general::__NR_openat, [general::AT_FDCWD as u64, path_va, u64::from(general::O_PATH), 0, 0, 0]);
            kassert!((opath_fd as i64) >= 0);

            write_ev(ev_va, EPOLLIN, 42)?;

            // 1. Copy `event` before fd lookup for ADD/MOD (`-EFAULT`), NOT for DEL.
            kassert_eq!(
                call(general::__NR_epoll_ctl, [999, u64::from(EPOLL_CTL_ADD), pr, 0, 0, 0]),
                neg(errno::EFAULT)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [999, u64::from(EPOLL_CTL_MOD), pr, 0, 0, 0]),
                neg(errno::EFAULT)
            );
            kassert_eq!(call(general::__NR_epoll_ctl, [999, u64::from(EPOLL_CTL_DEL), pr, 0, 0, 0]), neg(errno::EBADF));

            // 2. Closed or `O_PATH` `epfd` or `fd` -> `-EBADF` before `-EPERM`.
            kassert_eq!(
                call(general::__NR_epoll_ctl, [999, u64::from(EPOLL_CTL_ADD), reg_fd, ev_va, 0, 0]),
                neg(errno::EBADF)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), 999, ev_va, 0, 0]),
                neg(errno::EBADF)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [opath_fd, u64::from(EPOLL_CTL_ADD), pr, ev_va, 0, 0]),
                neg(errno::EBADF)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), opath_fd, ev_va, 0, 0]),
                neg(errno::EBADF)
            );

            // 3. `!file_can_poll` (`-EPERM`) BEFORE `!is_file_epoll` and `same_file` (`-EINVAL`):
            //    `epfd = reg_fd` (not an epoll) with `fd = reg_fd` (same file, not pollable) -> `-EPERM`!
            kassert_eq!(
                call(general::__NR_epoll_ctl, [reg_fd, u64::from(EPOLL_CTL_ADD), reg_fd, ev_va, 0, 0]),
                neg(errno::EPERM)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), null_fd, ev_va, 0, 0]),
                neg(errno::EPERM)
            );

            // 4. Not an epoll (`epfd = pr`) or same description (`epfd == fd`, `dup(epfd)`) -> `-EINVAL`.
            kassert_eq!(
                call(general::__NR_epoll_ctl, [pr, u64::from(EPOLL_CTL_ADD), pw, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), epfd, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), ep_dup, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );

            // 5. `EPOLLEXCLUSIVE` checks before `ELOOP` / `EEXIST` / `ENOENT`.
            let ep2 = call(general::__NR_epoll_create1, [0; 6]);
            kassert!((ep2 as i64) >= 0);
            write_ev(ev_va, EPOLLIN | EPOLLEXCLUSIVE, 1)?;
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), ep2, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_MOD), pr, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );
            write_ev(ev_va, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 1)?;
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), pr, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );

            // 6. `EPOLLEXCLUSIVE` accepted on valid pipe ADD, refused on MOD, deleted cleanly.
            write_ev(ev_va, EPOLLIN | EPOLLEXCLUSIVE, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), pr, ev_va, 0, 0]), 0);
            write_ev(ev_va, EPOLLIN, 1)?;
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_MOD), pr, ev_va, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_DEL), pr, 0, 0, 0]), 0);

            // 7. `EEXIST`, `ENOENT`, invalid `op`, and `ELOOP` (cycle).
            write_ev(ev_va, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), pr, ev_va, 0, 0]), 0);
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), pr, ev_va, 0, 0]),
                neg(errno::EEXIST)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_MOD), pw, ev_va, 0, 0]),
                neg(errno::ENOENT)
            );
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_DEL), pw, 0, 0, 0]),
                neg(errno::ENOENT)
            );
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, 99, pr, ev_va, 0, 0]), neg(errno::EINVAL));

            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), ep2, ev_va, 0, 0]), 0);
            kassert_eq!(
                call(general::__NR_epoll_ctl, [ep2, u64::from(EPOLL_CTL_ADD), epfd, ev_va, 0, 0]),
                neg(errno::ELOOP)
            );

            // Closing ep2 removes the graph edge as well as its interest.
            let ep_file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let old_file = process::current_io_file(ep2 as i32).ok_or("ep2")?;
            let parent_id = ep_file.epoll().ok_or("epfd type")?.id();
            let old_id = old_file.epoll().ok_or("ep2 type")?.id();
            drop(old_file);
            kassert_eq!(call(general::__NR_close, [ep2, 0, 0, 0, 0, 0]), 0);
            kassert!(!EPOLL_LOOPS.lock().contains(parent_id, old_id));
            let ep2 = call(general::__NR_epoll_create1, [0; 6]);
            kassert_eq!(call(general::__NR_epoll_ctl, [ep2, u64::from(EPOLL_CTL_ADD), epfd, ev_va, 0, 0]), 0);
            kassert_eq!(call(general::__NR_epoll_ctl, [ep2, u64::from(EPOLL_CTL_DEL), epfd, 0, 0, 0]), 0);
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), ep2, ev_va, 0, 0]), 0);

            // 8. `epoll_wait` validation order: `maxevents` (-EINVAL), `access_ok` (-EFAULT),
            //    then `epfd` (-EBADF / -EINVAL).
            kassert_eq!(call(general::__NR_epoll_wait, [999, 0, 0, 0, 0, 0]), neg(errno::EINVAL));
            kassert_eq!(
                call(general::__NR_epoll_wait, [999, ev_va, (i32::MAX / 12 + 1) as u64, 0, 0, 0]),
                neg(errno::EINVAL)
            );
            kassert_eq!(call(general::__NR_epoll_wait, [999, 0, 1, 0, 0, 0]), neg(errno::EFAULT));
            kassert_eq!(call(general::__NR_epoll_wait, [999, ev_va, 1, 0, 0, 0]), neg(errno::EBADF));
            kassert_eq!(call(general::__NR_epoll_wait, [pr, ev_va, 1, 0, 0, 0]), neg(errno::EINVAL));

            for fd in [ep2, ep_dup, epfd, pr, pw, reg_fd, null_fd, opath_fd] {
                kassert_eq!(call(general::__NR_close, [fd, 0, 0, 0, 0, 0]), 0);
            }
            copy_to_user(path_va, b"/tmp/epoll-ctl-order-reg\0").map_err(|_| "copy unlink path")?;
            kassert_eq!(call(general::__NR_unlinkat, [general::AT_FDCWD as u64, path_va, 0, 0, 0, 0]), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn copy_out_fault_unreports_the_rest() {
        use crate::linux::process;
        use crate::linux::{errno, general};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, neg, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLET, EPOLLIN, EPOLLONESHOT};

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let ev_va = base;
            let out_va = base + 64;
            let ro_page = base + 3 * 4096;

            // Make `ro_page` read-only so writing across `ro_page - 12`
            // succeeds for event 0 (12 bytes) and faults on event 1.
            kassert_eq!(call(general::__NR_mprotect, [ro_page, 4096, u64::from(general::PROT_READ), 0, 0, 0]), 0);

            let s0 = EpTestSource::new(PollMask::IN);
            let s1 = EpTestSource::new(PollMask::IN);
            let s2 = EpTestSource::new(PollMask::IN);
            let f0 = process::install_current_fd(s0.open_file().map_err(|_| "open s0")?, false).map_err(|_| "f0")?;
            let f1 = process::install_current_fd(s1.open_file().map_err(|_| "open s1")?, false).map_err(|_| "f1")?;
            let f2 = process::install_current_fd(s2.open_file().map_err(|_| "open s2")?, false).map_err(|_| "f2")?;

            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            kassert!((epfd as i64) >= 0);

            // Register LT (10), EPOLLONESHOT (20), and EPOLLET (30).
            write_ev(ev_va, EPOLLIN, 10)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), f0 as u64, ev_va, 0, 0]), 0);
            write_ev(ev_va, EPOLLIN | EPOLLONESHOT, 20)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), f1 as u64, ev_va, 0, 0]), 0);
            write_ev(ev_va, EPOLLIN | EPOLLET, 30)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, u64::from(EPOLL_CTL_ADD), f2 as u64, ev_va, 0, 0]), 0);

            // Fault at event 0: returns `-EFAULT` and unreports all three.
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, ro_page, 4, 0, 0, 0]), neg(errno::EFAULT));

            // Fault at event 1 (`ro_page - 12` fits event 0 only): returns `1`,
            // copies event 0 (`data == 10`), and unreports events 1 and 2
            // (re-arming `EPOLLONESHOT` and keeping `EPOLLET`).
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, ro_page - EPOLL_EVENT_SIZE as u64, 4, 0, 0, 0]), 1);
            kassert_eq!(read_ev(ro_page - EPOLL_EVENT_SIZE as u64, 0)?, (EPOLLIN, 10));

            // Next wait into a valid buffer must deliver the unreported
            // EPOLLONESHOT (20) and EPOLLET (30) events, plus LT (10).
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, out_va, 4, 0, 0, 0]), 3);
            let e0 = read_ev(out_va, 0)?;
            let e1 = read_ev(out_va, 1)?;
            let e2 = read_ev(out_va, 2)?;
            kassert_eq!(e0, (EPOLLIN, 20));
            kassert_eq!(e1, (EPOLLIN, 30));
            kassert_eq!(e2, (EPOLLIN, 10));

            // Now that EPOLLONESHOT (20) and EPOLLET (30) were genuinely
            // delivered, only LT (10) stays ready.
            kassert_eq!(call(general::__NR_epoll_wait, [epfd, out_va, 4, 0, 0, 0]), 1);
            kassert_eq!(read_ev(out_va, 0)?, (EPOLLIN, 10));

            for fd in [epfd, f0 as u64, f1 as u64, f2 as u64] {
                kassert_eq!(call(general::__NR_close, [fd, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn guardless_nested_wait_and_poll_return() {
        use crate::linux::{general, process};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::guardless(PollMask::EMPTY);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let inner = call(general::__NR_epoll_create1, [0; 6]);
            let outer = call(general::__NR_epoll_create1, [0; 6]);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            kassert_eq!(call(general::__NR_epoll_ctl, [outer, EPOLL_CTL_ADD as u64, inner, base, 0, 0]), 0);
            let inner_file = process::current_io_file(inner as i32).ok_or("inner")?;
            let inner_ep = inner_file.epoll().ok_or("epoll")?;
            kassert!(inner_ep.recheck_step().is_some());
            let outer_file = process::current_io_file(outer as i32).ok_or("outer")?;
            kassert!(outer_file.epoll().ok_or("outer epoll")?.recheck_step().is_some());
            kassert_eq!(src.head.subscriber_count(), 0);

            let waiter = process::spawn_test_thread(move |_| {
                call(general::__NR_epoll_wait, [outer, base + 64, 1, u64::MAX, 0, 0])
            })?;
            kassert!(waiter.settle());
            src.set_mask(PollMask::IN);
            kassert_eq!(waiter.join()?, 1);

            src.set_mask(PollMask::EMPTY);
            let mut pollfd = [0u8; 8];
            pollfd[..4].copy_from_slice(&(inner as i32).to_le_bytes());
            pollfd[4..6].copy_from_slice(&PollMask::IN.bits().to_le_bytes());
            copy_to_user(base + 128, &pollfd).map_err(|_| "pollfd")?;
            let poller =
                process::spawn_test_thread(move |_| call(general::__NR_poll, [base + 128, 1, u64::MAX, 0, 0, 0]))?;
            kassert!(poller.settle());
            src.set_mask(PollMask::IN);
            kassert_eq!(poller.join()?, 1);
            copy_from_user(&mut pollfd, base + 128).map_err(|_| "revents")?;
            kassert_eq!(u16::from_le_bytes([pollfd[6], pollfd[7]]), PollMask::IN.bits());

            // select shares the same propagation and must also use no timeout.
            src.set_mask(PollMask::EMPTY);
            copy_to_user(base + 192, &(1u64 << inner).to_le_bytes()).map_err(|_| "fdset")?;
            let selector =
                process::spawn_test_thread(move |_| call(general::__NR_select, [inner + 1, base + 192, 0, 0, 0, 0]))?;
            kassert!(selector.settle());
            src.set_mask(PollMask::IN);
            kassert_eq!(selector.join()?, 1);
            for f in [outer, inner, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn nested_wait_keeps_guard_after_guardless_removal() {
        use crate::linux::{general, process};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLLIN};

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            for close_target in [false, true] {
                let guardless = EpTestSource::guardless(PollMask::EMPTY);
                let guarded = EpTestSource::new(PollMask::EMPTY);
                let fd = process::install_current_fd(guardless.open_file().map_err(|_| "guardless")?, false)
                    .map_err(|_| "install guardless")? as u64;
                let guarded_fd = process::install_current_fd(guarded.open_file().map_err(|_| "guarded")?, false)
                    .map_err(|_| "install guarded")? as u64;
                let inner = call(general::__NR_epoll_create1, [0; 6]);
                let outer = call(general::__NR_epoll_create1, [0; 6]);
                write_ev(base, EPOLLIN, 1)?;
                kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
                kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_ADD as u64, guarded_fd, base, 0, 0]), 0);
                write_ev(base, EPOLLIN, 42)?;
                kassert_eq!(call(general::__NR_epoll_ctl, [outer, EPOLL_CTL_ADD as u64, inner, base, 0, 0]), 0);
                let file = process::current_io_file(outer as i32).ok_or("outer")?;
                let ep = file.epoll().ok_or("outer epoll")?;
                kassert!(ep.recheck_step().is_some());
                if close_target {
                    kassert_eq!(call(general::__NR_close, [fd, 0, 0, 0, 0, 0]), 0);
                } else {
                    kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_DEL as u64, fd, 0, 0, 0]), 0);
                }
                kassert!(ep.recheck_step().is_none());
                let waiter = process::spawn_test_thread(move |_| {
                    call(general::__NR_epoll_wait, [outer, base + 64, 1, u64::MAX, 0, 0])
                })?;
                kassert!(waiter.settle());
                kassert!(ep.recheck_step().is_none());
                guarded.set_mask(PollMask::IN);
                // With no timeout or recheck, only the retained subscription
                // can make this blocked outer waiter return.
                kassert_eq!(waiter.join()?, 1);
                kassert_eq!(read_ev(base + 64, 0)?, (EPOLLIN, 42));
                if !close_target {
                    kassert_eq!(call(general::__NR_close, [fd, 0, 0, 0, 0, 0]), 0);
                }
                for f in [outer, inner, guarded_fd] {
                    kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
                }
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn guardless_edges_require_zero_to_ready() {
        use crate::kassert_eq;
        use crate::linux::{general, process};
        use ktest_helpers::{EpTestSource, call, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLL_CTL_MOD, EPOLLIN, EPOLLONESHOT};

        use core::sync::atomic::AtomicUsize;

        struct RecheckProbe {
            ep: Weak<Epoll>,
            wakes: AtomicUsize,
            unlocked: AtomicBool,
        }
        impl PollWaker for RecheckProbe {
            fn wake(&self, _: PollMask, _: &mut PollDefer) {
                let ep = self.ep.upgrade().expect("test pins epoll");
                self.unlocked.store(ep.mtx.try_lock().is_some(), Ordering::SeqCst);
                self.wakes.fetch_add(1, Ordering::SeqCst);
            }
        }

        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::guardless(PollMask::EMPTY);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let inner = call(general::__NR_epoll_create1, [0; 6]);
            let outer = call(general::__NR_epoll_create1, [0; 6]);
            // Link the outer BEFORE the guardless source is added: later
            // rechecks must discover the inner's new requirement too.
            write_ev(base, EPOLLIN | EPOLLET, 2)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [outer, EPOLL_CTL_ADD as u64, inner, base, 0, 0]), 0);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            let file = process::current_io_file(inner as i32).ok_or("inner")?;
            let ep = file.epoll().ok_or("epoll")?;
            let probe = Arc::new(RecheckProbe {
                ep: Arc::downgrade(ep),
                wakes: AtomicUsize::new(0),
                unlocked: AtomicBool::new(false),
            });
            let _probe_guard = ep.poll.subscribe(probe.clone());
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [outer, base + 64, 1, 0, 0, 0]), 1);
            kassert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
            kassert_eq!(probe.unlocked.load(Ordering::SeqCst), true);
            kassert_eq!(call(general::__NR_epoll_wait, [outer, base + 64, 1, 0, 0, 0]), 0);
            // The LT inner must not create new outer edges on repeated peeks.
            kassert_eq!(call(general::__NR_epoll_wait, [outer, base + 64, 1, 0, 0, 0]), 0);
            write_ev(base, EPOLLIN | EPOLLET, 3)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_MOD as u64, fd, base, 0, 0]), 0);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            kassert_eq!(read_ev(base + 64, 0)?, (EPOLLIN, 3));
            src.set_mask(PollMask::EMPTY);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 0);
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            // Drain and refill entirely between waits: no poll observes EMPTY.
            src.set_mask(PollMask::EMPTY);
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            kassert_eq!(read_ev(base + 64, 0)?, (EPOLLIN, 3));
            // Unread guardless ET data may be reported once per wait.
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            src.set_mask(PollMask::EMPTY);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 0);
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            // The timer probe sees ready, but the subsequent scan sees zero.
            // That last poll must re-arm edge detection for the next transition.
            src.set_mask(PollMask::EMPTY);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 0);
            src.set_mask(PollMask::IN);
            src.clear_after_next_poll();
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 0);
            src.set_mask(PollMask::IN);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
            // MOD explicitly rearms an already-ready one-shot.
            write_ev(base, EPOLLIN | EPOLLET | EPOLLONESHOT, 4)?;
            for _ in 0..2 {
                kassert_eq!(call(general::__NR_epoll_ctl, [inner, EPOLL_CTL_MOD as u64, fd, base, 0, 0]), 0);
                kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 1);
                kassert_eq!(call(general::__NR_epoll_wait, [inner, base + 64, 1, 0, 0, 0]), 0);
            }
            for f in [outer, inner, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn blocked_epoll_wait_wakes_from_source() {
        use crate::linux::{general, process};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::new(PollMask::EMPTY);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            write_ev(base, EPOLLIN, 42)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ep = file.epoll().ok_or("epoll")?.clone();
            kassert!(ep.recheck_step().is_none());
            let waiter = process::spawn_test_thread(move |_| ep.wait(base + 64, 1, WaitTimeout::Forever))?;
            kassert!(waiter.settle());
            kassert_eq!(src.head.subscriber_count(), 1);
            src.set_mask(PollMask::IN);
            // No deadline/re-poll can rescue a missing EpWaker wake.
            kassert_eq!(waiter.join()?, Ok(1));
            kassert_eq!(read_ev(base + 64, 0)?, (EPOLLIN, 42));
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn empty_expired_wait_ignores_fatal_signal() {
        use crate::kassert_eq;
        use crate::linux::{general, process};
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::new(PollMask::IN);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ready = file.epoll().ok_or("epoll")?.clone();
            let waiter = process::spawn_test_thread(move |_| {
                let ep = Epoll::new();
                sched::with_current(|t| t.set_fatal_pending());
                (
                    ep.wait(process::KTEST_USER_VA, 1, WaitTimeout::Zero),
                    ep.wait(process::KTEST_USER_VA, 1, WaitTimeout::Deadline(0)),
                    ready.wait(base + 64, 1, WaitTimeout::Zero),
                )
            })?;
            kassert_eq!(waiter.join()?, (Ok(0), Ok(0), Err(errno::EINTR)));
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn allocation_failures_preserve_interests_and_retry() {
        use crate::linux::{general, process};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, neg, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::new(PollMask::IN);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ep = file.epoll().ok_or("epoll")?;
            write_ev(base, EPOLLIN | EPOLLET, 1)?;
            ep.mtx.lock().fail_add_reserve = true;
            kassert_eq!(
                call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]),
                neg(errno::ENOMEM)
            );
            kassert!(ep.mtx.lock().set.is_empty());
            kassert!(ep.recheck_step().is_none());
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            kassert!(ep.recheck_step().is_none());
            ep.mtx.lock().fail_scan_reserve = true;
            kassert!(!ep.peek_ready());
            kassert!(ep.peek_retry.load(Ordering::Acquire));
            kassert!(ep.recheck_step().is_some());
            kassert!(ep.rq.lock().events_available());
            kassert!(ep.peek_ready());
            kassert!(!ep.peek_retry.load(Ordering::Acquire));
            ep.mtx.lock().fail_scan_reserve = true;
            kassert_eq!(ep.wait(base + 64, 1, WaitTimeout::Zero), Err(errno::ENOMEM));
            ep.mtx.lock().fail_scan_reserve = true;
            kassert!(!ep.peek_ready());
            kassert!(ep.recheck_step().is_some());
            kassert_eq!(ep.wait(base + 64, 1, WaitTimeout::Zero), Ok(1));
            kassert!(ep.recheck_step().is_none());
            kassert_eq!(ep.wait(base + 64, 1, WaitTimeout::Zero), Ok(0));
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn maxevents_one_visits_each_ready_source() {
        use crate::kassert_eq;
        use crate::linux::{general, process};
        use ktest_helpers::{EpTestSource, call, read_ev, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            let mut fds = [0; 3];
            for (i, fd) in fds.iter_mut().enumerate() {
                let src = EpTestSource::new(PollMask::IN);
                *fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                    .map_err(|_| "install")? as u64;
                write_ev(base, EPOLLIN, i as u64)?;
                kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, *fd, base, 0, 0]), 0);
            }
            for i in 0..3 {
                kassert_eq!(call(general::__NR_epoll_wait, [epfd, base + 64, 1, 0, 0, 0]), 1);
                kassert_eq!(read_ev(base + 64, 0)?, (EPOLLIN, i));
            }
            for f in [epfd, fds[0], fds[1], fds[2]] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn poll_retries_after_peek_allocation_failure() {
        use crate::linux::{general, process};
        use crate::user::copy_to_user;
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::new(PollMask::EMPTY);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ep = file.epoll().ok_or("epoll")?;
            ep.mtx.lock().fail_scan_reserve = true;
            let mut pollfd = [0u8; 8];
            pollfd[..4].copy_from_slice(&(epfd as i32).to_le_bytes());
            pollfd[4..6].copy_from_slice(&PollMask::IN.bits().to_le_bytes());
            copy_to_user(base + 64, &pollfd).map_err(|_| "pollfd")?;
            let waiter =
                process::spawn_test_thread(move |_| call(general::__NR_poll, [base + 64, 1, u64::MAX, 0, 0, 0]))?;
            // A successful retry clears peek_retry. The retained guard must
            // still wake the waiter after its temporary recheck is removed.
            while ep.mtx.lock().scan_buf.is_empty() && !waiter.finished() {
                crate::time::sleep(1_000_000);
            }
            kassert!(waiter.settle());
            kassert!(!ep.peek_retry.load(Ordering::Acquire));
            src.set_mask(PollMask::IN);
            kassert_eq!(waiter.join()?, 1);
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn poll_discovers_guardless_source_added_while_blocked() {
        use crate::linux::{general, process};
        use crate::user::copy_to_user;
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::guardless(PollMask::EMPTY);
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ep = file.epoll().ok_or("epoll")?;
            let mut pollfd = [0u8; 8];
            pollfd[..4].copy_from_slice(&(epfd as i32).to_le_bytes());
            pollfd[4..6].copy_from_slice(&PollMask::IN.bits().to_le_bytes());
            copy_to_user(base + 64, &pollfd).map_err(|_| "pollfd")?;
            let waiter =
                process::spawn_test_thread(move |_| call(general::__NR_poll, [base + 64, 1, u64::MAX, 0, 0, 0]))?;
            kassert!(waiter.settle());
            kassert_eq!(ep.poll.subscriber_count(), 1);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            src.set_mask(PollMask::IN);
            kassert_eq!(waiter.join()?, 1);
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn serial_safety_step_rechecks_without_notification() {
        use crate::linux::{general, process};
        use crate::{kassert, kassert_eq};
        use ktest_helpers::{EpTestSource, call, write_ev};
        use lumen_epoll::{EPOLL_CTL_ADD, EPOLLIN};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            process::with_current_console(|c| c.io = process::ConsoleIo::Serial);
            let base = process::KTEST_USER_VA;
            let src = EpTestSource::legacy_console();
            let fd = process::install_current_fd(src.open_file().map_err(|_| "source")?, false)
                .map_err(|_| "install")? as u64;
            let epfd = call(general::__NR_epoll_create1, [0; 6]);
            write_ev(base, EPOLLIN, 1)?;
            kassert_eq!(call(general::__NR_epoll_ctl, [epfd, EPOLL_CTL_ADD as u64, fd, base, 0, 0]), 0);
            let file = process::current_io_file(epfd as i32).ok_or("epfd")?;
            let ep = file.epoll().ok_or("epoll")?.clone();
            let irq4 = crate::serial_in::by_interrupt();
            kassert_eq!(
                ep.recheck_step().map(|s| s.test_interval()),
                Some((if irq4 { 100_000_000 } else { 5_000_000 }, irq4))
            );
            // Ktest boot leaves IRQ 4 off. Select the same pure policy with
            // an IRQ-4 input, local to this instance, to test the 100 ms wait.
            ep.mtx.lock().recheck_override = ConsoleStep::for_console(process::ConsoleIo::Serial, true);
            kassert_eq!(ep.recheck_step().map(|s| s.test_interval()), Some((100_000_000, true)));
            let waiter = process::spawn_test_thread(move |_| ep.wait(base + 64, 1, WaitTimeout::Forever))?;
            kassert!(waiter.settle());
            // Model a lost IRQ notification: only readiness changes. The
            // 100 ms timer must perform the missing recheck (no elapsed ceiling).
            src.set_mask_quiet(PollMask::IN);
            kassert_eq!(waiter.join()?, Ok(1));
            for f in [epfd, fd] {
                kassert_eq!(call(general::__NR_close, [f, 0, 0, 0, 0, 0]), 0);
            }
            Ok(())
        })?
    }
);
