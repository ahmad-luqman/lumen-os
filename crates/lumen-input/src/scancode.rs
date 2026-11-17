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
//!
//! It also keeps which keys are held now (pressed, not yet released;
//! Pause, never released, isn't held), so a test can tell a key whose
//! release never came from one whose release did ([`Presses::held`]).
//!
//! The counting is syntactic: every make counts, including bytes the
//! kernel's decoder (`pc-keyboard`) then rejects (it counts those as
//! `undecoded`; Ctrl+Pause's `E0 46` is one), and Pause completes on its
//! first byte after `E1`, not where the decoder completes it. So `presses`
//! matches the presses decoded only while `undecoded` stays flat.

/// Counts the presses in set 1 bytes fed one at a time. The prefix seen so
/// far is carried from byte to byte (and so from one interrupt to the
/// next), and the count and the keys held kept with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Presses {
    state: State,
    count: u64,
    /// Bit `code` (0 to 127 plain, 128 to 255 after `0xE0`) set while that
    /// key is held.
    held: [u64; 4],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Start,
    /// After `0xE0`: an extended key's code comes next.
    Extended,
    /// After `0xE1`: the first byte of Pause's half (a make counts).
    PauseFirst,
    /// The second byte of Pause's half (never counts).
    PauseSecond,
}

/// The fake shifts' codes after `0xE0` (bit 7 clear).
const FAKE_SHIFTS: [u8; 2] = [0x2A, 0x36];

impl Presses {
    pub const fn new() -> Self {
        Self { state: State::Start, count: 0, held: [0; 4] }
    }

