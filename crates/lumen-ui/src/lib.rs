//! The desktop's toolkit (#156; ARCHITECTURE section 11, "Toolkit and the
//! automation tree"): a retained widget tree in which every widget is also
//! an automation node, made from the widget itself, so the tree can't drift
//! from what is drawn. Host-tested, `no_std` with `alloc`, no locks.
//!
//! - **Widgets** ([`Ui`]): stacks ([`Ui::vstack`], [`Ui::hstack`]), labels,
//!   buttons, checkboxes, single-line text fields and lists; laid out at
//!   their preferred sizes with padding (integer math); hit testing; the
//!   primary button's press and release, keys to the focused widget (Tab
//!   moves the focus); painted into a `lumen-gfx` `Surface`, with text
//!   measured and drawn through [`Text`] (the kernel's `lumen-font`, or
//!   [`FixedText`] in tests).
//! - **The automation tree** ([`node`]): [`NodeId`] (one counter for the
//!   whole system), [`Role`] (its actions, value kind and flags),
//!   [`Value`], [`Action`], [`StateFlags`] and [`AutomationNode`], one
//!   constructor per role. [`Ui::nodes`] gives a window's nodes; its root
//!   (role `window`) is the compositor's.
//! - **Actions** ([`Ui::invoke`]) run the handler a real click or key
//!   reaches, so `press` and a click leave the same state; errors change
//!   nothing.
//! - **An app** ([`App`]): a window's tree and what it does with what the
//!   tree reports; [`serve`] runs a request on it (the same handler a click
//!   reaches) and says what to answer once the tree is published.
//! - **Counter** ([`counter`]): the demo app, its logic here so tests
//!   drive the app the scenarios use.
//! - **A request's reply** ([`Reply`]): the state machine of `ui invoke`'s
//!   reply slot (`Pending`, `Running`, `Cancelled`, `Done`, `Taken`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod app;
pub mod counter;
pub mod node;
pub mod reply;
pub mod text;
pub mod ui;

#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod tests;

pub use app::{App, Repainted, Served, Then, repaint, serve};
pub use node::{Action, ActionKind, AutomationNode, InvokeError, NodeId, Role, StateFlags, Value, ValueKind};
pub use reply::{First, NotStarted, Phase, Reply, Second};
pub use text::{FixedText, Text};
pub use ui::{Input, Key, Palette, Repaint, Stack, Ui, UiEvent, Widget};
