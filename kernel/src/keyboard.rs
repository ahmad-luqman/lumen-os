//! The PS/2 keyboard: the 8042 controller's first port, through the I/O
//! APIC (ISA IRQ 1).
//!
//! [`init`] (once, from `kmain`: boot CPU, interrupts on, after the
//! scheduler) routes IRQ 1 first (its handler registered, the input left
//! masked; without an I/O APIC there is nothing to set up), then sets the
//! controller up with its IRQs off, polling every reply with a deadline:
//! both ports disabled (which stops their clocks), the output buffer
//! flushed, the configuration byte rewritten (both IRQs off, translation to
//! scancode set 1 on, both clocks still off; the system flag as firmware
//! left it), the controller's self-test, the configuration written again (a
//! self-test may reset it) and the buffer flushed again, port 1's interface
//! test, port 1 enabled (its clock on), scanning disabled, the keyboard
//! reset and scanning disabled again (a reset turns it back on), scancode
//! set 2 selected (the 8042 translates it to set 1) and scanning enabled.
//! The controller's own replies come while port 1's clock is off; while
//! waiting for the keyboard's, a byte that isn't the awaited reply (a key
//! pressed during boot) is skipped and counted, up to [`STRAYS`] per reply,
//! as is one from port 2; a byte the controller flags with a parity or
//! timeout error ends init (no device answering). Last, port 1's IRQ bit is
//! set, the input unmasked and the output buffer drained once: a byte that
//! was already waiting raised IRQ 1's edge while the input was masked, and
//! the I/O APIC doesn't keep a masked edge. If init fails after port 1 was
//! enabled, port 1 is disabled again (best effort).
//!
//! The queue's producer is always the boot CPU with interrupts off (every
//! I/O APIC interrupt goes there; the handler, or the drain at unmask): it
//! moves bytes from the controller into a lock-free single-producer,
//! single-consumer queue (`lumen_queue::ByteQueue`), discarding bytes from
//! the second port (a mouse) and counting and discarding those flagged with
//! a parity or timeout error. Readers ([`read_key`]) decode them with the
//! `pc-keyboard` crate under a lock, in the layout `lumen.keymap=` names
//! (`us` without one, or for a name it doesn't know, with a warning), into
//! characters and other keys (with the shift state).
//! kmon's `kbd` shows how it went ([`stats`]).

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{debug, info, warn};
use lumen_queue::ByteQueue;
use pc_keyboard::layouts::{
    AnyLayout, Azerty, Colemak, De105Key, Dvorak104Key, FiSe105Key, Jis109Key, No105Key, Uk105Key, Us104Key,
};
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, PS2Keyboard, ScancodeSet1};

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

/// The decoder and the name of its layout.
struct Keyboard {
    decoder: Decoder,
    layout: &'static str,
}

/// How [`init`] went.
#[derive(Clone, Copy)]
struct Status {
    /// IRQ 1's route, if it was routed (it stays routed, masked, when a
    /// later step fails).
    route: Option<Route>,
    result: Result<(), InitError>,
}

