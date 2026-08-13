//! Session instance definition and operations

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::containers::{self, DockerContainer};
use crate::tmux;

use super::container_config;
use super::environment::{
    build_docker_env_args_with_managed_codex_home, resolved_sandbox_environment, shell_escape,
};
use super::poller::SessionPoller;

use crate::session::capture::{
    capture_claude_session_id, capture_claude_session_id_in_container, capture_codex_session_id,
    capture_copilot_session_id, capture_gemini_session_id, capture_hermes_session_id,
    capture_kimi_session_id, capture_omp_session_id, capture_pi_session_id,
    capture_vibe_session_id, claude_poll_fn, claude_poll_fn_sandboxed, codex_poll_fn,
    codex_poll_fn_sandboxed, copilot_poll_fn, gemini_poll_fn, gemini_poll_fn_sandboxed,
    generate_claude_session_id, hermes_poll_fn, hermes_poll_fn_sandboxed, is_valid_session_id,
    kimi_poll_fn, omp_host_routing_environment, omp_poll_fn, omp_poll_fn_sandboxed,
    omp_sandbox_launch_marker, opencode_poll_fn, opencode_poll_fn_sandboxed, pi_poll_fn,
    pi_poll_fn_sandboxed, reject_omp_secret_args, resolve_omp_store_layout,
    resolve_omp_store_layout_in_container_with_environment,
    resolve_omp_store_layout_with_environment, try_capture_codex_session_id_in_container,
    try_capture_gemini_session_id_in_container, try_capture_hermes_session_id_in_container,
    try_capture_omp_session_id_in_container, try_capture_opencode_session_id,
    try_capture_opencode_session_id_in_container, try_capture_pi_session_id_in_container,
    try_capture_vibe_session_id_in_container, validate_omp_capture_metadata, validated_session_id,
    vibe_poll_fn, vibe_poll_fn_sandboxed, OmpCaptureMetadata, OmpCapturePlan, OmpCliCaptureOptions,
    OmpStoreKind,
};
type LaunchCommandParts = (
    Option<String>,
    bool,
    Option<OmpCapturePlan>,
    LaunchEnvironment,
);

struct LaunchEnvironment {
    pane: Vec<tmux::PaneEnvMutation>,
    container: Vec<(String, String)>,
}

struct PreparedLaunch {
    command: Option<String>,
    is_existing: bool,
    omp_capture_plan: Option<OmpCapturePlan>,
    launch_env: LaunchEnvironment,
    expected_prior_sid: Option<String>,
    expected_prior_intent: ResumeIntent,
    expected_prior_omp_generation: Option<String>,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

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

    /// Wire form for the HTTP API's `status` field: PascalCase, matching what
    /// `SessionResponse` has emitted since the endpoint shipped (the web
    /// dashboard and existing tests both key on it). Distinct from
    /// [`Status::as_str`], which is the lowercase CLI/hook form.
    ///
    /// Spelled out rather than leaning on `format!("{:?}")` so renaming a
    /// variant cannot silently change the public API;
    /// `status_api_wire_form_round_trips` pins the two together. See #3187.
    pub fn wire_str(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Waiting => "Waiting",
            Status::Idle => "Idle",
            Status::Unknown => "Unknown",
            Status::Stopped => "Stopped",
            Status::Error => "Error",
            Status::Starting => "Starting",
            Status::Deleting => "Deleting",
            Status::Creating => "Creating",
        }
    }

    /// Parse the form `/api/sessions` puts on the wire. That endpoint
    /// serializes with `format!("{:?}", inst.status)`, not serde, so the
    /// variant names are `CamelCase` rather than the `lowercase` rename
    /// `as_str` and `Deserialize` use. Kept next to `as_str` so both
    /// spellings of the same enum are read together;
    /// `status_api_wire_form_round_trips` locks the pairing against the
    /// server's formatter.
    ///
    /// `None` for anything unrecognized, which is how a newer daemon
    /// reaches an older client: the caller leaves the row's status alone
    /// rather than inventing one.
    pub fn from_api_str(s: &str) -> Option<Status> {
        match s {
            "Running" => Some(Status::Running),
            "Waiting" => Some(Status::Waiting),
            "Idle" => Some(Status::Idle),
            "Unknown" => Some(Status::Unknown),
            "Stopped" => Some(Status::Stopped),
            "Error" => Some(Status::Error),
            "Starting" => Some(Status::Starting),
            "Deleting" => Some(Status::Deleting),
            "Creating" => Some(Status::Creating),
            _ => None,
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

/// `last_error` the status poller stamps when the tmux server itself could
/// not be reached for a sustained period (past `UNKNOWN_ERROR_WINDOW_*`),
/// as distinct from `TMUX_SESSION_GONE_ERROR`'s "session confirmed absent"
/// case. This is a connectivity failure, not evidence the session's pane
/// was actually torn down, so consumers that treat `TMUX_SESSION_GONE_ERROR`
/// as the calm "Stopped" case must not conflate the two.
pub const TMUX_SERVER_UNREACHABLE_ERROR: &str =
    "tmux server could not be reached. It may be busy or have crashed.";

/// How long a session that has never once been confirmed alive
/// (`Instance::ever_confirmed_present == false`) tolerates a continuous
/// `tmux::SessionExistence::Unknown` before `update_status_with_metadata_inner`
/// latches `Status::Error`. There is nothing that could be "blipping" for a
/// session nobody has ever seen alive (e.g. `aoe add` without `--launch`, or
/// a row whose tmux session failed to spawn), so this stays close to the
/// pre-fix immediate-Error behavior rather than the long grace period below;
/// a couple of `status_poll_loop` ticks (2s each) is enough to smooth over
/// boot jitter without stalling the case a genuinely-dead server needs to
/// surface quickly (see `web/tests/live/ensure-session-restart.spec.ts`,
/// which waits up to 10s for exactly this transition).
const UNKNOWN_ERROR_WINDOW_NEVER_PRESENT: std::time::Duration = std::time::Duration::from_secs(4);

/// How long a session that HAS been confirmed alive tolerates a continuous
/// `tmux::SessionExistence::Unknown` before latching `Status::Error`. Sized
/// with real margin over the ~11s max tmux-server-unreachable blip observed
/// in production debug logs, so a transient hiccup on an actually-running
/// session never trips a false Error.
const UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// The MVP palette for the per-session color label (#2383). Kept deliberately
/// small and status-oriented: red = needs attention / blocked, amber =
/// working / in progress, green = done / ready. `None`/absent clears the dot.
/// Both the CLI (`aoe session color`) and the web PATCH endpoint validate
/// against this list via [`is_valid_session_color`].
pub const SESSION_COLORS: &[&str] = &["red", "amber", "green"];

/// True when `color` is a member of the [`SESSION_COLORS`] palette.
pub fn is_valid_session_color(color: &str) -> bool {
    SESSION_COLORS.contains(&color)
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRepo {
    pub name: String,
    pub source_path: String,
    pub branch: String,
    pub worktree_path: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    /// True when `branch` already existed in this repo and aoe merely checked it
    /// out, which makes branch deletion on session delete a no-op.
    ///
    /// Only ever set by `attach_project` with `--attach-existing-branch` (#3103):
    /// the workspace builder always creates the branch it names, so branch and
    /// worktree ownership coincide for a repo present at creation. Phrased as
    /// "pre-existing" rather than "aoe created it" so the serde default is
    /// correct for every record written before the field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub branch_preexisting: bool,
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

/// The session ids one agent left behind when an engine swap moved a row to a
/// different `tool`, parked in `Instance::prior_tool_session_ids` under that
/// agent's name so a swap back can resume where it left off. Both fields are
/// per-agent namespaces, which is exactly why they cannot travel with the row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PriorToolSession {
    /// The tmux-path conversation id, as `Instance::agent_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_session_id: Option<String>,
    /// The structured-view conversation id, as `Instance::acp_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) acp_session_id: Option<String>,
}

impl PriorToolSession {
    /// Nothing worth parking: an agent that never got a conversation id (never
    /// launched, or `/clear`ed) leaves no entry behind.
    fn is_empty(&self) -> bool {
        self.agent_session_id.is_none() && self.acp_session_id.is_none()
    }
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

/// One durable ownership protocol for every session lifecycle transition.
///
/// A transition first acquires the per-instance lifecycle flock, then records
/// a fresh generation under `Storage::update`. The durable reservation stays
/// held through hooks, external side effects, and the exact-generation commit;
/// callers may release the flock while running reentrant hooks. `status` is
/// presentation state and never proves ownership.
///
/// A crashed owner loses both the flock and, after the TTL, its reservation.
/// Recovery may then acquire a newer generation; exact-generation commits
/// ensure a late result can never mutate or clear that replacement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleOperation {
    Launch,
    Capture,
    Stop,
    Purge,
    Restore,
    Trash,
}

impl LifecycleOperation {
    pub(crate) fn busy_reason(self) -> String {
        format!("busy with lifecycle operation {self:?}")
    }

    pub(crate) fn already_in_progress_reason(self) -> String {
        format!("lifecycle operation {self:?} is already in progress")
    }
}

pub(crate) const NEWER_GENERATION_BUSY_REASON: &str = "busy with a newer lifecycle generation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReservationError {
    Busy(LifecycleOperation),
    GenerationOverflow,
}

impl std::fmt::Display for LifecycleReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(operation) => f.write_str(&operation.already_in_progress_reason()),
            Self::GenerationOverflow => f.write_str("lifecycle generation overflow"),
        }
    }
}

impl std::error::Error for LifecycleReservationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleReservation {
    pub op: LifecycleOperation,
    pub generation: u64,
    pub at: DateTime<Utc>,
}

/// Create-idempotency record for a plugin-created session (#2897). `key` is
/// the plugin-supplied idempotency key, unique within the creating plugin's
/// sessions; `payload_hash` is the host-computed hash of the semantic create
/// request, so a retried key with a different payload is rejected instead of
/// silently returning a session that does not match the request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCreateIdempotency {
    pub key: String,
    pub payload_hash: String,
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
    /// Set once a terminal (non-ACP) smart-rename one-shot has produced output
    /// for this session, so the poller-driven trigger never respawns a title
    /// generator on every later turn. Set only after the one-shot returns
    /// stdout (usable or sanitizer-rejected), never on a transient spawn/timeout
    /// failure, so a slow first turn can still be renamed by a later turn. ACP
    /// sessions use the in-memory `AppState` attempted set instead and never
    /// touch this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub smart_rename_attempted: bool,
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

    /// Durable ownership reservation for every in-flight lifecycle transition.
    /// Acquired atomically with a new `lifecycle_generation`; only that
    /// generation may perform the transition's irreversible phase, commit, or
    /// release it. This is intentionally the only persisted busy signal:
    /// `status` remains multi-writer presentation state, while the per-instance
    /// flock is the short-lived mutex protecting the final side effects and
    /// commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_reservation: Option<LifecycleReservation>,

    /// Namespaced per-session plugin data, keyed by plugin id. Each plugin
    /// owns only its own slot (`plugin_meta["<id>"]`), an opaque JSON value it
    /// reads and writes through the host API that lands with the Tier 1 host
    /// (#2095). Data for an uninstalled plugin is retained, since it is cheap
    /// and reinstalling restores the session's state. Additive: absent in
    /// older `sessions.json` rows, so no migration is needed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugin_meta: std::collections::BTreeMap<String, serde_json::Value>,

    /// Id of the plugin that created this session through the host session
    /// service (#2897). `None` for user-created sessions, including every row
    /// that predates the field. Turn delivery from a plugin is restricted to
    /// sessions whose `created_by_plugin` matches the calling plugin.
    /// Additive: absent in older `sessions.json` rows, so no migration is
    /// needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_plugin: Option<String>,

    /// Create-idempotency record for a plugin-created session, persisted
    /// atomically with the row itself (same `Storage::update`). Scoped to
    /// `created_by_plugin`; retention equals the lifetime of this session
    /// record, so archive/snooze/trash keep deduplicating and a hard delete
    /// releases the key. Additive: absent in older rows, no migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_create_idempotency: Option<PluginCreateIdempotency>,

    /// An initial prompt persisted with the session at create time and not
    /// yet delivered to the agent (#2897). Written in the same
    /// `Storage::update` that creates the row, so the create request and its
    /// first turn are accepted atomically; the session service drains it
    /// once the ACP worker is live (create fast path, and the reconciler
    /// tick after a crash or restart) and clears it after a successful
    /// publish + forward. Delivery is at-least-once: a crash between the
    /// forward and this field's clear re-delivers on the next drain.
    // ponytail: plain text plus a companion attachment-refs field below (no
    // dedup turn id); fold both into a typed record via a vNNN migration if
    // more turn state becomes necessary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_initial_turn: Option<String>,

    /// Attachment refs for `pending_initial_turn` when the queued turn is a
    /// rate-limit resume continuation replaying a prompt that carried
    /// images/files (#3028). Metadata only; bytes stay in the acp_attachments
    /// store and are reloaded at drain time. Empty for create-time initial
    /// turns (those are text-only). `#[serde(default)]` + skip-when-empty keeps
    /// pre-existing rows deserialising unchanged, so no migration is needed.
    /// Serve-only: `PromptAttachmentRef` lives in the serve-gated `acp` module,
    /// and only the structured-view resume path (serve) ever populates it.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_initial_turn_attachments: Vec<crate::acp::state::PromptAttachmentRef>,

    /// Explicit ACP approval-mode id this session should run under (#2897),
    /// applied via `session/set_mode` after every worker (re)spawn, taking
    /// precedence over the legacy `yolo_mode` bool (which stays authoritative
    /// for sessions without an explicit mode; unification is a follow-up).
    /// Set by the plugin host session-create path after the host classified
    /// the mode; also re-asserted before each plugin-delivered turn so a
    /// mode-application failure blocks unattended prompt delivery. Additive:
    /// absent in older rows, no migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_mode_id: Option<String>,

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
    /// Active OMP launch generation. Poller observations must carry this
    /// value through the storage CAS before they may update the durable sid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) omp_capture_generation: Option<String>,
    /// Monotone token for pane lifecycle commits. Async/CLI result merges may
    /// update lifecycle-owned fields only when they are at least this recent.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(crate) lifecycle_generation: u64,

    /// Session ids this row used under a *previous* `tool`, keyed by that
    /// tool's name, so an engine swap back (`claude` -> `pi` -> `claude`)
    /// resumes the original conversation instead of starting a third one.
    /// Written and read only by [`Self::swap_tool`], which parks the outgoing
    /// agent's ids and consumes the incoming agent's entry, so the map holds at
    /// most one entry per tool the row has ever run under.
    ///
    /// `resume_probe_failed_sid` is deliberately not parked with them: a
    /// restored sid is worth one fresh probe (the conversation may well still
    /// be there), and if it is gone the resume-fallback cascade already starts
    /// a new session instead. Additive: absent in older rows, no migration.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) prior_tool_session_ids: HashMap<String, PriorToolSession>,

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

    /// External work-queue dispatcher completion callback: an HTTP POST
    /// fires here when this session transitions to Idle, Waiting, or Error.
    /// Set only at session-create time via `CreateSessionBody.callback_url`;
    /// never exposed in `SessionResponse` (list/get) since URLs commonly
    /// embed bearer tokens. See #3156.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    /// Caller-supplied idempotency key from `POST /api/sessions`, persisted
    /// so a retry (even across a daemon restart) can be matched back to this
    /// instance instead of creating a duplicate. See #3156.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,

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

    /// Per-session color label for at-a-glance status signaling in the web
    /// sidebar (a colored dot next to the title). Purely a decoration: it does
    /// not re-rank the session. Settable from the web context menu and from the
    /// CLI (`aoe session color <id> <color>`) so a running agent can flag its
    /// own state (red = needs attention, amber = working, green = done) without
    /// the user opening the session. `None` clears the dot. Constrained to the
    /// [`SESSION_COLORS`] palette by [`is_valid_session_color`]. Additive:
    /// absent in older `sessions.json` rows, so no migration is needed. See
    /// #2383.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,

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
    /// Reasoning effort ("thought level") this session was explicitly pinned
    /// to, applied through the agent's `category:"thought_level"` config
    /// option after every worker (re)spawn. `None` means the session inherits
    /// whatever the per-agent configured default resolves to at spawn time, so
    /// only an explicit pick (structured view picker, or an explicit effort on
    /// create) is stored here. Cleared on an agent switch: effort vocabularies
    /// are adapter-specific, so the old agent's value is meaningless to the
    /// new one. Additive: absent in older rows, no migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_effort: Option<String>,
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
    /// Whether this in-memory `Instance` has ever observed
    /// `tmux::SessionExistence::Present` since being loaded. `#[serde(skip)]`
    /// like `live_status_baseline`, so it starts `false` on every fresh disk
    /// load / daemon boot. Gates how long `update_status_with_metadata_inner`
    /// tolerates a sustained `SessionExistence::Unknown` before latching
    /// `Status::Error`: a session that was confirmed alive can be riding out
    /// a transient tmux-server blip, but a session that has never once been
    /// confirmed alive has nothing to "blip" from, so `Unknown` escalates
    /// much sooner for it. See `UNKNOWN_ERROR_WINDOW_NEVER_PRESENT` and
    /// `UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT`.
    #[serde(skip)]
    pub ever_confirmed_present: bool,
    /// Instant this instance most recently entered a continuous streak of
    /// `tmux::SessionExistence::Unknown`. `None` while the last known
    /// existence was `Present`/`Absent`; set on the first `Unknown`
    /// observation of a streak and cleared the moment a `Present` or
    /// confirmed `Absent` reading breaks it. Compared against
    /// `UNKNOWN_ERROR_WINDOW_NEVER_PRESENT` /
    /// `UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT` to decide whether a
    /// sustained-`Unknown` session should latch `Status::Error`.
    #[serde(skip)]
    pub unknown_since: Option<std::time::Instant>,

    /// Runtime-only `KEY=VALUE` pairs minted by
    /// `host_hooks.before_session` for the host launch currently being
    /// assembled. They are appended after resolved static profile values in
    /// the protected one-shot pane environment file, so minted values win
    /// without entering tmux argv, pane metadata, or session environment.
    ///
    /// `#[serde(skip)]` is intentional and load-bearing: these values may be
    /// secrets with a short lifetime, so persisting them would leak them and
    /// replay stale values on a later launch. Every host launch re-mints them
    /// from scratch.
    #[serde(skip)]
    pending_host_env: Vec<(String, String)>,

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

/// Find another on-disk row that already holds `sid`.
///
/// Must be evaluated inside a `Storage::update` closure — i.e. under the
/// cross-process storage flock — so the answer reflects writes made by
/// concurrent aoe processes. The drain guards in `sync.rs` enforce the same
/// ownership rule, but only against the calling process's in-memory snapshot:
/// with a TUI and a serve daemon each draining their own pollers, one
/// process can assign a sid to instance A while the other's stale snapshot
/// still sees it unowned and hands it to instance B — both per-instance CAS
/// checks pass and disk ends up with a duplicate. This flock-scoped re-check
/// is the authoritative backstop (#2858).
fn foreign_sid_holder<'a>(
    instances: &'a [Instance],
    instance_id: &str,
    sid: &str,
) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.id != instance_id && i.agent_session_id.as_deref() == Some(sid))
}

/// CAS-write `agent_session_id` to disk. Caller passes the value the
/// in-memory mirror held at last reconcile as `expected_prior`; the closure
/// inside `Storage::update`'s flock skips the write if disk has diverged
/// (peer-poller observed a different sid). On Skipped, callers should
/// reload memory from disk to converge on the peer's value.
///
/// Beyond the per-instance CAS, the closure rejects (as `Skipped`) any write
/// that would violate a disk-level invariant a concurrent process may have
/// established since the caller's snapshot (#2858):
/// - the sid is already owned by another instance on disk;
/// - the target row carries an on-disk `set-session-id` pin
///   (`ResumeIntent::Use`) that the sid contradicts.
pub(crate) fn persist_session_to_storage(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    persist_session_to_storage_guarded(
        profile,
        instance_id,
        session_id,
        expected_prior,
        false,
        None,
        file_watch,
    )
}

pub(crate) fn persist_omp_session_to_storage(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    expected_generation: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    persist_session_to_storage_guarded(
        profile,
        instance_id,
        session_id,
        expected_prior,
        true,
        expected_generation,
        file_watch,
    )
}

