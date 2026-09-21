//! REST handlers and shared validation for daemon and dashboard clients.

pub(super) use super::AppState;

mod acp;
mod client_log;
mod file_provenance;
mod git;
mod groups;
mod log_level;
mod mcp;
pub(crate) mod plugin_settings;
pub mod plugins;
mod projects;
mod queue;
pub(crate) mod sessions;
mod skills;
pub(crate) mod system;
mod telemetry;

pub(crate) use acp::structured_spawn_error_message;
pub use acp::{
    acp_attachment, acp_cancel, acp_context_primer, acp_disable, acp_enable, acp_files,
    acp_force_end_turn, acp_prompt, acp_prompt_diff_comments, acp_replay, acp_set_config_option,
    acp_set_mode, acp_worker_log, get_option_catalog, install_agent, list_acp_agents,
    list_claude_sessions, resolve_approval, resolve_elicitation, shutdown_acp, spawn_acp,
    switch_acp_agent,
};

pub use queue::{queue_clear, queue_edit, queue_enqueue, queue_list, queue_remove};

pub use client_log::post_client_log;
pub use git::{clone_repo, is_git_repo, list_branches};
pub use groups::{collapse_group, create_group, delete_group, move_group};
pub use log_level::{get_log_level, patch_log_level};
pub use mcp::{drop_mcp_server, get_mcp_servers, keep_mcp_server, resolve_mcp_conflict};
pub use plugin_settings::resolve_options;
pub use plugins::{
    apply_plugin_update, dismiss_plugin_update, invoke_plugin_action, invoke_plugin_command,
    list_plugins, plugin_commands, plugin_details, plugin_discover, plugin_job_status,
    plugin_ui_state, plugin_update_preview, plugin_updates, preview_plugin_install,
    restart_plugin_worker, serve_plugin_icon, set_plugin_enabled, start_plugin_install,
    start_plugin_uninstall,
};
pub use projects::{create_project, delete_project, list_projects, update_project};
pub use sessions::{
    abandon_purge, attach_session_project, cancel_creation, create_session, delete_session,
    delete_workspace, ensure_container_terminal, ensure_session, ensure_terminal, ensure_tool,
    force_smart_rename, get_recent_projects, kill_terminal, list_sessions, paste_image,
    preview_volume_ignores_globs, read_output, rename_session, restart_session, restore_session,
    review_creation_trust, search_sessions, send_message, serve_session_artifact,
    session_diff_file, session_diff_files, session_file, set_worktree_name, start_session,
    stop_auxiliary, stop_session, summarize_session, trash_session, update_session_archive,
    update_session_color, update_session_diff_base, update_session_favorite, update_session_group,
    update_session_notifications, update_session_pin, update_session_snooze, update_session_unread,
    update_workspace_ordering, OutputQuery, SendMessageRequest,
};
pub(crate) use sessions::{
    lifecycle_rejection, persist_session_update, purge_expired_trash, reconcile_trashed_worktrees,
    reconcile_worktree_paths,
};
pub use skills::{
    adopt_skill, create_skill, delete_skill, edit_skill, list_skills, read_skill, sync_skills,
};
pub use system::{
    browse_filesystem, create_profile, default_profile, delete_profile, dismiss_update,
    docker_status, filesystem_home, get_about, get_agent_hooks_acknowledgement,
    get_cityhall_bundle, get_current_theme, get_profile_settings, get_resolved_theme, get_settings,
    get_settings_resolved, get_settings_schema, get_tips, get_update_status, get_web_ui_state,
    list_agents, list_groups, list_profiles, list_sounds, list_themes,
    mark_agent_hooks_acknowledged, mark_tip_seen, mark_volume_ignores_globs_acknowledged,
    mark_web_tour_seen, patch_web_ui_state, post_dashboard_presence, rename_profile,
    serve_sound_file, set_show_tips, system_health, update_profile_settings, update_settings,
    update_theme,
};
pub use telemetry::{
    get_telemetry_status, post_telemetry_seen, post_telemetry_structured_interaction,
    set_telemetry_consent,
};

