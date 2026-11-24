use super::*;
use alloc::{vec, vec::Vec};

#[derive(Debug)]
struct Sink {
    bytes: Vec<u8>,
    limit: usize,
    events: Vec<&'static str>,
}
impl Sink {
    fn new(limit: usize) -> Self {
        Self { bytes: Vec::new(), limit, events: Vec::new() }
    }
}
impl Output for Sink {
    fn room(&self) -> usize {
        self.limit.saturating_sub(self.bytes.len())
    }
    fn push(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.room());
        self.events.push("push");
        self.bytes.extend_from_slice(bytes);
    }
    fn flush(&mut self) {
        self.events.push("flush");
        self.bytes.clear();
    }
}
fn settings() -> Termios {
    let mut t = Termios::linux_default();
    t.c_lflag |= IEXTEN | ECHOKE | ECHOCTL;
    t.c_cc[VWERASE as usize] = 23;
    t.c_cc[VLNEXT as usize] = 22;
    t.c_cc[VREPRINT as usize] = 18;
    t
}
fn with_lflag(flags: u32) -> Termios {
    let mut t = settings();
    t.c_lflag = flags;
    t
}
fn raw() -> Termios {
    let mut t = with_lflag(0);
    t.c_iflag = 0;
    t
}
fn tty(t: Termios) -> LineDiscipline {
    LineDiscipline::try_new(t).unwrap()
}
fn receive(d: &mut LineDiscipline, byte: u8) -> (Effects, Vec<u8>) {
    let mut out = Sink::new(32768);
    let effects = d.receive(byte, &mut out);
    d.check_invariants();
    (effects, out.bytes)
}
fn feed(d: &mut LineDiscipline, bytes: &[u8]) -> Vec<u8> {
    let mut out = Sink::new(65536);
    for &byte in bytes {
        let _ = d.receive(byte, &mut out);
        d.check_invariants();
    }
    out.bytes
}
fn bytes(d: &mut LineDiscipline) -> Vec<u8> {
    let mut buf = [0; BUFFER_SIZE];
    let (result, _) = d.read(&mut buf);
    d.check_invariants();
    match result {
        Read::Bytes(n) => buf[..n.get()].to_vec(),
        _ => Vec::new(),
    }
}
fn write(d: &mut LineDiscipline, bytes: &[u8]) -> Vec<u8> {
    let mut out = Sink::new(bytes.len() * 2);
    assert_eq!(d.write(bytes, &mut out).consumed, bytes.len());
    out.bytes
}
fn fill(d: &mut LineDiscipline, byte: u8, count: usize) {
    let mut out = Sink::new(0);
    for _ in 0..count {
        let _ = d.receive(byte, &mut out);
    }
    d.check_invariants();
}

