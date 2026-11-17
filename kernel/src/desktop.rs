//! The desktop (#153; ARCHITECTURE section 11): windows on the whole
//! screen, composed by one kernel thread, `compositor`.
//!
//! - **Opt-in.** The text console stays the default. `desktop` (from any
//!   shell) or the `lumen.desktop` boot flag starts it: the back buffer is
//!   allocated, the compositor thread started, then the screen taken
//!   (`crate::vt::enter_desktop`: a `term::ScreenCanvas`); a step that
//!   fails undoes the ones before it (`desktop: not started: <reason>`).
//!   Alt+F1 to F4 leave it (`crate::vt`), `desktop` resumes it: its windows
//!   live on meanwhile. There is no `desktop off` in M4.
//! - **Windows** ([`open_window`], [`WindowHandle`]): an owner thread draws
//!   into its own surface with no lock and commits rectangles of it to the
//!   window's committed surface (at most 16 rows per hold of the window's
//!   lock); the compositor reads only the committed one. The frame (title
//!   bar, border, close box) is the compositor's, from `lumen-wm`'s
//!   geometry and the theme. Events reach the owner through the window's
//!   `lumen_wm::EventQueue` ([`WindowHandle::next_event`],
//!   [`WindowHandle::wait_event`]).
//! - **A pass** ([`Compositor::pass`]): the keyboard's decoding pass
//!   (`vt::decode_pending`, holding none of the desktop's locks), both
//!   input rings read (pointer frames through `lumen_wm::Wm` into focus,
//!   drags, the close box and windows' pointer events; key events to the
//!   focused window), the cut acted on (a pause or a resume), then under
//!   [`COMPOSITOR`] the damage and a snapshot of the stacking order taken;
//!   only the damaged rectangles are recomposed in the back buffer and
//!   copied to the screen, then the pointer is drawn straight onto the
//!   screen (never into the back buffer).
//! - **Locks**, in order: [`COMPOSITOR`] (the window manager, the damage,
//!   the statistics), a window's lock (its committed surface and queue),
//!   the heap's, the log's line lock, the screen's. Nothing holding the
//!   router lock (`crate::vt`), the log's or the screen's takes one of the
//!   desktop's, and the desktop takes the router lock holding none of its
//!   own. Every wake happens after the lock that guarded the change.
//! - **The crash screen still wins**: `crash::enter` calls `term::stop`,
//!   which ends the `ScreenCanvas` (no present row starts after it), and
//!   nothing on the crash path takes a desktop lock.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lumen_gfx::{Damage, Mask, Rect, Surface, Target};
use lumen_input::cursor::{SPRITE_H, SPRITE_W, draw_arrow};
use lumen_input::{KeyAction, KeyEvent, PointerEvent, Read, Reader};
use lumen_png::encode::Rows;
use lumen_wm::{
    Button, ButtonSet, Entry, EventQueue, Interruption, Metrics, Next, Outcome, Pointer, PointerEntry, Pushed,
    QueueCounts, Theme, WindowId, Wm,
};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

use crate::input::{KEY_EVENTS, KEYS, LockedRing, POINTER, POINTER_EVENTS};
use crate::sched::WaitQueue;
use crate::sync::{SpinLock, SpinLockGuard};
use crate::term::{self, CanvasError, Owner, ScreenCanvas};
use crate::vt;

/// Windows open at once, at most.
pub const MAX_WINDOWS: usize = 32;
/// Rows of a committed surface copied (or blitted) per hold of the
/// window's lock.
const BAND: u32 = 16;
/// The damage gathered between passes (past this many rectangles it
/// collapses into their bounding box: still correct, just more drawn).
type Gathered = Damage<32>;

/// The compositor sleeps here until [`PENDING`] is set or the keyboard
/// has bytes waiting.
pub static WAKE: WaitQueue = WaitQueue::new();
/// Set by every waker before it calls [`wake`]; the compositor clears it
/// as it wakes.
pub static PENDING: AtomicBool = AtomicBool::new(false);
/// Set once the compositor thread exists: [`wake`] does nothing before.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// A job's answer is in ([`State::answer`]).
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// Woken after every pass: a job's asker, and `desktop` waiting for the
/// compositor to catch up with the cut.
static DONE: WaitQueue = WaitQueue::new();

/// The live desktop's state: `None` until it starts (it never ends in M4).
pub static COMPOSITOR: SpinLock<Option<State>> = SpinLock::new(None);

/// Wake the compositor (nothing before the desktop started): IRQ 1 when
/// it queued bytes, the `mouse` thread after pushing frames, every commit
/// and request, a pause or a resume. Sets [`PENDING`] first. Called with
/// no desktop lock held (any other lock is fine: it takes only the wait
/// queue's).
pub fn wake() {
    if RUNNING.load(Ordering::Acquire) {
        PENDING.store(true, Ordering::Release);
        WAKE.wake_all();
    }
}

/// The light theme until #154 adds switching.
const THEME: &Theme = &Theme::LIGHT;
/// The frames' geometry.
const METRICS: Metrics = Metrics::DESKTOP;
/// The bitmap font the titles and the demo windows use (TrueType comes
/// with #154).
const FONT: RasterHeight = RasterHeight::Size16;

/// An automation request (#156 fills this in): none can be made yet, so
/// no window's queue ever holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {}

/// A window's event queue.
type Queue = EventQueue<KeyEvent, Request>;

/// What a window's owner is told ([`WindowHandle::next_event`]). Pointer
/// positions are in its client area's coordinates (outside it while a
/// grab follows the pointer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowEvent {
    Key(KeyEvent),
    PointerDown {
        x: i32,
        y: i32,
        button: Button,
        buttons: ButtonSet,
    },
    PointerMove {
        x: i32,
        y: i32,
        buttons: ButtonSet,
    },
    PointerUp {
        x: i32,
        y: i32,
        button: Button,
        buttons: ButtonSet,
    },
    FocusGained,
    /// Lost the focus: every key and button counts as released.
    FocusLost,
    /// Events for this window were dropped: every key and button counts as
    /// released.
    Lost,
    /// The close box was clicked: the owner decides (dropping the handle
    /// closes the window).
    Close,
    /// An automation request (#156).
    Request(Request),
}

impl From<Next<Entry<KeyEvent, Request>>> for WindowEvent {
    fn from(n: Next<Entry<KeyEvent, Request>>) -> WindowEvent {
        match n {
            Next::Lost => WindowEvent::Lost,
            Next::Focus(true) => WindowEvent::FocusGained,
            Next::Focus(false) => WindowEvent::FocusLost,
            Next::Close => WindowEvent::Close,
            Next::Event(Entry::Key(k)) => WindowEvent::Key(k),
            Next::Event(Entry::Pointer(PointerEntry::Down { x, y, button, buttons })) => {
                WindowEvent::PointerDown { x, y, button, buttons }
            }
            Next::Event(Entry::Pointer(PointerEntry::Move { x, y, buttons })) => {
                WindowEvent::PointerMove { x, y, buttons }
            }
            Next::Event(Entry::Pointer(PointerEntry::Up { x, y, button, buttons })) => {
                WindowEvent::PointerUp { x, y, button, buttons }
            }
            Next::Event(Entry::Request(r)) => WindowEvent::Request(r),
        }
    }
}

/// A window as the compositor and its owner share it.
pub struct Window {
    id: WindowId,
    title: String,
    inner: SpinLock<WindowInner>,
    /// Whether the queue holds an entry or a flag (`!is_idle()`), stored
    /// under the window's lock after every change: the owner's wake-up
    /// check reads it without the lock.
    queued: AtomicBool,
    /// Set while the owner waits in [`WindowHandle::wait_event`].
    idle: AtomicBool,
    /// Its owner waits here.
    events: WaitQueue,
    /// `desktop hold`: a `desktop spin` window's owner stops reading its
    /// events.
    held: AtomicBool,
    /// Opened by `desktop spin` (the only windows `desktop hold` takes).
    spin: bool,
    /// Key events its queue took.
    keys: AtomicU64,
}

