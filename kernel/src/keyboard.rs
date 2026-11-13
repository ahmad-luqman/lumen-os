//! The PS/2 keyboard: the 8042 controller's first port, through the I/O
//! APIC (ISA IRQ 1).
//!
//! [`init`] (boot CPU, interrupts on, after the scheduler) sets the
//! controller up with its interrupts off and IRQ 1's input masked, polling
//! every reply with a deadline: both ports disabled, the output buffer
//! flushed, an explicit configuration byte (scancode translation on, both
//! port IRQs off), the controller's self-test, port 1's interface test,
//! port 1 enabled, the keyboard reset, scancode set 2 selected (the 8042
//! translates it to set 1) and scanning enabled. Then IRQ 1 is routed (its
//! handler registered while the input is still masked), the port's IRQ bit
//! set, the input unmasked, and the output buffer drained once: IRQ 1 is
//! edge triggered, so a byte that was already waiting would never raise it.
//!
//! The interrupt handler only moves bytes from the controller into a
//! lock-free queue (`lumen_queue::ByteQueue`; the producer is always the boot
//! CPU, which takes every I/O APIC interrupt); bytes from the second port (a
//! mouse) are discarded. Readers ([`read_char`]) decode them with the
//! `pc-keyboard` crate under a lock, in the layout `lumen.keymap=` names
//! (`us` without one, or for a name it doesn't know, with a warning).

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{info, warn};
use lumen_queue::ByteQueue;
use pc_keyboard::layouts::{
    AnyLayout, Azerty, Colemak, De105Key, Dvorak104Key, FiSe105Key, Jis109Key, No105Key, Uk105Key, Us104Key,
};
use pc_keyboard::{DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};

use crate::arch::i8042;
use crate::sync::SpinLock;
use crate::{cmdline, interrupts, time};

/// A layout's name for `lumen.keymap=`, and how to make it.
type NamedLayout = (&'static str, fn() -> AnyLayout);

/// The layouts `lumen.keymap=` can name.
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
/// The decoder (and so the queue's consumer side); `None` before [`init`].
static DECODER: SpinLock<Option<Decoder>> = SpinLock::new(None);
/// Keyboard interrupts taken.
static IRQS: AtomicU64 = AtomicU64::new(0);
/// The layout's name, once [`init`] has chosen it.
static LAYOUT: SpinLock<&str> = SpinLock::new("none");

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
const KBD_ACK: u8 = 0xfa;
const KBD_RESEND: u8 = 0xfe;
const KBD_RESET_PASSED: u8 = 0xaa;

/// How long the controller may take to take a byte or to answer.
const REPLY_NS: u64 = 100_000_000;
/// How long a keyboard may take to reset (its self-test).
const RESET_NS: u64 = 1_000_000_000;
/// Most bytes one interrupt moves (the controller holds one at a time; this
/// only bounds a misbehaving one).
const BYTES_PER_IRQ: usize = 16;

/// Why the keyboard isn't usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The controller didn't take a byte, or didn't answer, in time (no
    /// controller, or no keyboard): what was being waited for.
    Timeout(&'static str),
    SelfTest(u8),
    PortTest(u8),
    /// The keyboard answered a command with something other than an ACK.
    NoAck {
        command: u8,
        reply: u8,
    },
    Reset(u8),
    /// IRQ 1 couldn't be routed.
    Route(interrupts::RouteError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            InitError::Timeout(what) => write!(f, "no reply in time ({what})"),
            InitError::SelfTest(r) => write!(f, "controller self-test failed ({r:#04x})"),
            InitError::PortTest(r) => write!(f, "port 1 interface test failed ({r:#04x})"),
            InitError::NoAck { command, reply } => write!(f, "command {command:#04x} answered {reply:#04x}"),
            InitError::Reset(r) => write!(f, "keyboard reset failed ({r:#04x})"),
            InitError::Route(e) => write!(f, "IRQ 1 can't be routed: {e}"),
        }
    }
}

/// The layout `value` names (`lumen.keymap=`'s), and whether it was one we
/// know; `us` for no value or an unknown one.
fn choose_layout(value: Option<&str>) -> (&'static str, AnyLayout, bool) {
    let want = value.unwrap_or("us");
    match LAYOUTS.iter().find(|(name, _)| *name == want) {
        Some(&(name, layout)) => (name, layout(), true),
        None => ("us", AnyLayout::Us104Key(Us104Key), false),
    }
}

