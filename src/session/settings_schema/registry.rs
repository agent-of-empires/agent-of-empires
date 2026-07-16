//! The assembled settings schema: every section's derived descriptors in one
//! flat list. Sections are added here as they migrate onto `#[derive(SettingsSection)]`.
//! This is the single list the TUI, web, and server all consume.

use super::FieldDescriptor;
use crate::session::config::{
    AcpConfig, AuthConfig, DiffConfig, LoggingConfig, SandboxConfig, SessionConfig,
    TelemetryConfig, ThemeConfig, TmuxConfig, UpdatesConfig, WebConfig, WorktreeConfig,
};
use crate::sound::SoundConfig;
use crate::status_hooks::StatusHookConfig;

/// All settings descriptors, in section then field order.
pub fn schema() -> Vec<FieldDescriptor> {
    let mut out = Vec::new();
    out.extend(ThemeConfig::settings_descriptors());
    out.extend(UpdatesConfig::settings_descriptors());
    out.extend(TelemetryConfig::settings_descriptors());
    out.extend(WorktreeConfig::settings_descriptors());
    out.extend(SandboxConfig::settings_descriptors());
    out.extend(TmuxConfig::settings_descriptors());
    out.extend(SessionConfig::settings_descriptors());
    out.extend(SoundConfig::settings_descriptors());
    out.extend(StatusHookConfig::settings_descriptors());
    out.extend(WebConfig::settings_descriptors());
    out.extend(AuthConfig::settings_descriptors());
    out.extend(AcpConfig::settings_descriptors());
    out.extend(DiffConfig::settings_descriptors());
    out.extend(LoggingConfig::settings_descriptors());
    out
}

/// The schema as the running process sees it: the static core [`schema`] plus
/// one virtual `plugin:<id>` section per active plugin's declared settings. The
/// server serves this over `GET /api/settings/schema`, validates PATCHes against
/// it, and the TUI builds its Plugins tab from it, so plugin settings render and
/// validate through the exact same path as core settings.
pub fn runtime_schema() -> Vec<FieldDescriptor> {
    let mut out = schema();
    for p in crate::plugin::registry().active() {
        out.extend(super::plugin::plugin_field_descriptors(
            p.id(),
            &p.manifest.settings,
        ));
    }
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
    fn acp_section_is_complete() {
        let acp: Vec<_> = schema()
            .into_iter()
            .filter(|d| d.section == "acp")
            .map(|d| d.field)
            .collect();
        // Every AcpConfig field that is a user setting must appear.
        for expected in [
            "default_agent",
            "max_concurrent_workers",
            "replay_events",
            "node_path",
            "show_tool_durations",
            "silent_orphan_grace_secs",
        ] {
            assert!(
                acp.iter().any(|f| f == expected),
                "acp.{expected} missing from schema"
            );
        }
    }

    #[test]
    fn acp_node_path_is_local_only() {
        let d = descriptor("acp", "node_path").expect("node_path descriptor");
        assert!(
            matches!(d.web_write, WebWritePolicy::LocalOnly { .. }),
            "node_path must stay local-only: it is a host binary execution surface"
        );
    }

    #[test]
    fn session_row_tag_is_select_with_options() {
        let d = descriptor("session", "row_tag").expect("row_tag");
        match d.widget {
            WidgetKind::Select { options } => {
                let values: Vec<_> = options.iter().map(|o| o.value.as_str()).collect();
                assert_eq!(values, ["none", "auto", "profile", "sandbox", "branch"]);
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn schema_serializes_with_tagged_widget_policy_validation() {
        // Locks the JSON contract the web `SettingsFieldDescriptor` TS type
        // depends on (GET /api/settings/schema). Widgets are tagged `kind`,
        // write policies `policy`, validation `rule`; every descriptor carries
        // a dotted-path id via section+field.
        let json = serde_json::to_value(schema()).expect("schema serializes");
        let arr = json.as_array().expect("schema is a JSON array");
        assert!(!arr.is_empty());
        for d in arr {
            let obj = d.as_object().expect("descriptor is an object");
            for key in [
                "section",
                "field",
                "category",
                "label",
                "description",
                "widget",
                "web_write",
                "profile_overridable",
                "validation",
            ] {
                assert!(obj.contains_key(key), "descriptor missing `{key}`: {d}");
            }
            assert!(d["widget"].get("kind").is_some(), "widget not tagged: {d}");
            assert!(
                d["web_write"].get("policy").is_some(),
                "web_write not tagged: {d}"
            );
            assert!(
                d["validation"].get("rule").is_some(),
                "validation not tagged: {d}"
            );
        }
    }

    #[test]
    fn acp_advanced_grouping() {
        let d = descriptor("acp", "max_concurrent_workers").unwrap();
        assert!(d.advanced);
        let d = descriptor("acp", "default_agent").unwrap();
        assert!(!d.advanced);
    }
}
