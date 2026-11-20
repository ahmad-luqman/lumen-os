//! The editor's commands, its history and its view, then a randomized model:
//! a naive editor over `Vec<char>` gives the text and cursor after every
//! command, and every undo and redo is checked against the text recorded
//! at its depth.

use super::*;

const BIG: usize = 1 << 20;

fn limits(text_bytes: usize, undo_edits: usize) -> Limits {
    Limits { text_bytes, line_bytes: BIG, undo_edits, undo_bytes: BIG }
}

fn ed(text: &str) -> Editor {
    Editor::new(text, limits(BIG, 1000)).0
}

fn typed(e: &mut Editor, s: &str) {
    for c in s.chars() {
        let cmd = match c {
            '\n' => Command::Newline,
            c => Command::Insert(c),
        };
        e.apply(cmd);
    }
}

#[test]
fn typing_inserts_at_the_cursor_and_reports_the_line() {
    let mut e = ed("");
    assert_eq!(e.apply(Command::Insert('h')), Change::Line(0));
    typed(&mut e, "ello");
    assert_eq!(e.text(), "hello");
    assert_eq!(e.line_col(), (0, 5));
    assert_eq!(e.apply(Command::Newline), Change::From(0));
    typed(&mut e, "wörld");
    assert_eq!(e.text(), "hello\nwörld");
    assert_eq!(e.line_col(), (1, 5));
    assert_eq!(e.line_count(), 2);
    assert_eq!(e.line(0), "hello");
    assert_eq!(e.line(1), "wörld");
    assert_eq!(e.line(2), "");
    assert_eq!(e.before_cursor(), "wörld");
    assert!(e.modified());
}

#[test]
fn control_characters_never_enter_the_text_and_tabs_and_line_ends_are_kept() {
    // A tab given becomes the editor's spaces, \r\n and a lone \r a line
    // break; the other control characters are dropped.
    let mut e = ed("a\tb\u{1b}c\r\nd\u{7f}\re");
    assert_eq!(e.text(), "a    bc\nd\ne");
    assert_eq!(e.apply(Command::Insert('\u{1b}')), Change::None);
    assert_eq!(e.apply(Command::Insert('\n')), Change::None);
    assert_eq!(e.apply(Command::Insert('\t')), Change::None);
    assert_eq!(e.text(), "a    bc\nd\ne");
    assert_eq!(e.replace_all("x\ty\u{2028}z\r\n\r\n"), Change::From(0));
    // U+2028 isn't a control character: the editor keeps it (the kernel's
    // sanitisers replace it with U+FFFD wherever it is shown or logged).
    assert_eq!(e.text(), "x    y\u{2028}z\n\n");
    assert_eq!(normalise("\r\r\n\n"), "\n\n\n");
    assert_eq!(normalise(""), "");
}

#[test]
fn new_says_whether_it_cut_the_text() {
    let lim = Limits { text_bytes: 5, line_bytes: 4, undo_edits: 10, undo_bytes: 100 };
    let (e, loaded) = Editor::new("ab\ncd", lim);
    assert_eq!((e.text(), loaded), ("ab\ncd", Loaded { cut: false }));
    let (e, loaded) = Editor::new("ab\ncde", lim);
    assert_eq!((e.text(), loaded), ("ab\ncd", Loaded { cut: true }), "past the text's bytes");
    let (e, loaded) = Editor::new("abcde\nx", lim);
    assert_eq!((e.text(), loaded), ("abcd", Loaded { cut: true }), "past a line's bytes");
    // A tab counts as its spaces.
    let (e, loaded) = Editor::new("a\t", lim);
    assert_eq!((e.text(), loaded), ("a   ", Loaded { cut: true }));
}

