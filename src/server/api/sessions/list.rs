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

#[cfg(test)]
pub(crate) async fn project_sessions(
    state: &Arc<AppState>,
    worker_states: &HashMap<
        String,
        (
            crate::daemon::AcpWorkerState,
            Option<crate::daemon::AcpWorkerStart>,
        ),
    >,
) -> anyhow::Result<Vec<SessionResponse>> {
    let recovery = state
        .session_service
        .recovery_snapshots(!state.read_only && !state.cityhall_mode)
        .await?;
    project_sessions_with_recovery(state, worker_states, &recovery).await
}

pub(crate) async fn project_sessions_with_recovery(
    state: &Arc<AppState>,
    worker_states: &HashMap<
        String,
        (
            crate::daemon::AcpWorkerState,
            Option<crate::daemon::AcpWorkerStart>,
        ),
    >,
    recovery: &HashMap<String, crate::server::session_service::RecoverySnapshot>,
) -> anyhow::Result<Vec<SessionResponse>> {
    let instances = state.instances.read().await;
    let claude_fullscreen = crate::claude_settings::read_tui_fullscreen();
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
            // An archived session is sunk, so its wakeup/monitor badge is
            // meaningless and the per-poll SQLite lookups are skipped.
            // latest_plan stays ungated: a collapsed archived row may still
            // show a plan summary.
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
            let (acp_worker_state, acp_worker_start) = worker_states
                .get(&inst.id)
                .cloned()
                .unwrap_or((crate::daemon::AcpWorkerState::Absent, None));
            let mut session = SessionResponse::from_instance_with_plan(
                inst,
                claude_fullscreen,
                plan_summary,
                acp_worker_state,
                next_wakeup_at,
                next_wakeup_reason,
                active_monitor,
            );
            session.acp_worker_start = acp_worker_start;
            if let Some(snapshot) = recovery
                .get(&inst.id)
                .filter(|snapshot| snapshot.matches(inst))
            {
                session.recovery = snapshot.recovery.clone();
                if snapshot.context_resume_available {
                    session.context_resume = Some(ContextResumeAvailability::Available);
                }
                if let Some(recovery) = &session.recovery {
                    session.last_error = Some(recovery.diagnostic.clone());
                    session.last_error_evidence = recovery.evidence;
                    if recovery.actions.restart_same {
                        session.context_resume = Some(ContextResumeAvailability::Available);
                    }
                }
            }
            if structured_live && acp_worker_state == crate::daemon::AcpWorkerState::Running {
                // Gate on a live worker: a pending nonce only exists on a
                // running worker, and spawn/attach sweep orphaned nonces out of
                // the durable log, so projecting a non-running row would surface
                // a phantom approval the resolver can only 404 on.
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
            session.update_restart =
                crate::session::update_restart::offer_pending(
                    inst,
                    session
                        .context_resume
                        .as_ref()
                        .is_some_and(|resume| update_restart_resumable(inst, resume)),
                    session.context_resume.as_ref().is_some_and(|resume| {
                        matches!(resume, ContextResumeAvailability::Available)
                    }) && session.pending_approvals.is_empty(),
                );
            session
        })
        .collect();

    // Share resolved config between the ACP-capability and smart-rename
    // overlays, halving disk reads when a profile/project pair repeats in the
    // 3s sidebar poll (#2603). Monotonic, so the delta below is this request's
    // own count and no reset can race a concurrent request.
    let misses_before = state
        .list_sessions_resolver_misses
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut session_cfg_cache = SessionCfgCache::new(&state.list_sessions_resolver_misses);
    let mut project_override_cache = ProjectRegistryCache::new();

    // Overlay custom-agent ACP capability; built-ins were resolved in the
    // constructor. Distinct `(profile, project_path)` pairs resolve once via
    // the shared cache.
    for (resp, inst) in sessions.iter_mut().zip(scoped_instances.iter().copied()) {
        if !resp.acp_capable {
            let cfg = session_cfg_cache.resolve(&inst.source_profile, &inst.project_path);
            // The key the constructor resolved built-ins with: `agent_name`, else `tool`.
            let agent_key = inst
                .agent_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&inst.tool);
            resp.acp_capable = custom_agent_acp_capable(cfg, agent_key);
        }
        // Only a terminal session that could switch needs the setting resolved.
        if resp.acp_capable && !inst.is_structured() {
            resp.structured_view_off = !session_cfg_cache
                .structured_view(&inst.source_profile, &inst.project_path)
                .is_offered();
        }
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
    // the standalone workdir action. Resolved once per distinct profile.
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

    // Snapshot canonical quota probes before releasing the registry read lock.
    let park_probes: Vec<(usize, String, String)> = sessions
        .iter()
        .zip(scoped_instances.iter().copied())
        .enumerate()
        .filter(|(_, (_, inst))| inst.is_structured() && !inst.is_archived() && !inst.is_trashed())
        .map(|(i, (_, inst))| (i, inst.id.clone(), inst.source_profile.clone()))
        .collect();

    // Overlay the smart-rename indicator. `Running` comes from the live
    // in-flight set, `Pending` from the shared eligibility predicate, so the
    // indicator cannot drift from the runtime gate.
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
            // A session whose one-shot already ran, and failed since the name
            // is still default, will not retry, so it is not pending either.
            if attempted.contains(&inst.id) {
                continue;
            }
            let session_cfg = session_cfg_cache.resolve(&inst.source_profile, &inst.project_path);
            let smart_rename_override = project_override_cache.smart_rename_override(
                &inst.source_profile,
                inst.scratch,
                inst.repo_path(),
                session_cfg,
            );
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
    tracing::trace!(
        target: "http.api.sessions",
        rows = sessions.len(),
        resolver_misses,
        "list_sessions resolved session config once per unique profile/project pair"
    );

    // The park probe touches config files and SQLite, so it runs with the
    // session registry unlocked rather than holding writers behind it.
    drop(scoped_instances);
    drop(instances);
    if !park_probes.is_empty() {
        let store = Arc::clone(&state.acp_event_store);
        let overlays = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            use std::collections::HashMap;
            let mut auto_resume_cache: HashMap<String, bool> = HashMap::new();
            park_probes
                .into_iter()
                .map(|(i, id, profile)| -> anyhow::Result<_> {
                    let auto_resume =
                        *auto_resume_cache.entry(profile.clone()).or_insert_with(|| {
                            crate::session::config::profile_config::resolve_config_or_warn(&profile)
                                .acp
                                .rate_limit_auto_resume
                        });
                    let (known, park) = match store.rate_limit_park(&id)? {
                        crate::acp::event_store::ControlKnowledge::Known(park) => (
                            true,
                            park.map(|park| {
                                park.info
                                    .unwrap_or_else(crate::acp::state::RateLimitInfo::undated)
                            }),
                        ),
                        crate::acp::event_store::ControlKnowledge::Indeterminate => (false, None),
                    };
                    Ok((i, auto_resume, known, park))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await??;
        for (i, auto_resume, known, park) in overlays {
            sessions[i].rate_limit_auto_resume = Some(auto_resume);
            sessions[i].rate_limit_known = Some(known);
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

    Ok(sessions)
}

#[derive(Default)]
pub(crate) struct SessionFilter {
    pub scope: Option<crate::session::SessionScope>,
    pub group: Option<String>,
    pub children_of: Option<String>,
    pub profile: Option<String>,
}

impl SessionFilter {
    fn matches(&self, row: &SessionResponse) -> bool {
        self.profile
            .as_deref()
            .is_none_or(|profile| row.profile == profile)
            && crate::session::SessionScope::matches(
                self.scope,
                row.archived_at.is_some(),
                row.trashed_at.is_some(),
            )
            && self.group.as_deref().is_none_or(|group| {
                row.group_path == group
                    || row
                        .group_path
                        .strip_prefix(group)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            && self
                .children_of
                .as_deref()
                .is_none_or(|parent| row.parent_session_id.as_deref() == Some(parent))
    }
}

struct ScopedSessions<'a> {
    rows: &'a [SessionResponse],
    filter: &'a SessionFilter,
}

impl serde::Serialize for ScopedSessions<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.rows.iter().filter(|row| self.filter.matches(row)))
    }
}

pub(crate) struct SessionsView {
    snapshot: Arc<crate::server::runtime::PublishedSnapshot>,
    filter: SessionFilter,
}

impl serde::Serialize for SessionsView {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Envelope<'a> {
            sessions: ScopedSessions<'a>,
            workspace_ordering: &'a [String],
        }
        Envelope {
            sessions: ScopedSessions {
                rows: &self.snapshot.value.contents.sessions,
                filter: &self.filter,
            },
            workspace_ordering: &self.snapshot.value.contents.workspace_ordering,
        }
        .serialize(serializer)
    }
}

