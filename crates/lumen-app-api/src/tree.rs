//! [`Tree`], what an app publishes with `window_publish` (#207;
//! ARCHITECTURE section 8, "Wire formats" and "Apps' automation trees"):
//! the app's `lumen-ui` nodes in pre-order, each a role-fused [`Node`]
//! mirroring `lumen-ui`'s per-role constructors, so a mismatched value, a
//! flag a role doesn't allow, a window's root, children under a leaf, a
//! stray item and a disagreeing selection can't be written at all.
//!
//! What is left to check, [`Tree::decode`] checks: ids non-zero and unique
//! (items' too); a list's selection below its items' count; every
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
use crate::limits::{TREE_BYTES, TREE_NODES};
use crate::wire::CappedVec;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lumen_ui::{AutomationNode, NodeId, Role, StateFlags, Value};
use serde::{Deserialize, Serialize};

/// What every node and every item has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Common {
    /// The app's own id (its `lumen_ui::NodeId`): non-zero, unique in the
    /// tree.
    pub id: u64,
    /// The accessible label ("Add").
    pub name: String,
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
        text: String,
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
        text: String,
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
        if bytes.len() > TREE_BYTES {
            return Err(TreeError::TooBig(bytes.len()));
        }
        let mut nodes = Vec::new();
        let mut count = 0;
        let mut rest = bytes;
        while !rest.is_empty() {
            if count >= TREE_NODES {
                return Err(TreeError::TooManyNodes);
            }
            let (node, more) = postcard::take_from_bytes::<Node>(rest)
                .map_err(|error| TreeError::Decode { node: nodes.len(), error })?;
            count += 1 + node.items().len();
            if count > TREE_NODES {
                return Err(TreeError::TooManyNodes);
            }
            nodes.push(node);
            rest = more;
        }
        Tree::new(nodes)
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
                Some(Value::Text(t)) => t.clone(),
                _ => String::new(),
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
    /// menu), client coordinates, each app id `global` maps (asked in
    /// pre-order, a node before its items).
    pub fn rows(&self, mut global: impl FnMut(u64) -> NodeId) -> (Vec<NodeId>, Vec<(u32, AutomationNode)>) {
        let ids: Vec<(NodeId, Vec<NodeId>)> = self
            .nodes
            .iter()
            .map(|n| (global(n.common().id), n.items().iter().map(|it| global(it.id)).collect()))
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
            let (name, bounds, disabled) = (c.name.as_str(), gfx(&c.bounds), c.disabled);
            let made = match node {
                Node::Group { .. } => AutomationNode::group(*id, name, bounds, disabled, kids),
                Node::Label { text, .. } => AutomationNode::label(*id, name, text, bounds, disabled),
                Node::Button { focused, .. } => AutomationNode::button(*id, name, bounds, *focused, disabled),
                Node::Checkbox { focused, checked, .. } => {
                    AutomationNode::checkbox(*id, name, *checked, bounds, *focused, disabled)
                }
                Node::TextField { focused, text, .. } => {
                    AutomationNode::textfield(*id, name, text, bounds, *focused, disabled)
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
                let (name, bounds) = (item.name.as_str(), gfx(&item.bounds));
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

/// A node's or an item's common fields from its automation node.
fn common_of(n: &AutomationNode) -> Common {
    let b = n.bounds();
    Common {
        id: n.id().get(),
        name: n.name().to_string(),
        bounds: Rect::new(b.x(), b.y(), b.w(), b.h()),
        disabled: n.state().contains(StateFlags::DISABLED),
        hidden: n.state().contains(StateFlags::HIDDEN),
    }
}

fn gfx(r: &Rect) -> lumen_gfx::Rect {
    lumen_gfx::Rect::new(r.x, r.y, r.w, r.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use postcard::Error as P;

    fn c(id: u64) -> Common {
        Common { id, name: String::from("a"), bounds: Rect::new(-1, 2, 3, 4), disabled: true, hidden: false }
    }

    /// `c(1)`'s bytes: id, name, bounds (zigzagged x), disabled, hidden.
    const C1: [u8; 9] = [1, 1, b'a', 1, 4, 3, 4, 1, 0];

    fn items(ids: &[u64]) -> Items {
        CappedVec::new(ids.iter().map(|&id| c(id)).collect()).unwrap()
    }

    fn enc(n: &Node) -> Vec<u8> {
        postcard::to_allocvec(n).unwrap()
    }

    /// Each variant's tag, by an exhaustive match: a variant added fails to
    /// build here until its arm is written; then the range checked below
    /// needs its case, and its bytes pinned.
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
            (Node::Label { common: c(1), text: String::from("t") }, [&[1][..], &C1, &[1, b't']].concat()),
            (Node::Button { common: c(1), focused: true }, [&[2][..], &C1, &[1]].concat()),
            (Node::Checkbox { common: c(1), focused: false, checked: true }, [&[3][..], &C1, &[0, 1]].concat()),
            (
                Node::TextField { common: c(1), focused: true, text: String::from("x") },
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
        assert_eq!(tags, (0..=6).collect::<Vec<u8>>(), "every variant pinned once");
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
            Node::Label { common: Common { bounds: Rect::new(0, 1, 1, i32::MAX as u32), ..c(1) }, text: String::new() };
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
        // Children counts consuming the nodes exactly.
        assert_eq!(Tree::new(vec![group(1, 2), button(2)]), Err(TreeError::Children), "one short");
        assert!(Tree::new(vec![group(1, 1), button(2), button(3)]).is_ok(), "the third at the top");
        assert_eq!(Tree::new(vec![group(1, u32::MAX), button(2)]), Err(TreeError::Children));
        assert!(Tree::new(vec![group(1, 0), group(2, 0)]).is_ok());
    }

    #[test]
    fn depths_and_parents_follow_the_children_counts() {
        // 1 (2: 2, 3 (1: 4)), 5
        let nodes = vec![group(1, 2), button(2), group(3, 1), button(4), button(5)];
        let tree = Tree::new(nodes).unwrap();
        let ids: Vec<NodeId> = (0..5).map(|_| NodeId::fresh()).collect();
        let mut asked = Vec::new();
        let (top, rows) = tree.rows(|local| {
            asked.push(local);
            ids[local as usize - 1]
        });
        assert_eq!(asked, [1, 2, 3, 4, 5], "asked in pre-order");
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
        let (top, rows) = tree.rows(|_| NodeId::fresh());
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

    /// A label whose text pads the tree's encoding to exactly `len` bytes.
    fn padded(len: usize) -> Vec<u8> {
        let base = enc(&Node::Label { common: c(1), text: String::new() }).len();
        // The text's length prefix grows with it: 3 bytes for 16 KiB or more.
        let text = "x".repeat(len - base - 2);
        let bytes = enc(&Node::Label { common: c(1), text });
        assert_eq!(bytes.len(), len);
        bytes
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
        let big = Tree::new(vec![Node::Label { common: c(1), text: "x".repeat(TREE_BYTES) }]).unwrap();
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
        assert_eq!(bad(&[7]), err(P::SerdeDeCustom));
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
            Node::Label { common: c(2), text: String::from("12") },
            Node::Checkbox { common: c(3), focused: true, checked: false },
            Node::List { common: c(4), focused: false, selected: Some(0), items: items(&[5, 6]) },
            Node::TextField { common: c(7), focused: false, text: String::from("é") },
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
        let mut map: Vec<(u64, NodeId)> = Vec::new();
        let (top_ids, got) = back.rows(|local| {
            let id = NodeId::fresh();
            map.push((local, id));
            id
        });
        let global = |local: NodeId| map.iter().find(|(l, _)| *l == local.get()).map(|(_, g)| *g).unwrap();
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
        // A row the wire can't hold: a window's root, a stray item.
        let root = AutomationNode::window(NodeId::fresh(), "w", lumen_gfx::Rect::new(0, 0, 1, 1), false, Vec::new());
        assert_eq!(Tree::from_automation(&[(0, root)]), Err(TreeError::Shape { row: 0 }));
        let item_row = rows.iter().position(|(_, n)| n.role() == Role::ListItem).unwrap();
        assert_eq!(Tree::from_automation(&rows[item_row..]), Err(TreeError::Shape { row: 0 }));
    }
}
