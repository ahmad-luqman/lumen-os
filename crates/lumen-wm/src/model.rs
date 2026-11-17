//! Tests of [`Wm`] as a whole: focus rules, the drag state machine, the
//! grab, the close box, clamping, pausing, the randomized model, and
//! press, pause (or gap), resume, release through each window's
//! [`EventQueue`] by [`deliver`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_gfx::Rect;
use lumen_input::{Button, Buttons};

use crate::geom::{Metrics, Part};
use crate::queue::{Entry, EventQueue, KeyLike, Next};
use crate::route::{PointerEntry, deliver};
use crate::wm::{Delivery, Hit, Interruption, Outcome, Pointer, WindowId, Wm};

const M: Metrics = Metrics { title_h: 4, border: 1, close: 2, close_margin: 0, strip: 3 };
const W: u32 = 40;
const H: u32 = 30;
const L: Buttons = Buttons::LEFT;
const R: Buttons = Buttons::RIGHT;
const NONE: Buttons = Buttons::NONE;
const LB: Button = Button::Left;
const RB: Button = Button::Right;

fn wm() -> Wm {
    Wm::new(W, H, M)
}

fn at(x: i32, y: i32, b: Buttons) -> Pointer {
    Pointer::at(x, y, b)
}

/// Opens a window whose client is `w` by `h` with its frame at (`x`, `y`).
fn open(wm: &mut Wm, x: i32, y: i32, w: u32, h: u32) -> WindowId {
    wm.open("w", w, h, x, y).0
}

fn frame(wm: &Wm, id: WindowId) -> Rect {
    wm.window(id).expect("open").frame()
}

/// Every pixel in `r` is covered by the outcome's damage.
fn covers(out: &Outcome, r: Rect) -> bool {
    (r.y()..r.bottom()).all(|y| (r.x()..r.right()).all(|x| out.damage.rects().iter().any(|d| d.contains(x, y))))
}

/// Where a moved frame of width `w` lands: [`Wm::move_to`]'s clamp, stated
/// again.
fn clamped(w: u32, x: i32, y: i32) -> (i32, i32) {
    let strip = M.strip.min(w) as i32;
    (x.clamp(strip - w as i32, W as i32 - strip), y.clamp(0, (H - M.title_h) as i32))
}

#[test]
fn opening_focuses_and_raises_and_ids_count_from_1_never_reused() {
    let mut wm = wm();
    assert_eq!(wm.focused(), None);
    let (a, out) = wm.open("a", 10, 8, 2, 2);
    assert_eq!(a.get(), 1);
    assert_eq!(out.deliveries, [Delivery::FocusGained(a)]);
    let (b, out) = wm.open("b", 10, 8, 6, 6);
    assert_eq!(b.get(), 2);
    assert_eq!(out.deliveries, [Delivery::FocusLost(a), Delivery::FocusGained(b)]);
    assert!(covers(&out, frame(&wm, a)) && covers(&out, frame(&wm, b)), "both frames redrawn (focus is drawn)");
    let _ = wm.close(b).expect("open");
    let (c, _) = wm.open("c", 1, 1, 0, 0);
    assert_eq!(c.get(), 3);
    assert_eq!(wm.windows().iter().map(|w| w.id()).collect::<Vec<_>>(), [a, c]);
    assert_eq!(wm.window(c).expect("open").title(), "c");
}

#[test]
fn ids_name_open_windows_only() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 5, 5);
    let b = open(&mut wm, 10, 0, 5, 5);
    assert_eq!((wm.id(1), wm.id(2), wm.id(3), wm.id(0)), (Some(a), Some(b), None, None));
    let _ = wm.close(a).expect("open");
    assert_eq!(wm.id(1), None);
    // Operations on a window that isn't open say so.
    assert_eq!(wm.close(a), None);
    assert_eq!(wm.focus(a), None);
    assert_eq!(wm.move_to(a, 1, 1), None);
}

#[test]
fn a_click_focuses_and_raises() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 12, 10);
    let b = open(&mut wm, 6, 6, 12, 10);
    // A's client, uncovered by B: (2, 5).
    assert_eq!(wm.hit(2, 5), Hit::Window(a, Part::Client));
    let out = wm.pointer(at(2, 5, L));
    assert_eq!(wm.focused(), Some(a));
    assert_eq!(wm.windows().last().map(|w| w.id()), Some(a));
    let client = wm.client(a).expect("open");
    assert_eq!(
        out.deliveries,
        [
            Delivery::FocusLost(b),
            Delivery::FocusGained(a),
            Delivery::PointerDown { window: a, x: 2 - client.x(), y: 5 - client.y(), button: LB, buttons: L },
        ]
    );
    assert!(covers(&out, frame(&wm, a)) && covers(&out, frame(&wm, b)));
    // Where they overlap, A is in front now.
    assert_eq!(wm.hit(8, 8), Hit::Window(a, Part::Client));
    // A click on the focused window in front changes nothing to repaint.
    let _ = wm.pointer(at(2, 5, NONE));
    let out = wm.pointer(at(2, 5, L));
    assert!(out.damage.is_empty());
    assert!(matches!(out.deliveries[..], [Delivery::PointerDown { .. }]));
}

