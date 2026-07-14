//! Session CRUD, ensure-* lifecycle endpoints, and per-file diff handlers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::git::error::GitError;
use crate::session::config::SessionConfig;
use crate::session::{ClaimOp, EnsureReadyError, EnsureReadyOutcome, Instance, Status, Storage};

use super::validate_display_label;
use super::validate_no_shell_injection;
use super::AppState;

#[derive(Serialize)]
pub struct SessionResponse {
    pub id: String,
    pub title: String,
    pub project_path: String,
    /// Absolute host path of the session's managed artifact directory. The
    /// web transcript maps agent-emitted artifact paths under this root (or
    /// the fixed sandbox mount) to the authenticated artifact route. See #2587.
    pub artifact_dir: String,
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
    /// `attention-urgent` hook (read from `/tmp/aoe-hooks-<euid>/{id}/attention.json`
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
    /// RFC3339 timestamp at which the session was moved to trash, or
    /// omitted when not trashed. Trashed rows are excluded from the
    /// default session list; the web client requests them with
    /// `?state=trashed` and renders a dedicated Trash section with restore
    /// and permanent-delete actions. See #2489.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<String>,
    /// Unread marker, mirroring `Instance::unread`: `true` when the session
    /// needs attention (a finished turn the user hasn't engaged with, or a
    /// manual flag), omitted when read. The web sidebar paints an unread
    /// accent and offers a right-click "Mark as read/unread" toggle; gated
    /// client-side on the `session.unread_indicator` setting. See the TUI's
    /// `theme.unread`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
    /// Strictly a single-repo aoe-managed worktree (`worktree_info`). Drives
    /// the sidebar "Edit workdir name" action and the tie-workdir overlay,
    /// neither of which applies to multi-repo workspace sessions. For
    /// "is there worktree state to clean up on delete", use
    /// `has_cleanable_worktree` instead.
    pub has_managed_worktree: bool,
    /// Whether deleting this session has aoe-managed worktree state to remove,
    /// covering single-repo worktrees AND multi-repo workspaces. Only the
    /// delete dialog's worktree/branch checkboxes consume this; keeping it
    /// separate from `has_managed_worktree` avoids lighting up worktree-only
    /// actions (Edit workdir) for workspace sessions (#2363).
    pub has_cleanable_worktree: bool,
    /// Whether renaming this session also moves its worktree directory (the
    /// resolved `session.tie_workdir_to_name` for an aoe-managed worktree).
    /// Populated by `list_sessions` from the per-profile config; single-session
    /// responses leave it `false` and the sidebar reads the list value. #1927.
    #[serde(default)]
    pub tie_workdir_to_name: bool,
    /// Smart-rename indicator state for structured view sessions: `pending`
    /// (still default-named and eligible, will auto-name on the next prompt),
    /// `running` (a one-shot title call is in flight), or `inactive`. Populated
    /// by `list_sessions`; single-session responses leave it `inactive`. See
    /// `session::smart_rename`.
    #[serde(default)]
    pub smart_rename: crate::session::smart_rename::SmartRenameState,
    /// Whether the session still carries its auto-generated civilization name.
    /// The sidebar gates the manual "Auto-name now" action on this (it only
    /// targets a still-default session, never overwriting a chosen title), and
    /// it is a more reliable signal than `smart_rename`: a timed-out one-shot
    /// stays `pending` while an unusable-output one goes `inactive`, but both
    /// leave the name default and recoverable. Populated by `list_sessions`;
    /// single-session responses leave it `false`.
    #[serde(default)]
    pub default_name: bool,
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
    /// How this session is rendered: `structured` (ACP native rendering) or
    /// `terminal` (tmux-backed PTY). The web dashboard branches on this to
    /// pick the structured panels vs the terminal view.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "crate::session::View::is_terminal")]
    pub view: crate::session::View,
    /// Live structured view worker lifecycle. `absent` for tmux sessions or
    /// structured view sessions whose worker has not been spawned/attached
    /// yet; `resuming` while the reconciler is mid-spawn or mid-attach;
    /// `running` once the supervisor holds a live worker. Drives the
    /// sidebar `Resuming…` chip and the per-session banner in the
    /// structured view. See #1088.
    #[cfg(feature = "serve")]
    pub acp_worker_state: crate::acp::supervisor::AcpWorkerState,
    /// True when this session's agent can run in structured view: a built-in
    /// with an ACP adapter, or a custom agent whose profile config
    /// declares a valid `agent_acp_cmd`. The web terminal view reads
    /// this to decide whether the "switch to structured view" affordance is
    /// available, replacing the hardcoded client-side tool list.
    #[cfg(feature = "serve")]
    pub acp_capable: bool,
    /// The session's captured ACP session id, present only once the
    /// structured-view worker has minted one. The web dashboard passes this
    /// as `fork_from` on a structured fork create, so the sidebar only offers
    /// "Fork" on a structured row that has a captured id to diverge from.
    /// Omitted when absent (terminal sessions, or structured ones whose worker
    /// has not minted an id yet).
    #[cfg(feature = "serve")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    /// The session's resolved ACP registry key (`agent_name` when set, else
    /// `tool`), matching the `name` entries `/api/acp/agents` returns. The
    /// structured view's switch-agent modal reads this as the current-agent
    /// fallback before the first `AgentSwitched` event lands (which is the
    /// only event that populates the reduced `state.agent`), so it can gray
    /// out the running backend on a never-switched session. Omitted for
    /// sessions with no resolved agent. See #2803.
    #[cfg(feature = "serve")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    /// True when this session's agent can run a structured ACP `session/fork`:
    /// it is ACP-capable AND declares a real fork strategy. Resume-only ACP
    /// agents (e.g. the bundled `aoe-agent`, which advertises `loadSession` but
    /// not `session/fork`) are ACP-capable yet not forkable, so gating the web
    /// "Fork" action on `acp_session_id` alone would offer a dead-end button
    /// that fails at the `session/fork` handshake. The true capability is only
    /// advertised transiently during the handshake, so this projects the static
    /// agent fork strategy instead, which is the set AoE treats as forkable.
    /// Omitted (read as not-forkable) for terminal sessions and non-forkable
    /// agents.
    #[cfg(feature = "serve")]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub acp_can_fork: bool,
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
    /// structured view sessions whose agent has emitted a Plan (directly via
    /// ACP `SessionUpdate::Plan` or indirectly via the ExitPlanMode
    /// bridge in `acp_client::map_update_to_events`). See #1061.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_summary: Option<PlanSummary>,
    /// Absolute RFC3339 timestamp at which the structured view session's
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
    /// True when the structured view session has an armed `Monitor` tool
    /// (a background watch). Unlike a scheduled wakeup there is no fire
    /// time, so the sidebar shows a static "monitoring" badge rather than a
    /// countdown. Cleared once a `UserPromptSent` lands after the monitor
    /// was armed (the user took over).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub monitor_active: bool,
    /// The `description` the agent gave the `Monitor` tool, shown as the
    /// badge tooltip. Only set when `monitor_active` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_description: Option<String>,
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
    /// Resolved `session.delete_to_trash`: when true, the web delete dialog
    /// defaults to "Move to Trash" with a permanent-delete disclosure;
    /// when false it goes straight to permanent delete. See #2489.
    pub delete_to_trash: bool,
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
            crate::acp::supervisor::AcpWorkerState::Absent,
            None,
            None,
            None,
        )
    }

    /// Build a response with the per-session plan snapshot. Called from
    /// the REST sessions endpoint after a single bulk read of the
    /// structured view event store; see #1061.
    pub fn from_instance_with_plan(
        inst: &Instance,
        claude_fullscreen: bool,
        plan_summary: Option<PlanSummary>,
        #[cfg(feature = "serve")] acp_worker_state: crate::acp::supervisor::AcpWorkerState,
        next_wakeup_at: Option<String>,
        next_wakeup_reason: Option<String>,
        // `Some(description)` when the session has an armed `Monitor` (the
        // inner description is itself optional); `None` when none is armed.
        // Mirrors `EventStore::latest_active_monitor`'s return so the caller
        // forwards it verbatim.
        active_monitor: Option<Option<String>>,
    ) -> Self {
        let (monitor_active, monitor_description) = match active_monitor {
            Some(description) => (true, description),
            None => (false, None),
        };
        Self {
            id: inst.id.clone(),
            title: inst.title.clone(),
            project_path: inst.project_path.clone(),
            artifact_dir: crate::session::artifacts::artifact_dir_path(&inst.id)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
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
            trashed_at: inst.trashed_at.map(|t| t.to_rfc3339()),
            // Surface the marker (omitted when read); the web gates the
            // visual on the `session.unread_indicator` setting.
            unread: inst.unread,
            has_managed_worktree: inst
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe),
            has_cleanable_worktree: inst.has_managed_worktree_or_workspace(),
            // Overlaid per-profile in list_sessions; see the field doc.
            tie_workdir_to_name: false,
            // Overlaid in list_sessions; single-session responses stay inactive.
            smart_rename: crate::session::smart_rename::SmartRenameState::Inactive,
            // Overlaid in list_sessions; single-session responses stay false.
            default_name: false,
            has_terminal: inst.terminal_info.is_some(),
            profile: inst.source_profile.clone(),
            cleanup_defaults: CleanupDefaults {
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
                delete_to_trash: true,
            },
            remote_owner: None,
            notify_on_waiting: inst.notify_on_waiting,
            notify_on_idle: inst.notify_on_idle,
            notify_on_error: inst.notify_on_error,
            #[cfg(feature = "serve")]
            view: inst.view,
            #[cfg(feature = "serve")]
            acp_worker_state,
            // Built-in ACP capability is resolved here from a process-wide
            // registry (cheap, no IO). Custom agents depend on profile
            // config; the list and create handlers overlay that without a
            // per-row config read.
            #[cfg(feature = "serve")]
            acp_capable: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                builtin_acp_registry().get(resolved).is_some()
            },
            #[cfg(feature = "serve")]
            acp_session_id: inst.acp_session_id.clone(),
            // Resolved the same way as `acp_capable` above: `agent_name` when
            // set and non-empty, else `tool`. This is the ACP registry key,
            // so it matches `/api/acp/agents` names the switch-agent modal
            // filters against. See #2803.
            #[cfg(feature = "serve")]
            acp_agent: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                (!resolved.is_empty()).then(|| resolved.to_string())
            },
            // Shares `agent_is_structured_fork_capable` with the create-time
            // guard so the web "Fork" affordance and server-side acceptance
            // cannot drift: forkable = ACP-capable AND a real fork strategy.
            #[cfg(feature = "serve")]
            acp_can_fork: agent_is_structured_fork_capable(&inst.tool, inst.agent_name.as_deref()),
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
            monitor_active,
            monitor_description,
        }
    }
}

/// Project a stored `Plan` into the lightweight `PlanSummary` shape the
/// sidebar consumes. Current step is the first non-Done entry; counts
/// reflect the persisted step state from the agent's last PlanUpdated.
fn plan_summary_from_plan(plan: crate::acp::state::Plan) -> PlanSummary {
    use crate::acp::state::PlanStepStatus;
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
fn builtin_acp_registry() -> &'static crate::acp::AgentRegistry {
    static REG: std::sync::OnceLock<crate::acp::AgentRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(crate::acp::AgentRegistry::with_defaults)
}

/// True iff this custom agent declares a valid `agent_acp_cmd` in the
/// given profile-resolved map. Built-in capability is handled separately
/// in the constructor, so this only covers the custom case.
#[cfg(feature = "serve")]
fn custom_agent_acp_capable(agent_acp_cmd: &HashMap<String, String>, tool: &str) -> bool {
    agent_acp_cmd
        .get(tool)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
}

/// Resolve the [`SessionConfig`] for `(profile, project_path)` through the
/// caller-owned per-request cache, resolving from disk on first miss only.
/// See the `session_cfg_cache` declaration in `list_sessions` for the
/// sharing rationale. See #2603.
fn resolve_session_cfg<'a>(
    cache: &'a mut HashMap<(String, String), SessionConfig>,
    profile: &str,
    project_path: &str,
) -> &'a SessionConfig {
    cache
        .entry((profile.to_string(), project_path.to_string()))
        .or_insert_with(|| {
            #[cfg(test)]
            LIST_SESSIONS_RESOLVER_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::session::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(project_path),
            )
            .session
        })
}

/// Test seam for the shared per-request cache invariant (#2603): bumped
/// exactly once per unique `(profile, project_path)` that resolves through
/// [`resolve_session_cfg`]. Mirrors the module-static test seam pattern used
/// by [`crate::session::FAIL_NEXT_LIST_PROFILES`]. Readers must hold
/// `#[serial_test::serial]`: a concurrent `list_sessions` call between reset
/// and load would leak bumps into the assertion.
#[cfg(test)]
pub(crate) static LIST_SESSIONS_RESOLVER_MISSES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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

pub async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<SessionsEnvelope> {
    let instances = state.instances.read().await;
    let claude_fullscreen = crate::claude_settings::read_tui_fullscreen();
    // Snapshot the supervisor's worker lifecycle map once per request
    // rather than locking it per row. See #1088.
    #[cfg(feature = "serve")]
    let worker_states = state.acp_supervisor.worker_states_snapshot().await;
    let mut sessions: Vec<SessionResponse> = instances
        .iter()
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
            #[cfg(feature = "serve")]
            let acp_worker_state = worker_states
                .get(&inst.id)
                .copied()
                .unwrap_or(crate::acp::supervisor::AcpWorkerState::Absent);
            SessionResponse::from_instance_with_plan(
                inst,
                claude_fullscreen,
                plan_summary,
                #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
    {
        for (resp, inst) in sessions.iter_mut().zip(instances.iter()) {
            if resp.acp_capable {
                continue;
            }
            let cfg = resolve_session_cfg(
                &mut session_cfg_cache,
                &inst.source_profile,
                &inst.project_path,
            );
            resp.acp_capable = custom_agent_acp_capable(&cfg.agent_acp_cmd, &inst.tool);
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
                crate::session::profile_config::resolve_config_or_warn(&session.profile)
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
        for (resp, inst) in sessions.iter_mut().zip(instances.iter()) {
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
        return super::read_only_response();
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
    /// When the session is tied (`session.tie_workdir_to_name`) and an
    /// aoe-managed worktree, also rename the underlying git branch to match
    /// the new title. Off by default; ignored for untied / non-worktree
    /// sessions. See #1927.
    #[serde(default)]
    pub rename_branch: bool,
}

fn apply_session_title_rename(inst: &mut Instance, title: String) {
    inst.title = title;
}

/// Quiesce a structured-view worker before its worktree directory is moved.
/// A live ACP worker is pinned to the current cwd; `git worktree move` pulls
/// that directory out, the worker crashes, and the supervisor respawns it at
/// the stale baked-in cwd, crash-looping until the reconciler parks the
/// session with a misleading install-the-adapter banner (#2260). The
/// blocks_worktree_edit gate does not catch this because a structured session
/// the user "stopped" sits at Idle yet still owns a live worker.
///
/// `shutdown` is the reversible teardown: it keeps the agent transcript and the
/// instance's acp_session_id, so once the move lands the reconciler fresh-spawns
/// at the new path and resumes context via session/load. Callers hold the
/// session's instance_lock across shutdown plus move plus persist, and the
/// reconciler re-reads project_path under that same lock, so the post-move
/// respawn never targets the old path. No-op for a session with no live worker;
/// refuses the move (409) if a live worker cannot be stopped, so the directory
/// is never moved out from under one.
async fn quiesce_structured_worker_for_worktree_move(
    state: &Arc<AppState>,
    id: &str,
    is_structured: bool,
) -> Result<(), axum::response::Response> {
    if !is_structured {
        return Ok(());
    }
    match state.acp_supervisor.shutdown(id).await {
        Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => Ok(()),
        Err(e) => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "could not stop structured-view worker before worktree move: {e}"
            );
            Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "worker_shutdown_failed",
                    "message": "Could not stop the structured view worker before renaming; retry in a moment"
                })),
            )
                .into_response())
        }
    }
}

/// Probe whether a sandboxed session's container is still holding its
/// worktree mount, on the blocking pool.
///
/// A sandbox container runs `sleep infinity` for the life of the session
/// and keeps the worktree dir bind-mounted even while the agent is Idle,
/// so a `git worktree move` would fail with `EBUSY`. Callers gate the
/// rename/workdir-edit endpoints on this probe.
///
/// Fails closed at the async boundary: a `spawn_blocking` panic or
/// cancellation reports the worktree as held (with a `warn!` log), so
/// the caller rejects the mutating request with `409 CONFLICT` rather
/// than risk `EBUSY` against a possibly-live container mount. Sharing
/// this helper between `rename_session` and `set_worktree_name` keeps
/// the fail-closed policy synchronized across the two endpoints (#2596).
async fn probe_container_holds_worktree(id: &str, is_sandboxed: bool) -> bool {
    let probe_id = id.to_string();
    let log_id = id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::worktree_edit::sandbox_container_holds_worktree(&probe_id, is_sandboxed)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(
            target: "server.api.sessions",
            session = %log_id,
            error = %e,
            "sandbox container probe task failed at the async boundary; failing closed and reporting the worktree as held to prevent EBUSY against a possibly-live container"
        );
        true
    })
}

