//! Terminal windows (#155; ARCHITECTURE section 11): the M2 terminal and
//! shell in desktop windows, several at once. `terminal` (from any shell)
//! opens one while the desktop runs.
//!
//! - **Two threads per window.** Its owner, `terminal`, owns the window
//!   ([`WindowHandle`]) and draws the grid onto the window's surface with
//!   the text console's renderer (`lumen_term::render::Renderer` and
//!   `term::Cells`, so a cell looks as it does on the console), committing
//!   only what changed. Its shell, `term-shell`, runs the screen shells'
//!   line editor and commands on a [`WindowConsole`]. They share a
//!   [`Shared`]: the `lumen_term::Terminal` (with its scrollback) behind a
//!   lock, which the shell's output feeds and the owner draws, and the keys
//!   the owner took from its window's events for the shell.
//! - **Keys** are the window's key presses (the compositor sends them to the
//!   focused window) through `lumen_shell::Key::from_press`, the screen
//!   shells' mapping (section 7); Shift+PageUp/PageDown scroll the window's
//!   view back and forward by half a screen, as on a virtual terminal.
//! - **The cursor** blinks only in the focused window, shown solid at once
//!   by output; an unfocused window's is drawn hollow and steady, so its
//!   owner waits with no deadline (a focused one never settles: section
//!   11 accepts that blinking windows don't).
//! - **No canvas:** a command that would draw on the screen (`mandelbrot`,
//!   `life`, a script's drawing) is refused in a terminal window
//!   (`term::CanvasError::InWindow`); `top` writes text, and runs.
//! - **Closing** (the close box): the owner tells the shell (its console
//!   closes: every key asked is Ctrl-C, and the shell ends after the key or
//!   command under way), closes the window at once, joins the shell, then
//!   frees the terminal and ends. A command that never asks for a key keeps
//!   the shell (and its thread) until it returns.
//! - `term text <window>` prints a terminal window's view as `term text`
//!   prints the screen's ([`view_text`]).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use lumen_gfx::{Damage, Rect, Surface};
use lumen_input::{KeyAction, KeyCode, KeyEvent};
use lumen_shell::Key;
use lumen_term::Terminal;
use lumen_term::render::{CursorLook, Renderer};
use noto_sans_mono_bitmap::{get_raster, get_raster_width};

use crate::desktop::{self, Waker, WindowEvent, WindowHandle};
use crate::fb;
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::term::{self, CELL_H, Cells, Ink, Pixels, ViewText};

/// A terminal window's grid.
pub const COLS: usize = 80;
pub const ROWS: usize = 24;
/// Lines kept above its screen.
pub const SCROLLBACK: usize = 100;
/// Pixels between the client area's edge and the grid.
const PAD: usize = 4;
/// How long the focused window's cursor stays on, then off.
const BLINK_NS: u64 = 500_000_000;
/// Where the first terminal window's frame goes; the next ones alternate
/// left and right, a step lower each pair.
const FIRST_AT: (i32, i32) = (24, 56);
const STEP: (i32, i32) = (640, 32);

/// What a window's owner and shell share.
struct Shared {
    /// The window's id (`windows`, `term text <window>`).
    id: u32,
    /// The grid, fed by the shell's output and drawn by the owner.
    term: SpinLock<Terminal>,
    /// Keys for the shell, oldest first.
    keys: SpinLock<VecDeque<Key>>,
    /// The shell waits here for a key (or the close).
    key_wait: WaitQueue,
    /// The window closed: the shell ends.
    closed: AtomicBool,
    /// Output since the owner last drew (it shows the cursor at once).
    output: AtomicBool,
    /// Wakes the owner for output.
    owner: Waker,
}

/// The terminal windows open, for `term text <window>` (weak: an entry
/// whose window closed names nothing, and is dropped when the next opens).
static OPEN: SpinLock<Vec<Weak<Shared>>> = SpinLock::new(Vec::new());
/// Terminal windows opened in this boot (their titles' numbers).
static OPENED: AtomicU32 = AtomicU32::new(0);

/// A terminal window's client size in pixels.
fn client_size() -> (u32, u32) {
    ((COLS * term::cell_width() + 2 * PAD) as u32, (ROWS * CELL_H + 2 * PAD) as u32)
}

/// The `n`th (from 1) terminal window's frame corner.
fn place(n: u32) -> (i32, i32) {
    let k = (n.saturating_sub(1) % 16) as i32;
    (FIRST_AT.0 + (k % 2) * STEP.0, FIRST_AT.1 + (k / 2) * STEP.1)
}

