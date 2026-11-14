//! The 256-colour palette as `0xRRGGBB`: 16 named colours (Lumen's own, tuned
//! for its dark blue background), then xterm's 6x6x6 colour cube and its
//! 24-step grey ramp.

/// Colours 0 to 15: black, red, green, yellow, blue, magenta, cyan, white,
/// then their bright versions.
pub const NAMED: [u32; 16] = [
    0x1b2130, 0xe05561, 0x8cc265, 0xd9a54a, 0x4d8fe8, 0xc162de, 0x42b3c2, 0xc8ccd4, //
    0x5c6370, 0xff6b6b, 0xa5e075, 0xffd479, 0x6cb2ff, 0xde73ff, 0x5fe3f0, 0xffffff,
];

/// The six levels of each channel in the colour cube (16 to 231).
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Palette entry `i` as `0xRRGGBB`.
pub const fn rgb(i: u8) -> u32 {
    match i {
        0..=15 => NAMED[i as usize],
        16..=231 => {
            let n = i - 16;
            let (r, g, b) = (CUBE[(n / 36) as usize], CUBE[(n / 6 % 6) as usize], CUBE[(n % 6) as usize]);
            pack(r, g, b)
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            pack(v, v, v)
        }
    }
}

const fn pack(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// The palette index (16 to 255: the cube or the grey ramp, never the named
/// colours, which a theme may change) closest to `(r, g, b)`, for truecolor
/// SGR (`38;2;r;g;b`).
pub fn nearest(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| -> usize {
        // The cube level closest to v.
        (0..CUBE.len()).min_by_key(|&i| (i32::from(CUBE[i]) - i32::from(v)).abs()).unwrap_or(0)
    };
    let cube = 16 + 36 * level(r) + 6 * level(g) + level(b);
    let mean = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let grey = 232 + (mean.saturating_sub(3) / 10).min(23) as usize;
    let dist = |i: usize| {
        let c = rgb(i as u8);
        let d = |shift: u32, v: u8| {
            let x = i32::from(((c >> shift) & 0xff) as u8) - i32::from(v);
            x * x
        };
        d(16, r) + d(8, g) + d(0, b)
    };
    if dist(grey) < dist(cube) { grey as u8 } else { cube as u8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_ramp_match_xterm() {
        assert_eq!(rgb(16), 0x000000);
        assert_eq!(rgb(196), 0xff0000);
        assert_eq!(rgb(21), 0x0000ff);
        assert_eq!(rgb(231), 0xffffff);
        assert_eq!(rgb(67), 0x5f87af);
        assert_eq!(rgb(232), 0x080808);
        assert_eq!(rgb(255), 0xeeeeee);
        assert_eq!(rgb(9), NAMED[9]);
    }

    #[test]
    fn nearest_finds_exact_entries_and_greys() {
        for i in 16..=255u8 {
            let c = rgb(i);
            let back = nearest((c >> 16) as u8, (c >> 8) as u8, c as u8);
            assert_eq!(rgb(back), c, "entry {i}");
        }
        assert_eq!(nearest(255, 0, 0), 196);
        assert_eq!(nearest(250, 5, 5), 196);
        assert_eq!(nearest(128, 128, 128), 244);
        assert_eq!(nearest(0, 0, 0), 16);
    }
}
