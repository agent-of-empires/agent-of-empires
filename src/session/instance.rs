//! Session instance definition and operations

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::truncate_id;
use crate::containers::{self, DockerContainer};
use crate::tmux;

use super::container_config;
use super::environment::{build_docker_env_args, shell_escape};
use super::poller::SessionPoller;

use crate::session::capture::{
    capture_claude_session_id, capture_claude_session_id_in_container, capture_codex_session_id,
    capture_copilot_session_id, capture_gemini_session_id, capture_hermes_session_id,
    capture_pi_session_id, capture_vibe_session_id, claude_poll_fn, claude_poll_fn_sandboxed,
    codex_poll_fn, codex_poll_fn_sandboxed, copilot_poll_fn, gemini_poll_fn,
    gemini_poll_fn_sandboxed, generate_claude_session_id, hermes_poll_fn, hermes_poll_fn_sandboxed,
    is_valid_session_id, opencode_poll_fn, opencode_poll_fn_sandboxed, pi_poll_fn,
    pi_poll_fn_sandboxed, try_capture_codex_session_id_in_container,
    try_capture_gemini_session_id_in_container, try_capture_hermes_session_id_in_container,
    try_capture_opencode_session_id, try_capture_opencode_session_id_in_container,
    try_capture_pi_session_id_in_container, try_capture_vibe_session_id_in_container,
    validated_session_id, vibe_poll_fn, vibe_poll_fn_sandboxed,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    #[serde(default)]
    pub created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Waiting,
    #[default]
    Idle,
    Unknown,
    Stopped,
    Error,
    Starting,
    Deleting,
    Creating,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Idle => "idle",
            Status::Unknown => "unknown",
            Status::Stopped => "stopped",
            Status::Error => "error",
            Status::Starting => "starting",
            Status::Deleting => "deleting",
            Status::Creating => "creating",
        }
    }

    /// Whether this status blocks an in-place worktree edit (move dir /
    /// rename branch). The worktree's checkout must be quiescent: an
    /// actively running agent, a session mid-start, or one being
    /// created/deleted can hold the directory or race the metadata write.
    /// Idle/Stopped/Error/Unknown sessions are safe to edit.
    pub fn blocks_worktree_edit(self) -> bool {
        matches!(
            self,
            Status::Running
                | Status::Waiting
                | Status::Starting
                | Status::Creating
                | Status::Deleting
        )
    }
}

/// `last_error` the status poller stamps when a session's tmux pane is simply
/// absent (killed, exited, server reboot) and nothing more specific was
/// captured from the pane. The preview treats this as the calm "Stopped" case
/// rather than a red crash error, since it carries no diagnostic detail.
pub const TMUX_SESSION_GONE_ERROR: &str =
    "tmux session is gone. The agent process may have exited or been killed.";

/// Outcome of `start_with_resume_fallback`.
///
/// Tmux/process failures propagate as `Err` so callers keep the existing
/// `Status::Error` + `last_error` path. Resume-probe death is represented
/// explicitly as `ResumeFailed` because it preserves durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session ID was set and resume succeeded; pane is alive.
    Resumed,
    /// Resume was attempted, but the pane died during the probe before AoE
    /// observed an explicit invalid-resume signal. The sid was preserved and
    /// marked so startup recovery does not retry it automatically.
    ResumeFailed { sid: String },
    /// No resume cascade ran. Either no prior sid, the agent doesn't support
    /// resume, the sid was invalid, the session is structured view-mode (no tmux
    /// pane), or the tmux session was already alive when entered (so
    /// `start_with_size_opts` was a no-op and the probe had nothing to
    /// detect). The pane is alive on return; whether a fresh launch
    /// actually occurred this call depends on the caller having killed
    /// any pre-existing pane first.
    Fresh,
    /// A resume was skipped, and the session started fresh instead, because
    /// `sid` already failed a resume probe once before. Retrying the
    /// identical sid would only reproduce the original `ResumeFailed`
    /// forever, so this launch routes through `ResumeIntent::Cleared`
    /// instead (same as a manual `aoe session set-session-id ""`): a fresh
    /// sid is assigned and `sid` is not carried forward. Distinct from
    /// `Fresh` so callers can tell the user their conversation did not
    /// resume, instead of silently starting a blank session; the prior
    /// conversation is still reachable through the agent's own resume/
    /// history picker. See #2609.
    FreshAfterFailedResume { sid: String },
}

/// Governs whether `start_with_resume_fallback` may pass `--resume <sid>` at
/// all, independent of the per-sid loop-breaker (`resume_probe_failed_sid`),
/// which always applies regardless of policy. `HonorAutoResumeSetting` is
/// used by explicit user restart/reattach (`e`, `Enter`); `Allow` is used by
/// Send Message and Live Send, which must keep trying to preserve agent
/// context even when the user has disabled auto-resume for manual restarts.
/// See #2609.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeAttemptPolicy {
    HonorAutoResumeSetting,
    Allow,
}

/// What `start_with_size_opts` did with the agent's session id this call.
/// `start_with_resume_fallback` matches on `Existing` to gate the Tier-1
/// settle probe; without the gate, fresh Claude launches mislabel as
/// `StartOutcome::Resumed` because `acquire_session_id` always assigns a
/// UUID for Claude.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSidOutcome {
    /// `acquire_session_id` reused a prior sid: `ResumeIntent::Use(sid)`,
    /// observed `agent_session_id`, or retroactive-capture hit. The launch
    /// command embedded the agent's resume flag.
    Existing { sid: String },
    /// `acquire_session_id` returned a fresh sid (Claude UUID generation)
    /// or `None`. No prior conversation continued.
    Fresh,
    /// `start_with_size_opts` short-circuited before `apply_session_flags`
    /// ran: structured view-mode session, or pre-existing tmux pane (kill_clean
    /// cache race). `agent_session_id` was not mutated this call.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    Alive,
    Dead,
}

const RESUME_PROBE_MAX: std::time::Duration = std::time::Duration::from_millis(3000);
const RESUME_PROBE_POLL: std::time::Duration = std::time::Duration::from_millis(50);
/// Grace window we keep observing after the pane stops running its boot
/// shell, before declaring `Alive`. Sized to cover the longest in-pane
/// boot a real agent takes before it would have crashed on a bad sid:
/// opencode (bun-compiled native binary that loads JS, parses argv, and
/// hits the session-not-found path) reaches `pane_dead = true` between
/// ~900ms and ~1100ms after spawn on a warm cache, longer on cold or
/// heavy projects. Healthy resumes pay this entire window once; the pane is
/// fully attachable for the duration so the cost is purely in the synchronous
/// restart path's latency, not in agent responsiveness afterward.
const RESUME_PROBE_POST_SHELL_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Pure decision: should a launch with this sid/tool use the resume probe?
/// Extracted for unit-testability: the probe path itself needs a real tmux
/// session to test end-to-end.
pub(crate) fn should_attempt_resume(agent_session_id: Option<&str>, tool: &str) -> bool {
    let valid = agent_session_id.map(is_valid_session_id).unwrap_or(false);
    if !valid {
        return false;
    }
    !matches!(
        crate::agents::get_agent(tool).map(|a| &a.resume_strategy),
        Some(crate::agents::ResumeStrategy::Unsupported) | None,
    )
}

/// Outcome of `Instance::ensure_pane_ready`. Callers surface this so the user
/// knows what (if anything) happened on their behalf before a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureReadyOutcome {
    /// Pane was already alive; no action taken.
    AlreadyAlive,
    /// Pane was dead (`#{pane_dead}=1`) and was respawned via the restart path.
    Respawned,
    /// Tmux session did not exist and was started via the resume-fallback
    /// path. Healthy resume and fresh launch both use this outcome;
    /// ambiguous probe failures use `ResumeFailed` instead.
    Started,
    /// Resume failed ambiguously while trying to start or respawn the pane.
    /// The durable sid remains stored for an explicit retry.
    ResumeFailed { sid: String },
}

/// How a session is rendered. `Structured` uses the ACP-based native
/// rendering (plan panels, tool-call cards, approvals); `Terminal` streams
/// the raw tmux/PTY through xterm.js. `Terminal` is the conservative
/// deserialization default; session creation sets the value explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Terminal,
    Structured,
}

impl View {
    /// `skip_serializing_if` predicate: only the non-default `Structured`
    /// value is persisted, mirroring the old `structured_view` bool shape.
    pub fn is_terminal(&self) -> bool {
        matches!(self, View::Terminal)
    }
}

/// Errors `ensure_pane_ready` can return. Separating transient lifecycle
/// states from real tmux failures lets HTTP callers map them to 409 (retry)
/// vs 500 (real failure) instead of lumping everything as a tmux error.
#[derive(Debug)]
pub enum EnsureReadyError {
    /// Instance is mid-lifecycle (Creating/Deleting). Caller should retry.
    Transient(Status),
    /// Instance is structured view-mode (no backing tmux pane); send is not supported.
    StructuredView,
    /// Underlying tmux operation failed.
    Tmux(anyhow::Error),
}

impl std::fmt::Display for EnsureReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnsureReadyError::Transient(status) => {
                write!(
                    f,
                    "Session is mid-lifecycle ({status:?}); cannot send right now"
                )
            }
            EnsureReadyError::StructuredView => write!(
                f,
                "Acp-mode sessions have no tmux pane; send is not supported"
            ),
            EnsureReadyError::Tmux(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EnsureReadyError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub branch: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    pub created_at: DateTime<Utc>,
    /// Branch the worktree was created from when `managed_by_aoe` is
    /// true. None means "the repo's default branch was used" (the
    /// historical behavior before #948) or the worktree was attached
    /// to a pre-existing branch (`create_branch = false`). Surfaced
    /// in `aoe list --json`, the TUI preview, and the web sessions
    /// API; not used by core logic, so old `sessions.json` files
    /// deserialize without the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRepo {
    pub name: String,
    pub source_path: String,
    pub branch: String,
    pub worktree_path: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
}

fn default_true() -> bool {
    true
}

fn status_hook_env_prefix(
    profile: &str,
    instance_id: &str,
    agent: Option<&crate::agents::AgentDef>,
) -> String {
    let has_hooks = agent.is_some_and(|a| a.hook_config.is_some() || a.sidecar_hooks.is_some());

    if has_hooks {
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} ",
            shell_escape(profile),
            shell_escape(instance_id)
        )
    } else {
        String::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub branch: String,
    pub workspace_dir: String,
    pub repos: Vec<WorkspaceRepo>,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub cleanup_on_delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    pub image: String,
    pub container_name: String,
    /// Additional environment entries (session-specific).
    /// `KEY` = pass through from host, `KEY=VALUE` = set explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_env: Option<Vec<String>>,
    /// Custom instruction text to inject into agent launch command
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,
    /// The container's working directory, captured from
    /// `ContainerConfig::working_dir` when the container is created (and
    /// backfilled from a live container for sessions created before this field
    /// existed). [`Instance::container_workdir`] returns this verbatim so every
    /// `docker exec -w` targets the path the container was actually built with,
    /// instead of a live recomputation that can drift once the host worktree's
    /// git linkage breaks (#2414).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_workdir: Option<String>,
    /// `KEY=VALUE` pairs minted on the host by `host_hooks.before_start` when
    /// the container last came up. Injected into the container environment as
    /// inherited (leak-safe) entries by `super::environment::collect_environment`.
    ///
    /// Runtime-only and secret: never serialized (so short-lived tokens never
    /// hit disk and a stale value never survives a restart) and re-minted on the
    /// next container come-up. See `Instance::ensure_before_start_env`.
    #[serde(skip)]
    pub before_start_env: Vec<(String, String)>,
}

/// Deserialize agent_session_id, treating empty/whitespace strings as None.
fn deserialize_session_id<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.trim().is_empty()))
}

/// User intent gating `acquire_session_id`, persisted independently of the
/// poller's observation in `agent_session_id`. CLI/REST/TUI write intent;
/// the poller writes observation. Disjoint writers, no race.
///
/// `#[serde(rename)]` pins wire names so a Rust-side variant rename
/// cannot silently break existing `sessions.json` deserialisation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub(crate) enum ResumeIntent {
    /// Fall back to the poller's observed `agent_session_id`.
    #[default]
    #[serde(rename = "Default")]
    Default,
    /// Pin to this sid: pass `--resume <sid>` regardless of observation.
    #[serde(rename = "Use")]
    Use(String),
    /// Force a fresh start on the next launch. Auto-promotes to `Default`
    /// after the launch completes (one-shot semantics).
    #[serde(rename = "Cleared")]
    Cleared,
    /// One-shot fork seed: on the next (first) launch, resume `from` and fork
    /// into a NEW session whose id was pre-pinned in `agent_session_id`.
    /// Auto-promotes to `Default` after that launch, exactly like `Cleared`,
    /// so later restarts resume the child's own id with a plain `--resume`.
    #[serde(rename = "Fork")]
    Fork { from: String },
}

impl ResumeIntent {
    fn is_default(&self) -> bool {
        matches!(self, ResumeIntent::Default)
    }
}

/// Mutually-exclusive lifecycle bucket a session belongs to, computed by
/// `Instance::effective_bucket()`. Precedence is `Trashed > Archived >
/// Active`. Used to route a session into the right list (active sidebar,
/// archived fold, or trash view) and to filter the `GET /api/sessions`
/// response by `?state=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Active,
    Archived,
    Trashed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub title: String,
    /// The last title written by the `smart_rename` automatic renamer.
    /// An auto-rename overwrites `title` only while `title` is still a
    /// default civ name or still equals this value, so a forced retry can
    /// replace an automatic title while a manual rename (which changes `title`
    /// but not this) is left untouched.
    /// `None` on legacy records and freshly created sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_auto_title: Option<String>,
    pub project_path: String,
    #[serde(default)]
    pub group_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extra_args: String,
    #[serde(default)]
    pub tool: String,
    /// Built-in agent name used for status detection, resolved at build time from
    /// config's agent_detect_as map. Avoids loading config during the polling hot path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detect_as: String,
    #[serde(default)]
    pub yolo_mode: bool,
    #[serde(default)]
    pub status: Status,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Wall-clock time of the most recent transition into `Idle`. Used by
    /// the TUI and web dashboard to highlight a freshly-stopped session
    /// for the duration of the configured idle-decay window
    /// (`Config.theme.idle_decay_minutes`); past the window the row drops
    /// back to the regular static idle look. Distinct from
    /// `last_accessed_at`, which is also bumped on user interaction (a
    /// viewed session stays "fresh" by design). `None` for non-Idle
    /// sessions or those that transitioned before this field existed.
    ///
    /// Named `idle_entered_at` rather than `idle_since` to avoid collision
    /// with `DwellState::idle_since` in `src/server/push.rs`, which is an
    /// in-process `Instant` for push-notification dwell timing, a
    /// different concept with a different type and lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_entered_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,

    /// Favorite marker; sibling of archive. When set AND the session is in
    /// a "needs help" status (Waiting, Error, Idle, Unknown), the session
    /// pre-empts all non-favorited peers in the same status tier, pinning it
    /// to the top of the Attention sort. In Running / Stopped / transient
    /// statuses the flag is visible (⭐ glyph + bold) but does NOT re-rank
    /// since live work isn't interrupted by a decoration. Opposite of archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorited_at: Option<DateTime<Utc>>,

    /// Snooze marker, a "temporary archive." When `snoozed_until` is in the
    /// future, the session sorts to tier 99 alongside archived rows and
    /// renders italic+dim with a `z ` prefix plus a remaining-time readout
    /// in the age column. When the timestamp falls into the past, the
    /// `is_snoozed()` predicate returns false and the row naturally rejoins
    /// the active attention sort (the stale timestamp stays on disk until
    /// the next mutation rewrites it, which is harmless). Mutually compatible with
    /// `favorited_at`: a snoozed favorite keeps its star when it wakes up.
    /// Archive wins over snooze (archiving a snoozed session clears nothing
    /// but renders as archive since is_archived() is checked first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,

    /// Unread marker: a session that needs attention. Set automatically when a
    /// turn finishes (`Running -> Idle`) and also by the manual `u` toggle;
    /// cleared by engaging with the session (open/attach, enter live-send,
    /// click, or dwell on it in the list) or the manual toggle. Surfaced as a
    /// non-intrusive `theme.unread` row color and an Attention-sort promoter
    /// ranked just below Waiting. The whole feature is gated behind
    /// `unread_enabled()` (the `session.unread_indicator` config toggle, on by
    /// default); when off, the field is never written and changes nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,

    /// Internal structured view idle-dormancy marker. Set by the reconciler's
    /// idle-reap pass when a structured view worker is shut down for inactivity
    /// (`acp.auto_stop_idle_secs`); while set, the reconciler skips
    /// respawning the worker, so the session stays stopped until the
    /// user comes back. Cleared by `touch_last_accessed()` (the same
    /// wake path that clears archive/snooze), so the next prompt revives
    /// the worker on the following reconciler tick. Distinct from
    /// `snoozed_until` (user-facing, deadline-based, sorts to tier 99)
    /// and `archived_at` (user-facing hide): dormancy is invisible to
    /// the UI sort and exists only to suppress auto-respawn. See #1689.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_dormant_since: Option<DateTime<Utc>>,

    /// Web-only pin marker. Distinct from `favorited_at`: favorite is the
    /// TUI attention-sort within-tier pin, while pin is a hard top-of-sort
    /// surfacing primitive surfaced through the web sidebar (where the TUI's
    /// Attention sort does not exist). Mutually exclusive with the sink
    /// states (`archived_at`, `snoozed_until`) via the `pin()` mutator and
    /// the inverse clear in `archive()` / `snooze()`. Orthogonal to
    /// `favorited_at` (both can be set; they drive different surfaces).
    /// Unlike archive/snooze, `pin` is NOT cleared by `touch_last_accessed`
    /// because it is an explicit persistent surfacing signal, not a sink
    /// state that "user is engaging" implicitly contradicts. See #1581.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<DateTime<Utc>>,

    /// Trash marker: the session is soft-deleted. A trashed row is hidden
    /// from every normal and archived view (trash is its own bucket, see
    /// `effective_bucket()`), its live processes are stopped, but its
    /// durable state (structured-view transcript, event rows, worktree,
    /// branch, container) is kept on disk so `restore` is faithful.
    /// Permanent teardown happens only at purge (the historical delete
    /// path) or when the configured retention window
    /// (`session.trash_retention_days`) elapses from `trashed_at`.
    ///
    /// Unlike `archive()`, `trash()` does NOT clear the sibling triage
    /// timestamps (`archived_at`, `favorited_at`, `snoozed_until`,
    /// `pinned_at`): trash takes precedence in bucketing while those are
    /// preserved, so a restored favorite comes back a favorite. Additive:
    /// absent in older `sessions.json` rows, so no migration is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<DateTime<Utc>>,

    /// The `project_path` a managed-worktree session had before it was
    /// trashed, captured when the trash flow relocates the worktree into the
    /// `.aoe-trash` holding area (see `src/session/trash.rs`). `project_path`
    /// is repointed to the trash location while trashed so the structured-view
    /// preview, diff, and purge keep reading the worktree at its real spot;
    /// restore moves the worktree back here and clears this field. `None` for
    /// sessions that were never relocated (plain / non-managed worktrees, or
    /// rows trashed before relocation existed). Additive: absent in older
    /// `sessions.json` rows, so no migration is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_trash_project_path: Option<String>,

    /// Namespaced per-session plugin data, keyed by plugin id. Each plugin
    /// owns only its own slot (`plugin_meta["<id>"]`), an opaque JSON value it
    /// reads and writes through the host API that lands with the Tier 1 host
    /// (#2095). Data for an uninstalled plugin is retained, since it is cheap
    /// and reinstalling restores the session's state. Additive: absent in
    /// older `sessions.json` rows, so no migration is needed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugin_meta: std::collections::BTreeMap<String, serde_json::Value>,

    /// Scratch-session marker. When true, `project_path` points at an
    /// auto-provisioned directory under `<app_dir>/scratch/<id>/` that the
    /// deletion path removes on `aoe rm` (unless the user opts in to keeping
    /// the directory). Mutually exclusive with worktree/workspace.
    /// See `src/session/scratch.rs`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scratch: bool,

    // Git worktree integration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_info: Option<WorktreeInfo>,

    // Multi-repo workspace integration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_info: Option<WorkspaceInfo>,

    // Docker sandbox integration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_info: Option<SandboxInfo>,

    // Paired terminal session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_info: Option<TerminalInfo>,

    // Agent session ID for conversation persistence
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_session_id"
    )]
    pub agent_session_id: Option<String>,

    /// Durable loop-breaker for ambiguous resume-probe failures. When this
    /// equals `agent_session_id`, startup recovery skips automatic resume so a
    /// transient pane crash does not repeatedly re-run the same failed probe.
    /// Explicit user actions can still retry the preserved sid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resume_probe_failed_sid: Option<String>,

    /// User intent gating `acquire_session_id`. See `ResumeIntent` for
    /// semantics. Non-`Default` values (`Use`, `Cleared`) are written only
    /// by user-initiated CLI commands; daemon-internal paths demote to
    /// `Default` only (one-shot `Cleared` auto-promote, cascade Tier-1
    /// `Use(stale_sid)` downgrade), both CAS-guarded, so a daemon restart
    /// cannot silently undo a user-set pin.
    #[serde(default, skip_serializing_if = "ResumeIntent::is_default")]
    pub(crate) resume_intent: ResumeIntent,

    /// Runtime-only, one-shot: set by `start_with_resume_fallback` right
    /// before calling `start_with_size_opts` to force this single launch
    /// through the `ResumeIntent::Cleared` path (no `--resume` flag, fresh
    /// sid) without persisting a real `Cleared` write ahead of time. Not
    /// serialized; `reconcile_from_disk` explicitly carries it across the
    /// `*self = disk` reload since it otherwise has no disk representation.
    /// Consumed (reset to `false`) at the top of `start_with_size_opts`. See
    /// #2609.
    #[serde(skip)]
    pub(crate) force_fresh_next_launch: bool,

    /// Runtime-only: which profile this instance was loaded from. Not persisted to disk.
    #[serde(default, skip_serializing)]
    pub source_profile: String,

    // Push-notification per-session overrides. None means "inherit the
    // server-wide default for this event type" (WebConfig.notify_on_*).
    // Some(true)/Some(false) is an explicit user toggle and takes
    // precedence over the global. Because the overrides are per-event-
    // type, a session can opt INTO an event that is globally off (e.g.,
    // Running to Idle), not just opt out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_waiting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_idle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_error: Option<bool>,

    /// Per-session override for the diff base ref. Takes precedence
    /// over `DiffConfig.default_branch` and the auto-detected default
    /// branch. Set when the eventual PR target differs from the project
    /// default (e.g. stacked PRs, hotfix off `release/*`). See #970.
    ///
    /// Accepts either a short branch name (`"main"`, `"release-1.2"`)
    /// or a remote-qualified ref (`"upstream/main"`); the diff resolver
    /// hands it straight to `compute_changed_files`, whose
    /// `get_commit_from_ref` resolves both forms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,

    /// How this session is rendered: `Structured` (ACP native rendering) or
    /// `Terminal` (raw tmux pane). When `Structured`, aoe spawns an ACP agent
    /// subprocess and renders structured events natively; tmux integration is
    /// bypassed for this session.
    #[serde(default, skip_serializing_if = "View::is_terminal")]
    pub view: View,
    /// Optional structured view agent name (e.g., "claude-code", "aoe-agent",
    /// "gemini"). When None, the structured view picks the default for the
    /// session's tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Optional model id forwarded to aoe-agent (e.g., "claude-opus-4-7",
    /// "gpt-5", "llama3.3:ollama").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    /// Agent-assigned ACP session id captured from `session/new`. When
    /// the agent advertises `agent_capabilities.load_session = true`
    /// (claude-agent-acp does), the next spawn calls `session/load`
    /// with this id so the agent reloads its on-disk transcript and
    /// the model retains context across `aoe serve` restarts. Cleared
    /// on acp_disable, session delete, or `session/load` failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,

    /// Set when this session was imported from an existing Claude Code
    /// session on disk. While true, the next structured spawn seeds the
    /// event store from the agent's `session/load` history replay (instead
    /// of suppressing it like a normal reattach does) so the imported
    /// transcript renders. Cleared once the load completes and the history
    /// is durably stored. See #2276.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_pending: Option<bool>,

    /// One-shot structured-fork seed: the parent ACP session id to fork from
    /// on first connect. Set at creation, consumed when the adapter assigns
    /// the forked child id (see `apply_acp_session_change`). `None` for
    /// non-fork sessions. Skipped in serialization when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_pending: Option<String>,

    // Runtime state (not serialized)
    #[serde(skip)]
    pub last_error_check: Option<std::time::Instant>,
    #[serde(skip)]
    pub last_start_time: Option<std::time::Instant>,
    /// Last status a caller has actually observed live, as distinct from
    /// the disk-loaded `status` field. `None` means no live observation
    /// exists yet for this in-memory object, so
    /// [`Self::update_status_with_metadata`] seeds the baseline on its
    /// first call without restamping. Every fresh disk load (TUI
    /// relaunch, daemon tick) starts with `None` because of
    /// `#[serde(skip)]`, and [`Instance::new`] also starts with `None` so
    /// in-memory and disk-loaded paths have the same first-check
    /// semantics. See #2690.
    ///
    /// The `#[serde(skip)]` + `Instance::new`-time `None` seed rely on
    /// construction-ordering: [`Instance::new`] is called before the
    /// instance enters any shared state (`state.instances`, `Storage`),
    /// so a poll thread cannot observe it mid-construction. Safety here
    /// is by construction-ordering, not by synchronization.
    #[serde(skip)]
    pub live_status_baseline: Option<Status>,
    #[serde(skip)]
    pub last_error: Option<String>,
    #[serde(skip)]
    pub session_id_poller: Option<Arc<Mutex<SessionPoller>>>,

    /// Runtime-only set of session IDs that retroactive capture must NOT
    /// re-discover from on-disk artifacts after an explicit resume-target
    /// invalidation. On-disk artifacts (opencode db, vibe meta.json, codex
    /// state, etc.) can retain the old row for several minutes.
    ///
    /// `#[serde(skip)]` is intentional. If the daemon dies between the
    /// explicit invalidation clearing the on-disk sid and the artifact decaying
    /// (~5-10 min), the next launch starts with this set empty and the
    /// freshly-spawned poller can re-import the bad sid once. The next
    /// `start_with_resume_fallback` then re-runs the invalidation and clears it
    /// again. Self-healing within one cycle; persisting a TTL set isn't
    /// worth the schema cost.
    #[serde(skip)]
    pub(crate) retroactive_capture_excludes: HashSet<String>,

    /// Cached `is_pane_dead()` reading from the most recent status_poller
    /// tick. Lets the Attention comparator treat dead-pane rows as sunk
    /// (tier 99) without re-querying tmux on every sort. Field name avoids
    /// `pane_dead` to prevent shadowing `tmux::Session::is_pane_dead()` at
    /// call sites that take both. Refreshed by status_poller; not persisted
    /// (clears to false on TUI restart, which is correct; a fresh poll
    /// will re-set it within one tick if the pane is genuinely dead).
    #[serde(skip)]
    pub pane_dead_observed: bool,

    /// Live FileWatchService handle for in-process Local fast-path
    /// notifications when this Instance's storage is mutated. `None` for
    /// Instances created via `Instance::new` without explicit injection;
    /// `Storage::load*` injects its own Arc into every loaded Instance
    /// so daemon and TUI hot paths reach the live service. Use sites
    /// fall back to `FileWatchService::noop()` when `None`, so ad-hoc
    /// constructions remain functional without an explicit injection.
    #[serde(skip, default)]
    pub(crate) file_watch: Option<std::sync::Arc<crate::file_watch::FileWatchService>>,
}

/// Append yolo-mode flags or environment variables to a launch command.
fn apply_yolo_mode(cmd: &mut String, yolo: &crate::agents::YoloMode, is_sandboxed: bool) {
    match yolo {
        crate::agents::YoloMode::CliFlag(flag) => {
            *cmd = format!("{} {}", cmd, flag);
        }
        crate::agents::YoloMode::EnvVar(key, value) if !is_sandboxed => {
            *cmd = format_env_var_prefix(key, value, cmd);
        }
        crate::agents::YoloMode::EnvVar(..) | crate::agents::YoloMode::AlwaysYolo => {}
    }
}

fn build_resume_flags(tool: &str, session_id: &str, is_existing_session: bool) -> String {
    use crate::agents::{get_agent, ResumeStrategy};

    if !is_valid_session_id(session_id) {
        tracing::warn!(target: "session.store",
            "Refusing to build resume flags: invalid session ID {:?}",
            session_id
        );
        return String::new();
    }
    let Some(agent) = get_agent(tool) else {
        return String::new();
    };
    match &agent.resume_strategy {
        ResumeStrategy::Flag(flag) => format!("{} {}", flag, session_id),
        ResumeStrategy::FlagPair {
            existing,
            new_session,
        } => {
            let flag = if is_existing_session {
                existing
            } else {
                new_session
            };
            format!("{} {}", flag, session_id)
        }
        ResumeStrategy::Subcommand(sub) => format!("{} {}", sub, session_id),
        ResumeStrategy::Unsupported => String::new(),
    }
}

/// Build the launch flags for a one-shot terminal fork. Returns the empty
/// string for an unforkable agent or an invalid id (mirroring
/// `build_resume_flags`'s fail-closed contract). The child id is pre-pinned so
/// the forked session is durable on disk before launch.
fn build_fork_flags(tool: &str, parent_id: &str, child_id: &str) -> String {
    use crate::agents::{get_agent, ForkStrategy, ResumeStrategy};

    if !is_valid_session_id(parent_id) || !is_valid_session_id(child_id) {
        tracing::warn!(target: "session.store",
            "Refusing to build fork flags: invalid id (parent={parent_id:?} child={child_id:?})");
        return String::new();
    }
    let Some(agent) = get_agent(tool) else {
        return String::new();
    };
    match agent.fork_strategy {
        ForkStrategy::ClaudeFork => {
            format!("--resume {parent_id} --fork-session --session-id {child_id}")
        }
        ForkStrategy::CodexFork => {
            // Codex mints its own forked id; child_id is unused. The subcommand
            // is inserted after the binary by apply_session_flags.
            format!("fork {parent_id}")
        }
        ForkStrategy::Flag(fork_flag) => {
            // Resume the parent session (using the agent's own resume flag),
            // then add the fork flag; the agent mints the new id.
            match agent.resume_strategy {
                ResumeStrategy::Flag(resume_flag) => {
                    format!("{resume_flag} {parent_id} {fork_flag}")
                }
                _ => String::new(),
            }
        }
        ForkStrategy::Unsupported => String::new(),
    }
}

/// Splice `part` into `cmd`: insert it right after the binary (before other
/// flags) when it is a subcommand, else append it. Shared by the resume and
/// fork launch-flag builders.
fn splice_subcommand_or_append(cmd: &mut String, part: &str, is_subcommand: bool) {
    if is_subcommand {
        if let Some(space_pos) = cmd.find(' ') {
            let binary = &cmd[..space_pos];
            let flags = &cmd[space_pos..];
            *cmd = format!("{} {}{}", binary, part, flags);
        } else {
            *cmd = format!("{} {}", cmd, part);
        }
    } else {
        *cmd = format!("{} {}", cmd, part);
    }
}

fn append_resume_flags(
    tool: &str,
    session_id: Option<&str>,
    is_existing_session: bool,
    cmd: &mut String,
    context: &str,
) -> bool {
    use crate::agents::{get_agent, ResumeStrategy};

    if let Some(session_id) = session_id {
        let resume_part = build_resume_flags(tool, session_id, is_existing_session);
        if resume_part.is_empty() {
            return false;
        }
        let is_subcommand = matches!(
            get_agent(tool).map(|a| &a.resume_strategy),
            Some(ResumeStrategy::Subcommand(_))
        );
        splice_subcommand_or_append(cmd, &resume_part, is_subcommand);
        tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, resume_part);
        return true;
    }
    false
}

/// Outcome of a CAS-guarded `agent_session_id` or `resume_intent` write.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidWrite {
    /// Disk matched `expected_prior`; new value committed.
    Applied,
    /// Disk diverged (peer wrote between caller's read and this write);
    /// caller should reload the in-memory mirror from disk.
    Skipped,
    /// I/O failure or row gone from disk; in-memory mirror is unchanged.
    Failed,
}

/// Caller contract for `persist_session_id`: whether to publish the
/// post-CAS `agent_session_id` to the tmux hidden env.
///
/// `Published`: memory reflects disk (Applied: just committed; Skipped:
/// reloaded). Caller publishes.
/// `Skip`: memory unchanged on invalid sid, storage error, or row gone.
/// Caller must not touch env.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidPersistOutcome {
    Published,
    Skip,
}

/// CAS-write `agent_session_id` to disk. Caller passes the value the
/// in-memory mirror held at last reconcile as `expected_prior`; the closure
/// inside `Storage::update`'s flock skips the write if disk has diverged
/// (peer-poller observed a different sid). On Skipped, callers should
/// reload memory from disk to converge on the peer's value.
pub(crate) fn persist_session_to_storage(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    if !is_valid_session_id(session_id) {
        tracing::warn!(target: "session.store",
            "Refusing to persist invalid session ID {:?} for {}",
            session_id,
            instance_id
        );
        return SidWrite::Failed;
    }

    let storage = match super::storage::Storage::new(profile, file_watch.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "session.store", "Failed to create storage for session ID persistence: {}", e);
            return SidWrite::Failed;
        }
    };

    let outcome = storage.update(|instances, _groups| {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) {
            if inst.agent_session_id.as_deref() != expected_prior {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected = ?expected_prior,
                    disk = ?inst.agent_session_id,
                    target = session_id,
                    "sid CAS mismatch; skipping persist"
                );
                return Ok(SidWrite::Skipped);
            }
            inst.agent_session_id = Some(session_id.to_string());
            inst.resume_probe_failed_sid = None;
            Ok(SidWrite::Applied)
        } else {
            Ok(SidWrite::Failed)
        }
    });

    match outcome {
        Ok(SidWrite::Applied) => {
            tracing::debug!(target: "session.store", "Session ID persisted for {}", instance_id);
            SidWrite::Applied
        }
        Ok(other) => other,
        Err(e) => {
            tracing::warn!(target: "session.store", "Failed to persist session ID for {}: {}", instance_id, e);
            SidWrite::Failed
        }
    }
}

/// Emit `fresh` only when it differs from the stored session id, the
/// "override only when distinct" contract shared by both branches of
/// `capture_freshest_session_id` (sidecar and mtime fallback).
fn override_if_distinct(stored: Option<&str>, fresh: String) -> Option<String> {
    match stored {
        Some(known) if known == fresh => None,
        _ => Some(fresh),
    }
}

fn tmux_env_session_name_for_instance_id(instance_id: &str) -> Option<String> {
    let suffix = format!("_{}", truncate_id(instance_id, 8));
    let output = crate::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut agent = None;
    let mut terminal = None;
    let mut container = None;
    for name in String::from_utf8_lossy(&output.stdout).lines() {
        if !name.ends_with(&suffix)
            || name.starts_with(tmux::TOOL_PREFIX)
            || crate::tmux::utils::is_pane_dead(name)
        {
            continue;
        }

        if name.starts_with(tmux::TERMINAL_PREFIX) {
            terminal.get_or_insert_with(|| name.to_string());
        } else if name.starts_with(tmux::CONTAINER_TERMINAL_PREFIX) {
            container.get_or_insert_with(|| name.to_string());
        } else if name.starts_with(tmux::SESSION_PREFIX) {
            agent.get_or_insert_with(|| name.to_string());
        }
    }

    agent.or(terminal).or(container)
}

/// Publish a captured session ID to the tmux environment only.
///
/// Background threads (poller on_change) call this so that
/// `build_exclusion_set()` on other instances can see the captured ID
/// without racing with the TUI thread's `save()`.
fn publish_session_to_tmux_env(tmux_session_name: &str, instance_id: &str, session_id: &str) {
    for (key, value) in [
        (crate::tmux::env::AOE_INSTANCE_ID_KEY, instance_id),
        (crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY, session_id),
    ] {
        if let Err(e) = crate::tmux::env::set_hidden_env(tmux_session_name, key, value) {
            tracing::warn!(target: "session.store", "Failed to write {} to tmux env: {}", key, e);
            return;
        }
    }
}