#[test]
fn the_status_line_says_where_the_cursor_is_and_what_happened() {
    let mut e = Editor::new("ab\ncd", Limits { text_bytes: 5, line_bytes: 100, undo_edits: 10, undo_bytes: 100 }).0;
    assert_eq!(e.status(), "Ln 1, Col 1");
    e.apply(Command::Down);
    e.apply(Command::End);
    assert_eq!(e.status(), "Ln 2, Col 3");
    e.apply(Command::Backspace);
    assert_eq!(e.status(), "Ln 2, Col 2, modified");
    e.apply(Command::Insert('x'));
    assert!(!e.full());
    assert_eq!(e.apply(Command::Insert('y')), Change::Refused);
    assert!(e.full());
    assert_eq!(e.status(), "Ln 2, Col 3, modified, full");
    // Any command after clears it; undone back to the start, not modified.
    e.apply(Command::Left);
    assert_eq!(e.status(), "Ln 2, Col 2, modified");
    assert_eq!(e.replace_all("1234567"), Change::Refused);
    assert!(e.full(), "a replacement refused too");
    e.apply(Command::Undo);
    e.apply(Command::Undo);
    assert_eq!(e.text(), "ab\ncd");
    assert_eq!(e.status(), "Ln 2, Col 3");
}

#[test]
fn tab_inserts_spaces_as_one_edit() {
    let mut e = ed("x");
    e.apply(Command::Tab);
    assert_eq!(e.text(), "    x");
    assert_eq!(e.undo_depth(), 1);
    e.apply(Command::Undo);
    assert_eq!(e.text(), "x");
}

#[test]
fn backspace_and_delete_join_lines_at_their_ends() {
    let mut e = ed("ab\ncd");
    assert_eq!(e.apply(Command::Backspace), Change::None, "nothing before the start");
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (1, 0));
    assert_eq!(e.apply(Command::Backspace), Change::From(0));
    assert_eq!(e.text(), "abcd");
    assert_eq!(e.line_col(), (0, 2));
    e.apply(Command::Newline);
    e.apply(Command::Left);
    assert_eq!(e.apply(Command::Delete), Change::From(0));
    assert_eq!(e.text(), "abcd");
    e.apply(Command::Bottom);
    assert_eq!(e.apply(Command::Delete), Change::None, "nothing after the end");
    assert_eq!(e.apply(Command::Backspace), Change::Line(0));
    assert_eq!(e.text(), "abc");
}

#[test]
fn moves_stop_at_the_ends_and_cross_lines() {
    let mut e = ed("one\ntwo\nthree");
    assert_eq!(e.apply(Command::Left), Change::None);
    assert_eq!(e.apply(Command::Up), Change::None, "already at the start");
    assert_eq!(e.apply(Command::End), Change::Cursor { from: 0, to: 0 });
    assert_eq!(e.apply(Command::Right), Change::Cursor { from: 0, to: 1 });
    assert_eq!(e.line_col(), (1, 0));
    assert_eq!(e.apply(Command::Left), Change::Cursor { from: 1, to: 0 });
    assert_eq!(e.line_col(), (0, 3), "the previous line's end");
    e.apply(Command::Bottom);
    assert_eq!(e.line_col(), (2, 5));
    assert_eq!(e.apply(Command::Right), Change::None);
    assert_eq!(e.apply(Command::Down), Change::None, "already at the end");
    e.apply(Command::Home);
    assert_eq!(e.line_col(), (2, 0));
    e.apply(Command::Top);
    assert_eq!(e.cursor(), 0);
}

#[test]
fn up_and_down_keep_the_column_moved_to_across() {
    // The cursor at column 5 of a long line keeps aiming at 5 through a
    // short line, landing at its end there.
    let mut e = ed("abcdefg\nab\nabcdefg");
    for _ in 0..5 {
        e.apply(Command::Right);
    }
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (1, 2), "the short line's end");
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (2, 5), "back at the goal");
    e.apply(Command::Up);
    e.apply(Command::Up);
    assert_eq!(e.line_col(), (0, 5));
    // Moving across sets a new goal.
    e.apply(Command::Left);
    e.apply(Command::Down);
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (2, 4));
    // Down on the last line goes to its end, Up on the first to its start.
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (2, 7));
    e.apply(Command::Top);
    e.apply(Command::End);
    e.apply(Command::Up);
    assert_eq!(e.cursor(), 0);
}

