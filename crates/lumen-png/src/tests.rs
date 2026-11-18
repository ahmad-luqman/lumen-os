extern crate std;

use crate::encode::{Error, MAX_DIMENSION, Png, Rows, SIGNATURE};
use crate::{base64, crc32, frame};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;
use std::path::PathBuf;
use std::string::ToString;

// ---- An allocator that says no: on the test's own thread, any
// allocation (or growth) past `limit` bytes fails, as the kernel's heap
// does when it can't grow. Other test threads are unaffected.

std::thread_local! {
    static LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
}

struct Refusing;

fn allowed(size: usize) -> bool {
    LIMIT.try_with(|l| size <= l.get()).unwrap_or(true)
}

// SAFETY: forwards to `System`, or returns null (an allocation failure,
// which callers must handle) without touching memory.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if allowed(layout.size()) {
            // SAFETY: the caller's contract, passed on.
            unsafe { System.alloc(layout) }
        } else {
            core::ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if allowed(new_size) {
            // SAFETY: the caller's contract, passed on.
            unsafe { System.realloc(ptr, layout, new_size) }
        } else {
            core::ptr::null_mut()
        }
    }
}

#[global_allocator]
static ALLOCATOR: Refusing = Refusing;

/// Run `f` with allocations past `limit` bytes refused on this thread.
fn refusing_past<R>(limit: usize, f: impl FnOnce() -> R) -> R {
    LIMIT.with(|l| l.set(limit));
    let r = f();
    LIMIT.with(|l| l.set(usize::MAX));
    r
}

/// A small deterministic generator (xorshift64) for test images.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---- A decoder for the tests only, independent of the encoder: chunks
// walked with their CRCs checked, IDAT inflated by miniz_oxide's
// decompressor (the harness also decodes the golden with PIL:
// tests/harness/test_screenshot.py).

#[derive(Debug)]
struct Decoded {
    width: u32,
    height: u32,
    chunks: Vec<[u8; 4]>,
    pixels: Vec<u8>,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn parse_encoded(png: &[u8]) -> Decoded {
    assert_eq!(png[..8], SIGNATURE, "signature");
    let mut at = 8;
    let (mut chunks, mut ihdr, mut idat) = (Vec::new(), None, Vec::new());
    while at < png.len() {
        let len = be32(&png[at..]) as usize;
        let kind: [u8; 4] = png[at + 4..at + 8].try_into().unwrap();
        let data = &png[at + 8..at + 8 + len];
        let crc = be32(&png[at + 8 + len..]);
        assert_eq!(crc, crc32(&png[at + 4..at + 8 + len]), "CRC of {kind:?}");
        match &kind {
            b"IHDR" => ihdr = Some(data.to_vec()),
            b"IDAT" => idat.extend_from_slice(data),
            _ => {}
        }
        chunks.push(kind);
        at += 12 + len;
    }
    assert_eq!(at, png.len(), "chunks end at the file's end");
    let ihdr = ihdr.expect("IHDR");
    assert_eq!(ihdr.len(), 13);
    let (width, height) = (be32(&ihdr[0..]), be32(&ihdr[4..]));
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).expect("IDAT inflates");
    let stride = width as usize * 3 + 1;
    assert_eq!(raw.len(), stride * height as usize, "IDAT holds exactly the rows");
    let mut pixels = Vec::new();
    for row in raw.chunks_exact(stride) {
        assert_eq!(row[0], 0, "every row unfiltered");
        pixels.extend_from_slice(&row[1..]);
    }
    Decoded { width, height, chunks, pixels }
}

/// Every pixel's bytes, row after row.
fn all_pixels(rows: &Rows) -> Vec<u8> {
    (0..rows.height()).flat_map(|y| rows.row(y).as_flattened().to_vec()).collect()
}

fn random_rows(rng: &mut Rng, width: u32, height: u32) -> Rows {
    let mut rows = Rows::new(width, height).unwrap();
    for y in 0..height {
        for b in rows.row_mut(y).as_flattened_mut() {
            // Few distinct values, so runs and repeats occur (as on a screen).
            *b = [0x0f, 0x14, 0x20, 0xe6, 0xff][rng.below(5) as usize];
        }
    }
    rows
}

/// Noise: compresses to about its own size.
fn noise_rows(width: u32, height: u32) -> Rows {
    let mut rng = Rng(7);
    let mut rows = Rows::new(width, height).unwrap();
    for y in 0..height {
        for b in rows.row_mut(y).as_flattened_mut() {
            *b = rng.next() as u8;
        }
    }
    rows
}

// ---- CRC-32

#[test]
fn crc32_matches_the_standard_check_values() {
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
    // The CRC every PNG's IEND chunk carries (of its type alone).
    assert_eq!(crc32(b"IEND"), 0xae42_6082);
}

