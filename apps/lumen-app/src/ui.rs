//! `lumen-ui` inside an app (#207; ARCHITECTURE section 8, "Apps'
//! automation trees"): the kernel's toolkit, run in the module, so an app's
//! window looks, takes input and shows in the automation tree like a kernel
//! window.
//!
//! - [`HostText`]: `lumen_ui::Text` over the host's three text calls, at
//!   the kernel windows' size (`UI_TEXT_PX`): the font stays the kernel's.
//! - [`Toolkit`]: an app's `lumen_ui::App` in its window, one step per
//!   event as the kernel's `ui::Toolkit` takes them: the event handed to the
//!   tree, then (if it changed) repainted, committed and published, and only
//!   then a request answered, so once `ui invoke` returns, `ui tree` shows
//!   its effect. A request runs through `lumen_ui::serve`, the handler a
//!   click reaches; what it said goes back as an `AppAnswer` (the kernel
//!   builds the row from the published tree).

use crate::{Desktop, Event, Handle, Result, Window, check, sys};
use lumen_app_api::answer::AppAnswer;
use lumen_app_api::event::{Button, toolkit_key};
use lumen_app_api::layout::{Rect as WireRect, TextDraw};
use lumen_app_api::limits::{TEXT_BYTES, UI_TEXT_PX};
use lumen_app_api::tree::Tree;
use lumen_gfx::{Rect, Surface};
use lumen_ui::{App, Input, Palette, Served, Text, Then, repaint, serve};
use sys::ptr;

/// The toolkit's text through the host: the kernel's UI font at
/// `UI_TEXT_PX`, measured and drawn by the host's text calls (each text cut
/// to `TEXT_BYTES` first: a widget's text past it is measured and drawn
/// only that far).
pub struct HostText {
    desktop: Handle,
    line: u32,
    /// Whether a refused text call was logged (once: the toolkit has no
    /// way to report it, and a WARN a frame would flood the log).
    warned: bool,
}

impl HostText {
    pub fn new(desktop: Desktop) -> Result<HostText> {
        let line = desktop.line_height(UI_TEXT_PX)?;
        Ok(HostText { desktop: desktop.0, line, warned: false })
    }

    /// A text call's result: its value, or (refused: a bug, the text is cut
    /// to its cap first) 0, the first refusal logged.
    fn checked(&mut self, call: &str, r: i32) -> u32 {
        match check(r) {
            Ok(v) => v,
            Err(e) => {
                if !self.warned {
                    self.warned = true;
                    crate::log::warn(&alloc::format!("ui: {call} refused: {e}"));
                }
                debug_assert!(false, "a text call refused");
                0
            }
        }
    }
}

impl Text for HostText {
    fn line_height(&self) -> u32 {
        self.line
    }

    fn width(&mut self, text: &str) -> u32 {
        let text = lumen_app_api::text::prefix(text, TEXT_BYTES);
        let r = sys::text_measure(self.desktop.raw(), UI_TEXT_PX as i32, ptr(text.as_ptr()), text.len() as i32);
        self.checked("text_measure", r)
    }

    fn draw(&mut self, surface: &mut Surface, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        let text = lumen_app_api::text::prefix(text, TEXT_BYTES);
        let args = TextDraw { x, y, px: UI_TEXT_PX, rgb: color, clip: clip.into() }.encode();
        let canvas = crate::canvas_of_mut(surface);
        // SAFETY: the canvas's pixels are `surface`'s, borrowed mutably and
        // ours to write (the host writes only inside `clip`, cut to the
        // canvas). A refusal draws nothing (logged once).
        let r = unsafe {
            sys::text_draw(
                self.desktop.raw(),
                ptr(canvas.as_ptr()),
                ptr(args.as_ptr()),
                ptr(text.as_ptr()),
                text.len() as i32,
            )
        };
        self.checked("text_draw", r);
    }
}

/// What a step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Carry,
    /// The window is closing (its `Close`, or an action that closed it,
    /// whose request the kernel answers once the window is gone): return.
    Close,
}

/// An app's toolkit in its window: see the module's doc.
pub struct Toolkit<A: App> {
    app: A,
    text: HostText,
    /// From the last `Theme` event (the first event, always).
    palette: Palette,
    window: Window,
    /// Why the last publish was refused, while the tree published is stale
    /// (the kernel keeps the last one): a request is then answered failed,
    /// since its row would come from that stale tree. Tried again when the
    /// tree next changes.
    unpublished: Option<crate::PublishError>,
    /// Whether this run of refusals was logged (once a run: a publish that
    /// goes through re-arms it).
    warned: bool,
}

impl<A: App> Toolkit<A> {
    /// `app` in `window`, laid out in the whole client area; drawn once its
    /// first event, `Theme`, says how.
    pub fn new(app: A, window: Window) -> Result<Toolkit<A>> {
        let text = HostText::new(window.desktop())?;
        let palette = Palette::from(lumen_app_api::event::Palette::default());
        Ok(Toolkit { app, text, palette, window, unpublished: None, warned: false })
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }

