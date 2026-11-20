use super::cache::{ENTRY_OVERHEAD, GlyphCache, cost};
use super::face::{Pen, RASTERIZED, ceil, to_64ths};
use super::*;
use std::collections::VecDeque;
use std::format;
use std::vec::Vec;

const MIB: usize = 1 << 20;

fn px(n: u16) -> Px {
    Px::new(n).unwrap()
}

fn face() -> Face {
    Face::ui().expect("the embedded font parses")
}

/// Glyphs this thread has rasterized so far.
fn rasterized() -> u64 {
    RASTERIZED.with(|n| n.get())
}

type Drawn = (i32, i32, u32, u32, Vec<u8>);

/// Every glyph `render` hands out, owned.
fn placed(font: &mut Font<'_>, text: &str, size: Px) -> Vec<Drawn> {
    let mut out = Vec::new();
    font.render(text, size, |g| out.push((g.x(), g.y(), g.width(), g.height(), g.coverage().to_vec())));
    out
}

#[test]
fn px_takes_one_to_max() {
    assert_eq!(Px::new(0), None);
    assert_eq!(Px::new(1).map(Px::get), Some(1));
    assert_eq!(Px::new(MAX_PX).map(Px::get), Some(MAX_PX));
    assert_eq!(Px::new(MAX_PX + 1), None);
}

#[test]
fn the_embedded_font_is_atkinson_hyperlegible_unchanged() {
    let face = face();
    assert_eq!(face.name(), Some("Atkinson Hyperlegible Regular"));
    assert_eq!(face.glyph_count(), 369);
    assert_eq!(UI_FONT.len(), 54_348);
    // The file as committed (assets/fonts/README.md records this and its
    // sha256): one flipped byte fails here.
    assert_eq!(fnv1a(UI_FONT.iter().copied()), UI_FONT_FNV1A, "{:#018x}", fnv1a(UI_FONT.iter().copied()));
}

#[test]
fn garbage_isnt_a_font() {
    assert!(Face::parse(b"not a font at all").is_err());
    assert!(Face::parse(&UI_FONT[..100]).is_err());
    let e = Face::parse(&[]).unwrap_err();
    assert!(format!("{e}").starts_with("can't parse the font: "), "{e}");
}

#[test]
fn known_glyphs_metrics_and_advances() {
    // Pinned as fontdue 0.9.5 measures Atkinson Hyperlegible 1.006, and
    // checked to scale with the size.
    let face = face();
    let a = face.glyph_index('A');
    assert_eq!(face.advance(a, px(12)), 481);
    assert_eq!(face.advance(a, px(24)), 962);
    assert_eq!(face.advance(a, px(48)), 1923);
    for (c, advance24) in [('x', 708), ('M', 1260), ('i', 427), (' ', 430), ('g', 860)] {
        let g = face.glyph_index(c);
        assert_eq!(face.advance(g, px(24)), advance24, "{c:?}");
        // Twice the size, twice the advance, to within rounding.
        assert!((face.advance(g, px(48)) - 2 * advance24).abs() <= 1, "{c:?}");
    }
    assert_eq!(face.line_metrics(px(24)), LineMetrics { ascent: 23, descent: 7, line_gap: 0 });

    // 'x' sits on the baseline with x-height 12; 'g' hangs below it.
    let x = face.rasterize(face.glyph_index('x'), px(24));
    assert_eq!((x.ink.width, x.ink.height, x.ink.left, x.ink.top), (12, 12, 0, 12));
    assert_eq!(x.coverage.len(), 12 * 12);
    let g = face.rasterize(face.glyph_index('g'), px(24));
    assert_eq!((g.ink.width, g.ink.height, g.ink.top), (12, 18, 13));
    assert!(g.ink.height as i32 > g.ink.top, "g descends below the baseline");
    // A space advances but has no ink.
    let space = face.rasterize(face.glyph_index(' '), px(24));
    assert_eq!((space.ink.width, space.ink.height, space.coverage.len()), (0, 0, 0));
}

