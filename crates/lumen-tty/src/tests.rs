use super::*;
use alloc::vec;

fn settings() -> Termios {
    let mut t = Termios::linux_default();
    t.c_lflag = ICANON | IEXTEN | ECHO | ECHOE | ECHOK | ECHOKE | ECHOCTL | ISIG;
    t.c_cc[VWERASE as usize] = 23;
    t.c_cc[VLNEXT as usize] = 22;
    t.c_cc[VREPRINT as usize] = 18;
    t
}
fn feed(d: &mut LineDiscipline, bytes: &[u8]) -> Vec<u8> {
    let mut echo = Vec::new();
    for &byte in bytes {
        echo.extend_from_slice(d.receive(byte).echo);
    }
    echo
}
fn read(d: &mut LineDiscipline) -> Vec<u8> {
    let mut b = [0; BUFFER_SIZE];
    let n = d.read(&mut b);
    b[..n].to_vec()
}
fn raw() -> Termios {
    let mut t = settings();
    t.c_lflag = 0;
    t.c_iflag = 0;
    t
}

#[test]
fn linux_default_matches_kernel() {
    let t = Termios::linux_default();
    // Literal values from kernel::linux::process::default_termios, including
    // the deliberately disabled extended editing keys and VEOL/VEOL2.
    assert_eq!(t.c_iflag, 0x4100);
    assert_eq!(t.c_oflag, 5);
    assert_eq!(t.c_cflag, 0xbf);
    assert_eq!(t.c_lflag, 0x3b);
    assert_eq!(t.c_line, 0);
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
    assert_eq!(abi.c_iflag, t.c_iflag);
    assert_eq!(abi.c_oflag, t.c_oflag);
    assert_eq!(abi.c_cflag, t.c_cflag);
    assert_eq!(abi.c_lflag, t.c_lflag);
    assert_eq!(abi.c_line, t.c_line);
    assert_eq!(abi.c_cc, t.c_cc);
    assert_eq!(Termios::from(abi), t);
    assert_eq!(size_of::<Termios>(), size_of::<linux_raw_sys::general::termios>());
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
        let mut d = LineDiscipline::new(t);
        feed(&mut d, input);
        assert_eq!(read(&mut d), expected, "flags {flags}");
    }
}

#[test]
fn breaks_are_distinct_from_nul() {
    for flags in [0, IGNBRK, BRKINT, IGNBRK | BRKINT] {
        let mut t = raw();
        t.c_iflag = flags;
        let mut d = LineDiscipline::new(t);
        assert_eq!(d.receive(0).signal, None);
        assert_eq!(read(&mut d), b"\0");
        feed(&mut d, b"x");
        let signal = d.receive_break().signal;
        if flags == BRKINT {
            assert_eq!(signal, Some(Signal { number: SIGINT, flush_input: true, flush_output: true }));
            assert_eq!(read(&mut d), b"");
        } else {
            assert_eq!(signal, None);
            assert_eq!(read(&mut d), if flags & IGNBRK != 0 { &b"x"[..] } else { &b"x\0"[..] });
        }
    }
    let mut t = raw();
    t.c_iflag = BRKINT;
    t.c_lflag = NOFLSH;
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"x");
    assert_eq!(d.receive_break().signal, Some(Signal { number: SIGINT, flush_input: false, flush_output: false }));
    assert_eq!(read(&mut d), b"x");
}

#[test]
fn canonical_erase_and_short_reads() {
    let mut d = LineDiscipline::new(settings());
    assert_eq!(feed(&mut d, b"ab\x7fc"), b"ab\x08 \x08c");
    assert!(!d.readable());
    assert_eq!(read(&mut d), b"");
    assert!(d.receive(b'\n').input_readable);
    feed(&mut d, b"de\npartial");
    let mut one = [0];
    assert_eq!(d.read(&mut one), 1);
    assert_eq!(one, [b'a']);
    assert!(d.readable());
    assert_eq!(read(&mut d), b"c\n");
    assert_eq!(read(&mut d), b"de\n");
    assert!(!d.readable());
    assert_eq!(read(&mut d), b"");
    assert_eq!(d.input_room(), BUFFER_SIZE - 7);
    feed(&mut d, b"\x7f\n");
    assert_eq!(read(&mut d), b"partia\n");
}

