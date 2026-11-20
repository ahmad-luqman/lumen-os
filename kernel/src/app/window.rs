//! The window and text host calls (#206, #207; ARCHITECTURE section 8, "The
//! host interface" and "Apps' automation trees"): an app's one window,
//! opened on the live desktop and owned by the app's thread (destroyed when
//! its handle closes or the app ends), the app's canvas copied into the
//! window's surface by `window_commit`, events taken one at a time
//! (`window_next_event`, one kept in hand across `window_wait` and
//! `BufferTooSmall`), text in the shared UI font through the app's own
//! glyph cache, and its automation tree: published (`window_publish`: each
//! app id mapped to a global `lumen_ui::NodeId`), `ui invoke` requests
//! handed on as the app's ids, one at a time, and answered from the
//! published tree (`window_answer`).
//!
//! Every call goes through `host::entry` and checks its arguments in
//! section 8's order: the handle, then its kind; the fixed structures
//! copied in, then every range they name; then the other values; then the
//! work. A refused call writes nothing. Where a layout's own check comes
//! within that order, section 8's "Fixed layouts" rows say so: a canvas's
//! stride below its width is `BadArgument` before its rows' range, and
//! `window_commit`'s count is checked before its rectangles' range (the
//! count says how long that range is).
//!
//! Fuel, on top of `CALL_FUEL` (section 8's list): `PIXEL_FUEL` per pixel
//! committed, per pixel of `text_draw`'s clip, per pixel of the three
//! surfaces `window_open` fills, `PIXEL_FUEL × px²` per glyph `text_draw`
//! rasterizes (a glyph its cache didn't hold), `TEXT_CHAR_FUEL` per
//! character of each layout pass (`text_measure` one, `text_draw` two),
//! `BYTE_FUEL` per byte of a tree or an answer copied in, and `NODE_FUEL`
//! per node of a tree decoded.

use super::host::{C, Res, blocking, copy_in, copy_in_capped, entry, memory_len, write_out};
use super::runtime::{self, AppCtx};
use crate::desktop::{self, Animating, OpenError, Wait, WindowEvent, WindowHandle};
use crate::ui::{Answer, Request, Started};
use crate::{sched, time};
use alloc::collections::BTreeMap;
use lumen_app_api::answer::AppAnswer;
use lumen_app_api::event::Event;
use lumen_app_api::layout::{self, CANVAS_SIZE, Canvas, CheckedCanvas, Clipped, RECT_SIZE, TEXT_DRAW_SIZE, TextDraw};
use lumen_app_api::limits::{ANSWER_BYTES, BYTE_FUEL, NODE_FUEL, TREE_BYTES};
use lumen_app_api::limits::{COMMIT_RECTS, EVENT_BYTES, PIXEL_FUEL, TEXT_BYTES, TEXT_PX_MAX, TEXT_PX_MIN, TITLE_BYTES};
use lumen_app_api::limits::{SURFACES, TEXT_CHAR_FUEL, WINDOW_PIXELS};
use lumen_app_api::request::{self, Running, Taken};
use lumen_app_api::tree::Tree;
use lumen_app_api::{Error, Kind, range, returned, text};
use lumen_font::Px;
use lumen_gfx::{Rect, Surface};
use lumen_ui::{NodeId, Role, Served};
use wasmi::Linker;

/// An app's window: the desktop's handle (its owner surface the canvas is
/// copied into), the event in hand, `text_draw`'s scratch surface, its
/// tree's ids, and the request it runs. Its fields drop in this order: the
/// window goes first (its queued requests answered "window closed"), then
/// a request the app was running is answered `OwnerEnded`.
pub(super) struct AppWindow {
    handle: WindowHandle,
    w: u32,
    h: u32,
    /// The event `window_wait` woke for, or one `window_next_event` kept
    /// (`BufferTooSmall`): the next `window_next_event` takes it.
    in_hand: Option<Held>,
    /// `text_draw`'s pixels, the window's size: the third surface in the
    /// app's worst case.
    scratch: Surface,
    /// The newest published tree's ids: each app id's global one (an id
    /// first seen gets `NodeId::fresh()`; one missing from a newer tree
    /// leaves, so its global id names nothing again).
    ids: BTreeMap<u64, NodeId>,
    /// The other way: each global id's app id and role.
    nodes: BTreeMap<u64, (u64, Role)>,
    /// The request delivered (or kept in hand) and not yet answered.
    running: OwnerEnded,
}

