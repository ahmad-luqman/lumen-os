//! The glyph cache: rasterized glyphs keyed by (glyph, pixel size), bounded
//! in bytes, least recently used evicted first.
//!
//! What counts against the bound is each glyph's coverage bytes plus
//! [`ENTRY_OVERHEAD`] (its record and its two index entries, rounded up), so
//! a cache of many small glyphs is bounded too. [`GlyphCache::used`] never
//! exceeds the bound: a glyph that alone costs more than the whole bound is
//! rasterized, used and dropped (counted in [`Stats::uncached`]) rather
//! than cached.

use crate::Px;
use crate::face::{Face, Glyph};
use alloc::collections::BTreeMap;
use core::mem::size_of;

/// Bytes counted for each cached glyph besides its coverage.
pub const ENTRY_OVERHEAD: usize = 64;

/// What the cache has done since it was made.
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    px: u16,
    glyph: u16,
}

#[derive(Debug)]
struct Entry {
    glyph: Glyph,
    /// When it was last used: its key in `GlyphCache::by_use`.
    used_at: u64,
}

/// Rasterized glyphs of one face. Lookups and inserts are O(log n), and so
/// is each eviction (an index by last use, oldest first).
#[derive(Debug)]
pub struct GlyphCache {
    bound: usize,
    used: usize,
    clock: u64,
    glyphs: BTreeMap<Key, Entry>,
    by_use: BTreeMap<u64, Key>,
    stats: Stats,
}

/// Bytes a glyph counts for while cached.
pub fn cost(glyph: &Glyph) -> usize {
    glyph.coverage().len() + ENTRY_OVERHEAD
}

// The overhead covers the entry's record (the index nodes' share is rounded
// in), so a change to the record that outgrows it shows up here.
const _: () = assert!(size_of::<Key>() + size_of::<Entry>() <= ENTRY_OVERHEAD);

impl GlyphCache {
    /// An empty cache that holds at most `bound` bytes.
    pub fn new(bound: usize) -> GlyphCache {
        GlyphCache {
            bound,
            used: 0,
            clock: 0,
            glyphs: BTreeMap::new(),
            by_use: BTreeMap::new(),
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Bytes held now, at most the bound.
    pub fn used(&self) -> usize {
        self.used
    }

    pub fn bound(&self) -> usize {
        self.bound
    }

    /// Glyphs held now.
    pub fn len(&self) -> usize {
        self.glyphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    /// Whether `glyph` at `px` is cached (doesn't count as a use).
    pub fn contains(&self, glyph: u16, px: Px) -> bool {
        self.glyphs.contains_key(&Key { px: px.get(), glyph })
    }

    /// Call `f` with `glyph` at `px` of `face`, rasterizing it only if it
    /// isn't cached. `face` must be the face this cache has always been
    /// used with ([`crate::Font`] keeps them together).
    pub fn with<R>(&mut self, face: &Face, glyph: u16, px: Px, f: impl FnOnce(&Glyph) -> R) -> R {
        let key = Key { px: px.get(), glyph };
        self.clock += 1;
        let now = self.clock;
        if let Some(entry) = self.glyphs.get_mut(&key) {
            self.stats.hits += 1;
            self.by_use.remove(&entry.used_at);
            entry.used_at = now;
            self.by_use.insert(now, key);
            return f(&entry.glyph);
        }
        self.stats.misses += 1;
        let rasterized = face.rasterize(glyph, px);
        let cost = cost(&rasterized);
        if cost > self.bound {
            self.stats.uncached += 1;
            return f(&rasterized);
        }
        while self.used + cost > self.bound && self.evict_oldest() {}
        self.used += cost;
        self.by_use.insert(now, key);
        let entry = self.glyphs.entry(key).or_insert(Entry { glyph: rasterized, used_at: now });
        f(&entry.glyph)
    }

    /// Drop the least recently used glyph; false if there is none (only
    /// when the cache is empty, so a glyph that fits the bound fits then).
    fn evict_oldest(&mut self) -> bool {
        let Some((_, key)) = self.by_use.pop_first() else { return false };
        if let Some(entry) = self.glyphs.remove(&key) {
            self.used -= cost(&entry.glyph);
            self.stats.evictions += 1;
        }
        true
    }
}
