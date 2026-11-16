//! 16550 UART logic, generic over how its registers are reached.
//!
//! The kernel reaches COM1 through port I/O; the tests (`tests.rs`) a
//! model 16550. Everything that decides what to write is here: whether a
//! UART answers ([`probe`]), how to program it and write to it ([`init`],
//! [`Mode`]), `\n` as `\r\n` in bursts ([`write_bytes`]), and reading
//! ([`data_ready`], [`try_read`]; by interrupt: [`enable_rx_interrupt`],
//! the handler's [`drain`], the reader's policy, [`read_next`] and
//! [`available`], how it waits, [`wait_mode`], how a wait ended,
//! [`another_period`] and [`classify`], and when a missing interrupt is
//! worth a warning, [`MissedInterrupts`]).
//!
//! **Reading by interrupt.** The received-data interrupt reaches the CPU
//! as an edge (ISA IRQ 4 through an I/O APIC). The worst case the design
//! guards against: the line rises when bytes start waiting and stays up
//! until the reader takes them all, so no new edge comes while any byte
//! waits. (QEMU does better, raising it again whenever bytes wait; real
//! UARTs and interrupt routers may not.) The handler drains what its queue
//! has room for; bytes past that stay in the UART, behind those queued. So
//! the reader takes the queue first, then the UART ([`read_next`]), and
//! must not wait while either holds a byte ([`available`]). The tests run
//! this against a model of that worst case, whose handler runs only on an
//! edge.
//!
//! A burst assumes the transmit FIFO is empty once the line status says so
//! (THRE), and fills all but one of its slots ([`BURST`]). Two writers
//! bursting after the same check would overrun it and lose bytes, so
//! bursts are for callers that serialize whole writes with each other. A
//! writer that can't ([`Mode::Bytes`], one byte per check) still fits in
//! the slot left over: one burst and one such byte after the same check
//! lose nothing. Two byte writers on a UART without a FIFO can still
//! collide in its one-byte holding register (as any polled driver can).

#![no_std]

/// Register offsets from the UART's base.
pub mod reg {
    /// Transmit (write) and receive (read) buffer; with DLAB set, the
    /// divisor's low byte.
    pub const DATA: u16 = 0;
    /// Interrupt enable; with DLAB set, the divisor's high byte.
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
/// Line status (THRE): the transmit holding register (with FIFOs on: the
/// whole transmit FIFO) is empty. The shift register may still be sending
/// its last byte.
pub const LSR_THR_EMPTY: u8 = 1 << 5;
/// Line status (TEMT): the transmit FIFO and the shift register are both
/// empty: nothing is left to send.
pub const LSR_TX_IDLE: u8 = 1 << 6;
/// Line control: divisor latch access (DLAB): data and interrupt enable
/// are the baud divisor.
pub const LCR_DLAB: u8 = 0x80;
/// Line control: 8 data bits, no parity, one stop bit.
pub const LCR_8N1: u8 = 0x03;
/// FIFO control: enable and clear both FIFOs, receive trigger level 14:
/// with the received-data interrupt on ([`enable_rx_interrupt`]), it comes
/// once 14 bytes wait, or after a character timeout (four characters' time
/// with fewer waiting and none arriving), so a typed line raises a few
/// interrupts, not one per byte.
pub const FCR_ENABLE_CLEAR: u8 = 0xc7;
/// Interrupt enable: received data available.
pub const IER_RX_DATA: u8 = 1 << 0;
/// Modem control: DTR and RTS.
pub const MCR_DTR_RTS: u8 = 0x03;
/// Modem control: OUT2, which gates the UART's interrupt line on a PC.
pub const MCR_OUT2: u8 = 1 << 3;
/// Modem control: loopback (what is sent comes back to the receiver).
pub const MCR_LOOPBACK: u8 = 1 << 4;
/// The 16550A's transmit (and receive) FIFO size.
pub const FIFO_SIZE: usize = 16;
/// Bytes per wait in [`Mode::Fifo`]: one slot is left for a writer that
/// doesn't take part in the callers' serialization ([`Mode::Bytes`], the
/// kernel's crash reporter), so its byte and a burst written after the same
/// check both fit.
pub const BURST: usize = FIFO_SIZE - 1;

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
    /// One byte per wait for an empty transmit FIFO: no working FIFO, or a
    /// writer that doesn't serialize with the bursting ones (its byte fits
    /// in the slot a burst leaves; see the crate doc).
    Bytes = 1,
    /// Up to [`BURST`] bytes per wait, for writers that serialize with each
    /// other.
    Fifo = 2,
}

impl Mode {
    /// The mode as a `u8` (for an atomic), read back by [`Mode::from_u8`].
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// The mode stored by [`Mode::to_u8`] (anything else is
    /// [`Mode::Absent`]: serial goes quiet rather than bursting).
    pub const fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::Bytes,
            2 => Mode::Fifo,
            _ => Mode::Absent,
        }
    }
}

