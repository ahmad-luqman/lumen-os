extern crate std;

use crate::bands;
use crate::life::{Life, NAMES, ORIGIN, PATTERNS, Place, pattern};
use crate::mandel::{ONE, View, color, escape};
use core::num::NonZeroUsize;
use std::collections::{HashMap, HashSet};
use std::vec;
use std::vec::Vec;

/// `escape` in floating point, as the definition says.
fn escape_f64(re: f64, im: f64, max_iter: u32) -> u32 {
    let (mut zr, mut zi) = (0.0f64, 0.0f64);
    for n in 0..max_iter {
        if zr * zr + zi * zi > 4.0 {
            return n;
        }
        (zr, zi) = (zr * zr - zi * zi + re, 2.0 * zr * zi + im);
    }
    max_iter
}

fn fixed(v: f64) -> i64 {
    (v * ONE as f64).round() as i64
}

#[test]
fn known_points_escape_as_they_should() {
    let at = |re: f64, im: f64| escape(fixed(re), fixed(im), 100);
    // Inside: the centre of the main cardioid, the period-2 bulb, the tip of
    // the needle (z stays at 2 exactly), i (period 2).
    assert_eq!(at(0.0, 0.0), 100);
    assert_eq!(at(-1.0, 0.0), 100);
    assert_eq!(at(-2.0, 0.0), 100);
    assert_eq!(at(0.0, 1.0), 100);
    // Outside: 1 escapes at z = 5 (after 0, 1, 2, 5), 2 at z = 6.
    assert_eq!(at(1.0, 0.0), escape_f64(1.0, 0.0, 100));
    assert_eq!(at(1.0, 0.0), 3);
    assert_eq!(at(2.0, 0.0), 2);
    // Far away: at once after the first step.
    assert_eq!(at(10.0, 10.0), 1);
}

#[test]
fn fixed_point_agrees_with_floating_point() {
    // Over a whole image, the escape counts agree except at a few points
    // right on the boundary, where rounding decides.
    let (w, h, max) = (320, 200, 64);
    let view = View::fit(w, h, max);
    let s = 1.0 / ONE as f64;
    let mut differ = 0;
    for y in 0..h {
        for x in 0..w {
            let (re, im) = view.point(x, y);
            if view.escape_at(x, y) != escape_f64(re as f64 * s, im as f64 * s, max) {
                differ += 1;
            }
        }
    }
    assert!(differ * 1000 < w * h, "{differ} of {} differ", w * h);
}

#[test]
fn extreme_points_never_overflow() {
    // (The tests run with overflow checks.) The corners of a wide view,
    // points just outside the radius-2 disc and far beyond it, and i64's
    // extremes.
    for &(re, im) in &[
        (-2.5, -1.25),
        (1.0, 1.25),
        (2.0, 2.0),
        (-2.0, 2.0),
        (1.9999, 0.0001),
        (-8.0, -8.0),
        (12.0, 0.0),
        (0.0, -12.0),
        (1e6, 1e6),
    ] {
        let _ = escape(fixed(re), fixed(im), 1000);
    }
    for re in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX] {
        for im in [i64::MIN, 0, i64::MAX] {
            let _ = escape(re, im, 100);
        }
    }
    // Every pixel of views far wider or taller than the set (the terminal
    // accepts such screens), and pixels far outside a view.
    for (w, h) in [(1280, 16), (100, 700), (1, 1), (4000, 1)] {
        let view = View::fit(w, h, 64);
        for y in 0..h {
            for x in 0..w {
                let _ = view.escape_at(x, y);
            }
        }
    }
    let _ = View::fit(100, 100, 64).escape_at(usize::MAX, usize::MAX);
    let _ = View::fit(usize::MAX, usize::MAX, 64).escape_at(usize::MAX / 2, 0);
}

