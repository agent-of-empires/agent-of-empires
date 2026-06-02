//! Session CRUD, ensure-* lifecycle endpoints, and per-file diff handlers.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::git::error::GitError;
use crate::session::{EnsureReadyError, EnsureReadyOutcome, Instance, Status, Storage};

use super::validate_no_shell_injection;
use super::AppState;

#[derive(Serialize)]
pub struct SessionResponse {
    pub id: String,
    pub title: String,
    pub project_path: String,
    pub group_path: String,
    pub tool: String,
    pub status: String,
    pub yolo_mode: bool,
    pub created_at: String,
    pub last_accessed_at: Option<String>,
    /// Wall-clock time of the most recent transition into Idle. Used by the
    /// web dashboard to fade a freshly-stopped session's color toward neutral.
    /// Distinct from `last_accessed_at`: viewing or messaging a session bumps
    /// `last_accessed_at` but leaves `idle_entered_at` alone.
    pub idle_entered_at: Option<String>,
    pub last_error: Option<String>,
    pub branch: Option<String>,
    pub main_repo_path: Option<String>,
    /// Base branch the worktree was created from when AoE managed the
    /// creation. None for sessions attached to a pre-existing branch,
    /// or those that took the repo's default branch. See #948.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Per-session override for the diff base, set via the web "vs &lt;ref&gt;"
    /// picker, the TUI diff view's `b` keybind, or
    /// `aoe session set-base`. Wins over `base_branch`, the profile
    /// default, and auto-detection. See #970.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
    pub is_sandboxed: bool,
    /// True when the session was created with `--scratch`; the
    /// `project_path` points at an auto-provisioned directory under
    /// `<app_dir>/scratch/<id>/` that the deletion path removes. The web
    /// wizard filters these out of the Recent-projects list.
    pub scratch: bool,
    /// True when the session is marked as a user favorite. Mirrors
    /// `Instance::is_favorited()`; surfaced so the web sidebar can pin
    /// favorited rows and render the `*` marker without re-implementing
    /// the predicate. Cross-feature parity with the TUI's `f`/`F` keybind.
    pub favorited: bool,
    /// True when the agent has flagged this session as urgent via the
    /// `attention-urgent` hook (read from `/tmp/aoe-hooks/{id}/attention.json`
    /// by `Instance::is_urgent()`). The web sidebar's Attention sort floats
    /// urgent rows above all non-urgent ones within their triage tier,
    /// matching the TUI's `attention_session_key` urgent-bias. `is_urgent()`
    /// returns false for archived/snoozed sessions, so a sunk row never
    /// claws back to the top. See #1640.
    pub urgent: bool,
    /// RFC3339 timestamp at which the session was web-pinned, or omitted
    /// when not pinned. Distinct from `favorited`: favorite is the TUI
    /// within-tier attention-sort signal, while pin is the hard
    /// top-of-sort surfacing primitive used by the web sidebar. The
    /// client derives a "pinned" boolean as `pinned_at != null`; no
    /// separate boolean field is exposed (the timestamp itself is the
    /// source of truth, matching `archived_at` and `snoozed_until`). See
    /// #1581.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<String>,
    /// RFC3339 timestamp at which the session was archived, or omitted
    /// when not archived. The web sidebar sinks archived workspaces into
    /// the "Snoozed & archived" collapsible section. See #1581.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    /// RFC3339 timestamp at which a snooze expires, or omitted when not
    /// snoozed. The web sidebar treats a non-null future timestamp the
    /// same as archived (sinks the workspace) and renders the remaining
    /// duration. Expired timestamps are stale-but-harmless: the
    /// `Instance::is_snoozed()` predicate returns false past the deadline,
    /// and the response simply omits the field. See #1581.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<String>,
    pub has_managed_worktree: bool,
    pub has_terminal: bool,
    pub profile: String,
    pub cleanup_defaults: CleanupDefaults,
    pub remote_owner: Option<String>,
    /// Per-session push-notification overrides. None means the session
    /// inherits the server-wide default (`web.notify_on_*`) for that
    /// event type; Some(true)/Some(false) is an explicit toggle.
    pub notify_on_waiting: Option<bool>,
    pub notify_on_idle: Option<bool>,
    pub notify_on_error: Option<bool>,
    /// True when this session uses ACP cockpit rendering instead of a
    /// tmux-backed PTY. The web dashboard branches on this to pick
    /// between the cockpit panels and the terminal view.
    #[cfg(feature = "serve")]
    pub cockpit_mode: bool,
    /// Live cockpit worker lifecycle. `absent` for tmux sessions or
    /// cockpit sessions whose worker has not been spawned/attached
    /// yet; `resuming` while the reconciler is mid-spawn or mid-attach;
    /// `running` once the supervisor holds a live worker. Drives the
    /// sidebar `Resuming…` chip and the per-session banner in the
    /// cockpit view. See #1088.
    #[cfg(feature = "serve")]
    pub cockpit_worker_state: crate::cockpit::supervisor::CockpitWorkerState,
    /// True when this session's agent can run in cockpit: a built-in
    /// with an ACP adapter, or a custom agent whose profile config
    /// declares a valid `agent_cockpit_cmd`. The web terminal view reads
    /// this to decide whether the "switch to cockpit" affordance is
    /// available, replacing the hardcoded client-side tool list.
    #[cfg(feature = "serve")]
    pub acp_capable: bool,
    /// True when the session is a Claude Code session AND the user has
    /// enabled Claude's fullscreen renderer (`tui: "fullscreen"` in
    /// `~/.claude/settings.json`). The web client uses this to skip
    /// scrollback-tracking workarounds that target tmux copy-mode.
    pub claude_fullscreen: bool,
    /// Repos in the multi-repo workspace (empty for single-repo sessions).
    /// Each entry mirrors `WorkspaceRepo` minus paths the dashboard does
    /// not need to display.
    pub workspace_repos: Vec<WorkspaceRepoSummary>,
    /// Non-fatal warnings emitted during worktree creation (e.g.
    /// post-checkout hook failures where the worktree was created
    /// successfully). Only populated on the create-session response;
    /// omitted from subsequent fetches because it lives on `BuildResult`
    /// and is not persisted to the instance.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Latest plan snapshot summarised for the sidebar. Present only on
    /// cockpit sessions whose agent has emitted a Plan (directly via
    /// ACP `SessionUpdate::Plan` or indirectly via the ExitPlanMode
    /// bridge in `acp_client::map_update_to_events`). See #1061.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_summary: Option<PlanSummary>,
    /// Absolute RFC3339 timestamp at which the cockpit session's
    /// `ScheduleWakeup` tool will fire (i.e. the next turn is expected
    /// to start). Cleared once a `UserPromptSent` lands after the
    /// scheduling tool call; the /loop skill's self-firing emits that
    /// prompt at wake time, so a wakeup whose seq is ≤ the latest
    /// prompt has already fired. See #1091.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_wakeup_at: Option<String>,
    /// User-facing reason the agent gave when scheduling the wakeup,
    /// shown alongside the countdown chip / banner. Only set when
    /// `next_wakeup_at` is also set. See #1091.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_wakeup_reason: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PlanSummary {
    /// First non-completed step's title, truncated to ~80 chars so the
    /// sidebar row doesn't overflow.
    pub current_step_title: Option<String>,
    /// Count of `PlanEntryStatus::Done` steps.
    pub completed: u32,
    /// Total step count.
    pub total: u32,
}

#[derive(Serialize, Clone)]
pub struct WorkspaceRepoSummary {
    pub name: String,
    pub source_path: String,
    pub branch: String,
}

#[derive(Serialize, Clone)]
pub struct CleanupDefaults {
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
}

impl SessionResponse {
    /// Build a response from a session instance plus the user's current
    /// Claude Code fullscreen-renderer preference.
    ///
    /// `claude_fullscreen` is the *user-level* setting (read once per
    /// request via `crate::claude_settings::read_tui_fullscreen()`); it
    /// surfaces on the response only when the session's agent is Claude.
    pub fn from_instance(inst: &Instance, claude_fullscreen: bool) -> Self {
        Self::from_instance_with_plan(
            inst,
            claude_fullscreen,
            None,
            #[cfg(feature = "serve")]
            crate::cockpit::supervisor::CockpitWorkerState::Absent,
            None,
            None,
        )
    }

