use super::*;
use alloc::{vec, vec::Vec};

#[derive(Debug)]
struct Sink {
    bytes: Vec<u8>,
    limit: usize,
    occupied: usize,
    events: Vec<&'static str>,
}
impl Sink {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit: limit.max(MAX_UNIT),
            occupied: MAX_UNIT.saturating_sub(limit),
            events: Vec::new(),
        }
    }
}
impl Output for Sink {
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.limit - self.occupied - self.bytes.len() {
            return false;
        }
        self.events.push("push");
        self.bytes.extend_from_slice(bytes);
        true
    }
    fn discard(&mut self) {
        self.events.push("discard");
        self.occupied = 0;
        self.bytes.clear();
    }
}
const INTR: u8 = 3;
const EOF: u8 = 4;
const REPRINT: u8 = 18;
const KILL: u8 = 21;
const LNEXT: u8 = 22;
const WERASE: u8 = 23;
const ERASE: u8 = 127;

fn line() -> LineDiscipline {
    tty(settings())
}
fn raw_timed(min: u8, time: u8) -> Termios {
    let mut t = raw();
    t.c_cc[VMIN as usize] = min;
    t.c_cc[VTIME as usize] = time;
    t
}
fn got(n: usize) -> Read {
    Read::Bytes(NonZeroUsize::new(n).unwrap())
}
fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}
fn fresh(d: &LineDiscipline, len: usize) -> Decision {
    d.read_decision(len, &mut ReadTimer::new())
}
fn signal(kind: JobSignalKind, flush: bool) -> Input {
    Input::Signal(JobSignal { kind, flush })
}

fn settings() -> Termios {
    let mut t = Termios::console_default();
    t.c_lflag |= IEXTEN | ECHOKE | ECHOCTL;
    t.c_cc[VWERASE as usize] = WERASE;
    t.c_cc[VLNEXT as usize] = LNEXT;
    t.c_cc[VREPRINT as usize] = REPRINT;
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
    let result = d.read(&mut buf).read;
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
    let mut out = Sink::new(65536);
    for _ in 0..count {
        let _ = d.receive(byte, &mut out);
    }
    d.check_invariants();
}

#[test]
fn console_default_matches_kernel() {
    let t = Termios::console_default();
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
    assert_eq!(effect.input, signal(JobSignalKind::Int, true));
    assert_eq!(echo, b"^C");
}

