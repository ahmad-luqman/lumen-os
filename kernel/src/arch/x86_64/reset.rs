//! Resetting the machine and powering it off, at the register level. The
//! order to try them in, and the waits between, are `crate::power`'s. The
//! ACPI registers are written only through the ports a FADT named:
//! `lumen_acpi`'s [`Pm1Control`] and [`AcpiEnable`] can't be made
//! otherwise, and [`SlpTyp`] is checked to fit its field.

use super::i8042;
use super::port::{inb, inw, outb, outw};
use core::arch::asm;
use lumen_acpi::{AcpiEnable, Pm1Control, SlpTyp};

/// The reset control register (Intel chipsets, and QEMU's q35 and PIIX).
const RESET_CONTROL: u16 = 0xcf9;
/// Bit 1: a system (platform) reset, not just an INIT of the CPU, when
/// `CPU_RESET` fires.
const SYSTEM_RESET: u8 = 1 << 1;
/// Bit 2: the reset itself, taken when the bit goes from 0 to 1.
const CPU_RESET: u8 = 1 << 2;
/// Bit 3: a full reset (power cycled), with `SYSTEM_RESET`.
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

/// Load an empty IDT (limit 0) and take `int3`: delivering vector 3 runs
/// past the IDT's limit, a #GP; delivering the #GP fails the same way, a
/// #DF; delivering the #DF fails too, and the CPU shuts down (a triple
/// fault), which resets the machine.
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

/// PM1 control bit 0, SCI_EN: the chipset is in ACPI mode.
const SCI_EN: u16 = 1 << 0;
/// Where PM1 control's 3-bit SLP_TYP field starts.
const SLP_TYP_SHIFT: u16 = 10;
/// PM1 control bits 10-12: the sleep type to enter.
const SLP_TYP: u16 = 0b111 << SLP_TYP_SHIFT;
/// PM1 control bit 13, SLP_EN: enter the sleep type in SLP_TYP.
const SLP_EN: u16 = 1 << 13;

/// Whether the PM1 control register `control` has SCI_EN set (the chipset
/// is in ACPI mode).
pub fn acpi_mode(control: Pm1Control) -> bool {
    // SAFETY: `control` is a PM1 control register a FADT named (only
    // `Fadt::parse` makes one); reading it has no side effects.
    unsafe { inw(control.port()) & SCI_EN != 0 }
}

/// Ask the firmware for ACPI mode: write the FADT's ACPI_ENABLE value to
/// its SMI command port.
pub fn enable_acpi(enable: AcpiEnable) {
    // SAFETY: the port and value are a FADT's, for exactly this (only
    // `Fadt::parse` makes an `AcpiEnable`).
    unsafe { outb(enable.port(), enable.value()) }
}

/// Set the sleep type `typ` in the PM1 control register `control`, then,
/// if `enter`, set SLP_EN (entering that sleep state). Other bits are kept.
pub fn pm1_sleep(control: Pm1Control, typ: SlpTyp, enter: bool) {
    // SAFETY: `control` is a PM1 control register a FADT named (only
    // `Fadt::parse` makes one) and `typ` fits SLP_TYP; this is how ACPI
    // enters a sleep state (ACPI 6.5, 4.8.3.2.1 and 7.4.2). Entering one
    // may power the machine off, which the caller asked for.
    unsafe {
        let v = (inw(control.port()) & !(SLP_TYP | SLP_EN)) | (u16::from(typ.get()) << SLP_TYP_SHIFT);
        outw(control.port(), if enter { v | SLP_EN } else { v });
    }
}