/// Whether a UART answers: its scratch register holds `0x55` and then
/// `0xaa` (complements, so a bit stuck either way fails one of them), or,
/// for a UART without one (an 8250), its interrupt enable register holds
/// `0x0f` and then `0x00` (briefly enabling the UART's interrupts: the
/// CPU's are off this early). Every register it writes gets its old value
/// back.
/// An absent port reads `0xff` whatever was written (on QEMU and modern
/// chipsets), so neither holds there.
pub fn probe(r: &mut impl Registers) -> bool {
    holds(r, reg::SCRATCH, [0x55, 0xaa], 0xff) || holds(r, reg::INT_ENABLE, [0x0f, 0x00], 0x0f)
}

/// Whether register `at` reads back each of `patterns` (in the bits of
/// `mask`); its old value is written back.
fn holds(r: &mut impl Registers, at: u16, patterns: [u8; 2], mask: u8) -> bool {
    let old = r.read(at);
    let mut held = true;
    for pattern in patterns {
        r.write(at, pattern);
        held &= r.read(at) & mask == pattern;
    }
    r.write(at, old);
    held
}

/// Whether the interrupt identification register says the FIFOs are on
/// and work: bits 7:6 = `11` (a 16550A). `10` (0x80) is the original
/// 16550's broken FIFO, `00` none.
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
    r.write(reg::LINE_CTRL, LCR_DLAB);
    r.write(reg::DATA, 0x01); // divisor 1 = 115200 baud
    r.write(reg::INT_ENABLE, 0x00); // the divisor's high byte
    r.write(reg::LINE_CTRL, LCR_8N1);
    r.write(reg::FIFO_CTRL, FCR_ENABLE_CLEAR);
    r.write(reg::MODEM_CTRL, MCR_DTR_RTS);
    if fifo_enabled(r.read(reg::INT_ID)) { Mode::Fifo } else { Mode::Bytes }
}

/// Write `bytes`, each `\n` as `\r\n`: wait until the transmit FIFO is
/// empty (THRE), then write up to [`BURST`] (expanded) bytes in
/// [`Mode::Fifo`], one in [`Mode::Bytes`], and so on; nothing when
/// [`Mode::Absent`]. Every call that writes waits before its first byte.
/// The waits are unbounded, as polled drivers' are: a UART that never
/// empties hangs the writer. Returns how many times it waited.
pub fn write_bytes(r: &mut impl Registers, mode: Mode, bytes: &[u8]) -> usize {
    let burst = match mode {
        Mode::Absent => return 0,
        Mode::Bytes => 1,
        Mode::Fifo => BURST,
    };
    let (mut room, mut waits) = (0, 0);
    for b in bytes {
        let out: &[u8] = if *b == b'\n' { b"\r\n" } else { core::slice::from_ref(b) };
        for &o in out {
            if room == 0 {
                wait_thr_empty(r);
                room = burst;
                waits += 1;
            }
            r.write(reg::DATA, o);
            room -= 1;
        }
    }
    waits
}

fn wait_thr_empty(r: &mut impl Registers) {
    while r.read(reg::LINE_STATUS) & LSR_THR_EMPTY == 0 {
        core::hint::spin_loop();
    }
}

/// Whether a received byte is waiting (never without a UART).
pub fn data_ready(r: &mut impl Registers, mode: Mode) -> bool {
    mode != Mode::Absent && r.read(reg::LINE_STATUS) & LSR_DATA_READY != 0
}

/// One received byte, if one is waiting (never without a UART).
pub fn try_read(r: &mut impl Registers, mode: Mode) -> Option<u8> {
    data_ready(r, mode).then(|| r.read(reg::DATA))
}

/// Interrupt when received bytes wait (after [`init`], which leaves DLAB
/// clear and, with a FIFO, the trigger level at 14: [`FCR_ENABLE_CLEAR`];
/// without one, with each byte): OUT2 set with DTR and RTS, and the
/// received-data interrupt enabled. Nothing without a UART.
pub fn enable_rx_interrupt(r: &mut impl Registers, mode: Mode) {
    if mode == Mode::Absent {
        return;
    }
    r.write(reg::MODEM_CTRL, MCR_DTR_RTS | MCR_OUT2);
    r.write(reg::INT_ENABLE, IER_RX_DATA);
}

/// The reader's next byte (the module doc, "Reading by interrupt"):
/// `popped`, the byte it took from the queue (the older bytes), else the
/// UART's (bytes the handler left there for want of room). Never waits.
/// The caller keeps the handler from running from its pop to the end of
/// this call (interrupts off on the handler's CPU), or a byte the handler
/// queues meanwhile would come after a newer one read here.
pub fn read_next(r: &mut impl Registers, mode: Mode, popped: Option<u8>) -> Option<u8> {
    popped.or_else(|| try_read(r, mode))
}

