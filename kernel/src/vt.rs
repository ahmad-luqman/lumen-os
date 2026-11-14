//! Virtual terminals' keys: the keyboard's keys go to the shell of the
//! terminal shown when each was typed (`lumen_term::vt::Router`), and
//! Alt+F1 to Alt+F4 switch which terminal is shown ([`term::show`]),
//! keys typed after the switch going to the new one.
//!
//! Every reader decodes: whichever screen shell runs first after IRQ 1
//! decodes the waiting bytes and routes each key under [`ROUTER`]'s lock,
//! then wakes the others if it routed one elsewhere (all of them wait on
//! the keyboard's wait queue, [`keyboard::wait_for_bytes_or`]). Lock order:
//! [`ROUTER`], then the keyboard's decoder and the screen's lock (a switch);
//! nothing takes [`ROUTER`] while holding either. [`HAS_KEYS`] mirrors which
//! rings hold keys, so a reader's wake-up check takes no lock.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::keyboard::{self, Key, KeyCode};
use crate::sync::SpinLock;
use crate::term::{self, ShowError, VTS};
use lumen_term::vt::Router;

/// Keys each terminal's ring holds at most (a reader that falls this far
/// behind loses the newest, counted).
const RING: usize = 64;

static ROUTER: SpinLock<Router<Key, VTS, RING>> = SpinLock::new(Router::new());
/// Bit `vt` set while terminal `vt`'s ring holds keys (changed under
/// [`ROUTER`]'s lock).
static HAS_KEYS: AtomicUsize = AtomicUsize::new(0);

/// The terminal Alt+F`n` shows (`n` 1 to [`VTS`]), if `key` is one.
fn switch_key(key: Key) -> Option<usize> {
    let Key::Raw { code, alt: true, .. } = key else { return None };
    [KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4].iter().take(VTS).position(|&f| f == code)
}

/// Decode the keyboard's waiting bytes and route each key (switching on
/// Alt+F`n`), then take the oldest key for `vt`. Wakes the other readers if
/// a key went elsewhere.
fn route_and_take(vt: usize) -> Option<Key> {
    let (key, elsewhere, refused) = {
        let mut router = ROUTER.lock();
        let (mut elsewhere, mut refused) = (false, None);
        while let Some(key) = keyboard::read_key() {
            // A command's canvas may have brought its terminal to the front.
            router.show(term::shown());
            match switch_key(key) {
                Some(to) => match term::show(to) {
                    Ok(()) => {
                        router.show(to);
                    }
                    Err(e) => refused = Some((to, e)),
                },
                None => {
                    router.route(key);
                    elsewhere |= router.shown() != vt;
                }
            }
        }
        let key = router.take(vt);
        update_has_keys(&router);
        (key, elsewhere, refused)
    };
    if elsewhere {
        keyboard::wake_readers();
    }
    // Logged outside the locks (the logger takes the screen's).
    if let Some((to, e)) = refused {
        log::debug!(target: "term", "not showing terminal {}: {e}", to + 1);
    }
    key
}

/// Mirror which rings hold keys into [`HAS_KEYS`].
fn update_has_keys(router: &Router<Key, VTS, RING>) {
    let mask = (0..VTS).filter(|&vt| router.waiting(vt) > 0).fold(0, |m, vt| m | 1 << vt);
    HAS_KEYS.store(mask, Ordering::Release);
}

/// The next key typed on virtual terminal `vt`, if one is waiting (never
/// blocks).
pub fn read_key(vt: usize) -> Option<Key> {
    route_and_take(vt)
}

/// The next key typed on virtual terminal `vt`, waiting for one (blocks the
/// calling thread; without a keyboard it never returns).
pub fn wait_key(vt: usize) -> Key {
    loop {
        if let Some(key) = route_and_take(vt) {
            return key;
        }
        keyboard::wait_for_bytes_or(|| HAS_KEYS.load(Ordering::Acquire) & 1 << vt != 0);
    }
}

/// Show virtual terminal `vt` (0 is the first): keys typed from now on go
/// to it. Refused while a command has the screen (`term::Canvas`).
pub fn show(vt: usize) -> Result<(), ShowError> {
    let mut router = ROUTER.lock();
    term::show(vt)?;
    router.show(vt);
    Ok(())
}

/// Keys dropped because their terminal's shell fell too far behind.
pub fn dropped() -> u64 {
    ROUTER.lock().dropped()
}

crate::ktest!(
    fn alt_f1_to_f4_switch_terminals() {
        // Alt with F1 to F4 is a switch to terminals 0 to 3; F5, or F1
        // without Alt, or a character typed with Alt, isn't.
        use crate::kassert_eq;
        let raw = |code, alt| Key::Raw { code, shift: false, alt };
        kassert_eq!(switch_key(raw(KeyCode::F1, true)), Some(0));
        kassert_eq!(switch_key(raw(KeyCode::F4, true)), Some(3));
        kassert_eq!(switch_key(raw(KeyCode::F5, true)), None);
        kassert_eq!(switch_key(raw(KeyCode::F1, false)), None);
        kassert_eq!(switch_key(Key::Char('1')), None);
        Ok(())
    }
);

crate::ktest!(
    fn showing_a_terminal_moves_the_keys_and_the_screen() {
        // `show` switches the screen and where keys go together; a terminal
        // that doesn't exist, or a switch while a command has the screen,
        // changes neither.
        use crate::{kassert, kassert_eq};
        kassert_eq!(term::shown(), 0);
        show(2).map_err(|_| "no switch")?;
        kassert_eq!((term::shown(), ROUTER.lock().shown()), (2, 2));
        kassert_eq!(show(VTS), Err(ShowError::NoSuchTerminal));
        let canvas = term::canvas(term::Owner::new()).map_err(|_| "no canvas")?;
        kassert_eq!(show(0), Err(ShowError::InUse));
        kassert_eq!((term::shown(), ROUTER.lock().shown()), (2, 2));
        drop(canvas);
        kassert!(show(0).is_ok());
        kassert_eq!((term::shown(), ROUTER.lock().shown()), (0, 0));
        Ok(())
    }
);
