//! [`Tree`], what an app publishes with `window_publish` (#207;
//! ARCHITECTURE section 8, "Wire formats" and "Apps' automation trees"):
//! the app's `lumen-ui` nodes in pre-order, each a role-fused [`Node`]
//! mirroring `lumen-ui`'s per-role constructors, so a mismatched value, a
//! flag a role doesn't allow, a window's root, children under a leaf, a
//! stray item and a disagreeing selection can't be written at all.
//!
//! What is left to check, [`Tree::decode`] checks: ids non-zero and unique
//! (items' too); at most one node focused; a list's selection below its items' count; every
//! rectangle's far edges within `i32`; the groups' `children` counts
//! consuming the nodes exactly; at most [`TREE_NODES`] nodes (items
//! counted) and [`TREE_BYTES`] encoded. It decodes node by node
//! (`postcard::take_from_bytes::<Node>` in a loop), every item list through
//! a capped visitor ([`CappedVec`]), and works out the shape with a stack on
//! the heap, never by recursion: no wire type here is recursive.
//!
//! Both sides of the conversion live here: [`Tree::from_automation`] (an
//! app's `Ui::nodes()`, the SDK's publish) and [`Tree::rows`] (the kernel's
//! copy, each app id mapped to a global `lumen_ui::NodeId`).

use crate::layout::Rect;
use crate::limits::{TEXT_BYTES, TREE_BYTES, TREE_NODES};
use crate::wire::{Capped, CappedVec};
use alloc::vec::Vec;
use lumen_ui::{AutomationNode, NodeId, Role, StateFlags, Value};
use serde::{Deserialize, Serialize};

/// A node's name or text: at most [`TEXT_BYTES`] (an app's toolkit cuts a
/// longer one, `Capped::cut`; a longer one decoded is refused). The kernel
/// shows it sanitised, as log text is ([`Tree::rows`]).
pub type Text = Capped<TEXT_BYTES>;

/// What every node and every item has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Common {
    /// The app's own id (its `lumen_ui::NodeId`): non-zero, unique in the
    /// tree.
    pub id: u64,
    /// The accessible label ("Add").
    pub name: Text,
    /// Client coordinates; `x + w` and `y + h` within `i32`.
    pub bounds: Rect,
    pub disabled: bool,
    pub hidden: bool,
}

/// A list's or a menu's item: the same fields as every node (a list
/// item's `selected` flag comes from its list).
pub type Item = Common;

/// A list's or a menu's items: at most [`TREE_NODES`], refused past that
/// before anything is allocated.
pub type Items = CappedVec<Item, TREE_NODES>;

/// One node, its role fused with what that role carries, in this (wire)
/// order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    /// Its `children` direct children follow it.
    Group {
        common: Common,
        children: u32,
    },
    Label {
        common: Common,
        text: Text,
    },
    Button {
        common: Common,
        focused: bool,
    },
    Checkbox {
        common: Common,
        focused: bool,
        checked: bool,
    },
    TextField {
        common: Common,
        focused: bool,
        text: Text,
    },
    List {
        common: Common,
        focused: bool,
        selected: Option<u32>,
        items: Items,
    },
    Menu {
        common: Common,
        focused: bool,
        expanded: bool,
        items: Items,
    },
}

impl Node {
    pub fn common(&self) -> &Common {
        match self {
            Node::Group { common, .. }
            | Node::Label { common, .. }
            | Node::Button { common, .. }
            | Node::Checkbox { common, .. }
            | Node::TextField { common, .. }
            | Node::List { common, .. }
            | Node::Menu { common, .. } => common,
        }
    }

    /// Its items (a list's or a menu's; none for the others).
    pub fn items(&self) -> &[Item] {
        match self {
            Node::List { items, .. } | Node::Menu { items, .. } => items.as_slice(),
            _ => &[],
        }
    }

    /// Whether it says it has the focus.
    pub fn focused(&self) -> bool {
        match self {
            Node::Button { focused, .. }
            | Node::Checkbox { focused, .. }
            | Node::TextField { focused, .. }
            | Node::List { focused, .. }
            | Node::Menu { focused, .. } => *focused,
            Node::Group { .. } | Node::Label { .. } => false,
        }
    }

    /// Its role in the automation tree.
    pub fn role(&self) -> Role {
        match self {
            Node::Group { .. } => Role::Group,
            Node::Label { .. } => Role::Label,
            Node::Button { .. } => Role::Button,
            Node::Checkbox { .. } => Role::Checkbox,
            Node::TextField { .. } => Role::TextField,
            Node::List { .. } => Role::List,
            Node::Menu { .. } => Role::Menu,
        }
    }
}

/// Why a tree isn't one an app may publish (to the app: `BadArgument`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// Encoded past [`TREE_BYTES`] (checked before decoding).
    TooBig(usize),
    /// Node `node` (counted from 0) didn't decode: postcard's refusal.
    Decode { node: usize, error: postcard::Error },
    /// More than [`TREE_NODES`] nodes, items counted.
    TooManyNodes,
    /// An id of 0.
    ZeroId,
    /// An id twice.
    DuplicateId(u64),
    /// Node `node`'s selection is past its items.
    Selection { node: usize },
    /// Node `node`'s rectangle (or one of its items') passes `i32`.
    Bounds { node: usize },
    /// The groups' `children` counts don't consume the nodes exactly.
    Children,
    /// More than one node focused (a window has one focus).
    Focus,
    /// A row `Tree::from_automation` can't write: a window's root, or an
    /// item that doesn't follow its list or menu (an app-side bug).
    Shape { row: usize },
}

