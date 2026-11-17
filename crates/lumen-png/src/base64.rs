//! Base64 (RFC 4648 section 4: the standard alphabet, `=` padding), the
//! encoding direction only: the screenshot's frame carries its PNG in it.

use alloc::string::String;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The length of `bytes` bytes encoded, padding included.
pub const fn encoded_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Append `bytes`, encoded, to `out`.
pub fn encode_into(bytes: &[u8], out: &mut String) {
    out.reserve(encoded_len(bytes.len()));
    let sextet = |v: u32, shift: u32| char::from(ALPHABET[((v >> shift) & 0x3f) as usize]);
    for group in bytes.chunks(3) {
        let v = group.iter().enumerate().fold(0u32, |v, (i, &b)| v | u32::from(b) << (16 - 8 * i));
        out.push(sextet(v, 18));
        out.push(sextet(v, 12));
        out.push(if group.len() > 1 { sextet(v, 6) } else { '=' });
        out.push(if group.len() > 2 { sextet(v, 0) } else { '=' });
    }
}
