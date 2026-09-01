//! A session row: the persisted record of one agent session plus the
//! behavior that launches, polls, and tears it down.
//!
//! The struct lives here; every slice of its behavior lives in a submodule
//! and adds its own `impl Instance` block.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::containers::{self, DockerContainer};
use crate::session::container_config;
use crate::session::environment::{
    build_docker_env_args_with_managed_codex_home, resolved_sandbox_environment, shell_escape,
};
use crate::session::poller::SessionPoller;
use crate::tmux;

use crate::session::capture::{
    capture_claude_session_id, capture_claude_session_id_in_container, capture_codex_session_id,
    capture_copilot_session_id, capture_gemini_session_id, capture_hermes_session_id,
    capture_kimi_session_id, capture_omp_session_id, capture_prime_agent_session_id,
    capture_vibe_session_id, claude_poll_fn, claude_poll_fn_sandboxed, codex_poll_fn,
    codex_poll_fn_sandboxed, copilot_poll_fn, gemini_poll_fn, gemini_poll_fn_sandboxed,
    generate_session_uuid, hermes_poll_fn, hermes_poll_fn_sandboxed, is_valid_session_id,
    kimi_poll_fn, omp_host_routing_environment, omp_poll_fn, omp_poll_fn_sandboxed,
    omp_sandbox_launch_marker, opencode_poll_fn, opencode_poll_fn_sandboxed, prime_agent_poll_fn,
    reject_omp_secret_args, resolve_omp_store_layout,
    resolve_omp_store_layout_in_container_with_environment,
    resolve_omp_store_layout_with_environment, try_capture_codex_session_id_in_container,
    try_capture_gemini_session_id_in_container, try_capture_hermes_session_id_in_container,
    try_capture_omp_session_id_in_container, try_capture_opencode_session_id,
    try_capture_opencode_session_id_in_container, try_capture_vibe_session_id_in_container,
    validate_omp_capture_metadata, validated_session_id, vibe_poll_fn, vibe_poll_fn_sandboxed,
    OmpCaptureMetadata, OmpCapturePlan, OmpCliCaptureOptions, OmpStoreKind,
};

mod accessors;
mod container;
mod flags;
mod hooks;
mod kill;
mod launch_command;

/// The extension AoE loads into Pi so a pane publishes its own conversation.
/// Written to the app dir for a host launch and into the Pi sandbox dir for a
/// container one.
pub(crate) const PI_SESSION_EXTENSION: &str = include_str!("../../../assets/pi/aoe-session-id.js");
mod lifecycle;
mod merge;
mod omp;
mod pane_status;
mod polling;
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
mod test_helpers;
mod tmux_session;
mod types;

pub use flags::{is_valid_session_color, SessionBucket, SESSION_COLORS};
pub(crate) use lifecycle::NEWER_GENERATION_BUSY_REASON;
pub use lifecycle::{LifecycleOperation, LifecycleReservation, LifecycleReservationError};
pub(crate) use omp::persist_omp_session_to_storage;
pub use ready::{EnsureReadyError, EnsureReadyOutcome};
pub(crate) use resume::{should_attempt_resume, ResumeAttemptPolicy};
pub(crate) use sid_persist::{persist_session_to_storage, SidPersistOutcome, SidWrite};
pub use start::{LaunchSidOutcome, StartOutcome};
pub(crate) use status::PassiveStatusPatch;
pub use status::{Status, TMUX_SERVER_UNREACHABLE_ERROR, TMUX_SESSION_GONE_ERROR};
pub(crate) use tmux_session::{
    duplicate_session_error, find_duplicate_session, is_duplicate_session,
};
pub(crate) use types::{PiSidecarSource, PriorToolSession, ResumeIntent};
pub use types::{
    PluginCreateIdempotency, SandboxInfo, TerminalInfo, View, WorkspaceInfo, WorkspaceRepo,
    WorktreeInfo,
};

