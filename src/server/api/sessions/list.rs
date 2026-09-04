//! Listing, recent projects, and workspace ordering endpoints.

use super::*;

#[derive(serde::Serialize)]
pub struct RecentProjectsResponse {
    pub projects: Vec<crate::session::RecentProjectEntry>,
}

/// Persisted recent projects for the new-session wizard, newest first.
/// Read-time pruning drops entries whose directory no longer exists; the
/// stored file (capped at write time) is left untouched, so a GET stays
/// side-effect free.
pub async fn get_recent_projects() -> Json<RecentProjectsResponse> {
    let projects = crate::session::load_recent_projects()
        .unwrap_or_else(|e| {
            tracing::warn!(target: "http.api.sessions", "failed to load recent projects: {e}");
            Vec::new()
        })
        .into_iter()
        .filter(|p| std::path::Path::new(&p.path).is_dir())
        .collect();
    Json(RecentProjectsResponse { projects })
}

/// Query params for `GET /api/sessions`. `state` shares its vocabulary with
/// the CLI's `aoe list --state` via [`crate::session::SessionScope`] so a
/// future third caller cannot drift.
#[derive(Deserialize)]
pub struct ListSessionsQuery {
    pub state: Option<crate::session::SessionScope>,
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListSessionsQuery>,
) -> Json<SessionsEnvelope> {
    let instances = state.instances.read().await;
    let claude_fullscreen = crate::claude_settings::read_tui_fullscreen();
    // Snapshot the supervisor's worker lifecycle map once per request
    // rather than locking it per row. See #1088.
    let worker_states = state.acp_supervisor.worker_states_snapshot().await;
    // Filtered once up front; every positional zip with `instances` below
    // (ACP capability overlay, smart-rename overlay) must walk this same
    // filtered view so indices stay aligned with `sessions`.
    let scoped_instances: Vec<&Instance> = instances
        .iter()
        // CityHall only ever creates structured sessions; a plain/terminal
        // session (from the TUI, `aoe add`, or another client on the same
        // daemon) must not be visible or actionable to a locked-down client, so
        // it never appears in the list. The lifecycle routes apply the matching
        // structured-target gate. See #7.
        .filter(|inst| !state.cityhall_mode || inst.is_structured())
        .filter(|inst| crate::session::SessionScope::matches(query.state, inst))
        .collect();
    let mut sessions: Vec<SessionResponse> = scoped_instances
        .iter()
        .copied()
        .map(|inst| {
            let plan_summary = if inst.is_structured() {
                state
                    .acp_event_store
                    .latest_plan(&inst.id)
                    .map(plan_summary_from_plan)
            } else {
                None
            };
            // Archived sessions are sunk and not live; their wakeup/monitor
            // badge is meaningless, so skip the per-poll SQLite lookups for
            // them. Unarchiving restores the queries. latest_plan stays
            // ungated: a collapsed archived row may still show a plan summary.
            let structured_live = inst.is_structured() && !inst.is_archived() && !inst.is_trashed();
            let (next_wakeup_at, next_wakeup_reason) = if structured_live {
                match state.acp_event_store.latest_pending_wakeup(&inst.id) {
                    Some((at, reason)) => (Some(at.to_rfc3339()), reason),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            let active_monitor = if structured_live {
                state.acp_event_store.latest_active_monitor(&inst.id)
            } else {
                None
            };
            let acp_worker_state = worker_states
                .get(&inst.id)
                .copied()
                .unwrap_or(crate::acp::supervisor::AcpWorkerState::Absent);
            SessionResponse::from_instance_with_plan(
                inst,
                claude_fullscreen,
                plan_summary,
                acp_worker_state,
                next_wakeup_at,
                next_wakeup_reason,
                active_monitor,
            )
        })
        .collect();

    // Shared per-request cache of the resolved `SessionConfig` keyed by
    // (profile, project_path). Both the ACP-capability overlay (serve-only)
    // and the smart-rename indicator overlay below fetch through this one
    // cache, halving the disk reads the 3s sidebar poll does when the same
    // pair appears in more than one row. See #2603.
    let mut session_cfg_cache: HashMap<(String, String), SessionConfig> = HashMap::new();

    // Overlay custom-agent ACP capability (built-ins were resolved in the
    // constructor). Distinct `(profile, project_path)` pairs each resolve
    // once via the shared cache above.
    for (resp, inst) in sessions.iter_mut().zip(scoped_instances.iter().copied()) {
        if resp.acp_capable {
            continue;
        }
        let cfg = resolve_session_cfg(
            &mut session_cfg_cache,
            &inst.source_profile,
            &inst.project_path,
        );
        resp.acp_capable = custom_agent_acp_capable(cfg, &inst.tool);
    }

    // Resolve per-profile cleanup defaults with a TTL cache on AppState
    let cache = {
        let guard = state.cleanup_defaults_cache.read().await;
        if guard.stale() {
            None
        } else {
            Some(guard.entries.clone())
        }
    };

    let defaults_map = if let Some(cached) = cache {
        cached
    } else {
        use std::collections::HashMap;
        let mut fresh: HashMap<String, CleanupDefaults> = HashMap::new();
        for session in &sessions {
            fresh.entry(session.profile.clone()).or_insert_with(|| {
                let cfg = crate::session::config::profile_config::resolve_config_or_warn(
                    &session.profile,
                );
                CleanupDefaults {
                    delete_worktree: cfg.worktree.auto_cleanup,
                    delete_branch: cfg.worktree.should_delete_branch_on_cleanup(),
                    delete_sandbox: cfg.sandbox.auto_cleanup,
                    delete_to_trash: cfg.session.delete_to_trash,
                }
            });
        }
        *state.cleanup_defaults_cache.write().await = crate::server::CleanupDefaultsCache {
            refreshed_at: std::time::Instant::now(),
            entries: fresh.clone(),
        };
        fresh
    };

    // Overlay the per-profile tie setting (#1927) so the sidebar can collapse
    // the standalone workdir action for tied worktree sessions. Resolved once
    // per distinct profile, not per session.
    {
        use std::collections::HashMap;
        let mut tie_cache: HashMap<String, bool> = HashMap::new();
        for session in &mut sessions {
            if !session.has_managed_worktree {
                continue;
            }
            let tied = *tie_cache.entry(session.profile.clone()).or_insert_with(|| {
                crate::session::config::profile_config::resolve_config_or_warn(&session.profile)
                    .session
                    .tie_workdir_to_name
            });
            session.tie_workdir_to_name = tied;
        }
    }

    // Overlay the smart-rename indicator. `Running` comes from the live
    // in-flight set; `Pending` from the shared eligibility predicate, so the
    // indicator cannot drift from the runtime gate. Config is projected from
    // the shared `session_cfg_cache` above so a repo-local override resolves
    // once per unique `(profile, project_path)` across both overlays.
    {
        use crate::session::smart_rename::{
            check_eligible_resolved, resolve_smart_rename_config, SmartRenameState,
        };
        use std::collections::HashSet;
        let inflight: HashSet<String> = state
            .smart_rename_inflight
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let attempted: HashSet<String> = state
            .smart_rename_attempted
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        for (resp, inst) in sessions.iter_mut().zip(scoped_instances.iter().copied()) {
            resp.default_name = crate::session::civilizations::is_default_civ_name(&inst.title);
            if inflight.contains(&inst.id) {
                resp.smart_rename = SmartRenameState::Running;
                continue;
            }
            // A session whose one-shot already ran (and failed, since the name
            // is still default) will not retry, so it is not pending either.
            if attempted.contains(&inst.id) {
                continue;
            }
            let session_cfg = resolve_session_cfg(
                &mut session_cfg_cache,
                &inst.source_profile,
                &inst.project_path,
            );
            let cfg = resolve_smart_rename_config(session_cfg);
            let eligible = check_eligible_resolved(
                inst.is_structured(),
                cfg.setting_on,
                &inst.title,
                &inst.tool,
                cfg.rename_agent,
                inst.is_sandboxed(),
                &inst.command,
                cfg.overrides,
            )
            .is_ok();
            if eligible {
                resp.smart_rename = SmartRenameState::Pending;
            }
        }
    }

    // Resolve remote owners with a permanent cache on AppState
    {
        let cache = state.remote_owner_cache.read().await;
        for session in &mut sessions {
            if let Some(defaults) = defaults_map.get(&session.profile) {
                session.cleanup_defaults = defaults.clone();
            }
            let repo_path = session
                .main_repo_path
                .as_deref()
                .unwrap_or(&session.project_path);
            if let Some(resolved) = cache.get(repo_path) {
                session.remote_owner = resolved.as_ref().map(|(owner, _)| owner.clone());
                session.remote_owner_key = resolved.as_ref().map(|(_, key)| key.clone());
            }
        }
    }

    // Fill any uncached repo paths
    let uncached: Vec<String> = sessions
        .iter()
        .filter(|s| s.remote_owner.is_none())
        .map(|s| {
            s.main_repo_path
                .clone()
                .unwrap_or_else(|| s.project_path.clone())
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if !uncached.is_empty() {
        let mut cache = state.remote_owner_cache.write().await;
        for path in &uncached {
            if !cache.contains_key(path.as_str()) {
                let resolved = crate::git::get_remote_owner_with_key(std::path::Path::new(path));
                cache.insert(path.clone(), resolved);
            }
        }
        for session in &mut sessions {
            let repo_path = session
                .main_repo_path
                .as_deref()
                .unwrap_or(&session.project_path);
            if session.remote_owner.is_none() {
                if let Some(resolved) = cache.get(repo_path) {
                    session.remote_owner = resolved.as_ref().map(|(owner, _)| owner.clone());
                    session.remote_owner_key = resolved.as_ref().map(|(_, key)| key.clone());
                }
            }
        }
    }

    let workspace_ordering =
        merge_workspace_ordering(&sessions, state.read_only).unwrap_or_else(|e| {
            tracing::error!(target: "http.api.sessions", "Failed to merge workspace ordering: {e}");
            Vec::new()
        });

    Json(SessionsEnvelope {
        sessions,
        workspace_ordering,
    })
}
// Workspace id derivation. Mirrors the client logic in `useWorkspaces.ts`:
// a session with a branch collapses to `${repoPath}::${branch}`; a
// branchless session gets its own workspace at `${repoPath}::__session__::${id}`.
// `repoPath` strips trailing slashes so the server and client compute the
// same string for the same session row.
fn workspace_id_for_session(s: &SessionResponse) -> String {
    let raw = s.main_repo_path.as_deref().unwrap_or(&s.project_path);
    let repo_path = raw.trim_end_matches('/');
    match &s.branch {
        Some(branch) => format!("{repo_path}::{branch}"),
        None => format!("{repo_path}::__session__::{}", s.id),
    }
}

// Prepend any workspace id we haven't seen before to the persisted
// ordering and return the merged list. Done server-side so concurrent
// clients (multiple tabs, multiple devices) converge on a single
// ordering without each racing to PUT their own prepend. In read-only
// mode we still compute the merge for the response, but we skip the
// disk write.
// Pure helper: merges newly observed workspace ids on top of the
// existing ordering, deduplicating and putting unknowns first
// (newest-first). Extracted so the merge math can run from both the
// read-only path (no lock) and the locked closure (where it operates
// on `ord.order` directly to avoid the read-modify-write race that
// `merge_workspace_ordering` originally had on a pre-lock snapshot).
fn compute_merged_ordering(sessions: &[SessionResponse], current_order: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = current_order.iter().map(String::as_str).collect();
    let mut seen_unknown: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut new_ids: Vec<String> = Vec::new();
    for s in sessions {
        let id = workspace_id_for_session(s);
        if known.contains(id.as_str()) {
            continue;
        }
        if seen_unknown.insert(id.clone()) {
            new_ids.push(id);
        }
    }
    if new_ids.is_empty() {
        return current_order.to_vec();
    }
    new_ids.reverse();
    new_ids.extend_from_slice(current_order);
    new_ids
}

fn merge_workspace_ordering(
    sessions: &[SessionResponse],
    read_only: bool,
) -> anyhow::Result<Vec<String>> {
    if read_only {
        let current = crate::session::load_workspace_ordering()
            .map(|w| w.order)
            .unwrap_or_default();
        return Ok(compute_merged_ordering(sessions, &current));
    }
    crate::session::update_workspace_ordering(|ord| {
        let merged = compute_merged_ordering(sessions, &ord.order);
        ord.order = merged.clone();
        Ok(merged)
    })
}

// --- Workspace ordering ---
//
// `PUT /api/workspace-ordering` overwrites the persisted workspace order
// with a fresh client-supplied list. Workspaces are a client construct
// (a group of sessions keyed on `repoPath::branch`), so the server
// treats the entries as opaque strings. New workspaces are folded in
// server-side by `merge_workspace_ordering` on every `GET /api/sessions`,
// so the file always covers every observed workspace; this PUT just
// reorders existing entries. Persisted globally (not per-profile)
// because the sidebar shows sessions across all profiles. See #1169.

// Caps on the inbound body. The order list is one entry per workspace
// row and workspaces map 1:1 to sessions in the worst case, so 4096 is
// comfortably above any realistic ceiling. Per-entry cap covers a
// long repo path plus a long branch name; ids longer than this can't
// come from the client's workspace id derivation in any sane setup.
const MAX_ORDER_ENTRIES: usize = 4096;
const MAX_ORDER_ENTRY_LEN: usize = 1024;

#[derive(Deserialize)]
pub struct UpdateWorkspaceOrderingBody {
    pub order: Vec<String>,
}

pub async fn update_workspace_ordering(
    State(state): State<Arc<AppState>>,
    body: Result<Json<UpdateWorkspaceOrderingBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    if body.order.len() > MAX_ORDER_ENTRIES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "message": format!("order has {} entries, max is {}", body.order.len(), MAX_ORDER_ENTRIES)
            })),
        )
            .into_response();
    }
    if let Some(bad) = body.order.iter().find(|e| e.len() > MAX_ORDER_ENTRY_LEN) {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "message": format!("order entry is {} bytes, max is {}", bad.len(), MAX_ORDER_ENTRY_LEN)
            })),
        )
            .into_response();
    }

    let new_order = body.order;
    let result = crate::session::update_workspace_ordering(|ord| {
        ord.order = new_order.clone();
        Ok(())
    });
    if let Err(e) = result {
        tracing::error!(target: "http.api.sessions", "Failed to persist workspace ordering: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "message": "Failed to persist ordering" })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "order": new_order })),
    )
        .into_response()
}

