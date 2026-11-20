//! [`Event`], what `window_next_event` hands an app (ARCHITECTURE section 8,
//! "Wire formats"): postcard over the serde-derived types here, each
//! `WindowEvent` the desktop delivers becoming one.
//!
//! serde numbers an enum's variants by declaration position (a `repr`
//! discriminant is ignored on the wire), so every wire enum here is
//! declared in its numbered order, field-less ones with explicit
//! discriminants checked against their positions at compile time, and a
//! host test pins every variant's bytes. The conversions from the kernel's
//! types (`lumen-input`'s keys, buttons and modifiers, `lumen-wm`'s theme,
//! `lumen-ui`'s palette and actions) live here too, so those crates need no
//! serde and reordering their variants can't change the ABI.
//!
//! Decoding follows section 8's rules: [`Event::decode`] refuses bytes
//! left over (`postcard::take_from_bytes`, then the remainder), and no type
//! here is recursive or a sequence of zero-sized elements. The host's
//! encoding is at most [`EVENT_BYTES`] (the largest event is tested), so it
//! encodes into a fixed buffer ([`Event::encode`]) and allocates nothing.

use crate::limits::{EVENT_BYTES, SET_VALUE_BYTES};
pub use crate::wire::DecodeError;
use alloc::string::String;
use serde::{Deserialize, Serialize};

wire_enum! {
    /// A desktop theme (`lumen_wm::ThemeId`).
    pub enum ThemeId {
        Light = 0,
        Dark = 1,
    }
}

impl From<lumen_wm::ThemeId> for ThemeId {
    fn from(id: lumen_wm::ThemeId) -> ThemeId {
        match id {
            lumen_wm::ThemeId::Light => ThemeId::Light,
            lumen_wm::ThemeId::Dark => ThemeId::Dark,
        }
    }
}

impl From<ThemeId> for lumen_wm::ThemeId {
    fn from(id: ThemeId) -> lumen_wm::ThemeId {
        match id {
            ThemeId::Light => lumen_wm::ThemeId::Light,
            ThemeId::Dark => lumen_wm::ThemeId::Dark,
        }
    }
}

/// The toolkit's colours for a theme, `0x00RRGGBB` each, in this (wire)
/// order: as the kernel's `ui::palette` gives them, so an app's toolkit
/// looks like a kernel window's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    pub background: u32,
    pub text: u32,
    pub accent: u32,
    pub accent_text: u32,
    pub border: u32,
    pub disabled: u32,
    pub field: u32,
}

impl From<&lumen_ui::Palette> for Palette {
    fn from(p: &lumen_ui::Palette) -> Palette {
        let lumen_ui::Palette { background, text, accent, accent_text, border, disabled, field } = *p;
        Palette { background, text, accent, accent_text, border, disabled, field }
    }
}

impl From<Palette> for lumen_ui::Palette {
    fn from(p: Palette) -> lumen_ui::Palette {
        let Palette { background, text, accent, accent_text, border, disabled, field } = p;
        lumen_ui::Palette { background, text, accent, accent_text, border, disabled, field }
    }
}

/// [`KeyCode`] from one list of `pc_keyboard::KeyCode`'s variants (0.9.0's,
/// in its order): the wire enum, and both conversions as exhaustive
/// matches, so a variant can't be missed by either.
macro_rules! key_codes {
    ($($variant:ident = $num:literal,)*) => {
        wire_enum! {
            /// A physical key: every `pc_keyboard::KeyCode` pc-keyboard 0.9.0
            /// has, in its order (all below 128: one byte on the wire).
            pub enum KeyCode {
                $($variant = $num,)*
            }
        }

        impl From<lumen_input::KeyCode> for KeyCode {
            fn from(k: lumen_input::KeyCode) -> KeyCode {
                match k {
                    $(lumen_input::KeyCode::$variant => KeyCode::$variant,)*
                }
            }
        }

        impl From<KeyCode> for lumen_input::KeyCode {
            fn from(k: KeyCode) -> lumen_input::KeyCode {
                match k {
                    $(KeyCode::$variant => lumen_input::KeyCode::$variant,)*
                }
            }
        }
    };
}