#[test]
fn a_glyph_number_past_the_end_is_the_missing_glyph() {
    // fontdue indexes its glyph table unchecked: past the end it would
    // panic. Every path that takes a glyph number maps it to glyph 0.
    let face = face();
    for bad in [369, 370, u16::MAX] {
        assert_eq!(face.advance(bad, px(24)), face.advance(0, px(24)));
        assert_eq!(face.ink_box(bad, px(24)), face.ink_box(0, px(24)));
        assert_eq!(face.rasterize(bad, px(24)), face.rasterize(0, px(24)));
        let mut cache = GlyphCache::new(MIB);
        assert_eq!(cache.with(&face, bad, px(24), |g| g.clone()), face.rasterize(0, px(24)));
    }
}

#[test]
fn the_ink_box_from_the_metrics_is_the_rasterized_box() {
    // `render_mask` sizes its mask from the metrics before drawing, so the
    // metrics' box must be exactly the mask's: every glyph, at small,
    // desktop and the largest sizes.
    let face = face();
    for size in [1, 7, 12, 16, 24, 32, 48, MAX_PX] {
        for glyph in 0..face.glyph_count() {
            let ink = face.ink_box(glyph, px(size));
            let g = face.rasterize(glyph, px(size));
            assert_eq!(ink, g.ink, "glyph {glyph} at {size}");
            assert_eq!(g.coverage.len(), g.ink.area(), "glyph {glyph} at {size}");
        }
    }
}

#[test]
fn a_missing_character_draws_the_missing_glyph_box_and_counts() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    assert_eq!(face.glyph_index('\u{263A}'), 0);
    let drawn = placed(&mut font, "\u{263A}", px(24));
    assert_eq!(drawn.len(), 1);
    let ink = drawn[0].4.iter().copied().max().unwrap_or(0);
    assert!(ink > 128, "the box has ink: {ink}");
    assert_eq!(font.stats().missing, 1);
    placed(&mut font, "a\u{263A}b\u{1F600}", px(24));
    assert_eq!(font.stats().missing, 3);
    placed(&mut font, "abc", px(24));
    assert_eq!(font.stats().missing, 3);
}

#[test]
fn glyphs_are_placed_on_the_baseline_at_their_pen_positions() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let ascent = face.line_metrics(px(24)).ascent;
    let drawn = placed(&mut font, "xx x", px(24));
    // Three inked glyphs: the space draws nothing.
    assert_eq!(drawn.len(), 3);
    // 'x' advances 708/64 = 11.06 px, the space 430/64 = 6.72: pens at 0,
    // 708 and 708 + 708 + 430 = 1846 64ths, nearest pixels 0, 11, 29.
    let xs: Vec<i32> = drawn.iter().map(|d| d.0).collect();
    assert_eq!(xs, [0, 11, 29]);
    for (_, y, _, h, _) in &drawn {
        assert_eq!(y + *h as i32, ascent, "x's bottom row sits on the baseline");
    }
    // A glyph's mask starts `left` pixels from its pen: 'j' reaches left
    // of its origin.
    let j = face.rasterize(face.glyph_index('j'), px(24));
    assert!(j.ink.left < 0, "j's tail reaches left of the pen: {}", j.ink.left);
    assert_eq!(placed(&mut font, "j", px(24))[0].0, j.ink.left);
    let after_x = placed(&mut font, "xj", px(24));
    assert_eq!(after_x[1].0, 11 + j.ink.left);
}