impl core::fmt::Display for TreeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TreeError::TooBig(n) => write!(f, "{n} bytes, past {TREE_BYTES}"),
            TreeError::Decode { node, error } => write!(f, "node {node} didn't decode ({error:?})"),
            TreeError::TooManyNodes => write!(f, "more than {TREE_NODES} nodes"),
            TreeError::ZeroId => f.write_str("an id of 0"),
            TreeError::DuplicateId(id) => write!(f, "id {id} twice"),
            TreeError::Selection { node } => write!(f, "node {node}'s selection is past its items"),
            TreeError::Bounds { node } => write!(f, "node {node}'s bounds pass i32"),
            TreeError::Children => f.write_str("the children counts don't consume the nodes exactly"),
            TreeError::Focus => f.write_str("more than one node focused"),
            TreeError::Shape { row } => write!(f, "row {row} has no wire node"),
        }
    }
}

/// Where a node sits: its depth (1 for a top node: the window's root, 0,
/// is the compositor's) and its parent's index (`None` at the top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    depth: u32,
    parent: Option<u32>,
}

/// A checked tree: its nodes in pre-order and where each sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    nodes: Vec<Node>,
    places: Vec<Place>,
}

/// A rectangle whose far edges are within `i32`.
fn fits(r: &Rect) -> bool {
    i64::from(r.x) + i64::from(r.w) <= i64::from(i32::MAX) && i64::from(r.y) + i64::from(r.h) <= i64::from(i32::MAX)
}

impl Tree {
    /// The tree `bytes` hold: its size first ([`TREE_BYTES`]), then node by
    /// node, at most [`TREE_NODES`] (items counted), then the rules above.
    /// No bytes is a tree of no nodes.
    pub fn decode(bytes: &[u8]) -> Result<Tree, TreeError> {
        Tree::decode_counted(bytes).0
    }

    /// [`Tree::decode`], and how many nodes it decoded (items counted),
    /// refused or not: the work `window_publish` charges `NODE_FUEL` for
    /// (a refusal can come after every node was decoded and its ids
    /// sorted).
    pub fn decode_counted(bytes: &[u8]) -> (Result<Tree, TreeError>, usize) {
        if bytes.len() > TREE_BYTES {
            return (Err(TreeError::TooBig(bytes.len())), 0);
        }
        let mut nodes = Vec::new();
        let mut count = 0;
        let mut rest = bytes;
        while !rest.is_empty() {
            if count >= TREE_NODES {
                return (Err(TreeError::TooManyNodes), count);
            }
            let (node, more) = match postcard::take_from_bytes::<Node>(rest) {
                Ok(taken) => taken,
                Err(error) => return (Err(TreeError::Decode { node: nodes.len(), error }), count),
            };
            count += 1 + node.items().len();
            if count > TREE_NODES {
                return (Err(TreeError::TooManyNodes), count);
            }
            nodes.push(node);
            rest = more;
        }
        (Tree::new(nodes), count)
    }

