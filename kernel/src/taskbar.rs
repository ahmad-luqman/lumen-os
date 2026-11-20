//! The desktop shell's taskbar (#157): a panel along the bottom of the
//! screen (`Desktop::open_panel`: frameless, in front, never focused),
//! built with `lumen-ui` so its buttons are in the automation tree.
//!
//! Left to right: the `Launcher` menu (its items open a terminal, the
//! Mandelbrot and Life windows or Counter, then start each bundled app
//! whose manifest grants `desktop`, by name: #206), a button per window in the
//! order they opened (a press restores it if minimized, then raises and
//! focuses it), and at the right end the `Clock`, a fixed-size label
//! showing the RTC's `HH:MM`.
//!
//! Its owner, the `taskbar` thread, waits up to a second at a time
//! (non-animating: the window settles between polls), polls the RTC each
//! second and redraws the clock when the minute changes (the toolkit's
//! per-widget damage: only the label's bounds are committed). The desktop
//! pokes it (its `Waker`) when a window opens or closes, and it brings its
//! buttons in line with the window list. Launching from a menu item brings
//! them in line at once, before the request that chose it is answered, so
//! `ui tree` after `ui invoke` already shows the new window's button.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use lumen_ui::{App, Stack, Text, Then, Ui, UiEvent, Widget};

use crate::desktop::{self, Animating, Wait, WindowEvent, WindowHandle};
use crate::ui::{Step, Toolkit};

/// Pixels between the taskbar's edges and its widgets.
const PAD: u32 = 4;
/// What the launcher opens first, in its order; the bundled desktop apps
/// follow ([`items`]).
const ITEMS: [&str; 4] = ["Terminal", "Mandelbrot", "Life", "Counter"];

/// The launcher's items: [`ITEMS`], then the bundled apps whose manifest
/// grants `desktop` (never the test apps), by name.
fn items() -> Vec<&'static str> {
    ITEMS.into_iter().chain(crate::app::desktop_apps()).collect()
}
/// How often the clock polls the RTC.
const POLL_NS: u64 = 1_000_000_000;

/// Minutes added to the clock's readings (`desktop tick`, for the tests).
static SKEW_MINUTES: AtomicU64 = AtomicU64::new(0);

/// Failed reads in a row after which the clock shows `--:--` (fewer keep
/// the last good reading).
const CLOCK_FAILURES: u32 = 3;
/// The most `lumen.taskbar_wake_delay_ms=` waits.
const WAKE_DELAY_MAX_MS: u64 = 5000;

/// `desktop tick`: move the clock ahead by a minute from now on, and wake
/// the taskbar to show it; whether there is a taskbar (else nothing moves).
pub fn tick() -> bool {
    let Some(taskbar) = desktop::taskbar() else { return false };
    SKEW_MINUTES.fetch_add(1, Ordering::Relaxed);
    taskbar.wake();
    true
}

/// `lumen.taskbar_wake_delay_ms=<ms>`, for the scenarios (0 without it):
/// how long the taskbar waits after a poke before it brings its buttons in
/// line, so a test can tell the launcher's own sync from the poke's.
fn wake_delay_ms() -> u64 {
    const KEY: &str = "lumen.taskbar_wake_delay_ms";
    let ms = crate::cmdline::millis(KEY, WAKE_DELAY_MAX_MS);
    if ms > 0 {
        log::info!(target: "taskbar", "waiting {ms} ms after each poke ({KEY})");
    }
    ms
}

/// The clock's readings: what it shows on a failed one.
#[derive(Default)]
struct Clock {
    /// The last good reading's text.
    last: Option<String>,
    /// Failed reads in a row.
    failures: u32,
}

impl Clock {
    /// The text for a reading: the time, or after a failure the last good
    /// one, `--:--` with none or after [`CLOCK_FAILURES`] in a row. Reads
    /// that start failing are warned about once, a recovery said once.
    fn text(&mut self, reading: Result<lumen_time::rtc::DateTime, crate::rtc::RtcError>) -> String {
        match reading {
            Ok(t) => {
                if self.failures > 0 {
                    log::info!(target: "taskbar", "the clock reads the RTC again");
                }
                self.failures = 0;
                let minutes = u64::from(t.hour()) * 60 + u64::from(t.minute()) + SKEW_MINUTES.load(Ordering::Relaxed);
                let minutes = minutes % (24 * 60);
                let text = format!("{:02}:{:02}", minutes / 60, minutes % 60);
                self.last = Some(text.clone());
                text
            }
            Err(e) => {
                if self.failures == 0 {
                    match self.last {
                        Some(_) => log::warn!(
                            target: "taskbar",
                            "the clock can't read the RTC ({e}); it keeps its last reading for now"
                        ),
                        None => log::warn!(target: "taskbar", "the clock can't read the RTC ({e}); it shows --:--"),
                    }
                }
                self.failures = self.failures.saturating_add(1);
                match &self.last {
                    Some(last) if self.failures < CLOCK_FAILURES => last.clone(),
                    _ => String::from("--:--"),
                }
            }
        }
    }
}

/// The taskbar's tree and what its widgets stand for.
struct Taskbar {
    ui: Ui,
    launcher: Widget,
    /// The launcher's items ([`items`]), in its order.
    items: Vec<&'static str>,
    windows: Stack,
    /// A button per window: its id and the button, in the order opened.
    buttons: Vec<(u32, Widget)>,
    clock: Widget,
    readings: Clock,
}

impl Taskbar {
    fn new(clock_width: u32) -> Taskbar {
        let mut ui = Ui::bar(PAD);
        let items = items();
        let launcher = ui.menu(None, "Launcher", &items);
        let windows = ui.hstack(None, "Windows");
        let clock = ui.fixed_label(None, "Clock", "--:--", clock_width);
        let pinned = ui.pin_right(clock);
        debug_assert!(pinned, "the clock is a top widget of a bar");
        Taskbar { ui, launcher, items, windows, buttons: Vec::new(), clock, readings: Clock::default() }
    }

