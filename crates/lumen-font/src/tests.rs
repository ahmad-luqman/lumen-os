use super::cache::{ENTRY_OVERHEAD, GlyphCache, cost};
use super::face::{ceil, to_64ths};
use super::*;
use std::collections::VecDeque;
use std::format;
use std::vec::Vec;

const MIB: usize = 1 << 20;

fn px(n: u16) -> Px {
    Px::new(n).unwrap()
}

fn ui() -> Font {
    Font::ui(MIB).expect("the embedded font parses")
}

/// Every glyph `render` hands out, owned.
fn placed(font: &mut Font, text: &str, size: Px) -> Vec<(i32, i32, u16, u16, Vec<u8>)> {
    let mut out = Vec::new();
    font.render(text, size, |g| out.push((g.x, g.y, g.width, g.height, g.coverage.to_vec())));
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
fn the_embedded_font_is_atkinson_hyperlegible() {
    let font = ui();
    assert_eq!(font.face().name(), Some("Atkinson Hyperlegible Regular"));
    assert_eq!(font.face().glyph_count(), 369);
    assert_eq!(UI_FONT.len(), 54_348);
}

#[test]
fn garbage_isnt_a_font() {
    assert!(Font::parse(b"not a font at all", MIB).is_err());
    assert!(Font::parse(&UI_FONT[..100], MIB).is_err());
    let e = Font::parse(&[], MIB).unwrap_err();
    assert!(format!("{e}").starts_with("can't parse the font: "), "{e}");
}

#[test]
fn known_glyphs_metrics_and_advances() {
    // Pinned as fontdue 0.9.5 measures Atkinson Hyperlegible 1.006, and
    // checked to scale with the size.
    let font = ui();
    let face = font.face();
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
    assert_eq!((x.width, x.height, x.left, x.top, x.advance), (12, 12, 0, 12, 708));
    assert_eq!(x.coverage().len(), 12 * 12);
    let g = face.rasterize(face.glyph_index('g'), px(24));
    assert_eq!((g.width, g.height, g.top), (12, 18, 13));
    assert!(i32::from(g.height) > i32::from(g.top), "g descends below the baseline");
    // A space advances but has no ink.
    let space = face.rasterize(face.glyph_index(' '), px(24));
    assert_eq!((space.width, space.height, space.coverage().len()), (0, 0, 0));
}

#[test]
fn a_missing_character_draws_the_missing_glyph_box() {
    let mut font = ui();
    let face = font.face();
    assert_eq!(face.glyph_index('\u{263A}'), 0);
    let drawn = placed(&mut font, "\u{263A}", px(24));
    assert_eq!(drawn.len(), 1);
    let ink = drawn[0].4.iter().copied().max().unwrap_or(0);
    assert!(ink > 128, "the box has ink: {ink}");
}

#[test]
fn glyphs_are_placed_on_the_baseline_at_their_pen_positions() {
    let mut font = ui();
    let ascent = font.line_metrics(px(24)).ascent;
    let drawn = placed(&mut font, "xx x", px(24));
    // Three inked glyphs: the space draws nothing.
    assert_eq!(drawn.len(), 3);
    // 'x' advances 708/64 = 11.06 px, the space 430/64 = 6.72: pens at 0,
    // 708 and 708 + 708 + 430 = 1846 64ths, nearest pixels 0, 11, 29.
    let xs: Vec<i32> = drawn.iter().map(|d| d.0).collect();
    assert_eq!(xs, [0, 11, 29]);
    for (_, y, _, h, _) in &drawn {
        assert_eq!(y + i32::from(*h), ascent, "x's bottom row sits on the baseline");
    }
}

#[test]
fn the_pen_rounds_to_the_nearest_pixel_and_kerns() {
    // Atkinson Hyperlegible kerns only in GPOS, which fontdue doesn't read
    // (its `kern` table is what it reads), so kerning is checked on the pen.
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
    // The face has no `kern` table, so every pair kerns by 0.
    let font = ui();
    let face = font.face();
    let (a, v) = (face.glyph_index('A'), face.glyph_index('V'));
    assert_eq!(face.kern(a, v, px(24)), 0);
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
fn a_second_render_rasterizes_nothing() {
    let mut font = ui();
    let first = placed(&mut font, sample::TEXT, px(24));
    let after_first = font.stats();
    let distinct = {
        let mut glyphs: Vec<u16> = sample::TEXT.chars().map(|c| font.face().glyph_index(c)).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        glyphs.len() as u64
    };
    assert_eq!(after_first.misses, distinct);
    assert_eq!(after_first.hits, sample::TEXT.chars().count() as u64 - distinct);
    let second = placed(&mut font, sample::TEXT, px(24));
    let after_second = font.stats();
    assert_eq!(second, first, "the same glyphs in the same places");
    assert_eq!(after_second.misses, after_first.misses, "nothing rasterized");
    assert_eq!(after_second.hits, after_first.hits + sample::TEXT.chars().count() as u64);
    assert_eq!((after_second.evictions, after_second.uncached), (0, 0));
    // Another size is another glyph.
    placed(&mut font, "A", px(25));
    assert_eq!(font.stats().misses, after_second.misses + 1);
}

#[test]
fn the_cache_counts_coverage_plus_overhead() {
    let mut font = ui();
    let face_glyph = font.face().rasterize(font.face().glyph_index('x'), px(24));
    placed(&mut font, "x", px(24));
    assert_eq!(font.cached_bytes(), 12 * 12 + ENTRY_OVERHEAD);
    assert_eq!(cost(&face_glyph), 12 * 12 + ENTRY_OVERHEAD);
    placed(&mut font, " ", px(24));
    assert_eq!(font.cached_bytes(), 12 * 12 + 2 * ENTRY_OVERHEAD);
}

/// The costs of 'a'..='z' at 24 px, in order.
fn letter_costs(face: &Face) -> Vec<usize> {
    ('a'..='z').map(|c| cost(&face.rasterize(face.glyph_index(c), px(24)))).collect()
}

#[test]
fn the_least_recently_used_glyph_is_evicted_first() {
    let font = ui();
    let face = font.face();
    let costs = letter_costs(face);
    let index = |c: char| face.glyph_index(c);
    // Room for 'a', 'b' and 'c' exactly.
    let mut cache = GlyphCache::new(costs[0] + costs[1] + costs[2]);
    for c in ['a', 'b', 'c'] {
        cache.with(face, index(c), px(24), |_| ());
    }
    assert_eq!(cache.used(), cache.bound());
    assert_eq!(cache.stats().evictions, 0);
    // Use 'a' again, so 'b' is now the oldest.
    cache.with(face, index('a'), px(24), |_| ());
    // 'd' needs room: 'b' goes (and 'c' too if 'b' alone isn't enough).
    cache.with(face, index('d'), px(24), |_| ());
    assert!(!cache.contains(index('b'), px(24)));
    assert!(cache.contains(index('a'), px(24)), "recently used, kept");
    assert!(cache.contains(index('d'), px(24)));
    assert!(cache.used() <= cache.bound());
    assert!(cache.stats().evictions >= 1);
    // An evicted glyph comes back by rasterizing it again (a miss), with
    // the same mask.
    let misses = cache.stats().misses;
    let again = cache.with(face, index('b'), px(24), |g| g.clone());
    assert_eq!(cache.stats().misses, misses + 1);
    assert_eq!(again, face.rasterize(index('b'), px(24)));
}

#[test]
fn a_glyph_bigger_than_the_whole_cache_is_used_and_dropped() {
    let font = ui();
    let face = font.face();
    let m = face.glyph_index('M');
    let mut cache = GlyphCache::new(ENTRY_OVERHEAD); // room for no ink at all
    let width = cache.with(face, m, px(24), |g| g.width);
    assert_eq!(width, 17, "still handed out");
    assert_eq!((cache.used(), cache.len()), (0, 0));
    assert_eq!(cache.stats().uncached, 1);
    // A space (no ink) fits exactly.
    cache.with(face, face.glyph_index(' '), px(24), |_| ());
    assert_eq!((cache.used(), cache.len()), (ENTRY_OVERHEAD, 1));
    // A zero-byte cache caches nothing and still draws.
    let mut font = Font::ui(0).unwrap();
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
    let font = ui();
    let face = font.face();
    let chars: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
    let sizes = [px(9), px(16), px(24)];
    // Each (glyph, size)'s cost, worked out up front.
    let mut costs = std::collections::BTreeMap::new();
    for &c in &chars {
        for &s in &sizes {
            costs.insert((face.glyph_index(c), s), cost(&face.rasterize(face.glyph_index(c), s)));
        }
    }
    for (seed, bound) in [(1_u64, 600_usize), (2, 2_000), (3, 8_000), (4, 50)] {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let mut cache = GlyphCache::new(bound);
        // The model: keys oldest first, and their total cost.
        let mut model: VecDeque<(u16, Px)> = VecDeque::new();
        let (mut hits, mut misses, mut evictions, mut uncached) = (0, 0, 0, 0);
        for _ in 0..3_000 {
            let c = chars[rng.below(chars.len() as u64) as usize];
            let s = sizes[rng.below(sizes.len() as u64) as usize];
            let key = (face.glyph_index(c), s);
            let expected = face.rasterize(key.0, s);
            if let Some(at) = model.iter().position(|k| *k == key) {
                hits += 1;
                model.remove(at);
                model.push_back(key);
            } else {
                misses += 1;
                let c = costs[&key];
                if c > bound {
                    uncached += 1;
                } else {
                    while model.iter().map(|k| costs[k]).sum::<usize>() + c > bound {
                        model.pop_front();
                        evictions += 1;
                    }
                    model.push_back(key);
                }
            }
            let got = cache.with(face, key.0, s, |g| g.clone());
            assert_eq!(got, expected, "the glyph handed out is the glyph");
            assert!(cache.used() <= bound, "{} > {bound}", cache.used());
            assert_eq!(cache.used(), model.iter().map(|k| costs[k]).sum::<usize>());
            assert_eq!(cache.len(), model.len());
        }
        for k in &model {
            assert!(cache.contains(k.0, k.1));
        }
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.evictions, stats.uncached), (hits, misses, evictions, uncached));
        assert!(bound == 50 || stats.hits > 0, "seed {seed}: the test exercised hits");
    }
}

