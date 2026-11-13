//! The PS/2 keyboard: the 8042 controller's first port, through the I/O
//! APIC (ISA IRQ 1).
//!
//! [`init`] (boot CPU, interrupts on, after the scheduler) routes IRQ 1 first
//! (its handler registered, the input left masked; without an I/O APIC
//! there is nothing to set up), then sets the controller up with its
//! interrupts off and port 1's clock off, polling every reply with a
//! deadline: both ports disabled, the output buffer flushed, the
//! configuration byte rewritten (both IRQs off, port 1's clock off,
//! translation to scancode set 1 on; the rest as firmware left it), the
//! controller's self-test, the configuration written again (a self-test may
//! reset it), port 1's interface test, port 1 enabled, scanning disabled,
//! the keyboard reset and scanning disabled again (a reset turns it back
//! on), scancode set 2 selected (the 8042 translates it to set 1) and
//! scanning enabled. A byte that isn't the awaited reply (a key pressed
//! during boot) is skipped, a bounded number of times. Last, port 1's clock
//! goes on with its IRQ bit, the input is unmasked and the output buffer is
//! drained once: a byte that was already waiting raised IRQ 1's edge while
//! the input was masked, and the I/O APIC doesn't keep a masked edge.
//!
//! The interrupt handler only moves bytes from the controller into a
//! lock-free single-producer, single-consumer queue (`lumen_queue::ByteQueue`;
//! the producer is always the boot CPU, which every I/O APIC interrupt goes
//! to, with interrupts off); bytes from the second port (a mouse) and bytes
//! the controller flags with a parity or timeout error are discarded (the
//! latter counted). Readers ([`read_char`]) decode them with the
//! `pc-keyboard` crate under a lock, in the layout `lumen.keymap=` names
//! (`us` without one, or for a name it doesn't know, with a warning).
//! kmon's `kbd` shows how it went ([`stats`]).

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{debug, info, warn};
use lumen_queue::ByteQueue;
use pc_keyboard::layouts::{
    AnyLayout, Azerty, Colemak, De105Key, Dvorak104Key, FiSe105Key, Jis109Key, No105Key, Uk105Key, Us104Key,
};
use pc_keyboard::{DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};

use crate::arch::i8042;
use crate::interrupts::{self, Route};
use crate::sync::SpinLock;
use crate::{arch, cmdline, time};

