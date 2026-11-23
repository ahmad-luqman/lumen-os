//! Repeated log lines, counted: the table behind the kernel's held-back
//! repeats (`klog::hold_repeats`, ktest kernels only, #347). Each key (the
//! kernel's: a log call site with its level) is seen a first time, its line
//! then printed, and after that held back: counted, the newest one's text
//! kept, until [`Repeats::held`] reports them and [`Repeats::clear`] forgets
//! them. [`Seen::is_held`] is the decision the logger acts on.
//!
//! Fixed-size, no allocation: the logger fills it holding its line lock,
//! which code holding the frame allocator's lock may log under. A key past
//! the table's `N` is never held ([`Seen::Full`]): its lines are printed as
//! if nothing were held, so a full table loses no line.

use core::fmt;

/// What [`Repeats::see`] made of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a line is printed or held by what was seen"]
pub enum Seen {
    /// Its first line: print it.
    First,
    /// A repeat: counted, its text kept as the newest, held back.
    Repeat,
    /// A new key with the table full: print it, nothing counted or kept.
    Full,
}

impl Seen {
    /// Whether the line is held back (else it is printed): a repeat only.
    pub fn is_held(self) -> bool {
        self == Seen::Repeat
    }
}

/// A line's text, the first `B` bytes of it (cut at a character boundary).
pub struct Text<const B: usize> {
    buf: [u8; B],
    len: usize,
    cut: bool,
}

impl<const B: usize> Text<B> {
    const fn new() -> Self {
        Text { buf: [0; B], len: 0, cut: false }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.cut = false;
    }

    /// The text kept (all of it unless [`is_cut`](Self::is_cut)).
    pub fn as_str(&self) -> &str {
        let text = core::str::from_utf8(&self.buf[..self.len]);
        // `write_str` copies whole characters only.
        debug_assert!(text.is_ok());
        text.unwrap_or("")
    }

    /// Whether the text was longer than `B` bytes and is cut short.
    pub fn is_cut(&self) -> bool {
        self.cut
    }
}

impl<const B: usize> fmt::Write for Text<B> {
    /// Copies what fits, whole characters only, and nothing once cut (a
    /// later piece that fits would read as if nothing were missing); never
    /// fails (a failure would end the caller's formatting part way, as a cut
    /// does anyway).
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cut {
            return Ok(());
        }
        let room = B - self.len;
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        self.cut = take < s.len();
        Ok(())
    }
}

/// Shown with `...` after it if it was cut, so a cut text never reads as a
/// whole one.
impl<const B: usize> fmt::Display for Text<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())?;
        if self.cut { f.write_str("...") } else { Ok(()) }
    }
}

struct Entry<K, const B: usize> {
    key: K,
    /// Lines held back after the first.
    held: u64,
    /// The newest line's text (the first's until one is held).
    newest: Text<B>,
}

/// Up to `N` keys, each with how many lines after its first were held back
/// and the newest one's text (up to `B` bytes), in the order first seen.
/// The keys fill the slots from the front: the first empty slot ends them.
pub struct Repeats<K, const N: usize, const B: usize> {
    entries: [Option<Entry<K, B>>; N],
}

impl<K: PartialEq, const N: usize, const B: usize> Default for Repeats<K, N, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: PartialEq, const N: usize, const B: usize> Repeats<K, N, B> {
    pub const fn new() -> Self {
        const { assert!(N > 0) };
        Repeats { entries: [const { None }; N] }
    }

    /// The keys' entries, in the order first seen.
    fn used(&self) -> impl Iterator<Item = &Entry<K, B>> {
        self.entries.iter().map_while(Option::as_ref)
    }

    /// A line with `key`: its first, a repeat (counted), or a new key with
    /// the table full. `write` writes the line's text into the kept one,
    /// for a first and a repeat (the newest kept); not for a full table.
    pub fn see(&mut self, key: K, write: impl FnOnce(&mut Text<B>)) -> Seen {
        for slot in &mut self.entries {
            match slot {
                Some(entry) if entry.key == key => {
                    entry.held += 1;
                    entry.newest.clear();
                    write(&mut entry.newest);
                    return Seen::Repeat;
                }
                Some(_) => {}
                None => {
                    let entry = slot.insert(Entry { key, held: 0, newest: Text::new() });
                    write(&mut entry.newest);
                    return Seen::First;
                }
            }
        }
        Seen::Full
    }

    /// Each key that had lines held back: how many, and the newest one's
    /// text, in the order first seen.
    pub fn held(&self) -> impl Iterator<Item = (u64, &Text<B>)> {
        self.used().filter(|e| e.held > 0).map(|e| (e.held, &e.newest))
    }

    /// Whether no key has been seen since it was made or cleared.
    pub fn is_empty(&self) -> bool {
        self.entries[0].is_none()
    }