#[test]
fn word_erase_and_iexten_gate() {
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"one two_3...  ");
    assert_eq!(d.receive(23).echo, b"\x08 \x08".repeat(10));
    feed(&mut d, b"\n");
    assert_eq!(read(&mut d), b"one \n");
    let mut t = settings();
    t.c_lflag &= !IEXTEN;
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"x\x17\x16\x12\n");
    assert_eq!(read(&mut d), b"x\x17\x16\x12\n");
}

#[test]
fn kill_preserves_completed_lines() {
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"keep\nabc");
    assert_eq!(d.receive(21).echo, b"\x08 \x08".repeat(3));
    assert_eq!(d.receive(21).echo, b"");
    feed(&mut d, b"x\n");
    assert_eq!(read(&mut d), b"keep\n");
    assert_eq!(read(&mut d), b"x\n");
}

#[test]
fn eof_at_start_midline_and_exact_read_boundary() {
    let mut d = LineDiscipline::new(settings());
    assert_eq!(d.receive(4).echo, b"");
    assert!(d.readable());
    assert_eq!(d.read(&mut []), 0);
    assert!(d.readable());
    assert_eq!(read(&mut d), b"");
    assert!(!d.readable());
    feed(&mut d, b"abc\x04next\n");
    let mut two = [0; 2];
    assert_eq!(d.read(&mut two), 2);
    assert_eq!(&two, b"ab");
    let mut one = [0];
    assert_eq!(d.read(&mut one), 1);
    assert_eq!(&one, b"c");
    assert_eq!(read(&mut d), b"next\n");
    assert!(!d.readable());
    feed(&mut d, b"\x04\x04x\n");
    assert_eq!(read(&mut d), b"");
    assert!(d.readable());
    assert_eq!(read(&mut d), b"");
    assert_eq!(read(&mut d), b"x\n");
}

#[test]
fn eol_eol2_and_disabled_control_characters() {
    let mut t = settings();
    t.c_cc[VEOL as usize] = b';';
    t.c_cc[VEOL2 as usize] = b':';
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"a;b:c\n");
    assert_eq!(read(&mut d), b"a;");
    assert_eq!(read(&mut d), b"b:");
    assert_eq!(read(&mut d), b"c\n");
    t.c_lflag &= !IEXTEN;
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"a:b;");
    assert_eq!(read(&mut d), b"a:b;");
    t.c_cc = [0; 19];
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"a\0b\n");
    assert_eq!(read(&mut d), b"a\0b\n");
}

#[test]
fn literal_next_quotes_signals_delimiters_and_mapping() {
    let mut d = LineDiscipline::new(settings());
    assert_eq!(d.receive(22).echo, b"^\x08");
    let e = d.receive(3);
    assert_eq!(e.signal, None);
    assert_eq!(e.echo, b"^C");
    assert_eq!(feed(&mut d, b"\x16\x7f\x16\x04\x16\n\x16\r"), b"^\x08^?^\x08^D^\x08^J^\x08^M");
    assert!(!d.readable());
    feed(&mut d, b"\n");
    assert_eq!(read(&mut d), b"\x03\x7f\x04\n\r\n");
    let mut t = settings();
    t.c_iflag |= ISTRIP;
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"\x16\xff\n");
    assert_eq!(read(&mut d), b"\x7f\n");
}

#[test]
fn utf8_erase_removes_one_character() {
    for (flags, expected) in [(IUTF8, &b"a\n"[..]), (0, &b"a\xe2\x82\n"[..])] {
        let mut t = settings();
        t.c_iflag = flags;
        let mut d = LineDiscipline::new(t);
        feed(&mut d, "a€".as_bytes());
        assert_eq!(d.receive(127).echo, b"\x08 \x08");
        feed(&mut d, b"\n");
        assert_eq!(read(&mut d), expected);
    }
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"\x80\x81\x7f\n");
    assert_eq!(read(&mut d), b"\n");
}