/// Canonical 404 for a session id that does not resolve to a live instance.
/// Body shape (`error` discriminator + human `message`) matches the rest of
/// the JSON error surface so the dashboard's generic `.message` handling and
/// `.error` discrimination both keep working.
pub(super) fn session_not_found() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not_found", "message": "Session not found" })),
    )
        .into_response()
}

/// Canonical 403 body for `aoe serve --read-only`.
pub(super) fn read_only_response() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::FORBIDDEN,
        crate::daemon::ApiErrorCode::ReadOnly.header(),
        axum::Json(serde_json::json!({
            "error": "read_only",
            "message": "Server is in read-only mode"
        })),
    )
        .into_response()
}

/// Canonical 403 body for CityHall client mode (`AOE_CITYHALL_MODE`). Terminal
/// (keystrokes + raw pane/output reads), diff, project management, agent/worker
/// lifecycle + config, git clone/probe, and uncurated settings/profile writes
/// are all closed here, not only by hiding the UI: the create path also strips
/// every client-controlled spawn field, and the curated settings/theme writes
/// are field-filtered. Reachability is enforced default-deny by the
/// `cityhall_gate` middleware against the `CITYHALL_MUTATION_ALLOW` table (with
/// the per-handler `cityhall_block*` calls kept as defense in depth); the
/// `every_mutating_route_is_cityhall_classified` audit and the
/// `serve_cityhall_lockdown` route tests keep the contract honest. See #7.
pub(crate) fn cityhall_response() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::FORBIDDEN,
        crate::daemon::ApiErrorCode::CityhallMode.header(),
        axum::Json(serde_json::json!({
            "error": "cityhall_mode",
            "message": "This action is disabled in CityHall client mode"
        })),
    )
        .into_response()
}

/// 403 guard for CityHall client mode, mirroring `read_only_block`. Callers do
/// `if let Some(resp) = cityhall_block(&state) { return resp; }`.
pub(crate) fn cityhall_block(state: &AppState) -> Option<axum::response::Response> {
    state.cityhall_mode.then(cityhall_response)
}

/// The operator agent allowlist, read off the async runtime because it touches
/// disk. Handlers use it to answer up front instead of letting a disallowed
/// agent fail at spawn time, which is the complaint #3241 opens with.
///
/// One load per request. Two requests can observe different policies if the
/// operator edits it in between, which is fine: each response is internally
/// consistent, and the supervisor re-checks at spawn regardless, so a handler
/// preflight is never the thing standing between a disallowed agent and a
/// process.
pub(crate) async fn agent_policy() -> crate::acp::agent_policy::AgentPolicy {
    tokio::task::spawn_blocking(crate::acp::agent_policy::AgentPolicy::load)
        .await
        .unwrap_or_else(|e| {
            // A panicked load task must not read as "everything is permitted".
            tracing::error!("agent policy load task failed: {e}");
            crate::acp::agent_policy::AgentPolicy::deny_all()
        })
}

/// 404 for the persist-then-apply race: the write was persisted to disk, but
/// the in-memory instance was concurrently removed before the apply step.
/// This is a caller-visible "session no longer exists", not a persist
/// failure, so it must not surface as a 500.
pub(super) fn session_gone_after_persist() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not_found",
            "message": "Session was removed while the update was being applied"
        })),
    )
        .into_response()
}

const SHELL_METACHARACTERS: &[char] = &[
    ';', '&', '|', '$', '`', '(', ')', '{', '}', '<', '>', '\n', '\r', '\\', '"', '\'', '!', '#',
    '*', '?', '[', ']', '~', '\t', '\0',
];

pub(super) fn validate_no_shell_injection(value: &str, field_name: &str) -> Result<(), String> {
    if let Some(c) = value.chars().find(|c| SHELL_METACHARACTERS.contains(c)) {
        return Err(format!(
            "Invalid character '{}' in {}. Shell metacharacters are not allowed.",
            c, field_name
        ));
    }
    Ok(())
}

