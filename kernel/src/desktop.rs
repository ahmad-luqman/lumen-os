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
//! - **A pass** ([`Compositor::pass`], after the keyboard's decoding pass,
//!   `vt::decode_pending`, which [`Compositor::run`] calls holding none of
//!   the desktop's locks): both input rings read (pointer frames through
//!   `lumen_wm::Wm` into focus, drags, the close box and windows' pointer
//!   events; key events to the focused window), the cut acted on (a pause
//!   or a resume), then under [`COMPOSITOR`] the damage and a snapshot of
//!   the stacking order taken; only the damaged rectangles are recomposed
//!   in the back buffer and copied to the screen, then the pointer is drawn
//!   straight onto the screen (never into the back buffer).
//! - **The live state** is a [`State`] in [`COMPOSITOR`]; the statics are
//!   private to this module, so outside it the lock order below holds as
//!   long as callers of its functions hold none of those locks.
//! - **Locks**, in order: [`COMPOSITOR`] (the window manager, the damage,
//!   the statistics), a window's lock (its committed surface and queue),
//!   the heap's, the log's line lock, the screen's. The input rings' and
//!   the cut's locks are leaves, taken under any of them. Nothing holding
//!   the router lock (`crate::vt`), the log's or the screen's takes one of
//!   the desktop's, and the desktop takes the router lock holding none of
//!   its own. A wait queue's lock comes before [`COMPOSITOR`] (`desktop`
//!   and a request's asker check [`COMPOSITOR`] in their wake-up
//!   predicates, under [`DONE`]'s lock); every wake happens after the lock
//!   that guarded the change, so no wake is ever made holding it.
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use lumen_font::{Font, Px};
use lumen_gfx::{Damage, Mask, Rect, Surface, Target};
use lumen_input::cursor::{SPRITE_H, SPRITE_W, draw_arrow};
use lumen_input::{Button, Buttons, KeyAction, KeyEvent, PointerEvent, Read, Reader, Seq};
use lumen_png::encode::Rows;
use lumen_wm::{
    Entry, EventQueue, Interruption, Metrics, Next, Outcome, Pointer, PointerEntry, Pushed, QueueCounts, Theme,
    WindowId, Wm,
};

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
static WAKE: WaitQueue = WaitQueue::new();
/// Set by [`wake`]; the compositor clears it as it wakes.
static PENDING: AtomicBool = AtomicBool::new(false);
/// Set once the compositor thread exists: [`wake`] does nothing before.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Woken after every pass: a request's asker, and `desktop` waiting for
/// the compositor to catch up with the cut. Their predicates read
/// [`COMPOSITOR`].
static DONE: WaitQueue = WaitQueue::new();
/// The live desktop's state: `None` until it starts (it never ends in M4).
static COMPOSITOR: SpinLock<Option<State>> = SpinLock::new(None);

/// Wake the compositor (nothing before the desktop started): IRQ 1 when
/// it queued bytes, the `mouse` thread after pushing frames, every commit
/// and request, a pause or a resume. It sets [`PENDING`] itself. Called
/// with no desktop lock held (any other lock is fine: it takes only the
/// wait queue's).
pub fn wake() {
    if RUNNING.load(Ordering::Acquire) {
        PENDING.store(true, Ordering::Release);
        WAKE.wake_all();
    }
}

/// The frames' geometry.
const METRICS: Metrics = Metrics::DESKTOP;

// -- the look: the theme, the text, the wallpaper (#154) ---------------------------

/// The theme drawn with, an index into `Theme::ALL`: light, until
/// `lumen.theme=` or `theme` picks another ([`switch_theme`]).
static THEME: AtomicUsize = AtomicUsize::new(0);
/// Bumped by every switch: a window whose count is behind is told
/// ([`WindowEvent::Theme`]) before anything else, and redraws.
static THEME_GEN: AtomicU64 = AtomicU64::new(0);

/// The theme the desktop draws with now (owners draw their client areas
/// with it, and redraw on [`WindowEvent::Theme`]).
pub fn theme() -> &'static Theme {
    Theme::ALL[THEME.load(Ordering::Acquire).min(Theme::ALL.len() - 1)]
}

/// The UI font's sizes: a title, a window's text and its heading.
const TITLE_PX: Px = px(15);
const TEXT_PX: Px = px(16);
const HEADING_PX: Px = px(24);

const fn px(n: u16) -> Px {
    match Px::new(n) {
        Some(px) => px,
        None => panic!("a text size out of range"),
    }
}

/// Each drawing thread's glyph cache (the compositor's and each owner's):
/// far more than the few sizes and glyphs here take, so a redraw hits.
const FONT_CACHE: usize = 128 * 1024;

/// Set once one of the warnings below is said (each said once a boot).
static NO_FONT_SAID: AtomicBool = AtomicBool::new(false);
static BAD_MASK_SAID: AtomicBool = AtomicBool::new(false);

/// A glyph cache over the shared UI font (`crate::font::face`, parsed on
/// first use), or `None` if the font didn't load (one warning: then no text
/// is drawn).
fn text_font() -> Option<Font<'static>> {
    match crate::font::face() {
        Ok(loaded) => Some(Font::new(loaded.face(), FONT_CACHE)),
        Err(e) => {
            if !NO_FONT_SAID.swap(true, Ordering::Relaxed) {
                log::warn!(target: "desktop", "no text: the UI font didn't load: {e}");
            }
            None
        }
    }
}

/// The wallpaper the kernel embeds (`tools/wallpaper.py` made it), decoded
/// once when the desktop starts.
static WALLPAPER_PNG: &[u8] = include_bytes!("../../assets/wallpaper/lumen.png");

/// An automation request (#156 fills this in): none can be made yet, so
/// no window's queue ever holds one. Not `Clone`: #156's requests answer
/// for themselves when dropped, so a copy would answer twice.
#[derive(Debug)]
pub enum Request {}

/// A window's event queue.
type Queue = EventQueue<KeyEvent, Request>;

/// What a window's owner is told ([`WindowHandle::next_event`]). Pointer
/// positions are in its client area's coordinates (outside it while a
/// grab follows the pointer). Not `Clone` and not compared (it may carry a
/// [`Request`]): match it (`matches!(e, WindowEvent::Close)`).
#[derive(Debug)]
#[allow(dead_code, reason = "the pointer events' fields are read by #155-#157's owners; the demos read keys only")]
pub enum WindowEvent {
    Key(KeyEvent),
    PointerDown {
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
    },
    PointerMove {
        x: i32,
        y: i32,
        buttons: Buttons,
    },
    PointerUp {
        x: i32,
        y: i32,
        button: Button,
        buttons: Buttons,
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
    /// The desktop's theme changed (`theme`, #154): redraw the client area
    /// in its colours. Delivered before anything queued, never dropped (a
    /// count per window, not a queue entry).
    Theme(&'static Theme),
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
    /// Whether the queue holds an entry or a flag (`!is_idle()`): an atomic
    /// outside the lock, stored under it after every change, so the
    /// owner's wake-up check reads it without the lock.
    queued: AtomicBool,
    /// Set while the owner waits in [`WindowHandle::wait_event`] (an atomic
    /// outside the lock).
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
    /// The theme switches its owner has been told of ([`THEME_GEN`]'s count
    /// when it last read an event).
    theme_seen: AtomicU64,
}

impl Window {
    /// Whether a theme switch hasn't reached its owner yet.
    fn theme_behind(&self) -> bool {
        self.theme_seen.load(Ordering::Acquire) != THEME_GEN.load(Ordering::Acquire)
    }
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
    /// The desktop hasn't started (or is still starting).
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

/// `desktop check`'s and `screenshot`'s one request slot, its jobs and
/// answers: a module of their own, so only [`Slot::start`] makes a
/// [`Ticket`].
mod slot {
    use super::{Box, Rows, Surface, fmt};

    /// What the compositor is asked to do right after a pass.
    pub(super) enum Job {
        /// `desktop check`, with its scratch surface.
        Check(Box<Surface>),
        /// `screenshot`: the back buffer into these rows.
        Capture(Box<Rows>),
    }

    /// The one request slot, reserved from the moment a request is accepted
    /// until its asker takes the answer (or gives up), so a second request
    /// meanwhile is refused and no asker ever gets another's answer.
    pub(super) enum Slot {
        Free,
        /// Accepted, waiting for the compositor's next pass.
        Queued(Job),
        /// The compositor is running it (taken out of the slot, the lock
        /// released); `abandoned` once its asker gave up waiting.
        Running {
            abandoned: bool,
        },
        /// Done, waiting for its asker.
        Answered(Answer),
    }

    /// A job taken out of the slot to run: only it can finish the slot, so an
    /// answer can't land in a slot whose job wasn't started (only this module
    /// makes one).
    #[must_use = "a started job's ticket finishes the slot"]
    pub(super) struct Ticket(());

    impl Slot {
        /// Accept `job` if the slot is free: refused as busy while another
        /// request holds it, or as stuck while the compositor still runs one
        /// its asker gave up on.
        pub(super) fn accept(&mut self, job: Job) -> Result<(), JobError> {
            match self {
                Slot::Free => {
                    *self = Slot::Queued(job);
                    Ok(())
                }
                Slot::Running { abandoned: true } => Err(JobError::Stuck),
                _ => Err(JobError::Busy),
            }
        }

        /// The compositor takes the queued job to run it (the slot stays
        /// reserved, `Running`).
        #[must_use = "a started job must be finished"]
        pub(super) fn start(&mut self) -> Option<(Job, Ticket)> {
            match core::mem::replace(self, Slot::Free) {
                Slot::Queued(job) => {
                    *self = Slot::Running { abandoned: false };
                    Some((job, Ticket(())))
                }
                other => {
                    *self = other;
                    None
                }
            }
        }

        /// The answer of the job `_ticket` started: for the asker, or dropped
        /// (the slot freed) if the asker gave up meanwhile; whether it was
        /// dropped.
        pub(super) fn finish(&mut self, _ticket: Ticket, answer: Answer) -> bool {
            match self {
                Slot::Running { abandoned: true } => {
                    *self = Slot::Free;
                    true
                }
                Slot::Running { abandoned: false } => {
                    *self = Slot::Answered(answer);
                    false
                }
                // Only a started job has a ticket, and nothing else leaves
                // `Running`.
                Slot::Free | Slot::Queued(_) | Slot::Answered(_) => {
                    debug_assert!(false, "a ticket for no running job");
                    false
                }
            }
        }

        /// Whether an answer waits for its asker.
        pub(super) fn is_answered(&self) -> bool {
            matches!(self, Slot::Answered(_))
        }

        /// The asker takes its answer, freeing the slot (`None`, changing
        /// nothing, before it is answered).
        pub(super) fn take(&mut self) -> Option<Answer> {
            match core::mem::replace(self, Slot::Free) {
                Slot::Answered(answer) => Some(answer),
                other => {
                    *self = other;
                    None
                }
            }
        }

        /// The asker gives up waiting: a queued job is withdrawn, a running
        /// one's answer will be dropped when it comes.
        pub(super) fn abandon(&mut self) {
            match self {
                Slot::Queued(_) | Slot::Answered(_) => *self = Slot::Free,
                Slot::Running { abandoned } => *abandoned = true,
                Slot::Free => {}
            }
        }

        /// A paused pass (nothing presented: no check or capture to make)
        /// answers a queued job `Paused` at once.
        pub(super) fn answer_paused(&mut self) {
            if let Some((_, ticket)) = self.start() {
                let _ = self.finish(ticket, Answer::Paused);
            }
        }
    }

    /// A job's answer.
    pub(super) enum Answer {
        /// Pixels that differ from a full repaint, and from the screen.
        Check {
            repaint: u64,
            screen: u64,
        },
        /// Damage or a commit arrived meanwhile: try again.
        Busy,
        /// The screen was taken back (Alt+F`n`) while the check read it.
        ScreenTaken,
        /// The desktop was paused before the job ran.
        Paused,
        Capture {
            rows: Box<Rows>,
            ns: u64,
        },
        /// The rows aren't the back buffer's size.
        WrongSize,
    }

    /// Why a request got no answer.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum JobError {
        NoDesktop,
        Paused,
        /// Another request holds the slot.
        Busy,
        /// The compositor still runs a request its asker gave up on.
        Stuck,
        NoAnswer,
    }

    impl fmt::Display for JobError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                JobError::NoDesktop => "no desktop",
                JobError::Paused => "paused (`desktop` resumes it)",
                JobError::Busy => "busy with another request (`desktop check` or `screenshot`): try again",
                JobError::Stuck => "the compositor hasn't finished a request given up on (stuck?)",
                JobError::NoAnswer => "no answer from the compositor in 20 s",
            })
        }
    }
}

