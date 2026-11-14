//! The CMOS real-time clock's registers as a date and time.
//!
//! The RTC (Motorola MC146818 and its successors in every PC chipset) keeps
//! seconds, minutes, hours, day, month and a two-digit year in registers,
//! in BCD or binary and with hours in 12- or 24-hour form, as its status
//! register B says. While it updates them (once a second) status register
//! A's update-in-progress bit is set and the registers can read half
//! updated: [`wait_ready`] waits (bounded) for the bit to clear, and
//! refuses a clock whose oscillator is off (or that isn't there), and
//! [`read_stable`] reads until two reads in a row agree. [`decode`] also
//! refuses a clock whose battery failed (status D) or whose updates are
//! halted (status B's SET bit).

use core::fmt;

/// Status register A: an update is in progress.
pub const STATUS_A_UIP: u8 = 1 << 7;
/// Status register A: the divider bits (6-4).
pub const STATUS_A_DIVIDER: u8 = 0b111 << 4;
/// The divider for a running 32.768 kHz time base (anything else stops or
/// resets the clock: 0 among them, as an absent clock reads).
pub const STATUS_A_DIVIDER_RUNNING: u8 = 0b010 << 4;
/// Status register B: updates are halted (the clock is being set).
pub const STATUS_B_SET: u8 = 1 << 7;
/// Status register B: hours are 24-hour (else 12-hour, bit 7 of the hour
/// meaning PM).
pub const STATUS_B_24H: u8 = 1 << 1;
/// Status register B: the registers hold binary (else BCD).
pub const STATUS_B_BINARY: u8 = 1 << 2;
/// Status register D: the time is valid (clear after the battery failed).
pub const STATUS_D_VRT: u8 = 1 << 7;
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
    pub status_d: u8,
}

/// A date and time, as the RTC keeps it. Which time zone that is, the
/// clock doesn't say: whoever set it decided (QEMU's `-rtc base=utc`, the
/// tests' setting, keeps UTC; Lumen never writes the clock).
/// Only [`DateTime::new`] makes one, so it is always a valid date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

/// Why registers aren't a date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A BCD register with a nibble over 9.
    NotBcd(u8),
    /// A field out of its range (a 13th month, a 31st of April, 24 hours,
    /// a 12-hour clock showing 0 or 13, a century past 99).
    OutOfRange,
    /// Status D says the time is invalid: the clock lost power.
    LostPower,
    /// Status B's SET bit: updates are halted, the time stands still.
    Halted,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NotBcd(b) => write!(f, "{b:#04x} isn't BCD"),
            DecodeError::OutOfRange => f.write_str("a field is out of range"),
            DecodeError::LostPower => f.write_str("the clock lost power (its time is invalid)"),
            DecodeError::Halted => f.write_str("the clock's updates are halted"),
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

/// The date and time `r` holds, in the format its status register B gives;
/// an error for a clock that lost power or is halted, or registers that
/// aren't a date.
pub fn decode(r: &Registers) -> Result<DateTime, DecodeError> {
    if r.status_d & STATUS_D_VRT == 0 {
        return Err(DecodeError::LostPower);
    }
    if r.status_b & STATUS_B_SET != 0 {
        return Err(DecodeError::Halted);
    }
    let binary = r.status_b & STATUS_B_BINARY != 0;
    let value = |b: u8| if binary { Ok(b) } else { from_bcd(b) };
    let hour = if r.status_b & STATUS_B_24H != 0 {
        value(r.hours)?
    } else {
        // 12-hour: 12 AM is midnight, 12 PM noon.
        let pm = r.hours & PM != 0;
        match (value(r.hours & !PM)?, pm) {
            (h @ 1..=11, false) => h,
            (12, false) => 0,
            (h @ 1..=11, true) => h + 12,
            (12, true) => 12,
            _ => return Err(DecodeError::OutOfRange),
        }
    };
    let century = match r.century {
        Some(c) => value(c)?,
        None => 20,
    };
    let year_in_century = value(r.year)?;
    if century > 99 || year_in_century > 99 {
        return Err(DecodeError::OutOfRange);
    }
    DateTime::new(
        u16::from(century) * 100 + u16::from(year_in_century),
        value(r.month)?,
        value(r.day)?,
        hour,
        value(r.minutes)?,
        value(r.seconds)?,
    )
    .ok_or(DecodeError::OutOfRange)
}

/// Why [`wait_ready`] gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotReady {
    /// Status A read 0xff: nothing answers at the clock's ports.
    Absent,
    /// Status A's divider isn't the running one: the oscillator is off (or
    /// held in reset), so the time doesn't advance.
    Stopped,
    /// The update-in-progress bit didn't clear in time.
    Busy,
}

