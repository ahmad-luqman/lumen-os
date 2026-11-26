//! Backing identities and allocation-free locked futex queues. User faults,
//! scheduler notifications and node destruction happen outside bucket locks.

use alloc::{boxed::Box, sync::Arc};
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, Ordering};
use lumen_futex::{BucketIndex, CancelOutcome, FutexNode, FutexTable, Woken};

use super::{errno, general, process::UserThread};
use crate::{sched, sync::SpinLock, time, user};

/// A private operation never matches a shared operation, even at the same
/// virtual address. Shared keys identify the backing observed under Lock 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FutexKey {
    Private { space: u64, uaddr: u64 },
    Anon { space: u64, uaddr: u64 },
    File { file_id: u64, offset: u64 },
    SharedFrame { frame: u64, offset: u64 },
}

impl FutexKey {
    fn bucket(self) -> usize {
        let (tag, a, b) = match self {
            Self::Private { space, uaddr } => (0, space, uaddr),
            Self::Anon { space, uaddr } => (1, space, uaddr),
            Self::File { file_id, offset } => (2, file_id, offset),
            Self::SharedFrame { frame, offset } => (3, frame, offset),
        };
        let mut h = a.rotate_left(17) ^ b ^ tag;
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((h ^ (h >> 31)) & 255) as usize
    }
}

struct FutexWaiter {
    bucket: BucketIndex,
    woken: AtomicBool,
    queue: sched::WaitQueue,
}

type Table = FutexTable<FutexKey, Arc<FutexWaiter>>;
static FUTEX_BUCKETS: [SpinLock<Table>; 256] = [const { SpinLock::new(FutexTable::new()) }; 256];

fn validate_address(addr: u64) -> Result<(), u32> {
    if !(lumen_vm::USER_MIN..lumen_vm::USER_MAX).contains(&addr) {
        return Err(errno::EFAULT);
    }
    if !addr.is_multiple_of(4) {
        return Err(errno::EINVAL);
    }
    Ok(())
}

pub(super) fn read_word(addr: u64) -> Result<u32, u32> {
    let mut word = [0; 4];
    user::copy_from_user(&mut word, addr).map_err(|_| errno::EFAULT)?;
    Ok(u32::from_ne_bytes(word))
}

fn fault_write(addr: u64) -> Result<(), u32> {
    user::fault_in_writable(addr).map_err(|e| match e {
        user::FaultIn::Fault => errno::EFAULT,
        user::FaultIn::OutOfMemory => errno::ENOMEM,
    })
}

fn key(addr: u64, private: bool) -> Result<FutexKey, u32> {
    validate_address(addr)?;
    let space = sched::current_address_space().ok_or(errno::EFAULT)?;
    if private {
        return Ok(FutexKey::Private { space: space.root().as_u64(), uaddr: addr });
    }
    loop {
        read_word(addr)?;
        if let Some(key) = space.futex_key(addr)? {
            return Ok(key);
        }
        // A writable private file mapping must be keyed by its private copy
        // even before userspace's first store. Never fault under a bucket.
        fault_write(addr)?;
    }
}

fn notify(woken: Woken<FutexKey, Arc<FutexWaiter>>) -> u64 {
    let count = woken.len() as u64;
    for node in woken {
        let waiter = node.waiter();
        waiter.woken.store(true, Ordering::Release);
        waiter.queue.wake_one();
    }
    count
}

/// Wake matching waiters, notifying and freeing them only after unlocking.
pub fn wake(addr: u64, private: bool, count: i32, bits: NonZeroU32) -> Result<u64, u32> {
    let key = key(addr, private)?;
    let woken = FUTEX_BUCKETS[key.bucket()].lock().wake(key, count, bits);
    Ok(notify(woken))
}

/// Check and enqueue atomically with respect to wakes. A wake that claimed
/// the node wins over a racing timeout or signal, including after requeue.
pub(super) fn wait(
    addr: u64,
    private: bool,
    expected: u32,
    bits: NonZeroU32,
    deadline: Option<u64>,
) -> Result<u64, u32> {
    validate_address(addr)?;
    let waiter = Arc::new(FutexWaiter {
        bucket: BucketIndex::new(0),
        woken: AtomicBool::new(false),
        queue: sched::WaitQueue::new(),
    });
    let mut node = Box::new(FutexNode::new(waiter.clone(), bits));
    loop {
        let key = key(addr, private)?;
        let mut bucket = FUTEX_BUCKETS[key.bucket()].lock();
        let value = user::read_u32_nofault(addr);
        match value {
            Err(_) => {
                drop(bucket);
                read_word(addr)?;
                continue;
            }
            Ok(value) if value != expected => return Err(errno::EAGAIN),
            Ok(_) => {}
        }
        waiter.bucket.set(key.bucket());
        bucket.enqueue(key, node);
        drop(bucket);
        let reason = waiter.queue.wait_interruptible(deadline, || waiter.woken.load(Ordering::Acquire));
        #[cfg(feature = "ktest")]
        cancel_window::pause(addr);
        match waiter.bucket.cancel_in(|b| FUTEX_BUCKETS[b].lock(), |w| Arc::ptr_eq(w, &waiter)) {
            CancelOutcome::AlreadyWoken => return Ok(0),
            CancelOutcome::Cancelled(cancelled) => node = cancelled,
        }
        if deadline.is_some_and(|d| time::now_ns() >= d) {
            return Err(errno::ETIMEDOUT);
        }
        if reason == sched::Wake::Interrupted {
            return Err(super::signal::ERESTARTSYS);
        }
    }
}

