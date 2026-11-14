//! The crate's logic against a model 16550: transmit FIFO that drains only
//! when the line status is read a few times (so a writer that doesn't wait
//! overruns it), loopback, scratch register, and an absent port.

extern crate std;

use super::*;
use std::vec::Vec;

/// A model 16550. The transmit FIFO drains one byte per line status read
/// after it was last written (as if a byte went out on the wire between
/// reads), so writing more than it holds without waiting is caught.
struct Model {
    /// No device: every read is `0xff`, writes go nowhere.
    absent: bool,
    /// The scratch register doesn't hold what is written (an 8250).
    no_scratch: bool,
    /// What reading `INT_ID` returns after the FIFOs were enabled.
    iir_fifo_bits: u8,
    fifo_on: bool,
    /// Line control (bit 7, DLAB: data and interrupt enable are the divisor).
    lcr: u8,
    divisor: u16,
    mcr: u8,
    scratch: u8,
    tx: Vec<u8>,
    rx: Vec<u8>,
    /// Bytes that went out (or came back, in loopback).
    sent: Vec<u8>,
    /// Bytes written while the transmit FIFO was full.
    overruns: usize,
    /// Every register write, in order.
    writes: Vec<(u16, u8)>,
}

impl Model {
    fn new() -> Self {
        Model {
            absent: false,
            no_scratch: false,
            iir_fifo_bits: 0xc0,
            fifo_on: false,
            lcr: 0,
            divisor: 0,
            mcr: 0,
            scratch: 0x5a,
            tx: Vec::new(),
            rx: Vec::new(),
            sent: Vec::new(),
            overruns: 0,
            writes: Vec::new(),
        }
    }

