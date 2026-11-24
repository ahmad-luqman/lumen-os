use core::num::NonZeroU8;

/// Which deadline the kernel must arm for a blocked read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    /// Starts at the read's first decision, even without input.
    Read,
    /// Starts with input and restarts when further bytes arrive.
    InterByte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Running,
    Expired,
}

/// One reader's timer protocol for one read call. Construct a new timer for
/// each read, keep it across wakeups, and deliver the current deadline's
/// callback through expire. Rearming must cancel the previous kernel callback.
/// The discipline never reads a clock and never shares this state with another
/// reader. Only read_decision arms the timer; repeated decisions do not extend
/// a running deadline without new input or changed VMIN/VTIME settings.
#[derive(Debug)]
pub struct ReadTimer {
    state: State,
    seen: Option<u64>,
    settings: Option<(usize, u8)>,
}

impl ReadTimer {
    /// An unarmed timer for a new read call.
    pub const fn new() -> Self {
        Self { state: State::Idle, seen: None, settings: None }
    }

    /// Reports expiration of the currently armed kernel timer. A callback
    /// while unarmed has no effect; the kernel must suppress stale callbacks.
    pub fn expire(&mut self) {
        if self.state == State::Running {
            self.state = State::Expired;
        }
    }

    pub(crate) fn canonical(&mut self, stored: u64) {
        self.state = State::Idle;
        self.settings = None;
        self.seen = Some(stored);
    }

    pub(crate) fn decide(&mut self, min: usize, time: u8, available: usize, stored: u64) -> Decision {
        let changed = self.seen.is_some_and(|seen| seen != stored);
        self.seen = Some(stored);
        if self.settings != Some((min, time)) {
            self.state = State::Idle;
            self.settings = Some((min, time));
        }
        if min > 0 && (changed || available == 0) {
            self.state = State::Idle;
        }
        if available >= min && (min > 0 || time == 0) {
            return Decision::Read;
        }
        let Some(deciseconds) = NonZeroU8::new(time) else {
            return Decision::Wait;
        };
        if min == 0 && available != 0 {
            return Decision::Read;
        }
        if min > 0 && available == 0 {
            return Decision::Wait;
        }
        match self.state {
            State::Expired => Decision::Read,
            State::Running => Decision::Wait,
            State::Idle => {
                self.state = State::Running;
                Decision::WaitWithTimer {
                    deciseconds,
                    kind: if min == 0 { TimerKind::Read } else { TimerKind::InterByte },
                }
            }
        }
    }
}

impl Default for ReadTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// A read protocol action. Read always requires calling LineDiscipline::read:
/// it does not consume input and does not itself report bytes or EOF.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Call read with the same destination length used for this decision.
    Read,
    /// Sleep until input, a mode change, a signal or the armed timer wakes you.
    Wait,
    /// (Re)arm the timer for this duration now, replacing the previous deadline.
    /// The ReadTimer already records Running, so the next decision won't rearm
    /// unless input or settings have changed.
    WaitWithTimer {
        /// Positive duration in tenths of a second.
        deciseconds: NonZeroU8,
        /// Whether this is an overall read or inter-byte deadline.
        kind: TimerKind,
    },
}
