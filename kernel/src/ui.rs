//! The desktop's toolkit in the kernel and the automation tree's commands
//! (#156; ARCHITECTURE section 11, "Toolkit and the automation tree"):
//!
//! - [`Toolkit`]: a `lumen_ui::App` in a window, one step per event: the
//!   event handed to the tree, then (if it changed) repainted, committed
//!   and published, and only then a request answered, so once `ui invoke`
//!   returns, `ui tree` shows its effect (and a window its action closed is
//!   gone). Text through `lumen-font` ([`UiText`]).
//! - [`Request`]: `ui invoke`'s request in a window's queue, with its reply
//!   slot (`lumen_ui::Reply` behind a lock, and the asker's wait queue).
//!   [`Request::start`] gives a [`Started`], the only thing that can answer
//!   it; dropped unanswered, either answers for itself.
//! - Counter, the demo app (`desktop counter`; its logic is
//!   `lumen_ui::counter`).
//! - `ui tree [window]` and `ui invoke <node> <action> [args]`.
//!
//! The rule: agents act only through this tree. `ui invoke` runs a widget's
//! own handler; it never fabricates mouse or keyboard input.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use lumen_font::{Font, Px};
use lumen_gfx::{Mask, Rect, Surface};
use lumen_input::{Button, KeyAction, KeyCode, KeyEvent};
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_ui::counter::Counter;
use lumen_ui::{
    Action, ActionKind, App, First, Input, InvokeError, Key, Palette, Phase, Reply, Second, Served, Text, Then, serve,
};
use lumen_wm::{Refused, Theme};

use crate::desktop::{self, NodeAt, TreeRow, WindowEvent, WindowHandle};
use crate::sched::WaitQueue;
use crate::shell::sysinfo::{int, text};
use crate::sync::SpinLock;

/// How long `ui invoke` waits for an answer, and again once the owner took
/// the request.
const DEADLINE_NS: u64 = 1_000_000_000;

// -- requests --------------------------------------------------------------------

/// What a request's asker is told.
#[derive(Debug)]
pub enum Answer {
    /// It ran (or was refused, changing nothing): what `serve` said.
    Ran(Served),
    /// Its window closed before it ran (or it was dropped unrun).
    WindowClosed,
    /// The owner dropped it while running it, unanswered (a bug).
    Dropped,
}

/// A request's reply slot, shared by its asker and the request: the reply
/// behind a leaf lock (taken under the wait queue's lock by the asker's
/// wake-up check, never held across a wake), and the queue its asker
/// waits on, woken after each answer.
pub struct ReplyCell {
    reply: SpinLock<Reply<Answer>>,
    done: WaitQueue,
}

impl ReplyCell {
    /// Where the reply is (for the ktests).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "for the ktests"))]
    pub fn phase(&self) -> Phase {
        self.reply.lock().phase()
    }

    /// The asker takes the answer (for the ktests).
    #[cfg(feature = "ktest")]
    pub fn take(&self) -> Option<Answer> {
        self.reply.lock().take()
    }

    /// Answer with `write` if it applies; wake the asker if it did.
    fn answer(&self, write: impl FnOnce(&mut Reply<Answer>) -> bool) {
        let wrote = write(&mut self.reply.lock());
        if wrote {
            self.done.wake_all();
        }
    }
}

/// `ui invoke`'s request in a window's queue: the node and the action, and
/// its reply. Not `Clone`; answers "window closed" if dropped before it
/// ran (so `ui invoke` always ends). Dropped only with no desktop lock
/// held (the queue hands refused and drained requests back).
pub struct Request {
    node: u64,
    action: Action,
    /// `None` once started.
    cell: Option<Arc<ReplyCell>>,
}

/// A request the owner took ([`Request::start`]): the only thing that can
/// answer it ([`Started::finish`]); dropped unanswered, it answers
/// "dropped while running".
pub struct Started {
    node: u64,
    action: Action,
    cell: Option<Arc<ReplyCell>>,
}

/// A request cancelled at its asker's deadline: the owner skips it.
#[derive(Debug)]
pub struct Skipped;

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request").field("node", &self.node).field("action", &self.action).finish_non_exhaustive()
    }
}