#[test]
fn the_cursor_at_a_line_end_stays_there_through_edits_and_moves() {
    // "cursor at line ends" (the issue's exit test): End, typing, Up/Down
    // at the end column, Backspace at a line end.
    let mut e = ed("first line\nsecond\nthird line here");
    e.apply(Command::End);
    assert_eq!(e.line_col(), (0, 10));
    typed(&mut e, "!");
    assert_eq!(e.line(0), "first line!");
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (1, 6), "the shorter line's end");
    e.apply(Command::End);
    assert_eq!(e.line_col(), (1, 6));
    e.apply(Command::Backspace);
    assert_eq!(e.line(1), "secon");
    assert_eq!(e.line_col(), (1, 5));
    e.apply(Command::Down);
    assert_eq!(e.line_col(), (2, 5));
}

#[test]
fn a_run_of_typing_is_one_edit_and_anything_else_ends_it() {
    let mut e = ed("");
    typed(&mut e, "hello");
    assert_eq!(e.undo_depth(), 1);
    e.apply(Command::Insert(' '));
    assert_eq!(e.undo_depth(), 1, "a space joins the run");
    e.apply(Command::Left);
    e.apply(Command::Right);
    typed(&mut e, "you");
    assert_eq!(e.undo_depth(), 2, "a move ended the run");
    e.apply(Command::Newline);
    typed(&mut e, "x");
    assert_eq!(e.undo_depth(), 4, "a newline is its own edit, and ends the run");
    e.apply(Command::Backspace);
    typed(&mut e, "y");
    assert_eq!(e.undo_depth(), 6);
    // A refused character ends the run too.
    let mut f = Editor::new("", limits(3, 100)).0;
    typed(&mut f, "abc");
    assert_eq!(f.apply(Command::Insert('d')), Change::Refused);
    f.apply(Command::Backspace);
    typed(&mut f, "z");
    assert_eq!(f.undo_depth(), 3);
    assert_eq!(f.text(), "abz");
    // So does a character not taken (a control character): the next
    // character typed starts a new edit.
    let mut g = ed("");
    typed(&mut g, "ab");
    assert_eq!(g.apply(Command::Insert('\u{7}')), Change::None);
    typed(&mut g, "c");
    assert_eq!(g.undo_depth(), 2);
    // Undo takes the whole run back.
    e.apply(Command::Undo);
    e.apply(Command::Undo);
    e.apply(Command::Undo);
    e.apply(Command::Undo);
    assert_eq!(e.text(), "hello you");
    e.apply(Command::Undo);
    assert_eq!(e.text(), "hello ");
    e.apply(Command::Undo);
    assert_eq!(e.text(), "");
    assert!(!e.modified());
}

#[test]
fn undo_and_redo_restore_the_text_and_the_cursor() {
    let mut e = ed("abc");
    e.apply(Command::End);
    typed(&mut e, "def");
    e.apply(Command::Home);
    e.apply(Command::Delete);
    assert_eq!(e.text(), "bcdef");
    assert_eq!(e.apply(Command::Undo), Change::Line(0));
    assert_eq!((e.text(), e.cursor()), ("abcdef", 0));
    e.apply(Command::Undo);
    assert_eq!((e.text(), e.cursor()), ("abc", 3));
    assert_eq!(e.apply(Command::Undo), Change::None, "nothing more");
    e.apply(Command::Redo);
    assert_eq!((e.text(), e.cursor()), ("abcdef", 6));
    e.apply(Command::Redo);
    assert_eq!((e.text(), e.cursor()), ("bcdef", 0));
    assert_eq!(e.apply(Command::Redo), Change::None);
    // A new edit drops what could be redone.
    e.apply(Command::Undo);
    typed(&mut e, "x");
    assert_eq!(e.redo_depth(), 0);
    assert_eq!(e.apply(Command::Redo), Change::None);
}

