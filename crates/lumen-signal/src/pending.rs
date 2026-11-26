//! Pending queues and a deterministic subset of Linux kernel/signal.c policy.

use alloc::alloc::{Layout, alloc};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;
use linux_raw_sys::{errno, general};

use crate::{Blocked, DefaultAction, STOP_LIST, STOP_SIGNALS, SigSet, Signal};

/// An initialized 128-byte Linux siginfo image with a valid si_signo. Payload
/// bytes are opaque; callers are responsible for syscall-specific si_code checks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SigInfo([u8; 128]);

impl SigInfo {
    pub fn new(signal: Signal, code: i32) -> Self {
        let mut bytes = [0; 128];
        bytes[..4].copy_from_slice(&signal.number().to_le_bytes());
        bytes[8..12].copy_from_slice(&code.to_le_bytes());
        Self(bytes)
    }

    /// Copies Linux's 48-byte kernel_siginfo payload, stamps the syscall's
    /// signal, and zeroes the expansion bytes as on Linux delivery. Linux's
    /// -E2BIG check for unknown layouts with nonzero expansion is not done here;
    /// the syscall layer must perform that layout-dependent check.
    pub fn from_user(signal: Signal, mut bytes: [u8; 128]) -> Self {
        bytes[..4].copy_from_slice(&signal.number().to_le_bytes());
        bytes[48..].fill(0);
        Self(bytes)
    }

    /// Decode an already trusted image; syscall inputs must use from_user.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; 128]) -> Option<Self> {
        Signal::new(u32::from_le_bytes(bytes[..4].try_into().ok()?))?;
        Some(Self(bytes))
    }

    pub fn signal(&self) -> Signal {
        // Construction guarantees si_signo is in 1..=64.
        Signal(self.0[0])
    }

    pub const fn bytes(&self) -> &[u8; 128] {
        &self.0
    }

    /// A POSIX timer notification with Linux's x86-64 timer payload.
    pub fn timer(signal: Signal, id: i32, value: u64) -> Self {
        let mut info = Self::new(signal, general::SI_TIMER);
        info.0[16..20].copy_from_slice(&id.to_le_bytes());
        info.0[24..32].copy_from_slice(&value.to_le_bytes());
        info
    }

    /// The timer id only for SI_TIMER; other union layouts cannot match.
    pub fn timer_id(&self) -> Option<i32> {
        (i32::from_le_bytes(self.0[8..12].try_into().unwrap()) == general::SI_TIMER)
            .then(|| i32::from_le_bytes(self.0[16..20].try_into().unwrap()))
    }

    /// Stamp the last delivery's bounded overrun count.
    pub fn set_timer_overrun(&mut self, overrun: i32) {
        self.0[20..24].copy_from_slice(&overrun.to_le_bytes());
    }

    /// A fault siginfo with si_addr (offset 16 on x86-64) set to the fault address.
    pub fn fault(signal: Signal, code: i32, address: u64) -> Self {
        let mut info = Self::new(signal, code);
        info.0[16..24].copy_from_slice(&address.to_le_bytes());
        info
    }
}

impl fmt::Debug for SigInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigInfo").field("signal", &self.signal()).finish_non_exhaustive()
    }
}

