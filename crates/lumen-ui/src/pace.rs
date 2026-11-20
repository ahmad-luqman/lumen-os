//! When an animated window ticks ([`Pacer`]): its app says, before every
//! wait, how far apart its ticks are now (or that it doesn't tick), and the
//! pacer keeps the next deadline. The SDK's `Toolkit::run_until` drives it;
//! it is pure, so host tests pin the rules.

/// The next tick's deadline, on one clock in nanoseconds (the SDK's
/// `Time::now_ns`):
///
/// - a period appearing (or changing) starts a fresh schedule, one period
///   from now;
/// - no period clears it: no deadline, so the window waits for events only
///   (and the desktop can settle);
/// - after a tick the next is one period on; a tick that came late by a
///   period or more re-bases the schedule one period after it, so a stalled
///   window ticks once, never a burst to catch up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pacer {
    /// The period the schedule runs on and its next deadline (both, or
    /// neither).
    schedule: Option<(u64, u64)>,
}

impl Pacer {
    pub const fn new() -> Pacer {
        Pacer { schedule: None }
    }

    /// The deadline at `now` for ticks `period` nanoseconds apart (`None`:
    /// no ticks), asked before every wait.
    pub fn deadline(&mut self, period: Option<u64>, now: u64) -> Option<u64> {
        self.schedule = match (period, self.schedule) {
            (None, _) => None,
            (Some(p), Some((q, next))) if p == q => Some((q, next)),
            (Some(p), _) => Some((p, now.saturating_add(p))),
        };
        self.schedule.map(|(_, next)| next)
    }

    /// Whether a tick is due at `now`.
    pub fn due(&self, now: u64) -> bool {
        self.schedule.is_some_and(|(_, next)| now >= next)
    }

    /// A tick was taken at `now`: the next deadline is one period on, or
    /// one period after `now` if that is already past.
    pub fn ticked(&mut self, now: u64) {
        if let Some((p, next)) = self.schedule {
            let on = next.saturating_add(p);
            self.schedule = Some((p, if on <= now { now.saturating_add(p) } else { on }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pacer;

    #[test]
    fn a_period_starts_a_schedule_one_period_from_now() {
        let mut p = Pacer::new();
        assert_eq!(p.deadline(Some(100), 1_000), Some(1_100));
        assert!(!p.due(1_099));
        assert!(p.due(1_100));
        // Asked again with the same period: the same deadline, not re-based.
        assert_eq!(p.deadline(Some(100), 1_050), Some(1_100));
    }

    #[test]
    fn no_period_clears_it_and_a_new_one_starts_afresh() {
        let mut p = Pacer::new();
        assert_eq!(p.deadline(None, 5), None);
        assert!(!p.due(u64::MAX));
        p.deadline(Some(100), 0);
        assert_eq!(p.deadline(None, 50), None);
        assert!(!p.due(1_000));
        // Ticking with nothing scheduled does nothing.
        p.ticked(1_000);
        assert_eq!(p.deadline(None, 1_000), None);
        assert_eq!(p.deadline(Some(100), 2_000), Some(2_100), "fresh, from now");
    }

    #[test]
    fn a_changed_period_re_bases() {
        let mut p = Pacer::new();
        p.deadline(Some(100), 0);
        assert_eq!(p.deadline(Some(30), 40), Some(70));
        assert_eq!(p.deadline(Some(30), 60), Some(70));
    }

    #[test]
    fn on_time_ticks_stay_on_their_grid() {
        let mut p = Pacer::new();
        p.deadline(Some(100), 0);
        // Taken a little late each time: the deadlines stay 100 apart.
        for (k, now) in [(1u64, 103u64), (2, 201), (3, 350)] {
            p.ticked(now);
            assert_eq!(p.deadline(Some(100), now), Some(100 * (k + 1)), "after the tick at {now}");
        }
    }

    #[test]
    fn a_late_tick_is_one_tick_never_a_burst() {
        let mut p = Pacer::new();
        p.deadline(Some(100), 0);
        // Ten periods late: one tick, then the next a whole period later.
        assert!(p.due(1_000));
        p.ticked(1_000);
        assert!(!p.due(1_000));
        assert_eq!(p.deadline(Some(100), 1_000), Some(1_100));
        // Exactly one period late (next + period == now): re-based too.
        let mut q = Pacer::new();
        q.deadline(Some(100), 0);
        q.ticked(200);
        assert!(!q.due(200));
        assert_eq!(q.deadline(Some(100), 200), Some(300));
    }

    #[test]
    fn the_clock_s_end_saturates() {
        let mut p = Pacer::new();
        assert_eq!(p.deadline(Some(10), u64::MAX - 3), Some(u64::MAX));
        p.ticked(u64::MAX);
        assert_eq!(p.deadline(Some(10), u64::MAX), Some(u64::MAX));
    }
}