impl Request {
    /// A request for `action` on node `node`, and its reply's cell (the
    /// asker's side).
    pub(crate) fn new(node: u64, action: Action) -> (Request, Arc<ReplyCell>) {
        let cell = Arc::new(ReplyCell { reply: SpinLock::new(Reply::new()), done: WaitQueue::new() });
        (Request { node, action, cell: Some(cell.clone()) }, cell)
    }

    /// The owner takes it: a [`Started`] to answer, or [`Skipped`] if it
    /// was cancelled at the asker's deadline (`cancelled` counted it).
    pub fn start(mut self) -> Result<Started, Skipped> {
        let Some(cell) = self.cell.take() else { return Err(Skipped) };
        let action = core::mem::replace(&mut self.action, Action::Press);
        if cell.reply.lock().start().is_ok() {
            Ok(Started { node: self.node, action, cell: Some(cell) })
        } else {
            Err(Skipped)
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.take() {
            cell.answer(|r| r.answer_unrun(Answer::WindowClosed));
        }
    }
}

impl Started {
    pub fn node(&self) -> u64 {
        self.node
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    /// The owner's answer, which wakes the asker.
    pub fn finish(mut self, answer: Answer) {
        if let Some(cell) = self.cell.take() {
            cell.answer(|r| r.finish(answer));
        }
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.take() {
            cell.answer(|r| r.answer_dropped(Answer::Dropped));
        }
    }
}

// -- the toolkit in a window ---------------------------------------------------------

/// The size the toolkit's text is drawn at.
const PX: Px = match Px::new(16) {
    Some(px) => px,
    None => panic!("16 px is in range"),
};
/// Each window's glyph cache.
const CACHE_BYTES: usize = 64 * 1024;
/// The widest line drawn.
const MAX_WIDTH: u32 = 2048;

/// The toolkit's text: the UI font (`crate::font::face`), one cache per
/// drawing thread.
pub struct UiText {
    font: Font<'static>,
    line: u32,
}

/// Whether a line too wide to draw was warned about (once a boot).
static TOO_WIDE_SAID: AtomicBool = AtomicBool::new(false);

impl UiText {
    /// The UI font at the toolkit's size (`Err` if it doesn't parse).
    pub fn new() -> Result<UiText, String> {
        let face = crate::font::face().map_err(|e| alloc::format!("no font: {e}"))?.face();
        let m = face.line_metrics(PX);
        let line = u32::try_from(m.ascent + m.descent).unwrap_or(16);
        Ok(UiText { font: Font::new(face, CACHE_BYTES), line })
    }
}

impl Text for UiText {
    fn line_height(&self) -> u32 {
        self.line
    }

    fn width(&mut self, text: &str) -> u32 {
        self.font.face().measure(text, PX).width
    }

    fn draw(&mut self, surface: &mut Surface, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        let line = match self.font.render_mask(text, PX, MAX_WIDTH) {
            Ok(line) => line,
            Err(e) => {
                if !TOO_WIDE_SAID.swap(true, Ordering::Relaxed) {
                    log::warn!(target: "ui", "a line too wide to draw ({e}): not drawn (said once)");
                }
                return;
            }
        };
        match Mask::new(line.width(), line.height(), line.coverage()) {
            Some(mask) => surface.blend_mask_in(&mask, x + line.x(), y + line.y(), color, clip),
            None => log::warn!(target: "ui", "a line's mask doesn't match its size: not drawn"),
        }
    }
}

/// The toolkit's palette from a theme.
pub fn palette(t: &Theme) -> Palette {
    Palette {
        background: t.client,
        text: t.client_text,
        accent: t.accent,
        accent_text: t.accent_text,
        border: t.border,
        disabled: t.border,
        field: t.client,
    }
}

/// A toolkit window's app and drawing state: see the module's doc.
pub struct Toolkit<A: App> {
    app: A,
    text: UiText,
    palette: Palette,
    /// Where the tree is laid out (client coordinates).
    area: Rect,
}

/// What a step did.
#[must_use = "a window asked to close must be closed"]
pub enum Step {
    Carry,
    /// Close the window: drop its handle, then (if its action closed it)
    /// answer the request that did ([`close`]).
    Close(Option<Started>),
}

impl<A: App> Toolkit<A> {
    pub fn new(app: A, area: Rect) -> Result<Toolkit<A>, String> {
        Ok(Toolkit { app, text: UiText::new()?, palette: palette(desktop::theme()), area })
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }

