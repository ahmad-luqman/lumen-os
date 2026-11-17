extern crate std;

use super::*;
use crate::crc32::{Crc32, crc32};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use std::string::ToString;

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
// decompressor, rows unfiltered by the PNG spec's definitions.

#[derive(Debug)]
struct Decoded {
    width: u32,
    height: u32,
    header: [u8; 5],
    chunks: Vec<[u8; 4]>,
    filters: Vec<u8>,
    pixels: Vec<u8>,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn decode(png: &[u8]) -> Decoded {
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
    let mut pixels = vec![0u8; (stride - 1) * height as usize];
    let mut filters = Vec::new();
    for y in 0..height as usize {
        let row = &raw[y * stride..(y + 1) * stride];
        filters.push(row[0]);
        for x in 0..stride - 1 {
            let left = if x >= 3 { pixels[y * (stride - 1) + x - 3] } else { 0 };
            let up = if y > 0 { pixels[(y - 1) * (stride - 1) + x] } else { 0 };
            pixels[y * (stride - 1) + x] = match row[0] {
                0 => row[1 + x],
                1 => row[1 + x].wrapping_add(left),
                2 => row[1 + x].wrapping_add(up),
                other => panic!("filter type {other}"),
            };
        }
    }
    Decoded { width, height, header: ihdr[8..13].try_into().unwrap(), chunks, filters, pixels }
}

fn random_rows(rng: &mut Rng, width: u32, height: u32) -> (Rows, Vec<u8>) {
    let mut rows = Rows::new(width, height).unwrap();
    let mut all = Vec::new();
    for y in 0..height {
        for b in rows.row_mut(y) {
            // Few distinct values, so runs and repeats occur (as on a screen).
            *b = [0x0f, 0x14, 0x20, 0xe6, 0xff][rng.below(5) as usize];
        }
        all.extend_from_slice(rows.row(y));
    }
    (rows, all)
}

const FILTERS: [Filter; 3] = [Filter::None, Filter::Sub, Filter::Up];

// ---- CRC-32

#[test]
fn crc32_matches_the_standard_check_values() {
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
    // The CRC every PNG's IEND chunk carries (of its type alone).
    assert_eq!(crc32(b"IEND"), 0xae42_6082);
}

#[test]
fn crc32_in_pieces_equals_crc32_at_once() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + i / 3) as u8).collect();
    for split in [0, 1, 3, 255, 256, 999, 1000] {
        let mut c = Crc32::new();
        c.update(&data[..split]);
        c.update(&data[split..]);
        assert_eq!(c.finish(), crc32(&data), "split at {split}");
    }
    assert_eq!(Crc32::default().finish(), 0);
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
        let mut out = String::new();
        base64::encode_into(plain.as_bytes(), &mut out);
        assert_eq!(out, coded, "{plain:?}");
        assert_eq!(base64::encoded_len(plain.len()), coded.len());
    }
}

#[test]
fn base64_round_trips_every_byte_value_at_every_alignment() {
    let all: Vec<u8> = (0..=255).collect();
    for start in 0..3 {
        let mut out = String::from("x");
        base64::encode_into(&all[start..], &mut out);
        // Appended: what was there stays.
        assert!(out.starts_with('x'));
        assert_eq!(base64_decode(&out[1..]), all[start..]);
        // The 62nd and 63rd characters both show up.
        assert!(out.contains('+') && out.contains('/'), "{out}");
    }
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
    // Within the address space but past what any allocator gives: refused,
    // not aborted.
    let (w, h) = (MAX_DIMENSION, 1 << 20);
    let len = (w as usize * 3 + 1) * h as usize;
    assert_eq!(Rows::new(w, h).unwrap_err(), Error::OutOfMemory(len));
    assert_eq!(Error::OutOfMemory(len).to_string(), std::format!("no memory for {len} bytes of pixels"));
}

