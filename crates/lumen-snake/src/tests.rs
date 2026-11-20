use super::*;
use std::collections::BTreeSet;

/// A small sky: every level's field fits.
const W: u16 = 16;
const H: u16 = 10;

fn game(seed: u64) -> Game {
    Game::new(W, H, seed).expect("a sky big enough")
}

/// The grain moved to `c` (an empty cell), for a test that needs it there.
fn put_dust(g: &mut Game, c: Cell) {
    if let Some(old) = g.dust {
        g.set(old, Content::Empty);
    }
    assert_eq!(g.content(c), Content::Empty, "{c:?}");
    g.set(c, Content::Dust);
    g.dust = Some(c);
}

/// The grain moved off the comet's way (to the bottom right corner).
fn dust_aside(g: &mut Game) {
    put_dust(g, Cell::new(W - 1, H - 1));
}

fn grid(g: &Game) -> Vec<Content> {
    let mut out = Vec::new();
    for y in 0..g.height() {
        for x in 0..g.width() {
            out.push(g.content(Cell::new(x, y)));
        }
    }
    out
}

/// What must hold after every step: the grid agrees with the comet (one
/// head, its tail, nothing else of it), one grain on an empty cell (or
/// none, the sky full), the comet's cells next to each other.
fn check(g: &Game) {
    let cells: Vec<Cell> = g.comet().collect();
    assert_eq!(g.content(cells[0]), Content::Head);
    for c in &cells[1..] {
        assert_eq!(g.content(*c), Content::Tail, "{c:?}");
    }
    for pair in cells.windows(2) {
        assert_eq!(pair[0].x.abs_diff(pair[1].x) + pair[0].y.abs_diff(pair[1].y), 1, "{pair:?}");
    }
    let unique: BTreeSet<Cell> = cells.iter().copied().collect();
    assert_eq!(unique.len(), cells.len(), "the comet crosses itself");
    let g2 = grid(g);
    assert_eq!(g2.iter().filter(|&&c| c == Content::Head).count(), 1);
    assert_eq!(g2.iter().filter(|&&c| c == Content::Tail).count(), cells.len() - 1);
    match g.dust() {
        Some(d) => {
            assert_eq!(g.content(d), Content::Dust);
            assert!(!unique.contains(&d), "a grain on the comet");
            assert_eq!(g2.iter().filter(|&&c| c == Content::Dust).count(), 1);
        }
        None => assert_eq!(g.state(), State::Full),
    }
}

#[test]
fn rng_is_the_same_for_a_seed_and_below_stays_below() {
    let (mut a, mut b) = (Rng::new(7), Rng::new(7));
    for _ in 0..1000 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
    assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    let mut r = Rng::new(1);
    let mut seen = [0u32; 6];
    for _ in 0..6000 {
        let v = r.below(6);
        assert!(v < 6);
        seen[v as usize] += 1;
    }
    // Every value comes up, roughly as often (a thousand each).
    assert!(seen.iter().all(|&n| (800..1200).contains(&n)), "{seen:?}");
    assert_eq!(Rng::new(3).below(1), 0);
}

#[test]
#[should_panic(expected = "below(0)")]
fn below_zero_is_refused() {
    Rng::new(0).below(0);
}

#[test]
fn a_game_starts_on_the_middle_row_heading_right() {
    let g = game(1);
    assert_eq!(g.state(), State::Flying);
    assert_eq!(g.comet().collect::<Vec<_>>(), [Cell::new(3, 5), Cell::new(2, 5), Cell::new(1, 5)]);
    assert_eq!((g.score(), g.level(), g.steps(), g.heading()), (0, 0, 0, Dir::Right));
    assert!(g.dust().is_some());
    check(&g);
    assert!(Game::new(MIN_W - 1, H, 1).is_none());
    assert!(Game::new(W, MIN_H - 1, 1).is_none());
    assert!(Game::new(MIN_W, MIN_H, 1).is_some());
}

