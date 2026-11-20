//! A window's app ([`App`]: its tree, what it does with what the tree
//! reports, and what it draws itself over it), running a request on it
//! ([`serve`]) and painting what changed ([`repaint`]). The kernel's
//! `ui::Toolkit` and an app's (the SDK's `ui::Toolkit`) drive all three;
//! host tests run the same code.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_gfx::{Damage, Rect, Surface};

/// The most rectangles one [`repaint`] paints and commits apart (more are
/// merged into their bounding box).
pub const REPAINT_RECTS: usize = 16;

use crate::node::{Action, AutomationNode, InvokeError};
use crate::text::Text;
use crate::ui::{Palette, Repaint, Ui, UiEvent};

/// What a window does after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Then {
    /// Carry on (repaint if its tree changed).
    Carry,
    /// Close the window (its owner drops the handle).
    Close,
    /// Carry on, but what the event asked for failed, and why (#157: a
    /// launcher item whose window didn't open): a request that reached
    /// it answers [`Served::Failed`].
    Failed(String),
}

/// What an app's own drawing is handed ([`App::paint_over`]): the surface,
/// the window's text and palette (the theme's colours, so custom drawing
/// follows a switch), and the rectangles being repainted now (the whole
/// surface on a full repaint), each already painted with the tree's paint
/// (its background and the widgets reaching it). Only pixels inside `rects`
/// are committed now; what is drawn outside them shows only at a later
/// commit that covers it, and text drawn there through the host is still
/// charged by the character.
pub struct Over<'a> {
    pub surface: &'a mut Surface,
    pub text: &'a mut dyn Text,
    pub palette: &'a Palette,
    pub rects: &'a [Rect],
}

impl Over<'_> {
    /// Whether `r` meets a rectangle being repainted.
    pub fn touches(&self, r: Rect) -> bool {
        self.rects.iter().any(|d| d.intersect(&r).is_some())
    }
}

/// A window's app.
pub trait App {
    fn ui_mut(&mut self) -> &mut Ui;
    /// Act on what the tree reported.
    fn apply(&mut self, events: &[UiEvent]) -> Then;
    /// Drawn over the painted tree in `over.rects` (an animation's frame,
    /// a board, a buffer's text); nothing by default. Each rectangle has
    /// the tree's paint already (the background, outside the tree's area
    /// too), so only the foreground is drawn: Life's cells, a clock's
    /// hands.
    fn paint_over(&mut self, _over: &mut Over<'_>) {}
    /// The rectangles `paint_over` will now paint differently from what is
    /// shown (surface coordinates), since the last repaint: [`repaint`]
    /// repaints and commits them with the tree's damage, without the tree
    /// counting as changed (so it isn't republished). None by default.
    fn take_drawn(&mut self) -> Vec<Rect> {
        Vec::new()
    }
}

/// What running a request did: the answer its asker gets, once the owner
/// has published its tree (so `ui tree` shows the effect) or, for
/// [`Served::ClosedItsWindow`], once the window is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Served {
    /// Done: the node as it is after the action (client coordinates).
    Row { depth: u32, node: AutomationNode },
    /// Refused: it changed nothing.
    Error(InvokeError),
    /// Done, and the node is gone after it (it ran: a retry would act
    /// twice).
    RanNodeGone,
    /// Done, and the action closed its window (a Quit button).
    ClosedItsWindow,
    /// The node took the action, but what it was for failed, and why
    /// ([`Then::Failed`]).
    Failed(String),
}

/// Run `action` on node `node` of `app`'s tree, through the handler a
/// click reaches, then let the app act on what happened; the tree laid out
/// again in `area` for the node's row.
pub fn serve(app: &mut dyn App, text: &mut dyn Text, area: Rect, node: u64, action: &Action) -> Served {
    let events = match app.ui_mut().invoke(node, action) {
        Ok(events) => events,
        Err(e) => return Served::Error(e),
    };
    let then = app.apply(&events);
    app.ui_mut().layout(area, text);
    match then {
        Then::Close => return Served::ClosedItsWindow,
        Then::Failed(why) => return Served::Failed(why),
        Then::Carry => {}
    }
    match app.ui_mut().node(node) {
        Some((depth, row)) => Served::Row { depth, node: row },
        None => Served::RanNodeGone,
    }
}

/// What [`repaint`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repainted {
    /// The rectangles painted, to commit (none: nothing to show).
    pub rects: Vec<Rect>,
    /// Whether the tree changed since it was last published.
    pub changed: bool,
}

/// Lay `app`'s tree out in `area`, then paint only what changed into
/// `surface`: the widgets whose look changed (or everything after a move or
/// a new palette) and what the app's own drawing changed
/// ([`App::take_drawn`]). Every rectangle repainted gets the tree's paint
/// first (its background, and the widgets reaching it), inside the tree's
/// area or not, then the app's frame over it ([`App::paint_over`], called
/// whenever anything is repainted): a full repaint and a partial one leave
/// the same pixels, and the app paints only its foreground. `changed` is
/// the tree's alone: the app's drawing never makes it publish.
pub fn repaint(
    app: &mut dyn App,
    surface: &mut Surface,
    text: &mut dyn Text,
    palette: &Palette,
    area: Rect,
) -> Repainted {
    app.ui_mut().layout(area, text);
    let changed = app.ui_mut().take_changed();
    let bounds = surface.bounds();
    let drawn: Vec<Rect> = app.take_drawn().iter().filter_map(|r| r.intersect(&bounds)).collect();
    let rects = match app.ui_mut().take_damage() {
        // A full paint covers the app's rectangles too.
        Repaint::All => alloc::vec![bounds],
        // The tree's and the app's rectangles merged where that covers no
        // more pixels (a commit pays by the pixel, so an overlap isn't
        // painted or paid twice), a bounding box past REPAINT_RECTS.
        damage => {
            let mut merged = Damage::<REPAINT_RECTS>::new();
            if let Repaint::Rects(tree) = damage {
                tree.into_iter().for_each(|r| merged.add(r));
            }
            drawn.into_iter().for_each(|r| merged.add(r));
            merged.rects().to_vec()
        }
    };
    if rects.is_empty() {
        return Repainted { rects, changed };
    }
    for r in &rects {
        app.ui_mut().paint_in(surface, text, palette, *r);
    }
    app.paint_over(&mut Over { surface, text, palette, rects: &rects });
    Repainted { rects, changed }
}
