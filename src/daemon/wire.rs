//! Shared wire types for the daemon REST API.

use serde::{Deserialize, Serialize};

use crate::session::SessionScope;

/// One repository's branch base in a creation request.
#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
pub struct RepoBaseInput {
    pub repo: String,
    pub base_branch: String,
}

#[derive(Serialize, Deserialize)]
pub struct CreationTrustRequest {
    pub path: String,
    pub profile: Option<String>,
    #[serde(default)]
    pub scratch: bool,
}

/// Captured configuration, rechecked before approval or provisioning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
pub struct CreationTrustFingerprint {
    pub project_path: String,
    pub base_hooks_hash: String,
    pub hooks_hash: Option<String>,
    pub mcp_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreationTrustReview {
    pub fingerprint: CreationTrustFingerprint,
    pub merged_hooks: crate::session::HooksConfig,
    pub repo_hooks: crate::session::HooksConfig,
    pub mcp_summaries: Vec<String>,
    pub hooks_need_trust: bool,
    pub mcp_need_trust: bool,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
pub struct CreateSessionBody {
    #[cfg_attr(test, ts(optional = nullable))]
    pub title: Option<String>,
    #[cfg_attr(test, ts(optional = nullable))]
    pub size: Option<TerminalSize>,
    pub path: String,
    pub tool: String,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<String>", optional))]
    pub group: String,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub yolo_mode: bool,
    /// An explicit branch also opts into worktree creation.
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub worktree_enabled: bool,
    #[cfg_attr(test, ts(optional = nullable))]
    pub worktree_branch: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub create_new_branch: bool,
    /// Used only for new branches; empty selects the repository default.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub base_branch: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub sandbox: bool,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<String>", optional))]
    pub extra_args: String,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<Vec<String>>", optional))]
    pub extra_env: Vec<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<Vec<String>>", optional))]
    pub extra_repo_paths: Vec<String>,
    /// Repository names or paths override the shared base_branch.
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<Vec<RepoBaseInput>>", optional))]
    pub repo_bases: Vec<RepoBaseInput>,
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<String>", optional))]
    pub command_override: String,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub custom_instruction: Option<String>,
    #[cfg_attr(test, ts(optional = nullable))]
    pub profile: Option<String>,
    /// Structured view requires an ACP-capable agent.
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<crate::session::View>", optional))]
    pub view: crate::session::View,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub agent_name: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub agent_model: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub agent_effort: Option<String>,
    /// Provision a scratch directory instead of path; excludes worktrees and extra repos.
    #[serde(default)]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    pub scratch: bool,
    /// Omit to refuse unapproved hooks, false to skip untrusted hooks/MCP, true to approve.
    /// Skipping preserves already-trusted repository hooks and MCP.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub trust_hooks: Option<bool>,
    /// Require the reviewed configuration to remain unchanged before provisioning.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub trust_review: Option<CreationTrustFingerprint>,
    /// Resume a Claude conversation through ACP; path must be its original cwd.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub import_acp_session_id: Option<String>,
    /// Provider conversation ID to fork, mutually exclusive with other sources.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub fork_from: Option<String>,
    /// Canonical AoE row to fork without supplying its provider conversation ID.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub fork_session_id: Option<String>,
    /// Completion callback; private and loopback destinations are refused.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub callback_url: Option<String>,
    /// Persisted replay key, retained until the created row is hard-deleted.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub idempotency_key: Option<String>,
}

/// Persisted group identity; empty paths and synthetic sidebar sections are invalid.
#[derive(Serialize, Deserialize)]
pub struct GroupLocation {
    pub profile: String,
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct MoveGroupBody {
    pub source: GroupLocation,
    pub target: GroupLocation,
}
#[derive(Serialize, Deserialize)]
pub struct CollapseGroupBody {
    pub group: GroupLocation,
    pub collapsed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteGroupMode {
    EmptyOnly,
    KeepSessions,
}

#[derive(Serialize, Deserialize)]
pub struct DeleteGroupBody {
    pub group: GroupLocation,
    pub mode: DeleteGroupMode,
}

#[derive(Serialize, Deserialize)]
pub struct GroupSessionOutcome {
    pub id: String,
    #[serde(flatten)]
    pub outcome: PurgeOutcome,
}

#[derive(Serialize, Deserialize)]
pub struct DeleteGroupOutcome {
    pub sessions: Vec<GroupSessionOutcome>,
}

#[derive(Serialize, Deserialize)]
pub struct CreateProfileBody {
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct RenameProfileBody {
    pub new_name: String,
}

#[derive(Serialize, Deserialize)]
pub struct DefaultProfileBody {
    pub name: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct DeleteProfileQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_default: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct CreateProjectBody {
    /// Profile placement or cross-scope conflict-check context, even for global writes.
    pub profile: String,
    pub scope: crate::session::ProjectScope,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub allow_override: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(
        default,
        skip_serializing_if = "crate::session::ProjectOverrides::is_empty"
    )]
    pub overrides: crate::session::ProjectOverrides,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
pub struct TerminalSize {
    #[cfg_attr(test, ts(type = "number"))]
    pub cols: std::num::NonZeroU16,
    #[cfg_attr(test, ts(type = "number"))]
    pub rows: std::num::NonZeroU16,
}

#[derive(Default, Serialize, Deserialize)]
pub struct StartSessionBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TerminalSize>,
}

/// Explicit relaunch; omitted launch settings preserve the authoritative values.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartSessionBody {
    pub size: Option<TerminalSize>,
    pub profile: Option<String>,
    pub tool: Option<String>,
    pub command_override: Option<String>,
    pub extra_args: Option<String>,
    #[serde(default)]
    pub unsnooze: bool,
    #[serde(default)]
    pub skip_on_launch: bool,
    #[serde(default)]
    pub bound_hooks: bool,
    #[serde(default)]
    pub discard_sandbox_container: bool,
    pub wake_message: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RestartOutcome {
    pub lifecycle_generation: u64,
    pub profile: String,
    pub target: Option<TerminalTarget>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnsureToolBody {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TerminalSize>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalTargetStatus {
    Created,
    Exists,
    Alive,
    Restarted,
}

#[derive(Serialize, Deserialize)]
pub struct TerminalTarget {
    pub tmux_session: String,
    pub status: TerminalTargetStatus,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateArchiveBody {
    pub archived: bool,
    /// False preserves tmux panes, not structured workers. Ignored on unarchive.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

#[derive(Serialize, Deserialize)]
pub struct RenameSessionBody {
    pub title: String,
    /// When the session is tied (`session.tie_workdir_to_name`) and an
    /// aoe-managed worktree, also rename the underlying git branch to match
    /// the new title. Off by default; ignored for untied / non-worktree
    /// sessions. See #1927.
    #[serde(default)]
    pub rename_branch: bool,
}

#[derive(Serialize, Deserialize)]
pub struct TrashSessionBody {
    /// False preserves tmux panes, not structured workers.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrashOutcome {
    pub relocation: TrashRelocationOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TrashRelocationOutcome {
    Skipped,
    Relocated,
    Failed { reason: String },
}

fn default_kill_pane() -> bool {
    true
}

impl Default for TrashSessionBody {
    fn default() -> Self {
        Self {
            kill_pane: default_kill_pane(),
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct DeleteSessionBody {
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub delete_sandbox: bool,
    #[serde(default)]
    pub force_delete: bool,
    /// Retain the scratch directory while removing its session row.
    #[serde(default)]
    pub keep_scratch: bool,
}

#[derive(Serialize, Deserialize)]
pub struct AbandonPurgeBody {
    pub expected_generation: std::num::NonZeroU64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PurgeOutcome {
    Deleted {
        messages: Vec<String>,
        cleanup_errors: Vec<String>,
    },
    Kept {
        messages: Vec<String>,
        teardown_started: bool,
    },
}

#[derive(Serialize, Deserialize)]
pub struct UpdatePinBody {
    pub pinned: bool,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateFavoriteBody {
    pub favorited: bool,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateColorBody {
    /// A palette color, or null to clear the label.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateSnoozeBody {
    /// Positive minutes within the shared snooze bounds, or null to unsnooze.
    #[serde(default)]
    pub minutes: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateUnreadBody {
    pub unread: bool,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateGroupBody {
    /// Empty means ungrouped; nonempty paths create an implicit group.
    pub group: String,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateDiffBaseBody {
    /// Null or an empty branch restores the inherited diff base.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Required for a workspace; absent for a single-repo session.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Missing fields preserve overrides; null restores inherited defaults.
#[derive(Serialize, Deserialize, Default)]
pub struct UpdateNotificationsBody {
    #[serde(
        default,
        deserialize_with = "deserialize_tristate",
        skip_serializing_if = "Tristate::is_unset"
    )]
    pub notify_on_waiting: Tristate,
    #[serde(
        default,
        deserialize_with = "deserialize_tristate",
        skip_serializing_if = "Tristate::is_unset"
    )]
    pub notify_on_idle: Tristate,
    #[serde(
        default,
        deserialize_with = "deserialize_tristate",
        skip_serializing_if = "Tristate::is_unset"
    )]
    pub notify_on_error: Tristate,
}

#[derive(Serialize, Default, Copy, Clone)]
#[serde(untagged)]
pub enum Tristate {
    #[default]
    Unset,
    Clear,
    Set(bool),
}

impl Tristate {
    fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }
}

fn deserialize_tristate<'de, D>(deserializer: D) -> Result<Tristate, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<bool>::deserialize(deserializer)? {
        None => Tristate::Clear,
        Some(value) => Tristate::Set(value),
    })
}

/// Which ACP `ContentBlock` an attachment maps to. The string form
/// (`"image"` / `"audio"` / `"resource"`) is the wire contract shared
/// with the web composer and the prompt-request DTO in `protocol.rs`,
/// so renaming a variant breaks the build on both sides rather than
/// silently dropping attachments.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptAttachmentRef {
    pub id: String,
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Decoded byte length, for the UI to show a size hint without
    /// fetching the blob.
    // A JSON number on the wire; ts-rs maps u64 to `bigint`, which
    // `JSON.parse` never produces.
    #[cfg_attr(test, ts(type = "number"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedPromptEntry {
    /// Client-minted stable id, unchanged across edits. Doubles as the
    /// optimistic-echo reconcile key on the client.
    pub id: String,
    /// Server-assigned monotonic order; the queue drains by ascending `seq`.
    // A JSON number on the wire; ts-rs maps u64 to `bigint`, which
    // `JSON.parse` never produces.
    #[cfg_attr(test, ts(type = "number"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpWorkerState {
    /// No worker for this session and no resume in flight.
    #[default]
    Absent,
    /// A spawn or attach is in progress; the UI shows the "Resuming…"
    /// banner + sidebar chip.
    Resuming,
    /// Worker is online and reachable.
    Running,
    /// A stop is in progress and the runner is not yet proven dead. The
    /// session refuses prompts and resumes until it settles.
    Stopping,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
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

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextResumeIndeterminateReason {
    RuntimeCheckRequired,
    AgentHandshakeRequired,
}

/// Whether the daemon can preserve agent context during a future authorized
/// lifecycle transition. This is not current start eligibility.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
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

/// One unresolved structured (ACP) approval, projected for the home TUI's
/// permission-response dialog. The TUI resolves the `nonce` through the ACP
/// resolver and shows `tool_name` / `target` / `destructive` so the user
/// sees what they are answering without entering the structured view. No
/// dashboard surface renders this; the web client ignores the field.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub nonce: String,
    pub tool_name: String,
    pub target: String,
    pub destructive: bool,
    /// True when the options are a list of answers (`is_choice_list`), not
    /// an allow/deny vocabulary. The home dialog must not answer these by
    /// kind; the user picks from the labeled options in the structured
    /// view. Defaults false for daemons that predate the field, matching
    /// the pre-#3741 projection.
    #[serde(default)]
    pub choice: bool,
}

/// One session from the daemon. Only `id` is required when decoding.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResponse {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub project_path: String,
    /// Absolute host path of the session's managed artifact directory. The
    /// web transcript maps agent-emitted artifact paths under this root (or
    /// the fixed sandbox mount) to the authenticated artifact route. See #2587.
    #[serde(default)]
    pub artifact_dir: String,
    #[serde(default)]
    pub group_path: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub extra_args: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_reservation: Option<crate::session::LifecycleReservation>,
    #[serde(default)]
    // A JSON number on the wire; ts-rs maps u64 to `bigint`, which
    // `JSON.parse` never produces.
    #[cfg_attr(test, ts(type = "number"))]
    pub lifecycle_generation: u64,
    /// True when the session's structured-view worker was auto-stopped for
    /// inactivity (resumable/dormant), as opposed to a deliberate Stop. Lets
    /// the dashboard render a distinct dormant dot instead of a live-idle one.
    /// A deliberate Stop keeps `status: "Stopped"` and reports `false` here.
    /// See #2250.
    #[serde(default)]
    pub dormant: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_dormant_since: Option<String>,
    #[serde(default)]
    pub pane_dead_observed: bool,
    #[serde(default)]
    pub agent_pane: crate::session::PaneObservation,
    #[serde(default)]
    pub auxiliary: Vec<crate::session::AuxiliaryObservation>,
    #[serde(default)]
    pub yolo_mode: bool,
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_created_at: Option<String>,
    /// Per-session override for the diff base, set via the web "vs &lt;ref&gt;"
    /// picker, the TUI diff view's `b` keybind, or
    /// `aoe session set-base`. Wins over `base_branch`, the profile
    /// default, and auto-detection. See #970.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
    #[serde(default)]
    pub is_sandboxed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_container_name: Option<String>,
    /// True when the session was created with `--scratch`; the
    /// `project_path` points at an auto-provisioned directory under
    /// `<app_dir>/scratch/<id>/` that the deletion path removes. The web
    /// wizard filters these out of the Recent-projects list.
    #[serde(default)]
    pub scratch: bool,
    /// True when the session is marked as a user favorite. Mirrors
    /// `Instance::is_favorited()`; surfaced so the web sidebar can pin
    /// favorited rows and render the `*` marker without re-implementing
    /// the predicate. Cross-feature parity with the TUI's `f`/`F` keybind.
    #[serde(default)]
    pub favorited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorited_at: Option<String>,
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
    #[serde(default)]
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
    #[serde(default)]
    pub has_managed_worktree: bool,
    /// Whether deleting this session has aoe-managed worktree state to remove,
    /// covering single-repo worktrees AND multi-repo workspaces. Only the
    /// delete dialog's worktree/branch checkboxes consume this; keeping it
    /// separate from `has_managed_worktree` avoids lighting up worktree-only
    /// actions (Edit workdir) for workspace sessions (#2363).
    #[serde(default)]
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
    #[serde(default)]
    pub has_terminal: bool,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
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
    /// Whether the daemon can preserve this agent's context across a future
    /// lifecycle transition. This does not report current start eligibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_resume: Option<ContextResumeAvailability>,
    /// Live structured view worker lifecycle. `absent` for tmux sessions or
    /// structured view sessions whose worker has not been spawned/attached
    /// yet; `resuming` while the reconciler is mid-spawn or mid-attach;
    /// `running` once the supervisor holds a live worker. Drives the
    /// sidebar `Resuming…` chip and the per-session banner in the
    /// structured view. See #1088.
    #[serde(default)]
    pub acp_worker_state: AcpWorkerState,
    /// Unresolved structured approvals in request order, projected only for
    /// live (`running` worker) structured sessions. The TUI uses these to
    /// route the existing permission-response dialog through the ACP
    /// resolver and to show what is being approved; no dashboard surface
    /// renders them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approvals: Vec<PendingApproval>,
    /// The provider rate limit this session is parked on, read from the
    /// daemon's durable park rather than a browser-side mirror, so the
    /// sidebar badge clears when a resume lands with no tab open (#3514).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::acp::state::RateLimitInfo>,
    /// Whether `[acp] rate_limit_auto_resume` is on for this session's
    /// profile, so the rate-limit banner can say whether the park ends by
    /// itself. Set by the list handler; omitted by single-session responses,
    /// whose readers keep the value they last saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_auto_resume: Option<bool>,
    /// True when this session's agent can run in structured view: a built-in
    /// with an ACP adapter, or a custom agent whose profile config
    /// declares a valid `agent_acp_cmd`. The web terminal view reads
    /// this to decide whether the "switch to structured view" affordance is
    /// available, replacing the hardcoded client-side tool list.
    #[serde(default)]
    pub acp_capable: bool,
    /// The session's server-owned prompt queue (follow-ups the user lined up
    /// while a turn was busy), ordered by `seq`. The daemon owns it, so it is
    /// visible across the user's devices and survives a client reload; the
    /// structured view renders it and drains happen server-side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<QueuedPromptEntry>,
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
    /// per `crate::session::fork::structured_fork_capable`. Resume-only ACP
    /// agents (e.g. `aoe-agent`) are ACP-capable yet not forkable, so the web
    /// gates "Fork" on this AND `acp_session_id` rather than on a captured id
    /// alone. Omitted (read as not-forkable) for terminal sessions and
    /// non-forkable agents.
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
    #[serde(default)]
    pub claude_fullscreen: bool,
    /// Repos in the multi-repo workspace (empty for single-repo sessions).
    /// Each entry mirrors `WorkspaceRepo` minus paths the dashboard does
    /// not need to display.
    #[serde(default)]
    pub workspace_repos: Vec<WorkspaceRepoSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_cleanup_on_delete: Option<bool>,
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

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlanSummary {
    /// First non-completed step's title, truncated to ~80 chars so the
    /// sidebar row doesn't overflow.
    pub current_step_title: Option<String>,
    /// Count of `PlanEntryStatus::Done` steps.
    pub completed: u32,
    /// Total step count.
    pub total: u32,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceRepoSummary {
    pub name: String,
    pub source_path: String,
    pub branch: String,
    #[serde(default)]
    pub worktree_path: String,
    #[serde(default)]
    pub main_repo_path: String,
    #[serde(default)]
    pub managed_by_aoe: bool,
    #[serde(default)]
    pub branch_preexisting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionsEnvelope {
    pub sessions: Vec<SessionResponse>,
    #[serde(default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_response_decodes_with_only_an_id() {
        // An older daemon that predates a field must not fail the parse and
        // blank a client's whole list; only `id` is load-bearing.
        let row: SessionResponse = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        assert_eq!(row.id, "a");
        assert_eq!(row.status, "");
        assert_eq!(row.view, crate::session::View::Terminal);
        assert_eq!(row.acp_worker_state, AcpWorkerState::Absent);
        assert!(!row.cleanup_defaults.delete_to_trash);
        assert!(row.workspace_repos.is_empty());
        assert_eq!(row.context_resume, None);

        assert!(serde_json::from_str::<SessionResponse>(r#"{"title":"no id"}"#).is_err());
    }

    /// The generated `web/src/lib/apiWire.ts` pins the field *names*: a rename
    /// shows up there as a committed diff. What it cannot pin is the spelling
    /// of a value, which is what an external `curl` consumer and this
    /// project's own hooks match on, so those are pinned here.
    #[test]
    fn the_rest_row_spells_its_enums_the_way_older_clients_match_on() {
        let row: SessionResponse = serde_json::from_str(
            r#"{"id":"a","status":"Running","view":"structured",
                "acp_worker_state":"resuming","smart_rename":"pending"}"#,
        )
        .unwrap();
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["status"], "Running");
        assert_eq!(json["view"], "structured");
        assert_eq!(json["acp_worker_state"], "resuming");
        assert_eq!(json["smart_rename"], "pending");

        // Two nested objects the dashboard reads through rather than flattened
        // onto the row, where a field could collide with one of the row's own.
        assert!(json["cleanup_defaults"].is_object());
        assert_eq!(json["cleanup_defaults"]["delete_to_trash"], false);

        let resuming: SessionResponse = serde_json::from_str(
            r#"{"id":"a","context_resume":{"state":"unavailable","reason":"agent_unsupported"}}"#,
        )
        .unwrap();
        let json = serde_json::to_value(&resuming).unwrap();
        assert_eq!(json["context_resume"]["state"], "unavailable");
        assert_eq!(json["context_resume"]["reason"], "agent_unsupported");
    }

    #[test]
    fn sessions_envelope_decodes_without_workspace_ordering() {
        let envelope: SessionsEnvelope =
            serde_json::from_str(r#"{"sessions":[{"id":"a"}]}"#).unwrap();
        assert_eq!(envelope.sessions.len(), 1);
        assert!(envelope.workspace_ordering.is_empty());
    }
}
