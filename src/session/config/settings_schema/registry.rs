//! The assembled settings schema: every section's derived descriptors in one list.

use super::FieldDescriptor;
use crate::session::config::{
    AcpConfig, AuthConfig, DiffConfig, LoggingConfig, SandboxConfig, ServeConfig, SessionConfig,
    SkillsConfig, SystemdSettings, TelemetryConfig, ThemeConfig, TmuxConfig, UpdatesConfig,
    WebConfig, WorktreeConfig,
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
    out.extend(SystemdSettings::settings_descriptors());
    out.extend(SessionConfig::settings_descriptors());
    out.extend(SoundConfig::settings_descriptors());
    out.extend(StatusHookConfig::settings_descriptors());
    out.extend(WebConfig::settings_descriptors());
    out.extend(AuthConfig::settings_descriptors());
    out.extend(ServeConfig::settings_descriptors());
    out.extend(AcpConfig::settings_descriptors());
    out.extend(DiffConfig::settings_descriptors());
    out.extend(SkillsConfig::settings_descriptors());
    out.extend(LoggingConfig::settings_descriptors());
    out
}

/// The core [`schema`] plus a virtual `plugin:<id>` section per active plugin.
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

pub fn descriptor(section: &str, field: &str) -> Option<&'static FieldDescriptor> {
    schema_ref()
        .iter()
        .find(|d| d.section == section && d.field == field)
}