/// `terminal`: open a terminal window titled `Terminal-<n>`, with its owner
/// and shell; its window id.
pub fn open() -> Result<u32, String> {
    let n = OPENED.fetch_add(1, Ordering::Relaxed) + 1;
    let title = format!("Terminal-{n}");
    let (w, h) = client_size();
    let (x, y) = place(n);
    let handle = desktop::open_window_at(&title, w, h, x, y).map_err(|e| match e {
        desktop::OpenError::NoDesktop => String::from("no desktop (`desktop` starts it)"),
        e => format!("no window: {e}"),
    })?;
    let id = handle.id();
    let shared = Arc::new(Shared {
        id,
        term: SpinLock::new(Terminal::new(COLS, ROWS, SCROLLBACK)),
        keys: SpinLock::new(VecDeque::new()),
        key_wait: WaitQueue::new(),
        closed: AtomicBool::new(false),
        output: AtomicBool::new(false),
        owner: handle.waker(),
    });
    {
        let mut open = OPEN.lock();
        open.retain(|w| w.strong_count() > 0);
        open.push(Arc::downgrade(&shared));
    }
    // The shell first (its prompt waits in the grid for the owner's first
    // drawing); a failure drops the handle, which closes the window.
    let console = WindowConsole { shared: shared.clone() };
    let shell = sched::spawn("term-shell", move || crate::shell::run_in_window(console, COLS))
        .map_err(|e| format!("no thread for its shell: {e}"))?;
    let for_owner = shared.clone();
    if let Err(e) = sched::spawn("terminal", move || owner(handle, for_owner, shell)) {
        // The window is gone with the closure; end the shell (detached
        // with it), which then frees what it holds.
        shared.closed.store(true, Ordering::Release);
        shared.key_wait.wake_all();
        return Err(format!("no thread for its owner: {e}"));
    }
    Ok(id)
}

/// A terminal window's owner: draw the grid, hand the keys to the shell,
/// blink the cursor while focused, until the close box; then end the shell
/// and free the window.
fn owner(mut h: WindowHandle, shared: Arc<Shared>, shell: sched::JoinHandle) {
    let cell_w = term::cell_width();
    let mut renderer = Renderer::new(COLS, ROWS);
    let (mut focused, mut blink_on, mut next_blink) = (false, true, 0);
    {
        // Everything once: the background, then every cell.
        let bounds = h.surface_mut().bounds();
        h.surface_mut().fill(fb::BG);
        renderer.forget(&mut shared.term.lock());
        draw(&mut h, &shared, &mut renderer, cell_w, None);
        h.commit(&[bounds]);
    }
    loop {
        let event = h.wait_event_until(focused.then_some(next_blink));
        let now = crate::time::now_ns();
        match event {
            Some(WindowEvent::Close) => break,
            Some(WindowEvent::Key(k)) => take_key(&shared, k),
            Some(WindowEvent::FocusGained) => {
                (focused, blink_on, next_blink) = (true, true, now + BLINK_NS);
            }
            Some(WindowEvent::FocusLost) => focused = false,
            Some(WindowEvent::Request(r)) => match r {},
            // Pointer events (no mouse in a terminal yet) and `Lost` (no key
            // is held here: each press is taken whole).
            Some(_) => {}
            None => {}
        }
        if shared.output.swap(false, Ordering::AcqRel) {
            (blink_on, next_blink) = (true, now + BLINK_NS);
        } else if focused && now >= next_blink {
            (blink_on, next_blink) = (!blink_on, now + BLINK_NS);
        }
        let look = if focused { blink_on.then_some(CursorLook::Solid) } else { Some(CursorLook::Hollow) };
        draw(&mut h, &shared, &mut renderer, cell_w, look);
    }
    shared.closed.store(true, Ordering::Release);
    shared.key_wait.wake_all();
    // The window goes now; the shell may still finish a command.
    drop(h);
    shell.join();
    let id = shared.id;
    drop(shared);
    log::debug!(target: "termwin", "terminal window {id} closed, its shell joined");
}

