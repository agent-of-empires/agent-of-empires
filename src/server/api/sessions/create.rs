//! Session creation: validation, hooks, idempotency, restart sync.

use super::*;

/// Hard cap on a single `idempotency_key`'s length, so one request cannot
/// persist an arbitrarily large string onto its instance. This bounds key
/// SIZE, not the number of distinct keys; entry count is bounded separately
/// by the pruning in `AppState::idempotency_lock`.
const IDEMPOTENCY_KEY_MAX_LEN: usize = 200;

/// Find a prior session created with the given `idempotency_key`. Scans all
/// instances, including trashed, so a retry against a soft-deleted session
/// still returns it rather than creating a duplicate; a hard-deleted
/// (physically removed) session falls through to a fresh create, a
/// documented, accepted limitation for this "nice-to-have" item.
pub(super) fn find_by_idempotency_key<'a>(
    instances: &'a [Instance],
    key: &str,
) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.idempotency_key.as_deref() == Some(key))
}

pub(super) fn create_body_uses_worktree(body: &CreateSessionBody) -> bool {
    body.worktree_enabled || body.worktree_branch.is_some()
}

pub(super) fn create_body_combines_scratch_and_worktree(body: &CreateSessionBody) -> bool {
    body.scratch && create_body_uses_worktree(body)
}

/// Build the provider fork seed after capability and source validation.
pub(super) fn resolve_create_fork_seed(
    tool: &str,
    parent_id: &str,
    structured: bool,
) -> Result<crate::session::ForkSeed, crate::session::ForkDenied> {
    if structured {
        return Ok(crate::session::ForkSeed::Structured {
            parent_acp_session_id: parent_id.to_string(),
        });
    }
    crate::session::fork::terminal_fork_seed(
        tool,
        Some(parent_id),
        crate::session::capture::generate_session_uuid(),
    )
}

pub(super) fn create_body_has_conflicting_sources(body: &CreateSessionBody) -> bool {
    [
        &body.import_acp_session_id,
        &body.fork_from,
        &body.fork_session_id,
    ]
    .into_iter()
    .filter(|value| {
        value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    })
    .take(2)
    .count()
        == 2
}

async fn resolve_canonical_fork_seed(
    state: &Arc<AppState>,
    body: &CreateSessionBody,
    source_id: &str,
) -> Result<crate::session::ForkSeed, axum::response::Response> {
    let _namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let lock = state.instance_lock(source_id).await;
    let _guard = lock.lock().await;
    let instances = state.instances.read().await;
    let source = instances
        .iter()
        .find(|row| row.id == source_id)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if source.has_fresh_lifecycle_reservation(chrono::Utc::now()) {
        return Err((
            StatusCode::CONFLICT,
            crate::daemon::ApiErrorCode::LifecycleLocked.header(),
        )
            .into_response());
    }
    if source.tool != body.tool
        || source.view != body.view
        || (source.is_structured()
            && acp_agent_key(&source.tool, source.agent_name.as_deref())
                != acp_agent_key(&body.tool, body.agent_name.as_deref()))
    {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    let parent_id = if source.is_structured() {
        if !crate::session::fork::structured_fork_capable(
            &source.tool,
            source.agent_name.as_deref(),
        ) {
            return Err(StatusCode::BAD_REQUEST.into_response());
        }
        source.acp_session_id.as_deref()
    } else {
        source.agent_session_id.as_deref()
    }
    .filter(|id| crate::session::capture::is_valid_session_id(id))
    .ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    resolve_create_fork_seed(&source.tool, parent_id, source.is_structured())
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())
}

/// The ACP registry key a create request resolves to: an explicit `agent_name`
/// when present, else the tool name. Shared by the capability check and the
/// allowlist check (#3241) so the two cannot judge different agents.
fn acp_agent_key<'a>(tool: &'a str, agent_name: Option<&'a str>) -> &'a str {
    agent_name.filter(|s| !s.is_empty()).unwrap_or(tool)
}

/// True iff the agent can run a structured (ACP) session in this project: a
/// built-in ACP agent in the registry, or a custom tool with a valid
/// `agent_acp_cmd`. Mirrors the post-build capability check (below) so
/// CityHall mode can reject a non-ACP agent up front instead of letting the
/// session silently downgrade to the terminal view. See #7.
pub(crate) fn agent_is_acp_capable(
    profile: &str,
    project_path: &std::path::Path,
    tool: &str,
    agent_name: Option<&str>,
) -> bool {
    let resolved = acp_agent_key(tool, agent_name);
    if crate::acp::AgentRegistry::with_defaults()
        .get(resolved)
        .is_some()
    {
        return true;
    }
    // Keyed off `resolved`, not `tool`: an explicit `agent_name` can point at a
    // different `agent_acp_cmd` entry, and `resolve_agent_spec` resolves the
    // custom map by that same name. Looking up `tool` here would report
    // not-capable for an agent that spawns fine, skipping the up-front 403 in
    // favor of a late refusal at spawn.
    let session = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        profile,
        project_path,
    )
    .session;
    session
        .agent_acp_cmd
        .get(resolved)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(resolved, cmd).is_ok())
        // A custom agent inheriting a registry-backed base via `agent_detect_as`
        // spawns fine through the base adapter, so report it capable up front.
        || crate::acp::inherited_acp_base(resolved, &session.agent_detect_as).is_some()
}