/// The event in hand: as the desktop gave it (`window_wait`: a request
/// isn't started until `window_next_event` takes it), or as the app reads it
/// (kept by `BufferTooSmall`; the desktop's is gone once converted: a
/// request's `Started` is then in `running`).
enum Held {
    Desktop(WindowEvent),
    Wire(Event),
}

/// A window's running request, answered `OwnerEnded` if it is dropped
/// unanswered: the app ended, or closed its window, while running it
/// (section 8's `Answer::OwnerEnded`).
pub(super) struct OwnerEnded(Running<Started>);

impl Drop for OwnerEnded {
    fn drop(&mut self) {
        if let Some(started) = self.0.take() {
            started.finish(Answer::OwnerEnded);
        }
    }
}

impl AppWindow {
    /// A waker for its owner's wait (`kill` uses it).
    pub(super) fn waker(&self) -> desktop::Waker {
        self.handle.waker()
    }

    /// Its title, as the desktop shows it (for the ktests).
    #[cfg(feature = "ktest")]
    pub(super) fn title(&self) -> &str {
        self.handle.title()
    }

    /// The app id and role of global node `node` in the newest tree.
    fn local(&self, node: u64) -> Option<(u64, Role)> {
        self.nodes.get(&node).copied()
    }

    /// What the asker of a running request on global node `node` gets for
    /// the app's `answer` (from the newest published tree).
    fn served(&self, node: u64, action: lumen_ui::ActionKind, answer: AppAnswer) -> Served {
        match answer {
            AppAnswer::Ran => match self.handle.published_node(node) {
                Some((depth, node)) => Served::Row { depth, node },
                None => Served::RanNodeGone,
            },
            AppAnswer::Refused(r) => {
                let role = self.local(node).map(|(_, role)| role);
                match role {
                    Some(role) => Served::Error(r.invoke_error(role, action)),
                    None => Served::Error(lumen_ui::InvokeError::NoNode),
                }
            }
            AppAnswer::Failed(why) => Served::Failed(alloc::string::ToString::to_string(&text::log_line(why.as_str()))),
            AppAnswer::ClosedItsWindow => Served::ClosedItsWindow,
        }
    }
}

pub(super) fn define(linker: &mut Linker<AppCtx>) -> Result<(), wasmi::Error> {
    let m = lumen_app_api::host::MODULE;
    linker.func_wrap(
        m,
        "window_open",
        |mut c: C<'_>, h: i32, title: i32, len: i32, width: i32, height: i32| -> Res<i32> {
            entry(&mut c, |c| Ok(returned(window_open(c, h, title, len, width, height))))
        },
    )?;
    linker.func_wrap(m, "window_commit", |mut c: C<'_>, h: i32, canvas: i32, rects: i32, n: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(window_commit(c, h, canvas, rects, n))))
    })?;
    linker.func_wrap(m, "window_next_event", |mut c: C<'_>, h: i32, buf: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(next_event(c, h, buf, len))))
    })?;
    linker.func_wrap(m, "window_wait", |mut c: C<'_>, h: i32, deadline: i64, animating: i32| -> Res<i32> {
        entry(&mut c, |c| window_wait(c, h, deadline, animating))
    })?;
    linker.func_wrap(m, "window_publish", |mut c: C<'_>, h: i32, tree: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(window_publish(c, h, tree, len))))
    })?;
    linker.func_wrap(m, "window_answer", |mut c: C<'_>, h: i32, answer: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(window_answer(c, h, answer, len))))
    })?;
    linker.func_wrap(m, "text_line_height", |mut c: C<'_>, h: i32, px: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(line_height(c, h, px))))
    })?;
    linker.func_wrap(m, "text_measure", |mut c: C<'_>, h: i32, px: i32, text: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(measure(c, h, px, text, len))))
    })?;
    linker.func_wrap(
        m,
        "text_draw",
        |mut c: C<'_>, h: i32, canvas: i32, args: i32, text: i32, len: i32| -> Res<i32> {
            entry(&mut c, |c| Ok(returned(text_draw(c, h, canvas, args, text, len))))
        },
    )?;
    Ok(())
}

