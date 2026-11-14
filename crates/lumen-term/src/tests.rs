extern crate std;

use super::*;
use std::string::String;
use std::vec::Vec;

/// Row `row` of the view as text (spacers skipped, trailing blanks trimmed).
fn line(t: &Terminal, row: usize) -> String {
    let s: String =
        (0..t.cols()).map(|c| t.cell(c, row)).filter(|c| !c.flags.contains(Flags::SPACER)).map(|c| c.ch).collect();
    s.trim_end().into()
}

fn lines(t: &Terminal) -> Vec<String> {
    (0..t.rows()).map(|r| line(t, r)).collect()
}

fn term(cols: usize, rows: usize, input: &str) -> Terminal {
    let mut t = Terminal::new(cols, rows, 100);
    t.write(input.as_bytes());
    t
}

fn at(t: &Terminal) -> (usize, usize) {
    let c = t.cursor();
    (c.col, c.row)
}

/// The grid's invariants, whatever was written: the cursor and the region
/// are inside the screen, the view inside the scrollback, and every wide
/// character has both halves.
fn check_invariants(t: &Terminal) {
    let g = &t.grid;
    assert!(g.col < g.cols && g.row < g.rows, "cursor ({}, {})", g.col, g.row);
    assert!(g.top < g.bottom && g.bottom <= g.rows, "region {}..{}", g.top, g.bottom);
    assert!(g.len <= g.capacity && g.view <= g.len && g.head < g.capacity.max(1));
    assert!(!g.wrap_pending || g.col == g.cols - 1);
    for r in 0..g.rows {
        for c in 0..g.cols {
            let cell = g.at(c, r);
            assert!(
                !(cell.flags.contains(Flags::WIDE) && cell.flags.contains(Flags::SPACER)),
                "both halves at ({c}, {r})"
            );
            if cell.flags.contains(Flags::WIDE) {
                assert!(c + 1 < g.cols && g.at(c + 1, r).flags.contains(Flags::SPACER), "lone wide at ({c}, {r})");
            }
            if cell.flags.contains(Flags::SPACER) {
                assert!(c > 0 && g.at(c - 1, r).flags.contains(Flags::WIDE), "lone spacer at ({c}, {r})");
            }
        }
    }
    if let Some(s) = g.scroll {
        assert!(s.top < s.bottom && s.bottom <= g.rows && s.lines != 0 && s.lines.unsigned_abs() < s.bottom - s.top);
    }
}

#[test]
fn text_lands_in_the_grid() {
    let t = term(10, 3, "ab\r\ncd\r\n");
    assert_eq!(lines(&t), ["ab", "cd", ""]);
    assert_eq!(at(&t), (0, 2));
    // A line feed alone keeps the column.
    let t = term(10, 3, "ab\ncd");
    assert_eq!(lines(&t), ["ab", "  cd", ""]);
}

#[test]
fn autowrap_waits_at_the_last_column() {
    let mut t = term(4, 3, "abcd");
    assert_eq!(at(&t), (3, 0));
    assert_eq!(lines(&t), ["abcd", "", ""]);
    t.write(b"e");
    assert_eq!(lines(&t), ["abcd", "e", ""]);
    assert_eq!(at(&t), (1, 1));
    // A carriage return cancels the wrap.
    let t = term(4, 3, "abcd\rX");
    assert_eq!(lines(&t), ["Xbcd", "", ""]);
    // Without autowrap the last column is overwritten.
    let t = term(4, 3, "\x1b[?7labcdef");
    assert_eq!(lines(&t), ["abcf", "", ""]);
    assert_eq!(at(&t), (3, 0));
}

