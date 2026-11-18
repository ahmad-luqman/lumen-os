//! The widget tree ([`Ui`]): building it, laying it out, hit testing,
//! dispatching pointer presses and keys, painting it, and the automation
//! tree made from it ([`Ui::nodes`]), with actions ([`Ui::invoke`]) run by
//! the same handler a click or a key reaches.

use alloc::string::String;
use alloc::vec::Vec;

use lumen_gfx::{Rect, Surface};

use crate::node::{Action, ActionKind, AutomationNode, InvokeError, NodeId, Role};
use crate::text::Text;

/// Padding around the whole tree (stacks add none of their own).
pub const PAD: u32 = 8;
/// Space between a stack's children.
pub const GAP: u32 = 8;
/// A button's padding around its name.
const BUTTON_PAD_X: u32 = 12;
const BUTTON_PAD_Y: u32 = 6;
/// A checkbox's box, and the space before its name.
const BOX: u32 = 14;
const BOX_GAP: u32 = 6;
/// A text field's width, and its padding around the text.
const FIELD_W: u32 = 160;
const FIELD_PAD: u32 = 4;
/// A list row's padding around its item.
const ROW_PAD: u32 = 2;

/// A widget of one [`Ui`] (an index into it: meaningless in another).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Widget(usize);

/// The colours a [`Ui`] paints with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The window's background.
    pub background: u32,
    pub text: u32,
    /// Buttons, a checked box, a selected list row.
    pub accent: u32,
    pub accent_text: u32,
    pub border: u32,
    /// A disabled widget's text and a disabled button.
    pub disabled: u32,
    /// A text field's and a list's background.
    pub field: u32,
}

/// Input a window's owner hands to its [`Ui`] (client coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The primary button went down.
    Down {
        x: i32,
        y: i32,
    },
    /// The primary button came up.
    Up {
        x: i32,
        y: i32,
    },
    /// The press was forgotten (the window lost the focus, or events were
    /// lost): nothing it armed is activated.
    Cancel,
    Key(Key),
}

/// A key press, as the toolkit takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A press's text: a text field inserts it (not a control character);
    /// a space or Enter activates a focused button or checkbox.
    Char(char),
    Backspace,
    Enter,
    /// Focus the next (or, `BackTab`, the previous) widget that takes it.
    Tab,
    BackTab,
}

/// What happened, for the owner to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    Pressed(Widget),
    Toggled(Widget, bool),
    Edited(Widget),
    Selected(Widget, usize),
    Focused(Widget),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Down,
    Across,
}

#[derive(Clone, Debug)]
enum Kind {
    Stack(Dir),
    Label(String),
    Button,
    Checkbox(bool),
    TextField(String),
    List { items: Vec<(NodeId, String)>, selected: Option<usize> },
}

#[derive(Clone, Debug)]
struct W {
    id: NodeId,
    name: String,
    kind: Kind,
    disabled: bool,
    bounds: Rect,
    children: Vec<usize>,
}

impl W {
    fn role(&self) -> Role {
        match self.kind {
            Kind::Stack(_) => Role::Group,
            Kind::Label(_) => Role::Label,
            Kind::Button => Role::Button,
            Kind::Checkbox(_) => Role::Checkbox,
            Kind::TextField(_) => Role::TextField,
            Kind::List { .. } => Role::List,
        }
    }

    /// Whether it takes the focus (a click, Tab, `focus`).
    fn focusable(&self) -> bool {
        self.role().actions().contains(&ActionKind::Focus)
    }

    /// List row `i`'s bounds.
    fn row(&self, i: usize, rows: usize) -> Rect {
        let h = self.bounds.h() / rows.max(1) as u32;
        Rect::new(self.bounds.x(), self.bounds.y() + (i as u32 * h) as i32, self.bounds.w(), h)
    }
}