/// A layout's name for `lumen.keymap=`, and how to make it.
type NamedLayout = (&'static str, fn() -> AnyLayout);

/// The layouts `lumen.keymap=` can name; the first is the default.
const LAYOUTS: [NamedLayout; 9] = [
    ("us", || AnyLayout::Us104Key(Us104Key)),
    ("uk", || AnyLayout::Uk105Key(Uk105Key)),
    ("de", || AnyLayout::De105Key(De105Key)),
    ("fr", || AnyLayout::Azerty(Azerty)),
    ("no", || AnyLayout::No105Key(No105Key)),
    ("se", || AnyLayout::FiSe105Key(FiSe105Key)),
    ("jp", || AnyLayout::Jis109Key(Jis109Key)),
    ("dvorak", || AnyLayout::Dvorak104Key(Dvorak104Key)),
    ("colemak", || AnyLayout::Colemak(Colemak)),
];

type Decoder = PS2Keyboard<AnyLayout, ScancodeSet1>;

/// Bytes from the keyboard, waiting to be decoded.
static QUEUE: ByteQueue<256> = ByteQueue::new();
/// The decoder and its layout's name (the queue's consumer side is used
/// only under this lock); `None` before [`init`].
static DECODER: SpinLock<Option<(Decoder, &'static str)>> = SpinLock::new(None);
/// How [`init`] went; `None` before it.
static STATUS: SpinLock<Option<Result<Route, InitError>>> = SpinLock::new(None);
/// Keyboard interrupts taken.
static IRQS: AtomicU64 = AtomicU64::new(0);
/// Bytes put on the queue.
static BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes the controller flagged with a parity or timeout error (discarded).
static LINE_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Bytes the decoder refused (the keyboard's own overrun code 0xff among
/// them: keys it lost).
static UNDECODED: AtomicU64 = AtomicU64::new(0);

// Controller commands (port 0x64).
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const DISABLE_PORT2: u8 = 0xa7;
const SELF_TEST: u8 = 0xaa;
const TEST_PORT1: u8 = 0xab;
const DISABLE_PORT1: u8 = 0xad;
const ENABLE_PORT1: u8 = 0xae;
// Configuration byte bits.
const PORT1_IRQ: u8 = 1 << 0;
const PORT2_IRQ: u8 = 1 << 1;
const PORT1_CLOCK_OFF: u8 = 1 << 4;
const TRANSLATE: u8 = 1 << 6;
// Replies.
const SELF_TEST_PASSED: u8 = 0x55;
const PORT_TEST_PASSED: u8 = 0x00;
// Keyboard commands (port 0x60) and its replies.
const KBD_RESET: u8 = 0xff;
const KBD_SCANCODE_SET: u8 = 0xf0;
const KBD_ENABLE_SCANNING: u8 = 0xf4;
const KBD_DISABLE_SCANNING: u8 = 0xf5;
const KBD_ACK: u8 = 0xfa;
const KBD_RESEND: u8 = 0xfe;
const KBD_RESET_PASSED: u8 = 0xaa;

/// How long the controller may take to take a byte, or to answer.
const REPLY_NS: u64 = 100_000_000;
/// How long a keyboard may take to reset (its self-test).
const RESET_NS: u64 = 1_000_000_000;
/// Bytes that aren't the awaited reply (scancodes of keys pressed during
/// boot) skipped while waiting for one, at most.
const STRAYS: usize = 32;
/// Most bytes one interrupt moves (the controller holds one at a time; this
/// only bounds a misbehaving one).
const BYTES_PER_IRQ: usize = 16;

/// Why the keyboard isn't usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// IRQ 1 couldn't be routed (no I/O APIC, say).
    Route(interrupts::RouteError),
    /// The controller didn't take `command` (or its argument), or didn't
    /// answer it, in time (no controller, or no keyboard): what was being
    /// waited for.
    Timeout {
        command: u8,
        waiting_for: &'static str,
    },
    /// Only bytes that weren't the reply to `command` came (more than
    /// [`STRAYS`]).
    NoReply {
        command: u8,
    },
    SelfTest(u8),
    PortTest(u8),
    /// The keyboard asked for `command` again more than twice.
    Resends {
        command: u8,
    },
    Reset(u8),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            InitError::Route(e) => write!(f, "IRQ 1 can't be routed: {e}"),
            InitError::Timeout { command, waiting_for } => {
                write!(f, "no reply in time (command {command:#04x}: {waiting_for})")
            }
            InitError::NoReply { command } => write!(f, "command {command:#04x} got only stray bytes"),
            InitError::SelfTest(r) => write!(f, "controller self-test failed ({r:#04x})"),
            InitError::PortTest(r) => write!(f, "port 1 interface test failed ({r:#04x})"),
            InitError::Resends { command } => write!(f, "command {command:#04x} resent too often"),
            InitError::Reset(r) => write!(f, "keyboard reset failed ({r:#04x})"),
        }
    }
}

/// The layout `value` names (`lumen.keymap=`'s), and whether it was one we
/// know; `us` for no value or an unknown one.
fn choose_layout(value: Option<&str>) -> (&'static str, AnyLayout, bool) {
    match LAYOUTS.iter().find(|(name, _)| *name == value.unwrap_or("us")) {
        Some(&(name, layout)) => (name, layout(), true),
        None => (LAYOUTS[0].0, LAYOUTS[0].1(), false),
    }
}

fn new_decoder(layout: AnyLayout) -> Decoder {
    Decoder::new(ScancodeSet1::new(), layout, HandleControl::Ignore)
}

