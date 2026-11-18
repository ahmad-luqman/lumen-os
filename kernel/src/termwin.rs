//! Terminal windows (#155; ARCHITECTURE section 11): the M2 terminal and
//! shell in desktop windows, several at once. `terminal` (from any shell)
//! opens one while the desktop is shown.
//!
//! - **Two threads per window.** Its owner, `terminal`, owns the window
//!   ([`WindowHandle`]) and draws the grid onto the window's surface with
//!   the text console's renderer (`lumen_term::render::Renderer` and
//!   `term::Cells`, so a cell looks as it does on the console), committing
//!   only what changed. Its shell, `term-shell`, runs the screen shells'
//!   line editor and commands on a [`WindowConsole`]. They share a
//!   [`Shared`]: the `lumen_term::Terminal` (with its scrollback) behind a
//!   lock of its own (a leaf: nothing is taken or allocated under it),
//!   which the shell's output feeds and the owner draws, and the keys the
//!   owner took from its window's events for the shell (at most
//!   [`KEYS_MAX`]; past that they are dropped, counted, and the first drop
//!   warned about).
//! - **Keys** are the window's key presses (the compositor sends them to the
//!   focused window) through `lumen_shell::Key::from_press`, the screen
//!   shells' mapping (section 7); Shift+PageUp/PageDown scroll the window's
//!   view back and forward by half a screen, as on a virtual terminal, and
//!   any other key the line editor takes brings it back.
//! - **The cursor** blinks only in the focused window (its owner's wait for
//!   the next blink is animating, so `desktop status` never says settled
//!   meanwhile), shown solid at once by output; an unfocused window's is
//!   drawn hollow and steady, and its owner waits with no deadline, idle.
//! - **Busy:** the shell marks its window busy (`desktop::Waker::set_busy`)
//!   except while it is blocked for a key with none queued, so a command
//!   running in an unfocused window (`top`) keeps `settled` at no.
//! - **No canvas:** a command that would draw on the screen (`mandelbrot`,
//!   `life`, a script's drawing) is refused in a terminal window, also at
//!   the head of a pipeline (`term::Place::Window`,
//!   `term::CanvasError::InWindow`); `top` writes text, and runs.
//! - **Closing** (the close box): the owner closes the shell's console
//!   (every key it asks for is `Key::Closed` from then on, which the line
//!   editor, the shell, `rhai`'s REPL and every command polling for a key
//!   stop on: `Key::interrupts`), closes the window at once (its surfaces
//!   freed), joins the shell, then frees the terminal and ends. A command
//!   that never asks for a key keeps the shell, its grid and the owner's
//!   join until it returns.
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
use lumen_term::render::{CursorLook, Renderer};
use lumen_term::{Cell, Terminal};

use crate::desktop::{self, Animating, Wait, Waker, WindowEvent, WindowHandle};
use crate::fb;
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::term::{self, CELL_H, Cells, Ink, Pixels, Place, ViewText};

/// A terminal window's grid.
pub const COLS: usize = 80;
pub const ROWS: usize = 24;
/// Lines kept above its screen.
pub const SCROLLBACK: usize = 100;
/// Keys waiting for a window's shell, at most (a virtual terminal's ring's
/// size): past that a key is dropped and counted.
pub const KEYS_MAX: usize = 128;
/// Pixels between the client area's edge and the grid.
const PAD: usize = 4;
/// How long the focused window's cursor stays on, then off.
const BLINK_NS: u64 = 500_000_000;
/// Where the first terminal window's frame goes; the next ones alternate
/// left and right, a step lower each pair.
const FIRST_AT: (i32, i32) = (24, 56);
const STEP: (i32, i32) = (640, 32);

/// The keys waiting for a window's shell.
struct Keys {
    queue: VecDeque<Key>,
    /// Keys dropped because the queue was full.
    dropped: u64,
}

