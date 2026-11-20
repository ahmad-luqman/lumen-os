//! The image viewer (#204): the images bundled with it (the desktop's
//! wallpaper, two small test images, and a corrupt one), decoded inside the
//! app by `lumen-png`, so a bad file can only end the app, never the kernel
//! (M6 brings files; until then these are its images).
//!
//! - **The window:** a panel of widgets on the left (the `Images` list,
//!   `Previous` and `Next`, the `Fit` checkbox, the `Info` label: the
//!   image's name and size, `decoding <name>`, or `<name>: <why>` for one
//!   it couldn't show) and the view, lumen-view's `VIEW`, on the right.
//! - **Showing an image:** choosing one (in the list, through Previous or
//!   Next, or `run viewer <name>` for the first) drops the one shown, says
//!   `decoding <name>` and answers at once; the decoding is the next tick's
//!   (`Ticking::period` is set only while one waits), so a request is never
//!   held up by it. `lumen_view::admit` refuses, from its header, an image
//!   bigger or costlier to decode than the memory plan covers; the decoder
//!   refuses a bad one. Either is shown in the view and the label, logged
//!   at INFO, and the app goes on.
//! - **1:1 or fitted:** `Fit` checked (the start) shows the image fitted
//!   to the view (`lumen_view::fitted`, a copy scaled once, by nearest
//!   neighbour); unchecked, at 1:1 (`lumen_view::actual`), the arrow keys
//!   panning a bigger image (`Pan`).
//! - **Drawing:** the view is the app's own (`with_area`: the tree is laid
//!   out in the panel only); a change to it is one rectangle of damage
//!   (`take_drawn`), drawn in `paint_over` only where the repainted
//!   rectangles reach it, so a commit pays only for what changed.
//! - **Memory:** fixed at `lumen_view::MEMORY_BYTES` (its `build.rs`),
//!   checked at the start (with where the heap starts, against
//!   lumen-view's plan): `tests/memory.rs` there checks the app's
//!   allocations fit.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode, Time, log};
use lumen_png::decode::{Image, decode};
use lumen_ui::{Action, App, Over, Then, Ui, UiEvent, Widget};
use lumen_view::{
    Area, MAX_IMAGE_H, MAX_IMAGE_W, PANEL_W, Pan, VIEW, WINDOW_H, WINDOW_W, actual, admit, fitted, scale,
};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// An image bundled with the app: its name (the list's item, `run viewer
/// <name>`) and its file.
struct Bundled {
    name: &'static str,
    png: &'static [u8],
}

/// The images, in the list's order: the first is shown at the start.
const IMAGES: [Bundled; 4] = [
    Bundled { name: "lumen.png", png: include_bytes!("../../../assets/wallpaper/lumen.png") },
    Bundled { name: "tiles.png", png: include_bytes!("../images/tiles.png") },
    Bundled { name: "grey16.png", png: include_bytes!("../images/grey16.png") },
    Bundled { name: "broken.png", png: include_bytes!("../images/broken.png") },
];

/// The label's width: the panel's, less its padding.
const INFO_W: u32 = PANEL_W - 16;
/// Where the view's text (decoding, an error) starts, from its corner.
const TEXT_INSET: i32 = 12;

/// What the view holds.
enum Shown {
    /// Waiting for the next tick to decode the current image.
    Decoding,
    /// The image, and its copy fitted to the view (where it goes, its
    /// pixels).
    Image { image: Image, fit_at: Area, fit: Vec<u32> },
    /// Why it couldn't be shown.
    Failed(String),
}

struct Viewer {
    ui: Ui,
    list: Widget,
    previous: Widget,
    next: Widget,
    fit_box: Widget,
    info: Widget,
    /// The image chosen (an index into `IMAGES`).
    current: usize,
    shown: Shown,
    fit: bool,
    pan: Pan,
    /// Whether the view changed since the last repaint.
    dirty: bool,
}

impl Viewer {
    fn new(first: usize) -> Viewer {
        let mut ui = Ui::new();
        let names: Vec<&str> = IMAGES.iter().map(|i| i.name).collect();
        let list = ui.list(None, "Images", &names);
        let row = ui.hstack(None, "Browse");
        let previous = ui.button(Some(row), "Previous");
        let next = ui.button(Some(row), "Next");
        let fit_box = ui.checkbox(None, "Fit", true);
        let info = ui.fixed_label(None, "Info", "", INFO_W);
        let mut v = Viewer {
            ui,
            list,
            previous,
            next,
            fit_box,
            info,
            current: first,
            shown: Shown::Decoding,
            fit: true,
            pan: Pan::default(),
            dirty: true,
        };
        v.choose(first);
        v
    }