key_codes! {
    Escape = 0, F1 = 1, F2 = 2, F3 = 3, F4 = 4, F5 = 5, F6 = 6, F7 = 7, F8 = 8, F9 = 9, F10 = 10,
    F11 = 11, F12 = 12, PrintScreen = 13, SysRq = 14, ScrollLock = 15, PauseBreak = 16, Oem8 = 17,
    Key1 = 18, Key2 = 19, Key3 = 20, Key4 = 21, Key5 = 22, Key6 = 23, Key7 = 24, Key8 = 25,
    Key9 = 26, Key0 = 27, OemMinus = 28, OemPlus = 29, Backspace = 30, Insert = 31, Home = 32,
    PageUp = 33, NumpadLock = 34, NumpadDivide = 35, NumpadMultiply = 36, NumpadSubtract = 37,
    Tab = 38, Q = 39, W = 40, E = 41, R = 42, T = 43, Y = 44, U = 45, I = 46, O = 47, P = 48,
    Oem4 = 49, Oem6 = 50, Oem5 = 51, Oem7 = 52, Delete = 53, End = 54, PageDown = 55,
    Numpad7 = 56, Numpad8 = 57, Numpad9 = 58, NumpadAdd = 59, CapsLock = 60, A = 61, S = 62,
    D = 63, F = 64, G = 65, H = 66, J = 67, K = 68, L = 69, Oem1 = 70, Oem3 = 71, Return = 72,
    Numpad4 = 73, Numpad5 = 74, Numpad6 = 75, LShift = 76, Z = 77, X = 78, C = 79, V = 80,
    B = 81, N = 82, M = 83, OemComma = 84, OemPeriod = 85, Oem2 = 86, RShift = 87, ArrowUp = 88,
    Numpad1 = 89, Numpad2 = 90, Numpad3 = 91, NumpadEnter = 92, LControl = 93, LWin = 94,
    LAlt = 95, Spacebar = 96, RAltGr = 97, RWin = 98, Apps = 99, RControl = 100, ArrowLeft = 101,
    ArrowDown = 102, ArrowRight = 103, Numpad0 = 104, NumpadPeriod = 105, Oem9 = 106,
    Oem10 = 107, Oem11 = 108, Oem12 = 109, Oem13 = 110, PrevTrack = 111, NextTrack = 112,
    Mute = 113, Calculator = 114, Play = 115, Stop = 116, VolumeDown = 117, VolumeUp = 118,
    WWWHome = 119, PowerOnTestOk = 120, TooManyKeys = 121, RControl2 = 122, RAlt2 = 123,
    Unknown = 124,
}

/// The modifier byte's bits (`Event::Key`'s `mods`): the state after the
/// key event.
pub mod mods {
    pub const LSHIFT: u8 = 1 << 0;
    pub const RSHIFT: u8 = 1 << 1;
    pub const LCTRL: u8 = 1 << 2;
    pub const RCTRL: u8 = 1 << 3;
    pub const LALT: u8 = 1 << 4;
    pub const RALT: u8 = 1 << 5;
    pub const CAPSLOCK: u8 = 1 << 6;
    pub const NUMLOCK: u8 = 1 << 7;
    /// Either Shift.
    pub const SHIFT: u8 = LSHIFT | RSHIFT;
    /// Either Ctrl.
    pub const CTRL: u8 = LCTRL | RCTRL;
    /// Either Alt.
    pub const ALT: u8 = LALT | RALT;
}

wire_enum! {
    /// A pointer button (`lumen_input::Button`): its number on the wire,
    /// and its bit in a [`Buttons`] byte (`1 << number`).
    pub enum Button {
        Left = 0,
        Right = 1,
        Middle = 2,
        Side = 3,
        Extra = 4,
    }
}

impl From<lumen_input::Button> for Button {
    fn from(b: lumen_input::Button) -> Button {
        match b {
            lumen_input::Button::Left => Button::Left,
            lumen_input::Button::Right => Button::Right,
            lumen_input::Button::Middle => Button::Middle,
            lumen_input::Button::Side => Button::Side,
            lumen_input::Button::Extra => Button::Extra,
        }
    }
}

impl From<Button> for lumen_input::Button {
    fn from(b: Button) -> lumen_input::Button {
        match b {
            Button::Left => lumen_input::Button::Left,
            Button::Right => lumen_input::Button::Right,
            Button::Middle => lumen_input::Button::Middle,
            Button::Side => lumen_input::Button::Side,
            Button::Extra => lumen_input::Button::Extra,
        }
    }
}

/// The modifier state after a key event (`Event::Key`'s `mods`): one raw
/// byte on the wire, [`mods`]' bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mods(u8);

