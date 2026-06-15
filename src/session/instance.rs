//! Session instance definition and operations

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::containers::{self, ContainerRuntimeInterface, DockerContainer};
use crate::tmux;

use super::container_config;
use super::environment::{build_docker_env_args, shell_escape};
use super::poller::SessionPoller;

use crate::session::capture::{
    capture_codex_session_id, capture_gemini_session_id, capture_hermes_session_id,
    capture_pi_session_id, capture_vibe_session_id, claude_poll_fn, claude_poll_fn_sandboxed,
    codex_poll_fn, codex_poll_fn_sandboxed, gemini_poll_fn, gemini_poll_fn_sandboxed,
    generate_claude_session_id, hermes_poll_fn, hermes_poll_fn_sandboxed, is_valid_session_id,
    opencode_poll_fn, opencode_poll_fn_sandboxed, pi_poll_fn, pi_poll_fn_sandboxed,
    try_capture_codex_session_id_in_container, try_capture_gemini_session_id_in_container,
    try_capture_hermes_session_id_in_container, try_capture_opencode_session_id,
    try_capture_opencode_session_id_in_container, try_capture_pi_session_id_in_container,
    try_capture_vibe_session_id_in_container, validated_session_id, vibe_poll_fn,
    vibe_poll_fn_sandboxed,
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

/// Outcome of a `start_with_resume_fallback` cascade.
///
/// Failures (both tiers) propagate as `Err` so callers keep the existing
/// `Status::Error` + `last_error` path. Only successful outcomes are
/// enumerated; mirrors the `EnsureReadyOutcome` shape.
/// `last_error` the status poller stamps when a session's tmux pane is simply
/// absent (killed, exited, server reboot) and nothing more specific was
/// captured from the pane. The preview treats this as the calm "Stopped" case
/// rather than a red crash error, since it carries no diagnostic detail.
pub const TMUX_SESSION_GONE_ERROR: &str =
    "tmux session is gone. The agent process may have exited or been killed.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session ID was set and resume succeeded; pane is alive.
    Resumed,
    /// Resume attempt crashed the pane fast; the bad sid was cleared and a
    /// fresh start succeeded. Caller should surface this: the user's prior
    /// conversation is gone. `stale_sid` is the sid that was cleared.
    Restarted { stale_sid: String },
    /// No resume cascade ran. Either no prior sid, the agent doesn't support
    /// resume, the sid was invalid, the session is structured view-mode (no tmux
    /// pane), or the tmux session was already alive when entered (so
    /// `start_with_size_opts` was a no-op and the probe had nothing to
    /// detect). The pane is alive on return; whether a fresh launch
    /// actually occurred this call depends on the caller having killed
    /// any pre-existing pane first.
    Fresh,
}

/// What `start_with_size_opts` did with the agent's session id this call.
/// `start_with_resume_fallback` matches on `Existing` to gate the Tier-1
/// settle probe; without the gate, fresh Claude launches mislabel as
/// `StartOutcome::Resumed` because `acquire_session_id` always assigns a
/// UUID for Claude.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSidOutcome {
    /// `acquire_session_id` reused a prior sid: `ResumeIntent::Use(sid)`,
    /// observed `agent_session_id`, or retroactive-capture hit. The launch
    /// command embedded the agent's resume flag.
    Existing,
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
/// heavy projects. Healthy resumes pay this entire window once on Tier-1
/// (and again on Tier-2 if it fires); the pane is fully attachable for
/// the duration so the cost is purely in the synchronous restart path's
/// latency, not in agent responsiveness afterward.
const RESUME_PROBE_POST_SHELL_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Pure decision: should the resume-fallback cascade probe and potentially
/// retry without resume after the initial start? Extracted for unit-testability:
/// the cascade itself needs a real tmux session to test end-to-end.
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
    /// `stale_sid` is `Some` when the resume-fallback cascade fired during the
    /// respawn, meaning the agent's prior conversation is gone; callers should
    /// surface this so the user understands why history disappeared.
    Respawned { stale_sid: Option<String> },
    /// Tmux session did not exist and was started via the resume-fallback
    /// cascade. `stale_sid` is `Some` when the cascade fired (sid was on
    /// disk from a prior run, the agent crashed on it, and we cleared it
    /// before retrying), `None` for a healthy resume or fresh launch
    /// without resume. Same surface-to-user contract as `Respawned`.
    Started { stale_sid: Option<String> },
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

