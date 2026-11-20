//! The desktop's anti-aliased text (`lumen-font`, #148) in the kernel: the
//! UI font, parsed once and shared ([`face`]), and [`Text`], one drawing
//! thread's glyph cache over it, which the desktop's titles and windows
//! draw with (#154). Here it also renders the golden sample for the
//! shell's `font` and its ktest, which show how long `fontdue`'s soft-float
//! parsing and rasterizing take and that the kernel's mask hashes to the
//! host golden's value.

use crate::shell::Ctx;
use crate::time;
use alloc::boxed::Box;
use alloc::string::String;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use lumen_font::{Face, Font, Px, Stats, TooWide, sample};
use lumen_gfx::{Mask, Rect, Surface};
use lumen_time::Millis;

/// The glyph cache's bound for the sample: far more than its glyphs take,
/// so nothing is evicted and the second render hits for every glyph.
const CACHE_BYTES: usize = 256 * 1024;

/// The widest line the sample's mask may take.
const MAX_WIDTH: u32 = 4096;

const PX: Px = px(sample::PX);

/// The parsed UI font and how long parsing it took. Only [`face`] makes
/// one.
pub struct Loaded {
    face: Face,
    parse_ns: u64,
}

impl Loaded {
    pub fn face(&self) -> &Face {
        &self.face
    }

    /// How long the first use spent parsing.
    pub fn parse_ns(&self) -> u64 {
        self.parse_ns
    }
}

/// Null until the first [`face`] publishes the parsed font; never freed
/// after (a `Box` leaked on purpose: the font lives as long as the kernel).
static LOADED: AtomicPtr<Loaded> = AtomicPtr::new(ptr::null_mut());

// `AtomicPtr<T>` is `Sync` for any `T`, so it wouldn't stop a `Loaded`
// that isn't safe to share from being handed to every thread: this does.
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<Loaded>();
};

/// How many times the UI font has been parsed (each successful
/// `Face::ui`): once, unless two first uses raced. The ktest checks a use
/// after the first doesn't parse.
static PARSES: AtomicU32 = AtomicU32::new(0);

/// The UI font, parsed on first use and shared by every caller after.
/// Parsing takes about 60 ms under QEMU's TCG (soft-float), with no lock
/// held, so call it from a thread, not an interrupt handler. Two threads
/// that both find it unparsed both parse it; the first to publish wins and
/// the other's copy is dropped.
pub fn face() -> Result<&'static Loaded, lumen_font::Error> {
    let current = LOADED.load(Ordering::Acquire);
    if !current.is_null() {
        // SAFETY: a non-null pointer was published (Release) from
        // `Box::into_raw` below and is never freed or written again; a
        // shared `&Loaded` on any thread is sound (`Loaded: Sync`, asserted
        // above).
        return Ok(unsafe { &*current });
    }
    let start = time::now_ns();
    let face = Face::ui()?;
    let parse_ns = time::now_ns() - start;
    PARSES.fetch_add(1, Ordering::Relaxed);
    let ours = Box::into_raw(Box::new(Loaded { face, parse_ns }));
    match LOADED.compare_exchange(ptr::null_mut(), ours, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: `ours` came from `Box::into_raw` and is now published,
        // never to be freed (and `Loaded: Sync`, as above).
        Ok(_) => Ok(unsafe { &*ours }),
        Err(theirs) => {
            // SAFETY: `ours` was never published, so this is its only
            // owner.
            drop(unsafe { Box::from_raw(ours) });
            // SAFETY: `theirs` was published by the winner from
            // `Box::into_raw` and is never freed, as above.
            Ok(unsafe { &*theirs })
        }
    }
}

// -- text on the desktop ------------------------------------------------------

/// The desktop's text sizes, in pixels per em: a window's title, its text,
/// and a heading.
pub const TITLE_PX: Px = px(15);
pub const TEXT_PX: Px = px(16);
pub const HEADING_PX: Px = px(24);

const fn px(n: u16) -> Px {
    match Px::new(n) {
        Some(px) => px,
        None => panic!("a text size out of range"),
    }
}

/// Each [`Text`]'s glyph cache: far more than a few sizes of the glyphs on a
/// desktop take, so a redraw hits.
const TEXT_CACHE_BYTES: usize = 128 * 1024;

// An app's glyph cache (#206) is charged as `GLYPH_CACHE_BYTES`: the two
// must agree.
const _: () = assert!(TEXT_CACHE_BYTES as u64 == lumen_app_api::limits::GLYPH_CACHE_BYTES);

