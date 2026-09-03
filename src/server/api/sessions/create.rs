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
    /// Branch the new worktree branch is based on. Only honored when
    /// `create_new_branch` is true; the server ignores it otherwise.
    /// `None` (or empty) falls back to the repository's detected
    /// default branch. See #948.
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
    /// Base branch for individual repos, as `{ repo, base_branch }` entries.
    /// `repo` is a repo directory name or one of the paths in `path` /
    /// `extra_repo_paths`. Outranks `base_branch`, which stays the base for
    /// every repo no entry names. See #3329.
    #[serde(default)]
    pub repo_bases: Vec<RepoBaseInput>,
    #[serde(default)]
    pub command_override: String,
    #[serde(default)]
    pub custom_instruction: Option<String>,
    pub profile: Option<String>,
    /// How the new session should render: `structured` or `terminal`. The
    /// bundled wizard sends an explicit value (`structured` for ACP-capable
    /// tools, `terminal` otherwise); other API callers may omit it, in which
    /// case it defaults to `terminal`. The value is re-validated against real
    /// ACP capability below before being persisted, so a tampered request
    /// can't force the structured view on a non-ACP tool.
    #[serde(default)]
    pub view: crate::session::View,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub agent_model: Option<String>,
    #[serde(default)]
    pub agent_effort: Option<String>,
    /// Scratch session: server provisions a fresh directory under
    /// `<app_dir>/scratch/<id>/` and ignores `path`. Mutually exclusive with
    /// `worktree_branch` and `extra_repo_paths`; the handler returns 400
    /// on either combination.
    #[serde(default)]
    pub scratch: bool,
    /// Approve the repo's `on_create` lifecycle hooks (and any project MCP) for
    /// this non-interactive create, mirroring the CLI `--trust-hooks` flag and
    /// the TUI trust dialog (#2066). When a repo defines hooks that need
    /// approval and this is unset/false, the handler returns a structured
    /// `hooks_need_trust` error so the caller can prompt and resubmit with
    /// `trust_hooks: true`. Already-trusted hooks run regardless.
    #[serde(default)]
    pub trust_hooks: Option<bool>,
    /// Import an existing Claude Code session: the on-disk session id (the
    /// `<sessionId>.jsonl` stem) to resume via `session/load`. When set, the
    /// new session adopts this id as its `acp_session_id`, is forced to the
    /// structured view, and seeds its transcript from the agent's history
    /// replay. `path` must be the session's original cwd. See #2276.
    #[serde(default)]
    pub import_acp_session_id: Option<String>,
    /// Fork an existing session: the source session's captured session id to
    /// resume and diverge from. The new session resumes that conversation as an
    /// independent session (the original is left untouched). The kind of fork
    /// follows `view`/the tool: when `view == Structured` and the tool is
    /// ACP-capable, this drives a structured ACP `session/fork` against the
    /// parent's `acp_session_id`; otherwise it drives a terminal fork that
    /// resumes the parent `agent_session_id` with the agent's fork flag. A
    /// structured fork requested for a non-ACP agent is rejected rather than
    /// silently downgraded.
    #[serde(default)]
    pub fork_from: Option<String>,
    /// External work-queue dispatcher completion callback: an HTTP POST
    /// fires here when the session transitions to Idle, Waiting, or Error.
    /// Must be `http`/`https` and not resolve to a loopback/private/
    /// link-local address; validated at create time, re-validated on every
    /// dispatch. See #3156.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Idempotency key for `POST /api/sessions`: a retry using the same key
    /// (even across a daemon restart, since it's persisted on the created
    /// instance) returns the existing session instead of creating a
    /// duplicate. See #3156.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

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

/// Resolve the one-shot fork seed for a `fork_from` create request. A
/// structured request (`structured == true`) forks through ACP `session/fork`
/// against the parent's `acp_session_id`; a terminal request resumes the
/// parent agent id with the agent's fork flag, generating a fresh child id.
/// `Err` reports an unforkable terminal agent or missing parent id; a
/// structured request is already rejected by the caller's
/// `agent_is_structured_fork_capable` guard before it reaches here.
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

/// True when a create request asks to both import an existing session and fork
/// a parent. The two seed the new session from different sources, so allowing
/// both would produce a contradictory half-imported, half-forked session.
/// Trailing whitespace is treated as unset, matching the per-field guards.
pub(super) fn both_import_and_fork_set(body: &CreateSessionBody) -> bool {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    set(&body.import_acp_session_id) && set(&body.fork_from)
}