    /// `nodes`, checked against every rule but the size.
    pub fn new(nodes: Vec<Node>) -> Result<Tree, TreeError> {
        if nodes.iter().map(|n| 1 + n.items().len()).sum::<usize>() > TREE_NODES {
            return Err(TreeError::TooManyNodes);
        }
        let mut ids = Vec::with_capacity(nodes.len());
        for (i, node) in nodes.iter().enumerate() {
            let common = node.common();
            if !fits(&common.bounds) || node.items().iter().any(|it| !fits(&it.bounds)) {
                return Err(TreeError::Bounds { node: i });
            }
            if let Node::List { selected: Some(s), items, .. } = node
                && *s as usize >= items.len()
            {
                return Err(TreeError::Selection { node: i });
            }
            ids.push(common.id);
            ids.extend(node.items().iter().map(|it| it.id));
        }
        ids.sort_unstable();
        if ids.first() == Some(&0) {
            return Err(TreeError::ZeroId);
        }
        if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
            return Err(TreeError::DuplicateId(w[0]));
        }
        drop(ids);
        if nodes.iter().filter(|n| n.focused()).count() > 1 {
            return Err(TreeError::Focus);
        }
        // The shape: a stack of (children still to come, the group's
        // index), on the heap and no deeper than the nodes.
        let mut places = Vec::with_capacity(nodes.len());
        let mut open: Vec<(u32, u32)> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            while open.last().is_some_and(|&(left, _)| left == 0) {
                open.pop();
            }
            let depth = open.len() as u32 + 1;
            let parent = open.last_mut().map(|(left, at)| {
                *left -= 1;
                *at
            });
            places.push(Place { depth, parent });
            if let Node::Group { children, .. } = node
                && *children > 0
            {
                open.push((*children, i as u32));
            }
        }
        while open.last().is_some_and(|&(left, _)| left == 0) {
            open.pop();
        }
        if !open.is_empty() {
            return Err(TreeError::Children);
        }
        Ok(Tree { nodes, places })
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Its nodes with their items counted (what [`TREE_NODES`] bounds).
    pub fn count(&self) -> usize {
        self.nodes.iter().map(|n| 1 + n.items().len()).sum()
    }

    /// Its encoding: node after node (refused past [`TREE_BYTES`]).
    pub fn encode(&self) -> Result<Vec<u8>, TreeError> {
        let mut out = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            out = postcard::to_extend(node, out).map_err(|error| TreeError::Decode { node: i, error })?;
            if out.len() > TREE_BYTES {
                return Err(TreeError::TooBig(out.len()));
            }
        }
        Ok(out)
    }

    /// An app's tree from its `Ui::nodes()` (depth first, items after their
    /// list or menu).
    pub fn from_automation(rows: &[(u32, AutomationNode)]) -> Result<Tree, TreeError> {
        let mut nodes = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let n = &rows[i].1;
            let common = common_of(n);
            let state = n.state();
            let focused = state.contains(StateFlags::FOCUSED);
            let text = || match n.value() {
                Some(Value::Text(t)) => Text::cut(t),
                _ => Text::cut(""),
            };
            let item_rows = |role: Role| -> Result<Items, TreeError> {
                let want = n.children();
                let rows = rows.get(i + 1..i + 1 + want.len()).ok_or(TreeError::Shape { row: i })?;
                let items: Vec<Item> = rows
                    .iter()
                    .zip(want)
                    .map(|((_, r), id)| if r.role() == role && r.id() == *id { Ok(common_of(r)) } else { Err(()) })
                    .collect::<Result<_, ()>>()
                    .map_err(|()| TreeError::Shape { row: i })?;
                CappedVec::new(items).ok_or(TreeError::TooManyNodes)
            };
            let node = match n.role() {
                Role::Group => Node::Group { common, children: n.children().len() as u32 },
                Role::Label => Node::Label { common, text: text() },
                Role::Button => Node::Button { common, focused },
                Role::Checkbox => {
                    Node::Checkbox { common, focused, checked: matches!(n.value(), Some(Value::Checked(true))) }
                }
                Role::TextField => Node::TextField { common, focused, text: text() },
                Role::List => {
                    let selected = match n.value() {
                        Some(Value::Selected(Some(s))) => Some(*s as u32),
                        _ => None,
                    };
                    Node::List { common, focused, selected, items: item_rows(Role::ListItem)? }
                }
                Role::Menu => Node::Menu {
                    common,
                    focused,
                    expanded: state.contains(StateFlags::EXPANDED),
                    items: item_rows(Role::MenuItem)?,
                },
                Role::Window | Role::ListItem | Role::MenuItem => return Err(TreeError::Shape { row: i }),
            };
            i += 1 + node.items().len();
            nodes.push(node);
        }
        Tree::new(nodes)
    }

    /// The kernel's copy: the root's children (the top nodes) and every
    /// node with its depth (from 1; items one deeper than their list or
    /// menu), client coordinates, each app id `global` maps, given with its
    /// role (asked in pre-order, a node before its items). Names and texts
    /// are shown sanitised, as log text is (`text::cut`: each control
    /// character and line or paragraph separator U+FFFD), so an app's node
    /// can't forge a line of `ui tree`.
    pub fn rows(&self, mut global: impl FnMut(u64, Role) -> NodeId) -> (Vec<NodeId>, Vec<(u32, AutomationNode)>) {
        let ids: Vec<(NodeId, Vec<NodeId>)> = self
            .nodes
            .iter()
            .map(|n| {
                let id = global(n.common().id, n.role());
                let item_role = if n.role() == Role::List { Role::ListItem } else { Role::MenuItem };
                (id, n.items().iter().map(|it| global(it.id, item_role)).collect())
            })
            .collect();
        let mut children: Vec<Vec<NodeId>> = alloc::vec![Vec::new(); self.nodes.len()];
        let mut top = Vec::new();
        for (i, place) in self.places.iter().enumerate() {
            match place.parent {
                Some(p) => children[p as usize].push(ids[i].0),
                None => top.push(ids[i].0),
            }
        }
        let mut rows = Vec::with_capacity(self.count());
        for (i, ((node, place), kids)) in self.nodes.iter().zip(&self.places).zip(children).enumerate() {
            let (id, item_ids) = &ids[i];
            let c = node.common();
            let name = shown(&c.name);
            let (name, bounds, disabled) = (&*name, c.bounds.into(), c.disabled);
            let made = match node {
                Node::Group { .. } => AutomationNode::group(*id, name, bounds, disabled, kids),
                Node::Label { text, .. } => AutomationNode::label(*id, name, &shown(text), bounds, disabled),
                Node::Button { focused, .. } => AutomationNode::button(*id, name, bounds, *focused, disabled),
                Node::Checkbox { focused, checked, .. } => {
                    AutomationNode::checkbox(*id, name, *checked, bounds, *focused, disabled)
                }
                Node::TextField { focused, text, .. } => {
                    AutomationNode::textfield(*id, name, &shown(text), bounds, *focused, disabled)
                }
                Node::List { focused, selected, .. } => {
                    let selected = selected.map(|s| s as usize);
                    AutomationNode::list(*id, name, item_ids.clone(), selected, bounds, *focused, disabled)
                }
                Node::Menu { focused, expanded, .. } => {
                    AutomationNode::menu(*id, name, bounds, *focused, disabled, *expanded, item_ids.clone())
                }
            };
            rows.push((place.depth, made.hidden_if(c.hidden)));
            for (index, (item, item_id)) in node.items().iter().zip(item_ids).enumerate() {
                let name = shown(&item.name);
                let (name, bounds) = (&*name, item.bounds.into());
                let made = match node {
                    Node::List { selected, .. } => {
                        let selected = selected.map(|s| s as usize);
                        AutomationNode::list_item(*item_id, name, index, selected, bounds, item.disabled)
                    }
                    _ => AutomationNode::menu_item(*item_id, name, bounds, item.disabled),
                };
                rows.push((place.depth + 1, made.hidden_if(item.hidden)));
            }
        }
        (top, rows)
    }
}