#[test]
fn tab_erase_tracks_columns_and_utf8() {
    for (prefix, text, width) in [
        (&b""[..], &b"ab\t"[..], 6),
        (&b"> "[..], &b"ab\t"[..], 4),
        (&b""[..], "€\t".as_bytes(), 7),
        (&b""[..], &b"\x01\t"[..], 6),
    ] {
        let mut d = LineDiscipline::new(settings());
        d.write(prefix);
        feed(&mut d, text);
        assert_eq!(d.receive(127).echo, vec![8; width]);
        assert_eq!(d.receive(b'\t').echo, b"\t");
        assert_eq!(d.receive(127).echo, vec![8; width]);
    }
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"a\tbc\x7f\x7f");
    assert_eq!(d.receive(127).echo, [8; 7]);
}

#[test]
fn canonical_line_limit_reserves_delimiter_and_still_edits() {
    let mut d = LineDiscipline::new(settings());
    for _ in 0..4095 {
        d.receive(b'a');
    }
    assert_eq!(d.input_room(), 1);
    assert!(!d.readable());
    assert_eq!(d.receive(b'b').echo, b"b"); // echoed but discarded
    assert_eq!(d.input_room(), 1);
    d.receive(127);
    d.receive(b'c');
    assert!(d.receive(b'\n').input_readable);
    assert_eq!(d.input_room(), 0);
    assert_eq!(d.receive(b'x').echo, b"");
    let line = read(&mut d);
    assert_eq!(line.len(), 4096);
    assert!(line[..4094].iter().all(|&b| b == b'a'));
    assert_eq!(&line[4094..], b"c\n");
    assert_eq!(d.input_room(), BUFFER_SIZE);
    for _ in 0..4095 {
        d.receive(b'z');
    }
    d.receive(4);
    assert_eq!(read(&mut d), vec![b'z'; 4095]);
    assert!(!d.readable());
}

#[test]
fn full_buffers_drop_input_and_imaxbel_rings() {
    let mut t = raw();
    t.c_iflag = IMAXBEL;
    let mut d = LineDiscipline::new(t);
    for _ in 0..BUFFER_SIZE {
        d.receive(b'x');
    }
    assert_eq!(d.input_room(), 0);
    assert_eq!(d.receive(b'y').echo, b"\x07");
    assert_eq!(read(&mut d), vec![b'x'; BUFFER_SIZE]);
    t.c_iflag = 0;
    d.set_termios(t, SetWhen::Now);
    for _ in 0..BUFFER_SIZE {
        d.receive(b'z');
    }
    assert_eq!(d.receive(b'y').echo, b"");
    assert_eq!(read(&mut d), vec![b'z'; BUFFER_SIZE]);
    t = settings();
    t.c_iflag |= IMAXBEL;
    d.set_termios(t, SetWhen::Now);
    for _ in 0..4095 {
        d.receive(b'a');
    }
    assert_eq!(d.receive(b'b').echo, b"\x07");
    assert!(d.receive(21).echo.len() <= BUFFER_SIZE);
    assert_eq!(d.input_room(), BUFFER_SIZE); // kill must finish even when echo fills
    d.receive(b'\n');
    assert_eq!(read(&mut d), b"\n");
}

#[test]
fn echo_flags_are_independently_gated() {
    for flags in [0, ECHOE, ECHOK, ECHOKE, ECHOCTL, ECHONL] {
        let mut t = settings();
        t.c_lflag = ICANON | flags;
        let mut d = LineDiscipline::new(t);
        assert_eq!(feed(&mut d, b"a\x7fb\x15"), b"");
        assert_eq!(d.receive(b'\n').echo, if flags == ECHONL { &b"\r\n"[..] } else { &b""[..] });
        assert_eq!(read(&mut d), b"\n");
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
        let mut t = settings();
        t.c_lflag = ICANON | flags;
        let mut d = LineDiscipline::new(t);
        assert_eq!(d.receive(b'a').echo, b"a");
        assert_eq!(d.receive(127).echo, erase);
        d.receive(b'b');
        assert_eq!(d.receive(21).echo, kill);
        assert_eq!(d.receive(b'\n').echo, b"\r\n");
        assert_eq!(read(&mut d), b"\n");
    }
}

