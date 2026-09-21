//! A persisted session row; each submodule adds one slice of `impl Instance`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::containers::{self, DockerContainer};
use crate::session::config::container_config;
use crate::session::conversation_carry::ConversationCarry;
use crate::session::environment::{
    build_docker_env_args_with_managed_codex_home, resolved_sandbox_environment, shell_escape,
    shell_escape_script_word,
};
use crate::session::poller::SessionPoller;
use crate::tmux;

use crate::session::capture::{
    capture_omp_session_id, codex_poll_fn_sandboxed_store, gemini_poll_fn_sandboxed_store,
    generate_session_uuid, hermes_poll_fn_sandboxed_store, is_valid_session_id,
    kimi_poll_fn_sandboxed_store, omp_host_routing_environment, omp_poll_fn, omp_poll_fn_sandboxed,
    omp_sandbox_launch_marker, prime_agent_poll_fn_sandboxed, reject_omp_secret_args,
    resolve_omp_store_layout, resolve_omp_store_layout_in_container_with_environment,
    resolve_omp_store_layout_with_environment, try_capture_omp_session_id_in_container,
    validate_omp_capture_metadata, validated_session_id, OmpCaptureMetadata, OmpCapturePlan,
    OmpCliCaptureOptions, OmpStoreKind, PrimeRootPublication,
};
mod accessors;
mod container;
mod flags;
mod hooks;
mod identity_sidecar;
mod kill;
mod launch_command;
mod lifecycle;
mod merge;
mod omp;
mod pane_status;
mod polling;
mod prime_capture;
mod ready;
mod reconcile;
mod resume;
mod retroactive_capture;
mod session_id;
mod sid_persist;
mod start;
mod status;
mod status_update;
mod terminal;
#[cfg(test)]
pub(crate) mod test_helpers;
mod tmux_session;
mod types;

/// Identity extension written for host launches and into sandbox binds.
pub(crate) const SESSION_IDENTITY_EXTENSION: &str =
    include_str!("../../../assets/session/aoe-session-id.js");

pub(crate) use accessors::resolved_agent_for;
pub use flags::{is_valid_session_color, SessionBucket, SESSION_COLORS};
pub(crate) use lifecycle::NEWER_GENERATION_BUSY_REASON;
pub use lifecycle::{LifecycleOperation, LifecycleReservation, LifecycleReservationError};
pub(crate) use omp::persist_omp_session_to_storage;
pub use polling::PollerStart;
pub use ready::{EnsureReadyError, EnsureReadyOutcome};
pub(crate) use resume::ResumeAttemptPolicy;
pub(crate) use sid_persist::{persist_session_to_storage, SidPersistOutcome, SidWrite};
pub use start::{LaunchSidOutcome, StartOutcome};
pub(crate) use status::PassiveStatusPatch;
pub use status::{Status, TMUX_SERVER_UNREACHABLE_ERROR, TMUX_SESSION_GONE_ERROR};
#[cfg(test)]
pub(crate) use test_helpers::install_aliases;
pub(crate) use tmux_session::{
    duplicate_session_error, find_duplicate_session, is_duplicate_session, AgentSeed,
};

/// Why a session can never resume, decided from the registry before any runtime probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeStaticUnavailable {
    Agent,
    Sandbox,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalContextResume {
    Available,
    RuntimeCheckRequired,
    NoTarget,
    AgentUnsupported,
    SandboxUnsupported,
    CommandUnsupported,
    ForcedFresh,
    InvalidTarget,
    ForkPending,
    PreviousFailure,
}
pub use types::{
    PluginCreateIdempotency, SandboxInfo, TerminalInfo, View, WorkspaceInfo, WorkspaceRepo,
    WorktreeInfo,
};
pub(crate) use types::{
    PrimeAgentCapturePlan, PriorToolSession, ResumeIntent, SandboxStoreTransitionPath,
    SessionSidecarSource,
};

// Sibling items the submodules reach through `use super::*`.
use hooks::status_hook_env_prefix;
pub(crate) use hooks::{generic_host_config_path_for, sidecar_host_config_path_for};
use launch_command::{
    append_resume_flags, build_fork_flags, parse_launch_command, shell_stdin_command,
    splice_subcommand_or_append, PreparedLaunch,
};
use omp::{gate_omp_launch, wrap_omp_host_launch, wrap_omp_launch};
use pane_status::{resolve_detected_status, summarize_error_from_pane};
use sid_persist::{override_if_distinct, persist_session_to_storage_guarded};
use status::{UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT, UNKNOWN_ERROR_WINDOW_NEVER_PRESENT};
use tmux_session::tmux_env_session_name_for_instance_id;
use types::{deserialize_session_id, is_zero_u64, is_zero_u8};

/// What one pane detection leaves for the next; every poller must carry it back
/// onto the live row or pending proposals are never confirmed (#3642).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectionState {
    pub activity: Option<i64>,
    /// Wall-clock second the last capture was stamped; `activity` has one-second
    /// granularity, so a capture inside that second proves nothing (#3624).
    pub captured_at: Option<i64>,
    pub rule: Option<&'static str>,
    /// Status proposed off non-chrome evidence, published once a second poll agrees.
    pub pending: Option<Status>,
}

