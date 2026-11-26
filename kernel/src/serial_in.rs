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
//! - **Reading** ([`read_byte`], on any CPU, serialized with IRQ 4
//!   by the interrupts-off receive lock): the
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
//!   later). Three of the latter in a row, with no wait an interrupt ended
//!   between, warn once (`lumen_uart::MissedInterrupts`). Input already
//!   there at the wait's first look, before it blocks, is none of these
//!   (`WaitEnd::Idle`), and neither is a wait that found nothing; a line
//!   that arrives while the shell is still busy is read before it waits at
//!   all, with no wait to count.
//! - **How it waits** (`lumen_uart::wait_mode`): with the interrupt, block
//!   with the deadline; without a UART, block for good; either only where
//!   the thread can block (not before the boot CPU's scheduler runs);
//!   otherwise, or without the interrupt (no route, or one refused), yield
//!   once per look: the shell polls, as it used to.
// The test kernel runs no shell; its receive test supplies queued bytes.
#![cfg_attr(feature = "ktest", allow(dead_code))]

use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use log::{debug, warn};
use lumen_queue::ByteQueue;
use lumen_uart::{MissedInterrupts, WaitEnd, WaitMode};
use lumen_vfs::{PollGuard, PollMask, PollWaker};

use crate::arch;
use crate::arch::apic::{Polarity, Trigger};
use crate::interrupts;
use crate::sched::WaitQueue;
use crate::shell::Ctx;
use crate::sync::SpinLock;
use crate::vfs::KernelPollHead;

/// The ISA IRQ COM1 interrupts on.
const COM1_IRQ: u8 = 4;
/// Bytes the queue holds (a pasted line or two ahead of the shell).
const QUEUE_SIZE: usize = 256;
/// The longest the serial shell blocks between looks at the UART: the
/// safety net for an interrupt that doesn't come. (The CPU halts between.)
const WAIT_NS: u64 = 100_000_000;

/// Received bytes passed from the interrupt handler to [`read_byte`].
/// Every push and pop holds `RECEIVE`, which serializes access across CPUs
/// and also protects the UART fallback and interrupt drain.
static QUEUE: ByteQueue<QUEUE_SIZE> = ByteQueue::new();
/// Serializes queue consumption, UART reads and IRQ 4 drains. Never held
/// while waking waiters or notifying poll subscribers (their readiness
/// callbacks may take this lock).
static RECEIVE: SpinLock<()> = SpinLock::new(());
/// The serial shell waits here for a byte.
static WAITING: WaitQueue = WaitQueue::new();
/// Readiness subscribers waiting on `/dev/console` serial input (`poll`/`select`,
/// B3; replaced by `Tty` in C10).
static POLL_HEAD: SpinLock<Option<KernelPollHead>> = SpinLock::new(None);
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
/// any (or one having, without waking the reader).
static AT_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// `poll`/`select` waits on `/dev/console` that found queued input on the
/// 100 ms safety step without an IRQ 4 `PollHead::notify` wake.
static POLL_AT_SAFETY_STEP: AtomicU64 = AtomicU64::new(0);
/// `poll`/`select` waits on `/dev/console` woken by IRQ 4's `PollHead::notify`.
static POLL_WOKEN: AtomicU64 = AtomicU64::new(0);
/// Sequence bracketing [`queue_waiting`]'s drain-to-notify section (odd while
/// the boot-CPU handler is between pushing into [`QUEUE`] and completing
/// `POLL_HEAD.notify`; even otherwise).
static POLL_NOTIFY_SEQ: AtomicU64 = AtomicU64::new(0);
/// When those are worth a warning (the serial shell's only).
static MISSED: SpinLock<MissedInterrupts> = SpinLock::new(MissedInterrupts::new());

