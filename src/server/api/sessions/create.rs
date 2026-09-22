//! Session creation: validation, hooks, idempotency, restart sync.

use super::*;

// --- Create session ---

/// One repo's creation base in a create-session request. See #3329.
#[derive(Deserialize)]
pub struct RepoBaseInput {
    pub repo: String,
    pub base_branch: String,
}

#[derive(Deserialize)]
pub struct CreateSessionBody {
    pub title: Option<String>,
    pub path: String,
    pub tool: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub yolo_mode: bool,
    /// Explicit worktree opt-in. When omitted or false, legacy callers that
    /// send `worktree_branch` still opt into worktree mode.
    #[serde(default)]
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    #[serde(default)]
    pub create_new_branch: bool,
    /// Branch the new worktree branch is based on, honored only when
    /// `create_new_branch` is true. Empty falls back to the repo's detected
    /// default branch.
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub extra_args: String,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub extra_env: Vec<String>,
    #[serde(default)]
    pub extra_repo_paths: Vec<String>,
    /// Per-repo base branches as `{ repo, base_branch }`. Outranks
    /// `base_branch`, which stays the base for every repo no entry names (#3329).
    #[serde(default)]
    pub repo_bases: Vec<RepoBaseInput>,
    #[serde(default)]
    pub command_override: String,
    #[serde(default)]
    pub custom_instruction: Option<String>,
    pub profile: Option<String>,
    /// How the new session renders: `structured` or `terminal`, defaulting to
    /// `terminal`. Re-validated against real ACP capability below, so a tampered
    /// request cannot force the structured view onto a non-ACP tool.
    #[serde(default)]
    pub view: crate::session::View,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub agent_model: Option<String>,
    #[serde(default)]
    pub agent_effort: Option<String>,
    /// Scratch session: the server provisions a fresh directory and ignores
    /// `path`. Mutually exclusive with `worktree_branch` and `extra_repo_paths`.
    #[serde(default)]
    pub scratch: bool,
    /// Approve the repo's `on_create` hooks (and any project MCP) for this
    /// non-interactive create, mirroring the CLI `--trust-hooks` flag (#2066).
    /// Without it a repo needing approval returns a structured
    /// `hooks_need_trust` error. Already-trusted hooks run regardless.
    #[serde(default)]
    pub trust_hooks: Option<bool>,
    /// Import an existing Claude Code session by its on-disk id. The new
    /// session adopts it as `acp_session_id`, is forced to the structured view,
    /// and seeds its transcript from history replay. `path` must be the
    /// session's original cwd (#2276).
    #[serde(default)]
    pub import_acp_session_id: Option<String>,
    /// Fork an existing session from its captured session id, leaving the
    /// original untouched. A structured fork drives ACP `session/fork` against
    /// the parent's `acp_session_id`; a terminal fork resumes the parent
    /// `agent_session_id` with the agent's fork flag. A structured fork of a
    /// non-ACP agent is rejected rather than silently downgraded.
    #[serde(default)]
    pub fork_from: Option<String>,
    /// Work-queue completion callback, fired when the session reaches Idle,
    /// Waiting, or Error. Must be `http`/`https` and must not resolve to a
    /// loopback/private/link-local address, checked again on every dispatch.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Idempotency key: a retry with the same key returns the existing session
    /// rather than creating a duplicate. Persisted on the instance, so it
    /// survives a daemon restart (#3156).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Hard cap on one `idempotency_key`'s length, so a request cannot persist an
/// arbitrarily large string. Entry count is bounded separately by the pruning in
/// `AppState::idempotency_lock`.
const IDEMPOTENCY_KEY_MAX_LEN: usize = 200;

/// Find a prior session created with this `idempotency_key`. Scans trashed
/// instances too, so a retry against a soft-deleted session returns it; a
/// hard-deleted one falls through to a fresh create.
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

/// Resolve the one-shot fork seed for a `fork_from` create. A structured
/// request forks through ACP `session/fork`; a terminal one resumes the parent
/// agent id with the agent's fork flag. `Err` reports an unforkable terminal
/// agent or a missing parent id.
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

/// True when a create asks to both import and fork. The two seed from
/// different sources, so allowing both yields a contradictory session.
pub(super) fn both_import_and_fork_set(body: &CreateSessionBody) -> bool {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    set(&body.import_acp_session_id) && set(&body.fork_from)
}

/// Alias for [`crate::session::fork::structured_fork_capable`], shared by the
/// `SessionResponse.acp_can_fork` projection and the create-time guard so the
/// web affordance and the guard cannot drift.
pub(super) fn agent_is_structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    crate::session::fork::structured_fork_capable(tool, agent_name)
}