// ---- base64

/// Decoding, for the round trips only.
fn base64_decode(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    assert_eq!(s.len() % 4, 0, "whole groups");
    let mut out = Vec::new();
    for group in s.as_bytes().chunks(4) {
        let pad = group.iter().filter(|&&c| c == b'=').count();
        let v = group.iter().fold(0u32, |v, &c| {
            let d = if c == b'=' { 0 } else { A.iter().position(|&a| a == c).expect("alphabet") as u32 };
            v << 6 | d
        });
        out.extend_from_slice(&v.to_be_bytes()[1..4 - pad]);
    }
    out
}

fn base64_string(bytes: &[u8]) -> String {
    let mut out = vec![b'#'; base64::encoded_len(bytes.len()) + 2];
    let n = base64::encode_to_slice(bytes, &mut out);
    assert_eq!(n, base64::encoded_len(bytes.len()));
    // Nothing written past what it returned.
    assert_eq!(out[n..], *b"##");
    String::from_utf8(out[..n].to_vec()).unwrap()
}

#[test]
fn base64_matches_rfc_4648_vectors() {
    for (plain, coded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64_string(plain.as_bytes()), coded, "{plain:?}");
    }
}

#[test]
fn base64_round_trips_every_byte_value_at_every_alignment() {
    let all: Vec<u8> = (0..=255).collect();
    for start in 0..3 {
        let out = base64_string(&all[start..]);
        assert_eq!(base64_decode(&out), all[start..]);
        // The 62nd and 63rd characters both show up.
        assert!(out.contains('+') && out.contains('/'), "{out}");
    }
}

#[test]
#[should_panic(expected = "4 bytes encode to 8 characters, room for 7")]
fn base64_into_too_small_a_slice_panics() {
    base64::encode_to_slice(b"abcd", &mut [0; 7]);
}

// ---- Rows

#[test]
fn new_refuses_empty_and_oversized_images() {
    assert_eq!(Rows::new(0, 10).unwrap_err(), Error::Empty);
    assert_eq!(Rows::new(10, 0).unwrap_err(), Error::Empty);
    assert_eq!(Rows::new(MAX_DIMENSION + 1, 1).unwrap_err(), Error::TooLarge);
    assert_eq!(Rows::new(1, MAX_DIMENSION + 1).unwrap_err(), Error::TooLarge);
    // Both dimensions at PNG's limit: past isize::MAX bytes.
    assert_eq!(Rows::new(MAX_DIMENSION, MAX_DIMENSION).unwrap_err(), Error::TooLarge);
}

#[test]
fn new_says_no_when_the_allocator_does() {
    // 1280x800: 3 MiB of rows, refused past 1 MiB.
    let len = (1280 * 3 + 1) * 800;
    assert_eq!(refusing_past(1 << 20, || Rows::new(1280, 800).unwrap_err()), Error::OutOfMemory(len));
    assert_eq!(Error::OutOfMemory(len).to_string(), std::format!("no memory for a buffer of {len} bytes"));
    // Within the address space but past what any allocator gives.
    let (w, h) = (MAX_DIMENSION, 1 << 20);
    assert_eq!(Rows::new(w, h).unwrap_err(), Error::OutOfMemory((w as usize * 3 + 1) * h as usize));
}