#[test]
fn canonical_erase_and_short_reads() {
    let mut d = tty(settings());
    assert_eq!(feed(&mut d, b"ab\x7fc"), b"ab\x08 \x08c");
    assert_eq!(fresh(&d, 4), Decision::Wait);
    assert_eq!(d.read(&mut [0; 8]).read, Read::Nothing);
    assert!(receive(&mut d, b'\n').0.wake.readers);
    feed(&mut d, b"de\npartial");
    let mut one = [0];
    assert_eq!(d.read(&mut one).read, got(1));
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
    assert_eq!(d.read(&mut buf).read, Read::Nothing);
    assert_eq!(receive(&mut d, 4).1, b"");
    assert_eq!(fresh(&d, 1), Decision::Read);
    assert_eq!(d.read(&mut []).read, Read::Nothing);
    assert!(d.poll_readable());
    assert_eq!(d.read(&mut buf).read, Read::Eof);
    assert!(!d.poll_readable());
    feed(&mut d, b"abc\x04next\n");
    assert_eq!(fresh(&d, 2), Decision::Read);
    assert_eq!(d.read(&mut buf).read, got(2));
    assert_eq!(&buf, b"ab");
    let mut one = [0];
    assert_eq!(d.read(&mut one).read, got(1));
    assert_eq!(&one, b"c");
    assert_eq!(bytes(&mut d), b"next\n");
    feed(&mut d, b"\x04\x04x\n");
    assert_eq!(d.read(&mut buf).read, Read::Eof);
    assert_eq!(d.read(&mut buf).read, Read::Eof);
    assert_eq!(bytes(&mut d), b"x\n");
    assert_eq!(d.read(&mut buf).read, Read::Nothing);
    let mut raw = tty(raw());
    assert_eq!(raw.read(&mut buf).read, Read::Nothing);
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
    assert_eq!(effect.input, Input::Stored);
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
    let mut d = tty(raw());
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
fn isig_discard_precedes_echo_and_noflsh_preserves_output() {
    for (byte, kind, number) in
        [(INTR, JobSignalKind::Int, SIGINT), (28, JobSignalKind::Quit, SIGQUIT), (26, JobSignalKind::Tstp, SIGTSTP)]
    {
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
            assert_eq!(e.input, signal(kind, !noflush));
            assert_eq!(JobSignal { kind, flush: !noflush }.number(), number);
            assert_eq!(out.events, if noflush { vec!["push"] } else { vec!["discard", "push"] });
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
    assert_eq!(e.input, signal(JobSignalKind::Int, true));
    assert_eq!(e.echo, Echo::Complete);
    assert!(out.bytes.is_empty());
    assert_eq!(out.events, ["discard"]);
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
            assert_eq!(e.input, signal(JobSignalKind::Int, true));
        } else {
            assert!(!matches!(e.input, Input::Signal(_)));
        }
        assert_eq!(
            e.input,
            if flags == 0 {
                Input::Stored
            } else if flags == BRKINT {
                signal(JobSignalKind::Int, true)
            } else {
                Input::Ignored
            }
        );
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
    assert_eq!(e.input, signal(JobSignalKind::Int, false));
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
    assert!(d.accepts(b'b'));
    assert!(d.accepts(b'\n'));
    assert_eq!(receive(&mut d, b'b').0.input, Input::Stored);
    assert_eq!(receive(&mut d, b'c').0.input, Input::Replaced);
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
    assert!(d.accepts(b'x'));
    assert!(d.accepts(b'\r'));
    assert!(d.accepts(4));
    for byte in [127, 23, 21, 3, 22, 18] {
        assert!(d.accepts(byte));
    }
    let _ = receive(&mut d, 22);
    assert!(d.accepts(b'\n'));
    let _ = receive(&mut d, b'\n');
    assert!(!d.poll_readable());
    let mut t = settings();
    t.c_iflag |= IGNCR;
    let _ = d.set_termios(t, Flush::Keep);
    let e = receive(&mut d, b'\r').0;
    assert_eq!(e.input, Input::Ignored);
    assert!(d.accepts(b'\r'));
    t.c_iflag |= ISTRIP;
    let _ = d.set_termios(t, Flush::Keep);
    assert!(d.accepts(0x83));
    t.c_cc = [0; NCCS as usize];
    let _ = d.set_termios(t, Flush::Keep);
    assert!(d.accepts(0));
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
fn onlcr_without_opost_preserves_lf() {
    let mut t = raw();
    t.c_oflag = ONLCR;
    let mut d = tty(t);
    assert_eq!(write(&mut d, b"\n"), b"\n");
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
    let mut d = line();
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
    let _ = d.set_termios(raw(), Flush::Keep);
    assert_eq!(bytes(&mut d), b"a\nb\0partial");
    feed(&mut d, b"raw");
    let _ = d.set_termios(settings(), Flush::Keep);
    feed(&mut d, b"new\x7f\n");
    assert_eq!(bytes(&mut d), b"raw");
    assert_eq!(bytes(&mut d), b"ne\n");
    feed(&mut d, b"part\x16");
    assert!(d.set_termios(raw(), Flush::Keep).readers);
    let _ = d.set_termios(settings(), Flush::Keep);
    assert_eq!(bytes(&mut d), b"part");
    assert!(matches!(receive(&mut d, INTR).0.input, Input::Signal(_)));
    let _ = d.set_termios(raw(), Flush::Keep);
    let _ = d.set_termios(settings(), Flush::Keep);
    assert!(!d.poll_readable());
}

#[test]
fn fallible_constructor_and_compact_entries() {
    let d = LineDiscipline::try_new(settings()).unwrap();
    d.check_invariants();
    assert!(matches!(LineDiscipline::with_capacity(settings(), usize::MAX), Err(NoMemory)));
    // Checks this rustc layout, not a public layout guarantee.
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
        let _ = d.set_termios(raw(), Flush::Keep);
        fill(&mut d, b'a', 4095);
        let _ = d.read(&mut [0; 11]);
        fill(&mut d, b'b', 11);
        let _ = d.set_termios(settings(), Flush::Keep);
        let _ = d.read(&mut [0; BUFFER_SIZE]);
        let _ = d.set_termios(settings(), if cycle % 2 == 0 { Flush::Input } else { Flush::Keep });
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
        next(&mut seed);
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
            assert_eq!(d.read(&mut buf[..size]).read, result, "read {step}");
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

fn armed(time: u8, kind: TimerKind) -> Decision {
    Decision::WaitWithTimer { deciseconds: core::num::NonZeroU8::new(time).unwrap(), kind }
}

#[test]
fn all_four_vmin_vtime_cases_and_timer_arming() {
    let mut d = tty(raw_timed(0, 0));
    let mut timer = ReadTimer::new();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(d.read(&mut [0; 9]).read, Read::Nothing);
    feed(&mut d, b"ab");
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(bytes(&mut d), b"ab");

    let mut d = tty(raw_timed(3, 0));
    let mut timer = ReadTimer::new();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Wait);
    feed(&mut d, b"ab");
    timer.expire();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Wait); // no timer was armed
    let _ = receive(&mut d, b'c');
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(bytes(&mut d), b"abc");

    let mut d = tty(raw_timed(0, 5));
    let mut timer = ReadTimer::new();
    assert_eq!(d.read_decision(9, &mut timer), armed(5, TimerKind::Read));
    for _ in 0..3 {
        assert_eq!(d.read_decision(9, &mut timer), Decision::Wait);
    }
    timer.expire();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(d.read(&mut [0; 9]).read, Read::Nothing);
    let mut timer = ReadTimer::new();
    assert_eq!(d.read_decision(9, &mut timer), armed(5, TimerKind::Read));
    let _ = receive(&mut d, b'x');
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);

    let mut d = tty(raw_timed(3, 2));
    let mut timer = ReadTimer::new();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Wait);
    let _ = receive(&mut d, b'a');
    assert_eq!(d.read_decision(9, &mut timer), armed(2, TimerKind::InterByte));
    assert_eq!(d.read_decision(9, &mut timer), Decision::Wait);
    let _ = receive(&mut d, b'b');
    assert_eq!(d.read_decision(9, &mut timer), armed(2, TimerKind::InterByte));
    timer.expire();
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(bytes(&mut d), b"ab");
    let mut timer = ReadTimer::new();
    feed(&mut d, b"abc");
    assert_eq!(d.read_decision(9, &mut timer), Decision::Read);
    assert_eq!(bytes(&mut d), b"abc");
}