fn persist_session_to_storage_guarded(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    guard_generation: bool,
    expected_generation: Option<&str>,
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
        if !instances.iter().any(|i| i.id == instance_id) {
            return Ok(SidWrite::Failed);
        }
        if let Some(holder) = foreign_sid_holder(instances, instance_id, session_id) {
            tracing::warn!(target: "session.store",
                instance_id = %instance_id,
                sid = %session_id,
                holder = %holder.id,
                "sid write rejected under flock: already owned by another instance"
            );
            return Ok(SidWrite::Skipped);
        }
        if let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) {
            if let ResumeIntent::Use(pinned) = &inst.resume_intent {
                if pinned != session_id {
                    tracing::warn!(target: "session.store",
                        instance_id = %instance_id,
                        sid = %session_id,
                        pinned = %pinned,
                        "sid write rejected under flock: contradicts on-disk set-session-id pin"
                    );
                    return Ok(SidWrite::Skipped);
                }
            }
            if guard_generation && inst.omp_capture_generation.as_deref() != expected_generation {
                tracing::warn!(target: "session.store",
                    instance_id = %instance_id,
                    expected_generation = ?expected_generation,
                    disk_generation = ?inst.omp_capture_generation,
                    "OMP generation CAS mismatch; skipping sid persist"
                );
                return Ok(SidWrite::Skipped);
            }
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
    let output = crate::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    crate::tmux::live_any_kind_name_for_id(stdout.lines(), instance_id)
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
    pub status: Status,
    pub lifecycle_generation: u64,
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
            status: inst.status,
            lifecycle_generation: inst.lifecycle_generation,
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
            smart_rename_attempted: false,
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
            lifecycle_reservation: None,
            plugin_meta: std::collections::BTreeMap::new(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
            pending_initial_turn: None,
            #[cfg(feature = "serve")]
            pending_initial_turn_attachments: Vec::new(),
            acp_mode_id: None,
            prior_tool_session_ids: HashMap::new(),
            scratch: false,
            worktree_info: None,
            workspace_info: None,
            sandbox_info: None,
            terminal_info: None,
            agent_session_id: None,
            omp_capture_generation: None,
            lifecycle_generation: 0,
            resume_probe_failed_sid: None,
            resume_intent: ResumeIntent::Default,
            force_fresh_next_launch: false,
            source_profile: String::new(),
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            callback_url: None,
            idempotency_key: None,
            base_branch_override: None,
            color: None,
            view: View::Terminal,
            agent_name: None,
            agent_model: None,
            acp_effort: None,
            acp_session_id: None,
            import_pending: None,
            fork_pending: None,
            last_error_check: None,
            last_start_time: None,
            live_status_baseline: None,
            ever_confirmed_present: false,
            unknown_since: None,
            pending_host_env: Vec::new(),
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

    /// Every repo this session works in, empty for a single-repo session.
    ///
    /// The one accessor consumers read, so nothing has to know that a session
    /// gains repos two ways: created multi-repo, or converted by
    /// `attach_project` (#3103). Both end up in `workspace_info.repos`, which is
    /// the point of converting rather than keeping a second list: a repo added
    /// later is indistinguishable from one present at creation.
    pub fn all_repos(&self) -> &[WorkspaceRepo] {
        self.workspace_info
            .as_ref()
            .map(|ws| ws.repos.as_slice())
            .unwrap_or(&[])
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

    /// Whether this session should render as "dormant" (worker auto-stopped
    /// for inactivity, resumable) rather than with its raw `status`. This is
    /// the single source of the deliberate-stop-vs-dormant precedence: a
    /// deliberate Stop also sets `idle_dormant_since` (see `stop_session`),
    /// so `Status::Stopped` must win here and keep showing the neutral
    /// "Stopped" dot; only a non-stopped row carrying the dormant marker
    /// (the idle-reaper's output) presents as dormant. The reaper only ever
    /// marks structured rows, so this is structured-only in practice. See
    /// #2250 and `idle_dormant_since`.
    pub fn is_shown_dormant(&self) -> bool {
        self.is_idle_dormant() && self.status != Status::Stopped
    }

    /// Mutates launch-owned state. A strictly newer lifecycle generation also
    /// imports its status timestamps and error snapshot as one unit.
    pub fn merge_post_start(&mut self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        if src.lifecycle_generation > self.lifecycle_generation {
            self.idle_entered_at = src.idle_entered_at;
            self.last_accessed_at = src.last_accessed_at;
            self.last_error = src.last_error.clone();
            self.last_error_check = src.last_error_check;
        }
        self.lifecycle_generation = src.lifecycle_generation;
        self.status = src.status;
        self.sandbox_info = src.sandbox_info.clone();
    }

    /// Same fields as `merge_post_start`. Resume-probe failure markers are
    /// copied only when the sid still matches so peer poller writes that land
    /// between phase 2 and phase 3 of the restart remain authoritative.
    pub fn merge_post_restart(&mut self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        self.merge_post_start(src);
        if self.agent_session_id == src.agent_session_id {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    pub fn merge_post_restart_with_baseline(&mut self, before: &Self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        self.merge_post_start(src);
        let generation_can_merge = self.omp_capture_generation == before.omp_capture_generation
            || self.omp_capture_generation == src.omp_capture_generation;
        self.lifecycle_generation = src.lifecycle_generation;
        let sid_unchanged = self.agent_session_id == before.agent_session_id;
        let marker_unchanged = self.resume_probe_failed_sid == before.resume_probe_failed_sid;

        if generation_can_merge {
            self.omp_capture_generation = src.omp_capture_generation.clone();
            self.session_id_poller = src.session_id_poller.clone();
            if sid_unchanged {
                self.agent_session_id = src.agent_session_id.clone();
            }
        } else if src.session_id_poller_is_running() {
            // A concurrent launch already published a third generation. The
            // restarted poller reloads tmux metadata on every tick, so keep
            // that live worker and let it rebind to the newer generation
            // without overwriting the newer durable identity.
            self.session_id_poller = src.session_id_poller.clone();
        }
        if generation_can_merge && marker_unchanged && self.agent_session_id == src.agent_session_id
        {
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
        let disk_has_newer_lifecycle = disk.lifecycle_generation > self.lifecycle_generation;
        if !disk_has_newer_lifecycle {
            disk.last_error_check = self.last_error_check;
            disk.last_error = self.last_error.take();
        }
        disk.last_start_time = self.last_start_time;
        disk.session_id_poller = self.session_id_poller.take();
        disk.retroactive_capture_excludes = std::mem::take(&mut self.retroactive_capture_excludes);
        disk.pane_dead_observed = self.pane_dead_observed;
        disk.force_fresh_next_launch = self.force_fresh_next_launch;
        disk.pending_host_env = std::mem::take(&mut self.pending_host_env);
        disk.source_profile = std::mem::take(&mut self.source_profile);
        disk.ever_confirmed_present = self.ever_confirmed_present;
        disk.unknown_since = self.unknown_since;
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

    /// Carry runtime-only state across a storage reload without constructing a
    /// lifecycle snapshot from two different generations.
    ///
    /// `status` and `idle_entered_at` ARE generation-governed: a strictly newer
    /// disk snapshot (a peer's `commit_reserved_lifecycle_status`) must win over
    /// the stale in-memory copy. `last_error`/`last_error_check` are NOT: no
    /// lifecycle writer (`reserve_/commit_/advance_lifecycle_generation`)
    /// produces an authoritative peer value for them. The only on-disk value is
    /// the one `reconcile_from_disk` round-trips back from this same in-memory
    /// poller state, so there is nothing to defer to and the in-memory value
    /// always wins. Gating it on the generation would let an unrelated
    /// generation bump (stop/unarchive, which pass `status: None`) discard a
    /// freshly poller-derived `TMUX_SESSION_GONE_ERROR`, leaving the row stuck
    /// at `Error`+`None`.
    pub(crate) fn merge_runtime_from_reload(&mut self, previous: &Self) {
        if self.lifecycle_generation <= previous.lifecycle_generation {
            self.status = previous.status;
            self.idle_entered_at = previous.idle_entered_at;
        }
        self.last_error = previous.last_error.clone();
        self.last_error_check = previous.last_error_check;
        self.last_start_time = previous.last_start_time;
        self.session_id_poller = previous.session_id_poller.clone();
        self.retroactive_capture_excludes = previous.retroactive_capture_excludes.clone();
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
        if src.lifecycle_generation >= self.lifecycle_generation {
            self.lifecycle_generation = src.lifecycle_generation;
            self.status = src.status;
            self.last_accessed_at = self.last_accessed_at.max(src.last_accessed_at);
            self.idle_entered_at = src.idle_entered_at;
        }
        // Launch-config fields are TUI-authoritative and only mutated after
        // creation by the restart dialog (engine / command / args swap). They
        // have no peer writer, so a plain copy is safe. Syncing them here is
        // required: `reconcile_from_disk`'s `*self = disk` reload runs on every
        // launch, so a swap that never reached disk is silently reverted and
        // the session respawns with its original tool. See #switching-tools.
        self.tool = src.tool.clone();
        self.command = src.command.clone();
        self.extra_args = src.extra_args.clone();
    }

    /// Move this row to a different `tool` (the TUI restart dialog's engine
    /// swap), parking the outgoing agent's session ids and picking up the
    /// incoming agent's, if it has been here before.
    ///
    /// Session ids live in per-agent namespaces: a Claude UUID means nothing
    /// to codex or gemini, but `is_valid_session_id` accepts any shape, so a
    /// carried-over sid makes the next launch emit `--resume <foreign-sid>`
    /// and the new engine starts by failing to resume. #3077 made the swap
    /// reach disk, which is what exposed this. The rest of what this clears
    /// mirrors the structured-view agent switch (`POST /api/acp/:id/switch`).
    ///
    /// A no-op when `new_tool` is the current tool, so a caller may apply it
    /// to a disk row and an in-memory row independently without the second
    /// call double-stashing.
    ///
    /// Callers must persist the result themselves: `merge_from_tui`
    /// deliberately does not sync these fields (the capture pollers own
    /// `agent_session_id` through CAS writes), so an in-memory-only swap is
    /// reverted by `reconcile_from_disk` on the next launch.
    pub(crate) fn swap_tool(&mut self, new_tool: &str) {
        if new_tool == self.tool {
            return;
        }
        // Park the outgoing agent's conversation under its own name so a swap
        // back to it resumes there instead of starting a third conversation.
        let outgoing = PriorToolSession {
            agent_session_id: self.agent_session_id.take(),
            acp_session_id: self.acp_session_id.take(),
        };
        if !outgoing.is_empty() {
            self.prior_tool_session_ids
                .insert(self.tool.clone(), outgoing);
        }
        self.tool = new_tool.to_string();
        // Consumed, not copied: the row owns exactly one live conversation per
        // agent, and leaving the entry behind would let a later swap restore an
        // id this session has since replaced.
        let restored = self
            .prior_tool_session_ids
            .remove(new_tool)
            .unwrap_or_default();
        self.agent_session_id = restored.agent_session_id;
        self.acp_session_id = restored.acp_session_id;
        self.resume_probe_failed_sid = None;
        // A pin/clear/fork directive names an id in the old agent's namespace,
        // so it cannot survive the swap either.
        self.resume_intent = ResumeIntent::Default;
        // Effort vocabularies are adapter-specific, so the old agent's pick is
        // meaningless to the new one; it falls back to the new agent's default.
        self.acp_effort = None;
        // Same for the pinned model: `claude-opus-4-7` means nothing to codex,
        // and it is re-injected on every spawn, so it has to go too.
        self.agent_model = None;
        // `acp_mode_id` deliberately stays. It is the session's approval
        // posture, and clearing it does not fall back to "default": the spawn
        // path's mode gate is `acp_mode_id.is_some() || yolo_mode`, whose
        // `None` arm resolves the adapter's *bypass* mode id, so dropping an
        // explicit restrictive mode from a `yolo_mode` row would silently
        // escalate the new agent to auto-approve. An unrecognized mode id is a
        // warn-and-continue no-op instead, which is the safe failure. The
        // structured-view agent switch passes it through for the same reason.
        self.import_pending = None;
        self.fork_pending = None;
        // The pinned structured-view agent belongs to the old tool; clearing it
        // lets the spawn path pick the new tool's default agent instead of
        // silently keeping the old backend alive across the swap.
        self.agent_name = None;
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
    /// `status`/`idle_entered_at` apply independently of timestamp only while
    /// the patch's lifecycle generation is current. This prevents an old pane
    /// poll from repainting a newer Stop/Restart/Archive commit.
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
    pub(crate) fn merge_passive_status_patch(&mut self, id: &str, patch: &PassiveStatusPatch) {
        if patch.lifecycle_generation < self.lifecycle_generation {
            tracing::debug!(
                target: "session.store",
                session_id = %id,
                patch_generation = patch.lifecycle_generation,
                disk_generation = self.lifecycle_generation,
                "dropped passive status patch from an older lifecycle generation"
            );
            return;
        }
        self.lifecycle_generation = patch.lifecycle_generation;
        self.status = patch.status;
        self.idle_entered_at = patch.idle_entered_at;
        let Some(incoming) = patch.last_accessed_at else {
            return;
        };
        if self.last_accessed_at.is_some_and(|disk| disk >= incoming) {
            tracing::debug!(
                target: "session.store",
                session_id = %id,
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
        if pre.color != post.color {
            self.color = post.color.clone();
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
        // `workspace_info` deliberately has NO arm. Attaching a project (#3103)
        // converts the session into a workspace, but it does that through
        // `Storage::update` (which takes both lock layers) rather than through a
        // user-action diff, so the value on disk is already authoritative here.
        // Assigning `post`'s copy would let a stale TUI snapshot clobber a
        // conversion a peer landed between the `pre` snapshot and this merge.
        if pre.status != post.status {
            self.status = post.status;
        }
        // Lifecycle ownership is intentionally never spliced from a TUI
        // snapshot. Only transition code holding the per-instance flock may
        // mutate the durable reservation and generation.
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

    /// Longer than any bounded hook, teardown, or worktree move. A crashed
    /// owner cannot retain the reservation forever; a late owner is still
    /// harmless because every commit is generation-checked.
    pub const LIFECYCLE_RESERVATION_TTL: chrono::Duration = chrono::Duration::minutes(10);

    /// Acquire exclusive durable ownership of the next lifecycle generation.
    ///
    /// Even a reservation for the same operation belongs to a peer: operation
    /// kind is not an identity. A caller that already owns a reservation must
    /// retain its returned generation and use
    /// [`Self::lifecycle_reservation_is_owned`] rather than reacquiring by kind.
    pub fn try_acquire_lifecycle_reservation(
        &mut self,
        operation: LifecycleOperation,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, LifecycleReservationError> {
        if let Some(reservation) = self.lifecycle_reservation.as_ref().filter(|reservation| {
            reservation.generation == self.lifecycle_generation && (now - reservation.at) < ttl
        }) {
            return Err(LifecycleReservationError::Busy(reservation.op));
        }

        let generation = self
            .lifecycle_generation
            .checked_add(1)
            .ok_or(LifecycleReservationError::GenerationOverflow)?;
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = Some(LifecycleReservation {
            op: operation,
            generation,
            at: now,
        });
        Ok(generation)
    }

    pub fn lifecycle_reservation_is_owned(
        &self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        self.lifecycle_generation == generation
            && matches!(
                &self.lifecycle_reservation,
                Some(reservation)
                    if reservation.op == operation && reservation.generation == generation
            )
    }

    pub fn has_fresh_lifecycle_reservation(&self, now: DateTime<Utc>) -> bool {
        matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) < Self::LIFECYCLE_RESERVATION_TTL
        )
    }

    pub fn release_lifecycle_reservation_if_owned(
        &mut self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        if self.lifecycle_reservation_is_owned(operation, generation) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    /// Clear a crashed owner's expired reservation. The generation is
    /// deliberately retained as the monotonic cache/result revision.
    pub fn clear_expired_lifecycle_reservation(
        &mut self,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        if matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) >= ttl
        ) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
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

    /// Set (or clear, with `None`) the per-session color label. Only a value
    /// in the [`SESSION_COLORS`] palette is accepted; anything else is
    /// rejected so the sidebar never has to render an unknown swatch. See
    /// #2383.
    pub fn set_color(&mut self, color: Option<String>) -> Result<(), String> {
        match color {
            None => self.color = None,
            Some(c) => {
                if !is_valid_session_color(&c) {
                    return Err(format!(
                        "invalid color {:?}; expected one of: {}, or none",
                        c,
                        SESSION_COLORS.join(", ")
                    ));
                }
                self.color = Some(c);
            }
        }
        Ok(())
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

    /// True iff this session should keep the machine awake: it is active
    /// (`Running`, `Waiting`, `Starting`, or `Creating`), or it went idle less
    /// than `window` ago. A session idle for `>= window` (or
    /// Stopped/Error/Unknown/Deleting) returns false, so the sleep-inhibit
    /// assertion may release. `Waiting`, `Starting`, and `Creating` all count
    /// as active unconditionally, so a session parked waiting for input, or
    /// one still starting or mid-create, holds sleep until it leaves that
    /// status: the predicate ages out only `Idle`, never these three. That is
    /// intentional for the opt-in v1, and nothing ages these three out:
    /// `Waiting` (an unanswered prompt) and `Creating` (a container, worktree,
    /// or submodule setup that never returns) can hold sleep indefinitely,
    /// while `Starting` is bounded by the ~3s `last_start_time` guard in
    /// `update_status_with_metadata_inner` and then re-resolves.
    pub fn has_recent_activity(&self, window: std::time::Duration) -> bool {
        matches!(
            self.status,
            Status::Running | Status::Waiting | Status::Starting | Status::Creating
        ) || matches!(self.idle_age(), Some(age) if age < window)
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

    fn omp_host_environment(&self) -> Vec<String> {
        let mut environment = super::environment::drop_shadowed_host_entries(
            self.profile_host_environment(),
            &self.pending_host_env,
        );
        environment.extend(self.pending_host_env.iter().map(|(key, value)| {
            // These are already-concrete hook values. Escape a leading `$`
            // back into the environment-list grammar so it remains literal.
            if value.starts_with('$') {
                format!("{key}=${value}")
            } else {
                format!("{key}={value}")
            }
        }));
        environment
    }

    /// Capture is safe only for the built-in OMP command and a transparent,
    /// parseable argv. Benign arguments remain supported; store-selecting
    /// flags are interpreted by the capture resolver.
    fn omp_capture_options(&self) -> Option<OmpCliCaptureOptions> {
        if self.tool != "omp" || self.has_command_override() {
            return None;
        }
        let args = super::config::quote_model_value_in_args(&self.extra_args);
        OmpCliCaptureOptions::parse(&args).ok()
    }

    /// Resolve OMP's store, routing environment, and per-launch marker after
    /// `on_launch`. Environment values remain transient; the marker and layout
    /// survive in capture metadata.
    fn resolve_omp_capture_plan(&self, options: &OmpCliCaptureOptions) -> Option<OmpCapturePlan> {
        let resolved = if self.is_sandboxed() {
            let sandbox = self.sandbox_info.as_ref()?;
            let launch_environment = resolved_sandbox_environment(
                &self.source_profile,
                sandbox,
                Path::new(&self.project_path),
            );
            resolve_omp_store_layout_in_container_with_environment(
                &sandbox.container_name,
                &self.container_workdir(),
                &launch_environment,
                options,
            )
        } else {
            resolve_omp_store_layout_with_environment(
                &self.omp_host_environment(),
                &self.project_path,
                options,
            )
        };
        let (layout, routing_fingerprint) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(
                    target: "session.store",
                    instance = %self.id,
                    "OMP capture disabled because launch routing could not be resolved: {error}"
                );
                return None;
            }
        };
        let launch_marker = if self.is_sandboxed() {
            omp_sandbox_launch_marker(&self.id)
        } else {
            match crate::hooks::ensure_instance_dir_path(&self.id) {
                Ok(path) => path.join("omp_launch").to_string_lossy().into_owned(),
                Err(error) => {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "OMP capture disabled because its launch marker directory is unavailable: {error}"
                    );
                    return None;
                }
            }
        };
        Some(OmpCapturePlan {
            layout,
            routing_fingerprint,
            launch_id: Uuid::new_v4().to_string(),
            launch_marker,
            container_runtime: self.is_sandboxed().then(|| {
                crate::session::config::Config::load()
                    .map(|config| config.sandbox.container_runtime)
                    .unwrap_or_default()
            }),
        })
    }

    /// Reconstruct metadata only for a legacy pane which predates launch
    /// snapshots. New launches transport their already-resolved plan directly
    /// into `finalize_launch` and never call this method.
    fn resolve_legacy_omp_capture_metadata(
        &self,
        options: &OmpCliCaptureOptions,
        launched_at_ms: u64,
    ) -> Option<OmpCaptureMetadata> {
        if launched_at_ms == 0 || self.is_sandboxed() {
            return None;
        }
        let layout =
            resolve_omp_store_layout(&self.omp_host_environment(), &self.project_path, options)
                .ok()?;
        Some(OmpCaptureMetadata {
            layout,
            launched_at_ms,
            launch_id: format!("legacy-{}-{launched_at_ms}", self.id),
            launch_marker: String::new(),
            routing_fingerprint: String::new(),
            container_runtime: None,
        })
    }

    /// Load typed launch metadata without the generic env cache. A pane carrying
    /// the regular bootstrap generation is modern; if its hidden metadata is
    /// absent, capture stays disabled instead of being legacy-migrated.
    fn omp_capture_metadata(
        &self,
        session_name: &str,
        options: &OmpCliCaptureOptions,
        launched_at_ms: Option<u64>,
    ) -> Option<OmpCaptureMetadata> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyLayout {
            sessions: PathBuf,
            terminal_sessions: PathBuf,
            kind: OmpStoreKind,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyMetadata {
            layout: LegacyLayout,
            launched_at_ms: u64,
        }

        let bootstrap_generation = || {
            crate::tmux::env::get_env_uncached(
                session_name,
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY,
            )
        };
        if let Some(encoded) = crate::tmux::env::get_hidden_env_uncached(
            session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
        ) {
            if let Ok(mut metadata) = serde_json::from_str::<OmpCaptureMetadata>(&encoded) {
                if metadata.launch_id.trim().is_empty() {
                    if bootstrap_generation().is_some() || self.omp_capture_generation.is_some() {
                        return None;
                    }
                    metadata.launch_id = format!("legacy-{}-{}", self.id, metadata.launched_at_ms);
                    let encoded = serde_json::to_string(&metadata).ok()?;
                    crate::tmux::env::set_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                        &encoded,
                    )
                    .ok()?;
                }
                if validate_omp_capture_metadata(&metadata).is_err() {
                    return None;
                }
                let ready_generation = || {
                    crate::tmux::env::get_hidden_env_uncached(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
                    )
                };
                match bootstrap_generation() {
                    Some(pane_generation)
                        if pane_generation == metadata.launch_id
                            && self.omp_capture_generation.as_deref()
                                == Some(metadata.launch_id.as_str())
                            && ready_generation().as_deref()
                                == Some(metadata.launch_id.as_str()) => {}
                    Some(_) => return None,
                    None if !metadata.launch_marker.is_empty()
                        || self.omp_capture_generation.is_some() =>
                    {
                        return None;
                    }
                    None => {}
                }
                return Some(metadata);
            }

            let legacy: LegacyMetadata = serde_json::from_str(&encoded).ok()?;
            if legacy.launched_at_ms == 0
                || !legacy.layout.sessions.is_absolute()
                || !legacy.layout.terminal_sessions.is_absolute()
                || bootstrap_generation().is_some()
                || self.omp_capture_generation.is_some()
            {
                return None;
            }
            let managed_sessions = legacy.layout.terminal_sessions.parent()?.join("sessions");
            let metadata = OmpCaptureMetadata {
                layout: crate::session::capture::OmpStoreLayout {
                    sessions: legacy.layout.sessions,
                    managed_sessions,
                    terminal_sessions: legacy.layout.terminal_sessions,
                    kind: legacy.layout.kind,
                },
                launched_at_ms: legacy.launched_at_ms,
                launch_id: format!("legacy-{}-{}", self.id, legacy.launched_at_ms),
                launch_marker: String::new(),
                routing_fingerprint: String::new(),
                container_runtime: None,
            };
            let encoded = serde_json::to_string(&metadata).ok()?;
            crate::tmux::env::set_hidden_env(
                session_name,
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                &encoded,
            )
            .ok()?;
            return Some(metadata);
        }

        if bootstrap_generation().is_some() || self.omp_capture_generation.is_some() {
            return None;
        }
        let legacy_watermark_ms = launched_at_ms.or_else(|| {
            crate::tmux::Session::from_name(session_name)
                .created_at_ms()
                .ok()
        })?;
        let metadata = self.resolve_legacy_omp_capture_metadata(options, legacy_watermark_ms)?;
        if bootstrap_generation().is_some()
            || self.omp_capture_generation.is_some()
            || crate::tmux::env::get_hidden_env_uncached(
                session_name,
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            )
            .is_some()
        {
            return None;
        }
        let encoded = serde_json::to_string(&metadata).ok()?;
        crate::tmux::env::set_hidden_env(
            session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            &encoded,
        )
        .ok()?;
        Some(metadata)
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

    /// Switch this structured-view session to terminal mode while keeping the
    /// conversation resumable (#2252). Carries the ACP-side `acp_session_id`
    /// into the terminal-side `agent_session_id` and pins it as the resume
    /// target (`ResumeIntent::Use`), so the next `start()` launches
    /// `<tool> --resume <sid>` instead of a fresh pane, then drops the
    /// structured-view-only ids.
    ///
    /// The caller must have confirmed the agent pairing shares a
    /// CLI-resumable transcript (see `agents::acp_transcript_cli_resumable`).
    /// When `acp_session_id` is unset this only flips the view, leaving no
    /// resume target, which is why the caller also gates on it being present.
    ///
    /// Only the serve-gated `acp_disable` handler calls this, so it is
    /// `cfg(serve)` to stay dead-code-free in a TUI-only build.
    #[cfg(feature = "serve")]
    pub(crate) fn switch_to_terminal_keep_context(&mut self) {
        if let Some(sid) = self.acp_session_id.take() {
            self.agent_session_id = Some(sid.clone());
            self.resume_intent = ResumeIntent::Use(sid);
        }
        self.import_pending = None;
        self.view = View::Terminal;
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
        // Only opencode has an opt-in preassign path; deciding it here (rather
        // than inside acquire_session_id_with) keeps the config read off every
        // non-opencode launch and keeps the inner fn a pure, testable seam.
        let preassign = self.tool == "opencode" && self.opencode_preassign_enabled();
        self.acquire_session_id_with(&|path| {
            preassign
                .then(|| super::capture::preassign_opencode_session_id(path))
                .flatten()
        })
    }

    /// Session-id acquisition with the opencode-preassign step injected as a
    /// seam, so tests can drive the fresh-launch arms without a real opencode
    /// binary or network. Production wraps this with the live preassign helper.
    fn acquire_session_id_with(
        &mut self,
        preassign_opencode: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, bool) {
        match self.resume_intent.clone() {
            ResumeIntent::Use(sid) => {
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                let session_id = self.fresh_launch_session_id(preassign_opencode);
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

        let session_id = self.fresh_launch_session_id(preassign_opencode);

        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.agent_session_id = session_id.clone();
        }

        (session_id, false)
    }

    /// Mint the session id for a brand-new launch. Claude pre-mints a UUID
    /// (`--session-id`); opencode optionally pre-creates its session through
    /// the injected preassign seam (opt-in, returns `None` when disabled or on
    /// failure, deferring to the SQLite poller); every other agent starts
    /// without a pinned id and is captured post-launch.
    fn fresh_launch_session_id(
        &self,
        preassign_opencode: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match self.tool.as_str() {
            "claude" => Some(generate_claude_session_id()),
            "opencode" => preassign_opencode(&self.project_path),
            _ => None,
        }
    }

    /// Whether opt-in opencode session-id preassignment applies to this launch.
    /// Host sessions only: the preassign POST targets a loopback `opencode
    /// serve` a sandboxed agent cannot reach, so containers keep polling.
    fn opencode_preassign_enabled(&self) -> bool {
        if self.is_sandboxed() {
            return false;
        }
        let profile = self.effective_profile();
        if !super::profile_config::resolve_config_or_warn(&profile)
            .session
            .opencode_preassign_session_id
        {
            return false;
        }
        self.opencode_launch_mirrorable_by_ambient_serve()
    }

    /// Whether the ephemeral `opencode serve` used for preassignment provably
    /// hits the same binary and data store as the real launch.
    ///
    /// Preassignment spawns the ambient `opencode` with AoE's own environment.
    /// A command override swaps the binary, and profile-scoped host env can
    /// redirect opencode's data store (e.g. `XDG_DATA_HOME` / `OPENCODE_DB`);
    /// in either case the preassigned id would land in a different store, so
    /// `opencode --session <id>` would fail "Session not found" instead of
    /// gracefully falling back. When this returns false we skip preassignment
    /// and defer to the poller, which reads that same ambient store.
    fn opencode_launch_mirrorable_by_ambient_serve(&self) -> bool {
        !self.has_command_override() && self.profile_host_environment().is_empty()
    }

    /// Full set of session IDs capture must skip for this instance: live tmux
    /// ownership, cascade-cleared ids, and conversations same-project peers
    /// parked while running another tool.
    fn retroactive_capture_exclusion_set(&self) -> HashSet<String> {
        super::capture::compose_exclusion_with_persisted_peers(
            &self.id,
            &self.project_path,
            &self.tool,
            self.tool == "claude" || (self.tool == "codex" && !self.is_sandboxed()),
            &self.effective_profile(),
            &self.retroactive_capture_excludes,
        )
    }

    pub(crate) fn try_retroactive_capture(&self) -> Option<String> {
        let result: Option<String> = match self.tool.as_str() {
            "claude" => {
                // Claude additionally extends the common live and parked-id
                // exclusion with stopped, archived, or pane-less peer sids so
                // the mtime fallback skips peers whose jsonl outlived their
                // tmux session (#2355).
                let exclusion = self.retroactive_capture_exclusion_set();
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
            "omp" => {
                let options = self.omp_capture_options()?;
                let exclusion = self.retroactive_capture_exclusion_set();
                let tmux_session_name = self
                    .tmux_env_session_name()
                    .or_else(|| self.tmux_session().ok().map(|s| s.name().to_string()))?;
                let metadata = self.omp_capture_metadata(&tmux_session_name, &options, None)?;
                if self.is_sandboxed() {
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    let marker = omp_sandbox_launch_marker(&self.id);
                    try_capture_omp_session_id_in_container(
                        &container_name,
                        &metadata,
                        &exclusion,
                        Some(&marker),
                    )
                    .ok()
                } else {
                    capture_omp_session_id(&metadata, &exclusion, &tmux_session_name).ok()
                }
            }
            "codex" => {
                if self.is_sandboxed() {
                    // Sandboxed Codex sessions have instance-private homes, so
                    // their transcript stores cannot contain a sibling's
                    // rollout (#3317). The common helper therefore omits
                    // inactive same-tool peers on this path.
                    let exclusion = self.retroactive_capture_exclusion_set();
                    let container_name = self.sandbox_info.as_ref()?.container_name.clone();
                    try_capture_codex_session_id_in_container(
                        &container_name,
                        &self.container_workdir(),
                        &exclusion,
                    )
                    .ok()
                } else {
                    // Host Codex sessions share `~/.codex/sessions/`. Include
                    // stopped and pane-less same-directory peers so the mtime
                    // scan cannot adopt a sibling's newer conversation.
                    let exclusion = self.retroactive_capture_exclusion_set();
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
            "kimi" => {
                // Kimi records sessions in `~/.kimi-code/session_index.jsonl`
                // keyed by workDir. Host capture reads it directly; sandbox
                // resume is a follow-up (the container's index is not read over
                // `docker exec`), so a sandboxed Kimi session starts fresh on
                // restart, mirroring Copilot.
                if self.is_sandboxed() {
                    None
                } else {
                    let exclusion = self.retroactive_capture_exclusion_set();
                    // Retroactive recovery is unrestricted (no launch floor):
                    // resuming an older session on restart is the goal here.
                    capture_kimi_session_id(&self.project_path, &exclusion, None).ok()
                }
            }
            _ => None,
        };
        result.and_then(validated_session_id)
    }

    /// Canonical `(tool, project_path)` keys shared by two or more id-less
    /// sessions. A read-command self-heal must abstain on these: the
    /// capture-deferred stores are keyed by directory (opencode indexes its
    /// SQLite `session` rows by `directory`, codex/gemini/... by cwd), so when
    /// several co-located id-less sessions of the same tool share one cwd, AoE
    /// cannot attribute a store entry to a specific instance and guessing risks
    /// resuming the wrong conversation. `foreign_sid_holder` already blocks a
    /// duplicate write under the flock; this declines the guess one step
    /// earlier so no instance mis-adopts. Keyed on the canonicalized path so a
    /// symlinked and a realpath spelling of the same dir count as one.
    pub(crate) fn contended_capture_cwds(instances: &[Instance]) -> HashSet<(String, String)> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut contended: HashSet<(String, String)> = HashSet::new();
        for inst in instances {
            // Only a peer that still owns no id AND has a live tmux pane can
            // cause a real attribution ambiguity: a stopped/dead peer's agent
            // is no longer writing to the tool's store, so it cannot be the
            // owner of a freshly-observed entry. Counting it would strand the
            // live session forever behind a ghost. Mirrors the liveness gate
            // `self_heal_session_id` applies to `self`.
            if inst.agent_session_id.is_some() || !inst.tmux_alive_cached() {
                continue;
            }
            let key = inst.contended_capture_key();
            if !seen.insert(key.clone()) {
                contended.insert(key);
            }
        }
        contended
    }

    /// Cache-only tmux liveness for the self-heal gates. Only a HIT
    /// (`Some(true)`) counts as live; a fresh-cache miss, a TTL-expired
    /// snapshot, or an unreachable server all read as not-live. Both self-heal
    /// call sites `refresh_session_cache()` immediately before, so a genuinely
    /// live session is always `Some(true)` here; treating the rest as not-live
    /// at worst DEFERS a best-effort heal, and never forks a `has-session`
    /// subprocess per dead id-less session (which `Session::exists` would, since
    /// it only short-circuits on `Some(true)` and falls through otherwise).
    fn tmux_alive_cached(&self) -> bool {
        let name = crate::tmux::Session::resolve_name(&self.id, &self.title);
        crate::tmux::session_exists_from_cache(&name) == Some(true)
    }

    /// The `(tool, canonical cwd)` identity used for shared-cwd contention.
    /// Canonicalized so a symlinked and a realpath spelling of the same dir
    /// count as one, matching the directory match in `filter_agent_sessions`.
    fn contended_capture_key(&self) -> (String, String) {
        (
            self.tool.clone(),
            super::capture::canonicalize_or_raw(&self.project_path)
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Best-effort backfill of a missing `agent_session_id` from a read-only
    /// CLI command (`aoe status`, `aoe session show`).
    ///
    /// A capture-deferred agent launched purely through the CLI with no
    /// `aoe serve` daemon and no TUI has no long-lived loop draining its
    /// session-id poller. For an agent whose session store is populated lazily
    /// (opencode writes its SQLite `session` row only on the first user turn,
    /// well after the bounded launch-time wait in
    /// [`crate::session::sync::capture_launched_session_id_blocking`] has
    /// elapsed), the id is never observed at launch and, absent a TUI/daemon,
    /// is never recovered. This heals it: the next time the user inspects the
    /// session, read the tool's store directly (the same
    /// [`Self::try_retroactive_capture`] path the TUI/daemon use at next
    /// launch, which needs no live poller) and persist through the guarded
    /// [`persist_session_to_storage`] CAS.
    ///
    /// Gated so it can never adopt a wrong id: only a session that still owns
    /// no id (`agent_session_id.is_none()`), has a plain resume intent
    /// (`ResumeIntent::Default`, so a user-cleared, pinned, or fork-seeded id
    /// is left alone), is not mid-teardown or mid-creation (`Deleting` /
    /// `Creating`), is still in the active bucket (an archived or trashed row
    /// is a sink a read command must not mutate, and `--no-kill` archiving or a
    /// not-yet-torn-down trashed row can still own a live pane), does not share
    /// its cwd with another id-less session of the same tool (`contended`, see
    /// [`Self::contended_capture_cwds`]), and has a live tmux session is
    /// eligible. The live-tmux check is the real liveness guard; the status and
    /// bucket checks skip the rows a read command must leave alone regardless
    /// of pane state. A captured id equal to `resume_probe_failed_sid` is
    /// rejected so a known-bad id the resume cascade already abandoned is not
    /// re-adopted. The sandboxed arms of `try_retroactive_capture` already
    /// return `None` when the container is down, so nothing else is needed for
    /// that case.
    ///
    /// Best-effort: any miss (no id observable yet, a peer already owns it, a
    /// CAS race) is a silent no-op, so a read command never fails or stalls on
    /// this. `aoe status --json` reports only status counts, so the backfill is
    /// invisible there; `aoe session show --json` does surface the healed
    /// `agent_session_id`, and either way the info-level "backfilled
    /// agent_session_id" log below records it.
    pub(crate) fn self_heal_session_id(
        &mut self,
        profile: &str,
        contended: &HashSet<(String, String)>,
    ) {
        if self.agent_session_id.is_some()
            || !self.resume_intent.is_default()
            || matches!(self.status, Status::Deleting | Status::Creating)
            || self.effective_bucket() != SessionBucket::Active
            || contended.contains(&self.contended_capture_key())
        {
            return;
        }
        if !self.tmux_alive_cached() {
            return;
        }
        let file_watch = self.resolve_file_watch();
        let ownership: Result<_> = (|| {
            let storage = super::storage::Storage::new(profile, file_watch.clone())?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&self.id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id)
                else {
                    anyhow::bail!("session disappeared before capture");
                };
                if stored.agent_session_id.is_some()
                    || !stored.resume_intent.is_default()
                    || matches!(stored.status, Status::Deleting | Status::Creating)
                    || stored.effective_bucket() != SessionBucket::Active
                {
                    anyhow::bail!("session is no longer eligible for capture");
                }
                stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Capture,
                        Self::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!("capture blocked: {error}"))
            })?;
            Ok((storage, lifecycle_lock, generation))
        })();
        let Ok((storage, _lifecycle_lock, generation)) = ownership else {
            return;
        };
        let captured = self.try_retroactive_capture();
        let applied = captured.as_ref().is_some_and(|captured| {
            self.resume_probe_failed_sid.as_deref() != Some(captured.as_str())
                && persist_session_to_storage(profile, &self.id, captured, None, &file_watch)
                    == SidWrite::Applied
        });
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored
                .release_lifecycle_reservation_if_owned(LifecycleOperation::Capture, generation))
        });
        if !matches!(released, Ok(true)) {
            tracing::warn!(
                target: "session.sync",
                instance = %self.id,
                "self-heal capture lost its lifecycle reservation before release",
            );
            return;
        }
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = None;
        if applied {
            self.agent_session_id = captured;
            self.resume_probe_failed_sid = None;
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                tool = %self.tool,
                "backfilled agent_session_id from a read-only CLI command; \
                 resume is now available without a TUI or daemon",
            );
        }
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
        // Sandboxed Copilot and Kimi start fresh: their session stores live
        // inside the container (Copilot's SQLite db, Kimi's
        // `~/.kimi-code/session_index.jsonl`), so a host-captured or manually
        // pinned sid would launch `--session[-id] <id>` against an id that does
        // not resolve there. Capture is already host-only above; drop the sid
        // to gate emission too.
        if matches!(self.tool.as_str(), "copilot" | "kimi") && self.is_sandboxed() {
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
            session.create_with_size(&self.project_path, None, size, &self.effective_profile())?;
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

        let managed_codex_home = container_config::managed_codex_home(
            &self.tool,
            Some(&self.detect_as),
            &self.source_profile,
            &self.id,
        )?;
        let env_info = build_docker_env_args_with_managed_codex_home(
            &self.source_profile,
            sandbox,
            std::path::Path::new(&self.project_path),
            managed_codex_home.as_deref(),
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

        // The pane wrapper opens the target values on a protected descriptor.
        // No repo-configured key is installed in the host shell or runtime
        // process environment.
        let session = self.container_terminal_tmux_session_indexed(index)?;
        let is_new = !session.exists();
        if is_new {
            let session = tmux::Session::from_name(session.name());
            session.create_with_size_env_and_container_env(
                &self.project_path,
                Some(&cmd),
                size,
                &self.effective_profile(),
                &[],
                &env_info.env,
            )?;
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
            &self.effective_profile(),
        );
    }

    fn apply_container_terminal_tmux_options(&self, index: u32) {
        let name =
            tmux::ContainerTerminalSession::resolve_name_indexed(&self.id, &self.title, index);
        self.apply_session_tmux_options(&name, &format!("{} (container)", self.title));
    }

    pub fn start(&mut self) -> Result<()> {
        self.start_with_size(None)
    }
    fn commit_lifecycle_launch(
        &mut self,
        storage: &super::storage::Storage,
        restart: bool,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation) {
                return Ok(false);
            }
            stored.status = self.status;
            stored.idle_entered_at = self.idle_entered_at;
            stored.last_accessed_at = self.last_accessed_at;
            stored.sandbox_info = self.sandbox_info.clone();
            if restart && stored.agent_session_id == self.agent_session_id {
                stored.resume_probe_failed_sid = self.resume_probe_failed_sid.clone();
            }
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
            Ok(true)
        })?;
        anyhow::ensure!(
            committed,
            "session {} disappeared or lost its lifecycle reservation before launch commit",
            self.id
        );
        self.lifecycle_reservation = None;
        Ok(())
    }

    fn acquire_lifecycle_reservation(
        &mut self,
        storage: &super::storage::Storage,
        operation: LifecycleOperation,
        status: Option<Status>,
    ) -> Result<u64> {
        let now = Utc::now();
        let mut acquired = None;
        storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(());
            };
            let generation = stored
                .try_acquire_lifecycle_reservation(operation, Self::LIFECYCLE_RESERVATION_TTL, now)
                .map_err(|error| match error {
                    LifecycleReservationError::Busy(holder) => {
                        anyhow::anyhow!("session {} is {}", self.id, holder.busy_reason())
                    }
                    LifecycleReservationError::GenerationOverflow => {
                        anyhow::anyhow!("session {} lifecycle generation overflow", self.id)
                    }
                })?;
            if let Some(status) = status {
                stored.status = status;
                if status != Status::Idle {
                    stored.idle_entered_at = None;
                }
            }
            acquired = Some((generation, stored.lifecycle_reservation.clone()));
            Ok(())
        })?;
        let Some((generation, reservation)) = acquired else {
            anyhow::bail!("session {} no longer exists", self.id);
        };
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = reservation;
        if let Some(status) = status {
            self.status = status;
            if status != Status::Idle {
                self.idle_entered_at = None;
            }
        }
        Ok(generation)
    }

    fn commit_lifecycle_status(
        &mut self,
        storage: &super::storage::Storage,
        operation: LifecycleOperation,
        status: Status,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if !stored.lifecycle_reservation_is_owned(operation, generation) {
                return Ok(false);
            }
            stored.status = status;
            if status != Status::Idle {
                stored.idle_entered_at = None;
            }
            stored.release_lifecycle_reservation_if_owned(operation, generation);
            Ok(true)
        })?;
        anyhow::ensure!(
            committed,
            "session {} disappeared or lost its lifecycle reservation before commit",
            self.id
        );
        self.lifecycle_reservation = None;
        self.status = status;
        if status != Status::Idle {
            self.idle_entered_at = None;
        }
        Ok(())
    }

    fn release_lifecycle_reservation(
        &mut self,
        storage: &super::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored.release_lifecycle_reservation_if_owned(operation, generation))
        })?;
        anyhow::ensure!(
            released,
            "session {} disappeared or lost its lifecycle reservation before release",
            self.id
        );
        self.lifecycle_reservation = None;
        Ok(())
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
        crate::session::validate_instance_id(&self.id)
            .context("refusing to launch: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        let profile = self.effective_profile();
        let storage = super::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;

        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance launch lock")?;
        self.reconcile_from_disk();
        if self.is_structured() || self.tmux_session()?.exists() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        self.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;

        // The durable reservation excludes peer launches while user hooks run.
        // The flock itself must be absent because a hook may invoke aoe for
        // this same session.
        drop(lifecycle_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to reacquire instance launch lock after hooks")?;
        self.reconcile_from_disk();
        if let Err(error) = hook_result {
            self.fail_reserved_launch(&storage, &error, false);
            return Err(error);
        }
        self.ensure_reservation_current_or_fail(&storage)?;
        self.apply_fresh_launch_intent();

        let prepared = match self.prepare_launch_command() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        };
        let result = self
            .spawn_prepared_launch(size, &profile, prepared)
            .and_then(|outcome| {
                self.commit_lifecycle_launch(&storage, false)?;
                Ok(outcome)
            });
        if let Err(error) = result {
            self.fail_reserved_launch(&storage, &error, true);
            return Err(error);
        }
        result
    }

    fn lifecycle_reservation_is_current(
        &self,
        storage: &super::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<bool> {
        let generation = self.lifecycle_generation;
        storage.update(|instances, _groups| {
            Ok(instances
                .iter()
                .find(|instance| instance.id == self.id)
                .is_some_and(|stored| stored.lifecycle_reservation_is_owned(operation, generation)))
        })
    }

    fn reservation_is_current(&self, storage: &super::storage::Storage) -> Result<bool> {
        self.lifecycle_reservation_is_current(storage, LifecycleOperation::Launch)
    }

    fn ensure_reservation_current(&self, storage: &super::storage::Storage) -> Result<()> {
        if self.reservation_is_current(storage)? {
            return Ok(());
        }
        anyhow::bail!(
            "session {} changed while launch hooks were running",
            self.id
        )
    }

    fn ensure_reservation_current_or_fail(
        &mut self,
        storage: &super::storage::Storage,
    ) -> Result<()> {
        match self.ensure_reservation_current(storage) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                Err(error)
            }
        }
    }

    fn fail_reserved_launch(
        &mut self,
        storage: &super::storage::Storage,
        error: &anyhow::Error,
        cleanup_pane: bool,
    ) {
        if !self.reservation_is_current(storage).unwrap_or(false) {
            return;
        }
        if cleanup_pane {
            let _ = self.kill_clean_locked();
        }
        self.last_error = Some(format!("{error:#}"));
        let _ = self.commit_lifecycle_status(storage, LifecycleOperation::Launch, Status::Error);
    }

    fn apply_fresh_launch_intent(&mut self) {
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }
        self.reconcile_sidecar_into_disk();
    }

    fn run_pre_launch_hooks(&mut self, skip_on_launch: bool, profile: &str) -> Result<()> {
        self.mint_host_session_env()?;
        self.run_launch_hooks(skip_on_launch, profile)
    }

    fn prepare_launch_command(&mut self) -> Result<PreparedLaunch> {
        let expected_prior_sid = self.agent_session_id.clone();
        let expected_prior_intent = self.resume_intent.clone();
        let expected_prior_omp_generation = self.omp_capture_generation.clone();
        let (command, is_existing, omp_capture_plan, launch_env) = self.build_launch_command()?;
        Ok(PreparedLaunch {
            command,
            is_existing,
            omp_capture_plan,
            launch_env,
            expected_prior_sid,
            expected_prior_intent,
            expected_prior_omp_generation,
        })
    }

    fn spawn_prepared_launch(
        &mut self,
        size: Option<(u16, u16)>,
        profile: &str,
        mut prepared: PreparedLaunch,
    ) -> Result<LaunchSidOutcome> {
        let session = self.tmux_session()?;
        if session.exists() {
            anyhow::bail!(
                "session {} gained a tmux pane before its reserved launch",
                self.id
            );
        }
        let launch_sid = if prepared.is_existing {
            Some(
                self.agent_session_id
                    .clone()
                    .expect("existing launch command carries agent_session_id"),
            )
        } else {
            None
        };

        tracing::debug!(
            target: "session.store",
            sandboxed = self.is_sandboxed(),
            has_command = prepared.command.is_some(),
            "agent launch command prepared"
        );

        if self.tool == "claude" {
            let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
        }

        let mut omp_capture_metadata = if let Some(plan) = prepared.omp_capture_plan {
            let launched_at_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("system clock predates UNIX_EPOCH during OMP launch")
                .and_then(|elapsed| {
                    u64::try_from(elapsed.as_millis())
                        .context("OMP launch timestamp does not fit in u64")
                })?;
            Some(OmpCaptureMetadata {
                layout: plan.layout,
                launched_at_ms,
                launch_id: plan.launch_id,
                launch_marker: plan.launch_marker,
                routing_fingerprint: plan.routing_fingerprint,
                container_runtime: plan.container_runtime,
            })
        } else {
            None
        };
        let omp_generation_published = self.publish_omp_launch_generation(
            profile,
            omp_capture_metadata.as_ref(),
            prepared.expected_prior_omp_generation.as_deref(),
        );
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            // The launch preamble (`wrap_omp_launch`) rewrites OMP's breadcrumb
            // and writes the capture marker only if the store's terminal-sessions
            // directory already exists; it otherwise falls through to a raw
            // launch and capture silently no-ops. A first-ever OMP launch (or a
            // freshly routed store) has no such directory yet, so ensure it here
            // for the host store. Sandboxed launches resolve a container-side
            // path the host must not create.
            if !self.is_sandboxed() {
                if let Err(error) = std::fs::create_dir_all(&metadata.layout.terminal_sessions) {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "OMP capture may no-op: could not ensure terminal-sessions dir: {error}"
                    );
                }
            }
            prepared.launch_env.pane.push(tmux::PaneEnvMutation::set(
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY.to_string(),
                metadata.launch_id.clone(),
            ));
        }
        session.create_with_size_env_and_container_env(
            &self.project_path,
            prepared.command.as_deref(),
            size,
            profile,
            &prepared.launch_env.pane,
            &prepared.launch_env.container,
        )?;
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let pane_generation = crate::tmux::env::get_env_uncached(
                session.name(),
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY,
            );
            if !omp_generation_published
                || pane_generation.as_deref() != Some(metadata.launch_id.as_str())
            {
                omp_capture_metadata = None;
            }
        }

        self.finalize_launch(
            session.name(),
            profile,
            prepared.expected_prior_sid.as_deref(),
            prepared.expected_prior_intent,
            omp_capture_metadata,
        );

        Ok(match launch_sid {
            Some(sid) => LaunchSidOutcome::Existing { sid },
            None => LaunchSidOutcome::Fresh,
        })
    }

    fn run_launch_hooks(&mut self, skip_on_launch: bool, profile: &str) -> Result<()> {
        if self.tool == "omp" && !self.has_command_override() {
            reject_omp_secret_args(&super::config::quote_model_value_in_args(&self.extra_args))?;
        }
        let agent = crate::agents::get_agent(&self.tool)
            .or_else(|| crate::agents::get_agent(&self.detect_as));
        self.install_agent_status_hooks(agent);
        self.propagate_managed_skills();

        let on_launch_hooks = self.resolve_on_launch_hooks(skip_on_launch, profile);
        if self.is_sandboxed() {
            self.get_container_for_instance()?;
            if let (Some(hook_cmds), Some(sandbox)) =
                (on_launch_hooks.as_ref(), self.sandbox_info.as_ref())
            {
                let hook_env = super::repo_config::lifecycle_env_vars(self);
                let workdir = self.container_workdir();
                if let Err(error) = super::repo_config::execute_hooks_in_container(
                    hook_cmds,
                    &sandbox.container_name,
                    &workdir,
                    &hook_env,
                ) {
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<super::repo_config::HookTimeout>()
                            .is_some()
                    }) {
                        return Err(error);
                    }
                    tracing::warn!(
                        target: "session.store",
                        "on_launch hook failed in container: {}",
                        error
                    );
                }
            }
        } else if let Some(hook_cmds) = on_launch_hooks.as_ref() {
            let hook_env = super::repo_config::lifecycle_env_vars(self);
            if let Err(error) = super::repo_config::execute_hooks(
                hook_cmds,
                Path::new(&self.project_path),
                &hook_env,
            ) {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<super::repo_config::HookTimeout>()
                        .is_some()
                }) {
                    return Err(error);
                }
                tracing::warn!(target: "session.store", "on_launch hook failed: {}", error);
            }
        }
        Ok(())
    }

    /// Construct the command only after hook execution has completed. Keeping
    /// this phase hook-free prevents a revalidation retry from replaying user
    /// code while the lifecycle lock is held.
    fn build_launch_command(&mut self) -> Result<LaunchCommandParts> {
        if self.tool == "omp" && !self.has_command_override() {
            reject_omp_secret_args(&super::config::quote_model_value_in_args(&self.extra_args))?;
        }
        let agent = crate::agents::get_agent(&self.tool)
            .or_else(|| crate::agents::get_agent(&self.detect_as));

        let (cmd, is_existing, omp_capture_plan, launch_env) = if self.is_sandboxed() {
            let image = self
                .sandbox_info
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed instance"))?
                .image
                .clone();
            let container = DockerContainer::new(&self.id, &image);

            // Snapshot only after container hooks have had their final chance
            // to mutate OMP dotenv/config routing, but before any executable
            // pane command exists.
            let omp_capture_plan = self
                .omp_capture_options()
                .and_then(|options| self.resolve_omp_capture_plan(&options));

            let launch_cmd = self.get_launch_command();
            let base_cmd = if self.extra_args.is_empty() {
                launch_cmd
            } else if self.command.is_empty() {
                // Default agent binary: quote a shell-active --model/-m value
                // the same way the host launch path does (build_host_command).
                // A custom command override is the user's own argv, so it is
                // left untouched, matching that path's scoping.
                format!(
                    "{} {}",
                    launch_cmd,
                    super::config::quote_model_value_in_args(&self.extra_args)
                )
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
            let managed_codex_home = container_config::managed_codex_home(
                &self.tool,
                Some(&self.detect_as),
                &self.source_profile,
                &self.id,
            )?;
            let mut env_info = build_docker_env_args_with_managed_codex_home(
                &self.source_profile,
                sandbox,
                std::path::Path::new(&self.project_path),
                managed_codex_home.as_deref(),
            );
            let profile = self.effective_profile();
            if !env_info.docker_args.is_empty() {
                env_info.docker_args.push(' ');
            }
            env_info.docker_args.push_str(&format!(
                "-e AOE_PROFILE={} -e AOE_INSTANCE_ID={}",
                shell_escape(&profile),
                shell_escape(&self.id)
            ));
            let env_part = format!("{} ", env_info.docker_args);
            let raw_command = container.exec_command(Some(&env_part), &tool_cmd);
            let launch_command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_tool_cmd = wrap_omp_launch(&tool_cmd, plan);
                let marked_command = container.exec_command(Some(&env_part), &marked_tool_cmd);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            let wrapped = wrap_command_ignore_suspend(&launch_command, &self.project_path);
            (
                Some(wrapped),
                is_existing,
                omp_capture_plan,
                LaunchEnvironment {
                    pane: Vec::new(),
                    container: env_info.env,
                },
            )
        } else {
            let result = self.build_host_command(agent)?;
            let mut env = super::environment::resolve_host_environment_pairs(
                &self.profile_host_environment(),
            )
            .into_iter()
            .map(|(key, value)| tmux::PaneEnvMutation::set(key, value))
            .collect::<Vec<_>>();
            // The protected file is sourced in order, so freshly minted hook
            // values appended last override same-keyed static profile values.
            env.extend(
                self.pending_host_env
                    .iter()
                    .cloned()
                    .map(|(key, value)| tmux::PaneEnvMutation::set(key, value)),
            );
            if result.2.is_some() {
                // Pin every routing input, including explicit empty values and
                // true absence, so tmux's frozen server environment cannot
                // select another OMP store. The in-pane fingerprint still
                // detects login-file drift.
                env.extend(omp_host_routing_environment(&self.omp_host_environment()));
            }
            (
                result.0,
                result.1,
                result.2,
                LaunchEnvironment {
                    pane: env,
                    container: Vec::new(),
                },
            )
        };

        Ok((cmd, is_existing, omp_capture_plan, launch_env))
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

    /// Make AoE-managed skills available to the agent this session launches, by
    /// reconciling the managed store into that agent's own skills directory
    /// (#3053). Skills reach an agent only as files on disk, so there is nothing
    /// to forward over a protocol; the copy is the mechanism.
    ///
    /// Off unless the user opted in, because it writes into their real agent
    /// config dirs. Best-effort: a root that is missing, read-only, or holds a
    /// conflicting skill is logged and never blocks the launch. A sandboxed
    /// session gets its own copy from `build_container_config`, which reconciles
    /// into the sandbox dir rather than relying on this host pass.
    fn propagate_managed_skills(&self) {
        // Read the global config, not the profile chain. `auto_propagate` is
        // declared `global_only`, and the sandbox path reads it globally too, so
        // resolving it per profile here would let a profile enable host
        // propagation while the same profile's sandboxed sessions ignored it,
        // and would widen a privilege the settings UI never offers per profile.
        let config = super::config::Config::load_or_warn();
        if !config.skills.auto_propagate {
            return;
        }
        let (Some(home), Ok(app_dir)) = (dirs::home_dir(), super::get_app_dir()) else {
            tracing::warn!(target: "session.skills", "skipping skill propagation: no home or app dir");
            return;
        };
        let Some(outcomes) = super::skills_model::sync_for_agent(&home, &app_dir, &self.tool)
        else {
            tracing::debug!(target: "session.skills", agent = %self.tool, "no skills location known for agent");
            return;
        };
        super::skills_model::log_sync_outcomes(&self.tool, &outcomes);
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

    /// Build the tmux command for a host session after all launch hooks have
    /// completed.
    fn build_host_command(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
    ) -> Result<(Option<String>, bool, Option<OmpCapturePlan>)> {
        // Resolve after `on_launch`. The snapshot is checked inside the
        // profile environment assignment scope executed by the login shell;
        // startup-file routing drift therefore disables capture.
        let omp_capture_plan = self
            .omp_capture_options()
            .and_then(|options| self.resolve_omp_capture_plan(&options));

        let profile = self.effective_profile();
        let env_prefix = status_hook_env_prefix(&profile, &self.id, agent);

        if self.command.is_empty() {
            match crate::agents::get_agent(&self.tool) {
                Some(a) => {
                    let mut cmd = a.launch_base_command();
                    if !self.extra_args.is_empty() {
                        // A model id carrying shell metacharacters (a
                        // context-window suffix such as `[1m]`) would abort the
                        // launch line before the agent starts.
                        cmd = format!(
                            "{} {}",
                            cmd,
                            super::config::quote_model_value_in_args(&self.extra_args)
                        );
                    }
                    if self.is_yolo_mode() {
                        if let Some(ref yolo) = a.yolo {
                            apply_yolo_mode(&mut cmd, yolo, false);
                        }
                    }
                    let is_existing = self.apply_session_flags(&mut cmd, "host agent");
                    apply_agent_launch_env(&mut cmd, agent);
                    let raw_command = format!("{}{}", env_prefix, cmd);
                    let command = if let Some(plan) = omp_capture_plan.as_ref() {
                        let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                        gate_omp_launch(&raw_command, &marked_command, plan)
                    } else {
                        raw_command
                    };
                    Ok((
                        Some(wrap_command_ignore_suspend(&command, &self.project_path)),
                        is_existing,
                        omp_capture_plan,
                    ))
                }
                None => Ok((None, false, omp_capture_plan)),
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
            let raw_command = format!("{}{}", env_prefix, cmd);
            let command = if let Some(plan) = omp_capture_plan.as_ref() {
                let marked_command = wrap_omp_host_launch(&env_prefix, &cmd, plan);
                gate_omp_launch(&raw_command, &marked_command, plan)
            } else {
                raw_command
            };
            Ok((
                Some(wrap_command_ignore_suspend(&command, &self.project_path)),
                is_existing,
                omp_capture_plan,
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
        mut omp_capture_metadata: Option<OmpCaptureMetadata>,
    ) {
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let published = serde_json::to_string(metadata).ok().and_then(|encoded| {
                crate::tmux::env::set_hidden_env(
                    session_name,
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                    &encoded,
                )
                .and_then(|()| {
                    crate::tmux::env::set_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
                        &metadata.launch_id,
                    )
                })
                .ok()
            });
            if published.is_none() {
                omp_capture_metadata = None;
            }
        }

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
        if let Some(sid) = &captured_sid {
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

        self.maybe_start_poller_since(omp_capture_metadata);

        self.status = Status::Starting;
        self.last_start_time = Some(std::time::Instant::now());

        // Apply status bar options in a background thread to avoid blocking
        // the TUI on the multiple tmux subprocess calls they require.
        let session_name = session_name.to_string();
        let instance_id_for_log = self.id.clone();
        let title = self.title.clone();
        let branch = self.worktree_info.as_ref().map(|w| w.branch.clone());
        let sandbox = self.sandbox_display();
        let options_profile = profile.to_string();
        match std::thread::Builder::new()
            .name(format!("finalize-tmux-{}", instance_id_for_log))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::tmux::status_bar::apply_all_tmux_options(
                        &session_name,
                        &title,
                        branch.as_deref(),
                        sandbox.as_ref(),
                        &options_profile,
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

    /// Publish the capture plan's generation, or mint a tombstone generation
    /// for an OMP launch whose capture plan could not be resolved.
    fn publish_omp_launch_generation(
        &mut self,
        profile: &str,
        metadata: Option<&OmpCaptureMetadata>,
        expected_prior: Option<&str>,
    ) -> bool {
        if let Some(metadata) = metadata {
            return self.persist_omp_capture_generation(
                profile,
                &metadata.launch_id,
                expected_prior,
            );
        }
        if self.tool != "omp" {
            return true;
        }
        // No capture plan: persist a distinct sentinel so any observation still
        // carrying the prior generation fails the CAS. The `tombstone-` prefix
        // marks it as never-captured in storage/logs (compared for equality,
        // never parsed).
        let tombstone = format!("tombstone-{}", Uuid::new_v4());
        self.persist_omp_capture_generation(profile, &tombstone, expected_prior)
    }

    /// CAS-persist one OMP capture generation and reload the durable winner
    /// when another writer has already advanced it.
    fn persist_omp_capture_generation(
        &mut self,
        profile: &str,
        generation: &str,
        expected_prior: Option<&str>,
    ) -> bool {
        let storage = match super::storage::Storage::new(profile, self.resolve_file_watch()) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(
                    target: "session.store",
                    instance = %self.id,
                    "Failed to open storage for OMP generation persist: {error}"
                );
                return false;
            }
        };
        let outcome = storage.update(|instances, _groups| {
            let Some(instance) = instances.iter_mut().find(|instance| instance.id == self.id)
            else {
                return Ok(SidWrite::Failed);
            };
            if instance.omp_capture_generation.as_deref() != expected_prior {
                return Ok(SidWrite::Skipped);
            }
            instance.omp_capture_generation = Some(generation.to_string());
            Ok(SidWrite::Applied)
        });
        if matches!(outcome, Ok(SidWrite::Applied)) {
            self.omp_capture_generation = Some(generation.to_string());
            return true;
        }
        if let Ok(instances) = storage.load() {
            if let Some(instance) = instances.iter().find(|instance| instance.id == self.id) {
                self.omp_capture_generation = instance.omp_capture_generation.clone();
            }
        }
        tracing::warn!(
            target: "session.store",
            instance = %self.id,
            generation,
            "OMP generation CAS failed; launch continues with capture disabled"
        );
        false
    }

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
        let mut cleared_holder_ids: Vec<String> = Vec::new();
        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter().find(|i| i.id == instance_id) else {
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

            // Disk-level ownership guard, mirrored from
            // `persist_session_to_storage` (see `foreign_sid_holder`): a
            // concurrent process may have assigned this sid to a peer since
            // the caller's snapshot. One exception: a launch that consumes an
            // explicit `set-session-id` pin for exactly this sid. The pin is
            // authoritative (#2708), and it is also the documented repair for
            // an existing duplicate — so instead of rejecting, the pinned
            // launch takes ownership and every stale holder is relieved of
            // the sid (their next capture re-establishes their own
            // conversations). The takeover requires the pin to still be
            // present on the target's on-disk row, not just in the caller's
            // pre-launch snapshot: a peer process may have re-pinned or
            // cleared the intent since, and a stale snapshot must not
            // authorize an ownership transfer the current disk state no
            // longer sanctions.
            if let Some(sid) = new_sid_for_closure.as_deref() {
                let consumed_pin = matches!(
                    &expected_prior_intent_for_closure,
                    ResumeIntent::Use(pinned) if pinned == sid
                ) && matches!(
                    &inst.resume_intent,
                    ResumeIntent::Use(pinned) if pinned == sid
                );
                let holder_ids: Vec<String> = instances
                    .iter()
                    .filter(|i| i.id != instance_id && i.agent_session_id.as_deref() == Some(sid))
                    .map(|i| i.id.clone())
                    .collect();
                if !holder_ids.is_empty() {
                    if consumed_pin {
                        for holder_id in &holder_ids {
                            tracing::warn!(target: "session.store",
                                instance_id = %instance_id,
                                sid = %sid,
                                holder = %holder_id,
                                "explicit pin consumed at launch: taking sid ownership from stale holder"
                            );
                            if let Some(holder) =
                                instances.iter_mut().find(|i| &i.id == holder_id)
                            {
                                holder.agent_session_id = None;
                                holder.resume_probe_failed_sid = None;
                            }
                        }
                        cleared_holder_ids = holder_ids;
                    } else {
                        tracing::warn!(target: "session.store",
                            instance_id = %instance_id,
                            sid = %sid,
                            holder = %holder_ids[0],
                            "sid write rejected under flock in finalize persist: already owned by another instance"
                        );
                        return Ok(SidWrite::Skipped);
                    }
                }
            }

            let Some(inst) = instances.iter_mut().find(|i| i.id == instance_id) else {
                return Ok(SidWrite::Failed);
            };
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
                // Outside the flock: a live cleared holder may still advertise
                // the taken sid via AOE_CAPTURED_SESSION_ID, which
                // `build_exclusion_set` treats as ownership truth, so the new
                // owner would exclude its own sid until the holder's next
                // capture republishes. Unset it best-effort; a holder with no
                // tmux session (stopped) has no env to poison.
                for holder_id in &cleared_holder_ids {
                    let Some(tmux_name) = tmux_env_session_name_for_instance_id(holder_id) else {
                        continue;
                    };
                    if let Err(e) = crate::tmux::env::remove_hidden_env(
                        &tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                    ) {
                        tracing::warn!(target: "session.store",
                            holder = %holder_id,
                            "Failed to clear taken sid from stale holder's tmux env: {e}");
                    }
                }
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
        let name = tmux::TerminalSession::resolve_name_indexed(&self.id, &self.title, index);
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
            container_config::refresh_agent_configs_for_instance(
                &self.effective_profile(),
                &self.id,
                &self.tool,
                Some(&self.detect_as),
            );
            self.backfill_container_workdir(&container);
            if self.is_yolo_mode() {
                container_config::ensure_yolo_trust_config_for_active_agent(
                    &self.tool,
                    Some(&self.detect_as),
                    &self.source_profile,
                    &self.id,
                    &self.container_workdir(),
                );
            }
            return Ok(container);
        }

        if container.exists()? {
            // Restart of a stopped container is a come-up: refresh so a
            // short-lived token is re-minted.
            self.ensure_before_start_env(true)?;
            container_config::refresh_agent_configs_for_instance(
                &self.effective_profile(),
                &self.id,
                &self.tool,
                Some(&self.detect_as),
            );
            container.start()?;
            self.backfill_container_workdir(&container);
            if self.is_yolo_mode() {
                container_config::ensure_yolo_trust_config_for_active_agent(
                    &self.tool,
                    Some(&self.detect_as),
                    &self.source_profile,
                    &self.id,
                    &self.container_workdir(),
                );
            }
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

    /// Mint the `host_hooks.before_session` environment for a host
    /// (non-sandboxed) session launch.
    ///
    /// No-ops for a sandboxed session so a launch runs exactly one of the two
    /// env-minting hooks: `before_start` on container bring-up,
    /// `before_session` on host spawn. Nothing is cached, unlike
    /// [`Self::ensure_before_start_env`], which stashes its result on
    /// `SandboxInfo` so re-attaching a live container does not re-mint, a host
    /// launch always spawns a fresh agent process, so re-running the hook is
    /// both correct and the point (short-lived values get refreshed).
    ///
    /// Gated on [`Self::is_sandboxed`] rather than `sandbox_info.is_some()` so
    /// the condition matches how `build_launch_command` picks its branch: an
    /// instance carrying disabled `SandboxInfo` builds a host command, and so
    /// must mint here, or `before_session` would silently not run for it.
    ///
    /// Resolved from global + profile config only; a repo cannot contribute the
    /// command. See [`super::repo_config::resolve_before_session_hooks`].
    fn mint_host_session_env(&mut self) -> Result<()> {
        self.pending_host_env.clear();
        if self.is_sandboxed() {
            return Ok(());
        }
        let commands = super::repo_config::resolve_before_session_hooks(&self.source_profile);
        if commands.is_empty() {
            return Ok(());
        }
        let hook_env = super::repo_config::lifecycle_env_vars(self);
        self.pending_host_env = super::repo_config::run_before_session_hooks(
            &commands,
            Path::new(&self.project_path),
            &hook_env,
            &[],
        )?;
        Ok(())
    }

    pub fn maybe_start_poller(&mut self) {
        self.maybe_start_poller_since(None);
    }

    fn maybe_start_poller_since(&mut self, omp_metadata: Option<OmpCaptureMetadata>) {
        if !self.supports_session_poller() {
            return;
        }
        let tool = self.tool.as_str();

        let tmux_session_name = self
            .tmux_env_session_name()
            .or_else(|| self.tmux_session().ok().map(|s| s.name().to_string()))
            .unwrap_or_default();
        let omp_metadata = if tool == "omp" {
            let options = match self.omp_capture_options() {
                Some(options) => options,
                None => return,
            };
            match omp_metadata
                .or_else(|| self.omp_capture_metadata(&tmux_session_name, &options, None))
            {
                Some(metadata) => Some(metadata),
                None => return,
            }
        } else {
            None
        };
        let mut poller = SessionPoller::new(tmux_session_name.clone());
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone();
        // Snapshot persisted peer ownership and per-instance excludes at
        // poller-spawn time. This keeps storage reads off the hot polling path
        // while preventing the poller from adopting a conversation another row
        // parked during a tool swap.
        let extra_excludes = self.retroactive_capture_exclusion_set();
        if tool == "omp" {
            let Some(metadata) = omp_metadata.as_ref() else {
                return;
            };
            let poll_fn: crate::session::poller::SessionIdPollFn = if self.is_sandboxed() {
                let container_name = match self.sandbox_info.as_ref() {
                    Some(s) => s.container_name.clone(),
                    None => return,
                };
                Box::new(omp_poll_fn_sandboxed(
                    container_name,
                    self.id.clone(),
                    Some(metadata.launch_marker.clone()),
                    extra_excludes,
                ))
            } else {
                Box::new(omp_poll_fn(self.id.clone(), extra_excludes))
            };
            let cb_instance_id = self.id.clone();
            let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
                tracing::info!(target: "session.store", "Session ID observed for {}: {}", cb_instance_id, new_id);
            });
            let initial_known = initial_known.map(|sid| metadata.session_observation(sid));
            if poller.start_observations(instance_id.clone(), poll_fn, on_change, initial_known) {
                self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
            } else {
                tracing::warn!(target: "session.store",
                    "Failed to start session poller for instance {}, poller will not be stored",
                    instance_id
                );
            }
            return;
        }

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
                let launch_time_ms = crate::util::now_ms() as f64;
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
            "kimi" => {
                // Host-only, mirroring Copilot: the Kimi session index is read
                // from the host `~/.kimi-code`. Sandboxed sessions have no
                // poller and start fresh on restart (sandbox resume is a
                // follow-up).
                if self.is_sandboxed() {
                    return;
                }
                let launch_time_ms = crate::util::now_ms() as f64;
                Box::new(kimi_poll_fn(
                    self.project_path.clone(),
                    self.id.clone(),
                    launch_time_ms,
                    extra_excludes,
                ))
            }
            _ => return,
        };

        let cb_instance_id = self.id.clone();

        // Log-only: the poller's raw observation must NOT be published to the
        // tmux hidden env here. This callback fires before any of the drain
        // guards in `sync.rs` run, and `build_exclusion_set` treats
        // AOE_CAPTURED_SESSION_ID as ownership truth — so a single transient
        // misobservation (e.g. a peer's fresher jsonl in a shared cwd, or the
        // `.claude.json` lastSessionId fallback) would instantly "claim" the
        // peer's sid, make the real owner exclude its own id, abandon its
        // anchor, and adopt a third session's conversation in a cascade
        // (#2858). `drain_and_persist_session_ids` publishes the env for
        // every touched instance after the guards and the CAS have settled.
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
            tracing::info!(target: "session.store", "Session ID observed for {}: {}", cb_instance_id, new_id);
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

    pub(crate) fn session_id_poller_is_running(&self) -> bool {
        self.session_id_poller.as_ref().is_some_and(|poller| {
            poller
                .lock()
                .map(|guard| guard.is_running())
                .unwrap_or_else(|poisoned| poisoned.into_inner().is_running())
        })
    }

    /// Replace a missing or finished poller once its tmux pane is live.
    ///
    /// OMP pollers reload pane metadata on every tick, so a replacement binds
    /// to the durable generation that won any concurrent restart race.
    pub(crate) fn repair_session_id_poller_if_needed(&mut self) -> bool {
        // Structured sessions have ACP workers rather than tmux panes. Their
        // lifecycle is reconciled by the daemon, so probing tmux here can only
        // fail and is especially costly from the native TUI's refresh loop.
        if self.is_structured()
            || !self.supports_session_poller()
            || self.session_id_poller_is_running()
            || !self.has_live_tmux_pane()
        {
            return false;
        }
        self.session_id_poller = None;
        self.maybe_start_poller();
        self.session_id_poller_is_running()
    }

    fn stop_poller(&self) {
        if let Some(ref poller_arc) = self.session_id_poller {
            match poller_arc.lock() {
                Ok(mut poller) => poller.stop(),
                Err(e) => e.into_inner().stop(),
            }
        }
    }
    /// Join the old poller and persist its final capture as a lifecycle
    /// transition.
    pub(crate) fn stop_and_flush_poller(&mut self) {
        let profile = self.effective_profile();
        let storage = match super::storage::Storage::new(&profile, self.resolve_file_watch()) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(target: "session.sync", session = %self.id, "capture storage failed: {error}");
                self.stop_poller();
                self.session_id_poller = None;
                return;
            }
        };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(target: "session.sync", session = %self.id, "capture lifecycle lock failed: {error}");
                self.stop_poller();
                self.session_id_poller = None;
                return;
            }
        };
        self.stop_and_flush_poller_lifecycle_locked();
    }

    fn stop_and_flush_poller_lifecycle_locked(&mut self) {
        // stop_poller() signals the thread but leaves the handle in place, so
        // this is_some() means "a poller existed and may have queued a final
        // observation": drain it before dropping the handle below.
        self.stop_poller();
        if self.session_id_poller.is_some() {
            let file_watch = self.resolve_file_watch();
            let _ = crate::session::sync::drain_and_persist_session_ids_lifecycle_locked(
                std::slice::from_mut(self),
                &file_watch,
            );
        }
        self.session_id_poller = None;
    }

    /// Last-chance exact-pane OMP capture while the old pane still exists.
    fn capture_omp_before_restart(&mut self, profile: &str) {
        self.reconcile_from_disk();
        if self.tool != "omp"
            || self.agent_session_id.is_some()
            || (self.is_sandboxed() && self.omp_capture_generation.is_none())
        {
            return;
        }
        let Some(captured) = self.try_retroactive_capture() else {
            return;
        };
        match persist_omp_session_to_storage(
            profile,
            &self.id,
            &captured,
            None,
            self.omp_capture_generation.as_deref(),
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.agent_session_id = Some(captured);
                self.resume_probe_failed_sid = None;
            }
            SidWrite::Skipped => self.reconcile_from_disk(),
            SidWrite::Failed => {}
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
    fn kill_clean_locked(&self) -> Result<()> {
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

    pub(crate) fn kill_clean(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = super::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = self.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        match self.kill_clean_locked() {
            Ok(()) => lifecycle.commit_lifecycle_status(
                &storage,
                LifecycleOperation::Stop,
                Status::Stopped,
            ),
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
    }

    /// Restart the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
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

    pub(crate) fn restart_with_resume_policy(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, true)
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
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, false)
    }

    fn orchestrate_resume_launch(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
        restart: bool,
    ) -> Result<StartOutcome> {
        crate::session::validate_instance_id(&self.id)
            .context("refusing to start: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        let profile = self.effective_profile();
        let storage = super::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;

        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance start lock")?;
        self.reconcile_from_disk();
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        if !restart && self.tmux_session()?.exists() {
            return Ok(StartOutcome::Fresh);
        }
        if self.status == Status::Error {
            self.status = Status::Idle;
            self.last_error = None;
            self.last_error_check = None;
        }
        self.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;
        if restart {
            self.stop_and_flush_poller_lifecycle_locked();
            self.capture_omp_before_restart(&profile);
        }

        // Keep the generation reservation durable, but allow hooks to invoke
        // aoe against this session without waiting on our flock.
        drop(lifecycle_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to reacquire instance start lock after hooks")?;
        self.reconcile_from_disk();
        if let Err(error) = hook_result {
            self.fail_reserved_launch(&storage, &error, false);
            return Err(error);
        }
        self.ensure_reservation_current_or_fail(&storage)?;
        let skipped_failed_resume_sid = self.apply_resume_policy(resume_policy);
        self.apply_fresh_launch_intent();

        let prepared = match self.prepare_launch_command() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        };
        let result = (|| {
            if restart {
                self.kill_clean_locked()?;
            }
            let launch_outcome = self.spawn_prepared_launch(size, &profile, prepared)?;
            let outcome =
                self.finish_resume_launch(launch_outcome, skipped_failed_resume_sid, &profile)?;
            self.commit_lifecycle_launch(&storage, restart)?;
            Ok(outcome)
        })();
        if let Err(error) = result {
            self.fail_reserved_launch(&storage, &error, true);
            return Err(error);
        }
        result
    }

    fn apply_resume_policy(&mut self, resume_policy: ResumeAttemptPolicy) -> Option<String> {
        if self.resume_intent != ResumeIntent::Default {
            return None;
        }
        let sid = self.agent_session_id.clone()?;
        let resume_allowed_by_policy = match resume_policy {
            ResumeAttemptPolicy::Allow => true,
            ResumeAttemptPolicy::HonorAutoResumeSetting => {
                super::profile_config::resolve_config_or_warn(&self.effective_profile())
                    .session
                    .auto_resume_on_restart
            }
        };
        if !should_attempt_resume(Some(&sid), &self.tool) {
            return None;
        }
        if self.resume_probe_failed_sid.as_deref() == Some(&sid) {
            self.force_fresh_next_launch = true;
            return Some(sid);
        }
        if !resume_allowed_by_policy {
            self.force_fresh_next_launch = true;
        }
        None
    }

    fn finish_resume_launch(
        &mut self,
        launch_outcome: LaunchSidOutcome,
        skipped_failed_resume_sid: Option<String>,
        profile: &str,
    ) -> Result<StartOutcome> {
        let attempted_sid = match launch_outcome {
            LaunchSidOutcome::Existing { sid } if should_attempt_resume(Some(&sid), &self.tool) => {
                Some(sid)
            }
            _ => None,
        };
        let Some(stale_sid) = attempted_sid else {
            return Ok(match skipped_failed_resume_sid {
                Some(sid) => StartOutcome::FreshAfterFailedResume { sid },
                None => StartOutcome::Fresh,
            });
        };

        let probe = match self.probe_settle(RESUME_PROBE_MAX, RESUME_PROBE_POLL) {
            Ok(probe) => probe,
            Err(error) => {
                self.stop_poller();
                self.session_id_poller = None;
                return Err(error);
            }
        };
        if probe == ProbeResult::Alive {
            return Ok(StartOutcome::Resumed);
        }

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
        if self.mark_resume_probe_failed(profile, &stale_sid) == SidWrite::Failed {
            anyhow::bail!(
                "resume probe failed for sid {} for {}, but marker could not be persisted",
                stale_sid,
                self.id,
            );
        }
        self.kill_clean_locked()
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

    pub(crate) fn kill_locked(&self) -> Result<()> {
        self.stop_poller();
        let session = self.tmux_session()?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = super::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = self.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        match self.kill_locked() {
            Ok(()) => lifecycle.commit_lifecycle_status(
                &storage,
                LifecycleOperation::Stop,
                Status::Stopped,
            ),
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
    }

    /// Kill every tmux session owned by this instance (agent, web
    /// terminal, container terminal, tool sub-sessions). Best-effort
    /// and silent; agent/terminal/container terminal failures log at
    /// `debug!` target `session.tmux_cleanup`. Tool sub-sessions are
    /// silent by design via `kill_all_tool_sessions_for_id`.
    pub fn kill_all_tmux_sessions(&self) {
        let profile = self.effective_profile();
        let storage = match super::storage::Storage::new(&profile, self.resolve_file_watch()) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_all_tmux_sessions: lifecycle storage failed"
                );
                return;
            }
        };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_all_tmux_sessions: lifecycle lock failed"
                );
                return;
            }
        };
        let mut lifecycle = self.clone();
        if let Err(error) =
            lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle reservation failed"
            );
            return;
        }
        self.kill_all_tmux_sessions_locked();
        if let Err(error) =
            lifecycle.commit_lifecycle_status(&storage, LifecycleOperation::Stop, Status::Stopped)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle commit failed"
            );
        }
    }

    /// Kill every tmux session owned by this instance while the caller holds
    /// the selected profile's per-instance lifecycle lock.
    ///
    /// Destructive deletion keeps that guard across tmux/container/worktree
    /// teardown and the durable row removal, so it must use this helper rather
    /// than reacquiring the non-reentrant lock via [`Self::kill_all_tmux_sessions`].
    pub(crate) fn kill_all_tmux_sessions_locked(&self) {
        self.kill_all_tmux_sessions_uncoordinated();
    }

    /// Break-glass teardown after force-removal has durably deleted the row.
    ///
    /// A lifecycle reservation cannot be acquired once the row is absent.
    /// Force-removal is limited to an already-Deleting row, so no launch can
    /// race this idempotent cleanup.
    pub(crate) fn kill_all_tmux_sessions_after_forced_removal(&self) {
        self.kill_all_tmux_sessions_uncoordinated();
    }

    fn kill_all_tmux_sessions_uncoordinated(&self) {
        if let Err(e) = self.kill_locked() {
            tracing::debug!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                kind = "agent",
                error = %e,
                "kill_all_tmux_sessions_uncoordinated: kill failed"
            );
        }
        self.kill_ancillary_tmux_sessions_locked();
    }

    pub(crate) fn kill_ancillary_tmux_sessions_locked(&self) {
        crate::tmux::kill_all_terminals_for_id(&self.id);
        crate::tmux::kill_all_tool_sessions_for_id(&self.id);
    }

    /// Kill every tmux session owned by this instance EXCEPT the agent
    /// session (web terminal, container terminal, tool sub-sessions).
    pub fn kill_ancillary_tmux_sessions(&self) {
        let profile = self.effective_profile();
        let storage = match super::storage::Storage::new(&profile, self.resolve_file_watch()) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_ancillary_tmux_sessions: lifecycle storage failed"
                );
                return;
            }
        };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_ancillary_tmux_sessions: lifecycle lock failed"
                );
                return;
            }
        };
        let mut lifecycle = self.clone();
        if let Err(error) =
            lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_ancillary_tmux_sessions: lifecycle reservation failed"
            );
            return;
        }
        self.kill_ancillary_tmux_sessions_locked();
        if let Err(error) =
            lifecycle.release_lifecycle_reservation(&storage, LifecycleOperation::Stop)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_ancillary_tmux_sessions: lifecycle release failed"
            );
        }
    }

    /// Stop the session and its sandbox container under the same lifecycle
    /// lock used by launch/restart.
    pub fn stop(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = super::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance stop lock")?;
        let mut lifecycle = self.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        let teardown = self.kill_locked().and_then(|()| {
            crate::session::worktree_edit::stop_sandbox_container(&self.id, self.is_sandboxed())
        });
        match teardown {
            Ok(()) => {
                lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Stopped,
                )?;
                crate::hooks::cleanup_hook_status_dir(&self.id);
                Ok(())
            }
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
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
    pub fn update_status_with_metadata(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
        let baseline = self.live_status_baseline;
        self.update_status_with_metadata_inner(metadata, resolved_name);
        if let Some(prev) = baseline {
            if prev != self.status {
                self.log_status_transition(prev);
                let now = Utc::now();
                self.last_accessed_at = Some(now);
                self.idle_entered_at = if self.status == Status::Idle {
                    Some(now)
                } else {
                    None
                };
            }
        }
        self.live_status_baseline = Some(self.status);
    }

    /// One `info` line per observed status transition, carrying the evidence a
    /// wrong-state report needs: the hook file's value and age at the moment
    /// of the flip, and (for Claude) a content-free fingerprint of which pane
    /// markers were on screen. Intermittent status flakes can't be reproduced
    /// on demand, so this trail must land at the default log level; the
    /// per-rule detector traces stay at debug/trace for when a report narrows
    /// the hunt.
    ///
    /// Sessions are identified by the opaque instance id, not the title:
    /// smart-rename derives titles from the first prompt, so a title in an
    /// always-on log would leak conversation-derived text and break the
    /// content-free promise the pane fingerprint keeps. `aoe list` maps ids
    /// back to titles when correlating.
    ///
    /// The hook file is re-read here rather than threaded out of the detection
    /// path, so a value that changed in the microseconds since detection can
    /// disagree with the decision; the age field makes that visible. Costs one
    /// file stat, plus one pane capture for Claude, gated on an actual
    /// transition, so steady-state polling pays nothing.
    fn log_status_transition(&self, prev: Status) {
        let detection_tool = if self.detect_as.is_empty() {
            &self.tool
        } else {
            &self.detect_as
        };
        let hook = crate::hooks::read_hook_status(&self.id);
        let hook_age_ms = crate::hooks::read_hook_status_age(&self.id).map(|age| age.as_millis());
        if detection_tool == "claude" {
            let fingerprint = self
                .tmux_session()
                .ok()
                .and_then(|s| s.capture_pane(50).ok())
                .map(|pane| tmux::claude_pane_marker_fingerprint(&pane))
                .unwrap_or_else(|| "capture_failed".to_string());
            tracing::info!(target: "session.status_change",
                "{} [{}] {:?} -> {:?} (hook={:?} hook_age_ms={:?} pane={})",
                self.id, detection_tool, prev, self.status, hook, hook_age_ms, fingerprint
            );
        } else {
            tracing::info!(target: "session.status_change",
                "{} [{}] {:?} -> {:?} (hook={:?} hook_age_ms={:?})",
                self.id, detection_tool, prev, self.status, hook, hook_age_ms
            );
        }
    }

    /// Drop a [`TMUX_SESSION_GONE_ERROR`] left on a row that no longer has a
    /// tmux pane to speak for it, so the UI stops showing a message that cannot
    /// apply to it any more (a session converted to, or restarted in, the
    /// structured view).
    ///
    /// Shared by the structured short-circuit below and by the daemon poll
    /// loop's `skip_tmux_decision_for_structured`, which skips that
    /// short-circuit outright; one copy keeps the two from drifting.
    pub(crate) fn clear_stale_tmux_error(&mut self) {
        if self.last_error.as_deref() == Some(TMUX_SESSION_GONE_ERROR) {
            self.last_error = None;
        }
    }

    fn update_status_with_metadata_inner(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
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
            self.clear_stale_tmux_error();
            if self.status == Status::Error {
                self.status = Status::Idle;
            }
            return;
        }

        if self.status == Status::Error && self.last_error.is_some() {
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

        let session = match resolved_name {
            Some(name) => tmux::Session::from_name(name),
            None => match self.tmux_session() {
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
            },
        };

        match session.existence() {
            tmux::SessionExistence::Absent => {
                tracing::trace!(target: "session.store",
                    "status '{}': session.existence()=Absent (tmux name={}), setting Error",
                    self.title,
                    session.name()
                );
                self.unknown_since = None;
                self.status = Status::Error;
                if self.last_error.is_none() {
                    self.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());
                }
                self.last_error_check = Some(std::time::Instant::now());
                return;
            }
            tmux::SessionExistence::Unknown => {
                // The tmux server itself was unreachable (stale socket,
                // refused connection), not a confirmed-absent session. This
                // is NOT evidence of anything on its own: a session that has
                // been confirmed alive rides out a bounded grace window
                // (absorbing a transient hiccup, the false-alarm bug this
                // branch exists to fix), but a session that has never once
                // been confirmed alive has nothing to "blip" from and gets a
                // much shorter one.
                let window = if self.ever_confirmed_present {
                    UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT
                } else {
                    UNKNOWN_ERROR_WINDOW_NEVER_PRESENT
                };
                let unknown_since = *self
                    .unknown_since
                    .get_or_insert_with(std::time::Instant::now);
                if unknown_since.elapsed() < window {
                    tracing::debug!(target: "session.store",
                        "status '{}': tmux server unreachable for {:?} (< {:?} window, ever_confirmed_present={}), retaining status {:?}",
                        self.title,
                        unknown_since.elapsed(),
                        window,
                        self.ever_confirmed_present,
                        self.status
                    );
                    return;
                }
                tracing::trace!(target: "session.store",
                    "status '{}': tmux server unreachable for {:?} (>= {:?} window, ever_confirmed_present={}), setting Error",
                    self.title,
                    unknown_since.elapsed(),
                    window,
                    self.ever_confirmed_present
                );
                self.status = Status::Error;
                if self.last_error.is_none() {
                    self.last_error = Some(TMUX_SERVER_UNREACHABLE_ERROR.to_string());
                }
                self.last_error_check = Some(std::time::Instant::now());
                return;
            }
            tmux::SessionExistence::Present => {
                self.unknown_since = None;
                self.ever_confirmed_present = true;
            }
        }

        let is_dead = metadata
            .map(|m| m.pane_dead)
            .unwrap_or_else(|| session.is_pane_dead());

        let pane_cmd = metadata
            .and_then(|m| m.pane_current_command.clone())
            .or_else(|| tmux::utils::pane_current_command(session.name()));

        tracing::trace!(target: "session.store",
            "status '{}': exists=true, is_dead={}, pane_cmd={:?}, tool={}, cmd_override={}",
            self.title,
            is_dead,
            pane_cmd,
            self.tool,
            self.has_command_override()
        );

        // Two detection identities: hooks are installed for (and must be
        // interpreted by) the `agent_detect_as` alias when one is set, so
        // hook reconciliation keeps the alias identity. The pane fallback
        // below instead prefers the session's own configured status rules
        // over the alias.
        let hook_tool: &str = if self.detect_as.is_empty() {
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
                // Three hook/pane mismatches need the pane captured and consulted:
                //
                // 1. Running hook, pane parked on a blocking prompt: Codex and
                //    Claude keep re-emitting running-mapped hooks while blocked,
                //    so a Running write can mean "still working" or "waiting on
                //    the user". Their reconcilers read the pane to tell which
                //    (Codex: plan/numbered prompts; Claude: tool-approval
                //    prompts, see #1913).
                // 2. Waiting hook gone stale: several agents write `waiting`
                //    directly when a prompt appears (Claude AskUserQuestion /
                //    permission prompt, Codex PermissionRequest, Cursor / Qwen /
                //    Gemini permission notifications). Esc-cancelling the prompt
                //    fires no completing hook, so the file sticks on `waiting`
                //    until the next turn (regression from #2937). Any such agent
                //    is reconciled against the pane by reconcile_waiting_hook.
                // 3. Idle hook on a session last observed Running/Waiting:
                //    Claude's `Notification(idle_prompt)` hook is
                //    fire-and-forget, so when a queued prompt submits at turn
                //    end its `idle` write can land after `UserPromptSubmit`'s
                //    `running`, showing Idle mid-turn until the first
                //    PreToolUse rewrites the file. The previous-status gate
                //    keeps parked sessions (the dominant steady state) from
                //    paying a capture per poll; see
                //    reconcile_claude_idle_hook_status.
                let reconciles_running = (hook_tool == "codex" || hook_tool == "claude")
                    && hook_status == Status::Running;
                let reconciles_waiting = hook_status == Status::Waiting;
                let reconciles_idle = hook_tool == "claude"
                    && hook_status == Status::Idle
                    && matches!(self.status, Status::Running | Status::Waiting);
                self.status = if reconciles_running || reconciles_waiting || reconciles_idle {
                    match session.capture_pane(50) {
                        Ok(pane_content) => {
                            if reconciles_waiting {
                                tmux::reconcile_waiting_hook(hook_tool, &pane_content)
                            } else if reconciles_idle {
                                tmux::reconcile_claude_idle_hook_status(&pane_content)
                            } else if hook_tool == "codex" {
                                tmux::reconcile_codex_hook_status(hook_status, &pane_content)
                            } else {
                                let running_age = crate::hooks::read_hook_status_age(&self.id);
                                tmux::reconcile_claude_hook_status(
                                    hook_status,
                                    &pane_content,
                                    running_age,
                                )
                            }
                        }
                        Err(e) => {
                            tracing::trace!(
                                "status '{}': {} hook fallback pane capture failed: {}",
                                self.title,
                                hook_tool,
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

        // Pane-fallback identity: the session's own configured status rules
        // outrank the `agent_detect_as` alias; without rules the alias applies.
        let pane_tool =
            tmux::status_rules::detection_tool(&self.source_profile, &self.tool, &self.detect_as);
        let pane_content = session.capture_pane(50).unwrap_or_default();
        let detected =
            tmux::detect_status_from_content_in(&self.source_profile, &pane_content, pane_tool);
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
                .and_then(|m| {
                    m.pane_current_command.as_deref().map(|current_command| {
                        tmux::utils::is_pane_running_shell_command(
                            current_command,
                            m.pane_start_command_is_protected,
                        )
                    })
                })
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
        // A Claude pane with unsubmitted typed text in the input box can show
        // no running signal at all while a turn streams (typing suppresses the
        // `esc to interrupt` hint and prose streaming renders no spinner), and
        // that pane is identical to a parked one minus the completion line. In
        // the ambiguous state, hold an already-observed Running rather than
        // flap a working session to Idle; the completion line rendered at turn
        // end releases the hold on the next poll.
        let detected = if detected == Status::Idle
            && !shell_stale
            && !is_dead
            && self.status == Status::Running
            && pane_tool == "claude"
            && tmux::claude_pane_is_ambiguous_typed_prompt(&pane_content)
        {
            tracing::debug!(target: "session.store",
                "status '{}': holding Running over ambiguous typed-prompt Idle", self.title);
            Status::Running
        } else {
            detected
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
        self.update_status_with_metadata(None, None);
    }

    /// Capture the session's window for the preview, with any panes the user
    /// split off composited in. `capture-pane` has no size parameters: the
    /// window is captured at its own dimensions.
    pub fn capture_output_composited(&self, lines: usize) -> Result<String> {
        self.tmux_session()?.capture_window_composited(lines)
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

/// Run a script through a dedicated descriptor so its size is not constrained
/// by the per-argument exec limit and the launched agent retains the pane TTY
/// on standard input. The delimiter grows until it cannot close a here-document
/// present in user-controlled command text.
fn shell_stdin_command(shell: &str, login: bool, script: &str, stem: &str) -> String {
    let mut delimiter = stem.to_string();
    while script.lines().any(|line| line == delimiter) {
        delimiter.push('_');
    }
    let flag = if login { "-l " } else { "" };
    format!(
        "{} {flag}/dev/fd/3 3<<'{delimiter}'\n{script}\n{delimiter}",
        shell_escape(shell)
    )
}

/// Disable terminal suspension before replacing the pane process with the
/// requested command. The user's POSIX login shell reads the launch script
/// from a dedicated descriptor, keeping both large prompts and the pane TTY.
///
/// `working_dir` is re-asserted with `cd` as the first statement in that
/// script, after the login shell's profile/rc files have run. tmux's
/// `new-session -c` only sets the shell's initial cwd, and a `-l` login
/// shell's rc files (or an nvm/direnv hook) can `cd` away before the agent
/// starts; re-cd-ing here wins regardless (#3265).
fn wrap_command_ignore_suspend(cmd: &str, working_dir: &str) -> String {
    let user = super::environment::user_shell();
    let posix = super::environment::user_posix_shell();
    let cd = super::environment::shell_escape(working_dir);
    let script = format!("cd {cd} || exit 1\nstty susp undef\nexec env {cmd}");
    shell_stdin_command(&posix, user == posix, &script, "AOE_LAUNCH_BODY")
}

/// Build a post-login routing fingerprint check without embedding any routing
/// value in argv. The pane hashes its live environment through stdin; if no
/// SHA-256 utility exists or startup files changed routing, capture is skipped
/// and the original OMP command runs untouched.
fn omp_routing_fingerprint_check(plan: &OmpCapturePlan) -> String {
    let keys = crate::session::capture::OMP_STORE_ENV_KEYS.join(" ");
    format!(
        "route_payload() {{ \
           for k in {keys}; do \
             eval \"s=\\${{$k+x}};v=\\${{$k-}}\"; \
             if [ \"$s\" ]; then printf '%s\\0001\\000%s\\000' \"$k\" \"$v\"; \
             else printf '%s\\0000\\000\\000' \"$k\"; fi; \
           done; \
         }}; \
          if command -v sha256sum >/dev/null 2>&1; then \
           route_fingerprint=$(route_payload | command sha256sum) || launch_raw; \
          elif command -v shasum >/dev/null 2>&1; then \
           route_fingerprint=$(route_payload | command shasum -a 256) || launch_raw; \
          else launch_raw; fi; \
          route_fingerprint=${{route_fingerprint%% *}}; \
          [ \"$route_fingerprint\" = {} ] || launch_raw; ",
        shell_escape(&plan.routing_fingerprint)
    )
}

/// Wait briefly for the parent to publish this launch generation's hidden
/// capture metadata. A timeout runs the uninstrumented command, so capture
/// fails closed without preventing the agent from starting.
fn gate_omp_launch(raw_command: &str, marked_command: &str, plan: &OmpCapturePlan) -> String {
    let expected = format!(
        "{}={}",
        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
        plan.launch_id
    );
    let script = format!(
        "expected={}; ready=; attempt=0; \
         while [ \"$attempt\" -lt 100 ]; do \
           ready=$(tmux show-environment -h -t \"$TMUX_PANE\" {} 2>/dev/null) || ready=; \
           [ \"$ready\" = \"$expected\" ] && break; \
           attempt=$((attempt + 1)); sleep 0.05; \
         done\n\
         if [ \"$ready\" = \"$expected\" ]; then\n\
           exec env {marked_command}\n\
         else\n\
           exec env {raw_command}\n\
         fi",
        shell_escape(&expected),
        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
    );
    shell_stdin_command("sh", false, &script, "AOE_OMP_CAPTURE_GATE")
}

/// Apply profile assignments to the marker wrapper itself, not only to its
/// eventual OMP command. The routing fingerprint must observe the same
/// effective environment that OMP inherits.
fn wrap_omp_host_launch(env_prefix: &str, tool_cmd: &str, plan: &OmpCapturePlan) -> String {
    format!("{env_prefix}{}", wrap_omp_launch(tool_cmd, plan))
}

/// Bind capture to the exact launch PTY. A valid pre-launch breadcrumb is
/// rewritten to a lexically different but equivalent session path; the marker
/// records that pending path so capture waits until OMP rewrites the breadcrumb.
/// If no breadcrumb exists, install a fresh sentinel from a private directory
/// by a no-clobber hardlink. Invalid breadcrumbs, collisions, symlinks, and
/// write failures launch raw OMP without capture.
fn wrap_omp_launch(tool_cmd: &str, plan: &OmpCapturePlan) -> String {
    let breadcrumb_tmp_leaf = format!(".aoe-omp-breadcrumb-{}", plan.launch_id);
    let pending_sentinel = plan
        .layout
        .managed_sessions
        .join(format!(".aoe-pending-{}", plan.launch_id))
        .join(format!("aoe-pending_{}.jsonl", plan.launch_id));
    let fingerprint_check = omp_routing_fingerprint_check(plan);
    let marked_launch = format!(
        "tool_cmd={}; \
         launch_raw() {{ exec sh -c \"$tool_cmd\"; }}; \
         {}\
         tty_path=$(tty) || launch_raw; \
         terminal_id=${{tty_path#/dev/}}; \
         [ \"$terminal_id\" != \"$tty_path\" ] && [ -n \"$terminal_id\" ] || launch_raw; \
         terminal_id=$(printf '%s' \"$terminal_id\" | tr '/' '-') || launch_raw; \
         terminal_dir={}; \
         [ -d \"$terminal_dir\" ] && [ ! -L \"$terminal_dir\" ] || launch_raw; \
         pending=; \
         breadcrumb=\"$terminal_dir/$terminal_id\"; \
         if [ -f \"$breadcrumb\" ] && [ ! -L \"$breadcrumb\" ]; then \
           breadcrumb_bytes=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | LC_ALL=C wc -c | tr -d '[:space:]'); \
           case \"$breadcrumb_bytes\" in ''|*[!0-9]*) breadcrumb_bytes=16385 ;; esac; \
           [ \"$breadcrumb_bytes\" -le 16384 ] || launch_raw; \
           crumb_cwd=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '1p') || launch_raw; \
           crumb_path=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '2p') || launch_raw; \
           crumb_marker=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '3p') || launch_raw; \
           crumb_lines=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '$=') || launch_raw; \
           case \"$crumb_lines:$crumb_marker\" in '2:'|'3:fresh') ;; *) launch_raw ;; esac; \
           [ -n \"$crumb_cwd\" ] && [ -n \"$crumb_path\" ] || launch_raw; \
           case \"$crumb_path\" in \
             /*) crumb_dir=${{crumb_path%/*}}; crumb_base=${{crumb_path##*/}}; \
                 [ -n \"$crumb_dir\" ] || crumb_dir=/; \
                 if [ \"$crumb_dir\" = / ]; then pending=\"/./$crumb_base\"; \
                 else pending=\"$crumb_dir/./$crumb_base\"; fi ;; \
             *) pending=\"./$crumb_path\" ;; \
           esac; \
           if [ \"$crumb_marker\" = fresh ]; then \
             rewritten_bytes=$(printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           else \
             rewritten_bytes=$(printf '%s\\n%s\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           fi; \
           case \"$rewritten_bytes\" in ''|*[!0-9]*) rewritten_bytes=16385 ;; esac; \
           [ \"$rewritten_bytes\" -le 16384 ] || launch_raw; \
           breadcrumb_tmp_dir=\"$terminal_dir\"/{}.tmp.$$; \
           (umask 077; mkdir \"$breadcrumb_tmp_dir\") || launch_raw; \
           breadcrumb_tmp=\"$breadcrumb_tmp_dir/breadcrumb\"; \
           if [ \"$crumb_marker\" = fresh ]; then \
             (umask 077; set -C; printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           else \
             (umask 077; set -C; printf '%s\\n%s\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           fi; \
           mv -f -- \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw; \
           rmdir \"$breadcrumb_tmp_dir\" 2>/dev/null || :; \
         elif [ ! -e \"$breadcrumb\" ] && [ ! -L \"$breadcrumb\" ]; then \
           crumb_cwd=$(pwd -P) || launch_raw; \
           [ -n \"$crumb_cwd\" ] || launch_raw; \
           pending={}; \
           rewritten_bytes=$(printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           case \"$rewritten_bytes\" in ''|*[!0-9]*) rewritten_bytes=16385 ;; esac; \
           [ \"$rewritten_bytes\" -le 16384 ] || launch_raw; \
           breadcrumb_tmp_dir=\"$terminal_dir\"/{}.tmp.$$; \
           (umask 077; mkdir \"$breadcrumb_tmp_dir\") || launch_raw; \
           breadcrumb_tmp=\"$breadcrumb_tmp_dir/breadcrumb\"; \
           (umask 077; set -C; printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           ln -n \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw; \
           rm -f -- \"$breadcrumb_tmp\" || launch_raw; \
           rmdir \"$breadcrumb_tmp_dir\" 2>/dev/null || :; \
         else \
           launch_raw; \
         fi; \
         [ -n \"$pending\" ] || launch_raw; \
         marker_tmp_dir={}.tmp.$$; \
         (umask 077; mkdir \"$marker_tmp_dir\") || launch_raw; \
         marker_tmp=\"$marker_tmp_dir/marker\"; \
         (umask 077; set -C; printf '%s\\n%s\\n%s\\n%s\\n' \"$terminal_id\" {} \"$pending\" \"$route_fingerprint\" > \"$marker_tmp\") || launch_raw; \
         mv -f -- \"$marker_tmp\" {} || launch_raw; \
         rmdir \"$marker_tmp_dir\" 2>/dev/null || :; \
         exec sh -c \"$tool_cmd\"",
        shell_escape(tool_cmd),
        fingerprint_check,
        shell_escape(&plan.layout.terminal_sessions.to_string_lossy()),
        shell_escape(&breadcrumb_tmp_leaf),
        shell_escape(&pending_sentinel.to_string_lossy()),
        shell_escape(&breadcrumb_tmp_leaf),
        shell_escape(&plan.launch_marker),
        shell_escape(&plan.launch_id),
        shell_escape(&plan.launch_marker),
    );
    shell_stdin_command("sh", false, &marked_launch, "AOE_OMP_MARKED_LAUNCH")
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
    use crate::session::test_support::EnvGuard;
    use tracing_test::traced_test;

    #[test]
    #[serial_test::serial]
    fn contended_capture_cwds_flags_only_live_colocated_idless_same_tool() {
        let cwd = std::env::current_dir().unwrap();
        let p = cwd.to_str().unwrap();
        let canon = crate::session::capture::canonicalize_or_raw(p)
            .to_string_lossy()
            .into_owned();
        let mk = |title: &str, tool: &str, sid: Option<&str>| {
            let mut i = Instance::new(title, p);
            i.tool = tool.to_string();
            i.agent_session_id = sid.map(str::to_string);
            i
        };
        let instances = vec![
            mk("a", "opencode", None),          // id-less opencode, live, same cwd
            mk("b", "opencode", None),          // -> contends with a (both live)
            mk("c", "codex", None),             // lone codex -> not contended
            mk("d", "opencode", Some("ses_x")), // has an id -> ignored
            mk("e", "opencode", None),          // id-less opencode, DEAD -> uncounted
        ];
        // Start from a clean, fresh, empty cache so name resolution is
        // deterministic (a prior test's residual cache could otherwise make
        // `resolve_name` pick a variant name shape). Resolve names the same way
        // `tmux_alive_cached` does, then mark a, b, c, d present and leave e out.
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[]);
        let name_of = |i: &Instance| crate::tmux::Session::resolve_name(&i.id, &i.title);
        let live: Vec<String> = instances[..4].iter().map(name_of).collect();
        let live_refs: Vec<&str> = live.iter().map(String::as_str).collect();
        guard.force_present(&live_refs);

        let contended = Instance::contended_capture_cwds(&instances);

        // a + b: two live id-less opencode in one cwd -> contended.
        assert!(contended.contains(&("opencode".to_string(), canon.clone())));
        // c: a single live codex -> not contended (proves the >=2 + tool key).
        assert!(!contended.contains(&("codex".to_string(), canon.clone())));

        // A live opencode session sharing its cwd with only a DEAD id-less
        // opencode peer must NOT be contended: the dead peer's agent is no
        // longer writing to the store, so it cannot cause a mis-attribution.
        // Rebuild with just one live + one dead to isolate that path.
        let live_only = mk("live", "opencode", None);
        let dead = mk("dead", "opencode", None);
        guard.force_present(&[name_of(&live_only).as_str()]);
        let contended = Instance::contended_capture_cwds(&[live_only, dead]);
        assert!(
            !contended.contains(&("opencode".to_string(), canon)),
            "a dead id-less peer must not force the live session to abstain"
        );
    }

    /// Force the tmux session cache into a fresh "server reachable, but this
    /// session is not in its list" snapshot so `Session::existence()` resolves
    /// to `Absent` regardless of whether a real tmux server happens to be up on
    /// the per-process test socket. Tests that assert detection latches `Error`
    /// must call this, otherwise their outcome depends on test scheduling
    /// (#2936). Returns the RAII guard; keep it bound for the test's duration
    /// (`let _cache = ...`) so it restores the prior cache on drop, and mark
    /// the test `#[serial_test::serial]` since the cache is process-global.
    #[must_use]
    fn force_session_absent() -> crate::tmux::SessionCacheGuard {
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&["aoe_some_other_session"]);
        guard
    }

    /// `/api/sessions` puts `format!("{:?}", inst.status)` on the wire, so
    /// `from_api_str` has to speak `CamelCase` while `as_str` and serde speak
    /// `lowercase`. Two spellings of one enum drift silently unless the pairing
    /// is asserted against the actual formatter, which is what this does: a new
    /// variant that nobody teaches `from_api_str` fails here rather than
    /// showing up as a structured row whose status stops moving.
    #[test]
    fn status_api_wire_form_round_trips() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ] {
            let wire = format!("{status:?}");
            // `wire_str` is the explicit spelling every API surface emits;
            // pin it to `Debug` so a variant rename cannot silently change
            // the public wire format on one side only. See #3187.
            assert_eq!(
                status.wire_str(),
                wire,
                "wire_str must match the Debug spelling callers already receive"
            );
            assert_eq!(
                Status::from_api_str(&wire),
                Some(status),
                "wire form {wire} must parse back"
            );
            assert_eq!(
                Status::from_api_str(status.wire_str()),
                Some(status),
                "wire_str output must parse back through from_api_str"
            );
            // The lowercase serde/`as_str` spelling is a different vocabulary
            // and must NOT be accepted here, or a caller mixing the two would
            // silently work for `error` and fail for `Error`.
            assert_eq!(
                Status::from_api_str(status.as_str()),
                None,
                "from_api_str must not accept the lowercase spelling {}",
                status.as_str()
            );
        }
        assert_eq!(Status::from_api_str(""), None);
        assert_eq!(Status::from_api_str("Hibernating"), None);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn switch_to_terminal_keep_context_carries_acp_id_into_resume_target() {
        let mut inst = Instance::new("claude", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());
        inst.import_pending = Some(true);

        inst.switch_to_terminal_keep_context();

        assert_eq!(inst.view, View::Terminal);
        assert_eq!(inst.agent_session_id.as_deref(), Some("sid-abc"));
        assert_eq!(inst.resume_intent, ResumeIntent::Use("sid-abc".to_string()));
        // Structured-view-only ids are dropped: terminal mode reads
        // agent_session_id, and a stale acp_session_id would wrongly drive a
        // session/load on a later re-enable.
        assert_eq!(inst.acp_session_id, None);
        assert_eq!(inst.import_pending, None);
    }

    #[test]
    fn set_color_accepts_palette_and_clears_with_none() {
        let mut inst = Instance::new("color-test", "/tmp");
        assert_eq!(inst.color, None);

        for c in SESSION_COLORS {
            inst.set_color(Some((*c).to_string())).unwrap();
            assert_eq!(inst.color.as_deref(), Some(*c));
        }

        inst.set_color(None).unwrap();
        assert_eq!(inst.color, None);
    }

    #[test]
    fn set_color_rejects_unknown_color_and_leaves_prior_value() {
        let mut inst = Instance::new("color-test", "/tmp");
        inst.set_color(Some("green".to_string())).unwrap();

        let err = inst
            .set_color(Some("chartreuse".to_string()))
            .expect_err("unknown color must be rejected");
        assert!(
            err.contains("chartreuse"),
            "error should name the value: {err}"
        );
        // A rejected write must not clobber the previously stored color.
        assert_eq!(inst.color.as_deref(), Some("green"));
    }

    #[test]
    fn is_valid_session_color_matches_palette() {
        assert!(is_valid_session_color("red"));
        assert!(is_valid_session_color("amber"));
        assert!(is_valid_session_color("green"));
        assert!(!is_valid_session_color("blue"));
        assert!(!is_valid_session_color(""));
        assert!(!is_valid_session_color("Red"));
    }

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
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
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
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
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
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
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
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        crate::session::config::update_config(|global| {
            global.session.agent_status_hooks = false;
        })
        .unwrap();

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
        inst.update_status_with_metadata(None, None);
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
        inst.update_status_with_metadata(None, None);
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
        inst.update_status_with_metadata(None, None);
        inst.unarchive();
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    /// Regression guard for the false-Error-latch bug: a confirmed-absent
    /// session (tmux server reachable, session missing from its list) must
    /// still latch `Status::Error` with `TMUX_SESSION_GONE_ERROR` exactly as
    /// before. Proves the `Unknown` fix did not soften the real-death case.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_absent_session_still_latches_error() {
        let mut inst = Instance::new("test-absent", "/tmp/test-absent");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        // Fresh cache, server reachable, but this instance's tmux session
        // name is not in it: a confirmed-absent session.
        guard.force_present(&["some_other_session"]);

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some(TMUX_SESSION_GONE_ERROR));
        assert!(inst.last_error_check.is_some());
    }

    /// the poller / serve / ps loops resolve the session's live tmux name
    /// once against the batch snapshot; the status probe must act on that name
    /// instead of resolving the id a second time from the (possibly stale)
    /// title. A live name the title could never derive proves which path ran:
    /// only the resolved-name path can confirm it present.
    #[test]
    #[serial_test::serial]
    fn update_status_probes_the_resolved_name_not_the_title() {
        let resolved = format!("{}live_elsewhere_00000000", crate::tmux::SESSION_PREFIX);

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[resolved.as_str()]);

        let mut inst = Instance::new("resolve-r2", "/tmp/resolve-r2");
        inst.status = Status::Running;
        inst.update_status_with_metadata_inner(None, Some(&resolved));
        assert!(
            inst.ever_confirmed_present,
            "the passed resolved name must be the one probed"
        );
        assert_ne!(inst.status, Status::Error);

        let mut untold = Instance::new("resolve-r2", "/tmp/resolve-r2");
        untold.status = Status::Running;
        untold.update_status_with_metadata_inner(None, None);
        assert_eq!(
            untold.status,
            Status::Error,
            "without the resolved name the title-derived name is absent from the cache"
        );
        assert_eq!(untold.last_error.as_deref(), Some(TMUX_SESSION_GONE_ERROR));
    }

    /// A tmux-server-unreachable probe (`SessionExistence::Unknown`) must not
    /// touch status, last_error, or last_error_check at all: a transient
    /// tmux hiccup must never look like every session died.
    #[test]
    #[serial_test::serial]
    fn test_unreachable_tmux_server_retains_running_status() {
        let mut inst = Instance::new("test-unknown", "/tmp/test-unknown");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        // Fresh cache with no data: mirrors what `refresh_session_cache`
        // writes when `list-sessions` itself fails (stale socket, refused
        // connection), not a confirmed-absent session.
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// Same `Unknown` retain-behavior, but starting from an already-set
    /// genuine `Status::Error`: an unreachable tmux server must not clear or
    /// overwrite a real prior failure either. "Retain" means untouched in
    /// both directions.
    #[test]
    #[serial_test::serial]
    fn test_unreachable_tmux_server_does_not_clear_existing_error() {
        let mut inst = Instance::new("test-unknown-error", "/tmp/test-unknown-error");
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        // None (rather than a stale Instant) so the 30s Error-recheck
        // throttle above this code path doesn't short-circuit before the
        // probe we're testing ever runs.
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
        assert_eq!(inst.last_error_check, None);
    }

    /// A session that has never been confirmed alive (`ever_confirmed_present`
    /// still `false`, e.g. `aoe add` without `--launch`) has nothing to
    /// "blip" from, so `Unknown` escalates to `Error` well before the long
    /// confirmed-present window; this is the case
    /// `web/tests/live/ensure-session-restart.spec.ts` depends on to see
    /// `Error` within its 10s wait.
    #[test]
    #[serial_test::serial]
    fn test_never_confirmed_present_unknown_escalates_after_fast_window() {
        let mut inst = Instance::new("test-never-present", "/tmp/test-never-present");
        inst.status = Status::Idle;
        inst.last_error = None;
        inst.last_error_check = None;
        assert!(!inst.ever_confirmed_present);
        inst.unknown_since = Some(
            std::time::Instant::now()
                - UNKNOWN_ERROR_WINDOW_NEVER_PRESENT
                - std::time::Duration::from_millis(1),
        );

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some(TMUX_SERVER_UNREACHABLE_ERROR)
        );
        assert!(inst.last_error_check.is_some());
    }

    /// The never-confirmed-present fast window must still absorb a fresh
    /// `Unknown` streak (elapsed just under the window), otherwise every
    /// freshly-added, not-yet-launched session would flap to `Error` on the
    /// very first couple of poll ticks before tmux even has a chance to
    /// answer.
    #[test]
    #[serial_test::serial]
    fn test_never_confirmed_present_unknown_retains_status_below_fast_window() {
        let mut inst = Instance::new("test-never-present-fresh", "/tmp/test-never-present-fresh");
        inst.status = Status::Idle;
        inst.last_error = None;
        inst.last_error_check = None;
        assert!(!inst.ever_confirmed_present);
        inst.unknown_since =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(500));

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// The real production blip case: a session confirmed alive at some
    /// point must ride out an `Unknown` streak up to the long window,
    /// covering the ~11s max blip duration observed in production with
    /// margin, before ever latching `Error`.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_present_unknown_retains_status_below_long_window() {
        let mut inst = Instance::new("test-confirmed-present", "/tmp/test-confirmed-present");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;
        inst.ever_confirmed_present = true;
        // 11s: the max blip duration observed in production. Must not latch.
        inst.unknown_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(11));

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// A session confirmed alive must still eventually latch `Error` once
    /// the tmux server has been unreachable past the long bounded window;
    /// the fix absorbs blips, it does not make a genuinely-dead server
    /// invisible forever.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_present_unknown_escalates_after_long_window() {
        let mut inst = Instance::new(
            "test-confirmed-present-dead",
            "/tmp/test-confirmed-present-dead",
        );
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;
        inst.ever_confirmed_present = true;
        inst.unknown_since = Some(
            std::time::Instant::now()
                - UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT
                - std::time::Duration::from_millis(1),
        );

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some(TMUX_SERVER_UNREACHABLE_ERROR)
        );
        assert!(inst.last_error_check.is_some());
    }

    /// `Present` must clear a stale `unknown_since` and flip
    /// `ever_confirmed_present` on, so a session that recovers from a real
    /// outage is treated as confirmed-alive (long window) on its next
    /// `Unknown` streak rather than falling back to the never-confirmed-present
    /// fast window.
    #[test]
    #[serial_test::serial]
    fn test_present_clears_unknown_since_and_marks_ever_confirmed_present() {
        let mut inst = Instance::new("present-clears-unknown", "/tmp/present-clears-unknown");
        inst.status = Status::Idle;
        inst.unknown_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        assert!(!inst.ever_confirmed_present);
        let name = tmux::Session::generate_name(&inst.id, &inst.title);

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[name.as_str()]);

        inst.update_status_with_metadata_inner(None, None);

        assert!(inst.ever_confirmed_present);
        assert_eq!(inst.unknown_since, None);
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
    fn test_merge_post_start_imports_newer_lifecycle_snapshot_as_a_unit() {
        let stale_idle = Utc::now() - chrono::Duration::minutes(5);
        let mut live = Instance::new("session", "/tmp/test");
        live.lifecycle_generation = 7;
        live.status = Status::Starting;
        live.idle_entered_at = Some(stale_idle);
        live.last_error = Some("stale pane observation".to_string());

        let mut disk = live.clone();
        disk.lifecycle_generation = 8;
        disk.status = Status::Stopped;
        disk.idle_entered_at = None;
        disk.last_error = None;

        live.merge_post_start(&disk);

        assert_eq!(live.lifecycle_generation, 8);
        assert_eq!(live.status, Status::Stopped);
        assert_eq!(live.idle_entered_at, None);
        assert_eq!(live.last_error, None);
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_status_commit_releases_the_acquired_generation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::storage::Storage::new_unwatched("lifecycle-lease").unwrap();
        let mut instance = Instance::new("session", "/tmp/test");

        let missing = instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap_err();
        assert!(missing.to_string().contains("no longer exists"));

        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let generation = instance.lifecycle_generation;
        instance
            .commit_lifecycle_status(&storage, LifecycleOperation::Launch, Status::Error)
            .unwrap();

        let reloaded = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == instance.id)
            .unwrap();
        assert_eq!(reloaded.lifecycle_generation, generation);
        assert_eq!(reloaded.lifecycle_reservation, None);
        assert_eq!(reloaded.status, Status::Error);
    }

    #[test]
    #[serial_test::serial]
    fn launch_hooks_run_without_the_instance_lifecycle_flock() {
        if !crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());

        for restart in [false, true] {
            let label = if restart { "restart" } else { "start" };
            let profile = format!("lifecycle-hook-{label}");
            let ready = temp.path().join(format!("{label}-ready"));
            let release = temp.path().join(format!("{label}-release"));
            let hook = format!(
                ": > {}; while [ ! -e {} ]; do sleep 0.01; done",
                super::shell_escape(&ready.to_string_lossy()),
                super::shell_escape(&release.to_string_lossy()),
            );
            crate::session::config::update_config(|global| {
                global.hooks.on_launch = vec![hook];
            })
            .unwrap();

            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            let title = format!("lifecycle hook {label}");
            let mut instance = Instance::new(&title, temp.path().to_str().unwrap());
            instance.source_profile = profile.clone();
            instance.command = "sleep 30".to_string();
            storage
                .update(|instances, _groups| {
                    instances.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            if restart {
                instance
                    .tmux_session()
                    .unwrap()
                    .create(temp.path().to_str().unwrap(), Some("sleep 30"), &profile)
                    .unwrap();
            }

            let (launch_tx, launch_rx) = std::sync::mpsc::channel();
            let launch = std::thread::spawn(move || {
                let result = if restart {
                    instance.restart_with_size_opts(None, false).map(|_| ())
                } else {
                    instance.start_with_size_opts(None, false).map(|_| ())
                };
                launch_tx.send((result, instance)).unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(ready.exists(), "{label} hook did not start");

            let lock_storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            let id = storage.load().unwrap()[0].id.clone();
            let release_for_lock = release.clone();
            let (lock_tx, lock_rx) = std::sync::mpsc::channel();
            let lock = std::thread::spawn(move || {
                let guard = lock_storage.acquire_instance_lifecycle_lock(&id).unwrap();
                drop(guard);
                std::fs::write(release_for_lock, b"release").unwrap();
                lock_tx.send(()).unwrap();
            });
            let acquired = lock_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            if !acquired {
                std::fs::write(&release, b"release").unwrap();
            }

            let (result, instance) = launch_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            launch.join().unwrap();
            lock.join().unwrap();
            let _ = instance.tmux_session().unwrap().kill();
            assert!(
                acquired,
                "{label} hook ran while the lifecycle flock was held"
            );
            result.unwrap();
        }
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_reservation_rejects_busy_state_without_blocking_first_launch() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-busy-reservation";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let now = Utc::now();
        let stale = now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        let mut cases = [
            ("unleased", Status::Starting, None, 0, true),
            (
                "leased_peer",
                Status::Starting,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "superseded",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                }),
                2,
                true,
            ),
            (
                "expired",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: stale,
                }),
                1,
                true,
            ),
            (
                "purge",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Purge,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "restore",
                Status::Stopped,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Restore,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "trash",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Trash,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "capture",
                Status::Running,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Capture,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            ("creating", Status::Creating, None, 0, true),
            ("idle", Status::Idle, None, 0, true),
            ("stopped", Status::Stopped, None, 0, true),
        ]
        .map(
            |(title, status, lifecycle_reservation, lifecycle_generation, allowed)| {
                let mut instance = Instance::new(title, "/tmp/test");
                instance.source_profile = profile.to_string();
                instance.status = status;
                instance.lifecycle_generation = lifecycle_generation;
                instance.lifecycle_reservation = lifecycle_reservation;
                (instance, allowed)
            },
        );
        storage
            .update(|instances, _groups| {
                instances.extend(cases.iter().map(|(instance, _)| instance.clone()));
                Ok(())
            })
            .unwrap();

        for (instance, allowed) in &mut cases {
            let result = instance.acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            );
            assert_eq!(result.is_ok(), *allowed, "{}", instance.title);
        }

        let leased = &cases[0].0;
        assert!(leased.reservation_is_current(&storage).unwrap());
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == leased.id)
                    .unwrap();
                peer.lifecycle_generation += 1;
                peer.status = Status::Stopped;
                Ok(())
            })
            .unwrap();
        assert!(!leased.reservation_is_current(&storage).unwrap());

        let mut busy = Instance::new("busy-leased", "/tmp/test");
        busy.source_profile = profile.to_string();
        busy.status = Status::Starting;
        busy.lifecycle_generation = 1;
        busy.lifecycle_reservation = Some(LifecycleReservation {
            op: LifecycleOperation::Launch,
            generation: 1,
            at: Utc::now(),
        });
        storage
            .update(|instances, _groups| {
                instances.push(busy.clone());
                Ok(())
            })
            .unwrap();

        let began = std::time::Instant::now();
        assert!(busy.stop().unwrap_err().to_string().contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));

        let mut recursive_start = busy.clone();
        let began = std::time::Instant::now();
        assert!(recursive_start
            .start_with_size_opts(None, true)
            .unwrap_err()
            .to_string()
            .contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    #[serial_test::serial]
    fn failed_launch_releases_reservation_after_status_drift() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-fail-drift";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("drift", "/tmp/test");
        inst.source_profile = profile.to_string();
        inst.status = Status::Idle;
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();

        inst.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )
        .unwrap();
        let reserved_gen = inst.lifecycle_generation;

        // A same-generation passive status patch changes presentation state
        // without changing ownership while prepare_launch runs unlocked.
        storage
            .update(|instances, _groups| {
                let stored = instances.iter_mut().find(|i| i.id == inst.id).unwrap();
                assert_eq!(stored.lifecycle_generation, reserved_gen);
                assert!(stored.lifecycle_reservation.is_some());
                stored.status = Status::Stopped;
                Ok(())
            })
            .unwrap();

        // The launch guard still recognizes the exact-generation reservation.
        // A later launch failure must release it rather than stranding the
        // marker until its TTL.
        inst.ensure_reservation_current_or_fail(&storage).unwrap();
        let error = anyhow::anyhow!("launch failed after status drift");
        inst.fail_reserved_launch(&storage, &error, false);

        let leftover = storage
            .update(|instances, _groups| {
                Ok(instances
                    .iter()
                    .find(|instance| instance.id == inst.id)
                    .and_then(|instance| instance.lifecycle_reservation.clone()))
            })
            .unwrap();
        assert!(
            leftover.is_none(),
            "a failed launch must clear its reservation even after a same-generation status drift"
        );
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_launch_commit_keeps_reserved_generation_and_rejects_stale_or_overflowed_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage =
            crate::session::storage::Storage::new_unwatched("lifecycle-launch-commit").unwrap();
        let mut committed = Instance::new("committed", "/tmp/test");
        let mut stale = Instance::new("stale", "/tmp/test");
        let mut overflow = Instance::new("overflow", "/tmp/test");
        overflow.lifecycle_generation = u64::MAX;
        storage
            .update(|instances, _groups| {
                instances.extend([committed.clone(), stale.clone(), overflow.clone()]);
                Ok(())
            })
            .unwrap();

        committed
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let reserved_generation = committed.lifecycle_generation;
        committed.status = Status::Running;
        committed.commit_lifecycle_launch(&storage, false).unwrap();
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == committed.id)
            .unwrap();
        assert_eq!(committed.lifecycle_generation, reserved_generation);
        assert_eq!(disk.lifecycle_generation, committed.lifecycle_generation);
        assert_eq!(disk.status, Status::Running);

        stale
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let stale_token = stale.lifecycle_generation;
        stale.status = Status::Running;
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == stale.id)
                    .unwrap();
                peer.lifecycle_generation = stale_token + 1;
                peer.status = Status::Stopped;
                Ok(())
            })
            .unwrap();
        let error = stale.commit_lifecycle_launch(&storage, false).unwrap_err();
        assert!(error.to_string().contains("lost its lifecycle reservation"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == stale.id)
            .unwrap();
        assert_eq!(stale.lifecycle_generation, stale_token);
        assert_eq!(disk.lifecycle_generation, stale_token + 1);
        assert_eq!(disk.status, Status::Stopped);

        assert!(overflow
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap_err()
            .to_string()
            .contains("overflow"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == overflow.id)
            .unwrap();
        assert_eq!(overflow.lifecycle_generation, u64::MAX);
        assert_eq!(overflow.status, Status::Idle);
        assert_eq!(disk.lifecycle_generation, u64::MAX);
        assert_eq!(disk.status, Status::Idle);
    }

    #[test]
    fn runtime_reload_keeps_strictly_newer_disk_lifecycle_snapshot() {
        let mut previous = Instance::new("session", "/tmp/test");
        previous.lifecycle_generation = 3;
        previous.status = Status::Starting;
        previous.idle_entered_at = Some(Utc::now());
        previous.last_error = Some("old observation".to_string());

        let mut reloaded = previous.clone();
        reloaded.lifecycle_generation = 4;
        reloaded.status = Status::Stopped;
        reloaded.idle_entered_at = None;
        reloaded.last_error = None;
        reloaded.merge_runtime_from_reload(&previous);

        // Generation-governed fields: the strictly-newer disk snapshot wins.
        assert_eq!(reloaded.lifecycle_generation, 4);
        assert_eq!(reloaded.status, Status::Stopped);
        assert_eq!(reloaded.idle_entered_at, None);
        // last_error is runtime-only: the in-memory poller value survives even a
        // newer generation, since no lifecycle writer persists last_error.
        assert_eq!(reloaded.last_error.as_deref(), Some("old observation"));
    }

    #[test]
    fn runtime_reload_preserves_poller_gone_error_across_generation_bump() {
        // A stop/unarchive bumps the disk generation with status: None, so the
        // reloaded row carries no last_error. The poller's freshly derived
        // TMUX_SESSION_GONE_ERROR (in memory) must survive, or the row freezes
        // at Error+None and the stopped preview never renders (#3230).
        let mut previous = Instance::new("session", "/tmp/test");
        previous.lifecycle_generation = 7;
        previous.status = Status::Error;
        previous.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());

        let mut reloaded = previous.clone();
        reloaded.lifecycle_generation = 8;
        reloaded.status = Status::Error;
        reloaded.last_error = None;
        reloaded.merge_runtime_from_reload(&previous);

        assert_eq!(
            reloaded.last_error.as_deref(),
            Some(TMUX_SESSION_GONE_ERROR)
        );
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
    fn test_is_shown_dormant_precedence() {
        // Idle + dormant marker: the idle-reaper's output, presents dormant.
        let mut idle_reaped = Instance::new("test", "/tmp/test");
        idle_reaped.status = Status::Idle;
        idle_reaped.mark_idle_dormant();
        assert!(idle_reaped.is_shown_dormant());

        // Stopped + dormant marker: a deliberate Stop (which also marks
        // dormant). Stopped must win so the row keeps the neutral "Stopped"
        // dot, not the dormant one. See #2250.
        let mut deliberate_stop = Instance::new("test", "/tmp/test");
        deliberate_stop.status = Status::Stopped;
        deliberate_stop.mark_idle_dormant();
        assert!(!deliberate_stop.is_shown_dormant());

        // Idle, no marker: a live idle session, unaffected.
        let mut live_idle = Instance::new("test", "/tmp/test");
        live_idle.status = Status::Idle;
        assert!(!live_idle.is_shown_dormant());

        // Running, no marker: live, unaffected.
        let mut running = Instance::new("test", "/tmp/test");
        running.status = Status::Running;
        assert!(!running.is_shown_dormant());
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

        stored.lifecycle_generation = 2;
        stored.status = Status::Stopped;
        working.lifecycle_generation = 1;
        working.status = Status::Starting;
        stored.merge_post_start(&working);
        assert_eq!(stored.status, Status::Stopped);
        stored.merge_from_tui(&working);
        assert_eq!(
            stored.status,
            Status::Stopped,
            "a stale async/TUI result must not overwrite a newer lifecycle commit"
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

        let mut before = Instance::new("omp-session", "/tmp/test");
        before.agent_session_id = Some("old-sid".to_string());
        before.omp_capture_generation = Some("generation-a".to_string());
        let mut restarted = before.clone();
        restarted.omp_capture_generation = Some("generation-b".to_string());
        let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
        assert!(poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,));
        let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
        restarted.session_id_poller = Some(restarted_poller.clone());
        let mut live = before.clone();
        live.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(live.omp_capture_generation.as_deref(), Some("generation-b"));
        assert!(live.session_id_poller.is_some());

        let mut generation_converged = before.clone();
        generation_converged.agent_session_id = Some("peer-sid".to_string());
        generation_converged.omp_capture_generation = Some("generation-b".to_string());
        generation_converged.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(
            generation_converged.agent_session_id.as_deref(),
            Some("peer-sid")
        );
        assert!(generation_converged.session_id_poller.is_some());

        let mut peer_relaunched = before.clone();
        peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
        peer_relaunched.merge_post_restart_with_baseline(&before, &restarted);
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

    #[test]
    fn lifecycle_reservation_roundtrips_and_legacy_rows_default_to_none() {
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(!fresh_json.contains("lifecycle_reservation"));
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert_eq!(parsed.lifecycle_reservation, None);

        let mut instance = Instance::new("s", "/tmp/x");
        let now = Utc::now();
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("free row grants the lease");
        let json = serde_json::to_string(&instance).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(
            back.lifecycle_reservation,
            Some(LifecycleReservation {
                op: LifecycleOperation::Purge,
                generation,
                at: now,
            })
        );
    }

    #[test]
    fn lifecycle_reservation_excludes_peers_and_uses_generation_as_identity() {
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("first operation acquires");

        for contender in [
            LifecycleOperation::Launch,
            LifecycleOperation::Stop,
            LifecycleOperation::Purge,
            LifecycleOperation::Restore,
            LifecycleOperation::Trash,
        ] {
            assert_eq!(
                instance.try_acquire_lifecycle_reservation(
                    contender,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    now + chrono::Duration::seconds(1),
                ),
                Err(LifecycleReservationError::Busy(LifecycleOperation::Purge)),
                "{contender:?} must not replace a live peer reservation",
            );
        }
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation + 1,));
        assert!(instance.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        assert!(
            instance.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation)
        );
    }

    #[test]
    fn expired_lifecycle_reservation_is_recoverable_without_reusing_generation() {
        let ttl = Instance::LIFECYCLE_RESERVATION_TTL;
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let old_generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                ttl,
                now - ttl - chrono::Duration::seconds(1),
            )
            .expect("first operation acquires");
        let new_generation = instance
            .try_acquire_lifecycle_reservation(LifecycleOperation::Restore, ttl, now)
            .expect("expired lease can be replaced");

        assert!(new_generation > old_generation);
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, old_generation,));
        assert!(
            instance.lifecycle_reservation_is_owned(LifecycleOperation::Restore, new_generation,)
        );
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
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(now),
            last_accessed_at: Some(now),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

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

        disk.lifecycle_generation = 2;
        disk.status = Status::Stopped;
        let mut stale_lifecycle = patch.clone();
        stale_lifecycle.lifecycle_generation = 1;
        stale_lifecycle.status = Status::Running;
        disk.merge_passive_status_patch(&disk.id.clone(), &stale_lifecycle);
        assert_eq!(
            disk.status,
            Status::Stopped,
            "a poll from an older pane generation must not repaint Stop"
        );
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
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(Utc::now()),
            last_accessed_at: None,
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

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
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(peer_touch - chrono::Duration::minutes(5)),
            last_accessed_at: Some(peer_touch - chrono::Duration::minutes(5)),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &stale_patch);

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
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        // Guard is `>=`: equal timestamps are not a real advance, so the
        // patch's last_accessed_at is dropped. The observable value stays
        // equal to `ts` either way (disk == incoming), so the assertion
        // does not change; the point of the guard is skipping the write.
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    /// Count the guard's drop-event log lines. `logs_assert` hands us lines
    /// already scoped to the calling test's span, and the message is unique to
    /// the drop branch, so matching the substring cannot be inflated by other
    /// `session.store` events.
    fn drop_log_count(lines: &[&str]) -> usize {
        lines
            .iter()
            .filter(|l| l.contains("dropped passive status patch's last_accessed_at as a no-op"))
            .count()
    }

    /// Closes I4 from #2756: the equal-timestamp guard's observability gap.
    /// Under `disk == incoming` the drop branch and the write branch leave the
    /// same observable `last_accessed_at`, so `boundary_equal_is_a_noop` above
    /// cannot prove the drop branch ran. Here `disk == incoming` must fire the
    /// `session.store` drop log exactly once.
    #[traced_test]
    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_equal_logs_drop_event() {
        // Tracing caches per-callsite `Interest` globally on first hit, so a
        // parallel test that reaches the drop callsite first without a
        // capturing subscriber pins it to `Interest::never()` and this
        // capture silently sees zero lines. Re-evaluate the (already
        // registered) callsite against `traced_test`'s subscriber first. Same
        // race `run_with_capture` documents in session::deletion.
        tracing::callsite::rebuild_interest_cache();

        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        disk.last_accessed_at = Some(ts);

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        logs_assert(|lines: &[&str]| match drop_log_count(lines) {
            1 => Ok(()),
            n => Err(format!("expected 1 drop event, got {n}")),
        });
    }

    /// Closes I4 from #2756 (write side): a strictly newer incoming timestamp
    /// skips the guard, so the drop log must fire zero times and the value is
    /// written. Pairing the zero-count write case with the exactly-once drop
    /// case above proves the log is a faithful drop-vs-write signal, not a line
    /// that fires regardless. Uses an explicit minute offset (as
    /// `boundary_newer_applies` does) to avoid a same-instant flake.
    #[traced_test]
    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_newer_no_drop_event() {
        // Same callsite-interest race as its paired test above. This one
        // asserts zero drops, so a lost race would make it pass for the
        // wrong reason; rebuild so the pair stays a faithful drop-vs-write
        // signal.
        tracing::callsite::rebuild_interest_cache();

        let mut disk = Instance::new("session", "/tmp/test");
        let older = Utc::now() - chrono::Duration::minutes(1);
        let newer = Utc::now();
        disk.last_accessed_at = Some(older);

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(newer),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        logs_assert(|lines: &[&str]| match drop_log_count(lines) {
            0 => Ok(()),
            n => Err(format!("expected 0 drop events, got {n}")),
        });
        assert_eq!(disk.last_accessed_at, Some(newer));
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_newer_applies() {
        let mut disk = Instance::new("session", "/tmp/test");
        let older = Utc::now() - chrono::Duration::minutes(1);
        disk.last_accessed_at = Some(older);

        let newer = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(newer),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

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
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_identical_is_idempotent() {
        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(ts),
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(ts));
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_increasing_newer_wins() {
        let mut disk = Instance::new("session", "/tmp/test");
        let t0 = Utc::now() - chrono::Duration::minutes(1);
        let t1 = Utc::now();

        disk.merge_passive_status_patch(
            &disk.id.clone(),
            &PassiveStatusPatch {
                lifecycle_generation: 0,
                status: Status::Running,
                idle_entered_at: None,
                last_accessed_at: Some(t0),
            },
        );
        disk.merge_passive_status_patch(
            &disk.id.clone(),
            &PassiveStatusPatch {
                lifecycle_generation: 0,
                status: Status::Idle,
                idle_entered_at: Some(t1),
                last_accessed_at: Some(t1),
            },
        );

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
    #[serial_test::serial]
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

        // Force detection to resolve to `Absent` -> Error deterministically:
        // a fresh cache snapshot that lists some other session but not this
        // instance's. Without this the outcome depends on whether an earlier
        // tmux-spawning test left a server reachable on the per-process
        // socket, making the test schedule-dependent and flaky (#2936).
        let _cache = force_session_absent();

        inst.update_status_with_metadata(None, None);

        // Detection confirms the session Absent, resolving to Error, which
        // differs from the stale disk `Starting`. That mismatch must NOT be
        // treated as a genuine transition.
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
    #[serial_test::serial]
    fn test_update_status_with_metadata_restamps_on_genuine_transition() {
        // Once a live baseline is established, a real status change still
        // restamps normally (no regression from the #2690 fix).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::hours(2));
        inst.last_accessed_at = Some(Utc::now() - chrono::Duration::hours(2));

        // Force detection to resolve to `Absent` -> Error deterministically
        // (see #2936; without this the outcome is schedule-dependent).
        let _cache = force_session_absent();

        let before = Utc::now();
        inst.update_status_with_metadata(None, None);
        let after = Utc::now();

        // Detection confirms the session Absent, resolving to Error: a
        // genuine transition away from the established Idle baseline.
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.idle_entered_at, None);
        let last_accessed = inst.last_accessed_at.expect("must be restamped");
        assert!(last_accessed >= before && last_accessed <= after);
        assert_eq!(inst.live_status_baseline, Some(Status::Error));
    }

    #[test]
    #[serial_test::serial]
    fn test_update_status_with_metadata_twice_same_status_never_restamps() {
        // Two consecutive calls that both detect the same status (session
        // confirmed Absent, so detection is deterministically Error) must
        // neither restamp: not the first (baseline already matches), and
        // not the second either.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Error);
        inst.status = Status::Error;
        let sentinel_idle = Some(Utc::now() - chrono::Duration::hours(3));
        let sentinel_accessed = Some(Utc::now() - chrono::Duration::hours(3));
        inst.idle_entered_at = sentinel_idle;
        inst.last_accessed_at = sentinel_accessed;

        // Force detection to resolve to `Absent` -> Error deterministically
        // (see #2936; without this the outcome is schedule-dependent).
        let _cache = force_session_absent();

        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, sentinel_idle,
            "first call must not restamp"
        );
        assert_eq!(
            inst.last_accessed_at, sentinel_accessed,
            "first call must not restamp"
        );

        inst.update_status_with_metadata(None, None);
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
        inst.update_status_with_metadata(None, None);
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
        inst.update_status_with_metadata(None, None);
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

        inst.update_status_with_metadata(None, None);

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
    fn test_merge_from_tui_syncs_launch_config_swap() {
        // The restart dialog mutates tool/command/extra_args in the TUI's
        // in-memory row. save() -> merge_from_tui must carry those onto disk,
        // otherwise reconcile_from_disk reverts the swap on the next launch and
        // the session respawns with its original tool.
        let mut stored = Instance::new("session", "/tmp/test");
        stored.tool = "claude".to_string();
        stored.command = String::new();
        stored.extra_args = String::new();

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.tool = "codex".to_string();
        src.command = "codex-wrapper".to_string();
        src.extra_args = "--foo".to_string();

        stored.merge_from_tui(&src);

        assert_eq!(stored.tool, "codex");
        assert_eq!(stored.command, "codex-wrapper");
        assert_eq!(stored.extra_args, "--foo");
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

    /// Real-tmux integration for #3157: a session whose stored title moved
    /// without its tmux session being renamed (smart rename, or a manual
    /// rename whose tmux rename failed) must still be resolvable, so teardown
    /// stops the running agent instead of a name that never existed, and a
    /// later start adopts the live session instead of spawning a second one.
    // Serialized for the same reason as its neighbours: it creates and kills a
    // real tmux session on the shared test server.
    #[test]
    #[serial_test::serial]
    fn retitled_session_is_still_resolved_and_torn_down() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "retitled-session-teardown";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();

        let mut inst = Instance::new("Vikings", "/tmp/test");
        inst.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let created_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &created_name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &created_name,
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

        // The rename that never reached tmux.
        inst.title = "Refactor billing module".to_string();
        let derived = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        assert_ne!(derived, created_name, "the derived name must have moved");

        let session = inst.tmux_session().expect("tmux_session");
        assert_eq!(
            session.name(),
            created_name,
            "lifecycle ops must resolve onto the live session, not the new derived name"
        );
        assert!(
            session.exists(),
            "the live session is reachable under the new title, so `create` adopts it \
             rather than spawning a second agent"
        );

        inst.kill().expect("kill");
        crate::tmux::refresh_session_cache();
        assert!(
            !crate::tmux::session_exists(&created_name),
            "teardown must stop the agent that is actually running"
        );
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
    fn test_has_recent_activity_active_statuses_are_true() {
        let window = std::time::Duration::from_secs(15 * 60);
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            assert!(
                inst.has_recent_activity(window),
                "{status:?} should keep the machine awake"
            );
        }
    }

    #[test]
    fn test_has_recent_activity_inactive_statuses_are_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        for status in [
            Status::Stopped,
            Status::Error,
            Status::Unknown,
            Status::Deleting,
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            assert!(
                !inst.has_recent_activity(window),
                "{status:?} must not hold the sleep-inhibit assertion"
            );
        }
    }

    #[test]
    fn test_has_recent_activity_idle_within_window_is_true() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(60));
        assert!(inst.has_recent_activity(window));
    }

    #[test]
    fn test_has_recent_activity_idle_past_window_is_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(30));
        assert!(!inst.has_recent_activity(window));
    }

    #[test]
    fn test_has_recent_activity_idle_without_timestamp_is_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = None;
        assert!(!inst.has_recent_activity(window));
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
        let cmd = format_env_var_prefix("OPENCODE_PERMISSION", r#"{"*":"allow"}"#, "opencode");
        let wrapped = wrap_command_ignore_suspend(&cmd, "/tmp/proj");
        assert!(
            wrapped.contains(r#"OPENCODE_PERMISSION='{"*":"allow"}' opencode"#),
            "wrapped command should preserve the env assignment: {wrapped}",
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_uses_stdin_script() {
        let original = std::env::var("SHELL").ok();
        for shell in &["/bin/bash", "/bin/zsh", "/usr/bin/fish", "/usr/bin/nu"] {
            std::env::set_var("SHELL", shell);
            let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
            assert!(
                wrapped.contains("/dev/fd/3 3<<'AOE_LAUNCH_BODY'"),
                "{shell}: {wrapped}"
            );
            assert!(wrapped.contains("\nstty susp undef\nexec env claude\n"));
            assert!(!wrapped.contains(" -c "));
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
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        assert!(
            wrapped.starts_with("'/bin/zsh' -l /dev/fd/3 "),
            "POSIX shell should use a login descriptor script: {wrapped}",
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
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        // The bash fallback must not load bash login files because the user's
        // PATH setup belongs to fish.
        assert!(
            wrapped.starts_with("'bash' /dev/fd/3 "),
            "fish should use a non-login bash descriptor script: {wrapped}",
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
        let wrapped = wrap_command_ignore_suspend("claude", "/tmp/proj");
        assert!(
            wrapped.starts_with("'bash' /dev/fd/3 "),
            "nu should use a non-login bash descriptor script: {wrapped}",
        );
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    /// #3265: a login shell's own profile/rc files can `cd` elsewhere
    /// (a stray line in `~/.bashrc`, or a legitimate nvm/pyenv/direnv hook)
    /// after tmux's `-c` has already set the pane's cwd, silently landing
    /// the agent in the wrong directory. The wrapper must re-assert
    /// `working_dir` inside the login shell's own script, after profile
    /// sourcing, so it wins regardless of what those files did.
    #[test]
    #[serial_test::serial(shell_env)]
    fn test_wrap_command_reasserts_working_dir_after_login_shell() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/bin/bash");
        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().join("some project's dir");
        std::fs::create_dir(&working_dir).unwrap();
        let wrapped = wrap_command_ignore_suspend("pwd", working_dir.to_str().unwrap());
        // The cd is the first statement inside the login shell's stdin script,
        // after profile sourcing, before disabling suspend and exec'ing.
        assert!(
            wrapped.contains("3<<'AOE_LAUNCH_BODY'\ncd "),
            "the cd must open the login shell's stdin script: {wrapped}",
        );
        assert!(
            wrapped.contains("|| exit 1\nstty susp undef"),
            "the cd must exit-on-failure before disabling suspend: {wrapped}",
        );
        let output = std::process::Command::new("bash")
            .args(["-c", &wrapped])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "wrapped command failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            working_dir.to_string_lossy(),
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
                branch_preexisting: false,
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

    /// An engine swap parks the outgoing agent's conversation ids under its own
    /// name and picks the incoming agent's back up, so claude -> pi -> claude
    /// lands in the original Claude conversation instead of a third one. The
    /// per-agent selectors go; the approval posture stays (clearing it resolves
    /// the adapter's bypass mode on a `yolo_mode` row).
    ///
    /// Replaces a test that hand-assigned `agent_session_id = None` and then
    /// asserted it was None, which could not fail.
    #[test]
    fn swap_tool_parks_and_restores_per_tool_session_ids() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("claude-session-123".to_string());
        inst.acp_session_id = Some("acp-claude-1".to_string());
        inst.resume_probe_failed_sid = Some("claude-session-123".to_string());
        inst.acp_effort = Some("high".to_string());
        inst.agent_model = Some("claude-opus-4-7".to_string());
        inst.agent_name = Some("claude-code".to_string());
        inst.acp_mode_id = Some("plan".to_string());

        inst.swap_tool("pi");
        assert_eq!(inst.tool, "pi");
        assert_eq!(
            inst.agent_session_id, None,
            "a Claude sid would make pi launch with --resume <foreign-sid>"
        );
        assert_eq!(inst.acp_session_id, None);
        assert_eq!(inst.acp_effort, None);
        assert_eq!(inst.agent_model, None);
        assert_eq!(inst.agent_name, None);
        assert_eq!(inst.resume_probe_failed_sid, None);
        assert_eq!(inst.acp_mode_id.as_deref(), Some("plan"));

        // pi runs and captures a sid of its own, then the user swaps back.
        inst.agent_session_id = Some("pi-session-9".to_string());
        inst.swap_tool("claude");
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some("claude-session-123"),
            "swapping back must resume the parked Claude conversation"
        );
        assert_eq!(inst.acp_session_id.as_deref(), Some("acp-claude-1"));
        assert_eq!(
            inst.prior_tool_session_ids["pi"]
                .agent_session_id
                .as_deref(),
            Some("pi-session-9"),
            "pi's conversation is the parked one now"
        );
        assert!(
            !inst.prior_tool_session_ids.contains_key("claude"),
            "a restored entry is consumed, so a later swap cannot resurrect it"
        );

        // Same-tool call is a no-op: the caller applies the swap to the disk row
        // and the in-memory row independently, and the second must not re-park.
        inst.swap_tool("claude");
        assert_eq!(inst.agent_session_id.as_deref(), Some("claude-session-123"));
        assert!(!inst.prior_tool_session_ids.contains_key("claude"));
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
    fn opencode_fresh_arm_uses_preassign_seam() {
        // opencode's fresh launch adopts the id the preassign seam returns and
        // stores it, exactly like Claude's pre-minted UUID (fresh, not resumed).
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) =
            inst.acquire_session_id_with(&|_| Some("ses_preassigned".to_string()));
        assert_eq!(sid, Some("ses_preassigned".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_preassigned".to_string()));
    }

    #[test]
    fn opencode_fresh_arm_falls_back_when_preassign_returns_none() {
        // A disabled setting or a failed preassign yields None, leaving the id
        // unpinned so the background SQLite poller captures it post-launch.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);
        assert_eq!(sid, None);
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    fn non_opencode_fresh_arm_never_calls_preassign_seam() {
        // The seam is opencode-only: Claude mints its own UUID and every other
        // agent starts unpinned, so the seam must not run for them.
        let mut claude = Instance::new("Test", "/tmp/test");
        claude.tool = "claude".to_string();
        let (claude_sid, _) =
            claude.acquire_session_id_with(&|_| panic!("preassign seam ran for claude"));
        assert!(claude_sid.is_some());

        let mut codex = Instance::new("Test", "/tmp/test");
        codex.tool = "codex".to_string();
        let (codex_sid, _) =
            codex.acquire_session_id_with(&|_| panic!("preassign seam ran for codex"));
        assert_eq!(codex_sid, None);
    }

    #[test]
    fn opencode_cleared_intent_also_uses_preassign_seam() {
        // A forced-fresh restart (ResumeIntent::Cleared) is still a new launch,
        // so it preassigns too rather than starting unpinned.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.resume_intent = ResumeIntent::Cleared;
        let (sid, is_existing) = inst.acquire_session_id_with(&|_| Some("ses_cleared".to_string()));
        assert_eq!(sid, Some("ses_cleared".to_string()));
        assert!(!is_existing);
        assert_eq!(inst.agent_session_id, Some("ses_cleared".to_string()));
    }

    #[test]
    fn opencode_preassign_skips_when_launch_not_mirrorable() {
        // Plain ambient opencode (no command override, no profile host env):
        // the ephemeral serve provably matches the launch, so preassign is
        // allowed to run.
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.tool = "opencode".to_string();
        assert!(inst.opencode_launch_mirrorable_by_ambient_serve());

        // A command override points the launch at a different binary/store,
        // which the ambient `opencode serve` cannot mirror, so preassign is
        // skipped (falls back to the poller) rather than risking a launch that
        // fails "Session not found".
        inst.command = "opencode-wrapper".to_string();
        assert!(!inst.opencode_launch_mirrorable_by_ambient_serve());
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
        inst.command = "unknown_agent".to_string();
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
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kimi")),
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
    fn omp_capture_accepts_benign_args_and_rejects_opaque_launches() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "omp".to_string();
        inst.extra_args =
            "--model sonnet --profile first --profile=work --session-dir '/tmp/omp sessions'"
                .to_string();

        let options = inst
            .omp_capture_options()
            .expect("benign argv must capture");
        assert_eq!(options.profile.as_deref(), Some("work"));
        assert_eq!(
            options.session_dir.as_deref(),
            Some(std::path::Path::new("/tmp/omp sessions"))
        );
        inst.extra_args = "--model ${model:---profile=work}".to_string();
        assert!(
            inst.omp_capture_options().is_some(),
            "model values are shell-quoted before both capture parsing and launch"
        );
        for arg in ["--continue", "-c", "--continue=false"] {
            inst.extra_args = arg.to_string();
            assert!(
                inst.omp_capture_options().is_some(),
                "{arg} remains a transparent OMP launch argument"
            );
        }
        inst.extra_args = "--model sonnet[1m]".to_string();
        assert!(
            inst.omp_capture_options().is_some(),
            "the launch path quotes model context suffixes before shell expansion"
        );

        inst.extra_args = "--no-session".to_string();
        assert!(inst.omp_capture_options().is_none());
        inst.extra_args = "'unterminated".to_string();
        assert!(inst.omp_capture_options().is_none());
        inst.extra_args.clear();
        inst.command = "omp".to_string();
        assert!(inst.omp_capture_options().is_some());
        inst.command = "omp-wrapper".to_string();
        assert!(inst.omp_capture_options().is_none());
    }

    #[test]
    fn omp_launch_rejects_api_keys_in_extra_args() {
        let mut instance = Instance::new("test", "/tmp/test");
        instance.tool = "omp".to_string();
        for extra_args in [
            "--api-key secret",
            "--api-key=secret",
            "--api-key$EMPTY secret",
        ] {
            instance.extra_args = extra_args.to_string();
            let error = instance
                .build_launch_command()
                .err()
                .expect("inline OMP credentials must abort before launch");
            if extra_args.contains('$') {
                assert!(error.to_string().contains("opaque shell syntax"), "{error}");
            } else {
                assert!(
                    error.to_string().contains("through the environment"),
                    "{extra_args}: {error}"
                );
            }
        }
    }

    fn omp_test_plan() -> OmpCapturePlan {
        OmpCapturePlan {
            layout: crate::session::capture::OmpStoreLayout {
                sessions: PathBuf::from("/tmp/omp/sessions"),
                managed_sessions: PathBuf::from("/tmp/omp/managed/sessions"),
                terminal_sessions: PathBuf::from("/tmp/omp/terminal-sessions"),
                kind: OmpStoreKind::Managed,
            },
            routing_fingerprint: "a".repeat(64),
            launch_id: "launch-unit-123".to_string(),
            launch_marker: "/tmp/aoe-omp.marker".to_string(),
            container_runtime: None,
        }
    }

    #[test]
    fn omp_routing_fingerprint_check_never_embeds_values() {
        let routing_values = ["/resolved omp/home", "default", "$resolved-secret-route"];
        let plan = omp_test_plan();

        let command = omp_routing_fingerprint_check(&plan);
        for value in routing_values {
            assert!(
                !command.contains(value),
                "resolved routing value leaked into command: {value}"
            );
        }
        assert!(command.contains("${$k+x}"));
        assert!(command.contains("${$k-}"));
        assert!(command.contains("sha256sum"));
        assert!(command.contains("shasum -a 256"));
        assert!(command.contains(&plan.routing_fingerprint));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_routing_fingerprint_accepts_matching_live_env_and_rejects_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (_, fingerprint) = resolve_omp_store_layout_with_environment(
            &[format!("HOME={}", home.display())],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        let mut plan = omp_test_plan();
        plan.routing_fingerprint = fingerprint;
        let check = omp_routing_fingerprint_check(&plan);
        let script = format!("launch_raw() {{ printf raw; exit 0; }}; {check}printf captured");
        let run = |live_home: &Path| {
            let mut command = std::process::Command::new("sh");
            command
                .args(["-c", &script])
                .env_clear()
                .env("PATH", "/usr/bin:/bin");
            // Pin the exact routing environment a host launch installs into the
            // pane for this HOME, so the check reproduces the fingerprint's env
            // instead of assuming the ambient OMP_STORE_ENV_KEYS are empty. They
            // are not on every runner, and host_launcher_environment folds them
            // into the fingerprint, so forcing empties here would diverge from
            // the digest on any host that exports one of those keys.
            for mutation in omp_host_routing_environment(&[format!("HOME={}", live_home.display())])
            {
                match mutation {
                    tmux::PaneEnvMutation::Set { key, value } => {
                        command.env(key, value);
                    }
                    tmux::PaneEnvMutation::Unset { key } => {
                        command.env_remove(key);
                    }
                }
            }
            command.output().unwrap()
        };

        assert_eq!(run(&home).stdout, b"captured");
        assert_eq!(run(&tmp.path().join("drifted")).stdout, b"raw");
    }

    #[test]
    fn omp_launch_wrapper_hashes_live_routing_and_marker_is_noclobber() {
        let routing_values = ["/sandbox home", "default", "/secret/$sandbox-route's"];
        let plan = omp_test_plan();

        let command = wrap_omp_launch("omp --profile work", &plan);
        for value in routing_values {
            assert!(
                !command.contains(value),
                "resolved routing value leaked into wrapper: {value}"
            );
        }
        assert!(command.contains("route_payload"));
        assert!(command.contains("route_fingerprint"));
        assert!(command.contains("tty_path=$(tty) || launch_raw"));
        assert!(command.contains("terminal_id=${tty_path#/dev/}"));
        assert!(command.contains("tr"));
        assert!(command.contains("launch-unit-123"));
        assert!(command.contains("/tmp/aoe-omp.marker"));
        assert!(command.contains("pending="));
        assert!(command.contains("pending=\"./$crumb_path\""));
        assert!(command.contains(".aoe-pending-launch-unit-123"));
        assert!(command.contains("aoe-pending_launch-unit-123.jsonl"));
        assert!(command.contains("mkdir \"$breadcrumb_tmp_dir\""));
        assert!(command.contains("ln -n \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw"));

        assert!(command.contains("mkdir \"$marker_tmp_dir\""));
        assert!(command.contains("(umask 077; set -C; printf"));
        assert!(command.contains("> \"$marker_tmp\") || launch_raw"));
        assert!(!command.contains(">| \"$marker_tmp\""));
        assert!(!command.contains("/dev/pts/*"));
        assert!(command.find("printf").unwrap() < command.rfind("exec sh -c").unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn omp_capture_gate_executes_nested_stdin_scripts() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tmux = bin.join("tmux");
        let expected = format!(
            "{}=launch-unit-123",
            crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY
        );
        std::fs::write(&tmux, format!("#!/bin/sh\nprintf '%s\\n' {expected:?}\n")).unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();

        let output = temp.path().join("result");
        let raw = format!("printf raw > {}", shell_escape(&output.to_string_lossy()));
        let marked = format!(
            "printf marked > {}",
            shell_escape(&output.to_string_lossy())
        );
        let gate = gate_omp_launch(&raw, &marked, &omp_test_plan());
        let outer = shell_stdin_command("sh", false, &format!("exec env {gate}"), "AOE_TEST_OUTER");
        let script = temp.path().join("launch.sh");
        std::fs::write(&script, outer).unwrap();
        let status = std::process::Command::new("sh")
            .arg(&script)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("TMUX_PANE", "%1")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "marked");

        // A valid 70 KiB prompt makes the capture gate body larger than
        // Linux's per-argument exec limit because the raw and marked branches
        // both contain it. The launch must still execute from the descriptor.
        let payload = "x".repeat(70 * 1024);
        let large_command = format!(
            "printf '%s' {} > {}",
            shell_escape(&payload),
            shell_escape(&output.to_string_lossy())
        );
        let large_gate = gate_omp_launch(&large_command, &large_command, &omp_test_plan());
        let large_outer = wrap_command_ignore_suspend(&large_gate, temp.path().to_str().unwrap());
        assert!(!large_outer.lines().next().unwrap().contains("-c"));
        std::fs::write(&script, large_outer).unwrap();
        let status = std::process::Command::new("sh")
            .arg(&script)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("TMUX_PANE", "%1")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::metadata(output).unwrap().len(),
            payload.len() as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn omp_private_paths_reject_symlink_fifo_and_breadcrumb_races() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        let marker_link = dir.path().join("marker-link.tmp");
        let marker_fifo = dir.path().join("marker-fifo.tmp");
        std::fs::write(&victim, "unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, &marker_link).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(&marker_fifo)
            .status()
            .unwrap()
            .success());

        for collision in [&marker_link, &marker_fifo] {
            let output = std::process::Command::new("sh")
                .args(["-c", "(umask 077; mkdir \"$1\")", "sh"])
                .arg(collision)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "private-dir creation must reject an existing path"
            );
        }

        let placeholder = dir.path().join("placeholder");
        let raced_file = dir.path().join("breadcrumb-file");
        let raced_link = dir.path().join("breadcrumb-link");
        std::fs::write(&placeholder, "cwd\nsentinel\nfresh\n").unwrap();
        std::fs::write(&raced_file, "winner").unwrap();
        std::os::unix::fs::symlink(&victim, &raced_link).unwrap();
        let raced_dir_link = dir.path().join("breadcrumb-dir-link");
        std::os::unix::fs::symlink(dir.path(), &raced_dir_link).unwrap();
        for collision in [&raced_file, &raced_link, &raced_dir_link] {
            let output = std::process::Command::new("sh")
                .args(["-c", "ln -n \"$1\" \"$2\"", "sh"])
                .arg(&placeholder)
                .arg(collision)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "hardlink installation must not clobber a raced destination"
            );
        }
        assert_eq!(std::fs::read_to_string(raced_file).unwrap(), "winner");
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "unchanged");
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"))
            .unwrap();
        assert!(cmd.is_some());
        assert!(cmd.as_ref().unwrap().contains("codex"));
    }

    #[test]
    fn test_build_host_command_with_yolo() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.yolo_mode = true;
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("claude"))
            .unwrap();
        let cmd_str = cmd.unwrap();
        assert!(cmd_str.contains("ses_abc123def456"));
        assert!(cmd_str.contains("--session-id") || cmd_str.contains("--resume"));
    }

    #[test]
    fn test_build_host_command_antigravity_forces_color() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "antigravity".to_string();
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"))
            .unwrap();
        assert!(cmd.unwrap().contains("kiro-cli chat"));
    }

    #[test]
    fn test_build_host_command_kiro_yolo_after_chat() {
        // YOLO flag must follow the `chat` subcommand, not precede it.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "kiro".to_string();
        inst.yolo_mode = true;
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("kiro"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("antigravity"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("codex"))
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
        let (cmd, _, _) = inst
            .build_host_command(crate::agents::get_agent("cursor"))
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
        use crate::session::test_support::EnvGuard;
        use serial_test::serial;
        use tempfile::tempdir;

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
                    .create(&inst.project_path, Some("sleep 60"), "default")
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

            assert!(fallback_source.contains("let attempted_sid = match launch_outcome"));
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
            let start = source.find("fn finish_resume_launch").unwrap();
            let end = source.find("pub fn ensure_pane_ready").unwrap();
            let fallback_source = &source[start..end];
            let local_marker = fallback_source
                .find("self.resume_probe_failed_sid = Some(stale_sid.clone())")
                .unwrap();
            let persisted_marker = fallback_source
                .find("self.mark_resume_probe_failed(profile, &stale_sid)")
                .unwrap();
            let cleanup = fallback_source.find("self.kill_clean_locked()").unwrap();

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
        fn reconcile_from_disk_preserves_unknown_streak_tracking() {
            // `ever_confirmed_present` and `unknown_since` are both
            // `#[serde(skip)]`, so the disk snapshot always has them at their
            // defaults (`false` / `None`). reconcile_from_disk (run before
            // every launch) must carry the live values forward, or a
            // previously-confirmed-present session would lose its long
            // tolerance window and drop back to the short never-present one
            // on every relaunch.
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage =
                crate::session::storage::Storage::new_unwatched("reconcile-unknown-since").unwrap();
            let mut inst = Instance::new("title", "/tmp/x");
            inst.source_profile = "reconcile-unknown-since".to_string();
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

            // Stamp the runtime tracking state into the in-memory instance
            // only, mirroring what a live poll tick would have set.
            inst.ever_confirmed_present = true;
            let unknown_since = std::time::Instant::now() - std::time::Duration::from_secs(5);
            inst.unknown_since = Some(unknown_since);

            inst.reconcile_from_disk();

            assert!(
                inst.ever_confirmed_present,
                "ever_confirmed_present must survive the pre-launch disk reload"
            );
            assert_eq!(
                inst.unknown_since,
                Some(unknown_since),
                "unknown_since must survive the pre-launch disk reload"
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
            // Isolate HOME and CLAUDE_CONFIG_DIR at an empty tempdir so
            // `acquire_session_id`'s freshest-observation probe reads scratch
            // state, never the caller's real `~/.claude`. Without this the
            // probe scans `~/.claude/projects/-tmp-x`, and any live transcript
            // there (present in a Claude dev environment) supersedes the stored
            // sid, so the assertion below fails deterministically. Mirrors the
            // `verify_on_resume` submodule's `claude_home_guard`.
            let temp = tempdir().unwrap();
            let mut pairs: Vec<(&'static str, std::path::PathBuf)> =
                vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
            let _home = EnvGuard::set(&pairs);

            let mut inst = Instance::new("intent-default", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("observed".to_string());
            inst.resume_intent = ResumeIntent::Default;

            // Default intent keeps the observed sid as the owned session. With
            // the isolated home holding no transcript for it, the empty thread
            // launches fresh-pinned (`is_existing = false`, `--session-id`)
            // rather than a certain-to-fail `--resume`.
            let (sid, is_existing) = inst.acquire_session_id();
            assert_eq!(sid.as_deref(), Some("observed"));
            assert!(!is_existing);
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
            let _opencode_db = EnvGuard::set(&[("OPENCODE_DB", &db_path)]);

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
            use crate::session::test_support::isolate_app_dir_at;
            use std::fs;
            use std::path::PathBuf;
            use std::time::{Duration, SystemTime};
            use tempfile::{tempdir, TempDir};

            /// Points `HOME`, `CLAUDE_CONFIG_DIR` (and, on Linux/macOS,
            /// `XDG_CONFIG_HOME`) at `temp` for the current test body.
            /// See [`crate::session::test_support`]: the snapshot/restore
            /// is `EnvGuard`'s, so a non-UTF-8 prior value round-trips
            /// instead of being dropped (#2751).
            fn claude_home_guard(temp: &TempDir) -> EnvGuard {
                let mut pairs: Vec<(&'static str, PathBuf)> =
                    vec![("HOME", temp.path().to_path_buf())];
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
                pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
                EnvGuard::set(&pairs)
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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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
            // sees live tmux peers; `compose_exclusion_with_persisted_peers`
            // adds the stopped peer's sid from `sessions.json` so this
            // instance's own (older) jsonl wins.
            #[test]
            #[serial]
            fn mtime_fallback_skips_stopped_peer_sid() {
                let temp = tempdir().unwrap();
                let _guard = claude_home_guard(&temp);

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

            // Companion to the above for the engine swap: the peer is not a
            // Claude session any more (it swapped to pi), so it no longer
            // passes the `tool` filter in
            // `compose_exclusion_with_persisted_peers`, and its Claude sid moved
            // out of `agent_session_id` into `prior_tool_session_ids`. Unless
            // parked ids are excluded too, the peer's Claude transcript is in no
            // exclusion set at all and the mtime fallback hands it to this
            // instance, which both steals the conversation the peer intends to
            // resume on a swap back and leaks its context.
            #[test]
            #[serial]
            fn mtime_fallback_skips_peer_sid_parked_by_a_tool_swap() {
                let temp = tempdir().unwrap();
                let _guard = claude_home_guard(&temp);

                let project_path = "/tmp/aoe-test-parked-peer";
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(project_path));
                fs::create_dir_all(&claude_dir).unwrap();

                let mine = "55555555-5555-4555-8555-555555555555";
                let parked = "66666666-6666-4666-8666-666666666666";
                let now = SystemTime::now();
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{mine}.jsonl")),
                    now - Duration::from_secs(120),
                );
                write_jsonl_with_mtime(
                    &claude_dir.join(format!("{parked}.jsonl")),
                    now - Duration::from_secs(5),
                );

                let profile = "verify-parked-peer";
                let mut peer_inst = Instance::new("swapped-peer-id", project_path);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "claude".to_string();
                peer_inst.agent_session_id = Some(parked.to_string());
                // The peer is mid-life and running: only its Claude
                // conversation is parked, not the row.
                peer_inst.status = Status::Running;
                peer_inst.swap_tool("pi");
                peer_inst.agent_session_id = Some("pi-session-parked".to_string());
                peer_inst.swap_tool("codex");
                assert_eq!(peer_inst.tool, "codex");
                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let pi_exclusion = crate::session::capture::compose_exclusion_with_persisted_peers(
                    "other-pi-instance",
                    project_path,
                    "pi",
                    false,
                    profile,
                    &std::collections::HashSet::new(),
                );
                assert!(
                    pi_exclusion.contains("pi-session-parked"),
                    "parked ids must be protected for every resumable tool"
                );

                let mut inst = Instance::new("verify-parked", project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(
                    sid.as_deref(),
                    Some(mine),
                    "the parked peer's fresher transcript must not be adopted"
                );
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // Companion to the above for #2858: the stopped peer's stored
            // `project_path` is an UNNORMALIZED spelling of the same
            // directory (`<parent>/decoy/../wt` vs `<parent>/wt`), as the
            // default `../{repo-name}-worktrees/{branch}` template used to
            // produce. A raw string comparison in
            // `compose_exclusion_with_persisted_peers` drops the peer from the
            // exclusion and re-opens the #2355 steal; the canonicalized
            // comparison must keep it.
            #[test]
            #[serial]
            fn mtime_fallback_skips_stopped_peer_with_unnormalized_path() {
                let temp = tempdir().unwrap();
                let _guard = claude_home_guard(&temp);

                let parent = temp.path().join("proj");
                fs::create_dir_all(parent.join("decoy")).unwrap();
                fs::create_dir_all(parent.join("wt")).unwrap();
                let project_path = parent.join("wt").to_string_lossy().to_string();
                let unnormalized = parent
                    .join("decoy")
                    .join("..")
                    .join("wt")
                    .to_string_lossy()
                    .to_string();

                // `acquire_session_id` canonicalizes before encoding, so the
                // transcript dir must be keyed by the canonical path (on
                // macOS `/tmp` itself resolves to `/private/tmp`).
                let canonical = std::fs::canonicalize(&project_path).unwrap();
                let claude_dir = temp
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(encode_claude_project_path(&canonical.to_string_lossy()));
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

                let profile = "verify-2858-unnormalized-peer";
                let mut peer_inst = Instance::new("unnormalized-peer-id", &unnormalized);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "claude".to_string();
                peer_inst.agent_session_id = Some(peer.to_string());
                peer_inst.status = Status::Stopped;
                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let mut inst = Instance::new("verify-2858", &project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "claude".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, _is_existing) = inst.acquire_session_id();
                assert_eq!(
                    sid.as_deref(),
                    Some(mine),
                    "peer with an unnormalized spelling of the same dir must still be excluded"
                );
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            // Companion to the above: same setup but the peer is archived
            // instead of stopped, exercising the `is_archived()` branch of
            // `compose_exclusion_with_persisted_peers`.
            #[test]
            #[serial]
            fn mtime_fallback_skips_archived_peer_sid() {
                let temp = tempdir().unwrap();
                let _guard = claude_home_guard(&temp);

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
                let _guard = claude_home_guard(&temp);

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

            // ── Per-tool bascule coverage (#2304) ────────────────────────────
            //
            // The Claude bascule above proves `acquire_session_id`'s Default arm
            // supersedes a stale stored sid with a fresher live observation. The
            // other six live-tracked agents inherit that behaviour through the
            // same `try_retroactive_capture` dispatch, but a regression in an
            // individual match arm (an accidental arm deletion or signature
            // drift) would not be caught by the Claude test alone. Each test
            // below seeds two on-disk sessions for one tool (older = stored,
            // newer = fresh) and asserts acquire replaces the stored sid with
            // the fresher one, exercising that tool's dispatch arm end-to-end.
            //
            // Each points `HOME` at a tempdir via `isolate_app_dir_at` so the
            // exclusion-set scan reads an empty storage rather than the
            // developer's real sessions.json. The tempdir is declared before
            // the guard so the guard drops first, restoring the env before the
            // directory `HOME` points at is removed.

            fn write_with_mtime(path: &std::path::Path, content: &str, mtime: SystemTime) {
                fs::write(path, content).unwrap();
                let f = fs::File::options().write(true).open(path).unwrap();
                f.set_times(fs::FileTimes::new().set_modified(mtime))
                    .unwrap();
            }

            #[test]
            #[serial]
            fn supersedes_stale_opencode_sid() {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());

                let project_path = temp.path().join("opencode-project");
                fs::create_dir_all(&project_path).unwrap();
                let project_path = project_path.to_string_lossy().to_string();

                let db_path = temp.path().join("opencode.db");
                let stale = "ses_opencode_stored";
                let fresh = "ses_opencode_fresh";
                seed_opencode_db(&db_path, stale, &project_path);
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.execute(
                    "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                    rusqlite::params![fresh, project_path, 2_000_000_i64],
                )
                .unwrap();
                drop(conn);
                let _db = EnvGuard::set(&[("OPENCODE_DB", &db_path)]);

                let mut inst = Instance::new("verify-opencode-bascule", &project_path);
                inst.tool = "opencode".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn supersedes_stale_vibe_sid() {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _vibe = EnvGuard::set(&[("VIBE_HOME", temp.path())]);

                let project_path = temp.path().join("vibe-project");
                fs::create_dir_all(&project_path).unwrap();
                let project_path = project_path.to_string_lossy().to_string();

                let sessions_dir = temp.path().join("logs").join("session");
                let stale = "vibe-stored-sid";
                let fresh = "vibe-fresh-sid";
                let now = SystemTime::now();
                for (sid, dir, age) in [(stale, "session-stale", 120), (fresh, "session-fresh", 10)]
                {
                    let sdir = sessions_dir.join(dir);
                    fs::create_dir_all(&sdir).unwrap();
                    let meta = serde_json::json!({
                        "session_id": sid,
                        "environment": {"working_directory": project_path},
                    });
                    write_with_mtime(
                        &sdir.join("meta.json"),
                        &meta.to_string(),
                        now - Duration::from_secs(age),
                    );
                }

                let mut inst = Instance::new("verify-vibe-bascule", &project_path);
                inst.tool = "vibe".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn supersedes_stale_codex_sid() {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _codex = EnvGuard::set(&[("CODEX_HOME", temp.path())]);

                let project_path = temp.path().join("codex-project");
                fs::create_dir_all(&project_path).unwrap();
                let project_path = project_path.to_string_lossy().to_string();

                let sessions_dir = temp.path().join("sessions");
                fs::create_dir_all(&sessions_dir).unwrap();
                let stale = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
                let fresh = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
                let now = SystemTime::now();
                for (uuid, age) in [(stale, 120), (fresh, 10)] {
                    let body = format!(
                        r#"{{"type":"session_meta","payload":{{"cwd":"{project_path}"}}}}"#
                    );
                    write_with_mtime(
                        &sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{uuid}.jsonl")),
                        &body,
                        now - Duration::from_secs(age),
                    );
                }

                let mut inst = Instance::new("verify-codex-bascule", &project_path);
                inst.tool = "codex".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn codex_mtime_fallback_skips_stopped_host_peer_sid() {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _codex = EnvGuard::set(&[("CODEX_HOME", temp.path())]);

                let project_path = temp.path().join("codex-project");
                fs::create_dir_all(&project_path).unwrap();
                let project_path = project_path.to_string_lossy().to_string();

                let sessions_dir = temp.path().join("sessions");
                fs::create_dir_all(&sessions_dir).unwrap();
                let mine = "11111111-1111-4111-8111-111111111111";
                let peer = "22222222-2222-4222-8222-222222222222";
                let now = SystemTime::now();
                for (uuid, age) in [(mine, 120), (peer, 5)] {
                    let body = format!(
                        r#"{{"type":"session_meta","payload":{{"cwd":"{project_path}"}}}}"#
                    );
                    write_with_mtime(
                        &sessions_dir.join(format!("rollout-2025-03-06T10-30-00-{uuid}.jsonl")),
                        &body,
                        now - Duration::from_secs(age),
                    );
                }

                let profile = "verify-codex-stopped-host-peer";
                let mut peer_inst = Instance::new("stopped-codex-peer-id", &project_path);
                peer_inst.source_profile = profile.to_string();
                peer_inst.tool = "codex".to_string();
                peer_inst.agent_session_id = Some(peer.to_string());
                peer_inst.status = Status::Stopped;
                super::seed_disk_for_sidecar_test(profile, &peer_inst);

                let mut inst = Instance::new("verify-codex-host-peer", &project_path);
                inst.source_profile = profile.to_string();
                inst.tool = "codex".to_string();
                inst.agent_session_id = Some(mine.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(mine));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(mine));
            }

            #[test]
            #[serial]
            fn supersedes_stale_gemini_sid() {
                use sha2::{Digest, Sha256};

                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _gemini = EnvGuard::set(&[("GEMINI_CLI_HOME", temp.path())]);

                let project_dir = temp.path().join("gemini-project");
                fs::create_dir_all(&project_dir).unwrap();
                let project_path = project_dir.to_string_lossy().to_string();

                // Directory name is sha256 of the canonicalized cwd, matching the
                // capture function's exact-match branch.
                let canonical = fs::canonicalize(&project_dir).unwrap();
                let hash = Sha256::digest(canonical.to_string_lossy().as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let chats_dir = temp.path().join("tmp").join(&hash).join("chats");
                fs::create_dir_all(&chats_dir).unwrap();

                let stale = "gemini-stored-id";
                let fresh = "gemini-fresh-id";
                let now = SystemTime::now();
                for (sid, age) in [(stale, 120), (fresh, 10)] {
                    let body =
                        format!(r#"{{"sessionId":"{sid}","projectHash":"{hash}","kind":"main"}}"#);
                    write_with_mtime(
                        &chats_dir.join(format!("session-{sid}.json")),
                        &body,
                        now - Duration::from_secs(age),
                    );
                }

                let mut inst = Instance::new("verify-gemini-bascule", &project_path);
                inst.tool = "gemini".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn supersedes_stale_pi_sid() {
                use crate::session::capture::encode_pi_project_path;

                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _pi = EnvGuard::set(&[("PI_CODING_AGENT_DIR", temp.path())]);

                let project_dir = temp.path().join("pi-project");
                fs::create_dir_all(&project_dir).unwrap();
                let project_path = project_dir.to_string_lossy().to_string();

                let project_session_dir = temp
                    .path()
                    .join("sessions")
                    .join(encode_pi_project_path(&project_path));
                fs::create_dir_all(&project_session_dir).unwrap();

                let stale = "cccccccc-3333-4333-8333-cccccccccccc";
                let fresh = "dddddddd-4444-4444-8444-dddddddddddd";
                let now = SystemTime::now();
                for (sid, age) in [(stale, 120), (fresh, 10)] {
                    let body =
                        format!(r#"{{"type":"session","id":"{sid}","cwd":"{project_path}"}}"#);
                    write_with_mtime(
                        &project_session_dir.join(format!("20260101T000000_{sid}.jsonl")),
                        &body,
                        now - Duration::from_secs(age),
                    );
                }

                let mut inst = Instance::new("verify-pi-bascule", &project_path);
                inst.tool = "pi".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
            }

            #[test]
            #[serial]
            fn supersedes_stale_hermes_sid() {
                let temp = tempdir().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let _hermes = EnvGuard::set(&[("HERMES_HOME", temp.path())]);

                let db_path = temp.path().join("state.db");
                let stale = "20260101_000000_stored";
                let fresh = "20260101_000000_fresh";
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.execute_batch(&format!(
                    "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL);
                     INSERT INTO sessions VALUES ('{stale}','cli',1000.0,NULL);
                     INSERT INTO sessions VALUES ('{fresh}','cli',2000.0,NULL);",
                ))
                .unwrap();
                drop(conn);

                // Hermes ignores the project path; it keys off the state.db rows.
                let mut inst = Instance::new("verify-hermes-bascule", "/tmp/aoe-test-2304-hermes");
                inst.tool = "hermes".to_string();
                inst.agent_session_id = Some(stale.to_string());
                inst.resume_intent = ResumeIntent::Default;

                let (sid, is_existing) = inst.acquire_session_id();
                assert_eq!(sid.as_deref(), Some(fresh));
                assert!(is_existing);
                assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
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
        fn use_intent_promotes_to_default_after_launch() {
            let temp = tempdir().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let profile = "use-promote";
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let pinned = "019342ab-1234-7def-8901-abcdef012345";

            let mut inst = Instance::new("Pinned", "/tmp/x");
            inst.tool = "claude".into();
            inst.source_profile = profile.into();
            inst.agent_session_id = Some(pinned.into());
            inst.resume_intent = ResumeIntent::Use(pinned.into());

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

            // Simulate the post-launch persist: expected_prior_intent is the Use
            // we launched with; the pinned id is already in agent_session_id.
            let expected_prior = inst.resume_intent.clone();
            let expected_sid = inst.agent_session_id.clone();
            let _ = inst.persist_session_id(profile, expected_sid.as_deref(), expected_prior);

            let reloaded = storage.load().unwrap();
            let disk = reloaded.iter().find(|i| i.id == inst.id).unwrap();
            assert_eq!(
                disk.resume_intent,
                ResumeIntent::Default,
                "Use must auto-promote to Default after the launch consumes the pin so the drain adopts subsequent post-launch captures (#2708)",
            );
            assert_eq!(
                inst.resume_intent,
                ResumeIntent::Default,
                "In-memory resume_intent must also promote so the drain PIN guard stops firing on the same tick",
            );
            assert_eq!(disk.agent_session_id.as_deref(), Some(pinned));
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test").unwrap();

            let stale_sid = "11111111-1111-1111-1111-111111111111".to_string();
            let mut inst = Instance::new("fallback_dies_test", project_path);
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test-live").unwrap();

            let stale_sid = "22222222-2222-2222-2222-222222222222".to_string();
            let mut inst = Instance::new("fallback_lives_test", project_path);
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            crate::session::config::update_config(|cfg| {
                cfg.session.auto_resume_on_restart = false;
            })
            .unwrap();

            let storage = crate::session::storage::Storage::new_unwatched("fb-toggle-off").unwrap();

            let stale_sid = "44444444-4444-4444-4444-444444444444".to_string();
            let mut inst = Instance::new("fallback_toggle_off_test", project_path);
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            crate::session::config::update_config(|cfg| {
                cfg.session.auto_resume_on_restart = false;
            })
            .unwrap();

            let storage =
                crate::session::storage::Storage::new_unwatched("fb-allow-ignores").unwrap();

            let stale_sid = "55555555-5555-5555-5555-555555555555".to_string();
            let mut inst = Instance::new("fallback_allow_ignores_toggle_test", project_path);
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-loop-break").unwrap();

            let stale_sid = "66666666-6666-6666-6666-666666666666".to_string();
            let mut inst = Instance::new("fallback_loop_break_test", project_path);
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
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let project_path = project_dir.to_str().unwrap();
            std::env::set_var("HOME", temp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

            let storage = crate::session::storage::Storage::new_unwatched("fb-test-grace").unwrap();

            let stale_sid = "33333333-3333-3333-3333-333333333333".to_string();
            let mut inst = Instance::new("fallback_grace_test", project_path);
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

    mod sid_disk_guards {
        use super::super::{
            persist_session_to_storage, Instance, ResumeIntent, SidPersistOutcome, SidWrite,
        };
        use crate::file_watch::FileWatchService;
        use crate::session::storage::Storage;
        use crate::session::test_support::EnvGuard;
        use crate::session::GroupTree;
        use serial_test::serial;
        use std::path::PathBuf;
        use tempfile::{tempdir, TempDir};

        const SID_X: &str = "019342ab-1234-7def-8901-111111111111";
        const SID_Y: &str = "019342ab-1234-7def-8901-222222222222";

        fn storage_home_guard(temp: &TempDir) -> EnvGuard {
            #[allow(unused_mut)]
            let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            EnvGuard::set(&pairs)
        }

        fn seed(profile: &str, insts: &[&Instance]) {
            let storage = Storage::new_unwatched(profile).unwrap();
            let owned: Vec<Instance> = insts.iter().map(|i| (*i).clone()).collect();
            storage
                .update(|i, g| {
                    *i = owned.clone();
                    *g = GroupTree::new_with_groups(&owned, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();
        }

        fn load(profile: &str) -> Vec<Instance> {
            Storage::new_unwatched(profile).unwrap().load().unwrap()
        }

        fn make_inst(profile: &str, title: &str) -> Instance {
            let mut inst = Instance::new(title, "/tmp/x");
            inst.source_profile = profile.to_string();
            inst
        }

        #[test]
        #[serial]
        fn persist_rejects_sid_owned_by_another_instance_on_disk() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-owned";

            let mut owner = make_inst(profile, "owner");
            owner.agent_session_id = Some(SID_X.to_string());
            let claimant = make_inst(profile, "claimant");
            seed(profile, &[&owner, &claimant]);

            let file_watch = FileWatchService::noop();
            let write = persist_session_to_storage(profile, &claimant.id, SID_X, None, &file_watch);

            assert_eq!(write, SidWrite::Skipped);
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == claimant.id)
                    .unwrap()
                    .agent_session_id,
                None
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == owner.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
        }

        #[test]
        #[serial]
        fn persist_rejects_sid_contradicting_on_disk_pin() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-pin";

            // The pin exists only on disk (written by `aoe session
            // set-session-id` in another process); the caller's expected_prior
            // matches, so only the flock-scoped pin guard can reject this.
            let mut pinned = make_inst(profile, "pinned");
            pinned.agent_session_id = Some(SID_X.to_string());
            pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
            seed(profile, &[&pinned]);

            let file_watch = FileWatchService::noop();
            let write =
                persist_session_to_storage(profile, &pinned.id, SID_Y, Some(SID_X), &file_watch);
            assert_eq!(write, SidWrite::Skipped);
            assert_eq!(
                load(profile)[0].agent_session_id.as_deref(),
                Some(SID_X),
                "pin must stay authoritative against a differing write"
            );

            // A write matching the pin is normal capture and must pass.
            let write =
                persist_session_to_storage(profile, &pinned.id, SID_X, Some(SID_X), &file_watch);
            assert_eq!(write, SidWrite::Applied);
        }

        #[test]
        #[serial]
        fn finalize_persist_rejects_foreign_sid_without_pin() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-reject";

            let mut owner = make_inst(profile, "owner");
            owner.agent_session_id = Some(SID_X.to_string());
            let claimant = make_inst(profile, "claimant");
            seed(profile, &[&owner, &claimant]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = claimant.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome =
                live.persist_session_id_with_storage(&storage, None, ResumeIntent::Default);

            // Skipped-and-reloaded: memory converges back to the disk value.
            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(live.agent_session_id, None);
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == owner.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == claimant.id)
                    .unwrap()
                    .agent_session_id,
                None
            );
        }

        #[test]
        #[serial]
        fn finalize_persist_consuming_pin_takes_ownership_from_stale_holder() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-pin";

            // The documented repair for a same-cwd duplicate: pin the true
            // owner via `set-session-id`, then launch it. The launch that
            // consumes the pin must take the sid even though stale holders
            // still carry it on disk — and every stale holder is relieved of
            // it so no duplicate can persist. Two holders because the bug
            // being repaired manufactures duplicates, so more than one stale
            // row with the same sid is a reachable state.
            let mut stale_holder = make_inst(profile, "stale-holder");
            stale_holder.agent_session_id = Some(SID_X.to_string());
            let mut second_holder = make_inst(profile, "second-holder");
            second_holder.agent_session_id = Some(SID_X.to_string());
            let mut pinned = make_inst(profile, "pinned");
            pinned.resume_intent = ResumeIntent::Use(SID_X.to_string());
            seed(profile, &[&stale_holder, &second_holder, &pinned]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = pinned.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome = live.persist_session_id_with_storage(
                &storage,
                None,
                ResumeIntent::Use(SID_X.to_string()),
            );

            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(live.agent_session_id.as_deref(), Some(SID_X));
            assert_eq!(
                live.resume_intent,
                ResumeIntent::Default,
                "consumed pin must promote to Default"
            );
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == pinned.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X)
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == stale_holder.id)
                    .unwrap()
                    .agent_session_id,
                None,
                "stale holder must be relieved of the sid the pin claimed"
            );
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == second_holder.id)
                    .unwrap()
                    .agent_session_id,
                None,
                "every duplicate holder must be relieved, not just the first"
            );
        }

        #[test]
        #[serial]
        fn finalize_persist_stale_pin_snapshot_does_not_take_ownership() {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "guards-finalize-stale-pin";

            // The caller consumed a Use(SID_X) pin pre-launch, but a peer
            // process has since rewritten the on-disk intent (here: cleared
            // it back to Default). The stale snapshot alone must not
            // authorize taking the sid from its current holder; the write is
            // rejected and memory converges to disk.
            let mut holder = make_inst(profile, "holder");
            holder.agent_session_id = Some(SID_X.to_string());
            let launcher = make_inst(profile, "launcher");
            seed(profile, &[&holder, &launcher]);

            let storage = Storage::new_unwatched(profile).unwrap();
            let mut live = launcher.clone();
            live.agent_session_id = Some(SID_X.to_string());
            let outcome = live.persist_session_id_with_storage(
                &storage,
                None,
                ResumeIntent::Use(SID_X.to_string()),
            );

            assert_eq!(outcome, SidPersistOutcome::Published);
            assert_eq!(
                live.agent_session_id, None,
                "launcher must converge to disk, not keep the contested sid"
            );
            let disk = load(profile);
            assert_eq!(
                disk.iter()
                    .find(|i| i.id == holder.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(SID_X),
                "holder must keep the sid when the pin is gone from disk"
            );
        }
    }

    mod publish_captured_sid {
        use super::super::{Instance, ResumeIntent, Status};
        use serial_test::serial;
        use std::collections::HashSet;
        use tempfile::{tempdir, TempDir};

        const VALID_SID: &str = "019342ab-1234-7def-8901-abcdef012345";
        const PEER_SID: &str = "019342aa-2222-7eee-8fff-aaaabbbbcccc";

        /// Stand-in for the post-CAS env publish in
        /// `sync::drain_and_persist_session_ids` (the poller's pre-CAS
        /// on_change publish was removed in #2858): writes the same two keys
        /// so these tests keep exercising the env naming and the
        /// `build_exclusion_set` attribution contract.
        fn publish_session_to_tmux_env(
            tmux_session_name: &str,
            instance_id: &str,
            session_id: &str,
        ) {
            for (key, value) in [
                (crate::tmux::env::AOE_INSTANCE_ID_KEY, instance_id),
                (crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY, session_id),
            ] {
                crate::tmux::env::set_hidden_env(tmux_session_name, key, value)
                    .unwrap_or_else(|e| panic!("failed to write {key} to tmux env: {e}"));
            }
        }

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
        fn omp_launch_without_capture_plan_publishes_tombstone_generation() {
            let temp = tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "omp-plan-failure-tombstone";
            let mut inst = make_inst(profile, "omp-plan-failure");
            inst.tool = "omp".to_string();
            let old_generation = "omp-old-generation";
            let stale_sid = "019342ab-1234-7def-8901-abcdef012349";
            inst.omp_capture_generation = Some(old_generation.to_string());
            seed_disk_row(profile, &inst);

            assert!(inst.publish_omp_launch_generation(profile, None, Some(old_generation)));
            let disk = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap();
            assert!(disk[0].omp_capture_generation.is_some());
            assert_eq!(disk[0].omp_capture_generation, inst.omp_capture_generation);
            assert_ne!(
                disk[0].omp_capture_generation.as_deref(),
                Some(old_generation)
            );
            assert_eq!(
                super::super::persist_omp_session_to_storage(
                    profile,
                    &inst.id,
                    stale_sid,
                    None,
                    Some(old_generation),
                    &crate::file_watch::FileWatchService::noop(),
                ),
                super::super::SidWrite::Skipped
            );
        }

        #[test]
        #[serial]
        fn stopped_poller_flush_persists_newest_omp_observation_without_tmux() {
            let temp = tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let profile = "omp-restart-final-flush";
            let generation = "omp-restart-generation";
            let sid = "019342ab-1234-7def-8901-abcdef012348";
            let mut inst = make_inst(profile, "omp-restart-flush");
            inst.tool = "omp".to_string();
            inst.omp_capture_generation = Some(generation.to_string());
            inst.status = Status::Stopped;
            seed_disk_row(profile, &inst);

            let poller = crate::session::poller::SessionPoller::new("unused-tmux".to_string());
            poller.inject_test_omp_update(&inst.id, sid, generation);
            inst.session_id_poller = Some(std::sync::Arc::new(std::sync::Mutex::new(poller)));
            inst.stop_and_flush_poller();

            assert!(inst.session_id_poller.is_none());
            assert_eq!(inst.agent_session_id.as_deref(), Some(sid));
            let disk = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap();
            assert_eq!(disk[0].agent_session_id.as_deref(), Some(sid));
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
        fn finalize_publish_applied_writes_omp_metadata() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let profile = "publish-applied";
            let mut inst = make_inst(profile, "fpaw");
            inst.tool = "omp".to_string();
            inst.pending_host_env = vec![
                ("OMP_PROFILE".to_string(), "work".to_string()),
                ("PI_CONFIG_DIR".to_string(), "/custom".to_string()),
            ];
            inst.agent_session_id = None;
            let plan = inst
                .resolve_omp_capture_plan(&inst.omp_capture_options().unwrap())
                .expect("OMP launch plan");
            let expected_layout = plan.layout.clone();
            seed_disk_row(profile, &inst);

            let tmux = TmuxSession::create(&inst.id, &inst.title);
            // Simulate dotenv/config drift after snapshot. Finalize must
            // publish the transported plan, not resolve these live values.
            inst.pending_host_env = vec![(
                "PI_CODING_AGENT_SESSION_DIR".to_string(),
                "/must-not-be-reread".to_string(),
            )];

            inst.agent_session_id = Some(VALID_SID.to_string());
            inst.finalize_launch(
                tmux.name(),
                profile,
                None,
                ResumeIntent::Default,
                Some(crate::session::capture::OmpCaptureMetadata {
                    layout: plan.layout,
                    launched_at_ms: 1000,
                    launch_id: plan.launch_id.clone(),
                    launch_marker: plan.launch_marker.clone(),
                    routing_fingerprint: plan.routing_fingerprint.clone(),
                    container_runtime: plan.container_runtime,
                }),
            );

            assert_eq!(captured_env(tmux.name()).as_deref(), Some(VALID_SID));
            let metadata: crate::session::capture::OmpCaptureMetadata = serde_json::from_str(
                &crate::tmux::env::get_hidden_env(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .expect("typed OMP capture metadata must survive poller reconstruction"),
            )
            .unwrap();
            assert_eq!(metadata.launched_at_ms, 1000);
            assert_eq!(metadata.layout, expected_layout);
            assert!(metadata.layout.sessions.is_absolute());
            assert!(metadata.layout.terminal_sessions.is_absolute());
            assert!(metadata.layout.managed_sessions.is_absolute());
            assert_eq!(metadata.launch_id, plan.launch_id);
        }

        #[test]
        #[serial]
        fn legacy_omp_pane_backfills_typed_metadata_from_tmux_creation() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let mut inst = make_inst("omp-legacy-metadata", "legacy-omp");
            inst.tool = "omp".to_string();
            inst.agent_session_id = Some(VALID_SID.to_string());
            let tmux = TmuxSession::create(&inst.id, &inst.title);
            assert!(crate::tmux::env::get_hidden_env(
                tmux.name(),
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            )
            .is_none());

            let expected_launch = crate::tmux::Session::from_name(tmux.name())
                .created_at_ms()
                .unwrap();
            let options = inst.omp_capture_options().unwrap();
            let metadata = inst
                .omp_capture_metadata(tmux.name(), &options, None)
                .expect("legacy pane should migrate");
            assert_eq!(metadata.launched_at_ms, expected_launch);
            assert_eq!(
                metadata.launch_id,
                format!("legacy-{}-{expected_launch}", inst.id)
            );
            assert!(metadata.layout.managed_sessions.is_absolute());

            let persisted: crate::session::capture::OmpCaptureMetadata = serde_json::from_str(
                &crate::tmux::env::get_hidden_env(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .expect("migration must backfill metadata"),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(persisted).unwrap(),
                serde_json::to_value(metadata).unwrap()
            );

            inst.omp_capture_generation = Some("modern-generation".to_string());
            assert!(
                inst.omp_capture_metadata(tmux.name(), &options, None)
                    .is_none(),
                "markerless typed metadata is legacy only while no durable generation exists"
            );
        }

        #[test]
        #[serial]
        fn modern_omp_pane_without_hidden_metadata_does_not_legacy_migrate() {
            if skip_if_no_tmux() {
                return;
            }
            let temp = tempdir().unwrap();
            isolate_home(&temp);

            let mut inst = make_inst("omp-modern-missing-metadata", "modern-omp");
            inst.tool = "omp".to_string();
            let generation = "modern-launch-generation";
            inst.omp_capture_generation = Some(generation.to_string());
            let tmux = TmuxSession::create(&inst.id, &inst.title);
            let status = crate::tmux::tmux_command()
                .args([
                    "set-environment",
                    "-t",
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY,
                    generation,
                ])
                .status()
                .unwrap();
            assert!(status.success());

            let options = inst.omp_capture_options().unwrap();
            assert!(
                inst.omp_capture_metadata(tmux.name(), &options, None)
                    .is_none(),
                "a current pane missing its hidden launch snapshot must fail closed"
            );
            assert!(
                crate::tmux::env::get_hidden_env_uncached(
                    tmux.name(),
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                )
                .is_none(),
                "the legacy path must not synthesize metadata for a current pane"
            );
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
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

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
            inst.finalize_launch(
                tmux.name(),
                profile,
                Some("stale"),
                ResumeIntent::Default,
                None,
            );

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
            inst.finalize_launch(
                tmux.name(),
                profile,
                Some("stale"),
                ResumeIntent::Default,
                None,
            );

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
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

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
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Default, None);

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
            inst.finalize_launch(tmux.name(), profile, None, ResumeIntent::Cleared, None);

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