#[test]
fn points_beyond_two_escape_after_one_step() {
    // |re| or |im| above 2: answered without iterating, as iterating would
    // (z₁ = c is out); 0 iterations allowed means inside, as for any point.
    for &(re, im) in &[(2.01, 0.0), (-2.01, 0.0), (0.0, 2.5), (-3.0, -3.0), (2.0001, 2.0)] {
        assert_eq!(escape(fixed(re), fixed(im), 50), 1);
        assert_eq!(escape_f64(re, im, 50), 1);
    }
    assert_eq!(escape(fixed(5.0), 0, 0), 0);
}

#[test]
fn the_view_fits_the_set_centred() {
    // Centred on -0.75 + 0i, the whole -2.5..1 by -1.25..1.25 in view.
    let fits = |w: usize, h: usize| {
        let view = View::fit(w, h, 64);
        let (re0, im0) = view.point(0, 0);
        let (re1, im1) = view.point(w - 1, h - 1);
        assert!(re0 <= -5 * ONE / 2 && re1 >= ONE - view.step, "{w}x{h}: re {re0}..{re1}");
        assert!(im0 <= -5 * ONE / 4 && im1 >= 5 * ONE / 4 - view.step, "{w}x{h}: im {im0}..{im1}");
        let (re, im) = view.point(w / 2, h / 2);
        assert!((re + 3 * ONE / 4).abs() <= view.step && im.abs() <= view.step, "{w}x{h}: centre {re}, {im}");
        view
    };
    // 1280x656 (the canvas under the header) is wider than the set's 7:5:
    // the height limits the step.
    let view = fits(1280, 656);
    assert_eq!(view.step, (5 * ONE / 2 + 655) / 656);
    // Taller than 7:5: the width limits it.
    let view = fits(350, 700);
    assert_eq!(view.step, (7 * ONE / 2 + 349) / 350);
}

#[test]
fn bands_deal_every_row_to_exactly_one_slot() {
    for height in [0, 1, 7, 8, 9, 32, 197, 656] {
        for slots in 1..=8 {
            for band in [1, 8, 13] {
                let mut seen = vec![0u32; height];
                let mut last_band = vec![None; slots];
                for slot in 0..slots {
                    let n = NonZeroUsize::new(slots).unwrap();
                    for rows in bands::rows(slot, n, height, NonZeroUsize::new(band).unwrap()) {
                        // A slot's band: whole, or the last one cut short.
                        assert!(rows.start % band == 0 && (rows.len() == band || rows.end == height));
                        assert_eq!((rows.start / band) % slots, slot);
                        assert!(last_band[slot] < Some(rows.start));
                        last_band[slot] = Some(rows.start);
                        rows.for_each(|y| seen[y] += 1);
                    }
                }
                assert!(seen.iter().all(|&n| n == 1), "{height} rows, {slots} slots, bands of {band}");
            }
        }
    }
}

#[test]
fn inside_is_black_and_outside_is_coloured() {
    assert_eq!(color(64, 64), 0);
    assert!((0..64).all(|n| color(n, 64) != 0));
    assert_eq!(color(3, 64), color(19, 64));
}

/// The live cells of `life`.
fn alive(life: &Life) -> Vec<(usize, usize)> {
    (0..life.height()).flat_map(|y| (0..life.width()).map(move |x| (x, y))).filter(|&(x, y)| life.get(x, y)).collect()
}

#[test]
fn a_glider_moves_one_cell_diagonally_every_four_generations() {
    let mut life = Life::new(20, 20);
    life.place(pattern("glider").unwrap(), 2, 2);
    let start = alive(&life);
    for _ in 0..8 {
        life.step(|_, _, _| {});
    }
    assert_eq!(life.generation(), 8);
    let moved: Vec<_> = start.iter().map(|&(x, y)| (x + 2, y + 2)).collect();
    assert_eq!(alive(&life), moved);
}

#[test]
fn still_lifes_stay_and_oscillators_oscillate() {
    let mut life = Life::new(10, 10);
    // A block and a blinker.
    for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2), (5, 6), (6, 6), (7, 6)] {
        life.set(x, y, true);
    }
    let start = alive(&life);
    let mut changes = Vec::new();
    life.step(|x, y, a| changes.push((x, y, a)));
    changes.sort();
    // Only the blinker changed: its ends died, cells above and below were born.
    assert_eq!(changes, [(5, 6, false), (6, 5, true), (6, 7, true), (7, 6, false)]);
    life.step(|_, _, _| {});
    assert_eq!(alive(&life), start);
}

