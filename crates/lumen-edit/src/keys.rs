//! The keys the editor takes: a key press (its wire code, the mods and the
//! text the keyboard's layout made of it) as a [`Command`].
//!
//! Letters are matched by what the layout made, never by where the key
//! is: [`KeyCode`] names a key's place on a US keyboard, so on a German one
//! the key labelled Z is `KeyCode::Y`, and on French, Dvorak or Colemak
//! Ctrl+Z is yet another place. The kernel decodes with `pc-keyboard`'s
//! `MapLettersToUnicode`, so a Ctrl+letter's text is the letter's control
//! code for the layout in use: Ctrl+Z is U+001A, Ctrl+Y U+0019. The keys
//! that aren't letters (Enter, Tab, the arrows, Home...) are where they are
//! on every layout, so they are matched by code.

use lumen_app_api::event::{KeyCode, Mods, mods};

use crate::Command;

/// Ctrl+Z's text, whatever the layout: undo (with Shift: redo).
const CTRL_Z: char = '\u{1a}';
/// Ctrl+Y's text: redo.
const CTRL_Y: char = '\u{19}';

/// The command a key press stands for, if any: Ctrl+Z undoes, Ctrl+Shift+Z
/// and Ctrl+Y redo (by the layout's letter); Enter (either), Tab,
/// Backspace, Delete, the arrows, Home and End (with Ctrl: the text's start
/// and end), Page Up and Page Down by their keys; any other press making a
/// character that isn't a control character types it, unless Ctrl is held.
pub fn command(code: KeyCode, m: Mods, text: Option<char>) -> Option<Command> {
    let ctrl = m.any(mods::CTRL);
    match text {
        Some(CTRL_Z) => return Some(if m.any(mods::SHIFT) { Command::Redo } else { Command::Undo }),
        Some(CTRL_Y) => return Some(Command::Redo),
        _ => {}
    }
    Some(match code {
        KeyCode::Home if ctrl => Command::Top,
        KeyCode::End if ctrl => Command::Bottom,
        KeyCode::Return | KeyCode::NumpadEnter => Command::Newline,
        KeyCode::Tab => Command::Tab,
        KeyCode::Backspace => Command::Backspace,
        KeyCode::Delete => Command::Delete,
        KeyCode::ArrowLeft => Command::Left,
        KeyCode::ArrowRight => Command::Right,
        KeyCode::ArrowUp => Command::Up,
        KeyCode::ArrowDown => Command::Down,
        KeyCode::Home => Command::Home,
        KeyCode::End => Command::End,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::PageDown => Command::PageDown,
        _ if ctrl => return None,
        _ => return text.filter(|c| !c.is_control()).map(Command::Insert),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_input::{KeyAction, KeyDecoder};
    use pc_keyboard::layouts::{AnyLayout, Azerty, Colemak, De105Key, Dvorak104Key, Us104Key};
    use pc_keyboard::{HandleControl, KeyboardLayout};

    const NONE: Mods = Mods::from_bits(0);
    const CTRL: Mods = Mods::from_bits(mods::LCTRL);
    const SHIFT: Mods = Mods::from_bits(mods::LSHIFT);

    #[test]
    fn every_key_the_editor_maps() {
        let by_code = [
            (KeyCode::Return, Command::Newline),
            (KeyCode::NumpadEnter, Command::Newline),
            (KeyCode::Tab, Command::Tab),
            (KeyCode::Backspace, Command::Backspace),
            (KeyCode::Delete, Command::Delete),
            (KeyCode::ArrowLeft, Command::Left),
            (KeyCode::ArrowRight, Command::Right),
            (KeyCode::ArrowUp, Command::Up),
            (KeyCode::ArrowDown, Command::Down),
            (KeyCode::Home, Command::Home),
            (KeyCode::End, Command::End),
            (KeyCode::PageUp, Command::PageUp),
            (KeyCode::PageDown, Command::PageDown),
        ];
        for (code, cmd) in by_code {
            // Their text (Enter's '\n', Tab's '\t', Backspace's '\u{8}', or
            // none) doesn't matter, nor Shift.
            for text in [None, Some('\n'), Some('\t'), Some('\u{8}')] {
                assert_eq!(command(code, NONE, text), Some(cmd), "{code:?} {text:?}");
                assert_eq!(command(code, SHIFT, text), Some(cmd), "{code:?} {text:?}");
            }
        }
        assert_eq!(command(KeyCode::Home, CTRL, None), Some(Command::Top));
        assert_eq!(command(KeyCode::End, CTRL, None), Some(Command::Bottom));
        // Either Ctrl.
        assert_eq!(command(KeyCode::End, Mods::from_bits(mods::RCTRL), None), Some(Command::Bottom));
        assert_eq!(command(KeyCode::Z, CTRL, Some(CTRL_Z)), Some(Command::Undo));
        assert_eq!(command(KeyCode::Z, Mods::from_bits(mods::LCTRL | mods::RSHIFT), Some(CTRL_Z)), Some(Command::Redo));
        assert_eq!(command(KeyCode::Y, CTRL, Some(CTRL_Y)), Some(Command::Redo));
        // Typing: a character, Shift's included; not a control character,
        // nor anything with Ctrl held, nor a key making no text.
        assert_eq!(command(KeyCode::A, NONE, Some('a')), Some(Command::Insert('a')));
        assert_eq!(command(KeyCode::A, SHIFT, Some('A')), Some(Command::Insert('A')));
        assert_eq!(command(KeyCode::Oem7, NONE, Some('é')), Some(Command::Insert('é')));
        assert_eq!(command(KeyCode::Escape, NONE, Some('\u{1b}')), None);
        assert_eq!(command(KeyCode::C, CTRL, Some('\u{3}')), None);
        assert_eq!(command(KeyCode::A, CTRL, Some('a')), None);
        assert_eq!(command(KeyCode::F1, NONE, None), None);
        // By the layout's letter, not the key's place: Z's place with no
        // Ctrl text is just typing.
        assert_eq!(command(KeyCode::Z, CTRL, None), None);
        assert_eq!(command(KeyCode::Z, NONE, Some('z')), Some(Command::Insert('z')));
    }

    /// The command for `key` (set 1 make code) pressed with `held` (make
    /// codes) down first, decoded on `layout` as the kernel decodes.
    fn decoded<L: KeyboardLayout>(layout: L, held: &[u8], key: u8) -> Option<Command> {
        let mut d = KeyDecoder::new(layout, HandleControl::MapLettersToUnicode);
        let mut got = None;
        for &b in held.iter().chain([key, key | 0x80].iter()) {
            d.add_byte(b, |e| {
                if e.code != pc_keyboard::KeyCode::LControl
                    && e.code != pc_keyboard::KeyCode::LShift
                    && let KeyAction::Press { text } = e.action
                {
                    got = Some(command(e.code.into(), e.mods.into(), text));
                }
            })
            .expect("a byte the decoder takes");
        }
        got.expect("a press")
    }

    /// Set 1 make codes: left Ctrl, left Shift, and the keys at Q, W, Y, Z,
    /// F, T, O and the slash's places on a US keyboard.
    const LCTRL: u8 = 0x1d;
    const LSHIFT: u8 = 0x2a;
    const Q: u8 = 0x10;
    const W: u8 = 0x11;
    const Y: u8 = 0x15;
    const Z: u8 = 0x2c;
    const F: u8 = 0x21;
    const T: u8 = 0x14;
    const O: u8 = 0x18;
    const SLASH: u8 = 0x35;

    #[test]
    fn undo_and_redo_follow_the_layout_s_letters() {
        // US: Z and Y where they are.
        assert_eq!(decoded(Us104Key, &[LCTRL], Z), Some(Command::Undo));
        assert_eq!(decoded(Us104Key, &[LCTRL, LSHIFT], Z), Some(Command::Redo));
        assert_eq!(decoded(Us104Key, &[LCTRL], Y), Some(Command::Redo));
        // German: Z and Y swap places.
        assert_eq!(decoded(De105Key, &[LCTRL], Y), Some(Command::Undo), "de: Z at Y's place");
        assert_eq!(decoded(De105Key, &[LCTRL], Z), Some(Command::Redo), "de: Y at Z's place");
        // French (AZERTY): Z at W's place; Q's place is A (nothing).
        assert_eq!(decoded(Azerty, &[LCTRL], W), Some(Command::Undo), "fr: Z at W's place");
        assert_eq!(decoded(Azerty, &[LCTRL], Q), None, "fr: Ctrl+A");
        assert_eq!(decoded(Azerty, &[LCTRL], Y), Some(Command::Redo), "fr: Y where US has it");
        // Dvorak: Z at the slash's place, Y at T's; Z's place is a
        // semicolon (nothing).
        assert_eq!(decoded(Dvorak104Key, &[LCTRL], SLASH), Some(Command::Undo), "dvorak: Z");
        assert_eq!(decoded(Dvorak104Key, &[LCTRL], T), Some(Command::Redo), "dvorak: Y");
        assert_eq!(decoded(Dvorak104Key, &[LCTRL], Z), None, "dvorak: Ctrl+;");
        // Colemak: Z where US has it, Y at O's place; Y's place is J
        // (nothing).
        assert_eq!(decoded(Colemak, &[LCTRL], Z), Some(Command::Undo), "colemak: Z");
        assert_eq!(decoded(Colemak, &[LCTRL], O), Some(Command::Redo), "colemak: Y");
        assert_eq!(decoded(Colemak, &[LCTRL], Y), None, "colemak: Ctrl+J");
        // Typing still types the layout's letter.
        assert_eq!(decoded(De105Key, &[], Y), Some(Command::Insert('z')));
        assert_eq!(decoded(AnyLayout::Azerty(Azerty), &[], Q), Some(Command::Insert('a')));
        assert_eq!(decoded(Us104Key, &[LSHIFT], F), Some(Command::Insert('F')));
    }
}