#[test]
fn sgr_sets_colours_and_attributes() {
    let t = term(
        20,
        2,
        "\x1b[31ma\x1b[91;44mb\x1b[38;5;200;48:5:17mc\x1b[38;2;255;0;0md\x1b[1;7me\x1b[0mf\x1b[32m\x1b[39mg",
    );
    let c = |i| t.cell(i, 0);
    assert_eq!((c(0).fg, c(0).bg), (Color::Indexed(1), Color::Default));
    assert_eq!((c(1).fg, c(1).bg), (Color::Indexed(9), Color::Indexed(4)));
    assert_eq!((c(2).fg, c(2).bg), (Color::Indexed(200), Color::Indexed(17)));
    assert_eq!(c(3).fg, Color::Indexed(196));
    assert!(c(4).flags.contains(Flags::BOLD) && c(4).flags.contains(Flags::INVERSE));
    assert_eq!(c(5), Cell { ch: 'f', ..Cell::BLANK });
    assert_eq!(c(6).fg, Color::Default);
    // Colon forms of truecolor, with and without the colour space id, and
    // `ESC [ m` alone resets.
    let t = term(20, 1, "\x1b[38:2::0:0:255ma\x1b[48:2:0:255:0mb\x1b[1;31m\x1b[mc\x1b[22;27md");
    assert_eq!(t.cell(0, 0).fg, Color::Indexed(21));
    assert_eq!(t.cell(1, 0).bg, Color::Indexed(46));
    assert_eq!(t.cell(2, 0), Cell { ch: 'c', ..Cell::BLANK });
}

#[test]
fn cursor_moves_and_clamps() {
    let mut t = term(10, 5, "\x1b[3;4H");
    assert_eq!(at(&t), (3, 2));
    t.write(b"\x1b[A");
    assert_eq!(at(&t), (3, 1));
    t.write(b"\x1b[99B\x1b[2C");
    assert_eq!(at(&t), (5, 4));
    t.write(b"\x1b[99D\x1b[99A");
    assert_eq!(at(&t), (0, 0));
    t.write(b"\x1b[99;99f");
    assert_eq!(at(&t), (9, 4));
    t.write(b"\x1b[3G\x1b[2d");
    assert_eq!(at(&t), (2, 1));
    t.write(b"\x1b[2E");
    assert_eq!(at(&t), (0, 3));
    t.write(b"\x1b[5C\x1b[F");
    assert_eq!(at(&t), (0, 2));
    t.write(b"\x1b[0;0H\x1b[H");
    assert_eq!(at(&t), (0, 0));
    t.write(b"\x08\x08x\x08");
    assert_eq!(at(&t), (0, 0));
}

#[test]
fn save_and_restore_keep_position_and_pen() {
    let mut t = term(10, 5, "\x1b[2;3H\x1b[31m\x1b7\x1b[H\x1b[0mA\x1b8B");
    assert_eq!(t.cell(2, 1), Cell { ch: 'B', fg: Color::Indexed(1), ..Cell::BLANK });
    assert_eq!(t.cell(0, 0), Cell { ch: 'A', ..Cell::BLANK });
    t.write(b"\x1b[4;5H\x1b[s\x1b[H\x1b[u");
    assert_eq!(at(&t), (4, 3));
    // Restoring with nothing saved goes home with the default pen.
    let t = term(10, 5, "\x1b[3;3H\x1b[31m\x1b8x");
    assert_eq!(t.cell(0, 0), Cell { ch: 'x', ..Cell::BLANK });
}

#[test]
fn erasing_uses_the_background_colour() {
    let fill = "abcde\r\nfghij\r\nklmno";
    let mut t = term(5, 3, fill);
    t.write(b"\x1b[2;3H\x1b[K");
    assert_eq!(lines(&t), ["abcde", "fg", "klmno"]);
    t.write(b"\x1b[1K");
    assert_eq!(lines(&t), ["abcde", "", "klmno"]);
    let mut t = term(5, 3, fill);
    t.write(b"\x1b[2;3H\x1b[44m\x1b[J");
    assert_eq!(lines(&t), ["abcde", "fg", ""]);
    assert_eq!(t.cell(2, 1), Cell::blank(Color::Indexed(4)));
    assert_eq!(t.cell(0, 2), Cell::blank(Color::Indexed(4)));
    let mut t = term(5, 3, fill);
    t.write(b"\x1b[2;3H\x1b[1J");
    assert_eq!(lines(&t), ["", "   ij", "klmno"]);
    t.write(b"\x1b[2J");
    assert_eq!(lines(&t), ["", "", ""]);
    let mut t = term(5, 3, fill);
    t.write(b"\x1b[1;2H\x1b[2X\x1b[3;1H\x1b[2K");
    assert_eq!(lines(&t), ["a  de", "fghij", ""]);
    // Erasing keeps the cursor where it was.
    assert_eq!(at(&t), (0, 2));
}

