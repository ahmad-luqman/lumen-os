//! The splitter against `shlex` on random lines made of the characters that
//! matter to quoting: both must agree on the words, and on which lines are
//! errors.

/// A tiny random number generator (xorshift64), seeded.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[test]
fn split_agrees_with_shlex() {
    let alphabet = ['a', 'b', ' ', ' ', '\t', '\n', '\'', '"', '\\', '#', '$', '`', 'é'];
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut errors, mut words) = (0, 0);
    for _ in 0..200_000 {
        let len = rng.below(12);
        let line: String = (0..len).map(|_| alphabet[rng.below(alphabet.len())]).collect();
        let ours = lumen_shell::words::split(&line).ok();
        assert_eq!(ours, shlex::split(&line), "{line:?}");
        match ours {
            None => errors += 1,
            Some(w) => words += w.len(),
        }
    }
    // Both kinds of lines came up plenty.
    assert!(errors > 10_000 && words > 100_000, "{errors} errors, {words} words");
}
