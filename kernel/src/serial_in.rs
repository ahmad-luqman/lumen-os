//! Serial input by interrupt (#120): COM1's received bytes come in on ISA
//! IRQ 4 and wait in a queue the serial shell reads, so the shell blocks
//! for a key instead of polling, and an idle boot CPU halts. (Polling kept
//! it 100% busy; under TCG on a loaded host such a vCPU was measured
//! going without interrupts for longer than the sound driver's 160 ms
//! queue. That the host descheduled it is inferred.)
//!
//! - **Set up** ([`init`], once, on the boot CPU, before the serial shell
//!   starts; not in the test kernel, whose ktests read COM1 themselves):
//!   IRQ 4 is routed masked, the UART interrupts when bytes wait
//!   (`arch::serial::enable_rx_interrupt`: at 14 bytes or after a
//!   character timeout), the input is unmasked, and the bytes that came
//!   before are queued. A level-triggered or active-low route (a MADT
//!   override) isn't used: the handler leaves bytes in the UART when the
//!   queue is full, which a level input would raise again after every EOI;
//!   and an active-low override is unexpected for IRQ 4 (ISA's are edge,
//!   active high), so it isn't trusted. (An override's polarity describes
//!   the pin after any inversion on the board, not the UART's own line.)
//! - **Interrupt:** moves the waiting bytes into [`QUEUE`] while it has
//!   room, and wakes the reader. Bytes past its room stay in the UART
//!   (QEMU holds the rest back; a real UART overruns and drops them).
//! - **Reading** ([`read_byte`], the serial shell on the boot CPU's `main`
//!   thread, with interrupts off so the handler can't run in between): the
//!   queue first, then the UART, for bytes the handler left there. In the
//!   worst case the design guards against, the interrupt is an edge and no
//!   new one comes until the reader lowers the line by taking them, so
//!   [`wait`] blocks only while neither holds a byte (`lumen_uart`'s crate
//!   doc, "Reading by interrupt"; host-tested against a model of that
//!   case).
//! - **Safety net:** the wait has a deadline ([`WAIT_NS`]), so input whose
//!   interrupt doesn't wake the shell (the UART's interrupt not reaching
//!   IRQ 4, or a handler that doesn't wake it) is still read, within 200
//!   ms: input found at the deadline in the UART only, nothing queued, gets
//!   one more period for a late interrupt to queue it (a race measured
//!   under load: QEMU's character timeout 1.6 ms after the deadline). Each
//!   wait's end is counted (`serial` shows them): woken by the handler;
//!   readied by the deadline first, with an interrupt bringing the input
//!   after it but before the reader ran (another thread on the CPU, or the
//!   vCPU, ran meanwhile: common under load during a sound); or input
//!   found at the deadline (with no interrupt, or one before the deadline
//!   that didn't wake it; if only in the UART, still there a period
//!   later). Three of the latter in a row, with no interrupt
//!   bringing input between, warn once (`lumen_uart::MissedInterrupts`).
//!   Input already there when the shell looks, before it blocks (or before
//!   it waits at all: a line that arrives while it is still busy), is none
//!   of these (`WaitEnd::Idle`), and neither is a wait that found nothing.
//! - **How it waits** (`lumen_uart::wait_mode`): with the interrupt, block
//!   with the deadline; without a UART, block for good; either only where
//!   the thread can block (not before the boot CPU's scheduler runs);
//!   otherwise, or without the interrupt (no route, or one refused), yield
//!   once per look: the shell polls, as it used to.
// (The test kernel runs no shell: nothing here is called there.)
#![cfg_attr(feature = "ktest", allow(dead_code))]

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use log::{debug, warn};
use lumen_queue::ByteQueue;
use lumen_uart::{MissedInterrupts, WaitEnd, WaitMode};

use crate::arch;
use crate::arch::apic::{Polarity, Trigger};
use crate::interrupts;
use crate::sched::WaitQueue;
use crate::shell::Ctx;
use crate::sync::SpinLock;

/// The ISA IRQ COM1 interrupts on.
const COM1_IRQ: u8 = 4;
/// Bytes the queue holds (a pasted line or two ahead of the shell).
const QUEUE_SIZE: usize = 256;
/// The longest the serial shell blocks between looks at the UART: the
/// safety net for an interrupt that doesn't come. (The CPU halts between.)
const WAIT_NS: u64 = 100_000_000;