#[test]
fn linux_default_matches_kernel() {
    let t = Termios::linux_default();
    assert_eq!((t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_line), (0x4100, 5, 0xbf, 0x3b, 0));
    assert_eq!(t.c_cc, [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn termios_conversions_preserve_every_field() {
    let t = Termios {
        c_iflag: 0x12345678,
        c_oflag: 0xabcdef01,
        c_cflag: 0x13579bdf,
        c_lflag: 0x2468ace0,
        c_line: 99,
        c_cc: core::array::from_fn(|i| i as u8 + 32),
    };
    let abi: linux_raw_sys::general::termios = t.into();
    assert_eq!(
        (abi.c_iflag, abi.c_oflag, abi.c_cflag, abi.c_lflag, abi.c_line, abi.c_cc),
        (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_line, t.c_cc)
    );
    assert_eq!(Termios::from(abi), t);
}

#[test]
fn input_flags_and_mapping_precedence() {
    for (flags, input, expected) in [
        (0, &b"\r\n\xff"[..], &b"\r\n\xff"[..]),
        (IGNCR, &b"\r\n"[..], &b"\n"[..]),
        (IGNCR | ICRNL, &b"\r\n"[..], &b"\n"[..]),
        (ICRNL, &b"\r\n"[..], &b"\n\n"[..]),
        (INLCR, &b"\r\n"[..], &b"\r\r"[..]),
        (ICRNL | INLCR, &b"\r\n"[..], &b"\n\r"[..]),
        (ISTRIP, &b"\xff\xc1\x80"[..], &b"\x7fA\0"[..]),
        (ISTRIP | ICRNL, &b"\x8d"[..], &b"\n"[..]),
    ] {
        let mut t = raw();
        t.c_iflag = flags;
        let mut d = tty(t);
        feed(&mut d, input);
        assert_eq!(bytes(&mut d), expected, "flags {flags}");
    }
    let mut t = settings();
    t.c_iflag |= ISTRIP;
    let (effect, echo) = receive(&mut tty(t), 0x83);
    assert_eq!(effect.signal, Some(Signal { number: SIGINT, flush: true }));
    assert_eq!(echo, b"^C");
}

#[test]
fn canonical_erase_and_short_reads() {
    let mut d = tty(settings());
    assert_eq!(feed(&mut d, b"ab\x7fc"), b"ab\x08 \x08c");
    assert_eq!(d.read_decision(4, TimerState::Idle), Decision::Wait);
    assert_eq!(d.read(&mut [0; 8]).0, Read::Nothing);
    assert!(receive(&mut d, b'\n').0.wake.readers);
    feed(&mut d, b"de\npartial");
    let mut one = [0];
    assert_eq!(d.read(&mut one).0, Read::Bytes(NonZeroUsize::new(1).unwrap()));
    assert_eq!(one, [b'a']);
    assert_eq!(bytes(&mut d), b"c\n");
    assert_eq!(bytes(&mut d), b"de\n");
    assert!(!d.poll_readable());
    feed(&mut d, b"\x7f\n");
    assert_eq!(bytes(&mut d), b"partia\n");
}

#[test]
fn word_erase_uses_latin1_and_stops_at_punctuation() {
    for text in [&b"one h\xe9llo"[..], "one héllo".as_bytes(), &b"one hi_23"[..]] {
        let mut d = tty(settings());
        feed(&mut d, text);
        let _ = receive(&mut d, 23);
        feed(&mut d, b"\n");
        assert_eq!(bytes(&mut d), b"one \n");
    }
    for delimiter in [b'.', 0xd7, 0xf7] {
        let mut d = tty(settings());
        feed(&mut d, &[b'f', b'o', b'o', delimiter, b'b', b'a', b'r']);
        assert_eq!(receive(&mut d, 23).1, b"\x08 \x08".repeat(3));
        feed(&mut d, b"\n");
        assert_eq!(bytes(&mut d), [b'f', b'o', b'o', delimiter, b'\n']);
    }
    for letter in [0xc0, 0xd6, 0xd8, 0xf6, 0xf8, 0xff] {
        let mut d = tty(settings());
        feed(&mut d, &[b'x', b' ', letter]);
        let _ = receive(&mut d, 23);
        feed(&mut d, b"\n");
        assert_eq!(bytes(&mut d), b"x \n");
    }
    let mut d = tty(settings());
    feed(&mut d, b"one two...  ");
    assert_eq!(receive(&mut d, 23).1, b"\x08 \x08".repeat(8));
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"one \n");
}

#[test]
fn word_erase_is_visual_without_echoe_but_plain_erase_is_not() {
    let mut t = settings();
    t.c_lflag &= !ECHOE;
    let mut d = tty(t);
    assert_eq!(receive(&mut d, 127).1, b""); // empty erase must not echo
    feed(&mut d, b"foo.bar");
    assert_eq!(receive(&mut d, 23).1, b"\x08 \x08".repeat(3));
    assert_eq!(receive(&mut d, 127).1, b"^?");
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"foo\n");
    t.c_lflag &= !IEXTEN;
    let mut d = tty(t);
    feed(&mut d, b"x\x17\x16\x12\n");
    assert_eq!(bytes(&mut d), b"x\x17\x16\x12\n");
}

#[test]
fn kill_preserves_completed_lines_and_erases_utf8_once() {
    let mut d = tty(settings());
    feed(&mut d, "keep\na€".as_bytes());
    assert_eq!(receive(&mut d, 21).1, b"\x08 \x08".repeat(2));
    assert_eq!(receive(&mut d, 21).1, b"");
    feed(&mut d, b"x\n");
    assert_eq!(bytes(&mut d), b"keep\n");
    assert_eq!(bytes(&mut d), b"x\n");
}

#[test]
fn read_distinguishes_eof_from_empty_and_preserves_exact_boundary() {
    let mut d = tty(settings());
    let mut buf = [0; 2];
    assert_eq!(d.read(&mut buf).0, Read::Nothing);
    assert_eq!(receive(&mut d, 4).1, b"");
    assert_eq!(d.read_decision(1, TimerState::Idle), Decision::Eof);
    assert_eq!(d.read(&mut []).0, Read::Nothing);
    assert!(d.poll_readable());
    assert_eq!(d.read(&mut buf).0, Read::Eof);
    assert!(!d.poll_readable());
    feed(&mut d, b"abc\x04next\n");
    assert_eq!(d.read_decision(2, TimerState::Expired), Decision::Line);
    assert_eq!(d.read(&mut buf).0, Read::Bytes(NonZeroUsize::new(2).unwrap()));
    assert_eq!(&buf, b"ab");
    let mut one = [0];
    assert_eq!(d.read(&mut one).0, Read::Bytes(NonZeroUsize::new(1).unwrap()));
    assert_eq!(&one, b"c");
    assert_eq!(bytes(&mut d), b"next\n");
    feed(&mut d, b"\x04\x04x\n");
    assert_eq!(d.read(&mut buf).0, Read::Eof);
    assert_eq!(d.read(&mut buf).0, Read::Eof);
    assert_eq!(bytes(&mut d), b"x\n");
    assert_eq!(d.read(&mut buf).0, Read::Nothing);
    let mut raw = tty(raw());
    assert_eq!(raw.read(&mut buf).0, Read::Nothing);
}

#[test]
fn eol_eol2_and_disabled_control_characters() {
    let mut t = settings();
    t.c_cc[VEOL as usize] = b';';
    t.c_cc[VEOL2 as usize] = b':';
    let mut d = tty(t);
    feed(&mut d, b"a;b:c\n");
    assert_eq!(bytes(&mut d), b"a;");
    assert_eq!(bytes(&mut d), b"b:");
    assert_eq!(bytes(&mut d), b"c\n");
    t.c_lflag &= !IEXTEN;
    let mut d = tty(t);
    feed(&mut d, b"a:b;");
    assert_eq!(bytes(&mut d), b"a:b;");
    t.c_cc = [0; NCCS as usize];
    let mut d = tty(t);
    feed(&mut d, b"a\0b\n");
    assert_eq!(bytes(&mut d), b"a\0b\n");
}

#[test]
fn literal_next_quotes_signals_delimiters_and_mapping() {
    let mut d = tty(settings());
    assert_eq!(receive(&mut d, 22).1, b"^\x08");
    let (effect, echo) = receive(&mut d, 3);
    assert_eq!(effect.signal, None);
    assert_eq!(echo, b"^C");
    assert_eq!(feed(&mut d, b"\x16\x7f\x16\x04\x16\n\x16\r"), b"^\x08^?^\x08^D^\x08^J^\x08^M");
    assert!(!d.poll_readable());
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"\x03\x7f\x04\n\r\n");
    let mut t = settings();
    t.c_iflag |= ISTRIP;
    let mut d = tty(t);
    feed(&mut d, b"\x16\xff\n");
    assert_eq!(bytes(&mut d), b"\x7f\n");
}