#[test]
fn reader_owned_timers_restart_on_input_flush_and_settings() {
    let mut d = tty(raw_timed(3, 2));
    let mut first = ReadTimer::new();
    let mut second = ReadTimer::new();
    let _ = receive(&mut d, b'a');
    assert_eq!(d.read_decision(8, &mut first), armed(2, TimerKind::InterByte));
    assert_eq!(d.read_decision(8, &mut second), armed(2, TimerKind::InterByte));
    first.expire();
    assert_eq!(d.read_decision(8, &mut first), Decision::Read);
    assert_eq!(d.read_decision(8, &mut second), Decision::Wait);
    let _ = receive(&mut d, b'b');
    assert_eq!(d.read_decision(8, &mut first), armed(2, TimerKind::InterByte));
    assert_eq!(d.read_decision(8, &mut second), armed(2, TimerKind::InterByte));
    let _ = d.set_termios(d.termios(), Flush::Keep);
    assert_eq!(d.read_decision(8, &mut first), Decision::Wait);
    first.expire();
    let _ = d.flush_input();
    assert_eq!(d.read_decision(8, &mut first), Decision::Wait);
    let _ = receive(&mut d, b'c');
    assert_eq!(d.read_decision(8, &mut first), armed(2, TimerKind::InterByte));
    // Flush + new input without an intervening decision must rearm too.
    first.expire();
    let _ = d.flush_input();
    let _ = receive(&mut d, b'd');
    assert_eq!(d.read_decision(8, &mut first), armed(2, TimerKind::InterByte));
    let _ = d.set_termios(raw_timed(3, 7), Flush::Keep);
    assert_eq!(d.read_decision(8, &mut first), armed(7, TimerKind::InterByte));
    d.stored = u64::MAX;
    let mut wrapped = ReadTimer::new();
    assert_eq!(d.read_decision(8, &mut wrapped), armed(7, TimerKind::InterByte));
    let _ = receive(&mut d, b'e');
    assert_eq!(d.stored, 0);
    assert_eq!(d.read_decision(8, &mut wrapped), armed(7, TimerKind::InterByte));
}