/// Glyphs [`Text::draw`] left out because `lumen_gfx::Mask::new` refused
/// their mask (a bug in `lumen-font`: its masks are always their size),
/// counted for `desktop status` (`glyphs_dropped`); the first is warned
/// about, naming its sizes.
static GLYPHS_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Set once the UI font failing to load has been warned about.
static NO_FONT_SAID: AtomicBool = AtomicBool::new(false);

/// Glyphs left out so far (see [`GLYPHS_DROPPED`]).
pub fn glyphs_dropped() -> u64 {
    GLYPHS_DROPPED.load(Ordering::Relaxed)
}

/// One drawing thread's text (#154): its own glyph cache over the shared UI
/// font, so drawing takes no shared lock and a glyph is rasterized once per
/// size, then drawn from the cache. The compositor has one for the titles;
/// each window's owner makes its own.
pub struct Text {
    font: Font<'static>,
}

impl Text {
    /// A glyph cache over the UI font (parsed on first use: call it from a
    /// thread), or why the font didn't load (warned about once a boot: then
    /// the caller draws no text).
    pub fn new() -> Result<Text, lumen_font::Error> {
        match face() {
            Ok(loaded) => Ok(Text { font: Font::new(loaded.face(), TEXT_CACHE_BYTES) }),
            Err(e) => {
                if !NO_FONT_SAID.swap(true, Ordering::Relaxed) {
                    log::warn!(target: "desktop", "no text: the UI font didn't load: {e}");
                }
                Err(e)
            }
        }
    }

    /// Draw `text` at `px` with its line box's top-left corner at (`x`,
    /// `y`), in `color`, touching only pixels inside `clip`: each glyph's
    /// mask blended on its own (no line mask allocated), clipped. A glyph
    /// whose box misses `clip` is skipped before it is rasterized, so the
    /// work follows the clip, not the text (an app's `text_draw`, #206).
    /// Blending isn't idempotent, so a redraw of one damaged rectangle
    /// passes that rectangle and the pixels outside it stay as they were.
    #[allow(clippy::too_many_arguments, reason = "a line of text: where, what, how big, which colour, inside what")]
    pub fn draw(&mut self, s: &mut Surface, x: i32, y: i32, text: &str, px: Px, color: u32, clip: Rect) {
        let visible =
            |gx: i32, gy: i32, w: u32, h: u32| Rect::new(gx, gy, w, h).translate(x, y).intersect(&clip).is_some();
        self.font.render_where(text, px, visible, |g| match Mask::new(g.width(), g.height(), g.coverage()) {
            Some(mask) => s.blend_mask_in(&mask, x.saturating_add(g.x()), y.saturating_add(g.y()), color, clip),
            None => {
                if GLYPHS_DROPPED.fetch_add(1, Ordering::Relaxed) == 0 {
                    log::warn!(
                        target: "desktop",
                        "a glyph's mask doesn't match its size ({}x{}, {} bytes): not drawn (counted in desktop status)",
                        g.width(),
                        g.height(),
                        g.coverage().len()
                    );
                }
            }
        });
    }

    /// `text`'s advance width at `px`, from the metrics alone (nothing
    /// rasterized): where [`Text::draw`]'s ink ends, give or take a glyph's
    /// side bearing.
    pub fn measure(&self, text: &str, px: Px) -> u32 {
        self.font.face().measure(text, px).width
    }

    /// A line's height at `px`: the face's ascent and descent.
    pub fn line_height(&self, px: Px) -> u32 {
        let m = self.font.face().line_metrics(px);
        u32::try_from(m.ascent + m.descent).unwrap_or(0)
    }

    /// The glyph cache's counters (`misses`: glyphs rasterized).
    pub fn stats(&self) -> Stats {
        self.font.stats()
    }
}

/// Why the sample didn't render.
#[derive(Clone, Copy, Debug)]
pub enum SampleError {
    Parse(lumen_font::Error),
    TooWide(TooWide),
}

impl fmt::Display for SampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SampleError::Parse(e) => e.fmt(f),
            SampleError::TooWide(e) => write!(f, "the sample: {e}"),
        }
    }
}