use slot::{Answer, Job, JobError, Slot};

/// What `desktop status` counts.
#[derive(Clone, Copy, Default)]
struct Stats {
    frames: u64,
    rects: u64,
    pixels: u64,
    present_ns: u64,
    no_focus_keys: u64,
    /// Deliveries for a window that had closed.
    dead_pushes: u64,
}

/// The desktop's state behind [`COMPOSITOR`] (or a test's own lock:
/// [`Desktop::without_screen`]).
struct State {
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
    /// Whether the start completed (windows may open only then).
    started: bool,
    /// Whether the compositor presents (its view of the cut), and the cut's
    /// generation it has acted on.
    shown: bool,
    seen_generation: u64,
    /// The readers' places and gaps, published after each pass.
    pointer_read: Seq,
    keys_read: Seq,
    lost: u64,
    /// Events read but held back (a key ring barrier, or a resume whose
    /// screen hasn't come yet).
    held_back: bool,
    /// The screen for the next resume (`vt::enter_desktop`'s, handed over).
    screen: Option<ScreenCanvas>,
    /// The compositor should end (its start failed after it began).
    abort: bool,
    /// `desktop check`'s and `screenshot`'s requests (see [`Slot`]).
    slot: Slot,
    /// The wallpaper drawn behind the windows, centred; `None`: the theme's
    /// flat colour (no wallpaper, or one that didn't decode).
    wallpaper: Option<Arc<Surface>>,
}

impl State {
    /// A `width` x `height` desktop, not started yet.
    fn new(width: u32, height: u32) -> State {
        // Paused until the compositor acts on the first cut, so the first
        // entry is a resume like every later one: `Wm::resume` with the
        // cut's baseline ignores the buttons held then (pressed in the
        // terminal) until every button is up. (Nothing to deliver: no
        // window yet.)
        let mut wm = Wm::new(width, height, METRICS);
        let _no_windows = wm.interrupt(Interruption::Pause);
        State {
            wm,
            windows: Vec::new(),
            damage: Gathered::new(),
            width,
            height,
            stats: Stats::default(),
            closed: QueueCounts::default(),
            commits: 0,
            committed: 0,
            passing: false,
            started: false,
            shown: false,
            seen_generation: 0,
            pointer_read: Seq::ZERO,
            keys_read: Seq::ZERO,
            lost: 0,
            held_back: false,
            screen: None,
            abort: false,
            slot: Slot::Free,
            wallpaper: None,
        }
    }

    fn window(&self, id: WindowId) -> Option<&Arc<Window>> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Repaint `out`'s damage and put its deliveries on their windows'
    /// queues (the owners to wake and the warnings go into `after`, for
    /// once the locks are released).
    fn apply(&mut self, out: Outcome, after: &mut After) {
        for r in out.damage.rects() {
            self.damage.add(*r);
        }
        for d in &out.deliveries {
            let _ = self.push(d.window(), after, |q| lumen_wm::deliver(q, d));
        }
    }

    /// Change window `id`'s queue with `f`, returning what it did; for a
    /// window no longer on the list, count a dead push instead (`None`).
    fn push(
        &mut self,
        id: WindowId,
        after: &mut After,
        f: impl FnOnce(&mut Queue) -> Option<Pushed>,
    ) -> Option<Pushed> {
        let Some(win) = self.window(id) else {
            self.stats.dead_pushes += 1;
            return None;
        };
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
        pushed
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

/// A desktop: the live one ([`Desktop::LIVE`], [`open_window`]) or one
/// with no compositor thread and no screen, for the ktests, so none leaves
/// a desktop running.
pub struct Desktop {
    home: Home,
}

#[cfg_attr(
    not(feature = "ktest"),
    allow(dead_code, reason = "the ktests' desktop; windows' owners come with #155-#157")
)]
impl Desktop {
    /// The live desktop.
    const LIVE: Desktop = Desktop { home: Home::Live };

    /// A 1280x800 desktop with no compositor and no screen, started:
    /// windows open and close, nothing is drawn.
    pub fn without_screen() -> Desktop {
        let mut state = State::new(1280, 800);
        state.started = true;
        Desktop { home: Home::Own(Arc::new(SpinLock::new(Some(state)))) }
    }

    /// Decode `png` as this desktop's wallpaper, as a start does: whether
    /// it is shown (if not, the theme's flat colour, and one warning). The
    /// whole screen is damaged either way.
    pub fn set_wallpaper(&self, png: &[u8]) -> bool {
        let Some((w, h)) = self.home.lock().as_ref().map(|s| (s.width, s.height)) else { return false };
        let wallpaper = load_wallpaper(png, w, h);
        let shown = wallpaper.is_some();
        if let Some(s) = self.home.lock().as_mut() {
            s.wallpaper = wallpaper;
            s.damage.add(Rect::new(0, 0, s.width, s.height));
        }
        self.home.wake();
        shown
    }

