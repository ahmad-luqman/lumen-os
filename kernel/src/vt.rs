//! Virtual terminals' keys: each key the keyboard decodes goes to the shell
//! of the terminal shown when it is routed ([`route_key`]: `term::shown()`
//! read then, so a canvas that brought its command's terminal to the front
//! gets the keys from then on), into that terminal's ring
//! (`lumen_term::vt::Router`); Alt+F1 to Alt+F4 switch which terminal is
//! shown ([`term::show`]).
//!
//! Keys are routed when a screen shell reads, which it does as soon as IRQ 1
//! wakes it: whichever runs first decodes the waiting bytes and routes each
//! key under [`ROUTER`]'s lock, then wakes the others if it routed one
//! elsewhere (all of them wait on the keyboard's wait queue,
//! [`keyboard::wait_for_bytes_or`]). So a key typed just before a command on
//! another terminal takes the canvas, but not routed yet, goes to that
//! command (not tested: the window is the time between IRQ 1 and the first
//! shell running). A canvas brings its terminal to the front without taking
//! [`ROUTER`]'s lock (`term::canvas`): a key being routed then lands on
//! either side.
//!
//! Lock order: [`ROUTER`], then the keyboard's decoder (released before
//! the key is routed), then the screen's lock (taken for each key, to read
//! which terminal is shown, and for a switch), [`CUT`] and the input
//! rings' locks (leaves); nothing takes [`ROUTER`] while holding any of
//! them. [`HAS_KEYS`] mirrors which rings hold keys, so a reader's wake-up
//! check takes no lock of its own (it runs under the wait queue's lock, so
//! not [`ROUTER`]'s).
//!
//! **The desktop** (#153; ARCHITECTURE section 11, "Desktop mode"): while
//! it is shown ([`DESKTOP`], set and cleared only under [`ROUTER`]'s lock)
//! the decoding pass routes only Alt+F1 to F4 to the terminals; every other
//! press goes to the desktop's key ring alone (every key event already
//! does: `keyboard::read_key`) and is counted (`Routed::Desktop`). An
//! Alt+F`n` press then leaves it ([`term::leave_desktop`]), clears
//! [`DESKTOP`] and writes the [`CUT`], all before any later key is routed,
//! so keys after it go to terminal `n`. [`enter_desktop`] (the `desktop`
//! command) does the reverse under the same lock. The compositor runs the
//! pass itself ([`decode_pending`]), holding none of its own locks.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::keyboard::{self, Key, KeyCode};
use crate::sync::SpinLock;
use crate::term::{self, CanvasError, Owner, ScreenCanvas, ShowError, VTS};
use lumen_input::{Buttons, Seq};
use lumen_term::vt::Router;

/// Keys each terminal's ring holds at most (a shell that falls this far
/// behind loses the newest, counted: [`stats`]): half the keyboard's
/// 256-byte queue, though a key takes two bytes or more (press and
/// release).
const RING: usize = 128;

type Rings = Router<Key, VTS, RING>;

static ROUTER: SpinLock<Rings> = SpinLock::new(Router::new());
/// Bit `vt` set while terminal `vt`'s ring holds keys: [`publish`]ed under
/// [`ROUTER`]'s lock after every change.
static HAS_KEYS: AtomicUsize = AtomicUsize::new(0);
/// Bit `vt` set once terminal `vt` has a shell reading it ([`attach`]):
/// keys typed on one without are discarded (counted), not routed to a ring
/// nothing reads.
static READERS: AtomicUsize = AtomicUsize::new(0);
/// Bit `vt` set from terminal `vt`'s first dropped key until its ring
/// empties again: one warning per burst (changed under [`ROUTER`]'s lock).
static DROPPING: AtomicUsize = AtomicUsize::new(0);
/// Keys each terminal discarded because no shell reads it.
static DISCARDED: [AtomicU64; VTS] = [const { AtomicU64::new(0) }; VTS];
/// Switches refused because a command had the screen ([`show_counted`]).
static REFUSED: AtomicU64 = AtomicU64::new(0);
/// Presses routed to the desktop alone while it was shown.
static TO_DESKTOP: AtomicU64 = AtomicU64::new(0);

