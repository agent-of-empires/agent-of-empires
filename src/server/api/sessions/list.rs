//! Listing, recent projects, and workspace ordering endpoints.

use super::*;

/// A structured session's pending approvals that can still be settled. A pending nonce only
/// exists on a running worker, and spawn/attach sweep orphaned nonces out of the durable log,
/// so for any other worker the log's leftovers are phantoms the resolver can only 404 on.
pub(crate) fn settleable_approvals(
    state: &AppState,
    id: &str,
    worker: crate::daemon::AcpWorkerState,
) -> Vec<crate::acp::approvals::Approval> {
    if worker != crate::daemon::AcpWorkerState::Running {
        return Vec::new();
    }
    state.acp_event_store.pending_approval_requests(id)
}

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
    let probes = crate::json_probe::Memo::default();
    let worker_states = state.acp_supervisor.worker_states_snapshot().await;
    // Every positional overlay shares this filtered view.
    let scoped_instances: Vec<&Instance> = instances
        .iter()
        // CityHall cannot expose terminal sessions.
        .filter(|inst| !state.cityhall_mode || inst.is_structured())
        .collect();
    let mut resume_configs: std::collections::HashMap<String, crate::session::ResumeProfileConfig> =
        std::collections::HashMap::new();
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
            let mut session = session_response_from_instance_with_plan(
                inst,
                &probes,
                plan_summary,
                acp_worker_state,
                next_wakeup_at,
                next_wakeup_reason,
                active_monitor,
            );
            session.acp_worker_start = acp_worker_start;
            // Archived and trashed rows render stopped; skip the resume check.
            if !inst.is_archived() && !inst.is_trashed() {
                session.context_resume = Some(context_resume_with(inst, || {
                    resume_configs
                        .entry(inst.source_profile.clone())
                        .or_insert_with(|| {
                            crate::session::resume_profile_config(&inst.source_profile)
                        })
                }));
            }
            if structured_live {
                session.pending_approvals = settleable_approvals(state, &inst.id, acp_worker_state)
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
    // 3s sidebar poll (#2603). Monotonic, so the delta below is this request's
    // own count and no reset can race a concurrent request.
    let misses_before = state
        .list_sessions_resolver_misses
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut session_cfg_store = state.session_cfg_store.lock().await;
    let mut session_cfg_cache =
        SessionCfgCache::new(&mut session_cfg_store, &state.list_sessions_resolver_misses);
    let mut project_override_cache = ProjectRegistryCache::new();
    let acp_registry = crate::acp::AgentRegistry::with_defaults();
    let worker_agent_keys = state
        .acp_supervisor
        .worker_agent_keys(
            scoped_instances
                .iter()
                .filter(|inst| inst.is_structured())
                .map(|inst| inst.id.clone())
                .collect(),
        )
        .await;

    // Overlay custom-agent ACP and fork capability; built-ins were resolved in
    // the constructor. Distinct `(profile, project_path)` pairs resolve once via
    // the shared cache.
    for (resp, inst) in sessions.iter_mut().zip(scoped_instances.iter().copied()) {
        if !resp.acp_capable {
            let cfg = session_cfg_cache.resolve(
                &inst.source_profile,
                &inst.repo_config_path().to_string_lossy(),
            );
            // The key the constructor resolved built-ins with: `agent_name`, else `tool`.
            let agent_key = inst
                .agent_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&inst.tool);
            resp.acp_capable = custom_agent_acp_capable(cfg, agent_key);
            resp.acp_can_fork =
                crate::session::fork::custom_structured_fork_capable(agent_key, None, cfg);
        }
        // The solo commands of the agent that classifies this session's prompts, resolved as the
        // queue and publication resolve it, so a custom agent's declared reset is advertised.
        if inst.is_structured() {
            use crate::acp::supervisor::SessionAgent;
            let agent = match worker_agent_keys.get(&inst.id) {
                Some(key) => SessionAgent::Worker(key.clone()),
                None => SessionAgent::Next {
                    tool: inst.tool.clone(),
                    explicit: inst.agent_name.clone(),
                },
            };
            let profile = match &agent {
                SessionAgent::Worker(key) if crate::acp::agent_profiles::is_builtin(key) => {
                    crate::acp::agent_profiles::resolve(key)
                }
                _ => {
                    let (session_cfg, default_agent) =
                        session_cfg_cache.profile_config(&inst.source_profile, &inst.project_path);
                    agent.profile_with(&acp_registry, session_cfg, default_agent)
                }
            };
            resp.agent_profile = crate::acp::agent_profiles::AgentProfileWire::new(
                profile,
                profile.probe_fullscreen(&probes),
            );
            resp.claude_fullscreen = inst.tool == "claude" && resp.agent_profile.fullscreen;
        }
        // Only a terminal session that could switch needs the setting resolved.
        if resp.acp_capable && !inst.is_structured() {
            resp.structured_view_off = !session_cfg_cache
                .structured_view(
                    &inst.source_profile,
                    &inst.repo_config_path().to_string_lossy(),
                )
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

    // Inputs for the rate-limit park overlay, snapshotted so the blocking
    // batch can run once the registry read lock is released. A live worker is
    // never parked, so only workerless sessions pay for the probe.
    let park_probes: Vec<(usize, String, String, Option<crate::session::Instance>)> = sessions
        .iter()
        .zip(scoped_instances.iter().copied())
        .enumerate()
        .filter(|(_, (_, inst))| inst.is_structured() && !inst.is_archived() && !inst.is_trashed())
        .map(|(i, (resp, inst))| {
            (
                i,
                inst.id.clone(),
                inst.source_profile.clone(),
                (resp.acp_worker_state != crate::daemon::AcpWorkerState::Running)
                    .then(|| inst.clone()),
            )
        })
        .collect();

    // Overlay the smart-rename indicator. `Running` comes from the live
    // in-flight set, `Pending` from the shared eligibility predicate, so the
    // indicator cannot drift from the runtime gate.
    {
        use crate::session::smart_rename::{
            check_eligible_with_declarations, resolve_smart_rename_config, SmartRenameState,
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
            let session_cfg = session_cfg_cache.resolve(
                &inst.source_profile,
                &inst.repo_config_path().to_string_lossy(),
            );
            let smart_rename_override = project_override_cache.smart_rename_override(
                &inst.source_profile,
                inst.scratch,
                inst.repo_path(),
                session_cfg,
            );
            let cfg = resolve_smart_rename_config(session_cfg, smart_rename_override);
            let eligible = check_eligible_with_declarations(
                inst.is_structured(),
                cfg.setting_on,
                false,
                &inst.title,
                &inst.tool,
                cfg.rename_agent,
                inst.is_sandboxed(),
                &inst.command,
                session_cfg,
            )
            .is_ok();
            if eligible {
                resp.smart_rename = SmartRenameState::Pending;
            }
        }
    }

    drop(session_cfg_store);
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

    // Sessions whose daemon queue holds messages, with what decides why it waits.
    let queue_probes: Vec<(usize, String, String, String, bool)> = scoped_instances
        .iter()
        .enumerate()
        .filter(|(_, inst)| inst.is_structured() && !inst.is_archived() && !inst.is_trashed())
        .filter_map(|(i, inst)| {
            let first = inst.queued_prompts.first()?;
            let agent_key = inst.agent_name.clone().unwrap_or_else(|| inst.tool.clone());
            Some((
                i,
                inst.id.clone(),
                first.text.clone(),
                agent_key,
                inst.is_idle_dormant(),
            ))
        })
        .collect();

    // The park probe touches config files and SQLite, so it runs with the
    // session registry unlocked rather than holding writers behind it.
    drop(scoped_instances);
    drop(instances);
    if !park_probes.is_empty() {
        let store = Arc::clone(&state.acp_event_store);
        let overlays = tokio::task::spawn_blocking(move || {
            use std::collections::HashMap;
            let mut config_cache: HashMap<
                String,
                (bool, crate::server::acp_reconciler::FailoverSettings),
            > = HashMap::new();
            park_probes
                .into_iter()
                .map(|(i, id, profile, workerless)| {
                    let (auto_resume, failover) = config_cache
                        .entry(profile.clone())
                        .or_insert_with(|| {
                            let config =
                                crate::session::config::profile_config::resolve_config_or_warn(
                                    &profile,
                                );
                            (
                                config.acp.rate_limit_auto_resume,
                                crate::server::acp_reconciler::FailoverSettings::of(&config),
                            )
                        })
                        .clone();
                    let park = workerless.as_ref().and_then(|_| store.rate_limit_park(&id));
                    let failover_to =
                        workerless
                            .as_ref()
                            .zip(park.as_ref())
                            .and_then(|(row, park)| {
                                crate::server::acp_reconciler::failover_target(
                                    row,
                                    &failover,
                                    park,
                                    store.rate_limit_redelivery_streak(&id),
                                    &store,
                                )
                            });
                    let park = park.map(|park| {
                        park.info
                            .unwrap_or_else(crate::acp::state::RateLimitInfo::undated)
                    });
                    (i, auto_resume, park, failover_to)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (i, auto_resume, park, failover_to) in overlays {
            sessions[i].rate_limit_auto_resume = Some(auto_resume);
            sessions[i].rate_limit = park;
            sessions[i].rate_limit_failover_to = failover_to;
        }
    }

    // After the park overlay, which is one of its inputs, and unlocked, because the fold
    // takes the registry lock itself.
    for (i, id, first, agent_key, idle_dormant) in queue_probes {
        use crate::daemon::AcpWorkerState::{Resuming, Running};
        let worker = crate::acp::dispatch::WorkerLiveness {
            running: matches!(sessions[i].acp_worker_state, Resuming | Running),
            idle_dormant,
            rate_limit_parked: sessions[i].rate_limit.is_some(),
        };
        let control = state.session_service.fold_control_state(&id).await;
        let solo = crate::acp::agent_profiles::resolve(&agent_key).is_solo_command(&first);
        sessions[i].queue_reason = crate::acp::dispatch::queue_hold(&control, worker, solo);
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

#[derive(Default)]
pub(crate) struct SessionFilter {
    pub scope: Option<crate::session::SessionScope>,
    pub group: Option<String>,
    pub children_of: Option<String>,
}

impl SessionFilter {
    fn matches(&self, row: &SessionResponse) -> bool {
        crate::session::SessionScope::matches(
            self.scope,
            row.archived_at.is_some(),
            row.trashed_at.is_some(),
        ) && self.group.as_deref().is_none_or(|group| {
            row.group_path == group
                || row
                    .group_path
                    .strip_prefix(group)
                    .is_some_and(|rest| rest.starts_with('/'))
        }) && self
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
            repository_order: &'a [String],
            sidebar_view: crate::session::config::SidebarView,
            trashed_count: usize,
        }
        let rows = &self.snapshot.value.contents.sessions;
        Envelope {
            sessions: ScopedSessions {
                rows,
                filter: &self.filter,
            },
            workspace_ordering: &self.snapshot.value.contents.workspace_ordering,
            repository_order: &self.snapshot.value.contents.repository_order,
            sidebar_view: self.snapshot.value.contents.sidebar_view,
            trashed_count: rows.iter().filter(|row| row.trashed_at.is_some()).count(),
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
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let view = match list_sessions_envelope(
        &state,
        SessionFilter {
            scope: query.state,
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
    // A snapshot's revision changes exactly when its contents do, so cursor and scope identify the body.
    let cursor = &view.snapshot.value.cursor;
    let etag = format!(
        "\"{}.{}.{:?}\"",
        cursor.epoch, cursor.revision, view.filter.scope
    );
    let response = if crate::server::runtime::if_none_match(&headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(&view).into_response()
    };
    // `no-cache` makes the browser revalidate and serve its stored body on a 304, so a poll of an unchanged list is
    // a header-only exchange that still reaches the page as a 200 carrying the cursor headers.
    let validators = [
        (axum::http::header::ETAG, etag),
        (
            axum::http::header::CACHE_CONTROL,
            "private, no-cache".into(),
        ),
    ];
    crate::server::runtime::mutation_response(cursor, (validators, response))
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

pub(crate) fn compute_merged_ordering(
    sessions: &[SessionResponse],
    current_order: &[String],
) -> Vec<String> {
    merge_workspace_ordering(
        sessions.iter().map(|session| {
            (
                workspace_id_for_session(session),
                crate::time::Timestamp::parse_rfc3339(&session.created_at).ok(),
            )
        }),
        current_order,
    )
}

/// Unknown workspaces precede the manual order, newest session first; ties keep the later
/// newest row first. `rows` yields each session's workspace and creation time.
pub(crate) fn merge_workspace_ordering(
    rows: impl IntoIterator<Item = (String, Option<crate::time::Timestamp>)>,
    current_order: &[String],
) -> Vec<String> {
    let known: std::collections::HashSet<&str> = current_order.iter().map(String::as_str).collect();
    let mut new_ids: std::collections::HashMap<String, (Option<crate::time::Timestamp>, usize)> =
        std::collections::HashMap::new();
    for (at, (id, created)) in rows.into_iter().enumerate() {
        if known.contains(id.as_str()) {
            continue;
        }
        let newest = new_ids.entry(id).or_insert((created, at));
        if created >= newest.0 {
            *newest = (created, at);
        }
    }
    let mut new_ids: Vec<_> = new_ids.into_iter().collect();
    new_ids.sort_by_key(|(_, newest)| std::cmp::Reverse(*newest));
    new_ids
        .into_iter()
        .map(|(id, _)| id)
        .chain(current_order.iter().cloned())
        .collect()
}

/// Each session's place in the shared Custom order: its workspace's index in `ordering`.
pub(crate) fn rank_by_workspace(sessions: &mut [SessionResponse], ordering: &[String]) {
    let mut rank: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    // A repeated id keeps its first place, the one a Client's move starts from.
    for (at, id) in ordering.iter().enumerate() {
        rank.entry(id.as_str())
            .or_insert_with(|| u32::try_from(at).unwrap_or(u32::MAX));
    }
    for session in sessions {
        session.sort_index = rank
            .get(workspace_id_for_session(session).as_str())
            .copied();
    }
}

const MAX_ORDER_ENTRIES: usize = 4096;
pub(crate) const MAX_ORDER_ENTRY_LEN: usize = 1024;

/// `order` without repeats or workspaces no session in `sessions` belongs to, and within
/// what `PUT /api/workspace-ordering` accepts, so rewriting the full order never grows it.
pub(crate) fn writable_workspace_order(
    sessions: &[SessionResponse],
    mut order: Vec<String>,
) -> Vec<String> {
    let mut live: std::collections::HashSet<String> =
        sessions.iter().map(workspace_id_for_session).collect();
    order.retain(|id| live.remove(id));
    writable_repository_order(order)
}

/// `order` with entries the endpoint refuses dropped and cut to the length it accepts.
pub(crate) fn writable_repository_order(mut order: Vec<String>) -> Vec<String> {
    order.retain(|entry| entry.len() <= MAX_ORDER_ENTRY_LEN);
    order.truncate(MAX_ORDER_ENTRIES);
    order
}

/// The 413 response for an order longer, or with an entry longer, than a write accepts.
pub(crate) fn oversized_order(list: &[String]) -> Option<axum::response::Response> {
    let message = if list.len() > MAX_ORDER_ENTRIES {
        format!(
            "order has {} entries, max is {}",
            list.len(),
            MAX_ORDER_ENTRIES
        )
    } else {
        let bad = list.iter().find(|e| e.len() > MAX_ORDER_ENTRY_LEN)?;
        format!(
            "order entry is {} bytes, max is {}",
            bad.len(),
            MAX_ORDER_ENTRY_LEN
        )
    };
    Some(
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({ "message": message })),
        )
            .into_response(),
    )
}

#[derive(serde::Serialize)]
struct WorkspaceOrderingResponse<'a> {
    order: &'a [String],
    repositories: &'a [String],
}

/// Replace the shared Custom order of the rows, of the Repository headers, or both; an
/// omitted list keeps its value. An empty list clears it back to newest first.
pub async fn update_workspace_ordering(
    State(state): State<Arc<AppState>>,
    body: Result<
        Json<crate::daemon::WorkspaceOrderingPatch>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    if let Some(refused) = [&body.order, &body.repositories]
        .into_iter()
        .flatten()
        .find_map(|list| oversized_order(list))
    {
        return refused;
    }
    // A request dropped while it waits for the guards leaves nothing behind; once it holds
    // them, the spawned commit owns them through reconciliation.
    let namespace = state.profile_namespace.clone().read_owned().await;
    let publication = state.publication.clone().write_owned().await;
    match tokio::spawn(commit_workspace_ordering(
        state,
        body,
        namespace,
        publication,
    ))
    .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Owns an admitted ordering write through reconciliation, so a dropped request cannot
/// release its guards before the canonical metadata catches up with the disk.
async fn commit_workspace_ordering(
    state: Arc<AppState>,
    body: crate::daemon::WorkspaceOrderingPatch,
    namespace: tokio::sync::OwnedRwLockReadGuard<()>,
    publication: tokio::sync::OwnedRwLockWriteGuard<()>,
) -> axum::response::Response {
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if let Some(base) = &body.base {
        let metadata = state.canonical_metadata.read().await;
        if base.order != metadata.workspace_ordering
            || base.repositories != metadata.repository_order
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "message": "The order changed elsewhere; try again on the refreshed list"
                })),
            )
                .into_response();
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_workspace_ordering(move |ordering| {
            if let Some(order) = body.order {
                ordering.order = order;
            }
            if let Some(repositories) = body.repositories {
                ordering.repositories = repositories;
            }
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
    {
        let mut metadata = state.canonical_metadata.write().await;
        metadata.workspace_ordering = ordering.order;
        metadata.repository_order = ordering.repositories;
    }
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
            repositories: &snapshot.value.contents.repository_order,
        }),
    )
}

/// Change the shared grouping or sort. Clients receive it in the runtime snapshot this
/// write publishes.
pub async fn update_sidebar_view(
    State(state): State<Arc<AppState>>,
    body: Result<Json<crate::daemon::SidebarViewPatch>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Same lock order and admission as the ordering write: namespace, then this write's lane.
    let namespace = state.profile_namespace.clone().read_owned().await;
    let lane = state.sidebar_view_lane.clone().lock_owned().await;
    match tokio::spawn(commit_sidebar_view(state, patch, namespace, lane)).await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Owns an admitted sidebar view write through reconciliation, like the ordering commit.
async fn commit_sidebar_view(
    state: Arc<AppState>,
    patch: crate::daemon::SidebarViewPatch,
    namespace: tokio::sync::OwnedRwLockReadGuard<()>,
    lane: tokio::sync::OwnedMutexGuard<()>,
) -> axum::response::Response {
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|app| {
            if let Some(group_by) = patch.group_by {
                app.group_by = Some(group_by);
            }
            if let Some(sort) = patch.sort {
                app.sort_order = Some(sort);
            }
            app.sidebar_view()
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(std::convert::identity);
    let view = match result {
        Ok(view) => view,
        Err(error) => {
            tracing::error!(target: "http.api.sessions", %error, "sidebar view commit failed");
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            };
            state.runtime.request_publish();
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "message": "Failed to save the sidebar view" })),
            )
                .into_response();
        }
    };
    // Bumped before any early return, so a reload that read state.toml before this write
    // discards itself and the healing reload reads this value.
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let publication = state.publication.write().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    state.canonical_metadata.write().await.sidebar_view = view;
    state.runtime.request_publish();
    drop(publication);
    drop(lane);
    drop(namespace);
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    crate::server::runtime::mutation_response(
        &snapshot.value.cursor,
        Json(snapshot.value.contents.sidebar_view),
    )
}

#[cfg(test)]
mod sidebar_view_tests {
    use super::*;
    use crate::daemon::SidebarViewPatch;
    use crate::session::config::{GroupByMode, SidebarView, SortOrder};

    #[tokio::test]
    #[serial_test::serial]
    async fn a_patch_changes_only_its_own_field_and_reaches_the_snapshot() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let steps = [
            (
                SidebarViewPatch {
                    group_by: Some(GroupByMode::Group),
                    sort: None,
                },
                SidebarView {
                    group_by: GroupByMode::Group,
                    sort: SortOrder::Custom,
                },
            ),
            (
                SidebarViewPatch {
                    group_by: None,
                    sort: Some(SortOrder::Newest),
                },
                SidebarView {
                    group_by: GroupByMode::Group,
                    sort: SortOrder::Newest,
                },
            ),
        ];
        for (patch, expected) in steps {
            let response = update_sidebar_view(State(state.clone()), Ok(Json(patch)))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            let snapshot = state.runtime.publish(&state).await.unwrap();
            assert_eq!(snapshot.value.contents.sidebar_view, expected);
            assert_eq!(
                crate::session::Config::load()
                    .unwrap()
                    .app_state
                    .sidebar_view(),
                expected
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_patch_leaves_the_stored_view_unchanged() {
        use crate::server::test_support;
        use tower::ServiceExt;
        let _home = crate::session::test_support::isolate_app_dir();
        let read_only = test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| state.read_only = true,
        );
        let cityhall = test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| state.cityhall_mode = true,
        );
        let degraded = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        *degraded.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
            code: crate::daemon::ReloadFailureCode::Metadata,
            profiles: Vec::new(),
        };
        for (name, state, status) in [
            ("read-only", read_only, StatusCode::FORBIDDEN),
            ("CityHall", cityhall, StatusCode::FORBIDDEN),
            ("degraded", degraded, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = test_support::build_router_for_test(state)
                .oneshot(
                    axum::http::Request::builder()
                        .method("PATCH")
                        .uri("/api/sidebar-view")
                        .header("host", "localhost")
                        .header("content-type", "application/json")
                        .extension(axum::extract::ConnectInfo(
                            "127.0.0.1:5555".parse::<std::net::SocketAddr>().unwrap(),
                        ))
                        .body(axum::body::Body::from(r#"{"sort":"name"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{name}");
            assert_eq!(
                crate::session::Config::load()
                    .unwrap()
                    .app_state
                    .sidebar_view()
                    .sort,
                SortOrder::Custom,
                "{name}"
            );
        }
    }

    /// A stalled write holds no publication guard, and once admitted it finishes after its
    /// request is dropped. Health lost during the write still invalidates older reloads.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_stalled_write_frees_publication_and_outlives_its_dropped_request() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        for degrade_mid_write in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let state = crate::server::test_support::build_test_app_state(Vec::new());
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                crate::session::update_app_state(|_| {
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
                .unwrap();
            });
            held_rx.recv_timeout(Duration::from_secs(10)).unwrap();

            let patch = SidebarViewPatch {
                group_by: None,
                sort: Some(SortOrder::Name),
            };
            let write = tokio::spawn(update_sidebar_view(State(state.clone()), Ok(Json(patch))));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while state.sidebar_view_lane.try_lock().is_ok() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the write never started"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop(
                tokio::time::timeout(Duration::from_secs(5), state.publication.write())
                    .await
                    .expect("a stalled sidebar write must not hold publication"),
            );
            assert!(!write.is_finished());
            let epoch = state.mutation_epoch.load(Ordering::SeqCst);
            if degrade_mid_write {
                *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::Metadata,
                    profiles: Vec::new(),
                };
                release_tx.send(()).unwrap();
                let response = write.await.unwrap().into_response();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            } else {
                write.abort();
                assert!(write.await.is_err_and(|error| error.is_cancelled()));
                release_tx.send(()).unwrap();
            }
            holder.join().unwrap();
            // The admitted commit keeps its lane until the canonical view matches the disk.
            let _lane =
                tokio::time::timeout(Duration::from_secs(10), state.sidebar_view_lane.lock())
                    .await
                    .expect("the detached commit never finished");
            assert!(state.mutation_epoch.load(Ordering::SeqCst) > epoch);
            let expected = if degrade_mid_write {
                SortOrder::Custom
            } else {
                SortOrder::Name
            };
            assert_eq!(
                state.canonical_metadata.read().await.sidebar_view.sort,
                expected
            );
        }
    }

    /// A request still waiting for the namespace guard leaves no commit behind when dropped.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_request_dropped_before_admission_writes_nothing() {
        use std::time::Duration;
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let exclusive = state.profile_namespace.write().await;
        let patch = SidebarViewPatch {
            group_by: None,
            sort: Some(SortOrder::Name),
        };
        let write = tokio::spawn(update_sidebar_view(State(state.clone()), Ok(Json(patch))));
        let ordering = tokio::spawn(update_workspace_ordering(
            State(state.clone()),
            Ok(Json(
                serde_json::from_value(serde_json::json!({ "order": ["w"] })).unwrap(),
            )),
        ));
        tokio::task::yield_now().await;
        write.abort();
        ordering.abort();
        assert!(write.await.is_err_and(|error| error.is_cancelled()));
        assert!(ordering.await.is_err_and(|error| error.is_cancelled()));
        drop(exclusive);
        // Both writers are gone, so the guards they would hold are free at once.
        drop(
            tokio::time::timeout(Duration::from_secs(5), state.profile_namespace.write())
                .await
                .expect("a dropped request kept a namespace guard"),
        );
        let stored = crate::session::Config::load().unwrap().app_state;
        assert_eq!(stored.sidebar_view().sort, SortOrder::Custom);
        assert!(crate::session::load_workspace_ordering()
            .unwrap()
            .order
            .is_empty());
    }
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
            initial_prompt_error: None,
            initial_prompt_error_code: None,
            pane_dead_observed: false,
            yolo_mode: false,
            sort_index: None,
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
            outside_repository: false,
            scratch: false,
            created_without_setup: false,
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
            context_resume: Some(ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }),
            rate_limit: None,
            rate_limit_auto_resume: None,
            rate_limit_failover_to: None,
            queued_count: 0,
            queue_reason: None,
            acp_capable: false,
            acp_session_id: None,
            import_pending: false,
            acp_agent: None,
            acp_can_fork: false,
            structured_view_off: false,
            keeps_context: false,
            agent_profile: Default::default(),
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
            urgent: false,
            pinned_at: None,
            archived_at: None,
            ignored_at: None,
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
                axum::http::HeaderMap::new(),
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

    #[tokio::test]
    #[serial]
    async fn unchanged_listing_answers_not_modified_without_a_body() -> anyhow::Result<()> {
        use axum::http::{header, HeaderMap, HeaderValue};
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let live = Instance::new("live", "/repo");
        let mut trashed = Instance::new("trashed", "/repo");
        trashed.trash();
        let state = crate::server::test_support::build_test_app_state(vec![live, trashed]);
        let get = |scope: Option<crate::session::SessionScope>, etag: Option<&str>| {
            let state = state.clone();
            let mut headers = HeaderMap::new();
            if let Some(etag) = etag {
                headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(etag).unwrap());
            }
            async move {
                list_sessions(
                    State(state),
                    axum::extract::Query(ListSessionsQuery { state: scope }),
                    headers,
                )
                .await
            }
        };
        let untrashed = Some(crate::session::SessionScope::Untrashed);

        let first = get(untrashed, None).await;
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first.headers()[header::ETAG].to_str()?.to_string();
        let revision = first.headers()[crate::daemon::RUNTIME_REVISION_HEADER].clone();
        let body = axum::body::to_bytes(first.into_body(), 1024 * 1024).await?;
        let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body)?;
        assert_eq!(envelope.sessions.len(), 1, "untrashed omits the trash");
        assert_eq!(envelope.trashed_count, 1);

        let again = get(untrashed, Some(&etag)).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()[header::ETAG], etag.as_str());
        assert_eq!(
            again.headers()[crate::daemon::RUNTIME_REVISION_HEADER],
            revision
        );
        assert!(axum::body::to_bytes(again.into_body(), 1024)
            .await?
            .is_empty());

        let other_scope = get(Some(crate::session::SessionScope::Trashed), Some(&etag)).await;
        assert_eq!(
            other_scope.status(),
            StatusCode::OK,
            "an ETag is bound to its scope"
        );

        state.instances.write().await[0].title = "renamed".into();
        state.runtime.publish(&state).await?;
        let changed = get(untrashed, Some(&etag)).await;
        assert_eq!(changed.status(), StatusCode::OK, "a new revision refetches");
        Ok(())
    }

    /// The snapshot ranks each row by its workspace's place in the shared order, unplaced
    /// workspaces first; a write that names only the repositories keeps the row order.
    #[tokio::test]
    #[serial]
    async fn the_shared_order_ranks_snapshot_rows_and_a_partial_write_keeps_the_rest(
    ) -> anyhow::Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let rows: Vec<Instance> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| Instance::new(name, &format!("/repo/{name}")))
            .collect();
        let workspace =
            |at: usize| format!("/repo/{}::__session__::{}", rows[at].title, rows[at].id);
        let placed = vec![workspace(1), workspace(0)];
        let state = crate::server::test_support::build_test_app_state(rows.clone());
        let put = |patch: crate::daemon::WorkspaceOrderingPatch| {
            let state = state.clone();
            async move {
                let response = update_workspace_ordering(State(state), Ok(Json(patch)))
                    .await
                    .into_response();
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
            }
        };
        put(crate::daemon::WorkspaceOrderingPatch {
            order: Some(placed.clone()),
            ..Default::default()
        })
        .await;
        let response = put(crate::daemon::WorkspaceOrderingPatch {
            repositories: Some(vec!["/repo/b".into(), "/repo/a".into()]),
            ..Default::default()
        })
        .await;

        assert_eq!(crate::session::load_workspace_ordering()?.order, placed);
        assert_eq!(
            response["repositories"],
            serde_json::json!(["/repo/b", "/repo/a"])
        );
        let snapshot = state.runtime.publish(&state).await?.value.clone();
        assert_eq!(snapshot.contents.repository_order, ["/repo/b", "/repo/a"]);
        assert_eq!(
            response["order"],
            serde_json::json!(snapshot.contents.workspace_ordering)
        );
        let rank = |at: usize| {
            snapshot
                .contents
                .sessions
                .iter()
                .find(|session| session.id == rows[at].id)
                .and_then(|session| session.sort_index)
        };
        // d and c are unplaced, newest first; then the persisted b, a.
        for (at, expected) in [(3, 0), (2, 1), (1, 2), (0, 3)] {
            assert_eq!(rank(at), Some(expected), "{}", rows[at].title);
        }
        assert_eq!(snapshot.contents.placed_workspaces, placed);

        // A write built on an order since replaced is refused; one on the stored order lands.
        for (base, expected) in [
            (Vec::new(), StatusCode::CONFLICT),
            (placed.clone(), StatusCode::OK),
        ] {
            let patch = crate::daemon::WorkspaceOrderingPatch {
                order: Some(Vec::new()),
                repositories: None,
                base: Some(crate::daemon::WorkspaceOrderingBase {
                    order: base,
                    repositories: vec!["/repo/b".into(), "/repo/a".into()],
                }),
            };
            let response = update_workspace_ordering(State(state.clone()), Ok(Json(patch)))
                .await
                .into_response();
            assert_eq!(response.status(), expected);
        }
        assert!(crate::session::load_workspace_ordering()?.order.is_empty());
        Ok(())
    }

    /// A repeated id ranks at its first place, and a full-order write drops repeats and
    /// workspaces no session has.
    #[test]
    fn a_repeated_or_stale_workspace_neither_ranks_twice_nor_is_written_back() {
        let mut sessions = vec![
            mock_response("s1", "/repo/a", Some("main")),
            mock_response("s2", "/repo/b", Some("main")),
        ];
        let order: Vec<String> = [
            "/repo/a::main",
            "/repo/gone::x",
            "/repo/b::main",
            "/repo/a::main",
        ]
        .map(String::from)
        .into();
        rank_by_workspace(&mut sessions, &order);
        assert_eq!(
            sessions.iter().map(|s| s.sort_index).collect::<Vec<_>>(),
            [Some(0), Some(2)]
        );
        assert_eq!(
            writable_workspace_order(&sessions, order),
            ["/repo/a::main", "/repo/b::main"]
        );
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
        let created = |id: &str, path: &str, at: &str| SessionResponse {
            created_at: at.to_string(),
            ..mock_response(id, path, Some("main"))
        };
        // Storage order is not creation order, e.g. an older row in a later-loaded profile;
        // second precision and sub-second precision compare as times, not strings.
        let cases = cases.into_iter().chain([
            (
                vec![
                    created("s1", "/repo/new", "2026-01-03T00:00:00.5+00:00"),
                    created("s2", "/repo/old", "2026-01-01T00:00:00+00:00"),
                    created("s3", "/repo/mid", "2026-01-03T00:00:00+00:00"),
                ],
                vec![],
                vec!["/repo/new::main", "/repo/mid::main", "/repo/old::main"],
            ),
            (
                // A tie goes to the workspace whose newest row comes later.
                vec![
                    created("s1", "/repo/a", "2026-01-01T10:00:00Z"),
                    created("s2", "/repo/b", "2026-01-01T11:00:00Z"),
                    created("s3", "/repo/a", "2026-01-01T11:00:00Z"),
                ],
                vec![],
                vec!["/repo/a::main", "/repo/b::main"],
            ),
        ]);
        for (sessions, existing, expected) in cases {
            assert_eq!(compute_merged_ordering(&sessions, &existing), expected);
        }
    }
}