#[test]
fn inserting_and_deleting_cells_and_lines() {
    let mut t = term(6, 4, "abcdef");
    t.write(b"\x1b[1;3H\x1b[2@");
    assert_eq!(line(&t, 0), "ab  cd");
    t.write(b"\x1b[3P");
    assert_eq!(line(&t, 0), "abd");
    t.write(b"\x1b[99P");
    assert_eq!(line(&t, 0), "ab");
    let mut t = term(3, 4, "a\r\nb\r\nc\r\nd");
    t.write(b"\x1b[2;2H\x1b[L");
    assert_eq!(lines(&t), ["a", "", "b", "c"]);
    assert_eq!(at(&t), (0, 1));
    t.write(b"\x1b[2M");
    assert_eq!(lines(&t), ["a", "c", "", ""]);
    // Lines inserted and deleted never go to the scrollback, not even
    // deleted from the top row.
    t.write(b"\x1b[H\x1b[M");
    assert_eq!(lines(&t), ["c", "", "", ""]);
    assert_eq!(t.scrollback_len(), 0);
}

#[test]
fn lines_scrolled_off_the_screen_go_to_the_scrollback() {
    let mut t = term(4, 3, "1\r\n2\r\n3\r\n4\r\n5");
    assert_eq!(lines(&t), ["3", "4", "5"]);
    assert_eq!(t.scrollback_len(), 2);
    // Scroll up and down (SU, SD) and reverse index at the top.
    t.write(b"\x1b[S");
    assert_eq!(lines(&t), ["4", "5", ""]);
    assert_eq!(t.scrollback_len(), 3);
    t.write(b"\x1b[T");
    assert_eq!(lines(&t), ["", "4", "5"]);
    t.write(b"\x1b[H\x1bM");
    assert_eq!(lines(&t), ["", "", "4"]);
    assert_eq!(t.scrollback_len(), 3);
    // ESC D and ESC E index like a line feed.
    let t = term(4, 2, "a\x1bDb\x1bEc");
    assert_eq!(lines(&t), [" b", "c"]);
    assert_eq!(t.scrollback_len(), 1);
}

#[test]
fn a_scroll_region_scrolls_alone_and_keeps_no_history() {
    let mut t = term(4, 5, "a\r\nb\r\nc\r\nd\r\ne");
    t.write(b"\x1b[2;4r");
    assert_eq!(at(&t), (0, 0));
    t.write(b"\x1b[4;1H\nX");
    assert_eq!(lines(&t), ["a", "c", "d", "X", "e"]);
    assert_eq!(t.scrollback_len(), 0);
    t.write(b"\x1b[2;1H\x1bM");
    assert_eq!(lines(&t), ["a", "", "c", "d", "e"]);
    // Below the region a line feed stops at the last row.
    t.write(b"\x1b[5;1H\n\n");
    assert_eq!(at(&t), (0, 4));
    assert_eq!(lines(&t), ["a", "", "c", "d", "e"]);
    // Cursor-up stops at the region's top when inside it.
    t.write(b"\x1b[3;1H\x1b[9A");
    assert_eq!(at(&t), (0, 1));
    // A region under two lines is refused; `ESC [ r` resets it.
    t.write(b"\x1b[3;3r");
    assert_eq!((t.grid.top, t.grid.bottom), (1, 4));
    t.write(b"\x1b[r");
    assert_eq!((t.grid.top, t.grid.bottom), (0, 5));
}

#[test]
fn the_scrollback_is_a_ring_and_the_view_scrolls_through_it() {
    let mut t = Terminal::new(4, 2, 3);
    for i in 0..7 {
        t.write(std::format!("{i}\r\n").as_bytes());
    }
    // Lines 0 to 5 scrolled off; the ring kept the newest 3.
    assert_eq!(lines(&t), ["6", ""]);
    assert_eq!(t.scrollback_len(), 3);
    assert!(t.scroll_view(1));
    assert_eq!(lines(&t), ["5", "6"]);
    assert_eq!(t.cursor_in_view(), None);
    assert!(t.scroll_view(99));
    assert_eq!(t.view_offset(), 3);
    assert_eq!(lines(&t), ["3", "4"]);
    assert!(!t.scroll_view(1));
    // New output: the view stays on the lines it shows while the ring has
    // them (here the oldest goes, so it shows the next).
    t.write(b"x\r\n");
    assert_eq!(lines(&t), ["4", "5"]);
    t.scroll_view(-1);
    t.write(b"y\r\n");
    assert_eq!(lines(&t), ["5", "6"]);
    assert_eq!(t.view_offset(), 3);
    assert!(t.reset_view());
    assert_eq!(lines(&t), ["y", ""]);
    assert_eq!(t.cursor_in_view(), Some((0, 1)));
    assert!(!t.reset_view());
    // ED 3 clears it (and shows the screen).
    t.scroll_view(2);
    t.write(b"\x1b[3J");
    assert_eq!((t.scrollback_len(), t.view_offset()), (0, 0));
    // A view into a ring that isn't full yet keeps its lines too.
    let mut t = Terminal::new(4, 2, 10);
    t.write(b"a\r\nb\r\nc\r\n");
    t.scroll_view(1);
    assert_eq!(lines(&t), ["b", "c"]);
    t.write(b"d\r\ne\r\n");
    assert_eq!(lines(&t), ["b", "c"]);
    assert_eq!(t.view_offset(), 3);
    // No scrollback at all.
    let mut t = Terminal::new(4, 2, 0);
    t.write(b"a\r\nb\r\nc");
    assert_eq!((lines(&t), t.scrollback_len()), (std::vec!["b".into(), "c".into()], 0));
    assert!(!t.scroll_view(1));
}