/// Set while the desktop is shown (its `ScreenCanvas` live): set and
/// cleared only under [`ROUTER`]'s lock ([`enter_desktop`], and an Alt+F`n`
/// in the decoding pass), so each key is routed on one side of the change.
pub static DESKTOP: AtomicBool = AtomicBool::new(false);

/// Both input rings' newest events at a change of the desktop's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// `crate::input::KEYS.newest()`.
    pub keys: Seq,
    /// `crate::input::POINTER.newest()`.
    pub pointer: Seq,
}

impl Marks {
    /// The rings' newest now (each ring's leaf lock taken in turn).
    fn now() -> Marks {
        Marks { keys: crate::input::KEYS.newest(), pointer: crate::input::POINTER.newest() }
    }
}

/// The cut: where each change of the desktop's state falls in the input
/// rings, so the compositor knows which events were the desktop's. Written
/// under [`ROUTER`]'s lock before any later key is routed; the compositor
/// reads it each pass. A leaf lock.
///
/// Every change counts one `generation` (0 before the desktop ever
/// started; odd while shown, even while paused). The latest pause's marks
/// and the latest start's or resume's are both kept, so a compositor that
/// sees a resume and then a pause in one look still has the marks of each
/// (a resume waits until the compositor has seen the pause before it, so
/// no older marks are ever needed).
#[derive(Clone, Copy, Debug, Default)]
pub struct Cut {
    pub generation: u64,
    /// At the latest pause (Alt+F`n`): the rings' newest, the Alt+F`n`
    /// press itself the newest key event.
    pub pause: Marks,
    /// At the latest start or resume (`desktop`).
    pub resume: Marks,
    /// The buttons the pointer ring's newest frame held at the latest
    /// start or resume: the baseline `Wm::resume` takes (captured here, as
    /// the ring may overwrite that frame before the compositor reads it).
    pub baseline: Buttons,
    /// Who holds the desktop's `ScreenCanvas` (the compositor's owner,
    /// made once when the desktop starts).
    pub owner: Option<Owner>,
}

/// The cut (see [`Cut`]).
pub static CUT: SpinLock<Cut> = SpinLock::new(Cut {
    generation: 0,
    pause: Marks { keys: Seq::ZERO, pointer: Seq::ZERO },
    resume: Marks { keys: Seq::ZERO, pointer: Seq::ZERO },
    baseline: Buttons::NONE,
    owner: None,
});

/// Alt+F`n` shows terminal `n` (from 1; the first [`VTS`] of them).
const SWITCH_KEYS: [KeyCode; 4] = [KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4];

/// The terminal Alt+F`n` shows (`n` 1 to [`VTS`]), if a press of `code`
/// with Alt held (`alt`) is one: the decoding pass's switch, and the
/// compositor's barrier in the key ring (the same predicate).
pub fn switch_key(code: KeyCode, alt: bool) -> Option<usize> {
    if !alt {
        return None;
    }
    SWITCH_KEYS[..VTS].iter().position(|&f| f == code)
}

/// [`switch_key`] for a decoded key.
fn switch_of(key: Key) -> Option<usize> {
    let Key::Raw { code, mods } = key else { return None };
    switch_key(code, mods.is_alt())
}

/// A modifier or lock key on its own: the decoder tracks it, no shell needs
/// it (and it would take a slot in a ring).
fn bare_modifier(key: Key) -> bool {
    use KeyCode::*;
    matches!(
        key,
        Key::Raw {
            code: LShift
                | RShift
                | LControl
                | RControl
                | RControl2
                | LAlt
                | RAltGr
                | RAlt2
                | LWin
                | RWin
                | CapsLock
                | NumpadLock
                | ScrollLock,
            ..
        }
    )
}

/// What [`route_key`] did with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routed {
    /// Into terminal `.0`'s ring.
    To(usize),
    /// Terminal `.0`'s ring was full: dropped (counted).
    Dropped(usize),
    /// Terminal `.0`, shown, has no shell: discarded (counted).
    NoShell(usize),
    /// Alt+F`n`: terminal `.0` is shown now.
    Switched(usize),
    /// Alt+F`n` refused (a command has the screen, say): terminal `.0` isn't
    /// shown.
    Refused(usize, ShowError),
    /// A modifier on its own: nothing to route.
    Modifier,
    /// The desktop is shown: the key went to its ring alone (counted).
    Desktop,
    /// Alt+F`n` while the desktop was shown: it was left, and terminal
    /// `.0` is shown now.
    Left(usize),
}