#[test]
fn a_right_or_middle_press_on_a_client_grabs_too() {
    for b in [RB, Button::Middle] {
        let mut wm = wm();
        let a = open(&mut wm, 10, 10, 10, 8);
        let c = wm.client(a).expect("open");
        assert_eq!(
            wm.pointer(at(c.x() + 2, c.y() + 1, b.into())).deliveries,
            [Delivery::PointerDown { window: a, x: 2, y: 1, button: b, buttons: b.into() }]
        );
        assert_eq!(wm.grabbing(), Some(a));
        assert_eq!(
            wm.pointer(at(c.x() + 2, c.y() + 1, NONE)).deliveries,
            [Delivery::PointerUp { window: a, x: 2, y: 1, button: b, buttons: NONE }]
        );
    }
}

#[test]
fn focus_raises_from_outside_the_pointer_too() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 12, 10);
    let b = open(&mut wm, 6, 6, 12, 10);
    let out = wm.focus(a).expect("open");
    assert_eq!(
        (wm.focused(), out.deliveries),
        (Some(a), alloc::vec![Delivery::FocusLost(b), Delivery::FocusGained(a)])
    );
    assert_eq!(wm.focus(a), Some(Outcome::default()));
}

#[test]
fn closing_the_focused_window_focuses_the_next_one_down() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 5, 5);
    let b = open(&mut wm, 10, 0, 5, 5);
    let c = open(&mut wm, 20, 0, 5, 5);
    // Closing one that isn't focused keeps the focus.
    let out = wm.close(b).expect("open");
    assert_eq!((wm.focused(), &out.deliveries[..]), (Some(c), &[][..]));
    let out = wm.close(c).expect("open");
    assert_eq!(wm.focused(), Some(a));
    assert_eq!(out.deliveries, [Delivery::FocusGained(a)]);
    assert!(covers(&out, frame(&wm, a)));
    let out = wm.close(a).expect("open");
    assert_eq!((wm.focused(), out.deliveries.len()), (None, 0));
}

#[test]
fn a_drag_moves_the_window_by_the_pointers_moves() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let f = frame(&wm, a);
    // Press on the title bar at (7, 6), move by (+6, +3), release.
    assert_eq!(wm.hit(7, 6), Hit::Window(a, Part::TitleBar));
    let out = wm.pointer(at(7, 6, L));
    assert_eq!(wm.dragging(), Some(a));
    assert!(out.deliveries.is_empty(), "nothing goes to the window: {:?}", out.deliveries);
    let out = wm.pointer(at(13, 9, L));
    let moved = Rect::new(11, 8, f.w(), f.h());
    assert_eq!(frame(&wm, a), moved);
    assert!(covers(&out, f) && covers(&out, moved), "old and new frames");
    let out = wm.pointer(at(13, 9, NONE));
    assert_eq!(wm.dragging(), None);
    assert!(out.damage.is_empty() && out.deliveries.is_empty());
    // A move with nothing held moves nothing.
    let _ = wm.pointer(at(30, 20, NONE));
    assert_eq!(frame(&wm, a), moved);
}

#[test]
fn another_buttons_click_during_a_drag_doesnt_end_it() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let _ = wm.pointer(at(7, 6, L));
    assert_eq!(wm.pointer(at(7, 6, L.with(R))), Outcome::default());
    assert_eq!(wm.pointer(at(7, 6, L)), Outcome::default());
    assert_eq!(wm.dragging(), Some(a));
    let _ = wm.pointer(at(10, 8, L));
    assert_eq!((frame(&wm, a).x(), frame(&wm, a).y()), (8, 7));
}

#[test]
fn a_drag_is_clamped_to_keep_a_strip_of_the_title_bar_on_screen() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8); // frame 12 x 13
    let f = frame(&wm, a);
    for (to, want) in [
        ((-100, 5), (3 - 12, 5)), // left: 3 pixels stay on the screen
        ((100, 5), (40 - 3, 5)),  // right
        ((5, -100), (5, 0)),      // top: the title bar's top row stays on
        ((5, 100), (5, 30 - 4)),  // bottom: the whole title bar stays on
        ((-100, 100), (-9, 26)),
    ] {
        let _ = wm.move_to(a, to.0, to.1).expect("open");
        assert_eq!((frame(&wm, a).x(), frame(&wm, a).y()), want, "move to {to:?}");
        assert_eq!((frame(&wm, a).w(), frame(&wm, a).h()), (f.w(), f.h()));
    }
    // Opening off the screen is clamped the same way, also at the
    // coordinate limits (by the whole width, before it could be shortened).
    for (x, want_x) in [(1000, 37), (i32::MAX, 37), (i32::MIN, 3 - 12)] {
        let b = open(&mut wm, x, -50, 10, 8);
        assert_eq!((frame(&wm, b).x(), frame(&wm, b).y()), (want_x, 0), "open at x {x}");
        let t = wm.metrics().title_bar(&frame(&wm, b));
        assert!(t.clip(W, H).is_some_and(|on| on.w() >= M.strip), "a strip of the title bar on screen");
    }
}

#[test]
fn a_window_narrower_than_the_strip_stays_whole_on_screen() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 0, 3); // frame 2 wide, under the strip of 3
    assert_eq!(frame(&wm, a).w(), 2);
    let _ = wm.move_to(a, -100, 5).expect("open");
    assert_eq!(frame(&wm, a).x(), 0);
    let _ = wm.move_to(a, 100, 5).expect("open");
    assert_eq!(frame(&wm, a).x(), W as i32 - 2);
}

