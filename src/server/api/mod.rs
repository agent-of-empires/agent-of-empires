//! HTTP REST handlers for the web dashboard backend.
//!
//! Originally a single 2,151-line module; split into:
//!   - `sessions`: session CRUD, ensure-* lifecycle endpoints, and rich diff
//!   - `git`: repo cloning and branch listing
//!   - `system`: agents, settings, themes, profiles, filesystem,
//!     groups, docker, about, devices
//!   - this file: shared validation helpers + module declarations and
//!     re-exports so external callers keep `api::*` paths.

pub(super) use super::AppState;

#[cfg(feature = "serve")]
mod acp;
#[cfg(feature = "serve")]
mod client_log;
mod git;
mod log_level;
mod mcp;
mod projects;
mod sessions;
pub(crate) mod system;
mod telemetry;

#[cfg(feature = "serve")]
pub use acp::{
    acp_attachment, acp_cancel, acp_context_primer, acp_disable, acp_enable, acp_files,
    acp_force_end_turn, acp_prompt, acp_prompt_diff_comments, acp_replay, acp_set_config_option,
    acp_set_mode, acp_worker_log, list_acp_agents, resolve_approval, shutdown_acp, spawn_acp,
    switch_acp_agent,
};

#[cfg(feature = "serve")]
pub use client_log::post_client_log;
pub use git::{clone_repo, list_branches};
pub use log_level::{get_log_level, patch_log_level};
pub use mcp::{drop_mcp_server, get_mcp_servers, keep_mcp_server, resolve_mcp_conflict};
pub use projects::{create_project, delete_project, list_projects, update_project};
pub use sessions::{
    create_session, delete_session, ensure_container_terminal, ensure_session, ensure_terminal,
    list_sessions, preview_volume_ignores_globs, read_output, rename_session, send_message,
    session_diff_file, session_diff_files, set_worktree_name, update_session_archive,
    update_session_diff_base, update_session_group, update_session_notifications,
    update_session_pin, update_session_snooze, update_workspace_ordering, CleanupDefaults,
    OutputQuery, SendMessageRequest, SessionResponse,
};
pub use system::{
    browse_filesystem, create_profile, default_profile, delete_profile, docker_status,
    filesystem_home, get_about, get_current_theme, get_profile_settings, get_resolved_theme,
    get_settings, get_settings_schema, get_update_status, list_agents, list_groups, list_profiles,
    list_sounds, list_themes, mark_volume_ignores_globs_acknowledged, mark_web_tour_seen,
    rename_profile, serve_sound_file, update_profile_settings, update_settings, update_theme,
};
pub use telemetry::{
    get_telemetry_status, post_telemetry_seen, post_telemetry_structured_interaction,
    set_telemetry_consent,
};

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