/// Rename a session's title (and, when tied, its worktree directory).
///
/// The sandbox container probe runs on the blocking pool via
/// [`probe_container_holds_worktree`], which fails closed on a
/// `spawn_blocking` panic or cancellation so the rename is rejected
/// with `409 CONFLICT` rather than proceeding against a possibly-live
/// container mount and hitting `EBUSY`.
pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
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
    if let Err(msg) = validate_display_label(&title, "title") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": msg })),
        )
            .into_response();
    }

    // Serialize against other mutations on this session (start, delete,
    // worktree edit) so the tied git move and the metadata write don't race.
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (worktree_info, current_path, status, profile, is_sandboxed, is_structured) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        (
            inst.worktree_info.clone(),
            inst.project_path.clone(),
            inst.status,
            inst.source_profile.clone(),
            inst.is_sandboxed(),
            inst.is_structured(),
        )
    };

    // Tied mode (#1927): renaming an aoe-managed worktree session also moves
    // its directory leaf to match the title, so title and dir cannot drift.
    let tied = crate::session::profile_config::resolve_config_or_warn(&profile)
        .session
        .tie_workdir_to_name
        && worktree_info.as_ref().is_some_and(|w| w.managed_by_aoe);

    // What to write to disk + memory once any git side effect has landed.
    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;

    if tied {
        // The dir move is gated on a quiescent worktree, exactly like the
        // standalone worktree-name edit. A running session must be stopped
        // first; the setting is the escape hatch for free-form relabeling.
        // A sandbox session's container keeps the worktree dir mounted even
        // while the agent is Idle, so the move would fail with EBUSY; stopping
        // the session tears the container down and releases the mount.
        let container_holds = probe_container_holds_worktree(&id, is_sandboxed).await;
        if status.blocks_worktree_edit() || container_holds {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session_running",
                    "message": "Stop the session before renaming it: its worktree directory moves to match the new name. Disable \"Tie Worktree Directory to Session Name\" to relabel a running session."
                })),
            )
                .into_response();
        }

        // Stop any live structured-view worker before the move so it can't
        // crash on the pulled-out cwd and respawn-loop at the stale path
        // (#2260). Done under the instance_lock held since the top of this
        // function. Preserves the agent transcript so the reconciler resumes
        // context at the new path.
        if let Err(resp) =
            quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
        {
            return resp;
        }

        let wt = worktree_info.expect("tied implies worktree_info is Some");
        let cur = current_path.clone();
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&title);
        let rename_branch = body.rename_branch;
        let edit = tokio::task::spawn_blocking(move || {
            crate::session::worktree_edit::edit_worktree_workdir(
                crate::session::worktree_edit::WorktreeEditRequest {
                    worktree_info: &wt,
                    current_path: std::path::Path::new(&cur),
                    new_name: &leaf,
                    rename_branch,
                },
            )
            .map(|o| (o.new_path.to_string_lossy().to_string(), o.new_branch))
        })
        .await;

        match edit {
            Ok(Ok((path, branch))) => {
                // The dir moved (path changed): a sandbox container created
                // against the old path is now stale, so drop it to force a
                // fresh create on next start. A branch-only edit leaves the
                // path (and the mount) unchanged, so skip it then. Awaited so
                // the response only lands once the stale container is gone; an
                // immediate restart must not race the removal and revive it.
                if path != current_path {
                    let id = id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::session::worktree_edit::discard_sandbox_container_after_move(
                            &id,
                            is_sandboxed,
                        )
                    })
                    .await;
                }
                new_path = Some(path);
                new_branch = branch;
            }
            // The title slug maps to the current leaf and no branch rename was
            // requested: nothing to move, fall through to a plain title rename.
            Ok(Err(crate::session::worktree_edit::WorktreeEditError::Unchanged)) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tied rename worktree edit failed: {e}");
                let (code, msg) = worktree_edit_error_response(&e);
                return (code, Json(serde_json::json!({ "message": msg }))).into_response();
            }
            Err(e) => {
                tracing::error!(target: "http.api.sessions", "tied rename worktree edit join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "message": "Worktree edit task failed" })),
                )
                    .into_response();
            }
        }
    }

    // Persist BEFORE mutating in-memory state: when a git move has landed, a
    // silent persist failure would otherwise leave metadata pointing at the
    // old path after a daemon restart, so it returns 500 rather than a
    // misleading 200.
    let persisted = {
        let storage = Storage::new(&profile, state.file_watch.clone());
        let title_clone = title.clone();
        let id_clone = id.clone();
        let new_path_clone = new_path.clone();
        let new_branch_clone = new_branch.clone();
        match storage {
            Ok(storage) => tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                        if let Some(path) = new_path_clone.as_deref() {
                            apply_worktree_name_edit(inst, path, new_branch_clone.as_deref());
                        }
                        apply_session_title_rename(inst, title_clone);
                    }
                    Ok(())
                })
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string())),
            Err(e) => Err(e.to_string()),
        }
    };
    if let Err(e) = persisted {
        tracing::error!(target: "http.api.sessions", session = %id, "Failed to save after rename: {e}");
        // Persist-first: never fall through to mutate in-memory state on a
        // failed write, or the rename silently reverts on restart. When a dir
        // move already landed, say so; otherwise it is a plain title persist.
        let message = if new_path.is_some() {
            "Worktree was moved on disk, but persisting the new session metadata failed"
        } else {
            "Persisting the renamed session failed"
        };
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "persist_failed", "message": message })),
        )
            .into_response();
    }

    let mut response = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        if let Some(path) = new_path.as_deref() {
            apply_worktree_name_edit(inst, path, new_branch.as_deref());
        }
        apply_session_title_rename(inst, title.clone());
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen())
    };
    // Single-session responses are not run through list_sessions' overlay, so
    // carry the resolved tie value here too (#1927); otherwise a client that
    // trusts the mutation response would see a managed worktree claim it is
    // untied until the next list refresh.
    response.tie_workdir_to_name = tied;

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

