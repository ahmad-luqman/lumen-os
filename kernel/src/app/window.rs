//! The window and text host calls (#206; ARCHITECTURE section 8, "The host
//! interface"): an app's one window, opened on the live desktop and owned
//! by the app's thread (destroyed when its handle closes or the app ends),
//! the app's canvas copied into the window's surface by `window_commit`,
//! events taken one at a time (`window_next_event`, one kept in hand
//! across `window_wait` and `BufferTooSmall`), and text in the shared UI
//! font through the app's own glyph cache.
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
//! rasterizes (a glyph its cache didn't hold), and `TEXT_CHAR_FUEL` per
//! character of each layout pass (`text_measure` one, `text_draw` two).

use super::host::{C, Res, blocking, copy_in, copy_in_capped, entry, memory_len, write_out};
use super::runtime::{self, AppCtx};
use crate::desktop::{self, Animating, OpenError, Wait, WindowEvent, WindowHandle};
use crate::{sched, time};
use lumen_app_api::event::Event;
use lumen_app_api::layout::{self, CANVAS_SIZE, Canvas, CheckedCanvas, Clipped, RECT_SIZE, TEXT_DRAW_SIZE, TextDraw};
use lumen_app_api::limits::{COMMIT_RECTS, EVENT_BYTES, PIXEL_FUEL, TEXT_BYTES, TEXT_PX_MAX, TEXT_PX_MIN, TITLE_BYTES};
use lumen_app_api::limits::{SURFACES, TEXT_CHAR_FUEL, WINDOW_PIXELS};
use lumen_app_api::{Error, Kind, range, returned, text};
use lumen_font::Px;
use lumen_gfx::{Rect, Surface};
use wasmi::Linker;

/// An app's window: the desktop's handle (its owner surface the canvas is
/// copied into), the event in hand, and `text_draw`'s scratch surface.
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
}

/// The event in hand: as the desktop gave it (`window_wait`, which never
/// keeps a request: until #207 it refuses them as it takes them), or as the
/// app would read it (kept by `BufferTooSmall`; the desktop's is gone once
/// converted). A request is never held in either form.
enum Held {
    Desktop(WindowEvent),
    Wire(Event),
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
    c.data_mut().add_window(AppWindow { handle, w, h: ht, in_hand: None, scratch })
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

/// A desktop event as the app gets it, or the request it is: apps publish
/// no tree until #207, so every request names a node they don't have and
/// is refused (`ui::refuse`, "no node"), never handed on.
fn wire_event(e: WindowEvent) -> Result<Event, crate::ui::Request> {
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
            Err(request) => {
                crate::ui::refuse(request);
                continue;
            }
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

/// `window_wait(h, deadline, animating)`: 1 with an event in hand, 0 at the
/// deadline (`time_now`'s clock; -1 none). Blocks (a slice end, `waiting`)
/// in `WindowHandle::wait_event_until`, its deadline moved to
/// `time::now_ns`'s clock; a `Woken` wake re-checks the kill flag and waits
/// again; a deadline already reached looks once, then yields.
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
            // A deadline already reached: a look (again, after a refused
            // request), then the CPU given up.
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
                // Until #207 a request names no node of the app's: refused
                // here, so a 1 always means an event to read.
                Wait::Event(WindowEvent::Request(r)) => crate::ui::refuse(r),
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