#[test]
fn echoctl_reprint_and_control_character_erase() {
    let mut d = LineDiscipline::new(settings());
    assert_eq!(feed(&mut d, b"\x01\x7f"), b"^A\x08 \x08\x08 \x08");
    assert_eq!(feed(&mut d, b"ab\t\x12"), b"ab\t^R\r\nab\t");
    assert_eq!(d.receive(127).echo, [8; 6]);
    feed(&mut d, b"\n");
    assert_eq!(read(&mut d), b"ab\n");
    let mut t = settings();
    t.c_lflag &= !ECHOCTL;
    let mut plain = LineDiscipline::new(t);
    assert_eq!(plain.receive(1).echo, b"\x01");
    assert_eq!(plain.receive(127).echo, b"");
    plain.receive(b'\n');
    assert_eq!(read(&mut plain), b"\n");
    let mut t = settings();
    t.c_lflag &= !ISIG;
    t.c_cc[VERASE as usize] = 8;
    let mut d = LineDiscipline::new(t);
    assert_eq!(feed(&mut d, b"\x03\x7f\t\n"), b"^C^?\t\r\n");
}

#[test]
fn isig_signals_and_noflsh() {
    for (byte, number) in [(3, SIGINT), (28, SIGQUIT), (26, SIGTSTP)] {
        for noflush in [false, true] {
            let mut t = settings();
            if noflush {
                t.c_lflag |= NOFLSH;
            }
            let mut d = LineDiscipline::new(t);
            feed(&mut d, b"old\npartial");
            let effects = d.receive(byte);
            assert_eq!(effects.signal, Some(Signal { number, flush_input: !noflush, flush_output: !noflush }));
            assert_eq!(effects.echo, &[b'^', byte + 64]);
            assert_eq!(effects.input_readable, noflush);
            feed(&mut d, b"x\n");
            if noflush {
                assert_eq!(read(&mut d), b"old\n");
                assert_eq!(read(&mut d), b"partialx\n");
            } else {
                assert_eq!(read(&mut d), b"x\n");
            }
        }
    }
    let mut t = settings();
    t.c_lflag &= !ISIG;
    let mut d = LineDiscipline::new(t);
    feed(&mut d, b"\x03\x1c\x1a\n");
    assert_eq!(read(&mut d), b"\x03\x1c\x1a\n");
    t.c_lflag = ISIG;
    let mut d = LineDiscipline::new(t);
    d.receive(b'x');
    assert_eq!(d.receive(3).signal.unwrap().number, SIGINT);
    assert_eq!(read(&mut d), b"");
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
fn raw_readiness_uses_external_timer_and_restarts_on_input() {
    let mut t = raw();
    t.c_cc[VMIN as usize] = 3;
    t.c_cc[VTIME as usize] = 2;
    let mut d = LineDiscipline::new(t);
    assert!(!d.readable());
    assert!(!d.receive(b'a').input_readable);
    assert_eq!(d.readiness(), Readiness::WaitWithTimer { deciseconds: 2, kind: TimerKind::InterByte });
    d.set_timer_state(TimerState::Running);
    assert_eq!(d.readiness(), Readiness::Wait);
    d.receive(b'b');
    assert_eq!(d.timer, TimerState::Idle);
    d.set_timer_state(TimerState::Expired);
    assert!(d.readable());
    assert_eq!(read(&mut d), b"ab");
    d.set_timer_state(TimerState::Idle);
    feed(&mut d, b"abc");
    assert!(d.readable());
    assert_eq!(read(&mut d), b"abc");
    t.c_cc[VMIN as usize] = 0;
    d.set_termios(t, SetWhen::Now);
    assert!(!d.readable());
    d.set_timer_state(TimerState::Expired);
    assert!(d.readable());
    assert_eq!(read(&mut d), b"");
    t.c_cc[VTIME as usize] = 0;
    d.set_termios(t, SetWhen::Now);
    assert!(d.readable());
    d.receive(b'x');
    assert_eq!(read(&mut d), b"x");
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
        let mut d = LineDiscipline::new(t);
        assert_eq!(d.write(input), expected, "flags {flags}");
        let mut d = LineDiscipline::new(t);
        let mut result = Vec::new();
        for b in input {
            result.extend(d.write(&[*b]));
        }
        assert_eq!(result, expected); // column persists across write calls
    }
}