/// What a window's owner and shell share.
struct Shared {
    /// The window's id (`windows`, `term text <window>`).
    id: u32,
    /// The grid, fed by the shell's output and drawn by the owner.
    term: SpinLock<Terminal>,
    /// Keys for the shell, oldest first.
    keys: SpinLock<Keys>,
    /// The shell waits here for a key (or the close).
    key_wait: WaitQueue,
    /// The window closed: the shell ends.
    closed: AtomicBool,
    /// Output since the owner last drew (it shows the cursor at once).
    output: AtomicBool,
    /// Wakes the owner for output, and marks the window busy.
    owner: Waker,
}

impl Shared {
    fn new(id: u32, owner: Waker) -> Arc<Shared> {
        Arc::new(Shared {
            id,
            term: SpinLock::new(Terminal::new(COLS, ROWS, SCROLLBACK)),
            keys: SpinLock::new(Keys { queue: VecDeque::with_capacity(KEYS_MAX), dropped: 0 }),
            key_wait: WaitQueue::new(),
            closed: AtomicBool::new(false),
            output: AtomicBool::new(false),
            owner,
        })
    }

    /// Close the shell's console (stored, then the shell woken).
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.key_wait.wake_all();
    }

    /// Queue `key` for the shell and wake it; a key past [`KEYS_MAX`] is
    /// dropped and counted, the window's first drop warned about.
    fn give(&self, key: Key) {
        let first_drop = {
            let mut k = self.keys.lock();
            if k.queue.len() < KEYS_MAX {
                k.queue.push_back(key);
                false
            } else {
                k.dropped += 1;
                k.dropped == 1
            }
        };
        self.key_wait.wake_all();
        if first_drop {
            log::warn!(target: "termwin", "terminal window {}: its shell isn't reading: dropping keys", self.id);
        }
    }

    /// The view as text, its cells copied out under the grid's lock into a
    /// vector allocated before it (the lock stays a leaf).
    fn view_text(&self) -> ViewText {
        let mut cells: Vec<Cell> = Vec::with_capacity(COLS * ROWS);
        let cursor = {
            let t = self.term.lock();
            cells.extend(t.view_cells());
            term::view_cursor(&t)
        };
        term::view_text_from(COLS, &cells, cursor)
    }
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
/// and shell; its window id. Refused unless the desktop is shown.
pub fn open() -> Result<u32, String> {
    if let Some(why) = desktop::refused_while_paused() {
        return Err(String::from(why));
    }
    let n = OPENED.fetch_add(1, Ordering::Relaxed) + 1;
    let title = format!("Terminal-{n}");
    let (w, h) = client_size();
    let (x, y) = place(n);
    let handle = match desktop::open_window_at(&title, w, h, x, y) {
        Ok(handle) => handle,
        Err(e) => {
            // A failed one doesn't burn its number (unless another opened
            // meanwhile).
            let _ = OPENED.compare_exchange(n, n - 1, Ordering::Relaxed, Ordering::Relaxed);
            return Err(match e {
                desktop::OpenError::NoDesktop => String::from("no desktop (`desktop` starts it)"),
                desktop::OpenError::TooBig => format!("the screen is smaller than a terminal window ({w}x{h})"),
                e => format!("no window: {e}"),
            });
        }
    };
    let id = handle.id();
    let shared = Shared::new(id, handle.waker());
    {
        let mut open = OPEN.lock();
        open.retain(|w| w.strong_count() > 0);
        open.push(Arc::downgrade(&shared));
    }
    // Busy until its shell first waits for a key.
    shared.owner.set_busy(true);
    // The shell first (its prompt waits in the grid for the owner's first
    // drawing); a failure drops the handle, which closes the window.
    let console = WindowConsole { shared: shared.clone() };
    let shell = sched::spawn("term-shell", move || crate::shell::run_in_window(console, COLS))
        .map_err(|e| format!("no thread for its shell: {e}"))?;
    let for_owner = shared.clone();
    if let Err(e) = sched::spawn("terminal", move || owner(handle, for_owner, shell)) {
        // The window is gone with the closure; end the shell (detached
        // with it), which then frees what it holds.
        shared.close();
        return Err(format!("no thread for its owner: {e}"));
    }
    Ok(id)
}

