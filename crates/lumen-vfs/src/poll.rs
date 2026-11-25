//! Readiness interface for `poll`/`ppoll`/`select`/`pselect6` (M8, B3) and
//! `epoll` (M9) (ARCHITECTURE section 10).
//!
//! Each readiness-driven file kind implements [`Pollable`] and returns
//! `Some(self)` from [`VfsInode::pollable`](crate::VfsInode::pollable); file
//! kinds returning `None` are always ready ([`PollMask::ALWAYS_READY`]).
//!
//! A [`PollHead`] holds subscribed [`PollWaker`]s behind its [`LockFamily`]'s
//! level-10e spin lock (section 9, level-10 `e`). [`PollHead::subscribe`]
//! allocates its list node before taking that lock, and [`PollGuard`]'s `Drop`
//! splices it out and frees it after releasing the lock, so neither
//! subscribing, notifying nor unsubscribing allocates or frees while the
//! level-10e spin lock is held.

use alloc::collections::LinkedList;
use alloc::sync::{Arc, Weak};
use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::fd::{LockFamily, VfsLock};
use crate::types::AccessMode;

/// Linux `poll(2)` / `epoll(7)` readiness bitmask (x86-64 `asm-generic/poll.h`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PollMask(u16);

impl AccessMode {
    /// True if this access mode allows reading (`O_RDONLY` or `O_RDWR`).
    pub const fn can_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    /// True if this access mode allows writing (`O_WRONLY` or `O_RDWR`).
    pub const fn can_write(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }
}

impl PollMask {
    /// No events ready.
    pub const EMPTY: Self = Self(0);
    /// Data other than high-priority data may be read without blocking (`POLLIN`).
    pub const IN: Self = Self(0x0001);
    /// Exceptional condition / high-priority data (`POLLPRI`).
    pub const PRI: Self = Self(0x0002);
    /// Normal data may be written without blocking (`POLLOUT`).
    pub const OUT: Self = Self(0x0004);
    /// Error condition (`POLLERR`, always reported in `revents`).
    pub const ERR: Self = Self(0x0008);
    /// Hang up (`POLLHUP`, always reported in `revents`).
    pub const HUP: Self = Self(0x0010);
    /// Invalid request: descriptor not open or `O_PATH` (`POLLNVAL`).
    pub const NVAL: Self = Self(0x0020);
    /// Normal data may be read without blocking (`POLLRDNORM`).
    pub const RDNORM: Self = Self(0x0040);
    /// Priority band data may be read without blocking (`POLLRDBAND`).
    pub const RDBAND: Self = Self(0x0080);
    /// Normal data may be written without blocking (`POLLWRNORM`).
    pub const WRNORM: Self = Self(0x0100);
    /// Priority band data may be written without blocking (`POLLWRBAND`).
    pub const WRBAND: Self = Self(0x0200);
    /// Stream peer closed its writing half (`POLLRDHUP`).
    pub const RDHUP: Self = Self(0x2000);

    /// Readiness mask for always-ready file kinds (regular files, directories,
    /// procfs, `/dev/null`, `/dev/zero`, `/dev/random`, `/dev/urandom`),
    /// matching Linux's `DEFAULT_POLLMASK`.
    pub const ALWAYS_READY: Self = Self(Self::IN.0 | Self::OUT.0 | Self::RDNORM.0 | Self::WRNORM.0);

    /// Linux `fs/select.c` `POLLIN_SET`: bits that mark a descriptor readable in `select`.
    pub const POLLIN_SET: Self =
        Self(Self::RDNORM.0 | Self::RDBAND.0 | Self::IN.0 | Self::HUP.0 | Self::ERR.0 | Self::NVAL.0);

    /// Linux `fs/select.c` `POLLOUT_SET`: bits that mark a descriptor writable in `select`.
    pub const POLLOUT_SET: Self = Self(Self::WRBAND.0 | Self::WRNORM.0 | Self::OUT.0 | Self::ERR.0 | Self::NVAL.0);

