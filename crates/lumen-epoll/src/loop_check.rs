//! Nested-epoll cycle and depth check ([`LoopCheck`], Linux v6.17+
//! `fs/eventpoll.c` `ep_loop_check` and `EP_MAX_NESTS`).
//!
//! Linux v6.17+ checks both downward and upward depth in `ep_loop_check`
//! (`depth + 1 + upwards_depth > EP_MAX_NESTS`); older Linux kernels checked
//! only downward from the target in `ep_loop_check_proc` and relied on
//! `reverse_path_check` for upward paths.

use alloc::collections::{BTreeMap, BTreeSet};

use crate::Errno;

/// Maximum nesting depth of epoll instances watching one another
/// (Linux `fs/eventpoll.c`, `EP_MAX_NESTS`).
///
/// Depth counts directed `parent -> child` epoll nesting edges: a chain of 4
/// edges (`E0 -> E1 -> E2 -> E3 -> E4`, 5 epoll instances) is permitted, and
/// a 5th edge (`E0 -> E1 -> E2 -> E3 -> E4 -> E5`) is refused with
/// [`Errno::Loop`].
pub const EP_MAX_NESTS: usize = 4;

/// Directed-graph cycle and nesting-depth checker for nested `epoll`
/// instances, keyed by opaque epoll-instance id `Id`.
///
/// Each edge `(parent, child)` records that epoll instance `parent` (`epfd`)
/// watches epoll instance `child` (`fd`). Duplicate registrations of the same
/// target epoll instance (for example via `dup`'d file descriptors) are
/// reference-counted in `outgoing` so removing one descriptor retains the edge
/// until the last one is removed.
///
/// B5 must keep a 1-to-1 pairing: one [`Self::add`] per successfully added
/// epoll-target interest and one [`Self::remove`] per teardown (undoing
/// [`Self::add`] with [`Self::remove`] if [`crate::EpollSet::add`] subsequently
/// fails).
///
/// Invariant: the graph is always acyclic and every directed path has length
/// at most [`EP_MAX_NESTS`] edges.
#[derive(Clone, Debug, Default)]
pub struct LoopCheck<Id: Ord + Copy> {
    outgoing: BTreeMap<Id, BTreeMap<Id, usize>>,
    incoming: BTreeMap<Id, BTreeSet<Id>>,
}

impl<Id: Ord + Copy> LoopCheck<Id> {
    /// Create an empty nesting graph.
    pub const fn new() -> Self {
        Self { outgoing: BTreeMap::new(), incoming: BTreeMap::new() }
    }

    /// Returns `true` if no nested-epoll edges are recorded.
    pub fn is_empty(&self) -> bool {
        self.outgoing.is_empty()
    }

    /// Returns `true` if `parent` currently watches `child`.
    pub fn contains(&self, parent: Id, child: Id) -> bool {
        self.edge_count(parent, child) > 0
    }

    /// Number of active registrations of `child` inside `parent`.
    pub fn edge_count(&self, parent: Id, child: Id) -> usize {
        self.outgoing.get(&parent).and_then(|m| m.get(&child)).copied().unwrap_or(0)
    }

    /// Record that `parent` (`epfd`) starts watching `child` (`fd`) — i.e.,
    /// adding `child` to `parent`.
    ///
    /// Returns [`Errno::Loop`] (leaving the graph unchanged) if the edge would:
    /// - create a cycle (including a self-loop `parent == child`; note that B5
    ///   checks `epfd == fd` -> `EINVAL` before calling `LoopCheck`), or
    /// - create any directed path longer than [`EP_MAX_NESTS`] edges, matching
    ///   Linux v6.17+ `ep_loop_check` (`up + 1 + down > EP_MAX_NESTS`),
    ///   regardless of insertion order.
    pub fn add(&mut self, parent: Id, child: Id) -> Result<(), Errno> {
        // An edge already present in the graph cannot introduce a cycle or
        // lengthen any path; just bump its reference count.
        if let Some(count) = self.outgoing.get_mut(&parent).and_then(|m| m.get_mut(&child)) {
            *count = count.checked_add(1).expect("epoll nesting edge refcount overflow");
            return Ok(());
        }

        // 1. Cycle check + longest forward path from `child`.
        //    Because the existing graph is a DAG of max depth <= EP_MAX_NESTS,
        //    recursion depth is bounded by EP_MAX_NESTS + 1 and memoization
        //    guarantees O(V + E) on diamond DAGs.
        let mut fwd_memo = BTreeMap::new();
        let down = forward_scan(&self.outgoing, child, parent, &mut fwd_memo)?;

        // 2. Longest path ending at `parent` (via reverse edges).
        //    Since `parent` is not reachable from `child`, ancestors of `parent`
        //    and descendants of `child` are disjoint, so `up + 1 + down` is the
        //    exact maximum path length through `parent -> child`.
        let mut bwd_memo = BTreeMap::new();
        let up = backward_scan(&self.incoming, parent, &mut bwd_memo);

        if up + 1 + down > EP_MAX_NESTS {
            return Err(Errno::Loop);
        }

        self.outgoing.entry(parent).or_default().insert(child, 1);
        self.incoming.entry(child).or_default().insert(parent);
        Ok(())
    }

