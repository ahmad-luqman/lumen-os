//! The legacy 8259 PICs: remapped off the exception vectors, then masked.
//! The local and I/O APICs take over; only a spurious IRQ 7 or 15 should
//! still arrive (see `idt`'s vector map).

use super::idt::PIC_BASE;
use super::port::{inb, outb};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const EOI: u8 = 0x20;

/// Remap both PICs to `PIC_BASE..PIC_BASE + 16` and mask every line. The
/// BIOS default puts the master on vectors 8-15, over the CPU exceptions
/// (IRQ 0 would arrive as a #DF), and what UEFI firmware leaves isn't
/// specified, so remap to a known range before anything can arrive.
pub fn remap_and_mask() {
    let wait = || {
        // A write to an unused port gives an old PIC time to settle.
        // SAFETY: port 0x80 is the POST diagnostic port; writes are harmless.
        unsafe { outb(0x80, 0) };
    };
    // SAFETY: the standard 8259 initialization sequence (ICW1-4), then
    // nothing else drives the PICs (`mask_all` masks every line next).
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
    }
    mask_all();
}

/// Mask every line of both PICs (OCW1).
pub fn mask_all() {
    // SAFETY: writing the mask registers only masks the PICs' lines.
    unsafe {
        outb(MASTER_DATA, 0xff);
        outb(SLAVE_DATA, 0xff);
    }
}

/// The masks both PICs hold (master in the low byte).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub fn masks() -> u16 {
    // SAFETY: reading the PICs' mask registers has no side effects.
    unsafe { u16::from(inb(MASTER_DATA)) | u16::from(inb(SLAVE_DATA)) << 8 }
}

/// Send the master a non-specific EOI: for a spurious IRQ 15 (the slave
/// raised it on the master's line 2, which the master counts as real), and
/// as the last step of [`eoi`].
pub fn eoi_master() {
    // SAFETY: a non-specific EOI to the master only ends its current IRQ.
    unsafe { outb(MASTER_CMD, EOI) };
}

/// End a real (not spurious) IRQ `irq` (0-15) at the PICs that saw it.
pub fn eoi(irq: u8) {
    if irq >= 8 {
        // SAFETY: as for `eoi_master`, at the slave.
        unsafe { outb(SLAVE_CMD, EOI) };
    }
    eoi_master();
}