    /// Build a response with the per-session plan snapshot. Called from
    /// the REST sessions endpoint after a single bulk read of the
    /// cockpit event store; see #1061.
    pub fn from_instance_with_plan(
        inst: &Instance,
        claude_fullscreen: bool,
        plan_summary: Option<PlanSummary>,
        #[cfg(feature = "serve")]
        cockpit_worker_state: crate::cockpit::supervisor::CockpitWorkerState,
        next_wakeup_at: Option<String>,
        next_wakeup_reason: Option<String>,
    ) -> Self {
        Self {
            id: inst.id.clone(),
            title: inst.title.clone(),
            project_path: inst.project_path.clone(),
            group_path: inst.group_path.clone(),
            tool: inst.tool.clone(),
            status: format!("{:?}", inst.status),
            yolo_mode: inst.yolo_mode,
            created_at: inst.created_at.to_rfc3339(),
            last_accessed_at: inst.last_accessed_at.map(|t| t.to_rfc3339()),
            idle_entered_at: inst.idle_entered_at.map(|t| t.to_rfc3339()),
            last_error: inst.last_error.clone(),
            branch: inst.worktree_info.as_ref().map(|w| w.branch.clone()),
            main_repo_path: inst
                .worktree_info
                .as_ref()
                .map(|w| w.main_repo_path.clone()),
            base_branch: inst
                .worktree_info
                .as_ref()
                .and_then(|w| w.base_branch.clone()),
            base_branch_override: inst.base_branch_override.clone(),
            is_sandboxed: inst.is_sandboxed(),
            scratch: inst.scratch,
            favorited: inst.is_favorited(),
            urgent: inst.is_urgent(),
            pinned_at: inst.pinned_at.map(|t| t.to_rfc3339()),
            archived_at: inst.archived_at.map(|t| t.to_rfc3339()),
            // Surface `snoozed_until` only when the snooze is still
            // active. `is_snoozed()` returns false once the timestamp
            // has expired, even though the persisted field stays set
            // until the next mutation rewrites it. Mirroring that
            // semantics on the wire prevents the web sidebar from
            // showing a "snoozed 0m" chip on rows that have already
            // woken on disk.
            snoozed_until: if inst.is_snoozed() {
                inst.snoozed_until.map(|t| t.to_rfc3339())
            } else {
                None
            },
            has_managed_worktree: inst
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe),
            has_terminal: inst.terminal_info.is_some(),
            profile: inst.source_profile.clone(),
            cleanup_defaults: CleanupDefaults {
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
            },
            remote_owner: None,
            notify_on_waiting: inst.notify_on_waiting,
            notify_on_idle: inst.notify_on_idle,
            notify_on_error: inst.notify_on_error,
            #[cfg(feature = "serve")]
            cockpit_mode: inst.cockpit_mode,
            #[cfg(feature = "serve")]
            cockpit_worker_state,
            // Built-in ACP capability is resolved here from a process-wide
            // registry (cheap, no IO). Custom agents depend on profile
            // config; the list and create handlers overlay that without a
            // per-row config read.
            #[cfg(feature = "serve")]
            acp_capable: {
                let resolved = inst
                    .cockpit_agent
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                builtin_acp_registry().get(resolved).is_some()
            },
            claude_fullscreen: claude_fullscreen && inst.tool == "claude",
            workspace_repos: inst
                .workspace_info
                .as_ref()
                .map(|w| {
                    w.repos
                        .iter()
                        .map(|r| WorkspaceRepoSummary {
                            name: r.name.clone(),
                            source_path: r.source_path.clone(),
                            branch: r.branch.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            warnings: Vec::new(),
            plan_summary,
            next_wakeup_at,
            next_wakeup_reason,
        }
    }
}

/// Project a stored `Plan` into the lightweight `PlanSummary` shape the
/// sidebar consumes. Current step is the first non-Done entry; counts
/// reflect the persisted step state from the agent's last PlanUpdated.
fn plan_summary_from_plan(plan: crate::cockpit::state::Plan) -> PlanSummary {
    use crate::cockpit::state::PlanStepStatus;
    let total = plan.steps.len() as u32;
    let completed = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStepStatus::Done))
        .count() as u32;
    let current_step_title = plan
        .steps
        .iter()
        .find(|s| !matches!(s.status, PlanStepStatus::Done))
        .map(|s| truncate_title(&s.title, 80));
    PlanSummary {
        current_step_title,
        completed,
        total,
    }
}

fn truncate_title(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// Envelope for `GET /api/sessions`. Wraps the sessions list with the
// user's persisted workspace ordering so the client can render the
// sidebar in the requested order on the first paint, with no extra
// round-trip. The order is a list of workspace ids; ids not present
// fall back to the client's default newest-first ordering. See #1169.
#[derive(serde::Serialize)]
pub struct SessionsEnvelope {
    pub sessions: Vec<SessionResponse>,
    pub workspace_ordering: Vec<String>,
}

/// Process-wide built-in ACP registry, built once. Used to compute
/// `SessionResponse.acp_capable` for built-in agents without allocating
/// a registry per response row.
#[cfg(feature = "serve")]
fn builtin_acp_registry() -> &'static crate::cockpit::AgentRegistry {
    static REG: std::sync::OnceLock<crate::cockpit::AgentRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(crate::cockpit::AgentRegistry::with_defaults)
}

/// True iff this custom agent declares a valid `agent_cockpit_cmd` in the
/// given profile-resolved map. Built-in capability is handled separately
/// in the constructor, so this only covers the custom case.
#[cfg(feature = "serve")]
fn custom_agent_acp_capable(
    agent_cockpit_cmd: &std::collections::HashMap<String, String>,
    tool: &str,
) -> bool {
    agent_cockpit_cmd
        .get(tool)
        .is_some_and(|cmd| crate::cockpit::AgentSpec::from_cockpit_cmd(tool, cmd).is_ok())
}

pub async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<SessionsEnvelope> {
    let instances = state.instances.read().await;
    let claude_fullscreen = crate::claude_settings::read_tui_fullscreen();
    // Snapshot the supervisor's worker lifecycle map once per request
    // rather than locking it per row. See #1088.
    #[cfg(feature = "serve")]
    let worker_states = state.cockpit_supervisor.worker_states_snapshot().await;
    let mut sessions: Vec<SessionResponse> = instances
        .iter()
        .map(|inst| {
            let plan_summary = if inst.cockpit_mode {
                state
                    .cockpit_event_store
                    .latest_plan(&inst.id)
                    .map(plan_summary_from_plan)
            } else {
                None
            };
            let (next_wakeup_at, next_wakeup_reason) = if inst.cockpit_mode {
                match state.cockpit_event_store.latest_pending_wakeup(&inst.id) {
                    Some((at, reason)) => (Some(at.to_rfc3339()), reason),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            #[cfg(feature = "serve")]
            let cockpit_worker_state = worker_states
                .get(&inst.id)
                .copied()
                .unwrap_or(crate::cockpit::supervisor::CockpitWorkerState::Absent);
            SessionResponse::from_instance_with_plan(
                inst,
                claude_fullscreen,
                plan_summary,
                #[cfg(feature = "serve")]
                cockpit_worker_state,
                next_wakeup_at,
                next_wakeup_reason,
            )
        })
        .collect();

    // Overlay custom-agent ACP capability (built-ins were resolved in the
    // constructor). Cache by (profile, project_path) since repo-local
    // config can override agent_cockpit_cmd, so each distinct pair is
    // resolved at most once.
    #[cfg(feature = "serve")]
    {
        use std::collections::HashMap;
        let mut cockpit_cmd_cache: HashMap<(String, String), HashMap<String, String>> =
            HashMap::new();
        for (resp, inst) in sessions.iter_mut().zip(instances.iter()) {
            if resp.acp_capable {
                continue;
            }
            let key = (inst.source_profile.clone(), inst.project_path.clone());
            let map = cockpit_cmd_cache.entry(key).or_insert_with(|| {
                crate::session::repo_config::resolve_config_with_repo_or_warn(
                    &inst.source_profile,
                    std::path::Path::new(&inst.project_path),
                )
                .session
                .agent_cockpit_cmd
            });
            resp.acp_capable = custom_agent_acp_capable(map, &inst.tool);
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
                let cfg = crate::session::profile_config::resolve_config_or_warn(&session.profile);
                CleanupDefaults {
                    delete_worktree: cfg.worktree.auto_cleanup,
                    delete_branch: cfg.worktree.delete_branch_on_cleanup,
                    delete_sandbox: cfg.sandbox.auto_cleanup,
                }
            });
        }
        *state.cleanup_defaults_cache.write().await = crate::server::CleanupDefaultsCache {
            refreshed_at: std::time::Instant::now(),
            entries: fresh.clone(),
        };
        fresh
    };

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
            if let Some(owner) = cache.get(repo_path) {
                session.remote_owner = owner.clone();
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
                let owner = crate::git::get_remote_owner(std::path::Path::new(path));
                cache.insert(path.clone(), owner);
            }
        }
        for session in &mut sessions {
            let repo_path = session
                .main_repo_path
                .as_deref()
                .unwrap_or(&session.project_path);
            if session.remote_owner.is_none() {
                if let Some(owner) = cache.get(repo_path) {
                    session.remote_owner = owner.clone();
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
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
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

// --- Rename session ---

#[derive(Deserialize)]
pub struct RenameSessionBody {
    pub title: String,
}

fn apply_session_title_rename(inst: &mut Instance, title: String) {
    inst.title = title;
}

pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Title cannot be empty" })),
        )
            .into_response();
    }
    if let Err(msg) = validate_no_shell_injection(&title, "title") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": msg })),
        )
            .into_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "message": "Session not found" })),
        )
            .into_response();
    };

    apply_session_title_rename(inst, title.clone());

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    let profile = inst.source_profile.clone();
    drop(instances);

    if let Ok(storage) = Storage::new(&profile) {
        let title_clone = title.clone();
        let id_clone = id.clone();
        match tokio::task::spawn_blocking(move || {
            storage.update(|instances, _groups| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                    apply_session_title_rename(inst, title_clone);
                }
                Ok(())
            })
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(target: "http.api.sessions", "Failed to save after rename: {e}")
            }
            Err(e) => {
                tracing::error!(target: "http.api.sessions", "Rename persist join failed: {e}")
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Edit worktree workdir name ---

#[derive(Deserialize)]
pub struct SetWorktreeNameBody {
    pub name: String,
    /// Also rename the underlying git branch to match. Off by default: the
    /// session may have done meaningful work on its branch already.
    #[serde(default)]
    pub rename_branch: bool,
}

/// Map a worktree-edit failure to an HTTP status + client-safe message.
/// Validation failures are 400/409; git/IO failures stay generic (raw git
/// stderr and IO paths must not reach the wire).
fn worktree_edit_error_response(
    e: &crate::session::worktree_edit::WorktreeEditError,
) -> (StatusCode, String) {
    use crate::session::worktree_edit::WorktreeEditError as E;
    match e {
        E::NotManaged => (
            StatusCode::BAD_REQUEST,
            "This worktree is not managed by aoe; its workdir name cannot be edited".to_string(),
        ),
        E::EmptyName => (
            StatusCode::BAD_REQUEST,
            "Workdir name cannot be empty".to_string(),
        ),
        E::Unchanged => (
            StatusCode::BAD_REQUEST,
            "The workdir name is unchanged".to_string(),
        ),
        E::NoParent(_) => (
            StatusCode::BAD_REQUEST,
            "Cannot determine the worktree's parent directory".to_string(),
        ),
        E::SourceMissing(_) => (
            StatusCode::CONFLICT,
            "The worktree directory no longer exists on disk".to_string(),
        ),
        E::TargetExists(_) => (
            StatusCode::CONFLICT,
            "A directory with that name already exists".to_string(),
        ),
        E::BranchExists(name) => (
            StatusCode::CONFLICT,
            format!("Branch '{name}' already exists"),
        ),
        E::RollbackFailed { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to move the worktree, and rolling back the branch rename also failed; the repository may be left on the new branch".to_string(),
        ),
        E::Git(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to move the worktree".to_string(),
        ),
    }
}

pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Workdir name cannot be empty" })),
        )
            .into_response();
    }
    if let Err(msg) = validate_no_shell_injection(&name, "name") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": msg })),
        )
            .into_response();
    }

    // Serialize against other mutations on this session (start, delete,
    // another rename) so the git ops and the metadata write don't race.
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (worktree_info, current_path, status, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        (
            inst.worktree_info.clone(),
            inst.project_path.clone(),
            inst.status,
            inst.source_profile.clone(),
        )
    };

    let Some(worktree_info) = worktree_info else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Session does not use a worktree" })),
        )
            .into_response();
    };
    if status.blocks_worktree_edit() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "message": "Cannot edit the workdir name while the session is active; stop it first"
            })),
        )
            .into_response();
    }

    let wt = worktree_info.clone();
    let cur = current_path.clone();
    let new_name = name.clone();
    let rename_branch = body.rename_branch;
    let edit = tokio::task::spawn_blocking(move || {
        crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info: &wt,
                current_path: std::path::Path::new(&cur),
                new_name: &new_name,
                rename_branch,
            },
        )
        .map(|o| (o.new_path.to_string_lossy().to_string(), o.new_branch))
    })
    .await;

    let (new_path, new_branch) = match edit {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "worktree edit failed: {e}");
            let (code, msg) = worktree_edit_error_response(&e);
            return (code, Json(serde_json::json!({ "message": msg }))).into_response();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "worktree edit join failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "message": "Worktree edit task failed" })),
            )
                .into_response();
        }
    };

    // The git move has already landed, so persist to disk BEFORE mutating
    // in-memory state. A silent persist failure here would leave stale
    // metadata that points at the old (now-moved) path after a daemon
    // restart, so any failure returns 500 instead of a misleading 200.
    let persist_failed = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "persist_failed",
                "message": "Worktree was moved on disk, but persisting the new session metadata failed"
            })),
        )
            .into_response()
    };

    let storage = match Storage::new(&profile) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "http.api.sessions", session = %id, "Storage::new failed after worktree edit: {e}");
            return persist_failed();
        }
    };
    let id_clone = id.clone();
    let new_path_clone = new_path.clone();
    let new_branch_clone = new_branch.clone();
    match tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                apply_worktree_name_edit(inst, &new_path_clone, new_branch_clone.as_deref());
            }
            Ok(())
        })
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Failed to save after worktree edit: {e}");
            return persist_failed();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Worktree edit persist join failed: {e}");
            return persist_failed();
        }
    }

    let response = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        apply_worktree_name_edit(inst, &new_path, new_branch.as_deref());
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen())
    };

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

fn apply_worktree_name_edit(inst: &mut Instance, new_path: &str, new_branch: Option<&str>) {
    inst.project_path = new_path.to_string();
    if let Some(branch) = new_branch {
        if let Some(wt) = inst.worktree_info.as_mut() {
            wt.branch = branch.to_string();
        }
    }
}

// --- Update session group ---

#[derive(Deserialize)]
pub struct UpdateGroupBody {
    /// Destination group path. Empty string means "ungrouped". A
    /// non-empty path auto-creates the group: `/api/groups` and the
    /// `GroupTree` render model both derive groups from instance
    /// `group_path` values, so no separate groups.json write is needed
    /// (this mirrors `create_session`, which never touches the groups
    /// Vec either).
    pub group: String,
}

fn apply_session_group(inst: &mut Instance, group: String) {
    inst.group_path = group;
}

/// `PATCH /api/sessions/:id/group`. Moves an existing session to another
/// group, creates a new group by assigning its path, or clears the group
/// (empty string). Web parity with the TUI rename dialog and `aoe session
/// rename --group`, which already support post-create group edits.
///
/// Persist-first like the other per-field PATCH sub-routes (`/pin`,
/// `/archive`, `/snooze`): disk is made durable before memory is touched,
/// so a failed write returns 500 without leaving memory and disk diverged.
/// See #1589.
pub async fn update_session_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateGroupBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let group = body.group;
    // Match `create_session`'s group handling exactly: shell-injection
    // check on a non-empty path, no trimming or slash normalization. The
    // empty string is the ungroup sentinel and skips validation.
    if !group.is_empty() {
        if let Err(msg) = validate_no_shell_injection(&group, "group") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": msg })),
            )
                .into_response();
        }
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        inst.source_profile.clone()
    };

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_group = group.clone();
    if persist_session_update(profile, "group update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            apply_session_group(inst, persist_group);
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::error!(
            target: "http.api.sessions",
            session = %id,
            "group update: instance vanished after persist"
        );
        return persist_failed_response();
    };
    apply_session_group(inst, group);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Update session notification preferences ---

/// Body for `PATCH /api/sessions/:id/notifications`. Each field is an
/// outer Option so absence means "leave this value alone"; an inner
/// Option where `Some(null)` is a valid JSON value means "clear this
/// override." We represent that as an untagged enum below so the
/// caller can send `{"notify_on_idle": true}`, `{"notify_on_idle": false}`,
/// or `{"notify_on_idle": null}` and each means what you'd expect.
#[derive(Deserialize, Default)]
pub struct UpdateNotificationsBody {
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_waiting: Tristate,
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_idle: Tristate,
    #[serde(default, deserialize_with = "deserialize_tristate")]
    pub notify_on_error: Tristate,
}

/// Three-state field representing JSON `undefined | null | true | false`:
/// - Unset: leave the current session value untouched.
/// - Clear: set to None (inherit the server default).
/// - Set(v): explicit user override.
#[derive(Default, Copy, Clone)]
pub enum Tristate {
    #[default]
    Unset,
    Clear,
    Set(bool),
}

fn deserialize_tristate<'de, D>(d: D) -> Result<Tristate, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Option<Option<bool>>: absent -> None, null -> Some(None), bool -> Some(Some(bool))
    let v: Option<Option<bool>> = Option::deserialize(d)?;
    Ok(match v {
        None => Tristate::Unset,
        Some(None) => Tristate::Clear,
        Some(Some(b)) => Tristate::Set(b),
    })
}

/// Persist a session mutation to its profile store before touching memory.
///
/// Opens `Storage` for `profile` and runs `mutate` inside the storage
/// `update` transaction on a blocking thread, collapsing all three failure
/// modes (store open, write, join) into `Err(())` after logging with
/// `label`. Callers MUST treat `Err` as HTTP 500 and leave the in-memory
/// instance untouched: persisting first is what keeps disk and memory from
/// diverging when a write fails, and stops the archive/snooze side effects
/// from firing on a write that never landed. See #1589.
async fn persist_session_update<F>(
    profile: String,
    label: &'static str,
    mutate: F,
) -> Result<(), ()>
where
    F: FnOnce(&mut Vec<Instance>) + Send + 'static,
{
    let storage = match Storage::new(&profile) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "http.api.sessions",
                "Failed to open storage for {label}: {e}"
            );
            return Err(());
        }
    };
    match tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            mutate(instances);
            Ok(())
        })
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::error!(
                target: "http.api.sessions",
                "Failed to persist {label}: {e}"
            );
            Err(())
        }
        Err(e) => {
            tracing::error!(
                target: "http.api.sessions",
                "Persist join failed for {label}: {e}"
            );
            Err(())
        }
    }
}

/// 500 response returned whenever `persist_session_update` reports failure.
/// The body shape (`error` + `message`) matches the other JSON error
/// responses in this module so the dashboard's `!res.ok` handling reads the
/// same keys it already does elsewhere.
fn persist_failed_response() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "persist_failed",
            "message": "Failed to persist session update"
        })),
    )
        .into_response()
}