#[test]
fn tabs_stop_every_8_columns_and_can_be_set_and_cleared() {
    let mut t = term(20, 1, "\ta\tb\tc");
    assert_eq!(line(&t, 0), "        a       b  c");
    t.write(b"\r\x1b[3g\t");
    assert_eq!(at(&t), (19, 0));
    t.write(b"\x1b[5G\x1bH\r\t");
    assert_eq!(at(&t), (4, 0));
    t.write(b"\x1b[g\r\t");
    assert_eq!(at(&t), (19, 0));
}

#[test]
fn wide_characters_take_two_cells() {
    let t = term(6, 2, "a\u{4e00}b");
    assert_eq!(t.cell(1, 0).ch, '\u{4e00}');
    assert!(t.cell(1, 0).flags.contains(Flags::WIDE) && t.cell(2, 0).flags.contains(Flags::SPACER));
    assert_eq!(line(&t, 0), "a\u{4e00}b");
    assert_eq!(at(&t), (4, 0));
    // No room at the last column: it is left blank and the character wraps.
    let t = term(4, 2, "abc\u{ff21}");
    assert_eq!(lines(&t), ["abc", "\u{ff21}"]);
    // Overwriting either half blanks the other.
    let t = term(6, 1, "\u{4e00}\u{4e00}\x1b[1GX\x1b[4GY");
    assert_eq!(line(&t, 0), "X  Y");
    check_invariants(&t);
    // Deleting a cell splits a pair: the lone half goes.
    let t = term(6, 1, "a\u{4e00}b\x1b[1;2H\x1b[P");
    assert_eq!(line(&t, 0), "a b");
    check_invariants(&t);
    // Zero-width characters (a combining accent) are dropped; ambiguous ones
    // are narrow.
    let t = term(6, 1, "e\u{301}\u{a1}x");
    assert_eq!(line(&t, 0), "e\u{a1}x");
    // A wide character never fits a 1-column terminal.
    let t = term(1, 1, "\u{4e00}");
    assert_eq!(line(&t, 0), "");
}

#[test]
fn cursor_visibility_and_reset() {
    let mut t = term(4, 2, "\x1b[?25l");
    assert!(!t.cursor().visible);
    assert_eq!(t.cursor_in_view(), None);
    t.write(b"\x1b[?25h\x1b[31;44mab\x1b[2;3r\x1bc");
    assert!(t.cursor().visible);
    assert_eq!(lines(&t), ["", ""]);
    assert_eq!(at(&t), (0, 0));
    t.write(b"x");
    assert_eq!(t.cell(0, 0), Cell { ch: 'x', ..Cell::BLANK });
    assert_eq!((t.grid.top, t.grid.bottom), (0, 2));
}