#[test]
fn the_pen_rounds_to_the_nearest_pixel_and_kerns() {
    let mut pen = Pen::default();
    assert_eq!(pen.kern(0), 0);
    pen.advance(31); // 0.48 px
    assert_eq!(pen.kern(0), 0);
    pen.advance(1); // exactly half: rounds up
    assert_eq!(pen.kern(0), 1);
    pen.advance(64 * 10);
    assert_eq!(pen.kern(-64), 10); // 10.5 - 1 = 9.5 rounds up to 10
    assert_eq!(pen.width(), 10); // 9.5 px, rounded up
    // Kerning back past the start: a negative pixel, but no negative width.
    let mut pen = Pen::default();
    assert_eq!(pen.kern(-100), -2); // -1.56 px is nearest -2
    assert_eq!(pen.kern(10), -1); // -1.40 px
    assert_eq!(pen.width(), 0);
    let mut pen = Pen::default();
    assert_eq!(pen.kern(-200), -3); // -3.13 px
    assert_eq!(pen.width(), 0, "a pen left of the start is no width, not a huge one");
    // The embedded face has no `kern` table, so every pair kerns by 0.
    let face = face();
    let (a, v) = (face.glyph_index('A'), face.glyph_index('V'));
    assert_eq!(face.kern(a, v, px(24)), 0);
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> usize {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// `font` with a `kern` table (version 0, one horizontal format-0
/// subtable) holding the one pair `left`, `right` = `value` font units.
/// The table directory is rebuilt; checksums are left 0 (nothing checks
/// them). A test fixture only: the font's licence declares no Reserved
/// Font Name, and the result never leaves the test.
fn with_kern_pair(font: &[u8], left: u16, right: u16, value: i16) -> Vec<u8> {
    let mut kern = Vec::new();
    // Table: version 0, 1 subtable. Subtable: version 0, length 20, format
    // 0 and coverage 1 (horizontal), 1 pair, searchRange 6, entrySelector
    // 0, rangeShift 0, then the pair.
    for v in [0_u16, 1, 0, 20, 0x0001, 1, 6, 0, 0, left, right, value as u16] {
        kern.extend(v.to_be_bytes());
    }
    let mut tables: Vec<([u8; 4], &[u8])> = (0..usize::from(be16(font, 4)))
        .map(|i| {
            let r = 12 + 16 * i;
            let (offset, len) = (be32(font, r + 8), be32(font, r + 12));
            (font[r..r + 4].try_into().unwrap(), &font[offset..offset + len])
        })
        .collect();
    tables.push((*b"kern", &kern));
    tables.sort_by_key(|(tag, _)| *tag);
    let n = tables.len() as u16;
    let search = 16 * (1_u16 << (15 - n.leading_zeros()));
    let mut out = font[..4].to_vec();
    for v in [n, search, search.trailing_zeros() as u16 - 4, n * 16 - search] {
        out.extend(v.to_be_bytes());
    }
    let mut data = Vec::new();
    let start = 12 + 16 * tables.len();
    for (tag, bytes) in &tables {
        out.extend(tag);
        out.extend(0_u32.to_be_bytes());
        out.extend(((start + data.len()) as u32).to_be_bytes());
        out.extend((bytes.len() as u32).to_be_bytes());
        data.extend(*bytes);
        data.resize(data.len().next_multiple_of(4), 0);
    }
    out.extend(data);
    out
}

#[test]
fn a_kern_pair_moves_the_right_glyph_and_the_measure() {
    let plain = face();
    let (a, v, space) = (plain.glyph_index('A'), plain.glyph_index('V'), plain.glyph_index(' '));
    let kerned_bytes = with_kern_pair(UI_FONT, a, v, -150);
    let kerned = Face::parse(&kerned_bytes).expect("the fixture parses");
    let size = px(24);
    // -150 units at 24 px, 1000 units per em: -3.6 px.
    let kern = -230;
    assert_eq!(kerned.kern(a, v, size), kern);
    // Only that pair, in that order.
    assert_eq!(kerned.kern(v, a, size), 0);
    assert_eq!(kerned.kern(a, a, size), 0);

    let (adv_a, adv_v, adv_space) = (plain.advance(a, size), plain.advance(v, size), plain.advance(space, size));
    let v_left = plain.rasterize(v, size).ink.left;
    let x_of = |face: &Face, text: &str, nth: usize| {
        let mut font = Font::new(face, MIB);
        placed(&mut font, text, size)[nth].0
    };
    // "AV": V moves left by the kerning, on the pixel nearest its pen.
    assert_eq!(x_of(&kerned, "AV", 1), (adv_a + kern + 32).div_euclid(64) + v_left);
    assert_eq!(x_of(&plain, "AV", 1), (adv_a + 32).div_euclid(64) + v_left);
    assert!(x_of(&kerned, "AV", 1) < x_of(&plain, "AV", 1));
    // Adjacent glyphs only: a space between them kerns nothing.
    assert_eq!(x_of(&kerned, "A V", 1), x_of(&plain, "A V", 1));
    assert_eq!(x_of(&kerned, "A V", 1), (adv_a + adv_space + 32).div_euclid(64) + v_left);
    // And again after an unkerned pair: the pen remembers the last glyph.
    assert_eq!(x_of(&kerned, "VAV", 2), (adv_v + adv_a + kern + 32).div_euclid(64) + v_left);
    // The measure includes it: it ends where the pen does.
    let m = kerned.measure("AV", size);
    assert_eq!(m.width, ((adv_a + kern + adv_v + 63) / 64) as u32);
    assert!(m.width < plain.measure("AV", size).width);
}

#[test]
fn rounding_helpers() {
    assert_eq!(to_64ths(0.0), 0);
    assert_eq!(to_64ths(1.0), 64);
    assert_eq!(to_64ths(0.5 / 64.0), 1); // a half rounds away from zero
    assert_eq!(to_64ths(-0.5 / 64.0), -1);
    assert_eq!(to_64ths(0.49 / 64.0), 0);
    assert_eq!(to_64ths(-2.25), -144);
    assert_eq!(to_64ths(f32::NAN), 0);
    assert_eq!(ceil(0.0), 0);
    assert_eq!(ceil(22.01), 23);
    assert_eq!(ceil(23.0), 23);
    assert_eq!(ceil(-6.5), -6);
    assert_eq!(ceil(-7.0), -7);
}

#[test]
fn glyphs_a_caller_refuses_are_never_rasterized() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let before = rasterized();
    let mut drawn = 0;
    font.render_where(sample::TEXT, px(24), |_, _, _, _| false, |_| drawn += 1);
    // Only the glyphs without ink (the space) still go through the cache:
    // their box is empty, so there is nothing to refuse.
    let mut inkless: Vec<u16> =
        sample::TEXT.chars().map(|c| face.glyph_index(c)).filter(|&g| face.ink_box(g, px(24)).width == 0).collect();
    inkless.sort_unstable();
    inkless.dedup();
    assert!(!inkless.is_empty());
    assert_eq!(drawn, 0);
    assert_eq!(rasterized() - before, inkless.len() as u64);
    assert_eq!(font.stats().misses, inkless.len() as u64);
    // Only the glyphs whose box reaches x < 12: the first letter or two.
    let all = placed(&mut Font::new(&face, MIB), sample::TEXT, px(24));
    let mut font = Font::new(&face, MIB);
    let before = rasterized();
    let mut kept = Vec::new();
    font.render_where(sample::TEXT, px(24), |x, _, _, _| x < 12, |g| kept.push((g.x(), g.y())));
    let want: Vec<(i32, i32)> = all.iter().filter(|g| g.0 < 12).map(|g| (g.0, g.1)).collect();
    assert!(!want.is_empty() && want.len() < all.len());
    assert_eq!(kept, want);
    assert_eq!(rasterized() - before, font.stats().misses);
    assert!(font.stats().misses < all.len() as u64);
    // Keeping everything is `render`.
    let mut font = Font::new(&face, MIB);
    let mut every = Vec::new();
    font.render_where(sample::TEXT, px(24), |_, _, _, _| true, |g| every.push((g.x(), g.y())));
    assert_eq!(every, all.iter().map(|g| (g.0, g.1)).collect::<Vec<_>>());
}

#[test]
fn a_second_render_rasterizes_nothing() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let before = rasterized();
    let first = placed(&mut font, sample::TEXT, px(24));
    let after_first = font.stats();
    let mut distinct: Vec<u16> = sample::TEXT.chars().map(|c| face.glyph_index(c)).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let chars = sample::TEXT.chars().count() as u64;
    assert_eq!(after_first.misses, distinct.len() as u64);
    assert_eq!(rasterized() - before, after_first.misses, "a rasterization per miss");
    assert_eq!(after_first.hits, chars - distinct.len() as u64);
    let middle = rasterized();
    let second = placed(&mut font, sample::TEXT, px(24));
    let after_second = font.stats();
    assert_eq!(second, first, "the same glyphs in the same places");
    assert_eq!(rasterized(), middle, "nothing rasterized");
    assert_eq!(after_second.misses, after_first.misses);
    assert_eq!(after_second.hits, after_first.hits + chars);
    assert_eq!((after_second.evictions, after_second.uncached, after_second.missing), (0, 0, 0));
    // Another size is another glyph.
    placed(&mut font, "A", px(25));
    assert_eq!(font.stats().misses, after_second.misses + 1);
    assert_eq!(rasterized(), middle + 1);
}