#[test]
fn straight_on_it_hits_the_edge_and_stays_put() {
    let mut g = game(1);
    dust_aside(&mut g);
    // From x = 3 to the last column, W - 1.
    for _ in 0..W - 1 - START_LEN {
        assert!(!g.step().is_empty());
        assert_eq!(g.state(), State::Flying);
    }
    assert_eq!(g.head(), Cell::new(W - 1, 5));
    let before = g.clone();
    assert!(g.step().is_empty());
    assert_eq!(g.state(), State::Over(Crash::Edge));
    // Nothing moved; nothing more happens.
    assert_eq!(g.comet().collect::<Vec<_>>(), before.comet().collect::<Vec<_>>());
    assert!(g.step().is_empty());
    assert!(!g.turn(Dir::Up));
}

#[test]
fn every_edge_ends_it() {
    for (turns, crash_at) in [
        (vec![Dir::Up], Cell::new(START_LEN, 0)),
        (vec![Dir::Down], Cell::new(START_LEN, H - 1)),
        (vec![Dir::Up, Dir::Left], Cell::new(0, 4)),
    ] {
        let mut g = game(2);
        dust_aside(&mut g);
        for d in turns {
            assert!(g.turn(d));
            g.step();
        }
        while g.state() == State::Flying {
            g.step();
        }
        assert_eq!(g.state(), State::Over(Crash::Edge));
        assert_eq!(g.head(), crash_at);
    }
}

#[test]
fn a_turn_back_into_the_tail_is_refused() {
    let mut g = game(3);
    dust_aside(&mut g);
    assert!(!g.turn(Dir::Left));
    assert_eq!(g.heading(), Dir::Right);
    assert!(g.turn(Dir::Up));
    // Not yet moved up: still refused is only the way back over the last
    // move (right), so down (back over nothing yet) is taken.
    assert!(g.turn(Dir::Down));
    g.step();
    assert!(!g.turn(Dir::Up));
    assert!(g.turn(Dir::Left));
}

#[test]
fn a_grain_scores_and_grows_the_tail_a_cell_a_step() {
    let mut g = game(4);
    put_dust(&mut g, Cell::new(START_LEN + 1, 5));
    let changed = g.step();
    assert_eq!(g.score(), 1);
    assert_eq!(g.len(), START_LEN as usize);
    // The grain eaten; the next one placed (and reported).
    let next = g.dust().expect("a new grain");
    assert!(changed.contains(&next));
    dust_aside(&mut g);
    for k in 1..=GROWTH as usize {
        g.step();
        assert_eq!(g.len(), START_LEN as usize + k);
    }
    g.step();
    assert_eq!(g.len(), START_LEN as usize + GROWTH as usize);
    check(&g);
}

#[test]
fn it_ends_against_its_own_tail() {
    let mut g = game(5);
    dust_aside(&mut g);
    g.grow = 6;
    // A tight turn: up, left, down runs into the tail.
    for d in [Dir::Up, Dir::Left, Dir::Down] {
        assert!(g.turn(d));
        g.step();
        if d != Dir::Down {
            assert_eq!(g.state(), State::Flying);
        }
    }
    assert_eq!(g.state(), State::Over(Crash::Tail));
}

#[test]
fn it_may_move_into_the_cell_its_tail_leaves() {
    let mut g = game(6);
    dust_aside(&mut g);
    // Four long, round a 2 x 2 square: each step's head goes where the
    // tail's end just left.
    g.grow = 1;
    g.step();
    assert_eq!(g.len(), 4);
    for _ in 0..3 {
        for d in [Dir::Down, Dir::Left, Dir::Up, Dir::Right] {
            assert!(g.turn(d));
            g.step();
            assert_eq!(g.state(), State::Flying, "{d:?}");
            check(&g);
        }
    }
    // But not while growing: the end stays, and the next move round the
    // square runs into it.
    g.grow = 1;
    assert!(g.turn(Dir::Down));
    assert!(g.step().is_empty());
    assert_eq!(g.state(), State::Over(Crash::Tail));
}

