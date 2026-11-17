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
//! which terminal is shown, and for a switch); nothing takes [`ROUTER`]
//! while holding either. [`HAS_KEYS`] mirrors which rings hold keys, so a
//! reader's wake-up check takes no lock of its own (it runs under the wait
//! queue's lock, so not [`ROUTER`]'s).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::keyboard::{self, Key, KeyCode};
use crate::sync::SpinLock;
use crate::term::{self, ShowError, VTS};
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

/// Alt+F`n` shows terminal `n` (from 1; the first [`VTS`] of them).
const SWITCH_KEYS: [KeyCode; 4] = [KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4];

/// The terminal Alt+F`n` shows (`n` 1 to [`VTS`]), if `key` is one.
fn switch_key(key: Key) -> Option<usize> {
    let Key::Raw { code, mods } = key else { return None };
    if !mods.is_alt() {
        return None;
    }
    SWITCH_KEYS[..VTS].iter().position(|&f| f == code)
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
/// of `readers`).
pub fn route_key(router: &mut Rings, key: Key, readers: usize) -> Routed {
    if let Some(to) = switch_key(key) {
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

/// The next key typed on virtual terminal `vt`, if one is waiting (never
/// blocks): first the keyboard's waiting bytes are decoded and each key
/// routed ([`route_key`]). Wakes the other terminals' shells if a key went
/// to theirs. Of the refusals and new bursts of drops in one pass, each is
/// counted and the last of each logged.
pub fn read_key(vt: usize) -> Option<Key> {
    assert!(vt < VTS, "no virtual terminal {vt}");
    // Logged after the locks (the logger draws on the screen at `info` and
    // up).
    let (mut refused, mut dropping) = (None, None);
    let (key, elsewhere) = {
        let mut router = ROUTER.lock();
        let readers = READERS.load(Ordering::Acquire);
        let mut elsewhere = false;
        while let Some(key) = keyboard::read_key() {
            match route_key(&mut router, key, readers) {
                Routed::To(to) => elsewhere |= to != vt,
                Routed::Dropped(to) => {
                    if DROPPING.fetch_or(1 << to, Ordering::Relaxed) & 1 << to == 0 {
                        dropping = Some(to);
                    }
                }
                Routed::Refused(to, e) => refused = Some((to, e)),
                Routed::Switched(_) | Routed::NoShell(_) | Routed::Modifier => {}
            }
        }
        let key = router.take(vt);
        publish(&router);
        (key, elsewhere)
    };
    if elsewhere {
        keyboard::wake_readers();
    }
    if let Some((to, e)) = refused {
        log::info!(target: "term", "not showing terminal {}: {e}", to + 1);
    }
    if let Some(to) = dropping {
        log::warn!(target: "term", "terminal {}'s shell is behind: dropping keys typed on it", to + 1);
    }
    key
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
}

pub fn stats() -> Stats {
    let router = ROUTER.lock();
    Stats {
        dropped: core::array::from_fn(|vt| router.dropped(vt)),
        discarded: core::array::from_fn(|vt| DISCARDED[vt].load(Ordering::Relaxed)),
        refused: REFUSED.load(Ordering::Relaxed),
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
        kassert_eq!(switch_key(raw(KeyCode::F1, true)), Some(0));
        kassert_eq!(switch_key(raw(KeyCode::F4, true)), Some(3));
        kassert_eq!(switch_key(raw(KeyCode::F5, true)), None);
        kassert_eq!(switch_key(raw(KeyCode::F1, false)), None);
        kassert_eq!(switch_key(Key::Char('1')), None);
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
