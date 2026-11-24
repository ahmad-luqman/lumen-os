//! Pending queues and a deterministic subset of Linux kernel/signal.c policy.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use linux_raw_sys::{errno, general};

use crate::{Blocked, DefaultAction, STOP_SIGNALS, SigSet, Signal};

/// An initialized 128-byte Linux siginfo image with a valid si_signo. Payload
/// bytes are opaque; callers are responsible for syscall-specific si_code checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigInfo([u8; 128]);

impl SigInfo {
    pub fn new(signal: Signal, code: i32) -> Self {
        let mut bytes = [0; 128];
        bytes[..4].copy_from_slice(&signal.number().to_le_bytes());
        bytes[8..12].copy_from_slice(&code.to_le_bytes());
        Self(bytes)
    }

    /// Linux __copy_siginfo_from_user: the syscall's signal overrides si_signo.
    pub fn from_user(signal: Signal, mut bytes: [u8; 128]) -> Self {
        bytes[..4].copy_from_slice(&signal.number().to_le_bytes());
        Self(bytes)
    }

    /// Decode an already trusted image; syscall inputs must use from_user.
    pub fn from_bytes(bytes: [u8; 128]) -> Option<Self> {
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

    /// A fault siginfo with si_addr (offset 16 on x86-64) set to the fault address.
    pub fn fault(signal: Signal, code: i32, address: u64) -> Self {
        let mut info = Self::new(signal, code);
        info.0[16..24].copy_from_slice(&address.to_le_bytes());
        info
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
    /// A real-time SI_USER signal survives, but its siginfo could not be queued.
    InfoLost,
}

/// The kernel classifies the validated sender; this is not inferred from a
/// user-controlled si_code. Only SI_USER has Linux's loss-of-info fallback.
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
    /// no user errno; both queue exhaustion and allocation failure are -EAGAIN.
    pub const fn errno(self) -> Option<i32> {
        match self {
            Self::Full | Self::NoMemory => Some(-(errno::EAGAIN as i32)),
            Self::InvalidThread => None,
        }
    }
}

/// One pending set, mutated only through PendingSignals so generation always
/// performs process-wide stop/continue cancellation. Capacity is per queue,
/// not Linux's per-user RLIMIT_SIGPENDING accounting.
#[derive(Debug)]
pub(crate) struct Pending {
    set: SigSet,
    standard: [Option<SigInfo>; 31],
    realtime: VecDeque<SigInfo>,
    info_less: SigSet,
    cap: usize,
}

impl Pending {
    pub(crate) fn new(cap: usize) -> Self {
        Self { set: SigSet::EMPTY, standard: [None; 31], realtime: VecDeque::new(), info_less: SigSet::EMPTY, cap }
    }

    pub(crate) fn enqueue(&mut self, info: SigInfo, sender: SenderClass) -> Result<Enqueued, PendingError> {
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
                self.info_less.insert(signal);
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

    pub(crate) fn discard(&mut self, signal: Signal) {
        if signal.is_realtime() {
            self.realtime.retain(|info| info.signal() != signal);
            self.info_less.remove(signal);
        } else {
            self.standard[signal.index()] = None;
        }
        self.set.remove(signal);
    }

    #[must_use]
    pub(crate) fn dequeue_mask(&mut self, mask: SigSet) -> Option<SigInfo> {
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
                if !self.info_less.contains(signal) && !self.realtime.iter().any(|info| info.signal() == signal) {
                    self.set.remove(signal);
                }
                return info;
            }
            self.info_less.remove(signal);
            self.set.remove(signal);
            // Linux collect_signal's minimal fallback: zero payload and SI_USER.
            Some(SigInfo::new(signal, general::SI_USER as i32))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Process,
    Thread(usize),
}

/// Shared and private pending state. Thread indices are stable and never reused;
/// removing a thread leaves a vacant slot. The owner serializes operations and
/// retains dispositions in its KernelSigaction table, not in this object.
#[derive(Debug)]
pub struct PendingSignals {
    shared: Pending,
    threads: Vec<Option<Pending>>,
    cap: usize,
}

impl PendingSignals {
    pub fn new(thread_count: usize, realtime_cap: usize) -> Result<Self, PendingError> {
        let mut threads = Vec::new();
        threads.try_reserve(thread_count).map_err(|_| PendingError::NoMemory)?;
        for _ in 0..thread_count {
            threads.push(Some(Pending::new(realtime_cap)));
        }
        Ok(Self { shared: Pending::new(realtime_cap), threads, cap: realtime_cap })
    }

    pub fn add_thread(&mut self) -> Result<usize, PendingError> {
        self.threads.try_reserve(1).map_err(|_| PendingError::NoMemory)?;
        let index = self.threads.len();
        self.threads.push(Some(Pending::new(self.cap)));
        Ok(index)
    }

    /// Drops only this thread's private pending state. Shared state survives.
    pub fn remove_thread(&mut self, thread: usize) -> Result<(), PendingError> {
        self.threads.get_mut(thread).and_then(Option::take).ok_or(PendingError::InvalidThread)?;
        Ok(())
    }

