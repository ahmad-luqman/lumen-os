use crate::input::Decoder;
use crate::words::{self, Partial, SplitError};
use crate::{Command, Complete, Editor, Event, History, Key, Mode, Table};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

fn split(line: &str) -> Vec<String> {
    words::split(line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
}

#[test]
fn words_are_split_on_blanks() {
    assert_eq!(split("  echo  a\tb\nc  "), ["echo", "a", "b", "c"]);
    assert!(split("").is_empty());
    assert!(split("   ").is_empty());
}

#[test]
fn quotes_and_backslashes() {
    assert_eq!(split(r#"echo 'a  b' "c d" e\ f"#), ["echo", "a  b", "c d", "e f"]);
    // Parts next to each other make one word; empty quotes an empty word.
    assert_eq!(split(r#"a'b c'd"e"f '' """#), ["ab cdef", "", ""]);
    // Inside single quotes nothing is special.
    assert_eq!(split(r#"'a\b"c$'"#), [r#"a\b"c$"#]);
    // Inside double quotes a backslash escapes $ ` " \ only.
    assert_eq!(split(r#""\$\`\"\\\n\a""#), [r#"$`"\\n\a"#]);
    // Outside quotes it escapes anything; before a newline it removes both.
    assert_eq!(split("\\a\\'\\\"\\\\ b\\\nc"), ["a'\"\\", "bc"]);
    assert_eq!(split("\"a\\\nb\""), ["ab"]);
}

#[test]
fn comments_start_at_a_word() {
    assert_eq!(split("echo a # b c"), ["echo", "a"]);
    assert_eq!(split("echo a#b '#c'"), ["echo", "a#b", "#c"]);
    assert_eq!(split("# all\necho next"), ["echo", "next"]);
}

#[test]
fn unclosed_quotes_and_a_trailing_backslash_are_errors() {
    assert_eq!(words::split("echo 'a"), Err(SplitError::UnclosedQuote('\'')));
    assert_eq!(words::split("echo \"a"), Err(SplitError::UnclosedQuote('"')));
    assert_eq!(words::split("echo \"a\\"), Err(SplitError::UnclosedQuote('"')));
    assert_eq!(words::split("echo a\\"), Err(SplitError::TrailingBackslash));
    assert_eq!(SplitError::UnclosedQuote('"').to_string(), "unclosed \" quote");
}

fn partial(line: &str) -> Option<Partial> {
    words::partial(line)
}

fn word(words: &[&str], start: usize, text: &str, quote: Option<char>) -> Option<Partial> {
    let words = words.iter().map(|w| w.to_string()).collect();
    Some(Partial { words, start, text: text.to_string(), quote })
}

#[test]
fn the_word_a_line_ends_in() {
    assert_eq!(partial(""), word(&[], 0, "", None));
    assert_eq!(partial("ec"), word(&[], 0, "ec", None));
    assert_eq!(partial("echo "), word(&["echo"], 5, "", None));
    assert_eq!(partial("crash  p"), word(&["crash"], 7, "p", None));
    assert_eq!(partial("echo a\\ b"), word(&["echo"], 5, "a b", None));
    assert_eq!(partial("echo 'a b"), word(&["echo"], 5, "a b", Some('\'')));
    assert_eq!(partial("echo x\"a"), word(&["echo"], 5, "xa", Some('"')));
    assert_eq!(partial("échο ü"), word(&["échο"], "échο ".len(), "ü", None));
    // Nothing to complete in a comment or right after a backslash.
    assert_eq!(partial("echo # c"), None);
    assert_eq!(partial("echo a\\"), None);
    assert_eq!(partial("echo \"a\\"), None);
}

#[test]
fn quoted_words_read_back_as_themselves() {
    let tricky = [
        "plain",
        "a b",
        "it's",
        "say \"hi\"",
        "back\\slash",
        "#hash",
        "$x `y`",
        "tab\there",
        "new\nline",
        "\\\"$`",
        "",
    ];
    for w in tricky {
        for q in [None, Some('\''), Some('"')] {
            let mut line: String = q.into_iter().collect();
            line.push_str(&words::quote(w, q));
            line.extend(q);
            assert_eq!(split(&line), [w], "{w:?} in {q:?}: {line:?}");
        }
    }
}

#[test]
fn history_keeps_lines_and_skips_blanks_and_repeats() {
    let mut h = History::new(3);
    for l in ["one", "", "  ", "two", "two", "one", "three", "four"] {
        h.push(l);
    }
    // "one" (the first) and "two" went; numbers are kept.
    let kept: Vec<_> = h.numbered().collect();
    assert_eq!(kept, [(3, "one"), (4, "three"), (5, "four")]);
    assert_eq!((h.len(), h.get(0), h.get(3)), (3, Some("one"), None));
    let mut none = History::new(0);
    none.push("x");
    assert!(none.is_empty());
}

fn decode(bytes: &[u8]) -> Vec<Key> {
    let mut d = Decoder::new();
    bytes.iter().filter_map(|&b| d.feed(b)).collect()
}

#[test]
fn serial_bytes_become_keys() {
    use Key::*;
    assert_eq!(decode(b"a Z~"), [Char('a'), Char(' '), Char('Z'), Char('~')]);
    assert_eq!(decode(b"\r\n\r\r\n\n"), [Enter, Enter, Enter, Enter]);
    assert_eq!(
        decode(b"\x7f\x08\t\x01\x05\x17\x1a"),
        [Backspace, Backspace, Tab, Ctrl('a'), Ctrl('e'), Ctrl('w'), Ctrl('z')]
    );
    assert_eq!(decode(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\x1bOD"), [Up, Down, Right, Left, Up, Left]);
    assert_eq!(
        decode(b"\x1b[H\x1b[F\x1bOH\x1bOF\x1b[1~\x1b[7~\x1b[4~\x1b[8~\x1b[3~"),
        [Home, End, Home, End, Home, Home, End, End, Delete]
    );
    // Modifiers are ignored; sequences it doesn't know are dropped whole.
    assert_eq!(decode(b"\x1b[1;5C\x1b[5~\x1b[?25hx\x1b[2;3Hy\x1bxz"), [Right, Char('x'), Home, Char('y'), Char('z')]);
}

#[test]
fn utf8_is_decoded_and_malformed_sequences_dropped() {
    use Key::*;
    assert_eq!(decode("é€😀".as_bytes()), [Char('é'), Char('€'), Char('😀')]);
    // A lead byte cut short by an ASCII one: the ASCII one still counts.
    assert_eq!(decode(b"\xc3a\xe2\x82b\x80\xffc"), [Char('a'), Char('b'), Char('c')]);
    // Control characters (U+0080 to U+009F) aren't printable.
    assert_eq!(decode(b"\xc2\x85d"), [Char('d')]);
    // The smallest 3 and 4 byte ones; the largest overlong ones below them.
    assert_eq!(decode("\u{800}\u{10000}".as_bytes()), [Char('\u{800}'), Char('\u{10000}')]);
    assert_eq!(decode(b"\xe0\x9f\xbf\xf0\x8f\xbf\xbfz"), [Char('z')]);
    // Overlong forms (A in 3 and 4 bytes), a surrogate, past U+10FFFF.
    assert_eq!(decode(b"\xe0\x81\x81\xf0\x80\x81\x81\xed\xa0\x80\xf4\x90\x80\x80e"), [Char('e')]);
}

#[test]
fn a_control_character_ends_an_escape_sequence() {
    use Key::*;
    // Esc then Enter (the CR doesn't vanish into the sequence, and the LF
    // after it is the same Enter), a sequence cut short by a Ctrl key, an
    // Esc cut short by another.
    assert_eq!(decode(b"\x1b\r\n\x1b[1\x15\x1bO\x1b[A\x1bxq"), [Enter, Ctrl('u'), Up, Char('q')]);
}

static NO_ARGS: &[&[&str]] = &[];
static CRASH_ARGS: &[&[&str]] = &[&["pf", "panic", "ud", "it's"], &["thread"]];
static TABLE: Table<()> = Table(&[
    Command { name: "echo", args: NO_ARGS, usage: "", about: "", run: () },
    Command { name: "exit", args: NO_ARGS, usage: "", about: "", run: () },
    Command { name: "crash", args: CRASH_ARGS, usage: "", about: "", run: () },
    Command { name: "clear", args: NO_ARGS, usage: "", about: "", run: () },
    Command { name: "clocks", args: NO_ARGS, usage: "", about: "", run: () },
]);

#[test]
fn the_table_completes_names_and_arguments() {
    let options = |words: &[&str]| {
        let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        let mut out = Vec::new();
        TABLE.options(&words, &mut out);
        out
    };
    assert_eq!(options(&[]), ["echo", "exit", "crash", "clear", "clocks"]);
    assert_eq!(options(&["crash"]), ["pf", "panic", "ud", "it's"]);
    assert_eq!(options(&["crash", "pf"]), ["thread"]);
    assert!(options(&["crash", "pf", "thread"]).is_empty());
    assert!(options(&["echo"]).is_empty());
    assert!(options(&["nope"]).is_empty());
    assert_eq!(TABLE.find("crash").map(|c| c.name), Some("crash"));
    assert!(TABLE.find("cr").is_none());
}

/// An editor with the test table, and everything it wrote.
struct Typist {
    editor: Editor,
    out: String,
    events: Vec<Event>,
}

impl Typist {
    fn new(mode: Mode) -> Typist {
        let mut editor = Editor::new("> ", mode, 20, 10);
        let mut out = String::new();
        editor.start(&mut out).unwrap();
        Typist { editor, out, events: Vec::new() }
    }

    fn keys(&mut self, keys: &[Key]) -> &mut Typist {
        for &k in keys {
            if let Some(e) = self.editor.key(k, &TABLE, &mut self.out).unwrap() {
                self.events.push(e);
                self.editor.start(&mut self.out).unwrap();
            }
        }
        self
    }

    fn text(&mut self, s: &str) -> &mut Typist {
        let keys: Vec<Key> = s.chars().map(Key::Char).collect();
        self.keys(&keys)
    }

    /// The output since the last call.
    fn take(&mut self) -> String {
        core::mem::take(&mut self.out)
    }

    fn state(&self) -> (String, usize) {
        (self.editor.line(), self.editor.cursor())
    }
}

#[test]
fn plain_mode_appends_and_erases_at_the_end() {
    use Key::*;
    let mut t = Typist::new(Mode::Plain);
    assert_eq!(t.take(), "> ");
    t.text("echo hi").keys(&[Backspace]);
    assert_eq!(t.take(), "echo hi\x08 \x08");
    // The cursor keys do nothing, so nothing is ever inserted mid-line.
    t.keys(&[Left, Home, Ctrl('a'), Ctrl('b'), Char('X')]);
    assert_eq!((t.take(), t.state()), ("X".into(), ("echo hX".into(), 7)));
    t.keys(&[Ctrl('w')]);
    assert_eq!((t.take(), t.state()), ("\x08 \x08".repeat(2), ("echo ".into(), 5)));
    t.text("there").keys(&[Ctrl('u')]);
    assert_eq!(t.take(), alloc::format!("there{}", "\x08 \x08".repeat(10)));
    t.text("ok").keys(&[Enter]);
    assert_eq!(t.take(), "ok\n> ");
    assert_eq!(t.events, [Event::Line("ok".into())]);
    // A wide character is erased over both its columns.
    t.text("日").keys(&[Backspace]);
    assert_eq!(t.take(), "日\x08\x08  \x08\x08");
}

#[test]
fn plain_mode_recalls_history_by_erasing_and_retyping() {
    use Key::*;
    let mut t = Typist::new(Mode::Plain);
    t.text("echo one").keys(&[Enter]).text("echo two").keys(&[Enter]).text("ech");
    t.take();
    t.keys(&[Up]);
    // "ech" is shared with "echo two": nothing erased, "o two" appended.
    assert_eq!((t.take(), t.state()), ("o two".into(), ("echo two".into(), 8)));
    t.keys(&[Up]);
    assert_eq!(t.take(), alloc::format!("{}one", "\x08 \x08".repeat(3)));
    t.keys(&[Up]); // nothing older
    assert_eq!(t.take(), "");
    t.keys(&[Down, Down]);
    // Past the newest: the line being typed comes back.
    assert_eq!(t.state(), ("ech".into(), 3));
    t.keys(&[Down]);
    assert_eq!(t.state(), ("ech".into(), 3));
}

#[test]
fn ansi_mode_edits_anywhere_and_redraws() {
    use Key::*;
    let mut t = Typist::new(Mode::Ansi);
    t.take();
    t.text("echo abc");
    assert_eq!(t.take(), "echo abc"); // appending writes just the characters
    t.keys(&[Left, Left]);
    assert_eq!((t.take(), t.state()), ("\x1b[1D\x1b[1D".into(), ("echo abc".into(), 6)));
    t.keys(&[Char('X')]);
    assert_eq!((t.take(), t.state()), ("\r> echo aXbc\x1b[K\x1b[2D".into(), ("echo aXbc".into(), 7)));
    t.keys(&[Home]);
    assert_eq!(t.take(), "\x1b[7D");
    t.keys(&[Delete, End]);
    assert_eq!((t.take(), t.state()), ("\r> cho aXbc\x1b[K\x1b[8D\x1b[8C".into(), ("cho aXbc".into(), 8)));
    t.keys(&[Ctrl('a'), Ctrl('f'), Ctrl('k')]);
    assert_eq!(t.state(), ("c".into(), 1));
    t.keys(&[Ctrl('e'), Ctrl('b'), Ctrl('d')]);
    assert_eq!(t.state(), ("".into(), 0));
    t.text("a b").keys(&[Ctrl('b'), Ctrl('u')]);
    assert_eq!(t.state(), ("b".into(), 0));
    t.take();
    t.keys(&[Enter]);
    assert_eq!(t.take(), "\r\n> ");
}

#[test]
fn ctrl_w_deletes_the_word_before_the_cursor() {
    use Key::*;
    let mut t = Typist::new(Mode::Ansi);
    t.text("crash  pf  thread");
    t.keys(&[Left; 6]).keys(&[Ctrl('w')]);
    assert_eq!(t.state(), ("crash  thread".into(), 7));
    t.keys(&[Ctrl('w'), Ctrl('w')]);
    assert_eq!(t.state(), ("thread".into(), 0));
}

#[test]
fn ctrl_c_drops_the_line_and_ctrl_l_shows_it_again() {
    use Key::*;
    let mut t = Typist::new(Mode::Ansi);
    t.text("echo x").keys(&[Left]);
    t.take();
    t.keys(&[Ctrl('l')]);
    assert_eq!(t.take(), "\x1b[H\x1b[2J> \r> echo x\x1b[K\x1b[1D");
    t.keys(&[Ctrl('c')]);
    assert_eq!(t.take(), "^C\r\n> ");
    assert_eq!(t.events, [Event::Cancelled]);
    assert_eq!(t.editor.history().len(), 0);
    // Plain mode has no Ctrl-L.
    let mut p = Typist::new(Mode::Plain);
    p.take();
    p.text("a").keys(&[Ctrl('l'), Ctrl('c')]);
    assert_eq!(p.take(), "a^C\n> ");
}

#[test]
fn lines_stop_at_the_maximum_and_refuse_unprintable_characters() {
    use Key::*;
    let mut t = Typist::new(Mode::Plain);
    t.text(&"x".repeat(25));
    assert_eq!(t.state().1, 20);
    let mut t = Typist::new(Mode::Plain);
    t.keys(&[Char('\u{7}'), Char('\u{300}'), Char('a')]);
    assert_eq!(t.state(), ("a".into(), 1));
}

#[test]
fn ansi_history_editing_a_recalled_line_keeps_the_history() {
    use Key::*;
    let mut t = Typist::new(Mode::Ansi);
    t.text("echo one").keys(&[Enter]).text("echo two").keys(&[Enter]);
    t.keys(&[Up, Up, Backspace, Backspace, Backspace]).text("1").keys(&[Enter]);
    assert_eq!(t.events.last(), Some(&Event::Line("echo 1".into())));
    let kept: Vec<_> = t.editor.history().numbered().map(|(_, l)| l.to_string()).collect();
    assert_eq!(kept, ["echo one", "echo two", "echo 1"]);
    // A repeat isn't kept twice; Ctrl-P/N browse too.
    t.keys(&[Ctrl('p'), Enter, Ctrl('p'), Ctrl('p'), Ctrl('n')]);
    assert_eq!(t.editor.history().len(), 3);
    assert_eq!(t.state(), ("echo 1".into(), 6));
}

#[test]
fn tab_completes_a_unique_match_with_a_space() {
    use Key::*;
    for mode in [Mode::Plain, Mode::Ansi] {
        let mut t = Typist::new(mode);
        t.text("ec").keys(&[Tab]);
        assert_eq!(t.state(), ("echo ".into(), 5));
        t.keys(&[Ctrl('u')]).text("crash pa").keys(&[Tab]).keys(&[Tab]);
        assert_eq!(t.state(), ("crash panic thread ".into(), 19));
    }
    // In plain mode the completion is just appended.
    let mut t = Typist::new(Mode::Plain);
    t.take();
    t.text("cra").keys(&[Tab]);
    assert_eq!(t.take(), "crash ");
}

#[test]
fn tab_lists_options_that_already_differ() {
    use Key::*;
    let mut t = Typist::new(Mode::Plain);
    t.take();
    t.text("c").keys(&[Tab]);
    // crash, clear, clocks: nothing in common past "c": listed, sorted.
    assert_eq!(t.take(), "c\nclear  clocks  crash\n> c");
    t.text("l").keys(&[Tab]);
    assert_eq!(t.take(), "l\nclear  clocks\n> cl");
    t.keys(&[Ctrl('u')]).text("e").keys(&[Tab]);
    // echo, exit: nothing in common past "e".
    assert_eq!(t.state(), ("e".into(), 1));
    t.keys(&[Ctrl('u')]).text("crash p").keys(&[Tab]);
    // pf, panic: "p" is all they share.
    assert_eq!(t.state(), ("crash p".into(), 7));
    t.keys(&[Ctrl('u')]).text("z").keys(&[Tab]);
    assert_eq!(t.state(), ("z".into(), 1));
    // A listing in ANSI mode shows the line again with the cursor in place.
    let mut a = Typist::new(Mode::Ansi);
    a.text("cl x").keys(&[Left, Left]);
    a.take();
    a.keys(&[Tab]);
    assert_eq!(a.take(), "\r\nclear  clocks\r\n> \r> cl x\x1b[K\x1b[2D");
}

#[test]
fn tab_keeps_the_quote_a_word_is_in() {
    use Key::*;
    let mut t = Typist::new(Mode::Ansi);
    t.text("crash 'i").keys(&[Tab]);
    assert_eq!(t.state(), ("crash 'it'\\''s' ".into(), 16));
    assert_eq!(words::split(&t.editor.line()).unwrap(), ["crash", "it's"]);
    let mut t = Typist::new(Mode::Ansi);
    t.text("crash i").keys(&[Tab]);
    assert_eq!(t.state(), ("crash it\\'s ".into(), 12));
    // Completing mid-line replaces only the word before the cursor.
    let mut t = Typist::new(Mode::Ansi);
    t.text("crash ud").keys(&[Home, Right, Right]).keys(&[Tab]);
    assert_eq!(t.state(), ("crash ash ud".into(), 6));
    // Nothing is completed past the line's maximum.
    let mut t = Typist::new(Mode::Ansi);
    t.text(&"x".repeat(16)).keys(&[Home]).text("cra").keys(&[Tab]);
    assert_eq!(t.state(), (alloc::format!("cra{}", "x".repeat(16)), 3));
}

/// A tiny random number generator (xorshift64), seeded.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// What a terminal shows: the editor's output run through lumen-term must
/// put the prompt and the line on the cursor's row, and the cursor where the
/// editor says it is, after every key of random edits, in both modes.
#[test]
fn the_terminal_shows_what_the_editor_holds() {
    use Key::*;
    let keys = [
        Char('a'),
        Char('b'),
        Char(' '),
        Char('é'),
        Char('日'),
        Left,
        Right,
        Home,
        End,
        Backspace,
        Delete,
        Up,
        Down,
        Tab,
        Ctrl('a'),
        Ctrl('e'),
        Ctrl('k'),
        Ctrl('u'),
        Ctrl('w'),
        Ctrl('d'),
        Ctrl('l'),
        Enter,
        Ctrl('c'),
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for mode in [Mode::Plain, Mode::Ansi] {
        for case in 0..300 {
            let mut term = lumen_term::Terminal::new(60, 8, 0);
            let mut t = Typist::new(mode);
            for step in 0..60 {
                // Words the table completes, sometimes.
                if rng.below(8) == 0 {
                    t.text(["c", "cr", "e", "crash p"][rng.below(4)]);
                }
                t.keys(&[keys[rng.below(keys.len())]]);
                // The terminal takes LF as a line feed only (the kernel's
                // consoles add the CR).
                let out = t.take().replace('\n', "\r\n").replace("\r\r\n", "\r\n");
                term.write(out.as_bytes());
                let (line, cursor) = t.state();
                let cur = term.cursor();
                let row: String = (0..term.cols())
                    .map(|c| term.cell(c, cur.row))
                    .filter(|c| !c.flags.contains(lumen_term::Flags::SPACER))
                    .map(|c| c.ch)
                    .collect();
                let want = alloc::format!("> {line}");
                let ctx = (mode, case, step, &line, &row);
                assert_eq!(row.trim_end(), want.trim_end(), "{ctx:?}");
                let width = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                let col = 2 + line.chars().take(cursor).map(width).sum::<usize>();
                assert_eq!(cur.col, col, "{ctx:?}");
            }
        }
    }
}

/// Options for every word: completes nothing but these.
struct Fixed(&'static [&'static str]);

impl Complete for Fixed {
    fn options(&self, _: &[String], out: &mut Vec<String>) {
        out.extend(self.0.iter().map(|o| o.to_string()));
    }
}

/// Type `text` then Tab in an editor completing from `options`: the line
/// and the cursor.
fn tab_after(mode: Mode, options: &'static [&'static str], text: &str) -> (String, usize) {
    let mut editor = Editor::new("> ", mode, 20, 10);
    let mut out = String::new();
    for c in text.chars() {
        editor.key(Key::Char(c), &Fixed(options), &mut out).unwrap();
    }
    editor.key(Key::Tab, &Fixed(options), &mut out).unwrap();
    (editor.line(), editor.cursor())
}

#[test]
fn tab_extends_to_the_start_several_options_share() {
    for mode in [Mode::Plain, Mode::Ansi] {
        // No space after it: the word isn't finished.
        assert_eq!(tab_after(mode, &["stats", "stack", "stop"], "x s"), ("x st".into(), 4));
        assert_eq!(tab_after(mode, &["stats", "stack"], "x s"), ("x sta".into(), 5));
        // Quoted as the word was, the quote left open.
        assert_eq!(tab_after(mode, &["it's one", "it's two"], "x 'i"), ("x 'it'\\''s ".into(), 11));
        assert_eq!(tab_after(mode, &["a b1", "a b2"], "x a"), ("x a\\ b".into(), 6));
        assert_eq!(words::split("x 'it'\\''s 'one").unwrap(), ["x", "it's one"]);
    }
}

#[test]
fn tab_with_one_option_the_start_of_another() {
    // "df" and "dfree" (crash's kinds): "df" typed is all they share, so
    // they're listed; "t" for "term" and "terminal" extends to "term".
    assert_eq!(tab_after(Mode::Plain, &["df", "dfree"], "x df"), ("x df".into(), 4));
    assert_eq!(tab_after(Mode::Plain, &["term", "terminal"], "x t"), ("x term".into(), 6));
}

#[test]
fn tab_near_the_line_limit() {
    // Just fits, with its space (the word replaced counts as freed).
    assert_eq!(tab_after(Mode::Ansi, &["crash"], "xxxxxxxxxxxx cra"), ("xxxxxxxxxxxx crash ".into(), 19));
    assert_eq!(tab_after(Mode::Ansi, &["日日x"], "xxxxxxxxxxxx 日日"), ("xxxxxxxxxxxx 日日x ".into(), 17));
    // Only the space doesn't fit: completed without it.
    assert_eq!(tab_after(Mode::Ansi, &["crash"], "xxxxxxxxxxxxxx cra"), ("xxxxxxxxxxxxxx crash".into(), 20));
    // The start two options share doesn't fit: listed instead.
    let mut editor = Editor::new("> ", Mode::Plain, 20, 10);
    let mut out = String::new();
    for c in "xxxxxxxxxxxxxxxxx s".chars() {
        editor.key(Key::Char(c), &Fixed(&["stats", "stack"]), &mut out).unwrap();
    }
    out.clear();
    editor.key(Key::Tab, &Fixed(&["stats", "stack"]), &mut out).unwrap();
    assert_eq!(out, "\nstack  stats\n> xxxxxxxxxxxxxxxxx s");
}

#[test]
fn a_line_that_didnt_fit_isnt_run() {
    use Key::*;
    // A wide character can't take the last column alone.
    let mut t = Typist::new(Mode::Plain);
    t.text(&"x".repeat(19)).text("日");
    assert_eq!(t.state(), ("x".repeat(19), 19));
    t.take();
    // Enter then says so and runs nothing, even after making room.
    t.keys(&[Backspace, Enter]);
    assert_eq!(t.take(), "\x08 \x08\nline too long (more than 20 columns): not run\n> ");
    assert_eq!(t.events, [Event::Cancelled]);
    assert!(t.editor.history().is_empty());
    // The next line starts afresh.
    t.text("ok").keys(&[Enter]);
    assert_eq!(t.events.last(), Some(&Event::Line("ok".into())));
}

#[test]
fn tab_completes_after_wider_characters() {
    // The word starts after multi-byte characters: its start is counted in
    // characters, not bytes.
    assert_eq!(tab_after(Mode::Ansi, &["thread"], "éé日 t"), ("éé日 thread ".into(), 11));
    let mut t = Typist::new(Mode::Ansi);
    t.text("crash éé t").keys(&[Key::Tab]);
    assert_eq!(t.state(), ("crash éé thread ".into(), 16));
}

#[test]
fn tab_skips_options_that_are_not_printable() {
    assert_eq!(tab_after(Mode::Plain, &["a\tb", "ab\u{7}"], "x a"), ("x a".into(), 3));
    assert_eq!(tab_after(Mode::Plain, &["a\tb", "abc"], "x a"), ("x abc ".into(), 6));
}

#[test]
fn the_line_is_limited_in_columns() {
    // 20 columns: a wide character takes two, so ten fill the line, and
    // the terminal (two for the prompt, one spare) never wraps.
    let mut term = lumen_term::Terminal::new(23, 4, 0);
    let mut t = Typist::new(Mode::Ansi);
    t.text(&"日".repeat(12));
    assert_eq!(t.state().1, 10);
    t.keys(&[Key::Backspace]).text("ab").keys(&[Key::Home]).text("x");
    assert_eq!(t.state(), (alloc::format!("{}ab", "日".repeat(9)), 0));
    t.keys(&[Key::Backspace, Key::Delete, Key::Char('x')]);
    term.write(t.take().as_bytes());
    let row: String = (0..term.cols()).map(|c| term.cell(c, 0).ch).collect();
    assert_eq!(term.cursor().row, 0);
    assert!(row.starts_with("> x"), "{row:?}");
    assert_eq!((term.cell(0, 1).ch, term.cursor().col), (' ', 3));
}

#[test]
fn the_terminal_shows_what_the_editor_holds_after_a_listing() {
    // The listing's lines scroll the prompt down; the check above covers
    // listings too (Tab with "c"), this pins one by hand.
    let mut term = lumen_term::Terminal::new(40, 4, 0);
    let mut t = Typist::new(Mode::Ansi);
    t.text("c").keys(&[Key::Tab]);
    term.write(t.take().as_bytes());
    let row = |r: usize| (0..term.cols()).map(|c| term.cell(c, r).ch).collect::<String>();
    assert_eq!(row(0).trim_end(), "> c");
    assert_eq!(row(1).trim_end(), "clear  clocks  crash");
    assert_eq!(row(2).trim_end(), "> c");
    assert_eq!(vec![term.cursor().col, term.cursor().row], [3, 2]);
}
