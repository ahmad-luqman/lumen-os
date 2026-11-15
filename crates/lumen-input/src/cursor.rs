//! The pointer drawn over pixels it doesn't own: an arrow sprite with a
//! save-under.
//!
//! [`Cursor::show`] keeps the pixels of the arrow's box (clipped to the
//! surface) before painting the arrow, and [`Cursor::hide`] puts them back.
//! That is right only if nothing else drew in the box meanwhile, so whoever
//! draws there hides the arrow first ([`Cursor::overlaps`] says whether a
//! rectangle touches it) and shows it again afterwards: what the arrow
//! covered is then never lost, and the arrow never leaves a trail.

/// The arrow's size in pixels; its tip (the pixel pointed at) is its
/// top-left corner.
pub const SPRITE_W: usize = 11;
pub const SPRITE_H: usize = 17;

/// The arrow: `X` its outline, `o` its fill, `.` clear.
const SPRITE: [&[u8; SPRITE_W]; SPRITE_H] = [
    b"X..........",
    b"XX.........",
    b"XoX........",
    b"XooX.......",
    b"XoooX......",
    b"XooooX.....",
    b"XoooooX....",
    b"XooooooX...",
    b"XoooooooX..",
    b"XooooooooX.",
    b"XoooooXXXXX",
    b"XooXooX....",
    b"XoX.XooX...",
    b"XX..XooX...",
    b"X....XooX..",
    b".....XooX..",
    b"......XX...",
];

/// What the arrow paints at (`dx`, `dy`) from its tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Clear,
    Outline,
    Fill,
}

/// The arrow's pixel at (`dx`, `dy`) (clear outside its box).
fn paint(dx: usize, dy: usize) -> Paint {
    match SPRITE.get(dy).and_then(|row| row.get(dx)) {
        Some(b'X') => Paint::Outline,
        Some(b'o') => Paint::Fill,
        _ => Paint::Clear,
    }
}

/// Pixels the cursor draws on and reads back: the values are the
/// surface's own (the kernel's are framebuffer-encoded colours).
pub trait Surface {
    /// Width and height in pixels.
    fn size(&self) -> (usize, usize);
    /// The pixel at (`x`, `y`), inside the surface.
    fn get(&self, x: usize, y: usize) -> u32;
    /// Set the pixel at (`x`, `y`), inside the surface.
    fn set(&mut self, x: usize, y: usize, value: u32);
}

/// The arrow's box at (`x`, `y`) clipped to a `w` x `h` surface: its
/// width and height (0 for a tip outside the surface).
fn clipped(x: usize, y: usize, w: usize, h: usize) -> (usize, usize) {
    (w.saturating_sub(x).min(SPRITE_W), h.saturating_sub(y).min(SPRITE_H))
}

/// The arrow and what is below it.
#[derive(Clone, Debug)]
pub struct Cursor {
    /// Where the tip is drawn and the clipped box's size, if it is drawn.
    drawn: Option<(usize, usize, usize, usize)>,
    /// The box's pixels before the arrow was painted, row by row
    /// (`SPRITE_W` apart).
    saved: [u32; SPRITE_W * SPRITE_H],
}

/// Not drawn anywhere.
impl Default for Cursor {
    fn default() -> Cursor {
        Cursor { drawn: None, saved: [0; SPRITE_W * SPRITE_H] }
    }
}

impl Cursor {
    /// Where its tip is drawn, if any of it is (a tip off the surface
    /// draws nothing, so it isn't drawn).
    pub fn drawn_at(&self) -> Option<(usize, usize)> {
        self.drawn.map(|(x, y, _, _)| (x, y))
    }

    /// Whether it is drawn and its box shares a pixel with the `w` x `h`
    /// rectangle at (`x`, `y`).
    pub fn overlaps(&self, x: usize, y: usize, w: usize, h: usize) -> bool {
        let Some((cx, cy, cw, ch)) = self.drawn else { return false };
        w > 0 && h > 0 && x < cx + cw && cx < x.saturating_add(w) && y < cy + ch && cy < y.saturating_add(h)
    }