/// A window's ids (the kernel's side, #207): each app id in the newest
/// published tree and its global `lumen_ui::NodeId`, and back (with its
/// role). An id first seen gets a fresh global id; one missing from a newer
/// tree leaves, so its global id names nothing again, and if it comes back
/// it gets a new one. Global ids are never reused.
#[derive(Clone, Debug, Default)]
pub struct IdMap {
    /// App id to global id.
    ids: alloc::collections::BTreeMap<u64, NodeId>,
    /// Global id to app id and role.
    nodes: alloc::collections::BTreeMap<u64, (u64, Role)>,
}

impl IdMap {
    pub fn new() -> IdMap {
        IdMap::default()
    }

    /// Map `tree`'s ids (an id kept from the last tree keeps its global id,
    /// a new one gets `fresh()`), replacing the old map: the kernel's rows,
    /// as [`Tree::rows`] gives them.
    pub fn remap(
        &mut self,
        tree: &Tree,
        mut fresh: impl FnMut() -> NodeId,
    ) -> (Vec<NodeId>, Vec<(u32, AutomationNode)>) {
        let old = core::mem::take(&mut self.ids);
        self.nodes.clear();
        let (ids, nodes) = (&mut self.ids, &mut self.nodes);
        tree.rows(|local, role| {
            let global = old.get(&local).copied().unwrap_or_else(&mut fresh);
            ids.insert(local, global);
            nodes.insert(global.get(), (local, role));
            global
        })
    }

    /// The app id and role of global id `global`, if it is in the newest
    /// tree.
    pub fn local(&self, global: u64) -> Option<(u64, Role)> {
        self.nodes.get(&global).copied()
    }

    /// The global id of app id `local`, if it is in the newest tree.
    pub fn global(&self, local: u64) -> Option<NodeId> {
        self.ids.get(&local).copied()
    }

