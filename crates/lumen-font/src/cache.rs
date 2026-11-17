//! The glyph cache: rasterized glyphs keyed by (glyph, pixel size), bounded
//! in bytes, least recently used evicted first.
//!
//! What counts against the bound is each glyph's coverage bytes plus
//! [`ENTRY_OVERHEAD`], a fixed charge per glyph so that many glyphs without
//! ink are bounded too. It is a count, not an upper bound on the heap the
//! cache takes (the map's nodes are allocated as the map sees fit).
//! [`GlyphCache::used`] never exceeds the bound: a glyph that alone costs
//! more than the whole bound is rasterized, used and dropped (counted in
//! [`Stats::uncached`]) rather than cached.

use crate::Px;
use crate::face::{Face, Glyph};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Bytes counted for each cached glyph besides its coverage.
pub(crate) const ENTRY_OVERHEAD: usize = 64;

/// What a [`crate::Font`]'s cache has done since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Lookups answered from the cache.
    pub hits: u64,
    /// Lookups that rasterized (every rasterization is a miss).
    pub misses: u64,
    /// Glyphs dropped to make room.
    pub evictions: u64,
    /// Misses whose glyph was too big to cache at all.
    pub uncached: u64,
    /// Characters rendered that the face lacks (drawn as its missing-glyph
    /// box).
    pub missing: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    px: u16,
    glyph: u16,
}

#[derive(Debug)]
struct Entry {
    glyph: Glyph,
    /// When it was last used: the least recent is evicted first.
    used_at: u64,
}

/// Rasterized glyphs of one face. A lookup is O(log n). A miss that needs
/// room sorts the entries by last use once, O(n log n), and drops the
/// oldest until the glyph fits; n is at most `bound / ENTRY_OVERHEAD` (4096
/// for the kernel's 256 KiB).
#[derive(Debug)]
pub(crate) struct GlyphCache {
    bound: usize,
    used: usize,
    clock: u64,
    glyphs: BTreeMap<Key, Entry>,
    stats: Stats,
}

/// Bytes a glyph counts for while cached.
pub(crate) fn cost(glyph: &Glyph) -> usize {
    glyph.coverage.len() + ENTRY_OVERHEAD
}

impl GlyphCache {
    /// An empty cache that holds at most `bound` bytes.
    pub(crate) fn new(bound: usize) -> GlyphCache {
        GlyphCache { bound, used: 0, clock: 0, glyphs: BTreeMap::new(), stats: Stats::default() }
    }

    pub(crate) fn stats(&self) -> Stats {
        self.stats
    }

    pub(crate) fn count_missing(&mut self) {
        self.stats.missing += 1;
    }

    /// Bytes held now, at most the bound.
    pub(crate) fn used(&self) -> usize {
        self.used
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.glyphs.len()
    }

    /// Whether `glyph` at `px` is cached (doesn't count as a use).
    #[cfg(test)]
    pub(crate) fn contains(&self, glyph: u16, px: Px) -> bool {
        self.glyphs.contains_key(&Key { px: px.get(), glyph })
    }

    /// Call `f` with `glyph` at `px` of `face`, rasterizing it only if it
    /// isn't cached. `face` must be the face this cache has always been
    /// used with ([`crate::Font`] keeps them together).
    pub(crate) fn with<R>(&mut self, face: &Face, glyph: u16, px: Px, f: impl FnOnce(&Glyph) -> R) -> R {
        let key = Key { px: px.get(), glyph };
        self.clock += 1;
        let now = self.clock;
        if let Some(entry) = self.glyphs.get_mut(&key) {
            self.stats.hits += 1;
            entry.used_at = now;
            return f(&entry.glyph);
        }
        self.stats.misses += 1;
        let rasterized = face.rasterize(glyph, px);
        let cost = cost(&rasterized);
        if cost > self.bound {
            self.stats.uncached += 1;
            return f(&rasterized);
        }
        if self.used + cost > self.bound {
            self.make_room(cost);
        }
        self.used += cost;
        let entry = self.glyphs.entry(key).or_insert(Entry { glyph: rasterized, used_at: now });
        f(&entry.glyph)
    }

    /// Drop the least recently used glyphs until `needed` more bytes fit:
    /// one sort by last use, then the oldest first. `needed` is at most the
    /// bound, so emptying the cache always makes room.
    fn make_room(&mut self, needed: usize) {
        let mut by_use: Vec<(u64, Key)> = self.glyphs.iter().map(|(k, e)| (e.used_at, *k)).collect();
        by_use.sort_unstable();
        for (_, key) in by_use {
            if self.used + needed <= self.bound {
                break;
            }
            if let Some(entry) = self.glyphs.remove(&key) {
                self.used -= cost(&entry.glyph);
                self.stats.evictions += 1;
            }
        }
    }
}
