//! Base64 (RFC 4648 section 4: the standard alphabet, `=` padding), the
//! encoding direction only: the screenshot's frame carries its PNG in it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The length of `bytes` bytes encoded, padding included.
pub const fn encoded_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Encode `bytes` into the start of `out`; returns how many characters
/// that took ([`encoded_len`]). Panics if `out` is shorter than that.
pub fn encode_to_slice(bytes: &[u8], out: &mut [u8]) -> usize {
    let len = encoded_len(bytes.len());
    assert!(out.len() >= len, "{} bytes encode to {len} characters, room for {}", bytes.len(), out.len());
    let sextet = |v: u32, shift: u32| ALPHABET[((v >> shift) & 0x3f) as usize];
    for (group, chars) in bytes.chunks(3).zip(out.as_chunks_mut::<4>().0) {
        let v = group.iter().enumerate().fold(0u32, |v, (i, &b)| v | u32::from(b) << (16 - 8 * i));
        chars[0] = sextet(v, 18);
        chars[1] = sextet(v, 12);
        chars[2] = if group.len() > 1 { sextet(v, 6) } else { b'=' };
        chars[3] = if group.len() > 2 { sextet(v, 0) } else { b'=' };
    }
    len
}