pub async fn update_session_notifications(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateNotificationsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Apply each field independently. `Unset` leaves the stored value
    // alone; `Clear` sets it to None (inherit default); `Set(v)` writes
    // an explicit override.
    fn apply(target: &mut Option<bool>, tri: Tristate) {
        match tri {
            Tristate::Unset => {}
            Tristate::Clear => *target = None,
            Tristate::Set(v) => *target = Some(v),
        }
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        inst.source_profile.clone()
    };

    let waiting = body.notify_on_waiting;
    let idle = body.notify_on_idle;
    let error = body.notify_on_error;

    // Persist first; only mutate memory once disk is durable so a write
    // failure leaves the two in agreement. See #1589.
    let persist_id = id.clone();
    if persist_session_update(profile, "notification update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            apply(&mut inst.notify_on_waiting, waiting);
            apply(&mut inst.notify_on_idle, idle);
            apply(&mut inst.notify_on_error, error);
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::error!(
            target: "http.api.sessions",
            session = %id,
            "notification update: instance vanished after persist"
        );
        return persist_failed_response();
    };
    apply(&mut inst.notify_on_waiting, waiting);
    apply(&mut inst.notify_on_idle, idle);
    apply(&mut inst.notify_on_error, error);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Diff base override ---
//
// `PATCH /api/sessions/{id}/diff-base` sets / clears the per-session
// override for the diff base ref. The web `vs <ref>` chip popover, the
// TUI diff view's `b` keybind, and `aoe session set-base` all funnel
// through this endpoint so the override is persisted alongside the
// session record and survives restart. See #970.

#[derive(Deserialize)]
pub struct UpdateDiffBaseBody {
    /// New override. `Some(non-empty)` sets the override; `Some("")` or
    /// `None` clears it (the diff then falls back to the profile default
    /// and then auto-detection).
    #[serde(default)]
    pub base_branch: Option<String>,
}

pub async fn update_session_diff_base(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateDiffBaseBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        inst.source_profile.clone()
    };

    let new_override = body
        .base_branch
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_override = new_override.clone();
    if persist_session_update(profile, "diff-base update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            inst.base_branch_override = persist_override;
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::error!(
            target: "http.api.sessions",
            session = %id,
            "diff-base update: instance vanished after persist"
        );
        return persist_failed_response();
    };
    inst.base_branch_override = new_override;

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Triage: pin / archive / snooze ---
//
// Three sibling endpoints surface the existing `Instance::pin`, `archive`,
// and `snooze` mutators to the web dashboard. They all follow the same
// shape: read-only 403, in-memory write under `state.instance_lock`,
// persist via `Storage::update` matching the notifications and diff-base
// precedent above. Archive additionally tears down the tmux pane and (for
// cockpit sessions) the supervisor's worker so the row is genuinely
// parked. Mutual-exclusion invariants (e.g. archive clears pin/favorite,
// pin clears archive+snooze) live in the `Instance` methods, so the
// handlers never set fields directly. See #1581.

#[derive(Deserialize)]
pub struct UpdatePinBody {
    pub pinned: bool,
}

#[derive(Deserialize)]
pub struct UpdateArchiveBody {
    pub archived: bool,
    /// When `archived = true`, kill the tmux pane (parity with the TUI's
    /// `z` keybind and the CLI's `aoe session archive` default). Omitted
    /// or `true` means kill; `false` keeps the pane alive while still
    /// marking the session archived. Ignored when `archived = false`.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

fn default_kill_pane() -> bool {
    true
}

#[derive(Deserialize)]
pub struct UpdateSnoozeBody {
    /// `Some(positive minutes)` snoozes for that duration. `None` (or a
    /// missing field) unsnoozes. Validated against
    /// `crate::session::validate_snooze_duration` so the same bounds the
    /// TUI dialog and CLI use also apply here.
    #[serde(default)]
    pub minutes: Option<u32>,
}

pub async fn update_session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePinBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        inst.source_profile.clone()
    };

    let pinned = body.pinned;

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    if persist_session_update(profile, "pin update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            if pinned {
                inst.pin();
            } else {
                inst.unpin();
            }
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::error!(
            target: "http.api.sessions",
            session = %id,
            "pin update: instance vanished after persist"
        );
        return persist_failed_response();
    };
    if pinned {
        inst.pin();
    } else {
        inst.unpin();
    }

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

pub async fn update_session_archive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateArchiveBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Read the profile without mutating memory yet. Persisting first means
    // a storage failure returns 500 with disk and memory still in
    // agreement, and the tmux/cockpit teardown below never fires on a write
    // that did not land. See #1589.
    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        inst.source_profile.clone()
    };

    let archived = body.archived;
    let persist_id = id.clone();
    if persist_session_update(profile, "archive update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            if archived {
                inst.archive();
            } else {
                inst.unarchive();
            }
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    // Disk is durable; apply to memory and snapshot what the side effects
    // need. Clone the instance once so we can call its `kill()` method
    // outside the lock without re-borrowing.
    let (was_cockpit_mode, inst_clone, kill_pane) = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::error!(
                target: "http.api.sessions",
                session = %id,
                "archive update: instance vanished after persist"
            );
            return persist_failed_response();
        };
        if archived {
            inst.archive();
        } else {
            inst.unarchive();
        }
        let response =
            SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
        let cockpit;
        #[cfg(feature = "serve")]
        {
            cockpit = inst.cockpit_mode;
        }
        #[cfg(not(feature = "serve"))]
        {
            cockpit = false;
        }
        let inst_snap = inst.clone();
        drop(instances);

        // Stash the cockpit flag + clone + response and break out to do
        // the side effects below. Return early on the non-archive path
        // because we have no work left to do; the kill_pane=false case
        // is NOT a short-circuit because cockpit shutdown still has to
        // run for cockpit-mode sessions (kill_pane is a tmux-only
        // switch, per the request-body documentation).
        if !archived {
            return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
        }
        (cockpit, inst_snap, body.kill_pane)
    };

    // Best-effort tmux pane teardown for tmux-backed sessions. Mirrors
    // `toggle_archive_at_cursor` in src/tui/home/operations.rs: if the
    // kill fails (pane already dead, tmux gone), log and continue
    // because the on-disk archived flag is the source of truth. The
    // kill_pane=false body opt-out applies only here, so a caller can
    // archive a tmux session without killing its pane while still
    // unconditionally stopping a cockpit worker on the other branch.
    if !was_cockpit_mode {
        if kill_pane {
            let inst_for_kill = inst_clone.clone();
            match tokio::task::spawn_blocking(move || inst_for_kill.kill()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    target: "http.api.sessions",
                    "Archive: tmux kill failed: {e}"
                ),
                Err(e) => tracing::warn!(
                    target: "http.api.sessions",
                    "Archive: tmux kill join failed: {e}"
                ),
            }
        }
    } else {
        // Cockpit sessions: shut down the worker so the supervisor's
        // reconciler does not race to respawn it. The reconciler also
        // skips archived sessions (see cockpit_reconciler.rs), but
        // shutting down here gives an immediate teardown rather than
        // waiting for the next poll tick. `shutdown` preserves the
        // agent transcript (no session/delete), so unarchiving resumes
        // the conversation instead of resetting it (#1710).
        #[cfg(feature = "serve")]
        match state.cockpit_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::cockpit::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "cockpit.supervisor",
                session = %id,
                "shutdown during archive failed: {e}"
            ),
        }
    }

    // Re-read the in-memory instance so the response reflects the
    // archived flag (the side effects above did not mutate it, but
    // re-reading also picks up any peer write that landed during the
    // unlock window).
    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

pub async fn update_session_snooze(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateSnoozeBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Server is in read-only mode"
            })),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Validate the duration up front. The TUI dialog presets, CLI, and
    // this endpoint all share the same bounds (1..=43200 minutes); see
    // `crate::session::config::validate_snooze_duration`.
    if let Some(minutes) = body.minutes {
        if let Err(msg) = crate::session::validate_snooze_duration(minutes as u64) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "validation_failed",
                    "message": msg,
                })),
            )
                .into_response();
        }
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (was_cockpit_mode, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        };
        let cockpit;
        #[cfg(feature = "serve")]
        {
            cockpit = inst.cockpit_mode;
        }
        #[cfg(not(feature = "serve"))]
        {
            cockpit = false;
        }
        (cockpit, inst.source_profile.clone())
    };

    let minutes = body.minutes;

    // Persist first; only mutate memory once disk is durable, and only fire
    // the cockpit teardown below on a write that landed. See #1589.
    let persist_id = id.clone();
    if persist_session_update(profile, "snooze update", move |instances| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
            match minutes {
                Some(m) => inst.snooze(m),
                None => inst.unsnooze(),
            }
        }
    })
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::error!(
                target: "http.api.sessions",
                session = %id,
                "snooze update: instance vanished after persist"
            );
            return persist_failed_response();
        };
        match minutes {
            Some(m) => inst.snooze(m),
            None => inst.unsnooze(),
        }
    }

    // For cockpit-mode sessions, snoozing tears down the worker the
    // same way archive does. Snooze is a "temporary archive" in the
    // data model and the cockpit worker (claude-agent-acp subprocess)
    // is heavy enough that keeping it idle while the row is sunk is a
    // resource hog. The reconciler skips snoozed sessions, so the
    // worker stays down until the snooze expires; the next reconciler
    // tick after expiry brings it back. Unsnooze just lets the
    // reconciler re-pick the session naturally, no explicit respawn.
    // `shutdown` preserves the agent transcript (no session/delete), so
    // that respawn resumes the conversation instead of resetting it
    // (#1710).
    #[cfg(feature = "serve")]
    if was_cockpit_mode && minutes.is_some() {
        match state.cockpit_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::cockpit::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "cockpit.supervisor",
                session = %id,
                "shutdown during snooze failed: {e}"
            ),
        }
    }
    #[cfg(not(feature = "serve"))]
    let _ = was_cockpit_mode;

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Delete session ---

#[derive(Default, Deserialize)]
pub struct DeleteSessionBody {
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub delete_sandbox: bool,
    #[serde(default)]
    pub force_delete: bool,
    /// For scratch sessions, keep the scratch directory on disk instead of
    /// removing it. The session record is still deleted. No effect on
    /// non-scratch sessions.
    #[serde(default)]
    pub keep_scratch: bool,
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        );
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Acquire per-instance lock to serialize concurrent mutations
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Find and clone the instance (need the full Instance for deletion)
    let instance = {
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).cloned()
    };

    let Some(instance) = instance else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "message": "Session not found" })),
        );
    };

    let profile = instance.source_profile.clone();

    // Mark as Deleting so polling clients see the status change
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.status = Status::Deleting;
        }
    }

    // Tear down the cockpit worker FIRST so the ACP subprocess + its
    // claude-agent-acp child don't leak past the session delete. The
    // supervisor's shutdown is best-effort: sessions without a worker
    // (tmux-mode, or cockpit sessions whose worker never spawned)
    // return UnknownSession, which we ignore.
    #[cfg(feature = "serve")]
    if instance.cockpit_mode {
        // Permanent removal: release the agent's persisted transcript
        // too, since the session is going away for good. See #1710.
        match state.cockpit_supervisor.shutdown_and_delete(&id).await {
            Ok(()) | Err(crate::cockpit::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => {
                tracing::warn!(
                    target: "cockpit.supervisor",
                    session = %id,
                    "shutdown during delete failed: {e}"
                );
            }
        }
        // Drop the per-session seq counter so a recreated session
        // with the same id (rare, but possible) starts cleanly from
        // seq=1.
        state.cockpit_supervisor.forget_session(&id);
        // On-disk history is the durable mirror; without this purge a
        // recreated session with the same id would inherit the deleted
        // session's transcript and the seq=1 first publish would
        // collide with a row already in the store.
        state.cockpit_event_store.delete_session(&id);
    }

    // Run deletion on a blocking thread (may do git/docker/tmux operations)
    let deletion_id = id.clone();
    let deletion_result = tokio::task::spawn_blocking(move || {
        crate::session::deletion::perform_deletion(&crate::session::deletion::DeletionRequest {
            session_id: deletion_id,
            instance,
            delete_worktree: body.delete_worktree,
            delete_branch: body.delete_branch,
            delete_sandbox: body.delete_sandbox,
            force_delete: body.force_delete,
            detach_hooks: true,
            keep_scratch: body.keep_scratch,
        })
    })
    .await;

    match deletion_result {
        Ok(result) if result.success => {
            // `perform_deletion` may have produced user-facing messages
            // (e.g. "Scratch directory kept at: <path>" when
            // `--keep-scratch` is set). Capture them now so the
            // success branch can echo them back; the result moves into
            // the spawn_blocking below.
            let messages = result.messages.clone();
            // Disk first: if persistence fails, the in-memory state is left
            // intact and we return 500. Otherwise the status poll loop
            // would silently re-add the entry from disk on the next tick
            // and the user would see "deleted" then the session
            // reappearing seconds later.
            let storage = match Storage::new(&profile) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(target: "http.api.sessions",
                        "Storage::new failed after deletion: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "persist_failed",
                            "message": format!(
                                "Session was torn down but storage init failed: {e}"
                            ),
                        })),
                    );
                }
            };
            let id_for_save = id.clone();
            let persist_result = tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    instances.retain(|i| i.id != id_for_save);
                    Ok(())
                })
            })
            .await;
            match persist_result {
                Ok(Ok(())) => {
                    {
                        let mut instances = state.instances.write().await;
                        instances.retain(|i| i.id != id);
                    }
                    state.instance_locks.write().await.remove(&id);
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "status": "deleted",
                            "messages": messages,
                        })),
                    )
                }
                Ok(Err(e)) => {
                    tracing::error!(target: "http.api.sessions",
                        "Failed to save after deletion: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "persist_failed",
                            "message": format!(
                                "Session deletion completed on disk, but \
                                 sessions.json could not be updated: {e}"
                            ),
                        })),
                    )
                }
                Err(join_err) => {
                    tracing::error!(target: "http.api.sessions",
                        "Persist task panicked: {join_err}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "persist_failed",
                            "message": "Persist task panicked",
                        })),
                    )
                }
            }
        }
        Ok(result) => {
            // Deletion had errors; set status to Error
            let error_msg = if result.errors.is_empty() {
                "Unknown error".to_string()
            } else {
                result.errors.join("; ")
            };
            {
                let mut instances = state.instances.write().await;
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    inst.status = Status::Error;
                    inst.last_error = Some(error_msg.clone());
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "deletion_failed",
                    "message": error_msg,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "internal",
                "message": format!("Deletion task failed: {e}"),
            })),
        ),
    }
}

// --- Create session ---

#[derive(Deserialize)]
pub struct CreateSessionBody {
    pub title: Option<String>,
    pub path: String,
    pub tool: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub yolo_mode: bool,
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
    #[serde(default)]
    pub command_override: String,
    #[serde(default)]
    pub custom_instruction: Option<String>,
    pub profile: Option<String>,
    /// Whether the new session should run in cockpit mode. The
    /// bundled wizard always sends an explicit value (true for ACP-
    /// capable tools when the master switch is on, false otherwise);
    /// other API callers may omit the field, in which case it
    /// defaults to false. Either way the value is re-gated by the
    /// master switch below before being persisted, so a tampered
    /// request can't escalate cockpit on past the master switch.
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub cockpit_mode: bool,
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub cockpit_agent: Option<String>,
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub cockpit_model: Option<String>,
    /// Scratch session: server provisions a fresh directory under
    /// `<app_dir>/scratch/<id>/` and ignores `path`. Mutually exclusive with
    /// `worktree_branch` and `extra_repo_paths`; the handler returns 400
    /// on either combination.
    #[serde(default)]
    pub scratch: bool,
}