/// A retained widget tree: see the module's doc. `Clone` only in tests: a
/// copy would share its widgets' node ids.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(Clone))]
pub struct Ui {
    widgets: Vec<W>,
    /// The widgets with no parent, top to bottom.
    top: Vec<usize>,
    focus: Option<usize>,
    /// The widget (and list row) a press went down on.
    armed: Option<(usize, Option<usize>)>,
    window_focused: bool,
    changed: bool,
}

impl Ui {
    pub fn new() -> Ui {
        Ui::default()
    }

    fn add(&mut self, parent: Option<Widget>, name: &str, kind: Kind) -> Widget {
        let i = self.widgets.len();
        self.widgets.push(W {
            id: NodeId::fresh(),
            name: String::from(name),
            kind,
            disabled: false,
            bounds: Rect::new(0, 0, 0, 0),
            children: Vec::new(),
        });
        match parent {
            Some(Widget(p)) if matches!(self.widgets[p].kind, Kind::Stack(_)) => self.widgets[p].children.push(i),
            _ => self.top.push(i),
        }
        self.changed = true;
        Widget(i)
    }

    /// A stack whose children go top to bottom.
    pub fn vstack(&mut self, parent: Option<Widget>, name: &str) -> Widget {
        self.add(parent, name, Kind::Stack(Dir::Down))
    }

    /// A stack whose children go left to right.
    pub fn hstack(&mut self, parent: Option<Widget>, name: &str) -> Widget {
        self.add(parent, name, Kind::Stack(Dir::Across))
    }

    pub fn label(&mut self, parent: Option<Widget>, name: &str, text: &str) -> Widget {
        self.add(parent, name, Kind::Label(String::from(text)))
    }

    pub fn button(&mut self, parent: Option<Widget>, name: &str) -> Widget {
        self.add(parent, name, Kind::Button)
    }

    pub fn checkbox(&mut self, parent: Option<Widget>, name: &str, checked: bool) -> Widget {
        self.add(parent, name, Kind::Checkbox(checked))
    }

    pub fn text_field(&mut self, parent: Option<Widget>, name: &str, text: &str) -> Widget {
        self.add(parent, name, Kind::TextField(String::from(text)))
    }

    pub fn list(&mut self, parent: Option<Widget>, name: &str, items: &[&str]) -> Widget {
        let items = items.iter().map(|s| (NodeId::fresh(), String::from(*s))).collect();
        self.add(parent, name, Kind::List { items, selected: None })
    }

    pub fn id(&self, w: Widget) -> NodeId {
        self.widgets[w.0].id
    }

    pub fn bounds(&self, w: Widget) -> Rect {
        self.widgets[w.0].bounds
    }

    /// A label's or a text field's text.
    pub fn text(&self, w: Widget) -> Option<&str> {
        match &self.widgets[w.0].kind {
            Kind::Label(t) | Kind::TextField(t) => Some(t),
            _ => None,
        }
    }

    /// Set a label's or a text field's text (others ignore it).
    pub fn set_text(&mut self, w: Widget, text: &str) {
        if let Kind::Label(t) | Kind::TextField(t) = &mut self.widgets[w.0].kind
            && t != text
        {
            *t = String::from(text);
            self.changed = true;
        }
    }

    pub fn checked(&self, w: Widget) -> Option<bool> {
        match self.widgets[w.0].kind {
            Kind::Checkbox(c) => Some(c),
            _ => None,
        }
    }

    pub fn selected(&self, w: Widget) -> Option<usize> {
        match &self.widgets[w.0].kind {
            Kind::List { selected, .. } => *selected,
            _ => None,
        }
    }

    pub fn is_disabled(&self, w: Widget) -> bool {
        self.widgets[w.0].disabled
    }

    /// Disable (or enable) a widget: a disabled one takes no click, key or
    /// action, and loses the focus.
    pub fn set_disabled(&mut self, w: Widget, disabled: bool) {
        if self.widgets[w.0].disabled != disabled {
            self.widgets[w.0].disabled = disabled;
            if disabled && self.focus == Some(w.0) {
                self.focus = None;
            }
            if disabled && self.armed.is_some_and(|(a, _)| a == w.0) {
                self.armed = None;
            }
            self.changed = true;
        }
    }

