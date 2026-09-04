//! The `SessionResponse` wire model and per-request config resolution.

use super::*;

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
    pub context_resume: ContextResumeAvailability,
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
            context_resume: context_resume_for(inst),
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
pub(super) fn plan_summary_from_plan(plan: crate::acp::state::Plan) -> PlanSummary {
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

pub(super) fn truncate_title(s: &str, max: usize) -> String {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextResumeUnavailableReason {
    AgentUnsupported,
    SandboxUnsupported,
    CommandUnsupported,
    ForcedFresh,
    InvalidTarget,
    ForkPending,
    PreviousFailure,
    NoTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextResumeIndeterminateReason {
    RuntimeCheckRequired,
    AgentHandshakeRequired,
}

/// Whether the daemon can preserve agent context during a future authorized
/// lifecycle transition. This is not current start eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContextResumeAvailability {
    Available,
    Indeterminate {
        reason: ContextResumeIndeterminateReason,
    },
    Unavailable {
        reason: ContextResumeUnavailableReason,
    },
}

pub(super) fn context_resume_for(inst: &Instance) -> ContextResumeAvailability {
    if inst.is_structured() {
        return if inst.fork_pending.is_some() {
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::ForkPending,
            }
        } else if inst.acp_session_id.is_some() {
            ContextResumeAvailability::Indeterminate {
                reason: ContextResumeIndeterminateReason::AgentHandshakeRequired,
            }
        } else {
            ContextResumeAvailability::Unavailable {
                reason: ContextResumeUnavailableReason::NoTarget,
            }
        };
    }

    match inst.terminal_context_resume_cached() {
        TerminalContextResume::Available => ContextResumeAvailability::Available,
        TerminalContextResume::RuntimeCheckRequired => ContextResumeAvailability::Indeterminate {
            reason: ContextResumeIndeterminateReason::RuntimeCheckRequired,
        },
        TerminalContextResume::NoTarget => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::NoTarget,
        },
        TerminalContextResume::AgentUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::AgentUnsupported,
        },
        TerminalContextResume::SandboxUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::SandboxUnsupported,
        },
        TerminalContextResume::CommandUnsupported => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::CommandUnsupported,
        },
        TerminalContextResume::ForcedFresh => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::ForcedFresh,
        },
        TerminalContextResume::InvalidTarget => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::InvalidTarget,
        },
        TerminalContextResume::ForkPending => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::ForkPending,
        },
        TerminalContextResume::PreviousFailure => ContextResumeAvailability::Unavailable {
            reason: ContextResumeUnavailableReason::PreviousFailure,
        },
    }
}

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
pub(super) fn custom_agent_acp_capable(
    session: &crate::session::config::SessionConfig,
    tool: &str,
) -> bool {
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
pub(super) fn resolve_session_cfg<'a>(
    cache: &'a mut HashMap<(String, String), SessionConfig>,
    profile: &str,
    project_path: &str,
) -> &'a SessionConfig {
    cache
        .entry((profile.to_string(), project_path.to_string()))
        .or_insert_with(|| {
            #[cfg(test)]
            LIST_SESSIONS_RESOLVER_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(
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
