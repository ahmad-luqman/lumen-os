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

/// A widget of one [`Ui`] (meaningless in another): a slot and the
/// slot's generation. Once [`Ui::remove`]d a widget stays removed and its
/// node id names nothing; a later widget may take its slot, under a new
/// generation (and a new node id), so the old handle still reads removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Widget {
    slot: usize,
    generation: u32,
}

/// A stack (a widget whose children go one after another): the only kind
/// of widget a child can be added to, so a child can't land at the top
/// level by mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stack(Widget);

impl Stack {
    pub const fn widget(self) -> Widget {
        self.0
    }
}

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

/// What changed on the screen since the last [`Ui::take_damage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repaint {
    Nothing,
    /// Everything (a new palette, a widget that moved).
    All,
    /// Only these rectangles (client coordinates): each covers a changed
    /// widget (with its focus ring, if it takes the focus).
    Rects(Vec<Rect>),
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
    /// A menu unfolded (`true`) or folded.
    Expanded(Widget, bool),
    /// A menu's item was chosen (the menu folds).
    Chosen(Widget, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Down,
    Across,
}

/// A sub-row of a widget with a node of its own: a list's item, a menu's
/// item.
#[derive(Clone, Debug)]
struct Item {
    id: NodeId,
    name: String,
    bounds: Rect,
    /// A menu item's width, measured at each layout.
    width: u32,
}

impl Item {
    fn new(name: &str) -> Item {
        Item { id: NodeId::fresh(), name: String::from(name), bounds: Rect::EMPTY, width: 0 }
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Stack(Dir),
    /// A label; `width`: a fixed width (the text clipped to it), else the
    /// text's own.
    Label {
        text: String,
        width: Option<u32>,
    },
    Button,
    Checkbox(bool),
    TextField(String),
    List {
        items: Vec<Item>,
        selected: Option<usize>,
    },
    /// A button-like head (its name) whose items follow it, across, while
    /// it is expanded.
    Menu {
        items: Vec<Item>,
        expanded: bool,
        head: Rect,
    },
}

#[derive(Clone, Debug)]
struct W {
    id: NodeId,
    /// Bumped each time the slot is taken again.
    generation: u32,
    name: String,
    kind: Kind,
    disabled: bool,
    removed: bool,
    /// Laid out with no room (past a bar's pinned widget): bounds empty,
    /// not hit, takes no action, listed `hidden`.
    hidden: bool,
    bounds: Rect,
    children: Vec<usize>,
    parent: Option<usize>,
}

impl W {
    /// A removed widget's slot, whole (nothing of the widget outlives it),
    /// keeping only the generation for the next to bump.
    fn tombstone(generation: u32) -> W {
        W {
            id: NodeId::fresh(),
            generation,
            name: String::new(),
            kind: Kind::Label { text: String::new(), width: None },
            disabled: false,
            removed: true,
            hidden: false,
            bounds: Rect::EMPTY,
            children: Vec::new(),
            parent: None,
        }
    }

    fn role(&self) -> Role {
        match self.kind {
            Kind::Stack(_) => Role::Group,
            Kind::Label { .. } => Role::Label,
            Kind::Button => Role::Button,
            Kind::Checkbox(_) => Role::Checkbox,
            Kind::TextField(_) => Role::TextField,
            Kind::List { .. } => Role::List,
            Kind::Menu { .. } => Role::Menu,
        }
    }

    /// Whether it takes the focus (a click, Tab, `focus`).
    fn focusable(&self) -> bool {
        self.role().actions().contains(&ActionKind::Focus)
    }

    /// The rectangle it paints in: its bounds, and the focus ring around
    /// them if it takes the focus.
    fn painted(&self) -> Rect {
        let b = self.bounds;
        if b.is_empty() || !self.focusable() { b } else { ring(b) }
    }

    /// The items shown (a list's; an expanded menu's).
    fn shown_items(&self) -> &[Item] {
        match &self.kind {
            Kind::List { items, .. } => items,
            Kind::Menu { items, expanded: true, .. } => items,
            _ => &[],
        }
    }
}

/// How far outside a widget its focus ring is drawn.
const RING: i32 = 2;

/// The focus ring around `r` ([`RING`] pixels outside it).
fn ring(r: Rect) -> Rect {
    Rect::new(r.x() - RING, r.y() - RING, r.w() + 2 * RING as u32, r.h() + 2 * RING as u32)
}

/// What a layout decided for one widget (compared before and after: a
/// change repaints everything).
#[derive(Debug, PartialEq, Eq)]
struct Geometry {
    bounds: Rect,
    head: Rect,
    items: Vec<Rect>,
    hidden: bool,
}

/// How a tree's top widgets are placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrange {
    /// Down from the area's corner.
    Down,
    /// Across, each centred in the area's height, `pinned` (a top widget)
    /// at the right edge and the others stopping short of it.
    Bar { pinned: Option<usize> },
}

