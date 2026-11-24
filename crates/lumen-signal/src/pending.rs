//! Pending queues and a deterministic subset of Linux kernel/signal.c policy.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use linux_raw_sys::{errno, general};

use crate::{DefaultAction, SigSet, Signal};

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingError {
    Full,
    NoMemory,
    InvalidThread,
    Unchangeable,
}

impl PendingError {
    /// Negative Linux errno; a bounded real-time queue reports -EAGAIN.
    pub const fn errno(self) -> i32 {
        -(match self {
            Self::Full => errno::EAGAIN,
            Self::NoMemory => errno::ENOMEM,
            Self::InvalidThread | Self::Unchangeable => errno::EINVAL,
        } as i32)
    }
}

/// One pending set. Standard signals retain the first siginfo until dequeued;
/// real-time signals retain FIFO order within each signal number. The cap is
/// per queue, not Linux's per-user RLIMIT_SIGPENDING accounting.
#[derive(Debug)]
pub struct Pending {
    set: SigSet,
    standard: [Option<SigInfo>; 31],
    realtime: VecDeque<SigInfo>,
    cap: usize,
}

impl Pending {
    pub fn new(realtime_cap: usize) -> Self {
        Self { set: SigSet::EMPTY, standard: [None; 31], realtime: VecDeque::new(), cap: realtime_cap }
    }

    pub const fn signals(&self) -> SigSet {
        self.set
    }

    pub fn enqueue(&mut self, info: SigInfo) -> Result<Enqueued, PendingError> {
        let signal = info.signal();
        if signal.is_realtime() {
            if self.realtime.len() == self.cap {
                return Err(PendingError::Full);
            }
            self.realtime.try_reserve(1).map_err(|_| PendingError::NoMemory)?;
            self.realtime.push_back(info);
        } else {
            let slot = &mut self.standard[signal.0 as usize - 1];
            if slot.is_some() {
                return Ok(Enqueued::Coalesced);
            }
            *slot = Some(info);
        }
        self.set.insert(signal);
        Ok(Enqueued::Queued)
    }

    pub fn discard(&mut self, signal: Signal) {
        if signal.is_realtime() {
            self.realtime.retain(|info| info.signal() != signal);
        } else {
            self.standard[signal.0 as usize - 1] = None;
        }
        self.set.remove(signal);
    }