#[cfg(test)]
mod context_resume_tests {
    use super::*;

    #[test]
    fn context_resume_projects_structured_and_terminal_states() {
        let mut structured = Instance::new("structured", "/tmp/structured");
        structured.view = crate::session::View::Structured;
        assert_eq!(
            context_resume_for(&structured),
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }
        );

        structured.acp_session_id = Some("opaque-server-target".to_string());
        assert_eq!(
            context_resume_for(&structured),
            ContextResumeAvailability::Indeterminate {
                reason: ContextResumeIndeterminateReason::AgentHandshakeRequired,
            }
        );

        structured.fork_pending = Some("opaque-parent".to_string());
        assert_eq!(
            context_resume_for(&structured),
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::ForkPending,
            }
        );

        let mut terminal = Instance::new("terminal", "/tmp/terminal");
        terminal.tool = "claude".to_string();
        terminal.agent_session_id = Some("terminal-context".to_string());
        assert_eq!(
            context_resume_for(&terminal),
            ContextResumeAvailability::Indeterminate {
                reason: ContextResumeIndeterminateReason::RuntimeCheckRequired,
            }
        );
    }
}

#[cfg(test)]
mod workspace_ordering_tests {
    use super::*;
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup_test_home(temp: &std::path::Path) -> AppDirGuard {
        isolate_app_dir_at(temp)
    }

