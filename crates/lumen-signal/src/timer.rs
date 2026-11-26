//! Timer forwarding and delivery accounting over supplied monotonic times.

/// A timer setting; overdue signals report a positive remainder until serviced.
/// Replacing a deadline requires a constructor, which also resets its clock history.
/// ```compile_fail
/// use lumen_signal::timer::Setting;
/// let mut setting = Setting::realtime(80, 3, 100, 10);
/// setting.deadline = Some(100);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Setting {
    deadline: Option<u64>,
    interval: u64,
    overrun: u64,
    overrun_last: i32,
    before_boot: u64,
}

impl Setting {
    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }
    pub fn interval(&self) -> u64 {
        self.interval
    }
    pub fn overrun(&self) -> u64 {
        self.overrun
    }
    pub fn overrun_last(&self) -> i32 {
        self.overrun_last
    }

    pub fn new(deadline: Option<u64>, interval: u64) -> Self {
        Self {
            deadline,
            interval: if deadline.is_some() { interval } else { 0 },
            overrun: 0,
            overrun_last: 0,
            before_boot: 0,
        }
    }

    /// Preserve the original phase even when the converted expiry predates
    /// monotonic zero. Such a setting is due immediately, with its elapsed
    /// history retained until forwarding accounts all missed periods.
    pub fn realtime(value: u64, interval: u64, realtime: u64, monotonic: u64) -> Self {
        if value == 0 {
            return Self::default();
        }
        let deadline = i128::from(monotonic) + i128::from(value) - i128::from(realtime);
        let mut setting = Self::new(Some(deadline.clamp(0, i128::from(u64::MAX)) as u64), interval);
        setting.before_boot = (-deadline).max(0) as u64;
        setting
    }

    pub fn remaining(&self, now: u64) -> u64 {
        self.deadline.map_or(0, |d| d.saturating_sub(now).max(1))
    }

    /// Advance past now in constant time, returning the number of expiries.
    /// At the representable clock limit an unrepresentable next expiry disarms.
    pub fn forward(&mut self, now: u64) -> u64 {
        let Some(deadline) = self.deadline.filter(|&d| d <= now) else { return 0 };
        if self.interval == 0 {
            self.deadline = None;
            return 1;
        }
        let elapsed = u128::from(now - deadline) + u128::from(self.before_boot);
        let count = elapsed / u128::from(self.interval) + 1;
        let next = u128::from(deadline) + count * u128::from(self.interval) - u128::from(self.before_boot);
        self.before_boot = 0;
        self.deadline = u64::try_from(next).ok();
        count.min(u128::from(u64::MAX)) as u64
    }

    pub fn missed(&mut self, count: u64) {
        self.overrun = self.overrun.saturating_add(count);
    }

    /// Count a refused enqueue. A one-shot has no later delivery to publish
    /// its loss, so expose it immediately; periodic losses await consumption.
    pub fn enqueue_refused(&mut self) {
        self.missed(1);
        if self.interval == 0 {
            self.overrun_last = self.overrun.min(i32::MAX as u64) as i32;
        }
    }

    /// Consumption publishes the overrun and resets its accumulator.
    pub fn delivered(&mut self) -> i32 {
        self.overrun_last = self.overrun.min(i32::MAX as u64) as i32;
        self.overrun = 0;
        self.overrun_last
    }
}