#[test]
fn measure_matches_render_without_touching_the_cache() {
    let mut font = ui();
    assert_eq!(font.measure("", px(24)), Measure { width: 0, ascent: 23, descent: 7 });
    let m = font.measure(sample::TEXT, px(24));
    assert_eq!(font.stats(), Stats::default(), "measuring rasterizes nothing");
    assert_eq!(font.cached_bytes(), 0);
    // The advances add up: 'x' is 708 64ths, so "xxx" is 2124 = 33.19 px,
    // rounded up to 34.
    assert_eq!(font.measure("xxx", px(24)).width, 34);
    assert_eq!(font.measure(" ", px(24)).width, 7);
    // A line's width is where its last glyph's pen ends: render the text
    // plus one more 'x' and that 'x' lands at the pixel nearest that pen.
    let text = format!("{}x", sample::TEXT);
    let drawn = placed(&mut font, &text, px(24));
    let last = drawn.last().unwrap();
    let pen_end = sample::TEXT.chars().map(|c| font.face().advance(font.face().glyph_index(c), px(24))).sum::<i32>();
    assert_eq!(last.0, (pen_end + 32) / 64);
    assert_eq!(m.width, ((pen_end + 63) / 64) as u32);
    assert_eq!((m.ascent, m.descent), (23, 7));
}