    /// The keys pressed in the bytes fed so far.
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The keys held now: pressed and not yet released (a repeat holds
    /// nothing more; a release of a key not held changes nothing).
    pub const fn held(&self) -> u32 {
        self.held[0].count_ones() + self.held[1].count_ones() + self.held[2].count_ones() + self.held[3].count_ones()
    }

    /// Feed one byte; true if it completes a key press (and counted it).
    pub fn feed(&mut self, byte: u8) -> bool {
        let make = byte & 0x80 == 0;
        let code = byte & 0x7F;
        // The key this byte presses or releases, if it is one that can be
        // held (its slot in `held`).
        let (next, key) = match self.state {
            State::Start => match byte {
                0xE0 => (State::Extended, None),
                0xE1 => (State::PauseFirst, None),
                _ => (State::Start, Some(usize::from(code))),
            },
            State::Extended if FAKE_SHIFTS.contains(&code) => (State::Start, None),
            State::Extended => (State::Start, Some(128 + usize::from(code))),
            State::PauseFirst => {
                // Pause: pressed, never released, so never held.
                self.state = State::PauseSecond;
                self.count += u64::from(make);
                return make;
            }
            State::PauseSecond => (State::Start, None),
        };
        self.state = next;
        let Some(slot) = key else { return false };
        let bit = 1u64 << (slot % 64);
        if make {
            self.held[slot / 64] |= bit;
            self.count += 1;
        } else {
            self.held[slot / 64] &= !bit;
        }
        make
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many presses `bytes` hold, fed to a fresh counter (checked
    /// against its `count`).
    fn presses(bytes: &[u8]) -> u64 {
        let mut p = Presses::new();
        let n = bytes.iter().filter(|&&b| p.feed(b)).count() as u64;
        assert_eq!(p.count(), n);
        n
    }

    /// What `feed` returns for each of `bytes`, from a fresh counter.
    fn fed(bytes: &[u8]) -> Vec<bool> {
        let mut p = Presses::new();
        bytes.iter().map(|&b| p.feed(b)).collect()
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
        assert_eq!(fed(&[0xE0, 0x48, 0xE0, 0xC8]), [false, true, false, false]);
    }

    #[test]
    fn a_repeated_key_counts_each_repeat() {
        // Typematic repeat: makes without breaks between them, plain and
        // extended.
        assert_eq!(presses(&[0x1E, 0x1E, 0x1E, 0x9E]), 3);
        assert_eq!(presses(&[0xE0, 0x48, 0xE0, 0x48, 0xE0, 0xC8]), 2);
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
    fn print_screen_with_modifiers_counts_once() {
        // With shift or Ctrl held a keyboard sends Print Screen without
        // the fake shift; with Alt, as SysRq (`54`).
        assert_eq!(fed(&[0xE0, 0x37, 0xE0, 0xB7]), [false, true, false, false]);
        assert_eq!(fed(&[0x54, 0xD4]), [true, false]);
    }

    #[test]
    fn pause_counts_once() {
        assert_eq!(fed(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]), [false, true, false, false, false, false]);
        // And the next key after it counts again.
        assert_eq!(presses(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E, 0x9E]), 2);
        // Ctrl+Pause is Break: `E0 46`, an extended key.
        assert_eq!(fed(&[0xE0, 0x46, 0xE0, 0xC6]), [false, true, false, false]);
    }

    #[test]
    fn pauses_second_byte_never_counts_and_ends_the_half() {
        // Were Pause's second byte (`45`) taken as a key, or its half
        // ended a byte early or late, these would differ: a make right
        // after each half counts, and only it.
        assert_eq!(fed(&[0xE1, 0x1D, 0x45, 0x1E]), [false, true, false, true]);
        assert_eq!(fed(&[0xE1, 0x9D, 0xC5, 0x1E]), [false, false, false, true]);
    }

    /// The keys held after `bytes`, fed to a fresh counter.
    fn held(bytes: &[u8]) -> u32 {
        let mut p = Presses::new();
        bytes.iter().for_each(|&b| {
            p.feed(b);
        });
        p.held()
    }

    #[test]
    fn keys_are_held_from_press_to_release() {
        assert_eq!(held(&[]), 0);
        assert_eq!(held(&[0x1E]), 1);
        assert_eq!(held(&[0x1E, 0x9E]), 0);
        // shift held over a, then released: none; shift's release lost:
        // one.
        assert_eq!(held(&[0x2A, 0x1E, 0x9E, 0xAA]), 0);
        assert_eq!(held(&[0x2A, 0x1E, 0x9E]), 1);
        // A repeat holds nothing more; a release of a key not held changes
        // nothing.
        assert_eq!(held(&[0x1E, 0x1E, 0x1E]), 1);
        assert_eq!(held(&[0x9E]), 0);
        assert_eq!(held(&[0x9E, 0x30]), 1);
    }

    #[test]
    fn an_extended_key_is_held_apart_from_its_plain_twin() {
        // Up (E0 48) and keypad 8 (48) are two keys: releasing one leaves
        // the other held. A lost E0 before Up's release releases keypad 8
        // instead, leaving Up held.
        assert_eq!(held(&[0xE0, 0x48, 0x48, 0xE0, 0xC8]), 1);
        assert_eq!(held(&[0xE0, 0x48, 0xC8]), 1);
        assert_eq!(held(&[0xE0, 0x48, 0xE0, 0xC8]), 0);
        // The highest codes of both halves have their own slots.
        assert_eq!(held(&[0x7F, 0xE0, 0x7F]), 2);
        assert_eq!(held(&[0x7F, 0xE0, 0x7F, 0xFF]), 1);
    }

    #[test]
    fn print_screen_and_pause_leave_nothing_held() {
        // Print Screen's fake shifts are never held; its key is, until its
        // release. Pause is never released, and never held.
        assert_eq!(held(&[0xE0, 0x2A, 0xE0, 0x37]), 1);
        assert_eq!(held(&[0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA]), 0);
        assert_eq!(held(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]), 0);
        assert_eq!(held(&[0xE0, 0x46, 0xE0, 0xC6]), 0);
    }

    #[test]
    fn a_mixed_stream_counts_every_key_where_it_ends() {
        // e c h o, Up, Pause, Print Screen, shift+1: which byte completes
        // each press, not only how many (two errors can't cancel out).
        let stream: &[(u8, bool)] = &[
            (0x12, true),
            (0x92, false),
            (0x2E, true),
            (0xAE, false),
            (0x23, true),
            (0xA3, false),
            (0x18, true),
            (0x98, false),
            (0xE0, false),
            (0x48, true),
            (0xE0, false),
            (0xC8, false),
            (0xE1, false),
            (0x1D, true),
            (0x45, false),
            (0xE1, false),
            (0x9D, false),
            (0xC5, false),
            (0xE0, false),
            (0x2A, false),
            (0xE0, false),
            (0x37, true),
            (0xE0, false),
            (0xB7, false),
            (0xE0, false),
            (0xAA, false),
            (0x2A, true),
            (0x02, true),
            (0x82, false),
            (0xAA, false),
        ];
        let bytes: Vec<u8> = stream.iter().map(|&(b, _)| b).collect();
        let want: Vec<bool> = stream.iter().map(|&(_, p)| p).collect();
        assert_eq!(fed(&bytes), want);
        assert_eq!(presses(&bytes), 9);
        assert_eq!(held(&bytes), 0);
    }
}