#[test]
fn replace_all_is_one_undoable_edit() {
    let mut e = ed("one\ntwo");
    e.apply(Command::Down);
    assert_eq!(e.replace_all("new"), Change::From(0));
    assert_eq!((e.text(), e.cursor()), ("new", 3));
    assert_eq!(e.replace_all("new"), Change::None, "the same text: no edit");
    assert_eq!(e.undo_depth(), 1);
    e.apply(Command::Undo);
    assert_eq!(e.text(), "one\ntwo");
    assert_eq!(e.line_col(), (1, 0), "the cursor where it was");
    e.apply(Command::Redo);
    assert_eq!(e.text(), "new");
}

#[test]
fn the_size_cap_refuses_an_edit_whole() {
    let mut e = Editor::new("abcdefgh", limits(5, 100)).0;
    assert_eq!(e.text(), "abcde", "the start cut to the cap");
    let mut u = Editor::new("aé", limits(2, 100)).0;
    assert_eq!(u.text(), "a", "cut on a char boundary");
    assert_eq!(e.apply(Command::Insert('x')), Change::Refused);
    assert_eq!(e.apply(Command::Newline), Change::Refused);
    assert_eq!(e.apply(Command::Tab), Change::Refused);
    assert_eq!(e.replace_all("123456"), Change::Refused);
    assert_eq!(e.text(), "abcde");
    assert_eq!(e.undo_depth(), 0);
    e.apply(Command::Bottom);
    e.apply(Command::Backspace);
    assert_eq!(e.apply(Command::Insert('é')), Change::Refused, "two bytes into one left");
    assert_eq!(e.apply(Command::Insert('x')), Change::Line(0));
    assert_eq!(e.replace_all("12345"), Change::From(0), "exactly the cap");
    assert_eq!(u.apply(Command::Insert('b')), Change::Line(0));
}

#[test]
fn the_line_cap_refuses_an_edit_making_a_line_too_long() {
    let lim = Limits { text_bytes: BIG, line_bytes: 5, undo_edits: 100, undo_bytes: BIG };
    // A longer line given is cut there (the rest of the text with it).
    assert_eq!(Editor::new("abcdefg\nhi", lim).0.text(), "abcde");
    assert_eq!(Editor::new("abc\nhijklmn", lim).0.text(), "abc\nhijkl");
    let mut e = Editor::new("abcd\nxy", lim).0;
    assert_eq!(e.apply(Command::Insert('Z')), Change::Line(0), "to exactly the cap");
    assert_eq!(e.apply(Command::Insert('Z')), Change::Refused);
    assert_eq!(e.apply(Command::Tab), Change::Refused);
    assert_eq!(e.text(), "Zabcd\nxy");
    // A newline splits a line: always fits.
    e.apply(Command::Right);
    assert_eq!(e.apply(Command::Newline), Change::From(0));
    assert_eq!(e.text(), "Za\nbcd\nxy");
    // Joining two lines is refused if the joined one is too long.
    e.apply(Command::Bottom);
    e.apply(Command::Home);
    assert_eq!(e.apply(Command::Backspace), Change::From(1), "bcd + xy = 5 bytes");
    e.apply(Command::Up);
    e.apply(Command::End);
    assert_eq!(e.apply(Command::Delete), Change::Refused, "Za + bcdxy = 7 bytes");
    // Multi-byte chars count by the byte; a whole text too.
    let mut u = Editor::new("", lim).0;
    assert_eq!(u.replace_all("ééé"), Change::Refused, "6 bytes on one line");
    assert_eq!(u.replace_all("éé\né"), Change::From(0));
    assert_eq!(u.replace_all("12345\n123456"), Change::Refused, "its second line too long");
}