    /// Choose image `i`: the list's selection follows (as a click on it
    /// does), the image shown is dropped, and the next tick decodes it.
    fn choose(&mut self, i: usize) {
        if let Some(item) = self.ui.item_id(self.list, i) {
            // Its `Selected` event is this choice itself.
            let _ = self.ui.invoke(item.get(), &Action::Select);
        }
        self.current = i;
        self.shown = Shown::Decoding;
        self.pan = Pan::default();
        self.dirty = true;
        self.ui.set_text(self.info, &format!("decoding {}", IMAGES[i].name));
    }

    /// Decode the current image: shown, or why not.
    fn decode(&mut self) {
        let image = &IMAGES[self.current];
        self.shown = match show(image.png) {
            Ok((image, fit_at, fit)) => Shown::Image { image, fit_at, fit },
            Err(why) => {
                log::info(&format!("viewer: {}: {why}", image.name));
                Shown::Failed(why)
            }
        };
        let info = match &self.shown {
            Shown::Image { image: shown, .. } => format!("{} {}x{}", image.name, shown.width(), shown.height()),
            Shown::Failed(why) => format!("{}: {why}", image.name),
            Shown::Decoding => format!("decoding {}", image.name),
        };
        self.ui.set_text(self.info, &info);
        self.dirty = true;
    }

    /// An arrow key in a 1:1 view: the pan moved (whether it did).
    fn pan_by(&mut self, dx: i32, dy: i32) -> bool {
        let Shown::Image { image, .. } = &self.shown else { return false };
        if self.fit || !self.pan.by(dx, dy, image.width(), image.height(), VIEW) {
            return false;
        }
        self.dirty = true;
        true
    }

    /// `run_until`'s `take`: the arrow keys pan a 1:1 view; everything else
    /// is the toolkit's.
    fn take(&mut self, e: &Event) -> Taken {
        let Event::Key { code, action: KeyAction::Press { .. }, .. } = *e else { return Taken::Pass };
        let (dx, dy) = match code {
            KeyCode::ArrowLeft => (-1, 0),
            KeyCode::ArrowRight => (1, 0),
            KeyCode::ArrowUp => (0, -1),
            KeyCode::ArrowDown => (0, 1),
            _ => return Taken::Pass,
        };
        self.pan_by(dx, dy);
        Taken::Took
    }
}

/// `png` decoded and fitted to the view, or why it can't be shown.
fn show(png: &[u8]) -> Result<(Image, Area, Vec<u32>), String> {
    admit(png).map_err(|e| e.to_string())?;
    let image = decode(png, MAX_IMAGE_W, MAX_IMAGE_H).map_err(|e| e.to_string())?;
    let at = fitted(image.width(), image.height(), VIEW);
    let fit = scale(image.pixels(), image.width(), image.height(), at.w, at.h)
        .ok_or_else(|| String::from("no memory for its fitted copy"))?;
    Ok((image, at, fit))
}

fn rect(a: Area) -> Rect {
    Rect::new(a.x, a.y, a.w, a.h)
}

/// Copy `src`'s rows (each `src_stride` pixels, starting at (`src_x`,
/// `src_y`) for the corner of `at`) into `surface` where `at` meets `clip`.
fn blit(
    surface: &mut lumen_app::Surface,
    src: &[u32],
    src_stride: usize,
    src_x: u32,
    src_y: u32,
    at: Area,
    clip: Rect,
) {
    let Some(r) = rect(at).intersect(&clip) else { return };
    let (dx, dy) = ((r.x() - at.x) as usize, (r.y() - at.y) as usize);
    for row in 0..r.h() as usize {
        let from = (src_y as usize + dy + row) * src_stride + src_x as usize + dx;
        let to = &mut surface.row_mut(r.y() as u32 + row as u32)[r.x() as usize..][..r.w() as usize];
        to.copy_from_slice(&src[from..from + r.w() as usize]);
    }
}

