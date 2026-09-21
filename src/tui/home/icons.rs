//! The glyphs the list view draws with.

pub(in crate::tui) const ICON_IDLE: &str = "⠒";
/// Solid dot so unread reads at a glance; matches the web sidebar.
pub(in crate::tui) const ICON_UNREAD: &str = "●";
/// How long a row must stay selected before its unread marker clears.
pub(in crate::tui) const UNREAD_DWELL: std::time::Duration = std::time::Duration::from_secs(2);
pub(in crate::tui) const ICON_ERROR: &str = "✕";
pub(in crate::tui) const ICON_UNKNOWN: &str = "⠤";
pub(in crate::tui) const ICON_STOPPED: &str = "⠒";
/// Idle-reaped structured session; distinct shape so dormancy reads without color.
pub(in crate::tui) const ICON_DORMANT: &str = "⠶";
pub(in crate::tui) const ICON_DELETING: &str = "✕";
pub(in crate::tui) const ICON_COLLAPSED: &str = "▶";
pub(in crate::tui) const ICON_EXPANDED: &str = "▼";
pub(in crate::tui) const ICON_PINNED: &str = "◆";
// Shelf glyphs stay single-width: wide glyphs break column alignment and hit-testing.
pub(in crate::tui) const ICON_TRASH_SECTION: &str = "⊘";
pub(in crate::tui) const ICON_ARCHIVED_SECTION: &str = "▤";
