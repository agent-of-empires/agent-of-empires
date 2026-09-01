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
use crate::session::{
    duplicate_session_error, is_duplicate_session, EnsureReadyError, EnsureReadyOutcome, Instance,
    LifecycleOperation, Status, Storage,
};

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
    /// True when the session's structured-view worker was auto-stopped for
    /// inactivity (resumable/dormant), as opposed to a deliberate Stop. Lets
    /// the dashboard render a distinct dormant dot instead of a live-idle one.
    /// A deliberate Stop keeps `status: "Stopped"` and reports `false` here.
    /// See #2250.
    pub dormant: bool,
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
    /// Per-session color label (`red` / `amber` / `green`), or omitted when
    /// unset. Rendered as a colored status dot in the web sidebar; set via the
    /// sidebar context menu or `aoe session color`. See #2383.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
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
    /// Host-scoped identity for `remote_owner` ("owner@host"), so the web
    /// sidebar's org axis can bucket by this instead of the bare owner: two
    /// owners of the same name on different hosts (GitHub "acme" vs GitLab
    /// "acme") must never merge into one group or one bulk-archive scope.
    /// `remote_owner` stays the display label. Populated the same way and on
    /// the same cadence as `remote_owner` (see the cache fill in
    /// `list_sessions`); `None` whenever `remote_owner` is `None`.
    pub remote_owner_key: Option<String>,
    /// Per-session push-notification overrides. None means the session
    /// inherits the server-wide default (`web.notify_on_*`) for that
    /// event type; Some(true)/Some(false) is an explicit toggle.
    pub notify_on_waiting: Option<bool>,
    pub notify_on_idle: Option<bool>,
    pub notify_on_error: Option<bool>,
    /// How this session is rendered: `structured` (ACP native rendering) or
    /// `terminal` (tmux-backed PTY). The web dashboard branches on this to
    /// pick the structured panels vs the terminal view.
    #[serde(default, skip_serializing_if = "crate::session::View::is_terminal")]
    pub view: crate::session::View,
    /// Live structured view worker lifecycle. `absent` for tmux sessions or
    /// structured view sessions whose worker has not been spawned/attached
    /// yet; `resuming` while the reconciler is mid-spawn or mid-attach;
    /// `running` once the supervisor holds a live worker. Drives the
    /// sidebar `Resuming…` chip and the per-session banner in the
    /// structured view. See #1088.
    pub acp_worker_state: crate::acp::supervisor::AcpWorkerState,
    /// True when this session's agent can run in structured view: a built-in
    /// with an ACP adapter, or a custom agent whose profile config
    /// declares a valid `agent_acp_cmd`. The web terminal view reads
    /// this to decide whether the "switch to structured view" affordance is
    /// available, replacing the hardcoded client-side tool list.
    pub acp_capable: bool,
    /// The session's server-owned prompt queue (follow-ups the user lined up
    /// while a turn was busy), ordered by `seq`. The daemon owns it, so it is
    /// visible across the user's devices and survives a client reload; the
    /// structured view renders it and drains happen server-side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<crate::acp::state::QueuedPromptEntry>,
    /// The session's captured ACP session id, present only once the
    /// structured-view worker has minted one. The web dashboard passes this
    /// as `fork_from` on a structured fork create and gates the "Fork" action
    /// on it together with `acp_can_fork`. Omitted when absent (terminal
    /// sessions, or structured ones whose worker has not minted an id yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    /// The session's resolved ACP registry key (`agent_name` when set, else
    /// `tool`), matching the `name` entries `/api/acp/agents` returns. The
    /// structured view's switch-agent modal reads this as the current-agent
    /// fallback before the first `AgentSwitched` event lands (which is the
    /// only event that populates the reduced `state.agent`), so it can gray
    /// out the running backend on a never-switched session. Omitted for
    /// sessions with no resolved agent. See #2803.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    /// True when this session's agent can run a structured ACP `session/fork`,
    /// per [`crate::session::fork::structured_fork_capable`]. Resume-only ACP
    /// agents (e.g. `aoe-agent`) are ACP-capable yet not forkable, so the web
    /// gates "Fork" on this AND `acp_session_id` rather than on a captured id
    /// alone. Omitted (read as not-forkable) for terminal sessions and
    /// non-forkable agents.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub acp_can_fork: bool,
    /// Whether switching this session between terminal and structured view
    /// preserves the conversation (only claude pairings share one
    /// CLI-resumable transcript). Server-owned via
    /// `agents::acp_transcript_cli_resumable` so the dashboard and TUI stop
    /// each recomputing it from `tool` + `acp_agent`. Omitted for
    /// non-preserving pairings.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keeps_context: bool,
    /// Slash-command aliases that reset the conversation for this session's
    /// agent (claude `/clear`, codex/opencode `/new`). Server-owned from
    /// `acp::agent_profiles::resolve(...).clear_aliases` so the composer's `/`
    /// palette and queued-prompt batching do not mirror the per-agent list.
    /// Omitted for agents with no clear alias.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear_aliases: Vec<String>,
    /// True when the session is a Claude Code session AND the user has
    /// enabled Claude's fullscreen renderer (`tui: "fullscreen"` in
    /// `~/.claude/settings.json`). The web client uses this to skip
    /// scrollback-tracking workarounds that target tmux copy-mode.
    pub claude_fullscreen: bool,
    /// Repos in the multi-repo workspace (empty for single-repo sessions).
    /// Each entry mirrors `WorkspaceRepo` minus paths the dashboard does
    /// not need to display.
    pub workspace_repos: Vec<WorkspaceRepoSummary>,
    /// Non-fatal warnings surfaced by a mutation response. On create these are
    /// worktree-creation warnings (e.g. post-checkout hook failures where the
    /// worktree was still created successfully). On rename these carry the
    /// tmux rekey warning emitted when the title was persisted durably but the
    /// live tmux session could not be renamed afterwards. Both live on the
    /// response only: the field is not persisted to the instance, so it is
    /// omitted from list/fetch responses.
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
        acp_worker_state: crate::acp::supervisor::AcpWorkerState,
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
            status: inst.status.wire_str().to_string(),
            dormant: inst.is_shown_dormant(),
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
            color: inst.color.clone(),
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
            remote_owner_key: None,
            notify_on_waiting: inst.notify_on_waiting,
            notify_on_idle: inst.notify_on_idle,
            notify_on_error: inst.notify_on_error,
            view: inst.view,
            queued_prompts: {
                let mut q = inst.queued_prompts.clone();
                q.sort_by_key(|e| e.seq);
                q
            },
            acp_worker_state,
            // Built-in ACP capability is resolved here from a process-wide
            // registry (cheap, no IO). Custom agents depend on profile
            // config; the list and create handlers overlay that without a
            // per-row config read.
            acp_capable: {
                let resolved = inst
                    .agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str());
                builtin_acp_registry().get(resolved).is_some()
            },
            acp_session_id: inst.acp_session_id.clone(),
            // Resolved the same way as `acp_capable` above: `agent_name` when
            // set and non-empty, else `tool`. This is the ACP registry key,
            // so it matches `/api/acp/agents` names the switch-agent modal
            // filters against. See #2803.
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
            // cannot drift: forkable = built-in ACP adapter verified to fork.
            acp_can_fork: agent_is_structured_fork_capable(&inst.tool, inst.agent_name.as_deref()),
            // Same agent resolution as `acp_agent` above; computed once here so
            // the web dashboard and native TUI stop mirroring the gate.
            keeps_context: crate::agents::acp_transcript_cli_resumable(
                &inst.tool,
                inst.agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str()),
            ),
            // Same agent resolution as `acp_agent` above; the composer palette
            // and queued-prompt clear-boundary hint read these instead of a
            // client-side per-agent mirror.
            clear_aliases: crate::acp::agent_profiles::resolve(
                inst.agent_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(inst.tool.as_str()),
            )
            .clear_aliases
            .iter()
            .map(|s| s.to_string())
            .collect(),
            claude_fullscreen: claude_fullscreen && inst.tool == "claude",
            // A session converted by `attach_project` (#3103) has a real
            // `workspace_info`, so this lists both repos with no special case:
            // the structured view's repo-relative path rendering, the diff-repo
            // resolver and the sidebar's multi-repo grouping all see the same
            // shape they see for a session created multi-repo.
            workspace_repos: inst
                .all_repos()
                .iter()
                .map(|r| WorkspaceRepoSummary {
                    name: r.name.clone(),
                    source_path: r.source_path.clone(),
                    branch: r.branch.clone(),
                })
                .collect(),
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
fn builtin_acp_registry() -> &'static crate::acp::AgentRegistry {
    static REG: std::sync::OnceLock<crate::acp::AgentRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(crate::acp::AgentRegistry::with_defaults)
}

/// True iff this custom agent can run in structured view: it declares a valid
/// `agent_acp_cmd`, or it inherits a registry-backed base via
/// `agent_detect_as`. Built-in capability is handled separately in the
/// constructor, so this only covers the custom case.
fn custom_agent_acp_capable(session: &crate::session::config::SessionConfig, tool: &str) -> bool {
    session
        .agent_acp_cmd
        .get(tool)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
        || crate::acp::inherited_acp_base(tool, &session.agent_detect_as).is_some()
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

/// Publish only fields owned by the rename transaction onto the current cache
/// row. Watchers and user actions may have advanced every other field while the
/// blocking git and storage work ran. Identity fields that the rename did not
/// change are reconciled from the authoritative disk snapshot only while the
/// cache still matches the live baseline captured at the start of the request.
struct SessionRenameCachePatch<'a> {
    title: &'a str,
    initial_path: &'a str,
    initial_branch: Option<&'a str>,
    authoritative_path: &'a str,
    authoritative_branch: Option<&'a str>,
    renamed_path: Option<&'a str>,
    renamed_branch: Option<&'a str>,
}

/// Reconcile one identity field the rename transaction does not own, returning
/// the value to write or `None` to keep the current cached value.
///
/// `renamed` is `Some` when the rename explicitly changed the field and always
/// wins. Otherwise the field is adopted from the `authoritative` disk snapshot
/// only while the live `cached` value still equals the `baseline` captured at
/// the start of the request; if a watcher or user action advanced it since,
/// `None` is returned so the newer cached value survives. `path` and `branch`
/// share this exact rule, so both route through here.
fn reconcile_unowned_identity<'a>(
    cached: Option<&str>,
    baseline: Option<&str>,
    authoritative: Option<&'a str>,
    renamed: Option<&'a str>,
) -> Option<&'a str> {
    match renamed {
        Some(_) => renamed,
        None if cached == baseline => authoritative,
        None => None,
    }
}

fn apply_session_rename_cache_patch(inst: &mut Instance, patch: SessionRenameCachePatch<'_>) {
    inst.title = patch.title.to_string();
    if let Some(path) = reconcile_unowned_identity(
        Some(inst.project_path.as_str()),
        Some(patch.initial_path),
        Some(patch.authoritative_path),
        patch.renamed_path,
    ) {
        inst.project_path = path.to_string();
    }
    let cached_branch = inst
        .worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.as_str());
    let branch = reconcile_unowned_identity(
        cached_branch,
        patch.initial_branch,
        patch.authoritative_branch,
        patch.renamed_branch,
    );
    if let (Some(worktree), Some(branch)) = (inst.worktree_info.as_mut(), branch) {
        worktree.branch = branch.to_string();
    }
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

/// Release a sandboxed session's hold on its worktree mount ahead of a
/// `git worktree move`, on the blocking pool, and report whether the
/// worktree is *still* held.
///
/// NOT a read-only probe: for a container that is merely stopped this
/// removes it, because a surviving container keeps pinning the bind mount
/// and the rename would fail. Only call it on a path that is about to
/// perform the move. See `ensure_sandbox_container_released` for the
/// running-vs-stopped split.
///
/// Fails closed at the async boundary: a `spawn_blocking` panic or
/// cancellation reports the worktree as held (with a `warn!` log), so
/// the caller rejects the mutating request with `409 CONFLICT` rather
/// than risk renaming against a possibly-live container mount. Sharing
/// this helper between `rename_session` and `set_worktree_name` keeps
/// the fail-closed policy synchronized across the two endpoints (#2596).
async fn ensure_sandbox_container_released_blocking(id: &str, is_sandboxed: bool) -> bool {
    let probe_id = id.to_string();
    let log_id = id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::worktree_edit::ensure_sandbox_container_released(&probe_id, is_sandboxed)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(
            target: "server.api.sessions",
            session = %log_id,
            error = %e,
            "sandbox container release task failed at the async boundary; failing closed and reporting the worktree as held rather than renaming against a possibly-live container mount"
        );
        true
    })
}

#[derive(Debug, PartialEq, Eq)]
enum RenamePersistOutcome {
    Updated { old_title: String },
    Missing,
}

fn persist_rename_metadata(
    storage: &Storage,
    id: &str,
    title: &str,
    new_path: Option<&str>,
    new_branch: Option<&str>,
) -> anyhow::Result<RenamePersistOutcome> {
    storage.update(|instances, _groups| {
        let Some(inst) = instances.iter_mut().find(|instance| instance.id == id) else {
            return Ok(RenamePersistOutcome::Missing);
        };
        let old_title = inst.title.clone();
        if let Some(path) = new_path {
            apply_worktree_name_edit(inst, path, new_branch);
        }
        apply_session_title_rename(inst, title.to_string());
        Ok(RenamePersistOutcome::Updated { old_title })
    })
}