    /// Remove one registration of `child` from `parent`.
    ///
    /// Returns `true` if such an edge was present (decrementing its reference
    /// count and deleting the edge when it reaches 0), or `false` if none was
    /// recorded.
    pub fn remove(&mut self, parent: Id, child: Id) -> bool {
        let Some(children) = self.outgoing.get_mut(&parent) else {
            return false;
        };
        let Some(count) = children.get_mut(&child) else {
            return false;
        };
        if *count > 1 {
            *count -= 1;
        } else {
            children.remove(&child);
            if children.is_empty() {
                self.outgoing.remove(&parent);
            }
            self.detach_incoming(child, parent);
        }
        true
    }

    /// Remove every edge entering or leaving `id` (e.g., when epoll instance
    /// `id` is closed and torn down).
    pub fn remove_set(&mut self, id: Id) {
        if let Some(children) = self.outgoing.remove(&id) {
            for child in children.into_keys() {
                self.detach_incoming(child, id);
            }
        }
        if let Some(parents) = self.incoming.remove(&id) {
            for parent in parents {
                if let Some(children) = self.outgoing.get_mut(&parent) {
                    children.remove(&id);
                    if children.is_empty() {
                        self.outgoing.remove(&parent);
                    }
                }
            }
        }
    }

    fn detach_incoming(&mut self, child: Id, parent: Id) {
        if let Some(parents) = self.incoming.get_mut(&child) {
            parents.remove(&parent);
            if parents.is_empty() {
                self.incoming.remove(&child);
            }
        }
    }

    /// Structural invariant check for tests.
    #[cfg(test)]
    pub(crate) fn assert_invariants(&self) {
        assert_eq!(self.outgoing.is_empty(), self.incoming.is_empty());
        for (&p, children) in &self.outgoing {
            assert!(!children.is_empty(), "empty outgoing map");
            for (&c, &cnt) in children {
                assert!(cnt > 0);
                assert!(self.incoming.get(&c).is_some_and(|ps| ps.contains(&p)));
            }
        }
        for (&c, parents) in &self.incoming {
            assert!(!parents.is_empty(), "empty incoming map");
            for &p in parents {
                assert!(self.outgoing.get(&p).and_then(|m| m.get(&c)).is_some_and(|&cnt| cnt > 0));
            }
        }
    }
}

fn forward_scan<Id: Ord + Copy>(
    outgoing: &BTreeMap<Id, BTreeMap<Id, usize>>,
    node: Id,
    forbidden: Id,
    memo: &mut BTreeMap<Id, usize>,
) -> Result<usize, Errno> {
    if node == forbidden {
        return Err(Errno::Loop);
    }
    if let Some(&cached) = memo.get(&node) {
        return Ok(cached);
    }
    let mut best = 0usize;
    if let Some(children) = outgoing.get(&node) {
        for (&child, &count) in children {
            debug_assert!(count > 0);
            let d = forward_scan(outgoing, child, forbidden, memo)?;
            best = best.max(1 + d);
        }
    }
    memo.insert(node, best);
    Ok(best)
}

fn backward_scan<Id: Ord + Copy>(
    incoming: &BTreeMap<Id, BTreeSet<Id>>,
    node: Id,
    memo: &mut BTreeMap<Id, usize>,
) -> usize {
    if let Some(&cached) = memo.get(&node) {
        return cached;
    }
    let mut best = 0usize;
    if let Some(parents) = incoming.get(&node) {
        for &parent in parents {
            let d = backward_scan(incoming, parent, memo);
            best = best.max(1 + d);
        }
    }
    memo.insert(node, best);
    best
}