impl App for Viewer {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        let n = IMAGES.len();
        for e in events {
            match *e {
                UiEvent::Selected(w, i) if w == self.list && i < n => self.choose(i),
                UiEvent::Pressed(w) if w == self.previous => self.choose((self.current + n - 1) % n),
                UiEvent::Pressed(w) if w == self.next => self.choose((self.current + 1) % n),
                UiEvent::Toggled(w, on) if w == self.fit_box => {
                    self.fit = on;
                    self.dirty = true;
                }
                _ => {}
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        let view = rect(VIEW);
        for r in over.rects {
            let Some(clip) = r.intersect(&view) else { continue };
            match &self.shown {
                Shown::Image { image, fit_at, fit } => {
                    if self.fit {
                        blit(over.surface, fit, fit_at.w as usize, 0, 0, *fit_at, clip);
                    } else {
                        let p = actual(image.width(), image.height(), VIEW, self.pan);
                        blit(over.surface, image.pixels(), image.width() as usize, p.src_x, p.src_y, p.dst, clip);
                    }
                }
                Shown::Decoding => {
                    let line = format!("Decoding {}...", IMAGES[self.current].name);
                    over.text.draw(
                        over.surface,
                        VIEW.x + TEXT_INSET,
                        VIEW.y + TEXT_INSET,
                        &line,
                        over.palette.text,
                        clip,
                    );
                }
                Shown::Failed(why) => {
                    let (x, y) = (VIEW.x + TEXT_INSET, VIEW.y + TEXT_INSET);
                    let first = format!("Can't show {}:", IMAGES[self.current].name);
                    over.text.draw(over.surface, x, y, &first, over.palette.text, clip);
                    let next = y + over.text.line_height() as i32;
                    over.text.draw(over.surface, x, next, why, over.palette.text, clip);
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        if core::mem::take(&mut self.dirty) { alloc::vec![rect(VIEW)] } else { Vec::new() }
    }
}

impl Ticking for Viewer {
    /// A tick only while an image waits to be decoded (the floor's 10 ms
    /// later): otherwise the window waits for events, and settles.
    fn period(&self) -> Option<u64> {
        matches!(self.shown, Shown::Decoding).then_some(0)
    }

    /// Not an animation: the one tick decodes, then it waits.
    fn animating(&self) -> bool {
        false
    }

    fn tick(&mut self, _now: u64) -> Flow {
        if matches!(self.shown, Shown::Decoding) {
            self.decode();
        }
        Flow::Carry
    }
}

unsafe extern "C" {
    /// Where the linker put the end of the stack and data: the heap's start.
    static __heap_base: u8;
}

/// The memory lumen-view's plan was checked for: its size, and the heap
/// starting no later than the plan's stack and data allow.
fn memory_as_planned() -> Result<(), String> {
    let memory = core::arch::wasm32::memory_size(0) * 65536;
    let heap = &raw const __heap_base as usize;
    if memory != lumen_view::MEMORY_BYTES {
        return Err(format!("memory {memory} bytes, not lumen-view's {}", lumen_view::MEMORY_BYTES));
    }
    if heap > lumen_view::STACK_BYTES + lumen_view::DATA_BYTES {
        return Err(format!("the heap starts at {heap}, past lumen-view's stack and data"));
    }
    Ok(())
}

/// The app's exit code: 0 once its window is closed, or 1 once its failure
/// is logged.
fn run() -> i32 {
    lumen_app::report(app())
}

fn app() -> Result<(), (&'static str, Error)> {
    if let Err(why) = memory_as_planned() {
        log::error(&format!("viewer: {why}"));
        return Err(("memory", Error::Unavailable));
    }
    let args = lumen_app::args().map_err(|e| ("arguments", e))?;
    let first = match args.first() {
        None => 0,
        Some(name) => match IMAGES.iter().position(|i| i.name == name.as_str()) {
            Some(i) => i,
            None => {
                let names: Vec<&str> = IMAGES.iter().map(|i| i.name).collect();
                log::error(&format!("viewer: no image {name} (it has {})", names.join(", ")));
                return Err(("arguments", Error::BadArgument));
            }
        },
    };
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Viewer", WINDOW_W, WINDOW_H).map_err(|e| ("no window", e))?;
    let panel = Rect::new(0, 0, PANEL_W, WINDOW_H);
    let mut tk = Toolkit::with_area(Viewer::new(first), window, panel).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, Viewer::take).map(|_| ()).map_err(|e| ("toolkit", e))
}
