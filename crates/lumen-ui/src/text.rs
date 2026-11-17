//! How the toolkit measures and draws text: behind a trait, so the kernel
//! draws with `lumen-font` (#148) and host tests with [`FixedText`].

use lumen_gfx::{Rect, Surface};

/// One line of text at one size.
pub trait Text {
    /// The height of a line box.
    fn line_height(&self) -> u32;
    /// `text`'s advance width (where text after it would start).
    fn width(&mut self, text: &str) -> u32;
    /// Draw `text` with its line box's top-left corner at (`x`, `y`) in
    /// `color`, touching only pixels inside `clip`.
    fn draw(&mut self, surface: &mut Surface, x: i32, y: i32, text: &str, color: u32, clip: Rect);
}

/// A fixed-width fake for host tests: every character 8 pixels wide, lines
/// 16 high; each character but a space drawn as a 6x10 block (so a test
/// sees where text went).
#[derive(Clone, Copy, Debug, Default)]
pub struct FixedText;

impl FixedText {
    pub const ADVANCE: u32 = 8;
    pub const LINE: u32 = 16;
}

impl Text for FixedText {
    fn line_height(&self) -> u32 {
        FixedText::LINE
    }

    fn width(&mut self, text: &str) -> u32 {
        text.chars().count() as u32 * FixedText::ADVANCE
    }

    fn draw(&mut self, surface: &mut Surface, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        for (i, c) in text.chars().enumerate() {
            if c == ' ' {
                continue;
            }
            let block = Rect::new(x + i as i32 * FixedText::ADVANCE as i32 + 1, y + 3, 6, 10);
            if let Some(b) = block.intersect(&clip) {
                surface.fill_rect(b, color);
            }
        }
    }
}