#[test]
fn utf8_erase_and_stray_continuations() {
    for (flags, expected) in [(IUTF8, &b"a\n"[..]), (0, &b"a\xe2\x82\n"[..])] {
        let mut t = settings();
        t.c_iflag = flags;
        let mut d = tty(t);
        feed(&mut d, "a€".as_bytes());
        assert_eq!(receive(&mut d, 127).1, b"\x08 \x08");
        feed(&mut d, b"\n");
        assert_eq!(bytes(&mut d), expected);
    }
    let mut d = tty(settings());
    feed(&mut d, b"\x80\x81");
    assert_eq!(receive(&mut d, 127).1, b"");
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"\x80\x81\n");
    feed(&mut d, b"keep\n\x80\x81");
    let _ = receive(&mut d, 23);
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"keep\n");
    assert_eq!(bytes(&mut d), b"\x80\x81\n");
}

#[test]
fn tab_erase_tracks_columns_without_opost() {
    for oflag in [0, OPOST | ONLCR] {
        for (prefix, text, width) in [
            (&b""[..], &b"ab\t"[..], 6),
            (&b"> "[..], &b"ab\t"[..], 4),
            (&b""[..], "€\t".as_bytes(), 7),
            (&b""[..], &b"\x01\t"[..], 6),
        ] {
            let mut t = settings();
            t.c_oflag = oflag;
            let mut d = tty(t);
            write(&mut d, prefix);
            feed(&mut d, text);
            assert_eq!(receive(&mut d, 127).1, vec![BS; width]);
            assert_eq!(receive(&mut d, b'\t').1, b"\t");
            assert_eq!(receive(&mut d, 127).1, vec![BS; width]);
        }
    }
}

#[test]
fn control_without_echoctl_does_not_move_column() {
    let mut t = settings();
    t.c_lflag &= !ECHOCTL;
    let mut d = tty(t);
    assert_eq!(feed(&mut d, b"a\x01\x7f\t"), b"a\x01\t");
    assert_eq!(receive(&mut d, 127).1, [BS; 7]);
    let mut t = raw();
    t.c_iflag = 0;
    let mut d = tty(t);
    assert_eq!(write(&mut d, b"\x80\x9f"), b"\x80\x9f");
    assert_eq!(d.column, 2);
}

#[test]
fn raw_newline_echo_and_echonl_gates() {
    for flags in [0, ECHO, ECHO | ECHOCTL, ECHONL, ECHONL | ECHOCTL] {
        let mut t = with_lflag(flags);
        t.c_iflag = ICRNL;
        let mut d = tty(t);
        let lf = if flags & ECHO == 0 {
            &b""[..]
        } else if flags & ECHOCTL != 0 {
            &b"^J"[..]
        } else {
            &b"\r\n"[..]
        };
        assert_eq!(receive(&mut d, b'\n').1, lf);
        assert_eq!(receive(&mut d, b'\r').1, if flags & ECHO != 0 { &b"\r\n"[..] } else { &b""[..] });
    }
    let mut d = tty(with_lflag(ICANON | IEXTEN | ECHONL));
    assert_eq!(feed(&mut d, b"\x16\n"), b"");
    assert!(!d.poll_readable());
    assert_eq!(receive(&mut d, b'\n').1, b"\r\n");
    assert_eq!(bytes(&mut d), b"\n\n");
    let mut d = tty(settings());
    assert_eq!(receive(&mut d, b'\n').1, b"\r\n");
}

#[test]
fn echo_flags_are_independently_gated() {
    for flag in [0, ECHOE, ECHOK, ECHOKE, ECHOCTL, ECHONL] {
        let mut d = tty(with_lflag(ICANON | flag));
        assert_eq!(feed(&mut d, b"a\x7fb\x15"), b"");
        assert_eq!(receive(&mut d, b'\n').1, if flag == ECHONL { &b"\r\n"[..] } else { &b""[..] });
        assert_eq!(bytes(&mut d), b"\n");
    }
    for (flags, erase, kill) in [
        (ECHO, &b"\x7f"[..], &b"\x15"[..]),
        (ECHO | ECHOE, &b"\x08 \x08"[..], &b"\x15"[..]),
        (ECHO | ECHOCTL, &b"^?"[..], &b"^U"[..]),
        (ECHO | ECHOK, &b"\x7f"[..], &b"\x15\r\n"[..]),
        (ECHO | ECHOKE, &b"\x7f"[..], &b"\x15"[..]),
        (ECHO | ECHOKE | ECHOK, &b"\x7f"[..], &b"\x15\r\n"[..]),
        (ECHO | ECHOKE | ECHOE, &b"\x08 \x08"[..], &b"\x15"[..]),
        (ECHO | ECHOKE | ECHOK | ECHOE, &b"\x08 \x08"[..], &b"\x08 \x08"[..]),
    ] {
        let mut d = tty(with_lflag(ICANON | flags));
        assert_eq!(receive(&mut d, b'a').1, b"a");
        assert_eq!(receive(&mut d, 127).1, erase);
        let _ = receive(&mut d, b'b');
        assert_eq!(receive(&mut d, 21).1, kill);
        assert_eq!(receive(&mut d, b'\n').1, b"\r\n");
        assert_eq!(bytes(&mut d), b"\n");
    }
}

