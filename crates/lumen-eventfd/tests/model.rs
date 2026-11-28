//! Differential model tests and `lumen_signal::timer::Setting` cross-checks
//! for `lumen-eventfd`.
//!
//! Intentionally imports only types (`EventCounter`, `TimerCount`, `ITimer`,
//! `Requeue`, error enums) — never the crate's constants — so the tests pin
//! Linux UAPI and arithmetic limits independently.
//!
//! Scope of independence:
//! - `EventfdModel` checks `EventCounter` independently using `u128` arithmetic
//!   and literal `0xffff_ffff_ffff_fffe` / `0xffff_ffff_ffff_ffff`.
//! - `NaiveTimerModel` is an independent reference for **expiry forwarding,
//!   remaining-time projection, `readable`, `next_wakeup`, and `Requeue`
//!   bookkeeping** (using a signed `i128` timeline, one-period-at-a-time loops,
//!   a clone-based `readable` check, and a simulated worker slot), **not** for
//!   `arm` policy; `arm` and `arm_realtime` rules are pinned separately by the
//!   Linux-derived table tests below.

use lumen_eventfd::{EventCounter, ITimer, ReadBytesError, Requeue, TimerCount, WouldBlock, WriteError};
use lumen_signal::timer::Setting;

/// Independent SplitMix64 PRNG for deterministic fuzzing.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, upper: u64) -> u64 {
        self.next() % upper
    }
}

/// Literal Linux constants (not imported from `lumen_eventfd`).
const LINUX_EVENTFD_MAX: u128 = 0xffff_ffff_ffff_fffe;
const LINUX_EVENTFD_INVALID_WRITE: u64 = 0xffff_ffff_ffff_ffff;
const LINUX_IO_BYTES: usize = 8;
const LINUX_KTIME_MAX: u64 = 9_223_372_036_854_775_807;

/// Reference model for Linux `fs/eventfd.c` using `u128` arithmetic.
struct EventfdModel {
    count: u128,
    semaphore: bool,
}

impl EventfdModel {
    fn new(initval: u32, semaphore: bool) -> Self {
        Self { count: u128::from(initval), semaphore }
    }

    fn write(&mut self, add: u64) -> Result<(), WriteError> {
        // fs/eventfd.c eventfd_write: `if (ucnt == ULLONG_MAX) return -EINVAL;`
        if add == LINUX_EVENTFD_INVALID_WRITE {
            return Err(WriteError::Invalid);
        }
        // fs/eventfd.c eventfd_write: `if (ULLONG_MAX - ctx->count > ucnt)`
        if self.count + u128::from(add) > LINUX_EVENTFD_MAX {
            return Err(WriteError::WouldBlock);
        }
        self.count += u128::from(add);
        Ok(())
    }

    fn read(&mut self) -> Result<u64, WouldBlock> {
        if self.count == 0 {
            return Err(WouldBlock);
        }
        if self.semaphore {
            self.count -= 1;
            Ok(1)
        } else {
            let v = self.count as u64;
            self.count = 0;
            Ok(v)
        }
    }

    fn readable(&self) -> bool {
        self.count > 0
    }

    fn writable(&self) -> bool {
        // fs/eventfd.c eventfd_poll: `if (ULLONG_MAX - 1 > count) events |= EPOLLOUT;`
        self.count < LINUX_EVENTFD_MAX
    }
}

