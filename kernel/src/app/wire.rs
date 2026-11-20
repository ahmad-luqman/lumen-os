//! postcard over serde-derived types: the crates of the app interface's
//! wire format (ARCHITECTURE section 8). The types apps exchange are
//! `lumen-app-api`'s (`event`, `wire`; host-tested there); this ktest pins
//! the encoding postcard gives a
//! struct and enums, and the decoding rules section 8 relies on: leftover
//! bytes noticed, a cut anywhere an error, an enum numbered by declaration
//! order (not by its discriminants).
//!
//! It also links both crates into the ktest kernel, so `make audit`'s
//! binary checks (no SSE, no red zone, the kernel code model) cover their
//! code there. Its IR checks read only the kernel crate's own IR, built
//! without `ktest`: they see these crates' generic code compiled into the
//! kernel (the booted kernel encodes events with them), while their own
//! non-generic code stays covered by the binary checks alone, as wasmi's
//! and rhai's are.

use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
use postcard::Error;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Sample {
    id: u64,
    name: String,
    flags: u8,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Shape {
    Unit,
    Point(i32, i32),
    Named { w: u32 },
}

/// Discriminants that differ from declaration order: serde ignores them.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
enum Tagged {
    A = 5,
    B = 2,
}

/// Encode `value`, printing postcard's error as a TAP diagnostic if it
/// refuses.
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, &'static str> {
    // black_box: the value comes from outside as far as LLVM knows, so the
    // encoder really runs instead of folding to constant bytes.
    postcard::to_allocvec(black_box(value)).map_err(|e| {
        crate::ktest::diag(format_args!("encode: {e:?}"));
        "postcard refused to encode"
    })
}

/// Decode a `T` from the front of `bytes` and count what is left over:
/// `postcard::from_bytes` ignores trailing bytes, and section 8 refuses
/// them, so callers decode this way.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<(T, usize), Error> {
    // black_box: the bytes come from outside as far as LLVM knows, so the
    // decoder really runs instead of folding to a constant result.
    postcard::take_from_bytes(black_box(bytes)).map(|(value, rest)| (value, rest.len()))
}

/// `value` encodes to exactly `want`, decodes back with nothing left over,
/// and every strict prefix of `want` (a cut anywhere, inside a varint or a
/// string too) is `DeserializeUnexpectedEnd`.
fn round_trips<T>(value: T, want: &[u8]) -> Result<(), &'static str>
where
    T: Serialize + DeserializeOwned + PartialEq + core::fmt::Debug,
{
    let bytes = encode(&value)?;
    crate::kassert_eq!(bytes.as_slice(), want);
    let back = decode::<T>(&bytes).map_err(|e| {
        crate::ktest::diag(format_args!("decode {value:?}: {e:?}"));
        "postcard refused its own encoding"
    })?;
    crate::kassert_eq!(back, (value, 0));
    for cut in 0..bytes.len() {
        let got = decode::<T>(&bytes[..cut]).map(|_| ());
        if got != Err(Error::DeserializeUnexpectedEnd) {
            crate::ktest::diag(format_args!("{bytes:?} cut to {cut} bytes: {got:?}"));
            return Err("a cut encoding wasn't DeserializeUnexpectedEnd");
        }
    }
    Ok(())
}

crate::ktest!(
    fn postcard_round_trips_derived_types() {
        // What postcard writes: integers wider than a byte as LEB128 varints
        // (signed ones zigzagged), a `u8` as its one byte, a string as its
        // length then its bytes, an enum as its variant's position then its
        // fields.
        round_trips(Sample { id: 300, name: String::from("hi"), flags: 200 }, &[0xac, 0x02, 0x02, b'h', b'i', 0xc8])?;
        round_trips(Shape::Unit, &[0x00])?;
        round_trips(Shape::Point(-1, 2), &[0x01, 0x01, 0x04])?;
        round_trips(Shape::Named { w: 128 }, &[0x02, 0x80, 0x01])?;
        // The position, not the `repr(u8)` discriminant (5 and 2).
        round_trips(Tagged::A, &[0x00])?;
        round_trips(Tagged::B, &[0x01])?;

        // Trailing bytes decode, and are left over for the caller to refuse.
        crate::kassert_eq!(decode::<Shape>(&[0x00, 0xff]), Ok((Shape::Unit, 1)));
        // A variant past the last, followed by more bytes so it can't pass
        // as a cut: serde's own refusal.
        crate::kassert_eq!(decode::<Shape>(&[0x03, 0x05]), Err(Error::SerdeDeCustom));
        Ok(())
    }
);