#[test]
fn malformed_and_unknown_sequences_are_ignored() {
    let t = term(
        10,
        3,
        concat!(
            "\x1b[99999999999999999999A", // overflowing parameter
            "\x1b[38;5m",                 // 256 colour without its index
            "\x1b[38;2;1m",               // truecolor cut short
            "\x1b[38;5;300m",             // index out of range
            "\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33m", // too many
            "\x1b]0;a title\x07",                                                                              // OSC
            "\x1bP1$r\x1b\\",                                                                                  // DCS
            "\x1b[>c\x1b[=5u\x1b(B\x1b#8", // intermediates and private markers
            "\x1b[?1049h\x1b[4h",          // unsupported modes
            "\x1b[Z\x1b[999q",             // unknown finals
            "ok"
        ),
    );
    assert_eq!(lines(&t)[0].trim_start(), "ok");
    check_invariants(&t);
    // Invalid UTF-8 prints a replacement character; a sequence split across
    // writes still works.
    let mut t = term(10, 1, "a\u{0}");
    t.write(b"\xffb\x1b[");
    t.write(b"31mc");
    assert_eq!(line(&t, 0), "a\u{fffd}bc");
    assert_eq!(t.cell(3, 0).fg, Color::Indexed(1));
}

/// A tiny deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_input_keeps_the_grid_consistent() {
    // Random bytes biased towards escapes, digits and wide characters, in
    // small and odd-sized terminals, checking the invariants after every
    // write, and that a renderer drawing only the damage shows the view.
    const PIECES: &[&str] = &[
        "\x1b[",
        "\x1b",
        ";",
        "?",
        "\x1b[?25l",
        "\x1b[?7l",
        "\x1b[?7h",
        "\r",
        "\n",
        "\t",
        "\x08",
        "\u{4e00}",
        "\u{301}",
        "a",
        "Z",
        " ",
        "m",
        "H",
        "J",
        "K",
        "@",
        "P",
        "L",
        "M",
        "S",
        "T",
        "r",
        "s",
        "u",
        "g",
        "X",
        "A",
        "B",
        "C",
        "D",
        "E",
        "F",
        "G",
        "d",
        "f",
        "7",
        "8",
        "c",
        "38;5;",
        "48;2;",
        "\x1bM",
        "\x1bD",
        "\x1bH",
        "\x1b]",
        "\x07",
        "\x1b[3J",
        "\x1b[2;3r",
        "\x1b[r",
    ];
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for case in 0..2000 {
        let (cols, rows) = (1 + rng.below(9) as usize, 1 + rng.below(6) as usize);
        let mut t = Terminal::new(cols, rows, rng.below(5) as usize);
        let mut shadow = Shadow::new(&t);
        for _ in 0..60 {
            let mut chunk = Vec::new();
            for _ in 0..rng.below(12) {
                match rng.below(4) {
                    0 => chunk.push(rng.below(256) as u8),
                    1 => chunk.extend(std::format!("{}", rng.below(30)).bytes()),
                    _ => chunk.extend(PIECES[rng.below(PIECES.len() as u64) as usize].bytes()),
                }
            }
            t.write(&chunk);
            if rng.below(8) == 0 {
                t.scroll_view(rng.below(7) as isize - 3);
            }
            check_invariants(&t);
            shadow.draw(&mut t);
            for r in 0..rows {
                for c in 0..cols {
                    assert_eq!(shadow.cells[r * cols + c], t.cell(c, r), "case {case}: ({c}, {r}) not redrawn");
                }
            }
        }
    }
}

/// What a renderer drew: it moves what it drew for a pending scroll
/// (leaving the rows brought in as they were), then draws the dirty rows,
/// and nothing else.
struct Shadow {
    cols: usize,
    cells: Vec<Cell>,
}

impl Shadow {
    fn new(t: &Terminal) -> Shadow {
        // Not what a blank terminal shows: a new terminal must be all dirty.
        Shadow { cols: t.cols(), cells: std::vec![Cell { ch: '#', ..Cell::BLANK }; t.cols() * t.rows()] }
    }

    fn draw(&mut self, t: &mut Terminal) {
        let c = self.cols;
        if let Some(s) = t.take_scroll() {
            let n = s.lines.unsigned_abs();
            if s.lines > 0 {
                self.cells.copy_within((s.top + n) * c..s.bottom * c, s.top * c);
            } else {
                self.cells.copy_within(s.top * c..(s.bottom - n) * c, (s.top + n) * c);
            }
        }
        for r in 0..t.rows() {
            if t.take_dirty(r) {
                for col in 0..c {
                    self.cells[r * c + col] = t.cell(col, r);
                }
            }
        }
    }
}