/// The ACP registry key a create resolves to: an explicit `agent_name`, else
/// the tool name. Shared by the capability and allowlist checks (#3241).
fn acp_agent_key<'a>(tool: &'a str, agent_name: Option<&'a str>) -> &'a str {
    agent_name.filter(|s| !s.is_empty()).unwrap_or(tool)
}

/// True iff the agent can run a structured (ACP) session here. Mirrors the
/// post-build capability check so CityHall can reject a non-ACP agent up front
/// instead of silently downgrading to the terminal view (#7).
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
    // different `agent_acp_cmd` entry, so looking up `tool` would report
    // not-capable for an agent that spawns fine.
    let session = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        profile,
        project_path,
    )
    .session;
    session
        .agent_acp_cmd
        .get(resolved)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(resolved, cmd).is_ok())
        // A custom agent inheriting a registry-backed base spawns through that
        // base adapter, so report it capable up front.
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

/// Insert `instance`, replacing any entry with the same id rather than pushing
/// a second copy.
///
/// `create_session` persists to disk before pushing here, so a `status_poll_loop`
/// tick in that window can insert the row first. A blind push would then list the
/// session twice until the next tick collapsed them.
pub(crate) fn upsert_instance(
    instances: &mut Vec<crate::session::Instance>,
    instance: crate::session::Instance,
) {
    if let Some(existing) = instances.iter_mut().find(|i| i.id == instance.id) {
        *existing = instance;
    } else {
        instances.push(instance);
    }
}