#[test]
fn the_edge_is_dead() {
    // A blinker on the edge: the cell beyond it doesn't count, so its far
    // end has one neighbour; the cell above the middle (outside) can't be
    // born. Vertical at x 0: the middle's left and right are (−1, y) and
    // (1, y); only (1, y) is born.
    let mut life = Life::new(5, 5);
    for y in 1..4 {
        life.set(0, y, true);
    }
    life.step(|_, _, _| {});
    assert_eq!(alive(&life), [(0, 2), (1, 2)]);
    // Placing past the edge leaves those cells out.
    let mut life = Life::new(3, 3);
    life.place(pattern("gun").unwrap(), 1, 1);
    assert!(life.population() <= 9);
}

#[test]
fn the_gun_fires_a_glider_every_thirty_generations() {
    let mut life = Life::new(80, 60);
    let gun = pattern("gun").unwrap();
    life.place(gun, 1, 1);
    assert_eq!(life.population(), 36);
    for _ in 0..30 {
        life.step(|_, _, _| {});
    }
    // The gun again, plus one glider.
    assert_eq!(life.population(), 36 + 5);
    let gun_cells: Vec<_> = gun.cells().map(|(x, y)| (x + 1, y + 1)).collect();
    assert!(gun_cells.iter().all(|&(x, y)| life.get(x, y)));
}

#[test]
fn every_pattern_has_cells_and_a_unique_name() {
    for (i, p) in PATTERNS.iter().enumerate() {
        assert!(p.cells().count() > 0, "{}", p.name);
        assert!(PATTERNS[..i].iter().all(|q| q.name != p.name));
        let (w, h) = p.size();
        assert!(p.cells().all(|(x, y)| x < w && y < h));
    }
    assert!(pattern("nope").is_none());
}

/// A pattern's live cells from its RLE (as LifeWiki gives it), for
/// checking [`PATTERNS`] against an independent source.
fn rle(text: &str) -> Vec<(usize, usize)> {
    let (mut x, mut y, mut n) = (0, 0, 0);
    let mut cells = Vec::new();
    for c in text.chars() {
        match c {
            '0'..='9' => n = n * 10 + c.to_digit(10).unwrap() as usize,
            'b' | 'o' | '$' => {
                let count = n.max(1);
                match c {
                    'b' => x += count,
                    'o' => {
                        cells.extend((x..x + count).map(|x| (x, y)));
                        x += count;
                    }
                    _ => (x, y) = (0, y + count),
                }
                n = 0;
            }
            '!' => break,
            _ => {}
        }
    }
    cells.sort();
    cells
}

#[test]
fn every_pattern_matches_its_published_rle() {
    let known = [
        ("glider", "bo$2bo$3o!"),
        ("gun", "24bo$22bobo$12b2o6b2o12b2o$11bo3bo4b2o12b2o$2o8bo5bo3b2o$2o8bo3bob2o4bobo$10bo5bo7bo$11bo3bo$12b2o!"),
        ("lwss", "bo2bo$o4b$o3bo$4o!"),
        ("rpentomino", "b2o$2o$bo!"),
        ("acorn", "bo5b$3bo3b$2o2b3o!"),
    ];
    assert_eq!(known.len(), PATTERNS.len());
    for (name, text) in known {
        let mut cells: Vec<_> = pattern(name).unwrap().cells().collect();
        cells.sort();
        assert_eq!(cells, rle(text), "{name}");
    }
}

#[test]
fn the_names_are_the_patterns() {
    assert_eq!(NAMES.len(), PATTERNS.len());
    assert!(NAMES.iter().zip(PATTERNS).all(|(n, p)| *n == p.name));
}