/// A passively-detected status transition, queued for a batched disk write.
/// Produced by the TUI's and daemon's background pollers when a genuine
/// live status change is observed (see [`Instance::update_status_with_metadata`]
/// and its `live_status_baseline` field), consumed by
/// [`Instance::merge_passive_status_patch`]. `pub(crate)`: this is an
/// internal wire format between the pollers and `merge_passive_status_patch`,
/// not a stable type for out-of-tree consumers.
///
/// ## Poller vocabulary (#2690 follow-up)
///
/// - **passive status**: a status transition detected by a background
///   poller from tmux pane state or ACP overlay, not by an explicit user
///   action.
/// - **passive status patch**: a minimal `PassiveStatusPatch` carrying
///   the fields a passive-status writer touches (`status`,
///   `idle_entered_at`, `last_accessed_at`), applied on disk via
///   [`Instance::merge_passive_status_patch`].
/// - **live status baseline**: the last `Status` a caller has actually
///   observed live for an in-memory `Instance`. Held on
///   `Instance::live_status_baseline` (`#[serde(skip)]`). `None` means
///   no live observation exists yet, so
///   [`Instance::update_status_with_metadata`] seeds it on the first
///   call without restamping.
/// - **detected status**: the `Status` a poller reads from tmux / ACP /
///   sandbox liveness on a single call. Distinct from the disk-loaded
///   `Instance::status`, which can be stale by up to one tick.
/// - **poller-authoritative status**: for plain-tmux sessions, the poller
///   owns `Instance::status`. For structured/ACP sessions,
///   `apply_acp_overlay_inplace` is the authority; see its docstring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassiveStatusPatch {
    pub id: String,
    pub status: Status,
    pub idle_entered_at: Option<DateTime<Utc>>,
    /// `None` when the source `Instance` was never touched by a user
    /// (`last_accessed_at` itself `None`); must stay `None` in that case
    /// rather than fabricating a stamp, or a session that transitions
    /// status before anyone ever attaches would gain a spurious
    /// `last_accessed_at` and break the "`None` = never touched" contract
    /// that idle-reap and the freshness sort rely on.
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl PassiveStatusPatch {
    /// Build a patch from the current state of `inst`, as observed by a
    /// background poller. The `last_accessed_at` None-preservation
    /// contract is on [`Self::last_accessed_at`].
    pub(crate) fn from_instance(inst: &Instance) -> Self {
        Self {
            id: inst.id.clone(),
            status: inst.status,
            idle_entered_at: inst.idle_entered_at,
            last_accessed_at: inst.last_accessed_at,
        }
    }
}

impl Instance {
    pub fn new(title: &str, project_path: &str) -> Self {
        Self {
            id: generate_id(),
            title: title.to_string(),
            last_auto_title: None,
            project_path: project_path.to_string(),
            group_path: String::new(),
            parent_session_id: None,
            command: String::new(),
            extra_args: String::new(),
            tool: "claude".to_string(),
            detect_as: String::new(),
            yolo_mode: false,
            status: Status::Idle,
            created_at: Utc::now(),
            last_accessed_at: None,
            idle_entered_at: None,
            archived_at: None,
            favorited_at: None,
            snoozed_until: None,
            unread: false,
            idle_dormant_since: None,
            pinned_at: None,
            trashed_at: None,
            pre_trash_project_path: None,
            plugin_meta: std::collections::BTreeMap::new(),
            scratch: false,
            worktree_info: None,
            workspace_info: None,
            sandbox_info: None,
            terminal_info: None,
            agent_session_id: None,
            resume_probe_failed_sid: None,
            resume_intent: ResumeIntent::Default,
            force_fresh_next_launch: false,
            source_profile: String::new(),
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            base_branch_override: None,
            view: View::Terminal,
            agent_name: None,
            agent_model: None,
            acp_session_id: None,
            import_pending: None,
            fork_pending: None,
            last_error_check: None,
            last_start_time: None,
            live_status_baseline: None,
            last_error: None,
            session_id_poller: None,
            retroactive_capture_excludes: HashSet::new(),
            pane_dead_observed: false,
            file_watch: None,
        }
    }

    /// Inject the live FileWatchService Arc into this Instance for
    /// in-process Local fast-path notifications during subsequent storage
    /// mutations. Called by `Storage::load*` automatically; manual call
    /// sites are daemon-side recovery and TUI session-creation paths that
    /// build Instances without going through Storage::load.
    pub(crate) fn set_file_watch(
        &mut self,
        fw: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) {
        self.file_watch = Some(fw);
    }

    /// Resolve the live `Arc<FileWatchService>` for this Instance, falling
    /// back to a noop service when none was injected (ad-hoc construction
    /// or pre-injection state). Use sites pair this with `Storage::new`
    /// directly because `new_unwatched` would shadow a live injection.
    fn resolve_file_watch(&self) -> std::sync::Arc<crate::file_watch::FileWatchService> {
        self.file_watch
            .clone()
            .unwrap_or_else(crate::file_watch::FileWatchService::noop)
    }

