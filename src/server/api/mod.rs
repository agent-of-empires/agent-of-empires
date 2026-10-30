//! REST handlers and shared validation for daemon and dashboard clients.

pub(super) use super::AppState;

use axum::{http::StatusCode, response::Response};
mod acp;
mod client_log;
mod file_provenance;
mod git;
mod groups;
mod log_level;
mod logs;
mod mcp;
mod mutation_guard;
pub(crate) mod plugin_settings;
pub mod plugins;
mod projects;
mod queue;
mod sandbox_image;
pub(crate) mod sessions;
mod skills;
mod suggestions;
pub(crate) mod system;
mod telemetry;

pub(crate) use acp::structured_spawn_error_message;
pub use acp::{
    acp_attachment, acp_cancel, acp_context_primer, acp_disable, acp_enable, acp_files,
    acp_force_end_turn, acp_prompt, acp_prompt_diff_comments, acp_replay, acp_set_config_option,
    acp_set_mode, acp_worker_log, agent_install_info, get_option_catalog, install_agent,
    list_acp_agents, list_importable_sessions, resolve_approval, resolve_elicitation, shutdown_acp,
    spawn_acp, switch_acp_agent, switch_acp_provider,
};
pub(crate) use acp::{
    publish_startup_error_if_current, recover_pending_terminal_handoffs, HandoffRecovery,
};

pub use queue::{queue_clear, queue_edit, queue_enqueue, queue_list, queue_remove};

pub use client_log::post_client_log;
pub use git::{clone_repo, is_git_repo, list_branches, list_worktrees};
pub use groups::{collapse_group, create_group, delete_group, move_group, reorder};
pub use log_level::{get_log_level, patch_log_level};
pub use logs::get_logs;
pub use mcp::{
    create_mcp_server, delete_mcp_server, drop_mcp_server, edit_mcp_server, get_mcp_servers,
    keep_mcp_server, resolve_mcp_conflict,
};
pub use plugin_settings::resolve_options;
pub use plugins::{
    apply_plugin_update, dismiss_plugin_update, invoke_plugin_action, invoke_plugin_command,
    list_plugins, mint_plugin_page_ticket, plugin_commands, plugin_details, plugin_discover,
    plugin_job_status, plugin_sessions, plugin_ui_state, plugin_update_preview, plugin_updates,
    preview_plugin_install, restart_plugin_worker, serve_plugin_icon, serve_plugin_page,
    set_plugin_enabled, start_plugin_install, start_plugin_uninstall,
};
pub use projects::{create_project, delete_project, list_projects, update_project};
pub use sandbox_image::{sandbox_image_dismiss, sandbox_image_pull, sandbox_image_status};
pub use sessions::{
    abandon_purge, attach_session_project, cancel_creation, create_session,
    create_session_progress, delete_session, delete_workspace, empty_trash,
    ensure_container_terminal, ensure_session, ensure_terminal, ensure_tool, force_smart_rename,
    get_recent_projects, kill_terminal, list_sessions, paste_image, preview_volume_ignores_globs,
    read_output, recreate_container, rename_session, restart_for_update, restart_session,
    restore_session, review_creation_trust, search_sessions, send_message, serve_session_artifact,
    session_diff_file, session_diff_file_raw, session_diff_files, session_file, session_file_raw,
    set_worktree_name, start_session, stop_auxiliary, stop_session, summarize_session,
    touch_session_access, trash_session, update_session_archive, update_session_color,
    update_session_diff_base, update_session_favorite, update_session_group,
    update_session_notifications, update_session_pin, update_session_snooze, update_session_unread,
    update_workspace_ordering, OutputQuery, SendMessageRequest,
};
pub(crate) use sessions::{
    lifecycle_rejection, persist_session_update, purge_expired_trash, reconcile_trashed_worktrees,
    reconcile_worktree_paths, trash_sweep_interval,
};
pub use skills::{
    adopt_skill, create_skill, delete_skill, edit_skill, list_skills, read_skill, sync_skills,
};
pub use suggestions::{
    delete_suggestion_mapping, delete_suggestion_template, dismiss_suggestion,
    get_suggestion_templates, get_suggestions, list_suggestion_mappings, patch_suggestion_prefs,
    prepare_suggestion, put_suggestion_mapping, put_suggestion_template, refresh_suggestions,
    run_suggestion_checks, undismiss_suggestion, update_suggestion_source,
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

/// A clone of the live instance with this id.
pub(crate) async fn find_instance(
    state: &std::sync::Arc<AppState>,
    id: &str,
) -> Option<crate::session::Instance> {
    state
        .instances
        .read()
        .await
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
}

pub(crate) async fn instance_exists(state: &std::sync::Arc<AppState>, id: &str) -> bool {
    state
        .instances
        .read()
        .await
        .iter()
        .any(|instance| instance.id == id)
}

/// The stored row, which can differ from the daemon's cache when a peer such as the CLI
/// archived, trashed, or purged it.
pub(crate) async fn load_persisted_instance(
    state: &AppState,
    profile: &str,
    id: &str,
) -> Result<Option<crate::session::Instance>, axum::response::Response> {
    let id_for_load = id.to_string();
    let profile_for_load = profile.to_string();
    let file_watch = state.file_watch.clone();
    let persisted = tokio::task::spawn_blocking(
        move || -> anyhow::Result<Option<crate::session::Instance>> {
            let storage = crate::session::Storage::new(&profile_for_load, file_watch)?;
            Ok(storage
                .load()?
                .into_iter()
                .find(|candidate| candidate.id == id_for_load))
        },
    )
    .await;
    let error = match persisted {
        Ok(Ok(found)) => return Ok(found),
        Ok(Err(error)) => format!("{error:#}"),
        Err(join_error) => join_error.to_string(),
    };
    tracing::error!(target: "http.api", session = %id, "load persisted session: {error}");
    Err(api_error(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "failed to read session state",
    ))
}

/// A 404 for a session that is gone, without the daemon's own envelope: the
/// callers answer from a store read that found nothing, not from a refusal.
pub(super) fn bare_not_found() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

/// 409 for a start or resume refused because the session is archived or trashed.
pub(crate) fn start_blocked_response(
    blocked: crate::session::StartBlocked,
) -> axum::response::Response {
    use crate::daemon::ApiErrorCode;
    let code = match blocked {
        crate::session::StartBlocked::Archived => ApiErrorCode::SessionArchived,
        crate::session::StartBlocked::Trashed => ApiErrorCode::SessionTrashed,
    };
    api_error_with_code_header(code, blocked.to_string())
}

pub(crate) fn api_error(
    status: axum::http::StatusCode,
    code: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        status,
        axum::Json(serde_json::json!({ "error": code, "message": message.into() })),
    )
        .into_response()
}

/// [`api_error`] for a caller that switches on the code header: the header
/// stays, and the body carries the same code and an actionable message, so a
/// client that only reads the body still learns the cause.
pub(crate) fn api_error_with_code_header(
    code: crate::daemon::ApiErrorCode,
    message: impl Into<String>,
) -> axum::response::Response {
    let mut response = api_error(code.status(), code.as_str(), message);
    let (name, value) = code.header()[0];
    if let Ok(value) = axum::http::HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
    response
}