/// The `N` bytes of a fixed structure at `ptr`, copied in by value (a range
/// outside the memory is `BadPointer`).
fn fixed<const N: usize>(c: &C<'_>, ptr: i32) -> Result<[u8; N], Error> {
    let r = range::range(ptr, N as i32, memory_len(c))?;
    let mut out = [0; N];
    out.copy_from_slice(&copy_in(c, r));
    Ok(out)
}

/// A text size: `TEXT_PX_MIN..=TEXT_PX_MAX`, else `BadArgument`.
fn text_px(px: i32) -> Result<Px, Error> {
    let px = u32::try_from(px).ok().filter(|p| (TEXT_PX_MIN..=TEXT_PX_MAX).contains(p)).ok_or(Error::BadArgument)?;
    Px::new(px as u16).ok_or(Error::BadArgument)
}

/// Text at `r` (a range already checked): its cap (before the copy), then
/// decoded (bad UTF-8 is `BadArgument`).
fn text_in(c: &C<'_>, r: core::ops::Range<usize>, cap: usize) -> Result<alloc::string::String, Error> {
    let bytes = copy_in_capped(c, r, cap)?;
    alloc::string::String::from_utf8(bytes).map_err(|_| Error::BadArgument)
}

/// A text's advance as a host call returns it (an `i32`).
fn advance(text: &crate::font::Text, s: &str, px: Px) -> i32 {
    text.measure(s, px).min(i32::MAX as u32) as i32
}

/// What the desktop's refusal to open a window is to the app.
fn open_error(e: OpenError) -> Error {
    match e {
        OpenError::NoDesktop => Error::Unavailable,
        OpenError::TooMany => Error::Limit,
        OpenError::TooBig => Error::BadArgument,
        OpenError::OutOfMemory => Error::OutOfMemory,
    }
}

fn window_open(c: &mut C<'_>, h: i32, title: i32, len: i32, width: i32, height: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Desktop)?;
    let r = range::range(title, len, memory_len(c))?;
    let (Ok(w), Ok(ht)) = (u32::try_from(width), u32::try_from(height)) else { return Err(Error::BadArgument) };
    if w == 0 || ht == 0 || u64::from(w) * u64::from(ht) > WINDOW_PIXELS {
        return Err(Error::BadArgument);
    }
    // Shown in the title bar, the taskbar and `windows`: cut and sanitised.
    let title = text::cut(&text_in(c, r, TITLE_BYTES)?, TITLE_BYTES);
    if c.data().has_window() {
        return Err(Error::Limit);
    }
    let handle = c.data().open_window(&title, w, ht).map_err(open_error)?;
    let scratch = Surface::with_stride(w, ht, w as usize).ok_or(Error::OutOfMemory)?;
    // Its three surfaces, each allocated and filled: paid by the pixel.
    runtime::charge(&mut *c, PIXEL_FUEL * SURFACES * u64::from(w) * u64::from(ht));
    // `window_answer` answers *the* running request.
    handle.one_request_at_a_time();
    let window = AppWindow {
        handle,
        w,
        h: ht,
        in_hand: None,
        scratch,
        ids: BTreeMap::new(),
        nodes: BTreeMap::new(),
        running: OwnerEnded(Running::new()),
    };
    c.data_mut().add_window(window)
}

fn window_commit(c: &mut C<'_>, h: i32, canvas: i32, rects: i32, n: i32) -> Result<i32, Error> {
    let (w, ht) = {
        let win = c.data_mut().window_mut(h)?;
        (win.w, win.h)
    };
    let canvas = Canvas::decode(&fixed::<CANVAS_SIZE>(c, canvas)?);
    let rect_bytes = layout::commit_count(n)?;
    let r = range::range(rects, rect_bytes as i32, memory_len(c))?;
    let raw = copy_in(c, r);
    let pixels = canvas.pixels(memory_len(c))?;
    if (canvas.w, canvas.h) != (w, ht) {
        return Err(Error::BadArgument);
    }
    let mut damage = [Rect::new(0, 0, 0, 0); COMMIT_RECTS];
    let mut count = 0;
    let mut area = 0;
    let memory = c.data().memory.ok_or(Error::BadPointer)?;
    let (mem, ctx) = memory.data_and_store_mut(&mut *c);
    let win = ctx.window_mut(h)?;
    for b in raw.as_chunks::<RECT_SIZE>().0 {
        let Some(clip) = layout::Rect::decode(b).clip(w, ht) else { continue };
        copy_rows(mem, &pixels, &clip, win.handle.surface_mut(), (0, 0));
        damage[count] = Rect::new(clip.x() as i32, clip.y() as i32, clip.w(), clip.h());
        count += 1;
        area += clip.area();
    }
    win.handle.commit(&damage[..count]);
    runtime::charge(&mut *c, PIXEL_FUEL * area);
    Ok(0)
}