    fn capacity(&self) -> usize {
        // A UART without a working FIFO (IIR bits 6-7 not both set) holds one.
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
                let status = (if self.tx.is_empty() { LSR_TX_EMPTY } else { 0 })
                    | (if self.rx.is_empty() { 0 } else { LSR_DATA_READY });
                self.drain_one();
                status
            }
            reg::MODEM_CTRL => self.mcr,
            reg::SCRATCH => self.scratch,
            _ => 0,
        }
    }

    fn write(&mut self, r: u16, value: u8) {
        self.writes.push((r, value));
        if self.absent {
            return;
        }
        let dlab = self.lcr & 0x80 != 0;
        match r {
            reg::DATA if dlab => self.divisor = self.divisor & 0xff00 | u16::from(value),
            reg::INT_ENABLE if dlab => self.divisor = self.divisor & 0xff | u16::from(value) << 8,
            reg::LINE_CTRL => self.lcr = value,
            reg::DATA => {
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

/// Count the line status reads made by `write_bytes`.
struct Counting<'a> {
    inner: &'a mut Model,
    status_reads: usize,
}

impl Registers for Counting<'_> {
    fn read(&mut self, r: u16) -> u8 {
        if r == reg::LINE_STATUS {
            self.status_reads += 1;
        }
        self.inner.read(r)
    }
    fn write(&mut self, r: u16, value: u8) {
        self.inner.write(r, value);
    }
}

/// Write `bytes`, then drain everything; returns (waits, what went out).
fn send(m: &mut Model, mode: Mode, bytes: &[u8]) -> (usize, Vec<u8>) {
    let waits = write_bytes(m, mode, bytes);
    while !m.tx.is_empty() {
        m.drain_one();
    }
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

fn ready(mode_fifo: bool) -> (Model, Mode) {
    let mut m = Model::new();
    if !mode_fifo {
        m.iir_fifo_bits = 0;
    }
    let mode = init(&mut m);
    (m, mode)
}

#[test]
fn init_detects_a_16550a_fifo() {
    let (m, mode) = ready(true);
    assert_eq!(mode, Mode::Fifo);
    assert!(m.fifo_on && m.mcr == MCR_DTR_RTS);
    assert_eq!((m.divisor, m.lcr), (1, 0x03)); // 115200 8N1, DLAB off again
    assert!(m.tx.is_empty() && m.sent.is_empty());
}

#[test]
fn init_without_a_working_fifo_writes_byte_by_byte() {
    for bits in [0x00, 0x80, 0x40] {
        let mut m = Model::new();
        m.iir_fifo_bits = bits;
        assert_eq!(init(&mut m), Mode::Bytes, "iir bits {bits:#x}");
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
    // Only the probe's scratch writes: nothing configured.
    assert!(m.writes.iter().all(|&(r, _)| r == reg::SCRATCH), "{:?}", m.writes);
    assert_eq!(write_bytes(&mut m, Mode::Absent, b"hello\n"), 0);
    assert_eq!(m.writes.iter().filter(|&&(r, _)| r == reg::DATA).count(), 0);
    // Its line status reads 0xff ("data ready"), but nothing is read.
    assert_eq!(try_read(&mut m, Mode::Absent), None);
}

#[test]
fn a_scratch_register_that_holds_nothing_means_absent() {
    let mut m = Model::new();
    m.no_scratch = true;
    assert!(!probe(&mut m));
}

#[test]
fn a_scratch_register_holding_only_one_pattern_means_absent() {
    // A port that latches the last write's low bits only: 0x55 reads back
    // wrong, 0xaa right (or the other way round).
    struct Half(u8, u8);
    impl Registers for Half {
        fn read(&mut self, _: u16) -> u8 {
            self.0 & self.1
        }
        fn write(&mut self, _: u16, v: u8) {
            self.0 = v;
        }
    }
    assert!(!probe(&mut Half(0, 0xf0 | 0xaa)));
    assert!(!probe(&mut Half(0, 0xf0 | 0x55)));
    assert!(probe(&mut Half(0, 0xff)));
}

#[test]
fn probe_restores_the_scratch_register() {
    let mut m = Model::new();
    m.scratch = 0x3c;
    assert!(probe(&mut m));
    assert_eq!(m.scratch, 0x3c);
    let scratch_writes: Vec<u8> = m.writes.iter().filter(|w| w.0 == reg::SCRATCH).map(|w| w.1).collect();
    assert_eq!(scratch_writes, [0x55, 0xaa, 0x3c]);
}

#[test]
fn a_fifo_burst_waits_once_per_sixteen_bytes() {
    let (mut m, mode) = ready(true);
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
    let (mut m, mode) = ready(true);
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
    let (mut m, mode) = ready(true);
    m.tx.extend_from_slice(&[b'x'; FIFO_SIZE]); // someone else's burst, still queued
    let mut c = Counting { inner: &mut m, status_reads: 0 };
    assert_eq!(write_bytes(&mut c, mode, b"ab"), 1);
    assert!(c.status_reads > 1); // it waited for the FIFO to drain
    assert_eq!(m.overruns, 0);
}

#[test]
fn byte_mode_waits_for_every_byte() {
    let (mut m, mode) = ready(false);
    assert_eq!(mode, Mode::Bytes);
    let (waits, out) = send(&mut m, mode, b"ab\ncd");
    assert_eq!((waits, out), (6, b"ab\r\ncd".to_vec()));
    assert_eq!(m.overruns, 0);
}

#[test]
fn a_burst_in_byte_mode_would_overrun() {
    // The model catches a writer that doesn't wait: what the burst size
    // guards against on a UART without a FIFO.
    let (mut m, _) = ready(false);
    write_bytes(&mut m, Mode::Fifo, b"0123");
    assert!(m.overruns > 0);
}

#[test]
fn loopback_returns_a_burst_in_order() {
    let (mut m, mode) = ready(true);
    m.write(reg::MODEM_CTRL, MCR_DTR_RTS | MCR_LOOPBACK);
    assert_eq!(write_bytes(&mut m, mode, b"0123456\n89abcde"), 1);
    while !m.tx.is_empty() {
        m.drain_one();
    }
    let mut back = Vec::new();
    while let Some(b) = try_read(&mut m, mode) {
        back.push(b);
    }
    assert_eq!(back, b"0123456\r\n89abcde");
}

#[test]
fn mode_round_trips_through_u8() {
    for mode in [Mode::Absent, Mode::Bytes, Mode::Fifo] {
        assert_eq!(Mode::from_u8(mode as u8), mode);
    }
    assert_eq!(Mode::from_u8(0xff), Mode::Absent);
}
