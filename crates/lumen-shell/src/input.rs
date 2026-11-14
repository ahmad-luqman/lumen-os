//! Keys from the bytes a serial terminal sends.
//!
//! - CR, LF, or CR LF together: [`Key::Enter`];
//! - DEL (0x7f) and BS (0x08): [`Key::Backspace`]; HT: [`Key::Tab`];
//! - other control characters 0x01 to 0x1a: [`Key::Ctrl`] with their letter;
//! - `ESC [ A` to `D` and `ESC O A` to `D`: the arrows; `ESC [ H`/`F`,
//!   `ESC O H`/`F`, `ESC [ 1 ~`/`7 ~` and `ESC [ 4 ~`/`8 ~`: Home and End;
//!   `ESC [ 3 ~`: Delete (modifiers, as in `ESC [ 1 ; 5 C`, are ignored);
//!   other escape sequences are dropped whole;
//! - UTF-8: [`Key::Char`] (a malformed sequence is dropped).

use crate::Key;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Escape,
    /// In `ESC [`, with its first parameter so far.
    Csi {
        param: u16,
        first: bool,
    },
    /// After `ESC O`.
    Ss3,
    /// In a UTF-8 sequence: the code point so far and the bytes still to
    /// come.
    Utf8 {
        code: u32,
        left: u8,
    },
}

/// Turns a serial terminal's bytes into keys, one byte at a time.
#[derive(Clone, Debug)]
pub struct Decoder {
    state: State,
    /// The last byte was a CR (a LF right after it is the same Enter).
    after_cr: bool,
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new()
    }
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder { state: State::Ground, after_cr: false }
    }

    /// Take the next byte; the key it completes, if any.
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        let after_cr = core::mem::replace(&mut self.after_cr, byte == b'\r');
        match self.state {
            State::Ground => self.ground(byte, after_cr),
            State::Escape => {
                self.state = match byte {
                    b'[' => State::Csi { param: 0, first: true },
                    b'O' => State::Ss3,
                    _ => State::Ground,
                };
                None
            }
            State::Csi { param, first } => match byte {
                b'0'..=b'9' => {
                    let param =
                        if first { param.saturating_mul(10).saturating_add(u16::from(byte - b'0')) } else { param };
                    self.state = State::Csi { param, first };
                    None
                }
                b';' | b':' => {
                    self.state = State::Csi { param, first: false };
                    None
                }
                // Intermediate bytes, and parameters this doesn't use.
                0x20..=0x3f => None,
                _ => {
                    self.state = State::Ground;
                    match byte {
                        b'~' => match param {
                            1 | 7 => Some(Key::Home),
                            3 => Some(Key::Delete),
                            4 | 8 => Some(Key::End),
                            _ => None,
                        },
                        _ => arrow(byte),
                    }
                }
            },
            State::Ss3 => {
                self.state = State::Ground;
                arrow(byte)
            }
            State::Utf8 { code, left } => {
                if byte & 0xc0 != 0x80 {
                    // Malformed: drop what came so far, start again here.
                    self.state = State::Ground;
                    return self.ground(byte, after_cr);
                }
                let code = code << 6 | u32::from(byte & 0x3f);
                if left > 1 {
                    self.state = State::Utf8 { code, left: left - 1 };
                    return None;
                }
                self.state = State::Ground;
                char::from_u32(code).filter(|c| !c.is_control()).map(Key::Char)
            }
        }
    }

    fn ground(&mut self, byte: u8, after_cr: bool) -> Option<Key> {
        match byte {
            b'\r' => Some(Key::Enter),
            b'\n' => (!after_cr).then_some(Key::Enter),
            0x7f | 0x08 => Some(Key::Backspace),
            b'\t' => Some(Key::Tab),
            0x1b => {
                self.state = State::Escape;
                None
            }
            0x01..=0x1a => Some(Key::Ctrl(char::from(b'a' + byte - 1))),
            0x20..=0x7e => Some(Key::Char(char::from(byte))),
            // Lead bytes of 2, 3 and 4 byte sequences (no overlong 0xc0
            // and 0xc1, nothing past U+10FFFF).
            0xc2..=0xdf => self.lead(byte & 0x1f, 1),
            0xe0..=0xef => self.lead(byte & 0x0f, 2),
            0xf0..=0xf4 => self.lead(byte & 0x07, 3),
            _ => None,
        }
    }

    fn lead(&mut self, bits: u8, left: u8) -> Option<Key> {
        self.state = State::Utf8 { code: u32::from(bits), left };
        None
    }
}

/// The key an escape sequence's final byte names (cursor keys, Home, End).
fn arrow(byte: u8) -> Option<Key> {
    Some(match byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    })
}