/// Rename a session's title (and, when tied, its worktree directory).
///
/// The sandbox container probe runs on the blocking pool via
/// `ensure_sandbox_container_released_blocking`, which fails closed on a
/// `spawn_blocking` panic or cancellation so the rename is rejected
/// with `409 CONFLICT` rather than proceeding against a possibly-live
/// container mount.
pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
    // Prompt submission has its own authority and never takes `instance_lock`
    // (#3621), so hold that one too or a queue drain lands a follow-up on the
    // worker this quiesces for the move. Submission first, as it documents,
    // and via the admission form so an unknown id allocates neither lock.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let live = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        inst.clone()
    };
    let profile = live.source_profile.clone();
    // App-wide and per-session flocks may wait on another process, so never
    // acquire them on a Tokio worker. Identity nests outside session title,
    // source lifecycle, and profile Storage.
    let _identity_lock = match tokio::task::spawn_blocking(
        crate::session::acquire_session_identity_lock,
    )
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to acquire session identity lock");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Session identity lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let lock_id = id.clone();
    let lock_profile = profile.clone();
    let lock_file_watch = state.file_watch.clone();
    let (_session_title_lock, _lifecycle_lock, storage, disk_instances) =
        match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let session_title_lock = crate::session::acquire_session_title_lock(&lock_id)?;
            let storage = Storage::new(&lock_profile, lock_file_watch)?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&lock_id)?;
            let instances = storage.load()?;
            Ok((session_title_lock, lifecycle_lock, storage, instances))
        })
        .await
        {
            Ok(Ok(locks)) => locks,
            Ok(Err(error)) => {
                tracing::error!(target: "http.api.sessions", session = %id, "failed to acquire rename locks or load authoritative state: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "title_lock_failed",
                        "message": "Could not serialize the session rename"
                    })),
                )
                    .into_response();
            }
            Err(error) => {
                tracing::error!(target: "http.api.sessions", session = %id, "rename lock task failed: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "title_lock_failed",
                        "message": "Could not serialize the session rename"
                    })),
                )
                    .into_response();
            }
        };
    let Some(mut fresh) = disk_instances
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
    else {
        return super::session_not_found();
    };
    fresh.source_profile.clone_from(&profile);
    fresh.merge_runtime_from_reload(&live);
    let current_title = fresh.title.clone();
    let worktree_info = fresh.worktree_info.clone();
    let current_path = fresh.project_path.clone();
    let current_branch = worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.clone());
    let status = fresh.status;
    let is_sandboxed = fresh.is_sandboxed();
    let is_structured = fresh.is_structured();

    // Tied mode (#1927): renaming an aoe-managed worktree session also moves
    // its directory leaf to match the title, so title and dir cannot drift.
    let tied = fresh.tie_workdir_applies(
        crate::session::profile_config::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name,
    );
    let duplicate_path = if tied {
        crate::session::worktree_edit::derived_worktree_path(
            std::path::Path::new(&current_path),
            &title,
        )
    } else {
        current_path.clone()
    };
    let pair_changed = title != current_title
        || duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(disk_instances.iter(), &title, &duplicate_path, Some(&id))
    {
        let message = duplicate_session_error(&title).to_string();
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "duplicate_session",
                "message": message,
            })),
        )
            .into_response();
    }

    // What to write to disk + memory once any git side effect has landed.
    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;

    if tied {
        // A directory move or branch rename is gated on a quiescent worktree,
        // exactly like the standalone worktree-name edit. A running session
        // must be stopped first; the setting is the escape hatch for
        // free-form relabeling.
        //
        // A sandbox session's container keeps the worktree dir mounted even
        // while the agent is Idle, so a directory move would fail. The helper
        // drops a merely-stopped container to free the mount and only reports
        // held for a live one, which the user has to stop.
        //
        // Short-circuited twice, because the helper removes a stopped
        // container: once on the status check, so a request about to be
        // rejected never discards, and once on whether the directory is
        // actually going to move, so a no-op or branch-only rename does not
        // either.
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&title);
        let moves_worktree = crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&current_path),
            &leaf,
        );
        let renames_branch = worktree_info.as_ref().is_some_and(|wt| {
            crate::session::worktree_edit::worktree_branch_rename_required(
                wt,
                &leaf,
                body.rename_branch,
            )
        });
        let container_holds = !status.blocks_worktree_edit()
            && moves_worktree
            && ensure_sandbox_container_released_blocking(&id, is_sandboxed).await;
        if (moves_worktree || renames_branch) && (status.blocks_worktree_edit() || container_holds)
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session_running",
                    "message": "Stop the session before renaming its worktree directory or branch. Disable \"Tie Worktree Directory to Session Name\" to relabel a running session."
                })),
            )
                .into_response();
        }

        // Stop a live structured-view worker only when its cwd will move. A
        // title-only or branch-only edit leaves the cwd valid and must not
        // interrupt the worker.
        if moves_worktree {
            if let Err(response) =
                quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
            {
                return response;
            }
        }

        let wt = worktree_info.expect("tied implies worktree_info is Some");
        let cur = current_path.clone();
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
    let title_clone = title.clone();
    let id_clone = id.clone();
    let new_path_clone = new_path.clone();
    let new_branch_clone = new_branch.clone();
    let persisted = tokio::task::spawn_blocking(move || {
        persist_rename_metadata(
            &storage,
            &id_clone,
            &title_clone,
            new_path_clone.as_deref(),
            new_branch_clone.as_deref(),
        )
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result.map_err(|error| error.to_string()));
    let persisted_old_title = match persisted {
        Ok(RenamePersistOutcome::Updated { old_title }) => old_title,
        Ok(RenamePersistOutcome::Missing) => {
            // AppState can lag an external delete. A missing authoritative row
            // is not a successful rename and must not trigger tmux/cache work.
            if let Some(path) = new_path.as_deref() {
                tracing::warn!(
                    target: "http.api.sessions",
                    session = %id,
                    new_path = %path,
                    "authoritative row vanished after the worktree move; the moved directory is unreferenced"
                );
            }
            return super::session_not_found();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, "Failed to save after rename: {error}");
            // Persist-first: never fall through to mutate in-memory state on a
            // failed write, or the rename silently reverts on restart. When a
            // dir move already landed, say so; otherwise it is a plain title
            // persist.
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
    };

    let published_path = new_path.as_deref().unwrap_or(&current_path);
    let renamed_path = new_path
        .as_deref()
        .filter(|path| *path != current_path.as_str());
    let published_branch = new_branch.as_deref().or(current_branch.as_deref());
    let renamed_branch = new_branch
        .as_deref()
        .filter(|branch| current_branch.as_deref() != Some(*branch));
    let initial_branch = live
        .worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.as_str());
    let mut response = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        apply_session_rename_cache_patch(
            inst,
            SessionRenameCachePatch {
                title: &title,
                initial_path: &live.project_path,
                initial_branch,
                authoritative_path: published_path,
                authoritative_branch: published_branch,
                renamed_path,
                renamed_branch,
            },
        );
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen())
    };
    // Single-session responses are not run through list_sessions' overlay, so
    // carry the resolved tie value here too (#1927); otherwise a client that
    // trusts the mutation response would see a managed worktree claim it is
    // untied until the next list refresh.
    response.tie_workdir_to_name = tied;
    drop(_identity_lock);

    let tmux_warning = if persisted_old_title != title && !is_structured {
        let rekey_id = id.clone();
        let rekey_old_title = persisted_old_title.clone();
        let rekey_new_title = title.clone();
        match tokio::task::spawn_blocking(move || {
            crate::tmux::rekey_session(&rekey_id, &rekey_old_title, &rekey_new_title)
        })
        .await
        {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tmux rename failed after persistence: {error}");
                Some(format!(
                    "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
                ))
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tmux rename task failed after persistence: {error}");
                Some(format!(
                    "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
                ))
            }
        }
    } else {
        None
    };
    if let Some(warning) = tmux_warning {
        response.warnings.push(warning);
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

/// Edit a managed worktree session's workdir directory name (and optionally
/// its git branch).
///
/// The sandbox container gate runs on the blocking pool via
/// `ensure_sandbox_container_released_blocking`, which fails closed on a
/// `spawn_blocking` panic or cancellation so the edit is rejected with
/// `409 CONFLICT` rather than proceeding against a possibly-live container
/// mount.
pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
    // Prompt submission has its own authority and never takes `instance_lock`
    // (#3621), so hold that one too or a queue drain lands a follow-up on the
    // worker this quiesces for the move. Submission first, as it documents,
    // and via the admission form so an unknown id allocates neither lock.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let live = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };
        inst.clone()
    };
    let profile = live.source_profile.clone();
    let _identity_lock = match tokio::task::spawn_blocking(
        crate::session::acquire_session_identity_lock,
    )
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to acquire worktree identity lock");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Worktree identity lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let lock_id = id.clone();
    let lock_profile = profile.clone();
    let lock_file_watch = state.file_watch.clone();
    let (_lifecycle_lock, storage, authoritative_instances) = match tokio::task::spawn_blocking(
        move || -> anyhow::Result<_> {
            let storage = Storage::new(&lock_profile, lock_file_watch)?;
            let lifecycle = storage.acquire_instance_lifecycle_lock(&lock_id)?;
            let instances = storage.load()?;
            Ok((lifecycle, storage, instances))
        },
    )
    .await
    {
        Ok(Ok(locked)) => locked,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to lock or load worktree rename");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Worktree rename lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(mut fresh) = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
    else {
        return super::session_not_found();
    };
    fresh.source_profile.clone_from(&profile);
    fresh.merge_runtime_from_reload(&live);
    let worktree_info = fresh.worktree_info.clone();
    let current_path = fresh.project_path.clone();
    let status = fresh.status;
    let is_sandboxed = fresh.is_sandboxed();
    let is_structured = fresh.is_structured();

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
    let duplicate_path = crate::session::worktree_edit::target_worktree_path(
        std::path::Path::new(&current_path),
        &name,
    )
    .unwrap_or_else(|| std::path::PathBuf::from(&current_path))
    .to_string_lossy()
    .into_owned();
    if duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/')
        && is_duplicate_session(
            authoritative_instances.iter(),
            &fresh.title,
            &duplicate_path,
            Some(&id),
        )
    {
        let message = duplicate_session_error(&fresh.title).to_string();
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "duplicate_session",
                "message": message,
            })),
        )
            .into_response();
    }
    // A sandbox container keeps the worktree dir mounted even while the agent
    // is Idle, so the move would fail. The helper drops a merely-stopped
    // container to free the mount and only reports held for a live one, which
    // the user has to stop, same as the active-status case.
    // Short-circuited twice, because the helper removes a stopped container:
    // once on the status check, so a request about to be rejected never
    // discards, and once on whether the directory is actually going to move, so
    // a no-op or branch-only edit does not either.
    let moves_worktree = crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&current_path),
        &name,
    );
    let container_holds = !status.blocks_worktree_edit()
        && moves_worktree
        && ensure_sandbox_container_released_blocking(&id, is_sandboxed).await;
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
    // the instance_lock acquired at the top of this function. Gated on
    // `moves_worktree` for the same reason as the tied `rename_session` path: a
    // branch-only edit (name unchanged, `rename_branch` set) leaves the cwd
    // valid, so interrupting the worker would be a needless respawn. When the
    // name is unchanged and no branch rename is requested, `edit_worktree_workdir`
    // rejects with `Unchanged` below and nothing is touched either way.
    if moves_worktree {
        if let Err(resp) =
            quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
        {
            return resp;
        }
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

    let id_clone = id.clone();
    let new_path_clone = new_path.clone();
    let new_branch_clone = new_branch.clone();
    match tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) else {
                return Ok(false);
            };
            apply_worktree_name_edit(inst, &new_path_clone, new_branch_clone.as_deref());
            Ok(true)
        })
    })
    .await
    {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                new_path = %new_path,
                "authoritative row vanished after the worktree move; the moved directory is unreferenced"
            );
            return super::session_not_found();
        }
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
    drop(_identity_lock);

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Attach a project to an existing session (#3103) ---

#[derive(Deserialize)]
pub struct AttachProjectBody {
    /// Absolute host path of the repo to attach, or the name of a registered
    /// project. A name is resolved against the project registry.
    pub project: String,
    /// Check out a branch that already exists in the added repo instead of
    /// refusing. Off by default: a same-named branch in another repo can hold
    /// unrelated commits, and checking it out would feed the agent the wrong
    /// tree. Setting this records the branch as not aoe-created, so deleting the
    /// session leaves it alone.
    #[serde(default)]
    pub attach_existing_branch: bool,
}

/// `POST /api/sessions/:id/projects`. Attaches a repo to a session that already
/// exists, converting it into a multi-repo workspace and restarting it so the
/// agent comes up there with its transcript intact.
///
/// Modelled on the workdir endpoint, which refuses while the session is active
/// because it moves the directory out from under a live worker (#2260).
/// Attaching moves it too, so rather than refuse (which would gut the feature)
/// this stops the session for the move and starts it again, which is what #2346
/// asks for. Mid-turn is still refused, with 409.
pub async fn attach_session_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<AttachProjectBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }
    // Defense in depth behind `cityhall_gate`, which already denies this route:
    // attaching takes an arbitrary host path, so it is classified with
    // `git/clone` and `POST /api/projects` rather than with the session lifecycle
    // routes CityHall mode allows.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let raw = body.project.trim().to_string();
    if raw.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Project path or name is required" })),
        )
            .into_response();
    }

    let profile = {
        let instances = state.instances.read().await;
        match instances.iter().find(|i| i.id == id) {
            Some(inst) => inst.source_profile.clone(),
            None => return super::session_not_found(),
        }
    };

    // A bare name is a registry lookup; anything path-shaped is used as-is. The
    // registry is what the picker offers, so this keeps the API usable by hand
    // without making the caller resolve names itself.
    let repo_path = match resolve_project_input(&profile, &raw).await {
        Ok(p) => p,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": message })),
            )
                .into_response();
        }
    };

    let on_existing = if body.attach_existing_branch {
        crate::session::attach_project::ExistingBranch::Attach
    } else {
        crate::session::attach_project::ExistingBranch::Refuse
    };

    match crate::server::attach_project::attach_project(&state, &id, &repo_path, on_existing).await
    {
        Ok((outcome, worker)) => {
            use crate::server::attach_project::WorkerOutcome;
            let (worker_status, worker_message) = match &worker {
                WorkerOutcome::Restarted => ("restarted", None),
                WorkerOutcome::NotRunning => ("not_running", None),
                WorkerOutcome::RestartFailed(m) => ("restart_failed", Some(m.clone())),
            };
            let response = {
                let instances = state.instances.read().await;
                instances.iter().find(|i| i.id == id).map(|inst| {
                    SessionResponse::from_instance(
                        inst,
                        crate::claude_settings::read_tui_fullscreen(),
                    )
                })
            };
            // 200 even on RestartFailed: the attachment itself succeeded and is
            // durable. The client renders the worker status so the user can see
            // the agent needs a restart rather than being told the whole
            // operation failed and left nothing behind.
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "session": response,
                    "attached": {
                        "name": outcome.repo.name,
                        "worktree_path": outcome.repo.worktree_path,
                        "branch": outcome.repo.branch,
                        "branch_created": !outcome.repo.branch_preexisting,
                        "moved_to": outcome.moved_to,
                    },
                    "warnings": outcome.warnings,
                    "worker": worker_status,
                    "worker_message": worker_message,
                })),
            )
                .into_response()
        }
        Err(e) => {
            use crate::server::attach_project::AttachError;
            let status = match &e {
                AttachError::NotFound => StatusCode::NOT_FOUND,
                AttachError::TurnInFlight => StatusCode::CONFLICT,
                AttachError::Rejected(_) => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(serde_json::json!({ "message": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Resolve the request's `project` field to a host path.
///
/// An absolute path is taken as-is. Anything else is looked up in the project
/// registry, so the web picker can send the name it already displays.
async fn resolve_project_input(profile: &str, raw: &str) -> Result<std::path::PathBuf, String> {
    // `Path` in this module is axum's extractor, so the std types are qualified.
    if std::path::Path::new(raw).is_absolute() {
        return Ok(std::path::PathBuf::from(raw));
    }
    // Path-shaped but not absolute. Without this the input falls through to the
    // registry lookup and comes back as "not in the registry", sending the user
    // after a registry problem they do not have.
    if raw.starts_with('~') || raw.contains('/') || raw.contains(std::path::MAIN_SEPARATOR) {
        return Err(format!(
            "'{raw}' looks like a path but is not absolute. Pass an absolute path, or the name of \
             a registered project."
        ));
    }
    let profile = profile.to_string();
    let name = raw.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::projects::resolve_names(&profile, &[name])
            .map_err(|e| format!("{e:#}"))
            .and_then(|projects| {
                projects
                    .into_iter()
                    .next()
                    .map(|p| std::path::PathBuf::from(p.path))
                    .ok_or_else(|| "Project not found in the registry".to_string())
            })
    })
    .await
    .map_err(|e| format!("project lookup panicked: {e}"))?
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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

pub async fn update_session_notifications(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateNotificationsBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
// `PATCH /api/sessions/{id}/diff-base` sets / clears the override for the
// diff base ref, scoped to one repo. The web `vs <ref>` chip popover, the
// TUI diff view's `b` keybind, and `aoe session set-base` all funnel
// through this endpoint (or its storage equivalent) so the override is
// persisted alongside the session record and survives restart. A workspace
// session must name the repo; a single-repo session omits it and the
// override lands on the session's own checkout. See #970, #3329.

#[derive(Deserialize)]
pub struct UpdateDiffBaseBody {
    /// New override. `Some(non-empty)` sets the override; `Some("")` or
    /// `None` clears it (the diff then falls back to the recorded creation
    /// base, the profile default, and then auto-detection).
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Workspace repo this override applies to. Omitted targets the
    /// session's own checkout, which only exists on a single-repo session;
    /// omitting it on a workspace is rejected rather than writing state
    /// nothing reads. See #3329.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Write a diff-base override onto the entry `repo` names, or onto the
/// session's own checkout when it is `None`. Split out so the persist
/// closure and the in-memory update cannot drift.
fn apply_diff_base_override(
    inst: &mut crate::session::Instance,
    repo: Option<&str>,
    value: Option<String>,
) {
    match repo {
        Some(name) => {
            if let Some(ws) = inst.workspace_info.as_mut() {
                if let Some(r) = ws.repos.iter_mut().find(|r| r.name == name) {
                    r.base_branch_override = value;
                }
            }
        }
        None => inst.base_branch_override = value,
    }
}

pub async fn update_session_diff_base(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateDiffBaseBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
        // Reject a target that names no entry, so a stale client cannot
        // silently write an override the diff never reads.
        match body.repo.as_deref() {
            Some(name) => {
                if !inst.all_repos().iter().any(|r| r.name == name) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "bad_request",
                            "message": "unknown workspace repo"
                        })),
                    )
                        .into_response();
                }
            }
            None => {
                if inst.workspace_info.is_some() {
                    let names: Vec<&str> =
                        inst.all_repos().iter().map(|r| r.name.as_str()).collect();
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "bad_request",
                            "message": format!(
                                "this session is a multi-repo workspace; name the repo to set a diff base for ({})",
                                names.join(", ")
                            )
                        })),
                    )
                        .into_response();
                }
            }
        }
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
    let persist_repo = body.repo.clone();
    if persist_session_update(
        profile,
        "diff-base update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                apply_diff_base_override(inst, persist_repo.as_deref(), persist_override);
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
    apply_diff_base_override(inst, body.repo.as_deref(), new_override);

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
pub struct UpdateColorBody {
    /// A palette member (`red` / `amber` / `green`) sets the label; `null` (or
    /// a missing field) clears it. Validated against
    /// `crate::session::is_valid_session_color`, matching the CLI.
    #[serde(default)]
    pub color: Option<String>,
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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

pub async fn update_session_color(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateColorBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Validate up front so an unknown color never reaches disk. `None` clears
    // the label. Mirrors the CLI's palette check.
    let new_color = body.color.map(|c| c.trim().to_lowercase());
    if let Some(c) = &new_color {
        if !crate::session::is_valid_session_color(c) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("invalid color {c:?}; expected one of: red, amber, green, or null"),
                })),
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
    let persist_color = new_color.clone();
    if persist_session_update(
        profile,
        "color update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                // Pre-validated above, so this cannot fail.
                let _ = inst.set_color(persist_color);
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
            "color update: instance vanished after persist"
        );
        return super::session_gone_after_persist();
    };
    let _ = inst.set_color(new_color);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