/// A kernel bug in the window calls: one ERROR, then a crash report (debug
/// assertions are on in every built kernel). In a kernel built without
/// them the call goes on, touching nothing.
fn kernel_bug(what: &str) {
    log::error!(target: "app", "{what} (a kernel bug)");
    debug_assert!(false, "{what}");
}

/// The rows of `clip`, a clip of `canvas` that, moved by `to` less its
/// corner, must also fit `surface`: checked on both sides before a pixel is
/// touched, either failing [`kernel_bug`] (nothing copied).
fn rows_between(
    canvas: &CheckedCanvas,
    clip: &Clipped,
    surface: &Surface,
    to: (u32, u32),
) -> Option<impl Iterator<Item = (u32, core::ops::Range<usize>)> + use<>> {
    let fits = clip.x() >= to.0
        && clip.y() >= to.1
        && u64::from(clip.x() - to.0) + u64::from(clip.w()) <= u64::from(surface.width())
        && u64::from(clip.y() - to.1) + u64::from(clip.h()) <= u64::from(surface.height());
    if !fits {
        kernel_bug("a clip that doesn't fit its surface");
        return None;
    }
    let rows = canvas.rows(clip);
    if rows.is_none() {
        kernel_bug("a clip outside its canvas");
    }
    rows
}

/// Copy `clip` of a checked canvas's pixels into `surface`, its corner at
/// `to` less the clip's (so `(0, 0)` keeps canvas coordinates), the top
/// byte cleared. Both sides are checked first ([`rows_between`]).
fn copy_rows(mem: &[u8], canvas: &CheckedCanvas, clip: &Clipped, surface: &mut Surface, to: (u32, u32)) {
    let Some(rows) = rows_between(canvas, clip, surface, to) else { return };
    let x = (clip.x() - to.0) as usize;
    for (y, bytes) in rows {
        let row = surface.row_mut(y - to.1);
        for (d, s) in row[x..x + clip.w() as usize].iter_mut().zip(mem[bytes].as_chunks::<4>().0) {
            *d = u32::from_le_bytes(*s) & 0x00ff_ffff;
        }
    }
}

/// The reverse of [`copy_rows`]: `clip` of `surface` (at `to` less the
/// clip's corner) back into the canvas's pixels.
fn copy_back(mem: &mut [u8], canvas: &CheckedCanvas, clip: &Clipped, surface: &Surface, to: (u32, u32)) {
    let Some(rows) = rows_between(canvas, clip, surface, to) else { return };
    let x = (clip.x() - to.0) as usize;
    for (y, bytes) in rows {
        let row = surface.row(y - to.1);
        for (d, s) in mem[bytes].as_chunks_mut::<4>().0.iter_mut().zip(&row[x..x + clip.w() as usize]) {
            d.copy_from_slice(&s.to_le_bytes());
        }
    }
}

/// A desktop event as the app gets it, or the request it is.
fn wire_event(e: WindowEvent) -> Result<Event, Request> {
    Ok(match e {
        WindowEvent::Theme(t) => Event::theme(t.id, &crate::ui::palette(t)),
        WindowEvent::Key(k) => Event::key(&k),
        WindowEvent::PointerDown { x, y, button, buttons } => {
            Event::PointerDown { x, y, button: button.into(), buttons: buttons.into() }
        }
        WindowEvent::PointerUp { x, y, button, buttons } => {
            Event::PointerUp { x, y, button: button.into(), buttons: buttons.into() }
        }
        WindowEvent::PointerMove { x, y, buttons } => Event::PointerMove { x, y, buttons: buttons.into() },
        WindowEvent::FocusGained => Event::FocusGained,
        WindowEvent::FocusLost => Event::FocusLost,
        WindowEvent::Lost => Event::Lost,
        WindowEvent::Close => Event::Close,
        WindowEvent::Request(r) => return Err(r),
    })
}