/// Requeue under ascending bucket locks; a comparison fault releases both
/// before paging in and recomputing both keys.
pub(super) fn requeue(
    addr: u64,
    private: bool,
    count: i32,
    dest: u64,
    moved: i32,
    expected: Option<u32>,
) -> Result<u64, u32> {
    lumen_futex::check_requeue_counts(count, moved).map_err(|_| errno::EINVAL)?;
    loop {
        let from = key(addr, private)?;
        let to = key(dest, private)?;
        let (b1, b2) = (from.bucket(), to.bucket());
        let mut first = FUTEX_BUCKETS[b1.min(b2)].lock();
        let mut second = (b1 != b2).then(|| FUTEX_BUCKETS[b1.max(b2)].lock());
        if let Some(expected) = expected {
            match user::read_u32_nofault(addr) {
                Err(_) => {
                    drop(second);
                    drop(first);
                    read_word(addr)?;
                    continue;
                }
                Ok(value) if value != expected => return Err(errno::EAGAIN),
                Ok(_) => {}
            }
        }
        let mark = |waiter: &Arc<FutexWaiter>, _| waiter.bucket.set(b2);
        let result = if let Some(second) = second.as_mut() {
            let (src, dst) = if b1 < b2 { (&mut first, second) } else { (second, &mut first) };
            src.requeue_into(from, dst, to, count, moved, mark)
        } else {
            first.requeue(from, to, count, moved, mark)
        };
        drop(second);
        drop(first);
        let (woken, moved) = result.map_err(|_| errno::EINVAL)?;
        return Ok(notify(woken) + moved as u64);
    }
}

fn read_u64(addr: u64) -> Result<u64, u32> {
    let mut word = [0; 8];
    user::copy_from_user(&mut word, addr).map_err(|_| errno::EFAULT)?;
    Ok(u64::from_ne_bytes(word))
}

fn owner_died(addr: u64, tid: u32, pending: bool) -> Result<(), u32> {
    validate_address(addr)?;
    loop {
        let word = read_word(addr)?;
        if word & general::FUTEX_TID_MASK != tid {
            if pending && (word & general::FUTEX_TID_MASK == 0 || word & general::FUTEX_WAITERS == 0) {
                wake(addr, false, 1, lumen_futex::FUTEX_BITSET_MATCH_ANY)?;
            }
            return Ok(());
        }
        let new = (word & general::FUTEX_WAITERS) | general::FUTEX_OWNER_DIED;
        match user::cmpxchg_u32(addr, word, new) {
            Err(_) => fault_write(addr)?,
            Ok(found) if found != word => continue,
            Ok(_) => {
                if word & general::FUTEX_WAITERS != 0 {
                    wake(addr, false, 1, lumen_futex::FUTEX_BITSET_MATCH_ANY)?;
                }
                return Ok(());
            }
        }
    }
}

fn walk_robust(head: u64, tid: u32) -> Result<(), u32> {
    let mut entry = read_u64(head)?;
    let offset = read_u64(head.checked_add(8).ok_or(errno::EFAULT)?)?;
    let pending = read_u64(head.checked_add(16).ok_or(errno::EFAULT)?)?;
    for _ in 0..2048 {
        let address = entry & !1;
        if address == head {
            break;
        }
        let next = read_u64(address)?;
        // The low pointer bit denotes a PI entry, unsupported by this ABI.
        if entry & 1 == 0 && address != pending & !1 {
            owner_died(address.wrapping_add(offset), tid, false)?;
        }
        entry = next;
    }
    if pending != 0 && pending & 1 == 0 {
        owner_died(pending.wrapping_add(offset), tid, true)?;
    }
    Ok(())
}

/// Release a thread's robust list on its old address space, before clearing
/// its child tid. A malformed list aborts cleanup, never the thread's exit.
pub fn exit_robust_list(thread: &UserThread) {
    let head = thread.robust_list.swap(0, Ordering::Relaxed);
    if head != 0 {
        let _ = walk_robust(head, thread.tid());
    }
}