#[test]
fn releasing_the_left_button_ends_a_drag_even_with_another_held() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let b = open(&mut wm, 25, 15, 8, 8);
    let _ = wm.focus(a).expect("open");
    // Left on A's title bar, right pressed too, left released.
    let _ = wm.pointer(at(7, 6, L));
    assert_eq!(wm.pointer(at(9, 7, L.with(R))).deliveries, []);
    let moved = frame(&wm, a);
    assert_eq!(wm.pointer(at(9, 7, R)), Outcome::default());
    assert_eq!(wm.dragging(), None);
    // Moves with only the right button held move nothing, even over B's
    // client, and its release goes nowhere.
    let bc = wm.client(b).expect("open");
    assert_eq!(wm.pointer(at(bc.x() + 1, bc.y() + 1, R)), Outcome::default());
    assert_eq!(wm.pointer(at(bc.x() + 1, bc.y() + 1, NONE)), Outcome::default());
    assert_eq!(frame(&wm, a), moved);
    // Then a press on B's client is B's.
    assert!(wm.pointer(at(bc.x() + 1, bc.y() + 1, L)).deliveries.contains(&Delivery::PointerDown {
        window: b,
        x: 1,
        y: 1,
        button: LB,
        buttons: L
    }));
}

#[test]
fn closing_a_window_mid_drag_ends_the_drag() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let _ = wm.pointer(at(7, 6, L));
    assert_eq!(wm.dragging(), Some(a));
    let _ = wm.close(a).expect("open");
    assert_eq!(wm.dragging(), None);
    assert_eq!(wm.pointer(at(20, 20, L)), Outcome::default());
}

#[test]
fn a_grab_lasts_until_its_last_button_is_released() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let b = open(&mut wm, 25, 15, 8, 8);
    let _ = wm.focus(a).expect("open");
    let c = wm.client(a).expect("open");
    let bc = wm.client(b).expect("open");
    let _ = wm.pointer(at(c.x(), c.y(), L));
    let _ = wm.pointer(at(c.x(), c.y(), L.with(R)));
    // Left released: A is told, and still holds the grab (it was told of
    // the right press, so it must be told of its release too).
    assert_eq!(
        wm.pointer(at(c.x(), c.y(), R)).deliveries,
        [Delivery::PointerUp { window: a, x: 0, y: 0, button: LB, buttons: R }]
    );
    assert_eq!(wm.grabbing(), Some(a));
    // A move over B's client with the right button held is A's.
    let (dx, dy) = (bc.x() - c.x(), bc.y() - c.y());
    assert_eq!(
        wm.pointer(at(bc.x(), bc.y(), R)).deliveries,
        [Delivery::PointerMove { window: a, x: dx, y: dy, buttons: R }]
    );
    assert_eq!(
        wm.pointer(at(bc.x(), bc.y(), NONE)).deliveries,
        [Delivery::PointerUp { window: a, x: dx, y: dy, button: RB, buttons: NONE }]
    );
    assert_eq!(wm.grabbing(), None);
}

#[test]
fn a_grab_ends_when_its_window_loses_the_focus() {
    let mut wm = wm();
    let mut o = Owners::default();
    let a = open(&mut wm, 0, 0, 10, 8);
    let b = open(&mut wm, 25, 15, 8, 8);
    let _ = wm.focus(a).expect("open");
    let c = wm.client(a).expect("open");
    o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L)));
    // The taskbar focuses B while A still holds the grab.
    let out = wm.focus(b).expect("open");
    assert_eq!(out.deliveries, [Delivery::FocusLost(a), Delivery::FocusGained(b)]);
    assert_eq!((wm.grabbing(), wm.counts().grab_ends), (None, 1));
    o.deliver(&out);
    // Nothing more reaches A: not the moves, not the release.
    for p in [at(c.x() + 3, c.y() + 2, L), at(c.x() + 3, c.y() + 2, L.with(R)), at(c.x() + 3, c.y() + 2, NONE)] {
        assert_eq!(wm.pointer(p), Outcome::default());
    }
    o.read();
    assert_eq!(o.holding(), []);
    assert_eq!(o.strays(), (0, 0), "no release reached A after FocusLost");
}

#[test]
fn a_release_outside_the_screen_ends_the_drag_clamped() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    let _ = wm.pointer(at(7, 6, L));
    let _ = wm.pointer(at(-500, -500, NONE));
    assert_eq!(wm.dragging(), None);
    assert_eq!((frame(&wm, a).x(), frame(&wm, a).y()), (-9, 0));
    // The next press there is on the desktop.
    let out = wm.pointer(at(30, 25, L));
    assert!(out.deliveries.is_empty() && out.damage.is_empty());
}

#[test]
fn a_press_that_starts_outside_any_window_does_nothing_until_released() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let f = frame(&wm, a);
    // Press on the desktop, drag over the window's title bar and client.
    let out = wm.pointer(at(1, 1, L));
    assert_eq!(out, Outcome::default());
    for p in [(12, 11), (14, 16), (16, 18)] {
        assert_eq!(wm.pointer(at(p.0, p.1, L)), Outcome::default());
    }
    // A second button there does nothing either.
    assert_eq!(wm.pointer(at(14, 16, L.with(R))), Outcome::default());
    assert_eq!(wm.pointer(at(14, 16, NONE)), Outcome::default());
    assert_eq!(frame(&wm, a), f);
    // Released: a press on the client is the window's again.
    assert!(matches!(wm.pointer(at(14, 16, L)).deliveries[..], [Delivery::PointerDown { .. }]));
}