#[test]
fn decisions_clamp_destinations_and_require_consuming_eof() {
    let mut d = tty(raw_timed(3, 0));
    feed(&mut d, b"ab");
    assert_eq!(fresh(&d, 4096), Decision::Wait);
    assert_eq!(fresh(&d, 2), Decision::Read);
    assert_eq!(d.read(&mut [0; 2]).read, got(2));
    for t in [settings(), raw_timed(3, 4)] {
        let mut d = tty(t);
        feed(&mut d, b"x");
        assert_eq!(fresh(&d, 0), Decision::Read);
        assert_eq!(d.read(&mut []).read, Read::Nothing);
        assert_eq!(d.input.len(), 1);
    }
    let mut d = line();
    let _ = receive(&mut d, EOF);
    assert_eq!(fresh(&d, 1), Decision::Read);
    assert_eq!(fresh(&d, 1), Decision::Read);
    assert_eq!(d.read(&mut [0]).read, Read::Eof);
    assert_eq!(fresh(&d, 1), Decision::Wait);
}

#[test]
fn wake_all_levels_cover_short_readers_and_multiple_lines() {
    let mut d = tty(raw_timed(3, 0));
    for byte in b"ab" {
        assert!(receive(&mut d, *byte).0.wake.readers);
    }
    assert_eq!(fresh(&d, 2), Decision::Read);
    assert!(!d.poll_readable());
    let read = d.read(&mut [0]);
    assert!(read.wake_writers);
    assert_eq!(read.read, got(1));
    assert!(d.read(&mut [0]).wake_writers);
    assert!(!d.read(&mut [0]).wake_writers);
    let mut d = line();
    for byte in b"a\nb\n" {
        assert!(receive(&mut d, *byte).0.wake.readers);
    }
    assert_eq!(bytes(&mut d), b"a\n");
    assert_eq!(bytes(&mut d), b"b\n");
    for action in [ERASE, WERASE, KILL, INTR] {
        feed(&mut d, b"xy");
        assert!(receive(&mut d, action).0.wake.writers);
        let _ = d.flush_input();
    }
    assert!(receive(&mut d, INTR).0.wake.writers); // flushing even an empty queue
    let _ = receive(&mut d, EOF);
    assert!(d.read(&mut [0]).wake_writers);
    let mut t = settings();
    t.c_iflag = 0;
    let _ = d.set_termios(t, Flush::Keep);
    let mut out = Sink::new(8);
    assert!(d.receive_break(&mut out).wake.readers);
}

#[test]
fn probe_b_all_termios_changes_wake_both_queues() {
    for (before, after) in
        [(settings(), raw_timed(0, 0)), (settings(), raw_timed(0, 5)), (raw_timed(1, 0), raw_timed(0, 0))]
    {
        let mut d = tty(before);
        assert_eq!(fresh(&d, 16), Decision::Wait);
        assert_eq!(d.set_termios(after, Flush::Keep), Wake { readers: true, writers: true });
        assert_ne!(fresh(&d, 16), Decision::Wait);
        assert_eq!(d.set_termios(after, Flush::Keep), Wake { readers: true, writers: true });
    }
}

#[test]
fn probe_c_overflow_reports_every_replacement_and_bel() {
    let mut d = line();
    fill(&mut d, b'a', 4095);
    assert_eq!(receive(&mut d, b'b').0.input, Input::Stored);
    assert_eq!(receive(&mut d, b'c').0.input, Input::Replaced);
    assert_eq!(receive(&mut d, b'\n').0.input, Input::Replaced);
    let data = bytes(&mut d);
    assert_eq!(data.len(), 4096);
    assert!(data[..4095].iter().all(|&b| b == b'a'));
    assert_eq!(data[4095], b'\n');
    let mut t = settings();
    t.c_iflag |= IMAXBEL;
    let _ = d.set_termios(t, Flush::Input);
    fill(&mut d, b'a', 4096);
    let stored = d.stored;
    let (e, echo) = receive(&mut d, b'z');
    assert_eq!(e.input, Input::Replaced);
    assert_eq!(d.stored, stored.wrapping_add(1));
    assert!(e.wake.readers && e.wake.writers);
    assert_eq!(echo, [BEL, b'z']);
}

