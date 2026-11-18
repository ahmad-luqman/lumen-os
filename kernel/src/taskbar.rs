//! The desktop shell's taskbar (#157): a panel along the bottom of the
//! screen (`Desktop::open_panel`: frameless, in front, never focused),
//! built with `lumen-ui` so its buttons are in the automation tree.
//!
//! Left to right: the `Launcher` menu (its items open a terminal, the
//! Mandelbrot and Life windows, or Counter), a button per window in the
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

use lumen_gfx::Rect;
use lumen_ui::{App, Stack, Text, Then, Ui, UiEvent, Widget};

use crate::desktop::{self, Animating, Wait, WindowEvent, WindowHandle};
use crate::ui::{Step, Toolkit};

/// Pixels between the taskbar's edges and its widgets.
const PAD: u32 = 4;
/// What the launcher opens, in its order.
const ITEMS: [&str; 4] = ["Terminal", "Mandelbrot", "Life", "Counter"];
/// How often the clock polls the RTC.
const POLL_NS: u64 = 1_000_000_000;

/// Minutes added to the clock's readings (`desktop tick`, for the tests).
static SKEW_MINUTES: AtomicU64 = AtomicU64::new(0);

/// `desktop tick`: move the clock ahead by a minute from now on, and wake
/// the taskbar to show it.
pub fn tick() {
    SKEW_MINUTES.fetch_add(1, Ordering::Relaxed);
    desktop::wake_taskbar();
}

/// The taskbar's tree and what its widgets stand for.
struct Taskbar {
    ui: Ui,
    launcher: Widget,
    windows: Stack,
    /// A button per window: its id and the button, in the order opened.
    buttons: Vec<(u32, Widget)>,
    clock: Widget,
}

impl Taskbar {
    fn new(clock_width: u32) -> Taskbar {
        let mut ui = Ui::bar(PAD);
        let launcher = ui.menu(None, "Launcher", &ITEMS);
        let windows = ui.hstack(None, "Windows");
        let clock = ui.fixed_label(None, "Clock", "--:--", clock_width);
        ui.pin_right(clock);
        Taskbar { ui, launcher, windows, buttons: Vec::new(), clock }
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
        let text = match crate::rtc::now() {
            Ok(t) => {
                let minutes = u64::from(t.hour()) * 60 + u64::from(t.minute()) + SKEW_MINUTES.load(Ordering::Relaxed);
                let minutes = minutes % (24 * 60);
                format!("{:02}:{:02}", minutes / 60, minutes % 60)
            }
            Err(_) => String::from("--:--"),
        };
        self.ui.set_text(self.clock, &text);
    }

    /// Open what launcher item `item` names; a failure is logged.
    fn launch(item: usize) {
        let name = ITEMS.get(item).copied().unwrap_or("?");
        let opened = match item {
            0 => crate::termwin::open(),
            1 => crate::demo::open_mandelbrot_window(),
            2 => crate::demo::open_life_window(),
            3 => crate::ui::open_counter(),
            _ => Err(String::from("no such item")),
        };
        match opened {
            Ok(id) => log::info!(target: "taskbar", "{name}: window {id}"),
            Err(e) => log::warn!(target: "taskbar", "{name}: {e}"),
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
                    Taskbar::launch(item);
                    // Its button now, before the request is answered.
                    self.sync();
                }
                UiEvent::Pressed(w) => {
                    if let Some(&(id, _)) = self.buttons.iter().find(|&&(_, b)| b == w) {
                        // Restores it if minimized (`Wm::focus`); a window
                        // closed meanwhile does nothing.
                        let _ = desktop::focus_window(id);
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
/// owner (the desktop's first start, after the demo windows).
pub fn open() -> Result<u32, String> {
    let (sw, sh) = desktop::screen_size().ok_or("no desktop")?;
    let mut text = crate::font::Text::new().map_err(|e| format!("no font: {e}"))?;
    // As high as a button with `PAD` above and below; the clock as wide as
    // its widest time.
    let height = lumen_ui::ui::button_height(Text::line_height(&text)) + 2 * PAD;
    let clock_width = (0..10).map(|d| Text::width(&mut text, &format!("{d}{d}:{d}{d}"))).max().unwrap_or(0) + 4;
    let frame = Rect::new(0, sh as i32 - height as i32, sw, height);
    let handle = desktop::open_panel("Taskbar", frame).map_err(|e| format!("no window: {e}"))?;
    let area = Rect::new(0, 0, sw, height);
    let tk = Toolkit::with_palette(Taskbar::new(clock_width), area, handle.theme(), crate::ui::taskbar_palette)?;
    let id = handle.id();
    let _detached = crate::sched::spawn("taskbar", move || owner(handle, tk)).map_err(|e| format!("no thread: {e}"))?;
    Ok(id)
}

/// The taskbar's owner: events through the toolkit; the buttons brought in
/// line when poked; the clock polled every second.
fn owner(mut h: WindowHandle, mut tk: Toolkit<Taskbar>) {
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