#[test]
fn reprint_after_prompt_records_new_tab_widths() {
    let mut d = tty(settings());
    write(&mut d, b"> ");
    feed(&mut d, b"ab\t");
    assert_eq!(receive(&mut d, 18).1, b"^R\r\nab\t");
    assert_eq!(receive(&mut d, 127).1, [BS; 6]); // original tab was only four columns
    feed(&mut d, b"\n");
    assert_eq!(bytes(&mut d), b"ab\n");
    assert_eq!(feed(&mut d, b"\x01\x7f"), b"^A\x08 \x08\x08 \x08");
}

#[test]
fn echo_units_are_atomic_prefixes_and_only_pushed_columns_commit() {
    for (source, expected, capacity) in [(&b"\x01"[..], &b""[..], 1), (&b"\n"[..], &b""[..], 1)] {
        let mut d = tty(settings());
        let mut out = Sink::new(capacity);
        let e = d.receive(source[0], &mut out);
        assert_eq!(e.echo, Echo::Truncated);
        assert_eq!(out.bytes, expected);
        assert_eq!(d.column, 0);
    }
    for capacity in 0..10 {
        let mut d = tty(settings());
        feed(&mut d, b"abc");
        let mut out = Sink::new(capacity);
        let e = d.receive(21, &mut out);
        let units = (capacity / 3).min(3);
        assert_eq!(out.bytes, b"\x08 \x08".repeat(units));
        assert_eq!(e.echo, if capacity < 9 { Echo::Truncated } else { Echo::Complete });
        assert_eq!(d.column, 3 - units);
        assert_eq!(d.pending, 0); // input editing completes despite lost echo
    }
    let mut d = tty(settings());
    feed(&mut d, b"abc");
    let mut out = Sink::new(3); // ^R fits; CRLF doesn't; following 'a' would fit but must be dropped
    let e = d.receive(18, &mut out);
    assert_eq!(e.echo, Echo::Truncated);
    assert_eq!(out.bytes, b"^R");
    assert_eq!(d.column, 5);
    let mut d = tty(settings());
    fill(&mut d, b'x', 4095);
    let mut out = Sink::new(4096);
    let e = d.receive(21, &mut out);
    assert_eq!(e.echo, Echo::Truncated);
    assert_eq!(out.bytes, b"\x08 \x08".repeat(1365));
    assert_eq!(d.pending, 0);
}

#[test]
fn output_backpressure_retries_without_column_or_byte_loss() {
    let mut d = tty(settings());
    let mut out = Sink::new(2);
    assert_eq!(d.write(b"a\nb", &mut out), Written { consumed: 1 });
    assert_eq!(out.bytes, b"a");
    assert_eq!(d.column, 1);
    out.bytes.clear();
    assert_eq!(d.write(b"\nb", &mut out), Written { consumed: 1 });
    assert_eq!(out.bytes, b"\r\n");
    assert_eq!(d.column, 0);
    out.bytes.clear();
    assert_eq!(d.write(b"b", &mut out), Written { consumed: 1 });
    assert_eq!(out.bytes, b"b");
    let mut t = raw();
    t.c_oflag = OPOST | ONOCR;
    let mut d = tty(t);
    let mut out = Sink::new(0);
    assert_eq!(d.write(b"\r\rX", &mut out).consumed, 2);
    assert_eq!(d.column, 0);
}

#[test]
fn isig_flush_precedes_echo_and_noflsh_preserves_output() {
    for (byte, number) in [(3, SIGINT), (28, SIGQUIT), (26, SIGTSTP)] {
        for noflush in [false, true] {
            let mut t = settings();
            if noflush {
                t.c_lflag |= NOFLSH;
            }
            let mut d = tty(t);
            feed(&mut d, b"old\npartial");
            let mut out = Sink::new(64);
            out.push(b"pending");
            out.events.clear();
            let e = d.receive(byte, &mut out);
            assert_eq!(e.input, Input::Ignored);
            assert_eq!(e.signal, Some(Signal { number, flush: !noflush }));
            assert_eq!(out.events, if noflush { vec!["push"] } else { vec!["flush", "push"] });
            let mut expected = if noflush { b"pending".to_vec() } else { Vec::new() };
            expected.extend([b'^', byte ^ 0x40]);
            assert_eq!(out.bytes, expected);
            feed(&mut d, b"x\n");
            if noflush {
                assert_eq!(bytes(&mut d), b"old\n");
                assert_eq!(bytes(&mut d), b"partialx\n");
            } else {
                assert_eq!(bytes(&mut d), b"x\n");
            }
        }
    }
    let mut d = tty(with_lflag(ISIG));
    let mut out = Sink::new(0);
    let e = d.receive(3, &mut out);
    assert_eq!(e.signal.unwrap().number, SIGINT);
    assert_eq!(e.echo, Echo::Complete);
    assert!(out.bytes.is_empty());
    assert_eq!(out.events, ["flush"]);
    let mut t = settings();
    t.c_lflag &= !ISIG;
    let mut d = tty(t);
    feed(&mut d, b"\x03\x1c\x1a\n");
    assert_eq!(bytes(&mut d), b"\x03\x1c\x1a\n");
}

