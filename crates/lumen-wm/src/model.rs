//! Tests of [`Wm`] as a whole: focus rules, the drag state machine, the
//! grab, the close box, clamping, the randomized damage model, and press,
//! pause (or gap), resume, release through [`EventQueue`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_gfx::Rect;

use crate::geom::{Metrics, Part};
use crate::queue::{Class, EventQueue, Next, QueueEvent};
use crate::wm::{ButtonSet, Delivery, Hit, Interruption, Outcome, Pointer, WindowId, Wm};

const M: Metrics = Metrics { title_h: 4, border: 1, close: 2, close_margin: 0, strip: 3 };
const W: u32 = 40;
const H: u32 = 30;
const L: ButtonSet = ButtonSet::LEFT;
const R: ButtonSet = ButtonSet::RIGHT;
const NONE: ButtonSet = ButtonSet::NONE;

fn wm() -> Wm {
    Wm::new(W, H, M)
}

fn at(x: i32, y: i32, b: ButtonSet) -> Pointer {
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
    wm.close(b);
    let (c, _) = wm.open("c", 1, 1, 0, 0);
    assert_eq!(c.get(), 3);
    assert_eq!(wm.windows().iter().map(|w| w.id()).collect::<Vec<_>>(), [a, c]);
    assert_eq!(wm.window(c).expect("open").title(), "c");
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
            Delivery::PointerDown { window: a, x: 2 - client.x(), y: 5 - client.y(), button: L, buttons: L },
        ]
    );
    assert!(covers(&out, frame(&wm, a)) && covers(&out, frame(&wm, b)));
    // Where they overlap, A is in front now.
    assert_eq!(wm.hit(8, 8), Hit::Window(a, Part::Client));
    // A click on the focused window in front changes nothing to repaint.
    wm.pointer(at(2, 5, NONE));
    let out = wm.pointer(at(2, 5, L));
    assert!(out.damage.is_empty());
    assert!(matches!(out.deliveries[..], [Delivery::PointerDown { .. }]));
}

#[test]
fn focus_raises_from_outside_the_pointer_too() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 12, 10);
    let b = open(&mut wm, 6, 6, 12, 10);
    let out = wm.focus(a);
    assert_eq!(
        (wm.focused(), out.deliveries),
        (Some(a), alloc::vec![Delivery::FocusLost(b), Delivery::FocusGained(a)])
    );
    assert!(wm.focus(a).damage.is_empty() && wm.focus(a).deliveries.is_empty());
}

#[test]
fn closing_the_focused_window_focuses_the_next_one_down() {
    let mut wm = wm();
    let a = open(&mut wm, 0, 0, 5, 5);
    let b = open(&mut wm, 10, 0, 5, 5);
    let c = open(&mut wm, 20, 0, 5, 5);
    // Closing one that isn't focused keeps the focus.
    let out = wm.close(b);
    assert_eq!((wm.focused(), &out.deliveries[..]), (Some(c), &[][..]));
    let out = wm.close(c);
    assert_eq!(wm.focused(), Some(a));
    assert_eq!(out.deliveries, [Delivery::FocusGained(a)]);
    assert!(covers(&out, frame(&wm, a)));
    let out = wm.close(a);
    assert_eq!((wm.focused(), out.deliveries.len()), (None, 0));
    // Closing one that isn't there does nothing.
    assert_eq!(wm.close(a), Outcome::default());
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
    wm.pointer(at(30, 20, NONE));
    assert_eq!(frame(&wm, a), moved);
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
        wm.move_to(a, to.0, to.1);
        assert_eq!((frame(&wm, a).x(), frame(&wm, a).y()), want, "move to {to:?}");
        assert_eq!((frame(&wm, a).w(), frame(&wm, a).h()), (f.w(), f.h()));
    }
    // Opening off the screen is clamped the same way.
    let b = open(&mut wm, 1000, -50, 10, 8);
    assert_eq!((frame(&wm, b).x(), frame(&wm, b).y()), (37, 0));
}

#[test]
fn a_release_outside_the_screen_ends_the_drag_clamped() {
    let mut wm = wm();
    let a = open(&mut wm, 5, 5, 10, 8);
    wm.pointer(at(7, 6, L));
    wm.pointer(at(-500, -500, NONE));
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
    wm.pointer(at(c.x() + 1, c.y() + 1, L));
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
            Delivery::PointerDown { window: a, x: -7 - c.x(), y: 3 - c.y(), button: R, buttons: L.with(R) },
        ]
    );
    let out = wm.pointer(at(-7, 3, R));
    assert_eq!(out.deliveries, [Delivery::PointerUp { window: a, x: -7 - c.x(), y: 3 - c.y(), button: L, buttons: R }]);
    assert_eq!(wm.grabbing(), Some(a));
    let out = wm.pointer(at(-7, 3, NONE));
    assert_eq!(
        out.deliveries,
        [Delivery::PointerUp { window: a, x: -7 - c.x(), y: 3 - c.y(), button: R, buttons: NONE }]
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
    wm.pointer(at(c.x(), c.y(), L));
    wm.close(a);
    assert_eq!((wm.grabbing(), wm.counts().grab_ends), (None, 1));
    assert_eq!(wm.pointer(at(1, 1, L)), Outcome::default());
    assert_eq!(wm.pointer(at(1, 1, NONE)), Outcome::default());
}