/// Thin server-side alias for [`crate::session::fork::structured_fork_capable`],
/// the single source of truth for "can this agent run the ACP `session/fork`
/// handshake?". Shared by the `SessionResponse.acp_can_fork` projection (the web
/// "Fork" affordance) and the create-time guard so they cannot drift.
pub(super) fn agent_is_structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    crate::session::fork::structured_fork_capable(tool, agent_name)
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

/// Insert `instance` into the live registry, replacing any entry that
/// already carries the same id rather than blind-pushing a second copy.
///
/// `create_session` persists the new session to disk (in `persist_and_start`)
/// before it pushes the in-memory copy here. A `status_poll_loop` tick that
/// fires in that window calls `load_all_instances`, reads the just-persisted
/// row, and inserts it first. A blind `push` would then leave two entries
/// with the same id in `state.instances` until the next poll tick collapses
/// them, and `GET /api/sessions` would briefly return the session twice.
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

/// Remove `id` from the live registry, bumping `mutation_epoch` when a row was
/// actually removed.
///
/// The delete path removes a row from `state.instances` in three places: the
/// `AlreadyGone` short-circuit, the structured purge's early mirror removal
/// (which then awaits ACP teardown before the handler finishes), and the final
/// commit block. Every one of them has to bump, and has to bump while the
/// caller still holds the `instances` write lock, because a reloader compares
/// the epoch under that same lock. A removal that skips the bump leaves a
/// window where a disk reload carrying a pre-delete snapshot rebuilds
/// `state.instances` from it and puts the deleted row back, so
/// `GET /api/sessions` lists a session the user just deleted.
///
/// Bumping only on an actual removal keeps the final commit block from
/// spending an epoch when the early removal already took the row; if a stale
/// reload DID restore it in between, the retain here finds it, removes it
/// again, and bumps as it should.
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

