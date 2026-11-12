//! Boot framebuffer: the "hello" screen, a scrolling text console for log
//! lines, and the crash screen.
//!
//! Text uses pre-rasterized Noto Sans Mono glyphs (anti-aliased, so we blend
//! each pixel between background and foreground). This is deliberately the
//! simplest thing that works; the real compositor and font renderer are M5.
//!
//! The header area (top-left 640x136 pixels) is deterministic and checked
//! against `tests/golden/boot-hello.png` by the screendump test.

use crate::sync::SpinLock;
use core::fmt::{self, Write};
use log::Level;
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

const BG: u32 = 0x0f1420;
const TITLE: u32 = 0xffd479;
const TEXT: u32 = 0xe6e6e6;
const DIM: u32 = 0x8a93a6;
const ACCENT: u32 = 0x3d7eff;
const CRASH_BG: u32 = 0x3a0b0b;
const CRASH_TITLE: u32 = 0xff6b6b;

/// Top of the log console, below the fixed header.
const CONSOLE_TOP: usize = 144;
const MARGIN: usize = 32;

#[derive(Clone, Copy)]
struct Framebuffer {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

// SAFETY: the framebuffer is plain MMIO-like memory owned by the kernel; all
// access goes through the `CONSOLE` lock (or the crash path, after it has
// stopped the other CPUs or named those that didn't stop).
unsafe impl Send for Framebuffer {}

impl Framebuffer {
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

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: u32) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                self.put(xx, yy, rgb);
            }
        }
    }

    /// Draw one glyph; returns its advance width.
    fn glyph(&mut self, x: usize, y: usize, c: char, st: Style) -> usize {
        let raster = get_raster(c, st.weight, st.size).or_else(|| get_raster('?', st.weight, st.size));
        let Some(raster) = raster else { return get_raster_width(st.weight, st.size) };
        for (dy, row) in raster.rows().enumerate() {
            for (dx, &a) in row.iter().enumerate() {
                self.put(x + dx, y + dy, blend(st.bg, st.fg, a));
            }
        }
        raster.width()
    }

    fn text(&mut self, x: usize, y: usize, s: &str, st: Style) {
        let mut cx = x;
        for c in s.chars() {
            cx += self.glyph(cx, y, c, st);
        }
    }

    /// Move rows `[from, from+len)` up to `to` (for scrolling).
    fn move_rows(&mut self, from: usize, to: usize, len: usize) {
        for i in 0..len {
            // SAFETY: both rows are inside the framebuffer (callers clamp to height);
            // `copy` handles the overlap.
            unsafe {
                core::ptr::copy(
                    self.base.add((from + i) * self.pitch),
                    self.base.add((to + i) * self.pitch),
                    self.width * 4,
                );
            }
        }
    }
}

/// How to draw text: font weight, size and colors.
#[derive(Clone, Copy)]
struct Style {
    weight: FontWeight,
    size: RasterHeight,
    fg: u32,
    bg: u32,
}

impl Style {
    const fn new(weight: FontWeight, size: RasterHeight, fg: u32, bg: u32) -> Self {
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

/// A scrolling text area on the framebuffer.
struct Console {
    fb: Framebuffer,
    top: usize,
    x: usize,
    y: usize,
    fg: u32,
    bg: u32,
}

const LINE_H: usize = 16;
const CONSOLE_SIZE: RasterHeight = RasterHeight::Size16;

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

static CONSOLE: SpinLock<Option<Console>> = SpinLock::new(None);

/// Draw the hello screen and enable the log console.
///
/// Returns the resolution, or `None` if there is no usable 32-bpp framebuffer.
pub fn init(fb: &limine::framebuffer::Framebuffer<'_>) -> Option<(usize, usize)> {
    if fb.bpp() != 32 || fb.red_mask_size() != 8 || fb.green_mask_size() != 8 || fb.blue_mask_size() != 8 {
        return None;
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

    *CONSOLE.lock() = Some(Console { fb: f, top: CONSOLE_TOP, x: MARGIN, y: CONSOLE_TOP, fg: TEXT, bg: BG });
    Some((w, h))
}

/// Mirror a log line to the screen, without the serial timestamp so
/// screenshots stay comparable (called by the logger).
pub fn console_log(level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    let Some(mut guard) = CONSOLE.try_lock() else { return };
    let Some(con) = guard.as_mut() else { return };
    con.fg = match level {
        Level::Error => CRASH_TITLE,
        Level::Warn => TITLE,
        _ => TEXT,
    };
    let _ = writeln!(con, "{level:<5} {tag}: {args}");
    con.fg = TEXT;
}

/// Switch the screen to the crash screen and run `f` with a writer for it.
///
/// Returns false (and does nothing) if there is no framebuffer.
///
/// # Safety
/// Crash path only: takes the console even if its lock is held, so no other
/// code may run concurrently.
pub unsafe fn with_crash_screen(f: impl FnOnce(&mut dyn Write)) -> bool {
    // SAFETY: upheld by the caller.
    let Some(con) = (unsafe { CONSOLE.force_get() }).as_mut() else { return false };
    let (w, h) = (con.fb.width, con.fb.height);
    con.fb.fill_rect(0, 0, w, h, CRASH_BG);
    let banner = Style::new(FontWeight::Bold, RasterHeight::Size32, CRASH_TITLE, CRASH_BG);
    con.fb.text(MARGIN, 24, "LUMEN CRASH REPORT", banner);
    con.top = 80;
    con.x = MARGIN;
    con.y = 80;
    con.fg = TEXT;
    con.bg = CRASH_BG;
    f(con);
    true
}
