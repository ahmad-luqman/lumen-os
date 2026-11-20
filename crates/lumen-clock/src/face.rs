//! The clock's window: its face (the app's own drawing: sixty dots, three
//! hands and a hub, in the theme's colours) above its tree (one
//! fixed-width label, `Time`, whose value is the time), as a
//! `lumen_ui::App` the app runs in the SDK's toolkit. When the time
//! changes, only the boxes of the hands that moved (where they were and
//! where they are) are its damage ([`App::take_drawn`]); host tests check
//! that such a partial repaint leaves the same pixels as a full one.

use alloc::format;
use alloc::vec::Vec;
use lumen_gfx::{Rect, Surface};
use lumen_ui::{App, Over, Palette, Then, Ui, UiEvent, Widget};

use crate::{Hands, TimeOfDay, line, point};

/// The window's client size: the face (a square) above the tree.
pub const WIDTH: u32 = 240;
pub const FACE_H: u32 = 240;
pub const TREE_H: u32 = 36;
pub const HEIGHT: u32 = FACE_H + TREE_H;
/// The face: the app's own part of the window.
pub const FACE: Rect = Rect::new(0, 0, WIDTH, FACE_H);
/// Where the tree is laid out (`Toolkit::with_area`).
pub const TREE: Rect = Rect::new(0, FACE_H as i32, WIDTH, TREE_H);
/// What the label shows while the clock has no time to show (its RTC
/// failing): as wide as a time.
pub const BLANK: &str = "--:--:--";

/// The face's centre, and the radius its dots are at.
const CX: i32 = (WIDTH / 2) as i32;
const CY: i32 = (FACE_H / 2) as i32;
const DOTS_R: i32 = 104;
/// A minute's dot and an hour's, each a square this wide.
const MINUTE_DOT: u32 = 2;
const HOUR_DOT: u32 = 6;
/// The hub over the hands' common end (drawn in every rect repainted, so
/// no hand's box needs to hold it).
const HUB: u32 = 8;
/// The `Time` label's fixed width (`HH:MM:SS` at the toolkit's text size,
/// with room to spare): a label that changed width would move the widgets
/// after it and repaint the whole window.
const TIME_W: u32 = 96;

/// A hand: which of [`Hands`] it shows, its length, its width (a square
/// brush) and its colour.
pub(crate) struct Hand {
    angle: fn(&Hands) -> u32,
    len: i32,
    brush: u32,
    colour: fn(&Palette) -> u32,
}

/// In drawing order: the second hand over the minute hand over the hour's.
pub(crate) const HANDS: [Hand; 3] = [
    Hand { angle: |h| h.hour, len: 56, brush: 6, colour: |p| p.text },
    Hand { angle: |h| h.minute, len: 84, brush: 4, colour: |p| p.text },
    Hand { angle: |h| h.second, len: 94, brush: 2, colour: |p| p.accent },
];

/// A `w`-wide square centred on `at`.
fn square((x, y): (i32, i32), w: u32) -> Rect {
    let half = (w / 2) as i32;
    Rect::new(x - half, y - half, w, w)
}

/// Fill the part of `square(at, w)` inside `clip`.
fn fill_square(surface: &mut Surface, at: (i32, i32), w: u32, colour: u32, clip: Rect) {
    if let Some(part) = square(at, w).intersect(&clip) {
        surface.fill_rect(part, colour);
    }
}

impl Hand {
    /// Its far end at `hands`.
    fn end(&self, hands: &Hands) -> (i32, i32) {
        point(CX, CY, self.len, (self.angle)(hands))
    }

    /// The box it covers at `hands`: its brush at either end and everything
    /// between (every square it paints lies in it, a host test checks).
    pub(crate) fn bounds(&self, hands: &Hands) -> Rect {
        square((CX, CY), self.brush).union(&square(self.end(hands), self.brush))
    }

    /// The squares it paints at `hands`, one per pixel of its line.
    fn squares(&self, hands: &Hands) -> impl Iterator<Item = Rect> + '_ {
        line((CX, CY), self.end(hands)).map(|at| square(at, self.brush))
    }

    /// Draw it at `hands`, only inside `clip`.
    fn draw(&self, surface: &mut Surface, palette: &Palette, hands: &Hands, clip: Rect) {
        if self.bounds(hands).intersect(&clip).is_none() {
            return;
        }
        let colour = (self.colour)(palette);
        for s in self.squares(hands) {
            if let Some(part) = s.intersect(&clip) {
                surface.fill_rect(part, colour);
            }
        }
    }
}