/// A retained widget tree: see the module's doc. `Clone` only in tests: a
/// copy would share its widgets' node ids.
#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
pub struct Ui {
    widgets: Vec<W>,
    /// The widgets with no parent, in order.
    top: Vec<usize>,
    focus: Option<usize>,
    /// The widget (and item) a press went down on.
    armed: Option<(usize, Option<usize>)>,
    window_focused: bool,
    /// The tree (what [`Ui::nodes`] shows) changed.
    changed: bool,
    /// What to repaint: everything, or these rectangles.
    all: bool,
    damage: Vec<Rect>,
    /// Padding around the whole tree.
    pad: u32,
    arrange: Arrange,
    /// Slots of removed widgets, free to take again.
    free: Vec<usize>,
}

impl Default for Ui {
    fn default() -> Ui {
        Ui {
            widgets: Vec::new(),
            top: Vec::new(),
            focus: None,
            armed: None,
            window_focused: false,
            changed: false,
            all: true,
            damage: Vec::new(),
            pad: PAD,
            arrange: Arrange::Down,
            free: Vec::new(),
        }
    }
}

impl Ui {
    pub fn new() -> Ui {
        Ui::default()
    }

    /// A bar: the top widgets across (each centred in the area's height),
    /// `pad` pixels in from its edges (a taskbar).
    pub fn bar(pad: u32) -> Ui {
        Ui { pad, arrange: Arrange::Bar { pinned: None }, ..Ui::default() }
    }

    fn add(&mut self, parent: Option<Stack>, name: &str, kind: Kind) -> Widget {
        // A child of a stack removed is made removed (its slot freed again
        // at once): nothing shows it, its handle reads removed.
        let parent = match parent {
            None => None,
            Some(Stack(p)) => match self.at(p) {
                Some(p) => Some(p),
                None => return self.dead(),
            },
        };
        debug_assert!(parent.is_none_or(|p| matches!(self.widgets[p].kind, Kind::Stack(_))));
        let w = W {
            id: NodeId::fresh(),
            generation: 0,
            name: String::from(name),
            kind,
            disabled: false,
            removed: false,
            hidden: false,
            bounds: Rect::EMPTY,
            children: Vec::new(),
            parent,
        };
        let i = match self.free.pop() {
            Some(i) => {
                let generation = self.widgets[i].generation.wrapping_add(1);
                self.widgets[i] = W { generation, ..w };
                i
            }
            None => {
                self.widgets.push(w);
                self.widgets.len() - 1
            }
        };
        match parent {
            Some(p) => self.widgets[p].children.push(i),
            None => self.top.push(i),
        }
        self.changed = true;
        self.widget(i)
    }

    /// A handle that reads removed (for a child of a removed stack).
    fn dead(&mut self) -> Widget {
        let w = self.add(None, "", Kind::Label { text: String::new(), width: None });
        self.remove(w);
        w
    }

    /// The handle of the widget in slot `i` now.
    fn widget(&self, i: usize) -> Widget {
        Widget { slot: i, generation: self.widgets[i].generation }
    }

    /// `w`'s slot, if it is still there (not removed, its generation the
    /// slot's).
    fn at(&self, w: Widget) -> Option<usize> {
        self.widgets.get(w.slot).filter(|x| x.generation == w.generation && !x.removed).map(|_| w.slot)
    }

    /// A stack whose children go top to bottom.
    pub fn vstack(&mut self, parent: Option<Stack>, name: &str) -> Stack {
        Stack(self.add(parent, name, Kind::Stack(Dir::Down)))
    }

    /// A stack whose children go left to right.
    pub fn hstack(&mut self, parent: Option<Stack>, name: &str) -> Stack {
        Stack(self.add(parent, name, Kind::Stack(Dir::Across)))
    }

    pub fn label(&mut self, parent: Option<Stack>, name: &str, text: &str) -> Widget {
        self.add(parent, name, Kind::Label { text: String::from(text), width: None })
    }

    /// A label `width` pixels wide whatever its text (clipped to it): a
    /// change of text repaints only its bounds and never moves its
    /// neighbours (the taskbar's clock).
    pub fn fixed_label(&mut self, parent: Option<Stack>, name: &str, text: &str, width: u32) -> Widget {
        self.add(parent, name, Kind::Label { text: String::from(text), width: Some(width) })
    }

    pub fn button(&mut self, parent: Option<Stack>, name: &str) -> Widget {
        self.add(parent, name, Kind::Button)
    }

    pub fn checkbox(&mut self, parent: Option<Stack>, name: &str, checked: bool) -> Widget {
        self.add(parent, name, Kind::Checkbox(checked))
    }

    pub fn text_field(&mut self, parent: Option<Stack>, name: &str, text: &str) -> Widget {
        self.add(parent, name, Kind::TextField(String::from(text)))
    }