/// Set up the decoder in the layout `lumen.keymap=` names, then the
/// controller and keyboard, then IRQ 1 (see the module docs). A missing or
/// broken keyboard is a warning, not a failure: the kernel runs without one.
pub fn init() {
    let asked = cmdline::value("lumen.keymap");
    let (name, layout, known) = choose_layout(asked);
    if !known {
        let names: alloc::vec::Vec<&str> = LAYOUTS.iter().map(|(n, _)| *n).collect();
        warn!(target: "kbd", "unknown keymap '{}' (known: {}); using us", asked.unwrap_or(""), names.join(" "));
    }
    *DECODER.lock() = Some(Decoder::new(ScancodeSet1::new(), layout, HandleControl::Ignore));
    *LAYOUT.lock() = name;
    match start() {
        Ok(route) => info!(
            target: "kbd",
            "PS/2 keyboard on ISA IRQ {} (GSI {}, vector {:#x}), layout {name}",
            route.irq,
            route.gsi,
            route.vector
        ),
        Err(e) => warn!(target: "kbd", "no PS/2 keyboard: {e}"),
    }
}

fn start() -> Result<interrupts::Route, InitError> {
    let config = set_up_controller()?;
    let route = interrupts::route_isa_irq(1, on_irq).map_err(InitError::Route)?;
    controller_command_with(WRITE_CONFIG, config | PORT1_IRQ)?;
    interrupts::set_masked(route, false);
    // A byte that arrived before the input was unmasked raised no edge.
    // (Interrupts off: the queue has one producer at a time.)
    let _off = crate::arch::irq::push_off();
    drain();
    Ok(route)
}