fn status_hook_env_prefix(instance_id: &str, agent: Option<&crate::agents::AgentDef>) -> String {
    let has_hooks = agent.is_some_and(|a| a.hook_config.is_some() || a.sidecar_hooks.is_some());

    if has_hooks {
        format!("AOE_INSTANCE_ID={} ", instance_id)
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
    /// `KEY=VALUE` pairs minted on the host by `host_hooks.before_start` when
    /// the container last came up. Injected into the container environment as
    /// inherited (leak-safe) entries by [`super::environment::collect_environment`].
    ///
    /// Runtime-only and secret: never serialized (so short-lived tokens never
    /// hit disk and a stale value never survives a restart) and re-minted on the
    /// next container come-up. See [`Instance::ensure_before_start_env`].
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
}

impl ResumeIntent {
    fn is_default(&self) -> bool {
        matches!(self, ResumeIntent::Default)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub title: String,
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

    /// User intent gating `acquire_session_id`. See `ResumeIntent` for
    /// semantics. Non-`Default` values (`Use`, `Cleared`) are written only
    /// by user-initiated CLI commands; daemon-internal paths demote to
    /// `Default` only (one-shot `Cleared` auto-promote, cascade Tier-1
    /// `Use(stale_sid)` downgrade), both CAS-guarded, so a daemon restart
    /// cannot silently undo a user-set pin.
    #[serde(default, skip_serializing_if = "ResumeIntent::is_default")]
    pub(crate) resume_intent: ResumeIntent,

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
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "View::is_terminal")]
    pub view: View,
    /// Optional structured view agent name (e.g., "claude-code", "aoe-agent",
    /// "gemini"). When None, the structured view picks the default for the
    /// session's tool.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Optional model id forwarded to aoe-agent (e.g., "claude-opus-4-7",
    /// "gpt-5", "llama3.3:ollama").
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    /// Agent-assigned ACP session id captured from `session/new`. When
    /// the agent advertises `agent_capabilities.load_session = true`
    /// (claude-agent-acp does), the next spawn calls `session/load`
    /// with this id so the agent reloads its on-disk transcript and
    /// the model retains context across `aoe serve` restarts. Cleared
    /// on acp_disable, session delete, or `session/load` failure.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,

    // Runtime state (not serialized)
    #[serde(skip)]
    pub last_error_check: Option<std::time::Instant>,
    #[serde(skip)]
    pub last_start_time: Option<std::time::Instant>,
    #[serde(skip)]
    pub last_error: Option<String>,
    #[serde(skip)]
    pub session_id_poller: Option<Arc<Mutex<SessionPoller>>>,

    /// Runtime-only set of session IDs that retroactive capture must NOT
    /// re-discover from on-disk artifacts. Populated by the resume-fallback
    /// cascade with the just-crashed session ID so the Tier-2 fresh start
    /// doesn't re-import the same bad sid via filesystem scan (opencode db,
    /// vibe meta.json, codex state, etc., all keep the bad session's row
    /// after the crash for several minutes).
    ///
    /// `#[serde(skip)]` is intentional. If the daemon dies between the
    /// cascade clearing the on-disk sid and the on-disk artifact decaying
    /// (~5-10 min), the next launch starts with this set empty and the
    /// freshly-spawned poller can re-import the bad sid once. The next
    /// `start_with_resume_fallback` then re-runs the cascade and clears it
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
        if is_subcommand {
            if let Some(space_pos) = cmd.find(' ') {
                let binary = &cmd[..space_pos];
                let flags = &cmd[space_pos..];
                *cmd = format!("{} {}{}", binary, resume_part, flags);
            } else {
                *cmd = format!("{} {}", cmd, resume_part);
            }
        } else {
            *cmd = format!("{} {}", cmd, resume_part);
        }
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

/// Publish a captured session ID to the tmux environment only.
///
/// Background threads (poller on_change) call this so that
/// `build_exclusion_set()` on other instances can see the captured ID
/// without racing with the TUI thread's `save()`.
fn publish_session_to_tmux_env(tmux_session_name: &str, session_id: &str) {
    if let Err(e) = crate::tmux::env::set_hidden_env(
        tmux_session_name,
        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
        session_id,
    ) {
        tracing::warn!(target: "session.store", "Failed to write captured session ID to tmux env: {}", e);
    }
}

impl Instance {
    pub fn new(title: &str, project_path: &str) -> Self {
        Self {
            id: generate_id(),
            title: title.to_string(),
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
            idle_dormant_since: None,
            pinned_at: None,
            scratch: false,
            worktree_info: None,
            workspace_info: None,
            sandbox_info: None,
            terminal_info: None,
            agent_session_id: None,
            resume_intent: ResumeIntent::Default,
            source_profile: String::new(),
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            base_branch_override: None,
            #[cfg(feature = "serve")]
            view: View::Terminal,
            #[cfg(feature = "serve")]
            agent_name: None,
            #[cfg(feature = "serve")]
            agent_model: None,
            #[cfg(feature = "serve")]
            acp_session_id: None,
            last_error_check: None,
            last_start_time: None,
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

    /// Same fields as `merge_post_start`. When `stale_sid` is `Some`, the
    /// resume-fallback cascade just invalidated that exact sid; clear
    /// `self.agent_session_id` only if it still matches the stale value, so
    /// a peer poller's freshly-discovered sid (landed between phase 2 and
    /// phase 3 of the restart) survives intact.
    pub fn merge_post_restart(&mut self, src: &Self, stale_sid: Option<&str>) {
        self.merge_post_start(src);
        if let Some(stale) = stale_sid {
            if self.agent_session_id.as_deref() == Some(stale) {
                self.agent_session_id = src.agent_session_id.clone();
            }
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
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
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
    /// on-demand from `/tmp/aoe-hooks/{id}/attention.json` so it picks up
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

    /// True if `snoozed_until` is set AND in the future. Expired snoozes
    /// return false so the row naturally rejoins the main sort on the next
    /// render; the stale timestamp stays on disk until the next mutation
    /// rewrites the session (harmless; `snoozed_until` is always compared
    /// against `Utc::now()`).
    pub fn is_snoozed(&self) -> bool {
        self.snoozed_until.map(|t| t > Utc::now()).unwrap_or(false)
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

    pub fn is_workspace(&self) -> bool {
        self.workspace_info.is_some()
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

    /// True when this session renders in the structured (ACP) view rather
    /// than a tmux pane. Always false when the `serve` feature is disabled,
    /// since the field doesn't exist and no session can be structured.
    pub fn is_structured(&self) -> bool {
        #[cfg(feature = "serve")]
        {
            self.view == View::Structured
        }
        #[cfg(not(feature = "serve"))]
        {
            false
        }
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
    /// a Claude UUID if applicable); `Default` falls back to the observed sid
    /// (`agent_session_id`), then retroactive capture, then fresh UUID for
    /// Claude.
    pub fn acquire_session_id(&mut self) -> (Option<String>, bool) {
        match &self.resume_intent {
            ResumeIntent::Use(sid) => {
                let sid = sid.clone();
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                let session_id = match self.tool.as_str() {
                    "claude" => Some(generate_claude_session_id()),
                    _ => None,
                };
                if let Some(ref id) = session_id {
                    self.agent_session_id = Some(id.clone());
                }
                return (session_id, false);
            }
            ResumeIntent::Default => {}
        }

        if self.agent_session_id.is_some() {
            return (self.agent_session_id.clone(), true);
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
        let exclusion = self.retroactive_capture_exclusion_set();
        let result: Option<String> = match self.tool.as_str() {
            "opencode" => {
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
            _ => None,
        };
        result.and_then(validated_session_id)
    }

    fn apply_session_flags(&mut self, cmd: &mut String, context: &str) -> bool {
        let (session_id, is_existing) = self.acquire_session_id();
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

    pub fn tmux_session(&self) -> Result<tmux::Session> {
        tmux::Session::new(&self.id, &self.title)
    }

    pub fn terminal_tmux_session(&self) -> Result<tmux::TerminalSession> {
        tmux::TerminalSession::new(&self.id, &self.title)
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
        let session = self.terminal_tmux_session()?;

        let is_new = !session.exists();
        if is_new {
            session.create_with_size(&self.project_path, None, size)?;
        }

        // Apply all configured tmux options to terminal sessions too
        if is_new {
            self.apply_terminal_tmux_options();
        }

        self.terminal_info = Some(TerminalInfo { created: true });

        Ok(())
    }

    pub fn kill_terminal(&self) -> Result<()> {
        let session = self.terminal_tmux_session()?;
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
        let session = self.terminal_tmux_session()?;
        if session.exists() && session.is_pane_dead() {
            let _ = session.kill();
            return Ok(true);
        }
        Ok(false)
    }

    pub fn container_terminal_tmux_session(&self) -> Result<tmux::ContainerTerminalSession> {
        tmux::ContainerTerminalSession::new(&self.id, &self.title)
    }

    pub fn has_container_terminal(&self) -> bool {
        self.container_terminal_tmux_session()
            .map(|s| s.exists())
            .unwrap_or(false)
    }

    /// `exists()` alone is insufficient: a pane can exist while its agent
    /// has died. Used by recovery, status polling, and TUI reload.
    pub fn has_live_tmux_pane(&self) -> bool {
        self.tmux_session()
            .map(|s| s.exists() && !s.is_pane_dead())
            .unwrap_or(false)
    }

    pub fn start_container_terminal_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
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

        let session = self.container_terminal_tmux_session()?;
        let is_new = !session.exists();
        if is_new {
            session.create_with_size(&self.project_path, Some(&session_cmd), size)?;
            self.apply_container_terminal_tmux_options();
        }

        Ok(())
    }

    pub fn kill_container_terminal(&self) -> Result<()> {
        let session = self.container_terminal_tmux_session()?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Container counterpart of [`Self::kill_terminal_if_dead`].
    pub fn kill_container_terminal_if_dead(&self) -> Result<bool> {
        let session = self.container_terminal_tmux_session()?;
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

    fn apply_container_terminal_tmux_options(&self) {
        let name = tmux::ContainerTerminalSession::generate_name(&self.id, &self.title);
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
        #[cfg(feature = "serve")]
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
        // CAS baseline). Covers Tier-1 and Tier-2 of the resume-fallback
        // cascade since both call this function.
        self.reconcile_from_disk();

        self.reconcile_sidecar_into_disk();

        // CAS baseline for `persist_session_id`. `build_launch_command` ->
        // `apply_session_flags` -> `acquire_session_id` may mutate
        // `agent_session_id` (Claude UUID generation); capture before that.
        let expected_prior_sid = self.agent_session_id.clone();
        let expected_prior_intent = self.resume_intent.clone();

        let profile = self.effective_profile();
        let (cmd, is_existing) = self.build_launch_command(skip_on_launch, &profile)?;

        tracing::debug!(target: "session.store",
            "container cmd: {}",
            cmd.as_ref().map_or("none".to_string(), |v| {
                super::environment::redact_env_values(v)
            })
        );

        if self.tool == "claude" {
            if let Ok(dir) = crate::hooks::hook_status_dir(&self.id) {
                let _ = std::fs::remove_file(dir.join("session_id"));
            }
        }

        session.create_with_size(&self.project_path, cmd.as_deref(), size)?;

        self.finalize_launch(
            session.name(),
            &profile,
            expected_prior_sid.as_deref(),
            expected_prior_intent,
        );

        Ok(if is_existing {
            LaunchSidOutcome::Existing
        } else {
            LaunchSidOutcome::Fresh
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
                        tracing::warn!(target: "session.store", "on_launch hook failed in container: {}", e);
                    }
                }
            }

            let base_cmd = if self.extra_args.is_empty() {
                self.get_tool_command().to_string()
            } else {
                format!("{} {}", self.get_tool_command(), self.extra_args)
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
            let docker_args = format!("{} -e AOE_INSTANCE_ID={}", env_info.docker_args, self.id);
            let env_part = format!("{} ", docker_args);
            let wrapped =
                wrap_command_ignore_suspend(&container.exec_command(Some(&env_part), &tool_cmd));
            (
                Some(prepend_exports(&env_info.exports, wrapped)),
                is_existing,
            )
        } else {
            self.build_host_command(agent, &on_launch_hooks)
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
        let hooks_enabled = super::profile_config::resolve_config_or_warn(&profile)
            .session
            .agent_status_hooks;
        if !hooks_enabled {
            return;
        }
        if let Some(sidecar) = agent.and_then(|a| a.sidecar_hooks.as_ref()) {
            // Sidecar agents (settl TOML, hermes YAML, kiro per-agent JSON)
            // install into a host config file; sandbox install is handled by
            // build_container_config. host_only agents (settl) are never
            // sandboxed, so the gate is a no-op for them.
            if !self.is_sandboxed() {
                if let Some(home) = dirs::home_dir() {
                    let config_path = home.join(sidecar.host_config_subpath);
                    match (sidecar.install)(&config_path) {
                        Ok(()) => {
                            if let Some(post_install) = sidecar.post_install_host {
                                post_install();
                            }
                        }
                        Err(e) => tracing::warn!(target: "session.store",
                            "Failed to install {} hooks: {}", self.tool, e),
                    }
                }
            }
        } else if agent.is_some_and(|a| a.name == "codex") && !self.is_sandboxed() {
            if let Some(hook_cfg) = agent.and_then(|a| a.hook_config.as_ref()) {
                match self.codex_config_path_for_launch_env() {
                    Ok(config_path) => {
                        if let Err(e) =
                            crate::hooks::install_codex_hooks(&config_path, hook_cfg.events)
                        {
                            tracing::warn!("Failed to install codex hooks: {}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Failed to resolve codex config path: {}", e),
                }
            }
        } else if let Some(hook_cfg) = agent.and_then(|a| a.hook_config.as_ref()) {
            if self.is_sandboxed() {
                // For sandboxed sessions, hooks are installed via build_container_config
            } else {
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
                            hook_cfg.events,
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
        }
    }

    fn codex_config_path_for_launch_env(&self) -> Result<PathBuf> {
        crate::hooks::codex_config_path_for_host_environment(&self.profile_host_environment())
    }

    /// Build the tmux command for a sandboxed (Docker) session.
    ///
    /// Runs on_launch hooks inside the container, constructs the tool command
    /// with yolo mode / custom instructions / session flags, and wraps it in a
    /// `docker exec` invocation.
    /// Build the tmux command for a host (non-sandboxed) session.
    ///
    /// Runs on_launch hooks on the host, then constructs the command from either
    /// the agent's default binary or a user-supplied custom command, applying
    /// yolo mode, session flags, and the AOE_INSTANCE_ID env prefix.
    fn build_host_command(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
        on_launch_hooks: &Option<Vec<String>>,
    ) -> (Option<String>, bool) {
        if let Some(ref hook_cmds) = on_launch_hooks {
            let hook_env = super::repo_config::lifecycle_env_vars(self);
            if let Err(e) = super::repo_config::execute_hooks(
                hook_cmds,
                Path::new(&self.project_path),
                &hook_env,
            ) {
                tracing::warn!(target: "session.store", "on_launch hook failed: {}", e);
            }
        }

        let mut env_prefix = status_hook_env_prefix(&self.id, agent);

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
                    let mut cmd = a.binary.to_string();
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
                    (
                        Some(wrap_command_ignore_suspend(&format!(
                            "{}{}",
                            env_prefix, cmd
                        ))),
                        is_existing,
                    )
                }
                None => (None, false),
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
            (
                Some(wrap_command_ignore_suspend(&format!(
                    "{}{}",
                    env_prefix, cmd
                ))),
                is_existing,
            )
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
        let promote_cleared = matches!(expected_prior_intent, ResumeIntent::Cleared);

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

            if promote_cleared {
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
                if promote_cleared {
                    if let Ok(insts) = storage.load() {
                        if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                            self.resume_intent = disk.resume_intent;
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

    /// Atomic single-flock CAS+clear of `agent_session_id` and (when
    /// disk still pins `Use(stale_sid)`) downgrade of `resume_intent`
    /// to `Default`. A split would let a daemon crash freeze disk at
    /// `(None, Use(stale_sid))`, forcing one extra cascade cycle on
    /// the next launch.
    ///
    /// Intent downgrade is gated on disk's `resume_intent` (read under
    /// the flock), not the caller's memory: a user repin landing
    /// between the probe and the clear keeps its fresh pin.
    ///
    /// Also heals the legacy `(None, Use(stale_sid))` shape that the
    /// previous two-flock implementation could persist after a daemon
    /// crash mid-cascade: when disk's sid is already `None` but intent
    /// still pins the dead sid, downgrade intent to `Default` and
    /// return `Applied`.
    ///
    /// On sid CAS skip: skip both writes. On Applied or Skipped:
    /// reload both fields from disk so memory matches whatever the
    /// closure committed (or the peer's state on Skipped).
    fn clear_session_for_resume_fallback(&mut self, profile: &str, stale_sid: &str) -> SidWrite {
        let storage = match super::storage::Storage::new(profile, self.resolve_file_watch()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to create storage for resume-fallback clear for {}: {}",
                    self.id,
                    e
                );
                return SidWrite::Failed;
            }
        };

        let instance_id = self.id.clone();
        let stale_for_closure = stale_sid.to_string();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };

            // Heal legacy `(None, Use(stale_sid))` stuck state from a
            // pre-flock daemon crash between the two writes. Rare
            // post-migration; tracing is for forensic visibility.
            if inst.agent_session_id.is_none()
                && matches!(&inst.resume_intent, ResumeIntent::Use(p) if p == &stale_for_closure)
            {
                tracing::info!(target: "session.store",
                    instance_id = %instance_id,
                    stale_sid = %stale_for_closure,
                    "healing legacy (None, Use(stale)) stuck state in resume-fallback clear"
                );
                inst.resume_intent = ResumeIntent::Default;
                return Ok(SidWrite::Applied);
            }

            if inst.agent_session_id.as_deref() != Some(stale_for_closure.as_str()) {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_sid = %stale_for_closure,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in resume-fallback clear; skipping both writes"
                );
                return Ok(SidWrite::Skipped);
            }

            inst.agent_session_id = None;

            // Downgrade only when the pin still names the dead sid. The
            // "user repinned to fresh-sid mid-cascade" and "intent was
            // already Default/Cleared" paths are legitimate, no warn.
            if matches!(&inst.resume_intent, ResumeIntent::Use(p) if p == &stale_for_closure) {
                inst.resume_intent = ResumeIntent::Default;
            }

            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped)) => {
                if let Ok(insts) = storage.load() {
                    if let Some(disk) = insts.into_iter().find(|i| i.id == self.id) {
                        self.agent_session_id = disk.agent_session_id;
                        self.resume_intent = disk.resume_intent;
                    }
                }
                write
            }
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Resume-fallback clear found no instance row for {}",
                    self.id
                );
                SidWrite::Failed
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to clear sid for resume fallback for {}: {}",
                    self.id,
                    e
                );
                SidWrite::Failed
            }
        }
    }
}

impl Instance {
    fn apply_terminal_tmux_options(&self) {
        let name = tmux::TerminalSession::generate_name(&self.id, &self.title);
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

        if container.is_running()? {
            // Already up: not a come-up, so don't re-mint. Fill lazily only if a
            // fresh process attached to a running container with no values yet.
            self.ensure_before_start_env(false)?;
            container_config::refresh_agent_configs();
            return Ok(container);
        }

        if container.exists()? {
            // Restart of a stopped container is a come-up: refresh so a
            // short-lived token is re-minted.
            self.ensure_before_start_env(true)?;
            container_config::refresh_agent_configs();
            container.start()?;
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
        }

        Ok(container)
    }

    /// Get the container working directory for this instance.
    pub fn container_workdir(&self) -> String {
        container_config::compute_volume_paths(Path::new(&self.project_path), &self.project_path)
            .map(|(_, wd)| wd)
            .unwrap_or_else(|_| "/workspace".to_string())
    }

    fn build_container_config(&self) -> Result<crate::containers::ContainerConfig> {
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;
        container_config::build_container_config(
            &self.project_path,
            sandbox,
            container_config::ContainerAgentSelection::new(&self.tool, Some(&self.detect_as)),
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
        let minted =
            super::repo_config::run_before_start_hooks(&commands, &project_path, &hook_env)?;
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
            .tmux_session()
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        let mut poller = SessionPoller::new(tmux_session_name.clone());
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone();
        // Snapshot per-instance excludes (sids cleared by the resume-fallback
        // cascade) at poller-spawn time. The cascade always tears down the
        // existing poller and re-enters this function AFTER inserting into
        // `retroactive_capture_excludes` (see start_with_resume_fallback),
        // so the freshly-spawned poller's first immediate poll sees the
        // populated set and won't re-import the bad sid.
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
            _ => return,
        };

        let cb_instance_id = self.id.clone();
        let cb_tmux_name = self
            .tmux_session()
            .map(|s| s.name().to_string())
            .unwrap_or_default();

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
            tracing::info!(target: "session.store", "Session ID changed for {}: {}", cb_instance_id, new_id);
            if !cb_tmux_name.is_empty() {
                publish_session_to_tmux_env(&cb_tmux_name, new_id);
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
    /// they already ran in the background creation poller).
    pub fn restart_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<StartOutcome> {
        self.stop_poller();
        self.session_id_poller = None;
        self.kill_clean()?;
        self.start_with_resume_fallback(size, skip_on_launch)
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
    ///   2. If the pane went dead within the probe window, stop the Tier-1
    ///      poller, tear down the dead tmux session, clear the bad sid
    ///      (in memory, on disk, and from retroactive-capture re-discovery),
    ///      and retry the start with `skip_on_launch=true` so on_launch
    ///      hooks do not run twice. Probe the second pane the same way:
    ///      `start_with_size_opts` only fails on tmux-subprocess errors, so
    ///      an in-pane crash (missing binary, broken `docker exec`, agent
    ///      panic on first run) would otherwise masquerade as
    ///      `StartOutcome::Restarted` over a corpse pane.
    ///   3. If the Tier-2 start fails at the tmux level *or* its pane crashes
    ///      within the probe window, propagate `Err` so the caller's existing
    ///      `Status::Error` + `last_error` path takes over.
    ///
    /// Latency: only fires the probe when `--resume <sid>` is being passed
    /// to a freshly-created tmux session. Healthy resumes on real agents
    /// pay `RESUME_PROBE_POST_SHELL_GRACE` (~2s) once on cold start;
    /// warm sessions and non-resume launches pay nothing. Shell-wrapper
    /// command overrides pay the full `RESUME_PROBE_MAX` (~3s) on every
    /// healthy resume because `is_pane_running_shell` never clears for
    /// them; see `probe_settle`. When the cascade fires, add `kill_clean`
    /// (~100ms macOS grace) + Tier-2 spawn + a second `RESUME_PROBE_MAX`
    /// window: ~6-7s total worst-case.
    ///
    /// Acp-mode sessions short-circuit (no tmux pane to probe).
    /// `StartOutcome::Fresh` is honest there: structured view's resume concept lives
    /// in `acp_session_id` and is handled by the ACP supervisor, not
    /// by this cascade.
    pub(crate) fn start_with_resume_fallback(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
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

        #[cfg(feature = "serve")]
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

        let outcome = self.start_with_size_opts(size, skip_on_launch)?;

        // Computed post-`start_with_size_opts` so it reflects post-reconcile
        // state. A pre-call read would miss a peer-CLI `Use(X)` write that
        // landed since the daemon's last status_poll, causing the cascade
        // to skip the very Use(X_dead) case Tier-1's downgrade is meant to
        // handle.
        //
        // Gated on `LaunchSidOutcome::Existing` so fresh launches (Cleared,
        // no observed sid + Claude UUID generation) skip the probe and
        // honestly report `Fresh`. Without this gate, every Claude launch
        // would probe (~2s) and return `Resumed` because acquire always
        // assigns a UUID, even when no `--resume` was passed.
        let attempting_resume = matches!(outcome, LaunchSidOutcome::Existing)
            && should_attempt_resume(self.agent_session_id.as_deref(), &self.tool);

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
            return Ok(StartOutcome::Fresh);
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

        let stale_sid = self
            .agent_session_id
            .clone()
            .expect("attempting_resume guarantees agent_session_id is Some");
        let profile = self.effective_profile();
        tracing::warn!(
            target: "session.store",
            "start: resume with sid {} for session {} crashed pane within probe; \
             clearing sid and retrying without resume",
            stale_sid,
            self.id,
        );

        self.stop_poller();
        self.session_id_poller = None;
        self.kill_clean()
            .with_context(|| format!("kill_clean before resume fallback for {}", self.id))?;

        self.agent_session_id = None;
        if let Ok(dir) = crate::hooks::hook_status_dir(&self.id) {
            let _ = std::fs::remove_file(dir.join("session_id"));
        }
        // Populate the poller exclusion before calling
        // `clear_session_for_resume_fallback` so its `Failed` bail
        // still keeps the bad sid out of the next retroactive capture
        // cycle. Must stay before that call for the bail to win.
        self.retroactive_capture_excludes.insert(stale_sid.clone());
        match self.clear_session_for_resume_fallback(&profile, &stale_sid) {
            SidWrite::Applied | SidWrite::Skipped => {}
            // `Failed` is unit-tested via
            // `clear_for_resume_fallback_returns_failed_on_missing_row`;
            // bail rather than launch Tier-2 against a disk we know
            // could not be cleaned, which would re-pin the dead sid.
            SidWrite::Failed => {
                anyhow::bail!(
                    "resume-fallback could not clear stale sid {} for {}",
                    stale_sid,
                    self.id,
                );
            }
        }

        let _ = self.start_with_size_opts(size, true).with_context(|| {
            format!(
                "fresh restart after resume fallback failed for {} (stale sid {} was cleared)",
                self.id, stale_sid,
            )
        })?;

        // Tier-2 needs the same settle-probe as Tier-1: tmux can spawn the
        // pane successfully while the agent inside crashes immediately
        // (missing binary, gone docker image, agent panic on first run).
        // Without this, the in-pane crash class - the very class the cascade
        // exists to surface - would silently report `StartOutcome::Restarted`.
        // The dead pane is left in tmux; the next user-initiated restart
        // goes through `restart_with_size_opts` -> `kill_clean` and self-heals.
        // Tier-2 settle probe. On Err (same rare condition as Tier-1),
        // tear down the Tier-2 poller spawned by the second
        // start_with_size_opts before propagating.
        let probe = match self.probe_settle(RESUME_PROBE_MAX, RESUME_PROBE_POLL) {
            Ok(p) => p,
            Err(e) => {
                self.stop_poller();
                self.session_id_poller = None;
                return Err(e);
            }
        };
        if matches!(probe, ProbeResult::Dead) {
            // Symmetric teardown with the Tier-1->Tier-2 transition above:
            // the Tier-2 spawn already started a fresh poller via
            // `finalize_launch`, and bailing without stopping it leaves an
            // orphan thread polling a dead pane. The pane stays in tmux
            // (intentional, for `tmux attach` diagnostic on the crash),
            // but the poller handle must be torn down so callers see a
            // consistent post-error state.
            self.stop_poller();
            self.session_id_poller = None;
            anyhow::bail!(
                "fresh restart after resume fallback crashed within probe for {} \
                 (stale sid {} was cleared; underlying issue persists)",
                self.id,
                stale_sid,
            );
        }

        Ok(StartOutcome::Restarted { stale_sid })
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
    /// on a dead resume-eligible pane can block for the full Tier-1 +
    /// Tier-2 cascade window (~6-7s; see `start_with_resume_fallback` for
    /// the breakdown) plus up to 3s of `wait_for_pane_ready` polling.
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
        #[cfg(feature = "serve")]
        if self.is_structured() {
            return Err(EnsureReadyError::StructuredView);
        }
        let session = self.tmux_session().map_err(EnsureReadyError::Tmux)?;
        if !session.exists() {
            // Route fresh starts through the cascade so a stale sid loaded
            // from disk that crashes the agent on launch is detected,
            // cleared, and retried. Without this, `aoe send` after a tmux
            // server kill or reboot resurrects the same bad sid the
            // restart paths exist to recover from.
            let outcome = self
                .start_with_resume_fallback(size, false)
                .map_err(EnsureReadyError::Tmux)?;
            self.wait_for_pane_ready(&session);
            let stale_sid = match outcome {
                StartOutcome::Restarted { stale_sid } => Some(stale_sid),
                StartOutcome::Resumed | StartOutcome::Fresh => None,
            };
            return Ok(EnsureReadyOutcome::Started { stale_sid });
        }
        if session.is_pane_dead() {
            let outcome = self
                .restart_with_size(size)
                .map_err(EnsureReadyError::Tmux)?;
            self.wait_for_pane_ready(&session);
            let stale_sid = match outcome {
                StartOutcome::Restarted { stale_sid } => Some(stale_sid),
                StartOutcome::Resumed | StartOutcome::Fresh => None,
            };
            return Ok(EnsureReadyOutcome::Respawned { stale_sid });
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
        if let Err(e) = self.kill_terminal() {
            tracing::debug!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                kind = "terminal",
                error = %e,
                "kill_ancillary_tmux_sessions: kill failed"
            );
        }
        if let Err(e) = self.kill_container_terminal() {
            tracing::debug!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                kind = "container_terminal",
                error = %e,
                "kill_ancillary_tmux_sessions: kill failed"
            );
        }
        crate::tmux::kill_all_tool_sessions_for_id(&self.id);
    }

    /// Stop the session: kill the tmux session and stop the Docker container
    /// (if sandboxed). The container is stopped but not removed, so it can be
    /// restarted on re-attach.
    pub fn stop(&self) -> Result<()> {
        self.kill()?;

        if self.is_sandboxed() {
            let container = containers::DockerContainer::from_session_id(&self.id);
            if container.is_running().unwrap_or(false) {
                container.stop()?;
            }
        }

        crate::hooks::cleanup_hook_status_dir(&self.id);

        Ok(())
    }

    /// Update status using pre-fetched pane metadata to avoid per-instance
    /// subprocess spawns. Falls back to subprocess calls if metadata is missing.
    pub fn update_status_with_metadata(&mut self, metadata: Option<&tmux::PaneMetadata>) {
        let prev_status = self.status;
        self.update_status_with_metadata_inner(metadata);
        if self.status != prev_status {
            let now = Utc::now();
            self.last_accessed_at = Some(now);
            self.idle_entered_at = if self.status == Status::Idle {
                Some(now)
            } else {
                None
            };
        }
    }

    fn update_status_with_metadata_inner(&mut self, metadata: Option<&tmux::PaneMetadata>) {
        if matches!(
            self.status,
            Status::Stopped | Status::Deleting | Status::Creating
        ) {
            return;
        }

        // Acp-mode sessions are not backed by a tmux pane; the structured view
        // worker supervisor owns their lifecycle and emits typed health
        // events over the broadcast. Probing tmux here only ever produces
        // a spurious "tmux session is gone" Error transition.
        #[cfg(feature = "serve")]
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
            status_hook_env_prefix("abc123", agent),
            "AOE_INSTANCE_ID=abc123 "
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

        let config_path = tmp.path().join(".codex").join("config.toml");
        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(config.contains("[[hooks.PreToolUse]]"));
        assert!(config.contains("aoe-hooks"));
        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
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

        let config_path = codex_home.join("config.toml");
        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(config.contains("[[hooks.PreToolUse]]"));
        assert!(config.contains("aoe-hooks"));
        assert!(!tmp.path().join(".codex").join("config.toml").exists());
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

        assert!(!tmp.path().join(".codex").join("config.toml").exists());
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

        let config_path = tmp.path().join(".codex").join("config.toml");
        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(config.contains("[[hooks.PreToolUse]]"));
        assert!(config.contains("aoe-hooks"));
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
    fn test_merge_post_restart_no_fallback_preserves_sid() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("peer-fresh-sid".to_string());
        stored.snooze(15);

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Idle;
        working.agent_session_id = Some("phase1-stale-sid".to_string());

        stored.merge_post_restart(&working, None);

        assert_eq!(stored.status, Status::Idle);
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("peer-fresh-sid"),
            "no-fallback restart must not clobber peer sid write"
        );
        assert!(stored.is_snoozed(), "peer snooze must survive merge");
    }

    #[test]
    fn test_merge_post_restart_fallback_clears_matching_sid() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("phase1-stale-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;
        working.agent_session_id = None;

        stored.merge_post_restart(&working, Some("phase1-stale-sid"));

        assert!(
            stored.agent_session_id.is_none(),
            "stored sid still equals stale; cascade invalidation propagates"
        );
    }

    #[test]
    fn test_merge_post_restart_preserves_poller_sid_landed_during_phase_2() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("poller-fresh-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;
        working.agent_session_id = None;

        stored.merge_post_restart(&working, Some("phase1-stale-sid"));

        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("poller-fresh-sid"),
            "poller wrote a fresh sid between phase 2 and phase 3; CAS preserves it"
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

    #[cfg(feature = "serve")]
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
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("tmux not available; skipping");
            return;
        }

        let mut inst = Instance::new("ensure_alive_test", "/tmp/test");
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .output();
        let created = std::process::Command::new("tmux")
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

        let _ = std::process::Command::new("tmux")
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

    #[cfg(feature = "serve")]
    #[test]
    fn test_instance_acp_acp_session_id_roundtrip() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        inst.acp_session_id = Some("acp-uuid-1234".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("acp_session_id"));
        let deserialized: Instance = serde_json::from_str(&json).unwrap();
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

        let (session_id, is_existing) = inst.acquire_session_id();

        assert_eq!(session_id, Some("session-42".to_string()));
        assert!(is_existing);
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

        // The method should return the existing session ID and mark it as existing
        let (session_id, is_existing) = inst.acquire_session_id();
        assert_eq!(session_id, Some("invalid-session-id".to_string()));
        assert!(is_existing);
    }

    #[test]
    fn test_build_resume_flags_rejects_invalid_id() {
        let flags = build_resume_flags("claude", "$(rm -rf /)", true);
        assert_eq!(flags, "");

        let flags = build_resume_flags("opencode", "id; echo pwned", false);
        assert_eq!(flags, "");
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

        assert!(first.is_some());
        assert!(!first_existing);
        assert!(second_existing);
        assert_eq!(first, second);
    }

    #[test]
    fn apply_session_flags_returns_acquire_is_existing() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "claude".to_string();
        let mut cmd = String::from("claude");
        assert!(!inst.apply_session_flags(&mut cmd, "test"));
        assert!(inst.apply_session_flags(&mut cmd, "test"));
    }

    #[cfg(feature = "serve")]
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
            status_hook_env_prefix("abc123", crate::agents::get_agent("hermes")),
            "AOE_INSTANCE_ID=abc123 "
        );
        assert_eq!(
            status_hook_env_prefix("abc123", crate::agents::get_agent("settl")),
            "AOE_INSTANCE_ID=abc123 "
        );
        assert_eq!(
            status_hook_env_prefix("abc123", crate::agents::get_agent("claude")),
            "AOE_INSTANCE_ID=abc123 "
        );
        assert_eq!(
            status_hook_env_prefix("abc123", crate::agents::get_agent("opencode")),
            ""
        );
        assert_eq!(
            status_hook_env_prefix("abc123", crate::agents::get_agent("kiro")),
            "AOE_INSTANCE_ID=abc123 "
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
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("codex"), &None);
        assert!(cmd.is_some());
        assert!(cmd.as_ref().unwrap().contains("codex"));
    }

    #[test]
    fn test_build_host_command_with_yolo() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.yolo_mode = true;
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("codex"), &None);
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
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("claude"), &None);
        let cmd_str = cmd.unwrap();
        assert!(cmd_str.contains("ses_abc123def456"));
        assert!(cmd_str.contains("--session-id") || cmd_str.contains("--resume"));
    }

    #[test]
    fn test_build_host_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("antigravity"), &None);
        let cmd_str = cmd.unwrap();

        assert!(cmd_str.contains("env -u NO_COLOR"));
        assert!(cmd_str.contains("TERM=xterm-256color"));
        assert!(cmd_str.contains("COLORTERM=truecolor"));
        assert!(cmd_str.contains("agy"));
    }

    #[test]
    fn test_build_host_custom_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        inst.command = "agy --some-flag".to_string();
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("antigravity"), &None);
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
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("codex"), &None);
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
        let (cmd, _) = inst.build_host_command(crate::agents::get_agent("cursor"), &None);
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
        use std::process::Command;

        fn tmux_available() -> bool {
            Command::new("tmux")
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
            let output = Command::new("tmux")
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
            let _ = Command::new("tmux")
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
        use super::super::{should_attempt_resume, Instance, ResumeIntent, StartOutcome, Status};
        use serial_test::serial;
        use tempfile::tempdir;

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
        }

        #[test]
        fn unsupported_agent_does_not_attempt_resume() {
            assert!(!should_attempt_resume(
                Some("11111111-1111-1111-1111-111111111111"),
                "cursor"
            ));
            assert!(!should_attempt_resume(
                Some("11111111-1111-1111-1111-111111111111"),
                "copilot"
            ));
        }

        #[test]
        fn unknown_tool_does_not_attempt_resume() {
            assert!(!should_attempt_resume(Some("uuid-abc-123"), "nonexistent"));
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

            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("observed"));
            assert!(is_existing);
        }

        #[test]
        fn resume_intent_serde_round_trip() {
            for intent in [
                ResumeIntent::Default,
                ResumeIntent::Use("abc".to_string()),
                ResumeIntent::Cleared,
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

        fn write_sidecar(instance_id: &str, sid: &str) -> std::path::PathBuf {
            let dir =
                crate::hooks::hook_status_dir(instance_id).expect("test id must be allowlist-safe");
            std::fs::create_dir_all(&dir).unwrap();
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

        fn seed_disk(profile: &str, inst: &Instance) {
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
        fn clear_for_resume_fallback_atomically_clears_and_downgrades() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-happy";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());
            seed_disk(profile, &inst);

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Applied);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
            assert_eq!(inst.agent_session_id, None);
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_intent_already_default() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-default";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Default;
            seed_disk(profile, &inst);

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Applied);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(loaded[0].resume_intent, ResumeIntent::Default);
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_preserves_user_repin() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-repin";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());
            seed_disk(profile, &inst);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            storage
                .update(|i, _g| {
                    i[0].resume_intent = ResumeIntent::Use("fresh".to_string());
                    Ok(())
                })
                .unwrap();

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Applied);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Use("fresh".to_string()),
                "user's repin must survive the cascade clear",
            );
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("fresh".to_string()),
                "memory must reload to honor the repin so Tier-2 picks it up",
            );
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_skips_on_sid_cas_mismatch() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-sid-mismatch";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());
            seed_disk(profile, &inst);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            storage
                .update(|i, _g| {
                    i[0].agent_session_id = Some("peer-fresh".to_string());
                    i[0].resume_intent = ResumeIntent::Use("peer-fresh".to_string());
                    Ok(())
                })
                .unwrap();

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Skipped);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id.as_deref(), Some("peer-fresh"));
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Use("peer-fresh".to_string()),
            );
            assert_eq!(
                inst.agent_session_id.as_deref(),
                Some("peer-fresh"),
                "memory must reload sid to converge on peer",
            );
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("peer-fresh".to_string()),
                "memory must reload intent to converge on peer",
            );
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_heals_legacy_stuck_state() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-heal-legacy";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());
            seed_disk(profile, &inst);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            storage
                .update(|i, _g| {
                    i[0].agent_session_id = None;
                    Ok(())
                })
                .unwrap();

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Applied);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Default,
                "legacy (None, Use(stale)) state must heal to Default",
            );
            assert_eq!(inst.agent_session_id, None);
            assert_eq!(inst.resume_intent, ResumeIntent::Default);
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_skips_on_user_repin_with_none_sid() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-repin-none-sid";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());
            seed_disk(profile, &inst);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            storage
                .update(|i, _g| {
                    i[0].agent_session_id = None;
                    i[0].resume_intent = ResumeIntent::Use("other".to_string());
                    Ok(())
                })
                .unwrap();

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Skipped);

            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Use("other".to_string()),
                "pin to a different sid must not be healed",
            );
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Use("other".to_string()),
                "memory must reload the user's repin",
            );
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_intent_cleared_not_downgraded() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-intent-cleared";
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Cleared;
            seed_disk(profile, &inst);

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Applied);

            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let loaded = storage.load().unwrap();
            assert_eq!(loaded[0].agent_session_id, None);
            assert_eq!(
                loaded[0].resume_intent,
                ResumeIntent::Cleared,
                "Cleared is not Use(stale_sid); downgrade must not fire",
            );
            assert_eq!(inst.resume_intent, ResumeIntent::Cleared);
        }

        #[test]
        #[serial]
        fn clear_for_resume_fallback_returns_failed_on_missing_row() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "fallback-clear-missing";
            let other = Instance::new("other", "/tmp/x");
            seed_disk(profile, &other);

            let mut inst = Instance::new("missing", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = Some("stale".to_string());
            inst.resume_intent = ResumeIntent::Use("stale".to_string());

            let outcome = inst.clear_session_for_resume_fallback(profile, "stale");
            assert_eq!(outcome, super::SidWrite::Failed);

            assert_eq!(inst.agent_session_id.as_deref(), Some("stale"));
            assert_eq!(inst.resume_intent, ResumeIntent::Use("stale".to_string()));
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

            let outcome = inst.start_with_resume_fallback(None, true).unwrap();
            assert_eq!(outcome, StartOutcome::Fresh);
        }

        #[test]
        #[serial]
        fn fallback_clears_sid_in_memory_and_on_disk_when_pane_dies() {
            if std::process::Command::new("tmux")
                .arg("-V")
                .output()
                .is_err()
            {
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
            let id = inst.id.clone();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = std::process::Command::new("tmux")
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

            let outcome = inst.start_with_resume_fallback(None, true);

            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let err = outcome
                .expect_err("Tier-2 with /bin/false must crash within probe and propagate Err");
            let chain = format!("{:#}", err);
            assert!(
                chain.contains("crashed within probe") && chain.contains(&stale_sid),
                "Tier-2 probe failure must surface the stale sid in its error chain, got: {chain}",
            );

            assert!(
                inst.retroactive_capture_excludes.contains(&stale_sid),
                "stale sid must be in exclusion set even when Tier-2 ultimately fails: \
                 the cleanup happens before the Tier-2 attempt and must survive the bail",
            );
            assert_ne!(
                inst.agent_session_id.as_deref(),
                Some(stale_sid.as_str()),
                "stale sid must not survive in memory after fallback, even on Tier-2 failure",
            );
            let loaded = storage.load().unwrap();
            let row = loaded.iter().find(|i| i.id == id).expect("instance");
            assert_ne!(
                row.agent_session_id.as_deref(),
                Some(stale_sid.as_str()),
                "stale sid must not survive on disk after fallback, even on Tier-2 failure",
            );
        }

        #[test]
        #[serial]
        fn fallback_returns_restarted_when_tier2_pane_lives() {
            if std::process::Command::new("tmux")
                .arg("-V")
                .output()
                .is_err()
            {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let _storage = crate::session::storage::Storage::new_unwatched("fb-test-live").unwrap();

            let stale_sid = "22222222-2222-2222-2222-222222222222".to_string();
            let mut inst = Instance::new("fallback_lives_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test-live".to_string();
            // Claude regenerates a fresh UUID on Tier-2 (acquire_session_id
            // emits `--session-id <new_uuid>`), so we cannot just count argv.
            // Match the *stale* sid specifically: Tier-1 appends `--resume
            // <stale_sid>` and the script exits, firing the cascade. Tier-2
            // appends `--session-id <new_uuid>` (different value), so the
            // pattern does not match and `sleep 30` keeps the pane alive
            // past RESUME_PROBE_MAX (3s).
            //
            // Pinned: this discriminator relies on
            // `ResumeStrategy::FlagPair` appending the sid to argv (see
            // `src/agents.rs`). If a future variant ever passes the sid
            // out of band (env var, stdin, file), update the wrapper to
            // observe the new channel instead of `$*`.
            inst.command = format!(
                "/bin/sh -c 'case \"$*\" in *{stale}*) exit 1 ;; esac; exec sleep 30' --",
                stale = stale_sid,
            );
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;

            // Seed required: clear_session_for_resume_fallback bails on
            // missing-row Failed before reaching Tier-2, so the row must
            // exist on disk for the cascade to reach the probe assertions.
            let xs = vec![inst.clone()];
            _storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(None, true);

            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .output();

            match outcome {
                Ok(StartOutcome::Restarted { stale_sid: cleared }) => {
                    assert_eq!(cleared, stale_sid);
                }
                Ok(other) => panic!(
                    "Tier-2 success path must return Restarted, got {other:?}; \
                     a different variant indicates the probe misfires"
                ),
                Err(e) => panic!(
                    "Tier-2 with a live binary must succeed: {e:#}; \
                     check probe_settle behavior on long-running shells"
                ),
            }
            assert!(inst.retroactive_capture_excludes.contains(&stale_sid));
            assert!(inst.agent_session_id.as_deref() != Some(stale_sid.as_str()));
        }

        #[test]
        #[serial]
        fn cascade_fires_when_pane_dies_inside_post_shell_grace_window() {
            if std::process::Command::new("tmux")
                .arg("-V")
                .output()
                .is_err()
            {
                eprintln!("tmux not available; skipping");
                return;
            }
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let _storage =
                crate::session::storage::Storage::new_unwatched("fb-test-grace").unwrap();

            let stale_sid = "33333333-3333-3333-3333-333333333333".to_string();
            let mut inst = Instance::new("fallback_grace_test", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test-grace".to_string();
            // Regression guard for RESUME_PROBE_POST_SHELL_GRACE.
            //
            // The Tier-1 wrapper exits its boot shell immediately via `exec
            // sleep N`, then the replacement process exits; tmux observes
            // pane_dead at roughly t = N seconds. We pick N inside the
            // window (current grace 2000ms, sleep 1.2s) so:
            //   * with grace = 500ms, probe_settle returns Alive at t=500ms
            //     before the death at t=1200ms; cascade misses it; the test
            //     observes Resumed; assertion FAILS (regression caught).
            //   * with grace >= ~1300ms, the grace timer is still open when
            //     pane_dead fires; probe_settle returns Dead; cascade fires;
            //     the test observes Restarted; assertion PASSES.
            //
            // This pins the LOWER bound of grace; the upper bound is
            // implicitly RESUME_PROBE_MAX (3000ms) since deadline charity
            // would otherwise mask future regressions.
            //
            // Tier-2 reuses the same wrapper but its sid differs from the
            // stale one (cascade clears agent_session_id; Claude's FlagPair
            // strategy regenerates a fresh UUID), so the case match misses
            // and `exec sleep 30` keeps the pane alive past the second
            // probe window.
            inst.command = format!(
                "/bin/sh -c 'case \"$*\" in *{stale}*) exec sleep 1.2 ;; esac; exec sleep 30' --",
                stale = stale_sid,
            );
            inst.agent_session_id = Some(stale_sid.clone());
            inst.status = Status::Idle;

            // Seed required: clear_session_for_resume_fallback bails on
            // missing-row Failed before reaching Tier-2, so the row must
            // exist on disk for the cascade to reach the probe assertions.
            let xs = vec![inst.clone()];
            _storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .output();

            let outcome = inst.start_with_resume_fallback(None, true);

            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .output();

            match outcome {
                Ok(StartOutcome::Restarted { stale_sid: cleared }) => {
                    assert_eq!(cleared, stale_sid);
                }
                Ok(StartOutcome::Resumed) => panic!(
                    "Tier-1 grace shortcut returned Alive before the t=1200ms pane_dead: \
                     RESUME_PROBE_POST_SHELL_GRACE is too short. \
                     Real opencode crashes at ~1000ms; raise the grace constant."
                ),
                Ok(other) => panic!(
                    "Expected Restarted or Resumed; got {other:?} (probe path is taking an unexpected branch)"
                ),
                Err(e) => panic!(
                    "Tier-2 must succeed because its wrapper does not match the stale sid: {e:#}; \
                     either the cascade is misrouting the sid or Tier-2 spawn is failing"
                ),
            }
            assert!(inst.retroactive_capture_excludes.contains(&stale_sid));
            assert!(inst.agent_session_id.as_deref() != Some(stale_sid.as_str()));
        }
    }

    mod publish_captured_sid {
        use super::super::{Instance, ResumeIntent};
        use serial_test::serial;
        use std::process::Command;
        use tempfile::{tempdir, TempDir};

        const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";
        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";

        struct TmuxSession(String);

        impl TmuxSession {
            fn create(id: &str, title: &str) -> Self {
                let name = crate::tmux::Session::generate_name(id, title);
                let _ = Command::new("tmux")
                    .args(["kill-session", "-t", &name])
                    .output();
                let status = Command::new("tmux")
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
                let _ = Command::new("tmux")
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }

        fn skip_if_no_tmux() -> bool {
            if Command::new("tmux").arg("-V").output().is_err() {
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
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    fn tmux_available() -> bool {
        std::process::Command::new("tmux")
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
        let created = std::process::Command::new("tmux")
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
        let dir = crate::hooks::hook_status_dir(&inst.id).expect("hook dir");
        std::fs::create_dir_all(&dir).expect("create hook dir");
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
            let cap = std::process::Command::new("tmux")
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
