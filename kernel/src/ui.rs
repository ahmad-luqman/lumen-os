//! The desktop's toolkit in the kernel and the automation tree's commands
//! (#156; ARCHITECTURE section 11, "Toolkit and the automation tree"):
//!
//! - [`Toolkit`]: a `lumen_ui::App` in a window, one step per event: the
//!   event handed to the tree, then (if it changed) repainted, committed
//!   and published, and only then a request answered, so once `ui invoke`
//!   returns, `ui tree` shows its effect (and a window its action closed is
//!   gone). Text through `crate::font::Text`, the desktop's one text path.
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
use lumen_gfx::{Rect, Surface};
use lumen_input::{Button, KeyEvent};
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_ui::counter::Counter;
use lumen_ui::{
    Action, ActionKind, App, First, Input, InvokeError, Key, Palette, Phase, Reply, Second, Served, Text, Then, serve,
};
use lumen_wm::{Refused, Theme};

use crate::desktop::{self, NodeAt, TreeRow, WindowEvent, WindowHandle};
use crate::sched::WaitQueue;
use crate::shell::sysinfo::{DASH, int, text, text_or_dash};
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
    /// Its owner, an app, ended or closed its window while running it,
    /// unanswered (#207): it may have run.
    OwnerEnded,
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

/// A window's one request slot, held by its request (#207: an app's window
/// holds at most one request queued or running, `desktop::push_request`
/// refusing another with `Refused::Busy`): freed when the request is
/// answered, skipped or refused, before its asker is woken, so the asker's
/// next request finds it free.
pub struct RequestSlot(Arc<AtomicBool>);

impl RequestSlot {
    /// The slot `flag` guards, if it is free.
    pub(crate) fn take(flag: &Arc<AtomicBool>) -> Option<RequestSlot> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok()?;
        Some(RequestSlot(flag.clone()))
    }
}

impl Drop for RequestSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
    /// Its window's one request slot, if its window has one.
    slot: Option<RequestSlot>,
}

