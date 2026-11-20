//! The desktop (#153, #154; ARCHITECTURE section 11): windows on the whole
//! screen, composed by one kernel thread, `compositor`.
//!
//! - **Opt-in.** The text console stays the default. `desktop` (from any
//!   shell) or the `lumen.desktop` boot flag starts it: the back buffer is
//!   allocated, the embedded wallpaper decoded (once a boot: a flat colour
//!   and one warning if it doesn't decode), the compositor thread started,
//!   then the screen taken
//!   (`crate::vt::enter_desktop`: a `term::ScreenCanvas`); a step that
//!   fails undoes the ones before it (`desktop: not started: <reason>`).
//!   Alt+F1 to F4 leave it (`crate::vt`), `desktop` resumes it: its windows
//!   live on meanwhile. There is no `desktop off` in M4.
//! - **Windows** ([`open_window`], [`WindowHandle`]): an owner thread draws
//!   into its own surface with no lock and commits rectangles of it to the
//!   window's committed surface (at most 16 rows per hold of the window's
//!   lock); the compositor reads only the committed one. The frame (title
//!   bar, border, close box) is the compositor's, from `lumen-wm`'s
//!   geometry and the theme, its title in the UI font (`font::Text`). Events
//!   reach the owner through the window's `lumen_wm::EventQueue`
//!   ([`WindowHandle::next_event`], [`WindowHandle::wait_event`]); the first
//!   is always [`WindowEvent::Theme`], the theme to draw with.
//! - **The look** (#154): the theme (`lumen_wm::ThemeId`: `theme
//!   light|dark`, `lumen.theme=`) is the kernel's; a switch damages the
//!   whole screen and tells every owner again. The wallpaper is drawn
//!   centred behind the windows.
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
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use lumen_gfx::{Damage, Mask, Rect, Surface, Target};
use lumen_input::cursor::{SPRITE_H, SPRITE_W, draw_arrow};
use lumen_input::{Button, Buttons, KeyAction, KeyEvent, PointerEvent, Read, Reader, Seq};
use lumen_png::encode::Rows;
use lumen_ui::AutomationNode;
use lumen_wm::{
    Entry, EventQueue, Interruption, Metrics, Next, Outcome, Pointer, PointerEntry, Pushed, QueueCounts, Theme,
    ThemeId, WindowId, Wm,
};

use crate::font::{HEADING_PX, TEXT_PX, TITLE_PX, Text};
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

/// The theme drawn with (a [`ThemeId`] as a byte): light, until
/// `lumen.theme=` or `theme` picks another ([`switch_theme`]). One for the
/// kernel: there is one screen.
static THEME: AtomicU8 = AtomicU8::new(ThemeId::Light.index());
/// Bumped by every switch: a window whose count is behind is told
/// ([`WindowEvent::Theme`]) before anything else, and redraws. Every window
/// starts one behind, so its first event is the theme to draw with.
static THEME_GEN: AtomicU64 = AtomicU64::new(0);

fn theme_id() -> ThemeId {
    ThemeId::from_index(THEME.load(Ordering::Acquire)).unwrap_or(ThemeId::Light)
}

/// The theme drawn with now: the compositor's, `desktop status`'s and
/// `theme`'s (private: a window's owner draws with
/// [`WindowHandle::theme`], the theme it was told of).
fn theme() -> &'static Theme {
    Theme::get(theme_id())
}

/// The compositor's text: the titles' glyph cache and their line height
/// (worked out once).
struct Titles {
    text: Text,
    line: u32,
}

impl Titles {
    fn new() -> Option<Titles> {
        let text = Text::new().ok()?;
        let line = text.line_height(TITLE_PX);
        Some(Titles { text, line })
    }
}

/// The wallpaper the kernel embeds (`tools/wallpaper.py` made it), decoded
/// once a boot, at the desktop's first start ([`boot_wallpaper`]).
static WALLPAPER_PNG: &[u8] = include_bytes!("../../assets/wallpaper/lumen.png");
/// The largest wallpaper decoded: either side past it is refused. A bound
/// on what a file can make the decoder hold (its inflated rows can't
/// refuse): a 2048x2048 RGBA image peaks at about 40 MiB while it is
/// decoded. One bigger than the screen is drawn centred and cut.
const WALLPAPER_MAX: u32 = 2048;

/// Why a wallpaper isn't shown: a word for `desktop status` (`unreadable`,
/// `too-big`, `no-memory`, `none`) and the whole reason (the warning's).
#[derive(Debug)]
struct NotShown {
    word: &'static str,
    why: String,
}

/// A wallpaper, or why there is none (behind `Arc`s: copied under locks
/// without allocating).
type WallpaperResult = Result<Arc<Surface>, Arc<NotShown>>;

/// Where a wallpaper's decode is kept once it is done (a leaf lock:
/// nothing is allocated or freed under it).
type WallpaperCache = SpinLock<Option<WallpaperResult>>;

/// The embedded wallpaper's decode, once a boot ([`boot_wallpaper`]).
static BOOT_WALLPAPER: WallpaperCache = SpinLock::new(None);

/// An automation request (`ui invoke`, #156: `crate::ui::Request`). Not
/// `Clone`: it answers for itself when dropped, so a copy would answer
/// twice; never dropped under a desktop lock (the queue hands refused and
/// drained ones back: `push_request`, [`WindowHandle`]'s drop).
pub use crate::ui::Request;

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
    /// The close box was clicked, or `ui invoke <root> close`: the owner
    /// decides (dropping the handle closes the window).
    Close,
    /// An automation request (#156).
    Request(Request),
    /// The theme to draw the client area with (#154): a window's first
    /// event, and again after each switch (`theme`). Delivered ahead of
    /// anything queued (entries, `Close` and requests included), never
    /// dropped (a count per window, not a queue entry); several switches
    /// before the owner reads give at least one, the last carrying the
    /// current theme.
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
    /// Its root in the automation tree (role `window`: the compositor's).
    root: lumen_ui::NodeId,
    title: String,
    inner: SpinLock<WindowInner>,
    /// What its owner has to read (see [`pending::Pending`]).
    pending: pending::Pending,
    /// Set while the owner waits in [`WindowHandle::wait_event`] or in a
    /// [`WindowHandle::wait_event_until`] that isn't animating (an atomic
    /// outside the lock).
    idle: AtomicBool,
    /// Set by a [`Waker`]: the owner's wait ends (output to draw, #155);
    /// `settled` waits for it to be taken.
    poked: AtomicBool,
    /// Set through a [`Waker`] while work for the window runs on another
    /// thread (a terminal window's shell running a command); `settled`
    /// waits for it clear.
    busy: AtomicBool,
    /// Its owner waits here.
    events: WaitQueue,
    /// `desktop hold`: a `desktop spin` window's owner stops reading its
    /// events.
    held: AtomicBool,
    /// Opened by `desktop spin` (the only windows `desktop hold` takes).
    spin: bool,
    /// Key events its queue took.
    keys: AtomicU64,
    /// Whether it takes one `ui invoke` request at a time (an app's window,
    /// #207: opened with [`Requests::OneAtATime`]).
    one_request: bool,
    /// Its one request slot, while `one_request`: taken by the request
    /// queued or running (`ui::RequestSlot`), freed when it is answered,
    /// skipped or refused.
    request_out: Arc<AtomicBool>,
}

/// What a window's owner has to read, kept apart so that every wait goes
/// through [`Pending::has_event`] (`Window::wait`): something queued, or a
/// theme it hasn't been told of.
mod pending {
    use super::THEME_GEN;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    pub(super) struct Pending {
        /// Whether the queue holds an entry or a flag (`!is_idle()`), stored
        /// under the window's lock after every change, read without it.
        queued: AtomicBool,
        /// The theme switches its owner has been told of ([`THEME_GEN`]'s
        /// count when it last took one); one behind at the start, so a
        /// window's first event is its theme.
        theme_seen: AtomicU64,
    }

    impl Pending {
        /// Nothing queued, and the theme not told yet (`switches`: the count
        /// read with the theme the owner starts from).
        pub(super) fn new(switches: u64) -> Pending {
            Pending { queued: AtomicBool::new(false), theme_seen: AtomicU64::new(switches.wrapping_sub(1)) }
        }

        pub(super) fn set_queued(&self, queued: bool) {
            self.queued.store(queued, Ordering::Release);
        }

        /// Whether a theme switch (or the first theme) hasn't reached the
        /// owner yet.
        pub(super) fn theme_behind(&self) -> bool {
            self.theme_seen.load(Ordering::Acquire) != THEME_GEN.load(Ordering::Acquire)
        }

        /// Whether the owner has an event to read: one queued, or a theme
        /// to be told of. What every wait for an event wakes for.
        pub(super) fn has_event(&self) -> bool {
            self.queued.load(Ordering::Acquire) || self.theme_behind()
        }

        /// Mark the owner told of the theme now; whether it was behind (so
        /// the caller delivers `Theme`).
        pub(super) fn take_theme(&self) -> bool {
            let switches = THEME_GEN.load(Ordering::Acquire);
            self.theme_seen.swap(switches, Ordering::AcqRel) != switches
        }
    }
}

impl Window {
    /// Wait until the owner has an event ([`pending::Pending::has_event`],
    /// always), `also` holds, or the deadline (`now_ns`) passes; whether it
    /// woke before the deadline. The one way an owner blocks.
    fn wait(&self, deadline: Option<u64>, mut also: impl FnMut() -> bool) -> bool {
        let mut ready = || self.pending.has_event() || also();
        match deadline {
            None => {
                self.events.wait_until(ready);
                true
            }
            Some(at) => self.events.wait_until_or_deadline(at, &mut ready) || ready(),
        }
    }
}

/// What a window's lock guards.
struct WindowInner {
    /// The surface the compositor reads.
    committed: Surface,
    queue: Queue,
    /// The thread that last read its events (the opener's until then).
    owner: &'static str,
    /// Its toolkit's tree as last published ([`WindowHandle::publish`]):
    /// the root's children, and every node below it with its depth (from
    /// 1), client coordinates. Empty for a window without a toolkit.
    top: Vec<lumen_ui::NodeId>,
    nodes: Vec<(u32, AutomationNode)>,
}