/// Show terminal `vt` (`term::show`), counting a refusal because a command
/// has the screen. Called under [`ROUTER`]'s lock, but by the ktests.
fn show_counted(vt: usize) -> Result<(), ShowError> {
    let shown = term::show(vt);
    if shown == Err(ShowError::InUse) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
    }
    shown
}

/// Route one decoded key: Alt+F`n` switches, a bare modifier is skipped,
/// any other goes to the terminal shown now, if a shell reads it (bit `vt`
/// of `readers`). While the desktop is shown, Alt+F`n` leaves it and any
/// other press stays the desktop's.
pub fn route_key(router: &mut Rings, key: Key, readers: usize) -> Routed {
    if DESKTOP.load(Ordering::Acquire) {
        if let Some(to) = switch_of(key) {
            leave_desktop(to);
            return Routed::Left(to);
        }
        if bare_modifier(key) {
            return Routed::Modifier;
        }
        TO_DESKTOP.fetch_add(1, Ordering::Relaxed);
        return Routed::Desktop;
    }
    if let Some(to) = switch_of(key) {
        return match show_counted(to) {
            Ok(()) => Routed::Switched(to),
            Err(e) => Routed::Refused(to, e),
        };
    }
    if bare_modifier(key) {
        return Routed::Modifier;
    }
    let vt = term::shown();
    if readers & 1 << vt == 0 {
        DISCARDED[vt].fetch_add(1, Ordering::Relaxed);
        Routed::NoShell(vt)
    } else if router.route(vt, key) {
        Routed::To(vt)
    } else {
        Routed::Dropped(vt)
    }
}

/// After a change to `router` (under [`ROUTER`]'s lock): which rings hold
/// keys, for the readers' wake-up check; a ring that emptied ends its burst
/// of drops.
fn publish(router: &Rings) {
    let mask = router.mask();
    HAS_KEYS.store(mask, Ordering::Release);
    DROPPING.fetch_and(mask, Ordering::Relaxed);
}

/// Terminal `vt` has a shell reading it from now on.
pub fn attach(vt: usize) {
    assert!(vt < VTS, "no virtual terminal {vt}");
    READERS.fetch_or(1 << vt, Ordering::AcqRel);
}

/// How many terminals have a shell reading them: the screen shells, which
/// run the keyboard's decoding pass (`kbd events` says when none does).
pub fn readers() -> u32 {
    READERS.load(Ordering::Acquire).count_ones()
}

/// Terminal `vt` has no shell after all (its thread didn't start): keys
/// typed on it are discarded, and those already routed to it too (counted).
pub fn detach(vt: usize) {
    assert!(vt < VTS, "no virtual terminal {vt}");
    let mut router = ROUTER.lock();
    READERS.fetch_and(!(1 << vt), Ordering::AcqRel);
    while router.take(vt).is_some() {
        DISCARDED[vt].fetch_add(1, Ordering::Relaxed);
    }
    publish(&router);
}

/// What one decoding pass did, for after the locks.
#[derive(Default)]
struct Pass {
    /// A key went to a terminal other than the reader's.
    elsewhere: bool,
    /// The last switch refused (to log).
    refused: Option<(usize, ShowError)>,
    /// The last terminal whose burst of drops began (to log).
    dropping: Option<usize>,
    /// The desktop was left (Alt+F`n`).
    left: bool,
}

/// Decode the keyboard's waiting bytes and route each key ([`route_key`]),
/// under [`ROUTER`]'s lock (`router`), for the shell of terminal `me` if a
/// shell runs it.
fn decode_locked(router: &mut Rings, me: Option<usize>) -> Pass {
    let readers = READERS.load(Ordering::Acquire);
    let mut pass = Pass::default();
    while let Some(key) = keyboard::read_key() {
        match route_key(router, key, readers) {
            Routed::To(to) => pass.elsewhere |= Some(to) != me,
            Routed::Dropped(to) => {
                if DROPPING.fetch_or(1 << to, Ordering::Relaxed) & 1 << to == 0 {
                    pass.dropping = Some(to);
                }
            }
            Routed::Refused(to, e) => pass.refused = Some((to, e)),
            Routed::Left(_) => pass.left = true,
            Routed::Switched(_) | Routed::NoShell(_) | Routed::Modifier | Routed::Desktop => {}
        }
    }
    pass
}