// Re-exported so each submodule can reach its siblings through `use super::*`.
use hooks::status_hook_env_prefix;
use launch_command::{
    append_resume_flags, build_fork_flags, shell_stdin_command, splice_subcommand_or_append,
    PreparedLaunch,
};
use omp::{gate_omp_launch, wrap_omp_host_launch, wrap_omp_launch};
use pane_status::{resolve_detected_status, summarize_error_from_pane};
use sid_persist::{override_if_distinct, persist_session_to_storage_guarded};
use status::{UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT, UNKNOWN_ERROR_WINDOW_NEVER_PRESENT};
use tmux_session::tmux_env_session_name_for_instance_id;
use types::{deserialize_session_id, is_zero_u64};

/// Runtime bookkeeping that one pane detection leaves for the next.
///
/// Not persisted, and no poller owns the row it decides on: the TUI polls a
/// clone and the daemon rebuilds every row from disk each tick. Each of those
/// boundaries has to carry this back onto the live row, or every poll starts
/// from `default()`, a `pending` proposal never meets the poll that agrees
/// with it, and a hookless manifest agent stays Running for the life of the
/// session (#3642).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectionState {
    /// tmux's last-output timestamp for the pane at the last capture. A pane
    /// that has drawn nothing since cannot have changed, so the capture is
    /// skipped and the previous verdict stands: a parked session costs one
    /// batched format read per poll rather than a subprocess.
    pub activity: Option<i64>,
    /// The wall-clock second in which the last capture was taken, stamped
    /// before it rather than after. `activity` has one-second granularity, so
    /// a capture taken inside the second it names can have read the screen
    /// before a later frame in that same second (#3624): the stamp is only
    /// proof of "nothing drawn since" once a capture has been taken past the
    /// end of that second. A clock that never satisfies that degrades to
    /// capturing every poll, which is the pre-gate behavior.
    pub captured_at: Option<i64>,
    /// The manifest rule that decided the last detection, for the
    /// status-change log. Names why a session is in the state it is in, which
    /// is what a wrong-state report needs and what a fingerprint of pane
    /// markers could only hint at.
    pub rule: Option<&'static str>,
    /// A status proposed by a rule that did not read it off the agent's own
    /// live chrome. It is published once a second poll agrees. Mid-redraw
    /// frames are otherwise indistinguishable from real transitions, and they
    /// flipped parked sessions between Idle and Running every few seconds.
    pub pending: Option<Status>,
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

    /// Server-owned follow-ups, ordered by `QueuedPromptEntry::seq`. Persisted
    /// here so the daemon can drain them without a connected client.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<crate::acp::state::QueuedPromptEntry>,

    /// Monotonic counter for `QueuedPromptEntry::seq`, so ordering is stable
    /// even after rows drain or are removed. Never reused within a session.
    #[cfg(feature = "serve")]
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub queued_prompt_next_seq: u64,

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
    /// Optional model id, injected at spawn as `AOE_AGENT_MODEL` (e.g.,
    /// "claude-opus-4-7", "gpt-5", "llama3.3:ollama"). Only `aoe-agent`
    /// reads that variable.
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

    /// What the last detection left behind, for the next one to read. See
    /// [`DetectionState`]: every poller decides on a copy of this row, so
    /// each one has to carry this field back or the confirming poll never
    /// arrives (#3642).
    #[serde(skip)]
    pub detection: DetectionState,

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

    /// Set when this pane's launch line carried the Pi session-id extension.
    /// Runtime only: after an AoE restart the pane is still running with it,
    /// and the sidecar it wrote is what says so (see
    /// `uses_pi_session_sidecar`).
    #[serde(skip)]
    pi_extension_launched: bool,

    /// Memo for `declares_agent_config_dir`. Resolving the profile config
    /// reads several files, and the answer gates the Pi sidecar, which sits on
    /// the per-refresh path. Runtime only, so an edit to `agent_config_dir`
    /// lands on the next reload rather than mid-life of a session object.
    #[serde(skip)]
    agent_config_dir_declared: std::sync::OnceLock<bool>,

    /// Absolute transcript path this Pi pane last published. Pi indexes
    /// sessions by their starting cwd, so this is what resumes a conversation
    /// whose managed worktree has since moved; the id alone would resolve to
    /// nothing there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_session_path: Option<String>,

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
