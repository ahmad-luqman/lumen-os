extern crate std;

use crate::life::{Life, PATTERNS, pattern};
use crate::mandel::{ONE, View, color, escape};
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
    // (The tests run with overflow checks.) The corners of a wide view and
    // points just outside the radius-2 disc.
    for &(re, im) in &[(-2.5, -1.25), (1.0, 1.25), (2.0, 2.0), (-2.0, 2.0), (1.9999, 0.0001), (-8.0, -8.0)] {
        let _ = escape(fixed(re), fixed(im), 1000);
    }
    let view = View::fit(1, 1, 1000);
    let _ = view.escape_at(0, 0);
}

#[test]
fn the_view_fits_the_set_centred() {
    // 1280x656 (the canvas under the header): the height limits it, so the
    // whole -1.25..1.25 is there and the real axis is the middle row.
    let view = View::fit(1280, 656, 64);
    let (re0, im0) = view.point(0, 0);
    let (re1, im1) = view.point(1279, 655);
    assert!(im0 <= -5 * ONE / 4 && im1 >= 5 * ONE / 4 - view.step);
    assert!(re0 <= -5 * ONE / 2 && re1 >= ONE - view.step);
    assert!((view.point(640, 328).1).abs() <= view.step);
    // Wide: the width limits it.
    let view = View::fit(700, 100, 64);
    assert!(view.point(0, 0).0 <= -5 * ONE / 2 && view.point(699, 0).0 >= ONE - view.step);
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