#[test]
fn an_asteroid_ends_it() {
    let mut g = game(7);
    dust_aside(&mut g);
    g.set(Cell::new(START_LEN + 2, 5), Content::Rock);
    g.step();
    assert_eq!(g.state(), State::Flying);
    assert!(g.step().is_empty());
    assert_eq!(g.state(), State::Over(Crash::Rock));
}

/// A random flight: turns from its own generator (so the game's isn't
/// touched), a step after each, until it ends or `steps` pass; `check` after
/// every step, and `each` with the changed cells and the grids around them.
fn fly(seed: u64, steps: usize, mut each: impl FnMut(&Game, &[Content], &[Cell], &[Content])) -> Game {
    let mut g = game(seed);
    let mut pilot = Rng::new(seed ^ 0x5eed);
    for _ in 0..steps {
        if g.state() != State::Flying {
            break;
        }
        if pilot.below(3) == 0 {
            let d = [Dir::Up, Dir::Down, Dir::Left, Dir::Right][pilot.below(4) as usize];
            g.turn(d);
        }
        let before = grid(&g);
        let changed = g.step();
        let after = grid(&g);
        check(&g);
        each(&g, &before, &changed, &after);
    }
    g
}

fn rock_count(g: &Game) -> usize {
    grid(g).iter().filter(|&&c| c == Content::Rock).count()
}

/// The cells a step changed are exactly the ones it reported, each once.
fn reports_exactly(g: &Game, before: &[Content], changed: &[Cell], after: &[Content]) {
    let reported: BTreeSet<Cell> = changed.iter().copied().collect();
    assert_eq!(reported.len(), changed.len(), "a cell reported twice");
    let differ: BTreeSet<Cell> = (0..before.len())
        .filter(|&i| before[i] != after[i])
        .map(|i| Cell::new((i % usize::from(g.width())) as u16, (i / usize::from(g.width())) as u16))
        .collect();
    assert_eq!(reported, differ);
}

#[test]
fn every_step_reports_exactly_the_cells_it_changed() {
    let mut steps = 0;
    for seed in 0..300 {
        fly(seed, 400, |g, before, changed, after| {
            reports_exactly(g, before, changed, after);
            steps += 1;
        });
    }
    assert!(steps > 3000, "{steps} steps flown");
}

#[test]
fn a_grain_never_lands_on_the_comet_or_an_asteroid() {
    // `check` after every step says so; enough flights reach every level,
    // and each step's damage is exact there too.
    let mut levels = BTreeSet::new();
    let mut grains = 0;
    for seed in 0..300 {
        let mut g = game(seed);
        let mut pilot = Rng::new(seed);
        // Fly towards each grain (greedy steering, with some wandering).
        for _ in 0..2000 {
            if g.state() != State::Flying {
                break;
            }
            if let Some(d) = g.dust() {
                let h = g.head();
                let want = if d.x > h.x {
                    Dir::Right
                } else if d.x < h.x {
                    Dir::Left
                } else if d.y > h.y {
                    Dir::Down
                } else {
                    Dir::Up
                };
                if !g.turn(want) || pilot.below(5) == 0 {
                    g.turn([Dir::Up, Dir::Down][pilot.below(2) as usize]);
                }
            }
            // Half the time the grain is moved to the cell ahead, if empty,
            // so flights eat enough to reach every level (each next grain
            // is still the game's own placing, which is what's checked).
            if pilot.below(2) == 0
                && let Some(ahead) = g.next_cell(g.head(), g.heading())
                && g.content(ahead) == Content::Empty
            {
                put_dust(&mut g, ahead);
            }
            let (score, level, rocks_before) = (g.score(), g.level(), rock_count(&g));
            let before = grid(&g);
            let changed = g.step();
            check(&g);
            // A grain never lands on an asteroid (it would erase it): the
            // rocks only change with the level.
            if g.level() == level {
                assert_eq!(rock_count(&g), rocks_before, "seed {seed}: a rock gone or added");
            }
            // Every level is reached here, so level changes' damage is
            // checked too.
            reports_exactly(&g, &before, &changed, &grid(&g));
            grains += (g.score() - score) as usize;
            levels.insert(g.level());
        }
    }
    assert!(grains > 500, "{grains} grains");
    assert_eq!(levels.len(), LEVELS.len(), "{levels:?}");
}