/// Linux alarm_setitimer: round up sub-second values, and fractions >= 0.5s.
pub fn alarm_seconds(ns: u64) -> u64 {
    let seconds = ns / 1_000_000_000;
    seconds + u64::from((ns != 0 && seconds == 0) || ns % 1_000_000_000 >= 500_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarding_preserves_phase_and_counts_missed_periods() {
        let mut s = Setting::new(Some(100), 10);
        assert_eq!(s.forward(99), 0);
        assert_eq!(s.remaining(99), 1);
        assert_eq!(s.forward(135), 4);
        assert_eq!(s.deadline, Some(140));
        assert_eq!(s.remaining(135), 5);
        assert_eq!(s.forward(140), 1);
        assert_eq!(s.deadline, Some(150));
        let mut one = Setting::new(Some(100), 0);
        assert_eq!(one.remaining(101), 1);
        assert_eq!(one.forward(101), 1);
        assert_eq!(one.remaining(101), 0);
        assert_eq!(one.forward(102), 0);
    }

    #[test]
    fn overruns_publish_only_on_delivery_and_saturate() {
        let mut s = Setting::new(Some(1), 1);
        let count = s.forward(20);
        s.missed(count - 1);
        assert_eq!(s.overrun_last, 0);
        assert_eq!(s.delivered(), 19);
        assert_eq!(s.overrun, 0);
        s.missed(u64::MAX);
        s.missed(1);
        assert_eq!(s.delivered(), i32::MAX);
        assert_eq!(s.delivered(), 0);
        assert_eq!(Setting::new(Some(100), 10).overrun_last, 0);
        let mut end = Setting::new(Some(0), 1);
        assert_eq!(end.forward(u64::MAX), u64::MAX);
        assert_eq!(end.deadline, None);
    }

    #[test]
    fn realtime_before_boot_keeps_phase_and_expiry_count() {
        const S: u64 = 1_000_000_000;
        let mut setting = Setting::realtime(80 * S, 3 * S, 100 * S + S / 2, 10 * S);
        assert_eq!(setting.forward(10 * S), 7);
        assert_eq!(setting.remaining(10 * S), S / 2);
        assert_eq!(setting.forward(10 * S), 0);
        assert_eq!(setting.forward(10 * S + S / 2), 1);
        assert_eq!(setting.remaining(10 * S + S / 2), 3 * S);
        let mut one = Setting::realtime(80 * S, 0, 100 * S + S / 2, 10 * S);
        assert_eq!(one.forward(10 * S), 1);
        assert_eq!(one.remaining(10 * S), 0);
        let future = Setting::realtime(105 * S, 3 * S, 100 * S + S / 2, 10 * S);
        assert_eq!(future.remaining(10 * S), 4 * S + S / 2);
    }

    #[test]
    fn disarm_clears_interval_for_both_clock_constructors() {
        assert_eq!(Setting::new(None, 99), Setting::default());
        assert_eq!(Setting::realtime(0, 99, 1000, 100), Setting::default());
    }

    #[test]
    fn refused_oneshot_publishes_loss_but_periodic_waits_for_delivery() {
        let mut one = Setting::new(Some(100), 0);
        assert_eq!(one.forward(135), 1);
        one.enqueue_refused();
        assert_eq!(one.deadline(), None);
        assert_eq!(one.overrun_last(), 1);
        assert_eq!(one.overrun_last(), 1);
        let mut periodic = Setting::new(Some(100), 10);
        periodic.missed(3);
        periodic.enqueue_refused();
        assert_eq!(periodic.overrun_last(), 0);
        assert_eq!(periodic.delivered(), 4);
        one = Setting::new(Some(200), 0);
        assert_eq!(one.overrun_last(), 0);
        assert_eq!(one.overrun(), 0);
    }

    #[test]
    fn replacing_setting_resets_preboot_history_and_accounting() {
        let mut setting = Setting::realtime(80, 3, 100, 10);
        assert_eq!(setting.deadline(), Some(0));
        setting.missed(7);
        setting.delivered();
        setting = Setting::new(Some(100), 10);
        assert_eq!(setting.forward(135), 4);
        assert_eq!(setting.deadline(), Some(140));
        assert_eq!(setting.overrun(), 0);
        assert_eq!(setting.overrun_last(), 0);
    }

    #[test]
    fn alarm_rounding_matches_linux() {
        for (ns, seconds) in
            [(0, 0), (1, 1), (499_999_999, 1), (1_499_999_999, 1), (1_500_000_000, 2), (2_000_000_000, 2)]
        {
            assert_eq!(alarm_seconds(ns), seconds);
        }
    }
}
