//! The heap's peak while the wire decoders take hostile input (ARCHITECTURE
//! section 8, "Sequences"): a counting global allocator, in a test binary of
//! its own with one test, so nothing else allocates meanwhile. A length
//! claimed past a sequence's cap must not reserve for it (refused first),
//! and one claimed past the bytes left reserves nothing (postcard gives no
//! hint then); the largest valid trees stay within 4 × `TREE_BYTES` at the
//! peak (an absolute bound, measured on them).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_app_api::answer::AppAnswer;
use lumen_app_api::limits::{TREE_BYTES, TREE_NODES};
use lumen_app_api::tree::{Common, Node, Tree};
use lumen_app_api::wire::CappedVec;

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to `System`, which upholds
// `GlobalAlloc`'s contract; the counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        NOW.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The peak above what was allocated before `f` ran.
fn peak_of<T>(f: impl FnOnce() -> T) -> usize {
    let base = NOW.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    drop(f());
    PEAK.load(Ordering::SeqCst) - base
}

fn common(id: u64) -> Common {
    Common {
        id,
        name: lumen_app_api::tree::Text::cut(""),
        bounds: lumen_app_api::layout::Rect::new(0, 0, 1, 1),
        disabled: false,
        hidden: false,
    }
}

#[test]
fn hostile_lengths_reserve_nothing_and_the_largest_tree_stays_small() {
    // A list node, then a claimed item count and few bytes behind it.
    let list_head = postcard::to_allocvec(&Node::List {
        common: common(1),
        focused: false,
        selected: None,
        items: CappedVec::new(Vec::new()).unwrap(),
    })
    .unwrap();
    let head = &list_head[..list_head.len() - 1];
    let claims: [&[u8]; 3] = [
        &[0xff, 0xff, 0xff, 0xff, 0x0f], // u32::MAX items, no bytes
        &[0x80, 0x80, 0x04],             // 65536 items: past the cap
        &[0x80, 0x08],                   // TREE_NODES items, no bytes
    ];
    for claim in claims {
        let bytes = [head, claim].concat();
        let peak = peak_of(|| Tree::decode(&bytes));
        assert!(Tree::decode(&bytes).is_err());
        assert!(peak <= 4096, "claim {claim:02x?}: {peak} bytes at the peak");
    }
    // A claim the bytes left could back (postcard then hints its length):
    // 60,000 items with 60,000 bytes behind them. A derived `Vec<Item>`
    // would reserve up to 1 MiB of items here; the capped visitor refuses
    // the length first.
    let mut backed = [head, &[0xe0, 0xd4, 0x03][..]].concat();
    backed.resize(backed.len() + 60_000, 0);
    assert!(backed.len() <= TREE_BYTES);
    let peak = peak_of(|| Tree::decode(&backed));
    assert!(Tree::decode(&backed).is_err());
    assert!(peak <= 4096, "a backed claim: {peak} bytes at the peak");
    // A claim one past the bytes left (no hint: nothing reserved for it),
    // its elements decoded until the input ends: within 2 × TREE_BYTES.
    let item = &postcard::to_allocvec(&common(9)).unwrap();
    let n = (TREE_BYTES - 16) / item.len();
    let mut short = [head, &postcard::to_allocvec(&(n as u32 + 1)).unwrap()[..]].concat();
    for i in 0..n {
        let mut it = postcard::to_allocvec(&common(2 + i as u64)).unwrap();
        it.truncate(item.len().min(it.len()));
        short.extend(it);
    }
    let short = &short[..short.len().min(TREE_BYTES)];
    let peak = peak_of(|| Tree::decode(short));
    assert!(Tree::decode(short).is_err());
    assert!(peak <= 2 * TREE_BYTES, "a claim one past the bytes left: {peak} bytes at the peak");
    // Seven lists of TREE_NODES - 1 items each, with the bytes behind
    // them: refused once the nodes pass TREE_NODES, after the first two.
    let full = postcard::to_allocvec(&Node::List {
        common: common(1),
        focused: false,
        selected: None,
        items: CappedVec::new((2..=TREE_NODES as u64).map(common).collect()).unwrap(),
    })
    .unwrap();
    let seven: Vec<u8> = full.iter().copied().cycle().take(7 * full.len()).collect();
    let seven = &seven[..seven.len().min(TREE_BYTES)];
    let peak = peak_of(|| Tree::decode(seven));
    assert!(Tree::decode(seven).is_err());
    assert!(peak <= 4 * TREE_BYTES, "seven long lists: {peak} bytes at the peak");
    // An answer claiming a huge text: postcard checks the bytes left first.
    let peak = peak_of(|| AppAnswer::decode(&[3, 0xff, 0xff, 0xff, 0xff, 0x0f, b'a']));
    assert!(peak <= 64, "{peak}");
    // The largest valid trees: TREE_NODES buttons, and one list of
    // TREE_NODES - 1 items, both within TREE_BYTES: decoded within a small
    // multiple of TREE_BYTES (the nodes' vector growing, their names).
    let buttons: Vec<u8> = (1..=TREE_NODES as u64)
        .flat_map(|id| postcard::to_allocvec(&Node::Button { common: common(id), focused: false }).unwrap())
        .collect();
    let items = CappedVec::new((2..=TREE_NODES as u64).map(common).collect()).unwrap();
    let list = postcard::to_allocvec(&Node::List { common: common(1), focused: false, selected: None, items }).unwrap();
    for bytes in [buttons, list] {
        assert!(bytes.len() <= TREE_BYTES);
        let peak = peak_of(|| Tree::decode(&bytes).unwrap());
        println!("a tree of {} bytes: {peak} bytes at the peak", bytes.len());
        assert!(peak <= 4 * TREE_BYTES, "{peak}");
    }
}
