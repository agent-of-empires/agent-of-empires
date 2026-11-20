//! Viewport breakpoints and layout helpers for narrow terminals.
//!
//! aoe runs over Mosh on phones where the viewport can be ~26 cols, and on
//! full-screen desktops at ~250. Every width/height-driven decision lives here
//! so the device-class assumptions are visible in one file rather than as magic
//! numbers across render code. Fixed sizes suit chrome (borders, footers) and
//! ratios suit content panes, but only above a usability floor: each constant
//! documents the width below which it stops working.

use crate::session::config::NarrowLayout;

/// Default width below which the home view stacks (list above preview) instead
/// of side-by-side, and the preview drops its info header for the session title
/// and status icon in the block title. `SessionConfig::narrow_breakpoint`
/// overrides it; 80 is the conventional narrow-terminal boundary, where at the
/// default list_width of 35 a side-by-side preview is 45 cols and a full-width
/// stacked one reads better. Phone widths live in this range.
pub const STACKED_BREAKPOINT: u16 = 80;

/// Minimum width the preview pane needs to render a tmux capture
/// without hash-soup wrapping. Used as the side-by-side preview floor.
pub const PREVIEW_MIN_WIDTH: u16 = 40;

/// Narrowest breakpoint with a usable side-by-side layout: a 10-column list
/// beside the [`PREVIEW_MIN_WIDTH`] preview floor. Configured breakpoints below
/// it are raised to it, so a tiny or zero setting cannot put side-by-side into
/// a width its preview cannot render in.
pub const MIN_STACKED_BREAKPOINT: u16 = PREVIEW_MIN_WIDTH + 10;

/// Width of the collapsed-sidebar strip: a bordered column wide enough
/// for the `»` expand glyph plus its two border columns. Everything else
/// goes to the preview, so the strip is intentionally minimal.
pub const COLLAPSED_STRIP_WIDTH: u16 = 3;

/// In stacked mode the list takes 1/N of vertical space.
pub const STACKED_LIST_HEIGHT_FRACTION: u16 = 3;

/// Lower bound on stacked-mode list height. Below this the list can't
/// show selection + 1 neighbor + spinner row.
pub const STACKED_LIST_HEIGHT_MIN: u16 = 5;

/// Upper bound on automatic stacked-mode list height.
pub const STACKED_LIST_HEIGHT_MAX: u16 = 12;

/// Lower bound on stacked-mode preview height. Below this the
/// selection header + 1 row of capture get clipped.
pub const STACKED_PREVIEW_MIN: u16 = 8;

/// Send-message dialog targets this percentage of viewport width.
pub const DIALOG_TARGET_PCT: u16 = 80;

/// Below this width the dialog takes the full viewport. The 26-col floor is the
/// width of the title hints; below that they disappear whatever the clamp, so
/// taking the full viewport at least preserves the message area.
pub const DIALOG_MIN_WIDTH: u16 = 26;

/// Cap on dialog width so it doesn't sprawl across wide desktops.
pub const DIALOG_MAX_WIDTH: u16 = 80;

/// The breakpoint a configured `SessionConfig::narrow_breakpoint` actually
/// applies, clamped up to [`MIN_STACKED_BREAKPOINT`].
pub fn effective_stacked_breakpoint(configured: u16) -> u16 {
    configured.max(MIN_STACKED_BREAKPOINT)
}

/// Whether `viewport_width` takes the narrow layout for a configured
/// `SessionConfig::narrow_breakpoint`. The comparison is strict, so a viewport
/// exactly at the breakpoint stays side by side.
pub fn is_narrow(viewport_width: u16, configured_breakpoint: u16) -> bool {
    viewport_width < effective_stacked_breakpoint(configured_breakpoint)
}

/// Compute send-message dialog width: the full viewport below
/// [`DIALOG_MIN_WIDTH`], otherwise [`DIALOG_TARGET_PCT`] of it clamped to
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

/// Which panes a narrow home view draws, and at what list height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrowArrangement {
    /// Only the list, full width and height.
    ListOnly,
    /// Only the preview, full width and height.
    PreviewOnly,
    /// The list above the preview at this height.
    Split(u16),
}

