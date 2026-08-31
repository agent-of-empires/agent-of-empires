//! Shared wire types for the daemon REST API.

use serde::{Deserialize, Serialize};

use crate::session::SessionScope;
/// Which ACP `ContentBlock` an attachment maps to. The string form
/// (`"image"` / `"audio"` / `"resource"`) is the wire contract shared
/// with the web composer and the prompt-request DTO in `protocol.rs`,
/// so renaming a variant breaks the build on both sides rather than
/// silently dropping attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptAttachmentKind {
    Image,
    Audio,
    Resource,
}

impl PromptAttachmentKind {
    /// Stable lowercase tag, matching the serde wire form. Used by the
    /// attachment store to persist the kind as a TEXT column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Resource => "resource",
        }
    }

    /// Parse the lowercase tag written by [`Self::as_str`], for reading the kind
    /// back out of the attachment store's TEXT column. `None` on an unknown
    /// tag (a corrupt or forward-version row), so the caller can skip it.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "resource" => Some(Self::Resource),
            _ => None,
        }
    }
}

/// Replay-side view of one prompt attachment. Carries metadata only,
/// never the bytes: the decoded blob lives in the `acp_attachments`
/// table keyed by `(session_id, id)` and is fetched lazily over
/// `GET /acp/attachments/{id}`. Keeping bytes out of the event log
/// is what stops `event_json` (and every WS replay frame) from bloating
/// to megabytes per screenshot. See #1000 / #965.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptAttachmentRef {
    pub id: String,
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Decoded byte length, for the UI to show a size hint without
    /// fetching the blob.
    pub size: u64,
}

/// One entry in a session's server-owned prompt queue: a follow-up the
/// user lined up while a turn was busy. The daemon is the source of truth
/// (persisted on the `Instance`), so the queue survives a client reload or
/// a closed PWA and drains on turn-end with no tab open.
///
/// Attachments carry metadata only, exactly like [`PromptAttachmentRef`]
/// on a live prompt: the bytes live in the event store's pending-attachment
/// table keyed by `(session_id, prompt_id, attachment_id)` (outside the
/// seq-keyed retention prune, since a queued prompt has no event seq yet) and
/// are reloaded at drain time, so a queued screenshot does not bloat the
/// session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedPromptEntry {
    /// Client-minted stable id, unchanged across edits. Doubles as the
    /// optimistic-echo reconcile key on the client.
    pub id: String,
    /// Server-assigned monotonic order; the queue drains by ascending `seq`.
    pub seq: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptAttachmentRef>,
    /// RFC3339 enqueue time, for retention and provenance.
    pub created_at: String,
    /// Which device enqueued it, for multi-device provenance. `None` for
    /// rows migrated from a pre-server-queue client localStorage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_device: Option<String>,
}

/// Public lifecycle state for a structured view worker, surfaced via
/// `SessionResponse.acp_worker_state` so the sidebar + structured view
/// can show a "Resuming…" affordance while the reconciler is mid-spawn
/// or mid-attach. Deliberately not persisted to the structured view event log:
/// daemon lifecycle is ephemeral, transcript replay should not carry
/// it. See #1088.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpWorkerState {
    /// No worker for this session and no resume in flight.
    Absent,
    /// A spawn or attach is in progress; the UI shows the "Resuming…"
    /// banner + sidebar chip.
    Resuming,
    /// Worker is online and reachable.
    Running,
}

#[derive(Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
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
    pub acp_worker_state: AcpWorkerState,
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
    pub queued_prompts: Vec<QueuedPromptEntry>,
    /// The session's captured ACP session id, present only once the
    /// structured-view worker has minted one. The web dashboard passes this
    /// as `fork_from` on a structured fork create, so the sidebar only offers
    /// "Fork" on a structured row that has a captured id to diverge from.
    /// Omitted when absent (terminal sessions, or structured ones whose worker
    /// has not minted an id yet).
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
    /// True when this session's agent can run a structured ACP `session/fork`:
    /// it is ACP-capable AND declares a real fork strategy. Resume-only ACP
    /// agents (e.g. `aoe-agent`, which advertises `loadSession` but not
    /// `session/fork`) are ACP-capable yet not forkable, so gating the web
    /// "Fork" action on `acp_session_id` alone would offer a dead-end button
    /// that fails at the `session/fork` handshake. The true capability is only
    /// advertised transiently during the handshake, so this projects the static
    /// agent fork strategy instead, which is the set AoE treats as forkable.
    /// Omitted (read as not-forkable) for terminal sessions and non-forkable
    /// agents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub monitor_active: bool,
    /// The `description` the agent gave the `Monitor` tool, shown as the
    /// badge tooltip. Only set when `monitor_active` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_description: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlanSummary {
    /// First non-completed step's title, truncated to ~80 chars so the
    /// sidebar row doesn't overflow.
    pub current_step_title: Option<String>,
    /// Count of `PlanEntryStatus::Done` steps.
    pub completed: u32,
    /// Total step count.
    pub total: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WorkspaceRepoSummary {
    pub name: String,
    pub source_path: String,
    pub branch: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CleanupDefaults {
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    /// Resolved `session.delete_to_trash`: when true, the web delete dialog
    /// defaults to "Move to Trash" with a permanent-delete disclosure;
    /// when false it goes straight to permanent delete. See #2489.
    pub delete_to_trash: bool,
}

// Envelope for `GET /api/sessions`. Wraps the sessions list with the
// user's persisted workspace ordering so the client can render the
// sidebar in the requested order on the first paint, with no extra
// round-trip. The order is a list of workspace ids; ids not present
// fall back to the client's default newest-first ordering. See #1169.
#[derive(Serialize, Deserialize)]
pub struct SessionsEnvelope {
    pub sessions: Vec<SessionResponse>,
    pub workspace_ordering: Vec<String>,
}

/// Query params for `GET /api/sessions`. `state` shares its vocabulary with
/// the CLI's `aoe list --state` via [`crate::session::SessionScope`] so a
/// future third caller cannot drift.
#[derive(Serialize, Deserialize)]
pub struct ListSessionsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SessionScope>,
}