#[test]
fn probes_a_f_master_and_reader_finish_long_and_quoted_lines() {
    for quoted in [false, true] {
        let mut d = line();
        let mut out = Sink::new(65536);
        let mut input = vec![b'a'; 5000];
        if quoted {
            input.insert(4095, LNEXT);
            input.insert(4096, INTR);
        }
        input.push(b'\n');
        let mut sent = 0;
        let mut read = false;
        let mut timer = ReadTimer::new();
        for step in 0..10_000 {
            if let Some(&byte) = input.get(sent)
                && d.accepts(byte)
            {
                let e = d.receive(byte, &mut out);
                assert_ne!(e.input, Input::Dropped);
                assert!(!matches!(e.input, Input::Signal(_)));
                sent += 1;
            }
            if d.read_decision(4096, &mut timer) == Decision::Read {
                let mut buf = [0; 4096];
                let outcome = d.read(&mut buf);
                assert_eq!(outcome.read, got(4096));
                assert_eq!(buf[4095], b'\n');
                assert!(buf[..4095].iter().all(|&b| b == b'a'));
                read = true;
            }
            if sent == input.len() && read {
                break;
            }
            assert!(step + 1 < 10_000, "master or reader stalled at byte {sent}");
        }
        assert_eq!(sent, input.len());
        assert!(read);
    }
    let mut d = line();
    fill(&mut d, b'a', 4095);
    let _ = receive(&mut d, LNEXT);
    for byte in 0..=255 {
        assert!(d.accepts(byte));
    }
    assert_eq!(receive(&mut d, INTR).0.input, Input::Stored);
}

#[test]
fn mode_switch_at_full_capacity_and_canonical_changes() {
    let mut d = line();
    fill(&mut d, b'a', 4096);
    let _ = d.set_termios(raw(), Flush::Keep);
    assert_eq!(d.readable_bytes(), 4096);
    assert_eq!(d.read(&mut [0; 2]).read, got(2));
    let _ = d.set_termios(settings(), Flush::Keep);
    assert_eq!(receive(&mut d, ERASE).0.input, Input::Ignored);
    assert_eq!(d.readable_bytes(), 4094); // synthetic record cannot be edited
    let _ = receive(&mut d, b'b');
    assert_eq!(bytes(&mut d), vec![b'a'; 4094]);
    let _ = receive(&mut d, b'\n');
    assert_eq!(bytes(&mut d), b"b\n");
    for complete in [false, true] {
        let mut d = line();
        if complete {
            feed(&mut d, b"done\n");
        }
        feed(&mut d, b"part");
        let mut t = settings();
        t.c_iflag ^= IGNCR;
        assert_eq!(d.set_termios(t, Flush::Keep), Wake { readers: true, writers: true });
        assert_eq!(d.readable_bytes(), if complete { 5 } else { 0 });
        if complete {
            assert_eq!(bytes(&mut d), b"done\n");
        }
        assert_eq!(fresh(&d, 8), Decision::Wait);
        assert_eq!(d.read(&mut [0; 8]).read, Read::Nothing);
        let _ = receive(&mut d, b'\n');
        assert_eq!(bytes(&mut d), b"part\n");
    }
}

#[test]
fn flush_input_preserves_literal_next_and_allows_interrupted_drain() {
    for flush in [Flush::Keep, Flush::Input] {
        let mut d = line();
        feed(&mut d, b"abc");
        assert_eq!(d.set_termios(raw(), flush), Wake { readers: true, writers: true });
        assert_eq!(bytes(&mut d), if flush == Flush::Keep { &b"abc"[..] } else { &b""[..] });
    }
    for via_termios in [false, true] {
        let mut d = line();
        feed(&mut d, &[b'x', LNEXT]);
        let wake = if via_termios { d.set_termios(settings(), Flush::Input) } else { d.flush_input() };
        assert!(wake.writers);
        assert_eq!(d.termios(), settings());
        assert_eq!(d.input.len(), 0);
        assert_eq!(receive(&mut d, INTR).0.input, Input::Stored);
        let _ = receive(&mut d, b'\n');
        assert_eq!(bytes(&mut d), [INTR, b'\n']);
    }
    let mut d = line();
    feed(&mut d, b"pending");
    let _ = d.flush_input(); // wait_until_sent can be interrupted here
    assert_eq!(d.termios(), settings());
    assert_eq!(d.input.len(), 0);
}