fn validate_session_tool_identity(
    tool: &str,
    profile: &str,
    project_path: &std::path::Path,
) -> bool {
    if crate::agents::get_agent(tool).is_some() {
        return true;
    }

    match crate::session::repo_config::resolve_config_with_repo(profile, project_path) {
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

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CreateSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Scratch sessions are server-provisioned; the worktree path is the
    // wrong model for them. Reject the combination before reaching the
    // builder so misbehaving clients get a clear 400 instead of a
    // less-specific builder bail surfaced as 500.
    if body.scratch && body.worktree_branch.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "validation_failed",
                "message": "Cannot combine scratch with worktree_branch"
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
    let mut shell_checks: Vec<(&str, &str)> = vec![
        (body.extra_args.as_str(), "extra_args"),
        (body.tool.as_str(), "tool"),
        (body.group.as_str(), "group"),
    ];
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
    if let Some(ref title) = body.title {
        if let Err(msg) = validate_no_shell_injection(title, "title") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
    }
    if let Some(ref branch) = body.worktree_branch {
        if let Err(msg) = validate_no_shell_injection(branch, "worktree_branch") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
    }
    if let Some(ref profile_name) = body.profile {
        if let Err(msg) = validate_no_shell_injection(profile_name, "profile") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": msg})),
            )
                .into_response();
        }
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

    let profile = body.profile.unwrap_or_else(|| state.profile.clone());
    let instances = state.instances.read().await;
    let existing_titles: Vec<String> = instances.iter().map(|i| i.title.clone()).collect();
    let existing_branches: Vec<String> = instances
        .iter()
        .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.clone()))
        .collect();
    drop(instances);

    // Snapshot the master-kill flag before moving into spawn_blocking
    // so the post-spawn write path (which still needs `state`) keeps
    // its handle.
    #[cfg(feature = "serve")]
    let cockpit_master_enabled = state
        .cockpit_master_enabled
        .load(std::sync::atomic::Ordering::Relaxed);

    let result = tokio::task::spawn_blocking(move || {
        use crate::session::builder::{self, InstanceParams};
        use crate::session::Config;

        let config = Config::load_or_warn();
        let sandbox_image = body.sandbox_image.unwrap_or_else(|| {
            if config.sandbox.default_image.is_empty() {
                "ubuntu:latest".to_string()
            } else {
                config.sandbox.default_image.clone()
            }
        });

        let title_refs: Vec<&str> = existing_titles.iter().map(|s| s.as_str()).collect();
        let branch_refs: Vec<&str> = existing_branches.iter().map(|s| s.as_str()).collect();
        let extra_repo_paths: Vec<String> = body
            .extra_repo_paths
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();

        // When worktree_branch is empty string, generate a name from civilizations.
        // The generated name is used as both title and branch.
        let title = body.title.unwrap_or_default();
        let worktree_branch = match body.worktree_branch {
            Some(b) if b.is_empty() => {
                let generated = crate::session::civilizations::generate_random_title(&title_refs);
                Some(generated)
            }
            other => other,
        };
        // If title is empty and we generated a branch name, use it as the title too
        let title = if title.is_empty() {
            worktree_branch.clone().unwrap_or_default()
        } else {
            title
        };

        let params = InstanceParams {
            title,
            path: body.path,
            group: body.group,
            tool: body.tool,
            worktree_enabled: worktree_branch.is_some(),
            worktree_branch,
            create_new_branch: body.create_new_branch,
            base_branch: if body.create_new_branch {
                body.base_branch
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            },
            sandbox: body.sandbox,
            sandbox_image,
            yolo_mode: body.yolo_mode,
            extra_env: body.extra_env,
            extra_args: body.extra_args,
            command_override: body.command_override,
            extra_repo_paths,
            scratch: body.scratch,
        };

        let build_result = builder::build_instance(params, &title_refs, &branch_refs, &profile)?;
        let mut instance = build_result.instance;
        instance.source_profile = profile.clone();
        let build_warnings = build_result.warnings;

        // Apply per-session sandbox overrides from the request body.
        if let Some(ref mut sandbox) = instance.sandbox_info {
            if body.custom_instruction.is_some() {
                sandbox.custom_instruction = body.custom_instruction;
            }
        }

        // Apply cockpit fields from the request body. cockpit_mode is
        // silently downgraded to tmux when the master switch is off
        // (`cockpit.enabled = false` in config.toml).
        #[cfg(feature = "serve")]
        {
            instance.cockpit_mode = body.cockpit_mode && cockpit_master_enabled;
            instance.cockpit_agent = body.cockpit_agent;
            instance.cockpit_model = body.cockpit_model;
            // Don't trust the client's capability decision. Re-resolve
            // whether this agent can actually run in cockpit; a custom
            // agent without an `agent_cockpit_cmd` (or any non-ACP tool)
            // falls back to tmux here rather than erroring at spawn time.
            if instance.cockpit_mode {
                let acp_registry = crate::cockpit::AgentRegistry::with_defaults();
                let resolved = instance
                    .cockpit_agent
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(instance.tool.as_str());
                let capable = acp_registry.get(resolved).is_some()
                    || crate::session::repo_config::resolve_config_with_repo_or_warn(
                        &instance.source_profile,
                        std::path::Path::new(&instance.project_path),
                    )
                    .session
                    .agent_cockpit_cmd
                    .get(&instance.tool)
                    .is_some_and(|cmd| {
                        crate::cockpit::AgentSpec::from_cockpit_cmd(&instance.tool, cmd).is_ok()
                    });
                instance.cockpit_mode = capable;
            }
        }

        // Anything that fails between here and the final `Ok(..)`
        // would otherwise orphan the scratch directory `build_instance`
        // already provisioned (Storage::new, storage.update,
        // instance.start). Wrap the tail in an IIFE-equivalent closure
        // so we can run cleanup on Err once, regardless of which step
        // tripped. Matches the CLI cleanup path in
        // `cleanup_partial_session(... scratch_dir: Some(...))`.
        let mut persist_and_start = || -> anyhow::Result<()> {
            let storage = Storage::new(&profile)?;
            let to_persist = instance.clone();
            storage.update(|all, _groups| {
                all.push(to_persist);
                Ok(())
            })?;

            // Cockpit-mode sessions are not backed by tmux; the cockpit
            // supervisor spawns the ACP agent on demand. Skip the tmux
            // `start()` to avoid creating an empty pane that no one will
            // attach to.
            #[cfg(feature = "serve")]
            let skip_tmux_start = instance.cockpit_mode;
            #[cfg(not(feature = "serve"))]
            let skip_tmux_start = false;
            if !skip_tmux_start {
                instance.start()?;
            }
            Ok(())
        };

        if let Err(e) = persist_and_start() {
            // Guarded the same way as the deletion path: only remove a
            // path that `is_scratch_path` blesses, so a corrupted
            // `project_path` cannot trick us into wiping unrelated
            // state.
            if instance.scratch {
                let scratch_path = std::path::PathBuf::from(&instance.project_path);
                if crate::session::scratch::is_scratch_path(&scratch_path) {
                    if let Err(rm_err) = std::fs::remove_dir_all(&scratch_path) {
                        tracing::warn!(
                            target: "http.api.sessions",
                            "Failed to clean up orphan scratch dir {} after create failure: {}",
                            scratch_path.display(),
                            rm_err
                        );
                    }
                }
            }
            return Err(e);
        }

        Ok::<(Instance, Vec<String>), anyhow::Error>((instance, build_warnings))
    })
    .await;

    match result {
        Ok(Ok((instance, warnings))) => {
            let mut resp = SessionResponse::from_instance(
                &instance,
                crate::claude_settings::read_tui_fullscreen(),
            );
            resp.warnings = warnings;
            // Overlay custom-agent ACP capability so the wizard's redirect
            // into the new session renders the right substrate immediately.
            #[cfg(feature = "serve")]
            if !resp.acp_capable {
                let cockpit_cmd = crate::session::repo_config::resolve_config_with_repo_or_warn(
                    &instance.source_profile,
                    std::path::Path::new(&instance.project_path),
                )
                .session
                .agent_cockpit_cmd;
                resp.acp_capable = custom_agent_acp_capable(&cockpit_cmd, &instance.tool);
            }
            #[cfg(feature = "serve")]
            let cockpit_spawn_target = if instance.cockpit_mode {
                Some((
                    instance.id.clone(),
                    instance.tool.clone(),
                    instance.cockpit_agent.clone(),
                    instance.cockpit_model.clone(),
                    instance.project_path.clone(),
                    instance.cockpit_acp_session_id.clone(),
                    instance.source_profile.clone(),
                    instance.yolo_mode,
                ))
            } else {
                None
            };
            let mut instances = state.instances.write().await;
            instances.push(instance);
            drop(instances);

            // Count the create for the opt-in telemetry trend counter. Bounded
            // accumulator, read-and-decremented by the snapshot loop; no-op for
            // opted-out installs (the snapshot is never built / sent).
            state
                .telemetry_session_creates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            #[cfg(feature = "serve")]
            if let Some((
                id,
                tool,
                agent_override,
                model,
                project_path,
                stored_acp_session_id,
                source_profile,
                yolo_mode,
            )) = cockpit_spawn_target
            {
                let agent = state
                    .cockpit_supervisor
                    .pick_agent_for_tool(
                        &tool,
                        agent_override.as_deref(),
                        &source_profile,
                        std::path::Path::new(&project_path),
                    )
                    .await;
                let cwd = std::path::PathBuf::from(project_path);
                let supervisor = state.cockpit_supervisor.clone();
                let state_for_check = state.clone();
                tokio::spawn(async move {
                    // Initial session creation: run on_launch hooks
                    // once here so cockpit-mode parity with the tmux
                    // path is preserved (tmux fires on_launch from
                    // `instance.start()` at creation, then never
                    // again).
                    let inst_lock = state_for_check.instance_lock(&id).await;
                    let sandbox_info = match crate::cockpit::sandbox::ensure_container_for_session(
                        &state_for_check.instances,
                        &inst_lock,
                        &id,
                        true,
                    )
                    .await
                    {
                        Ok(info) => info,
                        Err(e) => {
                            let message = format!("sandbox container ensure failed: {e}");
                            tracing::warn!(
                                target: "cockpit.supervisor",
                                session = %id,
                                "auto-spawn after create failed: {message}"
                            );
                            supervisor.publish_startup_error(&id, message);
                            return;
                        }
                    };
                    // Pass the session profile through regardless of
                    // sandboxing so the spawn path resolves agent_cockpit_cmd
                    // and worker env from the right profile for non-sandbox
                    // sessions too.
                    let source_profile_for_spawn = Some(source_profile.clone());
                    if let Err(e) = supervisor
                        .spawn(crate::cockpit::supervisor::SpawnRequest {
                            session_id: id.clone(),
                            agent: agent.clone(),
                            cwd,
                            additional_dirs: vec![],
                            provider_env: vec![],
                            model,
                            stored_acp_session_id,
                            sandbox_info,
                            source_profile: source_profile_for_spawn,
                            yolo_mode,
                        })
                        .await
                    {
                        // If the session was deleted during the 2-3s
                        // ACP handshake, the spawn error is for a
                        // vanished session; demote to debug instead
                        // of publishing AgentStartupError to a UI
                        // that no longer exists.
                        let still_present = state_for_check
                            .instances
                            .read()
                            .await
                            .iter()
                            .any(|i| i.id == id);
                        let message = format!("Failed to start cockpit agent {agent:?}: {e}");
                        if still_present {
                            tracing::warn!(
                                target: "cockpit.supervisor",
                                session = %id,
                                "auto-spawn after create failed: {message}"
                            );
                            supervisor.publish_startup_error(&id, message);
                        } else {
                            tracing::debug!(
                                target: "cockpit.supervisor",
                                session = %id,
                                "auto-spawn after create error after session removed (ignored): {message}"
                            );
                        }
                    }
                });
            }

            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sessions", "Session creation failed: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "create_failed", "message": public_create_session_error(&e)})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Session creation panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
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
fn public_create_session_error(e: &anyhow::Error) -> String {
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
fn apply_post_restart_sync(live: &mut Instance, started: &Instance) {
    live.status = started.status;
    live.last_error = None;
    live.agent_session_id = started.agent_session_id.clone();
    live.last_start_time = started.last_start_time;
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
}

/// Narrow sibling of [`apply_post_restart_sync`] that propagates only the
/// fields the resume-fallback cascade is responsible for: the post-cascade
/// `agent_session_id` (either `None` after a bailed Tier-1 cleanup, or a
/// fresh UUID acquired by Tier-2's `start_with_size_opts` ->
/// `acquire_session_id`) and the updated `retroactive_capture_excludes`.
///
/// Intended for error paths where the cascade may have run but the caller
/// does not want to touch user-visible status fields. `NotRunning` is the
/// canonical use case: a recoverable transient state where overwriting
/// `live.status` with `started.status` (typically `Starting` from the
/// post-cascade `finalize_launch`) would briefly mis-paint a broken pane
/// as `Starting` until the 2s status poll loop reconciles.
fn apply_cascade_state_sync(live: &mut Instance, started: &Instance) {
    live.agent_session_id = started.agent_session_id.clone();
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
}

/// Ensure the main agent tmux session is alive, restarting it if dead.
///
/// Mirrors the TUI's `attach_session` restart logic: checks the actual tmux
/// state (exists / pane dead / running unexpected shell) and restarts the
/// instance when needed. Returns the resulting status so the frontend can
/// decide whether to proceed with the WebSocket attach.
///
/// Concurrency: a per-instance `tokio::sync::Mutex` serializes ensure calls
/// for the same session so two rapid POSTs don't both decide "dead" and race
/// on `tmux new-session`.
///
/// Read-only: in read-only mode, the endpoint may report `alive` but will
/// refuse to kill+restart a session. Returns 403 when a restart is needed.
///
/// Latency: bounded by `RESUME_PROBE_MAX` (~3s) per probe.
///   * No-op (pane alive): inspect-only, ~tmux RTT.
///   * Healthy resume: Tier-1 probe only, returns after the
///     `RESUME_PROBE_POST_SHELL_GRACE` (~2s) shortcut. Shell-wrapper
///     overrides charitably burn the full ~3s instead (see
///     `Instance::probe_settle`).
///   * Cascade fires (Tier-1 detects a dead pane): Tier-1 returns Dead
///     fast (`pane_dead`/`!exists` is unambiguous), then `kill_clean`
///     (~100ms macOS grace) + Tier-2 tmux spawn + up to another
///     `RESUME_PROBE_MAX`.
///
/// HTTP clients should budget ~6-7s worst-case for the full Tier-1 +
/// Tier-2 cascade and configure timeouts accordingly.
pub async fn ensure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    // Serialize concurrent ensure calls for the same session. The decision
    // phase reads tmux state and the restart phase mutates it; any other
    // ensure for this id must wait so both see a consistent view.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    // Inspect tmux + make the restart decision on a blocking thread. Refresh
    // the cache first so rapid re-calls see the true current state (the
    // background status poller only refreshes every 2s).
    let decision_instance = instance.clone();
    let id_for_log = id.clone();
    let decision = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        crate::tmux::refresh_session_cache();
        let tmux_session = decision_instance.tmux_session()?;
        let exists = tmux_session.exists();
        let pane_dead = exists && tmux_session.is_pane_dead();
        let needs_restart = if !exists || pane_dead {
            true
        } else if crate::hooks::read_hook_status(&decision_instance.id).is_some() {
            // Hook status tracks this session; shell detection is unreliable.
            false
        } else if decision_instance.has_command_override() {
            // Custom command overrides run agents through wrapper scripts that
            // look like shells to tmux. Don't restart based on shell detection.
            false
        } else {
            !decision_instance.expects_shell() && tmux_session.is_pane_running_shell()
        };
        tracing::debug!(target: "http.api.sessions",
            session_id = id_for_log,
            exists,
            pane_dead,
            needs_restart,
            "ensure_session: restart decision"
        );
        Ok(needs_restart)
    })
    .await;

    let needs_restart = match decision {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "ensure_session: failed to inspect tmux for {id}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "ensure_session inspect panicked for {id}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response();
        }
    };

    if !needs_restart {
        return (StatusCode::OK, Json(serde_json::json!({"status": "alive"}))).into_response();
    }

    if state.read_only {
        // Read-only viewers must not kill + respawn a dead session. Signal
        // the frontend so it can show "session is stopped; ask an owner to
        // reattach" instead of silently replacing the agent process.
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "read_only",
                "message": "Session is stopped or errored. Restart requires write access.",
            })),
        )
            .into_response();
    }

    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.status = crate::session::Status::Starting;
            inst.last_error = None;
        }
    }

    let restart_result = tokio::task::spawn_blocking(
        move || -> Result<(Instance, crate::session::StartOutcome), Box<(Instance, anyhow::Error)>> {
            let mut inst = instance;
            // Use kill_clean (vs bare tmux kill) so a remain-on-exit dead
            // pane is respawned-then-killed; bare kill races against the
            // session cache on macOS and can leave the corpse pane behind,
            // which then trips the next start_with_resume_fallback's
            // `pane_was_preexisting` short-circuit. See `Instance::kill_clean`.
            if let Err(e) = inst.kill_clean() {
                return Err(Box::new((inst, e)));
            }
            // Surface the moved Instance on the Err arm so the caller can
            // sync the cascade-cleared `agent_session_id` and updated
            // `retroactive_capture_excludes` back to live state. Otherwise
            // the live entry retains the stale sid in memory while disk has
            // already been cleared, and subsequent calls within the
            // `status_poll_loop` reload window (~2s) keep re-attempting
            // resume with the bad sid. See `apply_post_restart_sync`.
            match inst.start_with_resume_fallback(None, false) {
                Ok(outcome) => Ok((inst, outcome)),
                Err(e) => Err(Box::new((inst, e))),
            }
        },
    )
    .await;

    match restart_result {
        Ok(Ok((started, outcome))) => {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(inst, &started);
            }
            let resume_outcome = match &outcome {
                crate::session::StartOutcome::Resumed => "resumed",
                crate::session::StartOutcome::Restarted { .. } => "restarted",
                crate::session::StartOutcome::Fresh => "fresh",
            };
            let mut body = serde_json::json!({
                "status": "restarted",
                "resume_outcome": resume_outcome,
            });
            if let crate::session::StartOutcome::Restarted { stale_sid } = &outcome {
                body["stale_session_id"] = serde_json::Value::String(stale_sid.clone());
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "ensure_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(inst, &started);
                inst.status = crate::session::Status::Error;
                inst.last_error = Some(msg.clone());
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "restart_failed",
                    "message": msg,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "ensure_session panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

// --- Paired terminal ---

pub async fn ensure_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let instances = state.instances.read().await;
    let inst = match instances.iter().find(|i| i.id == id) {
        Some(i) => i.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
    };
    drop(instances);

    // Serialize concurrent terminal-ensure calls for the same session so two
    // parallel requests don't both try to create the same tmux session
    // (the second would fail with "duplicate session").
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    // Re-check after acquiring the lock; the first caller may have created it.
    // `has_terminal()` only checks the in-memory `terminal_info.created` flag.
    // The pane shell can exit (Ctrl+D, `exit`, SIGHUP from a destroyed tmux
    // client, etc.) while the flag stays true and the session keeps existing
    // (because we set tmux's `remain-on-exit on`). When that happens the web
    // UI would attach to a dead pane that swallows every keystroke, so do
    // the same kill+recreate dance the TUI runs in src/tui/app.rs around the
    // attach path.
    {
        let instances = state.instances.read().await;
        if let Some(i) = instances.iter().find(|i| i.id == id) {
            if i.has_terminal() {
                let pane_dead = i
                    .terminal_tmux_session()
                    .ok()
                    .map(|s| s.exists() && s.is_pane_dead())
                    .unwrap_or(false);
                if !pane_dead {
                    return (
                        StatusCode::OK,
                        Json(serde_json::json!({"status": "exists"})),
                    )
                        .into_response();
                }
                tracing::warn!(
                    target: "terminal.ws",
                    session = %id,
                    "paired terminal pane is dead, respawning"
                );
            }
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_terminal_if_dead();
        inst_clone.start_terminal()
    })
    .await;

    match result {
        Ok(Ok(())) => {
            // Update in-memory cache
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({"status": "created"})),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Terminal creation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "create_failed", "message": "Failed to create terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Terminal creation panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn ensure_container_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let instances = state.instances.read().await;
    let inst = match instances.iter().find(|i| i.id == id) {
        Some(i) => i.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
    };
    drop(instances);

    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    // Same dead-pane rescue as `ensure_terminal`: an existing-but-dead
    // pane would otherwise silently swallow every keystroke from the
    // browser. See the longer comment in `ensure_terminal`.
    {
        let instances = state.instances.read().await;
        if let Some(i) = instances.iter().find(|i| i.id == id) {
            if i.has_container_terminal() {
                let pane_dead = i
                    .container_terminal_tmux_session()
                    .ok()
                    .map(|s| s.exists() && s.is_pane_dead())
                    .unwrap_or(false);
                if !pane_dead {
                    return (
                        StatusCode::OK,
                        Json(serde_json::json!({"status": "exists"})),
                    )
                        .into_response();
                }
                tracing::warn!(
                    target: "terminal.ws",
                    session = %id,
                    "container terminal pane is dead, respawning"
                );
            }
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_container_terminal_if_dead();
        inst_clone.start_container_terminal_with_size(None)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "created"})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Container terminal creation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "create_failed", "message": "Failed to create container terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Container terminal creation panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

// --- Rich Diff (per-file, merge-base aware) ---

#[derive(Serialize)]
pub struct RichDiffFileInfo {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    /// Name of the workspace repo this file belongs to. None for
    /// single-repo (non-workspace) sessions. The frontend uses this to
    /// group entries in the sidebar diff list and to disambiguate
    /// path collisions across repos. See #1047.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
}

#[derive(Serialize)]
pub struct RepoBase {
    /// None for single-repo sessions; Some for each workspace member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub base_branch: String,
}

#[derive(Serialize)]
pub struct RichDiffFilesResponse {
    pub files: Vec<RichDiffFileInfo>,
    /// One entry per repo whose diff was computed. Single-repo
    /// sessions get a one-element array with `repo_name: None`;
    /// workspace sessions get one entry per workspace member. Replaces
    /// the previous single-string `base_branch` since each member can
    /// have a different default. See #1047.
    pub per_repo_bases: Vec<RepoBase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Serialize)]
pub struct RichDiffLine {
    #[serde(rename = "type")]
    pub change_type: String,
    pub old_line_num: Option<usize>,
    pub new_line_num: Option<usize>,
    pub content: String,
}

#[derive(Serialize)]
pub struct RichDiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<RichDiffLine>,
}

#[derive(Serialize)]
pub struct RichFileDiffResponse {
    pub file: RichDiffFileInfo,
    pub hunks: Vec<RichDiffHunk>,
    pub is_binary: bool,
    /// True if the file was too large to diff and hunks were omitted.
    pub truncated: bool,
}

/// Max combined bytes of old+new content before we bail on diffing.
const MAX_DIFF_BYTES: usize = 2_000_000;
/// Max combined line count of old+new before we bail on diffing.
const MAX_DIFF_LINES: usize = 40_000;

/// Validate a user-supplied relative file path against a workdir.
///
/// Returns the canonicalized absolute path if the requested path is safe to
/// read (no absolute, no `..`, no symlink-escape out of the workdir) and
/// appears in `changed_files` (so only actually-diffed files are exposed).
/// Returns `Err(status, message)` otherwise.
fn validate_diff_path(
    workdir: &std::path::Path,
    requested: &std::path::Path,
    changed_files: &[crate::git::diff::DiffFile],
) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
    use std::path::Component;

    if requested.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty path"));
    }
    if requested.is_absolute() {
        return Err((StatusCode::BAD_REQUEST, "absolute path not allowed"));
    }
    for comp in requested.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((StatusCode::BAD_REQUEST, "path escapes workdir"));
            }
            _ => {}
        }
    }

    // Cross-check: path must be one of the currently-changed files.
    // This is the narrowest trust boundary: only files the user actually
    // modified on this branch are diffable, not arbitrary files in the worktree.
    let matches_changed = changed_files.iter().any(|f| f.path == requested);
    if !matches_changed {
        return Err((StatusCode::NOT_FOUND, "file not in changed set"));
    }

    // Canonicalize both sides and verify containment as defense in depth
    // against symlinks that might point outside the workdir.
    let canonical_workdir = workdir.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "workdir canonicalize failed",
        )
    })?;
    let full = canonical_workdir.join(requested);
    // The file may not exist on disk (e.g., deleted in the working tree), in
    // which case canonicalize fails; fall back to the non-canonical path and
    // just verify textual containment.
    let final_path = match full.canonicalize() {
        Ok(c) => {
            if !c.starts_with(&canonical_workdir) {
                return Err((StatusCode::BAD_REQUEST, "path escapes workdir"));
            }
            c
        }
        Err(_) => full,
    };
    Ok(final_path)
}

