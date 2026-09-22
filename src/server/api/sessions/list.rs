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

pub(crate) async fn project_sessions(state: &Arc<AppState>) -> Vec<SessionResponse> {
    let instances = state.instances.read().await;
    let claude_fullscreen = crate::claude_settings::read_tui_fullscreen();
    let worker_states = state.acp_supervisor.worker_states_snapshot().await;
    // Every positional overlay shares this filtered view.
    let scoped_instances: Vec<&Instance> = instances
        .iter()
        // CityHall cannot expose terminal sessions.
        .filter(|inst| !state.cityhall_mode || inst.is_structured())
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
                .unwrap_or(crate::daemon::AcpWorkerState::Absent);
            let mut session = SessionResponse::from_instance_with_plan(
                inst,
                claude_fullscreen,
                plan_summary,
                acp_worker_state,
                next_wakeup_at,
                next_wakeup_reason,
                active_monitor,
            );
            if structured_live && acp_worker_state == crate::daemon::AcpWorkerState::Running {
                // Gate on a live worker: the invariant (supervisor.rs) is that
                // a pending nonce only exists on a running worker, and
                // `spawn`/`attach` sweep orphaned nonces out of the durable
                // log. Projecting a non-running row would surface a phantom
                // approval the resolver can only 404 on. Also skips the
                // per-session SQLite scan for every non-running structured row.
                session.pending_approvals = state
                    .acp_event_store
                    .pending_approval_requests(&inst.id)
                    .into_iter()
                    .map(|approval| PendingApproval {
                        nonce: approval.nonce.0,
                        target: crate::acp::approvals::summarize_target(
                            &approval.tool_call.kind,
                            &approval.tool_call.args_preview,
                        ),
                        tool_name: approval.tool_call.name,
                        destructive: approval.destructive,
                        choice: approval.choice,
                    })
                    .collect();
            }
            session
        })
        .collect();

    // Share resolved config between the ACP-capability and smart-rename
    // overlays, halving disk reads when a profile/project pair repeats in the
    // 3s sidebar poll. See #2603.
    // Monotonic, so the delta below is this request's own count and no reset
    // can race a concurrent request on the same state.
    let misses_before = state
        .list_sessions_resolver_misses
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut session_cfg_cache = SessionCfgCache::new(&state.list_sessions_resolver_misses);
    let mut project_override_cache = ProjectRegistryCache::new();

    // Overlay custom-agent ACP capability (built-ins were resolved in the
    // constructor). Distinct `(profile, project_path)` pairs each resolve
    // once via the shared cache above.
    for (resp, inst) in sessions.iter_mut().zip(scoped_instances.iter().copied()) {
        if resp.acp_capable {
            continue;
        }
        let cfg = session_cfg_cache.resolve(&inst.source_profile, &inst.project_path);
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

    // Inputs for the rate-limit park overlay below, snapshotted here so the
    // blocking batch can run once the registry read lock is released. A live
    // worker is never parked, so only workerless sessions pay for the probe.
    let park_probes: Vec<(usize, String, String, bool)> = sessions
        .iter()
        .zip(scoped_instances.iter().copied())
        .enumerate()
        .filter(|(_, (_, inst))| inst.is_structured() && !inst.is_archived() && !inst.is_trashed())
        .map(|(i, (resp, inst))| {
            (
                i,
                inst.id.clone(),
                inst.source_profile.clone(),
                resp.acp_worker_state != crate::daemon::AcpWorkerState::Running,
            )
        })
        .collect();

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
            let session_cfg = session_cfg_cache.resolve(&inst.source_profile, &inst.project_path);
            let smart_rename_override = project_override_cache
                .smart_rename_override(&inst.source_profile, inst.repo_path());
            let cfg = resolve_smart_rename_config(session_cfg, smart_rename_override);
            let eligible = check_eligible_resolved(
                inst.is_structured(),
                cfg.setting_on,
                false,
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

    // Both overlays have run, so the count is final for this request.
    let resolver_misses = state
        .list_sessions_resolver_misses
        .load(std::sync::atomic::Ordering::Relaxed)
        .saturating_sub(misses_before);
    tracing::debug!(
        target: "http.api.sessions",
        rows = sessions.len(),
        resolver_misses,
        "list_sessions resolved session config once per unique profile/project pair"
    );

    // The park probe touches config files and SQLite; run it with the
    // session registry unlocked so writers are not held behind it.
    drop(scoped_instances);
    drop(instances);
    if !park_probes.is_empty() {
        let store = Arc::clone(&state.acp_event_store);
        let overlays = tokio::task::spawn_blocking(move || {
            use std::collections::HashMap;
            let mut auto_resume_cache: HashMap<String, bool> = HashMap::new();
            park_probes
                .into_iter()
                .map(|(i, id, profile, workerless)| {
                    let auto_resume =
                        *auto_resume_cache.entry(profile.clone()).or_insert_with(|| {
                            crate::session::config::profile_config::resolve_config_or_warn(&profile)
                                .acp
                                .rate_limit_auto_resume
                        });
                    let park = workerless
                        .then(|| {
                            store.rate_limit_park(&id).map(|park| {
                                park.info
                                    .unwrap_or_else(crate::acp::state::RateLimitInfo::undated)
                            })
                        })
                        .flatten();
                    (i, auto_resume, park)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (i, auto_resume, park) in overlays {
            sessions[i].rate_limit_auto_resume = Some(auto_resume);
            sessions[i].rate_limit = park;
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

    sessions
}

struct ScopedSessions<'a> {
    rows: &'a [SessionResponse],
    scope: Option<crate::session::SessionScope>,
}

impl serde::Serialize for ScopedSessions<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.rows.iter().filter(|row| {
            crate::session::SessionScope::matches(
                self.scope,
                row.archived_at.is_some(),
                row.trashed_at.is_some(),
            )
        }))
    }
}

#[derive(serde::Serialize)]
struct SessionsView<'a> {
    sessions: ScopedSessions<'a>,
    workspace_ordering: &'a [String],
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListSessionsQuery>,
) -> axum::response::Response {
    let snapshot = match state.runtime.snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(SessionsView {
        sessions: ScopedSessions {
            rows: &snapshot.value.contents.sessions,
            scope: query.state,
        },
        workspace_ordering: &snapshot.value.contents.workspace_ordering,
    })
    .into_response()
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

// Unknown workspaces precede the manual order, newest first.
pub(crate) fn compute_merged_ordering(
    sessions: &[SessionResponse],
    current_order: &[String],
) -> Vec<String> {
    let known: std::collections::HashSet<&str> = current_order.iter().map(String::as_str).collect();
    let mut new_ids = indexmap::IndexSet::new();
    for session in sessions {
        let id = workspace_id_for_session(session);
        if !known.contains(id.as_str()) {
            new_ids.insert(id);
        }
    }
    new_ids
        .into_iter()
        .rev()
        .chain(current_order.iter().cloned())
        .collect()
}

const MAX_ORDER_ENTRIES: usize = 4096;
const MAX_ORDER_ENTRY_LEN: usize = 1024;

#[derive(Deserialize)]
pub struct UpdateWorkspaceOrderingBody {
    pub order: Vec<String>,
}

#[derive(serde::Serialize)]
struct WorkspaceOrderingResponse<'a> {
    order: &'a [String],
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

    let namespace = state.profile_namespace.read().await;
    let publication = state.publication.write().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_workspace_ordering(move |ordering| {
            ordering.order = body.order;
            Ok(())
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(std::convert::identity);
    let (_, ordering) = match result {
        Ok(committed) => committed,
        Err(error) => {
            tracing::error!(target: "http.api.sessions", %error, "workspace ordering commit failed");
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
            state.runtime.request_publish();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "message": "Failed to persist ordering" })),
            )
                .into_response();
        }
    };
    state.canonical_metadata.write().await.workspace_ordering = ordering.order;
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.runtime.request_publish();
    drop(publication);
    drop(namespace);
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    crate::server::runtime::mutation_response(
        &snapshot.value.cursor,
        Json(WorkspaceOrderingResponse {
            order: &snapshot.value.contents.workspace_ordering,
        }),
    )
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

        structured.acp_load_session_capable = Some(false);
        assert_eq!(
            context_resume_for(&structured),
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::AgentUnsupported,
            }
        );

        structured.acp_load_session_capable = Some(true);
        assert_eq!(
            context_resume_for(&structured),
            ContextResumeAvailability::Available
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
            agent_pane: Default::default(),
            auxiliary: Vec::new(),
            id: id.to_string(),
            title: id.to_string(),
            project_path: project_path.to_string(),
            artifact_dir: String::new(),
            group_path: String::new(),
            tool: "claude".to_string(),
            command: String::new(),
            extra_args: String::new(),
            status: "Idle".to_string(),
            lifecycle_reservation: None,
            lifecycle_generation: 0,
            dormant: false,
            idle_dormant_since: None,
            pane_dead_observed: false,
            yolo_mode: false,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            branch: branch.map(str::to_string),
            main_repo_path: None,
            base_branch: None,
            base_branch_override: None,
            worktree_created_at: None,
            is_sandboxed: false,
            sandbox_container_name: None,
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
            pending_approvals: Vec::new(),
            acp_worker_state: crate::daemon::AcpWorkerState::Absent,
            context_resume: Some(ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }),
            rate_limit: None,
            rate_limit_auto_resume: None,
            queued_prompts: Vec::new(),
            acp_capable: false,
            acp_session_id: None,
            acp_agent: None,
            acp_can_fork: false,
            keeps_context: false,
            clear_aliases: Vec::new(),
            claude_fullscreen: false,
            workspace_repos: Vec::new(),
            workspace_dir: None,
            workspace_branch: None,
            workspace_created_at: None,
            workspace_cleanup_on_delete: None,
            warnings: Vec::new(),
            plan_summary: None,
            next_wakeup_at: None,
            next_wakeup_reason: None,
            monitor_active: false,
            monitor_description: None,
            favorited: false,
            favorited_at: None,
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

    #[tokio::test]
    #[serial]
    async fn listing_uses_canonical_rows_and_order_without_persisting() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let live = Instance::new("live", "/repo");
        let mut archived = Instance::new("archived", "/repo");
        archived.archive();
        let mut trashed = Instance::new("trashed", "/repo");
        trashed.trash();
        let ids = [live.id.clone(), archived.id.clone(), trashed.id.clone()];
        let state =
            crate::server::test_support::build_test_app_state(vec![live, archived, trashed]);
        async fn json(response: impl IntoResponse) -> serde_json::Value {
            let response = response.into_response();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
        let snapshot =
            json(crate::server::runtime::get_runtime_snapshot(State(state.clone())).await).await;
        let response = json(
            list_sessions(
                State(state.clone()),
                axum::extract::Query(ListSessionsQuery { state: None }),
            )
            .await,
        )
        .await;
        assert_eq!(
            crate::session::load_workspace_ordering()?.order,
            Vec::<String>::new(),
            "GET must not persist implicit workspaces"
        );
        assert_eq!(
            response["workspace_ordering"],
            snapshot["workspace_ordering"]
        );
        assert_eq!(response["sessions"], snapshot["sessions"]);
        let expected_order: Vec<_> = ids
            .iter()
            .rev()
            .map(|id| format!("/repo::__session__::{id}"))
            .collect();
        assert_eq!(
            snapshot["workspace_ordering"],
            serde_json::to_value(expected_order)?
        );
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