#[test]
fn the_close_box_clicks_on_release_over_it() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let b = wm.metrics().close_box(&frame(&wm, a));
    let (x, y) = (b.x(), b.y());
    assert_eq!(wm.hit(x, y), Hit::Window(a, Part::CloseBox));
    wm.pointer(at(x, y, L));
    assert_eq!(wm.pointer(at(x + 1, y + 1, NONE)).deliveries, [Delivery::Close(a)]);
    // Pressed on it, released elsewhere: no click.
    wm.pointer(at(x, y, L));
    assert!(wm.pointer(at(x - 5, y, NONE)).deliveries.is_empty());
    // The right button on it: no click.
    wm.pointer(at(x, y, R));
    assert!(wm.pointer(at(x, y, NONE)).deliveries.is_empty());
    // The window manager doesn't close it: the owner does.
    assert!(wm.window(a).is_some());
}

#[test]
fn a_press_on_a_border_focuses_and_raises_and_nothing_more() {
    let mut wm = wm();
    let a = open(&mut wm, 10, 10, 10, 8);
    let _b = open(&mut wm, 30, 20, 4, 4);
    let f = frame(&wm, a);
    let (x, y) = (f.x(), f.bottom() - 1);
    assert_eq!(wm.hit(x, y), Hit::Window(a, Part::Border));
    let out = wm.pointer(at(x, y, L));
    assert_eq!(wm.focused(), Some(a));
    assert!(!out.deliveries.iter().any(|d| matches!(d, Delivery::PointerDown { .. })));
    assert_eq!(wm.pointer(at(x + 3, y, L)), Outcome::default());
    assert_eq!(frame(&wm, a), f);
}

#[test]
fn tablet_positions_are_converted_saturating() {
    let mut s = lumen_input::PointerState { x: 12, y: usize::MAX, ..Default::default() };
    s.buttons = lumen_input::Buttons::RIGHT;
    let p = Pointer::from_state(&s, true);
    assert_eq!((p.x, p.y, p.buttons, p.after_overrun), (12, i32::MAX, R, true));
    let s = lumen_input::PointerState { buttons: lumen_input::Buttons::EXTRA, ..Default::default() };
    assert_eq!(Pointer::from_state(&s, false).buttons, ButtonSet::EXTRA);
}

/// A window's events as #153 queues them (a stand-in for `WindowEvent`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Down(ButtonSet),
    Up(ButtonSet),
    Move,
}

impl QueueEvent for Ev {
    type Key = u8;
    fn class(&self) -> Class<u8> {
        match *self {
            Ev::Down(b) => Class::Press(b.bits()),
            Ev::Up(b) => Class::Release(b.bits()),
            Ev::Move => Class::Move,
        }
    }
}

/// The windows' side: each window's queue, and the buttons its owner
/// thinks are held after reading everything.
#[derive(Default)]
struct Owners {
    queues: BTreeMap<u32, EventQueue<Ev>>,
    held: BTreeMap<u32, ButtonSet>,
}

impl Owners {
    /// What the compositor does with an outcome: queue each delivery.
    fn deliver(&mut self, out: &Outcome) {
        enum Act {
            Push(Ev),
            Focus(bool),
            Lost,
            Close,
        }
        for d in &out.deliveries {
            let (w, act) = match *d {
                Delivery::PointerDown { window, button, .. } => (window, Act::Push(Ev::Down(button))),
                Delivery::PointerUp { window, button, .. } => (window, Act::Push(Ev::Up(button))),
                Delivery::PointerMove { window, .. } => (window, Act::Push(Ev::Move)),
                Delivery::FocusGained(window) => (window, Act::Focus(true)),
                Delivery::FocusLost(window) => (window, Act::Focus(false)),
                Delivery::Lost(window) => (window, Act::Lost),
                Delivery::Close(window) => (window, Act::Close),
            };
            let q = self.queues.entry(w.get()).or_default();
            match act {
                Act::Push(e) => {
                    let _ = q.push(e);
                }
                Act::Focus(gained) => q.set_focus(gained),
                Act::Lost => q.set_lost(),
                Act::Close => q.request_close(),
            }
        }
    }

