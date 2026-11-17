//! Desktop events (#151; ARCHITECTURE section 7): the tablet's frames and
//! the keyboard's key events as the rings hold them, and the decoder that
//! turns scancode set 1 bytes into key events.
//!
//! [`KeyDecoder`] wraps `pc-keyboard`'s set 1 decoder: each byte fed to it
//! may finish a key event, which goes to the key ring ([`KeyEvent`]:
//! presses and releases, bare modifiers included, with the modifier state
//! after it), and a press also comes back as `pc-keyboard`'s
//! `DecodedKey` for the terminals, as before #151. `pc-keyboard`'s
//! pseudo-keys are handled so that one key is one press, as `kbd`'s
//! `presses` ([`crate::Presses`]) counts it:
//!
//! - `RAlt2`, the fake shift QEMU sends around Print Screen (`E0 2A`, `E0
//!   AA`), is not pushed;
//! - Pause (`E1 1D 45 E1 9D C5`) is pushed as one press of
//!   `KeyCode::PauseBreak` with no release: its pieces (`RControl2`, the
//!   `NumpadLock` press with it held, and both releases) are not;
//! - Ctrl+Pause sends `E0 46` (`E0 C6` released), which `pc-keyboard`
//!   0.9.0 doesn't map: the decoder takes it before `pc-keyboard` sees it,
//!   as one press of `KeyCode::PauseBreak` (its release not pushed);
//! - a `KeyState::SingleShot` key is pushed as a press then a release.
//!
//! Typematic repeat shows as repeated presses with no release between.

pub use pc_keyboard::KeyCode;
use pc_keyboard::{DecodedKey, HandleControl, KeyState, KeyboardLayout, Modifiers, PS2Keyboard, ScancodeSet1};

use crate::PointerState;

/// A tablet frame, and whether frames may have been lost at the device
/// before it ([`crate::AfterOverrun`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerEvent {
    pub state: PointerState,
    pub after_overrun: bool,
}

/// A key pressed or released, with the modifiers after it: Shift+a is
/// press `LShift` (shifted), press `A` (shifted, text `A`), release `A`
/// (shifted), release `LShift` (not shifted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The physical key.
    pub code: KeyCode,
    pub action: KeyAction,
    /// The modifier state after this event.
    pub mods: Mods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// `text`: `pc-keyboard`'s `DecodedKey::Unicode` for this press, if it
    /// made one (control characters included: Enter is '\n', Ctrl+C
    /// U+0003).
    Press {
        text: Option<char>,
    },
    Release,
}

/// The modifier state (`pc_keyboard::Modifiers` isn't `Copy` in 0.9.0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods(u8);

impl Mods {
    const LSHIFT: u8 = 1 << 0;
    const RSHIFT: u8 = 1 << 1;
    const LCTRL: u8 = 1 << 2;
    const RCTRL: u8 = 1 << 3;
    const LALT: u8 = 1 << 4;
    const RALT: u8 = 1 << 5;
    const CAPSLOCK: u8 = 1 << 6;
    const NUMLOCK: u8 = 1 << 7;

    /// No modifier held, no lock on.
    pub const NONE: Mods = Mods(0);

    const fn has(self, bits: u8) -> bool {
        self.0 & bits != 0
    }

    /// Either Shift held.
    pub const fn is_shifted(self) -> bool {
        self.has(Self::LSHIFT | Self::RSHIFT)
    }

    /// Either Ctrl held.
    pub const fn is_ctrl(self) -> bool {
        self.has(Self::LCTRL | Self::RCTRL)
    }

    /// Either Alt held.
    pub const fn is_alt(self) -> bool {
        self.has(Self::LALT | Self::RALT)
    }

    /// Right Alt, or Ctrl with left Alt (as `pc-keyboard` says).
    pub const fn is_altgr(self) -> bool {
        self.has(Self::RALT) || (self.has(Self::LALT) && self.is_ctrl())
    }

    pub const fn capslock(self) -> bool {
        self.has(Self::CAPSLOCK)
    }

    pub const fn numlock(self) -> bool {
        self.has(Self::NUMLOCK)
    }
}

impl From<&Modifiers> for Mods {
    fn from(m: &Modifiers) -> Mods {
        let bits = [
            (m.lshift, Mods::LSHIFT),
            (m.rshift, Mods::RSHIFT),
            (m.lctrl, Mods::LCTRL),
            (m.rctrl, Mods::RCTRL),
            (m.lalt, Mods::LALT),
            (m.ralt, Mods::RALT),
            (m.capslock, Mods::CAPSLOCK),
            (m.numlock, Mods::NUMLOCK),
        ];
        Mods(bits.iter().filter(|(on, _)| *on).fold(0, |acc, (_, bit)| acc | bit))
    }
}