/// Edit a managed worktree session's workdir directory name (and optionally
/// its git branch).
///
/// The sandbox container probe runs on the blocking pool via
/// [`probe_container_holds_worktree`], which fails closed on a
/// `spawn_blocking` panic or cancellation so the edit is rejected with
/// `409 CONFLICT` rather than proceeding against a possibly-live container
/// mount and hitting `EBUSY`.
pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
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
    // #2624: no shell-injection check here. `name` becomes a git branch and
    // filesystem leaf via `edit_worktree_workdir`, which already runs it
    // through `git_sanitize_branch_name` + `sanitize_branch_name` before
    // either ever sees a raw byte (src/session/worktree_edit.rs).

    // Serialize against other mutations on this session (start, delete,
    // another rename) so the git ops and the metadata write don't race.
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (worktree_info, current_path, status, profile, is_sandboxed, is_structured) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        (
            inst.worktree_info.clone(),
            inst.project_path.clone(),
            inst.status,
            inst.source_profile.clone(),
            inst.is_sandboxed(),
            inst.is_structured(),
        )
    };

    let Some(worktree_info) = worktree_info else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Session does not use a worktree" })),
        )
            .into_response();
    };
    // When tied (#1927), the directory is not edited independently: it follows
    // the title. Reject the standalone edit so no client can drift the two
    // apart, pointing callers at the unified rename.
    if worktree_info.managed_by_aoe
        && crate::session::profile_config::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "tied",
                "message": "Renaming is unified while \"Tie Worktree Directory to Session Name\" is on; rename the session instead, and its directory follows."
            })),
        )
            .into_response();
    }
    // A sandbox container keeps the worktree dir mounted even while the agent
    // is Idle, so the move would fail with EBUSY; stopping the session releases
    // the mount, same as the active-status case.
    let container_holds = probe_container_holds_worktree(&id, is_sandboxed).await;
    if status.blocks_worktree_edit() || container_holds {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "message": "Cannot edit the workdir name while the session is active; stop it first"
            })),
        )
            .into_response();
    }

    // Stop any live structured-view worker before the move so it can't crash on
    // the pulled-out cwd and respawn-loop at the stale path (#2260). Held under
    // the instance_lock acquired at the top of this function.
    if let Err(resp) = quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
    {
        return resp;
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

    // The dir moved (path changed): a sandbox container created against the old
    // path is now stale, so drop it to force a fresh create on next start. A
    // branch-only edit leaves the path (and the mount) unchanged. Awaited so
    // the response only lands once the stale container is gone; an immediate
    // restart must not race the removal and revive it.
    if new_path != current_path {
        let id_for_discard = id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::session::worktree_edit::discard_sandbox_container_after_move(
                &id_for_discard,
                is_sandboxed,
            )
        })
        .await;
    }

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

    let storage = match Storage::new(&profile, state.file_watch.clone()) {
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
            return super::session_not_found();
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
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let group = body.group;
    // Match `create_session`'s group handling exactly: display-label
    // check on a non-empty path, no trimming or slash normalization. The
    // empty string is the ungroup sentinel and skips validation.
    if !group.is_empty() {
        if let Err(msg) = validate_display_label(&group, "group") {
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
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_group = group.clone();
    if persist_session_update(
        profile,
        "group update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_session_group(inst, persist_group);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "group update: instance vanished after persist"
        );
        return super::session_gone_after_persist();
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
pub(crate) async fn persist_session_update<F>(
    profile: String,
    label: &'static str,
    file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    mutate: F,
) -> Result<(), ()>
where
    F: FnOnce(&mut Vec<Instance>) + Send + 'static,
{
    let storage = match Storage::new(&profile, file_watch) {
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

/// Run `f` on the disk instances under the storage flock (off the async
/// runtime) and return its result. The single serialization point shared by
/// every server claim/commit; the claim/commit decision itself lives in the
/// neutral `session::claim` helpers so the CLI, daemon, and TUI agree.
///
/// Claims are written to disk ONLY: the in-memory `state.instances.op_claim` is
/// eventually-consistent (the disk watcher reloads it) and never authoritative,
/// so no claim decision ever reads it. See #2541.
async fn with_locked_storage<T, F>(state: &Arc<AppState>, profile: &str, f: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce(&mut Vec<Instance>) -> T + Send + 'static,
{
    let profile = profile.to_string();
    let file_watch = state.file_watch.clone();
    match tokio::task::spawn_blocking(move || {
        let storage = Storage::new(&profile, file_watch).map_err(|e| e.to_string())?;
        storage
            .update(|instances, _groups| Ok(f(instances)))
            .map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "locked storage op failed: {e}");
            Err(())
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "locked storage op join failed: {e}");
            Err(())
        }
    }
}

/// Release the `op` claim for session `id` on disk and in memory,
/// ownership-guarded so a peer's fresh claim (stale-override) is never cleared.
/// Best-effort: a stranded claim self-heals via the TTL. See #2541.
async fn release_op_claim(state: &Arc<AppState>, profile: &str, id: &str, op: ClaimOp) {
    let _ = persist_session_update(
        profile.to_string(),
        "op-claim-release",
        state.file_watch.clone(),
        {
            let id = id.to_string();
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    inst.clear_op_claim_if_owned(op);
                }
            }
        },
    )
    .await;
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.clear_op_claim_if_owned(op);
    }
}

pub async fn update_session_notifications(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateNotificationsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
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
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    let waiting = body.notify_on_waiting;
    let idle = body.notify_on_idle;
    let error = body.notify_on_error;

    // Persist first; only mutate memory once disk is durable so a write
    // failure leaves the two in agreement. See #1589.
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "notification update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply(&mut inst.notify_on_waiting, waiting);
                apply(&mut inst.notify_on_idle, idle);
                apply(&mut inst.notify_on_error, error);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "notification update: instance vanished after persist"
        );
        return super::session_gone_after_persist();
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
        return super::read_only_response();
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
            return super::session_not_found();
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
    if persist_session_update(
        profile,
        "diff-base update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.base_branch_override = persist_override;
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "diff-base update: instance vanished after persist"
        );
        return super::session_gone_after_persist();
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
// structured view sessions) the supervisor's worker so the row is genuinely
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
    /// On archive, tear down every tmux session this instance owns. `false`
    /// keeps tmux state alive; structured-view supervisor shutdown is
    /// unconditional. Ignored when `archived = false`. See #1868.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

fn default_kill_pane() -> bool {
    true
}

#[derive(Deserialize)]
pub struct TrashSessionBody {
    /// On trash, tear down every tmux session this instance owns. `false`
    /// keeps tmux state alive; structured-view supervisor shutdown (which
    /// preserves the transcript) is unconditional. Defaults to `true`.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

// A no-body trash request resolves through `unwrap_or_default()`, so `Default`
// must match the serde field default (`true`). The derived `Default` would use
// `bool::default()` (`false`) and silently leave the pane running (#2523).
impl Default for TrashSessionBody {
    fn default() -> Self {
        Self {
            kill_pane: default_kill_pane(),
        }
    }
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

#[derive(Deserialize)]
pub struct UpdateUnreadBody {
    /// `true` flags the session manually unread (a deliberate "flag for
    /// later"); `false` marks it read, clearing both auto and manual markers.
    /// The clear is the explicit one (web "Mark as read"); the auto-clear on
    /// view is driven separately by the client, which only fires it for an
    /// `auto` marker, so a `false` here never silently drops a manual flag the
    /// user meant to keep.
    pub unread: bool,
}

pub async fn update_session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePinBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
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
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    let pinned = body.pinned;

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "pin update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                if pinned {
                    inst.pin();
                } else {
                    inst.unpin();
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "pin update: instance vanished after persist"
        );
        return super::session_gone_after_persist();
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
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Read the profile without mutating memory yet. Persisting first means
    // a storage failure returns 500 with disk and memory still in
    // agreement, and the tmux/acp teardown below never fires on a write
    // that did not land. See #1589.
    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    let archived = body.archived;
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "archive update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                if archived {
                    inst.archive();
                } else {
                    inst.unarchive();
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    // Disk is durable; apply to memory and snapshot what the side effects
    // need. Clone the instance once so we can call its `kill()` method
    // outside the lock without re-borrowing.
    let (was_structured_view, inst_clone, kill_pane) = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "archive update: instance vanished after persist"
            );
            return super::session_gone_after_persist();
        };
        if archived {
            inst.archive();
        } else {
            inst.unarchive();
        }
        let response =
            SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
        let structured_view;
        #[cfg(feature = "serve")]
        {
            structured_view = inst.is_structured();
        }
        #[cfg(not(feature = "serve"))]
        {
            structured_view = false;
        }
        let inst_snap = inst.clone();
        drop(instances);

        // Snapshot and drop the lock; run side effects below. Unarchive
        // returns here; archive does NOT short-circuit on kill_pane=false
        // because structured-view shutdown is unconditional.
        if !archived {
            return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
        }
        (structured_view, inst_snap, body.kill_pane)
    };

    // Best-effort tmux teardown (helper logs at debug). #1868.
    if was_structured_view {
        // Worker shutdown before ancillary kill so in-flight tool output
        // settles (mirrors acp.rs:1304-1310). shutdown() preserves the
        // transcript (#1710).
        #[cfg(feature = "serve")]
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during archive failed: {e}"
            ),
        }
        if kill_pane {
            let inst_for_kill = inst_clone.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || inst_for_kill.kill_ancillary_tmux_sessions())
                    .await
            {
                tracing::warn!(
                    target: "http.api.sessions",
                    "Archive: ancillary tmux kill join failed: {e}"
                );
            }
        }
    } else if kill_pane {
        let inst_for_kill = inst_clone.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || inst_for_kill.kill_all_tmux_sessions()).await
        {
            tracing::warn!(
                target: "http.api.sessions",
                "Archive: tmux kill join failed: {e}"
            );
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
            return super::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/trash`. Soft-delete a session into the trash
/// bucket: persist `trashed_at`, then stop the live session the same way
/// archive does (structured-view supervisor `shutdown`, which PRESERVES the
/// transcript, plus optional tmux teardown). Durable artifacts (transcript,
/// worktree, branch, container) are kept so `restore` is faithful; permanent
/// teardown happens only on purge (`DELETE`). See #2489.
pub async fn trash_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<TrashSessionBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "trash",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.trash();
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    // Disk durable; apply to memory and snapshot what teardown needs.
    let (was_structured_view, inst_clone) = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "trash: instance vanished after persist"
            );
            return super::session_gone_after_persist();
        };
        inst.trash();
        let structured_view;
        #[cfg(feature = "serve")]
        {
            structured_view = inst.is_structured();
        }
        #[cfg(not(feature = "serve"))]
        {
            structured_view = false;
        }
        (structured_view, inst.clone())
    };

    // Stop the live session (mirror archive teardown). shutdown() preserves
    // the transcript (#1710); purge is the only path that deletes it.
    if was_structured_view {
        #[cfg(feature = "serve")]
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during trash failed: {e}"
            ),
        }
        if body.kill_pane {
            let inst_for_kill = inst_clone.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || inst_for_kill.kill_ancillary_tmux_sessions())
                    .await
            {
                tracing::warn!(target: "http.api.sessions", "Trash: ancillary tmux kill join failed: {e}");
            }
        }
    } else if body.kill_pane {
        let inst_for_kill = inst_clone.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || inst_for_kill.kill_all_tmux_sessions()).await
        {
            tracing::warn!(target: "http.api.sessions", "Trash: tmux kill join failed: {e}");
        }
    }

    // The session is durably trashed and its agent stopped; relocate its
    // managed worktree out of the active dir into the holding area, then
    // persist the repointed project_path. The git move is blocking, so it runs
    // on a blocking thread. Best-effort: a failure leaves the worktree in
    // place and the daemon's reconcile pass can move it later. Never blocks the
    // trash itself, which already landed above.
    {
        let profile = inst_clone.source_profile.clone();
        match tokio::task::spawn_blocking(move || {
            let mut inst = inst_clone;
            let outcome = crate::session::trash::relocate_worktree_to_trash(&mut inst);
            (outcome, inst)
        })
        .await
        {
            Ok((crate::session::trash::RelocateOutcome::Relocated { .. }, moved)) => {
                let new_path = moved.project_path.clone();
                let pre = moved.pre_trash_project_path.clone();
                let persist_id = id.clone();
                let (np, pp) = (new_path.clone(), pre.clone());
                let _ = persist_session_update(
                    profile,
                    "trash-relocate",
                    state.file_watch.clone(),
                    move |instances| {
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                            inst.project_path = np.clone();
                            inst.pre_trash_project_path = pp.clone();
                        }
                    },
                )
                .await;
                let mut instances = state.instances.write().await;
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    inst.project_path = new_path;
                    inst.pre_trash_project_path = pre;
                }
            }
            Ok((crate::session::trash::RelocateOutcome::Failed { reason }, _)) => {
                tracing::warn!(
                    target: "http.api.sessions",
                    session = %id,
                    "trash worktree relocation skipped: {reason}"
                );
            }
            Ok((crate::session::trash::RelocateOutcome::Skipped, _)) => {}
            Err(e) => tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "trash worktree relocation join failed: {e}"
            ),
        }
    }

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return super::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/restore`. Move a session out of the trash bucket
/// by clearing `trashed_at`. The session returns to its prior bucket (active,
/// or archived if it was archived before trashing); the reconciler respawns a
/// structured-view worker on the next tick since the row is no longer
/// trashed. No teardown. See #2489.
pub async fn restore_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (profile, snapshot) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        (inst.source_profile.clone(), inst.clone())
    };

    // Symmetric claim (#2541): win the Restore claim on disk BEFORE the unlocked
    // worktree move. The in-memory `instance_lock` held above does not serialize
    // a purge running in another process (CLI / another daemon); only the
    // durable on-disk claim does. A fresh peer Purge claim wins and the restore
    // is refused.
    let claim_id = id.clone();
    match with_locked_storage(&state, &profile, move |instances| {
        crate::session::claim::decide_restore_claim(instances, &claim_id, chrono::Utc::now())
    })
    .await
    {
        Ok(crate::session::claim::RestoreClaimDecision::Claimed) => {}
        Ok(crate::session::claim::RestoreClaimDecision::AlreadyGone) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        }
        Ok(crate::session::claim::RestoreClaimDecision::PurgeInProgress) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "purge_in_progress",
                    "message": "Session is being purged; restore was refused"
                })),
            )
                .into_response();
        }
        Err(()) => return persist_failed_response(),
    }

    // Move the worktree back to its pre-trash location before clearing the
    // marker. Strict: if the original path is occupied or git refuses, keep
    // the session trashed and surface a conflict, rather than restoring it to
    // the holding-area path. The git move is blocking, so it runs off the
    // async runtime.
    let restored = match tokio::task::spawn_blocking(move || {
        let mut inst = snapshot;
        let outcome = crate::session::trash::restore_worktree_location(&mut inst);
        (outcome, inst)
    })
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore relocation join failed: {e}");
            release_op_claim(&state, &profile, &id, ClaimOp::Restore).await;
            return persist_failed_response();
        }
    };
    if let crate::session::trash::RestoreOutcome::Failed { reason } = &restored.0 {
        release_op_claim(&state, &profile, &id, ClaimOp::Restore).await;
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "worktree_restore_failed",
                "message": format!("Could not restore the worktree: {reason}")
            })),
        )
            .into_response();
    }
    let restored_path = restored.1.project_path.clone();
    let restored_pre = restored.1.pre_trash_project_path.clone();

    // Final commit under the flock: untrash + release our claim, but only if we
    // still own it. A stale-override purge (past the TTL) may have stolen it
    // mid-move; in that case do not untrash and let the purge win (degrades to
    // #2534, never worse than the status quo). See #2541.
    let commit_id = id.clone();
    let (rp, pre) = (restored_path.clone(), restored_pre.clone());
    let commit = match with_locked_storage(&state, &profile, move |instances| {
        crate::session::claim::finalize_restore_commit(instances, &commit_id, &rp, &pre)
    })
    .await
    {
        Ok(v) => v,
        Err(()) => return persist_failed_response(),
    };
    match commit {
        crate::session::claim::RestoreCommit::Committed => {}
        crate::session::claim::RestoreCommit::PurgeStoleClaim => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "purge_in_progress",
                    "message": "Session was claimed by a purge mid-restore; restore was refused"
                })),
            )
                .into_response();
        }
        crate::session::claim::RestoreCommit::AlreadyGone => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": "Session not found" })),
            )
                .into_response();
        }
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        return persist_failed_response();
    };
    inst.project_path = restored_path;
    inst.pre_trash_project_path = restored_pre;
    inst.untrash();
    inst.clear_op_claim_if_owned(ClaimOp::Restore);
    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/smart-rename`. Manual "Auto-name now" recovery for
/// a structured-view session whose automatic smart rename never landed (the
/// one-shot timed out, returned unusable output, or the daemon restarted with
/// the in-memory attempted set cleared). Clears the per-session attempted gate
/// and re-runs the one-shot against the session's first prompt.
///
/// Only targets a still-default-named session: a session the user (or a prior
/// rename) already named is left alone, so this never overwrites a chosen
/// title. The actual rename runs detached and best-effort, exactly like the
/// prompt-handler trigger; a `202` means "re-run started", not "renamed".
pub async fn force_smart_rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = super::acp::read_only_block(&state) {
        return resp;
    }

    let Some((profile, tool, command, sandboxed, title, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.is_sandboxed(),
                i.title.clone(),
                i.is_structured(),
            )
        })
    }) else {
        return super::session_not_found();
    };

    // Preflight the SAME gate the spawned try_smart_rename re-applies, so the
    // action never reports success (202) for a session the gate would silently
    // drop (disabled, sandboxed, or a resolved rename agent with no one-shot /
    // an overridden command). Without this, the sidebar would show success
    // while no title job runs.
    let config = crate::session::profile_config::resolve_config_or_warn(&profile);
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        structured,
        config.session.smart_rename,
        &title,
        &tool,
        &config.session.smart_rename_agent,
        sandboxed,
        &command,
        &config.session.agent_command_override,
    ) {
        use crate::session::smart_rename::SkipReason;
        let (status, message) = match reason {
            SkipReason::NotStructured => (
                StatusCode::BAD_REQUEST,
                "Session is not a structured-view session",
            ),
            SkipReason::NameNotDefault => {
                (StatusCode::CONFLICT, "Session already has a custom name")
            }
            SkipReason::Disabled => (StatusCode::CONFLICT, "Smart rename is disabled in settings"),
            SkipReason::Sandboxed => (
                StatusCode::CONFLICT,
                "Smart rename is not available for sandboxed sessions",
            ),
            SkipReason::NoOneshot => (
                StatusCode::CONFLICT,
                "The smart-rename agent has no one-shot mode",
            ),
            SkipReason::CommandOverridden => (
                StatusCode::CONFLICT,
                "The smart-rename agent's command is overridden",
            ),
        };
        return (status, Json(serde_json::json!({ "message": message }))).into_response();
    }

    let Some((first_user_prompt, agent_prose)) = state
        .acp_event_store
        .first_turn_context(&id, crate::session::smart_rename::FIRST_TURN_AGENT_BYTES)
    else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "message": "No prompt to name this session from yet" })),
        )
            .into_response();
    };
    let context = crate::session::smart_rename::render_first_turn(&first_user_prompt, &agent_prose);

    // Clear the attempted gate so try_smart_rename does not short-circuit on a
    // prior failed attempt. The inflight guard inside try_smart_rename still
    // prevents a concurrent one-shot for the same session.
    {
        let mut attempted = state
            .smart_rename_attempted
            .lock()
            .expect("smart_rename_attempted poisoned");
        attempted.remove(&id);
    }

    tokio::spawn(crate::session::smart_rename::try_smart_rename(
        state.clone(),
        id.clone(),
        crate::session::smart_rename::SmartRenameInput {
            first_user_prompt,
            context,
        },
        crate::session::smart_rename::RenameTrigger::Forced,
    ));
    StatusCode::ACCEPTED.into_response()
}

/// On-demand "summarize the conversation so far" for a structured-view
/// session. Preflights the same eligibility gate the spawned task re-applies
/// so the caller never gets a 202 for a session that would silently drop, then
/// runs the summary one-shot detached (best-effort, like the automatic
/// trigger). A `202` means "summary started", not "summary ready"; the result
/// arrives later as a `ConversationSummary` event over the structured-view WS.
/// Bypasses the `conversation_summary` setting and the delta threshold: an
/// explicit request always runs if the session is eligible. See #2808.
pub async fn summarize_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = super::acp::read_only_block(&state) {
        return resp;
    }

    let Some((profile, tool, command, sandboxed, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.is_sandboxed(),
                i.is_structured(),
            )
        })
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "message": "Session not found" })),
        )
            .into_response();
    };

    let config = crate::session::profile_config::resolve_config_or_warn(&profile);
    if let Err(reason) = crate::session::conversation_summary::resolve_summary_agent(
        structured,
        &tool,
        &config.session.conversation_summary_agent,
        sandboxed,
        &command,
        &config.session.agent_command_override,
    ) {
        use crate::session::smart_rename::SkipReason;
        let (status, message) = match reason {
            SkipReason::NotStructured => (
                StatusCode::BAD_REQUEST,
                "Session is not a structured-view session",
            ),
            SkipReason::Sandboxed => (
                StatusCode::CONFLICT,
                "Conversation summary is not available for sandboxed sessions",
            ),
            SkipReason::NoOneshot => (
                StatusCode::CONFLICT,
                "The summary agent has no one-shot mode",
            ),
            SkipReason::CommandOverridden => (
                StatusCode::CONFLICT,
                "The summary agent's command is overridden",
            ),
            // resolve_summary_agent never returns the rename-only reasons.
            SkipReason::NameNotDefault | SkipReason::Disabled => (
                StatusCode::CONFLICT,
                "Conversation summary is unavailable for this session",
            ),
        };
        return (status, Json(serde_json::json!({ "message": message }))).into_response();
    }

    tokio::spawn(
        crate::session::conversation_summary::try_conversation_summary(
            state.clone(),
            id.clone(),
            crate::session::conversation_summary::SummaryTrigger::Manual,
        ),
    );
    StatusCode::ACCEPTED.into_response()
}

/// Stop a session, matching the TUI's `x` keybind: kill the tmux pane and
/// stop (but do not remove) the Docker container for plain sessions; shut down
/// the worker for structured-view sessions. The session record is preserved
/// with status `Stopped` so it can be resumed later. This is NOT delete.
pub async fn stop_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Snapshot profile, session type, and current status without mutating yet
    // so a persist failure leaves disk and memory in agreement (mirrors the
    // archive handler).
    let (profile, is_structured, already_stopped) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        let structured;
        #[cfg(feature = "serve")]
        {
            structured = inst.is_structured();
        }
        #[cfg(not(feature = "serve"))]
        {
            structured = false;
        }
        // Mirror the TUI's `stop_selected` guard: a session that is already
        // stopped or mid-lifecycle has nothing to stop.
        let already = matches!(
            inst.status,
            Status::Stopped | Status::Deleting | Status::Creating
        );
        (inst.source_profile.clone(), structured, already)
    };

    if already_stopped {
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return super::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Persist Stopped first. For structured sessions also mark the row
    // idle-dormant so the acp reconciler does not respawn the worker we are
    // about to shut down (mirrors the structured auto-stop reaper).
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "stop session",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.status = Status::Stopped;
                if is_structured {
                    inst.mark_idle_dormant();
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    // Disk is durable; apply to memory and snapshot the instance for the
    // side effects below.
    let inst_clone = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "stop session: instance vanished after persist"
            );
            return super::session_gone_after_persist();
        };
        inst.status = Status::Stopped;
        if is_structured {
            inst.mark_idle_dormant();
        }
        inst.clone()
    };

    if is_structured {
        // Structured view: shut down the worker so the reconciler does not
        // race to respawn it. `shutdown` preserves the transcript, so the
        // session resumes the conversation when reopened.
        #[cfg(feature = "serve")]
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during stop failed: {e}"
            ),
        }
    } else {
        // Plain session: kill the tmux pane and stop (not remove) the Docker
        // container. `Instance::stop` can block ~10s on `docker stop`, so run
        // it off the async runtime. Mirrors the TUI's StopPoller.
        let inst_for_stop = inst_clone.clone();
        match tokio::task::spawn_blocking(move || inst_for_stop.stop()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                target: "http.api.sessions",
                "Stop: session stop failed: {e}"
            ),
            Err(e) => tracing::warn!(
                target: "http.api.sessions",
                "Stop: stop join failed: {e}"
            ),
        }
    }

    // Re-read so the response reflects the Stopped status.
    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return super::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// Start (resume) a stopped session, the inverse of [`stop_session`]. Plain
/// sessions are restarted exactly like `ensure_session` (kill any corpse pane,
/// then `start_with_resume_fallback`); structured sessions are un-parked by
/// clearing the idle-dormant mark so the acp reconciler respawns the worker on
/// its next tick (mirrors unarchive). No-op for a session that isn't stopped.
pub async fn start_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (profile, is_structured, is_stopped, instance) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        let structured;
        #[cfg(feature = "serve")]
        {
            structured = inst.is_structured();
        }
        #[cfg(not(feature = "serve"))]
        {
            structured = false;
        }
        (
            inst.source_profile.clone(),
            structured,
            matches!(inst.status, Status::Stopped),
            inst.clone(),
        )
    };

    // Only a stopped session has anything to start; otherwise return current.
    if !is_stopped {
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return super::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    if is_structured {
        // Un-park: clear the dormant mark and drop the Stopped status so the
        // reconciler's next tick treats it as a resume target and respawns the
        // worker (the transcript was preserved by stop's shutdown).
        let persist_id = id.clone();
        if persist_session_update(
            profile,
            "start session",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    inst.idle_dormant_since = None;
                    inst.status = Status::Idle;
                    inst.last_error = None;
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.idle_dormant_since = None;
                inst.status = Status::Idle;
                inst.last_error = None;
            }
        }
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return super::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Plain session: restart the tmux pane, mirroring ensure_session. Show
    // Starting immediately so the status poller doesn't flip it back while the
    // restart (which can block) is in flight.
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.status = Status::Starting;
            inst.last_error = None;
        }
    }

    let sync_base = instance.clone();
    let restart_result = tokio::task::spawn_blocking(
        move || -> Result<(Instance, crate::session::StartOutcome), Box<(Instance, anyhow::Error)>> {
            let mut inst = instance;
            if let Err(e) = inst.kill_clean() {
                return Err(Box::new((inst, e)));
            }
            // Explicit restart endpoint (web dashboard Restart button):
            // honor auto_resume_on_restart, same as TUI `e`/`Enter`. See #2609.
            match inst.start_with_resume_fallback(
                None,
                false,
                crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting,
            ) {
                Ok(outcome) => Ok((inst, outcome)),
                Err(e) => Err(Box::new((inst, e))),
            }
        },
    )
    .await;

    match restart_result {
        Ok(Ok((started, outcome))) => {
            let resume_failed_sid = match &outcome {
                crate::session::StartOutcome::ResumeFailed { sid } => Some(sid.clone()),
                _ => None,
            };
            let mut instances = state.instances.write().await;
            let response = match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) => {
                    apply_post_restart_sync(inst, &sync_base, &started);
                    SessionResponse::from_instance(
                        inst,
                        crate::claude_settings::read_tui_fullscreen(),
                    )
                }
                None => {
                    return super::session_not_found();
                }
            };
            if let Some(sid) = resume_failed_sid {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "resume_failed",
                        "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        "resume_session_id": sid,
                    })),
                )
                    .into_response();
            }
            (StatusCode::OK, Json(serde_json::json!(response))).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "start_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(inst, &sync_base, &started);
                inst.status = Status::Error;
                inst.last_error = Some(msg.clone());
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "restart_failed", "message": msg})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "start_session panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

pub async fn update_session_snooze(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateSnoozeBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
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

    let (was_structured_view, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        let structured_view;
        #[cfg(feature = "serve")]
        {
            structured_view = inst.is_structured();
        }
        #[cfg(not(feature = "serve"))]
        {
            structured_view = false;
        }
        (structured_view, inst.source_profile.clone())
    };

    let minutes = body.minutes;

    // Persist first; only mutate memory once disk is durable, and only fire
    // the structured view teardown below on a write that landed. See #1589.
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "snooze update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                match minutes {
                    Some(m) => inst.snooze(m),
                    None => inst.unsnooze(),
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "snooze update: instance vanished after persist"
            );
            return super::session_gone_after_persist();
        };
        match minutes {
            Some(m) => inst.snooze(m),
            None => inst.unsnooze(),
        }
    }

    // For structured view-mode sessions, snoozing tears down the worker the
    // same way archive does. Snooze is a "temporary archive" in the
    // data model and the structured view worker (claude-agent-acp subprocess)
    // is heavy enough that keeping it idle while the row is sunk is a
    // resource hog. The reconciler skips snoozed sessions, so the
    // worker stays down until the snooze expires; the next reconciler
    // tick after expiry brings it back. Unsnooze just lets the
    // reconciler re-pick the session naturally, no explicit respawn.
    // `shutdown` preserves the agent transcript (no session/delete), so
    // that respawn resumes the conversation instead of resetting it
    // (#1710).
    #[cfg(feature = "serve")]
    if was_structured_view && minutes.is_some() {
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during snooze failed: {e}"
            ),
        }
    }
    #[cfg(not(feature = "serve"))]
    let _ = was_structured_view;

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return super::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `PATCH /api/sessions/{id}/unread` — flag a session unread (`{"unread":true}`)
/// or mark it read (`{"unread":false}`). Mirrors the TUI's `u` toggle, but the
/// client computes the target from the current state rather than toggling
/// server-side, so an optimistic UI update can't desync. No-op when the
/// `session.unread_indicator` feature is off (the client hides the control
/// then, but guard here too). Persist-then-mutate, like snooze.
pub async fn update_session_unread(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUnreadBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let mark_unread = body.unread;

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        inst.source_profile.clone()
    };

    // Feature off: report the current state without mutating, matching the
    // TUI's no-op when `session.unread_indicator` is disabled.
    if crate::session::unread_enabled() {
        let persist_id = id.clone();
        if persist_session_update(
            profile,
            "unread update",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    if mark_unread {
                        inst.mark_unread();
                    } else {
                        inst.mark_read();
                    }
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }

        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "unread update: instance vanished after persist"
            );
            return super::session_gone_after_persist();
        };
        if mark_unread {
            inst.mark_unread();
        } else {
            inst.mark_read();
        }
    }

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return super::session_not_found();
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

/// Flip a session out of `Status::Deleting` into `Status::Error` so a
/// bookkeeping failure after teardown does not strand it greyed-out and
/// unclickable, the exact state this detached-task delete exists to prevent.
async fn mark_delete_error(state: &AppState, id: &str, message: String) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.status = Status::Error;
        inst.last_error = Some(message);
    }
}

/// Permanently purge a session: irreversible ACP teardown (structured
/// view), optional sidecar cleanup (worktree/branch/container/scratch per
/// `body`), and removal from both `sessions.json` and the in-memory list.
/// Shared by the `DELETE /api/sessions/{id}` handler and the retention
/// auto-purge worker so the permanent-delete path can never diverge between
/// the two. Returns the user-facing messages from `perform_deletion` on
/// success, or a descriptive error string on failure (the caller decides how
/// to surface it). The caller is expected to hold the per-instance lock.
#[cfg_attr(not(feature = "serve"), allow(unused_variables))]
async fn purge_session_artifacts(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
) -> Result<Vec<String>, String> {
    let profile = instance.source_profile.clone();
    let was_trashed = instance.is_trashed();

    // Symmetric claim (#2541): win the Purge claim on disk before any
    // irreversible teardown, so a restore from another process cannot bring the
    // session back after its artifacts are gone. Refuse when a fresh Restore
    // claim holds the row; a row already gone from disk proceeds (the teardown
    // is idempotent and the final removal is a no-op).
    let claim_id = id.to_string();
    match with_locked_storage(state, &profile, move |instances| {
        crate::session::claim::decide_purge_claim(
            instances,
            &claim_id,
            was_trashed,
            chrono::Utc::now(),
        )
    })
    .await
    {
        Ok(crate::session::claim::PurgeClaimDecision::Claimed)
        | Ok(crate::session::claim::PurgeClaimDecision::AlreadyGone) => {}
        Ok(crate::session::claim::PurgeClaimDecision::Restored)
        | Ok(crate::session::claim::PurgeClaimDecision::RestoreInProgress) => {
            return Err("Session is being restored, so it was not purged".to_string())
        }
        Err(()) => {
            return Err("Failed to acquire the purge claim under the storage lock".to_string())
        }
    }

    // True once we have crossed the irreversible line (the structured-view
    // transcript has been deleted). After that point a sidecar-cleanup
    // failure must NOT leave the session row restorable, since the restore
    // would resurrect a session whose transcript is already gone. See #2489.
    #[cfg(feature = "serve")]
    let transcript_purged = instance.is_structured();
    #[cfg(not(feature = "serve"))]
    let transcript_purged = false;

    // Tear down the structured view worker FIRST so the ACP subprocess + its
    // claude-agent-acp child don't leak past the session delete. Permanent
    // removal releases the agent's persisted transcript too (#1710); the
    // event store purge prevents a recreated same-id session from inheriting
    // the deleted transcript.
    #[cfg(feature = "serve")]
    if transcript_purged {
        match state.acp_supervisor.shutdown_and_delete(id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    "shutdown during purge failed: {e}"
                );
            }
        }
        state.acp_supervisor.forget_session(id);
        state.acp_event_store.delete_session(id);
    }

    let (delete_worktree, delete_branch, delete_sandbox, force_delete, keep_scratch) = (
        body.delete_worktree,
        body.delete_branch,
        body.delete_sandbox,
        body.force_delete,
        body.keep_scratch,
    );
    let deletion_id = id.to_string();
    let deletion_result = tokio::task::spawn_blocking(move || {
        crate::session::deletion::perform_deletion(&crate::session::deletion::DeletionRequest {
            session_id: deletion_id,
            instance,
            delete_worktree,
            delete_branch,
            delete_sandbox,
            force_delete,
            detach_hooks: true,
            keep_scratch,
        })
    })
    .await
    .map_err(|e| format!("Deletion task failed: {e}"))?;

    let mut messages = deletion_result.messages.clone();
    if !deletion_result.success {
        let errs = if deletion_result.errors.is_empty() {
            "Unknown error".to_string()
        } else {
            deletion_result.errors.join("; ")
        };
        if !transcript_purged {
            // Nothing irreversible happened (no transcript to lose), so keep
            // the row intact and let the caller surface the error; the user
            // can retry, e.g. with force on a dirty worktree.
            release_op_claim(state, &profile, id, ClaimOp::Purge).await;
            return Err(errs);
        }
        // The durable transcript is already gone; a kept row would only allow
        // a broken restore. Commit the removal and surface the sidecar errors
        // as warnings so the orphaned worktree/container can be cleaned up by
        // hand. See #2489.
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "purge sidecar cleanup failed after the transcript was deleted; removing the session row anyway: {errs}"
        );
        messages.push(format!(
            "Cleanup incomplete (session removed anyway): {errs}"
        ));
    }

    // Disk first: if persistence fails, in-memory state stays intact and the
    // poll loop will not re-add a half-deleted row.
    // #2534/#2541: revalidate under the flock. The teardown ran on an unlocked
    // snapshot; if a restore from another process brought the session back
    // (stale-override past the claim TTL), keep the row and release our purge
    // claim (ownership-guarded) rather than deleting a session the user just
    // restored. Degrades that rare residual to #2534 behavior, never worse.
    // Deliberately hand-rolled rather than routed through `with_locked_storage`:
    // this fn returns `Result<_, String>` and must propagate the storage error
    // string up to the caller, whereas `with_locked_storage` logs and collapses
    // to `Result<_, ()>`. See #2541.
    let storage = Storage::new(&profile, state.file_watch.clone())
        .map_err(|e| format!("Session was torn down but storage init failed: {e}"))?;
    let id_for_save = id.to_string();
    let kept_restored = tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            Ok(matches!(
                crate::session::claim::finalize_purge_removal(instances, &id_for_save, was_trashed),
                crate::session::claim::PurgeCommit::KeptRestored
            ))
        })
    })
    .await
    .map_err(|e| format!("Persist task panicked: {e}"))?
    .map_err(|e| {
        format!("Session deletion completed on disk, but sessions.json could not be updated: {e}")
    })?;

    if kept_restored {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "session was restored while its purge ran; kept the restored row, but its worktree, branch, container, or transcript may already be gone"
        );
        // Leave the in-memory row and its lock in place; the poll loop
        // converges its trashed flag from the peer's on-disk untrash.
        return Ok(messages);
    }

    {
        let mut instances = state.instances.write().await;
        instances.retain(|i| i.id != id);
    }
    state.instance_locks.write().await.remove(id);
    if let Some(entry) = recent_entry {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "http.api.sessions",
                "recording recent project after delete failed: {e}");
        }
    }
    Ok(messages)
}

/// Relocate any trashed managed worktree still sitting in the active dir into
/// the holding area, and heal a pointer left stale by a crash between the move
/// and its persist. Backfills rows trashed before relocation existed. Runs
/// once on daemon startup, best-effort and per-session locked; a failure on one
/// session logs and moves on. The git move is blocking, so it runs off the
/// async runtime.
pub(crate) async fn reconcile_trashed_worktrees(state: &Arc<AppState>) {
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_trashed())
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };

    // Self-heal (#2541): clear op_claims left by a purge/restore that crashed
    // mid-operation. `try_claim` already treats an expired claim as free (that
    // is the authoritative self-heal), so this startup sweep is
    // belt-and-suspenders that tidies stale claims on disk. It only visits
    // profiles that currently have a trashed candidate; a stray claim elsewhere
    // is still healed by the next `try_claim`.
    let profiles: std::collections::HashSet<String> =
        candidates.iter().map(|(_, p)| p.clone()).collect();
    let now = chrono::Utc::now();
    let ttl = Instance::OP_CLAIM_TTL;
    for profile in profiles {
        let _ = persist_session_update(
            profile,
            "op-claim-self-heal",
            state.file_watch.clone(),
            move |instances| {
                for inst in instances.iter_mut() {
                    inst.clear_expired_op_claim(ttl, now);
                }
            },
        )
        .await;
    }

    for (id, profile) in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        let snapshot = {
            let instances = state.instances.read().await;
            match instances.iter().find(|i| i.id == id) {
                Some(i) if i.is_trashed() => i.clone(),
                _ => continue,
            }
        };
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut inst = snapshot;
            let changed = crate::session::trash::reconcile_trashed_location(&mut inst);
            (changed, inst)
        })
        .await
        {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "trash reconcile join failed: {e}");
                continue;
            }
        };
        if !reconciled.0 {
            continue;
        }
        let moved = reconciled.1;
        let (np, pre) = (
            moved.project_path.clone(),
            moved.pre_trash_project_path.clone(),
        );
        let persist_id = id.clone();
        let _ = persist_session_update(
            profile,
            "trash-reconcile",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    inst.project_path = np.clone();
                    inst.pre_trash_project_path = pre.clone();
                }
            },
        )
        .await;
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.project_path = moved.project_path;
            inst.pre_trash_project_path = moved.pre_trash_project_path;
        }
    }
}

/// Auto-purge trashed sessions whose retention window has elapsed
/// (`trashed_at + session.trash_retention_days`). Runs on daemon startup and
/// hourly thereafter. Routed through [`purge_session_artifacts`] so the
/// permanent-delete path matches `DELETE` exactly. Each candidate is
/// per-instance locked and its trashed+expired state re-validated under the
/// lock, so a concurrent restore wins the race and is never purged. See
/// #2489.
#[cfg(feature = "serve")]
pub(crate) async fn purge_expired_trash(state: &Arc<AppState>) {
    use std::collections::HashMap;

    let now = chrono::Utc::now();
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_trashed())
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }

    let mut retention_by_profile: HashMap<String, u32> = HashMap::new();
    for (id, profile) in candidates {
        let retention = *retention_by_profile
            .entry(profile.clone())
            .or_insert_with(|| {
                crate::session::profile_config::resolve_config_or_warn(&profile)
                    .session
                    .trash_retention_days
            });
        if retention == 0 {
            continue;
        }

        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        // Re-validate under the lock: a restore (or an earlier purge) may
        // have landed since the snapshot.
        let (instance, recent_entry) = {
            let instances = state.instances.read().await;
            match instances.iter().find(|i| i.id == id) {
                Some(inst) if crate::session::trash::is_expired(inst, retention, now) => {
                    (inst.clone(), crate::session::recent_project_entry_for(inst))
                }
                _ => continue,
            }
        };

        // Permanent retention purge cleans sidecars per the profile defaults,
        // but forces removal so a dirty worktree can't keep an expired
        // session pinned in the trash forever.
        let cfg = crate::session::profile_config::resolve_config_or_warn(&instance.source_profile);
        let body = DeleteSessionBody {
            delete_worktree: cfg.worktree.auto_cleanup,
            delete_branch: cfg.worktree.should_delete_branch_on_cleanup(),
            delete_sandbox: cfg.sandbox.auto_cleanup,
            force_delete: true,
            keep_scratch: false,
        };
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok(_) => tracing::info!(
                target: "http.api.sessions",
                session = %id,
                "auto-purged expired trashed session"
            ),
            Err(e) => tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "auto-purge of expired trash failed: {e}"
            ),
        }
    }
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Acquire per-instance lock to serialize concurrent mutations.
    // Owned guard so it can move into the detached deletion task below and
    // stay held until the bookkeeping finishes, rather than only until this
    // request future is dropped.
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock_owned().await;

    // Find and clone the instance (need the full Instance for deletion)
    let instance = {
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).cloned()
    };

    let Some(instance) = instance else {
        return super::session_not_found();
    };

    // Captured before `instance` moves into the deletion task; recorded into
    // the persisted recent-projects store only once the delete fully
    // succeeds, so the project survives in the wizard Recent tab (#2141).
    let recent_entry = crate::session::recent_project_entry_for(&instance);

    // Run the whole teardown + bookkeeping in a detached task. The
    // git / docker / tmux teardown below is irreversible once it starts, but
    // the disk-removal and in-memory cleanup that must follow it live in this
    // request future. If the client disconnects mid-delete (e.g. closes the
    // tab during a multi-second worktree removal), dropping the request future
    // would abandon that bookkeeping after the session was already physically
    // gone, stranding it greyed-out in the "Deleting" state forever. A
    // detached task is not cancelled when the request future drops, so it
    // always runs to completion; the owned lock guard moves in and is held
    // until the bookkeeping finishes.
    let join = tokio::spawn(async move {
        let _guard = guard;

        // Mark as Deleting so polling clients see the status change
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.status = Status::Deleting;
            }
        }

        match purge_session_artifacts(&state, &id, instance, &body, recent_entry).await {
            Ok(messages) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "deleted",
                    "messages": messages,
                })),
            ),
            Err(msg) => {
                mark_delete_error(&state, &id, msg.clone()).await;
                tracing::error!(target: "http.api.sessions", "delete failed: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "deletion_failed",
                        "message": msg,
                    })),
                )
            }
        }
    });

    match join.await {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions",
                "Deletion task panicked or was cancelled: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal",
                    "message": "Deletion task failed",
                })),
            )
                .into_response()
        }
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
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub view: crate::session::View,
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub agent_name: Option<String>,
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub agent_model: Option<String>,
    #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
    #[serde(default)]
    pub fork_from: Option<String>,
}

fn create_body_uses_worktree(body: &CreateSessionBody) -> bool {
    body.worktree_enabled || body.worktree_branch.is_some()
}

fn create_body_combines_scratch_and_worktree(body: &CreateSessionBody) -> bool {
    body.scratch && create_body_uses_worktree(body)
}

/// Resolve the one-shot fork seed for a `fork_from` create request. A
/// structured request (`structured == true`) forks through ACP `session/fork`
/// against the parent's `acp_session_id`; a terminal request resumes the
/// parent agent id with the agent's fork flag, generating a fresh child id.
/// `Err` reports an unforkable terminal agent or missing parent id; structured
/// forks defer that check to the live `session/fork` handshake.
#[cfg(feature = "serve")]
fn resolve_create_fork_seed(
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
        crate::session::capture::generate_claude_session_id(),
    )
}

/// True when a create request asks to both import an existing session and fork
/// a parent. The two seed the new session from different sources, so allowing
/// both would produce a contradictory half-imported, half-forked session.
/// Trailing whitespace is treated as unset, matching the per-field guards.
#[cfg(feature = "serve")]
fn both_import_and_fork_set(body: &CreateSessionBody) -> bool {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    set(&body.import_acp_session_id) && set(&body.fork_from)
}

/// Thin server-side alias for [`crate::session::fork::structured_fork_capable`],
/// the single source of truth for "can this agent run the ACP `session/fork`
/// handshake?". Shared by the `SessionResponse.acp_can_fork` projection (the web
/// "Fork" affordance) and the create-time guard so they cannot drift.
#[cfg(feature = "serve")]
fn agent_is_structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    crate::session::fork::structured_fork_capable(tool, agent_name)
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

/// Insert `instance` into the live registry, replacing any entry that
/// already carries the same id rather than blind-pushing a second copy.
///
/// `create_session` persists the new session to disk (in `persist_and_start`)
/// before it pushes the in-memory copy here. A `status_poll_loop` tick that
/// fires in that window calls `load_all_instances`, reads the just-persisted
/// row, and inserts it first. A blind `push` would then leave two entries
/// with the same id in `state.instances` until the next poll tick collapses
/// them, and `GET /api/sessions` would briefly return the session twice.
fn upsert_instance(
    instances: &mut Vec<crate::session::Instance>,
    instance: crate::session::Instance,
) {
    if let Some(existing) = instances.iter_mut().find(|i| i.id == instance.id) {
        *existing = instance;
    } else {
        instances.push(instance);
    }
}

/// Carried out of `create_session` to mark a create that was refused because
/// the repo's hooks (or project MCP) need approval and the request did not pass
/// `trust_hooks: true` (#2066). The outer match downcasts this to emit a
/// structured `hooks_need_trust` response instead of the generic
/// `create_failed`, so a caller can show the commands and resubmit.
#[derive(Debug)]
struct HooksNeedTrust {
    /// The `on_create` commands that would run, for display in the prompt.
    on_create: Vec<String>,
    /// The `on_launch` commands the same approval would trust. They don't run
    /// on this create, but the recorded trust covers them for every later
    /// session (TUI/CLI included), so the prompt must show them too.
    on_launch: Vec<String>,
    /// Likewise for `on_destroy`, run when a session is deleted.
    on_destroy: Vec<String>,
    /// True when the repo's `.mcp.json` also needs approval at this fingerprint.
    needs_mcp_trust: bool,
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
struct CreateHookPlan {
    /// Commands to run, already merged (repo overrides global/profile per type).
    on_create: Vec<String>,
    /// `(hooks_hash, mcp_hash)` to persist into `trusted_repos.toml` when the
    /// caller passed `trust_hooks: true` and a surface needed approval. `None`
    /// when nothing new needs recording (already trusted, or no hooks/MCP).
    trust_write: Option<(Option<String>, Option<String>)>,
}

/// Resolve the repo's `on_create` hooks and the trust decision for a web-API
/// create. Returns `Err(HooksNeedTrust)` when a surface needs approval and the
/// caller did not pass `trust_hooks: true`; the surrounding handler maps that to
/// a structured `hooks_need_trust` response. Mirrors the CLI `--trust-hooks`
/// path in `src/cli/add.rs`, adapted for the API's non-interactive context.
fn resolve_create_hook_plan(
    profile: &str,
    project_path: &std::path::Path,
    scratch: bool,
    trust_hooks_requested: bool,
) -> anyhow::Result<CreateHookPlan> {
    use crate::session::repo_config::{self, TrustSurface};

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
fn run_create_hooks(
    instance: &mut Instance,
    plan: &CreateHookPlan,
    project_path: &std::path::Path,
) -> anyhow::Result<()> {
    use crate::session::repo_config;

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

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CreateSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

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

    // Import and fork are mutually exclusive: each seeds the new session from a
    // different source (import adopts an on-disk session id; fork resumes a
    // parent's captured id), and honoring both would leave the session in a
    // contradictory half-imported, half-forked state. Reject up front.
    #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
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
    #[cfg(feature = "serve")]
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

    let profile = body.profile.unwrap_or_else(|| state.profile.clone());
    let instances = state.instances.read().await;
    let existing_titles: Vec<String> = instances.iter().map(|i| i.title.clone()).collect();
    let existing_branches: Vec<String> = instances
        .iter()
        .filter_map(|i| i.worktree_info.as_ref().map(|w| w.branch.clone()))
        .collect();
    drop(instances);

    let file_watch_for_create = state.file_watch.clone();

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

        // Resolve repo hook trust BEFORE building the worktree (#2066): a repo
        // whose hooks need approval and that was not sent `trust_hooks: true`
        // is refused here, so the handler never leaves an orphan worktree on
        // disk. The original `path` is the trust anchor (the same source the
        // CLI/TUI use); `check_repo_trust` resolves a worktree path to its main
        // repo, so a worktree created from an already-trusted repo inherits its
        // trust without a separate prompt.
        let original_path = body.path.clone();
        let hook_plan = resolve_create_hook_plan(
            &profile,
            std::path::Path::new(&original_path),
            body.scratch,
            body.trust_hooks.unwrap_or(false),
        )?;

        let title = body.title.unwrap_or_default();
        let worktree_branch = body
            .worktree_branch
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());

        let params = InstanceParams {
            title,
            path: body.path,
            group: body.group,
            tool: body.tool,
            worktree_enabled,
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
            #[cfg(feature = "serve")]
            fork_seed,
            #[cfg(not(feature = "serve"))]
            fork_seed: None,
        };

        let build_result = builder::build_instance(params, &title_refs, &branch_refs, &profile)?;
        let mut instance = build_result.instance;
        instance.source_profile = profile.clone();
        let build_warnings = build_result.warnings;
        let created_worktree = build_result.created_worktree;
        let created_workspace_worktrees = build_result.created_workspace_worktrees;

        // Apply per-session sandbox overrides from the request body.
        if let Some(ref mut sandbox) = instance.sandbox_info {
            if body.custom_instruction.is_some() {
                sandbox.custom_instruction = body.custom_instruction;
            }
        }

        // Apply structured-view fields from the request body. structured_view is
        // re-validated below against real ACP capability; non-ACP tools
        // fall back to terminal view rather than erroring at spawn time.
        #[cfg(feature = "serve")]
        let agent_effort = {
            instance.view = body.view;
            // #2276: importing an existing Claude session forces the
            // structured view and adopts the on-disk session id, so the
            // structured spawn resumes it via session/load and seeds the
            // transcript from the agent's history replay. `path` is the
            // session's original cwd (the wizard prefills it).
            if let Some(import_id) = body
                .import_acp_session_id
                .clone()
                .filter(|s| !s.trim().is_empty())
            {
                instance.view = crate::session::View::Structured;
                instance.acp_session_id = Some(import_id);
                instance.import_pending = Some(true);
            }
            instance.agent_name = body.agent_name;
            let agent_key = instance
                .agent_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(instance.tool.as_str())
                .to_string();
            let resolved_config = crate::session::repo_config::resolve_config_with_repo_or_warn(
                &instance.source_profile,
                std::path::Path::new(&instance.project_path),
            );
            let defaults = resolved_config.acp.acp_defaults_for(&agent_key);
            // Preserve the explicit request model separately (trimmed to match
            // the resolver's normalization) so a terminal fallback below can
            // keep it while dropping any ACP-derived default; agent_model is
            // ACP-only.
            let explicit_model = body
                .agent_model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // Explicit request wins, else the per-agent default; effort is keyed
            // on the resolved model. Same single-source resolver the spawn path
            // uses; persist the model here so the composer shows it and the
            // session stays pinned to it. See resolve_spawn_model_effort.
            let (resolved_model, mut agent_effort) =
                crate::session::config::resolve_spawn_model_effort(
                    defaults,
                    explicit_model.clone(),
                    body.agent_effort,
                );
            instance.agent_model = resolved_model;
            // Don't trust the client's capability decision. Re-resolve
            // whether this agent can actually run in structured view; a custom
            // agent without an `agent_acp_cmd` (or any non-ACP tool)
            // falls back to tmux here rather than erroring at spawn time.
            if instance.is_structured() {
                let acp_registry = crate::acp::AgentRegistry::with_defaults();
                let resolved = instance
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(instance.tool.as_str());
                let capable = acp_registry.get(resolved).is_some()
                    || crate::session::repo_config::resolve_config_with_repo_or_warn(
                        &instance.source_profile,
                        std::path::Path::new(&instance.project_path),
                    )
                    .session
                    .agent_acp_cmd
                    .get(&instance.tool)
                    .is_some_and(|cmd| {
                        crate::acp::AgentSpec::from_acp_cmd(&instance.tool, cmd).is_ok()
                    });
                if capable {
                    instance.view = crate::session::View::Structured;
                } else {
                    instance.view = crate::session::View::Terminal;
                    // A non-ACP tool cannot run the structured session/fork
                    // handshake. If a malformed request seeded a structured
                    // fork (fork_pending/import_pending set by the builder),
                    // drop those markers so a later switch-to-structured does
                    // not fire an unexpected session/fork against the parent.
                    instance.fork_pending = None;
                    instance.import_pending = None;
                }
            }

            if !instance.is_structured() {
                agent_effort = None;
                // Terminal sessions keep only an explicitly requested model,
                // never an ACP-derived default (agent_model is ACP-only).
                instance.agent_model = explicit_model;
            }

            agent_effort
        };

        // Run on_create hooks now that the worktree exists, before the session
        // is persisted or started (#2066). Mirrors the TUI/CLI ordering so the
        // worktree is bootstrapped (`.env` copies, venv symlinks, DB seeds)
        // before the agent launches. On failure, tear down the just-built
        // worktree/container so a broken hook doesn't leave an orphan.
        if let Err(e) = run_create_hooks(
            &mut instance,
            &hook_plan,
            std::path::Path::new(&original_path),
        ) {
            builder::cleanup_instance(
                &instance,
                created_worktree.as_ref(),
                &created_workspace_worktrees,
            );
            return Err(anyhow::anyhow!("on_create hook failed: {e:#}"));
        }

        // Anything that fails between here and the final `Ok(..)`
        // would otherwise orphan the scratch directory `build_instance`
        // already provisioned (Storage::new, storage.update,
        // instance.start). Wrap the tail in an IIFE-equivalent closure
        // so we can run cleanup on Err once, regardless of which step
        // tripped. Matches the CLI cleanup path in
        // `cleanup_partial_session(... scratch_dir: Some(...))`.
        let mut persist_and_start = || -> anyhow::Result<()> {
            let storage = Storage::new(&profile, file_watch_for_create.clone())?;
            let to_persist = instance.clone();
            storage.update(|all, _groups| {
                all.push(to_persist);
                Ok(())
            })?;

            // Acp-mode sessions are not backed by tmux; the structured view
            // supervisor spawns the ACP agent on demand. Skip the tmux
            // `start()` to avoid creating an empty pane that no one will
            // attach to.
            #[cfg(feature = "serve")]
            let skip_tmux_start = instance.is_structured();
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

        #[cfg(feature = "serve")]
        return Ok::<(Instance, Vec<String>, Option<String>), anyhow::Error>((
            instance,
            build_warnings,
            agent_effort,
        ));

        #[cfg(not(feature = "serve"))]
        Ok::<(Instance, Vec<String>), anyhow::Error>((instance, build_warnings))
    })
    .await;

    match result {
        #[cfg(feature = "serve")]
        Ok(Ok((instance, warnings, agent_effort))) => {
            let mut resp = SessionResponse::from_instance(
                &instance,
                crate::claude_settings::read_tui_fullscreen(),
            );
            resp.warnings = warnings;
            // Carry the resolved tie value (#1927); list_sessions' overlay does
            // not run on this create response, so a managed worktree would
            // otherwise report untied until the next list refresh.
            if resp.has_managed_worktree {
                resp.tie_workdir_to_name = crate::session::profile_config::resolve_config_or_warn(
                    &instance.source_profile,
                )
                .session
                .tie_workdir_to_name;
            }
            if !resp.acp_capable {
                let acp_cmd = crate::session::repo_config::resolve_config_with_repo_or_warn(
                    &instance.source_profile,
                    std::path::Path::new(&instance.project_path),
                )
                .session
                .agent_acp_cmd;
                resp.acp_capable = custom_agent_acp_capable(&acp_cmd, &instance.tool);
            }
            let acp_spawn_target = if instance.is_structured() {
                Some((
                    instance.id.clone(),
                    instance.tool.clone(),
                    instance.agent_name.clone(),
                    instance.agent_model.clone(),
                    agent_effort,
                    instance.project_path.clone(),
                    instance.acp_session_id.clone(),
                    instance.source_profile.clone(),
                    instance.yolo_mode,
                    instance.command.clone(),
                    instance.import_pending == Some(true),
                    instance.fork_pending.clone(),
                ))
            } else {
                None
            };
            let mut instances = state.instances.write().await;
            upsert_instance(&mut instances, instance);
            drop(instances);

            // Count the create for the opt-in telemetry trend counter. Bounded
            // accumulator, read-and-decremented by the snapshot loop; no-op for
            // opted-out installs (the snapshot is never built / sent).
            state
                .telemetry_session_creates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if let Some((
                id,
                tool,
                agent_override,
                model,
                effort,
                project_path,
                stored_acp_session_id,
                source_profile,
                yolo_mode,
                command,
                seed_history_replay,
                fork_from,
            )) = acp_spawn_target
            {
                let agent = state
                    .acp_supervisor
                    .pick_agent_for_tool(
                        &tool,
                        agent_override.as_deref(),
                        &source_profile,
                        std::path::Path::new(&project_path),
                    )
                    .await;
                let command_override =
                    crate::server::acp_reconciler::command_override_for_spawn(&tool, &command);
                let cwd = std::path::PathBuf::from(project_path);
                let supervisor = state.acp_supervisor.clone();
                let state_for_check = state.clone();
                tokio::spawn(async move {
                    let inst_lock = state_for_check.instance_lock(&id).await;
                    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
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
                                target: "acp.supervisor",
                                session = %id,
                                "auto-spawn after create failed: {message}"
                            );
                            supervisor.publish_startup_error(&id, message);
                            return;
                        }
                    };
                    let source_profile_for_spawn = Some(source_profile.clone());
                    if let Err(e) = supervisor
                        .spawn(crate::acp::supervisor::SpawnRequest {
                            session_id: id.clone(),
                            agent: agent.clone(),
                            cwd,
                            additional_dirs: vec![],
                            provider_env: vec![],
                            model,
                            effort,
                            stored_acp_session_id,
                            fork_from,
                            sandbox_info,
                            source_profile: source_profile_for_spawn,
                            yolo_mode,
                            agent_command_override: command_override,
                            seed_history_replay,
                        })
                        .await
                    {
                        let still_present = state_for_check
                            .instances
                            .read()
                            .await
                            .iter()
                            .any(|i| i.id == id);
                        // Capacity-aware banner selection (and the benign
                        // first-tick duplicate) is documented on
                        // `structured_spawn_error_message`.
                        let message =
                            crate::server::api::structured_spawn_error_message(&e, &agent);
                        if still_present {
                            tracing::warn!(
                                target: "acp.supervisor",
                                session = %id,
                                "auto-spawn after create failed: {message}"
                            );
                            supervisor.publish_startup_error(&id, message);
                        } else {
                            tracing::debug!(
                                target: "acp.supervisor",
                                session = %id,
                                "auto-spawn after create error after session removed (ignored): {message}"
                            );
                        }
                    }
                });
            }

            (StatusCode::CREATED, Json(resp)).into_response()
        }
        #[cfg(not(feature = "serve"))]
        Ok(Ok((instance, warnings))) => {
            let mut resp = SessionResponse::from_instance(
                &instance,
                crate::claude_settings::read_tui_fullscreen(),
            );
            resp.warnings = warnings;
            let mut instances = state.instances.write().await;
            instances.push(instance);
            drop(instances);

            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Ok(Err(e)) => {
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
fn apply_post_restart_identity_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    // Treat the pre-restart snapshot as a CAS baseline for peer-writable
    // identity fields. If a poller/CLI/TUI peer changed the sid while the
    // restart clone was blocking, that newer sid and its marker stay
    // authoritative.
    let sid_unchanged = live.agent_session_id == before.agent_session_id;
    let marker_unchanged = live.resume_probe_failed_sid == before.resume_probe_failed_sid;
    if sid_unchanged {
        live.agent_session_id = started.agent_session_id.clone();
    }
    if marker_unchanged && live.agent_session_id == started.agent_session_id {
        live.resume_probe_failed_sid = started.resume_probe_failed_sid.clone();
    }
}

fn apply_post_restart_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    live.status = started.status;
    live.last_error = if started.status == Status::Error {
        started.last_error.clone()
    } else {
        None
    };
    live.last_error_check = started.last_error_check;
    apply_post_restart_identity_sync(live, before, started);
    live.last_start_time = started.last_start_time;
    live.retroactive_capture_excludes = started.retroactive_capture_excludes.clone();
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
fn apply_cascade_state_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    apply_post_restart_identity_sync(live, before, started);
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
///   * Probe failure (resume pane dies): Tier-1 returns Dead fast
///     (`pane_dead`/`!exists` is unambiguous), then `kill_clean` (~100ms
///     macOS grace) and a typed 409 response preserving the sid.
///
/// HTTP clients should budget ~3-4s worst-case for the resume probe and
/// configure timeouts accordingly.
pub async fn ensure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Serialize concurrent ensure calls for the same session. The decision
    // phase reads tmux state and the restart phase mutates it; any other
    // ensure for this id must wait so both see a consistent view.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

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

    let sync_base = instance.clone();
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
            // sync resume-path mutations back to live state. Otherwise the
            // live entry can retain stale marker/sid state until the next
            // `status_poll_loop` reload window (~2s). See
            // `apply_post_restart_sync`.
            //
            // `ensure_session` respawns on-demand before a WS attach/send,
            // the server-side analog of `ensure_pane_ready`: always `Allow`,
            // ignoring `auto_resume_on_restart`, so attaching doesn't
            // silently drop the agent's context. See #2609.
            match inst.start_with_resume_fallback(
                None,
                false,
                crate::session::ResumeAttemptPolicy::Allow,
            ) {
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
                apply_post_restart_sync(inst, &sync_base, &started);
            }
            let resume_outcome = match &outcome {
                crate::session::StartOutcome::Resumed => "resumed",
                crate::session::StartOutcome::ResumeFailed { .. } => "resume_failed",
                crate::session::StartOutcome::Fresh => "fresh",
                crate::session::StartOutcome::FreshAfterFailedResume { .. } => {
                    "fresh_after_failed_resume"
                }
            };
            let mut body = serde_json::json!({
                "status": "restarted",
                "resume_outcome": resume_outcome,
            });
            if let crate::session::StartOutcome::ResumeFailed { sid } = &outcome {
                body["status"] = serde_json::Value::String("resume_failed".to_string());
                body["error"] = serde_json::Value::String("resume_failed".to_string());
                body["message"] = serde_json::Value::String(format!(
                    "Resume failed for sid {sid}; preserved for explicit retry"
                ));
                body["resume_session_id"] = serde_json::Value::String(sid.clone());
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            if let crate::session::StartOutcome::FreshAfterFailedResume { sid } = &outcome {
                body["message"] = serde_json::Value::String(format!(
                    "Started fresh; a prior resume attempt failed for sid {sid}. \
                     The old conversation is still reachable via the agent's own \
                     resume/history picker."
                ));
                body["prior_session_id"] = serde_json::Value::String(sid.clone());
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "ensure_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                apply_post_restart_sync(inst, &sync_base, &started);
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
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let index = q.index;
    if index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
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
    // Index 0 has the in-memory `terminal_info.created` fast path; additional
    // terminals (index >= 1) are queried straight from tmux. Either way the
    // pane shell can exit (Ctrl+D, `exit`, SIGHUP from a destroyed tmux client,
    // etc.) while the session keeps existing (we set `remain-on-exit on`), so a
    // live-but-dead pane must be respawned the same way the TUI does on attach.
    {
        let instances = state.instances.read().await;
        if let Some(i) = instances.iter().find(|i| i.id == id) {
            let session = i.terminal_tmux_session_indexed(index).ok();
            let known = if index == 0 {
                i.has_terminal()
            } else {
                session.as_ref().map(|s| s.exists()).unwrap_or(false)
            };
            if known {
                let pane_dead = session
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
                    index,
                    "paired terminal pane is dead, respawning"
                );
            }
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_terminal_if_dead_indexed(index);
        inst_clone.start_terminal_with_size_indexed(index, None)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            // Only index 0 carries an in-memory cache flag.
            if index == 0 {
                let mut instances = state.instances.write().await;
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                    inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
                }
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
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let index = q.index;
    if index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
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
    // browser. Container terminals are always tmux-queried (no cache flag).
    {
        let instances = state.instances.read().await;
        if let Some(i) = instances.iter().find(|i| i.id == id) {
            let session = i.container_terminal_tmux_session_indexed(index).ok();
            if session.as_ref().map(|s| s.exists()).unwrap_or(false) {
                let pane_dead = session
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
                    index,
                    "container terminal pane is dead, respawning"
                );
            }
        }
    }

    let mut inst_clone = inst;

    let result = tokio::task::spawn_blocking(move || {
        let _ = inst_clone.kill_container_terminal_if_dead_indexed(index);
        inst_clone.start_container_terminal_with_size_indexed(index, None)
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

/// Kill an additional paired terminal (host + container) at `index`. Used when
/// the web dashboard closes an extra terminal tab so its tmux shell does not
/// leak for the session's lifetime. Index 0 is the primary terminal shared with
/// the native TUI; closing it in the web UI only hides the pane (the TUI keeps
/// its shell), so this endpoint rejects index 0. See #2437.
pub async fn kill_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    let index = q.index;
    if index == 0 || index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
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

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // A missing session is success (the `kill_*` helpers no-op when the
        // tmux session is absent); only a real tmux failure surfaces here, so
        // the caller can retry instead of leaving an orphaned shell behind.
        inst.kill_terminal_indexed(index)?;
        inst.kill_container_terminal_indexed(index)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "killed"})),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Terminal kill failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "kill_failed", "message": "Failed to kill terminal"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Terminal kill panicked: {}", e);
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

/// Contents-based diff response: raw old/new text that the web client parses
/// and renders itself via `@pierre/diffs`. See [`MAX_CONTENTS_BYTES`].
#[derive(Serialize)]
pub struct RichFileContentsResponse {
    pub file: RichDiffFileInfo,
    pub old_content: String,
    pub new_content: String,
    /// Server-computed unified diff of old → new. The client parses this as
    /// text (`parsePatchFiles`) instead of re-diffing the contents, which
    /// would block the main thread on large files. Empty for binary files.
    pub patch: String,
    pub is_binary: bool,
    /// True if the file was too large to send inline; contents are omitted.
    pub truncated: bool,
}

/// Caps for the contents-based diff endpoint. The client renders with a
/// virtualized, off-main-thread highlighter (`@pierre/diffs`), so the DOM and
/// main thread are no longer the bottleneck; the only real cost is JSON
/// payload size and the client-side parse. The byte cap is the real guard
/// against pathological payloads (minified bundles, generated code, data
/// blobs); the line cap is a secondary backstop.
const MAX_CONTENTS_BYTES: usize = 5_000_000;
const MAX_CONTENTS_LINES: usize = 200_000;

/// Validate a user-supplied relative file path against a workdir.
///
/// Returns `(canonical_path, is_changed)` if the requested path is safe to read
/// (no absolute, no `..`, no symlink-escape out of the workdir). `is_changed`
/// is true when the path appears in `changed_files` (diffable); false marks an
/// in-repo file with no diff against the base, served via the full-file
/// fallback (gated further on being a tracked blob; see
/// [`crate::git::diff::compute_unchanged_file_contents`]). See #1810.
///
/// A path that is neither in the changed set nor present on disk yields
/// `NOT_FOUND`. The non-canonical fallback is reserved for the changed-set case
/// (a file deleted in the working tree but still diffable); the unchanged
/// branch requires canonicalization to succeed. Returns `Err(status, message)`
/// otherwise.
fn validate_diff_path(
    workdir: &std::path::Path,
    requested: &std::path::Path,
    changed_files: &[crate::git::diff::DiffFile],
) -> Result<(std::path::PathBuf, bool), (StatusCode, &'static str)> {
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

    let is_changed = changed_files.iter().any(|f| f.path == requested);

    // Canonicalize both sides and verify containment as defense in depth
    // against symlinks that might point outside the workdir.
    let canonical_workdir = workdir.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "workdir canonicalize failed",
        )
    })?;
    let full = canonical_workdir.join(requested);
    match full.canonicalize() {
        Ok(c) => {
            if !c.starts_with(&canonical_workdir) {
                return Err((StatusCode::BAD_REQUEST, "path escapes workdir"));
            }
            Ok((c, is_changed))
        }
        // The file isn't on disk. A changed file may have been deleted in the
        // working tree but is still diffable, so fall back to the non-canonical
        // (component-vetted) path. An unchanged path that isn't on disk has
        // nothing to show.
        Err(_) if is_changed => Ok((full, true)),
        Err(_) => Err((StatusCode::NOT_FOUND, "file not found")),
    }
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
    /// The branch the worktree was created from, recorded at creation
    /// time. Slots below the explicit override but above the profile
    /// default and auto-detection. See #1951.
    base_from_worktree: Option<String>,
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
    let inst = instances
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(super::session_not_found)?;
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
        base_from_worktree: inst
            .worktree_info
            .as_ref()
            .and_then(|w| w.base_branch.clone()),
    })
}

/// Resolve the diff base for one repo path. Override (per-session)
/// wins over the worktree's recorded base, which wins over the
/// profile's `DiffConfig.default_branch`, which wins over
/// auto-detection (`get_default_base_ref`). See #970, #1951.
fn resolve_diff_base(
    override_value: Option<&str>,
    worktree_base: Option<&str>,
    config_default: Option<&str>,
    repo_path: &std::path::Path,
) -> String {
    if let Some(v) = override_value.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = worktree_base.map(str::trim).filter(|v| !v.is_empty()) {
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

    let scan_state = state.clone();
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
                ctx.base_from_worktree.as_deref(),
                config_default.as_deref(),
                path,
            );
            let warning = diff::check_merge_base_status(path, &base_branch);
            let changed = scan_state
                .changed_files_cached(path, &base_branch)
                .unwrap_or_default();

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
    let base_from_worktree = ctx.base_from_worktree.clone();
    let scan_state = state.clone();

    let result =
        tokio::task::spawn_blocking(move || -> Result<serde_json::Value, DiffFileError> {
            use crate::git::diff;

            let repo_path = std::path::Path::new(&project_path);
            let file_path = std::path::Path::new(&query.path);

            let config_default = crate::session::Config::load_or_warn()
                .diff
                .default_branch
                .clone();
            let base_branch = resolve_diff_base(
                base_branch_override.as_deref(),
                base_from_worktree.as_deref(),
                config_default.as_deref(),
                repo_path,
            );

            // Validate the requested path. Files in the changed set are diffed;
            // an in-repo file with no diff against the base is served through
            // the full-file fallback below. The path-traversal and containment
            // checks are the security boundary preventing arbitrary reads.
            let changed_files = scan_state
                .changed_files_cached(repo_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            let (canonical_path, is_changed) =
                match validate_diff_path(repo_path, file_path, &changed_files) {
                    Ok(v) => v,
                    Err((status, msg)) => {
                        return Err(if status == StatusCode::NOT_FOUND {
                            DiffFileError::NotFound(msg)
                        } else {
                            DiffFileError::BadRequest(msg)
                        });
                    }
                };

            // Full-file fallback: an agent-cited file with no diff against the
            // base. Render its current contents instead of a dead end. See #1810.
            if !is_changed {
                let full =
                    diff::compute_unchanged_file_contents(repo_path, file_path, &canonical_path)
                        .map_err(|e| DiffFileError::Internal(e.into()))?
                        .ok_or(DiffFileError::NotFound("file not found"))?;
                let file = RichDiffFileInfo {
                    path: query.path.clone(),
                    old_path: None,
                    status: "unchanged".to_string(),
                    additions: 0,
                    deletions: 0,
                    repo_name: selected_repo_name.clone(),
                };
                let total_lines = full.content.lines().count();
                let resp = if full.content.len() > MAX_CONTENTS_BYTES
                    || total_lines > MAX_CONTENTS_LINES
                {
                    RichFileContentsResponse {
                        file,
                        old_content: String::new(),
                        new_content: String::new(),
                        patch: String::new(),
                        is_binary: full.is_binary,
                        truncated: true,
                    }
                } else {
                    RichFileContentsResponse {
                        file,
                        old_content: String::new(),
                        new_content: full.content,
                        patch: String::new(),
                        is_binary: full.is_binary,
                        truncated: false,
                    }
                };
                return Ok(serde_json::to_value(resp)
                    .expect("RichFileContentsResponse is always serializable"));
            }

            // Hand the client raw old/new text plus a server-computed unified
            // patch. `@pierre/diffs` parses and renders that patch client-side
            // (virtualized, off-main-thread highlighting) without re-running
            // the diff algorithm in the browser.
            let contents = diff::compute_file_contents(repo_path, file_path, &base_branch)
                .map_err(|e| DiffFileError::Internal(e.into()))?;
            // additions/deletions aren't computed on this path; reuse the counts
            // the changed-files scan already produced for the sidebar.
            let (additions, deletions) = changed_files
                .iter()
                .find(|f| f.path == *file_path)
                .map(|f| (f.additions, f.deletions))
                .unwrap_or((0, 0));
            let file = RichDiffFileInfo {
                path: contents.path.to_string_lossy().to_string(),
                old_path: contents.old_path.map(|p| p.to_string_lossy().to_string()),
                status: contents.status.label().to_string(),
                additions,
                deletions,
                repo_name: selected_repo_name.clone(),
            };
            let total_bytes =
                contents.old_content.len() + contents.new_content.len() + contents.patch.len();
            let total_lines =
                contents.old_content.lines().count() + contents.new_content.lines().count();
            let resp = if total_bytes > MAX_CONTENTS_BYTES || total_lines > MAX_CONTENTS_LINES {
                RichFileContentsResponse {
                    file,
                    old_content: String::new(),
                    new_content: String::new(),
                    patch: String::new(),
                    is_binary: contents.is_binary,
                    truncated: true,
                }
            } else {
                RichFileContentsResponse {
                    file,
                    old_content: contents.old_content,
                    new_content: contents.new_content,
                    patch: contents.patch,
                    is_binary: contents.is_binary,
                    truncated: false,
                }
            };
            Ok(
                serde_json::to_value(resp)
                    .expect("RichFileContentsResponse is always serializable"),
            )
        })
        .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
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

#[derive(Deserialize)]
pub struct VolumeIgnoresPreviewQuery {
    pub path: String,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Serialize)]
pub struct VolumeIgnoresGlobPreview {
    pub pattern: String,
    pub matched_paths: Vec<String>,
}

#[derive(Serialize)]
pub struct VolumeIgnoresPreviewResponse {
    /// True once the user has acknowledged the snapshot-expansion behavior, so
    /// the wizard can skip the confirm modal without another round trip.
    pub acknowledged: bool,
    /// One entry per glob `volume_ignores` pattern with the directories it
    /// currently matches (container-side paths). Empty when none are configured.
    pub globs: Vec<VolumeIgnoresGlobPreview>,
}

/// Dry-run how glob `volume_ignores` entries would expand for a session rooted at
/// `path`, without creating anything. The wizard calls this before a sandbox
/// create to decide whether to show the snapshot-expansion confirm modal (#2045).
/// Read-only: no `read_only` guard needed.
pub async fn preview_volume_ignores_globs(
    axum::extract::Query(query): axum::extract::Query<VolumeIgnoresPreviewQuery>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let profile = query.profile.unwrap_or_default();
        let config = crate::session::repo_config::resolve_config_with_repo(
            &profile,
            std::path::Path::new(&query.path),
        )?;
        let expansions = crate::session::container_config::preview_glob_volume_ignores(
            &query.path,
            None,
            &config.sandbox.volume_ignores,
        )?;
        let acknowledged = crate::session::Config::load()
            .map(|c| c.app_state.has_acknowledged_volume_ignores_globs)
            .unwrap_or(false);
        Ok::<_, anyhow::Error>((acknowledged, expansions))
    })
    .await;

    match result {
        Ok(Ok((acknowledged, expansions))) => {
            let globs = expansions
                .into_iter()
                .map(|e| VolumeIgnoresGlobPreview {
                    pattern: e.pattern,
                    matched_paths: e.matched_container_paths,
                })
                .collect();
            (
                StatusCode::OK,
                Json(VolumeIgnoresPreviewResponse {
                    acknowledged,
                    globs,
                }),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sessions", "volume_ignores glob preview failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "preview_failed", "message": "Failed to preview volume_ignores"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "volume_ignores glob preview panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: String,
    pub snippet: String,
    pub match_count: usize,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
}

/// Full-text search over session conversation content (#2515). Scans the
/// structured-view event store on its read-only connection and returns
/// one hit per matching session, newest first. The response carries only
/// the session id; the web client already holds the session list and
/// resolves the title and state from it. Read-only; allowed in
/// `--read-only` mode.
pub async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> Json<SearchResponse> {
    let limit = q.limit.unwrap_or(10);
    // search_content does synchronous SQLite I/O plus JSON decoding; the
    // palette fires it repeatedly as the user types, so run it on the
    // blocking pool to keep slow scans off the Tokio worker threads.
    let store = Arc::clone(&state.acp_event_store);
    let query = q.q.clone();
    let results = tokio::task::spawn_blocking(move || {
        store
            .search_content(&query, limit)
            .into_iter()
            .map(|h| SearchHit {
                session_id: h.session_id,
                seq: h.seq,
                kind: h.kind.to_string(),
                snippet: h.snippet,
                match_count: h.match_count,
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    Json(SearchResponse { results })
}

/// Largest artifact the dashboard will serve inline. Generated screenshots
/// and status pages are small; the cap just bounds a pathological read.
const MAX_ARTIFACT_BYTES: u64 = 50 * 1024 * 1024;

/// Serve a file from a session's managed artifact directory
/// (`GET /api/sessions/{id}/artifacts/{*path}`). Auth is enforced by the
/// global middleware; `resolve_artifact_path` canonicalizes and confines the
/// request to the session's artifact root, so neither `..` nor a symlink can
/// escape it and arbitrary host paths are never served. HTML is sent as an
/// attachment (never inline) so a generated page cannot execute script in the
/// dashboard's authenticated origin. See #2587.
pub async fn serve_session_artifact(Path((id, path)): Path<(String, String)>) -> impl IntoResponse {
    let resolved = tokio::task::spawn_blocking(move || {
        crate::session::artifacts::resolve_artifact_path(&id, &path)
    })
    .await;

    let file_path = match resolved {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    match tokio::fs::metadata(&file_path).await {
        Ok(m) if m.len() > MAX_ARTIFACT_BYTES => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        Ok(_) => {}
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    let bytes = match tokio::fs::read(&file_path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    use axum::http::{header, HeaderMap, HeaderValue};
    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    let essence = mime.essence_str();
    // Any type that can execute script when opened as a top-level document is
    // served as a download, never inline. The frontend opens artifacts via
    // `window.open(blob:)`, and a blob URL inherits the dashboard's origin, so
    // an HTML/XHTML/SVG/XML artifact would otherwise run script in the
    // authenticated origin. Images and other passive types stay inline. See #2587.
    let force_download = matches!(
        essence,
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "application/xml" | "text/xml"
    );
    let content_type = if force_download {
        "application/octet-stream"
    } else {
        essence
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=60"),
    );
    if force_download {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }

    (StatusCode::OK, headers, bytes).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // #2587: the artifact route serves only canonicalized files confined to
    // the session's artifact dir, sets nosniff, and never serves HTML inline.
    mod artifact_route {
        use super::*;
        use crate::session::test_support::isolate_app_dir;
        use axum::body::to_bytes;
        use axum::extract::Path as AxumPath;
        use axum::http::header;
        use serial_test::serial;

        #[tokio::test]
        #[serial]
        async fn serves_image_with_nosniff() {
            let _tmp = isolate_app_dir();
            let id = format!("art-{}", uuid::Uuid::new_v4());
            let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
            std::fs::write(dir.join("shot.png"), b"\x89PNG\r\n").unwrap();
            let resp = serve_session_artifact(AxumPath((id, "shot.png".to_string())))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
                "nosniff"
            );
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                "image/png"
            );
        }

        #[tokio::test]
        #[serial]
        async fn rejects_traversal_with_empty_body() {
            let _tmp = isolate_app_dir();
            let id = format!("art-{}", uuid::Uuid::new_v4());
            crate::session::artifacts::session_artifact_dir(&id).unwrap();
            let resp = serve_session_artifact(AxumPath((id, "../../../../etc/hosts".to_string())))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            let body = to_bytes(resp.into_body(), 1024).await.unwrap();
            assert!(body.is_empty(), "unexpected body: {body:?}");
        }

        #[tokio::test]
        #[serial]
        async fn serves_svg_as_attachment() {
            // #2587: SVG can execute script as a top-level document, and the
            // frontend opens artifacts via a same-origin blob URL, so SVG must
            // download rather than render inline.
            let _tmp = isolate_app_dir();
            let id = format!("art-{}", uuid::Uuid::new_v4());
            let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
            std::fs::write(
                dir.join("d.svg"),
                b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
            )
            .unwrap();
            let resp = serve_session_artifact(AxumPath((id, "d.svg".to_string())))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/octet-stream"
            );
            assert_eq!(
                resp.headers().get(header::CONTENT_DISPOSITION).unwrap(),
                "attachment"
            );
        }

        #[tokio::test]
        #[serial]
        async fn serves_html_as_attachment() {
            let _tmp = isolate_app_dir();
            let id = format!("art-{}", uuid::Uuid::new_v4());
            let dir = crate::session::artifacts::session_artifact_dir(&id).unwrap();
            std::fs::write(dir.join("status.html"), b"<h1>hi</h1>").unwrap();
            let resp = serve_session_artifact(AxumPath((id, "status.html".to_string())))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/octet-stream"
            );
            assert_eq!(
                resp.headers().get(header::CONTENT_DISPOSITION).unwrap(),
                "attachment"
            );
        }
    }

    fn make_test_instance() -> Instance {
        let mut inst = Instance::new("test-session", "/tmp/test-project");
        inst.tool = "claude".to_string();
        inst.status = Status::Running;
        inst.group_path = "work/projects".to_string();
        inst
    }

    // Regression witness for #2603: the ACP-capability overlay and the
    // smart-rename indicator overlay share ONE per-request cache of the
    // resolved `SessionConfig` keyed by (profile, project_path). Three
    // instances covering two unique pairs must trigger exactly two calls
    // into `resolve_config_with_repo_or_warn`, not three (per row) and not
    // four (two independent per-overlay caches, the pre-#2603 state).
    // A non-built-in tool is used so the ACP overlay does not short-circuit
    // on the built-in registry (`SessionResponse` sets `acp_capable=true`
    // in the constructor for built-ins, which would skip the resolver
    // lookup and hide any regression in the ACP overlay).
    #[cfg(feature = "serve")]
    #[tokio::test]
    #[serial_test::serial]
    async fn list_sessions_shares_config_resolution_across_overlays() {
        use std::sync::atomic::Ordering;

        let tmp_home = tempfile::tempdir().expect("tempdir HOME");
        // SAFETY: serialized by `#[serial]`, matches other HOME-swapping tests.
        unsafe {
            std::env::set_var("HOME", tmp_home.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp_home.path().join(".config"));
        }

        let mk = |profile: &str, project_path: &str| {
            let mut inst = Instance::new("test-session", project_path);
            inst.tool = "custom-tool-2603".to_string();
            inst.source_profile = profile.to_string();
            inst
        };
        let a = mk("default", "/tmp/repo-a-2603");
        let a2 = mk("default", "/tmp/repo-a-2603");
        let b = mk("default", "/tmp/repo-b-2603");

        let state = crate::server::test_support::build_test_app_state(vec![a, a2, b]);

        LIST_SESSIONS_RESOLVER_MISSES.store(0, Ordering::Relaxed);
        let _envelope = list_sessions(axum::extract::State(state.clone())).await;
        let misses = LIST_SESSIONS_RESOLVER_MISSES.load(Ordering::Relaxed);

        assert_eq!(
            misses, 2,
            "shared cache must resolve exactly once per unique (profile, project_path) across both overlays; got {misses}",
        );
    }

    #[test]
    fn fork_from_builds_terminal_seed_for_claude() {
        // A non-structured (terminal) fork resolves through the shared
        // `terminal_fork_seed` helper; a claude parent id yields a Terminal
        // seed whose child id is a fresh, valid session id.
        let seed = resolve_create_fork_seed("claude", "parent-uuid", false)
            .expect("claude terminal fork allowed");
        match seed {
            crate::session::ForkSeed::Terminal {
                parent_agent_session_id,
                child_session_id,
            } => {
                assert_eq!(parent_agent_session_id, "parent-uuid");
                assert!(crate::session::capture::is_valid_session_id(
                    &child_session_id
                ));
            }
            _ => panic!("expected Terminal seed"),
        }
    }

    #[test]
    fn fork_from_builds_structured_seed_when_view_is_structured() {
        // A structured fork carries the parent's acp_session_id straight onto a
        // Structured seed; the builder turns that into the one-shot
        // fork_pending marker and the live session/fork handshake mints the
        // child id. The terminal forkability check is intentionally skipped.
        let seed = resolve_create_fork_seed("claude", "parent-acp-id", true)
            .expect("structured fork seed is always allowed at create time");
        assert_eq!(
            seed,
            crate::session::ForkSeed::Structured {
                parent_acp_session_id: "parent-acp-id".into(),
            }
        );
    }

    fn create_body_from_json(value: serde_json::Value) -> CreateSessionBody {
        serde_json::from_value(value).expect("valid CreateSessionBody")
    }

    #[test]
    fn worktree_enabled_true_opts_in_without_branch() {
        let body = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
            "worktree_enabled": true,
        }));

        assert!(create_body_uses_worktree(&body));
        assert!(body.worktree_branch.is_none());
    }

    #[test]
    fn worktree_branch_preserves_legacy_worktree_opt_in() {
        let explicit = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
            "worktree_branch": "feat/api",
        }));
        assert!(create_body_uses_worktree(&explicit));

        let empty = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
            "worktree_branch": "",
        }));
        assert!(create_body_uses_worktree(&empty));
    }

    #[test]
    fn worktree_defaults_off_without_flag_or_branch() {
        let body = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
        }));

        assert!(!create_body_uses_worktree(&body));
    }

    #[test]
    fn worktree_enabled_conflicts_with_scratch() {
        let body = create_body_from_json(serde_json::json!({
            "path": "",
            "tool": "claude",
            "scratch": true,
            "worktree_enabled": true,
        }));

        assert!(create_body_combines_scratch_and_worktree(&body));
    }

    #[test]
    fn both_import_and_fork_rejected() {
        // A request that sets both seeds the session from two contradictory
        // sources; the create handler rejects it before doing any work.
        let body = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
            "import_acp_session_id": "import-id",
            "fork_from": "parent-id",
        }));
        assert!(both_import_and_fork_set(&body));

        // Either alone is fine; trailing whitespace counts as unset.
        let import_only = create_body_from_json(serde_json::json!({
            "path": "/tmp/p", "tool": "claude", "import_acp_session_id": "import-id",
        }));
        assert!(!both_import_and_fork_set(&import_only));
        let fork_only = create_body_from_json(serde_json::json!({
            "path": "/tmp/p", "tool": "claude", "fork_from": "parent-id",
        }));
        assert!(!both_import_and_fork_set(&fork_only));
        let blank_fork = create_body_from_json(serde_json::json!({
            "path": "/tmp/p",
            "tool": "claude",
            "import_acp_session_id": "import-id",
            "fork_from": "   ",
        }));
        assert!(!both_import_and_fork_set(&blank_fork));
    }

    #[test]
    fn invalid_fork_id_is_rejected_by_create_guard() {
        // The create path gates `fork_from` on `is_valid_session_id` so a
        // malformed id can't slip through to `build_fork_flags`, which fails
        // closed (no fork flags) and would silently start a fresh session.
        use crate::session::capture::is_valid_session_id;
        assert!(!is_valid_session_id("../etc/passwd"));
        assert!(!is_valid_session_id("has spaces"));
        assert!(!is_valid_session_id("slash/id"));
        // A well-formed id still passes the same gate.
        assert!(is_valid_session_id("parent-uuid_123.v2"));
    }

    #[test]
    fn structured_fork_create_guard_matches_acp_can_fork() {
        // The create-time guard and the web `acp_can_fork` projection share
        // `agent_is_structured_fork_capable`, so they must agree per agent.
        // claude is ACP-capable with a real fork strategy: forkable.
        assert!(agent_is_structured_fork_capable("claude", None));
        // aoe-agent is ACP-capable but resume-only (no fork strategy), so the
        // create guard must reject a structured fork for it just as the web
        // suppresses the Fork affordance; gating on ACP-capability alone would
        // accept a create that can only fail later at the `session/fork`
        // handshake.
        assert!(!agent_is_structured_fork_capable("aoe-agent", None));
        // codex and opencode are ACP-registered AND declare a real terminal
        // ForkStrategy (used by the CLI `--fork-from` path), but neither ACP
        // adapter is verified to implement `session/fork`. Gating on
        // "fork_strategy != Unsupported" alone would report them forkable and
        // reproduce the same dead-end-handshake failure this function exists
        // to prevent for aoe-agent.
        assert!(!agent_is_structured_fork_capable("codex", None));
        assert!(!agent_is_structured_fork_capable("opencode", None));
        // A non-ACP tool is neither ACP-capable nor fork-capable.
        assert!(!agent_is_structured_fork_capable(
            "definitely-not-an-acp-agent",
            None
        ));

        // The two surfaces must report the same capability for each agent.
        for tool in [
            "claude",
            "aoe-agent",
            "codex",
            "opencode",
            "definitely-not-an-acp-agent",
        ] {
            let mut inst = make_test_instance();
            inst.tool = tool.to_string();
            assert_eq!(
                SessionResponse::from_instance(&inst, false).acp_can_fork,
                agent_is_structured_fork_capable(tool, None),
                "acp_can_fork and the create guard disagree for '{tool}'"
            );
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn acp_can_fork_tracks_acp_capable_and_fork_strategy() {
        // claude is ACP-capable AND declares a real fork strategy, so the web
        // gets a forkable signal.
        let mut claude = make_test_instance();
        claude.tool = "claude".to_string();
        assert!(SessionResponse::from_instance(&claude, false).acp_can_fork);

        // aoe-agent is ACP-capable (it is in the ACP registry) but declares no
        // fork strategy, so it is NOT forkable. Gating the web Fork action on
        // acp_session_id alone would offer a dead-end button for it; this is the
        // signal that suppresses that.
        let mut aoe_agent = make_test_instance();
        aoe_agent.tool = "aoe-agent".to_string();
        assert!(!SessionResponse::from_instance(&aoe_agent, false).acp_can_fork);

        // codex has a real terminal fork strategy but its ACP adapter is not
        // verified to implement `session/fork`, so the web signal must stay
        // false rather than offer a fork the live handshake would refuse.
        let mut codex = make_test_instance();
        codex.tool = "codex".to_string();
        assert!(!SessionResponse::from_instance(&codex, false).acp_can_fork);

        // A non-ACP agent is neither ACP-capable nor fork-capable.
        let mut other = make_test_instance();
        other.tool = "definitely-not-an-acp-agent".to_string();
        assert!(!SessionResponse::from_instance(&other, false).acp_can_fork);
    }

    #[test]
    fn trash_body_default_keeps_kill_pane_true() {
        // #2523: a no-body trash request resolves through
        // `unwrap_or_default()`. The derived `Default` would yield
        // `kill_pane = false` and leave the pane running; the hand impl must
        // match the serde field default.
        assert!(TrashSessionBody::default().kill_pane);

        // An empty JSON object goes through serde, which honors the field
        // default helper.
        let from_empty: TrashSessionBody = serde_json::from_str("{}").unwrap();
        assert!(from_empty.kill_pane);

        // An explicit `false` is still respected.
        let explicit: TrashSessionBody = serde_json::from_str(r#"{"kill_pane": false}"#).unwrap();
        assert!(!explicit.kill_pane);
    }

    #[test]
    fn upsert_instance_replaces_same_id_instead_of_duplicating() {
        // Race regression: `create_session` persists to disk before pushing
        // the in-memory copy, so a `status_poll_loop` tick can load the row
        // and insert it first. The handler's insert must replace that entry,
        // not append a second one with the same id.
        let poll_loaded = make_test_instance();
        let id = poll_loaded.id.clone();
        let mut instances = vec![poll_loaded];

        let mut handler_copy = make_test_instance();
        handler_copy.id = id.clone();
        handler_copy.status = Status::Starting;

        upsert_instance(&mut instances, handler_copy);

        assert_eq!(
            instances.len(),
            1,
            "same id must not duplicate in the registry"
        );
        assert_eq!(instances[0].id, id);
        assert_eq!(
            instances[0].status,
            Status::Starting,
            "handler copy must win"
        );
    }

    #[test]
    fn upsert_instance_appends_a_new_id() {
        let mut instances = vec![make_test_instance()];
        let other = Instance::new("other-session", "/tmp/other-project");
        let other_id = other.id.clone();
        upsert_instance(&mut instances, other);
        assert_eq!(instances.len(), 2);
        assert!(instances.iter().any(|i| i.id == other_id));
    }

    // Regression for #2363: a multi-repo workspace session carries
    // `workspace_info` and no `worktree_info`. The DTO must report
    // `has_cleanable_worktree: true` so the web delete dialog shows the
    // "Delete worktree" checkbox, while keeping `has_managed_worktree: false`
    // so worktree-only actions (sidebar "Edit workdir name", tie overlay) stay
    // hidden for workspace sessions.
    #[test]
    fn from_instance_reports_managed_worktree_for_workspace_session() {
        let mut inst = make_test_instance();
        inst.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: "/tmp/ws".to_string(),
            repos: vec![crate::session::WorkspaceRepo {
                name: "repo-a".to_string(),
                source_path: "/tmp/src/repo-a".to_string(),
                branch: "feature/abc".to_string(),
                worktree_path: "/tmp/ws/repo-a".to_string(),
                main_repo_path: "/tmp/src/repo-a".to_string(),
                managed_by_aoe: true,
            }],
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });

        let resp = SessionResponse::from_instance(&inst, false);
        assert!(
            resp.has_cleanable_worktree,
            "workspace session must report a cleanable worktree so the delete checkbox shows"
        );
        assert!(
            !resp.has_managed_worktree,
            "workspace session must NOT report a single-repo managed worktree (keeps Edit-workdir hidden)"
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn from_instance_surfaces_hook_urgent_flag() {
        // #1640: the web Attention sort needs `Instance::is_urgent()` on the
        // wire. Write the hook-side attention.json the agent would emit and
        // confirm it round-trips onto the response, then confirm a session
        // with no hook file reports urgent: false.
        let (_g, _, _tmp_base) = crate::hooks::test_support::BaseGuard::ready();
        let inst = make_test_instance();
        let dir = crate::hooks::ensure_instance_dir_path(&inst.id)
            .expect("guard must create instance subdir");
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
    fn resolve_diff_base_prefers_override_then_worktree_then_config_then_auto() {
        let tmp = tempfile::tempdir().unwrap();
        // Override wins over everything.
        assert_eq!(
            resolve_diff_base(Some("release-1.2"), None, Some("develop"), tmp.path()),
            "release-1.2"
        );
        // Worktree base wins after override; whitespace override falls through.
        assert_eq!(
            resolve_diff_base(
                Some("   "),
                Some("worktree-base"),
                Some("develop"),
                tmp.path()
            ),
            "worktree-base"
        );
        // Config wins when no override and no worktree base.
        assert_eq!(
            resolve_diff_base(None, None, Some("develop"), tmp.path()),
            "develop"
        );
        // Auto-detect when nothing is set. The tmp dir is not a repo so
        // `get_default_base_ref` returns Err -> "main" fallback.
        assert_eq!(resolve_diff_base(None, None, None, tmp.path()), "main");
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
        let before = live.clone();

        let mut started = make_test_instance();
        started.status = Status::Starting;
        started.agent_session_id = Some("claude-uuid-restart".to_string());
        started.last_start_time = Some(std::time::Instant::now());

        apply_post_restart_sync(&mut live, &before, &started);

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
        let before = live.clone();

        let mut started = make_test_instance();
        started.agent_session_id = Some("fresh-id".to_string());

        apply_post_restart_sync(&mut live, &before, &started);

        assert_eq!(live.agent_session_id.as_deref(), Some("fresh-id"));
    }

    #[test]
    fn apply_post_restart_sync_propagates_resume_failed_marker_and_error() {
        let mut live = make_test_instance();
        live.status = Status::Running;
        live.last_error = Some("prior failure".to_string());
        live.agent_session_id = Some("sid-before".to_string());
        live.resume_probe_failed_sid = None;
        let before = live.clone();

        let mut started = make_test_instance();
        started.status = Status::Error;
        started.agent_session_id = Some("sid-after".to_string());
        started.resume_probe_failed_sid = Some("sid-after".to_string());
        started.last_error =
            Some("resume failed for sid sid-after; preserved for explicit retry".to_string());
        started.last_error_check = Some(std::time::Instant::now());

        apply_post_restart_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Error);
        assert_eq!(
            live.last_error.as_deref(),
            Some("resume failed for sid sid-after; preserved for explicit retry")
        );
        assert!(live.last_error_check.is_some());
        assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
        assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
    }

    #[test]
    fn apply_cascade_state_sync_propagates_marker_without_status() {
        let mut live = make_test_instance();
        live.status = Status::Running;
        live.last_error = Some("keep me".to_string());
        live.agent_session_id = Some("sid-before".to_string());
        live.resume_probe_failed_sid = None;
        let before = live.clone();

        let mut started = make_test_instance();
        started.status = Status::Error;
        started.last_error = Some("resume failed".to_string());
        started.agent_session_id = Some("sid-after".to_string());
        started.resume_probe_failed_sid = Some("sid-after".to_string());

        apply_cascade_state_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Running);
        assert_eq!(live.last_error.as_deref(), Some("keep me"));
        assert_eq!(live.agent_session_id.as_deref(), Some("sid-after"));
        assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("sid-after"));
    }

    #[test]
    fn apply_post_restart_sync_preserves_peer_sid_write() {
        let mut before = make_test_instance();
        before.agent_session_id = Some("stale-restart-sid".to_string());
        before.resume_probe_failed_sid = None;

        let mut live = make_test_instance();
        live.agent_session_id = Some("peer-fresh-sid".to_string());
        live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

        let mut started = make_test_instance();
        started.status = Status::Error;
        started.agent_session_id = Some("stale-restart-sid".to_string());
        started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());
        started.last_error = Some("resume failed".to_string());

        apply_post_restart_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Error);
        assert_eq!(live.last_error.as_deref(), Some("resume failed"));
        assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
        assert_eq!(
            live.resume_probe_failed_sid.as_deref(),
            Some("peer-fresh-sid")
        );
    }

    #[test]
    fn apply_post_restart_sync_preserves_peer_marker_for_same_sid() {
        let mut before = make_test_instance();
        before.agent_session_id = Some("same-sid".to_string());
        before.resume_probe_failed_sid = None;

        let mut live = before.clone();
        live.resume_probe_failed_sid = Some("same-sid".to_string());

        let mut started = before.clone();
        started.status = Status::Starting;
        started.resume_probe_failed_sid = None;

        apply_post_restart_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Starting);
        assert_eq!(live.agent_session_id.as_deref(), Some("same-sid"));
        assert_eq!(live.resume_probe_failed_sid.as_deref(), Some("same-sid"));
    }

    #[test]
    fn apply_cascade_state_sync_preserves_peer_sid_write() {
        let mut before = make_test_instance();
        before.agent_session_id = Some("stale-restart-sid".to_string());
        before.resume_probe_failed_sid = None;

        let mut live = make_test_instance();
        live.status = Status::Running;
        live.last_error = Some("keep me".to_string());
        live.agent_session_id = Some("peer-fresh-sid".to_string());
        live.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());

        let mut started = make_test_instance();
        started.status = Status::Error;
        started.last_error = Some("resume failed".to_string());
        started.agent_session_id = Some("stale-restart-sid".to_string());
        started.resume_probe_failed_sid = Some("stale-restart-sid".to_string());

        apply_cascade_state_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Running);
        assert_eq!(live.last_error.as_deref(), Some("keep me"));
        assert_eq!(live.agent_session_id.as_deref(), Some("peer-fresh-sid"));
        assert_eq!(
            live.resume_probe_failed_sid.as_deref(),
            Some("peer-fresh-sid")
        );
    }

    #[test]
    #[serial_test::serial]
    fn send_message_post_restart_save_preserves_peer_sid_write() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _ = isolated_app_dir(temp_home.path());

        let profile = "send-post-restart-peer-sid";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut seed = make_test_instance();
        let id = seed.id.clone();
        seed.agent_session_id = Some("peer-fresh-sid".to_string());
        seed.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());
        storage
            .update(|instances, _groups| {
                instances.push(seed.clone());
                Ok(())
            })
            .unwrap();

        let mut sync_base_for_save = make_test_instance();
        sync_base_for_save.id = id.clone();
        sync_base_for_save.agent_session_id = Some("stale-restart-sid".to_string());
        sync_base_for_save.resume_probe_failed_sid = None;

        let mut started_for_save = make_test_instance();
        started_for_save.id = id.clone();
        started_for_save.status = Status::Starting;
        started_for_save.agent_session_id = Some("stale-restart-sid".to_string());
        started_for_save.resume_probe_failed_sid = None;

        storage
            .update(|all, _groups| {
                if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id) {
                    apply_post_restart_sync(disk_inst, &sync_base_for_save, &started_for_save);
                    disk_inst.touch_last_accessed();
                }
                Ok(())
            })
            .unwrap();

        let reloaded = storage.load().unwrap();
        let disk = reloaded.iter().find(|i| i.id == seed.id).unwrap();
        assert_eq!(disk.status, Status::Starting);
        assert_eq!(disk.agent_session_id.as_deref(), Some("peer-fresh-sid"));
        assert_eq!(
            disk.resume_probe_failed_sid.as_deref(),
            Some("peer-fresh-sid")
        );
        assert!(disk.last_accessed_at.is_some());
    }

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let config_home = temp_home.join(".config");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            config_home.join(crate::session::APP_DIR_NAME_XDG)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

    #[test]
    fn ensure_session_refreshes_instance_after_instance_lock() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions.rs"),
        )
        .unwrap();
        let start = source.find("pub async fn ensure_session").unwrap();
        let end = source.find("pub async fn ensure_terminal").unwrap();
        let ensure_source = &source[start..end];
        let lock = ensure_source
            .find("let inst_lock = state.instance_lock(&id).await")
            .unwrap();
        let read = ensure_source
            .find("let instances = state.instances.read().await")
            .unwrap();
        let sync_base = ensure_source
            .find("let sync_base = instance.clone()")
            .unwrap();

        assert!(lock < read);
        assert!(read < sync_base);
    }

    #[test]
    fn send_message_refreshes_instance_after_instance_lock() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions.rs"),
        )
        .unwrap();
        let start = source.find("pub async fn send_message").unwrap();
        let send_source = &source[start..];
        let lock = send_source
            .find("let inst_lock = state.instance_lock(&id).await")
            .unwrap();
        let read = send_source
            .find("let instances = state.instances.read().await")
            .unwrap();
        let sync_base = send_source
            .find("let sync_base = instance.clone()")
            .unwrap();

        assert!(lock < read);
        assert!(read < sync_base);
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
    fn validate_diff_path_accepts_unchanged_existing_file() {
        // An in-repo file that exists on disk but is not in the changed set is
        // now accepted for the full-file fallback (#1810), flagged
        // `is_changed = false`. The tracked-blob gate that blocks `.git/` and
        // gitignored secrets lives in compute_unchanged_file_contents, not here.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
        let (_, is_changed) = validate_diff_path(
            dir.path(),
            std::path::Path::new("existing.txt"),
            &changed(&["src/main.rs"]),
        )
        .unwrap();
        assert!(!is_changed);
    }

    #[test]
    fn validate_diff_path_rejects_nonexistent_unchanged_file() {
        // Not in the changed set and not on disk: nothing to show.
        let dir = TempDir::new().unwrap();
        let err = validate_diff_path(
            dir.path(),
            std::path::Path::new("ghost.txt"),
            &changed(&["src/main.rs"]),
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn validate_diff_path_accepts_changed_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("changed.txt"), "hello").unwrap();
        let (_, is_changed) = validate_diff_path(
            dir.path(),
            std::path::Path::new("changed.txt"),
            &changed(&["changed.txt"]),
        )
        .unwrap();
        assert!(is_changed);
    }

    #[test]
    fn validate_diff_path_accepts_deleted_file() {
        // A file that has been deleted on disk but is in the changed set
        // (status: Deleted) should still be diffable so the user can see
        // what was removed. canonicalize() on the joined path will fail,
        // so the validator must fall back to the non-canonical path.
        let dir = TempDir::new().unwrap();
        let (_, is_changed) = validate_diff_path(
            dir.path(),
            std::path::Path::new("deleted.txt"),
            &changed(&["deleted.txt"]),
        )
        .unwrap();
        assert!(is_changed);
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

    #[test]
    fn session_response_serializes_unread_marker() {
        use crate::session::Instance;
        let mut inst = Instance::new("t", "/tmp");
        // Read: the field is omitted from the wire (skip_serializing_if false).
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert!(json.get("unread").is_none());
        // Unread serializes as a bare boolean the web reads directly.
        inst.unread = true;
        let json = serde_json::to_value(SessionResponse::from_instance(&inst, false)).unwrap();
        assert_eq!(json["unread"], serde_json::json!(true));
    }

    fn step(
        id: &str,
        title: &str,
        status: crate::acp::state::PlanStepStatus,
    ) -> crate::acp::state::PlanStep {
        crate::acp::state::PlanStep {
            id: id.into(),
            title: title.into(),
            detail: None,
            status,
        }
    }

    #[test]
    fn plan_summary_counts_done_steps_only() {
        use crate::acp::state::PlanStepStatus::*;
        let plan = crate::acp::state::Plan {
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
        use crate::acp::state::PlanStepStatus::*;
        // First non-Done is the first Pending; InProgress later doesn't
        // override (matches the helper's `find(..)` semantics).
        let plan = crate::acp::state::Plan {
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
        use crate::acp::state::PlanStepStatus::*;
        let plan = crate::acp::state::Plan {
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
        use crate::acp::state::PlanStepStatus::*;
        let long_title: String = "x".repeat(120);
        let plan = crate::acp::state::Plan {
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
        let plan = crate::acp::state::Plan {
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
        let storage = Storage::new_unwatched(profile).unwrap();
        let seed = make_test_instance();
        let id = seed.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed.clone());
                Ok(())
            })
            .unwrap();

        let persist_id = id.clone();
        persist_session_update(
            profile.to_string(),
            "test",
            crate::file_watch::FileWatchService::noop(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    inst.base_branch_override = Some("release/x".to_string());
                }
            },
        )
        .await
        .expect("persist should succeed");

        let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
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

        let result = persist_session_update(
            profile.to_string(),
            "test",
            crate::file_watch::FileWatchService::noop(),
            |_instances| {},
        )
        .await;
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
        let storage = Storage::new_unwatched(profile).unwrap();
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
        persist_session_update(
            profile.to_string(),
            "group update",
            crate::file_watch::FileWatchService::noop(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == set_id) {
                    apply_session_group(inst, "team/alpha".to_string());
                }
            },
        )
        .await
        .expect("set should succeed");

        let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(
            reloaded.iter().find(|i| i.id == id).unwrap().group_path,
            "team/alpha",
            "group must move to the new path on disk"
        );

        // Clear to ungrouped via the empty-string sentinel.
        let clear_id = id.clone();
        persist_session_update(
            profile.to_string(),
            "group update",
            crate::file_watch::FileWatchService::noop(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == clear_id) {
                    apply_session_group(inst, String::new());
                }
            },
        )
        .await
        .expect("clear should succeed");

        let reloaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(
            reloaded.iter().find(|i| i.id == id).unwrap().group_path,
            "",
            "empty string must clear the group on disk"
        );
    }

    // --- #2066: web-API on_create hook trust + execution ---

    /// Write `.agent-of-empires/config.toml` with the given `on_create` hooks
    /// into a fresh project dir. Returns the dir so the caller keeps it alive.
    fn project_with_on_create_hooks(commands: &[&str]) -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        let cfg_dir = project.path().join(".agent-of-empires");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let list = commands
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            cfg_dir.join("config.toml"),
            format!("[hooks]\non_create = [{list}]\n"),
        )
        .unwrap();
        project
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_refuses_untrusted_repo_hooks() {
        // Bug #2066: the web API used to skip hooks entirely. The plan must now
        // refuse an untrusted repo with hooks unless trust_hooks is passed, so
        // the caller can prompt rather than silently get an un-bootstrapped
        // worktree.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = project_with_on_create_hooks(&["bash scripts/setup-worktree.sh"]);
        // Approval trusts the whole hooks hash, so the refusal must surface
        // every hook type, not just on_create.
        std::fs::write(
            project.path().join(".agent-of-empires/config.toml"),
            "[hooks]\non_create = [\"bash scripts/setup-worktree.sh\"]\non_launch = [\"npm start\"]\non_destroy = [\"rm -rf /tmp/seed\"]\n",
        )
        .unwrap();

        let err = resolve_create_hook_plan("default", project.path(), false, false)
            .expect_err("untrusted hooks must be refused");
        let needs_trust = err
            .downcast_ref::<HooksNeedTrust>()
            .expect("error must be HooksNeedTrust");
        assert_eq!(
            needs_trust.on_create,
            vec!["bash scripts/setup-worktree.sh".to_string()],
            "the refused error must carry the commands for the prompt"
        );
        assert_eq!(
            needs_trust.on_launch,
            vec!["npm start".to_string()],
            "approval also trusts on_launch, so the prompt must show it"
        );
        assert_eq!(needs_trust.on_destroy, vec!["rm -rf /tmp/seed".to_string()]);
        assert!(!needs_trust.needs_mcp_trust);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_trusts_and_runs_with_trust_hooks() {
        // trust_hooks: true mirrors the CLI --trust-hooks flag: approve, record
        // trust, and return the commands to run.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = project_with_on_create_hooks(&["echo hi"]);

        let plan = resolve_create_hook_plan("default", project.path(), false, true)
            .expect("trust_hooks: true must approve");
        assert_eq!(plan.on_create, vec!["echo hi".to_string()]);
        let (hooks_hash, mcp_hash) = plan
            .trust_write
            .expect("a newly-approved repo must record trust");
        assert!(hooks_hash.is_some(), "hooks hash must be recorded");
        assert!(mcp_hash.is_none(), "no .mcp.json means no mcp hash");

        // And the recorded trust makes a later create succeed without opting in.
        crate::session::repo_config::trust_repo(
            project.path(),
            hooks_hash.as_deref(),
            mcp_hash.as_deref(),
        )
        .unwrap();
        let plan2 = resolve_create_hook_plan("default", project.path(), false, false)
            .expect("already-trusted hooks must run without trust_hooks");
        assert_eq!(plan2.on_create, vec!["echo hi".to_string()]);
        assert!(
            plan2.trust_write.is_none(),
            "already-trusted repo needs no new trust record"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_absent_hooks_is_ok() {
        // A repo with no hooks (and no global hooks) is never refused.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = tempfile::tempdir().unwrap();

        let plan = resolve_create_hook_plan("default", project.path(), false, false)
            .expect("no hooks means no trust needed");
        assert!(plan.on_create.is_empty());
        assert!(plan.trust_write.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_scratch_skips_repo_trust() {
        // Scratch sessions have no repo config anchor; even pointing at a path
        // with untrusted hooks must not refuse (matches the CLI scratch branch).
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = project_with_on_create_hooks(&["echo nope"]);

        let plan = resolve_create_hook_plan("default", project.path(), true, false)
            .expect("scratch must skip the repo trust check");
        assert!(
            plan.on_create.is_empty(),
            "no global hooks, so scratch resolves to nothing"
        );
        assert!(plan.trust_write.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_does_not_block_on_untrusted_mcp_without_hooks() {
        // A repo with an untrusted `.mcp.json` but no hooks must NOT be refused:
        // the supervisor gates MCP at spawn, so blocking creation here would be
        // stricter than the CLI. The session is created with MCP left untrusted.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers": {"foo": {"command": "echo"}}}"#,
        )
        .unwrap();

        let plan = resolve_create_hook_plan("default", project.path(), false, false)
            .expect("untrusted MCP without hooks must not block creation");
        assert!(plan.on_create.is_empty());
        assert!(
            plan.trust_write.is_none(),
            "MCP is left untrusted when the caller did not opt in"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_hook_plan_inherits_trust_across_worktrees() {
        // Secondary half of #2066: hook trust is keyed on the main repo
        // (check_repo_trust resolves a worktree path back to it), so a worktree
        // created from an already-trusted repo inherits that trust without a
        // fresh prompt, even with trust_hooks: false.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _app_dir = isolated_app_dir(temp_home.path());

        let parent = tempfile::Builder::new()
            .prefix("aoe-test-")
            .tempdir()
            .unwrap();
        let root = parent.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        std::fs::create_dir_all(root.join(".agent-of-empires")).unwrap();
        std::fs::write(
            root.join(".agent-of-empires/config.toml"),
            "[hooks]\non_create = [\"echo wt\"]\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "proj\n").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Trust the main repo at its current hooks hash.
        let hooks = crate::session::repo_config::load_repo_config(&root)
            .unwrap()
            .and_then(|rc| rc.hooks())
            .unwrap();
        let hash = crate::session::repo_config::compute_hooks_hash(&hooks);
        crate::session::repo_config::trust_repo(&root, Some(&hash), None).unwrap();

        // A worktree of that repo inherits the trust.
        let main_wt = crate::git::GitWorktree::new(root.clone()).unwrap();
        let wt_path = parent.path().join("proj-wt");
        main_wt
            .create_worktree("wt-branch", &wt_path, true, None)
            .unwrap();

        let plan = resolve_create_hook_plan("default", &wt_path, false, false)
            .expect("worktree must inherit the main repo's hook trust");
        assert_eq!(plan.on_create, vec!["echo wt".to_string()]);
        assert!(
            plan.trust_write.is_none(),
            "inherited trust needs no new record"
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
    ResumeFailed(String),
    Transient(Status),
    StructuredView,
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
        return super::read_only_response();
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

    // Serialize concurrent sends (and other tmux mutations) for this id.
    // Without this, two POSTs racing against the same session would issue
    // overlapping `tmux send-keys -l` invocations and the bytes can interleave
    // inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let sync_base = instance.clone();
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
        // post-resume-path mutations (`agent_session_id`, failure marker,
        // and `retroactive_capture_excludes`) back to live state regardless
        // of which failure path fires. The
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
                        EnsureReadyError::StructuredView => SendKeysError::StructuredView,
                        EnsureReadyError::Tmux(e) => SendKeysError::Tmux(e),
                    };
                    // ensure_pane_ready did not mutate user-visible
                    // state via the outcome path. Tag as AlreadyAlive
                    // so the outer match's `did_work` flag stays
                    // false. `EnsureReadyError::Tmux` may be either
                    // pre-cascade (tmux_session() / start_with_size
                    // subprocess failure: `inst_owned` unmutated) or
                    // post-resume-path (mutations committed).
                    // The Tmux outer arm syncs unconditionally and
                    // covers both shapes; the others (Transient /
                    // StructuredView) bail before any mutation.
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
        if let EnsureReadyOutcome::ResumeFailed { sid } = &outcome {
            return Err(Box::new((
                inst_owned,
                outcome.clone(),
                SendKeysError::ResumeFailed(sid.clone()),
            )));
        }
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
                    apply_post_restart_sync(i, &sync_base, &started);
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
            let sync_base_for_save = sync_base.clone();
            let started_for_save = started.clone();
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile, state.file_watch.clone()) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id_for_save) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(
                                    disk_inst,
                                    &sync_base_for_save,
                                    &started_for_save,
                                );
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            });
            (StatusCode::OK, Json(serde_json::json!({"sent": true}))).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // ensure_pane_ready did mutate state when the outcome is
            // anything other than AlreadyAlive. `Started` and `Respawned`
            // touch fields the live entry needs to reflect (fresh sid from
            // acquire, last_start_time, etc.). Sync only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match send_err {
                SendKeysError::NotRunning => {
                    // External kill or remain-on-exit-off crash can race
                    // ensure_pane_ready's Alive decision against the
                    // tmux_session.exists() check. Propagate resume-path
                    // state when applicable; use the narrow sync helper to
                    // leave status and last_error untouched (NotRunning is
                    // recoverable; `started.status = Starting` from
                    // finalize_launch would briefly mis-paint a broken pane).
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": "session_not_running"})),
                    )
                        .into_response()
                }
                SendKeysError::ResumeFailed(sid) => {
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &sync_base, &started);
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "resume_failed",
                            "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                            "resume_session_id": sid,
                        })),
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
                SendKeysError::StructuredView => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "acp_mode_unsupported"})),
                )
                    .into_response(),
                SendKeysError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", "send_message: tmux error for {id}: {e}");
                    let msg = e.to_string();
                    // Sync cascade-mutated fields back to live state. Mirror
                    // `ensure_session`'s Err arm: full sync, then override
                    // `status` and `last_error` so observers don't see
                    // `Status::Starting` (set by `finalize_launch`) on a
                    // broken session. Tmux Err is the
                    // catch-all for both pre-cascade tmux failures (where
                    // `started` is unmutated and the sync is a no-op) and
                    // post-resume-path failures (where durable resume state
                    // must be copied back from the clone).
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &sync_base, &started);
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