/// How many `ui invoke` requests a window holds at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requests {
    /// As many as its queue takes (a kernel window: its owner answers each
    /// in turn).
    Many,
    /// One queued or running (an app's window, #207: its `window_answer`
    /// answers *the* running request): another is refused busy before
    /// anything is queued.
    OneAtATime,
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
    /// Deliveries and `ui invoke` requests for a window no longer on the
    /// list (closed).
    dead_pushes: u64,
    /// `ui invoke` requests cancelled at their deadline, and answers that
    /// came after it ("ran late").
    cancelled: u64,
    late: u64,
    /// Glyphs the compositor rasterized for the titles (its own cache's
    /// misses): flat once every title's glyphs are cached; `None` with no
    /// UI font.
    glyph_misses: Option<u64>,
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
    /// `ui invoke` requests queued and not finished by their askers.
    requests: u32,
    /// The wallpaper drawn behind the windows, centred, or why there is
    /// none (then the theme's flat colour): `desktop status`'s `wallpaper`.
    wallpaper: WallpaperResult,
    /// The taskbar's owner (#157), woken when a window opens or closes.
    taskbar: Option<Waker>,
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
            requests: 0,
            wallpaper: Err(Arc::new(NotShown { word: "none", why: String::from("none set") })),
            taskbar: None,
        }
    }

    /// A window opened or closed: the taskbar's owner is poked now, under
    /// this lock (so `settled`, which reads `poked` under it, can't say yes
    /// before the taskbar has caught up), and woken once the locks are
    /// released.
    fn list_changed(&mut self, after: &mut After) {
        let Some(t) = &self.taskbar else { return };
        if let Some(win) = self.windows.iter().find(|w| t.wakes(w)) {
            win.poked.store(true, Ordering::Release);
        }
        after.taskbar = Some(t.clone());
    }

    /// The windows that aren't panels (the taskbar), on the screen or
    /// minimized, back to front.
    fn ordinary_windows(&self) -> impl DoubleEndedIterator<Item = &lumen_wm::Window> {
        self.wm.windows().iter().filter(|w| !w.is_panel())
    }

    /// Damage the whole screen (a full recompose at the next pass).
    fn damage_all(&mut self) {
        self.damage.add(Rect::new(0, 0, self.width, self.height));
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
    /// Never for requests: `f` would be dropped here, under the lock, with
    /// the request it holds ([`push_request`] hands one back instead).
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
        let pushed = with_queue(win, f);
        if let Some(Pushed::Dropped { first_in_burst: true }) = pushed {
            after.behind.push(id.get());
        }
        after.wake.push(win.clone());
        pushed
    }
}

/// Change `win`'s queue with `f` under its lock (`f` always runs), keeping
/// the `queued` flag the owner's wake-up check reads.
fn with_queue<T>(win: &Window, f: impl FnOnce(&mut Queue) -> T) -> T {
    let mut inner = win.inner.lock();
    let out = f(&mut inner.queue);
    win.pending.set_queued(!inner.queue.is_idle());
    out
}

/// Work for after the locks: owners to wake, warnings to log.
#[derive(Default)]
struct After {
    wake: Vec<Arc<Window>>,
    behind: Vec<u32>,
    /// The taskbar's owner, to tell its windows changed.
    taskbar: Option<Waker>,
}

impl After {
    fn finish(self) {
        for w in &self.wake {
            w.events.wake_all();
        }
        if let Some(t) = &self.taskbar {
            // (Poked already, under the desktop's lock: `list_changed`.)
            t.wake_only();
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
/// a desktop running. A clone is the same desktop.
#[derive(Clone)]
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

    /// Decode `png` as this desktop's wallpaper, as a start decodes the
    /// embedded one (not cached): `Err` with why if it isn't shown, the
    /// theme's flat colour drawn instead and one warning logged (`Err("no
    /// desktop")`, with no warning, for a desktop not started). The whole
    /// screen is damaged either way.
    pub fn set_wallpaper(&self, png: &[u8]) -> Result<(), String> {
        if self.home.lock().is_none() {
            return Err(String::from("no desktop"));
        }
        let wallpaper = load_wallpaper(png);
        let result = wallpaper.as_ref().map(|_| ()).map_err(|not| not.why.clone());
        if let Some(s) = self.home.lock().as_mut() {
            s.wallpaper = wallpaper;
            s.damage_all();
        }
        self.home.wake();
        result
    }

    /// Open a window of `width` x `height` client pixels titled `title`,
    /// in front and focused, its frame's corner placed by how many are
    /// open. `NoDesktop` until the desktop's start completed.
    pub fn open_window(&self, title: &str, width: u32, height: u32) -> Result<WindowHandle, OpenError> {
        self.open(title, width, height, Place::Auto, false, Requests::Many)
    }

    /// Whether window `id` is open on this desktop (for the ktests: a
    /// request's answer checks its window is gone first).
    #[cfg(feature = "ktest")]
    pub fn is_open(&self, id: u32) -> bool {
        self.home.lock().as_ref().is_some_and(|s| s.wm.id(id).is_some())
    }

    /// [`Desktop::open_window`], taking `ui invoke` requests as `requests`
    /// says (an app's window: one at a time, from its opening on).
    pub fn open_window_with(
        &self,
        title: &str,
        width: u32,
        height: u32,
        requests: Requests,
    ) -> Result<WindowHandle, OpenError> {
        self.open(title, width, height, Place::Auto, false, requests)
    }

    /// Open a panel (#157, the taskbar) docked along the bottom, `rows`
    /// high and the screen's width: a frameless window in front of every
    /// other, never focused or moved, the windows clamped above it (see
    /// `lumen_wm::Wm::open_panel`). Its owner is woken (a [`Waker`] poke)
    /// whenever a window opens or closes.
    pub fn open_panel(&self, title: &str, rows: u32) -> Result<WindowHandle, OpenError> {
        let width = self.home.lock().as_ref().map_or(0, |s| s.width);
        let h = self.open(title, width, rows, Place::Panel, false, Requests::Many)?;
        if let Some(s) = self.home.lock().as_mut() {
            s.taskbar = Some(h.waker());
        }
        Ok(h)
    }

    fn open(
        &self,
        title: &str,
        width: u32,
        height: u32,
        at: Place,
        spin: bool,
        requests: Requests,
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
        // Its client area until the owner's first commit, filled before the
        // lock (a `SpinLock`: interrupts off; up to 4 MiB to fill): the
        // theme's colour now. A switch before the lock is fixed by the
        // first `Theme` the owner gets, whatever it says.
        committed.fill(theme().client);
        let owner = crate::sched::running_here().map_or("boot", |(_, name)| name);
        let mut after = After::default();
        let win = {
            let mut g = self.home.lock();
            let s = g.as_mut().filter(|s| s.started).ok_or(OpenError::NoDesktop)?;
            if s.windows.len() >= MAX_WINDOWS {
                return Err(OpenError::TooMany);
            }
            // Placed by how many windows (not panels) are open.
            let n = s.ordinary_windows().count() as i32;
            let (id, out) = match at {
                Place::Panel => s.wm.open_panel(title, height),
                Place::At(x, y) => s.wm.open(title, width, height, x, y),
                Place::Auto => s.wm.open(title, width, height, 96 + 32 * (n % 8), 80 + 32 * (n % 8)),
            };
            // The theme and its count, read together under the live
            // desktop's lock (every switch holds it); a test desktop's isn't
            // that lock, and needn't be: the count starts one behind, so the
            // owner's first event is the theme as it is then.
            let (switches, theme) = (THEME_GEN.load(Ordering::Acquire), theme());
            let win = Arc::new(Window {
                id,
                root: lumen_ui::NodeId::fresh(),
                title: String::from(title),
                inner: SpinLock::new(WindowInner {
                    committed,
                    queue: Queue::new(),
                    owner,
                    top: Vec::new(),
                    nodes: Vec::new(),
                }),
                pending: pending::Pending::new(switches),
                idle: AtomicBool::new(false),
                poked: AtomicBool::new(false),
                busy: AtomicBool::new(false),
                events: WaitQueue::new(),
                held: AtomicBool::new(false),
                spin,
                keys: AtomicU64::new(0),
                one_request: requests == Requests::OneAtATime,
                request_out: Arc::new(AtomicBool::new(false)),
            });
            s.windows.push(win.clone());
            s.apply(out, &mut after);
            s.list_changed(&mut after);
            (win, theme)
        };
        let (win, theme) = win;
        after.finish();
        self.home.wake();
        Ok(WindowHandle { win, surface, theme, home: self.home.clone(), _not_sync: PhantomData })
    }
}

/// Open a window on the live desktop (see [`Desktop::open_window`]).
#[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "its first callers are #155-#157's windows"))]
pub fn open_window(title: &str, width: u32, height: u32) -> Result<WindowHandle, OpenError> {
    Desktop::LIVE.open_window(title, width, height)
}

/// Open a window on the live desktop taking requests as `requests` says
/// (see [`Desktop::open_window_with`]).
pub fn open_window_with(title: &str, width: u32, height: u32, requests: Requests) -> Result<WindowHandle, OpenError> {
    Desktop::LIVE.open_window_with(title, width, height, requests)
}

/// [`open_window`] with its frame's corner at (`x`, `y`) (clamped like a
/// move, so a strip of its title bar stays on the screen): terminal windows
/// (#155) side by side.
pub fn open_window_at(title: &str, width: u32, height: u32, x: i32, y: i32) -> Result<WindowHandle, OpenError> {
    Desktop::LIVE.open(title, width, height, Place::At(x, y), false, Requests::Many)
}

/// Where [`Desktop::open`] puts a window.
#[derive(Clone, Copy)]
enum Place {
    /// By how many are open.
    Auto,
    /// Its frame's corner there (clamped like a move).
    At(i32, i32),
    /// A panel docked along the bottom (its client size: the screen's
    /// width, its rows).
    Panel,
}

/// Another thread's handle on a window's owner (#155: a terminal window's
/// shell): it ends the owner's wait ([`WindowHandle::wait_event_until`])
/// with no event, and marks the window busy for `settled`. Cloned freely.
/// Weak: it keeps nothing of a closed window alive, and does nothing to
/// one.
#[derive(Clone)]
pub struct Waker(Weak<Window>);

impl Waker {
    /// End the owner's wait (or the next one, if it isn't waiting): it has
    /// something to look at. Takes only the window's wait queue's lock:
    /// call it holding no desktop lock.
    pub fn wake(&self) {
        if let Some(w) = self.0.upgrade() {
            w.poked.store(true, Ordering::Release);
            w.events.wake_all();
        }
    }

    /// Whether this wakes `win`'s owner: compared by pointer, never
    /// upgraded (so no reference is made, or dropped, under a lock).
    fn wakes(&self, win: &Arc<Window>) -> bool {
        core::ptr::eq(self.0.as_ptr(), Arc::as_ptr(win))
    }

    /// End the owner's wait, `poked` already set (by `State::list_changed`,
    /// under the desktop's lock).
    fn wake_only(&self) {
        if let Some(w) = self.0.upgrade() {
            w.events.wake_all();
        }
    }

    /// Mark the window busy (work for it is under way on another thread)
    /// or not: `desktop status`'s `settled` waits for every window to be
    /// clear of it. A window opens not busy. One writer at a time: the
    /// thread that hands off the work sets it before the hand-off is seen
    /// (a terminal window's owner giving its shell a key), and the thread
    /// that ends it clears it once nothing is left (the shell, waiting for
    /// a key with none queued); no third party touches it.
    pub fn set_busy(&self, busy: bool) {
        if let Some(w) = self.live() {
            w.set_busy(busy);
        }
    }

    /// The window, held for a moment, if it is still open: so a caller can
    /// set its busy bit under a lock of its own without the last reference
    /// to the window going (and the window being freed) under that lock.
    pub fn live(&self) -> Option<LiveWindow> {
        self.0.upgrade().map(LiveWindow)
    }
}

/// A window held for a moment through its [`Waker`] (see [`Waker::live`]).
pub struct LiveWindow(Arc<Window>);

impl LiveWindow {
    /// [`Waker::set_busy`], on the window held.
    pub fn set_busy(&self, busy: bool) {
        self.0.busy.store(busy, Ordering::Release);
    }
}

/// Whether a wait with a deadline is part of an animation (a blinking
/// cursor, Life's next generation): an animating wait never counts as idle,
/// so `desktop status` never says `settled` while it lasts; a wait that
/// isn't (a clock waiting for its next minute) idles like
/// [`WindowHandle::wait_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Animating {
    Yes,
    No,
}

/// How a [`WindowHandle::wait_event_until`] ended. An event must be
/// taken; the other two say only that the owner should look again at its
/// own state (what another thread changed, the time).
#[derive(Debug)]
#[must_use = "an event the wait returned must be taken, or it is lost"]
pub enum Wait {
    /// An event for the owner.
    Event(WindowEvent),
    /// A [`Waker`] woke it (or one did since the last wait): re-check your
    /// own state.
    Woken,
    /// The deadline passed.
    Deadline,
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
    /// The theme the owner was last told of (at the open, the theme then).
    theme: &'static Theme,
    home: Home,
    _not_sync: PhantomData<Cell<()>>,
}

impl WindowHandle {
    /// The window's id (from 1, never reused in a boot).
    pub fn id(&self) -> u32 {
        self.win.id.get()
    }