/// What a window's lock guards.
struct WindowInner {
    /// The surface the compositor reads.
    committed: Surface,
    queue: Queue,
    /// The thread that last read its events (the opener's until then).
    owner: &'static str,
}

/// Why [`open_window`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The desktop hasn't started.
    NoDesktop,
    /// [`MAX_WINDOWS`] are open.
    TooMany,
    /// A client area empty or bigger than the screen.
    TooBig,
    /// No memory for its surfaces.
    OutOfMemory,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OpenError::NoDesktop => "no desktop",
            OpenError::TooMany => "too many windows",
            OpenError::TooBig => "a size from 1 pixel to the screen's, each way",
            OpenError::OutOfMemory => "no memory for its surfaces",
        })
    }
}

/// What the compositor is asked to do right after a pass.
enum Job {
    /// `desktop check`, with its scratch surface.
    Check(Box<Surface>),
    /// `screenshot`: the back buffer into these rows.
    Capture(Box<Rows>),
}

/// A job's answer.
enum Answer {
    /// Pixels that differ from a full repaint, and from the screen.
    Check {
        repaint: u64,
        screen: u64,
    },
    /// Damage or a commit arrived meanwhile: try again.
    Busy,
    Capture {
        rows: Box<Rows>,
        ns: u64,
    },
}

/// What `desktop status` counts.
#[derive(Clone, Copy, Default)]
struct Stats {
    frames: u64,
    rects: u64,
    pixels: u64,
    present_ns: u64,
    no_focus_keys: u64,
}

/// The desktop's state behind [`COMPOSITOR`] (or a test's own lock:
/// [`Desktop::without_screen`]).
pub struct State {
    wm: Wm,
    /// The open windows (in no order: found by id).
    windows: Vec<Arc<Window>>,
    damage: Gathered,
    width: u32,
    height: u32,
    stats: Stats,
    /// The queue counts of windows closed since the start.
    closed: QueueCounts,
    /// Commits under way, and commits finished in all.
    commits: u32,
    committed: u64,
    /// A pass between its snapshot and its end.
    passing: bool,
    /// Whether the compositor presents (its view of the cut), and the cut's
    /// generation it has acted on.
    shown: bool,
    seen_generation: u64,
    /// The readers' places and gaps, published after each pass.
    pointer_read: lumen_input::Seq,
    keys_read: lumen_input::Seq,
    lost: u64,
    /// Events read but held back (a key ring barrier, or a resume whose
    /// screen hasn't come yet).
    held_back: bool,
    /// The screen for the next resume (`vt::enter_desktop`'s, handed over).
    screen: Option<ScreenCanvas>,
    /// The compositor should end (its start failed after it began).
    abort: bool,
    job: Option<Job>,
    answer: Option<Answer>,
    /// The compositor's own owner of the screen.
    owner: Owner,
}

impl State {
    fn new(width: u32, height: u32, owner: Owner) -> State {
        State {
            wm: Wm::new(width, height, METRICS),
            windows: Vec::new(),
            damage: Gathered::new(),
            width,
            height,
            stats: Stats::default(),
            closed: QueueCounts::default(),
            commits: 0,
            committed: 0,
            passing: false,
            shown: false,
            seen_generation: 0,
            pointer_read: lumen_input::Seq::ZERO,
            keys_read: lumen_input::Seq::ZERO,
            lost: 0,
            held_back: false,
            screen: None,
            abort: false,
            job: None,
            answer: None,
            owner,
        }
    }

    fn window(&self, id: WindowId) -> Option<&Arc<Window>> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Repaint `out`'s damage and put its deliveries on their windows'
    /// queues: the windows to wake (after the locks) into `wakes`, the
    /// windows whose queue began dropping into `behind`.
    fn apply(&mut self, out: Outcome, after: &mut After) {
        for r in out.damage.rects() {
            self.damage.add(*r);
        }
        for d in &out.deliveries {
            self.push(d.window(), after, |q| lumen_wm::deliver(q, d));
        }
    }

    /// Change window `id`'s queue with `f` (nothing if it isn't open).
    fn push(&mut self, id: WindowId, after: &mut After, f: impl FnOnce(&mut Queue) -> Option<Pushed>) {
        let Some(win) = self.window(id) else { return };
        let pushed = {
            let mut inner = win.inner.lock();
            let pushed = f(&mut inner.queue);
            win.queued.store(!inner.queue.is_idle(), Ordering::Release);
            pushed
        };
        if let Some(Pushed::Dropped { first_in_burst: true }) = pushed {
            after.behind.push(id.get());
        }
        after.wake.push(win.clone());
    }
}

/// Work for after the locks: owners to wake, warnings to log.
#[derive(Default)]
struct After {
    wake: Vec<Arc<Window>>,
    behind: Vec<u32>,
}

impl After {
    fn finish(self) {
        for w in &self.wake {
            w.events.wake_all();
        }
        for id in self.behind {
            log::warn!(target: "desktop", "window {id} is behind: dropping events");
        }
    }
}

/// Where a desktop's state lives: the live one's [`COMPOSITOR`], or a
/// test's own ([`Desktop::without_screen`]).
#[derive(Clone)]
enum Home {
    Live,
    #[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "made by `Desktop::without_screen`, for the ktests"))]
    Own(Arc<SpinLock<Option<State>>>),
}

impl Home {
    fn lock(&self) -> SpinLockGuard<'_, Option<State>> {
        match self {
            Home::Live => COMPOSITOR.lock(),
            Home::Own(state) => state.lock(),
        }
    }

    fn wake(&self) {
        if let Home::Live = self {
            wake();
        }
    }
}

/// A desktop: the live one ([`open_window`]) or one with no compositor
/// thread and no screen, for the ktests, so none leaves a desktop running.
pub struct Desktop {
    home: Home,
}

#[cfg_attr(
    not(feature = "ktest"),
    allow(dead_code, reason = "the ktests' desktop; windows' owners come with #155-#157")
)]
impl Desktop {
    /// A 1280x800 desktop with no compositor and no screen: windows open
    /// and close, nothing is drawn.
    pub fn without_screen() -> Desktop {
        let state = State::new(1280, 800, Owner::new());
        Desktop { home: Home::Own(Arc::new(SpinLock::new(Some(state)))) }
    }

    /// Open a window of `width` x `height` client pixels titled `title`,
    /// in front and focused, its frame's corner placed by how many are
    /// open.
    pub fn open_window(&self, title: &str, width: u32, height: u32) -> Result<WindowHandle, OpenError> {
        self.open(title, width, height, None, false)
    }

    fn open(
        &self,
        title: &str,
        width: u32,
        height: u32,
        at: Option<(i32, i32)>,
        spin: bool,
    ) -> Result<WindowHandle, OpenError> {
        let (sw, sh) = {
            let g = self.home.lock();
            let s = g.as_ref().ok_or(OpenError::NoDesktop)?;
            if s.windows.len() >= MAX_WINDOWS {
                return Err(OpenError::TooMany);
            }
            (s.width, s.height)
        };
        if width == 0 || height == 0 || width > sw || height > sh {
            return Err(OpenError::TooBig);
        }
        // Both surfaces before any lock (`try_reserve`: a failure is an
        // error, not a panic).
        let surface = Surface::with_stride(width, height, width as usize).ok_or(OpenError::OutOfMemory)?;
        let mut committed = Surface::with_stride(width, height, width as usize).ok_or(OpenError::OutOfMemory)?;
        committed.fill(THEME.client);
        let owner = crate::sched::running_here().map_or("boot", |(_, name)| name);
        let mut after = After::default();
        let win = {
            let mut g = self.home.lock();
            let s = g.as_mut().ok_or(OpenError::NoDesktop)?;
            if s.windows.len() >= MAX_WINDOWS {
                return Err(OpenError::TooMany);
            }
            let n = s.windows.len() as i32;
            let (x, y) = at.unwrap_or((96 + 32 * (n % 8), 80 + 32 * (n % 8)));
            let (id, out) = s.wm.open(title, width, height, x, y);
            let win = Arc::new(Window {
                id,
                title: String::from(title),
                inner: SpinLock::new(WindowInner { committed, queue: Queue::new(), owner }),
                queued: AtomicBool::new(false),
                idle: AtomicBool::new(false),
                events: WaitQueue::new(),
                held: AtomicBool::new(false),
                spin,
                keys: AtomicU64::new(0),
            });
            s.windows.push(win.clone());
            s.apply(out, &mut after);
            win
        };
        after.finish();
        self.home.wake();
        Ok(WindowHandle { win, surface, home: self.home.clone(), _not_sync: PhantomData })
    }
}

