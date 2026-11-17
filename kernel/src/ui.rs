//! The desktop's toolkit in the kernel and the automation tree's commands
//! (#156; ARCHITECTURE section 11, "Toolkit and the automation tree"):
//!
//! - [`Toolkit`]: a `lumen_ui::Ui` in a window: text through `lumen-font`
//!   ([`UiText`]), painting, committing and publishing its tree, and its
//!   window's events (the primary button, keys, focus, requests) handed to
//!   it, requests answered by the handler a click reaches ([`serve`]).
//! - [`Request`]: `ui invoke`'s request in a window's queue, with its reply
//!   slot (`lumen_ui::Reply` behind a lock, and the asker's wait queue). It
//!   answers "window closed" if dropped unanswered.
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

use lumen_font::{Font, Px};
use lumen_gfx::{Mask, Rect, Surface};
use lumen_input::{Button, KeyAction, KeyCode, KeyEvent};
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_ui::counter::{Counter, Then};
use lumen_ui::{Action, AutomationNode, Input, Key, Outcome, Palette, Reply, Text, Ui, UiEvent};
use lumen_wm::Refused;

use crate::desktop::{self, NodeAt, TreeRow, WindowEvent, WindowHandle};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// How long `ui invoke` waits for an answer, and again once the owner took
/// the request.
const DEADLINE_NS: u64 = 1_000_000_000;

// -- requests --------------------------------------------------------------------

/// What a request's owner answered.
#[derive(Debug)]
pub enum Answer {
    /// Done: the node as it is after the action (client coordinates).
    Row { depth: u32, node: AutomationNode },
    /// Refused (it changed nothing): why.
    Error(String),
    /// Done, and the action closed its window (a Quit button).
    ClosedItsWindow,
    /// Its window closed before the request ran (or the request was
    /// dropped unanswered).
    WindowClosed,
}

/// A request's reply slot, shared by its asker and the request.
struct ReplyCell {
    reply: SpinLock<Reply<Answer>>,
    /// The asker waits here; woken after every write.
    done: WaitQueue,
}

/// `ui invoke`'s request in a window's queue: the node and the action, and
/// its reply. Not `Clone`; answers "window closed" if dropped unanswered
/// (so `ui invoke` always ends). Dropped only with no desktop lock held.
pub struct Request {
    node: u64,
    action: Action,
    /// `None` once answered.
    cell: Option<Arc<ReplyCell>>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request").field("node", &self.node).field("action", &self.action).finish_non_exhaustive()
    }
}

impl Request {
    fn new(node: u64, action: Action) -> (Request, Arc<ReplyCell>) {
        let cell = Arc::new(ReplyCell { reply: SpinLock::new(Reply::new()), done: WaitQueue::new() });
        (Request { node, action, cell: Some(cell.clone()) }, cell)
    }

    /// The owner takes it: false if it may not run (cancelled at the
    /// asker's deadline: skip it, `cancelled` counted it).
    pub fn start(&self) -> bool {
        self.cell.as_ref().is_some_and(|c| c.reply.lock().start().is_ok())
    }