/// Arrange a narrow (see [`is_narrow`]) home view for `layout`.
/// `preview_active` says the preview is the pane being worked in: live-send is
/// relaying keys, a transcript is being read, or a System Health takeover is
/// showing.
pub fn narrow_arrangement(
    layout: NarrowLayout,
    main_height: u16,
    requested_list_height: Option<u16>,
    preview_active: bool,
) -> NarrowArrangement {
    match (layout, preview_active) {
        (NarrowLayout::OnePane, true) => NarrowArrangement::PreviewOnly,
        (NarrowLayout::OnePane, false) => NarrowArrangement::ListOnly,
        (NarrowLayout::BigPreview, true) => {
            NarrowArrangement::Split(stacked_list_height(main_height, Some(0)))
        }
        (NarrowLayout::BigPreview | NarrowLayout::Stacked, _) => {
            NarrowArrangement::Split(stacked_list_height(main_height, requested_list_height))
        }
    }
}

pub fn stacked_list_height(main_height: u16, requested: Option<u16>) -> u16 {
    let automatic = (main_height / STACKED_LIST_HEIGHT_FRACTION)
        .clamp(STACKED_LIST_HEIGHT_MIN, STACKED_LIST_HEIGHT_MAX);
    let maximum = main_height.saturating_sub(STACKED_PREVIEW_MIN);
    requested
        .unwrap_or(automatic)
        .clamp(STACKED_LIST_HEIGHT_MIN.min(maximum), maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_width_and_stacked_height_clamp() {
        // 80% of the viewport, capped at MAX; under MIN (soft keyboard up on
        // an iPhone) it takes the whole viewport.
        for (viewport, want) in [
            (50, 40),
            (22, 22),
            (DIALOG_MIN_WIDTH, DIALOG_MIN_WIDTH),
            (200, DIALOG_MAX_WIDTH),
        ] {
            assert_eq!(dialog_width(viewport), want, "{viewport}");
        }
        for w in DIALOG_MIN_WIDTH..=DIALOG_MAX_WIDTH * 2 {
            assert!(dialog_width(w) <= w, "dialog_width({w}) > {w}");
        }
        for (height, want) in [(10, 2), (15, 5), (30, 10), (60, STACKED_LIST_HEIGHT_MAX)] {
            assert_eq!(stacked_list_height(height, None), want, "{height}");
        }
    }

    #[test]
    fn configured_breakpoint_moves_the_narrow_boundary() {
        // Default boundary: 79 is narrow, 80 is not.
        assert!(is_narrow(79, STACKED_BREAKPOINT));
        assert!(!is_narrow(80, STACKED_BREAKPOINT));
        // A configured 100 moves it: 99 is narrow, 100 is side by side.
        assert!(is_narrow(99, 100));
        assert!(!is_narrow(100, 100));
        // Tiny and zero settings clamp to the usable floor.
        for configured in [0, 1, MIN_STACKED_BREAKPOINT - 1] {
            assert_eq!(
                effective_stacked_breakpoint(configured),
                MIN_STACKED_BREAKPOINT,
                "{configured}"
            );
        }
        assert!(is_narrow(MIN_STACKED_BREAKPOINT - 1, 0));
        assert!(!is_narrow(MIN_STACKED_BREAKPOINT, 0));
    }

    #[test]
    fn narrow_arrangement_follows_the_layout() {
        let cases = [
            (NarrowLayout::Stacked, false, NarrowArrangement::Split(10)),
            (NarrowLayout::Stacked, true, NarrowArrangement::Split(10)),
            (NarrowLayout::OnePane, false, NarrowArrangement::ListOnly),
            (NarrowLayout::OnePane, true, NarrowArrangement::PreviewOnly),
            (
                NarrowLayout::BigPreview,
                false,
                NarrowArrangement::Split(10),
            ),
            (NarrowLayout::BigPreview, true, NarrowArrangement::Split(5)),
        ];
        for (layout, preview_active, want) in cases {
            assert_eq!(
                narrow_arrangement(layout, 30, None, preview_active),
                want,
                "{layout:?}"
            );
        }
    }

    #[test]
    fn requested_stacked_height_reserves_preview_space() {
        for (available, requested, expected) in [
            (45, 20, 20),
            (45, 0, 5),
            (45, u16::MAX, 37),
            (15, 20, 7),
            (10, 20, 2),
            (8, 20, 0),
            (0, 20, 0),
        ] {
            assert_eq!(stacked_list_height(available, Some(requested)), expected);
        }
    }
}