#[test]
fn background_writer_sigttou_decisions() {
    for bits in 0..128 {
        let t = bits & 1 != 0;
        let facts = WriterFacts {
            controlling_terminal: bits & 2 != 0,
            background: bits & 4 != 0,
            signal_blocked: bits & 8 != 0,
            signal_ignored: bits & 16 != 0,
            orphaned_group: bits & 32 != 0,
        };
        assert_eq!(background_write_signal(t, facts), if bits & 63 == 7 { Some(SIGTTOU) } else { None });
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
fn mode_switches_publish_pending_input_in_both_directions() {
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"a\nb\x04partial");
    assert!(matches!(d.set_termios(raw(), SetWhen::Now), TermiosChange::Applied { input_readable: true, .. }));
    assert_eq!(read(&mut d), b"a\nbpartial");
    assert!(!d.readable());
    feed(&mut d, b"raw");
    d.set_termios(settings(), SetWhen::Now);
    assert!(d.readable());
    feed(&mut d, b"new\x7f\n");
    assert_eq!(read(&mut d), b"raw");
    assert_eq!(read(&mut d), b"ne\n");
    assert!(!d.readable());
    feed(&mut d, b"part\x16");
    d.set_termios(raw(), SetWhen::Now);
    assert!(d.readable());
    d.set_termios(settings(), SetWhen::Now);
    assert_eq!(read(&mut d), b"part");
    assert!(d.receive(3).signal.is_some()); // literal-next was reset
    d.set_termios(raw(), SetWhen::Now);
    d.set_termios(settings(), SetWhen::Now);
    assert!(!d.readable());
}

#[test]
fn termios_drain_flush_and_request_replacement() {
    for when in [SetWhen::Drain, SetWhen::Flush] {
        let mut d = LineDiscipline::new(settings());
        feed(&mut d, b"abc");
        assert_eq!(d.set_termios(raw(), when), TermiosChange::DrainRequired);
        assert_eq!(d.termios(), settings());
        assert!(!d.readable());
        d.receive(b'd');
        assert_eq!(
            d.output_drained(),
            Some(TermiosChange::Applied {
                flush_input: when == SetWhen::Flush,
                input_readable: when == SetWhen::Drain
            })
        );
        assert_eq!(d.termios(), raw());
        assert_eq!(read(&mut d), if when == SetWhen::Drain { &b"abcd"[..] } else { &b""[..] });
        assert_eq!(d.output_drained(), None);
    }
    let mut d = LineDiscipline::new(settings());
    feed(&mut d, b"keep");
    d.set_termios(raw(), SetWhen::Flush);
    d.set_termios(raw(), SetWhen::Now);
    assert_eq!(d.output_drained(), None);
    assert_eq!(read(&mut d), b"keep");
    d.set_termios(settings(), SetWhen::Flush);
    d.set_termios(settings(), SetWhen::Drain);
    d.receive(b'x');
    d.output_drained();
    assert_eq!(read(&mut d), b"x");
}

/// Intentionally unbounded, string-like reference: completed lines and the
/// current line are separate, with no production buffer/marker machinery.
#[test]
fn randomized_canonical_reference_model() {
    let mut t = Termios::linux_default();
    t.c_lflag = ICANON | ECHO | ECHOE;
    t.c_iflag = ICRNL;
    t.c_oflag = 0;
    let mut d = LineDiscipline::new(t);
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
        assert_eq!(d.receive(byte).echo, expected, "echo step {step}");
        assert_eq!(d.readable(), !lines.is_empty(), "readiness step {step}");
        if seed & 3 == 0 {
            let mut buf = [0; 7];
            let size = (seed >> 8) as usize % 8;
            let mut expected = Vec::new();
            if size != 0
                && let Some(line) = lines.front_mut()
            {
                let count = size.min(line.len());
                expected.extend(line.drain(..count));
                if line.is_empty() {
                    lines.pop_front();
                }
            }
            let n = d.read(&mut buf[..size]);
            assert_eq!(&buf[..n], expected, "read step {step}");
        }
        // Drain regularly, keeping this oracle about editing rather than
        // overflow (the exact buffer limits have independent boundary tests).
        if step % 31 == 0 {
            while let Some(line) = lines.pop_front() {
                assert_eq!(read(&mut d), line);
            }
        }
    }
    feed(&mut d, b"\n");
    pending.push(b'\n');
    lines.push_back(pending);
    while let Some(line) = lines.pop_front() {
        assert_eq!(read(&mut d), line);
    }
    assert!(!d.readable());
}
