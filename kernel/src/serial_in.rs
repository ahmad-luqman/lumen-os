//! Serial input by interrupt (#120): COM1's received bytes come in on ISA
//! IRQ 4 and wait in a queue the serial shell reads, so the shell blocks
//! for a key instead of polling. (Polling kept the boot CPU 100% busy
//! while idle: under TCG one host CPU per idle guest, and a vCPU thread
//! the host, when loaded, put aside for 100 ms and more at a time.)
//!
//! - **Set up** ([`init`], once, on the boot CPU, before the serial shell
//!   starts; not in the test kernel, whose loopback ktest reads the UART
//!   itself): IRQ 4 is routed masked, the UART interrupts when bytes wait
//!   (`arch::serial::enable_rx_interrupt`: at 14 bytes or after a
//!   character timeout), the input is unmasked, and
//!   the bytes that came before are drained.
//! - **Interrupt:** moves the waiting bytes into [`QUEUE`] while it has
//!   room, and wakes the reader. Bytes past its room stay in the UART
//!   (and QEMU holds back the rest), so nothing is dropped here.
//! - **Reading** ([`read_byte`], the serial shell on the boot CPU's `main`
//!   thread, with interrupts off so the handler can't run in between): the
//!   queue first, then the UART itself, for bytes the handler left there
//!   (their interrupt's edge has passed: no other comes until the UART is
//!   emptied). [`wait`] blocks until either has a byte.
//! - **Without a route** (no I/O APIC input; a warning says why) or a
//!   UART, [`read_byte`] reads the UART and [`wait`] yields: the shell
//!   polls, as before.
// (The test kernel runs no shell: nothing here is called there.)
#![cfg_attr(feature = "ktest", allow(dead_code))]

use core::sync::atomic::{AtomicBool, Ordering};

use log::{debug, warn};
use lumen_queue::ByteQueue;

use crate::arch;
use crate::interrupts;
use crate::sched::WaitQueue;

/// The ISA IRQ COM1 interrupts on.
const COM1_IRQ: u8 = 4;
/// Bytes the queue holds (a pasted line or two ahead of the shell).
const QUEUE_SIZE: usize = 256;

/// Received bytes, from the interrupt handler (the producer: the boot CPU
/// with interrupts off) to [`read_byte`] (the consumer: the serial shell).
static QUEUE: ByteQueue<QUEUE_SIZE> = ByteQueue::new();
/// The serial shell waits here for a byte.
static WAITING: WaitQueue = WaitQueue::new();
/// Whether IRQ 4 brings the bytes (else the shell polls).
static BY_INTERRUPT: AtomicBool = AtomicBool::new(false);

/// Route IRQ 4 and have the UART interrupt when bytes wait (the module
/// doc). On the boot CPU, once; without a UART or a route the shell polls.
pub fn init() {
    if !arch::serial::present() {
        return;
    }
    let route = match interrupts::route_isa_irq(COM1_IRQ, on_irq) {
        Ok(route) => route,
        Err(e) => {
            warn!(target: "serial", "serial input is polled (no interrupt): {e}");
            return;
        }
    };
    if !arch::serial::enable_rx_interrupt() {
        return;
    }
    BY_INTERRUPT.store(true, Ordering::Release);
    interrupts::set_masked(route, false);
    // A byte that came before the input was unmasked raised its edge while
    // it was masked: take it now (no new edge comes while it waits).
    let _off = arch::irq::push_off();
    drain();
    debug!(
        target: "serial",
        "input on ISA IRQ {} (GSI {}, vector {:#04x})",
        route.irq(),
        route.gsi(),
        route.vector()
    );
}

/// IRQ 4 (interrupts off, the boot CPU).
fn on_irq() {
    drain();
}

/// Move the waiting bytes into the queue while it has room (the rest stay
/// in the UART), then wake the reader if any came. With interrupts off, on
/// the boot CPU: the queue's only producer.
fn drain() {
    debug_assert!(!arch::irq::enabled() && arch::cpu_index() == 0, "the serial queue's producer is the boot CPU");
    let room = QUEUE_SIZE - QUEUE.len();
    if arch::serial::drain(room, |b| {
        let queued = QUEUE.push(b);
        debug_assert!(queued, "drained only as much as the queue has room for");
    }) > 0
    {
        WAITING.wake_all();
    }
}

/// The next received byte, if one is waiting: the queue's, else the UART's
/// (the module doc). Never blocks. The serial shell's only (the queue's
/// one consumer), on the boot CPU.
pub fn read_byte() -> Option<u8> {
    debug_assert!(arch::cpu_index() == 0, "the serial queue's consumer runs on the boot CPU");
    let _off = arch::irq::push_off();
    QUEUE.pop().or_else(arch::serial::try_read_byte)
}

/// Wait for a received byte (see [`read_byte`]): block until the handler
/// queues one or the UART holds one; without the interrupt, yield to the
/// CPU's other threads once (the caller polls). Only where a thread can
/// block.
pub fn wait() {
    if BY_INTERRUPT.load(Ordering::Acquire) {
        WAITING.wait_until(|| !QUEUE.is_empty() || arch::serial::data_ready());
    } else {
        crate::sched::yield_now();
    }
}