#[test]
fn breaks_consume_literal_next_and_never_echo_nul() {
    for flags in [0, IGNBRK, BRKINT, IGNBRK | BRKINT] {
        let mut t = settings();
        t.c_iflag = flags;
        let mut d = tty(t);
        feed(&mut d, b"x\x16");
        let mut out = Sink::new(32);
        let e = d.receive_break(&mut out);
        assert_eq!(out.bytes, b"");
        if flags == BRKINT {
            assert_eq!(e.signal, Some(Signal { number: SIGINT, flush: true }));
        } else {
            assert_eq!(e.signal, None);
        }
        assert_eq!(e.input, if flags == 0 { Input::Stored } else { Input::Ignored });
        let _ = receive(&mut d, b'\n');
        assert_eq!(
            bytes(&mut d),
            if flags == 0 {
                &b"x\0\n"[..]
            } else if flags == BRKINT {
                &b"\n"[..]
            } else {
                &b"x\n"[..]
            }
        );
        assert!(!d.literal);
    }
    let mut t = raw();
    t.c_iflag = BRKINT;
    t.c_lflag = NOFLSH;
    let mut d = tty(t);
    let mut out = Sink::new(16);
    out.push(b"out");
    let _ = receive(&mut d, 0);
    let e = d.receive_break(&mut out);
    assert_eq!(e.signal, Some(Signal { number: SIGINT, flush: false }));
    assert_eq!(bytes(&mut d), b"\0");
    assert_eq!(out.bytes, b"out");
}

#[test]
fn raw_capacity_is_4095_and_reports_dropped_input() {
    let mut d = tty(raw());
    fill(&mut d, b'a', 4095);
    assert_eq!(d.input_room(), 0);
    assert!(!d.accepts(b'b'));
    let (e, echo) = receive(&mut d, b'b');
    assert_eq!(e.input, Input::Dropped);
    assert_eq!(echo, b"");
    assert_eq!(bytes(&mut d), vec![b'a'; 4095]);
    let mut t = raw();
    t.c_iflag |= IMAXBEL;
    let mut d = tty(t);
    fill(&mut d, b'x', 4095);
    let (e, echo) = receive(&mut d, b'y');
    assert_eq!(e.input, Input::Dropped);
    assert_eq!(echo, [BEL]);
}

#[test]
fn canonical_overflow_keeps_latest_character_and_reserves_terminator() {
    let mut d = tty(settings());
    fill(&mut d, b'a', 4095);
    assert!(!d.accepts(b'b'));
    assert!(d.accepts(b'\n'));
    assert_eq!(receive(&mut d, b'b').0.input, Input::Stored);
    assert_eq!(receive(&mut d, b'c').0.input, Input::Stored);
    assert_eq!(d.input.len(), 4096);
    assert_eq!(d.input.back().unwrap().byte, b'c');
    assert_eq!(receive(&mut d, 127).1, b"\x08 \x08");
    assert_eq!(d.input.len(), 4095);
    assert!(d.input.iter().all(|e| e.byte == b'a'));
    let _ = receive(&mut d, b'\n');
    let line = bytes(&mut d);
    assert_eq!(&line[..4095], vec![b'a'; 4095]);
    assert_eq!(line[4095], b'\n');
    fill(&mut d, b'x', 4095);
    let _ = receive(&mut d, b'y');
    let _ = receive(&mut d, b'z');
    let _ = receive(&mut d, b'\n');
    let line = bytes(&mut d);
    assert_eq!(line.len(), 4096);
    assert_eq!(&line[..4095], vec![b'x'; 4095]);
    assert_eq!(line[4095], b'\n');
    fill(&mut d, b'q', 4095);
    let _ = receive(&mut d, 4);
    assert_eq!(bytes(&mut d), vec![b'q'; 4095]);
}

#[test]
fn completed_unread_line_blocks_full_input_including_delimiters() {
    let mut d = tty(settings());
    feed(&mut d, b"a\n");
    fill(&mut d, b'b', 4093);
    for byte in [b'x', b'\n', 4] {
        assert!(!d.accepts(byte));
        let (e, echo) = receive(&mut d, byte);
        assert_eq!(e.input, Input::Dropped);
        assert!(echo.is_empty());
    }
    assert_eq!(bytes(&mut d), b"a\n");
    assert!(d.accepts(b'x'));
    let _ = receive(&mut d, b'\n');
    let line = bytes(&mut d);
    assert_eq!(&line[..4093], vec![b'b'; 4093]);
    assert_eq!(line[4093], b'\n');
    fill(&mut d, b'q', 4095);
    let _ = receive(&mut d, b'\n');
    assert_eq!(receive(&mut d, b'x').0.input, Input::Dropped);
    assert_eq!(d.input.len(), 4096);
}

#[test]
fn accepts_accounts_for_reserve_mapping_quoting_and_ignored_input() {
    let mut d = tty(settings());
    fill(&mut d, b'a', 4095);
    assert!(!d.accepts(b'x'));
    assert!(d.accepts(b'\r'));
    assert!(d.accepts(4));
    for byte in [127, 23, 21, 3, 22, 18] {
        assert!(d.accepts(byte));
    }
    let _ = receive(&mut d, 22);
    assert!(!d.accepts(b'\n'));
    let _ = receive(&mut d, b'\n');
    assert!(!d.poll_readable());
    let mut t = settings();
    t.c_iflag |= IGNCR;
    let _ = d.set_termios(t, false);
    let e = receive(&mut d, b'\r').0;
    assert_eq!(e.input, Input::Ignored);
    assert!(d.accepts(b'\r'));
    t.c_iflag |= ISTRIP;
    let _ = d.set_termios(t, false);
    assert!(d.accepts(0x83));
    t.c_cc = [0; NCCS as usize];
    let _ = d.set_termios(t, false);
    assert!(!d.accepts(0));
}

