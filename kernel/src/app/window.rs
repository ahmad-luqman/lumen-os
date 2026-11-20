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
//! work. A refused call writes nothing.

use super::host::{C, Res, blocking, copy_in, copy_in_capped, entry, memory_len, write_out};
use super::runtime::{self, AppCtx};
use crate::desktop::{self, Animating, OpenError, Wait, WindowEvent, WindowHandle};
use crate::{sched, time};
use lumen_app_api::event::{self, Event};
use lumen_app_api::layout::{self, CANVAS_SIZE, Canvas, Clipped, RECT_SIZE, TEXT_DRAW_SIZE, TextDraw};
use lumen_app_api::limits::{COMMIT_RECTS, EVENT_BYTES, PIXEL_FUEL, TEXT_BYTES, TEXT_PX_MAX, TEXT_PX_MIN, TITLE_BYTES};
use lumen_app_api::limits::{WINDOW_PIXELS, WINDOWS};
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

/// The event in hand: as the desktop gave it (`window_wait`), or as the
/// app would read it (kept by `BufferTooSmall`; the desktop's is gone once
/// converted, and a request is never kept).
enum Held {
    Desktop(WindowEvent),
    Wire(Event),
}

impl AppWindow {
    /// A waker for its owner's wait (`kill` uses it).
    pub(super) fn waker(&self) -> desktop::Waker {
        self.handle.waker()
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
    text::decode(&bytes, cap).map(alloc::string::String::from)
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
        const _: () = assert!(WINDOWS == 1, "one window per app");
        return Err(Error::Limit);
    }
    let handle = desktop::open_window(&title, w, ht).map_err(open_error)?;
    let scratch = Surface::with_stride(w, ht, w as usize).ok_or(Error::OutOfMemory)?;
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
        copy_rows(mem, &canvas, pixels.start, &clip, win.handle.surface_mut(), (0, 0));
        damage[count] = Rect::new(clip.x as i32, clip.y as i32, clip.w, clip.h);
        count += 1;
        area += clip.area();
    }
    win.handle.commit(&damage[..count]);
    runtime::charge(&mut *c, PIXEL_FUEL * area);
    Ok(0)
}

/// Copy `clip` (inside the canvas, which [`Canvas::pixels`] accepted: its
/// rows start at `start`) of the canvas's pixels into `surface`, its corner
/// at `to` less the clip's (so `(0, 0)` keeps canvas coordinates), the top
/// byte cleared.
fn copy_rows(mem: &[u8], canvas: &Canvas, start: usize, clip: &Clipped, surface: &mut Surface, to: (u32, u32)) {
    for y in clip.y..clip.y + clip.h {
        let from = start + canvas.offset(clip.x, y);
        let src = &mem[from..from + clip.w as usize * 4];
        let row = surface.row_mut(y - to.1);
        let x = (clip.x - to.0) as usize;
        for (d, s) in row[x..x + clip.w as usize].iter_mut().zip(src.as_chunks::<4>().0) {
            *d = u32::from_le_bytes(*s) & 0x00ff_ffff;
        }
    }
}

/// The reverse of [`copy_rows`]: `clip` of `surface` (at `to` less the
/// clip's corner) back into the canvas's pixels.
fn copy_back(mem: &mut [u8], canvas: &Canvas, start: usize, clip: &Clipped, surface: &Surface, to: (u32, u32)) {
    for y in clip.y..clip.y + clip.h {
        let at = start + canvas.offset(clip.x, y);
        let dst = &mut mem[at..at + clip.w as usize * 4];
        let row = surface.row(y - to.1);
        let x = (clip.x - to.0) as usize;
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(&row[x..x + clip.w as usize]) {
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
            Event::PointerDown { x, y, button: event::button(button), buttons: event::button_set(buttons) }
        }
        WindowEvent::PointerUp { x, y, button, buttons } => {
            Event::PointerUp { x, y, button: event::button(button), buttons: event::button_set(buttons) }
        }
        WindowEvent::PointerMove { x, y, buttons } => Event::PointerMove { x, y, buttons: event::button_set(buttons) },
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
        // Every event fits EVENT_BYTES (lumen-app-api's test of the largest).
        let n = wire.encode(&mut bytes).map_err(|_| Error::BufferTooSmall)?.len();
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
        if let Some(d) = until
            && d <= time::now_ns()
        {
            return match win.handle.next_event() {
                Some(e) => {
                    win.in_hand = Some(Held::Desktop(e));
                    1
                }
                None => {
                    sched::yield_now();
                    0
                }
            };
        }
        loop {
            if state.kill_asked() {
                return 0;
            }
            match win.handle.wait_event_until(until, animating) {
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
    Ok(text.measure(&s, px).min(i32::MAX as u32) as i32)
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
    let advance = text.measure(&s, px).min(i32::MAX as u32) as i32;
    // The clip, cut to the canvas, then to the scratch surface (the
    // window's size) from the clip's corner.
    let Some(mut clip) = args.clip.clip(canvas.w, canvas.h) else { return Ok(advance) };
    clip.w = clip.w.min(win.w);
    clip.h = clip.h.min(win.h);
    let to = (clip.x, clip.y);
    copy_rows(mem, &canvas, pixels.start, &clip, &mut win.scratch, to);
    let x = i64::from(args.x) - i64::from(clip.x);
    let y = i64::from(args.y) - i64::from(clip.y);
    let at = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let inside = Rect::new(0, 0, clip.w, clip.h);
    text.draw(&mut win.scratch, at(x), at(y), &s, px, args.rgb & 0x00ff_ffff, inside);
    copy_back(mem, &canvas, pixels.start, &clip, &win.scratch, to);
    let area = clip.area();
    runtime::charge(&mut *c, PIXEL_FUEL * area);
    Ok(advance)
}
