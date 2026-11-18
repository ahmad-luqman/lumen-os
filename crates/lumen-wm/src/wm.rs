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
//! **Coordinates:** screen pixels as `i32` (a window may hang off the
//! left, right or bottom edge, never the top), as `lumen-gfx` uses. The
//! tablet reports `usize` positions ([`lumen_input::PointerState`]);
//! [`Pointer::from_state`] is the one place they are converted
//! (saturating at `i32::MAX`, which no screen reaches).

use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU32;

use lumen_gfx::{Damage, Rect};
use lumen_input::{Button, Buttons};

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

    /// The first id a [`Wm`] gives, for tests that need one.
    #[cfg(test)]
    pub(crate) const fn first() -> WindowId {
        WindowId(NonZeroU32::MIN)
    }
}

/// One pointer event as the window manager takes it: where the pointer is
/// and what is held after a whole frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    pub x: i32,
    pub y: i32,
    pub buttons: Buttons,
    /// Frames may have been lost at the device just before this one
    /// (`PointerEvent::after_overrun`, #151): handled as a gap
    /// ([`Wm::interrupt`] with [`Interruption::Gap`]) before this event.
    pub after_overrun: bool,
}

impl Pointer {
    /// The pointer at (`x`, `y`) holding `buttons`.
    pub const fn at(x: i32, y: i32, buttons: Buttons) -> Pointer {
        Pointer { x, y, buttons, after_overrun: false }
    }

    /// A tablet state as the window manager takes it: the `usize`
    /// position converted to `i32` (saturating).
    pub fn from_state(s: &lumen_input::PointerState, after_overrun: bool) -> Pointer {
        let c = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        Pointer { x: c(s.x), y: c(s.y), buttons: s.buttons, after_overrun }
    }

    /// A frame as the pointer ring's reader got it (#151): its state, and a
    /// gap before it either way (`Read::gap`: this reader fell behind the
    /// ring, or frames were lost at the device), so the compositor never
    /// has to interrupt with [`Interruption::Gap`] apart for pointer input.
    pub fn from_read(r: &lumen_input::Read<lumen_input::PointerEvent>) -> Pointer {
        Pointer::from_state(&r.event.state, r.gap())
    }
}

