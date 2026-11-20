//! The text editor (#203): a buffer in memory (files come with M6), drawn
//! by the app over a `lumen-ui` text area, so its window shows in `ui tree`
//! (the buffer a `textfield` node whose value is its text, the status line
//! a label) and `ui invoke <Text> set_value <text>` replaces the text as one
//! edit that undo takes back. Its editing is `lumen_edit::Editor`'s
//! (host-tested).
//!
//! - **Keys** go to it whatever has the focus: characters type, Enter
//!   breaks the line, Tab inserts four spaces, Backspace and Delete, the
//!   arrows, Home and End (with Ctrl: the text's start and end), Page Up
//!   and Page Down; Ctrl+Z undoes, Ctrl+Y and Ctrl+Shift+Z redo.
//! - **Drawing** is by damage: only the lines an edit changed, the cursor's
//!   old and new places for a move, everything only when the view scrolls.
//!   Each line is drawn only inside the rectangles being repainted
//!   (`Over`'s): `text_draw` costs the app fuel by the character, so a
//!   screenful drawn again a key would be a slice's worth.
//! - **The view** scrolls to keep the cursor in it: by lines, and across in
//!   jumps of half its width. A line past 1,024 bytes shows (and measures)
//!   only its first 1,024 (the host's text cap).
//! - **The cursor** blinks every 500 ms while the window has the focus, and
//!   stays solid through the tick after a key; `run editor --no-blink`
//!   keeps it solid (the window then settles: the golden). Without the
//!   focus it isn't shown.
//! - **The tree:** the buffer is a `textfield` node named `Text`, its value
//!   the text's first 1,024 bytes (`TEXT_BYTES`, the tree's cap; the kernel
//!   shows control characters, `\n` included, as U+FFFD). A `set_value`
//!   (at most 512 bytes, the kernel's cap) replaces the text. The status
//!   line is a label named `Status`: `Ln <line>, Col <column>`, then
//!   `, modified` once the text differs from the one it started with.
//! - **Arguments:** `--empty` starts with no text (else a short sample),
//!   `--no-blink` as above.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lumen_app::api::limits::{TEXT_BYTES, UI_TEXT_PX};
use lumen_app::api::text::prefix;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode, Mods, Time, log, mods};
use lumen_edit::{Change, Command, Editor, Limits};
use lumen_ui::ui::{GAP, PAD};
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

const WIDTH: u32 = 560;
const HEIGHT: u32 = 400;
/// The text's margin inside the area's border.
const TEXT_PAD: i32 = 4;
const CURSOR_W: u32 = 2;
const BLINK_NS: u64 = 500_000_000;
/// What the editor holds at most: 16 KiB of text, and 500 edits or 256 KiB
/// of them to undo (the app's memory is 4 MiB).
const LIMITS: Limits = Limits { text_bytes: 16 * 1024, undo_edits: 500, undo_bytes: 256 * 1024 };

/// The text it starts with (unless `--empty`).
const SAMPLE: &str = "Welcome to Lumen's editor.\n\
\n\
Type to edit this text. The arrows, Home, End,\n\
Page Up and Page Down move; Ctrl+Z undoes and\n\
Ctrl+Y redoes.\n\
\n\
The text lives in memory: files come with M6.\n";

/// The app: the tree (the text area and the status line), the editor, and
/// what drawing needs.
struct EditorApp {
    ui: Ui,
    area: Widget,
    status: Widget,
    ed: Editor,
    desktop: Desktop,
    /// A line's height, and how many the view shows.
    line: u32,
    rows: usize,
    /// The cursor's x in its line's text (pixels from the line's start),
    /// and how far the view is scrolled across.
    cursor_x: u32,
    hscroll: u32,
    blink: bool,
    focused: bool,
    /// The blink's phase, and whether a key came since the last tick (the
    /// cursor then stays solid through it).
    cursor_on: bool,
    keyed: bool,
    /// Whether a measure was refused (logged once).
    warned: bool,
    drawn: Vec<Rect>,
}

impl EditorApp {
    fn new(desktop: Desktop, text: &str, blink: bool) -> Result<EditorApp, Error> {
        let line = desktop.line_height(UI_TEXT_PX)?;
        let mut ui = Ui::new();
        let w = WIDTH - 2 * PAD;
        let h = HEIGHT - 2 * PAD - GAP - line;
        let ed = Editor::new(text, LIMITS);
        let area = ui.text_area(None, "Text", prefix(ed.text(), TEXT_BYTES), w, h);
        let status = ui.fixed_label(None, "Status", "", w);
        // Inside the border, less the margin above and below.
        let rows = ((h.saturating_sub(2) as i32 - 2 * TEXT_PAD).max(0) as u32 / line.max(1)).max(1) as usize;
        let mut app = EditorApp {
            ui,
            area,
            status,
            ed,
            desktop,
            line,
            rows,
            cursor_x: 0,
            hscroll: 0,
            blink,
            focused: false,
            cursor_on: true,
            keyed: false,
            warned: false,
            drawn: Vec::new(),
        };
        app.ed.scroll_to(rows);
        app.show_status();
        Ok(app)
    }

