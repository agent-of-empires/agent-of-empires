//! The glyphs and indents the list view draws with.

pub(in crate::tui) const INDENTS: [&str; 10] = [
    "",
    " ",
    "  ",
    "   ",
    "    ",
    "     ",
    "      ",
    "       ",
    "        ",
    "         ",
];

pub(in crate::tui) fn get_indent(depth: usize) -> &'static str {
    INDENTS.get(depth).copied().unwrap_or(INDENTS[9])
}

pub(in crate::tui) const ICON_IDLE: &str = "⠒";

/// Unread rows swap the muted idle braille dot for a solid filled circle so
/// the marker reads at a glance (and matches the web sidebar's unread dot).
/// Paired with bold + `theme.unread` in the row formatter.
pub(in crate::tui) const ICON_UNREAD: &str = "●";

/// How long a session row must stay selected (with the list in the foreground)
/// before its unread marker clears. Long enough that arrowing past a row to get
/// somewhere doesn't read it, short enough that pausing to read the preview
/// does. See `tick_unread_dwell`.
pub(in crate::tui) const UNREAD_DWELL: std::time::Duration = std::time::Duration::from_secs(2);

pub(in crate::tui) const ICON_ERROR: &str = "✕";

pub(in crate::tui) const ICON_UNKNOWN: &str = "⠤";

pub(in crate::tui) const ICON_STOPPED: &str = "⠒";

/// A structured-view session parked by the idle reaper (resumable). A distinct
/// double-bar braille glyph so dormancy reads by shape as well as by its dim
/// amber color, staying legible against the single-bar idle/stopped dot in
/// monochrome terminals and for colorblind users. See #2250.
pub(in crate::tui) const ICON_DORMANT: &str = "⠶";

pub(in crate::tui) const ICON_DELETING: &str = "✕";

pub(in crate::tui) const ICON_COLLAPSED: &str = "▶";

pub(in crate::tui) const ICON_EXPANDED: &str = "▼";

/// Marks a pinned project header in project view. Geometric per DESIGN.md
/// (clean readable glyphs, not emoji).
pub(in crate::tui) const ICON_PINNED: &str = "◆";

/// Type glyphs for the two synthetic bottom-shelf section headers, so they read
/// as system shelves rather than user groups. Single-width geometric glyphs per
/// DESIGN.md (emoji would break column alignment and the shelf's mouse
/// hit-testing on terminals that render them double-width or as tofu).
pub(in crate::tui) const ICON_TRASH_SECTION: &str = "⊘";

pub(in crate::tui) const ICON_ARCHIVED_SECTION: &str = "▤";