#[test]
fn damage_marks_changed_rows_and_records_scrolls() {
    let mut t = Terminal::new(4, 4, 10);
    let take = |t: &mut Terminal| -> (Option<Scroll>, Vec<bool>) {
        (t.take_scroll(), (0..t.rows()).map(|r| t.take_dirty(r)).collect())
    };
    // A new terminal is all dirty.
    assert_eq!(take(&mut t), (None, std::vec![true; 4]));
    t.write(b"\x1b[3;1Hx");
    assert_eq!(take(&mut t), (None, std::vec![false, false, true, false]));
    assert_eq!(take(&mut t), (None, std::vec![false; 4]));
    // Row 2 changes, then two line feeds at the bottom scroll it to row 0:
    // its mark moves with it, the two new rows are dirty, and the scrolls
    // merge into one.
    t.write(b"y\x1b[4;1H\n\n");
    assert_eq!(take(&mut t), (Some(Scroll { top: 0, bottom: 4, lines: 2 }), std::vec![true, false, true, true]));
    // A scroll in another region can't merge: both regions are redrawn.
    t.write(b"\n\x1b[2;3r\x1b[3;1H\n");
    assert_eq!(take(&mut t), (None, std::vec![true; 4]));
    // Scrolling a whole region's height or more is drawn cell by cell.
    t.write(b"\x1b[r\x1b[4S");
    assert_eq!(take(&mut t), (None, std::vec![true; 4]));
    t.write(b"\x1b[2S");
    t.write(b"\x1b[2S");
    assert_eq!(take(&mut t), (None, std::vec![true; 4]));
    // Moving the view redraws it.
    t.scroll_view(1);
    assert_eq!(take(&mut t), (None, std::vec![true; 4]));
    t.reset_view();
    assert_eq!(take(&mut t).1, std::vec![true; 4]);
    // The cursor moving changes no cells.
    t.write(b"\x1b[2;2H\x1b[A");
    assert_eq!(take(&mut t), (None, std::vec![false; 4]));
}

#[test]
fn reverse_index_moves_up_away_from_the_top() {
    let mut t = term(4, 4, "a\r\nb\r\nc");
    t.write(b"\x1b[3;2H\x1bM");
    assert_eq!(at(&t), (1, 1));
    assert_eq!(lines(&t), ["a", "b", "c", ""]);
}

#[test]
fn operations_inside_and_outside_a_scroll_region() {
    let fill = "a\r\nb\r\nc\r\nd\r\ne\r\nf";
    // SU and SD scroll the region only.
    let mut t = term(4, 6, fill);
    t.write(b"\x1b[2;4r\x1b[S");
    assert_eq!(lines(&t), ["a", "c", "d", "", "e", "f"]);
    t.write(b"\x1b[2T");
    assert_eq!(lines(&t), ["a", "", "", "c", "e", "f"]);
    assert_eq!(t.scrollback_len(), 0);
    // Cursor down, next line and previous line stop at the region's edges
    // inside it.
    t.write(b"\x1b[3;2H\x1b[9B");
    assert_eq!(at(&t), (1, 3));
    t.write(b"\x1b[2;1H\x1b[9E");
    assert_eq!(at(&t), (0, 3));
    t.write(b"\x1b[9F");
    assert_eq!(at(&t), (0, 1));
    // Below the region they use the screen: down moves down, and so does a
    // line feed above the last row.
    t.write(b"\x1b[5;1H\x1b[B");
    assert_eq!(at(&t), (0, 5));
    t.write(b"\x1b[5;1H\n");
    assert_eq!(at(&t), (0, 5));
    assert_eq!(lines(&t), ["a", "", "", "c", "e", "f"]);
    // Inserting or deleting lines outside the region does nothing.
    let mut t = term(4, 6, fill);
    t.write(b"\x1b[3;5r\x1b[1;1H\x1b[L\x1b[6;1H\x1b[M");
    assert_eq!(lines(&t), ["a", "b", "c", "d", "e", "f"]);
    // A region from the top row that isn't the whole screen (a status line
    // below it) keeps no history.
    let mut t = term(4, 4, "a\r\nb\r\nc\r\nz");
    t.write(b"\x1b[1;3r\x1b[3;1H\n\n");
    assert_eq!(lines(&t), ["c", "", "", "z"]);
    assert_eq!(t.scrollback_len(), 0);
}

#[test]
fn erase_to_the_cursor_includes_its_column() {
    let mut t = term(5, 1, "abcde");
    t.write(b"\x1b[3G\x1b[1K");
    assert_eq!(line(&t, 0), "   de");
}

