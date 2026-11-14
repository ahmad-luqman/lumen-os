//! A ring of text: the newest bytes written, the oldest overwritten. The
//! kernel's logger keeps its lines in one (`dmesg`). Unlike [`TraceRing`]
//! it isn't lock-free: the writer and readers hold a lock (the logger's),
//! and nothing here allocates or blocks.
//!
//! [`TraceRing`]: crate::TraceRing

/// The last `N` bytes written.
pub struct TextRing<const N: usize> {
    buf: [u8; N],
    /// Bytes written in all; the next goes at `written % N`.
    written: u64,
}

impl<const N: usize> Default for TextRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TextRing<N> {
    pub const fn new() -> Self {
        assert!(N > 0);
        TextRing { buf: [0; N], written: 0 }
    }

    /// Append `bytes`, overwriting the oldest once full.
    pub fn push(&mut self, bytes: &[u8]) {
        // Only the last N can be kept.
        let skip = bytes.len().saturating_sub(N);
        self.written += skip as u64;
        for &b in &bytes[skip..] {
            self.buf[(self.written % N as u64) as usize] = b;
            self.written += 1;
        }
    }

    /// Bytes written in all.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Bytes held (at most `N`).
    pub fn len(&self) -> usize {
        self.written.min(N as u64) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    /// Byte `i` of what is held (0 is the oldest).
    fn byte(&self, i: usize) -> u8 {
        let first = self.written - self.len() as u64;
        self.buf[((first + i as u64) % N as u64) as usize]
    }

    /// Copy the newest whole lines held that fit into `out`, oldest first;
    /// returns how many bytes. A line is whole if it starts where writing
    /// started or just after a `\n` (the oldest bytes of a full ring are the
    /// end of a line whose start was overwritten, and are left out); the
    /// last line may lack its `\n` (it is still being written).
    pub fn copy_lines(&self, out: &mut [u8]) -> usize {
        let len = self.len();
        let mut start = len.saturating_sub(out.len());
        // Unless it starts where writing did, skip to after the first line
        // feed at or past `start - 1`.
        let at_beginning = start == 0 && self.written == len as u64;
        if !at_beginning {
            let from = start.saturating_sub(1);
            match (from..len).find(|&i| self.byte(i) == b'\n') {
                Some(nl) => start = nl + 1,
                None => return 0,
            }
        }
        for (i, slot) in (start..len).zip(out.iter_mut()) {
            *slot = self.byte(i);
        }
        len - start
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn lines<const N: usize>(r: &TextRing<N>, room: usize) -> Vec<u8> {
        let mut out = vec![0; room];
        let n = r.copy_lines(&mut out);
        out.truncate(n);
        out
    }

    #[test]
    fn holds_everything_until_full() {
        let mut r = TextRing::<16>::new();
        assert!(r.is_empty());
        assert_eq!(lines(&r, 64), b"");
        r.push(b"one\ntwo\n");
        assert_eq!((r.len(), r.written()), (8, 8));
        assert_eq!(lines(&r, 64), b"one\ntwo\n");
        // An unfinished last line is shown as far as it goes.
        r.push(b"thr");
        assert_eq!(lines(&r, 64), b"one\ntwo\nthr");
    }

    #[test]
    fn a_full_ring_drops_the_partial_oldest_line() {
        let mut r = TextRing::<16>::new();
        r.push(b"first\nsecond\nthird\n");
        // The last 16 bytes are "st\nsecond\nthird\n": "st" ends a line
        // whose start is gone.
        assert_eq!(r.len(), 16);
        assert_eq!(lines(&r, 64), b"second\nthird\n");
        // Exactly at a line boundary: the oldest line is whole but its
        // start can't be known to be one, so it is dropped as well.
        let mut r = TextRing::<6>::new();
        r.push(b"ab\ncd\nef\n");
        assert_eq!(lines(&r, 64), b"ef\n");
    }

    #[test]
    fn a_small_buffer_gets_the_newest_whole_lines() {
        let mut r = TextRing::<64>::new();
        r.push(b"alpha\nbeta\ngamma\n");
        assert_eq!(lines(&r, 10), b"gamma\n");
        assert_eq!(lines(&r, 11), b"beta\ngamma\n");
        assert_eq!(lines(&r, 17), b"alpha\nbeta\ngamma\n");
        // No line fits: nothing.
        assert_eq!(lines(&r, 5), b"");
    }

    #[test]
    fn pushing_more_than_it_holds_keeps_the_end() {
        let mut r = TextRing::<8>::new();
        r.push(b"0123456789\nabcdef\n");
        assert_eq!(r.written(), 18);
        assert_eq!(lines(&r, 64), b"abcdef\n");
    }

    #[test]
    fn matches_a_model_over_many_writes() {
        // Against keeping everything written: the newest whole lines that
        // fit in `room` and in the ring.
        const N: usize = 37;
        let mut r = TextRing::<N>::new();
        let mut all = Vec::new();
        for i in 0..500u32 {
            let line = std::format!("line {i} {}\n", "x".repeat((i % 13) as usize));
            r.push(line.as_bytes());
            all.extend_from_slice(line.as_bytes());
            for room in [8, 20, N, 100] {
                let held = all.len().min(N);
                let first = all.len().saturating_sub(room.min(held));
                // Whole lines: those starting after a line feed that is
                // still held (or where writing began).
                let want: &[u8] = if first == 0 {
                    &all
                } else {
                    let floor = (all.len() - held).max(first - 1);
                    match all[floor..].iter().position(|&b| b == b'\n') {
                        Some(p) => &all[floor + p + 1..],
                        None => &[],
                    }
                };
                assert_eq!(lines(&r, room), want, "after line {i}, room {room}");
            }
        }
    }
}