#[test]
fn the_grab_follows_the_pointer_out_of_the_window() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    let _ = wm.pointer(at(c.x() + 1, c.y() + 1, L));
    assert_eq!(wm.grabbing(), Some(a));
    // Out of the window and off the screen: client coordinates go
    // negative; the second button goes to the grab too.
    let out = wm.pointer(at(2, 3, L));
    assert_eq!(out.deliveries, [Delivery::PointerMove { window: a, x: 2 - c.x(), y: 3 - c.y(), buttons: L }]);
    let out = wm.pointer(at(-7, 3, L.with(R)));
    assert_eq!(
        out.deliveries,
        [
            Delivery::PointerMove { window: a, x: -7 - c.x(), y: 3 - c.y(), buttons: L },
            Delivery::PointerDown { window: a, x: -7 - c.x(), y: 3 - c.y(), button: RB, buttons: L.with(R) },
        ]
    );
    let out = wm.pointer(at(-7, 3, R));
    assert_eq!(
        out.deliveries,
        [Delivery::PointerUp { window: a, x: -7 - c.x(), y: 3 - c.y(), button: LB, buttons: R }]
    );
    assert_eq!(wm.grabbing(), Some(a));
    let out = wm.pointer(at(-7, 3, NONE));
    assert_eq!(
        out.deliveries,
        [Delivery::PointerUp { window: a, x: -7 - c.x(), y: 3 - c.y(), button: RB, buttons: NONE }]
    );
    assert_eq!(wm.grabbing(), None);
    // Moves with nothing held aren't delivered (no hover).
    assert_eq!(wm.pointer(at(c.x() + 2, c.y() + 2, NONE)), Outcome::default());
}

#[test]
fn a_grab_whose_window_closes_ends() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    let _ = wm.pointer(at(c.x(), c.y(), L));
    let _ = wm.close(a).expect("open");
    assert_eq!((wm.grabbing(), wm.counts().grab_ends), (None, 1));
    assert_eq!(wm.pointer(at(1, 1, L)), Outcome::default());
    assert_eq!(wm.pointer(at(1, 1, NONE)), Outcome::default());
}

#[test]
fn the_close_box_clicks_on_a_left_release_over_it() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let b = wm.metrics().close_box(&frame(&wm, a));
    let (x, y) = (b.x(), b.y());
    assert_eq!(wm.hit(x, y), Hit::Window(a, Part::CloseBox));
    let _ = wm.pointer(at(x, y, L));
    assert_eq!(wm.pointer(at(x + 1, y + 1, NONE)).deliveries, [Delivery::Close(a)]);
    // Pressed on it, released elsewhere: no click.
    let _ = wm.pointer(at(x, y, L));
    assert!(wm.pointer(at(x - 5, y, NONE)).deliveries.is_empty());
    // The right button on it: no click, even with a left click on it
    // while the right is down.
    let _ = wm.pointer(at(x, y, R));
    assert!(wm.pointer(at(x, y, NONE)).deliveries.is_empty());
    let _ = wm.pointer(at(x, y, R));
    let _ = wm.pointer(at(x, y, R.with(L)));
    assert_eq!(wm.pointer(at(x, y, R)), Outcome::default());
    assert_eq!(wm.pointer(at(x, y, NONE)), Outcome::default());
    // Another button clicked while the left is down on it is ignored: the
    // left release still clicks.
    let _ = wm.pointer(at(x, y, L));
    assert_eq!(wm.pointer(at(x, y, L.with(R))), Outcome::default());
    assert_eq!(wm.pointer(at(x, y, L)), Outcome::default());
    assert_eq!(wm.pointer(at(x, y, NONE)).deliveries, [Delivery::Close(a)]);
    // The window manager doesn't close it: the owner does.
    assert!(wm.window(a).is_some());
}

#[test]
fn a_press_on_a_border_focuses_and_raises_and_nothing_more() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let b = open(&mut wm, 30, 20, 4, 4);
    let f = frame(&wm, a);
    let (x, y) = (f.x(), f.bottom() - 1);
    assert_eq!(wm.hit(x, y), Hit::Window(a, Part::Border));
    let out = wm.pointer(at(x, y, L));
    assert_eq!(wm.focused(), Some(a));
    assert_eq!(wm.windows().iter().map(|w| w.id()).collect::<Vec<_>>(), [b, a], "raised");
    assert_eq!(out.deliveries, [Delivery::FocusLost(b), Delivery::FocusGained(a)]);
    assert_eq!(wm.pointer(at(x + 3, y, L)), Outcome::default());
    assert_eq!(frame(&wm, a), f);
}

#[test]
fn tablet_positions_are_converted_saturating() {
    let s = lumen_input::PointerState { x: 12, y: usize::MAX, buttons: R, ..Default::default() };
    let p = Pointer::from_state(&s, true);
    assert_eq!((p.x, p.y, p.buttons, p.after_overrun), (12, i32::MAX, R, true));
}

#[test]
fn a_read_frame_carries_its_gap_either_way() {
    // `Pointer::from_read`: the frame's state, and `after_overrun` set when
    // the reader fell behind the ring or the frame says frames were lost
    // at the device (all four combinations).
    use lumen_input::{PointerEvent, PointerState, Read, Ring};
    let state = PointerState { x: 7, y: 9, buttons: Buttons::LEFT, ..Default::default() };
    let mut ring = Ring::<PointerEvent, 2>::new();
    let seq = ring.push(PointerEvent { state, after_overrun: false });
    for (lost_before, marked, gap) in [(0, false, false), (0, true, true), (3, false, true), (3, true, true)] {
        let read = Read { seq, lost_before, event: PointerEvent { state, after_overrun: marked } };
        let p = Pointer::from_read(&read);
        assert_eq!((p.x, p.y, p.buttons, p.after_overrun), (7, 9, L, gap), "{lost_before} {marked}");
    }
}

/// No keys in these tests (the window manager deals in pointer events).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoKey;