/// A key press from the window: Shift+PageUp/PageDown scroll its view,
/// any other the line editor takes goes to the shell (bringing the view
/// back to the bottom).
fn take_key(shared: &Shared, k: KeyEvent) {
    let KeyEvent { code, action: KeyAction::Press { text }, mods, .. } = k else { return };
    match code {
        KeyCode::PageUp | KeyCode::PageDown if mods.is_shifted() => {
            let pages: isize = if code == KeyCode::PageUp { 1 } else { -1 };
            let mut t = shared.term.lock();
            let half = (t.rows() as isize / 2).max(1);
            let _ = t.scroll_view(pages * half);
        }
        _ => {
            let Some(key) = Key::from_press(text, Some(code), mods) else { return };
            let _ = shared.term.lock().reset_view();
            shared.keys.lock().push_back(key);
            shared.key_wait.wake_all();
        }
    }
}

/// Draw what changed in the grid onto the window's surface, then commit
/// it.
fn draw(h: &mut WindowHandle, shared: &Shared, renderer: &mut Renderer, cell_w: usize, look: Option<CursorLook>) {
    let damage = {
        let mut px = OnSurface { s: h.surface_mut(), damage: Damage::new() };
        let mut cells = Cells { px: &mut px, x0: PAD, y0: PAD, cell_w };
        renderer.render(&mut shared.term.lock(), &mut cells, look);
        px.damage
    };
    if !damage.is_empty() {
        h.commit(damage.rects());
    }
}

/// A window's surface as `term::Pixels`, noting what is drawn.
struct OnSurface<'a> {
    s: &'a mut Surface,
    damage: Damage<16>,
}

impl OnSurface<'_> {
    fn touched(&mut self, x: usize, y: usize, w: usize, h: usize) {
        self.damage.add(Rect::new(x as i32, y as i32, w as u32, h as u32));
    }
}

impl Pixels for OnSurface<'_> {
    fn width(&self) -> usize {
        self.s.width() as usize
    }

    fn uncover(&mut self, _: usize, _: usize, _: usize, _: usize) {}

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: u32) {
        self.s.fill_rect(Rect::new(x as i32, y as i32, w as u32, h as u32), rgb);
        self.touched(x, y, w, h);
    }

    fn glyph(&mut self, x: usize, y: usize, c: char, ink: Ink, max_w: usize) -> usize {
        // As `fb::Framebuffer::glyph_clipped` draws it, pixel for pixel.
        let w = ink.weight;
        let raster = get_raster(c, w, fb::CONSOLE_SIZE).or_else(|| get_raster('?', w, fb::CONSOLE_SIZE));
        let Some(raster) = raster else { return get_raster_width(w, fb::CONSOLE_SIZE).min(max_w) };
        for (dy, row) in raster.rows().enumerate() {
            for (dx, &a) in row.iter().take(max_w).enumerate() {
                self.s.set_pixel((x + dx) as i32, (y + dy) as i32, fb::blend(ink.bg, ink.fg, a));
            }
        }
        let w = raster.width().min(max_w);
        self.touched(x, y, w, raster.height());
        w
    }

    fn move_rows(&mut self, from: usize, to: usize, len: usize) {
        self.s.move_rows(from as u32, to as u32, len as u32);
        let w = self.width();
        self.touched(0, to, w, len);
    }
}

/// A terminal window as the shell's console: out, its grid (the owner
/// draws it); in, the keys its owner took.
pub struct WindowConsole {
    shared: Arc<Shared>,
}

impl fmt::Write for WindowConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        {
            let mut t = self.shared.term.lock();
            for (i, line) in s.split('\n').enumerate() {
                if i > 0 {
                    t.write(b"\r\n");
                }
                t.write(line.as_bytes());
            }
        }
        self.shared.output.store(true, Ordering::Release);
        self.shared.owner.wake();
        Ok(())
    }
}

impl crate::shell::Console for WindowConsole {
    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = self.try_key() {
                return key;
            }
            let s = &self.shared;
            s.key_wait.wait_until(|| s.closed.load(Ordering::Acquire) || !s.keys.lock().is_empty());
        }
    }

    fn try_key(&mut self) -> Option<Key> {
        if self.closed() {
            return Some(Key::Ctrl('c'));
        }
        self.shared.keys.lock().pop_front()
    }

    fn name(&self) -> &'static str {
        "a terminal window"
    }

    fn window(&self) -> Option<u32> {
        Some(self.shared.id)
    }

    fn closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

/// Terminal window `id`'s view as text (`term text <window>`), as
/// `term::view_text` gives the screen's; why not if `id` names no open
/// terminal window.
pub fn view_text(id: u32) -> Result<ViewText, String> {
    let shared = OPEN
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|s| s.id == id && !s.closed.load(Ordering::Acquire))
        .ok_or_else(|| format!("no terminal window {id}"))?;
    // Allocates under the grid's lock: its own, which no heap path takes.
    let view = term::view_text_of(&shared.term.lock());
    Ok(view)
}

