//! Hand-assembled WASM modules for wasmi's checks (the `wasm` command and
//! the ktests in `app`), written out opcode by opcode so they need no wasm
//! toolchain (#197 lands before the `apps/` workspace builds Rust modules,
//! #196). Each section is `id, size, contents`; sizes and counts are
//! LEB128 (all here fit in one byte, but for `large_body`'s).

use alloc::vec::Vec;

/// `run(n) -> acc`: `acc = acc * 31 + i` for `i` in `0..n` (wrapping, at
/// least once), 13 instructions (14 fuel) an iteration; `depth(n) -> n`,
/// by recursing `n` deep, so too large an `n` exhausts wasmi's call stack.
#[rustfmt::skip]
pub const LOOP: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // "\0asm", version 1
    // Type section: one type, (i32) -> i32.
    0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f,
    // Function section: two functions, both of type 0.
    0x03, 0x03, 0x02, 0x00, 0x00,
    // Export section: "run" is function 0, "depth" function 1.
    0x07, 0x0f, 0x02,
    0x03, b'r', b'u', b'n', 0x00, 0x00,
    0x05, b'd', b'e', b'p', b't', b'h', 0x00, 0x01,
    // Code section: two bodies.
    0x0a, 0x37, 0x02,
    // run: 31 bytes; locals: one group of two i32s ($i = 1, $acc = 2).
    0x1f, 0x01, 0x02, 0x7f,
    0x03, 0x40,       // loop (no result)
    0x20, 0x02,       //   local.get $acc
    0x41, 0x1f,       //   i32.const 31
    0x6c,             //   i32.mul
    0x20, 0x01,       //   local.get $i
    0x6a,             //   i32.add
    0x21, 0x02,       //   local.set $acc
    0x20, 0x01,       //   local.get $i
    0x41, 0x01,       //   i32.const 1
    0x6a,             //   i32.add
    0x22, 0x01,       //   local.tee $i
    0x20, 0x00,       //   local.get $n
    0x49,             //   i32.lt_u
    0x0d, 0x00,       //   br_if 0 (back to the loop)
    0x0b,             // end (loop)
    0x20, 0x02,       // local.get $acc
    0x0b,             // end (function)
    // depth: 21 bytes; no locals.
    0x15, 0x00,
    0x20, 0x00,       // local.get $n
    0x45,             // i32.eqz
    0x04, 0x7f,       // if (result i32)
    0x41, 0x00,       //   i32.const 0
    0x05,             // else
    0x20, 0x00,       //   local.get $n
    0x41, 0x01,       //   i32.const 1
    0x6b,             //   i32.sub
    0x10, 0x01,       //   call 1 (itself)
    0x41, 0x01,       //   i32.const 1
    0x6a,             //   i32.add
    0x0b,             // end (if)
    0x0b,             // end (function)
];

/// One float operation per export, each a single instruction on its
/// parameters, so the host's interpretation (soft-float in the kernel) is
/// all that decides the result: `add`, `div`, `min`, `mul` (f64, f64) ->
/// f64; `sqrt`, `nearest` (f64) -> f64; `f32_div`, `f32_add` (f32, f32)
/// -> f32; `u64_to_f64` (i64) -> f64 (`f64.convert_i64_u`); `trunc_sat`
/// (f64) -> i32 (`i32.trunc_sat_f64_s`, nontrapping-fptoint, which rustc
/// enables by default for `wasm32-unknown-unknown`); `lt`, `eq` (f64,
/// f64) -> i32; `demote` (f64) -> f32 (`f32.demote_f64`).
#[rustfmt::skip]
pub const FLOATS: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // "\0asm", version 1
    // Type section: seven types.
    0x01, 0x27, 0x07,
    0x60, 0x02, 0x7c, 0x7c, 0x01, 0x7c, // 0: (f64, f64) -> f64
    0x60, 0x01, 0x7c, 0x01, 0x7c,       // 1: (f64) -> f64
    0x60, 0x02, 0x7d, 0x7d, 0x01, 0x7d, // 2: (f32, f32) -> f32
    0x60, 0x01, 0x7e, 0x01, 0x7c,       // 3: (i64) -> f64
    0x60, 0x01, 0x7c, 0x01, 0x7f,       // 4: (f64) -> i32
    0x60, 0x02, 0x7c, 0x7c, 0x01, 0x7f, // 5: (f64, f64) -> i32
    0x60, 0x01, 0x7c, 0x01, 0x7d,       // 6: (f64) -> f32
    // Function section: thirteen functions and their types.
    0x03, 0x0e, 0x0d, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x03, 0x04, 0x00, 0x02, 0x05, 0x05, 0x06,
    // Export section: thirteen functions, 0 to 12.
    0x07, 0x6a, 0x0d,
    0x03, b'a', b'd', b'd', 0x00, 0x00,
    0x03, b'd', b'i', b'v', 0x00, 0x01,
    0x03, b'm', b'i', b'n', 0x00, 0x02,
    0x04, b's', b'q', b'r', b't', 0x00, 0x03,
    0x07, b'n', b'e', b'a', b'r', b'e', b's', b't', 0x00, 0x04,
    0x07, b'f', b'3', b'2', b'_', b'd', b'i', b'v', 0x00, 0x05,
    0x0a, b'u', b'6', b'4', b'_', b't', b'o', b'_', b'f', b'6', b'4', 0x00, 0x06,
    0x09, b't', b'r', b'u', b'n', b'c', b'_', b's', b'a', b't', 0x00, 0x07,
    0x03, b'm', b'u', b'l', 0x00, 0x08,
    0x07, b'f', b'3', b'2', b'_', b'a', b'd', b'd', 0x00, 0x09,
    0x02, b'l', b't', 0x00, 0x0a,
    0x02, b'e', b'q', 0x00, 0x0b,
    0x06, b'd', b'e', b'm', b'o', b't', b'e', 0x00, 0x0c,
    // Code section: thirteen bodies, each `size, 0 locals, code, end`.
    0x0a, 0x60, 0x0d,
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0xa0, 0x0b, // add: local.get 0, local.get 1, f64.add
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0xa3, 0x0b, // div: ..., f64.div
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0xa4, 0x0b, // min: ..., f64.min
    0x05, 0x00, 0x20, 0x00, 0x9f, 0x0b,             // sqrt: local.get 0, f64.sqrt
    0x05, 0x00, 0x20, 0x00, 0x9e, 0x0b,             // nearest: local.get 0, f64.nearest
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x95, 0x0b, // f32_div: ..., f32.div
    0x05, 0x00, 0x20, 0x00, 0xba, 0x0b,             // u64_to_f64: local.get 0, f64.convert_i64_u
    0x06, 0x00, 0x20, 0x00, 0xfc, 0x02, 0x0b,       // trunc_sat: local.get 0, i32.trunc_sat_f64_s
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0xa2, 0x0b, // mul: ..., f64.mul
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x92, 0x0b, // f32_add: ..., f32.add
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x63, 0x0b, // lt: ..., f64.lt
    0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x61, 0x0b, // eq: ..., f64.eq
    0x05, 0x00, 0x20, 0x00, 0xb6, 0x0b,             // demote: local.get 0, f32.demote_f64
];