/// What a terminal window's owner keeps between events.
struct View {
    renderer: Renderer,
    cell_w: usize,
    focused: bool,
    blink_on: bool,
    next_blink: u64,
}

impl View {
    /// Take one event from the window; whether it is the close.
    fn take(&mut self, shared: &Shared, e: WindowEvent) -> bool {
        let now = crate::time::now_ns();
        match e {
            WindowEvent::Close => return true,
            WindowEvent::Key(k) => take_key(shared, k),
            WindowEvent::FocusGained => {
                (self.focused, self.blink_on, self.next_blink) = (true, true, now + BLINK_NS);
            }
            WindowEvent::FocusLost => self.focused = false,
            // The terminal keeps the console's palette whatever the theme
            // (section 11): the frame is the compositor's to repaint, the
            // client area stays as drawn.
            WindowEvent::Theme(_) => {}
            // No key or button is held here: each press is taken whole.
            WindowEvent::Lost => {}
            // No mouse in a terminal yet.
            WindowEvent::PointerDown { .. } | WindowEvent::PointerMove { .. } | WindowEvent::PointerUp { .. } => {}
            WindowEvent::Request(r) => match r {},
        }
        false
    }

    /// How the cursor shows now (moving the blink on).
    fn look(&mut self, shared: &Shared) -> Option<CursorLook> {
        let now = crate::time::now_ns();
        if shared.output.swap(false, Ordering::AcqRel) {
            (self.blink_on, self.next_blink) = (true, now + BLINK_NS);
        } else if self.focused && now >= self.next_blink {
            (self.blink_on, self.next_blink) = (!self.blink_on, now + BLINK_NS);
        }
        if self.focused { self.blink_on.then_some(CursorLook::Solid) } else { Some(CursorLook::Hollow) }
    }
}

/// A terminal window's owner: draw the grid, hand the keys to the shell,
/// blink the cursor while focused, until the close box; then end the shell
/// and free the window.
fn owner(mut h: WindowHandle, shared: Arc<Shared>, shell: sched::JoinHandle) {
    let mut view = View {
        renderer: Renderer::new(COLS, ROWS),
        cell_w: term::cell_width(),
        focused: false,
        blink_on: true,
        next_blink: 0,
    };
    {
        // The background, then every cell that isn't blank: one commit.
        let bounds = h.surface_mut().bounds();
        h.surface_mut().fill(fb::BG);
        let mut t = shared.term.lock();
        view.renderer.forget(&mut t);
        let _ = render_on(h.surface_mut(), &mut view.renderer, &mut t, view.cell_w, None);
        drop(t);
        h.commit(&[bounds]);
    }
    'events: loop {
        let (deadline, animating) =
            if view.focused { (Some(view.next_blink), Animating::Yes) } else { (None, Animating::No) };
        if let Wait::Event(e) = h.wait_event_until(deadline, animating)
            && view.take(&shared, e)
        {
            break;
        }
        // Everything pending, then one drawing.
        while let Some(e) = h.next_event() {
            if view.take(&shared, e) {
                break 'events;
            }
        }
        let look = view.look(&shared);
        draw(&mut h, &shared, &mut view, look);
    }
    shared.close();
    // The window goes now (its surfaces with it); the shell may still
    // finish a command.
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
            let pages = if code == KeyCode::PageUp { 1 } else { -1 };
            let _ = term::scroll_half_pages_of(&mut shared.term.lock(), pages);
        }
        _ => {
            let Some(key) = Key::from_press(text, Some(code), mods) else { return };
            let _ = shared.term.lock().reset_view();
            shared.give(key);
        }
    }
}

/// Draw what changed in `t` onto `s` (the grid [`PAD`] in), the cursor as
/// `look`; what it drew.
fn render_on(
    s: &mut Surface,
    r: &mut Renderer,
    t: &mut Terminal,
    cell_w: usize,
    look: Option<CursorLook>,
) -> Damage<16> {
    let mut px = OnSurface { s, damage: Damage::new() };
    r.render(t, &mut Cells { px: &mut px, x0: PAD, y0: PAD, cell_w }, look);
    px.damage
}

