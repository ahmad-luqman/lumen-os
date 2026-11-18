//! The automation tree's vocabulary (ARCHITECTURE section 11): node ids,
//! roles, values, actions, state flags and [`AutomationNode`].

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};

use lumen_gfx::Rect;

/// A node's id: stable for the node's lifetime, from one counter for the
/// whole system ([`NodeId::fresh`]), never reused in a boot. Always written
/// qualified (`lumen_ui::NodeId`): the VFS has a `NodeId` of its own.
/// Lookups go by the raw number ([`NodeId::get`]); text never becomes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(NonZeroU64);

/// The next id [`NodeId::fresh`] hands out.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl NodeId {
    /// A new id, never handed out before in this boot.
    pub fn fresh() -> NodeId {
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // Starts at 1 and only grows: 2^64 ids would take centuries.
        NodeId(NonZeroU64::new(n).unwrap_or(NonZeroU64::MAX))
    }

    /// The raw number (what `ui tree` prints and `ui invoke` looks up).
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a node is. A role decides its value's kind ([`Role::value_kind`]),
/// its actions ([`Role::actions`]) and the state flags it may carry
/// ([`Role::flags`]); a new role is added to ARCHITECTURE section 11 first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// A window's root (the compositor's).
    Window,
    /// A stack of widgets, or the taskbar.
    Group,
    Label,
    Button,
    Checkbox,
    TextField,
    List,
    ListItem,
    Menu,
    MenuItem,
}

/// The kind of value a role's node has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Checked,
    Selected,
}

impl Role {
    /// Every role.
    pub const ALL: [Role; 10] = [
        Role::Window,
        Role::Group,
        Role::Label,
        Role::Button,
        Role::Checkbox,
        Role::TextField,
        Role::List,
        Role::ListItem,
        Role::Menu,
        Role::MenuItem,
    ];

    /// Its place in [`Role::ALL`] (checked at compile time).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The name `ui tree` prints (`where role = button`).
    pub fn name(self) -> &'static str {
        match self {
            Role::Window => "window",
            Role::Group => "group",
            Role::Label => "label",
            Role::Button => "button",
            Role::Checkbox => "checkbox",
            Role::TextField => "textfield",
            Role::List => "list",
            Role::ListItem => "listitem",
            Role::Menu => "menu",
            Role::MenuItem => "menuitem",
        }
    }

    /// The actions a node of this role takes.
    pub fn actions(self) -> &'static [ActionKind] {
        use ActionKind as A;
        match self {
            Role::Window => &[A::Focus, A::Close, A::Minimize, A::Restore],
            Role::Group | Role::Label => &[],
            Role::Button | Role::Checkbox => &[A::Press, A::Focus],
            Role::TextField => &[A::SetValue, A::Focus],
            Role::List => &[A::Focus],
            Role::ListItem => &[A::Select, A::Press],
            Role::Menu => &[A::Press, A::Focus],
            Role::MenuItem => &[A::Press],
        }
    }

    /// The kind of value its nodes have (`None`: no value).
    pub fn value_kind(self) -> Option<ValueKind> {
        match self {
            Role::Label | Role::TextField => Some(ValueKind::Text),
            Role::Checkbox => Some(ValueKind::Checked),
            Role::List => Some(ValueKind::Selected),
            _ => None,
        }
    }

    /// The state flags its nodes may carry.
    pub fn flags(self) -> StateFlags {
        let (f, d, s, e) = (StateFlags::FOCUSED, StateFlags::DISABLED, StateFlags::SELECTED, StateFlags::EXPANDED);
        // Every widget may be hidden (#157: past a bar's pinned widget); a
        // window's root never is.
        let h = StateFlags::HIDDEN;
        match self {
            Role::Window => f,
            Role::Group | Role::Label | Role::MenuItem => d.with(h),
            Role::Button | Role::Checkbox | Role::TextField | Role::List => f.with(d).with(h),
            Role::ListItem => s.with(d).with(h),
            Role::Menu => f.with(d).with(e).with(h),
        }
    }
}

// Every role and every action kind at its own place in its `ALL`, in
// declaration order: one put in twice or out of order fails to compile.
// (One left out isn't caught here: the host tests check that every action
// a role lists parses, and each list's length against its last variant.)
const _: () = {
    let mut i = 0;
    while i < Role::ALL.len() {
        assert!(Role::ALL[i].index() == i);
        i += 1;
    }
    let mut i = 0;
    while i < ActionKind::ALL.len() {
        assert!(ActionKind::ALL[i].index() == i);
        i += 1;
    }
};