pub(super) fn validate_session_tool_identity(
    tool: &str,
    profile: &str,
    project_path: &std::path::Path,
) -> bool {
    if crate::agents::get_agent(tool).is_some() {
        return true;
    }

    match crate::session::config::repo_config::resolve_config_with_repo(profile, project_path) {
        Ok(config) => config
            .session
            .custom_agents
            .get(tool)
            .is_some_and(|command| !command.trim().is_empty()),
        Err(e) => {
            tracing::warn!(
                "Failed to resolve config while validating session tool '{}': {e}",
                tool
            );
            false
        }
    }
}

/// Missing approval for repository hooks, including all commands it would cover.
#[derive(Debug)]
pub(crate) struct HooksNeedTrust {
    pub(crate) on_create: Vec<String>,
    pub(crate) on_launch: Vec<String>,
    pub(crate) on_destroy: Vec<String>,
    pub(crate) needs_mcp_trust: bool,
}

impl std::fmt::Display for HooksNeedTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Repository hooks require trust before this session can be created"
        )
    }
}

impl std::error::Error for HooksNeedTrust {}

#[derive(Debug, thiserror::Error)]
#[error("Repository trust review changed")]
pub(crate) struct CreationTrustChanged;

pub async fn cancel_creation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.session_service.cancel_creation(&id) {
        // The acknowledgement is a mutation response like any other, so the
        // caller can fence it against the snapshot it was applied at.
        let cursor = state
            .runtime
            .snapshot(&state)
            .await
            .map(|snapshot| snapshot.value.cursor.clone());
        match cursor {
            Ok(cursor) => crate::server::runtime::mutation_response(&cursor, StatusCode::ACCEPTED),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    } else {
        let code = crate::daemon::ApiErrorCode::CreationNotPending;
        (code.status(), code.header()).into_response()
    }
}

fn read_creation_trust(
    project_path: &std::path::Path,
    scratch: bool,
) -> anyhow::Result<crate::session::config::repo_config::RepoTrust> {
    use crate::session::config::repo_config::{self, RepoTrust, TrustSurface};
    if scratch {
        Ok(RepoTrust {
            project_path: String::new(),
            hooks: TrustSurface::Absent,
            mcp: TrustSurface::Absent,
        })
    } else {
        repo_config::check_repo_trust(project_path)
    }
}

fn creation_trust_fingerprint(
    base: &crate::session::HooksConfig,
    trust: &crate::session::config::repo_config::RepoTrust,
) -> crate::daemon::CreationTrustFingerprint {
    use crate::session::config::repo_config::{compute_hooks_hash, TrustSurface};
    crate::daemon::CreationTrustFingerprint {
        project_path: trust.project_path.clone(),
        base_hooks_hash: compute_hooks_hash(base),
        hooks_hash: match &trust.hooks {
            TrustSurface::Absent => None,
            TrustSurface::Trusted(hooks) => Some(compute_hooks_hash(hooks)),
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
        },
        mcp_hash: match &trust.mcp {
            TrustSurface::Absent => None,
            TrustSurface::Trusted(servers) => {
                Some(crate::session::mcp::project_mcp::fingerprint(servers))
            }
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
        },
    }
}