/// After a pass, with every lock released: wake the shells a key went to,
/// log what is worth it (the logger draws on the screen at `info` and up),
/// and wake the compositor if the desktop was left.
fn after_pass(pass: Pass) {
    if pass.elsewhere {
        keyboard::wake_readers();
    }
    if let Some((to, e)) = pass.refused {
        log::info!(target: "term", "not showing terminal {}: {e}", to + 1);
    }
    if let Some(to) = pass.dropping {
        log::warn!(target: "term", "terminal {}'s shell is behind: dropping keys typed on it", to + 1);
    }
    if pass.left {
        crate::desktop::wake();
    }
}

/// The next key typed on virtual terminal `vt`, if one is waiting (never
/// blocks): first the keyboard's waiting bytes are decoded and each key
/// routed ([`route_key`]). Wakes the other terminals' shells if a key went
/// to theirs. Of the refusals and new bursts of drops in one pass, each is
/// counted and the last of each logged.
pub fn read_key(vt: usize) -> Option<Key> {
    assert!(vt < VTS, "no virtual terminal {vt}");
    let (key, pass) = {
        let mut router = ROUTER.lock();
        let pass = decode_locked(&mut router, Some(vt));
        let key = router.take(vt);
        publish(&router);
        (key, pass)
    };
    after_pass(pass);
    key
}

/// The decoding pass alone, for the compositor (which takes no key of a
/// terminal's): the keyboard's waiting bytes decoded, each key routed (to
/// the desktop's ring while it is shown), the shells a key went to woken.
/// It holds [`ROUTER`]'s lock, so the caller holds none of the desktop's.
pub fn decode_pending() {
    let pass = {
        let mut router = ROUTER.lock();
        let pass = decode_locked(&mut router, None);
        publish(&router);
        pass
    };
    after_pass(pass);
}

/// Alt+F`n` while the desktop is shown (under [`ROUTER`]'s lock, the press
/// already in the key ring): give the screen back and show terminal `to`
/// (`term::leave_desktop`), clear [`DESKTOP`], and write the cut (paused,
/// both rings' newest: the press is the newest key event). The compositor
/// is woken after the locks ([`after_pass`]).
fn leave_desktop(to: usize) {
    let owner = CUT.lock().owner;
    if let Some(owner) = owner {
        // False only if the desktop no longer holds the screen (a crash
        // report began): paused all the same.
        let _ = term::leave_desktop(owner, to);
    }
    DESKTOP.store(false, Ordering::Release);
    let marks = Marks::now();
    let mut cut = CUT.lock();
    cut.generation += 1;
    cut.pause = marks;
}

/// Show the desktop (start or resume it; `crate::desktop`): under
/// [`ROUTER`]'s lock, decode the keyboard's waiting bytes (into the shown
/// terminal: typed before this), take the whole screen for `owner`
/// (`term::take_screen`), write the cut (shown, both rings' newest, the
/// pointer ring's newest frame's buttons as the baseline) and set
/// [`DESKTOP`]: nothing typed before reaches a window, nothing typed after
/// reaches a terminal. On an error nothing changes but the decoding. The
/// caller wakes the compositor.
pub fn enter_desktop(owner: Owner) -> Result<ScreenCanvas, CanvasError> {
    let (taken, pass) = {
        let mut router = ROUTER.lock();
        let pass = decode_locked(&mut router, None);
        publish(&router);
        let taken = term::take_screen(owner);
        if taken.is_ok() {
            let marks = Marks::now();
            let baseline = crate::input::POINTER.newest_event().map_or(Buttons::NONE, |(_, e)| e.state.buttons);
            let mut cut = CUT.lock();
            cut.generation += 1;
            cut.resume = marks;
            cut.baseline = baseline;
            cut.owner = Some(owner);
            drop(cut);
            DESKTOP.store(true, Ordering::Release);
        }
        (taken, pass)
    };
    after_pass(pass);
    taken
}

/// The next key typed on virtual terminal `vt`, waiting for one (blocks the
/// calling thread; without a keyboard it never returns).
pub fn wait_key(vt: usize) -> Key {
    assert!(vt < VTS, "no virtual terminal {vt}");
    loop {
        if let Some(key) = read_key(vt) {
            return key;
        }
        keyboard::wait_for_bytes_or(|| HAS_KEYS.load(Ordering::Acquire) & 1 << vt != 0);
    }
}