pub(crate) fn read_only_block(state: &AppState) -> Option<axum::response::Response> {
    state.read_only.then(read_only_response)
}

pub(crate) async fn read_only_gate(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !request.method().is_safe() {
        if let Some(response) = read_only_block(&state) {
            return response;
        }
    }
    next.run(request).await
}

/// Canonical 404 for a session id that does not resolve to a live instance.
/// Body shape (`error` discriminator + human `message`) matches the rest of
/// the JSON error surface so the dashboard's generic `.message` handling and
/// `.error` discrimination both keep working.
pub(super) fn session_not_found() -> axum::response::Response {
    api_error(
        axum::http::StatusCode::NOT_FOUND,
        "not_found",
        "Session not found",
    )
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

/// CityHall rejects unlisted mutations and field-filters its curated writes.
/// Request-level coverage lives in `serve_cityhall_lockdown`.
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

/// The error response when `name` is not an existing profile. Every profile is a real directory
/// under profiles/; an enumeration failure is a 500, not a client 400.
pub(crate) fn unknown_profile_response(name: &str) -> Option<Response> {
    let known = match crate::session::list_profiles() {
        Ok(list) => list,
        Err(e) => {
            tracing::error!(
                target: "server.sessions",
                "failed to enumerate profiles while validating a profile: {e:#}"
            );
            return Some(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("Failed to enumerate profiles: {e}"),
            ));
        }
    };
    (!known.iter().any(|p| p == name)).then(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "profile_not_found",
            format!("Profile '{name}' does not exist"),
        )
    })
}