#[test]
fn the_cache_counts_coverage_plus_overhead() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let x = face.rasterize(face.glyph_index('x'), px(24));
    placed(&mut font, "x", px(24));
    assert_eq!(font.cached_bytes(), 12 * 12 + ENTRY_OVERHEAD);
    assert_eq!(cost(&x), 12 * 12 + ENTRY_OVERHEAD);
    placed(&mut font, " ", px(24));
    assert_eq!(font.cached_bytes(), 12 * 12 + 2 * ENTRY_OVERHEAD);
}

#[test]
fn the_least_recently_used_glyph_is_evicted_first() {
    let face = face();
    let index = |c: char| face.glyph_index(c);
    let cost_of = |c: char| cost(&face.rasterize(index(c), px(24)));
    // Room for 'a', 'b' and 'c' exactly.
    let bound = cost_of('a') + cost_of('b') + cost_of('c');
    let mut cache = GlyphCache::new(bound);
    for c in ['a', 'b', 'c'] {
        cache.with(&face, index(c), px(24), |_| ());
    }
    assert_eq!(cache.used(), bound);
    assert_eq!(cache.stats().evictions, 0);
    // Use 'a' again, so 'b' is now the oldest.
    cache.with(&face, index('a'), px(24), |_| ());
    // 'd' needs room: 'b' goes (and 'c' too if 'b' alone isn't enough).
    cache.with(&face, index('d'), px(24), |_| ());
    assert!(!cache.contains(index('b'), px(24)));
    assert!(cache.contains(index('a'), px(24)), "recently used, kept");
    assert!(cache.contains(index('d'), px(24)));
    assert!(cache.used() <= bound);
    assert!(cache.stats().evictions >= 1);
    // An evicted glyph comes back by rasterizing it again (a miss), with
    // the same mask.
    let misses = cache.stats().misses;
    let again = cache.with(&face, index('b'), px(24), |g| g.clone());
    assert_eq!(cache.stats().misses, misses + 1);
    assert_eq!(again, face.rasterize(index('b'), px(24)));
}

