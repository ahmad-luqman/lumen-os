use linux_raw_sys::general as linux;

/// Linux's kernel termios fields, including all 19 control characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; linux::NCCS as usize],
}

impl Termios {
    /// Lumen's console defaults (`kernel/src/linux/process.rs`'s
    /// `default_termios()`), not Linux's `tty_std_termios`. The latter also
    /// enables IXON, HUPCL, ECHOCTL, ECHOKE, IEXTEN and ^R/^O/^W/^V.
    pub fn linux_default() -> Self {
        let mut c_cc = [0; linux::NCCS as usize];
        c_cc[linux::VINTR as usize] = 3;
        c_cc[linux::VQUIT as usize] = 28;
        c_cc[linux::VERASE as usize] = 127;
        c_cc[linux::VKILL as usize] = 21;
        c_cc[linux::VEOF as usize] = 4;
        c_cc[linux::VMIN as usize] = 1;
        c_cc[linux::VSTART as usize] = 17;
        c_cc[linux::VSTOP as usize] = 19;
        c_cc[linux::VSUSP as usize] = 26;
        Self {
            c_iflag: linux::ICRNL | linux::IUTF8,
            c_oflag: linux::OPOST | linux::ONLCR,
            c_cflag: linux::B38400 | linux::CS8 | linux::CREAD,
            c_lflag: linux::ISIG | linux::ICANON | linux::ECHO | linux::ECHOE | linux::ECHOK,
            c_line: 0,
            c_cc,
        }
    }
}

impl From<linux::termios> for Termios {
    fn from(t: linux::termios) -> Self {
        Self {
            c_iflag: t.c_iflag,
            c_oflag: t.c_oflag,
            c_cflag: t.c_cflag,
            c_lflag: t.c_lflag,
            c_line: t.c_line,
            c_cc: t.c_cc,
        }
    }
}

impl From<Termios> for linux::termios {
    fn from(t: Termios) -> Self {
        Self {
            c_iflag: t.c_iflag,
            c_oflag: t.c_oflag,
            c_cflag: t.c_cflag,
            c_lflag: t.c_lflag,
            c_line: t.c_line,
            c_cc: t.c_cc,
        }
    }
}

/// Window dimensions; pixel dimensions participate in change detection too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl From<linux::winsize> for Winsize {
    fn from(w: linux::winsize) -> Self {
        Self { ws_row: w.ws_row, ws_col: w.ws_col, ws_xpixel: w.ws_xpixel, ws_ypixel: w.ws_ypixel }
    }
}

impl From<Winsize> for linux::winsize {
    fn from(w: Winsize) -> Self {
        Self { ws_row: w.ws_row, ws_col: w.ws_col, ws_xpixel: w.ws_xpixel, ws_ypixel: w.ws_ypixel }
    }
}