    /// The theme to draw the client area with: the last one a
    /// [`WindowEvent::Theme`] handed the owner (the theme at the open until
    /// the first). Owners redraw on `Theme` and read this.
    pub fn theme(&self) -> &'static Theme {
        self.theme
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
            // A minimized window isn't on the screen: nothing to redraw
            // (its restore damages its whole frame).
            let shown = s.wm.window(self.win.id).is_some_and(|w| !w.is_minimized());
            if let Some(client) = s.wm.client(self.win.id).filter(|_| shown) {
                for r in rects.iter().filter_map(|r| r.intersect(&bounds)) {
                    s.damage.add(r.translate(client.x(), client.y()));
                }
            }
        }
        self.home.wake();
    }

    /// The next event, if one is waiting (never blocks): the theme to draw
    /// with first (the window's first event, then after each switch), then
    /// the queue's.
    pub fn next_event(&mut self) -> Option<WindowEvent> {
        if self.win.pending.take_theme() {
            self.theme = theme();
            return Some(WindowEvent::Theme(self.theme));
        }
        let mut inner = self.win.inner.lock();
        if let Some((_, name)) = crate::sched::running_here() {
            inner.owner = name;
        }
        let next = inner.queue.next_event();
        self.win.pending.set_queued(!inner.queue.is_idle());
        next.map(WindowEvent::from)
    }

    /// The next event, waiting for one: blocked until the owner has one
    /// (something queued, or a theme to be told of), the window counting as
    /// idle meanwhile (`desktop status`'s `settled`).
    /// Built on [`Self::wait_event_until`], it takes (and ignores) a
    /// [`Waker`]'s pokes: an owner that waits here hands no `Waker` out.
    pub fn wait_event(&mut self) -> WindowEvent {
        loop {
            if let Wait::Event(e) = self.wait_event_until(None, Animating::No) {
                return e;
            }
        }
    }

    /// The next event, waiting for one until `deadline` (`time::now_ns`;
    /// `None`: no deadline) or until a [`Waker`] of this window wakes it
    /// (at once if one woke it since the last wait). Through `Window::wait`,
    /// so a theme to be told of wakes it too. The window counts as idle
    /// while it waits unless the wait is `animating` (see [`Animating`]): a
    /// terminal window's owner (#155) waits here for keys, its shell's
    /// output and, while focused, its cursor's next blink (animating).
    pub fn wait_event_until(&mut self, deadline: Option<u64>, animating: Animating) -> Wait {
        debug_assert!(
            deadline.is_some() || animating == Animating::No,
            "an animating wait needs its next frame's deadline"
        );
        if let Some(e) = self.next_event() {
            return Wait::Event(e);
        }
        if self.win.poked.swap(false, Ordering::AcqRel) {
            return Wait::Woken;
        }
        self.win.idle.store(animating == Animating::No, Ordering::Release);
        let win = &self.win;
        let woke = win.wait(deadline, || win.poked.load(Ordering::Acquire));
        self.win.idle.store(false, Ordering::Release);
        if let Some(e) = self.next_event() {
            return Wait::Event(e);
        }
        if self.win.poked.swap(false, Ordering::AcqRel) || woke { Wait::Woken } else { Wait::Deadline }
    }

    /// A [`Waker`] for this window's owner, for another thread.
    pub fn waker(&self) -> Waker {
        Waker(Arc::downgrade(&self.win))
    }

    /// The window's title, as the title bar and `windows` show it (for
    /// the tests).
    #[cfg(feature = "ktest")]
    pub fn title(&self) -> &str {
        &self.win.title
    }

    /// Whether a [`Waker`] marks the window busy (for the tests).
    #[cfg(feature = "ktest")]
    pub fn is_busy(&self) -> bool {
        self.win.busy.load(Ordering::Acquire)
    }

    /// Publish its toolkit's tree (`ui tree` reads it without asking the
    /// owner): `top`, the root's children, and every node below the root
    /// with its depth (from 1), client coordinates. Built by the owner,
    /// then swapped in under the window's lock.
    pub fn publish(&mut self, top: Vec<lumen_ui::NodeId>, nodes: Vec<(u32, AutomationNode)>) {
        let (old_top, old_nodes) = {
            let mut inner = self.win.inner.lock();
            (core::mem::replace(&mut inner.top, top), core::mem::replace(&mut inner.nodes, nodes))
        };
        drop((old_top, old_nodes));
    }

    /// Its request slot's flag (for the ktests: whether a request holds it).
    #[cfg(feature = "ktest")]
    pub fn request_flag(&self) -> Arc<AtomicBool> {
        self.win.request_out.clone()
    }

    /// Node `id` (a global id) of the tree as last published, with its
    /// depth (client coordinates); `None` if it isn't in it.
    pub fn published_node(&self, id: u64) -> Option<(u32, AutomationNode)> {
        let inner = self.win.inner.lock();
        inner.nodes.iter().find(|(_, n)| n.id().get() == id).cloned()
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
        // The requests still queued come back from the queue, answered
        // ("window closed") once every lock is released.
        let mut after = After::default();
        let mut g = self.home.lock();
        let unanswered = if let Some(s) = g.as_mut() {
            s.windows.retain(|w| !Arc::ptr_eq(w, &self.win));
            let (counts, unanswered) = {
                let mut inner = self.win.inner.lock();
                let unanswered = inner.queue.kill();
                self.win.pending.set_queued(false);
                (inner.queue.counts(), unanswered)
            };
            s.closed += &counts;
            if let Some(out) = s.wm.close(self.win.id) {
                s.apply(out, &mut after);
            }
            if s.taskbar.as_ref().is_some_and(|t| t.wakes(&self.win)) {
                s.taskbar = None;
            }
            s.list_changed(&mut after);
            unanswered
        } else {
            self.win.inner.lock().queue.kill()
        };
        drop(g);
        drop(unanswered);
        after.finish();
        self.home.wake();
    }
}

// -- drawing -----------------------------------------------------------------

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
    /// A panel (#157): no frame drawn, its client area its frame.
    panel: bool,
}

/// What a pass draws: the windows back to front, the theme and the
/// wallpaper, all taken in one hold of the desktop's lock.
struct Scene {
    windows: Vec<Snap>,
    theme: &'static Theme,
    wallpaper: Option<Arc<Surface>>,
}

/// Where a `wallpaper` goes on a `target`: centred, cut where it is
/// bigger (it may be up to [`WALLPAPER_MAX`] each way).
fn wallpaper_place(wallpaper: &Surface, target: &Surface) -> Rect {
    let x = (i64::from(target.width()) - i64::from(wallpaper.width())) / 2;
    let y = (i64::from(target.height()) - i64::from(wallpaper.height())) / 2;
    // A screen and a wallpaper of at most WALLPAPER_MAX: far inside `i32`.
    Rect::new(x as i32, y as i32, wallpaper.width(), wallpaper.height())
}

/// Recompose `r` (screen coordinates) of the scene into `target`: the
/// wallpaper (the theme's colour where there is none), then every window
/// overlapping it from back to front, its frame then its committed surface
/// (blitted [`BAND`] rows per hold of the window's lock). Everything drawn
/// stays inside `r`, so damage-only recomposition equals a full one.
fn compose(target: &mut Surface, r: Rect, scene: &Scene, titles: &mut Option<Titles>) {
    let Some(r) = r.clip(target.width(), target.height()) else { return };
    paint_desktop(target, r, scene);
    for snap in &scene.windows {
        let Some(f) = snap.frame.intersect(&r) else { continue };
        let client = if snap.panel {
            snap.frame
        } else {
            paint_frame(target, snap, f, scene.theme, titles.as_mut());
            METRICS.client(&snap.frame)
        };
        let Some(c) = client.intersect(&r) else { continue };
        for band in bands(c) {
            let src = band.translate(-client.x(), -client.y());
            target.blit(&snap.win.inner.lock().committed, src, band.x(), band.y());
        }
    }
}