/// One repo's worth of diff context: a name (for workspace members)
/// and the filesystem path the diff helper walks. See #1047.
#[derive(Clone, Debug)]
struct DiffRepo {
    /// Workspace member name, or None for single-repo sessions.
    name: Option<String>,
    path: String,
}

struct DiffContext {
    repos: Vec<DiffRepo>,
    /// Per-session override for the diff base (set via
    /// `PATCH /api/sessions/{id}/diff-base`, the `aoe session set-base`
    /// CLI, or the TUI diff view's `b` keybind). Wins over the
    /// profile-level default and the auto-detected ref. See #970.
    base_branch_override: Option<String>,
}

/// Expand a session into the list of repos whose diffs the sidebar
/// cares about. Workspace sessions iterate `workspace_info.repos`
/// (each `worktree_path` becomes one entry); single-repo sessions
/// fall back to a one-element list of `[project_path]` so the
/// existing flow is unchanged. See #1047.
async fn resolve_diff_repos(
    state: &AppState,
    id: &str,
) -> Result<DiffContext, axum::response::Response> {
    let instances = state.instances.read().await;
    let inst = instances.iter().find(|i| i.id == id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found", "message": "Session not found"})),
        )
            .into_response()
    })?;
    let repos = if let Some(ws) = inst.workspace_info.as_ref() {
        ws.repos
            .iter()
            .map(|r| DiffRepo {
                name: Some(r.name.clone()),
                path: r.worktree_path.clone(),
            })
            .collect()
    } else {
        vec![DiffRepo {
            name: None,
            path: inst.project_path.clone(),
        }]
    };
    Ok(DiffContext {
        repos,
        base_branch_override: inst.base_branch_override.clone(),
    })
}

/// Resolve the diff base for one repo path. Override (per-session)
/// wins over the profile's `DiffConfig.default_branch`, which wins
/// over auto-detection (`get_default_base_ref`). See #970.
fn resolve_diff_base(
    override_value: Option<&str>,
    config_default: Option<&str>,
    repo_path: &std::path::Path,
) -> String {
    if let Some(v) = override_value.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = config_default.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    crate::git::diff::get_default_base_ref(repo_path).unwrap_or_else(|_| "main".to_string())
}