const _: () = {
    assert!(size_of::<SigInfo>() == size_of::<general::siginfo_t>());
    assert!(size_of::<SigInfo>() == general::SI_MAX_SIZE as usize);
    // include/uapi/asm-generic/siginfo.h: __SIGINFO header and aligned union.
    assert!(core::mem::offset_of!(general::siginfo__bindgen_ty_1__bindgen_ty_1, si_signo) == 0);
    assert!(core::mem::offset_of!(general::siginfo__bindgen_ty_1__bindgen_ty_1, si_errno) == 4);
    assert!(core::mem::offset_of!(general::siginfo__bindgen_ty_1__bindgen_ty_1, si_code) == 8);
    #[cfg(target_pointer_width = "64")]
    assert!(core::mem::offset_of!(general::siginfo__bindgen_ty_1__bindgen_ty_1, _sifields) == 16);
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enqueued {
    Queued,
    Coalesced,
    /// SI_USER exhaustion sets the pending bit without adding a queued entry.
    /// An existing entry of this number absorbs the send when it is dequeued.
    InfoLost,
}

/// The kernel classifies the validated sender; this is not inferred from a
/// user-controlled si_code. SiUser covers kill() and the kernel's special
/// SEND_SIG_NOINFO/SEND_SIG_PRIV sends (Linux is_si_special). SI_TKILL from
/// tkill/tgkill and rt_sigqueueinfo are Other, returning -EAGAIN on exhaustion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderClass {
    SiUser,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingError {
    Full,
    NoMemory,
    InvalidThread,
}

impl PendingError {
    /// Negative Linux errno. InvalidThread is a kernel bookkeeping bug and has
    /// no user errno. For enqueue, exhaustion/allocation failure are -EAGAIN.
    /// NoMemory from new/add_thread is the caller's to map (e.g. -ENOMEM on clone).
    pub const fn errno(self) -> Option<i32> {
        match self {
            Self::Full | Self::NoMemory => Some(-(errno::EAGAIN as i32)),
            Self::InvalidThread => None,
        }
    }
}

/// Queue-owned origin, separate from the user-visible siginfo image. Only
/// enqueue_timer attaches an instance; ordinary enqueue never infers one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingSignal {
    info: SigInfo,
    timer_instance: Option<u64>,
}
impl PendingSignal {
    pub fn info(self) -> SigInfo {
        self.info
    }
    pub fn timer_instance(self) -> Option<u64> {
        self.timer_instance
    }
    fn signal(&self) -> Signal {
        self.info.signal()
    }
}

/// One pending set, mutated only through PendingSignals so generation always
/// performs process-wide stop/continue cancellation. Capacity is per queue,
/// not Linux's per-user RLIMIT_SIGPENDING accounting.
#[derive(Debug)]
struct Pending {
    set: SigSet,
    standard: [Option<PendingSignal>; 31],
    realtime: VecDeque<PendingSignal>,
    cap: usize,
}

impl Pending {
    fn new(cap: usize) -> Self {
        Self { set: SigSet::EMPTY, standard: [None; 31], realtime: VecDeque::new(), cap }
    }

    fn enqueue(&mut self, info: PendingSignal, sender: SenderClass) -> Result<Enqueued, PendingError> {
        let signal = info.signal();
        if signal.is_realtime() {
            let capacity = if self.realtime.len() == self.cap {
                Err(PendingError::Full)
            } else {
                self.realtime.try_reserve(1).map_err(|_| PendingError::NoMemory)
            };
            if let Err(error) = capacity {
                if sender != SenderClass::SiUser {
                    return Err(error);
                }
                self.set.insert(signal);
                return Ok(Enqueued::InfoLost);
            }
            self.realtime.push_back(info);
        } else {
            let slot = &mut self.standard[signal.index()];
            if slot.is_some() {
                return Ok(Enqueued::Coalesced);
            }
            *slot = Some(info);
        }
        self.set.insert(signal);
        Ok(Enqueued::Queued)
    }

    fn discard(&mut self, signal: Signal) {
        if signal.is_realtime() {
            self.realtime.retain(|info| info.signal() != signal);
        } else {
            self.standard[signal.index()] = None;
        }
        self.set.remove(signal);
    }

    fn any(&self, predicate: &mut impl FnMut(&PendingSignal) -> bool) -> bool {
        self.standard.iter().flatten().chain(self.realtime.iter()).any(predicate)
    }

    fn discard_if(&mut self, predicate: &mut impl FnMut(&PendingSignal) -> bool) {
        for slot in &mut self.standard {
            if slot.as_ref().is_some_and(&mut *predicate) {
                self.set.remove(slot.take().unwrap().signal());
            }
        }
        let mut removed = SigSet::EMPTY;
        self.realtime.retain(|info| {
            if predicate(info) {
                removed.insert(info.signal());
                false
            } else {
                true
            }
        });
        // Preserve other entries of the same number, and overflow-only bits
        // that had no matching queued entry to remove.
        for info in &self.realtime {
            removed.remove(info.signal());
        }
        self.set.0 &= !removed.0;
    }