/// Open a window on the live desktop (see [`Desktop::open_window`]):
/// `NoDesktop` until it started.
#[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "its first callers are #155-#157's windows"))]
pub fn open_window(title: &str, width: u32, height: u32) -> Result<WindowHandle, OpenError> {
    Desktop { home: Home::Live }.open_window(title, width, height)
}

/// A window's owner side: its own surface (drawn with no lock), commits,
/// events. `Send` (a thread may open a window and hand it on), not `Sync`
/// and not `Clone`. Dropping it closes the window.
pub struct WindowHandle {
    win: Arc<Window>,
    surface: Surface,
    home: Home,
    _not_sync: PhantomData<Cell<()>>,
}

impl WindowHandle {
    /// The window's id (from 1, never reused in a boot).
    pub fn id(&self) -> u32 {
        self.win.id.get()
    }

    /// The owner's surface, the client area's size: drawn at the owner's
    /// pace, shown once committed.
    pub fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    /// Copy `rects` (client coordinates, clipped to it) of the owner's
    /// surface to the committed one, at most [`BAND`] rows per hold of the
    /// window's lock, then queue them as damage (moved to the window's
    /// place on the screen) and wake the compositor. A pass may compose a
    /// rectangle halfway through its copy; the damage, queued after the
    /// last rows, makes the next pass draw it whole. Never waits for a
    /// present.
    pub fn commit(&mut self, rects: &[Rect]) {
        let bounds = self.surface.bounds();
        if let Some(s) = self.home.lock().as_mut() {
            s.commits += 1;
        }
        for r in rects.iter().filter_map(|r| r.intersect(&bounds)) {
            let mut y = r.y();
            while y < r.bottom() {
                let rows = BAND.min((r.bottom() - y) as u32);
                let band = Rect::new(r.x(), y, r.w(), rows);
                self.win.inner.lock().committed.blit(&self.surface, band, r.x(), y);
                y += rows as i32;
            }
        }
        if let Some(s) = self.home.lock().as_mut() {
            s.commits -= 1;
            s.committed += 1;
            if let Some(client) = s.wm.client(self.win.id) {
                for r in rects.iter().filter_map(|r| r.intersect(&bounds)) {
                    s.damage.add(r.translate(client.x(), client.y()));
                }
            }
        }
        self.home.wake();
    }

    /// The next event, if one is waiting (never blocks).
    pub fn next_event(&mut self) -> Option<WindowEvent> {
        let mut inner = self.win.inner.lock();
        if let Some((_, name)) = crate::sched::running_here() {
            inner.owner = name;
        }
        let next = inner.queue.next_event();
        self.win.queued.store(!inner.queue.is_idle(), Ordering::Release);
        next.map(WindowEvent::from)
    }

    /// The next event, waiting for one (the window counts as idle
    /// meanwhile: `desktop status`'s `settled`).
    pub fn wait_event(&mut self) -> WindowEvent {
        loop {
            if let Some(e) = self.next_event() {
                return e;
            }
            self.win.idle.store(true, Ordering::Release);
            let win = &self.win;
            win.events.wait_until(|| win.queued.load(Ordering::Acquire));
            self.win.idle.store(false, Ordering::Release);
        }
    }

    /// Whether `desktop hold` holds this window (a `desktop spin` one).
    fn held(&self) -> bool {
        self.win.held.load(Ordering::Acquire)
    }
}

impl Drop for WindowHandle {
    fn drop(&mut self) {
        let counts = {
            let mut inner = self.win.inner.lock();
            inner.queue.kill();
            self.win.queued.store(false, Ordering::Release);
            inner.queue.counts()
        };
        let mut after = After::default();
        if let Some(s) = self.home.lock().as_mut() {
            s.windows.retain(|w| !Arc::ptr_eq(w, &self.win));
            add_counts(&mut s.closed, &counts);
            if let Some(out) = s.wm.close(self.win.id) {
                s.apply(out, &mut after);
            }
        }
        after.finish();
        self.home.wake();
    }
}

/// `into += from`, field by field.
fn add_counts(into: &mut QueueCounts, from: &QueueCounts) {
    into.rationed += from.rationed;
    into.overflowed += from.overflowed;
    into.busy += from.busy;
    into.stray_forgotten += from.stray_forgotten;
    into.stray_unpressed += from.stray_unpressed;
    into.key_releases_unpaired += from.key_releases_unpaired;
    into.dead_pushes += from.dead_pushes;
    into.merged += from.merged;
}

// -- drawing -----------------------------------------------------------------

/// A glyph of the bitmap font as a coverage mask (`?` for one it lacks).
fn glyph(c: char, weight: FontWeight) -> Option<Mask<'static>> {
    let raster = get_raster(c, weight, FONT).or_else(|| get_raster('?', weight, FONT))?;
    Mask::new(raster.width() as u32, raster.height() as u32, raster.pixels())
}

/// Draw `text` in the bitmap font with its top-left corner at (`x`, `y`)
/// in `color`, touching only pixels inside `clip` (blending isn't
/// idempotent: a redraw of one damaged rectangle passes it); returns the
/// pen's position after it.
fn draw_text(s: &mut Surface, x: i32, y: i32, text: &str, color: u32, weight: FontWeight, clip: Rect) -> i32 {
    let advance = get_raster_width(weight, FONT) as i32;
    let mut pen = x;
    for c in text.chars() {
        if pen >= clip.right() {
            break;
        }
        if let Some(mask) = glyph(c, weight) {
            s.blend_mask_in(&mask, pen, y, color, clip);
        }
        pen += advance;
    }
    pen
}

/// The close box's cross: a 16x16 coverage mask, two diagonals two pixels
/// wide, 4 in from each edge.
fn cross() -> [u8; 256] {
    let mut m = [0u8; 256];
    for i in 4..12 {
        for (x, y) in [(i, i), (i + 1, i), (15 - i, i), (14 - i, i)] {
            if x < 16 {
                m[y * 16 + x] = 255;
            }
        }
    }
    m
}

/// One window in a snapshot of the stacking order.
struct Snap {
    win: Arc<Window>,
    frame: Rect,
    focused: bool,
}

/// Recompose `r` (screen coordinates) of the scene into `target`: the
/// desktop's colour, then every window overlapping it from back to front,
/// its frame then its committed surface (blitted [`BAND`] rows per hold of
/// the window's lock). Everything drawn stays inside `r`, so damage-only
/// recomposition equals a full one.
fn compose(target: &mut Surface, r: Rect, scene: &[Snap]) {
    let Some(r) = r.clip(target.width(), target.height()) else { return };
    target.fill_rect(r, THEME.desktop);
    let cross = cross();
    for snap in scene {
        if snap.frame.intersect(&r).is_none() {
            continue;
        }
        paint_frame(target, snap, r, &cross);
        let client = METRICS.client(&snap.frame);
        let Some(c) = client.intersect(&r) else { continue };
        let mut y = c.y();
        while y < c.bottom() {
            let rows = BAND.min((c.bottom() - y) as u32);
            let src = Rect::new(c.x() - client.x(), y - client.y(), c.w(), rows);
            target.blit(&snap.win.inner.lock().committed, src, c.x(), y);
            y += rows as i32;
        }
    }
}