/// What rendering the sample twice found.
#[derive(Clone)]
pub struct Sample {
    pub loaded: &'static Loaded,
    /// The first render, rasterizing each distinct glyph.
    pub first_ns: u64,
    pub first: Stats,
    /// The second, from the cache.
    pub second_ns: u64,
    pub second: Stats,
    pub width: u32,
    pub height: u32,
    pub hash: u64,
    /// Whether the second render's mask was the first's.
    pub same_again: bool,
    pub cached_bytes: usize,
}

/// Render [`sample::TEXT`] twice with a fresh cache, timing each render.
pub fn render_sample() -> Result<Sample, SampleError> {
    let loaded = face().map_err(SampleError::Parse)?;
    let mut font = Font::new(loaded.face(), CACHE_BYTES);
    let start = time::now_ns();
    let mask = font.render_mask(sample::TEXT, PX, MAX_WIDTH).map_err(SampleError::TooWide)?;
    let first_done = time::now_ns();
    let first = font.stats();
    let again = font.render_mask(sample::TEXT, PX, MAX_WIDTH);
    let second_done = time::now_ns();
    Ok(Sample {
        loaded,
        first_ns: first_done - start,
        first,
        second_ns: second_done - first_done,
        second: font.stats(),
        width: mask.width(),
        height: mask.height(),
        hash: mask.fnv1a(),
        same_again: again.as_ref() == Ok(&mask),
        cached_bytes: font.cached_bytes(),
    })
}

/// `font`: the sample rendered twice. On success, three lines:
///
/// ```text
/// font: Atkinson Hyperlegible Regular, 369 glyphs, parsed in 61.784 ms on first use
/// font: "Lumen: AVATAR fj 0123" at 24 px: 251x30, rendered in 5.171 ms (17 rasterized), again in 0.155 ms (0 rasterized)
/// font: cache 25 hits, 17 misses, 0 evictions, 4114 of 262144 bytes; mask fnv1a 0xcaef385be76cda2b (the golden)
/// ```
pub fn command(ctx: &mut Ctx<'_>, _: &[String]) {
    match render_sample() {
        Ok(s) => report(ctx, &s),
        Err(e) => ctx.fail(format_args!("font: {e}")),
    }
}

/// `font`'s output for `s`. A mask that isn't the golden ends the last line
/// with `(not the golden 0x...)` instead of `(the golden)` and adds the
/// failure line `font: the mask isn't the golden`; a second render that
/// differs from the first adds `font: the second render's mask differs
/// from the first's`. When both happen, both failure lines are printed, the
/// second render's first.
fn report(ctx: &mut Ctx<'_>, s: &Sample) {
    let face = s.loaded.face();
    let out = &mut *ctx.out;
    let _ = writeln!(
        out,
        "font: {}, {} glyphs, parsed in {} on first use",
        face.name().unwrap_or("(unnamed)"),
        face.glyph_count(),
        Millis(s.loaded.parse_ns())
    );
    let _ = writeln!(
        out,
        "font: {:?} at {} px: {}x{}, rendered in {} ({} rasterized), again in {} ({} rasterized)",
        sample::TEXT,
        sample::PX,
        s.width,
        s.height,
        Millis(s.first_ns),
        s.first.misses,
        Millis(s.second_ns),
        s.second.misses - s.first.misses,
    );
    let _ = write!(
        out,
        "font: cache {} hits, {} misses, {} evictions, {} of {CACHE_BYTES} bytes; mask fnv1a {:#018x}",
        s.second.hits, s.second.misses, s.second.evictions, s.cached_bytes, s.hash
    );
    let golden = s.hash == sample::HASH;
    if golden {
        let _ = writeln!(out, " (the golden)");
    } else {
        let _ = writeln!(out, " (not the golden {:#018x})", sample::HASH);
    }
    if !s.same_again {
        ctx.fail("font: the second render's mask differs from the first's");
    }
    if !golden {
        ctx.fail("font: the mask isn't the golden");
    }
}