    /// Lay the tree out and paint it (and the app's frame over it) into
    /// the owner's surface, then commit the client area; publish the tree
    /// if it changed. Every frame of an animation.
    pub fn frame(&mut self, h: &mut WindowHandle) {
        let changed = self.lay_out();
        self.paint_and_commit(h);
        if changed {
            h.publish(self.app.ui().top_ids(), self.app.ui().nodes());
        }
    }

    /// As [`Toolkit::frame`], only if the tree changed.
    pub fn refresh(&mut self, h: &mut WindowHandle) {
        if self.lay_out() {
            self.paint_and_commit(h);
            h.publish(self.app.ui().top_ids(), self.app.ui().nodes());
        }
    }

    /// Lay out: whether anything shown changed since the last paint.
    fn lay_out(&mut self) -> bool {
        self.app.ui().layout(self.area, &mut self.text);
        self.app.ui().take_changed()
    }

    fn paint_and_commit(&mut self, h: &mut WindowHandle) {
        let s = h.surface_mut();
        self.app.ui().paint(s, &mut self.text, &self.palette);
        self.app.paint_over(s);
        let bounds = s.bounds();
        h.commit(&[bounds]);
    }

    /// One step: hand window event `e` to the tree; then, if it changed,
    /// repaint, commit and publish; then answer a request it ran, so its
    /// asker sees the effect in `ui tree`. A window to close is
    /// [`Step::Close`] (its request answered once it is gone).
    pub fn step(&mut self, h: &mut WindowHandle, e: WindowEvent) -> Step {
        let mut answer = None;
        let then = match e {
            WindowEvent::Close => Then::Close,
            WindowEvent::Request(req) => match req.start() {
                Err(Skipped) => Then::Carry,
                Ok(started) => {
                    let served = serve(&mut self.app, &mut self.text, self.area, started.node(), started.action());
                    if served == Served::ClosedItsWindow {
                        return Step::Close(Some(started));
                    }
                    answer = Some((started, served));
                    Then::Carry
                }
            },
            WindowEvent::FocusGained => {
                self.app.ui().set_window_focused(true);
                Then::Carry
            }
            WindowEvent::FocusLost => {
                self.app.ui().set_window_focused(false);
                self.input(Input::Cancel)
            }
            WindowEvent::Lost => self.input(Input::Cancel),
            WindowEvent::PointerDown { x, y, button: Button::Left, .. } => self.input(Input::Down { x, y }),
            WindowEvent::PointerUp { x, y, button: Button::Left, .. } => self.input(Input::Up { x, y }),
            WindowEvent::Key(k) => match key(&k) {
                Some(k) => self.input(Input::Key(k)),
                None => Then::Carry,
            },
            WindowEvent::PointerDown { .. } | WindowEvent::PointerUp { .. } | WindowEvent::PointerMove { .. } => {
                Then::Carry
            }
        };
        if then == Then::Close {
            return Step::Close(None);
        }
        self.refresh(h);
        if let Some((started, served)) = answer {
            started.finish(Answer::Ran(served));
        }
        Step::Carry
    }