#[test]
fn one_seed_plays_one_game() {
    for seed in [0, 1, 42, u64::MAX] {
        let a = fly(seed, 500, |_, _, _, _| {});
        let b = fly(seed, 500, |_, _, _, _| {});
        assert_eq!(a, b);
    }
    // Another seed, another sky: the first grains differ among a few seeds.
    let firsts: BTreeSet<Option<Cell>> = (0..8).map(|s| game(s).dust()).collect();
    assert!(firsts.len() > 4, "{firsts:?}");
}

#[test]
fn levels_change_every_few_grains_and_the_last_stays() {
    let mut g = game(9);
    for grain in 1..=LEVEL_EVERY * (LEVELS.len() as u32 + 1) {
        // The grain straight ahead, the comet kept from the edge: it turns
        // back and forth on rows it clears.
        let h = g.head();
        let ahead = match g.heading() {
            Dir::Right if h.x + 1 < W => Cell::new(h.x + 1, h.y),
            Dir::Left if h.x > 0 => Cell::new(h.x - 1, h.y),
            _ => unreachable!(),
        };
        if g.content(ahead) == Content::Rock {
            g.set(ahead, Content::Empty);
        }
        if g.content(ahead) != Content::Dust {
            put_dust(&mut g, ahead);
        }
        let old_rocks: Vec<Cell> = rocks(g.level(), W, H);
        let level = g.level();
        let changed = g.step();
        assert_eq!(g.score(), grain);
        let want = ((grain / LEVEL_EVERY) as usize).min(LEVELS.len() - 1);
        assert_eq!(g.level(), want);
        if want != level {
            // The old field gone (unless it is the new one's), the new
            // one's empty cells rocks, all reported.
            let new_rocks: BTreeSet<Cell> = rocks(want, W, H).into_iter().collect();
            for c in old_rocks.iter().filter(|c| !new_rocks.contains(c)) {
                assert_ne!(g.content(*c), Content::Rock, "{c:?}");
            }
            for c in &new_rocks {
                let held = g.content(*c);
                assert!(matches!(held, Content::Rock | Content::Head | Content::Tail | Content::Dust), "{c:?}");
                if held == Content::Rock {
                    assert!(changed.contains(c) || old_rocks.contains(c), "{c:?}");
                }
            }
        }
        // Turn round at the edges (a U-turn two rows down, clear of rocks).
        let h = g.head();
        if (g.heading() == Dir::Right && h.x + 2 >= W) || (g.heading() == Dir::Left && h.x <= 1) {
            let back = g.heading().opposite();
            for c in [Cell::new(h.x, h.y + 1), Cell::new(h.x, h.y + 2)] {
                if g.content(c) == Content::Rock || g.content(c) == Content::Dust {
                    g.set(c, Content::Empty);
                    if g.dust == Some(c) {
                        g.dust = None;
                        put_dust(&mut g, Cell::new(0, 0));
                    }
                }
            }
            g.turn(Dir::Down);
            g.step();
            g.turn(back);
        }
        assert_eq!(g.state(), State::Flying, "grain {grain}");
    }
    assert_eq!(g.level(), LEVELS.len() - 1);
}

#[test]
fn every_field_lies_on_the_sky() {
    for (w, h) in [(MIN_W, MIN_H), (W, H), (SKY_W, SKY_H), (40, 30)] {
        assert!(rocks(0, w, h).is_empty());
        for level in 1..LEVELS.len() {
            let field = rocks(level, w, h);
            assert!(!field.is_empty(), "level {level} on {w} x {h}");
            let unique: BTreeSet<Cell> = field.iter().copied().collect();
            assert_eq!(unique.len(), field.len(), "a rock twice: level {level} on {w} x {h}");
            for c in &field {
                assert!(c.x < w && c.y < h, "{c:?} off a {w} x {h} sky");
            }
        }
    }
}