#[test]
fn a_glyph_bigger_than_the_whole_cache_is_used_and_dropped() {
    let face = face();
    let m = face.glyph_index('M');
    let mut cache = GlyphCache::new(ENTRY_OVERHEAD); // room for no ink at all
    let width = cache.with(&face, m, px(24), |g| g.ink.width);
    assert_eq!(width, 17, "still handed out");
    assert_eq!((cache.used(), cache.len()), (0, 0));
    assert_eq!(cache.stats().uncached, 1);
    // A space (no ink) fits exactly.
    cache.with(&face, face.glyph_index(' '), px(24), |_| ());
    assert_eq!((cache.used(), cache.len()), (ENTRY_OVERHEAD, 1));
    // A zero-byte cache caches nothing and still draws.
    let mut font = Font::new(&face, 0);
    let drawn = placed(&mut font, "Mx", px(24));
    assert_eq!(drawn.len(), 2);
    assert_eq!(font.cached_bytes(), 0);
    assert_eq!(font.stats().uncached, 2);
}

/// A small xorshift, so the randomized test is reproducible.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn the_cache_agrees_with_a_model_lru_and_never_exceeds_its_bound() {
    let face = face();
    let chars: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
    let sizes = [px(9), px(16), px(24)];
    // Each (glyph, size)'s mask and cost, worked out up front.
    let mut expected = std::collections::BTreeMap::new();
    for &c in &chars {
        for &s in &sizes {
            let g = face.rasterize(face.glyph_index(c), s);
            expected.insert((face.glyph_index(c), s), (cost(&g), g));
        }
    }
    let cost_of = |k: &(u16, Px)| expected[k].0;
    for (seed, bound) in [(1_u64, 600_usize), (2, 2_000), (3, 8_000), (4, 50)] {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let mut cache = GlyphCache::new(bound);
        // The model: keys oldest first.
        let mut model: VecDeque<(u16, Px)> = VecDeque::new();
        let (mut hits, mut misses, mut evictions, mut uncached) = (0, 0, 0, 0);
        for step in 0..3_000 {
            let c = chars[rng.below(chars.len() as u64) as usize];
            let s = sizes[rng.below(sizes.len() as u64) as usize];
            let key = (face.glyph_index(c), s);
            let found = model.iter().position(|k| *k == key);
            let was_miss = found.is_none();
            if let Some(at) = found {
                hits += 1;
                model.remove(at);
                model.push_back(key);
            } else {
                misses += 1;
                if cost_of(&key) > bound {
                    uncached += 1;
                } else {
                    while model.iter().map(cost_of).sum::<usize>() + cost_of(&key) > bound {
                        model.pop_front();
                        evictions += 1;
                    }
                    model.push_back(key);
                }
            }
            let before = rasterized();
            let got = cache.with(&face, key.0, s, |g| g.clone());
            let stats = cache.stats();
            let at = format!("seed {seed} step {step}");
            assert_eq!(got, expected[&key].1, "{at}: the glyph handed out is the glyph");
            assert_eq!(
                (stats.hits, stats.misses, stats.evictions, stats.uncached),
                (hits, misses, evictions, uncached),
                "{at}"
            );
            assert_eq!(rasterized() - before, u64::from(was_miss), "{at}: rasterized only on a miss");
            assert!(cache.used() <= bound, "{at}: {} > {bound}", cache.used());
            assert_eq!(cache.used(), model.iter().map(cost_of).sum::<usize>(), "{at}");
            assert_eq!(cache.len(), model.len(), "{at}");
        }
        for k in &model {
            assert!(cache.contains(k.0, k.1));
        }
        assert!(bound == 50 || hits > 0, "seed {seed}: the test exercised hits");
    }
}

