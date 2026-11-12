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

/// Run channel 2 for `count` ticks of its 1.193182 MHz clock, calling
/// `started` just as it starts and returning once it has finished. Busy-waits
/// (up to about 55 ms with a working PIT); call with interrupts off so the
/// window isn't stretched. Returns false if there is no working PIT: its
/// output didn't go low when programmed (a missing PIT's ports read all
/// ones), or didn't go high again within [`BOUND_TSC`] TSC ticks.
pub fn wait(count: u16, started: impl FnOnce()) -> bool {
    // SAFETY: programs channel 2 only (mode 0, one-shot) with the speaker
    // disconnected; channel 0 and the speaker are left alone otherwise.
    unsafe {
        let ctl = inb(SPEAKER) & !(GATE | SPEAKER_ON);
        outb(SPEAKER, ctl); // gate low: hold the count
        outb(COMMAND, 0b1011_0000); // channel 2, lobyte/hibyte, mode 0, binary
        outb(CHANNEL2, count as u8);
        outb(CHANNEL2, (count >> 8) as u8);
        // Mode 0 drives the output low as soon as it is programmed.
        let finished = inb(SPEAKER) & OUT == 0 && {
            outb(SPEAKER, ctl | GATE); // gate high: count down
            started();
            let t0 = rdtsc();
            loop {
                if inb(SPEAKER) & OUT != 0 {
                    break true;
                }
                if rdtsc().wrapping_sub(t0) > BOUND_TSC {
                    break false;
                }
            }
        };
        outb(SPEAKER, ctl);
        finished
    }
}
