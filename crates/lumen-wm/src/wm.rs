//! Stacking, focus, hit testing, dragging and the implicit grab.
//!
//! [`Wm`] keeps the windows in stacking order (the last one in front),
//! which one is focused, and what the pointer is doing (a drag, a grab, a
//! press on the close box, or nothing). Every operation returns an
//! [`Outcome`]: the [`Damage`] it caused (the screen pixels whose picture
//! changed: a moved window's old and new frames, a raised window's frame,
//! the frames of the windows whose focus changed, since a focused window's
//! title bar is drawn differently) and the [`Delivery`]s for the windows
//! (pointer events in client coordinates, focus changes, close-box
//! clicks, losses).
//!
//! **Coordinates:** screen pixels as `i32` (a window may hang off the top
//! or left edge), as `lumen-gfx` uses. The tablet reports `usize`
//! positions ([`lumen_input::PointerState`]); [`Pointer::from_state`] is
//! the one place they are converted (saturating at `i32::MAX`, which no
//! screen reaches).

use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU32;

use lumen_gfx::{Damage, Rect};

use crate::geom::{Metrics, Part};

/// The damage one operation reports: a few frames at most, so 8
/// rectangles never collapse in practice.
pub type WmDamage = Damage<8>;

/// A window's id: from 1, never reused (in one [`Wm`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(NonZeroU32);

impl WindowId {
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// The pointer buttons held, one bit each as [`lumen_input::Buttons`]
/// names them (left, right, middle, side, extra).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ButtonSet(u8);

impl ButtonSet {
    pub const NONE: ButtonSet = ButtonSet(0);
    pub const LEFT: ButtonSet = ButtonSet(1 << 0);
    pub const RIGHT: ButtonSet = ButtonSet(1 << 1);
    pub const MIDDLE: ButtonSet = ButtonSet(1 << 2);
    pub const SIDE: ButtonSet = ButtonSet(1 << 3);
    pub const EXTRA: ButtonSet = ButtonSet(1 << 4);
    const ALL: [ButtonSet; 5] = [Self::LEFT, Self::RIGHT, Self::MIDDLE, Self::SIDE, Self::EXTRA];

    /// The set with these bits (bits past the fifth are dropped).
    pub const fn from_bits(bits: u8) -> ButtonSet {
        ButtonSet(bits & 0x1f)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: ButtonSet) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn with(self, other: ButtonSet) -> ButtonSet {
        ButtonSet(self.0 | other.0)
    }

    pub const fn without(self, other: ButtonSet) -> ButtonSet {
        ButtonSet(self.0 & !other.0)
    }

    /// Each button in the set on its own, left first.
    pub fn each(self) -> impl Iterator<Item = ButtonSet> {
        Self::ALL.into_iter().filter(move |b| self.contains(*b))
    }
}

impl From<lumen_input::Buttons> for ButtonSet {
    fn from(b: lumen_input::Buttons) -> ButtonSet {
        use lumen_input::Buttons as B;
        [B::LEFT, B::RIGHT, B::MIDDLE, B::SIDE, B::EXTRA]
            .into_iter()
            .zip(ButtonSet::ALL)
            .filter(|(input, _)| b.contains(*input))
            .fold(ButtonSet::NONE, |set, (_, ours)| set.with(ours))
    }
}

/// One pointer event as the window manager takes it: where the pointer is
/// and what is held after a whole frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    pub x: i32,
    pub y: i32,
    pub buttons: ButtonSet,
    /// Frames may have been lost at the device just before this one
    /// (`PointerEvent::after_overrun`, #151): handled as a gap
    /// ([`Wm::interrupt`] with [`Interruption::Gap`]) before this event.
    pub after_overrun: bool,
}

impl Pointer {
    /// The pointer at (`x`, `y`) holding `buttons`.
    pub const fn at(x: i32, y: i32, buttons: ButtonSet) -> Pointer {
        Pointer { x, y, buttons, after_overrun: false }
    }

    /// A tablet state as the window manager takes it: the `usize`
    /// position converted to `i32` (saturating), the buttons to a
    /// [`ButtonSet`].
    pub fn from_state(s: &lumen_input::PointerState, after_overrun: bool) -> Pointer {
        let c = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        Pointer { x: c(s.x), y: c(s.y), buttons: s.buttons.into(), after_overrun }
    }
}