impl Mods {
    pub const fn from_bits(bits: u8) -> Mods {
        Mods(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether any of `bits` ([`mods`]' constants) is set.
    pub const fn any(self, bits: u8) -> bool {
        self.0 & bits != 0
    }

    pub const fn shift(self) -> bool {
        self.any(mods::SHIFT)
    }

    pub const fn ctrl(self) -> bool {
        self.any(mods::CTRL)
    }

    pub const fn alt(self) -> bool {
        self.any(mods::ALT)
    }

    pub const fn capslock(self) -> bool {
        self.any(mods::CAPSLOCK)
    }

    pub const fn numlock(self) -> bool {
        self.any(mods::NUMLOCK)
    }
}

impl From<lumen_input::Mods> for Mods {
    fn from(m: lumen_input::Mods) -> Mods {
        Mods(m.bits())
    }
}

/// The pointer buttons held (a pointer event's `buttons`): one raw byte on
/// the wire, a [`Button`] numbered `n` at bit `1 << n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Buttons(u8);

impl Buttons {
    pub const fn from_bits(bits: u8) -> Buttons {
        Buttons(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether `b` is held.
    pub const fn has(self, b: Button) -> bool {
        self.0 & (1 << b as u8) != 0
    }
}

impl From<lumen_input::Buttons> for Buttons {
    fn from(set: lumen_input::Buttons) -> Buttons {
        Buttons(set.each().fold(0, |bits, b| bits | 1 << Button::from(b) as u8))
    }
}

/// What a key event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    /// Pressed, with the character it typed, if any (control characters
    /// included: Enter is `'\n'`).
    Press {
        text: Option<char>,
    },
    Release,
}

impl From<lumen_input::KeyAction> for KeyAction {
    fn from(a: lumen_input::KeyAction) -> KeyAction {
        match a {
            lumen_input::KeyAction::Press { text } => KeyAction::Press { text },
            lumen_input::KeyAction::Release => KeyAction::Release,
        }
    }
}

/// A key event as `lumen-ui`'s toolkit takes it (releases and the keys it
/// ignores: `None`): Tab (Shift+Tab is `BackTab`), Backspace, Enter (Return
/// or the keypad's), or a press's text that isn't a control character. The
/// kernel's toolkit and an app's (the SDK's) both map keys through this, so
/// the two can't disagree.
pub fn toolkit_key(code: KeyCode, mods: Mods, action: KeyAction) -> Option<lumen_ui::Key> {
    use lumen_ui::Key;
    let KeyAction::Press { text } = action else { return None };
    match (code, text) {
        (KeyCode::Tab, _) if mods.shift() => Some(Key::BackTab),
        (KeyCode::Tab, _) => Some(Key::Tab),
        (KeyCode::Backspace, _) => Some(Key::Backspace),
        (KeyCode::Return | KeyCode::NumpadEnter, _) => Some(Key::Enter),
        (_, Some(c)) if !c.is_control() => Some(Key::Char(c)),
        _ => None,
    }
}

/// A `set_value` request's text: at most [`SET_VALUE_BYTES`], whichever
/// way it is made ([`crate::wire::Capped`]: built with `try_from`, or
/// decoded, a longer one refused with postcard's `SerdeDeCustom`), so every
/// [`Event`] fits [`EVENT_BYTES`].
pub type SetValue = crate::wire::Capped<SET_VALUE_BYTES>;

/// An automation request's action (`lumen_ui::Action`), for the app's
/// node `node`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Press,
    SetValue(SetValue),
    Select,
    Focus,
    Close,
    Minimize,
    Restore,
}

impl From<Action> for lumen_ui::Action {
    fn from(a: Action) -> lumen_ui::Action {
        match a {
            Action::Press => lumen_ui::Action::Press,
            Action::SetValue(text) => lumen_ui::Action::SetValue(String::from(text.as_str())),
            Action::Select => lumen_ui::Action::Select,
            Action::Focus => lumen_ui::Action::Focus,
            Action::Close => lumen_ui::Action::Close,
            Action::Minimize => lumen_ui::Action::Minimize,
            Action::Restore => lumen_ui::Action::Restore,
        }
    }
}

/// One event for an app's window, in its numbered order (pinned by a test).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The theme to draw with: always the first event, and again after
    /// each switch.
    Theme {
        theme: ThemeId,
        palette: Palette,
    },
    Key {
        code: KeyCode,
        mods: Mods,
        action: KeyAction,
    },
    /// Client coordinates; `button` the one pressed or released, `buttons` the
    /// set held after it.
    PointerDown {
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
    },
    PointerUp {
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
    },
    PointerMove {
        x: i32,
        y: i32,
        buttons: Buttons,
    },
    FocusGained,
    /// Every key and button counts as released.
    FocusLost,
    /// Events were dropped: every key and button counts as released.
    Lost,
    /// The close box, or `ui invoke <root> close`: the app's to act on.
    Close,
    /// An automation request for the app's node `node` (its own id; #207).
    Request {
        node: u64,
        action: Action,
    },
}

/// A request's `set_value` text past [`SET_VALUE_BYTES`]: refused (wrong
/// arguments) before it becomes an [`Event`].
pub use crate::wire::TooLong;

impl Event {
    /// A key event (`lumen_input::KeyEvent`; its `after_drop` mark stays
    /// the kernel's).
    pub fn key(k: &lumen_input::KeyEvent) -> Event {
        Event::Key { code: k.code.into(), mods: k.mods.into(), action: k.action.into() }
    }

    /// The theme `id` with the toolkit's `palette` for it.
    pub fn theme(id: lumen_wm::ThemeId, palette: &lumen_ui::Palette) -> Event {
        Event::Theme { theme: id.into(), palette: palette.into() }
    }

    /// A request to the app's node `node`, or [`TooLong`] for a `set_value`
    /// text past [`SET_VALUE_BYTES`].
    pub fn request(node: u64, action: &lumen_ui::Action) -> Result<Event, TooLong> {
        let action = match action {
            lumen_ui::Action::Press => Action::Press,
            lumen_ui::Action::SetValue(text) => Action::SetValue(SetValue::try_from(text.as_str())?),
            lumen_ui::Action::Select => Action::Select,
            lumen_ui::Action::Focus => Action::Focus,
            lumen_ui::Action::Close => Action::Close,
            lumen_ui::Action::Minimize => Action::Minimize,
            lumen_ui::Action::Restore => Action::Restore,
        };
        Ok(Event::Request { node, action })
    }

    /// Whether an app may take this event itself before its toolkit sees it
    /// (the SDK's `run_with`/`run_until` offer it to the app's `take`):
    /// input, yes; a `Request`, `Close` or `Theme`, never. Those are the
    /// toolkit's, so a request is always answered, a close always ends the
    /// loop and the palette always follows the theme, whatever the app
    /// takes. Exhaustive, so a new variant has to be decided here.
    pub fn offered_to_app(&self) -> bool {
        match self {
            Event::Key { .. }
            | Event::PointerDown { .. }
            | Event::PointerUp { .. }
            | Event::PointerMove { .. }
            | Event::FocusGained
            | Event::FocusLost
            | Event::Lost => true,
            Event::Request { .. } | Event::Close | Event::Theme { .. } => false,
        }
    }

    /// Its encoding, written into `buf` (no allocation): the bytes used.
    /// Every event fits [`EVENT_BYTES`] (tested on the largest), so with a
    /// buffer that big this can't fail.
    pub fn encode<'b>(&self, buf: &'b mut [u8; EVENT_BYTES]) -> Result<&'b [u8], postcard::Error> {
        postcard::to_slice(self, buf).map(|used| &*used)
    }