/// 404 for the persist-then-apply race: the write was persisted to disk, but
/// the in-memory instance was concurrently removed before the apply step.
/// This is a caller-visible "session no longer exists", not a persist
/// failure, so it must not surface as a 500.
pub(super) fn session_gone_after_persist() -> axum::response::Response {
    api_error(
        axum::http::StatusCode::NOT_FOUND,
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
    use super::*;

    // Write handlers and their existing rejection/extractor boundaries.
    const MUTATING_HANDLERS: &[(&str, &str, &[&str])] = &[
        (
            "sessions/create.rs",
            include_str!("sessions/create.rs"),
            &["cancel_creation", "create_session", "review_creation_trust"],
        ),
        (
            "sessions/delete.rs",
            include_str!("sessions/delete.rs"),
            &[
                "abandon_purge",
                "delete_session",
                "delete_workspace",
                "empty_trash",
            ],
        ),
        (
            "sessions/rename.rs",
            include_str!("sessions/rename.rs"),
            &[
                "attach_session_project",
                "rename_session",
                "set_worktree_name",
            ],
        ),
        (
            "sessions/send.rs",
            include_str!("sessions/send.rs"),
            &["paste_image", "send_message"],
        ),
        (
            "sessions/ensure.rs",
            include_str!("sessions/ensure.rs"),
            &[
                "ensure_container_terminal",
                "ensure_session",
                "ensure_terminal",
                "ensure_tool",
                "kill_terminal",
                "stop_auxiliary",
            ],
        ),
        (
            "sessions/update.rs",
            include_str!("sessions/update.rs"),
            &[
                "update_session_diff_base",
                "update_session_group",
                "update_session_notifications",
            ],
        ),
        (
            "sessions/lifecycle.rs",
            include_str!("sessions/lifecycle.rs"),
            &[
                "force_smart_rename",
                "recreate_container",
                "restart_for_update",
                "restart_session",
                "restore_session",
                "start_session",
                "stop_session",
                "summarize_session",
                "touch_session_access",
                "trash_session",
                "update_session_archive",
                "update_session_color",
                "update_session_favorite",
                "update_session_pin",
                "update_session_snooze",
                "update_session_unread",
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
            &[
                "create_mcp_server",
                "delete_mcp_server",
                "drop_mcp_server",
                "edit_mcp_server",
                "keep_mcp_server",
                "resolve_mcp_conflict",
            ],
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
            "sandbox_image.rs",
            include_str!("sandbox_image.rs"),
            &["sandbox_image_pull", "sandbox_image_dismiss"],
        ),
        (
            "system.rs",
            include_str!("system.rs"),
            &[
                "create_profile",
                "default_profile",
                "delete_profile",
                "dismiss_update",
                "mark_tip_seen",
                "mark_volume_ignores_globs_acknowledged",
                "mark_web_tour_seen",
                "patch_web_ui_state",
                "rename_profile",
                "set_show_tips",
                "update_profile_settings",
                "update_settings",
                "update_theme",
            ],
        ),
        (
            "acp/worker.rs",
            include_str!("acp/worker.rs"),
            &[
                "shutdown_acp",
                "spawn_acp",
                "switch_acp_agent",
                "switch_acp_provider",
            ],
        ),
        (
            "acp/prompt.rs",
            include_str!("acp/prompt.rs"),
            &[
                "acp_cancel",
                "acp_force_end_turn",
                "acp_prompt",
                "acp_prompt_diff_comments",
                "resolve_approval",
                "resolve_elicitation",
            ],
        ),
        (
            "acp/view.rs",
            include_str!("acp/view.rs"),
            &["acp_disable", "acp_enable"],
        ),
        (
            "acp/config.rs",
            include_str!("acp/config.rs"),
            &["acp_set_config_option", "acp_set_mode"],
        ),
        (
            "push.rs",
            include_str!("../push.rs"),
            &["push::subscribe", "push::test", "push::unsubscribe"],
        ),
        (
            "telemetry.rs",
            include_str!("telemetry.rs"),
            &[
                "post_telemetry_seen",
                "post_telemetry_structured_interaction",
                "set_telemetry_consent",
            ],
        ),
        (
            "plugins.rs",
            include_str!("plugins.rs"),
            &[
                "apply_plugin_update",
                "dismiss_plugin_update",
                "invoke_plugin_action",
                "invoke_plugin_command",
                "preview_plugin_install",
                "restart_plugin_worker",
                "set_plugin_enabled",
                "start_plugin_install",
                "start_plugin_uninstall",
            ],
        ),
        (
            "suggestions.rs",
            include_str!("suggestions.rs"),
            &[
                "delete_suggestion_mapping",
                "delete_suggestion_template",
                "dismiss_suggestion",
                "patch_suggestion_prefs",
                "put_suggestion_mapping",
                "put_suggestion_template",
                "refresh_suggestions",
                "run_suggestion_checks",
                "undismiss_suggestion",
                "update_suggestion_source",
            ],
        ),
        (
            "skills.rs",
            include_str!("skills.rs"),
            &[
                "adopt_skill",
                "create_skill",
                "delete_skill",
                "edit_skill",
                "sync_skills",
            ],
        ),
        (
            "groups.rs",
            include_str!("groups.rs"),
            &[
                "collapse_group",
                "create_group",
                "delete_group",
                "move_group",
                "reorder",
            ],
        ),
        (
            "queue.rs",
            include_str!("queue.rs"),
            &["queue_clear", "queue_edit", "queue_enqueue", "queue_remove"],
        ),
        (
            "acp/install.rs",
            include_str!("acp/install.rs"),
            &["install_agent"],
        ),
    ];

    const MUTATION_AUDIT_HELPERS: &[(&str, &str, &[&str])] = &[
        (
            "sessions/lifecycle.rs",
            include_str!("sessions/lifecycle.rs"),
            &["prepare_agent_session", "prepare_agent_session_inner"],
        ),
        (
            "acp/install.rs",
            include_str!("acp/install.rs"),
            &["install_gate"],
        ),
    ];

    // Exact exceptions to the read-only handler audit. Authentication and
    // transport-only requests retain their own policy instead of a write guard.
    const MUTATION_AUDIT_EXEMPTIONS: &[(&str, &str, &str, &str)] = &[
        (
            "POST",
            "/api/presence",
            "api::post_dashboard_presence",
            "Ephemeral browser foreground heartbeat.",
        ),
        (
            "POST",
            "/api/plugins/{id}/pages/{page_id}/ticket",
            "api::mint_plugin_page_ticket",
            "Scoped authentication ticket for displaying a plugin page.",
        ),
        (
            "POST",
            "/api/client-log",
            "api::post_client_log",
            "Bounded browser diagnostic relay.",
        ),
        (
            "POST",
            "/api/plugins/{id}/settings/options/resolve",
            "api::resolve_options",
            "UI option discovery may run capability probes and populate the shared catalog.",
        ),
        (
            "POST",
            "/api/suggestions/prepare",
            "api::prepare_suggestion",
            "Resolves a wizard prefill; mutates nothing.",
        ),
        (
            "POST",
            "/api/login",
            "login::login_handler",
            "Authentication remains available in read-only mode.",
        ),
        (
            "POST",
            "/api/login/elevate",
            "login::elevate_handler",
            "Authentication elevation retains its existing policy.",
        ),
        (
            "POST",
            "/api/logout",
            "login::logout_handler",
            "Signing out remains available in read-only mode.",
        ),
        (
            "POST",
            "/api/login/logout-all",
            "login::logout_all_handler",
            "Device sign-out retains its elevation policy.",
        ),
        (
            "DELETE",
            "/api/login/sessions/{id}",
            "login::revoke_session_handler",
            "Device revocation retains its elevation policy.",
        ),
        (
            "POST",
            "/api/login/passkey/start",
            "passkeys::login_start_handler",
            "Passkey authentication remains available in read-only mode.",
        ),
        (
            "POST",
            "/api/login/passkey/finish",
            "passkeys::login_finish_handler",
            "Passkey authentication remains available in read-only mode.",
        ),
        (
            "POST",
            "/api/passkeys/register/start",
            "passkeys::register_start_handler",
            "Passkey registration retains its elevation policy.",
        ),
        (
            "POST",
            "/api/passkeys/register/finish",
            "passkeys::register_finish_handler",
            "Passkey registration retains its elevation policy.",
        ),
        (
            "DELETE",
            "/api/passkeys/{id}",
            "passkeys::delete_handler",
            "Passkey removal retains its elevation policy.",
        ),
        (
            "POST",
            "/mcp",
            "super::mcp::service(state.clone())",
            "Read-only tools pinned by server::mcp::tests::tools_list_advertises_both_tools; revisit this exemption when adding a tool.",
        ),
        (
            "PUT",
            "/mcp",
            "super::mcp::service(state.clone())",
            "Read-only tools pinned by server::mcp::tests::tools_list_advertises_both_tools; revisit this exemption when adding a tool.",
        ),
        (
            "PATCH",
            "/mcp",
            "super::mcp::service(state.clone())",
            "Read-only tools pinned by server::mcp::tests::tools_list_advertises_both_tools; revisit this exemption when adding a tool.",
        ),
        (
            "DELETE",
            "/mcp",
            "super::mcp::service(state.clone())",
            "Read-only tools pinned by server::mcp::tests::tools_list_advertises_both_tools; revisit this exemption when adding a tool.",
        ),
    ];

    fn audited_handler(identity: &str, audited: &[&str]) -> bool {
        audited.iter().any(|name| {
            if name.contains("::") {
                identity == *name
            } else {
                identity.strip_prefix("api::") == Some(*name)
            }
        })
    }

    fn unrouted_mutating_handlers<'a>(
        routes: &[crate::server::access::MutatingRoute<'_>],
        audited: &[&'a str],
    ) -> Vec<&'a str> {
        audited
            .iter()
            .copied()
            .filter(|name| {
                !routes
                    .iter()
                    .any(|route| audited_handler(route.handler, &[*name]))
            })
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum MutationAuditFailureKind {
        UnknownHandler,
        AuditedAndExempt,
        MissingReason,
        StaleExemption,
        DuplicateExemption,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct MutationAuditFailure<'a> {
        kind: MutationAuditFailureKind,
        identity: (&'a str, &'a str, &'a str),
    }

    fn mutation_audit_failures<'a>(
        routes: &[crate::server::access::MutatingRoute<'a>],
        audited: &[&str],
        exemptions: &[(&'a str, &'a str, &'a str, &str)],
    ) -> Vec<MutationAuditFailure<'a>> {
        let mut failures = Vec::new();
        for route in routes {
            let exempt = exemptions.iter().any(|(method, path, handler, _)| {
                (route.method, route.path, route.handler) == (*method, *path, *handler)
            });
            let kind = match (audited_handler(route.handler, audited), exempt) {
                (false, false) => Some(MutationAuditFailureKind::UnknownHandler),
                (true, true) => Some(MutationAuditFailureKind::AuditedAndExempt),
                _ => None,
            };
            if let Some(kind) = kind {
                failures.push(MutationAuditFailure {
                    kind,
                    identity: (route.method, route.path, route.handler),
                });
            }
        }
        for (index, (method, path, handler, reason)) in exemptions.iter().enumerate() {
            let identity = (*method, *path, *handler);
            if reason.trim().is_empty() {
                failures.push(MutationAuditFailure {
                    kind: MutationAuditFailureKind::MissingReason,
                    identity,
                });
            }
            if !routes.iter().any(|route| {
                (route.method, route.path, route.handler) == (*method, *path, *handler)
            }) {
                failures.push(MutationAuditFailure {
                    kind: MutationAuditFailureKind::StaleExemption,
                    identity,
                });
            }
            if exemptions[..index]
                .iter()
                .any(|(m, p, h, _)| (*m, *p, *h) == (*method, *path, *handler))
            {
                failures.push(MutationAuditFailure {
                    kind: MutationAuditFailureKind::DuplicateExemption,
                    identity,
                });
            }
        }
        failures
    }

    #[test]
    fn mutation_audit_rejects_unknown_handlers_without_prefix_exceptions() {
        use crate::server::access::MutatingRoute;

        for handler in [
            "api::new_handler",
            "other::new_handler",
            "login::new_handler",
            "push::new_handler",
        ] {
            let routes = [MutatingRoute {
                method: "POST",
                path: "/unrelated/new-route",
                handler,
                read_only_layer: true,
            }];
            assert_eq!(
                mutation_audit_failures(&routes, &[], &[]),
                vec![MutationAuditFailure {
                    kind: MutationAuditFailureKind::UnknownHandler,
                    identity: ("POST", "/unrelated/new-route", handler),
                }]
            );
        }
    }

    #[test]
    fn mutation_audit_exemptions_require_exact_identity_and_reason() {
        use crate::server::access::MutatingRoute;
        use MutationAuditFailureKind::*;

        let routes = [MutatingRoute {
            method: "POST",
            path: "/discovery",
            handler: "api::discover",
            read_only_layer: false,
        }];
        let exemption = ("POST", "/discovery", "api::discover", "Discovery policy.");
        assert!(mutation_audit_failures(&routes, &[], &[exemption]).is_empty());
        for changed in [
            ("PUT", exemption.1, exemption.2, exemption.3),
            (exemption.0, "/other", exemption.2, exemption.3),
            (exemption.0, exemption.1, "other::discover", exemption.3),
        ] {
            assert_eq!(
                mutation_audit_failures(&routes, &[], &[changed]),
                vec![
                    MutationAuditFailure {
                        kind: UnknownHandler,
                        identity: (exemption.0, exemption.1, exemption.2),
                    },
                    MutationAuditFailure {
                        kind: StaleExemption,
                        identity: (changed.0, changed.1, changed.2),
                    },
                ]
            );
        }
        for (routes, audited, exemptions, kind) in [
            (&[][..], &[][..], vec![exemption], StaleExemption),
            (
                &routes[..],
                &[][..],
                vec![exemption, exemption],
                DuplicateExemption,
            ),
            (
                &routes[..],
                &["discover"][..],
                vec![exemption],
                AuditedAndExempt,
            ),
            (
                &routes[..],
                &[][..],
                vec![(exemption.0, exemption.1, exemption.2, " ")],
                MissingReason,
            ),
        ] {
            assert_eq!(
                mutation_audit_failures(routes, audited, &exemptions),
                vec![MutationAuditFailure {
                    kind,
                    identity: (exemption.0, exemption.1, exemption.2),
                }]
            );
        }
    }

    #[test]
    fn mutation_audit_matches_qualified_handlers_without_conflating_modules() {
        use crate::server::access::MutatingRoute;

        let routes = [MutatingRoute {
            method: "POST",
            path: "/push",
            handler: "push::subscribe",
            read_only_layer: false,
        }];
        assert!(mutation_audit_failures(&routes, &["push::subscribe"], &[]).is_empty());
        for audited in ["subscribe", "api::subscribe"] {
            assert_eq!(
                mutation_audit_failures(&routes, &[audited], &[]),
                vec![MutationAuditFailure {
                    kind: MutationAuditFailureKind::UnknownHandler,
                    identity: ("POST", "/push", "push::subscribe"),
                }]
            );
        }
    }

    #[test]
    fn mutation_audit_rejects_unrouted_handlers_without_conflating_modules() {
        use crate::server::access::MutatingRoute;

        let routes = [
            MutatingRoute {
                method: "POST",
                path: "/kept",
                handler: "api::kept",
                read_only_layer: false,
            },
            MutatingRoute {
                method: "POST",
                path: "/push",
                handler: "push::subscribe",
                read_only_layer: false,
            },
        ];
        assert_eq!(
            unrouted_mutating_handlers(
                &routes,
                &["kept", "removed", "push::subscribe", "subscribe"]
            ),
            vec!["removed", "subscribe"]
        );
    }

    fn handler_source<'a>(source: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
        use syn::spanned::Spanned;

        let file = syn::parse_file(source)
            .unwrap_or_else(|error| panic!("invalid Rust source while auditing `{name}`: {error}"));
        file.items.into_iter().find_map(|item| {
            let syn::Item::Fn(function) = item else {
                return None;
            };
            if function.sig.ident != name {
                return None;
            }
            let signature = function.sig.span().byte_range();
            let opening = function.block.brace_token.span.open().byte_range();
            let closing = function.block.brace_token.span.close().byte_range();
            Some((
                source.get(signature)?,
                source.get(opening.start..closing.end)?,
            ))
        })
    }

    fn eager_json_type(ty: &syn::Type) -> bool {
        let syn::Type::Path(path) = ty else {
            return false;
        };
        let Some(segment) = path.path.segments.last() else {
            return false;
        };
        if segment.ident == "Json" {
            return true;
        }
        if segment.ident != "Option" {
            return false;
        }
        let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
            return false;
        };
        arguments.args.iter().any(
            |argument| matches!(argument, syn::GenericArgument::Type(ty) if eager_json_type(ty)),
        )
    }

    fn signature_extractors(signature: &str) -> (Option<usize>, Option<usize>) {
        use syn::spanned::Spanned;

        let signature =
            syn::parse_str::<syn::Signature>(signature).expect("audited function signature");
        let mut parts_guard = None;
        let mut consumer = None;
        for argument in &signature.inputs {
            let syn::FnArg::Typed(argument) = argument else {
                continue;
            };
            // Optional or nested guards do not establish unconditional admission.
            if parts_guard.is_none()
                && matches!(&*argument.ty, syn::Type::Path(path)
                    if path.path.segments.last().is_some_and(|segment|
                        segment.ident == "ElevatedMutationGuard"
                            && matches!(segment.arguments, syn::PathArguments::None)))
            {
                parts_guard = Some(argument.ty.span().byte_range().start);
            }
            if consumer.is_none() && eager_json_type(&argument.ty) {
                consumer = Some(argument.ty.span().byte_range().start);
            }
            if parts_guard.is_some() && consumer.is_some() {
                break;
            }
        }
        (parts_guard, consumer)
    }

    fn body_admission_guards(signature: &str, body: &str, delegate: Option<&str>) -> (bool, bool) {
        use syn::{spanned::Spanned, visit::Visit};

        struct FirstUse<'a> {
            binding: &'a syn::Ident,
            position: Option<usize>,
        }
        impl FirstUse<'_> {
            fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
                for token in tokens {
                    match token {
                        proc_macro2::TokenTree::Ident(ident) if ident == *self.binding => {
                            self.record(ident.span().byte_range().start);
                        }
                        proc_macro2::TokenTree::Group(group) => self.tokens(group.stream()),
                        _ => {}
                    }
                }
            }

            fn record(&mut self, position: usize) {
                self.position = Some(self.position.map_or(position, |old| old.min(position)));
            }
        }
        impl<'ast> Visit<'ast> for FirstUse<'_> {
            fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
                if path.path.get_ident() == Some(self.binding) {
                    self.record(path.span().byte_range().start);
                }
                syn::visit::visit_expr_path(self, path);
            }

            fn visit_macro(&mut self, expression: &'ast syn::Macro) {
                self.tokens(expression.tokens.clone());
            }
        }

        fn ungroup(expression: &syn::Expr) -> &syn::Expr {
            match expression {
                syn::Expr::Paren(value) if value.attrs.is_empty() => ungroup(&value.expr),
                syn::Expr::Group(value) if value.attrs.is_empty() => ungroup(&value.expr),
                _ => expression,
            }
        }

        fn expression_exits(expression: &syn::Expr) -> bool {
            match ungroup(expression) {
                syn::Expr::Return(value) => value.attrs.is_empty(),
                syn::Expr::Block(value) if value.attrs.is_empty() => exits(&value.block),
                syn::Expr::If(value) if value.attrs.is_empty() => {
                    exits(&value.then_branch)
                        && value
                            .else_branch
                            .as_ref()
                            .is_some_and(|(_, otherwise)| expression_exits(otherwise))
                }
                _ => false,
            }
        }

        fn exits(block: &syn::Block) -> bool {
            block.stmts.iter().any(|statement| {
                matches!(statement, syn::Stmt::Expr(expression, _) if expression_exits(expression))
            })
        }

        fn state_value(expression: &syn::Expr, state: Option<&syn::Ident>) -> bool {
            match ungroup(expression) {
                syn::Expr::Reference(value) if value.attrs.is_empty() => {
                    state_value(&value.expr, state)
                }
                syn::Expr::Path(value) => {
                    value.attrs.is_empty() && state.is_some() && value.path.get_ident() == state
                }
                _ => false,
            }
        }

        fn state_call<'a>(
            expression: &'a syn::Expr,
            target: &str,
            state: Option<&syn::Ident>,
        ) -> Option<&'a syn::ExprCall> {
            let syn::Expr::Call(call) = ungroup(expression) else {
                return None;
            };
            (call.attrs.is_empty()
                && matches!(ungroup(&call.func), syn::Expr::Path(path)
                    if path.attrs.is_empty() && path.qself.is_none()
                    && path.path.segments.last().is_some_and(|name| name.ident == target))
                && call
                    .args
                    .first()
                    .is_some_and(|argument| state_value(argument, state)))
            .then_some(call)
        }

        fn read_only_call(expression: &syn::Expr, state: Option<&syn::Ident>) -> bool {
            if let Some(call) = state_call(expression, "read_only_block", state) {
                return call.args.len() == 1;
            }
            matches!(ungroup(expression), syn::Expr::MethodCall(call)
                if call.attrs.is_empty() && call.method == "or_else" && call.args.len() == 1
                && matches!(ungroup(&call.args[0]), syn::Expr::Closure(fallback)
                    if fallback.attrs.is_empty() && fallback.inputs.is_empty()
                    && fallback.asyncness.is_none())
                && read_only_call(&call.receiver, state))
        }

        fn app_state_type(ty: &syn::Type) -> bool {
            match ty {
                syn::Type::Reference(reference) => app_state_type(&reference.elem),
                syn::Type::Path(path) => path.path.segments.iter().any(|segment| {
                    segment.ident == "AppState"
                        || matches!(&segment.arguments, syn::PathArguments::AngleBracketed(arguments)
                            if arguments.args.iter().any(|argument|
                                matches!(argument, syn::GenericArgument::Type(ty) if app_state_type(ty))))
                }),
                _ => false,
            }
        }

        fn read_only_condition(condition: &syn::Expr, state: Option<&syn::Ident>) -> bool {
            match ungroup(condition) {
                syn::Expr::Field(field) => {
                    field.attrs.is_empty()
                        && matches!(&field.member, syn::Member::Named(member) if member == "read_only")
                        && state_value(&field.base, state)
                }
                syn::Expr::Let(binding) => {
                    binding.attrs.is_empty()
                        && matches!(&*binding.pat, syn::Pat::TupleStruct(pattern)
                            if pattern.path.is_ident("Some") && pattern.elems.len() == 1)
                        && read_only_call(&binding.expr, state)
                }
                _ => false,
            }
        }

        fn delegated_condition(
            condition: &syn::Expr,
            state: Option<&syn::Ident>,
            delegate: Option<&str>,
        ) -> bool {
            let (syn::Expr::Let(binding), Some(target)) = (ungroup(condition), delegate) else {
                return false;
            };
            binding.attrs.is_empty()
                && matches!(&*binding.pat, syn::Pat::TupleStruct(pattern)
                    if pattern.path.is_ident("Err") && pattern.elems.len() == 1)
                && state_call(&binding.expr, target, state).is_some_and(|call| call.args.len() == 1)
        }

        fn tail_delegate<'a>(
            body: &'a syn::Block,
            state: Option<&syn::Ident>,
            delegate: Option<&str>,
        ) -> Option<&'a syn::ExprCall> {
            let [syn::Stmt::Expr(expression, semicolon)] = body.stmts.as_slice() else {
                return None;
            };
            let expression = match ungroup(expression) {
                syn::Expr::Return(value) if value.attrs.is_empty() => value.expr.as_deref()?,
                expression if semicolon.is_none() => expression,
                _ => return None,
            };
            let syn::Expr::Await(value) = ungroup(expression) else {
                return None;
            };
            if !value.attrs.is_empty() {
                return None;
            }
            state_call(&value.base, delegate?, state)
        }

        fn rejection_branch(block: &syn::Block, condition: &syn::Expr) -> bool {
            let binding = match ungroup(condition) {
                syn::Expr::Let(value) => match &*value.pat {
                    syn::Pat::TupleStruct(pattern) => {
                        pattern.elems.first().and_then(|pattern| match pattern {
                            syn::Pat::Ident(binding)
                                if binding.attrs.is_empty() && binding.subpat.is_none() =>
                            {
                                Some(&binding.ident)
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                },
                _ => None,
            };
            fn direct(expression: &syn::Expr, binding: Option<&syn::Ident>) -> bool {
                match ungroup(expression) {
                    syn::Expr::Path(path) => {
                        path.attrs.is_empty()
                            && binding.is_some()
                            && path.path.get_ident() == binding
                    }
                    syn::Expr::Call(call) => {
                        call.attrs.is_empty()
                            && call.args.is_empty()
                            && matches!(ungroup(&call.func), syn::Expr::Path(path)
                                if path.attrs.is_empty() && path.qself.is_none()
                                && path.path.segments.last().is_some_and(|segment|
                                    segment.ident == "read_only_response"))
                    }
                    _ => false,
                }
            }
            fn forbidden(expression: &syn::Expr) -> bool {
                let syn::Expr::Path(path) = ungroup(expression) else {
                    return false;
                };
                if !path.attrs.is_empty() || path.qself.is_some() {
                    return false;
                }
                let mut segments = path.path.segments.iter().rev();
                segments
                    .next()
                    .is_some_and(|segment| segment.ident == "FORBIDDEN")
                    && segments
                        .next()
                        .is_some_and(|segment| segment.ident == "StatusCode")
            }
            fn forbidden_value(expression: &syn::Expr) -> bool {
                match ungroup(expression) {
                    syn::Expr::MethodCall(call) => {
                        call.attrs.is_empty()
                            && call.method == "into_response"
                            && call.args.is_empty()
                            && forbidden(&call.receiver)
                    }
                    syn::Expr::Tuple(tuple) if tuple.attrs.is_empty() && tuple.elems.len() == 2 => {
                        forbidden(&tuple.elems[0])
                            && matches!(ungroup(&tuple.elems[1]), syn::Expr::MethodCall(call)
                                if call.attrs.is_empty() && call.method == "into"
                                && call.args.is_empty()
                                && matches!(ungroup(&call.receiver), syn::Expr::Lit(value)
                                    if value.attrs.is_empty() && matches!(value.lit, syn::Lit::Str(_))))
                    }
                    _ => false,
                }
            }
            let [syn::Stmt::Expr(syn::Expr::Return(returned), _)] = block.stmts.as_slice() else {
                return false;
            };
            let Some(value) = returned
                .expr
                .as_deref()
                .filter(|_| returned.attrs.is_empty())
            else {
                return false;
            };
            direct(value, binding)
                || matches!(ungroup(value), syn::Expr::Call(call)
                    if call.attrs.is_empty() && call.args.len() == 1
                    && matches!(ungroup(&call.func), syn::Expr::Path(path)
                        if path.attrs.is_empty() && path.path.is_ident("Err"))
                    && (direct(&call.args[0], binding) || forbidden_value(&call.args[0])))
        }

        struct Propagates(bool);
        impl<'ast> Visit<'ast> for Propagates {
            fn visit_expr_try(&mut self, _: &'ast syn::ExprTry) {
                self.0 = true;
            }
        }

        fn guarded_before(
            body: &syn::Block,
            state: Option<&syn::Ident>,
            delegate: Option<&str>,
            position: usize,
        ) -> bool {
            for statement in &body.stmts {
                if let syn::Stmt::Expr(syn::Expr::If(guard), _) = statement {
                    if guard.attrs.is_empty()
                        && guard.span().byte_range().end <= position
                        && rejection_branch(&guard.then_branch, &guard.cond)
                        && (read_only_condition(&guard.cond, state)
                            || delegated_condition(&guard.cond, state, delegate))
                    {
                        return true;
                    }
                }
                let mut propagates = Propagates(false);
                propagates.visit_stmt(statement);
                if propagates.0 {
                    break;
                }
                // Opaque control flow is not admission evidence.
                match statement {
                    syn::Stmt::Local(local)
                        if local
                            .init
                            .as_ref()
                            .is_some_and(|init| expression_exits(&init.expr)) =>
                    {
                        break
                    }
                    syn::Stmt::Expr(expression, _) if expression_exits(expression) => break,
                    syn::Stmt::Expr(
                        syn::Expr::Loop(_)
                        | syn::Expr::ForLoop(_)
                        | syn::Expr::While(_)
                        | syn::Expr::Match(_),
                        _,
                    )
                    | syn::Stmt::Macro(_) => break,
                    _ => {}
                }
            }
            false
        }

        let signature = syn::parse_str::<syn::Signature>(signature).expect("audited signature");
        let state = signature.inputs.iter().find_map(|argument| {
            let syn::FnArg::Typed(argument) = argument else {
                return None;
            };
            if !app_state_type(&argument.ty) {
                return None;
            }
            match &*argument.pat {
                syn::Pat::Ident(binding) => Some(&binding.ident),
                syn::Pat::TupleStruct(pattern) if pattern.elems.len() == 1 => {
                    match pattern.elems.first()? {
                        syn::Pat::Ident(binding) => Some(&binding.ident),
                        _ => None,
                    }
                }
                _ => None,
            }
        });
        let body = syn::parse_str::<syn::Block>(body).expect("audited body");
        let delegated = tail_delegate(&body, state, delegate);
        let guarded = delegated.is_some() || guarded_before(&body, state, delegate, usize::MAX);
        let deferred = signature.inputs.iter().all(|argument| {
            let syn::FnArg::Typed(argument) = argument else {
                return true;
            };
            let syn::Type::Path(path) = &*argument.ty else {
                return true;
            };
            let Some(segment) = path.path.segments.last() else {
                return true;
            };
            let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                return true;
            };
            if segment.ident != "Result"
                || !matches!(arguments.args.first(), Some(syn::GenericArgument::Type(ty)) if eager_json_type(ty))
            {
                return true;
            }
            let syn::Pat::Ident(binding) = &*argument.pat else {
                return false;
            };
            let mut first = FirstUse {
                binding: &binding.ident,
                position: None,
            };
            first.visit_block(&body);
            let Some(first) = first.position else {
                return true;
            };
            if guarded_before(&body, state, delegate, first) {
                return true;
            }
            delegated.is_some_and(|call| {
                call.args.iter().any(|argument| {
                    matches!(ungroup(argument), syn::Expr::Path(path)
                        if path.path.get_ident() == Some(&binding.ident)
                        && path.span().byte_range().start == first)
                })
            })
        });
        (guarded, deferred)
    }

    #[test]
    fn handler_source_stays_inside_the_selected_top_level_function() {
        let source = r###"
            pub async fn selected(body: Option<Json<Body>>) {
                let text = "}";
                let raw = r#"{ fn fake() {} }"#;
                let character = '}';
                /* outer { /* nested } */ } */
                if true { consume(text, raw, character); }
            }
            pub(crate) fn neighbor() { if state.read_only {} }
            #[cfg(test)]
            mod tests {
                fn selected() { if state.read_only {} }
                fn only_in_tests() {}
            }
        "###;
        let (signature, body) = handler_source(source, "selected").unwrap();
        assert!(signature.contains("Option<Json<Body>>"));
        assert!(body.contains("consume(text, raw, character)"));
        assert!(!body.contains("state.read_only"));
        assert!(!body.contains("neighbor"));
        assert!(handler_source(source, "only_in_tests").is_none());
        let (_, install) = handler_source(include_str!("acp/install.rs"), "install_agent").unwrap();
        assert!(!install.contains("state.read_only"));
        assert!(install.contains("install_gate(&state)"));
    }

    #[test]
    fn eager_json_detection_distinguishes_deferred_rejection() {
        for (signature, expected) in [
            ("fn write(body: Json<Body>)", true),
            ("fn write(body: Option<Json<Body>>)", true),
            ("fn write(body: Option<axum::Json<Body>>)", true),
            ("fn write(body: Result<Json<Body>, JsonRejection>)", false),
            (
                "fn write(body: Result<Option<Json<Body>>, JsonRejection>)",
                false,
            ),
            ("fn write(body: Bytes)", false),
        ] {
            assert_eq!(
                signature_extractors(signature).1.is_some(),
                expected,
                "{signature}"
            );
        }
    }

    #[test]
    fn parts_guard_detection_rejects_decoy_extractors() {
        for (signature, guarded) in [
            (
                "fn write(guard: ElevatedMutationGuard, body: Json<Body>)",
                true,
            ),
            (
                "fn write(guard: api::ElevatedMutationGuard, body: Json<Body>)",
                true,
            ),
            (
                "fn write(guard: NotElevatedMutationGuard, body: Json<Body>)",
                false,
            ),
            (
                "fn write(guard: ElevatedMutationGuardExtra, body: Json<Body>)",
                false,
            ),
            (
                "fn write(ElevatedMutationGuard: Other, body: Json<Body>)",
                false,
            ),
            (
                "fn write(guard: Option<ElevatedMutationGuard>, body: Json<Body>)",
                false,
            ),
        ] {
            let (parts_guard, consumer) = signature_extractors(signature);
            assert_eq!(
                parts_guard
                    .zip(consumer)
                    .is_some_and(|(guard, body)| guard < body),
                guarded,
                "{signature}"
            );
        }
    }

    #[test]
    fn body_admission_rejects_non_executable_guards_and_delegation() {
        let signature =
            "fn write(State(state): State<Arc<AppState>>, body: Result<Json<Body>, JsonRejection>)";
        let shared = "fn gate(state: &AppState)";
        for (signature, body, delegate, expected) in [
            (signature, "{ if state.read_only { return read_only_response(); } work(); }", None, true),
            (signature, "{ if other_gate() { return denied(); } if state.read_only { return read_only_response(); } work(); }", None, true),
            (signature, "{ if let Some(resp) = crate::server::api::read_only_block(&state) { return resp; } work(); }", None, true),
            (shared, "{ if let Some(resp) = read_only_block(state).or_else(|| cityhall_block(state)) { return Err(resp); } Ok(()) }", None, true),
            (signature, "{ prepare_agent_session(state, id, body, mode, None).await }", Some("prepare_agent_session"), true),
            (signature, "{ return super::lifecycle::prepare_agent_session(state, id, body, mode, None).await; }", Some("prepare_agent_session"), true),
            (signature, "{ if let Err(resp) = install_gate(&state) { return resp; } work(); }", Some("install_gate"), true),
            (signature, "{ if state.read_only { return Err(read_only_response()); } work(); }", None, true),
            (signature, "{ if state.read_only { return Err(StatusCode::FORBIDDEN.into_response()); } work(); }", None, true),
            (signature, "{ if state.read_only { return Err((StatusCode::FORBIDDEN, \"read-only\".into())); } work(); }", None, true),
            (signature, "{ check()?; if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ let value = wrap(check()?); if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ if state.read_only { return StatusCode::OK.into_response(); } work(); }", None, false),
            (signature, "{ if let Some(resp) = read_only_block(&state) { return other; } work(); }", None, false),
            (signature, "{ if let Some(resp) = read_only_block(&state) { return Ok(resp); } work(); }", None, false),
            (signature, "{ if state.read_only { mutate(); return read_only_response(); } work(); }", None, false),
            (signature, "{ /* if state.read_only { return read_only_response(); } */ work(); }", None, false),
            (signature, "{ let text = \"state.read_only\"; work(); }", None, false),
            (signature, r##"{ let text = r#"if state.read_only { return read_only_response(); }"#; work(); }"##, None, false),
            (signature, "{ work(); // read_only_block(&state)\n }", None, false),
            (signature, "{ let callback = || { if state.read_only { return read_only_response(); } }; work(); }", None, false),
            (signature, "{ let future = async { if state.read_only { return read_only_response(); } }; work(); }", None, false),
            (signature, "{ fn nested(state: &AppState) { if state.read_only { return read_only_response(); } } work(); }", None, false),
            (signature, "{ if false { if state.read_only { return read_only_response(); } } work(); }", None, false),
            (signature, "{ if state.read_only { record(); } work(); }", None, false),
            (signature, "{ if state.read_only && condition { return read_only_response(); } work(); }", None, false),
            (signature, "{ return ok(); if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ let stopped = { return ok(); }; if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ { return ok(); } if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ if flag { return a(); } else { return b(); } if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ #[cfg(any())] if state.read_only { return read_only_response(); } work(); }", None, false),
            (signature, "{ let text = \"prepare_agent_session(state, id, body, mode, None)\"; work(); }", Some("prepare_agent_session"), false),
            (signature, "{ let future = prepare_agent_session(state, id, body, mode, None); work(); }", Some("prepare_agent_session"), false),
            (signature, "{ let callback = || prepare_agent_session(state, id, body, mode, None); work(); }", Some("prepare_agent_session"), false),
            (signature, "{ if false { prepare_agent_session(state, id, body, mode, None).await; } work(); }", Some("prepare_agent_session"), false),
            (signature, "{ prepare_agent_session(state, id, body, mode, None).await; }", Some("prepare_agent_session"), false),
            (signature, "{ prepare_agent_session(state, id, body, mode, None).await; work(); }", Some("prepare_agent_session"), false),
            (signature, "{ prepare_agent_session(other, id, body, mode, None).await }", Some("prepare_agent_session"), false),
            (signature, "{ different(state, id, body, mode, None).await }", Some("prepare_agent_session"), false),
            (signature, "{ if let Err(resp) = install_gate(&other) { return resp; } work(); }", Some("install_gate"), false),
            (signature, "{ install_gate(&state); work(); }", Some("install_gate"), false),
            (signature, "{ let text = \"install_gate(&state)\"; work(); }", Some("install_gate"), false),
        ] {
            assert_eq!(
                body_admission_guards(signature, body, delegate).0,
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn deferred_json_requires_a_completed_guard_before_first_use() {
        for parameter in ["body", "payload"] {
            for ty in ["Json<Body>", "Option<axum::Json<Body>>"] {
                let signature = format!(
                    "fn write(State(app): State<Arc<AppState>>, {parameter}: std::result::Result<{ty}, JsonRejection>)"
                );
                for consumer in [
                    format!("let value = match {parameter} {{ Ok(value) => value, Err(error) => return error }};"),
                    format!("let Ok(value) = {parameter} else {{ return bad_body() }};"),
                    format!("let value = {parameter}.map_err(rejection)?;"),
                    format!("consume!({parameter});"),
                ] {
                    for guard in [
                        "if app.read_only { return read_only_response() }",
                        "if let Some(response) = read_only_block(&app) { return response }",
                    ] {
                        assert!(body_admission_guards(
                            &signature, &format!("{{ {guard} {consumer} }}"), None
                        ).1);
                        assert!(!body_admission_guards(
                            &signature, &format!("{{ {consumer} {guard} }}"), None
                        ).1);
                    }
                    for not_a_guard in [
                        "if app.read_only { return StatusCode::OK.into_response(); }",
                        "if let Some(response) = read_only_block(&app) { return other }",
                        "check()?; if app.read_only { return read_only_response(); }",
                        "let value = wrap(check()?); if app.read_only { return read_only_response(); }",
                        r#"let text = "if app.read_only { return read_only_response() }";"#,
                        "/* if app.read_only { return read_only_response() } */",
                        "if app.read_only { record() }",
                        "if app.read_only && condition { return read_only_response() }",
                        "if condition { if app.read_only { return read_only_response() } }",
                    ] {
                        assert!(!body_admission_guards(
                            &signature, &format!("{{ {not_a_guard} {consumer} }}"), None
                        ).1);
                    }
                }
                assert!(
                    body_admission_guards(
                        &signature,
                        &format!("{{ prepare(app, id, {parameter}).await }}"),
                        Some("prepare")
                    )
                    .1
                );
                for body in [
                    format!("{{ prepare(app, id, {parameter}.map(transform)).await }}"),
                    format!("{{ inspect(&{parameter}); prepare(app, id, {parameter}).await }}"),
                    format!("{{ unknown(app, id, {parameter}).await }}"),
                ] {
                    assert!(!body_admission_guards(&signature, &body, Some("prepare")).1);
                }
            }
        }
    }

    #[test]
    fn mutating_handlers_guard_read_only_before_body_extraction() {
        let routes = crate::server::access::router_mutating_handlers();
        let audited: Vec<_> = MUTATING_HANDLERS
            .iter()
            .flat_map(|(_, _, names)| names.iter().copied())
            .collect();
        let classification_failures =
            mutation_audit_failures(&routes, &audited, MUTATION_AUDIT_EXEMPTIONS);
        let mut failures = Vec::new();
        failures.extend(
            unrouted_mutating_handlers(&routes, &audited)
                .into_iter()
                .map(|name| format!("audited handler `{name}` has no mutating route")),
        );
        for (file, source, handlers) in MUTATING_HANDLERS.iter().chain(MUTATION_AUDIT_HELPERS) {
            for name in *handlers {
                let source_name = name.rsplit("::").next().expect("handler name");
                let Some((signature, body)) = handler_source(source, source_name) else {
                    failures.push(format!("{file}: handler `{name}` not found"));
                    continue;
                };
                let delegate = match *name {
                    "ensure_session" | "start_session" => Some("prepare_agent_session"),
                    "prepare_agent_session" => Some("prepare_agent_session_inner"),
                    "install_agent" => Some("install_gate"),
                    _ => None,
                };
                let mut handler_routes = routes
                    .iter()
                    .filter(|route| audited_handler(route.handler, &[*name]))
                    .peekable();
                let route_layer = handler_routes.peek().is_some()
                    && handler_routes.all(|route| route.read_only_layer);
                let (parts_guard, consumer) = signature_extractors(signature);
                let (guarded, deferred_guarded) = if route_layer || parts_guard.is_some() {
                    (true, true)
                } else {
                    body_admission_guards(signature, body, delegate)
                };
                if !route_layer && parts_guard.is_none() && !guarded {
                    failures.push(format!("{file}: `{name}` lacks a read-only guard"));
                }
                if consumer.is_some()
                    && !route_layer
                    && !parts_guard
                        .zip(consumer)
                        .is_some_and(|(guard, body)| guard < body)
                {
                    failures.push(format!(
                        "{file}: `{name}` rejects JSON before its read-only guard"
                    ));
                }
                if !route_layer && parts_guard.is_none() && !deferred_guarded {
                    failures.push(format!(
                        "{file}: `{name}` can reject deferred JSON before its read-only guard"
                    ));
                }
            }
        }
        assert!(
            classification_failures.is_empty() && failures.is_empty(),
            "{classification_failures:#?}\n{}",
            failures.join("\n")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_mutation_admission_preserves_body_and_elevation_precedence() {
        use axum::http::{Method, Request};
        use base64::Engine;
        use tower::ServiceExt;

        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| state.read_only = true,
        );
        let app = crate::server::test_support::build_router_for_test(state);
        for (method, path) in [
            (Method::POST, "/api/sessions/creation-trust"),
            (Method::POST, "/api/sessions/missing/creation/cancel"),
            (Method::PATCH, "/api/sessions/missing/favorite"),
            (Method::PATCH, "/api/sessions/missing/access"),
            (Method::POST, "/api/sessions/missing/purge/abandon"),
            (Method::POST, "/api/sessions/missing/restart"),
            (Method::POST, "/api/sessions/missing/restart-for-update"),
            (Method::POST, "/api/sessions/missing/tools/ensure"),
            (Method::POST, "/api/sessions/missing/auxiliary/stop"),
            (Method::POST, "/api/sessions/missing/acp/switch-agent"),
            (Method::PATCH, "/api/sessions/missing"),
            (Method::DELETE, "/api/sessions/missing"),
            (Method::POST, "/api/sessions/missing/trash"),
            (Method::POST, "/api/plugins/commands/missing/invoke"),
            (Method::POST, "/api/sessions/empty-trash"),
            (Method::POST, "/api/groups"),
            (Method::PATCH, "/api/groups"),
            (Method::DELETE, "/api/groups"),
            (Method::PATCH, "/api/groups/collapse"),
            (Method::POST, "/api/reorder"),
            (Method::DELETE, "/api/workspaces"),
            (Method::POST, "/api/plugins/missing/enabled"),
            (Method::POST, "/api/plugins/missing/worker/restart"),
            (Method::POST, "/api/plugins/missing/uninstall"),
            (Method::POST, "/api/plugins/install/preview"),
            (Method::POST, "/api/plugins/install"),
            (Method::POST, "/api/plugins/missing/update/apply"),
            (Method::POST, "/api/plugins/missing/update/dismiss"),
            (Method::PATCH, "/api/theme"),
            (Method::POST, "/api/suggestions/refresh"),
            (Method::POST, "/api/suggestions/checks/run"),
            (Method::POST, "/api/suggestions/dismiss"),
            (Method::POST, "/api/suggestions/undismiss"),
            (Method::PATCH, "/api/suggestions/sources"),
            (Method::PATCH, "/api/suggestions/prefs"),
            (Method::PUT, "/api/suggestions/templates"),
            (Method::DELETE, "/api/suggestions/templates"),
            (Method::PUT, "/api/suggestions/mappings"),
            (Method::DELETE, "/api/suggestions/mappings"),
            (Method::POST, "/api/sandbox/image-pull"),
            (Method::POST, "/api/sandbox/image-dismiss"),
        ] {
            let mut request = Request::builder()
                .method(method.clone())
                .uri(path)
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{"))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                crate::server::peer::ConnectionPeer::UnixOwner {
                    uid: nix::unistd::geteuid().as_raw(),
                },
            ));
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(
                response.headers()[crate::daemon::ERROR_CODE_HEADER],
                "read_only",
                "{method} {path}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"], "read_only", "{method} {path}");
        }

        let login_manager =
            std::sync::Arc::new(crate::server::login::LoginManager::new(Some("test")));
        let binding = [0x42; 32];
        let session = login_manager
            .create_session(&binding, "127.0.0.1", "plugin-pane-policy")
            .await;
        assert!(!login_manager.is_elevated(&session).await);
        let binding_header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(binding);
        for (read_only, authenticated, status, code) in [
            (false, false, StatusCode::UNAUTHORIZED, "login_required"),
            (false, true, StatusCode::SERVICE_UNAVAILABLE, "no_host"),
            (true, true, StatusCode::FORBIDDEN, "read_only"),
        ] {
            let state = crate::server::test_support::build_test_app_state_with_policy_configured(
                Vec::new(),
                vec!["localhost".into()],
                vec!["http://localhost:8080".into()],
                None,
                |state| {
                    state.read_only = read_only;
                    state.login_manager = login_manager.clone();
                    state.plugin_host = None;
                },
            );
            let app = crate::server::test_support::build_router_for_test(state);
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/api/plugins/missing/action")
                .header("host", "localhost")
                .header("origin", "http://localhost:8080")
                .header("content-type", "application/json");
            if authenticated {
                request = request
                    .header("cookie", format!("aoe_session={session}"))
                    .header("x-aoe-device-binding", &binding_header);
            }
            let mut request = request
                .body(axum::body::Body::from(
                    r#"{"method":"github.refresh","params":{}}"#,
                ))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                crate::server::peer::ConnectionPeer::Tcp(([127, 0, 0, 1], 12345).into()),
            ));
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                status,
                "read_only={read_only}, auth={authenticated}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body["error"], code,
                "read_only={read_only}, auth={authenticated}"
            );
        }
    }

    #[test]
    fn validate_no_shell_injection_rejects_every_metacharacter() {
        for input in [
            "name;command",
            "name&command",
            "name|command",
            "$(command)",
            "`command`",
            "path(1)",
            "path)",
            "path{a,b}",
            "path}",
            "name<input",
            "name>output",
            "name\ncommand",
            "name\rcommand",
            "path\\name",
            "\"quoted\"",
            "it's",
            "name!0",
            "name#comment",
            "path/*",
            "name?",
            "path[0",
            "path]",
            "~root",
            "name\targ",
            "name\0arg",
        ] {
            let result = validate_no_shell_injection(input, "field");
            assert!(
                result.is_err(),
                "validate_no_shell_injection should reject {:?} but accepted {:?}",
                input,
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