/// Take a request from the window's queue (`lumen_app_api::request::take`):
/// skipped if its asker cancelled it; answered at once if its node isn't
/// in the newest tree or its role doesn't take the action (or its text is
/// too long); otherwise started (running) and the event the app gets.
fn take_request(win: &mut AppWindow, req: Request) -> Option<Event> {
    let (node, action) = (req.node(), req.action().clone());
    let local = win.local(node);
    match request::take(req.start().ok(), local, &action) {
        Taken::Skipped => None,
        Taken::Answer { started, error } => {
            started.finish(Answer::Ran(Served::Error(error)));
            None
        }
        Taken::Deliver { started, event } => {
            win.running.0.start(started);
            Some(event)
        }
    }
}

fn next_event(c: &mut C<'_>, h: i32, buf: i32, len: i32) -> Result<i32, Error> {
    c.data_mut().window_mut(h)?;
    let out = range::range(buf, len, memory_len(c))?;
    let mut bytes = [0u8; EVENT_BYTES];
    loop {
        let ctx = c.data_mut();
        let win = ctx.window_mut(h)?;
        let next = match win.in_hand.take() {
            Some(Held::Wire(e)) => Ok(e),
            Some(Held::Desktop(e)) => wire_event(e),
            None => match win.handle.next_event() {
                Some(e) => wire_event(e),
                None => return Ok(0),
            },
        };
        let wire = match next {
            Ok(wire) => wire,
            Err(request) => match take_request(win, request) {
                Some(event) => event,
                None => continue,
            },
        };
        // Encoded first, into a buffer as big as any event (lumen-app-api
        // tests the largest; `SetValue` holds every request's text to its
        // cap): a failure is a kernel bug (an ERROR, then a crash report),
        // never reported to the app as something it isn't.
        let n = match wire.encode(&mut bytes) {
            Ok(encoded) => encoded.len(),
            Err(e) => {
                kernel_bug(&alloc::format!("an event for app {} didn't encode ({e:?})", ctx.state.id));
                continue;
            }
        };
        if n > out.len() {
            // (A request's `Started` stays in `running` meanwhile.)
            win.in_hand = Some(Held::Wire(wire));
            return Err(Error::BufferTooSmall);
        }
        if wire == Event::Close {
            ctx.given_close();
        }
        write_out(c, out.start, &bytes[..n]);
        return Ok(n as i32);
    }
}

/// Whether the window's running request has been read by the app (not in
/// hand): only then may it answer.
fn delivered(win: &AppWindow) -> bool {
    win.running.0.is_running() && !matches!(win.in_hand, Some(Held::Wire(Event::Request { .. })))
}

/// `window_publish(h, tree, len)`: the app's automation tree, its size
/// checked before it is copied (`TREE_BYTES`), decoded node by node and
/// checked (`Tree::decode`), each app id mapped to a global one, then
/// swapped in (`ui tree` reads it). A refused tree keeps the last.
fn window_publish(c: &mut C<'_>, h: i32, ptr: i32, len: i32) -> Result<i32, Error> {
    c.data_mut().window_mut(h)?;
    let r = range::range(ptr, len, memory_len(c))?;
    let bytes = copy_in_capped(c, r, TREE_BYTES)?;
    let decoded = Tree::decode(&bytes);
    let nodes = decoded.as_ref().map_or(0, |t| t.count()) as u64;
    // Paid for what was copied and decoded, refused or not.
    runtime::charge(&mut *c, BYTE_FUEL * bytes.len() as u64 + NODE_FUEL * nodes);
    let tree = decoded.map_err(|e| {
        log::debug!(target: "app", "app {}: a tree of {} bytes refused: {e}", c.data().state.id, bytes.len());
        Error::BadArgument
    })?;
    let win = c.data_mut().window_mut(h)?;
    let old = core::mem::take(&mut win.ids);
    let mut ids = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    let (top, rows) = tree.rows(|local, role| {
        let global = old.get(&local).copied().unwrap_or_else(NodeId::fresh);
        ids.insert(local, global);
        nodes.insert(global.get(), (local, role));
        global
    });
    win.ids = ids;
    win.nodes = nodes;
    win.handle.publish(top, rows);
    Ok(0)
}