    fn input(&mut self, input: Input) -> Then {
        let events = self.app.ui().handle(input);
        self.app.apply(&events)
    }
}

/// Close a window ([`Step::Close`]): drop its handle (the window goes),
/// then answer the request whose action closed it.
pub fn close(h: WindowHandle, closing: Option<Started>) {
    drop(h);
    if let Some(started) = closing {
        started.finish(Answer::Ran(Served::ClosedItsWindow));
    }
}

/// A key press as the toolkit takes it (releases and keys it ignores:
/// `None`).
fn key(k: &KeyEvent) -> Option<Key> {
    let KeyAction::Press { text } = k.action else { return None };
    match (k.code, text) {
        (KeyCode::Tab, _) if k.mods.is_shifted() => Some(Key::BackTab),
        (KeyCode::Tab, _) => Some(Key::Tab),
        (KeyCode::Backspace, _) => Some(Key::Backspace),
        (KeyCode::Return | KeyCode::NumpadEnter, _) => Some(Key::Enter),
        (_, Some(c)) if !c.is_control() => Some(Key::Char(c)),
        _ => None,
    }
}

// -- Counter ------------------------------------------------------------------------------

/// The Counter window's client size.
const COUNTER_SIZE: (u32, u32) = (300, 140);

/// `desktop counter`: open the Counter window and start its owner; its id.
pub fn open_counter() -> Result<u32, String> {
    let (w, h) = COUNTER_SIZE;
    let toolkit = Toolkit::new(Counter::new(), Rect::new(0, 0, w, h))?;
    let handle = desktop::open_window("Counter", w, h).map_err(|e| alloc::format!("no window: {e}"))?;
    let id = handle.id();
    let _detached = crate::sched::spawn("desktop-counter", move || counter_owner(handle, toolkit))
        .map_err(|e| alloc::format!("no thread: {e}"))?;
    Ok(id)
}

/// The Counter window's owner: its tree drawn and published after every
/// change, until it closes (its close box, `ui invoke <root> close`, Quit).
fn counter_owner(mut h: WindowHandle, mut tk: Toolkit<Counter>) {
    tk.refresh(&mut h);
    loop {
        let e = h.wait_event();
        if let Step::Close(closing) = tk.step(&mut h, e) {
            return close(h, closing);
        }
    }
}

// -- the commands --------------------------------------------------------------------------

pub const USAGE: &str = "usage: ui tree [window] | invoke <node> <action> [args]";

/// `ui tree`'s columns (ARCHITECTURE section 11, in this order).
pub const COLUMNS: [Column; 12] = [
    Column::new("id", Kind::Int, Align::Right, 3),
    Column::new("window", Kind::Int, Align::Right, 6),
    Column::new("depth", Kind::Int, Align::Right, 5),
    Column::new("role", Kind::Text, Align::Left, 8),
    Column::new("name", Kind::Text, Align::Left, 8),
    Column::new("value", Kind::Text, Align::Left, 5),
    Column::new("state", Kind::Text, Align::Left, 7),
    Column::new("actions", Kind::Text, Align::Left, 11),
    Column::new("x", Kind::Int, Align::Right, 4),
    Column::new("y", Kind::Int, Align::Right, 4),
    Column::new("w", Kind::Int, Align::Right, 4),
    Column::new("h", Kind::Int, Align::Right, 4),
];

/// No value (`-`, never matched by `where`).
const DASH: Value = Value::Absent("-");

/// A node's record (screen coordinates).
fn record(r: &TreeRow) -> Vec<Value> {
    let n = &r.node;
    let b = n.bounds();
    let actions: Vec<&str> = n.actions().iter().map(|a| a.name()).collect();
    alloc::vec![
        int(n.id().get()),
        int(u64::from(r.window)),
        int(u64::from(r.depth)),
        text(n.role().name()),
        text(n.name()),
        n.value().map_or(DASH, |v| Value::Text(v.to_string())),
        if n.state().is_empty() { DASH } else { Value::Text(n.state().to_string()) },
        if actions.is_empty() { DASH } else { Value::Text(actions.join(",")) },
        Value::Int(i64::from(b.x())),
        Value::Int(i64::from(b.y())),
        int(u64::from(b.w())),
        int(u64::from(b.h())),
    ]
}

fn table(rows: &[TreeRow]) -> pipe::Table {
    let mut t = pipe::Table::new(&COLUMNS);
    for r in rows {
        t.push(record(r));
    }
    t
}

/// `ui tree [window]`: one row per node, depth first, each window's root
/// first, front to back.
pub fn tree_table(args: &[String]) -> Result<pipe::Table, String> {
    let window = match args {
        [] => None,
        [w] => Some(w.parse::<u32>().map_err(|_| String::from(USAGE))?),
        _ => return Err(String::from(USAGE)),
    };
    desktop::tree_rows(window).map(|rows| table(&rows)).map_err(|e| alloc::format!("ui tree: {e}"))
}

/// `ui`'s records: `ui tree [window]`'s; `ui invoke` runs as a command.
pub fn records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    match args {
        [what, rest @ ..] if what == "tree" => Some(tree_table(rest)),
        _ => None,
    }
}

/// The shell's `ui`.
pub fn command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    use crate::shell::sysinfo::print_table;
    match args {
        [what, rest @ ..] if what == "tree" => print_table(ctx, tree_table(rest)),
        [what, node, action, rest @ ..] if what == "invoke" => {
            let Ok(node) = node.parse::<u64>() else { return ctx.fail(USAGE) };
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            match Action::parse(action, &rest) {
                Ok(action) => invoke(ctx, node, action),
                Err(e) => ctx.fail(format_args!("ui invoke: {e}")),
            }
        }
        _ => ctx.fail(USAGE),
    }
}