    /// Forgets the owners of windows that are gone.
    fn keep(&mut self, wm: &Wm) {
        let alive = |w: &u32| wm.windows().iter().any(|x| x.id().get() == *w);
        self.queues.retain(|w, _| alive(w));
        self.held.retain(|w, _| alive(w));
    }

    /// Each owner reads its whole queue.
    fn read(&mut self) {
        for (w, q) in &mut self.queues {
            let held = self.held.entry(*w).or_default();
            while let Some(n) = q.next_event() {
                match n {
                    Next::Lost | Next::Focus(false) => *held = NONE,
                    Next::Event(Ev::Down(b)) => *held = held.with(b),
                    Next::Event(Ev::Up(b)) => *held = held.without(b),
                    Next::Focus(true) | Next::Close | Next::Event(Ev::Move) => {}
                }
            }
        }
    }

    fn holding(&self) -> Vec<(u32, ButtonSet)> {
        self.held.iter().filter(|(_, b)| !b.is_empty()).map(|(w, b)| (*w, *b)).collect()
    }
}

/// Press on a client, interrupt, then (after resuming, for a pause)
/// release: no window is left holding a button.
fn press_interrupt_release(interrupt: impl Fn(&mut Wm) -> Outcome, resume: bool) {
    let mut wm = wm();
    let mut o = Owners::default();
    let a = open(&mut wm, 10, 10, 10, 8);
    let c = wm.client(a).expect("open");
    o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L.with(R))));
    o.read();
    assert_eq!(o.holding(), [(a.get(), L.with(R))], "the press reached the owner");
    o.deliver(&interrupt(&mut wm));
    if resume {
        o.deliver(&wm.resume());
    }
    // Still held at the next event (the starting point), then released.
    o.deliver(&wm.pointer(at(c.x() + 3, c.y() + 1, L)));
    o.deliver(&wm.pointer(at(c.x() + 3, c.y() + 1, NONE)));
    o.read();
    assert_eq!(o.holding(), [], "no window holds a button");
    // The window works as before.
    o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L)));
    o.read();
    assert_eq!(o.holding(), [(a.get(), L)]);
}

#[test]
fn press_pause_resume_release_leaves_no_button_held() {
    press_interrupt_release(|wm| wm.interrupt(Interruption::Pause), true);
}

#[test]
fn press_gap_release_leaves_no_button_held() {
    press_interrupt_release(|wm| wm.interrupt(Interruption::Gap), false);
}

#[test]
fn press_then_an_event_after_an_overrun_leaves_no_button_held() {
    // The gap is the next event's `after_overrun`: nothing is delivered
    // until every button is up, so L's release isn't either.
    press_interrupt_release(
        |wm| {
            let p = Pointer { after_overrun: true, ..at(0, 0, L.with(R)) };
            wm.pointer(p)
        },
        false,
    );
}

#[test]
fn a_button_pressed_while_paused_is_ignored_until_released() {
    // Pressed in a terminal while the desktop was paused (the compositor
    // reads no pointer events then), still held when it resumes, over a
    // window's client and then its title bar: neither a press nor a drag.
    for (x, y) in [(15, 18), (12, 11)] {
        let mut wm = wm();
        let a = open(&mut wm, 10, 10, 10, 8);
        let f = frame(&wm, a);
        wm.interrupt(Interruption::Pause);
        wm.resume();
        assert_eq!(wm.pointer(at(x, y, L)), Outcome::default(), "({x}, {y})");
        assert_eq!(wm.pointer(at(x + 4, y + 2, L)), Outcome::default());
        assert_eq!(wm.pointer(at(x + 4, y + 2, NONE)), Outcome::default());
        assert_eq!(frame(&wm, a), f);
        assert!(matches!(wm.pointer(at(15, 18, L)).deliveries[..], [Delivery::PointerDown { .. }]));
    }
}

#[test]
fn a_grab_on_a_window_that_lost_the_focus_is_told_too() {
    for why in [Interruption::Pause, Interruption::Gap] {
        let mut wm = wm();
        let mut o = Owners::default();
        let a = open(&mut wm, 0, 0, 10, 8);
        let b = open(&mut wm, 25, 15, 8, 8);
        let c = wm.client(a).expect("open");
        o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L)));
        // The taskbar focuses B while A still holds the grab.
        o.deliver(&wm.focus(b));
        o.read();
        assert_eq!(o.holding(), [], "{why:?}: losing the focus releases A's press");
        o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, L.with(R))));
        let out = wm.interrupt(why);
        assert!(out.deliveries.contains(&Delivery::Lost(a)), "{why:?}: {:?}", out.deliveries);
        o.deliver(&out);
        o.deliver(&wm.pointer(at(c.x() + 1, c.y() + 1, NONE)));
        o.read();
        assert_eq!(o.holding(), [], "{why:?}");
    }
}