/// Unicode bidirectional-format characters (category Cf): `char::is_control()`
/// only covers Cc, so these pass through unblocked otherwise. Left in a
/// display label, they let the rendered text reorder relative to what's
/// stored (Trojan-Source-style spoofing, e.g. CVE-2021-42574) in shared UI
/// surfaces. This is the same set rustc's own bidi lint blocks in source
/// literals.
const BIDI_CONTROL_CHARS: &[char] = &[
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', // LRE RLE PDF LRO RLO
    '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', // LRI RLI FSI PDI
];

/// Validate a pure display label (session title, group path): these are
/// never passed to a shell or interpreted as a path (#2624), so unlike
/// `validate_no_shell_injection` this allows apostrophes, punctuation, and
/// most metacharacters. It still rejects control characters and bidi
/// override/isolate characters, since a literal newline, NUL, or bidi
/// override corrupts single-line UI rendering, storage, or the displayed
/// text's actual order regardless of shell context.
pub(super) fn validate_display_label(value: &str, field_name: &str) -> Result<(), String> {
    if let Some(c) = value
        .chars()
        .find(|c| c.is_control() || BIDI_CONTROL_CHARS.contains(c))
    {
        return Err(format!(
            "Invalid control character U+{:04X} in {}.",
            c as u32, field_name
        ));
    }
    Ok(())
}

// The settings PATCH write surface (which sections/fields the web may write,
// which need elevation, which are host-only) is no longer a hand-kept list
// here: it is derived from the settings schema in
// `crate::session::config::settings_schema::policy`, the single source of truth shared
// with the TUI and web (#1692). See `update_settings` / `update_profile_settings`
// in `system.rs`, which validate each PATCH leaf via `validate_patch`.

