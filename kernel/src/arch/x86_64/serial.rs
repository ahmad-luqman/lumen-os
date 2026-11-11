//! 16550 UART on COM1, polled.
//!
//! This is the kernel's first output device and the test harness's channel:
//! logs and TAP results go out here, and scenario input comes back in.

use super::port::{inb, outb};
use core::fmt;

const COM1: u16 = 0x3f8;
const DATA: u16 = COM1;
const INT_ENABLE: u16 = COM1 + 1;
const FIFO_CTRL: u16 = COM1 + 2;
const LINE_CTRL: u16 = COM1 + 3;
const MODEM_CTRL: u16 = COM1 + 4;
const LINE_STATUS: u16 = COM1 + 5;

#[cfg_attr(feature = "ktest", allow(dead_code))] // kmon input
const LSR_DATA_READY: u8 = 1 << 0;
const LSR_TX_EMPTY: u8 = 1 << 5;

/// Program COM1 for 115200 8N1, FIFOs on, interrupts off.
pub fn init() {
    // SAFETY: COM1 is a standard PC device; these writes only configure it.
    unsafe {
        outb(INT_ENABLE, 0x00);
        outb(LINE_CTRL, 0x80); // DLAB on: next two writes set the divisor
        outb(DATA, 0x01); // divisor 1 = 115200 baud
        outb(INT_ENABLE, 0x00);
        outb(LINE_CTRL, 0x03); // 8 bits, no parity, one stop bit
        outb(FIFO_CTRL, 0xc7); // enable + clear FIFOs, 14-byte threshold
        outb(MODEM_CTRL, 0x03); // DTR + RTS
    }
}

pub fn write_byte(byte: u8) {
    // SAFETY: polling the line status and writing the data register of COM1.
    unsafe {
        while inb(LINE_STATUS) & LSR_TX_EMPTY == 0 {
            core::hint::spin_loop();
        }
        outb(DATA, byte);
    }
}

pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // kmon input
/// Non-blocking read of one received byte.
pub fn try_read_byte() -> Option<u8> {
    // SAFETY: reading COM1 status/data registers.
    unsafe { (inb(LINE_STATUS) & LSR_DATA_READY != 0).then(|| inb(DATA)) }
}

/// A lock-free writer. The UART itself is the only shared state, and
/// interleaving between CPUs is acceptable for M0 (single CPU). Higher layers
/// (the logger) serialize whole lines.
#[derive(Clone, Copy, Default)]
pub struct SerialWriter;

impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_bytes(s.as_bytes());
        Ok(())
    }
}
