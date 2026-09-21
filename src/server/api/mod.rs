//! HTTP REST handlers for the web dashboard, plus shared response and
//! validation helpers.

pub(super) use super::AppState;

mod acp;
mod client_log;
mod file_provenance;
mod git;
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
    attach_session_project, create_session, delete_session, delete_workspace,
    ensure_container_terminal, ensure_session, ensure_terminal, force_smart_rename,
    get_recent_projects, kill_terminal, list_sessions, paste_image, preview_volume_ignores_globs,
    read_output, rename_session, restore_session, search_sessions, send_message,
    serve_session_artifact, session_diff_file, session_diff_files, session_file, set_worktree_name,
    start_session, stop_session, summarize_session, trash_session, update_session_archive,
    update_session_color, update_session_diff_base, update_session_group,
    update_session_notifications, update_session_pin, update_session_snooze, update_session_unread,
    update_workspace_ordering, OutputQuery, SendMessageRequest,
};
pub use skills::{
    adopt_skill, create_skill, delete_skill, edit_skill, list_skills, read_skill, sync_skills,
};
// Not route handlers: used by the daemon's background loops.
pub(crate) use sessions::{
    persist_session_update, purge_expired_trash, reconcile_trashed_worktrees,
    reconcile_worktree_paths,
};
pub use system::{
    browse_filesystem, create_profile, default_profile, delete_profile, dismiss_update,
    docker_status, filesystem_home, get_about, get_cityhall_bundle, get_current_theme,
    get_profile_settings, get_resolved_theme, get_settings, get_settings_resolved,
    get_settings_schema, get_tips, get_update_status, get_web_ui_state, list_agents, list_groups,
    list_profiles, list_sounds, list_themes, mark_tip_seen, mark_volume_ignores_globs_acknowledged,
    mark_web_tour_seen, patch_web_ui_state, post_dashboard_presence, rename_profile,
    serve_sound_file, set_show_tips, system_health, update_profile_settings, update_settings,
    update_theme,
};
pub use telemetry::{
    get_telemetry_status, post_telemetry_seen, post_telemetry_structured_interaction,
    set_telemetry_consent,
};

use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};

/// JSON error body shared by the API: `{"error": code, "message": message}`.
pub(crate) fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let message: String = message.into();
    (
        status,
        axum::Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

pub(super) fn session_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "Session not found")
}

/// 404 with no `message`, used by the terminal/send endpoints.
pub(super) fn bare_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

pub(super) fn read_only_response() -> Response {
    api_error(
        StatusCode::FORBIDDEN,
        "read_only",
        "Server is in read-only mode",
    )
}

/// 403 guard for `aoe serve --read-only`.
pub(crate) fn read_only_block(state: &AppState) -> Option<Response> {
    state.read_only.then(read_only_response)
}

/// 403 for CityHall client mode. Enforced default-deny by the `cityhall_gate`
/// middleware; per-handler `cityhall_block*` calls are defense in depth.
pub(crate) fn cityhall_response() -> Response {
    api_error(
        StatusCode::FORBIDDEN,
        "cityhall_mode",
        "This action is disabled in CityHall client mode",
    )
}

pub(crate) fn cityhall_block(state: &AppState) -> Option<Response> {
    state.cityhall_mode.then(cityhall_response)
}

/// A clone of the live instance with this id.
pub(super) async fn find_instance(state: &AppState, id: &str) -> Option<crate::session::Instance> {
    state
        .instances
        .read()
        .await
        .iter()
        .find(|i| i.id == id)
        .cloned()
}

pub(super) async fn instance_exists(state: &AppState, id: &str) -> bool {
    state.instances.read().await.iter().any(|i| i.id == id)
}

/// The operator agent allowlist, loaded off the async runtime. The supervisor
/// re-checks at spawn, so this is only an early answer.
pub(crate) async fn agent_policy() -> crate::acp::agent_policy::AgentPolicy {
    tokio::task::spawn_blocking(crate::acp::agent_policy::AgentPolicy::load)
        .await
        .unwrap_or_else(|e| {
            // A panicked load task must not read as "everything is permitted".
            tracing::error!("agent policy load task failed: {e}");
            crate::acp::agent_policy::AgentPolicy::deny_all()
        })
}