pub async fn session_diff_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let result = tokio::task::spawn_blocking(move || {
        use crate::git::diff;

        let config_default = crate::session::Config::load_or_warn()
            .diff
            .default_branch
            .clone();
        let mut all_files: Vec<RichDiffFileInfo> = Vec::new();
        let mut per_repo_bases: Vec<RepoBase> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for repo in &ctx.repos {
            let path = std::path::Path::new(&repo.path);
            let base_branch = resolve_diff_base(
                ctx.base_branch_override.as_deref(),
                config_default.as_deref(),
                path,
            );
            let warning = diff::check_merge_base_status(path, &base_branch);
            let changed = diff::compute_changed_files(path, &base_branch).unwrap_or_default();

            for f in changed {
                all_files.push(RichDiffFileInfo {
                    path: f.path.to_string_lossy().to_string(),
                    old_path: f.old_path.map(|p| p.to_string_lossy().to_string()),
                    status: f.status.label().to_string(),
                    additions: f.additions,
                    deletions: f.deletions,
                    repo_name: repo.name.clone(),
                });
            }
            per_repo_bases.push(RepoBase {
                repo_name: repo.name.clone(),
                base_branch: base_branch.clone(),
            });
            if let Some(w) = warning {
                match repo.name.as_deref() {
                    Some(n) => warnings.push(format!("{n}: {w}")),
                    None => warnings.push(w),
                }
            }
        }

        RichDiffFilesResponse {
            files: all_files,
            per_repo_bases,
            warning: if warnings.is_empty() {
                None
            } else {
                Some(warnings.join("\n"))
            },
        }
    })
    .await;

    match result {
        Ok(resp) => (
            StatusCode::OK,
            Json(serde_json::to_value(resp).expect("RichDiffFilesResponse is always serializable")),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Diff files panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct FileDiffQuery {
    pub path: String,
    /// Workspace repo name when the session is a multi-repo workspace.
    /// Omitted for single-repo sessions; if a workspace session omits
    /// it, the handler defaults to the first member so the legacy
    /// single-repo URL keeps working for the primary repo. See #1047.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Response for a rejected diff request (bad path, file not changed, etc.).
enum DiffFileError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Internal(anyhow::Error),
}

pub async fn session_diff_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<FileDiffQuery>,
) -> impl IntoResponse {
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    // Pick the workspace member named in `?repo=`. When the param is
    // missing we default to the first member, which matches the
    // legacy single-repo URL contract (`?path=...` against the
    // session's primary repo). When the named repo doesn't exist, the
    // request is rejected so a stale link doesn't quietly diff the
    // wrong repo. See #1047.
    let selected_repo =
        match query.repo.as_deref() {
            Some(name) => match ctx.repos.iter().find(|r| r.name.as_deref() == Some(name)) {
                Some(r) => r.clone(),
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "bad_request",
                            "message": "unknown workspace repo"
                        })),
                    )
                        .into_response();
                }
            },
            None => ctx.repos.first().cloned().expect(
                "resolve_diff_repos always returns at least one entry (single-repo fallback)",
            ),
        };
    let project_path = selected_repo.path;
    let selected_repo_name = selected_repo.name;
    let base_branch_override = ctx.base_branch_override.clone();

    let result =
        tokio::task::spawn_blocking(move || -> Result<RichFileDiffResponse, DiffFileError> {
            use crate::git::diff;
            use similar::ChangeTag;

            let repo_path = std::path::Path::new(&project_path);
            let file_path = std::path::Path::new(&query.path);

            let config_default = crate::session::Config::load_or_warn()
                .diff
                .default_branch
                .clone();
            let base_branch = resolve_diff_base(
                base_branch_override.as_deref(),
                config_default.as_deref(),
                repo_path,
            );

            // Validate the requested path against the set of actually-changed files.
            // This is the primary security boundary: only files modified on this
            // branch are diffable, preventing arbitrary file reads via ?path=...
            let changed_files = diff::compute_changed_files(repo_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            match validate_diff_path(repo_path, file_path, &changed_files) {
                Ok(_) => {}
                Err((status, msg)) => {
                    return Err(if status == StatusCode::NOT_FOUND {
                        DiffFileError::NotFound(msg)
                    } else {
                        DiffFileError::BadRequest(msg)
                    });
                }
            }

            let file_diff = diff::compute_file_diff(repo_path, file_path, &base_branch, 3)
                .map_err(|e| DiffFileError::Internal(e.into()))?;

            let file = RichDiffFileInfo {
                path: file_diff.file.path.to_string_lossy().to_string(),
                old_path: file_diff
                    .file
                    .old_path
                    .map(|p| p.to_string_lossy().to_string()),
                status: file_diff.file.status.label().to_string(),
                additions: file_diff.file.additions,
                deletions: file_diff.file.deletions,
                repo_name: selected_repo_name.clone(),
            };

            // Size cap: avoid OOM'ing the browser on huge files (minified bundles,
            // generated code, data blobs that slipped past .gitignore).
            let total_line_count: usize = file_diff.hunks.iter().map(|h| h.lines.len()).sum();
            let total_bytes: usize = file_diff
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .map(|l| l.content.len())
                .sum();
            if total_line_count > MAX_DIFF_LINES || total_bytes > MAX_DIFF_BYTES {
                return Ok(RichFileDiffResponse {
                    file,
                    hunks: Vec::new(),
                    is_binary: file_diff.is_binary,
                    truncated: true,
                });
            }

            let hunks: Vec<RichDiffHunk> = file_diff
                .hunks
                .into_iter()
                .map(|h| RichDiffHunk {
                    old_start: h.old_start,
                    old_lines: h.old_lines,
                    new_start: h.new_start,
                    new_lines: h.new_lines,
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l| RichDiffLine {
                            change_type: match l.tag {
                                ChangeTag::Insert => "add".to_string(),
                                ChangeTag::Delete => "delete".to_string(),
                                ChangeTag::Equal => "equal".to_string(),
                            },
                            old_line_num: l.old_line_num,
                            new_line_num: l.new_line_num,
                            content: l.content,
                        })
                        .collect(),
                })
                .collect();

            Ok(RichFileDiffResponse {
                file,
                hunks,
                is_binary: file_diff.is_binary,
                truncated: false,
            })
        })
        .await;

    match result {
        Ok(Ok(resp)) => (
            StatusCode::OK,
            Json(serde_json::to_value(resp).expect("RichFileDiffResponse is always serializable")),
        )
            .into_response(),
        Ok(Err(DiffFileError::BadRequest(msg))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_request", "message": msg})),
        )
            .into_response(),
        Ok(Err(DiffFileError::NotFound(msg))) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found", "message": msg})),
        )
            .into_response(),
        Ok(Err(DiffFileError::Internal(e))) => {
            tracing::error!(target: "http.api.sessions", "File diff failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "diff_failed", "message": "Failed to compute file diff"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "File diff panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_test_instance() -> Instance {
        let mut inst = Instance::new("test-session", "/tmp/test-project");
        inst.tool = "claude".to_string();
        inst.status = Status::Running;
        inst.group_path = "work/projects".to_string();
        inst
    }

    #[test]
    fn from_instance_surfaces_hook_urgent_flag() {
        // #1640: the web Attention sort needs `Instance::is_urgent()` on the
        // wire. Write the hook-side attention.json the agent would emit and
        // confirm it round-trips onto the response, then confirm a session
        // with no hook file reports urgent: false.
        let inst = make_test_instance();
        let dir = crate::hooks::hook_status_dir(&inst.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("attention.json"),
            r#"{"urgent":true,"urgent_reason":"needs input"}"#,
        )
        .unwrap();

        let urgent_resp = SessionResponse::from_instance(&inst, false);
        assert!(urgent_resp.urgent, "hook-flagged session must be urgent");

        crate::hooks::cleanup_hook_status_dir(&inst.id);
        let plain_resp = SessionResponse::from_instance(&inst, false);
        assert!(
            !plain_resp.urgent,
            "session with no hook file must not be urgent"
        );
    }

    #[test]
    fn public_create_session_error_forwards_whitelisted_git_errors() {
        let dup: anyhow::Error =
            GitError::WorktreeAlreadyExists(std::path::PathBuf::from("/tmp/repo-worktrees/foo"))
                .into();
        assert_eq!(
            public_create_session_error(&dup),
            "Worktree already exists at /tmp/repo-worktrees/foo"
        );

        let in_use: anyhow::Error =
            GitError::BranchAlreadyCheckedOut("feature/foo".to_string()).into();
        assert_eq!(
            public_create_session_error(&in_use),
            "Branch 'feature/foo' is already in use by another worktree"
        );

        // Whitelisted variants survive an anyhow::Context wrapper too.
        let wrapped = anyhow::Error::from(GitError::BranchNotFound("nope".to_string()))
            .context("while creating worktree");
        assert_eq!(
            public_create_session_error(&wrapped),
            "Branch 'nope' not found"
        );
    }

    #[test]
    fn public_create_session_error_hides_unsafe_messages() {
        // Raw git stderr (even already-sanitized) must not reach the client.
        let cmd: anyhow::Error = GitError::WorktreeCommandFailed(
            "fatal: unable to access 'https://<redacted>@host/repo.git'".to_string(),
        )
        .into();
        assert_eq!(
            public_create_session_error(&cmd),
            "Failed to create session"
        );

        let clone: anyhow::Error =
            GitError::CloneFailed("https://alice:supersecret@host/repo.git".to_string()).into();
        let msg = public_create_session_error(&clone);
        assert_eq!(msg, "Failed to create session");
        assert!(!msg.contains("supersecret"));

        // A non-GitError anyhow also stays generic.
        let other = anyhow::anyhow!("something internal at /home/user/.config/secret");
        assert_eq!(
            public_create_session_error(&other),
            "Failed to create session"
        );
    }

    #[test]
    fn session_response_from_instance() {
        let inst = make_test_instance();
        let resp = SessionResponse::from_instance(&inst, false);

        assert_eq!(resp.id, inst.id);
        assert_eq!(resp.title, "test-session");
        assert_eq!(resp.project_path, "/tmp/test-project");
        assert_eq!(resp.tool, "claude");
        assert_eq!(resp.status, "Running");
        assert_eq!(resp.group_path, "work/projects");
        assert!(!resp.is_sandboxed);
        assert!(!resp.has_terminal);
    }

    #[test]
    fn session_response_status_variants() {
        let mut inst = make_test_instance();

        for (status, expected) in [
            (Status::Running, "Running"),
            (Status::Waiting, "Waiting"),
            (Status::Error, "Error"),
            (Status::Stopped, "Stopped"),
            (Status::Idle, "Idle"),
            (Status::Starting, "Starting"),
        ] {
            inst.status = status;
            assert_eq!(
                SessionResponse::from_instance(&inst, false).status,
                expected
            );
        }
    }

    #[test]
    fn session_response_branch_from_worktree() {
        let mut inst = make_test_instance();
        assert!(SessionResponse::from_instance(&inst, false)
            .branch
            .is_none());

        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/test".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        assert_eq!(
            SessionResponse::from_instance(&inst, false)
                .branch
                .as_deref(),
            Some("feature/test")
        );
    }

    #[test]
    fn session_response_surfaces_base_branch_override() {
        let mut inst = make_test_instance();
        // Default: no override -> field omitted from JSON.
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(
            json.get("base_branch_override").is_none(),
            "base_branch_override should be omitted when None, got: {json}"
        );

        inst.base_branch_override = Some("upstream/main".to_string());
        let resp = SessionResponse::from_instance(&inst, false);
        assert_eq!(resp.base_branch_override.as_deref(), Some("upstream/main"));
    }

    #[test]
    fn resolve_diff_base_prefers_override_then_config_then_auto() {
        let tmp = tempfile::tempdir().unwrap();
        // Override wins over everything.
        assert_eq!(
            resolve_diff_base(Some("release-1.2"), Some("develop"), tmp.path()),
            "release-1.2"
        );
        // Config wins when no override; empty / whitespace override falls
        // through to the next layer.
        assert_eq!(
            resolve_diff_base(Some("   "), Some("develop"), tmp.path()),
            "develop"
        );
        assert_eq!(
            resolve_diff_base(None, Some("develop"), tmp.path()),
            "develop"
        );
        // Auto-detect when neither is set. The tmp dir is not a repo so
        // `get_default_base_ref` returns Err -> "main" fallback.
        assert_eq!(resolve_diff_base(None, None, tmp.path()), "main");
    }

    #[test]
    fn session_response_surfaces_base_branch_when_set() {
        let mut inst = make_test_instance();
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/test".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: Some("release-1.2".to_string()),
        });
        let resp = SessionResponse::from_instance(&inst, false);
        assert_eq!(resp.base_branch.as_deref(), Some("release-1.2"));

        // Field is omitted from the wire JSON when None so old clients
        // don't see a flood of nulls.
        inst.worktree_info.as_mut().unwrap().base_branch = None;
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(
            json.get("base_branch").is_none(),
            "base_branch should be omitted when None, got: {json}"
        );
    }

    #[test]
    fn session_response_serializes_to_json() {
        let inst = make_test_instance();
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();

        assert!(json.get("id").is_some());
        assert_eq!(json["tool"], "claude");
        assert_eq!(json["status"], "Running");
        assert_eq!(json["is_sandboxed"], false);
        assert_eq!(json["claude_fullscreen"], false);
    }

    #[test]
    fn session_response_omits_empty_warnings() {
        let inst = make_test_instance();
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(resp.warnings.is_empty());

        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("warnings").is_none(),
            "empty warnings should be omitted from the JSON body, got: {json}"
        );
    }

    #[test]
    fn session_response_serializes_populated_warnings() {
        let inst = make_test_instance();
        let mut resp = SessionResponse::from_instance(&inst, false);
        resp.warnings = vec![
            "post-checkout hook failed for repo-a".to_string(),
            "post-checkout hook failed for repo-b".to_string(),
        ];

        let json = serde_json::to_value(&resp).unwrap();
        let warnings = json
            .get("warnings")
            .expect("warnings should appear in JSON when populated");
        let arr = warnings
            .as_array()
            .expect("warnings should serialize as a JSON array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "post-checkout hook failed for repo-a");
        assert_eq!(arr[1], "post-checkout hook failed for repo-b");
    }

    #[test]
    fn claude_fullscreen_set_for_claude_when_enabled() {
        let resp = SessionResponse::from_instance(&make_test_instance(), true);
        assert_eq!(resp.tool, "claude");
        assert!(resp.claude_fullscreen);
    }

    #[test]
    fn session_response_surfaces_pinned_at() {
        let mut inst = make_test_instance();

        // Default: no pin -> field omitted from the JSON body.
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(
            json.get("pinned_at").is_none(),
            "pinned_at should be omitted when None, got: {json}"
        );

        inst.pin();
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(resp.pinned_at.is_some(), "pinned_at must surface when set");
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("pinned_at").is_some(),
            "pinned_at must appear in JSON when set"
        );
    }

    #[test]
    fn session_response_surfaces_archived_at() {
        let mut inst = make_test_instance();
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(json.get("archived_at").is_none());

        inst.archive();
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(resp.archived_at.is_some());
    }

    #[test]
    fn session_response_gates_snoozed_until_on_active_snooze() {
        let mut inst = make_test_instance();

        // Not snoozed -> field omitted.
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(resp.snoozed_until.is_none());

        // Active snooze -> field surfaced.
        inst.snooze(30);
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(resp.snoozed_until.is_some());

        // Expired snooze -> stays on disk for the next mutation to rewrite,
        // but the API gates on `is_snoozed()` so the wire value is None.
        // This prevents the web from rendering "snoozed 0m" on rows that
        // have already woken on the server.
        inst.snoozed_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        let resp = SessionResponse::from_instance(&inst, false);
        assert!(
            resp.snoozed_until.is_none(),
            "expired snooze must be filtered out on the wire even though the persisted field stays set"
        );
    }

    #[test]
    fn update_pin_body_parses() {
        let body: UpdatePinBody = serde_json::from_str(r#"{"pinned": true}"#).unwrap();
        assert!(body.pinned);
        let body: UpdatePinBody = serde_json::from_str(r#"{"pinned": false}"#).unwrap();
        assert!(!body.pinned);
    }

    #[test]
    fn update_archive_body_defaults_kill_pane_to_true() {
        let body: UpdateArchiveBody = serde_json::from_str(r#"{"archived": true}"#).unwrap();
        assert!(body.archived);
        assert!(
            body.kill_pane,
            "kill_pane must default to true so callers that omit the field get TUI/CLI parity"
        );

        let body: UpdateArchiveBody =
            serde_json::from_str(r#"{"archived": true, "kill_pane": false}"#).unwrap();
        assert!(body.archived);
        assert!(!body.kill_pane);
    }

    #[test]
    fn update_snooze_body_parses_minutes_and_null() {
        let body: UpdateSnoozeBody = serde_json::from_str(r#"{"minutes": 60}"#).unwrap();
        assert_eq!(body.minutes, Some(60));

        // `{"minutes": null}` and an empty body both mean unsnooze.
        let body: UpdateSnoozeBody = serde_json::from_str(r#"{"minutes": null}"#).unwrap();
        assert_eq!(body.minutes, None);
        let body: UpdateSnoozeBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(body.minutes, None);
    }

    #[test]
    fn update_snooze_validates_against_shared_bounds() {
        // The handler uses `validate_snooze_duration` to reject 0 and >
        // SNOOZE_MAX_MINUTES. Mirror the assertions here so a regression in
        // the validator shape (or in the dialog presets at
        // src/tui/dialogs/snooze_duration.rs) is caught locally.
        assert!(crate::session::validate_snooze_duration(0).is_err());
        for &m in &[60u64, 120, 180, 240, 300, 360, 1440, 7 * 1440] {
            assert!(
                crate::session::validate_snooze_duration(m).is_ok(),
                "preset {m} min must pass validator (matches TUI dialog presets)"
            );
        }
    }

    #[test]
    fn claude_fullscreen_unset_for_non_claude_even_when_enabled() {
        let mut inst = make_test_instance();
        inst.tool = "cursor".to_string();
        let resp = SessionResponse::from_instance(&inst, true);
        assert!(!resp.claude_fullscreen);
    }

    #[test]
    fn claude_fullscreen_unset_when_setting_disabled() {
        let resp = SessionResponse::from_instance(&make_test_instance(), false);
        assert!(!resp.claude_fullscreen);
    }

    #[test]
    fn rename_updates_title_without_changing_worktree_branch() {
        let mut inst = make_test_instance();
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/test".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        apply_session_title_rename(&mut inst, "Renamed Session".to_string());

        assert_eq!(inst.title, "Renamed Session");
        assert_eq!(
            inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("feature/test")
        );
    }

    #[test]
    fn worktree_name_edit_updates_path_and_optionally_branch() {
        let mut inst = make_test_instance();
        inst.project_path = "/tmp/repo-worktrees/old".to_string();
        inst.title = "My Session".to_string();
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "old".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        // Path-only edit leaves the branch and title untouched.
        apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/new", None);
        assert_eq!(inst.project_path, "/tmp/repo-worktrees/new");
        assert_eq!(inst.title, "My Session");
        assert_eq!(
            inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("old")
        );

        // Branch rename also updates worktree_info.branch.
        apply_worktree_name_edit(&mut inst, "/tmp/repo-worktrees/newer", Some("newer"));
        assert_eq!(inst.project_path, "/tmp/repo-worktrees/newer");
        assert_eq!(inst.title, "My Session");
        assert_eq!(
            inst.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("newer")
        );
    }

    #[test]
    fn apply_post_restart_sync_propagates_agent_session_id() {
        // Models the rapid double-restart case: in-memory state is stale
        // (agent_session_id = None) because the 2s status poller hasn't
        // refreshed yet, while the just-finished restart produced a Claude
        // UUID via acquire_session_id. The sync must propagate that ID so a
        // second ensure_session within the poller window doesn't generate a
        // fresh UUID and orphan the persisted Claude conversation.
        let mut live = make_test_instance();
        live.status = Status::Stopped;
        live.last_error = Some("prior failure".to_string());
        live.agent_session_id = None;
        live.last_start_time = None;

        let mut started = make_test_instance();
        started.status = Status::Starting;
        started.agent_session_id = Some("claude-uuid-restart".to_string());
        started.last_start_time = Some(std::time::Instant::now());

        apply_post_restart_sync(&mut live, &started);

        assert_eq!(live.status, Status::Starting);
        assert!(live.last_error.is_none());
        assert_eq!(
            live.agent_session_id.as_deref(),
            Some("claude-uuid-restart")
        );
        assert_eq!(live.last_start_time, started.last_start_time);
    }

    #[test]
    fn apply_post_restart_sync_overwrites_stale_session_id() {
        // If somehow the in-memory ID was non-None and the start path
        // produced a different (newer) ID, the sync must use the newer one.
        // Belt-and-suspenders: in practice acquire_session_id reuses an
        // existing ID, but the contract here is "started wins."
        let mut live = make_test_instance();
        live.agent_session_id = Some("stale-id".to_string());

        let mut started = make_test_instance();
        started.agent_session_id = Some("fresh-id".to_string());

        apply_post_restart_sync(&mut live, &started);

        assert_eq!(live.agent_session_id.as_deref(), Some("fresh-id"));
    }

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(target_os = "linux")]
        {
            let config_home = temp_home.join(".config");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            config_home.join(crate::session::APP_DIR_NAME_LINUX)
        }
        #[cfg(not(target_os = "linux"))]
        {
            temp_home.join(crate::session::APP_DIR_NAME_OTHER)
        }
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_accepts_builtin_agent() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let project = tempfile::tempdir().unwrap();

        assert!(validate_session_tool_identity(
            "claude",
            "default",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_accepts_non_empty_configured_custom_agent() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = "ssh -t host claude"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        assert!(validate_session_tool_identity(
            "remote-claude",
            "default",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_rejects_unknown_agent() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let project = tempfile::tempdir().unwrap();

        assert!(!validate_session_tool_identity(
            "surprise-agent",
            "default",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_rejects_empty_custom_agent_command() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = ""
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        assert!(!validate_session_tool_identity(
            "remote-claude",
            "default",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_rejects_whitespace_only_custom_agent_command() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = "   "
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        assert!(!validate_session_tool_identity(
            "remote-claude",
            "default",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_uses_requested_profile() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        let work_profile = app_dir.join("profiles").join("work");
        std::fs::create_dir_all(&work_profile).unwrap();
        std::fs::write(
            work_profile.join("config.toml"),
            r#"
                [session.custom_agents]
                work-agent = "ssh -t work claude"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        assert!(!validate_session_tool_identity(
            "work-agent",
            "default",
            project.path()
        ));
        assert!(validate_session_tool_identity(
            "work-agent",
            "work",
            project.path()
        ));
    }

    #[test]
    #[serial_test::serial]
    fn session_tool_identity_uses_repo_aware_config_for_request_path() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = tempfile::tempdir().unwrap();
        let repo_config_dir = project.path().join(".agent-of-empires");
        std::fs::create_dir_all(&repo_config_dir).unwrap();
        std::fs::write(
            repo_config_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                repo-agent = "ssh -t repo claude"
            "#,
        )
        .unwrap();

        assert!(validate_session_tool_identity(
            "repo-agent",
            "default",
            project.path()
        ));
    }

    #[test]
    fn create_session_validates_tool_before_builder_or_persistence() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions.rs"),
        )
        .unwrap();
        let create_start = source.find("pub async fn create_session").unwrap();
        let create_source = &source[create_start..];
        let validation = create_source
            .find("validate_session_tool_identity")
            .unwrap();
        let unwrap_or_else = create_source.find("body.profile.unwrap_or_else").unwrap();
        let spawn_blocking = create_source.find("tokio::task::spawn_blocking").unwrap();
        let builder = create_source.find("builder::build_instance").unwrap();
        let storage = create_source.find("Storage::new").unwrap();

        assert!(validation < unwrap_or_else);
        assert!(validation < spawn_blocking);
        assert!(validation < builder);
        assert!(validation < storage);
        assert!(create_source.contains("body.profile.as_deref().unwrap_or(&state.profile)"));
        assert!(create_source.contains("std::path::Path::new(&body.path)"));
        assert!(!create_source[validation..spawn_blocking].contains("command_override"));
    }
    // ── validate_diff_path: security regression tests ──────────────────────────
    //
    // Regression for a path-traversal vulnerability in the first cut of the
    // `/api/sessions/{id}/diff/file?path=...` endpoint. Any authenticated user
    // could pass `?path=/etc/passwd` or `?path=../../etc/shadow` and have the
    // server dump the file contents in a diff response. The validator must
    // reject absolute paths, parent-dir traversal, and any path that isn't in
    // the set of actually-changed files.

    use crate::git::diff::{DiffFile, FileStatus};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn changed(paths: &[&str]) -> Vec<DiffFile> {
        paths
            .iter()
            .map(|p| DiffFile {
                path: PathBuf::from(p),
                old_path: None,
                status: FileStatus::Modified,
                additions: 0,
                deletions: 0,
            })
            .collect()
    }

    #[test]
    fn validate_diff_path_rejects_absolute() {
        let dir = TempDir::new().unwrap();
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new("/etc/passwd"),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_diff_path_rejects_parent_dir() {
        let dir = TempDir::new().unwrap();
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new("../../etc/passwd"),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_diff_path_rejects_parent_dir_in_middle() {
        let dir = TempDir::new().unwrap();
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new("src/../../etc/passwd"),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_diff_path_rejects_empty() {
        let dir = TempDir::new().unwrap();
        let err = validate_diff_path(dir.path(), std::path::Path::new(""), &[]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_diff_path_rejects_unchanged_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
        // File exists inside workdir but is not in the changed set.
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new("existing.txt"),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn validate_diff_path_accepts_changed_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("changed.txt"), "hello").unwrap();
        let ok = validate_diff_path(
            dir.path(),
            std::path::Path::new("changed.txt"),
            &changed(&["changed.txt"]),
        );
        assert!(ok.is_ok(), "expected Ok, got {:?}", ok);
    }

    #[test]
    fn validate_diff_path_accepts_deleted_file() {
        // A file that has been deleted on disk but is in the changed set
        // (status: Deleted) should still be diffable so the user can see
        // what was removed. canonicalize() on the joined path will fail,
        // so the validator must fall back to the non-canonical path.
        let dir = TempDir::new().unwrap();
        let ok = validate_diff_path(
            dir.path(),
            std::path::Path::new("deleted.txt"),
            &changed(&["deleted.txt"]),
        );
        assert!(ok.is_ok(), "expected Ok, got {:?}", ok);
    }

    #[test]
    fn truncate_title_returns_unchanged_under_limit() {
        assert_eq!(truncate_title("hello", 10), "hello");
    }

    #[test]
    fn truncate_title_returns_unchanged_at_exact_limit() {
        assert_eq!(truncate_title("hello", 5), "hello");
    }

    #[test]
    fn truncate_title_appends_ellipsis_when_over_limit() {
        let out = truncate_title("abcdefghij", 5);
        assert_eq!(out, "abcd…");
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_title_counts_characters_not_bytes() {
        // Multi-byte input: each ☃ is 3 bytes, 1 char. Truncating to 3
        // chars must split on character boundary, not byte offset.
        let out = truncate_title("☃☃☃☃☃", 3);
        assert_eq!(out, "☃☃…");
        assert_eq!(out.chars().count(), 3);
    }

    fn step(
        id: &str,
        title: &str,
        status: crate::cockpit::state::PlanStepStatus,
    ) -> crate::cockpit::state::PlanStep {
        crate::cockpit::state::PlanStep {
            id: id.into(),
            title: title.into(),
            detail: None,
            status,
        }
    }

    #[test]
    fn plan_summary_counts_done_steps_only() {
        use crate::cockpit::state::PlanStepStatus::*;
        let plan = crate::cockpit::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps: vec![
                step("a", "alpha", Done),
                step("b", "beta", Done),
                step("c", "gamma", InProgress),
                step("d", "delta", Pending),
            ],
        };
        let s = plan_summary_from_plan(plan);
        assert_eq!(s.total, 4);
        assert_eq!(s.completed, 2);
        assert_eq!(s.current_step_title.as_deref(), Some("gamma"));
    }

    #[test]
    fn plan_summary_current_step_skips_done_picks_first_non_done() {
        use crate::cockpit::state::PlanStepStatus::*;
        // First non-Done is the first Pending; InProgress later doesn't
        // override (matches the helper's `find(..)` semantics).
        let plan = crate::cockpit::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps: vec![
                step("a", "alpha", Done),
                step("b", "beta", Pending),
                step("c", "gamma", InProgress),
            ],
        };
        let s = plan_summary_from_plan(plan);
        assert_eq!(s.current_step_title.as_deref(), Some("beta"));
    }

    #[test]
    fn plan_summary_none_when_all_done() {
        use crate::cockpit::state::PlanStepStatus::*;
        let plan = crate::cockpit::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps: vec![step("a", "alpha", Done), step("b", "beta", Done)],
        };
        let s = plan_summary_from_plan(plan);
        assert_eq!(s.completed, 2);
        assert_eq!(s.total, 2);
        assert!(s.current_step_title.is_none());
    }

    #[test]
    fn plan_summary_truncates_long_current_step_title() {
        use crate::cockpit::state::PlanStepStatus::*;
        let long_title: String = "x".repeat(120);
        let plan = crate::cockpit::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps: vec![step("a", &long_title, Pending)],
        };
        let s = plan_summary_from_plan(plan);
        let t = s.current_step_title.unwrap();
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn plan_summary_empty_steps_yields_zero_total() {
        let plan = crate::cockpit::state::Plan {
            plan_id: "p1".into(),
            version: 1,
            steps: vec![],
        };
        let s = plan_summary_from_plan(plan);
        assert_eq!(s.total, 0);
        assert_eq!(s.completed, 0);
        assert!(s.current_step_title.is_none());
    }

    // --- persist_session_update (the persist-first contract from #1589) ---
    //
    // The five session-mutation PATCH handlers route every write through
    // this helper and only touch memory after it returns `Ok`, so disk and
    // memory cannot diverge on a write failure. Full-handler coverage is
    // impractical (AppState has no test constructor), so these lock the
    // helper's two guarantees directly: a success durably writes, and every
    // storage failure surfaces as `Err`.

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_session_update_writes_to_disk() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _ = isolated_app_dir(temp_home.path());

        let profile = "persist-success";
        let storage = Storage::new(profile).unwrap();
        let seed = make_test_instance();
        let id = seed.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed.clone());
                Ok(())
            })
            .unwrap();

        let persist_id = id.clone();
        persist_session_update(profile.to_string(), "test", move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.base_branch_override = Some("release/x".to_string());
            }
        })
        .await
        .expect("persist should succeed");

        let reloaded = Storage::new(profile).unwrap().load().unwrap();
        let inst = reloaded.iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            inst.base_branch_override.as_deref(),
            Some("release/x"),
            "mutation must be durable on disk"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_session_update_surfaces_storage_error() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _ = isolated_app_dir(temp_home.path());

        let profile = "persist-failure";
        // Make `sessions.json` a directory so the store's `read_to_string`
        // during `update` fails, forcing the write path to error.
        let dir = crate::session::get_profile_dir(profile).unwrap();
        std::fs::create_dir_all(dir.join("sessions.json")).unwrap();

        let result = persist_session_update(profile.to_string(), "test", |_instances| {}).await;
        assert!(result.is_err(), "a storage failure must surface as Err");
    }

    // Group edit (#1726): the persisted instance's group_path is the only
    // thing that changes; the groups Vec is left alone (the group list is
    // derived from instance group_path, exactly like create_session). Set
    // and clear both round-trip to disk.
    #[tokio::test]
    #[serial_test::serial]
    async fn group_edit_set_and_clear_round_trip_to_disk() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _ = isolated_app_dir(temp_home.path());

        let profile = "group-edit";
        let storage = Storage::new(profile).unwrap();
        let seed = make_test_instance(); // seeded in "work/projects"
        let id = seed.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed.clone());
                Ok(())
            })
            .unwrap();

        // Move to a brand-new group.
        let set_id = id.clone();
        persist_session_update(profile.to_string(), "group update", move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == set_id) {
                apply_session_group(inst, "team/alpha".to_string());
            }
        })
        .await
        .expect("set should succeed");

        let reloaded = Storage::new(profile).unwrap().load().unwrap();
        assert_eq!(
            reloaded.iter().find(|i| i.id == id).unwrap().group_path,
            "team/alpha",
            "group must move to the new path on disk"
        );

        // Clear to ungrouped via the empty-string sentinel.
        let clear_id = id.clone();
        persist_session_update(profile.to_string(), "group update", move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == clear_id) {
                apply_session_group(inst, String::new());
            }
        })
        .await
        .expect("clear should succeed");

        let reloaded = Storage::new(profile).unwrap().load().unwrap();
        assert_eq!(
            reloaded.iter().find(|i| i.id == id).unwrap().group_path,
            "",
            "empty string must clear the group on disk"
        );
    }
}

