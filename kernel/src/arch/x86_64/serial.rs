//! 16550 UART on COM1, polled (the logic is `lumen-uart`'s).
//!
//! This is the kernel's first output device and the test harness's channel:
//! logs and TAP results go out here, and scenario input comes back in.
//!
//! [`init`] checks a UART is there (its scratch register) before touching
//! anything else; without one, writes do nothing and reads find nothing
//! (`boot` says so on the screen). With a working FIFO (a 16550A), writes
//! go out in bursts of 16 bytes per wait for an empty transmitter.

use super::port::{inb, outb};
use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};
use lumen_uart::{Mode, Registers};

const COM1: u16 = 0x3f8;

/// A 16550's registers, by port I/O from its base port.
#[derive(Clone, Copy)]
pub struct Com(pub u16);

impl Registers for Com {
    fn read(&mut self, reg: u16) -> u8 {
        // SAFETY: reading a UART register has no side effect beyond the
        // UART's own state (the receive buffer, the identification).
        unsafe { inb(self.0 + reg) }
    }

    fn write(&mut self, reg: u16, value: u8) {
        // SAFETY: writing a UART register only changes the UART (or nothing,
        // on a port where none answers).
        unsafe { outb(self.0 + reg, value) }
    }
}

/// How COM1 is written (a [`Mode`] as `u8`), set once by [`init`]. A plain
/// atomic: `init` runs before the GDT, GS and the IDT are set up, and every
/// write reads it lock-free.
static MODE: AtomicU8 = AtomicU8::new(Mode::Absent as u8);

/// Find COM1 and program it for 115200 8N1, FIFOs on, interrupts off.
pub fn init() {
    MODE.store(lumen_uart::init(&mut Com(COM1)) as u8, Ordering::Relaxed);
}

/// How COM1 is written: not at all, byte by byte, or in 16-byte bursts.
pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Whether [`init`] found a UART at COM1.
pub fn present() -> bool {
    mode() != Mode::Absent
}

/// Write `bytes`, each `\n` as `\r\n` (nothing without a UART). Callers
/// serialize whole writes (see [`SerialWriter`]).
pub fn write_bytes(bytes: &[u8]) {
    lumen_uart::write_bytes(&mut Com(COM1), mode(), bytes);
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // the serial shell's input
/// Non-blocking read of one received byte (never without a UART).
pub fn try_read_byte() -> Option<u8> {
    lumen_uart::try_read(&mut Com(COM1), mode())
}

/// A writer without a lock of its own. A burst assumes the transmit FIFO
/// is empty once the line status says so: two CPUs writing at once could
/// both burst after the same check and overrun it, losing bytes. So callers
/// serialize whole writes: the logger, both shells' serial output, the
/// ktest runner's TAP lines and the `crash` command's messages hold the
/// log's line lock (`klog::with_line_lock`), and the crash reporter stops
/// the other CPUs first.
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
        // (15 bytes with a newline, 16 on the wire: one wait) comes back in
        // order. Under the line lock throughout, so no log line or TAP line
        // goes into the loop meanwhile.
        use crate::{kassert, kassert_eq};
        use lumen_uart::{MCR_LOOPBACK, reg};
        let sent = b"0123456\n89abcde";
        let mut back = [0u8; 32];
        let (waits, got) = crate::klog::with_line_lock(|| {
            let mut com = Com(COM1);
            let drain = |com: &mut Com| while lumen_uart::try_read(com, Mode::Fifo).is_some() {};
            drain(&mut com);
            let mcr = com.read(reg::MODEM_CTRL);
            com.write(reg::MODEM_CTRL, mcr | MCR_LOOPBACK);
            let waits = lumen_uart::write_bytes(&mut com, Mode::Fifo, sent);
            let deadline = crate::time::now_ns() + 100_000_000;
            let mut got = 0;
            while got < back.len() && crate::time::now_ns() < deadline {
                if let Some(b) = lumen_uart::try_read(&mut com, Mode::Fifo) {
                    back[got] = b;
                    got += 1;
                }
            }
            com.write(reg::MODEM_CTRL, mcr);
            drain(&mut com);
            (waits, got)
        });
        kassert_eq!(mode(), Mode::Fifo);
        kassert_eq!(waits, 1);
        kassert!(&back[..got] == b"0123456\r\n89abcde");
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
        let mut com = Com(COM1);
        com.write(reg::SCRATCH, 0x3c);
        kassert!(lumen_uart::probe(&mut com));
        kassert_eq!(com.read(reg::SCRATCH), 0x3c);
        Ok(())
    }
);

crate::ktest!(
    fn absent_uart_is_not_detected() {
        // No COM4 on QEMU's q35 (the harness adds no second serial port):
        // the port reads 0xff whatever is written, so its probe fails.
        crate::kassert!(!lumen_uart::probe(&mut Com(0x2e8)));
        Ok(())
    }
);
