//! postcard over serde-derived types, linked for the app interface's wire
//! format (ARCHITECTURE section 8). The types apps exchange are
//! `lumen-app-api`'s (#199); this ktest only proves the pair builds and
//! runs in the kernel (soft-float target, kernel rustflags) and pins the
//! encoding postcard gives a struct and an enum, so the flag audit covers
//! both crates in the ktest kernel. The booted kernel links them from #199.

use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
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

crate::ktest!(
    fn postcard_round_trips_derived_types() {
        // Each value, and the bytes postcard writes for it: integers as
        // LEB128 varints (signed ones zigzagged), a string as its length
        // then its bytes, an enum as its variant's position then its fields.
        let sample = Sample { id: 300, name: String::from("hi"), flags: 7 };
        let shapes: [(Shape, &[u8]); 3] = [
            (Shape::Unit, &[0x00]),
            (Shape::Point(-1, 2), &[0x01, 0x01, 0x04]),
            (Shape::Named { w: 128 }, &[0x02, 0x80, 0x01]),
        ];

        let bytes: Vec<u8> = postcard::to_allocvec(black_box(&sample)).map_err(|_| "encode")?;
        crate::kassert_eq!(bytes.as_slice(), &[0xac, 0x02, 0x02, b'h', b'i', 0x07][..]);
        let back: Sample = postcard::from_bytes(black_box(&bytes)).map_err(|_| "decode")?;
        crate::kassert_eq!(back, sample);

        for (shape, want) in shapes {
            let bytes: Vec<u8> = postcard::to_allocvec(black_box(&shape)).map_err(|_| "encode")?;
            crate::kassert_eq!(bytes.as_slice(), want);
            let back: Shape = postcard::from_bytes(black_box(&bytes)).map_err(|_| "decode")?;
            crate::kassert_eq!(back, shape);
        }

        // Cut short or naming no variant: an error, never a panic.
        crate::kassert!(postcard::from_bytes::<Sample>(&[0xac, 0x02, 0x02, b'h']).is_err());
        crate::kassert!(postcard::from_bytes::<Shape>(&[0x03]).is_err());
        Ok(())
    }
);