// ============================================================================
// Send + read-output endpoints
//
// Together these are the minimum primitive an external orchestrator needs to
// run an aoe session as a controlled subagent: push a prompt in, read the
// pane back. Mirrors what the TUI's send-message dialog and pane preview do,
// without requiring keyboard or websocket attach.
// ============================================================================

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    /// Whether to auto-revive a dead/stopped session before sending. Defaults
    /// to `true`; set to `false` for fail-loud behavior (parity with the
    /// `--no-revive` CLI flag).
    #[serde(default = "default_revive")]
    pub revive: bool,
}

fn default_revive() -> bool {
    true
}

enum SendKeysError {
    NotRunning,
    Transient(Status),
    CockpitMode,
    Tmux(anyhow::Error),
}

type SendKeysResult =
    Result<(EnsureReadyOutcome, Instance), Box<(Instance, EnsureReadyOutcome, SendKeysError)>>;

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "read_only"})),
        )
            .into_response();
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message_empty"})),
        )
            .into_response();
    }

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    // Serialize concurrent sends (and other tmux mutations) for this id.
    // Without this, two POSTs racing against the same session would issue
    // overlapping `tmux send-keys -l` invocations and the bytes can interleave
    // inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let tool = instance.tool.clone();
    let message = req.message;
    let revive = req.revive;
    let send_result = tokio::task::spawn_blocking(move || -> SendKeysResult {
        // Revive the pane before sending. Without this, a send to a dead
        // pane silently writes keystrokes to a corpse with no agent.
        // Skipped when the caller opts out via `revive: false`.
        //
        // The closure surfaces both `inst_owned` AND the
        // `EnsureReadyOutcome` on the Err arm so the caller can sync
        // the post-cascade `agent_session_id` (None after Tier-1
        // cleanup, or the fresh UUID acquired by Tier-2) and the
        // updated `retroactive_capture_excludes` back to live state
        // regardless of which post-cascade failure path fires. The
        // outcome lets the caller distinguish cascade-fired
        // (`Respawned`/`Started`) from the no-op `AlreadyAlive` path
        // so a sync only happens when there's actual cascade state to
        // propagate; this avoids clobbering live `last_error` on the
        // `revive=false + NotRunning` path where `started` is
        // unmutated.
        let mut inst_owned = instance;
        let outcome = if revive {
            match inst_owned.ensure_pane_ready() {
                Ok(o) => o,
                Err(e) => {
                    let mapped = match e {
                        EnsureReadyError::Transient(s) => SendKeysError::Transient(s),
                        EnsureReadyError::CockpitMode => SendKeysError::CockpitMode,
                        EnsureReadyError::Tmux(e) => SendKeysError::Tmux(e),
                    };
                    // ensure_pane_ready did not mutate user-visible
                    // state via the outcome path. Tag as AlreadyAlive
                    // so the outer match's `did_work` flag stays
                    // false. `EnsureReadyError::Tmux` may be either
                    // pre-cascade (tmux_session() / start_with_size
                    // subprocess failure: `inst_owned` unmutated) or
                    // post-cascade (Tier-2 bail: mutations committed).
                    // The Tmux outer arm syncs unconditionally and
                    // covers both shapes; the others (Transient /
                    // CockpitMode) bail before any mutation.
                    return Err(Box::new((
                        inst_owned,
                        EnsureReadyOutcome::AlreadyAlive,
                        mapped,
                    )));
                }
            }
        } else {
            EnsureReadyOutcome::AlreadyAlive
        };
        let tmux_session = match inst_owned.tmux_session() {
            Ok(s) => s,
            Err(e) => return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e)))),
        };
        if !tmux_session.exists() {
            return Err(Box::new((inst_owned, outcome, SendKeysError::NotRunning)));
        }
        let delay = crate::agents::send_keys_enter_delay(&tool);
        if let Err(e) = tmux_session.send_keys_with_delay(&message, delay) {
            return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e))));
        }
        Ok((outcome, inst_owned))
    })
    .await;

    match send_result {
        Ok(Ok((outcome, started))) => {
            // ensure_pane_ready mutated `started` (status, agent_session_id,
            // last_start_time, last_error) on the clone. Sync those back to
            // the live entry so the next request sees a coherent view;
            // without this, a rapid follow-up could generate a fresh
            // `agent_session_id` and orphan the prior Claude conversation.
            // See `apply_post_restart_sync`. Also stamp last_accessed_at so
            // the activity column reflects API-driven interaction.
            let mut instances = state.instances.write().await;
            let profile = if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                if !matches!(outcome, EnsureReadyOutcome::AlreadyAlive) {
                    apply_post_restart_sync(i, &started);
                }
                i.touch_last_accessed();
                i.source_profile.clone()
            } else {
                // Session was deleted between the send and the stamp; nothing
                // left to persist.
                return (StatusCode::OK, Json(serde_json::json!({"sent": true}))).into_response();
            };
            drop(instances);
            let id_for_save = id.clone();
            let started_for_save = started.clone();
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id_for_save) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(disk_inst, &started_for_save);
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            });
            let mut body = serde_json::json!({"sent": true});
            let stale_sid = match &outcome {
                EnsureReadyOutcome::Respawned {
                    stale_sid: Some(sid),
                }
                | EnsureReadyOutcome::Started {
                    stale_sid: Some(sid),
                } => Some(sid.clone()),
                _ => None,
            };
            if let Some(sid) = stale_sid {
                body["stale_session_id"] = serde_json::Value::String(sid);
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // ensure_pane_ready did mutate state when the outcome is
            // anything other than AlreadyAlive. The cascade itself only
            // runs in `Respawned { stale_sid: Some(_) }`, but `Started`
            // and `Respawned { stale_sid: None }` also touch fields the
            // live entry needs to reflect (fresh sid from acquire,
            // last_start_time, etc.). Sync only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match send_err {
                SendKeysError::NotRunning => {
                    // External kill or remain-on-exit-off Tier-2 crash can
                    // race ensure_pane_ready's Alive decision against the
                    // tmux_session.exists() check. Propagate the
                    // post-cascade agent_session_id (fresh UUID acquired
                    // in place of the stale, or None for Tier-1 cleanup)
                    // and the updated excludes when applicable so the
                    // next call won't orphan or re-attempt resume with
                    // the bad sid; use the narrow sync helper to leave
                    // status and last_error untouched (NotRunning is
                    // recoverable; `started.status = Starting` from
                    // finalize_launch would briefly mis-paint a broken
                    // pane).
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &started);
                        }
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": "session_not_running"})),
                    )
                        .into_response()
                }
                SendKeysError::Transient(status) => (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "session_transient",
                        "status": format!("{status:?}"),
                    })),
                )
                    .into_response(),
                SendKeysError::CockpitMode => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "cockpit_mode_unsupported"})),
                )
                    .into_response(),
                SendKeysError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", "send_message: tmux error for {id}: {e}");
                    let msg = e.to_string();
                    // Sync cascade-mutated fields back to live state. Mirror
                    // `ensure_session`'s Err arm: full sync, then override
                    // `status` and `last_error` so observers don't see
                    // `Status::Starting` (set by `finalize_launch` before
                    // Tier-2 bail) on a broken session. Tmux Err is the
                    // catch-all for both pre-cascade tmux failures (where
                    // `started` is unmutated and the sync is a no-op) and
                    // post-cascade Tier-2 bails (where the sync propagates
                    // the cleared sid + updated excludes).
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &started);
                        i.status = crate::session::Status::Error;
                        i.last_error = Some(msg);
                    }
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "tmux_error"})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "send_message: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_output_lines")]
    pub lines: u32,
    #[serde(default = "default_output_format")]
    pub format: String,
}

