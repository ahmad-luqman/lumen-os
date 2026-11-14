//! Resetting the machine and powering it off, at the register level. The
//! order to try them in, and the waits between, are `crate::power`'s.

use super::i8042;
use super::port::{inb, inw, outb, outw};
use core::arch::asm;

/// The reset control register (Intel chipsets, and QEMU's q35 and PIIX).
const RESET_CONTROL: u16 = 0xcf9;
/// Its bits: system reset (with `CPU_RESET`, a full platform reset rather
/// than an INIT), the reset itself (taken on a 0 to 1 change), and a full
/// reset (power cycled).
const SYSTEM_RESET: u8 = 1 << 1;
const CPU_RESET: u8 = 1 << 2;
const FULL_RESET: u8 = 1 << 3;

/// Reset through port 0xCF9: a full reset. Returns if nothing happened.
pub fn reset_control() {
    // SAFETY: 0xCF9 is the reset control register; writing it can only
    // reset the machine, which is the point. (A machine without it ignores
    // the write.)
    unsafe {
        let keep = inb(RESET_CONTROL) & !(SYSTEM_RESET | CPU_RESET | FULL_RESET);
        outb(RESET_CONTROL, keep | SYSTEM_RESET | FULL_RESET);
        outb(RESET_CONTROL, keep | SYSTEM_RESET | CPU_RESET | FULL_RESET);
    }
}

/// The 8042's command to pulse the CPU's reset line.
const PULSE_RESET: u8 = 0xfe;

/// Pulse the reset line through the 8042 keyboard controller, once it
/// takes a command (or after a bounded wait for it). Returns if nothing
/// happened.
pub fn keyboard_controller() {
    for _ in 0..100_000 {
        if i8042::status() & i8042::INPUT_FULL == 0 {
            break;
        }
        core::hint::spin_loop();
    }
    i8042::write_command(PULSE_RESET);
}

/// Load an empty IDT and take an exception: with no handler for it, nor
/// for the #DF that follows, the CPU shuts down (a triple fault), which
/// resets the machine.
pub fn triple_fault() -> ! {
    #[repr(C, packed(2))]
    struct Empty {
        limit: u16,
        base: u64,
    }
    let empty = Empty { limit: 0, base: 0 };
    // SAFETY: deliberate: after `lidt` no interrupt can be delivered, and
    // `int3` faults on the empty table, then the #DF does: the CPU resets.
    // Nothing runs after it.
    unsafe {
        asm!("cli", "lidt [{}]", "int3", in(reg) &raw const empty, options(readonly, nostack));
    }
    loop {
        core::hint::spin_loop();
    }
}

/// The PM1 control register's fields.
const SCI_EN: u16 = 1 << 0;
const SLP_TYP_SHIFT: u16 = 10;
const SLP_TYP: u16 = 0b111 << SLP_TYP_SHIFT;
const SLP_EN: u16 = 1 << 13;

/// Whether the PM1 control register at `port` has SCI_EN set (the
/// chipset is in ACPI mode).
pub fn acpi_mode(port: u16) -> bool {
    // SAFETY: `port` is the PM1a control register the FADT names; reading
    // it has no side effects.
    unsafe { inw(port) & SCI_EN != 0 }
}

/// Ask the firmware for ACPI mode: write `value` (the FADT's ACPI_ENABLE)
/// to its SMI command port.
pub fn enable_acpi(smi_command: u16, value: u8) {
    // SAFETY: the port and value are the FADT's, for exactly this.
    unsafe { outb(smi_command, value) }
}

/// Set the sleep type `typ` in the PM1 control register at `port`, then,
/// if `enter`, set SLP_EN (entering that sleep state). Other bits are kept.
pub fn pm1_sleep(port: u16, typ: u8, enter: bool) {
    // SAFETY: `port` is a PM1 control register the FADT names; this is how
    // ACPI enters a sleep state (ACPI 6.5, 7.4.2 and 4.8.3.2.1).
    unsafe {
        let v = (inw(port) & !(SLP_TYP | SLP_EN)) | (u16::from(typ & 7) << SLP_TYP_SHIFT);
        outw(port, if enter { v | SLP_EN } else { v });
    }
}