/// `window_answer(h, answer, len)`: the app's answer to its running
/// request (`BadArgument` with none read, past `ANSWER_BYTES` before the
/// copy, or one that doesn't decode: it stays running), built into the
/// asker's answer from the newest published tree. `ClosedItsWindow` closes
/// the window first, then answers.
fn window_answer(c: &mut C<'_>, h: i32, ptr: i32, len: i32) -> Result<i32, Error> {
    c.data_mut().window_mut(h)?;
    let r = range::range(ptr, len, memory_len(c))?;
    if !delivered(c.data_mut().window_mut(h)?) {
        return Err(Error::BadArgument);
    }
    let bytes = copy_in_capped(c, r, ANSWER_BYTES)?;
    runtime::charge(&mut *c, BYTE_FUEL * bytes.len() as u64);
    let win = c.data_mut().window_mut(h)?;
    let (started, answer) = win.running.0.answer(&bytes).map_err(|_| Error::BadArgument)?;
    let served = win.served(started.node(), started.action().kind(), answer);
    if served == Served::ClosedItsWindow {
        // The window goes first: `ui invoke`'s answer comes once it is gone.
        c.data_mut().close(h)?;
    }
    started.finish(Answer::Ran(served));
    Ok(0)
}

/// `window_wait(h, deadline, animating)`: 1 with an event in hand, 0 at the
/// deadline (`time_now`'s clock; -1 none). Blocks (a slice end, `waiting`)
/// in `WindowHandle::wait_event_until`, its deadline moved to
/// `time::now_ns`'s clock; a `Woken` wake re-checks the kill flag and waits
/// again; a deadline already reached looks once, then yields. A request it
/// wakes for is an event in hand like any (from #207 a 1 may be followed
/// by a `window_next_event` of 0: the request skipped, or answered without
/// the app).
fn window_wait(c: &mut C<'_>, h: i32, deadline: i64, animating: i32) -> Res<i32> {
    let in_hand = match c.data_mut().window_mut(h) {
        Ok(win) => win.in_hand.is_some(),
        Err(e) => return Ok(e.returned()),
    };
    let animating = match animating {
        0 => Animating::No,
        1 => Animating::Yes,
        _ => return Ok(Error::BadArgument.returned()),
    };
    if deadline < -1 || (deadline == -1 && animating == Animating::Yes) {
        return Ok(Error::BadArgument.returned());
    }
    if in_hand {
        return Ok(1);
    }
    let until = u64::try_from(deadline).ok().map(|d| {
        let boot = time::since_boot_ns().unwrap_or(0);
        time::now_ns().saturating_add(d.saturating_sub(boot))
    });
    blocking(c, "window_wait", |ctx, state| {
        let Ok(win) = ctx.window_mut(h) else { return 0 };
        loop {
            if state.kill_asked() {
                return 0;
            }
            // A deadline already reached: a look, then the CPU given up.
            let next = match until {
                Some(d) if d <= time::now_ns() => match win.handle.next_event() {
                    Some(e) => Wait::Event(e),
                    None => {
                        sched::yield_now();
                        return 0;
                    }
                },
                _ => win.handle.wait_event_until(until, animating),
            };
            match next {
                // A request is kept as it came (started only when
                // `window_next_event` takes it): if it is then skipped or
                // answered without the app, that call returns 0.
                Wait::Event(e) => {
                    win.in_hand = Some(Held::Desktop(e));
                    return 1;
                }
                Wait::Woken => {}
                Wait::Deadline => return 0,
            }
        }
    })
}

fn line_height(c: &mut C<'_>, h: i32, px: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Desktop)?;
    let px = text_px(px)?;
    let (text, _) = c.data_mut().text_and_window()?;
    Ok(text.line_height(px) as i32)
}

