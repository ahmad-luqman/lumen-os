//! The CMOS registers (the RTC's among them): index at port 0x70, data at
//! 0x71. The RTC's logic is in `crate::rtc`; this is only the port access.
//!
//! An index write and its data read are a pair, so every access goes
//! through [`lock`]'s guard. Two side effects to know: the index port's bit
//! 7 masks NMIs, and every write here leaves it clear (NMIs enabled; nothing
//! in Lumen masks NMIs there); and reading status register C (0x0c)
//! acknowledges the RTC's pending interrupt flags (nothing uses RTC
//! interrupts yet).

use super::port::{inb, outb};
use crate::sync::{SpinLock, SpinLockGuard};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;

static LOCK: SpinLock<()> = SpinLock::new(());

/// Access to the CMOS registers, held for a run of reads (interrupts are
/// off while it is held: keep it short).
pub struct Cmos {
    _guard: SpinLockGuard<'static, ()>,
}

/// Take the CMOS registers (waits for another CPU's access to finish).
pub fn lock() -> Cmos {
    Cmos { _guard: LOCK.lock() }
}

impl Cmos {
    /// CMOS register `reg` (0-0x7f), leaving NMIs enabled.
    pub fn read(&self, reg: u8) -> u8 {
        // SAFETY: the guard serializes index/data pairs; selecting a
        // register and reading it changes nothing but the index and the NMI
        // mask bit (left clear), except for register C, whose read
        // acknowledges RTC interrupts nothing uses.
        unsafe {
            outb(INDEX, reg & 0x7f);
            inb(DATA)
        }
    }
}