#[test]
fn rows_start_black_and_keep_what_is_written() {
    let mut rows = Rows::new(3, 2).unwrap();
    assert_eq!((rows.width(), rows.height()), (3, 2));
    assert_eq!(all_pixels(&rows), [0; 18]);
    assert_eq!(rows.row_mut(1).len(), 3);
    rows.row_mut(1).copy_from_slice(&[[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
    rows.row_mut(0)[0] = [42, 0, 0];
    assert_eq!(rows.row(1), [[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
    assert_eq!(rows.row(0), [[42, 0, 0], [0, 0, 0], [0, 0, 0]]);
    assert_eq!(std::format!("{rows:?}"), "Rows(3x2)");
}

#[test]
#[should_panic(expected = "row 2 of a 2-row image")]
fn row_mut_past_the_last_row_panics() {
    let mut rows = Rows::new(3, 2).unwrap();
    let _ = rows.row_mut(2);
}

#[test]
#[should_panic(expected = "row 5 of a 2-row image")]
fn row_past_the_last_row_panics() {
    let _ = Rows::new(3, 2).unwrap().row(5);
}

// ---- The file

#[test]
fn encode_writes_signature_ihdr_idat_iend() {
    let mut rng = Rng(0x1234_5678);
    let png = random_rows(&mut rng, 1280, 3).encode().unwrap();
    assert_eq!((png.width(), png.height()), (1280, 3));
    let png = png.bytes();
    assert_eq!(png[..8], SIGNATURE);
    // IHDR first, at a fixed place: length 13, type, width, height, then
    // depth 8, colour type 2 (RGB), compression 0, filter method 0, no
    // interlace.
    assert_eq!(png[8..16], [0, 0, 0, 13, b'I', b'H', b'D', b'R']);
    assert_eq!(be32(&png[16..]), 1280);
    assert_eq!(be32(&png[20..]), 3);
    assert_eq!(png[24..29], [8, 2, 0, 0, 0]);
    assert_eq!(be32(&png[29..]), crc32(&png[12..29]));
    // IDAT next, its length the data's.
    assert_eq!(png[37..41], *b"IDAT");
    assert_eq!(be32(&png[33..]) as usize, png.len() - 41 - 4 - 12);
    // IEND last, exactly as every PNG ends.
    assert_eq!(png[png.len() - 12..], [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
    let d = parse_encoded(png);
    assert_eq!(d.chunks, [*b"IHDR", *b"IDAT", *b"IEND"]);
}

#[test]
fn images_of_every_shape_round_trip() {
    let mut rng = Rng(0xdead_beef);
    for (w, h) in [(1, 1), (1, 7), (7, 1), (2, 2), (33, 17), (300, 5)] {
        let rows = random_rows(&mut rng, w, h);
        let pixels = all_pixels(&rows);
        let d = parse_encoded(rows.encode().unwrap().bytes());
        assert_eq!((d.width, d.height), (w, h));
        assert_eq!(d.pixels, pixels, "{w}x{h}");
    }
}

#[test]
fn noise_bigger_than_the_first_buffer_round_trips() {
    // Incompressible: the file outgrows its first buffer (a sixteenth of
    // the rows) several times over.
    let rows = noise_rows(400, 300);
    let pixels = all_pixels(&rows);
    let png = rows.encode().unwrap();
    assert!(png.bytes().len() > pixels.len(), "{} bytes", png.bytes().len());
    assert_eq!(parse_encoded(png.bytes()).pixels, pixels);
}

#[test]
fn the_trailer_always_fits_in_the_room_kept_back() {
    // Small noise images end their compressed data at every offset of the
    // file's buffer, the last 16 bytes included (6x3, 19x1 to 24x1, ...
    // without the room kept back): finishing the file must never need
    // more room than was kept (it asserts so), and every one round-trips.
    for w in 1..80 {
        for h in 1..=3 {
            let rows = noise_rows(w, h);
            let pixels = all_pixels(&rows);
            assert_eq!(parse_encoded(rows.encode().unwrap().bytes()).pixels, pixels, "{w}x{h}");
        }
    }
}

/// A screen-like image: a dark background with rows of "glyphs" (a few
/// light pixels in 7x16 cells), as the text console shows.
fn text_screen(w: u32, h: u32) -> Rows {
    let mut rng = Rng(42);
    let mut rows = Rows::new(w, h).unwrap();
    for y in 0..h {
        let row = rows.row_mut(y);
        row.fill([0x0f, 0x14, 0x20]);
        if (144..h - 16).contains(&y) && y % 16 < 12 && (y / 16) % 3 != 0 {
            for x in (32..w as usize - 32).step_by(7) {
                if rng.below(3) == 0 {
                    row[x] = [0xe6, 0xe6, 0xe6];
                }
            }
        }
    }
    rows
}

#[test]
fn a_text_screen_compresses_to_a_small_fraction() {
    // Stored blocks (zune-png's encoder) would be bigger than the raw rows;
    // this must be deflate proper. 1280x800 RGB is 3 MB raw.
    let rows = text_screen(1280, 800);
    let pixels = all_pixels(&rows);
    let png = rows.encode().unwrap();
    assert!(png.bytes().len() * 20 < pixels.len(), "{png:?} for {} bytes of pixels", pixels.len());
    assert_eq!(parse_encoded(png.bytes()).pixels, pixels);
    // A uniform screen: next to nothing.
    let mut flat = Rows::new(1280, 800).unwrap();
    for y in 0..800 {
        flat.row_mut(y).fill([0x80; 3]);
    }
    assert!(flat.encode().unwrap().bytes().len() < 10_000);
}

#[test]
fn encode_says_no_when_the_file_cant_grow() {
    // The first buffer (a sixteenth of the rows) refused: 1280x800 needs
    // about 192 KiB, refused past 150 KiB.
    let rows = text_screen(1280, 800);
    let first = 8 + 25 + 8 + (1280 * 3 + 1) * 800 / 16 + 16 + 64;
    assert_eq!(refusing_past(150 << 10, || rows.encode().unwrap_err()), Error::OutOfMemory(first));
    // The file growing past 1 MiB (noise compresses to about 3 MiB): each
    // of the compressor's boxes (the largest about 83 KiB) is given, the
    // file's growth isn't.
    let noise = noise_rows(1280, 800);
    let err = refusing_past(1 << 20, || noise.encode().unwrap_err());
    let Error::OutOfMemory(bytes) = err else { panic!("{err:?}") };
    assert!(bytes > 1 << 20, "{bytes}");
    // And with room, the same image encodes.
    assert!(noise_rows(1280, 800).encode().is_ok());
}

// ---- The golden file, pinned byte for byte (and decoded by PIL in
// tests/harness/test_screenshot.py, which also parses the frame below:
// the two ends of the wire format, tied together).

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The golden image: 32x16, a gradient with a pattern, as
/// tests/harness/test_screenshot.py computes it too.
fn golden_rows() -> Rows {
    let mut rows = Rows::new(32, 16).unwrap();
    for y in 0..16u32 {
        for (x, px) in rows.row_mut(y).iter_mut().enumerate() {
            let x = x as u32;
            *px = [(x * 8) as u8, (y * 16) as u8, ((x ^ y) * 9) as u8];
        }
    }
    rows
}

fn frame_text(png: &Png) -> String {
    let mut out = String::new();
    frame::write(png, |line| {
        out.push_str(line);
        Ok(())
    })
    .unwrap();
    out
}

/// `bytes` must equal the golden file `name`; `LUMEN_UPDATE_GOLDEN=1`
/// writes it instead (look at what changed before committing it).
fn assert_golden(name: &str, bytes: &[u8]) {
    let path = golden_dir().join(name);
    if std::env::var_os("LUMEN_UPDATE_GOLDEN").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    let golden = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(golden == bytes, "{} changed (LUMEN_UPDATE_GOLDEN=1 re-records it)", path.display());
}

#[test]
fn the_golden_png_and_its_frame_are_unchanged() {
    let png = golden_rows().encode().unwrap();
    assert_golden("gradient.png", png.bytes());
    let text = frame_text(&png);
    assert!(text.lines().count() >= 5, "several body lines:\n{text}");
    assert_golden("gradient.frame.txt", text.as_bytes());
    assert_eq!(parse_encoded(png.bytes()).pixels, all_pixels(&golden_rows()));
}

// ---- The frame

/// The frame's lines back into a PNG, checked against its begin line, for
/// the round trips (the harness's reader is Python: screenshot.py).
fn unframe(text: &str) -> (u32, u32, Vec<u8>) {
    let lines: Vec<&str> = text.split_terminator('\n').collect();
    let begin = lines[0].strip_prefix(frame::BEGIN).expect("begin line");
    let fields: Vec<&str> = begin.split(' ').collect();
    let (w, h) = fields[0].split_once('x').unwrap();
    let field = |name: &str| fields.iter().find_map(|f| f.strip_prefix(name)).unwrap().to_string();
    let (bytes, crc, n) = (field("bytes="), field("crc32="), field("lines="));
    assert_eq!(*lines.last().unwrap(), frame::END);
    let body = &lines[1..lines.len() - 1];
    assert_eq!(body.len(), n.parse::<usize>().unwrap(), "body lines");
    let mut b64 = String::new();
    for (i, line) in body.iter().enumerate() {
        let chars = line.strip_prefix(frame::BODY).expect("body line");
        if i + 1 < body.len() {
            assert_eq!(chars.len(), frame::LINE_CHARS, "a full line");
        } else {
            assert!(!chars.is_empty() && chars.len() <= frame::LINE_CHARS);
        }
        b64.push_str(chars);
    }
    let png = base64_decode(&b64);
    assert_eq!(png.len(), bytes.parse::<usize>().unwrap());
    assert_eq!(crc, std::format!("{:08x}", crc32(&png)));
    (w.parse().unwrap(), h.parse().unwrap(), png)
}

#[test]
fn the_frame_carries_the_png_whole() {
    let rows = text_screen(320, 200);
    let pixels = all_pixels(&rows);
    let png = rows.encode().unwrap();
    let text = frame_text(&png);
    assert!(text.ends_with("\n@@png-end\n"));
    // Every line is the frame's: none could be taken for a log line.
    assert!(text.lines().all(|l| l.starts_with("@@png")));
    let (w, h, back) = unframe(&text);
    assert_eq!((w, h), (320, 200));
    assert_eq!(back, png.bytes());
    assert_eq!(parse_encoded(&back).pixels, pixels);
}

#[test]
fn write_hands_out_whole_lines_and_stops_at_an_error() {
    let png = Png::from_parts(7, 9, (0..1000u32).map(|i| (i * 31) as u8).collect());
    let mut lines = Vec::new();
    frame::write(&png, |line| {
        lines.push(line.to_string());
        Ok(())
    })
    .unwrap();
    // One call per line, each ending in its one newline.
    assert_eq!(lines.len(), 1 + frame::body_lines(1000) + 1);
    assert!(lines.iter().all(|l| l.ends_with('\n') && l.matches('\n').count() == 1), "{lines:?}");
    // An error at the begin line, at a body line, at the end line: no call
    // after it.
    let total = lines.len();
    for fail_at in [1, 3, total] {
        let mut calls = 0;
        let r = frame::write(&png, |_| {
            calls += 1;
            if calls == fail_at { Err(core::fmt::Error) } else { Ok(()) }
        });
        assert!(r.is_err(), "failing at call {fail_at}");
        assert_eq!(calls, fail_at, "nothing written after the error");
    }
}

#[test]
fn the_begin_line_states_size_length_crc_and_lines() {
    // 181 bytes: two full lines of 90 and one of 1.
    let bytes: Vec<u8> = (0..181u32).map(|i| i as u8).collect();
    let crc = crc32(&bytes);
    let text = frame_text(&Png::from_parts(7, 9, bytes.clone()));
    let first = text.lines().next().unwrap();
    assert_eq!(first, std::format!("@@png-begin 7x9 bytes=181 crc32={crc:08x} lines=3"));
    assert_eq!(frame::body_lines(181), 3);
    assert_eq!(frame::body_lines(180), 2);
    assert_eq!(frame::body_lines(0), 0);
    // Byte 180 (0xb4): 101101 00(0000), "tA" then padding.
    assert_eq!(text.lines().nth(3).unwrap(), "@@png tA==");
    assert_eq!(unframe(&text).2, bytes);
}

#[test]
fn the_longest_begin_line_fits_its_buffer() {
    // Every number at its widest.
    let line = frame::begin_line(u32::MAX, u32::MAX, usize::MAX, u32::MAX, usize::MAX).unwrap();
    assert_eq!(
        line.as_str().unwrap(),
        std::format!("@@png-begin {0}x{0} bytes={1} crc32=ffffffff lines={1}\n", u32::MAX, usize::MAX)
    );
}

#[test]
fn an_empty_file_frames_as_its_begin_and_end_lines() {
    let png = Png::from_parts(u32::MAX, u32::MAX, Vec::new());
    assert_eq!(
        frame_text(&png),
        std::format!("@@png-begin {0}x{0} bytes=0 crc32=00000000 lines=0\n@@png-end\n", u32::MAX)
    );
    assert_eq!(std::format!("{png:?}"), "Png(4294967295x4294967295, 0 bytes)");
}

// ---- Decoding (#154): `crate::decode::decode` over files built here
// chunk by chunk (any colour type, bit depth and filter), and over the
// encoder's own output.

use crate::decode::{self, DecodeError, Image};

/// One chunk: its length, type, data and CRC.
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crced = kind.to_vec();
    crced.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crced).to_be_bytes());
}

/// The chunks to put between IHDR and IDAT: type and data.
type Extra<'a> = &'a [(&'a [u8; 4], &'a [u8])];

/// A PNG of `width` x `height` with IHDR's `color_type`, `depth` and
/// `interlace`, `extra` chunks after IHDR, and `idat` (a zlib stream, as
/// given) in one IDAT.
fn png_raw(width: u32, height: u32, ihdr_tail: [u8; 3], extra: Extra<'_>, idat: &[u8]) -> Vec<u8> {
    let [depth, color_type, interlace] = ihdr_tail;
    let mut out = SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[depth, color_type, 0, 0, interlace]);
    chunk(&mut out, b"IHDR", &ihdr);
    for (kind, data) in extra {
        chunk(&mut out, kind, data);
    }
    chunk(&mut out, b"IDAT", idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// [`png_raw`] not interlaced, with `scanlines` (each row's filter byte,
/// then its samples) compressed into the IDAT.
fn png_file(width: u32, height: u32, color_type: u8, depth: u8, extra: Extra<'_>, scanlines: &[u8]) -> Vec<u8> {
    let idat = miniz_oxide::deflate::compress_to_vec_zlib(scanlines, 6);
    png_raw(width, height, [depth, color_type, 0], extra, &idat)
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

fn decode_ok(png: &[u8]) -> Image {
    decode::decode(png, 4096, 4096).unwrap_or_else(|e| panic!("should decode: {e}"))
}

#[test]
fn decode_reads_what_encode_writes() {
    let mut rng = Rng(11);
    for (w, h) in [(1, 1), (3, 2), (17, 9), (64, 40)] {
        let rows = random_rows(&mut rng, w, h);
        let want: Vec<Vec<u32>> = (0..h).map(|y| rows.row(y).iter().map(|p| rgb(p[0], p[1], p[2])).collect()).collect();
        let img = decode_ok(rows.encode().unwrap().bytes());
        assert_eq!((img.width(), img.height()), (w, h));
        for y in 0..h {
            assert_eq!(img.row(y), &want[y as usize][..], "row {y} of {w}x{h}");
        }
    }
}

#[test]
fn decode_reads_every_colour_type() {
    // Greyscale, 8 bits: two rows, the second Sub-filtered (each byte the
    // difference from the one to its left).
    let grey = png_file(2, 2, 0, 8, &[], &[0, 10, 200, 1, 50, 25]);
    assert_eq!(decode_ok(&grey).pixels(), &[rgb(10, 10, 10), rgb(200, 200, 200), rgb(50, 50, 50), rgb(75, 75, 75)]);
    // Greyscale, 1 bit: 0b1010 across four pixels.
    let bits = png_file(4, 1, 0, 1, &[], &[0, 0b1010_0000]);
    assert_eq!(decode_ok(&bits).pixels(), &[rgb(255, 255, 255), 0, rgb(255, 255, 255), 0]);
    // Greyscale with alpha: the alpha dropped.
    let grey_alpha = png_file(1, 1, 4, 8, &[], &[0, 99, 7]);
    assert_eq!(decode_ok(&grey_alpha).pixels(), &[rgb(99, 99, 99)]);
    // RGB, 16 bits: the top 8 bits kept.
    let wide = png_file(1, 1, 2, 16, &[], &[0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
    assert_eq!(decode_ok(&wide).pixels(), &[rgb(0x12, 0x56, 0x9a)]);
    // RGBA, Up-filtered second row: the alpha dropped.
    let rgba = png_file(1, 2, 6, 8, &[], &[0, 1, 2, 3, 4, 2, 10, 20, 30, 0]);
    assert_eq!(decode_ok(&rgba).pixels(), &[rgb(1, 2, 3), rgb(11, 22, 33)]);
    // A 2-bit palette, with transparency (made RGBA by the decoder, its
    // alpha dropped here).
    let palette: &[u8] = &[255, 0, 0, 0, 255, 0, 0, 0, 255];
    let plte = png_file(3, 1, 3, 2, &[(b"PLTE", palette), (b"tRNS", &[255, 0])], &[0, 0b0001_1000]);
    assert_eq!(decode_ok(&plte).pixels(), &[rgb(255, 0, 0), rgb(0, 255, 0), rgb(0, 0, 255)]);
}

/// A small RGB PNG (the encoder's: filter 0 on every row) with some
/// structure.
fn small_png() -> Vec<u8> {
    let mut rng = Rng(5);
    random_rows(&mut rng, 12, 7).encode().unwrap().bytes().to_vec()
}

#[test]
fn a_bad_png_is_an_error() {
    for bad in [&b""[..], b"PNG", b"\x89PNG\r\n\x1a\n", b"GIF89a\x01\x00\x01\x00", &[0u8; 64]] {
        assert!(matches!(decode::decode(bad, 4096, 4096), Err(DecodeError::Png(_))), "{bad:?}");
    }
    // A good signature, then garbage.
    let mut garbage = SIGNATURE.to_vec();
    garbage.extend((0..200u32).map(|i| (i * 37 % 251) as u8));
    assert!(matches!(decode::decode(&garbage, 4096, 4096), Err(DecodeError::Png(_))));
    // An unknown colour type (5), and a bit depth a palette doesn't allow.
    for (color_type, depth) in [(5, 8), (3, 16)] {
        let png = png_file(1, 1, color_type, depth, &[], &[0, 0, 0, 0]);
        assert!(decode::decode(&png, 4096, 4096).is_err(), "colour type {color_type}, depth {depth}");
    }
    // A palette image with no palette: the decoder takes it, and doesn't
    // panic.
    let png = png_file(1, 1, 3, 8, &[], &[0, 0]);
    let _either = decode::decode(&png, 4096, 4096);
    // Not enough rows in IDAT for the header's height.
    let png = png_file(2, 3, 0, 8, &[], &[0, 1, 2, 0, 3, 4]);
    assert!(decode::decode(&png, 4096, 4096).is_err());
}

#[test]
fn a_truncated_png_is_an_error_at_every_length() {
    let png = small_png();
    decode_ok(&png);
    for len in 0..png.len() {
        let got = decode::decode(&png[..len], 4096, 4096);
        assert!(got.is_err(), "{len} of {} bytes decoded: {got:?}", png.len());
    }
}

#[test]
fn a_corrupted_byte_is_an_error() {
    // Every byte of a small file flipped in turn: each chunk's CRC is
    // checked, so a change anywhere is refused (never a panic, never other
    // pixels).
    let png = small_png();
    for at in 0..png.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut bad = png.clone();
            bad[at] ^= flip;
            let got = decode::decode(&bad, 4096, 4096);
            assert!(got.is_err(), "byte {at} ^ {flip:#04x} decoded");
        }
    }
}

#[test]
fn an_image_past_the_limits_is_refused_with_its_size() {
    let png = png_file(64, 32, 0, 8, &[], &vec![0u8; 65 * 32]);
    assert_eq!(decode_ok(&png).width(), 64);
    let too_big = |w, h| DecodeError::TooBig { width: 64, height: 32, max_width: w, max_height: h };
    assert_eq!(decode::decode(&png, 63, 32), Err(too_big(63, 32)));
    assert_eq!(decode::decode(&png, 64, 31), Err(too_big(64, 31)));
    assert_eq!(decode::decode(&png, 64, 32).map(|i| i.height()), Ok(32));
    assert_eq!(too_big(63, 32).to_string(), "64x32 pixels, more than the 63x32 allowed");
}

#[test]
fn decode_says_no_when_the_allocator_does() {
    // Our two buffers refused, each is an error, not an abort: the pixels
    // (4 bytes each, the biggest), and the samples (3 bytes each, taken
    // before the decoder inflates anything into them).
    let (w, h) = (64u32, 64u32);
    let png = png_file(w, h, 2, 8, &[], &vec![0u8; (w as usize * 3 + 1) * h as usize]);
    let pixels = w as usize * h as usize * 4;
    let got = refusing_past(pixels - 1, || decode::decode(&png, 4096, 4096));
    assert_eq!(got, Err(DecodeError::OutOfMemory));
    let samples = w as usize * h as usize * 3;
    let got = refusing_past(samples - 1, || decode::decode(&png, 4096, 4096));
    assert_eq!(got, Err(DecodeError::OutOfMemory));
    assert_eq!(DecodeError::OutOfMemory.to_string(), "no memory for its pixels");
    decode_ok(&png);
}

#[test]
fn the_wallpaper_decodes() {
    // The desktop's wallpaper (assets/wallpaper/, made by
    // tools/wallpaper.py), embedded in the kernel: a 1280x800 image.
    let bytes = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/wallpaper/lumen.png"))
        .expect("the wallpaper");
    let img = decode_ok(&bytes);
    assert_eq!((img.width(), img.height()), (1280, 800));
    assert!(bytes.len() < 1024 * 1024, "the wallpaper is {} bytes", bytes.len());
}

#[test]
fn a_bad_adler32_is_an_error() {
    // The zlib stream's checksum wrong inside an IDAT whose chunk CRC is
    // right: strict mode refuses it.
    let scanlines = [0u8, 1, 2, 3, 0, 4, 5, 6];
    let mut zlib = miniz_oxide::deflate::compress_to_vec_zlib(&scanlines, 6);
    let last = zlib.len() - 1;
    zlib[last] ^= 0x01;
    let png = png_raw(1, 2, [8, 2, 0], &[], &zlib);
    assert!(matches!(decode::decode(&png, 4096, 4096), Err(DecodeError::Png(_))));
    decode_ok(&png_file(1, 2, 2, 8, &[], &scanlines));
}

/// A zTXt's data: a keyword, compression method 0, then `inflated` zero
/// bytes compressed.
fn ztxt_of_zeros(inflated: usize) -> Vec<u8> {
    let mut data = b"Comment\0\0".to_vec();
    data.extend(miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; inflated], 9));
    data
}

/// The heap a decode may take here, past its own file (the tests' images
/// are 1x1): far less than any bomb below inflates to.
const BOMB_HEAP: usize = 16 << 20;

#[test]
fn ancillary_chunks_never_reach_the_decoder() {
    // zTXt, iTXt and iCCP bombs (10 MiB of zeros each, valid CRCs) are
    // dropped, not inflated: each file decodes with the heap kept small. A
    // large ordinary iTXt (an XMP packet's size) decodes too.
    let pixel = [0u8, 7];
    let bomb = ztxt_of_zeros(10 << 20);
    for kind in [b"zTXt", b"iTXt", b"iCCP"] {
        let png = png_file(1, 1, 0, 8, &[(kind, &bomb)], &pixel);
        let got = refusing_past(BOMB_HEAP, || decode::decode(&png, 4096, 4096));
        assert_eq!(got.map(|i| i.pixels().to_vec()), Ok(vec![rgb(7, 7, 7)]), "{kind:?}");
    }
    let mut xmp = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    xmp.extend(core::iter::repeat_n(b'x', 200_000));
    assert_eq!(decode_ok(&png_file(1, 1, 0, 8, &[(b"iTXt", &xmp)], &pixel)).pixels(), &[rgb(7, 7, 7)]);
}

#[test]
fn chunks_zune_would_misread_hide_no_bomb() {
    // zune-png's own parsers take more than a short iTXt, zTXt or tRNS
    // declares, so a walk beside it could see one chunk where it sees
    // another: a hidden zTXt bomb. Only the image's chunks are handed to
    // it, so whatever the file says, the heap stays small and the answer is
    // an image or an error, never an abort.
    let png = png_file(1, 1, 0, 8, &[], &[0, 7]);
    let ihdr_end = 8 + 12 + 13;
    let bomb = ztxt_of_zeros(64 << 20);
    let hidden = |first: &[u8; 4], data: &[u8], skipped: &[u8]| {
        let mut out = png[..ihdr_end].to_vec();
        chunk(&mut out, first, data);
        out.extend_from_slice(skipped);
        chunk(&mut out, b"zTXt", &bomb);
        out.extend_from_slice(&png[ihdr_end..]);
        out
    };
    for file in [
        hidden(b"iTXt", b"k\0", &[0x7f, 0xff, 0xff, 0xff]),
        hidden(b"zTXt", b"k\0", &[0x7f]),
        hidden(b"tRNS", b"", &[0x7f, 0xff]),
    ] {
        let _image_or_error = refusing_past(BOMB_HEAP, || decode::decode(&file, 4096, 4096));
    }
    // Many small zTXt chunks (each about 4 MiB inflated): dropped too.
    let small = ztxt_of_zeros(4 << 20);
    let many: Vec<(&[u8; 4], &[u8])> = (0..64).map(|_| (b"zTXt", small.as_slice())).collect();
    let file = png_file(1, 1, 0, 8, &many, &[0, 7]);
    let got = refusing_past(BOMB_HEAP, || decode::decode(&file, 4096, 4096));
    assert_eq!(got.map(|i| i.pixels().to_vec()), Ok(vec![rgb(7, 7, 7)]));
}

#[test]
fn an_unknown_critical_chunk_or_a_bad_palette_is_an_error() {
    let pixel = [0u8, 7];
    let cases: [(&[u8; 4], &[u8], &str); 4] = [
        (b"ABCD", &[], "an unknown critical chunk ABCD"),
        (b"PLTE", &[1, 2], "a PLTE of 2 bytes"),
        (b"PLTE", &[0; 771], "a PLTE of 771 bytes"),
        (b"PLTE", &[], "a PLTE of 0 bytes"),
    ];
    for (kind, data, why) in cases {
        let got = decode::decode(&png_file(1, 1, 0, 8, &[(kind, data)], &pixel), 4096, 4096);
        assert_eq!(got, Err(DecodeError::Png(why.into())));
    }
    let bad_type = png_file(1, 1, 0, 8, &[(b"a1bc", &[])], &pixel);
    let got = decode::decode(&bad_type, 4096, 4096);
    assert_eq!(got, Err(DecodeError::Png("a chunk type that isn't four letters".into())));
    // A chunk declaring more than the file holds.
    let mut huge = png_file(1, 1, 0, 8, &[], &pixel);
    huge[8..12].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
    assert_eq!(decode::decode(&huge, 4096, 4096), Err(DecodeError::Png("cut short in a IHDR chunk".into())));
}

/// An Adam7-interlaced PNG of `pixels` (RGB, `width` x `height`): each of
/// the seven passes' rows, filter 0, in order (empty passes left out).
fn adam7(width: u32, height: u32, pixels: &[[u8; 3]]) -> Vec<u8> {
    const PASSES: [(u32, u32, u32, u32); 7] =
        [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];
    let mut scanlines = Vec::new();
    for (x0, y0, dx, dy) in PASSES {
        if x0 >= width {
            continue;
        }
        for y in (y0..height).step_by(dy as usize) {
            scanlines.push(0);
            for x in (x0..width).step_by(dx as usize) {
                scanlines.extend_from_slice(&pixels[(y * width + x) as usize]);
            }
        }
    }
    png_raw(width, height, [8, 2, 1], &[], &miniz_oxide::deflate::compress_to_vec_zlib(&scanlines, 6))
}

#[test]
fn an_interlaced_png_decodes() {
    // 9x7: every pass has pixels, and the last column and row are partial.
    let (w, h) = (9u32, 7u32);
    let pixels: Vec<[u8; 3]> = (0..w * h).map(|i| [i as u8, (i * 3) as u8, (255 - i) as u8]).collect();
    let img = decode_ok(&adam7(w, h, &pixels));
    let want: Vec<u32> = pixels.iter().map(|p| rgb(p[0], p[1], p[2])).collect();
    assert_eq!((img.width(), img.height()), (w, h));
    assert_eq!(img.pixels(), &want[..]);
    assert_eq!(img.into_pixels(), want);
}
