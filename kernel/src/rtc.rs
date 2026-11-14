//! The CMOS real-time clock: the date and time for the shell's `date`.
//!
//! The registers are read through `arch::cmos`, whose guard serializes
//! index/data pairs; it is taken for each status read and for each set of
//! registers, not across the wait (it turns interrupts off). [`now`] waits
//! for an update in progress to end (at most 10 ms per read), reads until
//! two reads agree (at most 5 reads), and decodes BCD or binary, 12 or 24
//! hours, refusing a clock that lost power or is halted: all of that is
//! `lumen_time::rtc`'s, host-tested. The century is taken as 20 (the FADT's
//! century register isn't read yet). The clock's time zone is whatever set
//! it (the tests run QEMU with `-rtc base=utc`); Lumen never writes it.

use crate::arch::cmos;
use core::fmt;
use lumen_time::rtc::{self, DateTime, DecodeError, NotReady, Registers, Unsteady};

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;
const STATUS_D: u8 = 0x0d;
/// How long an update may keep the registers busy: the update cycle takes
/// under 2 ms, and the flag is raised 244 µs before it.
const UPDATE_WAIT_NS: u64 = 10_000_000;
/// Reads before giving up on two agreeing (two suffice unless an update
/// lands between them).
const TRIES: usize = 5;

/// Why [`now`] has no date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcError {
    /// Status A read 0xff: no clock answers.
    Absent,
    /// An update never ended (10 ms).
    Busy,
    /// No two reads in a row agreed.
    NeverAgreed,
    /// The registers aren't a valid date (or the clock lost power, or is
    /// halted).
    Decode(DecodeError),
}

impl fmt::Display for RtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtcError::Absent => f.write_str("no clock answers at the CMOS ports"),
            RtcError::Busy => f.write_str("the clock's update didn't end in 10 ms"),
            RtcError::NeverAgreed => write!(f, "no two of {TRIES} reads of the clock agreed"),
            RtcError::Decode(e) => write!(f, "no date: {e}"),
        }
    }
}

/// The date and time the RTC holds. A clock whose registers aren't a date
/// is also logged (a warning, with the registers).
pub fn now() -> Result<DateTime, RtcError> {
    let wait_ready = || rtc::wait_ready(|| cmos::lock().read(STATUS_A), crate::time::now_ns, UPDATE_WAIT_NS);
    let registers = || {
        let c = cmos::lock();
        Registers {
            seconds: c.read(SECONDS),
            minutes: c.read(MINUTES),
            hours: c.read(HOURS),
            day: c.read(DAY),
            month: c.read(MONTH),
            year: c.read(YEAR),
            century: None,
            status_b: c.read(STATUS_B),
            status_d: c.read(STATUS_D),
        }
    };
    let r = rtc::read_stable(wait_ready, registers, TRIES).map_err(|e| match e {
        Unsteady::NotReady(NotReady::Absent) => RtcError::Absent,
        Unsteady::NotReady(NotReady::Busy) => RtcError::Busy,
        Unsteady::NeverAgreed => RtcError::NeverAgreed,
    })?;
    rtc::decode(&r).map_err(|e| {
        log::warn!(target: "rtc", "the clock's registers aren't a date ({e}): {r:x?}");
        RtcError::Decode(e)
    })
}

crate::ktest!(
    fn the_rtc_reads_a_plausible_date() {
        // QEMU's RTC follows the host's clock: a date from this century,
        // and a second read no earlier than the first.
        use crate::kassert;
        let read = || {
            now().map_err(|e| {
                crate::ktest::diag(format_args!("rtc: {e}"));
                "the RTC didn't read"
            })
        };
        let first = read()?;
        let second = read()?;
        crate::ktest::diag(format_args!("rtc: {first}"));
        kassert!(first.year >= 2024 && first.year < 2100);
        kassert!(second >= first);
        Ok(())
    }
);
