//! HTTP REST handlers for the web dashboard backend.
//!
//! Originally a single 2,151-line module; split into:
//!   - `sessions` — session CRUD, ensure-* lifecycle endpoints, and rich diff
//!   - `git`      — repo cloning and branch listing
//!   - `system`   — agents, settings, themes, profiles, filesystem,
//!     groups, docker, about, devices
//!   - this file  — shared validation helpers + module declarations and
//!     re-exports so external callers keep `api::*` paths.

pub(super) use super::AppState;

#[cfg(feature = "serve")]
mod client_log;
#[cfg(feature = "serve")]
mod cockpit;
mod git;
mod log_level;
mod projects;
mod sessions;
mod system;

#[cfg(feature = "serve")]
pub use cockpit::{
    cockpit_cancel, cockpit_context_primer, cockpit_disable, cockpit_enable, cockpit_files,
    cockpit_force_end_turn, cockpit_prompt, cockpit_replay, cockpit_set_mode, list_cockpit_agents,
    resolve_approval, set_cockpit_master, shutdown_cockpit, spawn_cockpit, switch_cockpit_agent,
};

#[cfg(feature = "serve")]
pub use client_log::post_client_log;
pub use git::{clone_repo, list_branches};
pub use log_level::{get_log_level, patch_log_level};
pub use projects::{create_project, delete_project, list_projects};
pub use sessions::{
    create_session, delete_session, ensure_container_terminal, ensure_session, ensure_terminal,
    list_sessions, read_output, rename_session, send_message, send_text_to_tmux_session,
    session_diff_file, session_diff_files, update_session_diff_base, update_session_notifications,
    update_workspace_ordering, CleanupDefaults, OutputQuery, SendMessageRequest, SendTextError,
    SendTextOutcome, SessionResponse,
};
pub use system::{
    browse_filesystem, create_profile, default_profile, delete_profile, docker_status,
    filesystem_home, get_about, get_current_theme, get_profile_settings, get_resolved_theme,
    get_settings, get_update_status, list_agents, list_devices, list_groups, list_profiles,
    list_sounds, list_themes, rename_profile, serve_sound_file, update_profile_settings,
    update_settings,
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

/// Sections that PATCH /api/settings (global config) may write.
///
/// Keep this list narrower than the profile-write list: anything here lands
/// in the process-global Config and is shared by every profile, so the bar
/// for inclusion is higher. `description` is intentionally absent because
/// it is a per-profile field (#949).
pub(super) const ALLOWED_GLOBAL_SETTINGS_SECTIONS: &[&str] = &[
    "theme", "session", "tmux", "updates", "sound", "sandbox", "worktree",
    // web: audited 2026-04-24, contains only boolean notification toggles
    // (notifications_enabled, notify_on_waiting, notify_on_idle, notify_on_error).
    // No shell commands, no binary paths, no RCE surface.
    "web",
    // logging: persistent tracing filter (default_level + per-target map).
    // No shell commands, no binary paths. Values are validated against the
    // EnvFilter parser before being written back to disk.
    "logging",
];

/// Sections that PATCH /api/settings/profile/:name may write.
///
/// Superset of the global list; adds `description`, which is a top-level
/// per-profile string field (Option<String>) surfaced as helper text in
/// the wizard profile picker (#949). Plain text only, no shell
/// metacharacters or binary paths.
pub(super) const ALLOWED_PROFILE_SETTINGS_SECTIONS: &[&str] = &[
    "theme",
    "session",
    "tmux",
    "updates",
    "sound",
    "sandbox",
    "worktree",
    "web",
    "logging",
    "description",
];

pub(super) const SESSION_BLOCKED_FIELDS: &[&str] = &[
    "agent_command_override",
    "agent_extra_args",
    "extra_env",
    // custom_agents maps names to arbitrary shell commands (e.g., "ssh -t host claude").
    // agent_detect_as maps names to detection targets but is part of the agent config
    // surface that should only be editable locally.
    "custom_agents",
    "agent_detect_as",
];

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
    //! Regression tests that pin security-critical constants.
    //!
    //! These three constants were silently rewritten in an earlier hand-
    //! assembled version of this split. The failure mode was specific: a
    //! refactor PR that claimed "no behavior changes" dropped 4 shell
    //! metacharacters (`#`, `[`, `]`, `~`) from the injection blocklist,
    //! added `"hooks"` to the settings-write allowlist (a hooks section
    //! set via the API runs arbitrary shell commands on session start;
    //! local RCE), and replaced the `SESSION_BLOCKED_FIELDS` contents
    //! with two field names that don't exist on `SessionConfig`,
    //! turning the blocklist into a no-op.
    //!
    //! Pin the contents here so the next refactor that touches this
    //! file fails CI instead of silently regressing security.
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
                    "send_message",
                    "ensure_session",
                    "ensure_terminal",
                    "ensure_container_terminal",
                    "update_session_notifications",
                    "update_session_diff_base",
                    "update_workspace_ordering",
                ],
            ),
            ("api/git.rs", include_str!("git.rs"), &["clone_repo"]),
            (
                "api/log_level.rs",
                include_str!("log_level.rs"),
                &["patch_log_level"],
            ),
            (
                "api/projects.rs",
                include_str!("projects.rs"),
                &["create_project", "delete_project"],
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
                "api/cockpit.rs",
                include_str!("cockpit.rs"),
                &[
                    "spawn_cockpit",
                    "shutdown_cockpit",
                    "cockpit_prompt",
                    "cockpit_cancel",
                    "cockpit_force_end_turn",
                    "cockpit_enable",
                    "cockpit_disable",
                    "cockpit_set_mode",
                    "resolve_approval",
                    "set_cockpit_master",
                ],
            ),
            (
                "server/push.rs",
                include_str!("../push.rs"),
                &["subscribe", "unsubscribe", "test"],
            ),
        ];

        let guard_patterns: &[&str] = &[
            "state.read_only",
            "self.read_only",
            // Cockpit handlers use the shared helper from api/cockpit.rs.
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
                    "send_message",
                    "ensure_session",
                    "ensure_terminal",
                    "ensure_container_terminal",
                    "update_session_notifications",
                    "update_session_diff_base",
                    "update_workspace_ordering",
                ],
            ),
            ("api/git.rs", include_str!("git.rs"), &["clone_repo"]),
            (
                "api/log_level.rs",
                include_str!("log_level.rs"),
                &["patch_log_level"],
            ),
            (
                "api/projects.rs",
                include_str!("projects.rs"),
                &["create_project", "delete_project"],
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
                "api/cockpit.rs",
                include_str!("cockpit.rs"),
                &[
                    "spawn_cockpit",
                    "shutdown_cockpit",
                    "cockpit_prompt",
                    "cockpit_cancel",
                    "cockpit_force_end_turn",
                    "cockpit_enable",
                    "cockpit_disable",
                    "cockpit_set_mode",
                    "resolve_approval",
                    "set_cockpit_master",
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
            "SHELL_METACHARACTERS size changed — every addition/removal must be \
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

    #[test]
    fn allowed_global_settings_sections_are_pinned() {
        // If you're adding a new top-level settings section, add it here AND
        // confirm the schema deserializes user input safely (no shell
        // commands that run on launch, no binary overrides). The `hooks`
        // section in particular must NOT be API-writable because global
        // hooks bypass the trust prompt that gates repo hooks.
        //
        // Global allowlist is intentionally narrower than the profile
        // allowlist: profile-only fields (e.g., `description`) must not be
        // accepted by PATCH /api/settings, only by the per-profile endpoint.
        let expected: &[&str] = &[
            "theme", "session", "tmux", "updates", "sound", "sandbox", "worktree",
            // web: audited 2026-04-24. WebConfig has 4 boolean fields
            // (notifications_enabled, notify_on_waiting, notify_on_idle,
            // notify_on_error). No shell commands, no binary paths.
            "web",
            // logging: persistent tracing filter. EnvFilter parser
            // validates every value before save_config writes it back.
            "logging",
        ];
        assert_eq!(
            ALLOWED_GLOBAL_SETTINGS_SECTIONS.len(),
            expected.len(),
            "ALLOWED_GLOBAL_SETTINGS_SECTIONS size changed — adding a section widens \
             the API write surface and must be reviewed as a security change. \
             In particular, do NOT add 'hooks' without auditing the RCE surface."
        );
        for section in expected {
            assert!(
                ALLOWED_GLOBAL_SETTINGS_SECTIONS.contains(section),
                "ALLOWED_GLOBAL_SETTINGS_SECTIONS lost section {:?}",
                section
            );
        }
        // Explicitly guard against accidental hooks re-addition.
        assert!(
            !ALLOWED_GLOBAL_SETTINGS_SECTIONS.contains(&"hooks"),
            "hooks must not be API-writable: global/profile hooks bypass the \
             repo-hook trust prompt and run arbitrary shell commands on session \
             start (local RCE)"
        );
        // `description` is a per-profile field and must not appear on the
        // global endpoint, even though it is plain text and safe in itself.
        assert!(
            !ALLOWED_GLOBAL_SETTINGS_SECTIONS.contains(&"description"),
            "description is a per-profile field and must only be writable via \
             PATCH /api/settings/profile/:name, not the global endpoint"
        );
    }

    #[test]
    fn allowed_profile_settings_sections_are_pinned() {
        // Profile allowlist is the global list plus `description`. Anything
        // global can also be set per-profile (overrides); profile-only fields
        // (`description` today) are the additions.
        let expected: &[&str] = &[
            "theme",
            "session",
            "tmux",
            "updates",
            "sound",
            "sandbox",
            "worktree",
            "web",
            "logging",
            // description: optional string surfaced in the wizard profile
            // picker (#949). Plain text, no shell metacharacters.
            "description",
        ];
        assert_eq!(
            ALLOWED_PROFILE_SETTINGS_SECTIONS.len(),
            expected.len(),
            "ALLOWED_PROFILE_SETTINGS_SECTIONS size changed — adding a section \
             widens the API write surface and must be reviewed as a security change."
        );
        for section in expected {
            assert!(
                ALLOWED_PROFILE_SETTINGS_SECTIONS.contains(section),
                "ALLOWED_PROFILE_SETTINGS_SECTIONS lost section {:?}",
                section
            );
        }
        assert!(
            !ALLOWED_PROFILE_SETTINGS_SECTIONS.contains(&"hooks"),
            "hooks must not be API-writable on any endpoint"
        );
        // Every global section must also be writable per-profile (overrides).
        for section in ALLOWED_GLOBAL_SETTINGS_SECTIONS {
            assert!(
                ALLOWED_PROFILE_SETTINGS_SECTIONS.contains(section),
                "global section {:?} must also be accepted by the per-profile endpoint",
                section
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

    #[test]
    fn session_blocked_fields_are_pinned() {
        // These fields let an API caller swap the agent binary,
        // append arbitrary argv, inject environment variables, or define
        // custom agent commands — all command-injection vectors. If Rust
        // renames a field it must be renamed here in the same commit.
        let expected: &[&str] = &[
            "agent_command_override",
            "agent_extra_args",
            "extra_env",
            // custom_agents: maps agent names to arbitrary shell commands
            "custom_agents",
            // agent_detect_as: part of the agent config surface
            "agent_detect_as",
        ];
        assert_eq!(
            SESSION_BLOCKED_FIELDS.len(),
            expected.len(),
            "SESSION_BLOCKED_FIELDS size changed — this is the blocklist \
             that strips attacker-supplied command-injection vectors from \
             incoming /api/settings session objects. Changes must be \
             reviewed as a security change."
        );
        for field in expected {
            assert!(
                SESSION_BLOCKED_FIELDS.contains(field),
                "SESSION_BLOCKED_FIELDS lost field {:?}",
                field
            );
        }
    }
}
