//! Repeated log lines, counted: the table behind the kernel's held-back
//! repeats (`klog::hold_repeats`, ktest kernels only, #347). Each key (the
//! kernel's: a log call site with its level) is seen a first time, its line
//! then printed and its text kept here, and after that only counted, until
//! [`Repeats::held`] reports the counts and [`Repeats::clear`] forgets them.
//!
//! Fixed-size, no allocation: the logger fills it holding its line lock,
//! which code holding the frame allocator's lock may log under. A key past
//! the table's `N` is never held ([`Seen::Full`]): its lines are printed as
//! if nothing were held, so a full table loses no line.

use core::fmt;

/// What [`Repeats::see`] made of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Its first line: print it (its text is now kept).
    First,
    /// A repeat: counted, held back.
    Repeat,
    /// A new key with the table full: print it, nothing counted.
    Full,
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

    /// The text kept (all of it unless [`is_cut`](Self::is_cut)).
    pub fn as_str(&self) -> &str {
        // `write_str` copies whole characters only, so this is UTF-8.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// Whether the text was longer than `B` bytes and is cut short.
    pub fn is_cut(&self) -> bool {
        self.cut
    }
}

impl<const B: usize> fmt::Write for Text<B> {
    /// Copies what fits, whole characters only; never fails (a failure
    /// would end the caller's formatting part way, as a cut does anyway).
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
    first: Text<B>,
}

/// Up to `N` keys, each with its first line's text (up to `B` bytes) and
/// how many lines after it were held back, in the order first seen.
pub struct Repeats<K, const N: usize, const B: usize> {
    entries: [Option<Entry<K, B>>; N],
    len: usize,
}

impl<K: PartialEq, const N: usize, const B: usize> Default for Repeats<K, N, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: PartialEq, const N: usize, const B: usize> Repeats<K, N, B> {
    pub const fn new() -> Self {
        Repeats { entries: [const { None }; N], len: 0 }
    }

    /// A line with `key`: its first (`write_first` then writes its text
    /// into the kept one), a repeat (counted), or a new key with the table
    /// full (not kept: `write_first` isn't called).
    pub fn see(&mut self, key: K, write_first: impl FnOnce(&mut Text<B>)) -> Seen {
        let found = self.entries[..self.len].iter_mut().flatten().find(|e| e.key == key);
        if let Some(entry) = found {
            entry.held += 1;
            return Seen::Repeat;
        }
        let Some(slot) = self.entries.get_mut(self.len) else {
            return Seen::Full;
        };
        let entry = slot.insert(Entry { key, held: 0, first: Text::new() });
        write_first(&mut entry.first);
        self.len += 1;
        Seen::First
    }

    /// Each key that had lines held back: how many, and its first line's
    /// text, in the order first seen.
    pub fn held(&self) -> impl Iterator<Item = (u64, &Text<B>)> {
        self.entries[..self.len].iter().flatten().filter(|e| e.held > 0).map(|e| (e.held, &e.first))
    }

    /// Forget every key (in place: nothing this size is moved).
    pub fn clear(&mut self) {
        for slot in &mut self.entries[..self.len] {
            *slot = None;
        }
        self.len = 0;
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

    #[test]
    fn the_first_line_of_a_key_is_kept_and_its_repeats_counted() {
        let mut r = Repeats::<u32, 4, 32>::new();
        assert_eq!(r.see(7, |t| write!(t, "seven {}", 0).unwrap()), Seen::First);
        assert_eq!(r.see(9, |t| t.write_str("nine").unwrap()), Seen::First);
        // A repeat doesn't write: its text isn't asked for.
        for _ in 0..3 {
            assert_eq!(r.see(7, |_| panic!("a repeat's text written")), Seen::Repeat);
        }
        // Only keys with repeats are reported, in the order first seen,
        // each with its first line's text.
        assert_eq!(summary(&r), [(3, "seven 0".into())]);
        r.see(9, |_| {});
        assert_eq!(r.see(5, |t| t.write_str("five").unwrap()), Seen::First);
        r.see(5, |_| {});
        assert_eq!(summary(&r), [(3, "seven 0".into()), (1, "nine".into()), (1, "five".into())]);
    }

    #[test]
    fn a_new_key_past_the_table_is_never_held() {
        let mut r = Repeats::<u32, 2, 16>::new();
        r.see(1, |t| t.write_str("one").unwrap());
        r.see(2, |t| t.write_str("two").unwrap());
        // Every line of a third key is printed: none counted or kept.
        for _ in 0..3 {
            assert_eq!(r.see(3, |_| panic!("a full table kept a text")), Seen::Full);
        }
        // The keys it has still count.
        assert_eq!(r.see(2, |_| {}), Seen::Repeat);
        assert_eq!(summary(&r), [(1, "two".into())]);
    }

    #[test]
    fn clearing_forgets_every_key() {
        let mut r = Repeats::<u32, 2, 16>::new();
        r.see(1, |t| t.write_str("one").unwrap());
        r.see(1, |_| {});
        r.see(2, |t| t.write_str("two").unwrap());
        r.clear();
        assert_eq!(summary(&r), []);
        // A key seen before is a first again, with its new text, and the
        // room is back.
        assert_eq!(r.see(2, |t| t.write_str("two again").unwrap()), Seen::First);
        assert_eq!(r.see(3, |t| t.write_str("three").unwrap()), Seen::First);
        r.see(2, |_| {});
        assert_eq!(summary(&r), [(1, "two again".into())]);
    }

    #[test]
    fn a_long_text_is_cut_at_a_character_and_says_so() {
        let mut r = Repeats::<u32, 2, 8>::new();
        // "abcdefg" and then 'é' (2 bytes) would end at byte 9: cut before it.
        r.see(1, |t| t.write_str("abcdefgé and more").unwrap());
        r.see(1, |_| {});
        let (_, text) = r.held().next().unwrap();
        assert_eq!((text.as_str(), text.is_cut()), ("abcdefg", true));
        assert_eq!(text.to_string(), "abcdefg...");
        // Written in pieces, exactly filling it: whole, not cut.
        let mut r = Repeats::<u32, 2, 8>::new();
        r.see(1, |t| ["abcd", "efgh"].iter().for_each(|s| t.write_str(s).unwrap()));
        r.see(1, |_| {});
        let (_, text) = r.held().next().unwrap();
        assert_eq!((text.as_str(), text.is_cut(), text.to_string()), ("abcdefgh", false, "abcdefgh".into()));
        // One byte more, in a later piece: cut.
        let mut r = Repeats::<u32, 2, 8>::new();
        r.see(1, |t| ["abcdefgh", "i"].iter().for_each(|s| t.write_str(s).unwrap()));
        r.see(1, |_| {});
        assert_eq!(summary(&r), [(1, "abcdefgh...".into())]);
    }

    #[test]
    fn matches_a_model_over_many_lines() {
        // Against a model keeping every key's lines: the first `N` keys
        // seen are kept with their first text and counted; later keys are
        // never held.
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
                Some((_, held, _)) => {
                    *held += 1;
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