#[test]
fn a_full_sky_ends_it() {
    let mut g = game(10);
    for i in 0..g.grid.len() {
        if g.grid[i] == Content::Empty || g.grid[i] == Content::Dust {
            g.grid[i] = Content::Rock;
        }
    }
    g.dust = None;
    let mut changed = Vec::new();
    g.place_dust(&mut changed);
    assert_eq!(g.state(), State::Full);
    assert!(changed.is_empty());
    assert!(g.step().is_empty());
}

#[test]
fn the_scenario_s_seed_puts_its_first_grain_ahead() {
    // tests/scenarios/test_apps.py's Comet tests run `--seed` with
    // COMET_SEED on the app's sky: its first grain is on the start row,
    // COMET_DUST_AHEAD cells ahead of the head, so that many Step presses
    // score it, and the next ones fly into the edge.
    let g = Game::new(SKY_W, SKY_H, COMET_SEED).unwrap();
    let h = g.head();
    assert_eq!(g.dust(), Some(Cell::new(h.x + COMET_DUST_AHEAD, h.y)));
    let mut g = g;
    for _ in 0..COMET_DUST_AHEAD {
        g.step();
    }
    assert_eq!(g.score(), 1);
    // The second grain isn't on the row ahead either: straight on, it
    // ends at the edge with a score of 1, after COMET_STEPS_TO_EDGE steps.
    let mut steps = COMET_DUST_AHEAD as u64;
    while g.state() == State::Flying {
        g.step();
        steps += 1;
    }
    assert_eq!((g.state(), g.score()), (State::Over(Crash::Edge), 1));
    assert_eq!(g.steps(), u64::from(COMET_STEPS_TO_EDGE));
    assert_eq!(steps, u64::from(COMET_STEPS_TO_EDGE) + 1);
}

/// The scenario's numbers (tests/scenarios/test_apps.py says the same).
const COMET_SEED: u64 = 65;
const COMET_DUST_AHEAD: u16 = 6;
const COMET_STEPS_TO_EDGE: u16 = 20;

#[test]
fn the_labels_say_what_happened() {
    assert_eq!(score_label(12), "Score 12");
    assert_eq!(level_label(0), "Level 1: Open sky");
    assert_eq!(level_label(3), "Level 4: Ring of rocks");
    assert_eq!(comet_label(Cell::new(23, 17), Dir::Right), "at 23,17 right");
    assert_eq!(comet_label(Cell::new(0, 0), Dir::Up), "at 0,0 up");
    let names: Vec<&str> = [Dir::Up, Dir::Down, Dir::Left, Dir::Right].iter().map(|d| d.name()).collect();
    assert_eq!(names, ["up", "down", "left", "right"]);
    let states = [
        (State::Flying, false, "flying"),
        (State::Flying, true, "stepping"),
        (State::Over(Crash::Edge), false, "over: edge"),
        (State::Over(Crash::Tail), true, "over: tail"),
        (State::Over(Crash::Rock), false, "over: asteroid"),
        (State::Full, true, "sky full"),
    ];
    for (state, stepping, said) in states {
        assert_eq!(state_label(state, stepping), said);
    }
}

#[test]
fn every_label_fits_its_width() {
    // Measured as the app's toolkit lays its labels out: the UI font at
    // the apps' text size. A text past LABEL_W would be clipped.
    let face = lumen_font::Face::ui().expect("the UI font parses");
    let px = lumen_font::Px::new(lumen_app_api::limits::UI_TEXT_PX as u16).unwrap();
    let width = |t: &str| face.measure(t, px).width;
    let mut texts: Vec<String> = (0..LEVELS.len()).map(level_label).collect();
    texts.push(score_label(u32::from(SKY_W) * u32::from(SKY_H)));
    for state in
        [State::Flying, State::Over(Crash::Edge), State::Over(Crash::Tail), State::Over(Crash::Rock), State::Full]
    {
        texts.push(state_label(state, false).into());
        texts.push(state_label(state, true).into());
    }
    for y in 0..SKY_H {
        for x in 0..SKY_W {
            for d in [Dir::Up, Dir::Down, Dir::Left, Dir::Right] {
                texts.push(comet_label(Cell::new(x, y), d));
            }
        }
    }
    for t in &texts {
        assert!(width(t) <= LABEL_W, "{t:?} is {} px, past {LABEL_W}", width(t));
    }
    // The widest is close to the width: the column isn't needlessly wide.
    let widest = texts.iter().map(|t| width(t)).max().unwrap();
    assert!(widest + 32 > LABEL_W, "the widest label is {widest} px");
}