    /// The widget with the focus.
    pub fn focused(&self) -> Option<Widget> {
        self.focus.map(Widget)
    }

    /// Whether the window has the focus (the focused widget shows it, and
    /// carries `focused`, only then).
    pub fn set_window_focused(&mut self, focused: bool) {
        if self.window_focused != focused {
            self.window_focused = focused;
            self.changed = true;
        }
    }

    /// Whether anything shown changed since the last call (the owner then
    /// repaints, commits and publishes its tree).
    pub fn take_changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// Mark the whole tree changed: repainted at the next
    /// [`Ui::take_changed`] (a new palette, say).
    pub fn invalidate(&mut self) {
        self.changed = true;
    }

    // -- layout --------------------------------------------------------------

    /// Each widget at its preferred size, the top ones down from `area`'s
    /// corner (padded), each stack's children one after another. Marks the
    /// tree changed only if a widget moved or changed size.
    pub fn layout(&mut self, area: Rect, text: &mut dyn Text) {
        // A widget's children come after it (made after their stack), so one
        // pass from the last widget to the first sizes every child first.
        let mut sizes = alloc::vec![(0u32, 0u32); self.widgets.len()];
        for i in (0..self.widgets.len()).rev() {
            sizes[i] = self.size(i, text, &sizes);
        }
        let before: Vec<Rect> = self.widgets.iter().map(|w| w.bounds).collect();
        let x = area.x() + PAD as i32;
        let mut y = area.y() + PAD as i32;
        let top = self.top.clone();
        for i in top {
            self.place(i, x, y, &sizes);
            y += (sizes[i].1 + GAP) as i32;
        }
        if self.widgets.iter().zip(&before).any(|(w, b)| w.bounds != *b) {
            self.changed = true;
        }
    }

    /// Widget `i`'s preferred size, its children's in `sizes` already.
    fn size(&self, i: usize, text: &mut dyn Text, sizes: &[(u32, u32)]) -> (u32, u32) {
        let line = text.line_height();
        let w = &self.widgets[i];
        match &w.kind {
            Kind::Stack(dir) => {
                let mut size = (0u32, 0u32);
                for (n, &c) in w.children.iter().enumerate() {
                    debug_assert!(c > i, "a child made before its stack");
                    let (cw, ch) = sizes[c];
                    let gap = if n == 0 { 0 } else { GAP };
                    size = match dir {
                        Dir::Down => (size.0.max(cw), size.1 + gap + ch),
                        Dir::Across => (size.0 + gap + cw, size.1.max(ch)),
                    };
                }
                size
            }
            Kind::Label(t) => (text.width(t), line),
            Kind::Button => (text.width(&w.name) + 2 * BUTTON_PAD_X, line + 2 * BUTTON_PAD_Y),
            Kind::Checkbox(_) => (BOX + BOX_GAP + text.width(&w.name), line.max(BOX) + 2 * BUTTON_PAD_Y),
            Kind::TextField(_) => (FIELD_W, line + 2 * FIELD_PAD),
            Kind::List { items, .. } => {
                let widest = items.iter().map(|(_, s)| text.width(s)).max().unwrap_or(0);
                (widest + 2 * FIELD_PAD, items.len() as u32 * (line + 2 * ROW_PAD))
            }
        }
    }

    fn place(&mut self, i: usize, x: i32, y: i32, sizes: &[(u32, u32)]) {
        let (w, h) = sizes[i];
        self.widgets[i].bounds = Rect::new(x, y, w, h);
        if let Kind::Stack(dir) = self.widgets[i].kind {
            let children = self.widgets[i].children.clone();
            let (mut cx, mut cy) = (x, y);
            for c in children {
                self.place(c, cx, cy, sizes);
                match dir {
                    Dir::Down => cy += (sizes[c].1 + GAP) as i32,
                    Dir::Across => cx += (sizes[c].0 + GAP) as i32,
                }
            }
        }
    }

    // -- hit testing and dispatch ----------------------------------------------