/// The face's foreground (the background is the tree's paint, already
/// there), only inside `clip`: the dots, the hands at `hands` (none while
/// there is no time to show) and the hub.
pub fn draw_face(surface: &mut Surface, palette: &Palette, hands: Option<&Hands>, clip: Rect) {
    for i in 0..60u32 {
        let at = point(CX, CY, DOTS_R, i * 12);
        let (w, colour) = if i % 5 == 0 { (HOUR_DOT, palette.text) } else { (MINUTE_DOT, palette.border) };
        fill_square(surface, at, w, colour, clip);
    }
    if let Some(hands) = hands {
        for hand in &HANDS {
            hand.draw(surface, palette, hands, clip);
        }
    }
    fill_square(surface, (CX, CY), HUB, palette.accent, clip);
}

/// The clock's window as an app: its tree (the `Time` label), the time it
/// shows (`None`: none to show, [`BLANK`] and no hands), and the boxes to
/// repaint.
pub struct Face {
    ui: Ui,
    label: Widget,
    shown: Option<TimeOfDay>,
    drawn: Vec<Rect>,
}

impl Face {
    pub fn new(shown: Option<TimeOfDay>) -> Face {
        let mut ui = Ui::new();
        let label = ui.fixed_label(None, "Time", &text(shown), TIME_W);
        Face { ui, label, shown, drawn: Vec::new() }
    }

    pub fn shown(&self) -> Option<TimeOfDay> {
        self.shown
    }

    /// Show `t` (or no time): the label, and the boxes of the hands that
    /// moved, appeared or went. The same time again changes nothing.
    pub fn show(&mut self, t: Option<TimeOfDay>) {
        if t == self.shown {
            return;
        }
        let (was, now) = (self.shown.map(|t| t.hands()), t.map(|t| t.hands()));
        for hand in &HANDS {
            if was.map(|h| (hand.angle)(&h)) != now.map(|h| (hand.angle)(&h)) {
                self.drawn.extend(was.iter().chain(&now).map(|h| hand.bounds(h)));
            }
        }
        self.shown = t;
        self.ui.set_text(self.label, &text(t));
    }
}

/// What the label shows for `t`.
fn text(t: Option<TimeOfDay>) -> alloc::string::String {
    t.map_or_else(|| alloc::string::String::from(BLANK), |t| format!("{t}"))
}