/// Whether the reader has a byte to take (`queued`: its queue isn't
/// empty): the condition its wait for the interrupt must check, since a
/// byte left in the UART may raise no new edge (the module doc).
pub fn available(r: &mut impl Registers, mode: Mode, queued: bool) -> bool {
    queued || data_ready(r, mode)
}

/// Hand received bytes to `push` while one is waiting, at most `room`;
/// how many. Bytes past `room` stay in the UART for the reader
/// ([`read_next`]). Behind a full FIFO the sender's next bytes are held
/// back by QEMU; a real UART drops them (an overrun).
pub fn drain(r: &mut impl Registers, mode: Mode, room: usize, mut push: impl FnMut(u8)) -> usize {
    let mut n = 0;
    while n < room {
        let Some(byte) = try_read(r, mode) else { break };
        push(byte);
        n += 1;
    }
    n
}

/// How the reader waits for input, when it has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitMode {
    /// Block for good: there is no UART, so nothing will come.
    Forever,
    /// Block until the interrupt brings a byte, at most until a deadline
    /// (the safety net for an interrupt that doesn't come), then look.
    Deadline,
    /// Yield once and look again (polling): the reader can't block here,
    /// or no interrupt brings the bytes.
    Yield,
}

/// How to wait ([`WaitMode`]): with a UART (`present`) whose bytes come by
/// interrupt (`by_interrupt`), block with a deadline; without a UART, block
/// for good; either only where the reader's thread can block
/// (`can_block`); otherwise yield.
pub fn wait_mode(present: bool, by_interrupt: bool, can_block: bool) -> WaitMode {
    match (present, by_interrupt, can_block) {
        (_, _, false) => WaitMode::Yield,
        (false, _, true) => WaitMode::Forever,
        (true, true, true) => WaitMode::Deadline,
        (true, false, true) => WaitMode::Yield,
    }
}

/// How a [`WaitMode::Deadline`] wait ended ([`classify`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// The interrupt handler's wake readied the reader: an interrupt
    /// brought input.
    Woken,
    /// The deadline readied the reader first, and an interrupt brought the
    /// input at or after the deadline, before the reader ran (another
    /// thread, or the vCPU, ran meanwhile): the interrupt came, late.
    Late,
    /// The deadline passed and input was waiting with no interrupt having
    /// brought it, or with one before the deadline that didn't wake the
    /// reader: the interrupt may not be reaching the CPU, or not waking it.
    InputAtDeadline,
    /// Nothing came; or a byte was already waiting at the first look,
    /// before any deadline (its interrupt still pending): neither says
    /// anything about the interrupt.
    Idle,
}

/// Whether a wait that found input should wait one more period for a late
/// interrupt: input `found` at the deadline (`past_deadline`), not woken
/// by the handler (`woken`), with nothing `queued` (so it is in the UART
/// alone, its interrupt not taken yet; it may be on its way).
pub fn another_period(found: bool, woken: bool, queued: bool, past_deadline: bool) -> bool {
    found && past_deadline && !woken && !queued
}

/// How a deadline wait ended: `woken` by the handler; else the first
/// interrupt that brought input since the reader blocked
/// (`first_input_irq_ns`) at or after the `deadline_ns` ([`WaitEnd::Late`]);
/// else input `found` once `now_ns` is past the deadline
/// ([`WaitEnd::InputAtDeadline`]: no interrupt, or one before the deadline
/// that didn't wake the reader); else [`WaitEnd::Idle`].
pub fn classify(woken: bool, first_input_irq_ns: Option<u64>, deadline_ns: u64, found: bool, now_ns: u64) -> WaitEnd {
    if woken {
        WaitEnd::Woken
    } else if first_input_irq_ns.is_some_and(|t| t >= deadline_ns) {
        WaitEnd::Late
    } else if found && now_ns >= deadline_ns {
        WaitEnd::InputAtDeadline
    } else {
        WaitEnd::Idle
    }
}

/// When input found at the deadline is worth a warning: after
/// [`MissedInterrupts::THRESHOLD`] such waits in a row, with no interrupt
/// bringing input between them (one alone may be a race with a late
/// interrupt), once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MissedInterrupts {
    /// Waits in a row that found input at the deadline.
    in_a_row: u32,
    warned: bool,
}

impl MissedInterrupts {
    /// Waits in a row that found input at the deadline before the warning.
    pub const THRESHOLD: u32 = 3;

    pub const fn new() -> Self {
        MissedInterrupts { in_a_row: 0, warned: false }
    }

    /// Note how a wait ended; whether to warn now (true once at most).
    pub fn note(&mut self, end: WaitEnd) -> bool {
        match end {
            WaitEnd::Woken | WaitEnd::Late => self.in_a_row = 0,
            WaitEnd::Idle => {}
            WaitEnd::InputAtDeadline => self.in_a_row = self.in_a_row.saturating_add(1),
        }
        let warn = !self.warned && self.in_a_row >= Self::THRESHOLD;
        self.warned |= warn;
        warn
    }
}

#[cfg(test)]
mod tests;