/// Route IRQ 4 and have the UART interrupt when bytes wait (the module
/// doc). On the boot CPU, once. Without a usable route the shell polls;
/// without a UART it blocks for good where it can block, else yields.
pub fn init() {
    // Initialize `POLL_HEAD` eagerly (outside any lock) before any early
    // return so `subscribe_poll` never hits its lazy allocation path in the
    // shell kernel.
    let _ = ensure_poll_head();
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
/// in the UART), then wake the serial shell's wait queue (counting a wake that
/// readied it) and notify [`POLL_HEAD`] subscribers with `POLLIN | POLLRDNORM`;
/// returns how many bytes were queued. Every push holds `RECEIVE`, as does
/// every pop by a reader; the lock is released before wake/notify.
fn queue_waiting() -> usize {
    let receive = RECEIVE.lock();
    let room = QUEUE_SIZE - QUEUE.len();
    if room == 0 {
        return 0;
    }
    POLL_NOTIFY_SEQ.fetch_add(1, Ordering::SeqCst);
    let queued = arch::serial::drain(room, |b| {
        let fit = QUEUE.push(b);
        debug_assert!(fit, "drained only as much as the queue has room for");
    });
    drop(receive);
    if queued > 0 {
        if WAITING.wake_all() > 0 {
            READER_WOKEN.fetch_add(1, Ordering::Relaxed);
        }
        // Clone the `Arc`-backed head out and release `POLL_HEAD` before
        // taking the head's level-10e subscriber lock in `notify`. Because
        // `POLL_HEAD` retains its own clone, dropping `head` here never frees.
        let head = POLL_HEAD.lock().clone();
        if let Some(head) = head {
            head.notify(PollMask::IN | PollMask::RDNORM);
        }
    }
    POLL_NOTIFY_SEQ.fetch_add(1, Ordering::SeqCst);
    queued
}

fn ensure_poll_head() -> KernelPollHead {
    if let Some(head) = POLL_HEAD.lock().clone() {
        return head;
    }
    // Allocate outside `POLL_HEAD`'s spinlock so no heap allocation runs under it.
    let fresh = KernelPollHead::new();
    let (head, unused) = {
        let mut slot = POLL_HEAD.lock();
        if let Some(existing) = slot.as_ref() {
            (existing.clone(), Some(fresh))
        } else {
            *slot = Some(fresh.clone());
            (fresh, None)
        }
    };
    // Drop any losing allocation after releasing `POLL_HEAD` so deallocation
    // never runs under the spinlock.
    drop(unused);
    head
}

/// Subscribe `waker` to serial input arrivals until the returned [`PollGuard`] drops.
pub fn subscribe_poll(waker: Arc<dyn PollWaker>) -> PollGuard {
    ensure_poll_head().subscribe(waker)
}

/// Whether COM1 input arrives via IRQ 4 (else `poll` falls back to short timed rechecks).
pub fn by_interrupt() -> bool {
    BY_INTERRUPT.load(Ordering::Acquire)
}

/// Number of active `PollHead` subscribers on serial input.
pub fn poll_subscribers() -> usize {
    let head = POLL_HEAD.lock().clone();
    head.map_or(0, |h| h.subscriber_count())
}

/// Whether a byte waits, serialized with the handler's drain and other
/// readers. Safe from wait-queue readiness callbacks on any CPU.
pub fn available() -> bool {
    let _receive = RECEIVE.lock();
    arch::serial::available(!QUEUE.is_empty())
}

/// The next received byte, queue first and then UART. Never blocks. The
/// receive lock serializes multiple CPUs with each other and IRQ 4, so
/// only one consumer advances the queue or reads the UART at a time.
pub fn read_byte() -> Option<u8> {
    let _receive = RECEIVE.lock();
    let popped = QUEUE.pop();
    arch::serial::read_next(popped)
}

/// Wait for a received byte (see [`read_byte`]; the caller looks again
/// afterwards), as `lumen_uart::wait_mode` says: block with the deadline,
/// block for good, or yield once. Safe for a reader on any CPU; another
/// reader may consume the byte before the caller runs, so it rechecks.
pub fn wait() {
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

/// Classify a `/dev/console` `poll`/`select` wait that ended on the 100 ms
/// IRQ 4 safety step and then found the console ready: counts a hit (and warns
/// once) only when input is already in [`QUEUE`] (the IRQ handler ran and
/// completed its `PollHead::notify` pass) while `fired()` is still false.
/// Input sitting in the UART alone (FIFO trigger / character-timeout window)
/// or a concurrent handler between push and notify is a late IRQ, not a lost
/// one.
pub fn note_poll_safety_step(fired: impl Fn() -> bool) {
    let s1 = POLL_NOTIFY_SEQ.load(Ordering::SeqCst);
    if (s1 & 1) != 0 || QUEUE.is_empty() || fired() {
        return;
    }
    if POLL_NOTIFY_SEQ.load(Ordering::SeqCst) != s1 {
        return;
    }
    if POLL_AT_SAFETY_STEP.fetch_add(1, Ordering::Relaxed) == 0 {
        warn!(
            target: "serial",
            "COM1 poll found queued input at the 100 ms safety step without IRQ 4 waking it (GSI {}, vector {:#04x})",
            GSI.load(Ordering::Relaxed),
            VECTOR.load(Ordering::Relaxed)
        );
    }
}

/// Record a `/dev/console` `poll`/`select` wait woken by IRQ 4's
/// `PollHead::notify`.
pub fn note_poll_woken() {
    POLL_WOKEN.fetch_add(1, Ordering::Relaxed);
}

/// `serial [poll]`: how COM1's input comes, and how the serial shell's waits
/// for it ended (`serial poll` reports `poll`/`select` safety-step hits,
/// IRQ-woken waits, and active `PollHead` subscribers).
pub fn serial_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if args.len() == 1 && args[0] == "poll" {
        let mut t = String::new();
        let _ = writeln!(
            t,
            "serial: poll safety-step hits: {}, woken: {}, subscribers: {}",
            POLL_AT_SAFETY_STEP.load(Ordering::Relaxed),
            POLL_WOKEN.load(Ordering::Relaxed),
            poll_subscribers()
        );
        let _ = ctx.out.write_str(&t);
        return;
    }
    if !args.is_empty() {
        return ctx.fail("usage: serial [poll]");
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

crate::ktest!(
    fn reads_queued_input_on_non_boot_cpu() {
        use crate::{kassert, kassert_eq, sched};
        let cpu = sched::running_cpus().into_iter().find(|&cpu| cpu != 0).ok_or("needs an AP")?;
        const INPUT: &[u8; 4] = b"B6\r\n";
        {
            let _receive = RECEIVE.lock();
            kassert!(QUEUE.is_empty());
            for &byte in INPUT {
                kassert!(QUEUE.push(byte));
            }
        }
        let result = Arc::new(SpinLock::new(None));
        let out = result.clone();
        let h = sched::spawn_on(cpu, "serial-reader", move || {
            wait(); // the test kernel uses the polled path, on this AP
            let ready = available();
            let bytes = core::array::from_fn::<_, 4, _>(|_| read_byte());
            // Exercise the UART fallback too, after consuming the queue.
            let empty = read_byte();
            *out.lock() = Some((arch::cpu_index(), ready, bytes, empty));
        })
        .map_err(|e| {
            crate::ktest::diag(format_args!("serial reader: {e}"));
            "spawn serial reader"
        })?;
        sched::join_bounded(h)?;
        let (ran, ready, bytes, empty) = result.lock().take().ok_or("reader result")?;
        kassert_eq!(ran, cpu);
        kassert!(ready);
        kassert_eq!(bytes, INPUT.map(Some));
        kassert_eq!(empty, None);
        kassert!(QUEUE.is_empty());
        Ok(())
    }
);