/// A schema section describes every field, so an undescribed key in it is skipped or a typo.
pub fn section_in_schema(section: &str) -> bool {
    schema_ref().iter().any(|d| d.section == section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::settings_schema::{ValidationKind, WebWritePolicy, WidgetKind};

    #[test]
    fn split_terminal_settings_expose_defaults_and_validate_bounds() {
        let config: SessionConfig = toml::from_str("").unwrap();
        let defaults = serde_json::to_value(config).unwrap();
        for (field, default, min, max) in [
            ("split_terminal_height_percent", 50, 1, 99),
            ("split_terminal_min_width", 100, 0, 1000),
            ("split_terminal_min_height", 30, 0, 1000),
        ] {
            assert_eq!(defaults[field], default, "{field}");
            let d = descriptor("session", field).unwrap();
            assert!(!d.profile_overridable, "{field}");
            assert_eq!(
                d.widget,
                WidgetKind::Number {
                    min: Some(min),
                    max: Some(max),
                },
                "{field}"
            );
            for (value, valid) in [(min - 1, false), (min, true), (max, true), (max + 1, false)] {
                assert_eq!(
                    super::super::validate_value(&d.validation, &serde_json::json!(value)).is_ok(),
                    valid,
                    "{field}: {value}"
                );
            }
        }
    }

    #[test]
    fn notice_timeout_is_capped_below_the_browser_timer_limit() {
        // The web dashboard passes this value to setTimeout in milliseconds,
        // which overflows above 2^31 - 1 ms (#4475).
        let d = descriptor("acp", "notice_timeout_secs").unwrap();
        assert_eq!(
            d.validation,
            ValidationKind::RangeU64 {
                min: 1,
                max: Some(2_147_483),
            }
        );
        assert!(crate::session::config::settings_schema::validate_value(
            &d.validation,
            &serde_json::json!(2_147_483)
        )
        .is_ok());
        assert!(crate::session::config::settings_schema::validate_value(
            &d.validation,
            &serde_json::json!(2_147_484)
        )
        .is_err());
    }

    #[test]
    fn schema_has_no_duplicate_paths() {
        let schema = schema();
        let mut seen = std::collections::HashSet::new();
        for d in &schema {
            assert!(seen.insert(d.path()), "duplicate field path {}", d.path());
        }
    }

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
    fn systemd_section_is_a_global_only_schema_section() {
        use super::super::ValidationKind;
        // (field, widget, validation, advanced)
        let cases = [
            ("enabled", WidgetKind::Toggle, ValidationKind::None, false),
            (
                "slice",
                WidgetKind::Text {
                    multiline: false,
                    mono: false,
                },
                ValidationKind::SystemdSlice,
                false,
            ),
            (
                "survive_daemon_restart",
                WidgetKind::Toggle,
                ValidationKind::None,
                false,
            ),
            (
                "memory_max",
                WidgetKind::OptionalText { mono: false },
                ValidationKind::MemoryLimit,
                true,
            ),
            (
                "tasks_max",
                WidgetKind::Number {
                    min: Some(0),
                    max: Some(4194304),
                },
                ValidationKind::RangeU64 {
                    min: 0,
                    max: Some(4194304),
                },
                true,
            ),
        ];
        let fields: Vec<&str> = schema_ref()
            .iter()
            .filter(|d| d.section == "systemd")
            .map(|d| d.field.as_str())
            .collect();
        // The web tags every systemd row "Linux only" by section.
        assert_eq!(fields, cases.iter().map(|c| c.0).collect::<Vec<_>>());
        for (field, widget, validation, advanced) in cases {
            let d = descriptor("systemd", field).unwrap();
            assert_eq!(d.category, "Systemd", "{field}");
            assert_eq!(d.widget, widget, "{field}");
            assert_eq!(d.validation, validation, "{field}");
            assert_eq!(d.advanced, advanced, "{field}");
            assert!(!d.profile_overridable, "{field} is global-only");
            assert!(matches!(d.web_write, WebWritePolicy::Allow), "{field}");
        }
    }

    #[test]
    fn schema_serializes_with_tagged_widget_policy_validation() {
        // The JSON contract of the web `SettingsFieldDescriptor` type.
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
            assert!(d["web_write"].get("policy").is_some());
            assert!(d["validation"].get("rule").is_some());
        }
    }

    /// The schema row for `section.field`, as `GET /api/settings/schema` serves it.
    fn served_row(path: &str) -> serde_json::Value {
        let json = serde_json::to_value(schema()).expect("schema serializes");
        json.as_array()
            .expect("schema is a JSON array")
            .iter()
            .find(|d| {
                format!(
                    "{}.{}",
                    d["section"].as_str().unwrap(),
                    d["field"].as_str().unwrap()
                ) == path
            })
            .unwrap_or_else(|| panic!("no schema row for {path}"))
            .clone()
    }

    #[test]
    fn a_declared_summary_is_served_beside_the_description() {
        let row = served_row("session.delete_to_trash");
        assert_eq!(
            row["summary"],
            "Trashed Sessions can be restored until they're purged. Turn off to erase them immediately."
        );
        assert_eq!(row["label"], "Use Trash");
        // The doc comment stays the description, for developers and the docs site.
        assert_ne!(row["description"], row["summary"]);
    }

    #[test]
    fn a_field_without_a_summary_serializes_without_the_key() {
        let row = served_row("session.sidebar_position");
        assert!(row.as_object().unwrap().get("summary").is_none(), "{row}");
        assert_eq!(row["label"], "Sidebar position");
    }

    fn paths_where(flag: impl Fn(&FieldDescriptor) -> bool) -> Vec<String> {
        let mut paths: Vec<String> = schema_ref()
            .iter()
            .filter(|d| flag(d))
            .map(|d| d.path())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn restart_and_web_only_flags_mark_exactly_the_audited_fields() {
        // Each restart field is read once per process (a startup snapshot or a
        // OnceLock), so a saved change waits for the next start.
        let restart = [
            "acp.max_concurrent_workers",
            "logging.file_path",
            "logging.keep_count",
            "logging.max_size_mib",
            "logging.output",
            "logging.rotation",
            "logging.show_spans",
            "session.session_id_poller_max_threads",
            "systemd.enabled",
            "systemd.memory_max",
            "systemd.slice",
            "systemd.survive_daemon_restart",
            "systemd.tasks_max",
            "tmux.socket_name",
            "web.mcp_enabled",
            "web.notifications_enabled",
            "web.notify_on_error",
            "web.notify_on_idle",
            "web.notify_on_waiting",
            "web.notify_on_wake_fire",
        ];
        assert_eq!(paths_where(|d| d.restart), restart);
        assert_eq!(
            paths_where(|d| d.web_only),
            [
                "session.show_session_colors",
                "web.auto_open_diff_pane",
                "web.auto_open_plugin_panes",
                "web.auto_open_terminal_pane",
            ]
        );
        assert!(paths_where(|d| d.web_only && d.tui_only).is_empty());
        for d in schema_ref() {
            assert!(
                !d.label.to_lowercase().contains("restart req"),
                "{}: the Needs restart tag replaces the label suffix",
                d.path()
            );
        }
    }

    #[test]
    fn flags_are_served_only_when_set() {
        let flagged = served_row("logging.output");
        assert_eq!(flagged["restart"], true, "{flagged}");
        assert_eq!(flagged["label"], "Write to");
        assert_eq!(served_row("session.show_session_colors")["web_only"], true);
        let plain = served_row("session.delete_to_trash");
        let obj = plain.as_object().unwrap();
        assert!(
            !obj.contains_key("restart") && !obj.contains_key("web_only"),
            "{plain}"
        );
    }

    #[test]
    fn acp_section_fields_are_all_structured_view_settings() {
        // The web tags every acp row "Structured view only" by section. A field
        // that also changes Terminal view belongs in another section, or the
        // web rule needs an exception.
        let fields: Vec<&str> = schema_ref()
            .iter()
            .filter(|d| d.section == "acp")
            .map(|d| d.field.as_str())
            .collect();
        assert_eq!(
            fields,
            [
                "structured_view",
                "default_agent",
                "restrict_agents",
                "allowed_agents",
                "max_concurrent_workers",
                "replay_events",
                "node_path",
                "show_tool_durations",
                "wrap_tool_output",
                "compaction_reminder",
                "compaction_reminder_percent",
                "notice_display",
                "notice_timeout_secs",
                "silent_orphan_grace_secs",
                "auto_stop_idle_secs",
                "rate_limit_auto_resume",
                "rate_limit_failover",
                "rate_limit_failover_accounts",
                "allow_agent_install",
                "acp_defaults",
            ]
        );
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

    #[test]
    fn field_repo_policy_resolves_from_field_then_section() {
        use super::super::RepoPolicy;
        for (section, field, expected) in [
            ("sandbox", "extra_volumes", RepoPolicy::Deny),
            ("sandbox", "selinux_relabel", RepoPolicy::Deny),
            ("sandbox", "shared_credential", RepoPolicy::Deny),
            ("worktree", "path_template", RepoPolicy::Deny),
            ("session", "default_tool", RepoPolicy::Deny),
            ("session", "agent_detect_as", RepoPolicy::Allow),
            // Inherited from the section's repo_default.
            ("session", "yolo_mode_default", RepoPolicy::Deny),
            ("sandbox", "memory_limit", RepoPolicy::Allow),
            ("sandbox", "container_runtime", RepoPolicy::Allow),
            ("worktree", "auto_cleanup", RepoPolicy::Allow),
        ] {
            let d = descriptor(section, field).unwrap_or_else(|| panic!("{section}.{field}"));
            assert_eq!(d.repo_policy, expected, "{section}.{field}");
        }

        assert!(
            !descriptor("sandbox", "container_runtime")
                .unwrap()
                .profile_overridable
        );
    }
}