    /// Linux `fs/select.c` `POLLEX_SET`: bits that mark a descriptor exceptional in `select`.
    pub const POLLEX_SET: Self = Self(Self::PRI.0 | Self::NVAL.0);

    /// Construct a mask from raw Linux `poll` event bits.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Raw Linux `poll` event bits.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// True if no event bits are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True if every bit in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// True if any bit in `other` is set in `self`.
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// Bitwise union of two masks.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Bitwise intersection of two masks.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Filter a source's readiness mask for a `poll`/`ppoll` entry requesting
    /// `events`: keeps requested bits plus `POLLERR` and `POLLHUP`.
    pub const fn filter_events(self, events: Self) -> Self {
        Self(self.0 & (events.0 | Self::ERR.0 | Self::HUP.0))
    }
}

impl BitOr for PollMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PollMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for PollMask {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for PollMask {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for PollMask {
    type Output = Self;
    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

/// Callback invoked by [`PollHead::notify`] when a source's readiness may have changed.
pub trait PollWaker: Send + Sync {
    /// Called under the source's `PollHead` lock with the bits that may have
    /// turned on. May only set atomics and wake a thread (or, M9, queue an
    /// epoll entry under a leaf lock); never allocates or blocks.
    fn wake(&self, ready: PollMask);
}

/// Readiness source implemented by pipes, FIFOs, terminals, and future async
/// file kinds (sockets, `eventfd`, `timerfd`).
pub trait Pollable: Send + Sync {
    /// The readiness of one open description now (`mode`: its access mode,
    /// since a pipe's two ends share one inode and an `O_RDWR` FIFO answers
    /// for both). With a waker, return a guard unless the caller re-polls you;
    /// subscribe no later than the mask is read, so no change falls between.
    fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>);
}

trait PollHeadOps: Send + Sync {
    fn unsubscribe(&self, id: u64);
}

/// RAII subscription to a [`PollHead`]: dropping it removes the waker from
/// the head's subscriber list.
#[must_use = "dropping a PollGuard immediately unsubscribes"]
pub struct PollGuard {
    head: Weak<dyn PollHeadOps>,
    id: u64,
}

impl fmt::Debug for PollGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollGuard").field("id", &self.id).finish()
    }
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        if let Some(head) = self.head.upgrade() {
            head.unsubscribe(self.id);
        }
    }
}

static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

struct Subscriber {
    id: u64,
    waker: Arc<dyn PollWaker>,
}

struct PollHeadInner<F: LockFamily> {
    subscribers: F::LeafLock<LinkedList<Subscriber>>,
}

impl<F: LockFamily + 'static> PollHeadOps for PollHeadInner<F> {
    fn unsubscribe(&self, id: u64) {
        let removed = {
            let mut list = self.subscribers.lock();
            if let Some(pos) = list.iter().position(|s| s.id == id) {
                let mut tail = list.split_off(pos);
                let mut rest = tail.split_off(1);
                list.append(&mut rest);
                Some(tail)
            } else {
                None
            }
        };
        // Drop the removed list node and its `Arc<dyn PollWaker>` outside the
        // leaf lock so deallocation never runs under `PollHead`'s lock.
        drop(removed);
    }
}

/// Subscriber list for a [`Pollable`] readiness source, protected by `F`'s
/// leaf lock (section 9, level-10 `e`).
pub struct PollHead<F: LockFamily> {
    inner: Arc<PollHeadInner<F>>,
}

impl<F: LockFamily> Clone for PollHead<F> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<F: LockFamily> Default for PollHead<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: LockFamily> PollHead<F> {
    /// Create an empty subscriber list.
    pub fn new() -> Self {
        Self { inner: Arc::new(PollHeadInner { subscribers: F::LeafLock::new(LinkedList::new()) }) }
    }

