//! The 8042 PS/2 controller's registers: data at port 0x60, status (read)
//! and command (write) at 0x64. The protocol (init, the keyboard) is in
//! `crate::keyboard`; this is only the port access.

use super::port::{inb, outb};

const DATA: u16 = 0x60;
const STATUS_COMMAND: u16 = 0x64;

/// Status bit 0: a byte waits in the output buffer (read it from `DATA`).
pub const OUTPUT_FULL: u8 = 1 << 0;
/// Status bit 1: the controller hasn't taken the last byte written yet.
pub const INPUT_FULL: u8 = 1 << 1;
/// Status bit 5: the waiting byte came from the second port (a mouse).
pub const FROM_PORT2: u8 = 1 << 5;

pub fn status() -> u8 {
    // SAFETY: reading the 8042's status register has no side effects.
    unsafe { inb(STATUS_COMMAND) }
}

/// Read the output buffer (call when `OUTPUT_FULL` is set).
pub fn read_data() -> u8 {
    // SAFETY: reading the data port takes the waiting byte; nothing else
    // reads it (the keyboard driver serializes its users).
    unsafe { inb(DATA) }
}

/// Write `byte` to the data port: a byte for the first port's device, or a
/// command's argument (call when `INPUT_FULL` is clear).
pub fn write_data(byte: u8) {
    // SAFETY: the keyboard driver is the 8042's only user.
    unsafe { outb(DATA, byte) }
}

/// Write a controller command (call when `INPUT_FULL` is clear).
pub fn write_command(command: u8) {
    // SAFETY: the keyboard driver is the 8042's only user.
    unsafe { outb(STATUS_COMMAND, command) }
}