crate::ktest!(
    fn a_window_shell_runs_a_line_and_ends_when_closed() {
        // A terminal window's shell on a desktop with no screen: keys handed
        // to it (as the owner hands them) run `echo`, whose output lands in
        // the grid and wakes the owner's wait; once the window closes, the
        // shell ends (joined, bounded) and its console says so.
        use crate::{kassert, kassert_eq};
        let d = desktop::Desktop::without_screen();
        let mut h = d.open_window("t", 64, 64).map_err(|_| "a window")?;
        let shared = Arc::new(Shared {
            id: h.id(),
            term: SpinLock::new(Terminal::new(COLS, ROWS, SCROLLBACK)),
            keys: SpinLock::new(VecDeque::new()),
            key_wait: WaitQueue::new(),
            closed: AtomicBool::new(false),
            output: AtomicBool::new(false),
            owner: h.waker(),
        });
        // Its focus gain, from the opening.
        while h.next_event().is_some() {}
        let console = WindowConsole { shared: shared.clone() };
        let shell = sched::spawn("ktest-term-shell", move || crate::shell::run_in_window(console, COLS))
            .map_err(|_| "spawn failed")?;
        for c in "echo from-the-window\n".chars() {
            let key = Key::from_press(Some(c), None, lumen_input::Mods::NONE).ok_or("a key")?;
            shared.keys.lock().push_back(key);
        }
        shared.key_wait.wake_all();
        let deadline = crate::time::now_ns() + 5_000_000_000;
        let ran = |s: &Shared| term::view_text_of(&s.term.lock()).rows.iter().any(|r| r == "from-the-window");
        while !ran(&shared) {
            // The shell's output pokes the owner: its wait ends with no
            // event, long before the deadline.
            kassert!(h.wait_event_until(Some(deadline)).is_none());
            kassert!(crate::time::now_ns() < deadline);
        }
        kassert!(shared.output.load(Ordering::Acquire));
        shared.closed.store(true, Ordering::Release);
        shared.key_wait.wake_all();
        sched::join_bounded(shell)?;
        let mut console = WindowConsole { shared: shared.clone() };
        kassert!(crate::shell::Console::closed(&console));
        kassert_eq!(crate::shell::Console::try_key(&mut console), Some(Key::Ctrl('c')));
        kassert_eq!(crate::shell::Console::window(&console), Some(h.id()));
        Ok(())
    }
);

crate::ktest!(
    fn a_terminal_window_s_cells_look_as_the_console_s() {
        // The window's pixels (`OnSurface`) against the console's own
        // drawing of the same glyph (`Framebuffer::glyph_clipped`'s
        // blending, recomputed here from the font): a bold `A` in a red
        // foreground, a hollow cursor's outline around a blank cell.
        use crate::kassert;
        let cell_w = term::cell_width();
        let mut s = Surface::new((4 * cell_w) as u32, CELL_H as u32);
        s.fill(fb::BG);
        let mut t = Terminal::new(4, 1, 0);
        t.write(b"\x1b[1;31mA\x1b[0m");
        let mut r = Renderer::new(4, 1);
        let damage = {
            let mut px = OnSurface { s: &mut s, damage: Damage::new() };
            let mut cells = Cells { px: &mut px, x0: 0, y0: 0, cell_w };
            r.render(&mut t, &mut cells, Some(CursorLook::Hollow));
            px.damage
        };
        kassert!(!damage.is_empty());
        let raster = get_raster('A', noto_sans_mono_bitmap::FontWeight::Bold, fb::CONSOLE_SIZE).ok_or("no A")?;
        let red = lumen_term::palette::rgb(1);
        for (dy, row) in raster.rows().enumerate() {
            for (dx, &a) in row.iter().take(cell_w).enumerate() {
                kassert!(s.pixel(dx as i32, dy as i32) == Some(fb::blend(fb::BG, red, a)));
            }
        }
        // The cursor at column 1: its outline in the default foreground,
        // its middle the background.
        let (x, mid) = (cell_w as i32, (CELL_H / 2) as i32);
        kassert!(s.pixel(x, 0) == Some(fb::TEXT));
        kassert!(s.pixel(x + cell_w as i32 - 1, mid) == Some(fb::TEXT));
        kassert!(s.pixel(x + 2, mid) == Some(fb::BG));
        Ok(())
    }
);