fn measure(c: &mut C<'_>, h: i32, px: i32, ptr: i32, len: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Desktop)?;
    let r = range::range(ptr, len, memory_len(c))?;
    let px = text_px(px)?;
    let s = text_in(c, r, TEXT_BYTES)?;
    let (text, _) = c.data_mut().text_and_window()?;
    let advance = advance(text, &s, px);
    // One layout pass over every character.
    runtime::charge(&mut *c, TEXT_CHAR_FUEL * s.chars().count() as u64);
    Ok(advance)
}

fn text_draw(c: &mut C<'_>, h: i32, canvas: i32, args: i32, ptr: i32, len: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Desktop)?;
    let canvas = Canvas::decode(&fixed::<CANVAS_SIZE>(c, canvas)?);
    let args = TextDraw::decode(&fixed::<TEXT_DRAW_SIZE>(c, args)?);
    let r = range::range(ptr, len, memory_len(c))?;
    let pixels = canvas.pixels(memory_len(c))?;
    let px = text_px(args.px as i32)?;
    let s = text_in(c, r, TEXT_BYTES)?;
    let memory = c.data().memory.ok_or(Error::BadPointer)?;
    let (mem, ctx) = memory.data_and_store_mut(&mut *c);
    let (text, window) = ctx.text_and_window()?;
    let win = window.ok_or(Error::Unavailable)?;
    let advance = advance(text, &s, px);
    let chars = s.chars().count() as u64;
    // The clip, cut to the canvas, then to the scratch surface (the
    // window's size) from the clip's corner.
    let Some(clip) = args.clip.clip(canvas.w, canvas.h).and_then(|c| c.shrink_to(win.w, win.h)) else {
        // Only the advance's layout pass ran.
        runtime::charge(&mut *c, TEXT_CHAR_FUEL * chars);
        return Ok(advance);
    };
    let to = (clip.x(), clip.y());
    copy_rows(mem, &pixels, &clip, &mut win.scratch, to);
    let x = i64::from(args.x) - i64::from(clip.x());
    let y = i64::from(args.y) - i64::from(clip.y());
    let at = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let inside = Rect::new(0, 0, clip.w(), clip.h());
    // Only the glyphs reaching the clip are rasterized (`font::Text::draw`).
    let glyphs = text.draw(&mut win.scratch, at(x), at(y), &s, px, args.rgb & 0x00ff_ffff, inside);
    copy_back(mem, &pixels, &clip, &win.scratch, to);
    // Two layout passes over every character (the advance, the drawing),
    // the clip's pixels, and each glyph rasterized at px² of them.
    let px2 = u64::from(px.get()) * u64::from(px.get());
    runtime::charge(&mut *c, 2 * TEXT_CHAR_FUEL * chars + PIXEL_FUEL * (clip.area() + glyphs * px2));
    Ok(advance)
}

/// An app window over `handle` with one published node: app id 1, a
/// button, global id `global` (for the ktests, which have no module).
#[cfg(feature = "ktest")]
fn test_window(handle: WindowHandle, global: NodeId) -> AppWindow {
    handle.one_request_at_a_time();
    let mut win = AppWindow {
        handle,
        w: 8,
        h: 8,
        in_hand: None,
        scratch: Surface::new(8, 8),
        ids: BTreeMap::new(),
        nodes: BTreeMap::new(),
        running: OwnerEnded(Running::new()),
    };
    win.ids.insert(1, global);
    win.nodes.insert(global.get(), (1, Role::Button));
    let node = lumen_ui::AutomationNode::button(global, "Go", Rect::new(0, 0, 4, 4), false, false);
    win.handle.publish(alloc::vec![global], alloc::vec![(1, node)]);
    win
}

/// The next request in `win`'s queue, taken as `window_next_event` takes
/// it: the event the app would get, if any.
#[cfg(feature = "ktest")]
fn take_next(win: &mut AppWindow) -> Option<Event> {
    while let Some(e) = win.handle.next_event() {
        if let WindowEvent::Request(r) = e {
            return take_request(win, r);
        }
    }
    None
}