/// Set up the decoder in the layout `lumen.keymap=` names, then IRQ 1 and
/// the controller and keyboard (see the module docs). A missing or broken
/// keyboard is a warning, not a failure: the kernel runs without one.
pub fn init() {
    let asked = cmdline::value("lumen.keymap");
    let (name, layout, known) = choose_layout(asked);
    if !known {
        let names: alloc::vec::Vec<&str> = LAYOUTS.iter().map(|(n, _)| *n).collect();
        warn!(target: "kbd", "unknown keymap '{}' (known: {}); using us", asked.unwrap_or(""), names.join(" "));
    }
    *DECODER.lock() = Some((new_decoder(layout), name));
    let started = start();
    match started {
        Ok(route) => info!(
            target: "kbd",
            "PS/2 keyboard on ISA IRQ {} (GSI {}, vector {:#x}), layout {name}",
            route.irq(),
            route.gsi(),
            route.vector()
        ),
        Err(e) => warn!(target: "kbd", "no PS/2 keyboard: {e}"),
    }
    *STATUS.lock() = Some(started);
}

fn start() -> Result<Route, InitError> {
    // Routed first: without an I/O APIC there is no point resetting the
    // keyboard. (If a later step fails, IRQ 1 stays routed, masked.)
    let route = interrupts::route_isa_irq(1, on_irq).map_err(InitError::Route)?;
    let config = set_up_controller()?;
    controller_command_with(WRITE_CONFIG, (config & !PORT1_CLOCK_OFF) | PORT1_IRQ)?;
    enable(route);
    Ok(route)
}

/// Unmask IRQ 1's input, then drain the output buffer: a byte that arrived
/// before the input was unmasked raised IRQ 1's edge while it was masked,
/// and is still waiting (and holds the line high: no new edge would come).
fn enable(route: Route) {
    interrupts::set_masked(route, false);
    let _off = arch::irq::push_off();
    drain();
}

/// IRQ 1's route, once [`init`] has set the keyboard up.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn route() -> Option<Route> {
    STATUS.lock().and_then(Result::ok)
}

/// Wait until `ready(status)` or `ns` pass; whether it did (looked at once
/// more after the deadline: a thread preempted in between isn't failed for
/// it).
fn wait(ns: u64, ready: impl Fn(u8) -> bool) -> bool {
    let deadline = time::now_ns() + ns;
    while time::now_ns() <= deadline {
        if ready(i8042::status()) {
            return true;
        }
        core::hint::spin_loop();
    }
    ready(i8042::status())
}

/// Wait until the controller has taken the last byte written.
fn wait_input_empty(command: u8, waiting_for: &'static str) -> Result<(), InitError> {
    if wait(REPLY_NS, |s| s & i8042::INPUT_FULL == 0) {
        Ok(())
    } else {
        Err(InitError::Timeout { command, waiting_for })
    }
}

fn send_command(command: u8) -> Result<(), InitError> {
    wait_input_empty(command, "the controller to take it")?;
    i8042::write_command(command);
    Ok(())
}

/// Write `byte` to the data port (for the keyboard, or `command`'s argument).
fn send_data(command: u8, byte: u8) -> Result<(), InitError> {
    wait_input_empty(command, "the controller to take a byte")?;
    i8042::write_data(byte);
    Ok(())
}

fn controller_command_with(command: u8, arg: u8) -> Result<(), InitError> {
    send_command(command)?;
    send_data(command, arg)
}

/// The next byte from the controller within `ns`.
fn receive(command: u8, ns: u64, waiting_for: &'static str) -> Result<u8, InitError> {
    if !wait(ns, |s| s & i8042::OUTPUT_FULL != 0) {
        return Err(InitError::Timeout { command, waiting_for });
    }
    Ok(i8042::read_data())
}