/// Validate that a profile name contains only safe characters.
/// Rejects path traversal attempts (../, /) and shell metacharacters.
pub(super) fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Profile name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err("Profile name must be 64 characters or fewer".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(
            "Profile name must contain only letters, digits, hyphens, and underscores".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Regression tests that pin security-critical helpers.
    //!
    //! `SHELL_METACHARACTERS` was silently rewritten in an earlier hand-
    //! assembled version of this split: a refactor PR that claimed "no
    //! behavior changes" dropped 4 shell metacharacters (`#`, `[`, `]`,
    //! `~`) from the injection blocklist. Pin its contents here so the
    //! next refactor that touches this file fails CI instead of silently
    //! regressing security.
    //!
    //! The settings PATCH write surface (allowed sections, blocked agent-
    //! command fields, elevation surfaces) is no longer a constant here:
    //! it is derived from the settings schema and pinned by the tests in
    //! `crate::session::config::settings_schema::policy` (#1692).
    use super::*;

    /// CityHall lockdown (#7): the shared guard returns 403 so terminal, diff,
    /// project-management, and advanced-settings endpoints are unreachable in
    /// CityHall client mode, not merely hidden in the UI.
    #[test]
    fn cityhall_response_is_forbidden() {
        assert_eq!(
            cityhall_response().status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }

    /// A plugin pane action is forwarded to the worker (the trust boundary)
    /// and mutates no host-managed state, so it is gated on read-write mode
    /// only, never on passphrase elevation (#2454). This static check guards
    /// against a refactor re-introducing the elevation gate on the action
    /// path and re-breaking the refresh button under login. Same body-boundary
    /// walk as `every_mutating_handler_has_read_only_guard`.
    #[test]
    fn plugin_action_does_not_require_elevation() {
        let source = include_str!("plugins.rs");
        let needle = "fn invoke_plugin_action(";
        let start = source
            .find(needle)
            .expect("handler `invoke_plugin_action` not found (rename/refactor?)");
        let rest = &source[start + needle.len()..];
        let body_terminators: &[&str] = &["\npub async fn ", "\npub fn ", "\nasync fn ", "\nfn "];
        let end = body_terminators
            .iter()
            .filter_map(|t| rest.find(t))
            .min()
            .unwrap_or(rest.len());
        let body = &rest[..end];
        // `mutation_gate` bundles the elevation check; `is_elevated` /
        // `elevation_required` would mean elevation was reintroduced inline.
        for marker in ["mutation_gate", "is_elevated", "elevation_required"] {
            assert!(
                !body.contains(marker),
                "invoke_plugin_action must not elevation-gate (found `{marker}`). \
                 A pane action mutates no host state; keep the read-only gate only. \
                 If an action ever needs elevation, make it opt-in per action (#2454)."
            );
        }
    }

    #[test]
    fn shell_metacharacters_blocklist_is_exhaustive() {
        // Every character here has a documented shell-injection vector when
        // interpolated into a command line. Removing a character from this
        // list without removing the corresponding regression below is a
        // security change that must be reviewed on its own, not smuggled
        // through a refactor.
        let expected: &[char] = &[
            ';', '&', '|', '$', '`', '(', ')', '{', '}', '<', '>', '\n', '\r', '\\', '"', '\'',
            '!', '#', '*', '?', '[', ']', '~', '\t', '\0',
        ];
        assert_eq!(
            SHELL_METACHARACTERS.len(),
            expected.len(),
            "SHELL_METACHARACTERS size changed; every addition/removal must be \
             reviewed as a security change, not a refactor tidy-up"
        );
        for c in expected {
            assert!(
                SHELL_METACHARACTERS.contains(c),
                "SHELL_METACHARACTERS lost character {:?}. Each character blocks \
                 a specific shell-injection vector: # starts a comment, [ ] are \
                 glob metacharacters, ~ triggers tilde expansion, etc. If the \
                 intent is to actually stop blocking this character, update both \
                 this test and the list in the same commit with justification.",
                c
            );
        }
    }

    #[test]
    fn validate_no_shell_injection_rejects_every_metacharacter() {
        for &c in SHELL_METACHARACTERS {
            let input = format!("prefix{}suffix", c);
            let result = validate_no_shell_injection(&input, "field");
            assert!(
                result.is_err(),
                "validate_no_shell_injection should reject {:?} but accepted {:?}",
                c,
                input
            );
        }
    }

    /// #2624: real-world session titles/groups (imported from Claude Code
    /// summaries) routinely contain apostrophes, question marks, and other
    /// shell metacharacters that are harmless for a display label.
    #[test]
    fn display_label_accepts_common_punctuation() {
        for value in [
            "I've read @filename?",
            "I'm testing this out",
            "Goal: fix the parser",
            "What's next?",
            "Fix [draft] (wip) ~ #123",
            "work/claude/imports",
        ] {
            assert!(
                validate_display_label(value, "title").is_ok(),
                "should accept {:?}",
                value
            );
        }
    }

    #[test]
    fn display_label_rejects_control_characters() {
        for value in [
            "bad\nname",
            "bad\rname",
            "bad\tname",
            "bad\u{1b}name",
            "bad\0name",
        ] {
            assert!(
                validate_display_label(value, "title").is_err(),
                "should reject {:?}",
                value
            );
        }
    }

    /// `is_control()` alone misses Cf-category bidi override/isolate chars;
    /// unblocked, they let a title's rendered order differ from what's
    /// stored (Trojan-Source-style spoofing).
    #[test]
    fn display_label_rejects_bidi_control_characters() {
        for &c in BIDI_CONTROL_CHARS {
            let value = format!("bad{}name", c);
            assert!(
                validate_display_label(&value, "title").is_err(),
                "should reject {:?}",
                value
            );
        }
    }

    // The settings PATCH write-surface pins (allowed sections, blocked session
    // fields, elevation surfaces) moved to
    // `crate::session::config::settings_schema::policy` when the curated constants were
    // replaced by schema-derived `validate_patch` (#1692). The security
    // invariants (hooks never writable, agent-command fields denied,
    // sandbox/worktree require elevation) are pinned by that module's tests.

    #[test]
    fn profile_name_rejects_path_traversal() {
        assert!(validate_profile_name("../etc").is_err());
        assert!(validate_profile_name("foo/bar").is_err());
        assert!(validate_profile_name("..").is_err());
        assert!(validate_profile_name(".hidden").is_err());
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn profile_name_accepts_valid_names() {
        assert!(validate_profile_name("default").is_ok());
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("my-profile").is_ok());
        assert!(validate_profile_name("profile_2").is_ok());
        assert!(validate_profile_name("A").is_ok());
    }
}