#[test]
fn render_mask_is_the_highest_coverage_of_any_glyph_at_each_pixel() {
    let mut font = ui();
    // "fj": the f's hook and the j's tail come close or overlap.
    for text in ["fj", sample::TEXT, "\u{263A}Wg", ""] {
        let drawn = placed(&mut font, text, px(24));
        let mask = font.render_mask(text, px(24));
        assert_eq!(mask.coverage.len(), (mask.width * mask.height) as usize);
        let measure = font.measure(text, px(24));
        assert!(mask.x <= 0 && mask.y <= 0);
        assert!(mask.x + mask.width as i32 >= measure.width as i32);
        assert!(mask.y + mask.height as i32 >= measure.ascent + measure.descent);
        for row in 0..mask.height as i32 {
            for col in 0..mask.width as i32 {
                let (x, y) = (mask.x + col, mask.y + row);
                let expected = drawn
                    .iter()
                    .filter(|(gx, gy, gw, gh, _)| {
                        x >= *gx && x < gx + i32::from(*gw) && y >= *gy && y < gy + i32::from(*gh)
                    })
                    .map(|(gx, gy, gw, _, c)| c[((y - gy) * i32::from(*gw) + (x - gx)) as usize])
                    .max()
                    .unwrap_or(0);
                assert_eq!(mask.coverage[(row * mask.width as i32 + col) as usize], expected, "{text:?} at {x},{y}");
            }
        }
        // Every glyph lies inside the mask.
        for (gx, gy, gw, gh, _) in &drawn {
            assert!(*gx >= mask.x && *gy >= mask.y);
            assert!(gx + i32::from(*gw) <= mask.x + mask.width as i32);
            assert!(gy + i32::from(*gh) <= mask.y + mask.height as i32);
        }
    }
}