/// A request the owner took ([`Request::start`]): the only thing that can
/// answer it ([`Started::finish`]); dropped unanswered, it answers
/// "dropped while running".
pub struct Started {
    node: u64,
    action: Action,
    cell: Option<Arc<ReplyCell>>,
    slot: Option<RequestSlot>,
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
        (Request { node, action, cell: Some(cell.clone()), slot: None }, cell)
    }

    /// Its window's request slot, held until it is answered, skipped or
    /// refused.
    pub(crate) fn holding(mut self, slot: RequestSlot) -> Request {
        self.slot = Some(slot);
        self
    }

    /// The node it is for (a global id).
    pub fn node(&self) -> u64 {
        self.node
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    /// The owner takes it: a [`Started`] to answer, or [`Skipped`] if it
    /// was cancelled at the asker's deadline (`cancelled` counted it).
    pub fn start(mut self) -> Result<Started, Skipped> {
        let Some(cell) = self.cell.take() else { return Err(Skipped) };
        let action = core::mem::replace(&mut self.action, Action::Press);
        let started = cell.reply.lock().start();
        match started {
            Ok(()) => Ok(Started { node: self.node, action, cell: Some(cell), slot: self.slot.take() }),
            Err(why) => {
                // Only the asker's cancel takes a request out of `Pending`
                // before its owner reads it.
                debug_assert_eq!(why, lumen_ui::NotStarted::Cancelled);
                Err(Skipped)
            }
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        drop(self.slot.take());
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

    /// The owner's answer, which wakes the asker (its window's request
    /// slot freed first).
    pub fn finish(mut self, answer: Answer) {
        drop(self.slot.take());
        if let Some(cell) = self.cell.take() {
            cell.answer(|r| r.finish(answer));
        }
    }
}

/// Answer `req` for a window without a toolkit (a demo, a terminal window,
/// #155): it publishes no nodes, so `ui invoke` reaches only its root (whose
/// `focus` the compositor answers and whose `close` comes as `Close`), and
/// a request that still arrives is answered as `ui invoke` answers a node
/// it can't find: never dropped (that would answer "closed before it
/// ran"). One cancelled at its asker's deadline is skipped.
pub fn refuse(req: Request) {
    if let Ok(started) = req.start() {
        started.finish(Answer::Ran(Served::Error(InvokeError::NoNode)));
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        drop(self.slot.take());
        if let Some(cell) = self.cell.take() {
            log::error!(target: "ui", "a request on node {} was dropped while running, unanswered (a bug)", self.node);
            cell.answer(|r| r.answer_dropped(Answer::Dropped));
        }
    }
}

// -- the toolkit in a window ---------------------------------------------------------

/// The toolkit's text: `crate::font::Text` (the UI font, one glyph cache
/// per drawing thread) at `TEXT_PX`, the one text path the desktop draws
/// with.
impl Text for crate::font::Text {
    fn line_height(&self) -> u32 {
        crate::font::Text::line_height(self, crate::font::TEXT_PX)
    }

    fn width(&mut self, text: &str) -> u32 {
        self.measure(text, crate::font::TEXT_PX)
    }

    fn draw(&mut self, surface: &mut Surface, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        crate::font::Text::draw(self, surface, x, y, text, crate::font::TEXT_PX, color, clip);
    }
}

/// The taskbar's palette (#157): its own colours; its buttons in the
/// title bars' colours.
pub fn taskbar_palette(t: &Theme) -> Palette {
    Palette {
        background: t.taskbar,
        text: t.taskbar_text,
        accent: t.title,
        accent_text: t.title_text,
        border: t.border,
        disabled: t.border,
        field: t.client,
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
    text: crate::font::Text,
    /// From the theme its window was last told of (`WindowEvent::Theme`).
    palette: Palette,
    /// How a theme becomes its palette ([`palette`], [`taskbar_palette`]).
    choose: fn(&Theme) -> Palette,
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
    /// `app` laid out in `area`, drawn with `theme` (its window's,
    /// `WindowHandle::theme`) until a `Theme` event says otherwise.
    pub fn new(app: A, area: Rect, theme: &Theme) -> Result<Toolkit<A>, String> {
        Toolkit::with_palette(app, area, theme, palette)
    }

    /// [`Toolkit::new`] with the palette made from each theme by `choose`
    /// (the taskbar's colours, #157).
    pub fn with_palette(
        app: A,
        area: Rect,
        theme: &Theme,
        choose: fn(&Theme) -> Palette,
    ) -> Result<Toolkit<A>, String> {
        let text = crate::font::Text::new().map_err(|e| alloc::format!("no font: {e}"))?;
        Ok(Toolkit { app, text, palette: choose(theme), choose, area })
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }

    /// Lay the tree out and paint all of it (and the app's frame over
    /// it) into the owner's surface, then commit the client area; publish
    /// the tree if it changed. Every frame of an animation.
    pub fn frame(&mut self, h: &mut WindowHandle) {
        let changed = self.lay_out();
        let _all = self.app.ui_mut().take_damage();
        self.paint_all(h);
        if changed {
            self.publish(h);
        }
    }

    /// Paint the whole tree and the app's frame over it, and commit it.
    fn paint_all(&mut self, h: &mut WindowHandle) {
        let s = h.surface_mut();
        self.app.ui_mut().paint(s, &mut self.text, &self.palette);
        self.app.paint_over(s);
        let bounds = s.bounds();
        h.commit(&[bounds]);
    }

    /// Lay the tree out; repaint and commit only what changed
    /// (`lumen_ui::repaint`), then publish the tree if it changed.
    pub fn refresh(&mut self, h: &mut WindowHandle) {
        let done = lumen_ui::repaint(&mut self.app, h.surface_mut(), &mut self.text, &self.palette, self.area);
        if !done.rects.is_empty() {
            h.commit(&done.rects);
        }
        if done.changed {
            self.publish(h);
        }
    }

    fn publish(&mut self, h: &mut WindowHandle) {
        let ui = self.app.ui_mut();
        h.publish(ui.top_ids(), ui.nodes());
    }

    /// Lay out: whether the tree changed since the last publish.
    fn lay_out(&mut self) -> bool {
        self.app.ui_mut().layout(self.area, &mut self.text);
        self.app.ui_mut().take_changed()
    }

    /// One step: hand window event `e` to the tree; then, if it changed,
    /// repaint, commit and publish; then answer a request it ran, so its
    /// asker sees the effect in `ui tree`. A window to close is
    /// [`Step::Close`] (its request answered once it is gone).
    pub fn step(&mut self, h: &mut WindowHandle, e: WindowEvent) -> Step {
        let then = match e {
            WindowEvent::Close => Then::Close,
            WindowEvent::Request(req) => return self.request(h, req),
            WindowEvent::Theme(theme) => {
                // "Redraw": the whole tree in the theme handed over.
                self.palette = (self.choose)(theme);
                self.app.ui_mut().invalidate();
                Then::Carry
            }
            WindowEvent::FocusGained => {
                self.app.ui_mut().set_window_focused(true);
                Then::Carry
            }
            WindowEvent::FocusLost => {
                self.app.ui_mut().set_window_focused(false);
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
        if matches!(then, Then::Close) {
            return Step::Close(None);
        }
        self.refresh(h);
        Step::Carry
    }

    /// A request: run it (unless cancelled), publish what it changed, then
    /// answer; one that closed its window is answered once it is gone.
    fn request(&mut self, h: &mut WindowHandle, req: Request) -> Step {
        let Ok(started) = req.start() else { return Step::Carry };
        let served = serve(&mut self.app, &mut self.text, self.area, started.node(), started.action());
        if served == Served::ClosedItsWindow {
            return Step::Close(Some(started));
        }
        self.refresh(h);
        started.finish(Answer::Ran(served));
        Step::Carry
    }

    fn input(&mut self, input: Input) -> Then {
        let events = self.app.ui_mut().handle(input);
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

/// A key event as the toolkit takes it (releases and keys it ignores:
/// `None`): `lumen_app_api::event::toolkit_key`, the one mapping an app's
/// toolkit uses too.
fn key(k: &KeyEvent) -> Option<Key> {
    lumen_app_api::event::toolkit_key(k.code.into(), k.mods.into(), k.action.into())
}

// -- Counter ------------------------------------------------------------------------------

/// The Counter window's client size.
const COUNTER_SIZE: (u32, u32) = (300, 140);

/// `desktop counter`: open the Counter window and start its owner; its id.
pub fn open_counter() -> Result<u32, String> {
    let (w, h) = COUNTER_SIZE;
    let handle = desktop::open_window("Counter", w, h).map_err(|e| alloc::format!("no window: {e}"))?;
    // (Without a font the window goes again, dropped with its handle.)
    let toolkit = Toolkit::new(Counter::new(), Rect::new(0, 0, w, h), handle.theme())?;
    let id = handle.id();
    let _detached = crate::sched::spawn("desktop-counter", move || counter_owner(handle, toolkit))
        .map_err(|e| alloc::format!("no thread: {e}"))?;
    Ok(id)
}

/// The Counter window's owner: its tree drawn and published after every
/// change (its first event, `Theme`, draws it), until it closes (its close
/// box, `ui invoke <root> close`, Quit).
fn counter_owner(mut h: WindowHandle, mut tk: Toolkit<Counter>) {
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

/// A node's record (screen coordinates).
fn record(r: &TreeRow) -> Vec<Value> {
    let n = &r.node;
    let b = n.bounds();
    let actions: Vec<&str> = n.actions().iter().map(|a| a.name()).collect();
    alloc::vec![
        int(n.id().get()),
        int(r.window),
        int(r.depth),
        text(n.role().name()),
        text(n.name()),
        n.value().map_or(DASH, |v| Value::Text(v.to_string())),
        text_or_dash(n.state().to_string()),
        if actions.is_empty() { DASH } else { Value::Text(actions.join(",")) },
        int(b.x()),
        int(b.y()),
        int(b.w()),
        int(b.h()),
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
/// `focus`, `minimize` and `restore`, the owner `close` (its `Close`);
/// none takes the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootAction {
    Focus,
    Close,
    Minimize,
    Restore,
}

fn root_action(kind: ActionKind) -> Option<RootAction> {
    match kind {
        ActionKind::Focus => Some(RootAction::Focus),
        ActionKind::Close => Some(RootAction::Close),
        ActionKind::Minimize => Some(RootAction::Minimize),
        ActionKind::Restore => Some(RootAction::Restore),
        ActionKind::Press | ActionKind::SetValue | ActionKind::Select => None,
    }
}

/// An action on a window's root (a panel's, the taskbar's, is a group's:
/// it takes none).
fn invoke_root(ctx: &mut crate::shell::Ctx<'_>, window: u32, action: &Action) {
    let kind = action.kind();
    let role = desktop::root_role(window).unwrap_or(lumen_ui::Role::Window);
    let what = if role.actions().contains(&kind) { root_action(kind) } else { None };
    let changed = |ctx: &mut crate::shell::Ctx<'_>, r: Result<desktop::TreeRow, desktop::NotChanged>, already| match r {
        Ok(row) => print_rows(ctx, &[row]),
        Err(desktop::NotChanged::Closed) => ctx.fail(format_args!("ui invoke: window {window} closed")),
        Err(desktop::NotChanged::Already) => ctx.fail(format_args!("ui invoke: window {window} {already}")),
        Err(desktop::NotChanged::Taskbar) => ctx.fail(format_args!("ui invoke: window {window} is the taskbar")),
    };
    match what {
        None => ctx.fail(format_args!("ui invoke: {}", InvokeError::NotAllowed(role, kind))),
        Some(RootAction::Focus) => match desktop::focus_window(window) {
            Some(row) => print_rows(ctx, &[row]),
            None => ctx.fail(format_args!("ui invoke: window {window} closed")),
        },
        Some(RootAction::Minimize) => changed(ctx, desktop::minimize_window(window), "is minimized already"),
        Some(RootAction::Restore) => changed(ctx, desktop::restore_window(window), "isn't minimized"),
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
    if let Err(refused) = desktop::push_request(window, req) {
        let busy = matches!(refused, Refused::Busy(_));
        // Dropped here, with no desktop lock held: it answers a reply
        // nobody reads.
        drop(refused);
        return if busy {
            ctx.fail(format_args!("ui invoke: window {window} busy (try again)"))
        } else {
            ctx.fail(format_args!("ui invoke: window {window} closed"))
        };
    }
    let wait = || {
        let deadline = crate::time::now_ns().saturating_add(DEADLINE_NS);
        let _ = cell.done.wait_until_or_deadline(deadline, || cell.reply.lock().is_done());
    };
    wait();
    // (Each reply read is bound first: its guard is never held past it.)
    let first = cell.reply.lock().after_first_deadline();
    let asked = match first {
        First::Answered(answer) => Asked::Answered(answer),
        First::Cancelled => Asked::Cancelled,
        First::Running => {
            wait();
            let second = cell.reply.lock().after_second_deadline();
            match second {
                Second::Late(answer) => Asked::Late(answer),
                Second::StillRunning => Asked::StillRunning,
            }
        }
    };
    desktop::request_finished(matches!(asked, Asked::Cancelled), matches!(asked, Asked::Late(_)));
    match asked {
        Asked::Answered(answer) => print_answer(ctx, window, node, answer, false),
        Asked::Late(answer) => print_answer(ctx, window, node, answer, true),
        Asked::Cancelled => {
            ctx.fail(format_args!("ui invoke: cancelled: window {window} didn't take it in 1 s (try again)"))
        }
        Asked::StillRunning => ctx.fail(format_args!("ui invoke: still running (window {window})")),
    }
}

/// How a request's asker came out of its deadlines.
enum Asked {
    Answered(Answer),
    Late(Answer),
    Cancelled,
    StillRunning,
}

fn print_answer(ctx: &mut crate::shell::Ctx<'_>, window: u32, node: u64, answer: Answer, late: bool) {
    if late {
        let _ = writeln!(ctx.out, "ui invoke: ran late");
    }
    match answer {
        Answer::Ran(Served::Row { depth, node }) => match desktop::node_row(window, depth, node) {
            Some(row) => print_rows(ctx, &[row]),
            // Closed between the answer and the row: it ran.
            None => {
                let _ = writeln!(ctx.out, "ui invoke: ran; window {window} closed");
            }
        },
        Answer::Ran(Served::Error(InvokeError::NoNode)) => ctx.fail(format_args!("ui invoke: no node {node}")),
        Answer::Ran(Served::Error(e)) => ctx.fail(format_args!("ui invoke: {e}")),
        Answer::Ran(Served::RanNodeGone) => {
            let _ = writeln!(ctx.out, "ui invoke: ran; node {node} is gone");
        }
        Answer::Ran(Served::ClosedItsWindow) => {
            let _ = writeln!(ctx.out, "ui invoke: ran; window {window} closed");
        }
        Answer::Ran(Served::Failed(why)) => ctx.fail(format_args!("ui invoke: {why}")),
        Answer::WindowClosed => ctx.fail(format_args!("ui invoke: window {window} closed before it ran")),
        Answer::Dropped => ctx.fail(format_args!("ui invoke: dropped while running (window {window}): a bug")),
        Answer::OwnerEnded => {
            ctx.fail(format_args!("ui invoke: window {window} closed while running it (it may have run)"))
        }
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
        // The UI font through lumen-ui's `Text` (`font::Text` at
        // `TEXT_PX`): a longer line measures wider; a line drawn unclipped
        // has its rightmost ink between 4 pixels short of its measured
        // width (the last glyph's right bearing) and 1 past it (rounding);
        // a clip keeps ink inside it.
        use crate::kassert;
        let mut t = crate::font::Text::new().map_err(|_| "no font")?;
        let t: &mut dyn Text = &mut t;
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

crate::ktest!(
    fn a_request_is_answered_only_after_its_effect_is_published() {
        // `Toolkit::step` on a desktop with no screen, with a probe app whose
        // `paint_over` records the reply's phase while the tree is painted:
        // `Running` then (the repaint and publish come first), `Done` once
        // the step returns. A press whose action closes the window comes
        // back unanswered (`Step::Close`), answered by `close` only after the
        // handle is dropped (the window gone).
        use crate::kassert;
        use lumen_ui::{Ui, UiEvent, Widget};
        struct Probe {
            ui: Ui,
            button: Widget,
            cell: Option<Arc<ReplyCell>>,
            seen: Option<Phase>,
            closes: bool,
        }
        impl App for Probe {
            fn ui_mut(&mut self) -> &mut Ui {
                &mut self.ui
            }
            fn apply(&mut self, events: &[UiEvent]) -> Then {
                if self.closes && events.contains(&UiEvent::Pressed(self.button)) { Then::Close } else { Then::Carry }
            }
            fn paint_over(&mut self, _: &mut Surface) {
                if let Some(cell) = &self.cell {
                    self.seen = Some(cell.phase());
                }
            }
        }
        let d = desktop::Desktop::without_screen();
        let mut h = d.open_window("probe", 120, 60).map_err(|_| "a window")?;
        let mut ui = Ui::new();
        let button = ui.button(None, "Go");
        let app = Probe { ui, button, cell: None, seen: None, closes: false };
        let mut tk = Toolkit::new(app, Rect::new(0, 0, 120, 60), h.theme()).map_err(|_| "no font")?;
        while let Some(e) = h.next_event() {
            kassert!(matches!(tk.step(&mut h, e), Step::Carry));
        }
        let id = tk.app_mut().ui.id(button).ok_or("removed")?.get();
        // A press that focuses the button: the tree changes, so it repaints.
        let (req, cell) = Request::new(id, Action::Press);
        tk.app_mut().cell = Some(cell.clone());
        kassert!(matches!(tk.step(&mut h, WindowEvent::Request(req)), Step::Carry));
        kassert!(tk.app_mut().seen == Some(Phase::Running));
        kassert!(cell.phase() == Phase::Done);
        // A press that closes its window.
        tk.app_mut().closes = true;
        let (req, cell) = Request::new(id, Action::Press);
        let Step::Close(Some(started)) = tk.step(&mut h, WindowEvent::Request(req)) else {
            return Err("not closing with its request");
        };
        kassert!(cell.phase() == Phase::Running);
        close(h, Some(started));
        kassert!(matches!(cell.take(), Some(Answer::Ran(Served::ClosedItsWindow))));
        Ok(())
    }
);