#[test]
fn readable_bytes_excludes_eof_and_unfinished_canonical_input() {
    let mut d = line();
    feed(&mut d, b"one\nxx\x04\x04tail");
    assert_eq!(d.readable_bytes(), 6);
    assert_eq!(d.read(&mut [0; 2]).read, got(2));
    assert_eq!(d.readable_bytes(), 4);
    assert_eq!(bytes(&mut d), b"e\n");
    assert_eq!(d.readable_bytes(), 2);
    assert_eq!(bytes(&mut d), b"xx");
    assert_eq!(d.readable_bytes(), 0);
    assert!(d.poll_readable());
    assert_eq!(d.read(&mut [0]).read, Read::Eof);
    assert_eq!(d.readable_bytes(), 0);
    assert!(!d.poll_readable());
    let _ = d.set_termios(raw(), Flush::Keep);
    assert_eq!(d.readable_bytes(), 4);
    let _ = receive(&mut d, 0);
    assert_eq!(d.readable_bytes(), 5);
    let _ = d.set_termios(settings(), Flush::Keep);
    assert_eq!(bytes(&mut d), b"tail\0"); // documented synthetic-line difference
    let _ = d.set_termios(raw(), Flush::Keep);
    let _ = receive(&mut d, 0);
    let _ = d.set_termios(settings(), Flush::Keep);
    assert_eq!(d.read(&mut [0]).read, got(1));
}

#[test]
fn newline_precedes_eof_and_visual_kill_clears_stray_continuations() {
    let mut t = settings();
    t.c_cc[VEOF as usize] = b'\n';
    let mut d = tty(t);
    assert_eq!(receive(&mut d, b'\n').1, b"\r\n");
    assert_eq!(bytes(&mut d), b"\n");
    let mut d = line();
    feed(&mut d, b"done\n\x80\x81");
    assert_eq!(receive(&mut d, KILL).1, b"");
    assert_eq!(d.pending, 0);
    assert_eq!(bytes(&mut d), b"done\n");
    let _ = receive(&mut d, b'\n');
    assert_eq!(bytes(&mut d), b"\n");
}

#[test]
fn erase_uses_only_actually_echoed_widths_across_echo_changes() {
    let mut d = line();
    let _ = receive(&mut d, b'a');
    let mut blocked = Sink::new(0);
    assert_eq!(d.receive(b'b', &mut blocked).echo, Echo::Truncated);
    assert_eq!(receive(&mut d, ERASE).1, b"");
    assert_eq!(d.column, 1);
    assert_eq!(receive(&mut d, ERASE).1, b"\x08 \x08");
    assert_eq!(d.column, 0);
    let mut t = settings();
    t.c_lflag &= !ECHO;
    let _ = d.set_termios(t, Flush::Keep);
    let _ = receive(&mut d, b'x');
    let _ = d.set_termios(settings(), Flush::Keep);
    assert_eq!(receive(&mut d, ERASE).1, b"");
    let _ = receive(&mut d, b'y');
    let _ = d.set_termios(t, Flush::Keep);
    assert_eq!(receive(&mut d, ERASE).1, b"");
    assert_eq!(d.column, 1);
    // Width is recorded under the old ECHOCTL, not recomputed when erasing.
    let _ = d.set_termios(settings(), Flush::Keep);
    let _ = receive(&mut d, 1);
    let mut plain = settings();
    plain.c_lflag &= !ECHOCTL;
    let _ = d.set_termios(plain, Flush::Keep);
    assert_eq!(receive(&mut d, ERASE).1, b"\x08 \x08".repeat(2));
}

#[test]
fn control_gates_reprint_pending_only_and_editing_fates() {
    let mut d = line();
    feed(&mut d, b"done\nab");
    assert_eq!(
        receive(&mut d, REPRINT),
        (Effects { input: Input::Ignored, echo: Echo::Complete, wake: Wake::default() }, b"^R\r\nab".to_vec())
    );
    let mut t = settings();
    t.c_lflag &= !ECHOCTL;
    let mut d = tty(t);
    assert_eq!(receive(&mut d, LNEXT).1, b"");
    assert_eq!(receive(&mut d, INTR).0.input, Input::Stored);
    let _ = receive(&mut d, b'\n');
    assert_eq!(bytes(&mut d), [INTR, b'\n']);
    t.c_lflag &= !ECHO;
    let mut d = tty(t);
    assert_eq!(receive(&mut d, REPRINT).0.input, Input::Stored); // disabled reprint becomes data
    let _ = receive(&mut d, b'\n');
    assert_eq!(bytes(&mut d), [REPRINT, b'\n']);
    for control in [ERASE, WERASE, KILL, LNEXT, REPRINT] {
        let mut d = line();
        feed(&mut d, b"ab");
        assert_eq!(receive(&mut d, control).0.input, Input::Ignored);
    }
    for byte in [0xa0, 0xaa, 0xb5, 0xbf] {
        let mut t = settings();
        t.c_iflag &= !IUTF8; // Latin-1 bytes, not UTF-8 continuation bytes.
        let mut d = tty(t);
        feed(&mut d, &[b'a', byte, b'b']);
        let _ = receive(&mut d, WERASE);
        let _ = receive(&mut d, b'\n');
        assert_eq!(bytes(&mut d), [b'a', byte, b'\n']);
    }
}