    #[must_use]
    fn dequeue_mask(&mut self, mask: SigSet) -> Option<PendingSignal> {
        // kernel/signal.c SYNCHRONOUS_MASK (ABRT is deliberately absent).
        const SYNCHRONOUS: SigSet =
            SigSet::of(&[Signal::SEGV, Signal::BUS, Signal::ILL, Signal::TRAP, Signal::FPE, Signal::SYS]);
        let available = self.set.0 & !mask.0;
        let synchronous = available & SYNCHRONOUS.0;
        let candidates = if synchronous != 0 { synchronous } else { available };
        if candidates == 0 {
            return None;
        }
        let signal = Signal((candidates.trailing_zeros() + 1) as u8);
        if signal.is_realtime() {
            if let Some(index) = self.realtime.iter().position(|info| info.signal() == signal) {
                let info = self.realtime.remove(index);
                if !self.realtime.iter().any(|info| info.signal() == signal) {
                    self.set.remove(signal);
                }
                return info;
            }
            self.set.remove(signal);
            // Linux collect_signal's minimal fallback: zero payload and SI_USER.
            Some(PendingSignal { info: SigInfo::new(signal, general::SI_USER as i32), timer_instance: None })
        } else {
            self.set.remove(signal);
            self.standard[signal.index()].take()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Default,
    Ignore,
    Handler,
}

impl Disposition {
    fn ignores(self, signal: Signal) -> bool {
        self == Self::Ignore
            || (self == Self::Default && (signal.default_action() == DefaultAction::Ign || signal == Signal::CONT))
    }
}

/// Process-local identity minted by new/add_thread, not a candidate position.
/// Vacancies are reused with a fresh generation; removed handles stay invalid.
/// The kernel must keep a slot with its owning process.
/// ```compile_fail
/// use lumen_signal::pending::ThreadSlot;
/// let forged = ThreadSlot { index: 0, generation: 0 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSlot {
    index: usize,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Process,
    Thread(ThreadSlot),
}

struct ThreadPending {
    generation: u64,
    pending: Pending,
}

type SlotStorage = Option<Box<ThreadPending>>;

/// Private queues are boxed; removing one releases its queue and leaves only a
/// pointer-sized vacancy. The owner serializes operations and retains dispositions
/// in its KernelSigaction table. Initial slots are returned alongside the state.
pub struct PendingSignals {
    shared: Pending,
    threads: Vec<SlotStorage>,
    next_generation: u64,
    cap: usize,
}

impl fmt::Debug for PendingSignals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignals")
            .field("slots", &self.threads.len())
            .field("realtime_cap", &self.cap)
            .finish_non_exhaustive()
    }
}

fn boxed_pending(cap: usize, generation: u64) -> Result<Box<ThreadPending>, PendingError> {
    // SAFETY: ThreadPending is nonzero-sized, and its Layout supplies the size and
    // alignment required by the global allocator. A null allocation is handled.
    let pointer = unsafe { alloc(Layout::new::<ThreadPending>()) }.cast::<ThreadPending>();
    if pointer.is_null() {
        return Err(PendingError::NoMemory);
    }
    // SAFETY: pointer uniquely owns a correctly aligned ThreadPending allocation.
    // new does not allocate or panic. Initialize before transferring ownership
    // to Box, which deallocates through the same global allocator and Layout.
    unsafe {
        pointer.write(ThreadPending { generation, pending: Pending::new(cap) });
        Ok(Box::from_raw(pointer))
    }
}

impl PendingSignals {
    pub fn new(thread_count: usize, realtime_cap: usize) -> Result<(Self, Vec<ThreadSlot>), PendingError> {
        let mut threads = Vec::new();
        let mut slots = Vec::new();
        threads.try_reserve(thread_count).map_err(|_| PendingError::NoMemory)?;
        slots.try_reserve(thread_count).map_err(|_| PendingError::NoMemory)?;
        let mut state = Self { shared: Pending::new(realtime_cap), threads, next_generation: 0, cap: realtime_cap };
        for _ in 0..thread_count {
            slots.push(state.add_thread()?);
        }
        Ok((state, slots))
    }

    /// Reuses a vacancy before extending storage. Generations never wrap: once
    /// exhausted, returns NoMemory without changing any queue or live handle.
    pub fn add_thread(&mut self) -> Result<ThreadSlot, PendingError> {
        let generation = self.next_generation.checked_add(1).ok_or(PendingError::NoMemory)?;
        let index = self.threads.iter().position(Option::is_none).unwrap_or(self.threads.len());
        if index == self.threads.len() {
            self.threads.try_reserve(1).map_err(|_| PendingError::NoMemory)?;
        }
        let pending = boxed_pending(self.cap, generation)?;
        if index == self.threads.len() {
            self.threads.push(Some(pending));
        } else {
            self.threads[index] = Some(pending);
        }
        self.next_generation = generation;
        Ok(ThreadSlot { index, generation })
    }