/// Wait until `ready(status)` or `ns` pass; whether it did.
fn wait(ns: u64, ready: impl Fn(u8) -> bool) -> bool {
    let deadline = time::now_ns() + ns;
    loop {
        if ready(i8042::status()) {
            return true;
        }
        if time::now_ns() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

fn send_command(command: u8) -> Result<(), InitError> {
    if !wait(REPLY_NS, |s| s & i8042::INPUT_FULL == 0) {
        return Err(InitError::Timeout("the controller to take a command"));
    }
    i8042::write_command(command);
    Ok(())
}

fn send_data(byte: u8) -> Result<(), InitError> {
    if !wait(REPLY_NS, |s| s & i8042::INPUT_FULL == 0) {
        return Err(InitError::Timeout("the controller to take a byte"));
    }
    i8042::write_data(byte);
    Ok(())
}

fn receive(ns: u64, what: &'static str) -> Result<u8, InitError> {
    if !wait(ns, |s| s & i8042::OUTPUT_FULL != 0) {
        return Err(InitError::Timeout(what));
    }
    Ok(i8042::read_data())
}

fn controller_command_with(command: u8, arg: u8) -> Result<(), InitError> {
    send_command(command)?;
    send_data(arg)
}

/// Send the keyboard `byte` and wait for its ACK (resending, at most twice,
/// when it asks).
fn keyboard_command(byte: u8) -> Result<(), InitError> {
    for _ in 0..3 {
        send_data(byte)?;
        match receive(REPLY_NS, "the keyboard to acknowledge")? {
            KBD_ACK => return Ok(()),
            KBD_RESEND => continue,
            reply => return Err(InitError::NoAck { command: byte, reply }),
        }
    }
    Err(InitError::NoAck { command: byte, reply: KBD_RESEND })
}

/// The controller and keyboard, polled with their interrupts off (see the
/// module docs); the configuration byte to enable port 1's IRQ in.
fn set_up_controller() -> Result<u8, InitError> {
    send_command(DISABLE_PORT1)?;
    send_command(DISABLE_PORT2)?;
    for _ in 0..64 {
        if i8042::status() & i8042::OUTPUT_FULL == 0 {
            break;
        }
        let _ = i8042::read_data();
    }
    send_command(READ_CONFIG)?;
    let firmware = receive(REPLY_NS, "the configuration byte")?;
    // Both IRQs off, port 1's clock on, translation to set 1 on; the rest
    // (port 2's clock, the system flag) as firmware left it.
    let config = (firmware | TRANSLATE) & !(PORT1_IRQ | PORT2_IRQ | PORT1_CLOCK_OFF);
    controller_command_with(WRITE_CONFIG, config)?;
    send_command(SELF_TEST)?;
    match receive(REPLY_NS, "the controller's self-test")? {
        SELF_TEST_PASSED => {}
        r => return Err(InitError::SelfTest(r)),
    }
    // Some controllers reset their configuration in the self-test.
    controller_command_with(WRITE_CONFIG, config)?;
    send_command(TEST_PORT1)?;
    match receive(REPLY_NS, "port 1's interface test")? {
        PORT_TEST_PASSED => {}
        r => return Err(InitError::PortTest(r)),
    }
    send_command(ENABLE_PORT1)?;
    // Reset: an ACK and the self-test's result, in either order.
    send_data(KBD_RESET)?;
    let mut seen = (false, false);
    while seen != (true, true) {
        match receive(RESET_NS, "the keyboard to reset")? {
            KBD_ACK => seen.0 = true,
            KBD_RESET_PASSED => seen.1 = true,
            r => return Err(InitError::Reset(r)),
        }
    }
    keyboard_command(KBD_SCANCODE_SET)?;
    keyboard_command(2)?;
    keyboard_command(KBD_ENABLE_SCANNING)?;
    Ok(config)
}

/// IRQ 1 (interrupts off, the boot CPU).
fn on_irq() {
    IRQS.fetch_add(1, Ordering::Relaxed);
    drain();
}

/// Move every waiting byte from the controller into the queue, discarding
/// the second port's. Interrupts off, on the boot CPU (the queue's only
/// producer).
fn drain() {
    for _ in 0..BYTES_PER_IRQ {
        let status = i8042::status();
        if status & i8042::OUTPUT_FULL == 0 {
            break;
        }
        let byte = i8042::read_data();
        if status & i8042::FROM_PORT2 == 0 {
            let _ = QUEUE.push(byte); // a full queue counts it (`dropped`)
        }
    }
}

/// The next character typed, if one is waiting (keys that make none, such
/// as shift alone or the arrows, are consumed and skipped). Never blocks.
pub fn read_char() -> Option<char> {
    let mut decoder = DECODER.lock();
    decode(decoder.as_mut()?, || QUEUE.pop())
}

/// Feed `next()`'s bytes to `decoder` until one completes a character.
fn decode(decoder: &mut Decoder, mut next: impl FnMut() -> Option<u8>) -> Option<char> {
    while let Some(byte) = next() {
        if let Ok(Some(event)) = decoder.add_byte(byte)
            && let Some(DecodedKey::Unicode(c)) = decoder.process_keyevent(event)
        {
            return Some(c);
        }
    }
    None
}

/// What kmon's `kbd` shows: the layout, interrupts taken, bytes dropped
/// because the queue was full.
pub struct Stats {
    pub layout: &'static str,
    pub irqs: u64,
    pub dropped: u64,
}

pub fn stats() -> Stats {
    Stats { layout: *LAYOUT.lock(), irqs: IRQS.load(Ordering::Relaxed), dropped: QUEUE.dropped() }
}

/// Every character set-1 scancodes `bytes` make in layout `name`.
#[cfg(feature = "ktest")]
fn decode_all(name: &str, bytes: &[u8]) -> alloc::string::String {
    let (_, layout, _) = choose_layout(Some(name));
    let mut decoder = Decoder::new(ScancodeSet1::new(), layout, HandleControl::Ignore);
    let mut it = bytes.iter().copied();
    core::iter::from_fn(|| decode(&mut decoder, || it.next())).collect()
}

crate::ktest!(
    fn scancodes_decode_per_layout() {
        // Set 1: the Y key is 0x15, Z 0x2c, A 0x1e, 3 0x04, Enter 0x1c,
        // Backspace 0x0e, left shift 0x2a; a release adds 0x80.
        use crate::kassert_eq;
        let yz = [0x15, 0x95, 0x2c, 0xac];
        kassert_eq!(decode_all("us", &yz).as_str(), "yz");
        kassert_eq!(decode_all("de", &yz).as_str(), "zy");
        let shift_a_3 = [0x2a, 0x1e, 0x9e, 0x04, 0x84, 0xaa];
        kassert_eq!(decode_all("us", &shift_a_3).as_str(), "A#");
        kassert_eq!(decode_all("uk", &shift_a_3).as_str(), "A£");
        kassert_eq!(decode_all("us", &[0x1c, 0x9c, 0x0e, 0x8e]).as_str(), "\n\u{8}");
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
        kassert_eq!((choose_layout(None).0, choose_layout(None).2), ("us", true));
        kassert_eq!((choose_layout(Some("klingon")).0, choose_layout(Some("klingon")).2), ("us", false));
        Ok(())
    }
);

crate::ktest!(
    fn the_keyboard_is_routed_and_unmasked() {
        // On QEMU's q35 the 8042 and its keyboard are there: IRQ 1 is routed
        // (so routing it again is refused) and its input unmasked.
        use crate::kassert;
        kassert!(matches!(interrupts::route_isa_irq(1, on_irq), Err(interrupts::RouteError::Taken(_))));
        kassert!(interrupts::is_unmasked_isa(1));
        Ok(())
    }
);