impl KeyLike for NoKey {
    type Code = ();
    fn code(&self) {}
    fn is_press(&self) -> bool {
        true
    }
}

/// The windows' side: each window's queue, and the buttons its owner
/// thinks are held after reading everything.
#[derive(Default)]
struct Owners {
    queues: BTreeMap<WindowId, EventQueue<NoKey, ()>>,
    held: BTreeMap<WindowId, Buttons>,
}

impl Owners {
    /// What the compositor does with an outcome: each delivery onto its
    /// window's queue.
    fn deliver(&mut self, out: &Outcome) {
        for d in &out.deliveries {
            let _ = deliver(self.queues.entry(d.window()).or_default(), d);
        }
    }

    /// Forgets the owners of windows that are gone.
    fn keep(&mut self, wm: &Wm) {
        self.queues.retain(|w, _| wm.window(*w).is_some());
        self.held.retain(|w, _| wm.window(*w).is_some());
    }

    /// Each owner reads its whole queue.
    fn read(&mut self) {
        for (w, q) in &mut self.queues {
            let held = self.held.entry(*w).or_default();
            while let Some(n) = q.next_event() {
                match n {
                    Next::Lost | Next::Focus(false) => *held = NONE,
                    Next::Event(Entry::Pointer(PointerEntry::Down { button, .. })) => *held = held.with(button.into()),
                    Next::Event(Entry::Pointer(PointerEntry::Up { button, .. })) => *held = held.without(button.into()),
                    Next::Focus(true) | Next::Close | Next::Event(Entry::Pointer(PointerEntry::Move { .. })) => {}
                    Next::Event(Entry::Key(_) | Entry::Request(())) => unreachable!("no keys or requests here"),
                }
            }
        }
    }

    fn holding(&self) -> Vec<(WindowId, Buttons)> {
        self.held.iter().filter(|(_, b)| !b.is_empty()).map(|(w, b)| (*w, *b)).collect()
    }

    /// Strays over every queue: (forgotten, never pressed).
    fn strays(&self) -> (u64, u64) {
        self.queues
            .values()
            .map(|q| q.counts())
            .fold((0, 0), |(f, u), c| (f + c.stray_forgotten, u + c.stray_unpressed))
    }
}

/// Press on a client, interrupt (the owner releases everything at once),
/// then (after resuming, for a pause, with both buttons still held at the
/// cut) release: no window is left holding a button, and the window works
/// as before.
fn press_interrupt_release(interrupt: impl Fn(&mut Wm, WindowId) -> Outcome, resume: bool) {
    let mut wm = wm();
    let mut o = Owners::default();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L.with(R))));
    o.read();
    assert_eq!(o.holding(), [(a, L.with(R))], "the press reached the owner");
    o.deliver(&interrupt(&mut wm, a));
    o.read();
    assert_eq!(o.holding(), [], "the interruption released the owner's buttons");
    if resume {
        assert_eq!(wm.resume(L.with(R)).deliveries, [Delivery::FocusGained(a)]);
    }
    for p in [at(c.x() + 3, c.y() + 1, L), at(c.x() + 3, c.y() + 1, NONE)] {
        assert_eq!(wm.pointer(p), Outcome::default(), "nothing until every button is up");
    }
    assert_eq!(wm.grabbing(), None);
    o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L)));
    o.read();
    assert_eq!(o.holding(), [(a, L)]);
    assert_eq!(o.strays(), (0, 0));
}

#[test]
fn press_pause_resume_release_leaves_no_button_held() {
    press_interrupt_release(
        |wm, a| {
            let out = wm.interrupt(Interruption::Pause);
            assert_eq!(out.deliveries, [Delivery::FocusLost(a)]);
            out
        },
        true,
    );
}

#[test]
fn press_gap_release_leaves_no_button_held() {
    press_interrupt_release(
        |wm, a| {
            let out = wm.interrupt(Interruption::Gap);
            assert_eq!(out.deliveries, [Delivery::Lost(a)]);
            out
        },
        false,
    );
}

#[test]
fn press_then_an_event_after_an_overrun_leaves_no_button_held() {
    // The gap is the next event's `after_overrun`, at the same place with
    // the same buttons: the grabbing window is told it lost its events, and
    // the grab ends.
    press_interrupt_release(
        |wm, a| {
            let c = wm.client(a).expect("open");
            let out = wm.pointer(Pointer { after_overrun: true, ..at(c.x() + 1, c.y() + 1, L.with(R)) });
            assert_eq!(out.deliveries, [Delivery::Lost(a)]);
            assert_eq!(wm.grabbing(), None);
            out
        },
        false,
    );
}

#[test]
fn the_first_click_after_a_resume_or_a_gap_is_a_click() {
    for gap in [false, true] {
        let mut wm = wm();
        let a = open(&mut wm, 10, 10, 10, 8);
        let c = wm.client(a).expect("open");
        if gap {
            assert_eq!(wm.interrupt(Interruption::Gap).deliveries, [Delivery::Lost(a)]);
        } else {
            let _ = wm.interrupt(Interruption::Pause);
            let _ = wm.resume(NONE);
        }
        // A press with no move first, as the harness's click sends it.
        assert_eq!(
            wm.pointer(at(c.x() + 1, c.y() + 1, L)).deliveries,
            [Delivery::PointerDown { window: a, x: 1, y: 1, button: LB, buttons: L }],
            "gap {gap}"
        );
        assert_eq!(wm.counts().ignored_presses, 0);
    }
}