/// `fill(n) -> byte`: `memory.fill` of the first `n` bytes of its two pages
/// (128 KiB) with 0xab, then the last of them read back. wasmi charges a
/// bulk operation by the bytes it touches (1 fuel per 64, rounded down),
/// all before the instruction runs, so a large `n` needs more fuel than a
/// small slice.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
#[rustfmt::skip]
pub const FILL: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // "\0asm", version 1
    // Type section: one type, (i32) -> i32.
    0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f,
    // Function section: one function, of type 0.
    0x03, 0x02, 0x01, 0x00,
    // Memory section: one memory, no maximum, 2 pages.
    0x05, 0x03, 0x01, 0x00, 0x02,
    // Export section: "fill" is function 0.
    0x07, 0x08, 0x01, 0x04, b'f', b'i', b'l', b'l', 0x00, 0x00,
    // Code section: one body of 20 bytes, no locals.
    0x0a, 0x16, 0x01, 0x14, 0x00,
    0x41, 0x00,       // i32.const 0 (where)
    0x41, 0xab, 0x01, // i32.const 0xab (the byte; signed LEB128)
    0x20, 0x00,       // local.get $n (how many)
    0xfc, 0x0b, 0x00, // memory.fill (memory 0)
    0x20, 0x00,       // local.get $n
    0x41, 0x01,       // i32.const 1
    0x6b,             // i32.sub
    0x2d, 0x00, 0x00, // i32.load8_u (align 1, offset 0)
    0x0b,             // end (function)
];

/// A module whose one export, `add(x) -> x + adds`, is `adds` repetitions
/// of `i32.const 1, i32.add`: a body of `3 * adds + 4` bytes. wasmi's lazy
/// compilation charges a body's bytes (7 fuel each) to the fuel left in
/// the slice of its first call; compiled eagerly, a body costs only its
/// instructions.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub fn large_body(adds: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x20, 0x00]); // no locals; local.get 0
    for _ in 0..adds {
        body.extend_from_slice(&[0x41, 0x01, 0x6a]); // i32.const 1; i32.add
    }
    body.push(0x0b); // end (function)
    let mut code = Vec::new();
    code.push(0x01); // one body
    leb128(&mut code, body.len() as u32);
    code.extend_from_slice(&body);

    let mut m = Vec::new();
    m.extend_from_slice(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
    m.extend_from_slice(&[0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f]); // (i32) -> i32
    m.extend_from_slice(&[0x03, 0x02, 0x01, 0x00]); // one function, type 0
    m.extend_from_slice(&[0x07, 0x07, 0x01, 0x03, b'a', b'd', b'd', 0x00, 0x00]); // "add"
    m.push(0x0a); // code section
    leb128(&mut m, code.len() as u32);
    m.extend_from_slice(&code);
    m
}

/// `v` as unsigned LEB128.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub(super) fn leb128(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}