/// Show virtual terminal `vt` (0 is the first): keys routed from now on go
/// to it. Refused (and counted) while a command has the screen
/// (`term::Canvas`), unless `vt` is shown already.
pub fn show(vt: usize) -> Result<(), ShowError> {
    // Under the router's lock: a key being routed lands on one side of the
    // switch.
    let _router = ROUTER.lock();
    show_counted(vt)
}

/// What the shell's `vt` shows.
pub struct Stats {
    /// Keys each terminal dropped because its ring was full (its shell fell
    /// behind).
    pub dropped: [u64; VTS],
    /// Keys each terminal discarded because no shell reads it.
    pub discarded: [u64; VTS],
    /// Switches refused because a command had the screen.
    pub refused: u64,
    /// Presses that went to the desktop alone while it was shown.
    pub to_desktop: u64,
}

pub fn stats() -> Stats {
    let router = ROUTER.lock();
    Stats {
        dropped: core::array::from_fn(|vt| router.dropped(vt)),
        discarded: core::array::from_fn(|vt| DISCARDED[vt].load(Ordering::Relaxed)),
        refused: REFUSED.load(Ordering::Relaxed),
        to_desktop: TO_DESKTOP.load(Ordering::Relaxed),
    }
}

/// A raw key for the ktests: `code`, with Alt held or not.
#[cfg(feature = "ktest")]
fn raw_key(code: KeyCode, alt: bool) -> Key {
    Key::Raw { code, mods: if alt { lumen_input::Mods::NONE.with_alt() } else { lumen_input::Mods::NONE } }
}

crate::ktest!(
    fn alt_f1_to_f4_switch_terminals() {
        // Alt with F1 to F4 is a switch to terminals 0 to 3; F5, or F1
        // without Alt, or a character typed with Alt, isn't. Modifiers on
        // their own are skipped; other keys aren't.
        use crate::{kassert, kassert_eq};
        let raw = raw_key;
        kassert_eq!(switch_of(raw(KeyCode::F1, true)), Some(0));
        kassert_eq!(switch_of(raw(KeyCode::F4, true)), Some(3));
        kassert_eq!(switch_of(raw(KeyCode::F5, true)), None);
        kassert_eq!(switch_of(raw(KeyCode::F1, false)), None);
        kassert_eq!(switch_of(Key::Char('1')), None);
        kassert_eq!(switch_key(KeyCode::F2, true), Some(1));
        kassert!(bare_modifier(raw(KeyCode::LAlt, true)) && bare_modifier(raw(KeyCode::LShift, false)));
        kassert!(!bare_modifier(raw(KeyCode::ArrowUp, false)) && !bare_modifier(Key::Char('a')));
        Ok(())
    }
);