#[test]
fn bright_backgrounds_and_attribute_resets() {
    let t = term(10, 1, "\x1b[100ma\x1b[107mb\x1b[44;49mc\x1b[1;7m\x1b[22md\x1b[1;7m\x1b[27me");
    assert_eq!(t.cell(0, 0).bg, Color::Indexed(8));
    assert_eq!(t.cell(1, 0).bg, Color::Indexed(15));
    assert_eq!(t.cell(2, 0).bg, Color::Default);
    assert_eq!(t.cell(3, 0).flags, Flags::INVERSE);
    assert_eq!(t.cell(4, 0).flags, Flags::BOLD);
    // Truecolor in both forms, each channel on its own (the nearest cube
    // entry: 255 is level 5, 135 level 2, 0 level 0; 95 level 1, 215 level 4).
    let t = term(4, 1, "\x1b[38:2::255:135:0ma\x1b[48;2;0;95;215mb");
    assert_eq!(t.cell(0, 0).fg, Color::Indexed(16 + 36 * 5 + 6 * 2));
    assert_eq!(t.cell(1, 0).bg, Color::Indexed(16 + 6 + 4));
}

#[test]
fn reset_restores_tabs_modes_and_the_saved_cursor() {
    let mut t = term(20, 3, "\x1b[3;5H\x1b7\x1b[3g\x1b[?7l\x1bc");
    // Autowrap is back on.
    t.write(b"\x1b[1;20Hxy");
    assert_eq!(lines(&t)[..2], ["                   x", "y"]);
    // Tab stops are back, and nothing is saved (restore goes home).
    t.write(b"\r\t");
    assert_eq!(at(&t), (8, 1));
    t.write(b"\x1b8");
    assert_eq!(at(&t), (0, 0));
}

#[test]
fn pending_wraps_with_autowrap_and_save() {
    // Turning autowrap off cancels a pending wrap.
    let mut t = term(3, 2, "abc\x1b[?7lX");
    assert_eq!(lines(&t), ["abX", ""]);
    // A saved pending wrap comes back with the cursor.
    t.write(b"\x1b[?7h\x1bc");
    t.write(b"abc\x1b7\x1b[2;1H\x1b8d");
    assert_eq!(lines(&t), ["abc", "d"]);
    // ... but not once autowrap is off.
    let t = term(3, 2, "abc\x1b7\x1b[?7l\x1b8d");
    assert_eq!(lines(&t), ["abd", ""]);
    // A wide character at the last column without autowrap is dropped.
    let t = term(3, 1, "\x1b[?7lab\u{4e00}");
    assert_eq!(line(&t, 0), "ab");
    // A tab clears a pending wrap.
    let t = term(10, 2, "0123456789\tx");
    assert_eq!(lines(&t), ["012345678x", ""]);
    // A wide character that doesn't fit blanks the last column first.
    let t = term(4, 2, "abcd\x1b[1;4H\u{4e00}");
    assert_eq!(lines(&t), ["abc", "\u{4e00}"]);
}

#[test]
fn scrolls_and_inserts_bring_in_the_pen_background() {
    let mut t = term(4, 3, "a\r\nb\r\nc\x1b[44m\n");
    assert_eq!(t.cell(0, 2).bg, Color::Indexed(4));
    t.write(b"\x1b[H\x1b[L");
    assert_eq!(t.cell(3, 0).bg, Color::Indexed(4));
    t.write(b"\x1b[2;1H\x1b[2@");
    assert_eq!(t.cell(0, 1).bg, Color::Indexed(4));
}

#[test]
fn erasing_scrollback_while_scrolled_back_redraws() {
    let mut t = Terminal::new(4, 2, 10);
    t.write(b"a\r\nb\r\nc\r\nd");
    t.scroll_view(2);
    let _ = (t.take_scroll(), (0..2).map(|r| t.take_dirty(r)).count());
    t.write(b"\x1b[3J");
    assert_eq!(t.view_offset(), 0);
    assert!((0..2).all(|r| t.take_dirty(r)));
    assert_eq!(lines(&t), ["c", "d"]);
}

#[test]
fn save_with_parameters_saves_nothing() {
    let mut t = term(10, 3, "\x1b[2;3H\x1b[s\x1b[3;5H\x1b[1;2s\x1b[H\x1b[u");
    assert_eq!(at(&t), (2, 1));
    t.write(b"\x1b[`");
    assert_eq!(at(&t), (0, 1));
}