/// Remove `id`, bumping `mutation_epoch` only when a row was actually removed.
///
/// Every delete-path removal must bump while still holding the `instances` write
/// lock, because a reloader compares the epoch under that same lock; a removal
/// that skips the bump lets a pre-delete disk snapshot resurrect the row.
/// Bumping only on a real removal keeps the final commit from spending an epoch
/// the early removal already covered.
pub(crate) fn remove_instance(
    instances: &mut Vec<crate::session::Instance>,
    id: &str,
    mutation_epoch: &std::sync::atomic::AtomicU64,
) {
    let before = instances.len();
    instances.retain(|i| i.id != id);
    if instances.len() != before {
        mutation_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Marks a create refused because the repo's hooks (or project MCP) need
/// approval and the request did not pass `trust_hooks: true` (#2066). The outer
/// match downcasts this to emit a structured `hooks_need_trust` response.
#[derive(Debug)]
pub(crate) struct HooksNeedTrust {
    /// The `on_create` commands that would run, for display in the prompt.
    pub(crate) on_create: Vec<String>,
    /// The `on_launch` commands the same approval would trust. They do not run
    /// on this create, but the recorded trust covers them later, so the prompt
    /// must show them.
    pub(crate) on_launch: Vec<String>,
    /// Likewise for `on_destroy`, run when a session is deleted.
    pub(crate) on_destroy: Vec<String>,
    /// True when the repo's `.mcp.json` also needs approval at this fingerprint.
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

/// Resolved plan for a web-API create's `on_create` hooks (#2066). Computed
/// before the worktree is built so an untrusted repo fails fast without leaving
/// an orphan worktree.
#[derive(Debug)]
pub(crate) struct CreateHookPlan {
    /// Already merged (repo overrides global/profile per type).
    pub(crate) hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    /// `(hooks_hash, mcp_hash)` to persist into `trusted_repos.toml` when the
    /// caller passed `trust_hooks: true`. `None` when nothing needs recording.
    pub(crate) trust_write: Option<(Option<String>, Option<String>)>,
}

impl CreateHookPlan {
    pub(crate) fn on_create(&self) -> &[String] {
        self.hooks.as_ref().map_or(&[], |h| &h.hooks().on_create)
    }
}

/// Resolve the repo's `on_create` hooks and the trust decision. Returns
/// `Err(HooksNeedTrust)` when a surface needs approval and the caller did not
/// opt in. Mirrors the CLI `--trust-hooks` path in `src/cli/add.rs`.
pub(crate) fn resolve_create_hook_plan(
    profile: &str,
    project_path: &std::path::Path,
    scratch: bool,
    trust_hooks_requested: bool,
) -> anyhow::Result<CreateHookPlan> {
    use crate::session::config::repo_config::{self, TrustSurface};

    // Scratch sessions have no repo-anchored config, so skip the repo trust
    // check and fall back to profile-level hooks, matching the CLI.
    if scratch {
        return Ok(CreateHookPlan {
            hooks: repo_config::ResolvedHooks::global(profile),
            trust_write: None,
        });
    }

    let trust = match repo_config::check_repo_trust(project_path) {
        Ok(t) => t,
        Err(e) => {
            // A failed trust check must not drop already-trusted global/profile
            // hooks; degrade to profile hooks like the CLI does.
            tracing::warn!(target: "http.api.sessions", "Failed to check repo trust: {e:#}");
            return Ok(CreateHookPlan {
                hooks: repo_config::ResolvedHooks::global(profile),
                trust_write: None,
            });
        }
    };

    // Refuse only when HOOKS need approval. Project MCP is not a gate: the
    // supervisor skips an untrusted `.mcp.json` at spawn, so blocking creation
    // would be more aggressive than the CLI. A passed `trust_hooks` still
    // records MCP trust below.
    if trust.hooks.needs_trust() && !trust_hooks_requested {
        // Approving trusts the repo's whole hooks hash, so the refusal must
        // list every hook type that trust would cover, not just on_create.
        let merged = match &trust.hooks {
            TrustSurface::Trusted(h) | TrustSurface::NeedsTrust { config: h, .. } => {
                repo_config::merge_hooks_for_display(profile, h)
            }
            TrustSurface::Absent => {
                repo_config::resolve_global_profile_hooks(profile).unwrap_or_default()
            }
        };
        return Err(anyhow::Error::new(HooksNeedTrust {
            on_create: merged.on_create,
            on_launch: merged.on_launch,
            on_destroy: merged.on_destroy,
            needs_mcp_trust: trust.mcp.needs_trust(),
        }));
    }

    // Approved (nothing needed prompting, or the caller passed trust_hooks).
    let repo_hooks = match &trust.hooks {
        TrustSurface::Trusted(h) | TrustSurface::NeedsTrust { config: h, .. } => Some(h.clone()),
        TrustSurface::Absent => None,
    };
    let trust_write = if trust_hooks_requested {
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
    let hooks = match repo_hooks {
        Some(h) => repo_config::ResolvedHooks::with_repo(
            profile,
            std::path::Path::new(&trust.project_path),
            h,
        ),
        None => repo_config::ResolvedHooks::global(profile),
    };
    Ok(CreateHookPlan { hooks, trust_write })
}

/// Record pending trust and run the planned `on_create` hooks (#2066), after
/// the worktree exists. Output is streamed to a discarded channel so the shared
/// executor's terminal-detach (credential-prompt suppression) still applies.
pub(crate) fn run_create_hooks(
    instance: &mut Instance,
    plan: &CreateHookPlan,
    project_path: &std::path::Path,
) -> anyhow::Result<()> {
    use crate::session::config::repo_config;

    if let Some((hooks_hash, mcp_hash)) = &plan.trust_write {
        repo_config::trust_repo(project_path, hooks_hash.as_deref(), mcp_hash.as_deref())?;
    }

    if plan.on_create().is_empty() {
        return Ok(());
    }

    let hook_env = repo_config::lifecycle_env_vars(instance);
    // No live consumer: drop the receiver so sends no-op while the executor's
    // detach-tty behavior and error-tail capture still apply.
    let (progress_tx, progress_rx) = std::sync::mpsc::channel::<repo_config::HookProgress>();
    drop(progress_rx);

    if instance.sandbox_info.is_some() {
        instance.get_container_for_instance()?;
        let workdir = instance.container_workdir();
        if let Some(sandbox) = instance.sandbox_info.as_ref() {
            repo_config::execute_hooks_in_container_streamed(
                plan.on_create(),
                &sandbox.container_name,
                &workdir,
                &progress_tx,
                &hook_env,
            )?;
        }
    } else {
        repo_config::execute_hooks_streamed(
            plan.on_create(),
            std::path::Path::new(&instance.project_path),
            &progress_tx,
            &hook_env,
        )?;
    }
    Ok(())
}

/// CityHall structured-target gate for per-session routes. CityHall only
/// creates structured sessions, so a mutation must refuse any non-structured or
/// unknown target; otherwise a locked-down client could respawn, destroy or edit
/// a pre-existing terminal session from the TUI or another client. Returns the
/// canonical 403, never a 404, so the mode does not leak which ids exist (#7).
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
/// a structured session this mode created (#7).
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
/// until the new session leaves `Starting` (or a bounded timeout elapses), so a
/// caller sending a message straight after create does not race startup.
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
/// Subscribes to `status_tx` before the first check, so a transition landing in
/// between is queued rather than lost. On `Lagged`, re-reads live state instead
/// of trusting the broadcast position. `None` only if the instance vanished.
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

    if state.cityhall_mode {
        // CityHall sessions are server-derived and locked down. Every
        // client-supplied field that could escape the mode is neutralized (#7).
        let projects = crate::session::projects::load_merged(&state.profile).unwrap_or_default();
        if projects.is_empty() {
            return api_error(
                StatusCode::BAD_REQUEST,
                "cityhall_no_projects",
                "CityHall mode requires at least one configured project",
            );
        }
        body.scratch = false;
        // Reset every client-controllable spawn / branch field. Deriving
        // path/repos/view is not enough: `command_override` is load-bearing,
        // since the ACP supervisor validates the registry-default binary but
        // then adopts the client's `argv[0]` unchecked, so a shell command on a
        // registry ACP tool would pass the gate below and spawn anything (#7).
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
        // would run operator-repo commands from a locked-down user (#7).
        body.trust_hooks = None;
        // The "primary" repo is the first entry in merged registry order; the
        // rest ride along as workspace repos. The pick only affects labeling.
        // Non-empty is checked above, so `next()` is Some.
        let mut paths = projects.into_iter().map(|p| p.path);
        body.path = paths.next().unwrap();
        body.extra_repo_paths = paths.collect();
        body.view = crate::session::View::Structured;
        // Fork and import resume an existing agent session and would bypass the
        // server-derived path and ACP gate, so they are not honored here.
        body.fork_from = None;
        body.import_acp_session_id = None;
        let profile = body
            .profile
            .clone()
            .unwrap_or_else(|| state.profile.clone());
        if !agent_is_acp_capable(
            &profile,
            std::path::Path::new(&body.path),
            &body.tool,
            body.agent_name.as_deref(),
        ) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "cityhall_agent_not_acp",
                "CityHall mode requires an ACP-capable agent",
            );
        }
    }

    // Scratch sessions are server-provisioned, so the worktree path is the
    // wrong model. Reject before the builder, for a clear 400 instead of a
    // less-specific bail surfaced as 500.
    if create_body_combines_scratch_and_worktree(&body) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "Cannot combine scratch with worktree mode",
        );
    }
    if body.scratch && !body.extra_repo_paths.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "Cannot combine scratch with extra_repo_paths",
        );
    }
    // The builder ignores `path` in scratch mode, but accepting both silently
    // can make repo-aware tool validation consult config from a repo the session
    // never uses. Fail loudly.
    if body.scratch && !body.path.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "Cannot combine scratch with path",
        );
    }

    // Validate user inputs for shell injection. `path` is server-provisioned
    // for scratch sessions, so skip it there.
    let mut shell_checks: Vec<(&str, &str)> = vec![(body.extra_args.as_str(), "extra_args")];
    if !body.scratch {
        shell_checks.push((body.path.as_str(), "path"));
    }
    for (value, name) in shell_checks {
        if let Err(msg) = validate_no_shell_injection(value, name) {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
        }
    }
    // #2624: `title`/`group` are display labels, so they go through
    // `validate_display_label` instead. `tool` is checked against the registry,
    // `worktree_branch` is re-sanitized for git-ref safety in the builder, and
    // `profile` is checked against `list_profiles()`. None reaches a shell.
    if let Err(msg) = validate_display_label(&body.group, "group") {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
    }
    if let Some(ref title) = body.title {
        if let Err(msg) = validate_display_label(title, "title") {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
        }
    }
    if let Some(ref profile_name) = body.profile {
        // Every profile is a real directory under profiles/. Distinguish an
        // enumeration failure from a missing profile so the client does not see
        // a 400 when the real problem is server-side.
        let known = match crate::session::list_profiles() {
            Ok(list) => list,
            Err(e) => {
                tracing::error!(
                    target: "server.sessions",
                    "failed to enumerate profiles while validating create_session: {e:#}"
                );
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("Failed to enumerate profiles: {e}"),
                );
            }
        };
        if !known.contains(profile_name) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "profile_not_found",
                format!("Profile '{}' does not exist", profile_name),
            );
        }
    }

    let validation_profile = body.profile.as_deref().unwrap_or(&state.profile);
    if !validate_session_tool_identity(
        &body.tool,
        validation_profile,
        std::path::Path::new(&body.path),
    ) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            format!("Unknown agent '{}'", body.tool),
        );
    }

    // Operator agent allowlist (#3241), answered here rather than failing at
    // spawn. Applies outside CityHall too, whose create path only proves the
    // agent is ACP-capable, not that the operator permits it. Placed after the
    // tool-identity check so an unknown agent reports a 400 about the request
    // rather than a 403 about policy, and gated on the session actually running
    // ACP, since terminal sessions are out of scope.
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
            return api_error(
                StatusCode::FORBIDDEN,
                "agent_not_allowed",
                crate::acp::supervisor::SupervisorError::AgentNotAllowed(agent_key.to_string())
                    .to_string(),
            );
        }
    }

    // Import and fork are mutually exclusive: each seeds from a different
    // source, so honoring both would leave a half-imported, half-forked session.
    if both_import_and_fork_set(&body) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Cannot set both import_acp_session_id and fork_from",
        );
    }

    let worktree_enabled = create_body_uses_worktree(&body);

    // Importing a Claude session (#2276) is tightly scoped: it resumes one
    // on-disk id in its original cwd via the claude structured agent. Reject an
    // id paired with a different workspace shape, a non-claude agent, or a
    // foreign cwd. Runs after tool-identity validation, ahead of the build.
    if let Some(import_id) = body
        .import_acp_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let bad = |msg: &str| api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
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

    // `fork_from` carries the source session's captured id. The seed is
    // resolved ahead of the build so an unforkable terminal agent or missing
    // parent id returns a clean 400. The builder applies it: a structured seed
    // forces the structured view and sets the one-shot fork/import markers; a
    // terminal seed pre-pins the child id and the Fork intent.
    let fork_seed = match body
        .fork_from
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(parent_id) => {
            // `build_fork_flags` fails closed on an invalid id, which would
            // otherwise start a fresh, non-forked session with no error.
            if !crate::session::capture::is_valid_session_id(parent_id) {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "fork_invalid",
                    "fork_from is not a valid session id",
                );
            }
            let structured = body.view == crate::session::View::Structured;
            // A structured fork needs a live ACP connection. Reject it here, or
            // the post-build capability check silently downgrades it to a
            // non-forked terminal session, dropping the fork.
            if structured
                && !agent_is_structured_fork_capable(&body.tool, body.agent_name.as_deref())
            {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "fork_unsupported",
                    "A structured fork requires an ACP agent that supports forking",
                );
            }
            match resolve_create_fork_seed(&body.tool, parent_id, structured) {
                Ok(seed) => Some(seed),
                Err(_) => {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "fork_unsupported",
                        "This agent or session cannot be forked",
                    );
                }
            }
        }
        None => None,
    };

    if let Some(url) = body.callback_url.as_deref() {
        if let Err(msg) = crate::server::callback::validate_callback_url(url) {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
        }
    }

    if let Some(key) = body.idempotency_key.as_deref() {
        if key.is_empty() || key.len() > IDEMPOTENCY_KEY_MAX_LEN {
            return api_error(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                format!("idempotency_key must be 1-{IDEMPOTENCY_KEY_MAX_LEN} characters"),
            );
        }
    }

    // Idempotency: hold a per-key lock across the check-and-create so two
    // concurrent requests sharing a new key cannot both scan-miss and create.
    // Only requests sharing this exact key serialize.
    let _idempotency_guard = if let Some(key) = body.idempotency_key.as_deref() {
        let lock = state.idempotency_lock(key).await;
        let guard = lock.lock_owned().await;
        let existing = {
            let instances = state.instances.read().await;
            find_by_idempotency_key(&instances, key).map(|inst| {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            })
        };
        if let Some(resp) = existing {
            return (StatusCode::OK, Json(resp)).into_response();
        }
        Some(guard)
    } else {
        None
    };

    let profile = body.profile.unwrap_or_else(|| state.profile.clone());

    let spec = crate::server::session_spawn::StructuredSessionSpec {
        title: body.title,
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
        custom_instruction: body.custom_instruction,
        callback_url: body.callback_url,
        idempotency_key: body.idempotency_key,
        profile,
        // Never decoded from the request body: only the plugin host path
        // stamps these, through create_structured_session (#2897).
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
            let mut resp = SessionResponse::from_instance(
                &instance,
                crate::claude_settings::read_tui_fullscreen(),
            );
            resp.warnings = outcome.warnings;
            // Carry the resolved tie value (#1927); list_sessions' overlay does
            // not run here, so a managed worktree would report untied until the
            // next list refresh.
            if resp.has_managed_worktree {
                resp.tie_workdir_to_name =
                    crate::session::config::profile_config::resolve_config_or_warn(
                        &instance.source_profile,
                    )
                    .session
                    .tie_workdir_to_name;
            }
            if !resp.acp_capable {
                let session =
                    crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                        &instance.source_profile,
                        std::path::Path::new(&instance.project_path),
                    )
                    .session;
                resp.acp_capable = custom_agent_acp_capable(&session, &instance.tool);
            }

            if query.wait.as_deref() == Some("ready") && instance.status == Status::Starting {
                if let Some(fresh) =
                    wait_until_left_starting(&state, &instance.id, WAIT_READY_TIMEOUT).await
                {
                    // `wire_str`, not `as_str`: must match the casing this
                    // endpoint returns without `?wait=ready`, or a dispatcher
                    // polling `GET /api/sessions` never matches (#3187).
                    resp.status = fresh.status.wire_str().to_string();
                    resp.last_error = fresh.last_error;
                }
            }

            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => {
            // A build-task panic keeps its 500; a plain build failure is a 400.
            if let Some(panicked) =
                e.downcast_ref::<crate::server::session_spawn::SessionBuildPanicked>()
            {
                tracing::error!(target: "http.api.sessions", "Session creation panicked: {}", panicked.0);
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "Internal server error",
                );
            }
            // A repo whose hooks need approval gets a structured response so the
            // caller can surface the commands and resubmit (#2066).
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
            api_error(
                StatusCode::BAD_REQUEST,
                "create_failed",
                public_create_session_error(&e),
            )
        }
    }
}