/// `ui invoke <node> <action>`: see ARCHITECTURE section 11. The asker's
/// own errors (no desktop, no node, a root's action) come first; a widget's
/// action is the owner's.
fn invoke(ctx: &mut crate::shell::Ctx<'_>, node: u64, action: Action) {
    if !desktop::started() {
        return ctx.fail("ui invoke: no desktop");
    }
    match desktop::find_node(node) {
        None => ctx.fail(format_args!("ui invoke: no node {node}")),
        Some(NodeAt::Root(window)) => invoke_root(ctx, window, &action),
        Some(NodeAt::Widget(window)) => invoke_widget(ctx, window, node, action),
    }
}

/// What a window's root does with each action: the compositor answers
/// `focus`, the owner `close` (its `Close`); none takes the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootAction {
    Focus,
    Close,
}

fn root_action(kind: ActionKind) -> Option<RootAction> {
    match kind {
        ActionKind::Focus => Some(RootAction::Focus),
        ActionKind::Close => Some(RootAction::Close),
        // `minimize` and `restore` come with #157.
        ActionKind::Press | ActionKind::SetValue | ActionKind::Select | ActionKind::Minimize | ActionKind::Restore => {
            None
        }
    }
}

/// An action on a window's root.
fn invoke_root(ctx: &mut crate::shell::Ctx<'_>, window: u32, action: &Action) {
    let kind = action.kind();
    match root_action(kind) {
        None => ctx.fail(format_args!("ui invoke: {}", InvokeError::NotAllowed(lumen_ui::Role::Window, kind))),
        Some(RootAction::Focus) => match desktop::focus_window(window) {
            Some(row) => print_rows(ctx, &[row]),
            None => ctx.fail(format_args!("ui invoke: window {window} closed")),
        },
        Some(RootAction::Close) => {
            if desktop::close_window(window, DEADLINE_NS) {
                let _ = writeln!(ctx.out, "ui invoke: window {window} closed");
            } else {
                ctx.fail(format_args!("ui invoke: window {window} still open (its close is still queued)"));
            }
        }
    }
}

/// An action on a widget: a request in its window's queue, answered by the
/// owner (see [`Reply`]'s steps).
fn invoke_widget(ctx: &mut crate::shell::Ctx<'_>, window: u32, node: u64, action: Action) {
    let (req, cell) = Request::new(node, action);
    match desktop::push_request(window, req) {
        Ok(()) => {}
        Err(Refused::Busy(req)) => {
            drop(req);
            return ctx.fail(format_args!("ui invoke: window {window} busy (try again)"));
        }
        Err(Refused::Dead(req)) => {
            drop(req);
            return ctx.fail(format_args!("ui invoke: window {window} closed"));
        }
    }
    let wait = || {
        let deadline = crate::time::now_ns().saturating_add(DEADLINE_NS);
        let _ = cell.done.wait_until_or_deadline(deadline, || cell.reply.lock().is_done());
    };
    wait();
    let first = cell.reply.lock().after_first_deadline();
    let (answer, late) = match first {
        First::Answered(answer) => (Some(answer), false),
        First::Cancelled => (None, false),
        First::Running => {
            wait();
            match cell.reply.lock().after_second_deadline() {
                Second::Late(answer) => (Some(answer), true),
                Second::StillRunning => {
                    desktop::request_finished(false, false);
                    return ctx.fail(format_args!("ui invoke: still running (window {window})"));
                }
            }
        }
    };
    desktop::request_finished(answer.is_none(), late);
    match answer {
        Some(answer) => print_answer(ctx, window, node, answer, late),
        None => ctx.fail(format_args!("ui invoke: cancelled: window {window} didn't take it in 1 s (try again)")),
    }
}

