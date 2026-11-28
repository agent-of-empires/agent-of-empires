//! Allowlisted registry of opt-in features and accessory preferences whose adoption
//! telemetry reports.

use std::collections::BTreeMap;

use crate::session::config::UpdateCheckMode;
use crate::session::Config;

/// Reads the global, pre-profile-merge config: an install-level signal, not per-session usage.
pub fn active_features(config: &Config) -> BTreeMap<String, bool> {
    let s = &config.session;
    [
        ("worktree", config.worktree.enabled),
        ("sandbox", config.sandbox.enabled_by_default),
        (
            "auto_update",
            matches!(config.updates.update_check_mode, UpdateCheckMode::Auto),
        ),
        ("sounds", config.sound.enabled),
        ("unread_indicator", s.unread_indicator),
        ("diagnostics_pane", s.show_diagnostics_pane),
        ("tips", s.show_tips),
        ("shortcut_bar", s.show_shortcut_bar),
        ("activity_age", s.show_activity_age),
        ("structured_badge", s.show_structured_badge),
        ("strict_hotkeys", s.strict_hotkeys),
        ("mouse_capture", s.mouse_capture),
        ("host_tab_title", s.host_tab_title),
        ("smart_rename", s.smart_rename),
        ("conversation_summary", s.conversation_summary),
        ("prevent_sleep", s.prevent_sleep_when_active),
        ("auto_stop_idle", s.auto_stop_idle_secs > 0),
        ("delete_to_trash", s.delete_to_trash),
        ("confirm_before_quit", s.confirm_before_quit),
    ]
    .into_iter()
    .map(|(name, on)| (name.to_string(), on))
    .collect()
}

/// Closed-vocabulary values of layout and ordering preferences. `unset` means never chosen.
pub fn active_prefs(config: &Config) -> BTreeMap<String, String> {
    let app = &config.app_state;
    [
        ("sidebar_sort", app.sort_order.map(serde_name)),
        ("sidebar_group_by", app.group_by.map(serde_name)),
        ("row_tag", Some(serde_name(config.session.row_tag))),
        (
            "sidebar_position",
            Some(serde_name(config.session.sidebar_position)),
        ),
        (
            "narrow_layout",
            Some(serde_name(config.session.narrow_layout)),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.unwrap_or_else(|| "unset".into())))
    .collect()
}

/// The `snake_case` serde name of a unit enum variant.
fn serde_name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::SortOrder;

    #[test]
    fn reports_allowlisted_flags_from_config() {
        let mut config = Config::default();
        config.worktree.enabled = true;
        config.updates.update_check_mode = UpdateCheckMode::Auto;
        config.session.auto_stop_idle_secs = 0;

        let features = active_features(&config);
        assert_eq!(features.get("worktree"), Some(&true));
        assert_eq!(features.get("auto_update"), Some(&true));
        assert_eq!(features.get("sandbox"), Some(&false));
        assert_eq!(features.get("auto_stop_idle"), Some(&false));
    }

    #[test]
    fn prefs_report_closed_values() {
        let mut config = Config::default();
        assert_eq!(active_prefs(&config)["sidebar_sort"], "unset");

        config.app_state.sort_order = Some(SortOrder::Name);
        let prefs = active_prefs(&config);
        assert_eq!(prefs["sidebar_sort"], "name");
        assert_eq!(prefs["row_tag"], serde_name(config.session.row_tag));
    }
}
