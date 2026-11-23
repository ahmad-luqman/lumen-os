//! Page-span arithmetic for a file's page cache (4 KiB pages): which pages a
//! byte range touches and where, how many bytes of a page lie inside a
//! file, and which bytes past a file's end must be zero when its size
//! changes. The kernel's `PageCache` (shared by `tmpfs` and `ext4`) walks
//! its reads and writes with [`spans`], zeroes with [`resize_tail`] and, on
//! a shrink, drops the pages from [`pages_in`] on; a page's [`valid_len`]
//! bytes are what `ext4` reads in and writes back, and what a private
//! mapping's copy of a page keeps (`MappedPage`, either filesystem's).

use core::ops::Range;

/// A page-cache page's size.
pub const PAGE_SIZE: usize = 4096;

const PAGE: u64 = PAGE_SIZE as u64;

/// One page's part of a byte range: the bytes [`buf`](Self::buf) of the
/// range (offsets into the caller's buffer) lie in page
/// [`index`](Self::index) (file offset `index * 4096`) from
/// [`in_page`](Self::in_page) on, never past the page's end. Only
/// [`spans`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageSpan {
    index: u64,
    in_page: usize,
    buf: Range<usize>,
}

impl PageSpan {
    /// The page's index in the file.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Where in the page the span starts.
    pub fn in_page(&self) -> usize {
        self.in_page
    }

    /// The span's bytes, as offsets into the caller's buffer (never empty).
    pub fn buf(&self) -> Range<usize> {
        self.buf.clone()
    }

    /// Whether the span covers its whole page.
    pub fn whole_page(&self) -> bool {
        self.buf.len() == PAGE_SIZE
    }

    /// The file offset where the span's page starts.
    pub fn page_start(&self) -> u64 {
        self.index * PAGE
    }
}

/// The pages the `len` bytes at file offset `offset` touch, in order, each
/// with the part of the range it holds: the first from `offset % 4096`, the
/// others from 0, each up to its page's end or the range's. `None` when the
/// range ends past `u64::MAX` (no file reaches there); an empty range has no
/// spans.
pub fn spans(offset: u64, len: usize) -> Option<PageSpans> {
    offset.checked_add(len as u64)?;
    Some(PageSpans { offset, len, done: 0 })
}

/// The iterator [`spans`] returns.
#[derive(Clone, Debug)]
pub struct PageSpans {
    offset: u64,
    len: usize,
    done: usize,
}

impl Iterator for PageSpans {
    type Item = PageSpan;

    fn next(&mut self) -> Option<PageSpan> {
        if self.done >= self.len {
            return None;
        }
        // No overflow: `spans` checked `offset + len`.
        let at = self.offset + self.done as u64;
        let in_page = (at % PAGE) as usize;
        let chunk = (PAGE_SIZE - in_page).min(self.len - self.done);
        let span = PageSpan { index: at / PAGE, in_page, buf: self.done..self.done + chunk };
        self.done += chunk;
        Some(span)
    }
}

/// `step` run on each of `spans` in order, until one fails: how many of
/// the buffer's bytes the spans before it cover, or that first span's error
/// when nothing came before it. The page cache's reads and writes answer
/// this way (a short count, never an error, once some bytes moved, as
/// Linux's `filemap_read` and `generic_perform_write` do); the caller's next
/// call starts at the failing page and gets its error then. No span runs
/// after a failure.
pub fn short_count<E>(
    spans: impl IntoIterator<Item = PageSpan>,
    mut step: impl FnMut(&PageSpan) -> Result<(), E>,
) -> Result<usize, E> {
    let mut done = 0usize;
    for span in spans {
        if let Err(err) = step(&span) {
            return if done > 0 { Ok(done) } else { Err(err) };
        }
        done = span.buf().end;
    }
    Ok(done)
}

/// How many pages a file of `size` bytes has: its pages are `0..pages_in(size)`,
/// and a shrink to `size` drops every cached page from `pages_in(size)` on.
pub const fn pages_in(size: u64) -> u64 {
    size.div_ceil(PAGE)
}

/// How many bytes of page `index` lie below `file_size`: 4096 for a page
/// wholly inside the file, fewer for the page holding its end, 0 for a
/// page at or past it (an index whose offset overflows included).
pub const fn valid_len(index: u64, file_size: u64) -> usize {
    let len = file_size.saturating_sub(index.saturating_mul(PAGE));
    if len < PAGE { len as usize } else { PAGE_SIZE }
}

/// A page's bytes from [`from`](Self::from) to its end, which lie past the
/// smaller of a file's old and new ends: `0 < from < 4096`. Only
/// [`resize_tail`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tail {
    index: u64,
    from: usize,
}

impl Tail {
    /// The page's index in the file.
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// Where in the page the bytes to zero start.
    pub const fn from(&self) -> usize {
        self.from
    }
}