fn print_answer(ctx: &mut crate::shell::Ctx<'_>, window: u32, node: u64, answer: Answer, late: bool) {
    if late {
        let _ = writeln!(ctx.out, "ui invoke: ran late");
    }
    match answer {
        Answer::Ran(Served::Row { depth, node }) => print_rows(ctx, &[desktop::node_row(window, depth, node)]),
        Answer::Ran(Served::Error(InvokeError::NoNode)) => ctx.fail(format_args!("ui invoke: no node {node}")),
        Answer::Ran(Served::Error(e)) => ctx.fail(format_args!("ui invoke: {e}")),
        Answer::Ran(Served::RanNodeGone) => {
            let _ = writeln!(ctx.out, "ui invoke: ran; node {node} is gone");
        }
        Answer::Ran(Served::ClosedItsWindow) => {
            let _ = writeln!(ctx.out, "ui invoke: ran; window {window} closed");
        }
        Answer::WindowClosed => ctx.fail(format_args!("ui invoke: window {window} closed before it ran")),
        Answer::Dropped => ctx.fail(format_args!("ui invoke: dropped while running (window {window}): a bug")),
    }
}

fn print_rows(ctx: &mut crate::shell::Ctx<'_>, rows: &[TreeRow]) {
    crate::shell::sysinfo::print_table(ctx, Ok(table(rows)));
}

// -- ktests -----------------------------------------------------------------------------------

crate::ktest!(
    fn a_request_answers_for_itself_when_dropped() {
        // Dropped before it ran (its window closed): "window closed";
        // dropped while running: "dropped"; cancelled first: skipped and
        // never answered; started and finished: answered once.
        use crate::kassert;
        let (req, cell) = Request::new(1, Action::Press);
        drop(req);
        kassert!(matches!(cell.reply.lock().take(), Some(Answer::WindowClosed)));
        let (req, cell) = Request::new(2, Action::Press);
        let started = req.start().map_err(|_| "not started")?;
        drop(started);
        kassert!(matches!(cell.reply.lock().take(), Some(Answer::Dropped)));
        let (req, cell) = Request::new(3, Action::Press);
        kassert!(cell.reply.lock().cancel());
        kassert!(req.start().is_err());
        kassert!(cell.phase() == Phase::Cancelled);
        let (req, cell) = Request::new(4, Action::Press);
        let started = req.start().map_err(|_| "not started")?;
        kassert!(started.node() == 4 && *started.action() == Action::Press);
        started.finish(Answer::Ran(Served::RanNodeGone));
        kassert!(matches!(cell.reply.lock().take(), Some(Answer::Ran(Served::RanNodeGone))));
        Ok(())
    }
);

crate::ktest!(
    fn every_root_action_reaches_a_handler() {
        // Each action a window's root lists is answered (focus by the
        // compositor, close by the owner); the others aren't taken.
        use crate::kassert;
        for &kind in lumen_ui::Role::Window.actions() {
            kassert!(root_action(kind).is_some());
        }
        kassert!(root_action(ActionKind::Press).is_none());
        Ok(())
    }
);

crate::ktest!(
    fn the_toolkit_text_draws_as_wide_as_it_measures() {
        // The UI font through lumen-ui's `Text`: a longer line measures
        // wider, a line drawn unclipped inks up to its measured width (and
        // not past it), and a clip keeps ink inside it.
        use crate::kassert;
        let mut t = UiText::new().map_err(|_| "no font")?;
        kassert!(t.line_height() >= 16);
        let width = t.width("Add more");
        kassert!(width > t.width("Add") && t.width("Add") > 0);
        let mut s = Surface::new(200, 40);
        s.fill(0xffffff);
        let all = s.bounds();
        t.draw(&mut s, 4, 4, "Add more", 0x000000, all);
        let right =
            (0..200u32).rev().find(|&x| (0..40u32).any(|y| s.row(y)[x as usize] != 0xffffff)).ok_or("no ink")?;
        crate::ktest::diag(format_args!("width {width}, rightmost ink at {}", right - 4));
        kassert!(right - 4 <= width + 1 && right - 4 + 4 >= width);
        let mut s = Surface::new(200, 40);
        s.fill(0xffffff);
        t.draw(&mut s, 4, 4, "Add more", 0x000000, Rect::new(0, 0, 30, 40));
        kassert!((0..40u32).all(|y| s.row(y)[30..].iter().all(|&p| p == 0xffffff)));
        Ok(())
    }
);
