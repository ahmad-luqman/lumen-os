//! What a window's published tree costs the kernel's heap (#207;
//! `TREE_COPIES_BYTES`, ARCHITECTURE section 8, "Memory"): the sequence
//! `window_publish` runs, through the same functions (the bytes copied in,
//! `Tree::decode_counted`, the bytes dropped, `IdMap::remap`, the old rows
//! dropped once the new are swapped in), on the worst trees there are,
//! with the old tree still held. A counting global allocator counts each
//! block as the kernel's heap sizes it (`lumen_heap::block_size`), in a
//! test binary of its own with one test, so nothing else allocates
//! meanwhile. What a window holds after a publish, and the peak during one,
//! stay within `TREE_COPIES_BYTES`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_app_api::layout::Rect;
use lumen_app_api::limits::{TEXT_BYTES, TREE_BYTES, TREE_COPIES_BYTES, TREE_NODES};
use lumen_app_api::tree::{Common, IdMap, Node, Text, Tree};
use lumen_app_api::wire::CappedVec;
use lumen_ui::{AutomationNode, NodeId};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn block(layout: Layout) -> usize {
    lumen_heap::block_size(layout).unwrap_or(layout.size())
}

// SAFETY: every call is forwarded to `System`, which upholds
// `GlobalAlloc`'s contract; the counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(block(layout), Ordering::SeqCst) + block(layout);
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        NOW.fetch_sub(block(layout), Ordering::SeqCst);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// What a window holds: its desktop's copy (the top ids and the rows) and
/// its id map.
struct Held {
    top: Vec<NodeId>,
    rows: Vec<(u32, AutomationNode)>,
    ids: IdMap,
}

/// `window_publish`'s steps on `bytes` (as the app sent them), the old
/// tree in `held` swapped out last. The order is the kernel's
/// (`app/window.rs`): the copied bytes dropped once decoded, the decoded
/// tree dropped once its rows are built; dropping either later raises the
/// peak (to about 990 KB).
fn publish(held: &mut Held, sent: &[u8]) {
    let bytes = sent.to_vec(); // copy_in_capped
    let (tree, _) = Tree::decode_counted(&bytes);
    drop(bytes);
    let tree = tree.expect("a valid tree");
    let (top, rows) = held.ids.remap(&tree, NodeId::fresh);
    drop(tree);
    let old = (std::mem::replace(&mut held.top, top), std::mem::replace(&mut held.rows, rows));
    drop(old);
}

/// Control characters: each shown as U+FFFD (3 bytes) in the kernel's rows.
fn odd(n: usize) -> Text {
    Text::cut(&"\u{1b}".repeat(n))
}

fn common(id: u64, name: Text) -> Common {
    Common { id, name, bounds: Rect::new(0, 0, 1, 1), disabled: false, hidden: false }
}

fn encode(nodes: Vec<Node>) -> Vec<u8> {
    let bytes = Tree::new(nodes).expect("valid").encode().expect("within TREE_BYTES");
    assert!(bytes.len() <= TREE_BYTES);
    bytes
}

/// The worst trees: as many nodes as allowed, their names and texts filling
/// `TREE_BYTES` with characters the kernel shows three times as long; few
/// nodes with the longest texts; one list of `TREE_NODES - 1` items.
fn worst() -> Vec<(&'static str, Vec<u8>)> {
    let per = (TREE_BYTES - 16) / TREE_NODES - 14;
    let many = (1..=TREE_NODES as u64)
        .map(|id| Node::Label { common: common(id, odd(per / 2)), text: odd(per - per / 2) })
        .collect();
    let long = (1..=60u64).map(|id| Node::Label { common: common(id, odd(32)), text: odd(TEXT_BYTES - 40) }).collect();
    let items = (2..=TREE_NODES as u64).map(|id| common(id, odd(48))).collect();
    let list = vec![Node::List {
        common: common(1, odd(16)),
        focused: false,
        selected: None,
        items: CappedVec::new(items).unwrap(),
    }];
    // A chain as deep as allowed, each group named with ESCs.
    let name = (TREE_BYTES - 16) / TREE_NODES - 11;
    let chain = (1..=TREE_NODES as u64)
        .map(|id| Node::Group { common: common(id, odd(name)), children: u32::from(id < TREE_NODES as u64) })
        .collect();
    // Labels with short names and long texts.
    let lopsided =
        (1..=TREE_NODES as u64).map(|id| Node::Label { common: common(id, odd(11)), text: odd(42) }).collect();
    vec![
        ("many labels", encode(many)),
        ("long texts", encode(long)),
        ("a long list", encode(list)),
        ("a deep chain", encode(chain)),
        ("labels of 11 + 42", encode(lopsided)),
    ]
}

#[test]
fn a_published_tree_stays_within_its_charge() {
    let trees = worst();
    let mut highest = (0, 0);
    for (what, bytes) in &trees {
        let base = NOW.load(Ordering::SeqCst);
        let mut held = Held { top: Vec::new(), rows: Vec::new(), ids: IdMap::new() };
        // A first publish (from nothing), then the same tree again with
        // the old one held: the steady state's peak.
        PEAK.store(base, Ordering::SeqCst);
        publish(&mut held, bytes);
        let after = NOW.load(Ordering::SeqCst) - base;
        PEAK.store(NOW.load(Ordering::SeqCst), Ordering::SeqCst);
        publish(&mut held, bytes);
        let peak = PEAK.load(Ordering::SeqCst) - base;
        println!("{what} ({} bytes sent): {after} bytes held, {peak} at a republish's peak", bytes.len());
        highest = (highest.0.max(after), highest.1.max(peak));
        drop(held);
    }
    println!("TREE_COPIES_BYTES {TREE_COPIES_BYTES}: held at most {}, peak at most {}", highest.0, highest.1);
    assert!(highest.1 as u64 <= TREE_COPIES_BYTES, "{highest:?}");
}