    /// Linux next_signal order: unmasked SYNCHRONOUS_MASK signals first, then
    /// the lowest number; masks cannot suppress SIGKILL or SIGSTOP.
    pub fn dequeue(&mut self, mask: SigSet) -> Option<SigInfo> {
        // kernel/signal.c SYNCHRONOUS_MASK (SIGABRT is deliberately absent).
        const SYNCHRONOUS: u64 = (1 << (general::SIGSEGV - 1))
            | (1 << (general::SIGBUS - 1))
            | (1 << (general::SIGILL - 1))
            | (1 << (general::SIGTRAP - 1))
            | (1 << (general::SIGFPE - 1))
            | (1 << (general::SIGSYS - 1));
        let available = self.set.0 & !mask.unblockable_stripped().0;
        let synchronous = available & SYNCHRONOUS;
        let candidates = if synchronous != 0 { synchronous } else { available };
        if candidates == 0 {
            return None;
        }
        let signal = Signal((candidates.trailing_zeros() + 1) as u8);
        if signal.is_realtime() {
            let index = self.realtime.iter().position(|info| info.signal() == signal)?;
            let info = self.realtime.remove(index);
            if !self.realtime.iter().any(|info| info.signal() == signal) {
                self.set.remove(signal);
            }
            info
        } else {
            self.set.remove(signal);
            self.standard[signal.0 as usize - 1].take()
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
        self == Self::Ignore || (self == Self::Default && signal.default_action() == DefaultAction::Ign)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Process,
    Thread(usize),
}

/// One shared queue and one private queue per thread. The owner supplies stable
/// thread indices and serialization. Stop/continue generation and disposition
/// changes atomically discard matching entries across *all* queues.
#[derive(Debug)]
pub struct PendingSignals {
    shared: Pending,
    threads: Vec<Pending>,
    dispositions: [Disposition; 64],
}

impl PendingSignals {
    pub fn new(thread_count: usize, realtime_cap: usize) -> Self {
        Self {
            shared: Pending::new(realtime_cap),
            threads: (0..thread_count).map(|_| Pending::new(realtime_cap)).collect(),
            dispositions: [Disposition::Default; 64],
        }
    }

    fn discard_all(&mut self, signal: Signal) {
        self.shared.discard(signal);
        for pending in &mut self.threads {
            pending.discard(signal);
        }
    }

    /// SIGKILL and SIGSTOP dispositions cannot change. Ignoring a signal,
    /// including a default-Ign disposition, flushes every pending instance.
    pub fn set_disposition(&mut self, signal: Signal, disposition: Disposition) -> Result<(), PendingError> {
        if signal.is_unblockable() {
            return Err(PendingError::Unchangeable);
        }
        self.dispositions[signal.0 as usize - 1] = disposition;
        if disposition.ignores(signal) {
            self.discard_all(signal);
        }
        Ok(())
    }

    pub fn disposition(&self, signal: Signal) -> Disposition {
        self.dispositions[signal.0 as usize - 1]
    }

    /// Queue a generated signal, including currently ignored signals: blocked
    /// ignored signals can be consumed by sigwait. This omits Linux's early
    /// ignore optimization, which needs masks and ptrace state. The owner must
    /// apply the disposition when dequeuing for delivery; sigwait can consume
    /// an ignored signal. Cancellation is unconditional, as in prepare_signal.
    pub fn enqueue(&mut self, target: Target, info: SigInfo) -> Result<Enqueued, PendingError> {
        if matches!(target, Target::Thread(index) if index >= self.threads.len()) {
            return Err(PendingError::InvalidThread);
        }
        let signal = info.signal();
        // Linux prepare_signal applies cancellation even when the new signal
        // is blocked, caught, or ignored; it depends on the number, not handler.
        if signal.default_action() == DefaultAction::Stop {
            self.discard_all(Signal(general::SIGCONT as u8));
        } else if signal.number() == general::SIGCONT {
            for number in [general::SIGSTOP, general::SIGTSTP, general::SIGTTIN, general::SIGTTOU] {
                self.discard_all(Signal(number as u8));
            }
        }
        match target {
            Target::Process => self.shared.enqueue(info),
            Target::Thread(index) => self.threads[index].enqueue(info),
        }
    }

    /// Like Linux dequeue_signal, checks private pending before shared pending;
    /// synchronous priority applies separately within each queue.
    pub fn dequeue(&mut self, thread: usize, mask: SigSet) -> Result<Option<SigInfo>, PendingError> {
        let private = self.threads.get_mut(thread).ok_or(PendingError::InvalidThread)?;
        Ok(private.dequeue(mask).or_else(|| self.shared.dequeue(mask)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Running,
    Sleeping,
    Stopped,
    Traced,
    Exiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadCandidate {
    pub mask: SigSet,
    pub state: ThreadState,
    pub has_pending: bool,
}

/// Simplified complete_signal/wants_signal: prefer the named thread, then scan
/// cyclically. Running threads or sleepers without another pending notification
/// accept an unblocked signal; KILL also accepts stopped/traced threads. Exiting
/// threads never accept. The caller retains the rotation cursor and implements
/// wakeups, SIGCONT resumption, group-exit and ptrace/job-control side effects.
pub fn select_target(signal: Signal, threads: &[ThreadCandidate], preferred: usize) -> Option<usize> {
    if threads.is_empty() {
        return None;
    }
    let start = preferred % threads.len();
    (start..threads.len()).chain(0..start).find(|&index| {
        let thread = threads[index];
        if thread.state == ThreadState::Exiting || thread.mask.unblockable_stripped().contains(signal) {
            return false;
        }
        signal.number() == general::SIGKILL
            || thread.state == ThreadState::Running
            || (thread.state == ThreadState::Sleeping && !thread.has_pending)
    })
}