/// The compositor's merge of both rings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Pointer(PointerEvent),
}

/// The extended-key prefix in set 1.
const E0: u8 = 0xE0;
/// Ctrl+Pause's code after `E0` (`0xC6` released).
const CTRL_PAUSE: u8 = 0x46;

/// Scancode set 1 bytes into key events (see the module doc).
pub struct KeyDecoder<L: KeyboardLayout> {
    keyboard: PS2Keyboard<L, ScancodeSet1>,
    /// An `E0` held back from `pc-keyboard`: Ctrl+Pause's (`E0 46`) or
    /// any other extended key's, which the next byte tells.
    held_e0: bool,
    /// A Pause press was pushed: its `NumpadLock` release, which comes
    /// after `RControl2`'s, isn't.
    pause_release_due: bool,
}

impl<L: KeyboardLayout> KeyDecoder<L> {
    pub const fn new(layout: L, handle_ctrl: HandleControl) -> Self {
        Self {
            keyboard: PS2Keyboard::new(ScancodeSet1::new(), layout, handle_ctrl),
            held_e0: false,
            pause_release_due: false,
        }
    }

    /// The modifier state now.
    pub fn modifiers(&self) -> &Modifiers {
        self.keyboard.get_modifiers()
    }

    /// Feed one byte: `push` gets each key event it finishes (none, one, or
    /// two for a single-shot key); a press also comes back as
    /// `pc-keyboard`'s decoding of it (a character, or a raw key code), as
    /// the terminals take keys. An error is a byte `pc-keyboard` refused.
    pub fn add_byte(
        &mut self,
        byte: u8,
        mut push: impl FnMut(KeyEvent),
    ) -> Result<Option<DecodedKey>, pc_keyboard::Error> {
        if self.held_e0 {
            self.held_e0 = false;
            match byte {
                CTRL_PAUSE => {
                    self.pause_release_due = false;
                    push(self.event(KeyCode::PauseBreak, KeyAction::Press { text: None }));
                    return Ok(Some(DecodedKey::RawKey(KeyCode::PauseBreak)));
                }
                b if b == CTRL_PAUSE | 0x80 => return Ok(None),
                _ => {
                    // Not Ctrl+Pause: `pc-keyboard` gets the prefix after all.
                    let none = self.keyboard.add_byte(E0)?;
                    debug_assert!(none.is_none(), "E0 only starts a key");
                }
            }
        } else if byte == E0 {
            self.held_e0 = true;
            return Ok(None);
        }
        let Some(raw) = self.keyboard.add_byte(byte)? else { return Ok(None) };
        // Pause: `NumpadLock` pressed with `RControl2` held.
        let pause = raw.code == KeyCode::NumpadLock && raw.state == KeyState::Down && self.modifiers().rctrl2;
        let decoded = self.keyboard.process_keyevent(raw.clone());
        let releasing_pause = self.pause_release_due;
        self.pause_release_due = false;
        match (raw.code, raw.state) {
            (KeyCode::RAlt2, _) => {}
            (KeyCode::RControl2, KeyState::Up) => self.pause_release_due = releasing_pause,
            (KeyCode::RControl2, _) => {}
            (KeyCode::NumpadLock, KeyState::Up) if releasing_pause => {}
            (_, KeyState::Down) if pause => {
                self.pause_release_due = true;
                push(self.event(KeyCode::PauseBreak, KeyAction::Press { text: None }));
            }
            (code, KeyState::Down) => push(self.event(code, KeyAction::Press { text: text_of(decoded) })),
            (code, KeyState::Up) => push(self.event(code, KeyAction::Release)),
            (code, KeyState::SingleShot) => {
                push(self.event(code, KeyAction::Press { text: text_of(decoded) }));
                push(self.event(code, KeyAction::Release));
            }
        }
        Ok(match raw.state {
            KeyState::Up => None,
            _ => decoded,
        })
    }

    fn event(&self, code: KeyCode, action: KeyAction) -> KeyEvent {
        KeyEvent { code, action, mods: Mods::from(self.modifiers()) }
    }
}