/// Bytes from the keyboard, waiting to be decoded.
static QUEUE: ByteQueue<256> = ByteQueue::new();
/// The decoder: the queue's consumer side is used only under this lock
/// (outside the ktests). `None` before [`init`].
static KEYBOARD: SpinLock<Option<Keyboard>> = SpinLock::new(None);
/// `None` before [`init`].
static STATUS: SpinLock<Option<Status>> = SpinLock::new(None);
/// Keyboard interrupts taken.
static IRQS: AtomicU64 = AtomicU64::new(0);
/// Bytes put on the queue.
static BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes the controller flagged with a parity or timeout error (discarded).
static LINE_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Bytes the decoder refused (the keyboard's own overrun code 0xff among
/// them: keys it lost).
static UNDECODED: AtomicU64 = AtomicU64::new(0);
/// Bytes skipped during init: not the awaited reply, from port 2, or
/// flushed.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

// Controller commands (port 0x64).
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const DISABLE_PORT2: u8 = 0xa7;
const SELF_TEST: u8 = 0xaa;
const TEST_PORT1: u8 = 0xab;
const DISABLE_PORT1: u8 = 0xad;
const ENABLE_PORT1: u8 = 0xae;
/// Put the next data byte in the output buffer as if from port 1 (ktests).
#[cfg(feature = "ktest")]
const WRITE_PORT1_OUTPUT: u8 = 0xd2;
/// The same, as if from port 2 (ktests).
#[cfg(feature = "ktest")]
const WRITE_PORT2_OUTPUT: u8 = 0xd3;
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
/// boot) skipped while waiting for one reply, at most.
const STRAYS: usize = 32;
/// Most bytes a flush reads.
const FLUSH: usize = 64;
/// Most bytes one interrupt moves (the controller holds one at a time; this
/// only bounds a misbehaving one).
const BYTES_PER_IRQ: usize = 16;

/// Why the keyboard isn't usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// IRQ 1 couldn't be routed (no I/O APIC, say).
    Route(interrupts::RouteError),
    /// The controller didn't take `command` (or its argument), or nothing
    /// answered it, in time (no controller, or no keyboard): what was being
    /// waited for.
    Timeout {
        command: u8,
        waiting_for: &'static str,
    },
    /// The controller flagged its answer to `command` with a timeout or
    /// parity error (`status`): no device answered.
    LineError {
        command: u8,
        status: u8,
    },
    /// Only bytes that weren't the reply to `command` came (more than
    /// [`STRAYS`]); `last` is the last one.
    NoReply {
        command: u8,
        last: u8,
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
            InitError::LineError { command, status } => {
                write!(f, "command {command:#04x} answered with a line error (status {status:#04x}): no device")
            }
            InitError::NoReply { command, last } => {
                write!(f, "command {command:#04x} got only stray bytes (the last {last:#04x})")
            }
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
/// keyboard is a warning, not a failure: the kernel runs without one. Once.
pub fn init() {
    assert!(STATUS.lock().is_none(), "keyboard::init runs once");
    let asked = cmdline::value("lumen.keymap");
    let (name, layout, known) = choose_layout(asked);
    if let (false, Some(asked)) = (known, asked) {
        let names: alloc::vec::Vec<&str> = LAYOUTS.iter().map(|(n, _)| *n).collect();
        warn!(target: "kbd", "unknown keymap '{asked}' (known: {}); using us", names.join(" "));
    }
    *KEYBOARD.lock() = Some(Keyboard { decoder: new_decoder(layout), layout: name });
    let status = start();
    match (status.route, status.result) {
        (Some(route), Ok(())) => info!(
            target: "kbd",
            "PS/2 keyboard on ISA IRQ {} (GSI {}, vector {:#x}), layout {name}",
            route.irq(),
            route.gsi(),
            route.vector()
        ),
        (_, Err(e)) => warn!(target: "kbd", "no PS/2 keyboard: {e}"),
        (None, Ok(())) => unreachable!("a keyboard set up without a route"),
    }
    let skipped = SKIPPED.load(Ordering::Relaxed);
    if skipped > 0 {
        info!(target: "kbd", "skipped {skipped} stray byte(s) while setting the keyboard up");
    }
    *STATUS.lock() = Some(status);
}

fn start() -> Status {
    // Routed first: without an I/O APIC there is no point resetting the
    // keyboard. (If a later step fails, IRQ 1 stays routed, masked.)
    let route = match interrupts::route_isa_irq(1, on_irq) {
        Ok(route) => route,
        Err(e) => return Status { route: None, result: Err(InitError::Route(e)) },
    };
    let result = set_up_controller().and_then(|config| {
        // Port 1's clock bit clear (ENABLE_PORT1 cleared it), its IRQ on.
        controller_command_with(WRITE_CONFIG, (config & !PORT1_CLOCK_OFF) | PORT1_IRQ)?;
        enable(route);
        Ok(())
    });
    if result.is_err() {
        // Best effort: no scanning keyboard left on a port nothing reads.
        let _ = send_command(DISABLE_PORT1);
    }
    Status { route: Some(route), result }
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
    STATUS.lock().filter(|s| s.result.is_ok()).and_then(|s| s.route)
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

/// [`wait`], or a timeout naming `command` and what was waited for.
fn wait_for(command: u8, ns: u64, waiting_for: &'static str, ready: impl Fn(u8) -> bool) -> Result<(), InitError> {
    if wait(ns, ready) { Ok(()) } else { Err(InitError::Timeout { command, waiting_for }) }
}

fn send_command(command: u8) -> Result<(), InitError> {
    wait_for(command, REPLY_NS, "the controller to take it", |s| s & i8042::INPUT_FULL == 0)?;
    i8042::write_command(command);
    Ok(())
}

/// Write `byte` to the data port (for the keyboard, or `command`'s argument).
fn send_data(command: u8, byte: u8) -> Result<(), InitError> {
    wait_for(command, REPLY_NS, "the controller to take a byte", |s| s & i8042::INPUT_FULL == 0)?;
    i8042::write_data(byte);
    Ok(())
}

fn controller_command_with(command: u8, arg: u8) -> Result<(), InitError> {
    send_command(command)?;
    send_data(command, arg)
}

/// The next byte from port 1 within `ns` (bytes from port 2 are skipped and
/// counted); one the controller flags with a line error ends init.
fn receive(command: u8, ns: u64, waiting_for: &'static str) -> Result<u8, InitError> {
    let deadline = time::now_ns() + ns;
    loop {
        let left = deadline.saturating_sub(time::now_ns());
        wait_for(command, left, waiting_for, |s| s & i8042::OUTPUT_FULL != 0)?;
        let status = i8042::status();
        let byte = i8042::read_data();
        if status & i8042::LINE_ERROR != 0 {
            return Err(InitError::LineError { command, status });
        }
        if status & i8042::FROM_PORT2 == 0 {
            return Ok(byte);
        }
        SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Wait (`ns` in all) for the first byte `is_reply` accepts, skipping and
/// counting up to [`STRAYS`] others (scancodes of keys pressed during boot).
fn await_reply(
    command: u8,
    ns: u64,
    waiting_for: &'static str,
    is_reply: impl Fn(u8) -> bool,
) -> Result<u8, InitError> {
    let deadline = time::now_ns() + ns;
    let mut last = 0;
    for _ in 0..=STRAYS {
        let byte = receive(command, deadline.saturating_sub(time::now_ns()), waiting_for)?;
        if is_reply(byte) {
            return Ok(byte);
        }
        SKIPPED.fetch_add(1, Ordering::Relaxed);
        debug!(target: "kbd", "skipped {byte:#04x} waiting for {waiting_for}");
        last = byte;
    }
    Err(InitError::NoReply { command, last })
}

/// Discard what waits in the output buffer ([`FLUSH`] bytes at most),
/// counting it as skipped; warn if it doesn't empty.
fn flush_output() {
    for _ in 0..FLUSH {
        if i8042::status() & i8042::OUTPUT_FULL == 0 {
            return;
        }
        let _ = i8042::read_data();
        SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    if i8042::status() & i8042::OUTPUT_FULL != 0 {
        warn!(target: "kbd", "the 8042's output buffer didn't empty in {FLUSH} reads");
    }
}

/// Run controller command `command` and check its reply is `passed`, else
/// fail with `failed(reply)`. (Port 1's clock is off, so no scancode can
/// come.)
fn controller_test(command: u8, passed: u8, failed: fn(u8) -> InitError) -> Result<(), InitError> {
    send_command(command)?;
    let reply = receive(command, REPLY_NS, "the test's result")?;
    if reply == passed { Ok(()) } else { Err(failed(reply)) }
}

/// Send the keyboard `command`, then each of `args`, each waiting for its
/// ACK (resending a byte, at most twice, when the keyboard asks).
fn keyboard_command(command: u8, args: &[u8]) -> Result<(), InitError> {
    for byte in core::iter::once(command).chain(args.iter().copied()) {
        send_acked(command, byte)?;
    }
    Ok(())
}

fn send_acked(command: u8, byte: u8) -> Result<(), InitError> {
    for _ in 0..3 {
        send_data(command, byte)?;
        let reply = await_reply(command, REPLY_NS, "the keyboard's ACK", |b| matches!(b, KBD_ACK | KBD_RESEND))?;
        if reply == KBD_ACK {
            return Ok(());
        }
    }
    Err(InitError::Resends { command })
}

/// Reset the keyboard: an ACK and its self-test's result, in either order,
/// within [`RESET_NS`] (not resent).
fn reset_keyboard() -> Result<(), InitError> {
    send_data(KBD_RESET, KBD_RESET)?;
    let deadline = time::now_ns() + RESET_NS;
    let (mut acked, mut passed) = (false, false);
    while !(acked && passed) {
        let left = deadline.saturating_sub(time::now_ns());
        let waiting_for = if acked { "the keyboard's self-test" } else { "the keyboard to acknowledge the reset" };
        match await_reply(KBD_RESET, left, waiting_for, |b| matches!(b, KBD_ACK | KBD_RESET_PASSED | 0xfc..=0xfd))? {
            KBD_ACK => acked = true,
            KBD_RESET_PASSED => passed = true,
            r => return Err(InitError::Reset(r)), // 0xfc, 0xfd: its self-test failed
        }
    }
    Ok(())
}

/// The controller and keyboard, polled with their IRQs off (see the module
/// docs); the configuration byte as last written (its port-1-clock-off bit
/// still set; ENABLE_PORT1 has since turned the clock on).
fn set_up_controller() -> Result<u8, InitError> {
    send_command(DISABLE_PORT1)?;
    send_command(DISABLE_PORT2)?;
    // Taken (and so in effect) before the flush, so nothing more can come.
    wait_for(DISABLE_PORT2, REPLY_NS, "the controller to take it", |s| s & i8042::INPUT_FULL == 0)?;
    flush_output();
    send_command(READ_CONFIG)?;
    let firmware = receive(READ_CONFIG, REPLY_NS, "the configuration byte")?;
    // Both IRQs off, port 1's clock off (as DISABLE_PORT1 left it, and
    // port 2's as DISABLE_PORT2 did), translation to set 1 on; the system
    // flag as firmware left it.
    let config = (firmware | TRANSLATE | PORT1_CLOCK_OFF) & !(PORT1_IRQ | PORT2_IRQ);
    controller_command_with(WRITE_CONFIG, config)?;
    controller_test(SELF_TEST, SELF_TEST_PASSED, InitError::SelfTest)?;
    // Some controllers reset their configuration in the self-test (port 1's
    // clock on meanwhile: flush what it let in).
    controller_command_with(WRITE_CONFIG, config)?;
    flush_output();
    controller_test(TEST_PORT1, PORT_TEST_PASSED, InitError::PortTest)?;
    send_command(ENABLE_PORT1)?;
    keyboard_command(KBD_DISABLE_SCANNING, &[])?;
    reset_keyboard()?;
    keyboard_command(KBD_DISABLE_SCANNING, &[])?;
    keyboard_command(KBD_SCANCODE_SET, &[2])?;
    keyboard_command(KBD_ENABLE_SCANNING, &[])?;
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

/// A key read from the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A key making a character, in the layout.
    Char(char),
    /// Any other key pressed (an arrow, PageUp, a modifier), and whether
    /// shift was held.
    Raw { code: KeyCode, shift: bool },
}

/// The next key pressed, if one is waiting (releases are consumed and
/// skipped). Never blocks.
/// (A byte the queue dropped can leave the decoder out of step, e.g. shift
/// still held, until the key is pressed again; `kbd` counts drops.)
pub fn read_key() -> Option<Key> {
    let mut keyboard = KEYBOARD.lock();
    decode(&mut keyboard.as_mut()?.decoder, || QUEUE.pop())
}

/// Feed `next()`'s bytes to `decoder` until one completes a key press;
/// bytes it refuses are counted.
fn decode(decoder: &mut Decoder, mut next: impl FnMut() -> Option<u8>) -> Option<Key> {
    while let Some(byte) = next() {
        match decoder.add_byte(byte) {
            Ok(Some(event)) => match decoder.process_keyevent(event) {
                Some(DecodedKey::Unicode(c)) => return Some(Key::Char(c)),
                Some(DecodedKey::RawKey(code)) => {
                    return Some(Key::Raw { code, shift: decoder.get_modifiers().is_shifted() });
                }
                None => {}
            },
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
    /// IRQ 1's route, if it was routed (also when init failed later).
    pub route: Option<Route>,
    /// How [`init`] went (`None` before it).
    pub result: Option<Result<(), InitError>>,
    pub irqs: u64,
    /// Bytes put on the queue.
    pub bytes: u64,
    /// Bytes dropped because the queue was full.
    pub dropped: u64,
    pub undecoded: u64,
    pub line_errors: u64,
    /// Bytes skipped while setting the keyboard up.
    pub skipped: u64,
}

pub fn stats() -> Stats {
    let status = *STATUS.lock();
    Stats {
        layout: KEYBOARD.lock().as_ref().map_or("none", |k| k.layout),
        route: status.and_then(|s| s.route),
        result: status.map(|s| s.result),
        irqs: IRQS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        dropped: QUEUE.dropped(),
        undecoded: UNDECODED.load(Ordering::Relaxed),
        line_errors: LINE_ERRORS.load(Ordering::Relaxed),
        skipped: SKIPPED.load(Ordering::Relaxed),
    }
}

/// Every character set-1 scancodes `bytes` make in layout `name`.
#[cfg(feature = "ktest")]
fn decode_all(name: &str, bytes: &[u8]) -> alloc::string::String {
    let mut decoder = new_decoder(choose_layout(Some(name)).1);
    let mut it = bytes.iter().copied();
    core::iter::from_fn(|| decode(&mut decoder, || it.next()))
        .filter_map(|k| match k {
            Key::Char(c) => Some(c),
            Key::Raw { .. } => None,
        })
        .collect()
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
    fn other_keys_come_with_the_shift_state() {
        // Set 1: PageUp is 0xe0 0x49 (released 0xe0 0xc9), PageDown 0xe0 0x51;
        // left shift 0x2a (released 0xaa).
        use crate::kassert_eq;
        let mut decoder = new_decoder(choose_layout(Some("us")).1);
        let bytes = [0x2a, 0xe0, 0x49, 0xe0, 0xc9, 0xaa, 0xe0, 0x51, 0xe0, 0xd1, 0x1e, 0x9e];
        let mut it = bytes.iter().copied();
        let keys: alloc::vec::Vec<Key> = core::iter::from_fn(|| decode(&mut decoder, || it.next())).collect();
        kassert_eq!(
            keys,
            alloc::vec![
                Key::Raw { code: KeyCode::LShift, shift: true },
                Key::Raw { code: KeyCode::PageUp, shift: true },
                Key::Raw { code: KeyCode::PageDown, shift: false },
                Key::Char('a'),
            ]
        );
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
        kassert!(route().is_some());
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
        // again). A byte from port 2 is read and dropped. Whatever happens,
        // the input is unmasked and the buffer and queue emptied after.
        use crate::{kassert, kassert_eq};
        struct Restore(Route);
        impl Drop for Restore {
            fn drop(&mut self) {
                enable(self.0);
                while QUEUE.pop().is_some() {}
            }
        }
        let route = route().ok_or("no keyboard")?;
        let _restore = Restore(route);
        while QUEUE.pop().is_some() {} // (nothing else reads it in the ktests)
        interrupts::set_masked(route, true);
        controller_command_with(WRITE_PORT1_OUTPUT, 0x1e).map_err(|_| "writing the output buffer")?;
        kassert!(wait(REPLY_NS, |s| s & i8042::OUTPUT_FULL != 0));
        kassert_eq!(i8042::status() & i8042::FROM_PORT2, 0);
        enable(route);
        kassert_eq!(QUEUE.pop(), Some(0x1e));
        let before = IRQS.load(Ordering::Relaxed);
        controller_command_with(WRITE_PORT1_OUTPUT, 0x9e).map_err(|_| "writing the output buffer")?;
        let deadline = time::now_ns() + 10 * REPLY_NS;
        while IRQS.load(Ordering::Relaxed) == before && time::now_ns() < deadline {
            core::hint::spin_loop();
        }
        kassert!(IRQS.load(Ordering::Relaxed) > before);
        kassert_eq!(QUEUE.pop(), Some(0x9e));
        {
            let _off = arch::irq::push_off();
            controller_command_with(WRITE_PORT2_OUTPUT, 0x42).map_err(|_| "writing the second port's buffer")?;
            kassert!(wait(REPLY_NS, |s| s & i8042::OUTPUT_FULL != 0));
            kassert!(i8042::status() & i8042::FROM_PORT2 != 0);
            drain();
            kassert_eq!(i8042::status() & i8042::OUTPUT_FULL, 0);
        }
        kassert_eq!(QUEUE.pop(), None);
        Ok(())
    }
);

crate::ktest!(
    fn a_stray_byte_before_the_ack_is_skipped() {
        // A byte waiting before the keyboard's ACK (as a key pressed during
        // init would leave; QEMU never puts one there by itself, so the 8042
        // is told to with 0xd2) is skipped and counted, and the command
        // still succeeds, leaving nothing behind.
        use crate::{kassert, kassert_eq};
        let route = route().ok_or("no keyboard")?;
        while QUEUE.pop().is_some() {} // (nothing else reads it in the ktests)
        interrupts::set_masked(route, true);
        let skipped = SKIPPED.load(Ordering::Relaxed);
        let (planted, full, result) = {
            let _off = arch::irq::push_off();
            let planted = controller_command_with(WRITE_PORT1_OUTPUT, 0x1e);
            let full = wait(REPLY_NS, |s| s & i8042::OUTPUT_FULL != 0);
            (planted.is_ok(), full, keyboard_command(KBD_ENABLE_SCANNING, &[]))
        };
        enable(route);
        kassert!(planted && full);
        kassert_eq!(result, Ok(()));
        kassert_eq!(SKIPPED.load(Ordering::Relaxed), skipped + 1);
        kassert_eq!(QUEUE.pop(), None);
        Ok(())
    }
);
