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
//! - Its loops: [`Toolkit::run`], [`Toolkit::run_with`] (the app takes some
//!   input itself first: [`Taken`]) and [`Toolkit::run_until`] (an animated
//!   app's ticks, [`Ticking`], paced by `lumen_ui::Pacer`). Whatever the
//!   app takes, a `Request`, `Close` or `Theme` always reaches the toolkit,
//!   and the focus and `Lost` reach it too after the app (`Event::offer`),
//!   so a request is always answered, a close always ends the loop and a
//!   press is always cancelled when the focus goes.
//! - An app draws its own area (beside the tree's: [`Toolkit::with_area`])
//!   in `lumen_ui::App::paint_over`, with the theme's palette and the
//!   host's text, and reports what it changed with `App::take_drawn`: that
//!   is repainted and committed without republishing an unchanged tree.

use crate::{Desktop, Error, Event, Handle, Result, Time, Window, check, sys};
use lumen_app_api::answer::AppAnswer;
use lumen_app_api::event::{Button, Offer, toolkit_key};
use lumen_app_api::layout::{Rect as WireRect, TextDraw};
use lumen_app_api::limits::{TEXT_BYTES, UI_TEXT_PX};
use lumen_app_api::tree::Tree;
use lumen_gfx::{Rect, Surface};
use lumen_ui::{App, Input, Pace, Pacer, Palette, Served, Text, Then, repaint, serve};
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

/// What an app's `take` did with an input event offered to it
/// ([`Toolkit::run_with`], [`Toolkit::run_until`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// Not the app's: the tree gets it.
    Pass,
    /// The app acted on it itself (the tree never sees it); the window is
    /// refreshed.
    Took,
    /// Stop: the loop returns [`Ended::Quit`]; the window closes when the
    /// caller drops the `Toolkit`, which owns the `Window`.
    Quit,
}

/// How a loop ended ([`Toolkit::run_with`], [`Toolkit::run_until`]). The
/// window is still open either way: it closes when the `Toolkit` (which
/// owns the `Window`) is dropped, and a request still queued then is
/// answered "window closed".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The window's `Close`, or an action that closed it.
    Closed,
    /// The app said [`Taken::Quit`] or [`Flow::Quit`].
    Quit,
}

/// What a tick said ([`Ticking::tick`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Carry,
    /// Stop, as [`Taken::Quit`] does.
    Quit,
}

/// An app that changes on its own, tick by tick ([`Toolkit::run_until`]).
pub trait Ticking: App {
    /// Nanoseconds between ticks now (at least `lumen_ui::MIN_PERIOD_NS`,
    /// 10 ms: a shorter one is taken as that), or `None` while nothing
    /// moves (the loop then waits for events only, so the desktop can
    /// settle: a stopped animation, a step mode, a frozen view). Asked
    /// before every wait, so a button that starts or stops the animation
    /// takes effect at once; a shorter period takes effect at once, a
    /// longer one after the next tick, and one that changes on every ask
    /// (a clock's "until the next second") still ticks at its boundary.
    fn period(&self) -> Option<u64>;

    /// Whether the waits between ticks are an animation's (the desktop
    /// never counts the window idle meanwhile). Yes by default; a clock
    /// that ticks once a second may say no.
    fn animating(&self) -> bool {
        true
    }

    /// A tick that is due (`now` on `Time::now_ns`'s clock, at or past its
    /// deadline; one tick however late). What it changes is shown by the
    /// refresh after it: the tree's changes through the tree, its own
    /// drawing through `take_drawn`.
    fn tick(&mut self, now: u64) -> Flow;
}