    /// The owner's answer, which wakes the asker.
    pub fn finish(mut self, answer: Answer) {
        if let Some(cell) = self.cell.take() {
            let _ = cell.reply.lock().finish(answer);
            cell.done.wake_all();
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.take()
            && cell.reply.lock().answer_unrun(Answer::WindowClosed)
        {
            cell.done.wake_all();
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
        let Ok(line) = self.font.render_mask(text, PX, MAX_WIDTH) else { return };
        match Mask::new(line.width(), line.height(), line.coverage()) {
            Some(mask) => surface.blend_mask_in(&mask, x + line.x(), y + line.y(), color, clip),
            None => log::warn!(target: "ui", "a line's mask doesn't match its size: not drawn"),
        }
    }
}

/// The toolkit's palette from the desktop's theme.
pub fn palette() -> Palette {
    let t = desktop::theme();
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

/// A window's app: its tree, and what it does with what the tree reports.
pub trait App {
    fn ui(&mut self) -> &mut Ui;
    /// Act on `events`: whether the window closes now.
    fn apply(&mut self, events: &[UiEvent]) -> bool;
}

/// Counter, the toolkit's demo app.
impl App for Counter {
    fn ui(&mut self) -> &mut Ui {
        self.ui_mut()
    }

    fn apply(&mut self, events: &[UiEvent]) -> bool {
        Counter::apply(self, events) == Then::Quit
    }
}

/// A toolkit window's drawing state.
pub struct Toolkit {
    pub text: UiText,
    pub palette: Palette,
    /// Where the tree is laid out (client coordinates).
    pub area: Rect,
}

/// What an event did to a toolkit window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Carry,
    /// Close the window (drop its handle).
    Close,
}

impl Toolkit {
    pub fn new(area: Rect) -> Result<Toolkit, String> {
        Ok(Toolkit { text: UiText::new()?, palette: palette(), area })
    }

    /// Lay `ui` out and paint it into the owner's surface (not committed).
    pub fn paint(&mut self, h: &mut WindowHandle, ui: &mut Ui) {
        ui.layout(self.area, &mut self.text);
        ui.paint(h.surface_mut(), &mut self.text, &self.palette);
    }

    /// Publish `ui`'s tree as the window's.
    pub fn publish(h: &mut WindowHandle, ui: &Ui) {
        h.publish(ui.top_ids(), ui.nodes());
    }

    /// If `ui` changed: lay it out, paint, commit the whole client area and
    /// publish its tree.
    pub fn refresh(&mut self, h: &mut WindowHandle, ui: &mut Ui) {
        if !ui.take_changed() {
            return;
        }
        self.paint(h, ui);
        let _ = ui.take_changed();
        let bounds = h.surface_mut().bounds();
        h.commit(&[bounds]);
        Toolkit::publish(h, ui);
    }

    /// Hand window event `e` to `app`'s tree.
    pub fn handle(&mut self, app: &mut dyn App, e: WindowEvent) -> Flow {
        let input = match e {
            WindowEvent::Close => return Flow::Close,
            WindowEvent::Request(req) => {
                return if serve(req, app, &mut self.text, self.area) { Flow::Close } else { Flow::Carry };
            }
            WindowEvent::FocusGained => {
                app.ui().set_window_focused(true);
                return Flow::Carry;
            }
            WindowEvent::FocusLost => {
                app.ui().set_window_focused(false);
                Input::Cancel
            }
            WindowEvent::Lost => Input::Cancel,
            WindowEvent::PointerDown { x, y, button: Button::Left, .. } => Input::Down { x, y },
            WindowEvent::PointerUp { x, y, button: Button::Left, .. } => Input::Up { x, y },
            WindowEvent::Key(k) => match key(&k) {
                Some(k) => Input::Key(k),
                None => return Flow::Carry,
            },
            WindowEvent::PointerDown { .. } | WindowEvent::PointerUp { .. } | WindowEvent::PointerMove { .. } => {
                return Flow::Carry;
            }
        };
        let events = app.ui().handle(input);
        if app.apply(&events) { Flow::Close } else { Flow::Carry }
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

/// Run request `req` on `app`'s tree, if it may still run: the node's row
/// after the action (laid out in `area`), the error that changed nothing,
/// or (the action closed the window) "ran; window closed". Whether the
/// window closes now.
pub fn serve(req: Request, app: &mut dyn App, text: &mut dyn Text, area: Rect) -> bool {
    if !req.start() {
        return false;
    }
    let node = req.node;
    match app.ui().invoke(node, &req.action) {
        Ok(events) => {
            if app.apply(&events) {
                req.finish(Answer::ClosedItsWindow);
                return true;
            }
            app.ui().layout(area, text);
            let answer = match app.ui().node(node) {
                Some((depth, node)) => Answer::Row { depth, node },
                None => Answer::Error(String::from("the node went away")),
            };
            req.finish(answer);
            false
        }
        Err(e) => {
            req.finish(Answer::Error(e.to_string()));
            false
        }
    }
}

// -- Counter ------------------------------------------------------------------------------

/// The Counter window's client size.
const COUNTER_SIZE: (u32, u32) = (300, 140);

/// `desktop counter`: open the Counter window and start its owner; its id.
pub fn open_counter() -> Result<u32, String> {
    let (w, h) = COUNTER_SIZE;
    let toolkit = Toolkit::new(Rect::new(0, 0, w, h))?;
    let handle = desktop::open_window("Counter", w, h).map_err(|e| alloc::format!("no window: {e}"))?;
    let id = handle.id();
    let _detached = crate::sched::spawn("desktop-counter", move || counter_owner(handle, toolkit))
        .map_err(|e| alloc::format!("no thread: {e}"))?;
    Ok(id)
}

/// The Counter window's owner: its tree drawn and published after every
/// change, until it closes (its close box, `ui invoke <root> close`, Quit).
fn counter_owner(mut h: WindowHandle, mut tk: Toolkit) {
    let mut app = Counter::new();
    loop {
        tk.refresh(&mut h, app.ui_mut());
        let e = h.wait_event();
        if tk.handle(&mut app, e) == Flow::Close {
            return;
        }
    }
}

// -- the commands --------------------------------------------------------------------------

pub const USAGE: &str = "usage: ui tree [window] | invoke <node> <action> [args]";

const COLUMNS: [Column; 12] = [
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

fn int(n: u64) -> Value {
    i64::try_from(n).map_or(Value::Absent("-"), Value::Int)
}

/// A node's record (screen coordinates).
fn record(r: &TreeRow) -> Vec<Value> {
    let n = &r.node;
    let b = n.bounds();
    let actions: Vec<&str> = n.actions().iter().map(|a| a.name()).collect();
    alloc::vec![
        int(n.id().get()),
        int(u64::from(r.window)),
        int(u64::from(r.depth)),
        Value::Text(String::from(n.role().name())),
        Value::Text(String::from(n.name())),
        Value::Text(n.value().map_or(String::from("-"), |v| v.to_string())),
        Value::Text(n.state().to_string()),
        Value::Text(if actions.is_empty() { String::from("-") } else { actions.join(",") }),
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

/// `ui invoke <node> <action>`: see ARCHITECTURE section 11.
fn invoke(ctx: &mut crate::shell::Ctx<'_>, node: u64, action: Action) {
    match desktop::find_node(node) {
        None => ctx.fail(format_args!("ui invoke: no node {node}")),
        Some(NodeAt::Root(window)) => invoke_root(ctx, window, &action),
        Some(NodeAt::Widget(window)) => invoke_widget(ctx, window, node, action),
    }
}

/// An action on a window's root: `focus` the compositor answers; `close`
/// asks the owner (its `Close`) and waits for the window to go.
fn invoke_root(ctx: &mut crate::shell::Ctx<'_>, window: u32, action: &Action) {
    match action {
        Action::Focus => match desktop::focus_window(window) {
            Some(row) => print_rows(ctx, &[row]),
            None => ctx.fail(format_args!("ui invoke: no window {window}")),
        },
        Action::Close => {
            if desktop::close_window(window, DEADLINE_NS) {
                let _ = writeln!(ctx.out, "ui invoke: window {window} closed");
            } else {
                ctx.fail(format_args!("ui invoke: window {window} still open"));
            }
        }
        other => ctx.fail(format_args!(
            "ui invoke: {}",
            lumen_ui::InvokeError::NotAllowed(lumen_ui::Role::Window, other.kind())
        )),
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
            return ctx.fail(format_args!("ui invoke: window {window} busy"));
        }
        Err(Refused::Dead(req)) => {
            drop(req);
            return ctx.fail(format_args!("ui invoke: no node {node}"));
        }
    }
    let wait = || {
        let deadline = crate::time::now_ns().saturating_add(DEADLINE_NS);
        let _ = cell.done.wait_until_or_deadline(deadline, || cell.reply.lock().is_done());
    };
    wait();
    let mut outcome = cell.reply.lock().after_deadline();
    if matches!(outcome, Outcome::Running) {
        wait();
        outcome = cell.reply.lock().after_second_deadline();
    }
    desktop::request_finished(matches!(outcome, Outcome::Cancelled), matches!(outcome, Outcome::Late(_)));
    match outcome {
        Outcome::Answered(answer) => print_answer(ctx, window, answer, false),
        Outcome::Late(answer) => print_answer(ctx, window, answer, true),
        Outcome::Cancelled => {
            ctx.fail(format_args!("ui invoke: cancelled: window {window} didn't take it in 1 s (try again)"))
        }
        Outcome::StillRunning | Outcome::Running => {
            ctx.fail(format_args!("ui invoke: still running (window {window})"))
        }
    }
}

fn print_answer(ctx: &mut crate::shell::Ctx<'_>, window: u32, answer: Answer, late: bool) {
    if late {
        let _ = writeln!(ctx.out, "ui invoke: ran late");
    }
    match answer {
        Answer::Row { depth, node } => print_rows(ctx, &[desktop::node_row(window, depth, node)]),
        Answer::Error(e) => ctx.fail(format_args!("ui invoke: {e}")),
        Answer::ClosedItsWindow => {
            let _ = writeln!(ctx.out, "ui invoke: ran; window {window} closed");
        }
        Answer::WindowClosed => ctx.fail(format_args!("ui invoke: window {window} closed before it ran")),
    }
}

fn print_rows(ctx: &mut crate::shell::Ctx<'_>, rows: &[TreeRow]) {
    crate::shell::sysinfo::print_table(ctx, Ok(table(rows)));
}

// -- ktests -----------------------------------------------------------------------------------

crate::ktest!(
    fn a_request_dropped_unanswered_answers_window_closed() {
        // A request the owner never answers (its window closed, or it was
        // dropped) answers itself, waking its asker; one cancelled first
        // stays cancelled; a finished one isn't answered again.
        use crate::kassert;
        let (req, cell) = Request::new(1, Action::Press);
        drop(req);
        kassert!(matches!(cell.reply.lock().take(), Some(Answer::WindowClosed)));
        let (req, cell) = Request::new(2, Action::Press);
        kassert!(cell.reply.lock().cancel());
        kassert!(!req.start());
        drop(req);
        kassert!(matches!(*cell.reply.lock(), Reply::Cancelled));
        let (req, cell) = Request::new(3, Action::Press);
        kassert!(req.start());
        req.finish(Answer::Error(String::from("no")));
        kassert!(matches!(cell.reply.lock().take(), Some(Answer::Error(_))));
        Ok(())
    }
);

crate::ktest!(
    fn the_toolkit_text_measures_what_it_draws() {
        // The UI font through lumen-ui's `Text`: a line's width grows with
        // its text, and drawing "Add" puts ink inside the clip only.
        use crate::kassert;
        let mut t = UiText::new().map_err(|_| "no font")?;
        kassert!(t.line_height() >= 16);
        kassert!(t.width("Add") > 0 && t.width("Add more") > t.width("Add"));
        let mut s = Surface::new(80, 40);
        s.fill(0xffffff);
        let clip = Rect::new(0, 0, 40, 40);
        t.draw(&mut s, 4, 4, "Add more", 0x000000, clip);
        let inked = |x0: u32, x1: u32| (0..40).any(|y| (x0..x1).any(|x| s.row(y)[x as usize] != 0xffffff));
        kassert!(inked(0, 40));
        kassert!(!inked(40, 80));
        Ok(())
    }
);