#[test]
fn rows_start_black_and_keep_what_is_written() {
    let mut rows = Rows::new(3, 2).unwrap();
    assert_eq!((rows.width(), rows.height()), (3, 2));
    assert!(rows.row(0).iter().chain(rows.row(1)).all(|&b| b == 0));
    assert_eq!(rows.row_mut(1).len(), 9);
    rows.row_mut(1).copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    rows.row_mut(0)[0] = 42;
    assert_eq!(rows.row(1), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(rows.row(0), [42, 0, 0, 0, 0, 0, 0, 0, 0]);
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

// ---- Filters, worked by hand

#[test]
fn filters_take_the_differences_the_spec_defines() {
    // 2x2: row 0 (10,20,30) (15,25,5); row 1 (10,20,30) (200,0,255).
    let fill = || {
        let mut rows = Rows::new(2, 2).unwrap();
        rows.row_mut(0).copy_from_slice(&[10, 20, 30, 15, 25, 5]);
        rows.row_mut(1).copy_from_slice(&[10, 20, 30, 200, 0, 255]);
        rows
    };
    assert_eq!(fill().filtered(Filter::None), [0, 10, 20, 30, 15, 25, 5, 0, 10, 20, 30, 200, 0, 255]);
    // Sub: the first pixel as is, then minus the pixel to the left (mod 256).
    assert_eq!(fill().filtered(Filter::Sub), [1, 10, 20, 30, 5, 5, 231, 1, 10, 20, 30, 190, 236, 225]);
    // Up: the first row as is, then minus the row above (mod 256).
    assert_eq!(fill().filtered(Filter::Up), [2, 10, 20, 30, 15, 25, 5, 2, 0, 0, 0, 185, 231, 250]);
}

// ---- The file

#[test]
fn encode_writes_signature_ihdr_idat_iend() {
    let mut rng = Rng(0x1234_5678);
    let (rows, _) = random_rows(&mut rng, 1280, 3);
    let png = rows.encode(Filter::Up, 6);
    assert_eq!(png[..8], SIGNATURE);
    // IHDR first, at a fixed place: length 13, type, width, height, then
    // depth 8, colour type 2 (RGB), compression 0, filter method 0, no
    // interlace.
    assert_eq!(png[8..16], [0, 0, 0, 13, b'I', b'H', b'D', b'R']);
    assert_eq!(be32(&png[16..]), 1280);
    assert_eq!(be32(&png[20..]), 3);
    assert_eq!(png[24..29], [8, 2, 0, 0, 0]);
    assert_eq!(be32(&png[29..]), crc32(&png[12..29]));
    // IEND last, exactly as every PNG ends.
    assert_eq!(png[png.len() - 12..], [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
    let d = decode(&png);
    assert_eq!(d.chunks, [*b"IHDR", *b"IDAT", *b"IEND"]);
    assert_eq!(d.header, [8, 2, 0, 0, 0]);
}

#[test]
fn every_filter_and_level_round_trips() {
    let mut rng = Rng(0xdead_beef);
    for (w, h) in [(1, 1), (1, 7), (7, 1), (2, 2), (33, 17), (300, 5)] {
        for filter in FILTERS {
            for level in [0, 1, 6, 10] {
                let (rows, pixels) = random_rows(&mut rng, w, h);
                let d = decode(&rows.encode(filter, level));
                assert_eq!((d.width, d.height), (w, h));
                assert!(d.filters.iter().all(|&f| f == filter.byte()), "{filter:?}");
                assert_eq!(d.pixels, pixels, "{w}x{h} {filter:?} level {level}");
            }
        }
    }
}

/// A screen-like image: a dark background with rows of "glyphs" (a few
/// light pixels in 7x16 cells), as the text console shows.
fn text_screen(w: u32, h: u32) -> (Rows, Vec<u8>) {
    let mut rng = Rng(42);
    let mut rows = Rows::new(w, h).unwrap();
    let mut all = Vec::new();
    for y in 0..h {
        let row = rows.row_mut(y);
        for x in 0..w as usize {
            row[x * 3..x * 3 + 3].copy_from_slice(&[0x0f, 0x14, 0x20]);
        }
        if (144..h - 16).contains(&y) && y % 16 < 12 && (y / 16) % 3 != 0 {
            for x in (32..w as usize - 32).step_by(7) {
                if rng.below(3) == 0 {
                    row[x * 3..x * 3 + 3].copy_from_slice(&[0xe6, 0xe6, 0xe6]);
                }
            }
        }
        all.extend_from_slice(row);
    }
    (rows, all)
}

#[test]
fn a_text_screen_compresses_to_a_small_fraction() {
    // Stored blocks (zune-png's encoder) would be bigger than the raw rows;
    // this must be deflate proper. 1280x800 RGB is 3 MB raw.
    for filter in FILTERS {
        let (rows, pixels) = text_screen(1280, 800);
        let raw = pixels.len();
        let png = rows.encode(filter, 6);
        assert!(png.len() * 20 < raw, "{filter:?}: {} bytes of PNG for {raw} of pixels", png.len());
        assert_eq!(decode(&png).pixels, pixels);
    }
    // A uniform screen: next to nothing.
    let mut flat = Rows::new(1280, 800).unwrap();
    for y in 0..800 {
        flat.row_mut(y).fill(0x80);
    }
    assert!(flat.encode(Filter::Up, 6).len() < 10_000);
}

// ---- The frame

/// The frame's lines back into a PNG, checked against its begin line, for
/// the round trips (the harness's reader is Python: screenshot.py).
fn unframe(text: &str) -> (u32, u32, Vec<u8>) {
    let lines: Vec<&str> = text.split_terminator('\n').filter(|l| l.starts_with("@@png")).collect();
    let begin = lines[0].strip_prefix(frame::BEGIN).expect("begin line");
    let fields: Vec<&str> = begin.split(' ').collect();
    let (w, h) = fields[0].split_once('x').unwrap();
    let field = |name: &str| {
        let f = fields.iter().find_map(|f| f.strip_prefix(name)).unwrap();
        f.to_string()
    };
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
    assert_eq!(crc.len(), 8);
    (w.parse().unwrap(), h.parse().unwrap(), png)
}

#[test]
fn the_frame_carries_the_png_whole() {
    let (rows, pixels) = text_screen(320, 200);
    let png = rows.encode(Filter::Up, 6);
    let text = frame::frame(&png, 320, 200);
    assert!(text.len() <= frame::max_len(png.len()), "{} > {}", text.len(), frame::max_len(png.len()));
    assert!(text.ends_with("\n@@png-end\n"));
    // Every line is the frame's: none could be taken for a log line.
    assert!(text.lines().all(|l| l.starts_with("@@png")));
    let (w, h, back) = unframe(&text);
    assert_eq!((w, h), (320, 200));
    assert_eq!(back, png);
    assert_eq!(decode(&back).pixels, pixels);
}

#[test]
fn the_begin_line_states_size_length_crc_and_lines() {
    // 181 bytes: two full lines of 90 and one of 1.
    let png: Vec<u8> = (0..181u32).map(|i| i as u8).collect();
    let text = frame::frame(&png, 7, 9);
    let first = text.lines().next().unwrap();
    assert_eq!(first, std::format!("@@png-begin 7x9 bytes=181 crc32={:08x} lines=3", crc32(&png)));
    assert_eq!(frame::body_lines(181), 3);
    assert_eq!(frame::body_lines(180), 2);
    assert_eq!(frame::body_lines(0), 0);
    let last_body = text.lines().nth(3).unwrap();
    // Byte 180 (0xb4): 101101 00(0000), "tA" then padding.
    assert_eq!(last_body, "@@png tA==");
    assert_eq!(unframe(&text).2, png);
}

#[test]
fn log_lines_between_frame_lines_are_skipped() {
    let png: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
    let text = frame::frame(&png, 4, 4);
    let mut mixed = String::new();
    for (i, line) in text.lines().enumerate() {
        mixed.push_str(line);
        mixed.push('\n');
        if i % 3 == 0 {
            mixed.push_str("[    1.234567] INFO  boot: a log line in between\n");
        }
    }
    assert_eq!(unframe(&mixed).2, png);
}

#[test]
fn max_len_bounds_the_largest_begin_line() {
    // Numbers at their widest still fit the bound (an empty PNG: no body).
    let begin = std::format!(
        "{}{}x{} bytes={} crc32={:08x} lines={}\n",
        frame::BEGIN,
        u32::MAX,
        u32::MAX,
        usize::MAX,
        u32::MAX,
        usize::MAX
    );
    assert!(begin.len() + frame::END.len() < frame::max_len(0), "{}", begin.len());
    let empty = frame::frame(&[], 1, 1);
    assert_eq!(empty, "@@png-begin 1x1 bytes=0 crc32=00000000 lines=0\n@@png-end\n");
}