/// A node's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Checked(bool),
    /// A list's selected item, by index.
    Selected(Option<usize>),
}

impl Value {
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Text(_) => ValueKind::Text,
            Value::Checked(_) => ValueKind::Checked,
            Value::Selected(_) => ValueKind::Selected,
        }
    }
}

/// As `ui tree` prints it: the text, `checked`/`unchecked`, or the index
/// (`-` for none).
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Text(t) => f.write_str(t),
            Value::Checked(true) => f.write_str("checked"),
            Value::Checked(false) => f.write_str("unchecked"),
            Value::Selected(Some(i)) => write!(f, "{i}"),
            Value::Selected(None) => f.write_str("-"),
        }
    }
}

/// An action, with what it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    SetValue(String),
    Select,
    Focus,
    Close,
    Minimize,
    Restore,
}

/// An action's kind, without what it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Press,
    SetValue,
    Select,
    Focus,
    Close,
    Minimize,
    Restore,
}

impl ActionKind {
    pub const ALL: [ActionKind; 7] = [
        ActionKind::Press,
        ActionKind::SetValue,
        ActionKind::Select,
        ActionKind::Focus,
        ActionKind::Close,
        ActionKind::Minimize,
        ActionKind::Restore,
    ];

    /// Its place in [`ActionKind::ALL`] (checked at compile time).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The word `ui invoke` takes and `ui tree` prints.
    pub fn name(self) -> &'static str {
        match self {
            ActionKind::Press => "press",
            ActionKind::SetValue => "set_value",
            ActionKind::Select => "select",
            ActionKind::Focus => "focus",
            ActionKind::Close => "close",
            ActionKind::Minimize => "minimize",
            ActionKind::Restore => "restore",
        }
    }
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Press => ActionKind::Press,
            Action::SetValue(_) => ActionKind::SetValue,
            Action::Select => ActionKind::Select,
            Action::Focus => ActionKind::Focus,
            Action::Close => ActionKind::Close,
            Action::Minimize => ActionKind::Minimize,
            Action::Restore => ActionKind::Restore,
        }
    }

    /// `ui invoke`'s words after the node: an action's name and its
    /// arguments (`set_value` takes exactly one, the others none).
    pub fn parse(word: &str, args: &[&str]) -> Result<Action, InvokeError> {
        let kind = ActionKind::ALL.into_iter().find(|k| k.name() == word).ok_or(InvokeError::NoSuchAction)?;
        match (kind, args) {
            (ActionKind::SetValue, [text]) => Ok(Action::SetValue(String::from(*text))),
            (ActionKind::SetValue, _) => Err(InvokeError::Arguments),
            (_, [_, ..]) => Err(InvokeError::Arguments),
            (ActionKind::Press, []) => Ok(Action::Press),
            (ActionKind::Select, []) => Ok(Action::Select),
            (ActionKind::Focus, []) => Ok(Action::Focus),
            (ActionKind::Close, []) => Ok(Action::Close),
            (ActionKind::Minimize, []) => Ok(Action::Minimize),
            (ActionKind::Restore, []) => Ok(Action::Restore),
        }
    }
}

/// Why an action changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvokeError {
    /// No node has that id (or its window closed).
    NoNode,
    /// Not an action's name.
    NoSuchAction,
    /// The node's role doesn't list the action.
    NotAllowed(Role, ActionKind),
    /// The node is disabled.
    Disabled,
    /// The node isn't shown (#157: it doesn't fit its bar).
    Hidden,
    /// Wrong arguments for the action.
    Arguments,
}

impl fmt::Display for InvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvokeError::NoNode => f.write_str("no node"),
            InvokeError::NoSuchAction => {
                f.write_str("no such action (")?;
                for (i, kind) in ActionKind::ALL.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(kind.name())?;
                }
                f.write_str(")")
            }
            InvokeError::NotAllowed(role, action) => write!(f, "a {} doesn't take {}", role.name(), action.name()),
            InvokeError::Disabled => f.write_str("the node is disabled"),
            InvokeError::Hidden => f.write_str("not shown: it doesn't fit"),
            InvokeError::Arguments => f.write_str("wrong arguments (set_value takes one, the others none)"),
        }
    }
}

/// A node's state flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StateFlags(u8);