/// A window's frame, the part of it inside `clip`: the border, the title
/// bar with its title, the close box with its cross.
fn paint_frame(target: &mut Surface, snap: &Snap, clip: Rect, cross: &[u8; 256]) {
    let (border, title_bg, title_fg) = if snap.focused {
        (THEME.border_focused, THEME.title_focused, THEME.title_focused_text)
    } else {
        (THEME.border, THEME.title, THEME.title_text)
    };
    let frame = snap.frame;
    let Some(f) = frame.intersect(&clip) else { return };
    target.fill_rect(f, border);
    let bar = METRICS.title_bar(&frame);
    if let Some(b) = bar.intersect(&clip) {
        target.fill_rect(b, title_bg);
    }
    let close = METRICS.close_box(&frame);
    if let Some(c) = close.intersect(&clip) {
        target.fill_rect(c, THEME.close);
        if let Some(mask) = Mask::new(16, 16, cross) {
            target.blend_mask_in(&mask, close.x(), close.y(), THEME.close_glyph, c);
        }
    }
    // The title, left of the close box, never over it.
    let text_right = close.x().saturating_sub(4).max(bar.x());
    let area = Rect::new(bar.x(), bar.y(), (text_right - bar.x()).max(0) as u32, bar.h());
    if let Some(a) = area.intersect(&clip) {
        let y = bar.y() + (METRICS.title_h as i32 - 16) / 2;
        draw_text(target, bar.x() + 8, y, &snap.win.title, title_fg, FontWeight::Bold, a);
    }
}

// -- the compositor ------------------------------------------------------------

type PointerRing = LockedRing<PointerEvent, POINTER_EVENTS>;
type KeyRing = LockedRing<KeyEvent, KEY_EVENTS>;

/// The compositor thread's own state: touched by it alone, so no lock.
pub struct Compositor {
    back: Surface,
    screen: Option<ScreenCanvas>,
    pointers: Reader<PointerRing>,
    keys: Reader<KeyRing>,
    /// Pointer frames and key events read but not acted on yet.
    pointer_queue: VecDeque<Read<PointerEvent>>,
    key_queue: VecDeque<Read<KeyEvent>>,
    /// The cut's generation acted on, and whether it is shown then.
    generation: u64,
    shown: bool,
    /// The newest pointer position read, and where the arrow is drawn.
    pointer: Option<(i32, i32)>,
    drawn: Option<(i32, i32)>,
}

/// Whether `e` is an Alt+F`n` press: the cut's barrier in the key ring.
fn is_switch(e: &KeyEvent) -> bool {
    matches!(e.action, KeyAction::Press { .. }) && vt::switch_key(e.code, e.mods.is_alt()).is_some()
}

/// The arrow's box with its tip at `at`.
fn arrow_box(at: (i32, i32)) -> Rect {
    Rect::new(at.0, at.1, SPRITE_W as u32, SPRITE_H as u32)
}

/// The position of a pointer frame, as the window manager takes it.
fn position(e: &PointerEvent) -> (i32, i32) {
    let p = Pointer::from_state(&e.state, false);
    (p.x, p.y)
}

impl Compositor {
    fn new(back: Surface) -> Compositor {
        Compositor {
            back,
            screen: None,
            pointers: Reader::from_now(&POINTER),
            keys: Reader::from_now(&KEYS),
            pointer_queue: VecDeque::new(),
            key_queue: VecDeque::new(),
            generation: 0,
            shown: false,
            pointer: None,
            drawn: None,
        }
    }

    /// The thread: a pass each time it is woken, until a crash (or its
    /// start fails: [`State::abort`]).
    fn run(mut self) {
        loop {
            WAKE.wait_until(|| PENDING.swap(false, Ordering::AcqRel) || crate::keyboard::bytes_pending());
            if COMPOSITOR.lock().as_ref().is_none_or(|s| s.abort) {
                return;
            }
            vt::decode_pending();
            self.pass();
            DONE.wake_all();
        }
    }

    /// One pass (see the module doc).
    fn pass(&mut self) {
        while let Some(r) = self.pointers.read() {
            self.pointer = Some(position(&r.event));
            self.pointer_queue.push_back(r);
        }
        while let Some(r) = self.keys.read() {
            self.key_queue.push_back(r);
        }
        // After reading the rings: a pause written after this read has
        // marks past every event read, so those were the desktop's.
        let cut = *vt::CUT.lock();
        let mut after = After::default();
        let (damage, scene) = {
            let mut g = COMPOSITOR.lock();
            let Some(s) = g.as_mut() else { return };
            let full = self.input(s, &cut, &mut after);
            s.seen_generation = self.generation;
            s.shown = self.shown;
            s.held_back = !self.key_queue.is_empty() || !self.pointer_queue.is_empty();
            if !self.shown {
                // Paused: the damage gathers until the resume, which
                // presents everything.
                drop(g);
                after.finish();
                self.publish(None);
                return;
            }
            let mut damage = s.damage.take();
            if full {
                damage.clear();
                damage.add(Rect::new(0, 0, s.width, s.height));
            }
            s.passing = true;
            (damage, snapshot(s))
        };
        after.finish();
        for r in damage.rects() {
            compose(&mut self.back, *r, &scene);
        }
        let start = crate::time::now_ns();
        let mut pixels = 0;
        if let Some(screen) = self.screen.as_mut() {
            self.back.present(damage.rects(), screen);
            pixels = damage
                .rects()
                .iter()
                .filter_map(|r| r.clip(self.back.width(), self.back.height()))
                .map(|r| r.area())
                .sum();
        }
        let present_ns = crate::time::now_ns().saturating_sub(start);
        let pointer_moved = self.place_pointer(damage.rects());
        let presented = !damage.is_empty() || pointer_moved;
        self.publish(Some((damage.rects().len() as u64, pixels, present_ns, presented)));
        self.run_job();
    }

    /// Act on the input read and the cut (under [`COMPOSITOR`]): whether the
    /// whole screen must be presented (a resume).
    fn input(&mut self, s: &mut State, cut: &vt::Cut, after: &mut After) -> bool {
        let mut full = false;
        while self.generation < cut.generation {
            if self.shown {
                // A pause: the events up to it were the desktop's (keys
                // before the Alt+F`n` press), the rest the terminal's.
                let marks = cut.pause;
                self.feed_pointer(s, after, |seq| seq <= marks.pointer);
                self.feed_keys(s, after, |seq| seq < marks.keys);
                let out = s.wm.interrupt(Interruption::Pause);
                s.apply(out, after);
                self.pointer_queue.clear();
                self.key_queue.clear();
                self.shown = false;
                // `leave_desktop` took the screen back and redrew it.
                self.screen = None;
                self.drawn = None;
            } else {
                // A resume (or the start): its screen comes from `desktop`
                // after the cut; until then, wait.
                let Some(screen) = s.screen.take() else { return full };
                let marks = cut.resume;
                self.pointers.skip_to(marks.pointer);
                self.keys.skip_to(marks.keys);
                self.pointer_queue.retain(|r| r.seq > marks.pointer);
                self.key_queue.retain(|r| r.seq > marks.keys);
                let out = s.wm.resume(cut.baseline.into());
                s.apply(out, after);
                self.screen = Some(screen);
                self.shown = true;
                self.drawn = None;
                self.pointer = POINTER.newest_event().map(|(_, e)| position(&e));
                full = true;
            }
            self.generation += 1;
        }
        if self.shown {
            self.feed_pointer(s, after, |_| true);
            self.feed_keys(s, after, |_| true);
        } else {
            self.pointer_queue.clear();
            self.key_queue.clear();
        }
        full
    }

    /// Pointer frames numbered `upto` into the window manager.
    fn feed_pointer(&mut self, s: &mut State, after: &mut After, upto: impl Fn(lumen_input::Seq) -> bool) {
        while let Some(r) = self.pointer_queue.front() {
            if !upto(r.seq) {
                break;
            }
            let p = Pointer::from_read(r);
            self.pointer_queue.pop_front();
            let out = s.wm.pointer(p);
            s.apply(out, after);
        }
    }