/// `r` (on the target) of what is behind the windows: the wallpaper where
/// it is, the theme's colour elsewhere (each pixel drawn once).
fn paint_desktop(target: &mut Surface, r: Rect, scene: &Scene) {
    let covered = scene.wallpaper.as_deref().and_then(|wp| {
        let at = wallpaper_place(wp, target);
        at.intersect(&r).map(|w| (wp, at, w))
    });
    let Some((wp, at, w)) = covered else {
        target.fill_rect(r, scene.theme.desktop);
        return;
    };
    for piece in &r.subtract(&w) {
        target.fill_rect(*piece, scene.theme.desktop);
    }
    target.blit(wp, w.translate(-at.x(), -at.y()), w.x(), w.y());
}

/// The part `clip` (inside the frame) of a window's frame: the border, the
/// title bar with its title (none without a font), the close box with its
/// cross.
fn paint_frame(target: &mut Surface, snap: &Snap, clip: Rect, theme: &Theme, titles: Option<&mut Titles>) {
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
    // The minimize box (#157): a bar along its bottom, in the title's
    // colour.
    let minimize = METRICS.minimize_box(&frame);
    if !minimize.is_empty() {
        let line = Rect::new(minimize.x() + 4, minimize.bottom() - 5, minimize.w().saturating_sub(8), 2);
        if let Some(l) = line.intersect(&clip) {
            target.fill_rect(l, title_fg);
        }
    }
    // The title, left of the boxes, never over them.
    let first_box = if minimize.is_empty() { close } else { minimize };
    let text_right = first_box.x().saturating_sub(4).max(bar.x());
    let area = Rect::new(bar.x(), bar.y(), (text_right - bar.x()).max(0) as u32, bar.h());
    if let (Some(a), Some(t)) = (area.intersect(&clip), titles) {
        let y = bar.y() + (METRICS.title_h as i32 - t.line as i32) / 2;
        t.text.draw(target, bar.x() + 8, y, &snap.win.title, TITLE_PX, title_fg, a);
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
    /// The titles' text (the compositor's own glyph cache).
    titles: Option<Titles>,
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
    let ms = crate::cmdline::millis(KEY, CUT_DELAY_MAX_MS);
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
            titles: Titles::new(),
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
            compose(&mut self.back, *r, &scene, &mut self.titles);
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
        let pointer_moved = self.place_pointer(damage.rects(), scene.theme);
        let frame = !damage.is_empty() || pointer_moved;
        if !self.timed && pixels > 0 {
            // The first present is the whole screen (the start's full
            // damage): how long it took to compose (the wallpaper, and any
            // window already open: the demo windows open just after the
            // start, so usually none) and to copy to the screen.
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
    fn place_pointer(&mut self, presented: &[Rect], theme: &Theme) -> bool {
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
        s.stats.glyph_misses = self.titles.as_ref().map(|t| t.text.stats().misses);
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
        compose(&mut scratch, all, scene, &mut self.titles);
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
            .filter(|w| !w.is_minimized())
            .filter_map(|w| {
                let win = s.window(w.id())?.clone();
                Some(Snap { win, frame: w.frame(), focused: focused == Some(w.id()), panel: w.is_panel() })
            })
            .collect();
    Scene { windows, theme: theme(), wallpaper: s.wallpaper.as_ref().ok().cloned() }
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
    if result.is_ok() {
        // Each start (the first, after its demo windows; a resume; one that
        // found it shown) opens the taskbar if there is none.
        ensure_taskbar();
    }
    START_BUSY.store(false, Ordering::Release);
    result.map_err(|why| Refused { resuming: started, why })
}

/// `png` as a surface to draw behind the windows: refused (why) unless it
/// decodes within [`WALLPAPER_MAX`]. Its pixels become the surface's (no
/// copy). The decoder's own buffers abort on a heap that can't hold them
/// (`lumen_png::decode`); the pixels refuse.
fn decode_wallpaper(png: &[u8]) -> Result<Surface, NotShown> {
    use lumen_png::decode::DecodeError;
    let image = lumen_png::decode::decode(png, WALLPAPER_MAX, WALLPAPER_MAX).map_err(|e| {
        let word = match e {
            DecodeError::Png(_) => "unreadable",
            DecodeError::TooBig { .. } => "too-big",
            DecodeError::OutOfMemory => "no-memory",
        };
        NotShown { word, why: alloc::format!("{e}") }
    })?;
    let (w, h) = (image.width(), image.height());
    Surface::from_pixels(w, h, image.into_pixels())
        .ok_or_else(|| NotShown { word: "unreadable", why: String::from("its pixels aren't its size") })
}

/// The wallpaper from `png`, or why not, with one warning saying why:
/// `WARN desktop: wallpaper not shown: <reason>` (then the theme's flat
/// colour is drawn).
fn load_wallpaper(png: &[u8]) -> WallpaperResult {
    let start = crate::time::now_ns();
    match decode_wallpaper(png) {
        Ok(s) => {
            let took = lumen_time::Millis(crate::time::now_ns().saturating_sub(start));
            log::info!(target: "desktop", "wallpaper: {}x{}, {} bytes of PNG decoded in {took}", s.width(), s.height(), png.len());
            Ok(Arc::new(s))
        }
        Err(not) => {
            log::warn!(target: "desktop", "wallpaper not shown: {}", not.why);
            Err(Arc::new(not))
        }
    }
}

/// `png`'s wallpaper, decoded at the first call and kept in `cache`, so it
/// is decoded and a failure warned about once; but a decode the heap
/// couldn't hold isn't kept, and is tried (and warned about) again at the
/// next call. Called with no lock held (a decode takes tens of
/// milliseconds); the callers are one at a time (`desktop`'s first start).
fn cached_wallpaper(cache: &WallpaperCache, png: &[u8]) -> WallpaperResult {
    if let Some(done) = cache.lock().as_ref() {
        return done.clone();
    }
    let got = load_wallpaper(png);
    if matches!(&got, Err(not) if not.word == "no-memory") {
        return got;
    }
    let mut kept = cache.lock();
    match kept.as_ref() {
        Some(done) => done.clone(),
        None => {
            *kept = Some(got.clone());
            got
        }
    }
}

/// The embedded wallpaper ([`cached_wallpaper`]: once a boot).
fn boot_wallpaper() -> WallpaperResult {
    cached_wallpaper(&BOOT_WALLPAPER, WALLPAPER_PNG)
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
    // The back buffer first (the desktop needs it; the wallpaper is
    // optional), then the wallpaper, with no lock held (a warning, and the
    // theme's flat colour, if it doesn't decode).
    let back = Surface::with_stride(w, h, w as usize).ok_or(StartError::NoMemory)?;
    let wallpaper = boot_wallpaper();
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
    match ThemeId::by_name(name) {
        Some(id) => {
            let _ = switch_theme(&Home::Live, id);
            log::info!(target: "desktop", "theme {} ({KEY})", id.name());
        }
        None => log::warn!(target: "desktop", "{KEY}={name}: not a theme (light, dark), ignored"),
    }
}

/// Switch to theme `id` (`theme`, `lumen.theme=`), whether it changed. The
/// theme is the kernel's (one screen): under the live desktop's lock, the
/// theme and its count change and the live desktop's whole screen is
/// damaged (one full repaint); `home`'s too when it is another (a test's
/// desktop). After the locks, every window's owner there is woken to be
/// told ([`WindowEvent::Theme`]), and the compositor.
fn switch_theme(home: &Home, id: ThemeId) -> bool {
    let full = |s: &mut State, windows: &mut Vec<Arc<Window>>| {
        s.damage_all();
        windows.extend(s.windows.iter().cloned());
    };
    let mut windows = Vec::new();
    {
        let mut live = COMPOSITOR.lock();
        if theme_id() == id {
            return false;
        }
        THEME.store(id.index(), Ordering::Release);
        THEME_GEN.fetch_add(1, Ordering::AcqRel);
        if let Some(s) = live.as_mut() {
            full(s, &mut windows);
        }
    }
    if let Home::Own(_) = home
        && let Some(s) = home.lock().as_mut()
    {
        full(s, &mut windows);
    }
    for w in &windows {
        w.events.wake_all();
    }
    wake();
    true
}

/// The shell's `theme`: the desktop's theme, or switch it (before the
/// desktop starts too: it starts in that theme).
pub fn theme_command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    match args {
        [] => {
            let _ = writeln!(ctx.out, "theme: {}", theme().id.name());
        }
        [name] => match ThemeId::by_name(name) {
            Some(id) => {
                let note = if switch_theme(&Home::Live, id) { "" } else { " (already)" };
                let _ = writeln!(ctx.out, "theme: {}{note}", id.name());
            }
            None => ctx.fail(format_args!("theme: no theme {name:?} (light, dark)")),
        },
        _ => ctx.fail("usage: theme [light|dark]"),
    }
}

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

// -- the automation tree (#156; `crate::ui` prints it) ---------------------------------

/// A node of `ui tree`: its window, its depth (0 for the root) and the node,
/// its bounds on the screen.
pub struct TreeRow {
    pub window: u32,
    pub depth: u32,
    pub node: AutomationNode,
}

/// Where a node is: a window's root, or a node of its published tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAt {
    Root(u32),
    Widget(u32),
}

/// Window `id`'s root's row: its root node (role `window`, named by its
/// title, its children its toolkit's top nodes), bounds its client area on
/// the screen, depth 0.
fn root_row(s: &State, id: WindowId) -> Option<TreeRow> {
    let w = s.wm.window(id)?;
    let win = s.window(id)?;
    let client = s.wm.client(id)?;
    let top = win.inner.lock().top.clone();
    let node = if w.is_panel() {
        // The taskbar's root is a group: no actions (it can't be focused,
        // minimized or closed).
        AutomationNode::group(win.root, w.title(), client, false, top)
    } else {
        AutomationNode::window(win.root, w.title(), client, s.wm.focused() == Some(id), top)
    };
    Some(TreeRow { window: id.get(), depth: 0, node })
}

/// The role of window `window`'s root: `window`, or `group` for a panel
/// (the taskbar); `None` if it isn't open.
pub fn root_role(window: u32) -> Option<lumen_ui::Role> {
    let g = COMPOSITOR.lock();
    let s = g.as_ref()?;
    let w = s.wm.window(s.wm.id(window)?)?;
    Some(if w.is_panel() { lumen_ui::Role::Group } else { lumen_ui::Role::Window })
}

/// Why [`minimize_window`] or [`restore_window`] did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotChanged {
    Closed,
    /// Minimized already (a minimize), or not minimized (a restore).
    Already,
    /// The window is the taskbar (a panel: never minimized).
    Taskbar,
}