    /// Open a window of `width` x `height` client pixels titled `title`,
    /// in front and focused, its frame's corner placed by how many are
    /// open. `NoDesktop` until the desktop's start completed.
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
            let s = g.as_ref().filter(|s| s.started).ok_or(OpenError::NoDesktop)?;
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
        committed.fill(theme().client);
        let owner = crate::sched::running_here().map_or("boot", |(_, name)| name);
        let mut after = After::default();
        let win = {
            let mut g = self.home.lock();
            let s = g.as_mut().filter(|s| s.started).ok_or(OpenError::NoDesktop)?;
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
                // Read under the desktop's lock, which every switch holds:
                // a switch is either before (its theme drawn from the
                // start) or after (told).
                theme_seen: AtomicU64::new(THEME_GEN.load(Ordering::Acquire)),
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

/// Open a window on the live desktop (see [`Desktop::open_window`]).
#[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "its first callers are #155-#157's windows"))]
pub fn open_window(title: &str, width: u32, height: u32) -> Result<WindowHandle, OpenError> {
    Desktop::LIVE.open_window(title, width, height)
}

/// The bands of at most [`BAND`] rows `r` is copied in, top to bottom (one
/// hold of a window's lock each).
fn bands(r: Rect) -> impl Iterator<Item = Rect> {
    (r.y()..r.bottom())
        .step_by(BAND as usize)
        .map(move |y| Rect::new(r.x(), y, r.w(), BAND.min((r.bottom() - y) as u32)))
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
        for band in rects.iter().filter_map(|r| r.intersect(&bounds)).flat_map(bands) {
            self.win.inner.lock().committed.blit(&self.surface, band, band.x(), band.y());
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

    /// The next event, if one is waiting (never blocks): a theme switch
    /// first, then the queue's.
    pub fn next_event(&mut self) -> Option<WindowEvent> {
        let switches = THEME_GEN.load(Ordering::Acquire);
        if self.win.theme_seen.swap(switches, Ordering::AcqRel) != switches {
            return Some(WindowEvent::Theme(theme()));
        }
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
            win.events.wait_until(|| win.queued.load(Ordering::Acquire) || win.theme_behind());
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
        // In one hold of the desktop's lock: off the list (so nothing more
        // is delivered to its queue), then its queue killed and its counts
        // kept, then out of the window manager.
        let mut after = After::default();
        let mut g = self.home.lock();
        if let Some(s) = g.as_mut() {
            s.windows.retain(|w| !Arc::ptr_eq(w, &self.win));
            let counts = {
                let mut inner = self.win.inner.lock();
                inner.queue.kill();
                self.win.queued.store(false, Ordering::Release);
                inner.queue.counts()
            };
            s.closed += &counts;
            if let Some(out) = s.wm.close(self.win.id) {
                s.apply(out, &mut after);
            }
        } else {
            self.win.inner.lock().queue.kill();
        }
        drop(g);
        after.finish();
        self.home.wake();
    }
}

// -- drawing -----------------------------------------------------------------

/// Draw `text` in the UI font at `px`, its line box's top-left corner at
/// (`x`, `y`), in `color`, touching only pixels inside `clip` (blending
/// isn't idempotent: a redraw of one damaged rectangle passes it, so each
/// pixel is drawn once per recompose); returns the pen's position after
/// it. Glyphs come from `font`'s cache, rasterized only the first time.
#[allow(clippy::too_many_arguments, reason = "a line of text: where, what, how big, which colour, inside what")]
fn draw_text(s: &mut Surface, font: &mut Font<'_>, x: i32, y: i32, text: &str, px: Px, color: u32, clip: Rect) -> i32 {
    font.render(text, px, |g| match Mask::new(g.width(), g.height(), g.coverage()) {
        Some(mask) => s.blend_mask_in(&mask, x + g.x(), y + g.y(), color, clip),
        None => {
            // `lumen-font` promises a mask of its size: a bug, not drawn.
            if !BAD_MASK_SAID.swap(true, Ordering::Relaxed) {
                log::warn!(target: "desktop", "a glyph's mask doesn't match its size: not drawn");
            }
        }
    });
    x.saturating_add(i32::try_from(font.face().measure(text, px).width).unwrap_or(i32::MAX))
}

/// A line's height at `px` (ascent and descent).
fn line_height(font: &Font<'_>, px: Px) -> i32 {
    let m = font.face().line_metrics(px);
    m.ascent + m.descent
}

/// The close box's cross: a 16x16 coverage mask, two diagonals two pixels
/// wide, 4 in from each edge.
const CROSS: [u8; 256] = {
    let mut m = [0u8; 256];
    let mut i = 4;
    while i < 12 {
        m[i * 16 + i] = 255;
        m[i * 16 + i + 1] = 255;
        m[i * 16 + 15 - i] = 255;
        m[i * 16 + 14 - i] = 255;
        i += 1;
    }
    m
};

/// One window in a snapshot of the stacking order.
struct Snap {
    win: Arc<Window>,
    frame: Rect,
    focused: bool,
}

/// What a pass draws: the windows back to front, the theme and the
/// wallpaper, all taken in one hold of the desktop's lock.
struct Scene {
    windows: Vec<Snap>,
    theme: &'static Theme,
    wallpaper: Option<Arc<Surface>>,
}

/// Where a `wallpaper` goes on a `target`: centred (cut where it is bigger).
fn wallpaper_place(wallpaper: &Surface, target: &Surface) -> Rect {
    let x = (i64::from(target.width()) - i64::from(wallpaper.width())) / 2;
    let y = (i64::from(target.height()) - i64::from(wallpaper.height())) / 2;
    // Both at most a screen's size: far inside `i32`.
    Rect::new(x as i32, y as i32, wallpaper.width(), wallpaper.height())
}

/// Recompose `r` (screen coordinates) of the scene into `target`: the
/// wallpaper (the theme's colour where there is none), then every window
/// overlapping it from back to front, its frame then its committed surface
/// (blitted [`BAND`] rows per hold of the window's lock). Everything drawn
/// stays inside `r`, so damage-only recomposition equals a full one.
fn compose(target: &mut Surface, r: Rect, scene: &Scene, font: &mut Option<Font<'static>>) {
    let Some(r) = r.clip(target.width(), target.height()) else { return };
    match scene.wallpaper.as_deref() {
        Some(wp) => {
            let at = wallpaper_place(wp, target);
            match at.intersect(&r) {
                Some(w) => {
                    for piece in &r.subtract(&w) {
                        target.fill_rect(*piece, scene.theme.desktop);
                    }
                    target.blit(wp, w.translate(-at.x(), -at.y()), w.x(), w.y());
                }
                None => target.fill_rect(r, scene.theme.desktop),
            }
        }
        None => target.fill_rect(r, scene.theme.desktop),
    }
    for snap in &scene.windows {
        let Some(f) = snap.frame.intersect(&r) else { continue };
        paint_frame(target, snap, f, scene.theme, font.as_mut());
        let client = METRICS.client(&snap.frame);
        let Some(c) = client.intersect(&r) else { continue };
        for band in bands(c) {
            let src = band.translate(-client.x(), -client.y());
            target.blit(&snap.win.inner.lock().committed, src, band.x(), band.y());
        }
    }
}

/// The part `clip` (inside the frame) of a window's frame: the border, the
/// title bar with its title (none without a font), the close box with its
/// cross.
fn paint_frame(target: &mut Surface, snap: &Snap, clip: Rect, theme: &Theme, font: Option<&mut Font<'static>>) {
    let (border, title_bg, title_fg) = if snap.focused {
        (theme.border_focused, theme.title_focused, theme.title_focused_text)
    } else {
        (theme.border, theme.title, theme.title_text)
    };
    let frame = snap.frame;
    target.fill_rect(clip, border);
    let bar = METRICS.title_bar(&frame);
    if let Some(b) = bar.intersect(&clip) {
        target.fill_rect(b, title_bg);
    }
    let close = METRICS.close_box(&frame);
    if let Some(c) = close.intersect(&clip) {
        target.fill_rect(c, theme.close);
        if let Some(mask) = Mask::new(16, 16, &CROSS) {
            target.blend_mask_in(&mask, close.x(), close.y(), theme.close_glyph, c);
        }
    }
    // The title, left of the close box, never over it.
    let text_right = close.x().saturating_sub(4).max(bar.x());
    let area = Rect::new(bar.x(), bar.y(), (text_right - bar.x()).max(0) as u32, bar.h());
    if let (Some(a), Some(font)) = (area.intersect(&clip), font) {
        let y = bar.y() + (METRICS.title_h as i32 - line_height(font, TITLE_PX)) / 2;
        draw_text(target, font, bar.x() + 8, y, &snap.win.title, TITLE_PX, title_fg, a);
    }
}

// -- the compositor ------------------------------------------------------------

type PointerRing = LockedRing<PointerEvent, POINTER_EVENTS>;
type KeyRing = LockedRing<KeyEvent, KEY_EVENTS>;

/// The compositor thread's own state: touched by it alone, so no lock.
struct Compositor {
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
    /// For the scenarios: a wait between reading the rings and reading the
    /// cut (`lumen.desktop_cut_delay_ms=`), widening the window a pause
    /// can be written in.
    cut_delay_ns: u64,
    /// The titles' glyph cache (the compositor's own).
    font: Option<Font<'static>>,
    /// Whether the first presenting pass was timed (said at INFO once).
    timed: bool,
}

/// What a presenting pass did, for `desktop status`.
struct Presented {
    rects: u64,
    pixels: u64,
    ns: u64,
    /// Whether anything reached the screen (a rectangle or the pointer).
    frame: bool,
}

/// The most `lumen.desktop_cut_delay_ms=` waits: under the 5 s a resume
/// waits for the compositor.
const CUT_DELAY_MAX_MS: u64 = 2000;

/// `lumen.desktop_cut_delay_ms=<ms>`, for the scenarios (0 without it): a
/// wait between reading the rings and reading the cut, said at INFO; a
/// value that doesn't parse is warned about and ignored, one past
/// [`CUT_DELAY_MAX_MS`] capped.
fn cut_delay_ms() -> u64 {
    const KEY: &str = "lumen.desktop_cut_delay_ms";
    let Some(v) = crate::cmdline::value(KEY) else { return 0 };
    let Ok(ms) = v.parse::<u64>() else {
        log::warn!(target: "desktop", "{KEY}={v}: not a number of milliseconds, ignored");
        return 0;
    };
    let ms = ms.min(CUT_DELAY_MAX_MS);
    if ms > 0 {
        log::info!(target: "desktop", "waiting {ms} ms between reading input and the cut ({KEY})");
    }
    ms
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
        let delay_ms = cut_delay_ms();
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
            cut_delay_ns: delay_ms.saturating_mul(1_000_000),
            font: text_font(),
            timed: false,
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

    /// Both rings' new events into the queues.
    fn drain(&mut self) {
        while let Some(r) = self.pointers.read() {
            self.pointer = Some(position(&r.event));
            self.pointer_queue.push_back(r);
        }
        while let Some(r) = self.keys.read() {
            self.key_queue.push_back(r);
        }
    }

    /// One pass (see the module doc).
    fn pass(&mut self) {
        self.drain();
        if self.cut_delay_ns > 0 {
            let _ = crate::sched::sleep(self.cut_delay_ns);
        }
        // After reading the rings: a pause written after this read has
        // marks past every event read, so those were the desktop's (one
        // written between the read and here: see `input`'s pause).
        let cut = vt::cut();
        let mut after = After::default();
        let (damage, scene) = {
            let mut g = COMPOSITOR.lock();
            let Some(s) = g.as_mut() else { return };
            let full = self.input(s, &cut, &mut after);
            s.seen_generation = self.generation;
            s.shown = self.shown;
            s.held_back = !self.key_queue.is_empty() || !self.pointer_queue.is_empty();
            if !self.shown {
                // Not shown: nothing is presented, and the damage gathers
                // until the resume, which presents everything. Really
                // paused (not a resume waiting for its screen), a request
                // waiting can't be answered from the screen.
                if cut.is_paused() {
                    s.slot.answer_paused();
                }
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
        let composing = crate::time::now_ns();
        for r in damage.rects() {
            compose(&mut self.back, *r, &scene, &mut self.font);
        }
        let composed_ns = crate::time::now_ns().saturating_sub(composing);
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
        let ns = crate::time::now_ns().saturating_sub(start);
        let pointer_moved = self.place_pointer(damage.rects());
        let frame = !damage.is_empty() || pointer_moved;
        if !self.timed && pixels > 0 {
            // The first present is the whole screen: how long the wallpaper,
            // the frames and the titles (rasterized now, cached after) took
            // to compose, and the copy to the screen.
            self.timed = true;
            let (c, p) = (lumen_time::Millis(composed_ns), lumen_time::Millis(ns));
            log::info!(target: "desktop", "first frame: composed in {c}, presented in {p}");
        }
        self.publish(Some(Presented { rects: damage.rects().len() as u64, pixels, ns, frame }));
        self.run_queued_job();
    }

    /// Act on the input read and the cut (under [`COMPOSITOR`]): whether the
    /// whole screen must be presented (a resume).
    fn input(&mut self, s: &mut State, cut: &vt::Cut, after: &mut After) -> bool {
        let mut full = false;
        while self.generation < cut.generation() {
            if self.shown {
                // A pause: the events up to it were the desktop's (keys
                // before the Alt+F`n` press), the rest the terminal's. Read
                // the rings again first: a screen shell's decoding pass may
                // have pushed events and written the pause after `pass`
                // read them, and every event up to the marks is in the
                // rings now.
                self.drain();
                self.feed_pointer(s, after, |seq| cut.pause_owns_pointer(seq));
                self.feed_keys(s, after, |seq| cut.pause_owns_key(seq));
                let out = s.wm.interrupt(Interruption::Pause);
                s.apply(out, after);
                // (Events past the marks were the terminal's: the
                // trailing branch below drops them unless a resume in this
                // same look keeps those after its own marks.)
                self.shown = false;
                // `leave_desktop` took the screen back and redrew it.
                self.screen = None;
                self.drawn = None;
            } else {
                // A resume (or the start): its screen comes from `desktop`
                // after the cut; until then, wait.
                let Some(screen) = s.screen.take() else { return full };
                // (`skip_to` is redundant with the `retain`s here and the
                // queues cleared while paused; it keeps the reader's place
                // where the cut is, not wherever the last read left it.)
                let marks = cut.resume();
                self.pointers.skip_to(marks.pointer);
                self.keys.skip_to(marks.keys);
                self.pointer_queue.retain(|r| cut.after_resume_pointer(r.seq));
                self.key_queue.retain(|r| cut.after_resume_key(r.seq));
                let out = s.wm.resume(cut.baseline());
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
    fn feed_pointer(&mut self, s: &mut State, after: &mut After, upto: impl Fn(Seq) -> bool) {
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
    /// Alt+F`n` press: the router writes the cut naming it in the same pass
    /// that pushed it, so a later pass reads that cut and acts on the
    /// pause.
    fn feed_keys(&mut self, s: &mut State, after: &mut After, upto: impl Fn(Seq) -> bool) {
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
                    if s.push(id, after, |q| Some(q.push(Entry::Key(r.event)))) == Some(Pushed::Queued)
                        && let Some(w) = s.window(id)
                    {
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
        // A tip on the screen (the arrow drawn from it, clipped at the
        // right and bottom edges).
        if let Some((x, y)) = want.filter(|&(x, y)| self.back.bounds().contains(x, y))
            && let Some(b) = arrow_box((x, y)).clip(self.back.width(), self.back.height())
        {
            let (w, h) = (b.w() as usize, b.h() as usize);
            let mut px = [0u32; SPRITE_W * SPRITE_H];
            for row in 0..h {
                let src = &self.back.row(y as u32 + row as u32)[x as usize..x as usize + w];
                px[row * SPRITE_W..row * SPRITE_W + w].copy_from_slice(src);
            }
            let theme = theme();
            draw_arrow(&mut px, SPRITE_W, (w, h), (0, 0), theme.pointer_outline, theme.pointer_fill);
            for row in 0..h {
                screen.write_span(x as u32, y as u32 + row as u32, &px[row * SPRITE_W..row * SPRITE_W + w]);
            }
            self.drawn = Some((x, y));
        }
        true
    }

    /// The pass is over: its statistics and the readers' places.
    fn publish(&self, presented: Option<Presented>) {
        let mut g = COMPOSITOR.lock();
        let Some(s) = g.as_mut() else { return };
        s.passing = false;
        s.pointer_read = self.pointers.after();
        s.keys_read = self.keys.after();
        s.lost = self.pointers.lost() + self.keys.lost();
        if let Some(p) = presented {
            s.stats.rects += p.rects;
            s.stats.pixels += p.pixels;
            s.stats.present_ns += p.ns;
            s.stats.frames += u64::from(p.frame);
        }
    }

    /// A request accepted meanwhile (`desktop check`, `screenshot`), run
    /// right after a presenting pass.
    fn run_queued_job(&mut self) {
        let (job, ticket, scene, committed) = {
            let mut g = COMPOSITOR.lock();
            let Some(s) = g.as_mut() else { return };
            let Some((job, ticket)) = s.slot.start() else { return };
            (job, ticket, snapshot(s), s.committed)
        };
        let answer = match job {
            Job::Check(scratch) => self.check(scratch, &scene, committed),
            Job::Capture(rows) => self.capture(rows),
        };
        let dropped = COMPOSITOR.lock().as_mut().is_some_and(|s| s.slot.finish(ticket, answer));
        if dropped {
            log::info!(target: "desktop", "a request's answer came after its asker gave up: dropped");
        }
    }

    /// `desktop check`: the scene recomposed whole into `scratch` against
    /// the back buffer, and the screen read back against it. Damage or a
    /// commit since the pass (`committed` its count then) makes that a
    /// moving target: `Busy`.
    fn check(&mut self, mut scratch: Box<Surface>, scene: &Scene, committed: u64) -> Answer {
        let moving = |s: &State| !s.damage.is_empty() || s.commits > 0 || s.committed != committed;
        if COMPOSITOR.lock().as_ref().is_none_or(moving) {
            return Answer::Busy;
        }
        let all = scratch.bounds();
        compose(&mut scratch, all, scene, &mut self.font);
        let repaint = (0..self.back.height())
            .map(|y| {
                let differ = self.back.row(y).iter().zip(scratch.row(y)).filter(|(a, b)| (**a ^ **b) & 0xff_ffff != 0);
                differ.count() as u64
            })
            .sum();
        let Some(screen) = self.screen_differences() else { return Answer::ScreenTaken };
        if COMPOSITOR.lock().as_ref().is_none_or(moving) {
            return Answer::Busy;
        }
        Answer::Check { repaint, screen }
    }

    /// `screenshot`: the back buffer, as the pass left it, into `rows`.
    fn capture(&self, mut rows: Box<Rows>) -> Answer {
        if (rows.width(), rows.height()) != (self.back.width(), self.back.height()) {
            return Answer::WrongSize;
        }
        let start = crate::time::now_ns();
        for y in 0..rows.height() {
            for (out, &p) in rows.row_mut(y).iter_mut().zip(self.back.row(y)) {
                *out = [(p >> 16) as u8, (p >> 8) as u8, p as u8];
            }
        }
        Answer::Capture { rows, ns: crate::time::now_ns().saturating_sub(start) }
    }

    /// Pixels on the screen that differ from the back buffer, outside the
    /// pointer's box; `None` if the screen was taken back meanwhile.
    fn screen_differences(&self) -> Option<u64> {
        let screen = self.screen.as_ref()?;
        let arrow = self.drawn.map(arrow_box);
        let mut row = alloc::vec![0u32; self.back.width() as usize];
        let mut differ = 0;
        for y in 0..self.back.height() {
            if !screen.read_span(0, y as usize, &mut row) {
                return None;
            }
            for (x, (a, b)) in self.back.row(y).iter().zip(&row).enumerate() {
                if (a ^ b) & 0xff_ffff != 0 && !arrow.is_some_and(|r| r.contains(x as i32, y as i32)) {
                    differ += 1;
                }
            }
        }
        Some(differ)
    }
}

/// The scene as a pass draws it: the stacking order back to front, the
/// theme (switched only under the desktop's lock, so it goes with the
/// damage taken in the same hold) and the wallpaper.
fn snapshot(s: &State) -> Scene {
    let focused = s.wm.focused();
    let windows =
        s.wm.windows()
            .iter()
            .filter_map(|w| {
                let win = s.window(w.id())?.clone();
                Some(Snap { win, frame: w.frame(), focused: focused == Some(w.id()) })
            })
            .collect();
    Scene { windows, theme: theme(), wallpaper: s.wallpaper.clone() }
}

// -- starting, resuming, the requests --------------------------------------------

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
    /// Another `desktop` is starting or resuming it.
    Busy,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::NoScreen => f.write_str("no screen to draw on"),
            StartError::NoMemory => f.write_str("no memory for the back buffer"),
            StartError::NoThread(e) => write!(f, "no thread for the compositor: {e}"),
            StartError::Screen(e) => write!(f, "{e}"),
            StartError::Behind => f.write_str("the compositor hasn't caught up with the last pause"),
            StartError::Busy => f.write_str("another `desktop` is starting or resuming it"),
        }
    }
}

/// A start or resume refused: why, and which it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The desktop had started (this was a resume).
    pub resuming: bool,
    pub why: StartError,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.resuming {
            write!(f, "not resumed: {} (try again)", self.why)
        } else {
            write!(f, "not started: {}", self.why)
        }
    }
}

/// What `start` did: the desktop is shown, since now (started), again
/// (resumed), or already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    Now,
    Resumed,
    Already,
}

/// Set while a start or resume runs (two shells typing `desktop` at once:
/// the second is refused).
static START_BUSY: AtomicBool = AtomicBool::new(false);

/// Start the desktop, or resume it (see the module doc).
pub fn start() -> Result<Shown, Refused> {
    let started = || COMPOSITOR.lock().as_ref().is_some_and(|s| s.started);
    // Win the flag first, then look: a look before it could be stale by the
    // time the other `desktop` finished, and pick a first start over a live
    // desktop.
    if START_BUSY.swap(true, Ordering::AcqRel) {
        return Err(Refused { resuming: started(), why: StartError::Busy });
    }
    let started = started();
    let result = if started { resume() } else { first_start() };
    START_BUSY.store(false, Ordering::Release);
    result.map_err(|why| Refused { resuming: started, why })
}

/// `png` as a surface to draw behind the windows: refused (`Err`, why)
/// unless it decodes and fits a `w` x `h` screen.
fn decode_wallpaper(png: &[u8], w: u32, h: u32) -> Result<Surface, String> {
    let image = lumen_png::decode::decode(png, w, h).map_err(|e| alloc::format!("{e}"))?;
    let mut s = Surface::with_stride(image.width(), image.height(), image.width() as usize)
        .ok_or_else(|| String::from("no memory for it"))?;
    for y in 0..image.height() {
        s.row_mut(y).copy_from_slice(image.row(y));
    }
    Ok(s)
}

/// The wallpaper for a `w` x `h` desktop from `png`, or `None` (the
/// theme's flat colour) with one warning saying why: `WARN desktop:
/// wallpaper not shown: <reason>`.
fn load_wallpaper(png: &[u8], w: u32, h: u32) -> Option<Arc<Surface>> {
    let start = crate::time::now_ns();
    match decode_wallpaper(png, w, h) {
        Ok(s) => {
            let took = lumen_time::Millis(crate::time::now_ns().saturating_sub(start));
            log::info!(target: "desktop", "wallpaper: {}x{}, {} bytes of PNG decoded in {took}", s.width(), s.height(), png.len());
            Some(Arc::new(s))
        }
        Err(why) => {
            log::warn!(target: "desktop", "wallpaper not shown: {why}");
            None
        }
    }
}

/// The first `desktop`.
/// Never over a desktop already there (a state, started or starting):
/// that is refused as busy, the live one untouched.
fn first_start() -> Result<Shown, StartError> {
    if COMPOSITOR.lock().is_some() {
        return Err(StartError::Busy);
    }
    let (w, h) = term::screen_size().ok_or(StartError::NoScreen)?;
    let (w, h) = (w as u32, h as u32);
    // Decoded before the back buffer, with no lock held (a warning, and the
    // theme's flat colour, if it doesn't decode).
    let wallpaper = load_wallpaper(WALLPAPER_PNG, w, h);
    let back = Surface::with_stride(w, h, w as usize).ok_or(StartError::NoMemory)?;
    let owner = Owner::new();
    {
        let mut g = COMPOSITOR.lock();
        if g.is_some() {
            return Err(StartError::Busy);
        }
        let mut state = State::new(w, h);
        state.wallpaper = wallpaper;
        *g = Some(state);
    }
    let undo = || *COMPOSITOR.lock() = None;
    let compositor = Compositor::new(back);
    let thread = match crate::sched::spawn("compositor", move || compositor.run()) {
        Ok(t) => t,
        Err(e) => {
            undo();
            return Err(StartError::NoThread(e));
        }
    };
    RUNNING.store(true, Ordering::Release);
    match vt::enter_desktop(owner) {
        Ok(screen) => {
            if let Some(s) = COMPOSITOR.lock().as_mut() {
                s.screen = Some(screen);
                s.started = true;
            }
            drop(thread);
            wake();
            open_demo_windows();
            log::info!(target: "desktop", "started on {w}x{h}");
            Ok(Shown::Now)
        }
        Err(e) => {
            if let Some(s) = COMPOSITOR.lock().as_mut() {
                s.abort = true;
            }
            wake();
            thread.join();
            RUNNING.store(false, Ordering::Release);
            undo();
            Err(StartError::Screen(e))
        }
    }
}

/// `desktop` while started: resume it if paused.
fn resume() -> Result<Shown, StartError> {
    if vt::desktop_shown() {
        return Ok(Shown::Already);
    }
    // The compositor acts on the pause before this resume writes the next
    // cut, so the cut never needs more than its latest marks.
    let deadline = crate::time::now_ns().saturating_add(5_000_000_000);
    let caught_up = || {
        let generation = vt::cut().generation();
        COMPOSITOR.lock().as_ref().is_some_and(|s| s.seen_generation == generation)
    };
    wake();
    if !DONE.wait_until_or_deadline(deadline, caught_up) && !caught_up() {
        log::warn!(target: "desktop", "the compositor hasn't acted on the last pause in 5 s: is it stuck?");
        return Err(StartError::Behind);
    }
    let owner = vt::cut().owner().ok_or(StartError::NoScreen)?;
    let screen = vt::enter_desktop(owner).map_err(StartError::Screen)?;
    if let Some(s) = COMPOSITOR.lock().as_mut() {
        s.screen = Some(screen);
    }
    wake();
    Ok(Shown::Resumed)
}

/// The theme `lumen.theme=` names, at boot (before the desktop starts):
/// a name that isn't a theme is warned about and ignored.
fn theme_at_boot() {
    const KEY: &str = "lumen.theme";
    let Some(name) = crate::cmdline::value(KEY) else { return };
    match Theme::by_name(name) {
        Some(t) => {
            let _ = switch_theme(&Home::Live, t);
            log::info!(target: "desktop", "theme {} ({KEY})", t.name);
        }
        None => log::warn!(target: "desktop", "{KEY}={name}: not a theme (light, dark), ignored"),
    }
}

/// Switch to `t` (`theme`, `lumen.theme=`): under `home`'s lock, the theme
/// and its count change and the whole screen is damaged (one full
/// repaint); after it, every window's owner is woken to be told
/// ([`WindowEvent::Theme`]), and the compositor. Whether it changed.
fn switch_theme(home: &Home, t: &'static Theme) -> bool {
    let Some(index) = Theme::ALL.iter().position(|a| a.name == t.name) else { return false };
    let windows = {
        let mut g = home.lock();
        if THEME.load(Ordering::Acquire) == index {
            return false;
        }
        THEME.store(index, Ordering::Release);
        THEME_GEN.fetch_add(1, Ordering::AcqRel);
        match g.as_mut() {
            Some(s) => {
                s.damage.add(Rect::new(0, 0, s.width, s.height));
                s.windows.clone()
            }
            None => Vec::new(),
        }
    };
    for w in &windows {
        w.events.wake_all();
    }
    home.wake();
    true
}

/// The shell's `theme`: the desktop's theme, or switch it (before the
/// desktop starts too: it starts in that theme).
pub fn theme_command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    match args {
        [] => {
            let _ = writeln!(ctx.out, "theme: {}", theme().name);
        }
        [name] => match Theme::by_name(name) {
            Some(t) => {
                let note = if switch_theme(&Home::Live, t) { "" } else { " (already)" };
                let _ = writeln!(ctx.out, "theme: {}{note}", t.name);
            }
            None => ctx.fail(format_args!("theme: no theme {name:?} (light, dark)")),
        },
        _ => ctx.fail(format_args!("{THEME_USAGE}")),
    }
}

pub const THEME_USAGE: &str = "usage: theme [light|dark]";

/// At boot: the theme `lumen.theme=` names, then the desktop for the
/// `lumen.desktop` boot flag (a warning if it can't start, and the boot
/// goes on in text mode).
pub fn start_at_boot() {
    theme_at_boot();
    if !crate::cmdline::has_flag("lumen.desktop") {
        return;
    }
    if let Err(e) = start() {
        log::warn!(target: "desktop", "{e}");
    }
}

/// Whether the desktop is shown (its screen live).
pub fn is_shown() -> bool {
    vt::desktop_shown()
}

/// Accept `job` on `home`'s desktop: refused without one, while the live
/// one is paused, or while another request holds the slot.
fn submit(home: &Home, job: Job) -> Result<(), JobError> {
    let mut g = home.lock();
    let s = g.as_mut().ok_or(JobError::NoDesktop)?;
    if matches!(home, Home::Live) && !is_shown() {
        return Err(JobError::Paused);
    }
    s.slot.accept(job)
}

/// Wait (up to 20 s) for the answer to the request this asker holds the
/// slot with; giving up frees the slot (a late answer is dropped) and
/// warns.
fn await_answer(home: &Home) -> Result<Answer, JobError> {
    let deadline = crate::time::now_ns().saturating_add(20_000_000_000);
    let answered = || home.lock().as_ref().is_none_or(|s| s.slot.is_answered());
    let _ = DONE.wait_until_or_deadline(deadline, answered);
    let taken = {
        let mut g = home.lock();
        let s = g.as_mut().ok_or(JobError::NoDesktop)?;
        let answer = s.slot.take();
        if answer.is_none() {
            s.slot.abandon();
        }
        answer
    };
    match taken {
        Some(Answer::Paused) => Err(JobError::Paused),
        Some(answer) => Ok(answer),
        None => {
            log::warn!(target: "desktop", "no answer from the compositor in 20 s");
            Err(JobError::NoAnswer)
        }
    }
}

/// Hand `job` to the live compositor and wait for its answer. One request
/// at a time: the slot stays reserved until this asker takes its answer or
/// gives up, and another request meanwhile is refused (`JobError::Busy`).
fn run_job(job: Job) -> Result<Answer, JobError> {
    submit(&Home::Live, job)?;
    wake();
    await_answer(&Home::Live)
}

/// The back buffer as RGB rows (`screenshot` while the desktop is shown),
/// with how long the copy took: `None` if it isn't shown (or was paused
/// before the copy), so the text screen is the source.
pub fn capture(rows: Rows) -> Option<Result<(Rows, u64), String>> {
    if !is_shown() {
        return None;
    }
    capture_answer(run_job(Job::Capture(Box::new(rows))))
}

/// What a capture request's outcome means for `screenshot`.
fn capture_answer(outcome: Result<Answer, JobError>) -> Option<Result<(Rows, u64), String>> {
    match outcome {
        Ok(Answer::Capture { rows, ns }) => Some(Ok((*rows, ns))),
        Err(JobError::Paused | JobError::NoDesktop) => None,
        Ok(Answer::WrongSize) => Some(Err(String::from("the rows aren't the back buffer's size"))),
        Ok(_) => Some(Err(String::from("the compositor answered something else"))),
        Err(e) => Some(Err(alloc::format!("{e}"))),
    }
}

// -- the demo windows and `desktop spin` --------------------------------------------

/// The two windows the desktop starts with.
#[derive(Clone, Copy)]
enum Demo {
    Welcome,
    Colours,
}

/// The Welcome window's heading (at [`HEADING_PX`]) and lines (at
/// [`TEXT_PX`]): text at two sizes, which `test_text_rendering_golden`
/// compares.
const WELCOME_HEADING: &str = "Welcome to Lumen";
const WELCOME_LINES: [&str; 4] = [
    "Drag a title bar to move a window;",
    "click a window to bring it to the front.",
    "Alt+F1 shows the terminals; `desktop` comes back.",
    "`theme dark` and `theme light` switch the colours.",
];

/// Open the demo windows, each with its owner thread (a warning for one
/// that can't open).
fn open_demo_windows() {
    let demos = [
        (Demo::Welcome, "Welcome", 440, 210, (96, 96), "desktop-welcome"),
        (Demo::Colours, "Colours", 400, 210, (440, 300), "desktop-colours"),
    ];
    for (demo, title, w, h, at, thread) in demos {
        let handle = match Desktop::LIVE.open(title, w, h, Some(at), false) {
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
/// until its close box is clicked; a theme switch redraws it all.
fn demo_owner(mut h: WindowHandle, demo: Demo) {
    let mut font = text_font();
    let mut last: Option<(lumen_input::KeyCode, Option<char>)> = None;
    draw_demo(&mut h, demo, font.as_mut(), last);
    loop {
        match h.wait_event() {
            WindowEvent::Close => return,
            WindowEvent::Theme(_) => draw_demo(&mut h, demo, font.as_mut(), last),
            WindowEvent::Key(KeyEvent { code, action: KeyAction::Press { text }, .. }) => {
                last = Some((code, text));
                let line = draw_last_key(&mut h, font.as_mut(), last);
                h.commit(&[line]);
            }
            _ => {}
        }
    }
}

/// A demo window drawn whole, in the theme's colours, and committed.
fn draw_demo(
    h: &mut WindowHandle,
    demo: Demo,
    mut font: Option<&mut Font<'static>>,
    last: Option<(lumen_input::KeyCode, Option<char>)>,
) {
    let theme = theme();
    let bounds = h.surface_mut().bounds();
    {
        let s = h.surface_mut();
        s.fill(theme.client);
        match demo {
            Demo::Welcome => {
                if let Some(font) = font.as_deref_mut() {
                    draw_text(s, font, 16, 12, WELCOME_HEADING, HEADING_PX, theme.client_text, bounds);
                    for (i, line) in WELCOME_LINES.iter().enumerate() {
                        draw_text(s, font, 16, 60 + 26 * i as i32, line, TEXT_PX, theme.client_text, bounds);
                    }
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
    draw_last_key(h, font, last);
    h.commit(&[bounds]);
}

/// The "last key: ..." line of a demo window; the rectangle drawn.
fn draw_last_key(
    h: &mut WindowHandle,
    font: Option<&mut Font<'static>>,
    key: Option<(lumen_input::KeyCode, Option<char>)>,
) -> Rect {
    let theme = theme();
    let s = h.surface_mut();
    let line = Rect::new(0, LAST_KEY_Y, s.width(), 24);
    s.fill_rect(line, theme.client);
    let mut text = String::from("last key: ");
    match key {
        None => text.push('-'),
        Some((_, Some(c))) if !c.is_control() => text.push(c),
        Some((code, _)) => {
            use core::fmt::Write;
            let _ = write!(text, "{code:?}");
        }
    }
    if let Some(font) = font {
        draw_text(s, font, 16, LAST_KEY_Y + 2, &text, TEXT_PX, theme.client_text, line);
    }
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
        s.fill(theme().client);
        s.fill_rect(Rect::new(x, 20, 40, 20), GREEN);
        h.commit(&[bounds]);
        x = (x + 8) % (bounds.w() as i32 - 40).max(1);
        if !h.held() {
            while let Some(e) = h.next_event() {
                if matches!(e, WindowEvent::Close) {
                    return;
                }
            }
        }
        let _ = crate::sched::sleep(20_000_000);
    }
}

/// `desktop spin`: open its window and start its owner.
fn spin() -> Result<u32, String> {
    let h = Desktop::LIVE.open("spin", 200, 60, None, true).map_err(|e| alloc::format!("no window: {e}"))?;
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

/// `desktop status`: one record. The keyboard's count, the rings' newest
/// and the cut are read just before the desktop's lock, the rest under it:
/// a snapshot of the desktop, not of all of them at one instant.
pub fn status_table() -> pipe::Table {
    const COLUMNS: [Column; 25] = [
        Column::new("state", Kind::Text, Align::Left, 5),
        Column::new("theme", Kind::Text, Align::Left, 5),
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
    let all_decoded = crate::keyboard::all_decoded();
    let (pointer_newest, keys_newest) = (POINTER.newest(), KEYS.newest());
    let generation = vt::cut().generation();
    let pending = PENDING.load(Ordering::Acquire);
    let g = COMPOSITOR.lock();
    let Some(s) = g.as_ref() else {
        let mut row = alloc::vec![text("off"), text(theme().name)];
        row.extend((2..COLUMNS.len() - 1).map(|_| Value::Absent("-")));
        row.push(text("no"));
        t.push(row);
        return t;
    };
    let mut counts = s.closed;
    let mut windows_idle = true;
    for w in &s.windows {
        let inner = w.inner.lock();
        counts += &inner.queue.counts();
        windows_idle &= inner.queue.is_idle() && w.idle.load(Ordering::Acquire) && !w.theme_behind();
    }
    let shown = vt::desktop_shown();
    let settled = Settled {
        shown,
        compositor_shown: s.shown,
        acted_on_cut: s.seen_generation == generation,
        all_decoded,
        pointer_read: s.pointer_read == pointer_newest,
        keys_read: s.keys_read == keys_newest,
        held_back: s.held_back,
        damage: !s.damage.is_empty(),
        passing: s.passing,
        committing: s.commits > 0,
        pending,
        windows_idle,
    }
    .holds();
    let wm = s.wm.counts();
    let ms = s.stats.present_ns / 1_000_000;
    let us = (s.stats.present_ns / 1_000) % 1_000;
    t.push(alloc::vec![
        text(if shown { "shown" } else { "paused" }),
        text(theme().name),
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
        int(counts.dead_pushes + s.stats.dead_pushes),
        int(0),
        int(0),
        yes(settled),
    ]);
    t
}

/// What `desktop status`'s `settled` is made of (§11): each a look taken
/// for that record.
#[derive(Clone, Copy)]
struct Settled {
    /// The desktop is shown (`vt::desktop_shown`).
    shown: bool,
    /// The compositor presents, and has acted on the cut's generation.
    compositor_shown: bool,
    acted_on_cut: bool,
    /// Every byte the keyboard queued is decoded.
    all_decoded: bool,
    /// Each reader is at its ring's newest.
    pointer_read: bool,
    keys_read: bool,
    /// The compositor holds events back.
    held_back: bool,
    /// Damage is gathered, a pass is running, a commit is under way, a wake
    /// is pending.
    damage: bool,
    passing: bool,
    committing: bool,
    pending: bool,
    /// Every window's queue is idle and its owner waits.
    windows_idle: bool,
}

impl Settled {
    fn holds(&self) -> bool {
        self.shown
            && self.compositor_shown
            && self.acted_on_cut
            && self.all_decoded
            && self.pointer_read
            && self.keys_read
            && !self.held_back
            && !self.damage
            && !self.passing
            && !self.committing
            && !self.pending
            && self.windows_idle
    }
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
    use crate::shell::sysinfo::print_table;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    // `check`, `spin`, `hold` and `release` need the desktop shown.
    if let [what, ..] = words[..]
        && matches!(what, "check" | "spin" | "hold" | "release")
        && let Some(why) = refused_while_paused()
    {
        return ctx.fail(why);
    }
    match words[..] {
        [] => match start() {
            Ok(Shown::Already) => print_table(ctx, Ok(status_table())),
            Ok(Shown::Now) => {
                let _ = writeln!(ctx.out, "desktop: started (Alt+F1 shows the terminals)");
            }
            Ok(Shown::Resumed) => {
                let _ = writeln!(ctx.out, "desktop: resumed");
            }
            Err(e) => ctx.fail(format_args!("desktop: {e}")),
        },
        ["status"] => print_table(ctx, Ok(status_table())),
        ["check"] => {
            let Some((w, h)) = COMPOSITOR.lock().as_ref().map(|s| (s.width, s.height)) else {
                return ctx.fail("desktop: no desktop");
            };
            let Some(scratch) = Surface::with_stride(w, h, w as usize) else {
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
                Ok(Answer::ScreenTaken) => ctx.fail("desktop check: the screen was taken back during the check"),
                Ok(Answer::Paused | Answer::Capture { .. } | Answer::WrongSize) => {
                    ctx.fail("desktop check: the compositor answered something else")
                }
                Err(e) => ctx.fail(format_args!("desktop check: {e}")),
            }
        }
        ["spin"] => match spin() {
            Ok(id) => {
                let _ = writeln!(ctx.out, "desktop spin: window {id}");
            }
            Err(e) => ctx.fail(format_args!("desktop spin: {e}")),
        },
        [what @ ("hold" | "release"), id] => match hold(id, what == "hold") {
            Ok(()) => {
                let _ = writeln!(ctx.out, "desktop {what}: window {id}");
            }
            Err(e) => ctx.fail(format_args!("desktop {what}: {e}")),
        },
        _ => ctx.fail(USAGE),
    }
}

/// Why `desktop check`, `spin`, `hold` and `release` refuse now, if they
/// do.
fn refused_while_paused() -> Option<&'static str> {
    let started = COMPOSITOR.lock().as_ref().is_some_and(|s| s.started);
    if !started {
        Some("desktop: not started (`desktop` starts it)")
    } else if !is_shown() {
        Some("desktop: paused (`desktop` resumes it)")
    } else {
        None
    }
}

/// The shell's `windows` (without a desktop, the plain line `windows: no
/// desktop`, which §11 and the harness match).
pub fn windows_command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: windows");
    }
    match windows_table() {
        Ok(t) => crate::shell::sysinfo::print_table(ctx, Ok(t)),
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
    fn a_window_opens_only_once_the_start_completed() {
        // A desktop whose start hasn't completed (its screen not taken yet)
        // refuses windows, so a start that fails opens none and no window
        // id is ever reused in a boot.
        use crate::{kassert, kassert_eq};
        let home = Home::Own(Arc::new(SpinLock::new(Some(State::new(1280, 800)))));
        let d = Desktop { home };
        kassert_eq!(d.open_window("w", 16, 16).err(), Some(OpenError::NoDesktop));
        if let Some(s) = d.home.lock().as_mut() {
            s.started = true;
        }
        kassert!(d.open_window("w", 16, 16).is_ok_and(|w| w.id() == 1));
        Ok(())
    }
);

/// Puts the live desktop's state back to none when dropped: a ktest that
/// sets one keeps one, so failing half-way leaves no desktop behind.
#[cfg(feature = "ktest")]
struct NoDesktopAfter;

#[cfg(feature = "ktest")]
impl Drop for NoDesktopAfter {
    fn drop(&mut self) {
        *COMPOSITOR.lock() = None;
    }
}

crate::ktest!(
    fn a_first_start_over_a_live_desktop_is_refused() {
        // A second `desktop` that (from a stale look) takes the first-start
        // path while a desktop is there, started or starting, is refused as
        // busy and leaves that desktop's state as it was: never a second
        // state over it, its compositor never told to end.
        use crate::kassert;
        kassert!(COMPOSITOR.lock().is_none());
        let _none = NoDesktopAfter;
        let mut live = State::new(321, 123);
        live.started = true;
        *COMPOSITOR.lock() = Some(live);
        kassert!(matches!(first_start(), Err(StartError::Busy)));
        let g = COMPOSITOR.lock();
        let s = g.as_ref().ok_or("the state was taken")?;
        kassert!((s.width, s.height, s.started, s.abort) == (321, 123, true, false));
        Ok(())
    }
);

crate::ktest!(
    fn settled_needs_every_one_of_its_conditions() {
        // `desktop status`'s `settled`: yes with every condition met, and
        // no as soon as any one of them isn't (a pass running included).
        use crate::kassert;
        let all = Settled {
            shown: true,
            compositor_shown: true,
            acted_on_cut: true,
            all_decoded: true,
            pointer_read: true,
            keys_read: true,
            held_back: false,
            damage: false,
            passing: false,
            committing: false,
            pending: false,
            windows_idle: true,
        };
        kassert!(all.holds());
        let broken: [fn(&mut Settled); 12] = [
            |s| s.shown = false,
            |s| s.compositor_shown = false,
            |s| s.acted_on_cut = false,
            |s| s.all_decoded = false,
            |s| s.pointer_read = false,
            |s| s.keys_read = false,
            |s| s.held_back = true,
            |s| s.damage = true,
            |s| s.passing = true,
            |s| s.committing = true,
            |s| s.pending = true,
            |s| s.windows_idle = false,
        ];
        for (i, f) in broken.iter().enumerate() {
            let mut one = all;
            f(&mut one);
            if one.holds() {
                crate::ktest::diag(format_args!("condition {i} ignored"));
                return Err("settled with a condition unmet");
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_closed_window_s_deliveries_count_as_dead_pushes() {
        // A delivery for a window no longer on the list (closed) is
        // counted, not lost silently; the closed window's own queue counts
        // are kept in the totals (its rationed key press, here).
        use crate::kassert_eq;
        let d = Desktop::without_screen();
        let w = d.open_window("w", 16, 16).map_err(|_| "a window")?;
        let id = {
            let mut g = d.home.lock();
            let s = g.as_mut().ok_or("the state")?;
            let id = s.wm.id(w.id()).ok_or("its id")?;
            // Fill its queue to the ration, then one press more: rationed.
            let mut after = After::default();
            for _ in 0..=lumen_wm::queue::RATIONED {
                let _ = s.push(id, &mut after, |q| Some(q.push(Entry::Key(test_key()))));
            }
            id
        };
        drop(w);
        let mut g = d.home.lock();
        let s = g.as_mut().ok_or("the state")?;
        let mut after = After::default();
        s.push(id, &mut after, |q| Some(q.push(Entry::Key(test_key()))));
        kassert_eq!(s.stats.dead_pushes, 1);
        kassert_eq!(after.wake.len(), 0);
        kassert_eq!(s.closed.rationed, 1);
        Ok(())
    }
);

/// A key press for the ktests.
#[cfg(feature = "ktest")]
fn test_key() -> KeyEvent {
    KeyEvent {
        code: lumen_input::KeyCode::A,
        action: KeyAction::Press { text: Some('a') },
        mods: lumen_input::Mods::NONE,
        after_drop: false,
    }
}

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

crate::ktest!(
    fn a_button_held_at_the_first_entry_is_ignored_until_released() {
        // The state the desktop starts with, entered as the compositor
        // enters it (`Wm::resume` with the cut's baseline): the left button
        // held at the start (pressed in the terminal) is no press on the
        // window under it: a move with it held drags nothing, a second
        // button pressed meanwhile is counted, not delivered. Once every
        // button is up, a press on the title bar drags as usual.
        use crate::{kassert, kassert_eq};
        let mut s = State::new(1280, 800);
        let (id, _) = s.wm.open("w", 200, 100, 100, 100);
        let frame = s.wm.window(id).ok_or("no window")?.frame();
        let _ = s.wm.resume(Buttons::LEFT);
        let bar = (frame.x() + 50, frame.y() + 10);
        let ignored = s.wm.counts().ignored_presses;
        let out = s.wm.pointer(Pointer::at(bar.0, bar.1, Buttons::LEFT));
        kassert!(out.deliveries.is_empty());
        let _ = s.wm.pointer(Pointer::at(bar.0 + 40, bar.1 + 40, Buttons::LEFT));
        let _ = s.wm.pointer(Pointer::at(bar.0 + 40, bar.1 + 40, Buttons::LEFT.with(Buttons::RIGHT)));
        kassert_eq!(s.wm.dragging(), None);
        kassert_eq!(s.wm.window(id).ok_or("no window")?.frame(), frame);
        kassert_eq!(s.wm.counts().ignored_presses, ignored + 1);
        let _ = s.wm.pointer(Pointer::at(bar.0 + 40, bar.1 + 40, Buttons::NONE));
        let _ = s.wm.pointer(Pointer::at(bar.0, bar.1, Buttons::LEFT));
        let _ = s.wm.pointer(Pointer::at(bar.0 + 10, bar.1 + 20, Buttons::LEFT));
        kassert_eq!(s.wm.dragging(), Some(id));
        kassert_eq!(s.wm.window(id).ok_or("no window")?.frame().x(), frame.x() + 10);
        Ok(())
    }
);

/// A one-pixel capture job for the ktests.
#[cfg(feature = "ktest")]
fn test_job() -> Job {
    Job::Capture(Box::new(Rows::new(1, 1).expect("one pixel")))
}

crate::ktest!(
    fn a_request_holds_the_slot_until_its_asker_takes_the_answer() {
        // The request slot `desktop check` and `screenshot` share: a second
        // request is refused while the first is queued, while the
        // compositor runs it (taken out of the slot) and while its answer
        // waits, so no asker gets another's answer; taking the answer frees
        // it. An asker that gives up withdraws a queued job, and a running
        // one's late answer is dropped, freeing the slot.
        use crate::{kassert, kassert_eq};
        let mut slot = Slot::Free;
        kassert!(slot.accept(test_job()).is_ok());
        kassert_eq!(slot.accept(test_job()).err(), Some(JobError::Busy));
        let (_, ticket) = slot.start().ok_or("not started")?;
        kassert!(slot.start().is_none());
        kassert_eq!(slot.accept(test_job()).err(), Some(JobError::Busy));
        kassert!(slot.take().is_none() && !slot.is_answered());
        kassert!(!slot.finish(ticket, Answer::Busy));
        kassert!(slot.is_answered());
        kassert_eq!(slot.accept(test_job()).err(), Some(JobError::Busy));
        kassert!(matches!(slot.take(), Some(Answer::Busy)));
        kassert!(matches!(slot, Slot::Free));
        // Given up while queued: withdrawn.
        kassert!(slot.accept(test_job()).is_ok());
        slot.abandon();
        kassert!(matches!(slot, Slot::Free) && slot.start().is_none());
        // Given up while running: the late answer is dropped.
        kassert!(slot.accept(test_job()).is_ok());
        let (_, ticket) = slot.start().ok_or("not started")?;
        slot.abandon();
        // Not "busy with another request": the one given up on still runs.
        kassert_eq!(slot.accept(test_job()).err(), Some(JobError::Stuck));
        // The late answer is dropped.
        kassert!(slot.finish(ticket, Answer::Busy));
        kassert!(matches!(slot, Slot::Free) && slot.take().is_none());
        Ok(())
    }
);

crate::ktest!(
    fn a_second_request_is_refused_while_the_first_waits() {
        // Through the asker's own steps, `submit` and `await_answer` (what
        // `run_job` calls on the live desktop), on a desktop of its own: the
        // first request is accepted, a second meanwhile is refused as busy
        // and never queued; once the first's answer is taken, the next is
        // accepted.
        use crate::{kassert, kassert_eq};
        let d = Desktop::without_screen();
        kassert!(submit(&d.home, test_job()).is_ok());
        kassert_eq!(submit(&d.home, test_job()).err(), Some(JobError::Busy));
        {
            let mut g = d.home.lock();
            let s = g.as_mut().ok_or("the state")?;
            let (_, ticket) = s.slot.start().ok_or("the first request")?;
            let _ = s.slot.finish(ticket, Answer::Busy);
        }
        kassert!(matches!(await_answer(&d.home), Ok(Answer::Busy)));
        kassert!(submit(&d.home, test_job()).is_ok());
        Ok(())
    }
);

crate::ktest!(
    fn a_request_queued_when_the_desktop_pauses_is_answered_paused() {
        // A request accepted just before Alt+F`n`: the paused pass answers
        // it at once (`Slot::answer_paused`), freeing the asker rather
        // than leaving it 20 s; the asker takes it as `JobError::Paused`,
        // which for `screenshot`, like a request refused while paused,
        // means "read the text screen".
        use crate::kassert;
        let d = Desktop::without_screen();
        let mut slot = Slot::Free;
        slot.answer_paused();
        kassert!(matches!(slot, Slot::Free));
        kassert!(submit(&d.home, test_job()).is_ok());
        if let Some(s) = d.home.lock().as_mut() {
            s.slot.answer_paused();
        }
        kassert!(matches!(await_answer(&d.home), Err(JobError::Paused)));
        kassert!(capture_answer(Err(JobError::Paused)).is_none());
        kassert!(matches!(capture_answer(Ok(Answer::WrongSize)), Some(Err(_))));
        kassert!(matches!(capture_answer(Err(JobError::Busy)), Some(Err(_))));
        Ok(())
    }
);

/// How many lines of the kernel log (as much as its ring holds) contain
/// `what`: compared before and after, to tell a warning was logged.
#[cfg(feature = "ktest")]
fn logged(what: &str) -> Result<usize, &'static str> {
    let mut log = alloc::vec![0u8; crate::klog::RING_BYTES];
    let (n, _) = crate::klog::copy_recent(&mut log);
    let log = core::str::from_utf8(&log[..n]).map_err(|_| "the log isn't UTF-8")?;
    Ok(log.lines().filter(|l| l.contains(what)).count())
}

crate::ktest!(
    fn a_bad_wallpaper_falls_back_with_one_warning() {
        // On a desktop with no screen: a wallpaper that doesn't decode (not
        // a PNG, cut short) leaves the theme's flat colour, one warning
        // each, and a recompose draws that colour; the embedded one decodes
        // to the screen's size, with no warning, and a recompose draws its
        // pixels.
        use crate::{kassert, kassert_eq};
        const SAID: &str = "WARN  desktop: wallpaper not shown: ";
        let d = Desktop::without_screen();
        let before = logged(SAID)?;
        kassert!(!d.set_wallpaper(b"not a PNG at all"));
        kassert_eq!(logged(SAID)?, before + 1);
        kassert!(!d.set_wallpaper(&WALLPAPER_PNG[..WALLPAPER_PNG.len() / 2]));
        kassert_eq!(logged(SAID)?, before + 2);
        let mut font = None;
        let mut target = Surface::with_stride(1280, 800, 1280).ok_or("no memory for a screen")?;
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        kassert!(scene.wallpaper.is_none());
        compose(&mut target, Rect::new(100, 100, 64, 32), &scene, &mut font);
        kassert_eq!(target.pixel(130, 110), Some(theme().desktop));
        kassert!(d.set_wallpaper(WALLPAPER_PNG));
        kassert_eq!(logged(SAID)?, before + 2);
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let wp = scene.wallpaper.clone().ok_or("the embedded wallpaper isn't shown")?;
        kassert_eq!((wp.width(), wp.height()), (1280, 800));
        compose(&mut target, Rect::new(100, 100, 64, 32), &scene, &mut font);
        kassert_eq!(target.pixel(130, 110), wp.pixel(130, 110));
        Ok(())
    }
);

crate::ktest!(
    fn a_title_drawn_again_rasterizes_nothing() {
        // A window's frame recomposed twice with the compositor's kind of
        // glyph cache: the first draw rasterizes the title's glyphs, the
        // second only hits the cache (and draws the same pixels).
        use crate::{kassert, kassert_eq};
        let d = Desktop::without_screen();
        let _w = d.open_window("Rasterized once", 200, 80).map_err(|_| "no window")?;
        let mut font = Some(text_font().ok_or("no UI font")?);
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let frame = scene.windows.first().ok_or("no window in the scene")?.frame;
        let mut first = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        compose(&mut first, frame, &scene, &mut font);
        let after_first = font.as_ref().map(|f| f.stats()).ok_or("no font")?;
        kassert!(after_first.misses > 0);
        let mut second = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        compose(&mut second, frame, &scene, &mut font);
        let after_second = font.as_ref().map(|f| f.stats()).ok_or("no font")?;
        kassert_eq!(after_second.misses, after_first.misses);
        kassert!(after_second.hits > after_first.hits);
        kassert!((0..800).all(|y| first.row(y) == second.row(y)));
        Ok(())
    }
);

crate::ktest!(
    fn a_theme_switch_repaints_everything_and_tells_each_window() {
        // On a desktop with no screen: switching to dark damages the whole
        // screen and tells the window's owner once, before the key queued
        // earlier (`Theme(dark)`, then the key); a switch to the theme
        // already shown changes nothing. Light again at the end (the theme
        // is the kernel's, not the test desktop's).
        use crate::{kassert, kassert_eq};
        let d = Desktop::without_screen();
        let mut w = d.open_window("w", 16, 16).map_err(|_| "no window")?;
        while w.next_event().is_some() {}
        let key = KeyEvent {
            code: lumen_input::KeyCode::A,
            action: KeyAction::Press { text: Some('a') },
            mods: lumen_input::Mods::NONE,
            after_drop: false,
        };
        if let Some(s) = d.home.lock().as_mut() {
            let _ = s.damage.take();
            let mut after = After::default();
            let id = w.win.id;
            kassert_eq!(s.push(id, &mut after, |q| Some(q.push(Entry::Key(key)))), Some(Pushed::Queued));
        }
        let result = (|| {
            kassert!(switch_theme(&d.home, &Theme::DARK));
            kassert_eq!(theme().name, "dark");
            let full = d.home.lock().as_mut().map(|s| s.damage.take()).ok_or("no state")?;
            kassert!(full.rects().iter().any(|r| *r == Rect::new(0, 0, 1280, 800)));
            kassert!(w.win.theme_behind());
            kassert!(matches!(w.next_event(), Some(WindowEvent::Theme(t)) if t.name == "dark"));
            kassert!(!w.win.theme_behind());
            kassert!(matches!(w.next_event(), Some(WindowEvent::Key(k)) if k == key));
            kassert!(w.next_event().is_none());
            kassert!(!switch_theme(&d.home, &Theme::DARK));
            kassert!(d.home.lock().as_ref().is_some_and(|s| s.damage.is_empty()));
            Ok(())
        })();
        let _ = switch_theme(&d.home, &Theme::LIGHT);
        result
    }
);