    /// Key events numbered `upto` to the focused window, stopping at an
    /// Alt+F`n` press (the pause's own: the cut that names it comes in the
    /// same pass of the router, read next time).
    fn feed_keys(&mut self, s: &mut State, after: &mut After, upto: impl Fn(lumen_input::Seq) -> bool) {
        while let Some(r) = self.key_queue.front() {
            if !upto(r.seq) || is_switch(&r.event) {
                break;
            }
            let r = *r;
            self.key_queue.pop_front();
            if r.gap() {
                let out = s.wm.interrupt(Interruption::KeyGap);
                s.apply(out, after);
            }
            match s.wm.focused() {
                Some(id) => {
                    let mut took = false;
                    s.push(id, after, |q| {
                        let pushed = q.push(Entry::Key(r.event));
                        took = pushed == Pushed::Queued;
                        Some(pushed)
                    });
                    if took && let Some(w) = s.window(id) {
                        w.keys.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => s.stats.no_focus_keys += 1,
            }
        }
    }

    /// Draw the pointer onto the screen if it moved, or if a rectangle just
    /// presented touched it (which overwrote part of it): its old box taken
    /// off (the back buffer's pixels copied back), the back buffer's pixels
    /// under its new box copied into an array, the arrow painted there, the
    /// array copied to the screen. Whether it changed anything.
    fn place_pointer(&mut self, presented: &[Rect]) -> bool {
        let want = self.pointer.filter(|_| self.shown);
        let touched = self.drawn.is_some_and(|d| presented.iter().any(|r| r.intersect(&arrow_box(d)).is_some()));
        if want == self.drawn && !touched {
            return false;
        }
        let Some(screen) = self.screen.as_mut() else { return false };
        if let Some(d) = self.drawn.take() {
            self.back.copy_to(arrow_box(d), screen, d.0, d.1);
        }
        if let Some((x, y)) = want
            && let Some(b) = arrow_box((x, y)).clip(self.back.width(), self.back.height())
            && b.x() == x
            && b.y() == y
        {
            let (w, h) = (b.w() as usize, b.h() as usize);
            let mut px = [0u32; SPRITE_W * SPRITE_H];
            for row in 0..h {
                let src = &self.back.row(b.y() as u32 + row as u32)[b.x() as usize..b.x() as usize + w];
                px[row * SPRITE_W..row * SPRITE_W + w].copy_from_slice(src);
            }
            draw_arrow(&mut px, SPRITE_W, (w, h), (0, 0), THEME.pointer_outline, THEME.pointer_fill);
            for row in 0..h {
                screen.write_span(b.x() as u32, b.y() as u32 + row as u32, &px[row * SPRITE_W..row * SPRITE_W + w]);
            }
            self.drawn = Some((x, y));
        }
        true
    }

    /// The pass is over: its statistics and the readers' places.
    fn publish(&self, presented: Option<(u64, u64, u64, bool)>) {
        let mut g = COMPOSITOR.lock();
        let Some(s) = g.as_mut() else { return };
        s.passing = false;
        s.pointer_read = self.pointers.after();
        s.keys_read = self.keys.after();
        s.lost = self.pointers.lost() + self.keys.lost();
        if let Some((rects, pixels, ns, frame)) = presented {
            s.stats.rects += rects;
            s.stats.pixels += pixels;
            s.stats.present_ns += ns;
            s.stats.frames += u64::from(frame);
        }
    }

    /// A job asked for meanwhile (`desktop check`, `screenshot`), run right
    /// after the pass.
    fn run_job(&mut self) {
        let (job, scene, committed) = {
            let mut g = COMPOSITOR.lock();
            let Some(s) = g.as_mut() else { return };
            let Some(job) = s.job.take() else { return };
            // A check compares with a full repaint of the scene as it is:
            // damage or a commit since the pass makes that a moving target.
            // (A capture copies the back buffer as the pass left it.)
            if matches!(job, Job::Check(_)) && (!s.damage.is_empty() || s.commits > 0) {
                s.answer = Some(Answer::Busy);
                drop(g);
                ANSWERED.store(true, Ordering::Release);
                return;
            }
            (job, snapshot(s), s.committed)
        };
        let answer = match job {
            Job::Check(mut scratch) => {
                let all = scratch.bounds();
                compose(&mut scratch, all, &scene);
                let repaint = (0..self.back.height())
                    .map(|y| {
                        self.back
                            .row(y)
                            .iter()
                            .zip(scratch.row(y))
                            .filter(|(a, b)| (**a ^ **b) & 0xff_ffff != 0)
                            .count() as u64
                    })
                    .sum();
                let screen = self.screen_differences();
                let g = COMPOSITOR.lock();
                let busy = g.as_ref().is_none_or(|s| !s.damage.is_empty() || s.commits > 0 || s.committed != committed);
                if busy { Answer::Busy } else { Answer::Check { repaint, screen } }
            }
            Job::Capture(mut rows) => {
                let start = crate::time::now_ns();
                for y in 0..rows.height().min(self.back.height()) {
                    let src = self.back.row(y);
                    for (out, &p) in rows.row_mut(y).iter_mut().zip(src) {
                        *out = [(p >> 16) as u8, (p >> 8) as u8, p as u8];
                    }
                }
                Answer::Capture { rows, ns: crate::time::now_ns().saturating_sub(start) }
            }
        };
        if let Some(s) = COMPOSITOR.lock().as_mut() {
            s.answer = Some(answer);
        }
        ANSWERED.store(true, Ordering::Release);
    }

    /// Pixels on the screen that differ from the back buffer, outside the
    /// pointer's box.
    fn screen_differences(&self) -> u64 {
        let Some(screen) = self.screen.as_ref() else { return u64::MAX };
        let arrow = self.drawn.map(arrow_box);
        let mut row = alloc::vec![0u32; self.back.width() as usize];
        let mut differ = 0;
        for y in 0..self.back.height() {
            if !screen.read_span(0, y as usize, &mut row) {
                return u64::MAX;
            }
            for (x, (a, b)) in self.back.row(y).iter().zip(&row).enumerate() {
                if (a ^ b) & 0xff_ffff != 0 && !arrow.is_some_and(|r| r.contains(x as i32, y as i32)) {
                    differ += 1;
                }
            }
        }
        differ
    }
}

/// The stacking order, back to front, as a pass draws it.
fn snapshot(s: &State) -> Vec<Snap> {
    let focused = s.wm.focused();
    s.wm.windows()
        .iter()
        .filter_map(|w| {
            let win = s.window(w.id())?.clone();
            Some(Snap { win, frame: w.frame(), focused: focused == Some(w.id()) })
        })
        .collect()
}

// -- starting, resuming, the commands --------------------------------------------

/// Why the desktop didn't start or resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// No terminal on a framebuffer (or a crash report took the screen).
    NoScreen,
    /// No memory for the back buffer.
    NoMemory,
    /// No thread for the compositor.
    NoThread(crate::sched::SpawnError),
    /// The screen couldn't be taken (a command holds a canvas).
    Screen(CanvasError),
    /// The compositor hasn't caught up with the last pause.
    Behind,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::NoScreen => f.write_str("no screen to draw on"),
            StartError::NoMemory => f.write_str("no memory for the back buffer"),
            StartError::NoThread(e) => write!(f, "no thread for the compositor: {e}"),
            StartError::Screen(e) => write!(f, "{e}"),
            StartError::Behind => f.write_str("the compositor hasn't caught up with the last pause"),
        }
    }
}

/// What `start` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    Started,
    Resumed,
    /// It was shown already.
    Shown,
}

/// Set while a start or resume runs (two shells typing `desktop` at once:
/// the second is refused).
static START_BUSY: AtomicBool = AtomicBool::new(false);

/// Start the desktop, or resume it (see the module doc).
pub fn start() -> Result<Started, StartError> {
    if START_BUSY.swap(true, Ordering::AcqRel) {
        return Err(StartError::Screen(CanvasError::InUse));
    }
    let result = start_or_resume();
    START_BUSY.store(false, Ordering::Release);
    result
}

fn start_or_resume() -> Result<Started, StartError> {
    let state = COMPOSITOR.lock().as_ref().map(|s| s.owner);
    match state {
        Some(owner) => resume(owner),
        None => first_start(),
    }
}