    /// The area inside its border.
    fn inner(&self) -> Rect {
        let b = self.ui.bounds(self.area);
        Rect::new(b.x() + 1, b.y() + 1, b.w().saturating_sub(2), b.h().saturating_sub(2))
    }

    /// Where the text's lines start across (scrolled).
    fn text_x(&self) -> i32 {
        self.inner().x() + TEXT_PAD - self.hscroll as i32
    }

    /// View row `row`'s band, the area's width.
    fn row_rect(&self, row: usize) -> Rect {
        let inner = self.inner();
        Rect::new(inner.x(), inner.y() + TEXT_PAD + (row as u32 * self.line) as i32, inner.w(), self.line)
    }

    /// Text line `line`'s band, if the view shows it.
    fn line_rect(&self, line: usize) -> Option<Rect> {
        let top = self.ed.top();
        (line >= top && line < top + self.rows).then(|| self.row_rect(line - top))
    }

    /// The cursor's rectangle (inside the area), if its line is shown.
    fn cursor_rect(&self) -> Option<Rect> {
        let band = self.line_rect(self.ed.cursor_line())?;
        let x = self.text_x() + self.cursor_x as i32;
        Rect::new(x, band.y(), CURSOR_W, self.line).intersect(&self.inner())
    }

    fn cursor_shown(&self) -> bool {
        self.focused && (self.cursor_on || !self.blink)
    }

    /// Every row from line `from`'s to the view's last, as one rectangle.
    fn damage_from(&mut self, from: usize) {
        let top = self.ed.top();
        let first = from.max(top);
        if first >= top + self.rows {
            return;
        }
        let a = self.row_rect(first - top);
        let z = self.row_rect(self.rows - 1);
        self.drawn.push(a.union(&z));
    }

    fn damage_cursor(&mut self) {
        if let Some(r) = self.cursor_rect() {
            self.drawn.push(r);
        }
    }

    /// The cursor's x from its line's text before it (the host's measure).
    fn measure_cursor(&mut self) -> u32 {
        match self.desktop.measure(UI_TEXT_PX, prefix(self.ed.before_cursor(), TEXT_BYTES)) {
            Ok(x) => x,
            Err(e) => {
                if !self.warned {
                    self.warned = true;
                    log::warn(&format!("editor: measure refused: {e}"));
                }
                0
            }
        }
    }

    /// Run `cmd`, then show what it changed.
    fn command(&mut self, cmd: Command) {
        let old_cursor = self.cursor_rect();
        let change = self.ed.apply(cmd);
        self.changed(change, old_cursor);
    }

    /// Mark what `change` changed to repaint (`old_cursor`: where the
    /// cursor was drawn), scroll to the cursor, and update the tree.
    fn changed(&mut self, change: Change, old_cursor: Option<Rect>) {
        // A key keeps the cursor solid through the next tick.
        self.keyed = true;
        let was_on = self.cursor_on;
        self.cursor_on = true;
        if matches!(change, Change::None | Change::Refused) {
            if !was_on {
                self.damage_cursor();
            }
            return;
        }
        self.cursor_x = self.measure_cursor();
        let scrolled = self.ed.scroll_to(self.rows) | self.scroll_across();
        match change {
            _ if scrolled => self.damage_from(0),
            Change::Cursor { .. } => {
                if let Some(r) = old_cursor {
                    self.drawn.push(r);
                }
                self.damage_cursor();
            }
            Change::Line(l) => {
                if let Some(r) = self.line_rect(l) {
                    self.drawn.push(r);
                }
            }
            Change::From(l) => self.damage_from(l),
            Change::None | Change::Refused => {}
        }
        self.ui.set_text(self.area, prefix(self.ed.text(), TEXT_BYTES));
        self.show_status();
    }

    /// Scroll across so the cursor shows (in jumps of half the view's
    /// width): whether it moved.
    fn scroll_across(&mut self) -> bool {
        let width = self.inner().w().saturating_sub(2 * TEXT_PAD as u32 + CURSOR_W);
        let x = self.cursor_x;
        if x >= self.hscroll && x <= self.hscroll + width {
            return false;
        }
        let to = if x <= width { 0 } else { x - width / 2 };
        let moved = to != self.hscroll;
        self.hscroll = to;
        moved
    }

    fn show_status(&mut self) {
        let (line, col) = self.ed.line_col();
        let modified = if self.ed.modified() { ", modified" } else { "" };
        let text = format!("Ln {}, Col {}{modified}", line + 1, col + 1);
        self.ui.set_text(self.status, &text);
    }