/// A turn queued for delivery once the (resumed) worker is live. See
/// `Instance::pending_initial_turn`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInitialTurn {
    pub text: String,
    /// Attachment refs for a rate-limit resume continuation replaying a
    /// prompt that carried images/files (#3028). Metadata only; bytes stay in
    /// the acp_attachments store and are reloaded at drain time. Empty for
    /// create-time initial turns (those are text-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::daemon::PromptAttachmentRef>,
    /// True when the daemon queued this turn itself (a rate-limit resume
    /// continuation) rather than the user typing it at session-create time.
    /// Carried onto the resulting `Event::UserPromptSent` so the transcript
    /// model skips rendering a row for it: the user already saw this text
    /// once, before the park.
    #[serde(default)]
    pub synthesized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub title: String,
    /// Last title written by the automatic renamer; a manual rename leaves it stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_auto_title: Option<String>,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detect_as: String,
    #[serde(default)]
    pub yolo_mode: bool,
    #[serde(default)]
    pub status: Status,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_entered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorited_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_dormant_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_trash_project_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_reservation: Option<LifecycleReservation>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugin_meta: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_create_idempotency: Option<PluginCreateIdempotency>,

    /// A turn persisted with the session and not yet delivered to the agent:
    /// either the initial prompt from session create (#2897), or a
    /// rate-limit resume continuation replaying an interrupted prompt
    /// (#3028). Written in the same `Storage::update` that creates the row
    /// (create case) or by `SessionService::set_pending_initial_turn`
    /// (continuation case), so the write and its first turn are accepted
    /// atomically; the session service drains it once the ACP worker is live
    /// (create fast path, and the reconciler tick after a crash or restart)
    /// and clears it after a successful publish + forward. Delivery is
    /// at-least-once: a crash between the forward and this field's clear
    /// re-delivers on the next drain. v029 folded this from two flat fields
    /// (`pending_initial_turn: Option<String>` plus a companion
    /// `pending_initial_turn_attachments`) into one typed record once a third
    /// piece of turn state (`synthesized`) needed to ride along.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_initial_turn: Option<PendingInitialTurn>,

    /// Server-owned follow-ups, ordered by `QueuedPromptEntry::seq`. Persisted
    /// here so the daemon can drain them without a connected client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<crate::daemon::QueuedPromptEntry>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub queued_prompt_next_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scratch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_info: Option<WorktreeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_info: Option<WorkspaceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_info: Option<SandboxInfo>,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub(crate) sandbox_store_generation: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) sandbox_store_transition_paths: Vec<SandboxStoreTransitionPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_info: Option<TerminalInfo>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_session_id"
    )]
    pub agent_session_id: Option<String>,
    /// Poller observations must carry this through the storage CAS to update the sid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) omp_capture_generation: Option<String>,
    /// Monotone token; async merges touch lifecycle-owned fields only when this recent.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(crate) lifecycle_generation: u64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) prior_tool_session_ids: HashMap<String, PriorToolSession>,
    /// When equal to `agent_session_id`, startup recovery skips automatic resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resume_probe_failed_sid: Option<String>,
    #[serde(default, skip_serializing_if = "ResumeIntent::is_default")]
    pub(crate) resume_intent: ResumeIntent,
    /// Runtime one-shot forcing the next launch through the `Cleared` path (#2609).
    #[serde(skip)]
    pub(crate) force_fresh_next_launch: bool,
    /// Runtime only: the profile this row was loaded from.
    #[serde(default, skip_serializing)]
    pub source_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_waiting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_idle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_error: Option<bool>,
    /// Never exposed in API responses: URLs commonly embed bearer tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "View::is_terminal")]
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_pending: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_pending: Option<String>,
    #[serde(skip)]
    pub acp_load_session_capable: Option<bool>,
    #[serde(skip)]
    pub last_error_check: Option<std::time::Instant>,
    #[serde(skip)]
    pub last_start_time: Option<std::time::Instant>,
    /// Last status observed live by this in-memory row; `None` seeds without restamping.
    #[serde(skip)]
    pub live_status_baseline: Option<Status>,
    #[serde(skip)]
    pub ever_confirmed_present: bool,
    #[serde(skip)]
    pub unknown_since: Option<std::time::Instant>,
    #[serde(skip)]
    pub detection: DetectionState,
    /// Values minted by `host_hooks.before_session`; may be secrets, so never persisted.
    #[serde(skip)]
    pending_host_env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) capture_started_at: Option<std::time::SystemTime>,
    #[serde(skip)]
    pi_extension_launched: bool,
    #[serde(skip)]
    identity_publisher_launched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pi_session_path: Option<String>,
    #[serde(skip)]
    pub last_error: Option<String>,
    #[serde(skip)]
    pub session_id_poller: Option<Arc<Mutex<SessionPoller>>>,
    #[serde(skip)]
    pub(crate) poller_repair: crate::session::poller::PollerRepairBackoff,
    #[serde(skip)]
    pub(crate) session_id_poller_retry_after: Option<std::time::Instant>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub(crate) retroactive_capture_excludes: HashSet<String>,
    #[serde(skip)]
    pub pane_dead_observed: bool,
    #[serde(skip, default)]
    pub(crate) file_watch: Option<std::sync::Arc<crate::file_watch::FileWatchService>>,
}