    /// Forget every key (in place: nothing this size is moved).
    pub fn clear(&mut self) {
        for slot in &mut self.entries {
            if slot.take().is_none() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::fmt::Write;
    use std::string::{String, ToString};
    use std::vec::Vec;

    fn summary<const N: usize, const B: usize>(r: &Repeats<u32, N, B>) -> Vec<(u64, String)> {
        r.held().map(|(n, t)| (n, t.to_string())).collect()
    }

    fn line(text: &str) -> impl FnOnce(&mut Text<32>) + '_ {
        move |t| t.write_str(text).unwrap()
    }

    #[test]
    fn a_keys_first_line_is_printed_and_its_repeats_held_with_the_newest_kept() {
        let mut r = Repeats::<u32, 4, 32>::new();
        assert_eq!(r.see(7, |t| write!(t, "seven {}", 0).unwrap()), Seen::First);
        assert_eq!(r.see(9, line("nine 0")), Seen::First);
        for i in 1..4 {
            assert_eq!(r.see(7, |t| write!(t, "seven {i}").unwrap()), Seen::Repeat);
        }
        // Only keys with repeats are reported, in the order first seen,
        // each with its newest line's text.
        assert_eq!(summary(&r), [(3, "seven 3".into())]);
        let _ = r.see(9, line("nine 1"));
        assert_eq!(r.see(5, line("five 0")), Seen::First);
        let _ = r.see(5, line("five 1"));
        assert_eq!(summary(&r), [(3, "seven 3".into()), (1, "nine 1".into()), (1, "five 1".into())]);
    }

    #[test]
    fn only_a_repeat_is_held() {
        assert!(Seen::Repeat.is_held());
        // A first line and a full table's line are printed.
        assert!(!Seen::First.is_held());
        assert!(!Seen::Full.is_held());
    }

    #[test]
    fn a_new_key_past_the_table_is_never_held() {
        let mut r = Repeats::<u32, 2, 32>::new();
        let _ = r.see(1, line("one"));
        let _ = r.see(2, line("two"));
        // Every line of a third key is printed: none counted or kept.
        for _ in 0..3 {
            let seen = r.see(3, |_| panic!("a full table kept a text"));
            assert_eq!(seen, Seen::Full);
            assert!(!seen.is_held());
        }
        // The keys it has still count.
        assert_eq!(r.see(2, line("two again")), Seen::Repeat);
        assert_eq!(summary(&r), [(1, "two again".into())]);
    }

    #[test]
    fn clearing_forgets_every_key() {
        let mut r = Repeats::<u32, 2, 32>::new();
        assert!(r.is_empty());
        // A key seen once, nothing held: not empty all the same.
        let _ = r.see(1, line("one"));
        assert!(!r.is_empty() && summary(&r).is_empty());
        let _ = r.see(1, line("one again"));
        let _ = r.see(2, line("two"));
        r.clear();
        assert!(r.is_empty());
        assert_eq!(summary(&r), []);
        // A key seen before is a first again, and the room is back.
        assert_eq!(r.see(2, line("two")), Seen::First);
        assert_eq!(r.see(3, line("three")), Seen::First);
        let _ = r.see(2, line("two again"));
        assert_eq!(summary(&r), [(1, "two again".into())]);
    }

    #[test]
    fn a_long_text_is_cut_at_a_character_and_says_so() {
        let mut r = Repeats::<u32, 2, 8>::new();
        let _ = r.see(1, |_| {});
        // "abcdefg" and then 'é' (2 bytes) would end at byte 9: cut before it.
        let _ = r.see(1, |t| t.write_str("abcdefgé and more").unwrap());
        let (_, text) = r.held().next().unwrap();
        assert_eq!((text.as_str(), text.is_cut()), ("abcdefg", true));
        assert_eq!(text.to_string(), "abcdefg...");
        // Written in pieces, exactly filling it: whole, not cut.
        let _ = r.see(1, |t| ["abcd", "efgh"].iter().for_each(|s| t.write_str(s).unwrap()));
        let (_, text) = r.held().next().unwrap();
        assert_eq!((text.as_str(), text.is_cut(), text.to_string()), ("abcdefgh", false, "abcdefgh".into()));
        // One byte more, in a later piece: cut.
        let _ = r.see(1, |t| ["abcdefgh", "i"].iter().for_each(|s| t.write_str(s).unwrap()));
        assert_eq!(summary(&r), [(3, "abcdefgh...".into())]);
        // Once cut, a later piece that would fit isn't added: the text
        // stays cut, never reading as whole.
        let _ = r.see(1, |t| ["abcdefgé", "x"].iter().for_each(|s| t.write_str(s).unwrap()));
        assert_eq!(summary(&r), [(4, "abcdefg...".into())]);
        // A repeat's text replaces the last one's whole, cut or not.
        let _ = r.see(1, line_8("short"));
        assert_eq!(summary(&r), [(5, "short".into())]);
    }

    fn line_8(text: &str) -> impl FnOnce(&mut Text<8>) + '_ {
        move |t| t.write_str(text).unwrap()
    }

    #[test]
    fn matches_a_model_over_many_lines() {
        // Against a model listing the keys in the order first seen, up to
        // `N`, each with its repeats' count and newest text: a key past
        // them is never held.
        const N: usize = 5;
        let mut r = Repeats::<u32, N, 24>::new();
        let mut model: Vec<(u32, u64, String)> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for i in 0..2000u32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let key = (state % 9) as u32;
            let text = std::format!("key {key} line {i}");
            let seen = r.see(key, |t| t.write_str(&text).unwrap());
            let kept = model.len();
            let want = match model.iter_mut().find(|(k, ..)| *k == key) {
                Some((_, held, newest)) => {
                    *held += 1;
                    *newest = text.clone();
                    Seen::Repeat
                }
                None if kept < N => {
                    model.push((key, 0, text.clone()));
                    Seen::First
                }
                None => Seen::Full,
            };
            assert_eq!(seen, want, "line {i}");
        }
        let want: Vec<(u64, String)> =
            model.into_iter().filter(|(_, held, _)| *held > 0).map(|(_, held, text)| (held, text)).collect();
        assert_eq!(summary(&r), want);
    }
}