/// What a window is told. Pointer positions are in its client area's
/// coordinates (negative or past its size while a grab follows the
/// pointer outside it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// `button` went down; `buttons` is everything held now.
    PointerDown {
        window: WindowId,
        x: i32,
        y: i32,
        button: ButtonSet,
        buttons: ButtonSet,
    },
    PointerMove {
        window: WindowId,
        x: i32,
        y: i32,
        buttons: ButtonSet,
    },
    /// `button` came up; `buttons` is what is still held.
    PointerUp {
        window: WindowId,
        x: i32,
        y: i32,
        button: ButtonSet,
        buttons: ButtonSet,
    },
    FocusGained(WindowId),
    FocusLost(WindowId),
    /// Events for it may have been lost: it treats every key and button
    /// as released.
    Lost(WindowId),
    /// Its close box was clicked (pressed and released on it). The window
    /// manager doesn't close it: its owner decides, and [`Wm::close`]
    /// removes it.
    Close(WindowId),
}

/// What an operation did: the damage to repaint and what to tell windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub damage: WmDamage,
    pub deliveries: Vec<Delivery>,
}

impl Outcome {
    fn merge(&mut self, other: Outcome) {
        for r in other.damage.rects() {
            self.damage.add(*r);
        }
        self.deliveries.extend(other.deliveries);
    }
}

/// Why pointer input was interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    /// The desktop was left (Alt+F1 to F4): input goes to a terminal
    /// until it is resumed ([`Wm::resume`]).
    Pause,
    /// Pointer events were lost: a gap in the reader's sequence, or one
    /// marked `after_overrun`.
    Gap,
}

/// What a point on the screen is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Desktop,
    Window(WindowId, Part),
}

/// A window as the window manager knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    id: WindowId,
    title: String,
    frame: Rect,
}

impl Window {
    pub fn id(&self) -> WindowId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn frame(&self) -> Rect {
        self.frame
    }
}

/// What the pointer is doing, from the first button pressed with none
/// held until every button is released again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    None,
    /// A left press on a title bar: moves move the window, keeping the
    /// pointer at (`dx`, `dy`) from the frame's corner, until the left
    /// button is released (other buttons held then are ignored until they
    /// are up too).
    Drag {
        id: WindowId,
        dx: i32,
        dy: i32,
    },
    /// A press on a client area: every button change and move goes to
    /// that window (the implicit grab), until the last button is released.
    /// It doesn't end at the first button's release: the window was told
    /// of every press since, so each release must reach it too, or it
    /// would hold a button for good.
    Grab {
        id: WindowId,
    },
    /// A left press on a close box: a release on it clicks it.
    CloseBox {
        id: WindowId,
    },
    /// A press on the desktop, a border, with another button on a title
    /// bar or close box, or anything after a pause or gap (a button may
    /// have gone down meanwhile): nothing until every button is up.
    Ignore,
}

/// Counts of things worth showing (`desktop status`, #153).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WmCounts {
    /// Grabs that ended because their window closed.
    pub grab_ends: u64,
}

/// The window manager's state for one screen.
#[derive(Clone, Debug)]
pub struct Wm {
    width: u32,
    height: u32,
    metrics: Metrics,
    /// Back to front.
    windows: Vec<Window>,
    next_id: u32,
    focused: Option<WindowId>,
    held: ButtonSet,
    last: (i32, i32),
    gesture: Gesture,
    counts: WmCounts,
}