    /// The input the editor takes itself: its keys (whatever has the
    /// focus), and the window's focus (seen, then handed on to the toolkit).
    fn take(&mut self, e: &Event) -> Taken {
        match *e {
            Event::Key { code, mods, action: KeyAction::Press { text } } => match command(code, mods, text) {
                Some(cmd) => {
                    self.command(cmd);
                    Taken::Took
                }
                None => Taken::Pass,
            },
            Event::FocusGained | Event::FocusLost => {
                self.focused = matches!(e, Event::FocusGained);
                self.cursor_on = true;
                self.damage_cursor();
                Taken::Pass
            }
            _ => Taken::Pass,
        }
    }
}

/// The command a key press stands for, if any.
fn command(code: KeyCode, m: Mods, text: Option<char>) -> Option<Command> {
    let ctrl = m.any(mods::CTRL);
    let shift = m.any(mods::SHIFT);
    Some(match code {
        KeyCode::Z if ctrl && shift => Command::Redo,
        KeyCode::Z if ctrl => Command::Undo,
        KeyCode::Y if ctrl => Command::Redo,
        KeyCode::Home if ctrl => Command::Top,
        KeyCode::End if ctrl => Command::Bottom,
        KeyCode::Return | KeyCode::NumpadEnter => Command::Newline,
        KeyCode::Tab => Command::Tab,
        KeyCode::Backspace => Command::Backspace,
        KeyCode::Delete => Command::Delete,
        KeyCode::ArrowLeft => Command::Left,
        KeyCode::ArrowRight => Command::Right,
        KeyCode::ArrowUp => Command::Up,
        KeyCode::ArrowDown => Command::Down,
        KeyCode::Home => Command::Home,
        KeyCode::End => Command::End,
        KeyCode::PageUp => Command::PageUp,
        KeyCode::PageDown => Command::PageDown,
        _ if ctrl => return None,
        _ => return text.filter(|c| !c.is_control()).map(Command::Insert),
    })
}

impl App for EditorApp {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            // `set_value` on the text: one edit replacing it all.
            if let UiEvent::Edited(w) = e
                && *w == self.area
            {
                let old_cursor = self.cursor_rect();
                let text = String::from(self.ui.text(self.area).unwrap_or(""));
                let change = self.ed.replace_all(&text);
                self.changed(change, old_cursor);
                // A text the editor took in differently (its control
                // characters but `\n` dropped) shows as it holds it.
                self.ui.set_text(self.area, prefix(self.ed.text(), TEXT_BYTES));
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        let x = self.text_x();
        let top = self.ed.top();
        let cursor = if self.cursor_shown() { self.cursor_rect() } else { None };
        for row in 0..self.rows {
            let band = self.row_rect(row);
            if !over.touches(band) {
                continue;
            }
            let text = self.ed.line(top + row);
            // Each line only inside the rectangles being repainted (they are
            // disjoint: each pixel drawn once).
            for r in over.rects {
                let Some(clip) = band.intersect(r) else { continue };
                if !text.is_empty() {
                    over.text.draw(over.surface, x, band.y(), text, over.palette.text, clip);
                }
                if let Some(c) = cursor.and_then(|c| c.intersect(&clip)) {
                    over.surface.fill_rect(c, over.palette.text);
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for EditorApp {
    /// The blink's half period while the window has the focus.
    fn period(&self) -> Option<u64> {
        (self.blink && self.focused).then_some(BLINK_NS)
    }

    fn tick(&mut self, _now: u64) -> Flow {
        if core::mem::take(&mut self.keyed) {
            // Solid through the tick after a key.
            if !self.cursor_on {
                self.cursor_on = true;
                self.damage_cursor();
            }
        } else {
            self.cursor_on = !self.cursor_on;
            self.damage_cursor();
        }
        Flow::Carry
    }
}

/// The app's exit code: 0 once its window is closed, 1 once a failure (or
/// a bad argument) is logged.
fn run() -> i32 {
    lumen_app::report(app())
}

fn app() -> Result<(), (&'static str, Error)> {
    let (mut empty, mut blink) = (false, true);
    for arg in lumen_app::args().map_err(|e| ("its arguments", e))? {
        match arg.as_str() {
            "--empty" => empty = true,
            "--no-blink" => blink = false,
            _ => return Err(("usage: run editor [--empty] [--no-blink]", Error::BadArgument)),
        }
    }
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let editor = EditorApp::new(desktop, if empty { "" } else { SAMPLE }, blink).map_err(|e| ("no font", e))?;
    let window = desktop.open_window("Editor", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::new(editor, window).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, EditorApp::take).map(|_| ()).map_err(|e| ("toolkit", e))
}