// The settings PATCH write surface (which sections/fields the web may write,
// which need elevation, which are host-only) is no longer a hand-kept list
// here: it is derived from the settings schema in
// `crate::session::settings_schema::policy`, the single source of truth shared
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
    //! `crate::session::settings_schema::policy` (#1692).
    use super::*;

    /// Read-only audit: every mutating handler must check `state.read_only`
    /// (directly, or via the `read_only_block` helper) and return 403
    /// before performing any write. This static check walks the handler
    /// source files at compile time via `include_str!` and looks for the
    /// canonical guard pattern inside each named handler's body.
    ///
    /// Why static: building a full AppState in a unit test requires a tmux
    /// runtime, login manager, token manager, broadcast channels, and an
    /// app-data dir. The end-to-end Playwright spec in
    /// `web/tests/live/read-only-mode.spec.ts` covers the runtime path;
    /// this test guards against a contributor adding a new POST/PATCH/DELETE
    /// handler and forgetting the guard.
    ///
    /// Body boundaries: each handler's body runs from `fn <name>` up to
    /// the next `pub async fn `, `pub fn `, or `async fn ` in the same
    /// file. This is more robust than a fixed-char window, which silently
    /// misses guards in handlers whose bodies grow past the window
    /// (caught a real regression on `ensure_session` after an upstream
    /// rebase).
    #[test]
    fn every_mutating_handler_has_read_only_guard() {
        // (file_label, source, list_of_handler_fn_names_we_expect_guarded).
        // When a new POST / PATCH / DELETE handler is added, list its fn
        // name here. The test then enforces that its body contains the
        // guard.
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "api/sessions.rs",
                include_str!("sessions.rs"),
                &[
                    "create_session",
                    "delete_session",
                    "rename_session",
                    "set_worktree_name",
                    "send_message",
                    "ensure_session",
                    "ensure_terminal",
                    "ensure_container_terminal",
                    "update_session_group",
                    "update_session_notifications",
                    "update_session_diff_base",
                    "update_session_pin",
                    "update_session_archive",
                    "update_session_snooze",
                    "update_workspace_ordering",
                ],
            ),
            ("api/git.rs", include_str!("git.rs"), &["clone_repo"]),
            (
                "api/mcp.rs",
                include_str!("mcp.rs"),
                &["resolve_mcp_conflict", "keep_mcp_server", "drop_mcp_server"],
            ),
            (
                "api/log_level.rs",
                include_str!("log_level.rs"),
                &["patch_log_level"],
            ),
            (
                "api/projects.rs",
                include_str!("projects.rs"),
                &["create_project", "delete_project", "update_project"],
            ),
            (
                "api/system.rs",
                include_str!("system.rs"),
                &[
                    "update_settings",
                    "mark_web_tour_seen",
                    "mark_volume_ignores_globs_acknowledged",
                    "create_profile",
                    "delete_profile",
                    "rename_profile",
                    "default_profile",
                    "update_profile_settings",
                ],
            ),
            (
                "api/acp.rs",
                include_str!("acp.rs"),
                &[
                    "spawn_acp",
                    "shutdown_acp",
                    "acp_prompt",
                    "acp_prompt_diff_comments",
                    "acp_cancel",
                    "acp_force_end_turn",
                    "acp_enable",
                    "acp_disable",
                    "acp_set_mode",
                    "acp_set_config_option",
                    "resolve_approval",
                ],
            ),
            (
                "server/push.rs",
                include_str!("../push.rs"),
                &["subscribe", "unsubscribe", "test"],
            ),
            (
                "api/telemetry.rs",
                include_str!("telemetry.rs"),
                &[
                    "set_telemetry_consent",
                    "post_telemetry_seen",
                    "post_telemetry_structured_interaction",
                ],
            ),
        ];

        let guard_patterns: &[&str] = &[
            "state.read_only",
            "self.read_only",
            // Acp handlers use the shared helper from api/acp.rs.
            "read_only_block(",
        ];
        let body_terminators: &[&str] = &["\npub async fn ", "\npub fn ", "\nasync fn ", "\nfn "];

        let mut missing: Vec<String> = Vec::new();
        for (file_label, source, handler_names) in cases {
            for name in *handler_names {
                let needle = format!("fn {name}(");
                let Some(start) = source.find(&needle) else {
                    missing.push(format!(
                        "{file_label}: handler `{name}` not found (rename/refactor?)"
                    ));
                    continue;
                };
                // Body runs from this function's `fn name(` to the start
                // of the next function definition in the file.
                let rest = &source[start + needle.len()..];
                let end_offset = body_terminators
                    .iter()
                    .filter_map(|t| rest.find(t))
                    .min()
                    .unwrap_or(rest.len());
                let body = &rest[..end_offset];
                let has_guard = guard_patterns.iter().any(|p| body.contains(p));
                if !has_guard {
                    missing.push(format!(
                        "{file_label}: handler `{name}` is missing read-only guard. \
                         Mutating handlers must check `state.read_only` (or call \
                         `read_only_block(&state)`) and return 403 before performing \
                         any write. Add the guard, or if the handler is intentionally \
                         read-safe, drop it from this list in the same commit with \
                         justification."
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "Read-only audit failed:\n{}",
            missing.join("\n")
        );
    }

    /// Companion to `every_mutating_handler_has_read_only_guard`: enforce
    /// that any mutating handler taking a typed JSON body extracts it
    /// lazily, so the read-only short-circuit can run BEFORE body shape
    /// validation. Otherwise axum's `Json<T>` extractor returns 422 on a
    /// malformed body and the read-only guard never fires (see #1229).
    ///
    /// Accepted signatures for a Json-bearing handler:
    ///   - `body: Result<Json<T>, ...JsonRejection>` (preferred)
    ///   - `body: Option<Json<T>>`                   (already lazy)
    ///   - `_: Json<serde_json::Value>` does NOT save you: even a Value
    ///     extractor 422s on non-JSON bytes. Wrap it in `Result<...>`.
    ///
    /// The rejected pattern is the eager destructure
    /// `Json(body): Json<T>` (or `Json(_): Json<T>`).
    #[test]
    fn mutating_handlers_extract_body_lazily() {
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "api/sessions.rs",
                include_str!("sessions.rs"),
                &[
                    "create_session",
                    "delete_session",
                    "rename_session",
                    "set_worktree_name",
                    "send_message",
                    "ensure_session",
                    "ensure_terminal",
                    "ensure_container_terminal",
                    "update_session_group",
                    "update_session_notifications",
                    "update_session_diff_base",
                    "update_session_pin",
                    "update_session_archive",
                    "update_session_snooze",
                    "update_workspace_ordering",
                ],
            ),
            ("api/git.rs", include_str!("git.rs"), &["clone_repo"]),
            (
                "api/mcp.rs",
                include_str!("mcp.rs"),
                &["resolve_mcp_conflict", "keep_mcp_server", "drop_mcp_server"],
            ),
            (
                "api/log_level.rs",
                include_str!("log_level.rs"),
                &["patch_log_level"],
            ),
            (
                "api/projects.rs",
                include_str!("projects.rs"),
                &["create_project", "delete_project", "update_project"],
            ),
            (
                "api/system.rs",
                include_str!("system.rs"),
                &[
                    "update_settings",
                    "create_profile",
                    "delete_profile",
                    "rename_profile",
                    "default_profile",
                    "update_profile_settings",
                ],
            ),
            (
                "api/acp.rs",
                include_str!("acp.rs"),
                &[
                    "spawn_acp",
                    "shutdown_acp",
                    "acp_prompt",
                    "acp_prompt_diff_comments",
                    "acp_cancel",
                    "acp_force_end_turn",
                    "acp_enable",
                    "acp_disable",
                    "acp_set_mode",
                    "acp_set_config_option",
                    "resolve_approval",
                ],
            ),
            (
                "api/telemetry.rs",
                include_str!("telemetry.rs"),
                &[
                    "set_telemetry_consent",
                    "post_telemetry_seen",
                    "post_telemetry_structured_interaction",
                ],
            ),
            (
                "server/push.rs",
                include_str!("../push.rs"),
                &["subscribe", "unsubscribe", "test"],
            ),
        ];

        let mut failures: Vec<String> = Vec::new();
        for (file_label, source, handler_names) in cases {
            for name in *handler_names {
                let needle = format!("fn {name}(");
                let Some(start) = source.find(&needle) else {
                    failures.push(format!(
                        "{file_label}: handler `{name}` not found (rename/refactor?)"
                    ));
                    continue;
                };
                let rest = &source[start..];
                // Signature spans `fn name(` ... `)` matching the opening
                // paren. Walk a depth counter so nested generics like
                // `Result<Json<T>, JsonRejection>` don't trip the close.
                let after_open = &rest[needle.len()..];
                let mut depth = 1usize;
                let mut end = None;
                for (i, c) in after_open.char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(i);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(end_off) = end else {
                    failures.push(format!(
                        "{file_label}: handler `{name}` signature parse failed"
                    ));
                    continue;
                };
                let signature = &after_open[..end_off];
                // Only handlers that take a JSON body need the lazy
                // pattern. If the signature mentions `Json<` at all,
                // the only safe forms are inside `Result<` or `Option<`.
                if !signature.contains("Json<") {
                    continue;
                }
                // Catch both eager forms:
                //   `Json(body): Json<T>`  -- pattern destructure
                //   `body: Json<T>`        -- typed parameter (still eager)
                // Either parameter triggers axum's extractor before the
                // handler body runs, defeating the read-only short-circuit.
                let has_eager = signature.split(',').any(|arg| {
                    let trimmed = arg.trim_start();
                    trimmed.starts_with("Json(") || trimmed.contains(": Json<")
                });
                if has_eager {
                    failures.push(format!(
                        "{file_label}: handler `{name}` uses eager JSON extraction. \
                         Mutating handlers must extract the body via \
                         `Result<Json<T>, axum::extract::rejection::JsonRejection>` (or \
                         `Option<Json<T>>`) so the read-only short-circuit can run \
                         before body shape validation. See #1229."
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "Lazy-body-extraction audit failed:\n{}",
            failures.join("\n")
        );
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

    // The settings PATCH write-surface pins (allowed sections, blocked session
    // fields, elevation surfaces) moved to
    // `crate::session::settings_schema::policy` when the curated constants were
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