pub async fn review_creation_trust(
    State(state): State<Arc<AppState>>,
    body: Result<
        Json<crate::daemon::CreationTrustRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> axum::response::Response {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if state.cityhall_mode {
        return crate::server::api::cityhall_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let _namespace = state.profile_namespace.read().await;
    if !matches!(
        *state.canonical_health.read().await,
        crate::daemon::RuntimeHealth::Healthy
    ) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let profile = match body.profile {
        Some(profile) => profile,
        None => state
            .canonical_metadata
            .read()
            .await
            .default_profile
            .clone(),
    };
    let result = tokio::task::spawn_blocking(
        move || -> anyhow::Result<crate::daemon::CreationTrustReview> {
            use crate::session::config::repo_config::{self, TrustSurface};
            anyhow::ensure!(!profile.is_empty(), "Missing profile");
            let profile = crate::session::resolve_existing_profile(&profile)?;
            let path = std::path::Path::new(&body.path);
            anyhow::ensure!(body.scratch || path.is_dir(), "Project path does not exist");
            let base = crate::session::resolve_config(&profile)?.hooks;
            let trust = read_creation_trust(path, body.scratch)?;
            let fingerprint = creation_trust_fingerprint(&base, &trust);
            let hooks_need_trust = trust.hooks.needs_trust();
            let mcp_need_trust = trust.mcp.needs_trust();
            let repo_hooks = match trust.hooks {
                TrustSurface::Trusted(hooks) | TrustSurface::NeedsTrust { config: hooks, .. } => {
                    hooks
                }
                TrustSurface::Absent => Default::default(),
            };
            let mcp_summaries = match trust.mcp {
                TrustSurface::Trusted(servers)
                | TrustSurface::NeedsTrust {
                    config: servers, ..
                } => servers
                    .iter()
                    .map(|server| server.redacted_summary())
                    .collect(),
                TrustSurface::Absent => Vec::new(),
            };
            Ok(crate::daemon::CreationTrustReview {
                fingerprint,
                merged_hooks: repo_config::apply_repo_hook_overrides(base, &repo_hooks),
                repo_hooks,
                mcp_summaries,
                hooks_need_trust,
                mcp_need_trust,
            })
        },
    )
    .await;
    match result {
        Ok(Ok(review)) => Json(review).into_response(),
        Ok(Err(_)) => StatusCode::BAD_REQUEST.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Creation hooks captured before worktree provisioning.
#[derive(Debug)]
pub(crate) struct CreateHookPlan {
    /// Already merged against the canonical config snapshot.
    pub(crate) hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    /// Approved hashes to persist before provisioning; absent when no new trust is needed.
    pub(crate) trust_write: Option<(Option<String>, Option<String>)>,
}

impl CreateHookPlan {
    pub(crate) fn on_create(&self) -> &[String] {
        self.hooks
            .as_ref()
            .map_or(&[], |hooks| &hooks.hooks().on_create)
    }
}

/// Resolve hooks under the caller’s explicit approval or skip decision.
pub(crate) fn resolve_create_hook_plan(
    profile: &str,
    base: &crate::session::HooksConfig,
    project_path: &std::path::Path,
    scratch: bool,
    trust_hooks_requested: Option<bool>,
    expected_review: Option<&crate::daemon::CreationTrustFingerprint>,
) -> anyhow::Result<CreateHookPlan> {
    use crate::session::config::repo_config::{self, TrustSurface};

    let trust = read_creation_trust(project_path, scratch)?;
    if expected_review.is_some_and(|expected| *expected != creation_trust_fingerprint(base, &trust))
    {
        return Err(CreationTrustChanged.into());
    }

    // MCP is gated at spawn; an omitted decision refuses unapproved hooks only.
    if trust.hooks.needs_trust() && trust_hooks_requested.is_none() {
        // Approval covers every hook type, not just on_create.
        let merged = match &trust.hooks {
            TrustSurface::Trusted(h) | TrustSurface::NeedsTrust { config: h, .. } => {
                repo_config::apply_repo_hook_overrides(base.clone(), h)
            }
            TrustSurface::Absent => base.clone(),
        };
        return Err(anyhow::Error::new(HooksNeedTrust {
            on_create: merged.on_create,
            on_launch: merged.on_launch,
            on_destroy: merged.on_destroy,
            needs_mcp_trust: trust.mcp.needs_trust(),
        }));
    }

    let repo_hooks = match &trust.hooks {
        TrustSurface::Trusted(h) => Some(h),
        TrustSurface::NeedsTrust { config, .. } if trust_hooks_requested == Some(true) => {
            Some(config)
        }
        _ => None,
    };
    let trust_write = if trust_hooks_requested == Some(true) {
        let hooks_hash = match &trust.hooks {
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
            _ => None,
        };
        let mcp_hash = match &trust.mcp {
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
            _ => None,
        };
        if hooks_hash.is_some() || mcp_hash.is_some() {
            Some((hooks_hash, mcp_hash))
        } else {
            None
        }
    } else {
        None
    };
    let repo_root = repo_hooks.map(|_| std::path::Path::new(&trust.project_path));
    let merged = repo_hooks
        .map(|hooks| repo_config::apply_repo_hook_overrides(base.clone(), hooks))
        .unwrap_or_else(|| base.clone());
    let hooks = repo_config::ResolvedHooks::from_merged(profile, repo_root, merged);
    Ok(CreateHookPlan { hooks, trust_write })
}

/// Run captured creation hooks with streamed error-tail capture. `progress`,
/// when present, receives the live command and output for the requesting
/// surface.
pub(crate) fn run_create_hooks(
    instance: &mut Instance,
    plan: &CreateHookPlan,
    store: &dyn crate::session::SessionStore,
    progress: Option<&dyn Fn(crate::session::config::repo_config::HookProgress)>,
) -> anyhow::Result<()> {
    use crate::session::config::repo_config;

    if plan.on_create().is_empty() {
        return Ok(());
    }

    let hook_env = repo_config::lifecycle_env_vars(instance);

    if instance.sandbox_info.is_some() {
        instance.ensure_container_in(store)?;
        let workdir = instance.container_workdir();
        if let Some(sandbox) = instance.sandbox_info.as_ref() {
            repo_config::execute_hooks_in_container_streamed(
                plan.on_create(),
                &sandbox.container_name,
                &workdir,
                progress,
                &hook_env,
            )?;
        }
    } else {
        repo_config::execute_hooks_streamed(
            plan.on_create(),
            std::path::Path::new(&instance.project_path),
            progress,
            &hook_env,
        )?;
    }
    Ok(())
}

/// CityHall structured-target gate for per-session lifecycle / metadata routes.
/// CityHall only ever creates structured sessions and `list_sessions` hides
/// everything else, so a mutation must refuse any non-structured target (or an
/// unknown id): otherwise a locked-down client could enumerate a pre-existing
/// plain/terminal session (from the TUI, `aoe add`, or another client on the
/// same daemon) and respawn it (re-running its stored `command_override` host
/// binary via `build_host_command`), destroy it, or edit it. Returns the
/// canonical CityHall 403 (never a 404, so the mode does not leak which ids
/// exist); `None` in normal mode or for a genuine structured target. See #7.
pub(super) async fn cityhall_block_non_structured(
    state: &AppState,
    id: &str,
) -> Option<axum::response::Response> {
    if !state.cityhall_mode {
        return None;
    }
    let is_structured_target = state
        .instances
        .read()
        .await
        .iter()
        .find(|i| i.id == id)
        .is_some_and(|i| i.is_structured());
    (!is_structured_target).then(crate::server::api::cityhall_response)
}

/// Plural [`cityhall_block_non_structured`]: refuse unless EVERY id resolves to
/// a structured session this mode created. Used by multi-session teardown
/// (`delete_workspace`), which acts on all ids, not just the owner. See #7.
pub(super) async fn cityhall_block_any_non_structured(
    state: &AppState,
    ids: &[String],
) -> Option<axum::response::Response> {
    if !state.cityhall_mode {
        return None;
    }
    let instances = state.instances.read().await;
    let all_structured = ids.iter().all(|id| {
        instances
            .iter()
            .find(|i| &i.id == id)
            .is_some_and(|i| i.is_structured())
    });
    (!all_structured).then(crate::server::api::cityhall_response)
}

/// Query params for `POST /api/sessions`. `wait=ready` blocks the response
/// until the new session's status leaves `Starting` (or a bounded timeout
/// elapses), so a caller that sends a message immediately after create
/// doesn't race the agent's own startup. See #3156.
#[derive(Deserialize)]
pub struct CreateSessionQuery {
    pub wait: Option<String>,
}

/// Bound on `?wait=ready`: how long `create_session` will block before
/// returning whatever status the session has reached.
const WAIT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn current_instance(state: &Arc<AppState>, id: &str) -> Option<Instance> {
    state
        .instances
        .read()
        .await
        .iter()
        .find(|i| i.id == id)
        .cloned()
}

/// Blocks until `id`'s status leaves `Starting`, or `timeout` elapses.
/// Subscribes to `status_tx` before checking current state, so a transition
/// that lands between the subscribe and the first check is still queued on
/// the receiver rather than lost; the direct check covers a transition that
/// already happened before subscribing. On `Lagged`, falls back to
/// re-reading live state rather than trusting the (possibly stale) broadcast
/// position. Returns `None` only if the instance vanished outright.
pub(super) async fn wait_until_left_starting(
    state: &Arc<AppState>,
    id: &str,
    timeout: std::time::Duration,
) -> Option<Instance> {
    let mut rx = state.status_tx.subscribe();

    let initial = current_instance(state, id).await?;
    if initial.status != Status::Starting {
        return Some(initial);
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return current_instance(state, id).await;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(change)) => {
                if change.instance_id == id && change.new != Status::Starting {
                    return current_instance(state, id).await;
                }
                // Different session, or re-entered Starting: keep waiting.
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                match current_instance(state, id).await {
                    Some(inst) if inst.status != Status::Starting => return Some(inst),
                    Some(_) => continue,
                    None => return None,
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return current_instance(state, id).await;
            }
            Err(_elapsed) => return current_instance(state, id).await,
        }
    }
}

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<CreateSessionQuery>,
    body: Result<Json<CreateSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(mut body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let default_profile = state
        .canonical_metadata
        .read()
        .await
        .default_profile
        .clone();

    if state.cityhall_mode {
        // CityHall sessions are server-derived and locked down: they span every
        // configured project, always render in structured view, and must run an
        // ACP-capable agent. Every client-supplied field that could escape the
        // mode (path/repos/view/scratch plus the spawn/branch fields reset
        // below) is neutralized so a crafted request cannot escape it. See #7.
        if body
            .fork_session_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        {
            return (
                StatusCode::FORBIDDEN,
                crate::daemon::ApiErrorCode::CityhallMode.header(),
            )
                .into_response();
        }
        let projects = crate::session::projects::load_merged(&default_profile).unwrap_or_default();
        if projects.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "cityhall_no_projects",
                    "message": "CityHall mode requires at least one configured project"
                })),
            )
                .into_response();
        }
        body.scratch = false;
        // Reset every client-controllable spawn / branch field to its default.
        // Deriving path/repos/view is not enough: a crafted request could still
        // smuggle an alternate binary, extra args/env, yolo mode, a chosen
        // branch/base, or a sandbox container past the locked-down mode.
        // `command_override` is the load-bearing one: the ACP supervisor
        // validates the registry-default binary but then adopts the client's
        // `argv[0]` unchecked, so `command_override: "/bin/sh -c ..."` on a
        // registry ACP tool would pass the ACP-capable gate below and spawn an
        // arbitrary binary as the agent. See #7 review.
        body.command_override = String::new();
        body.extra_args = String::new();
        body.extra_env = Vec::new();
        body.yolo_mode = false;
        body.worktree_enabled = false;
        body.worktree_branch = None;
        body.create_new_branch = false;
        body.base_branch = None;
        body.sandbox = false;
        body.sandbox_image = None;
        // Do not let the client approve the repo's `on_create` host hooks: that
        // would run (and persist durable trust for) operator-repo commands from
        // a locked-down user. Reset to the untrusted default. See #7 review.
        body.trust_hooks = None;
        body.trust_review = None;
        // The "primary" repo is the first entry in merged registry order; the
        // rest ride along as workspace repos. With multiple projects that pick
        // is arbitrary but deterministic (registry order is stable), and the
        // session spans them all regardless, so which one is primary only
        // affects labeling. Non-empty is checked above, so `next()` is Some.
        let mut paths = projects.into_iter().map(|p| p.path);
        body.path = paths.next().unwrap();
        body.extra_repo_paths = paths.collect();
        body.view = crate::session::View::Structured;
        // Fork / import resume an existing agent session and would bypass the
        // server-derived path + ACP gate, so they are not honored in the mode.
        body.fork_from = None;
        body.import_acp_session_id = None;
        let profile = body
            .profile
            .clone()
            .unwrap_or_else(|| default_profile.clone());
        if !agent_is_acp_capable(
            &profile,
            std::path::Path::new(&body.path),
            &body.tool,
            body.agent_name.as_deref(),
        ) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "cityhall_agent_not_acp",
                    "message": "CityHall mode requires an ACP-capable agent"
                })),
            )
                .into_response();
        }
    }

    // Scratch sessions are server-provisioned; the worktree path is the
    // wrong model for them. Reject the combination before reaching the
    // builder so misbehaving clients get a clear 400 instead of a
    // less-specific builder bail surfaced as 500.
    if create_body_combines_scratch_and_worktree(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "validation_failed",
                "message": "Cannot combine scratch with worktree mode"
            })),
        )
            .into_response();
    }
    if body.scratch && !body.extra_repo_paths.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "validation_failed",
                "message": "Cannot combine scratch with extra_repo_paths"
            })),
        )
            .into_response();
    }
    // The builder ignores `path` in scratch mode (provisions its own
    // directory), but accepting both silently is a surprising contract
    // for API callers and can make repo-aware tool validation consult
    // config from a repo the session will never use. Fail loudly.
    if body.scratch && !body.path.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "validation_failed",
                "message": "Cannot combine scratch with path"
            })),
        )
            .into_response();
    }

    // Validate user inputs for shell injection. For scratch sessions the
    // `path` field is server-provisioned (and clients typically send an
    // empty string), so skip the path entry in that case.
    let mut shell_checks: Vec<(&str, &str)> = vec![(body.extra_args.as_str(), "extra_args")];
    if !body.scratch {
        shell_checks.push((body.path.as_str(), "path"));
    }
    for (value, name) in shell_checks {
        if let Err(msg) = validate_no_shell_injection(value, name) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
    }
    // #2624: `title`/`group` are display labels, not shell input, so they
    // go through `validate_display_label` (control characters only)
    // instead. `tool` is checked against the agent registry below
    // (`validate_session_tool_identity`); `worktree_branch` is re-sanitized
    // for git-ref safety in the builder; `profile` is checked against
    // `list_profiles()` right below. None of the four ever reach a shell,
    // so `validate_no_shell_injection` no longer runs on them.
    if let Err(msg) = validate_display_label(&body.group, "group") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": msg})),
        )
            .into_response();
    }
    if let Some(ref title) = body.title {
        if let Err(msg) = validate_display_label(title, "title") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
    }
    if let Some(ref profile_name) = body.profile {
        // Verify the profile exists. Every profile is a real directory under
        // profiles/; there is no implicitly-valid profile name. Distinguish
        // an enumeration failure (I/O, permissions) from a missing profile
        // so the client doesn't see a 400 when the real problem is server-side.
        let known = match crate::session::list_profiles() {
            Ok(list) => list,
            Err(e) => {
                tracing::error!(
                    target: "server.sessions",
                    "failed to enumerate profiles while validating create_session: {e:#}"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "internal_error",
                        "message": format!("Failed to enumerate profiles: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        if !known.contains(profile_name) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "profile_not_found",
                    "message": format!("Profile '{}' does not exist", profile_name)
                })),
            )
                .into_response();
        }
    }

    let validation_profile = body.profile.as_deref().unwrap_or(&default_profile);
    if !validate_session_tool_identity(
        &body.tool,
        validation_profile,
        std::path::Path::new(&body.path),
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "validation_failed",
                "message": format!("Unknown agent '{}'", body.tool),
            })),
        )
            .into_response();
    }

    // Operator agent allowlist (#3241). Answer here rather than letting the
    // session get built and then fail at spawn, which is the complaint the issue
    // opens with. Applies in and out of CityHall: a shared deployment wants the
    // restriction too, and CityHall's own create path above only proves the agent
    // is ACP-capable, not that the operator permits it.
    //
    // After the tool-identity check above on purpose: an unknown agent is a 400
    // about the request, not a 403 about policy, and judging policy on a name
    // that names nothing would report the wrong reason.
    //
    // Gated on the session actually running ACP. A Structured request for a
    // non-ACP tool is downgraded to a terminal session further down, and terminal
    // sessions are deliberately out of scope (a pane can exec any binary), so
    // refusing here would reject a session the policy does not govern.
    if body.view == crate::session::View::Structured {
        let agent_key = acp_agent_key(&body.tool, body.agent_name.as_deref());
        let profile = validation_profile.to_string();
        let project_path = std::path::PathBuf::from(&body.path);
        let tool = body.tool.clone();
        let agent_name = body.agent_name.clone();
        let acp_capable = tokio::task::spawn_blocking(move || {
            agent_is_acp_capable(&profile, &project_path, &tool, agent_name.as_deref())
        })
        .await
        .unwrap_or(false);
        if acp_capable && !crate::server::api::agent_policy().await.allows(agent_key) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "agent_not_allowed",
                    "message": crate::acp::supervisor::SupervisorError::AgentNotAllowed(
                        agent_key.to_string(),
                    )
                    .to_string(),
                })),
            )
                .into_response();
        }
    }

    // A new session has exactly one conversation source.
    if create_body_has_conflicting_sources(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "message": "Choose only one import or fork source",
            })),
        )
            .into_response();
    }

    let worktree_enabled = create_body_uses_worktree(&body);

    // Importing an existing Claude session (#2276) is tightly scoped: it
    // resumes a specific on-disk session id in its original cwd via the claude
    // structured agent. Reject any request that pairs the id with a different
    // workspace shape, a non-claude agent, or a cwd the id doesn't belong to,
    // so a stale or hand-written request can't seed the transcript in the
    // wrong place. Runs after tool-identity validation so it sits ahead of
    // the build's spawn_blocking but behind the agent check.
    if let Some(import_id) = body
        .import_acp_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let bad = |msg: &str| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response()
        };
        if body.tool != "claude"
            || body
                .agent_name
                .as_deref()
                .is_some_and(|n| !n.trim().is_empty())
        {
            return bad("Importing a Claude session requires the built-in claude agent");
        }
        if body.scratch || worktree_enabled || !body.extra_repo_paths.is_empty() {
            return bad(
                "Importing a Claude session cannot use scratch, a worktree, or extra repos",
            );
        }
        let import_cwd = body.path.trim().to_string();
        let import_id_owned = import_id.to_string();
        let belongs = tokio::task::spawn_blocking(move || {
            crate::session::claude_import::scan_sessions()
                .into_iter()
                .any(|s| s.session_id == import_id_owned && s.cwd == import_cwd)
        })
        .await
        .unwrap_or(false);
        if !belongs {
            return bad("Unknown Claude session for this directory");
        }
    }

    // Forking an existing session: `fork_from` carries the source session's
    // captured session id. A structured request (`view == Structured`) forks
    // through ACP `session/fork` against the parent's `acp_session_id`; a
    // terminal request resumes the parent agent id with the agent's fork flag.
    // The seed is resolved here, ahead of the build, so an unforkable terminal
    // agent or a missing parent id returns a clean 400 rather than failing
    // later. The builder applies the seed: a structured seed forces the
    // structured view and sets the one-shot `fork_pending`/`import_pending`
    // markers; a terminal seed pre-pins the child id and the Fork intent.
    let mut fork_seed = match body
        .fork_from
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(parent_id) => {
            // Reject a malformed parent id up front. `build_fork_flags` fails
            // closed on an invalid id (no fork flags), which would otherwise
            // start a fresh, non-forked session with no error to the caller.
            if !crate::session::capture::is_valid_session_id(parent_id) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "fork_invalid",
                        "message": "fork_from is not a valid session id",
                    })),
                )
                    .into_response();
            }
            let structured = body.view == crate::session::View::Structured;
            // A structured fork only runs over a live ACP connection. Reject it
            // here for a non-ACP agent rather than letting the post-build
            // capability check silently downgrade it to a non-forked terminal
            // session (the fork markers would be cleared, dropping the fork).
            if structured
                && !crate::session::fork::structured_fork_capable(
                    &body.tool,
                    body.agent_name.as_deref(),
                )
            {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "fork_unsupported",
                        "message": "A structured fork requires an ACP agent that supports forking",
                    })),
                )
                    .into_response();
            }
            match resolve_create_fork_seed(&body.tool, parent_id, structured) {
                Ok(seed) => Some(seed),
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "fork_unsupported",
                            "message": "This agent or session cannot be forked",
                        })),
                    )
                        .into_response();
                }
            }
        }
        None => None,
    };

    if let Some(url) = body.callback_url.as_deref() {
        if let Err(msg) = crate::server::callback::validate_callback_url(url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
    }

    if let Some(key) = body.idempotency_key.as_deref() {
        if key.is_empty() || key.len() > IDEMPOTENCY_KEY_MAX_LEN {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "validation_failed",
                    "message": format!(
                        "idempotency_key must be 1-{IDEMPOTENCY_KEY_MAX_LEN} characters"
                    ),
                })),
            )
                .into_response();
        }
    }

    // Idempotency: hold a per-key lock across the check-and-create so two
    // concurrent requests sharing a new key can't both scan-miss and both
    // create a session. The guard lives until this handler returns (Rust
    // drops it at end of scope); only requests sharing this exact key
    // serialize, not general session-create throughput.
    let _idempotency_guard = if let Some(key) = body.idempotency_key.as_deref() {
        let lock = state.idempotency_lock(key).await;
        let guard = lock.lock_owned().await;
        let existing = {
            let instances = state.instances.read().await;
            find_by_idempotency_key(&instances, key).map(|inst| inst.id.clone())
        };
        if let Some(id) = existing {
            return created_session_response(&state, &id, Vec::new(), StatusCode::OK).await;
        }
        Some(guard)
    } else {
        None
    };

    if let Some(source_id) = body
        .fork_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        fork_seed = Some(
            match resolve_canonical_fork_seed(&state, &body, source_id).await {
                Ok(seed) => seed,
                Err(response) => return response,
            },
        );
    }

    let profile = body.profile.unwrap_or(default_profile);

    let spec = crate::server::session_spawn::StructuredSessionSpec {
        title: body.title,
        size: body.size,
        path: body.path,
        group: body.group,
        tool: body.tool,
        worktree_enabled,
        worktree_branch: body.worktree_branch,
        create_new_branch: body.create_new_branch,
        base_branch: body.base_branch,
        sandbox: body.sandbox,
        sandbox_image: body.sandbox_image,
        yolo_mode: body.yolo_mode,
        extra_env: body.extra_env,
        extra_args: body.extra_args,
        command_override: body.command_override,
        extra_repo_paths: body.extra_repo_paths,
        repo_base_branches: body
            .repo_bases
            .into_iter()
            .map(|r| (r.repo, r.base_branch))
            .collect(),
        scratch: body.scratch,
        trust_hooks: body.trust_hooks,
        trust_review: body.trust_review,
        custom_instruction: body.custom_instruction,
        callback_url: body.callback_url,
        idempotency_key: body.idempotency_key,
        profile,
        // Never decoded from the request body: only the plugin host path
        // stamps these, through create_structured_session. See #2897.
        created_by_plugin: None,
        plugin_create_idempotency: None,
        pending_initial_turn: None,
        acp_mode_id: None,
        view: body.view,
        agent_name: body.agent_name,
        agent_model: body.agent_model,
        agent_effort: body.agent_effort,
        import_acp_session_id: body.import_acp_session_id,
        fork_seed,
    };

    match state
        .session_service
        .create_structured_session(spec, None, None, None)
        .await
    {
        Ok((outcome, _created)) => {
            let instance = outcome.instance;
            if query.wait.as_deref() == Some("ready") && instance.status == Status::Starting {
                let _ = wait_until_left_starting(&state, &instance.id, WAIT_READY_TIMEOUT).await;
            }
            created_session_response(&state, &instance.id, outcome.warnings, StatusCode::CREATED)
                .await
        }
        Err(e) => {
            if e.is::<crate::server::session_service::CreationCancelled>() {
                let code = crate::daemon::ApiErrorCode::CreationCancelled;
                return (code.status(), code.header()).into_response();
            }
            if e.is::<CreationTrustChanged>() {
                let code = crate::daemon::ApiErrorCode::CreationTrustChanged;
                return (code.status(), code.header()).into_response();
            }
            if e.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if let Some(panicked) =
                e.downcast_ref::<crate::server::session_spawn::SessionBuildPanicked>()
            {
                tracing::error!(target: "http.api.sessions", "Session creation panicked: {}", panicked.0);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
                )
                    .into_response();
            }
            // A repo whose hooks need approval gets a distinct, structured
            // response so the caller can surface the commands and resubmit with
            // `trust_hooks: true` (#2066), rather than the opaque create_failed.
            if let Some(needs_trust) = e.downcast_ref::<HooksNeedTrust>() {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "hooks_need_trust",
                        "message": "Repository hooks require trust. Resubmit with trust_hooks: true to approve.",
                        "on_create": needs_trust.on_create,
                        "on_launch": needs_trust.on_launch,
                        "on_destroy": needs_trust.on_destroy,
                        "needs_mcp_trust": needs_trust.needs_mcp_trust,
                    })),
                )
                    .into_response();
            }
            tracing::warn!(target: "http.api.sessions", "Session creation failed: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "create_failed", "message": public_create_session_error(&e)})),
            )
                .into_response()
        }
    }
}
async fn created_session_response(
    state: &Arc<AppState>,
    id: &str,
    warnings: Vec<String>,
    status: StatusCode,
) -> axum::response::Response {
    let snapshot = match state.runtime.publish(state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(row) = snapshot
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
    else {
        return crate::server::api::session_gone_after_persist();
    };
    let mut response = std::borrow::Cow::Borrowed(row);
    if !warnings.is_empty() {
        response.to_mut().warnings = warnings;
    }
    crate::server::runtime::mutation_response(&snapshot.value.cursor, (status, Json(response)))
}

/// Pick the client-facing message for a failed session creation.
///
/// The full error is always logged server-side; this only governs what
/// reaches the browser. We whitelist the well-typed `GitError` variants
/// that carry a clear, actionable, credential-free message (a branch name
/// or a worktree path the user chose) and let everything else fall back to
/// the generic string. This keeps raw git stderr, libgit2 internals, IO
/// paths, and arbitrary `bail!` strings off the wire even though the
/// duplicate-worktree case now surfaces its real message.
pub(super) fn public_create_session_error(e: &anyhow::Error) -> String {
    if let Some(git_err) = e.chain().find_map(|c| c.downcast_ref::<GitError>()) {
        match git_err {
            GitError::WorktreeAlreadyExists(_)
            | GitError::BranchAlreadyCheckedOut(_)
            | GitError::BranchNotFound(_)
            | GitError::NotAGitRepo => return git_err.to_string(),
            // Raw command output / libgit2 / IO: not safe to expose.
            GitError::WorktreeCommandFailed(_)
            | GitError::CloneFailed(_)
            | GitError::WorktreeNotFound(_)
            | GitError::Git2Error(_)
            | GitError::IoError(_) => {}
        }
    }
    "Failed to create session".to_string()
}

// --- Ensure agent session ---

/// Copy fields the start path mutated on the working `Instance` clone back
/// onto the in-memory `state.instances` entry after a successful restart.
///
/// `agent_session_id` is the load-bearing one: Claude's `acquire_session_id`
/// generates a fresh UUID at launch time and `persist_session_id` writes it
/// to disk, but the in-memory state lives in a separate Vec that the 2s
/// status poller refreshes from disk on its own cadence. Without this sync,
/// a rapid second restart inside that window would see a stale
/// `agent_session_id = None` and generate (and persist) a new UUID,
/// silently orphaning the previous Claude conversation.
pub(super) fn apply_post_restart_identity_sync(
    live: &mut Instance,
    before: &Instance,
    started: &Instance,
) {
    if started.lifecycle_generation < live.lifecycle_generation {
        return;
    }
    // Treat the pre-restart snapshot as a CAS baseline for peer-writable
    // identity fields. If a poller/CLI/TUI peer changed the sid while the
    // restart clone was blocking, that newer sid and its marker stay
    // authoritative.
    let generation_can_merge = live.omp_capture_generation == before.omp_capture_generation
        || live.omp_capture_generation == started.omp_capture_generation;
    let sid_unchanged = live.agent_session_id == before.agent_session_id;
    let marker_unchanged = live.resume_probe_failed_sid == before.resume_probe_failed_sid;
    if generation_can_merge {
        live.omp_capture_generation = started.omp_capture_generation.clone();
        live.session_id_poller = started.session_id_poller.clone();
        if sid_unchanged {
            live.agent_session_id = started.agent_session_id.clone();
        }
    } else if started.session_id_poller_is_running() {
        // The worker follows the pane name and will rebind itself to the
        // concurrently published generation on its next metadata refresh.
        live.session_id_poller = started.session_id_poller.clone();
    }
    if generation_can_merge && marker_unchanged && live.agent_session_id == started.agent_session_id
    {
        live.resume_probe_failed_sid = started.resume_probe_failed_sid.clone();
    }
    // A running restart poller means the working clone's repair schedule was
    // cleared on start; the live row must not keep the stale backoff.
    if started.session_id_poller_is_running() {
        live.poller_repair.reset();
    }
    live.lifecycle_generation = started.lifecycle_generation;
}

pub(super) fn apply_post_restart_sync(
    live: &mut Instance,
    before: &Instance,
    started: &Instance,
) -> bool {
    if started.lifecycle_generation < live.lifecycle_generation {
        return false;
    }
    live.merge_post_restart_with_baseline(before, started);
    live.last_error = if started.status == Status::Error {
        started.last_error.clone()
    } else {
        None
    };
    live.last_error_check = started.last_error_check;
    live.last_start_time = started.last_start_time;
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
    true
}

/// Narrow sibling of [`apply_post_restart_sync`] that propagates only the
/// fields the resume path is responsible for: the post-probe
/// `agent_session_id`, the `resume_probe_failed_sid` marker, and the updated
/// `retroactive_capture_excludes`.
///
/// Intended for error paths where the cascade may have run but the caller
/// does not want to touch user-visible status fields. `NotRunning` is the
/// canonical use case: a recoverable transient state where overwriting
/// `live.status` with `started.status` (typically `Starting` from the
/// post-cascade `finalize_launch`) would briefly mis-paint a broken pane
/// as `Starting` until the 2s status poll loop reconciles.
pub(super) fn apply_cascade_state_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    if started.lifecycle_generation < live.lifecycle_generation {
        return;
    }
    apply_post_restart_identity_sync(live, before, started);
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
}