/// Max decoded size of a pasted image (5 MiB). Claude Code caps image
/// attachments around this size; the route body limit in `build_router`
/// leaves headroom for base64's ~33% overhead plus JSON framing.
const MAX_PASTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Directory, relative to the session worktree, holding images pasted into
/// the live terminal. It lives inside the worktree so a Docker-sandboxed
/// pane, which mounts the worktree but cannot see the host temp dir, can
/// still read the file. A self-ignoring `.gitignore` keeps the blobs out of
/// git. See #2678.
const PASTE_IMAGE_DIR: &str = ".aoe-pasted-images";

#[derive(Deserialize)]
pub struct PasteImageRequest {
    /// Client-declared MIME. Advisory only: the extension and the
    /// accept/reject decision come from magic-byte sniffing, never this field.
    #[serde(default)]
    pub mime_type: String,
    /// Standard-base64 image bytes.
    pub data: String,
}

fn paste_image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Write the decoded blob into the worktree's paste-image dir and return the
/// host path plus the generated file name. Sync (filesystem I/O); call from a
/// blocking pool.
fn write_paste_image(
    project_path: &str,
    bytes: &[u8],
    ext: &str,
) -> std::io::Result<(std::path::PathBuf, String)> {
    let dir = std::path::Path::new(project_path).join(PASTE_IMAGE_DIR);
    std::fs::create_dir_all(&dir)?;
    // A `.gitignore` of `*` also ignores itself, so the whole directory stays
    // invisible to `git add` with no git subprocess.
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    let file_name = format!("aoe-paste-{}.{}", uuid::Uuid::new_v4(), ext);
    let path = dir.join(&file_name);
    // create_new: uuid names never collide; fail loud if the impossible happens.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    Ok((path, file_name))
}