#[test]
fn all_four_vmin_vtime_cases() {
    use Readiness::*;
    use TimerState::*;
    for timer in [Idle, Running, Expired] {
        for n in [0, 1, 2, 5] {
            assert_eq!(ready(0, 0, n, timer), ReturnNow(n));
            assert_eq!(ready(2, 0, n, timer), if n >= 2 { ReturnNow(n) } else { Wait });
            if n != 0 {
                assert_eq!(ready(0, 7, n, timer), ReturnNow(n));
            }
            if n >= 2 {
                assert_eq!(ready(2, 7, n, timer), ReturnNow(n));
            }
        }
        assert_eq!(ready(2, 7, 0, timer), Wait);
    }
    assert_eq!(ready(0, 7, 0, Idle), WaitWithTimer { deciseconds: 7, kind: TimerKind::Read });
    assert_eq!(ready(0, 7, 0, Running), Wait);
    assert_eq!(ready(0, 7, 0, Expired), ReturnNow(0));
    assert_eq!(ready(2, 7, 1, Idle), WaitWithTimer { deciseconds: 7, kind: TimerKind::InterByte });
    assert_eq!(ready(2, 7, 1, Running), Wait);
    assert_eq!(ready(2, 7, 1, Expired), ReturnNow(1));
}

#[test]
fn reader_timers_are_independent_and_destinations_clamp_vmin() {
    let mut t = raw();
    t.c_cc[VMIN as usize] = 3;
    t.c_cc[VTIME as usize] = 2;
    let mut d = tty(t);
    assert!(receive(&mut d, b'a').0.timer_restart);
    assert_eq!(d.read_decision(8, TimerState::Running), Decision::Raw(Readiness::Wait));
    assert_eq!(d.read_decision(8, TimerState::Expired), Decision::Raw(Readiness::ReturnNow(1)));
    assert_eq!(d.read_decision(1, TimerState::Running), Decision::Raw(Readiness::ReturnNow(1)));
    assert_eq!(d.read_decision(0, TimerState::Running), Decision::Raw(Readiness::ReturnNow(0)));
    let _ = d.set_termios(t, false); // must not reset either reader's timer
    assert_eq!(d.read_decision(8, TimerState::Running), Decision::Raw(Readiness::Wait));
    assert_eq!(d.read_decision(8, TimerState::Expired), Decision::Raw(Readiness::ReturnNow(1)));
    let _ = d.set_termios(t, true);
    assert_eq!(d.read_decision(8, TimerState::Expired), Decision::Raw(Readiness::Wait));
    assert!(receive(&mut d, b'b').0.timer_restart);
    assert_eq!(
        d.read_decision(8, TimerState::Idle),
        Decision::Raw(Readiness::WaitWithTimer { deciseconds: 2, kind: TimerKind::InterByte })
    );
    fill(&mut d, b'x', 4094);
    assert!(!receive(&mut d, b'x').0.timer_restart);
    for (min, time) in [(0, 0), (1, 0), (0, 2)] {
        t.c_cc[VMIN as usize] = min;
        t.c_cc[VTIME as usize] = time;
        let mut d = tty(t);
        assert!(!receive(&mut d, b'x').0.timer_restart);
    }
    let mut t = raw();
    t.c_cc[VMIN as usize] = 0;
    t.c_cc[VTIME as usize] = 9;
    t.c_lflag = ISIG;
    let mut d = tty(t);
    let _ = receive(&mut d, 3);
    assert_eq!(d.read_decision(4, TimerState::Running), Decision::Raw(Readiness::Wait));
    assert_eq!(d.read_decision(4, TimerState::Expired), Decision::Raw(Readiness::ReturnNow(0)));
}

#[test]
fn poll_readiness_never_treats_empty_raw_input_as_eof() {
    for (min, time, threshold) in [(0, 0, 1), (0, 7, 1), (3, 0, 3), (3, 7, 1)] {
        let mut t = raw();
        t.c_cc[VMIN as usize] = min;
        t.c_cc[VTIME as usize] = time;
        let mut d = tty(t);
        assert!(!d.poll_readable());
        for n in 1..=4 {
            let _ = receive(&mut d, b'x');
            assert_eq!(d.poll_readable(), n >= threshold);
        }
        assert_eq!(bytes(&mut d), b"xxxx");
        assert!(!d.poll_readable());
    }
    let mut d = tty(settings());
    let _ = receive(&mut d, b'a');
    assert!(!d.poll_readable());
    let _ = receive(&mut d, 4);
    assert!(d.poll_readable());
}

#[test]
fn output_flags_and_column_tracking() {
    for (flags, input, expected) in [
        (ONLCR | OCRNL | ONOCR | ONLRET, &b"\r\n\t\x08x"[..], &b"\r\n\t\x08x"[..]),
        (OPOST, &b"\r\n"[..], &b"\r\n"[..]),
        (OPOST | ONLCR, &b"a\nb\r"[..], &b"a\r\nb\r"[..]),
        (OPOST | OCRNL | ONLCR, &b"\r\n"[..], &b"\n\r\n"[..]),
        (OPOST | ONOCR, &b"\rA\r\r"[..], &b"A\r"[..]),
        (OPOST | ONOCR | ONLRET, &b"A\n\r"[..], &b"A\n"[..]),
        (OPOST | ONOCR, &b"A\n\r"[..], &b"A\n\r"[..]),
        (OPOST | ONOCR | OCRNL, &b"\rA\r\r"[..], &b"A\n\n"[..]),
        (OPOST | ONOCR | OCRNL | ONLRET, &b"A\r\r"[..], &b"A\n"[..]),
        (OPOST | ONOCR, &b"A\x08\r\t\r"[..], &b"A\x08\t\r"[..]),
    ] {
        let mut t = raw();
        t.c_oflag = flags;
        let mut d = tty(t);
        assert_eq!(write(&mut d, input), expected);
        let mut d = tty(t);
        let mut result = Vec::new();
        for &b in input {
            result.extend(write(&mut d, &[b]));
        }
        assert_eq!(result, expected);
    }
}