/// Wait (`ns` in all) for the first byte `is_reply` accepts, skipping up to
/// [`STRAYS`] others (scancodes of keys pressed during boot).
fn await_reply(
    command: u8,
    ns: u64,
    waiting_for: &'static str,
    is_reply: impl Fn(u8) -> bool,
) -> Result<u8, InitError> {
    let deadline = time::now_ns() + ns;
    for _ in 0..=STRAYS {
        let left = deadline.saturating_sub(time::now_ns());
        let byte = receive(command, left, waiting_for)?;
        if is_reply(byte) {
            return Ok(byte);
        }
        debug!(target: "kbd", "skipped {byte:#04x} waiting for {waiting_for}");
    }
    Err(InitError::NoReply { command })
}

/// Run controller command `command` and check its one-byte reply is
/// `passed`, else fail with `failed(reply)`. (Port 1's clock is off, so no
/// scancode can come.)
fn controller_test(command: u8, passed: u8, failed: fn(u8) -> InitError) -> Result<(), InitError> {
    send_command(command)?;
    match receive(command, REPLY_NS, "the test's result")? {
        r if r == passed => Ok(()),
        r => Err(failed(r)),
    }
}

/// Send the keyboard `byte` (a command, or `command`'s argument) and wait
/// for its ACK (resending, at most twice, when it asks).
fn keyboard_command(command: u8, byte: u8) -> Result<(), InitError> {
    for _ in 0..3 {
        send_data(command, byte)?;
        let reply = await_reply(command, REPLY_NS, "the keyboard's ACK", |b| matches!(b, KBD_ACK | KBD_RESEND))?;
        if reply == KBD_ACK {
            return Ok(());
        }
    }
    Err(InitError::Resends { command })
}

/// The controller and keyboard, polled with their interrupts off (see the
/// module docs); the configuration byte, port 1's clock still off.
fn set_up_controller() -> Result<u8, InitError> {
    send_command(DISABLE_PORT1)?;
    send_command(DISABLE_PORT2)?;
    for _ in 0..64 {
        if i8042::status() & i8042::OUTPUT_FULL == 0 {
            break;
        }
        let _ = i8042::read_data();
    }
    if i8042::status() & i8042::OUTPUT_FULL != 0 {
        debug!(target: "kbd", "the output buffer didn't empty in 64 reads");
    }
    send_command(READ_CONFIG)?;
    let firmware = receive(READ_CONFIG, REPLY_NS, "the configuration byte")?;
    // Both IRQs off, port 1's clock off (as DISABLE_PORT1 left it),
    // translation to set 1 on; the rest (port 2's clock, the system flag)
    // as firmware left it.
    let config = (firmware | TRANSLATE | PORT1_CLOCK_OFF) & !(PORT1_IRQ | PORT2_IRQ);
    controller_command_with(WRITE_CONFIG, config)?;
    controller_test(SELF_TEST, SELF_TEST_PASSED, InitError::SelfTest)?;
    // Some controllers reset their configuration in the self-test.
    controller_command_with(WRITE_CONFIG, config)?;
    controller_test(TEST_PORT1, PORT_TEST_PASSED, InitError::PortTest)?;
    send_command(ENABLE_PORT1)?;
    keyboard_command(KBD_DISABLE_SCANNING, KBD_DISABLE_SCANNING)?;
    // Reset: an ACK and the self-test's result, in either order.
    send_data(KBD_RESET, KBD_RESET)?;
    let deadline = time::now_ns() + RESET_NS;
    let (mut acked, mut passed) = (false, false);
    while !(acked && passed) {
        let left = deadline.saturating_sub(time::now_ns());
        let reply = await_reply(KBD_RESET, left, "the keyboard to reset", |b| {
            matches!(b, KBD_ACK | KBD_RESET_PASSED) || (0xfc..=0xfd).contains(&b)
        })?;
        match reply {
            KBD_ACK => acked = true,
            KBD_RESET_PASSED => passed = true,
            r => return Err(InitError::Reset(r)), // 0xfc, 0xfd: its self-test failed
        }
    }
    keyboard_command(KBD_DISABLE_SCANNING, KBD_DISABLE_SCANNING)?;
    keyboard_command(KBD_SCANCODE_SET, KBD_SCANCODE_SET)?;
    keyboard_command(KBD_SCANCODE_SET, 2)?;
    keyboard_command(KBD_ENABLE_SCANNING, KBD_ENABLE_SCANNING)?;
    Ok(config)
}