impl schemars::JsonSchema for SessionsView {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        <crate::daemon::SessionsEnvelope as schemars::JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <crate::daemon::SessionsEnvelope as schemars::JsonSchema>::json_schema(generator)
    }
}

pub(crate) async fn list_sessions_envelope(
    state: &Arc<AppState>,
    filter: SessionFilter,
) -> anyhow::Result<SessionsView> {
    Ok(SessionsView {
        snapshot: state.runtime.snapshot(state).await?,
        filter,
    })
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListSessionsQuery>,
) -> axum::response::Response {
    let _namespace = state.profile_namespace.read().await;
    let profile = if let Some(requested) = query.profile.as_deref() {
        let selectable =
            match tokio::task::spawn_blocking(crate::server::reload::selectable_profiles).await {
                Ok(Ok(profiles)) => profiles,
                _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
        let Some(selected) = selectable.iter().find(|profile| {
            profile.name == requested || profile.aliases.iter().any(|alias| alias == requested)
        }) else {
            return api_error(
                StatusCode::NOT_FOUND,
                "profile_not_found",
                "Profile is not selectable",
            );
        };
        let inventory = state
            .runtime_read_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(accepted) = inventory
            .inventory
            .iter()
            .find(|profile| profile.identity == selected.identity)
        else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        Some(accepted.name.clone())
    } else {
        None
    };
    let view = match list_sessions_envelope(
        &state,
        SessionFilter {
            scope: query.state,
            profile,
            ..SessionFilter::default()
        },
    )
    .await
    {
        Ok(view) => view,
        Err(error) => {
            tracing::debug!(target: "server.runtime", %error, "runtime snapshot failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    crate::server::runtime::mutation_response(&view.snapshot.value.cursor, Json(&view))
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
            agent_pane: Default::default(),
            auxiliary: Vec::new(),
            acp_provider: None,
            id: id.to_string(),
            idempotency_key: None,
            title: id.to_string(),
            project_path: project_path.to_string(),
            artifact_dir: String::new(),
            agent_session_id: None,
            parent_session_id: None,
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
            sort_index: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            last_error_evidence: None,
            recovery: None,
            branch: branch.map(str::to_string),
            main_repo_path: None,
            base_branch: None,
            base_branch_override: None,
            worktree_created_at: None,
            is_sandboxed: false,
            sandbox_container_name: None,
            outside_repository: false,
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
            trash_lifecycle_id: None,
            failed_purge_forced: None,
            remote_owner: None,
            remote_owner_key: None,
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            view: crate::session::View::Terminal,
            pending_approvals: Vec::new(),
            acp_worker_state: crate::daemon::AcpWorkerState::Absent,
            acp_worker_start: None,
            update_restart: false,
            context_resume: Some(ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }),
            rate_limit: None,
            rate_limit_known: None,
            rate_limit_auto_resume: None,
            acp_capable: false,
            acp_session_id: None,
            import_pending: false,
            acp_agent: None,
            acp_can_fork: false,
            structured_view_off: false,
            keeps_context: false,
            clear_aliases: Vec::new(),
            compact_aliases: Vec::new(),
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
    fn workspace_id_keys_on_repo_and_branch() {
        let mut worktree = mock_response("s1", "/tmp/worktree", Some("main"));
        worktree.main_repo_path = Some("/tmp/repo".to_string());
        for (response, expected) in [
            (
                mock_response("s1", "/tmp/repo", Some("feature/x")),
                "/tmp/repo::feature/x",
            ),
            (
                mock_response("abc123", "/tmp/repo", None),
                "/tmp/repo::__session__::abc123",
            ),
            // Matches the client's `normalizePath`, which strips trailing slashes.
            (
                mock_response("s1", "/tmp/repo/", Some("main")),
                "/tmp/repo::main",
            ),
            (worktree, "/tmp/repo::main"),
        ] {
            assert_eq!(workspace_id_for_session(&response), expected);
        }
    }

    #[tokio::test]
    #[serial]
    async fn canonical_quota_metadata_preserves_known_absent_unknown_and_sql_failure() {
        use crate::acp::event_sender::EventMetadata;
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::state::{test_support::prompt, Event, RateLimitInfo};
        for (present, background, shelf, undated) in [
            (false, false, "live", false),
            (true, false, "live", false),
            (false, true, "live", false),
            (true, true, "live", false),
            (true, false, "live", true),
            (true, false, "archived", false),
            (true, false, "trashed", false),
            (false, false, "terminal", false),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempdir().unwrap();
            let mut row = Instance::new("quota metadata", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = if shelf == "terminal" {
                crate::session::View::Terminal
            } else {
                crate::session::View::Structured
            };
            row.status = if background {
                crate::session::Status::Running
            } else {
                crate::session::Status::Idle
            };
            if shelf == "archived" {
                row.archive();
            }
            if shelf == "trashed" {
                row.trash();
            }
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            // Snapshot DATA only: no worker is installed and no runtime or I/O grant is created.
            let mut workers = HashMap::new();
            if present {
                workers.insert(
                    row.id.clone(),
                    (crate::daemon::AcpWorkerState::Running, None),
                );
            }
            let input = prompt("interrupted main");
            state
                .acp_event_store
                .record(&row.id, 1, &input, &EventMetadata::host(&input))
                .unwrap();
            let mut metadata = crate::server::test_support::sql_projection_metadata();
            state
                .acp_event_store
                .record(&row.id, 2, &Event::ThinkingStarted, &metadata)
                .unwrap();
            metadata.operation.as_mut().unwrap().outcome =
                Some(crate::acp::protocol::AcpOriginalRequestResult::Failed);
            metadata.prompt_observation =
                Some(crate::acp::protocol::AcpPromptObservation::Written {
                    window_id: crate::acp::protocol::AcpPromptWindowId::new(),
                });
            let reset = chrono::Utc::now() + chrono::Duration::hours(1);
            let quota = Event::RateLimit {
                info: RateLimitInfo {
                    status: "limited".into(),
                    resets_at: (!undated).then_some(reset),
                    kind: "usage".into(),
                },
            };
            state
                .acp_event_store
                .record(&row.id, 3, &quota, &metadata)
                .unwrap();
            state
                .acp_event_store
                .record(
                    &row.id,
                    4,
                    &Event::Stopped {
                        reason: "rate_limited".into(),
                    },
                    &metadata,
                )
                .unwrap();
            if background {
                let event = Event::BackgroundAgentLaunched {
                    agent_id: "background".into(),
                    tool_call_id: "background-tool".into(),
                    description: "historical work".into(),
                    prompt: "historical work".into(),
                    model: "test".into(),
                    output_file: String::new(),
                    workflow: None,
                    started_at: chrono::Utc::now(),
                };
                state
                    .acp_event_store
                    .record(&row.id, 5, &event, &EventMetadata::history())
                    .unwrap();
            }
            assert_eq!(
                state
                    .acp_event_store
                    .has_in_flight_main_turn(&row.id)
                    .unwrap(),
                Known(false)
            );
            let projected = project_sessions(&state, &workers)
                .await
                .expect("canonical projection");
            let response = &projected[0];
            let eligible = shelf == "live";
            assert_eq!(response.rate_limit.is_some(), eligible);
            assert_eq!(response.rate_limit_known, eligible.then_some(true));
            assert_eq!(response.rate_limit_auto_resume, eligible.then_some(false));
            assert_eq!(
                response.acp_worker_state == crate::daemon::AcpWorkerState::Running,
                present
            );
            if eligible {
                assert_eq!(response.status, row.status.wire_str());
                assert_eq!(
                    response.rate_limit.as_ref().unwrap().resets_at,
                    (!undated).then_some(reset)
                );
                state
                    .acp_event_store
                    .test_conn()
                    .execute(
                        "DELETE FROM acp_events WHERE session_id=?1 AND seq<4",
                        [&row.id],
                    )
                    .unwrap();
                state.acp_event_store.control_cache().forget(&row.id);
                let unknown = project_sessions(&state, &workers).await.unwrap();
                assert_eq!(unknown[0].rate_limit_known, Some(false));
                assert!(unknown[0].rate_limit.is_none());
                assert_eq!(
                    serde_json::to_value(&unknown[0]).unwrap()["rate_limit_known"],
                    false
                );
                assert_eq!(unknown[0].status, row.status.wire_str());
                let next = state.acp_event_store.highest_seq(&row.id) + 1;
                let new_input = prompt("new explicit main intent");
                state
                    .acp_event_store
                    .record(&row.id, next, &new_input, &EventMetadata::host(&new_input))
                    .unwrap();
                let absent = project_sessions(&state, &workers).await.unwrap();
                assert_eq!(absent[0].rate_limit_known, Some(true));
                assert!(absent[0].rate_limit.is_none());
                state
                    .acp_event_store
                    .test_conn()
                    .execute_batch(
                        "ALTER TABLE acp_rate_limit_budgets RENAME TO hidden_list_budgets",
                    )
                    .unwrap();
                state.acp_event_store.control_cache().forget(&row.id);
                assert!(project_sessions(&state, &workers).await.is_err());
                state
                    .acp_event_store
                    .test_conn()
                    .execute_batch(
                        "ALTER TABLE hidden_list_budgets RENAME TO acp_rate_limit_budgets",
                    )
                    .unwrap();
            }
            state.shutdown.cancel();
        }
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
                axum::extract::Query(ListSessionsQuery {
                    state: None,
                    profile: None,
                }),
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
    fn compute_merged_ordering_prepends_unknowns_newest_first_once() {
        let known = |id: &str, path: &str, branch: &str| mock_response(id, path, Some(branch));
        let existing = vec!["/repo/x::main".to_string(), "/repo/y::dev".to_string()];
        let cases = [
            (
                vec![
                    known("s1", "/repo/a", "main"),
                    known("s2", "/repo/b", "dev"),
                ],
                vec![],
                vec!["/repo/b::dev", "/repo/a::main"],
            ),
            (
                vec![
                    known("s1", "/repo/a", "main"),
                    known("s2", "/repo/a", "main"),
                    known("s3", "/repo/b", "dev"),
                ],
                vec![],
                vec!["/repo/b::dev", "/repo/a::main"],
            ),
            (
                vec![known("s1", "/repo/z", "feat")],
                existing.clone(),
                vec!["/repo/z::feat", "/repo/x::main", "/repo/y::dev"],
            ),
            (
                vec![
                    known("s1", "/repo/x", "main"),
                    known("s2", "/repo/y", "dev"),
                ],
                existing,
                vec!["/repo/x::main", "/repo/y::dev"],
            ),
        ];
        for (sessions, existing, expected) in cases {
            assert_eq!(compute_merged_ordering(&sessions, &existing), expected);
        }
    }
}

#[cfg(test)]
mod shared_recovery_tests {
    use super::*;
    use crate::acp::supervisor::test_support::managed::with_managed;
    use crate::server::test_support::managed_control_session;
    use crate::session::{
        RuntimeEvidence, SessionRecoveryAction, SessionRecoveryReason, SessionRecoveryRequest,
    };

    async fn projected(state: &Arc<AppState>, id: &str) -> SessionResponse {
        let workers = state.acp_supervisor.worker_states_snapshot().await;
        project_sessions(state, &workers)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
    }

    async fn partial_output(live: &crate::server::test_support::ManagedControlSession) {
        live.native
            .send(serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"partial native output before failure"}
                    }
                }
            }))
            .await;
    }

    async fn auth_failure(
        live: &mut crate::server::test_support::ManagedControlSession,
        request: &serde_json::Value,
    ) {
        live.native
            .send(serde_json::json!({
                "jsonrpc":"2.0","id":request["id"],"error":{
                    "code":-32603,
                    "message":"Sign in required sk-ant-abcdefghijklmnop1234567890",
                    "data":{"details":"Repair agent authentication","trace":"privateTokenMarker"}
                }
            }))
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if matches!(live.client.next_event().await.unwrap(),
                    crate::acp::Event::Stopped { reason } if reason == "prompt_failed")
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    async fn background_agent(
        live: &crate::server::test_support::ManagedControlSession,
        agent_id: &str,
        output_file: &str,
    ) {
        live.native
            .send(serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{
                        "sessionUpdate":"tool_call_update","toolCallId":format!("{agent_id}-tool"),
                        "_meta":{"claudeCode":{"toolName":"Agent","toolResponse":{
                            "agentId":agent_id,"description":"background recovery guard",
                            "prompt":"keep working","resolvedModel":"test-model",
                            "outputFile":output_file,"status":"async_launched"
                        }}}
                    }
                }
            }))
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let folded = live
                    .state
                    .session_service
                    .fold_control_state(live.id())
                    .await
                    .unwrap();
                if folded.background_agents.iter().any(|agent| {
                    agent.agent_id == agent_id
                        && agent.resource_ref.is_some() == !output_file.is_empty()
                        && agent.ended_at.is_none()
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Native background launch reaches its qualified fold");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn same_refuses_unresolved_queue_without_output_uncertainty_and_preserves_all_input() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "shared-recovery-queued-input").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let prompt = live.start_prompt("failed explicit input").await;
                live.finish_prompt(&prompt, Some("provider_failure")).await;
                let folded = state.session_service.fold_control_state(&id).await.unwrap();
                assert!(!folded.operation_output_uncertain);
                assert!(
                    projected(&state, &id)
                        .await
                        .recovery
                        .unwrap()
                        .actions
                        .restart_same
                );
                state
                    .session_service
                    .enqueue_prompt(
                        &id,
                        "unresolved-queue".into(),
                        "do not replay this queued input".into(),
                        Vec::new(),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                for pending_initial in [false, true] {
                    if pending_initial {
                        state
                            .session_service
                            .set_pending_initial_turn(
                                &id,
                                "retain blocked initial input".into(),
                                Vec::new(),
                            )
                            .await;
                    }
                    let recovery = projected(&state, &id).await.recovery.unwrap();
                    assert!(!recovery.actions.restart_same && recovery.actions.fresh);
                    assert!(!recovery.work_in_progress && recovery.uncertainty.is_none());
                    let original = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    let storage =
                        crate::session::Storage::open_unwatched(&original.source_profile).unwrap();
                    let persisted = storage
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap();
                    let methods = live.native.requests();
                    let owner = live.client.actual().connected_runner();
                    let response = crate::server::api::sessions::restart_session(
                        State(state.clone()),
                        Path(id.clone()),
                        Ok(Some(Json(crate::daemon::RestartSessionBody {
                            recovery: Some(SessionRecoveryRequest {
                                action: SessionRecoveryAction::Same,
                                expected: recovery.expected,
                            }),
                            ..Default::default()
                        }))),
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    let current = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    assert_eq!(
                        serde_json::to_value(&current).unwrap(),
                        serde_json::to_value(&original).unwrap()
                    );
                    let saved = storage
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(saved).unwrap(),
                        serde_json::to_value(persisted).unwrap()
                    );
                    assert_eq!(live.client.actual().connected_runner(), owner);
                    assert_eq!(live.native.requests(), methods);
                    assert!(
                        state
                            .session_service
                            .fold_control_state(&id)
                            .await
                            .unwrap()
                            .connection_ready
                    );
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn qualified_background_work_refuses_concurrent_same_and_fresh() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live =
                    managed_control_session(scope, "shared-recovery-current-background").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let prompt = live.start_prompt("failed main turn").await;
                live.finish_prompt(&prompt, Some("provider_failure")).await;
                let actual = live.client.actual();
                let store = actual.native_store.as_ref().unwrap().stores[0].clone();
                let project = store.join("projects/-managed");
                std::fs::create_dir_all(project.join("subagents")).unwrap();
                std::fs::write(project.join(format!("{}.jsonl", live.sid)), "").unwrap();
                let transcript = project.join("subagents/current-background.jsonl");
                std::fs::write(&transcript, "").unwrap();
                background_agent(&live, "current-background", transcript.to_str().unwrap()).await;
                let folded = state.session_service.fold_control_state(&id).await.unwrap();
                assert!(!folded.turn_active && folded.has_qualified_background_work());
                let recovery = projected(&state, &id).await.recovery.unwrap();
                assert!(
                    recovery.work_in_progress && recovery.actions.attach && recovery.actions.stop
                );
                assert!(
                    !recovery.actions.restart_same
                        && !recovery.actions.fresh
                        && !recovery.actions.retry_operation
                );
                let original = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                let methods = live.native.requests();
                for action in [SessionRecoveryAction::Same, SessionRecoveryAction::Fresh] {
                    let response = crate::server::api::sessions::restart_session(
                        State(state.clone()),
                        Path(id.clone()),
                        Ok(Some(Json(crate::daemon::RestartSessionBody {
                            recovery: Some(SessionRecoveryRequest {
                                action,
                                expected: recovery.expected.clone(),
                            }),
                            ..Default::default()
                        }))),
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    let current = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    assert_eq!(
                        serde_json::to_value(&current).unwrap(),
                        serde_json::to_value(&original).unwrap()
                    );
                    assert_eq!(live.native.requests(), methods);
                    let folded = state.session_service.fold_control_state(&id).await.unwrap();
                    assert!(folded.connection_ready && folded.has_qualified_background_work());
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn actual_prompt_failure_reaches_canonical_consumers_without_replacing_its_live_native() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "shared-recovery-error").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let owner = live.client.actual().connected_runner();
                let prompt = live.start_prompt("original explicit input").await;
                partial_output(&live).await;
                auth_failure(&mut live, &prompt).await;
                let response = projected(&state, &id).await;
                let recovery = response
                    .recovery
                    .as_ref()
                    .expect("current failure is public");
                assert_eq!(response.acp_session_id.as_deref(), Some(live.sid.as_str()));
                assert_eq!(
                    response.last_error.as_deref(),
                    Some(recovery.diagnostic.as_str())
                );
                assert_eq!(response.last_error_evidence, recovery.evidence);
                assert_eq!(recovery.reason, SessionRecoveryReason::OperationFailed);
                assert_eq!(recovery.runtime, RuntimeEvidence::AcpTransportReady);
                assert_eq!(recovery.evidence.unwrap().runtime, RuntimeEvidence::Unknown);
                assert_eq!(recovery.evidence.unwrap().retryable, None);
                assert_eq!(recovery.evidence.unwrap().origin, None);
                assert!(!recovery
                    .diagnostic
                    .contains("sk-ant-abcdefghijklmnop1234567890"));
                assert!(!recovery.diagnostic.contains("privateTokenMarker"));
                assert!(!recovery.work_in_progress);
                assert!(recovery.uncertainty.is_some());
                assert!(
                    recovery.actions.attach
                        && recovery.actions.restart_same
                        && recovery.actions.stop
                        && recovery.actions.fresh
                );
                assert!(!recovery.actions.retry_operation);
                let before = live.native.requests();
                state.acp_control_cache.forget(&id);
                let reloaded = projected(&state, &id).await;
                assert_eq!(reloaded.recovery, response.recovery);
                assert_eq!(live.native.requests(), before);
                assert_eq!(live.client.actual().connected_runner(), owner);

                // Admitting new work supersedes the error, not opening or replaying the view.
                let next = live.start_prompt("new explicitly composed input").await;
                let rejected = crate::server::api::sessions::restart_session(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(Some(Json(crate::daemon::RestartSessionBody {
                        recovery: Some(SessionRecoveryRequest {
                            action: SessionRecoveryAction::Same,
                            expected: recovery.expected.clone(),
                        }),
                        ..Default::default()
                    }))),
                )
                .await;
                assert_eq!(rejected.status(), StatusCode::CONFLICT);
                let current = state.session_service.fold_control_state(&id).await.unwrap();
                assert!(current.turn_active && current.connection_ready);
                assert!(current.operation_diagnostic.is_none());
                assert_eq!(live.client.actual().connected_runner(), owner);
                assert_eq!(
                    live.native
                        .requests()
                        .iter()
                        .filter(|method| method.as_str() == "session/prompt")
                        .count(),
                    2
                );
                assert_eq!(next["params"]["sessionId"], live.sid);
                live.finish_prompt(&next, None).await;
                assert!(projected(&state, &id).await.recovery.is_none());
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn same_and_fresh_use_real_qualified_control_paths_without_replaying_or_erasing_history()
    {
        for action in [SessionRecoveryAction::Same, SessionRecoveryAction::Fresh] {
            with_managed(move |scope| Box::pin(async move {
                let mut live = managed_control_session(scope, "shared-explicit-recovery").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let prompt = live.start_prompt("input that must never be replayed").await;
                partial_output(&live).await;
                live.finish_prompt(&prompt, Some("provider_failure")).await;
                if action == SessionRecoveryAction::Same {
                    background_agent(&live, "historical-untracked-launch", "").await;
                    let folded = state.session_service.fold_control_state(&id).await.unwrap();
                    assert!(folded.has_active_background_agent());
                    assert!(!folded.has_qualified_background_work());
                }
                if action == SessionRecoveryAction::Fresh {
                    state.session_service.set_pending_initial_turn(
                        &id, "blocked automatic continuation".into(), Vec::new(),
                    ).await;
                    state.session_service.enqueue_prompt(
                        &id, "pending-user-input".into(), "pending AoE input".into(),
                        Vec::new(), None, None,
                    ).await.unwrap();
                }
                let selected = projected(&state, &id).await;
                let recovery = selected.recovery.unwrap();
                let original = state.instances.read().await.iter()
                    .find(|row| row.id == id).unwrap().clone();
                if action == SessionRecoveryAction::Fresh {
                    assert!(!recovery.actions.restart_same && recovery.actions.fresh);
                    let before = serde_json::to_value((
                        &original.pending_initial_turn, &original.queued_prompts,
                    )).unwrap();
                    let refused = crate::server::api::sessions::restart_session(
                        State(state.clone()), Path(id.clone()),
                        Ok(Some(Json(crate::daemon::RestartSessionBody {
                            recovery: Some(SessionRecoveryRequest {
                                action: SessionRecoveryAction::Same,
                                expected: recovery.expected.clone(),
                            }),
                            ..Default::default()
                        }))),
                    ).await;
                    assert_eq!(refused.status(), StatusCode::CONFLICT);
                    let current = state.instances.read().await.iter()
                        .find(|row| row.id == id).unwrap().clone();
                    assert_eq!(serde_json::to_value((
                        &current.pending_initial_turn, &current.queued_prompts,
                    )).unwrap(), before);
                    assert!(original.conversation_state().matches_checkpoint(&current));
                    assert_eq!(original.acp_establishment, current.acp_establishment);
                }
                let store = original.agent_session_binding.as_ref()
                    .and_then(|binding| binding.execution.as_ref())
                    .expect("real Native establishment records its selected store")
                    .stores.first().unwrap();
                std::fs::create_dir_all(store).unwrap();
                let history = store.join("old-native-history.jsonl");
                std::fs::write(&history, b"retained native history").unwrap();
                let expected_sid = if action == SessionRecoveryAction::Same {
                    live.sid.clone()
                } else {
                    uuid::Uuid::new_v4().to_string()
                };
                let next_sid = expected_sid.clone();
                let original_sid = live.sid.clone();
                let project = original.project_path.clone();
                let (config, replacement) = scope.native_peer(
                    project.into(), "claude", "claude",
                    move |request| match request["method"].as_str().unwrap() {
                        "initialize" => Some(serde_json::json!({
                            "jsonrpc":"2.0","id":request["id"],
                            "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}
                        })),
                        "session/load" => {
                            assert_eq!(action, SessionRecoveryAction::Same);
                            assert_eq!(request["params"]["sessionId"], original_sid);
                            Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"sessionId":next_sid}}))
                        }
                        "session/new" => {
                            assert_eq!(action, SessionRecoveryAction::Fresh);
                            Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"sessionId":next_sid}}))
                        }
                        other => panic!("unexpected recovery Native method: {other}"),
                    },
                );
                let events = scope.bind_external_launch(&id, config);
                let response = crate::server::api::sessions::restart_session(
                    State(state.clone()), Path(id.clone()),
                    Ok(Some(Json(crate::daemon::RestartSessionBody {
                        recovery: Some(SessionRecoveryRequest { action, expected: recovery.expected }),
                        ..Default::default()
                    }))),
                ).await;
                assert!(response.status().is_success(), "recovery returned {}", response.status());
                let resumed = scope.observe_external_resume(id.clone(), events).await;
                resumed.ready().await;
                let current = state.instances.read().await.iter()
                    .find(|row| row.id == id).unwrap().clone();
                assert_eq!(current.acp_session_id.as_deref(), Some(expected_sid.as_str()));
                assert_eq!(current.id, id);
                assert!(current.pending_initial_turn.is_none());
                assert!(current.queued_prompts.is_empty());
                state.session_service.drain_pending_initial_turn(&id).await;
                state.session_service.drain_queued_prompts_once(&id).await;
                assert!(projected(&state, &id).await.recovery.is_none());
                assert_eq!(std::fs::read(&history).unwrap(), b"retained native history");
                assert_eq!(live.native.requests().iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
                assert!(!replacement.requests().iter().any(|method| method == "session/prompt"));
                assert_eq!(replacement.requests().iter().filter(|method| method.as_str() == "session/new").count(),
                    usize::from(action == SessionRecoveryAction::Fresh));
                assert_eq!(replacement.requests().iter().filter(|method| method.as_str() == "session/load").count(),
                    usize::from(action == SessionRecoveryAction::Same));
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unreadable_current_owner_revokes_actions_and_preserves_the_real_error_and_identity() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "shared-unknown-owner").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let prompt = live.start_prompt("one explicit input").await;
                partial_output(&live).await;
                live.finish_prompt(&prompt, Some("provider_failure")).await;
                state
                    .session_service
                    .set_pending_initial_turn(
                        &id,
                        "retained blocked continuation".into(),
                        Vec::new(),
                    )
                    .await;
                state
                    .session_service
                    .enqueue_prompt(
                        &id,
                        "retained-queue".into(),
                        "retained queued input".into(),
                        Vec::new(),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                let input_before = {
                    let rows = state.instances.read().await;
                    let row = rows.iter().find(|row| row.id == id).unwrap();
                    serde_json::to_value((&row.pending_initial_turn, &row.queued_prompts)).unwrap()
                };
                let before = projected(&state, &id).await;
                let path = crate::process::worker_registry::record_path(&id).unwrap();
                struct Restore(std::path::PathBuf, Vec<u8>);
                impl Drop for Restore {
                    fn drop(&mut self) {
                        crate::session::atomic_write(&self.0, &self.1).unwrap();
                    }
                }
                let restore = Restore(path.clone(), std::fs::read(&path).unwrap());
                crate::session::atomic_write(&path, b"privateTokenMarker malformed registry")
                    .unwrap();
                let unknown = projected(&state, &id).await;
                let recovery = unknown.recovery.unwrap();
                assert_eq!(recovery.runtime, RuntimeEvidence::Unknown);
                assert_eq!(unknown.acp_session_id, before.acp_session_id);
                assert_eq!(
                    recovery.diagnostic,
                    before.recovery.as_ref().unwrap().diagnostic
                );
                assert_eq!(
                    recovery.evidence,
                    before.recovery.as_ref().unwrap().evidence
                );
                assert_eq!(
                    recovery.uncertainty,
                    before.recovery.as_ref().unwrap().uncertainty
                );
                assert!(recovery.actions.attach);
                assert!(!recovery.actions.retry_operation && !recovery.actions.restart_same);
                assert!(!recovery.actions.stop && !recovery.actions.fresh);
                let methods = live.native.requests();
                let response = crate::server::api::sessions::restart_session(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(Some(Json(crate::daemon::RestartSessionBody {
                        recovery: Some(SessionRecoveryRequest {
                            action: SessionRecoveryAction::Fresh,
                            expected: before.recovery.unwrap().expected,
                        }),
                        ..Default::default()
                    }))),
                )
                .await;
                assert_eq!(response.status(), StatusCode::CONFLICT);
                let input_after = {
                    let rows = state.instances.read().await;
                    let row = rows.iter().find(|row| row.id == id).unwrap();
                    serde_json::to_value((&row.pending_initial_turn, &row.queued_prompts)).unwrap()
                };
                assert_eq!(input_after, input_before);
                assert_eq!(live.native.requests(), methods);
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    b"privateTokenMarker malformed registry"
                );
                drop(restore);
            })
        })
        .await;
    }
}

