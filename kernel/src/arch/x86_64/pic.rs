//! The legacy 8259 PICs: remapped off the exception vectors, then masked.
//! The local and I/O APICs take over; only a spurious IRQ 7 or 15 can still
//! arrive (see `idt`'s vector map).

use super::idt::PIC_BASE;
use super::port::{inb, outb};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const EOI: u8 = 0x20;

/// Remap both PICs to `PIC_BASE..PIC_BASE + 16` and mask every line. Firmware
/// leaves them at vectors 8-15 (on top of the CPU exceptions), so a spurious
/// IRQ would otherwise look like a double fault.
pub fn remap_and_mask() {
    let wait = || {
        // A write to an unused port gives an old PIC time to settle.
        // SAFETY: port 0x80 is the POST diagnostic port; writes are harmless.
        unsafe { outb(0x80, 0) };
    };
    // SAFETY: the standard 8259 initialization sequence (ICW1-4), then
    // OCW1 masking everything; nothing else drives the PICs.
    unsafe {
        outb(MASTER_CMD, 0x11); // ICW1: initialize, ICW4 follows
        wait();
        outb(SLAVE_CMD, 0x11);
        wait();
        outb(MASTER_DATA, PIC_BASE); // ICW2: vector offset
        wait();
        outb(SLAVE_DATA, PIC_BASE + 8);
        wait();
        outb(MASTER_DATA, 0x04); // ICW3: slave on IRQ 2
        wait();
        outb(SLAVE_DATA, 0x02); // ICW3: slave identity
        wait();
        outb(MASTER_DATA, 0x01); // ICW4: 8086 mode
        wait();
        outb(SLAVE_DATA, 0x01);
        wait();
        outb(MASTER_DATA, 0xff); // OCW1: mask all
        outb(SLAVE_DATA, 0xff);
    }
}

/// The masks both PICs hold (master in the low byte).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub fn masks() -> u16 {
    // SAFETY: reading the PICs' mask registers has no side effects.
    unsafe { u16::from(inb(MASTER_DATA)) | u16::from(inb(SLAVE_DATA)) << 8 }
}

/// Acknowledge a spurious IRQ 15 at the master (the slave raised it on the
/// master's line 2, which the master counts as a real interrupt).
pub fn eoi_master() {
    // SAFETY: a non-specific EOI to the master only ends its current IRQ.
    unsafe { outb(MASTER_CMD, EOI) };
}