/// Map the host paste-image file to the path the tmux pane reads. Non-sandboxed
/// panes share the host filesystem, so the absolute host path is correct. A
/// sandboxed pane mounts the worktree under a container path (`/workspace/...`);
/// reuse `compute_volume_paths` so the pasted path matches that mount.
fn pane_visible_paste_path(project_path: &str, is_sandboxed: bool, file_name: &str) -> String {
    if is_sandboxed {
        if let Ok((_, working_dir)) = crate::session::container_config::compute_volume_paths(
            std::path::Path::new(project_path),
            project_path,
        ) {
            return format!("{working_dir}/{PASTE_IMAGE_DIR}/{file_name}");
        }
    }
    std::path::Path::new(project_path)
        .join(PASTE_IMAGE_DIR)
        .join(file_name)
        .to_string_lossy()
        .to_string()
}

/// Save a clipboard image pasted into the live terminal and return the path
/// the tmux pane can read, so the CLI agent (e.g. Claude Code) attaches it.
/// See #2678.
pub async fn paste_image(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PasteImageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    use base64::Engine as _;

    if state.read_only {
        return super::read_only_response();
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
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

    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data.as_bytes()) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_base64"})),
            )
                .into_response();
        }
    };
    if bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "empty"})),
        )
            .into_response();
    }
    if bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "too_large"})),
        )
            .into_response();
    }
    let Some(mime) = super::acp::sniff_image_mime(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "not_an_image"})),
        )
            .into_response();
    };
    let ext = paste_image_extension(mime);

    let project_path = instance.project_path.clone();
    let is_sandboxed = instance.is_sandboxed();
    let write_project = project_path.clone();
    let (host_path, file_name) =
        match tokio::task::spawn_blocking(move || write_paste_image(&write_project, &bytes, ext))
            .await
        {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: write failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
        };

    // Best-effort TTL cleanup: the file only needs to outlive the agent
    // reading it. A detached task keeps the worktree from accumulating blobs
    // without any teardown bookkeeping.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        let _ = tokio::fs::remove_file(&host_path).await;
    });

    let pane_path = pane_visible_paste_path(&project_path, is_sandboxed, &file_name);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "path": pane_path })),
    )
        .into_response()
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
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            #[cfg(feature = "serve")]
            view: crate::session::View::Terminal,
            #[cfg(feature = "serve")]
            acp_worker_state: crate::acp::supervisor::AcpWorkerState::Absent,
            #[cfg(feature = "serve")]
            acp_capable: false,
            #[cfg(feature = "serve")]
            acp_session_id: None,
            #[cfg(feature = "serve")]
            acp_agent: None,
            #[cfg(feature = "serve")]
            acp_can_fork: false,
            claude_fullscreen: false,
            workspace_repos: Vec::new(),
            warnings: Vec::new(),
            plan_summary: None,
            next_wakeup_at: None,
            next_wakeup_reason: None,
            monitor_active: false,
            monitor_description: None,
            favorited: false,
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