    pub fn list(&mut self, parent: Option<Stack>, name: &str, items: &[&str]) -> Widget {
        let items = items.iter().map(|s| Item::new(s)).collect();
        self.add(parent, name, Kind::List { items, selected: None })
    }

    /// A menu named `name` (its head shows the name) whose `items` follow
    /// it, across, while it is expanded (a press on the head toggles it;
    /// choosing an item folds it).
    pub fn menu(&mut self, parent: Option<Stack>, name: &str, items: &[&str]) -> Widget {
        let items = items.iter().map(|s| Item::new(s)).collect();
        self.add(parent, name, Kind::Menu { items, expanded: false, head: Rect::EMPTY })
    }

    /// Place top widget `w` at the area's right edge (in a [`Ui::bar`]);
    /// the others stop short of it, those that don't fit hidden.
    /// Whether it was pinned: only a top widget of a bar is.
    pub fn pin_right(&mut self, w: Widget) -> bool {
        match (self.at(w), &mut self.arrange) {
            (Some(i), Arrange::Bar { pinned }) if self.top.contains(&i) => {
                *pinned = Some(i);
                true
            }
            _ => false,
        }
    }

    /// Remove `w` and everything in it (a stack's children): from then on
    /// its node id names nothing, it isn't laid out, hit, painted or
    /// listed, its accessors read as empty and its setters do nothing.
    /// What it covered is repainted. Whether it was there to remove.
    pub fn remove(&mut self, w: Widget) -> bool {
        let Some(slot) = self.at(w) else { return false };
        let mut gone = alloc::vec![slot];
        let mut n = 0;
        while n < gone.len() {
            gone.extend(self.widgets[gone[n]].children.iter().copied());
            n += 1;
        }
        match self.widgets[slot].parent {
            Some(p) => self.widgets[p].children.retain(|&c| c != slot),
            None => self.top.retain(|&c| c != slot),
        }
        for &i in &gone {
            self.touch(i);
            // What it held freed now; the slot kept for the next widget.
            self.widgets[i] = W::tombstone(self.widgets[i].generation);
            self.free.push(i);
        }
        if self.focus.is_some_and(|f| gone.contains(&f)) {
            self.focus = None;
        }
        if self.armed.is_some_and(|(a, _)| gone.contains(&a)) {
            self.armed = None;
        }
        if let Arrange::Bar { pinned } = &mut self.arrange
            && pinned.is_some_and(|p| gone.contains(&p))
        {
            *pinned = None;
        }
        true
    }

