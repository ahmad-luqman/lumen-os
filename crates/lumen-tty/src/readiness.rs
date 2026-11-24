/// The caller supplies the clock state for the current read, never wall time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerState {
    #[default]
    Idle,
    Running,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    /// Starts when a read begins, even with no input.
    Read,
    /// Starts with the first byte and restarts on each subsequent byte.
    InterByte,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    ReturnNow(usize),
    Wait,
    WaitWithTimer { deciseconds: u8, kind: TimerKind },
}

/// The four noncanonical VMIN/VTIME cases. `available` is the queued byte
/// count. For a short destination the caller clamps VMIN to its size, and
/// clamps ReturnNow's count too. Running timers are left running; after a
/// new byte, pass Idle to restart an inter-byte timer. Expiry only completes
/// an inter-byte read after its first byte. Nonblocking I/O is caller policy.
pub fn ready(vmin: u8, vtime: u8, available: usize, timer: TimerState) -> Readiness {
    use Readiness::*;
    match (vmin, vtime) {
        (0, 0) => ReturnNow(available),
        (0, _) if available != 0 || timer == TimerState::Expired => ReturnNow(available),
        (0, _) => match timer {
            TimerState::Idle => WaitWithTimer { deciseconds: vtime, kind: TimerKind::Read },
            _ => Wait,
        },
        (_, _) if available >= usize::from(vmin) => ReturnNow(available),
        (_, 0) => Wait,
        (_, _) if available == 0 => Wait,
        (_, _) => match timer {
            TimerState::Expired => ReturnNow(available),
            TimerState::Idle => WaitWithTimer { deciseconds: vtime, kind: TimerKind::InterByte },
            TimerState::Running => Wait,
        },
    }
}

/// A read's next action. Wait means no completed canonical record. A zero
/// destination returns Raw(ReturnNow(0)) in either mode and consumes nothing.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Line,
    Eof,
    Wait,
    Raw(Readiness),
}
