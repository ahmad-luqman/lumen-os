//! The PIT's channel 2, polled: a counted window for calibrating the TSC
//! without an HPET, and for cross-checking it. (Channel 2 drives the PC speaker; its gate and
//! output are in port 0x61, so it needs no interrupt.)

use super::cpu::rdtsc;
use super::port::{inb, outb};
use lumen_time::{Pause, PitReading, PitWait};

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

/// Latch channel 2's counter between two TSC reads, then read the count the
/// latch froze (however long that takes doesn't matter, so it is outside the
/// bracket: under KVM each port access is a VM exit). `pause` is called with
/// `before` after the first TSC read and with `after` after the latch, before
/// the second.
///
/// # Safety
/// Channel 2 must be programmed for lobyte/hibyte access.
unsafe fn read(pause: &mut impl FnMut(Pause), before: Pause, after: Pause) -> PitReading {
    let tsc_before = rdtsc();
    pause(before);
    // SAFETY: the latch command for channel 2; nothing else is touched.
    unsafe { outb(COMMAND, 0b1000_0000) };
    pause(after);
    let tsc_after = rdtsc();
    pause(Pause::CountRead);
    // SAFETY: two reads of the latched count, as the caller programmed it.
    let count = unsafe {
        let lo = inb(CHANNEL2);
        u16::from_le_bytes([lo, inb(CHANNEL2)])
    };
    PitReading { count, tsc_before, tsc_after }
}

/// Run channel 2 for `count` ticks of its 1.193182 MHz clock and return once
/// it has run out, with its counter read at both ends ([`PitWait`]): a late
/// look at the end doesn't stretch the window, since the counter says how far
/// past the end it was. `pause` is called at each [`Pause`] point (a no-op
/// but in tests). Busy-waits (up to about 55 ms with a working PIT); call
/// with interrupts off. `None` if there is no working PIT: its output didn't
/// go low when programmed (a missing PIT's ports read all ones), or didn't go
/// high again within [`BOUND_TSC`] TSC ticks.
pub fn wait(count: u16, mut pause: impl FnMut(Pause)) -> Option<PitWait> {
    // SAFETY: programs channel 2 only (mode 0, one-shot, lobyte/hibyte) and
    // reads it back (`read` needs that lobyte/hibyte access); channel 0 is
    // left alone, and the speaker stays disconnected.
    unsafe {
        let ctl = inb(SPEAKER) & !(GATE | SPEAKER_ON);
        // Gate low. A real 8254 holds the count until the gate goes high;
        // QEMU's counts on in mode 0 (measured under TCG, #45; KVM's wasn't
        // measured), so the start is read, not assumed.
        outb(SPEAKER, ctl);
        outb(COMMAND, 0b1011_0000); // channel 2, lobyte/hibyte, mode 0, binary
        outb(CHANNEL2, count as u8);
        outb(CHANNEL2, (count >> 8) as u8);
        // Mode 0 drives the output low as soon as it is programmed.
        let waited = if inb(SPEAKER) & OUT == 0 {
            outb(SPEAKER, ctl | GATE); // gate high: count down
            pause(Pause::Started);
            let start = read(&mut pause, Pause::StartBeforeRead, Pause::StartAfterRead);
            let (mut polls, mut last_running_tsc) = (0, start.tsc_after);
            loop {
                polls += 1;
                let t = rdtsc();
                if inb(SPEAKER) & OUT != 0 {
                    pause(Pause::Ended);
                    let end = read(&mut pause, Pause::EndBeforeRead, Pause::EndAfterRead);
                    break Some(PitWait { start, end, last_running_tsc, polls });
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