#[test]
fn a_button_held_at_the_resume_is_ignored_until_every_button_is_up() {
    // Pressed in a terminal while the desktop was paused and still held at
    // the cut, over a window's client and then its title bar: neither a
    // press nor a drag, and another press meanwhile is counted.
    for (x, y) in [(15, 18), (12, 11)] {
        let mut wm = wm();
        let a = open(&mut wm, 10, 10, 10, 8);
        let f = frame(&wm, a);
        let _ = wm.interrupt(Interruption::Pause);
        let _ = wm.resume(L);
        assert_eq!(wm.pointer(at(x, y, L)), Outcome::default(), "({x}, {y})");
        assert_eq!(wm.pointer(at(x + 4, y + 2, L.with(R))), Outcome::default());
        assert_eq!(wm.counts().ignored_presses, 1);
        assert_eq!(wm.pointer(at(x + 4, y + 2, R)), Outcome::default());
        assert_eq!(wm.pointer(at(x + 4, y + 2, NONE)), Outcome::default());
        assert_eq!(frame(&wm, a), f);
        assert!(matches!(wm.pointer(at(15, 18, L)).deliveries[..], [Delivery::PointerDown { .. }]));
    }
}

#[test]
fn while_paused_focus_changes_wait_for_the_resume() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    assert_eq!(wm.interrupt(Interruption::Pause).deliveries, [Delivery::FocusLost(a)]);
    assert!(wm.is_paused());
    // A second pause, and a gap meanwhile, tell nobody anything.
    assert_eq!(wm.interrupt(Interruption::Pause), Outcome::default());
    assert_eq!(wm.interrupt(Interruption::Gap), Outcome::default());
    // Closing the focused window and opening another: damage, no focus
    // deliveries; pointer events are ignored.
    let out = wm.close(a).expect("open");
    assert!(out.deliveries.is_empty() && !out.damage.is_empty());
    let (b, out) = wm.open("b", 5, 5, 0, 0);
    assert!(out.deliveries.is_empty() && !out.damage.is_empty());
    let c = wm.client(b).expect("open");
    assert_eq!(wm.pointer(at(c.x(), c.y(), L)), Outcome::default());
    assert_eq!(wm.focused(), Some(b));
    // The resume tells the window focused now, once.
    assert_eq!(wm.resume(NONE).deliveries, [Delivery::FocusGained(b)]);
    assert!(!wm.is_paused());
    assert_eq!(wm.resume(NONE), Outcome::default(), "a resume without a pause");
}

#[test]
fn a_key_gap_ends_a_grab_and_leaves_a_drag() {
    // A grab: the focused window is told, the grab ends, its buttons are
    // ignored until up (not counted as presses after an interruption).
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    let _ = wm.pointer(at(c.x(), c.y(), L));
    assert_eq!(wm.interrupt(Interruption::KeyGap).deliveries, [Delivery::Lost(a)]);
    assert_eq!((wm.grabbing(), wm.counts().grab_ends), (None, 1));
    assert_eq!(wm.pointer(at(c.x() + 2, c.y(), L.with(R))), Outcome::default());
    assert_eq!(wm.pointer(at(c.x() + 2, c.y(), NONE)), Outcome::default());
    assert_eq!(wm.counts().ignored_presses, 0);
    assert!(matches!(wm.pointer(at(c.x(), c.y(), L)).deliveries[..], [Delivery::PointerDown { .. }]));
    // A drag: told too, and the drag goes on.
    let mut wm = Wm::new(W, H, M);
    let a = open(&mut wm, 5, 5, 10, 8);
    let _ = wm.pointer(at(7, 6, L));
    assert_eq!(wm.interrupt(Interruption::KeyGap).deliveries, [Delivery::Lost(a)]);
    assert_eq!((wm.dragging(), wm.counts().grab_ends), (Some(a), 0));
    let _ = wm.pointer(at(10, 8, L));
    assert_eq!((frame(&wm, a).x(), frame(&wm, a).y()), (8, 7));
    // While paused, and with no window, nobody is told.
    let _ = wm.interrupt(Interruption::Pause);
    assert_eq!(wm.interrupt(Interruption::KeyGap), Outcome::default());
    let mut empty = Wm::new(W, H, M);
    assert_eq!(empty.interrupt(Interruption::KeyGap), Outcome::default());
}

#[test]
fn a_pause_or_a_gap_ends_a_grab_and_counts_it() {
    for why in [Interruption::Pause, Interruption::Gap] {
        let mut wm = wm();
        let a = open(&mut wm, 10, 10, 10, 8);
        let c = wm.client(a).expect("open");
        let _ = wm.pointer(at(c.x(), c.y(), L));
        let _ = wm.interrupt(why);
        assert_eq!((wm.grabbing(), wm.counts().grab_ends), (None, 1), "{why:?}");
    }
}

#[test]
fn a_press_in_an_event_after_an_overrun_isnt_a_press() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    // Nothing held before; the event after the overrun holds the left
    // button, pressed in the frames lost.
    let out = wm.pointer(Pointer { after_overrun: true, ..at(c.x(), c.y(), L) });
    assert_eq!(out.deliveries, [Delivery::Lost(a)]);
    assert_eq!((wm.grabbing(), wm.counts().ignored_presses), (None, 1));
    assert_eq!(wm.pointer(at(c.x(), c.y(), NONE)), Outcome::default());
    assert!(matches!(wm.pointer(at(c.x(), c.y(), L)).deliveries[..], [Delivery::PointerDown { .. }]));
}