/// Draw what changed in the grid onto the window's surface, then commit
/// it.
fn draw(h: &mut WindowHandle, shared: &Shared, view: &mut View, look: Option<CursorLook>) {
    let damage = render_on(h.surface_mut(), &mut view.renderer, &mut shared.term.lock(), view.cell_w, look);
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
        // `fb::glyph_pixels`: the console's own glyph drawing.
        let s = &mut *self.s;
        let (w, h) = fb::glyph_pixels(c, ink.style(), max_w, |dx, dy, rgb| {
            s.set_pixel((x + dx) as i32, (y + dy) as i32, rgb);
        });
        self.touched(x, y, w, h);
        w
    }

    fn move_pixel_rows(&mut self, from: usize, to: usize, len: usize) {
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
        term::write_text(&mut self.shared.term.lock(), s);
        self.shared.output.store(true, Ordering::Release);
        self.shared.owner.wake();
        Ok(())
    }
}

impl crate::shell::Console for WindowConsole {
    /// The next key, blocking for one; the window counts as not busy only
    /// while this blocks with none queued.
    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = self.try_key() {
                return key;
            }
            let s = &self.shared;
            s.owner.set_busy(false);
            s.key_wait.wait_until(|| s.closed.load(Ordering::Acquire) || !s.keys.lock().queue.is_empty());
            s.owner.set_busy(true);
        }
    }

    fn try_key(&mut self) -> Option<Key> {
        if self.closed() {
            return Some(Key::Closed);
        }
        self.shared.keys.lock().queue.pop_front()
    }

    fn name(&self) -> &'static str {
        "a terminal window"
    }

    fn place(&self) -> Place {
        Place::Window(self.shared.id)
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
    Ok(shared.view_text())
}

// -- ktests --------------------------------------------------------------------

/// Closes its console when dropped: a ktest that starts a window's shell
/// keeps one, so failing half-way leaves no shell blocked for good.
#[cfg(feature = "ktest")]
struct CloseAfter(Arc<Shared>);

#[cfg(feature = "ktest")]
impl Drop for CloseAfter {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A window and its console's shared part on a desktop with no screen, its
/// focus gain (from the opening) and theme read, for the ktests.
#[cfg(feature = "ktest")]
fn test_shared(d: &desktop::Desktop) -> Result<(Arc<Shared>, WindowHandle), &'static str> {
    let mut h = d.open_window("t", 64, 64).map_err(|_| "a window")?;
    while h.next_event().is_some() {}
    let shared = Shared::new(h.id(), h.waker());
    // Busy until its shell first waits for a key, as `open` makes it.
    shared.owner.set_busy(true);
    Ok((shared, h))
}

/// Start a shell on `shared`'s console (see [`CloseAfter`]).
#[cfg(feature = "ktest")]
fn test_shell(shared: &Arc<Shared>) -> Result<sched::JoinHandle, &'static str> {
    let console = WindowConsole { shared: shared.clone() };
    sched::spawn("ktest-term-shell", move || crate::shell::run_in_window(console, COLS)).map_err(|_| "spawn failed")
}

/// Hand the shell `text`'s keys, as the owner does.
#[cfg(feature = "ktest")]
fn type_text(shared: &Shared, text: &str) -> Result<(), &'static str> {
    for c in text.chars() {
        shared.give(Key::from_press(Some(c), None, lumen_input::Mods::NONE).ok_or("a key")?);
    }
    Ok(())
}

/// Wait (polling, at most 10 s) until `done` holds of `shared`'s view.
#[cfg(feature = "ktest")]
fn wait_view(shared: &Shared, done: impl Fn(&ViewText) -> bool) -> Result<(), &'static str> {
    let deadline = crate::time::now_ns() + 10_000_000_000;
    while !done(&shared.view_text()) {
        if crate::time::now_ns() >= deadline {
            return Err("the view never showed it");
        }
        let _ = sched::sleep(5_000_000);
    }
    Ok(())
}