/// IRQ 1 (interrupts off, the boot CPU).
fn on_irq() {
    IRQS.fetch_add(1, Ordering::Relaxed);
    drain();
}

/// Move up to [`BYTES_PER_IRQ`] waiting bytes from the controller into the
/// queue, discarding the second port's and those with a line error. With
/// interrupts off, on the boot CPU: the queue's only producer.
fn drain() {
    debug_assert!(!arch::irq::enabled() && arch::cpu_index() == 0, "the keyboard queue's producer is the boot CPU");
    for _ in 0..BYTES_PER_IRQ {
        let status = i8042::status();
        if status & i8042::OUTPUT_FULL == 0 {
            break;
        }
        let byte = i8042::read_data();
        if status & i8042::LINE_ERROR != 0 {
            LINE_ERRORS.fetch_add(1, Ordering::Relaxed);
        } else if status & i8042::FROM_PORT2 == 0 && QUEUE.push(byte) {
            BYTES.fetch_add(1, Ordering::Relaxed); // (a full queue counts it: `dropped`)
        }
    }
}

/// The next character typed, if one is waiting (keys that make none, such
/// as shift alone or the arrows, are consumed and skipped). Never blocks.
/// (A byte the queue dropped can leave the decoder out of step, e.g. shift
/// still held, until the key is pressed again; `kbd` counts drops.)
pub fn read_char() -> Option<char> {
    let mut decoder = DECODER.lock();
    decode(&mut decoder.as_mut()?.0, || QUEUE.pop())
}