/// Minimize (`to`) or restore window `window` (`ui invoke <root>
/// minimize|restore`, the taskbar): its root's row after.
fn set_minimized(window: u32, to: bool) -> Result<TreeRow, NotChanged> {
    let mut after = After::default();
    let row = {
        let mut g = COMPOSITOR.lock();
        let s = g.as_mut().ok_or(NotChanged::Closed)?;
        let id = s.wm.id(window).ok_or(NotChanged::Closed)?;
        let w = s.wm.window(id).ok_or(NotChanged::Closed)?;
        if w.is_panel() {
            return Err(NotChanged::Taskbar);
        }
        let out = if to { s.wm.minimize(id) } else { s.wm.restore(id) };
        match out {
            Some(out) => {
                s.apply(out, &mut after);
                root_row(s, id).ok_or(NotChanged::Closed)
            }
            None => Err(NotChanged::Already),
        }
    };
    after.finish();
    wake();
    row
}

pub fn minimize_window(window: u32) -> Result<TreeRow, NotChanged> {
    set_minimized(window, true)
}

pub fn restore_window(window: u32) -> Result<TreeRow, NotChanged> {
    set_minimized(window, false)
}

/// Open a panel on the live desktop (see [`Desktop::open_panel`]).
pub fn open_panel(title: &str, rows: u32) -> Result<WindowHandle, OpenError> {
    Desktop::LIVE.open_panel(title, rows)
}

/// The live desktop's taskbar owner's [`Waker`], if it has a taskbar.
pub fn taskbar() -> Option<Waker> {
    COMPOSITOR.lock().as_ref().and_then(|s| s.taskbar.clone())
}

/// Open the taskbar if the desktop has none (each start: the first, and a
/// resume after a first that couldn't), warning if it can't.
fn ensure_taskbar() {
    if taskbar().is_some() {
        return;
    }
    if let Err(e) = crate::taskbar::open() {
        log::warn!(target: "desktop", "no taskbar: {e}");
    }
}

/// The windows (not panels) in the order they opened, for the taskbar:
/// id and title.
pub fn window_list() -> Vec<(u32, String)> {
    let g = COMPOSITOR.lock();
    let Some(s) = g.as_ref() else { return Vec::new() };
    let mut list: Vec<(u32, String)> = s.ordinary_windows().map(|w| (w.id().get(), String::from(w.title()))).collect();
    list.sort_unstable_by_key(|(id, _)| *id);
    list
}

/// Whether the desktop has started.
pub fn started() -> bool {
    COMPOSITOR.lock().as_ref().is_some_and(|s| s.started)
}

/// `ui tree [window]`'s rows: front to back, each window's root then its
/// published nodes, on the screen. `Err` without a desktop, or for a
/// window number that names none.
pub fn tree_rows(window: Option<u32>) -> Result<Vec<TreeRow>, String> {
    let g = COMPOSITOR.lock();
    let s = g.as_ref().filter(|s| s.started).ok_or_else(|| String::from("no desktop"))?;
    if let Some(n) = window
        && s.wm.id(n).is_none()
    {
        return Err(alloc::format!("no window {n}"));
    }
    let mut rows = Vec::new();
    for w in s.wm.windows().iter().rev() {
        if window.is_some_and(|n| n != w.id().get()) {
            continue;
        }
        let (Some(root), Some(win)) = (root_row(s, w.id()), s.window(w.id())) else { continue };
        let client = root.node.bounds();
        rows.push(root);
        for (depth, node) in &win.inner.lock().nodes {
            rows.push(TreeRow { window: w.id().get(), depth: *depth, node: node.translated(client.x(), client.y()) });
        }
    }
    Ok(rows)
}

/// Where node number `n` is (`None`: no node, or its window closed).
pub fn find_node(n: u64) -> Option<NodeAt> {
    let g = COMPOSITOR.lock();
    let s = g.as_ref()?;
    s.windows.iter().find_map(|win| {
        if win.root.get() == n {
            return Some(NodeAt::Root(win.id.get()));
        }
        let inner = win.inner.lock();
        inner.nodes.iter().any(|(_, node)| node.id().get() == n).then_some(NodeAt::Widget(win.id.get()))
    })
}

/// A row of window `window`'s node `node` (client coordinates) on the
/// screen, at depth `depth` (`None`: the window has closed since).
pub fn node_row(window: u32, depth: u32, node: AutomationNode) -> Option<TreeRow> {
    let g = COMPOSITOR.lock();
    let client = g.as_ref().and_then(|s| s.wm.id(window).and_then(|id| s.wm.client(id)))?;
    Some(TreeRow { window, depth, node: node.translated(client.x(), client.y()) })
}

/// `ui invoke <root> focus`: raise and focus window `window` (the
/// compositor answers it); its root's row after (`None`: no such window).
pub fn focus_window(window: u32) -> Option<TreeRow> {
    let mut after = After::default();
    let row = {
        let mut g = COMPOSITOR.lock();
        let s = g.as_mut()?;
        let id = s.wm.id(window)?;
        // Nothing returns early once the focus changed: its damage and
        // deliveries go out, and the compositor is woken, below.
        if let Some(out) = s.wm.focus(id) {
            s.apply(out, &mut after);
        }
        root_row(s, id)
    };
    after.finish();
    wake();
    row
}

/// `ui invoke <root> close`: ask window `window`'s owner to close it (its
/// `Close`), then wait up to `timeout_ns` for it to go; whether it went (if
/// not, its close stays queued: the owner reads it when it can).
pub fn close_window(window: u32, timeout_ns: u64) -> bool {
    let mut after = After::default();
    {
        let mut g = COMPOSITOR.lock();
        let Some(s) = g.as_mut() else { return true };
        let Some(id) = s.wm.id(window) else { return true };
        let _ = s.push(id, &mut after, |q| {
            q.request_close();
            None
        });
    }
    after.finish();
    wake();
    // (`DONE`'s waiter: its predicate takes `COMPOSITOR` under the wait
    // queue's lock, as `resume`'s does.)
    let gone = || COMPOSITOR.lock().as_ref().is_none_or(|s| s.wm.id(window).is_none());
    let deadline = crate::time::now_ns().saturating_add(timeout_ns);
    DONE.wait_until_or_deadline(deadline, gone) || gone()
}

/// Queue `ui invoke`'s request on the live desktop's window `window` (see
/// [`Desktop::push_request`]).
pub fn push_request(window: u32, req: Request) -> Result<(), lumen_wm::Refused<Request>> {
    Desktop::LIVE.push_request(window, req)
}

impl Desktop {
    /// Queue `ui invoke`'s request on window `window`, waking its owner; a
    /// refused one is handed back (busy, or the window gone: not on the
    /// list), to be dropped (answered) with no desktop lock held.
    pub fn push_request(&self, window: u32, req: Request) -> Result<(), lumen_wm::Refused<Request>> {
        let mut after = After::default();
        let result = 'push: {
            let mut g = self.home.lock();
            let Some(s) = g.as_mut() else { break 'push Err(lumen_wm::Refused::Dead(req)) };
            let Some(win) = s.wm.id(window).and_then(|id| s.window(id)).cloned() else {
                s.stats.dead_pushes += 1;
                break 'push Err(lumen_wm::Refused::Dead(req));
            };
            // One at a time: the slot taken here, held by the request.
            let req = if win.one_request {
                match crate::ui::RequestSlot::take(&win.request_out) {
                    Some(slot) => req.holding(slot),
                    None => break 'push Err(lumen_wm::Refused::Busy(req.slot_held())),
                }
            } else {
                req
            };
            let pushed = with_queue(&win, |q| q.push_request(req));
            if pushed.is_ok() {
                s.requests += 1;
                after.wake.push(win);
            }
            pushed
        };
        after.finish();
        result
    }
}

