//! A window's geometry: the frame, and the parts it is drawn and hit in.
//!
//! ```text
//! frame.x                                   frame.right()
//!   +-----------------------------------------+  frame.y
//!   | title bar (title_h rows, whole width) [x]|  <- close box
//!   +-+-------------------------------------+-+  frame.y + title_h
//!   | |                                     | |
//!   | |  client (the owner's surface)       | |  border columns
//!   | |                                     | |
//!   +-+-------------------------------------+-+  frame.bottom()
//!                 border row
//! ```
//!
//! The title bar spans the frame's whole width (the side borders' top
//! rows belong to it), so a press anywhere in its rows except the close
//! box can start a drag. The client sits `border` in from the left, right
//! and bottom edges and right below the title bar.

use lumen_gfx::Rect;

/// The sizes a frame is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// The title bar's height, top border included.
    pub title_h: u32,
    /// The left, right and bottom borders' width.
    pub border: u32,
    /// The close box's side (it is square).
    pub close: u32,
    /// Room between the close box and the frame's right border.
    pub close_margin: u32,
    /// How much of the title bar, at least, stays on the screen
    /// horizontally when a window is moved (see [`crate::Wm::move_to`]).
    pub strip: u32,
}

impl Metrics {
    /// The desktop's frames (#153 draws them with these).
    pub const DESKTOP: Metrics = Metrics { title_h: 24, border: 1, close: 16, close_margin: 4, strip: 48 };

    /// The frame whose client area is `client_w` by `client_h` with its
    /// top-left corner at (`x`, `y`).
    pub fn frame_for(&self, x: i32, y: i32, client_w: u32, client_h: u32) -> Rect {
        let (w, h) = self.frame_size(client_w, client_h);
        Rect::new(x, y, w, h)
    }

    /// The size of a frame around a `client_w` by `client_h` client area,
    /// before [`Rect::new`] shortens it to keep its edges in `i32`.
    pub fn frame_size(&self, client_w: u32, client_h: u32) -> (u32, u32) {
        let w = client_w.saturating_add(self.border.saturating_mul(2));
        let h = client_h.saturating_add(self.title_h).saturating_add(self.border);
        (w, h)
    }

    /// The title bar: the frame's top `title_h` rows.
    pub fn title_bar(&self, frame: &Rect) -> Rect {
        Rect::new(frame.x(), frame.y(), frame.w(), self.title_h.min(frame.h()))
    }

    /// The close box, inside the title bar at its right end (empty if the
    /// frame is too narrow or short for any of it).
    pub fn close_box(&self, frame: &Rect) -> Rect {
        let title = self.title_bar(frame);
        let inset = i64::from(self.border) + i64::from(self.close_margin) + i64::from(self.close);
        let x = (i64::from(frame.right()) - inset).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let y = frame.y().saturating_add((self.title_h.saturating_sub(self.close) / 2) as i32);
        Rect::new(x, y, self.close, self.close).intersect(&title).unwrap_or(Rect::EMPTY)
    }

    /// The client area: inside the borders, below the title bar.
    pub fn client(&self, frame: &Rect) -> Rect {
        let b = self.border;
        Rect::new(
            frame.x().saturating_add(b as i32),
            frame.y().saturating_add(self.title_h as i32),
            frame.w().saturating_sub(b.saturating_mul(2)),
            frame.h().saturating_sub(self.title_h).saturating_sub(b),
        )
    }

    /// Which part of a window with this `frame` the pixel (`x`, `y`) is
    /// in, or `None` if it is outside the frame.
    pub fn part_at(&self, frame: &Rect, x: i32, y: i32) -> Option<Part> {
        if !frame.contains(x, y) {
            None
        } else if self.close_box(frame).contains(x, y) {
            Some(Part::CloseBox)
        } else if self.title_bar(frame).contains(x, y) {
            Some(Part::TitleBar)
        } else if self.client(frame).contains(x, y) {
            Some(Part::Client)
        } else {
            Some(Part::Border)
        }
    }
}

/// A part of a window's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    CloseBox,
    TitleBar,
    Client,
    /// The left, right or bottom border.
    Border,
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { title_h: 10, border: 2, close: 6, close_margin: 1, strip: 8 };

    #[test]
    fn the_frame_wraps_the_client() {
        let f = M.frame_for(5, 7, 40, 30);
        assert_eq!(f, Rect::new(5, 7, 44, 42));
        assert_eq!(M.client(&f), Rect::new(7, 17, 40, 30));
        assert_eq!(M.title_bar(&f), Rect::new(5, 7, 44, 10));
        // Right border 2, margin 1, box 6: x = 49 - 9 = 40; centred in 10 rows.
        assert_eq!(M.close_box(&f), Rect::new(40, 9, 6, 6));
    }

    #[test]
    fn every_edge_of_every_part_is_hit_exactly() {
        let f = M.frame_for(5, 7, 40, 30); // x 5..49, y 7..49
        let at = |x, y| M.part_at(&f, x, y);
        // Outside each edge of the frame.
        for (x, y) in [(4, 20), (49, 20), (20, 6), (20, 49), (4, 6), (49, 49)] {
            assert_eq!(at(x, y), None, "({x}, {y})");
        }
        // The title bar's corners and its last row; the side borders' top
        // rows are title bar.
        for (x, y) in [(5, 7), (48, 7), (5, 16), (48, 16), (39, 7), (46, 16)] {
            assert_eq!(at(x, y), Some(Part::TitleBar), "({x}, {y})");
        }
        // The close box's corners, and one past each of its edges.
        for (x, y) in [(40, 9), (45, 9), (40, 14), (45, 14)] {
            assert_eq!(at(x, y), Some(Part::CloseBox), "({x}, {y})");
        }
        for (x, y) in [(39, 9), (46, 9), (40, 8), (40, 15)] {
            assert_eq!(at(x, y), Some(Part::TitleBar), "({x}, {y})");
        }
        // The client's corners.
        for (x, y) in [(7, 17), (46, 17), (7, 46), (46, 46)] {
            assert_eq!(at(x, y), Some(Part::Client), "({x}, {y})");
        }
        // The borders: left, right, bottom, bottom corners.
        for (x, y) in [(5, 17), (6, 46), (47, 17), (48, 46), (20, 47), (20, 48), (5, 48), (48, 48)] {
            assert_eq!(at(x, y), Some(Part::Border), "({x}, {y})");
        }
    }

    #[test]
    fn a_frame_too_small_for_its_parts_keeps_them_inside_it() {
        // Narrower than the close box and its margins.
        let f = M.frame_for(0, 0, 0, 0); // 4 x 12
        assert_eq!(f, Rect::new(0, 0, 4, 12));
        assert!(f.contains_rect(&M.close_box(&f)));
        assert!(M.client(&f).is_empty());
        // Shorter than the title bar: the title bar is the whole frame.
        let short = Rect::new(0, 0, 50, 3);
        assert_eq!(M.title_bar(&short), short);
        assert!(short.contains_rect(&M.close_box(&short)));
    }

    #[test]
    fn frames_at_the_coordinate_limits_dont_overflow() {
        let f = M.frame_for(i32::MAX - 3, i32::MIN, u32::MAX, u32::MAX);
        assert!(f.contains_rect(&M.close_box(&f)));
        let _ = M.client(&f);
        let g = M.frame_for(i32::MIN, i32::MIN, 1, 1);
        assert!(g.contains_rect(&M.close_box(&g)));
        assert_eq!(M.part_at(&g, i32::MIN, i32::MIN), Some(Part::TitleBar));
    }
}
