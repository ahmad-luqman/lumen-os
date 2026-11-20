//! The text editor (#203): a buffer in memory (files come with M6), drawn
//! by the app over a `lumen-ui` text area, so its window shows in `ui tree`
//! (the buffer a `textfield` node whose value is its text, the status line
//! a label) and `ui invoke <Text> set_value <text>` replaces the text as one
//! edit that undo takes back. Its logic is `lumen-edit`'s (host-tested):
//! the editing (`Editor`), the keys (`command`) and the view across
//! (`view`).
//!
//! - **Keys** go to it whatever has the focus, mapped by
//!   `lumen_edit::command` (host-tested, on several layouts): characters
//!   type, Enter breaks the line, Tab inserts four spaces, Backspace and
//!   Delete, the arrows, Home and End (with Ctrl: the text's start and
//!   end), Page Up and Page Down; Ctrl+Z undoes, Ctrl+Y and Ctrl+Shift+Z
//!   redo, by the layout's letters.
//! - **Drawing** is by damage: a line an edit changed (for an edit at the
//!   cursor, from just before it on: a glyph's edge reaches a little left), every row from the first changed line on when
//!   lines come or go, the cursor's old and new places for a move (one rect
//!   on one line), every row when the view scrolls. Each line is drawn once
//!   a repaint, only as far as the view shows and only inside the rect
//!   being repainted that meets it: `text_draw` costs the app fuel by the
//!   character, and repainting costs it by the pixel.
//! - **The view** scrolls to keep the cursor in it: by lines, and across by
//!   characters (`lumen_edit::view`): each line drawn, and the cursor
//!   measured, only as far as the view can show. A line holds at most
//!   4 KiB.
//! - **The cursor** toggles every 500 ms while the window has the focus
//!   (shown 500, hidden 500), and stays solid through the tick after a
//!   key; `run editor --no-blink` keeps it solid (the window then settles:
//!   the golden). Without the focus it isn't shown and the editor has no
//!   period, so the desktop settles.
//! - **The tree:** the buffer is a `textfield` node named `Text`, its value
//!   the text's first 1,024 bytes (`TEXT_BYTES`, the tree's cap; the kernel
//!   shows control characters, `\n` included, as U+FFFD): a lossy view of
//!   the buffer, not the buffer. A `set_value` (at most 512 bytes, the
//!   kernel's cap) replaces the text, normalised (a tab as spaces), as one
//!   edit; one past the editor's limits is answered failed. The status
//!   line is a label named `Status` (`lumen_edit::Editor::status`): `Ln
//!   <line>, Col <column>`, then `, modified` once the text differs from
//!   the one it started with, and `, full` while the last edit was refused
//!   (past the text's 16 KiB or a line's 4 KiB).
//! - **Arguments:** `--empty` starts with no text (else a short sample),
//!   `--no-blink` as above.
//! - **Not built:** selection, a scrollbar and placing the cursor with the
//!   mouse (a click focuses the text); opening and saving files (M6).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lumen_app::api::limits::{SET_VALUE_BYTES, TEXT_BYTES, UI_TEXT_PX};
use lumen_app::api::text::prefix;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, Time, log};
use lumen_edit::{Change, Command, Editor, Limits, TAB};
use lumen_ui::ui::{GAP, PAD};
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

const WIDTH: u32 = 560;
const HEIGHT: u32 = 400;
/// The text's margin inside the area's border.
const TEXT_PAD: i32 = 4;
const CURSOR_W: u32 = 2;
/// How far left of where a glyph starts it may draw (at `UI_TEXT_PX`): an
/// edit at the cursor repaints from this far before it.
const GLYPH_OVERHANG: i32 = 4;
const BLINK_NS: u64 = 500_000_000;
/// What the editor holds at most: 16 KiB of text, 4 KiB a line, and 500
/// edits or 256 KiB of their text to undo (the app's memory is 4 MiB).
const LIMITS: Limits = Limits { text_bytes: 16 * 1024, line_bytes: 4 * 1024, undo_edits: 500, undo_bytes: 256 * 1024 };