fn text_of(decoded: Option<DecodedKey>) -> Option<char> {
    match decoded {
        Some(DecodedKey::Unicode(c)) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use pc_keyboard::layouts::{De105Key, Us104Key};

    use super::*;
    use crate::Presses;

    fn us() -> KeyDecoder<Us104Key> {
        KeyDecoder::new(Us104Key, HandleControl::MapLettersToUnicode)
    }

    /// The key events `bytes` make, and how many bytes were refused.
    fn events(bytes: &[u8]) -> (Vec<KeyEvent>, usize) {
        let mut d = us();
        let (mut out, mut refused) = (Vec::new(), 0);
        for &b in bytes {
            if d.add_byte(b, |e| out.push(e)).is_err() {
                refused += 1;
            }
        }
        (out, refused)
    }

    fn press(code: KeyCode, text: Option<char>, mods: Mods) -> KeyEvent {
        KeyEvent { code, action: KeyAction::Press { text }, mods }
    }

    fn release(code: KeyCode, mods: Mods) -> KeyEvent {
        KeyEvent { code, action: KeyAction::Release, mods }
    }

    fn mods(m: Modifiers) -> Mods {
        Mods::from(&m)
    }

    /// Nothing held: pc-keyboard starts with Num Lock on.
    const BASE: Mods = Mods(Mods::NUMLOCK);
    const SHIFT: Mods = Mods(Mods::NUMLOCK | Mods::LSHIFT);

    #[test]
    fn shift_a_is_two_presses_and_two_releases_with_the_state_after_each() {
        // Set 1: left Shift 0x2A (0xAA released), A 0x1E (0x9E).
        let (got, refused) = events(&[0x2A, 0x1E, 0x9E, 0xAA]);
        assert_eq!(refused, 0);
        assert_eq!(
            got,
            [
                press(KeyCode::LShift, None, SHIFT),
                press(KeyCode::A, Some('A'), SHIFT),
                release(KeyCode::A, SHIFT),
                release(KeyCode::LShift, BASE),
            ]
        );
    }

    #[test]
    fn a_press_comes_back_for_the_terminals_and_a_release_doesnt() {
        let mut d = us();
        assert_eq!(d.add_byte(0x1E, |_| {}), Ok(Some(DecodedKey::Unicode('a'))));
        assert_eq!(d.add_byte(0x9E, |_| {}), Ok(None));
        // Ctrl with a letter makes a control character, in the text too.
        let (got, _) = events(&[0x1D, 0x2E, 0xAE, 0x9D]);
        assert_eq!(got[1].action, KeyAction::Press { text: Some('\u{3}') });
        assert!(got[1].mods.is_ctrl());
        // An extended key: an arrow (E0 4B) is a raw code, no text.
        assert_eq!(d.add_byte(0xE0, |_| {}), Ok(None));
        assert_eq!(d.add_byte(0x4B, |_| {}), Ok(Some(DecodedKey::RawKey(KeyCode::ArrowLeft))));
        let (got, _) = events(&[0xE0, 0x4B, 0xE0, 0xCB]);
        assert_eq!(got, [press(KeyCode::ArrowLeft, None, BASE), release(KeyCode::ArrowLeft, BASE)]);
    }

    #[test]
    fn pause_is_one_press() {
        let (got, refused) = events(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
        assert_eq!(refused, 0);
        assert_eq!(got, [press(KeyCode::PauseBreak, None, BASE)]);
        // Num Lock is untouched, and a later Num Lock press and release
        // are both pushed.
        let (got, _) = events(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x45, 0xC5]);
        // (pc-keyboard starts with Num Lock on: a real press turns it off.)
        assert_eq!(
            got,
            [
                press(KeyCode::PauseBreak, None, BASE),
                press(KeyCode::NumpadLock, None, Mods::NONE),
                release(KeyCode::NumpadLock, Mods::NONE),
            ]
        );
    }

    #[test]
    fn ctrl_pause_is_one_press() {
        // Ctrl held, then Pause: E0 46 (E0 C6 released), which pc-keyboard
        // refuses on its own.
        let (got, refused) = events(&[0x1D, 0xE0, 0x46, 0xE0, 0xC6, 0x9D]);
        assert_eq!(refused, 0);
        let ctrl = Mods(Mods::NUMLOCK | Mods::LCTRL);
        assert_eq!(
            got,
            [
                press(KeyCode::LControl, None, ctrl),
                press(KeyCode::PauseBreak, None, ctrl),
                release(KeyCode::LControl, BASE),
            ]
        );
    }

    #[test]
    fn ralt2_is_dropped() {
        // QEMU's Print Screen: E0 2A E0 37, released E0 B7 E0 AA.
        let (got, refused) = events(&[0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA]);
        assert_eq!(refused, 0);
        assert_eq!(got, [press(KeyCode::PrintScreen, None, BASE), release(KeyCode::PrintScreen, BASE)]);
        // The other fake shift doesn't decode at all, as before.
        assert_eq!(events(&[0xE0, 0x36]), (Vec::new(), 1));
    }

    #[test]
    fn typematic_repeat_is_presses_with_no_release_between() {
        let (got, _) = events(&[0x1E, 0x1E, 0x1E, 0x9E]);
        let a = press(KeyCode::A, Some('a'), BASE);
        assert_eq!(got, [a, a, a, release(KeyCode::A, BASE)]);
    }

    #[test]
    fn mods_convert_every_field() {
        let m = |f: fn(&mut Modifiers)| {
            let mut x = Modifiers::default();
            f(&mut x);
            mods(x)
        };
        assert_eq!(mods(Modifiers::default()), Mods::NONE);
        assert!(m(|x| x.lshift = true).is_shifted() && m(|x| x.rshift = true).is_shifted());
        assert!(m(|x| x.lctrl = true).is_ctrl() && m(|x| x.rctrl = true).is_ctrl());
        assert!(m(|x| x.lalt = true).is_alt() && m(|x| x.ralt = true).is_alt());
        assert!(m(|x| x.ralt = true).is_altgr());
        assert!(!m(|x| x.lalt = true).is_altgr());
        assert!(
            m(|x| {
                x.lalt = true;
                x.lctrl = true
            })
            .is_altgr()
        );
        assert!(m(|x| x.capslock = true).capslock() && m(|x| x.numlock = true).numlock());
        // Each field is its own bit, and the hidden rctrl2 none.
        let fields: [fn(&mut Modifiers); 8] = [
            |x| x.lshift = true,
            |x| x.rshift = true,
            |x| x.lctrl = true,
            |x| x.rctrl = true,
            |x| x.lalt = true,
            |x| x.ralt = true,
            |x| x.capslock = true,
            |x| x.numlock = true,
        ];
        let bits: Vec<u8> = fields.iter().map(|f| m(*f).0).collect();
        assert_eq!(bits, [1, 2, 4, 8, 16, 32, 64, 128]);
        assert_eq!(m(|x| x.rctrl2 = true), Mods::NONE);
        // None of the predicates holds for nothing held.
        let n = Mods::NONE;
        assert!(!(n.is_shifted() || n.is_ctrl() || n.is_alt() || n.is_altgr() || n.capslock() || n.numlock()));
    }

    #[test]
    fn the_layout_decides_the_text_not_the_code() {
        // The key at Y's place (0x15) is Z on a German layout.
        let mut de = KeyDecoder::new(De105Key, HandleControl::MapLettersToUnicode);
        let mut got = Vec::new();
        de.add_byte(0x15, |e| got.push(e)).unwrap();
        assert_eq!(got, [press(KeyCode::Y, Some('z'), BASE)]);
    }

    /// Presses counted by `kbd`'s [`Presses`] and pushed by the decoder for
    /// `bytes`, and whether the decoder refused any byte.
    fn both(bytes: &[u8]) -> (u64, u64, bool) {
        let mut p = Presses::new();
        let mut d = us();
        let (mut pushed, mut refused) = (0, false);
        for &b in bytes {
            p.feed(b);
            let r = d.add_byte(b, |e| pushed += u64::from(matches!(e.action, KeyAction::Press { .. })));
            refused |= r.is_err();
        }
        (p.count(), pushed, refused)
    }

    #[test]
    fn presses_and_the_decoder_agree_unless_the_decoder_refuses_a_byte() {
        // One corpus, both counters: every 1-byte make with its break,
        // every E0 make with its break, Print Screen, Pause, Ctrl+Pause,
        // typematic repeat. Where the decoder refuses nothing, the counts
        // agree, but for the exceptions listed below.
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        for code in 0x01..0x80u8 {
            corpus.push(vec![code, code | 0x80]);
            corpus.push(vec![E0, code, E0, code | 0x80]);
        }
        corpus.push(vec![0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA]); // Print Screen
        corpus.push(vec![0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]); // Pause
        corpus.push(vec![0x1D, 0xE0, 0x46, 0xE0, 0xC6, 0x9D]); // Ctrl+Pause
        corpus.push(vec![0x1E, 0x1E, 0x1E, 0x9E]); // typematic repeat
        corpus.push(vec![0x2A, 0x1E, 0x9E, 0xAA]); // Shift+a
        // The known exceptions, where both decode without error but count
        // differently: Pause cut off after its first byte past E1 (Presses
        // counts at 0x1D, the decoder at 0x45, where pc-keyboard tells
        // Pause).
        let exceptions: [&[u8]; 1] = [&[0xE1, 0x1D]];
        let (mut agreed, mut refused) = (0, 0);
        for bytes in &corpus {
            let (presses, pushed, refused_any) = both(bytes);
            if refused_any {
                refused += 1;
                continue;
            }
            assert_eq!(presses, pushed, "{bytes:02x?}");
            agreed += 1;
        }
        for bytes in exceptions {
            let (presses, pushed, refused_any) = both(bytes);
            assert!(!refused_any && presses != pushed, "{bytes:02x?}: {presses} vs {pushed}");
        }
        // Most of the corpus decodes (the unmapped codes are refused).
        assert!(agreed > 100 && refused > 0, "{agreed} agreed, {refused} refused");
    }
}
