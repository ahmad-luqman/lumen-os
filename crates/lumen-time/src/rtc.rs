//! The CMOS real-time clock's registers as a date and time.
//!
//! The RTC (Motorola MC146818 and its successors in every PC chipset) keeps
//! seconds, minutes, hours, day, month and a two-digit year in registers,
//! in BCD or binary and with hours in 12- or 24-hour form, as its status
//! register B says. While it updates them (once a second) status register
//! A's update-in-progress bit is set and the registers can read half
//! updated: [`read_stable`] waits for the bit to clear and reads until two
//! reads in a row agree.

use core::fmt;

/// Status register B: hours are 24-hour (else 12-hour, bit 7 of the hour
/// meaning PM).
pub const STATUS_B_24H: u8 = 1 << 1;
/// Status register B: the registers hold binary (else BCD).
pub const STATUS_B_BINARY: u8 = 1 << 2;
/// Status register A: an update is in progress.
pub const STATUS_A_UIP: u8 = 1 << 7;
/// In 12-hour form, the hour register's PM flag.
const PM: u8 = 1 << 7;

/// The raw registers, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registers {
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub day: u8,
    pub month: u8,
    /// The year within its century (two digits).
    pub year: u8,
    /// The century register, if the firmware has one (ACPI's FADT names
    /// it; none means 20xx).
    pub century: Option<u8>,
    pub status_b: u8,
}

/// A date and time (the RTC's own; Lumen keeps it in UTC: QEMU's
/// `-rtc base=utc`, and most PC firmware outside Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Why registers aren't a date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A BCD register with a nibble over 9.
    NotBcd(u8),
    /// A field out of its range (a 13th month, a 31st of April, 24 hours,
    /// a 12-hour clock showing 0 or 13).
    OutOfRange,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NotBcd(b) => write!(f, "{b:#04x} isn't BCD"),
            DecodeError::OutOfRange => f.write_str("a field is out of range"),
        }
    }
}

/// A BCD byte's value.
fn from_bcd(b: u8) -> Result<u8, DecodeError> {
    let (hi, lo) = (b >> 4, b & 0xf);
    if hi > 9 || lo > 9 {
        return Err(DecodeError::NotBcd(b));
    }
    Ok(hi * 10 + lo)
}

