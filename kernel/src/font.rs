//! The desktop's anti-aliased text (`lumen-font`, #148) in the kernel. The
//! desktop doesn't draw with it yet (M4's compositor will); for now this
//! renders the golden sample for the shell's `font` and its ktest, which
//! show how long `fontdue`'s soft-float parsing and rasterizing take here
//! and that the kernel's mask is the host's, byte for byte.

use crate::shell::Ctx;
use crate::time;
use alloc::string::String;
use lumen_font::{Font, Px, Stats, sample};
use lumen_time::Millis;

/// The glyph cache's bound for the sample: far more than its glyphs take,
/// so nothing is evicted and the second render hits for every glyph.
const CACHE_BYTES: usize = 256 * 1024;

const PX: Px = match Px::new(sample::PX) {
    Some(px) => px,
    None => panic!("the sample's size is out of range"),
};

/// What rendering the sample twice found.
pub struct Sample {
    pub name: Option<String>,
    pub glyphs: u16,
    /// Parsing the embedded font (every glyph's outline).
    pub parse_ns: u64,
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

impl Sample {
    /// Whether the mask is the host's golden.
    pub fn is_golden(&self) -> bool {
        self.hash == sample::HASH
    }
}

/// Parse the UI font and render [`sample::TEXT`] twice, timing each step.
pub fn render_sample() -> Result<Sample, lumen_font::Error> {
    let start = time::now_ns();
    let mut font = Font::ui(CACHE_BYTES)?;
    let parsed = time::now_ns();
    let mask = font.render_mask(sample::TEXT, PX);
    let first_done = time::now_ns();
    let first = font.stats();
    let again = font.render_mask(sample::TEXT, PX);
    let second_done = time::now_ns();
    Ok(Sample {
        name: font.face().name().map(String::from),
        glyphs: font.face().glyph_count(),
        parse_ns: parsed - start,
        first_ns: first_done - parsed,
        first,
        second_ns: second_done - first_done,
        second: font.stats(),
        width: mask.width,
        height: mask.height,
        hash: mask.fnv1a(),
        same_again: again == mask,
        cached_bytes: font.cached_bytes(),
    })
}

/// `font`: the sample rendered twice, in three lines:
///
/// ```text
/// font: Atkinson Hyperlegible Regular, 369 glyphs, parsed in 12.345 ms
/// font: "Lumen: AVATAR fj 0123" at 24 px: 251x30, rendered in 3.000 ms (17 rasterized), again in 0.100 ms (0 rasterized)
/// font: cache 21 hits, 17 misses, 0 evictions, 4096 of 262144 bytes; mask fnv1a 0x02cb17f54193ee6d (the golden)
/// ```
///
/// A mask that isn't the golden ends its line with `(not the golden
/// 0x...)` and fails the command.
pub fn command(ctx: &mut Ctx<'_>, _: &[String]) {
    let s = match render_sample() {
        Ok(s) => s,
        Err(e) => return ctx.fail(format_args!("font: {e}")),
    };
    let out = &mut *ctx.out;
    let _ = writeln!(
        out,
        "font: {}, {} glyphs, parsed in {}",
        s.name.as_deref().unwrap_or("(unnamed)"),
        s.glyphs,
        Millis(s.parse_ns)
    );
    let _ = writeln!(
        out,
        "font: {:?} at {} px: {}x{}, rendered in {} ({} rasterized), again in {} ({} rasterized{})",
        sample::TEXT,
        sample::PX,
        s.width,
        s.height,
        Millis(s.first_ns),
        s.first.misses,
        Millis(s.second_ns),
        s.second.misses - s.first.misses,
        if s.same_again { "" } else { ", a different mask" },
    );
    let _ = write!(
        out,
        "font: cache {} hits, {} misses, {} evictions, {} of {CACHE_BYTES} bytes; mask fnv1a {:#018x} ",
        s.second.hits, s.second.misses, s.second.evictions, s.cached_bytes, s.hash
    );
    if s.is_golden() && s.same_again {
        let _ = writeln!(out, "(the golden)");
    } else {
        let _ = writeln!(out);
        ctx.fail(format_args!("font: not the golden {:#018x}", sample::HASH));
    }
}

crate::ktest!(
    fn renders_and_caches() {
        use crate::{kassert, kassert_eq};
        // The embedded font parses, the sample's mask is the host golden's
        // (soft-float is IEEE exact, so the bytes agree), and the second
        // render rasterizes nothing: every glyph is a hit.
        let s = match render_sample() {
            Ok(s) => s,
            Err(e) => {
                crate::ktest::diag(format_args!("{e}"));
                return Err("the embedded font doesn't parse");
            }
        };
        log::info!(
            target: "font",
            "sample: parsed {} glyphs in {}, rendered in {} ({} rasterized), again in {} ({} rasterized)",
            s.glyphs,
            Millis(s.parse_ns),
            Millis(s.first_ns),
            s.first.misses,
            Millis(s.second_ns),
            s.second.misses - s.first.misses,
        );
        kassert_eq!(s.name.as_deref(), Some("Atkinson Hyperlegible Regular"));
        kassert_eq!(s.hash, sample::HASH);
        kassert!(s.same_again);
        kassert!(s.first.misses > 0);
        kassert_eq!(s.second.misses, s.first.misses);
        let chars = sample::TEXT.chars().count() as u64;
        kassert_eq!(s.second.hits, s.first.hits + chars);
        kassert_eq!(s.second.evictions, 0);
        kassert!(s.cached_bytes > 0 && s.cached_bytes <= CACHE_BYTES);
        Ok(())
    }
);