// A `set_value` (at most `SET_VALUE_BYTES`, the kernel's cap) always fits
// the text's and a line's limits once normalised (each byte a tab at most,
// which grows to `TAB`'s spaces); undoing one keeps its history (twice the
// text: what it removed and inserted); and there is a history at all.
const SET_VALUE_NORMALISED: usize = SET_VALUE_BYTES * TAB.len();
const _: () = assert!(SET_VALUE_NORMALISED <= LIMITS.text_bytes && SET_VALUE_NORMALISED <= LIMITS.line_bytes);
const _: () = assert!(LIMITS.undo_bytes >= 2 * LIMITS.text_bytes && LIMITS.undo_edits > 0);

/// Narrow glyphs: the narrowest one's measured advance bounds how many
/// chars the view shows (as many as fit: a run of narrower ones, each
/// advance rounded by the host, or of zero-width marks may stop short of
/// the view's right edge).
const NARROW: [&str; 9] = [" ", ".", ",", "'", "i", "l", "|", "!", "j"];

/// The view's width for the text, inside an area `area_w` wide: less its
/// border, the margins and the cursor.
const fn view_width(area_w: u32) -> u32 {
    area_w.saturating_sub(2 + 2 * TEXT_PAD as u32 + CURSOR_W)
}

/// The widest status line it shows (a 16 KiB text has fewer lines and
/// columns than this).
const WIDEST_STATUS: &str = "Ln 99999, Col 99999, modified, full";

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
    /// The view across: the first char each line shows from, the most it
    /// shows (its width over the narrowest glyph), and the cursor's x from
    /// the view's start (`lumen_edit::view`).
    first: usize,
    max_chars: usize,
    cursor_x: u32,
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
        let (ed, loaded) = Editor::new(text, LIMITS);
        if loaded.cut {
            // The sample fits its limits (a const assertion can't measure
            // it): a cut is a bug, said rather than shown short.
            log::error(&format!("editor: the text given was cut to {} bytes", ed.text().len()));
        }
        let area = ui.text_area(None, "Text", prefix(ed.text(), TEXT_BYTES), w, h);
        // As wide as its longest text, not the window's: its repaint (each
        // key's) costs the app by the pixel.
        let status_w = (desktop.measure(UI_TEXT_PX, WIDEST_STATUS)? + 4).min(w);
        let status = ui.fixed_label(None, "Status", "", status_w);
        // Inside the border, less the margin above and below.
        let rows = (h.saturating_sub(2 + 2 * TEXT_PAD as u32) / line.max(1)).max(1) as usize;
        // The narrowest glyph the view may show, and so the most chars it
        // shows (each text drawn or measured at most that many, so at most
        // `TEXT_BYTES`: 4 bytes a char).
        let mut narrowest = u32::MAX;
        for c in NARROW {
            narrowest = narrowest.min(desktop.measure(UI_TEXT_PX, c)?);
        }
        let max_chars = ((view_width(w) / narrowest.max(1)) as usize + 2).min(TEXT_BYTES / 4);
        let mut app = EditorApp {
            ui,
            area,
            status,
            ed,
            desktop,
            line,
            rows,
            first: 0,
            max_chars,
            cursor_x: 0,
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

    /// Where the view's text starts across.
    fn text_x(&self) -> i32 {
        self.inner().x() + TEXT_PAD
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

    /// The view across scrolled so the cursor shows (`lumen_edit::view::
    /// across`: every text measured at most the view's chars), the
    /// cursor's x measured: whether the view moved.
    fn across(&mut self) -> bool {
        let width = view_width(self.ui.bounds(self.area).w());
        let (line, col) = self.ed.line_col();
        let desktop = self.desktop;
        let mut refused = None;
        let a = lumen_edit::view::across(self.ed.line(line), col, self.first, self.max_chars, width, |t| {
            desktop.measure(UI_TEXT_PX, t).unwrap_or_else(|e| {
                refused = Some(e);
                0
            })
        });
        if let Some(e) = refused
            && !self.warned
        {
            self.warned = true;
            log::warn(&format!("editor: measure refused: {e}"));
        }
        let moved = a.first != self.first;
        (self.first, self.cursor_x) = (a.first, a.x);
        moved
    }

    /// Run `cmd`, then show what it changed. An edit at the cursor (typing,
    /// Tab, Backspace, Delete) changes its line only from the cursor's
    /// leftmost place on: that part of the line is repainted, not all of it
    /// (the app pays by the pixel).
    fn command(&mut self, cmd: Command) {
        let old_cursor = self.cursor_rect();
        let change = self.ed.apply(cmd);
        let at_cursor = matches!(cmd, Command::Insert(_) | Command::Tab | Command::Backspace | Command::Delete);
        self.changed(change, old_cursor);
        if at_cursor
            && let Change::Line(l) = change
            && let (Some(band), Some(old), Some(new)) = (self.line_rect(l), old_cursor, self.cursor_rect())
            && self.drawn.last() == Some(&band)
        {
            // A few pixels before it too: a glyph reaches past where it
            // starts (its anti-aliased edge, a kerned pair), so one typed or
            // deleted at the cursor changes a pixel or two to its left.
            let x = (old.x().min(new.x()) - GLYPH_OVERHANG).max(band.x());
            self.drawn.pop();
            self.drawn.push(Rect::new(x, band.y(), (band.right() - x) as u32, band.h()));
        }
    }

    /// Mark what `change` changed to repaint (`old_cursor`: where the
    /// cursor was drawn), scroll to the cursor, and update the tree.
    fn changed(&mut self, change: Change, old_cursor: Option<Rect>) {
        // The tree's value follows the text: after an edit, and after a
        // `set_value` the editor took in differently (normalised: a tab as
        // spaces, `\r` as `\n`, other control characters dropped) or not at
        // all (the same text, or refused).
        self.ui.set_text(self.area, prefix(self.ed.text(), TEXT_BYTES));
        // A key keeps the cursor solid through the next tick.
        self.keyed = true;
        let was_on = self.cursor_on;
        self.cursor_on = true;
        if matches!(change, Change::None | Change::Refused) {
            if !was_on {
                self.damage_cursor();
            }
            // A refused edit says so (`, full`); the next command clears it.
            self.show_status();
            return;
        }
        let scrolled = self.ed.scroll_to(self.rows) | self.across();
        match change {
            _ if scrolled => self.damage_from(0),
            // The old and new places; on one line, one rect covering both,
            // so the line is drawn once (`paint_over`).
            Change::Cursor { .. } => match (old_cursor, self.cursor_rect()) {
                (Some(old), Some(new)) if old.y() == new.y() => self.drawn.push(old.union(&new)),
                (old, new) => self.drawn.extend(old.into_iter().chain(new)),
            },
            Change::Line(l) => {
                if let Some(r) = self.line_rect(l) {
                    self.drawn.push(r);
                }
            }
            Change::From(l) => self.damage_from(l),
            Change::None | Change::Refused => {}
        }
        self.show_status();
    }

    fn show_status(&mut self) {
        let text = self.ed.status();
        self.ui.set_text(self.status, &text);
    }

    /// The input the editor takes itself: its keys (whatever has the
    /// focus), and the window's focus (seen, then handed on to the toolkit).
    fn take(&mut self, e: &Event) -> Taken {
        match *e {
            Event::Key { code, mods, action: KeyAction::Press { text } } => match lumen_edit::command(code, mods, text)
            {
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

impl App for EditorApp {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        let mut then = Then::Carry;
        for e in events {
            // `set_value` on the text: one edit replacing it all.
            if let UiEvent::Edited(w) = e
                && *w == self.area
            {
                let old_cursor = self.cursor_rect();
                // A text area always has a text: none would be a bug, said
                // (an ERROR, the request answered failed), never taken as
                // an empty text wiping the buffer.
                let Some(text) = self.ui.text(self.area).map(String::from) else {
                    log::error("editor: the text area has no text");
                    then = Then::Failed(String::from("the text area has no text"));
                    continue;
                };
                let change = self.ed.replace_all(&text);
                self.changed(change, old_cursor);
                if change == Change::Refused {
                    // Past the editor's limits (a `set_value` fits them: the
                    // assertions above), answered so, the text as it was.
                    then = Then::Failed(String::from("text too long"));
                }
            }
        }
        then
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        let x = self.text_x();
        let top = self.ed.top();
        let cursor = if self.cursor_shown() { self.cursor_rect() } else { None };
        for row in 0..self.rows {
            let band = self.row_rect(row);
            // Only what the view can show of the line (the host charges
            // `text_draw` by the character, whatever reaches the clip).
            let text = lumen_edit::view::slice(self.ed.line(top + row), self.first, self.max_chars, TEXT_BYTES);
            // Drawn inside each rectangle being repainted that meets its
            // band, never across the gap between two (pixels not repainted
            // would be blended twice, showing at a later commit). The damage
            // gives a band one rectangle (a line, the cursor's places on it
            // merged, every row from one on), so a line is drawn once.
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
