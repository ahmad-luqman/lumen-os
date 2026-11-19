//! `run`'s arguments as an app reads them (`args_len`, `args_read`): the
//! words after the app's name, each followed by a NUL, at most
//! [`ARGS_BYTES`] in all.

use crate::limits::ARGS_BYTES;
use alloc::vec::Vec;

/// Why arguments can't be handed to an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgsError {
    /// Past [`ARGS_BYTES`].
    TooLong,
    /// A word holds a NUL, which would split it.
    Nul,
}

/// The arguments' bytes.
pub fn encode<'a>(words: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>, ArgsError> {
    let mut out = Vec::new();
    for w in words {
        if w.as_bytes().contains(&0) {
            return Err(ArgsError::Nul);
        }
        if out.len() + w.len() + 1 > ARGS_BYTES {
            return Err(ArgsError::TooLong);
        }
        out.extend_from_slice(w.as_bytes());
        out.push(0);
    }
    Ok(out)
}

/// The words in arguments' bytes (as an app splits them).
pub fn words(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == 0).take(bytes.iter().filter(|&&b| b == 0).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        let bytes = encode(["sleep", "", "x y"]).unwrap();
        assert_eq!(bytes, b"sleep\0\0x y\0");
        assert_eq!(words(&bytes).collect::<Vec<_>>(), [&b"sleep"[..], b"", b"x y"]);
        assert_eq!(encode([]).unwrap(), b"");
        assert_eq!(words(b"").count(), 0);
    }

    #[test]
    fn too_long_or_holding_a_nul_is_refused() {
        let fits = "a".repeat(ARGS_BYTES - 1);
        assert_eq!(encode([fits.as_str()]).unwrap().len(), ARGS_BYTES);
        let past = "a".repeat(ARGS_BYTES);
        assert_eq!(encode([past.as_str()]), Err(ArgsError::TooLong));
        assert_eq!(encode(["a\0b"]), Err(ArgsError::Nul));
    }
}
