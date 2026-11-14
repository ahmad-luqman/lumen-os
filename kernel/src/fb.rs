//! Boot framebuffer: pixels, glyphs, the "hello" header, and the crash
//! screen. Below the header, the terminal (`crate::term`) draws log lines and
//! the screen's shell.
//!
//! Text uses pre-rasterized Noto Sans Mono glyphs (anti-aliased, so we blend
//! each pixel between background and foreground). This is deliberately the
//! simplest thing that works; the real compositor and font renderer are M5.
//!
//! The header area (top-left 640x136 pixels) is deterministic and checked
//! against `tests/golden/boot-hello.png` by the screendump test.
//!
//! The crash screen draws straight to the pixels, not through the terminal,
//! its lock or the heap: the report must work when any of them is broken
//! (another CPU stopped mid-update, a corrupt heap).

use crate::sync::SpinLock;
use core::fmt::{self, Write};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

pub const BG: u32 = 0x0f1420;
const TITLE: u32 = 0xffd479;
pub const TEXT: u32 = 0xe6e6e6;
const DIM: u32 = 0x8a93a6;
const ACCENT: u32 = 0x3d7eff;
const CRASH_BG: u32 = 0x3a0b0b;
const CRASH_TITLE: u32 = 0xff6b6b;

/// Top of the terminal, below the fixed header.
pub const CONSOLE_TOP: usize = 144;
pub const MARGIN: usize = 32;

/// The boot framebuffer (32 bits per pixel). Not `Copy`: a second handle
/// (the crash screen's) comes only from [`Framebuffer::alias`].
pub struct Framebuffer {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

// SAFETY: the framebuffer is plain MMIO-like memory owned by the kernel. Two
// handles draw: the terminal's, under its lock, and the crash screen's (from
// `FRAMEBUFFER`), after the crash path stopped the terminal (`term::stop`)
// and the other CPUs, or named those that didn't stop.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// Another handle to the same pixels.
    ///
    /// # Safety
    /// The two handles must never draw at the same time.
    unsafe fn alias(&self) -> Framebuffer {
        Framebuffer { ..*self }
    }

    fn encode(&self, rgb: u32) -> u32 {
        let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
        (r << self.red_shift) | (g << self.green_shift) | (b << self.blue_shift)
    }

    fn put(&mut self, x: usize, y: usize, rgb: u32) {
        if x < self.width && y < self.height {
            let px = self.encode(rgb);
            // SAFETY: (x, y) is inside the framebuffer and pixels are 32-bit.
            unsafe { (self.base.add(y * self.pitch + x * 4) as *mut u32).write_volatile(px) }
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: u32) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                self.put(xx, yy, rgb);
            }
        }
    }

    /// Draw one glyph (`?` for one the font lacks); returns its advance
    /// width.
    fn glyph(&mut self, x: usize, y: usize, c: char, st: Style) -> usize {
        self.glyph_clipped(x, y, c, st, usize::MAX)
    }

    /// Width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// The pixel at (`x`, `y`) as stored (0 outside the framebuffer; for the
    /// tests).
    #[cfg(feature = "ktest")]
    pub fn pixel(&self, x: usize, y: usize) -> u32 {
        if x < self.width && y < self.height {
            // SAFETY: (x, y) is inside the framebuffer and pixels are 32-bit.
            unsafe { (self.base.add(y * self.pitch + x * 4) as *const u32).read_volatile() }
        } else {
            0
        }
    }

    /// Draw one glyph (`?` for one the font lacks), at most `max_w` pixels
    /// of it wide; returns its advance in pixels, clipped to `max_w`.
    pub fn glyph_clipped(&mut self, x: usize, y: usize, c: char, st: Style, max_w: usize) -> usize {
        let raster = get_raster(c, st.weight, st.size).or_else(|| get_raster('?', st.weight, st.size));
        let Some(raster) = raster else { return get_raster_width(st.weight, st.size).min(max_w) };
        for (dy, row) in raster.rows().enumerate() {
            for (dx, &a) in row.iter().take(max_w).enumerate() {
                self.put(x + dx, y + dy, blend(st.bg, st.fg, a));
            }
        }
        raster.width().min(max_w)
    }

    fn text(&mut self, x: usize, y: usize, s: &str, st: Style) {
        let mut cx = x;
        for c in s.chars() {
            cx += self.glyph(cx, y, c, st);
        }
    }

    /// Move pixel rows `[from, from+len)` to `[to, to+len)`, up or down (for
    /// scrolling); rows outside the framebuffer are left alone.
    pub fn move_rows(&mut self, from: usize, to: usize, len: usize) {
        let len = len.min(self.height.saturating_sub(from.max(to)));
        let copy = |i: usize| {
            // SAFETY: both rows are inside the framebuffer (`len` is clamped
            // to it); `copy` allows them to overlap.
            unsafe {
                core::ptr::copy(
                    self.base.add((from + i) * self.pitch),
                    self.base.add((to + i) * self.pitch),
                    self.width * 4,
                );
            }
        };
        // Moving down, copy the bottom row first so no row is overwritten
        // before it is read.
        if to > from {
            (0..len).rev().for_each(copy);
        } else {
            (0..len).for_each(copy);
        }
    }
}

/// How to draw text: font weight, size and colors.
#[derive(Clone, Copy)]
pub struct Style {
    weight: FontWeight,
    size: RasterHeight,
    fg: u32,
    bg: u32,
}

impl Style {
    pub const fn new(weight: FontWeight, size: RasterHeight, fg: u32, bg: u32) -> Self {
        Self { weight, size, fg, bg }
    }
}