    /// How many slots the tree holds (live and free), for the tests.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> usize {
        self.widgets.len()
    }

    pub fn is_removed(&self, w: Widget) -> bool {
        self.at(w).is_none()
    }

    /// Its node id (`None` once removed: it names nothing then).
    pub fn id(&self, w: Widget) -> Option<NodeId> {
        self.at(w).map(|i| self.widgets[i].id)
    }

    /// Its bounds (empty once removed, or while hidden: not fitting a bar).
    pub fn bounds(&self, w: Widget) -> Rect {
        self.at(w).map_or(Rect::EMPTY, |i| self.widgets[i].bounds)
    }

    /// A label's or a text field's text.
    pub fn text(&self, w: Widget) -> Option<&str> {
        match &self.widgets[self.at(w)?].kind {
            Kind::Label { text: t, .. } | Kind::TextField(t) => Some(t),
            _ => None,
        }
    }

    /// Set a label's or a text field's text (others ignore it).
    pub fn set_text(&mut self, w: Widget, text: &str) {
        let Some(i) = self.at(w) else { return };
        if let Kind::Label { text: t, .. } | Kind::TextField(t) = &mut self.widgets[i].kind
            && t != text
        {
            *t = String::from(text);
            self.touch(i);
        }
    }

    pub fn checked(&self, w: Widget) -> Option<bool> {
        match self.widgets[self.at(w)?].kind {
            Kind::Checkbox(c) => Some(c),
            _ => None,
        }
    }

    pub fn selected(&self, w: Widget) -> Option<usize> {
        match &self.widgets[self.at(w)?].kind {
            Kind::List { selected, .. } => *selected,
            _ => None,
        }
    }

    /// Whether a menu is expanded.
    pub fn expanded(&self, w: Widget) -> Option<bool> {
        match &self.widgets[self.at(w)?].kind {
            Kind::Menu { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }

    /// Fold (or unfold) a menu.
    pub fn set_expanded(&mut self, w: Widget, to: bool) {
        let Some(i) = self.at(w) else { return };
        if let Kind::Menu { expanded, .. } = &mut self.widgets[i].kind
            && *expanded != to
        {
            *expanded = to;
            if !to && self.armed.is_some_and(|(a, item)| a == i && item.is_some()) {
                self.armed = None;
            }
            self.touch(i);
        }
    }

    /// A list's or a menu's item `index`'s node id.
    pub fn item_id(&self, w: Widget, index: usize) -> Option<NodeId> {
        match &self.widgets[self.at(w)?].kind {
            Kind::List { items, .. } | Kind::Menu { items, .. } => items.get(index).map(|i| i.id),
            _ => None,
        }
    }

    pub fn is_disabled(&self, w: Widget) -> bool {
        self.at(w).is_some_and(|i| self.widgets[i].disabled)
    }

    /// Disable (or enable) a widget: a disabled one takes no click, key or
    /// action, and loses the focus.
    pub fn set_disabled(&mut self, w: Widget, disabled: bool) {
        let Some(i) = self.at(w) else { return };
        if self.widgets[i].disabled != disabled {
            self.widgets[i].disabled = disabled;
            if disabled && self.focus == Some(i) {
                self.focus = None;
            }
            if disabled && self.armed.is_some_and(|(a, _)| a == i) {
                self.armed = None;
            }
            self.touch(i);
        }
    }

    /// The widget with the focus.
    pub fn focused(&self) -> Option<Widget> {
        self.focus.map(|i| self.widget(i))
    }

    /// Whether the window has the focus (the focused widget shows it, and
    /// carries `focused`, only then).
    pub fn set_window_focused(&mut self, focused: bool) {
        if self.window_focused != focused {
            self.window_focused = focused;
            self.changed = true;
            if let Some(f) = self.focus {
                self.touch(f);
            }
        }
    }

    /// Whether the tree changed since the last call (the owner then
    /// publishes it; [`Ui::take_damage`] says what to repaint).
    pub fn take_changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// What to repaint since the last call: nothing, everything, or the
    /// rectangles of the widgets that changed.
    pub fn take_damage(&mut self) -> Repaint {
        let damage = core::mem::take(&mut self.damage);
        if core::mem::take(&mut self.all) {
            Repaint::All
        } else if damage.is_empty() {
            Repaint::Nothing
        } else {
            Repaint::Rects(damage)
        }
    }

    /// Mark the whole tree changed: repainted (all of it) and published at
    /// the next [`Ui::take_damage`] and [`Ui::take_changed`] (a new
    /// palette, say).
    pub fn invalidate(&mut self) {
        self.changed = true;
        self.all = true;
    }

    /// Widget `i` changed: the tree changed, and its painted rectangle is
    /// damaged.
    fn touch(&mut self, i: usize) {
        self.changed = true;
        let r = self.widgets[i].painted();
        if !r.is_empty() && !self.all {
            if self.damage.len() >= MAX_DAMAGE {
                self.all = true;
                self.damage.clear();
            } else {
                self.damage.push(r);
            }
        }
    }

    // -- layout --------------------------------------------------------------

    /// Each widget at its preferred size, the top ones from `area`'s
    /// corner (padded), down (or, in a [`Ui::bar`], across, with the pinned
    /// one at the right), each stack's children one after another. A
    /// widget that doesn't fit across is hidden (its bounds empty). If a
    /// widget moved or changed size, everything is repainted and the tree
    /// changed.
    pub fn layout(&mut self, area: Rect, text: &mut dyn Text) {
        let before = self.geometry();
        // Twice at most: a menu hidden in the first pass is folded, and its
        // smaller size may fit in the second.
        for _ in 0..2 {
            if !self.place_all(area, text) {
                break;
            }
        }
        if self.geometry() != before {
            self.changed = true;
            self.all = true;
        }
    }

    /// What layout decides: each widget's bounds, a menu's head, the items
    /// shown, whether it is hidden.
    fn geometry(&self) -> Vec<Geometry> {
        self.widgets
            .iter()
            .map(|w| Geometry {
                bounds: w.bounds,
                head: match &w.kind {
                    Kind::Menu { head, .. } => *head,
                    _ => Rect::EMPTY,
                },
                items: w.shown_items().iter().map(|i| i.bounds).collect(),
                hidden: w.hidden,
            })
            .collect()
    }

    /// One layout pass; whether it folded a menu it hid.
    fn place_all(&mut self, area: Rect, text: &mut dyn Text) -> bool {
        // Each widget after everything in it (the tree's order reversed:
        // a reused slot may come before its stack's).
        let mut sizes = alloc::vec![(0u32, 0u32); self.widgets.len()];
        for (i, _) in self.walk().into_iter().rev() {
            sizes[i] = self.size(i, text, &sizes);
        }
        self.measure_menus(text);
        let pad = self.pad as i32;
        let top = self.top.clone();
        let mut folded = false;
        match self.arrange {
            Arrange::Bar { pinned } => {
                let right = area.right() - pad;
                let centred_y = |h: u32| area.y() + centred(area.h(), h);
                let limit = match pinned {
                    Some(p) => {
                        let x = right - sizes[p].0 as i32;
                        folded |= self.place(p, x, centred_y(sizes[p].1), &sizes, right);
                        x - GAP as i32
                    }
                    None => right,
                };
                let mut x = area.x() + pad;
                for i in top.into_iter().filter(|&i| Some(i) != pinned) {
                    folded |= self.place(i, x, centred_y(sizes[i].1), &sizes, limit);
                    x += (sizes[i].0 + GAP) as i32;
                }
            }
            Arrange::Down => {
                let x = area.x() + pad;
                let mut y = area.y() + pad;
                for i in top {
                    folded |= self.place(i, x, y, &sizes, i32::MAX);
                    y += (sizes[i].1 + GAP) as i32;
                }
            }
        }
        folded
    }

    /// Widget `i`'s preferred size, its children's in `sizes` already.
    fn size(&self, i: usize, text: &mut dyn Text, sizes: &[(u32, u32)]) -> (u32, u32) {
        let line = text.line_height();
        let w = &self.widgets[i];
        match &w.kind {
            Kind::Stack(dir) => {
                let mut size = (0u32, 0u32);
                for (n, &c) in w.children.iter().enumerate() {
                    let (cw, ch) = sizes[c];
                    let gap = if n == 0 { 0 } else { GAP };
                    size = match dir {
                        Dir::Down => (size.0.max(cw), size.1 + gap + ch),
                        Dir::Across => (size.0 + gap + cw, size.1.max(ch)),
                    };
                }
                size
            }
            Kind::Label { text: t, width } => (width.unwrap_or_else(|| text.width(t)), line),
            Kind::Button => button_size(text, &w.name),
            Kind::Checkbox(_) => (BOX + BOX_GAP + text.width(&w.name), line.max(BOX) + 2 * BUTTON_PAD_Y),
            Kind::TextField(_) => (FIELD_W, line + 2 * FIELD_PAD),
            Kind::List { items, .. } => {
                let widest = items.iter().map(|i| text.width(&i.name)).max().unwrap_or(0);
                (widest + 2 * FIELD_PAD, items.len() as u32 * (line + 2 * ROW_PAD))
            }
            Kind::Menu { items, expanded, .. } => {
                let (mut mw, h) = button_size(text, &w.name);
                if *expanded {
                    for item in items {
                        mw += GAP + button_size(text, &item.name).0;
                    }
                }
                (mw, h)
            }
        }
    }

    /// Each menu's head and items measured (their widths, for `place`).
    fn measure_menus(&mut self, text: &mut dyn Text) {
        for w in self.widgets.iter_mut().filter(|w| !w.removed) {
            if let Kind::Menu { items, head, .. } = &mut w.kind {
                let (hw, hh) = button_size(text, &w.name);
                *head = Rect::new(head.x(), head.y(), hw, hh);
                for item in items {
                    item.width = button_size(text, &item.name).0;
                }
            }
        }
    }

    /// Place widget `i` at (`x`, `y`). A leaf that would cross `limit` (a
    /// pinned widget's left, in a bar) is hidden; a stack that would is cut
    /// to what shows of it, each of its children placed (or hidden) in
    /// turn. Whether it folded a menu it hid.
    fn place(&mut self, i: usize, x: i32, y: i32, sizes: &[(u32, u32)], limit: i32) -> bool {
        let (w, h) = sizes[i];
        let fits = i64::from(x) + i64::from(w) <= i64::from(limit);
        let leaf = !matches!(self.widgets[i].kind, Kind::Stack(_));
        if leaf && !fits {
            return self.hide(i);
        }
        self.widgets[i].hidden = false;
        self.widgets[i].bounds = Rect::new(x, y, w, h);
        let mut folded = false;
        match &mut self.widgets[i].kind {
            Kind::Stack(dir) => {
                let dir = *dir;
                let children = self.widgets[i].children.clone();
                let (mut cx, mut cy) = (x, y);
                for c in children {
                    folded |= self.place(c, cx, cy, sizes, limit);
                    match dir {
                        Dir::Down => cy += (sizes[c].1 + GAP) as i32,
                        Dir::Across => cx += (sizes[c].0 + GAP) as i32,
                    }
                }
                if !fits {
                    // What still shows of it.
                    let shown = (i64::from(limit) - i64::from(x)).clamp(0, i64::from(w)) as u32;
                    self.widgets[i].bounds = Rect::new(x, y, shown, h);
                }
            }
            Kind::List { items, .. } => {
                let rows = items.len().max(1) as u32;
                let rh = h / rows;
                for (r, item) in items.iter_mut().enumerate() {
                    item.bounds = Rect::new(x, y + (r as u32 * rh) as i32, w, rh);
                }
            }
            Kind::Menu { items, expanded, head } => {
                // The widths were measured before (`measure_menus`).
                *head = Rect::new(x, y, head.w(), h);
                let mut cx = head.right();
                for item in items.iter_mut() {
                    item.bounds = if *expanded {
                        let r = Rect::new(cx + GAP as i32, y, item.width, h);
                        cx = r.right();
                        r
                    } else {
                        Rect::EMPTY
                    };
                }
            }
            _ => {}
        }
        folded
    }

    /// Hide widget `i` and all it holds (bounds empty), folding a menu (its
    /// items mustn't stay listed or chosen); whether it folded one.
    fn hide(&mut self, i: usize) -> bool {
        let w = &mut self.widgets[i];
        w.hidden = true;
        w.bounds = Rect::EMPTY;
        let mut folded = false;
        match &mut w.kind {
            Kind::List { items, .. } => items.iter_mut().for_each(|it| it.bounds = Rect::EMPTY),
            Kind::Menu { items, head, expanded } => {
                items.iter_mut().for_each(|it| it.bounds = Rect::EMPTY);
                *head = Rect::EMPTY;
                folded = core::mem::replace(expanded, false);
            }
            _ => {}
        }
        if folded && self.armed.is_some_and(|(a, item)| a == i && item.is_some()) {
            self.armed = None;
        }
        for c in self.widgets[i].children.clone() {
            folded |= self.hide(c);
        }
        folded
    }

    // -- hit testing and dispatch ----------------------------------------------

    /// The widget (not a stack) under (`x`, `y`), and the item there (a
    /// list's row; an expanded menu's item: `None` on its head).
    pub fn hit(&self, x: i32, y: i32) -> Option<(Widget, Option<usize>)> {
        self.hit_slot(x, y).map(|(i, item)| (self.widget(i), item))
    }

    fn hit_slot(&self, x: i32, y: i32) -> Option<(usize, Option<usize>)> {
        self.widgets.iter().enumerate().filter(|(_, w)| !w.removed && !w.hidden).find_map(|(i, w)| {
            let item = w.shown_items().iter().position(|it| it.bounds.contains(x, y));
            match &w.kind {
                Kind::Stack(_) => None,
                Kind::Menu { head, .. } => (head.contains(x, y) || item.is_some()).then_some((i, item)),
                _ => w.bounds.contains(x, y).then_some((i, item)),
            }
        })
    }

    /// Hand `input` to the tree: what happened.
    pub fn handle(&mut self, input: Input) -> Vec<UiEvent> {
        match input {
            Input::Down { x, y } => {
                self.armed = None;
                if let Some((i, row)) = self.hit_slot(x, y)
                    && !self.widgets[i].disabled
                {
                    self.focus_on(i);
                    self.armed = Some((i, row));
                }
                Vec::new()
            }
            Input::Up { x, y } => match self.armed.take() {
                Some((i, row)) if self.hit_slot(x, y) == Some((i, row)) => self.activate(i, row),
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
                let pressable = matches!(self.widgets[i].kind, Kind::Button | Kind::Checkbox(_) | Kind::Menu { .. });
                if pressable && matches!(k, Key::Enter | Key::Char(' ')) {
                    return self.activate(i, None);
                }
                match (&mut self.widgets[i].kind, k) {
                    (Kind::TextField(t), Key::Char(c)) if !c.is_control() => {
                        t.push(c);
                        self.touch(i);
                        alloc::vec![UiEvent::Edited(self.widget(i))]
                    }
                    (Kind::TextField(t), Key::Backspace) => {
                        if t.pop().is_some() {
                            self.touch(i);
                            alloc::vec![UiEvent::Edited(self.widget(i))]
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
        alloc::vec![UiEvent::Focused(self.widget(order[next]))]
    }

    fn takes_focus(&self, i: usize) -> bool {
        let w = &self.widgets[i];
        w.focusable() && !w.disabled && !w.removed && !w.hidden
    }

    fn focus_on(&mut self, i: usize) {
        if self.takes_focus(i) && self.focus != Some(i) {
            if let Some(old) = self.focus.replace(i) {
                self.touch(old);
            }
            self.touch(i);
        }
    }

    /// What a click on widget `i` (item `item`) does, and a `press` (or a
    /// list item's `select`) on it: the one handler both reach.
    fn activate(&mut self, i: usize, item: Option<usize>) -> Vec<UiEvent> {
        self.focus_on(i);
        let w = self.widget(i);
        let events = match &mut self.widgets[i].kind {
            Kind::Button => return alloc::vec![UiEvent::Pressed(w)],
            Kind::Checkbox(c) => {
                *c = !*c;
                alloc::vec![UiEvent::Toggled(w, *c)]
            }
            Kind::List { selected, .. } => match item {
                Some(r) => {
                    *selected = Some(r);
                    alloc::vec![UiEvent::Selected(w, r)]
                }
                None => return Vec::new(),
            },
            Kind::Menu { expanded, .. } => match item {
                Some(r) => {
                    *expanded = false;
                    alloc::vec![UiEvent::Chosen(w, r)]
                }
                None => {
                    *expanded = !*expanded;
                    alloc::vec![UiEvent::Expanded(w, *expanded)]
                }
            },
            Kind::Stack(_) | Kind::Label { .. } | Kind::TextField(_) => return Vec::new(),
        };
        self.touch(i);
        events
    }

    /// Run `action` on the node numbered `node` (a widget, or an item
    /// shown): the handler a click or key reaches, so the state after is
    /// the same as after the click. Errors change nothing: no such node,
    /// an action its role doesn't list, a disabled node.
    pub fn invoke(&mut self, node: u64, action: &Action) -> Result<Vec<UiEvent>, InvokeError> {
        let (i, item) = self.find(node).ok_or(InvokeError::NoNode)?;
        let role = match (item, &self.widgets[i].kind) {
            (Some(_), Kind::Menu { .. }) => Role::MenuItem,
            (Some(_), _) => Role::ListItem,
            (None, _) => self.widgets[i].role(),
        };
        let kind = action.kind();
        if !role.actions().contains(&kind) {
            return Err(InvokeError::NotAllowed(role, kind));
        }
        if self.widgets[i].disabled {
            return Err(InvokeError::Disabled);
        }
        if self.widgets[i].hidden {
            return Err(InvokeError::Hidden);
        }
        match action {
            Action::Press | Action::Select => Ok(self.activate(i, item)),
            Action::Focus => {
                self.focus_on(i);
                Ok(alloc::vec![UiEvent::Focused(self.widget(i))])
            }
            Action::SetValue(v) => {
                self.focus_on(i);
                let w = self.widget(i);
                self.set_text(w, v);
                Ok(alloc::vec![UiEvent::Edited(w)])
            }
            // No widget role lists these.
            Action::Close | Action::Minimize | Action::Restore => Err(InvokeError::NotAllowed(role, kind)),
        }
    }

    /// The widget numbered `node`, or the widget and index of an item
    /// shown (a folded menu's items name nothing); never a removed one.
    fn find(&self, node: u64) -> Option<(usize, Option<usize>)> {
        self.widgets.iter().enumerate().filter(|(_, w)| !w.removed).find_map(|(i, w)| {
            if w.id.get() == node {
                return Some((i, None));
            }
            w.shown_items().iter().position(|it| it.id.get() == node).map(|r| (i, Some(r)))
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
                Kind::Label { text: t, .. } => AutomationNode::label(w.id, &w.name, t, w.bounds, w.disabled),
                Kind::Button => AutomationNode::button(w.id, &w.name, w.bounds, focused, w.disabled),
                Kind::Checkbox(c) => AutomationNode::checkbox(w.id, &w.name, *c, w.bounds, focused, w.disabled),
                Kind::TextField(t) => AutomationNode::textfield(w.id, &w.name, t, w.bounds, focused, w.disabled),
                Kind::List { items, selected } => {
                    let ids = items.iter().map(|it| it.id).collect();
                    AutomationNode::list(w.id, &w.name, ids, *selected, w.bounds, focused, w.disabled)
                }
                Kind::Menu { expanded, head, .. } => {
                    let ids = w.shown_items().iter().map(|it| it.id).collect();
                    AutomationNode::menu(w.id, &w.name, *head, focused, w.disabled, *expanded, ids)
                }
            };
            out.push((depth, node.hidden_if(w.hidden)));
            match &w.kind {
                Kind::List { items, selected } => {
                    for (r, it) in items.iter().enumerate() {
                        let node = AutomationNode::list_item(it.id, &it.name, r, *selected, it.bounds, w.disabled);
                        out.push((depth + 1, node.hidden_if(w.hidden)));
                    }
                }
                Kind::Menu { .. } => {
                    for it in w.shown_items() {
                        out.push((depth + 1, AutomationNode::menu_item(it.id, &it.name, it.bounds, w.disabled)));
                    }
                }
                _ => {}
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
        self.paint_in(s, text, p, s.bounds());
    }

    /// Paint what of the tree lies in `clip` (its background first),
    /// touching no pixel outside it: the same pixels there as
    /// [`Ui::paint`].
    pub fn paint_in(&self, s: &mut Surface, text: &mut dyn Text, p: &Palette, clip: Rect) {
        let Some(clip) = clip.intersect(&s.bounds()) else { return };
        let mut pen = Pen { s, clip };
        pen.fill(clip, p.background);
        let line = text.line_height();
        for (i, w) in self.widgets.iter().enumerate() {
            if w.removed || w.painted().intersect(&clip).is_none() {
                continue;
            }
            let b = w.bounds;
            let fg = if w.disabled { p.disabled } else { p.text };
            let focused = self.window_focused && self.focus == Some(i);
            match &w.kind {
                Kind::Stack(_) => {}
                Kind::Label { text: t, .. } => pen.text(text, b.x(), b.y(), t, fg, b),
                Kind::Button => pen.button(text, b, &w.name, w.disabled, p),
                Kind::Checkbox(checked) => {
                    let boxed = Rect::new(b.x(), b.y() + centred(b.h(), BOX), BOX, BOX);
                    pen.fill(boxed, p.field);
                    pen.stroke(boxed, p.border);
                    if *checked {
                        pen.fill(Rect::new(boxed.x() + 3, boxed.y() + 3, BOX - 6, BOX - 6), p.accent);
                    }
                    let x = b.x() + (BOX + BOX_GAP) as i32;
                    pen.text(text, x, b.y() + centred(b.h(), line), &w.name, fg, b);
                }
                Kind::TextField(t) => {
                    pen.fill(b, p.field);
                    pen.stroke(b, p.border);
                    pen.text(text, b.x() + FIELD_PAD as i32, b.y() + FIELD_PAD as i32, t, fg, b);
                }
                Kind::List { items, selected } => {
                    pen.fill(b, p.field);
                    for (r, it) in items.iter().enumerate() {
                        let ink = if *selected == Some(r) {
                            pen.fill(it.bounds, p.accent);
                            p.accent_text
                        } else {
                            fg
                        };
                        let (x, y) = (it.bounds.x() + FIELD_PAD as i32, it.bounds.y() + ROW_PAD as i32);
                        pen.text(text, x, y, &it.name, ink, it.bounds);
                    }
                    pen.stroke(b, p.border);
                }
                Kind::Menu { items, expanded, head } => {
                    pen.button(text, *head, &w.name, w.disabled, p);
                    if *expanded {
                        // An open menu's head is outlined; its items are
                        // buttons drawn in the field colour.
                        pen.stroke(*head, p.text);
                        for it in items {
                            pen.fill(it.bounds, p.field);
                            pen.stroke(it.bounds, p.border);
                            let x = it.bounds.x() + centred(it.bounds.w(), text.width(&it.name));
                            let y = it.bounds.y() + centred(it.bounds.h(), line);
                            pen.text(text, x, y, &it.name, fg, it.bounds);
                        }
                    }
                }
            }
            if focused {
                // A ring just outside it (inside the gap to its neighbours).
                let r = match &w.kind {
                    Kind::Menu { head, .. } => *head,
                    _ => b,
                };
                pen.stroke(ring(r), p.text);
            }
        }
    }
}

/// The most rectangles kept before everything is repainted instead.
const MAX_DAMAGE: usize = 16;

/// A button's size for its name.
fn button_size(text: &mut dyn Text, name: &str) -> (u32, u32) {
    (text.width(name) + 2 * BUTTON_PAD_X, button_height(text.line_height()))
}

/// The height of a button (and of a bar's widgets) at a line height.
pub const fn button_height(line: u32) -> u32 {
    line + 2 * BUTTON_PAD_Y
}

/// Drawing clipped to one rectangle.
struct Pen<'a> {
    s: &'a mut Surface,
    clip: Rect,
}

impl Pen<'_> {
    fn fill(&mut self, r: Rect, color: u32) {
        if let Some(r) = r.intersect(&self.clip) {
            self.s.fill_rect(r, color);
        }
    }

    /// A one-pixel outline of `r`.
    fn stroke(&mut self, r: Rect, color: u32) {
        if r.w() == 0 || r.h() == 0 {
            return;
        }
        self.fill(Rect::new(r.x(), r.y(), r.w(), 1), color);
        self.fill(Rect::new(r.x(), r.bottom() - 1, r.w(), 1), color);
        self.fill(Rect::new(r.x(), r.y(), 1, r.h()), color);
        self.fill(Rect::new(r.right() - 1, r.y(), 1, r.h()), color);
    }

    /// `t` at (`x`, `y`), inside `within` (and the clip).
    fn text(&mut self, text: &mut dyn Text, x: i32, y: i32, t: &str, color: u32, within: Rect) {
        if let Some(clip) = within.intersect(&self.clip) {
            text.draw(self.s, x, y, t, color, clip);
        }
    }

    /// A button at `b` showing `name`.
    fn button(&mut self, text: &mut dyn Text, b: Rect, name: &str, disabled: bool, p: &Palette) {
        let (bg, ink) = if disabled { (p.disabled, p.background) } else { (p.accent, p.accent_text) };
        self.fill(b, bg);
        let x = b.x() + centred(b.w(), text.width(name));
        let y = b.y() + centred(b.h(), text.line_height());
        self.text(text, x, y, name, ink, b);
    }
}

/// Where something `inner` long starts to sit centred in `outer`.
fn centred(outer: u32, inner: u32) -> i32 {
    (outer as i32 - inner as i32) / 2
}