/// 404 when the instance vanished between persisting a write and applying it.
pub(super) fn session_gone_after_persist() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "Session was removed while the update was being applied",
    )
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

/// Bidi override/isolate characters (category Cf, missed by `is_control`);
/// in a label they can spoof the rendered order (CVE-2021-42574).
const BIDI_CONTROL_CHARS: &[char] = &[
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', // LRE RLE PDF LRO RLO
    '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', // LRI RLI FSI PDI
];

/// Validate a display label (title, group path). Labels never reach a shell
/// (#2624), so only control and bidi characters are rejected.
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

/// Profile names are path components: ASCII alphanumerics, `-`, `_`.
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
    use super::*;

    /// Mutating handlers by source file. Each must short-circuit in read-only
    /// mode and must extract any JSON body lazily, or axum's `Json<T>` 422s on a
    /// malformed body before the read-only guard runs (#1229).
    const MUTATING_HANDLERS: &[(&str, &str, &[&str])] = &[
        (
            "sessions/create.rs",
            include_str!("sessions/create.rs"),
            &["create_session"],
        ),
        (
            "sessions/delete.rs",
            include_str!("sessions/delete.rs"),
            &["delete_session", "delete_workspace"],
        ),
        (
            "sessions/rename.rs",
            include_str!("sessions/rename.rs"),
            &[
                "rename_session",
                "set_worktree_name",
                "attach_session_project",
            ],
        ),
        (
            "sessions/send.rs",
            include_str!("sessions/send.rs"),
            &["send_message"],
        ),
        (
            "sessions/ensure.rs",
            include_str!("sessions/ensure.rs"),
            &[
                "ensure_session",
                "ensure_terminal",
                "ensure_container_terminal",
            ],
        ),
        (
            "sessions/update.rs",
            include_str!("sessions/update.rs"),
            &[
                "update_session_group",
                "update_session_notifications",
                "update_session_diff_base",
            ],
        ),
        (
            "sessions/lifecycle.rs",
            include_str!("sessions/lifecycle.rs"),
            &[
                "update_session_pin",
                "update_session_color",
                "update_session_archive",
                "update_session_snooze",
                "trash_session",
                "restore_session",
                "update_session_unread",
                "stop_session",
                "force_smart_rename",
                "start_session",
            ],
        ),
        (
            "sessions/list.rs",
            include_str!("sessions/list.rs"),
            &["update_workspace_ordering"],
        ),
        ("git.rs", include_str!("git.rs"), &["clone_repo"]),
        (
            "mcp.rs",
            include_str!("mcp.rs"),
            &["resolve_mcp_conflict", "keep_mcp_server", "drop_mcp_server"],
        ),
        (
            "log_level.rs",
            include_str!("log_level.rs"),
            &["patch_log_level"],
        ),
        (
            "projects.rs",
            include_str!("projects.rs"),
            &["create_project", "delete_project", "update_project"],
        ),
        (
            "system.rs",
            include_str!("system.rs"),
            &[
                "update_settings",
                "dismiss_update",
                "patch_web_ui_state",
                "mark_web_tour_seen",
                "mark_tip_seen",
                "set_show_tips",
                "mark_volume_ignores_globs_acknowledged",
                "create_profile",
                "delete_profile",
                "rename_profile",
                "default_profile",
                "update_profile_settings",
            ],
        ),
        (
            "acp/worker.rs",
            include_str!("acp/worker.rs"),
            &["spawn_acp", "shutdown_acp"],
        ),
        (
            "acp/prompt.rs",
            include_str!("acp/prompt.rs"),
            &[
                "acp_prompt",
                "acp_prompt_diff_comments",
                "acp_cancel",
                "acp_force_end_turn",
                "resolve_approval",
                "resolve_elicitation",
            ],
        ),
        (
            "acp/view.rs",
            include_str!("acp/view.rs"),
            &["acp_enable", "acp_disable"],
        ),
        (
            "acp/config.rs",
            include_str!("acp/config.rs"),
            &["acp_set_mode", "acp_set_config_option"],
        ),
        (
            "push.rs",
            include_str!("../push.rs"),
            &["subscribe", "unsubscribe", "test"],
        ),
        (
            "telemetry.rs",
            include_str!("telemetry.rs"),
            &[
                "set_telemetry_consent",
                "post_telemetry_seen",
                "post_telemetry_structured_interaction",
            ],
        ),
        (
            "plugins.rs",
            include_str!("plugins.rs"),
            &["invoke_plugin_action"],
        ),
    ];

    /// `(signature, body)` of `fn name(` in `source`. The body runs to the
    /// next top-level fn definition.
    fn handler_source<'a>(source: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
        let needle = format!("fn {name}(");
        let rest = &source[source.find(&needle)? + needle.len()..];
        let mut depth = 1usize;
        let sig_end = rest.char_indices().find_map(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(i)
        })?;
        let body_end = ["\npub async fn ", "\npub fn ", "\nasync fn ", "\nfn "]
            .iter()
            .filter_map(|t| rest.find(t))
            .min()
            .unwrap_or(rest.len());
        Some((&rest[..sig_end], &rest[..body_end]))
    }

    #[test]
    fn mutating_handlers_guard_read_only_before_body_extraction() {
        let mut failures = Vec::new();
        for (file, source, handlers) in MUTATING_HANDLERS {
            for name in *handlers {
                let Some((signature, body)) = handler_source(source, name) else {
                    failures.push(format!("{file}: handler `{name}` not found"));
                    continue;
                };
                if !["state.read_only", "self.read_only", "read_only_block("]
                    .iter()
                    .any(|p| body.contains(p))
                {
                    failures.push(format!("{file}: `{name}` lacks a read-only guard"));
                }
                let eager = signature.split(',').any(|arg| {
                    let arg = arg.trim_start();
                    arg.starts_with("Json(") || arg.contains(": Json<")
                });
                if eager {
                    failures.push(format!(
                        "{file}: `{name}` extracts its JSON body eagerly; use \
                         `Result<Json<T>, JsonRejection>` or `Option<Json<T>>`"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A plugin pane action mutates no host state, so it is gated on read-only
    /// mode only, never on elevation (#2454).
    #[test]
    fn plugin_action_does_not_require_elevation() {
        let (_, body) = handler_source(include_str!("plugins.rs"), "invoke_plugin_action")
            .expect("invoke_plugin_action");
        for marker in ["mutation_gate", "is_elevated", "elevation_required"] {
            assert!(!body.contains(marker), "found `{marker}`");
        }
    }

    #[test]
    fn shell_metacharacters_blocklist_is_exhaustive() {
        // Removing a character here is a security change, not a tidy-up.
        let expected: &[char] = &[
            ';', '&', '|', '$', '`', '(', ')', '{', '}', '<', '>', '\n', '\r', '\\', '"', '\'',
            '!', '#', '*', '?', '[', ']', '~', '\t', '\0',
        ];
        assert_eq!(SHELL_METACHARACTERS, expected);
        for &c in SHELL_METACHARACTERS {
            assert!(validate_no_shell_injection(&format!("prefix{c}suffix"), "field").is_err());
        }
    }

    #[test]
    fn display_label_validation() {
        // #2624: imported titles carry punctuation that is harmless in a label.
        for value in [
            "I've read @filename?",
            "What's next?",
            "Fix [draft] (wip) ~ #123",
            "work/claude/imports",
        ] {
            assert!(validate_display_label(value, "title").is_ok(), "{value:?}");
        }
        let bidi = BIDI_CONTROL_CHARS.iter().map(|c| format!("bad{c}name"));
        let control = [
            "bad\nname",
            "bad\rname",
            "bad\tname",
            "bad\u{1b}name",
            "bad\0name",
        ]
        .into_iter()
        .map(String::from);
        for value in control.chain(bidi) {
            assert!(
                validate_display_label(&value, "title").is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn profile_name_validation() {
        for bad in ["../etc", "foo/bar", "..", ".hidden", ""] {
            assert!(validate_profile_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_profile_name(&"a".repeat(65)).is_err());
        for good in ["default", "my-profile", "profile_2", "A"] {
            assert!(validate_profile_name(good).is_ok(), "{good:?}");
        }
    }
}
