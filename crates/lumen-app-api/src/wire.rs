//! What every wire type (ARCHITECTURE section 8, "Wire formats") shares:
//! [`wire_enum!`] for a field-less enum's numbered table, [`decode_exact`],
//! the one decoder (bytes left over refused), [`Capped`], a string held to
//! its cap however it is made, and [`CappedVec`], a sequence whose length
//! is refused past its cap before anything is allocated for it (section
//! 8's rule for every wire `Vec`). [`crate::event`], [`crate::tree`] and
//! [`crate::answer`] use them.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;
use serde::de::{DeserializeOwned, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A field-less wire enum from one list: explicit discriminants, each
/// checked against its declaration position at compile time (serde encodes
/// the position), `ALL` in that order and `name`.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident = $num:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
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

/// Text past a [`Capped`]'s cap: refused (wrong arguments) before it
/// becomes a wire value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong;

/// A string of at most `N` bytes, whichever way it is made
/// ([`Capped::try_from`], or decoded: a longer one is refused, postcard's
/// `SerdeDeCustom`). On the wire it is a plain string (postcard checks the
/// bytes left before allocating one, so decoding never allocates past the
/// input).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Capped<const N: usize>(String);

impl<const N: usize> Capped<N> {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `text` cut to the cap on a char boundary (never refused).
    pub fn cut(text: &str) -> Capped<N> {
        Capped(String::from(crate::text::prefix(text, N)))
    }
}

impl<const N: usize> TryFrom<&str> for Capped<N> {
    type Error = TooLong;

    fn try_from(text: &str) -> Result<Capped<N>, TooLong> {
        if text.len() > N { Err(TooLong) } else { Ok(Capped(String::from(text))) }
    }
}

impl<'de, const N: usize> Deserialize<'de> for Capped<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Capped<N>, D::Error> {
        let text = String::deserialize(d)?;
        if text.len() > N {
            return Err(serde::de::Error::custom("a text past its cap"));
        }
        Ok(Capped(text))
    }
}

/// A sequence of at most `N` elements. Decoded through a visitor that
/// refuses a length past `N` before allocating, preallocates at most
/// `min(hint, N)` (postcard gives a hint only for a length within the bytes
/// left), and refuses an element past `N`; a derived `Vec` would
/// preallocate up to 1 MiB of elements first (section 8, "Sequences").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CappedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> CappedVec<T, N> {
    /// `items`, or `None` past the cap.
    pub fn new(items: Vec<T>) -> Option<CappedVec<T, N>> {
        (items.len() <= N).then_some(CappedVec(items))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: Serialize, const N: usize> Serialize for CappedVec<T, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for CappedVec<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<CappedVec<T, N>, D::Error> {
        struct Capping<T, const N: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Capping<T, N> {
            type Value = CappedVec<T, N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a sequence of at most {N} elements")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<CappedVec<T, N>, A::Error> {
                let hint = seq.size_hint().unwrap_or(0);
                if hint > N {
                    return Err(serde::de::Error::custom("a sequence past its cap"));
                }
                let mut out = Vec::with_capacity(hint.min(N));
                while let Some(item) = seq.next_element()? {
                    if out.len() == N {
                        return Err(serde::de::Error::custom("a sequence past its cap"));
                    }
                    out.push(item);
                }
                Ok(CappedVec(out))
            }
        }

        d.deserialize_seq(Capping(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_capped_text_is_held_to_its_cap_however_it_is_made() {
        assert_eq!(Capped::<3>::try_from("abc").map(|c| String::from(c.as_str())), Ok(String::from("abc")));
        assert_eq!(Capped::<3>::try_from("abcd"), Err(TooLong));
        assert_eq!(Capped::<3>::cut("abcd").as_str(), "abc");
        assert_eq!(Capped::<3>::cut("aé€").as_str(), "aé", "cut on a char boundary");
        let at: Capped<3> = decode_exact(&[3, b'a', b'b', b'c']).unwrap();
        assert_eq!(at.as_str(), "abc");
        assert_eq!(
            decode_exact::<Capped<3>>(&[4, b'a', b'b', b'c', b'd']),
            Err(DecodeError::Postcard(postcard::Error::SerdeDeCustom))
        );
        assert_eq!(postcard::to_allocvec(&at).unwrap(), [3, b'a', b'b', b'c']);
    }

    #[test]
    fn a_capped_sequence_is_refused_past_its_cap() {
        let at: CappedVec<u8, 2> = decode_exact(&[2, 7, 8]).unwrap();
        assert_eq!(at.as_slice(), &[7, 8]);
        assert_eq!(postcard::to_allocvec(&at).unwrap(), [2, 7, 8]);
        // One past with the bytes to back it (a hint): the cap's error.
        assert_eq!(
            decode_exact::<CappedVec<u8, 2>>(&[3, 1, 2, 3]),
            Err(DecodeError::Postcard(postcard::Error::SerdeDeCustom))
        );
        // A huge claim without them: the end of the input, nothing reserved.
        assert_eq!(
            decode_exact::<CappedVec<u8, 2>>(&[0xff, 0xff, 0xff, 0xff, 0x0f, 1]),
            Err(DecodeError::Postcard(postcard::Error::DeserializeUnexpectedEnd))
        );
        assert!(CappedVec::<u8, 2>::new(vec![1, 2, 3]).is_none());
        assert_eq!(CappedVec::<u8, 2>::new(vec![1]).map(|v| v.len()), Some(1));
    }
}