    /// Whether a title rename should also move the worktree directory leaf,
    /// given the resolved `session.tie_workdir_to_name` setting. True only for
    /// aoe-managed worktree sessions: non-worktree (scratch, plain tmux) and
    /// externally-attached worktrees are always a no-op. See #1927.
    pub fn tie_workdir_applies(&self, tie_setting: bool) -> bool {
        tie_setting
            && self
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe)
    }

    /// Whether deleting this session has aoe-managed worktree state to clean
    /// up, covering BOTH single-repo and multi-repo (workspace) sessions.
    /// Single-repo sessions carry an aoe-managed `worktree_info`; workspace
    /// sessions carry `workspace_info` instead (with `worktree_info = None`),
    /// and opt into cleanup via `cleanup_on_delete`. Entry points use this to
    /// decide whether to set `delete_worktree`; gating on `worktree_info`
    /// alone silently leaks the workspace directory (#2363). Mirrors the TUI
    /// group-delete predicate so every surface agrees.
    pub fn has_managed_worktree_or_workspace(&self) -> bool {
        self.worktree_info
            .as_ref()
            .is_some_and(|w| w.managed_by_aoe)
            || self
                .workspace_info
                .as_ref()
                .is_some_and(|ws| ws.cleanup_on_delete)
    }

    /// Stamp `last_accessed_at` to the current time AND wake the session
    /// from any sink state. Call this on user-initiated interactions
    /// (attach, send keys, etc.); every existing call site already does.
    ///
    /// Auto-unarchive/unsnooze: sending a message or attaching is the user
    /// explicitly saying "I care about this now." Leaving `archived_at` or
    /// `snoozed_until` set after such interaction is incoherent; the row
    /// would render italic+dim at tier 99 even while live traffic flows.
    /// User rule (2026-04-23): "messaging should unarchive."
    ///
    /// `favorited_at` is preserved: fav is a positive "care more" signal,
    /// orthogonal to the sink states. A favorited session that was snoozed
    /// stays favorited when the user wakes it.
    pub fn touch_last_accessed(&mut self) {
        self.last_accessed_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
        self.idle_dormant_since = None;
    }

    /// Whether this session's structured view worker was auto-stopped for
    /// inactivity and should not be respawned by the reconciler until the
    /// user wakes it. See `idle_dormant_since` and #1689.
    pub fn is_idle_dormant(&self) -> bool {
        self.idle_dormant_since.is_some()
    }

    /// Mark the session dormant after its structured view worker was auto-stopped
    /// for inactivity. Idempotent: re-marking refreshes the timestamp.
    pub fn mark_idle_dormant(&mut self) {
        self.idle_dormant_since = Some(Utc::now());
    }

    /// Mutates: `status`, `sandbox_info`. Field set must match what
    /// `start_with_size_opts` writes; missing fields re-introduce the
    /// wholesale-replace clobber.
    pub fn merge_post_start(&mut self, src: &Self) {
        self.status = src.status;
        self.sandbox_info = src.sandbox_info.clone();
    }

    /// Same fields as `merge_post_start`. Resume-probe failure markers are
    /// copied only when the sid still matches so peer poller writes that land
    /// between phase 2 and phase 3 of the restart remain authoritative.
    pub fn merge_post_restart(&mut self, src: &Self) {
        self.merge_post_start(src);
        if self.agent_session_id == src.agent_session_id {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    /// Baseline-aware sibling for async restart workers. Copies worker-produced
    /// identity only when the live row still matches the pre-restart snapshot;
    /// peer writes that land while the worker is blocked remain authoritative.
    pub fn merge_post_restart_with_baseline(&mut self, before: &Self, src: &Self) {
        self.merge_post_start(src);
        let sid_unchanged = self.agent_session_id == before.agent_session_id;
        let marker_unchanged = self.resume_probe_failed_sid == before.resume_probe_failed_sid;

        if sid_unchanged {
            self.agent_session_id = src.agent_session_id.clone();
            self.session_id_poller = src.session_id_poller.clone();
        }

        if marker_unchanged && self.agent_session_id == src.agent_session_id {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    /// Reload this instance from disk before a launch that would re-persist
    /// peer-writable fields. Refreshes `agent_session_id` (poller-observed)
    /// and `resume_intent` (user-set) from disk; carries runtime-only fields
    /// (`#[serde(skip)]` + `source_profile`) onto the disk snapshot. Closes
    /// the ~2s `status_poll_loop` lag window in which a CLI peer
    /// `set-session-id` would otherwise be silently overwritten. No-op on
    /// storage error or if the row is gone from disk.
    fn reconcile_from_disk(&mut self) {
        let Ok(storage) =
            super::storage::Storage::new(&self.effective_profile(), self.resolve_file_watch())
        else {
            tracing::warn!(target: "session.store",
                session = %self.id,
                "failed to open storage to reload disk state before launch; using in-memory value");
            return;
        };
        let mut disk = match storage.load() {
            Ok(instances) => match instances.into_iter().find(|i| i.id == self.id) {
                Some(d) => d,
                None => return,
            },
            Err(e) => {
                tracing::warn!(target: "session.store",
                    session = %self.id,
                    error = %e,
                    "failed to load disk state before launch; using in-memory value");
                return;
            }
        };

        // Carry runtime-only fields (`#[serde(skip)]`) and locally-mutated
        // launch-time state from `self` onto the disk snapshot. This carry
        // set is not required to match `merge_runtime_fields` exactly: each
        // reconciliation path feeds a different consumer, and each consumer
        // rewrites the runtime field it observes before reading
        // (`pane_dead_observed` is rewritten by the TUI's status poller
        // before its TUI-only consumers read).
        disk.last_error_check = self.last_error_check;
        disk.last_start_time = self.last_start_time;
        disk.last_error = self.last_error.take();
        disk.session_id_poller = self.session_id_poller.take();
        disk.retroactive_capture_excludes = std::mem::take(&mut self.retroactive_capture_excludes);
        disk.pane_dead_observed = self.pane_dead_observed;
        disk.force_fresh_next_launch = self.force_fresh_next_launch;
        disk.source_profile = std::mem::take(&mut self.source_profile);
        // `before_start_env` is `#[serde(skip)]`, so the disk snapshot always
        // has it empty. Carry the live value forward; otherwise this reload
        // (which runs before every launch) would wipe the host-minted cache and
        // make `get_container_for_instance` re-run the before_start hook on each
        // relaunch of an already-running container, defeating the one-time
        // backfill and re-minting credentials needlessly.
        if let (Some(disk_sandbox), Some(runtime_sandbox)) =
            (disk.sandbox_info.as_mut(), self.sandbox_info.as_ref())
        {
            disk_sandbox.before_start_env = runtime_sandbox.before_start_env.clone();
        }

        *self = disk;
    }

    /// Closes the data-loss window where `/clear` writes the sidecar but
    /// the daemon crashes before the next poll tick persists it: without
    /// this step, the next launch's wipe destroys the fresh sid.
    ///
    /// Claude-only (sole sidecar tool); `Default` intent only (`Use(X)`
    /// and `Cleared` override); excluded sids skipped (cascade re-poison
    /// guard).
    fn reconcile_sidecar_into_disk(&mut self) {
        if self.tool != "claude" {
            return;
        }
        if !matches!(self.resume_intent, ResumeIntent::Default) {
            return;
        }
        let Some(fresh) = crate::hooks::read_hook_session_id(&self.id) else {
            return;
        };
        if Some(&fresh) == self.agent_session_id.as_ref() {
            return;
        }
        if self.retroactive_capture_excludes.contains(&fresh) {
            return;
        }
        let profile = self.effective_profile();
        let baseline = self.agent_session_id.as_deref();
        match persist_session_to_storage(
            &profile,
            &self.id,
            &fresh,
            baseline,
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.agent_session_id = Some(fresh);
            }
            SidWrite::Skipped => {
                // Peer wrote between reconcile and CAS; reload to converge.
                self.reconcile_from_disk();
            }
            SidWrite::Failed => {}
        }
    }

    /// Splice TUI-mirrored, persisted fields from `src` onto `self`. Used by
    /// `HomeView::save` for fields the TUI is the canonical disk writer of
    /// (the daemon's `status_poll_loop` keeps these in memory only). The
    /// server's `send_message` respawn briefly writes `status` via
    /// `apply_post_restart_sync`; the resulting transient mis-paint
    /// converges on the next `status_poll` tick.
    /// User-action fields (archived/favorited/snoozed/title/group_path/...)
    /// are NOT here; they go through `apply_user_action` per-action so peer
    /// writers (CLI) cannot be clobbered by a stale TUI snapshot.
    pub fn merge_from_tui(&mut self, src: &Self) {
        self.status = src.status;
        self.last_accessed_at = self.last_accessed_at.max(src.last_accessed_at);
        self.idle_entered_at = src.idle_entered_at;
    }

    /// Apply a passively-detected status transition to a disk row. Touches
    /// the same three fields as [`Self::merge_from_tui`] (`status`,
    /// `idle_entered_at`, `last_accessed_at`); the real distinction is the
    /// API shape (a minimal [`PassiveStatusPatch`] rather than a full
    /// `Self`) and the merge policy on `last_accessed_at`: `merge_from_tui`
    /// takes the monotone max, this drops the incoming `last_accessed_at`
    /// outright when disk already has a strictly newer one, so a
    /// poller-produced patch loses to a newer explicit user touch instead of
    /// racing it.
    ///
    /// `status`/`idle_entered_at` always apply regardless: the poller is the
    /// sole authority on detected agent status, and gating them on the
    /// `last_accessed_at` comparison would let an unrelated peer touch
    /// strand a real status transition on disk until the next one. See
    /// #2690.
    ///
    /// The `>=` guard on `last_accessed_at` compares `chrono::Utc::now()`
    /// values, which delegate to `SystemTime::now()` (wall clock, not
    /// monotonic). Under an NTP rewind, a genuinely newer live observation
    /// stamped after the rewind can compare less than a value stamped
    /// before it and be silently dropped. Best-effort monotone, not a hard
    /// guarantee; the next poll tick converges regardless.
    ///
    /// A `last_accessed_at` older-or-equal to disk is silently dropped
    /// (the `>=` guard) with a `session.store` debug log at drop time,
    /// while `status` and `idle_entered_at` still apply unconditionally.
    /// Callers relying on the observable `last_accessed_at` change must
    /// re-read the field after `merge_passive_status_patch` returns.
    pub(crate) fn merge_passive_status_patch(&mut self, patch: &PassiveStatusPatch) {
        self.status = patch.status;
        self.idle_entered_at = patch.idle_entered_at;
        let Some(incoming) = patch.last_accessed_at else {
            return;
        };
        if self.last_accessed_at.is_some_and(|disk| disk >= incoming) {
            tracing::debug!(
                target: "session.store",
                session_id = %patch.id,
                disk_ts = ?self.last_accessed_at,
                patch_ts = %incoming,
                "dropped passive status patch's last_accessed_at as a no-op (disk value is at least as recent; status/idle_entered_at still applied)"
            );
            return;
        }
        self.last_accessed_at = Some(incoming);
    }

    /// Per-field-conditional splice: copy `post.X` onto `self.X` only when
    /// `pre.X != post.X`. Peer writes to fields the mutation did not touch
    /// survive even when the field is in the user-action set.
    /// `last_accessed_at` is monotone-max (no diff guard).
    /// `source_profile` is excluded; cross-profile moves bypass this path.
    /// Post-splice rules enforce the same cross-field invariants the
    /// per-mutation methods enforce (archive XOR favorite, touch unarchives)
    /// so concurrent peer writes cannot violate them.
    pub fn merge_user_action_diff(&mut self, pre: &Self, post: &Self) {
        debug_assert_eq!(
            pre.source_profile, post.source_profile,
            "apply_user_action must not change source_profile; cross-profile moves go through mutate_instance"
        );
        if pre.title != post.title {
            self.title = post.title.clone();
        }
        if pre.group_path != post.group_path {
            self.group_path = post.group_path.clone();
        }
        if pre.archived_at != post.archived_at {
            self.archived_at = post.archived_at;
        }
        if pre.favorited_at != post.favorited_at {
            self.favorited_at = post.favorited_at;
        }
        if pre.snoozed_until != post.snoozed_until {
            self.snoozed_until = post.snoozed_until;
        }
        if pre.pinned_at != post.pinned_at {
            self.pinned_at = post.pinned_at;
        }
        if pre.trashed_at != post.trashed_at {
            self.trashed_at = post.trashed_at;
        }
        if pre.pre_trash_project_path != post.pre_trash_project_path {
            self.pre_trash_project_path = post.pre_trash_project_path.clone();
        }
        if pre.unread != post.unread {
            self.unread = post.unread;
        }
        if pre.base_branch_override != post.base_branch_override {
            self.base_branch_override = post.base_branch_override.clone();
        }
        // Worktree workdir edit (move dir / rename branch) mutates these two;
        // both the TUI and the CLI can write them, so they go through the
        // same conditional-diff path as the triage fields. See #1723.
        if pre.project_path != post.project_path {
            self.project_path = post.project_path.clone();
        }
        if pre.worktree_info != post.worktree_info {
            self.worktree_info = post.worktree_info.clone();
        }
        if pre.status != post.status {
            self.status = post.status;
        }
        self.last_accessed_at = self.last_accessed_at.max(post.last_accessed_at);

        let archived_changed = pre.archived_at != post.archived_at;
        let favorited_changed = pre.favorited_at != post.favorited_at;
        let snoozed_changed = pre.snoozed_until != post.snoozed_until;
        let pinned_changed = pre.pinned_at != post.pinned_at;
        // Touch is an event invariant: any advance of last_accessed_at
        // (TUI-side or peer-side) dethrones a concurrent archive.
        let touched = self.last_accessed_at > pre.last_accessed_at;

        // archive(): archived=Some => favorited=None, snoozed=None, pinned=None
        if archived_changed && post.archived_at.is_some() {
            self.favorited_at = None;
            self.snoozed_until = None;
            self.pinned_at = None;
        }
        // favorite(): favorited=Some => archived=None, snoozed=None
        if favorited_changed && post.favorited_at.is_some() {
            self.archived_at = None;
            self.snoozed_until = None;
        }
        // snooze(): snoozed=Some => pinned=None (sink clears surface).
        if snoozed_changed && post.snoozed_until.is_some() {
            self.pinned_at = None;
        }
        // pin(): pinned=Some => archived=None, snoozed=None (surface clears sinks).
        if pinned_changed && post.pinned_at.is_some() {
            self.archived_at = None;
            self.snoozed_until = None;
        }
        // touch_last_accessed(): clears archived + snoozed + idle-dormant.
        // Does NOT clear favorite or pin (both are explicit user-surfacing
        // signals, not sink states). Mirrors touch_last_accessed() so the
        // wake-from-dormancy invariant holds on the concurrent-writer merge
        // path too, not just direct touches (#1689).
        if touched {
            self.archived_at = None;
            self.snoozed_until = None;
            self.idle_dormant_since = None;
        }
        // Final-state invariant: archive is the strongest dismiss and
        // wins over snooze. The per-mutation rules above clear other
        // flags on the change side, but the diff can also leave disk
        // archived (pre-existing) AND snoozed (added by post); without
        // this check the row would persist both and the web sidebar's
        // tier comparator (which assumes exactly one active triage
        // state) would render contradictory chips. See #1581.
        if self.archived_at.is_some() {
            self.snoozed_until = None;
        }
    }

    /// Mark the session archived. Archived sessions sink to the bottom of
    /// the Attention sort and render in italic+dim style, but remain
    /// visible. Auto-cleared by the attention-signal hook on Waiting/Error.
    ///
    /// Mutual exclusion with `favorite`, `snooze`, and `pin`: archiving
    /// clears `favorited_at`, `snoozed_until`, and `pinned_at`. Archive
    /// is the strongest dismiss; keeping any other triage flag on a row
    /// the user just sunk produces contradictory state, and the web
    /// sidebar's tier comparator already assumes the server enforces a
    /// single active triage state (see `sidebarSort.ts` in #1581).
    pub fn archive(&mut self) {
        self.archived_at = Some(Utc::now());
        self.favorited_at = None;
        self.snoozed_until = None;
        self.pinned_at = None;
    }

    pub fn unarchive(&mut self) {
        self.archived_at = None;
        self.idle_dormant_since = None;
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Soft-delete the session into the trash bucket. Stops the live
    /// session (handled by the caller: ACP `shutdown`, optional tmux kill)
    /// but keeps every durable artifact so `untrash` can bring it back
    /// intact. Intentionally additive: only `trashed_at` is set, the
    /// sibling triage flags (`archived_at`, `favorited_at`, `snoozed_until`,
    /// `pinned_at`) are left untouched so restore is faithful.
    /// `effective_bucket()` makes trash win regardless. Idempotent.
    pub fn trash(&mut self) {
        if self.trashed_at.is_none() {
            self.trashed_at = Some(Utc::now());
        }
    }

    /// Restore a trashed session back to its prior bucket (active or
    /// archived, depending on the preserved sibling flags). Idempotent.
    pub fn untrash(&mut self) {
        self.trashed_at = None;
    }

    pub fn is_trashed(&self) -> bool {
        self.trashed_at.is_some()
    }

    /// The mutually-exclusive lifecycle bucket a session renders in.
    /// Precedence is `Trashed > Archived > Active`: a trashed row never
    /// shows in active or archived views, and an archived row never shows
    /// in active views. Snooze/favorite/pin are orthogonal decorations
    /// within a bucket, not buckets of their own, so they are not consulted
    /// here. Use this instead of bare `!is_archived()` filters so trashed
    /// rows cannot leak into the active list.
    pub fn effective_bucket(&self) -> SessionBucket {
        if self.is_trashed() {
            SessionBucket::Trashed
        } else if self.is_archived() {
            SessionBucket::Archived
        } else {
            SessionBucket::Active
        }
    }

    /// Mark the session favorite. Sibling of `archive`, with opposite semantics.
    /// Pinning logic lives in `attention_session_key`: favorite is a
    /// within-tier pin (top of its respective category), not a cross-tier
    /// promoter. A favorited Running stays in the Running bucket but sorts
    /// above non-favorited Running peers.
    ///
    /// Mutual exclusion with the sink states: favoriting clears `archived_at`
    /// AND `snoozed_until`. Favorite's whole purpose is "surface this row";
    /// leaving either sink-state flag set would force the row to tier 99 and
    /// the favorite bias would be suppressed; user presses `f` and sees
    /// nothing change. The user's explicit rule: "marking as favorite
    /// unarchives," extended to snooze because snooze shares tier 99 and
    /// shares the burial outcome.
    pub fn favorite(&mut self) {
        self.favorited_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unfavorite(&mut self) {
        self.favorited_at = None;
    }

    pub fn is_favorited(&self) -> bool {
        self.favorited_at.is_some()
    }

    /// Read the agent-raised urgent flag from `attention.json`. Sourced
    /// on-demand from `/tmp/aoe-hooks-<euid>/{id}/attention.json` so it picks up
    /// changes the running agent makes (via the `attention-urgent` script)
    /// without an Instance state mutation. Suppressed for archived/snoozed
    /// rows so a sunk session can't claw its way back to the top.
    pub fn is_urgent(&self) -> bool {
        if self.is_archived() || self.is_snoozed() {
            return false;
        }
        crate::hooks::read_hook_urgent(&self.id)
    }

    /// Temporarily defer this session for `minutes`; sets `snoozed_until`
    /// to `Utc::now() + minutes`. Behaves like a timed archive: the row
    /// sinks to tier 99, renders italic+dim with a `z ` prefix, and shows
    /// remaining time in the age column. When the timestamp expires the
    /// row rejoins the active attention sort automatically (next render
    /// tick); no timer task needed. Resolution of `minutes` happens at
    /// snooze time, not render time, so changing the config default mid-
    /// snooze does NOT extend currently-sleeping rows.
    ///
    /// Clears `pinned_at` for the same reason archive does: snooze is a
    /// sink state, and a pinned-yet-snoozed row is contradictory. The
    /// existing favorite mutator is intentionally NOT touched here
    /// (favorite is the TUI within-tier signal, snoozed favorites keep
    /// their star when they wake; see field doc for `favorited_at`).
    pub fn snooze(&mut self, minutes: u32) {
        self.snoozed_until = Some(Utc::now() + chrono::Duration::minutes(minutes as i64));
        self.pinned_at = None;
    }

    pub fn unsnooze(&mut self) {
        self.snoozed_until = None;
    }

    /// True if the session carries the unread marker.
    pub fn is_unread(&self) -> bool {
        self.unread
    }

    /// Mark the session unread. Used both by the auto-mark on a finished turn
    /// (`Running -> Idle`) and the manual "Mark as unread" action; the single
    /// state means there is no kind to preserve. Idempotent.
    pub fn mark_unread(&mut self) {
        self.unread = true;
    }

    /// Clear the unread marker. Used whenever the user engages with the
    /// session (open/attach, live-send, click, dwell) and by the explicit
    /// "Mark as read" action. Idempotent.
    pub fn mark_read(&mut self) {
        self.unread = false;
    }

    /// Manual toggle (`U`): read -> unread; unread -> read.
    pub fn toggle_unread(&mut self) {
        self.unread = !self.unread;
    }

    /// True if `snoozed_until` is set AND in the future. Expired snoozes
    /// return false so the row naturally rejoins the main sort on the next
    /// render; the stale timestamp stays on disk until the next mutation
    /// rewrites the session (harmless; `snoozed_until` is always compared
    /// against `Utc::now()`).
    pub fn is_snoozed(&self) -> bool {
        self.snoozed_until.map(|t| t > Utc::now()).unwrap_or(false)
    }

    /// Combined "don't bother me" sink-state check: trashed, snoozed, or
    /// archived. Callers that walk sessions looking for something to land on
    /// (e.g. the `w`/jump-to-next-attention passes) use this instead of the
    /// three-call form so a row in any sink state is uniformly excluded.
    pub fn is_dismissed(&self) -> bool {
        self.is_trashed() || self.is_snoozed() || self.is_archived()
    }

    /// Remaining snooze duration as a `chrono::Duration`, or `None` if the
    /// session isn't snoozed (or the timestamp has already expired).
    pub fn snooze_remaining(&self) -> Option<chrono::Duration> {
        self.snoozed_until.and_then(|t| {
            let delta = t - Utc::now();
            if delta > chrono::Duration::zero() {
                Some(delta)
            } else {
                None
            }
        })
    }

    /// Mark this session pinned. Pin is a web-only surfacing primitive:
    /// pinned workspaces sort to the top of the web sidebar (across all
    /// sort modes), regardless of last-activity. Distinct from
    /// `favorited_at`, which drives the TUI Attention sort's within-tier
    /// pin and stays unchanged here (see #1581).
    ///
    /// Mutual exclusion with the sink states: pinning clears
    /// `archived_at` and `snoozed_until`. A pinned-yet-sunk row would
    /// contradict the entire point of pinning (surface this), so the
    /// sinks come off, identical to how `favorite()` handles it.
    pub fn pin(&mut self) {
        self.pinned_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unpin(&mut self) {
        self.pinned_at = None;
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned_at.is_some()
    }

    /// Time elapsed since this session most recently transitioned into
    /// `Idle`. `None` for non-Idle sessions, sessions with a missing
    /// timestamp (legacy state), or sessions whose `idle_entered_at` is in
    /// the future (clock skew). Negative deltas are clamped away rather than
    /// returned as `Duration` since `chrono::Duration::to_std` rejects them.
    pub fn idle_age(&self) -> Option<std::time::Duration> {
        if self.status != Status::Idle {
            return None;
        }
        let since = self.idle_entered_at?;
        (Utc::now() - since).to_std().ok()
    }

    /// Return the profile that should drive config resolution for this
    /// instance, falling back to the user's globally configured default
    /// when `source_profile` was never populated (e.g. legacy callers).
    pub fn effective_profile(&self) -> String {
        super::config::effective_profile(&self.source_profile)
    }

    /// Resolve the effective `environment` list for this session's profile,
    /// falling back to the global list when the profile has no override.
    fn profile_host_environment(&self) -> Vec<String> {
        let profile = self.effective_profile();
        super::profile_config::resolve_config_or_warn(&profile).environment
    }

    pub fn is_sub_session(&self) -> bool {
        self.parent_session_id.is_some()
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandbox_info.as_ref().is_some_and(|s| s.enabled)
    }

    /// The repo this session groups under: the worktree's main repo when
    /// present (so all branches of a repo group together), else the project
    /// path. Shared by sidebar project grouping and new-session prefill so
    /// the "which directory does this session belong to" rule lives in one
    /// place.
    pub fn repo_path(&self) -> &str {
        self.worktree_info
            .as_ref()
            .map(|w| w.main_repo_path.as_str())
            .unwrap_or(&self.project_path)
    }

    pub fn is_yolo_mode(&self) -> bool {
        self.yolo_mode
    }

    /// True when this session renders in the structured (ACP) view. The
    /// persisted `view` field exists in every build so non-serve writers
    /// round-trip it intact; rows damaged by pre-fix writers are healed on
    /// reload by the server's structured row repair path.
    pub fn is_structured(&self) -> bool {
        self.view == View::Structured
    }

    /// Whether this agent uses a session ID poller for live tracking.
    pub fn supports_session_poller(&self) -> bool {
        crate::agents::get_agent(&self.tool).is_some_and(|a| {
            !matches!(
                a.resume_strategy,
                crate::agents::ResumeStrategy::Unsupported
            )
        })
    }

    /// Acquire a pre-launch session ID for the agent.
    ///
    /// Returns `(session_id, is_existing)`. Consults `resume_intent` first:
    /// `Use(sid)` returns the user-pinned target; `Cleared` skips both the
    /// observed sid and retroactive capture (forces a fresh start, generating
    /// a Claude UUID if applicable); `Default` verifies the observed sid
    /// against live tool state via `capture_freshest_session_id` (so a
    /// post-`/clear` session id supersedes a stale stored one), falls back
    /// to retroactive capture when no sid is observed, then to a fresh
    /// Claude UUID.
    pub fn acquire_session_id(&mut self) -> (Option<String>, bool) {
        match &self.resume_intent {
            ResumeIntent::Use(sid) => {
                let sid = sid.clone();
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                let session_id = match self.tool.as_str() {
                    "claude" => Some(generate_claude_session_id()),
                    _ => None,
                };
                if let Some(ref id) = session_id {
                    self.agent_session_id = Some(id.clone());
                }
                return (session_id, false);
            }
            ResumeIntent::Fork { .. } => {
                // The child id was pre-generated and stored in
                // agent_session_id at creation. acquire returns it as the
                // session this instance owns; the actual fork flags
                // (--resume <parent> --fork-session --session-id <child>) are
                // emitted by apply_session_flags, which reads the parent off
                // the Fork intent. Report `false` (not an in-place resume): a
                // fork starts a new session.
                return (self.agent_session_id.clone(), false);
            }
            ResumeIntent::Default => {}
        }

        if let Some(stored) = self.agent_session_id.clone() {
            if let Some(fresh) = self.capture_freshest_session_id() {
                tracing::info!(
                    target: "session.store",
                    stale = %stored,
                    fresh = %fresh,
                    tool = %self.tool,
                    "Replacing stored session id with fresher live observation"
                );
                self.agent_session_id = Some(fresh.clone());
                return (Some(fresh), true);
            }
            // A stored Claude sid with no transcript on disk is not resumable:
            // Claude minted the UUID at first launch but nothing was ever
            // written (an empty thread killed before the first prompt), so
            // `--resume <sid>` is a guaranteed launch failure that lands the
            // session in the "resume failed for sid ...; preserved for explicit
            // retry" state. Launch it as a fresh pinned session instead
            // (`is_existing = false` -> `--session-id <sid>`), which succeeds
            // and keeps the id stable so a later first prompt stays continuous.
            // Claude is the only tool AoE pre-mints a UUID for (see the fresh
            // arm below), so no other agent reaches this branch with a
            // self-created empty-thread sid. Host-only: a sandboxed transcript
            // lives inside the container, which may not be up at acquire time.
            if self.tool == "claude"
                && !self.is_sandboxed()
                && super::capture::claude_host_transcript_confirmed_absent(
                    &self.project_path,
                    &stored,
                )
            {
                tracing::info!(
                    target: "session.store",
                    sid = %stored,
                    "stored Claude sid has no transcript on disk; launching fresh \
                     with --session-id instead of --resume to avoid a certain \
                     resume failure"
                );
                return (Some(stored), false);
            }
            return (Some(stored), true);
        }

        let tmux_exists = self.tmux_session().is_ok_and(|s| s.exists());
        if tmux_exists {
            if let Some(id) = self.try_retroactive_capture() {
                tracing::info!(target: "session.store",
                    "Retroactive capture found session ID for {}: {}",
                    self.tool,
                    id
                );
                self.agent_session_id = Some(id);
                return (self.agent_session_id.clone(), true);
            }
        }

        let session_id = match self.tool.as_str() {
            "claude" => Some(generate_claude_session_id()),
            "opencode" => None,
            _ => None,
        };

        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.agent_session_id = session_id.clone();
        }

        (session_id, false)
    }

    /// Full set of session IDs that retroactive capture must skip for THIS
    /// instance: the live tmux-discovered set plus any sids the
    /// resume-fallback cascade has explicitly cleared. Composed of
    /// `build_exclusion_set` (live tmux scan) and
    /// `self.retroactive_capture_excludes` (cascade memory) so the caller
    /// gets the complete picture in one call.
    fn retroactive_capture_exclusion_set(&self) -> HashSet<String> {
        super::capture::compose_exclusion(&self.id, &self.retroactive_capture_excludes)
    }

    pub(crate) fn try_retroactive_capture(&self) -> Option<String> {
        let result: Option<String> = match self.tool.as_str() {
            "claude" => {
                // Claude-only: extend the live-tmux exclusion with stopped,
                // archived, or pane-less peer sids read from sessions.json so
                // the mtime fallback skips peers whose jsonl outlived their
                // tmux session (#2355). Other tool arms call
                // `retroactive_capture_exclusion_set()` directly for the
                // live-only set.
                let exclusion = super::capture::compose_exclusion_with_stopped_peers(
                    &self.id,
                    &self.project_path,
                    &self.effective_profile(),
                    &self.retroactive_capture_excludes,
                );
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    capture_claude_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                        None,
                    )
                    .ok()
                } else {
                    capture_claude_session_id(&self.project_path, None, &exclusion).ok()
                }
            }
            "opencode" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_opencode_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                        None,
                    )
                    .ok()
                } else {
                    try_capture_opencode_session_id(&self.project_path, &exclusion, None).ok()
                }
            }
            "vibe" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_vibe_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_vibe_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "pi" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_pi_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_pi_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "codex" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_codex_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_codex_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "gemini" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_gemini_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_gemini_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "hermes" => {
                let exclusion = self.retroactive_capture_exclusion_set();
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_hermes_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    capture_hermes_session_id(&self.project_path, &exclusion).ok()
                }
            }
            "copilot" => {
                // Copilot stores sessions in a SQLite db. Host capture reads it
                // directly; sandbox resume is a follow-up (the container's db is
                // not read over `docker exec`), so a sandboxed Copilot session
                // simply starts fresh on restart.
                if self.is_sandboxed() {
                    None
                } else {
                    let exclusion = self.retroactive_capture_exclusion_set();
                    capture_copilot_session_id(&self.project_path, &exclusion).ok()
                }
            }
            _ => None,
        };
        result.and_then(validated_session_id)
    }

    /// Returns `Some(fresh)` when the live tool state shows a session id
    /// distinct from `self.agent_session_id`, otherwise `None`. Reuses
    /// the per-tool dispatch in `try_retroactive_capture` so the freshness
    /// contract (mtime, SQLite ordering, exclusion set, host/container)
    /// stays encapsulated in each tool's existing capture function.
    ///
    /// For Claude the authoritative per-instance sidecar
    /// (`/tmp/aoe-hooks-<euid>/<instance_id>/session_id`, written by the
    /// SessionStart / UserPromptSubmit hooks) is consulted first. It is keyed
    /// by instance id, so it can never name a peer instance's conversation,
    /// unlike the mtime disk scan, which picks the most-recent jsonl in the
    /// shared `~/.claude/projects/<encoded-cwd>/` dir and so can select a
    /// co-located peer's session when several AoE sessions share one cwd
    /// (#2344). The mtime scan is only used as a fallback when no fresh
    /// sidecar exists (e.g. an old session resumed after the 5-minute
    /// sidecar window), matching the ordering already used by
    /// `claude_poll_fn`. Sandboxed Claude is included: its `SessionStart`
    /// hook writes through the `/tmp/aoe-hooks/<id>` bind-mount onto the
    /// host path, so `read_hook_session_id` reads it the same way, and the
    /// mtime fallback below still routes through the container-aware branch
    /// of `try_retroactive_capture`.
    ///
    /// Two deliberate divergences from `claude_poll_fn`, both correct for the
    /// resume context: (1) an excluded sidecar id returns `None` here rather
    /// than falling through to the mtime scan, since falling through is what
    /// re-opens #2344; (2) this reader and `claude_poll_fn` read the same
    /// sidecar without a shared snapshot, so a hook rotation between the two
    /// reads can briefly surface different UUIDs, benign under the existing
    /// eventual-consistency capture model.
    pub(crate) fn capture_freshest_session_id(&self) -> Option<String> {
        if self.tool == "claude" {
            if let Some(authoritative) = crate::hooks::read_hook_session_id(&self.id) {
                if self.retroactive_capture_excludes.contains(&authoritative) {
                    return None;
                }
                return override_if_distinct(self.agent_session_id.as_deref(), authoritative);
            }
        }

        let live = self.try_retroactive_capture()?;
        override_if_distinct(self.agent_session_id.as_deref(), live)
    }

    fn apply_session_flags(&mut self, cmd: &mut String, context: &str) -> bool {
        if let ResumeIntent::Fork { from } = self.resume_intent.clone() {
            let child = self.agent_session_id.clone();
            if let Some(child_id) = child.as_deref() {
                let fork_part = build_fork_flags(&self.tool, &from, child_id);
                if !fork_part.is_empty() {
                    // Codex's fork is a subcommand and must sit right after the
                    // binary (before other flags), like its resume subcommand.
                    // Flag-shaped forks (claude, opencode) append.
                    let is_subcommand = matches!(
                        crate::agents::get_agent(&self.tool).map(|a| &a.fork_strategy),
                        Some(crate::agents::ForkStrategy::CodexFork)
                    );
                    splice_subcommand_or_append(cmd, &fork_part, is_subcommand);
                }
            }
            // A fork is a fresh session, not an in-place resume.
            return false;
        }
        let (mut session_id, is_existing) = self.acquire_session_id();
        // Sandboxed Copilot starts fresh: the session db lives inside the
        // container, so a host-captured or manually pinned sid would launch
        // `--session-id <id>` against a UUID that does not resolve there.
        // Capture is already host-only above; drop the sid to gate emission too.
        if self.tool == "copilot" && self.is_sandboxed() {
            session_id = None;
        }
        let emitted =
            append_resume_flags(&self.tool, session_id.as_deref(), is_existing, cmd, context);
        is_existing && emitted
    }

    pub fn has_custom_command(&self) -> bool {
        if !self.extra_args.is_empty() {
            return true;
        }
        self.has_command_override()
    }

    /// True only when the launch command differs from the agent's default
    /// binary (ignores extra_args). Use this for status-detection and
    /// restart guards where only a wrapper script matters.
    pub fn has_command_override(&self) -> bool {
        if self.command.is_empty() {
            return false;
        }
        crate::agents::get_agent(&self.tool)
            .map(|a| self.command != a.binary)
            .unwrap_or(true)
    }

    pub fn expects_shell(&self) -> bool {
        crate::tmux::utils::is_shell_command(self.get_tool_command())
    }

    pub fn get_tool_command(&self) -> &str {
        if self.command.is_empty() {
            crate::agents::get_agent(&self.tool)
                .map(|a| a.binary)
                .unwrap_or("bash")
        } else {
            &self.command
        }
    }

    /// The text searched for a user-selected `--agent NAME` flag: both the
    /// command override (where a custom command like `kiro-cli chat --agent x`
    /// may live) and the extra-args field (the usual place). Joined so a flag
    /// in either is found.
    fn selected_agent_args(&self) -> String {
        if self.command.is_empty() {
            self.extra_args.clone()
        } else if self.extra_args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.extra_args)
        }
    }

    /// Launch command including any agent `launch_subcommand` (e.g.
    /// `kiro-cli chat`). A user command override takes precedence verbatim and
    /// the subcommand is not applied to it. Used when assembling the launch
    /// command so subcommand-scoped flags (yolo, resume) parse correctly.
    fn get_launch_command(&self) -> String {
        if self.command.is_empty() {
            crate::agents::get_agent(&self.tool)
                .map(|a| a.launch_base_command())
                .unwrap_or_else(|| "bash".to_string())
        } else {
            self.command.clone()
        }
    }

    pub fn tmux_session(&self) -> Result<tmux::Session> {
        tmux::Session::new(&self.id, &self.title)
    }

    pub(crate) fn tmux_env_session_name(&self) -> Option<String> {
        tmux_env_session_name_for_instance_id(&self.id)
    }

    pub fn terminal_tmux_session(&self) -> Result<tmux::TerminalSession> {
        self.terminal_tmux_session_indexed(0)
    }

    /// Paired host terminal at `index`. Index 0 is the historical single
    /// terminal (the only one the TUI uses); index >= 1 are the additional
    /// web dashboard terminal tabs (#2437).
    pub fn terminal_tmux_session_indexed(&self, index: u32) -> Result<tmux::TerminalSession> {
        tmux::TerminalSession::new_indexed(&self.id, &self.title, index)
    }

    pub fn has_terminal(&self) -> bool {
        self.terminal_info
            .as_ref()
            .map(|t| t.created)
            .unwrap_or(false)
    }

    pub fn start_terminal(&mut self) -> Result<()> {
        self.start_terminal_with_size(None)
    }

    pub fn start_terminal_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_terminal_with_size_indexed(0, size)
    }

    pub fn start_terminal_with_size_indexed(
        &mut self,
        index: u32,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        let session = self.terminal_tmux_session_indexed(index)?;

        let is_new = !session.exists();
        if is_new {
            session.create_with_size(&self.project_path, None, size)?;
            // Apply all configured tmux options to terminal sessions too
            self.apply_terminal_tmux_options(index);
        }

        // The persisted `terminal_info` cache is the index-0 fast path the TUI
        // reads; additional terminals (index >= 1) are tracked by the web
        // dashboard and queried straight from tmux, like container terminals.
        if index == 0 {
            self.terminal_info = Some(TerminalInfo { created: true });
        }

        Ok(())
    }

    pub fn kill_terminal(&self) -> Result<()> {
        self.kill_terminal_indexed(0)
    }

    pub fn kill_terminal_indexed(&self, index: u32) -> Result<()> {
        let session = self.terminal_tmux_session_indexed(index)?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Kill the paired terminal tmux session if its pane is dead (shell
    /// exited while `remain-on-exit on` kept the session as a tombstone).
    /// Returns true if a kill happened so the caller knows to re-spawn.
    /// A missing session or a live pane both return Ok(false).
    pub fn kill_terminal_if_dead(&self) -> Result<bool> {
        self.kill_terminal_if_dead_indexed(0)
    }

    pub fn kill_terminal_if_dead_indexed(&self, index: u32) -> Result<bool> {
        let session = self.terminal_tmux_session_indexed(index)?;
        if session.exists() && session.is_pane_dead() {
            let _ = session.kill();
            return Ok(true);
        }
        Ok(false)
    }

    pub fn container_terminal_tmux_session(&self) -> Result<tmux::ContainerTerminalSession> {
        self.container_terminal_tmux_session_indexed(0)
    }

    pub fn container_terminal_tmux_session_indexed(
        &self,
        index: u32,
    ) -> Result<tmux::ContainerTerminalSession> {
        tmux::ContainerTerminalSession::new_indexed(&self.id, &self.title, index)
    }

    pub fn has_container_terminal(&self) -> bool {
        self.container_terminal_tmux_session()
            .map(|s| s.exists())
            .unwrap_or(false)
    }

    /// `exists()` alone is insufficient: a pane can exist while its agent
    /// has died. Used by recovery, status polling, and TUI reload.
    pub fn has_live_tmux_pane(&self) -> bool {
        self.tmux_env_session_name().is_some()
    }

    pub fn start_container_terminal_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_container_terminal_with_size_indexed(0, size)
    }

    pub fn start_container_terminal_with_size_indexed(
        &mut self,
        index: u32,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        if !self.is_sandboxed() {
            anyhow::bail!("Cannot create container terminal for non-sandboxed session");
        }

        let container = self.get_container_for_instance()?;
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;

        let env_info = build_docker_env_args(
            &self.source_profile,
            sandbox,
            std::path::Path::new(&self.project_path),
        );
        let env_part = if env_info.docker_args.is_empty() {
            String::new()
        } else {
            format!("{} ", env_info.docker_args)
        };

        // Get workspace path inside container (handles bare repo worktrees correctly)
        let container_workdir = self.container_workdir();

        let cmd = container.exec_command(
            Some(&format!("-w {} {}", container_workdir, env_part)),
            CONTAINER_TERMINAL_AUTODETECT_CMD,
        );

        // If there are secret env vars, prepend shell exports and use `exec`
        // so the outer shell (whose argv briefly contains the export values)
        // is replaced immediately, keeping secrets out of long-lived process argv.
        let session_cmd = if env_info.exports.is_empty() {
            cmd
        } else {
            let exports = env_info.exports.join("; ");
            format!("{}; exec {}", exports, cmd)
        };

        let session = self.container_terminal_tmux_session_indexed(index)?;
        let is_new = !session.exists();
        if is_new {
            session.create_with_size(&self.project_path, Some(&session_cmd), size)?;
            self.apply_container_terminal_tmux_options(index);
        }

        Ok(())
    }

    pub fn kill_container_terminal(&self) -> Result<()> {
        self.kill_container_terminal_indexed(0)
    }

    pub fn kill_container_terminal_indexed(&self, index: u32) -> Result<()> {
        let session = self.container_terminal_tmux_session_indexed(index)?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Container counterpart of [`Self::kill_terminal_if_dead`].
    pub fn kill_container_terminal_if_dead(&self) -> Result<bool> {
        self.kill_container_terminal_if_dead_indexed(0)
    }

    pub fn kill_container_terminal_if_dead_indexed(&self, index: u32) -> Result<bool> {
        let session = self.container_terminal_tmux_session_indexed(index)?;
        if session.exists() && session.is_pane_dead() {
            let _ = session.kill();
            return Ok(true);
        }
        Ok(false)
    }

    fn sandbox_display(&self) -> Option<crate::tmux::status_bar::SandboxDisplay> {
        self.sandbox_info.as_ref().and_then(|s| {
            if s.enabled {
                Some(crate::tmux::status_bar::SandboxDisplay {
                    container_name: s.container_name.clone(),
                })
            } else {
                None
            }
        })
    }

    /// Apply all configured tmux options to a session with the given name and title.
    fn apply_session_tmux_options(&self, session_name: &str, display_title: &str) {
        let branch = self
            .worktree_info
            .as_ref()
            .map(|w| w.branch.as_str())
            .or_else(|| self.workspace_info.as_ref().map(|w| w.branch.as_str()));
        let sandbox = self.sandbox_display();
        crate::tmux::status_bar::apply_all_tmux_options(
            session_name,
            display_title,
            branch,
            sandbox.as_ref(),
        );
    }

    fn apply_container_terminal_tmux_options(&self, index: u32) {
        let name =
            tmux::ContainerTerminalSession::generate_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (container)", self.title));
    }

    pub fn start(&mut self) -> Result<()> {
        self.start_with_size(None)
    }

    pub fn start_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_with_size_opts(size, false).map(|_| ())
    }

    /// Start the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
    pub fn start_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<LaunchSidOutcome> {
        // Validate before any shell-command construction in
        // `build_launch_command` (covers `status_hook_env_prefix` and
        // the sandbox docker_args interpolation). Runs before the
        // structured view short-circuit so a tampered id surfaces as `Err` for
        // structured view sessions too.
        crate::session::validate_instance_id(&self.id)
            .context("refusing to launch: AOE_INSTANCE_ID failed validation")?;

        // Acp-mode sessions are not backed by tmux. The structured view
        // worker supervisor spawns the ACP agent process directly;
        // calling start() on a structured view session is a no-op (status
        // updates flow through the ACP event channel, not tmux).
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }

        let session = self.tmux_session()?;

        if session.exists() {
            return Ok(LaunchSidOutcome::Skipped);
        }

        // Refresh peer-writable persisted fields (`agent_session_id`,
        // `resume_intent`) from disk before the launch decision. Closes the
        // status-poll lag window for both the read side
        // (`acquire_session_id`) and the write side (`persist_session_id`'s
        // CAS baseline). Covers resume-probe launches and explicit fresh
        // launches since both call this function.
        self.reconcile_from_disk();

        // Consume the one-shot override, if `start_with_resume_fallback` set
        // it, now that `reconcile_from_disk`'s `*self = disk` reload can no
        // longer clobber it. Forces this launch through the same
        // `ResumeIntent::Cleared` path a manual `aoe session set-session-id
        // ""` would take: no `--resume` flag, fresh sid, one-shot
        // auto-promote back to `Default` on disk after this launch persists.
        // See #2609.
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }

        self.reconcile_sidecar_into_disk();

        // CAS baseline for `persist_session_id`. `build_launch_command` ->
        // `apply_session_flags` -> `acquire_session_id` may mutate
        // `agent_session_id` (Claude UUID generation); capture before that.
        let expected_prior_sid = self.agent_session_id.clone();
        let expected_prior_intent = self.resume_intent.clone();

        let profile = self.effective_profile();
        let (cmd, is_existing) = self.build_launch_command(skip_on_launch, &profile)?;
        let launch_sid = if is_existing {
            Some(
                self.agent_session_id
                    .clone()
                    .expect("existing launch command carries agent_session_id"),
            )
        } else {
            None
        };

        tracing::debug!(target: "session.store",
            "container cmd: {}",
            cmd.as_ref().map_or("none".to_string(), |v| {
                super::environment::redact_env_values(v)
            })
        );

        if self.tool == "claude" {
            // Route through dir_guard so the session_id removal participates
            // in the same `*at`-anchored, mode-checked, owner-checked
            // discipline as every other hook I/O. Path-join + remove_file
            // would have bypassed base verification on the first launch
            // before any other hook code ran (#1844 follow-up).
            let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
        }

        session.create_with_size(&self.project_path, cmd.as_deref(), size)?;

        self.finalize_launch(
            session.name(),
            &profile,
            expected_prior_sid.as_deref(),
            expected_prior_intent,
        );

        Ok(match launch_sid {
            Some(sid) => LaunchSidOutcome::Existing { sid },
            None => LaunchSidOutcome::Fresh,
        })
    }

    /// Build the launch command string the way `start_with_size_opts` would,
    /// but without creating a tmux session. Returns `None` for structured view or
    /// other modes where there is no command to launch.
    ///
    /// Side effects mirror the start path: agent status hooks are installed,
    /// and (for sandboxed sessions) on_launch hooks run inside the container.
    fn build_launch_command(
        &mut self,
        skip_on_launch: bool,
        profile: &str,
    ) -> Result<(Option<String>, bool)> {
        let on_launch_hooks = self.resolve_on_launch_hooks(skip_on_launch, profile);

        let agent = crate::agents::get_agent(&self.tool)
            .or_else(|| crate::agents::get_agent(&self.detect_as));
        self.install_agent_status_hooks(agent);

        let (cmd, is_existing) = if self.is_sandboxed() {
            let container = self.get_container_for_instance()?;
            if let Some(ref hook_cmds) = on_launch_hooks {
                let hook_env = super::repo_config::lifecycle_env_vars(self);
                if let Some(ref sandbox) = self.sandbox_info {
                    let workdir = self.container_workdir();
                    if let Err(e) = super::repo_config::execute_hooks_in_container(
                        hook_cmds,
                        &sandbox.container_name,
                        &workdir,
                        &hook_env,
                    ) {
                        if e.chain().any(|c| {
                            c.downcast_ref::<super::repo_config::HookTimeout>()
                                .is_some()
                        }) {
                            return Err(e);
                        }
                        tracing::warn!(target: "session.store", "on_launch hook failed in container: {}", e);
                    }
                }
            }

            let launch_cmd = self.get_launch_command();
            let base_cmd = if self.extra_args.is_empty() {
                launch_cmd
            } else {
                format!("{} {}", launch_cmd, self.extra_args)
            };
            let mut tool_cmd = if self.is_yolo_mode() {
                if let Some(ref yolo) = agent.and_then(|a| a.yolo.as_ref()) {
                    match yolo {
                        crate::agents::YoloMode::CliFlag(flag) => {
                            format!("{} {}", base_cmd, flag)
                        }
                        crate::agents::YoloMode::EnvVar(..)
                        | crate::agents::YoloMode::AlwaysYolo => base_cmd,
                    }
                } else {
                    base_cmd
                }
            } else {
                base_cmd
            };
            if let Some(instruction) = self
                .sandbox_info
                .as_ref()
                .and_then(|s| s.custom_instruction.as_ref())
                .filter(|s| !s.is_empty())
            {
                if let Some(flag_template) = agent.and_then(|a| a.instruction_flag) {
                    let escaped = shell_escape(instruction);
                    let flag = flag_template.replace("{}", &escaped);
                    tool_cmd = format!("{} {}", tool_cmd, flag);
                }
            }

            let is_existing = self.apply_session_flags(&mut tool_cmd, "sandboxed");
            apply_agent_launch_env(&mut tool_cmd, agent);

            let sandbox = self
                .sandbox_info
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed instance"))?;
            let env_info = build_docker_env_args(
                &self.source_profile,
                sandbox,
                std::path::Path::new(&self.project_path),
            );
            let profile = self.effective_profile();
            let docker_args = format!(
                "{} -e AOE_PROFILE={} -e AOE_INSTANCE_ID={}",
                env_info.docker_args,
                shell_escape(&profile),
                shell_escape(&self.id)
            );
            let env_part = format!("{} ", docker_args);
            let wrapped =
                wrap_command_ignore_suspend(&container.exec_command(Some(&env_part), &tool_cmd));
            (
                Some(prepend_exports(&env_info.exports, wrapped)),
                is_existing,
            )
        } else {
            self.build_host_command(agent, &on_launch_hooks)?
        };

        Ok((cmd, is_existing))
    }

    /// Resolve on_launch hooks from the full config chain (global > profile > repo).
    ///
    /// Repo hooks go through trust verification; global/profile hooks are
    /// implicitly trusted. Returns `None` when skipped or no hooks are configured.
    pub(crate) fn resolve_on_launch_hooks(
        &self,
        skip_on_launch: bool,
        profile: &str,
    ) -> Option<Vec<String>> {
        if skip_on_launch {
            return None;
        }

        // Start with global+profile hooks as the base
        let mut resolved_on_launch = super::profile_config::resolve_config_or_warn(profile)
            .hooks
            .on_launch;

        // Check if repo has trusted hooks that override. Only the hooks surface
        // matters here; untrusted project MCP must not suppress trusted hooks.
        if let Ok(trust) = super::repo_config::check_repo_trust(Path::new(&self.project_path)) {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_launch.is_empty() {
                    resolved_on_launch = hooks.on_launch;
                }
            }
        }

        if resolved_on_launch.is_empty() {
            None
        } else {
            Some(resolved_on_launch)
        }
    }

    /// Install status-detection hooks for agents that support them.
    ///
    /// For sandboxed sessions hooks are installed via `build_container_config`,
    /// so this only acts on host sessions by writing to the user's home directory.
    /// Respects the `agent_status_hooks` config setting.
    fn install_agent_status_hooks(&self, agent: Option<&'static crate::agents::AgentDef>) {
        let profile = self.effective_profile();
        let config = super::profile_config::resolve_config_or_warn(&profile);
        if !config.session.agent_status_hooks {
            return;
        }
        if let Some(agent) = agent {
            if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
                let events = match crate::agents::resolved_sidecar_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                // Sidecar agents (settl TOML, hermes YAML, kiro per-agent JSON)
                // install into a host config file; sandbox install is handled by
                // build_container_config. host_only agents (settl) are never
                // sandboxed, so the gate is a no-op for them.
                if !self.is_sandboxed() {
                    if let Some(home) = dirs::home_dir() {
                        self.install_sidecar_host_hooks(sidecar, &home, &config.session, &events);
                    }
                }
            } else if let Some(hook_cfg) = agent.hook_config.as_ref() {
                let events = match crate::agents::resolved_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                if !self.is_sandboxed() {
                    match hook_cfg.format {
                        crate::agents::HookFormat::CodexJson => {
                            self.install_codex_host_hooks(&events)
                        }
                        crate::agents::HookFormat::JsonSettings => {
                            self.install_json_host_hooks(hook_cfg, &events)
                        }
                    }
                }
                // Sandboxed sessions install via build_container_config.
            }
        }
    }

    /// Install a sidecar agent's host hooks. For agents whose hooks are scoped
    /// to a user-selected named agent (`selected_agent_hooks`, e.g. Kiro), and
    /// when the user actually selected one and the merge setting is on, install
    /// into that agent's own config file and stop. Otherwise install into the
    /// agent's standalone config and run any `post_install_host` follow-up.
    fn install_sidecar_host_hooks(
        &self,
        sidecar: &'static crate::agents::SidecarHooks,
        home: &Path,
        session_cfg: &super::config::SessionConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) {
        if session_cfg.merge_hooks_into_selected_agent {
            if let Some(sel) = sidecar.selected_agent_hooks.as_ref() {
                if let Some(name) =
                    crate::agents::parse_selected_agent(&self.selected_agent_args(), sel.flag)
                {
                    // The selected agent is what the CLI loads; install AoE's
                    // hooks into its config (these CLIs have no global hooks) and
                    // skip the standalone-agent install + post_install_host. The
                    // agents directory is the parent of the standalone hooks
                    // agent's config (e.g. `.kiro/agents`); the resolver picks the
                    // right file within it by `name`.
                    let agents_dir = home.join(
                        Path::new(sidecar.host_config_subpath)
                            .parent()
                            .unwrap_or(Path::new(".")),
                    );
                    let path = (sel.resolve_config_file)(&agents_dir, &name);
                    match (sidecar.install)(&path, crate::hooks::HookInstallTarget::Host, events) {
                        Ok(()) => tracing::info!(target: "session.store",
                            "Installed AoE status hooks into {} agent '{}' at {}", self.tool, name, path.display()),
                        Err(e) => tracing::warn!(target: "session.store",
                            "Failed to install AoE hooks into {} agent '{}' at {}: {}", self.tool, name, path.display(), e),
                    }
                    return;
                }
            }
        }

        let config_path = home.join(sidecar.host_config_subpath);
        match (sidecar.install)(&config_path, crate::hooks::HookInstallTarget::Host, events) {
            Ok(()) => {
                tracing::info!(target: "session.store",
                    "Installed AoE status hooks for {} via standalone hooks agent", self.tool);
                if let Some(post_install) = sidecar.post_install_host {
                    post_install();
                }
            }
            Err(e) => tracing::warn!(target: "session.store",
                "Failed to install {} hooks: {}", self.tool, e),
        }
    }

    fn install_codex_host_hooks(&self, events: &[crate::agents::ResolvedHookEvent]) {
        match crate::hooks::codex_hooks_json_path_for_host_environment(
            &self.profile_host_environment(),
        ) {
            Ok(hooks_path) => {
                if let Err(e) = crate::hooks::install_hooks(
                    &hooks_path,
                    events,
                    crate::hooks::HookInstallTarget::Host,
                ) {
                    tracing::warn!(target: "session.store", "Failed to install codex hooks: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!(target: "session.store", "Failed to resolve codex hooks path: {}", e)
            }
        }
    }

    fn install_json_host_hooks(
        &self,
        hook_cfg: &crate::agents::AgentHookConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) {
        // Install hooks in the agent's host settings file, honoring a
        // config-dir override env var (e.g. CLAUDE_CONFIG_DIR) so hooks
        // land where the agent actually reads them.
        match crate::hooks::agent_settings_path_for_host_environment(
            hook_cfg,
            &self.profile_host_environment(),
        ) {
            Ok(settings_path) => {
                if let Err(e) = crate::hooks::install_hooks(
                    &settings_path,
                    events,
                    crate::hooks::HookInstallTarget::Host,
                ) {
                    tracing::warn!(target: "session.store", "Failed to install agent hooks: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!(target: "session.store", "Failed to resolve agent hooks path: {}", e)
            }
        }
    }

    /// Build the tmux command for a host (non-sandboxed) session.
    ///
    /// Runs on_launch hooks on the host, then constructs the command from either
    /// the agent's default binary or a user-supplied custom command, applying
    /// yolo mode, session flags, and the AOE_INSTANCE_ID env prefix.
    ///
    /// Returns `Err` only when an on_launch hook timed out under an active
    /// `HookTimeoutScope` (recovery path). Generic hook failures continue to
    /// be logged at `warn` and the cascade proceeds with whatever partial
    /// setup the hook produced, matching the historical behavior for
    /// non-recovery callers (`aoe add`, manual restart).
    fn build_host_command(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
        on_launch_hooks: &Option<Vec<String>>,
    ) -> Result<(Option<String>, bool)> {
        if let Some(ref hook_cmds) = on_launch_hooks {
            let hook_env = super::repo_config::lifecycle_env_vars(self);
            if let Err(e) = super::repo_config::execute_hooks(
                hook_cmds,
                Path::new(&self.project_path),
                &hook_env,
            ) {
                if e.chain().any(|c| {
                    c.downcast_ref::<super::repo_config::HookTimeout>()
                        .is_some()
                }) {
                    return Err(e);
                }
                tracing::warn!(target: "session.store", "on_launch hook failed: {}", e);
            }
        }

        let profile = self.effective_profile();
        let mut env_prefix = status_hook_env_prefix(&profile, &self.id, agent);

        // Profile-scoped host environment entries (KEY=value, KEY=$VAR,
        // KEY=$$literal, or bare KEY for passthrough). Sandboxed sessions
        // intentionally skip this injection because the entries are
        // host-side; sandbox users should configure `sandbox.environment`
        // for the in-container env list.
        let host_env = self.profile_host_environment();
        if !host_env.is_empty() {
            env_prefix = format!(
                "{}{}",
                super::environment::host_environment_prefix(&host_env),
                env_prefix
            );
        }

        if self.command.is_empty() {
            match crate::agents::get_agent(&self.tool) {
                Some(a) => {
                    let mut cmd = a.launch_base_command();
                    if !self.extra_args.is_empty() {
                        cmd = format!("{} {}", cmd, self.extra_args);
                    }
                    if self.is_yolo_mode() {
                        if let Some(ref yolo) = a.yolo {
                            apply_yolo_mode(&mut cmd, yolo, false);
                        }
                    }
                    let is_existing = self.apply_session_flags(&mut cmd, "host agent");
                    apply_agent_launch_env(&mut cmd, agent);
                    Ok((
                        Some(wrap_command_ignore_suspend(&format!(
                            "{}{}",
                            env_prefix, cmd
                        ))),
                        is_existing,
                    ))
                }
                None => Ok((None, false)),
            }
        } else {
            let mut cmd = self.command.clone();
            if !self.extra_args.is_empty() {
                cmd = format!("{} {}", cmd, self.extra_args);
            }
            if self.is_yolo_mode() {
                if let Some(yolo) = agent.and_then(|a| a.yolo.as_ref()) {
                    apply_yolo_mode(&mut cmd, yolo, false);
                }
            }
            let is_existing = self.apply_session_flags(&mut cmd, "host custom");
            apply_agent_launch_env(&mut cmd, agent);
            Ok((
                Some(wrap_command_ignore_suspend(&format!(
                    "{}{}",
                    env_prefix, cmd
                ))),
                is_existing,
            ))
        }
    }

    /// Post-launch setup: persist state, start pollers, and apply tmux options.
    fn finalize_launch(
        &mut self,
        session_name: &str,
        profile: &str,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
    ) {
        let outcome = self.persist_session_id(profile, expected_prior_sid, expected_prior_intent);

        // Skip outcomes leave AOE_CAPTURED_SESSION_ID untouched: this path
        // runs before any poller publish, so env is empty for fresh sessions.
        let publish_sid = matches!(outcome, SidPersistOutcome::Published);
        let captured_sid: Option<String> = if publish_sid {
            self.agent_session_id.clone()
        } else {
            None
        };

        let mut entries: Vec<(&str, &str, &str)> = vec![(
            session_name,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
            &self.id,
        )];
        if let Some(ref sid) = captured_sid {
            entries.push((
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                sid.as_str(),
            ));
        }
        if let Err(e) = crate::tmux::env::set_hidden_env_batch(&entries) {
            let keys: Vec<&str> = entries.iter().map(|(_, k, _)| *k).collect();
            tracing::warn!(target: "session.store",
                "Failed to set tmux env keys [{}] at finalize_launch: {}", keys.join(", "), e);
        }

        if publish_sid && self.agent_session_id.is_none() {
            if let Err(e) = crate::tmux::env::remove_hidden_env(
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            ) {
                tracing::warn!(target: "session.store",
                    instance = %self.id,
                    "Failed to clear captured sid in tmux env: {}", e);
            }
        }

        self.maybe_start_poller();

        self.status = Status::Starting;
        self.last_start_time = Some(std::time::Instant::now());

        // Apply status bar options in a background thread to avoid blocking
        // the TUI on the multiple tmux subprocess calls they require.
        let session_name = session_name.to_string();
        let instance_id_for_log = self.id.clone();
        let title = self.title.clone();
        let branch = self.worktree_info.as_ref().map(|w| w.branch.clone());
        let sandbox = self.sandbox_display();
        match std::thread::Builder::new()
            .name(format!("finalize-tmux-{}", instance_id_for_log))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::tmux::status_bar::apply_all_tmux_options(
                        &session_name,
                        &title,
                        branch.as_deref(),
                        sandbox.as_ref(),
                    );
                })) {
                    tracing::error!(target: "session.store", "finalize-tmux thread panicked: {:?}", panic);
                }
            }) {
            Ok(_handle) => {}
            Err(e) => {
                tracing::error!(target: "session.store",
                    session = %instance_id_for_log,
                    error = %e,
                    "Failed to spawn finalize-tmux thread"
                );
            }
        }
    }

    /// Atomic single-flock CAS+write of `agent_session_id` and (when
    /// `expected_prior_intent == Cleared`) the auto-promote to `Default`.
    /// A split would let a daemon crash freeze disk at `(new_sid, Cleared)`,
    /// which the next launch's `acquire_session_id` short-circuits
    /// on, orphaning the conversation just created with `new_sid`.
    ///
    /// On sid CAS skip: rollback both fields from disk.
    /// On intent CAS skip with sid match: persist sid, leave intent as
    /// peer wrote it, reload intent in memory.
    ///
    /// Returns `Published` if `self.agent_session_id` after return reflects
    /// disk (Applied: committed under flock; Skipped: reloaded). Returns
    /// `Skip` for invalid sid early-return, storage error, or `SidWrite::Failed`:
    /// memory is unchanged and the caller must not touch the tmux env.
    fn persist_session_id(
        &mut self,
        profile: &str,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
    ) -> SidPersistOutcome {
        let new_sid = self.agent_session_id.clone();

        if let Some(ref sid) = new_sid {
            if !is_valid_session_id(sid) {
                tracing::warn!(target: "session.store",
                    "Refusing to persist invalid session ID {:?} for {}",
                    sid,
                    self.id
                );
                return SidPersistOutcome::Skip;
            }
        }

        let storage = match super::storage::Storage::new(profile, self.resolve_file_watch()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to create storage for finalize-launch persist for {}: {}",
                    self.id,
                    e
                );
                return SidPersistOutcome::Skip;
            }
        };

        self.persist_session_id_with_storage(&storage, expected_prior_sid, expected_prior_intent)
    }

    fn persist_session_id_with_storage(
        &mut self,
        storage: &super::storage::Storage,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
    ) -> SidPersistOutcome {
        let new_sid = self.agent_session_id.clone();
        // Cleared, Fork, and Use are all one-shot launch directives: after the
        // launch they ran with completes, the session resumes its own id
        // normally, so the intent must auto-promote to Default. A fork left as
        // Fork on disk would re-fork the parent on the next restart
        // (double-fork). A Use pin left durable would let the drain never
        // adopt a post-launch capture (e.g. the resume-probe fallback minting
        // a fresh sid, or a later `/clear`), so a launched pin hands control
        // back to normal capture; a pin on a session that never launches keeps
        // Use and stays authoritative (see #2708).
        let promote_one_shot = matches!(
            expected_prior_intent,
            ResumeIntent::Cleared | ResumeIntent::Fork { .. } | ResumeIntent::Use(_)
        );

        let instance_id = self.id.clone();
        let new_sid_for_closure = new_sid.clone();
        let expected_prior_intent_for_closure = expected_prior_intent.clone();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            if inst.agent_session_id.as_deref() != expected_prior_sid {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = ?expected_prior_sid,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in finalize persist; skipping both writes"
                );
                return Ok(SidWrite::Skipped);
            }

            inst.agent_session_id = new_sid_for_closure.clone();
            inst.resume_probe_failed_sid = None;

            if promote_one_shot {
                if inst.resume_intent == expected_prior_intent_for_closure {
                    inst.resume_intent = ResumeIntent::Default;
                } else {
                    tracing::warn!(target: "session.store",
                        instance_id = %instance_id,
                        expected_intent = ?expected_prior_intent_for_closure,
                        disk_intent = ?inst.resume_intent,
                        "resume_intent CAS mismatch in finalize persist; sid persisted but intent left as peer wrote it"
                    );
                }
            }

            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(SidWrite::Applied) => {
                self.resume_probe_failed_sid = None;
                if promote_one_shot {
                    if let Ok(insts) = storage.load() {
                        if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                            self.resume_intent = disk.resume_intent;
                            self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        }
                    }
                }
                SidPersistOutcome::Published
            }
            Ok(SidWrite::Skipped) => match storage.load() {
                Ok(insts) => match insts.into_iter().find(|i| i.id == self.id) {
                    Some(disk) => {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                        SidPersistOutcome::Published
                    }
                    None => {
                        tracing::warn!(target: "session.store",
                            "Skipped reload found no row for {}; leaving memory and env untouched",
                            self.id
                        );
                        SidPersistOutcome::Skip
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Skipped reload failed for {}: {}; leaving memory and env untouched",
                        self.id, e
                    );
                    SidPersistOutcome::Skip
                }
            },
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Finalize persist found no instance row for {}",
                    self.id
                );
                SidPersistOutcome::Skip
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to persist session state for {}: {}",
                    self.id,
                    e
                );
                SidPersistOutcome::Skip
            }
        }
    }

    /// Persist an ambiguous resume-probe failure without clearing the durable
    /// resume sid. The CAS guard keeps peer sid changes authoritative.
    fn mark_resume_probe_failed(&mut self, profile: &str, sid: &str) -> SidWrite {
        let storage = match super::storage::Storage::new(profile, self.resolve_file_watch()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to create storage for resume-probe failure marker for {}: {}",
                    self.id,
                    e
                );
                return SidWrite::Failed;
            }
        };

        let instance_id = self.id.clone();
        let sid_for_closure = sid.to_string();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            if inst.agent_session_id.as_deref() != Some(sid_for_closure.as_str()) {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = %sid_for_closure,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in resume-probe failure marker; skipping write"
                );
                return Ok(SidWrite::Skipped);
            }

            inst.resume_probe_failed_sid = Some(sid_for_closure.clone());
            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped)) => {
                if let Ok(insts) = storage.load() {
                    if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                        self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                    }
                }
                write
            }
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Resume-probe failure marker found no instance row for {}",
                    self.id
                );
                SidWrite::Failed
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to mark resume-probe failure for {}: {}",
                    self.id,
                    e
                );
                SidWrite::Failed
            }
        }
    }
}