/// Received bytes, from the interrupt handler (the producer: the boot CPU
/// with interrupts off) to [`read_byte`] (the consumer: the serial shell).
static QUEUE: ByteQueue<QUEUE_SIZE> = ByteQueue::new();
/// The serial shell waits here for a byte.
static WAITING: WaitQueue = WaitQueue::new();
/// Whether IRQ 4 brings the bytes (else the shell polls).
static BY_INTERRUPT: AtomicBool = AtomicBool::new(false);
/// IRQ 4's GSI and vector, once routed (for `serial` and the warning).
static GSI: AtomicU32 = AtomicU32::new(0);
static VECTOR: AtomicU8 = AtomicU8::new(0);
/// IRQ 4 interrupts that queued input.
static INPUT_IRQS: AtomicU64 = AtomicU64::new(0);
/// Times the handler woke the waiting reader (it was blocked, and this
/// wake, not its deadline, readied it: `WaitQueue::wake_all`'s count).
static READER_WOKEN: AtomicU64 = AtomicU64::new(0);
/// When (`time::now_ns`) the first interrupt that brought input since the
/// reader last blocked came (`u64::MAX`: none yet).
static FIRST_INPUT_IRQ_NS: AtomicU64 = AtomicU64::new(u64::MAX);
/// Waits woken by an interrupt that brought input.
static WOKEN: AtomicU64 = AtomicU64::new(0);
/// Waits readied by their deadline, whose input an interrupt then brought
/// before the reader ran (it was no longer blocked, so the wake didn't
/// count).
static LATE: AtomicU64 = AtomicU64::new(0);
/// Waits that found input at their deadline, no interrupt having brought
/// any.
static AT_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// When those are worth a warning (the serial shell's only).
static MISSED: SpinLock<MissedInterrupts> = SpinLock::new(MissedInterrupts::new());

/// Route IRQ 4 and have the UART interrupt when bytes wait (the module
/// doc). On the boot CPU, once. Without a usable route the shell polls;
/// without a UART it blocks for good where it can block, else yields.
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
    GSI.store(route.gsi(), Ordering::Relaxed);
    VECTOR.store(route.vector(), Ordering::Relaxed);
    if route.trigger() == Trigger::Level || route.polarity() == Polarity::ActiveLow {
        // Left routed and masked. A level input would storm while bytes
        // wait; an active-low one is unexpected for IRQ 4, so not trusted.
        warn!(
            target: "serial",
            "serial input is polled: IRQ {COM1_IRQ} is {:?}, {:?} (GSI {}), not ISA's edge, active high",
            route.trigger(),
            route.polarity(),
            route.gsi()
        );
        return;
    }
    arch::serial::enable_rx_interrupt();
    BY_INTERRUPT.store(true, Ordering::Release);
    interrupts::set_masked(route, false);
    {
        // A byte that came before the input was unmasked raised its edge
        // while it was masked: take it now (no new edge may come while it
        // waits).
        let _off = arch::irq::push_off();
        queue_waiting();
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
    if queue_waiting() > 0 {
        INPUT_IRQS.fetch_add(1, Ordering::Relaxed);
        FIRST_INPUT_IRQ_NS.fetch_min(crate::time::now_ns(), Ordering::Relaxed);
    }
}

/// Move the waiting bytes into the queue while it has room (the rest stay
/// in the UART), then wake the reader if any came (counting a wake that
/// readied it); how many. With interrupts off, on the boot CPU: the
/// queue's only producer.
fn queue_waiting() -> usize {
    let room = QUEUE_SIZE - QUEUE.len();
    let queued = arch::serial::drain(room, |b| {
        let fit = QUEUE.push(b);
        debug_assert!(fit, "drained only as much as the queue has room for");
    });
    if queued > 0 && WAITING.wake_all() > 0 {
        READER_WOKEN.fetch_add(1, Ordering::Relaxed);
    }
    queued
}

/// Whether a byte waits for [`read_byte`], in the queue or the UART.
fn available() -> bool {
    arch::serial::available(!QUEUE.is_empty())
}

