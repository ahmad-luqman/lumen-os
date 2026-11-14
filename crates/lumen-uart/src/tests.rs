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
    /// The transmit FIFO never empties (a wedged UART).
    stuck: bool,
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
            stuck: false,
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
                let empty = self.tx.is_empty() && !self.stuck;
                let status = (if empty { LSR_THR_EMPTY | LSR_TX_IDLE } else { 0 })
                    | (if self.rx.is_empty() { 0 } else { LSR_DATA_READY });
                if !self.stuck {
                    self.drain_one();
                }
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
    let waits = write_bytes(m, mode, bytes).expect("not stuck");
    m.flush_tx();
    (waits, core::mem::take(&mut m.sent))
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
    assert_eq!(write_bytes(&mut m, Mode::Absent, b"hello\n"), Ok(0));
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
    // Bits stuck at 0 in both registers the probe tries: 0x55 reads back
    // wrong and 0xaa right (or the other way round), so either pattern
    // alone would pass. Absent, and both registers get their old values.
    for stuck in [0x05, 0x0a] {
        let mut m = Model::new();
        m.stuck_low = stuck;
        m.scratch = 0x30;
        m.ier = 0x00;
        assert!(!probe(&mut m), "stuck bits {stuck:#x}");
        let to = |r| m.writes.iter().filter(|w| w.0 == r).map(|w| w.1).collect::<Vec<u8>>();
        assert_eq!(to(reg::SCRATCH), [0x55, 0xaa, 0x30]);
        assert_eq!(to(reg::INT_ENABLE), [0x0f, 0x00, 0x00]);
    }
}

#[test]
fn probe_restores_the_scratch_register() {
    let mut m = Model::new();
    m.scratch = 0x3c;
    assert!(probe(&mut m));
    assert_eq!(m.scratch, 0x3c);
    let scratch_writes: Vec<u8> = m.writes.iter().filter(|w| w.0 == reg::SCRATCH).map(|w| w.1).collect();
    assert_eq!(scratch_writes, [0x55, 0xaa, 0x3c]);
    // A scratch register that holds: interrupt enable isn't touched.
    assert!(m.writes.iter().all(|w| w.0 == reg::SCRATCH));
}

#[test]
fn a_fifo_burst_waits_once_per_sixteen_bytes() {
    let (mut m, mode) = ready(0xc0);
    for len in [1usize, 15, 16, 17, 31, 32, 33, 100] {
        let bytes: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let (waits, out) = send(&mut m, mode, &bytes);
        assert_eq!(waits, len.div_ceil(FIFO_SIZE), "len {len}");
        assert_eq!(out, bytes, "len {len}");
    }
    assert_eq!(m.overruns, 0);
}

#[test]
fn newlines_count_twice_at_every_burst_boundary() {
    // A `\n` as the 15th, 16th and 17th input byte: its `\r\n` ends one
    // burst, straddles two, or starts the next. Never more than 16 bytes
    // per wait (the model counts overruns).
    let (mut m, mode) = ready(0xc0);
    for at in 13..19 {
        for len in at + 1..at + 20 {
            let mut bytes: Vec<u8> = (0..len).map(|i| b'0' + (i % 10) as u8).collect();
            bytes[at] = b'\n';
            let (waits, out) = send(&mut m, mode, &bytes);
            assert_eq!(out, expanded(&bytes), "newline at {at}, len {len}");
            assert_eq!(waits, (len + 1).div_ceil(FIFO_SIZE), "newline at {at}, len {len}");
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
    assert_eq!(write_bytes(&mut m, mode, b"ab"), Ok(1));
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
    write_bytes(&mut m, Mode::Fifo, b"0123").expect("not stuck");
    assert!(m.overruns > 0);
}

#[test]
fn loopback_returns_a_burst_in_order() {
    let (mut m, mode) = ready(0xc0);
    m.write(reg::MODEM_CTRL, MCR_DTR_RTS | MCR_LOOPBACK);
    assert_eq!(write_bytes(&mut m, mode, b"0123456\n89abcde"), Ok(1));
    m.flush_tx();
    let mut back = Vec::new();
    while let Some(b) = try_read(&mut m, mode) {
        back.push(b);
    }
    assert_eq!(back, b"0123456\r\n89abcde");
}

#[test]
fn mode_round_trips_through_u8() {
    for mode in [Mode::Absent, Mode::Bytes, Mode::Fifo] {
        assert_eq!(Mode::from_u8(mode.to_u8()), mode);
    }
    assert_eq!(Mode::from_u8(0xff), Mode::Absent);
}

#[test]
fn a_transmitter_that_never_empties_is_given_up_on() {
    // A wedged UART: the bounded wait ends in `Stuck` (after exactly the
    // allowed reads) instead of spinning forever, having written nothing.
    let (mut m, mode) = ready(0xc0);
    m.stuck = true;
    m.status_reads = 0;
    assert_eq!(write_waiting(&mut m, mode, b"hello", 100), Err(Stuck));
    assert_eq!(m.status_reads, 100);
    assert!(m.tx.is_empty());
    assert_eq!(write_waiting(&mut m, Mode::Bytes, b"x", 3), Err(Stuck));
    // Absent never waits.
    assert_eq!(write_waiting(&mut m, Mode::Absent, b"x", 0), Ok(0));
}