#[test]
fn isig_precedes_igncr_and_eol_del_echo_rules() {
    let mut t = settings();
    t.c_iflag |= IGNCR;
    let mut d = tty(t);
    assert_eq!(receive(&mut d, b'\r').0.input, Input::Ignored);
    assert_eq!(d.input.len(), 0);
    t.c_cc[VINTR as usize] = b'\r';
    let _ = d.set_termios(t, Flush::Keep);
    assert_eq!(receive(&mut d, b'\r').0.input, signal(JobSignalKind::Int, true));
    t = settings();
    t.c_cc[VEOL as usize] = b';';
    t.c_cc[VEOL2 as usize] = b':';
    let mut d = tty(t);
    assert_eq!(receive(&mut d, b';').1, b";");
    assert_eq!(receive(&mut d, b':').1, b":");
    assert_eq!(bytes(&mut d), b";");
    assert_eq!(bytes(&mut d), b":");
    t.c_lflag &= !ECHO;
    let mut d = tty(t);
    assert_eq!(receive(&mut d, b';').1, b"");
    assert_eq!(receive(&mut d, b':').1, b"");
    let mut d = tty(raw());
    assert_eq!(write(&mut d, b"a\x7f"), b"a\x7f");
    assert_eq!(d.column, 1);
}

#[test]
fn output_max_unit_and_refusal_preserve_prefix_and_column() {
    let mut out = Sink::new(MAX_UNIT);
    assert!(out.push(&[b'x'; MAX_UNIT]));
    assert!(!out.push(b"z"));
    assert_eq!(out.bytes, [b'x'; MAX_UNIT]);
    out.discard();
    assert!(out.push(&[b'y'; MAX_UNIT]));
    let mut d = line();
    let mut out = Sink::new(1);
    assert_eq!(d.write(b"\n", &mut out).consumed, 0);
    assert_eq!(d.column, 0);
    assert!(out.bytes.is_empty());
    out.discard();
    assert_eq!(d.write(b"\n", &mut out).consumed, 1);
    assert_eq!(out.bytes, b"\r\n");
}

fn snapshot(d: &LineDiscipline) -> LineDiscipline {
    let mut input = d.input.clone();
    input.try_reserve_exact(BUFFER_SIZE - input.len()).unwrap();
    LineDiscipline {
        termios: d.termios,
        input,
        pending: d.pending,
        records: d.records,
        literal: d.literal,
        column: d.column,
        winsize: d.winsize,
        stored: d.stored,
    }
}

fn acceptance_oracle(d: &LineDiscipline) {
    for byte in 0..=255 {
        let mut clone = snapshot(d);
        let mut out = Sink::new(32768);
        let before_len = clone.input.len();
        let before_stored = clone.stored;
        let e = clone.receive(byte, &mut out);
        assert_eq!(
            d.accepts(byte),
            e.input != Input::Dropped,
            "byte {byte}, queued {before_len}, literal {}",
            d.literal
        );
        match e.input {
            Input::Stored => {
                assert_eq!(clone.input.len(), before_len + 1);
                assert_eq!(clone.stored, before_stored.wrapping_add(1));
            }
            Input::Replaced => {
                assert_eq!(clone.input.len(), before_len);
                assert_eq!(clone.stored, before_stored.wrapping_add(1));
            }
            Input::Dropped => {
                assert_eq!(clone.input.len(), before_len);
                assert_eq!(clone.stored, before_stored);
                assert!(
                    clone
                        .input
                        .iter()
                        .zip(&d.input)
                        .all(|(a, b)| a.byte == b.byte && a.kind == b.kind && a.width == b.width)
                );
            }
            _ => {}
        }
        assert_eq!(e.echo, Echo::Complete); // largest possible echo fits this sink
        clone.check_invariants();
    }
}