#[test]
fn measure_matches_render_and_needs_no_cache() {
    let face = face();
    assert_eq!(face.measure("", px(24)), Measure { width: 0, ascent: 23, descent: 7 });
    let before = rasterized();
    let m = face.measure(sample::TEXT, px(24));
    assert_eq!(rasterized(), before, "measuring rasterizes nothing");
    // The advances add up: 'x' is 708 64ths, so "xxx" is 2124 = 33.19 px,
    // rounded up to 34.
    assert_eq!(face.measure("xxx", px(24)).width, 34);
    assert_eq!(face.measure(" ", px(24)).width, 7);
    // A line's width is where its last glyph's pen ends: render the text
    // plus one more 'x' and that 'x' lands at the pixel nearest that pen.
    let mut font = Font::new(&face, MIB);
    let text = format!("{}x", sample::TEXT);
    let drawn = placed(&mut font, &text, px(24));
    let last = drawn.last().unwrap();
    let pen_end = sample::TEXT.chars().map(|c| face.advance(face.glyph_index(c), px(24))).sum::<i32>();
    assert_eq!(last.0, (pen_end + 32) / 64);
    assert_eq!(m.width, ((pen_end + 63) / 64) as u32);
    assert_eq!((m.ascent, m.descent), (23, 7));
}

#[test]
fn render_mask_is_the_highest_coverage_of_any_glyph_at_each_pixel() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    // "fj": the f's hook and the j's tail come close; "jT" starts left of
    // the pen (a negative x); "TTQx": the T's bars meet and overlap.
    for text in ["fj", sample::TEXT, "\u{263A}Wg", "", "jT", "TTQx"] {
        let drawn = placed(&mut font, text, px(24));
        let mask = font.render_mask(text, px(24), 4096).unwrap();
        assert_eq!(mask.coverage().len(), (mask.width() * mask.height()) as usize);
        let measure = face.measure(text, px(24));
        assert!(mask.x() <= 0 && mask.y() <= 0);
        assert!(mask.x() + mask.width() as i32 >= measure.width as i32);
        assert!(mask.y() + mask.height() as i32 >= measure.ascent + measure.descent);
        for row in 0..mask.height() as i32 {
            for col in 0..mask.width() as i32 {
                let (x, y) = (mask.x() + col, mask.y() + row);
                let expected = drawn
                    .iter()
                    .filter(|(gx, gy, gw, gh, _)| x >= *gx && x < gx + *gw as i32 && y >= *gy && y < gy + *gh as i32)
                    .map(|(gx, gy, gw, _, c)| c[((y - gy) * *gw as i32 + (x - gx)) as usize])
                    .max()
                    .unwrap_or(0);
                assert_eq!(
                    mask.coverage()[(row * mask.width() as i32 + col) as usize],
                    expected,
                    "{text:?} at {x},{y}"
                );
            }
        }
        // Every glyph lies inside the mask, and the mask is no bigger than
        // the line box and the glyphs need.
        let left = drawn.iter().map(|d| d.0).min().unwrap_or(0).min(0);
        assert_eq!(mask.x(), left, "{text:?}");
        for (gx, gy, gw, gh, _) in &drawn {
            assert!(*gx >= mask.x() && *gy >= mask.y());
            assert!(gx + *gw as i32 <= mask.x() + mask.width() as i32);
            assert!(gy + *gh as i32 <= mask.y() + mask.height() as i32);
        }
    }
    let overlap = placed(&mut font, "TT", px(24));
    assert!(overlap[0].0 + overlap[0].2 as i32 > overlap[1].0, "the T's bars overlap: {overlap:?}");
    assert!(placed(&mut font, "jT", px(24))[0].0 < 0, "j starts left of the pen");
}

