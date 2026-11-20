//! A view across a line, bounded so drawing and measuring cost no more than
//! what it can show (#203: the host charges `text_draw` and `text_measure`
//! by the character, whatever reaches the clip).
//!
//! The view is scrolled across by characters: every line shows from the
//! same column, `first`, and at most `max_chars` characters of it (the
//! view's width over the narrowest glyph, so they always reach its right
//! edge). With a proportional font the columns don't line up across lines
//! once scrolled; the cursor's line always shows its cursor.

/// `line`'s characters from char `first` on: at most `max_chars` of them
/// and `max_bytes` bytes (cut on a char boundary).
pub fn slice(line: &str, first: usize, max_chars: usize, max_bytes: usize) -> &str {
    let start = line.char_indices().nth(first).map_or(line.len(), |(i, _)| i);
    let rest = &line[start..];
    let mut end = rest.char_indices().nth(max_chars).map_or(rest.len(), |(i, _)| i).min(max_bytes);
    while !rest.is_char_boundary(end) {
        end -= 1;
    }
    &rest[..end]
}

/// Where a view across a line starts, and the cursor's x in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Across {
    /// The first char shown.
    pub first: usize,
    /// The cursor's x from the view's start (`measure` of the chars from
    /// `first` to it).
    pub x: u32,
}

/// The view across `line` scrolled so its cursor, at char `col`, shows
/// within `width` pixels: from `first` if it does there, else with the
/// cursor half the view's characters in (or fewer, until it fits), never
/// before the line's start. `max_chars` is the most characters the view
/// shows: every text handed to `measure` is at most that many, from the
/// view's start to the cursor.
pub fn across(line: &str, col: usize, first: usize, max_chars: usize, width: u32, mut measure: impl FnMut(&str) -> u32) -> Across {
    let max = max_chars.max(1);
    let mut fits = |first: usize| -> Option<u32> {
        if col < first || col - first > max {
            return None;
        }
        let x = measure(slice(line, first, col - first, usize::MAX));
        (x <= width).then_some(x)
    };
    if let Some(x) = fits(first) {
        return Across { first, x };
    }
    let mut back = (max / 2).min(col);
    loop {
        if let Some(x) = fits(col - back) {
            return Across { first: col - back, x };
        }
        if back == 0 {
            // Nothing before the cursor shows: it is at the view's start.
            return Across { first: col, x: 0 };
        }
        back /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn a_slice_is_bounded_by_chars_and_bytes() {
        assert_eq!(slice("hello world", 0, 5, 100), "hello");
        assert_eq!(slice("hello world", 6, 100, 100), "world");
        assert_eq!(slice("hello", 9, 3, 100), "", "past the end");
        assert_eq!(slice("héllo", 1, 3, 100), "éll");
        // Bytes: 'é' is two, cut on a boundary.
        assert_eq!(slice("éééé", 0, 4, 5), "éé");
        assert_eq!(slice("aéb", 0, 3, 2), "a");
        assert_eq!(slice("", 0, 3, 3), "");
    }

    /// A measure of 10 pixels a char ('W' 30), recording what it measured.
    fn measure(log: &mut Vec<usize>) -> impl FnMut(&str) -> u32 + '_ {
        move |t: &str| {
            log.push(t.chars().count());
            t.chars().map(|c| if c == 'W' { 30 } else { 10 }).sum()
        }
    }

    #[test]
    fn the_view_stays_while_the_cursor_shows_in_it() {
        let line = "a".repeat(100);
        let mut log = Vec::new();
        // 20 chars in a 200-pixel view: the cursor at 15 shows from 0.
        assert_eq!(across(&line, 15, 0, 20, 200, measure(&mut log)), Across { first: 0, x: 150 });
        // At the edge exactly.
        assert_eq!(across(&line, 20, 0, 20, 200, measure(&mut log)), Across { first: 0, x: 200 });
        // Scrolled, and the cursor still in view.
        assert_eq!(across(&line, 45, 40, 20, 200, measure(&mut log)), Across { first: 40, x: 50 });
        assert!(log.iter().all(|&n| n <= 20), "{log:?}");
    }

    #[test]
    fn past_either_edge_the_cursor_comes_half_the_view_in() {
        let line = "a".repeat(100);
        let mut log = Vec::new();
        // Right of the view: the cursor half the view's chars in.
        assert_eq!(across(&line, 21, 0, 20, 200, measure(&mut log)), Across { first: 11, x: 100 });
        assert_eq!(across(&line, 90, 0, 20, 200, measure(&mut log)), Across { first: 80, x: 100 });
        // Left of it: the same, never before the line's start.
        assert_eq!(across(&line, 50, 60, 20, 200, measure(&mut log)), Across { first: 40, x: 100 });
        assert_eq!(across(&line, 4, 60, 20, 200, measure(&mut log)), Across { first: 0, x: 40 });
        assert_eq!(across(&line, 0, 60, 20, 200, measure(&mut log)), Across { first: 0, x: 0 });
        // Every text measured is at most the view's characters, however far
        // the cursor is along the line.
        assert!(log.iter().all(|&n| n <= 20), "{log:?}");
    }

    #[test]
    fn wide_characters_bring_the_cursor_nearer_the_view_s_start() {
        // 'W's are three times as wide: half the view's chars (10, 300
        // pixels) don't fit 200, five (150) do.
        let line = "W".repeat(100);
        let mut log = Vec::new();
        assert_eq!(across(&line, 50, 0, 20, 200, measure(&mut log)), Across { first: 45, x: 150 });
        // In view by chars but not by pixels: scrolled too.
        assert_eq!(across(&line, 10, 0, 20, 200, measure(&mut log)), Across { first: 5, x: 150 });
        // A view narrower than one glyph: the cursor at its start.
        assert_eq!(across(&line, 10, 0, 20, 20, measure(&mut log)), Across { first: 10, x: 0 });
        assert!(log.iter().all(|&n| n <= 20), "{log:?}");
    }
}