    fn mock_response(id: &str, project_path: &str, branch: Option<&str>) -> SessionResponse {
        SessionResponse {
            id: id.to_string(),
            title: id.to_string(),
            project_path: project_path.to_string(),
            artifact_dir: String::new(),
            group_path: String::new(),
            tool: "claude".to_string(),
            status: "Idle".to_string(),
            dormant: false,
            yolo_mode: false,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            branch: branch.map(str::to_string),
            main_repo_path: None,
            base_branch: None,
            base_branch_override: None,
            is_sandboxed: false,
            scratch: false,
            has_managed_worktree: false,
            has_cleanable_worktree: false,
            tie_workdir_to_name: false,
            smart_rename: crate::session::smart_rename::SmartRenameState::Inactive,
            default_name: false,
            has_terminal: false,
            profile: "default".to_string(),
            cleanup_defaults: CleanupDefaults {
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                delete_to_trash: true,
            },
            trashed_at: None,
            remote_owner: None,
            remote_owner_key: None,
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            view: crate::session::View::Terminal,
            context_resume: ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            },
            acp_worker_state: crate::acp::supervisor::AcpWorkerState::Absent,
            queued_prompts: Vec::new(),
            acp_capable: false,
            acp_session_id: None,
            acp_agent: None,
            acp_can_fork: false,
            keeps_context: false,
            clear_aliases: Vec::new(),
            claude_fullscreen: false,
            workspace_repos: Vec::new(),
            warnings: Vec::new(),
            plan_summary: None,
            next_wakeup_at: None,
            next_wakeup_reason: None,
            monitor_active: false,
            monitor_description: None,
            favorited: false,
            color: None,
            urgent: false,
            pinned_at: None,
            archived_at: None,
            snoozed_until: None,
            unread: false,
        }
    }

    #[test]
    fn id_uses_branch_when_present() {
        let r = mock_response("s1", "/tmp/repo", Some("feature/x"));
        assert_eq!(workspace_id_for_session(&r), "/tmp/repo::feature/x");
    }

    #[test]
    fn id_falls_back_to_session_id_when_branchless() {
        let r = mock_response("abc123", "/tmp/repo", None);
        assert_eq!(
            workspace_id_for_session(&r),
            "/tmp/repo::__session__::abc123"
        );
    }

    #[test]
    fn id_strips_trailing_slash() {
        // The client's `useWorkspaces.normalizePath` strips trailing
        // slashes. Server must match so the merged ordering keys line up.
        let r = mock_response("s1", "/tmp/repo/", Some("main"));
        assert_eq!(workspace_id_for_session(&r), "/tmp/repo::main");
    }

    #[test]
    fn id_prefers_main_repo_path_over_project_path() {
        let mut r = mock_response("s1", "/tmp/worktree", Some("main"));
        r.main_repo_path = Some("/tmp/repo".to_string());
        assert_eq!(workspace_id_for_session(&r), "/tmp/repo::main");
    }

    #[test]
    #[serial]
    fn merge_prepends_unseen_newest_first() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        // Persisted ordering already contains `b`. Sessions come in
        // creation order (oldest first) `[b, a, c]`; `a` and `c` are
        // unseen and should land at the top in newest-first order: `[c, a, b]`.
        crate::session::update_workspace_ordering(|ord| {
            ord.order = vec!["/tmp/repo::b".to_string()];
            Ok(())
        })?;

        let sessions = vec![
            mock_response("sb", "/tmp/repo", Some("b")),
            mock_response("sa", "/tmp/repo", Some("a")),
            mock_response("sc", "/tmp/repo", Some("c")),
        ];

        let merged = merge_workspace_ordering(&sessions, /* read_only */ false)?;
        assert_eq!(
            merged,
            vec![
                "/tmp/repo::c".to_string(),
                "/tmp/repo::a".to_string(),
                "/tmp/repo::b".to_string(),
            ]
        );

        // And the merge was persisted.
        let on_disk = crate::session::load_workspace_ordering()?;
        assert_eq!(on_disk.order, merged);

        Ok(())
    }

    #[test]
    #[serial]
    fn merge_dedupes_within_a_single_request() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        // Two sessions on the same workspace (rare but legal: multiple
        // agents in one worktree). The workspace id appears once.
        let sessions = vec![
            mock_response("sa1", "/tmp/repo", Some("main")),
            mock_response("sa2", "/tmp/repo", Some("main")),
        ];

        let merged = merge_workspace_ordering(&sessions, false)?;
        assert_eq!(merged, vec!["/tmp/repo::main".to_string()]);
        Ok(())
    }

    #[test]
    #[serial]
    fn merge_no_op_when_all_known() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        crate::session::update_workspace_ordering(|ord| {
            ord.order = vec!["/tmp/repo::a".to_string(), "/tmp/repo::b".to_string()];
            Ok(())
        })?;

        let sessions = vec![
            mock_response("sa", "/tmp/repo", Some("a")),
            mock_response("sb", "/tmp/repo", Some("b")),
        ];

        let merged = merge_workspace_ordering(&sessions, false)?;
        assert_eq!(
            merged,
            vec!["/tmp/repo::a".to_string(), "/tmp/repo::b".to_string()]
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn merge_read_only_returns_merged_but_does_not_write() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        // Empty starting state. Read-only request observes a new
        // workspace; the response includes it but disk is untouched.
        let sessions = vec![mock_response("sa", "/tmp/repo", Some("a"))];

        let merged = merge_workspace_ordering(&sessions, /* read_only */ true)?;
        assert_eq!(merged, vec!["/tmp/repo::a".to_string()]);

        let on_disk = crate::session::load_workspace_ordering()?;
        assert!(on_disk.order.is_empty(), "read-only path must not persist");

        Ok(())
    }

    #[test]
    fn compute_merged_ordering_pure_no_known_ids() {
        let sessions = vec![
            mock_response("s1", "/repo/a", Some("main")),
            mock_response("s2", "/repo/b", Some("dev")),
        ];
        let merged = compute_merged_ordering(&sessions, &[]);
        assert_eq!(
            merged,
            vec!["/repo/b::dev".to_string(), "/repo/a::main".to_string()]
        );
    }

    #[test]
    fn compute_merged_ordering_pure_dedupes_unknowns() {
        let sessions = vec![
            mock_response("s1", "/repo/a", Some("main")),
            mock_response("s2", "/repo/a", Some("main")),
            mock_response("s3", "/repo/b", Some("dev")),
        ];
        let merged = compute_merged_ordering(&sessions, &[]);
        assert_eq!(merged.len(), 2);
        assert!(merged.contains(&"/repo/a::main".to_string()));
        assert!(merged.contains(&"/repo/b::dev".to_string()));
    }

    #[test]
    fn compute_merged_ordering_pure_preserves_existing_order() {
        let existing = vec!["/repo/x::main".to_string(), "/repo/y::dev".to_string()];
        let sessions = vec![mock_response("s1", "/repo/z", Some("feat"))];
        let merged = compute_merged_ordering(&sessions, &existing);
        assert_eq!(
            merged,
            vec![
                "/repo/z::feat".to_string(),
                "/repo/x::main".to_string(),
                "/repo/y::dev".to_string(),
            ]
        );
    }

    #[test]
    fn compute_merged_ordering_pure_returns_existing_when_all_known() {
        let existing = vec!["/repo/x::main".to_string(), "/repo/y::dev".to_string()];
        let sessions = vec![
            mock_response("s1", "/repo/x", Some("main")),
            mock_response("s2", "/repo/y", Some("dev")),
        ];
        let merged = compute_merged_ordering(&sessions, &existing);
        assert_eq!(merged, existing);
    }
}
