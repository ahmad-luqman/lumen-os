use alloc::collections::VecDeque;
use alloc::string::{String, ToString};

/// The lines entered, oldest first, up to a capacity (the oldest go first).
/// Blank lines and a repeat of the newest aren't kept. Each line keeps its
/// number (1 for the first ever entered) when older ones go.
#[derive(Clone, Debug)]
pub struct History {
    lines: VecDeque<String>,
    capacity: usize,
    /// How many lines went (the oldest kept is number `dropped + 1`).
    dropped: usize,
}

impl History {
    pub fn new(capacity: usize) -> History {
        History { lines: VecDeque::with_capacity(capacity), capacity, dropped: 0 }
    }

    /// Keep `line`, unless it is blank or the same as the newest.
    pub fn push(&mut self, line: &str) {
        if self.capacity == 0 || line.trim().is_empty() || self.lines.back().is_some_and(|l| l == line) {
            return;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line.to_string());
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The `i`th line kept (0: the oldest).
    pub fn get(&self, i: usize) -> Option<&str> {
        self.lines.get(i).map(String::as_str)
    }

    /// The lines kept with their numbers, oldest first.
    pub fn numbered(&self) -> impl Iterator<Item = (usize, &str)> {
        self.lines.iter().enumerate().map(|(i, l)| (self.dropped + i + 1, l.as_str()))
    }
}
