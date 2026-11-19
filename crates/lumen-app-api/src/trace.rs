//! `trace_read`'s output: a [`HEADER_SIZE`]-byte header (`next`, `missed`),
//! then trace events of [`EVENT_SIZE`] bytes each (`seq`, `tsc`, `cpu`,
//! `kind`, `pid`, `args`), all little-endian with no alignment needed
//! (ARCHITECTURE section 8, "Fixed layouts"). Encoded by the kernel, decoded
//! by an app (X-ray, #208); the bytes are pinned by a test.

use crate::Error;
use crate::limits::TRACE_EVENTS_PER_READ;
use lumen_trace::Event;

/// The header's bytes.
pub const HEADER_SIZE: usize = 16;
/// One event's bytes.
pub const EVENT_SIZE: usize = 56;

/// How many events a `trace_read` into a buffer of `len` bytes copies at
/// most: [`Error::BufferTooSmall`] below a header and one event, else what
/// fits, at most [`TRACE_EVENTS_PER_READ`].
pub fn events_that_fit(len: usize) -> Result<usize, Error> {
    if len < HEADER_SIZE + EVENT_SIZE {
        return Err(Error::BufferTooSmall);
    }
    Ok(((len - HEADER_SIZE) / EVENT_SIZE).min(TRACE_EVENTS_PER_READ))
}

/// The header: the next `since`, and the numbers counted missed.
pub fn header(next: u64, missed: u64) -> [u8; HEADER_SIZE] {
    let mut out = [0; HEADER_SIZE];
    out[..8].copy_from_slice(&next.to_le_bytes());
    out[8..].copy_from_slice(&missed.to_le_bytes());
    out
}

/// A header's (next, missed).
pub fn decode_header(bytes: &[u8; HEADER_SIZE]) -> (u64, u64) {
    let word = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
    (word(0), word(8))
}

/// One event's bytes.
pub fn event(e: &Event) -> [u8; EVENT_SIZE] {
    let mut out = [0; EVENT_SIZE];
    out[0..8].copy_from_slice(&e.seq.to_le_bytes());
    out[8..16].copy_from_slice(&e.tsc.to_le_bytes());
    out[16..18].copy_from_slice(&e.cpu.to_le_bytes());
    out[18..20].copy_from_slice(&e.kind.to_le_bytes());
    out[20..24].copy_from_slice(&e.pid.to_le_bytes());
    for (i, a) in e.args.iter().enumerate() {
        out[24 + 8 * i..32 + 8 * i].copy_from_slice(&a.to_le_bytes());
    }
    out
}

/// An event back from its bytes.
pub fn decode_event(bytes: &[u8; EVENT_SIZE]) -> Event {
    let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
    let u16_at = |i: usize| u16::from_le_bytes(bytes[i..i + 2].try_into().expect("2 bytes"));
    Event {
        seq: u64_at(0),
        tsc: u64_at(8),
        cpu: u16_at(16),
        kind: u16_at(18),
        pid: u32::from_le_bytes(bytes[20..24].try_into().expect("4 bytes")),
        args: [u64_at(24), u64_at(32), u64_at(40), u64_at(48)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layouts_are_pinned() {
        assert_eq!(header(0x0102, 3), [0x02, 0x01, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_header(&header(u64::MAX, 7)), (u64::MAX, 7));
        let e = Event { seq: 1, tsc: 2, cpu: 3, kind: 0x0506, pid: 7, args: [8, 9, 10, u64::MAX] };
        let bytes = event(&e);
        #[rustfmt::skip]
        let want: [u8; EVENT_SIZE] = [
            1, 0, 0, 0, 0, 0, 0, 0,
            2, 0, 0, 0, 0, 0, 0, 0,
            3, 0, 0x06, 0x05, 7, 0, 0, 0,
            8, 0, 0, 0, 0, 0, 0, 0,
            9, 0, 0, 0, 0, 0, 0, 0,
            10, 0, 0, 0, 0, 0, 0, 0,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        assert_eq!(bytes, want);
        assert_eq!(decode_event(&bytes), e);
    }

    #[test]
    fn what_a_buffer_holds() {
        assert_eq!(events_that_fit(0), Err(Error::BufferTooSmall));
        assert_eq!(events_that_fit(HEADER_SIZE + EVENT_SIZE - 1), Err(Error::BufferTooSmall));
        assert_eq!(events_that_fit(HEADER_SIZE + EVENT_SIZE), Ok(1));
        assert_eq!(events_that_fit(HEADER_SIZE + 3 * EVENT_SIZE + 55), Ok(3));
        assert_eq!(events_that_fit(usize::MAX), Ok(TRACE_EVENTS_PER_READ));
    }
}