#[cfg(test)]
mod paste_image_tests {
    use super::*;
    use tempfile::tempdir;

    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[test]
    fn extension_from_sniffed_mime() {
        assert_eq!(paste_image_extension("image/png"), "png");
        assert_eq!(paste_image_extension("image/jpeg"), "jpg");
        assert_eq!(paste_image_extension("image/gif"), "gif");
        assert_eq!(paste_image_extension("image/webp"), "webp");
    }

    #[test]
    fn write_paste_image_lands_in_worktree_and_ignores_itself() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let (path, name) = write_paste_image(&project, PNG_1PX, "png").unwrap();

        assert!(path.exists(), "image file must be written");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            PNG_1PX,
            "bytes must round-trip"
        );
        assert!(name.starts_with("aoe-paste-") && name.ends_with(".png"));
        let gitignore = dir.path().join(PASTE_IMAGE_DIR).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(gitignore).unwrap(),
            "*\n",
            "dir must self-ignore so pasted blobs never reach git"
        );
    }

    #[test]
    fn non_sandboxed_pane_path_is_absolute_host_path() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let pane = pane_visible_paste_path(&project, false, "aoe-paste-x.png");

        let expected = dir
            .path()
            .join(PASTE_IMAGE_DIR)
            .join("aoe-paste-x.png")
            .to_string_lossy()
            .to_string();
        assert_eq!(pane, expected);
    }

    #[test]
    fn sandboxed_pane_path_uses_container_mount() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let dir_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let pane = pane_visible_paste_path(&project, true, "aoe-paste-x.png");

        // A non-git worktree mounts under /workspace/<dir-name>; the pasted
        // path must be the container-visible path, not the host path.
        assert_eq!(
            pane,
            format!("/workspace/{dir_name}/{PASTE_IMAGE_DIR}/aoe-paste-x.png")
        );
    }
}