/// What a window is told. Pointer positions are in its client area's
/// coordinates (negative or past its size while a grab follows the
/// pointer outside it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// `button` went down; `buttons` is everything held now (with it).
    PointerDown {
        window: WindowId,
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
    },
    PointerMove {
        window: WindowId,
        x: i32,
        y: i32,
        buttons: Buttons,
    },
    /// `button` came up; `buttons` is what is still held (without it).
    PointerUp {
        window: WindowId,
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
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

impl Delivery {
    /// The window it is for.
    pub fn window(&self) -> WindowId {
        match *self {
            Delivery::PointerDown { window, .. }
            | Delivery::PointerMove { window, .. }
            | Delivery::PointerUp { window, .. }
            | Delivery::FocusGained(window)
            | Delivery::FocusLost(window)
            | Delivery::Lost(window)
            | Delivery::Close(window) => window,
        }
    }
}

/// What an operation did: the damage to repaint and what to tell windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "its damage must be repainted and its deliveries queued"]
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
    /// Pointer events were lost: a gap in the pointer reader's sequence,
    /// or one marked `after_overrun`.
    Gap,
    /// Key events were lost: a gap in the key reader's sequence
    /// (`Read::lost_before`, #151), or a key event marked `after_drop`
    /// (bytes dropped before it). The focused window may hold a key
    /// whose release is gone.
    KeyGap,
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
    /// Off the screen (#157): not drawn, not hit, never focused; its place
    /// kept for [`Wm::restore`].
    minimized: bool,
    /// A panel (#157, the taskbar): frameless (its frame is its client
    /// area), in front of every other window, never focused or raised.
    panel: bool,
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

    pub fn is_minimized(&self) -> bool {
        self.minimized
    }

    pub fn is_panel(&self) -> bool {
        self.panel
    }

    /// Whether it is on the screen and takes the focus: neither a panel
    /// nor minimized.
    fn focusable(&self) -> bool {
        !self.panel && !self.minimized
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
    /// would hold a button for good. It ends early if the window loses the
    /// focus (told `FocusLost`, it treats every button as released) or
    /// closes.
    Grab {
        id: WindowId,
    },
    /// A left press on a close box: a left release on it clicks it (other
    /// buttons meanwhile are ignored).
    CloseBox {
        id: WindowId,
    },
    /// A left press on a minimize box (#157): a left release on it
    /// minimizes the window.
    MinimizeBox {
        id: WindowId,
    },
    /// Nothing until every button is up: a press on the desktop or a
    /// border (after it focused and raised the window); another button
    /// than the left on a title bar or close box; the buttons still held
    /// when a drag, a close-box press or a grab ended early.
    Ignore,
    /// Nothing until every button is up, for buttons held across a pause
    /// or a pointer gap, at a resume, or pressed in an `after_overrun`
    /// event: pressed in a terminal or in frames lost, not on a window.
    /// Presses meanwhile are counted (`ignored_presses`).
    Held,
}

/// Counts of things worth showing (`desktop status`, #153).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WmCounts {
    /// Grabs that ended before their last button came up (their window
    /// closed or lost the focus, or input was interrupted).
    pub grab_ends: u64,
    /// Presses not taken as presses on a window: made in an
    /// `after_overrun` event, or while buttons held across a pause or gap
    /// or at a resume were still down.
    pub ignored_presses: u64,
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
    held: Buttons,
    last: (i32, i32),
    gesture: Gesture,
    /// Between [`Wm::interrupt`] with a pause and [`Wm::resume`].
    paused: bool,
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
            held: Buttons::NONE,
            last: (0, 0),
            gesture: Gesture::None,
            paused: false,
            counts: WmCounts::default(),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The windows, back to front (the panels last; minimized ones in
    /// their place, though not shown).
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The id of the open window numbered `n` (a number typed in `ui
    /// invoke`, a `windows` row), or `None` if none is.
    pub fn id(&self, n: u32) -> Option<WindowId> {
        self.windows.iter().map(|w| w.id).find(|id| id.get() == n)
    }

    pub fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    pub fn counts(&self) -> WmCounts {
        self.counts
    }

    /// Between a pause and the resume.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// The buttons held, as of the last pointer event (or the resume).
    pub fn buttons(&self) -> Buttons {
        self.held
    }

    /// The window being dragged, if any.
    pub fn dragging(&self) -> Option<WindowId> {
        match self.gesture {
            Gesture::Drag { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The window holding the implicit grab, if any (the focused one, or
    /// a panel: a press on a panel grabs without focusing).
    pub fn grabbing(&self) -> Option<WindowId> {
        match self.gesture {
            Gesture::Grab { id } => Some(id),
            _ => None,
        }
    }

    /// No gesture: the next press starts one.
    pub fn is_idle(&self) -> bool {
        self.gesture == Gesture::None
    }

    /// A window's client area on the screen (a panel's is its frame).
    pub fn client(&self, id: WindowId) -> Option<Rect> {
        self.window(id).map(|w| if w.panel { w.frame } else { self.metrics.client(&w.frame) })
    }

    /// The part of window `w` at (`x`, `y`), or `None` outside it (or if
    /// it is minimized): what the picture shows there.
    fn part_at(&self, w: &Window, x: i32, y: i32) -> Option<Part> {
        if w.minimized {
            None
        } else if w.panel {
            w.frame.contains(x, y).then_some(Part::Client)
        } else {
            self.metrics.part_at(&w.frame, x, y)
        }
    }

    /// What is at (`x`, `y`), front to back.
    pub fn hit(&self, x: i32, y: i32) -> Hit {
        self.windows
            .iter()
            .rev()
            .find_map(|w| self.part_at(w, x, y).map(|p| Hit::Window(w.id, p)))
            .unwrap_or(Hit::Desktop)
    }

    /// The rows windows keep their title bar in: down to the highest
    /// panel's top (the panels' rows reserved), the whole screen with none.
    pub fn work_height(&self) -> u32 {
        self.windows
            .iter()
            .filter(|w| w.panel)
            .map(|w| w.frame.y().clamp(0, self.height as i32) as u32)
            .min()
            .unwrap_or(self.height)
    }

    /// The front window that takes the focus (neither a panel nor
    /// minimized).
    fn front(&self) -> Option<WindowId> {
        self.windows.iter().rev().find(|w| w.focusable()).map(|w| w.id)
    }

    /// Where a window that isn't a panel goes in front: just behind the
    /// panels.
    fn front_slot(&self) -> usize {
        self.windows.iter().position(|w| w.panel).unwrap_or(self.windows.len())
    }

    /// Opens a window with a `client_w` by `client_h` client area, its
    /// frame's corner at (`x`, `y`) (moved as [`Wm::move_to`] would to keep
    /// its title bar reachable), in front (behind the panels) and focused.
    pub fn open(&mut self, title: &str, client_w: u32, client_h: u32, x: i32, y: i32) -> (WindowId, Outcome) {
        let id = self.take_id();
        // Clamped by its whole width: at x near i32::MAX, Rect::new would
        // shorten the frame and the strip would land off the screen.
        let (w, _) = self.metrics.frame_size(client_w, client_h);
        let (cx, cy) = self.clamp(w, x, y);
        let frame = self.metrics.frame_for(cx, cy, client_w, client_h);
        let at = self.front_slot();
        self.windows.insert(at, Window { id, title: String::from(title), frame, minimized: false, panel: false });
        let mut out = Outcome::default();
        out.damage.add(frame);
        self.set_focus(Some(id), &mut out);
        (id, out)
    }

    /// The next window id (from 1, never reused).
    fn take_id(&mut self) -> WindowId {
        let id = WindowId(NonZeroU32::new(self.next_id).expect("window ids start at 1"));
        self.next_id = self.next_id.checked_add(1).expect("fewer than 2^32 windows in one boot");
        id
    }

    /// Opens a panel (#157, the taskbar) docked along the bottom of the
    /// screen, `rows` high and the screen's whole width (its frame is its
    /// client area: no title bar, no border): in front of every window,
    /// never focused, raised or moved. Its rows are reserved: every window
    /// is clamped again so its title bar stays above them (their damage is
    /// in the outcome), as later opens and moves are. Nothing is told (the
    /// focus doesn't move).
    pub fn open_panel(&mut self, title: &str, rows: u32) -> (WindowId, Outcome) {
        let id = self.take_id();
        let rows = rows.min(self.height);
        let frame = Rect::new(0, (self.height - rows) as i32, self.width, rows);
        self.windows.push(Window { id, title: String::from(title), frame, minimized: false, panel: true });
        let mut out = Outcome::default();
        out.damage.add(frame);
        let placed: Vec<(WindowId, Rect)> = self.windows.iter().map(|w| (w.id, w.frame)).collect();
        for (w, f) in placed {
            // (`move_to` refuses the panel itself.)
            out.merge(self.move_to(w, f.x(), f.y()).unwrap_or_default());
        }
        (id, out)
    }

    /// A drag, grab or box press on window `id` ends (it closed or was
    /// minimized).
    fn end_gesture_on(&mut self, id: WindowId) {
        match self.gesture {
            Gesture::Grab { id: g } if g == id => {
                self.counts.grab_ends += 1;
                self.gesture = Gesture::Ignore;
            }
            Gesture::Drag { id: g, .. } | Gesture::CloseBox { id: g } | Gesture::MinimizeBox { id: g } if g == id => {
                self.gesture = Gesture::Ignore
            }
            _ => {}
        }
    }

    /// Removes a window (`None` if it isn't open). If it was focused, the
    /// window in front of the rest gets the focus; a drag, grab or
    /// close-box press on it ends.
    #[must_use = "its damage must be repainted and its deliveries queued"]
    pub fn close(&mut self, id: WindowId) -> Option<Outcome> {
        let at = self.windows.iter().position(|w| w.id == id)?;
        let mut out = Outcome::default();
        let w = self.windows.remove(at);
        out.damage.add(w.frame);
        self.end_gesture_on(id);
        if self.focused == Some(id) {
            self.focused = None;
            let next = self.front();
            self.set_focus(next, &mut out);
        }
        Some(out)
    }

    /// Raises a window to the front and focuses it (a click, the taskbar,
    /// `ui invoke <root> focus`), restoring it if it is minimized; `None` if
    /// it isn't open, or is a panel.
    #[must_use = "its damage must be repainted and its deliveries queued"]
    pub fn focus(&mut self, id: WindowId) -> Option<Outcome> {
        let w = self.window(id)?;
        if w.panel {
            return None;
        }
        if w.minimized {
            return self.restore(id);
        }
        let mut out = Outcome::default();
        self.raise_and_focus(id, &mut out);
        Some(out)
    }

    /// Minimizes a window (#157): it leaves the screen (its place kept),
    /// a drag, grab or box press on it ends, and if it was focused the
    /// front window that isn't minimized gets the focus. `None` if it isn't
    /// open, is a panel, or is minimized already.
    #[must_use = "its damage must be repainted and its deliveries queued"]
    pub fn minimize(&mut self, id: WindowId) -> Option<Outcome> {
        let at = self.windows.iter().position(|w| w.id == id && w.focusable())?;
        let mut out = Outcome::default();
        self.windows[at].minimized = true;
        out.damage.add(self.windows[at].frame);
        self.end_gesture_on(id);
        if self.focused == Some(id) {
            let next = self.front();
            self.set_focus(next, &mut out);
        }
        Some(out)
    }

    /// Brings a minimized window back (#157), raised and focused. `None` if
    /// it isn't open or isn't minimized.
    #[must_use = "its damage must be repainted and its deliveries queued"]
    pub fn restore(&mut self, id: WindowId) -> Option<Outcome> {
        let at = self.windows.iter().position(|w| w.id == id && w.minimized)?;
        let mut out = Outcome::default();
        self.windows[at].minimized = false;
        out.damage.add(self.windows[at].frame);
        self.raise_and_focus(id, &mut out);
        Some(out)
    }

    /// Moves a window's frame corner to (`x`, `y`), clamped so the title
    /// bar stays reachable: its top row on the screen's rows that leave
    /// the whole title bar visible above the panels' rows
    /// ([`Wm::work_height`]), and at least `strip` pixels of its
    /// width (or all of it, if narrower) on the screen. `None` if it isn't
    /// open, or is a panel (docked).
    #[must_use = "its damage must be repainted and its deliveries queued"]
    pub fn move_to(&mut self, id: WindowId, x: i32, y: i32) -> Option<Outcome> {
        let at = self.windows.iter().position(|w| w.id == id && !w.panel)?;
        let mut out = Outcome::default();
        let old = self.windows[at].frame;
        let (x, y) = self.clamp(old.w(), x, y);
        let new = Rect::new(x, y, old.w(), old.h());
        if new != old {
            self.windows[at].frame = new;
            out.damage.add(old);
            out.damage.add(new);
        }
        Some(out)
    }

    /// Input was interrupted (see [`Interruption`]).
    ///
    /// - **A pause:** the focused window is told `FocusLost`, and the drag
    ///   or grab ends. Until [`Wm::resume`], pointer events are ignored
    ///   and focus changes (a window opened or closed meanwhile) aren't
    ///   delivered; the resume delivers the focus as it is then.
    /// - **A pointer gap:** the focused window (the grabbing one, if any)
    ///   is told `Lost`, and the drag or grab ends. If buttons were held,
    ///   nothing more is delivered until a pointer event reports every
    ///   button up (a press meanwhile, in the frames lost, wasn't seen);
    ///   if none were, the next press is a press as usual.
    /// - **A key gap:** the focused window is told `Lost` (it releases its
    ///   keys, and its buttons too); a grab on it ends (its buttons are
    ///   ignored until up), a title-bar drag goes on. A panel's grab goes
    ///   on too (a panel has no keys).
    ///
    /// A panel's grab (#157: a press on the taskbar, which isn't the
    /// focused window) ends on a pause or a pointer gap, the panel told
    /// `Lost`.
    ///
    /// While paused, nothing (a second pause does nothing either).
    pub fn interrupt(&mut self, why: Interruption) -> Outcome {
        let mut out = Outcome::default();
        if self.paused {
            return out;
        }
        out.deliveries.extend(self.focused.map(match why {
            Interruption::Pause => Delivery::FocusLost,
            Interruption::Gap | Interruption::KeyGap => Delivery::Lost,
        }));
        let grab = match self.gesture {
            Gesture::Grab { id } => Some(id),
            _ => None,
        };
        // A panel's grab (#157) isn't the focused window's. A key gap
        // leaves it alone (a panel has no keys); a pause or a pointer gap
        // ends it, the panel told it lost its buttons.
        let panel_grab = grab.filter(|&g| Some(g) != self.focused);
        if let Some(p) = panel_grab {
            if why == Interruption::KeyGap {
                return out;
            }
            out.deliveries.push(Delivery::Lost(p));
        }
        let grabbing = grab.is_some();
        match why {
            Interruption::Pause => {
                self.paused = true;
                self.gesture = Gesture::Held;
            }
            Interruption::Gap => self.ignore_held(),
            Interruption::KeyGap if grabbing => self.gesture = Gesture::Ignore,
            Interruption::KeyGap => {}
        }
        if grabbing {
            self.counts.grab_ends += 1;
        }
        out
    }

    /// The desktop is shown again after a pause. `baseline` is what the
    /// pointer ring's newest frame held at the cut (captured with the cut,
    /// as the ring may overwrite that frame): buttons pressed or released
    /// while the terminal had the input. Buttons held then are ignored
    /// until a pointer event reports every button up. The focused window,
    /// as it is now, is told `FocusGained`. Without a pause before, it
    /// does nothing.
    pub fn resume(&mut self, baseline: Buttons) -> Outcome {
        let mut out = Outcome::default();
        if !self.paused {
            return out;
        }
        self.paused = false;
        self.held = baseline;
        self.ignore_held();
        out.deliveries.extend(self.focused.map(Delivery::FocusGained));
        out
    }

    /// One pointer event: the gesture in progress follows the move, then
    /// each newly pressed button (left first) is handled, then each
    /// released one. While paused it is ignored.
    pub fn pointer(&mut self, p: Pointer) -> Outcome {
        let mut out = Outcome::default();
        if self.paused {
            return out;
        }
        if p.after_overrun {
            out.merge(self.interrupt(Interruption::Gap));
            // A button down in this event went down in the frames lost:
            // not a press on a window.
            if !p.buttons.without(self.held).is_empty() {
                self.gesture = Gesture::Held;
            }
        }
        let pressed = p.buttons.without(self.held);
        let released = self.held.without(p.buttons);
        if (p.x, p.y) != self.last {
            match self.gesture {
                Gesture::Drag { id, dx, dy } => {
                    let moved = self.move_to(id, p.x.saturating_sub(dx), p.y.saturating_sub(dy));
                    debug_assert!(moved.is_some(), "a drag of a window not open");
                    out.merge(moved.unwrap_or_default());
                }
                Gesture::Grab { id } => self.send(&mut out, id, p.x, p.y, Sent::Move),
                _ => {}
            }
        }
        self.last = (p.x, p.y);
        for b in pressed.each() {
            self.held = self.held.with(b.into());
            match self.gesture {
                Gesture::None => self.start(b, p.x, p.y, &mut out),
                Gesture::Grab { id } => self.send(&mut out, id, p.x, p.y, Sent::Down(b)),
                Gesture::Held => self.counts.ignored_presses += 1,
                _ => {}
            }
        }
        for b in released.each() {
            self.held = self.held.without(b.into());
            match self.gesture {
                Gesture::Grab { id } => self.send(&mut out, id, p.x, p.y, Sent::Up(b)),
                Gesture::Drag { .. } if b == Button::Left => self.gesture = Gesture::Ignore,
                Gesture::CloseBox { id } if b == Button::Left => {
                    if self.hit(p.x, p.y) == Hit::Window(id, Part::CloseBox) {
                        out.deliveries.push(Delivery::Close(id));
                    }
                    self.gesture = Gesture::Ignore;
                }
                Gesture::MinimizeBox { id } if b == Button::Left => {
                    self.gesture = Gesture::Ignore;
                    if self.hit(p.x, p.y) == Hit::Window(id, Part::MinimizeBox) {
                        out.merge(self.minimize(id).unwrap_or_default());
                    }
                }
                _ => {}
            }
        }
        if self.held.is_empty() {
            self.gesture = Gesture::None;
        }
        out
    }

    /// After a gap or at a resume: nothing until every button is up if any
    /// is held, else ready for the next press.
    fn ignore_held(&mut self) {
        self.gesture = if self.held.is_empty() { Gesture::None } else { Gesture::Held };
    }

    /// The first button pressed with none held: what is under the pointer
    /// decides the gesture.
    fn start(&mut self, b: Button, x: i32, y: i32, out: &mut Outcome) {
        let Hit::Window(id, part) = self.hit(x, y) else {
            self.gesture = Gesture::Ignore;
            return;
        };
        if self.window(id).is_some_and(|w| w.panel) {
            // A panel takes the press as a grab, the focus left where it
            // is (a taskbar button is about another window).
            self.send(out, id, x, y, Sent::Down(b));
            self.gesture = Gesture::Grab { id };
            return;
        }
        self.raise_and_focus(id, out);
        self.gesture = match (part, b == Button::Left) {
            (Part::TitleBar, true) => {
                let f = self.window(id).expect("the window just hit").frame;
                Gesture::Drag { id, dx: x.saturating_sub(f.x()), dy: y.saturating_sub(f.y()) }
            }
            (Part::CloseBox, true) => Gesture::CloseBox { id },
            (Part::MinimizeBox, true) => Gesture::MinimizeBox { id },
            (Part::Client, _) => {
                self.send(out, id, x, y, Sent::Down(b));
                Gesture::Grab { id }
            }
            _ => Gesture::Ignore,
        };
    }

    /// A pointer delivery for window `id`, at the screen point (`x`, `y`)
    /// in its client coordinates.
    fn send(&self, out: &mut Outcome, id: WindowId, x: i32, y: i32, what: Sent) {
        let Some(c) = self.client(id) else {
            debug_assert!(false, "a pointer delivery for a window not open");
            return;
        };
        let (x, y) = (x.saturating_sub(c.x()), y.saturating_sub(c.y()));
        let buttons = self.held;
        out.deliveries.push(match what {
            Sent::Down(button) => {
                debug_assert!(buttons.has(button), "{button:?} of {buttons:?}");
                Delivery::PointerDown { window: id, x, y, button, buttons }
            }
            Sent::Move => Delivery::PointerMove { window: id, x, y, buttons },
            Sent::Up(button) => {
                debug_assert!(!buttons.has(button), "{button:?} of {buttons:?}");
                Delivery::PointerUp { window: id, x, y, button, buttons }
            }
        });
    }

    fn raise_and_focus(&mut self, id: WindowId, out: &mut Outcome) {
        let Some(at) = self.windows.iter().position(|w| w.id == id && w.focusable()) else {
            debug_assert!(false, "raising a window not open, minimized or a panel");
            return;
        };
        if at + 1 != self.front_slot() {
            // The focused window is always the front one that isn't
            // minimized (behind the panels), so a raise comes with a focus
            // change, whose damage
            // covers this frame too; this keeps a raise correct on its own.
            let w = self.windows.remove(at);
            out.damage.add(w.frame);
            let slot = self.front_slot();
            self.windows.insert(slot, w);
        }
        self.set_focus(Some(id), out);
    }

    /// Moves the focus: both frames are damaged (a focused frame is drawn
    /// differently), and both windows told unless paused. A grab on the
    /// window losing it ends.
    fn set_focus(&mut self, to: Option<WindowId>, out: &mut Outcome) {
        if self.focused == to {
            return;
        }
        if let Some(old) = self.focused.and_then(|f| self.window(f)) {
            out.damage.add(old.frame);
            if !self.paused {
                out.deliveries.push(Delivery::FocusLost(old.id));
            }
        }
        if let Gesture::Grab { id } = self.gesture
            && Some(id) == self.focused
        {
            self.counts.grab_ends += 1;
            self.gesture = Gesture::Ignore;
        }
        self.focused = to;
        if let Some(new) = to.and_then(|f| self.window(f)) {
            out.damage.add(new.frame);
            if !self.paused {
                out.deliveries.push(Delivery::FocusGained(new.id));
            }
        }
    }

    fn clamp(&self, w: u32, x: i32, y: i32) -> (i32, i32) {
        let (w, sw) = (i64::from(w), i64::from(self.width));
        let strip = i64::from(self.metrics.strip).min(w);
        let x = i64::from(x).clamp(strip - w, (sw - strip).max(strip - w));
        let work = i64::from(self.work_height());
        let y = i64::from(y).clamp(0, (work - i64::from(self.metrics.title_h)).max(0));
        let narrow = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        (narrow(x), narrow(y))
    }
}

/// What [`Wm::send`] delivers.
#[derive(Clone, Copy)]
enum Sent {
    Down(Button),
    Move,
    Up(Button),
}
