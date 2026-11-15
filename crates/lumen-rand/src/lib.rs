//! ChaCha20 and the entropy pool behind the kernel's `rng` (#97).
//!
//! - [`block`] is RFC 8439's ChaCha20 block function (section 2.3): a
//!   32-byte key, a 32-bit block counter and a 96-bit nonce in, 64 bytes of
//!   keystream out.
//! - [`Pool`] absorbs entropy from the kernel's sources (RDRAND, virtio-rng)
//!   and hands out [`Stream`]s. Absorbing XORs each 32-byte chunk into the
//!   key and replaces the key with the first half of a block under it, so
//!   every chunk changes every later output. A stream is made by "fast key
//!   erasure": one block under the pool's key gives the stream's key (its
//!   second half) and the pool's next key (its first half), so neither
//!   reveals the other, and a pool key read later can't recover streams
//!   handed out before.
//! - Each use of the block function gets a nonce of its own: absorbing and
//!   splitting use their domain word and their own 64-bit count, a stream
//!   uses the stream domain and the high half of its 64-bit block number
//!   (the low half is the block counter), so absorbing, splitting and
//!   streaming never share a keystream.
//! - The pool counts the bytes it was given (each is credited as one byte of
//!   entropy: the kernel only mixes in its sources' output) and refuses to
//!   hand out a stream until it has [`SEED_BYTES`]. Streams come only from
//!   a seeded pool (`Stream`'s constructor is private).
//! - Pure helpers for the kernel's sources, host-tested with fakes:
//!   [`retry`] (a hardware draw tried up to [`RETRIES`] times),
//!   [`fill_words`], [`all_same`] (a stuck source, or a device answering one
//!   repeated byte) and [`below`] (a number below a bound, without modulo
//!   bias).
//!
//! Not audited, not constant-time-reviewed: a hobby OS's randomness. The
//! code is safe Rust. Keys and buffers are cleared ([`clear`]) when dropped
//! or used up, but the compiler may still keep copies elsewhere (registers,
//! moved-from stack slots): clearing is not a guarantee.

#![cfg_attr(not(test), no_std)]

/// The key length (bytes).
const KEY_LEN: usize = 32;
/// The block length (bytes).
const BLOCK_LEN: usize = 64;
/// How many bytes of entropy the pool needs before it hands out a stream.
pub const SEED_BYTES: usize = 32;

/// RFC 8439's constants: "expand 32-byte k".
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Nonce domains (the nonce's first word).
const MIX: u32 = 1;
const SPLIT: u32 = 2;
const STREAM: u32 = 3;

#[inline(always)]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// ChaCha20's block function (RFC 8439 section 2.3): 20 rounds over the
/// constants, `key`, `counter` and `nonce`, plus the input, serialized
/// little-endian.
pub(crate) fn block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; 12]) -> [u8; BLOCK_LEN] {
    let mut input = [0u32; 16];
    input[..4].copy_from_slice(&SIGMA);
    for (w, b) in input[4..12].iter_mut().zip(key.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*b);
    }
    input[12] = counter;
    for (w, b) in input[13..].iter_mut().zip(nonce.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*b);
    }
    let mut s = input;
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; BLOCK_LEN];
    for (i, chunk) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        chunk.copy_from_slice(&s[i].wrapping_add(input[i]).to_le_bytes());
    }
    s.fill(0);
    input.fill(0);
    core::hint::black_box((&s, &input));
    out
}

/// A nonce in `domain`, numbered `n`.
fn nonce(domain: u32, n: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..4].copy_from_slice(&domain.to_le_bytes());
    out[4..].copy_from_slice(&n.to_le_bytes());
    out
}

/// A block's two halves, as keys.
fn halves(b: &[u8; BLOCK_LEN]) -> (&[u8; KEY_LEN], &[u8; KEY_LEN]) {
    let ([first, second], _) = b.as_chunks::<KEY_LEN>() else { unreachable!("a block is two keys long") };
    (first, second)
}

/// Overwrite a secret buffer with zeros, through `black_box` so the stores
/// aren't optimized away as dead. Copies made earlier (registers, other
/// stack slots) aren't reached.
pub fn clear(bytes: &mut [u8]) {
    bytes.fill(0);
    core::hint::black_box(bytes);
}

/// The entropy pool: absorbs sources' output ([`Pool::mix`]) and hands out
/// [`Stream`]s once seeded (the module doc has the construction).
pub struct Pool {
    key: [u8; KEY_LEN],
    /// Bytes mixed in so far (saturating).
    credited: usize,
    /// Chunks absorbed and streams split off so far: each block the pool
    /// computes has a nonce of its own.
    mixes: u64,
    splits: u64,
}