crate::ktest!(
    fn one_request_per_window_is_refused_busy_until_answered() {
        // An app's window holds one request queued or running: a second is
        // refused busy before it is queued, while the first waits and while
        // it runs; the slot frees once it is answered (here, "no node"
        // without the app, then a delivered one answered).
        use crate::kassert;
        use lumen_ui::Action;
        let d = desktop::Desktop::without_screen();
        let global = NodeId::fresh();
        let mut win = test_window(d.open_window("probe", 8, 8).map_err(|_| "a window")?, global);
        let id = win.handle.id();
        let busy = |d: &desktop::Desktop| {
            let (req, _) = Request::new(global.get(), Action::Press);
            matches!(d.push_request(id, req), Err(lumen_wm::Refused::Busy(_)))
        };
        // Queued: a second is busy. A request for a node the app doesn't
        // have is answered "no node" without the app, which frees the slot.
        let (req, cell) = Request::new(12345, Action::Press);
        kassert!(d.push_request(id, req).is_ok());
        kassert!(busy(&d));
        kassert!(take_next(&mut win).is_none());
        kassert!(matches!(cell.take(), Some(Answer::Ran(Served::Error(lumen_ui::InvokeError::NoNode)))));
        // Delivered, as the app's id 1: running, still busy.
        let (req, cell) = Request::new(global.get(), Action::Press);
        kassert!(d.push_request(id, req).is_ok());
        let got = take_next(&mut win);
        kassert!(matches!(got, Some(Event::Request { node: 1, .. })));
        kassert!(delivered(&win));
        kassert!(busy(&d));
        // Answered: the row from the published tree; the slot free, so a
        // root action can be asked (never reaching the app: NotAllowed on
        // its button).
        let (started, answer) = win.running.0.answer(&[0]).map_err(|_| "an answer")?;
        let served = win.served(started.node(), started.action().kind(), answer);
        kassert!(matches!(&served, Served::Row { depth: 1, node } if node.id() == global));
        started.finish(Answer::Ran(served));
        kassert!(matches!(cell.take(), Some(Answer::Ran(Served::Row { .. }))));
        let (req, cell) = Request::new(global.get(), Action::Close);
        kassert!(d.push_request(id, req).is_ok());
        kassert!(take_next(&mut win).is_none());
        let not_allowed = lumen_ui::InvokeError::NotAllowed(Role::Button, lumen_ui::ActionKind::Close);
        kassert!(matches!(cell.take(), Some(Answer::Ran(Served::Error(e))) if e == not_allowed));
        // Refused answers name the role and the action; a failure is cut
        // and sanitised.
        use lumen_app_api::answer::Refusal;
        let refused = win.served(global.get(), lumen_ui::ActionKind::Press, AppAnswer::Refused(Refusal::NotAllowed));
        let press = lumen_ui::InvokeError::NotAllowed(Role::Button, lumen_ui::ActionKind::Press);
        kassert!(matches!(refused, Served::Error(e) if e == press));
        let failed = AppAnswer::Failed(lumen_app_api::wire::Capped::try_from("a\nb").map_err(|_| "text")?);
        kassert!(
            matches!(win.served(global.get(), lumen_ui::ActionKind::Press, failed), Served::Failed(t) if t == "a\u{fffd}b")
        );
        kassert!(matches!(win.served(999, lumen_ui::ActionKind::Press, AppAnswer::Ran), Served::RanNodeGone));
        // Answered at once: the slot is free again (the probe queues one).
        kassert!(!busy(&d));
        Ok(())
    }
);

crate::ktest!(
    fn an_app_ending_answers_owner_ended() {
        // A request the app was running when it ended (its window dropped):
        // answered `OwnerEnded` (the ending path's own drop), after its
        // window is gone; a request still queued is answered "window
        // closed".
        use crate::kassert;
        use lumen_ui::Action;
        let d = desktop::Desktop::without_screen();
        let global = NodeId::fresh();
        let mut win = test_window(d.open_window("probe", 8, 8).map_err(|_| "a window")?, global);
        let id = win.handle.id();
        let (req, running) = Request::new(global.get(), Action::Press);
        kassert!(d.push_request(id, req).is_ok());
        kassert!(take_next(&mut win).is_some());
        kassert!(running.phase() == lumen_ui::Phase::Running);
        drop(win);
        kassert!(matches!(running.take(), Some(Answer::OwnerEnded)));
        let (req, _) = Request::new(global.get(), Action::Press);
        // Its window is gone.
        kassert!(matches!(d.push_request(id, req), Err(lumen_wm::Refused::Dead(_))));
        Ok(())
    }
);