#[test]
fn fnv1a_covers_the_header_and_the_coverage() {
    let mask = Mask { x: 0, y: 0, width: 1, height: 1, coverage: Vec::from([7]) };
    let moved = Mask { x: -1, ..mask.clone() };
    let darker = Mask { coverage: Vec::from([8]), ..mask.clone() };
    assert_ne!(mask.fnv1a(), moved.fnv1a());
    assert_ne!(mask.fnv1a(), darker.fnv1a());
    // An empty mask hashes its 16 header bytes, all zero.
    let empty = Mask { x: 0, y: 0, width: 0, height: 0, coverage: Vec::new() };
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for _ in 0..16 {
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    assert_eq!(empty.fnv1a(), h);
}

/// The golden: `tests/golden/sample.pgm` (a binary PGM, viewable as an
/// image), re-recorded with `LUMEN_FONT_BLESS=1 cargo test -p lumen-font`
/// (look at it before committing).
#[test]
fn the_sample_renders_as_the_golden() {
    let mut font = ui();
    let mask = font.render_mask(sample::TEXT, px(sample::PX));
    let mut pgm = format!(
        "P5\n# lumen-font sample {:?} at {} px, mask at ({}, {})\n{} {}\n255\n",
        sample::TEXT,
        sample::PX,
        mask.x,
        mask.y,
        mask.width,
        mask.height
    )
    .into_bytes();
    pgm.extend_from_slice(&mask.coverage);
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/sample.pgm");
    if std::env::var_os("LUMEN_FONT_BLESS").is_some() {
        std::fs::write(path, &pgm).unwrap();
    }
    let golden = std::fs::read(path).expect("tests/golden/sample.pgm (LUMEN_FONT_BLESS=1 records it)");
    assert!(golden == pgm, "the sample's mask differs from tests/golden/sample.pgm");
    assert_eq!(mask.fnv1a(), sample::HASH, "sample::HASH is {:#018x}", mask.fnv1a());
    // Not blank: plenty of full and partial coverage (anti-aliased).
    assert!(mask.coverage.iter().filter(|&&c| c == 255).count() > 300);
    assert!(mask.coverage.iter().filter(|&&c| c > 0 && c < 255).count() > 300);
}