impl Default for Pool {
    fn default() -> Self {
        Pool::new()
    }
}

impl Pool {
    /// An empty pool: an all-zero key, nothing credited.
    pub const fn new() -> Pool {
        Pool { key: [0; KEY_LEN], credited: 0, mixes: 0, splits: 0 }
    }

    /// Absorb `input`, crediting each byte as one byte of entropy. Each
    /// 32-byte chunk (the last may be shorter) is XORed into the key, which
    /// is then replaced by the first half of a block under it (the chunk's
    /// length is the block counter, so a short chunk differs from the same
    /// bytes followed by zeros). Empty input changes nothing.
    pub fn mix(&mut self, input: &[u8]) {
        for chunk in input.chunks(KEY_LEN) {
            for (k, b) in self.key.iter_mut().zip(chunk) {
                *k ^= b;
            }
            let mut b = block(&self.key, chunk.len() as u32, &nonce(MIX, self.mixes));
            self.key = *halves(&b).0;
            clear(&mut b);
            self.mixes = self.mixes.wrapping_add(1);
        }
        self.credited = self.credited.saturating_add(input.len());
    }

    /// The bytes mixed in so far (saturating).
    pub fn credited(&self) -> usize {
        self.credited
    }

    /// At least [`SEED_BYTES`] mixed in.
    pub fn is_seeded(&self) -> bool {
        self.credited >= SEED_BYTES
    }

    /// A stream keyed from the pool, the pool's key replaced (fast key
    /// erasure); `None` until the pool is seeded.
    pub fn stream(&mut self) -> Option<Stream> {
        if !self.is_seeded() {
            return None;
        }
        let mut b = block(&self.key, 0, &nonce(SPLIT, self.splits));
        self.splits = self.splits.wrapping_add(1);
        let (next, stream_key) = halves(&b);
        self.key = *next;
        let stream = Stream::new(*stream_key);
        clear(&mut b);
        Some(stream)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        clear(&mut self.key);
    }
}

/// ChaCha20's keystream under one key: blocks 0, 1, 2... (the block number's
/// low 32 bits as the counter, its high 32 bits in the nonce, after the
/// stream domain). Never runs out in practice (2^64 blocks).
pub struct Stream {
    key: [u8; KEY_LEN],
    /// The next block to compute.
    next: u64,
    buf: [u8; BLOCK_LEN],
    /// Bytes of `buf` already handed out.
    used: usize,
}

impl Stream {
    /// The keystream under `key` (only [`Pool::stream`] makes one, from a
    /// seeded pool).
    pub(crate) fn new(key: [u8; KEY_LEN]) -> Stream {
        Stream { key, next: 0, buf: [0; BLOCK_LEN], used: BLOCK_LEN }
    }

