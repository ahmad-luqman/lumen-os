//! "Counter", the toolkit's demo app (#156; the kernel's `desktop counter`
//! opens it in a window): a count, an Add and a Reset button, a Lock
//! checkbox that disables Add, and a Quit button that closes the window.
//! Its logic lives here so host tests drive the very app the scenarios do.

use alloc::string::ToString;
use alloc::vec::Vec;

use crate::ui::{Ui, UiEvent, Widget};

/// The app: its tree and the widgets it acts on.
#[derive(Clone, Debug)]
pub struct Counter {
    ui: Ui,
    count: u64,
    pub count_label: Widget,
    pub add: Widget,
    pub reset: Widget,
    pub lock: Widget,
    pub quit: Widget,
}

/// What the owner does after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Carry on (repaint if the tree changed).
    Carry,
    /// Quit was pressed: close the window.
    Quit,
}

impl Default for Counter {
    fn default() -> Self {
        Counter::new()
    }
}

impl Counter {
    /// The window's tree: a row with the caption and the count, a row of
    /// Add, Reset and Lock, then Quit.
    pub fn new() -> Counter {
        let mut ui = Ui::new();
        let top = ui.hstack(None, "Display");
        ui.label(Some(top), "Caption", "Count:");
        let count_label = ui.label(Some(top), "Count", "0");
        let row = ui.hstack(None, "Controls");
        let add = ui.button(Some(row), "Add");
        let reset = ui.button(Some(row), "Reset");
        let lock = ui.checkbox(Some(row), "Lock", false);
        let quit = ui.button(None, "Quit");
        Counter { ui, count: 0, count_label, add, reset, lock, quit }
    }

    pub fn ui(&self) -> &Ui {
        &self.ui
    }

    pub fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Act on what the tree reported.
    pub fn apply(&mut self, events: &[UiEvent]) -> Then {
        let mut then = Then::Carry;
        for e in events {
            match *e {
                UiEvent::Pressed(w) if w == self.add => self.set_count(self.count + 1),
                UiEvent::Pressed(w) if w == self.reset => self.set_count(0),
                UiEvent::Pressed(w) if w == self.quit => then = Then::Quit,
                UiEvent::Toggled(w, on) if w == self.lock => self.ui.set_disabled(self.add, on),
                _ => {}
            }
        }
        then
    }

    fn set_count(&mut self, n: u64) {
        self.count = n;
        self.ui.set_text(self.count_label, &n.to_string());
    }

    /// The node ids of its buttons by name (for tests).
    pub fn buttons(&self) -> Vec<(&'static str, u64)> {
        [("Add", self.add), ("Reset", self.reset), ("Quit", self.quit)]
            .into_iter()
            .map(|(n, w)| (n, self.ui.id(w).get()))
            .collect()
    }
}
