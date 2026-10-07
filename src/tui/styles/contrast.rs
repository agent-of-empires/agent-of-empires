//! WCAG contrast helpers used by the TUI to decide when a status color
//! still reads against a background (e.g. session row fg vs the highlight
//! bg) and when we need to swap in `theme.text` for legibility.

use ratatui::style::Color;

/// WCAG 2.x relative luminance for sRGB. `None` when the color isn't an
/// `Rgb(..)` (palette / named / Reset); callers should treat that as
/// "can't determine" and fall back to a safe default.
fn relative_luminance(c: Color) -> Option<f32> {
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => return None,
    };
    let to_lin = |c: u8| -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * to_lin(r) + 0.7152 * to_lin(g) + 0.0722 * to_lin(b))
}

/// WCAG contrast ratio between two colors. `None` if either side isn't
/// `Color::Rgb` (downsampled palette themes, named colors, `Reset`).
pub fn contrast_ratio(a: Color, b: Color) -> Option<f32> {
    let la = relative_luminance(a)?;
    let lb = relative_luminance(b)?;
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// True when `fg` on `bg` clears the WCAG `threshold` (e.g. 3.0 for AA
/// Large / bold UI text, 4.5 for AA Normal).
///
/// Returns `false` for any non-Rgb pair so palette / named / Reset colors
/// take the conservative fallback path. Palette-mode themes downsample
/// every color to `Color::Indexed`, so this matches the pre-contrast
/// "always override" behavior for those modes — there's no clean way to
/// compute WCAG luminance on an xterm-256 index without an inverse table,
/// and selection-bg readability matters more than per-status color when
/// the user already opted into a lossy color space.
pub fn has_min_contrast(fg: Color, bg: Color, threshold: f32) -> bool {
    contrast_ratio(fg, bg).is_some_and(|r| r >= threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: u32) -> Color {
        Color::Rgb(
            ((hex >> 16) & 0xff) as u8,
            ((hex >> 8) & 0xff) as u8,
            (hex & 0xff) as u8,
        )
    }

    #[test]
    fn contrast_ratio_math() {
        for (fg, bg, want) in [(0x000000, 0xffffff, 21.0), (0x6272a4, 0x6272a4, 1.0)] {
            let r = contrast_ratio(rgb(fg), rgb(bg)).unwrap();
            assert!((r - want).abs() < 0.01, "{fg:06x}/{bg:06x}: {r}");
        }
        let a = contrast_ratio(rgb(0x3c3c3c), rgb(0x50785a)).unwrap();
        let b = contrast_ratio(rgb(0x50785a), rgb(0x3c3c3c)).unwrap();
        assert!((a - b).abs() < 0.001, "symmetric");
        assert!(contrast_ratio(Color::Reset, rgb(0xffffff)).is_none());
        assert!(contrast_ratio(rgb(0xffffff), Color::Indexed(10)).is_none());
    }

    #[test]
    fn has_min_contrast_cases() {
        let cases = [
            // dracula dim == session_selection: 1.0 fails any positive floor.
            (rgb(0x6272a4), rgb(0x6272a4), 1.5, false),
            // phosphor running vs session_selection: ~8.4.
            (rgb(0x00ffb4), rgb(0x3c3c3c), 3.0, true),
            // phosphor dim vs session_selection: ~2.19, the case that
            // motivated the override-to-theme.text fix.
            (rgb(0x50785a), rgb(0x3c3c3c), 3.0, false),
            // Non-RGB and palette-mode colors take the conservative path.
            (Color::Reset, rgb(0xffffff), 3.0, false),
            (Color::Indexed(2), rgb(0x3c3c3c), 3.0, false),
        ];
        for (fg, bg, min, want) in cases {
            assert_eq!(has_min_contrast(fg, bg, min), want, "{fg:?} on {bg:?}");
        }
    }
}