    /// The widget (not a stack) under (`x`, `y`), and the list row there.
    pub fn hit(&self, x: i32, y: i32) -> Option<(Widget, Option<usize>)> {
        self.widgets.iter().enumerate().find(|(_, w)| !matches!(w.kind, Kind::Stack(_)) && w.bounds.contains(x, y)).map(
            |(i, w)| {
                let row = match &w.kind {
                    Kind::List { items, .. } => (0..items.len()).find(|&r| w.row(r, items.len()).contains(x, y)),
                    _ => None,
                };
                (Widget(i), row)
            },
        )
    }

    /// Hand `input` to the tree: what happened.
    pub fn handle(&mut self, input: Input) -> Vec<UiEvent> {
        match input {
            Input::Down { x, y } => {
                self.armed = None;
                if let Some((Widget(i), row)) = self.hit(x, y)
                    && !self.widgets[i].disabled
                {
                    self.focus_on(i);
                    self.armed = Some((i, row));
                }
                Vec::new()
            }
            Input::Up { x, y } => match self.armed.take() {
                Some((i, row)) if self.hit(x, y) == Some((Widget(i), row)) => self.activate(i, row),
                _ => Vec::new(),
            },
            Input::Cancel => {
                self.armed = None;
                Vec::new()
            }
            Input::Key(k) => self.key(k),
        }
    }

