//! The crate's logic against a model 16550: a transmit FIFO that drains
//! one byte per line status read (so a writer that doesn't wait overruns
//! it), loopback, the scratch and interrupt enable registers, and an absent
//! port.

extern crate std;

use super::*;
use std::vec::Vec;

/// A model 16550. The transmit FIFO drains one byte per line status read,
/// after the status is taken (as if a byte went out on the wire between
/// reads), so writing more than it holds without waiting is caught.
struct Model {
    /// No device: every read is `0xff`, writes go nowhere.
    absent: bool,
    /// The scratch register doesn't hold what is written (an 8250).
    no_scratch: bool,
    /// The interrupt enable register doesn't hold what is written either.
    no_ier: bool,
    /// Bits that read as 0 in the scratch and interrupt enable registers.
    stuck_low: u8,
    /// What reading `INT_ID` returns after the FIFOs were enabled.
    iir_fifo_bits: u8,
    fifo_on: bool,
    /// Line control (bit 7, DLAB: data and interrupt enable are the divisor).
    lcr: u8,
    divisor: u16,
    mcr: u8,
    ier: u8,
    scratch: u8,
    tx: Vec<u8>,
    rx: Vec<u8>,
    /// Bytes that went out on the wire (not in loopback: those go to `rx`).
    sent: Vec<u8>,
    /// Bytes written while the transmit FIFO was full.
    overruns: usize,
    /// Every register write, in order.
    writes: Vec<(u16, u8)>,
    /// Line status reads so far.
    status_reads: usize,
    /// Line status reads since the last data write: a writer polling far
    /// longer than any burst needs is broken (fail, don't hang the test).
    idle_status_reads: usize,
}

/// More line status reads in a row than any wait here needs.
const MAX_IDLE_STATUS_READS: usize = 10_000;

impl Model {
    fn new() -> Self {
        Model {
            absent: false,
            no_scratch: false,
            no_ier: false,
            stuck_low: 0,
            iir_fifo_bits: 0xc0,
            fifo_on: false,
            lcr: 0,
            divisor: 0,
            mcr: 0,
            ier: 0,
            scratch: 0x5a,
            tx: Vec::new(),
            rx: Vec::new(),
            sent: Vec::new(),
            overruns: 0,
            writes: Vec::new(),
            status_reads: 0,
            idle_status_reads: 0,
        }
    }

    /// Send everything still in the transmit FIFO.
    fn flush_tx(&mut self) {
        while !self.tx.is_empty() {
            self.drain_one();
        }
    }

    fn capacity(&self) -> usize {
        // A UART without a working FIFO (IIR bits 7:6 not both set) holds one.
        if self.fifo_on && self.iir_fifo_bits == 0xc0 { FIFO_SIZE } else { 1 }
    }

    /// Move one byte from the transmit FIFO onto the wire (or back to the
    /// receiver in loopback).
    fn drain_one(&mut self) {
        if self.tx.is_empty() {
            return;
        }
        let b = self.tx.remove(0);
        if self.mcr & MCR_LOOPBACK != 0 {
            if self.rx.len() < FIFO_SIZE {
                self.rx.push(b);
            }
        } else {
            self.sent.push(b);
        }
    }
}

impl Registers for Model {
    fn read(&mut self, r: u16) -> u8 {
        if self.absent {
            return 0xff;
        }
        match r {
            reg::DATA => {
                if self.rx.is_empty() {
                    0
                } else {
                    self.rx.remove(0)
                }
            }
            reg::INT_ID => {
                if self.fifo_on {
                    self.iir_fifo_bits | 0x01
                } else {
                    0x01
                }
            }
            reg::LINE_STATUS => {
                self.status_reads += 1;
                self.idle_status_reads += 1;
                assert!(self.idle_status_reads <= MAX_IDLE_STATUS_READS, "polling the line status forever");
                let status = (if self.tx.is_empty() { LSR_THR_EMPTY | LSR_TX_IDLE } else { 0 })
                    | (if self.rx.is_empty() { 0 } else { LSR_DATA_READY });
                self.drain_one();
                status
            }
            reg::INT_ENABLE if self.lcr & LCR_DLAB == 0 => self.ier & !self.stuck_low,
            reg::MODEM_CTRL => self.mcr,
            reg::SCRATCH => self.scratch & !self.stuck_low,
            _ => 0,
        }
    }

    fn write(&mut self, r: u16, value: u8) {
        self.writes.push((r, value));
        if self.absent {
            return;
        }
        let dlab = self.lcr & LCR_DLAB != 0;
        match r {
            reg::DATA if dlab => self.divisor = self.divisor & 0xff00 | u16::from(value),
            reg::INT_ENABLE if dlab => self.divisor = self.divisor & 0xff | u16::from(value) << 8,
            reg::INT_ENABLE if !self.no_ier => self.ier = value & 0x0f,
            reg::LINE_CTRL => self.lcr = value,
            reg::DATA => {
                self.idle_status_reads = 0;
                if self.tx.len() >= self.capacity() {
                    self.overruns += 1;
                } else {
                    self.tx.push(value);
                }
            }
            reg::FIFO_CTRL => self.fifo_on = value & 1 != 0,
            reg::MODEM_CTRL => self.mcr = value,
            reg::SCRATCH if !self.no_scratch => self.scratch = value,
            _ => {}
        }
    }
}