#[test]
fn seeded_probes_d_e_f_check_acceptance_wakes_and_full_buffers() {
    let mut seed = 0x0034_0364_dead_beef_u64;
    let mut d = line();
    let mut out = Sink::new(32768);
    let alphabet = [
        b'a', b'b', b' ', b'.', b'\t', 0xc3, 0xa9, 0x80, b'\n', b'\r', INTR, EOF, KILL, LNEXT, WERASE, REPRINT, ERASE,
        0, 255, b';',
    ];
    let mut hit4095 = false;
    let mut hit4096 = false;
    let mut replaced = false;
    // Finite whole-buffer phases exercise the old fuzz's missing boundary.
    for size in [4095, 4096, 4100] {
        let _ = d.set_termios(settings(), Flush::Input);
        for _ in 0..size {
            let e = d.receive(b'a', &mut out);
            replaced |= e.input == Input::Replaced;
        }
        hit4095 |= d.input.len() == 4095;
        hit4096 |= d.input.len() == 4096;
        d.check_invariants();
        acceptance_oracle(&d);
        let _ = receive(&mut d, LNEXT);
        acceptance_oracle(&d);
        let _ = receive(&mut d, INTR);
        acceptance_oracle(&d);
        out.discard();
        out.events.clear();
    }
    let _ = d.set_termios(raw(), Flush::Input);
    fill(&mut d, b'a', 4095);
    acceptance_oracle(&d);
    let _ = d.set_termios(settings(), Flush::Input);
    feed(&mut d, b"done\n");
    fill(&mut d, b'a', 4090);
    acceptance_oracle(&d); // completed line blocks further storage
    for step in 0..512 {
        let r = next(&mut seed);
        let accepted_before: [bool; 256] = core::array::from_fn(|i| d.accepts(i as u8));
        let decisions_before = [fresh(&d, 1), fresh(&d, 4096)];
        let mut out = Sink::new((r as usize >> 8) % 17);
        let mut read_operation = false;
        let wake = match r % 10 {
            0..=3 => {
                let byte = if r & 0x100 == 0 { alphabet[(r >> 20) as usize % alphabet.len()] } else { (r >> 32) as u8 };
                d.receive(byte, &mut out).wake
            }
            4 => d.receive_break(&mut out).wake,
            5 => {
                read_operation = true;
                let len = step % 65;
                let mut buf = [0; 64];
                let decision = fresh(&d, len);
                let result = d.read(&mut buf[..len]);
                if len == 0 {
                    assert_eq!(decision, Decision::Read);
                    assert_eq!(result.read, Read::Nothing);
                }
                if d.local(ICANON) && decision == Decision::Wait {
                    assert_eq!(result.read, Read::Nothing);
                }
                Wake { readers: false, writers: result.wake_writers }
            }
            6 => {
                let mut t = settings();
                t.c_lflag = (r >> 12) as u32;
                t.c_iflag = (r >> 28) as u32;
                t.c_oflag = (r >> 36) as u32;
                // Control collisions, not only random mostly-disabled keys.
                for cc in &mut t.c_cc {
                    *cc = alphabet[next(&mut seed) as usize % alphabet.len()];
                }
                t.c_cc[VMIN as usize] = (r >> 44) as u8 % 6;
                t.c_cc[VTIME as usize] = (r >> 50) as u8 % 3;
                d.set_termios(t, if r & 0x1000 == 0 { Flush::Keep } else { Flush::Input })
            }
            7 => {
                let _ = d.write(&[b'a', b'\n', b'\r', b'\t', BS, 0, 255], &mut out);
                Wake::default()
            }
            8 => d.flush_input(),
            _ => {
                let ws = Winsize {
                    ws_row: r as u16,
                    ws_col: (r >> 16) as u16,
                    ws_xpixel: (r >> 32) as u16,
                    ws_ypixel: (r >> 48) as u16,
                };
                let _ = d.set_winsize(ws);
                assert_eq!(d.winsize(), ws);
                Wake::default()
            }
        };
        if !read_operation && decisions_before != [fresh(&d, 1), fresh(&d, 4096)] {
            assert!(wake.readers, "changed reader decision at step {step}");
        }
        for byte in 0..=255 {
            if !accepted_before[byte as usize] && d.accepts(byte) {
                assert!(wake.writers, "writer stranded at step {step}, byte {byte}");
            }
        }
        d.check_invariants();
        acceptance_oracle(&d);
    }
    assert!(hit4095 && hit4096 && replaced, "full-buffer phases must reach and replace overflow");
}
