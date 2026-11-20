//! The image viewer (#204): the images bundled with it (the desktop's
//! wallpaper, two small test images, a corrupt one, and one too costly to
//! decode), decoded inside the app by `lumen-png`, so a bad file can only
//! end the app, never the kernel (M6 brings files; until then these are
//! its images).
//!
//! - **The window:** a panel of widgets on the left (the `Images` list,
//!   `Previous` and `Next`, the `Fit` checkbox, the `Info` label: the
//!   image's name and size, `decoding <name>`, or `<name>: <why>` for one
//!   it couldn't show) and the view, lumen-view's `VIEW`, on the right.
//! - **Showing an image:** choosing one (in the list, through Previous or
//!   Next, or `run viewer <name>` for the first; the one already chosen
//!   changes nothing) drops the one shown, says `decoding <name>` and
//!   answers at once: the decoding is the next tick's (`Ticking::period`
//!   is set only while one waits), so the request choosing it isn't held
//!   up by it (one arriving meanwhile waits for it). The wait before that
//!   tick animates, so the desktop settles only once the image is shown.
//!   The list's selection follows the choice without taking the focus.
//! - **Refused:** `lumen_view::prepare` refuses, from its length and
//!   header (`admit`), a file longer, an image bigger or costlier to decode
//!   than the memory plan covers; the decoder refuses a bad one. Either is
//!   shown in the view (its reason wrapped to the view's width) and the
//!   label, logged at INFO, and the app goes on. Running out of memory is
//!   the plan failing, not the file: a WARN.
//! - **1:1 or fitted:** `Fit` checked (the start) shows the image fitted
//!   to the view (a copy scaled once, by nearest neighbour); unchecked, at
//!   1:1 (`lumen_view::actual`), the arrow keys panning a bigger image
//!   (`Pan`).
//! - **Drawing:** the view is the app's own (`with_area`: the tree is laid
//!   out in the panel only); a change to it is one rectangle of damage
//!   (`take_drawn`), its pixels drawn in `paint_over` only where the
//!   repainted rectangles reach it, each line of text only where its box
//!   meets one.
//! - **Memory:** fixed at `lumen_view::MEMORY_BYTES` (its `build.rs`),
//!   checked at the start (with where the heap starts, against
//!   lumen-view's plan): `tests/memory.rs` there checks the app's
//!   allocations (`lumen_view::prepare`, this app's one call) fit. A
//!   failed check, or an image `run` names that it doesn't have, is one
//!   ERROR and exit 1, before the window opens.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode, Surface, Time, log};
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};
use lumen_view::{PANEL, PANEL_W, Pan, Prepared, VIEW, WINDOW_H, WINDOW_W, actual, prepare};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// An image bundled with the app: its name (the list's item, `run viewer
/// <name>`) and its file.
struct Bundled {
    name: &'static str,
    png: &'static [u8],
}

/// The images, in the list's order: the first is shown at the start.
const IMAGES: [Bundled; 5] = [
    Bundled { name: "lumen.png", png: include_bytes!("../../../assets/wallpaper/lumen.png") },
    Bundled { name: "tiles.png", png: include_bytes!("../images/tiles.png") },
    Bundled { name: "grey16.png", png: include_bytes!("../images/grey16.png") },
    Bundled { name: "broken.png", png: include_bytes!("../images/broken.png") },
    Bundled { name: "costly.png", png: include_bytes!("../images/costly.png") },
];

/// The label's width: the panel's, less its padding.
const INFO_W: u32 = PANEL_W - 16;
/// Where the view's text (decoding, an error) starts, from its corner.
const TEXT_INSET: i32 = 12;