#[test]
fn presses_are_counted_as_ignored_only_after_an_interruption() {
    let mut wm = wm();
    let _a = open(&mut wm, 10, 10, 10, 8);
    let _ = wm.interrupt(Interruption::Pause);
    let _ = wm.resume(L);
    // Each press while the baseline's buttons are down counts once.
    let _ = wm.pointer(at(1, 1, L.with(R)));
    let _ = wm.pointer(at(1, 1, L));
    let _ = wm.pointer(at(1, 1, L.with(R)));
    assert_eq!(wm.counts().ignored_presses, 2);
    let _ = wm.pointer(at(1, 1, NONE));
    // A press on the desktop and another button with it: ignored, not
    // counted.
    let _ = wm.pointer(at(1, 1, L));
    let _ = wm.pointer(at(1, 1, L.with(R)));
    let _ = wm.pointer(at(1, 1, NONE));
    assert_eq!(wm.counts().ignored_presses, 2);
}

#[test]
fn a_focus_loss_while_paused_is_held_back() {
    let mut wm = wm();
    let _a = open(&mut wm, 0, 0, 5, 5);
    let b = open(&mut wm, 10, 0, 5, 5);
    assert_eq!(wm.interrupt(Interruption::Pause).deliveries, [Delivery::FocusLost(b)]);
    // Opening C moves the focus from B to C: neither is told now.
    let (c, out) = wm.open("c", 5, 5, 20, 0);
    assert_eq!(out.deliveries, []);
    assert_eq!(wm.resume(NONE).deliveries, [Delivery::FocusGained(c)]);
}

/// What the picture shows at a pixel: the painter's algorithm, back to
/// front, the last window covering it wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Px {
    Desktop,
    Window { id: WindowId, part: Part, focused: bool },
}

fn painted(wm: &Wm, x: i32, y: i32) -> Px {
    let mut px = Px::Desktop;
    for w in wm.windows() {
        if let Some(part) = wm.metrics().part_at(&w.frame(), x, y) {
            px = Px::Window { id: w.id(), part, focused: wm.focused() == Some(w.id()) };
        }
    }
    px
}

fn full(wm: &Wm) -> Vec<Px> {
    (0..H as i32).flat_map(|y| (0..W as i32).map(move |x| (x, y))).map(|(x, y)| painted(wm, x, y)).collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u64) as i32
    }
    fn pick<T: Copy>(&mut self, of: &[T]) -> Option<T> {
        (!of.is_empty()).then(|| of[self.next() as usize % of.len()])
    }
    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }
    /// A random pixel of `r` (its corner if it is empty).
    fn point_in(&mut self, r: Rect) -> (i32, i32) {
        (r.x() + self.range(0, r.w().max(1) as i32), r.y() + self.range(0, r.h().max(1) as i32))
    }
}

/// An owner answering a close-box click: it closes its window now and
/// then. Returns how many clicks there were.
fn answer_closes(wm: &mut Wm, rng: &mut Rng, out: &Outcome, outs: &mut Vec<Outcome>) -> u32 {
    let mut clicks = 0;
    for d in &out.deliveries {
        if let Delivery::Close(id) = *d {
            clicks += 1;
            if rng.one_in(2) {
                outs.extend(wm.close(id));
            }
        }
    }
    clicks
}