/// What the picture shows at a pixel: the painter's algorithm, back to
/// front, the last window covering it wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Px {
    Desktop,
    Window { id: u32, part: Part, focused: bool },
}

fn painted(wm: &Wm, x: i32, y: i32) -> Px {
    let mut px = Px::Desktop;
    for w in wm.windows() {
        if let Some(part) = wm.metrics().part_at(&w.frame(), x, y) {
            px = Px::Window { id: w.id().get(), part, focused: wm.focused() == Some(w.id()) };
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
}

#[test]
fn repainting_only_the_damage_matches_repainting_everything() {
    let buttons = [L, R, ButtonSet::MIDDLE];
    for seed in [1, 0xdead_beef, 0x1234_5678_9abc_def1, 42] {
        let mut rng = Rng(seed);
        let mut wm = wm();
        let mut o = Owners::default();
        let mut screen = full(&wm);
        let mut held = NONE;
        let mut ids: Vec<WindowId> = Vec::new();
        for step in 0..4000 {
            let ids_now: Vec<WindowId> = wm.windows().iter().map(|w| w.id()).collect();
            let out = match rng.range(0, 14) {
                0 | 1 if ids_now.len() < 6 => {
                    let (x, y) = (rng.range(-10, W as i32 + 5), rng.range(-10, H as i32 + 5));
                    let (id, out) = wm.open("w", rng.range(0, 20) as u32, rng.range(0, 14) as u32, x, y);
                    ids.push(id);
                    out
                }
                2 => match rng.pick(&ids) {
                    // Sometimes an id already closed.
                    Some(id) => wm.close(id),
                    None => Outcome::default(),
                },
                3 => rng.pick(&ids_now).map_or_else(Outcome::default, |id| wm.focus(id)),
                4 => rng.pick(&ids_now).map_or_else(Outcome::default, |id| {
                    wm.move_to(id, rng.range(-50, W as i32 + 50), rng.range(-50, H as i32 + 50))
                }),
                5 => wm.interrupt(if rng.next().is_multiple_of(2) { Interruption::Pause } else { Interruption::Gap }),
                6 => wm.resume(),
                _ => {
                    if rng.next().is_multiple_of(3) {
                        let b = rng.pick(&buttons).expect("buttons");
                        held = if held.contains(b) { held.without(b) } else { held.with(b) };
                    }
                    // Aim at a window's parts often, so drags and close
                    // boxes happen.
                    let (x, y) = match rng.pick(&ids_now).filter(|_| rng.next().is_multiple_of(2)) {
                        Some(id) => {
                            let f = frame(&wm, id);
                            let t = wm.metrics().title_bar(&f);
                            if rng.next().is_multiple_of(2) {
                                (t.x() + rng.range(0, t.w().max(1) as i32), t.y() + rng.range(0, t.h().max(1) as i32))
                            } else {
                                (f.x() + rng.range(0, f.w().max(1) as i32), f.y() + rng.range(0, f.h().max(1) as i32))
                            }
                        }
                        None => (rng.range(-8, W as i32 + 8), rng.range(-8, H as i32 + 8)),
                    };
                    let p = Pointer { after_overrun: rng.next().is_multiple_of(40), ..at(x, y, held) };
                    wm.pointer(p)
                }
            };
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
            assert_eq!(screen, full(&wm), "seed {seed:#x} step {step}: damage missed a change");
            // Exactly one window is focused (the one in front), or none.
            assert_eq!(wm.focused(), wm.windows().last().map(|w| w.id()), "seed {seed:#x} step {step}");
            // Hit testing agrees with the picture.
            for _ in 0..4 {
                let (x, y) = (rng.range(0, W as i32), rng.range(0, H as i32));
                let want = match painted(&wm, x, y) {
                    Px::Desktop => Hit::Desktop,
                    Px::Window { id, part, .. } => {
                        Hit::Window(wm.windows().iter().find(|w| w.id().get() == id).expect("painted").id(), part)
                    }
                };
                assert_eq!(wm.hit(x, y), want, "seed {seed:#x} step {step} ({x}, {y})");
            }
            // The owners agree with the window manager: only the grabbing
            // window thinks buttons are held, and only buttons held.
            o.deliver(&out);
            o.read();
            o.keep(&wm);
            for (w, b) in o.holding() {
                assert_eq!(wm.grabbing().map(WindowId::get), Some(w), "seed {seed:#x} step {step}: {w} holds {b:?}");
                assert!(wm.buttons().contains(b), "seed {seed:#x} step {step}: {w} holds {b:?} of {:?}", wm.buttons());
            }
        }
    }
}