/// Carried out of `create_session` to mark a create that was refused because
/// the repo's hooks (or project MCP) need approval and the request did not pass
/// `trust_hooks: true` (#2066). The outer match downcasts this to emit a
/// structured `hooks_need_trust` response instead of the generic
/// `create_failed`, so a caller can show the commands and resubmit.
#[derive(Debug)]
pub(crate) struct HooksNeedTrust {
    /// The `on_create` commands that would run, for display in the prompt.
    pub(crate) on_create: Vec<String>,
    /// The `on_launch` commands the same approval would trust. They don't run
    /// on this create, but the recorded trust covers them for every later
    /// session (TUI/CLI included), so the prompt must show them too.
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

/// Resolved plan for a web-API create's `on_create` lifecycle hooks (#2066).
/// Computed before the worktree is built so an untrusted repo fails fast
/// without leaving an orphan worktree; executed after the build once the
/// session directory exists.
#[derive(Debug)]
pub(crate) struct CreateHookPlan {
    /// Commands to run, already merged (repo overrides global/profile per type).
    pub(crate) on_create: Vec<String>,
    /// `(hooks_hash, mcp_hash)` to persist into `trusted_repos.toml` when the
    /// caller passed `trust_hooks: true` and a surface needed approval. `None`
    /// when nothing new needs recording (already trusted, or no hooks/MCP).
    pub(crate) trust_write: Option<(Option<String>, Option<String>)>,
}

/// Resolve the repo's `on_create` hooks and the trust decision for a web-API
/// create. Returns `Err(HooksNeedTrust)` when a surface needs approval and the
/// caller did not pass `trust_hooks: true`; the surrounding handler maps that to
/// a structured `hooks_need_trust` response. Mirrors the CLI `--trust-hooks`
/// path in `src/cli/add.rs`, adapted for the API's non-interactive context.
pub(crate) fn resolve_create_hook_plan(
    profile: &str,
    project_path: &std::path::Path,
    scratch: bool,
    trust_hooks_requested: bool,
) -> anyhow::Result<CreateHookPlan> {
    use crate::session::config::repo_config::{self, TrustSurface};

    // Scratch sessions have no `.agent-of-empires/config.toml` anchored on a
    // repo path, so skip the repo trust check entirely and fall back to
    // profile-level hooks (matching the CLI scratch branch).
    if scratch {
        let on_create = repo_config::resolve_global_profile_hooks(profile)
            .map(|h| h.on_create)
            .unwrap_or_default();
        return Ok(CreateHookPlan {
            on_create,
            trust_write: None,
        });
    }

    let trust = match repo_config::check_repo_trust(project_path) {
        Ok(t) => t,
        Err(e) => {
            // A failed trust check must not silently drop already-trusted hooks
            // run via global/profile; degrade to profile hooks like the CLI does.
            tracing::warn!(target: "http.api.sessions", "Failed to check repo trust: {e:#}");
            let on_create = repo_config::resolve_global_profile_hooks(profile)
                .map(|h| h.on_create)
                .unwrap_or_default();
            return Ok(CreateHookPlan {
                on_create,
                trust_write: None,
            });
        }
    };

    // Refuse only when HOOKS need approval and the caller did not opt in.
    // Project MCP is deliberately not a gate here: the supervisor skips an
    // untrusted `.mcp.json` at spawn (it's the real MCP gate), so blocking
    // creation on it would be more aggressive than the CLI, which still
    // creates the session when MCP is declined. A passed `trust_hooks` still
    // records MCP trust below, bundling approval the way the CLI does.
    if trust.hooks.needs_trust() && !trust_hooks_requested {
        // Approving trusts the repo's whole hooks hash, so the refusal must
        // carry every hook type the trust would cover (on_launch runs on every
        // later session start, on_destroy on delete), not just on_create;
        // mirrors hook_display_groups in the CLI/TUI prompts.
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
    let on_create = match repo_hooks {
        Some(h) => repo_config::merge_hooks_with_config(profile, h)
            .map(|m| m.on_create)
            .unwrap_or_default(),
        None => repo_config::resolve_global_profile_hooks(profile)
            .map(|h| h.on_create)
            .unwrap_or_default(),
    };
    Ok(CreateHookPlan {
        on_create,
        trust_write,
    })
}

/// Record any pending trust and run the planned `on_create` hooks for a
/// web-API create (#2066). Runs after the worktree exists. Hook output is
/// streamed to a discarded channel so the shared streamed executor's
/// terminal-detach (credential-prompt suppression) applies; failures surface
/// through the returned `Result` with a captured output tail.
pub(crate) fn run_create_hooks(
    instance: &mut Instance,
    plan: &CreateHookPlan,
    project_path: &std::path::Path,
) -> anyhow::Result<()> {
    use crate::session::config::repo_config;

    if let Some((hooks_hash, mcp_hash)) = &plan.trust_write {
        repo_config::trust_repo(project_path, hooks_hash.as_deref(), mcp_hash.as_deref())?;
    }

    if plan.on_create.is_empty() {
        return Ok(());
    }

    let hook_env = repo_config::lifecycle_env_vars(instance);
    // No live consumer: drop the receiver so the executor's sends no-op while
    // its detach-tty behavior and error-tail capture still apply.
    let (progress_tx, progress_rx) = std::sync::mpsc::channel::<repo_config::HookProgress>();
    drop(progress_rx);

    if instance.sandbox_info.is_some() {
        instance.get_container_for_instance()?;
        let workdir = instance.container_workdir();
        if let Some(sandbox) = instance.sandbox_info.as_ref() {
            repo_config::execute_hooks_in_container_streamed(
                &plan.on_create,
                &sandbox.container_name,
                &workdir,
                &progress_tx,
                &hook_env,
            )?;
        }
    } else {
        repo_config::execute_hooks_streamed(
            &plan.on_create,
            std::path::Path::new(&instance.project_path),
            &progress_tx,
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

    if state.cityhall_mode {
        // CityHall sessions are server-derived and locked down: they span every
        // configured project, always render in structured view, and must run an
        // ACP-capable agent. Every client-supplied field that could escape the
        // mode (path/repos/view/scratch plus the spawn/branch fields reset
        // below) is neutralized so a crafted request cannot escape it. See #7.
        let projects = crate::session::projects::load_merged(&state.profile).unwrap_or_default();
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
            .unwrap_or_else(|| state.profile.clone());
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

    let validation_profile = body.profile.as_deref().unwrap_or(&state.profile);
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

    // Import and fork are mutually exclusive: each seeds the new session from a
    // different source (import adopts an on-disk session id; fork resumes a
    // parent's captured id), and honoring both would leave the session in a
    // contradictory half-imported, half-forked state. Reject up front.
    if both_import_and_fork_set(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "message": "Cannot set both import_acp_session_id and fork_from",
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
    let fork_seed = match body
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
                && !agent_is_structured_fork_capable(&body.tool, body.agent_name.as_deref())
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
            let mut resp = SessionResponse::from_instance(
                &instance,
                crate::claude_settings::read_tui_fullscreen(),
            );
            resp.warnings = outcome.warnings;
            // Carry the resolved tie value (#1927); list_sessions' overlay does
            // not run on this create response, so a managed worktree would
            // otherwise report untied until the next list refresh.
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
                    // `wire_str`, not `as_str`: this must match the casing the
                    // same endpoint returns without `?wait=ready`, or a
                    // dispatcher comparing against a `GET /api/sessions` poll
                    // never matches. See #3187.
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
