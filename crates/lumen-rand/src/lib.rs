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
//! - Each use of the block function gets its own nonce (a domain word, then
//!   a 64-bit counter), so absorbing, splitting and streaming never share a
//!   keystream.
//! - The pool counts the bytes it was given (each is credited as one byte of
//!   entropy: the kernel only mixes in its sources' output) and refuses to
//!   hand out a stream until it has [`SEED_BYTES`].
//!
//! Not audited, not constant-time-reviewed: a hobby OS's randomness. The
//! code is safe Rust; keys are cleared on drop on a best-effort basis
//! (nothing stops an earlier copy surviving on a stack).

#![cfg_attr(not(test), no_std)]

/// The key length (bytes).
pub const KEY_LEN: usize = 32;
/// The block length (bytes).
pub const BLOCK_LEN: usize = 64;
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

fn word(bytes: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]])
}

/// ChaCha20's block function (RFC 8439 section 2.3): 20 rounds over the
/// constants, `key`, `counter` and `nonce`, plus the input, serialized
/// little-endian.
pub fn block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; 12]) -> [u8; BLOCK_LEN] {
    let mut input = [0u32; 16];
    input[..4].copy_from_slice(&SIGMA);
    for i in 0..8 {
        input[4 + i] = word(key, i);
    }
    input[12] = counter;
    for i in 0..3 {
        input[13 + i] = word(nonce, i);
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

/// The first or second half of a block, as a key.
fn half(b: &[u8; BLOCK_LEN], second: bool) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    let at = if second { KEY_LEN } else { 0 };
    key.copy_from_slice(&b[at..at + KEY_LEN]);
    key
}

/// Clear a buffer, best effort (the compiler may not drop the stores).
fn clear(bytes: &mut [u8]) {
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
            self.key = half(&b, false);
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
        self.key = half(&b, false);
        let stream = Stream::new(half(&b, true));
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
    /// The keystream under `key`.
    pub fn new(key: [u8; KEY_LEN]) -> Stream {
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

    /// The next 8 bytes of the keystream as a number (little-endian).
    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        clear(&mut self.key);
        clear(&mut self.buf);
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
        let mut s = Stream::new(k);
        let a = s.next_u64();
        assert_eq!(a.to_le_bytes(), whole[..8]);
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