impl Instance {
    fn apply_terminal_tmux_options(&self, index: u32) {
        let name = tmux::TerminalSession::generate_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (terminal)", self.title));
    }

    pub fn get_container_for_instance(&mut self) -> Result<containers::DockerContainer> {
        let image = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot ensure container for non-sandboxed session"))?
            .image
            .clone();
        let container = DockerContainer::new(&self.id, &image);

        // Direct is_running()? / exists()? here rather than probe_running():
        // this function already returns Result, so `?` correctly propagates
        // a daemon-down transient to the caller as Err, letting them render
        // an actionable error rather than silently falling through to a
        // create attempt that would also fail. See #2596.
        if container.is_running()? {
            // Already up: not a come-up, so don't re-mint. Fill lazily only if a
            // fresh process attached to a running container with no values yet.
            self.ensure_before_start_env(false)?;
            container_config::refresh_agent_configs_for_profile(&self.effective_profile());
            self.backfill_container_workdir(&container);
            return Ok(container);
        }

        if container.exists()? {
            // Restart of a stopped container is a come-up: refresh so a
            // short-lived token is re-minted.
            self.ensure_before_start_env(true)?;
            container_config::refresh_agent_configs_for_profile(&self.effective_profile());
            container.start()?;
            self.backfill_container_workdir(&container);
            return Ok(container);
        }

        // Ensure image is available (always pulls to get latest)
        let runtime = containers::get_container_runtime();
        runtime.ensure_image(&image)?;

        // Mint before building the container config so the docker-run env also
        // carries the values (leak-safe via the inherit path in run_create).
        self.ensure_before_start_env(true)?;
        let config = self.build_container_config()?;
        let container_id = container.create(&config)?;

        if let Some(ref mut sandbox) = self.sandbox_info {
            sandbox.container_id = Some(container_id);
            // Pin the workdir to exactly what the container was built with, so
            // later `docker exec -w` can never drift from it (#2414).
            sandbox.container_workdir = Some(config.working_dir.clone());
        }

        Ok(container)
    }

    /// Backfill [`SandboxInfo::container_workdir`] from a live container for a
    /// session created before that field existed (or one whose value was
    /// cleared). Authoritative: the value is the container's own
    /// `Config.WorkingDir`, so a later host-side git-linkage break can't make
    /// [`Self::container_workdir`] drift from the path the container was built
    /// with (#2414). No-op once the value is set, when the session is not
    /// sandboxed, or when the runtime can't report it (the live fallback
    /// stands). Not persisted here; the next start re-backfills if needed.
    fn backfill_container_workdir(&mut self, container: &containers::DockerContainer) {
        let needs_backfill = self
            .sandbox_info
            .as_ref()
            .is_some_and(|s| s.container_workdir.is_none());
        if !needs_backfill {
            return;
        }
        if let Some(workdir) = container.working_dir() {
            if let Some(sandbox) = self.sandbox_info.as_mut() {
                sandbox.container_workdir = Some(workdir);
            }
        }
    }

    /// Get the container working directory for this instance.
    /// The working directory a `docker exec` into this session's sandbox must
    /// chdir to. Pinned to what the container was actually created with
    /// ([`SandboxInfo::container_workdir`]): set at create time from
    /// `ContainerConfig::working_dir` and backfilled from a live container for
    /// sessions that predate the field.
    ///
    /// Recomputing it live from `compute_volume_paths` is unsafe, which is what
    /// #2414 hit: that helper resolves the worktree's git linkage, and once the
    /// container is up that linkage can break on the host (e.g. the worktree's
    /// admin entry under `<main>/.git/worktrees/<name>` is pruned). When it
    /// can't resolve, `compute_volume_paths` silently collapses to
    /// `/workspace/<basename>` -- a path the container never mounted -- and the
    /// exec dies with `chdir to cwd ("/workspace/<name>") ... no such file or
    /// directory`. The live computation survives only as a fallback for a
    /// session whose container has not been created yet, where there is nothing
    /// to pin to.
    pub fn container_workdir(&self) -> String {
        if let Some(pinned) = self
            .sandbox_info
            .as_ref()
            .and_then(|s| s.container_workdir.clone())
        {
            return pinned;
        }
        container_config::compute_volume_paths(Path::new(&self.project_path), &self.project_path)
            .map(|(_, wd)| wd)
            .unwrap_or_else(|_| "/workspace".to_string())
    }

    fn build_container_config(&self) -> Result<crate::containers::ContainerConfig> {
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;
        // Resolve the user-selected agent (e.g. Kiro `--agent NAME`) so the
        // sandbox installs status hooks into that agent's config, matching the
        // host path. Gated by the same setting; only applies to agents that
        // declare selected_agent_hooks.
        let merge_selected =
            super::profile_config::resolve_config_or_warn(&self.effective_profile())
                .session
                .merge_hooks_into_selected_agent;
        let selected_agent = if merge_selected {
            // Mirror the host path's agent resolution (a custom wrapper detected
            // as kiro carries kiro's sidecar via detect_as), and the sandbox's
            // own `resolve_active_agent`, which also falls back to detect_as.
            crate::agents::get_agent(&self.tool)
                .or_else(|| crate::agents::get_agent(&self.detect_as))
                .and_then(|a| a.sidecar_hooks.as_ref())
                .and_then(|s| s.selected_agent_hooks.as_ref())
                .and_then(|sel| {
                    crate::agents::parse_selected_agent(&self.selected_agent_args(), sel.flag)
                })
        } else {
            None
        };
        container_config::build_container_config(
            &self.project_path,
            sandbox,
            container_config::ContainerAgentSelection::new(&self.tool, Some(&self.detect_as))
                .with_selected_agent(selected_agent.as_deref()),
            self.is_yolo_mode(),
            &self.id,
            self.workspace_info.as_ref(),
            &self.source_profile,
        )
    }

    /// Run `host_hooks.before_start` on the host and stash the resulting
    /// `KEY=VALUE` pairs on `sandbox_info.before_start_env`, from where
    /// [`super::environment::collect_environment`] injects them into the
    /// container environment on every surface (docker run, the tmux `docker
    /// exec` launch, and the structured-view worker).
    ///
    /// `force` re-mints unconditionally (a container come-up); when false the
    /// hooks run only if no values are stashed yet, so attaching to an
    /// already-running container backfills without re-minting on every relaunch.
    /// A hook failure is propagated so the container does not come up without
    /// the values the agent depends on. Hooks are resolved from profile/global
    /// config only, never from the repo.
    fn ensure_before_start_env(&mut self, force: bool) -> Result<()> {
        if self.sandbox_info.is_none() {
            return Ok(());
        }
        let commands = super::repo_config::resolve_before_start_hooks(&self.source_profile);
        if commands.is_empty() {
            if let Some(sb) = self.sandbox_info.as_mut() {
                sb.before_start_env.clear();
            }
            return Ok(());
        }
        let already_minted = self
            .sandbox_info
            .as_ref()
            .is_some_and(|s| !s.before_start_env.is_empty());
        if !force && already_minted {
            return Ok(());
        }

        let hook_env = super::repo_config::lifecycle_env_vars(self);
        let project_path = PathBuf::from(&self.project_path);
        // Feed the session's sandbox env into the hook so it can read a
        // per-session value (e.g. `$TEST_VAR`) to scope what it mints.
        // Repo-contributed env is filtered out so an untrusted repo can't
        // influence the host hook's environment.
        let session_env = self
            .sandbox_info
            .as_ref()
            .map(|sb| {
                super::environment::session_host_env_pairs(&self.source_profile, &project_path, sb)
            })
            .unwrap_or_default();
        let minted = super::repo_config::run_before_start_hooks(
            &commands,
            &project_path,
            &hook_env,
            &session_env,
        )?;
        if let Some(sb) = self.sandbox_info.as_mut() {
            sb.before_start_env = minted;
        }
        Ok(())
    }