fn blend(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let ch = |shift: u32| {
        let b = (bg >> shift) & 0xff;
        let f = (fg >> shift) & 0xff;
        ((f * a + b * (255 - a)) / 255) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// A scrolling text area drawn straight to the pixels: the crash screen's
/// writer.
struct Console {
    fb: Framebuffer,
    top: usize,
    x: usize,
    y: usize,
    fg: u32,
    bg: u32,
}

const LINE_H: usize = 16;
pub const CONSOLE_SIZE: RasterHeight = RasterHeight::Size16;

impl Console {
    fn newline(&mut self) {
        self.x = MARGIN;
        self.y += LINE_H;
        if self.y + LINE_H > self.fb.height {
            let region = self.fb.height - self.top;
            let keep = region - LINE_H;
            self.fb.move_rows(self.top + LINE_H, self.top, keep);
            self.y -= LINE_H;
            let (w, bg) = (self.fb.width, self.bg);
            self.fb.fill_rect(0, self.y, w, LINE_H, bg);
        }
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let cw = get_raster_width(FontWeight::Regular, CONSOLE_SIZE);
        for c in s.chars() {
            match c {
                '\n' => self.newline(),
                '\r' => self.x = MARGIN,
                c => {
                    if self.x + cw > self.fb.width.saturating_sub(MARGIN / 2) {
                        self.newline();
                    }
                    let st = Style::new(FontWeight::Regular, CONSOLE_SIZE, self.fg, self.bg);
                    let (x, y) = (self.x, self.y);
                    self.x += self.fb.glyph(x, y, c, st);
                }
            }
        }
        Ok(())
    }
}

/// The framebuffer, once [`init`] found a usable one (for the crash screen).
static FRAMEBUFFER: SpinLock<Option<Framebuffer>> = SpinLock::new(None);

/// Why a framebuffer can't be used.
pub enum Unusable {
    /// Not 32 bits per pixel.
    Bpp(u16),
    /// A colour channel isn't 8 bits wide.
    Masks { red: u8, green: u8, blue: u8 },
    /// Rows shorter than 4 bytes a pixel.
    Pitch { pitch: u64, width: u64 },
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unusable::Bpp(bpp) => write!(f, "{bpp} bits per pixel, not 32"),
            Unusable::Masks { red, green, blue } => {
                write!(f, "colour channels of {red}, {green} and {blue} bits, not 8")
            }
            Unusable::Pitch { pitch, width } => write!(f, "rows of {pitch} bytes for {width} pixels"),
        }
    }
}

/// Draw the hello screen and start the terminal below it.
///
/// Returns the resolution, or why the framebuffer can't be used.
pub fn init(fb: &limine::framebuffer::Framebuffer<'_>) -> Result<(usize, usize), Unusable> {
    let (red, green, blue) = (fb.red_mask_size(), fb.green_mask_size(), fb.blue_mask_size());
    if fb.bpp() != 32 {
        return Err(Unusable::Bpp(fb.bpp()));
    }
    if (red, green, blue) != (8, 8, 8) {
        return Err(Unusable::Masks { red, green, blue });
    }
    if fb.pitch() < fb.width() * 4 {
        return Err(Unusable::Pitch { pitch: fb.pitch(), width: fb.width() });
    }
    let mut f = Framebuffer {
        base: fb.addr(),
        width: fb.width() as usize,
        height: fb.height() as usize,
        pitch: fb.pitch() as usize,
        red_shift: fb.red_mask_shift(),
        green_shift: fb.green_mask_shift(),
        blue_shift: fb.blue_mask_shift(),
    };
    let (w, h) = (f.width, f.height);
    f.fill_rect(0, 0, w, h, BG);
    f.text(MARGIN, 28, "Lumen", Style::new(FontWeight::Bold, RasterHeight::Size32, TITLE, BG));
    f.text(MARGIN, 72, "hello from the kernel", Style::new(FontWeight::Regular, RasterHeight::Size16, TEXT, BG));
    f.text(MARGIN, 94, "milestone 0 - x86-64 - limine", Style::new(FontWeight::Regular, RasterHeight::Size16, DIM, BG));
    f.fill_rect(MARGIN, 124, 576, 2, ACCENT);

    // SAFETY: the crash screen's handle draws only after the crash path
    // stopped the terminal (`with_crash_screen`).
    let crash = unsafe { f.alias() };
    *FRAMEBUFFER.lock() = Some(crash);
    crate::term::init(f);
    Ok((w, h))
}

/// Switch the screen to the crash screen and run `f` with a writer for it.
///
/// Returns false if there is no framebuffer. It also stops the terminal (a
/// backstop: `crash::enter` already did).
///
/// # Safety
/// Crash path only: takes the framebuffer even if its lock is held, so no
/// other code may run concurrently.
pub unsafe fn with_crash_screen(f: impl FnOnce(&mut dyn Write)) -> bool {
    crate::term::stop();
    // SAFETY: upheld by the caller; the terminal draws no more (stopped
    // above), so this is the only handle drawing.
    let Some(fb) = (unsafe { FRAMEBUFFER.force_get() }).as_ref().map(|f| unsafe { f.alias() }) else {
        return false;
    };
    let (w, h) = (fb.width, fb.height);
    let mut con = Console { fb, top: 80, x: MARGIN, y: 80, fg: TEXT, bg: CRASH_BG };
    con.fb.fill_rect(0, 0, w, h, CRASH_BG);
    let banner = Style::new(FontWeight::Bold, RasterHeight::Size32, CRASH_TITLE, CRASH_BG);
    con.fb.text(MARGIN, 24, "LUMEN CRASH REPORT", banner);
    f(&mut con);
    true
}