crate::ktest!(
    fn a_window_shell_runs_a_line_and_ends_when_closed() {
        // A terminal window's shell on a desktop with no screen: keys handed
        // to it (as the owner hands them) run `echo`, whose output lands in
        // the grid and wakes the owner's wait promptly; once the window
        // closes, the shell ends (joined, bounded) and its console gives
        // `Key::Closed` for every key asked.
        use crate::{kassert, kassert_eq};
        let d = desktop::Desktop::without_screen();
        let (shared, mut h) = test_shared(&d)?;
        let _close = CloseAfter(shared.clone());
        let shell = test_shell(&shared)?;
        type_text(&shared, "echo from-the-window\n")?;
        let deadline = crate::time::now_ns() + 5_000_000_000;
        let ran = |s: &Shared| s.view_text().rows.iter().any(|r| r == "from-the-window");
        while !ran(&shared) {
            // The shell's output pokes the owner: its wait ends, long before
            // the deadline.
            kassert!(matches!(h.wait_event_until(Some(deadline), Animating::No), Wait::Woken));
            kassert!(crate::time::now_ns() < deadline);
        }
        kassert!(shared.output.load(Ordering::Acquire));
        shared.close();
        sched::join_bounded(shell)?;
        let mut console = WindowConsole { shared: shared.clone() };
        use crate::shell::Console;
        kassert!(console.closed());
        kassert_eq!(console.try_key(), Some(Key::Closed));
        kassert_eq!(console.read_key(), Key::Closed);
        kassert_eq!(console.place(), Place::Window(h.id()));
        Ok(())
    }
);

