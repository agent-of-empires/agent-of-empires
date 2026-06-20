//! Viewport breakpoints and layout helpers for narrow terminals.
//!
//! aoe runs over Mosh on phones and tablets where the viewport can be
//! anywhere from ~26 cols (iPhone-portrait Mosh, soft keyboard up) to
//! ~250 cols (full-screen desktop). All width/height-driven layout
//! decisions live here so the device-class assumptions are visible in
//! one file rather than scattered as magic numbers across render code.
//!
//! Why named constants and not raw ratios?
//! - ratatui's `Constraint::Length` is the right tool for fixed-cost
//!   chrome (borders, footers, status bar). A "70% border" is nonsense.
//! - Ratios are right for content panes but only above a usability
//!   floor: 33% of 30 cols is 10 cols, which can't render a tmux
//!   capture. Each constant below has a "below this it stops working"
//!   reason in its doc comment.
//! - The bug pattern these replace was *unnamed* hard numbers in render
//!   code, not the use of fixed sizes per se.

/// Below this width, the home view switches from side-by-side
/// (list | preview) to stacked (list above preview), and the preview
/// pane drops its info header in favor of just the session title +
/// status icon in the outer block title.
///
/// 80 is the conventional "narrow terminal" boundary: at default
/// list_width (35), side-by-side preview at viewport 80 is 45 cols,
/// barely usable; below that the floor binds and both panes lose. A
/// full-width stacked preview reads better than a 45-col side-by-side
/// one, and phone widths (Mosh landscape, Termius) live in this range.
pub const STACKED_BREAKPOINT: u16 = 80;

/// Minimum width the preview pane needs to render a tmux capture
/// without hash-soup wrapping. Used as the side-by-side preview floor.
pub const PREVIEW_MIN_WIDTH: u16 = 40;

/// Width of the collapsed-sidebar strip: a bordered column wide enough
/// for the `»` expand glyph plus its two border columns. Everything else
/// goes to the preview, so the strip is intentionally minimal.
pub const COLLAPSED_STRIP_WIDTH: u16 = 3;

/// In stacked mode the list takes 1/N of vertical space.
pub const STACKED_LIST_HEIGHT_FRACTION: u16 = 3;

/// Lower bound on stacked-mode list height. Below this the list can't
/// show selection + 1 neighbor + spinner row.
pub const STACKED_LIST_HEIGHT_MIN: u16 = 5;

/// Upper bound on stacked-mode list height; keeps the preview from
/// being squeezed on tall viewports.
pub const STACKED_LIST_HEIGHT_MAX: u16 = 12;

/// Lower bound on stacked-mode preview height. Below this the
/// selection header + 1 row of capture get clipped.
pub const STACKED_PREVIEW_MIN: u16 = 8;

/// Send-message dialog targets this percentage of viewport width.
pub const DIALOG_TARGET_PCT: u16 = 80;

/// Below this width, the dialog takes the full viewport (truncates but
/// stays visible). The 26-col floor is the width of the title hints
/// (" Enter send Esc cancel " plus rounded borders); below that the
/// hints disappear regardless of clamp choice, so taking the full
/// viewport at least preserves the message area.
pub const DIALOG_MIN_WIDTH: u16 = 26;

/// Cap on dialog width so it doesn't sprawl across wide desktops.
pub const DIALOG_MAX_WIDTH: u16 = 80;

/// Compute send-message dialog width for a given viewport width.
///
/// Below [`DIALOG_MIN_WIDTH`], take the full viewport.
/// Otherwise target [`DIALOG_TARGET_PCT`] of viewport, clamped to
/// `[DIALOG_MIN_WIDTH, DIALOG_MAX_WIDTH]`.
pub fn dialog_width(viewport_width: u16) -> u16 {
    if viewport_width <= DIALOG_MIN_WIDTH {
        viewport_width
    } else {
        ((viewport_width as u32 * DIALOG_TARGET_PCT as u32 / 100) as u16)
            .clamp(DIALOG_MIN_WIDTH, DIALOG_MAX_WIDTH)
            .min(viewport_width)
    }
}

/// Compute stacked-mode list pane height for a given main-region height.
pub fn stacked_list_height(main_height: u16) -> u16 {
    (main_height / STACKED_LIST_HEIGHT_FRACTION)
        .clamp(STACKED_LIST_HEIGHT_MIN, STACKED_LIST_HEIGHT_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_width_iphone_portrait() {
        // ~50 cols (iPhone-portrait Mosh zoomed out): 80% of 50 = 40,
        // above MIN_WIDTH 26 so the clamp is a no-op.
        assert_eq!(dialog_width(50), 40);
    }

    #[test]
    fn dialog_width_under_min_takes_full_viewport() {
        // soft keyboard up on iPhone: 22 cols → 22 (truncate but visible).
        assert_eq!(dialog_width(22), 22);
        assert_eq!(dialog_width(DIALOG_MIN_WIDTH), DIALOG_MIN_WIDTH);
    }

    #[test]
    fn dialog_width_caps_at_max() {
        // Wide desktop: 80% of 200 = 160, capped to MAX_WIDTH 80.
        assert_eq!(dialog_width(200), DIALOG_MAX_WIDTH);
    }

    #[test]
    fn dialog_width_does_not_exceed_viewport() {
        // Any viewport ≥ MIN; width never exceeds viewport.
        for w in DIALOG_MIN_WIDTH..=DIALOG_MAX_WIDTH * 2 {
            assert!(dialog_width(w) <= w, "dialog_width({w}) > {w}");
        }
    }

    #[test]
    fn stacked_list_height_clamped() {
        assert_eq!(stacked_list_height(10), STACKED_LIST_HEIGHT_MIN);
        assert_eq!(stacked_list_height(15), 5);
        assert_eq!(stacked_list_height(30), 10);
        assert_eq!(stacked_list_height(60), STACKED_LIST_HEIGHT_MAX);
    }
}