    /// Bring the buttons in line with the window list: a button for a
    /// window gone removed, one for a new window added (at the end: ids
    /// grow in opening order).
    fn sync(&mut self) {
        let list = desktop::window_list();
        let ui = &mut self.ui;
        self.buttons.retain(|&(id, w)| {
            let open = list.iter().any(|(l, _)| *l == id);
            if !open {
                ui.remove(w);
            }
            open
        });
        for (id, title) in list {
            if !self.buttons.iter().any(|&(b, _)| b == id) {
                let w = self.ui.button(Some(self.windows), &title);
                self.buttons.push((id, w));
            }
        }
    }

    /// Show the RTC's time (skewed by `desktop tick`); a change of minute
    /// damages only the clock.
    fn poll_clock(&mut self) {
        let text = self.readings.text(crate::rtc::now_quiet());
        self.ui.set_text(self.clock, &text);
    }

    /// Open what launcher item `item` names: what it opened (`window 3`,
    /// or an app started through `run`'s path, `app 2`, its window still to
    /// come), or why not (each refused while the desktop is paused, as
    /// `terminal` is).
    fn launch(&self, item: usize) -> Result<String, String> {
        if let Some(why) = desktop::refused_while_paused() {
            return Err(String::from(why));
        }
        let Some(&name) = self.items.get(item) else { return Err(String::from("no such item")) };
        let window = |id: Result<u32, String>| id.map(|id| format!("window {id}"));
        // An app's name is lower case (kernel/build.rs): never a demo's.
        match name {
            "Terminal" => window(crate::termwin::open()),
            "Mandelbrot" => window(crate::demo::open_mandelbrot_window()),
            "Life" => window(crate::demo::open_life_window()),
            "Counter" => window(crate::ui::open_counter()),
            app => crate::app::start(app).map(|id| format!("app {id}")),
        }
    }
}

impl App for Taskbar {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            match *e {
                UiEvent::Chosen(w, item) if w == self.launcher => {
                    let name = self.items.get(item).copied().unwrap_or("?");
                    let opened = self.launch(item);
                    // Its button now, before the request is answered.
                    self.sync();
                    match opened {
                        Ok(what) => log::info!(target: "taskbar", "{name}: {what}"),
                        Err(e) => {
                            log::warn!(target: "taskbar", "{name}: {e}");
                            return Then::Failed(format!("{name}: {e}"));
                        }
                    }
                }
                UiEvent::Pressed(w) => {
                    if let Some(&(id, _)) = self.buttons.iter().find(|&&(_, b)| b == w) {
                        // Restores it if minimized (`Wm::focus`). A window
                        // closed meanwhile: its button goes now, before the
                        // press is answered.
                        if desktop::focus_window(id).is_none() {
                            log::debug!(target: "taskbar", "window {id} closed before its button's press");
                            self.sync();
                            return Then::Failed(format!("window {id} closed"));
                        }
                    }
                }
                _ => {}
            }
        }
        // The taskbar never closes.
        Then::Carry
    }
}

/// Open the taskbar along the bottom of the live desktop and start its
/// owner (`desktop::ensure_taskbar`, at each `desktop` start that finds
/// none: the first, after its demo windows).
pub fn open() -> Result<u32, String> {
    let mut text = crate::font::Text::new().map_err(|e| format!("no font: {e}"))?;
    // As high as a button with `PAD` above and below; the clock as wide as
    // its widest time.
    let height = lumen_ui::ui::button_height(Text::line_height(&text)) + 2 * PAD;
    let clock_width = (0..10).map(|d| Text::width(&mut text, &format!("{d}{d}:{d}{d}"))).max().unwrap_or(0) + 4;
    let mut handle = desktop::open_panel("Taskbar", height).map_err(|e| format!("no window: {e}"))?;
    let area = handle.surface_mut().bounds();
    let tk = Toolkit::with_palette(Taskbar::new(clock_width), area, handle.theme(), crate::ui::taskbar_palette)?;
    let id = handle.id();
    let _detached = crate::sched::spawn("taskbar", move || owner(handle, tk)).map_err(|e| format!("no thread: {e}"))?;
    Ok(id)
}

/// The taskbar's owner: events through the toolkit; the buttons brought in
/// line when poked; the clock polled every second.
fn owner(mut h: WindowHandle, mut tk: Toolkit<Taskbar>) {
    let wake_delay_ms = wake_delay_ms();
    tk.app_mut().sync();
    tk.app_mut().poll_clock();
    let mut next_poll = crate::time::now_ns().saturating_add(POLL_NS);
    loop {
        match h.wait_event_until(Some(next_poll), Animating::No) {
            // A panel has no close box, and its root takes no `close`.
            Wait::Event(WindowEvent::Close) => {}
            Wait::Event(e) => {
                if let Step::Close(closing) = tk.step(&mut h, e) {
                    // `apply` never closes it, and `Close` is taken above.
                    debug_assert!(false, "the taskbar asked to close");
                    drop(closing);
                }
            }
            Wait::Woken => {
                if wake_delay_ms > 0 {
                    let _ = crate::sched::sleep(wake_delay_ms * 1_000_000);
                }
                tk.app_mut().sync();
                tk.app_mut().poll_clock();
                tk.refresh(&mut h);
            }
            Wait::Deadline => {}
        }
        let now = crate::time::now_ns();
        if now >= next_poll {
            tk.app_mut().poll_clock();
            tk.refresh(&mut h);
            next_poll = now.saturating_add(POLL_NS);
        }
    }
}