#[test]
fn eventfd_matches_u128_reference_model() {
    let mut rng = Rng(0x4580_cafe_1234_5678);
    for semaphore in [false, true] {
        for initval in [0u32, 1, 7, u32::MAX] {
            let mut dut = EventCounter::new(initval, semaphore);
            let mut model = EventfdModel::new(initval, semaphore);

            for step in 0..2_000 {
                assert_eq!(dut.count(), model.count as u64, "step {step} count mismatch");
                assert_eq!(dut.readable(), model.readable(), "step {step} readable mismatch");
                assert_eq!(dut.writable(), model.writable(), "step {step} writable mismatch");
                assert_eq!(dut.is_semaphore(), semaphore);

                match rng.range(10) {
                    0..=4 => {
                        // Pick boundary-heavy write values.
                        let add = match rng.range(8) {
                            0 => 0,
                            1 => 1,
                            2 => 2,
                            3 => LINUX_EVENTFD_MAX as u64,
                            4 => LINUX_EVENTFD_INVALID_WRITE,
                            5 => (LINUX_EVENTFD_MAX - model.count) as u64,
                            6 => ((LINUX_EVENTFD_MAX - model.count) as u64).saturating_add(1),
                            _ => rng.next(),
                        };
                        if step % 3 == 0 {
                            let len = (rng.range(13)) as usize;
                            let mut buf = [0u8; 12];
                            buf[..LINUX_IO_BYTES].copy_from_slice(&add.to_ne_bytes());
                            let expected = if len < LINUX_IO_BYTES {
                                Err(WriteError::Invalid)
                            } else {
                                model.write(add).map(|()| LINUX_IO_BYTES)
                            };
                            assert_eq!(dut.write_bytes(&buf[..len]), expected, "step {step} write_bytes({add:#x})");
                        } else {
                            assert_eq!(dut.write(add), model.write(add), "step {step} write({add:#x})");
                        }
                    }
                    _ => {
                        if step % 3 == 0 {
                            let len = (rng.range(13)) as usize;
                            let mut buf = [0xA5u8; 12];
                            let expected = if len < LINUX_IO_BYTES {
                                Err(ReadBytesError::Invalid)
                            } else {
                                model.read().map_err(ReadBytesError::from)
                            };
                            let got = dut.read_bytes(&mut buf[..len]);
                            match expected {
                                Ok(v) => {
                                    assert_eq!(got, Ok(LINUX_IO_BYTES));
                                    assert_eq!(u64::from_ne_bytes(buf[..LINUX_IO_BYTES].try_into().unwrap()), v);
                                    assert!(buf[LINUX_IO_BYTES..len].iter().all(|&b| b == 0xA5));
                                }
                                Err(e) => assert_eq!(got, Err(e), "step {step} read_bytes(len={len})"),
                            }
                        } else {
                            assert_eq!(dut.read(), model.read(), "step {step} read");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn timer_counts_across_missed_intervals_match_table_and_lumen_signal_setting() {
    // Each row: (deadline, interval, now, expected_expiries, expected_next_deadline).
    // Checked both against this hand-verified table AND against
    // `lumen_signal::timer::Setting::forward` + `Setting::missed` (crates/lumen-signal/src/timer.rs).
    // Note: `deadline == u64::MAX` and computed `next == u64::MAX` are tested
    // separately because `TimerCount` reserves `u64::MAX` for the "never" sentinel.
    let table: &[(u64, u64, u64, u64, Option<u64>)] = &[
        // Before deadline: 0 expiries, deadline unchanged.
        (100, 0, 99, 0, Some(100)),
        (100, 10, 99, 0, Some(100)),
        (100, 10, 0, 0, Some(100)),
        // One-shot at and past deadline: 1 expiry, disarmed.
        (100, 0, 100, 1, None),
        (100, 0, 101, 1, None),
        (100, 0, u64::MAX, 1, None),
        // Periodic exact hit: 1 expiry, advances by 1 interval.
        (100, 10, 100, 1, Some(110)),
        (100, 10, 109, 1, Some(110)),
        // Periodic missed intervals (overruns): phase preserved.
        (100, 10, 110, 2, Some(120)),
        (100, 10, 135, 4, Some(140)),
        (100, 10, 139, 4, Some(140)),
        (100, 10, 140, 5, Some(150)),
        (1, 1, 20, 20, Some(21)),
        (50, 7, 100, 8, Some(106)),
        // Large nanosecond timestamps (seconds / hours).
        (1_000_000_000, 10_000_000, 1_000_000_000, 1, Some(1_010_000_000)),
        (1_000_000_000, 10_000_000, 2_000_000_000, 101, Some(2_010_000_000)),
        (1_000_000_000, 3_000_000_000, 100_500_000_000, 34, Some(103_000_000_000)),
        // Large interval exceeding remaining u64 range -> disarms after firing.
        (u64::MAX - 5, 10, u64::MAX - 5, 1, None),
        (u64::MAX - 5, 10, u64::MAX, 1, None),
        (1, u64::MAX, u64::MAX, 1, None),
        // Maximum possible expiry count across u64 span.
        (1, 1, u64::MAX, u64::MAX, None),
        (2, 1, u64::MAX, u64::MAX - 1, None),
    ];

    for &(deadline, interval, now, expected_count, expected_next) in table {
        // 1. Check TimerCount against the table.
        let mut tc = TimerCount::new();
        let _ = tc.arm(0, ITimer::new(deadline, interval), true);
        let (got, _) = tc.on_expiry(now);
        assert_eq!(
            got, expected_count,
            "TimerCount::on_expiry mismatch for deadline={deadline}, interval={interval}, now={now}"
        );
        assert_eq!(
            tc.deadline(),
            expected_next,
            "TimerCount::deadline mismatch for deadline={deadline}, interval={interval}, now={now}"
        );
        if expected_count == 0 {
            assert!(!tc.readable(now));
            assert_eq!(tc.read(now), Err(WouldBlock));
        } else {
            assert!(tc.readable(now));
            assert_eq!(tc.read(now).map(|(n, _)| n), Ok(expected_count));
        }

        // 2. Check lumen_signal::timer::Setting::forward + Setting::missed agrees identically.
        let mut setting = Setting::new(Some(deadline), interval);
        let sig_count = setting.forward(now);
        setting.missed(sig_count);
        assert_eq!(
            got, sig_count,
            "TimerCount disagreed with Setting::forward for deadline={deadline}, interval={interval}, now={now}"
        );
        assert_eq!(
            tc.deadline(),
            setting.deadline(),
            "TimerCount next deadline disagreed with Setting for deadline={deadline}, interval={interval}, now={now}"
        );
        assert_eq!(
            setting.overrun(),
            expected_count,
            "Setting::missed disagreed for deadline={deadline}, interval={interval}, now={now}"
        );
    }
}

#[test]
fn arm_realtime_matches_table_and_lumen_signal_setting_realtime() {
    struct RealtimeRow {
        value: u64,
        interval: u64,
        rt: u64,
        mono: u64,
        eval1: u64,
        count1: u64,
        next1: Option<u64>,
        rem1: u64,
        eval2: u64,
        count2: u64,
        next2: Option<u64>,
        rem2: u64,
    }

    // Includes future, past-after-boot, converted deadline == 0, and converted
    // deadline < 0 (pre-boot), each verified across TWO consecutive forwards
    // against `Setting::realtime(..).forward(..)` so clearing `before_boot`
    // after the first forward is exercised on every row.
    const S: u64 = 1_000_000_000;
    let table: &[RealtimeRow] = &[
        // Future realtime deadline: monotonic = 10s + (105s - 100.5s) = 14.5s.
        RealtimeRow {
            value: 105 * S,
            interval: 3 * S,
            rt: 100 * S + S / 2,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 0,
            next1: Some(14 * S + S / 2),
            rem1: 4 * S + S / 2,
            eval2: 18 * S,
            count2: 2,
            next2: Some(20 * S + S / 2),
            rem2: 2 * S + S / 2,
        },
        // Past after boot: monotonic = 10s + (95s - 100s) = 5s (5s, 8s => 2, next=11s; then at 14s: 11s, 14s => 2, next=17s).
        RealtimeRow {
            value: 95 * S,
            interval: 3 * S,
            rt: 100 * S,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 2,
            next1: Some(11 * S),
            rem1: S,
            eval2: 14 * S,
            count2: 2,
            next2: Some(17 * S),
            rem2: 3 * S,
        },
        // Converted deadline == 0 exactly: 10s + 90s - 100s = 0 (0s, 3s, 6s, 9s => 4, next=12s; then at 12s => 1, next=15s).
        RealtimeRow {
            value: 90 * S,
            interval: 3 * S,
            rt: 100 * S,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 4,
            next1: Some(12 * S),
            rem1: 2 * S,
            eval2: 12 * S,
            count2: 1,
            next2: Some(15 * S),
            rem2: 3 * S,
        },
        // Converted deadline == 0, one-shot: fires once and disarms, second forward is 0.
        RealtimeRow {
            value: 90 * S,
            interval: 0,
            rt: 100 * S,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 1,
            next1: None,
            rem1: 0,
            eval2: 20 * S,
            count2: 0,
            next2: None,
            rem2: 0,
        },
        // Pre-boot (< 0): 10s + 80s - 100.5s = -10.5s (Setting::realtime canonical vector).
        // First forward at 10s => 7, next=10.5s; second forward at 10.5s => 1, next=13.5s.
        RealtimeRow {
            value: 80 * S,
            interval: 3 * S,
            rt: 100 * S + S / 2,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 7,
            next1: Some(10 * S + S / 2),
            rem1: S / 2,
            eval2: 10 * S + S / 2,
            count2: 1,
            next2: Some(13 * S + S / 2),
            rem2: 3 * S,
        },
        // Pre-boot (< 0), one-shot: fires once, disarms, second forward is 0.
        RealtimeRow {
            value: 80 * S,
            interval: 0,
            rt: 100 * S + S / 2,
            mono: 10 * S,
            eval1: 10 * S,
            count1: 1,
            next1: None,
            rem1: 0,
            eval2: 15 * S,
            count2: 0,
            next2: None,
            rem2: 0,
        },
        // Pre-boot (< 0), evaluated at 14s (9 expiries, next=16.5s), then at 20s (16.5s, 19.5s => 2, next=22.5s).
        RealtimeRow {
            value: 80 * S,
            interval: 3 * S,
            rt: 100 * S + S / 2,
            mono: 10 * S,
            eval1: 14 * S,
            count1: 9,
            next1: Some(16 * S + S / 2),
            rem1: 2 * S + S / 2,
            eval2: 20 * S,
            count2: 2,
            next2: Some(22 * S + S / 2),
            rem2: 2 * S + S / 2,
        },
        // Deep pre-boot: value=10, interval=7, rt=100, mono=5 => deadline=-85,
        // first at 5 => 13 expiries, next=6, rem=1; second at 15 => 6, 13 => 2 expiries, next=20, rem=5.
        RealtimeRow {
            value: 10,
            interval: 7,
            rt: 100,
            mono: 5,
            eval1: 5,
            count1: 13,
            next1: Some(6),
            rem1: 1,
            eval2: 15,
            count2: 2,
            next2: Some(20),
            rem2: 5,
        },
    ];

    for r in table {
        let mut tc = TimerCount::new();
        let _ = tc.arm_realtime(r.mono, r.rt, ITimer::new(r.value, r.interval));
        let mut setting = Setting::realtime(r.value, r.interval, r.rt, r.mono);

        // First forward (and pre-expiry get).
        assert_eq!(tc.get(r.eval1), ITimer::new(r.rem1, r.interval));
        let (got1, _) = tc.on_expiry(r.eval1);
        assert_eq!(got1, r.count1);
        assert_eq!(tc.deadline(), r.next1);
        assert_eq!(tc.get(r.eval1), ITimer::new(r.rem1, r.interval));

        let sig1 = setting.forward(r.eval1);
        assert_eq!(got1, sig1);
        assert_eq!(tc.deadline(), setting.deadline());

        // Second forward (and pre-second-expiry get) after before_boot must have cleared.
        assert_eq!(tc.get(r.eval2), ITimer::new(r.rem2, r.interval));
        let (got2, _) = tc.on_expiry(r.eval2);
        assert_eq!(got2, r.count2);
        assert_eq!(tc.deadline(), r.next2);
        assert_eq!(tc.get(r.eval2), ITimer::new(r.rem2, r.interval));

        let sig2 = setting.forward(r.eval2);
        assert_eq!(got2, sig2);
        assert_eq!(tc.deadline(), setting.deadline());
    }
}

#[test]
fn arm_matches_linux_timerfd_setup_and_old_value_rules() {
    // Linux v6.12 fs/timerfd.c:
    // - timerfd_setup: `ctx->expired = 0; ctx->ticks = 0; ctx->tintv = ktmr->it_interval; if (texp != 0) ...`
    // - do_timerfd_settime: snapshots `old` (forwarding overdue periodic) before replacing.
    // - timerfd_get_remaining: returns 0 once overdue for one-shot timers.
    struct ArmCase {
        name: &'static str,
        init: (u64, ITimer, bool),
        rearm: (u64, ITimer, bool),
        expected_old: ITimer,
        expected_new_deadline: Option<u64>,
        expected_new_get: ITimer,
    }

    let cases = [
        ArmCase {
            name: "absolute disarm with nonzero interval keeps interval",
            init: (100, ITimer::new(50, 10), false),
            rearm: (120, ITimer::new(0, 77), true),
            expected_old: ITimer::new(30, 10),
            expected_new_deadline: None,
            expected_new_get: ITimer::new(0, 77),
        },
        ArmCase {
            name: "relative disarm with nonzero interval keeps interval",
            init: (100, ITimer::new(50, 10), false),
            rearm: (120, ITimer::new(0, 55), false),
            expected_old: ITimer::new(30, 10),
            expected_new_deadline: None,
            expected_new_get: ITimer::new(0, 55),
        },
        ArmCase {
            name: "overdue periodic timer returns projected next-phase old value",
            init: (100, ITimer::new(20, 10), false), // deadline=120, interval=10
            rearm: (145, ITimer::new(50, 0), false), // overdue: 120,130,140 -> next=150 -> rem=5
            expected_old: ITimer::new(5, 10),
            expected_new_deadline: Some(195),
            expected_new_get: ITimer::new(50, 0),
        },
        ArmCase {
            name: "overdue periodic timer at exact boundary returns interval as old remaining",
            init: (100, ITimer::new(20, 10), false),  // deadline=120
            rearm: (140, ITimer::new(300, 25), true), // at boundary 140 -> next=150 -> rem=10
            expected_old: ITimer::new(10, 10),
            expected_new_deadline: Some(300),
            expected_new_get: ITimer::new(160, 25),
        },
        ArmCase {
            name: "overdue one-shot timer returns (0, 0) as old value (timerfd_get_remaining)",
            init: (100, ITimer::new(20, 0), false), // deadline=120, one-shot
            rearm: (145, ITimer::new(50, 10), false),
            expected_old: ITimer::new(0, 0),
            expected_new_deadline: Some(195),
            expected_new_get: ITimer::new(50, 10),
        },
        ArmCase {
            name: "absolute arm in the past sets deadline verbatim and projects get",
            init: (0, ITimer::new(0, 0), false),
            rearm: (135, ITimer::new(100, 10), true),
            expected_old: ITimer::new(0, 0),
            expected_new_deadline: Some(100),
            expected_new_get: ITimer::new(5, 10),
        },
    ];

    for c in cases {
        let mut tc = TimerCount::new();
        let _ = tc.arm(c.init.0, c.init.1, c.init.2);
        let (old, _) = tc.arm(c.rearm.0, c.rearm.1, c.rearm.2);
        assert_eq!(old, c.expected_old, "{}: old mismatch", c.name);
        assert_eq!(tc.deadline(), c.expected_new_deadline, "{}: deadline mismatch", c.name);
        assert_eq!(tc.get(c.rearm.0), c.expected_new_get, "{}: get mismatch", c.name);
        assert_eq!(tc.ticks(), 0, "{}: arm must reset ticks", c.name);
    }
}

/// Independent step-by-step reference model for `TimerCount`'s expiry
/// forwarding, remaining-time projection, `readable`, and `next_wakeup`
/// state machine.
///
/// Stores the deadline as a signed `i128` (no `before_boot` split), advances
/// one period at a time in a loop (no division/modulo), derives `readable(now)`
/// by simulating `read(now)` on a clone, and derives `next_wakeup()` from its
/// own `fired_unread` latch rather than copying the crate's predicates.
#[derive(Clone)]
struct NaiveTimerModel {
    deadline: Option<i128>,
    interval: u64,
    ticks: u64,
    fired_unread: bool,
}

impl NaiveTimerModel {
    fn new() -> Self {
        Self { deadline: None, interval: 0, ticks: 0, fired_unread: false }
    }

    fn exposed_deadline(&self) -> Option<u64> {
        self.deadline.map(|d| d.clamp(0, i128::from(u64::MAX)) as u64)
    }

    fn next_wakeup(&self) -> Option<u64> {
        if self.fired_unread {
            return None;
        }
        self.exposed_deadline().filter(|&d| d != u64::MAX)
    }

    fn readable(&self, now: u64) -> bool {
        let mut probe = self.clone();
        probe.read(now).is_ok()
    }

    fn get(&self, now: u64) -> ITimer {
        let Some(mut d) = self.deadline else {
            return ITimer::new(0, self.interval);
        };
        let now128 = i128::from(now);
        let max128 = i128::from(u64::MAX);
        let raw = if d >= max128 || d > now128 {
            (d.min(max128) - now128) as u64
        } else if self.interval == 0 {
            0
        } else {
            let iv = i128::from(self.interval);
            while d <= now128 {
                d += iv;
                if d >= max128 {
                    return ITimer::new(0, self.interval);
                }
            }
            (d - now128) as u64
        };
        ITimer::new(raw.min(LINUX_KTIME_MAX.saturating_sub(now)), self.interval)
    }

    fn arm(&mut self, now: u64, it: ITimer, absolute: bool) -> ITimer {
        let old = self.get(now);
        self.ticks = 0;
        self.fired_unread = false;
        self.interval = it.interval;
        self.deadline = if it.value == 0 {
            None
        } else if absolute {
            Some(i128::from(it.value))
        } else {
            Some(i128::from(now.saturating_add(it.value)))
        };
        old
    }

    fn arm_realtime(&mut self, now_mono: u64, now_rt: u64, it: ITimer) -> ITimer {
        let old = self.get(now_mono);
        self.ticks = 0;
        self.fired_unread = false;
        self.interval = it.interval;
        self.deadline = if it.value == 0 {
            None
        } else {
            let raw = i128::from(now_mono) + i128::from(it.value) - i128::from(now_rt);
            Some(raw.min(i128::from(u64::MAX)))
        };
        old
    }

    fn disarm(&mut self) {
        *self = Self::new();
    }

    fn on_expiry(&mut self, now: u64) -> u64 {
        let Some(mut d) = self.deadline else {
            return 0;
        };
        let now128 = i128::from(now);
        let max128 = i128::from(u64::MAX);
        if d >= max128 || d > now128 {
            return 0;
        }
        if self.interval == 0 {
            self.deadline = None;
            self.ticks = self.ticks.saturating_add(1);
            self.fired_unread = true;
            return 1;
        }
        let iv = i128::from(self.interval);
        let mut count = 0u64;
        while d <= now128 {
            count = count.saturating_add(1);
            d += iv;
            if d >= max128 {
                self.deadline = None;
                self.ticks = self.ticks.saturating_add(count);
                self.fired_unread = true;
                return count;
            }
        }
        self.deadline = Some(d);
        self.ticks = self.ticks.saturating_add(count);
        self.fired_unread = true;
        count
    }

    fn read(&mut self, now: u64) -> Result<u64, WouldBlock> {
        self.on_expiry(now);
        if self.ticks == 0 {
            return Err(WouldBlock);
        }
        let v = self.ticks;
        self.ticks = 0;
        self.fired_unread = false;
        Ok(v)
    }
}

fn apply_requeue(worker_slot: &mut Option<u64>, req: Requeue, step: usize, op: &str) {
    if let Some(rm) = req.remove {
        assert_eq!(*worker_slot, Some(rm), "step {step} {op}: Requeue.remove did not match queued worker key");
        *worker_slot = None;
    }
    if let Some(ins) = req.insert {
        assert_eq!(*worker_slot, None, "step {step} {op}: Requeue.insert overwrote live worker key");
        *worker_slot = Some(ins);
    }
}

#[test]
fn timerfd_matches_naive_step_by_step_model() {
    let mut rng = Rng(0x4580_beef_dead_c0de);
    let mut dut = TimerCount::new();
    let mut model = NaiveTimerModel::new();
    let mut worker_slot: Option<u64> = None;
    let mut now = 100u64;
    const RT_OFFSET: u64 = 500;

    for step in 0..3_000 {
        assert_eq!(dut.deadline(), model.exposed_deadline(), "step {step} deadline");
        assert_eq!(dut.next_wakeup(), model.next_wakeup(), "step {step} next_wakeup");
        assert_eq!(worker_slot, model.next_wakeup(), "step {step} simulated worker slot diverged");
        assert_eq!(dut.interval(), model.interval, "step {step} interval");
        assert_eq!(dut.ticks(), model.ticks, "step {step} ticks");
        assert_eq!(dut.readable(now), model.readable(now), "step {step} readable({now})");
        assert_eq!(dut.get(now), model.get(now), "step {step} get({now})");

        match rng.range(12) {
            0..=1 => {
                // Arm relative or absolute (sometimes past, value=0, or u64::MAX).
                let absolute = rng.range(2) == 0;
                let value = match rng.range(7) {
                    0 => 0,
                    1 if absolute => now.saturating_sub(rng.range(40)).max(1),
                    2 if absolute => now,
                    3 if absolute => u64::MAX,
                    _ => {
                        if absolute {
                            now.saturating_add(rng.range(50) + 1)
                        } else {
                            rng.range(50) + 1
                        }
                    }
                };
                let interval = if rng.range(4) == 0 { 0 } else { rng.range(20) + 1 };
                let it = ITimer::new(value, interval);
                let (old, rq) = dut.arm(now, it, absolute);
                apply_requeue(&mut worker_slot, rq, step, "arm");
                assert_eq!(old, model.arm(now, it, absolute), "step {step} arm");
            }
            2 => {
                // Arm realtime absolute (including pre-boot < 0, == 0, past, future, value=0, u64::MAX).
                let now_rt = now + RT_OFFSET;
                let value = match rng.range(6) {
                    0 => 0,
                    1 => now_rt.saturating_sub(now + rng.range(80) + 1).max(1), // pre-boot (< 0)
                    2 => now_rt - now,                                          // converted deadline == 0
                    3 => u64::MAX,
                    _ => now_rt.saturating_add(rng.range(60)).saturating_sub(30).max(1),
                };
                let interval = if rng.range(4) == 0 { 0 } else { rng.range(20) + 1 };
                let it = ITimer::new(value, interval);
                let (old, rq) = dut.arm_realtime(now, now_rt, it);
                apply_requeue(&mut worker_slot, rq, step, "arm_realtime");
                assert_eq!(old, model.arm_realtime(now, now_rt, it), "step {step} arm_realtime");
            }
            3 => {
                let rq = dut.disarm();
                apply_requeue(&mut worker_slot, rq, step, "disarm");
                model.disarm();
            }
            4..=6 => {
                // Advance time and run worker expiry.
                now = now.saturating_add(rng.range(100));
                let (added, rq) = dut.on_expiry(now);
                apply_requeue(&mut worker_slot, rq, step, "on_expiry");
                assert_eq!(added, model.on_expiry(now), "step {step} on_expiry({now})");
            }
            7 => {
                // Advance time without running the worker (tests read-time forwarding and readable(now)).
                now = now.saturating_add(rng.range(80));
            }
            8 => {
                // Query get() and readable() at future times without mutating.
                let probe = now.saturating_add(rng.range(80));
                assert_eq!(dut.get(probe), model.get(probe), "step {step} get({probe})");
                assert_eq!(dut.readable(probe), model.readable(probe), "step {step} readable({probe})");
            }
            _ => {
                if step % 3 == 0 {
                    let len = (rng.range(12)) as usize;
                    let mut buf = [0x33u8; 11];
                    let expected = if len < LINUX_IO_BYTES {
                        Err(ReadBytesError::Invalid)
                    } else {
                        model.read(now).map_err(ReadBytesError::from)
                    };
                    let got = dut.read_bytes(&mut buf[..len], now);
                    match expected {
                        Ok(v) => {
                            let (n, rq) = got.expect("expected read_bytes Ok");
                            apply_requeue(&mut worker_slot, rq, step, "read_bytes");
                            assert_eq!(n, LINUX_IO_BYTES);
                            assert_eq!(u64::from_ne_bytes(buf[..LINUX_IO_BYTES].try_into().unwrap()), v);
                            assert!(buf[LINUX_IO_BYTES..len].iter().all(|&b| b == 0x33));
                        }
                        Err(e) => assert_eq!(got, Err(e), "step {step} read_bytes(len={len})"),
                    }
                } else {
                    match (dut.read(now), model.read(now)) {
                        (Ok((v, rq)), Ok(ev)) => {
                            apply_requeue(&mut worker_slot, rq, step, "read");
                            assert_eq!(v, ev, "step {step} read({now})");
                        }
                        (Err(e), Err(ee)) => assert_eq!(e, ee, "step {step} read({now})"),
                        (got, exp) => panic!("step {step} read({now}): got {got:?}, expected {exp:?}"),
                    }
                }
            }
        }
    }
}