/// Pick the client-facing message for a failed session creation.
///
/// The full error is always logged server-side. Only the well-typed `GitError`
/// variants carrying a credential-free, actionable message pass through; raw git
/// stderr, libgit2 internals, IO paths and arbitrary `bail!` strings fall back
/// to the generic string.
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

/// Copy fields the start path mutated on the working `Instance` clone back onto
/// the `state.instances` entry after a successful restart.
///
/// `agent_session_id` is load-bearing: it is generated and persisted at launch,
/// but in-memory state is only refreshed from disk by the 2s poller. Without
/// this sync a rapid second restart would see `None`, generate a new UUID, and
/// orphan the previous Claude conversation.
pub(super) fn apply_post_restart_identity_sync(
    live: &mut Instance,
    before: &Instance,
    started: &Instance,
) {
    if started.lifecycle_generation < live.lifecycle_generation {
        return;
    }
    // The pre-restart snapshot is a CAS baseline for peer-writable identity
    // fields: a sid a poller/CLI/TUI peer wrote while the clone was blocking
    // stays authoritative.
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
        // The worker follows the pane name and rebinds itself to the
        // concurrently published generation on its next metadata refresh.
        live.session_id_poller = started.session_id_poller.clone();
    }
    if generation_can_merge && marker_unchanged && live.agent_session_id == started.agent_session_id
    {
        live.resume_probe_failed_sid = started.resume_probe_failed_sid.clone();
    }
    // A running restart poller means the working clone's repair schedule was
    // cleared on start, so the live row must not keep the stale backoff.
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
/// resume path's fields: the post-probe `agent_session_id`, the
/// `resume_probe_failed_sid` marker, and `retroactive_capture_excludes`.
///
/// For error paths that must not touch user-visible status. `NotRunning` is the
/// case: overwriting `live.status` with a post-cascade `Starting` would
/// mis-paint a broken pane until the 2s poll reconciles.
pub(super) fn apply_cascade_state_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    if started.lifecycle_generation < live.lifecycle_generation {
        return;
    }
    apply_post_restart_identity_sync(live, before, started);
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
}