/// Wait until status A (read by `status_a`) shows a running clock with no
/// update in progress, for at most `timeout_ns` by the clock `now_ns`.
pub fn wait_ready(
    mut status_a: impl FnMut() -> u8,
    mut now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
) -> Result<(), NotReady> {
    let deadline = now_ns().saturating_add(timeout_ns);
    loop {
        match status_a() {
            0xff => return Err(NotReady::Absent),
            a if a & STATUS_A_DIVIDER != STATUS_A_DIVIDER_RUNNING => return Err(NotReady::Stopped),
            a if a & STATUS_A_UIP == 0 => return Ok(()),
            _ => {}
        }
        if now_ns() > deadline {
            return Err(NotReady::Busy);
        }
        core::hint::spin_loop();
    }
}

/// Why [`read_stable`] has no reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsteady<E> {
    /// The wait before a read failed.
    NotReady(E),
    /// No two reads in a row agreed.
    NeverAgreed,
}

/// Read the registers until two reads in a row agree, each after
/// `wait_ready` (which waits for the update-in-progress bit to clear); at
/// most `tries` reads.
pub fn read_stable<R: PartialEq, E>(
    mut wait_ready: impl FnMut() -> Result<(), E>,
    mut read: impl FnMut() -> R,
    tries: usize,
) -> Result<R, Unsteady<E>> {
    let mut last = None;
    for _ in 0..tries {
        wait_ready().map_err(Unsteady::NotReady)?;
        let now = read();
        if last.as_ref() == Some(&now) {
            return Ok(now);
        }
        last = Some(now);
    }
    Err(Unsteady::NeverAgreed)
}

