//! The CMOS real-time clock: the date and time (UTC) for the shell's `date`.
//!
//! The RTC's registers are read through the CMOS index and data ports
//! (`arch::cmos`), under a lock (an index write and its data read must not
//! be split by another CPU's). [`now`] waits for an update in progress to end
//! and reads until two reads agree; decoding (BCD or binary, 12 or 24 hours)
//! is `lumen_time::rtc`'s, host-tested. The century is taken as 20 (the
//! FADT's century register isn't read yet).

use crate::arch::cmos;
use crate::sync::SpinLock;
use core::fmt;
use lumen_time::rtc::{self, DateTime, DecodeError, Registers};

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;
/// How long an update may keep the registers busy: the update cycle takes
/// under 2 ms, and the flag is raised 244 µs before it.
const UPDATE_WAIT_NS: u64 = 10_000_000;
/// Reads before giving up on two agreeing (two suffice unless an update
/// lands between them).
const TRIES: usize = 5;

/// Serializes CMOS index/data pairs.
static CMOS: SpinLock<()> = SpinLock::new(());

/// Why [`now`] has no date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcError {
    /// An update never ended, or no two reads agreed.
    Unsteady,
    /// The registers aren't a date.
    Decode(DecodeError),
}

impl fmt::Display for RtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtcError::Unsteady => f.write_str("the clock kept updating while read"),
            RtcError::Decode(e) => write!(f, "the clock's registers aren't a date: {e}"),
        }
    }
}

/// The date and time the RTC holds. The lock is taken for each status
/// read and each set of registers, not across the wait (it turns interrupts
/// off).
pub fn now() -> Result<DateTime, RtcError> {
    let wait_ready = || {
        let deadline = crate::time::now_ns() + UPDATE_WAIT_NS;
        loop {
            let status_a = {
                let _cmos = CMOS.lock();
                cmos::read(STATUS_A)
            };
            if status_a & rtc::STATUS_A_UIP == 0 {
                return true;
            }
            if crate::time::now_ns() > deadline {
                return false;
            }
            core::hint::spin_loop();
        }
    };
    let registers = || {
        let _cmos = CMOS.lock();
        Registers {
            seconds: cmos::read(SECONDS),
            minutes: cmos::read(MINUTES),
            hours: cmos::read(HOURS),
            day: cmos::read(DAY),
            month: cmos::read(MONTH),
            year: cmos::read(YEAR),
            century: None,
            status_b: cmos::read(STATUS_B),
        }
    };
    let r = rtc::read_stable(wait_ready, registers, TRIES).ok_or(RtcError::Unsteady)?;
    rtc::decode(&r).map_err(RtcError::Decode)
}

crate::ktest!(
    fn the_rtc_reads_a_plausible_date() {
        // QEMU's RTC follows the host's clock: a date from this century,
        // and a second read no earlier than the first.
        use crate::kassert;
        let first = now().map_err(|_| "the RTC didn't read")?;
        let second = now().map_err(|_| "the RTC didn't read twice")?;
        crate::ktest::diag(format_args!("rtc: {first}"));
        kassert!(first.year >= 2024 && first.year < 2100);
        kassert!(second >= first);
        Ok(())
    }
);