pub async fn update_session_archive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateArchiveBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
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

        let structured_view = inst.is_structured();
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

/// `POST /api/sessions/:id/trash`. The per-instance lifecycle flock is held
/// from the durable Trash reservation through teardown, relocation, and final commit.
pub async fn trash_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<TrashSessionBody>>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return super::read_only_response();
    }
    let body = body.map(|Json(body)| body).unwrap_or_default();

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    let (profile, snapshot) = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return super::session_not_found();
        };
        (instance.source_profile.clone(), instance.clone())
    };

    let reserve_profile = profile.clone();
    let reserve_id = id.clone();
    let file_watch = state.file_watch.clone();
    let (storage, lifecycle_lock, generation) = match tokio::task::spawn_blocking(
        move || -> anyhow::Result<_> {
            let storage = Storage::new(&reserve_profile, file_watch)?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&reserve_id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(instance) = instances
                    .iter_mut()
                    .find(|instance| instance.id == reserve_id)
                else {
                    anyhow::bail!("session disappeared before trash");
                };
                instance
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)?;
                instance.trash();
                Ok(instance.lifecycle_generation)
            })?;
            Ok((storage, lifecycle_lock, generation))
        },
    )
    .await
    {
        Ok(Ok(reserved)) => reserved,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash reservation failed: {error}");
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "lifecycle_busy",
                    "message": error.to_string()
                })),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, "trash reservation join failed: {error}");
            return persist_failed_response();
        }
    };

    let was_structured_view = snapshot.is_structured();
    {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return super::session_gone_after_persist();
        };
        instance.trash();
        instance.lifecycle_generation = generation;
    }

    if was_structured_view {
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(error) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during trash failed: {error}"
            ),
        }
    }

    let work_id = id.clone();
    let kill_pane = body.kill_pane;
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _lifecycle_lock = lifecycle_lock;
        let mut instance = snapshot;
        if kill_pane {
            if was_structured_view {
                instance.kill_ancillary_tmux_sessions_locked();
            } else {
                instance.kill_all_tmux_sessions_locked();
            }
        }
        let outcome = crate::session::trash::prepare_trashed_worktree(&mut instance);
        let relocation = match &outcome {
            crate::session::trash::RelocateOutcome::Relocated { .. } => {
                Some(crate::session::trash::TrashRelocation {
                    new_project_path: instance.project_path.clone(),
                    pre_trash_project_path: instance.pre_trash_project_path.clone(),
                })
            }
            crate::session::trash::RelocateOutcome::Skipped
            | crate::session::trash::RelocateOutcome::Failed { .. } => None,
        };
        storage.update(|instances, _groups| {
            if let Some(relocation) = &relocation {
                let commit = crate::session::claim::commit_trash_relocation(
                    instances, &work_id, generation, relocation,
                );
                anyhow::ensure!(
                    commit == crate::session::claim::RelocationCommit::Persisted,
                    "trash relocation reservation was superseded"
                );
            } else if let Some(stored) = instances
                .iter_mut()
                .find(|candidate| candidate.id == work_id)
            {
                stored
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
            }
            Ok(())
        })?;
        let durable = storage
            .load()?
            .into_iter()
            .find(|candidate| candidate.id == work_id);
        Ok((outcome, durable))
    })
    .await;

    let (outcome, durable) = match transition {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash transition failed: {error}");
            return persist_failed_response();
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash transition join failed: {error}");
            return persist_failed_response();
        }
    };
    if let crate::session::trash::RelocateOutcome::Failed { reason } = outcome {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "trash worktree relocation skipped: {reason}",
        );
    }

    let Some(durable) = durable else {
        return super::session_not_found();
    };
    let response = {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return super::session_gone_after_persist();
        };
        instance.project_path = durable.project_path;
        instance.pre_trash_project_path = durable.pre_trash_project_path;
        instance.lifecycle_generation = durable.lifecycle_generation;
        instance.lifecycle_reservation = durable.lifecycle_reservation;
        SessionResponse::from_instance(instance, crate::claude_settings::read_tui_fullscreen())
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/restore`. The lifecycle flock covers reservation
/// acquisition, worktree restoration, and durable untrash as one transition.
pub async fn restore_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return super::read_only_response();
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return super::session_not_found();
        };
        instance.source_profile.clone()
    };

    enum RestoreTransitionError {
        NotFound,
        Busy(String),
        Worktree(String),
        Persist(String),
    }

    let restore_profile = profile.clone();
    let restore_id = id.clone();
    let file_watch = state.file_watch.clone();
    let restored = tokio::task::spawn_blocking(move || {
        let run = || -> Result<Instance, RestoreTransitionError> {
            let storage = Storage::new(&restore_profile, file_watch)
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let _lifecycle_lock = storage
                .acquire_instance_lifecycle_lock(&restore_id)
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let decision = storage
                .update(|instances, _groups| {
                    crate::session::claim::decide_restore_claim(
                        instances,
                        &restore_id,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)
                })
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let generation = match decision {
                crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
                crate::session::claim::RestoreClaimDecision::AlreadyGone => {
                    return Err(RestoreTransitionError::NotFound);
                }
                crate::session::claim::RestoreClaimDecision::Busy(holder) => {
                    return Err(RestoreTransitionError::Busy(holder.busy_reason()));
                }
            };
            let Some(mut instance) = storage
                .load()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?
                .into_iter()
                .find(|candidate| candidate.id == restore_id)
            else {
                return Err(RestoreTransitionError::NotFound);
            };
            if let crate::session::trash::RestoreOutcome::Failed { reason } =
                crate::session::trash::restore_worktree_location(&mut instance)
            {
                let _ = storage.update(|instances, _groups| {
                    if let Some(stored) = instances
                        .iter_mut()
                        .find(|candidate| candidate.id == restore_id)
                    {
                        stored.release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Restore,
                            generation,
                        );
                    }
                    Ok(())
                });
                return Err(RestoreTransitionError::Worktree(reason));
            }
            let restored_path = instance.project_path.clone();
            let restored_pre = instance.pre_trash_project_path.clone();
            let commit = storage
                .update(|instances, _groups| {
                    Ok(crate::session::claim::finalize_restore_commit(
                        instances,
                        &restore_id,
                        generation,
                        &restored_path,
                        &restored_pre,
                    ))
                })
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            match commit {
                crate::session::claim::RestoreCommit::Committed => {
                    instance.untrash();
                    instance.lifecycle_reservation = None;
                    Ok(instance)
                }
                crate::session::claim::RestoreCommit::Superseded => {
                    Err(RestoreTransitionError::Busy(
                        crate::session::NEWER_GENERATION_BUSY_REASON.to_string(),
                    ))
                }
                crate::session::claim::RestoreCommit::AlreadyGone => {
                    Err(RestoreTransitionError::NotFound)
                }
            }
        };
        run()
    })
    .await;

    let restored = match restored {
        Ok(Ok(instance)) => instance,
        Ok(Err(RestoreTransitionError::NotFound)) => return super::session_not_found(),
        Ok(Err(RestoreTransitionError::Busy(holder))) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "lifecycle_busy",
                    "message": format!("Session is {holder}, so it was not restored")
                })),
            )
                .into_response();
        }
        Ok(Err(RestoreTransitionError::Worktree(reason))) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "worktree_restore_failed",
                    "message": format!("Could not restore the worktree: {reason}")
                })),
            )
                .into_response();
        }
        Ok(Err(RestoreTransitionError::Persist(error))) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore transition failed: {error}");
            return persist_failed_response();
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore transition join failed: {error}");
            return persist_failed_response();
        }
    };

    let response = {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return super::session_gone_after_persist();
        };
        instance.project_path = restored.project_path;
        instance.pre_trash_project_path = restored.pre_trash_project_path;
        instance.lifecycle_generation = restored.lifecycle_generation;
        instance.lifecycle_reservation = restored.lifecycle_reservation;
        instance.untrash();
        SessionResponse::from_instance(instance, crate::claude_settings::read_tui_fullscreen())
    };
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = super::acp::read_only_block(&state) {
        return resp;
    }

    let Some((profile, tool, command, project_path, sandboxed, title, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.project_path.clone(),
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
    // drop (a resolved rename agent with no one-shot, an overridden command, or
    // a sandboxed session whose rename agent is not its own). Without this, the
    // sidebar would show success while no title job runs. Resolves with the SAME repo-aware config the worker
    // uses (resolve_config_with_repo_or_warn), so a repo-local smart_rename_agent
    // or agent_command_override cannot make the preflight and worker disagree.
    // Passes `setting_on = true` because this is the manual "Auto-name now"
    // action, which runs on demand even when auto-rename-on-start is disabled
    // (#3039); the spawned try_smart_rename gets `force = true` below to match.
    let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
        &profile,
        std::path::Path::new(&project_path),
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        structured,
        true,
        &title,
        &tool,
        &config.smart_rename_agent,
        sandboxed,
        &command,
        &config.agent_command_override,
    ) {
        use crate::session::smart_rename::SkipReason;
        // Wording comes from the shared `user_message` so this response and the
        // TUI's dialog cannot drift; only the status code is per-reason.
        let status = match reason {
            SkipReason::NotStructured => StatusCode::BAD_REQUEST,
            _ => StatusCode::CONFLICT,
        };
        return (
            status,
            Json(serde_json::json!({ "message": reason.user_message() })),
        )
            .into_response();
    }

    // A sandboxed session's one-shot runs inside its container, so a stopped
    // container is the one remaining way the spawned job would drop the session
    // after the static gate passed. Probe it here too, else this would answer 202
    // while nothing renames, which is exactly what the gate above exists to
    // prevent. Same check and wording as the TUI's preflight; the spawned
    // try_smart_rename re-probes and stays the authority.
    if sandboxed {
        use crate::containers::Probe;
        let sid = id.clone();
        let probe = tokio::task::spawn_blocking(move || {
            crate::containers::DockerContainer::from_session_id(&sid).probe_running()
        })
        .await;
        // A failed inspection is not a stopped container: telling the user to
        // start a container that may already be running sends them the wrong
        // way, so the runtime error is surfaced as its own state. Same split as
        // the TUI preflight.
        let unknown = match probe {
            Ok(Probe::Running) => None,
            Ok(Probe::NotRunning) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "container_not_running",
                        "message": "The session's sandbox container is not running, so its agent cannot be asked for a name. Open the session to start it, then try again.",
                    })),
                )
                    .into_response();
            }
            Ok(Probe::Unknown(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(err) = unknown {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "container_state_unknown",
                    "message": format!("Couldn't check the session's sandbox container, so its agent cannot be asked for a name: {err}"),
                })),
            )
                .into_response();
        }
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
        // Manual action forces past the smart_rename-disabled gate (#3039).
        true,
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
            SkipReason::NameNotDefault
            | SkipReason::Disabled
            | SkipReason::SandboxRenameAgentMismatch => (
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
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

        let structured = inst.is_structured();
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

    // Structured sessions have no tmux/container teardown transaction, so
    // persist their dormant stop before asking the supervisor to shut down.
    // Plain sessions delegate the full reserve/teardown/commit sequence to
    // `Instance::stop` below.
    if is_structured {
        let persist_id = id.clone();
        if persist_session_update(
            profile.clone(),
            "stop session",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    inst.status = Status::Stopped;
                    inst.mark_idle_dormant();
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }
    }

    let inst_clone = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "stop session: instance vanished before teardown"
            );
            return super::session_gone_after_persist();
        };
        if is_structured {
            inst.status = Status::Stopped;
            inst.mark_idle_dormant();
        }
        inst.clone()
    };

    if is_structured {
        // Structured view: shut down the worker so the reconciler does not
        // race to respawn it. `shutdown` preserves the transcript, so the
        // session resumes the conversation when reopened.
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
        let stop_profile = profile.clone();
        let stop_id = id.clone();
        match tokio::task::spawn_blocking(move || {
            let stop_result = inst_for_stop.stop();
            let disk_result = Storage::new_unwatched(&stop_profile)
                .and_then(|storage| storage.load())
                .map(|instances| {
                    instances
                        .into_iter()
                        .find(|instance| instance.id == stop_id)
                });
            (stop_result, disk_result)
        })
        .await
        {
            Ok((stop_result, disk_result)) => {
                if let Err(e) = stop_result {
                    tracing::warn!(target: "http.api.sessions", "Stop: session stop failed: {e}");
                }
                match disk_result {
                    Ok(Some(stopped)) => {
                        let mut instances = state.instances.write().await;
                        if let Some(live) = instances.iter_mut().find(|instance| instance.id == id)
                        {
                            live.merge_post_start(&stopped);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        target: "http.api.sessions",
                        "Stop: failed to reload lifecycle generation: {e}"
                    ),
                }
            }
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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

        let structured = inst.is_structured();
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
            // Explicit restart endpoint (web dashboard Restart button):
            // honor auto_resume_on_restart, same as TUI `e`/`Enter`. The
            // instance-level cascade holds the lifecycle lock across final
            // poller drain, exact-pane OMP capture, kill, and relaunch.
            match inst.restart_with_resume_policy(
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
                if apply_post_restart_sync(inst, &sync_base, &started) {
                    inst.status = Status::Error;
                    inst.last_error = Some(msg.clone());
                }
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (was_structured_view, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return super::session_not_found();
        };

        let structured_view = inst.is_structured();
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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

#[derive(Default, Deserialize, Clone)]
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
/// auto-purge worker so the permanent-delete path cannot diverge between the
/// two. Returns user-facing deletion messages on success, or a descriptive
/// error string on failure. Blocking reservation, hook, and completion phases
/// are dispatched internally; no caller-held lifecycle guard crosses an await.
/// The `bool` in the success tuple is `true` when the session row was actually
/// removed, and `false` when a concurrent restore won the race and the row was
/// deliberately kept (see the `kept_restored` branch). Callers must not report
/// a kept row as deleted.
async fn purge_session_artifacts(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
) -> Result<(bool, Vec<String>), String> {
    let profile = instance.source_profile.clone();
    if profile.is_empty() {
        return Err(
            "Session has no source profile; refusing to acquire a default-profile purge lock"
                .to_string(),
        );
    }
    let delete_request = crate::session::deletion::DeletionRequest {
        session_id: id.to_string(),
        instance: instance.clone(),
        delete_worktree: body.delete_worktree,
        delete_branch: body.delete_branch,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        detach_hooks: true,
        keep_scratch: body.keep_scratch,
    };
    let file_watch = state.file_watch.clone();
    let reserve_profile = profile.clone();
    let reservation = tokio::task::spawn_blocking(move || {
        let storage = Storage::new(&reserve_profile, file_watch)
            .map_err(|e| format!("Storage init failed before session teardown: {e}"))?;
        crate::session::deletion::PurgeTransaction::reserve(storage, delete_request)
            .map_err(|e| format!("Failed to reserve session purge: {e}"))
    })
    .await
    .map_err(|e| format!("Deletion reservation task failed: {e}"))??;
    let transaction = match reservation {
        crate::session::deletion::PurgeReservation::Reserved(transaction) => transaction,
        crate::session::deletion::PurgeReservation::Rejected(result) => {
            return match result.disposition {
                crate::session::deletion::DeletionDisposition::AlreadyGone => {
                    remove_instance(
                        &mut *state.instances.write().await,
                        id,
                        &state.mutation_epoch,
                    );
                    state.instance_locks.write().await.remove(id);
                    state.session_service.forget_prompt_lock(id).await;
                    Ok((true, result.messages))
                }
                crate::session::deletion::DeletionDisposition::KeptRestored => {
                    Err("Session is being restored, so it was not purged".to_string())
                }
                crate::session::deletion::DeletionDisposition::Busy => {
                    Err(result.errors.first().cloned().unwrap_or_else(|| {
                        "Session is busy with another lifecycle operation, so it was not purged"
                            .to_string()
                    }))
                }
                crate::session::deletion::DeletionDisposition::Failed
                | crate::session::deletion::DeletionDisposition::Removed => {
                    Err(result.errors.join("; "))
                }
            };
        }
    };
    let transaction = tokio::task::spawn_blocking(move || transaction.run_hooks())
        .await
        .map_err(|e| format!("Deletion hook task failed: {e}"))?;

    let transcript_purged = instance.is_structured();

    let deletion_result = if transcript_purged {
        // Commit the row removal before deleting the ACP transcript. A lost
        // restore/generation race therefore leaves both row and transcript
        // intact; a successful commit makes later cleanup failures
        // non-restorable by construction.
        let committed = tokio::task::spawn_blocking(move || transaction.begin_irreversible())
            .await
            .map_err(|e| format!("Irreversible deletion commit task failed: {e}"))?;
        match committed {
            Err(result) => *result,
            Ok(committed) => {
                // Remove the local mirror before awaiting ACP so the reconciler
                // cannot surface a durable row that no longer exists. Bumps the
                // epoch under the same lock: the ACP teardown below is slow, and
                // a reload landing inside it would otherwise restore the row.
                remove_instance(
                    &mut *state.instances.write().await,
                    id,
                    &state.mutation_epoch,
                );

                // The worker may still use the worktree, so ACP teardown stays
                // ahead of sidecar cleanup. The durable row is already gone.
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

                tokio::task::spawn_blocking(move || committed.finish())
                    .await
                    .map_err(|e| format!("Deletion cleanup task failed: {e}"))?
            }
        }
    } else {
        tokio::task::spawn_blocking(move || transaction.complete())
            .await
            .map_err(|e| format!("Deletion task failed: {e}"))?
    };

    let mut messages = deletion_result.messages.clone();
    match deletion_result.disposition {
        crate::session::deletion::DeletionDisposition::KeptRestored
        | crate::session::deletion::DeletionDisposition::Busy => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "session changed or was restored before purge completion; kept the durable row"
            );
            return Ok((false, messages));
        }
        crate::session::deletion::DeletionDisposition::Failed => {
            let errs = if deletion_result.errors.is_empty() {
                "Unknown error".to_string()
            } else {
                deletion_result.errors.join("; ")
            };
            return Err(errs);
        }
        crate::session::deletion::DeletionDisposition::Removed
        | crate::session::deletion::DeletionDisposition::AlreadyGone => {}
    }
    if !deletion_result.success {
        let errs = if deletion_result.errors.is_empty() {
            "Unknown error".to_string()
        } else {
            deletion_result.errors.join("; ")
        };
        if !transcript_purged {
            return Err(errs);
        }
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "purge sidecar cleanup failed after durable removal; session stays removed: {errs}"
        );
        messages.push(format!(
            "Cleanup incomplete (session removed anyway): {errs}"
        ));
    }

    {
        // The row is now gone from both disk and memory, so any reloader still
        // carrying a `sessions.json` snapshot that predates either removal must
        // drop it rather than fold the deleted row back in. `remove_instance`
        // bumps while still holding the `instances` write lock: a reloader
        // checks the epoch under that same lock, so the removal and the bump
        // land as one step and a reload cannot slip between them. See
        // invariant 8 on `reload_state_instances_from_disk`.
        let mut instances = state.instances.write().await;
        remove_instance(&mut instances, id, &state.mutation_epoch);
    }
    state.instance_locks.write().await.remove(id);
    state.session_service.forget_prompt_lock(id).await;
    if let Some(entry) = recent_entry {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "http.api.sessions",
                "recording recent project after delete failed: {e}");
        }
    }
    Ok((true, messages))
}

/// Heal managed worktree sessions whose recorded `project_path` no longer
/// exists because the directory was moved outside aoe, rewriting it from git's
/// own worktree listing. Runs once on daemon startup, so every later
/// path-derived decision (worker cwd, diff, the rename pre-flight gates) acts
/// on the live location. See #2002.
///
/// The recorded path existing short-circuits the whole pass inside
/// [`crate::session::worktree_reconcile::reconcile_and_persist`], so a healthy
/// session costs one `stat` and never shells out to git. Every non-move outcome
/// leaves the row untouched.
pub(crate) async fn reconcile_worktree_paths(state: &Arc<AppState>) {
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.worktree_info.as_ref().is_some_and(|wt| wt.managed_by_aoe))
            .map(|i| i.id.clone())
            .collect()
    };
    for id in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        let snapshot = {
            let instances = state.instances.read().await;
            match instances.iter().find(|instance| instance.id == id) {
                Some(instance) => instance.clone(),
                None => continue,
            }
        };
        // `exists()` and the git listing are blocking filesystem work, so the
        // whole reconcile runs off the runtime and only the resulting path is
        // reapplied under the write lock.
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut instance = snapshot;
            // An empty profile resolves to the *default* profile rather than
            // failing, which would aim the persist at another profile's
            // sessions.json. The compare-and-set inside the reconcile makes
            // that a no-op, but refuse outright rather than lean on it.
            anyhow::ensure!(
                !instance.source_profile.is_empty(),
                "session has no source profile; refusing worktree path reconciliation"
            );
            let storage = crate::session::Storage::open_unwatched(&instance.source_profile)?;
            let resolution = crate::session::worktree_reconcile::reconcile_and_persist(
                &storage,
                &mut instance,
                &mut Default::default(),
            )?;
            anyhow::Ok((resolution, instance))
        })
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile skipped: {error}");
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile join failed: {error}");
                continue;
            }
        };
        let crate::session::worktree_reconcile::WorktreePathResolution::Moved(_) = reconciled.0
        else {
            continue;
        };
        let mut instances = state.instances.write().await;
        if let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) {
            instance.project_path = reconciled.1.project_path;
        }
    }
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
    for (id, _profile) in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        let snapshot = {
            let instances = state.instances.read().await;
            match instances.iter().find(|instance| instance.id == id) {
                Some(instance) if instance.is_trashed() => instance.clone(),
                _ => continue,
            }
        };
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut instance = snapshot;
            let changed = crate::session::trash::reconcile_trashed_transition(&mut instance)?;
            anyhow::Ok((changed, instance))
        })
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "trash reconcile skipped: {error}");
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "trash reconcile join failed: {error}");
                continue;
            }
        };
        if !reconciled.0 {
            continue;
        }
        let moved = reconciled.1;
        let mut instances = state.instances.write().await;
        if let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) {
            instance.project_path = moved.project_path;
            instance.pre_trash_project_path = moved.pre_trash_project_path;
            instance.lifecycle_generation = moved.lifecycle_generation;
            instance.lifecycle_reservation = moved.lifecycle_reservation;
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

        // Submission authority before `instance_lock`, as the permanent
        // `DELETE` path takes them: teardown must not start under an in-flight
        // queue drain (#3650). A row that vanished since the snapshot is
        // skipped here rather than below.
        let Some(_submission) = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
        else {
            continue;
        };
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
            Ok((_removed, _messages)) => tracing::info!(
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return super::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Serialize concurrent mutations. Prompt submission first, per
    // `prompt_submission`: a queue drain snapshots an idle turn under that
    // guard and never takes `instance_lock`, so without it the delivery runs
    // against a worker, worktree and transcript this is tearing down (#3650).
    // Both guards are owned so they move into the detached deletion task below
    // and stay held until the bookkeeping finishes, rather than only until
    // this request future is dropped.
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return super::session_not_found();
    };
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
        let _submission = submission;

        // Mark as Deleting so polling clients see the status change
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.status = Status::Deleting;
            }
        }

        match purge_session_artifacts(&state, &id, instance, &body, recent_entry).await {
            Ok((removed, messages)) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    // A concurrent restore can keep the row (removed=false); do
                    // not claim it was deleted in that case.
                    "status": if removed { "deleted" } else { "kept" },
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

// --- Delete workspace (atomic multi-session) ---

/// Body for `DELETE /api/workspaces`. `session_ids` is the full set of
/// sessions in one web-UI workspace, all sharing a single git worktree +
/// branch, ordered so the first id is the worktree owner (the web
/// `sessions[0]` primary). The cleanup flags mirror [`DeleteSessionBody`]:
/// they apply to the whole workspace, and the shared worktree/branch is
/// removed exactly once, on the owner.
#[derive(Default, Deserialize)]
pub struct DeleteWorkspaceBody {
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub delete_sandbox: bool,
    #[serde(default)]
    pub force_delete: bool,
    #[serde(default)]
    pub keep_scratch: bool,
}

#[derive(Serialize)]
struct WorkspaceDeleteFailure {
    id: String,
    error: String,
}

/// Drop duplicate session ids while preserving first-seen order. A workspace
/// delete must never list the same session twice: with `["owner", "owner"]`
/// the first pass would delete the owner using the record-only sibling flags
/// and the second pass would skip the now-missing row, returning success
/// without ever removing the shared worktree or branch (#2536 review).
fn dedupe_session_ids(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Build the per-session deletion order for a workspace delete. All sessions
/// in a workspace share one git worktree + branch, so worktree/branch cleanup
/// must run exactly once. The owner (`session_ids[0]`, the web primary)
/// carries the caller's worktree/branch flags and is deleted LAST; every
/// sibling is deleted first with worktree/branch removal forced off.
///
/// Owner-last is the safety property. Siblings hold only a record + container,
/// never the shared worktree, so tearing them down while the worktree is still
/// present lets a sibling failure abort before the worktree is touched, leaving
/// nothing orphaned. Deleting the owner first (worktree gone) and then failing
/// on a sibling would strand a live record pointing at a deleted worktree, the
/// exact failure #2536 exists to remove.
fn order_workspace_deletion(
    session_ids: &[String],
    body: &DeleteWorkspaceBody,
) -> Vec<(String, DeleteSessionBody)> {
    let Some((owner, siblings)) = session_ids.split_first() else {
        return Vec::new();
    };
    let sibling_body = DeleteSessionBody {
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        keep_scratch: body.keep_scratch,
    };
    let owner_body = DeleteSessionBody {
        delete_worktree: body.delete_worktree,
        delete_branch: body.delete_branch,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        keep_scratch: body.keep_scratch,
    };
    let mut plan: Vec<(String, DeleteSessionBody)> = siblings
        .iter()
        .map(|id| (id.clone(), sibling_body.clone()))
        .collect();
    plan.push((owner.clone(), owner_body));
    plan
}

/// Owner-worktree dirty preflight for a workspace delete. Mirrors the per-
/// session dirty gate in `perform_deletion` so a non-force delete of a dirty
/// shared worktree is refused before any session is torn down, keeping dirty +
/// non-force all-or-nothing. Returns the first dirty message found.
fn workspace_dirty_message(instance: &Instance) -> Option<String> {
    if let Some(wt) = &instance.worktree_info {
        if wt.managed_by_aoe {
            let path = std::path::PathBuf::from(&instance.project_path);
            if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                return Some(msg);
            }
        }
    }
    if let Some(ws) = &instance.workspace_info {
        if ws.cleanup_on_delete {
            for repo in &ws.repos {
                if repo.managed_by_aoe {
                    let path = std::path::PathBuf::from(&repo.worktree_path);
                    if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                        return Some(format!("{}: {}", repo.name, msg));
                    }
                }
            }
        }
    }
    None
}

/// Tear down every session in a workspace: record-only siblings first, then the
/// shared-worktree owner last (see [`order_workspace_deletion`]). Each session
/// goes through the shared [`purge_session_artifacts`].
///
/// The owner's submission guard and instance lock are acquired up front and
/// held for the whole teardown, and the dirty-worktree gate is re-checked
/// under them right before any sibling is torn down. This serializes the dirty
/// check with the teardown so dirty + non-force stays all-or-nothing even if
/// the worktree is dirtied between the handler preflight and now, and it
/// cannot deadlock: a session belongs to exactly one workspace, so two
/// workspace deletes never contend for each other's locks, and single-session
/// deletes only ever hold one session's locks at a time. Sibling locks are
/// then taken one session at a time. A session already gone (a retention purge
/// won the race) is skipped, not failed; a
/// pre-owner failure aborts before the worktree is removed, so the shared
/// worktree keeps its live owning session rather than being orphaned. A
/// session whose row a concurrent restore kept (`removed == false`) is reported
/// neither deleted nor failed.
async fn purge_workspace_artifacts(
    state: &Arc<AppState>,
    owner_id: String,
    plan: Vec<(String, DeleteSessionBody)>,
    owner_needs_dirty_check: bool,
) -> (Vec<String>, Vec<WorkspaceDeleteFailure>, Vec<String>) {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut messages = Vec::new();

    // Hold the owner's locks across the entire teardown (see doc comment).
    // `None` only when the owner row already vanished; the plan loop skips it.
    let _owner_submission = state
        .session_service
        .prompt_submission_for_session(&owner_id)
        .await;
    let owner_lock = state.instance_lock(&owner_id).await;
    let _owner_guard = owner_lock.lock_owned().await;

    // Authoritative dirty re-check under the owner lock, before any sibling is
    // torn down (#2536 review). If the worktree went dirty since the handler
    // preflight, abort with nothing deleted.
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(&owner) {
                failed.push(WorkspaceDeleteFailure {
                    id: owner_id,
                    error: format!("Workspace: {msg}"),
                });
                return (deleted, failed, messages);
            }
        }
    }

    for (id, body) in plan {
        // The owner's locks are already held; only siblings need their own,
        // one at a time. Re-locking the owner here would self-deadlock.
        let _sibling_locks = if id == owner_id {
            None
        } else {
            Some((
                state
                    .session_service
                    .prompt_submission_for_session(&id)
                    .await,
                state.instance_lock(&id).await.lock_owned().await,
            ))
        };

        let instance = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == id).cloned()
        };
        let Some(instance) = instance else {
            // Already deleted (a concurrent retention auto-purge won the race).
            // The row we were asked to delete is gone, so this is a no-op, not
            // a failure.
            continue;
        };

        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.status = Status::Deleting;
            }
        }

        let recent_entry = crate::session::recent_project_entry_for(&instance);
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok((removed, mut msgs)) => {
                messages.append(&mut msgs);
                // A concurrent restore can keep the row (removed=false); only
                // report rows that were actually removed as deleted, so the
                // client never drops local state for a session that survived.
                if removed {
                    deleted.push(id.clone());
                }
            }
            Err(msg) => {
                mark_delete_error(state, &id, msg.clone()).await;
                failed.push(WorkspaceDeleteFailure {
                    id: id.clone(),
                    error: msg,
                });
                // Stop before the remaining plan entries. The owner is last, so
                // a sibling failure here leaves the shared worktree intact with
                // its owning session still present, never orphaned.
                break;
            }
        }
    }

    (deleted, failed, messages)
}

/// `DELETE /api/workspaces`: atomic multi-session workspace delete. Replaces
/// the web client's N-call fan-out (one `DELETE /api/sessions/:id` per session)
/// with a single call that tears the whole workspace down in the correct order
/// under one detached task, so a mid-delete client disconnect can no longer
/// leave the workspace half-removed. See #2536.
pub async fn delete_workspace(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DeleteWorkspaceBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return super::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();
    // Dedupe up front so a repeated id can't have the owner deleted with
    // sibling flags and then skipped (#2536 review).
    let session_ids = dedupe_session_ids(&body.session_ids);
    let Some(owner_id) = session_ids.first().cloned() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "message": "session_ids must not be empty",
            })),
        )
            .into_response();
    };

    // CityHall: `purge_workspace_artifacts` tears down EVERY id in the list, not
    // just the owner, so every id (not only `session_ids.first()`) must be a
    // structured session this mode created. Otherwise a client could smuggle a
    // foreign plain session in as a sibling and have it destroyed. See #7.
    if let Some(resp) = cityhall_block_any_non_structured(&state, &session_ids).await {
        return resp;
    }

    let owner_needs_dirty_check = body.delete_worktree && !body.force_delete;

    // Preflight: refuse a non-force delete of a dirty shared worktree before
    // tearing down any session, so dirty + non-force stays all-or-nothing. The
    // owner (session_ids[0]) is the session that carries the shared worktree.
    // This is a fast early 409 for the common case; `purge_workspace_artifacts`
    // re-checks authoritatively under the owner lock.
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(&owner) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "dirty_worktree",
                        "message": msg,
                    })),
                )
                    .into_response();
            }
        }
    }

    let plan = order_workspace_deletion(&session_ids, &body);

    // Detached task, mirroring `delete_session`: the teardown must run to
    // completion even if the client disconnects mid-delete.
    let join = tokio::spawn(async move {
        purge_workspace_artifacts(&state, owner_id, plan, owner_needs_dirty_check).await
    });

    match join.await {
        Ok((deleted, failed, messages)) => {
            if deleted.is_empty() && !failed.is_empty() {
                let msg = failed
                    .iter()
                    .map(|f| f.error.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::error!(target: "http.api.sessions", "workspace delete failed: {msg}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "deletion_failed",
                        "message": msg,
                        "failed": failed,
                    })),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": if failed.is_empty() { "deleted" } else { "partial" },
                    "deleted": deleted,
                    "failed": failed,
                    "messages": messages,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions",
                "Workspace deletion task panicked or was cancelled: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal",
                    "message": "Workspace deletion task failed",
                })),
            )
                .into_response()
        }
    }
}

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
fn find_by_idempotency_key<'a>(instances: &'a [Instance], key: &str) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.idempotency_key.as_deref() == Some(key))
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
/// `Err` reports an unforkable terminal agent or missing parent id; a
/// structured request is already rejected by the caller's
/// `agent_is_structured_fork_capable` guard before it reaches here.
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
        crate::session::capture::generate_session_uuid(),
    )
}

/// True when a create request asks to both import an existing session and fork
/// a parent. The two seed the new session from different sources, so allowing
/// both would produce a contradictory half-imported, half-forked session.
/// Trailing whitespace is treated as unset, matching the per-field guards.
fn both_import_and_fork_set(body: &CreateSessionBody) -> bool {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    set(&body.import_acp_session_id) && set(&body.fork_from)
}

/// Thin server-side alias for [`crate::session::fork::structured_fork_capable`],
/// the single source of truth for "can this agent run the ACP `session/fork`
/// handshake?". Shared by the `SessionResponse.acp_can_fork` projection (the web
/// "Fork" affordance) and the create-time guard so they cannot drift.
fn agent_is_structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    crate::session::fork::structured_fork_capable(tool, agent_name)
}

/// True iff the agent can run a structured (ACP) session in this project: a
/// built-in ACP agent in the registry, or a custom tool with a valid
/// `agent_acp_cmd`. Mirrors the post-build capability check (below) so
/// CityHall mode can reject a non-ACP agent up front instead of letting the
/// session silently downgrade to the terminal view. See #7.
/// The ACP registry key a create request resolves to: an explicit `agent_name`
/// when present, else the tool name. Shared by the capability check and the
/// allowlist check (#3241) so the two cannot judge different agents.
fn acp_agent_key<'a>(tool: &'a str, agent_name: Option<&'a str>) -> &'a str {
    agent_name.filter(|s| !s.is_empty()).unwrap_or(tool)
}

pub(super) fn agent_is_acp_capable(
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
    let session =
        crate::session::repo_config::resolve_config_with_repo_or_warn(profile, project_path)
            .session;
    session
        .agent_acp_cmd
        .get(resolved)
        .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(resolved, cmd).is_ok())
        // A custom agent inheriting a registry-backed base via `agent_detect_as`
        // spawns fine through the base adapter, so report it capable up front.
        || crate::acp::inherited_acp_base(resolved, &session.agent_detect_as).is_some()
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
pub(crate) struct CreateHookPlan {
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
pub(crate) fn resolve_create_hook_plan(
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
pub(crate) fn run_create_hooks(
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

/// CityHall structured-target gate for per-session lifecycle / metadata routes.
/// CityHall only ever creates structured sessions and `list_sessions` hides
/// everything else, so a mutation must refuse any non-structured target (or an
/// unknown id): otherwise a locked-down client could enumerate a pre-existing
/// plain/terminal session (from the TUI, `aoe add`, or another client on the
/// same daemon) and respawn it (re-running its stored `command_override` host
/// binary via `build_host_command`), destroy it, or edit it. Returns the
/// canonical CityHall 403 (never a 404, so the mode does not leak which ids
/// exist); `None` in normal mode or for a genuine structured target. See #7.
async fn cityhall_block_non_structured(
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
    (!is_structured_target).then(super::cityhall_response)
}

/// Plural [`cityhall_block_non_structured`]: refuse unless EVERY id resolves to
/// a structured session this mode created. Used by multi-session teardown
/// (`delete_workspace`), which acts on all ids, not just the owner. See #7.
async fn cityhall_block_any_non_structured(
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
    (!all_structured).then(super::cityhall_response)
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
async fn wait_until_left_starting(
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
        return super::read_only_response();
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
        if acp_capable && !super::agent_policy().await.allows(agent_key) {
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
                resp.tie_workdir_to_name = crate::session::profile_config::resolve_config_or_warn(
                    &instance.source_profile,
                )
                .session
                .tie_workdir_to_name;
            }
            if !resp.acp_capable {
                let session = crate::session::repo_config::resolve_config_with_repo_or_warn(
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

fn apply_post_restart_sync(live: &mut Instance, before: &Instance, started: &Instance) -> bool {
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
fn apply_cascade_state_sync(live: &mut Instance, before: &Instance, started: &Instance) {
    if started.lifecycle_generation < live.lifecycle_generation {
        return;
    }
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
            // `ensure_session` respawns on demand before a WS attach/send,
            // the server-side analog of `ensure_pane_ready`: always `Allow`,
            // ignoring `auto_resume_on_restart`, so attaching does not drop
            // the agent's context. The instance-level cascade holds the
            // lifecycle lock across final poller drain, exact-pane OMP
            // capture, kill, and relaunch.
            match inst.restart_with_resume_policy(
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
                if apply_post_restart_sync(inst, &sync_base, &started) {
                    inst.status = crate::session::Status::Error;
                    inst.last_error = Some(msg.clone());
                }
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
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
    /// Worktree path this entry's diff was computed in. The web base
    /// picker queries it for that repo's branch list, so a workspace
    /// member's typeahead lists its own branches rather than the launch
    /// repo's. See #3329.
    pub repo_path: String,
    /// This entry's explicit override, when one is set. Absent means
    /// `base_branch` came from the recorded creation base, the profile
    /// default, or auto-detection, so the client hides its reset
    /// affordance. See #3329.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_override: Option<String>,
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

/// One repo's worth of diff context: a name (for workspace members),
/// the filesystem path the diff helper walks, and the two base-branch
/// layers that vary per repo. See #1047, #3329.
#[derive(Clone, Debug)]
struct DiffRepo {
    /// Workspace member name, or None for single-repo sessions.
    name: Option<String>,
    path: String,
    /// Explicit override for this entry's diff base, set via
    /// `PATCH /api/sessions/{id}/diff-base`, the `aoe session set-base`
    /// CLI, or the TUI diff view's `b` keybind. For a workspace member
    /// that is `WorkspaceRepo::base_branch_override`; for a single-repo
    /// session's own checkout it is `Instance::base_branch_override`.
    /// See #970, #3329.
    base_override: Option<String>,
    /// The branch this entry's worktree was created from, recorded at
    /// creation. `WorkspaceRepo::base_branch` for a workspace member,
    /// `WorktreeInfo::base_branch` for a single-repo session. Slots
    /// below the explicit override but above the profile default and
    /// auto-detection. See #1951, #3329.
    recorded_base: Option<String>,
}

struct DiffContext {
    repos: Vec<DiffRepo>,
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
    Ok(DiffContext {
        repos: diff_repos_of(inst),
    })
}

/// The repo entries for one session, split out of [`resolve_diff_repos`] so the
/// per-repo base plumbing is testable without an `AppState`.
fn diff_repos_of(inst: &crate::session::Instance) -> Vec<DiffRepo> {
    // A session with any repo record (a creation-time workspace, repos attached
    // later, or both) lists one entry per repo. A session with none falls back
    // to its project_path, which is the single-repo flow unchanged.
    let mut repos: Vec<DiffRepo> = inst
        .all_repos()
        .iter()
        .map(|r| DiffRepo {
            name: Some(r.name.clone()),
            path: r.worktree_path.clone(),
            base_override: r.base_branch_override.clone(),
            recorded_base: r.base_branch.clone(),
        })
        .collect();
    if inst.workspace_info.is_none() {
        // A session with no repo records is single-repo: its own checkout is
        // the only entry, and the session-level override is that entry's
        // override. `attach_project` converts a session into a workspace, so
        // a named entry and this unnamed one never coexist. See #3329.
        repos.insert(
            0,
            DiffRepo {
                name: None,
                path: inst.project_path.clone(),
                base_override: inst.base_branch_override.clone(),
                recorded_base: inst
                    .worktree_info
                    .as_ref()
                    .and_then(|w| w.base_branch.clone()),
            },
        );
    }
    repos
}

/// Resolve the diff base for one repo. The repo's own override wins
/// over the base its worktree was recorded as forked from, which wins
/// over the profile's `DiffConfig.default_branch`, which wins over
/// auto-detection (`get_default_base_ref`). Every layer above the
/// config default is per repo, so each workspace member resolves
/// independently. See #970, #1951, #3329.
fn resolve_diff_base(
    override_value: Option<&str>,
    recorded_base: Option<&str>,
    config_default: Option<&str>,
    repo_path: &std::path::Path,
) -> String {
    if let Some(v) = override_value.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = recorded_base.map(str::trim).filter(|v| !v.is_empty()) {
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
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
                repo.base_override.as_deref(),
                repo.recorded_base.as_deref(),
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
                repo_path: repo.path.clone(),
                base_override: repo.base_override.clone(),
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
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
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
    let base_override = selected_repo.base_override;
    let recorded_base = selected_repo.recorded_base;
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
                base_override.as_deref(),
                recorded_base.as_deref(),
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
pub struct SessionFileQuery {
    pub path: String,
}

/// Response for the session file-read endpoint. Mirrors the typed shape of its
/// sibling [`RichFileContentsResponse`]; `content` is empty for a binary or
/// truncated file (the client renders a notice instead).
#[derive(Serialize)]
pub struct SessionFileResponse {
    pub content: String,
    pub is_binary: bool,
    pub truncated: bool,
}

/// Read a session file for the dashboard file viewer (#3088).
///
/// Git-agnostic (works on non-git scratch sessions). A read is allowed when the
/// canonical target is under a session project root (project_path + worktree
/// paths) or is a path the agent touched this session, recovered from the ACP
/// event log. Confinement and bounded reading live in the private
/// `file_provenance` module.
pub async fn session_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SessionFileQuery>,
) -> impl IntoResponse {
    // Reads workspace file contents: the same code-inspection surface as the
    // diff reads, and the Files pane is hidden in CityHall, so close it too.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let ctx = match resolve_diff_repos(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let project_paths: Vec<std::path::PathBuf> = ctx
        .repos
        .iter()
        .map(|r| std::path::PathBuf::from(&r.path))
        .collect();
    let store = state.acp_event_store.clone();
    let session_id = id.clone();
    let requested = query.path.clone();

    let result = tokio::task::spawn_blocking(move || {
        // Canonicalize project roots up front; a root that no longer resolves
        // is dropped so a stale worktree can't break or widen confinement.
        let roots: Vec<std::path::PathBuf> = project_paths
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();

        // Provenance fallback: page the whole session log and collect the paths
        // the agent touched. Deferred behind a closure so it runs only when the
        // target is outside every project root; a workspace file (the common
        // case) never pays for the replay.
        // ponytail: per-request scan on the miss path; cache per session keyed
        // on highest_seq if it shows up hot on long/active sessions.
        let touched = || {
            let mut events = Vec::new();
            let mut since = 0u64;
            loop {
                let page = store.replay_page(&session_id, since, Some(1000));
                let advance = page.last_scanned_seq;
                events.extend(page.events);
                match (page.has_more, advance) {
                    (true, Some(seq)) => since = seq,
                    _ => break,
                }
            }
            super::file_provenance::collect_touched_paths(&events)
        };

        let confined = super::file_provenance::confine_path(
            &roots,
            touched,
            std::path::Path::new(&requested),
        )?;
        let (content, is_binary, truncated) =
            super::file_provenance::read_confined(&confined, MAX_CONTENTS_BYTES)?;
        Ok::<_, (StatusCode, &'static str)>(SessionFileResponse {
            content,
            is_binary,
            truncated,
        })
    })
    .await;

    match result {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err((status, msg))) => (
            status,
            Json(serde_json::json!({"error": "file_read", "message": msg})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "session_file panicked: {}", e);
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

    /// `remove_instance` is the only way a row leaves `state.instances` on the
    /// delete path, so the epoch bump has to be tied to an actual removal
    /// rather than to reaching the call. Bumping unconditionally would spend
    /// an epoch on the final commit block after the structured purge's early
    /// removal already took the row, dropping a reload that was perfectly
    /// valid; not bumping at all leaves the window a stale reload uses to put
    /// a deleted row back.
    #[test]
    fn remove_instance_bumps_the_epoch_only_when_it_removes_a_row() {
        let epoch = std::sync::atomic::AtomicU64::new(0);
        let read = || epoch.load(std::sync::atomic::Ordering::SeqCst);
        let mut instances = vec![
            Instance::new("keep", "/tmp/keep"),
            Instance::new("doomed", "/tmp/doomed"),
        ];
        let doomed_id = instances[1].id.clone();

        remove_instance(&mut instances, &doomed_id, &epoch);
        assert_eq!(read(), 1, "a real removal bumps");
        assert_eq!(
            instances
                .iter()
                .map(|i| i.title.as_str())
                .collect::<Vec<_>>(),
            vec!["keep"]
        );

        // The structured purge reaches the final commit block after its early
        // removal already took the row. Nothing left to remove, nothing to
        // invalidate, so no epoch is spent.
        remove_instance(&mut instances, &doomed_id, &epoch);
        assert_eq!(read(), 1, "a no-op removal does not bump");

        remove_instance(&mut instances, "never-existed", &epoch);
        assert_eq!(read(), 1, "an unknown id does not bump");
    }
    fn build_rename_test_state(
        persisted: Vec<Instance>,
        cached: Vec<Instance>,
    ) -> (Storage, std::sync::Arc<crate::server::AppState>) {
        let storage = Storage::new_unwatched("default").unwrap();
        storage
            .update(|instances, _groups| {
                *instances = persisted;
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(cached);
        (storage, state)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rename_session_rejects_duplicate_and_preserves_newer_cache() {
        use axum::body::to_bytes;

        let _guard = crate::session::test_support::isolate_app_dir();
        let mut existing = Instance::new("main branch", "/tmp/repo/");
        existing.source_profile = "default".to_string();
        let mut target = Instance::new("throwaway", "/tmp/repo");
        target.source_profile = "default".to_string();
        let target_id = target.id.clone();
        let mut stale_existing = existing.clone();
        stale_existing.title = "previous title".to_string();
        let mut stale_target = target.clone();
        stale_target.project_path = "/tmp/stale".to_string();
        let (storage, state) =
            build_rename_test_state(vec![existing, target], vec![stale_existing, stale_target]);

        let response = rename_session(
            State(state.clone()),
            Path(target_id.clone()),
            Ok(Json(RenameSessionBody {
                title: "main branch".to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 2048).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("duplicate_session"));
        assert_eq!(
            state
                .instances
                .read()
                .await
                .iter()
                .find(|instance| instance.id == target_id)
                .unwrap()
                .title,
            "throwaway"
        );

        storage
            .update(|instances, _groups| {
                instances
                    .iter_mut()
                    .find(|instance| instance.id != target_id)
                    .unwrap()
                    .title = "other".to_string();
                Ok(())
            })
            .unwrap();
        // A user action can advance the live cache while the disk snapshot the
        // rename will persist still has the older row. Publication must patch
        // only rename-owned identity fields, not replace this favorite.
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|instance| instance.id == target_id)
            .unwrap()
            .favorite();
        let response = rename_session(
            State(state.clone()),
            Path(target_id.clone()),
            Ok(Json(RenameSessionBody {
                title: "main branch".to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let instances = state.instances.read().await;
        let target = instances
            .iter()
            .find(|instance| instance.id == target_id)
            .unwrap();
        assert_eq!(target.title, "main branch");
        assert_eq!(target.project_path, "/tmp/repo");
        assert_eq!(target.source_profile, "default");
        assert!(
            target.is_favorited(),
            "newer cached user action must survive rename publication"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rename_session_rejects_tied_drifted_path_collision() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let _tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let mut existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
        existing.source_profile = "default".to_string();
        let mut drifted = Instance::new("main branch", "/tmp/worktrees/drifted");
        drifted.source_profile = "default".to_string();
        drifted.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "main-branch".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let drifted_id = drifted.id.clone();
        let (_storage, state) = build_rename_test_state(
            vec![existing.clone(), drifted.clone()],
            vec![existing, drifted],
        );

        let response = rename_session(
            State(state),
            Path(drifted_id),
            Ok(Json(RenameSessionBody {
                title: "main branch".to_string(),
                rename_branch: false,
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_renames_commit_only_one_same_identity_pair() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let mut first = Instance::new("first", "/tmp/shared");
        first.source_profile = "default".to_string();
        let mut second = Instance::new("second", "/tmp/shared/");
        second.source_profile = "default".to_string();
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let storage = Storage::new_unwatched("default").unwrap();
        storage
            .update(|instances, _groups| {
                *instances = vec![first.clone(), second.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![first, second]);

        let first_rename = rename_session(
            State(state.clone()),
            Path(first_id),
            Ok(Json(RenameSessionBody {
                title: "shared title".to_string(),
                rename_branch: false,
            })),
        );
        let second_rename = rename_session(
            State(state.clone()),
            Path(second_id),
            Ok(Json(RenameSessionBody {
                title: "shared title".to_string(),
                rename_branch: false,
            })),
        );
        let (first_response, second_response) = tokio::join!(first_rename, second_rename);
        let statuses = [
            first_response.into_response().status(),
            second_response.into_response().status(),
        ];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count(),
            1
        );
        assert_eq!(
            storage
                .load()
                .unwrap()
                .iter()
                .filter(|instance| {
                    instance.title == "shared title"
                        && instance.project_path.trim_end_matches('/') == "/tmp/shared"
                })
                .count(),
            1
        );
    }

    // #2536: the workspace-delete order must tear down record-only siblings
    // first and the shared-worktree owner last, so a sibling failure can never
    // orphan a session against an already-removed worktree.
    mod workspace_deletion {
        use super::*;

        fn body() -> DeleteWorkspaceBody {
            DeleteWorkspaceBody {
                session_ids: vec![],
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: true,
                force_delete: false,
                keep_scratch: false,
            }
        }

        #[test]
        fn owner_is_last_and_siblings_are_record_only() {
            let ids = vec!["owner".to_string(), "sib1".to_string(), "sib2".to_string()];
            let plan = order_workspace_deletion(&ids, &body());

            let order: Vec<&str> = plan.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(
                order,
                vec!["sib1", "sib2", "owner"],
                "siblings must precede the owner so the worktree owner is torn down last"
            );

            // Siblings never touch the shared worktree/branch.
            for (id, b) in &plan[..2] {
                assert!(
                    !b.delete_worktree,
                    "sibling {id} must not remove the worktree"
                );
                assert!(!b.delete_branch, "sibling {id} must not delete the branch");
                assert!(
                    b.delete_sandbox,
                    "sibling {id} still tears down its own sandbox"
                );
            }
            // The owner (last) carries the caller's worktree/branch flags.
            let (owner_id, owner_body) = plan.last().unwrap();
            assert_eq!(owner_id, "owner");
            assert!(owner_body.delete_worktree);
            assert!(owner_body.delete_branch);
        }

        #[test]
        fn single_session_is_owner_only_with_full_flags() {
            let ids = vec!["solo".to_string()];
            let plan = order_workspace_deletion(&ids, &body());
            assert_eq!(plan.len(), 1);
            let (id, b) = &plan[0];
            assert_eq!(id, "solo");
            assert!(
                b.delete_worktree,
                "the only session owns the worktree cleanup"
            );
            assert!(b.delete_branch);
        }

        #[test]
        fn empty_input_is_empty_plan() {
            assert!(order_workspace_deletion(&[], &body()).is_empty());
        }

        #[test]
        fn worktree_flags_off_stay_off_for_owner() {
            let mut b = body();
            b.delete_worktree = false;
            b.delete_branch = false;
            let ids = vec!["owner".to_string(), "sib".to_string()];
            let plan = order_workspace_deletion(&ids, &b);
            let (_, owner_body) = plan.last().unwrap();
            assert!(!owner_body.delete_worktree);
            assert!(!owner_body.delete_branch);
        }

        #[test]
        fn dedupe_drops_repeats_preserving_first_seen_order() {
            let ids = vec![
                "a".to_string(),
                "b".to_string(),
                "a".to_string(),
                "c".to_string(),
                "b".to_string(),
            ];
            assert_eq!(dedupe_session_ids(&ids), vec!["a", "b", "c"]);
        }

        #[test]
        fn duplicate_owner_still_removes_the_worktree() {
            // #2536 review: ["owner", "owner"] must not delete the owner with
            // sibling (record-only) flags and then skip the repeat. After
            // dedupe the single owner entry keeps the real worktree flags.
            let ids = dedupe_session_ids(&["owner".to_string(), "owner".to_string()]);
            assert_eq!(ids, vec!["owner"]);
            let plan = order_workspace_deletion(&ids, &body());
            assert_eq!(plan.len(), 1);
            let (id, b) = &plan[0];
            assert_eq!(id, "owner");
            assert!(
                b.delete_worktree,
                "the deduped owner must still own the worktree cleanup"
            );
            assert!(b.delete_branch);
        }
    }

    // CityHall create-time capability gate (#7): create_session rejects a
    // non-ACP agent up front instead of downgrading to a hidden terminal view.
    mod cityhall_capability {
        use super::*;
        use crate::session::test_support::isolate_app_dir;
        use serial_test::serial;

        #[test]
        fn builtin_agent_is_acp_capable() {
            // Built-in ACP agents resolve via the registry without reading
            // config, so the gate accepts them regardless of the project path.
            assert!(agent_is_acp_capable(
                "default",
                std::path::Path::new("/nonexistent"),
                "claude",
                None,
            ));
        }

        #[test]
        #[serial]
        fn an_explicit_agent_name_keys_the_custom_acp_cmd_lookup() {
            // An explicit `agent_name` can point at a different `agent_acp_cmd`
            // entry than `tool`, and `resolve_agent_spec` resolves the custom map
            // by that same name. Keying this lookup off `tool` reported
            // not-capable for an agent that spawns fine, which skipped the
            // up-front 403 in favor of a late refusal at spawn.
            let _tmp = isolate_app_dir();
            crate::session::config::update_config(|c| {
                c.session
                    .agent_acp_cmd
                    .insert("acp-helper".into(), "acp-helper --acp".into());
            })
            .unwrap();
            let path = std::path::Path::new("/nonexistent");
            assert!(agent_is_acp_capable(
                "default",
                path,
                "plain-tool",
                Some("acp-helper"),
            ));
            // Without the override there is nothing to resolve to, so the same
            // tool stays not-capable.
            assert!(!agent_is_acp_capable("default", path, "plain-tool", None));
        }

        #[test]
        #[serial]
        fn unknown_tool_is_not_acp_capable() {
            let _tmp = isolate_app_dir();
            assert!(!agent_is_acp_capable(
                "default",
                std::path::Path::new("/nonexistent"),
                "definitely-not-a-real-tool",
                None,
            ));
        }

        /// Why `acp_enable` gates on this predicate and not on
        /// `pick_agent_for_tool`: the default-agent fallback always names a
        /// registry entry, so a post-fallback registry lookup reports every
        /// tool capable and would switch a terminal-only session into a
        /// structured one running some other agent.
        #[test]
        #[serial]
        fn the_default_agent_fallback_is_not_a_capability_signal() {
            let _tmp = isolate_app_dir();
            let fallback = crate::session::config::DEFAULT_ACP_AGENT;
            assert!(
                crate::acp::AgentRegistry::with_defaults()
                    .get(fallback)
                    .is_some(),
                "the fallback must be spawnable, which is what makes it useless as a gate"
            );
            assert!(!agent_is_acp_capable(
                "default",
                std::path::Path::new("/nonexistent"),
                "plain-tool",
                None,
            ));
        }
    }

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
    // #3058 review: the force_smart_rename preflight must resolve config with
    // the repo-aware resolver so a repo-local agent_command_override is honored.
    // Reverting to the profile-only resolver would miss the override and fall
    // through to the "no prompt yet" path (both are 409, so this asserts the
    // body message, not just the status).
    #[tokio::test]
    #[serial_test::serial]
    async fn force_smart_rename_preflight_sees_command_override_but_not_from_a_repo() {
        use axum::body::to_bytes;

        async fn preflight_message(repo: &std::path::Path) -> String {
            let mut inst = Instance::new("Vikings", repo.to_str().unwrap());
            inst.tool = "claude".to_string();
            inst.source_profile = "default".to_string();
            inst.view = crate::session::View::Structured;
            let id = inst.id.clone();

            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let resp = force_smart_rename(axum::extract::State(state), axum::extract::Path(id))
                .await
                .into_response();
            assert_eq!(resp.status(), StatusCode::CONFLICT);
            let body = to_bytes(resp.into_body(), 1024).await.unwrap();
            String::from_utf8_lossy(&body).to_string()
        }

        let tmp_home = tempfile::tempdir().expect("tempdir HOME");
        let repo = tempfile::tempdir().expect("tempdir repo");
        // SAFETY: serialized by #[serial]; matches other HOME-swapping tests.
        unsafe {
            std::env::set_var("HOME", tmp_home.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp_home.path().join(".config"));
        }

        // A repo declaring the override changes nothing: command-bearing
        // session fields are not repo-overridable (#3154).
        let cfg_dir = repo.path().join(".agent-of-empires");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[session.agent_command_override]\nclaude = \"wrapper-3058\"\n",
        )
        .unwrap();
        let msg = preflight_message(repo.path()).await;
        assert!(
            !msg.contains("command is overridden"),
            "a repo must not be able to declare the agent command override; got: {msg}"
        );

        // The user's own override is still seen through the repo-aware
        // resolution the preflight routes through (#3058).
        let app_dir = isolated_app_dir(tmp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            "[session.agent_command_override]\nclaude = \"wrapper-3058\"\n",
        )
        .unwrap();
        let msg = preflight_message(repo.path()).await;
        assert!(
            msg.contains("command is overridden"),
            "preflight must see the user's override via repo-aware resolution; got: {msg}"
        );
    }

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
        let _envelope = list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(ListSessionsQuery { state: None }),
        )
        .await;
        let misses = LIST_SESSIONS_RESOLVER_MISSES.load(Ordering::Relaxed);

        assert_eq!(
            misses, 2,
            "shared cache must resolve exactly once per unique (profile, project_path) across both overlays; got {misses}",
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_sessions_state_filter() {
        let mut live = Instance::new("live", "/tmp/scope-live");
        live.id = "scope-live".to_string();
        let mut trashed = Instance::new("trashed", "/tmp/scope-trashed");
        trashed.id = "scope-trashed".to_string();
        trashed.trash();
        let mut archived = Instance::new("archived", "/tmp/scope-archived");
        archived.id = "scope-archived".to_string();
        archived.archived_at = Some(chrono::Utc::now());

        let state = crate::server::test_support::build_test_app_state(vec![
            live.clone(),
            trashed.clone(),
            archived.clone(),
        ]);

        let ids = |envelope: &SessionsEnvelope| -> Vec<String> {
            envelope.sessions.iter().map(|s| s.id.clone()).collect()
        };

        let all = list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(ListSessionsQuery { state: None }),
        )
        .await;
        assert_eq!(
            ids(&all).len(),
            3,
            "no param stays unfiltered (back-compat)"
        );

        let live_only = list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(ListSessionsQuery {
                state: Some(crate::session::SessionScope::Live),
            }),
        )
        .await;
        assert_eq!(ids(&live_only), vec!["scope-live".to_string()]);

        let trashed_only = list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(ListSessionsQuery {
                state: Some(crate::session::SessionScope::Trashed),
            }),
        )
        .await;
        assert_eq!(ids(&trashed_only), vec!["scope-trashed".to_string()]);

        let explicit_all = list_sessions(
            axum::extract::State(state),
            axum::extract::Query(ListSessionsQuery {
                state: Some(crate::session::SessionScope::All),
            }),
        )
        .await;
        assert_eq!(ids(&explicit_all).len(), 3);
    }

    #[tokio::test]
    async fn wait_until_left_starting_returns_immediately_if_already_left() {
        let mut inst = Instance::new("already-running", "/tmp/wait-a");
        inst.id = "wait-already-left".to_string();
        inst.status = Status::Running;
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        let started = std::time::Instant::now();
        let result = wait_until_left_starting(
            &state,
            "wait-already-left",
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(result.map(|i| i.status), Some(Status::Running));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "must not wait when the instance already left Starting"
        );
    }

    #[tokio::test]
    async fn wait_until_left_starting_resolves_on_broadcast() {
        let mut inst = Instance::new("starting", "/tmp/wait-b");
        inst.id = "wait-resolves".to_string();
        inst.status = Status::Starting;
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        let updater_state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            {
                let mut instances = updater_state.instances.write().await;
                if let Some(inst) = instances.iter_mut().find(|i| i.id == "wait-resolves") {
                    inst.status = Status::Waiting;
                }
            }
            let _ = updater_state
                .status_tx
                .send(crate::server::push::StatusChange {
                    instance_id: "wait-resolves".to_string(),
                    instance_title: "starting".to_string(),
                    old: Status::Starting,
                    new: Status::Waiting,
                    at: chrono::Utc::now(),
                });
        });

        let started = std::time::Instant::now();
        let result =
            wait_until_left_starting(&state, "wait-resolves", std::time::Duration::from_secs(5))
                .await;
        assert_eq!(result.map(|i| i.status), Some(Status::Waiting));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must resolve promptly off the broadcast, not sit out the full timeout"
        );
    }

    #[tokio::test]
    async fn wait_until_left_starting_times_out_with_current_status() {
        let mut inst = Instance::new("stuck", "/tmp/wait-c");
        inst.id = "wait-timeout".to_string();
        inst.status = Status::Starting;
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        let result = wait_until_left_starting(
            &state,
            "wait-timeout",
            std::time::Duration::from_millis(150),
        )
        .await;
        assert_eq!(
            result.map(|i| i.status),
            Some(Status::Starting),
            "timeout must still return the freshest known status, not lie about readiness"
        );
    }

    #[tokio::test]
    async fn wait_until_left_starting_returns_none_if_instance_vanished() {
        let state = crate::server::test_support::build_test_app_state(vec![]);
        let result = wait_until_left_starting(
            &state,
            "never-existed",
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(result.is_none());
    }

    #[test]
    fn find_by_idempotency_key_matches_trashed_but_not_missing() {
        let mut with_key = Instance::new("has-key", "/tmp/idem-a");
        with_key.id = "idem-has-key".to_string();
        with_key.idempotency_key = Some("retry-token-1".to_string());
        with_key.trash(); // soft-deleted; a retry must still find it.

        let mut without_key = Instance::new("no-key", "/tmp/idem-b");
        without_key.id = "idem-no-key".to_string();

        let instances = vec![with_key, without_key];

        let found = find_by_idempotency_key(&instances, "retry-token-1");
        assert_eq!(found.map(|i| i.id.as_str()), Some("idem-has-key"));

        assert!(find_by_idempotency_key(&instances, "never-seen").is_none());
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
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
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
    fn session_response_dormant_reflects_shown_dormant() {
        let mut inst = make_test_instance();

        // Live idle: not dormant.
        inst.status = Status::Idle;
        assert!(!SessionResponse::from_instance(&inst, false).dormant);

        // Idle-reaped (marker set, status left Idle): dormant.
        inst.mark_idle_dormant();
        assert!(SessionResponse::from_instance(&inst, false).dormant);

        // Deliberate stop (marker set AND Stopped): reports NOT dormant so the
        // dashboard keeps the neutral Stopped dot. See #2250.
        inst.status = Status::Stopped;
        assert!(!SessionResponse::from_instance(&inst, false).dormant);
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

    /// Each workspace member carries its own override and recorded base, and
    /// the session-level `base_branch_override` does not leak into any of
    /// them. That leak is what made a multi-repo diff compare every repo
    /// against one ref. See #3329.
    #[test]
    fn diff_repos_of_scopes_bases_per_workspace_repo() {
        fn repo(
            name: &str,
            base: Option<&str>,
            over: Option<&str>,
        ) -> crate::session::WorkspaceRepo {
            crate::session::WorkspaceRepo {
                name: name.to_string(),
                source_path: format!("/src/{name}"),
                branch: "feature/x".to_string(),
                worktree_path: format!("/ws/{name}"),
                main_repo_path: format!("/src/{name}"),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: base.map(str::to_string),
                base_branch_override: over.map(str::to_string),
            }
        }

        let mut inst = make_test_instance();
        inst.base_branch_override = Some("session-wide".to_string());
        inst.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: "/ws".to_string(),
            repos: vec![
                repo("api", Some("develop"), None),
                repo("web", Some("develop"), Some("epic/checkout")),
                repo("infra", None, None),
            ],
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });

        let repos = diff_repos_of(&inst);
        let seen: Vec<_> = repos
            .iter()
            .map(|r| {
                (
                    r.name.as_deref(),
                    r.base_override.as_deref(),
                    r.recorded_base.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (Some("api"), None, Some("develop")),
                (Some("web"), Some("epic/checkout"), Some("develop")),
                (Some("infra"), None, None),
            ],
            "workspace members must not inherit the session-level override"
        );

        // A single-repo session is the other shape: one unnamed entry whose
        // override IS the session-level field.
        let mut single = make_test_instance();
        single.base_branch_override = Some("upstream/main".to_string());
        single.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/x".to_string(),
            main_repo_path: "/src/only".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: Some("develop".to_string()),
        });
        let repos = diff_repos_of(&single);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, None);
        assert_eq!(repos[0].base_override.as_deref(), Some("upstream/main"));
        assert_eq!(repos[0].recorded_base.as_deref(), Some("develop"));
    }

    /// The PATCH write lands on exactly the named repo, and the unnamed
    /// target still writes the session field. See #3329.
    #[test]
    fn apply_diff_base_override_writes_only_the_named_repo() {
        let mut inst = make_test_instance();
        inst.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feature/x".to_string(),
            workspace_dir: "/ws".to_string(),
            repos: ["api", "web"]
                .iter()
                .map(|n| crate::session::WorkspaceRepo {
                    name: n.to_string(),
                    source_path: format!("/src/{n}"),
                    branch: "feature/x".to_string(),
                    worktree_path: format!("/ws/{n}"),
                    main_repo_path: format!("/src/{n}"),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                })
                .collect(),
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });

        apply_diff_base_override(&mut inst, Some("web"), Some("epic/checkout".to_string()));
        let overrides: Vec<_> = inst
            .all_repos()
            .iter()
            .map(|r| (r.name.as_str(), r.base_branch_override.as_deref()))
            .collect();
        assert_eq!(
            overrides,
            vec![("api", None), ("web", Some("epic/checkout"))]
        );
        assert_eq!(
            inst.base_branch_override, None,
            "a per-repo write must not touch the session field"
        );

        // Clearing one repo leaves its sibling alone.
        apply_diff_base_override(&mut inst, Some("web"), None);
        assert_eq!(inst.all_repos()[1].base_branch_override, None);

        // The unnamed target is the session's own checkout.
        apply_diff_base_override(&mut inst, None, Some("develop".to_string()));
        assert_eq!(inst.base_branch_override.as_deref(), Some("develop"));
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
    fn title_only_rename_cache_patch_preserves_newer_path_and_branch() {
        let mut cached = make_test_instance();
        cached.title = "Old title".to_string();
        cached.project_path = "/tmp/worktrees/concurrent".to_string();
        cached.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "concurrent-branch".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        apply_session_rename_cache_patch(
            &mut cached,
            SessionRenameCachePatch {
                title: "New title",
                initial_path: "/tmp/worktrees/initial",
                initial_branch: Some("initial-branch"),
                authoritative_path: "/tmp/worktrees/earlier-snapshot",
                authoritative_branch: Some("earlier-snapshot-branch"),
                renamed_path: None,
                renamed_branch: None,
            },
        );

        assert_eq!(cached.title, "New title");
        assert_eq!(cached.project_path, "/tmp/worktrees/concurrent");
        assert_eq!(
            cached
                .worktree_info
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some("concurrent-branch")
        );
        let response = SessionResponse::from_instance(&cached, false);
        assert_eq!(response.title, "New title");
    }

    #[test]
    fn tied_rename_cache_patch_publishes_owned_path_and_branch() {
        let mut cached = make_test_instance();
        cached.project_path = "/tmp/worktrees/concurrent".to_string();
        cached.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "concurrent-branch".to_string(),
            main_repo_path: "/tmp/repo".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        apply_session_rename_cache_patch(
            &mut cached,
            SessionRenameCachePatch {
                title: "New title",
                initial_path: "/tmp/worktrees/initial",
                initial_branch: Some("initial-branch"),
                authoritative_path: "/tmp/worktrees/renamed",
                authoritative_branch: Some("renamed-branch"),
                renamed_path: Some("/tmp/worktrees/renamed"),
                renamed_branch: Some("renamed-branch"),
            },
        );

        assert_eq!(cached.title, "New title");
        assert_eq!(cached.project_path, "/tmp/worktrees/renamed");
        assert_eq!(
            cached
                .worktree_info
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some("renamed-branch")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rename_session_distinguishes_cwd_stable_title_and_branch_changes() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let paths = tempfile::tempdir().unwrap();
        let title_path = paths.path().join("my-session");
        let branch_path = paths.path().join("branch-only");
        let title_id = "rename-title-only".to_string();
        let branch_id = "rename-branch-only".to_string();

        let mut title_only = Instance::new(
            "Original title",
            title_path.to_str().expect("UTF-8 temp path"),
        );
        title_only.id = title_id.clone();
        title_only.status = Status::Running;
        title_only.view = crate::session::View::Structured;
        title_only.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "my-session".to_string(),
            main_repo_path: paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        let mut branch_only = Instance::new(
            "Branch Only",
            branch_path.to_str().expect("UTF-8 temp path"),
        );
        branch_only.id = branch_id.clone();
        branch_only.status = Status::Running;
        branch_only.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "existing-branch".to_string(),
            main_repo_path: paths
                .path()
                .join("missing-repo")
                .to_string_lossy()
                .into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });

        let (_storage, state) = build_rename_test_state(
            vec![title_only.clone(), branch_only.clone()],
            vec![title_only, branch_only],
        );
        state.acp_supervisor.test_insert_worker(&title_id).await;

        // The title changes, but its slug already matches both the cwd leaf
        // and branch. Even with the branch toggle armed, this is title-only.
        let title_response = rename_session(
            State(state.clone()),
            Path(title_id.clone()),
            Ok(Json(RenameSessionBody {
                title: "My Session!".to_string(),
                rename_branch: true,
            })),
        )
        .await
        .into_response();
        assert_eq!(title_response.status(), StatusCode::OK);
        let title_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(title_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(title_json["tie_workdir_to_name"], true);
        assert!(
            state.acp_supervisor.is_running(&title_id).await,
            "a cwd-stable title-only rename must not stop the structured worker"
        );

        {
            let instances = state.instances.read().await;
            let renamed = instances.iter().find(|inst| inst.id == title_id).unwrap();
            assert_eq!(renamed.title, "My Session!");
            assert_eq!(renamed.project_path, title_path.to_str().unwrap());
            assert_eq!(
                renamed.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
                Some("my-session")
            );
        }

        let branch_response = rename_session(
            State(state.clone()),
            Path(branch_id.clone()),
            Ok(Json(RenameSessionBody {
                title: "Branch Only".to_string(),
                rename_branch: true,
            })),
        )
        .await
        .into_response();
        assert_eq!(branch_response.status(), StatusCode::CONFLICT);
        let branch_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(branch_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(branch_json["error"], "session_running");

        let instances = state.instances.read().await;
        let rejected = instances.iter().find(|inst| inst.id == branch_id).unwrap();
        assert_eq!(rejected.title, "Branch Only");
        assert_eq!(rejected.project_path, branch_path.to_str().unwrap());
        assert_eq!(
            rejected.worktree_info.as_ref().map(|wt| wt.branch.as_str()),
            Some("existing-branch"),
            "the active branch-only request must be rejected before git mutation"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rename_session_quiesces_structured_worker_only_when_its_cwd_moves() {
        // Invariant #2260: a live structured-view worker is pinned to its cwd,
        // so a tied rename that MOVES the worktree directory must stop the
        // worker first (else it crash-loops at the pulled-out path), while a
        // rename that leaves the cwd in place must NOT interrupt it. The
        // quiesce runs before the git edit, so the cwd-moving assertion holds
        // even though the edit itself then fails on a fixture with no real
        // worktree to move: what #2260 pins is that the worker is gone by then.
        let _app_dir = crate::session::test_support::isolate_app_dir();

        struct Case {
            id: &'static str,
            leaf: &'static str,
            new_title: &'static str,
            // Whether the new title's slug relocates the worktree directory.
            moves_cwd: bool,
        }
        // The cwd-stable row's slug ("my-session") equals the existing leaf, so
        // the edit is a no-op move; the cwd-moving row's slug differs, forcing
        // a relocation.
        let cases = [
            Case {
                id: "quiesce-cwd-stable",
                leaf: "my-session",
                new_title: "My Session!",
                moves_cwd: false,
            },
            Case {
                id: "quiesce-cwd-moving",
                leaf: "old-leaf",
                new_title: "A Brand New Name",
                moves_cwd: true,
            },
        ];

        for case in cases {
            let paths = tempfile::tempdir().unwrap();
            let project_path = paths.path().join(case.leaf);
            let mut inst = Instance::new(
                "Original title",
                project_path.to_str().expect("UTF-8 temp path"),
            );
            inst.id = case.id.to_string();
            // Idle, not Running: a structured session the user "stopped" sits
            // at Idle yet still owns a live worker, which is exactly the gap
            // `blocks_worktree_edit` misses and quiesce closes.
            inst.status = Status::Idle;
            inst.view = crate::session::View::Structured;
            inst.worktree_info = Some(crate::session::WorktreeInfo {
                branch: case.leaf.to_string(),
                main_repo_path: paths
                    .path()
                    .join("missing-repo")
                    .to_string_lossy()
                    .into_owned(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let (_storage, state) = build_rename_test_state(vec![inst.clone()], vec![inst]);
            state.acp_supervisor.test_insert_worker(case.id).await;

            let _ = rename_session(
                State(state.clone()),
                Path(case.id.to_string()),
                Ok(Json(RenameSessionBody {
                    title: case.new_title.to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response();

            assert_eq!(
                state.acp_supervisor.is_running(case.id).await,
                !case.moves_cwd,
                "{}: worker should be {} for moves_cwd={}",
                case.id,
                if case.moves_cwd {
                    "stopped"
                } else {
                    "preserved"
                },
                case.moves_cwd
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn set_worktree_name_quiesces_structured_worker_only_when_its_cwd_moves() {
        // The standalone-endpoint mirror of the rename_session gate above: both
        // stop a live structured-view worker only when the edit actually moves
        // the worktree cwd (#2260), never for a cwd-stable or branch-only edit.
        // The quiesce precedes the git edit, so the cwd-moving assertion holds
        // even though the edit itself then fails on a fixture with no real
        // worktree to move: what #2260 pins is that the worker is gone by then.
        let _app_dir = crate::session::test_support::isolate_app_dir();
        // set_worktree_name refuses a tied managed worktree (tied callers must
        // go through rename_session), so untie the profile to reach the worker
        // gate that this test exercises.
        let mut overrides = serde_json::Map::new();
        overrides.insert(
            "session".to_string(),
            serde_json::json!({ "tie_workdir_to_name": false }),
        );
        crate::session::profile_config::save_profile_config(
            "test",
            &crate::session::profile_config::ProfileConfig {
                description: None,
                overrides,
            },
        )
        .expect("write test profile override");

        struct Case {
            id: &'static str,
            leaf: &'static str,
            new_name: &'static str,
            // Whether the requested name relocates the worktree directory.
            moves_cwd: bool,
        }
        // The cwd-stable row's name equals the existing leaf (a no-op move); the
        // cwd-moving row's name differs, forcing a relocation.
        let cases = [
            Case {
                id: "sw-cwd-stable",
                leaf: "my-session",
                new_name: "my-session",
                moves_cwd: false,
            },
            Case {
                id: "sw-cwd-moving",
                leaf: "old-leaf",
                new_name: "new-leaf",
                moves_cwd: true,
            },
        ];

        for case in cases {
            let paths = tempfile::tempdir().unwrap();
            let project_path = paths.path().join(case.leaf);
            let mut inst = Instance::new(
                "Original title",
                project_path.to_str().expect("UTF-8 temp path"),
            );
            inst.id = case.id.to_string();
            inst.source_profile = "test".to_string();
            inst.status = Status::Idle;
            inst.view = crate::session::View::Structured;
            inst.worktree_info = Some(crate::session::WorktreeInfo {
                branch: case.leaf.to_string(),
                main_repo_path: paths
                    .path()
                    .join("missing-repo")
                    .to_string_lossy()
                    .into_owned(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let storage = Storage::new_unwatched("test").unwrap();
            storage
                .update(|instances, _groups| {
                    *instances = vec![inst.clone()];
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            state.acp_supervisor.test_insert_worker(case.id).await;

            let _ = set_worktree_name(
                State(state.clone()),
                Path(case.id.to_string()),
                Ok(Json(SetWorktreeNameBody {
                    name: case.new_name.to_string(),
                    rename_branch: false,
                })),
            )
            .await
            .into_response();

            assert_eq!(
                state.acp_supervisor.is_running(case.id).await,
                !case.moves_cwd,
                "{}: worker should be {} for moves_cwd={}",
                case.id,
                if case.moves_cwd {
                    "stopped"
                } else {
                    "preserved"
                },
                case.moves_cwd
            );
        }
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
        started.omp_capture_generation = Some("omp-generation-restart".to_string());
        let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
        assert!(poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,));
        let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
        started.session_id_poller = Some(restarted_poller.clone());
        started.last_start_time = Some(std::time::Instant::now());

        apply_post_restart_sync(&mut live, &before, &started);

        assert_eq!(live.status, Status::Starting);
        assert!(live.last_error.is_none());
        assert_eq!(
            live.agent_session_id.as_deref(),
            Some("claude-uuid-restart")
        );
        assert_eq!(
            live.omp_capture_generation.as_deref(),
            Some("omp-generation-restart")
        );
        assert!(live.session_id_poller.is_some());
        assert_eq!(live.last_start_time, started.last_start_time);

        let mut generation_converged = before.clone();
        generation_converged.agent_session_id = Some("peer-sid".to_string());
        generation_converged.omp_capture_generation = Some("omp-generation-restart".to_string());
        apply_post_restart_identity_sync(&mut generation_converged, &before, &started);
        assert_eq!(
            generation_converged.agent_session_id.as_deref(),
            Some("peer-sid")
        );
        assert!(generation_converged.session_id_poller.is_some());

        let mut peer_relaunched = before.clone();
        peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
        apply_post_restart_identity_sync(&mut peer_relaunched, &before, &started);
        assert_eq!(
            peer_relaunched.omp_capture_generation.as_deref(),
            Some("peer-generation")
        );
        assert!(std::sync::Arc::ptr_eq(
            peer_relaunched
                .session_id_poller
                .as_ref()
                .expect("running restart poller"),
            &restarted_poller,
        ));
        restarted_poller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stop();
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
    fn restart_sync_rejects_an_older_lifecycle_generation() {
        let mut before = make_test_instance();
        before.lifecycle_generation = 4;

        let mut started = before.clone();
        started.status = Status::Error;
        started.agent_session_id = Some("stale-restart-sid".to_string());
        started.retroactive_capture_excludes = ["stale-exclusion".to_string()].into();

        let mut live = before.clone();
        live.lifecycle_generation = 5;
        live.status = Status::Running;
        live.agent_session_id = Some("newer-restart-sid".to_string());
        live.retroactive_capture_excludes = ["newer-exclusion".to_string()].into();

        assert!(!apply_post_restart_sync(&mut live, &before, &started));
        apply_cascade_state_sync(&mut live, &before, &started);

        assert_eq!(live.lifecycle_generation, 5);
        assert_eq!(live.status, Status::Running);
        assert_eq!(live.agent_session_id.as_deref(), Some("newer-restart-sid"));
        assert_eq!(
            live.retroactive_capture_excludes,
            ["newer-exclusion".to_string()].into()
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
    fn session_tool_identity_uses_repo_aware_config_but_not_repo_custom_agents() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                my-agent = "ssh -t lenovo claude"
            "#,
        )
        .unwrap();

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

        // The user's own custom agent resolves through the repo-aware path.
        assert!(validate_session_tool_identity(
            "my-agent",
            "default",
            project.path()
        ));
        // A repo-defined one does not exist as far as AoE is concerned (#3154).
        assert!(!validate_session_tool_identity(
            "repo-agent",
            "default",
            project.path()
        ));
    }

    /// Build one structured, idle session with an empty `source_profile`, so
    /// `purge_session_artifacts` refuses on its first line. The teardown that
    /// follows is what a delete must not start under an in-flight submission;
    /// these tests only need to observe that it waits for one.
    fn delete_race_state(id: &str) -> std::sync::Arc<crate::server::AppState> {
        delete_race_state_for(&[id])
    }

    /// [`delete_race_state`] for a workspace: every id shares the shape, so a
    /// sibling teardown can be observed the same way the owner's is.
    fn delete_race_state_for(ids: &[&str]) -> std::sync::Arc<crate::server::AppState> {
        let instances = ids
            .iter()
            .map(|id| {
                let mut inst = Instance::new("delete-3650", "/tmp/aoe-3650-delete");
                inst.id = (*id).to_string();
                inst.view = crate::session::View::Structured;
                inst.status = Status::Idle;
                inst
            })
            .collect();
        crate::server::test_support::build_test_app_state(instances)
    }

    /// #3650: prompt submission moved off `instance_lock`, so a permanent
    /// delete that takes only `instance_lock` no longer excludes a queue drain
    /// that snapshotted an idle turn and is on its way to `send_turn`. The
    /// delete would then stop the worker, purge the transcript and remove the
    /// worktree under a delivery already in flight.
    ///
    /// Each permanent-delete path is checked the same way: hold the session's
    /// submission guard (standing in for that drain) and assert the delete
    /// parks before any teardown, then completes once the guard drops.
    #[tokio::test]
    async fn permanent_deletion_waits_for_an_in_flight_submission() {
        use std::time::Duration;

        // Direct delete.
        let state = delete_race_state("sess-3650-direct");
        let delivering = state
            .session_service
            .prompt_submission("sess-3650-direct")
            .await;
        let delete = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                delete_session(
                    State(state),
                    Path("sess-3650-direct".to_string()),
                    Some(Json(DeleteSessionBody::default())),
                )
                .await
                .into_response()
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !delete.is_finished(),
            "a delete must not tear a session down under an in-flight submission"
        );
        assert_eq!(
            state.instances.read().await[0].status,
            Status::Idle,
            "the session must not even be marked Deleting yet"
        );
        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), delete)
            .await
            .expect("the delete lands once the submission releases the session")
            .expect("delete task must not panic");

        // Workspace teardown, on the owner's own guard.
        let state = delete_race_state("sess-3650-owner");
        let delivering = state
            .session_service
            .prompt_submission("sess-3650-owner")
            .await;
        let workspace = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                purge_workspace_artifacts(
                    &state,
                    "sess-3650-owner".to_string(),
                    vec![("sess-3650-owner".to_string(), DeleteSessionBody::default())],
                    false,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !workspace.is_finished(),
            "a workspace teardown must wait for the owner's in-flight submission"
        );
        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), workspace)
            .await
            .expect("the workspace teardown lands once the submission releases")
            .expect("workspace task must not panic");

        // Workspace teardown, on a sibling's guard. The owner's is free, so
        // the plan loop reaches the sibling and must park there: the owner is
        // ordered last, and a sibling torn down under a live delivery is the
        // case #3650 names alongside the direct delete.
        let state = delete_race_state_for(&["sess-3650-sib", "sess-3650-ws-owner"]);
        let delivering = state
            .session_service
            .prompt_submission("sess-3650-sib")
            .await;
        let workspace = tokio::spawn({
            let state = std::sync::Arc::clone(&state);
            async move {
                purge_workspace_artifacts(
                    &state,
                    "sess-3650-ws-owner".to_string(),
                    vec![
                        ("sess-3650-sib".to_string(), DeleteSessionBody::default()),
                        (
                            "sess-3650-ws-owner".to_string(),
                            DeleteSessionBody::default(),
                        ),
                    ],
                    false,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !workspace.is_finished(),
            "a workspace teardown must wait for a sibling's in-flight submission"
        );
        assert!(
            state
                .instances
                .read()
                .await
                .iter()
                .all(|i| i.status == Status::Idle),
            "no row may be marked Deleting while the sibling's submission is in flight"
        );
        drop(delivering);
        tokio::time::timeout(Duration::from_secs(10), workspace)
            .await
            .expect("the workspace teardown lands once the sibling submission releases")
            .expect("workspace task must not panic");
    }

    /// The retention purge's copy of the same barrier. It resolves profile
    /// config, which reads the user's global config, before it reaches the
    /// guard, so the race above cannot cover it without reading user state.
    /// The lock order is asserted in the source instead.
    #[test]
    fn the_retention_purge_takes_submission_before_the_instance_lock() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/api/sessions.rs"),
        )
        .unwrap();
        let start = source
            .find("pub(crate) async fn purge_expired_trash")
            .unwrap();
        let body = &source[start..];
        let submission = body.find("prompt_submission_for_session(&id)").unwrap();
        let inst_lock = body.find("state.instance_lock(&id).await").unwrap();
        let purge = body.find("purge_session_artifacts(").unwrap();
        assert!(
            submission < inst_lock,
            "submission authority is taken first"
        );
        assert!(inst_lock < purge, "both are held across the teardown");
    }

    /// #3650's barrier applies to every handler that stops a worker, not just
    /// the ones that delete a session. `drain_queued_prompts_once` reads the
    /// status and the trashed/archived/snoozed flags once under the submission
    /// guard and only then reaches `send_turn`, which respawns a worker it
    /// finds gone. So a stop that lands inside that window is undone: the user
    /// presses Stop and the session comes back running the queued prompt.
    ///
    /// Before #3639 the drain held `instance_lock` across delivery and these
    /// four handlers were excluded by it. They take the submission guard now
    /// for the same reason `attach_project` and the tied renames do.
    #[tokio::test]
    async fn worker_stopping_handlers_wait_for_an_in_flight_submission() {
        use std::time::Duration;

        async fn call(
            which: &str,
            state: std::sync::Arc<crate::server::AppState>,
            id: String,
        ) -> axum::response::Response {
            match which {
                "stop" => stop_session(State(state), Path(id)).await.into_response(),
                "trash" => trash_session(State(state), Path(id), None)
                    .await
                    .into_response(),
                "archive" => update_session_archive(
                    State(state),
                    Path(id),
                    Ok(Json(UpdateArchiveBody {
                        archived: true,
                        kill_pane: true,
                    })),
                )
                .await
                .into_response(),
                "snooze" => update_session_snooze(
                    State(state),
                    Path(id),
                    Ok(Json(UpdateSnoozeBody { minutes: Some(30) })),
                )
                .await
                .into_response(),
                other => unreachable!("unknown handler {other}"),
            }
        }

        for which in ["stop", "trash", "archive", "snooze"] {
            let id = format!("sess-3650-{which}");
            let state = delete_race_state(&id);
            let delivering = state.session_service.prompt_submission(&id).await;
            let handler = tokio::spawn({
                let state = std::sync::Arc::clone(&state);
                let id = id.clone();
                async move { call(which, state, id).await }
            });

            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                !handler.is_finished(),
                "{which} must not quiesce a worker a submission is mid-delivery on"
            );
            assert_eq!(
                state.instances.read().await[0].status,
                Status::Idle,
                "{which} must not have touched the session yet"
            );

            drop(delivering);
            tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"))
                .unwrap_or_else(|e| panic!("{which} task must not panic: {e}"));
        }
    }

    /// #3651: `prompt_submission` auto-vivifies a registry entry for whatever
    /// id it is handed and nothing prunes it, so every externally reachable
    /// mutation that claims it must prove the session exists first. Otherwise
    /// an authenticated client grows daemon memory with random ids.
    #[tokio::test]
    async fn session_mutations_allocate_no_prompt_lock_for_an_unknown_id() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let service = std::sync::Arc::clone(&state.session_service);

        for i in 0..3 {
            let id = format!("sess-gone-{i}");
            assert_eq!(
                rename_session(
                    State(std::sync::Arc::clone(&state)),
                    Path(id.clone()),
                    Ok(Json(RenameSessionBody {
                        title: "new title".to_string(),
                        rename_branch: false,
                    })),
                )
                .await
                .into_response()
                .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                set_worktree_name(
                    State(std::sync::Arc::clone(&state)),
                    Path(id.clone()),
                    Ok(Json(SetWorktreeNameBody {
                        name: "new-dir".to_string(),
                        rename_branch: false,
                    })),
                )
                .await
                .into_response()
                .status(),
                StatusCode::NOT_FOUND
            );
            assert!(matches!(
                crate::server::attach_project::attach_project(
                    &state,
                    &id,
                    std::path::Path::new("/tmp"),
                    crate::session::attach_project::ExistingBranch::Refuse,
                )
                .await,
                Err(crate::server::attach_project::AttachError::NotFound)
            ));
            assert!(matches!(
                service
                    .edit_queued_prompt(&id, "q1".to_string(), "text".to_string())
                    .await,
                crate::server::session_service::EditQueuedOutcome::NotFound
            ));
            assert!(!service.remove_queued_prompt(&id, "q1".to_string()).await);
            service.clear_queued_prompts(&id).await;
        }

        assert_eq!(
            service.prompt_locks_len().await,
            0,
            "an id that was never admitted must not leave a lock-registry entry behind"
        );
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

    #[test]
    #[serial_test::serial]
    fn rename_persistence_reports_missing_authoritative_row() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let _ = isolated_app_dir(temp_home.path());
        let storage = Storage::new_unwatched("rename-missing").unwrap();

        let outcome =
            persist_rename_metadata(&storage, "missing-id", "New title", None, None).unwrap();
        assert_eq!(outcome, RenamePersistOutcome::Missing);
        assert!(
            storage.load().unwrap().is_empty(),
            "a missing row must not be synthesized by rename persistence"
        );
    }

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
    // Terminal keystroke injection: CityHall sessions are structured-view only
    // (the composer drives the agent via the ACP prompt route), so close this
    // explicitly rather than leaning on the downstream StructuredView error.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
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
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            i.status = crate::session::Status::Error;
                            i.last_error = Some(msg);
                        }
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
    // Raw terminal pane content: CityHall hides the terminal UI + WS relay, so
    // this read must be closed too or the pane is reachable by session id.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
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