/// Feed `next()`'s bytes to `decoder` until one completes a character;
/// bytes it refuses are counted.
fn decode(decoder: &mut Decoder, mut next: impl FnMut() -> Option<u8>) -> Option<char> {
    while let Some(byte) = next() {
        match decoder.add_byte(byte) {
            Ok(Some(event)) => {
                if let Some(DecodedKey::Unicode(c)) = decoder.process_keyevent(event) {
                    return Some(c);
                }
            }
            Ok(None) => {}
            Err(_) => {
                UNDECODED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    None
}

/// What kmon's `kbd` shows.
pub struct Stats {
    /// The layout's name (`none` before [`init`]).
    pub layout: &'static str,
    /// How [`init`] went (`None` before it).
    pub status: Option<Result<Route, InitError>>,
    pub irqs: u64,
    /// Bytes put on the queue.
    pub bytes: u64,
    /// Bytes dropped because the queue was full.
    pub dropped: u64,
    pub undecoded: u64,
    pub line_errors: u64,
}

pub fn stats() -> Stats {
    Stats {
        layout: DECODER.lock().as_ref().map_or("none", |d| d.1),
        status: *STATUS.lock(),
        irqs: IRQS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        dropped: QUEUE.dropped(),
        undecoded: UNDECODED.load(Ordering::Relaxed),
        line_errors: LINE_ERRORS.load(Ordering::Relaxed),
    }
}

/// Every character set-1 scancodes `bytes` make in layout `name`.
#[cfg(feature = "ktest")]
fn decode_all(name: &str, bytes: &[u8]) -> alloc::string::String {
    let mut decoder = new_decoder(choose_layout(Some(name)).1);
    let mut it = bytes.iter().copied();
    core::iter::from_fn(|| decode(&mut decoder, || it.next())).collect()
}

crate::ktest!(
    fn scancodes_decode_per_layout() {
        // Set 1: the Y key is 0x15, Z 0x2c, A 0x1e, 3 0x04, Enter 0x1c,
        // Backspace 0x0e, left shift 0x2a; a release adds 0x80. An overrun
        // (0xff) makes nothing and is counted.
        use crate::kassert_eq;
        let yz = [0x15, 0x95, 0x2c, 0xac];
        kassert_eq!(decode_all("us", &yz).as_str(), "yz");
        kassert_eq!(decode_all("de", &yz).as_str(), "zy");
        let shift_a_3 = [0x2a, 0x1e, 0x9e, 0x04, 0x84, 0xaa];
        kassert_eq!(decode_all("us", &shift_a_3).as_str(), "A#");
        kassert_eq!(decode_all("uk", &shift_a_3).as_str(), "A£");
        kassert_eq!(decode_all("us", &[0x1c, 0x9c, 0x0e, 0x8e]).as_str(), "\n\u{8}");
        let before = UNDECODED.load(Ordering::Relaxed);
        kassert_eq!(decode_all("us", &[0xff, 0x15, 0x95]).as_str(), "y");
        kassert_eq!(UNDECODED.load(Ordering::Relaxed), before + 1);
        Ok(())
    }
);

crate::ktest!(
    fn keymap_names_choose_a_layout() {
        // Every listed name is known; none, or an unknown name, is `us`.
        use crate::{kassert, kassert_eq};
        for (name, _) in LAYOUTS {
            let (chosen, _, known) = choose_layout(Some(name));
            kassert!(known && chosen == name);
        }
        let (name, _, known) = choose_layout(None);
        kassert_eq!((name, known), ("us", true));
        let (name, _, known) = choose_layout(Some("klingon"));
        kassert_eq!((name, known), ("us", false));
        Ok(())
    }
);

crate::ktest!(
    fn the_keyboard_is_routed_and_unmasked() {
        // On QEMU's q35 the 8042 and its keyboard are there: init succeeded,
        // IRQ 1 is routed (so routing it again is refused) and its input
        // unmasked.
        use crate::kassert;
        kassert!(matches!(*STATUS.lock(), Some(Ok(_))));
        kassert!(matches!(interrupts::route_isa_irq(1, on_irq), Err(interrupts::RouteError::Taken(_))));
        kassert!(interrupts::is_unmasked_isa(1));
        Ok(())
    }
);

crate::ktest!(
    fn a_byte_waiting_at_unmask_is_drained_and_port2_bytes_dropped() {
        // The 8042 puts a byte of our choosing in its output buffer, as if
        // from port 1 (command 0xd2) or port 2 (0xd3). One waiting while
        // IRQ 1's input is masked raised no edge we kept: `enable` drains
        // it, after which the next byte raises IRQ 1 (the line went low
        // again). A byte from port 2 is read and dropped.
        use crate::{kassert, kassert_eq};
        let route = route().ok_or("no keyboard")?;
        while QUEUE.pop().is_some() {} // (nothing else reads it in the ktests)
        interrupts::set_masked(route, true);
        controller_command_with(0xd2, 0x1e).map_err(|_| "writing the output buffer")?;
        kassert!(wait(REPLY_NS, |s| s & i8042::OUTPUT_FULL != 0));
        kassert_eq!(i8042::status() & i8042::FROM_PORT2, 0);
        enable(route);
        kassert_eq!(QUEUE.pop(), Some(0x1e));
        let before = IRQS.load(Ordering::Relaxed);
        controller_command_with(0xd2, 0x9e).map_err(|_| "writing the output buffer")?;
        let deadline = time::now_ns() + REPLY_NS;
        while IRQS.load(Ordering::Relaxed) == before && time::now_ns() < deadline {
            core::hint::spin_loop();
        }
        kassert!(IRQS.load(Ordering::Relaxed) > before);
        kassert_eq!(QUEUE.pop(), Some(0x9e));
        {
            let _off = arch::irq::push_off();
            controller_command_with(0xd3, 0x42).map_err(|_| "writing the second port's buffer")?;
            kassert!(wait(REPLY_NS, |s| s & i8042::OUTPUT_FULL != 0));
            kassert!(i8042::status() & i8042::FROM_PORT2 != 0);
            drain();
            kassert_eq!(i8042::status() & i8042::OUTPUT_FULL, 0);
        }
        kassert_eq!(QUEUE.pop(), None);
        Ok(())
    }
);