    /// The next `out.len()` bytes of the keystream.
    pub fn fill(&mut self, out: &mut [u8]) {
        let mut at = 0;
        while at < out.len() {
            if self.used == BLOCK_LEN {
                let n = self.next;
                self.buf = block(&self.key, n as u32, &nonce(STREAM, n >> 32));
                self.next = n.wrapping_add(1);
                self.used = 0;
            }
            let take = (BLOCK_LEN - self.used).min(out.len() - at);
            out[at..at + take].copy_from_slice(&self.buf[self.used..self.used + take]);
            // Handed out: not kept.
            clear(&mut self.buf[self.used..self.used + take]);
            self.used += take;
            at += take;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        clear(&mut self.key);
        clear(&mut self.buf);
    }
}

/// How many times [`retry`] tries a draw (Intel's guidance for RDRAND: a
/// failure means the generator is momentarily drained; 10 failures in a row
/// mean something is wrong).
pub const RETRIES: u32 = 10;

/// One draw from a hardware source, tried up to [`RETRIES`] times: the first
/// value `try_once` gives, or `None` if every try failed.
pub fn retry(mut try_once: impl FnMut() -> Option<u64>) -> Option<u64> {
    (0..RETRIES).find_map(|_| try_once())
}

/// A draw that failed every one of its [`RETRIES`] tries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawFailed;

/// Fill `out` with one [`retry`]ed draw per word (little-endian). If a draw
/// fails, `out` is cleared and the error returned (no partial output).
pub fn fill_words(out: &mut [[u8; 8]], mut try_once: impl FnMut() -> Option<u64>) -> Result<(), DrawFailed> {
    for word in out.iter_mut() {
        match retry(&mut try_once) {
            Some(v) => *word = v.to_le_bytes(),
            None => {
                clear(out.as_flattened_mut());
                return Err(DrawFailed);
            }
        }
    }
    Ok(())
}

/// More than one item, all equal: a stuck source (RDRAND returning one
/// value, a device answering one repeated byte or all zeros).
pub fn all_same<T: PartialEq>(items: &[T]) -> bool {
    items.len() > 1 && items.iter().all(|x| *x == items[0])
}

/// The smallest value [`below`] accepts for `bound`: `2^64 mod bound`, so
/// the accepted values `zone_start(bound)..=u64::MAX` are a whole number
/// of runs of `bound` values each, and `v % bound` is uniform over them.
fn zone_start(bound: u64) -> u64 {
    bound.wrapping_neg() % bound
}

/// A number below `bound` (above 0) from `next`'s uniform 64-bit values,
/// without modulo bias: values below [`zone_start`] are drawn again.
/// Panics if `bound` is 0.
pub fn below<E>(bound: u64, mut next: impl FnMut() -> Result<u64, E>) -> Result<u64, E> {
    assert!(bound > 0, "below(0)");
    let min = zone_start(bound);
    loop {
        let v = next()?;
        if v >= min {
            return Ok(v % bound);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn key(bytes: &[u8]) -> [u8; KEY_LEN] {
        bytes.try_into().unwrap()
    }

    #[test]
    fn block_matches_rfc_8439_section_2_3_2() {
        let k = key(&(0..32).collect::<Vec<u8>>());
        let n: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        let want = hex("10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e");
        assert_eq!(block(&k, 1, &n).to_vec(), want);
    }

    #[test]
    fn block_matches_rfc_8439_appendix_a_1_vector_1() {
        let want = hex("76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586");
        assert_eq!(block(&[0; 32], 0, &[0; 12]).to_vec(), want);
    }

    #[test]
    fn a_stream_is_chacha20s_keystream_in_order() {
        let k = key(&[7; 32]);
        let mut s = Stream::new(k);
        let mut out = [0u8; 3 * BLOCK_LEN];
        s.fill(&mut out);
        for i in 0..3 {
            let want = block(&k, i as u32, &nonce(STREAM, 0));
            assert_eq!(&out[i * BLOCK_LEN..(i + 1) * BLOCK_LEN], &want[..], "block {i}");
        }
    }

    #[test]
    fn the_block_number_carries_into_the_nonce() {
        let k = key(&[9; 32]);
        let mut s = Stream::new(k);
        s.next = u64::from(u32::MAX);
        let mut out = [0u8; 2 * BLOCK_LEN];
        s.fill(&mut out);
        assert_eq!(&out[..BLOCK_LEN], &block(&k, u32::MAX, &nonce(STREAM, 0))[..]);
        assert_eq!(&out[BLOCK_LEN..], &block(&k, 0, &nonce(STREAM, 1))[..]);
    }

    #[test]
    fn filling_in_pieces_gives_the_same_bytes() {
        let k = key(&[3; 32]);
        let mut whole = [0u8; 300];
        Stream::new(k).fill(&mut whole);
        let mut s = Stream::new(k);
        let mut pieces = Vec::new();
        for len in [1, 63, 0, 64, 65, 7, 100] {
            let mut b = vec![0u8; len];
            s.fill(&mut b);
            pieces.extend(b);
        }
        assert_eq!(pieces, whole);
    }

    #[test]
    fn an_unseeded_pool_hands_out_nothing() {
        let mut p = Pool::new();
        assert!(p.stream().is_none());
        p.mix(&[1; SEED_BYTES - 1]);
        assert!(!p.is_seeded() && p.stream().is_none());
        assert_eq!(p.credited(), SEED_BYTES - 1);
        p.mix(&[]);
        assert!(p.stream().is_none());
        p.mix(&[2]);
        assert!(p.is_seeded());
        assert!(p.stream().is_some());
    }

    fn first_bytes(p: &mut Pool) -> [u8; 32] {
        let mut b = [0u8; 32];
        p.stream().unwrap().fill(&mut b);
        b
    }

    fn pool_of(inputs: &[&[u8]]) -> Pool {
        let mut p = Pool::new();
        for i in inputs {
            p.mix(i);
        }
        p
    }

    #[test]
    fn the_same_input_gives_the_same_output() {
        let input = [0x5a; 40];
        assert_eq!(first_bytes(&mut pool_of(&[&input])), first_bytes(&mut pool_of(&[&input])));
    }

    #[test]
    fn every_input_byte_and_its_order_matter() {
        let base = [0x11u8; 100];
        let reference = first_bytes(&mut pool_of(&[&base]));
        for at in [0, 31, 32, 63, 64, 99] {
            let mut changed = base;
            changed[at] ^= 1;
            assert_ne!(first_bytes(&mut pool_of(&[&changed])), reference, "byte {at}");
        }
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_ne!(first_bytes(&mut pool_of(&[&a, &b])), first_bytes(&mut pool_of(&[&b, &a])));
        // A short chunk isn't the same bytes padded with zeros.
        let mut padded = [0u8; 32];
        padded[..31].copy_from_slice(&[4; 31]);
        let mut short = pool_of(&[&[4; 31]]);
        short.mix(&[9]);
        let mut long = pool_of(&[&padded]);
        long.mix(&[9]);
        assert_ne!(first_bytes(&mut short), first_bytes(&mut long));
    }

    #[test]
    fn each_stream_differs_and_doesnt_reveal_the_next() {
        let mut p = pool_of(&[&[0xa5; 32]]);
        let key_before = p.key;
        let (a, b) = (first_bytes(&mut p), first_bytes(&mut p));
        assert_ne!(a, b);
        // The pool's key moved on, and no stream key is the pool's key.
        assert_ne!(p.key, key_before);
        let mut q = pool_of(&[&[0xa5; 32]]);
        let s = q.stream().unwrap();
        assert_ne!(s.key, q.key);
        assert_ne!(s.key, key_before);
        // The same pool state gives the same first stream (deterministic).
        let mut r = pool_of(&[&[0xa5; 32]]);
        assert_eq!(first_bytes(&mut r), a);
    }

    #[test]
    fn mixing_after_a_stream_changes_the_next_one() {
        let mut p = pool_of(&[&[1; 32]]);
        let mut q = pool_of(&[&[1; 32]]);
        let _ = first_bytes(&mut p);
        let _ = first_bytes(&mut q);
        q.mix(&[0]);
        assert_ne!(first_bytes(&mut p), first_bytes(&mut q));
    }

    #[test]
    fn handed_out_bytes_are_not_kept_in_the_stream() {
        let mut s = Stream::new([8; 32]);
        let mut b = [0u8; 10];
        s.fill(&mut b);
        assert!(s.buf[..10].iter().all(|&x| x == 0));
        assert!(s.buf[10..].iter().any(|&x| x != 0));
    }

    const KAT_FIRST: &str = "2b38654b3a9db6c10067023d1b012e8c416fdaaf0940cca8bf450835359ec32d\
                             680214b50d251cc2aa1ccf67562050220ba4666ccfdd9e2a6daec09d851cc295\
                             8758629251f1746ee20929890ab33440";
    const KAT_SECOND: &str = "76c3b043d985e8ce763c303f6e3a74ac";

    #[test]
    fn a_pool_gives_known_bytes_for_known_input() {
        // Pins the construction (the domains, the mix and split counters,
        // the chunk length as the counter): computed once by this code.
        let mut p = pool_of(&[&(0..40).collect::<Vec<u8>>(), b"lumen"]);
        let mut first = [0u8; 80];
        p.stream().unwrap().fill(&mut first);
        let mut second = [0u8; 16];
        p.stream().unwrap().fill(&mut second);
        assert_eq!(first.to_vec(), hex(KAT_FIRST), "first stream");
        assert_eq!(second.to_vec(), hex(KAT_SECOND), "second stream");
    }

    #[test]
    fn retry_tries_ten_times_then_gives_up() {
        let mut calls = 0;
        assert_eq!(
            retry(|| {
                calls += 1;
                None
            }),
            None
        );
        assert_eq!(calls, RETRIES);
        // Fails 9 times, then answers: the answer.
        let mut calls = 0;
        assert_eq!(
            retry(|| {
                calls += 1;
                (calls == RETRIES).then_some(7)
            }),
            Some(7)
        );
        // Answers at once: tried once.
        let mut calls = 0;
        assert_eq!(
            retry(|| {
                calls += 1;
                Some(1)
            }),
            Some(1)
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn fill_words_fills_each_word_or_nothing() {
        let mut out = [[0xffu8; 8]; 3];
        let mut n = 0u64;
        assert_eq!(
            fill_words(&mut out, || {
                n += 1;
                Some(n)
            }),
            Ok(())
        );
        assert_eq!(out, [1u64.to_le_bytes(), 2u64.to_le_bytes(), 3u64.to_le_bytes()]);
        // The second draw fails every try: nothing left behind.
        let mut calls = 0;
        let mut out = [[0xffu8; 8]; 3];
        let got = fill_words(&mut out, || {
            calls += 1;
            (calls == 1 || calls > 1 + RETRIES as usize).then_some(5)
        });
        assert_eq!(got, Err(DrawFailed));
        assert_eq!(out, [[0u8; 8]; 3]);
        // A failure retried within its 10 tries isn't one.
        let mut calls = 0;
        let mut out = [[0u8; 8]; 2];
        assert_eq!(
            fill_words(&mut out, || {
                calls += 1;
                (calls % 3 == 0).then_some(9)
            }),
            Ok(())
        );
    }

    #[test]
    fn all_same_finds_a_stuck_source() {
        assert!(all_same(&[5u64; 8]));
        assert!(all_same(&[0u8; 32]) && all_same(&[0xabu8; 32]));
        assert!(!all_same(&[1u64, 1, 1, 2]) && !all_same(&[2u8, 1, 1, 1]));
        // One item (or none) proves nothing.
        assert!(!all_same(&[0u8]) && !all_same::<u8>(&[]));
    }

    #[test]
    fn the_zone_is_two_to_the_64_mod_the_bound() {
        assert_eq!(zone_start(1), 0);
        assert_eq!(zone_start(2), 0);
        assert_eq!(zone_start(3), 1);
        assert_eq!(zone_start(6), 4);
        assert_eq!(zone_start(1 << 63), 0);
        assert_eq!(zone_start((1 << 63) + 1), (1 << 63) - 1);
        assert_eq!(zone_start(u64::MAX), 1);
    }

    fn below_from(bound: u64, values: &[u64]) -> (u64, usize) {
        let mut it = values.iter();
        let mut used = 0;
        let v = below::<()>(bound, || {
            used += 1;
            Ok(*it.next().expect("ran out of values"))
        })
        .unwrap();
        (v, used)
    }

    #[test]
    fn below_redraws_values_before_the_zone() {
        // 0 is before bound 3's zone (it would make 0 likelier): drawn again.
        assert_eq!(below_from(3, &[0, 5]), (2, 2));
        assert_eq!(below_from(3, &[1]), (1, 1));
        assert_eq!(below_from(3, &[u64::MAX]), (0, 1));
        let b = (1 << 63) + 1;
        assert_eq!(below_from(b, &[0, (1 << 63) - 2, (1 << 63) - 1]), ((1 << 63) - 1, 3));
        assert_eq!(below_from(b, &[u64::MAX]), ((1 << 63) - 2, 1));
        // Bounds dividing 2^64 take every value.
        assert_eq!(below_from(1, &[0]), (0, 1));
        assert_eq!(below_from(1 << 63, &[0]), (0, 1));
        assert_eq!(below_from(2, &[u64::MAX]), (1, 1));
        // An error from the source is passed on.
        assert_eq!(below(3, || Err::<u64, _>("dry")), Err("dry"));
    }

    #[test]
    #[should_panic(expected = "below(0)")]
    fn below_zero_panics() {
        let _ = below::<()>(0, || Ok(1));
    }

    #[test]
    fn the_keystream_is_balanced() {
        // 1 MiB: every byte value appears close to 1/256 of the time (a
        // chi-square well under the 0.1% critical value for 255 degrees
        // of freedom, 330.5), and the bits are half ones within 0.1%.
        let mut s = pool_of(&[b"a seed for the balance test, 32+"]).stream().unwrap();
        let mut data = vec![0u8; 1 << 20];
        s.fill(&mut data);
        let mut counts = [0u64; 256];
        for &b in &data {
            counts[usize::from(b)] += 1;
        }
        let expected = data.len() as f64 / 256.0;
        let chi2: f64 = counts.iter().map(|&c| (c as f64 - expected).powi(2) / expected).sum();
        assert!(chi2 < 330.5, "chi-square {chi2}");
        let ones: u64 = data.iter().map(|b| u64::from(b.count_ones())).sum();
        let bits = (data.len() * 8) as f64;
        assert!((ones as f64 / bits - 0.5).abs() < 0.001, "{ones} ones in {bits} bits");
    }
}
