//! Resetting the machine and powering it off, at the register level. The
//! order to try them in, and the waits between, are `crate::power`'s. The
//! ACPI registers are reached only through `lumen_acpi`'s [`Pm1Control`]
//! and [`AcpiEnable`] (only `Fadt::parse` makes them, from a checksummed
//! FACP table), and [`SlpTyp`] is checked to fit its field.

use super::i8042;
use super::port::{inb, inw, outb, outw};
use core::arch::asm;
use lumen_acpi::{AcpiEnable, Pm1Control, SCI_EN, SleepStep, SleepType, SlpTyp, pm1_control_value};

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
/// takes a command (or after a bounded wait for it, sending it anyway).
/// Returns if nothing happened: whether the 8042 was ready for the command.
pub fn keyboard_controller() -> bool {
    let ready = (0..100_000).any(|_| {
        core::hint::spin_loop();
        i8042::status() & i8042::INPUT_FULL == 0
    });
    i8042::write_command(PULSE_RESET);
    ready
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

/// Whether the PM1 control register `control` has SCI_EN set (the chipset
/// is in ACPI mode).
pub fn acpi_mode(control: Pm1Control) -> bool {
    // SAFETY: `control` is a PM1 control register's port: only
    // `Fadt::parse` makes one, from a checksummed FACP table. Reading it
    // has no side effects.
    unsafe { inw(control.port()) & SCI_EN != 0 }
}

/// Ask the firmware for ACPI mode: write the FADT's ACPI_ENABLE value to
/// its SMI command port.
pub fn enable_acpi(enable: AcpiEnable) {
    // SAFETY: the port and value are for exactly this: only `Fadt::parse`
    // makes an `AcpiEnable`, from a checksummed FACP table.
    unsafe { outb(enable.port(), enable.value()) }
}

/// Enter the sleep state `typ` gives: for each `SleepStep` in order, write
/// PM1a, then PM1b if there is one (`lumen_acpi::pm1_control_value` makes
/// each value from the register's current one). Returns if the machine
/// didn't sleep (or power off).
pub fn enter_sleep(pm1a: Pm1Control, pm1b: Option<Pm1Control>, typ: SleepType) {
    for step in SleepStep::ORDER {
        pm1_write(pm1a, typ.a, step);
        if let Some(pm1b) = pm1b {
            pm1_write(pm1b, typ.b, step);
        }
    }
}

fn pm1_write(control: Pm1Control, typ: SlpTyp, step: SleepStep) {
    // SAFETY: `control` is a PM1 control register's port (only
    // `Fadt::parse` makes one, from a checksummed FACP table) and the
    // value only changes SLP_TYP and SLP_EN; this is how ACPI enters a
    // sleep state. Entering one may power the machine off, which the
    // caller asked for.
    unsafe {
        let v = pm1_control_value(inw(control.port()), typ, step);
        outw(control.port(), v);
    }
}