crate::ktest!(
    fn keys_go_to_the_terminal_shown_when_routed() {
        // Each key goes to the terminal shown when it is routed: the first
        // shown, then the one Alt+F3 switches to; Alt+F2 while a command on
        // the third holds the canvas is refused and the next key still goes
        // to the third; a canvas a command on the third takes while the
        // first is shown brings the third to the front, and keys follow it.
        // A shown terminal without a shell gets nothing (counted); bare
        // modifiers take no slot.
        use crate::kassert_eq;
        use term::Owner;
        const ALL: usize = (1 << VTS) - 1;
        let raw = raw_key;
        let mut r = Rings::new();
        kassert_eq!(term::shown(), 0);
        let _first = term::ShowFirstAfter;
        kassert_eq!(route_key(&mut r, Key::Char('a'), ALL), Routed::To(0));
        kassert_eq!(route_key(&mut r, raw(KeyCode::LShift, false), ALL), Routed::Modifier);
        kassert_eq!(route_key(&mut r, raw(KeyCode::F3, true), ALL), Routed::Switched(2));
        kassert_eq!(route_key(&mut r, Key::Char('b'), ALL), Routed::To(2));
        let canvas = term::canvas(Owner::on_vt(2)).map_err(|_| "no canvas")?;
        kassert_eq!(route_key(&mut r, raw(KeyCode::F2, true), ALL), Routed::Refused(1, ShowError::InUse));
        kassert_eq!(route_key(&mut r, Key::Char('c'), ALL), Routed::To(2));
        drop(canvas);
        kassert_eq!(route_key(&mut r, raw(KeyCode::F1, true), ALL), Routed::Switched(0));
        let canvas = term::canvas(Owner::on_vt(2)).map_err(|_| "no canvas")?;
        kassert_eq!(route_key(&mut r, Key::Char('d'), ALL), Routed::To(2));
        drop(canvas);
        kassert_eq!(term::show(0), Ok(()));
        let discarded = stats().discarded;
        kassert_eq!(route_key(&mut r, Key::Char('e'), ALL & !1), Routed::NoShell(0));
        kassert_eq!(stats().discarded[0], discarded[0] + 1);
        let ring = |r: &mut Rings, vt| core::iter::from_fn(|| r.take(vt)).collect::<alloc::vec::Vec<_>>();
        kassert_eq!(ring(&mut r, 0), alloc::vec![Key::Char('a')]);
        kassert_eq!(ring(&mut r, 2), alloc::vec![Key::Char('b'), Key::Char('c'), Key::Char('d')]);
        kassert_eq!(r.mask(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn showing_a_terminal_switches_the_screen_unless_a_command_has_it() {
        // `show` switches the screen (where keys are routed from then on);
        // a terminal that doesn't exist, or another terminal while a command
        // has the screen, changes nothing (only the second refusal is
        // counted); showing the one shown succeeds.
        use crate::{kassert, kassert_eq};
        kassert_eq!(term::shown(), 0);
        let _first = term::ShowFirstAfter;
        show(2).map_err(|_| "no switch")?;
        kassert_eq!(term::shown(), 2);
        let refused = stats().refused;
        kassert_eq!(show(VTS), Err(ShowError::NoSuchTerminal));
        kassert_eq!(stats().refused, refused);
        let canvas = term::canvas(term::Owner::new()).map_err(|_| "no canvas")?;
        kassert_eq!(show(0), Err(ShowError::InUse));
        kassert_eq!(stats().refused, refused + 1);
        kassert!(show(2).is_ok());
        kassert_eq!(term::shown(), 2);
        drop(canvas);
        kassert!(show(0).is_ok());
        kassert_eq!(term::shown(), 0);
        Ok(())
    }
);

/// Shells attached to every terminal until dropped (the test kernel runs
/// none), which drains the keyboard's queue and detaches them, discarding
/// their keys.
#[cfg(feature = "ktest")]
struct Attached;

#[cfg(feature = "ktest")]
impl Attached {
    fn all() -> Attached {
        (0..VTS).for_each(attach);
        Attached
    }
}

#[cfg(feature = "ktest")]
impl Drop for Attached {
    fn drop(&mut self) {
        while keyboard::read_key().is_some() {}
        (0..VTS).for_each(detach);
    }
}

crate::ktest!(
    fn typed_keys_reach_the_terminal_shown_when_they_are_read() {
        // Scancodes for `a`, Alt+F2 and `b`, decoded in one pass: `a` is the
        // first terminal's, terminal 2 is shown and `b` is its. Then a canvas
        // a command on the third takes shows the third, and a `c` typed
        // meanwhile is the third's, not the second's. (Set 1: a 0x1e, left
        // Alt 0x38, F2 0x3c, b 0x30, c 0x2e; a release adds 0x80.)
        use crate::{kassert, kassert_eq};
        kassert_eq!(term::shown(), 0);
        let _first = term::ShowFirstAfter;
        let _attached = Attached::all();
        kassert!(keyboard::queue_for_test(&[0x1e, 0x9e, 0x38, 0x3c, 0xbc, 0xb8, 0x30, 0xb0]));
        kassert_eq!(read_key(0), Some(Key::Char('a')));
        kassert_eq!(term::shown(), 1);
        kassert_eq!((read_key(1), read_key(1), read_key(0)), (Some(Key::Char('b')), None, None));
        let canvas = term::canvas(term::Owner::on_vt(2)).map_err(|_| "no canvas")?;
        kassert!(keyboard::queue_for_test(&[0x2e, 0xae]));
        kassert_eq!(read_key(1), None);
        kassert_eq!(read_key(2), Some(Key::Char('c')));
        drop(canvas);
        Ok(())
    }
);

crate::ktest!(
    fn a_full_ring_drops_and_counts_and_a_detached_one_discards() {
        // 129 presses of `a` typed on the first terminal while its shell
        // reads nothing: its ring keeps 128 and drops one, counted for it
        // alone (and its burst marked). Taking its shell off discards the
        // 128 kept, counted, and ends the burst. (Set 1: `a` 0x1e, released
        // 0x9e; queued in batches the keyboard's byte queue holds.)
        use crate::{kassert, kassert_eq};
        kassert_eq!(term::shown(), 0);
        let attached = Attached::all();
        let before = stats();
        kassert!(keyboard::queue_for_test(&[0x1e; RING]));
        kassert_eq!(read_key(1), None);
        kassert!(keyboard::queue_for_test(&[0x1e, 0x9e]));
        kassert_eq!(read_key(1), None);
        let after = stats();
        let dropped: [u64; VTS] = core::array::from_fn(|vt| after.dropped[vt] - before.dropped[vt]);
        kassert_eq!(dropped, [1, 0, 0, 0]);
        kassert!(DROPPING.load(Ordering::Relaxed) & 1 != 0);
        kassert!(HAS_KEYS.load(Ordering::Acquire) & 1 != 0);
        drop(attached);
        kassert_eq!(stats().discarded[0], after.discarded[0] + RING as u64);
        kassert_eq!((HAS_KEYS.load(Ordering::Acquire), DROPPING.load(Ordering::Relaxed) & 1), (0, 0));
        Ok(())
    }
);

/// Clears [`DESKTOP`] when dropped: a ktest that shows the desktop keeps
/// one, so failing half-way doesn't leave the next tests' keys the
/// desktop's.
#[cfg(feature = "ktest")]
struct DesktopOffAfter;

#[cfg(feature = "ktest")]
impl Drop for DesktopOffAfter {
    fn drop(&mut self) {
        DESKTOP.store(false, Ordering::Release);
    }
}

crate::ktest!(
    fn keys_while_the_desktop_is_shown_are_its_own_until_alt_f_leaves_it() {
        // `enter_desktop` takes the whole screen and writes the cut (one
        // generation on, shown, the rings' newest); a command's canvas is
        // refused meanwhile. While shown, a key goes to the desktop alone
        // (counted), a bare modifier is skipped, Alt+F2 leaves it: the
        // screen given back with terminal 2 shown, `DESKTOP` clear, the cut
        // paused at the rings' newest. Then keys go to the terminals again
        // and a canvas can be taken.
        use crate::{kassert, kassert_eq};
        const ALL: usize = (1 << VTS) - 1;
        kassert_eq!(term::shown(), 0);
        let _first = term::ShowFirstAfter;
        let _off = DesktopOffAfter;
        let mut r = Rings::new();
        let before = CUT.lock().generation;
        let owner = Owner::new();
        let screen = enter_desktop(owner).map_err(|_| "no screen")?;
        let cut = *CUT.lock();
        kassert_eq!((cut.generation, cut.owner), (before + 1, Some(owner)));
        kassert_eq!(cut.resume, Marks::now());
        kassert!(DESKTOP.load(Ordering::Acquire));
        kassert!(matches!(term::canvas(Owner::new()), Err(CanvasError::InUse)));
        let sent = stats().to_desktop;
        kassert_eq!(route_key(&mut r, Key::Char('a'), ALL), Routed::Desktop);
        kassert_eq!(route_key(&mut r, raw_key(KeyCode::LShift, false), ALL), Routed::Modifier);
        kassert_eq!(stats().to_desktop, sent + 1);
        kassert_eq!(route_key(&mut r, raw_key(KeyCode::F2, true), ALL), Routed::Left(1));
        kassert!(!DESKTOP.load(Ordering::Acquire) && !screen.is_live());
        kassert_eq!(term::shown(), 1);
        let cut = *CUT.lock();
        kassert_eq!((cut.generation, cut.pause), (before + 2, Marks::now()));
        drop(screen);
        kassert_eq!(route_key(&mut r, Key::Char('b'), ALL), Routed::To(1));
        kassert_eq!(r.mask(), 1 << 1);
        drop(term::canvas(Owner::new()).map_err(|_| "no canvas after the desktop")?);
        Ok(())
    }
);