#[test]
fn rng_is_splitmix64() {
    // SplitMix64's reference outputs for seed 1234567 (as published with
    // the algorithm), and a seed's high bits count.
    let mut r = Rng::new(1_234_567);
    let want =
        [6457827717110365317, 3203168211198807973, 9817491932198370423, 4593380528125082431, 16408922859458223821];
    for w in want {
        assert_eq!(r.next_u64(), w);
    }
    assert_ne!(Rng::new(1 << 40).next_u64(), Rng::new(0).next_u64());
    assert_ne!(Game::new(SKY_W, SKY_H, 1 << 40), Game::new(SKY_W, SKY_H, 0));
}

/// SplitMix64's output is a bijection of its state, so a test can pick the
/// seed whose first draw is any value it wants.
fn seed_drawing_first(out: u64) -> u64 {
    fn unxorshift(y: u64, k: u32) -> u64 {
        let mut x = y;
        for _ in 0..64 / k + 1 {
            x = y ^ (x >> k);
        }
        x
    }
    fn inv(c: u64) -> u64 {
        let mut x = c;
        for _ in 0..6 {
            x = x.wrapping_mul(2u64.wrapping_sub(c.wrapping_mul(x)));
        }
        x
    }
    let z2 = unxorshift(out, 31);
    let z1 = unxorshift(z2.wrapping_mul(inv(0x94d0_49bb_1331_11eb)), 27);
    let z = unxorshift(z1.wrapping_mul(inv(0xbf58_476d_1ce4_e5b9)), 30);
    z.wrapping_sub(0x9e37_79b9_7f4a_7c15)
}

#[test]
fn below_draws_again_past_the_last_whole_multiple() {
    let seed = seed_drawing_first(u64::MAX);
    let mut r = Rng::new(seed);
    assert_eq!(r.next_u64(), u64::MAX);
    let second = r.next_u64();
    for n in [3u32, 5, 15, 17] {
        // 2^64 - 1 is a multiple of n: the zone is u64::MAX, which is past it.
        assert_eq!(u64::MAX % u64::from(n), 0);
        assert_ne!(second % u64::from(n), 0, "{n}");
        assert_eq!(u64::from(Rng::new(seed).below(n)), second % u64::from(n), "{n}");
    }
}

#[test]
fn one_empty_cell_left_still_takes_a_grain_and_eating_the_last_fills_the_sky() {
    let mut g = game(10);
    let ahead = Cell::new(g.head().x + 1, g.head().y);
    let spare = Cell::new(W - 1, H - 1);
    for i in 0..g.grid.len() {
        if g.grid[i] == Content::Empty || g.grid[i] == Content::Dust {
            g.grid[i] = Content::Rock;
        }
    }
    g.dust = None;
    g.set(spare, Content::Empty);
    let mut changed = Vec::new();
    g.place_dust(&mut changed);
    assert_eq!((g.state(), g.dust(), changed.as_slice()), (State::Flying, Some(spare), &[spare][..]));
    // Now the grain right ahead, the last empty cell: eating it fills the sky.
    g.set(spare, Content::Rock);
    g.set(ahead, Content::Dust);
    g.dust = Some(ahead);
    g.grow = 1; // still growing: the tail's end stays
    let head = g.head();
    let changed = g.step();
    assert_eq!(g.state(), State::Full);
    assert_eq!((g.score(), g.dust()), (1, None));
    assert_eq!(changed, [head, ahead]);
}