crate::ktest!(
    fn renders_and_caches() {
        // The embedded font parses, the sample's mask hashes to the host
        // golden's value, and the second render rasterizes nothing: every
        // glyph is a hit.
        use crate::{kassert, kassert_eq};
        let s = match render_sample() {
            Ok(s) => s,
            Err(e) => {
                crate::ktest::diag(format_args!("{e}"));
                return Err("the sample didn't render");
            }
        };
        log::info!(
            target: "font",
            "sample: parsed {} glyphs in {} (first use), rendered in {} ({} rasterized), again in {} ({} rasterized)",
            s.loaded.face().glyph_count(),
            Millis(s.loaded.parse_ns()),
            Millis(s.first_ns),
            s.first.misses,
            Millis(s.second_ns),
            s.second.misses - s.first.misses,
        );
        kassert_eq!(s.loaded.face().name(), Some("Atkinson Hyperlegible Regular"));
        kassert_eq!(s.hash, sample::HASH);
        kassert!(s.same_again);
        kassert!(s.first.misses > 0);
        kassert_eq!(s.second.misses, s.first.misses);
        let chars = sample::TEXT.chars().count() as u64;
        kassert_eq!(s.second.hits, s.first.hits + chars);
        kassert_eq!(s.second.evictions, 0);
        kassert_eq!(s.cached_bytes, 4114);
        // Parsed once: a later use is the same face and parses nothing
        // (counted, not `== 1`: another test may have been the first use).
        let parses = PARSES.load(Ordering::Relaxed);
        kassert!(parses >= 1);
        let again = match face() {
            Ok(loaded) => loaded,
            Err(_) => return Err("the font didn't parse the second time"),
        };
        kassert!(core::ptr::eq(again, s.loaded));
        kassert_eq!(PARSES.load(Ordering::Relaxed), parses);
        Ok(())
    }
);

crate::ktest!(
    fn font_reports_each_failure() {
        // `font`'s report for a good sample, a mask that isn't the golden,
        // a second render that differed, and both at once.
        use crate::{kassert, kassert_eq};
        let good = match render_sample() {
            Ok(s) => s,
            Err(_) => return Err("the sample didn't render"),
        };
        let run = |s: &Sample| {
            let mut console = crate::shell::Captured::default();
            let history = lumen_shell::History::new(0);
            report(&mut Ctx::new(&mut console, false, &history), s);
            console.text
        };
        const NOT_GOLDEN: &str = "font: the mask isn't the golden\n";
        const DIFFERS: &str = "font: the second render's mask differs from the first's\n";

        let text = run(&good);
        kassert_eq!(text.lines().count(), 3);
        kassert!(text.ends_with(" (the golden)\n"));
        kassert!(!text.contains(NOT_GOLDEN) && !text.contains(DIFFERS));

        let wrong = Sample { hash: !sample::HASH, ..good.clone() };
        let text = run(&wrong);
        let expected =
            alloc::format!("mask fnv1a {:#018x} (not the golden {:#018x})\n{NOT_GOLDEN}", !sample::HASH, sample::HASH);
        kassert!(text.ends_with(&expected));
        kassert!(!text.contains(DIFFERS) && !text.contains("(the golden)"));

        let differs = Sample { same_again: false, ..good.clone() };
        let text = run(&differs);
        kassert!(text.ends_with(&alloc::format!(" (the golden)\n{DIFFERS}")));
        kassert!(!text.contains(NOT_GOLDEN));

        let both = Sample { hash: !sample::HASH, same_again: false, ..good };
        let text = run(&both);
        kassert!(text.ends_with(&alloc::format!("\n{DIFFERS}{NOT_GOLDEN}")));
        kassert_eq!(text.lines().count(), 5);
        Ok(())
    }
);

crate::ktest!(
    fn text_ends_where_measure_says() {
        // A line drawn with `Text::draw` (glyph by glyph, clipped) has its
        // ink from where it starts to about its measured width: past it by
        // no more than a side bearing, short of it by no more than one.
        use crate::kassert;
        let mut text = Text::new().map_err(|_| "no UI font")?;
        let mut s = Surface::with_stride(400, 40, 400).ok_or("no memory")?;
        let (x, line) = (10, "Lumen: measure me");
        let all = s.bounds();
        text.draw(&mut s, x, 4, line, TEXT_PX, 0x00ff_ffff, all);
        let width = i32::try_from(text.measure(line, TEXT_PX)).map_err(|_| "too wide")?;
        let inked = |col: u32| (0..40).any(|y| s.row(y)[col as usize] != 0);
        let left = (0..400).find(|&c| inked(c)).ok_or("nothing drawn")? as i32;
        let right = (0..400).rev().find(|&c| inked(c)).ok_or("nothing drawn")? as i32 + 1;
        crate::ktest::diag(format_args!("measured {width}, ink {left}..{right}"));
        kassert!(left >= x && left <= x + 2);
        kassert!(right <= x + width + 2 && right >= x + width - 3);
        kassert!(text.line_height(TEXT_PX) >= 16);
        Ok(())
    }
);