    fn key(&mut self, k: Key) -> Vec<UiEvent> {
        match k {
            Key::Tab => self.focus_step(true),
            Key::BackTab => self.focus_step(false),
            _ => {
                let Some(i) = self.focus else { return Vec::new() };
                let pressable = matches!(self.widgets[i].kind, Kind::Button | Kind::Checkbox(_));
                if pressable && matches!(k, Key::Enter | Key::Char(' ')) {
                    return self.activate(i, None);
                }
                match (&mut self.widgets[i].kind, k) {
                    (Kind::TextField(t), Key::Char(c)) if !c.is_control() => {
                        t.push(c);
                        self.changed = true;
                        alloc::vec![UiEvent::Edited(Widget(i))]
                    }
                    (Kind::TextField(t), Key::Backspace) => {
                        if t.pop().is_some() {
                            self.changed = true;
                            alloc::vec![UiEvent::Edited(Widget(i))]
                        } else {
                            Vec::new()
                        }
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    /// Focus the next (or previous) enabled widget that takes the focus,
    /// in tree order, wrapping around.
    fn focus_step(&mut self, forward: bool) -> Vec<UiEvent> {
        let order: Vec<usize> = self.walk().into_iter().map(|(i, _)| i).filter(|&i| self.takes_focus(i)).collect();
        if order.is_empty() {
            return Vec::new();
        }
        let at = self.focus.and_then(|f| order.iter().position(|&i| i == f));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => order.len() - 1,
            (Some(p), true) => (p + 1) % order.len(),
            (Some(p), false) => (p + order.len() - 1) % order.len(),
        };
        self.focus_on(order[next]);
        alloc::vec![UiEvent::Focused(Widget(order[next]))]
    }

    fn takes_focus(&self, i: usize) -> bool {
        self.widgets[i].focusable() && !self.widgets[i].disabled
    }

    fn focus_on(&mut self, i: usize) {
        if self.takes_focus(i) && self.focus != Some(i) {
            self.focus = Some(i);
            self.changed = true;
        }
    }

    /// What a click on widget `i` (list row `row`) does, and a `press` (or
    /// a list item's `select`) on it: the one handler both reach.
    fn activate(&mut self, i: usize, row: Option<usize>) -> Vec<UiEvent> {
        self.focus_on(i);
        let w = Widget(i);
        match &mut self.widgets[i].kind {
            Kind::Button => alloc::vec![UiEvent::Pressed(w)],
            Kind::Checkbox(c) => {
                *c = !*c;
                let now = *c;
                self.changed = true;
                alloc::vec![UiEvent::Toggled(w, now)]
            }
            Kind::List { selected, .. } => match row {
                Some(r) => {
                    *selected = Some(r);
                    self.changed = true;
                    alloc::vec![UiEvent::Selected(w, r)]
                }
                None => Vec::new(),
            },
            Kind::Stack(_) | Kind::Label(_) | Kind::TextField(_) => Vec::new(),
        }
    }

    /// Run `action` on the node numbered `node` (a widget, or a list's
    /// item): the handler a click or key reaches, so the state after is the
    /// same as after the click. Errors change nothing: no such node, an
    /// action its role doesn't list, a disabled node.
    pub fn invoke(&mut self, node: u64, action: &Action) -> Result<Vec<UiEvent>, InvokeError> {
        let (i, row) = self.find(node).ok_or(InvokeError::NoNode)?;
        let role = if row.is_some() { Role::ListItem } else { self.widgets[i].role() };
        let kind = action.kind();
        if !role.actions().contains(&kind) {
            return Err(InvokeError::NotAllowed(role, kind));
        }
        if self.widgets[i].disabled {
            return Err(InvokeError::Disabled);
        }
        match action {
            Action::Press | Action::Select => Ok(self.activate(i, row)),
            Action::Focus => {
                self.focus_on(i);
                Ok(alloc::vec![UiEvent::Focused(Widget(i))])
            }
            Action::SetValue(v) => {
                self.focus_on(i);
                let w = Widget(i);
                self.set_text(w, v);
                Ok(alloc::vec![UiEvent::Edited(w)])
            }
            // No widget role lists these.
            Action::Close | Action::Minimize | Action::Restore => Err(InvokeError::NotAllowed(role, kind)),
        }
    }

    /// The widget numbered `node`, or the list and row of an item.
    fn find(&self, node: u64) -> Option<(usize, Option<usize>)> {
        self.widgets.iter().enumerate().find_map(|(i, w)| {
            if w.id.get() == node {
                return Some((i, None));
            }
            match &w.kind {
                Kind::List { items, .. } => items.iter().position(|(id, _)| id.get() == node).map(|r| (i, Some(r))),
                _ => None,
            }
        })
    }

    /// The widgets in tree order (depth first, the top ones in turn), each
    /// with its depth (1 for a top one).
    fn walk(&self) -> Vec<(usize, u32)> {
        let mut out = Vec::with_capacity(self.widgets.len());
        let mut stack: Vec<(usize, u32)> = self.top.iter().rev().map(|&i| (i, 1)).collect();
        while let Some((i, depth)) = stack.pop() {
            out.push((i, depth));
            stack.extend(self.widgets[i].children.iter().rev().map(|&c| (c, depth + 1)));
        }
        out
    }

    // -- the automation tree ---------------------------------------------------

    /// The top widgets' node ids (a window root's children).
    pub fn top_ids(&self) -> Vec<NodeId> {
        self.top.iter().map(|&i| self.widgets[i].id).collect()
    }

    /// Every node, depth first, with its depth (1 for a top widget: the
    /// window's root, 0, is the compositor's). Client coordinates.
    pub fn nodes(&self) -> Vec<(u32, AutomationNode)> {
        let mut out = Vec::new();
        for (i, depth) in self.walk() {
            let w = &self.widgets[i];
            let focused = self.window_focused && self.focus == Some(i);
            let node = match &w.kind {
                Kind::Stack(_) => {
                    let children = w.children.iter().map(|&c| self.widgets[c].id).collect();
                    AutomationNode::group(w.id, &w.name, w.bounds, w.disabled, children)
                }
                Kind::Label(t) => AutomationNode::label(w.id, &w.name, t, w.bounds, w.disabled),
                Kind::Button => AutomationNode::button(w.id, &w.name, w.bounds, focused, w.disabled),
                Kind::Checkbox(c) => AutomationNode::checkbox(w.id, &w.name, *c, w.bounds, focused, w.disabled),
                Kind::TextField(t) => AutomationNode::textfield(w.id, &w.name, t, w.bounds, focused, w.disabled),
                Kind::List { items, selected } => {
                    let ids = items.iter().map(|(id, _)| *id).collect();
                    AutomationNode::list(w.id, &w.name, ids, *selected, w.bounds, focused, w.disabled)
                }
            };
            out.push((depth, node));
            if let Kind::List { items, selected } = &w.kind {
                for (r, (id, name)) in items.iter().enumerate() {
                    let row = w.row(r, items.len());
                    out.push((depth + 1, AutomationNode::list_item(*id, name, r, *selected, row, w.disabled)));
                }
            }
        }
        out
    }

    /// The node numbered `node`, with its depth.
    pub fn node(&self, node: u64) -> Option<(u32, AutomationNode)> {
        self.nodes().into_iter().find(|(_, n)| n.id().get() == node)
    }

    // -- painting --------------------------------------------------------------

    /// Paint the whole tree into `s` (its background first).
    pub fn paint(&self, s: &mut Surface, text: &mut dyn Text, p: &Palette) {
        s.fill(p.background);
        let line = text.line_height();
        for (i, w) in self.widgets.iter().enumerate() {
            let b = w.bounds;
            let fg = if w.disabled { p.disabled } else { p.text };
            let focused = self.window_focused && self.focus == Some(i);
            match &w.kind {
                Kind::Stack(_) => {}
                Kind::Label(t) => text.draw(s, b.x(), b.y(), t, fg, b),
                Kind::Button => {
                    let (bg, ink) = if w.disabled { (p.disabled, p.background) } else { (p.accent, p.accent_text) };
                    s.fill_rect(b, bg);
                    let x = b.x() + centred(b.w(), text.width(&w.name));
                    text.draw(s, x, b.y() + centred(b.h(), line), &w.name, ink, b);
                }
                Kind::Checkbox(checked) => {
                    let boxed = Rect::new(b.x(), b.y() + centred(b.h(), BOX), BOX, BOX);
                    s.fill_rect(boxed, p.field);
                    stroke(s, boxed, p.border);
                    if *checked {
                        s.fill_rect(Rect::new(boxed.x() + 3, boxed.y() + 3, BOX - 6, BOX - 6), p.accent);
                    }
                    let x = b.x() + (BOX + BOX_GAP) as i32;
                    text.draw(s, x, b.y() + centred(b.h(), line), &w.name, fg, b);
                }
                Kind::TextField(t) => {
                    s.fill_rect(b, p.field);
                    stroke(s, b, p.border);
                    text.draw(s, b.x() + FIELD_PAD as i32, b.y() + FIELD_PAD as i32, t, fg, b);
                }
                Kind::List { items, selected } => {
                    s.fill_rect(b, p.field);
                    for (r, (_, name)) in items.iter().enumerate() {
                        let row = w.row(r, items.len());
                        let ink = if *selected == Some(r) {
                            s.fill_rect(row, p.accent);
                            p.accent_text
                        } else {
                            fg
                        };
                        text.draw(s, row.x() + FIELD_PAD as i32, row.y() + ROW_PAD as i32, name, ink, row);
                    }
                    stroke(s, b, p.border);
                }
            }
            if focused {
                // A ring just outside it (inside the gap to its neighbours).
                let ring = Rect::new(b.x() - 2, b.y() - 2, b.w() + 4, b.h() + 4);
                stroke(s, ring, p.text);
            }
        }
    }
}

/// Where something `inner` long starts to sit centred in `outer`.
fn centred(outer: u32, inner: u32) -> i32 {
    (outer as i32 - inner as i32) / 2
}

/// A one-pixel outline of `r`.
fn stroke(s: &mut Surface, r: Rect, color: u32) {
    if r.w() == 0 || r.h() == 0 {
        return;
    }
    s.fill_rect(Rect::new(r.x(), r.y(), r.w(), 1), color);
    s.fill_rect(Rect::new(r.x(), r.bottom() - 1, r.w(), 1), color);
    s.fill_rect(Rect::new(r.x(), r.y(), 1, r.h()), color);
    s.fill_rect(Rect::new(r.right() - 1, r.y(), 1, r.h()), color);
}
