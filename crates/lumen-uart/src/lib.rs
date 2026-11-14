//! 16550 UART logic, generic over how its registers are reached.
//!
//! The kernel reaches COM1 through port I/O; the tests here through a model
//! 16550 ([`tests`]). Everything that decides what to write is here:
//!
//! - [`probe`]: is a UART there at all? Its scratch register must hold two
//!   patterns (an absent port reads `0xff` whatever was written). The old
//!   value is put back.
//! - [`init`]: program 115200 8N1 with the FIFOs on, and say how to write
//!   ([`Mode`]): in bursts of [`FIFO_SIZE`] bytes if the interrupt
//!   identification register says the FIFOs work (a 16550A), else byte by
//!   byte, or not at all without a UART.
//! - [`write_bytes`]: each `\n` goes out as `\r\n`; one wait for an empty
//!   transmitter, then up to a FIFO's worth of (expanded) bytes.
//! - [`try_read`]: one received byte, if any (never on an absent port,
//!   whose line status reads `0xff`: "data ready" forever).
//!
//! A burst assumes the FIFO is empty once the line status says so: two
//! writers bursting after the same check overrun it and lose bytes. Callers
//! serialize whole writes.

#![no_std]

/// Register offsets from the UART's base.
pub mod reg {
    /// Transmit (write) and receive (read) buffer.
    pub const DATA: u16 = 0;
    pub const INT_ENABLE: u16 = 1;
    /// FIFO control when written...
    pub const FIFO_CTRL: u16 = 2;
    /// ...interrupt identification when read.
    pub const INT_ID: u16 = 2;
    pub const LINE_CTRL: u16 = 3;
    pub const MODEM_CTRL: u16 = 4;
    pub const LINE_STATUS: u16 = 5;
    pub const SCRATCH: u16 = 7;
}

/// Line status: a received byte is waiting.
pub const LSR_DATA_READY: u8 = 1 << 0;
/// Line status: the transmit holding register (with FIFOs on: the whole
/// transmit FIFO) is empty.
pub const LSR_TX_EMPTY: u8 = 1 << 5;
/// Modem control: DTR and RTS.
pub const MCR_DTR_RTS: u8 = 0x03;
/// Modem control: loopback (what is sent comes back to the receiver).
pub const MCR_LOOPBACK: u8 = 1 << 4;
/// The 16550A's transmit (and receive) FIFO size.
pub const FIFO_SIZE: usize = 16;

/// How the UART's registers are reached.
pub trait Registers {
    fn read(&mut self, reg: u16) -> u8;
    fn write(&mut self, reg: u16, value: u8);
}

/// How [`write_bytes`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// No UART: writes do nothing, reads find nothing.
    Absent = 0,
    /// One byte per wait for an empty transmitter (no working FIFO).
    Bytes = 1,
    /// Up to [`FIFO_SIZE`] bytes per wait.
    Fifo = 2,
}

impl Mode {
    /// The mode stored as `self as u8` (anything else is [`Mode::Absent`]).
    pub const fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::Bytes,
            2 => Mode::Fifo,
            _ => Mode::Absent,
        }
    }

    /// Bytes written per wait for an empty transmitter.
    pub const fn burst(self) -> usize {
        match self {
            Mode::Absent => 0,
            Mode::Bytes => 1,
            Mode::Fifo => FIFO_SIZE,
        }
    }
}

/// Whether a UART answers: its scratch register holds `0x55` and then
/// `0xaa`. The register's old value is written back either way.
pub fn probe(r: &mut impl Registers) -> bool {
    let old = r.read(reg::SCRATCH);
    let mut held = true;
    for pattern in [0x55, 0xaa] {
        r.write(reg::SCRATCH, pattern);
        held &= r.read(reg::SCRATCH) == pattern;
    }
    r.write(reg::SCRATCH, old);
    held
}

/// Whether the interrupt identification register says the FIFOs are on
/// and work (bits 6-7 both set; `0b10` is the original 16550's broken
/// FIFO, `0b00` none).
pub const fn fifo_enabled(iir: u8) -> bool {
    iir & 0xc0 == 0xc0
}

/// Program the UART for 115200 8N1, FIFOs on and cleared, interrupts off,
/// DTR and RTS set, and say how to write to it. Without a UART ([`probe`]
/// fails) nothing else is written.
pub fn init(r: &mut impl Registers) -> Mode {
    if !probe(r) {
        return Mode::Absent;
    }
    r.write(reg::INT_ENABLE, 0x00);
    r.write(reg::LINE_CTRL, 0x80); // DLAB on: the next two writes set the divisor
    r.write(reg::DATA, 0x01); // divisor 1 = 115200 baud
    r.write(reg::INT_ENABLE, 0x00);
    r.write(reg::LINE_CTRL, 0x03); // 8 bits, no parity, one stop bit
    r.write(reg::FIFO_CTRL, 0xc7); // enable + clear FIFOs, 14-byte threshold
    r.write(reg::MODEM_CTRL, MCR_DTR_RTS);
    if fifo_enabled(r.read(reg::INT_ID)) { Mode::Fifo } else { Mode::Bytes }
}

/// Write `bytes`, each `\n` as `\r\n`: wait until the transmitter is empty,
/// then write up to `mode.burst()` (expanded) bytes, and so on. Returns how
/// many times it waited (0 for nothing written).
pub fn write_bytes(r: &mut impl Registers, mode: Mode, bytes: &[u8]) -> usize {
    let burst = mode.burst();
    if burst == 0 {
        return 0;
    }
    let (mut room, mut waits) = (0, 0);
    let mut put = |r: &mut _, b| {
        if room == 0 {
            wait_tx_empty(r);
            room = burst;
            waits += 1;
        }
        Registers::write(r, reg::DATA, b);
        room -= 1;
    };
    for &b in bytes {
        if b == b'\n' {
            put(&mut *r, b'\r');
        }
        put(&mut *r, b);
    }
    waits
}

fn wait_tx_empty(r: &mut impl Registers) {
    while r.read(reg::LINE_STATUS) & LSR_TX_EMPTY == 0 {
        core::hint::spin_loop();
    }
}

/// One received byte, if one is waiting (never without a UART).
pub fn try_read(r: &mut impl Registers, mode: Mode) -> Option<u8> {
    if mode == Mode::Absent {
        return None;
    }
    (r.read(reg::LINE_STATUS) & LSR_DATA_READY != 0).then(|| r.read(reg::DATA))
}

#[cfg(test)]
mod tests;