/// Whether `year` is a leap year (Gregorian).
pub fn is_leap(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// The days in `month` (1-12) of `year`.
pub fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The date and time `r` holds, in the format its status register B gives.
pub fn decode(r: &Registers) -> Result<DateTime, DecodeError> {
    let binary = r.status_b & STATUS_B_BINARY != 0;
    let value = |b: u8| if binary { Ok(b) } else { from_bcd(b) };
    let pm = r.status_b & STATUS_B_24H == 0 && r.hours & PM != 0;
    let mut hour = value(if r.status_b & STATUS_B_24H == 0 { r.hours & !PM } else { r.hours })?;
    if r.status_b & STATUS_B_24H == 0 {
        // 12-hour: 12 AM is midnight, 12 PM noon.
        if !(1..=12).contains(&hour) {
            return Err(DecodeError::OutOfRange);
        }
        hour = match (hour, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (h, false) => h,
            (h, true) => h + 12,
        };
    }
    let century = match r.century {
        Some(c) => value(c)?,
        None => 20,
    };
    let year = u16::from(century) * 100 + u16::from(value(r.year)?);
    let dt = DateTime {
        year,
        month: value(r.month)?,
        day: value(r.day)?,
        hour,
        minute: value(r.minutes)?,
        second: value(r.seconds)?,
    };
    let valid = (1..=12).contains(&dt.month)
        && (1..=days_in_month(dt.year, dt.month)).contains(&dt.day)
        && dt.hour < 24
        && dt.minute < 60
        && dt.second < 60
        && value(r.year)? < 100;
    if valid { Ok(dt) } else { Err(DecodeError::OutOfRange) }
}

/// Read the registers until two reads in a row agree, each after
/// `wait_ready` (which waits for the update-in-progress bit to clear and
/// says whether it did, in time); at most `tries` reads. `None` if
/// `wait_ready` gave up or no two reads agreed.
pub fn read_stable<R: PartialEq>(
    mut wait_ready: impl FnMut() -> bool,
    mut read: impl FnMut() -> R,
    tries: usize,
) -> Option<R> {
    let mut last = None;
    for _ in 0..tries {
        if !wait_ready() {
            return None;
        }
        let now = read();
        if last.as_ref() == Some(&now) {
            return Some(now);
        }
        last = Some(now);
    }
    None
}

impl DateTime {
    /// Seconds since 1970-01-01 00:00:00 (for years from 1970).
    pub fn unix_seconds(&self) -> u64 {
        let days_before_year: u64 = (1970..self.year).map(|y| if is_leap(y) { 366 } else { 365 }).sum();
        let days_before_month: u64 = (1..self.month).map(|m| u64::from(days_in_month(self.year, m))).sum();
        let days = days_before_year + days_before_month + u64::from(self.day) - 1;
        days * 86_400 + u64::from(self.hour) * 3600 + u64::from(self.minute) * 60 + u64::from(self.second)
    }
}

/// `2026-11-14 09:05:03`.
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::ToString;
    use std::vec::Vec;

    fn regs(status_b: u8, [seconds, minutes, hours, day, month, year]: [u8; 6]) -> Registers {
        Registers { seconds, minutes, hours, day, month, year, century: None, status_b }
    }

    fn dt(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
        DateTime { year, month, day, hour, minute, second }
    }

    #[test]
    fn bcd_24_hour() {
        let r = regs(STATUS_B_24H, [0x59, 0x34, 0x23, 0x31, 0x12, 0x26]);
        assert_eq!(decode(&r), Ok(dt(2026, 12, 31, 23, 34, 59)));
    }

    #[test]
    fn binary_24_hour() {
        let r = regs(STATUS_B_24H | STATUS_B_BINARY, [7, 5, 9, 14, 11, 26]);
        assert_eq!(decode(&r), Ok(dt(2026, 11, 14, 9, 5, 7)));
        assert_eq!(decode(&r).unwrap().to_string(), "2026-11-14 09:05:07");
    }

    #[test]
    fn twelve_hour_in_bcd_and_binary() {
        // (register, PM flag) -> 24-hour hour, both encodings.
        let cases = [(0x12, false, 0), (0x01, false, 1), (0x11, false, 11), (0x12, true, 12), (0x01, true, 13)];
        for (bcd, pm, want) in cases {
            let flag = if pm { 0x80 } else { 0 };
            let r = regs(0, [0, 0, bcd | flag, 1, 1, 0]);
            assert_eq!(decode(&r).map(|d| d.hour), Ok(want), "bcd {bcd:#x} pm {pm}");
            let bin = from_bcd(bcd).unwrap();
            let r = regs(STATUS_B_BINARY, [0, 0, bin | flag, 1, 1, 0]);
            assert_eq!(decode(&r).map(|d| d.hour), Ok(want), "binary {bin} pm {pm}");
        }
        // 0 and 13 aren't 12-hour hours.
        assert_eq!(decode(&regs(0, [0, 0, 0x00, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(0, [0, 0, 0x13, 1, 1, 0])), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn century_register() {
        let mut r = regs(STATUS_B_24H, [0, 0, 0, 1, 1, 0x99]);
        r.century = Some(0x19);
        assert_eq!(decode(&r).map(|d| d.year), Ok(1999));
        r.century = None;
        assert_eq!(decode(&r).map(|d| d.year), Ok(2099));
    }

    #[test]
    fn bad_values_are_refused() {
        let h = STATUS_B_24H;
        assert_eq!(decode(&regs(h, [0x5a, 0, 0, 1, 1, 0])), Err(DecodeError::NotBcd(0x5a)));
        assert_eq!(decode(&regs(h, [0x60, 0, 0, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0x24, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x31, 0x04, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x29, 0x02, 0x25])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x29, 0x02, 0x24])).map(|d| d.day), Ok(29));
        assert_eq!(decode(&regs(h, [0, 0, 0, 1, 0x13, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h | STATUS_B_BINARY, [0, 0, 0, 1, 1, 100])), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn leap_years() {
        assert!(is_leap(2024) && is_leap(2000) && !is_leap(1900) && !is_leap(2026));
    }

    #[test]
    fn unix_seconds_match_known_dates() {
        assert_eq!(dt(1970, 1, 1, 0, 0, 0).unix_seconds(), 0);
        assert_eq!(dt(2000, 3, 1, 0, 0, 0).unix_seconds(), 951_868_800);
        assert_eq!(dt(2026, 11, 14, 9, 5, 7).unix_seconds(), 1_794_647_107);
    }

    #[test]
    fn read_stable_waits_for_two_agreeing_reads() {
        // A read torn by an update (the second) is retried.
        let mut reads = [1, 2, 2].into_iter();
        let mut waits = 0;
        let got = read_stable(
            || {
                waits += 1;
                true
            },
            || reads.next().unwrap(),
            5,
        );
        assert_eq!((got, waits), (Some(2), 3));
        // Never agreeing, or never ready: nothing.
        let mut n = 0;
        assert_eq!(
            read_stable(
                || true,
                || {
                    n += 1;
                    n
                },
                4
            ),
            None
        );
        let mut seen = Vec::new();
        assert_eq!(read_stable(|| false, || seen.push(()), 4), None);
        assert!(seen.is_empty());
    }
}