/// What the view holds.
enum Shown {
    /// Waiting for the next tick to decode the current image.
    Decoding,
    /// The image, and its copy fitted to the view.
    Image(Prepared),
    /// Why it couldn't be shown, and that reason wrapped to the view's
    /// width (worked out at its first paint).
    Failed { why: String, lines: Option<Vec<String>> },
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
        let list = ui.list(None, "Images", &names());
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
        v.start(first);
        v
    }

    /// Choose image `i`; the one already chosen (shown, refused, or still
    /// waiting for its decode) changes nothing, so a click on it never
    /// decodes it again.
    fn choose(&mut self, i: usize) {
        if i != self.current {
            self.start(i);
        }
    }

    /// Start showing image `i`: the list's selection follows (leaving the
    /// focus where it is: Enter on a focused Next keeps pressing Next), the
    /// image shown is dropped, and the next tick decodes it.
    fn start(&mut self, i: usize) {
        self.ui.set_selected(self.list, Some(i));
        self.current = i;
        self.shown = Shown::Decoding;
        self.pan = Pan::default();
        self.dirty = true;
        self.ui.set_text(self.info, &format!("decoding {}", IMAGES[i].name));
    }

    /// Decode the current image: shown, or why not.
    fn decode(&mut self) {
        let name = IMAGES[self.current].name;
        let info = match prepare(IMAGES[self.current].png) {
            Ok(shown) => {
                let info = format!("{name} {}x{}", shown.image().width(), shown.image().height());
                self.shown = Shown::Image(shown);
                info
            }
            Err(why) => {
                // A bad or refused file is the file's; running out of memory
                // is the memory plan failing (lumen-view's tests/memory.rs
                // says it fits), worth a warning.
                if why.out_of_memory() {
                    log::warn(&format!("{name}: out of memory: lumen-view's memory plan doesn't hold ({why})"));
                } else {
                    log::info(&format!("{name}: {why}"));
                }
                let info = format!("{name}: {why}");
                self.shown = Shown::Failed { why: why.to_string(), lines: None };
                info
            }
        };
        self.ui.set_text(self.info, &info);
        self.dirty = true;
    }

    /// An arrow key in a 1:1 view: the pan moved.
    fn pan_by(&mut self, dx: i32, dy: i32) {
        let Shown::Image(shown) = &self.shown else { return };
        let image = shown.image();
        if !self.fit && self.pan.by(dx, dy, image.width(), image.height(), VIEW) {
            self.dirty = true;
        }
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

/// `text` cut into lines at most `width` pixels wide at its spaces (a word
/// wider than that alone on its line, cut by the clip when drawn); each
/// word measured once.
fn wrap(text: &mut dyn lumen_ui::Text, s: &str, width: u32) -> Vec<String> {
    let space = text.width(" ");
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0;
    for word in s.split(' ').filter(|w| !w.is_empty()) {
        let w = text.width(word);
        if !line.is_empty() && line_w + space + w > width {
            lines.push(core::mem::take(&mut line));
            line_w = 0;
        }
        if !line.is_empty() {
            line.push(' ');
            line_w += space;
        }
        line.push_str(word);
        line_w += w;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The images' names, in the list's order.
fn names() -> Vec<&'static str> {
    IMAGES.iter().map(|i| i.name).collect()
}

/// Copy `src`, its corner at (`x`, `y`), into `surface` inside `clip` (and
/// inside `src`: lumen-gfx's clipped copy).
fn blit(surface: &mut Surface, src: &Surface, x: i32, y: i32, clip: Rect) {
    src.copy_to(clip.translate(-x, -y), surface, clip.x(), clip.y());
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
        let name = IMAGES[self.current].name;
        // Text lines: drawn only where their box meets what is repainted
        // (each `text_draw` is charged by the character, wherever it lands).
        let line_h = over.text.line_height() as i32;
        let text_w = VIEW.w() - 2 * TEXT_INSET as u32;
        let line_at = |n: usize| {
            Rect::new(VIEW.x() + TEXT_INSET, VIEW.y() + TEXT_INSET + n as i32 * line_h, text_w, line_h as u32)
        };
        if let Shown::Failed { why, lines: lines @ None } = &mut self.shown {
            *lines = Some(wrap(over.text, why, text_w));
        }
        let text: Vec<String> = match &self.shown {
            Shown::Image(_) => Vec::new(),
            Shown::Decoding => alloc::vec![format!("Decoding {name}...")],
            Shown::Failed { lines, .. } => {
                let mut text = alloc::vec![format!("Can't show {name}:")];
                text.extend(lines.iter().flatten().cloned());
                text
            }
        };
        for r in over.rects {
            let Some(clip) = r.intersect(&VIEW) else { continue };
            if let Shown::Image(shown) = &self.shown {
                let (image, fitted) = (shown.image(), shown.fitted());
                if self.fit {
                    blit(over.surface, fitted.pixels(), fitted.at().x(), fitted.at().y(), clip);
                } else {
                    let p = actual(image.width(), image.height(), VIEW, self.pan);
                    blit(over.surface, image, p.dst.x() - p.src_x as i32, p.dst.y() - p.src_y as i32, clip);
                }
            }
            for (n, line) in text.iter().enumerate() {
                let at = line_at(n);
                if at.bottom() > VIEW.bottom() {
                    break;
                }
                if let Some(cut) = at.intersect(&clip) {
                    over.text.draw(over.surface, at.x(), at.y(), line, over.palette.text, cut);
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        if core::mem::take(&mut self.dirty) { alloc::vec![VIEW] } else { Vec::new() }
    }
}

impl Ticking for Viewer {
    /// A tick only while an image waits to be decoded (the floor's 10 ms
    /// later): otherwise the window waits for events, and settles. The wait
    /// before that tick is an animating one (`animating`'s default), so the
    /// desktop never counts the window settled while its view still says
    /// `Decoding`.
    fn period(&self) -> Option<u64> {
        matches!(self.shown, Shown::Decoding).then_some(0)
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

/// The app's exit code: 0 once its window is closed; 1 once a failure is
/// logged as one ERROR (the viewer's own checks, or a host call's).
fn run() -> i32 {
    match first_image() {
        Ok(first) => lumen_app::report(app(first)),
        Err(why) => {
            log::error(&why);
            1
        }
    }
}

/// The viewer's own checks before it opens its window: its memory is the
/// plan's, and the image `run viewer <name>` names (the first, without
/// one) is one it has. Which image to show first, or why not.
fn first_image() -> Result<usize, String> {
    memory_as_planned()?;
    let args = lumen_app::args().map_err(|e| format!("its arguments: {e}"))?;
    let Some(name) = args.first() else { return Ok(0) };
    IMAGES
        .iter()
        .position(|i| i.name == name.as_str())
        .ok_or_else(|| format!("no image {name} (it has {})", names().join(", ")))
}

fn app(first: usize) -> Result<(), (&'static str, Error)> {
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Viewer", WINDOW_W, WINDOW_H).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::with_area(Viewer::new(first), window, PANEL).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, Viewer::take).map(|_| ()).map_err(|e| ("toolkit", e))
}