    /// Subscribe `waker` to subsequent [`Self::notify`] calls until the
    /// returned [`PollGuard`] drops. Allocates the list node before taking the
    /// leaf lock.
    pub fn subscribe(&self, waker: Arc<dyn PollWaker>) -> PollGuard
    where
        F: 'static,
    {
        let id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "poll subscriber id exhausted");
        // Allocate the list node before acquiring the leaf lock so splicing
        // under the lock performs zero heap allocations.
        let mut node = LinkedList::new();
        node.push_back(Subscriber { id, waker });
        {
            let mut list = self.inner.subscribers.lock();
            list.append(&mut node);
        }
        let head = Arc::downgrade(&self.inner) as Weak<dyn PollHeadOps>;
        PollGuard { head, id }
    }

    /// Notify every current subscriber that the bits in `ready` may have
    /// turned on. Does nothing if `ready` is empty.
    pub fn notify(&self, ready: PollMask) {
        if ready.is_empty() {
            return;
        }
        let list = self.inner.subscribers.lock();
        for sub in list.iter() {
            sub.waker.wake(ready);
        }
    }

    /// Number of active subscribers currently registered on this head.
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.lock().len()
    }
}

impl<F: LockFamily> fmt::Debug for PollHead<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollHead").field("subscribers", &self.subscriber_count()).finish()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize};
    use std::sync::{Condvar, Mutex};
    use std::thread;

    struct HostMutex<T>(Mutex<T>);

    impl<T> VfsLock<T> for HostMutex<T> {
        type Guard<'a>
            = std::sync::MutexGuard<'a, T>
        where
            T: 'a;

        fn new(val: T) -> Self {
            Self(Mutex::new(val))
        }

        fn lock(&self) -> Self::Guard<'_> {
            self.0.lock().unwrap()
        }

        fn get_mut(&mut self) -> &mut T {
            self.0.get_mut().unwrap()
        }
    }

    struct HostLocks;

    impl LockFamily for HostLocks {
        type Lock<T> = HostMutex<T>;
        type LeafLock<T: Send> = HostMutex<T>;
    }

    struct RecordingWaker {
        wakes: AtomicUsize,
        last_mask: AtomicU16,
    }

    impl RecordingWaker {
        fn new() -> Arc<Self> {
            Arc::new(Self { wakes: AtomicUsize::new(0), last_mask: AtomicU16::new(0) })
        }
    }

    impl PollWaker for RecordingWaker {
        fn wake(&self, ready: PollMask) {
            self.last_mask.store(ready.bits(), Ordering::SeqCst);
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn poll_mask_bits_and_sets_match_linux() {
        assert_eq!(PollMask::EMPTY.bits(), 0);
        assert_eq!(PollMask::IN.bits(), 0x0001);
        assert_eq!(PollMask::PRI.bits(), 0x0002);
        assert_eq!(PollMask::OUT.bits(), 0x0004);
        assert_eq!(PollMask::ERR.bits(), 0x0008);
        assert_eq!(PollMask::HUP.bits(), 0x0010);
        assert_eq!(PollMask::NVAL.bits(), 0x0020);
        assert_eq!(PollMask::RDNORM.bits(), 0x0040);
        assert_eq!(PollMask::RDBAND.bits(), 0x0080);
        assert_eq!(PollMask::WRNORM.bits(), 0x0100);
        assert_eq!(PollMask::WRBAND.bits(), 0x0200);
        assert_eq!(PollMask::RDHUP.bits(), 0x2000);

        assert_eq!(PollMask::ALWAYS_READY, PollMask::IN | PollMask::OUT | PollMask::RDNORM | PollMask::WRNORM);
        assert_eq!(
            PollMask::POLLIN_SET,
            PollMask::RDNORM | PollMask::RDBAND | PollMask::IN | PollMask::HUP | PollMask::ERR | PollMask::NVAL
        );
        assert_eq!(
            PollMask::POLLOUT_SET,
            PollMask::WRBAND | PollMask::WRNORM | PollMask::OUT | PollMask::ERR | PollMask::NVAL
        );
        assert_eq!(PollMask::POLLEX_SET, PollMask::PRI | PollMask::NVAL);
    }

    #[test]
    fn poll_mask_filter_and_bitwise_ops() {
        let ready = PollMask::IN | PollMask::OUT | PollMask::ERR | PollMask::HUP | PollMask::PRI;
        // Requested nothing: only ERR and HUP survive.
        assert_eq!(ready.filter_events(PollMask::EMPTY), PollMask::ERR | PollMask::HUP);
        // Requested IN: IN, ERR, HUP survive; OUT and PRI do not.
        assert_eq!(ready.filter_events(PollMask::IN), PollMask::IN | PollMask::ERR | PollMask::HUP);
        // Requested OUT | PRI: OUT, PRI, ERR, HUP survive.
        assert_eq!(
            ready.filter_events(PollMask::OUT | PollMask::PRI),
            PollMask::OUT | PollMask::PRI | PollMask::ERR | PollMask::HUP
        );

        let mut m = PollMask::EMPTY;
        assert!(m.is_empty());
        m |= PollMask::IN | PollMask::RDNORM;
        assert!(m.contains(PollMask::IN));
        assert!(m.contains(PollMask::IN | PollMask::RDNORM));
        assert!(!m.contains(PollMask::OUT));
        assert!(m.intersects(PollMask::POLLIN_SET));
        assert!(!m.intersects(PollMask::POLLOUT_SET));
        assert!(!m.intersects(PollMask::POLLEX_SET));
        m &= PollMask::IN;
        assert_eq!(m, PollMask::IN);
        assert_eq!(m.union(PollMask::OUT).intersection(PollMask::OUT), PollMask::OUT);
        assert_eq!((!PollMask::EMPTY).bits(), u16::MAX);
    }

    #[test]
    fn poll_head_subscribe_notify_and_drop_unsubscribe() {
        let head: PollHead<HostLocks> = PollHead::new();
        let w1 = RecordingWaker::new();
        let w2 = RecordingWaker::new();

        assert_eq!(head.subscriber_count(), 0);

        let g1 = head.subscribe(Arc::clone(&w1) as Arc<dyn PollWaker>);
        let g2 = head.subscribe(Arc::clone(&w2) as Arc<dyn PollWaker>);
        // Subscribing the same waker twice registers two independent guards.
        let g1_dup = head.subscribe(Arc::clone(&w1) as Arc<dyn PollWaker>);
        assert_eq!(head.subscriber_count(), 3);

        // Empty notify does not wake anyone.
        head.notify(PollMask::EMPTY);
        assert_eq!(w1.wakes.load(Ordering::SeqCst), 0);
        assert_eq!(w2.wakes.load(Ordering::SeqCst), 0);

        head.notify(PollMask::IN | PollMask::RDNORM);
        assert_eq!(w1.wakes.load(Ordering::SeqCst), 2);
        assert_eq!(w2.wakes.load(Ordering::SeqCst), 1);
        assert_eq!(w1.last_mask.load(Ordering::SeqCst), (PollMask::IN | PollMask::RDNORM).bits());
        assert_eq!(w2.last_mask.load(Ordering::SeqCst), (PollMask::IN | PollMask::RDNORM).bits());

        // Dropping one of w1's guards removes only that subscription.
        drop(g1);
        assert_eq!(head.subscriber_count(), 2);
        head.notify(PollMask::OUT);
        assert_eq!(w1.wakes.load(Ordering::SeqCst), 3);
        assert_eq!(w2.wakes.load(Ordering::SeqCst), 2);
        assert_eq!(w1.last_mask.load(Ordering::SeqCst), PollMask::OUT.bits());

        // Dropping w1's second guard removes w1 completely; w2 stays subscribed.
        drop(g1_dup);
        assert_eq!(head.subscriber_count(), 1);
        head.notify(PollMask::HUP);
        assert_eq!(w1.wakes.load(Ordering::SeqCst), 3);
        assert_eq!(w2.wakes.load(Ordering::SeqCst), 3);
        assert_eq!(w2.last_mask.load(Ordering::SeqCst), PollMask::HUP.bits());

        // Dropping the head before the last guard is safe (no panic).
        drop(head);
        drop(g2);
    }

    #[test]
    fn poll_head_loses_no_change_between_poll_and_subscription() {
        // Exercises this mock source's lock-ordered subscribe-and-read protocol
        // against concurrent `PollHead::notify` calls: across thousands of
        // flips, the consumer must never observe an empty mask AND miss the
        // wake.
        struct MockSource {
            state: Mutex<PollMask>,
            head: PollHead<HostLocks>,
        }

        impl MockSource {
            fn new() -> Self {
                Self { state: Mutex::new(PollMask::EMPTY), head: PollHead::new() }
            }

            fn set_ready(&self, mask: PollMask) {
                let mut st = self.state.lock().unwrap();
                *st = mask;
                self.head.notify(mask);
            }

            fn clear(&self) {
                let mut st = self.state.lock().unwrap();
                *st = PollMask::EMPTY;
            }
        }

        impl Pollable for MockSource {
            fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
                let st = self.state.lock().unwrap();
                let guard = waker.map(|w| self.head.subscribe(Arc::clone(w)));
                (*st, guard)
            }
        }

        struct CondWaker {
            fired: AtomicBool,
            lock: Mutex<()>,
            cv: Condvar,
        }

        impl CondWaker {
            fn new() -> Arc<Self> {
                Arc::new(Self { fired: AtomicBool::new(false), lock: Mutex::new(()), cv: Condvar::new() })
            }

            fn wait_until_fired(&self) {
                let mut g = self.lock.lock().unwrap();
                while !self.fired.load(Ordering::SeqCst) {
                    g = self.cv.wait(g).unwrap();
                }
            }
        }

        impl PollWaker for CondWaker {
            fn wake(&self, _ready: PollMask) {
                let _g = self.lock.lock().unwrap();
                self.fired.store(true, Ordering::SeqCst);
                self.cv.notify_one();
            }
        }

        const ROUNDS: usize = 2000;
        let src = Arc::new(MockSource::new());
        let round = Arc::new(AtomicUsize::new(0));
        let go = Arc::new((Mutex::new(0usize), Condvar::new()));

        let prod_src = Arc::clone(&src);
        let prod_go = Arc::clone(&go);
        let producer = thread::spawn(move || {
            for r in 1..=ROUNDS {
                {
                    let (lk, cv) = &*prod_go;
                    let mut g = lk.lock().unwrap();
                    while *g < r {
                        g = cv.wait(g).unwrap();
                    }
                }
                prod_src.set_ready(PollMask::IN | PollMask::RDNORM);
            }
        });

        for r in 1..=ROUNDS {
            src.clear();
            let waker = CondWaker::new();
            let waker_dyn: Arc<dyn PollWaker> = Arc::clone(&waker) as Arc<dyn PollWaker>;
            // Kick off the producer concurrently with our poll+subscribe.
            {
                let (lk, cv) = &*go;
                let mut g = lk.lock().unwrap();
                *g = r;
                cv.notify_one();
            }
            let (mask, _guard) = src.poll(AccessMode::ReadOnly, Some(&waker_dyn));
            if !mask.intersects(PollMask::IN) {
                // Producer hadn't flipped before our poll; our subscription
                // MUST catch its transition.
                waker.wait_until_fired();
                let (after, _) = src.poll(AccessMode::ReadOnly, None);
                assert!(after.contains(PollMask::IN | PollMask::RDNORM), "round {r}: missed readiness after wake");
            }
            round.store(r, Ordering::Relaxed);
        }

        producer.join().unwrap();
        assert_eq!(src.head.subscriber_count(), 0);
    }
}
