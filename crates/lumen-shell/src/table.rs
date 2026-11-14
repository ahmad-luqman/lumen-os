use crate::Complete;
use alloc::string::{String, ToString};

/// A command: its name, the words each of its arguments can be (for
/// completion; an empty list completes nothing there), and what runs it.
#[derive(Debug)]
pub struct Command<T: 'static> {
    pub name: &'static str,
    /// For each argument in turn, the words it can be.
    pub args: &'static [&'static [&'static str]],
    pub run: T,
}

/// The commands a shell knows. It completes their names (the first word)
/// and their arguments' words.
#[derive(Debug)]
pub struct Table<T: 'static>(pub &'static [Command<T>]);

impl<T> Table<T> {
    /// The command called `name`.
    pub fn find(&self, name: &str) -> Option<&'static Command<T>> {
        self.0.iter().find(|c| c.name == name)
    }
}

impl<T> Complete for Table<T> {
    fn options(&self, words: &[String], out: &mut alloc::vec::Vec<String>) {
        match words {
            [] => out.extend(self.0.iter().map(|c| c.name.to_string())),
            [name, args @ ..] => {
                if let Some(choices) = self.find(name).and_then(|c| c.args.get(args.len())) {
                    out.extend(choices.iter().map(|w| w.to_string()));
                }
            }
        }
    }
}
