//! 16550 UART on COM1, polled (the logic is `lumen-uart`'s).
//!
//! This is the kernel's first output device and the test harness's channel:
//! logs and TAP results go out here, and scenario input comes back in.
//!
//! [`init`] checks a UART is there before touching anything else; without
//! one, writes do nothing and reads find nothing (`boot` says so on the
//! screen). Writes come in two kinds:
//!
//! - [`SerialWriter`]: byte by byte (one wait for an empty transmit FIFO
//!   per byte), without a lock: the crash reporter's, which can't wait for
//!   one (it may run while another CPU holds the line lock, or alongside a
//!   CPU it couldn't stop).
//! - [`burst_bytes`]: with a working FIFO (a 16550A), up to 15 bytes per
//!   wait (`lumen_uart::BURST`). Two CPUs bursting after the same check
//!   would overrun the FIFO and lose bytes, so only `klog` calls it
//!   (`klog::burst_held`), holding the log's line lock: for the logger
//!   itself, and through `klog::LockedSerial` for the serial shell, the
//!   ktest runner and the `crash` command. The slot a burst leaves free
//!   takes a byte the crash reporter writes after the same check, so the
//!   two lose nothing.
//!
//! Waits are unbounded (as on any polled driver): a UART that never
//! empties hangs its writer.

use super::port::{inb, outb};
use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};
use lumen_uart::{Mode, Registers};

/// A 16550's registers, by port I/O from its base port.
struct Com(u16);

/// The first serial port, the kernel's.
fn com1() -> Com {
    Com(0x3f8)
}

impl Com {
    /// The UART at `base` (for the ktests' empty COM4).
    ///
    /// # Safety
    /// `base..base + 8` is a 16550's ports or ports nothing answers on:
    /// [`Registers`] reads and writes them from safe code.
    #[cfg(feature = "ktest")]
    const unsafe fn new(base: u16) -> Com {
        Com(base)
    }
}

impl Registers for Com {
    fn read(&mut self, reg: u16) -> u8 {
        debug_assert!(reg < 8, "a 16550 has eight registers");
        // SAFETY: a `Com` is COM1 (`com1`) or a port range `Com::new`'s caller vouched
        // for (a 16550's or an empty one); reading a UART register changes
        // only the UART's own state (the receive buffer, the identification).
        unsafe { inb(self.0 + reg) }
    }

    fn write(&mut self, reg: u16, value: u8) {
        debug_assert!(reg < 8, "a 16550 has eight registers");
        // SAFETY: as in `read`: writing a UART register only changes the
        // UART (or nothing, where none answers).
        unsafe { outb(self.0 + reg, value) }
    }
}

/// How COM1 is written (a [`Mode`] as `u8`), set once by [`init`]. A plain
/// atomic, not a `SpinLock`: `init`
/// runs before GS is set up, and a lock needs the CPU index GS gives.
static MODE: AtomicU8 = AtomicU8::new(Mode::Absent.to_u8());

/// Find COM1 and program it for 115200 8N1, FIFOs on, interrupts off.
pub fn init() {
    MODE.store(lumen_uart::init(&mut com1()).to_u8(), Ordering::Relaxed);
}

/// How COM1 is written in bursts: not at all, byte by byte (no working
/// FIFO), or up to 15 bytes at a time.
pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Whether [`init`] found a UART at COM1.
pub fn present() -> bool {
    mode() != Mode::Absent
}

/// The mode for a writer that doesn't serialize with the others: byte by
/// byte where bursts would be.
fn unserialized(mode: Mode) -> Mode {
    match mode {
        Mode::Fifo => Mode::Bytes,
        other => other,
    }
}

/// Write `bytes` through `com` in `mode`; the waits for an empty transmit
/// FIFO.
fn write_to(com: &mut Com, mode: Mode, bytes: &[u8]) -> usize {
    lumen_uart::write_bytes(com, mode, bytes)
}

/// One received byte through `com` in `mode` (never when absent).
fn read_from(com: &mut Com, mode: Mode) -> Option<u8> {
    lumen_uart::try_read(com, mode)
}

/// Write `bytes`, each `\n` as `\r\n`, byte by byte (nothing without a
/// UART), without a lock; the waits (for the ktests). [`SerialWriter`]'s.
fn write_bytes(bytes: &[u8]) -> usize {
    write_to(&mut com1(), unserialized(mode()), bytes)
}

