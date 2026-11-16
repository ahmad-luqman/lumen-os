//! Serial input by interrupt (#120): COM1's received bytes come in on ISA
//! IRQ 4 and wait in a queue the serial shell reads, so the shell blocks
//! for a key instead of polling, and an idle boot CPU halts. (Polling kept
//! it 100% busy; under TCG on a loaded host such a vCPU was measured
//! going without interrupts for longer than the sound driver's 160 ms
//! queue. That the host descheduled it is inferred.)
//!
//! - **Set up** ([`init`], once, on the boot CPU, before the serial shell
//!   starts; not in the test kernel, whose loopback ktest reads the UART
//!   itself): IRQ 4 is routed masked, the UART interrupts when bytes wait
//!   (`arch::serial::enable_rx_interrupt`: at 14 bytes or after a
//!   character timeout), the input is unmasked, and the bytes that came
//!   before are drained. A level-triggered route (a MADT override) isn't
//!   used: the handler leaves bytes in the UART when the queue is full,
//!   which would raise the interrupt again after every EOI.
//! - **Interrupt:** moves the waiting bytes into [`QUEUE`] while it has
//!   room, and wakes the reader. Bytes past its room stay in the UART
//!   (QEMU then stops feeding it; a real UART overruns and drops the next
//!   ones, unseen).
//! - **Reading** ([`read_byte`], the serial shell on the boot CPU's `main`
//!   thread, with interrupts off so the handler can't run in between): the
//!   queue first, then the UART, for bytes the handler left there. The
//!   interrupt is an edge, and no new one comes until the reader lowers
//!   the line by taking them, so [`wait`] blocks only while neither holds a
//!   byte (`lumen_uart`'s module doc, "Reading by interrupt"; host-tested
//!   against a model whose handler runs only on an edge).
//! - **Safety net:** the wait has a deadline ([`WAIT_NS`]), so input whose
//!   interrupt never comes is still read, late. Found that way while IRQ 4
//!   has never come, it warns once (a wrong route, or OUT2 ignored).
//! - **Without the interrupt** (no route, a level-triggered one, or a CPU
//!   whose scheduler doesn't run yet, where a thread can't block), the
//!   shell polls, yielding, as before; without a UART it blocks for good.
// (The test kernel runs no shell: nothing here is called there.)
#![cfg_attr(feature = "ktest", allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{debug, warn};
use lumen_queue::ByteQueue;

use crate::arch;
use crate::interrupts;
use crate::sched::WaitQueue;

/// The ISA IRQ COM1 interrupts on.
const COM1_IRQ: u8 = 4;
/// Bytes the queue holds (a pasted line or two ahead of the shell).
const QUEUE_SIZE: usize = 256;
/// The longest the serial shell blocks between looks at the UART: the
/// safety net for an interrupt that doesn't come. (The CPU halts between.)
const WAIT_NS: u64 = 100_000_000;
/// Deadline wakes that found input, with IRQ 4 never taken, before the
/// warning: one could be a race with a late interrupt (QEMU's character
/// timeout runs on its main loop), several in a row with none ever taken
/// can't.
const MISSED_BEFORE_WARNING: u64 = 3;

/// Received bytes, from the interrupt handler (the producer: the boot CPU
/// with interrupts off) to [`read_byte`] (the consumer: the serial shell).
static QUEUE: ByteQueue<QUEUE_SIZE> = ByteQueue::new();
/// The serial shell waits here for a byte.
static WAITING: WaitQueue = WaitQueue::new();
/// Whether IRQ 4 brings the bytes (else the shell polls).
static BY_INTERRUPT: AtomicBool = AtomicBool::new(false);
/// IRQ 4 interrupts taken.
static IRQS: AtomicU64 = AtomicU64::new(0);
/// Deadline wakes that found input with no interrupt taken during the wait.
static MISSED: AtomicU64 = AtomicU64::new(0);

/// Route IRQ 4 and have the UART interrupt when bytes wait (the module
/// doc). On the boot CPU, once; without a UART or a usable route the shell
/// polls.
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
    if route.level_triggered() {
        // Left routed and masked.
        warn!(target: "serial", "serial input is polled: IRQ {COM1_IRQ} is level-triggered (GSI {})", route.gsi());
        return;
    }
    arch::serial::enable_rx_interrupt();
    BY_INTERRUPT.store(true, Ordering::Release);
    interrupts::set_masked(route, false);
    {
        // A byte that came before the input was unmasked raised its edge
        // while it was masked: take it now (no new edge comes while it
        // waits).
        let _off = arch::irq::push_off();
        drain();
    }
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
    IRQS.fetch_add(1, Ordering::Relaxed);
    drain();
}

/// Move the waiting bytes into the queue while it has room (the rest stay
/// in the UART), then wake the reader if any came. With interrupts off, on
/// the boot CPU: the queue's only producer.
fn drain() {
    debug_assert!(!arch::irq::enabled() && arch::cpu_index() == 0, "the serial queue's producer is the boot CPU");
    let room = QUEUE_SIZE - QUEUE.len();
    let queued = arch::serial::drain(room, |b| {
        let fit = QUEUE.push(b);
        debug_assert!(fit, "drained only as much as the queue has room for");
    });
    if queued > 0 {
        WAITING.wake_all();
    }
}

/// Whether a byte waits for [`read_byte`], in the queue or the UART.
fn available() -> bool {
    arch::serial::available(!QUEUE.is_empty())
}

/// The next received byte, if one is waiting: the queue's, else the UART's
/// (the module doc). Never blocks. The serial shell's only (the queue's
/// one consumer), on the boot CPU.
pub fn read_byte() -> Option<u8> {
    debug_assert!(arch::cpu_index() == 0, "the serial queue's consumer runs on the boot CPU");
    let _off = arch::irq::push_off();
    arch::serial::read_next(|| QUEUE.pop())
}

/// Wait for a received byte (see [`read_byte`]; the caller looks again
/// afterwards). With the interrupt, where the thread can block: until a
/// byte waits, or at most [`WAIT_NS`]. Without a UART: for good. Otherwise
/// yield to the CPU's other threads once (the caller polls).
pub fn wait() {
    let can_block = crate::sched::can_block_now();
    if !arch::serial::present() && can_block {
        // Nothing will ever come.
        WAITING.wait_until(|| false);
    } else if BY_INTERRUPT.load(Ordering::Acquire) && can_block {
        let irqs = IRQS.load(Ordering::Relaxed);
        let deadline = crate::time::now_ns() + WAIT_NS;
        let found = WAITING.wait_until_or_deadline(deadline, available);
        if found && IRQS.load(Ordering::Relaxed) == irqs && crate::time::now_ns() >= deadline {
            note_missed();
        }
    } else {
        crate::sched::yield_now();
    }
}

/// A deadline wake found input with no interrupt during the wait: warn
/// once if it keeps happening while IRQ 4 has never come.
fn note_missed() {
    let missed = MISSED.fetch_add(1, Ordering::Relaxed) + 1;
    if missed == MISSED_BEFORE_WARNING && IRQS.load(Ordering::Relaxed) == 0 {
        warn!(target: "serial", "COM1 input found without its interrupt; check IRQ {COM1_IRQ}'s routing");
    }
}