#[test]
fn past_max_undo_the_oldest_edit_is_forgotten() {
    let mut e = Editor::new("", limits(BIG, 3)).0;
    for _ in 0..5 {
        e.apply(Command::Newline);
    }
    assert_eq!(e.undo_depth(), 3);
    while e.apply(Command::Undo) != Change::None {}
    assert_eq!(e.text(), "\n\n", "two newlines can't be undone");
}

#[test]
fn past_undo_bytes_the_oldest_edits_are_forgotten() {
    // 10 bytes of history: "abc" (3) and "defg" (4) kept, then a 5-byte
    // edit forgets the first; one past the limit alone keeps nothing.
    let mut e = Editor::new("", Limits { text_bytes: BIG, line_bytes: BIG, undo_edits: 100, undo_bytes: 10 }).0;
    typed(&mut e, "abc");
    e.apply(Command::Left);
    e.apply(Command::Right);
    typed(&mut e, "defg");
    assert_eq!(e.undo_depth(), 2);
    e.apply(Command::Left);
    e.apply(Command::Right);
    typed(&mut e, "hijkl");
    assert_eq!(e.undo_depth(), 2, "the oldest forgotten");
    assert_eq!(e.replace_all("0123456789x"), Change::From(0));
    assert_eq!(e.undo_depth(), 0, "a 23-byte edit alone is past the limit");
    assert_eq!(e.apply(Command::Undo), Change::None);
    // A typing run growing past it forgets itself too.
    let mut f = Editor::new("", Limits { text_bytes: BIG, line_bytes: BIG, undo_edits: 100, undo_bytes: 4 }).0;
    typed(&mut f, "abcd");
    assert_eq!(f.undo_depth(), 1);
    typed(&mut f, "e");
    assert_eq!(f.undo_depth(), 0);
    assert_eq!(f.text(), "abcde");
}

#[test]
fn a_huge_view_neither_overflows_nor_turns_a_page_around() {
    let mut e = ed("0\n1\n2\n3");
    e.apply(Command::Down);
    assert!(!e.scroll_to(usize::MAX));
    assert_eq!(e.top(), 0);
    assert_eq!(e.apply(Command::PageDown), Change::Cursor { from: 1, to: 3 });
    assert_eq!(e.line_col(), (3, 0), "the last line, at the goal column");
    assert_eq!(e.apply(Command::PageUp), Change::Cursor { from: 3, to: 0 });
    assert_eq!(e.line_col(), (0, 0), "the first line");
}

#[test]
fn a_typing_run_the_history_forgot_is_over() {
    let mut f = Editor::new("", Limits { text_bytes: BIG, line_bytes: BIG, undo_edits: 100, undo_bytes: 4 }).0;
    typed(&mut f, "abcd");
    assert!(f.typing);
    typed(&mut f, "e");
    assert_eq!(f.undo_depth(), 0);
    assert!(!f.typing, "nothing left to join");
    typed(&mut f, "f");
    assert_eq!((f.undo_depth(), f.text()), (1, "abcdef"));
}