    /// Number of slot entries scanned by process-wide signal cancellation,
    /// including vacancies; bounded by the peak number of simultaneously live queues.
    pub fn slot_count(&self) -> usize {
        self.threads.len()
    }

    fn thread(&self, slot: ThreadSlot) -> Result<&Pending, PendingError> {
        self.threads
            .get(slot.index)
            .and_then(Option::as_deref)
            .filter(|thread| thread.generation == slot.generation)
            .map(|thread| &thread.pending)
            .ok_or(PendingError::InvalidThread)
    }
    fn thread_mut(&mut self, slot: ThreadSlot) -> Result<&mut Pending, PendingError> {
        self.threads
            .get_mut(slot.index)
            .and_then(Option::as_deref_mut)
            .filter(|thread| thread.generation == slot.generation)
            .map(|thread| &mut thread.pending)
            .ok_or(PendingError::InvalidThread)
    }

    /// Drops only this thread's private pending state. Shared state survives.
    pub fn remove_thread(&mut self, slot: ThreadSlot) -> Result<(), PendingError> {
        self.thread(slot)?;
        self.threads[slot.index] = None;
        Ok(())
    }

    /// Private | shared pending signals, including blocked ones, for rt_sigpending.
    pub fn pending(&self, slot: ThreadSlot) -> Result<SigSet, PendingError> {
        Ok(SigSet(self.thread(slot)?.set.0 | self.shared.set.0))
    }

    /// Whether any private or shared signal is outside the blocked mask.
    pub fn has_deliverable(&self, slot: ThreadSlot, blocked: Blocked) -> Result<bool, PendingError> {
        Ok(self.pending(slot)?.0 & !blocked.set().0 != 0)
    }

    /// Whether any queued siginfo in any live private or shared queue matches.
    /// Overflow-only pending bits have no siginfo and are not inspected.
    pub fn any(&self, mut predicate: impl FnMut(&SigInfo) -> bool) -> bool {
        self.any_entry(&mut |entry| predicate(&entry.info))
    }

    /// Remove matching siginfos from all queues, preserving unrelated entries,
    /// FIFO order, and pending bits for retained or overflow-only signals.
    pub fn discard_if(&mut self, mut predicate: impl FnMut(&SigInfo) -> bool) {
        self.discard_entries(&mut |entry| predicate(&entry.info));
    }

    fn any_entry(&self, predicate: &mut impl FnMut(&PendingSignal) -> bool) -> bool {
        self.shared.any(predicate) || self.threads.iter().flatten().any(|t| t.pending.any(predicate))
    }
    fn discard_entries(&mut self, predicate: &mut impl FnMut(&PendingSignal) -> bool) {
        self.shared.discard_if(predicate);
        for thread in self.threads.iter_mut().flatten() {
            thread.pending.discard_if(predicate);
        }
    }
    /// Inspect only the kernel-owned instance, never si_code or si_timerid.
    pub fn any_timer(&self, instance: u64) -> bool {
        self.any_entry(&mut |entry| entry.timer_instance == Some(instance))
    }
    /// Retire a timer's notifications without discarding user SI_TIMER images.
    pub fn discard_timer(&mut self, instance: u64) {
        self.discard_entries(&mut |entry| entry.timer_instance == Some(instance));
    }
    /// Exec retires every notification carrying kernel timer provenance.
    pub fn discard_timers(&mut self) {
        self.discard_entries(&mut |entry| entry.timer_instance.is_some());
    }

    fn discard_all(&mut self, signal: Signal) {
        self.shared.discard(signal);
        for pending in self.threads.iter_mut().flatten() {
            pending.pending.discard(signal);
        }
    }

    /// Flush after a disposition change; KILL/STOP cannot change disposition.
    /// SIG_KERNEL_IGNORE_MASK includes default CONT as well as default-Ign.
    pub fn disposition_changed(&mut self, signal: Signal, new: Disposition) {
        if !signal.is_unblockable() && new.ignores(signal) {
            self.discard_all(signal);
        }
    }