    /// How many ids it maps (nodes and items).
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// A name or a text as `ui tree` shows it: sanitised.
fn shown(t: &Text) -> crate::text::LogText {
    crate::text::cut(t.as_str(), TEXT_BYTES)
}

/// A node's or an item's common fields from its automation node.
fn common_of(n: &AutomationNode) -> Common {
    Common {
        id: n.id().get(),
        name: Text::cut(n.name()),
        bounds: n.bounds().into(),
        disabled: n.state().contains(StateFlags::DISABLED),
        hidden: n.state().contains(StateFlags::HIDDEN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use postcard::Error as P;

    fn c(id: u64) -> Common {
        Common { id, name: Text::cut("a"), bounds: Rect::new(-1, 2, 3, 4), disabled: true, hidden: false }
    }

    /// `c(1)`'s bytes: id, name, bounds (zigzagged x), disabled, hidden.
    const C1: [u8; 9] = [1, 1, b'a', 1, 4, 3, 4, 1, 0];

    fn items(ids: &[u64]) -> Items {
        CappedVec::new(ids.iter().map(|&id| c(id)).collect()).unwrap()
    }

    fn enc(n: &Node) -> Vec<u8> {
        postcard::to_allocvec(n).unwrap()
    }

    /// How many variants `Node` has: written once, for the pin's range and
    /// the past-the-last byte.
    const NODE_VARIANTS: u8 = 7;

    /// Each variant's tag, by an exhaustive match: a variant added fails to
    /// build here until its arm is written; then `NODE_VARIANTS` needs
    /// raising, its case and bytes pinned.
    fn tag(n: &Node) -> u8 {
        match n {
            Node::Group { .. } => 0,
            Node::Label { .. } => 1,
            Node::Button { .. } => 2,
            Node::Checkbox { .. } => 3,
            Node::TextField { .. } => 4,
            Node::List { .. } => 5,
            Node::Menu { .. } => 6,
        }
    }

    #[test]
    fn every_node_s_encoding_is_pinned() {
        let cases: Vec<(Node, Vec<u8>)> = vec![
            (Node::Group { common: c(1), children: 2 }, [&[0][..], &C1, &[2]].concat()),
            (Node::Label { common: c(1), text: Text::cut("t") }, [&[1][..], &C1, &[1, b't']].concat()),
            (Node::Button { common: c(1), focused: true }, [&[2][..], &C1, &[1]].concat()),
            (Node::Checkbox { common: c(1), focused: false, checked: true }, [&[3][..], &C1, &[0, 1]].concat()),
            (
                Node::TextField { common: c(1), focused: true, text: Text::cut("x") },
                [&[4][..], &C1, &[1, 1, b'x']].concat(),
            ),
            (
                Node::List { common: c(1), focused: false, selected: Some(1), items: items(&[2, 3]) },
                [&[5][..], &C1, &[0, 1, 1, 2, 2], &C1[1..], &[3], &C1[1..]].concat(),
            ),
            (
                Node::Menu { common: c(1), focused: true, expanded: false, items: items(&[]) },
                [&[6][..], &C1, &[1, 0, 0]].concat(),
            ),
        ];
        let mut tags: Vec<u8> = cases.iter().map(|(n, _)| tag(n)).collect();
        for (node, bytes) in &cases {
            assert_eq!(&enc(node), bytes, "{node:?}");
            assert_eq!(bytes[0], tag(node));
            assert_eq!(postcard::from_bytes::<Node>(bytes).as_ref(), Ok(node));
        }
        tags.sort_unstable();
        assert_eq!(tags, (0..NODE_VARIANTS).collect::<Vec<u8>>(), "every variant pinned once");
    }

    /// A tree of `nodes`, encoded.
    fn bytes_of(nodes: &[Node]) -> Vec<u8> {
        nodes.iter().flat_map(enc).collect()
    }

    fn group(id: u64, children: u32) -> Node {
        Node::Group { common: c(id), children }
    }

    fn button(id: u64) -> Node {
        Node::Button { common: c(id), focused: false }
    }

    #[test]
    fn the_tree_s_rules_are_checked() {
        // A group of two (the second a list with two items), then a top button.
        let list = Node::List { common: c(3), focused: false, selected: Some(1), items: items(&[4, 5]) };
        let ok = vec![group(1, 2), button(2), list.clone(), button(6)];
        let tree = Tree::decode(&bytes_of(&ok)).unwrap();
        assert_eq!(tree.nodes(), &ok[..]);
        assert_eq!(tree.count(), 6);
        assert_eq!(Tree::decode(&[]).map(|t| t.count()), Ok(0), "no bytes: no nodes");
        // Ids: non-zero, unique (items' too).
        assert_eq!(Tree::new(vec![button(0)]), Err(TreeError::ZeroId));
        assert_eq!(Tree::new(vec![button(2), button(2)]), Err(TreeError::DuplicateId(2)));
        let clash = Node::List { common: c(3), focused: false, selected: None, items: items(&[4, 3]) };
        assert_eq!(Tree::new(vec![clash]), Err(TreeError::DuplicateId(3)));
        let zero_item = Node::Menu { common: c(3), focused: false, expanded: true, items: items(&[0]) };
        assert_eq!(Tree::new(vec![zero_item]), Err(TreeError::ZeroId));
        // A selection below the items' count.
        let past = Node::List { common: c(3), focused: false, selected: Some(2), items: items(&[4, 5]) };
        assert_eq!(Tree::new(vec![button(1), past]), Err(TreeError::Selection { node: 1 }));
        let none = Node::List { common: c(3), focused: false, selected: Some(0), items: items(&[]) };
        assert_eq!(Tree::new(vec![none]), Err(TreeError::Selection { node: 0 }));
        // Far edges within i32: exactly at i32::MAX fits, one past doesn't.
        let at =
            |x: i32, w: u32| Node::Button { common: Common { bounds: Rect::new(x, 0, w, 1), ..c(1) }, focused: false };
        assert!(Tree::new(vec![at(i32::MAX - 10, 10)]).is_ok());
        assert_eq!(Tree::new(vec![at(i32::MAX - 10, 11)]), Err(TreeError::Bounds { node: 0 }));
        assert!(Tree::new(vec![at(i32::MIN, u32::MAX)]).is_ok(), "-2^31 + 2^32 - 1 = i32::MAX");
        let tall =
            Node::Label { common: Common { bounds: Rect::new(0, 1, 1, i32::MAX as u32), ..c(1) }, text: Text::cut("") };
        assert_eq!(Tree::new(vec![tall]), Err(TreeError::Bounds { node: 0 }));
        let mut wide_item = c(4);
        wide_item.bounds = Rect::new(1, 0, i32::MAX as u32, 1);
        let bad_item = Node::Menu {
            common: c(3),
            focused: false,
            expanded: true,
            items: CappedVec::new(vec![wide_item]).unwrap(),
        };
        assert_eq!(Tree::new(vec![button(1), bad_item]), Err(TreeError::Bounds { node: 1 }));
        // At most one node focused.
        let focused = |id| Node::Button { common: c(id), focused: true };
        assert!(Tree::new(vec![focused(1), button(2)]).is_ok());
        assert_eq!(Tree::new(vec![focused(1), button(2), focused(3)]), Err(TreeError::Focus));
        // TREE_NODES nodes, items counted, by `new` too.
        assert_eq!(Tree::new((1..=TREE_NODES as u64 + 1).map(button).collect()), Err(TreeError::TooManyNodes));
        assert!(Tree::new((1..=TREE_NODES as u64).map(button).collect()).is_ok());
        // Children counts consuming the nodes exactly.
        assert_eq!(Tree::new(vec![group(1, 2), button(2)]), Err(TreeError::Children), "one short");
        assert!(Tree::new(vec![group(1, 1), button(2), button(3)]).is_ok(), "the third at the top");
        assert_eq!(Tree::new(vec![group(1, u32::MAX), button(2)]), Err(TreeError::Children));
        assert!(Tree::new(vec![group(1, 0), group(2, 0)]).is_ok());
    }

    #[test]
    fn names_and_texts_are_capped_and_shown_sanitised() {
        // Past TEXT_BYTES: refused when decoded (an app's toolkit cuts it).
        let mut long = vec![1u8, 1, 0x81, 0x08];
        long.extend(core::iter::repeat_n(b'x', TEXT_BYTES + 1));
        assert_eq!(Tree::decode(&long), Err(TreeError::Decode { node: 0, error: P::SerdeDeCustom }));
        assert_eq!(Text::cut(&"é".repeat(TEXT_BYTES)).as_str().len(), TEXT_BYTES);
        // Control characters and line separators: U+FFFD in the kernel's
        // rows, so no name or text can forge a line of `ui tree`.
        let bad = "a\u{1b}b\nc\u{2028}d\u{2029}e";
        let named = Common { name: Text::cut(bad), ..c(1) };
        let tree = Tree::new(vec![Node::Label { common: named, text: Text::cut(bad) }]).unwrap();
        let (_, rows) = tree.rows(|_, _| NodeId::fresh());
        let shown = "a\u{fffd}b\u{fffd}c\u{fffd}d\u{fffd}e";
        assert_eq!(rows[0].1.name(), shown);
        assert_eq!(rows[0].1.value(), Some(&Value::Text(alloc::string::String::from(shown))));
    }

    #[test]
    fn ids_keep_their_global_id_and_one_that_leaves_comes_back_new() {
        let list =
            |items: &[u64]| Node::List { common: c(3), focused: false, selected: None, items: self::items(items) };
        let mut map = IdMap::new();
        let (_, first) = map.remap(&Tree::new(vec![button(1), list(&[4, 5])]).unwrap(), NodeId::fresh);
        let g = |rows: &[(u32, AutomationNode)], i: usize| rows[i].1.id();
        // Kept ids keep their global ids; each role is the node's own (a
        // list's items are list items, which take `select`).
        assert_eq!(map.local(g(&first, 0).get()), Some((1, Role::Button)));
        assert_eq!(map.local(g(&first, 2).get()), Some((4, Role::ListItem)));
        assert!(Role::ListItem.actions().contains(&lumen_ui::ActionKind::Select));
        assert_eq!(map.len(), 4);
        // 1 leaves; 4 stays; 6 is new.
        let (_, second) = map.remap(&Tree::new(vec![list(&[4, 6])]).unwrap(), NodeId::fresh);
        assert_eq!(map.global(4), Some(g(&first, 2)));
        assert_eq!(map.local(g(&first, 0).get()), None, "a leaving id names nothing");
        assert_eq!(map.global(1), None);
        let new6 = map.global(6).unwrap();
        assert!(![g(&first, 0), g(&first, 1), g(&first, 2), g(&first, 3)].contains(&new6));
        assert_eq!(g(&second, 2), new6);
        // 1 comes back: a new global id, never its old one.
        map.remap(&Tree::new(vec![button(1), list(&[4, 6])]).unwrap(), NodeId::fresh);
        let back = map.global(1).unwrap();
        assert_ne!(back, g(&first, 0));
        assert_eq!(map.local(back.get()), Some((1, Role::Button)));
        // A menu's items are menu items.
        let menu = Node::Menu { common: c(7), focused: false, expanded: true, items: items(&[8]) };
        map.remap(&Tree::new(vec![menu]).unwrap(), NodeId::fresh);
        assert_eq!(map.global(8).and_then(|g| map.local(g.get())), Some((8, Role::MenuItem)));
        assert!(!map.is_empty());
    }

    #[test]
    fn a_refused_tree_still_counts_the_nodes_it_decoded() {
        // An id twice, found after both were decoded (and sorted): both
        // counted. A node that doesn't decode: those before it. Too big:
        // none.
        let list = Node::List { common: c(3), focused: false, selected: None, items: items(&[4, 5]) };
        let (got, n) = Tree::decode_counted(&bytes_of(&[button(1), list, button(1)]));
        assert_eq!((got, n), (Err(TreeError::DuplicateId(1)), 5));
        let (got, n) = Tree::decode_counted(&[&bytes_of(&[button(1), button(2)])[..], &[0xff]].concat());
        assert!(matches!(got, Err(TreeError::Decode { node: 2, .. })));
        assert_eq!(n, 2);
        assert_eq!(Tree::decode_counted(&vec![0; TREE_BYTES + 1]).1, 0);
        assert_eq!(Tree::decode_counted(&bytes_of(&[button(1)])).1, 1);
    }

    #[test]
    fn depths_and_parents_follow_the_children_counts() {
        // 1 (2: 2, 3 (1: 4)), 5
        let nodes = vec![group(1, 2), button(2), group(3, 1), button(4), button(5)];
        let tree = Tree::new(nodes).unwrap();
        let ids: Vec<NodeId> = (0..5).map(|_| NodeId::fresh()).collect();
        let mut asked = Vec::new();
        let (top, rows) = tree.rows(|local, role| {
            asked.push((local, role));
            ids[local as usize - 1]
        });
        let (g, b) = (Role::Group, Role::Button);
        assert_eq!(asked, [(1, g), (2, b), (3, g), (4, b), (5, b)], "asked in pre-order, with roles");
        // A list or a menu before its items, each item with its role.
        let list = Node::List { common: c(1), focused: false, selected: None, items: items(&[2, 3]) };
        let menu = Node::Menu { common: c(4), focused: false, expanded: true, items: items(&[5]) };
        let mut asked = Vec::new();
        let _ = Tree::new(vec![list, menu]).unwrap().rows(|local, role| {
            asked.push((local, role));
            NodeId::fresh()
        });
        let (l, li, m, mi) = (Role::List, Role::ListItem, Role::Menu, Role::MenuItem);
        assert_eq!(asked, [(1, l), (2, li), (3, li), (4, m), (5, mi)], "a node before its items");
        assert_eq!(top, [ids[0], ids[4]]);
        let depths: Vec<u32> = rows.iter().map(|(d, _)| *d).collect();
        assert_eq!(depths, [1, 2, 2, 3, 1]);
        assert_eq!(rows[0].1.children(), &[ids[1], ids[2]]);
        assert_eq!(rows[2].1.children(), &[ids[3]]);
    }

    #[test]
    fn a_chain_as_deep_as_tree_nodes_decodes_and_one_more_is_refused() {
        // Every node a group holding the next: the deepest tree there is,
        // decoded and shaped without recursion.
        let chain = |n: u64| -> Vec<Node> { (1..=n).map(|id| group(id, u32::from(id < n))).collect() };
        let bytes = bytes_of(&chain(TREE_NODES as u64));
        assert!(bytes.len() <= TREE_BYTES);
        let tree = Tree::decode(&bytes).unwrap();
        let (top, rows) = tree.rows(|_, _| NodeId::fresh());
        assert_eq!(top.len(), 1);
        assert_eq!(rows.last().map(|(d, _)| *d), Some(TREE_NODES as u32));
        assert_eq!(Tree::decode(&bytes_of(&chain(TREE_NODES as u64 + 1))), Err(TreeError::TooManyNodes));
        // Items count: a list of TREE_NODES - 1 items and the list is
        // exactly TREE_NODES; one more item is past it.
        let ids: Vec<u64> = (2..=TREE_NODES as u64).collect();
        let full = Node::List { common: c(1), focused: false, selected: None, items: items(&ids) };
        assert_eq!(Tree::decode(&bytes_of(core::slice::from_ref(&full))).map(|t| t.count()), Ok(TREE_NODES));
        assert_eq!(Tree::decode(&bytes_of(&[full, button(5000)])), Err(TreeError::TooManyNodes));
    }

    /// Labels (texts at most `TEXT_BYTES`) whose encoding is exactly `len`
    /// bytes (`len` at least a few hundred).
    fn padded(len: usize) -> Vec<u8> {
        let label = |id: u64, n: usize| enc(&Node::Label { common: c(id), text: Text::cut(&"x".repeat(n)) });
        let mut out = Vec::new();
        let mut id = 1;
        // Whole labels of 1000 characters while more than two fit.
        while len - out.len() > 2 * label(id, 1000).len() {
            out.extend(label(id, 1000));
            id += 1;
        }
        // Then two that make up the rest exactly (each length prefix 1 or 2
        // bytes: the text's size tried until the bytes add up).
        let rest = len - out.len();
        let half = rest / 2;
        out.extend(label(id, half - 12));
        let left = len - out.len();
        let n = (0..TEXT_BYTES).find(|&n| label(id + 1, n).len() == left).expect("a text that pads it exactly");
        out.extend(label(id + 1, n));
        assert_eq!(out.len(), len);
        out
    }

    #[test]
    fn tree_bytes_exactly_is_accepted_and_one_past_refused_before_decoding() {
        assert!(Tree::decode(&padded(TREE_BYTES)).is_ok());
        assert_eq!(Tree::decode(&padded(TREE_BYTES + 1)), Err(TreeError::TooBig(TREE_BYTES + 1)));
        // Sizes first: bytes that wouldn't decode at all, one past the cap,
        // are refused for their size.
        assert_eq!(Tree::decode(&vec![0xff; TREE_BYTES + 1]), Err(TreeError::TooBig(TREE_BYTES + 1)));
        assert!(matches!(Tree::decode(&vec![0xff; TREE_BYTES]), Err(TreeError::Decode { node: 0, .. })));
        // Encoding refuses what decoding would.
        let label = |id| Node::Label { common: c(id), text: Text::cut(&"x".repeat(TEXT_BYTES)) };
        let big = Tree::new((1..=70).map(label).collect()).unwrap();
        assert!(matches!(big.encode(), Err(TreeError::TooBig(_))));
    }

    #[test]
    fn malformed_values_are_refused_with_postcard_s_errors() {
        // Each in node 1 (after a good button), so the node is counted too.
        let lead = enc(&button(9));
        let bad = |rest: &[u8]| Tree::decode(&[&lead[..], rest].concat());
        let err = |error| Err(TreeError::Decode { node: 1, error });
        // A label named with bad UTF-8.
        assert_eq!(bad(&[1, 1, 1, 0xff]), err(P::DeserializeBadUtf8));
        // A button's `disabled` of 2.
        assert_eq!(bad(&[2, 1, 0, 0, 0, 0, 0, 2]), err(P::DeserializeBadBool));
        // An id past u64 (an overflowing varint).
        assert_eq!(bad(&[2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]), err(P::DeserializeBadVarint));
        // A list's selection with an `Option` tag of 2.
        assert_eq!(bad(&[5, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2]), err(P::DeserializeBadOption));
        // A variant past the last (serde's own message: postcard's custom
        // error).
        assert_eq!(bad(&[NODE_VARIANTS]), err(P::SerdeDeCustom));
        // Truncated.
        assert_eq!(bad(&[2, 1]), err(P::DeserializeUnexpectedEnd));
        // No wire type here holds a `char` (the surrogate case is `Event`'s).
    }

    #[test]
    fn item_lists_are_refused_past_their_cap_before_anything_is_allocated() {
        // A list claiming TREE_NODES + 1 items, then nothing: refused by the
        // capped visitor (or the end of the input), never preallocated.
        let mut b = vec![5, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&[0x81, 0x08]); // 1025
        assert!(matches!(Tree::decode(&b), Err(TreeError::Decode { node: 0, .. })));
        // Claiming u32::MAX items.
        let mut b = vec![5, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert!(matches!(Tree::decode(&b), Err(TreeError::Decode { node: 0, .. })));
        // A length past the cap with the bytes to back it: the cap's custom
        // error, not the end of the input.
        let many: Vec<u8> =
            [&[5u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x81, 0x08][..], &[1, 0, 0, 0, 0, 0, 0, 0].repeat(TREE_NODES + 1)]
                .concat();
        assert_eq!(Tree::decode(&many), Err(TreeError::Decode { node: 0, error: P::SerdeDeCustom }));
    }

    /// A tree with a node of every kind.
    fn sample() -> Vec<u8> {
        bytes_of(&[
            group(1, 3),
            Node::Label { common: c(2), text: Text::cut("12") },
            Node::Checkbox { common: c(3), focused: true, checked: false },
            Node::List { common: c(4), focused: false, selected: Some(0), items: items(&[5, 6]) },
            Node::TextField { common: c(7), focused: false, text: Text::cut("é") },
            Node::Menu { common: c(8), focused: false, expanded: true, items: items(&[9]) },
        ])
    }

    #[test]
    fn decoding_never_panics_on_truncations_flips_or_trailing_bytes() {
        let good = sample();
        assert!(Tree::decode(&good).is_ok());
        for n in 0..good.len() {
            let _ = Tree::decode(&good[..n]);
        }
        for i in 0..good.len() {
            for v in 0..=255u8 {
                let mut b = good.clone();
                b[i] = v;
                let _ = Tree::decode(&b);
            }
        }
        for extra in [&[0u8][..], &[0xff], &[2, 1], &[7, 7, 7]] {
            let b = [&good[..], extra].concat();
            assert!(Tree::decode(&b).is_err(), "trailing {extra:?}: a node that doesn't decode");
        }
        // Random bytes.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 64) as usize;
            let b: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8 ^ i as u8).collect();
            let _ = Tree::decode(&b);
        }
    }

    #[test]
    fn an_app_s_toolkit_tree_goes_over_the_wire_and_back() {
        // Every widget lumen-ui has, as an app publishes it and the kernel
        // reads it: the same rows, with each app id mapped once.
        use lumen_ui::{FixedText, Ui};
        let mut ui = Ui::new();
        let top = ui.vstack(None, "Top");
        ui.label(Some(top), "Caption", "Count: 3");
        let add = ui.button(Some(top), "Add");
        ui.checkbox(Some(top), "Lock", true);
        ui.text_field(Some(top), "Name", "é");
        let list = ui.list(Some(top), "Files", &["a", "b", "c"]);
        let menu = ui.menu(None, "File", &["New", "Quit"]);
        ui.set_disabled(add, true);
        ui.layout(lumen_gfx::Rect::new(0, 0, 400, 400), &mut FixedText);
        let _ = ui.invoke(ui.item_id(list, 1).unwrap().get(), &lumen_ui::Action::Select);
        let _ = ui.invoke(ui.id(menu).unwrap().get(), &lumen_ui::Action::Press);
        ui.layout(lumen_gfx::Rect::new(0, 0, 400, 400), &mut FixedText);
        let rows = ui.nodes();
        let tree = Tree::from_automation(&rows).unwrap();
        let back = Tree::decode(&tree.encode().unwrap()).unwrap();
        assert_eq!(back, tree);
        let mut map: Vec<(u64, NodeId, Role)> = Vec::new();
        let (top_ids, got) = back.rows(|local, role| {
            let id = NodeId::fresh();
            map.push((local, id, role));
            id
        });
        // Each id asked with the role its row gets.
        for (_, id, role) in &map {
            assert_eq!(got.iter().find(|(_, n)| n.id() == *id).map(|(_, n)| n.role()), Some(*role));
        }
        let global = |local: NodeId| map.iter().find(|(l, ..)| *l == local.get()).map(|(_, g, _)| *g).unwrap();
        assert_eq!(top_ids, ui.top_ids().into_iter().map(global).collect::<Vec<_>>());
        assert_eq!(got.len(), rows.len());
        for ((d0, a), (d1, b)) in rows.iter().zip(&got) {
            assert_eq!(d0, d1);
            assert_eq!(global(a.id()), b.id());
            assert_eq!(
                (a.role(), a.name(), a.value(), a.state(), a.bounds()),
                (b.role(), b.name(), b.value(), b.state(), b.bounds())
            );
            assert_eq!(a.children().iter().map(|&c| global(c)).collect::<Vec<_>>(), b.children());
        }
        // Hidden nodes and items go over too: a bar too narrow for its
        // widgets hides those past its pinned one.
        let mut bar = Ui::bar(4);
        for name in ["One", "Two", "Three", "Four"] {
            bar.button(None, name);
        }
        let pinned = bar.button(None, "Pinned");
        assert!(bar.pin_right(pinned));
        bar.layout(lumen_gfx::Rect::new(0, 0, 120, 30), &mut FixedText);
        let bar_rows = bar.nodes();
        assert!(bar_rows.iter().any(|(_, n)| n.state().contains(StateFlags::HIDDEN)), "something hidden");
        let back = Tree::decode(&Tree::from_automation(&bar_rows).unwrap().encode().unwrap()).unwrap();
        let (_, got) = back.rows(|_, _| NodeId::fresh());
        let hidden = |rows: &[(u32, AutomationNode)]| -> Vec<bool> {
            rows.iter().map(|(_, n)| n.state().contains(StateFlags::HIDDEN)).collect()
        };
        assert_eq!(hidden(&bar_rows), hidden(&got));
        // A row the wire can't hold: a window's root, a stray item, a list
        // followed by a row that isn't its item.
        let root = AutomationNode::window(NodeId::fresh(), "w", lumen_gfx::Rect::new(0, 0, 1, 1), false, Vec::new());
        assert_eq!(Tree::from_automation(&[(0, root)]), Err(TreeError::Shape { row: 0 }));
        let item_row = rows.iter().position(|(_, n)| n.role() == Role::ListItem).unwrap();
        assert_eq!(Tree::from_automation(&rows[item_row..]), Err(TreeError::Shape { row: 0 }));
        let mut wrong = rows.clone();
        let list_row = item_row - 1;
        wrong[item_row] = (
            wrong[item_row].0,
            AutomationNode::button(NodeId::fresh(), "x", lumen_gfx::Rect::new(0, 0, 1, 1), false, false),
        );
        assert_eq!(Tree::from_automation(&wrong), Err(TreeError::Shape { row: list_row }));
    }
}