    pub fn maybe_start_poller(&mut self) {
        if !self.supports_session_poller() {
            return;
        }
        let tool = self.tool.as_str();

        let tmux_session_name = self
            .tmux_env_session_name()
            .or_else(|| self.tmux_session().ok().map(|s| s.name().to_string()))
            .unwrap_or_default();
        let mut poller = SessionPoller::new(tmux_session_name.clone());
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone();
        // Snapshot per-instance excludes at poller-spawn time. Explicit sid
        // invalidation inserts into `retroactive_capture_excludes` before any
        // fresh poller starts, so the first immediate poll won't re-import the
        // invalidated sid.
        let extra_excludes = self.retroactive_capture_excludes.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = match tool {
            "claude" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(claude_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        initial_known.clone(),
                        instance_id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(claude_poll_fn(
                        self.project_path.clone(),
                        initial_known.clone(),
                        instance_id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "opencode" => {
                let launch_time_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0);
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(opencode_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        launch_time_ms,
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(opencode_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        launch_time_ms,
                        extra_excludes.clone(),
                    ))
                }
            }
            "vibe" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(vibe_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(vibe_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "pi" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(pi_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(pi_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "codex" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(codex_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(codex_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "gemini" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(gemini_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(gemini_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "hermes" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(hermes_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes,
                    ))
                } else {
                    Box::new(hermes_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes,
                    ))
                }
            }
            "copilot" => {
                // Host-only: the Copilot session-store SQLite db is read
                // directly on the host. Sandboxed sessions have no poller, so
                // their session id is never captured and they start fresh on
                // restart (sandbox resume is a follow-up).
                if self.is_sandboxed() {
                    return;
                }
                Box::new(copilot_poll_fn(
                    self.project_path.clone(),
                    self.id.clone(),
                    extra_excludes,
                ))
            }
            _ => return,
        };

        let cb_instance_id = self.id.clone();

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
            tracing::info!(target: "session.store", "Session ID changed for {}: {}", cb_instance_id, new_id);
            if let Some(tmux_name) = tmux_env_session_name_for_instance_id(&cb_instance_id) {
                publish_session_to_tmux_env(&tmux_name, &cb_instance_id, new_id);
            }
        });

        if poller.start(instance_id.clone(), poll_fn, on_change, initial_known) {
            self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
        } else {
            tracing::warn!(target: "session.store",
                "Failed to start session poller for instance {}, poller will not be stored",
                instance_id
            );
        }
    }

    fn stop_poller(&self) {
        if let Some(ref poller_arc) = self.session_id_poller {
            match poller_arc.lock() {
                Ok(mut poller) => poller.stop(),
                Err(e) => e.into_inner().stop(),
            }
        }
    }

    pub fn restart_with_size(&mut self, size: Option<(u16, u16)>) -> Result<StartOutcome> {
        self.restart_with_size_opts(size, false)
    }

    /// Tear down the current tmux session cleanly so a fresh
    /// `start_with_size_opts` can recreate it.
    ///
    /// `remain-on-exit on` keeps the tmux session alive after the agent
    /// process exits, leaving a frozen pane. The plain kill-session +
    /// new-session flow can race against the session cache
    /// (kill_process_tree on a defunct pid stalls on macOS, and the
    /// subsequent kill can run while start's exists() check still sees the
    /// cached entry), leaving the dead pane in place. Respawning the pane
    /// into a shell first puts it back in a live state so the kill path
    /// proceeds cleanly. The kill below then sees a live pane and tears it
    /// down. Caller is responsible for the subsequent
    /// `start_with_size_opts` to recreate the session with the agent
    /// command.
    pub(crate) fn kill_clean(&self) -> Result<()> {
        let session = self.tmux_session()?;
        if !session.exists() {
            return Ok(());
        }
        if session.is_pane_dead() {
            tracing::info!(target: "session.store",
                "restart: pane dead for session {} (remain-on-exit), \
                 respawning shell before recreate",
                session.name()
            );
            let shell = super::environment::user_shell();
            if let Err(e) = session.respawn_dead_pane(&self.project_path, Some(&shell)) {
                tracing::warn!(target: "session.store",
                    "respawn_dead_pane failed for {}: {}; falling back to kill+start",
                    session.name(),
                    e
                );
            }
        }
        session.kill()?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }

    /// Restart the session, optionally skipping on_launch hooks (e.g. when
    /// they already ran in the background creation poller). Honors
    /// `SessionConfig::auto_resume_on_restart`; this is the explicit
    /// user-initiated restart/reattach path (`e`, `Enter`, CLI `session
    /// restart`, startup recovery). See #2609.
    pub fn restart_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<StartOutcome> {
        self.restart_with_resume_policy(
            size,
            skip_on_launch,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
        )
    }

    /// Shared restart cascade behind `restart_with_size_opts`. Broken out so
    /// `ensure_pane_ready_with_size`'s dead-pane respawn (Send Message / Live
    /// Send) can pass `ResumeAttemptPolicy::Allow`, keeping those surfaces
    /// unaffected by `auto_resume_on_restart`. See #2609.
    fn restart_with_resume_policy(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.stop_poller();
        self.session_id_poller = None;
        self.kill_clean()?;
        self.start_with_resume_fallback(size, skip_on_launch, resume_policy)
    }

    /// Settle-based pane probe used by the resume-fallback cascade.
    ///
    /// Returns `Dead` immediately if the pane dies or the session evaporates
    /// during the probe window. Returns `Alive` only after the pane has been
    /// off the boot shell for `RESUME_PROBE_POST_SHELL_GRACE` consecutive
    /// time (handles agents whose boot wrapper sits before the agent
    /// crashes on a bad sid), or charitably on full timeout for slow-start
    /// agents. `pane_dead` is the unambiguous signal we trust to fire the
    /// cascade.
    ///
    /// For instances using a shell-wrapper command (`/bin/sh -c '...'`,
    /// agent-override scripts), `is_pane_running_shell` stays true for the
    /// entire probe and the post-shell grace shortcut never fires. Such
    /// instances rely exclusively on `pane_dead`: if the wrapper exits
    /// when the agent crashes, the cascade fires correctly; if the wrapper
    /// holds the pane open past the agent crash (e.g., trailing `sleep`),
    /// the cascade misses it. Pathological shape; not worth special-casing.
    ///
    /// Latency consequence: shell-wrapper instances therefore burn the full
    /// `RESUME_PROBE_MAX` on every healthy resume. Real agents settle in
    /// ~`RESUME_PROBE_POST_SHELL_GRACE`.
    fn probe_settle(
        &self,
        max: std::time::Duration,
        poll: std::time::Duration,
    ) -> Result<ProbeResult> {
        let session = self.tmux_session()?;
        let deadline = std::time::Instant::now() + max;
        let mut first_post_shell: Option<std::time::Instant> = None;
        loop {
            if !session.exists() {
                return Ok(ProbeResult::Dead);
            }
            if session.is_pane_dead() {
                return Ok(ProbeResult::Dead);
            }
            let now = std::time::Instant::now();
            if !session.is_pane_running_shell() {
                let started = *first_post_shell.get_or_insert(now);
                if now.duration_since(started) >= RESUME_PROBE_POST_SHELL_GRACE {
                    return Ok(ProbeResult::Alive);
                }
            } else {
                first_post_shell = None;
            }
            if now >= deadline {
                return Ok(ProbeResult::Alive);
            }
            std::thread::sleep(poll);
        }
    }

    /// Start the session with a one-shot resume fallback.
    ///
    /// Cascade:
    ///   1. If a valid `agent_session_id` is set and the agent supports
    ///      resume, attempt the start (which appends `--resume <sid>` or
    ///      equivalent). Probe the pane via `probe_settle`.
    ///   2. If the pane went dead within the probe window, stop the poller,
    ///      tear down the dead tmux session, preserve the sid, persist a
    ///      `resume_probe_failed_sid` loop-breaker, and return
    ///      `StartOutcome::ResumeFailed`. A dead pane is not proof that the
    ///      sid is invalid, so this path must not clear it or launch fresh.
    ///
    /// `resume_policy` gates step 1: `HonorAutoResumeSetting` additionally
    /// requires `SessionConfig::auto_resume_on_restart`; `Allow` always
    /// permits an attempt (subject to `should_attempt_resume`). Independent
    /// of policy, a sid that already equals `resume_probe_failed_sid` from a
    /// prior call never re-attempts resume: it returns
    /// `StartOutcome::FreshAfterFailedResume` instead of repeating the same
    /// doomed probe. See #2609.
    ///
    /// Latency: only fires the probe when `--resume <sid>` is being passed
    /// to a freshly-created tmux session. Healthy resumes on real agents
    /// pay `RESUME_PROBE_POST_SHELL_GRACE` (~2s) once on cold start;
    /// warm sessions and non-resume launches pay nothing. Shell-wrapper
    /// command overrides pay the full `RESUME_PROBE_MAX` (~3s) on every
    /// healthy resume because `is_pane_running_shell` never clears for
    /// them; see `probe_settle`. When the failure path fires, add
    /// `kill_clean` (~100ms macOS grace) before returning.
    ///
    /// Acp-mode sessions short-circuit (no tmux pane to probe).
    /// `StartOutcome::Fresh` is honest there: structured view's resume concept lives
    /// in `acp_session_id` and is handled by the ACP supervisor, not
    /// by this cascade.
    pub(crate) fn start_with_resume_fallback(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        // Clear `Status::Error` on entry so a successful relaunch from any
        // restart surface (REST `ensure_session`, TUI Enter/restart, CLI
        // `aoe session restart [id|--all]`, structured view-mode short-circuit)
        // does not leave a stale error chip up. REST `ensure_session`
        // re-asserts `status=Starting`, `last_error=None` pre-call as
        // defense in depth.
        //
        // `last_error_check` is cleared alongside `last_error` to mirror
        // how the field is otherwise managed: `update_status` writes both
        // together when transitioning into Error (see the write sites
        // gated on the 30s rate-limit). The clear is functionally inert
        // today because the only reader is gated on `status == Error` and
        // we just left Error, but the symmetry is intentional defense
        // against a future read site that drops that gate.
        if self.status == Status::Error {
            self.status = Status::Idle;
            self.last_error = None;
            self.last_error_check = None;
        }

        if self.is_structured() {
            let _ = self.start_with_size_opts(size, skip_on_launch)?;
            return Ok(StartOutcome::Fresh);
        }

        // Defense in depth: every current caller runs `kill_clean()` (or
        // its equivalent) first, so this is normally false. It can still
        // be true if `kill_clean` raced the macOS tmux session cache
        // (see `Instance::kill_clean` doc): in that case
        // `start_with_size_opts` no-ops, the probe would have nothing to
        // detect, and reporting `Fresh` is the least-wrong outcome
        // (returning `Resumed` would mean lying about a `--resume <sid>`
        // that was never passed). The debug_assert surfaces the protocol
        // violation in dev/test if a future caller forgets to tear down;
        // the tracing::warn! mirrors it in release so the race is visible
        // in `aoe logs` for diagnosis. The branch on `attempting_resume`
        // separates the dangerous case (sid was passed but no probe ran,
        // pane could be left frozen) from the benign one (no resume was
        // attempted, the race is just kill_clean cache staleness).
        let pane_was_preexisting = self.tmux_session().is_ok_and(|s| s.exists());

        // Decide BEFORE launching whether this call may pass `--resume`, since
        // that flag is baked into the command by `acquire_session_id` inside
        // `start_with_size_opts`, not by anything read afterward. Only
        // intervenes on the ambient auto-resume path (`ResumeIntent::Default`):
        // an explicit one-shot `Use`/`Fork`/`Cleared` intent (e.g. just set via
        // `aoe session set-session-id`) is a deliberate user action and must
        // not be silently overridden by the restart-time policy or
        // loop-breaker. When it fires, `force_fresh_next_launch` routes the
        // launch through the same `ResumeIntent::Cleared` path a manual
        // `set-session-id ""` would take. See #2609.
        let mut skipped_failed_resume_sid: Option<String> = None;
        if self.resume_intent == ResumeIntent::Default {
            if let Some(sid) = self.agent_session_id.clone() {
                let resume_allowed_by_policy = match resume_policy {
                    ResumeAttemptPolicy::Allow => true,
                    ResumeAttemptPolicy::HonorAutoResumeSetting => {
                        super::profile_config::resolve_config_or_warn(&self.effective_profile())
                            .session
                            .auto_resume_on_restart
                    }
                };
                let resume_capable = should_attempt_resume(Some(&sid), &self.tool);
                let probe_already_failed = self.resume_probe_failed_sid.as_deref() == Some(&sid);
                if resume_capable && probe_already_failed {
                    // Loop-breaker: a sid that already failed a probe is never
                    // retried automatically, regardless of policy, mirroring
                    // the check `is_recovery_candidate` already applies to
                    // the passive startup sweep. Without it, `e`/`Enter`
                    // retries the identical doomed sid forever.
                    skipped_failed_resume_sid = Some(sid);
                    self.force_fresh_next_launch = true;
                } else if resume_capable && !resume_allowed_by_policy {
                    self.force_fresh_next_launch = true;
                }
            }
        }

        let outcome = self.start_with_size_opts(size, skip_on_launch)?;

        // Gated on `LaunchSidOutcome::Existing` so fresh launches (Cleared,
        // no observed sid + Claude UUID generation) skip the probe and
        // honestly report `Fresh`. Without this gate, every Claude launch
        // would probe (~2s) and return `Resumed` because acquire always
        // assigns a UUID, even when no `--resume` was passed. When the
        // pre-launch decision above forced `Cleared`, `outcome` is already
        // `LaunchSidOutcome::Fresh` here, so this naturally yields `None`.
        let attempted_sid = match &outcome {
            LaunchSidOutcome::Existing { sid } if should_attempt_resume(Some(sid), &self.tool) => {
                Some(sid.clone())
            }
            _ => None,
        };
        let attempting_resume = attempted_sid.is_some();

        if pane_was_preexisting {
            if attempting_resume {
                tracing::warn!(
                    target: "session.store",
                    instance_id = %self.id,
                    "start_with_resume_fallback: tmux session still exists on \
                     entry with attempting_resume=true; cascade skipped, \
                     returning Fresh. --resume <sid> was passed to \
                     start_with_size_opts but no probe ran; if the agent \
                     crashes inside the pane, it will be left frozen.",
                );
            } else {
                tracing::warn!(
                    target: "session.store",
                    instance_id = %self.id,
                    "start_with_resume_fallback: tmux session still exists on \
                     entry (no resume attempted); cascade skipped, returning \
                     Fresh. Likely a kill_clean race or caller protocol violation.",
                );
            }
        }
        debug_assert!(
            !pane_was_preexisting,
            "start_with_resume_fallback callers must kill_clean() first; \
             tmux session for {} still exists on entry",
            self.id
        );

        // Defensive `|| pane_was_preexisting`: covers the TOCTOU window
        // where a peer killed the pane between the snapshot above and
        // `start_with_size_opts`'s internal `session.exists()` check, in
        // which case `outcome` could be `Existing` despite the snapshot.
        if !attempting_resume || pane_was_preexisting {
            return Ok(match skipped_failed_resume_sid {
                Some(sid) => StartOutcome::FreshAfterFailedResume { sid },
                None => StartOutcome::Fresh,
            });
        }

        // Tier-1 settle probe. On Err (rare: only when `tmux_session()`
        // fails), tear down the Tier-1 poller spawned by the
        // start_with_size_opts above before propagating, so a transient
        // tmux failure cannot leak a poller thread onto a presumed-broken
        // pane.
        let probe = match self.probe_settle(RESUME_PROBE_MAX, RESUME_PROBE_POLL) {
            Ok(p) => p,
            Err(e) => {
                self.stop_poller();
                self.session_id_poller = None;
                return Err(e);
            }
        };
        match probe {
            ProbeResult::Alive => return Ok(StartOutcome::Resumed),
            ProbeResult::Dead => {}
        }

        let stale_sid = attempted_sid.expect("attempting_resume guarantees launch sid is Some");
        let profile = self.effective_profile();
        tracing::warn!(
            target: "session.store",
            "start: resume with sid {} for session {} crashed pane within probe; \
             preserving sid and marking resume failure",
            stale_sid,
            self.id,
        );

        self.stop_poller();
        self.session_id_poller = None;
        self.resume_probe_failed_sid = Some(stale_sid.clone());
        match self.mark_resume_probe_failed(&profile, &stale_sid) {
            SidWrite::Applied | SidWrite::Skipped => {}
            SidWrite::Failed => {
                anyhow::bail!(
                    "resume probe failed for sid {} for {}, but marker could not be persisted",
                    stale_sid,
                    self.id,
                );
            }
        }
        self.kill_clean()
            .with_context(|| format!("kill_clean before resume fallback for {}", self.id))?;
        self.status = Status::Error;
        self.last_error = Some(format!(
            "resume failed for sid {}; preserved for explicit retry",
            stale_sid
        ));
        self.last_error_check = Some(std::time::Instant::now());

        Ok(StartOutcome::ResumeFailed { sid: stale_sid })
    }

    /// Smart-send precondition: bring this session's tmux pane to a state
    /// where `send_keys_with_delay` is safe.
    ///
    /// Without this, a send to a dead pane silently writes keystrokes to a
    /// corpse with no agent to respond, and the user sees no error.
    ///
    /// Handles three states the caller would otherwise hit:
    /// - Tmux session missing: start from scratch via `start_with_size`.
    /// - Pane dead (`#{pane_dead}=1`): reuse the restart path (same path
    ///   E/F5 uses; well-tested).
    /// - Already alive: no-op.
    ///
    /// Bails on Creating/Deleting (transient lifecycle states) and on
    /// structured view-mode sessions (no backing tmux pane).
    ///
    /// On `Started` / `Respawned`, polls briefly so keystrokes don't race the
    /// agent's startup splash. Best-effort: returns after the timeout even if
    /// the pane is still settling.
    ///
    /// Latency: `AlreadyAlive` is ~tmux RTT. The `Respawned` path routes
    /// through `restart_with_size` -> `start_with_resume_fallback`, which
    /// on a dead resume-eligible pane can block for the resume probe window
    /// (~3s; see `start_with_resume_fallback` for the breakdown) plus up to
    /// 3s of `wait_for_pane_ready` polling.
    /// Smart-send, TUI Enter, and `aoe send` callers should size timeouts
    /// and spinner copy accordingly.
    ///
    /// Note: callers that mutate a clone (e.g. inside `spawn_blocking`) must
    /// sync the post-start state (`status`, `agent_session_id`,
    /// `last_start_time`, `last_error`) back onto the in-memory entry, since
    /// `finalize_launch` writes those fields and they would otherwise be
    /// dropped with the clone. See `apply_post_restart_sync`.
    pub fn ensure_pane_ready(&mut self) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        self.ensure_pane_ready_with_size(None)
    }

    /// Like [`ensure_pane_ready`](Self::ensure_pane_ready), but seeds a
    /// freshly created or respawned pane at `size` (cols, rows) instead of
    /// letting tmux fall back to its 80x24 default.
    ///
    /// Live-send entry passes the visible preview-pane size here so the agent
    /// boots at the width it will be shown at. Without it the agent boots
    /// narrow (80 cols) and depends on a single post-boot `resize-window`
    /// SIGWINCH to grow into the live area. That SIGWINCH races the agent's
    /// startup: if it lands before the agent installs its resize handler the
    /// reflow is lost, and because the per-frame resize loop is deduped on the
    /// (already-correct) tmux window size, nothing re-issues it. The pane then
    /// stays pinned at ~80 cols (≈50% of a wide live area) until live mode is
    /// exited and re-entered. Booting at the right size sidesteps the race.
    ///
    /// `None` keeps tmux's default for callers with no target geometry.
    pub fn ensure_pane_ready_with_size(
        &mut self,
        size: Option<(u16, u16)>,
    ) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        if matches!(self.status, Status::Creating | Status::Deleting) {
            return Err(EnsureReadyError::Transient(self.status));
        }
        if self.is_structured() {
            return Err(EnsureReadyError::StructuredView);
        }
        let session = self.tmux_session().map_err(EnsureReadyError::Tmux)?;
        if !session.exists() {
            // Route fresh starts through the resume probe so a sid loaded
            // from disk that crashes the agent on launch is detected and
            // preserved with a loop-breaker instead of being retried
            // automatically. Always `Allow`: Send Message and Live Send must
            // keep trying to preserve agent context regardless of
            // `auto_resume_on_restart`, which only scopes explicit
            // restart/reattach. See #2609.
            let outcome = self
                .start_with_resume_fallback(size, false, ResumeAttemptPolicy::Allow)
                .map_err(EnsureReadyError::Tmux)?;
            match outcome {
                StartOutcome::ResumeFailed { sid } => {
                    return Ok(EnsureReadyOutcome::ResumeFailed { sid });
                }
                StartOutcome::Resumed
                | StartOutcome::Fresh
                | StartOutcome::FreshAfterFailedResume { .. } => {}
            }
            self.wait_for_pane_ready(&session);
            return Ok(EnsureReadyOutcome::Started);
        }
        if session.is_pane_dead() {
            let outcome = self
                .restart_with_resume_policy(size, false, ResumeAttemptPolicy::Allow)
                .map_err(EnsureReadyError::Tmux)?;
            match outcome {
                StartOutcome::ResumeFailed { sid } => {
                    return Ok(EnsureReadyOutcome::ResumeFailed { sid });
                }
                StartOutcome::Resumed
                | StartOutcome::Fresh
                | StartOutcome::FreshAfterFailedResume { .. } => {}
            }
            self.wait_for_pane_ready(&session);
            return Ok(EnsureReadyOutcome::Respawned);
        }
        Ok(EnsureReadyOutcome::AlreadyAlive)
    }

    /// Best-effort wait for a freshly-started pane to settle past its initial
    /// shell/splash so subsequent `send-keys` land in the agent instead of a
    /// boot prompt. Polls up to 3s in 50ms increments; returns even on
    /// timeout so a sluggish agent doesn't block the send indefinitely.
    ///
    /// Readiness signal:
    /// - Agents that expect a shell, run a custom command override, or have
    ///   an active hook status file: just wait for the pane to not be dead.
    ///   Wrapper scripts look like shells to tmux, so `is_pane_running_shell`
    ///   would never clear for them and we would eat the full 3s every time.
    ///   This mirrors the same guard chain `ensure_session` uses.
    /// - Real agents (e.g. claude, opencode): also wait for the pane to no
    ///   longer be running a shell, so a keystroke doesn't land in the boot
    ///   prompt that runs before the agent binary takes over.
    fn wait_for_pane_ready(&self, session: &tmux::Session) {
        let shell_check_unreliable = self.expects_shell()
            || self.has_command_override()
            || crate::hooks::read_hook_status(&self.id).is_some();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
        loop {
            if !session.exists() {
                return;
            }
            let pane_alive = !session.is_pane_dead();
            if pane_alive && (shell_check_unreliable || !session.is_pane_running_shell()) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    pub fn kill(&self) -> Result<()> {
        self.stop_poller();
        let session = self.tmux_session()?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Kill every tmux session owned by this instance (agent, web
    /// terminal, container terminal, tool sub-sessions). Best-effort
    /// and silent; agent/terminal/container terminal failures log at
    /// `debug!` target `session.tmux_cleanup`. Tool sub-sessions are
    /// silent by design via `kill_all_tool_sessions_for_id`.
    pub fn kill_all_tmux_sessions(&self) {
        if let Err(e) = self.kill() {
            tracing::debug!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                kind = "agent",
                error = %e,
                "kill_all_tmux_sessions: kill failed"
            );
        }
        self.kill_ancillary_tmux_sessions();
    }

    /// Kill every tmux session owned by this instance EXCEPT the agent
    /// session (web terminal, container terminal, tool sub-sessions).
    /// Used by call sites that want to handle the agent kill failure
    /// with caller-specific tracing while still letting all other
    /// kinds be cleaned up consistently.
    pub fn kill_ancillary_tmux_sessions(&self) {
        // Reaps every paired terminal (host + container, index 0 and the
        // additional web terminal tabs) in one tmux scan, so multi-terminal
        // sessions (#2437) do not leak panes on teardown.
        crate::tmux::kill_all_terminals_for_id(&self.id);
        crate::tmux::kill_all_tool_sessions_for_id(&self.id);
    }

    /// Stop the session: kill the tmux session and stop the Docker container
    /// (if sandboxed). The container is stopped but not removed, so it can be
    /// restarted on re-attach.
    ///
    /// On a docker inspect failure ([`crate::containers::Probe::Unknown`])
    /// the stop is attempted anyway with a `warn!` log, so a possibly-live
    /// container is not silently abandoned. A second `warn!` is emitted if
    /// the stop itself also fails (e.g. docker daemon is down); the overall
    /// `stop()` return still succeeds in that case (best-effort: the session
    /// record is marked Stopped regardless).
    pub fn stop(&self) -> Result<()> {
        self.kill()?;

        if self.is_sandboxed() {
            let container = containers::DockerContainer::from_session_id(&self.id);
            match container.probe_running() {
                containers::Probe::Running => container.stop()?,
                containers::Probe::NotRunning => {}
                containers::Probe::Unknown(e) => {
                    tracing::warn!(
                        target: "containers.runtime",
                        session = %self.id,
                        error = %e,
                        "docker inspect failed while probing sandbox container before session stop; attempting stop anyway to avoid leaving a possibly-live container behind"
                    );
                    if let Err(stop_err) = container.stop() {
                        tracing::warn!(
                            target: "containers.runtime",
                            session = %self.id,
                            error = %stop_err,
                            "sandbox container stop failed after probe failure; container may already be gone or docker is unreachable"
                        );
                    }
                }
            }
        }

        crate::hooks::cleanup_hook_status_dir(&self.id);

        Ok(())
    }

    /// Update status using pre-fetched pane metadata to avoid per-instance
    /// subprocess spawns. Falls back to subprocess calls if metadata is missing.
    ///
    /// Restamps `idle_entered_at`/`last_accessed_at` only when the detected
    /// status differs from [`Self::live_status_baseline`]. The baseline
    /// invariant lives on the field itself; this method's job is the
    /// guard shape (baseline vs. newly detected). Every call re-seeds the
    /// baseline at exit, so the next call compares against a value this
    /// method itself wrote.
    pub fn update_status_with_metadata(&mut self, metadata: Option<&tmux::PaneMetadata>) {
        let baseline = self.live_status_baseline;
        self.update_status_with_metadata_inner(metadata);
        if baseline.is_some_and(|prev| prev != self.status) {
            let now = Utc::now();
            self.last_accessed_at = Some(now);
            self.idle_entered_at = if self.status == Status::Idle {
                Some(now)
            } else {
                None
            };
        }
        self.live_status_baseline = Some(self.status);
    }

    fn update_status_with_metadata_inner(&mut self, metadata: Option<&tmux::PaneMetadata>) {
        if matches!(
            self.status,
            Status::Stopped | Status::Deleting | Status::Creating
        ) {
            return;
        }

        // Archived sessions have their tmux torn down on purpose (#1868), so
        // probing tmux here only ever produces a spurious "tmux session is
        // gone" Error transition (#2206). Short-circuit so the poller never
        // re-probes a row whose tmux is gone by design; this keeps
        // archive/unarchive status-preserving. Rows already persisted as Error
        // by a pre-fix build are cleaned up once by the v016 migration.
        if self.is_archived() {
            return;
        }

        // Acp-mode sessions are not backed by a tmux pane; the structured view
        // worker supervisor owns their lifecycle and emits typed health
        // events over the broadcast. Probing tmux here only ever produces
        // a spurious "tmux session is gone" Error transition.
        if self.is_structured() {
            // Clear any stale tmux-derived error so the UI doesn't show
            // a misleading message after a session is converted or
            // restarted in the structured view.
            if self.last_error.as_deref() == Some(TMUX_SESSION_GONE_ERROR) {
                self.last_error = None;
            }
            if self.status == Status::Error {
                self.status = Status::Idle;
            }
            return;
        }

        if self.status == Status::Error {
            if let Some(last_check) = self.last_error_check {
                if last_check.elapsed().as_secs() < 30 {
                    return;
                }
            }
        }

        if let Some(start_time) = self.last_start_time {
            if start_time.elapsed().as_secs() < 3 {
                self.status = Status::Starting;
                return;
            }
        }

        let session = match self.tmux_session() {
            Ok(s) => s,
            Err(_) => {
                tracing::trace!(target: "session.store",
                    "status '{}': tmux_session() failed, setting Error",
                    self.title
                );
                self.status = Status::Error;
                if self.last_error.is_none() {
                    self.last_error = Some(
                        "Could not reach tmux. Is tmux still running on the host?".to_string(),
                    );
                }
                self.last_error_check = Some(std::time::Instant::now());
                return;
            }
        };

        if !session.exists() {
            tracing::trace!(target: "session.store",
                "status '{}': session.exists()=false (tmux name={}), setting Error",
                self.title,
                tmux::Session::generate_name(&self.id, &self.title)
            );
            self.status = Status::Error;
            if self.last_error.is_none() {
                self.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());
            }
            self.last_error_check = Some(std::time::Instant::now());
            return;
        }

        let is_dead = metadata
            .map(|m| m.pane_dead)
            .unwrap_or_else(|| session.is_pane_dead());

        let pane_cmd = metadata
            .and_then(|m| m.pane_current_command.clone())
            .or_else(|| {
                let name = tmux::Session::generate_name(&self.id, &self.title);
                tmux::utils::pane_current_command(&name)
            });

        tracing::trace!(target: "session.store",
            "status '{}': exists=true, is_dead={}, pane_cmd={:?}, tool={}, cmd_override={}",
            self.title,
            is_dead,
            pane_cmd,
            self.tool,
            self.has_command_override()
        );

        let detection_tool = if self.detect_as.is_empty() {
            &self.tool
        } else {
            &self.detect_as
        };

        if let Some(hook_status) = crate::hooks::read_hook_status(&self.id) {
            tracing::trace!(target: "session.store",
                "status '{}': hook detected {:?}, is_dead={}",
                self.title,
                hook_status,
                is_dead
            );
            if is_dead {
                self.status = Status::Error;
                if self.last_error.is_none() {
                    let pane_content = session.capture_pane(20).unwrap_or_default();
                    self.last_error = Some(summarize_error_from_pane(&pane_content));
                }
            } else {
                // Codex and Claude both report Running from hooks while their
                // pane is actually parked on a blocking prompt, so when the
                // hook says Running we capture the pane and let the agent's
                // reconciler downgrade it (Codex: plan/numbered prompts;
                // Claude: tool-approval prompts, see #1913).
                let reconciles_running = detection_tool == "codex" || detection_tool == "claude";
                self.status = if reconciles_running && hook_status == Status::Running {
                    match session.capture_pane(50) {
                        Ok(pane_content) => {
                            if detection_tool == "codex" {
                                tmux::reconcile_codex_hook_status(hook_status, &pane_content)
                            } else {
                                tmux::reconcile_claude_hook_status(hook_status, &pane_content)
                            }
                        }
                        Err(e) => {
                            tracing::trace!(
                                "status '{}': {} hook fallback pane capture failed: {}",
                                self.title,
                                detection_tool,
                                e
                            );
                            hook_status
                        }
                    }
                } else {
                    hook_status
                };
                self.last_error = None;
            }
            return;
        }

        let pane_content = session.capture_pane(50).unwrap_or_default();
        let detected = tmux::detect_status_from_content(&pane_content, detection_tool);
        tracing::trace!(target: "session.store",
            "status '{}': detected={:?}, cmd_override={}, custom_cmd={}",
            self.title,
            detected,
            self.has_command_override(),
            self.has_custom_command(),
        );
        let is_shell_stale = || {
            let expects = self.expects_shell();
            if expects {
                return false;
            }
            let shell_check = metadata
                .and_then(|m| m.pane_current_command.as_deref())
                .map(tmux::utils::is_shell_command)
                .unwrap_or_else(|| session.is_pane_running_shell());
            tracing::trace!(target: "session.store",
                "status '{}': is_shell_stale check: expects_shell={}, shell_check={}",
                self.title,
                expects,
                shell_check,
            );
            shell_check
        };
        let has_command_override = self.has_command_override();
        let shell_stale = if detected == Status::Idle && !has_command_override && !is_dead {
            is_shell_stale()
        } else {
            false
        };
        self.status = resolve_detected_status(
            detected,
            is_dead,
            shell_stale,
            has_command_override,
            &pane_content,
            &self.tool,
        );

        tracing::trace!(target: "session.store", "status '{}': final={:?}", self.title, self.status);

        if self.status == Status::Error {
            if self.last_error.is_none() {
                self.last_error = Some(summarize_error_from_pane(&pane_content));
            }
        } else {
            self.last_error = None;
        }
    }

    pub fn update_status(&mut self) {
        self.update_status_with_metadata(None);
    }

    pub fn capture_output(&self, lines: usize) -> Result<String> {
        // capture-pane has no size parameters: the pane is captured at
        // the window's own dimensions. (A previous *_with_size variant
        // accepted width/height and silently ignored them.)
        self.tmux_session()?.capture_pane(lines)
    }
}

fn generate_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")[..16].to_string()
}

/// Build a short human-readable hint for why a session transitioned to Error.
///
/// Called when we set Status::Error but don't already have a `last_error`
/// populated (e.g. an agent process exited on its own). We grab the last few
/// non-empty lines of the pane and pick something that looks like an error
/// message; otherwise fall back to a generic "stopped responding" string so
/// the UI never renders an Error state without any explanation.
fn summarize_error_from_pane(pane_content: &str) -> String {
    let cleaned = crate::tmux::utils::strip_ansi(pane_content);
    let tail: Vec<&str> = cleaned
        .lines()
        .rev()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(12)
        .collect();

    for line in &tail {
        let lower = line.to_lowercase();
        if lower.contains("error")
            || lower.contains("command not found")
            || lower.contains("permission denied")
            || lower.contains("cannot")
            || lower.contains("failed")
            || lower.contains("no such file")
            || lower.contains("traceback")
            || lower.contains("panic")
        {
            return truncate_error_line(line);
        }
    }

    if let Some(last) = tail.first() {
        return format!(
            "Agent stopped responding. Last line: {}",
            truncate_error_line(last)
        );
    }

    "Agent stopped responding and the pane is empty.".to_string()
}

fn truncate_error_line(line: &str) -> String {
    const MAX: usize = 200;
    let trimmed = line.trim();
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut out = String::with_capacity(MAX + 1);
        for (i, ch) in trimmed.char_indices() {
            if i >= MAX {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        out
    }
}

/// Format an environment variable assignment as a shell-safe command prefix.
///
/// Uses `shell_escape` (single-quote escaping) so the value is preserved
/// verbatim when parsed by the inner `bash -c '...'` shell created by
/// `wrap_command_ignore_suspend`.
fn format_env_var_prefix(key: &str, value: &str, cmd: &str) -> String {
    let escaped = shell_escape(value);
    format!("{}={} {}", key, escaped, cmd)
}

/// Prepend agent-specific environment overrides to a launch command.
///
/// Some terminal agents inherit the parent tmux env, which can carry
/// `NO_COLOR=1` and silently disable their terminal palettes even though the
/// web renderer handles ANSI fine. Unsetting `NO_COLOR` and advertising
/// `TERM=xterm-256color` plus `COLORTERM=truecolor` at launch keeps color on
/// without pinning tools to a specific `FORCE_COLOR` depth.
fn apply_agent_launch_env(cmd: &mut String, agent: Option<&'static crate::agents::AgentDef>) {
    if !matches!(agent.map(|a| a.name), Some("antigravity" | "codex")) {
        return;
    }

    *cmd = format!(
        "env -u NO_COLOR TERM=xterm-256color COLORTERM=truecolor {}",
        cmd
    );
}

/// Wrap a command to disable Ctrl-Z (SIGTSTP) suspension.
///
/// Command run inside the sandbox container for the web Container terminal tab.
///
/// Resolves the container user's login shell at spawn time, inside the container,
/// and execs it as a login shell so profile/rc files load (parity with the Host
/// terminal tab, which launches the user's default shell as a login shell).
/// Resolution order: the passwd entry (the authoritative login shell, what
/// `chsh` writes and what `login(1)` reads into `$SHELL`), then the container's
/// `$SHELL`, then bash, sh. Passwd comes first because `docker exec` never goes
/// through `login(1)`, so `$SHELL` is usually unset or a generic image default
/// rather than the user's configured shell. Each candidate is run through
/// `command -v` so an unset, stale, or non-executable value falls through to the
/// next instead of killing the pane.
///
/// The single-quoted body is evaluated by the container's `sh`, not the host
/// shell tmux uses to spawn the session, so the embedded `$()` runs in the
/// container. The host does not propagate its own `$SHELL` into the container,
/// so this reads the container's value, not the host's.
const CONTAINER_TERMINAL_AUTODETECT_CMD: &str = r#"sh -c 'exec "$(command -v "$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f7)" 2>/dev/null || command -v "$SHELL" 2>/dev/null || command -v bash || command -v sh)" -l'"#;

/// When running agents directly as tmux session commands (without a parent shell),
/// pressing Ctrl-Z suspends the process with no way to recover via job control.
/// This wrapper disables the suspend character at the terminal level before exec'ing
/// the actual command.
///
/// Uses POSIX-standard `stty susp undef` which works on both Linux and macOS.
/// Single quotes in `cmd` are escaped with the `'\''` technique to prevent
/// breaking out of the outer single-quoted wrapper.
///
/// The leading `exec` ensures the tmux default shell (which may be fish, nu,
/// etc.) replaces itself with the POSIX wrapper. Without it, fish stays as the
/// pane process because fish does not exec the last command in `-c` mode. That
/// causes `#{pane_current_command}` to report "fish", which triggers a false
/// restart on reattach. See #757.
fn wrap_command_ignore_suspend(cmd: &str) -> String {
    let user = super::environment::user_shell();
    let posix = super::environment::user_posix_shell();
    let escaped = cmd.replace('\'', "'\\''");
    // Use login shell (-l) so version-manager PATHs (NVM, etc.) are available.
    // Skip -l when falling back to bash for a non-POSIX user shell (fish, nu,
    // pwsh): bash's login scripts won't contain the user's PATH setup and -l
    // may reset the inherited PATH that already has the correct entries.
    let flag = if user == posix { "-lc" } else { "-c" };
    format!(
        "exec {} {} 'stty susp undef; exec env {}'",
        posix, flag, escaped
    )
}

/// Prepend shell `export` statements to an already-wrapped sandbox command.
///
/// `wrapped` MUST be the output of `wrap_command_ignore_suspend`, which
/// guarantees a leading `exec`. This function therefore MUST NOT add another
/// `exec` of its own: in bash, `exec exec <cmd>` searches PATH for a binary
/// literally named `exec`, fails with exit 127, and kills the tmux pane on
/// every sandboxed launch. zsh-on-macOS happens to tolerate the double-exec,
/// which is why this regression hid for several days after #757 added the
/// leading `exec` to `wrap_command_ignore_suspend`. See PR #819.
fn prepend_exports(exports: &[String], wrapped: String) -> String {
    if exports.is_empty() {
        wrapped
    } else {
        format!("{}; {}", exports.join("; "), wrapped)
    }
}

fn resolve_detected_status(
    detected: Status,
    is_dead: bool,
    is_shell_stale: bool,
    has_command_override: bool,
    pane_content: &str,
    tool: &str,
) -> Status {
    match detected {
        Status::Idle if has_command_override => {
            // Custom commands run agents through wrapper scripts that appear
            // as shell processes to tmux, so we can't trust the pane's current
            // command here; decide from pane *content* instead. A pane that is
            // still rendering the agent TUI is genuinely parked at its prompt,
            // so a detected Idle is real and we keep it (otherwise on_idle /
            // on_waiting status hooks never fire for wrapped agents, e.g. an
            // opencode session launched via agent_command_override, see #2022).
            // Only declare Error when the pane is actually dead; a live pane
            // without recognizable agent content stays Unknown.
            if is_dead {
                Status::Error
            } else if pane_has_agent_content(pane_content, tool) {
                Status::Idle
            } else {
                Status::Unknown
            }
        }
        Status::Idle if is_dead => Status::Error,
        Status::Idle if is_shell_stale => resolve_shell_stale_status(pane_content, tool),
        other => other,
    }
}

fn resolve_shell_stale_status(pane_content: &str, tool: &str) -> Status {
    if pane_has_agent_content(pane_content, tool) {
        Status::Idle
    } else if pane_looks_like_bare_shell_prompt(pane_content) {
        Status::Error
    } else {
        Status::Unknown
    }
}

fn pane_looks_like_bare_shell_prompt(raw_content: &str) -> bool {
    let clean = crate::tmux::utils::strip_ansi(raw_content);
    let Some(last) = clean.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let last = last.trim();
    last.ends_with('$') || last.ends_with('#') || last.ends_with('%') || last.ends_with('\u{276f}')
}

/// Check whether captured pane content indicates a living agent rather than
/// a bare shell prompt. Used to prevent `is_shell_stale()` from producing
/// false `Error` status when the agent binary is a shell wrapper or spawns
/// persistent child shell processes.
fn pane_has_agent_content(raw_content: &str, tool: &str) -> bool {
    let clean = crate::tmux::utils::strip_ansi(raw_content);
    let non_empty: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();

    if non_empty.is_empty() {
        return false;
    }

    // If the last visible line looks like a shell prompt, the agent likely
    // exited and the shell took over. This catches servers with verbose MOTD
    // that would otherwise exceed the line-count threshold.
    if pane_looks_like_bare_shell_prompt(raw_content) {
        return false;
    }

    // Agent TUIs fill the screen with UI elements. A bare shell prompt
    // (after MOTD) rarely exceeds this threshold once the prompt check
    // above filters out typical shell endings.
    if non_empty.len() > 5 {
        return true;
    }

    // Use word-boundary matching so short names like "pi" don't produce
    // false positives inside words like "api" or "pipeline".
    let mut tool_names = vec![tool.to_lowercase()];
    if let Some(agent) = crate::agents::get_agent(tool) {
        let binary = agent.binary.to_lowercase();
        if !tool_names.contains(&binary) {
            tool_names.push(binary);
        }
    }
    let lower = clean.to_lowercase();
    if lower
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|word| tool_names.iter().any(|name| word == name))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_terminal_autodetect_cmd_resolves_login_shell() {
        let cmd = CONTAINER_TERMINAL_AUTODETECT_CMD;
        // Resolution order: passwd entry first (authoritative, since docker exec
        // skips login(1) and so $SHELL is usually unset), then $SHELL, then
        // bash, sh. Each candidate is guarded by `command -v` so an unset, stale,
        // or non-executable value falls through rather than killing the pane.
        assert!(cmd.contains("getent passwd"));
        assert!(cmd.contains(r#"command -v "$SHELL""#));
        assert!(cmd.contains("command -v bash"));
        assert!(cmd.contains("command -v sh"));
        // Passwd is resolved ahead of $SHELL.
        assert!(cmd.find("getent passwd").unwrap() < cmd.find(r#"command -v "$SHELL""#).unwrap());
        // Login shell so profile/rc files load, matching the Host terminal tab.
        assert!(cmd.contains("-l"));
        // Single-quoted body: the embedded command substitution is evaluated by
        // the container's sh, not the host shell tmux spawns the session with.
        assert!(cmd.starts_with("sh -c '"));
    }

    struct CodexHomeGuard(Option<String>);
    impl CodexHomeGuard {
        fn unset() -> Self {
            let prev = std::env::var("CODEX_HOME").ok();
            std::env::remove_var("CODEX_HOME");
            Self(prev)
        }
    }
    impl Drop for CodexHomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }

    /// Regression for issue #2414: a sandboxed worktree session's
    /// `container_workdir()` must stay pinned to what the container was created
    /// with, even after the host worktree's git linkage breaks.
    ///
    /// When the worktree's admin entry under `<main>/.git/worktrees/<name>` is
    /// pruned, the `.git` file's gitdir no longer resolves, `compute_volume_paths`
    /// can't find the main repo, and it silently collapses to
    /// `/workspace/<basename>` -- a path the container never mounted -- so a
    /// `docker exec -w` dies with `chdir to cwd ... no such file or directory`.
    /// The create-time-pinned `SandboxInfo::container_workdir` defends against
    /// that drift.
    #[test]
    fn container_workdir_stays_pinned_when_worktree_linkage_breaks() {
        use tempfile::TempDir;
        let root = TempDir::new().unwrap();
        // An orphaned worktree: a `.git` file whose gitdir points nowhere,
        // exactly the state a pruned admin entry leaves behind.
        let worktree = root.path().join("myrepo-worktrees").join("contexec");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../does-not-exist/.git/worktrees/contexec\n",
        )
        .unwrap();

        let mut inst = Instance::new("contexec", worktree.to_str().unwrap());
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "img".to_string(),
            container_name: "aoe-sandbox-test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });

        // Bug reproduction: with nothing pinned, the live recompute can't resolve
        // the orphaned worktree and falls back to the basename. This is the path
        // that produced the `chdir to cwd ("/workspace/contexec")` failure.
        assert_eq!(inst.container_workdir(), "/workspace/contexec");

        // Fix: the value the container was actually built with is returned
        // verbatim, so the exec targets a path that exists in the container.
        let pinned = "/workspace/myrepo-worktrees/contexec".to_string();
        inst.sandbox_info.as_mut().unwrap().container_workdir = Some(pinned.clone());
        assert_eq!(inst.container_workdir(), pinned);
    }

    #[test]
    fn test_new_instance() {
        let inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.title, "test");
        assert_eq!(inst.project_path, "/tmp/test");
        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.id.len(), 16);
    }

    #[test]
    fn test_codex_gets_status_hook_env_prefix() {
        let agent = crate::agents::get_agent("codex");
        assert_eq!(
            status_hook_env_prefix("work", "abc123", agent),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_custom_codex_detected_agent_uses_codex_hook_installer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let mut inst = Instance::new("wrapped", "/tmp/test");
        inst.tool = "my-codex-wrapper".to_string();
        inst.detect_as = "codex".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = tmp.path().join(".codex").join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
        assert!(!tmp.path().join(".codex").join("config.toml").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_uses_profile_codex_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let codex_home = tmp.path().join("profile-codex-home");
        let profile_dir = crate::session::get_profile_dir("codex-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!("environment = [\"CODEX_HOME={}\"]\n", codex_home.display()),
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "codex-profile".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = codex_home.join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_respects_profile_hooks_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let profile_dir = crate::session::get_profile_dir("hooks-disabled").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = false\n",
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "hooks-disabled".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_respects_profile_hooks_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let mut global = crate::session::config::Config::default();
        global.session.agent_status_hooks = false;
        crate::session::config::save_config(&global).unwrap();

        let profile_dir = crate::session::get_profile_dir("hooks-enabled").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = true\n",
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "hooks-enabled".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = tmp.path().join(".codex").join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
    }

    #[test]
    fn test_is_sub_session() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sub_session());

        inst.parent_session_id = Some("parent123".to_string());
        assert!(inst.is_sub_session());
    }

    /// `touch_last_accessed` is what `aoe send` and the TUI dispatch path
    /// call when the user interacts with a session. It must auto-wake
    /// archived and snoozed rows so sending a message to a sunk session
    /// brings it back, while preserving the favorite flag (favorite is a
    /// positive "care more" signal, not a sink state).
    #[test]
    fn test_touch_last_accessed_clears_archived() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        assert!(inst.is_archived());
        inst.touch_last_accessed();
        assert!(!inst.is_archived());
        assert!(inst.last_accessed_at.is_some());
    }

    #[test]
    fn test_archived_session_not_marked_error_when_tmux_gone() {
        // #2206: archiving kills the session's tmux on purpose. A subsequent
        // status poll must not flip the archived row to Error for the missing
        // tmux; the archived guard short-circuits, so an idle row stays Idle.
        // Red on the pre-fix tree, where the tmux probe stamps Error.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.update_status_with_metadata(None);
        assert_ne!(inst.status, Status::Error);
        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.last_error, None);
    }

    #[test]
    fn test_archived_session_preserves_genuine_error() {
        // #2206 regression guard (passes on both trees): the archived guard
        // never mutates status, so a genuinely errored session keeps its Error
        // state while archived. The legacy on-disk footprint is cleaned up by
        // the v016 migration, not by the poller.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        inst.update_status_with_metadata(None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn test_archived_unarchived_genuine_error_roundtrips() {
        // #2206: archive then unarchive must stay status-preserving for a real
        // failure. The archived guard leaves Error untouched; after unarchive
        // the tmux probe re-stamps Error and its is_none() guard preserves the
        // original message regardless of whether tmux is installed on the box.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        inst.update_status_with_metadata(None);
        inst.unarchive();
        inst.update_status_with_metadata(None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn test_touch_last_accessed_clears_snooze() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.snooze(30);
        assert!(inst.is_snoozed());
        inst.touch_last_accessed();
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_touch_last_accessed_clears_idle_dormant() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.mark_idle_dormant();
        assert!(inst.is_idle_dormant());
        inst.touch_last_accessed();
        assert!(!inst.is_idle_dormant());
    }

    #[test]
    fn test_unarchive_clears_idle_dormant() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.mark_idle_dormant();
        assert!(inst.is_archived());
        assert!(inst.is_idle_dormant());

        inst.unarchive();

        assert!(!inst.is_archived());
        assert!(
            !inst.is_idle_dormant(),
            "unarchive should wake sessions blocked by idle auto-stop"
        );
    }

    #[test]
    fn test_mark_unread_and_mark_read_are_idempotent() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_unread());
        // read -> unread
        inst.mark_unread();
        assert!(inst.is_unread());
        // unread -> unread (idempotent)
        inst.mark_unread();
        assert!(inst.is_unread());
        // unread -> read
        inst.mark_read();
        assert!(!inst.is_unread());
        // read -> read (idempotent)
        inst.mark_read();
        assert!(!inst.is_unread());
    }

    #[test]
    fn test_toggle_unread_round_trips() {
        let mut inst = Instance::new("test", "/tmp/test");
        // read -> unread
        inst.toggle_unread();
        assert!(inst.is_unread());
        // unread -> read
        inst.toggle_unread();
        assert!(!inst.is_unread());
    }

    #[test]
    fn test_unread_serde_round_trip() {
        // Absent field deserializes to false (older sessions.json).
        let inst: Instance = serde_json::from_value(serde_json::json!({
            "id": "abc",
            "title": "t",
            "project_path": "/tmp",
            "tool": "claude",
            "status": "idle",
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("deserialize without unread");
        assert!(!inst.unread);

        // Round-trips when set, and is omitted when false.
        let mut set = Instance::new("t", "/tmp");
        set.unread = true;
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["unread"], serde_json::json!(true));
        let back: Instance = serde_json::from_value(json).unwrap();
        assert!(back.unread);

        let read = Instance::new("t", "/tmp");
        let json = serde_json::to_value(&read).unwrap();
        assert!(
            json.get("unread").is_none(),
            "false must skip serialization"
        );
    }

    #[test]
    fn test_plugin_meta_serde_round_trip() {
        // Empty map is omitted from disk.
        let inst = Instance::new("t", "/tmp");
        let json = serde_json::to_value(&inst).unwrap();
        assert!(
            json.get("plugin_meta").is_none(),
            "empty plugin_meta must skip serialization"
        );

        // A plugin's namespaced slot round-trips.
        let mut set = Instance::new("t", "/tmp");
        set.plugin_meta
            .insert("aoe.status".to_string(), serde_json::json!({ "score": 3 }));
        let json = serde_json::to_value(&set).unwrap();
        let back: Instance = serde_json::from_value(json).unwrap();
        assert_eq!(back.plugin_meta["aoe.status"]["score"], 3);

        // Rows written before the field existed deserialize to an empty map.
        let inst: Instance = serde_json::from_value(serde_json::json!({
            "id": "abc",
            "title": "t",
            "project_path": "/tmp",
            "tool": "claude",
            "status": "idle",
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("deserialize without plugin_meta");
        assert!(inst.plugin_meta.is_empty());
    }

    #[test]
    fn test_merge_user_action_diff_propagates_unread() {
        let pre = Instance::new("t", "/tmp");
        let mut post = pre.clone();
        post.unread = true;
        let mut disk = pre.clone();
        disk.merge_user_action_diff(&pre, &post);
        assert!(disk.unread);

        // Clearing also propagates.
        let pre2 = post.clone();
        let mut post2 = pre2.clone();
        post2.unread = false;
        let mut disk2 = pre2.clone();
        disk2.merge_user_action_diff(&pre2, &post2);
        assert!(!disk2.unread);
    }

    #[test]
    fn test_merge_user_action_diff_propagates_trash_marker() {
        let pre = Instance::new("t", "/tmp");
        let mut post = pre.clone();
        post.trash();
        let mut disk = pre.clone();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.is_trashed());

        let pre2 = post.clone();
        let mut post2 = pre2.clone();
        post2.untrash();
        let mut disk2 = pre2.clone();

        disk2.merge_user_action_diff(&pre2, &post2);

        assert!(!disk2.is_trashed());
    }

    #[test]
    fn test_mark_idle_dormant_sets_marker() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_idle_dormant());
        inst.mark_idle_dormant();
        assert!(inst.is_idle_dormant());
        assert!(inst.idle_dormant_since.is_some());
    }

    #[test]
    fn test_touch_last_accessed_preserves_favorite() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.favorite();
        assert!(inst.is_favorited());
        inst.touch_last_accessed();
        // Favorite is orthogonal to sink states; user interaction must not
        // clear it.
        assert!(inst.is_favorited());
    }

    #[test]
    fn test_merge_post_start_preserves_peer_field_writes() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.archive();
        stored.agent_session_id = Some("daemon-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;

        stored.merge_post_start(&working);

        assert_eq!(stored.status, Status::Starting);
        assert!(stored.is_archived(), "peer archive must survive merge");
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("daemon-sid"),
            "peer-written sid must survive merge"
        );
    }

    #[test]
    fn test_merge_post_restart_preserves_peer_sid() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("peer-fresh-sid".to_string());
        stored.snooze(15);

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Idle;
        working.agent_session_id = Some("phase1-stale-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(stored.status, Status::Idle);
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("peer-fresh-sid"),
            "restart merge must not clobber peer sid write"
        );
        assert!(stored.is_snoozed(), "peer snooze must survive merge");
    }

    #[test]
    fn test_merge_post_restart_copies_resume_failed_marker_when_sid_matches() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("failed-sid".to_string());
        stored.resume_probe_failed_sid = None;

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Error;
        working.agent_session_id = Some("failed-sid".to_string());
        working.resume_probe_failed_sid = Some("failed-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(stored.status, Status::Error);
        assert_eq!(stored.agent_session_id.as_deref(), Some("failed-sid"));
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("failed-sid")
        );
    }

    #[test]
    fn test_merge_post_restart_preserves_peer_marker_when_sid_mismatches() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("poller-fresh-sid".to_string());
        stored.resume_probe_failed_sid = Some("poller-fresh-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;
        working.agent_session_id = Some("phase1-stale-sid".to_string());
        working.resume_probe_failed_sid = Some("phase1-stale-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("poller-fresh-sid"),
            "poller wrote a fresh sid between phase 2 and phase 3; merge preserves it"
        );
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("poller-fresh-sid"),
            "marker for peer sid remains authoritative"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_favorite() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.favorite();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.favorited_at.is_some(), "TUI favorite landed");
        assert!(
            disk.archived_at.is_none(),
            "favorite() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_favorite_loses_to_tui_archive() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.favorite();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "TUI archive landed");
        assert!(
            disk.favorited_at.is_none(),
            "archive() invariant must clear concurrent peer favorite"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_touch() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.touch_last_accessed();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.archived_at.is_none(),
            "touch_last_accessed() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_touch_clears_tui_archive() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));

        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.touch_last_accessed();

        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.archived_at.is_none(),
            "peer touch (newer last_accessed_at) must dethrone TUI archive per messaging-unarchives rule"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_clears_concurrent_tui_snooze() {
        // The web/TUI/CLI contract treats pinned/archived/snoozed as
        // mutually exclusive (the sidebar tier comparator assumes a
        // single active triage state, see #1581). When a TUI snooze
        // races a peer archive, archive wins: snooze is a temporary
        // sink and archive is the indefinite one, so leaving both set
        // would surface contradictory triage state on the next render.
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.snooze(15);

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "peer archive survives");
        assert!(
            disk.snoozed_until.is_none(),
            "archive() invariant must clear a concurrent TUI snooze"
        );
    }

    #[test]
    fn test_archive_clears_snooze() {
        // Direct mutator test (no merge): the data-layer contract is
        // that archive is mutually exclusive with every other triage
        // flag. The sidebar tier comparator in `sidebarSort.ts`
        // assumes the server enforces exactly one active state, so a
        // snooze-then-archive transition must leave only archive
        // behind. See #1581.
        let mut inst = Instance::new("s", "/tmp/x");
        inst.snooze(15);
        assert!(inst.is_snoozed());
        inst.archive();
        assert!(inst.is_archived());
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_merge_diff_tui_unfavorite_does_not_resurrect_peer_archive() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.favorite();

        let mut post = pre.clone();
        post.unfavorite();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.favorited_at.is_none(), "TUI unfavorite landed");
        assert!(
            disk.archived_at.is_some(),
            "post.favorited_at == None; favorite-invariant rule must NOT fire"
        );
    }

    #[test]
    fn test_merge_diff_uses_self_not_post_for_touch_detection() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));
        pre.archived_at = Some(Utc::now() - chrono::Duration::seconds(120));

        let mut post = pre.clone();
        post.title = "renamed".into();

        let mut disk = pre.clone();
        disk.touch_last_accessed();

        disk.merge_user_action_diff(&pre, &post);

        assert_eq!(disk.title, "renamed");
        assert!(disk.archived_at.is_none());
    }

    #[test]
    fn test_pin_clears_archive_and_snooze() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.archive();
        assert!(inst.is_archived());
        inst.pin();
        assert!(inst.is_pinned());
        assert!(!inst.is_archived());
        assert!(!inst.is_snoozed());

        inst.snooze(15);
        assert!(inst.is_snoozed());
        inst.pin();
        assert!(inst.is_pinned());
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_archive_clears_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.archive();
        assert!(inst.is_archived());
        assert!(!inst.is_pinned());
    }

    #[test]
    fn test_trash_untrash_roundtrip() {
        let mut inst = Instance::new("s", "/tmp/x");
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);

        inst.trash();
        assert!(inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);

        inst.untrash();
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
    }

    #[test]
    fn test_trash_preserves_sibling_triage_flags() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.favorite();
        inst.pin();
        assert!(inst.is_favorited());
        assert!(inst.is_pinned());

        inst.trash();
        // Trash wins the bucket but leaves the decorations intact so
        // restore is faithful (a trashed favorite comes back a favorite).
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);
        assert!(inst.is_favorited(), "favorite preserved across trash");
        assert!(inst.is_pinned(), "pin preserved across trash");

        inst.untrash();
        assert!(inst.is_favorited());
        assert!(inst.is_pinned());
    }

    #[test]
    fn test_effective_bucket_trash_beats_archive() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.archive();
        assert_eq!(inst.effective_bucket(), SessionBucket::Archived);
        inst.trash();
        assert_eq!(
            inst.effective_bucket(),
            SessionBucket::Trashed,
            "trash takes precedence over archive in bucketing"
        );
        // archived_at is preserved, so restore returns to the archived bucket.
        assert!(inst.is_archived());
        inst.untrash();
        assert_eq!(inst.effective_bucket(), SessionBucket::Archived);
    }

    #[test]
    fn test_trashed_at_serde_roundtrip_and_default() {
        // A non-trashed instance omits trashed_at on the wire
        // (skip_serializing_if), so deserializing it exercises the
        // missing-field path that legacy rows hit: it must default to None,
        // which is why no migration is needed.
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(
            !fresh_json.contains("trashed_at"),
            "None trashed_at must not be serialized"
        );
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert!(!parsed.is_trashed(), "missing trashed_at => None");

        let mut inst = Instance::new("s", "/tmp/x");
        inst.trash();
        let json = serde_json::to_string(&inst).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert!(back.is_trashed());
    }

    // A non-fork session omits fork_pending on the wire (skip_serializing_if),
    // so legacy sessions.json without the key deserializes to None and no
    // migration is needed. A seeded fork id round-trips.
    #[cfg(feature = "serve")]
    #[test]
    fn test_fork_pending_serde_roundtrip_and_default() {
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(
            !fresh_json.contains("fork_pending"),
            "None fork_pending must not be serialized"
        );
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert_eq!(parsed.fork_pending, None, "missing fork_pending => None");

        let mut inst = Instance::new("s", "/tmp/x");
        inst.fork_pending = Some("parent-acp-id".into());
        let json = serde_json::to_string(&inst).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back.fork_pending.as_deref(), Some("parent-acp-id"));
    }

    #[test]
    fn test_snooze_clears_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.snooze(30);
        assert!(inst.is_snoozed());
        assert!(!inst.is_pinned());
    }

    #[test]
    fn test_touch_last_accessed_preserves_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.touch_last_accessed();
        // Pin is an explicit user surfacing signal, not a sink state.
        // User interaction (send, attach) must NOT clear it.
        assert!(inst.is_pinned());
    }

    #[test]
    fn test_pin_and_favorite_coexist() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.favorite();
        assert!(inst.is_favorited());
        inst.pin();
        // Pin and favorite drive different surfaces (TUI Attention vs web
        // sidebar). They must coexist; pinning does NOT clear favorite.
        assert!(inst.is_pinned());
        assert!(inst.is_favorited());

        let mut inst2 = Instance::new("s2", "/tmp/x");
        inst2.pin();
        inst2.favorite();
        // Same in reverse: favoriting does NOT clear pin.
        assert!(inst2.is_pinned());
        assert!(inst2.is_favorited());
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_pin() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.pin();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.pinned_at.is_some(), "TUI pin landed");
        assert!(
            disk.archived_at.is_none(),
            "pin() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_pin_loses_to_tui_archive() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.pin();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "TUI archive landed");
        assert!(
            disk.pinned_at.is_none(),
            "archive() invariant must clear concurrent peer pin"
        );
    }

    #[test]
    fn test_merge_diff_peer_pin_loses_to_tui_snooze() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.snooze(30);

        let mut disk = pre.clone();
        disk.pin();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.snoozed_until.is_some(), "TUI snooze landed");
        assert!(
            disk.pinned_at.is_none(),
            "snooze() invariant must clear concurrent peer pin"
        );
    }

    #[test]
    fn test_merge_diff_peer_touch_preserves_pin() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));

        let mut post = pre.clone();
        post.pin();

        let mut disk = pre.clone();
        disk.touch_last_accessed();

        disk.merge_user_action_diff(&pre, &post);

        // Touch dethrones archive/snooze but NOT pin: pin is an explicit
        // surfacing signal that the user's interaction does not contradict.
        assert!(
            disk.pinned_at.is_some(),
            "peer touch must NOT clear concurrent TUI pin"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_applies_status_and_timestamps() {
        let mut disk = Instance::new("session", "/tmp/test");
        disk.status = Status::Running;
        disk.idle_entered_at = None;
        disk.last_accessed_at = Some(Utc::now() - chrono::Duration::hours(1));
        disk.title = "peer-title".to_string();
        disk.group_path = "peer/group".to_string();
        disk.unread = true;
        disk.archived_at = Some(Utc::now());
        disk.favorited_at = None;
        disk.pinned_at = Some(Utc::now());
        let before = disk.clone();

        let now = Utc::now();
        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: Some(now),
            last_accessed_at: Some(now),
        };
        disk.merge_passive_status_patch(&patch);

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(now));
        assert_eq!(disk.last_accessed_at, Some(now));
        // Narrow splice: nothing else moves.
        assert_eq!(disk.title, before.title);
        assert_eq!(disk.group_path, before.group_path);
        assert_eq!(disk.unread, before.unread);
        assert_eq!(disk.archived_at, before.archived_at);
        assert_eq!(disk.favorited_at, before.favorited_at);
        assert_eq!(disk.pinned_at, before.pinned_at);
    }

    #[test]
    fn test_merge_passive_status_patch_never_fabricates_last_accessed_at() {
        // The source Instance was never touched by a user (last_accessed_at
        // itself None); the patch must preserve that rather than fabricate
        // a stamp, or a session that transitions status before anyone
        // attaches gains a spurious "touched" signal.
        let mut disk = Instance::new("session", "/tmp/test");
        disk.status = Status::Starting;
        disk.last_accessed_at = None;

        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: Some(Utc::now()),
            last_accessed_at: None,
        };
        disk.merge_passive_status_patch(&patch);

        assert_eq!(disk.status, Status::Idle, "status must still apply");
        assert_eq!(
            disk.last_accessed_at, None,
            "must not fabricate a last_accessed_at the source never had"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_status_and_idle_entered_at_apply_even_when_last_accessed_at_is_stale(
    ) {
        // A peer (CLI, TUI apply_user_action) touched last_accessed_at more
        // recently than the passive patch's snapshot: only last_accessed_at
        // is guarded. status/idle_entered_at still apply, or a real status
        // transition would silently strand on disk until the next one.
        let mut disk = Instance::new("session", "/tmp/test");
        let peer_touch = Utc::now();
        disk.status = Status::Running;
        disk.last_accessed_at = Some(peer_touch);
        disk.idle_entered_at = None;

        let stale_patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: Some(peer_touch - chrono::Duration::minutes(5)),
            last_accessed_at: Some(peer_touch - chrono::Duration::minutes(5)),
        };
        disk.merge_passive_status_patch(&stale_patch);

        assert_eq!(
            disk.status,
            Status::Idle,
            "status must apply even when last_accessed_at is stale"
        );
        assert_eq!(
            disk.idle_entered_at,
            Some(peer_touch - chrono::Duration::minutes(5)),
            "idle_entered_at must apply even when last_accessed_at is stale"
        );
        assert_eq!(
            disk.last_accessed_at,
            Some(peer_touch),
            "only last_accessed_at itself is guarded against the stale patch"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_equal_is_a_noop() {
        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        disk.last_accessed_at = Some(ts);

        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&patch);

        // Guard is `>=`: equal timestamps are not a real advance, so the
        // patch's last_accessed_at is dropped. The observable value stays
        // equal to `ts` either way (disk == incoming), so the assertion
        // does not change; the point of the guard is skipping the write.
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_newer_applies() {
        let mut disk = Instance::new("session", "/tmp/test");
        let older = Utc::now() - chrono::Duration::minutes(1);
        disk.last_accessed_at = Some(older);

        let newer = Utc::now();
        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(newer),
        };
        disk.merge_passive_status_patch(&patch);

        assert_eq!(disk.last_accessed_at, Some(newer));
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_disk_none_applies() {
        // disk.last_accessed_at == None means never touched, not "newer":
        // `is_some_and` short-circuits to false, so the patch always wins.
        let mut disk = Instance::new("session", "/tmp/test");
        disk.last_accessed_at = None;

        let ts = Utc::now();
        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&patch);

        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_identical_is_idempotent() {
        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        let patch = PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: Some(ts),
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&patch);
        disk.merge_passive_status_patch(&patch);

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(ts));
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_increasing_newer_wins() {
        let mut disk = Instance::new("session", "/tmp/test");
        let t0 = Utc::now() - chrono::Duration::minutes(1);
        let t1 = Utc::now();

        disk.merge_passive_status_patch(&PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Running,
            idle_entered_at: None,
            last_accessed_at: Some(t0),
        });
        disk.merge_passive_status_patch(&PassiveStatusPatch {
            id: disk.id.clone(),
            status: Status::Idle,
            idle_entered_at: Some(t1),
            last_accessed_at: Some(t1),
        });

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(t1));
        assert_eq!(disk.last_accessed_at, Some(t1));
    }

    #[test]
    fn test_merge_from_tui_copies_status_pipeline() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.status = Status::Idle;

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.status = Status::Running;
        src.idle_entered_at = Some(Utc::now());

        stored.merge_from_tui(&src);

        assert_eq!(stored.status, Status::Running);
        assert_eq!(stored.idle_entered_at, src.idle_entered_at);
    }

    #[test]
    fn test_update_status_with_metadata_seeds_baseline_without_restamp() {
        // #2690: a session loaded fresh from disk (e.g. TUI relaunch, or
        // every tick of the daemon's status_poll_loop) has no live
        // observation history yet: `live_status_baseline` is `None`. The
        // very first status check must not treat a mismatch between the
        // disk-loaded `status` and the freshly detected status as a real
        // transition, or every reload would reset idle_entered_at/
        // last_accessed_at to `now`. Red on the pre-fix tree (which compares
        // against `self.status` directly and always restamps here, since no
        // real tmux session exists for this instance).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = None;
        inst.status = Status::Starting;
        let stale_idle_entered_at = Some(Utc::now() - chrono::Duration::hours(2));
        let stale_last_accessed_at = Some(Utc::now() - chrono::Duration::hours(2));
        inst.idle_entered_at = stale_idle_entered_at;
        inst.last_accessed_at = stale_last_accessed_at;

        inst.update_status_with_metadata(None);

        // No real tmux session exists for this instance, so detection
        // resolves to Error, differing from the stale disk `Starting`. That
        // mismatch must NOT be treated as a genuine transition.
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, stale_idle_entered_at,
            "first check after a fresh load must not clobber a stale-but-real idle_entered_at"
        );
        assert_eq!(
            inst.last_accessed_at, stale_last_accessed_at,
            "first check after a fresh load must not clobber a stale-but-real last_accessed_at"
        );
        assert_eq!(
            inst.live_status_baseline,
            Some(Status::Error),
            "the first check must seed the baseline for subsequent comparisons"
        );
    }

    #[test]
    fn test_update_status_with_metadata_restamps_on_genuine_transition() {
        // Once a live baseline is established, a real status change still
        // restamps normally (no regression from the #2690 fix).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::hours(2));
        inst.last_accessed_at = Some(Utc::now() - chrono::Duration::hours(2));

        let before = Utc::now();
        inst.update_status_with_metadata(None);
        let after = Utc::now();

        // No real tmux session exists, so detection resolves to Error: a
        // genuine transition away from the established Idle baseline.
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.idle_entered_at, None);
        let last_accessed = inst.last_accessed_at.expect("must be restamped");
        assert!(last_accessed >= before && last_accessed <= after);
        assert_eq!(inst.live_status_baseline, Some(Status::Error));
    }

    #[test]
    fn test_update_status_with_metadata_twice_same_status_never_restamps() {
        // Two consecutive calls that both detect the same status (no real
        // tmux session, so detection is deterministically Error) must
        // neither restamp: not the first (baseline already matches), and
        // not the second either.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Error);
        inst.status = Status::Error;
        let sentinel_idle = Some(Utc::now() - chrono::Duration::hours(3));
        let sentinel_accessed = Some(Utc::now() - chrono::Duration::hours(3));
        inst.idle_entered_at = sentinel_idle;
        inst.last_accessed_at = sentinel_accessed;

        inst.update_status_with_metadata(None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, sentinel_idle,
            "first call must not restamp"
        );
        assert_eq!(
            inst.last_accessed_at, sentinel_accessed,
            "first call must not restamp"
        );

        inst.update_status_with_metadata(None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, sentinel_idle,
            "second call must not restamp"
        );
        assert_eq!(
            inst.last_accessed_at, sentinel_accessed,
            "second call must not restamp"
        );
    }

    #[test]
    fn test_update_status_with_metadata_twice_different_statuses_both_restamp() {
        // Two back-to-back genuine transitions must both restamp, and the
        // baseline must update between calls so the second comparison is
        // against the first call's result, not the original value.
        //
        // Archiving short-circuits update_status_with_metadata_inner before
        // it touches `status` (see the `is_archived()` guard), which lets
        // this test fully control the "detected" status for two
        // independent calls without a real tmux session.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Running;

        let before1 = Utc::now();
        inst.update_status_with_metadata(None);
        let after1 = Utc::now();
        assert_eq!(
            inst.status,
            Status::Running,
            "archived guard preserves status"
        );
        assert_eq!(inst.idle_entered_at, None, "non-idle transition clears it");
        let first_stamp = inst
            .last_accessed_at
            .expect("first transition must restamp");
        assert!(first_stamp >= before1 && first_stamp <= after1);
        assert_eq!(inst.live_status_baseline, Some(Status::Running));

        inst.status = Status::Idle;
        let before2 = Utc::now();
        inst.update_status_with_metadata(None);
        let after2 = Utc::now();
        assert_eq!(inst.status, Status::Idle);
        let second_idle = inst
            .idle_entered_at
            .expect("second transition must restamp");
        assert!(second_idle >= before2 && second_idle <= after2);
        assert!(
            second_idle >= first_stamp,
            "second restamp must not be older than the first"
        );
        assert_eq!(inst.live_status_baseline, Some(Status::Idle));
    }

    #[test]
    fn test_instance_new_seeds_live_status_baseline_none() {
        // #2690 follow-up. A freshly constructed Instance has no live
        // observation yet. Seeding `Some(Status::Idle)` here was the root
        // cause of the false restamp on the first poll after
        // `finalize_launch`: the baseline claimed "I saw Idle" while
        // `finalize_launch` (and other post-construction status writers)
        // advanced `status` to Starting without touching baseline, so the
        // wrapper's next call read `baseline=Some(Idle) != status=Starting`
        // and stamped `last_accessed_at` on a session no user ever
        // touched. Uniform `None` matches the disk-load path (which is
        // `None` because of `#[serde(skip)]`) so both paths seed on the
        // first poll rather than restamping.
        let inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.live_status_baseline, None);
    }

    #[test]
    fn test_first_poll_after_status_write_does_not_fabricate_last_accessed_at() {
        // #2690 follow-up regression lock. Reproduces the pre-fix bug:
        // `Instance::new` used to seed `live_status_baseline: Some(Idle)`,
        // then a post-construction status writer (like `finalize_launch`)
        // advanced `status` to Starting WITHOUT touching baseline. The
        // very first poll then read a stale baseline, treated the
        // detected-status mismatch as a "genuine transition", and stamped
        // `last_accessed_at` for a session the user never touched.
        //
        // Under the fix (`Instance::new` seeds `None`), the first poll
        // seeds baseline from the detected status and does NOT restamp;
        // `last_accessed_at` stays `None` for a truly untouched session.
        //
        // The assertion is guard-only: whatever `update_status_with_metadata_inner`
        // resolves `status` to (`Error` in the no-tmux path, could be a
        // different value if `_inner` grows a new branch), the wrapper's
        // `baseline.is_some_and(...)` guard at
        // [`Self::update_status_with_metadata`] short-circuits on
        // `baseline == None`, so no restamp path runs. A future refactor
        // of `_inner` cannot silently weaken the lock; only a change to
        // the wrapper's guard shape can.
        let mut inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.last_accessed_at, None, "fixture invariant");
        // Simulate any post-construction status writer, `finalize_launch`
        // being the canonical one (`src/session/instance.rs`).
        inst.status = Status::Starting;

        inst.update_status_with_metadata(None);

        assert_eq!(
            inst.last_accessed_at, None,
            "first poll must not fabricate a `last_accessed_at` on an untouched session"
        );
    }

    #[test]
    fn test_merge_from_tui_takes_max_last_accessed() {
        let earlier = Utc::now() - chrono::Duration::minutes(5);
        let later = Utc::now();

        let mut stored = Instance::new("a", "/tmp/a");
        stored.last_accessed_at = Some(later);
        let mut src = Instance::new("a", "/tmp/a");
        src.id = stored.id.clone();
        src.last_accessed_at = Some(earlier);
        stored.merge_from_tui(&src);
        assert_eq!(
            stored.last_accessed_at,
            Some(later),
            "peer's freshest activity timestamp must survive a stale TUI src"
        );

        let mut stored = Instance::new("b", "/tmp/b");
        stored.last_accessed_at = Some(earlier);
        let mut src = Instance::new("b", "/tmp/b");
        src.id = stored.id.clone();
        src.last_accessed_at = Some(later);
        stored.merge_from_tui(&src);
        assert_eq!(stored.last_accessed_at, Some(later));
    }

    #[test]
    fn test_merge_from_tui_does_not_touch_user_action_fields() {
        let peer_archived = Some(Utc::now());
        let peer_favorited = Some(Utc::now() - chrono::Duration::minutes(2));
        let peer_snoozed = Some(Utc::now() + chrono::Duration::minutes(30));
        let peer_pinned = Some(Utc::now() - chrono::Duration::minutes(1));

        let mut stored = Instance::new("session", "/tmp/test");
        stored.archived_at = peer_archived;
        stored.favorited_at = peer_favorited;
        stored.snoozed_until = peer_snoozed;
        stored.pinned_at = peer_pinned;
        stored.title = "peer-renamed".to_string();
        stored.group_path = "peer/group".to_string();
        stored.agent_session_id = Some("daemon-sid".to_string());
        stored.notify_on_waiting = Some(true);
        stored.base_branch_override = Some("upstream/main".to_string());

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.archived_at = None;
        src.favorited_at = None;
        src.snoozed_until = None;
        src.pinned_at = None;
        src.title = "tui-stale".to_string();
        src.group_path = "tui/stale".to_string();
        src.agent_session_id = Some("tui-stale-sid".to_string());
        src.notify_on_waiting = Some(false);
        src.base_branch_override = None;

        stored.merge_from_tui(&src);

        assert_eq!(stored.archived_at, peer_archived);
        assert_eq!(stored.favorited_at, peer_favorited);
        assert_eq!(stored.snoozed_until, peer_snoozed);
        assert_eq!(stored.pinned_at, peer_pinned);
        assert_eq!(stored.title, "peer-renamed");
        assert_eq!(stored.group_path, "peer/group");
        assert_eq!(stored.agent_session_id.as_deref(), Some("daemon-sid"));
        assert_eq!(stored.notify_on_waiting, Some(true));
        assert_eq!(
            stored.base_branch_override.as_deref(),
            Some("upstream/main")
        );
    }

    #[test]
    fn test_merge_from_tui_preserves_immutable_identity() {
        let mut stored = Instance::new("session", "/tmp/test");
        let immutable_id = stored.id.clone();
        let immutable_path = stored.project_path.clone();
        let immutable_created = stored.created_at;

        let mut src = Instance::new("renamed", "/tmp/different");
        src.id = "different-id".to_string();

        stored.merge_from_tui(&src);

        assert_eq!(stored.id, immutable_id);
        assert_eq!(stored.project_path, immutable_path);
        assert_eq!(stored.created_at, immutable_created);
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_creating() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Creating;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Creating)) => {}
            other => panic!("expected Transient(Creating), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_deleting() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Deleting;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Deleting)) => {}
            other => panic!("expected Transient(Deleting), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_structured() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.view = View::Structured;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::StructuredView) => {}
            other => panic!("expected StructuredView, got {other:?}"),
        }
    }

    /// Real-tmux integration: an alive pane yields AlreadyAlive with no
    /// status/start_time mutations. Skipped if tmux isn't installed.
    // Serialized: this test creates and kills a real tmux session. Unserialized
    // it can kill the shared server's last session while a `#[serial]` peer's
    // `new-session` is connecting, which fails that peer with "server exited
    // unexpectedly" (and its own skip-on-failure fallback silently masks the
    // same race in the other direction).
    #[test]
    #[serial_test::serial]
    fn test_ensure_pane_ready_alive_pane_is_noop() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }

        let mut inst = Instance::new("ensure_alive_test", "/tmp/test");
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "60",
            ])
            .status();
        if !created.map(|s| s.success()).unwrap_or(false) {
            eprintln!("tmux new-session failed; skipping");
            return;
        }
        crate::tmux::refresh_session_cache();

        inst.status = Status::Running;
        let prev_start = inst.last_start_time;
        let prev_status = inst.status;

        let outcome = inst.ensure_pane_ready().expect("ensure_pane_ready ok");
        assert_eq!(outcome, EnsureReadyOutcome::AlreadyAlive);
        assert_eq!(inst.last_start_time, prev_start);
        assert_eq!(inst.status, prev_status);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
    }

    #[test]
    fn test_idle_age_returns_none_for_non_idle() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Running;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(60));
        // A Running session never has an idle age, even if a stale
        // `idle_entered_at` timestamp is sitting around (e.g. a transition
        // that bumped from Idle → Running but missed the cleanup path).
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_idle_age_returns_none_when_no_timestamp() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = None;
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_idle_age_returns_positive_duration() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(5));
        let age = inst.idle_age().expect("idle age should be present");
        // Allow generous slack so the test isn't flaky on slow CI.
        assert!(age.as_secs() >= 4 && age.as_secs() <= 30);
    }

    #[test]
    fn test_idle_age_clamps_negative_to_none() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        // Future timestamp (clock skew, hand-crafted state). `to_std()` on a
        // negative `chrono::Duration` returns Err, which we map to None so
        // the freshness logic sees "fully decayed" rather than panicking
        // or treating the session as freshly stopped.
        inst.idle_entered_at = Some(Utc::now() + chrono::Duration::seconds(60));
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_all_agents_have_yolo_support() {
        for agent in crate::agents::AGENTS {
            assert!(
                agent.yolo.is_some(),
                "Agent '{}' should have YOLO mode configured",
                agent.name
            );
        }
    }

    #[test]
    fn test_yolo_mode_helper() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_yolo_mode());

        inst.yolo_mode = true;
        assert!(inst.is_yolo_mode());

        inst.yolo_mode = false;
        assert!(!inst.is_yolo_mode());
    }

    #[test]
    fn test_yolo_mode_without_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sandboxed());

        inst.yolo_mode = true;
        assert!(inst.is_yolo_mode());
        assert!(!inst.is_sandboxed());
    }

    #[test]
    #[serial_test::serial]
    fn test_yolo_envvar_command_is_quoted() {
        // EnvVar values containing JSON must be shell-escaped to prevent
        // the inner bash from expanding special characters ({, *, ").
        let result = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        assert_eq!(result, r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#);
    }

    #[test]
    fn test_yolo_envvar_survives_suspend_wrapper() {
        // The full chain: format_env_var_prefix -> wrap_command_ignore_suspend
        // must preserve the JSON value through both quoting layers.
        // Single quotes from shell_escape are escaped by wrap_command_ignore_suspend
        // via the '\'' technique, which correctly round-trips through the shell.
        let cmd = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        let wrapped = wrap_command_ignore_suspend(&cmd);
        // The inner single quotes from shell_escape become '\'' in the outer wrapper
        assert!(
            wrapped.contains(r#"OPENCODE_PERMISSION='\''{"*":"allow"}'\'' opencode"#),
            "wrapped command should contain the escaped env var assignment: {}",
            wrapped,
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_prepend_exports_does_not_double_exec() {
        // Regression: `wrap_command_ignore_suspend` always emits a string
        // starting with `exec` (since #757). `prepend_exports` MUST NOT add
        // another `exec`, because bash interprets `exec exec <cmd>` as
        // "exec a binary literally named `exec`", fails with exit 127, and
        // kills the pane on every sandboxed launch. zsh-on-macOS happens
        // to tolerate the double-exec, which is why this regression hid
        // for several days after #757 merged. See PR #819.
        std::env::set_var("SHELL", "/bin/bash");
        let wrapped = wrap_command_ignore_suspend("docker exec -it container claude");
        assert!(
            wrapped.starts_with("exec "),
            "test invariant: wrapped must start with `exec ` (else this test \
             is misaligned with wrap_command_ignore_suspend's contract): {}",
            wrapped,
        );

        let exports = vec![
            "export TERM='xterm-256color'".to_string(),
            "export COLORTERM='truecolor'".to_string(),
        ];
        let session_cmd = prepend_exports(&exports, wrapped);

        assert!(
            !session_cmd.contains("exec exec"),
            "session cmd must not contain `exec exec` -- bash exits 127 on it: {}",
            session_cmd,
        );

        // Empty exports must pass through unchanged.
        let wrapped2 = wrap_command_ignore_suspend("docker exec -it container claude");
        assert_eq!(prepend_exports(&[], wrapped2.clone()), wrapped2);
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_starts_with_exec() {
        // All wrapped commands must start with `exec` so that the tmux
        // default shell (which may be fish/nu) replaces itself with the
        // POSIX wrapper. Without this, fish stays as the pane process and
        // #{pane_current_command} reports "fish", triggering false restarts
        // on reattach. See #757.
        let original = std::env::var("SHELL").ok();
        for shell in &["/bin/bash", "/bin/zsh", "/usr/bin/fish", "/usr/bin/nu"] {
            std::env::set_var("SHELL", shell);
            let wrapped = wrap_command_ignore_suspend("claude");
            assert!(
                wrapped.starts_with("exec "),
                "SHELL={}: wrapped command must start with 'exec': {}",
                shell,
                wrapped,
            );
        }
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_posix_shell_uses_login() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/bin/zsh");
        let wrapped = wrap_command_ignore_suspend("claude");
        // POSIX shell: should use -lc for version-manager PATHs
        assert!(
            wrapped.contains("-lc"),
            "POSIX shell should use -lc: {}",
            wrapped,
        );
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_fish_skips_login() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/usr/bin/fish");
        let wrapped = wrap_command_ignore_suspend("claude");
        // Fish: should use -c (no -l) because bash's login scripts
        // won't have fish's PATH setup.
        assert!(
            wrapped.starts_with("exec bash -c "),
            "fish shell should produce 'exec bash -c ...': {}",
            wrapped,
        );
        assert!(
            !wrapped.contains("-lc"),
            "fish shell should NOT use -lc: {}",
            wrapped,
        );
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_nu_skips_login() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/usr/bin/nu");
        let wrapped = wrap_command_ignore_suspend("claude");
        assert!(
            wrapped.starts_with("exec bash -c "),
            "nu shell should produce 'exec bash -c ...': {}",
            wrapped,
        );
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    // Additional tests for is_sandboxed
    #[test]
    fn test_is_sandboxed_without_sandbox_info() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sandboxed());
    }

    #[test]
    fn test_is_sandboxed_with_disabled_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.sandbox_info = Some(SandboxInfo {
            enabled: false,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(!inst.is_sandboxed());
    }

    #[test]
    fn test_is_sandboxed_with_enabled_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(inst.is_sandboxed());
    }

    // Tests for get_tool_command
    #[test]
    fn test_get_tool_command_default_claude() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        assert_eq!(inst.get_tool_command(), "claude");
    }

    #[test]
    fn test_get_tool_command_opencode() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "opencode".to_string();
        assert_eq!(inst.get_tool_command(), "opencode");
    }

    #[test]
    fn test_get_tool_command_codex() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        assert_eq!(inst.get_tool_command(), "codex");
    }

    #[test]
    fn test_get_tool_command_gemini() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "gemini".to_string();
        assert_eq!(inst.get_tool_command(), "gemini");
    }

    #[test]
    fn test_get_tool_command_unknown_tool() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "unknown".to_string();
        assert_eq!(inst.get_tool_command(), "bash");
    }

    #[test]
    fn test_get_tool_command_custom_command() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "claude --resume abc123".to_string();
        assert_eq!(inst.get_tool_command(), "claude --resume abc123");
    }

    // Tests for Status enum
    #[test]
    fn test_status_default() {
        let status = Status::default();
        assert_eq!(status, Status::Idle);
    }

    #[test]
    fn test_status_serialization() {
        let statuses = vec![
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: Status = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    // Tests for WorktreeInfo
    #[test]
    fn test_worktree_info_serialization() {
        let info = WorktreeInfo {
            branch: "feature/test".to_string(),
            main_repo_path: "/home/user/repo".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: WorktreeInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(info.branch, deserialized.branch);
        assert_eq!(info.main_repo_path, deserialized.main_repo_path);
        assert_eq!(info.managed_by_aoe, deserialized.managed_by_aoe);
    }

    // Tests for SandboxInfo
    #[test]
    fn test_sandbox_info_serialization() {
        let info = SandboxInfo {
            enabled: true,
            container_id: Some("abc123".to_string()),
            image: "myimage:latest".to_string(),
            container_name: "test_container".to_string(),
            extra_env: Some(vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        let deserialized: SandboxInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(info.enabled, deserialized.enabled);
        assert_eq!(info.container_id, deserialized.container_id);
        assert_eq!(info.image, deserialized.image);
        assert_eq!(info.container_name, deserialized.container_name);
        assert_eq!(info.extra_env, deserialized.extra_env);
    }

    #[test]
    fn test_sandbox_info_minimal_serialization() {
        // Required fields: enabled, image, container_name
        let json = r#"{"enabled":false,"image":"test-image","container_name":"test"}"#;
        let info: SandboxInfo = serde_json::from_str(json).unwrap();

        assert!(!info.enabled);
        assert_eq!(info.image, "test-image");
        assert_eq!(info.container_name, "test");
        assert!(info.container_id.is_none());
    }

    // Tests for Instance serialization
    #[test]
    fn test_instance_serialization_roundtrip() {
        let mut inst = Instance::new("Test Project", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.group_path = "work/clients".to_string();
        inst.command = "claude --resume xyz".to_string();

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(inst.id, deserialized.id);
        assert_eq!(inst.title, deserialized.title);
        assert_eq!(inst.project_path, deserialized.project_path);
        assert_eq!(inst.group_path, deserialized.group_path);
        assert_eq!(inst.tool, deserialized.tool);
        assert_eq!(inst.command, deserialized.command);
    }

    #[test]
    fn test_instance_serialization_skips_runtime_fields() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.last_error_check = Some(std::time::Instant::now());
        inst.last_start_time = Some(std::time::Instant::now());
        inst.last_error = Some("test error".to_string());

        let json = serde_json::to_string(&inst).unwrap();

        // Runtime fields should not appear in JSON
        assert!(!json.contains("last_error_check"));
        assert!(!json.contains("last_start_time"));
        assert!(!json.contains("last_error"));
    }

    #[test]
    fn test_instance_acp_acp_session_id_roundtrip() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        inst.agent_name = Some("codex".to_string());
        inst.agent_model = Some("gpt-5".to_string());
        inst.acp_session_id = Some("acp-uuid-1234".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("\"view\":\"structured\""));
        assert!(json.contains("agent_name"));
        assert!(json.contains("agent_model"));
        assert!(json.contains("acp_session_id"));
        let deserialized: Instance = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.view, View::Structured);
        assert_eq!(deserialized.agent_name, Some("codex".to_string()));
        assert_eq!(deserialized.agent_model, Some("gpt-5".to_string()));
        assert_eq!(
            deserialized.acp_session_id,
            Some("acp-uuid-1234".to_string())
        );

        // None should not be serialized.
        let mut inst2 = Instance::new("Test", "/tmp/test");
        inst2.view = View::Structured;
        let json2 = serde_json::to_string(&inst2).unwrap();
        assert!(!json2.contains("acp_session_id"));
    }

    #[test]
    fn test_instance_with_worktree_info() {
        let mut inst = Instance::new("Test", "/tmp/worktree");
        inst.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/main".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert!(deserialized.worktree_info.is_some());
        let wt = deserialized.worktree_info.unwrap();
        assert_eq!(wt.branch, "feature/abc");
        assert!(wt.managed_by_aoe);
    }

    #[test]
    fn has_managed_worktree_or_workspace_covers_both_shapes() {
        // Single-repo aoe-managed worktree.
        let mut wt = Instance::new("WT", "/tmp/wt");
        wt.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/main".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        assert!(wt.has_managed_worktree_or_workspace());

        // Multi-repo workspace opting into cleanup (worktree_info is None).
        let mut ws = Instance::new("WS", "/tmp/ws/repo-a");
        ws.workspace_info = Some(WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: "/tmp/ws".to_string(),
            repos: vec![WorkspaceRepo {
                name: "repo-a".to_string(),
                source_path: "/tmp/src/repo-a".to_string(),
                branch: "feature/abc".to_string(),
                worktree_path: "/tmp/ws/repo-a".to_string(),
                main_repo_path: "/tmp/src/repo-a".to_string(),
                managed_by_aoe: true,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        assert!(ws.has_managed_worktree_or_workspace());

        // Workspace that opted out of cleanup: nothing to clean.
        if let Some(info) = ws.workspace_info.as_mut() {
            info.cleanup_on_delete = false;
        }
        assert!(!ws.has_managed_worktree_or_workspace());

        // Plain session: neither worktree nor workspace.
        let plain = Instance::new("Plain", "/tmp/plain");
        assert!(!plain.has_managed_worktree_or_workspace());
    }

    #[test]
    fn test_repo_path_prefers_worktree_main_repo() {
        let mut inst = Instance::new("Test", "/tmp/worktrees/feature");
        assert_eq!(inst.repo_path(), "/tmp/worktrees/feature");
        inst.worktree_info = Some(WorktreeInfo {
            branch: "feature".to_string(),
            main_repo_path: "/tmp/main-repo".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        assert_eq!(
            inst.repo_path(),
            "/tmp/main-repo",
            "worktree sessions group under the main repo, not the worktree dir"
        );
    }

    // Test generate_id function properties
    #[test]
    fn test_generate_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| Instance::new("t", "/t").id).collect();
        let unique_ids: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique_ids.len());
    }

    #[test]
    fn test_generate_id_format() {
        let inst = Instance::new("test", "/tmp/test");
        // ID should be 16 hex characters
        assert_eq!(inst.id.len(), 16);
        assert!(inst.id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_has_terminal_false_by_default() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.has_terminal());
    }

    #[test]
    fn test_has_terminal_true_when_created() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.terminal_info = Some(TerminalInfo { created: true });
        assert!(inst.has_terminal());
    }

    #[test]
    fn test_terminal_info_none_means_no_terminal() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(inst.terminal_info.is_none());
        assert!(!inst.has_terminal());
    }

    #[test]
    fn test_terminal_info_created_false_means_no_terminal() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.terminal_info = Some(TerminalInfo { created: false });
        assert!(!inst.has_terminal());
    }

    // Tests for agent_session_id field
    #[test]
    fn test_agent_session_id_none_by_default() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_agent_session_id_serialization() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.agent_session_id = Some("session-123".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.agent_session_id,
            Some("session-123".to_string())
        );
    }

    #[test]
    fn test_agent_session_id_skips_none() {
        let inst = Instance::new("test", "/tmp/test");
        let json = serde_json::to_string(&inst).unwrap();

        // agent_session_id should not appear in JSON when None
        assert!(!json.contains("agent_session_id"));
    }

    #[test]
    fn test_agent_session_id_defaults_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();

        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_build_claude_resume_flags_existing() {
        let session_id = "abc123-def456";
        let flags = build_resume_flags("claude", session_id, true);
        assert_eq!(flags, "--resume abc123-def456");
    }

    #[test]
    fn test_build_claude_session_id_flags_new() {
        let session_id = "abc123-def456";
        let flags = build_resume_flags("claude", session_id, false);
        assert_eq!(flags, "--session-id abc123-def456");
    }

    #[test]
    fn test_build_opencode_resume_flags() {
        let session_id = "session-789";
        let flags = build_resume_flags("opencode", session_id, false);
        assert_eq!(flags, "--session session-789");

        let flags = build_resume_flags("opencode", session_id, true);
        assert_eq!(flags, "--session session-789");
    }

    #[test]
    fn test_opencode_acquire_returns_none_for_deferred_capture() {
        let mut inst = Instance::new("Test", "/nonexistent/opencode/test");
        inst.tool = "opencode".to_string();

        let (session_id, is_existing) = inst.acquire_session_id();

        assert!(session_id.is_none());
        assert!(!is_existing);
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_persisted_opencode_session_id_reused() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("oc-session-42".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("oc-session-42".to_string()));
        assert!(is_existing);
    }

    // Test that instance with agent_session_id can be serialized and deserialized
    #[test]
    fn test_instance_with_agent_session_id_roundtrip() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("session-abc-123".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(inst.id, deserialized.id);
        assert_eq!(inst.title, deserialized.title);
        assert_eq!(inst.project_path, deserialized.project_path);
        assert_eq!(inst.tool, deserialized.tool);
        assert_eq!(inst.agent_session_id, deserialized.agent_session_id);
    }

    // Test: agent switch clears session ID
    #[test]
    fn test_agent_switch_clears_session_id() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("claude-session-123".to_string());

        // Simulate agent switch by clearing session ID
        inst.agent_session_id = None;
        inst.tool = "opencode".to_string();

        // Session ID should be None after switch
        assert!(inst.agent_session_id.is_none());
        assert_eq!(inst.tool, "opencode");
    }

    #[test]
    fn test_persisted_session_id_reused_when_already_set() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("session-42".to_string());

        // A persisted sid is returned as the session this instance owns. The
        // `--resume` vs `--session-id` decision (is_existing) is
        // transcript-dependent for Claude and is covered hermetically in
        // `verify_on_resume`; asserting it here would read the developer's real
        // `~/.claude`.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("session-42".to_string()));
    }

    #[test]
    fn test_persisted_session_id_reused_for_unsupported_agent() {
        // The cache-hit path is generic across agents; a persisted ID is
        // returned regardless of whether the agent supports resume yet.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.agent_session_id = Some("sess-99".to_string());

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("sess-99".to_string()));
        assert!(is_existing);
    }

    #[test]
    fn test_resume_with_arbitrary_session_id() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("invalid-session-id".to_string());

        // With an existing (persisted) session, should use --resume
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), true);
        assert_eq!(flags, "--resume invalid-session-id");

        // A fresh (no prior transcript) launch pins the id instead.
        let flags = build_resume_flags(&inst.tool, inst.agent_session_id.as_ref().unwrap(), false);
        assert_eq!(flags, "--session-id invalid-session-id");

        // The method returns the persisted id as the owned session. The
        // is_existing flag is transcript-dependent for Claude (see
        // `verify_on_resume`) and would read the real `~/.claude` here.
        let (session_id, _is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("invalid-session-id".to_string()));
    }

    #[test]
    fn test_build_resume_flags_rejects_invalid_id() {
        let flags = build_resume_flags("claude", "$(rm -rf /)", true);
        assert_eq!(flags, "");

        let flags = build_resume_flags("opencode", "id; echo pwned", false);
        assert_eq!(flags, "");
    }

    #[test]
    fn fork_intent_emits_resume_fork_session_and_pins_child() {
        let flags = build_fork_flags(
            "claude",
            "parent-1111-2222-3333-444444444444",
            "child-5555-6666-7777-888888888888",
        );
        assert_eq!(
            flags,
            "--resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
    }

    #[test]
    fn fork_flags_reject_invalid_ids() {
        assert_eq!(
            build_fork_flags("claude", "$(rm -rf /)", "child"),
            String::new()
        );
        assert_eq!(
            build_fork_flags("claude", "parent", "; echo pwned"),
            String::new()
        );
    }

    #[test]
    fn fork_flags_empty_for_unsupported_agent() {
        assert_eq!(build_fork_flags("cursor", "parent", "child"), String::new());
    }

    #[test]
    fn acquire_session_id_fork_pins_child_and_reports_fresh() {
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "claude".to_string();
        // The child id was pre-generated and stored in agent_session_id at
        // creation; the Fork intent carries the parent to resume from.
        inst.agent_session_id = Some("child-5555-6666-7777-888888888888".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-1111-2222-3333-444444444444".to_string(),
        };
        let mut cmd = "claude".to_string();
        let is_existing = inst.apply_session_flags(&mut cmd, "test");
        assert_eq!(
            cmd,
            "claude --resume parent-1111-2222-3333-444444444444 --fork-session --session-id child-5555-6666-7777-888888888888"
        );
        // A fork is a NEW session (not a resume-in-place), so report not-existing.
        assert!(!is_existing);
        // The child id we will resume from here on stays pinned in agent_session_id.
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some("child-5555-6666-7777-888888888888")
        );
    }

    #[test]
    fn fork_flags_for_codex_and_opencode() {
        // Codex: `fork <parent>` subcommand. child_id unused (codex mints its own).
        let codex = build_fork_flags("codex", "parent-id", "ignored-child");
        assert_eq!(codex, "fork parent-id");
        // OpenCode: resume the parent session and add --fork. agent mints new id.
        let oc = build_fork_flags("opencode", "parent-id", "ignored-child");
        assert_eq!(oc, "--session parent-id --fork");
    }

    #[test]
    fn fork_command_inserts_codex_subcommand_after_binary() {
        // codex fork must sit right after the binary, before other flags,
        // mirroring how codex `resume` is inserted as a subcommand.
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "codex".to_string();
        inst.agent_session_id = Some("child-ignored-by-codex".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-1234".to_string(),
        };
        let mut cmd = "codex --some-flag".to_string();
        inst.apply_session_flags(&mut cmd, "test");
        assert_eq!(cmd, "codex fork parent-1234 --some-flag");
    }

    #[test]
    fn fork_command_appends_opencode_flags() {
        let mut inst = Instance::new("Forked", "/tmp/x");
        inst.tool = "opencode".to_string();
        inst.agent_session_id = Some("child-ignored".to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: "parent-9999".to_string(),
        };
        let mut cmd = "opencode".to_string();
        inst.apply_session_flags(&mut cmd, "test");
        assert_eq!(cmd, "opencode --session parent-9999 --fork");
    }

    // Test: backwards compatibility - load old JSON without agent_session_id
    #[test]
    fn test_backwards_compatibility() {
        // Old JSON without agent_session_id field
        let old_json = r#"{"id":"old-session-123","title":"Old Session","project_path":"/home/user/old","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;

        let inst: Instance = serde_json::from_str(old_json).unwrap();

        // Should parse successfully with agent_session_id defaulting to None
        assert_eq!(inst.id, "old-session-123");
        assert_eq!(inst.title, "Old Session");
        assert_eq!(inst.project_path, "/home/user/old");
        assert_eq!(inst.tool, "claude");
        assert!(inst.agent_session_id.is_none());

        // After loading, can set a new session ID
        let mut inst = inst;
        inst.agent_session_id = Some("new-session-456".to_string());
        assert_eq!(inst.agent_session_id, Some("new-session-456".to_string()));
    }

    #[test]
    fn test_empty_string_deserializes_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":""}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_whitespace_string_deserializes_to_none() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":"   "}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert!(inst.agent_session_id.is_none());
    }

    #[test]
    fn test_valid_session_id_preserved() {
        let json = r#"{"id":"test123","title":"Test","project_path":"/tmp/test","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z","agent_session_id":"abc-123"}"#;
        let inst: Instance = serde_json::from_str(json).unwrap();
        assert_eq!(inst.agent_session_id, Some("abc-123".to_string()));
    }

    #[test]
    fn test_build_unknown_tool_resume_flags() {
        let flags = build_resume_flags("mistral", "session-123", false);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_build_pi_resume_flags() {
        let flags = build_resume_flags("pi", "019342ab-1234-7def-8901-abcdef012345", true);
        assert_eq!(flags, "--session 019342ab-1234-7def-8901-abcdef012345");

        let flags_new = build_resume_flags("pi", "019342ab-1234-7def-8901-abcdef012345", false);
        assert_eq!(flags_new, "--session 019342ab-1234-7def-8901-abcdef012345");
    }

    #[test]
    fn test_acquire_session_id_idempotence() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();

        let (first, first_existing) = inst.acquire_session_id();
        let (second, second_existing) = inst.acquire_session_id();

        // Repeated acquire yields a STABLE id. The first mint reports fresh; a
        // second acquire with no transcript on disk stays fresh-pinned (an empty
        // thread's sid is not resumable) but returns the same id, so a later
        // relaunch keeps `--session-id <same>` rather than a doomed `--resume`.
        assert!(first.is_some());
        assert!(!first_existing);
        assert!(!second_existing);
        assert_eq!(first, second);
    }

    #[test]
    fn apply_session_flags_returns_acquire_is_existing() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        // Fresh mint (no prior transcript): acquire reports a new session
        // (`--session-id`), so apply_session_flags returns false.
        let mut cmd = String::from("claude");
        assert!(!inst.apply_session_flags(&mut cmd, "test"));
        // A user-pinned resume intent reports an existing session
        // unconditionally, so apply_session_flags returns true.
        inst.resume_intent = ResumeIntent::Use("019342ab-1234-7def-8901-abcdef012345".to_string());
        let mut cmd2 = String::from("claude");
        assert!(inst.apply_session_flags(&mut cmd2, "test"));
    }

    #[test]
    fn start_with_size_opts_returns_skipped_for_structured() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        let outcome = inst.start_with_size_opts(None, false).unwrap();
        assert_eq!(outcome, LaunchSidOutcome::Skipped);
    }

    #[test]
    fn test_has_custom_command_empty() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_same_as_agent_binary() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "claude".to_string();
        assert!(!inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_override() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.command = "my-wrapper".to_string();
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_has_custom_command_unknown_tool() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "unknown_agent".to_string();
        inst.command = "some-binary".to_string();
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_status_hook_env_prefix_includes_hermes() {
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("hermes")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("settl")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("claude")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("opencode")),
            ""
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kiro")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
    }

    #[test]
    fn test_has_command_override_extra_args_only() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.extra_args = "--model opus".to_string();
        assert!(!inst.has_command_override());
        assert!(inst.has_custom_command());
    }

    #[test]
    fn test_expects_shell() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.expects_shell());

        inst.tool = "unknown-tool".to_string();
        inst.command = String::new();
        assert!(inst.expects_shell());

        inst.tool = "claude".to_string();
        inst.command = "bash".to_string();
        assert!(inst.expects_shell());

        inst.command = "my-agent".to_string();
        assert!(!inst.expects_shell());
    }

    #[test]
    fn test_status_unknown_serialization() {
        let status = Status::Unknown;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"unknown\"");
        let deserialized: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, Status::Unknown);
    }

    #[test]
    fn test_build_host_command_basic() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), &None)
            .unwrap();
        assert!(cmd.is_some());
        assert!(cmd.as_ref().unwrap().contains("codex"));
    }

    #[test]
    fn test_build_host_command_with_yolo() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.yolo_mode = true;
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        let agent = crate::agents::get_agent("codex").unwrap();
        match agent.yolo.as_ref().unwrap() {
            crate::agents::YoloMode::CliFlag(flag) => assert!(cmd_str.contains(flag)),
            crate::agents::YoloMode::EnvVar(key, _) => assert!(cmd_str.contains(key)),
            crate::agents::YoloMode::AlwaysYolo => {}
        }
    }

    #[test]
    fn test_build_host_command_with_resume() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("ses_abc123def456".to_string());
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("claude"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        assert!(cmd_str.contains("ses_abc123def456"));
        assert!(cmd_str.contains("--session-id") || cmd_str.contains("--resume"));
    }

    #[test]
    fn test_build_host_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("agy"));
    }

    #[test]
    fn test_build_host_command_kiro_uses_chat_subcommand() {
        // Regression: Kiro must launch via `kiro-cli chat` so the binary
        // accepts chat-scoped flags. Bare `kiro-cli` rejects --trust-all-tools.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), &None)
            .unwrap();
        assert!(cmd.unwrap().contains("kiro-cli chat"));
    }

    #[test]
    fn test_build_host_command_kiro_yolo_after_chat() {
        // YOLO flag must follow the `chat` subcommand, not precede it.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.yolo_mode = true;
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        let chat_pos = cmd_str
            .find("kiro-cli chat")
            .expect("chat subcommand present");
        let yolo_pos = cmd_str
            .find("--trust-all-tools")
            .expect("yolo flag present");
        assert!(
            yolo_pos > chat_pos,
            "--trust-all-tools must come after `kiro-cli chat`: {cmd_str}"
        );
    }

    #[test]
    fn test_build_host_command_custom_override_skips_subcommand() {
        // A user command override is passed through verbatim; AoE must not
        // inject a launch subcommand into it (the user is in full control).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.command = "kiro-cli chat --trust-all-tools".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();
        // Exactly one "chat" token (no doubled `chat chat`).
        assert_eq!(
            cmd_str.matches("chat").count(),
            1,
            "no duplicate subcommand: {cmd_str}"
        );
    }

    #[test]
    fn test_selected_agent_args_combines_command_and_extra() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.extra_args = "--agent custom-agent".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst.selected_agent_args(), "--agent"),
            Some("custom-agent".to_string())
        );

        // Agent named inside a command override is also found.
        let mut inst2 = Instance::new("test", "/tmp/test");
        inst2.tool = "kiro".to_string();
        inst2.command = "kiro-cli chat --agent custom-agent".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst2.selected_agent_args(), "--agent"),
            Some("custom-agent".to_string())
        );

        // extra_args is appended after the command override, so a per-session
        // --agent there wins over one baked into the override (last wins).
        let mut inst3 = Instance::new("test", "/tmp/test");
        inst3.tool = "kiro".to_string();
        inst3.command = "kiro-cli chat --agent from-command".to_string();
        inst3.extra_args = "--agent from-extra".to_string();
        assert_eq!(
            crate::agents::parse_selected_agent(&inst3.selected_agent_args(), "--agent"),
            Some("from-extra".to_string())
        );
    }

    #[test]
    fn test_build_host_custom_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        inst.command = "agy --some-flag".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("agy --some-flag"));
    }

    #[test]
    fn test_build_host_command_codex_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("codex"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("codex"));
    }

    #[test]
    fn test_build_host_command_color_env_is_limited_to_color_sensitive_agents() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "cursor".to_string();
        let (cmd, _) = inst
            .build_host_command(crate::agents::get_agent("cursor"), &None)
            .unwrap();
        let cmd_str = cmd.unwrap();

        assert!(!cmd_str.contains("env -u NO_COLOR"));
        assert!(!cmd_str.contains("TERM=xterm-256color"));
        assert!(!cmd_str.contains("COLORTERM=truecolor"));
    }

    #[test]
    fn test_pane_has_agent_content_bare_shell() {
        assert!(!pane_has_agent_content("$ ", "opencode"));
        assert!(!pane_has_agent_content("user@host:~$ ", "opencode"));
        assert!(!pane_has_agent_content("\n\n$ \n", "opencode"));
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_agent_content_stays_idle() {
        let content = "ctrl+p commands \u{2022} OpenCode 1.3.13+650d0db";
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, false, content, "opencode"),
            Status::Idle
        );
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_bare_prompt_is_error() {
        assert_eq!(
            resolve_detected_status(
                Status::Idle,
                false,
                true,
                false,
                "Welcome\nuser@host:~$ ",
                "opencode",
            ),
            Status::Error
        );
    }

    #[test]
    fn test_resolve_detected_status_shell_stale_unclear_is_unknown() {
        assert_eq!(
            resolve_detected_status(
                Status::Idle,
                false,
                true,
                false,
                "Restoring previous session...",
                "opencode",
            ),
            Status::Unknown
        );
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, false, "", "opencode"),
            Status::Unknown
        );
    }

    #[test]
    fn test_resolve_detected_status_keeps_hard_failures_as_error() {
        assert_eq!(
            resolve_detected_status(Status::Idle, true, false, false, "", "opencode"),
            Status::Error
        );
        assert_eq!(
            resolve_detected_status(Status::Idle, true, true, true, "", "opencode"),
            Status::Error
        );
    }

    #[test]
    fn test_resolve_detected_status_live_command_override_is_unknown() {
        assert_eq!(
            resolve_detected_status(Status::Idle, false, true, true, "$ ", "opencode"),
            Status::Unknown
        );
    }

    #[test]
    fn test_resolve_detected_status_command_override_agent_content_stays_idle() {
        // A wrapped agent (agent_command_override) whose pane still renders the
        // agent TUI must keep its detected Idle so on_idle / on_waiting status
        // hooks fire; previously the override masked every Idle to Unknown and
        // those hooks never ran (#2022).
        let content = "ctrl+p commands \u{2022} OpenCode 1.16.2";
        assert_eq!(
            resolve_detected_status(Status::Idle, false, false, true, content, "opencode"),
            Status::Idle
        );
    }

    #[test]
    fn test_pane_has_agent_content_agent_ui() {
        let opencode_idle = "ctrl+p commands \u{2022} OpenCode 1.3.13+650d0db";
        assert!(pane_has_agent_content(opencode_idle, "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_substantial_output() {
        let many_lines = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pane_has_agent_content(&many_lines, "vibe"));
    }

    #[test]
    fn test_pane_has_agent_content_empty() {
        assert!(!pane_has_agent_content("", "opencode"));
        assert!(!pane_has_agent_content("   \n  \n  ", "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_shell_prompt_at_end() {
        // Verbose MOTD followed by shell prompt should be detected as a
        // bare shell, not agent content, even with >5 lines.
        let motd_then_prompt = "Welcome to Ubuntu 22.04 LTS\n\
            System load:  0.5\n\
            Memory usage: 42%\n\
            Disk usage:   67%\n\
            Swap usage:   0%\n\
            Temperature:  45C\n\
            2 updates available\n\
            user@host:~$ ";
        assert!(!pane_has_agent_content(motd_then_prompt, "opencode"));

        // Same with # prompt (root)
        let root_prompt = "line1\nline2\nline3\nline4\nline5\nline6\n# ";
        assert!(!pane_has_agent_content(root_prompt, "opencode"));

        // Fish/zsh fancy prompt (❯)
        let fancy_prompt = "line1\nline2\nline3\nline4\nline5\nline6\n\u{276f}";
        assert!(!pane_has_agent_content(fancy_prompt, "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_short_tool_name() {
        // Short tool names like "pi" should NOT match substrings in
        // unrelated content (e.g., "api" contains "pi").
        assert!(!pane_has_agent_content("api endpoint ready", "pi"));
        assert!(!pane_has_agent_content("pipeline started", "pi"));

        // But "pi" as a standalone word should match.
        assert!(pane_has_agent_content("pi file saved", "pi"));
        assert!(pane_has_agent_content("done\npi>", "pi"));

        // Longer names like "opencode" should still match.
        assert!(pane_has_agent_content("OpenCode v1.0", "opencode"));
    }

    #[test]
    fn test_pane_has_agent_content_matches_agent_binary_alias() {
        assert!(pane_has_agent_content("agy ready", "antigravity"));
    }

    mod kill_terminal_if_dead {
        use super::*;

        fn tmux_available() -> bool {
            crate::tmux::tmux_command()
                .arg("-V")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        /// Manually create a tmux session under `name` with `remain-on-exit on`
        /// so the session survives the inner command's exit. Used to simulate
        /// the dead-pane state without going through `start_terminal`, which
        /// would also apply unrelated tmux options.
        fn spawn_remain_on_exit(name: &str, cmd: &str) {
            let output = crate::tmux::tmux_command()
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    name,
                    "-x",
                    "80",
                    "-y",
                    "24",
                    cmd,
                    ";",
                    "set-option",
                    "-p",
                    "-t",
                    name,
                    "remain-on-exit",
                    "on",
                ])
                .output()
                .expect("tmux new-session");
            assert!(
                output.status.success(),
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            crate::tmux::refresh_session_cache();
        }

        fn cleanup(name: &str) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", name])
                .output();
            crate::tmux::refresh_session_cache();
        }

        #[test]
        #[serial_test::serial]
        fn returns_false_when_no_session() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_missing", "/tmp");
            crate::tmux::refresh_session_cache();
            assert!(!inst.kill_terminal_if_dead().unwrap());
        }

        #[test]
        #[serial_test::serial]
        fn returns_false_when_pane_alive() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_alive", "/tmp");
            let name = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
            spawn_remain_on_exit(&name, "sleep 30");
            // Give tmux a moment to register the pane.
            std::thread::sleep(std::time::Duration::from_millis(200));

            let result = inst.kill_terminal_if_dead();
            cleanup(&name);

            assert!(!result.unwrap(), "live pane should not trigger a kill");
        }

        #[test]
        #[serial_test::serial]
        fn kills_dead_pane_session() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_dead", "/tmp");
            let name = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
            // `true` exits immediately; remain-on-exit keeps the session alive
            // with a dead pane (matches the production failure mode: shell
            // exited via Ctrl+D / `exit` / SIGHUP, session still listed).
            spawn_remain_on_exit(&name, "true");
            // Allow the pane to transition to dead.
            std::thread::sleep(std::time::Duration::from_millis(300));

            let session = inst.terminal_tmux_session().unwrap();
            assert!(
                session.exists(),
                "session should still exist via remain-on-exit"
            );
            assert!(
                session.is_pane_dead(),
                "pane should be dead after `true` exits"
            );

            let killed = inst.kill_terminal_if_dead().unwrap();
            assert!(
                killed,
                "kill_terminal_if_dead should return true for dead pane"
            );

            let session = inst.terminal_tmux_session().unwrap();
            assert!(!session.exists(), "session should be gone after kill");

            // Idempotent: second call on now-missing session returns false.
            assert!(
                !inst.kill_terminal_if_dead().unwrap(),
                "second call on missing session should return false"
            );

            cleanup(&name);
        }
    }

    mod resume_fallback {
        use super::super::{
            should_attempt_resume, Instance, LaunchSidOutcome, ResumeAttemptPolicy, ResumeIntent,
            SidPersistOutcome, StartOutcome, Status,
        };
        use serial_test::serial;
        use tempfile::tempdir;

        struct EnvVarGuard {
            key: &'static str,
            prev: Option<std::ffi::OsString>,
        }

        impl EnvVarGuard {
            fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
                let prev = std::env::var_os(key);
                std::env::set_var(key, value);
                Self { key, prev }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }

        struct TmuxSessionGuard(String);

        impl TmuxSessionGuard {
            fn create(inst: &Instance) -> Option<Self> {
                let tmux_available = crate::tmux::tmux_command()
                    .arg("-V")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if !tmux_available {
                    eprintln!("Skipping: tmux not available");
                    return None;
                }

                let session = inst.tmux_session().unwrap();
                session
                    .create(&inst.project_path, Some("sleep 60"))
                    .expect("create tmux session");
                Some(Self(session.name().to_string()))
            }
        }

        impl Drop for TmuxSessionGuard {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
                crate::tmux::refresh_session_cache();
            }
        }

        fn seed_opencode_db(db_path: &std::path::Path, sid: &str, project_path: &str) {
            let conn = rusqlite::Connection::open(db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    time_updated INTEGER NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                rusqlite::params![sid, project_path, 1_000_000_i64],
            )
            .unwrap();
        }

        #[test]
        fn no_sid_does_not_attempt_resume() {
            assert!(!should_attempt_resume(None, "claude"));
            assert!(!should_attempt_resume(Some(""), "claude"));
            assert!(!should_attempt_resume(Some("   "), "claude"));
        }

        #[test]
        fn invalid_sid_does_not_attempt_resume() {
            assert!(!should_attempt_resume(Some("bad id!"), "claude"));
            assert!(!should_attempt_resume(Some("path/slash"), "claude"));
            assert!(!should_attempt_resume(Some(&"x".repeat(257)), "claude"));
        }

        #[test]
        fn valid_sid_for_resume_supporting_agent_attempts() {
            assert!(should_attempt_resume(
                Some("11111111-1111-1111-1111-111111111111"),
                "claude"
            ));
            assert!(should_attempt_resume(Some("session_abc.123"), "opencode"));
            assert!(should_attempt_resume(Some("uuid-abc-123"), "codex"));
            assert!(should_attempt_resume(Some("uuid-abc-123"), "gemini"));
            assert!(should_attempt_resume(Some("uuid-abc-123"), "copilot"));
        }

        #[test]
        fn unsupported_agent_does_not_attempt_resume() {
            assert!(!should_attempt_resume(
                Some("11111111-1111-1111-1111-111111111111"),
                "cursor"
            ));
        }

        #[test]
        fn unknown_tool_does_not_attempt_resume() {
            assert!(!should_attempt_resume(Some("uuid-abc-123"), "nonexistent"));
        }

        #[test]
        fn launch_sid_outcome_carries_emitted_sid() {
            let outcome = LaunchSidOutcome::Existing {
                sid: "11111111-1111-1111-1111-111111111111".to_string(),
            };

            match outcome {
                LaunchSidOutcome::Existing { sid } => {
                    assert_eq!(sid, "11111111-1111-1111-1111-111111111111");
                }
                other => panic!("expected Existing, got {other:?}"),
            }
        }

        #[test]
        fn start_with_resume_fallback_uses_launch_sid_for_probe_decision() {
            let source = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session/instance.rs"),
            )
            .unwrap();
            let start = source
                .find("pub(crate) fn start_with_resume_fallback")
                .unwrap();
            let end = source.find("pub fn ensure_pane_ready").unwrap();
            let fallback_source = &source[start..end];

            assert!(fallback_source.contains("let attempted_sid = match &outcome"));
            assert!(fallback_source.contains("LaunchSidOutcome::Existing { sid }"));
            assert!(
                !fallback_source.contains("should_attempt_resume(self.agent_session_id.as_deref()")
            );
            assert!(
                !fallback_source.contains("let stale_sid = self\n            .agent_session_id")
            );
        }

        #[test]
        fn resume_probe_failure_marks_before_cleanup() {
            let source = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session/instance.rs"),
            )
            .unwrap();
            let start = source
                .find("pub(crate) fn start_with_resume_fallback")
                .unwrap();
            let end = source.find("pub fn ensure_pane_ready").unwrap();
            let fallback_source = &source[start..end];
            let local_marker = fallback_source
                .find("self.resume_probe_failed_sid = Some(stale_sid.clone())")
                .unwrap();
            let persisted_marker = fallback_source
                .find("self.mark_resume_probe_failed(&profile, &stale_sid)")
                .unwrap();
            let cleanup = fallback_source.find("self.kill_clean()").unwrap();

            assert!(local_marker < cleanup);
            assert!(persisted_marker < cleanup);
        }

        #[test]
        #[serial]
        fn persist_session_to_storage_skips_on_cas_mismatch() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("cas-persist-mismatch").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.agent_session_id = Some("peer-wrote".to_string());
            let id = inst.id.clone();
            let xs = vec![inst];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let outcome = super::persist_session_to_storage(
                "cas-persist-mismatch",
                &id,
                "ours",
                Some("old"),
                &crate::file_watch::FileWatchService::noop(),
            );
            assert_eq!(outcome, super::SidWrite::Skipped);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id.as_deref(), Some("peer-wrote"));
        }

        #[test]
        #[serial]
        fn persist_session_to_storage_writes_on_cas_match() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("cas-persist-match").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.agent_session_id = Some("old".to_string());
            let id = inst.id.clone();
            let xs = vec![inst];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let outcome = super::persist_session_to_storage(
                "cas-persist-match",
                &id,
                "new",
                Some("old"),
                &crate::file_watch::FileWatchService::noop(),
            );
            assert_eq!(outcome, super::SidWrite::Applied);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id.as_deref(), Some("new"));
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial]
        async fn persist_session_to_storage_delivers_notification_to_in_process_subscriber() {
            use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
            use std::sync::Arc;
            use std::time::Duration;
            use tokio::time::timeout;

            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            // Seed via a noop service so the seed write produces no Local
            // notification on the live service constructed below; the
            // subscriber attaches AFTER the seed so any seed-side kernel
            // echo is filtered out by the subscribe boundary.
            let seed_storage =
                crate::session::storage::Storage::new_unwatched("sid-persist-notify").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.agent_session_id = Some("old".to_string());
            let id = inst.id.clone();
            let on_disk = vec![inst.clone()];
            seed_storage
                .update(|i, g| {
                    *i = on_disk.clone();
                    *g = crate::session::GroupTree::new_with_groups(&on_disk, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();
            drop(seed_storage);

            let svc: Arc<FileWatchService> = FileWatchService::new().expect("init");
            let profile_dir = crate::session::get_profile_dir_path("sid-persist-notify").unwrap();
            let sessions_path = profile_dir.join("sessions.json");
            let (mut rx, _handle) = svc
                .subscribe_channel(
                    WatchSpec {
                        dir: profile_dir,
                        matcher: FileMatcher::Exact(sessions_path),
                        debounce: Some(Duration::from_millis(75)),
                    },
                    4,
                )
                .expect("subscribe");

            let outcome = super::persist_session_to_storage(
                "sid-persist-notify",
                &id,
                "new-sid",
                Some("old"),
                &svc,
            );
            assert_eq!(outcome, super::SidWrite::Applied);

            // Wiring assertion: the in-process subscriber receives a delivery
            // for sessions.json within sub-tick budget. The Local-first
            // invariant of notify_local_change vs the kernel echo is locked
            // separately by file_watch::tests::
            // notify_local_change_delivers_local_first_and_tolerates_late_kernel_echo;
            // the dispatcher's debounce window may coalesce both into a
            // kernel-sourced slot on platforms where canonicalize latency
            // exceeds the kernel pipeline.
            let evt = timeout(Duration::from_millis(2_500), rx.recv())
                .await
                .expect("delivery within budget")
                .expect("dispatcher alive");
            assert_eq!(
                evt.path.file_name().and_then(|n| n.to_str()),
                Some("sessions.json"),
                "subscriber must observe the sessions.json write"
            );
        }
        #[test]
        #[serial]
        fn reconcile_from_disk_picks_up_peer_persist() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("reconcile-test").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "reconcile-test".to_string();
            inst.agent_session_id = Some("old-sid".to_string());
            let id = inst.id.clone();
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            // Simulate a peer CLI `set-session-id` write to disk.
            let _ = super::persist_session_to_storage(
                "reconcile-test",
                &id,
                "new-sid",
                Some("old-sid"),
                &crate::file_watch::FileWatchService::noop(),
            );

            assert_eq!(inst.agent_session_id.as_deref(), Some("old-sid"));
            inst.reconcile_from_disk();
            assert_eq!(inst.agent_session_id.as_deref(), Some("new-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_from_disk_preserves_before_start_env() {
            // `before_start_env` is `#[serde(skip)]`, so the disk snapshot has
            // it empty. reconcile_from_disk (run before every launch) must carry
            // the live host-minted cache forward, or an already-running
            // container would re-mint on every relaunch.
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("reconcile-before-start").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "reconcile-before-start".to_string();
            inst.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "img".to_string(),
                container_name: "ctr".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            // Stamp a freshly-minted value into the in-memory cache only.
            inst.sandbox_info.as_mut().unwrap().before_start_env =
                vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())];

            inst.reconcile_from_disk();

            assert_eq!(
                inst.sandbox_info.as_ref().unwrap().before_start_env,
                vec![("GH_TOKEN".to_string(), "ghs_minted".to_string())],
                "live before_start_env must survive the pre-launch disk reload"
            );
        }

        #[test]
        #[serial]
        fn reconcile_from_disk_picks_up_peer_clear() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("reconcile-clear").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "reconcile-clear".to_string();
            inst.agent_session_id = Some("old-sid".to_string());
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            storage
                .update(|i, _g| {
                    i[0].agent_session_id = None;
                    Ok(())
                })
                .unwrap();

            inst.reconcile_from_disk();
            assert_eq!(inst.agent_session_id, None);
        }

        #[test]
        #[serial]
        fn resume_intent_use_returns_pinned_sid_without_observation() {
            let mut inst = Instance::new("intent-use", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("user-pinned"));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some("user-pinned"));
        }

        #[test]
        #[serial]
        fn resume_intent_use_overrides_observation() {
            let mut inst = Instance::new("intent-use-override", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("observed".to_string());
            inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("user-pinned"));
            assert!(is_existing);
        }

        #[test]
        #[serial]
        fn resume_intent_cleared_for_claude_generates_fresh_uuid() {
            let mut inst = Instance::new("intent-cleared-claude", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("observed".to_string());
            inst.resume_intent = ResumeIntent::Cleared;

            let (sid, is_existing) = inst.acquire_session_id();
            assert!(
                sid.is_some(),
                "Claude must always have a session id at launch"
            );
            assert!(!is_existing, "Cleared intent must not report is_existing");
            assert_ne!(sid.as_deref(), Some("observed"));
            assert_eq!(inst.agent_session_id, sid);
        }

        #[test]
        #[serial]
        fn resume_intent_cleared_for_opencode_returns_none() {
            let mut inst = Instance::new("intent-cleared-opencode", "/tmp/x");
            inst.tool = "opencode".to_string();
            inst.agent_session_id = Some("observed".to_string());
            inst.resume_intent = ResumeIntent::Cleared;

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid, None);
            assert!(!is_existing);
            assert_eq!(inst.agent_session_id, None);
        }

        #[test]
        #[serial]
        fn resume_intent_default_uses_observed() {
            let mut inst = Instance::new("intent-default", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("observed".to_string());
            inst.resume_intent = ResumeIntent::Default;

            // Default intent returns the observed sid as the owned session. The
            // is_existing flag is transcript-dependent for Claude (covered in
            // `verify_on_resume`); asserting it here would read the real
            // `~/.claude`.
            let (sid, _is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("observed"));
        }

        #[test]
        fn resume_intent_serde_round_trip() {
            for intent in [
                ResumeIntent::Default,
                ResumeIntent::Use("abc".to_string()),
                ResumeIntent::Cleared,
                ResumeIntent::Fork {
                    from: "some-parent-id".to_string(),
                },
            ] {
                let json = serde_json::to_string(&intent).unwrap();
                let back: ResumeIntent = serde_json::from_str(&json).unwrap();
                assert_eq!(intent, back);
            }
        }

        #[test]
        fn resume_intent_wire_format_is_pinned() {
            assert_eq!(
                serde_json::to_string(&ResumeIntent::Default).unwrap(),
                r#"{"kind":"Default"}"#
            );
            assert_eq!(
                serde_json::to_string(&ResumeIntent::Use("abc".to_string())).unwrap(),
                r#"{"kind":"Use","value":"abc"}"#
            );
            assert_eq!(
                serde_json::to_string(&ResumeIntent::Cleared).unwrap(),
                r#"{"kind":"Cleared"}"#
            );
            // `Fork` is a struct variant, so its `value` is a nested object
            // (`{"from":...}`), not a bare string like `Use`. This shape is
            // persisted to `sessions.json`; pin it so a refactor cannot break
            // deserialisation of saved fork seeds.
            assert_eq!(
                serde_json::to_string(&ResumeIntent::Fork {
                    from: "some-parent-id".to_string()
                })
                .unwrap(),
                r#"{"kind":"Fork","value":{"from":"some-parent-id"}}"#
            );
        }

        #[test]
        fn resume_intent_missing_in_json_defaults_to_default() {
            let mut inst = Instance::new("title", "/tmp/x");
            inst.resume_intent = ResumeIntent::Use("X".to_string());
            let json: serde_json::Value = serde_json::to_value(&inst).unwrap();
            let mut obj = json.as_object().unwrap().clone();
            obj.remove("resume_intent");
            let stripped = serde_json::Value::Object(obj);

            let back: Instance = serde_json::from_value(stripped).unwrap();
            assert_eq!(back.resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn reconcile_from_disk_picks_up_peer_resume_intent() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("intent-reconcile").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "intent-reconcile".to_string();
            inst.resume_intent = ResumeIntent::Default;
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            storage
                .update(|i, _g| {
                    i[0].resume_intent = ResumeIntent::Use("peer-pinned".to_string());
                    Ok(())
                })
                .unwrap();

            assert_eq!(inst.resume_intent, ResumeIntent::Default);
            inst.reconcile_from_disk();
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("peer-pinned".to_string())
            );
        }

        /// Seed a Claude transcript on disk for `sid` under `project_path`, in
        /// the exact location `acquire_session_id`'s existence check reads
        /// (`CLAUDE_CONFIG_DIR` or `$HOME/.claude`). The probe tests below drive
        /// the `--resume` cascade, which acquire now only takes when a stored
        /// sid has a real prior conversation on disk; an empty thread's sid
        /// launches fresh-pinned (`--session-id`) instead. Callers must have set
        /// `HOME` to a temp dir first.
        fn seed_claude_transcript(project_path: &str, sid: &str) {
            let home = std::env::var("CLAUDE_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dirs::home_dir().expect("home dir").join(".claude"));
            let canonical = std::fs::canonicalize(project_path)
                .unwrap_or_else(|_| std::path::PathBuf::from(project_path));
            let dir =
                home.join("projects")
                    .join(crate::session::capture::encode_claude_project_path(
                        &canonical.to_string_lossy(),
                    ));
            std::fs::create_dir_all(&dir).expect("create claude project dir");
            std::fs::write(dir.join(format!("{sid}.jsonl")), "seed\n").expect("write transcript");
        }

        fn write_sidecar(instance_id: &str, sid: &str) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let base = crate::hooks::hook_base_path();
            if !base.exists() {
                std::fs::create_dir_all(&base).expect("create hook base dir");
            }
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
                .expect("set hook base mode 0700");
            let dir =
                crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .expect("set hook instance mode 0700");
            std::fs::write(dir.join("session_id"), sid).unwrap();
            dir
        }

        fn seed_disk_for_sidecar_test(profile: &str, inst: &Instance) {
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let snapshot = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![snapshot.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&snapshot),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();
        }

        const SIDECAR_TEST_FRESH_UUID: &str = "11111111-2222-4333-8444-555555555555";

        #[test]
        #[serial]
        fn reconcile_sidecar_adopts_fresh_sid_for_claude_default() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-adopt";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Default;
            inst.agent_session_id = Some("stale-disk-sid".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some(SIDECAR_TEST_FRESH_UUID)
            );
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(
                on_disk.agent_session_id.as_deref(),
                Some(SIDECAR_TEST_FRESH_UUID)
            );
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_noop_when_tool_not_claude() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-noop-tool";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "opencode".to_string();
            inst.resume_intent = ResumeIntent::Default;
            inst.agent_session_id = Some("disk-sid".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_noop_when_intent_use() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-noop-use";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Use("user-pinned".to_string());
            inst.agent_session_id = Some("disk-sid".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_noop_when_intent_cleared() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-noop-cleared";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Cleared;
            inst.agent_session_id = Some("disk-sid".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_noop_when_sid_in_retroactive_excludes() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-noop-excluded";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Default;
            inst.agent_session_id = Some("disk-sid".to_string());
            inst.retroactive_capture_excludes
                .insert(SIDECAR_TEST_FRESH_UUID.to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_noop_when_sidecar_absent() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-absent";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Default;
            inst.agent_session_id = Some("disk-sid".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            inst.reconcile_sidecar_into_disk();

            assert_eq!(inst.agent_session_id.as_deref(), Some("disk-sid"));
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("disk-sid"));
        }

        #[test]
        #[serial]
        fn reconcile_sidecar_reloads_on_cas_skip() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "sidecar-cas-skip";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.tool = "claude".to_string();
            inst.resume_intent = ResumeIntent::Default;
            inst.agent_session_id = Some("memory-baseline".to_string());
            seed_disk_for_sidecar_test(profile, &inst);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            storage
                .update(|i, _g| {
                    i[0].agent_session_id = Some("peer-wrote-this".to_string());
                    Ok(())
                })
                .unwrap();

            let dir = write_sidecar(&inst.id, SIDECAR_TEST_FRESH_UUID);

            inst.reconcile_sidecar_into_disk();
            std::fs::remove_dir_all(&dir).ok();

            assert_eq!(inst.agent_session_id.as_deref(), Some("peer-wrote-this"));
            let on_disk = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|i| i.id == inst.id)
                .unwrap();
            assert_eq!(on_disk.agent_session_id.as_deref(), Some("peer-wrote-this"));
        }

        #[test]
        fn acquire_default_with_no_observation_generates_uuid_for_claude() {
            let mut inst = Instance::new("acquire-default-fresh", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Default;

            let (sid, is_existing) = inst.acquire_session_id();
            assert!(sid.is_some());
            assert!(!is_existing);
            assert_eq!(inst.agent_session_id, sid);
        }

        #[test]
        #[serial]
        fn acquire_session_id_default_picks_up_retroactive_capture() {
            let temp = tempdir().unwrap();
            let project_path = temp.path().join("opencode-project");
            std::fs::create_dir_all(&project_path).unwrap();
            let project_path = project_path.to_string_lossy().to_string();
            let db_path = temp.path().join("opencode.db");
            let captured_sid = "ses_retroactive_capture";
            seed_opencode_db(&db_path, captured_sid, &project_path);
            let _opencode_db = EnvVarGuard::set("OPENCODE_DB", &db_path);

            let mut inst = Instance::new("retroactive-opencode", &project_path);
            inst.tool = "opencode".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Default;
            let Some(_tmux) = TmuxSessionGuard::create(&inst) else {
                return;
            };

            let (sid, is_existing) = inst.acquire_session_id();

            assert_eq!(sid.as_deref(), Some(captured_sid));
            assert!(is_existing);
            assert_eq!(inst.agent_session_id.as_deref(), Some(captured_sid));
        }

        mod verify_on_resume {
            use super::*;
            use crate::session::capture::encode_claude_project_path;
            use std::fs;
            use std::time::{Duration, SystemTime};
            use tempfile::{tempdir, TempDir};

            struct ClaudeHomeGuard {
                prev_home: Option<String>,
                prev_xdg: Option<String>,
                prev_claude: Option<String>,
            }

            impl ClaudeHomeGuard {
                fn set(temp: &TempDir) -> Self {
                    let prev_home = std::env::var("HOME").ok();
                    let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
                    let prev_claude = std::env::var("CLAUDE_CONFIG_DIR").ok();
                    std::env::set_var("HOME", temp.path());
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
                    std::env::set_var("CLAUDE_CONFIG_DIR", temp.path().join(".claude"));
                    Self {
                        prev_home,
                        prev_xdg,
                        prev_claude,
                    }
                }
            }

            impl Drop for ClaudeHomeGuard {
                fn drop(&mut self) {
                    restore_or_remove("HOME", self.prev_home.take());
                    restore_or_remove("XDG_CONFIG_HOME", self.prev_xdg.take());
                    restore_or_remove("CLAUDE_CONFIG_DIR", self.prev_claude.take());
                }
            }

            fn restore_or_remove(key: &str, prev: Option<String>) {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }

            fn write_jsonl_with_mtime(path: &std::path::Path, mtime: SystemTime) {
                fs::write(path, "").unwrap();
                let f = fs::File::options().write(true).open(path).unwrap();
                f.set_times(fs::FileTimes::new().set_modified(mtime))
                    .unwrap();
            }

            #[test]
            #[serial]
            fn supersedes_stale_claude_sid_after_clear() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2291-claude-bascule";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let stale = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
                let fresh = "11111111-2222-3333-4444-555555555555";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{stale}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{fresh}.jsonl")),
                    now - Duration::from_secs(10),
                );

                let mut inst = Instance::new("verify-claude-bascule", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn no_bascule_when_claude_stored_matches_freshest() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2291-claude-steady";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let live = "ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{live}.jsonl")),
                    now - Duration::from_secs(10),
                );

                let mut inst = Instance::new("verify-claude-steady", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(live.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(live));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(live));
            }

            // An empty Claude thread killed before its first prompt has a
            // stored sid but no transcript on disk. `claude --resume <sid>`
            // would fail for it every time (the "resume failed for sid ...;
            // preserved for explicit retry" loop), so acquire must launch it as
            // a fresh pinned session (`--session-id <sid>`, is_existing=false)
            // while keeping the id stable for a later first prompt.
            #[test]
            #[serial]
            fn stored_sid_without_transcript_launches_fresh_pinned() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2291-no-jsonl";
                let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";

                let mut inst = Instance::new("verify-claude-no-jsonl", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stored.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(stored));
                assert!(
                    !is_existing,
                    "a stored sid with no transcript must launch fresh-pinned, not --resume"
                );
                assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
            }

            // Regression guard for the existence-only transcript check: an idle
            // but real conversation whose jsonl is older than the 5-minute
            // live-capture window must still resume. The mtime scan returns
            // nothing (stale), so acquire falls through to the transcript check,
            // which is age-agnostic and confirms the sid is resumable.
            #[test]
            #[serial]
            fn stored_sid_with_stale_transcript_still_resumes() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-stale-transcript";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let stored = "12121212-3434-5656-7878-9a9a9a9a9a9a";
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{stored}.jsonl")),
                    SystemTime::now() - Duration::from_secs(3600),
                );

                let mut inst = Instance::new("verify-claude-stale", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stored.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(stored));
                assert!(
                    is_existing,
                    "a real (if idle) transcript on disk must resume with --resume"
                );
                assert_eq!(inst.agent_session_id.as_deref(), Some(stored));
            }

            #[test]
            #[serial]
            fn unaffected_for_unsupported_tool() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let mut inst = Instance::new("verify-cursor", "/tmp/aoe-test-2291-cursor");
                inst.tool = "cursor".to_string();
                inst.agent_session_id = Some("stored-cursor-sid".to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some("stored-cursor-sid"));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some("stored-cursor-sid"));
            }

            // #2344: when several AoE Claude sessions share one cwd, the
            // most-recent jsonl in the shared `~/.claude/projects/<encoded-cwd>/`
            // dir is often a *peer* session's conversation. The mtime scan would
            // pick it and clobber this instance's stored sid on resume. The
            // per-instance hook sidecar is authoritative and must win over the
            // mtime guess: here the sidecar names the instance's own conversation
            // while a peer's jsonl is strictly fresher on disk.
            #[test]
            #[serial]
            fn sidecar_wins_over_fresher_peer_jsonl() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2344-shared-cwd";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                // `mine` is this instance's real conversation (named by its
                // sidecar). `peer` is a co-located peer's conversation that is
                // strictly freshest on disk. `stored` is a stale id distinct
                // from `mine`, so asserting `sid == mine` proves the sidecar
                // actively overrode the stored value rather than the stored
                // value passing through unchanged.
                let mine = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
                let peer = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
                let stored = "cccccccc-3333-4333-8333-cccccccccccc";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{peer}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let mut inst = Instance::new("verify-2344-shared-cwd", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stored.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let dir = super::write_sidecar(&inst.id, mine);
                let (sid, is_existing) = inst.acquire_session_id();
                std::fs::remove_dir_all(&dir).ok();

                // The authoritative sidecar overrides the stale stored sid;
                // the peer's fresher jsonl never wins.
                assert_eq!(sid.as_deref(), Some(mine));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // #2344 follow-up: a sandboxed Claude session must also consult the
            // sidecar. Its SessionStart hook writes through the
            // `/tmp/aoe-hooks/<id>` bind-mount onto the host path, so
            // `read_hook_session_id` reads it the same way a host session's is
            // read. Without the sidecar short-circuit the sandbox-aware mtime
            // branch would pick a peer's fresher jsonl in the shared cwd.
            #[test]
            #[serial]
            fn sidecar_consulted_for_sandboxed_claude() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2344-sandbox";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                // `stored` is distinct from the sidecar `mine`, so the assertion
                // proves the sidecar actively overrode the stale stored value.
                let mine = "eeeeeeee-5555-4555-8555-eeeeeeeeeeee";
                let peer = "ffffffff-6666-4666-8666-ffffffffffff";
                let stored = "dddddddd-7777-4777-8777-dddddddddddd";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{peer}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let mut inst = Instance::new("verify-2344-sandbox", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stored.to_string());
                inst.resume_intent = ResumeIntent::Default;
                inst.sandbox_info = Some(crate::session::SandboxInfo {
                    enabled: true,
                    container_id: None,
                    image: "test-image".to_string(),
                    container_name: "verify-2344-sandbox".to_string(),
                    extra_env: None,
                    custom_instruction: None,
                    before_start_env: Vec::new(),
                    container_workdir: None,
                });
                assert!(inst.is_sandboxed());

                let dir = super::write_sidecar(&inst.id, mine);
                let (sid, is_existing) = inst.acquire_session_id();
                std::fs::remove_dir_all(&dir).ok();

                // Sidecar (host-readable) names this instance's conversation, so
                // the peer's fresher jsonl does not win even though sandbox would
                // otherwise route through the container-aware mtime branch.
                assert_eq!(sid.as_deref(), Some(mine));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // Companion to the above: without a sidecar (e.g. a session resumed
            // after the 5-minute sidecar window) the mtime fallback still
            // applies, preserving the #2291 daemon-mode fix.
            #[test]
            #[serial]
            fn mtime_fallback_applies_without_sidecar() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2344-no-sidecar";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let stale = "cccccccc-3333-4333-8333-cccccccccccc";
                let fresh = "dddddddd-4444-4444-8444-dddddddddddd";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{stale}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{fresh}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let mut inst = Instance::new("verify-2344-no-sidecar", project_path);
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            // #2355: when a co-located stopped peer leaves a fresher jsonl in
            // the shared `~/.claude/projects/<encoded-cwd>/` dir, the mtime
            // fallback must skip the peer's sid. `build_exclusion_set` only
            // sees live tmux peers; `compose_exclusion_with_stopped_peers`
            // adds the stopped peer's sid from `sessions.json` so this
            // instance's own (older) jsonl wins.
            #[test]
            #[serial]
            fn mtime_fallback_skips_stopped_peer_sid() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2355-stopped-peer";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let mine = "11111111-1111-4111-8111-111111111111";
                let peer = "22222222-2222-4222-8222-222222222222";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{peer}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let profile = "verify-2355-stopped-peer";
                let mut peer_inst = Instance::new("stopped-peer-id", project_path);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "claude".to_string();
                peer_inst.agent_session_id = Some(peer.to_string());
                peer_inst.status = Status::Stopped;
                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let mut inst = Instance::new("verify-2355", project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(mine));
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // Companion to the above: same setup but the peer is archived
            // instead of stopped, exercising the `is_archived()` branch of
            // `compose_exclusion_with_stopped_peers`.
            #[test]
            #[serial]
            fn mtime_fallback_skips_archived_peer_sid() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2355-archived-peer";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let mine = "33333333-3333-4333-8333-333333333333";
                let peer = "44444444-4444-4444-8444-444444444444";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{peer}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let profile = "verify-2355-archived-peer";
                let mut peer_inst = Instance::new("archived-peer-id", project_path);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "claude".to_string();
                peer_inst.agent_session_id = Some(peer.to_string());
                peer_inst.archive();

                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let mut inst = Instance::new("verify-2355-archived", project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(mine));
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // Companion to the above: same setup but the peer carries the
            // default `Status::Idle` and is not archived, exercising the
            // `!inst.has_live_tmux_pane()` branch on its own. The peer has
            // never spawned a tmux pane in the test, so it counts as
            // pane-less even though its Status field does not flag it.
            #[test]
            #[serial]
            fn mtime_fallback_skips_pane_less_peer_sid() {
                let temp = tempdir().unwrap();
                let _guard = ClaudeHomeGuard::set(&temp);

                let project_path = "/tmp/aoe-test-2355-paneless-peer";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let mine = "55555555-5555-4555-8555-555555555555";
                let peer = "66666666-6666-4666-8666-666666666666";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{peer}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let profile = "verify-2355-paneless-peer";
                let mut peer_inst = Instance::new("paneless-peer-id", project_path);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "claude".to_string();
                peer_inst.agent_session_id = Some(peer.to_string());
                assert!(!peer_inst.is_archived());
                assert!(matches!(peer_inst.status, Status::Idle));
                assert!(!peer_inst.has_live_tmux_pane());

                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let mut inst = Instance::new("verify-2355-paneless", project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(mine));
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }
        }

        #[test]
        #[serial]
        fn persist_session_id_reloads_memory_on_skipped() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-skipped-reload").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-skipped-reload".to_string();
            inst.agent_session_id = Some("peer-wrote".to_string());
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            // Daemon thinks disk is "stale" but peer wrote "peer-wrote".
            // After persist_session_id, in-memory should converge on disk.
            inst.agent_session_id = Some("daemon-fresh".to_string());
            let _ = inst.persist_session_id(
                "persist-skipped-reload",
                Some("stale"),
                ResumeIntent::Default,
            );

            assert_eq!(inst.agent_session_id.as_deref(), Some("peer-wrote"));
        }

        #[test]
        #[serial]
        fn persist_session_id_atomic_writes_both_fields_on_match() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-atomic-match").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-atomic-match".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Cleared;
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
            let _ = inst.persist_session_id("persist-atomic-match", None, ResumeIntent::Cleared);

            let loaded = storage.load().unwrap();
            assert_eq!(
                loaded[0].agent_session_id.as_deref(),
                Some("019342ab-1234-7def-8901-abcdef012345"),
                "sid must persist atomically with intent promotion"
            );
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Default,
                "Cleared must auto-promote to Default in the same flock"
            );
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn persist_session_id_writes_none_atomically_when_sid_absent() {
            let temp = tempdir().unwrap();
            let profile = "persist-none-sid";
            let storage = crate::session::storage::Storage::new_for_test_path(
                profile,
                temp.path()
                    .join("profiles")
                    .join(profile)
                    .join("sessions.json"),
            );
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Default;
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            let outcome =
                inst.persist_session_id_with_storage(&storage, None, ResumeIntent::Default);

            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(inst.agent_session_id, None);
            let loaded = storage.load().unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].id, inst.id);
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn fork_intent_promotes_to_default_after_launch() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fork-promote";
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let mut inst = Instance::new("Forked", "/tmp/x");
            inst.tool = "claude".into();
            inst.source_profile = profile.into();
            inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".into());
            inst.resume_intent = ResumeIntent::Fork {
                from: "019342aa-2222-7eee-8fff-aaaabbbbcccc".into(),
            };
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            // Simulate the post-launch persist: expected_prior_intent is the Fork
            // we launched with; the child id is already pinned in agent_session_id.
            let expected_prior = inst.resume_intent.clone();
            let expected_sid = inst.agent_session_id.clone();
            let _ = inst.persist_session_id(profile, expected_sid.as_deref(), expected_prior);

            let reloaded = storage.load().unwrap();
            let disk = reloaded.iter().find(|i| i.id == inst.id).unwrap();
            assert_eq!(
                disk.resume_intent,
                ResumeIntent::Default,
                "Fork must auto-promote to Default after the first launch so restarts resume the child plainly"
            );
            assert_eq!(
                disk.agent_session_id.as_deref(),
                Some("019342ab-1234-7def-8901-abcdef012345")
            );
        }

        #[test]
        #[serial]
        fn persist_session_id_writes_sid_only_on_default_intent() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-default-intent").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-default-intent".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Default;
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
            let _ = inst.persist_session_id("persist-default-intent", None, ResumeIntent::Default);

            let loaded = storage.load().unwrap();
            assert_eq!(
                loaded[0].agent_session_id.as_deref(),
                Some("019342ab-1234-7def-8901-abcdef012345"),
            );
            assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Default,
                "Default intent path must not mutate in-memory intent",
            );
        }

        #[test]
        #[serial]
        fn persist_session_id_clears_resume_probe_failed_marker() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-clear-resume-marker")
                    .unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-clear-resume-marker".to_string();
            inst.agent_session_id = Some("019342aa-2222-7eee-8fff-aaaabbbbcccc".to_string());
            inst.resume_probe_failed_sid = Some("019342aa-2222-7eee-8fff-aaaabbbbcccc".to_string());
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
            let _ = inst.persist_session_id(
                "persist-clear-resume-marker",
                Some("019342aa-2222-7eee-8fff-aaaabbbbcccc"),
                ResumeIntent::Default,
            );

            let loaded = storage.load().unwrap();
            assert_eq!(
                loaded[0].agent_session_id.as_deref(),
                Some("019342ab-1234-7def-8901-abcdef012345"),
            );
            assert_eq!(loaded[0].resume_probe_failed_sid, None);
            assert_eq!(inst.resume_probe_failed_sid, None);
        }

        #[test]
        #[serial]
        fn persist_session_id_persists_sid_when_intent_cas_mismatches() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-intent-mismatch").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-intent-mismatch".to_string();
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Cleared;
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            storage
                .update(|i, _g| {
                    i[0].resume_intent = ResumeIntent::Use("peer-pinned".to_string());
                    Ok(())
                })
                .unwrap();

            inst.agent_session_id = Some("019342ab-1234-7def-8901-abcdef012345".to_string());
            let _ = inst.persist_session_id("persist-intent-mismatch", None, ResumeIntent::Cleared);

            let loaded = storage.load().unwrap();
            assert_eq!(
                loaded[0].agent_session_id.as_deref(),
                Some("019342ab-1234-7def-8901-abcdef012345"),
                "sid must persist even when peer rewrote intent",
            );
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Use("peer-pinned".to_string()),
                "peer's intent must survive when CAS mismatches",
            );
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("peer-pinned".to_string()),
                "memory must converge on peer's intent",
            );
        }

        #[test]
        #[serial]
        fn persist_session_id_skipped_reloads_both_fields() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("persist-skipped-reload-both")
                    .unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "persist-skipped-reload-both".to_string();
            inst.agent_session_id = Some("peer-sid".to_string());
            inst.resume_intent = ResumeIntent::Use("peer-pinned".to_string());
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();

            inst.agent_session_id = Some("daemon-fresh".to_string());
            inst.resume_intent = ResumeIntent::Cleared;
            let _ = inst.persist_session_id(
                "persist-skipped-reload-both",
                Some("stale"),
                ResumeIntent::Cleared,
            );

            assert_eq!(inst.agent_session_id.as_deref(), Some("peer-sid"));
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("peer-pinned".to_string()),
                "intent must reload from disk on sid CAS skip",
            );
        }

        #[cfg(feature = "serve")]
        #[test]
        #[serial]
        fn restart_outcome_for_acp_session_is_fresh() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let mut inst = Instance::new("acp_test", "/tmp/x");
            inst.view = crate::session::instance::View::Structured;
            inst.agent_session_id = Some("11111111-1111-1111-1111-111111111111".to_string());
            inst.tool = "claude".to_string();

            let outcome = inst
                .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                .unwrap();
            assert_eq!(outcome, StartOutcome::Fresh);
        }

        #[test]
        #[serial]
        fn fallback_marks_resume_failed_and_preserves_sid_when_pane_dies() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test").unwrap();

            let stale_sid = "11111111-1111-1111-1111-111111111111".to_string();
            let mut inst = Instance::new("fallback_dies_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test".to_string();
            inst.command = "/bin/false".to_string();
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;
            // Real prior conversation on disk so acquire takes the --resume path.
            seed_claude_transcript(&inst.project_path, &stale_sid);
            let id = inst.id.clone();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(
                outcome.unwrap(),
                StartOutcome::ResumeFailed {
                    sid: stale_sid.clone(),
                }
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
            assert_eq!(
                inst.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );
            assert_eq!(inst.status, Status::Error);
            assert_eq!(
                inst.last_error.as_deref(),
                Some(
                    format!("resume failed for sid {stale_sid}; preserved for explicit retry")
                        .as_str()
                )
            );
            assert!(inst.last_error_check.is_some());
            let loaded = storage.load().unwrap();
            let row = loaded.iter().find(|i| i.id == id).expect("instance");
            assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid.as_str()));
            assert_eq!(
                row.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );
        }

        #[test]
        #[serial]
        fn fallback_does_not_launch_fresh_when_command_would_live_without_stale_sid() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test-live").unwrap();

            let stale_sid = "22222222-2222-2222-2222-222222222222".to_string();
            let mut inst = Instance::new("fallback_lives_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test-live".to_string();
            inst.command = format!(
                "/bin/sh -c 'case \"$*\" in *{stale}*) exit 1 ;; esac; exec sleep 30' --",
                stale = stale_sid,
            );
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;
            // Real prior conversation on disk so acquire takes the --resume path.
            seed_claude_transcript(&inst.project_path, &stale_sid);

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(
                outcome.unwrap(),
                StartOutcome::ResumeFailed {
                    sid: stale_sid.clone(),
                }
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
            assert_eq!(
                inst.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );
            let loaded = storage.load().unwrap();
            let row = loaded.iter().find(|i| i.id == inst.id).expect("instance");
            assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid.as_str()));
            assert_eq!(
                row.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );
        }

        // #2609: `auto_resume_on_restart = false` must stop `--resume <sid>`
        // from ever reaching the launched command on the restart/reattach
        // path (`HonorAutoResumeSetting`), while leaving Send Message / Live
        // Send (`Allow`) unaffected.
        #[test]
        #[serial]
        fn auto_resume_on_restart_false_skips_stored_sid_and_launches_fresh() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let mut cfg = crate::session::config::Config::default();
            cfg.session.auto_resume_on_restart = false;
            crate::session::config::save_config(&cfg).unwrap();

            let storage = crate::session::storage::Storage::new_unwatched("fb-toggle-off").unwrap();

            let stale_sid = "44444444-4444-4444-4444-444444444444".to_string();
            let mut inst = Instance::new("fallback_toggle_off_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-toggle-off".to_string();
            // Would die if (and only if) `--resume <stale_sid>` reached the
            // command; with the toggle off it must never be passed, so this
            // process lives.
            inst.command = format!(
                "/bin/sh -c 'case \"$*\" in *{stale}*) exit 1 ;; esac; exec sleep 30' --",
                stale = stale_sid,
            );
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(
                None,
                true,
                ResumeAttemptPolicy::HonorAutoResumeSetting,
            );

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(outcome.unwrap(), StartOutcome::Fresh);
            assert_ne!(
                inst.agent_session_id.as_deref(),
                Some(stale_sid.as_str()),
                "toggle off must generate a fresh sid, not reuse the stale one"
            );
        }

        // #2609: Send Message / Live Send (`Allow`) must keep attempting resume
        // regardless of `auto_resume_on_restart`, so a dead pane still surfaces
        // `ResumeFailed` (proving `--resume <sid>` was passed) rather than
        // silently starting fresh and losing agent context.
        #[test]
        #[serial]
        fn allow_policy_still_attempts_resume_when_auto_resume_on_restart_is_false() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let mut cfg = crate::session::config::Config::default();
            cfg.session.auto_resume_on_restart = false;
            crate::session::config::save_config(&cfg).unwrap();

            let storage =
                crate::session::storage::Storage::new_unwatched("fb-allow-ignores").unwrap();

            let stale_sid = "55555555-5555-5555-5555-555555555555".to_string();
            let mut inst = Instance::new("fallback_allow_ignores_toggle_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-allow-ignores".to_string();
            inst.command = "/bin/false".to_string();
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;
            // Real prior conversation on disk so acquire takes the --resume path.
            seed_claude_transcript(&inst.project_path, &stale_sid);

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(
                outcome.unwrap(),
                StartOutcome::ResumeFailed {
                    sid: stale_sid.clone(),
                },
                "Allow must ignore auto_resume_on_restart=false and still attempt resume"
            );
        }

        // #2609 core bug: a sid whose resume probe already failed once must
        // never be retried automatically. Reproduces the reported infinite
        // loop (two consecutive `e`/`Enter` presses against the same doomed
        // sid) and proves the second attempt terminates it instead of
        // repeating `ResumeFailed` forever.
        #[test]
        #[serial]
        fn stale_probe_failed_sid_is_not_retried_on_next_attempt() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-loop-break").unwrap();

            let stale_sid = "66666666-6666-6666-6666-666666666666".to_string();
            let mut inst = Instance::new("fallback_loop_break_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-loop-break".to_string();
            inst.command = "/bin/false".to_string();
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;
            // Real prior conversation on disk so the FIRST attempt takes the
            // --resume path (and fails); the loop-breaker on the second attempt
            // then fires from the persisted marker, independent of the transcript.
            seed_claude_transcript(&inst.project_path, &stale_sid);

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            // First attempt: reproduces the pre-existing `ResumeFailed` path,
            // exactly like `fallback_marks_resume_failed_and_preserves_sid_when_pane_dies`.
            let first = inst
                .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                .unwrap();
            assert_eq!(
                first,
                StartOutcome::ResumeFailed {
                    sid: stale_sid.clone(),
                }
            );
            assert_eq!(
                inst.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );

            // Second attempt, same sid, same doomed command: on the pre-fix
            // tree this reproduces the reported bug (identical `ResumeFailed`
            // forever). The fix must instead skip the resume attempt and
            // start fresh.
            inst.kill_clean().unwrap();
            let second = inst
                .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                .unwrap();

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(
                second,
                StartOutcome::FreshAfterFailedResume {
                    sid: stale_sid.clone(),
                },
                "a sid that already failed a resume probe must not be retried automatically"
            );
            assert_ne!(
                inst.agent_session_id.as_deref(),
                Some(stale_sid.as_str()),
                "loop-breaker must generate a fresh sid instead of repeating the doomed one"
            );
            assert_eq!(
                inst.resume_probe_failed_sid, None,
                "loop-breaker's fresh launch clears the stale marker, matching ResumeIntent::Cleared semantics"
            );
        }

        #[test]
        #[serial]
        fn resume_failed_fires_when_pane_dies_inside_post_shell_grace_window() {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test-grace").unwrap();

            let stale_sid = "33333333-3333-3333-3333-333333333333".to_string();
            let mut inst = Instance::new("fallback_grace_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test-grace".to_string();
            inst.command = format!(
                "/bin/sh -c 'case \"$*\" in *{stale}*) exec sleep 1.2 ;; esac; exec sleep 30' --",
                stale = stale_sid,
            );
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;
            // Real prior conversation on disk so acquire takes the --resume path.
            seed_claude_transcript(&inst.project_path, &stale_sid);

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            match outcome {
                Ok(StartOutcome::ResumeFailed { sid }) => assert_eq!(sid, stale_sid),
                Ok(StartOutcome::Resumed) => panic!(
                    "Tier-1 grace shortcut returned Alive before the t=1200ms pane_dead: \
                     RESUME_PROBE_POST_SHELL_GRACE is too short. \
                     Real opencode crashes at ~1000ms; raise the grace constant."
                ),
                Ok(other) => panic!(
                    "Expected ResumeFailed or Resumed; got {other:?} (probe path is taking an unexpected branch)"
                ),
                Err(e) => panic!("resume failure should be a typed outcome, got: {e:#}"),
            }
            assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
            assert_eq!(
                inst.resume_probe_failed_sid.as_deref(),
                Some(stale_sid.as_str())
            );
        }
    }

    mod publish_captured_sid {
        use super::super::{publish_session_to_tmux_env, Instance, ResumeIntent};
        use serial_test::serial;
        use std::collections::HashSet;
        use tempfile::{tempdir, TempDir};

        const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";
        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";

        struct TmuxSession(String);

        impl TmuxSession {
            fn create(id: &str, title: &str) -> Self {
                Self::create_named(crate::tmux::Session::generate_name(id, title))
            }

            fn create_terminal(id: &str, title: &str) -> Self {
                Self::create_named(crate::tmux::TerminalSession::generate_name(id, title))
            }

            fn create_named(name: String) -> Self {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &name])
                    .output();
                let status = crate::tmux::tmux_command()
                    .args(["new-session", "-d", "-s", &name])
                    .status()
                    .expect("failed to spawn tmux");
                assert!(status.success(), "tmux new-session failed for {}", name);
                Self(name)
            }

            fn name(&self) -> &str {
                &self.0
            }
        }

        impl Drop for TmuxSession {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }

        fn skip_if_no_tmux() -> bool {
            if crate::tmux::tmux_command().arg("-V").output().is_err() {
                eprintln!("Skipping: tmux not available");
                return true;
            }
            false
        }

        fn isolate_home(temp: &TempDir) {
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        fn captured_env(name: &str) -> Option<String> {
            crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
        }

        fn instance_env(name: &str) -> Option<String> {
            crate::tmux::env::get_hidden_env(name, crate::tmux::env::AOE_INSTANCE_ID_KEY)
        }

        fn make_inst(profile: &str, title: &str) -> Instance {
            let mut inst = Instance::new(title, "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = profile.to_string();
            inst
        }

        fn seed_disk_row(profile: &str, inst: &Instance) {
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let on_disk = inst.clone();
            storage
                .update(|i, g| {
                    *i = vec![on_disk.clone()];
                    *g = crate::session::GroupTree::new_with_groups(
                        std::slice::from_ref(&on_disk),
                        &[],
                    )
                    .get_all_groups();
                    Ok(())
                })
                .unwrap();
        }

        #[test]
        #[serial]
        fn poller_publish_writes_terminal_session_env() {
            if skip_if_no_tmux() {
                return;
            }

            let mut inst = make_inst("publish-terminal", "tailscale-operator-followup");
            inst.terminal_info = Some(crate::session::TerminalInfo { created: true });
            let tmux = TmuxSession::create_terminal(&inst.id, &inst.title);
            inst.title = "renamed-after-terminal-create".to_string();

            assert_eq!(inst.tmux_env_session_name().as_deref(), Some(tmux.name()));
            assert!(tmux.name().starts_with(crate::tmux::TERMINAL_PREFIX));
            assert!(tmux.name().contains("tailscale-operator-f"));

            let agent_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            publish_session_to_tmux_env(tmux.name(), &inst.id, VALID_SID);

            assert!(captured_env(&agent_name).is_none());
            assert_eq!(instance_env(tmux.name()).as_deref(), Some(inst.id.as_str()));
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }

        #[test]
        #[serial]
        fn terminal_publish_feeds_exclusion_set_for_other_instances() {
            if skip_if_no_tmux() {
                return;
            }

            let mut peer = make_inst("publish-terminal-exclusion", "peer-terminal");
            peer.terminal_info = Some(crate::session::TerminalInfo { created: true });
            let tmux = TmuxSession::create_terminal(&peer.id, &peer.title);

            publish_session_to_tmux_env(tmux.name(), &peer.id, PEER_SID);

            let extra = HashSet::new();
            let other_exclusion =
                crate::session::capture::compose_exclusion("other-instance", &extra);
            assert!(other_exclusion.contains(PEER_SID));

            let own_exclusion = crate::session::capture::compose_exclusion(&peer.id, &extra);
            assert!(!own_exclusion.contains(PEER_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_env() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-applied";
            let mut inst = make_inst(profile, "fpaw");
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default);

            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_applied_writes_env_for_non_claude_tool() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-applied-opencode";
            let mut inst = make_inst(profile, "fpaw-oc");
            inst.tool = "opencode".to_string();
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some(VALID_SID),
                "non-claude tools must also publish AOE_CAPTURED_SESSION_ID at finalize"
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_skipped_disk_some_publishes_disk_value() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-skipped-some";
            let mut inst = make_inst(profile, "fpsdspd");
            inst.agent_session_id = Some(PEER_SID.to_string());
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, Some("stale"), ResumeIntent::Default);

            assert_eq!(inst.agent_session_id.as_deref(), Some(PEER_SID));
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(PEER_SID));
        }

        #[test]
        #[serial]
        fn finalize_publish_skipped_disk_none_unsets_env() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-skipped-none";
            let mut inst = make_inst(profile, "fpsdne");
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-leftover",
            )
            .unwrap();

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, Some("stale"), ResumeIntent::Default);

            assert!(inst.agent_session_id.is_none());
            assert!(captured_env(tmux.name()).is_none());
        }

        #[test]
        #[serial]
        fn finalize_publish_failed_leaves_env_unchanged() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-failed";
            let _ = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let mut inst = make_inst(profile, "fpfle");

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-untouched",
            )
            .unwrap();

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some("stale-untouched")
            );
            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some(VALID_SID),
                "memory must keep the daemon-set sid when persist returns Failed"
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_invalid_sid_skips_publish() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-invalid";
            let mut inst = make_inst(profile, "fpisp");
            inst.agent_session_id = None;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            crate::tmux::env::set_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                "stale-untouched",
            )
            .unwrap();

            inst.agent_session_id = Some("bad sid!".to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default);

            assert_eq!(
                captured_env(tmux.name()).as_deref(),
                Some("stale-untouched")
            );
        }

        #[test]
        #[serial]
        fn finalize_publish_promote_cleared_applied_uses_new_sid() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-promote";
            let mut inst = make_inst(profile, "fppca");
            inst.agent_session_id = None;
            inst.resume_intent = ResumeIntent::Cleared;
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Cleared);

            assert_eq!(inst.agent_session_id.as_deref(), Some(VALID_SID));
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
        }
    }

    fn instance_with_id(id: &str) -> Instance {
        let mut inst = Instance::new("tampered-id-test", "/tmp");
        inst.id = id.to_string();
        inst
    }

    #[test]
    fn start_with_size_opts_rejects_tampered_instance_id() {
        for poisoned in ["; rm -rf $HOME #", "../etc", ""] {
            let mut instance = instance_with_id(poisoned);
            let result = instance.start_with_size_opts(None, false);
            let err = match result {
                Ok(_) => panic!("must refuse tampered id at launch (id={poisoned:?})"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("AOE_INSTANCE_ID"),
                "error must surface validator failure for id={poisoned:?}, got: {err}"
            );
            assert!(
                !instance.tmux_session().map(|s| s.exists()).unwrap_or(false),
                "no tmux session must exist after refusal for id={poisoned:?}"
            );
        }
    }

    struct KillTmuxOnDrop(String);
    impl Drop for KillTmuxOnDrop {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// End-to-end regression for #1913 through the real status pipeline.
    ///
    /// A sandboxed (or hook-equipped) Claude session reports `running` from
    /// its hook while the pane is actually parked on a tool-approval prompt:
    /// the `Notification` -> waiting write gets clobbered by a running-mapped
    /// hook that re-fires during concurrent turn activity, and Claude keeps
    /// its live spinner rendered below the prompt. Before the fix the pipeline
    /// trusted the hook's `running` and showed green; now it captures the pane
    /// and reconciles to Waiting.
    #[test]
    #[serial_test::serial]
    fn update_status_reconciles_running_hook_to_waiting_on_claude_approval_prompt() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut inst = Instance::new("aoe_test_1913_wait", "/tmp");
        assert_eq!(inst.tool, "claude");

        // Pane shows the approval prompt with the live spinner still active
        // below it, the exact shape from the issue screenshot. The spinner
        // line means the bare pane detector would say Running, so a green
        // reading here can only come from reconciliation doing its job.
        let pane = "  Bash command\n    \
touch /tmp/aoe_test_1913/marker.txt\n    Create marker file\n  \
Do you want to proceed?\n  \u{276f} 1. Yes\n    \
2. Yes, and always allow access to this project\n    3. No\n  \
Esc to cancel \u{b7} Tab to amend \u{b7} ctrl+e to explain\n\
\u{2736} Herding\u{2026} (53s \u{b7} \u{2193} 7.0k tokens)\n";
        let pane_file = std::env::temp_dir().join(format!("aoe_test_1913_{}.txt", inst.id));
        std::fs::write(&pane_file, pane).expect("write pane fixture");

        let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
        let _guard = KillTmuxOnDrop(session_name.clone());
        // Single-quote the path so a temp dir with spaces or shell
        // metacharacters (e.g. macOS `$TMPDIR`) can't break the launch
        // command; embedded single quotes are closed/escaped/reopened.
        let quoted_pane_file =
            format!("'{}'", pane_file.to_string_lossy().replace('\'', r#"'\''"#));
        let launch = format!("cat {quoted_pane_file}; sleep 300");
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "40",
                &launch,
            ])
            .output()
            .expect("spawn tmux");
        assert!(
            created.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        // The clobbered hook state that produced the green row.
        use std::os::unix::fs::PermissionsExt;
        let base = crate::hooks::hook_base_path();
        if !base.exists() {
            std::fs::create_dir_all(&base).expect("create hook base dir");
        }
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("set hook base mode 0700");
        let dir = crate::hooks::hook_status_dir(&inst.id).expect("hook dir");
        std::fs::create_dir_all(&dir).expect("create hook dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("set hook instance mode 0700");
        std::fs::write(dir.join("status"), "running").expect("write status");
        assert_eq!(
            crate::hooks::read_hook_status(&inst.id),
            Some(Status::Running),
            "precondition: the raw hook signal is the Running that showed green"
        );

        // Wait for the pane to actually paint the cat output before the
        // authoritative read; a fixed sleep is flaky under parallel test load.
        let mut painted = false;
        for _ in 0..50 {
            let cap = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", &session_name])
                .output();
            if let Ok(out) = cap {
                if String::from_utf8_lossy(&out.stdout).contains("Do you want to proceed?") {
                    painted = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(painted, "approval prompt never painted into the tmux pane");

        // `Session::exists()` reads a process-global 2s session cache that a
        // concurrent test may have snapshotted before this session existed,
        // which surfaces as a spurious Error (and the 30s error latch would
        // then pin it). Refresh from live tmux now that the pane is painted so
        // the single authoritative read sees a true existence result.
        crate::tmux::refresh_session_cache();
        inst.update_status();

        std::fs::remove_file(&pane_file).ok();
        crate::hooks::cleanup_hook_status_dir(&inst.id);

        assert_eq!(
            inst.status,
            Status::Waiting,
            "Claude blocked on an approval prompt must reconcile Running -> Waiting (#1913)"
        );
    }
}
