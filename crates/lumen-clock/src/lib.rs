//! The clock app's logic (#200), host-tested: the time of day from the
//! RTC's seconds (`time_wall`, taken as UTC) and from `run clock --at`, and
//! the face's geometry in integers (the apps have no `libm`, ARCHITECTURE
//! section 8): an integer sine table in half-degree steps, each hand's
//! angle, a point at a radius and angle, and the pixels of a straight line.
//!
//! Angles are **steps**: half-degrees clockwise from 12 o'clock, `0..STEPS`.
//!
//! [`face`] is the clock's window as a `lumen_ui::App`: its drawing and its
//! damage, checked against full repaints.

#![no_std]

extern crate alloc;

pub mod face;

use core::fmt;

/// Half-degree steps in a turn.
pub const STEPS: u32 = 720;

/// What [`sin`] and [`cos`] give for 1: values are scaled by `2^14`.
pub const SCALE: i32 = 1 << 14;

/// `sin` of 0° to 90° in half-degree steps, scaled by [`SCALE`] and
/// rounded (a host test checks every entry against `f64::sin`).
const QUARTER: [i16; 181] = [
    0, 143, 286, 429, 572, 715, 857, 1000, 1143, 1285, 1428, 1570, //
    1713, 1855, 1997, 2139, 2280, 2422, 2563, 2704, 2845, 2986, 3126, 3266, //
    3406, 3546, 3686, 3825, 3964, 4102, 4240, 4378, 4516, 4653, 4790, 4927, //
    5063, 5199, 5334, 5469, 5604, 5738, 5872, 6005, 6138, 6270, 6402, 6533, //
    6664, 6794, 6924, 7053, 7182, 7311, 7438, 7565, 7692, 7818, 7943, 8068, //
    8192, 8316, 8438, 8561, 8682, 8803, 8923, 9043, 9162, 9280, 9397, 9514, //
    9630, 9746, 9860, 9974, 10087, 10199, 10311, 10422, 10531, 10641, 10749, 10856, //
    10963, 11069, 11174, 11278, 11381, 11484, 11585, 11686, 11786, 11885, 11982, 12080, //
    12176, 12271, 12365, 12458, 12551, 12642, 12733, 12822, 12911, 12998, 13085, 13170, //
    13255, 13338, 13421, 13502, 13583, 13662, 13741, 13818, 13894, 13970, 14044, 14117, //
    14189, 14260, 14330, 14399, 14466, 14533, 14598, 14663, 14726, 14788, 14849, 14909, //
    14968, 15025, 15082, 15137, 15191, 15244, 15296, 15346, 15396, 15444, 15491, 15537, //
    15582, 15626, 15668, 15709, 15749, 15788, 15826, 15862, 15897, 15931, 15964, 15996, //
    16026, 16055, 16083, 16110, 16135, 16159, 16182, 16204, 16225, 16244, 16262, 16279, //
    16294, 16309, 16322, 16333, 16344, 16353, 16362, 16368, 16374, 16378, 16382, 16383, //
    16384,
];

/// `sin` of `step` (any number: taken modulo [`STEPS`]), scaled by
/// [`SCALE`].
pub fn sin(step: u32) -> i32 {
    let s = step % STEPS;
    let q = (s % 360) as usize;
    let v = if q <= 180 { QUARTER[q] } else { QUARTER[360 - q] };
    if s < 360 { i32::from(v) } else { -i32::from(v) }
}

/// `cos` of `step`, scaled by [`SCALE`].
pub fn cos(step: u32) -> i32 {
    sin(step % STEPS + 180)
}

/// `n / SCALE` rounded to the nearest (halves up).
fn unscale(n: i32) -> i32 {
    (n + SCALE / 2).div_euclid(SCALE)
}

/// The point `radius` pixels from (`cx`, `cy`) at angle `step` (clockwise
/// from 12 o'clock; y grows downwards, as on the screen).
pub fn point(cx: i32, cy: i32, radius: i32, step: u32) -> (i32, i32) {
    (cx + unscale(radius * sin(step)), cy - unscale(radius * cos(step)))
}

/// The pixels of the straight line from `from` to `to`, both included, each
/// next to the one before (8-connected): Bresenham's.
pub fn line(from: (i32, i32), to: (i32, i32)) -> Line {
    let (dx, dy) = ((to.0 - from.0).abs(), -(to.1 - from.1).abs());
    Line {
        at: from,
        to,
        dx,
        dy,
        sx: if from.0 < to.0 { 1 } else { -1 },
        sy: if from.1 < to.1 { 1 } else { -1 },
        err: dx + dy,
        done: false,
    }
}

/// [`line`]'s pixels, in order from its start.
#[derive(Clone, Debug)]
pub struct Line {
    at: (i32, i32),
    to: (i32, i32),
    dx: i32,
    dy: i32,
    sx: i32,
    sy: i32,
    err: i32,
    done: bool,
}