    /// The event `bytes` hold, exactly: postcard's refusal, or bytes left
    /// over after a whole event.
    pub fn decode(bytes: &[u8]) -> Result<Event, DecodeError> {
        crate::wire::decode_exact(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn enc(e: &Event) -> Vec<u8> {
        let mut buf = [0; EVENT_BYTES];
        let bytes = e.encode(&mut buf).unwrap().to_vec();
        assert_eq!(Event::decode(&bytes), Ok(e.clone()), "{e:?} round trip");
        bytes
    }

    fn palette() -> Palette {
        Palette { background: 1, text: 2, accent: 3, accent_text: 4, border: 5, disabled: 6, field: 0x80 }
    }

    fn sv(text: &str) -> SetValue {
        SetValue::try_from(text).unwrap()
    }

    /// How many variants each data-carrying enum has: written once, for the
    /// pin's range and the past-the-last bytes.
    const EVENT_VARIANTS: u8 = 10;
    const ACTION_VARIANTS: u8 = 7;
    const KEY_ACTION_VARIANTS: u8 = 2;

    /// Each variant's tag, as an exhaustive match (no `_` arm): a variant
    /// added fails to build here until its tag arm is written; its count
    /// above, its case and its bytes are then to be extended with it.
    fn event_tag(e: &Event) -> u8 {
        match e {
            Event::Theme { .. } => 0,
            Event::Key { .. } => 1,
            Event::PointerDown { .. } => 2,
            Event::PointerUp { .. } => 3,
            Event::PointerMove { .. } => 4,
            Event::FocusGained => 5,
            Event::FocusLost => 6,
            Event::Lost => 7,
            Event::Close => 8,
            Event::Request { .. } => 9,
        }
    }

    fn action_tag(a: &Action) -> u8 {
        match a {
            Action::Press => 0,
            Action::SetValue(_) => 1,
            Action::Select => 2,
            Action::Focus => 3,
            Action::Close => 4,
            Action::Minimize => 5,
            Action::Restore => 6,
        }
    }

    fn key_action_tag(a: &KeyAction) -> u8 {
        match a {
            KeyAction::Press { .. } => 0,
            KeyAction::Release => 1,
        }
    }

    #[test]
    fn only_input_is_offered_to_the_app() {
        let p = Palette::default();
        let key = KeyAction::Press { text: None };
        let none = Buttons::from_bits(0);
        let cases = [
            (Event::Theme { theme: ThemeId::Light, palette: p }, false),
            (Event::Key { code: KeyCode::ArrowUp, mods: Mods::default(), action: key }, true),
            (Event::PointerDown { x: 0, y: 0, button: Button::Left, buttons: none }, true),
            (Event::PointerUp { x: 0, y: 0, button: Button::Left, buttons: none }, true),
            (Event::PointerMove { x: 0, y: 0, buttons: none }, true),
            (Event::FocusGained, true),
            (Event::FocusLost, true),
            (Event::Lost, true),
            (Event::Close, false),
            (Event::Request { node: 1, action: Action::Press }, false),
        ];
        // Every variant once (the tags 0 to 9).
        let mut tags: Vec<u8> = cases.iter().map(|(e, _)| event_tag(e)).collect();
        tags.sort_unstable();
        assert_eq!(tags, (0..10).collect::<Vec<u8>>());
        for (e, offered) in cases {
            assert_eq!(e.offered_to_app(), offered, "{e:?}");
        }
    }

    #[test]
    fn every_variant_s_encoding_is_pinned() {
        let p = palette();
        let cases: Vec<(Event, Vec<u8>)> = alloc::vec![
            (Event::Theme { theme: ThemeId::Dark, palette: p }, alloc::vec![0, 1, 1, 2, 3, 4, 5, 6, 0x80, 1]),
            (
                Event::Key {
                    code: KeyCode::A,
                    mods: Mods::from_bits(mods::LSHIFT),
                    action: KeyAction::Press { text: Some('A') },
                },
                alloc::vec![1, 61, 1, 0, 1, 1, b'A'],
            ),
            (
                Event::Key { code: KeyCode::Escape, mods: Mods::default(), action: KeyAction::Press { text: None } },
                alloc::vec![1, 0, 0, 0, 0],
            ),
            (
                Event::Key { code: KeyCode::Unknown, mods: Mods::from_bits(0xff), action: KeyAction::Release },
                alloc::vec![1, 124, 0xff, 1],
            ),
            // Zigzagged: -1 is 1, 2 is 4.
            (
                Event::PointerDown { x: -1, y: 2, button: Button::Right, buttons: Buttons::from_bits(0b10) },
                alloc::vec![2, 1, 4, 1, 2],
            ),
            (
                Event::PointerUp { x: 64, y: 0, button: Button::Middle, buttons: Buttons::from_bits(0b1_0001) },
                alloc::vec![3, 0x80, 1, 0, 2, 0x11],
            ),
            (Event::PointerMove { x: 3, y: -2, buttons: Buttons::from_bits(0b101) }, alloc::vec![4, 6, 3, 5]),
            (Event::FocusGained, alloc::vec![5]),
            (Event::FocusLost, alloc::vec![6]),
            (Event::Lost, alloc::vec![7]),
            (Event::Close, alloc::vec![8]),
            (Event::Request { node: 300, action: Action::Press }, alloc::vec![9, 0xac, 2, 0]),
            (Event::Request { node: 1, action: Action::SetValue(sv("hi")) }, alloc::vec![9, 1, 1, 2, b'h', b'i'],),
            (Event::Request { node: 1, action: Action::Select }, alloc::vec![9, 1, 2]),
            (Event::Request { node: 1, action: Action::Focus }, alloc::vec![9, 1, 3]),
            (Event::Request { node: 1, action: Action::Close }, alloc::vec![9, 1, 4]),
            (Event::Request { node: 1, action: Action::Minimize }, alloc::vec![9, 1, 5]),
            (Event::Request { node: 1, action: Action::Restore }, alloc::vec![9, 1, 6]),
        ];
        for (event, bytes) in &cases {
            assert_eq!(&enc(event), bytes, "{event:?}");
            // The first byte is the tag the exhaustive match pins; the first
            // of a request's action bytes its action's, a key's fourth byte
            // its action's.
            assert_eq!(bytes[0], event_tag(event), "{event:?}");
            match event {
                Event::Request { action, .. } => {
                    assert_eq!(bytes[bytes.len() - action_bytes(action)], action_tag(action))
                }
                Event::Key { action, .. } => assert_eq!(bytes[3], key_action_tag(action)),
                _ => {}
            }
        }
        // Every tag of each enum pinned above.
        let mut tags: Vec<u8> = cases.iter().map(|(e, _)| event_tag(e)).collect();
        tags.dedup();
        assert_eq!(tags, (0..EVENT_VARIANTS).collect::<Vec<u8>>());
        let mut actions: Vec<u8> = cases
            .iter()
            .filter_map(|(e, _)| match e {
                Event::Request { action, .. } => Some(action_tag(action)),
                _ => None,
            })
            .collect();
        actions.dedup();
        assert_eq!(actions, (0..ACTION_VARIANTS).collect::<Vec<u8>>());
        let mut keys: Vec<u8> = cases
            .iter()
            .filter_map(|(e, _)| match e {
                Event::Key { action, .. } => Some(key_action_tag(action)),
                _ => None,
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys, (0..KEY_ACTION_VARIANTS).collect::<Vec<u8>>());
    }

    /// The bytes an action takes at the end of a request's encoding.
    fn action_bytes(a: &Action) -> usize {
        match a {
            Action::SetValue(text) => 2 + text.as_str().len(),
            _ => 1,
        }
    }

    #[test]
    fn wire_tables_are_pinned() {
        assert_eq!(ThemeId::ALL.map(|t| t as u8), [0, 1]);
        assert_eq!(ThemeId::ALL.map(ThemeId::name), ["Light", "Dark"]);
        assert_eq!(KeyCode::ALL.len(), 125);
        for (i, k) in KeyCode::ALL.iter().enumerate() {
            assert_eq!(*k as usize, i);
            // Each one byte on the wire: its position.
            assert_eq!(postcard::to_allocvec(k).unwrap(), [i as u8], "{k:?}");
        }
        let firsts: Vec<&str> = KeyCode::ALL[..4].iter().map(|k| k.name()).collect();
        assert_eq!(firsts, ["Escape", "F1", "F2", "F3"]);
        assert_eq!((KeyCode::F12 as u8, KeyCode::Return as u8, KeyCode::Spacebar as u8), (12, 72, 96));
        assert_eq!(
            [
                mods::LSHIFT,
                mods::RSHIFT,
                mods::LCTRL,
                mods::RCTRL,
                mods::LALT,
                mods::RALT,
                mods::CAPSLOCK,
                mods::NUMLOCK
            ],
            [1, 2, 4, 8, 16, 32, 64, 128]
        );
        assert_eq!(Button::ALL.map(|b| b as u8), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn key_codes_convert_both_ways_for_every_variant() {
        for k in KeyCode::ALL {
            let pc: lumen_input::KeyCode = k.into();
            assert_eq!(KeyCode::from(pc), k);
            // Named alike, numbered as pc-keyboard declares them.
            assert_eq!(alloc::format!("{pc:?}"), k.name());
            assert_eq!(pc as usize, k as usize, "{pc:?}");
        }
    }

    #[test]
    fn actions_and_buttons_convert_back_for_an_app_s_toolkit() {
        // Every wire action to lumen-ui's and back (`Event::request`).
        let actions = [
            lumen_ui::Action::Press,
            lumen_ui::Action::SetValue(String::from("12")),
            lumen_ui::Action::Select,
            lumen_ui::Action::Focus,
            lumen_ui::Action::Close,
            lumen_ui::Action::Minimize,
            lumen_ui::Action::Restore,
        ];
        assert_eq!(actions.len(), lumen_ui::ActionKind::ALL.len());
        for a in actions {
            let Ok(Event::Request { node: 3, action }) = Event::request(3, &a) else { panic!("{a:?}") };
            assert_eq!(lumen_ui::Action::from(action), a);
        }
        // Every button both ways.
        for b in Button::ALL {
            assert_eq!(Button::from(lumen_input::Button::from(b)), b);
        }
        assert_eq!(lumen_input::Button::from(Button::Middle), lumen_input::Button::Middle);
        assert_eq!(
            KeyAction::from(lumen_input::KeyAction::Press { text: Some('x') }),
            KeyAction::Press { text: Some('x') }
        );
        assert_eq!(KeyAction::from(lumen_input::KeyAction::Release), KeyAction::Release);
    }

    #[test]
    fn keys_reach_the_toolkit_as_the_kernel_s_windows_take_them() {
        use lumen_ui::Key;
        let press = |text| KeyAction::Press { text };
        let none = Mods::default();
        let shift = Mods::from_bits(mods::RSHIFT);
        assert_eq!(toolkit_key(KeyCode::Tab, none, press(Some('\t'))), Some(Key::Tab));
        assert_eq!(toolkit_key(KeyCode::Tab, shift, press(None)), Some(Key::BackTab));
        assert_eq!(toolkit_key(KeyCode::Backspace, none, press(Some('\u{8}'))), Some(Key::Backspace));
        assert_eq!(toolkit_key(KeyCode::Return, none, press(Some('\n'))), Some(Key::Enter));
        assert_eq!(toolkit_key(KeyCode::NumpadEnter, none, press(Some('\n'))), Some(Key::Enter));
        assert_eq!(toolkit_key(KeyCode::Key7, none, press(Some('7'))), Some(Key::Char('7')));
        assert_eq!(toolkit_key(KeyCode::OemPlus, shift, press(Some('+'))), Some(Key::Char('+')));
        // A control character, a key that typed nothing, a release: none.
        assert_eq!(toolkit_key(KeyCode::C, Mods::from_bits(mods::LCTRL), press(Some('\u{3}'))), None);
        assert_eq!(toolkit_key(KeyCode::ArrowUp, none, press(None)), None);
        assert_eq!(toolkit_key(KeyCode::Key7, none, KeyAction::Release), None);
    }

    #[test]
    fn kernel_types_convert() {
        use lumen_input::KeyEvent;
        let k = KeyEvent {
            code: lumen_input::KeyCode::Return,
            action: lumen_input::KeyAction::Press { text: Some('\n') },
            mods: lumen_input::Mods::NONE.with_shift().with_ctrl(),
            after_drop: false,
        };
        assert_eq!(
            Event::key(&k),
            Event::Key {
                code: KeyCode::Return,
                mods: Mods::from_bits(mods::LSHIFT | mods::LCTRL),
                action: KeyAction::Press { text: Some('\n') }
            }
        );
        let up = KeyEvent { action: lumen_input::KeyAction::Release, mods: lumen_input::Mods::NONE, ..k };
        assert_eq!(
            Event::key(&up),
            Event::Key { code: KeyCode::Return, mods: Mods::default(), action: KeyAction::Release }
        );
        // Each of pc-keyboard's eight modifier fields alone: its own bit.
        type Set = fn(&mut pc_keyboard::Modifiers);
        let fields: [(Set, u8); 8] = [
            (|m| m.lshift = true, mods::LSHIFT),
            (|m| m.rshift = true, mods::RSHIFT),
            (|m| m.lctrl = true, mods::LCTRL),
            (|m| m.rctrl = true, mods::RCTRL),
            (|m| m.lalt = true, mods::LALT),
            (|m| m.ralt = true, mods::RALT),
            (|m| m.capslock = true, mods::CAPSLOCK),
            (|m| m.numlock = true, mods::NUMLOCK),
        ];
        let mut all = pc_modifiers();
        for (set, bit) in fields {
            let mut m = pc_modifiers();
            set(&mut m);
            set(&mut all);
            assert_eq!(Mods::from(lumen_input::Mods::from(&m)).bits(), bit);
        }
        assert_eq!(Mods::from(lumen_input::Mods::from(&all)).bits(), 0xff);
        assert_eq!(Mods::from(lumen_input::Mods::from(&pc_modifiers())).bits(), 0);
        let m = Mods::from_bits(mods::RSHIFT | mods::LALT | mods::NUMLOCK);
        assert_eq!((m.shift(), m.ctrl(), m.alt(), m.capslock(), m.numlock()), (true, false, true, false, true));
        // Buttons.
        for b in lumen_input::Button::ALL {
            assert_eq!(Button::from(b) as usize, b.index());
            let set = Buttons::from(lumen_input::Buttons::from(b));
            assert_eq!(set.bits(), 1 << b.index());
            assert!(set.has(b.into()));
        }
        let two = Buttons::from(lumen_input::Buttons::LEFT.with(lumen_input::Buttons::EXTRA));
        assert_eq!(two.bits(), 0b1_0001);
        assert!(two.has(Button::Left) && two.has(Button::Extra) && !two.has(Button::Right));
        assert_eq!(Buttons::from(lumen_input::Buttons::NONE).bits(), 0);
        // The theme and its palette.
        let ui = lumen_ui::Palette {
            background: 10,
            text: 11,
            accent: 12,
            accent_text: 13,
            border: 14,
            disabled: 15,
            field: 16,
        };
        let e = Event::theme(lumen_wm::ThemeId::Light, &ui);
        let Event::Theme { theme, palette } = e else { panic!("{e:?}") };
        assert_eq!(theme, ThemeId::Light);
        // Field by field, so two swapped fields can't pass.
        assert_eq!(
            palette,
            Palette { background: 10, text: 11, accent: 12, accent_text: 13, border: 14, disabled: 15, field: 16 }
        );
        assert_eq!(lumen_ui::Palette::from(palette), ui);
        for id in lumen_wm::ThemeId::ALL {
            assert_eq!(lumen_wm::ThemeId::from(ThemeId::from(id)), id);
            assert_eq!(ThemeId::from(id) as u8, id.index());
        }
    }

    fn pc_modifiers() -> pc_keyboard::Modifiers {
        pc_keyboard::Modifiers::default()
    }

    #[test]
    fn a_set_value_past_its_cap_is_refused_before_encoding() {
        let at = lumen_ui::Action::SetValue("v".repeat(SET_VALUE_BYTES));
        assert!(Event::request(7, &at).is_ok());
        let past = lumen_ui::Action::SetValue("v".repeat(SET_VALUE_BYTES + 1));
        assert_eq!(Event::request(7, &past), Err(TooLong));
        // Bytes, not characters: 256 two-byte characters are at the cap.
        assert!(Event::request(7, &lumen_ui::Action::SetValue("é".repeat(SET_VALUE_BYTES / 2))).is_ok());
        assert_eq!(Event::request(7, &lumen_ui::Action::SetValue("é".repeat(SET_VALUE_BYTES / 2 + 1))), Err(TooLong));
        for (ui, wire) in [
            (lumen_ui::Action::Press, Action::Press),
            (lumen_ui::Action::Select, Action::Select),
            (lumen_ui::Action::Focus, Action::Focus),
            (lumen_ui::Action::Close, Action::Close),
            (lumen_ui::Action::Minimize, Action::Minimize),
            (lumen_ui::Action::Restore, Action::Restore),
        ] {
            assert_eq!(Event::request(3, &ui), Ok(Event::Request { node: 3, action: wire }));
        }
        // The text type itself: built at most at the cap, and a longer one
        // refused when decoded too (an app can't be handed one).
        assert_eq!(SetValue::try_from("v".repeat(SET_VALUE_BYTES + 1).as_str()), Err(TooLong));
        assert_eq!(sv("abc").as_str(), "abc");
        let mut bytes = alloc::vec![9, 1, 1, 0x81, 0x04];
        bytes.extend(core::iter::repeat_n(b'v', SET_VALUE_BYTES + 1));
        assert_eq!(Event::decode(&bytes), Err(DecodeError::Postcard(postcard::Error::SerdeDeCustom)));
        let mut at = alloc::vec![9, 1, 1, 0x80, 0x04];
        at.extend(core::iter::repeat_n(b'v', SET_VALUE_BYTES));
        assert!(Event::decode(&at).is_ok());
    }

    /// The largest event: a request with the biggest node id and a
    /// `set_value` text at its cap of 4-byte characters.
    fn largest() -> Event {
        let text: String = core::iter::repeat_n('\u{10ffff}', SET_VALUE_BYTES / 4).collect();
        assert_eq!(text.len(), SET_VALUE_BYTES);
        Event::Request { node: u64::MAX, action: Action::SetValue(sv(&text)) }
    }

    #[test]
    fn the_largest_event_fits() {
        let bytes = enc(&largest());
        assert!(bytes.len() <= EVENT_BYTES, "{}", bytes.len());
        // Tag, a 10-byte u64, the action's tag, a 2-byte length, the text.
        assert_eq!(bytes.len(), 1 + 10 + 1 + 2 + SET_VALUE_BYTES);
        // Every other variant at its biggest varints is far smaller.
        let p = Palette {
            background: u32::MAX,
            text: u32::MAX,
            accent: u32::MAX,
            accent_text: u32::MAX,
            border: u32::MAX,
            disabled: u32::MAX,
            field: u32::MAX,
        };
        for e in [
            Event::Theme { theme: ThemeId::Dark, palette: p },
            Event::Key {
                code: KeyCode::Unknown,
                mods: Mods::from_bits(0xff),
                action: KeyAction::Press { text: Some('\u{10ffff}') },
            },
            Event::PointerDown { x: i32::MIN, y: i32::MIN, button: Button::Extra, buttons: Buttons::from_bits(0xff) },
            Event::PointerMove { x: i32::MIN, y: i32::MAX, buttons: Buttons::from_bits(0xff) },
        ] {
            assert!(enc(&e).len() < 64, "{e:?}");
        }
    }

    fn samples() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for e in [
            largest(),
            Event::Theme { theme: ThemeId::Light, palette: palette() },
            Event::Key { code: KeyCode::Q, mods: Mods::from_bits(3), action: KeyAction::Press { text: Some('é') } },
            Event::PointerUp { x: -300, y: 70000, button: Button::Right, buttons: Buttons::default() },
            Event::Request { node: 9, action: Action::SetValue(sv("12+3")) },
            Event::Close,
        ] {
            out.push(enc(&e));
        }
        out
    }

    #[test]
    fn decoding_never_panics_on_truncations_flips_or_trailing_bytes() {
        for bytes in samples() {
            // Every cut short is an error.
            for n in 0..bytes.len() {
                assert!(Event::decode(&bytes[..n]).is_err(), "{n} of {bytes:?}");
            }
            // Trailing bytes: refused, counted.
            for extra in [&[0u8][..], &[1, 2, 3], &[0xff; 9]] {
                let mut longer = bytes.clone();
                longer.extend_from_slice(extra);
                assert_eq!(Event::decode(&longer), Err(DecodeError::LeftOver(extra.len())));
            }
            // Every byte flipped every way: decodes or errs, never panics.
            for i in 0..bytes.len() {
                for v in 0..=255u8 {
                    let mut b = bytes.clone();
                    b[i] = v;
                    let _ = Event::decode(&b);
                }
            }
        }
        // Random bytes too (a fixed xorshift, so a failure repeats).
        let mut s = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let len = (s % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (s >> (i % 8 * 8)) as u8 ^ i as u8).collect();
            let _ = Event::decode(&bytes);
        }
    }

    #[test]
    fn malformed_values_are_refused_with_postcard_s_errors() {
        use postcard::Error as P;
        let bad = |bytes: &[u8]| Event::decode(bytes);
        // A set_value text that isn't UTF-8.
        assert_eq!(bad(&[9, 1, 1, 2, 0xff, b'a']), Err(DecodeError::Postcard(P::DeserializeBadUtf8)));
        // A key's text: a bad Option tag, and a surrogate char.
        assert_eq!(bad(&[1, 61, 0, 0, 2]), Err(DecodeError::Postcard(P::DeserializeBadOption)));
        assert_eq!(bad(&[1, 61, 0, 0, 1, 3, 0xed, 0xa0, 0x80]), Err(DecodeError::Postcard(P::DeserializeBadChar)));
        // An i32 varint running past 5 bytes, and one past u32's range.
        assert_eq!(
            bad(&[4, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0, 0]),
            Err(DecodeError::Postcard(P::DeserializeBadVarint))
        );
        assert_eq!(bad(&[4, 0xff, 0xff, 0xff, 0xff, 0x7f, 0, 0]), Err(DecodeError::Postcard(P::DeserializeBadVarint)));
        // A variant past the last, a key code past the table, a theme past
        // the two: serde's refusal, postcard's custom error.
        assert_eq!(bad(&[EVENT_VARIANTS]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        // A request's action past the last, and a key's action past the
        // last (nested enums).
        assert_eq!(bad(&[9, 1, ACTION_VARIANTS]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        assert_eq!(bad(&[1, 61, 0, KEY_ACTION_VARIANTS]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        assert_eq!(bad(&[1, 125, 0, 1]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        assert_eq!(bad(&[0, 2, 0, 0, 0, 0, 0, 0, 0]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
    }
}