    /// Draw the arrow with its tip at (`x`, `y`) in `outline` and `fill`,
    /// keeping what it covers (clipped to the surface; a tip outside it
    /// draws nothing). Taken off its old place first; already drawn there,
    /// nothing changes (the caller hid it if something drew below it).
    pub fn show(&mut self, s: &mut impl Surface, x: usize, y: usize, outline: u32, fill: u32) {
        if self.drawn_at() == Some((x, y)) {
            return;
        }
        self.hide(s);
        let (sw, sh) = s.size();
        let (w, h) = clipped(x, y, sw, sh);
        for dy in 0..h {
            for dx in 0..w {
                self.saved[dy * SPRITE_W + dx] = s.get(x + dx, y + dy);
                match paint(dx, dy) {
                    Paint::Outline => s.set(x + dx, y + dy, outline),
                    Paint::Fill => s.set(x + dx, y + dy, fill),
                    Paint::Clear => {}
                }
            }
        }
        self.drawn = (w > 0 && h > 0).then_some((x, y, w, h));
    }

    /// Put back what the arrow covered, if it is drawn (only the pixels it
    /// painted: the clear ones were never changed).
    pub fn hide(&mut self, s: &mut impl Surface) {
        let Some((x, y, w, h)) = self.drawn.take() else { return };
        for dy in 0..h {
            for dx in 0..w {
                if paint(dx, dy) != Paint::Clear {
                    s.set(x + dx, y + dy, self.saved[dy * SPRITE_W + dx]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A surface of numbered pixels.
    #[derive(Clone, PartialEq, Debug)]
    struct Grid {
        w: usize,
        h: usize,
        px: Vec<u32>,
    }

    impl Grid {
        fn new(w: usize, h: usize, seed: u32) -> Grid {
            Grid { w, h, px: (0..(w * h) as u32).map(|i| i.wrapping_mul(2_654_435_761) ^ seed).collect() }
        }
    }

    impl Surface for Grid {
        fn size(&self) -> (usize, usize) {
            (self.w, self.h)
        }
        fn get(&self, x: usize, y: usize) -> u32 {
            assert!(x < self.w && y < self.h, "read outside ({x}, {y})");
            self.px[y * self.w + x]
        }
        fn set(&mut self, x: usize, y: usize, v: u32) {
            assert!(x < self.w && y < self.h, "write outside ({x}, {y})");
            self.px[y * self.w + x] = v;
        }
    }

    const OUT: u32 = 0xaaaa_aaaa;
    const FILL: u32 = 0x5555_5555;

    /// What the screen should show: `model` with the arrow at `at`.
    fn expected(model: &Grid, at: Option<(usize, usize)>) -> Grid {
        let mut g = model.clone();
        if let Some((x, y)) = at {
            for dy in 0..SPRITE_H {
                for dx in 0..SPRITE_W {
                    let (px, py) = (x + dx, y + dy);
                    if px < g.w && py < g.h {
                        match paint(dx, dy) {
                            Paint::Outline => g.px[py * g.w + px] = OUT,
                            Paint::Fill => g.px[py * g.w + px] = FILL,
                            Paint::Clear => {}
                        }
                    }
                }
            }
        }
        g
    }

    #[test]
    fn the_sprite_is_an_outlined_arrow() {
        assert_eq!(paint(0, 0), Paint::Outline);
        assert_eq!(paint(1, 2), Paint::Fill);
        assert_eq!(paint(10, 0), Paint::Clear);
        assert_eq!(paint(SPRITE_W, 0), Paint::Clear);
        assert_eq!(paint(0, SPRITE_H), Paint::Clear);
        // Every fill pixel has outline or fill on all four sides: the
        // outline closes the arrow (it shows on any background).
        for dy in 0..SPRITE_H {
            for dx in 0..SPRITE_W {
                if paint(dx, dy) == Paint::Fill {
                    let near = [(dx - 1, dy), (dx + 1, dy), (dx, dy - 1), (dx, dy + 1)];
                    assert!(near.iter().all(|&(x, y)| paint(x, y) != Paint::Clear), "({dx}, {dy}) leaks");
                }
            }
        }
    }

    #[test]
    fn hiding_puts_back_what_was_there() {
        let model = Grid::new(40, 30, 7);
        let mut screen = model.clone();
        let mut c = Cursor::default();
        assert_eq!(c.drawn_at(), None);
        c.hide(&mut screen);
        assert_eq!(screen, model, "hiding an undrawn arrow changes nothing");
        c.show(&mut screen, 5, 6, OUT, FILL);
        assert_eq!(c.drawn_at(), Some((5, 6)));
        assert_eq!(screen, expected(&model, Some((5, 6))));
        // Shown again at the same place: nothing changes.
        c.show(&mut screen, 5, 6, OUT, FILL);
        assert_eq!(screen, expected(&model, Some((5, 6))));
        // Moved: the old place is restored.
        c.show(&mut screen, 7, 9, OUT, FILL);
        assert_eq!(screen, expected(&model, Some((7, 9))));
        c.hide(&mut screen);
        assert_eq!(screen, model);
        assert_eq!(c.drawn_at(), None);
    }

    #[test]
    fn the_arrow_is_clipped_at_the_edges() {
        let model = Grid::new(20, 20, 3);
        for (x, y) in [(19, 19), (15, 0), (0, 10), (10, 19), (20, 5), (5, 20), (100, 100)] {
            let mut screen = model.clone();
            let mut c = Cursor::default();
            c.show(&mut screen, x, y, OUT, FILL);
            assert_eq!(screen, expected(&model, Some((x, y))), "at ({x}, {y})");
            c.hide(&mut screen);
            assert_eq!(screen, model, "at ({x}, {y})");
        }
    }

    #[test]
    fn overlap_is_the_clipped_box() {
        let mut screen = Grid::new(30, 30, 1);
        let mut c = Cursor::default();
        assert!(!c.overlaps(0, 0, 30, 30), "not drawn");
        c.show(&mut screen, 10, 10, OUT, FILL);
        assert!(c.overlaps(10, 10, 1, 1));
        assert!(c.overlaps(10 + SPRITE_W - 1, 10 + SPRITE_H - 1, 1, 1));
        assert!(!c.overlaps(10 + SPRITE_W, 10, 5, 5), "right of it");
        assert!(!c.overlaps(10, 10 + SPRITE_H, 5, 5), "below it");
        assert!(!c.overlaps(0, 0, 10, 30), "left of it");
        assert!(!c.overlaps(0, 0, 30, 10), "above it");
        assert!(c.overlaps(0, 0, 11, 11));
        assert!(!c.overlaps(12, 12, 0, 5), "an empty rectangle");
        assert!(c.overlaps(0, 0, usize::MAX, usize::MAX), "no overflow");
        // Clipped: at (25, 25) only 5 x 5 pixels are on a 30 x 30 surface.
        c.show(&mut screen, 25, 25, OUT, FILL);
        assert!(c.overlaps(29, 29, 1, 1));
        assert!(!c.overlaps(30, 25, 10, 10));
        // Off the surface: drawn nowhere, overlapping nothing (the old
        // place restored).
        let shown = screen.clone();
        c.show(&mut screen, 30, 30, OUT, FILL);
        assert_eq!(c.drawn_at(), None);
        assert!(!c.overlaps(0, 0, usize::MAX, usize::MAX));
        assert_ne!(screen, shown, "the arrow at (25, 25) was taken off");
    }

    #[test]
    fn drawing_around_the_arrow_never_leaves_a_trail() {
        // A model of the kernel's use: the screen is `model` plus the arrow;
        // other drawing (random rectangles) hides the arrow first if it
        // overlaps, draws, and shows it again; the arrow moves at random.
        // After every step the screen is exactly the model with the arrow.
        let mut rng = 0x1234_5678_u64;
        let mut next = |n: usize| {
            rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((rng >> 33) as usize) % n
        };
        let (w, h) = (48, 40);
        let mut model = Grid::new(w, h, 99);
        let mut screen = model.clone();
        let mut c = Cursor::default();
        let mut at = None;
        for step in 0..20_000 {
            match next(4) {
                0 | 1 => {
                    let (x, y) = (next(w + 4), next(h + 4));
                    c.show(&mut screen, x, y, OUT, FILL);
                    at = Some((x, y));
                }
                2 => {
                    let (x, y, rw, rh) = (next(w), next(h), 1 + next(12), 1 + next(12));
                    let colour = next(1 << 20) as u32;
                    let hidden = c.overlaps(x, y, rw, rh);
                    if hidden {
                        c.hide(&mut screen);
                    }
                    for py in y..(y + rh).min(h) {
                        for px in x..(x + rw).min(w) {
                            screen.set(px, py, colour);
                            model.set(px, py, colour);
                        }
                    }
                    if hidden && let Some((x, y)) = at {
                        c.show(&mut screen, x, y, OUT, FILL);
                    }
                }
                _ => {
                    c.hide(&mut screen);
                    at = None;
                }
            }
            assert_eq!(screen, expected(&model, at), "step {step}");
        }
    }
}