#[cfg(test)]
mod profile_selection_tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn physical_profile_aliases_select_canonical_rows_without_creating_an_alias_owner() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut first = Instance::new("first", "/tmp/profile-first");
        first.source_profile = "canonical".into();
        let mut second = Instance::new("second", "/tmp/profile-second");
        second.source_profile = "other".into();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "canonical",
            vec![first.clone()],
        );
        crate::server::test_support::seed_instances_on_disk_for_test("other", vec![second.clone()]);
        let state =
            crate::server::test_support::build_test_app_state(vec![first.clone(), second.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        let alias = crate::session::get_profile_dir_path("selected-alias").unwrap();
        std::os::unix::fs::symlink(
            crate::session::get_profile_dir_path("canonical").unwrap(),
            &alias,
        )
        .unwrap();
        for (requested, expected_id, expected_profile) in [
            ("selected-alias", first.id.as_str(), "canonical"),
            ("other", second.id.as_str(), "other"),
        ] {
            let response = list_sessions(
                State(state.clone()),
                axum::extract::Query(ListSessionsQuery {
                    state: Some(crate::session::SessionScope::All),
                    profile: Some(requested.into()),
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: crate::daemon::SessionsEnvelope = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body.sessions.len(), 1);
            assert_eq!(body.sessions[0].id, expected_id);
            assert_eq!(body.sessions[0].profile, expected_profile);
            assert!(body.sessions[0].recovery.is_none());
        }
        let refused = list_sessions(
            State(state),
            axum::extract::Query(ListSessionsQuery {
                state: None,
                profile: Some("missing".into()),
            }),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    }
}