#[test]
fn background_writer_decisions_distinguish_orphaned_groups() {
    for bits in 0..64 {
        let facts = WriterFacts {
            controlling_terminal: bits & 2 != 0,
            background: bits & 4 != 0,
            signal_blocked: bits & 8 != 0,
            signal_ignored: bits & 16 != 0,
            orphaned_group: bits & 32 != 0,
        };
        let expected = match bits {
            7 => BackgroundWrite::SendSigttou,
            39 => BackgroundWrite::Eio,
            _ => BackgroundWrite::Allow,
        };
        assert_eq!(background_write_signal(bits & 1 != 0, facts), expected);
    }
}

#[test]
fn winsize_reports_each_field_change() {
    let mut d = LineDiscipline::default();
    assert_eq!(d.winsize(), Winsize::default());
    for ws in [
        Winsize { ws_row: 24, ..Winsize::default() },
        Winsize { ws_row: 24, ws_col: 80, ..Winsize::default() },
        Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 640, ws_ypixel: 0 },
        Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 640, ws_ypixel: 480 },
    ] {
        assert!(d.set_winsize(ws));
        assert_eq!(d.winsize(), ws);
        assert!(!d.set_winsize(ws));
        assert_eq!(Winsize::from(linux_raw_sys::general::winsize::from(ws)), ws);
    }
}

#[test]
fn mode_switches_preserve_eof_as_nul_and_publish_synthetic_line() {
    let mut d = tty(settings());
    feed(&mut d, b"a\nb\x04partial");
    let _ = d.set_termios(raw(), false);
    assert_eq!(bytes(&mut d), b"a\nb\0partial");
    feed(&mut d, b"raw");
    let _ = d.set_termios(settings(), false);
    feed(&mut d, b"new\x7f\n");
    assert_eq!(bytes(&mut d), b"raw");
    assert_eq!(bytes(&mut d), b"ne\n");
    feed(&mut d, b"part\x16");
    assert!(d.set_termios(raw(), false).wake.readers);
    let _ = d.set_termios(settings(), false);
    assert_eq!(bytes(&mut d), b"part");
    assert!(receive(&mut d, 3).0.signal.is_some());
    let _ = d.set_termios(raw(), false);
    let _ = d.set_termios(settings(), false);
    assert!(!d.poll_readable());
}

#[test]
fn termios_changes_apply_now_and_flush_only_when_requested() {
    for flush in [false, true] {
        let mut d = tty(settings());
        feed(&mut d, b"abc");
        let applied = d.set_termios(raw(), flush);
        assert_eq!(applied.flush_input, flush);
        assert_eq!(d.termios(), raw());
        assert_eq!(bytes(&mut d), if flush { &b""[..] } else { &b"abc"[..] });
    }
    let mut d = tty(settings());
    feed(&mut d, b"keep");
    // There is no queued request to apply after an interrupted kernel drain.
    let _ = d.set_termios(raw(), false);
    let _ = d.set_termios(settings(), false);
    assert_eq!(bytes(&mut d), b"keep");
}

#[test]
fn wake_edges_cover_reads_edits_flushes_and_termios_changes() {
    let mut d = tty(settings());
    assert_eq!(receive(&mut d, b'a').0.wake, Wake::default());
    assert_eq!(receive(&mut d, b'\n').0.wake, Wake { readers: true, writers: false });
    assert_eq!(receive(&mut d, b'\n').0.wake, Wake::default());
    let _ = d.set_termios(raw(), true);
    fill(&mut d, b'x', 4095);
    let (_, wake) = d.read(&mut [0]);
    assert_eq!(wake, Wake { readers: false, writers: true });
    assert_eq!(d.read(&mut [0]).1, Wake::default());
    for action in [3, 21, 127] {
        let mut d = tty(settings());
        fill(&mut d, b'x', 4095);
        assert_eq!(receive(&mut d, action).0.wake, Wake { readers: false, writers: true });
    }
    let mut d = tty(settings());
    fill(&mut d, b'x', 4095);
    assert_eq!(d.set_termios(settings(), true).wake, Wake { readers: false, writers: true });
    let mut d = tty(settings());
    feed(&mut d, b"partial");
    assert_eq!(d.set_termios(raw(), false).wake, Wake { readers: true, writers: false });
    let mut t = raw();
    t.c_cc[VMIN as usize] = 4;
    let mut d = tty(t);
    feed(&mut d, b"ab");
    assert!(!d.poll_readable());
    t.c_cc[VMIN as usize] = 2;
    assert_eq!(d.set_termios(t, false).wake, Wake { readers: true, writers: false });
    assert_eq!(d.set_termios(t, false).wake, Wake::default());
    let mut d = tty(settings());
    fill(&mut d, b'x', 4095);
    let mut t = settings();
    t.c_lflag |= NOFLSH;
    let _ = d.set_termios(t, false);
    assert_eq!(receive(&mut d, 3).0.wake, Wake::default());
}

#[test]
fn fallible_constructor_and_compact_entries() {
    let d = LineDiscipline::try_new(settings()).unwrap();
    d.check_invariants();
    assert!(matches!(LineDiscipline::with_capacity(settings(), usize::MAX), Err(NoMemory)));
    assert_eq!(size_of::<Entry>(), 3);
}

