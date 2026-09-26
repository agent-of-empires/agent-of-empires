//! Truecolor -> xterm-256 palette mapping used by the `palette_mode` color option.

use ratatui::style::Color;

/// Convert a 24-bit RGB color to the nearest xterm-256 palette index.
///
/// The xterm-256 palette has three zones:
///   0-15    : 16 basic ANSI colors (skipped — we prefer cube/grey approximations
///             over ambiguous terminal-configurable basics)
///   16-231  : 6×6×6 RGB cube. Axis levels are [0, 95, 135, 175, 215, 255].
///   232-255 : 24-step greyscale ramp from #080808 to #eeeeee.
///
/// Strategy: compute both the cube candidate and the grey candidate, return
/// whichever is closer to the input in squared-distance.
pub fn rgb_to_palette_index(r: u8, g: u8, b: u8) -> u8 {
    const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    fn nearest_cube_axis(v: u8) -> (usize, u8) {
        let mut best_i = 0;
        let mut best_d = i32::MAX;
        for (i, level) in CUBE_LEVELS.iter().enumerate() {
            let d = (v as i32 - *level as i32).abs();
            if d < best_d {
                best_d = d;
                best_i = i;
            }
        }
        (best_i, CUBE_LEVELS[best_i])
    }

    let (ri, rc) = nearest_cube_axis(r);
    let (gi, gc) = nearest_cube_axis(g);
    let (bi, bc) = nearest_cube_axis(b);
    let cube_idx = 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8;
    let cube_d = sq_dist(r, g, b, rc, gc, bc);

    // Greyscale ramp: level[i] = 8 + 10*i for i in 0..24 → 8, 18, ..., 238.
    // Plus #000000 (via cube 16) and #ffffff (via cube 231) bracketing.
    let grey_target = ((r as u32 + g as u32 + b as u32) / 3) as u8;
    let (grey_idx, grey_level) = if grey_target < 8 {
        (16u8, 0u8) // black via cube — better than grey[0]=#080808 for pure black
    } else if grey_target > 238 {
        (231u8, 255u8) // white via cube
    } else {
        let i = ((grey_target as i32 - 8) / 10).clamp(0, 23) as u8;
        (232 + i, 8 + 10 * i)
    };
    let grey_d = sq_dist(r, g, b, grey_level, grey_level, grey_level);

    if grey_d < cube_d {
        grey_idx
    } else {
        cube_idx
    }
}

fn sq_dist(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> i32 {
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    dr * dr + dg * dg + db * db
}

/// Convert a ratatui Color to its palette-mode equivalent. Only `Rgb(r,g,b)`
/// is transformed; other variants (Reset, Indexed, named) are returned as-is.
pub fn color_to_palette(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_palette_index(r, g, b)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_downsampling() {
        // Pure primaries land exactly on the 6x6x6 cube extreme indexes.
        assert_eq!(rgb_to_palette_index(255, 0, 0), 196);
        assert_eq!(rgb_to_palette_index(0, 255, 0), 46);
        assert_eq!(rgb_to_palette_index(0, 0, 255), 21);
        assert_eq!(rgb_to_palette_index(255, 255, 0), 226);
        assert_eq!(rgb_to_palette_index(0, 255, 255), 51);
        assert_eq!(rgb_to_palette_index(255, 0, 255), 201);
        assert_eq!(rgb_to_palette_index(255, 255, 255), 231);
        assert_eq!(rgb_to_palette_index(0, 0, 0), 16);

        // Grey values around the middle of the ramp should pick a 232-255 index,
        // not a cube vertex — grey ramp is denser near #808080 than the cube.
        let mid_grey = rgb_to_palette_index(128, 128, 128);
        assert!(
            (232..=255).contains(&mid_grey),
            "expected grey-ramp index for #808080, got {}",
            mid_grey
        );
        assert_eq!(color_to_palette(Color::Reset), Color::Reset);
        assert_eq!(color_to_palette(Color::Indexed(42)), Color::Indexed(42));
        assert_eq!(color_to_palette(Color::Red), Color::Red);
    }
}