/// Write `bytes`, then drain everything; returns (waits, what went out).
fn send(m: &mut Model, mode: Mode, bytes: &[u8]) -> (usize, Vec<u8>) {
    let waits = write_bytes(m, mode, bytes);
    m.flush_tx();
    (waits, core::mem::take(&mut m.sent))
}

/// The values written to register `r`, in order.
fn writes_to(m: &Model, r: u16) -> Vec<u8> {
    m.writes.iter().filter(|w| w.0 == r).map(|w| w.1).collect()
}

fn expanded(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// A model whose IIR reports `iir_fifo_bits` once the FIFOs are on, after
/// `init`.
fn ready(iir_fifo_bits: u8) -> (Model, Mode) {
    let mut m = Model::new();
    m.iir_fifo_bits = iir_fifo_bits;
    let mode = init(&mut m);
    (m, mode)
}

#[test]
fn init_detects_a_16550a_fifo() {
    let mut m = Model::new();
    m.ier = 0x05; // left on by firmware
    let mode = init(&mut m);
    assert_eq!(mode, Mode::Fifo);
    assert_eq!(m.ier, 0); // interrupts off
    assert!(m.writes.contains(&(reg::FIFO_CTRL, FCR_ENABLE_CLEAR)));
    assert!(m.fifo_on && m.mcr == MCR_DTR_RTS);
    assert_eq!((m.divisor, m.lcr), (1, 0x03)); // 115200 8N1, DLAB off again
    assert!(m.tx.is_empty() && m.sent.is_empty());
}

#[test]
fn init_without_a_working_fifo_writes_byte_by_byte() {
    for bits in [0x00, 0x80, 0x40] {
        assert_eq!(ready(bits).1, Mode::Bytes, "iir bits {bits:#x}");
    }
}

#[test]
fn fifo_bits_are_both_needed() {
    assert!(fifo_enabled(0xc1) && fifo_enabled(0xcc));
    assert!(!fifo_enabled(0x81) && !fifo_enabled(0x41) && !fifo_enabled(0x01));
}

#[test]
fn an_absent_port_is_not_detected_and_left_alone() {
    let mut m = Model::new();
    m.absent = true;
    assert_eq!(init(&mut m), Mode::Absent);
    // Only the probe's writes (scratch, then interrupt enable): nothing
    // configured.
    assert!(m.writes.iter().all(|&(r, _)| r == reg::SCRATCH || r == reg::INT_ENABLE), "{:?}", m.writes);
    assert_eq!(write_bytes(&mut m, Mode::Absent, b"hello\n"), 0);
    assert_eq!(m.writes.iter().filter(|&&(r, _)| r == reg::DATA).count(), 0);
    // Its line status reads 0xff ("data ready"), but nothing is read.
    assert_eq!(try_read(&mut m, Mode::Absent), None);
}

#[test]
fn a_uart_without_a_scratch_register_is_found_by_its_interrupt_enable() {
    // An 8250: no scratch register (and no FIFO), but the interrupt enable
    // register holds what is written. Found, written byte by byte, and the
    // interrupt enable register is left as it was (then cleared by init).
    let mut m = Model::new();
    m.no_scratch = true;
    m.ier = 0x05;
    assert!(probe(&mut m));
    assert_eq!(m.ier, 0x05);
    m.iir_fifo_bits = 0;
    assert_eq!(init(&mut m), Mode::Bytes);
    assert_eq!(m.ier, 0);
}

#[test]
fn a_port_holding_neither_register_means_absent() {
    let mut m = Model::new();
    m.no_scratch = true;
    m.no_ier = true;
    assert!(!probe(&mut m));
    assert_eq!(init(&mut m), Mode::Absent);
}

#[test]
fn registers_holding_only_one_pattern_mean_absent_and_are_restored() {
    // Bits stuck at 0 in both registers the probe tries: one of each
    // register's patterns reads back wrong, so a probe trying only the
    // pattern that reads back right would pass. Absent, and both registers
    // get their old values.
    for stuck in [0x05, 0x0a] {
        let mut m = Model::new();
        m.stuck_low = stuck;
        m.scratch = 0x30;
        m.ier = 0x00;
        assert!(!probe(&mut m), "stuck bits {stuck:#x}");
        assert_eq!(writes_to(&m, reg::SCRATCH), [0x55, 0xaa, 0x30]);
        assert_eq!(writes_to(&m, reg::INT_ENABLE), [0x0f, 0x00, 0x00]);
    }
}

#[test]
fn probe_restores_the_scratch_register() {
    let mut m = Model::new();
    m.scratch = 0x3c;
    assert!(probe(&mut m));
    assert_eq!(m.scratch, 0x3c);
    assert_eq!(writes_to(&m, reg::SCRATCH), [0x55, 0xaa, 0x3c]);
    // A scratch register that holds: interrupt enable isn't touched.
    assert!(m.writes.iter().all(|w| w.0 == reg::SCRATCH));
}

#[test]
fn a_fifo_burst_waits_once_per_fifteen_bytes() {
    let (mut m, mode) = ready(0xc0);
    for len in [1usize, 14, 15, 16, 29, 30, 31, 100] {
        let bytes: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let (waits, out) = send(&mut m, mode, &bytes);
        assert_eq!(waits, len.div_ceil(BURST), "len {len}");
        assert_eq!(out, bytes, "len {len}");
    }
    assert_eq!(m.overruns, 0);
}

#[test]
fn newlines_count_twice_at_every_burst_boundary() {
    // A `\n` as the 14th, 15th and 16th input byte: its `\r\n` ends one
    // burst, straddles two, or starts the next. Never more than a burst
    // per wait (the model counts overruns).
    let (mut m, mode) = ready(0xc0);
    for at in 12..18 {
        for len in at + 1..at + 20 {
            let mut bytes: Vec<u8> = (0..len).map(|i| b'0' + (i % 10) as u8).collect();
            bytes[at] = b'\n';
            let (waits, out) = send(&mut m, mode, &bytes);
            assert_eq!(out, expanded(&bytes), "newline at {at}, len {len}");
            assert_eq!(waits, (len + 1).div_ceil(BURST), "newline at {at}, len {len}");
        }
    }
    let (waits, out) = send(&mut m, mode, b"\n\n\n\n\n\n\n\n\n");
    assert_eq!((waits, out), (2, expanded(b"\n\n\n\n\n\n\n\n\n")));
    assert_eq!(m.overruns, 0);
}

#[test]
fn every_call_waits_before_its_first_byte() {
    // Room isn't carried from one call to the next: another writer (or the
    // crash reporter) may have written in between.
    let (mut m, mode) = ready(0xc0);
    m.tx.extend_from_slice(&[b'x'; FIFO_SIZE]); // someone else's burst, still queued
    m.status_reads = 0;
    assert_eq!(write_bytes(&mut m, mode, b"ab"), 1);
    assert!(m.status_reads > 1); // it waited for the FIFO to drain
    assert_eq!(m.overruns, 0);
}

#[test]
fn byte_mode_waits_for_every_byte() {
    let (mut m, mode) = ready(0x00);
    assert_eq!(mode, Mode::Bytes);
    let (waits, out) = send(&mut m, mode, b"ab\ncd");
    assert_eq!((waits, out), (6, b"ab\r\ncd".to_vec()));
    assert_eq!(m.overruns, 0);
}

#[test]
fn a_burst_in_byte_mode_would_overrun() {
    // The model catches a writer that doesn't wait: what the burst size
    // guards against on a UART without a FIFO.
    let (mut m, _) = ready(0x00);
    write_bytes(&mut m, Mode::Fifo, b"0123");
    assert!(m.overruns > 0);
}

#[test]
fn loopback_returns_a_burst_in_order() {
    let (mut m, mode) = ready(0xc0);
    m.write(reg::MODEM_CTRL, MCR_DTR_RTS | MCR_LOOPBACK);
    assert_eq!(write_bytes(&mut m, mode, b"012345\n789abcd"), 1);
    m.flush_tx();
    let mut back = Vec::new();
    while let Some(b) = try_read(&mut m, mode) {
        back.push(b);
    }
    assert_eq!(back, b"012345\r\n789abcd");
}

#[test]
fn a_burst_and_a_lock_free_byte_after_the_same_check_fit() {
    // Two writers see an empty transmit FIFO at once: one bursts, the
    // other (byte mode, the crash reporter) writes its one byte. With a
    // burst of `BURST` both fit; a full-FIFO burst would lose a byte.
    for (burst, lost) in [(BURST, 0), (FIFO_SIZE, 1)] {
        let (mut m, _) = ready(0xc0);
        for _ in 0..2 {
            assert_ne!(m.read(reg::LINE_STATUS) & LSR_THR_EMPTY, 0);
        }
        for i in 0..burst {
            m.write(reg::DATA, b'a' + i as u8);
        }
        m.write(reg::DATA, b'!');
        assert_eq!(m.overruns, lost, "burst {burst}");
    }
}

#[test]
fn a_waiting_receive_byte_doesnt_end_the_wait() {
    // In loopback the first burst comes back while the second waits: the
    // wait is for THRE, not for any status bit (a received byte must not
    // let a burst go into a full FIFO).
    let (mut m, mode) = ready(0xc0);
    m.write(reg::MODEM_CTRL, MCR_DTR_RTS | MCR_LOOPBACK);
    let bytes = [b'x'; 2 * BURST];
    assert_eq!(write_bytes(&mut m, mode, &bytes), 2);
    assert_eq!(m.overruns, 0);
}

#[test]
fn mode_round_trips_through_u8() {
    for mode in [Mode::Absent, Mode::Bytes, Mode::Fifo] {
        assert_eq!(Mode::from_u8(mode.to_u8()), mode);
    }
    assert_eq!(Mode::from_u8(0xff), Mode::Absent);
}