impl Wm {
    /// No windows, on a `width` by `height` screen.
    pub fn new(width: u32, height: u32, metrics: Metrics) -> Wm {
        Wm {
            width,
            height,
            metrics,
            windows: Vec::new(),
            next_id: 1,
            focused: None,
            held: ButtonSet::NONE,
            last: (0, 0),
            gesture: Gesture::None,
            counts: WmCounts::default(),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn screen(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The windows, back to front.
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    pub fn counts(&self) -> WmCounts {
        self.counts
    }

    /// The buttons held, as of the last pointer event.
    pub fn buttons(&self) -> ButtonSet {
        self.held
    }

    /// The window being dragged, if any.
    pub fn dragging(&self) -> Option<WindowId> {
        match self.gesture {
            Gesture::Drag { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The window holding the implicit grab, if any.
    pub fn grabbing(&self) -> Option<WindowId> {
        match self.gesture {
            Gesture::Grab { id } => Some(id),
            _ => None,
        }
    }

    /// A window's client area on the screen.
    pub fn client(&self, id: WindowId) -> Option<Rect> {
        self.window(id).map(|w| self.metrics.client(&w.frame))
    }

    /// What is at (`x`, `y`), front to back.
    pub fn hit(&self, x: i32, y: i32) -> Hit {
        self.windows
            .iter()
            .rev()
            .find_map(|w| self.metrics.part_at(&w.frame, x, y).map(|p| Hit::Window(w.id, p)))
            .unwrap_or(Hit::Desktop)
    }

    /// Opens a window with a `client_w` by `client_h` client area, its
    /// frame's corner at (`x`, `y`) (moved as [`Wm::move_to`] would to keep
    /// its title bar reachable), in front and focused.
    pub fn open(&mut self, title: &str, client_w: u32, client_h: u32, x: i32, y: i32) -> (WindowId, Outcome) {
        let id = WindowId(NonZeroU32::new(self.next_id).expect("window ids start at 1"));
        self.next_id = self.next_id.checked_add(1).expect("fewer than 2^32 windows in one boot");
        let frame = self.metrics.frame_for(x, y, client_w, client_h);
        let (cx, cy) = self.clamp(&frame, x, y);
        let frame = self.metrics.frame_for(cx, cy, client_w, client_h);
        self.windows.push(Window { id, title: String::from(title), frame });
        let mut out = Outcome::default();
        out.damage.add(frame);
        self.set_focus(Some(id), &mut out);
        (id, out)
    }

    /// Removes a window. If it was focused, the window in front of the
    /// rest gets the focus; a grab or drag it held ends.
    pub fn close(&mut self, id: WindowId) -> Outcome {
        let mut out = Outcome::default();
        let Some(at) = self.windows.iter().position(|w| w.id == id) else {
            return out;
        };
        let w = self.windows.remove(at);
        out.damage.add(w.frame);
        match self.gesture {
            Gesture::Grab { id: g } if g == id => {
                self.counts.grab_ends += 1;
                self.gesture = Gesture::Ignore;
            }
            Gesture::Drag { id: g, .. } | Gesture::CloseBox { id: g } if g == id => self.gesture = Gesture::Ignore,
            _ => {}
        }
        if self.focused == Some(id) {
            self.focused = None;
            let next = self.windows.last().map(|w| w.id);
            self.set_focus(next, &mut out);
        }
        out
    }

    /// Raises a window to the front and focuses it (a click, the taskbar,
    /// `ui invoke <root> focus`).
    pub fn focus(&mut self, id: WindowId) -> Outcome {
        let mut out = Outcome::default();
        self.raise_and_focus(id, &mut out);
        out
    }

    /// Moves a window's frame corner to (`x`, `y`), clamped so the title
    /// bar stays reachable: its top row on the screen's rows that leave
    /// the whole title bar visible, and at least `strip` pixels of its
    /// width (or all of it, if narrower) on the screen.
    pub fn move_to(&mut self, id: WindowId, x: i32, y: i32) -> Outcome {
        let mut out = Outcome::default();
        let Some(at) = self.windows.iter().position(|w| w.id == id) else {
            return out;
        };
        let old = self.windows[at].frame;
        let (x, y) = self.clamp(&old, x, y);
        let new = Rect::new(x, y, old.w(), old.h());
        if new != old {
            self.windows[at].frame = new;
            out.damage.add(old);
            out.damage.add(new);
        }
        out
    }

    /// Input was interrupted (see [`Interruption`]): the focused window is
    /// told it lost the focus (a pause) or its events (a gap), a window
    /// holding the grab is told it lost its events, and the drag or grab
    /// ends. Until every button is up again nothing is delivered (a button
    /// pressed meanwhile, in a terminal or in the frames lost, isn't a
    /// press on a window).
    pub fn interrupt(&mut self, why: Interruption) -> Outcome {
        let mut out = Outcome::default();
        if let Some(f) = self.focused {
            out.deliveries.push(match why {
                Interruption::Pause => Delivery::FocusLost(f),
                Interruption::Gap => Delivery::Lost(f),
            });
        }
        // The focused window was told already (both mean "treat every key
        // and button as released").
        if let Gesture::Grab { id } = self.gesture
            && self.focused != Some(id)
        {
            out.deliveries.push(Delivery::Lost(id));
        }
        self.gesture = Gesture::Ignore;
        out
    }

    /// The desktop is shown again after a pause: the focused window gets
    /// its focus back.
    pub fn resume(&mut self) -> Outcome {
        let mut out = Outcome::default();
        if let Some(f) = self.focused {
            out.deliveries.push(Delivery::FocusGained(f));
        }
        out
    }

    /// One pointer event: the gesture in progress follows the move, then
    /// each newly pressed button (left first) is handled, then each
    /// released one.
    pub fn pointer(&mut self, p: Pointer) -> Outcome {
        let mut out = Outcome::default();
        if p.after_overrun {
            out.merge(self.interrupt(Interruption::Gap));
        }
        let pressed = p.buttons.without(self.held);
        let released = self.held.without(p.buttons);
        if (p.x, p.y) != self.last {
            match self.gesture {
                Gesture::Drag { id, dx, dy } => {
                    out.merge(self.move_to(id, p.x.saturating_sub(dx), p.y.saturating_sub(dy)));
                }
                Gesture::Grab { id } => {
                    if let Some((x, y)) = self.to_client(id, p.x, p.y) {
                        out.deliveries.push(Delivery::PointerMove { window: id, x, y, buttons: self.held });
                    }
                }
                _ => {}
            }
        }
        self.last = (p.x, p.y);
        for b in pressed.each() {
            self.held = self.held.with(b);
            match self.gesture {
                Gesture::None => self.start(b, p.x, p.y, &mut out),
                Gesture::Grab { id } => {
                    if let Some((x, y)) = self.to_client(id, p.x, p.y) {
                        out.deliveries.push(Delivery::PointerDown { window: id, x, y, button: b, buttons: self.held });
                    }
                }
                _ => {}
            }
        }
        for b in released.each() {
            self.held = self.held.without(b);
            match self.gesture {
                Gesture::Grab { id } => {
                    if let Some((x, y)) = self.to_client(id, p.x, p.y) {
                        out.deliveries.push(Delivery::PointerUp { window: id, x, y, button: b, buttons: self.held });
                    }
                }
                Gesture::Drag { .. } if b == ButtonSet::LEFT => self.gesture = Gesture::Ignore,
                Gesture::CloseBox { id } if b == ButtonSet::LEFT => {
                    if self.hit(p.x, p.y) == Hit::Window(id, Part::CloseBox) {
                        out.deliveries.push(Delivery::Close(id));
                    }
                    self.gesture = Gesture::Ignore;
                }
                _ => {}
            }
        }
        if self.held.is_empty() {
            self.gesture = Gesture::None;
        }
        out
    }

    /// The first button pressed with none held: what is under the pointer
    /// decides the gesture.
    fn start(&mut self, b: ButtonSet, x: i32, y: i32, out: &mut Outcome) {
        let Hit::Window(id, part) = self.hit(x, y) else {
            self.gesture = Gesture::Ignore;
            return;
        };
        self.raise_and_focus(id, out);
        self.gesture = match (part, b == ButtonSet::LEFT) {
            (Part::TitleBar, true) => {
                let f = self.window(id).expect("the window just hit").frame;
                Gesture::Drag { id, dx: x.saturating_sub(f.x()), dy: y.saturating_sub(f.y()) }
            }
            (Part::CloseBox, true) => Gesture::CloseBox { id },
            (Part::Client, _) => {
                if let Some((cx, cy)) = self.to_client(id, x, y) {
                    out.deliveries.push(Delivery::PointerDown {
                        window: id,
                        x: cx,
                        y: cy,
                        button: b,
                        buttons: self.held,
                    });
                }
                Gesture::Grab { id }
            }
            _ => Gesture::Ignore,
        };
    }

    fn raise_and_focus(&mut self, id: WindowId, out: &mut Outcome) {
        let Some(at) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        if at + 1 != self.windows.len() {
            // The focused window is always the one in front, so a raise
            // comes with a focus change, whose damage covers this frame
            // too; this keeps a raise correct on its own.
            let w = self.windows.remove(at);
            out.damage.add(w.frame);
            self.windows.push(w);
        }
        self.set_focus(Some(id), out);
    }

    fn set_focus(&mut self, to: Option<WindowId>, out: &mut Outcome) {
        if self.focused == to {
            return;
        }
        if let Some(old) = self.focused.and_then(|f| self.window(f)) {
            out.damage.add(old.frame);
            out.deliveries.push(Delivery::FocusLost(old.id));
        }
        self.focused = to;
        if let Some(new) = to.and_then(|f| self.window(f)) {
            out.damage.add(new.frame);
            out.deliveries.push(Delivery::FocusGained(new.id));
        }
    }

    /// A screen point in a window's client coordinates.
    fn to_client(&self, id: WindowId, x: i32, y: i32) -> Option<(i32, i32)> {
        let c = self.client(id)?;
        Some((x.saturating_sub(c.x()), y.saturating_sub(c.y())))
    }

    fn clamp(&self, frame: &Rect, x: i32, y: i32) -> (i32, i32) {
        let (w, sw, sh) = (i64::from(frame.w()), i64::from(self.width), i64::from(self.height));
        let strip = i64::from(self.metrics.strip).min(w);
        let x = i64::from(x).clamp(strip - w, (sw - strip).max(strip - w));
        let y = i64::from(y).clamp(0, (sh - i64::from(self.metrics.title_h)).max(0));
        let narrow = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        (narrow(x), narrow(y))
    }
}
