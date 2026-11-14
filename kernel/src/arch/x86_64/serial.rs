//! 16550 UART on COM1, polled (the logic is `lumen-uart`'s).
//!
//! This is the kernel's first output device and the test harness's channel:
//! logs and TAP results go out here, and scenario input comes back in.
//!
//! [`init`] checks a UART is there before touching anything else; without
//! one, writes do nothing and reads find nothing (`boot` says so on the
//! screen). Writes come in two kinds:
//!
//! - [`write_bytes`] and [`SerialWriter`]: byte by byte (one wait for an
//!   empty transmit FIFO per byte), safe from any CPU at any time without a
//!   lock: the crash reporter's. Two such writers can only interleave.
//! - [`burst_bytes`]: with a working FIFO (a 16550A), 16 bytes per wait.
//!   Two CPUs bursting after the same check would overrun the FIFO and lose
//!   bytes, so only the log calls it, holding its line lock: everything else
//!   that writes serial (the logger, the serial shell, the ktest runner, the
//!   `crash` command) goes through `klog::LockedSerial`.

use super::port::{inb, outb};
use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};
use lumen_uart::{Mode, Registers, Stuck};

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
        // SAFETY: a `Com` is COM1 (`com1`) or a port range `Com::new`'s caller vouched
        // for (a 16550's or an empty one); reading a UART register changes
        // only the UART's own state (the receive buffer, the identification).
        unsafe { inb(self.0 + reg) }
    }

    fn write(&mut self, reg: u16, value: u8) {
        // SAFETY: as in `read`: writing a UART register only changes the
        // UART (or nothing, where none answers).
        unsafe { outb(self.0 + reg, value) }
    }
}

/// How COM1 is written (a [`Mode`] as `u8`), set by [`init`] (and set to
/// absent if it stops emptying). A plain atomic, not a `SpinLock`: `init`
/// runs before GS is set up, and a lock needs the CPU index GS gives.
static MODE: AtomicU8 = AtomicU8::new(Mode::Absent.to_u8());

/// Find COM1 and program it for 115200 8N1, FIFOs on, interrupts off.
pub fn init() {
    MODE.store(lumen_uart::init(&mut com1()).to_u8(), Ordering::Relaxed);
}

/// How COM1 is written in bursts: not at all, byte by byte (no working
/// FIFO), or 16 bytes at a time.
pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Whether [`init`] found a UART at COM1 (and it hasn't stopped emptying).
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

/// Write `bytes` through `com` in `mode`: the waits for an empty transmit
/// FIFO, or `Stuck`.
fn write_to(com: &mut Com, mode: Mode, bytes: &[u8]) -> Result<usize, Stuck> {
    lumen_uart::write_bytes(com, mode, bytes)
}

/// One received byte through `com` in `mode` (never when absent).
fn read_from(com: &mut Com, mode: Mode) -> Option<u8> {
    lumen_uart::try_read(com, mode)
}

/// Write `bytes` to COM1 in `mode`; the waits. A UART whose transmit FIFO
/// never empties is then treated as absent (nothing is logged from here:
/// the log writes through this).
fn write(mode: Mode, bytes: &[u8]) -> usize {
    write_to(&mut com1(), mode, bytes).unwrap_or_else(|Stuck| {
        MODE.store(Mode::Absent.to_u8(), Ordering::Relaxed);
        0
    })
}

/// Write `bytes`, each `\n` as `\r\n`, byte by byte (nothing without a
/// UART): safe without any lock, from any CPU.
pub fn write_bytes(bytes: &[u8]) {
    write(unserialized(mode()), bytes);
}

/// Write `bytes`, each `\n` as `\r\n`, in 16-byte bursts if the FIFO works;
/// the waits for an empty transmit FIFO (for the ktests). Only `klog` calls
/// this, holding the log's line lock: the lock is what keeps two bursts
/// from overrunning the FIFO together.
pub(crate) fn burst_bytes(bytes: &[u8]) -> usize {
    write(mode(), bytes)
}

/// Take COM1 out of loopback (a ktest may have left it there when another
/// CPU's crash stopped it): the crash reporter's first step, so its report
/// reaches the host. Lock-free.
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

crate::ktest!(
    fn fifo_burst_round_trip() {
        // In loopback mode what is sent comes back to the receiver: a burst
        // through the log's own path (`burst_bytes`: 15 bytes with a
        // newline, 16 on the wire, so one wait) comes back in order, and
        // nothing more. This checks the wiring (loopback, FIFO
        // detection, CR LF, order): QEMU moves each byte to the receiver as
        // it is written, so it can't overrun; `cargo:lumen-uart`'s model
        // checks bursts against overruns. Under the line lock throughout, so
        // no log or TAP line goes into the loop; the asserts stay outside
        // that closure (a failing one writes under the same lock).
        use crate::kassert_eq;
        use lumen_uart::{LSR_TX_IDLE, MCR_LOOPBACK, reg};
        const SENT: &[u8] = b"0123456\n89abcde";
        const BACK: &[u8] = b"0123456\r\n89abcde";
        /// Reads after the last expected byte that find nothing more.
        const QUIET_READS: usize = 10_000;
        let mode = mode();
        kassert_eq!(mode, Mode::Fifo); // a burst on another UART could overrun it
        let mut back = [0u8; 32];
        let (waits, got) = crate::klog::with_line_lock(|| {
            let mut com = com1();
            // What earlier lines left in the transmitter goes out first: in
            // loopback it would come back here instead.
            for _ in 0..lumen_uart::THR_SPINS {
                if com.read(reg::LINE_STATUS) & LSR_TX_IDLE != 0 {
                    break;
                }
            }
            let drain = |com: &mut Com| while lumen_uart::try_read(com, mode).is_some() {};
            drain(&mut com);
            let mcr = com.read(reg::MODEM_CTRL);
            com.write(reg::MODEM_CTRL, mcr | MCR_LOOPBACK);
            let waits = burst_bytes(SENT);
            let deadline = crate::time::now_ns() + 100_000_000;
            let (mut got, mut quiet) = (0, 0);
            // Until the expected bytes are in and then a while longer (to
            // catch extras), bounded in time and in reads.
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
                    None if got >= BACK.len() => quiet += 1,
                    None => {}
                }
            }
            com.write(reg::MODEM_CTRL, mcr);
            drain(&mut com);
            (waits, got)
        });
        kassert_eq!(waits, 1);
        kassert_eq!(&back[..got], BACK);
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
        // The kernel's write and read paths with the mode init gives an
        // empty port (COM4): writes wait for nothing (an absent port's line
        // status says "empty" and "data ready" forever), and nothing is
        // read (a read would return 0xff). Unserialized writers never burst.
        use crate::kassert_eq;
        // SAFETY: COM4's port range, which nothing claims on q35.
        let mut com4 = unsafe { Com::new(0x2e8) };
        let mode = lumen_uart::init(&mut com4);
        kassert_eq!(write_to(&mut com4, unserialized(mode), b"hello\n"), Ok(0));
        kassert_eq!(write_to(&mut com4, mode, b"hello\n"), Ok(0));
        kassert_eq!(read_from(&mut com4, mode), None);
        kassert_eq!(
            [Mode::Absent, Mode::Bytes, Mode::Fifo].map(unserialized),
            [Mode::Absent, Mode::Bytes, Mode::Bytes]
        );
        Ok(())
    }
);