#[test]
fn two_turns_in_one_step_keep_only_the_last_allowed() {
    // Heading right: Up then Left before the next step (a U-turn typed
    // quickly): Left is refused against the last move, so only Up is kept.
    let mut g = game(1);
    dust_aside(&mut g);
    assert!(g.turn(Dir::Up));
    assert!(!g.turn(Dir::Left));
    g.step();
    g.step();
    assert_eq!(g.head(), Cell::new(3, 3), "the second turn was dropped");
}

#[test]
fn a_second_grain_while_growing_adds_its_growth_too() {
    let mut g = game(11);
    put_dust(&mut g, Cell::new(START_LEN + 1, 5));
    g.step();
    put_dust(&mut g, Cell::new(START_LEN + 2, 5));
    g.step();
    assert_eq!(g.score(), 2);
    dust_aside(&mut g);
    for _ in 0..2 * GROWTH {
        g.step();
    }
    assert_eq!(g.comet().count(), (START_LEN + 2 * GROWTH as u16) as usize);
    g.step();
    assert_eq!(g.comet().count(), (START_LEN + 2 * GROWTH as u16) as usize);
}

/// A field as rows of `#` (a rock) and `.`.
fn picture(field: &[Cell], w: u16, h: u16) -> Vec<String> {
    (0..h).map(|y| (0..w).map(|x| if field.contains(&Cell::new(x, y)) { '#' } else { '.' }).collect()).collect()
}

#[test]
fn the_fields_on_comet_s_sky() {
    // Our own layouts, pinned on the app's sky.
    let twin_belts = [
        "........................",
        "........................",
        "........................",
        "........................",
        "........................",
        "........................",
        "......############......",
        "........................",
        "........................",
        "........................",
        "........................",
        "......############......",
        "........................",
        "........................",
        "........................",
        "........................",
        "........................",
        "........................",
    ];
    let crossing = [
        "........................",
        "........................",
        "........................",
        "........................",
        "............#...........",
        "............#...........",
        "............#...........",
        "............#...........",
        "........................",
        "......#####...####......",
        "........................",
        "............#...........",
        "............#...........",
        "............#...........",
        "........................",
        "........................",
        "........................",
        "........................",
    ];
    let ring_of_rocks = [
        "........................",
        "........................",
        "..##.##.##.##.##.##.##..",
        "..#.....................",
        ".....................#..",
        "..#..................#..",
        "..#.....................",
        ".....................#..",
        "..#..................#..",
        "..#.....................",
        ".....................#..",
        "..#..................#..",
        "..#.....................",
        ".....................#..",
        "..#..................#..",
        "..#.##.##.##.##.##.##...",
        "........................",
        "........................",
    ];
    assert!(rocks(0, SKY_W, SKY_H).is_empty());
    for (level, want) in [(1, twin_belts), (2, crossing), (3, ring_of_rocks)] {
        assert_eq!(picture(&rocks(level, SKY_W, SKY_H), SKY_W, SKY_H), want, "{}", LEVELS[level]);
    }
}

#[test]
fn the_scenario_s_turns_run_into_the_tail() {
    // test_apps.py's steering test: from the scenario's seed, the grain
    // eaten (growing), then up, left and down: into its own tail.
    let mut g = Game::new(SKY_W, SKY_H, COMET_SEED).unwrap();
    for _ in 0..COMET_DUST_AHEAD {
        g.step();
    }
    let h = g.head();
    assert_eq!((g.score(), h), (1, Cell::new(START_LEN + COMET_DUST_AHEAD, SKY_H / 2)));
    for d in [Dir::Up, Dir::Left] {
        assert!(g.turn(d));
        g.step();
        assert_eq!(g.state(), State::Flying);
    }
    assert!(g.turn(Dir::Down));
    g.step();
    assert_eq!(g.state(), State::Over(Crash::Tail));
    // And from the start: down, then right, both moves made.
    let mut g = Game::new(SKY_W, SKY_H, COMET_SEED).unwrap();
    assert!(g.turn(Dir::Down));
    g.step();
    assert!(g.turn(Dir::Right));
    g.step();
    assert_eq!(g.head(), Cell::new(START_LEN + 1, SKY_H / 2 + 1));
    assert_eq!(g.score(), 0);
}