/// A one-shot test pause after a wait returns and before it cancels its node.
#[cfg(feature = "ktest")]
mod cancel_window {
    use super::*;
    pub(super) struct Pause {
        pub addr: u64,
        pub entered: AtomicBool,
        pub release: AtomicBool,
    }
    pub(super) static ACTIVE: SpinLock<Option<Arc<Pause>>> = SpinLock::new(None);
    pub(super) fn pause(addr: u64) {
        let pause = ACTIVE.lock().as_ref().filter(|p| p.addr == addr).cloned();
        if let Some(pause) = pause {
            pause.entered.store(true, Ordering::Release);
            while !pause.release.load(Ordering::Acquire) {
                time::sleep(1_000_000);
            }
        }
    }
}

crate::ktest!(
    fn futex_cancel_follows_requeue_and_wake_wins() {
        use super::process::{KTEST_USER_VA as A, in_test_process, spawn_test_thread};
        use crate::{kassert, kassert_eq};
        use lumen_futex::FUTEX_BITSET_MATCH_ANY as ANY;
        in_test_process(|_| {
            let source = key(A, true).map_err(|_| "source key")?;
            // Exercise both the one-lock and the ordered two-lock paths.
            for same_bucket in [true, false] {
                let dest = (A + 4..A + 4096)
                    .step_by(4)
                    .find(|&addr| key(addr, true).is_ok_and(|k| (k.bucket() == source.bucket()) == same_bucket))
                    .ok_or("no destination bucket")?;
                let destination = key(dest, true).map_err(|_| "destination key")?;
                let child = spawn_test_thread(move |_| {
                    let result = wait(A, true, 0, ANY, None);
                    sched::clear_current_sigpending();
                    result
                })?;
                kassert!(child.settle());
                kassert_eq!(requeue(A, true, 0, dest, 1, Some(0)), Ok(1));
                super::signal::raise_for_test(child.thread(), lumen_signal::Signal::USR1);
                kassert_eq!(child.join()?, Err(super::signal::ERESTARTSYS));
                kassert_eq!(FUTEX_BUCKETS[destination.bucket()].lock().count_for_key(destination), 0);
            }
            // Force a signal return, then let a wake claim the node before
            // cancellation. Returning EINTR without checking AlreadyWoken fails.
            let pause = Arc::new(cancel_window::Pause {
                addr: A,
                entered: AtomicBool::new(false),
                release: AtomicBool::new(false),
            });
            *cancel_window::ACTIVE.lock() = Some(pause.clone());
            let child = spawn_test_thread(move |_| {
                let result = wait(A, true, 0, ANY, None);
                sched::clear_current_sigpending();
                result
            })?;
            let blocked = child.settle();
            super::signal::raise_for_test(child.thread(), lumen_signal::Signal::USR1);
            while !pause.entered.load(Ordering::Acquire) && !child.finished() {
                time::sleep(1_000_000);
            }
            let woke = wake(A, true, 1, ANY);
            pause.release.store(true, Ordering::Release);
            let result = child.join()?;
            *cancel_window::ACTIVE.lock() = None;
            kassert!(blocked);
            kassert_eq!(woke, Ok(1));
            kassert_eq!(result, Ok(0));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn robust_walk_checks_owners_pending_and_faults() {
        use super::process::{KTEST_USER_VA as A, current_thread, in_test_process};
        use crate::{kassert, kassert_eq};
        use lumen_futex::FUTEX_BITSET_MATCH_ANY as ANY;
        in_test_process(|tid| {
            let put64 = |addr, word: u64| user::copy_to_user(addr, &word.to_ne_bytes()).map_err(|_| "put64");
            let put32 = |addr, word: u32| user::copy_to_user(addr, &word.to_ne_bytes()).map_err(|_| "put32");
            let head = A;
            let entry = A + 32;
            let pending = A + 64;
            put64(head, entry)?;
            put64(head + 8, 8)?;
            put64(head + 16, pending)?;
            put64(entry, head)?;
            put32(entry + 8, tid | general::FUTEX_WAITERS)?;
            put32(pending + 8, tid)?;
            current_thread().robust_list.store(head, Ordering::Relaxed);
            exit_robust_list(&current_thread());
            kassert_eq!(current_thread().robust_list.load(Ordering::Relaxed), 0);
            kassert_eq!(read_word(entry + 8), Ok(general::FUTEX_WAITERS | general::FUTEX_OWNER_DIED));
            kassert_eq!(read_word(pending + 8), Ok(general::FUTEX_OWNER_DIED));

            // Pending cleanup may wake a waiter without owning the word;
            // a non-pending entry may not. Check all branches with real nodes.
            for (word, pending, should_wake) in [
                (0, true, true),
                (general::FUTEX_WAITERS, true, true),
                (tid + 1, true, true),
                ((tid + 1) | general::FUTEX_WAITERS, true, false),
                (0, false, false),
            ] {
                put32(entry + 8, word)?;
                let key = key(entry + 8, false).map_err(|_| "key")?;
                let waiter = Arc::new(FutexWaiter {
                    bucket: BucketIndex::new(key.bucket()),
                    woken: AtomicBool::new(false),
                    queue: sched::WaitQueue::new(),
                });
                let node = Box::new(FutexNode::new(waiter.clone(), ANY));
                FUTEX_BUCKETS[key.bucket()].lock().enqueue(key, node);
                let handled = owner_died(entry + 8, tid, pending);
                let cancelled = waiter.bucket.cancel_in(|b| FUTEX_BUCKETS[b].lock(), |w| Arc::ptr_eq(w, &waiter));
                kassert_eq!(handled, Ok(()));
                kassert_eq!(read_word(entry + 8), Ok(word));
                kassert_eq!(waiter.woken.load(Ordering::Acquire), should_wake);
                kassert_eq!(matches!(cancelled, CancelOutcome::AlreadyWoken), should_wake);
            }
            // A fault in the next pointer aborts before mutating its word or
            // touching pending. A negative offset is handled as signed addition.
            put64(head, 0x4000)?;
            put32(A + 72, tid)?;
            kassert_eq!(walk_robust(head, tid), Err(errno::EFAULT));
            kassert_eq!(read_word(A + 72), Ok(tid));
            put64(head, entry)?;
            put64(head + 8, (-4i64) as u64)?;
            put64(head + 16, 0)?;
            put64(entry, head)?;
            put32(entry - 4, tid)?;
            kassert_eq!(walk_robust(head, tid), Ok(()));
            kassert_eq!(read_word(entry - 4), Ok(general::FUTEX_OWNER_DIED));
            // A cyclic userspace list is bounded even when no owner matches.
            put64(entry, entry)?;
            kassert!(walk_robust(head, tid).is_ok());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn futex_file_keys_and_private_cow_before_wait() {
        use super::process::{in_test_process, spawn_test_thread};
        use crate::mm::user_vm::{Sharing, file_vm_open};
        use crate::vfs::page_cache::ktest_files::{self, TmpfsVolume};
        use crate::{kassert, kassert_eq};
        use lumen_futex::FUTEX_BITSET_MATCH_ANY as ANY;
        use lumen_vm::{MapPlace, PAGE_SIZE, Prot};
        in_test_process(|_| {
            let volume = TmpfsVolume::new(383, 8 * PAGE_SIZE as usize);
            let file = ktest_files::file(&volume.root, "futex", &[0; 8192])?;
            let other = ktest_files::file(&volume.root, "other", &[0; 4096])?;
            let space = sched::current_address_space().ok_or("space")?;
            let map = |file, offset, prot, sharing| {
                space
                    .mmap_file(MapPlace::Hint(None), PAGE_SIZE, prot, file_vm_open(file)?, offset, sharing)
                    .map_err(|_| "file mmap")
            };
            let rw = Prot::READ | Prot::WRITE;
            let first = map(&file, 0, rw, Sharing::Shared)?;
            let alias = map(&file, 0, Prot::READ, Sharing::Private)?;
            let next = map(&file, PAGE_SIZE, rw, Sharing::Shared)?;
            let other = map(&other, 0, rw, Sharing::Shared)?;
            kassert_eq!(key(first, false), key(alias, false));
            kassert!(key(first, false).is_ok_and(|k| matches!(k, FutexKey::File { .. })));
            kassert!(key(first, false) != key(first + 4, false));
            kassert!(key(first, false) != key(next, false));
            kassert!(key(first, false) != key(other, false));
            kassert!(key(first, true) != key(alias, true));
            kassert!(key(first, true) != key(first, false));

            // The parent does not resolve this key until WAIT is blocked:
            // probing with requeue beforehand could itself break CoW and hide
            // a missing fault_in_writable in the waiter's key resolution.
            let private = map(&file, 0, rw, Sharing::Private)?;
            let child = spawn_test_thread(move |_| wait(private, false, 0, ANY, None))?;
            let blocked = child.settle();
            let stored = user::copy_to_user(private, &1u32.to_ne_bytes());
            let woke = wake(private, false, 1, ANY);
            // On failure, still release the child so the ktest reports rather
            // than hangs. The injected signal cannot turn a missing wake into 0.
            super::signal::raise_for_test(child.thread(), lumen_signal::Signal::USR1);
            let result = child.join()?;
            kassert!(blocked);
            kassert_eq!(stored, Ok(()));
            kassert_eq!(woke, Ok(1));
            kassert_eq!(result, Ok(0));
            kassert_eq!(read_word(first), Ok(0));
            kassert!(key(private, false).is_ok_and(|k| matches!(k, FutexKey::Anon { .. })));
            Ok(())
        })?
    }
);