    /// Linux prepare_signal cancellation, including ignored generation. Call
    /// this before dropping an ignored stop/continue signal without enqueueing.
    pub fn prepare_signal(&mut self, signal: Signal) {
        if STOP_SIGNALS.contains(signal) {
            self.discard_all(Signal::CONT);
        } else if signal == Signal::CONT {
            for stop in STOP_LIST {
                self.discard_all(stop);
            }
        }
    }

    /// Queues even ignored signals so blocked signals remain available to sigwait.
    /// The kernel drops unblocked ignored signals before calling, after applying
    /// prepare_signal when needed. SI_USER exhaustion sets only the pending bit.
    /// With no queued entry, dequeue synthesizes minimal SI_USER siginfo. Taking
    /// the last queued entry clears the bit: an overflow adds no extra delivery.
    pub fn enqueue(&mut self, target: Target, info: SigInfo, sender: SenderClass) -> Result<Enqueued, PendingError> {
        self.enqueue_entry(target, PendingSignal { info, timer_instance: None }, sender)
    }

    /// Kernel timer send only: attaches trusted process-local arming identity.
    pub fn enqueue_timer(&mut self, target: Target, info: SigInfo, instance: u64) -> Result<Enqueued, PendingError> {
        self.enqueue_entry(target, PendingSignal { info, timer_instance: Some(instance) }, SenderClass::Other)
    }

    fn enqueue_entry(
        &mut self,
        target: Target,
        info: PendingSignal,
        sender: SenderClass,
    ) -> Result<Enqueued, PendingError> {
        if let Target::Thread(slot) = target {
            self.thread(slot)?;
        }
        self.prepare_signal(info.signal());
        match target {
            Target::Process => self.shared.enqueue(info, sender),
            Target::Thread(slot) => self.thread_mut(slot)?.enqueue(info, sender),
        }
    }

    /// Private precedes shared. Each queue prefers SYNCHRONOUS_MASK, then the
    /// lowest number, with FIFO real-time siginfo. Blocked already excludes KILL
    /// and STOP; this function does not alter it.
    #[must_use = "A dequeued signal must be consumed or delivered"]
    pub fn dequeue(&mut self, slot: ThreadSlot, blocked: Blocked) -> Result<Option<SigInfo>, PendingError> {
        self.dequeue_entry(slot, blocked).map(|entry| entry.map(PendingSignal::info))
    }
    /// Dequeue with trusted provenance for kernel timer accounting.
    pub fn dequeue_entry(&mut self, slot: ThreadSlot, blocked: Blocked) -> Result<Option<PendingSignal>, PendingError> {
        self.dequeue_mask(slot, blocked.set())
    }

    /// sigwait takes only wanted signals, always excluding KILL/STOP.
    #[must_use = "A dequeued signal must be consumed or delivered"]
    pub fn dequeue_wanted(&mut self, slot: ThreadSlot, wanted: SigSet) -> Result<Option<SigInfo>, PendingError> {
        self.dequeue_wanted_entry(slot, wanted).map(|entry| entry.map(PendingSignal::info))
    }
    /// sigwait dequeue with trusted provenance for kernel timer accounting.
    pub fn dequeue_wanted_entry(
        &mut self,
        slot: ThreadSlot,
        wanted: SigSet,
    ) -> Result<Option<PendingSignal>, PendingError> {
        self.dequeue_mask(slot, SigSet(!wanted.unblockable_stripped().0))
    }

