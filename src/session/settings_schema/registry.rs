//! The assembled settings schema: every section's derived descriptors in one
//! flat list. Sections are added here as they migrate onto `#[derive(SettingsSection)]`.
//! This is the single list the TUI, web, and server all consume.

use super::FieldDescriptor;
use crate::session::config::{
    CockpitConfig, LoggingConfig, SandboxConfig, SessionConfig, ThemeConfig, TmuxConfig,
    UpdatesConfig, WebConfig, WorktreeConfig,
};
use crate::sound::SoundConfig;
use crate::status_hooks::StatusHookConfig;

/// All settings descriptors, in section then field order.
pub fn schema() -> Vec<FieldDescriptor> {
    let mut out = Vec::new();
    out.extend(ThemeConfig::settings_descriptors());
    out.extend(UpdatesConfig::settings_descriptors());
    out.extend(WorktreeConfig::settings_descriptors());
    out.extend(SandboxConfig::settings_descriptors());
    out.extend(TmuxConfig::settings_descriptors());
    out.extend(SessionConfig::settings_descriptors());
    out.extend(SoundConfig::settings_descriptors());
    out.extend(StatusHookConfig::settings_descriptors());
    out.extend(WebConfig::settings_descriptors());
    out.extend(CockpitConfig::settings_descriptors());
    out.extend(LoggingConfig::settings_descriptors());
    out
}

/// Look up a single field's descriptor by `section` and `field`.
pub fn descriptor(section: &str, field: &str) -> Option<FieldDescriptor> {
    schema()
        .into_iter()
        .find(|d| d.section == section && d.field == field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::settings_schema::{WebWritePolicy, WidgetKind};

    #[test]
    fn schema_has_no_duplicate_paths() {
        let schema = schema();
        let mut seen = std::collections::HashSet::new();
        for d in &schema {
            assert!(seen.insert(d.path()), "duplicate field path {}", d.path());
        }
    }

    #[test]
    fn cockpit_section_is_complete() {
        let cockpit: Vec<_> = schema()
            .into_iter()
            .filter(|d| d.section == "cockpit")
            .map(|d| d.field)
            .collect();
        // Every CockpitConfig field that is a user setting must appear.
        for expected in [
            "enabled",
            "default_for_claude",
            "default_agent",
            "max_concurrent_workers",
            "replay_events",
            "replay_bytes",
            "node_path",
            "show_tool_durations",
            "queue_drain_mode",
            "max_concurrent_resumes",
            "force_end_turn_threshold_secs",
            "silent_orphan_grace_secs",
            "silent_orphan_fast_grace_secs",
        ] {
            assert!(
                cockpit.iter().any(|f| f == expected),
                "cockpit.{expected} missing from schema"
            );
        }
    }

    #[test]
    fn cockpit_node_path_is_local_only() {
        let d = descriptor("cockpit", "node_path").expect("node_path descriptor");
        assert!(
            matches!(d.web_write, WebWritePolicy::LocalOnly { .. }),
            "node_path must stay local-only: it is a host binary execution surface"
        );
        assert!(!d.web_write.is_web_writable());
    }

    #[test]
    fn cockpit_queue_drain_is_select_with_options() {
        let d = descriptor("cockpit", "queue_drain_mode").expect("queue_drain_mode");
        match d.widget {
            WidgetKind::Select { options } => {
                let values: Vec<_> = options.iter().map(|o| o.value.as_str()).collect();
                assert_eq!(values, ["combined", "serial"]);
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn cockpit_advanced_grouping() {
        let d = descriptor("cockpit", "max_concurrent_workers").unwrap();
        assert!(d.advanced);
        let d = descriptor("cockpit", "enabled").unwrap();
        assert!(!d.advanced);
    }
}