#[test]
fn repainting_only_the_damage_matches_repainting_everything() {
    let buttons = [L, R, Buttons::MIDDLE];
    let (mut clicks, mut drags, mut follows, mut downs) = (0, 0, 0, 0);
    let (mut grab_seconds, mut held_resumes) = (0, 0);
    for seed in [1, 0xdead_beef, 0x1234_5678_9abc_def1, 42] {
        let mut rng = Rng(seed);
        let mut wm = wm();
        let mut o = Owners::default();
        let mut screen = full(&wm);
        // What the tablet holds (it goes on while the desktop is paused).
        let mut held = NONE;
        let mut ids: Vec<WindowId> = Vec::new();
        // A drag's offset from the frame's corner, as it started.
        let mut drag: Option<(WindowId, i32, i32)> = None;
        for step in 0..4000 {
            let ids_now: Vec<WindowId> = wm.windows().iter().map(|w| w.id()).collect();
            let mut outs = Vec::new();
            match rng.range(0, 16) {
                0 | 1 if ids_now.len() < 6 => {
                    let (x, y) = (rng.range(-10, W as i32 + 5), rng.range(-10, H as i32 + 5));
                    let (id, out) = wm.open("w", rng.range(0, 20) as u32, rng.range(0, 14) as u32, x, y);
                    ids.push(id);
                    outs.push(out);
                }
                // Sometimes an id already closed.
                2 => outs.extend(rng.pick(&ids).and_then(|id| wm.close(id))),
                3 => outs.extend(rng.pick(&ids_now).and_then(|id| wm.focus(id))),
                4 => outs.extend(
                    rng.pick(&ids_now)
                        .and_then(|id| wm.move_to(id, rng.range(-50, W as i32 + 50), rng.range(-50, H as i32 + 50))),
                ),
                5 => outs.push(wm.interrupt(match rng.range(0, 3) {
                    0 => Interruption::Pause,
                    1 => Interruption::Gap,
                    _ => Interruption::KeyGap,
                })),
                6 => {
                    if wm.is_paused() && !held.is_empty() {
                        held_resumes += 1;
                    }
                    outs.push(wm.resume(held));
                }
                // A click (press and release at one place, as the
                // harness's `click` sends it) on a window's close box,
                // title bar or client.
                7 if held.is_empty() && !ids_now.is_empty() => {
                    let id = rng.pick(&ids_now).expect("a window");
                    let f = frame(&wm, id);
                    let kind = rng.range(0, 3);
                    let aim = match kind {
                        0 => wm.metrics().close_box(&f),
                        1 => wm.metrics().title_bar(&f),
                        _ => wm.metrics().client(&f),
                    };
                    let (x, y) = rng.point_in(aim);
                    for b in [L, NONE] {
                        let out = wm.pointer(at(x, y, b));
                        let n = answer_closes(&mut wm, &mut rng, &out, &mut outs);
                        if kind == 0 {
                            clicks += n;
                        }
                        outs.push(out);
                    }
                }
                _ => {
                    if rng.one_in(3) {
                        let b = rng.pick(&buttons).expect("buttons");
                        held = if held.contains(b) { held.without(b) } else { held.with(b) };
                    }
                    // Aim at a window's parts often, so drags, grabs and
                    // close-box clicks happen.
                    let (x, y) = match rng.pick(&ids_now).filter(|_| rng.one_in(2)) {
                        Some(id) => {
                            let f = frame(&wm, id);
                            let aim = match rng.range(0, 3) {
                                0 => wm.metrics().title_bar(&f),
                                1 => wm.metrics().close_box(&f),
                                _ => f,
                            };
                            rng.point_in(aim)
                        }
                        None => (rng.range(-8, W as i32 + 8), rng.range(-8, H as i32 + 8)),
                    };
                    if wm.grabbing().is_some() && !held.without(wm.buttons()).is_empty() {
                        grab_seconds += 1;
                    }
                    let p = Pointer { after_overrun: rng.one_in(40), ..at(x, y, held) };
                    // A first press on a client is that window's.
                    let first = (wm.is_idle() && !wm.is_paused() && wm.buttons().is_empty())
                        .then(|| held.each().next())
                        .flatten()
                        .and_then(|b| match wm.hit(x, y) {
                            Hit::Window(id, Part::Client) => Some((id, b)),
                            _ => None,
                        });
                    let out = wm.pointer(p);
                    if let Some((id, b)) = first {
                        assert!(
                            out.deliveries.iter().any(|d| matches!(*d, Delivery::PointerDown { window, button, .. } if window == id && button == b)),
                            "seed {seed:#x} step {step}: a first press on {id:?}'s client: {:?}",
                            out.deliveries
                        );
                        downs += 1;
                    }
                    // A drag follows the pointer, clamped.
                    match (wm.dragging(), drag) {
                        (Some(id), Some((was, dx, dy))) if id == was => {
                            let f = frame(&wm, id);
                            assert_eq!((f.x(), f.y()), clamped(f.w(), x - dx, y - dy), "seed {seed:#x} step {step}");
                            follows += 1;
                        }
                        (Some(id), _) => {
                            let f = frame(&wm, id);
                            drag = Some((id, x - f.x(), y - f.y()));
                            drags += 1;
                        }
                        (None, _) => drag = None,
                    }
                    answer_closes(&mut wm, &mut rng, &out, &mut outs);
                    outs.insert(0, out);
                }
            }
            if wm.dragging().is_none() {
                drag = None;
            }
            for out in &outs {
                // Repaint only the damage, on the screen.
                for d in out.damage.rects() {
                    if let Some(r) = d.clip(W, H) {
                        for y in r.y()..r.bottom() {
                            for x in r.x()..r.right() {
                                screen[(y as u32 * W + x as u32) as usize] = painted(&wm, x, y);
                            }
                        }
                    }
                }
                o.deliver(out);
            }
            assert_eq!(screen, full(&wm), "seed {seed:#x} step {step}: damage missed a change");
            // A drag lasts only while the left button is held.
            if wm.dragging().is_some() {
                assert!(wm.buttons().contains(L), "seed {seed:#x} step {step}: a drag without the left button");
            }
            // Exactly one window is focused (the one in front), or none;
            // a grab is always the focused window's.
            assert_eq!(wm.focused(), wm.windows().last().map(|w| w.id()), "seed {seed:#x} step {step}");
            if let Some(g) = wm.grabbing() {
                assert_eq!(wm.focused(), Some(g), "seed {seed:#x} step {step}");
            }
            // Hit testing agrees with the picture.
            for _ in 0..4 {
                let (x, y) = (rng.range(0, W as i32), rng.range(0, H as i32));
                let want = match painted(&wm, x, y) {
                    Px::Desktop => Hit::Desktop,
                    Px::Window { id, part, .. } => Hit::Window(id, part),
                };
                assert_eq!(wm.hit(x, y), want, "seed {seed:#x} step {step} ({x}, {y})");
            }
            // The owners agree with the window manager: only the grabbing
            // window thinks buttons are held, and only buttons held; and
            // no release ever came for something never pressed.
            o.read();
            for (w, b) in o.holding() {
                assert_eq!(wm.grabbing(), Some(w), "seed {seed:#x} step {step}: {w:?} holds {b:?}");
                assert!(
                    wm.buttons().contains(b),
                    "seed {seed:#x} step {step}: {w:?} holds {b:?} of {:?}",
                    wm.buttons()
                );
            }
            assert_eq!(o.strays(), (0, 0), "seed {seed:#x} step {step}: a stray release reached a queue");
            o.keep(&wm);
        }
    }
    // The model reached what it checks.
    assert!(
        clicks >= 10 && drags >= 10 && follows >= 20 && downs >= 20 && grab_seconds >= 10 && held_resumes >= 5,
        "close-box clicks {clicks}, drags {drags} (followed {follows} times), first presses {downs}, \
         second buttons in a grab {grab_seconds}, resumes with buttons held {held_resumes}"
    );
}