    fn dequeue_mask(&mut self, slot: ThreadSlot, mask: SigSet) -> Result<Option<PendingSignal>, PendingError> {
        let private = self.thread_mut(slot)?.dequeue_mask(mask);
        Ok(private.or_else(|| self.shared.dequeue_mask(mask)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// Currently executing on a CPU (Linux task_curr).
    Running,
    /// Not on a CPU, regardless of the scheduler's precise sleep state.
    Sleeping,
    Stopped,
    Traced,
    Exiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadCandidate {
    pub slot: ThreadSlot,
    pub mask: Blocked,
    pub state: ThreadState,
    /// Linux TIF_SIGPENDING notification state.
    pub has_pending: bool,
}

/// A stale or absent preferred slot is a caller error, not "no eligible thread".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadCursor;

/// Simplified complete_signal/wants_signal. preferred names Linux's p/curr_target,
/// not an index in the candidate slice. The cyclic scan returns a slot identity.
/// The caller retains the cursor and implements wakeups, group-exit and job control.
#[must_use = "Handle a stale cursor separately from having no eligible thread"]
pub fn select_target(
    signal: Signal,
    threads: &[ThreadCandidate],
    preferred: ThreadSlot,
) -> Result<Option<ThreadSlot>, BadCursor> {
    let start = threads.iter().position(|thread| thread.slot == preferred).ok_or(BadCursor)?;
    Ok((start..threads.len()).chain(0..start).find_map(|index| {
        let thread = threads[index];
        if thread.state == ThreadState::Exiting || thread.mask.contains(signal) {
            return None;
        }
        (signal == Signal::KILL
            || thread.state == ThreadState::Running
            || (thread.state == ThreadState::Sleeping && !thread.has_pending))
            .then_some(thread.slot)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_slots_reuse_vacancies_and_refuse_stale_handles() {
        let (mut pending, slots) = PendingSignals::new(1, 0).unwrap();
        let mut stale = pending.add_thread().unwrap();
        pending.remove_thread(stale).unwrap();
        for _ in 0..4096 {
            let live = pending.add_thread().unwrap();
            assert_eq!(pending.slot_count(), 2, "slot storage exceeds peak live queues");
            assert_eq!(live.index, stale.index);
            assert_ne!(live, stale);
            assert_eq!(pending.pending(live), Ok(SigSet::EMPTY));
            pending.enqueue(Target::Thread(live), SigInfo::new(Signal::USR1, 0), SenderClass::SiUser).unwrap();
            pending.enqueue(Target::Process, SigInfo::new(Signal::STOP, 0), SenderClass::SiUser).unwrap();
            assert_eq!(pending.pending(stale), Err(PendingError::InvalidThread));
            assert_eq!(pending.has_deliverable(stale, Blocked::default()), Err(PendingError::InvalidThread));
            assert_eq!(pending.remove_thread(stale), Err(PendingError::InvalidThread));
            assert_eq!(pending.dequeue(stale, Blocked::default()), Err(PendingError::InvalidThread));
            assert_eq!(pending.dequeue_wanted(stale, SigSet::ALL), Err(PendingError::InvalidThread));
            // A stale CONT send must not cancel the shared STOP before refusal.
            assert_eq!(
                pending.enqueue(Target::Thread(stale), SigInfo::new(Signal::CONT, 0), SenderClass::SiUser),
                Err(PendingError::InvalidThread)
            );
            let candidates = [ThreadCandidate {
                slot: live,
                mask: Blocked::default(),
                state: ThreadState::Running,
                has_pending: true,
            }];
            assert_eq!(select_target(Signal::USR1, &candidates, stale), Err(BadCursor));
            assert_eq!(pending.pending(live), Ok(SigSet::of(&[Signal::USR1, Signal::STOP])));
            assert_eq!(pending.dequeue(slots[0], Blocked::default()), Ok(Some(SigInfo::new(Signal::STOP, 0))));
            // The next occupant must not inherit this thread's private USR1.
            pending.remove_thread(live).unwrap();
            stale = live;
        }
    }

    #[test]
    fn thread_slot_generation_exhaustion_does_not_reuse_an_identity() {
        let (mut pending, slots) = PendingSignals::new(1, 0).unwrap();
        pending.next_generation = u64::MAX - 1;
        let last = pending.add_thread().unwrap();
        assert_eq!(last.generation, u64::MAX);
        pending.remove_thread(last).unwrap();
        assert_eq!(pending.add_thread(), Err(PendingError::NoMemory));
        assert_eq!(pending.slot_count(), 2);
        assert_eq!(pending.pending(last), Err(PendingError::InvalidThread));
        assert_eq!(pending.pending(slots[0]), Ok(SigSet::EMPTY));
    }

    #[test]
    fn vacant_thread_slot_is_one_word() {
        assert_eq!(size_of::<SlotStorage>(), size_of::<usize>());
        let (mut pending, slots) = PendingSignals::new(1, 0).unwrap();
        pending.remove_thread(slots[0]).unwrap();
        assert!(pending.threads[0].is_none());
    }

    #[test]
    fn trusted_siginfo_decoder_validates_signo() {
        let mut bytes = [0x5a; 128];
        bytes[..4].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(*SigInfo::from_bytes(bytes).unwrap().bytes(), bytes);
        for number in [0u32, 65, 0x100_0001] {
            bytes[..4].copy_from_slice(&number.to_le_bytes());
            assert_eq!(SigInfo::from_bytes(bytes), None);
        }
    }
}