impl StateFlags {
    pub const NONE: StateFlags = StateFlags(0);
    pub const FOCUSED: StateFlags = StateFlags(1);
    pub const DISABLED: StateFlags = StateFlags(2);
    pub const SELECTED: StateFlags = StateFlags(4);
    pub const EXPANDED: StateFlags = StateFlags(8);
    /// Laid out with no room (#157: past a bar's pinned widget): not
    /// drawn, not hit, takes no action.
    pub const HIDDEN: StateFlags = StateFlags(16);
    const NAMES: [(StateFlags, &'static str); 5] = [
        (StateFlags::FOCUSED, "focused"),
        (StateFlags::DISABLED, "disabled"),
        (StateFlags::SELECTED, "selected"),
        (StateFlags::EXPANDED, "expanded"),
        (StateFlags::HIDDEN, "hidden"),
    ];

    pub const fn with(self, other: StateFlags) -> StateFlags {
        StateFlags(self.0 | other.0)
    }

    /// These flags, `other`'s on if `on`.
    pub const fn with_if(self, other: StateFlags, on: bool) -> StateFlags {
        if on { self.with(other) } else { self }
    }

    pub const fn contains(self, other: StateFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Only the flags `allowed` has.
    pub const fn within(self, allowed: StateFlags) -> StateFlags {
        StateFlags(self.0 & allowed.0)
    }
}

/// As `ui tree` prints them: the set flags joined by `,` (`-` for none).
impl fmt::Display for StateFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("-");
        }
        let mut first = true;
        for (flag, name) in StateFlags::NAMES {
            if self.contains(flag) {
                if !first {
                    f.write_str(",")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// One node of the automation tree, made from the widget itself (so the
/// tree can't drift from what is drawn). Its fields are private; there is
/// one constructor per role, each keeping the role's rules: its kind of
/// value, and only the flags the role may carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationNode {
    id: NodeId,
    role: Role,
    /// The accessible label: "Add", "File name".
    name: String,
    value: Option<Value>,
    state: StateFlags,
    /// In client coordinates.
    bounds: Rect,
    children: Vec<NodeId>,
}

impl AutomationNode {
    fn new(
        id: NodeId,
        role: Role,
        name: &str,
        value: Option<Value>,
        state: StateFlags,
        bounds: Rect,
        children: Vec<NodeId>,
    ) -> AutomationNode {
        debug_assert_eq!(value.as_ref().map(Value::kind), role.value_kind());
        AutomationNode {
            id,
            role,
            name: String::from(name),
            value,
            state: state.within(role.flags()),
            bounds,
            children,
        }
    }

    /// A window's root: its name the title.
    pub fn window(id: NodeId, title: &str, bounds: Rect, focused: bool, children: Vec<NodeId>) -> AutomationNode {
        let state = StateFlags::NONE.with_if(StateFlags::FOCUSED, focused);
        AutomationNode::new(id, Role::Window, title, None, state, bounds, children)
    }

    pub fn group(id: NodeId, name: &str, bounds: Rect, disabled: bool, children: Vec<NodeId>) -> AutomationNode {
        let state = StateFlags::NONE.with_if(StateFlags::DISABLED, disabled);
        AutomationNode::new(id, Role::Group, name, None, state, bounds, children)
    }

    pub fn label(id: NodeId, name: &str, text: &str, bounds: Rect, disabled: bool) -> AutomationNode {
        let state = StateFlags::NONE.with_if(StateFlags::DISABLED, disabled);
        AutomationNode::new(id, Role::Label, name, Some(Value::Text(String::from(text))), state, bounds, Vec::new())
    }

    pub fn button(id: NodeId, name: &str, bounds: Rect, focused: bool, disabled: bool) -> AutomationNode {
        let state = focus_disabled(focused, disabled);
        AutomationNode::new(id, Role::Button, name, None, state, bounds, Vec::new())
    }

    pub fn checkbox(
        id: NodeId,
        name: &str,
        checked: bool,
        bounds: Rect,
        focused: bool,
        disabled: bool,
    ) -> AutomationNode {
        let state = focus_disabled(focused, disabled);
        AutomationNode::new(id, Role::Checkbox, name, Some(Value::Checked(checked)), state, bounds, Vec::new())
    }

    pub fn textfield(
        id: NodeId,
        name: &str,
        text: &str,
        bounds: Rect,
        focused: bool,
        disabled: bool,
    ) -> AutomationNode {
        let state = focus_disabled(focused, disabled);
        AutomationNode::new(id, Role::TextField, name, Some(Value::Text(String::from(text))), state, bounds, Vec::new())
    }

    /// A list: its items' nodes are `items`, `selected` the one selected
    /// (each item's `selected` flag is derived from it:
    /// [`AutomationNode::list_item`]).
    pub fn list(
        id: NodeId,
        name: &str,
        items: Vec<NodeId>,
        selected: Option<usize>,
        bounds: Rect,
        focused: bool,
        disabled: bool,
    ) -> AutomationNode {
        let selected = selected.filter(|&i| i < items.len());
        let state = focus_disabled(focused, disabled);
        AutomationNode::new(id, Role::List, name, Some(Value::Selected(selected)), state, bounds, items)
    }

    /// Item `index` of a list whose selection is `selected`: selected
    /// exactly when it is that one.
    pub fn list_item(
        id: NodeId,
        name: &str,
        index: usize,
        selected: Option<usize>,
        bounds: Rect,
        disabled: bool,
    ) -> AutomationNode {
        let state = StateFlags::NONE
            .with_if(StateFlags::SELECTED, selected == Some(index))
            .with_if(StateFlags::DISABLED, disabled);
        AutomationNode::new(id, Role::ListItem, name, None, state, bounds, Vec::new())
    }

    pub fn menu(
        id: NodeId,
        name: &str,
        bounds: Rect,
        focused: bool,
        disabled: bool,
        expanded: bool,
        items: Vec<NodeId>,
    ) -> AutomationNode {
        let state = focus_disabled(focused, disabled).with_if(StateFlags::EXPANDED, expanded);
        AutomationNode::new(id, Role::Menu, name, None, state, bounds, items)
    }

    /// The same node, marked hidden if `hidden` (and its role allows it).
    pub fn hidden_if(mut self, hidden: bool) -> AutomationNode {
        self.state = self.state.with_if(StateFlags::HIDDEN, hidden).within(self.role.flags());
        self
    }

    pub fn menu_item(id: NodeId, name: &str, bounds: Rect, disabled: bool) -> AutomationNode {
        let state = StateFlags::NONE.with_if(StateFlags::DISABLED, disabled);
        AutomationNode::new(id, Role::MenuItem, name, None, state, bounds, Vec::new())
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }

    pub fn state(&self) -> StateFlags {
        self.state
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    /// Its role's actions.
    pub fn actions(&self) -> &'static [ActionKind] {
        self.role.actions()
    }

    /// The same node with its bounds moved by (`dx`, `dy`) (client to
    /// screen coordinates, for `ui tree`).
    pub fn translated(&self, dx: i32, dy: i32) -> AutomationNode {
        AutomationNode { bounds: self.bounds.translate(dx, dy), ..self.clone() }
    }
}

fn focus_disabled(focused: bool, disabled: bool) -> StateFlags {
    StateFlags::NONE.with_if(StateFlags::FOCUSED, focused).with_if(StateFlags::DISABLED, disabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Rect {
        Rect::new(1, 2, 3, 4)
    }

    /// One node of every role, every flag asked for.
    fn every_role() -> Vec<AutomationNode> {
        let id = NodeId::fresh;
        let items = alloc::vec![id(), id()];
        let nodes = alloc::vec![
            AutomationNode::window(id(), "w", r(), true, Vec::new()),
            AutomationNode::group(id(), "g", r(), true, Vec::new()),
            AutomationNode::label(id(), "l", "text", r(), true),
            AutomationNode::button(id(), "b", r(), true, true),
            AutomationNode::checkbox(id(), "c", true, r(), true, true),
            AutomationNode::textfield(id(), "t", "x", r(), true, true),
            AutomationNode::list(id(), "list", items, Some(1), r(), true, true),
            AutomationNode::list_item(id(), "i", 1, Some(1), r(), true),
            AutomationNode::menu(id(), "m", r(), true, true, true, Vec::new()),
            AutomationNode::menu_item(id(), "mi", r(), true),
        ];
        nodes.into_iter().map(|n| n.hidden_if(true)).collect()
    }

    #[test]
    fn ids_are_fresh_and_never_reused() {
        let a = NodeId::fresh();
        let b = NodeId::fresh();
        assert!(b.get() > a.get());
        assert_eq!(alloc::format!("{a}"), alloc::format!("{}", a.get()));
    }

    #[test]
    fn each_role_has_its_value_kind_actions_and_flags() {
        // The table in ARCHITECTURE section 11, row by row.
        use ActionKind as A;
        use ValueKind as V;
        let (f, d, s, e) = (StateFlags::FOCUSED, StateFlags::DISABLED, StateFlags::SELECTED, StateFlags::EXPANDED);
        let h = StateFlags::HIDDEN;
        let table: [(Role, Option<V>, &[A], StateFlags); 10] = [
            (Role::Window, None, &[A::Focus, A::Close, A::Minimize, A::Restore], f),
            (Role::Group, None, &[], d.with(h)),
            (Role::Label, Some(V::Text), &[], d.with(h)),
            (Role::Button, None, &[A::Press, A::Focus], f.with(d).with(h)),
            (Role::Checkbox, Some(V::Checked), &[A::Press, A::Focus], f.with(d).with(h)),
            (Role::TextField, Some(V::Text), &[A::SetValue, A::Focus], f.with(d).with(h)),
            (Role::List, Some(V::Selected), &[A::Focus], f.with(d).with(h)),
            (Role::ListItem, None, &[A::Select, A::Press], s.with(d).with(h)),
            (Role::Menu, None, &[A::Press, A::Focus], f.with(d).with(e).with(h)),
            (Role::MenuItem, None, &[A::Press], d.with(h)),
        ];
        assert_eq!(table.map(|row| row.0), Role::ALL);
        for (role, value, actions, flags) in table {
            assert_eq!(role.value_kind(), value, "{role:?}");
            assert_eq!(role.actions(), actions, "{role:?}");
            assert_eq!(role.flags(), flags, "{role:?}");
        }
        // Each constructor keeps its role's rules, even asked for more.
        for node in every_role() {
            let role = node.role();
            assert_eq!(node.value().map(Value::kind), role.value_kind(), "{role:?}");
            assert_eq!(node.state().within(role.flags()), node.state(), "{role:?}");
            assert_eq!(node.state(), role.flags(), "{role:?}: every flag it may carry was asked for");
            assert_eq!(node.actions(), role.actions());
        }
    }

    #[test]
    fn a_list_s_selection_decides_its_items_flags() {
        let items = [NodeId::fresh(), NodeId::fresh(), NodeId::fresh()];
        let list = AutomationNode::list(NodeId::fresh(), "l", items.to_vec(), Some(2), r(), false, false);
        assert_eq!(list.value(), Some(&Value::Selected(Some(2))));
        assert_eq!(list.children(), &items);
        let flags: Vec<bool> = (0..3)
            .map(|i| {
                AutomationNode::list_item(items[i], "i", i, Some(2), r(), false).state().contains(StateFlags::SELECTED)
            })
            .collect();
        assert_eq!(flags, [false, false, true]);
        // A selection past the items is none.
        let list = AutomationNode::list(NodeId::fresh(), "l", items.to_vec(), Some(3), r(), false, false);
        assert_eq!(list.value(), Some(&Value::Selected(None)));
    }

    #[test]
    fn values_and_flags_print_as_ui_tree_shows_them() {
        assert_eq!(alloc::format!("{}", Value::Text(String::from("7"))), "7");
        assert_eq!(alloc::format!("{}", Value::Checked(true)), "checked");
        assert_eq!(alloc::format!("{}", Value::Checked(false)), "unchecked");
        assert_eq!(alloc::format!("{}", Value::Selected(Some(3))), "3");
        assert_eq!(alloc::format!("{}", Value::Selected(None)), "-");
        assert_eq!(alloc::format!("{}", StateFlags::NONE), "-");
        assert_eq!(alloc::format!("{}", StateFlags::DISABLED.with(StateFlags::FOCUSED)), "focused,disabled");
    }

    #[test]
    fn actions_parse_from_words_and_refuse_wrong_arguments() {
        assert_eq!(Action::parse("press", &[]), Ok(Action::Press));
        assert_eq!(Action::parse("set_value", &["hi"]), Ok(Action::SetValue(String::from("hi"))));
        assert_eq!(Action::parse("set_value", &[]), Err(InvokeError::Arguments));
        assert_eq!(Action::parse("set_value", &["a", "b"]), Err(InvokeError::Arguments));
        assert_eq!(Action::parse("press", &["x"]), Err(InvokeError::Arguments));
        assert_eq!(Action::parse("poke", &[]), Err(InvokeError::NoSuchAction));
        for kind in ActionKind::ALL {
            let args: &[&str] = if kind == ActionKind::SetValue { &["v"] } else { &[] };
            assert_eq!(Action::parse(kind.name(), args).map(|a| a.kind()), Ok(kind));
        }
    }

    #[test]
    fn translated_moves_only_the_bounds() {
        let n = AutomationNode::button(NodeId::fresh(), "Add", Rect::new(10, 20, 30, 40), false, false);
        let t = n.translated(100, 200);
        assert_eq!(t.bounds(), Rect::new(110, 220, 30, 40));
        assert_eq!((t.id(), t.name(), t.role()), (n.id(), n.name(), n.role()));
    }
}