#[test]
fn render_mask_refuses_a_line_wider_than_its_limit() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let width = font.render_mask("xxx", px(24), 4096).unwrap().width();
    assert_eq!(width, 34);
    assert!(font.render_mask("xxx", px(24), 34).is_ok());
    assert_eq!(font.render_mask("xxx", px(24), 33), Err(TooWide { width: 34 }));
    // The ink left of the line's start counts: "jT"'s mask starts left of
    // the pen, and its whole width is what's checked.
    let jt = font.render_mask("jT", px(24), 4096).unwrap();
    assert!(jt.x() < 0);
    assert!(font.render_mask("jT", px(24), jt.width()).is_ok());
    assert_eq!(font.render_mask("jT", px(24), jt.width() - 1), Err(TooWide { width: u64::from(jt.width()) }));
    let stats = font.stats();
    // Refused from the metrics: nothing rasterized, no cache touched.
    let before = rasterized();
    let long: std::string::String = "x".repeat(100_000);
    assert!(matches!(font.render_mask(&long, px(MAX_PX), 1 << 20), Err(TooWide { width }) if width > 1 << 20));
    assert_eq!(rasterized(), before);
    assert_eq!(font.stats(), stats);
}

#[test]
fn fnv1a_covers_the_header_and_the_coverage() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let mask = font.render_mask("x", px(24), 4096).unwrap();
    let moved = LineMask { x: -1, ..mask.clone() };
    let mut coverage = mask.coverage().to_vec();
    coverage[0] ^= 1;
    let changed = LineMask { coverage, ..mask.clone() };
    assert_ne!(mask.fnv1a(), moved.fnv1a());
    assert_ne!(mask.fnv1a(), changed.fnv1a());
    // FNV-1a 64's published vectors.
    assert_eq!(fnv1a([]), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(*b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(*b"foobar"), 0x8594_4171_f739_67e8);
}

/// The golden: `tests/golden/sample.pgm` (a binary PGM, viewable as an
/// image), re-recorded with `LUMEN_FONT_BLESS=1 cargo test -p lumen-font`
/// (look at it before committing).
#[test]
fn the_sample_renders_as_the_golden() {
    let face = face();
    let mut font = Font::new(&face, MIB);
    let mask = font.render_mask(sample::TEXT, px(sample::PX), 4096).unwrap();
    let mut pgm = format!(
        "P5\n# lumen-font sample {:?} at {} px, mask at ({}, {})\n{} {}\n255\n",
        sample::TEXT,
        sample::PX,
        mask.x(),
        mask.y(),
        mask.width(),
        mask.height()
    )
    .into_bytes();
    pgm.extend_from_slice(mask.coverage());
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/sample.pgm");
    if std::env::var_os("LUMEN_FONT_BLESS").is_some() {
        std::fs::write(path, &pgm).unwrap();
    }
    let golden = std::fs::read(path).expect("tests/golden/sample.pgm (LUMEN_FONT_BLESS=1 records it)");
    assert!(golden == pgm, "the sample's mask differs from tests/golden/sample.pgm");
    assert_eq!(mask.fnv1a(), sample::HASH, "sample::HASH is {:#018x}", mask.fnv1a());
    // Not blank: plenty of full and partial coverage (anti-aliased).
    assert!(mask.coverage().iter().filter(|&&c| c == 255).count() > 300);
    assert!(mask.coverage().iter().filter(|&&c| c > 0 && c < 255).count() > 300);
}