    /// Its window (its events: an app may take some itself before handing
    /// the rest to [`Toolkit::step`]).
    pub fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }

    /// Steps until the window closes.
    pub fn run(&mut self) -> Result<()> {
        self.run_with(|_, _| false)
    }

    /// [`Toolkit::run`], each input event (not a request, `Close` or
    /// `Theme`: those are the toolkit's) offered to `take` first: one it
    /// takes (`true`: the app acted on it itself, such as the calculator's
    /// keys whichever button has the focus) skips the tree, then the window
    /// is refreshed.
    pub fn run_with(&mut self, mut take: impl FnMut(&mut A, &Event) -> bool) -> Result<()> {
        loop {
            let e = self.window.wait_event()?;
            let offered = !matches!(e, Event::Request { .. } | Event::Close | Event::Theme { .. });
            if offered && take(&mut self.app, &e) {
                self.refresh()?;
            } else if self.step(e)? == Step::Close {
                return Ok(());
            }
        }
    }

    /// One step: hand `e` to the tree; then, if it changed, repaint, commit
    /// and publish; then answer a request it ran.
    pub fn step(&mut self, e: Event) -> Result<Step> {
        let then = match e {
            Event::Close => return Ok(Step::Close),
            Event::Request { node, action } => return self.request(node, action.into()),
            Event::Theme { palette, .. } => {
                self.palette = palette.into();
                self.app.ui_mut().invalidate();
                Then::Carry
            }
            Event::FocusGained => {
                self.app.ui_mut().set_window_focused(true);
                Then::Carry
            }
            Event::FocusLost => {
                self.app.ui_mut().set_window_focused(false);
                self.input(Input::Cancel)
            }
            Event::Lost => self.input(Input::Cancel),
            Event::PointerDown { x, y, button: Button::Left, .. } => self.input(Input::Down { x, y }),
            Event::PointerUp { x, y, button: Button::Left, .. } => self.input(Input::Up { x, y }),
            Event::Key { code, mods, action } => match toolkit_key(code, mods, action) {
                Some(k) => self.input(Input::Key(k)),
                None => Then::Carry,
            },
            Event::PointerDown { .. } | Event::PointerUp { .. } | Event::PointerMove { .. } => Then::Carry,
        };
        if matches!(then, Then::Close) {
            return Ok(Step::Close);
        }
        self.refresh()?;
        Ok(Step::Carry)
    }

    /// A request: run it, publish what it changed, then answer (one that
    /// closed the window is answered at once: the kernel closes the window,
    /// then answers its asker).
    fn request(&mut self, node: u64, action: lumen_ui::Action) -> Result<Step> {
        let area = self.window.canvas.bounds();
        let served = serve(&mut self.app, &mut self.text, area, node, &action);
        let answer = AppAnswer::from_served(&served);
        if served == Served::ClosedItsWindow {
            self.window.answer(&answer)?;
            return Ok(Step::Close);
        }
        // Answered even if the refresh fails (its asker would wait for
        // nothing), then the failure is the step's; one whose effect isn't
        // published (the tree refused) is answered failed, not with a row
        // from the stale tree.
        if let Err(e) = self.refresh() {
            let why = lumen_app_api::answer::FailText::cut(&alloc::format!("couldn't refresh: {e}"));
            if let Err(lost) = self.window.answer(&AppAnswer::Failed(why)) {
                crate::log::error(&alloc::format!("ui: couldn't refresh: {e}"));
                return Err(lost);
            }
            return Err(e);
        }
        let answer = match &self.unpublished {
            Some(e) => {
                AppAnswer::Failed(lumen_app_api::answer::FailText::cut(&alloc::format!("couldn't publish: {e}")))
            }
            None => answer,
        };
        self.window.answer(&answer)?;
        Ok(Step::Carry)
    }

    fn input(&mut self, input: Input) -> Then {
        let events = self.app.ui_mut().handle(input);
        self.app.apply(&events)
    }

    /// Lay the tree out; repaint and commit only what changed
    /// (`lumen_ui::repaint`, as the kernel's toolkit does), then publish the
    /// tree if it changed. A failed commit marks the whole tree changed
    /// again, so the next refresh repaints and republishes it; a refused
    /// publish is logged once a run and tried again when the tree changes.
    pub fn refresh(&mut self) -> Result<()> {
        let area = self.window.canvas.bounds();
        let done = repaint(&mut self.app, &mut self.window.canvas, &mut self.text, &self.palette, area);
        if !done.rects.is_empty() {
            let wire: alloc::vec::Vec<WireRect> = done.rects.iter().map(|&r| r.into()).collect();
            if let Err(e) = self.window.commit(&wire) {
                self.app.ui_mut().invalidate();
                return Err(e);
            }
        }
        if done.changed {
            // (A refusal is kept in `unpublished` and logged: the window
            // goes on with the last tree published.)
            let _ = self.publish();
        }
        Ok(())
    }

    /// Publish the tree as it is now. A refusal (the tree too big, the host
    /// refusing it) is logged once a run of refusals and kept until a
    /// publish goes through; the kernel keeps the last tree meanwhile.
    pub fn publish(&mut self) -> core::result::Result<(), crate::PublishError> {
        let nodes = self.app.ui_mut().nodes();
        let result =
            Tree::from_automation(&nodes).map_err(crate::PublishError::Tree).and_then(|t| self.window.publish(&t));
        match &result {
            Ok(()) => {
                self.unpublished = None;
                self.warned = false;
            }
            Err(e) => {
                if !self.warned {
                    self.warned = true;
                    crate::log::warn(&alloc::format!("ui: a tree not published: {e}"));
                }
                self.unpublished = Some(e.clone());
            }
        }
        result
    }
}