/// The first `desktop`.
fn first_start() -> Result<Started, StartError> {
    let (w, h) = term::screen_size().ok_or(StartError::NoScreen)?;
    let (w, h) = (w as u32, h as u32);
    let back = Surface::with_stride(w, h, w as usize).ok_or(StartError::NoMemory)?;
    let owner = Owner::new();
    *COMPOSITOR.lock() = Some(State::new(w, h, owner));
    let compositor = Compositor::new(back);
    let thread = match crate::sched::spawn("compositor", move || compositor.run()) {
        Ok(t) => t,
        Err(e) => {
            *COMPOSITOR.lock() = None;
            return Err(StartError::NoThread(e));
        }
    };
    RUNNING.store(true, Ordering::Release);
    match vt::enter_desktop(owner) {
        Ok(screen) => {
            if let Some(s) = COMPOSITOR.lock().as_mut() {
                s.screen = Some(screen);
            }
            drop(thread);
            wake();
            open_demo_windows();
            log::info!(target: "desktop", "started on {w}x{h}");
            Ok(Started::Started)
        }
        Err(e) => {
            if let Some(s) = COMPOSITOR.lock().as_mut() {
                s.abort = true;
            }
            PENDING.store(true, Ordering::Release);
            WAKE.wake_all();
            thread.join();
            RUNNING.store(false, Ordering::Release);
            *COMPOSITOR.lock() = None;
            Err(StartError::Screen(e))
        }
    }
}

/// `desktop` while started: resume it if paused.
fn resume(owner: Owner) -> Result<Started, StartError> {
    if vt::DESKTOP.load(Ordering::Acquire) {
        return Ok(Started::Shown);
    }
    // The compositor acts on the pause before this resume writes the next
    // cut, so the cut never needs more than its latest marks.
    let deadline = crate::time::now_ns().saturating_add(5_000_000_000);
    let caught_up = || {
        let generation = vt::CUT.lock().generation;
        COMPOSITOR.lock().as_ref().is_some_and(|s| s.seen_generation == generation)
    };
    wake();
    if !DONE.wait_until_or_deadline(deadline, caught_up) && !caught_up() {
        return Err(StartError::Behind);
    }
    let screen = vt::enter_desktop(owner).map_err(StartError::Screen)?;
    if let Some(s) = COMPOSITOR.lock().as_mut() {
        s.screen = Some(screen);
    }
    wake();
    Ok(Started::Resumed)
}

/// Start the desktop for the `lumen.desktop` boot flag (a warning if it
/// can't, and the boot goes on in text mode).
pub fn start_at_boot() {
    if !crate::cmdline::has_flag("lumen.desktop") {
        return;
    }
    if let Err(e) = start() {
        log::warn!(target: "desktop", "not started: {e}");
    }
}

/// Whether the desktop is shown (its screen live).
pub fn is_shown() -> bool {
    vt::DESKTOP.load(Ordering::Acquire)
}

/// Hand `job` to the compositor and wait (up to 20 s) for its answer:
/// `Err` with why it wasn't run.
fn run_job(job: Job) -> Result<Answer, &'static str> {
    {
        let mut g = COMPOSITOR.lock();
        let s = g.as_mut().ok_or("no desktop")?;
        if !is_shown() {
            return Err("paused (`desktop` resumes it)");
        }
        if s.job.is_some() || s.answer.is_some() {
            return Err("busy with another request");
        }
        ANSWERED.store(false, Ordering::Release);
        s.job = Some(job);
    }
    wake();
    let deadline = crate::time::now_ns().saturating_add(20_000_000_000);
    let answered = DONE.wait_until_or_deadline(deadline, || ANSWERED.load(Ordering::Acquire));
    let mut g = COMPOSITOR.lock();
    let s = g.as_mut().ok_or("no desktop")?;
    if !answered && s.answer.is_none() {
        s.job = None;
        return Err("no answer from the compositor in 20 s");
    }
    s.answer.take().ok_or("no answer from the compositor")
}

/// The back buffer as RGB rows (`screenshot` while the desktop is shown):
/// `None` if it isn't (then the text screen is the source), with how long
/// the copy took.
pub fn capture(rows: Rows) -> Option<Result<(Rows, u64), &'static str>> {
    if !is_shown() {
        return None;
    }
    Some(match run_job(Job::Capture(Box::new(rows))) {
        Ok(Answer::Capture { rows, ns }) => Ok((*rows, ns)),
        Ok(_) => Err("the compositor answered something else"),
        Err(e) => Err(e),
    })
}

// -- the demo windows and `desktop spin` --------------------------------------------

/// The two windows the desktop starts with.
#[derive(Clone, Copy)]
enum Demo {
    Welcome,
    Colours,
}

const WELCOME_LINES: [&str; 4] = [
    "Welcome to the Lumen desktop.",
    "Drag a title bar to move a window,",
    "click a window to bring it to the front.",
    "Alt+F1 shows the terminals; `desktop` comes back.",
];

/// Open the demo windows, each with its owner thread (a warning for one
/// that can't open).
fn open_demo_windows() {
    let demos = [
        (Demo::Welcome, "Welcome", 440, 210, (96, 96), "desktop-welcome"),
        (Demo::Colours, "Colours", 400, 210, (440, 300), "desktop-colours"),
    ];
    for (demo, title, w, h, at, thread) in demos {
        let handle = match (Desktop { home: Home::Live }).open(title, w, h, Some(at), false) {
            Ok(h) => h,
            Err(e) => {
                log::warn!(target: "desktop", "no {title} window: {e}");
                continue;
            }
        };
        if let Err(e) = crate::sched::spawn(thread, move || demo_owner(handle, demo)) {
            log::warn!(target: "desktop", "no thread for the {title} window: {e}");
        }
    }
}

/// The row of a demo window's "last key" line.
const LAST_KEY_Y: i32 = 180;

/// A demo window's owner: draw it, then show the last key typed in it
/// until its close box is clicked.
fn demo_owner(mut h: WindowHandle, demo: Demo) {
    let bounds = h.surface_mut().bounds();
    {
        let s = h.surface_mut();
        s.fill(THEME.client);
        match demo {
            Demo::Welcome => {
                for (i, line) in WELCOME_LINES.iter().enumerate() {
                    draw_text(s, 16, 16 + 24 * i as i32, line, THEME.client_text, FontWeight::Regular, bounds);
                }
            }
            Demo::Colours => {
                const SWATCHES: [u32; 8] =
                    [0xe53935, 0xfb8c00, 0xfdd835, 0x43a047, 0x1e88e5, 0x3949ab, 0x8e24aa, 0x546e7a];
                for (i, &c) in SWATCHES.iter().enumerate() {
                    let (col, row) = (i as i32 % 4, i as i32 / 4);
                    s.fill_rect(Rect::new(16 + col * 94, 16 + row * 76, 84, 66), c);
                }
            }
        }
    }
    draw_last_key(&mut h, None);
    h.commit(&[bounds]);
    loop {
        match h.wait_event() {
            WindowEvent::Close => return,
            WindowEvent::Key(KeyEvent { code, action: KeyAction::Press { text }, .. }) => {
                let line = draw_last_key(&mut h, Some((code, text)));
                h.commit(&[line]);
            }
            _ => {}
        }
    }
}

/// The "last key: ..." line of a demo window; the rectangle drawn.
fn draw_last_key(h: &mut WindowHandle, key: Option<(lumen_input::KeyCode, Option<char>)>) -> Rect {
    let s = h.surface_mut();
    let line = Rect::new(0, LAST_KEY_Y, s.width(), 20);
    s.fill_rect(line, THEME.client);
    let mut text = String::from("last key: ");
    match key {
        None => text.push('-'),
        Some((_, Some(c))) if !c.is_control() => text.push(c),
        Some((code, _)) => {
            use core::fmt::Write;
            let _ = write!(text, "{code:?}");
        }
    }
    draw_text(s, 16, LAST_KEY_Y + 2, &text, THEME.client_text, FontWeight::Regular, line);
    line
}