/// An app's toolkit in its window: see the module's doc.
pub struct Toolkit<A: App> {
    app: A,
    /// Where the tree is laid out (the rest of the client is the app's).
    area: Rect,
    text: HostText,
    /// From the last `Theme` event (the first event, always); all zero
    /// before it.
    palette: Palette,
    /// Whether a `Theme` has come (through any loop or [`Toolkit::step`]):
    /// [`Toolkit::run_until`] ticks only after it, since nothing is drawn
    /// until then.
    themed: bool,
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
        let area = window.canvas.bounds();
        Toolkit::with_area(app, window, area)
    }

    /// `app` in `window` with its tree laid out in `area` (cut to the
    /// client area; `BadArgument` if none of it is on the client, the
    /// window then dropped); the rest of the client is the app's own (a
    /// board, a clock face, a buffer's text): every rect repainted there
    /// gets the tree's background first, then the app's `paint_over` draws
    /// its foreground.
    pub fn with_area(app: A, window: Window, area: Rect) -> Result<Toolkit<A>> {
        let area = area.intersect(&window.canvas.bounds()).ok_or(Error::BadArgument)?;
        let text = HostText::new(window.desktop())?;
        let palette = Palette::from(lumen_app_api::event::Palette::default());
        Ok(Toolkit { app, area, text, palette, themed: false, window, unpublished: None, warned: false })
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }

    /// Its window (its events: an app may take some itself before handing
    /// the rest to [`Toolkit::step`]).
    pub fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }

    /// The palette the window is drawn with now (the last `Theme`'s; the
    /// light one before the first). `paint_over` is handed it too.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Where the tree is laid out.
    pub fn area(&self) -> Rect {
        self.area
    }

    /// Steps until the window closes (it can't quit: nothing takes an
    /// event).
    pub fn run(&mut self) -> Result<()> {
        self.run_with(|_, _| Taken::Pass).map(|_| ())
    }

    /// [`Toolkit::run`], each input event offered to `take` first
    /// (`Event::offer`): one it takes ([`Taken::Took`]: the app acted on it
    /// itself, such as the calculator's keys whichever button has the focus,
    /// or a game's arrows) skips the tree, then the window is refreshed. The
    /// focus coming or going and `Lost` are offered too, then always handed
    /// to the tree; a request, `Close` or `Theme` never reaches `take`.
    /// [`Taken::Quit`] returns [`Ended::Quit`], a close [`Ended::Closed`].
    pub fn run_with(&mut self, mut take: impl FnMut(&mut A, &Event) -> Taken) -> Result<Ended> {
        loop {
            let e = self.window.wait_event()?;
            if let Some(ended) = self.offer(e, &mut take)? {
                return Ok(ended);
            }
        }
    }

    /// [`Toolkit::run_with`] for an animated app: each turn, every event
    /// waiting is handled first (as `run_with` does), then, once the first
    /// `Theme` has come, a tick that is due ([`Ticking::period`] asked
    /// before every wait, paced by `lumen_ui::Pacer`: at least a period
    /// apart, at least `MIN_PERIOD_NS`, one tick however late) is taken and
    /// the window refreshed; otherwise it waits for an event until the next
    /// deadline (none without a period, so the desktop can settle). `time`
    /// is the clock the deadlines are on. A tick saying [`Flow::Quit`]
    /// returns [`Ended::Quit`].
    pub fn run_until(&mut self, time: Time, mut take: impl FnMut(&mut A, &Event) -> Taken) -> Result<Ended>
    where
        A: Ticking,
    {
        let mut pacer = Pacer::new();
        loop {
            while let Some(e) = self.window.next_event()? {
                if let Some(ended) = self.offer(e, &mut take)? {
                    return Ok(ended);
                }
            }
            // No ticks before the first `Theme`: nothing is drawn until then.
            let period = if self.themed { self.app.period() } else { None };
            match pacer.poll(period, time.now_ns()?) {
                Pace::Due => {
                    if self.app.tick(time.now_ns()?) == Flow::Quit {
                        return Ok(Ended::Quit);
                    }
                    self.refresh()?;
                    pacer.ticked(time.now_ns()?);
                }
                Pace::Until(deadline) => {
                    self.window.wait(Some(deadline), self.app.animating())?;
                }
                Pace::Idle => {
                    self.window.wait(None, false)?;
                }
            }
        }
    }

    /// One event of a loop, by who it is for (`Event::offer`): input
    /// offered to `take`, stepped unless it took it; the focus or `Lost`
    /// offered to `take`, then always stepped (unless it quit); a request,
    /// `Close` or `Theme` stepped.
    fn offer(&mut self, e: Event, take: &mut impl FnMut(&mut A, &Event) -> Taken) -> Result<Option<Ended>> {
        match e.offer() {
            Offer::Toolkit => {}
            Offer::App => match take(&mut self.app, &e) {
                Taken::Pass => {}
                Taken::Took => {
                    self.refresh()?;
                    return Ok(None);
                }
                Taken::Quit => return Ok(Some(Ended::Quit)),
            },
            Offer::Notify => {
                if take(&mut self.app, &e) == Taken::Quit {
                    return Ok(Some(Ended::Quit));
                }
            }
        }
        Ok((self.step(e)? == Step::Close).then_some(Ended::Closed))
    }

    /// One step: hand `e` to the tree; then, if it changed, repaint, commit
    /// and publish; then answer a request it ran.
    pub fn step(&mut self, e: Event) -> Result<Step> {
        let then = match e {
            Event::Close => return Ok(Step::Close),
            Event::Request { node, action } => return self.request(node, action.into()),
            Event::Theme { palette, .. } => {
                self.palette = palette.into();
                self.themed = true;
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
        let served = serve(&mut self.app, &mut self.text, self.area, node, &action);
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
    /// (`lumen_ui::repaint`, as the kernel's toolkit does): the tree's
    /// damage and the app's own (`App::take_drawn`), with `paint_over` over
    /// it; then publish the tree only if it changed (the app's drawing alone
    /// never republishes it). This is the one frame path: a tick's, an
    /// event's, or an app's own call after it drew. A failed commit marks
    /// everything to repaint again, so the next refresh repaints and
    /// republishes it; a refused publish is logged once a run and tried
    /// again when the tree changes.
    pub fn refresh(&mut self) -> Result<()> {
        let done = repaint(&mut self.app, &mut self.window.canvas, &mut self.text, &self.palette, self.area);
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
