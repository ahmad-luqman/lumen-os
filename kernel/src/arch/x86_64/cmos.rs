//! The CMOS registers (the RTC's among them): index at port 0x70, data at
//! 0x71. The RTC's logic is in `crate::rtc`; this is only the port access.

use super::port::{inb, outb};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;

/// CMOS register `reg` (0-0x7f), with NMIs left enabled (index bit 7
/// clear). The index write and the data read are a pair: callers serialize
/// them (`crate::rtc` holds a lock).
pub fn read(reg: u8) -> u8 {
    // SAFETY: selecting a CMOS register and reading it changes nothing but
    // the index, which every read sets first.
    unsafe {
        outb(INDEX, reg & 0x7f);
        inb(DATA)
    }
}