impl Iterator for Line {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<(i32, i32)> {
        if self.done {
            return None;
        }
        let here = self.at;
        if here == self.to {
            self.done = true;
            return Some(here);
        }
        let e2 = 2 * self.err;
        if e2 >= self.dy {
            self.err += self.dy;
            self.at.0 += self.sx;
        }
        if e2 <= self.dx {
            self.err += self.dx;
            self.at.1 += self.sy;
        }
        Some(here)
    }
}

/// A time of day, to the second (`00:00:00` to `23:59:59`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeOfDay {
    h: u8,
    m: u8,
    s: u8,
}

/// Seconds in a day.
const DAY: u64 = 24 * 60 * 60;

impl TimeOfDay {
    /// `h:m:s`, if it is a time of day.
    pub fn new(h: u8, m: u8, s: u8) -> Option<TimeOfDay> {
        (h < 24 && m < 60 && s < 60).then_some(TimeOfDay { h, m, s })
    }

    /// The time of day `secs` seconds after 1970-01-01 00:00:00 (what
    /// `time_wall` gives: the RTC taken as UTC, so this is UTC too, as
    /// `date` shows it).
    pub fn from_unix(secs: u64) -> TimeOfDay {
        let t = secs % DAY;
        TimeOfDay { h: (t / 3600) as u8, m: (t / 60 % 60) as u8, s: (t % 60) as u8 }
    }

    /// `HH:MM` or `HH:MM:SS` (the hour one digit or two, the minutes and
    /// seconds two each; seconds 0 if left out), as `run clock --at` takes
    /// it; `None` for anything else or a time past `23:59:59`.
    pub fn parse(text: &str) -> Option<TimeOfDay> {
        let mut parts = text.split(':');
        let h = number(parts.next()?, 1..=2)?;
        let m = number(parts.next()?, 2..=2)?;
        let s = match parts.next() {
            Some(p) => number(p, 2..=2)?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        TimeOfDay::new(h, m, s)
    }

    pub fn hour(&self) -> u8 {
        self.h
    }

    pub fn minute(&self) -> u8 {
        self.m
    }

    pub fn second(&self) -> u8 {
        self.s
    }

    /// Where each hand points, in steps: the hour hand moving with the
    /// minutes (half a degree a minute), the minute hand with the seconds
    /// (a step every five), the second hand a second at a time.
    pub fn hands(&self) -> Hands {
        let (h, m, s) = (u32::from(self.h), u32::from(self.m), u32::from(self.s));
        Hands { hour: (h % 12) * 60 + m, minute: m * 12 + s / 5, second: s * 12 }
    }
}

/// `text` as a number, if it is ASCII digits only, as many as `len` allows.
fn number(text: &str, len: core::ops::RangeInclusive<usize>) -> Option<u8> {
    if !len.contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `HH:MM:SS`, zero-padded (what the clock's label shows).
impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}:{:02}", self.h, self.m, self.s)
    }
}