/// Write `bytes`, each `\n` as `\r\n`, in bursts if the FIFO works; the
/// waits for an empty transmit FIFO. Only `klog::burst_held` calls it,
/// with the log's line lock held (see the module doc): the lock is what
/// keeps two bursts from overrunning the FIFO together.
pub(crate) fn burst_bytes(bytes: &[u8]) -> usize {
    write_to(&mut com1(), mode(), bytes)
}

/// Take COM1 out of loopback (a ktest may have left it there when another
/// CPU's crash stopped it), so the crash report reaches the host. Lock-free;
/// the crash reporter calls it before its first line and again once the
/// other CPUs are stopped (one may have entered loopback in between).
pub fn leave_loopback() {
    if present() {
        com1().write(lumen_uart::reg::MODEM_CTRL, lumen_uart::MCR_DTR_RTS);
    }
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // the serial shell's input
/// Non-blocking read of one received byte (never without a UART).
pub fn try_read_byte() -> Option<u8> {
    read_from(&mut com1(), mode())
}

/// A byte-by-byte writer without a lock (see the module doc): for the crash
/// reporter, which can't wait for a lock. Everything else writes through
/// `klog::LockedSerial`.
#[derive(Clone, Copy, Default)]
pub struct SerialWriter;

impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Wait (bounded: a ktest must fail, not hang) until the transmitter is
/// idle (TEMT: what earlier lines left in it has gone out, so loopback
/// won't bring it back); whether it got there.
#[cfg(feature = "ktest")]
fn wait_tx_idle(com: &mut Com) -> bool {
    let deadline = crate::time::now_ns() + 1_000_000_000;
    for _ in 0..100_000_000 {
        if com.read(lumen_uart::reg::LINE_STATUS) & lumen_uart::LSR_TX_IDLE != 0 {
            return true;
        }
        if crate::time::now_ns() > deadline {
            break;
        }
    }
    false
}

/// For the ktests, with the line lock held and the transmitter idle: put
/// COM1 in loopback, write with `write` (its waits), read back up to
/// `back.len()` bytes until `expect` bytes are in and nothing more comes
/// for a while (bounded in time and reads), and restore the modem control.
/// Returns (waits, bytes read).
#[cfg(feature = "ktest")]
fn loop_back(write: impl FnOnce() -> usize, expect: usize, back: &mut [u8]) -> (usize, usize) {
    use lumen_uart::{MCR_LOOPBACK, reg};
    /// Reads after the last expected byte that find nothing more.
    const QUIET_READS: usize = 10_000;
    let mut com = com1();
    let mode = mode();
    let drain = |com: &mut Com| while lumen_uart::try_read(com, mode).is_some() {};
    drain(&mut com);
    let mcr = com.read(reg::MODEM_CTRL);
    com.write(reg::MODEM_CTRL, mcr | MCR_LOOPBACK);
    let waits = write();
    let deadline = crate::time::now_ns() + 100_000_000;
    let (mut got, mut quiet) = (0, 0);
    for _ in 0..10_000_000 {
        if quiet >= QUIET_READS || crate::time::now_ns() > deadline {
            break;
        }
        match lumen_uart::try_read(&mut com, mode) {
            Some(b) if got < back.len() => {
                back[got] = b;
                got += 1;
            }
            Some(_) => {}
            None if got >= expect => quiet += 1,
            None => {}
        }
    }
    com.write(reg::MODEM_CTRL, mcr);
    drain(&mut com);
    (waits, got)
}

crate::ktest!(
    fn fifo_burst_round_trip() {
        // In loopback mode what is sent comes back to the receiver. Through
        // the logger's own burst path (`klog::burst_held`), 14 bytes with a
        // newline (15 on the wire) take one wait; through `SerialWriter`'s
        // path, one wait per byte. Both come back in order and nothing more
        // (15 bytes leave room in the 16-byte receive FIFO for an extra to
        // show). This checks the wiring (loopback, FIFO detection, burst
        // and byte paths, CR LF, order): QEMU moves each byte to the
        // receiver as it is written, so it can't overrun; `cargo:lumen-uart`'s
        // model checks bursts against overruns. Under the line lock
        // throughout, so no log or TAP line goes into the loop; the asserts
        // stay outside that closure (a failing one writes under the same
        // lock).
        use crate::{kassert, kassert_eq};
        const SENT: &[u8] = b"012345\n789abcd";
        const BACK: &[u8] = b"012345\r\n789abcd";
        // The one-wait check needs a working FIFO (QEMU's 16550A has one).
        kassert_eq!(mode(), Mode::Fifo);
        let mut burst = [0u8; 32];
        let mut bytes = [0u8; 32];
        let (idle, sent_burst, sent_bytes) = crate::klog::with_line_lock(|| {
            let idle = wait_tx_idle(&mut com1());
            let a = loop_back(|| crate::klog::burst_held(SENT), BACK.len(), &mut burst);
            let b = loop_back(|| write_bytes(SENT), BACK.len(), &mut bytes);
            (idle, a, b)
        });
        kassert!(idle);
        let ((burst_waits, burst_got), (byte_waits, bytes_got)) = (sent_burst, sent_bytes);
        kassert_eq!(burst_waits, 1);
        kassert_eq!(&burst[..burst_got], BACK);
        kassert_eq!(byte_waits, BACK.len());
        kassert_eq!(&bytes[..bytes_got], BACK);
        Ok(())
    }
);

crate::ktest!(
    fn leaving_loopback_restores_the_modem_control() {
        // What the crash reporter does first: take COM1 out of loopback
        // (DTR and RTS, as `init` left it).
        use crate::{kassert, kassert_eq};
        use lumen_uart::{MCR_DTR_RTS, MCR_LOOPBACK, reg};
        let (idle, before, after) = crate::klog::with_line_lock(|| {
            let mut com = com1();
            let idle = wait_tx_idle(&mut com);
            let before = com.read(reg::MODEM_CTRL);
            com.write(reg::MODEM_CTRL, before | MCR_LOOPBACK);
            leave_loopback();
            let after = com.read(reg::MODEM_CTRL);
            com.write(reg::MODEM_CTRL, before);
            while lumen_uart::try_read(&mut com, mode()).is_some() {}
            (idle, before, after)
        });
        kassert!(idle);
        kassert_eq!(before, MCR_DTR_RTS);
        kassert_eq!(after, MCR_DTR_RTS);
        Ok(())
    }
);

crate::ktest!(
    fn presence_detected() {
        // COM1 answers (the tests read it), and probing again leaves its
        // scratch register as it was.
        use crate::{kassert, kassert_eq};
        use lumen_uart::reg;
        kassert!(present());
        let mut com = com1();
        com.write(reg::SCRATCH, 0x3c);
        kassert!(lumen_uart::probe(&mut com));
        kassert_eq!(com.read(reg::SCRATCH), 0x3c);
        Ok(())
    }
);

crate::ktest!(
    fn absent_uart_is_not_detected() {
        // No COM4 on QEMU's q35 (QEMU puts its nth serial port at COM n, and
        // the harness gives it one): the ports read 0xff whatever is
        // written, so the probe fails, and init finds no UART.
        use crate::{kassert, kassert_eq};
        // SAFETY: COM4's port range, which nothing claims on q35.
        let mut com4 = unsafe { Com::new(0x2e8) };
        kassert!(!lumen_uart::probe(&mut com4));
        kassert_eq!(lumen_uart::init(&mut com4), Mode::Absent);
        Ok(())
    }
);

crate::ktest!(
    fn an_absent_uart_is_neither_written_nor_read() {
        // The kernel's write and read helpers (`write_to`, `read_from`, which
        // `SerialWriter`, `burst_bytes` and `try_read_byte` use on COM1)
        // with the mode init gives an empty port (COM4): writes wait for
        // nothing (an absent port's line status says "empty" and "data
        // ready" forever), and nothing is read (a read would return 0xff).
        // Unserialized writers never burst.
        use crate::kassert_eq;
        // SAFETY: COM4's port range, which nothing claims on q35.
        let mut com4 = unsafe { Com::new(0x2e8) };
        let mode = lumen_uart::init(&mut com4);
        kassert_eq!(write_to(&mut com4, unserialized(mode), b"hello\n"), 0);
        kassert_eq!(write_to(&mut com4, mode, b"hello\n"), 0);
        kassert_eq!(read_from(&mut com4, mode), None);
        kassert_eq!(
            [Mode::Absent, Mode::Bytes, Mode::Fifo].map(unserialized),
            [Mode::Absent, Mode::Bytes, Mode::Bytes]
        );
        Ok(())
    }
);