/// `desktop spin`'s window: a pure green bar moving every 20 ms, its
/// events read meanwhile (not while `desktop hold` holds it).
fn spin_owner(mut h: WindowHandle) {
    const GREEN: u32 = 0x00ff00;
    let bounds = h.surface_mut().bounds();
    let mut x = 0i32;
    loop {
        let s = h.surface_mut();
        s.fill(THEME.client);
        s.fill_rect(Rect::new(x, 20, 40, 20), GREEN);
        h.commit(&[bounds]);
        x = (x + 8) % (bounds.w() as i32 - 40).max(1);
        if !h.held() {
            while let Some(e) = h.next_event() {
                if e == WindowEvent::Close {
                    return;
                }
            }
        }
        let _ = crate::sched::sleep(20_000_000);
    }
}

/// `desktop spin`: open its window and start its owner.
fn spin() -> Result<u32, String> {
    let h = (Desktop { home: Home::Live })
        .open("spin", 200, 60, None, true)
        .map_err(|e| alloc::format!("no window: {e}"))?;
    let id = h.id();
    let _detached =
        crate::sched::spawn("desktop-spin", move || spin_owner(h)).map_err(|e| alloc::format!("no thread: {e}"))?;
    Ok(id)
}

/// `desktop hold <id>` / `desktop release <id>`.
fn hold(id: &str, held: bool) -> Result<(), String> {
    let n: u32 = id.parse().map_err(|_| String::from(USAGE))?;
    let g = COMPOSITOR.lock();
    let s = g.as_ref().ok_or_else(|| String::from("no desktop"))?;
    let win = s.wm.id(n).and_then(|id| s.window(id)).ok_or_else(|| alloc::format!("no window {n}"))?;
    if !win.spin {
        return Err(alloc::format!("window {n} isn't a `desktop spin` window"));
    }
    win.held.store(held, Ordering::Release);
    Ok(())
}

pub const USAGE: &str = "usage: desktop [status|check|spin|hold <window>|release <window>]";

// -- records -----------------------------------------------------------------------

use lumen_shell::pipe::{self, Align, Column, Kind, Value};

fn int(n: u64) -> Value {
    i64::try_from(n).map_or(Value::Absent("-"), Value::Int)
}

fn text(s: &str) -> Value {
    Value::Text(String::from(s))
}

fn yes(b: bool) -> Value {
    text(if b { "yes" } else { "no" })
}

/// A window's row of `windows`.
struct WindowRow {
    id: u32,
    title: String,
    frame: Rect,
    z: usize,
    focused: bool,
    owner: &'static str,
    lost: u64,
    keys: u64,
}

/// `windows`: one row per window, front to back; `Err` without a desktop.
pub fn windows_table() -> Result<pipe::Table, String> {
    const COLUMNS: [Column; 12] = [
        Column::new("id", Kind::Int, Align::Right, 2),
        Column::new("title", Kind::Text, Align::Left, 8),
        Column::new("x", Kind::Int, Align::Right, 4),
        Column::new("y", Kind::Int, Align::Right, 4),
        Column::new("w", Kind::Int, Align::Right, 4),
        Column::new("h", Kind::Int, Align::Right, 4),
        Column::new("z", Kind::Int, Align::Right, 1),
        Column::new("focused", Kind::Text, Align::Left, 7),
        Column::new("minimized", Kind::Text, Align::Left, 9),
        Column::new("owner", Kind::Text, Align::Left, 8),
        Column::new("lost", Kind::Int, Align::Right, 4),
        Column::new("keys", Kind::Int, Align::Right, 4),
    ];
    let rows: Vec<WindowRow> = {
        let g = COMPOSITOR.lock();
        let s = g.as_ref().ok_or_else(|| String::from("windows: no desktop"))?;
        let focused = s.wm.focused();
        s.wm.windows()
            .iter()
            .rev()
            .enumerate()
            .filter_map(|(z, w)| {
                let win = s.window(w.id())?;
                let (owner, lost) = {
                    let inner = win.inner.lock();
                    (inner.owner, inner.queue.counts().lost())
                };
                Some(WindowRow {
                    id: w.id().get(),
                    title: String::from(w.title()),
                    frame: w.frame(),
                    z,
                    focused: focused == Some(w.id()),
                    owner,
                    lost,
                    keys: win.keys.load(Ordering::Relaxed),
                })
            })
            .collect()
    };
    let mut t = pipe::Table::new(&COLUMNS);
    for r in rows {
        t.push(alloc::vec![
            int(u64::from(r.id)),
            Value::Text(r.title),
            Value::Int(i64::from(r.frame.x())),
            Value::Int(i64::from(r.frame.y())),
            int(u64::from(r.frame.w())),
            int(u64::from(r.frame.h())),
            int(r.z as u64),
            yes(r.focused),
            yes(false),
            text(r.owner),
            int(r.lost),
            int(r.keys),
        ]);
    }
    Ok(t)
}

/// `desktop status`: one record.
pub fn status_table() -> pipe::Table {
    const COLUMNS: [Column; 24] = [
        Column::new("state", Kind::Text, Align::Left, 5),
        Column::new("windows", Kind::Int, Align::Right, 7),
        Column::new("frames", Kind::Int, Align::Right, 6),
        Column::new("rects", Kind::Int, Align::Right, 5),
        Column::new("pixels", Kind::Int, Align::Right, 6),
        Column::new("present_ms", Kind::Text, Align::Right, 10),
        Column::new("pointer_read", Kind::Int, Align::Right, 12),
        Column::new("pointer_newest", Kind::Int, Align::Right, 14),
        Column::new("keys_read", Kind::Int, Align::Right, 9),
        Column::new("keys_newest", Kind::Int, Align::Right, 11),
        Column::new("lost", Kind::Int, Align::Right, 4),
        Column::new("damage", Kind::Int, Align::Right, 6),
        Column::new("no_focus_keys", Kind::Int, Align::Right, 13),
        Column::new("stray_forgotten", Kind::Int, Align::Right, 15),
        Column::new("stray_unpressed", Kind::Int, Align::Right, 15),
        Column::new("key_releases_unpaired", Kind::Int, Align::Right, 21),
        Column::new("ignored_presses", Kind::Int, Align::Right, 15),
        Column::new("rationed", Kind::Int, Align::Right, 8),
        Column::new("overflowed", Kind::Int, Align::Right, 10),
        Column::new("grab_ends", Kind::Int, Align::Right, 9),
        Column::new("dead_pushes", Kind::Int, Align::Right, 11),
        Column::new("cancelled", Kind::Int, Align::Right, 9),
        Column::new("late", Kind::Int, Align::Right, 4),
        Column::new("settled", Kind::Text, Align::Left, 7),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    // Read before the desktop's lock: the keyboard's, the rings' and the
    // cut's (none of them is taken under it).
    let all_decoded = crate::keyboard::all_decoded();
    let (pointer_newest, keys_newest) = (POINTER.newest(), KEYS.newest());
    let cut = *vt::CUT.lock();
    let pending = PENDING.load(Ordering::Acquire);
    let g = COMPOSITOR.lock();
    let Some(s) = g.as_ref() else {
        let mut row = alloc::vec![text("off")];
        row.extend((1..COLUMNS.len() - 1).map(|_| Value::Absent("-")));
        row.push(text("no"));
        t.push(row);
        return t;
    };
    let mut counts = s.closed;
    let mut windows_idle = true;
    for w in &s.windows {
        let inner = w.inner.lock();
        add_counts(&mut counts, &inner.queue.counts());
        windows_idle &= inner.queue.is_idle() && w.idle.load(Ordering::Acquire);
    }
    let shown = vt::DESKTOP.load(Ordering::Acquire);
    let settled = shown
        && s.shown
        && s.seen_generation == cut.generation
        && all_decoded
        && s.pointer_read == pointer_newest
        && s.keys_read == keys_newest
        && !s.held_back
        && s.damage.is_empty()
        && !s.passing
        && s.commits == 0
        && !pending
        && windows_idle;
    let wm = s.wm.counts();
    let ms = s.stats.present_ns / 1_000_000;
    let us = (s.stats.present_ns / 1_000) % 1_000;
    t.push(alloc::vec![
        text(if shown { "shown" } else { "paused" }),
        int(s.windows.len() as u64),
        int(s.stats.frames),
        int(s.stats.rects),
        int(s.stats.pixels),
        Value::Text(alloc::format!("{ms}.{us:03}")),
        int(s.pointer_read.get()),
        int(pointer_newest.get()),
        int(s.keys_read.get()),
        int(keys_newest.get()),
        int(s.lost),
        int(s.damage.rects().len() as u64),
        int(s.stats.no_focus_keys),
        int(counts.stray_forgotten),
        int(counts.stray_unpressed),
        int(counts.key_releases_unpaired),
        int(wm.ignored_presses),
        int(counts.rationed),
        int(counts.overflowed),
        int(wm.grab_ends),
        int(counts.dead_pushes),
        int(0),
        int(0),
        yes(settled),
    ]);
    t
}

/// `desktop`'s records: `desktop status`'s.
pub fn records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    match args {
        [what] if what == "status" => Some(Ok(status_table())),
        _ => None,
    }
}