impl DateTime {
    /// A date and time, if it is one (month 1-12, a day of that month,
    /// 24-hour hours, minutes and seconds under 60).
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Option<DateTime> {
        let valid = (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60;
        valid.then_some(DateTime { year, month, day, hour, minute, second })
    }

    pub fn year(&self) -> u16 {
        self.year
    }

    pub fn month(&self) -> u8 {
        self.month
    }

    pub fn day(&self) -> u8 {
        self.day
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }

    pub fn minute(&self) -> u8 {
        self.minute
    }

    pub fn second(&self) -> u8 {
        self.second
    }

    /// Seconds since 1970-01-01 00:00:00; `None` before 1970.
    pub fn unix_seconds(&self) -> Option<u64> {
        let d = self;
        if d.year < 1970 {
            return None;
        }
        let days_before_year: u64 = (1970..d.year).map(|y| if is_leap(y) { 366 } else { 365 }).sum();
        let days_before_month: u64 = (1..d.month).map(|m| u64::from(days_in_month(d.year, m))).sum();
        let days = days_before_year + days_before_month + u64::from(d.day) - 1;
        Some(days * 86_400 + u64::from(d.hour) * 3600 + u64::from(d.minute) * 60 + u64::from(d.second))
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
    use std::cell::Cell;
    use std::string::ToString;

    const VALID: u8 = STATUS_D_VRT;

    fn regs(status_b: u8, [seconds, minutes, hours, day, month, year]: [u8; 6]) -> Registers {
        Registers { seconds, minutes, hours, day, month, year, century: None, status_b, status_d: VALID }
    }

    fn dt(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
        DateTime::new(year, month, day, hour, minute, second).unwrap()
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
        let cases = [
            (0x12, false, 0),
            (0x01, false, 1),
            (0x11, false, 11),
            (0x12, true, 12),
            (0x01, true, 13),
            (0x11, true, 23),
        ];
        for (bcd, pm, want) in cases {
            let flag = if pm { 0x80 } else { 0 };
            let r = regs(0, [0, 0, bcd | flag, 1, 1, 0]);
            assert_eq!(decode(&r).map(|d| d.hour()), Ok(want), "bcd {bcd:#x} pm {pm}");
            let bin = from_bcd(bcd).unwrap();
            let r = regs(STATUS_B_BINARY, [0, 0, bin | flag, 1, 1, 0]);
            assert_eq!(decode(&r).map(|d| d.hour()), Ok(want), "binary {bin} pm {pm}");
        }
        // 0 and 13 aren't 12-hour hours.
        assert_eq!(decode(&regs(0, [0, 0, 0x00, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(0, [0, 0, 0x13, 1, 1, 0])), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn century_register() {
        let mut r = regs(STATUS_B_24H, [0, 0, 0, 1, 1, 0x99]);
        r.century = Some(0x19);
        assert_eq!(decode(&r).map(|d| d.year()), Ok(1999));
        r.century = None;
        assert_eq!(decode(&r).map(|d| d.year()), Ok(2099));
        // Binary: the century is binary too; past 99 isn't a century.
        let mut r = regs(STATUS_B_24H | STATUS_B_BINARY, [0, 0, 0, 1, 1, 5]);
        r.century = Some(21);
        assert_eq!(decode(&r).map(|d| d.year()), Ok(2105));
        r.century = Some(100);
        assert_eq!(decode(&r), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn bad_values_are_refused() {
        let h = STATUS_B_24H;
        assert_eq!(decode(&regs(h, [0x5a, 0, 0, 1, 1, 0])), Err(DecodeError::NotBcd(0x5a)));
        assert_eq!(decode(&regs(h, [0xa0, 0, 0, 1, 1, 0])), Err(DecodeError::NotBcd(0xa0)));
        assert_eq!(decode(&regs(h, [0x60, 0, 0, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0x60, 0, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0x24, 1, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x31, 0x04, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x31, 0x11, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x29, 0x02, 0x25])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0x29, 0x02, 0x24])).map(|d| d.day()), Ok(29));
        assert_eq!(decode(&regs(h, [0, 0, 0, 1, 0x13, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 1, 0x00, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h, [0, 0, 0, 0, 1, 0])), Err(DecodeError::OutOfRange));
        assert_eq!(decode(&regs(h | STATUS_B_BINARY, [0, 0, 0, 1, 1, 100])), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn a_clock_that_lost_power_or_is_halted_is_refused() {
        let mut r = regs(STATUS_B_24H, [0, 0, 0, 1, 1, 0]);
        assert!(decode(&r).is_ok());
        r.status_d = 0;
        assert_eq!(decode(&r), Err(DecodeError::LostPower));
        r.status_d = VALID;
        r.status_b |= STATUS_B_SET;
        assert_eq!(decode(&r), Err(DecodeError::Halted));
    }

    #[test]
    fn leap_years() {
        assert!(is_leap(2024) && is_leap(2000) && !is_leap(1900) && !is_leap(2026));
    }

    #[test]
    fn unix_seconds_match_known_dates() {
        assert_eq!(dt(1970, 1, 1, 0, 0, 0).unix_seconds(), Some(0));
        assert_eq!(dt(2000, 3, 1, 0, 0, 0).unix_seconds(), Some(951_868_800));
        assert_eq!(dt(2026, 11, 14, 9, 5, 7).unix_seconds(), Some(1_794_647_107));
        assert_eq!(dt(1969, 12, 31, 23, 59, 59).unix_seconds(), None);
        assert_eq!(DateTime::new(2026, 2, 30, 0, 0, 0), None);
        assert_eq!(DateTime::new(2026, 1, 1, 24, 0, 0), None);
    }

    #[test]
    fn wait_ready_waits_for_the_update_to_end() {
        // The clock advances 1 ns per call; UIP clears after 3 reads (a
        // running divider throughout).
        let t = Cell::new(0u64);
        let now = || {
            t.set(t.get() + 1);
            t.get()
        };
        let reads = Cell::new(0);
        let status = || {
            reads.set(reads.get() + 1);
            if reads.get() > 3 { 0x26 } else { 0x26 | STATUS_A_UIP }
        };
        assert_eq!(wait_ready(status, now, 100), Ok(()));
        assert_eq!(reads.get(), 4);
        // Never clearing: gives up once the deadline passed.
        let t = Cell::new(0u64);
        let reads = Cell::new(0);
        let busy = || {
            reads.set(reads.get() + 1);
            0x26 | STATUS_A_UIP
        };
        let now = || {
            t.set(t.get() + 1);
            t.get()
        };
        assert_eq!(wait_ready(busy, now, 10), Err(NotReady::Busy));
        assert!((5..=12).contains(&reads.get()), "{} reads", reads.get());
        // No clock at the ports, or a stopped oscillator (an all-zero
        // status among them): at once, with or without UIP (the clock
        // advances, so a mutant that waits instead times out, not hangs).
        let t = Cell::new(0u64);
        let now = || {
            t.set(t.get() + 1);
            t.get()
        };
        assert_eq!(wait_ready(|| 0xff, now, 10), Err(NotReady::Absent));
        for a in [0x00, 0x06, 0x66, 0x76 | STATUS_A_UIP, 0x06 | STATUS_A_UIP] {
            assert_eq!(wait_ready(|| a, now, 10), Err(NotReady::Stopped), "status A {a:#x}");
        }
    }

    #[test]
    fn read_stable_waits_for_two_agreeing_reads() {
        // The first read disagrees with the next (an update came between);
        // the second and third agree.
        let mut reads = [1, 2, 2].into_iter();
        let mut waits = 0;
        let got = read_stable::<_, ()>(
            || {
                waits += 1;
                Ok(())
            },
            || reads.next().unwrap(),
            5,
        );
        assert_eq!((got, waits), (Ok(2), 3));
        // Never agreeing: exactly `tries` reads, then nothing.
        let mut n = 0;
        let got = read_stable::<_, ()>(
            || Ok(()),
            || {
                n += 1;
                n
            },
            4,
        );
        assert_eq!((got, n), (Err(Unsteady::NeverAgreed), 4));
        // Never ready: no read, and why.
        let mut read = 0;
        assert_eq!(read_stable(|| Err(NotReady::Busy), || read += 1, 4), Err(Unsteady::NotReady(NotReady::Busy)));
        assert_eq!(read, 0);
    }
}