crate::ktest!(
    fn closing_a_window_ends_what_its_shell_runs() {
        // Each command waiting in a terminal window when the window closes
        // ends on its console's `Key::Closed` (where Ctrl-C would only
        // restart `rhai`'s prompt), then the shell, so the owner's join
        // returns (bounded here): `rhai`'s REPL waiting for a line, `top`
        // polling for q between frames, a script in a loop (its Ctrl-C
        // check). Meanwhile the shell keeps its window busy while a command
        // runs (not while the REPL waits for a key: idle).
        use crate::kassert;
        let d = desktop::Desktop::without_screen();
        let cases =
            [("rhai", "rhai>", false), ("top", "top:", true), ("rhai -e loop{}", "lumen> rhai -e loop{}", true)];
        for (command, started, busy) in cases {
            let (shared, h) = test_shared(&d)?;
            let _close = CloseAfter(shared.clone());
            let shell = test_shell(&shared)?;
            type_text(&shared, command)?;
            type_text(&shared, "\n")?;
            wait_view(&shared, |v| v.rows.iter().any(|r| r.starts_with(started)))?;
            let _ = sched::sleep(50_000_000);
            kassert!(shared.keys.lock().queue.is_empty());
            kassert!(h.is_busy() == busy);
            shared.close();
            sched::join_bounded(shell)?;
            drop(h);
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_window_s_owner_joins_its_shell_before_it_ends() {
        // The owner, told its window closed, closes the shell's console and
        // drops the window at once, but ends only once the shell has ended:
        // a shell busy in a command (here a thread standing in for it,
        // blocked until let go) keeps the owner alive.
        use crate::kassert;
        let d = desktop::Desktop::without_screen();
        let (shared, h) = test_shared(&d)?;
        let _close = CloseAfter(shared.clone());
        let release = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
        let r = release.clone();
        let shell = sched::spawn("ktest-busy-shell", move || r.0.wait_until(|| r.1.load(Ordering::Acquire)))
            .map_err(|_| "spawn failed")?;
        let id = h.id();
        let (o, closer) = (shared.clone(), desktop::test_closer(&d, id));
        let owner_thread = sched::spawn("ktest-owner", move || owner(h, o, shell)).map_err(|_| "spawn failed")?;
        let _ = sched::sleep(20_000_000);
        closer()?;
        let _ = sched::sleep(50_000_000);
        kassert!(shared.closed.load(Ordering::Acquire));
        kassert!(owner_thread.thread().state() != sched::State::Dead);
        release.1.store(true, Ordering::Release);
        release.0.wake_all();
        sched::join_bounded(owner_thread)?;
        Ok(())
    }
);

crate::ktest!(
    fn keys_past_the_queue_s_room_are_dropped_and_counted() {
        // A shell that doesn't read (a long `sleep`) while keys come: the
        // queue keeps `KEYS_MAX`, drops the rest and counts them.
        use crate::kassert_eq;
        let d = desktop::Desktop::without_screen();
        let (shared, _h) = test_shared(&d)?;
        for _ in 0..KEYS_MAX + 3 {
            shared.give(Key::Char('x'));
        }
        let k = shared.keys.lock();
        kassert_eq!(k.queue.len(), KEYS_MAX);
        kassert_eq!(k.dropped, 3);
        Ok(())
    }
);

crate::ktest!(
    fn a_terminal_window_s_cells_look_as_the_console_s() {
        // The window's pixels (`OnSurface`) against the console's own
        // drawing of the same glyph (`fb::glyph_pixels`, which
        // `Framebuffer::glyph_clipped` draws with): a bold `A` in a red
        // foreground; a hollow cursor's outline around a blank cell.
        use crate::kassert;
        let cell_w = term::cell_width();
        let mut s = Surface::new((4 * cell_w + 2 * PAD) as u32, (CELL_H + 2 * PAD) as u32);
        s.fill(fb::BG);
        let mut t = Terminal::new(4, 1, 0);
        t.write(b"\x1b[1;31mA\x1b[0m");
        let mut r = Renderer::new(4, 1);
        let damage = render_on(&mut s, &mut r, &mut t, cell_w, Some(CursorLook::Hollow));
        kassert!(!damage.is_empty());
        let red = lumen_term::palette::rgb(1);
        let ink = Ink { weight: noto_sans_mono_bitmap::FontWeight::Bold, fg: red, bg: fb::BG };
        let mut same = true;
        let _ = fb::glyph_pixels('A', ink.style(), cell_w, |dx, dy, rgb| {
            same &= s.pixel((PAD + dx) as i32, (PAD + dy) as i32) == Some(rgb);
        });
        kassert!(same);
        // The cursor at column 1: its outline in the default foreground,
        // its middle the background.
        let (x, y, mid) = ((PAD + cell_w) as i32, PAD as i32, (PAD + CELL_H / 2) as i32);
        kassert!(s.pixel(x, y) == Some(fb::TEXT));
        kassert!(s.pixel(x + cell_w as i32 - 1, mid) == Some(fb::TEXT));
        kassert!(s.pixel(x + 2, mid) == Some(fb::BG));
        Ok(())
    }
);

crate::ktest!(
    fn a_window_s_scrolled_pixels_match_a_full_repaint() {
        // More lines than the grid's rows, drawn a line at a time (each line
        // feed at the bottom moving the surface's rows), then the same
        // terminal drawn whole on a fresh surface: every pixel equal.
        use crate::kassert;
        let cell_w = term::cell_width();
        let (w, h) = ((COLS * cell_w + 2 * PAD) as u32, (ROWS * CELL_H + 2 * PAD) as u32);
        let mut stepped = Surface::new(w, h);
        stepped.fill(fb::BG);
        let mut t = Terminal::new(COLS, ROWS, SCROLLBACK);
        let mut r = Renderer::new(COLS, ROWS);
        for i in 0..ROWS + 7 {
            term::write_text(&mut t, &format!("line {i}: \x1b[3{}m{}\x1b[0m\n", i % 8, "#".repeat(i % 20)));
            let _ = render_on(&mut stepped, &mut r, &mut t, cell_w, Some(CursorLook::Solid));
        }
        let mut whole = Surface::new(w, h);
        whole.fill(fb::BG);
        let mut fresh = Renderer::new(COLS, ROWS);
        fresh.forget(&mut t);
        let _ = render_on(&mut whole, &mut fresh, &mut t, cell_w, Some(CursorLook::Solid));
        kassert!((0..h).all(|y| stepped.row(y) == whole.row(y)));
        Ok(())
    }
);