/// A request's asker is done with it (`settled` waits for none to be);
/// count it if it was cancelled at its deadline or answered late.
pub fn request_finished(cancelled: bool, late: bool) {
    if let Some(s) = COMPOSITOR.lock().as_mut() {
        debug_assert!(s.requests > 0, "a request finished that was never counted");
        s.requests = s.requests.saturating_sub(1);
        s.stats.cancelled += u64::from(cancelled);
        s.stats.late += u64::from(late);
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
        let handle = match Desktop::LIVE.open(title, w, h, Place::At(at.0, at.1), false, Requests::Many) {
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

/// A demo window's owner: drawn whole at each `Theme` (its first event,
/// then each switch) in [`WindowHandle::theme`], showing the last key typed
/// in it, until its close box is clicked.
fn demo_owner(mut h: WindowHandle, demo: Demo) {
    let mut text = Text::new().ok();
    let mut last: Option<(lumen_input::KeyCode, Option<char>)> = None;
    loop {
        match h.wait_event() {
            WindowEvent::Close => return,
            WindowEvent::Theme(_) => {
                let theme = h.theme();
                draw_demo(&mut h, demo, theme, text.as_mut(), last);
            }
            WindowEvent::Key(KeyEvent { code, action: KeyAction::Press { text: typed }, .. }) => {
                last = Some((code, typed));
                let theme = h.theme();
                let line = draw_last_key(&mut h, theme, text.as_mut(), last);
                h.commit(&[line]);
            }
            // No toolkit, no nodes: only its root takes actions.
            WindowEvent::Request(req) => crate::ui::refuse(req),
            _ => {}
        }
    }
}

/// A demo window drawn whole in `theme`, and committed.
fn draw_demo(
    h: &mut WindowHandle,
    demo: Demo,
    theme: &Theme,
    mut text: Option<&mut Text>,
    last: Option<(lumen_input::KeyCode, Option<char>)>,
) {
    let bounds = h.surface_mut().bounds();
    {
        let s = h.surface_mut();
        s.fill(theme.client);
        match demo {
            Demo::Welcome => {
                if let Some(t) = text.as_deref_mut() {
                    t.draw(s, 16, 12, WELCOME_HEADING, HEADING_PX, theme.client_text, bounds);
                    for (i, line) in WELCOME_LINES.iter().enumerate() {
                        t.draw(s, 16, 60 + 26 * i as i32, line, TEXT_PX, theme.client_text, bounds);
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
    draw_last_key(h, theme, text, last);
    h.commit(&[bounds]);
}

/// The "last key: ..." line of a demo window in `theme`; the rectangle
/// drawn.
fn draw_last_key(
    h: &mut WindowHandle,
    theme: &Theme,
    text: Option<&mut Text>,
    key: Option<(lumen_input::KeyCode, Option<char>)>,
) -> Rect {
    let s = h.surface_mut();
    let line = Rect::new(0, LAST_KEY_Y, s.width(), 24);
    s.fill_rect(line, theme.client);
    let mut said = String::from("last key: ");
    match key {
        None => said.push('-'),
        Some((_, Some(c))) if !c.is_control() => said.push(c),
        Some((code, _)) => {
            use core::fmt::Write;
            let _ = write!(said, "{code:?}");
        }
    }
    if let Some(t) = text {
        t.draw(s, 16, LAST_KEY_Y + 2, &said, TEXT_PX, theme.client_text, line);
    }
    line
}

/// `desktop spin`'s client: the bar's strip, then the Stop button's.
const SPIN_SIZE: (u32, u32) = (200, 100);
const SPIN_STRIP: u32 = 60;

/// `desktop spin`'s app: one button, Stop, which closes the window (a
/// request its queue can refuse as busy while `desktop hold` holds it),
/// under a pure green bar moving every frame.
struct Spin {
    ui: lumen_ui::Ui,
    stop: lumen_ui::Widget,
    x: i32,
}

impl lumen_ui::App for Spin {
    fn ui_mut(&mut self) -> &mut lumen_ui::Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[lumen_ui::UiEvent]) -> lumen_ui::Then {
        if events.contains(&lumen_ui::UiEvent::Pressed(self.stop)) {
            lumen_ui::Then::Close
        } else {
            lumen_ui::Then::Carry
        }
    }

    fn paint_over(&mut self, s: &mut Surface) {
        const GREEN: u32 = 0x00ff00;
        s.fill_rect(Rect::new(self.x, 20, 40, 20), GREEN);
    }
}

/// `desktop spin`'s window: a frame every 20 ms (the bar moved), its
/// events read meanwhile (not while `desktop hold` holds it).
fn spin_owner(mut h: WindowHandle, mut tk: crate::ui::Toolkit<Spin>) {
    let width = h.surface_mut().width() as i32;
    loop {
        tk.frame(&mut h);
        let app = tk.app_mut();
        app.x = (app.x + 8) % (width - 40).max(1);
        while !h.held()
            && let Some(e) = h.next_event()
        {
            if let crate::ui::Step::Close(closing) = tk.step(&mut h, e) {
                return crate::ui::close(h, closing);
            }
        }
        let _ = crate::sched::sleep(20_000_000);
    }
}

/// `desktop spin`: open its window and start its owner.
fn spin() -> Result<u32, String> {
    let (w, h) = SPIN_SIZE;
    let mut ui = lumen_ui::Ui::new();
    let stop = ui.button(None, "Stop");
    let area = Rect::new(0, SPIN_STRIP as i32, w, h - SPIN_STRIP);
    let handle = Desktop::LIVE
        .open("spin", w, h, Place::Auto, true, Requests::Many)
        .map_err(|e| alloc::format!("no window: {e}"))?;
    let tk = crate::ui::Toolkit::new(Spin { ui, stop, x: 0 }, area, handle.theme())?;
    let id = handle.id();
    let _detached = crate::sched::spawn("desktop-spin", move || spin_owner(handle, tk))
        .map_err(|e| alloc::format!("no thread: {e}"))?;
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

pub const USAGE: &str = "usage: desktop [status|check|spin|counter|tick|hold <window>|release <window>]";

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
    minimized: bool,
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
        // Not the panels (the taskbar is the desktop's, like the frames).
        s.ordinary_windows()
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
                    minimized: w.is_minimized(),
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
            yes(r.minimized),
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
    const COLUMNS: [Column; 29] = [
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
        Column::new("glyph_misses", Kind::Int, Align::Right, 12),
        Column::new("glyphs_dropped", Kind::Int, Align::Right, 14),
        Column::new("wallpaper", Kind::Text, Align::Left, 9),
        Column::new("taskbar", Kind::Text, Align::Left, 7),
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
        let mut row = alloc::vec![text("off"), text(theme().id.name())];
        row.extend((2..COLUMNS.len() - 1).map(|_| Value::Absent("-")));
        row.push(text("no"));
        t.push(row);
        return t;
    };
    let mut counts = s.closed;
    for w in &s.windows {
        counts += &w.inner.lock().queue.counts();
    }
    let (windows_idle, themes_told) = windows_quiet(s);
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
        requesting: s.requests > 0,
        themes_told,
    }
    .holds();
    let wm = s.wm.counts();
    let ms = s.stats.present_ns / 1_000_000;
    let us = (s.stats.present_ns / 1_000) % 1_000;
    t.push(alloc::vec![
        text(if shown { "shown" } else { "paused" }),
        text(theme().id.name()),
        int(s.ordinary_windows().count() as u64),
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
        s.stats.glyph_misses.map_or(Value::Absent("-"), int),
        int(crate::font::glyphs_dropped()),
        text(match &s.wallpaper {
            Ok(_) => "shown",
            Err(not) => not.word,
        }),
        yes(s.taskbar.is_some()),
        int(s.stats.cancelled),
        int(s.stats.late),
        yes(settled),
    ]);
    t
}

/// Whether every window's queue is idle with its owner waiting (not in an
/// animating wait), clear of work on another thread (no `Waker` busy bit,
/// no wake not taken yet: #155's terminal windows), and
/// whether every owner has been told of the last theme switch: in that
/// order for each window, the work first, the owner's idle bit last. An
/// owner told of a switch clears its idle bit before it takes the theme
/// (`wait_event` stores it before `next_event`), so a look that finds the
/// theme told and then the bit set saw the owner wait again after
/// redrawing, never mid-redraw.
fn windows_quiet(s: &State) -> (bool, bool) {
    let (mut idle, mut told) = (true, true);
    for w in &s.windows {
        let queue_idle = w.inner.lock().queue.is_idle();
        told &= !w.pending.theme_behind();
        let waiting = w.idle.load(Ordering::Acquire);
        // After the idle bit: whoever hands work to the window's other
        // thread (its owner giving a terminal's shell a key) sets busy
        // before the owner can idle again, so a look that finds the owner
        // idle finds that busy too; and a wake a thread made before the
        // owner idled is still set if the owner hasn't taken it.
        let clear = !w.busy.load(Ordering::Acquire) && !w.poked.load(Ordering::Acquire);
        idle &= queue_idle && waiting && clear;
    }
    (idle, told)
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
    /// Every window's queue is idle and its owner waits (blocked in
    /// `wait_event` or a `wait_event_until` that isn't animating), no
    /// other thread marks it busy and no wake waits to be taken (#155);
    /// read after `themes_told` for each window (`windows_quiet`).
    windows_idle: bool,
    /// A `ui invoke` request was pushed and its asker isn't done with it
    /// yet.
    requesting: bool,
    /// Every window's owner has been told of the last theme switch.
    themes_told: bool,
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
            && !self.requesting
            && self.themes_told
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
    // `check`, `spin`, `counter`, `hold` and `release` need the desktop
    // shown.
    if let [what, ..] = words[..]
        && matches!(what, "check" | "spin" | "counter" | "hold" | "release" | "tick")
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
        [what @ ("spin" | "counter")] => {
            let opened = if what == "spin" { spin() } else { crate::ui::open_counter() };
            match opened {
                Ok(id) => {
                    let _ = writeln!(ctx.out, "desktop {what}: window {id}");
                }
                Err(e) => ctx.fail(format_args!("desktop {what}: {e}")),
            }
        }
        ["tick"] => {
            if crate::taskbar::tick() {
                let _ = writeln!(ctx.out, "desktop tick: the clock moved a minute ahead");
            } else {
                ctx.fail("desktop tick: no taskbar");
            }
        }
        [what @ ("hold" | "release"), id] => match hold(id, what == "hold") {
            Ok(()) => {
                let _ = writeln!(ctx.out, "desktop {what}: window {id}");
            }
            Err(e) => ctx.fail(format_args!("desktop {what}: {e}")),
        },
        _ => ctx.fail(USAGE),
    }
}

/// Why `desktop check`, `spin`, `counter`, `hold`, `release` and
/// `terminal` (#155) refuse now, if they do.
pub(crate) fn refused_while_paused() -> Option<&'static str> {
    if !started() {
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
            requesting: false,
            themes_told: true,
        };
        kassert!(all.holds());
        let broken: [fn(&mut Settled); 14] = [
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
            |s| s.requesting = true,
            |s| s.themes_told = false,
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

crate::ktest!(
    fn wait_event_until_ends_at_its_deadline_or_a_wake() {
        // A terminal window's owner (#155) waits for events, its shell's
        // output (a `Waker`) and, focused, its cursor's next blink (a
        // deadline, animating): the deadline passing ends the wait as
        // `Deadline`; a wake from another thread ends it at once as `Woken`,
        // and so does one made before the wait; an event queued is returned.
        // A wait that isn't animating counts as idle while it lasts, an
        // animating one never does. A wake, not the timer, ended a Woken
        // wait (`sched::deadline_wakes_in`).
        use crate::kassert;
        let d = Desktop::without_screen();
        let mut h = d.open_window("w", 16, 16).map_err(|_| "a window")?;
        while h.next_event().is_some() {}
        // Waits until 5 s from now: what it returned, and whether the timer
        // ended it.
        let wait = |h: &mut WindowHandle, animating| {
            crate::sched::deadline_wakes_in(|| {
                h.wait_event_until(Some(crate::time::now_ns() + 5_000_000_000), animating)
            })
        };
        let start = crate::time::now_ns();
        let (got, timer_ended) =
            crate::sched::deadline_wakes_in(|| h.wait_event_until(Some(start + 20_000_000), Animating::No));
        kassert!(matches!(got, Wait::Deadline));
        kassert!(crate::time::now_ns() >= start + 20_000_000);
        // The timer ended that one, if it blocked (a stall past 20 ms before
        // it skips the block); that the count works is sched's
        // `a_lost_wake_is_ended_by_the_timer_and_counted`.
        kassert!(timer_ended <= 1);
        let waker = h.waker();
        waker.wake();
        kassert!(matches!(wait(&mut h, Animating::No), (Wait::Woken, 0)));
        for (animating, idles) in [(Animating::No, true), (Animating::Yes, false)] {
            let saw_idle = Arc::new(AtomicBool::new(!idles));
            let (w, seen, win) = (waker.clone(), saw_idle.clone(), h.win.clone());
            let t = crate::sched::spawn("ktest-waker", move || {
                // Not animating: once the wait is idle (up to 5 s; a wait
                // that never is fails). Animating: looked at 10 ms in, idle
                // must read false whether or not the wait has started.
                let deadline = crate::time::now_ns() + 5_000_000_000;
                if idles {
                    while !win.idle.load(Ordering::Acquire) && crate::time::now_ns() < deadline {
                        let _ = crate::sched::sleep(1_000_000);
                    }
                } else {
                    let _ = crate::sched::sleep(10_000_000);
                }
                seen.store(win.idle.load(Ordering::Acquire), Ordering::Release);
                w.wake();
            })
            .map_err(|_| "spawn failed")?;
            kassert!(matches!(wait(&mut h, animating), (Wait::Woken, 0)));
            crate::sched::join_bounded(t)?;
            kassert!(saw_idle.load(Ordering::Acquire) == idles);
            kassert!(!h.win.idle.load(Ordering::Acquire));
        }
        d.push_key(h.id(), test_key())?;
        let got = h.wait_event_until(Some(crate::time::now_ns() + 5_000_000_000), Animating::No);
        kassert!(matches!(got, Wait::Event(WindowEvent::Key(_))));
        kassert!(h.next_event().is_none());
        // Closed, the window is beyond its wakers: they do nothing.
        let win = Arc::downgrade(&h.win);
        drop(h);
        waker.wake();
        waker.set_busy(true);
        kassert!(win.upgrade().is_none());
        Ok(())
    }
);

crate::ktest!(
    fn closing_a_window_answers_its_queued_requests() {
        // A request still queued when its window closes comes back from the
        // queue and is answered "window closed" once the locks are released
        // (its asker would print "window <id> closed before it ran").
        use crate::kassert;
        use lumen_ui::Phase;
        let d = Desktop::without_screen();
        let w = d.open_window("w", 16, 16).map_err(|_| "a window")?;
        let (req, cell) = crate::ui::Request::new(1, lumen_ui::Action::Press);
        kassert!(d.push_request(w.id(), req).is_ok());
        kassert!(cell.phase() == Phase::Pending);
        drop(w);
        kassert!(matches!(cell.take(), Some(crate::ui::Answer::WindowClosed)));
        Ok(())
    }
);

crate::ktest!(
    fn settled_waits_for_a_busy_or_woken_window() {
        // A window whose owner idles counts as quiet only while no other
        // thread marks it busy and no wake waits to be taken (#155: a
        // terminal window's shell running a command, its output not drawn).
        use crate::kassert;
        let d = Desktop::without_screen();
        let mut h = d.open_window("w", 16, 16).map_err(|_| "a window")?;
        while h.next_event().is_some() {}
        let waker = h.waker();
        h.win.idle.store(true, Ordering::Release);
        let quiet = |d: &Desktop| d.home.lock().as_ref().map(|s| windows_quiet(s).0);
        kassert!(quiet(&d) == Some(true));
        waker.set_busy(true);
        kassert!(quiet(&d) == Some(false));
        waker.set_busy(false);
        waker.wake();
        kassert!(quiet(&d) == Some(false));
        h.win.poked.store(false, Ordering::Release);
        kassert!(quiet(&d) == Some(true));
        Ok(())
    }
);

crate::ktest!(
    fn a_request_for_a_closed_window_is_handed_back_unanswered() {
        // A window no longer on the list: the request comes back as dead,
        // still pending (never dropped under the desktop's lock), and the
        // miss is counted; dropped then, it answers "window closed".
        use crate::{kassert, kassert_eq};
        use lumen_ui::Phase;
        let d = Desktop::without_screen();
        let w = d.open_window("w", 16, 16).map_err(|_| "a window")?;
        let id = w.id();
        drop(w);
        let (req, cell) = crate::ui::Request::new(1, lumen_ui::Action::Press);
        let Err(lumen_wm::Refused::Dead(back)) = d.push_request(id, req) else {
            return Err("not handed back as dead");
        };
        kassert!(cell.phase() == Phase::Pending);
        kassert_eq!(d.home.lock().as_ref().map(|s| s.stats.dead_pushes), Some(1));
        drop(back);
        kassert!(cell.phase() == Phase::Done);
        Ok(())
    }
);

#[cfg(feature = "ktest")]
impl Desktop {
    /// Click window `id`'s close box (its `Close` queued, its owner woken),
    /// for the ktests of owners on a desktop with no screen.
    pub fn request_close(&self, id: u32) -> Result<(), &'static str> {
        self.push_entry(id, |q| {
            q.request_close();
            None
        })
    }

    /// Queue key event `k` for window `id`, as the compositor delivers one
    /// (for the ktests).
    pub fn push_key(&self, id: u32, k: KeyEvent) -> Result<(), &'static str> {
        self.push_entry(id, |q| Some(q.push(Entry::Key(k))))
    }

    fn push_entry(&self, id: u32, f: impl FnOnce(&mut Queue) -> Option<Pushed>) -> Result<(), &'static str> {
        let mut after = After::default();
        {
            let mut g = self.home.lock();
            let s = g.as_mut().ok_or("the state")?;
            let wid = s.wm.id(id).ok_or("no such window")?;
            let _ = s.push(wid, &mut after, f);
        }
        after.finish();
        Ok(())
    }
}

/// How many lines of the kernel log (as much as its ring holds) contain
/// `what`: compared before and after, to tell a warning was logged.
#[cfg(feature = "ktest")]
pub(crate) fn logged(what: &str) -> Result<usize, &'static str> {
    let mut log = alloc::vec![0u8; crate::klog::RING_BYTES];
    let (n, _) = crate::klog::copy_recent(&mut log);
    let log = core::str::from_utf8(&log[..n]).map_err(|_| "the log isn't UTF-8")?;
    Ok(log.lines().filter(|l| l.contains(what)).count())
}

crate::ktest!(
    fn a_bad_wallpaper_falls_back_with_one_warning() {
        // On a desktop with no screen: a wallpaper that doesn't decode (not
        // a PNG, cut short) leaves the theme's flat colour, one warning
        // each, and a recompose draws that colour at every pixel of the
        // rectangle; the embedded one decodes (1280x800), with no warning,
        // and a recompose draws its pixels. A desktop not started
        // says so, with no warning.
        use crate::{kassert, kassert_eq};
        const SAID: &str = "WARN  desktop: wallpaper not shown: ";
        let d = Desktop::without_screen();
        let before = logged(SAID)?;
        kassert!(d.set_wallpaper(b"not a PNG at all").is_err());
        kassert_eq!(logged(SAID)?, before + 1);
        kassert!(d.set_wallpaper(&WALLPAPER_PNG[..WALLPAPER_PNG.len() / 2]).is_err());
        kassert_eq!(logged(SAID)?, before + 2);
        let mut titles = None;
        let mut target = Surface::with_stride(1280, 800, 1280).ok_or("no memory for a screen")?;
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        kassert!(scene.wallpaper.is_none());
        let r = Rect::new(100, 100, 64, 32);
        compose(&mut target, r, &scene, &mut titles);
        let flat =
            (r.y()..r.bottom()).all(|y| target.row(y as u32)[100..164].iter().all(|&p| p == scene.theme.desktop));
        kassert!(flat);
        kassert!(d.set_wallpaper(WALLPAPER_PNG).is_ok());
        kassert_eq!(logged(SAID)?, before + 2);
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let wp = scene.wallpaper.clone().ok_or("the embedded wallpaper isn't shown")?;
        kassert_eq!((wp.width(), wp.height()), (1280, 800));
        compose(&mut target, r, &scene, &mut titles);
        let same = (r.y()..r.bottom()).all(|y| target.row(y as u32)[100..164] == wp.row(y as u32)[100..164]);
        kassert!(same);
        let unstarted = Desktop { home: Home::Own(Arc::new(SpinLock::new(None))) };
        kassert_eq!(unstarted.set_wallpaper(WALLPAPER_PNG), Err(String::from("no desktop")));
        kassert_eq!(logged(SAID)?, before + 2);
        Ok(())
    }
);

crate::ktest!(
    fn a_smaller_wallpaper_is_drawn_centred_over_the_flat_colour() {
        // A 64x32 wallpaper (made with lumen-png's encoder) on a 1280x800
        // desktop: centred, the theme's colour around it.
        use crate::{kassert, kassert_eq};
        let mut rows = Rows::new(64, 32).map_err(|_| "no rows")?;
        for y in 0..32 {
            for (x, p) in rows.row_mut(y).iter_mut().enumerate() {
                *p = [x as u8 * 4, y as u8 * 8, 0x80];
            }
        }
        let png = rows.encode().map_err(|_| "no PNG")?;
        let d = Desktop::without_screen();
        kassert!(d.set_wallpaper(png.bytes()).is_ok());
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let mut target = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        let all = target.bounds();
        compose(&mut target, all, &scene, &mut None);
        let (x0, y0) = ((1280 - 64) / 2, (800 - 32) / 2);
        kassert_eq!(target.pixel(0, 0), Some(scene.theme.desktop));
        kassert_eq!(target.pixel(x0 - 1, y0), Some(scene.theme.desktop));
        kassert_eq!(target.pixel(x0, y0), Some(0x0000_0080));
        kassert_eq!(target.pixel(x0 + 10, y0 + 3), Some((40 << 16) | (24 << 8) | 0x80));
        kassert_eq!(target.pixel(x0 + 63, y0 + 31), Some((252 << 16) | (248 << 8) | 0x80));
        kassert_eq!(target.pixel(x0 + 64, y0 + 31), Some(scene.theme.desktop));
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
        let mut titles = Some(Titles::new().ok_or("no UI font")?);
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let frame = scene.windows.first().ok_or("no window in the scene")?.frame;
        let stats = |t: &Option<Titles>| t.as_ref().map(|t| t.text.stats()).ok_or("no font");
        let mut first = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        compose(&mut first, frame, &scene, &mut titles);
        let after_first = stats(&titles)?;
        kassert!(after_first.misses > 0);
        let mut second = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        compose(&mut second, frame, &scene, &mut titles);
        let after_second = stats(&titles)?;
        kassert_eq!(after_second.misses, after_first.misses);
        kassert!(after_second.hits > after_first.hits);
        kassert!((0..800).all(|y| first.row(y) == second.row(y)));
        Ok(())
    }
);

/// A key event for the ktests: `a` pressed.
#[cfg(feature = "ktest")]
fn key_a() -> KeyEvent {
    KeyEvent {
        code: lumen_input::KeyCode::A,
        action: KeyAction::Press { text: Some('a') },
        mods: lumen_input::Mods::NONE,
        after_drop: false,
    }
}

/// Run `f`, then switch the theme back to light (the theme is the
/// kernel's, not a test desktop's).
#[cfg(feature = "ktest")]
fn then_light(f: impl FnOnce() -> Result<(), &'static str>) -> Result<(), &'static str> {
    let result = f();
    let _ = switch_theme(&Home::Live, ThemeId::Light);
    result
}

crate::ktest!(
    fn a_window_s_first_event_is_the_theme_to_draw_with() {
        // Every window is told the theme first (even one opened with the
        // dark theme already on), so its owner draws with the theme it is
        // handed; after that, only a switch tells it again.
        use crate::kassert;
        then_light(|| {
            let d = Desktop::without_screen();
            let mut light = d.open_window("light", 16, 16).map_err(|_| "no window")?;
            kassert!(matches!(light.next_event(), Some(WindowEvent::Theme(t)) if t.id == ThemeId::Light));
            kassert!(light.next_event().is_none());
            kassert!(switch_theme(&d.home, ThemeId::Dark));
            let mut dark = d.open_window("dark", 16, 16).map_err(|_| "no window")?;
            kassert!(matches!(dark.next_event(), Some(WindowEvent::Theme(t)) if t.id == ThemeId::Dark));
            kassert!(dark.next_event().is_none());
            kassert!(matches!(light.next_event(), Some(WindowEvent::Theme(t)) if t.id == ThemeId::Dark));
            Ok(())
        })
    }
);

crate::ktest!(
    fn a_theme_switch_repaints_everything_and_tells_each_window() {
        // On a desktop with no screen: switching to dark damages the whole
        // screen and tells the window's owner once, before the key queued
        // earlier (`Theme(dark)`, then the key); a switch to the theme
        // already shown changes nothing.
        use crate::{kassert, kassert_eq};
        then_light(|| {
            let d = Desktop::without_screen();
            let mut w = d.open_window("w", 16, 16).map_err(|_| "no window")?;
            while w.next_event().is_some() {}
            if let Some(s) = d.home.lock().as_mut() {
                let _ = s.damage.take();
                let mut after = After::default();
                let id = w.win.id;
                kassert_eq!(s.push(id, &mut after, |q| Some(q.push(Entry::Key(key_a())))), Some(Pushed::Queued));
            }
            kassert!(switch_theme(&d.home, ThemeId::Dark));
            kassert_eq!(theme().id, ThemeId::Dark);
            let full = d.home.lock().as_mut().map(|s| s.damage.take()).ok_or("no state")?;
            kassert!(full.rects().iter().any(|r| *r == Rect::new(0, 0, 1280, 800)));
            kassert!(w.win.pending.theme_behind() && w.win.pending.has_event());
            kassert!(matches!(w.next_event(), Some(WindowEvent::Theme(t)) if t.id == ThemeId::Dark));
            kassert!(!w.win.pending.theme_behind());
            kassert!(matches!(w.next_event(), Some(WindowEvent::Key(k)) if k == key_a()));
            kassert!(w.next_event().is_none() && !w.win.pending.has_event());
            kassert!(!switch_theme(&d.home, ThemeId::Dark));
            kassert!(d.home.lock().as_ref().is_some_and(|s| s.damage.is_empty()));
            Ok(())
        })
    }
);

crate::ktest!(
    fn switches_before_the_owner_reads_end_with_the_current_theme() {
        // Dark then light before the owner reads: it is told once more, and
        // the last `Theme` it gets carries light; nothing follows.
        use crate::kassert;
        then_light(|| {
            let d = Desktop::without_screen();
            let mut w = d.open_window("w", 16, 16).map_err(|_| "no window")?;
            while w.next_event().is_some() {}
            kassert!(switch_theme(&d.home, ThemeId::Dark));
            kassert!(switch_theme(&d.home, ThemeId::Light));
            let mut last = None;
            while let Some(e) = w.next_event() {
                let WindowEvent::Theme(t) = e else { return Err("not a theme") };
                last = Some(t.id);
            }
            kassert!(last == Some(ThemeId::Light));
            Ok(())
        })
    }
);

crate::ktest!(
    fn settled_waits_for_every_owner_to_be_told_of_a_switch() {
        // An owner waiting (its idle bit set) with nothing queued: quiet. A
        // switch makes it not quiet (not told) until it reads the theme.
        use crate::kassert;
        then_light(|| {
            let d = Desktop::without_screen();
            let mut w = d.open_window("w", 16, 16).map_err(|_| "no window")?;
            while w.next_event().is_some() {}
            w.win.idle.store(true, Ordering::Release);
            let quiet = || d.home.lock().as_ref().map(windows_quiet).ok_or("no state");
            kassert!(quiet()? == (true, true));
            kassert!(switch_theme(&d.home, ThemeId::Dark));
            kassert!(quiet()? == (true, false));
            kassert!(matches!(w.next_event(), Some(WindowEvent::Theme(_))));
            kassert!(quiet()? == (true, true));
            Ok(())
        })
    }
);

crate::ktest!(
    fn a_wallpaper_is_decoded_and_warned_about_once() {
        // A bad file through a cache: decoded and warned about once, the
        // same answer twice (and the reason kept: `unreadable`). The
        // embedded wallpaper the same way: the same surface twice, said at
        // INFO once.
        use crate::{kassert, kassert_eq};
        const WARNED: &str = "WARN  desktop: wallpaper not shown: ";
        const SAID: &str = "INFO  desktop: wallpaper: ";
        let cache: WallpaperCache = SpinLock::new(None);
        let before = logged(WARNED)?;
        let first = cached_wallpaper(&cache, b"not a PNG");
        let again = cached_wallpaper(&cache, b"not a PNG");
        kassert_eq!(logged(WARNED)?, before + 1);
        let (Err(a), Err(b)) = (&first, &again) else { return Err("a bad file shown") };
        kassert!(Arc::ptr_eq(a, b) && a.word == "unreadable");
        let said = logged(SAID)?;
        let one = boot_wallpaper().map_err(|_| "the embedded wallpaper isn't shown")?;
        let two = boot_wallpaper().map_err(|_| "the embedded wallpaper isn't shown")?;
        kassert!(Arc::ptr_eq(&one, &two));
        kassert!(logged(SAID)? <= said + 1);
        Ok(())
    }
);

crate::ktest!(
    fn a_wallpaper_bigger_than_the_screen_is_cut_centred() {
        // 1290x810 on a 1280x800 desktop: taken (the bound is 2048, not
        // the screen), drawn centred and cut: the screen's corner is the
        // wallpaper's (5, 5), its last pixel the wallpaper's (1284, 804).
        use crate::{kassert, kassert_eq};
        let mut rows = Rows::new(1290, 810).map_err(|_| "no rows")?;
        for y in 0..810 {
            for (x, p) in rows.row_mut(y).iter_mut().enumerate() {
                *p = [x as u8, y as u8, (x / 256 + y as usize / 256 * 8) as u8];
            }
        }
        let png = rows.encode().map_err(|_| "no PNG")?;
        let d = Desktop::without_screen();
        kassert!(d.set_wallpaper(png.bytes()).is_ok());
        let scene = d.home.lock().as_ref().map(snapshot).ok_or("no state")?;
        let wp = scene.wallpaper.clone().ok_or("not shown")?;
        kassert_eq!((wp.width(), wp.height()), (1290, 810));
        let mut target = Surface::with_stride(1280, 800, 1280).ok_or("no memory")?;
        let all = target.bounds();
        compose(&mut target, all, &scene, &mut None);
        kassert_eq!(target.pixel(0, 0), wp.pixel(5, 5));
        kassert_eq!(target.pixel(1279, 799), wp.pixel(1284, 804));
        kassert_eq!(target.pixel(600, 300), wp.pixel(605, 305));
        Ok(())
    }
);

crate::ktest!(
    fn an_owner_blocked_for_an_event_wakes_for_a_switch() {
        // An owner thread blocked in `wait_event` with nothing queued: a
        // switch wakes it, and what it gets is `Theme(dark)`.
        use crate::kassert;
        use core::sync::atomic::AtomicU8;
        then_light(|| {
            let d = Desktop::without_screen();
            let mut w = d.open_window("blocked", 16, 16).map_err(|_| "no window")?;
            while w.next_event().is_some() {}
            let win = w.win.clone();
            let got = Arc::new(AtomicU8::new(u8::MAX));
            let seen = got.clone();
            let owner = crate::sched::spawn("blocked-owner", move || {
                if let WindowEvent::Theme(t) = w.wait_event() {
                    seen.store(t.id.index(), Ordering::Release);
                }
            })
            .map_err(|_| "no thread")?;
            let deadline = crate::time::now_ns() + 5_000_000_000;
            while !win.idle.load(Ordering::Acquire) {
                kassert!(crate::time::now_ns() < deadline);
                crate::sched::yield_now();
            }
            kassert!(switch_theme(&d.home, ThemeId::Dark));
            owner.join();
            kassert!(got.load(Ordering::Acquire) == ThemeId::Dark.index());
            Ok(())
        })
    }
);
