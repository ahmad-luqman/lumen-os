//! When an animated window ticks ([`Pacer`]): its app says, before every
//! wait, how far apart its ticks are now (or that it doesn't tick), and the
//! pacer keeps the next deadline. The SDK's `Toolkit::run_until` drives it;
//! it is pure, so host tests pin the rules.

/// The shortest period a pacer keeps: a shorter one (0 included) is taken
/// as this, so a window ticks at most 100 times a second and always waits
/// between ticks.
pub const MIN_PERIOD_NS: u64 = 10_000_000;

/// What [`Pacer::poll`] says to do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Tick now, then call [`Pacer::ticked`] with the time read after the
    /// tick's work.
    Due,
    /// Wait for events until this deadline.
    Until(u64),
    /// No ticks: wait for events only.
    Idle,
}

/// The next tick's deadline, on one clock in nanoseconds (the SDK's
/// `Time::now_ns`), from the period the app asks for before every wait:
///
/// - a period below [`MIN_PERIOD_NS`] is taken as that;
/// - a period appearing starts a schedule one period from now; no period
///   clears it (the window waits for events only, and the desktop can
///   settle);
/// - whether a tick is due is decided on the schedule as it stands, before
///   a changed period moves it;
/// - a changed period not yet due moves the deadline to the earlier of the
///   old one and one new period from now: a shorter period takes effect at
///   once, a longer one after the next tick, and a period that changes on
///   every ask (a clock's "until the next second") still ticks at its
///   boundary;
/// - a tick is due once: the next deadline is one period after it was
///   found due, or, once [`Pacer::ticked`] says its work (the tick and the
///   refresh) ran past that, one period after the work ended. So ticks are
///   always at least a period apart, and a late or overrunning window still
///   waits a whole period, never bursting to catch up. (Counted from when
///   the tick was found due, not from its deadline: a period asked at that
///   moment, such as a clock's "until the next second", is measured from
///   then, and counting it from an earlier deadline would tick early. A
///   steady period drifts late by each tick's lateness instead.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pacer {
    schedule: Option<Schedule>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Schedule {
    period: u64,
    next: u64,
    /// A tick was found due and not yet [`Pacer::ticked`].
    taking: bool,
}

impl Pacer {
    pub const fn new() -> Pacer {
        Pacer { schedule: None }
    }

    /// What to do at `now` for ticks `period` nanoseconds apart (`None`: no
    /// ticks), asked before every wait.
    pub fn poll(&mut self, period: Option<u64>, now: u64) -> Pace {
        let Some(p) = period.map(|p| p.max(MIN_PERIOD_NS)) else {
            self.schedule = None;
            return Pace::Idle;
        };
        let s = match self.schedule {
            None => Schedule { period: p, next: now.saturating_add(p), taking: false },
            Some(s) if now >= s.next => {
                self.schedule = Some(Schedule { period: p, next: now.saturating_add(p), taking: true });
                return Pace::Due;
            }
            Some(s) if s.period != p => Schedule { period: p, next: s.next.min(now.saturating_add(p)), ..s },
            Some(s) => s,
        };
        self.schedule = Some(s);
        Pace::Until(s.next)
    }

