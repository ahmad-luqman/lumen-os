//! Key presses in a stream of PS/2 scancode set 1 bytes (#149): what the
//! kernel's `kbd` counts as `presses`, so a test can confirm that each key
//! it typed reached the guest.
//!
//! The 8042 translates the keyboard's set 2 into set 1
//! (`kernel/src/keyboard.rs`), so the bytes read from it are set 1:
//!
//! - a key's make code is one byte below `0x80`, its break the same byte
//!   with bit 7 set;
//! - an extended key's codes follow an `0xE0` prefix;
//! - `0xE0 0x2A` and `0xE0 0x36` (and their breaks) are *fake shifts*,
//!   which a keyboard sends around some extended keys (QEMU around Print
//!   Screen): not keys of their own;
//! - Pause sends `0xE1 0x1D 0x45 0xE1 0x9D 0xC5` when pressed and nothing
//!   when released: one press.
//!
//! So each key pressed counts once, whatever its prefix, and a key held
//! down long enough for the keyboard to repeat it counts once per repeat
//! (QEMU's keyboard doesn't repeat).

/// Counts the presses in set 1 bytes fed one at a time: the prefix seen
/// so far is carried from byte to byte (and so from one interrupt to the
/// next).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Presses {
    state: State,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Start,
    /// After `0xE0`: an extended key's code comes next.
    Extended,
    /// After `0xE1`: `.0` more bytes of Pause's half (its first byte's make
    /// counts).
    Pause(u8),
}

/// The fake shifts' codes after `0xE0` (bit 7 clear).
const FAKE_SHIFTS: [u8; 2] = [0x2A, 0x36];

impl Presses {
    pub const fn new() -> Self {
        Self { state: State::Start }
    }

    /// Feed one byte; true if it completes a key press.
    pub fn feed(&mut self, byte: u8) -> bool {
        let make = byte & 0x80 == 0;
        let (next, pressed) = match self.state {
            State::Start => match byte {
                0xE0 => (State::Extended, false),
                0xE1 => (State::Pause(2), false),
                _ => (State::Start, make),
            },
            State::Extended => (State::Start, make && !FAKE_SHIFTS.contains(&(byte & 0x7F))),
            State::Pause(2) => (State::Pause(1), make),
            State::Pause(_) => (State::Start, false),
        };
        self.state = next;
        pressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many presses `bytes` hold, fed to a fresh counter.
    fn presses(bytes: &[u8]) -> usize {
        let mut p = Presses::new();
        bytes.iter().filter(|&&b| p.feed(b)).count()
    }

    #[test]
    fn a_key_counts_once_when_pressed_not_released() {
        // `a` pressed and released; shift+a (shift down, a down, a up,
        // shift up): two keys pressed.
        assert_eq!(presses(&[0x1E]), 1);
        assert_eq!(presses(&[0x1E, 0x9E]), 1);
        assert_eq!(presses(&[0x9E]), 0);
        assert_eq!(presses(&[0x2A, 0x1E, 0x9E, 0xAA]), 2);
    }

    #[test]
    fn an_extended_key_counts_once() {
        // Up arrow pressed and released; right Ctrl.
        assert_eq!(presses(&[0xE0, 0x48, 0xE0, 0xC8]), 1);
        assert_eq!(presses(&[0xE0, 0x1D, 0xE0, 0x9D]), 1);
        // The prefix carries across feeds: split anywhere, still one.
        let mut p = Presses::new();
        assert!(!p.feed(0xE0));
        assert!(p.feed(0x48));
        assert!(!p.feed(0xE0));
        assert!(!p.feed(0xC8));
    }

    #[test]
    fn fake_shifts_arent_keys() {
        // QEMU's Print Screen (set 2 `E0 12 E0 7C`, translated), and its
        // release (`E0 B7 E0 AA`); and fake right shifts.
        assert_eq!(presses(&[0xE0, 0x2A, 0xE0, 0x37]), 1);
        assert_eq!(presses(&[0xE0, 0xB7, 0xE0, 0xAA]), 0);
        assert_eq!(presses(&[0xE0, 0x36, 0xE0, 0xB6]), 0);
        // But the real shifts, unprefixed, are keys.
        assert_eq!(presses(&[0x2A, 0x36]), 2);
    }

    #[test]
    fn pause_counts_once() {
        assert_eq!(presses(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]), 1);
        // And the next key after it counts again.
        assert_eq!(presses(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E, 0x9E]), 2);
    }

    #[test]
    fn a_mixed_stream_counts_every_key() {
        // e c h o, Up, Pause, Print Screen, shift+1: 4 + 1 + 1 + 1 + 2.
        let mut bytes = vec![0x12, 0x92, 0x2E, 0xAE, 0x23, 0xA3, 0x18, 0x98];
        bytes.extend([0xE0, 0x48, 0xE0, 0xC8]);
        bytes.extend([0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
        bytes.extend([0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA]);
        bytes.extend([0x2A, 0x02, 0x82, 0xAA]);
        assert_eq!(presses(&bytes), 9);
    }
}