    pub fn pending(&self, thread: usize) -> Result<SigSet, PendingError> {
        let private = self.threads.get(thread).and_then(Option::as_ref).ok_or(PendingError::InvalidThread)?;
        Ok(SigSet(private.set.0 | self.shared.set.0))
    }

    pub fn has_deliverable(&self, thread: usize, blocked: Blocked) -> Result<bool, PendingError> {
        Ok(self.pending(thread)?.0 & !blocked.set().0 != 0)
    }

    fn discard_all(&mut self, signal: Signal) {
        self.shared.discard(signal);
        for pending in self.threads.iter_mut().flatten() {
            pending.discard(signal);
        }
    }

    /// Apply a disposition change already validated and stored by the kernel.
    /// SIG_KERNEL_IGNORE_MASK includes default SIGCONT as well as default-Ign.
    pub fn disposition_changed(&mut self, signal: Signal, new: Disposition) {
        if new.ignores(signal) {
            self.discard_all(signal);
        }
    }

    /// Linux prepare_signal cancellation, including ignored generation. Call
    /// this before dropping an ignored stop/continue signal without enqueueing.
    pub fn prepare_signal(&mut self, signal: Signal) {
        if STOP_SIGNALS.contains(signal) {
            self.discard_all(Signal::CONT);
        } else if signal == Signal::CONT {
            for number in 1..=64 {
                let signal = Signal(number);
                if STOP_SIGNALS.contains(signal) {
                    self.discard_all(signal);
                }
            }
        }
    }

    /// Queues even ignored signals (blocked ignored signals may serve sigwait).
    /// Linux drops unblocked ignored signals at generation: the kernel must do
    /// that before calling, after prepare_signal's cancellation when applicable.
    /// SI_USER queue exhaustion retains a coalescing info-less real-time marker;
    /// dequeue synthesizes zero payload with si_signo and SI_USER for that marker.
    pub fn enqueue(&mut self, target: Target, info: SigInfo, sender: SenderClass) -> Result<Enqueued, PendingError> {
        if let Target::Thread(index) = target {
            self.pending(index)?;
        }
        self.prepare_signal(info.signal());
        match target {
            Target::Process => self.shared.enqueue(info, sender),
            Target::Thread(index) => {
                self.threads[index].as_mut().ok_or(PendingError::InvalidThread)?.enqueue(info, sender)
            }
        }
    }

    /// Delivery dequeue: private pending precedes shared pending. Within each
    /// queue Linux next_signal prefers SYNCHRONOUS_MASK, then the lowest number,
    /// with FIFO siginfo for a real-time number. Blocked already excludes KILL
    /// and STOP; this function does not alter the supplied mask.
    #[must_use = "A dequeued signal must be consumed or delivered"]
    pub fn dequeue(&mut self, thread: usize, blocked: Blocked) -> Result<Option<SigInfo>, PendingError> {
        self.dequeue_mask(thread, blocked.set())
    }

    /// sigwait dequeue: only wanted signals, always excluding KILL and STOP,
    /// regardless of whether those uncatchable signals were explicitly wanted.
    #[must_use = "A dequeued signal must be consumed or delivered"]
    pub fn dequeue_wanted(&mut self, thread: usize, wanted: SigSet) -> Result<Option<SigInfo>, PendingError> {
        self.dequeue_mask(thread, SigSet(!wanted.unblockable_stripped().0))
    }

    fn dequeue_mask(&mut self, thread: usize, mask: SigSet) -> Result<Option<SigInfo>, PendingError> {
        let private = self.threads.get_mut(thread).and_then(Option::as_mut).ok_or(PendingError::InvalidThread)?;
        Ok(private.dequeue_mask(mask).or_else(|| self.shared.dequeue_mask(mask)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// Currently executing on a CPU (Linux task_curr).
    Running,
    Sleeping,
    Stopped,
    Traced,
    Exiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadCandidate {
    pub mask: Blocked,
    pub state: ThreadState,
    pub has_pending: bool,
}

/// Simplified complete_signal/wants_signal: preferred represents both Linux's
/// p and curr_target, then a cyclic scan chooses an eligible thread. An invalid
/// preferred index is refused. Running means task_curr (on a CPU); otherwise a
/// sleeper accepts only without pending notification. KILL also accepts stopped
/// or traced threads; exiting threads never accept. The caller retains the
/// cursor and implements wakeups, SIGCONT resumption, group-exit and job control.
#[must_use]
pub fn select_target(signal: Signal, threads: &[ThreadCandidate], preferred: usize) -> Option<usize> {
    if preferred >= threads.len() {
        return None;
    }
    (preferred..threads.len()).chain(0..preferred).find(|&index| {
        let thread = threads[index];
        if thread.state == ThreadState::Exiting || thread.mask.contains(signal) {
            return false;
        }
        signal == Signal::KILL
            || thread.state == ThreadState::Running
            || (thread.state == ThreadState::Sleeping && !thread.has_pending)
    })
}