#[test]
fn the_rules_are_b3_s23() {
    // A cell with n live neighbours (n = 0..=8), alive or dead: born with
    // exactly 3, survives with 2 or 3, else dead.
    let around = [(0, 0), (1, 0), (2, 0), (0, 1), (2, 1), (0, 2), (1, 2), (2, 2)];
    for alive in [false, true] {
        for n in 0..=8 {
            let mut life = Life::new(3, 3);
            life.set(1, 1, alive);
            for &(x, y) in &around[..n] {
                life.set(x, y, true);
            }
            life.step(|_, _, _| {});
            assert_eq!(life.get(1, 1), n == 3 || (alive && n == 2), "alive {alive}, {n} neighbours");
        }
    }
}

#[test]
fn a_lwss_moves_two_cells_left_every_four_generations() {
    let lwss = pattern("lwss").unwrap();
    let mut life = Life::new(30, 12);
    let (x, y) = lwss.origin(30, 12);
    assert_eq!((x, y), (30 - 5 - ORIGIN, (12 - 4) / 2));
    life.place(lwss, x, y);
    let start = alive(&life);
    for _ in 0..4 {
        life.step(|_, _, _| {});
    }
    let moved: Vec<_> = start.iter().map(|&(x, y)| (x - 2, y)).collect();
    assert_eq!(alive(&life), moved);
}

#[test]
fn patterns_start_where_their_place_says() {
    for p in PATTERNS {
        let (w, h) = p.size();
        let (x, y) = p.origin(160, 80);
        assert!(x + w <= 160 && y + h <= 80, "{}", p.name);
        match p.place {
            Place::TopLeft => assert_eq!((x, y), (ORIGIN, ORIGIN)),
            Place::Centre => assert_eq!((x, y), ((160 - w) / 2, (80 - h) / 2)),
            Place::Right => assert_eq!((x, y), (160 - w - ORIGIN, (80 - h) / 2)),
        }
    }
    // A grid smaller than the pattern: at the edge, not past it.
    assert_eq!(pattern("acorn").unwrap().origin(3, 2), (0, 0));
}

/// Life on an endless plane, the obvious way (a set of live cells): the
/// reference the grid is checked against.
fn step_plane(cells: &HashSet<(i64, i64)>) -> HashSet<(i64, i64)> {
    let mut counts: HashMap<(i64, i64), u32> = HashMap::new();
    for &(x, y) in cells {
        for dy in -1..=1 {
            for dx in -1..=1 {
                if (dx, dy) != (0, 0) {
                    *counts.entry((x + dx, y + dy)).or_default() += 1;
                }
            }
        }
    }
    counts.into_iter().filter(|&(c, n)| n == 3 || (n == 2 && cells.contains(&c))).map(|(c, _)| c).collect()
}

fn plane(name: &str) -> HashSet<(i64, i64)> {
    pattern(name).unwrap().cells().map(|(x, y)| (x as i64, y as i64)).collect()
}

#[test]
fn the_r_pentomino_stabilises_at_116_cells_after_1103_generations() {
    // Its known history on an endless plane checks the reference.
    let mut cells = plane("rpentomino");
    for _ in 0..1103 {
        cells = step_plane(&cells);
    }
    assert_eq!(cells.len(), 116);
}

#[test]
fn the_grid_agrees_with_an_endless_plane_away_from_its_edges() {
    // R-pentomino and acorn in the middle of a grid large enough that
    // nothing reaches the edge in 150 generations: the same cells as on the
    // plane, generation by generation (the gun's gliders included).
    for name in ["rpentomino", "acorn", "gun"] {
        let (size, offset) = (200usize, 100i64);
        let mut life = Life::new(size, size);
        life.place(pattern(name).unwrap(), offset as usize, offset as usize);
        let mut cells = plane(name);
        for generation in 1..=150 {
            life.step(|_, _, _| {});
            cells = step_plane(&cells);
            let mut want: Vec<_> = cells.iter().map(|&(x, y)| ((x + offset) as usize, (y + offset) as usize)).collect();
            want.sort_by_key(|&(x, y)| (y, x));
            assert_eq!(alive(&life), want, "{name}, generation {generation}");
        }
    }
}