#[test]
fn input_storage_capacity_never_grows() {
    let mut d = tty(settings());
    let capacity = d.input.capacity();
    let mut out = Sink::new(0);
    for cycle in 0..12 {
        for _ in 0..5000 {
            let _ = d.receive(b'x', &mut out);
        }
        let _ = d.receive(18, &mut out);
        let _ = d.receive(21, &mut out);
        for _ in 0..4095 {
            let _ = d.receive(4, &mut out);
        }
        for _ in 0..BUFFER_SIZE {
            if !d.poll_readable() {
                break;
            }
            let _ = d.read(&mut [0; 7]);
        }
        assert!(!d.poll_readable());
        let _ = d.set_termios(raw(), false);
        fill(&mut d, b'a', 4095);
        let _ = d.read(&mut [0; 11]);
        fill(&mut d, b'b', 11);
        let _ = d.set_termios(settings(), false);
        let _ = d.read(&mut [0; BUFFER_SIZE]);
        let _ = d.set_termios(settings(), cycle % 2 == 0);
        assert_eq!(d.input.capacity(), capacity);
        d.check_invariants();
    }
}

/// Separate pending bytes and completed vectors, deliberately without the
/// production ring, Kind or column machinery. All drain loops have bounds.
#[test]
fn randomized_canonical_reference_model() {
    let mut t = with_lflag(ICANON | ECHO | ECHOE);
    t.c_iflag = ICRNL;
    t.c_oflag = 0;
    let mut d = tty(t);
    let mut pending = Vec::new();
    let mut lines: VecDeque<Vec<u8>> = VecDeque::new();
    let alphabet = b"abc xyz\x7f\r\n\x04\x15";
    let mut seed = 0x340badcafeu64;
    for step in 0..30_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let byte = alphabet[seed as usize % alphabet.len()];
        let b = if byte == b'\r' { b'\n' } else { byte };
        let expected = match b {
            127 => {
                if pending.pop().is_some() {
                    b"\x08 \x08".to_vec()
                } else {
                    Vec::new()
                }
            }
            21 => {
                let echo = if pending.is_empty() { Vec::new() } else { vec![21] };
                pending.clear();
                echo
            }
            4 => {
                lines.push_back(core::mem::take(&mut pending));
                Vec::new()
            }
            b'\n' => {
                pending.push(b);
                lines.push_back(core::mem::take(&mut pending));
                vec![b]
            }
            _ => {
                pending.push(b);
                vec![b]
            }
        };
        assert_eq!(receive(&mut d, byte).1, expected, "echo {step}");
        assert_eq!(d.poll_readable(), !lines.is_empty());
        d.check_invariants();
        if seed & 3 == 0 {
            let mut buf = [0; 7];
            let size = (seed >> 8) as usize % 8;
            let mut expected = Vec::new();
            let mut result = Read::Nothing;
            if size != 0
                && let Some(line) = lines.front_mut()
            {
                let count = size.min(line.len());
                expected.extend(line.drain(..count));
                result = NonZeroUsize::new(count).map_or(Read::Eof, Read::Bytes);
                if line.is_empty() {
                    lines.pop_front();
                }
            }
            assert_eq!(d.read(&mut buf[..size]).0, result, "read {step}");
            assert_eq!(&buf[..expected.len()], expected);
            d.check_invariants();
        }
        if step % 31 == 0 {
            for line in lines.drain(..) {
                assert_eq!(bytes(&mut d), line);
            }
        }
    }
    feed(&mut d, b"\n");
    pending.push(b'\n');
    lines.push_back(pending);
    for line in lines {
        assert_eq!(bytes(&mut d), line);
    }
    assert!(!d.poll_readable());
    d.check_invariants();
}

#[test]
fn seeded_bounded_all_entry_points_fuzz() {
    let mut seed = 0x0034_0364_dead_beef_u64;
    let mut d = tty(settings());
    let mut out = Sink::new(64);
    for step in 0..40_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.bytes.clear();
        out.events.clear();
        out.limit = (seed as usize >> 8) % 65;
        let byte = (seed >> 32) as u8;
        match seed % 10 {
            0..=3 => {
                let _ = d.accepts(byte);
                let _ = d.receive(byte, &mut out);
            }
            4 => {
                let _ = d.receive_break(&mut out);
            }
            5 => {
                let mut buf = [0; 19];
                let _ = d.read(&mut buf[..step % 20]);
            }
            6 => {
                let t = Termios {
                    c_iflag: seed as u32,
                    c_oflag: (seed >> 16) as u32,
                    c_cflag: (seed >> 32) as u32,
                    c_lflag: (seed >> 12) as u32,
                    c_line: byte,
                    c_cc: core::array::from_fn(|i| (seed.rotate_right(i as u32) >> 32) as u8),
                };
                let _ = d.set_termios(t, seed & 0x100 != 0);
            }
            7 => {
                let _ = d.write(&[byte, b'\n', b'\r', b'\t', 8, 0, 255], &mut out);
            }
            8 => {
                for timer in [TimerState::Idle, TimerState::Running, TimerState::Expired] {
                    let _ = d.read_decision(step % 300, timer);
                }
                let _ = d.poll_readable();
                let _ = d.input_room();
            }
            _ => {
                let ws = Winsize {
                    ws_row: seed as u16,
                    ws_col: (seed >> 16) as u16,
                    ws_xpixel: (seed >> 32) as u16,
                    ws_ypixel: (seed >> 48) as u16,
                };
                let _ = d.set_winsize(ws);
                assert_eq!(d.winsize(), ws);
            }
        }
        d.check_invariants();
        assert!(out.bytes.len() <= out.limit);
    }
}