fn default_output_lines() -> u32 {
    200
}

fn default_output_format() -> String {
    "text".to_string()
}

enum CaptureError {
    NotRunning,
    Tmux(anyhow::Error),
}

pub async fn read_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<OutputQuery>,
) -> impl IntoResponse {
    let lines = (q.lines as usize).clamp(1, 2000);
    let want_ansi = match q.format.as_str() {
        "ansi" => true,
        "text" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "format_invalid",
                    "allowed": ["text", "ansi"]
                })),
            )
                .into_response();
        }
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let capture_result = tokio::task::spawn_blocking(move || -> Result<String, CaptureError> {
        let tmux_session = instance.tmux_session().map_err(CaptureError::Tmux)?;
        if !tmux_session.exists() {
            return Err(CaptureError::NotRunning);
        }
        let raw = tmux_session
            .capture_pane(lines)
            .map_err(CaptureError::Tmux)?;
        if want_ansi {
            Ok(raw)
        } else {
            Ok(crate::tmux::utils::strip_ansi(&raw))
        }
    })
    .await;

    match capture_result {
        Ok(Ok(content)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id,
                "lines": lines,
                "format": q.format,
                "content": content,
            })),
        )
            .into_response(),
        Ok(Err(CaptureError::NotRunning)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session_not_running"})),
        )
            .into_response(),
        Ok(Err(CaptureError::Tmux(e))) => {
            tracing::error!(target: "http.api.sessions", "read_output: tmux error for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "tmux_error"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "read_output: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod workspace_ordering_tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup_test_home(temp: &std::path::Path) {
        std::env::set_var("HOME", temp);
        #[cfg(target_os = "linux")]
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"));
    }

    fn mock_response(id: &str, project_path: &str, branch: Option<&str>) -> SessionResponse {
        SessionResponse {
            id: id.to_string(),
            title: id.to_string(),
            project_path: project_path.to_string(),
            group_path: String::new(),
            tool: "claude".to_string(),
            status: "Idle".to_string(),
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
            has_terminal: false,
            profile: "default".to_string(),
            cleanup_defaults: CleanupDefaults {
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
            },
            remote_owner: None,
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            #[cfg(feature = "serve")]
            cockpit_mode: false,
            #[cfg(feature = "serve")]
            cockpit_worker_state: crate::cockpit::supervisor::CockpitWorkerState::Absent,
            #[cfg(feature = "serve")]
            acp_capable: false,
            claude_fullscreen: false,
            workspace_repos: Vec::new(),
            warnings: Vec::new(),
            plan_summary: None,
            next_wakeup_at: None,
            next_wakeup_reason: None,
            favorited: false,
            urgent: false,
            pinned_at: None,
            archived_at: None,
            snoozed_until: None,
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
        setup_test_home(temp.path());

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
        setup_test_home(temp.path());

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
        setup_test_home(temp.path());

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
        setup_test_home(temp.path());

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

#[cfg(test)]
mod send_output_tests {
    use super::*;

    #[test]
    fn output_query_default_constants() {
        assert_eq!(default_output_lines(), 200);
        assert_eq!(default_output_format(), "text");
    }

    #[test]
    fn send_message_request_requires_message_field() {
        let r: Result<SendMessageRequest, _> = serde_json::from_str("{}");
        assert!(r.is_err(), "missing message must reject");
    }

    #[test]
    fn send_message_request_accepts_message() {
        let r: SendMessageRequest = serde_json::from_str("{\"message\":\"hello\"}").unwrap();
        assert_eq!(r.message, "hello");
    }
}