impl App for Face {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, _: &[UiEvent]) -> Then {
        Then::Carry
    }

    /// The face's foreground, only where the rects being repainted reach
    /// (each has the tree's background already).
    fn paint_over(&mut self, over: &mut Over<'_>) {
        let hands = self.shown.map(|t| t.hands());
        for &clip in over.rects {
            draw_face(over.surface, over.palette, hands.as_ref(), clip);
        }
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        core::mem::take(&mut self.drawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::STEPS;
    use lumen_ui::{FixedText, repaint};

    /// Seven distinct colours, so a pixel painted in the wrong one shows.
    const PALETTE: Palette = Palette {
        background: 0x111111,
        text: 0x222222,
        accent: 0x333333,
        accent_text: 0x444444,
        border: 0x555555,
        disabled: 0x666666,
        field: 0x777777,
    };

    fn canvas() -> Surface {
        Surface::new(WIDTH, HEIGHT)
    }

    /// `face` repainted (what the toolkit's refresh does): the rects
    /// committed.
    fn refresh(face: &mut Face, s: &mut Surface) -> Vec<Rect> {
        repaint(face, s, &mut FixedText, &PALETTE, TREE).rects
    }

    /// A fresh window showing `t`, painted whole, and its pixels.
    fn painted(t: Option<TimeOfDay>) -> (Face, Surface) {
        let mut face = Face::new(t);
        let mut s = canvas();
        face.ui_mut().invalidate();
        assert_eq!(refresh(&mut face, &mut s), alloc::vec![s.bounds()]);
        (face, s)
    }

    /// What a full repaint of `t` leaves.
    fn full(t: Option<TimeOfDay>) -> Surface {
        painted(t).1
    }

    fn at(secs: u64) -> Option<TimeOfDay> {
        Some(TimeOfDay::from_unix(secs))
    }

    #[test]
    fn every_square_a_hand_paints_is_in_its_box_on_the_face() {
        for hand in &HANDS {
            for step in 0..STEPS {
                let hands = Hands { hour: step, minute: step, second: step };
                let b = hand.bounds(&hands);
                assert!(FACE.contains_rect(&b), "box {b:?} at step {step}");
                for s in hand.squares(&hands) {
                    assert!(b.contains_rect(&s), "{s:?} outside {b:?} at step {step}");
                }
            }
        }
        for i in 0..60u32 {
            assert!(FACE.contains_rect(&square(point(CX, CY, DOTS_R, i * 12), HOUR_DOT)));
        }
    }

    #[test]
    fn a_partial_repaint_every_second_leaves_what_a_full_one_does() {
        // Twelve hours a second at a time (every hand at every place it
        // takes), compared with a full repaint every 37 s and through the
        // first two minutes and each hour's turn.
        let (mut face, mut s) = painted(at(0));
        for secs in 1..=12 * 3600 + 2 {
            face.show(at(secs));
            refresh(&mut face, &mut s);
            let check = secs < 120 || secs % 37 == 0 || secs % 3600 < 2 || secs % 3600 == 3599;
            if check {
                assert!(s.pixels() == full(at(secs)).pixels(), "at {}", TimeOfDay::from_unix(secs));
            }
        }
    }

    #[test]
    fn a_partial_repaint_after_any_jump_leaves_what_a_full_one_does() {
        // Jumps (a clock set, a host stall) between any two times, and to
        // and from no time at all.
        let (mut face, mut s) = painted(at(0));
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for i in 0..1500 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = if i % 50 == 7 { None } else { at(x % 86_400) };
            face.show(t);
            refresh(&mut face, &mut s);
            assert!(s.pixels() == full(t).pixels(), "jump {i} to {t:?}");
        }
    }

    #[test]
    fn it_draws_the_hands_in_their_colours_and_labels_the_time() {
        // 10:09:00: the second hand straight up from the centre, in the
        // accent (the hour and minute hands far from it); with no time,
        // no hands: the background there.
        let t = at(36_540);
        assert_eq!(full(t).pixel(CX, CY - 60), Some(PALETTE.accent));
        assert_eq!(full(None).pixel(CX, CY - 60), Some(PALETTE.background));
        // The hour hand (towards 10) in the text colour, a minute hand's
        // length out from the centre: the minute hand points elsewhere.
        let (hx, hy) = point(CX, CY, 40, t.unwrap().hands().hour);
        assert_eq!(full(t).pixel(hx, hy), Some(PALETTE.text));
        let face = Face::new(t);
        assert_eq!(face.ui.text(face.label), Some("10:09:00"));
    }

    #[test]
    fn showing_follows_the_time_and_the_same_time_repaints_nothing() {
        let (mut face, mut s) = painted(at(0));
        let t = at(10 * 3600 + 9 * 60);
        face.show(t);
        assert_eq!(face.shown(), t);
        assert_eq!(face.ui.text(face.label), Some("10:09:00"));
        assert!(!refresh(&mut face, &mut s).is_empty());
        assert!(s.pixels() == full(t).pixels());
        // A tick that finds the same second: no damage, nothing committed.
        face.show(t);
        assert!(face.take_drawn().is_empty());
        assert!(refresh(&mut face, &mut s).is_empty());
        // No time: the label blank and the hands gone (all three boxes).
        face.show(None);
        assert_eq!(face.ui.text(face.label), Some(BLANK));
        assert_eq!(face.drawn.len(), 3);
    }

    #[test]
    fn one_second_repaints_only_the_second_hand_and_the_label() {
        // 10:09:01 → 10:09:02: only the second hand's two boxes (and the
        // label, the tree's) are committed, never the whole face.
        let (mut face, mut s) = painted(at(36_541));
        face.show(at(36_542));
        let drawn = face.drawn.clone();
        assert_eq!(drawn.len(), 2);
        let rects = refresh(&mut face, &mut s);
        let px: u64 = rects.iter().map(|r| r.area()).sum();
        assert!(px < u64::from(WIDTH * FACE_H) / 4, "{px} pixels for one second");
    }
}