#[test]
fn the_view_keeps_the_cursor_s_line_in_its_rows() {
    let mut e = ed("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    assert!(!e.scroll_to(4));
    assert_eq!(e.top(), 0);
    for _ in 0..4 {
        e.apply(Command::Down);
    }
    assert!(e.scroll_to(4), "line 4 below rows 0-3");
    assert_eq!(e.top(), 1);
    e.apply(Command::PageDown);
    assert_eq!(e.cursor_line(), 8, "a page is the view's rows");
    assert!(e.scroll_to(4));
    assert_eq!(e.top(), 5);
    e.apply(Command::PageUp);
    assert_eq!(e.cursor_line(), 4);
    assert!(e.scroll_to(4), "line 4 above the view");
    assert_eq!(e.top(), 4);
    e.apply(Command::Down);
    assert!(!e.scroll_to(4), "still in view");
    e.apply(Command::Top);
    assert!(e.scroll_to(4));
    assert_eq!(e.top(), 0);
    // Lines removed below the view's end pull it back up.
    e.apply(Command::Bottom);
    e.scroll_to(4);
    assert_eq!(e.top(), 6);
    e.replace_all("a\nb");
    assert!(e.scroll_to(4));
    assert_eq!(e.top(), 0);
    // A view of no rows is taken as one.
    let mut one = ed("a\nb");
    one.apply(Command::Down);
    assert!(one.scroll_to(0));
    assert_eq!(one.top(), 1);
}

#[test]
fn a_change_names_the_lines_to_redraw() {
    let mut e = ed("ab\ncd\nef");
    e.apply(Command::Down);
    assert_eq!(e.apply(Command::Insert('x')), Change::Line(1));
    assert_eq!(e.apply(Command::Down), Change::Cursor { from: 1, to: 2 });
    assert_eq!(e.apply(Command::Newline), Change::From(2));
    // Undoing an edit on another line than the cursor's: from the lower.
    e.apply(Command::Top);
    assert_eq!(e.apply(Command::Undo), Change::From(0));
    assert_eq!(e.text(), "ab\nxcd\nef");
    assert_eq!(e.line_col(), (2, 1));
    // The cursor going back to line 1: from there.
    assert_eq!(e.apply(Command::Undo), Change::From(1));
    assert_eq!(e.text(), "ab\ncd\nef");
    // Undo and redo on the cursor's own line: that line.
    e.apply(Command::Redo);
    assert_eq!(e.apply(Command::Undo), Change::Line(1));
}

// -- the randomized model -------------------------------------------------

/// The same editor, naively: chars, lines found by scanning.
struct Model {
    text: Vec<char>,
    cursor: usize,
    goal: Option<usize>,
    rows: usize,
}

impl Model {
    fn line_starts(&self) -> Vec<usize> {
        let mut s = vec![0];
        s.extend(self.text.iter().enumerate().filter(|(_, c)| **c == '\n').map(|(i, _)| i + 1));
        s
    }

    fn line_col(&self) -> (usize, usize) {
        let starts = self.line_starts();
        let line = starts.iter().rposition(|&s| s <= self.cursor).unwrap();
        (line, self.cursor - starts[line])
    }

    fn line_len(&self, line: usize) -> usize {
        let starts = self.line_starts();
        let end = starts.get(line + 1).map_or(self.text.len(), |&s| s - 1);
        end - starts[line]
    }

    /// Whether removing the char at `at`, a line break, would join two
    /// lines into one past `line_cap` bytes.
    fn join_too_long(&self, at: usize, line_cap: usize) -> bool {
        if self.text[at] != '\n' {
            return false;
        }
        let bytes = |cs: &[char]| cs.iter().map(|c| c.len_utf8()).sum::<usize>();
        let start = self.text[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let end = self.text[at + 1..].iter().position(|&c| c == '\n').map_or(self.text.len(), |i| at + 1 + i);
        bytes(&self.text[start..at]) + bytes(&self.text[at + 1..end]) > line_cap
    }

    fn vertical(&mut self, by: isize) {
        let (line, col) = self.line_col();
        let goal = *self.goal.get_or_insert(col);
        let last = self.line_starts().len() - 1;
        if by < 0 && line == 0 {
            self.cursor = 0;
        } else if by > 0 && line == last {
            self.cursor = self.text.len();
        } else {
            let target = (line as isize + by).clamp(0, last as isize) as usize;
            self.cursor = self.line_starts()[target] + goal.min(self.line_len(target));
        }
    }

    fn insert(&mut self, s: &str, cap: usize, line_cap: usize) {
        let bytes: usize = self.text.iter().map(|c| c.len_utf8()).sum();
        let starts = self.line_starts();
        let (line, _) = self.line_col();
        let line_bytes: usize = self.text[starts[line]..starts[line] + self.line_len(line)].iter().map(|c| c.len_utf8()).sum();
        if bytes + s.len() > cap || (s != "\n" && line_bytes + s.len() > line_cap) {
            return;
        }
        for c in s.chars() {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.goal = None;
    }

    fn apply(&mut self, cmd: Command, cap: usize, line_cap: usize) {
        let (line, _) = self.line_col();
        match cmd {
            Command::Insert(c) if !c.is_control() => self.insert(c.encode_utf8(&mut [0; 4]), cap, line_cap),
            Command::Insert(_) => {}
            Command::Newline => self.insert("\n", cap, line_cap),
            Command::Tab => self.insert("    ", cap, line_cap),
            Command::Backspace if self.cursor > 0 && !self.join_too_long(self.cursor - 1, line_cap) => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
                self.goal = None;
            }
            Command::Delete if self.cursor < self.text.len() && !self.join_too_long(self.cursor, line_cap) => {
                self.text.remove(self.cursor);
                self.goal = None;
            }
            Command::Backspace | Command::Delete => {}
            Command::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                self.goal = None;
            }
            Command::Right => {
                self.cursor = (self.cursor + 1).min(self.text.len());
                self.goal = None;
            }
            Command::Up => self.vertical(-1),
            Command::Down => self.vertical(1),
            Command::PageUp => self.vertical(-(self.rows as isize)),
            Command::PageDown => self.vertical(self.rows as isize),
            Command::Home => {
                self.cursor = self.line_starts()[line];
                self.goal = None;
            }
            Command::End => {
                self.cursor = self.line_starts()[line] + self.line_len(line);
                self.goal = None;
            }
            Command::Top => {
                self.cursor = 0;
                self.goal = None;
            }
            Command::Bottom => {
                self.cursor = self.text.len();
                self.goal = None;
            }
            Command::Undo | Command::Redo => unreachable!("checked by depth"),
        }
    }
}

/// xorshift64*: a fixed seed's sequence.
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

const CHARS: [char; 8] = ['a', 'b', ' ', 'é', '中', '😀', 'z', '\u{1b}'];

fn random_command(r: &mut Rng) -> Command {
    match r.below(18) {
        0..=4 => Command::Insert(CHARS[r.below(CHARS.len() as u64) as usize]),
        5 => Command::Newline,
        6 => Command::Tab,
        7 => Command::Backspace,
        8 => Command::Delete,
        9 => Command::Left,
        10 => Command::Right,
        11 => Command::Up,
        12 => Command::Down,
        13 => [Command::Home, Command::End][r.below(2) as usize],
        14 => [Command::PageUp, Command::PageDown][r.below(2) as usize],
        15 => [Command::Top, Command::Bottom][r.below(2) as usize],
        16 => Command::Undo,
        _ => Command::Redo,
    }
}

/// The model as the editor stands (after an undo or redo, which the model
/// doesn't model: one that did something forgets the goal column, one with
/// nothing to do keeps it).
fn model_of(e: &Editor, rows: usize) -> Model {
    let cursor = e.text()[..e.cursor()].chars().count();
    Model { text: e.text().chars().collect(), cursor, goal: e.goal, rows }
}

#[test]
fn a_randomized_model_agrees_and_every_undo_and_redo_restores_the_text_exactly() {
    for seed in 1..=300u64 {
        let cap = if seed % 5 == 0 { 24 } else { 4096 };
        let rows = 1 + (seed % 7) as usize;
        let line_cap = if seed % 3 == 0 { 12 } else { BIG };
        let mut e = Editor::new("start\ntext", Limits { line_bytes: line_cap, ..limits(cap, 1000) }).0;
        e.scroll_to(rows);
        let mut m = model_of(&e, rows);
        let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        // The text (and cursor) after the edit at each undo depth; depth 0
        // is the start.
        let mut at_depth: Vec<(String, usize)> = vec![(String::from(e.text()), e.cursor())];
        for step in 0..400 {
            let cmd = random_command(&mut r);
            let before = String::from(e.text());
            let (before_line, before_cursor) = (e.cursor_line(), e.cursor());
            let before_lines: Vec<String> = (0..e.line_count()).map(|n| String::from(e.line(n))).collect();
            let change = e.apply(cmd);
            covers(&change, &before_lines, before_line, before_cursor, &e, seed, step);
            if let Command::Undo | Command::Redo = cmd {
                let d = e.undo_depth();
                assert!(d < at_depth.len(), "seed {seed} step {step}");
                // The text recorded at the new depth; after a redo, the
                // cursor where that edit left it.
                assert_eq!(e.text(), at_depth[d].0, "seed {seed} step {step} {cmd:?}");
                if matches!(cmd, Command::Redo) && change != Change::None {
                    assert_eq!(e.cursor(), at_depth[d].1, "seed {seed} step {step}");
                }
                m = model_of(&e, rows);
            } else {
                m.apply(cmd, cap, line_cap);
                let text: String = m.text.iter().collect();
                assert_eq!(e.text(), text, "seed {seed} step {step} {cmd:?}");
                let cursor = e.text()[..e.cursor()].chars().count();
                assert_eq!(cursor, m.cursor, "seed {seed} step {step} {cmd:?} in {:?}", e.text());
                // An edit (the text changed) drops what could be redone: a
                // new depth, or a typing run joined at this one.
                let d = e.undo_depth();
                if e.text() != before {
                    at_depth.truncate(d);
                    at_depth.push((String::from(e.text()), e.cursor()));
                }
                if matches!(change, Change::Refused) {
                    assert_eq!(e.text(), before, "a refused edit changes nothing");
                }
            }
            assert!(e.text().len() <= cap);
            assert!((0..e.line_count()).all(|n| e.line(n).len() <= line_cap), "seed {seed} step {step}");
            assert!(e.text().is_char_boundary(e.cursor()));
            e.scroll_to(rows);
            let line = e.cursor_line();
            assert!(e.top() <= line && line < e.top() + rows, "seed {seed} step {step}");
        }
        // Every edit undone, then every one redone.
        let depth = e.undo_depth();
        let end = String::from(e.text());
        for d in (0..depth).rev() {
            e.apply(Command::Undo);
            assert_eq!(e.text(), at_depth[d].0, "seed {seed}: undo to {d}");
        }
        for (d, (text, _)) in at_depth.iter().enumerate().take(depth + 1).skip(1) {
            e.apply(Command::Redo);
            assert_eq!(e.text(), text, "seed {seed}: redo to {d}");
        }
        assert_eq!(e.text(), end);
    }
}

/// `change` names every line a window must redraw: each line whose text
/// changed and the lines the cursor left and reached (a refused or empty
/// command changing nothing at all).
fn covers(change: &Change, before: &[String], line: usize, cursor: usize, e: &Editor, seed: u64, step: usize) {
    let after: Vec<&str> = (0..e.line_count()).map(|n| e.line(n)).collect();
    let at = format!("seed {seed} step {step} {change:?}");
    match *change {
        Change::None | Change::Refused => {
            assert_eq!(after, before, "{at}");
            assert_eq!(e.cursor(), cursor, "{at}");
        }
        Change::Cursor { from, to } => {
            assert_eq!(after, before, "{at}");
            assert_eq!((from, to), (line, e.cursor_line()), "{at}");
        }
        Change::Line(l) => {
            assert_eq!(after.len(), before.len(), "{at}");
            assert!((0..after.len()).all(|n| n == l || after[n] == before[n]), "{at}");
            assert_eq!((line, e.cursor_line()), (l, l), "{at}");
        }
        Change::From(f) => {
            assert!((0..f).all(|n| after.get(n).copied() == before.get(n).map(String::as_str)), "{at}");
            assert!(line >= f && e.cursor_line() >= f, "{at}");
        }
    }
}
