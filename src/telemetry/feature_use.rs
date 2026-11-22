//! Closed registry of accessory-feature use counts. The TUI and the web dashboard report
//! uses to the Daemon, which folds them into its next `usage_snapshot`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use super::usage_signals::UsageSeenCounters;
use crate::session::config::{GroupByMode, SortOrder};

macro_rules! feature_uses {
    ($($variant:ident => $name:literal,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum FeatureUse { $($variant,)* }

        impl FeatureUse {
            pub const NAMES: &'static [&'static str] = &[$($name,)*];

            pub fn as_str(self) -> &'static str {
                match self { $(FeatureUse::$variant => $name,)* }
            }
        }
    };
}

feature_uses! {
    SortNewest => "sort_newest",
    SortAttention => "sort_attention",
    SortLastActivity => "sort_last_activity",
    SortOldest => "sort_oldest",
    SortAz => "sort_az",
    SortZa => "sort_za",
    SortCustom => "sort_custom",
    GroupByManual => "group_by_manual",
    GroupByProject => "group_by_project",
    GroupByOrg => "group_by_org",
    GroupByStatus => "group_by_status",
    ArchiveToggle => "archive_toggle",
    FavoriteToggle => "favorite_toggle",
    SnoozeToggle => "snooze_toggle",
    UnreadToggle => "unread_toggle",
    RowReorder => "row_reorder",
    JumpFinished => "jump_finished",
    AttentionJump => "attention_jump",
    ProjectPinToggle => "project_pin_toggle",
    HideStoppedToggle => "hide_stopped_toggle",
    PreviewInfoToggle => "preview_info_toggle",
    DiagnosticsToggle => "diagnostics_toggle",
    SplitTerminalToggle => "split_terminal_toggle",
    ContainerTerminalToggle => "container_terminal_toggle",
    SystemHealth => "system_health",
    Tips => "tips",
    Fork => "fork",
    AutoName => "auto_name",
    CommandPalette => "command_palette",
    Search => "search",
    NewFromSelection => "new_from_selection",
    TrashRestore => "trash_restore",
    TrashEmpty => "trash_empty",
}

/// Web dashboard uses with no TUI counterpart; the shared ones go through [`FeatureUse`] names.
const WEB_ONLY: &[&str] = &[
    "sort_manual",
    "sort_plugin",
    "axis_repo",
    "axis_org",
    "axis_group",
    "axis_repo_group",
    "axis_status",
    "pin_toggle",
    "session_color",
    "focus_mode",
    "summarize",
    "files_pane",
    "agents_pane",
    "cheat_code",
];

impl From<SortOrder> for FeatureUse {
    fn from(order: SortOrder) -> Self {
        match order {
            SortOrder::Newest => FeatureUse::SortNewest,
            SortOrder::Attention => FeatureUse::SortAttention,
            SortOrder::LastActivity => FeatureUse::SortLastActivity,
            SortOrder::Oldest => FeatureUse::SortOldest,
            SortOrder::AZ => FeatureUse::SortAz,
            SortOrder::ZA => FeatureUse::SortZa,
            SortOrder::Custom => FeatureUse::SortCustom,
        }
    }
}

impl From<GroupByMode> for FeatureUse {
    fn from(mode: GroupByMode) -> Self {
        match mode {
            GroupByMode::Manual => FeatureUse::GroupByManual,
            GroupByMode::Project => FeatureUse::GroupByProject,
            GroupByMode::Org => FeatureUse::GroupByOrg,
            GroupByMode::Status => FeatureUse::GroupByStatus,
        }
    }
}

static COUNTS: LazyLock<UsageSeenCounters> = LazyLock::new(|| {
    UsageSeenCounters::with_names(FeatureUse::NAMES.iter().chain(WEB_ONLY).copied())
});

/// For names a client reports to the Daemon; false when off the registry.
pub fn record_name(name: &str) -> bool {
    COUNTS.record(name)
}

pub fn snapshot() -> BTreeMap<String, u32> {
    COUNTS.snapshot()
}

pub(super) fn zero() {
    COUNTS.zero();
}

/// Called only after a confirmed send, so uses recorded mid-send survive.
pub fn clear_reported(reported: &BTreeMap<String, u32>) {
    COUNTS.decrement(reported);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_snake_case_and_match_variants() {
        let mut seen = std::collections::BTreeSet::new();
        for name in FeatureUse::NAMES.iter().chain(WEB_ONLY) {
            assert!(seen.insert(*name), "duplicate {name}");
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
        }
        assert_eq!(
            FeatureUse::from(SortOrder::Attention).as_str(),
            "sort_attention"
        );
        assert_eq!(
            FeatureUse::from(GroupByMode::Status).as_str(),
            "group_by_status"
        );
    }
}
