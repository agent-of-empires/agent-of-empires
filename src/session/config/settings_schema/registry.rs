//! The assembled settings schema: every section's derived descriptors in one
//! flat list. Sections are added here as they migrate onto `#[derive(SettingsSection)]`.
//! This is the single list the TUI, web, and server all consume.

use super::FieldDescriptor;
use crate::session::config::{
    AcpConfig, AuthConfig, DiffConfig, LoggingConfig, SandboxConfig, SessionConfig, SkillsConfig,
    TelemetryConfig, ThemeConfig, TmuxConfig, UpdatesConfig, WebConfig, WorktreeConfig,
};
use crate::sound::SoundConfig;
use crate::status_hooks::StatusHookConfig;

/// All settings descriptors, in section then field order.
pub fn schema() -> Vec<FieldDescriptor> {
    schema_ref().to_vec()
}

/// [`schema`] without the clone, for callers that only read.
pub fn schema_ref() -> &'static [FieldDescriptor] {
    static SCHEMA: std::sync::OnceLock<Vec<FieldDescriptor>> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(build_schema)
}

fn build_schema() -> Vec<FieldDescriptor> {
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
    out.extend(SkillsConfig::settings_descriptors());
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
pub fn descriptor(section: &str, field: &str) -> Option<&'static FieldDescriptor> {
    schema_ref()
        .iter()
        .find(|d| d.section == section && d.field == field)
}

/// Whether the section is described by the settings schema at all. A section
/// that is (every `#[derive(SettingsSection)]` config struct) declares every
/// field a surface may touch, so a key it does not describe is `skip`ped or
/// misspelled rather than merely undocumented.
pub fn section_in_schema(section: &str) -> bool {
    schema_ref().iter().any(|d| d.section == section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::settings_schema::{WebWritePolicy, WidgetKind};

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
        match &d.widget {
            WidgetKind::Select { options } => {
                let values: Vec<_> = options.iter().map(|o| o.value.as_str()).collect();
                assert_eq!(values, ["none", "auto", "profile", "sandbox", "branch"]);
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    /// The poller-thread ceiling is process-wide tuning: the web shows it
    /// under the Session tab's Advanced fold with the "not
    /// profile-overridable" note, as a lower-bounded number (0 = default).
    #[test]
    fn session_id_poller_max_threads_is_an_advanced_global_only_number() {
        let d = descriptor("session", "session_id_poller_max_threads")
            .expect("session_id_poller_max_threads descriptor");
        assert!(d.advanced, "sits under the Advanced fold");
        assert!(
            !d.profile_overridable,
            "global-only: one ceiling per process"
        );
        assert!(matches!(d.web_write, WebWritePolicy::Allow));
        assert_eq!(
            d.widget,
            WidgetKind::Number {
                min: Some(0),
                max: None
            }
        );
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

    #[test]
    fn field_declared_repo_and_write_policies() {
        use super::super::{RepoPolicy, ValidationKind, WebWritePolicy};
        for (field, validation) in [
            ("privileged", ValidationKind::None),
            ("cap_add", ValidationKind::CapabilityList),
            ("cap_drop", ValidationKind::CapabilityList),
            ("security_opt", ValidationKind::SecurityOptList),
            ("extra_run_args", ValidationKind::None),
        ] {
            let d = descriptor("sandbox", field).unwrap_or_else(|| panic!("sandbox.{field}"));
            assert_eq!(d.repo_policy, RepoPolicy::Deny, "sandbox.{field}");
            assert_eq!(d.validation, validation, "sandbox.{field}");
            assert!(
                matches!(d.web_write, WebWritePolicy::LocalOnly { .. }),
                "sandbox.{field} must be local_only, got {:?}",
                d.web_write
            );
        }
    }

    /// Every descriptor carries a resolved repo policy: its own
    /// `#[setting(repo = ...)]` where it declares one, otherwise the
    /// `repo_default` of its `#[setting_section(...)]`.
    #[test]
    fn field_repo_policy_resolves_from_field_then_section() {
        use super::super::RepoPolicy;
        for (section, field, expected) in [
            // Declared on the field.
            ("sandbox", "extra_volumes", RepoPolicy::Deny),
            ("sandbox", "selinux_relabel", RepoPolicy::Deny),
            ("session", "default_tool", RepoPolicy::Deny),
            ("session", "agent_detect_as", RepoPolicy::Allow),
            // Inherited: `session` declares repo_default = "deny", every other
            // section defaults to allow.
            ("session", "yolo_mode_default", RepoPolicy::Deny),
            ("sandbox", "memory_limit", RepoPolicy::Allow),
            ("sandbox", "container_runtime", RepoPolicy::Allow),
            ("worktree", "path_template", RepoPolicy::Allow),
        ] {
            let d = descriptor(section, field).unwrap_or_else(|| panic!("{section}.{field}"));
            assert_eq!(d.repo_policy, expected, "{section}.{field}");
        }

        // A permissive policy does not make a global-only field settable; the
        // repo gate checks `profile_overridable` first.
        assert!(
            !descriptor("sandbox", "container_runtime")
                .unwrap()
                .profile_overridable
        );
    }

    #[test]
    fn section_in_schema_separates_derived_sections_from_hooks() {
        assert!(section_in_schema("session"));
        assert!(section_in_schema("sandbox"));
        assert!(section_in_schema("worktree"));
        assert!(section_in_schema("updates"));
        // `hooks` is a repo-overridable section with no `SettingsSection`
        // derive, so its keys have no descriptors and must not fail closed.
        assert!(!section_in_schema("hooks"));
    }
}
