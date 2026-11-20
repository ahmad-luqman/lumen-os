//! What every wire type (ARCHITECTURE section 8, "Wire formats") shares:
//! [`wire_enum!`] for a field-less enum's numbered table, and
//! [`decode_exact`], the one decoder (bytes left over refused). #206's
//! [`crate::event`] uses both; #207's tree and answer types are to as well.

use serde::de::DeserializeOwned;

/// A field-less wire enum from one list: explicit discriminants, each
/// checked against its declaration position at compile time (serde encodes
/// the position), `ALL` in that order and `name`.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident = $num:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[repr(u8)]
        $vis enum $name {
            $($variant = $num,)*
        }

        impl $name {
            /// Every variant, in its numbered (declaration) order.
            pub const ALL: [$name; [$($num),*].len()] = [$($name::$variant),*];

            /// Its name (`F1`).
            pub const fn name(self) -> &'static str {
                match self {
                    $($name::$variant => stringify!($variant),)*
                }
            }
        }

        const _: () = {
            let mut i = 0;
            while i < $name::ALL.len() {
                assert!($name::ALL[i] as usize == i, "a wire enum's discriminant must be its position");
                i += 1;
            }
        };
    };
}

/// Why bytes aren't a value of a wire type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// postcard's refusal.
    Postcard(postcard::Error),
    /// A whole value, then this many bytes more.
    LeftOver(usize),
}

/// The value `bytes` hold, exactly: postcard's refusal, or bytes left over
/// after a whole value (`postcard::from_bytes` would accept them, so this
/// takes the value, then checks the remainder).
pub fn decode_exact<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((value, [])) => Ok(value),
        Ok((_, rest)) => Err(DecodeError::LeftOver(rest.len())),
        Err(e) => Err(DecodeError::Postcard(e)),
    }
}