    /// The tick [`Pacer::poll`] found due is done, its work (the app's tick
    /// and the window's refresh) ending at `after`.
    pub fn ticked(&mut self, after: u64) {
        let Some(s) = &mut self.schedule else { return };
        if !core::mem::take(&mut s.taking) {
            return;
        }
        if s.next <= after {
            s.next = after.saturating_add(s.period);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_PERIOD_NS, Pace, Pacer};

    const P: u64 = 100 * MIN_PERIOD_NS;

    #[test]
    fn a_period_starts_a_schedule_one_period_from_now() {
        let mut p = Pacer::new();
        assert_eq!(p.poll(Some(P), 1_000), Pace::Until(1_000 + P));
        // Asked again with the same period: the same deadline, not re-based.
        assert_eq!(p.poll(Some(P), 1_050), Pace::Until(1_000 + P));
        assert_eq!(p.poll(Some(P), 1_000 + P - 1), Pace::Until(1_000 + P));
        assert_eq!(p.poll(Some(P), 1_000 + P), Pace::Due);
    }

    #[test]
    fn a_period_below_the_floor_is_the_floor() {
        for short in [0, 1, MIN_PERIOD_NS - 1] {
            let mut p = Pacer::new();
            assert_eq!(p.poll(Some(short), 5), Pace::Until(5 + MIN_PERIOD_NS), "{short}");
        }
        // A tick with no work time: the next is still a whole floor later.
        let mut p = Pacer::new();
        p.poll(Some(0), 0);
        assert_eq!(p.poll(Some(0), MIN_PERIOD_NS), Pace::Due);
        p.ticked(MIN_PERIOD_NS);
        assert_eq!(p.poll(Some(0), MIN_PERIOD_NS), Pace::Until(2 * MIN_PERIOD_NS));
    }

    #[test]
    fn no_period_clears_it_and_a_new_one_starts_afresh() {
        let mut p = Pacer::new();
        assert_eq!(p.poll(None, 5), Pace::Idle);
        assert_eq!(p.poll(None, u64::MAX), Pace::Idle);
        p.poll(Some(P), 0);
        assert_eq!(p.poll(None, 50), Pace::Idle);
        assert_eq!(p.poll(None, 10 * P), Pace::Idle, "not due: cleared");
        // Ticked with nothing scheduled: nothing.
        p.ticked(10 * P);
        assert_eq!(p.poll(Some(P), 20 * P), Pace::Until(21 * P), "fresh, from now");
    }

    #[test]
    fn due_is_decided_before_a_changed_period_moves_it() {
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        // At the deadline with another period: due, not re-based.
        assert_eq!(p.poll(Some(3 * P), P), Pace::Due);
    }

    #[test]
    fn a_changed_period_takes_the_earlier_deadline() {
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        // Shorter: at once.
        assert_eq!(p.poll(Some(P / 4), P / 2), Pace::Until(P / 2 + P / 4));
        // Longer: the old deadline stands; the new period after the tick.
        assert_eq!(p.poll(Some(10 * P), P / 2 + 1), Pace::Until(P / 2 + P / 4));
        assert_eq!(p.poll(Some(10 * P), P), Pace::Due);
        p.ticked(P);
        assert_eq!(p.poll(Some(10 * P), P), Pace::Until(11 * P));
    }

    #[test]
    fn a_clock_whose_period_changes_on_every_ask_ticks_at_each_boundary() {
        // A clock asking for "the time until the next second" (plus jitter
        // of up to 7 ms): asked every 3 to 40 ms for 10 s, it ticks once per
        // second, each tick at or past its boundary, none missed.
        const S: u64 = 1_000_000_000;
        let mut p = Pacer::new();
        let mut now = 17u64;
        let mut ticks = alloc::vec::Vec::new();
        let mut k = 0u64;
        while now < 10 * S {
            let jitter = (k * 7_919) % 7_000_000;
            let period = S - now % S + jitter;
            match p.poll(Some(period), now) {
                Pace::Due => {
                    ticks.push(now);
                    p.ticked(now + 1_000_000);
                }
                Pace::Until(d) => assert!(d > now),
                Pace::Idle => unreachable!(),
            }
            k += 1;
            now += 3_000_000 + (k * 104_729) % 37_000_000;
        }
        assert_eq!(ticks.len(), 9, "{ticks:?}");
        for (i, t) in ticks.iter().enumerate() {
            let boundary = (i as u64 + 1) * S;
            assert!(*t >= boundary && *t < boundary + S / 10, "tick {i} at {t}");
        }
    }

    #[test]
    fn quick_ticks_come_a_period_after_each_was_found_due() {
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        // Found due a little late each time, the work quick: the next a
        // period after (late by that much, never early).
        let mut at = P;
        for k in 1..=3u64 {
            let now = at + k * 7;
            assert_eq!(p.poll(Some(P), now), Pace::Due, "tick {k}");
            p.ticked(now + 1);
            assert_eq!(p.poll(Some(P), now + 1), Pace::Until(now + P), "after tick {k}");
            at = now + P;
        }
    }

    #[test]
    fn a_tick_is_due_once_until_ticked() {
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        assert_eq!(p.poll(Some(P), P), Pace::Due);
        // Not ticked yet: not due again, waiting a period from then.
        assert_eq!(p.poll(Some(P), P), Pace::Until(2 * P));
        assert_eq!(p.poll(Some(P), P + 1), Pace::Until(2 * P));
        // Only the tick found due is ticked: a second call, however late,
        // moves nothing.
        p.ticked(P + 1);
        p.ticked(5 * P);
        assert_eq!(p.poll(Some(P), 5 * P), Pace::Due);
    }

    #[test]
    fn a_stray_ticked_moves_nothing() {
        // Ticked with no tick found due: the schedule stands.
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        p.ticked(10 * P);
        assert_eq!(p.poll(Some(P), P), Pace::Due);
    }

    #[test]
    fn a_late_or_overrunning_tick_waits_a_whole_period() {
        // Ten periods late: one tick, then a whole period after it.
        let mut p = Pacer::new();
        p.poll(Some(P), 0);
        assert_eq!(p.poll(Some(P), 10 * P), Pace::Due);
        p.ticked(10 * P);
        assert_eq!(p.poll(Some(P), 10 * P), Pace::Until(11 * P));
        assert_eq!(p.poll(Some(P), 11 * P - 1), Pace::Until(11 * P), "no burst");
        // On time, but its work ran past the next deadline: a period after
        // the work, not at once.
        let mut q = Pacer::new();
        q.poll(Some(P), 0);
        assert_eq!(q.poll(Some(P), P), Pace::Due);
        q.ticked(2 * P + 5);
        assert_eq!(q.poll(Some(P), 2 * P + 5), Pace::Until(3 * P + 5));
        // Exactly at the next deadline (deadline + period == after): too.
        let mut r = Pacer::new();
        r.poll(Some(P), 0);
        r.poll(Some(P), P);
        r.ticked(2 * P);
        assert_eq!(r.poll(Some(P), 2 * P), Pace::Until(3 * P));
    }

    #[test]
    fn an_app_whose_ticks_overrun_still_waits_between_them() {
        // Each tick's work takes 1.5 periods: ticks come at least a period
        // after each one's work ended.
        let mut p = Pacer::new();
        let mut now = 0;
        let mut ended: Option<u64> = None;
        for _ in 0..50 {
            match p.poll(Some(P), now) {
                Pace::Due => {
                    if let Some(e) = ended {
                        assert!(now >= e + P, "{now} after work ending {e}");
                    }
                    now += P + P / 2;
                    p.ticked(now);
                    ended = Some(now);
                }
                Pace::Until(d) => now = d,
                Pace::Idle => unreachable!(),
            }
        }
    }

    #[test]
    fn the_clock_s_end_saturates() {
        // Nothing past the clock's end: no overflow, and (it can't wait past
        // it) every poll there is due.
        let mut p = Pacer::new();
        assert_eq!(p.poll(Some(P), u64::MAX - 3), Pace::Until(u64::MAX));
        assert_eq!(p.poll(Some(P), u64::MAX), Pace::Due);
        p.ticked(u64::MAX);
        assert_eq!(p.poll(Some(P), u64::MAX), Pace::Due);
    }
}