/// The bytes a change of a file's size from `old` to `new` must zero in the
/// cached page holding the smaller of the two ends, from that end to the
/// page's end; `None` when the size doesn't change or that end falls on a
/// page boundary (no page holds bytes past it).
///
/// - A shrink (`new < old`): the cut bytes of the new last page, so that
///   growing the file again reads zeros there.
/// - A growth (`new > old`: a truncate up, or a write starting past the end,
///   for which `new` is where the write starts): the old last page's bytes
///   past the old end, where a `MAP_SHARED` mapping may have stored, which
///   now lie inside the file and must read as zeros.
///
/// A write starting at or before the end (`new == old`) zeroes nothing: it
/// overwrites the bytes it extends the file over.
pub const fn resize_tail(old: u64, new: u64) -> Option<Tail> {
    if old == new {
        return None;
    }
    let end = if new < old { new } else { old };
    let from = (end % PAGE) as usize;
    if from == 0 { None } else { Some(Tail { index: end / PAGE, from }) }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    /// Offsets around page edges, and near `u64::MAX`.
    fn offsets() -> Vec<u64> {
        let mut v = Vec::new();
        for page in [0u64, 1, 2, 7] {
            for d in [0u64, 1, 2, 100, PAGE - 2, PAGE - 1] {
                v.push(page * PAGE + d);
            }
        }
        v.extend([u64::MAX - 3 * PAGE, u64::MAX - PAGE - 1, u64::MAX - 10, u64::MAX]);
        v
    }

    fn lens() -> Vec<usize> {
        let mut v = vec![0, 1, 2, 3, 100, PAGE_SIZE - 1, PAGE_SIZE, PAGE_SIZE + 1];
        v.extend([2 * PAGE_SIZE - 1, 2 * PAGE_SIZE, 2 * PAGE_SIZE + 1, 3 * PAGE_SIZE + 77]);
        v
    }

    #[test]
    fn spans_match_a_byte_by_byte_model() {
        // Every byte of the range in exactly one span, in order, at its own
        // page and offset there; each span as long as it can be (the next
        // one starts a new page); none empty.
        for off in offsets() {
            for len in lens() {
                let Some(it) = spans(off, len) else {
                    assert!(off.checked_add(len as u64).is_none(), "{off} {len} refused");
                    continue;
                };
                assert!(off.checked_add(len as u64).is_some(), "{off} {len} accepted");
                // Bounded, so spans that stop advancing fail rather than
                // hang: a range touches at most `len / 4096 + 2` pages.
                let got: Vec<PageSpan> = it.take(len / PAGE_SIZE + 3).collect();
                assert!(got.len() <= len / PAGE_SIZE + 2, "{off} {len}: {} spans", got.len());
                let mut i = 0usize;
                for (k, s) in got.iter().enumerate() {
                    assert!(!s.buf.is_empty());
                    assert_eq!(s.buf.start, i, "{off} {len}: span {k} starts where the last ended");
                    assert!(s.in_page + s.buf.len() <= PAGE_SIZE);
                    for b in s.buf.clone() {
                        let at = off + b as u64;
                        assert_eq!((s.index, s.in_page + (b - s.buf.start)), (at / PAGE, (at % PAGE) as usize));
                    }
                    if k + 1 < got.len() {
                        assert_eq!(s.in_page + s.buf.len(), PAGE_SIZE, "{off} {len}: span {k} stops at its page's end");
                        assert_eq!(got[k + 1].index, s.index + 1);
                        assert_eq!(got[k + 1].in_page, 0);
                    }
                    assert_eq!((s.index(), s.in_page(), s.buf()), (s.index, s.in_page, s.buf.clone()));
                    assert_eq!(s.page_start(), s.index * PAGE);
                    assert_eq!(s.whole_page(), s.buf.len() == PAGE_SIZE);
                    i = s.buf.end;
                }
                assert_eq!(i, len, "{off} {len}: the spans cover the range");
            }
        }
    }

    #[test]
    fn spans_refuse_a_range_past_u64_max() {
        assert!(spans(u64::MAX, 1).is_none());
        assert!(spans(u64::MAX - 1, 2).is_none());
        let last: Vec<PageSpan> = spans(u64::MAX - 1, 1).expect("ends at u64::MAX").take(4).collect();
        assert_eq!(last, vec![PageSpan { index: u64::MAX / PAGE, in_page: PAGE_SIZE - 2, buf: 0..1 }]);
        assert_eq!(spans(u64::MAX, 0).expect("empty").count(), 0);
    }

    #[test]
    fn spans_of_some_ranges() {
        // `take`: spans that stop advancing fail here rather than hang.
        let s = |off, len| spans(off, len).expect("in range").take(8).collect::<Vec<_>>();
        assert_eq!(s(10, 0), vec![]);
        assert_eq!(s(10, 5), vec![PageSpan { index: 0, in_page: 10, buf: 0..5 }]);
        assert_eq!(
            s(PAGE - 1, 2),
            vec![
                PageSpan { index: 0, in_page: PAGE_SIZE - 1, buf: 0..1 },
                PageSpan { index: 1, in_page: 0, buf: 1..2 }
            ]
        );
        assert_eq!(
            s(2 * PAGE, PAGE_SIZE + 3),
            vec![
                PageSpan { index: 2, in_page: 0, buf: 0..PAGE_SIZE },
                PageSpan { index: 3, in_page: 0, buf: PAGE_SIZE..PAGE_SIZE + 3 }
            ]
        );
    }

    #[test]
    fn pages_in_counts_partial_pages() {
        assert_eq!(pages_in(0), 0);
        assert_eq!(pages_in(1), 1);
        assert_eq!(pages_in(PAGE - 1), 1);
        assert_eq!(pages_in(PAGE), 1);
        assert_eq!(pages_in(PAGE + 1), 2);
        assert_eq!(pages_in(u64::MAX), u64::MAX / PAGE + 1);
    }

    #[test]
    fn valid_len_matches_a_byte_by_byte_model() {
        // The bytes of page `index` below `size`, counted one by one.
        let model = |index: u64, size: u64| {
            (0..PAGE)
                .filter(|&b| index.checked_mul(PAGE).and_then(|s| s.checked_add(b)).is_some_and(|at| at < size))
                .count()
        };
        let sizes: Vec<u64> = offsets();
        for index in [0u64, 1, 2, 7, 8, u64::MAX / PAGE - 1, u64::MAX / PAGE, u64::MAX / PAGE + 1, u64::MAX] {
            for &size in &sizes {
                assert_eq!(valid_len(index, size), model(index, size), "page {index} of {size}");
            }
        }
        assert_eq!(valid_len(0, 0), 0);
        assert_eq!(valid_len(0, 100), 100);
        assert_eq!(valid_len(1, 5000), 5000 - PAGE_SIZE);
        assert_eq!(valid_len(1, 3 * PAGE), PAGE_SIZE);
        assert_eq!(valid_len(3, 3 * PAGE), 0);
    }

    /// The bytes past a file's end that a size change from `old` to `new`
    /// must zero, byte by byte: every byte from the smaller end to the end of
    /// its page, when the size changes.
    fn model_tail(old: u64, new: u64) -> Vec<(u64, usize)> {
        if old == new {
            return Vec::new();
        }
        let end = old.min(new);
        let page_end = end.div_ceil(PAGE) * PAGE;
        (end..page_end).map(|b| (b / PAGE, (b % PAGE) as usize)).collect()
    }

    #[test]
    fn resize_tail_matches_a_byte_by_byte_model() {
        let sizes: Vec<u64> = offsets().into_iter().filter(|&o| o < u64::MAX - 2 * PAGE).collect();
        for &old in &sizes {
            for &new in &sizes {
                let got: Vec<(u64, usize)> = match resize_tail(old, new) {
                    None => Vec::new(),
                    Some(t) => {
                        assert!(t.from > 0 && t.from < PAGE_SIZE, "{old} -> {new}: {t:?}");
                        (t.from..PAGE_SIZE).map(|b| (t.index, b)).collect()
                    }
                };
                assert_eq!(got, model_tail(old, new), "{old} -> {new}");
            }
        }
    }

    #[test]
    fn resize_tail_of_some_changes() {
        assert_eq!(resize_tail(100, 100), None);
        assert_eq!(resize_tail(100, 50), Some(Tail { index: 0, from: 50 }));
        assert_eq!(resize_tail(100, 5000), Some(Tail { index: 0, from: 100 }));
        assert_eq!(resize_tail(PAGE, 5000), None); // the old end on a boundary
        assert_eq!(resize_tail(5000, PAGE), None); // the new end on one
        assert_eq!(resize_tail(PAGE + 7, 0), None);
        assert_eq!(resize_tail(3 * PAGE + 1, PAGE + 9), Some(Tail { index: 1, from: 9 }));
    }

    #[test]
    fn short_count_is_the_bytes_before_the_first_failure() {
        // Every range and every page in it failing, alone or with all after
        // it: the bytes of the spans before the failing one, the error when
        // it is the first, the whole range when none fails; and no span runs
        // after a failure (a failing middle page never lets a later one count).
        for offset in offsets().into_iter().filter(|&o| o < u64::MAX - 4 * PAGE) {
            for len in lens() {
                let all: Vec<PageSpan> = spans(offset, len).unwrap().collect();
                assert_eq!(short_count(all.clone(), |_| Ok::<(), ()>(())), Ok(len), "{offset}+{len}");
                for bad in 0..all.len() {
                    for later_fail in [false, true] {
                        let mut ran = Vec::new();
                        let got = short_count(all.clone(), |s| {
                            ran.push(s.index());
                            let i = s.index() - all[0].index();
                            if i as usize == bad || (later_fail && i as usize > bad) { Err(i) } else { Ok(()) }
                        });
                        let want = if bad == 0 { Err(0) } else { Ok(all[bad].buf().start) };
                        assert_eq!(got, want, "{offset}+{len}, page {bad} failing");
                        let indices: Vec<u64> = all[..=bad].iter().map(|s| s.index()).collect();
                        assert_eq!(ran, indices, "{offset}+{len}, page {bad} failing");
                    }
                }
            }
        }
    }
}
