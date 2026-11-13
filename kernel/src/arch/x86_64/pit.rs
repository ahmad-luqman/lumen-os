//! The PIT's channel 2, polled: a known-length delay for calibrating the TSC
//! when there is no HPET. (Channel 2 drives the PC speaker; its gate and
//! output are in port 0x61, so it needs no interrupt.)

use super::cpu::rdtsc;
use super::port::{inb, outb};

const CHANNEL2: u16 = 0x42;
const COMMAND: u16 = 0x43;
const SPEAKER: u16 = 0x61;
/// Port 0x61: bit 0 gates channel 2, bit 1 connects it to the speaker, bit
/// 5 reads its output.
const GATE: u8 = 1 << 0;
const SPEAKER_ON: u8 = 1 << 1;
const OUT: u8 = 1 << 5;

/// The longest a wait may take, in TSC ticks (under 5 s at 1 GHz and up),
/// before the PIT counts as missing: the longest count is 55 ms.
const BOUND_TSC: u64 = 1 << 32;

/// A reading of channel 2's counter, latched between two TSC reads.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub count: u16,
    pub tsc_before: u64,
    pub tsc_after: u64,
}

/// How a [`wait`] went: the counter read just after it started and again once
/// it had run out (in mode 0 it counts on past 0, so the second reading says
/// how far past), the TSC just before the last poll that still saw it
/// running, and how many times its output was polled.
#[derive(Clone, Copy, Debug)]
pub struct Waited {
    pub start: Reading,
    pub end: Reading,
    pub last_running_tsc: u64,
    pub polls: u64,
}

/// Where [`wait`] calls its `pause`: calibration passes a no-op; tests pause
/// there, as a host descheduling the vCPU would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    /// Once the count has run out, before the end reading. Harmless: the
    /// counter runs on, and the reading says how far.
    Ended,
    /// In the end reading, after the counter is latched and before the TSC is
    /// read: that end's gap widens by the pause.
    EndReading,
}

/// Latch channel 2's counter and read it, between two TSC reads (`latched`
/// runs in between, just before the second).
///
/// # Safety
/// Channel 2 must be programmed for lobyte/hibyte access.
unsafe fn read(latched: impl FnOnce()) -> Reading {
    let tsc_before = rdtsc();
    // SAFETY: the latch command and two reads of channel 2's count, as the
    // caller programmed it; nothing else is touched.
    let count = unsafe {
        outb(COMMAND, 0b1000_0000); // latch channel 2
        let lo = inb(CHANNEL2);
        u16::from_le_bytes([lo, inb(CHANNEL2)])
    };
    latched();
    Reading { count, tsc_before, tsc_after: rdtsc() }
}

/// Run channel 2 for `count` ticks of its 1.193182 MHz clock and return once
/// it has run out, with its counter read at both ends ([`Waited`]): a late
/// look at the end doesn't stretch the window, since the counter says how far
/// past the end it was. `pause` is called at the points [`Pause`] lists.
/// Busy-waits (up to about 55 ms with a working PIT);
/// call with interrupts off. `None` if there is no working PIT: its output
/// didn't go low when programmed (a missing PIT's ports read all ones), or
/// didn't go high again within [`BOUND_TSC`] TSC ticks.
pub fn wait(count: u16, mut pause: impl FnMut(Pause)) -> Option<Waited> {
    // SAFETY: programs channel 2 only (mode 0, one-shot, lobyte/hibyte) with
    // the speaker disconnected, and reads it back (`read` needs that lobyte/
    // hibyte access); channel 0 and the speaker are left alone.
    unsafe {
        let ctl = inb(SPEAKER) & !(GATE | SPEAKER_ON);
        outb(SPEAKER, ctl); // gate low: hold the count
        outb(COMMAND, 0b1011_0000); // channel 2, lobyte/hibyte, mode 0, binary
        outb(CHANNEL2, count as u8);
        outb(CHANNEL2, (count >> 8) as u8);
        // Mode 0 drives the output low as soon as it is programmed.
        let waited = if inb(SPEAKER) & OUT == 0 {
            outb(SPEAKER, ctl | GATE); // gate high: count down
            let start = read(|| {});
            let (mut polls, mut last_running_tsc) = (0, start.tsc_after);
            loop {
                polls += 1;
                let t = rdtsc();
                if inb(SPEAKER) & OUT != 0 {
                    pause(Pause::Ended);
                    let end = read(|| pause(Pause::EndReading));
                    break Some(Waited { start, end, last_running_tsc, polls });
                }
                last_running_tsc = t;
                if t.wrapping_sub(start.tsc_after) > BOUND_TSC {
                    break None;
                }
            }
        } else {
            None
        };
        outb(SPEAKER, ctl);
        waited
    }
}