/// Each hand's angle, in steps (clockwise from 12 o'clock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hands {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::format;
    use std::vec::Vec;

    #[test]
    fn the_table_is_sine_rounded() {
        for (i, &v) in QUARTER.iter().enumerate() {
            let want = ((i as f64 / 2.0).to_radians().sin() * f64::from(SCALE)).round() as i32;
            assert_eq!(i32::from(v), want, "step {i}");
        }
    }

    #[test]
    fn sine_and_cosine_over_the_whole_turn() {
        for step in 0..STEPS * 2 {
            let a = (f64::from(step) / 2.0).to_radians();
            let (s, c) = (sin(step), cos(step));
            assert!((f64::from(s) - a.sin() * f64::from(SCALE)).abs() <= 0.5, "sin {step}: {s}");
            assert!((f64::from(c) - a.cos() * f64::from(SCALE)).abs() <= 0.5, "cos {step}: {c}");
        }
        assert_eq!((sin(0), sin(180), sin(360), sin(540)), (0, SCALE, 0, -SCALE));
        assert_eq!((cos(0), cos(180), cos(360), cos(540)), (SCALE, 0, -SCALE, 0));
        assert_eq!(sin(u32::MAX), sin(u32::MAX % STEPS));
        assert_eq!(cos(u32::MAX), cos(u32::MAX % STEPS));
    }

    #[test]
    fn points_at_the_quarters_and_between() {
        let (cx, cy, r) = (100, 50, 40);
        assert_eq!(point(cx, cy, r, 0), (100, 10)); // 12 o'clock
        assert_eq!(point(cx, cy, r, 180), (140, 50)); // 3
        assert_eq!(point(cx, cy, r, 360), (100, 90)); // 6
        assert_eq!(point(cx, cy, r, 540), (60, 50)); // 9
        // 45°: r / √2 = 28.28 each way, rounded.
        assert_eq!(point(cx, cy, r, 90), (128, 22));
        assert_eq!(point(cx, cy, r, 630), (72, 22));
        for step in 0..STEPS {
            let (x, y) = point(0, 0, 1000, step);
            let d = ((x * x + y * y) as f64).sqrt();
            assert!((d - 1000.0).abs() <= 1.0, "step {step}: ({x}, {y})");
        }
    }

    #[test]
    fn a_line_runs_from_end_to_end_one_pixel_at_a_time() {
        let ends = [((0, 0), (0, 0)), ((0, 0), (5, 0)), ((3, 7), (-4, 2)), ((10, 10), (12, -30)), ((-5, 1), (9, 6))];
        for (from, to) in ends {
            let pts: Vec<(i32, i32)> = line(from, to).collect();
            assert_eq!(pts.first(), Some(&from));
            assert_eq!(pts.last(), Some(&to));
            let len = (to.0 - from.0).abs().max((to.1 - from.1).abs()) as usize + 1;
            assert_eq!(pts.len(), len, "{from:?} -> {to:?}");
            for w in pts.windows(2) {
                let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
                assert!(dx.abs() <= 1 && dy.abs() <= 1 && (dx, dy) != (0, 0), "{w:?}");
            }
            // Backwards, as many pixels.
            assert_eq!(line(to, from).count(), len);
        }
    }

    #[test]
    fn the_time_of_day_from_unix_seconds() {
        assert_eq!(TimeOfDay::from_unix(0), TimeOfDay::new(0, 0, 0).unwrap());
        assert_eq!(TimeOfDay::from_unix(DAY - 1), TimeOfDay::new(23, 59, 59).unwrap());
        assert_eq!(TimeOfDay::from_unix(DAY), TimeOfDay::new(0, 0, 0).unwrap());
        // 2023-11-14 22:13:20 UTC.
        assert_eq!(TimeOfDay::from_unix(1_700_000_000), TimeOfDay::new(22, 13, 20).unwrap());
        assert_eq!(format!("{}", TimeOfDay::from_unix(u64::MAX)), "07:00:15");
    }

    #[test]
    fn shown_zero_padded() {
        assert_eq!(format!("{}", TimeOfDay::new(7, 5, 9).unwrap()), "07:05:09");
        assert_eq!(format!("{}", TimeOfDay::new(23, 59, 59).unwrap()), "23:59:59");
    }

    #[test]
    fn new_refuses_what_isnt_a_time_of_day() {
        assert!(TimeOfDay::new(24, 0, 0).is_none());
        assert!(TimeOfDay::new(0, 60, 0).is_none());
        assert!(TimeOfDay::new(0, 0, 60).is_none());
        assert!(TimeOfDay::new(23, 59, 59).is_some());
    }

    #[test]
    fn at_parses_hours_and_minutes_and_maybe_seconds() {
        let t = |h, m, s| TimeOfDay::new(h, m, s);
        assert_eq!(TimeOfDay::parse("10:09"), t(10, 9, 0));
        assert_eq!(TimeOfDay::parse("10:09:30"), t(10, 9, 30));
        assert_eq!(TimeOfDay::parse("7:05"), t(7, 5, 0));
        assert_eq!(TimeOfDay::parse("00:00:00"), t(0, 0, 0));
        assert_eq!(TimeOfDay::parse("23:59:59"), t(23, 59, 59));
        for bad in [
            "",
            "10",
            "10:",
            "10:9",
            "10:009",
            "100:09",
            ":09",
            "24:00",
            "10:60",
            "10:09:60",
            "10:09:",
            "10:09:5",
            "10:09:30:00",
            " 10:09",
            "10:09 ",
            "+1:05",
            "1a:05",
            "10:0x",
            "１0:09",
        ] {
            assert_eq!(TimeOfDay::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hands_point_where_a_clock_s_do() {
        let hands = |h, m, s| TimeOfDay::new(h, m, s).unwrap().hands();
        assert_eq!(hands(12, 0, 0), Hands { hour: 0, minute: 0, second: 0 });
        assert_eq!(hands(0, 0, 0), Hands { hour: 0, minute: 0, second: 0 });
        // 3:00: the hour hand at 90°; 6:30: half way between 6 and 7.
        assert_eq!(hands(15, 0, 0).hour, 180);
        assert_eq!(hands(6, 30, 0), Hands { hour: 390, minute: 360, second: 0 });
        // 10:09: the hour hand 9 minutes past 10 (304.5°).
        assert_eq!(hands(10, 9, 0), Hands { hour: 609, minute: 108, second: 0 });
        // Seconds: a step of the minute hand every five; 6° each.
        assert_eq!(hands(0, 0, 15), Hands { hour: 0, minute: 3, second: 180 });
        assert_eq!(hands(23, 59, 59), Hands { hour: 719, minute: 719, second: 708 });
        for t in [hands(23, 59, 59), hands(11, 59, 59)] {
            assert!(t.hour < STEPS && t.minute < STEPS && t.second < STEPS);
        }
    }
}