/// `windows`'s records.
pub fn windows_records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    args.is_empty().then(windows_table)
}

/// The shell's `desktop`.
pub fn command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    let print = |ctx: &mut crate::shell::Ctx<'_>, t: pipe::Table| {
        let mut out = String::new();
        t.render(&mut out);
        let _ = ctx.out.write_str(&out);
    };
    match args {
        [] => match start() {
            Ok(Started::Shown) => print(ctx, status_table()),
            Ok(Started::Started) => {
                let _ = writeln!(ctx.out, "desktop: started (Alt+F1 shows the terminals)");
            }
            Ok(Started::Resumed) => {
                let _ = writeln!(ctx.out, "desktop: resumed");
            }
            Err(e) => ctx.fail(format_args!("desktop: not started: {e}")),
        },
        [what] if what == "status" => print(ctx, status_table()),
        [what] if what == "check" => {
            if let Some(why) = refused_while_paused() {
                return ctx.fail(why);
            }
            let Some(scratch) = COMPOSITOR.lock().as_ref().map(|s| (s.width, s.height)) else {
                return ctx.fail("desktop: no desktop");
            };
            let Some(scratch) = Surface::with_stride(scratch.0, scratch.1, scratch.0 as usize) else {
                return ctx.fail("desktop check: no memory for the scratch surface");
            };
            match run_job(Job::Check(Box::new(scratch))) {
                Ok(Answer::Check { repaint, screen }) => {
                    let _ = writeln!(
                        ctx.out,
                        "desktop check: {repaint} pixels differ from a full repaint, {screen} from the screen"
                    );
                }
                Ok(Answer::Busy) => {
                    let _ = writeln!(ctx.out, "desktop check: busy");
                }
                Ok(Answer::Capture { .. }) => ctx.fail("desktop check: the compositor answered something else"),
                Err(e) => ctx.fail(format_args!("desktop check: {e}")),
            }
        }
        [what] if what == "spin" => {
            if let Some(why) = refused_while_paused() {
                return ctx.fail(why);
            }
            match spin() {
                Ok(id) => {
                    let _ = writeln!(ctx.out, "desktop spin: window {id}");
                }
                Err(e) => ctx.fail(format_args!("desktop spin: {e}")),
            }
        }
        [what, id] if what == "hold" || what == "release" => {
            if let Some(why) = refused_while_paused() {
                return ctx.fail(why);
            }
            match hold(id, what == "hold") {
                Ok(()) => {
                    let _ = writeln!(ctx.out, "desktop {what}: window {id}");
                }
                Err(e) => ctx.fail(format_args!("desktop {what}: {e}")),
            }
        }
        _ => ctx.fail(USAGE),
    }
}

/// Why `desktop check`, `spin`, `hold` and `release` refuse now, if they
/// do.
fn refused_while_paused() -> Option<&'static str> {
    let started = COMPOSITOR.lock().is_some();
    if !started {
        Some("desktop: not started (`desktop` starts it)")
    } else if !is_shown() {
        Some("desktop: paused (`desktop` resumes it)")
    } else {
        None
    }
}

/// The shell's `windows`.
pub fn windows_command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: windows");
    }
    match windows_table() {
        Ok(t) => {
            let mut out = String::new();
            t.render(&mut out);
            let _ = ctx.out.write_str(&out);
        }
        Err(e) => {
            let _ = writeln!(ctx.out, "{e}");
        }
    }
}

// -- ktests ------------------------------------------------------------------------

crate::ktest!(
    fn opening_past_the_window_limit_fails() {
        // On a desktop with no screen: 32 windows open, the 33rd is
        // refused; closing one makes room again (its id is never reused);
        // an empty or oversized client is refused; the live desktop, not
        // started in the test kernel, refuses every window.
        use crate::{kassert, kassert_eq};
        let d = Desktop::without_screen();
        let mut open = Vec::new();
        for _ in 0..MAX_WINDOWS {
            open.push(d.open_window("w", 16, 16).map_err(|_| "a window under the limit")?);
        }
        kassert_eq!(d.open_window("w", 16, 16).err(), Some(OpenError::TooMany));
        let last = open.pop().ok_or("no window")?.id();
        let again = d.open_window("w", 16, 16).map_err(|_| "room after a close")?;
        kassert!(again.id() > last);
        kassert_eq!(d.open_window("w", 16, 16).err(), Some(OpenError::TooMany));
        drop(open);
        kassert_eq!(d.open_window("w", 0, 16).err(), Some(OpenError::TooBig));
        kassert_eq!(d.open_window("w", 1281, 16).err(), Some(OpenError::TooBig));
        kassert_eq!(d.open_window("w", 16, 801).err(), Some(OpenError::TooBig));
        kassert!(d.open_window("w", 1280, 800).is_ok());
        kassert_eq!(open_window("w", 16, 16).err(), Some(OpenError::NoDesktop));
        Ok(())
    }
);

crate::ktest!(
    fn a_present_stops_at_the_next_row_once_the_canvas_dies() {
        // The real span path: the whole screen taken (`term::take_screen`),
        // a 40-row band presented from a surface of one colour, the epoch
        // changed by the test hook after 3 rows (as `leave_desktop` or a
        // crash report would change it). Read back under the screen's lock
        // before the canvas is dropped (which redraws the terminal): the
        // first 3 rows are the colour, every later row is as it was.
        use crate::kassert;
        let mut screen = term::take_screen(Owner::new()).map_err(|_| "no screen")?;
        let (w, h) = screen.size();
        let (top, rows, written) = (200usize, 40usize, 3usize);
        let width = w as usize;
        let colour = 0x00c0_ffee;
        let read = |into: &mut Vec<u32>| {
            term::read_pixels(|fb| {
                for (i, p) in into.iter_mut().enumerate() {
                    *p = fb.pixel(i % width, top + i / width);
                }
            })
        };
        let mut before = alloc::vec![0u32; width * rows];
        kassert!(read(&mut before) == Some(true));
        let mut back = Surface::new(w, h);
        back.fill(colour);
        screen.kill_after(written);
        back.present(&[Rect::new(0, top as i32, w, rows as u32)], &mut screen);
        kassert!(!screen.is_live());
        let mut after = alloc::vec![0u32; width * rows];
        let mut encoded = 0;
        kassert!(
            term::read_pixels(|fb| {
                encoded = fb.encode(colour);
                for (i, p) in after.iter_mut().enumerate() {
                    *p = fb.pixel(i % width, top + i / width);
                }
            }) == Some(true)
        );
        drop(screen);
        let (first, rest) = after.split_at(width * written);
        kassert!(first.iter().all(|&p| p == encoded));
        kassert!(rest == &before[width * written..]);
        kassert!(before.iter().any(|&p| p != encoded));
        Ok(())
    }
);