/// The next received byte, if one is waiting: the queue's, else the UART's
/// (the module doc). Never blocks. The serial shell's only (the queue's
/// one consumer), on the boot CPU.
pub fn read_byte() -> Option<u8> {
    let _off = arch::irq::push_off();
    let popped = QUEUE.pop();
    arch::serial::read_next(popped)
}

/// Wait for a received byte (see [`read_byte`]; the caller looks again
/// afterwards), as `lumen_uart::wait_mode` says: block with the deadline,
/// block for good, or yield once. The serial shell's only, on the boot
/// CPU.
pub fn wait() {
    debug_assert!(arch::cpu_index() == 0, "the serial shell waits on the boot CPU");
    let present = arch::serial::present();
    let by_interrupt = BY_INTERRUPT.load(Ordering::Acquire);
    match lumen_uart::wait_mode(present, by_interrupt, crate::sched::can_block_now()) {
        WaitMode::Forever => WAITING.wait_until(|| false),
        WaitMode::Deadline => wait_with_deadline(),
        WaitMode::Yield => crate::sched::yield_now(),
    }
}

/// Block until a byte waits, or at most [`WAIT_NS`]; count how the wait
/// ended, and warn once if input keeps being found at the deadline.
fn wait_with_deadline() {
    let woken_before = READER_WOKEN.load(Ordering::Relaxed);
    FIRST_INPUT_IRQ_NS.store(u64::MAX, Ordering::Relaxed);
    let deadline = crate::time::now_ns() + WAIT_NS;
    let found = WAITING.wait_until_or_deadline(deadline, available);
    let woken = || READER_WOKEN.load(Ordering::Relaxed) != woken_before;
    let past_deadline = crate::time::now_ns() >= deadline;
    if lumen_uart::another_period(found, woken(), !QUEUE.is_empty(), past_deadline) {
        // Input in the UART alone at the deadline, its interrupt not taken:
        // it may be on its way (on a loaded TCG host QEMU's character
        // timeout was measured arriving 1.6 ms after such a deadline). One
        // more period for the handler to queue it; if it doesn't, the input
        // waited a whole period without its interrupt.
        let grace = crate::time::now_ns() + WAIT_NS;
        let _ = WAITING.wait_until_or_deadline(grace, || !QUEUE.is_empty());
    }
    let first = FIRST_INPUT_IRQ_NS.load(Ordering::Relaxed);
    let end =
        lumen_uart::classify(woken(), (first != u64::MAX).then_some(first), deadline, found, crate::time::now_ns());
    match end {
        WaitEnd::Woken => WOKEN.fetch_add(1, Ordering::Relaxed),
        WaitEnd::Late => LATE.fetch_add(1, Ordering::Relaxed),
        WaitEnd::InputAtDeadline => AT_DEADLINE.fetch_add(1, Ordering::Relaxed),
        WaitEnd::Idle => 0,
    };
    if MISSED.lock().note(end) {
        warn!(
            target: "serial",
            "COM1 input found without its interrupt {} times in a row; check IRQ {COM1_IRQ}'s routing (GSI {}, vector {:#04x}): input is read late (within 200 ms) instead",
            MissedInterrupts::THRESHOLD,
            GSI.load(Ordering::Relaxed),
            VECTOR.load(Ordering::Relaxed)
        );
    }
}

/// `serial`: how COM1's input comes, and how the serial shell's waits for
/// it ended.
pub fn serial_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: serial");
    }
    let mut t = String::new();
    if !arch::serial::present() {
        t.push_str("serial: no UART at COM1\n");
    } else if BY_INTERRUPT.load(Ordering::Acquire) {
        let _ = writeln!(
            t,
            "serial: COM1 input by interrupt (ISA IRQ {COM1_IRQ}, GSI {}, vector {:#04x}): {} interrupts brought input; waits woken by one: {}, readied by the deadline first: {}, input found at the deadline: {}",
            GSI.load(Ordering::Relaxed),
            VECTOR.load(Ordering::Relaxed),
            INPUT_IRQS.load(Ordering::Relaxed),
            WOKEN.load(Ordering::Relaxed),
            LATE.load(Ordering::Relaxed),
            AT_DEADLINE.load(Ordering::Relaxed)
        );
    } else {
        t.push_str("serial: COM1 input polled (no interrupt)\n");
    }
    let _ = ctx.out.write_str(&t);
}
