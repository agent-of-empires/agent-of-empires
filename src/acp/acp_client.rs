//! ACP client wrapper.
//!
//! aoe is the *client* in ACP terms; the agent (claude-code, aoe-agent,
//! gemini, etc.) is the *server*. The client sends `initialize`,
//! `session/new`, `session/prompt` and handles incoming `session/update`
//! notifications and `session/request_permission` requests.
//!
//! Architecture: spawn the agent subprocess, build a `ByteStreams`
//! transport over its stdio, run `Client.builder().connect_with(...)` on
//! a background tokio task. The task drives a long-lived loop:
//! initialize once, create one ACP session, then pump commands from an
//! mpsc channel into ACP requests until shutdown.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ErrorCode;
use agent_client_protocol::schema::{
    AudioContent, BlobResourceContents, CancelNotification, ClientCapabilities, ContentBlock,
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, ElicitationAction, ElicitationCapabilities,
    ElicitationFormCapabilities, EmbeddedResource, EmbeddedResourceResource,
    FileSystemCapabilities, ImageContent, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, McpServer, MessageId, NewSessionRequest,
    PermissionOptionKind, PromptRequest, ProtocolVersion, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigValueId, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest,
    StopReason, TerminalId, TerminalOutputRequest, TerminalOutputResponse, TextContent,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectionTo, JsonRpcRequest, JsonRpcResponse, Responder,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, info, trace, warn, Instrument};

use super::agent_compat::{self, ExpectedAgent};
use super::agent_profiles;
use super::agent_registry::AgentSpec;
use super::approvals::{is_destructive, ApprovalDecision, Nonce};
use super::elicitations::{
    build_response, parse_elicitation, summarize_answers, Elicitation, ElicitationAnswer,
    ElicitationOutcome, ElicitationResolution,
};
use super::event_store::AttachmentBlob;
use super::fs_handler::{self, FsPolicy, SandboxPathMap};
use super::mcp_config;
use super::permissions::build_approval;
use super::state::{
    AcpSessionId, AvailableCommand, ConfigOptionCategory, ConfigOptionChoice,
    ConfigOptionDescriptor, DiffPreview, Event, MemoryRecall, ModeInfo, Plan, PlanStep,
    PlanStepStatus, PromptAttachmentKind, RateLimitInfo, SessionMode, SessionUsage,
    StartupErrorDetail, ToolCall, ToolOutputBlock, UsageCost,
};
use super::terminal_handler::TerminalManager;
use crate::session::SandboxInfo;

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// The session's working directory does not exist on disk. Distinct
    /// from a generic spawn ENOENT (which on POSIX is indistinguishable
    /// at the libc level between missing binary, missing interpreter, and
    /// missing cwd). Surfaced as its own variant so the UI can render a
    /// targeted remediation banner instead of the default "install the
    /// adapter" copy. See issue #1089.
    #[error("project path no longer exists: {path}")]
    ProjectPathMissing { path: PathBuf },
    /// The ACP `initialize` handshake completed but the adapter failed
    /// the per-adapter compatibility policy (see
    /// `src/acp/agent_compat.rs`). Carries the structured detail so
    /// the supervisor can publish a matching `Event::IncompatibleAgent`
    /// through the broadcast sink (the in-process event_tx the failed
    /// `AcpClient::spawn` opened is never delivered, so the structured
    /// payload has to ride out of band on the typed error). The payload
    /// is boxed to keep `AcpError` small on the Ok hot path (clippy's
    /// `result_large_err`).
    #[error("incompatible agent: {0}")]
    IncompatibleAgent(Box<IncompatibleAgentError>),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("agent process exited unexpectedly")]
    AgentExited,
    #[error("client task is not running")]
    NotRunning,
    #[error("no pending approval with that nonce")]
    UnknownNonce,
    #[error("agent did not offer a {0:?} option")]
    NoMatchingOption(ApprovalDecision),
    /// A submitted elicitation answer failed server-side validation. The
    /// pending elicitation is left intact so the client can correct the
    /// answer and resubmit (rather than the question aborting). See #2100.
    #[error("submitted answer is invalid: {0}")]
    InvalidAnswer(String),
}

/// Boxed payload for `AcpError::IncompatibleAgent`. Carries the
/// structured `StartupErrorDetail` plus a pre-formatted free-form
/// summary the supervisor mirrors into the legacy
/// `Event::AgentStartupError { message }` channel for status-derivation
/// callers that don't yet read the structured detail.
#[derive(Debug)]
pub struct IncompatibleAgentError {
    pub detail: super::state::StartupErrorDetail,
    pub message: String,
}

impl std::fmt::Display for IncompatibleAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl AcpError {
    /// Inspect a `std::io::Error` returned by `Command::spawn` against
    /// the spawn site's cwd + (resolved) command. POSIX returns ENOENT
    /// for both "binary not on PATH" and "cwd does not exist", so the
    /// disambiguation has to happen via filesystem stat. Stats only on
    /// the ENOENT branch to keep the hot path free.
    ///
    /// Belt-and-suspenders for the cwd-missing case: the supervisor
    /// pre-flights `cwd.exists()` before spawning, but the directory
    /// can race-disappear between pre-flight and exec. Without this
    /// classifier the bare ENOENT bubbles up as a generic spawn error
    /// and the UI lands on the wrong remediation banner. See #1089.
    pub fn classify_spawn_error(
        err: std::io::Error,
        cwd: &std::path::Path,
        spawn_command: &str,
    ) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound && !cwd.exists() {
            return AcpError::ProjectPathMissing {
                path: cwd.to_path_buf(),
            };
        }
        AcpError::Spawn(format!("{err} (command `{spawn_command}`)"))
    }

    /// Build the enriched "binary not found" spawn error for a bare-command
    /// ENOENT (no PATH resolution, cwd present). Appends the exact install
    /// command when the binary is a known ACP adapter so the web banner can
    /// show a copyable line instead of making the user guess. See #2109.
    fn missing_binary_spawn_error(err: &std::io::Error, command: &str) -> Self {
        let hint = crate::acp::install_hints::install_hint_for(command)
            .map(|cmd| format!(". Install with: {cmd}"))
            .unwrap_or_default();
        AcpError::Spawn(format!(
            "{err} (binary `{command}` not found on the daemon's PATH or in \
             any known node-manager bin dir; install it where the daemon can \
             see it, or restart `aoe serve` from a shell where `which \
             {command}` resolves){hint}"
        ))
    }
}

/// Inspect an ACP-level error response from a `session/prompt` request
/// and return a `RateLimitInfo` if the adapter reported a quota/usage
/// limit hit. claude-agent-acp signals this via `data.errorKind ==
/// "rate_limit"` on the JSON-RPC error object. Other adapters may
/// surface the same signal differently; the catch-all message regex in
/// `classify_rate_limit_from_message` is the defensive fallback.
///
/// Reset time is recovered from `data.resets_at` (RFC3339) when
/// present. Some claude-agent-acp versions only embed the time in the
/// message text ("resets 12:10pm (Europe/Paris)"); robustly parsing
/// arbitrary locale strings would require chrono-tz, so the fallback
/// is `now + 1h` and the message is preserved verbatim in
/// `RateLimitInfo.status` so the UI can surface the exact text.
pub(crate) fn classify_rate_limit_error(
    err: &agent_client_protocol::Error,
) -> Option<RateLimitInfo> {
    let data = err.data.as_ref()?;
    let kind = data.get("errorKind").and_then(|v| v.as_str())?;
    if kind != "rate_limit" {
        return None;
    }
    let resets_at = data
        .get("resets_at")
        .or_else(|| data.get("resetsAt"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(1));
    Some(RateLimitInfo {
        status: err.message.clone(),
        resets_at,
        kind: kind.to_string(),
    })
}

/// Defensive fallback for the connection-task end path. The outer
/// error type carries no structured `data`, only a Display string, so
/// match on the fingerprint claude-agent-acp embeds in its error
/// payload. Hit rate is intentionally narrow: a substring match on
/// `errorKind":"rate_limit"` only matches the JSON the adapter pastes
/// into its error message; unrelated logs that mention "rate_limit"
/// won't trigger.
pub(crate) fn classify_rate_limit_from_message(message: &str) -> Option<RateLimitInfo> {
    if !message.contains("\"errorKind\":\"rate_limit\"")
        && !message.contains("\"errorKind\": \"rate_limit\"")
    {
        return None;
    }
    Some(RateLimitInfo {
        status: message.to_string(),
        resets_at: chrono::Utc::now() + chrono::Duration::hours(1),
        kind: "rate_limit".to_string(),
    })
}

/// Experimental `session/delete` ACP request. Adapters advertising
/// `sessionCapabilities.delete: {}` (claude-agent-acp >= 0.36) handle
/// this by releasing adapter-side state for the session (e.g. clearing
/// the persisted Claude session record on disk). Other adapters reply
/// with `-32601 method_not_found` and the supervisor falls through to
/// the existing SIGTERM path. The Rust ACP schema crate (0.12) does
/// not yet expose `SessionCapabilities.delete`, so the request type is
/// defined here against the wire format from the TypeScript SDK. See
/// #1404.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "session/delete", response = DeleteSessionResponse)]
#[serde(rename_all = "camelCase")]
struct DeleteSessionRequest {
    session_id: agent_client_protocol::schema::SessionId,
    /// Emit `_meta: {}` so adapters that validate against the strict
    /// `unstable_session_delete` schema accept the request. Optional
    /// in the TS schema, but a defensive default avoids `-32602
    /// invalid_params` from future adapter validators.
    #[serde(rename = "_meta")]
    meta: serde_json::Value,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct DeleteSessionResponse {}

/// Outcome of an experimental `session/delete` call. Every variant is
/// non-fatal; the supervisor logs and proceeds to SIGTERM regardless.
#[derive(Debug)]
pub enum DeleteSessionOutcome {
    /// Adapter accepted the request and returned a successful response.
    Deleted,
    /// Adapter returned JSON-RPC `-32601 method_not_found`. Expected on
    /// adapters that don't advertise `sessionCapabilities.delete`
    /// (`aoe-agent`, `codex`, `opencode`, older `claude-agent-acp`).
    UnsupportedMethod,
    /// The bounded wait elapsed before the adapter responded.
    TimedOut,
    /// Any other failure (non-`-32601` JSON-RPC error, transport drop,
    /// dispatch channel closed). Carries the reason for the log line.
    Failed(String),
}

/// Adapter error messages flow through here verbatim. Cap at this many
/// bytes before logging so a chatty or malicious adapter cannot bloat
/// `debug.log`. 256 leaves room for the prefix while preserving the
/// JSON-RPC error code and a useful slice of the message.
const ACP_DELETE_ERROR_MSG_MAX: usize = 256;

/// Hard cap on the wait for `session/delete`. Adapters that succeed
/// (claude-agent-acp clears a local file) complete in tens of ms; the
/// timeout protects the delete path from a wedged adapter. Not a
/// `AcpConfig` field on purpose: this is best-effort experimental
/// cleanup with no operator-visible failure mode.
const ACP_SESSION_DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Configuration for spawning an ACP agent.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Registry key of the agent (e.g. `"claude"`, `"codex"`,
    /// `"opencode"`). Used to resolve the static `AgentProfile` that
    /// gates server-side claude-specific event synthesis and routes
    /// per-agent slash commands. Defaults to `"claude"` for legacy
    /// callers; the supervisor passes the real key when it spawns.
    pub agent_key: String,
    pub spec: AgentSpec,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    /// Provider env vars to forward (after applying the agent's allowlist).
    pub provider_env: Vec<(String, String)>,
    /// Optional default reasoning effort to apply on fresh ACP sessions
    /// through the adapter's `thought_level` config option.
    pub default_effort: Option<String>,
    /// Reserved for a future agent-in-container that natively speaks
    /// the socket transport. The current structured view sandbox path runs
    /// `docker exec` from the host-side runner (which already holds the
    /// daemon↔runner socket) and proxies the agent's stdio across the
    /// container boundary, so no bind-mount is needed today.
    pub socket_path: Option<PathBuf>,
    /// ACP session id from a previous run, captured during the last
    /// `session/new` and persisted on `Instance.acp_session_id`.
    /// When `Some` and the agent advertises
    /// `agent_capabilities.load_session = true`, the connection task
    /// sends `LoadSessionRequest` instead of `NewSessionRequest`. On
    /// load failure the task falls back to `session/new` and emits a
    /// `SessionContextReset` event.
    pub stored_acp_session_id: Option<String>,
    /// When `Some`, the agent runs inside the named Docker container.
    /// Daemon-side spawn wraps the argv in `docker exec` and the
    /// fs/terminal handlers route across the container boundary using
    /// the container_workdir / mount map.
    pub sandbox_info: Option<SandboxInfo>,
    /// Source profile of the session. Used together with `sandbox_info`
    /// to resolve profile-level `sandbox.environment` entries so the
    /// structured view sandbox env mirrors the tmux view. `None` for
    /// non-sandboxed sessions.
    pub source_profile: Option<String>,
    /// MCP servers to forward to the agent on `session/new` and
    /// `session/load`, resolved from the global `<app_dir>/mcp.json` by the
    /// supervisor. Capability gating (dropping `http`/`sse` the agent did not
    /// advertise) happens later, against the `initialize` response. Empty when
    /// no config file exists, which preserves pre-feature behavior.
    pub mcp_servers: Vec<McpServer>,
    /// When true and this spawn resumes via `session/load`, seed the event
    /// store from the agent's history replay instead of suppressing it.
    /// Set for the first spawn of an imported Claude session whose store is
    /// empty; false for normal reattach (the transcript is already stored,
    /// so re-ingesting would duplicate-key panic). See #2276.
    pub seed_history_replay: bool,
}

/// Commands sent from `AcpClient` methods to the background connection task.
enum ClientCmd {
    /// The fully-built prompt content blocks (text first, then any
    /// attachments). Built in `send_prompt` so the connection task just
    /// forwards them to `session/prompt`. See #1000.
    Prompt(Vec<ContentBlock>),
    Cancel,
    /// Force-stop now: end the in-flight turn with `user_forced` and let
    /// the drain task kill the worker process group + respawn. See #1727.
    ForceStop,
    SetMode(String),
    /// Send `session/set_config_option` for the given (`config_id`,
    /// `value`) pair. The connection task fires the request detached so
    /// the cmd_rx loop keeps polling for Cancel during the round-trip.
    /// See #1403.
    SetConfigOption {
        config_id: String,
        value: String,
    },
    /// Send the experimental `session/delete` RPC for the given ACP
    /// session id and report the outcome via `respond_to`. Issued by
    /// the supervisor before the existing shutdown path during structured view
    /// session deletion. See #1404.
    DeleteSession {
        acp_session_id: String,
        respond_to: oneshot::Sender<DeleteSessionOutcome>,
    },
    Shutdown,
}

/// How the connection task should handle the ACP handshake against the
/// agent.
///
/// `Fresh` is the standard path: send `initialize`, then either
/// `session/load` (if the agent advertises support AND we have a stored
/// id) or `session/new`.
///
/// `Resume` is used by `AcpClient::attach` on `aoe serve` restart, when
/// the per-session `aoe __acp-runner` shim kept the agent process
/// alive across the daemon's death. The agent is already initialized
/// and the session is already in its in-memory map; re-sending
/// `session/new` would split context onto a new session id (which the
/// in-flight turn does not address), and re-sending `session/load`
/// against an agent that advertises `loadSession: false` (e.g. the
/// bundled `aoe-agent`) would fall through to `session/new` with the
/// same split-context bug. In `Resume` mode the daemon still sends
/// `initialize` (idempotent for capabilities, cheap, lets us learn the
/// agent's caps) but skips both `session/new` and `session/load` and
/// uses the supplied `acp_session_id` as-is. `in_flight_turn` arms the
/// resume-idle watchdog described in `run_connection_task`.
#[derive(Debug, Clone)]
enum ConnectMode {
    Fresh {
        stored_acp_session_id: Option<String>,
        /// Seed the event store from the `session/load` history replay
        /// instead of suppressing it (imported session, empty store). See
        /// #2276.
        seed_history_replay: bool,
    },
    Resume {
        acp_session_id: String,
        in_flight_turn: bool,
    },
}

/// Time after a `Resume`-mode attach with `in_flight_turn = true`,
/// during which the runner forwards NO inbound notification, before the
/// watchdog synthesizes a `Stopped { reason: "reattach_idle" }` event.
/// The watchdog disarms permanently on the first inbound notification
/// (see `first_event_after_attach`): once the runner forwards anything,
/// the turn is observable and later silence is normal mid-turn
/// reasoning, not an orphan. So this grace only bounds the fully-silent
/// reattach case (the orphaned `session/prompt` response was lost and
/// no notification ever arrives). 30s leaves headroom for a slow first
/// post-attach event (model reasoning before its first chunk) while
/// still clearing a truly-dead reattach quickly. See #1216.
const RESUME_IDLE_GRACE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(30);

/// Grace window between the first `session/cancel` notification (sent
/// during an in-flight `session/prompt`) and the daemon declaring the
/// agent unresponsive. When this fires, the connection task ends with
/// `Stopped { reason: "agent_unresponsive" }` and the supervisor
/// SIGTERMs the runner before respawning via `session/load`. 10s is
/// long enough for claude-agent-acp to resolve a real cancel through
/// the SDK message boundary but short enough that a user who clicked
/// "Force end turn" isn't watching a frozen UI for 30s while the
/// daemon waits.
///
/// claude-agent-acp >=0.37.0 (upstream #694) now resolves cancel by
/// returning `PromptResponse { stop_reason: StopReason::Cancelled }`
/// promptly; in that path the watchdog never fires and the terminal
/// Stopped reason is `cancelled` (set by `prompt_cancelled` in the
/// prompt loop) instead of `agent_unresponsive`. The 10s watchdog
/// stays as a transport-wedge defense: native cancel only protects
/// against the adapter ignoring the signal, not against socket /
/// stdout / process-level wedges that prevent the PromptResponse from
/// reaching the daemon at all. See #1196.
///
/// claude-agent-acp >=0.41.0 (upstream #680) also force-resolves a
/// prompt loop wedged in a `TaskOutput { block: true }` poll against a
/// hung background task: ~30s after the first cancel it returns
/// `cancelled` instead of hanging forever. The floor (see
/// `agent_compat`) guarantees that path, so a cancel during off-protocol
/// background work no longer rides the 30-minute
/// `OFF_PROTOCOL_WORK_GRACE_FLOOR` below before recovering.
pub(crate) const CANCEL_ESCALATION_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Vendor-agnostic silent-orphan grace fallback used when no config
/// value is available. Mirrors `AcpConfig::silent_orphan_grace_secs`
/// default. Bumped from 60s to 120s in #1360 so async-agent flows
/// (Claude SDK `Agent` tool with `isAsync: true`) survive normal sub-
/// agent wait windows. See `silent_orphan_grace()`.
const SILENT_ORPHAN_GRACE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(120);

/// Minimum effective grace applied when the prompt loop has observed
/// off-protocol work in the current turn: an async-agent launch
/// (`OffProtocolWorkKind::AsyncAgent`, see #1360) or a backgrounded
/// Bash launch (`OffProtocolWorkKind::BackgroundCommand`, see #1401).
/// The watchdog stays armed but uses this as a floor against the
/// configured base grace, so an operator who deliberately set a higher
/// `silent_orphan_grace_secs` still wins. Finite by design: if claude-
/// agent-acp hangs DURING the off-protocol wait with no cancel sent, the
/// watchdog still recovers after 30 minutes rather than holding the turn
/// open forever. When a cancel IS sent, claude-agent-acp >=0.41.0
/// (upstream #680) force-resolves the wedge in ~30s, so this 30-minute
/// floor only governs the un-cancelled quiet-wait case.
/// See #1360, #1401, and upstream `agentclientprotocol/claude-agent-acp#336`.
const OFF_PROTOCOL_WORK_GRACE_FLOOR: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Accelerated silent-orphan grace fallback used when a cost-populated
/// `UsageUpdate` notification has arrived for the current prompt. The
/// daemon treats that frame as claude-agent-acp's "wrap up accounting"
/// terminal-candidate marker emitted just before `PromptResponse`;
/// when the prompt response then fails to arrive, recovery doesn't
/// need the full vendor-agnostic grace. Mirrors
/// `AcpConfig::silent_orphan_fast_grace_secs` default. See
/// `silent_orphan_fast_grace()`.
const SILENT_ORPHAN_FAST_GRACE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(20);

/// Cadence at which the silent-orphan select arm wakes up to evaluate
/// whether the watchdog should fire. Polling cadence rather than reset-
/// on-signal so the prompt loop owns the timer without needing the
/// notification handler to reach back into a pinned `tokio::time::sleep`.
const SILENT_ORPHAN_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Idle grace for the between-prompt watchdog once the cost-bearing
/// end-of-turn `UsageUpdate` has arrived. Much shorter than the per-prompt
/// `SILENT_ORPHAN_FAST_GRACE_DEFAULT`: that one also waits out a
/// possibly-late `PromptResponse` over the wire, but a between-prompt
/// agent-initiated turn has no RPC to wait for, so once it emits its
/// end-of-turn marker and goes quiet a few seconds is enough. Kept low so
/// the "monitoring" badge and running status clear promptly after a monitor
/// turn finishes. A turn that actually continues emits fresh progress,
/// which resets the idle timer and clears `cost_seen`, so this cannot cut a
/// live turn short. See #2325.
const BETWEEN_PROMPT_IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Tick cadence for the between-prompt idle check. Faster than
/// `SILENT_ORPHAN_CHECK_INTERVAL` so the badge and status clear within a few
/// seconds of the turn ending. Only polled while the command loop is parked
/// between prompts, so the extra wakeups are cheap. See #2325.
const BETWEEN_PROMPT_IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn opencode_data_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("opencode"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("opencode"),
    )
}

fn opencode_db_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("OPENCODE_DB") {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == ":memory:" {
            return None;
        }
        let path = PathBuf::from(trimmed);
        if path.is_absolute() {
            return Some(path);
        }
        return opencode_data_dir().map(|dir| dir.join(path));
    }

    let data_dir = opencode_data_dir()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(&data_dir).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_candidate =
            name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"));
        if !is_candidate {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if best
            .as_ref()
            .map(|(best_mtime, _)| modified > *best_mtime)
            .unwrap_or(true)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
        .or_else(|| Some(data_dir.join("opencode.db")))
}

fn recover_opencode_prompt_error_from_sqlite_at(
    db_path: &Path,
    acp_session_id: &str,
    prompt_started_at_ms: i64,
) -> Option<String> {
    use rusqlite::{Connection, OpenFlags};

    if !db_path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(100));
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(data, '$.error.data.message')
             FROM message
             WHERE session_id = ?1
               AND json_extract(data, '$.role') = 'assistant'
               AND CAST(json_extract(data, '$.time.created') AS INTEGER) >= ?2
               AND json_extract(data, '$.error.data.message') IS NOT NULL
             ORDER BY CAST(json_extract(data, '$.time.created') AS INTEGER) DESC
             LIMIT 1",
        )
        .ok()?;
    let message: String = stmt
        .query_row(
            rusqlite::params![acp_session_id, prompt_started_at_ms],
            |row| row.get(0),
        )
        .ok()?;
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn recover_opencode_prompt_error(
    acp_session_id: &str,
    prompt_started_at_ms: i64,
) -> Option<String> {
    let db_path = opencode_db_path()?;
    recover_opencode_prompt_error_from_sqlite_at(&db_path, acp_session_id, prompt_started_at_ms)
}

/// Classification of an inbound ACP `SessionUpdate` for the silent-
/// orphan watchdog state machine. Sent from the notification handler
/// to the prompt loop via a dedicated mpsc; the prompt loop owns the
/// `Instant`-based timers and the tool-id map, so the handler doesn't
/// need to touch shared atomics on the hot path.
#[derive(Debug, Clone)]
pub(crate) enum LifecycleSignal {
    /// Transcript-producing event that resets the silent-orphan timer:
    /// `AgentMessageChunk`, `AgentThoughtChunk`, `Plan`, or a
    /// non-terminal `ToolCallUpdate` other than `InProgress`.
    Progress,
    /// A new tool call has started (`SessionUpdate::ToolCall`) or
    /// transitioned to `InProgress` (`ToolCallUpdate`). Added to the
    /// prompt-loop's `tool_calls_in_flight` map; while non-empty, the
    /// watchdog stays suppressed so long-running tools (npm install,
    /// Playwright runs, Task subagents) never false-positive.
    /// `is_background_task` carries the Claude SDK `run_in_background`
    /// flag from `raw_input` when available; the prompt loop uses it to
    /// flip `off_protocol_work_seen` at completion time even if the
    /// completion content marker is stripped or reshaped. See #1401.
    ToolStarted {
        id: String,
        is_background_task: bool,
    },
    /// A tool call reached terminal status (`Completed` or `Failed`).
    /// Removed from `tool_calls_in_flight`; when the map drains to
    /// empty after at least one progress event, the watchdog arms.
    /// `off_protocol_work` is `Some(_)` when the completion content
    /// text carries one of the Claude SDK markers detected by
    /// `detect_off_protocol_work_completed`. The matching `ToolStarted`'s
    /// `is_background_task` flag is only honored when `succeeded == true`
    /// (the prompt loop branches on this in `apply_signal`); a failed
    /// background launch must not pin the watchdog open for 30 minutes.
    /// See #1360, #1401, and upstream
    /// `agentclientprotocol/claude-agent-acp#336`.
    ToolCompleted {
        id: String,
        succeeded: bool,
        off_protocol_work: Option<OffProtocolWorkKind>,
    },
    /// Cost-populated `UsageUpdate`: claude-agent-acp's "wrap up
    /// accounting" marker. Switches the effective grace from the
    /// vendor-agnostic default to the accelerated value for this
    /// prompt only. Does NOT count as progress (it's accounting
    /// telemetry, not lifecycle), so the silent-orphan timer keeps
    /// running from the previous progress event.
    TerminalUsage,
    /// The Claude SDK `ScheduleWakeup` tool registered an absolute wake
    /// timestamp. Suppresses the watchdog until `at + base_grace`,
    /// converted to a monotonic `Instant` deadline at signal receipt so
    /// wall-clock jumps don't perturb the suppression. After the
    /// deadline the watchdog rearms with its normal grace. See #1401.
    WakeupPending { at: chrono::DateTime<chrono::Utc> },
}

/// Classify a `SessionUpdate` into a `LifecycleSignal`, or `None` for
/// ambient state (mode changes, available_commands, raw metadata,
/// usage-without-cost) that shouldn't influence the silent-orphan
/// watchdog timer. Out-of-band notifications must NOT reset the timer:
/// claude-agent-acp can interleave mode and command refreshes mid-turn
/// or after final accounting, and treating those as progress would
/// mask the exact wedge the watchdog is designed to detect. See #1240.
fn classify_lifecycle_signal(
    update: &agent_client_protocol::schema::SessionUpdate,
) -> Option<LifecycleSignal> {
    use agent_client_protocol::schema::{SessionUpdate, ToolCallStatus};
    match update {
        SessionUpdate::UsageUpdate(u) if u.cost.is_some() => Some(LifecycleSignal::TerminalUsage),
        SessionUpdate::AgentMessageChunk(_)
        | SessionUpdate::AgentThoughtChunk(_)
        | SessionUpdate::Plan(_) => Some(LifecycleSignal::Progress),
        SessionUpdate::ToolCall(tc) => {
            let is_background_task = tc
                .raw_input
                .as_ref()
                .and_then(|v| v.as_object())
                .and_then(|obj| obj.get("run_in_background"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(LifecycleSignal::ToolStarted {
                id: tc.tool_call_id.0.to_string(),
                is_background_task,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            match update.fields.status {
                Some(ToolCallStatus::Completed) => {
                    // Only successful completions can mark a real off-protocol
                    // work launch. A `Failed` update may carry arbitrary error
                    // content that happens to mention one of the SDK markers
                    // (e.g. as part of a stack trace or echoed input), and
                    // treating that as off-protocol work would pin the watchdog
                    // suppression for 30 minutes even though no background work
                    // is actually running. See CodeRabbit feedback on PR #1364.
                    let off_protocol_work =
                        detect_off_protocol_work_completed(&update.fields.content);
                    Some(LifecycleSignal::ToolCompleted {
                        id,
                        succeeded: true,
                        off_protocol_work,
                    })
                }
                Some(ToolCallStatus::Failed) => Some(LifecycleSignal::ToolCompleted {
                    id,
                    succeeded: false,
                    off_protocol_work: None,
                }),
                Some(ToolCallStatus::InProgress) => Some(LifecycleSignal::ToolStarted {
                    id,
                    // `InProgress` updates never carry the original
                    // `raw_input` so we cannot re-derive the flag here.
                    // `apply_signal` ORs this with any existing
                    // metadata so a later `InProgress` cannot
                    // overwrite a `true` from the original `ToolCall`.
                    is_background_task: false,
                }),
                _ => Some(LifecycleSignal::Progress),
            }
        }
        _ => None,
    }
}

/// Kind of off-protocol work the daemon has observed during the current
/// prompt. Both variants flip the silent-orphan watchdog to its
/// `OFF_PROTOCOL_WORK_GRACE_FLOOR` window so a legitimately quiet turn
/// doesn't get cancelled. See #1360 (`AsyncAgent`) and #1401
/// (`BackgroundCommand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffProtocolWorkKind {
    /// Claude SDK `Agent` tool with `isAsync: true`. Sub-agent runs inside
    /// the claude binary, polled via an internal SDK channel that is
    /// invisible at the ACP layer.
    AsyncAgent,
    /// Claude SDK `Bash` tool with `run_in_background: true`. The visible
    /// `ToolCall` completes immediately while the underlying subprocess
    /// keeps running off-protocol; the agent polls later via `BashOutput`.
    BackgroundCommand,
    /// Claude SDK `ScheduleWakeup` tool: the agent deliberately parks the
    /// turn until a future wake time (a monitor or `/loop` run). The turn
    /// stays in-flight while the agent is intentionally idle, so the
    /// silent-orphan watchdog must not treat the quiet window as a wedge.
    /// Like `AsyncAgent` (and unlike `BackgroundCommand`) this survives a
    /// `TerminalUsage` marker, because a scheduled wake legitimately
    /// outlasts the turn's final accounting frame. See #1360, #1401, and
    /// the monitor-killed-by-watchdog regression.
    ScheduledWakeup,
}

/// Per-tool metadata stored in the silent-orphan watchdog's
/// `tool_calls_in_flight` map. Lets the watchdog remember the original
/// `run_in_background` flag observed at `ToolStarted` time so the
/// completion path can flip `off_protocol_work_seen` even if the
/// completion content marker is missing or reshaped. See #1401.
#[derive(Debug, Clone, Copy)]
struct ToolMetadata {
    is_background_task: bool,
}

/// Runtime configuration handed to `SilentOrphanWatchdog::should_fire`
/// and `apply_signal`. Decoupled from `AcpConfig` and the env-var
/// overrides so unit tests can drive synthetic graces deterministically
/// without touching process-global state.
#[derive(Debug, Clone, Copy)]
struct SilentOrphanWatchdogConfig {
    base_grace: std::time::Duration,
    fast_grace: std::time::Duration,
    off_protocol_grace_floor: std::time::Duration,
}

/// Pure state machine for the silent-orphan watchdog. The prompt loop
/// owns one instance per prompt; the `apply_signal` method consumes
/// `LifecycleSignal`s from the notification handler and `should_fire`
/// returns the firing predicate on each polling tick.
///
/// All wall-clock and monotonic time inputs are injected by the caller
/// (`now: Instant`, `wall_now: DateTime<Utc>`) so the unit tests can
/// step the clock forward synthetically. The struct never reaches into
/// `Instant::now()` or `chrono::Utc::now()` directly.
///
/// Invariants:
///
/// - `tool_calls_in_flight` non-empty → watchdog is always suppressed.
/// - `off_protocol_work_seen.is_some()` → effective grace lifts to at
///   least `off_protocol_grace_floor` for the rest of this prompt.
/// - `wakeup_suppress_until.is_some()` and `now < deadline` →
///   suppressed regardless of grace.
/// - `cost_seen` switches the no-off-protocol case to fast grace; any
///   subsequent `Progress` / `ToolStarted` / `ToolCompleted` /
///   `WakeupPending` clears it.
///
/// See #1240 (original wedge), #1360 (async-agent floor), and #1401
/// (backgrounded Bash + ScheduleWakeup).
#[derive(Debug, Default)]
struct SilentOrphanWatchdog {
    saw_first_progress: bool,
    last_progress_at: Option<tokio::time::Instant>,
    cost_seen: bool,
    tool_calls_in_flight: std::collections::HashMap<String, ToolMetadata>,
    off_protocol_work_seen: Option<OffProtocolWorkKind>,
    wakeup_suppress_until: Option<tokio::time::Instant>,
}

impl SilentOrphanWatchdog {
    fn new() -> Self {
        Self::default()
    }

    /// Fold a lifecycle signal into the state machine. Called once per
    /// signal received from the notification handler's mpsc.
    fn apply_signal(
        &mut self,
        sig: LifecycleSignal,
        now: tokio::time::Instant,
        wall_now: chrono::DateTime<chrono::Utc>,
        cfg: SilentOrphanWatchdogConfig,
    ) {
        match sig {
            LifecycleSignal::Progress => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
            }
            LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            } => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                // OR the new flag with any existing metadata. A late
                // `ToolCallUpdate(InProgress)` lacks `raw_input` and
                // classifies as `is_background_task = false`; without
                // the OR it would erase the `true` captured from the
                // original `ToolCall` and break the raw-input arm of
                // the defense-in-depth detection. See #1401 review.
                self.tool_calls_in_flight
                    .entry(id)
                    .and_modify(|m| m.is_background_task |= is_background_task)
                    .or_insert(ToolMetadata { is_background_task });
            }
            LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            } => {
                let started_as_background = self
                    .tool_calls_in_flight
                    .remove(&id)
                    .map(|m| m.is_background_task)
                    .unwrap_or(false);
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                // Defense in depth: trust either the completion-content
                // marker OR the original raw_input flag. Either path
                // alone is enough to mark this prompt as having
                // off-protocol work pending. The raw-input fallback is
                // ONLY trusted on successful completion: a Failed
                // backgrounded Bash means the subprocess never
                // actually started, so suppressing for 30 minutes
                // would create a fresh false-positive class. See
                // #1401 and the post-impl review notes.
                let kind = off_protocol_work.or({
                    if succeeded && started_as_background {
                        Some(OffProtocolWorkKind::BackgroundCommand)
                    } else {
                        None
                    }
                });
                if let Some(kind) = kind {
                    self.off_protocol_work_seen = Some(kind);
                }
            }
            LifecycleSignal::TerminalUsage => {
                self.cost_seen = true;
                // A cost-resolved UsageUpdate is the end-of-turn marker
                // (mid-turn usages carry `cost: null`, see #1360). A
                // backgrounded command is fire-and-forget: the agent
                // launches it and moves on, so it legitimately outlives
                // the turn and its suppression is moot once the turn
                // ends. Drop it here, otherwise `effective_grace` keeps
                // the 30-minute floor and a turn that streamed its final
                // usage but never returned the PromptResponse hangs for
                // half an hour instead of recovering on the fast grace
                // (#1858). Self-correcting: if the turn somehow
                // continues, the next Progress / ToolStarted /
                // ToolCompleted clears `cost_seen` and the next
                // backgrounded tool re-arms suppression. An AsyncAgent
                // await or a ScheduledWakeup blocks the turn (the agent
                // idles waiting and resumes in-band), so their floor is
                // left intact to preserve the #1360 fix and the monitor
                // fix; only the fire-and-forget BackgroundCommand drops.
                if self.off_protocol_work_seen == Some(OffProtocolWorkKind::BackgroundCommand) {
                    self.off_protocol_work_seen = None;
                }
            }
            LifecycleSignal::WakeupPending { at } => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                // A scheduled wake is deliberate off-protocol idling, not
                // a wedge: mark the turn so the fast grace (cost_seen)
                // never applies and the post-`at` grace is the generous
                // 30-minute off-protocol floor. Overwrite any prior kind
                // so a later `TerminalUsage` cannot clear it (only
                // `BackgroundCommand` is dropped there). Without this a
                // monitor / `/loop` turn that emitted a cost-bearing
                // `UsageUpdate` was killed ~20s after the wake window
                // lapsed even though the agent intended to keep going.
                self.off_protocol_work_seen = Some(OffProtocolWorkKind::ScheduledWakeup);
                // Convert the wall-clock `at` to a monotonic `Instant`
                // deadline now, so wall-clock jumps between signal
                // receipt and the next firing check can't perturb
                // suppression. Add the off-protocol floor as a tail so
                // the watchdog doesn't snap-fire the instant the sleep
                // ends; the agent needs room after `at` to emit the
                // wake's first progress, and a monitor whose wake `at`
                // is itself further out than the floor stays suppressed
                // the whole time. See #1401 and the monitor regression.
                let until_wakeup = at
                    .signed_duration_since(wall_now)
                    .to_std()
                    .unwrap_or(std::time::Duration::ZERO);
                let deadline = now + until_wakeup + cfg.off_protocol_grace_floor;
                // Multiple wakeups should EXTEND (not shorten)
                // suppression. The agent may re-issue a longer
                // ScheduleWakeup mid-turn; only the later deadline
                // wins.
                self.wakeup_suppress_until = Some(
                    self.wakeup_suppress_until
                        .map_or(deadline, |existing| existing.max(deadline)),
                );
            }
        }
    }

    fn effective_grace(&self, cfg: SilentOrphanWatchdogConfig) -> std::time::Duration {
        if self.off_protocol_work_seen.is_some() {
            cfg.base_grace.max(cfg.off_protocol_grace_floor)
        } else if self.cost_seen && cfg.fast_grace > std::time::Duration::ZERO {
            cfg.fast_grace
        } else {
            cfg.base_grace
        }
    }

    /// Returns `true` iff the watchdog must fire now. Also clears any
    /// expired `wakeup_suppress_until` deadline as a side effect so
    /// subsequent ticks don't re-evaluate stale state.
    fn should_fire(&mut self, now: tokio::time::Instant, cfg: SilentOrphanWatchdogConfig) -> bool {
        if self
            .wakeup_suppress_until
            .is_some_and(|deadline| now >= deadline)
        {
            self.wakeup_suppress_until = None;
        }
        let wakeup_suppressed = self
            .wakeup_suppress_until
            .is_some_and(|deadline| now < deadline);
        let elapsed = self.last_progress_at.map(|t| now.duration_since(t));
        self.saw_first_progress
            && self.tool_calls_in_flight.is_empty()
            && !wakeup_suppressed
            && elapsed
                .map(|d| d >= self.effective_grace(cfg))
                .unwrap_or(false)
    }

    fn tool_calls_in_flight_len(&self) -> usize {
        self.tool_calls_in_flight.len()
    }

    fn off_protocol_work_seen(&self) -> Option<OffProtocolWorkKind> {
        self.off_protocol_work_seen
    }

    /// True once a cost-populated `UsageUpdate` (the end-of-turn
    /// accounting marker) has arrived and nothing has reset progress
    /// since. At watchdog-fire time this means the turn demonstrably
    /// wrapped up but the adapter never sent the JSON-RPC PromptResponse,
    /// so the right recovery is a clean `prompt_complete`, not a
    /// cancel-and-restart orphan. See #2237.
    fn cost_seen(&self) -> bool {
        self.cost_seen
    }

    fn saw_progress(&self) -> bool {
        self.saw_first_progress
    }
}

/// Resolve the terminal `Stopped` reason for a prompt turn from the
/// mutually-prioritised end-of-turn flags. Extracted as a pure function
/// so the precedence is unit-testable without the connection loop.
///
/// Precedence (highest first) and why each wins where it does is
/// documented inline at the single call site. The finished-but-unacked
/// recovery (#2237) deliberately sets NONE of these flags and breaks the
/// loop, so it falls through to `prompt_complete`: the turn finished, the
/// adapter just never sent the PromptResponse, so it must NOT collapse
/// into `prompt_orphaned` (which would trigger a worker restart). Its
/// post-cancel sibling `finished_after_orphan_cancel` (#2370) covers the
/// case where the cost marker arrives only AFTER the grace already expired
/// and the orphan cancel fired: the cancel was premature, so the turn is
/// demoted back to `prompt_complete` unless the adapter is still wedged on
/// the RPC (`agent_unresponsive` / `shutdown`), which keeps the restart.
fn terminal_stop_reason(
    rate_limited: bool,
    force_stopped: bool,
    prompt_orphaned: bool,
    agent_unresponsive: bool,
    shutdown: bool,
    prompt_cancelled: bool,
    finished_after_orphan_cancel: bool,
) -> &'static str {
    if rate_limited {
        "rate_limited"
    } else if force_stopped {
        "user_forced"
    } else if finished_after_orphan_cancel && !agent_unresponsive && !shutdown {
        // A cost-populated UsageUpdate that lands after a timer-driven orphan
        // cancel proves the turn actually finished; the cancel was premature.
        // End cleanly as prompt_complete (no worker restart, no "didn't notify
        // the daemon" banner), the post-cancel sibling of the pre-grace #2237
        // recovery. The cancel makes the adapter resolve as
        // StopReason::Cancelled, so this must outrank both prompt_orphaned and
        // prompt_cancelled. A genuinely RPC-wedged adapter (cancel-escalation
        // grace fired, so agent_unresponsive / shutdown) is excluded here and
        // still restarts the worker, since the connection may be unusable for
        // the next prompt. See #2370.
        "prompt_complete"
    } else if prompt_orphaned {
        "prompt_orphaned"
    } else if agent_unresponsive {
        "agent_unresponsive"
    } else if shutdown {
        "shutdown"
    } else if prompt_cancelled {
        "cancelled"
    } else {
        "prompt_complete"
    }
}

/// Decide whether the between-prompt idle watchdog should synthesize a
/// terminal `Stopped` for an agent-initiated turn that ran with no
/// aoe-issued `session/prompt`. A claude-code Monitor (or any backgrounded
/// task) can fire AFTER the prompt that armed it already completed,
/// resuming the agent into a fresh turn the per-prompt watchdog never
/// saw; without this the turn never ends and the UI stays "running"
/// forever. See #2325.
///
/// Pure so the precedence is unit-testable without the connection loop.
/// Times are wall-clock millis (`chrono::Utc::now().timestamp_millis()`),
/// matching the resume-idle watchdog. Mirrors the per-prompt watchdog's
/// grace policy: the cost-bearing `UsageUpdate` is claude-agent-acp's
/// end-of-turn marker, so once it has arrived the fast grace applies;
/// otherwise the vendor-agnostic off-protocol floor governs. A pending
/// scheduled wake (`wake_at` in the future) suppresses firing so a
/// legitimately-sleeping monitor is never killed early; once `wake_at` is
/// in the past the turn is treated as finished and self-heals fast (#2371).
/// State update the between-prompt watchdog should apply for one inbound
/// notification's classified signals. `None` when neither a lifecycle nor a
/// wakeup signal is present (ambient updates do not touch the watchdog).
///
/// Extracted as a pure function so the cost / progress / wake bookkeeping is
/// unit-testable without the notification closure. Every tracked signal
/// refreshes `last_lifecycle_at` to `now_ms`, including `TerminalUsage`: the
/// cost-bearing `UsageUpdate` is the end-of-turn marker, and the fast grace
/// must measure from it (when the turn wrapped up) rather than from a
/// possibly-stale earlier progress event. See #2325.
#[derive(Debug, PartialEq)]
struct BetweenPromptUpdate {
    cost_seen: bool,
    last_lifecycle_at: i64,
    /// Absolute wake timestamp (ms) of the most recent pending scheduled
    /// wake, `0` when none. The watchdog suppresses while `now < wake_at`
    /// (a future wake the agent is legitimately sleeping toward) and, once
    /// the wake `at` has passed with no agent resume, treats the turn as
    /// finished and fires on the fast grace instead of the 30-minute floor.
    /// Stored as the wake `at` directly (not `at + floor`) so an expired
    /// wake self-heals promptly. See #2371.
    wake_at: i64,
}

fn between_prompt_signal_update(
    lifecycle: Option<&LifecycleSignal>,
    wakeup: Option<&LifecycleSignal>,
    now_ms: i64,
    prev_wake_at: i64,
) -> Option<BetweenPromptUpdate> {
    let mut update = match lifecycle {
        Some(LifecycleSignal::TerminalUsage) => Some(BetweenPromptUpdate {
            cost_seen: true,
            last_lifecycle_at: now_ms,
            wake_at: prev_wake_at,
        }),
        Some(_) => Some(BetweenPromptUpdate {
            cost_seen: false,
            last_lifecycle_at: now_ms,
            wake_at: prev_wake_at,
        }),
        None => None,
    };
    // A scheduled wake (a re-armed monitor or /loop fallback) suppresses
    // firing until its `at`. Multiple wakes extend, never shorten,
    // suppression, so keep the latest deadline.
    if let Some(LifecycleSignal::WakeupPending { at }) = wakeup {
        update = Some(BetweenPromptUpdate {
            cost_seen: false,
            last_lifecycle_at: now_ms,
            wake_at: at.timestamp_millis().max(prev_wake_at),
        });
    }
    update
}

#[allow(clippy::too_many_arguments)]
fn between_prompt_should_fire(
    active: bool,
    now_ms: i64,
    last_lifecycle_ms: i64,
    wake_at_ms: Option<i64>,
    cost_seen: bool,
    tools_in_flight: bool,
    off_protocol_work_seen: bool,
    fast_grace: std::time::Duration,
    floor: std::time::Duration,
) -> bool {
    if !active {
        return false;
    }
    // An in-flight tool (npm install, Playwright, a Task subagent) means the
    // turn is legitimately busy; never fire while one is open. See #1401.
    if tools_in_flight {
        return false;
    }
    // A future wake is the agent legitimately sleeping toward `at`; suppress
    // until it passes so a parked /loop or monitor is not killed early.
    if wake_at_ms.is_some_and(|at| now_ms < at) {
        return false;
    }
    let expired_wake = wake_at_ms.is_some_and(|at| now_ms >= at);
    // Off-protocol work (backgrounded Bash, async sub-agent) completes on the
    // protocol while the real work keeps running, so hold the conservative
    // floor even though no tool is "in flight". See #1401, #1858. Otherwise a
    // cost-resolved end-of-turn marker OR an expired wake (the agent should
    // have resumed and did not) means the turn is done: self-heal fast.
    let grace = if off_protocol_work_seen {
        floor
    } else if cost_seen || expired_wake {
        fast_grace
    } else {
        floor
    };
    now_ms - last_lifecycle_ms >= grace.as_millis() as i64
}

/// Tagged lifecycle signal carried over the watchdog mpsc. The
/// `epoch` field is captured at signal-construction time from the
/// shared `current_prompt_epoch` atomic; the prompt loop discards
/// envelopes whose epoch doesn't match the prompt currently being
/// drained. This keeps a notification handler parked on a full
/// channel from leaking its previous-prompt signal into the next
/// prompt's watchdog state when it eventually unblocks. See #1401
/// post-impl review.
#[derive(Debug, Clone)]
pub(crate) struct LifecycleEnvelope {
    pub epoch: u64,
    pub signal: LifecycleSignal,
}

/// Deliver a lifecycle envelope from the notification handler to the
/// prompt loop with the right backpressure policy.
///
/// `Progress` uses `try_send` first to avoid blocking the notification
/// handler under streaming-chunk bursts; if the channel is full it
/// falls back to an awaited `send`. The fallback preserves correctness
/// (a dropped `Progress` after a `TerminalUsage` would leave
/// `cost_seen = true` with a stale `last_progress_at`, false-firing
/// the fast-grace path on a healthy turn).
///
/// All other lifecycle variants use an awaited `send` directly because
/// their loss can flip the watchdog into a false-positive state that
/// only the next equivalent signal would clear. See #1401 design
/// rationale.
async fn send_lifecycle_signal(
    tx: &mpsc::Sender<LifecycleEnvelope>,
    env: LifecycleEnvelope,
    session_label: &str,
) {
    match &env.signal {
        LifecycleSignal::Progress => {
            let env = match tx.try_send(env) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(env)) => env,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            };
            if tx.send(env).await.is_err() {
                trace!(
                    target: "acp.protocol",
                    session = session_label,
                    "lifecycle channel closed; dropping Progress fallback"
                );
            }
        }
        _ => {
            if tx.send(env).await.is_err() {
                trace!(
                    target: "acp.protocol",
                    session = session_label,
                    "lifecycle channel closed; dropping load-bearing signal"
                );
            }
        }
    }
}

/// Detect whether a `ToolCallUpdate` completion content array carries a
/// Claude SDK marker that the underlying work continues off-protocol after
/// the visible tool call completes. Two markers today:
///
/// - `"Async agent launched successfully"`: the `Agent` tool with
///   `isAsync: true` spawned a sub-agent polled via an internal SDK
///   channel (#1360).
/// - `"Command running in background with ID: "`: the `Bash` tool with
///   `run_in_background: true` left a subprocess running; the agent will
///   poll later via `BashOutput` / `KillShell` (#1401).
///
/// Text detection is intentionally narrow. Both prefixes are hardcoded in
/// the Anthropic Claude SDK and are the most stable identifiers available
/// short of upstream `agentclientprotocol/claude-agent-acp#336` forwarding
/// the off-protocol notifications natively. If a prefix ever changes,
/// this detector returns `None` and the watchdog falls back to the base
/// grace. For backgrounded Bash, the daemon also tracks
/// `raw_input.run_in_background == true` at `ToolStarted` time as a
/// defense in depth, so a single broken signal cannot reintroduce the
/// false-positive class this fix targets.
///
/// Match anchors at the start of a text block (or any line within it)
/// rather than substring `contains`. The SDK emits these markers as the
/// FIRST line of the completion content; user output from a regular
/// `Bash` that happens to print `Command running in background with
/// ID: ...` would otherwise trip the watchdog to its 30-minute floor.
/// See CodeRabbit review on PR #1406.
fn detect_off_protocol_work_completed(
    content: &Option<Vec<agent_client_protocol::schema::ToolCallContent>>,
) -> Option<OffProtocolWorkKind> {
    use agent_client_protocol::schema::ToolCallContent;
    let blocks = content.as_ref()?;
    for block in blocks {
        let ToolCallContent::Content(c) = block else {
            continue;
        };
        let ContentBlock::Text(t) = &c.content else {
            continue;
        };
        for line in t.text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Async agent launched successfully") {
                return Some(OffProtocolWorkKind::AsyncAgent);
            }
            if trimmed.starts_with("Command running in background with ID: ") {
                return Some(OffProtocolWorkKind::BackgroundCommand);
            }
        }
    }
    None
}

/// Monotonic counter appended to synthetic tool-call IDs so two events
/// minted within the same millisecond don't collide on the
/// `(session_id, tool_id)` keys used by the structured view event store.
static SYNTHETIC_TOOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read the resume-idle grace. In debug builds, honors
/// `AOE_RESUME_IDLE_GRACE_MS` so integration tests can short-circuit
/// the default 10s without making real failures racy. Values below
/// 100ms are clamped up so a typo can't effectively disable the
/// watchdog. Release builds always use `RESUME_IDLE_GRACE_DEFAULT`
/// so a misconfigured env var can't surface false-positive Stopped
/// events to real users.
fn resume_idle_grace() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_RESUME_IDLE_GRACE_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    RESUME_IDLE_GRACE_DEFAULT
}

/// Resolve the session's effective `AcpConfig` so per-profile
/// `silent_orphan_*` overrides set in the settings TUI actually apply
/// at runtime. Returns `None` if no config exists yet (fresh install,
/// pre-migration); the helpers fall back to constants in that case.
fn resolved_acp_config(profile: Option<&str>) -> Option<crate::session::config::AcpConfig> {
    match profile {
        Some(p) => Some(crate::session::profile_config::resolve_config_or_warn(p).acp),
        None => crate::session::load_config().ok().flatten().map(|c| c.acp),
    }
}

/// Deadline for the runner unix socket to appear after spawning the
/// `aoe __acp-runner` shim. 10s is enough in production, but a
/// debug-build cold-start under heavy CI load (v8 coverage + multiple
/// parallel `aoe serve` binaries + a runner subprocess that re-execs
/// the same debug binary) can blow past it deterministically. Honors
/// `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS` in debug builds so the
/// Playwright harness can lift it; release builds keep the original
/// 10s ceiling.
fn runner_socket_deadline() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            // Clamp to a floor of 100ms so a typo like
            // `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS=0` does not make
            // wait_for_socket fail immediately and surface as a
            // mysterious "runner socket did not appear" without ever
            // polling.
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    std::time::Duration::from_secs(10)
}

/// Test-only fault injection for the #1890 regression e2e. When
/// `AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES=N` is set, the first N *fresh*-spawn
/// ACP handshakes fail right after the runner has come up, before the daemon
/// records an in-memory worker. The runner keeps its agent alive and its
/// on-disk registry entry, so the daemon is left with a live, registered
/// runner it never adopted: the exact orphan state #1890 got permanently
/// stuck in, reproduced deterministically without depending on host timing.
/// Each call consumes one budgeted failure; `0` (the default, var unset) is a
/// no-op. Debug builds only, so release can never trip it. Mirrors the
/// `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS` debug knob above.
#[cfg(debug_assertions)]
fn take_injected_fresh_handshake_failure() -> bool {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::OnceLock;
    static REMAINING: OnceLock<AtomicI64> = OnceLock::new();
    let remaining = REMAINING.get_or_init(|| {
        let n = std::env::var("AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        AtomicI64::new(n)
    });
    remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n > 0).then_some(n - 1)
        })
        .is_ok()
}

/// Read the silent-orphan watchdog grace for the given source profile.
/// In debug builds, honors `AOE_SILENT_ORPHAN_GRACE_MS` so the
/// integration test can drive a sub-second cadence without making
/// real failures racy. Otherwise reads
/// `acp.silent_orphan_grace_secs` from the profile-resolved
/// config so per-profile overrides set in the settings TUI take
/// effect. A value of `0` means "disabled" and the caller skips the
/// watchdog entirely; non-zero values smaller than 120s clamp up at
/// runtime to the new production floor so a typo cannot produce an
/// absurdly tight grace that false-positives on healthy turns. The
/// floor was raised from 10s to 120s in #1360 alongside the default
/// bump from 60 to 120; users who explicitly want a shorter grace
/// must set `0` to disable instead.
fn silent_orphan_grace(profile: Option<&str>) -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_GRACE_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms == 0 {
                return std::time::Duration::ZERO;
            }
            return std::time::Duration::from_millis(ms);
        }
    }
    match resolved_acp_config(profile) {
        Some(acp) => {
            let secs = acp.silent_orphan_grace_secs;
            if secs == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs(u64::from(secs).max(120))
            }
        }
        None => SILENT_ORPHAN_GRACE_DEFAULT,
    }
}

/// Read the accelerated silent-orphan grace for the given source
/// profile. Same env-var override pattern as `silent_orphan_grace`;
/// reads `acp.silent_orphan_fast_grace_secs` from the profile-
/// resolved config. A value of `0` disables the accelerator: the
/// watchdog keeps using the default grace even after a cost-populated
/// `UsageUpdate` arrives. Non-zero values smaller than 5s clamp up.
fn silent_orphan_fast_grace(profile: Option<&str>) -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_FAST_GRACE_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms == 0 {
                return std::time::Duration::ZERO;
            }
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    match resolved_acp_config(profile) {
        Some(acp) => {
            let secs = acp.silent_orphan_fast_grace_secs;
            if secs == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs(u64::from(secs).max(5))
            }
        }
        None => SILENT_ORPHAN_FAST_GRACE_DEFAULT,
    }
}

/// Read the silent-orphan polling cadence. Constant in production;
/// tunable in debug builds via `AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS`
/// so the disabled-path integration test can verify the watchdog
/// stays silent without waiting a full polling tick.
fn silent_orphan_check_interval() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return std::time::Duration::from_millis(ms.max(10));
        }
    }
    SILENT_ORPHAN_CHECK_INTERVAL
}

/// Resolution channel for a parked agent->client request awaiting a user
/// decision. Stored in the pending-responders map keyed by the structured
/// view's server-generated nonce. One map carries both permission
/// approvals and form elicitations; nonces are unique across both, and
/// the resolver variant records which kind of request is parked.
struct PendingResponder {
    resolver: PendingResolver,
}

enum PendingResolver {
    /// `session/request_permission` awaiting allow/deny.
    Approval(oneshot::Sender<ApprovalResolutionMessage>),
    /// `elicitation/create` awaiting an accept/decline/cancel answer. The
    /// parsed form is kept so `resolve_elicitation` can validate the
    /// submitted answer BEFORE consuming the resolver: a validation
    /// failure then leaves the elicitation pending for a corrected
    /// resubmission instead of permanently cancelling it. The validated
    /// response (and its outcome) ride the oneshot so the parked callback
    /// just forwards them. Boxed to keep the enum small.
    Elicitation {
        elicitation: Box<Elicitation>,
        resolver: oneshot::Sender<ElicitationResolutionMessage>,
    },
}

/// Message sent over the resolver oneshot to unblock the parked
/// `on_receive_request` callback.
enum ApprovalResolutionMessage {
    Decision { decision: ApprovalDecision },
    Cancelled,
}

/// Message sent over the elicitation resolver oneshot. Carries the
/// validated wire response for the agent, the outcome for status
/// derivation, and the display-ready answers for the transcript
/// (`Event::ElicitationResolved.answers`). See #2209.
struct ElicitationResolutionMessage {
    response: CreateElicitationResponse,
    outcome: ElicitationOutcome,
    answers: Vec<ElicitationAnswer>,
}

type PendingResponders = Arc<Mutex<HashMap<Nonce, PendingResponder>>>;

type ToolContextCache = Arc<std::sync::Mutex<ToolCallContextCache>>;

const TOOL_CONTEXT_CACHE_LIMIT: usize = 256;

#[derive(Debug, Default)]
struct ToolCallContextCache {
    raw_inputs: HashMap<String, serde_json::Value>,
    insertion_order: VecDeque<String>,
}

impl ToolCallContextCache {
    fn record(&mut self, tool_call_id: String, raw_input: serde_json::Value) {
        if raw_input_has_no_user_context(&raw_input) {
            return;
        }
        if !self.raw_inputs.contains_key(&tool_call_id) {
            self.insertion_order.push_back(tool_call_id.clone());
        }
        self.raw_inputs.insert(tool_call_id, raw_input);
        self.enforce_limit();
    }

    fn get(&self, tool_call_id: &str) -> Option<serde_json::Value> {
        self.raw_inputs.get(tool_call_id).cloned()
    }

    fn remove(&mut self, tool_call_id: &str) {
        self.raw_inputs.remove(tool_call_id);
        self.insertion_order.retain(|id| id != tool_call_id);
    }

    fn enforce_limit(&mut self) {
        while self.raw_inputs.len() > TOOL_CONTEXT_CACHE_LIMIT {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            self.raw_inputs.remove(&oldest);
        }
    }
}

fn permission_raw_input_with_context(
    permission_raw: Option<&serde_json::Value>,
    cached_raw: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let cached_raw =
        cached_raw.and_then(|cached| (!raw_input_has_no_user_context(cached)).then_some(cached));
    match (permission_raw, cached_raw) {
        (Some(permission), Some(cached)) => Some(merge_permission_raw_input(permission, cached)),
        (Some(permission), None) if permission.is_null() => None,
        (Some(permission), None) => Some(permission.clone()),
        (None, Some(cached)) => Some(cached.clone()),
        (None, None) => None,
    }
}

fn merge_permission_raw_input(
    permission_raw: &serde_json::Value,
    cached_raw: &serde_json::Value,
) -> serde_json::Value {
    if let (serde_json::Value::Object(permission), serde_json::Value::Object(cached)) =
        (permission_raw, cached_raw)
    {
        let mut merged = cached.clone();
        for (key, value) in permission {
            merged.insert(key.clone(), value.clone());
        }
        return serde_json::Value::Object(merged);
    }

    if raw_input_has_no_user_context(permission_raw) {
        cached_raw.clone()
    } else {
        permission_raw.clone()
    }
}

fn raw_input_has_no_user_context(raw_input: &serde_json::Value) -> bool {
    match raw_input {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.keys().all(|key| key.starts_with("_aoe_")),
        _ => false,
    }
}

fn update_tool_context_cache(
    cache: &ToolContextCache,
    event: &Event,
    source_update: &SessionUpdate,
) {
    match event {
        Event::ToolCallStarted { tool_call } => {
            if let Some(raw_input) = raw_input_for_tool_event(source_update, &tool_call.id) {
                cache
                    .lock()
                    .expect("tool context cache mutex poisoned")
                    .record(tool_call.id.clone(), raw_input);
            }
        }
        Event::ToolCallUpdated { tool_call_id, .. } => {
            if let Some(raw_input) = raw_input_for_tool_event(source_update, tool_call_id) {
                cache
                    .lock()
                    .expect("tool context cache mutex poisoned")
                    .record(tool_call_id.clone(), raw_input);
            }
        }
        Event::ToolCallCompleted { tool_call_id, .. } => {
            cache
                .lock()
                .expect("tool context cache mutex poisoned")
                .remove(tool_call_id);
        }
        _ => {}
    }
}

fn raw_input_for_tool_event(
    source_update: &SessionUpdate,
    tool_call_id: &str,
) -> Option<serde_json::Value> {
    match source_update {
        SessionUpdate::ToolCall(tool_call)
            if tool_call.tool_call_id.0.to_string() == tool_call_id =>
        {
            tool_call.raw_input.clone()
        }
        SessionUpdate::ToolCallUpdate(update)
            if update.tool_call_id.0.to_string() == tool_call_id =>
        {
            update.fields.raw_input.clone()
        }
        _ => None,
    }
}

/// Top-level ACP client. Owns the subprocess lifetime and pumps events
/// from the connection task.
pub struct AcpClient {
    pub session_id: AcpSessionId,
    /// Inbound event receiver. Optional so the supervisor can `take()` it
    /// for the drain task, decoupling event polling from the client mutex
    /// (otherwise next_event().await would hold the mutex forever and
    /// deadlock send_prompt).
    inbound: Option<mpsc::Receiver<Event>>,
    cmd_tx: Option<mpsc::Sender<ClientCmd>>,
    pending_responders: PendingResponders,
    /// Hold the subprocess so it gets killed when the client is dropped.
    _child: Option<Arc<Mutex<tokio::process::Child>>>,
}

/// Sandbox handles a connection task needs to route ACP fs/* and
/// terminal/* requests across the container boundary.
#[derive(Debug, Clone)]
pub struct SessionSandbox {
    pub container_name: String,
    pub container_workdir: PathBuf,
    /// Snapshot of the session's sandbox info, used to re-resolve env on
    /// every `terminal/create` so the agent's shell commands see the same
    /// env entries (including any rotated host values) as the interactive
    /// tmux pane.
    pub sandbox_info: SandboxInfo,
    /// Profile the session was created under. Required for
    /// `resolved_sandbox_config` to pick up per-profile env overrides.
    pub source_profile: Option<String>,
    /// Host-side project path. `resolved_sandbox_config` walks up from
    /// here to find any repo-local config overrides.
    pub project_path: PathBuf,
}

impl SessionSandbox {
    /// Build a `SessionSandbox` + `SandboxPathMap` from a `SandboxInfo`
    /// and the session's host-side project_path. Path-map entries
    /// cover only the workspace volume(s) the container was built
    /// with; see `docs/acp.md` for the known-limitations note on
    /// agent-config and `extra_volumes`.
    pub fn from_info(
        sandbox: &SandboxInfo,
        project_path: &Path,
        source_profile: Option<String>,
    ) -> Result<(Self, SandboxPathMap), AcpError> {
        let project_path_str = project_path.to_string_lossy().to_string();
        let (volumes, computed_workdir) =
            crate::session::container_config::compute_volume_paths(project_path, &project_path_str)
                .map_err(|e| AcpError::Spawn(format!("compute container workdir: {e}")))?;
        // The workdir must be what the container was actually created with, not
        // a live recompute. `compute_volume_paths` resolves the worktree's git
        // linkage and silently collapses to `/workspace/<basename>` once that
        // linkage breaks on the host (and it ignores multi-repo `workspace_info`),
        // which would `docker exec -w` into a path the container never mounted
        // (#2414). Prefer the create-time-pinned value, then the live container's
        // own `WorkingDir`, and only fall back to the recompute when neither is
        // available (no container created yet). The mount map stays the computed
        // project volumes; making it workspace-complete needs `workspace_info`,
        // which the reattach path does not carry (tracked separately).
        let workdir = sandbox
            .container_workdir
            .clone()
            .or_else(|| {
                crate::containers::get_container_runtime()
                    .container_working_dir(&sandbox.container_name)
            })
            .unwrap_or(computed_workdir);
        let mounts: Vec<(PathBuf, PathBuf)> = volumes
            .into_iter()
            .map(|v| (PathBuf::from(v.container_path), PathBuf::from(v.host_path)))
            .collect();
        Ok((
            Self {
                container_name: sandbox.container_name.clone(),
                container_workdir: PathBuf::from(workdir),
                sandbox_info: sandbox.clone(),
                source_profile,
                project_path: project_path.to_path_buf(),
            },
            SandboxPathMap::new(mounts),
        ))
    }

    /// Re-resolve env entries for this session's sandbox. Called on every
    /// `terminal/create` so rotated host values (e.g. refreshed tokens)
    /// reach the agent's shell commands without requiring a container
    /// recreate.
    ///
    /// A missing `source_profile` only happens for legacy `WorkerRecord`
    /// entries written before the field was persisted. Warns once per
    /// call rather than failing, since refusing resolution would break
    /// `terminal/create` for sessions that are otherwise healthy.
    pub fn current_env_entries(&self) -> Vec<crate::containers::container_interface::EnvEntry> {
        let profile = match self.source_profile.as_deref() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    target: "acp.terminal",
                    container = %self.container_name,
                    "SessionSandbox has no source_profile (likely a legacy WorkerRecord); \
                     resolving terminal/create env against the global default profile"
                );
                ""
            }
        };
        let sandbox_config =
            crate::session::environment::resolved_sandbox_config(profile, &self.project_path);
        crate::session::environment::collect_environment(&sandbox_config, &self.sandbox_info)
    }
}

/// Per-session resources the connection task uses to handle ACP fs/* and
/// terminal/* requests delegated by the agent.
#[derive(Clone)]
struct SessionResources {
    fs_policy: Arc<FsPolicy>,
    terminals: TerminalManager,
    cwd: PathBuf,
    label: String,
    sandbox: Option<SessionSandbox>,
}

impl AcpClient {
    /// Construct a client that does not actually spawn anything. Useful
    /// for unit tests of structured view state without a real agent.
    pub fn fake_for_test(session_id: AcpSessionId) -> (Self, mpsc::Sender<Event>) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: None,
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx)
    }

    /// Like `fake_for_test`, but wires a live `cmd_tx` whose consumer
    /// records whether a `session/delete` RPC was issued. The returned
    /// `AtomicBool` flips to `true` the moment a
    /// `ClientCmd::DeleteSession` is received, and the consumer answers
    /// it immediately so the caller's `delete_session` returns without
    /// waiting on the timeout. Used to assert that reversible teardown
    /// does NOT delete the agent transcript while permanent removal
    /// does (#1710).
    #[cfg(test)]
    pub fn fake_for_test_recording(
        session_id: AcpSessionId,
    ) -> (
        Self,
        mpsc::Sender<Event>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let saw_delete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_delete_task = saw_delete.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let ClientCmd::DeleteSession { respond_to, .. } = cmd {
                    saw_delete_task.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
                }
            }
        });
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx, saw_delete)
    }

    /// Spawn an ACP agent subprocess, run the handshake + create a
    /// session, and start pumping notifications into the inbound channel.
    pub async fn spawn(config: SpawnConfig, session_id: AcpSessionId) -> Result<Self, AcpError> {
        // Pre-flight: if the session's project_path was renamed or moved
        // externally (e.g. `git worktree move` or a plain `mv`), the
        // agent process's `current_dir` will ENOENT at exec time. POSIX
        // surfaces that as the same `os error 2` as a missing binary,
        // so without the pre-flight the UI lands on the wrong "install
        // the adapter" remediation. Fail fast with a typed variant so
        // the supervisor can route to a targeted banner. See #1089.
        if !config.cwd.exists() {
            return Err(AcpError::ProjectPathMissing {
                path: config.cwd.clone(),
            });
        }
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let (event_tx, event_rx) = mpsc::channel::<Event>(64);
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));

        // Two transports:
        //  - Socket (runner-mediated): for every structured view session in
        //    production. Spawn `aoe __acp-runner` detached via
        //    `setsid`; the runner binds the unix socket, spawns the
        //    agent over stdio, and survives `aoe serve --stop`. The
        //    daemon then dials the socket and runs the ACP handshake.
        //  - Stdio (in-proc): the legacy direct-spawn path. Retained for
        //    tests where we don't want to depend on `current_exe()` being
        //    a real `aoe` binary, and as a safety valve.
        let mode = ConnectMode::Fresh {
            stored_acp_session_id: config.stored_acp_session_id.clone(),
            seed_history_replay: config.seed_history_replay,
        };
        let sandbox_pair = if let Some(info) = &config.sandbox_info {
            // `from_info` resolves the container workdir, which touches git2 and
            // (for a legacy session with no pinned workdir) shells out to
            // `docker inspect`. Run it off the async executor.
            let info = info.clone();
            let cwd = config.cwd.clone();
            let profile = config.source_profile.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    SessionSandbox::from_info(&info, cwd.as_path(), profile)
                })
                .await
                .map_err(|e| AcpError::Spawn(format!("sandbox resolve task panicked: {e}")))??,
            )
        } else {
            None
        };
        let runner_sandbox = sandbox_pair.as_ref().map(|(handle, _)| handle);
        let profile = agent_profiles::resolve(&config.agent_key);
        let install_binary = config.spec.command.clone();
        let source_profile_for_task = config.source_profile.clone();
        let default_effort = config.default_effort.clone();
        let mcp_servers = config.mcp_servers.clone();
        if let Some(socket_path) = config.socket_path.clone() {
            // Supersede guard: a fresh spawn overwrites this session's
            // registry entry, so any runner already registered for it would
            // be orphaned (its agent's node/SDK children reparent to PID 1
            // and leak, accumulating across restarts). Reap the prior
            // runner's whole process group and clear its stale entry/socket
            // before binding the replacement. No-op when there is no live
            // prior runner. See #1689.
            super::worker_registry::terminate(&session_id.0);
            spawn_runner_detached(&config, &socket_path, session_id.0.clone(), runner_sandbox)?;
            return Self::connect_via_socket(
                socket_path,
                config.cwd,
                config.additional_dirs,
                mode,
                session_id,
                pending_responders,
                cmd_tx,
                cmd_rx,
                event_tx,
                event_rx,
                sandbox_pair,
                profile,
                install_binary,
                source_profile_for_task,
                default_effort.clone(),
                mcp_servers,
            )
            .await;
        }

        let child = spawn_subprocess(&config)?;
        let child = Arc::new(Mutex::new(child));
        Self::start_with_stdio(
            config.cwd,
            config.additional_dirs,
            mode,
            session_id,
            child,
            pending_responders,
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            sandbox_pair,
            profile,
            install_binary,
            source_profile_for_task,
            default_effort,
            mcp_servers,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_stdio(
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        mode: ConnectMode,
        session_id: AcpSessionId,
        child: Arc<Mutex<tokio::process::Child>>,
        pending_responders: PendingResponders,
        cmd_tx: mpsc::Sender<ClientCmd>,
        cmd_rx: mpsc::Receiver<ClientCmd>,
        event_tx: mpsc::Sender<Event>,
        event_rx: mpsc::Receiver<Event>,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        profile: &'static agent_profiles::AgentProfile,
        install_binary: String,
        source_profile: Option<String>,
        default_effort: Option<String>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Self, AcpError> {
        let (stdin, stdout) = {
            let mut guard = child.lock().await;
            let stdin = guard
                .stdin
                .take()
                .ok_or_else(|| AcpError::Spawn("no stdin handle".into()))?;
            let stdout = guard
                .stdout
                .take()
                .ok_or_else(|| AcpError::Spawn("no stdout handle".into()))?;
            (stdin, stdout)
        };

        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let session_label = session_id.0.clone();
        let child_for_task = child.clone();
        let pending_for_task = pending_responders.clone();
        let expected_agent = ExpectedAgent::from_command(&install_binary);

        // Allowed fs roots: cwd + any explicit additional directories.
        let mut roots = vec![cwd.clone()];
        roots.extend(additional_dirs);
        let (sandbox_handle, fs_policy) = match sandbox {
            Some((handle, path_map)) => (
                Some(handle),
                Arc::new(FsPolicy::with_sandbox_map(roots, path_map)),
            ),
            None => (None, Arc::new(FsPolicy::new(roots))),
        };
        let resources = SessionResources {
            fs_policy,
            terminals: TerminalManager::new(),
            cwd: cwd.clone(),
            label: session_label.clone(),
            sandbox: sandbox_handle,
        };

        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();

        // Wrap the per-session connection task in a span carrying the
        // session id so every nested event inherits it; the daemon's
        // per-session log tee routes by that field (#1864). The span name
        // must match `crate::acp::session_tee::SESSION_SPAN`.
        let conn_span = tracing::info_span!("acp_session", session = %session_label);
        tokio::spawn(
            run_connection_task(
                transport,
                event_tx,
                cmd_rx,
                cwd,
                session_label.clone(),
                Some(child_for_task),
                pending_for_task,
                resources,
                None,
                mode,
                Some(ready_tx),
                profile,
                expected_agent,
                source_profile,
                default_effort,
                mcp_servers,
            )
            .instrument(conn_span),
        );

        wait_for_handshake(&session_label, ready_rx, Some(&child), &install_binary).await?;

        Ok(Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            _child: Some(child),
        })
    }

    /// Connect to a per-session runner over its unix socket. Used by the
    /// post-spawn "wait for runner to bind, then dial" path AND by the
    /// `Self::attach` reattach path on `aoe serve` startup. The runner
    /// owns the agent subprocess so this constructor returns an
    /// `AcpClient` with `_child = None`; dropping the client does not
    /// terminate the worker.
    #[allow(clippy::too_many_arguments)]
    async fn connect_via_socket(
        socket_path: PathBuf,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        mode: ConnectMode,
        session_id: AcpSessionId,
        pending_responders: PendingResponders,
        cmd_tx: mpsc::Sender<ClientCmd>,
        cmd_rx: mpsc::Receiver<ClientCmd>,
        event_tx: mpsc::Sender<Event>,
        event_rx: mpsc::Receiver<Event>,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        profile: &'static agent_profiles::AgentProfile,
        install_binary: String,
        source_profile: Option<String>,
        default_effort: Option<String>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Self, AcpError> {
        // Poll for the runner to finish binding the socket. The runner
        // binds before it spawns the agent so this is usually fast (a
        // few ms) but bound the wait so a wedged runner returns a typed
        // error instead of parking the supervisor.
        let stream = wait_for_socket(&socket_path, runner_socket_deadline()).await?;
        // #1890 regression hook (debug-only): simulate a fresh-spawn whose
        // daemon-side handshake fails after the runner is already up and
        // registered. Dropping the socket closes the daemon's end cleanly; the
        // runner keeps its agent alive and its registry entry, leaving the
        // orphaned-but-live-runner state the readopt pass must recover from.
        // Gated on `Fresh` so the recovery reattach/respawn is never failed,
        // and budgeted by the env var so only the first spawn trips.
        #[cfg(debug_assertions)]
        if matches!(mode, ConnectMode::Fresh { .. }) && take_injected_fresh_handshake_failure() {
            drop(stream);
            return Err(AcpError::Spawn(
                "injected fresh-handshake failure (AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES)".into(),
            ));
        }
        let (read_half, write_half) = stream.into_split();
        let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());

        let mut roots = vec![cwd.clone()];
        roots.extend(additional_dirs);
        let (sandbox_handle, fs_policy) = match sandbox {
            Some((handle, path_map)) => (
                Some(handle),
                Arc::new(FsPolicy::with_sandbox_map(roots, path_map)),
            ),
            None => (None, Arc::new(FsPolicy::new(roots))),
        };
        let resources = SessionResources {
            fs_policy,
            terminals: TerminalManager::new(),
            cwd: cwd.clone(),
            label: session_id.0.clone(),
            sandbox: sandbox_handle,
        };

        let session_label = session_id.0.clone();
        let pending_for_task = pending_responders.clone();
        let expected_agent = ExpectedAgent::from_command(&install_binary);

        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();

        // See the sibling spawn in `spawn`: the connection task runs inside
        // an `acp_session` span so per-session log teeing (#1864) catches
        // events that do not set the `session` field explicitly.
        let conn_span = tracing::info_span!("acp_session", session = %session_label);
        tokio::spawn(
            run_connection_task(
                transport,
                event_tx,
                cmd_rx,
                cwd,
                session_label.clone(),
                None,
                pending_for_task,
                resources,
                None,
                mode,
                Some(ready_tx),
                profile,
                expected_agent,
                source_profile,
                default_effort,
                mcp_servers,
            )
            .instrument(conn_span),
        );

        wait_for_handshake(&session_label, ready_rx, None, &install_binary).await?;

        Ok(Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            _child: None,
        })
    }

    /// Reattach to an already-running structured view worker over its unix
    /// socket. Used by `aoe serve` startup when a registry entry has a
    /// live PID and an existing socket file; we connect, send only the
    /// (idempotent) ACP `initialize` request, and reuse the existing
    /// `stored_acp_session_id` directly. We deliberately do NOT issue
    /// `session/new` or `session/load`: the agent process is still
    /// running (the runner kept it alive across `aoe serve --stop`) and
    /// the session is already loaded in its memory, so re-sending those
    /// requests would either split context onto a new session id (when
    /// the agent doesn't advertise `loadSession`) or double-load against
    /// a busy session.
    ///
    /// `in_flight_turn = true` tells the connection task that the
    /// session was mid-prompt when the previous daemon detached. The
    /// task arms a watchdog that emits a synthetic
    /// `Event::Stopped { reason: "reattach_idle" }` after
    /// `RESUME_IDLE_GRACE` of inbound silence, because the agent's
    /// eventual response to the orphaned `session/prompt` carries a
    /// request id this client never issued and is dropped silently by
    /// the underlying transport, leaving the UI otherwise stuck on
    /// "thinking".
    #[allow(clippy::too_many_arguments)]
    pub async fn attach(
        socket_path: PathBuf,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        stored_acp_session_id: String,
        in_flight_turn: bool,
        session_id: AcpSessionId,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        agent_key: String,
        source_profile: Option<String>,
    ) -> Result<Self, AcpError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let (event_tx, event_rx) = mpsc::channel::<Event>(64);
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
        let mode = ConnectMode::Resume {
            acp_session_id: stored_acp_session_id,
            in_flight_turn,
        };
        let profile = agent_profiles::resolve(&agent_key);
        // Resolve the binary name from the registry so the resume path
        // still routes through the per-adapter compatibility gate
        // (`agent_compat::ExpectedAgent::from_command`). Reattaching to
        // a stale claude-agent-acp@0.32.0 worker that survived an aoe
        // serve restart should re-trigger the >=0.37.0 check, not
        // silently skip it just because the resume path has no install
        // hint to surface. Empty fallback only when the agent key is
        // not in the registry (an unknown user-configured agent);
        // policy maps that to Other anyway.
        let install_binary = super::AgentRegistry::with_defaults()
            .get(&agent_key)
            .map(|spec| spec.command.clone())
            .unwrap_or_default();
        Self::connect_via_socket(
            socket_path,
            cwd,
            additional_dirs,
            mode,
            session_id,
            pending_responders,
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            sandbox,
            profile,
            install_binary,
            source_profile,
            None,
            // Reattach uses ConnectMode::Resume, which reuses the stored ACP
            // session id without sending session/new or session/load, so no
            // MCP servers are forwarded here (they were sent on first connect).
            Vec::new(),
        )
        .await
    }

    /// Send a user message to the agent (ACP `session/prompt`). The
    /// `attachments` are mapped to the matching ACP `ContentBlock`
    /// (`Image` / `Audio` / `Resource`) and appended after the text
    /// block. Callers are responsible for gating attachment kinds on
    /// the agent's advertised `prompt_capabilities`; this method does
    /// not re-check them. See #1000 / #965.
    pub async fn send_prompt(
        &self,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), AcpError> {
        use base64::Engine as _;
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(1 + attachments.len());
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        for att in attachments {
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let block = match att.kind {
                PromptAttachmentKind::Image => {
                    ContentBlock::Image(ImageContent::new(data_b64, att.mime_type.clone()))
                }
                PromptAttachmentKind::Audio => {
                    ContentBlock::Audio(AudioContent::new(data_b64, att.mime_type.clone()))
                }
                PromptAttachmentKind::Resource => {
                    // Embedded binary resource. ACP requires a uri; the
                    // bytes never leave the daemon so a synthetic
                    // `attachment://` uri is enough for the agent to
                    // refer to it.
                    let uri = format!("attachment:///{}", att.id);
                    let blob =
                        BlobResourceContents::new(data_b64, uri).mime_type(att.mime_type.clone());
                    ContentBlock::Resource(EmbeddedResource::new(
                        EmbeddedResourceResource::BlobResourceContents(blob),
                    ))
                }
            };
            blocks.push(block);
        }
        cmd_tx
            .send(ClientCmd::Prompt(blocks))
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Cancel the agent's currently-running turn (ACP `session/cancel`
    /// notification). Best-effort: returns Ok even if no turn is in
    /// flight, since the UI can race the agent finishing on its own.
    pub async fn cancel_prompt(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::Cancel)
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Force-stop the in-flight turn immediately, bypassing the 10s
    /// cancel-escalation grace. If a prompt is in flight the connection
    /// task ends the turn with `Stopped { reason: "user_forced" }`, which
    /// the drain task treats like `agent_unresponsive`: it kills the
    /// worker's process group and respawns with `session/load`. This is
    /// the only lever that reliably stops a tool the agent runs
    /// internally (a monitor/until loop) and ignores `session/cancel` on.
    /// Best-effort: returns Ok even if no turn is in flight. See #1727.
    pub async fn force_cancel(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::ForceStop)
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Switch the active session mode via ACP `session/set_mode`.
    pub async fn set_mode(&self, mode_id: &str) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::SetMode(mode_id.to_string()))
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Set a per-session selector (model, reasoning effort, etc.) via
    /// ACP `session/set_config_option`. The structured view treats every
    /// adapter-advertised category through this one path; specific
    /// helpers per category would just duplicate the wiring. See
    /// #1403.
    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::SetConfigOption {
                config_id: config_id.to_string(),
                value: value.to_string(),
            })
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Resolve a pending permission request. Looks up the parked
    /// responder by nonce and unblocks the `on_receive_request` callback.
    pub async fn resolve_permission(
        &self,
        nonce: Nonce,
        decision: ApprovalDecision,
    ) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        // Only consume the entry if it is actually a permission; a nonce
        // that belongs to an elicitation is "unknown" to this endpoint.
        let PendingResolver::Approval(_) = &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let PendingResolver::Approval(resolver) = map.remove(&nonce).unwrap().resolver else {
            unreachable!("checked above");
        };
        resolver
            .send(ApprovalResolutionMessage::Decision { decision })
            .map_err(|_| AcpError::AgentExited)
    }

    /// Cancel a pending permission request. Marks it as cancelled so
    /// the agent receives a structured cancellation outcome.
    pub async fn cancel_permission(&self, nonce: Nonce) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        let PendingResolver::Approval(_) = &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let PendingResolver::Approval(resolver) = map.remove(&nonce).unwrap().resolver else {
            unreachable!("checked above");
        };
        resolver
            .send(ApprovalResolutionMessage::Cancelled)
            .map_err(|_| AcpError::AgentExited)
    }

    /// Resolve a pending elicitation by nonce, unblocking the parked
    /// `elicitation/create` callback with the user's accept/decline/cancel
    /// answer. A nonce belonging to a permission (or already resolved) is
    /// reported as unknown.
    ///
    /// The submitted answer is validated (`build_response`) BEFORE the
    /// parked resolver is consumed. An invalid answer returns
    /// `InvalidAnswer` and leaves the elicitation pending, so the client
    /// can correct it and resubmit instead of the question aborting on a
    /// client/server validation mismatch (#2100). Only a valid answer
    /// removes the nonce and forwards the built response to the agent.
    pub async fn resolve_elicitation(
        &self,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        let PendingResolver::Elicitation { elicitation, .. } =
            &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        // Validate against the parked form while it is still borrowed; on
        // failure the nonce stays in the map untouched.
        let outcome = resolution.outcome();
        // Render the submitted answers for the transcript before
        // `build_response` consumes `resolution`. The parked form supplies
        // question titles; selects carry the clean label. See #2209.
        let answers = match &resolution {
            ElicitationResolution::Accept { answers } => summarize_answers(elicitation, answers),
            ElicitationResolution::Decline | ElicitationResolution::Cancel => Vec::new(),
        };
        let response = build_response(elicitation, resolution)
            .map_err(|e| AcpError::InvalidAnswer(e.to_string()))?;
        // Valid: now consume the responder and forward the built response.
        let PendingResolver::Elicitation { resolver, .. } = map.remove(&nonce).unwrap().resolver
        else {
            unreachable!("checked above");
        };
        resolver
            .send(ElicitationResolutionMessage {
                response,
                outcome,
                answers,
            })
            .map_err(|_| AcpError::AgentExited)
    }

    /// Best-effort experimental `session/delete` RPC. Sent before
    /// `shutdown` during structured view session deletion so adapters that
    /// persist session-side state (claude-agent-acp clears the on-disk
    /// Claude session record) get a chance to clean up before SIGTERM.
    ///
    /// All outcomes are non-fatal. Adapters that don't implement the
    /// method return `-32601 method_not_found` and surface as
    /// `UnsupportedMethod`; the supervisor proceeds to the existing
    /// kill path either way. Bounded by `ACP_SESSION_DELETE_TIMEOUT`
    /// so a wedged adapter cannot stall delete. See #1404.
    pub async fn delete_session(&self, acp_session_id: String) -> DeleteSessionOutcome {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return DeleteSessionOutcome::Failed("client not running".into());
        };
        let (tx, rx) = oneshot::channel();
        // Outer guard wraps BOTH the cmd_tx send AND the response wait.
        // The mpsc send is `await`-able and can block if the connect
        // task is wedged or the channel is saturated; without the
        // guard a stalled worker would freeze the delete path
        // indefinitely while the supervisor holds the per-instance
        // lock at `sessions.rs:1361`. Wait slightly longer than the
        // in-task `ACP_SESSION_DELETE_TIMEOUT` so the inner
        // classification (Deleted/UnsupportedMethod/Failed) wins when
        // the task is healthy.
        let request = async {
            if cmd_tx
                .send(ClientCmd::DeleteSession {
                    acp_session_id,
                    respond_to: tx,
                })
                .await
                .is_err()
            {
                return DeleteSessionOutcome::Failed("connect task gone".into());
            }
            match rx.await {
                Ok(outcome) => outcome,
                Err(_) => DeleteSessionOutcome::Failed("respond channel closed".into()),
            }
        };
        match tokio::time::timeout(
            ACP_SESSION_DELETE_TIMEOUT + std::time::Duration::from_millis(500),
            request,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => DeleteSessionOutcome::TimedOut,
        }
    }

    /// Shutdown the connection task and kill the subprocess.
    pub async fn shutdown(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let _ = cmd_tx.send(ClientCmd::Shutdown).await;
        Ok(())
    }

    /// Drain the next event the agent emitted. Returns None once the
    /// receiver has been moved out via `take_inbound` (the supervisor
    /// path) or the connection task has dropped its sender.
    pub async fn next_event(&mut self) -> Option<Event> {
        match self.inbound.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// Take ownership of the inbound event receiver. The supervisor uses
    /// this so the drain task can poll events without holding the client
    /// mutex (which would deadlock send_prompt).
    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.inbound.take()
    }
}

/// Reject `provider_env` request entries whose key would either escape
/// the agent sandbox (PATH, HOME, etc.; `always_forward` already wires
/// those from the operator's environment) or hijack the dynamic linker
/// (LD_PRELOAD, DYLD_INSERT_LIBRARIES, etc.) to run arbitrary code in
/// the child. Provider auth keys (`ANTHROPIC_API_KEY`, etc.) are
/// deliberately NOT on the denylist because per-session provider auth
/// is the legitimate use case for `provider_env`.
///
/// Returns `Some(reason)` if the key is rejected, `None` if it's safe
/// to forward. The reason string is logged as a structured field.
fn provider_env_denyreason(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("empty key");
    }
    if key == "AOE_TOKEN" {
        return Some("aoe auth token, must not reach the agent");
    }
    // Infrastructure / locale keys that `always_forward` already wires
    // from the parent env. Letting `provider_env` override them lets the
    // request point the agent's binary lookup or home tree at an
    // attacker-controlled location.
    const INFRA_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];
    if INFRA_KEYS.contains(&key) {
        return Some("infrastructure key, controlled by operator env");
    }
    // Dynamic linker hooks: glibc `LD_*` and macOS `DYLD_*`. Overriding
    // these causes the child process to load attacker-chosen shared
    // objects before main(), bypassing the agent binary entirely.
    if key.starts_with("LD_") || key.starts_with("DYLD_") {
        return Some("dynamic linker hook, would alter child binary load");
    }
    None
}

/// Scrub well-known secret patterns from agent stderr before it lands in
/// `debug.log`. Conservative; only redacts strings that unambiguously
/// signal a secret via prefix (Anthropic `sk-`, GitHub `ghp_`,
/// `Bearer <token>`, etc.). Catches the common case where an adapter
/// prints "auth failed: api_key=sk-ant-..."; will not catch a hand-rolled
/// secret with no recognisable shape. Users sharing logs in bug reports
/// should still scan them; see docs/acp.md#sharing-debug-logs.
fn scrub_stderr_secrets(line: &str) -> std::borrow::Cow<'_, str> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"\b(sk-(?:ant-)?[A-Za-z0-9_\-]{16,}|ghp_[A-Za-z0-9]{16,}|gho_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,}|AKIA[A-Z0-9]{16}|Bearer\s+[A-Za-z0-9_.\-]{20,})",
        )
        .expect("static secret-scrub regex must compile")
    });
    re.replace_all(line, "<redacted-secret>")
}

/// Resolve a bare agent command name to an absolute path, scanning common
/// node-version-manager bin dirs (nvm, fnm, mise, asdf, Volta) plus the
/// usual system locations. Returns the absolute binary path and the bin
/// dir we found it in; the caller prepends that dir to the agent's PATH
/// so the adapter's own subprocesses (`node`, `npx`) can still resolve.
///
/// Re-runs per spawn (no cache) so an `nvm use <other-version>` after the
/// daemon started picks up immediately without a daemon restart. Returns
/// None when the command is already a path, contains a `${placeholder}`,
/// or isn't found anywhere we know to look.
pub fn resolve_agent_command(command: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    if command.contains('/') || command.contains('\\') || command.contains("${") {
        return None;
    }

    if let Some(path) = find_in_path_env(command) {
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(std::path::PathBuf::new);
        return Some((path, parent));
    }

    for dir in node_search_dirs() {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some((candidate, dir));
        }
    }
    None
}

fn find_in_path_env(binary: &str) -> Option<std::path::PathBuf> {
    which::which(binary).ok()
}

/// Best-effort enumeration of node bin dirs the user is likely to have
/// the adapter installed into. Order matters only for tie-breaking; the
/// first hit wins, but in practice each binary only lives in one place.
fn node_search_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        // nvm: `~/.nvm/versions/node/v<ver>/bin/<binary>`
        push_subdirs(&mut out, &home.join(".nvm/versions/node"), "bin");
        // fnm: `~/.fnm/node-versions/v<ver>/installation/bin/<binary>`
        push_subdirs(
            &mut out,
            &home.join(".fnm/node-versions"),
            "installation/bin",
        );
        // mise: `~/.local/share/mise/installs/node/<ver>/bin/<binary>`
        push_subdirs(
            &mut out,
            &home.join(".local/share/mise/installs/node"),
            "bin",
        );
        // asdf: `~/.asdf/installs/nodejs/<ver>/bin/<binary>`
        push_subdirs(&mut out, &home.join(".asdf/installs/nodejs"), "bin");
        // Volta + user-scoped npm prefixes
        out.push(home.join(".volta/bin"));
        out.push(home.join(".npm-global/bin"));
        out.push(home.join(".local/bin"));
        out.push(home.join("bin"));
    }
    out.push(std::path::PathBuf::from("/usr/local/bin"));
    out.push(std::path::PathBuf::from("/opt/homebrew/bin"));
    out.push(std::path::PathBuf::from("/usr/bin"));
    out
}

fn push_subdirs(out: &mut Vec<std::path::PathBuf>, root: &std::path::Path, leaf: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let bin = entry.path().join(leaf);
        if bin.is_dir() {
            out.push(bin);
        }
    }
}

/// Spawn the `aoe __acp-runner` shim as a detached process. The
/// runner owns the agent subprocess and outlives the daemon. We retain
/// no `Child` handle here; once the runner is up, the daemon talks to
/// it over the unix socket and the OS keeps the runner alive across
/// `aoe serve` restarts.
fn spawn_runner_detached(
    config: &SpawnConfig,
    socket_path: &std::path::Path,
    session_id: String,
    session_sandbox: Option<&SessionSandbox>,
) -> Result<(), AcpError> {
    use std::process::Command as StdCommand;
    let current_exe =
        std::env::current_exe().map_err(|e| AcpError::Spawn(format!("current_exe: {e}")))?;
    let log_path = crate::acp::worker_registry::log_path_for(&session_id)
        .map_err(|e| AcpError::Spawn(format!("log path: {e}")))?;
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Sandboxed sessions wrap the agent in `docker exec`. Host-side
    // PATH resolution is skipped because the agent binary lives inside
    // the container; the container's own PATH resolves it. The
    // container_workdir is reused from the SessionSandbox built upstream
    // so we don't redo `compute_volume_paths`.
    let sandbox_argv = match (&config.sandbox_info, session_sandbox) {
        (Some(sandbox), Some(handle)) => {
            let argv = build_sandbox_docker_argv(
                config,
                sandbox,
                handle.container_workdir.to_string_lossy().as_ref(),
            )?;
            info!(
                target: "acp.protocol.spawn",
                session = %session_id,
                container = %sandbox.container_name,
                container_id = sandbox.container_id.as_deref().unwrap_or("?"),
                image = %sandbox.image,
                workdir = %handle.container_workdir.display(),
                docker = %argv.docker_binary,
                "docker wrap applied"
            );
            Some(argv)
        }
        (Some(_), None) => {
            return Err(AcpError::Spawn(
                "sandbox_info set but SessionSandbox handle missing; \
                 SessionSandbox::from_info must run before spawn_runner_detached"
                    .into(),
            ));
        }
        (None, _) => {
            info!(
                target: "acp.protocol.spawn",
                session = %session_id,
                "docker wrap skipped (no sandbox_info)"
            );
            None
        }
    };

    // Resolve the agent binary against PATH + known node-manager dirs so
    // the runner spawns the right binary even when the daemon's frozen
    // PATH doesn't contain it. See #1048. The resolved bin dir is also
    // prepended to PATH below so the adapter's own `node`/`npx`
    // subprocesses land in the same install.
    let resolved = if sandbox_argv.is_some() {
        None
    } else {
        resolve_agent_command(&config.spec.command)
    };
    let (spawn_command, extra_path_dir) = match (&sandbox_argv, &resolved) {
        (Some(s), _) => (s.docker_binary.clone(), None),
        (None, Some((abs, dir))) => (abs.to_string_lossy().into_owned(), Some(dir.clone())),
        (None, None) => (config.spec.command.clone(), None),
    };

    let mut cmd = StdCommand::new(&current_exe);
    cmd.arg("__acp-runner")
        .arg("--socket")
        .arg(socket_path)
        .arg("--session-id")
        .arg(&session_id)
        .arg("--agent-name")
        .arg(&config.spec.command)
        .arg("--agent-key")
        .arg(&config.agent_key)
        .arg("--cwd")
        .arg(&config.cwd);
    if !config.additional_dirs.is_empty() {
        cmd.arg("--additional-dirs").arg(
            config
                .additional_dirs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    let provider_keys: Vec<&str> = config
        .provider_env
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    if !provider_keys.is_empty() {
        cmd.arg("--provider-env-keys").arg(provider_keys.join(","));
    }
    if let Some(profile) = config.source_profile.as_deref().filter(|s| !s.is_empty()) {
        cmd.arg("--source-profile").arg(profile);
    }
    if let Some(stored) = &config.stored_acp_session_id {
        cmd.arg("--stored-acp-session-id").arg(stored);
    }
    cmd.arg("--");
    if let Some(s) = &sandbox_argv {
        cmd.arg(&s.docker_binary);
        for a in &s.docker_args {
            cmd.arg(a);
        }
    } else {
        // Pass the resolved absolute path (or fall back to the bare command).
        // The runner spawns whatever it receives, so an absolute path bypasses
        // any PATH lookup inside the runner.
        cmd.arg(&spawn_command);
        for a in &config.spec.args {
            cmd.arg(a);
        }
    }

    // Env: apply the same allowlist + provider_env filtering that the
    // legacy in-proc path does, then hand the cleaned env to the runner.
    // The runner inherits this env when it spawns the agent (no second
    // filter pass needed). AOE_TOKEN is stripped here so it never reaches
    // either process.
    cmd.env_clear();
    apply_env_filter(&mut cmd, config);
    if let Some(s) = &sandbox_argv {
        // The agent runs inside the container; docker reads each
        // `-e KEY` flag's value from its own process env. Set the
        // corresponding values on the runner so docker (its child)
        // can forward them across the container boundary.
        for (key, value) in &s.inherit_env {
            cmd.env(key, value);
        }
    }
    if let Some(extra) = &extra_path_dir {
        // Prepend the resolved bin dir to the PATH we just forwarded so
        // the adapter's own `node`/`npx` lookups land in the same install
        // as the adapter itself, not whatever node happens to be on the
        // daemon's frozen PATH.
        let current = std::env::var("PATH").unwrap_or_default();
        let extra_s = extra.to_string_lossy();
        if !std::env::split_paths(&current).any(|p| p == *extra) {
            cmd.env("PATH", format!("{}:{}", extra_s, current));
        }
    }

    // Detach: child becomes its own session leader so a SIGTERM/SIGHUP
    // to the aoe daemon's group doesn't cascade. The runner installs its
    // own signal handlers.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    // Redirect stdio: the runner writes its own log file. Inheriting our
    // stdio would (a) pollute the shared debug.log with the per-session
    // noise and (b) keep a pipe open to the daemon, which then closes
    // when we die, making the runner observe EOF on its own stdin/stdout.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    info!(
        target: "acp.protocol.spawn",
        session = %session_id,
        socket = %socket_path.display(),
        runner = %current_exe.display(),
        agent = %config.spec.command,
        resolved = %spawn_command,
        "spawning detached structured view runner"
    );

    cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            session = %session_id,
            "runner spawn failed: {e}"
        );
        AcpError::Spawn(format!("spawn runner: {e}"))
    })?;
    // Drop the std::process::Child here. std::process::Command doesn't
    // wait on drop, so the runner stays alive. setsid + nohup-equivalent
    // make this an actual detach.
    Ok(())
}

/// Result of constructing the `docker exec` argv for a sandboxed structured view
/// spawn. `docker_binary` is argv[0] (the docker/podman runtime);
/// `docker_args` is everything after it (including the container name
/// and the in-container agent argv). `inherit_env` is the set of
/// (key, value) pairs the parent process must export so docker can
/// forward them via the matching `-e KEY` flags already in `docker_args`.
struct SandboxArgv {
    docker_binary: String,
    docker_args: Vec<String>,
    inherit_env: Vec<(String, String)>,
}

/// Build the `docker exec` argv for a sandboxed structured view spawn. The
/// resulting command is what the runner executes; docker proxies the
/// agent's stdio across the container boundary. Mirrors the tmux
/// view's env handling so the same `sandbox.environment` and
/// `extra_env` entries take effect.
///
/// `container_workdir` is the in-container working directory for the
/// session, pre-computed by `SessionSandbox::from_info` and passed
/// through to avoid re-running `compute_volume_paths`.
fn build_sandbox_docker_argv(
    config: &SpawnConfig,
    sandbox: &SandboxInfo,
    container_workdir: &str,
) -> Result<SandboxArgv, AcpError> {
    use crate::containers::container_interface::docker_env_args;

    let runtime = crate::containers::get_container_runtime();
    let docker_binary = runtime.base.binary.to_string();

    let project_path = config.cwd.as_path();
    let profile_for_env = config.source_profile.as_deref().unwrap_or("");
    let sandbox_config =
        crate::session::environment::resolved_sandbox_config(profile_for_env, project_path);
    let env_entries = crate::session::environment::collect_environment(&sandbox_config, sandbox);

    let mut docker_args: Vec<String> = vec![
        "exec".into(),
        "-i".into(),
        "-w".into(),
        container_workdir.to_string(),
    ];
    // `collect_environment` already dedupes by key, so the entry list is
    // unique. We still track `seen_keys` so the provider-auth block below
    // can skip keys we've already forwarded.
    let mut seen_keys: std::collections::HashSet<String> =
        env_entries.iter().map(|e| e.key().to_string()).collect();
    let (env_argv, inherit_pairs) = docker_env_args(&env_entries);
    docker_args.extend(env_argv);
    let mut inherit_env: Vec<(String, String)> = inherit_pairs;

    // Provider auth keys: forward into the container only when set on
    // the host AND not already in the sandbox env list. Value-typed
    // only; host filesystem paths (e.g. `CLAUDE_CONFIG_DIR`) must not
    // cross the namespace boundary because they reference paths that
    // don't exist inside the container. The agent's config dir is
    // already bind-mounted at the canonical container path via
    // `AGENT_CONFIG_MOUNTS`.
    const PROVIDER_AUTH_KEYS: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ];
    for &key in PROVIDER_AUTH_KEYS {
        if seen_keys.contains(key) {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            seen_keys.insert(key.to_string());
            docker_args.push("-e".into());
            docker_args.push(key.into());
            inherit_env.push((key.into(), value));
        }
    }

    // Per-spawn provider_env entries (the request's auth payload).
    for (key, value) in &config.provider_env {
        if provider_env_denyreason(key).is_some() {
            continue;
        }
        if seen_keys.insert(key.clone()) {
            docker_args.push("-e".into());
            docker_args.push(key.clone());
            inherit_env.push((key.clone(), value.clone()));
        }
    }

    // Model override (AOE_AGENT_MODEL): the supervisor folds the
    // requested model into provider_env above, so it's already covered.

    docker_args.push(sandbox.container_name.clone());
    docker_args.push(config.spec.command.clone());
    for a in &config.spec.args {
        docker_args.push(a.clone());
    }

    Ok(SandboxArgv {
        docker_binary,
        docker_args,
        inherit_env,
    })
}

/// Apply the env_clear + allowlist + provider_env filtering used by both
/// the detached-runner path and the in-proc stdio path. Pulled out so
/// the two spawn sites share the same security posture.
fn apply_env_filter(cmd: &mut std::process::Command, config: &SpawnConfig) {
    const ALWAYS_FORWARD: &[&str] = &[
        "PATH",
        "HOME",
        // XDG_CONFIG_HOME drives `get_app_dir()` on Linux (see
        // src/session/mod.rs). Without forwarding, the runner falls
        // back to `$HOME/.config/agent-of-empires[-dev]`, which
        // diverges from the daemon when the operator (or live test
        // harness) has set XDG_CONFIG_HOME to a non-default value.
        // The runner then writes its WorkerRecord to a path the
        // daemon never reads, the daemon's `reap_user_stopped`
        // observes the registry as missing on the next tick, emits
        // `Stopped { user_stopped }`, and respawns, turning a fine
        // worker into a respawn loop. See #1383 (CI Linux live
        // specs under an isolated $XDG_CONFIG_HOME).
        "XDG_CONFIG_HOME",
        "LANG",
        "LC_ALL",
        "TERM",
        "USER",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CONFIG_DIR",
    ];
    for name in ALWAYS_FORWARD {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    if let Some(extra_allowlist) = &config.spec.env_allowlist {
        for name in extra_allowlist {
            if name == "AOE_TOKEN" {
                continue;
            }
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
    }
    for (key, value) in &config.provider_env {
        if provider_env_denyreason(key).is_some() {
            continue;
        }
        cmd.env(key, value);
    }
}

/// Poll the socket file's existence with `connect()` until a deadline.
/// Used by `connect_via_socket` to wait for the runner to finish binding
/// before the daemon dials in.
async fn wait_for_socket(
    path: &std::path::Path,
    deadline: std::time::Duration,
) -> Result<tokio::net::UnixStream, AcpError> {
    let started = std::time::Instant::now();
    let mut delay_ms = 20_u64;
    loop {
        if path.exists() {
            match tokio::net::UnixStream::connect(path).await {
                Ok(s) => return Ok(s),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused) => {
                    // Listener not yet ready; back off and retry.
                }
                Err(e) => return Err(AcpError::Spawn(format!("connect {}: {e}", path.display()))),
            }
        }
        if started.elapsed() >= deadline {
            return Err(AcpError::Spawn(format!(
                "runner socket {} did not appear within {}s",
                path.display(),
                deadline.as_secs()
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        delay_ms = (delay_ms * 2).min(200);
    }
}

fn spawn_subprocess(config: &SpawnConfig) -> Result<tokio::process::Child, AcpError> {
    // Resolve bare command names against PATH + known node-manager dirs.
    // `aoe serve` captures PATH at daemon-launch time and freezes it for
    // its lifetime; without this, a `nvm use` after launch leaves the
    // adapter installed but unreachable. See #1048.
    let resolved = resolve_agent_command(&config.spec.command);
    let (spawn_command, extra_path_dir) = match &resolved {
        Some((abs, dir)) => (abs.to_string_lossy().into_owned(), Some(dir.clone())),
        None => (config.spec.command.clone(), None),
    };

    let mut cmd = tokio::process::Command::new(&spawn_command);
    cmd.args(&config.spec.args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Env: clear, then forward an explicit allowlist + provider-specific
    // creds. AOE_TOKEN must NEVER reach the agent.
    cmd.env_clear();
    let always_forward = [
        "PATH",
        "HOME",
        // Mirror the runner-mode ALWAYS_FORWARD: XDG_CONFIG_HOME drives
        // `get_app_dir()` on Linux, so the stdio agent must see the
        // same value the daemon resolved against (otherwise a custom
        // XDG_CONFIG_HOME diverges between daemon and agent).
        "XDG_CONFIG_HOME",
        "LANG",
        "LC_ALL",
        "TERM",
        "USER",
        // Provider auth: forwarded by default so users who already have
        // `ANTHROPIC_API_KEY` (or have run `claude /login` so their
        // ~/.claude credentials sit under HOME) get a working agent
        // without manual env_allowlist plumbing.
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CONFIG_DIR",
    ];
    let mut forwarded_keys: Vec<&str> = Vec::new();
    for name in always_forward {
        if let Ok(mut value) = std::env::var(name) {
            // Prepend the resolved bin dir to PATH so the adapter's own
            // `node`/`npx` lookups land in the same node install as the
            // adapter itself, not whatever node happens to be on the
            // daemon's frozen PATH.
            if name == "PATH" {
                if let Some(extra) = &extra_path_dir {
                    let extra_s = extra.to_string_lossy();
                    if !std::env::split_paths(&value).any(|p| p == *extra) {
                        value = format!("{}:{}", extra_s, value);
                    }
                }
            }
            cmd.env(name, value);
            forwarded_keys.push(name);
        }
    }
    if let Some(extra_allowlist) = &config.spec.env_allowlist {
        for name in extra_allowlist {
            if name == "AOE_TOKEN" {
                warn!(target: "acp", "ignoring AOE_TOKEN in agent env allowlist");
                continue;
            }
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
                forwarded_keys.push(name.as_str());
            }
        }
    }
    let mut provider_keys: Vec<&str> = Vec::new();
    for (key, value) in &config.provider_env {
        if let Some(reason) = provider_env_denyreason(key) {
            warn!(
                target: "acp",
                key = %key,
                reason,
                "rejecting provider_env override of protected key",
            );
            continue;
        }
        cmd.env(key, value);
        provider_keys.push(key.as_str());
    }

    // Socket-transport agents need to know where to connect. Pass the
    // path via env so the agent's bootstrap can `connect()` to it
    // instead of falling back to stdio.
    if let Some(socket_path) = &config.socket_path {
        cmd.env("AOE_ACP_SOCKET", socket_path);
    }

    info!(
        target: "acp.protocol.spawn",
        command = %config.spec.command,
        resolved = %spawn_command,
        args = ?config.spec.args,
        cwd = %config.cwd.display(),
        transport = if config.socket_path.is_some() { "socket" } else { "stdio" },
        socket = ?config.socket_path,
        env_forwarded = ?forwarded_keys,
        provider_env = ?provider_keys,
        "spawning ACP agent subprocess"
    );

    let mut child = cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            resolved = %spawn_command,
            "spawn failed: {e}"
        );
        // POSIX ENOENT on `Command::spawn` is ambiguous: missing binary,
        // missing cwd, or missing interpreter all surface as the same
        // libc error. Order matters here:
        //   1. cwd missing → ProjectPathMissing (so the UI renders the
        //      "restore or rebind project_path" banner, not the
        //      install-adapter copy). See #1089.
        //   2. bare-command ENOENT with no PATH resolution → enriched
        //      Spawn message hinting at the frozen-PATH cause. See #1048.
        //   3. fallback → generic Spawn classification.
        if e.kind() == std::io::ErrorKind::NotFound && config.cwd.exists() && resolved.is_none() {
            AcpError::missing_binary_spawn_error(&e, &config.spec.command)
        } else {
            AcpError::classify_spawn_error(e, &config.cwd, &spawn_command)
        }
    })?;

    let pid = child.id();
    info!(
        target: "acp.protocol.spawn",
        command = %config.spec.command,
        pid = ?pid,
        "ACP agent subprocess started"
    );

    // Drain stderr line-by-line into the tracing log. Without this the
    // child's stderr pipe fills up at ~64KB and the agent blocks on
    // write, looking like a wedged ACP handshake. Logging every line
    // also gives us a record of what the adapter said before it died.
    if let Some(stderr) = child.stderr.take() {
        let command_label = config.spec.command.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        debug!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "{}",
                            scrub_stderr_secrets(&line),
                        );
                    }
                    Ok(None) => {
                        debug!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "stderr EOF"
                        );
                        break;
                    }
                    Err(e) => {
                        warn!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "stderr read error: {e}"
                        );
                        break;
                    }
                }
            }
        });
    } else {
        warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            pid = ?pid,
            "child has no stderr handle; agent crashes will be silent"
        );
    }

    Ok(child)
}

/// Translate the user's decision into the matching option_id from the
/// list the agent offered. Falls back gracefully if the agent didn't
/// offer the preferred kind.
fn pick_option_id(
    options: &[agent_client_protocol::schema::PermissionOption],
    decision: ApprovalDecision,
) -> Option<agent_client_protocol::schema::PermissionOptionId> {
    let preferred_kinds = match decision {
        ApprovalDecision::Allow => &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ][..],
        ApprovalDecision::AllowAlways => &[
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce,
        ][..],
        ApprovalDecision::Deny => &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ][..],
        // Synthetic decision emitted by the daemon-restart rehydration
        // sweep. Has no agent option to map to (the agent never sees
        // it); the caller falls through to `RequestPermissionOutcome::
        // Cancelled` when this returns None.
        ApprovalDecision::Cancelled => &[][..],
    };
    for kind in preferred_kinds {
        if let Some(opt) = options.iter().find(|o| &o.kind == kind) {
            return Some(opt.option_id.clone());
        }
    }
    None
}

/// True when the event would reproduce a prior turn's visible
/// transcript. Used to scope the post-`session/load` suppression
/// window: claude-agent-acp re-emits historical assistant chunks and
/// tool calls during the load handshake (which would double-render
/// against our own SQLite-restored transcript), but it ALSO emits
/// ambient state (available_commands, current_mode, usage) and
/// lifecycle events that the UI needs immediately on resume. Drop the
/// former, pass the latter through.
fn is_transcript_event(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentMessageChunk { .. }
            | Event::ToolCallStarted { .. }
            | Event::ToolCallCompleted { .. }
            | Event::ToolCallContent { .. }
            | Event::ToolCallUpdated { .. }
            | Event::DiffEmitted { .. }
            | Event::PlanUpdated { .. }
            | Event::TodoListUpdated { .. }
            | Event::ThinkingStarted
            | Event::ThinkingEnded
            | Event::UserPromptSent { .. }
            | Event::UserDiffCommentsPrompt { .. }
            | Event::ApprovalRequested { .. }
            | Event::ApprovalResolved { .. }
            | Event::RawAgentUpdate { .. }
            // A replayed launch must be dropped too: the tailer spawn is
            // skipped during suppression, so letting it through would
            // create a running record with nothing to ever complete it.
            | Event::BackgroundAgentLaunched { .. }
            | Event::PromptRuntimeError { .. }
    )
}

/// Cheap discriminant for log breadcrumbs (matches the one in
/// event_store, kept separate so this module doesn't depend on the
/// store's private helper).
fn transcript_event_kind(event: &Event) -> &'static str {
    match event {
        Event::AgentMessageChunk { .. } => "agent_message_chunk",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::ToolCallContent { .. } => "tool_call_content",
        Event::ToolCallUpdated { .. } => "tool_call_updated",
        Event::DiffEmitted { .. } => "diff_emitted",
        Event::PlanUpdated { .. } => "plan_updated",
        Event::TodoListUpdated { .. } => "todo_list_updated",
        Event::ThinkingStarted => "thinking_started",
        Event::ThinkingEnded => "thinking_ended",
        Event::UserPromptSent { .. } => "user_prompt_sent",
        Event::UserDiffCommentsPrompt { .. } => "user_diff_comments_prompt",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
        Event::PromptRuntimeError { .. } => "prompt_runtime_error",
        _ => "other",
    }
}

/// Build a `WakeupScheduled` event from a `ScheduleWakeup` tool's
/// raw_input. Reads `delaySeconds` (number, falls back to numeric
/// string) and the optional `reason`; computes the absolute wake
/// timestamp from `Utc::now()`. Returns `None` if `delaySeconds` is
/// missing or non-finite, better to skip the event than publish a
/// wakeup at epoch zero. See #1091.
fn wakeup_event_from_raw(raw_input: &serde_json::Value) -> Option<Event> {
    let Some(delay_value) = raw_input.get("delaySeconds") else {
        debug!(
            target: "acp.protocol.wakeup",
            "ScheduleWakeup raw_input missing `delaySeconds`; not emitting WakeupScheduled"
        );
        return None;
    };
    let Some(delay_secs) = delay_value
        .as_f64()
        .or_else(|| delay_value.as_str().and_then(|s| s.parse().ok()))
    else {
        debug!(
            target: "acp.protocol.wakeup",
            value = %delay_value,
            "ScheduleWakeup `delaySeconds` not numeric; not emitting WakeupScheduled"
        );
        return None;
    };
    if !delay_secs.is_finite() || delay_secs < 0.0 {
        warn!(
            target: "acp.protocol.wakeup",
            delay_secs,
            "ScheduleWakeup `delaySeconds` non-finite or negative; refusing to emit"
        );
        return None;
    }
    let delay_ms = (delay_secs * 1000.0).clamp(0.0, i64::MAX as f64) as i64;
    let at = chrono::Utc::now() + chrono::Duration::milliseconds(delay_ms);
    let reason = raw_input
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    info!(
        target: "acp.protocol.wakeup",
        delay_secs,
        wake_at = %at,
        reason = ?reason,
        "emitting WakeupScheduled from ScheduleWakeup tool args"
    );
    Some(Event::WakeupScheduled { at, reason })
}

/// Build a `MonitorArmed` event from a `Monitor` tool's raw_input. Reads
/// the optional `description` for the badge label. Returns `None` when the
/// frame carries neither `description` nor `command`: claude-agent-acp emits
/// the initial `tool_call` frame with empty args (the real args land on a
/// later `ToolCallUpdate`), and an empty frame should not arm the badge.
fn monitor_event_from_raw(raw_input: &serde_json::Value) -> Option<Event> {
    let description = raw_input
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let has_command = raw_input.get("command").and_then(|v| v.as_str()).is_some();
    if description.is_none() && !has_command {
        return None;
    }
    info!(
        target: "acp.protocol.wakeup",
        description = ?description,
        "emitting MonitorArmed from Monitor tool args"
    );
    Some(Event::MonitorArmed { description })
}

/// Derive a `LifecycleSignal::WakeupPending` from a `SessionUpdate`.
///
/// The watchdog must not suppress on every `Event::WakeupScheduled`:
/// the initial `ToolCall` frame is emitted eagerly before the tool is
/// known to have succeeded, and a later `Failed` status means no
/// wakeup was ever registered. Either case would let a real adapter
/// wedge masquerade as a pending wake and pin the prompt open for
/// `delay + base_grace`.
///
/// Gate: source must be a `ToolCallUpdate` whose status is NOT
/// `Failed` (Completed or InProgress are both acceptable; real
/// `claude-agent-acp` lands the raw_input on an interim
/// `ToolCallUpdate` before the final `Completed` arrives, and the
/// Completed update often strips `raw_input`, so requiring strictly
/// `Completed` would miss the wakeup in production). The title
/// must be `ScheduleWakeup` and `raw_input.delaySeconds` must
/// parse.
///
/// UI emit of `Event::WakeupScheduled` keeps its current best-effort
/// behavior so the sidebar countdown lights up immediately. See
/// CodeRabbit review on PR #1406.
fn wakeup_lifecycle_signal_from_update(
    update: &agent_client_protocol::schema::SessionUpdate,
    profile: &agent_profiles::AgentProfile,
) -> Option<LifecycleSignal> {
    use agent_client_protocol::schema::{SessionUpdate, ToolCallStatus};
    if !profile.supports_wakeup_tools {
        return None;
    }
    let SessionUpdate::ToolCallUpdate(u) = update else {
        return None;
    };
    if matches!(u.fields.status, Some(ToolCallStatus::Failed)) {
        return None;
    }
    if u.fields.title.as_deref() != Some("ScheduleWakeup") {
        return None;
    }
    let raw = u.fields.raw_input.as_ref()?;
    match wakeup_event_from_raw(raw)? {
        Event::WakeupScheduled { at, .. } => Some(LifecycleSignal::WakeupPending { at }),
        _ => None,
    }
}

/// Classify a notification for both watchdog lanes. Returns
/// `(lifecycle_signal, wakeup_signal)`.
///
/// During post-load history replay suppression we intentionally surface no
/// signal, so stale replay frames cannot suppress or disarm watchdogs for a
/// new prompt epoch.
fn classify_watchdog_notification_signals(
    update: &agent_client_protocol::schema::SessionUpdate,
    profile: &agent_profiles::AgentProfile,
    suppressing_history_replay: bool,
) -> (Option<LifecycleSignal>, Option<LifecycleSignal>) {
    if suppressing_history_replay {
        return (None, None);
    }
    (
        classify_lifecycle_signal(update),
        wakeup_lifecycle_signal_from_update(update, profile),
    )
}

/// Parse Claude's ExitPlanMode tool input into a structured `Plan`.
/// Claude ships the plan markdown in `raw_input.plan`; we extract its
/// bullet- or number-prefixed lines as `PlanStep`s with status=Pending,
/// matching the ACP `SessionUpdate::Plan` shape so the existing
/// PlanStrip renderer can consume it.
///
/// Returns `None` when the input has no `plan` key, the value isn't a
/// string, or the string has no recognisable list items; in which case
/// the generic tool card is still rendered so the user sees the raw
/// plan text. See #1059 for the upstream gap this works around.
fn extract_plan_from_switch_mode(raw_input: &serde_json::Value) -> Option<Plan> {
    let plan_text = raw_input.get("plan")?.as_str()?;
    let steps = parse_plan_steps(plan_text);
    if steps.is_empty() {
        return None;
    }
    Some(Plan {
        plan_id: format!("plan-{}", chrono::Utc::now().timestamp_millis()),
        version: 1,
        steps,
    })
}

/// Flatten plan markdown into `PlanStep`s. v1 heuristic: every line
/// starting with `-`, `*`, or `<digit>.` becomes one step. Sub-bullets
/// flatten into the parent list (PlanEntry has no nesting field in the
/// ACP spec). Strips bold/italic markers from the step title so the
/// PlanStrip doesn't render literal `**foo**`.
fn parse_plan_steps(text: &str) -> Vec<PlanStep> {
    use std::sync::OnceLock;
    static BULLET: OnceLock<regex::Regex> = OnceLock::new();
    let bullet = BULLET.get_or_init(|| {
        regex::Regex::new(r"^\s*(?:[-*]|\d+\.)\s+(.+?)\s*$")
            .expect("static plan-step regex must compile")
    });

    let mut steps = Vec::new();
    for line in text.lines() {
        if let Some(caps) = bullet.captures(line) {
            let raw_title = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let title = strip_markdown_emphasis(raw_title);
            if title.is_empty() {
                continue;
            }
            steps.push(PlanStep {
                id: format!("step-{}", steps.len()),
                title,
                detail: None,
                status: PlanStepStatus::Pending,
            });
        }
    }
    steps
}

fn strip_markdown_emphasis(s: &str) -> String {
    // Replace **bold**, __bold__, *italic*, _italic_ markers with their
    // inner text. Keep it permissive; the source is Claude's planning
    // markdown, which is usually well-formed but occasionally drops a
    // closing marker.
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\*\*(.+?)\*\*|__(.+?)__|\*([^*]+?)\*|_([^_]+?)_")
            .expect("static emphasis-strip regex must compile")
    });
    re.replace_all(s.trim(), |caps: &regex::Captures<'_>| {
        for i in 1..=4 {
            if let Some(m) = caps.get(i) {
                return m.as_str().to_string();
            }
        }
        String::new()
    })
    .into_owned()
}

/// Heuristic detector for the end of a `/compact` cycle. The Claude ACP
/// adapter emits "Compacting..." while the compaction runs and
/// "Compacting completed." once the model's context window has been
/// replaced by a summary; both as plain `agent_message_chunk`s with no
/// `_meta` flag (see #1050 for the upstream gap). String-matching on
/// the completion message is fragile to localisation but the wrong-firing
/// failure mode (an extra "context reset" divider) is harmless; it can
/// never destroy transcript data.
fn is_compact_completion(text: &str) -> bool {
    text.contains("Compacting completed.")
}

/// Tracks the in-flight assistant text block so claude-agent-acp's leaked
/// consolidated `agent_message_chunk` restatement can be dropped before it
/// reaches the watchdog, the event store, or any client. The adapter streams a
/// text block as incremental chunks, then re-sends the whole block as one
/// chunk; its own dedup (`streamedTextIds`) is meant to suppress that copy but
/// misses on a message-id mismatch (deterministic right after an Opus to Sonnet
/// switch, intermittent otherwise), so both reach us and every reducer appends
/// both, doubling the message. See #2281.
///
/// The adapter's `message_id` only ever marks a new message, never reuses one
/// across messages, so a same-id chunk is always a genuine delta and is never
/// dropped (a legitimately repeated delta keeps the same id, for example
/// streamed "ha" then "ha"). Any other chunk whose text restates the open
/// block's accumulated text verbatim is the leaked consolidated copy, regardless
/// of whether its id is present, absent, or merely different from the block's.
/// Matching on content rather than on both ids being present closes the
/// id-presence-asymmetry hole the adapter hits when `message_start` carries no
/// id: the streamed deltas then arrive id-less while the restatement still
/// carries a fallback uuid, so the two ids differ and the restatement is caught.
/// claude-agent-acp >=0.49.0 dedups by content upstream (#800), so this is a
/// backstop for that and for any other adapter that leaks the copy. The only
/// shape left unfiltered is both sides genuinely id-less, where a verbatim
/// repeat is indistinguishable from a leak; that degrades to the same-id append
/// path rather than risk corrupting real output.
#[derive(Default)]
struct AgentMessageDedup {
    block: Option<AgentTextBlock>,
}

struct AgentTextBlock {
    id: Option<MessageId>,
    text: String,
}

impl AgentMessageDedup {
    /// Forget any in-flight block. Called while post-load history replay is
    /// suppressed so replayed chunks cannot poison live block tracking once
    /// suppression lifts.
    fn reset(&mut self) {
        self.block = None;
    }

    /// Returns true when `update` is the leaked consolidated restatement and the
    /// whole notification should be skipped (not mapped, not emitted).
    fn observe(&mut self, update: &SessionUpdate) -> bool {
        let SessionUpdate::AgentMessageChunk(chunk) = update else {
            // Any non-message-chunk update ends the current text block. The
            // event stream never interleaves ambient updates inside a streamed
            // block, so this is a safe block terminator.
            self.block = None;
            return false;
        };
        let ContentBlock::Text(t) = &chunk.content else {
            // Non-text content (image, audio) ends the text block.
            self.block = None;
            return false;
        };
        if t.text.is_empty() {
            // The adapter emits an empty chunk at each block start; treat it as
            // the boundary so adjacent blocks never merge in the accumulator.
            // Empty text renders nothing, so keep forwarding it.
            self.block = Some(AgentTextBlock {
                id: chunk.message_id.clone(),
                text: String::new(),
            });
            return false;
        }
        match &mut self.block {
            Some(block) if block.id == chunk.message_id => {
                // Same message: a genuine streamed delta (or both ids absent).
                // Never dropped.
                block.text.push_str(&t.text);
                false
            }
            Some(block) if block.text == t.text => {
                // Arm 1 already consumed the equal-id case, so the ids differ
                // here (present-and-different, or one side absent). A non-empty
                // chunk restating the whole block verbatim under a different id
                // is the leaked consolidated copy. Drop it and close the block.
                self.block = None;
                true
            }
            _ => {
                // A genuinely new block (id changed, text differs) or no open
                // block: start tracking fresh.
                self.block = Some(AgentTextBlock {
                    id: chunk.message_id.clone(),
                    text: t.text.clone(),
                });
                false
            }
        }
    }
}

/// Map an ACP `SessionUpdate` to the structured view's typed `Event`. Variants we
/// don't yet handle pass through as `RawAgentUpdate` so UI clients can at
/// least see them; we'll narrow these as the schema stabilises.
///
/// `profile` carries per-agent gates for claude-specific synthesis
/// (subagent linkage namespace, ExitPlanMode-to-Plan, ScheduleWakeup);
/// other agents pass these through as plain tool calls.
fn map_update_to_events(
    update: SessionUpdate,
    profile: &'static agent_profiles::AgentProfile,
) -> Vec<Event> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => {
                // /compact emits a plain text chunk ("Compacting completed.")
                // and a usage_update with used=0; no typed signal. Detect
                // the literal string the adapter uses and append a typed
                // SessionContextReset event so the structured view can render a
                // divider, otherwise the silent context replacement leaves
                // the chat looking unchanged while the model's view has
                // been swapped out underneath the user. See #1050.
                let mut events = vec![Event::AgentMessageChunk {
                    text: text.text.clone(),
                }];
                if is_compact_completion(&text.text) {
                    events.push(Event::ConversationCompacted);
                    // /compact wipes the model's tool-state alongside the
                    // chat history, so any TodoWrite plan it was tracking
                    // is gone from its perspective. The structured view plan strip
                    // (PlanStrip + sidebar PlanProgressMini) lives in our
                    // own event log though, so without this clear it keeps
                    // showing a plan Claude no longer remembers; the user
                    // then asks "resolve the first task" and Claude
                    // responds "no task list." Emit an empty PlanUpdated
                    // so the UI matches the model's actual context.
                    events.push(Event::PlanUpdated {
                        plan: Plan {
                            plan_id: format!("plan-{}", chrono::Utc::now().timestamp_millis()),
                            version: 1,
                            steps: Vec::new(),
                        },
                    });
                }
                events
            }
            other => vec![raw_event(&other)],
        },
        // Replayed user turns (#2276): claude-agent-acp re-emits each prior
        // user message as a user_message_chunk during session/load. Live user
        // prompts are recorded by the send_prompt path as UserPromptSent, so
        // this only fires on replay; mapping it to UserPromptSent makes the
        // imported transcript show the user's bubbles. On a normal reattach
        // these are suppressed (already in the store) by is_transcript_event.
        SessionUpdate::UserMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => vec![Event::UserPromptSent {
                text: text.text,
                attachments: Vec::new(),
            }],
            other => vec![raw_event(&other)],
        },
        SessionUpdate::AgentThoughtChunk(_) => vec![Event::ThinkingStarted],
        SessionUpdate::ToolCall(tc) => {
            let raw_args = tc.raw_input.clone().unwrap_or(serde_json::Value::Null);
            // Empty (not the literal "null") when the agent ships no
            // raw_input, so argless tool cards render a clean empty-state.
            // See #1713.
            let args_preview = preview_optional_args(tc.raw_input.as_ref());
            let parent_tool_call_id = profile.parent_tool_use_id_from_meta(&tc.meta);
            if let Some(parent) = parent_tool_call_id.as_deref() {
                // Breadcrumb so AOE_ACP_TRACE=1 sessions can verify the
                // subagent linkage round-trip (parent Task id → child
                // tool_call id) end-to-end. See #1041 layer C.
                debug!(
                    target: "acp.protocol",
                    child = %tc.tool_call_id.0,
                    parent,
                    kind = %tool_kind_str(&tc.kind),
                    "subagent child tool_call linked to parent via _meta.claudeCode.parentToolUseId"
                );
            }
            let memory_recall = if profile.supports_memory_recall_tool() {
                extract_memory_recall(&tc.meta, &tc.locations, &tc.content)
            } else {
                None
            };
            // Codex (and any ACP agent) can attach structured file diffs to
            // the initial tool_call via `ToolCallContent::Diff`. Bridge them
            // onto the ToolCall so the edit card shows the path + preview
            // instead of "(unknown file)". See #1721.
            let diffs = extract_diffs_from_content(&tc.content);
            let tool_call = ToolCall {
                id: tc.tool_call_id.0.to_string(),
                name: tc.title.clone(),
                kind: tool_kind_str(&tc.kind),
                args_preview: args_preview.clone(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id,
                memory_recall,
                diffs,
            };
            let mut events = vec![Event::ToolCallStarted { tool_call }];
            if is_destructive(&tc.title, &args_preview) {
                debug!(target: "acp.protocol", "tool {} flagged destructive on tool_call ingest", tc.title);
            }
            // claude-agent-acp routes Claude's built-in ExitPlanMode through
            // the tool channel (kind=switch_mode, plan markdown in
            // raw_input.plan) instead of the structured SessionUpdate::Plan
            // channel. Synthesise a PlanUpdated event so the structured view's
            // PlanStrip and the rest of the plan-aware UI light up. See
            // #1059. Gated on the agent's profile so codex / opencode /
            // gemini mode switches don't spuriously emit empty Plans.
            if profile.supports_exit_plan_mode
                && matches!(tc.kind, agent_client_protocol::schema::ToolKind::SwitchMode)
            {
                if let Some(plan) = extract_plan_from_switch_mode(&raw_args) {
                    events.push(Event::PlanUpdated { plan });
                }
            }
            // The Claude Agent SDK's `ScheduleWakeup` tool sleeps the
            // session until `now + delaySeconds`, with `/loop` dynamic
            // mode self-firing a fresh prompt when the wake triggers.
            // Capture an absolute `at` timestamp here so the sidebar
            // countdown survives daemon restarts and never has to parse
            // the natural-language output string. See #1091. Gated on
            // the agent's profile (claude-only today) so coincidental
            // tool names on other agents don't fire a wakeup event.
            if profile.supports_wakeup_tools && tc.title == "ScheduleWakeup" {
                if let Some(event) = wakeup_event_from_raw(&raw_args) {
                    events.push(event);
                }
            }
            events
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let is_error = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::ToolCallStatus::Failed)
            );
            let completed = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::ToolCallStatus::Completed)
                    | Some(agent_client_protocol::schema::ToolCallStatus::Failed)
            );
            // claude-agent-acp emits the initial `tool_call` frame
            // eagerly, often well before the underlying bash / read /
            // edit actually starts running. Use `status: InProgress` as
            // the canonical "running now" signal and re-stamp the
            // tool's `started_at` so the duration label measures real
            // tool runtime rather than adapter scheduling overhead.
            // See #1060.
            let in_progress = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::ToolCallStatus::InProgress)
            );
            let content_text = update
                .fields
                .content
                .as_ref()
                .map(|blocks| extract_tool_content_text(blocks))
                .unwrap_or_default();
            // Codex emits `apply_patch` diffs on the in-progress and
            // completion updates, not only the initial tool_call. Pull any
            // Diff blocks off this frame so the edit card's path + preview
            // survive when they arrive late. `Some` here REPLACES the card's
            // diffs in the reducer; absent diff blocks stay `None` so a
            // text-only update can't wipe diffs from an earlier frame. See
            // #1721.
            let new_diffs = update.fields.content.as_ref().and_then(|blocks| {
                let diffs = extract_diffs_from_content(blocks);
                (!diffs.is_empty()).then_some(diffs)
            });
            // Drop an explicit JSON null so a late-arriving update never
            // patches the card's args with the literal "null"; leaving it
            // None means the reducer keeps whatever args it already has.
            // See #1713.
            let new_args_preview = update
                .fields
                .raw_input
                .as_ref()
                .filter(|value| !value.is_null())
                .map(preview_args);
            // Structured completion payload: media/resource blocks that the
            // text concat above drops. Only extracted on the terminal frame
            // (the card renders it once on completion). See #1818.
            let output_blocks = if completed {
                update
                    .fields
                    .content
                    .as_ref()
                    .map(|blocks| extract_tool_output_blocks(blocks))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let new_title = update.fields.title.clone();
            let mut events: Vec<Event> = Vec::new();
            if new_title.is_some()
                || new_args_preview.is_some()
                || in_progress
                || new_diffs.is_some()
            {
                events.push(Event::ToolCallUpdated {
                    tool_call_id: id.clone(),
                    title: new_title,
                    args_preview: new_args_preview,
                    started_at: if in_progress {
                        Some(chrono::Utc::now())
                    } else {
                        None
                    },
                    diffs: new_diffs,
                });
            }
            if completed {
                // An async sub-agent launch (Claude `Task` with isAsync)
                // completes immediately with the `Async agent launched
                // successfully` marker while the real work runs
                // off-protocol. Flag it so renderers draw a neutral
                // background-sub-agent card and drop the marker body
                // (which leaks an internal agent id). Same detector the
                // silent-orphan watchdog uses; this just forwards it to
                // the UI event stream.
                let async_subagent = matches!(
                    detect_off_protocol_work_completed(&update.fields.content),
                    Some(OffProtocolWorkKind::AsyncAgent)
                );
                events.push(Event::ToolCallCompleted {
                    tool_call_id: id,
                    is_error,
                    content: content_text,
                    output: output_blocks,
                    completed_at: chrono::Utc::now(),
                    async_subagent,
                });
            } else if !content_text.is_empty() {
                events.push(Event::ToolCallContent {
                    tool_call_id: id,
                    content: content_text,
                });
            } else if events.is_empty() {
                // The async sub-agent launch rides a metadata-only
                // ToolCallUpdate (`_meta.claudeCode.toolName == "Agent"`,
                // status "async_launched", no status/content/title), so it
                // lands here rather than the unknown-variant catch-all
                // below. Promote it to a typed BackgroundAgentLaunched so
                // the daemon tails the agent's transcript; otherwise pass
                // the raw payload through unchanged.
                let payload = serde_json::to_value(&update).unwrap_or(serde_json::Value::Null);
                match background_agent_launched_from_value(&payload) {
                    Some(event) => events.push(event),
                    None => events.push(Event::RawAgentUpdate { payload }),
                }
            }
            // claude-agent-acp emits the initial `tool_call` frame for
            // ScheduleWakeup with empty `raw_input`; the actual
            // `delaySeconds` lands on a subsequent `ToolCallUpdate`. The
            // emit path in the `ToolCall` branch above therefore never
            // sees real args and `wakeup_event_from_raw` returns None,
            // so re-check here when the update carries both the title
            // and a populated raw_input. See #1091. Gated on profile so
            // non-claude agents don't fire WakeupScheduled on coincidence.
            if profile.supports_wakeup_tools
                && matches!(update.fields.title.as_deref(), Some("ScheduleWakeup"))
            {
                if let Some(raw) = update.fields.raw_input.as_ref() {
                    if let Some(event) = wakeup_event_from_raw(raw) {
                        events.push(event);
                    }
                }
            }
            // The Claude SDK's `Monitor` tool is fire-and-forget: the tool
            // call completes immediately while the background watch keeps
            // running off-protocol, so the turn ends and the session sits
            // Idle while the monitor is still armed. Like ScheduleWakeup the
            // initial `tool_call` frame carries empty args; the real
            // `command` / `description` land on this update. Emit MonitorArmed
            // so the sidebar can flag the session instead of showing a plain
            // grey "idle" dot that looks dead. Gated on the same claude-only
            // profile flag as the wakeup tools.
            if profile.supports_wakeup_tools
                && matches!(update.fields.title.as_deref(), Some("Monitor"))
            {
                if let Some(raw) = update.fields.raw_input.as_ref() {
                    if let Some(event) = monitor_event_from_raw(raw) {
                        events.push(event);
                    }
                }
            }
            events
        }
        SessionUpdate::Plan(p) => {
            // Build the structured plan + a synthetic TodoWrite tool call
            // from the same entries. claude-agent-acp routes Claude's
            // TodoWrite through the structured `SessionUpdate::Plan`
            // channel (not the tool channel), so without this synthesis
            // the structured view's PlanStrip + sidebar light up but no tool
            // card ever renders; the user sees a plan appear "from
            // nowhere" and has no per-update record of which calls
            // produced which states. Emit a ToolCallStarted /
            // ToolCallCompleted pair shaped to match what the
            // TodoUpdateCard classifier in ToolCards.tsx expects
            // (`name = "TodoWrite"`, `args.todos = [...]`), one per
            // adapter update.
            // Append a session-local monotonic counter so two plan updates
            // arriving in the same millisecond don't share a synthetic ID
            // (which would collide in the acp_events row keys and
            // render as a single card instead of two).
            let ts_ms = chrono::Utc::now().timestamp_millis();
            let seq = SYNTHETIC_TOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let plan_id = format!("plan-{ts_ms}-{seq}");
            let tool_id = format!("todo-{ts_ms}-{seq}");
            let todos_json: Vec<serde_json::Value> = p
                .entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "content": e.content,
                        "status": plan_status_to_str(&e.status),
                    })
                })
                .collect();
            let args_preview = serde_json::json!({ "todos": todos_json }).to_string();
            let steps: Vec<PlanStep> = p
                .entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| PlanStep {
                    id: format!("step-{i}"),
                    title: e.content,
                    detail: None,
                    status: map_plan_status(e.status),
                })
                .collect();
            let now = chrono::Utc::now();
            vec![
                Event::ToolCallStarted {
                    tool_call: ToolCall {
                        id: tool_id.clone(),
                        name: "TodoWrite".to_string(),
                        kind: "think".to_string(),
                        args_preview,
                        started_at: now,
                        parent_tool_call_id: None,
                        memory_recall: None,
                        diffs: Vec::new(),
                    },
                },
                Event::PlanUpdated {
                    plan: Plan {
                        plan_id,
                        version: 1,
                        steps,
                    },
                },
                Event::ToolCallCompleted {
                    tool_call_id: tool_id,
                    is_error: false,
                    content: String::new(),
                    output: Vec::new(),
                    completed_at: now,
                    async_subagent: false,
                },
            ]
        }
        SessionUpdate::CurrentModeUpdate(mode_update) => {
            let id = mode_update.current_mode_id.0.to_string();
            // Emit both events: CurrentModeChanged (the real id) and
            // a best-effort ModeChanged (for the legacy enum-based
            // UI, in case that path is still used somewhere).
            // Gemini surfaces its approval modes over `gemini --acp` with the
            // gemini-cli `ApprovalMode` ids (`auto_edit`, `yolo`); fold them
            // onto the existing semantic equivalents so a Gemini session is
            // classified the same as the claude-agent-acp modes. See #1819.
            let mode = match id.as_str() {
                "default" => SessionMode::Default,
                "plan" => SessionMode::Plan,
                "accept_edits" | "acceptEdits" | "auto_edit" | "autoEdit" => {
                    SessionMode::AcceptEdits
                }
                "bypass_permissions" | "bypassPermissions" | "yolo" => {
                    SessionMode::BypassPermissions
                }
                _ => SessionMode::Default,
            };
            vec![
                Event::CurrentModeChanged {
                    current_mode_id: id,
                },
                Event::ModeChanged { mode },
            ]
        }
        SessionUpdate::UsageUpdate(u) => {
            let usage = SessionUsage {
                used: u.used,
                size: u.size,
                cost: u.cost.map(|c| UsageCost {
                    amount: c.amount,
                    currency: c.currency,
                }),
            };
            vec![Event::UsageUpdated { usage }]
        }
        SessionUpdate::AvailableCommandsUpdate(u) => {
            use agent_client_protocol::schema::AvailableCommandInput;
            let commands: Vec<AvailableCommand> = u
                .available_commands
                .into_iter()
                .map(|c| AvailableCommand {
                    name: c.name,
                    description: c.description,
                    accepts_input: matches!(c.input, Some(AvailableCommandInput::Unstructured(_))),
                })
                .collect();
            debug!(
                target: "acp.protocol",
                count = commands.len(),
                "received AvailableCommandsUpdate from agent"
            );
            vec![Event::AvailableCommandsUpdated { commands }]
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            let options: Vec<ConfigOptionDescriptor> = update
                .config_options
                .into_iter()
                .filter_map(map_acp_config_option)
                .collect();
            debug!(
                target: "acp.protocol",
                count = options.len(),
                "received ConfigOptionUpdate from agent"
            );
            vec![Event::ConfigOptionsUpdated { options }]
        }
        // Ignore agent-pushed session titles. AoE owns automatic renaming via
        // `session::smart_rename`; forwarding Claude ACP title suggestions here
        // gives the session two competing auto-title writers.
        SessionUpdate::SessionInfoUpdate(_) => Vec::new(),
        // Variants we don't have a typed mapping for yet pass through as
        // RawAgentUpdate so the UI can render best-effort and we can
        // narrow these as we go.
        other => vec![raw_event(&other)],
    }
}

/// Detect a Claude async sub-agent launch in an otherwise-unmapped ACP
/// update and build a typed `BackgroundAgentLaunched`. The launch arrives
/// as `{ _meta: { claudeCode: { toolName: "Agent", toolResponse: {
/// agentId, description, prompt, resolvedModel, outputFile, status:
/// "async_launched" } } }, toolCallId }`. Returns `None` for anything
/// else (the caller falls back to `RawAgentUpdate`). Field extraction is
/// fully defensive: a missing `agentId` is the only hard requirement.
fn background_agent_launched_from_value(v: &serde_json::Value) -> Option<Event> {
    let cc = v.get("_meta")?.get("claudeCode")?;
    if cc.get("toolName").and_then(|t| t.as_str()) != Some("Agent") {
        return None;
    }
    let tr = cc.get("toolResponse")?;
    if tr.get("status").and_then(|s| s.as_str()) != Some("async_launched") {
        return None;
    }
    let agent_id = tr.get("agentId").and_then(|s| s.as_str())?.to_string();
    let str_field = |key: &str| {
        tr.get(key)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(Event::BackgroundAgentLaunched {
        agent_id,
        tool_call_id: v
            .get("toolCallId")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        description: str_field("description"),
        prompt: str_field("prompt"),
        model: str_field("resolvedModel"),
        output_file: str_field("outputFile"),
        started_at: chrono::Utc::now(),
    })
}

/// Build a `ConfigOptionsUpdated` event from a session response's
/// `config_options`, or `None` when the response carried none (so the
/// cockpit's cached selectors persist). A present-but-empty list is a
/// real full replacement and must propagate, otherwise stale selectors
/// never clear when an adapter intentionally drops them (see #1403).
///
/// Model selection rides the generic `config_option` channel (category
/// `Model`, config id `model`): claude-agent-acp >=0.44 and the ACP
/// crate >=0.14 dropped the dedicated `session/set_model` capability in
/// favor of session config options, so there is no longer a second
/// channel to normalize. See #1403, #1820.
fn config_options_event(
    raw: Option<Vec<agent_client_protocol::schema::SessionConfigOption>>,
) -> Option<Event> {
    raw.map(|raw| Event::ConfigOptionsUpdated {
        options: raw.into_iter().filter_map(map_acp_config_option).collect(),
    })
}

/// Route a `SetConfigOption` command to `session/set_config_option` and
/// emit the resulting UI update. claude-agent-acp returns the full
/// updated config_options list in the response but does NOT emit a
/// follow-up `config_option_update` notification (see
/// acp-agent.js:1358-1410), so the success path re-emits a
/// `ConfigOptionsUpdated` snapshot from the response and the frontend
/// reducer clears pending state. The round-trip is spawned detached so
/// the command loop never blocks on it. See #1403.
fn dispatch_set_config_option(
    connection: &ConnectionTo<Agent>,
    acp_session_id: &SessionId,
    config_id: String,
    value: String,
    event_tx: mpsc::Sender<Event>,
) {
    info!(
        target: "cockpit.acp",
        "sending session/set_config_option {config_id}={value}"
    );
    let sent = connection.send_request(SetSessionConfigOptionRequest::new(
        acp_session_id.clone(),
        SessionConfigId::new(config_id.clone()),
        SessionConfigValueId::new(value.clone()),
    ));
    tokio::spawn(async move {
        match sent.block_task().await {
            Ok(resp) => {
                if let Some(event) = config_options_event(Some(resp.config_options)) {
                    let _ = event_tx.send(event).await;
                }
            }
            Err(e) => {
                let reason = format!("{e}");
                warn!(
                    target: "cockpit.acp",
                    "session/set_config_option failed: {reason}"
                );
                let _ = event_tx
                    .send(Event::ConfigOptionSwitchFailed {
                        config_id,
                        value,
                        reason,
                    })
                    .await;
            }
        }
    });
}

fn thought_level_config_id(
    options: &[agent_client_protocol::schema::SessionConfigOption],
) -> Option<agent_client_protocol::schema::SessionConfigId> {
    use agent_client_protocol::schema::{SessionConfigKind, SessionConfigOptionCategory};

    options.iter().find_map(|option| {
        if !matches!(
            option.category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        ) {
            return None;
        }
        if !matches!(option.kind, SessionConfigKind::Select(_)) {
            return None;
        }
        Some(option.id.clone())
    })
}

/// Build a structured view `ConfigOptionDescriptor` from an ACP
/// `SessionConfigOption`. Returns `None` when the option has a kind
/// the structured view does not yet render (today everything except `Select`).
/// See #1403.
fn map_acp_config_option(
    option: agent_client_protocol::schema::SessionConfigOption,
) -> Option<ConfigOptionDescriptor> {
    use agent_client_protocol::schema::{
        SessionConfigKind, SessionConfigOptionCategory, SessionConfigSelectOptions,
    };

    let category = option.category.map(|c| match c {
        SessionConfigOptionCategory::Mode => ConfigOptionCategory::Mode,
        SessionConfigOptionCategory::Model => ConfigOptionCategory::Model,
        SessionConfigOptionCategory::ThoughtLevel => ConfigOptionCategory::ThoughtLevel,
        SessionConfigOptionCategory::Other(s) => ConfigOptionCategory::Other(s),
        // The schema enum is `#[non_exhaustive]`, so this arm is required
        // to compile. Unknown category *names* arrive via the untagged
        // `Other(String)` arm above; this fires only when upstream adds a
        // genuinely new named variant we haven't mapped yet. Warn so the
        // gap is visible instead of silently surfacing a categoryless
        // option with an empty payload.
        other => {
            tracing::warn!(
                target: "acp.protocol",
                variant = ?other,
                "unknown SessionConfigOptionCategory; treating as Other(\"\"). \
                 Bump claude-agent-acp or add a match arm.",
            );
            ConfigOptionCategory::Other(String::new())
        }
    });

    // Only `Select` is rendered today; future kinds (boolean toggles
    // behind `unstable_boolean_config`) skip until the structured view grows a
    // matching widget. The schema enum is `#[non_exhaustive]` so a
    // catch-all is required.
    let select = match option.kind {
        SessionConfigKind::Select(s) => s,
        _ => return None,
    };

    let choices: Vec<ConfigOptionChoice> = match select.options {
        SessionConfigSelectOptions::Ungrouped(opts) => opts
            .into_iter()
            .map(|o| ConfigOptionChoice {
                value: o.value.0.to_string(),
                name: o.name,
                description: o.description,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .into_iter()
            .flat_map(|g| {
                g.options.into_iter().map(|o| ConfigOptionChoice {
                    value: o.value.0.to_string(),
                    name: o.name,
                    description: o.description,
                })
            })
            .collect(),
        // Catch-all for `#[non_exhaustive]` future variants.
        _ => Vec::new(),
    };

    Some(ConfigOptionDescriptor {
        id: option.id.0.to_string(),
        name: option.name,
        description: option.description,
        category: category.unwrap_or(ConfigOptionCategory::Other(String::new())),
        current_value: select.current_value.0.to_string(),
        options: choices,
    })
}

fn map_plan_status(status: agent_client_protocol::schema::PlanEntryStatus) -> PlanStepStatus {
    use agent_client_protocol::schema::PlanEntryStatus;
    match status {
        PlanEntryStatus::Pending => PlanStepStatus::Pending,
        PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
        PlanEntryStatus::Completed => PlanStepStatus::Done,
        // The schema is non-exhaustive; treat unknown variants as Pending.
        _ => PlanStepStatus::Pending,
    }
}

/// Lowercase string form of a PlanEntryStatus for the synthetic
/// TodoWrite args payload. Matches the values
/// `web/src/components/acp/ToolCards.tsx::normaliseTodoStatus`
/// accepts so the TodoUpdateCard renders the right glyph.
fn plan_status_to_str(status: &agent_client_protocol::schema::PlanEntryStatus) -> &'static str {
    use agent_client_protocol::schema::PlanEntryStatus;
    match status {
        PlanEntryStatus::Pending => "pending",
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

fn raw_event<T: serde::Serialize>(value: &T) -> Event {
    Event::RawAgentUpdate {
        payload: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
    }
}

/// Stable lowercased string form of an ACP `ToolKind`. Used to drive the
/// per-tool renderer dispatch on the web side.
fn tool_kind_str(kind: &agent_client_protocol::schema::ToolKind) -> String {
    use agent_client_protocol::schema::ToolKind;
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
    .into()
}

/// 16 KB cap on tool-call argument preview, with control chars stripped.
fn preview_args(raw: &serde_json::Value) -> String {
    let serialised = serde_json::to_string(raw).unwrap_or_default();
    let mut out = String::with_capacity(serialised.len().min(16 * 1024));
    for c in serialised.chars() {
        if out.len() >= 16 * 1024 {
            out.push_str("\u{2026}[truncated]");
            break;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Preview for an optional ACP `raw_input`. Treats both a missing field
/// (`None`) and an explicit JSON `null` as "no args provided", returning
/// an empty string. The empty string lets the UI render a dedicated
/// empty-state instead of the literal text "null" that
/// `preview_args(&Value::Null)` would otherwise produce. Gemini's
/// permission flow ships argless tool calls this way. See #1713.
fn preview_optional_args(raw: Option<&serde_json::Value>) -> String {
    match raw {
        Some(value) if !value.is_null() => preview_args(value),
        _ => String::new(),
    }
}

/// Close a permission-request tool card with a terminal error row when
/// the user denies (or no compatible option exists). Pairs the start
/// frame emitted in `handle_permission_request`; without it a denied tool
/// hangs on "running" until the turn ends. See #1713.
async fn emit_permission_denied(event_tx: &mpsc::Sender<Event>, tool_call_id: &str, content: &str) {
    let _ = event_tx
        .send(Event::ToolCallCompleted {
            tool_call_id: tool_call_id.to_string(),
            is_error: true,
            content: content.to_string(),
            output: Vec::new(),
            completed_at: chrono::Utc::now(),
            async_subagent: false,
        })
        .await;
}

/// Concat the textual portion of a tool call's `content` array. Drops
/// non-text content blocks (images, resources, embedded terminals); the
/// per-tool renderer fall-back path only knows how to display text. Diff
/// blocks are bridged separately by `extract_diffs_from_content`.
fn extract_tool_content_text(blocks: &[agent_client_protocol::schema::ToolCallContent]) -> String {
    use agent_client_protocol::schema::ToolCallContent;
    let mut out = String::new();
    for block in blocks {
        if let ToolCallContent::Content(c) = block {
            if let ContentBlock::Text(t) = &c.content {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&t.text);
            }
        }
    }
    out
}

/// Max base64 length kept for an inline image/audio payload. Media this
/// large is persisted in the event store and reshipped on every WS replay,
/// so an oversized blob would bloat both; past the cap the inline data is
/// dropped (a placeholder/uri is surfaced instead). ~4 MiB of base64 is
/// ~3 MiB of bytes, comfortably above a typical screenshot.
const MAX_INLINE_MEDIA_B64: usize = 4 * 1024 * 1024;

/// Bridge an ACP `ToolCallContent` array into the cockpit's renderable
/// `ToolOutputBlock` list, preserving non-text completion payloads (images,
/// audio, resource links/contents) that `extract_tool_content_text` drops.
/// Diff blocks are bridged separately (`extract_diffs_from_content`) and are
/// skipped here; an embedded terminal surfaces as a text placeholder since
/// cockpit does not own ACP terminals. Returns an EMPTY vec when every block
/// is plain text (or diff): the existing `content` text path renders those,
/// so the structured list only carries weight when real media is present.
/// See #1818.
fn extract_tool_output_blocks(
    blocks: &[agent_client_protocol::schema::ToolCallContent],
) -> Vec<ToolOutputBlock> {
    use agent_client_protocol::schema::{EmbeddedResourceResource, ToolCallContent};
    let mut out: Vec<ToolOutputBlock> = Vec::new();
    let mut has_media = false;
    let cap =
        |data: String| -> Option<String> { (data.len() <= MAX_INLINE_MEDIA_B64).then_some(data) };
    for block in blocks {
        match block {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Text(t) => out.push(ToolOutputBlock::Text {
                    text: t.text.clone(),
                }),
                ContentBlock::Image(img) => {
                    has_media = true;
                    out.push(ToolOutputBlock::Image {
                        mime_type: img.mime_type.clone(),
                        data: cap(img.data.clone()),
                        uri: img.uri.clone(),
                    });
                }
                ContentBlock::Audio(audio) => {
                    has_media = true;
                    out.push(ToolOutputBlock::Audio {
                        mime_type: audio.mime_type.clone(),
                        data: cap(audio.data.clone()),
                    });
                }
                ContentBlock::ResourceLink(link) => {
                    has_media = true;
                    out.push(ToolOutputBlock::ResourceLink {
                        uri: link.uri.clone(),
                        name: link.name.clone(),
                        mime_type: link.mime_type.clone(),
                    });
                }
                ContentBlock::Resource(res) => {
                    has_media = true;
                    let block = match &res.resource {
                        EmbeddedResourceResource::TextResourceContents(t) => {
                            ToolOutputBlock::Resource {
                                uri: t.uri.clone(),
                                mime_type: t.mime_type.clone(),
                                text: Some(t.text.clone()),
                                data: None,
                            }
                        }
                        EmbeddedResourceResource::BlobResourceContents(b) => {
                            // Keep the inline bytes (capped) so a blob without
                            // a fetchable uri is still recoverable as a
                            // download instead of an empty placeholder. See
                            // #1818 review.
                            ToolOutputBlock::Resource {
                                uri: b.uri.clone(),
                                mime_type: b.mime_type.clone(),
                                text: None,
                                data: cap(b.blob.clone()),
                            }
                        }
                        _ => continue,
                    };
                    out.push(block);
                }
                _ => {}
            },
            ToolCallContent::Terminal(term) => {
                has_media = true;
                out.push(ToolOutputBlock::Text {
                    text: format!("[terminal {}]", term.terminal_id.0),
                });
            }
            ToolCallContent::Diff(_) => {}
            _ => {}
        }
    }
    if has_media {
        out
    } else {
        Vec::new()
    }
}

/// Inspect a `tool_call` payload for the `memory_recall` shape
/// claude-agent-acp v0.37.0 routes through the tool channel (upstream
/// #703). The adapter sends `_meta.claudeCode.toolName == "memory_recall"`
/// plus either `locations` (recall mode, one entry per loaded memory
/// file) or `content` (synthesize mode, one text block with the
/// synthesised reply). Returns `None` when the meta marker is absent.
/// Caller gates this on `AgentProfile::supports_memory_recall_tool`
/// so unrelated agents that happen to share field shapes don't trip
/// the classifier.
fn extract_memory_recall(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
    locations: &[agent_client_protocol::schema::ToolCallLocation],
    content: &[agent_client_protocol::schema::ToolCallContent],
) -> Option<MemoryRecall> {
    let map = meta.as_ref()?;
    let claude_code = map.get("claudeCode")?;
    let tool_name = claude_code.get("toolName").and_then(|v| v.as_str())?;
    if tool_name != "memory_recall" {
        return None;
    }
    let mode = claude_code
        .get("toolResponse")
        .and_then(|tr| tr.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("recall")
        .to_string();
    let paths: Vec<String> = locations
        .iter()
        .map(|loc| loc.path.to_string_lossy().to_string())
        .collect();
    let synthesized_text = if mode == "synthesize" {
        let text = extract_tool_content_text(content);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        None
    };
    Some(MemoryRecall {
        mode,
        paths,
        synthesized_text,
    })
}

/// Max bytes of diff text kept per side (old/new) when bridging an ACP
/// `ToolCallContent::Diff` into a structured view `DiffPreview`. The card only
/// previews ~20 lines, but the untrimmed text is persisted in the event
/// store and shipped over every WS replay frame, so a large `apply_patch`
/// would bloat both without a cap here. Mirrors `preview_args`' 16 KB ceiling.
const MAX_DIFF_TEXT_BYTES: usize = 16 * 1024;

/// Max number of per-file diffs kept from a single tool call. A patch
/// touching more files than this keeps the first `MAX_TOOL_DIFFS` rather
/// than letting one event grow unbounded.
const MAX_TOOL_DIFFS: usize = 16;

/// Truncate diff text to `MAX_DIFF_TEXT_BYTES` on a UTF-8 char boundary,
/// appending a sentinel so the cut reads as intentional rather than as a
/// corrupt diff.
fn cap_diff_text(text: &str) -> String {
    if text.len() <= MAX_DIFF_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_DIFF_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str("\n\u{2026}[truncated]");
    out
}

/// Bridge ACP `ToolCallContent::Diff` blocks into structured view `DiffPreview`
/// entries. Codex routes `apply_patch` edits through this channel (one
/// block per touched file) instead of the legacy `old_string`/`new_string`
/// raw_input keys, so the edit card reads the path and +/- preview from
/// here. Non-diff blocks (text, images, terminals) are ignored; the enum
/// is `#[non_exhaustive]`, so the wildcard arm keeps this compiling as the
/// schema grows. Per-side text is capped and the list bounded. See #1721.
fn extract_diffs_from_content(
    blocks: &[agent_client_protocol::schema::ToolCallContent],
) -> Vec<DiffPreview> {
    use agent_client_protocol::schema::ToolCallContent;
    let created_at = chrono::Utc::now();
    blocks
        .iter()
        .filter_map(|block| match block {
            ToolCallContent::Diff(d) => Some(DiffPreview {
                path: d.path.to_string_lossy().to_string(),
                old_text: d.old_text.as_deref().map(cap_diff_text),
                new_text: Some(cap_diff_text(&d.new_text)),
                created_at,
            }),
            _ => None,
        })
        .take(MAX_TOOL_DIFFS)
        .collect()
}

/// Dispatch the experimental `session/delete` RPC from the connect
/// task's cmd_rx arm. The wait is detached via `tokio::spawn` so the
/// cmd_rx select arm keeps polling other commands during the
/// round-trip; the outcome is delivered to the caller via the
/// `respond_to` oneshot. Bounded by `ACP_SESSION_DELETE_TIMEOUT` so a
/// wedged adapter still resolves the oneshot in time for the caller's
/// outer guard. See #1404.
fn handle_delete_session_cmd(
    connection: &ConnectionTo<Agent>,
    acp_session_id: String,
    respond_to: oneshot::Sender<DeleteSessionOutcome>,
) {
    let target = agent_client_protocol::schema::SessionId::from(acp_session_id);
    // `block_task()` is documented as safe to await from a spawned
    // task: it waits on the per-request oneshot the main connection
    // task feeds via its inbound pump, so the dispatch loop keeps
    // running while this future is parked. The spawn here keeps the
    // cmd_rx select arm responsive to other commands during the
    // round-trip, mirroring the pattern used by SetMode /
    // SetConfigOption.
    let sent = connection.send_request(DeleteSessionRequest {
        session_id: target,
        meta: serde_json::Value::Object(serde_json::Map::new()),
    });
    tokio::spawn(async move {
        let outcome =
            match tokio::time::timeout(ACP_SESSION_DELETE_TIMEOUT, sent.block_task()).await {
                Ok(Ok(_resp)) => DeleteSessionOutcome::Deleted,
                Ok(Err(err)) => {
                    if err.code == ErrorCode::MethodNotFound {
                        DeleteSessionOutcome::UnsupportedMethod
                    } else {
                        // Adapter error messages reach debug.log
                        // verbatim. Run them through the existing
                        // stderr secret scrubber so a leaked
                        // `sk-...` / `Bearer ...` / GitHub PAT in
                        // the adapter's own error string doesn't
                        // land in operator logs, then cap length.
                        let scrubbed = scrub_stderr_secrets(&err.message);
                        DeleteSessionOutcome::Failed(format!(
                            "acp error {}: {}",
                            i32::from(err.code),
                            truncate_for_log(&scrubbed, ACP_DELETE_ERROR_MSG_MAX)
                        ))
                    }
                }
                Err(_) => DeleteSessionOutcome::TimedOut,
            };
        let _ = respond_to.send(outcome);
    });
}

/// Defensive truncation of adapter-provided strings before they land
/// in `debug.log`. A malformed or malicious adapter could emit a
/// multi-megabyte message; this caps the allocation while preserving
/// a useful prefix and respecting UTF-8 boundaries.
fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}

/// Whether the agent advertised the given mode ID in its session modes
/// or config-option mode category.
///
/// Returns `false` (skip) when:
/// - `available_mode_ids` is `Some` and the normalized mode_id is not in the list, or
/// - `available_mode_ids` is `None` and the agent uses config-option modes
///   (session/set_mode won't work for arbitrary mode IDs).
///
/// Returns `true` (allow) only when there is no mode information at all
/// (e.g. the test shim, which handles all set_mode requests).
fn is_mode_advertised(
    mode_id: &str,
    available_mode_ids: &Option<Vec<String>>,
    has_config_option_mode: bool,
) -> bool {
    match available_mode_ids {
        Some(ids) => {
            let normalized = mode_id.replace('_', "").to_lowercase();
            ids.iter()
                .any(|id| id.replace('_', "").to_lowercase() == normalized)
        }
        None => !has_config_option_mode,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connection_task<W, R>(
    transport: ByteStreams<W, R>,
    event_tx: mpsc::Sender<Event>,
    cmd_rx: mpsc::Receiver<ClientCmd>,
    cwd: PathBuf,
    session_label: String,
    child: Option<Arc<Mutex<tokio::process::Child>>>,
    pending_responders: PendingResponders,
    resources: SessionResources,
    socket_path: Option<PathBuf>,
    mode: ConnectMode,
    ready_tx: Option<oneshot::Sender<Result<(), AcpError>>>,
    profile: &'static agent_profiles::AgentProfile,
    expected_agent: ExpectedAgent,
    source_profile: Option<String>,
    default_effort: Option<String>,
    mcp_servers: Vec<McpServer>,
) where
    W: futures_util::AsyncWrite + Send + 'static,
    R: futures_util::AsyncRead + Send + 'static,
{
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    let ready_tx = Arc::new(Mutex::new(ready_tx));
    let ready_for_block = ready_tx.clone();
    let event_tx_for_notif = event_tx.clone();
    let event_tx_for_perm = event_tx.clone();
    let event_tx_for_elicit = event_tx.clone();
    let event_tx_for_block = event_tx.clone();
    let pending_for_perm = pending_responders.clone();
    let pending_for_elicit = pending_responders.clone();
    let tool_context_cache: ToolContextCache =
        Arc::new(std::sync::Mutex::new(ToolCallContextCache::default()));
    let tool_context_cache_for_notif = tool_context_cache.clone();
    let tool_context_cache_for_perm = tool_context_cache.clone();
    let mut cmd_rx = cmd_rx;
    let session_label_for_log = session_label.clone();

    // Silent-orphan watchdog plumbing. The notification handler
    // classifies each inbound `SessionUpdate` into a `LifecycleSignal`
    // (or `None` for ambient state like mode/available_commands) and
    // sends it over a dedicated mpsc to the prompt loop, which owns the
    // `Instant` timers and the in-flight tool map. Keeping the timer
    // state inside the prompt loop avoids the cross-task contention of
    // a shared atomic and scopes liveness cleanly to the current
    // prompt. See #1240.
    //
    // Signals are wrapped in `LifecycleEnvelope { epoch, signal }`
    // tagged with the prompt epoch that was current at signal-
    // construction time. The prompt loop increments
    // `current_prompt_epoch` before issuing each `session/prompt` and
    // discards envelopes whose epoch is not the current one. This
    // makes the awaited `send` paths safe across prompt boundaries:
    // a notification handler parked on a full channel from the
    // previous prompt cannot leak its stale signal into the next
    // prompt's watchdog state when it eventually wakes up. See #1401
    // post-impl review.
    let (lifecycle_signal_tx, lifecycle_signal_rx) = mpsc::channel::<LifecycleEnvelope>(128);
    let lifecycle_signal_tx_for_notif = lifecycle_signal_tx.clone();
    let mut lifecycle_signal_rx = lifecycle_signal_rx;
    let current_prompt_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let current_prompt_epoch_for_notif = current_prompt_epoch.clone();
    let res_read = resources.clone();
    let res_write = resources.clone();
    let res_term_create = resources.clone();
    let res_term_output = resources.clone();
    let res_term_wait = resources.clone();
    let res_term_kill = resources.clone();
    let res_term_release = resources.clone();

    // After a successful `session/load`, claude-agent-acp re-emits the
    // full prior transcript as `session/update` notifications (each
    // historical assistant turn replayed as agent_message_chunk
    // events). Our SQLite event store already has those events from
    // the original run, so passing them through would double the
    // transcript on the next reload; every prior assistant bubble
    // appears once from disk replay, then again from the agent's
    // history dump. Suppress agent-side notifications during the
    // window between session/load success and the first user prompt;
    // cleared on the first ClientCmd::Prompt below.
    let suppress_history_replay = Arc::new(AtomicBool::new(false));
    let suppress_for_notif = suppress_history_replay.clone();
    let suppress_for_block = suppress_history_replay.clone();
    let session_label_for_notif = session_label.clone();

    // Watchdog inputs (only consulted when `mode` is `Resume { in_flight_turn: true }`):
    //   - `last_event_at`: epoch-ms of the last inbound notification.
    //     Updated by the notification handler below. Initialized to "now"
    //     so a session that never receives a single notification still
    //     fires Stopped after RESUME_IDLE_GRACE rather than immediately.
    //   - `first_event_after_attach`: set true on the first inbound
    //     lifecycle-bearing notification after attach (progress, tool
    //     lifecycle, terminal usage, wakeup). Ambient updates like mode
    //     or available-command refreshes do not prove turn progress, so
    //     they must not disarm the watchdog.
    //   - `prompt_sent_since_attach`: set when the user issues a prompt
    //     after attach; the user's real PromptRequest will own the next
    //     Stopped, so the watchdog must stand down.
    //   - `watchdog_fired`: ensures we synthesize Stopped at most once.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let last_event_at = Arc::new(AtomicI64::new(now_ms));
    let first_event_after_attach = Arc::new(AtomicBool::new(false));
    let prompt_sent_since_attach = Arc::new(AtomicBool::new(false));
    let watchdog_fired = Arc::new(AtomicBool::new(false));
    // Between-prompt idle watchdog state (#2325). Tracks an agent-initiated
    // turn (Monitor / scheduled-wake resume) that runs with no aoe-issued
    // `session/prompt`, so the outer command loop's idle tick can synthesize
    // its terminal Stopped. `last_lifecycle_at` is updated only on transcript
    // progress (NOT ambient AvailableCommandsUpdate), so periodic
    // command-list refreshes can't keep resetting the idle timer.
    let last_lifecycle_at = Arc::new(AtomicI64::new(now_ms));
    let between_prompt_active = Arc::new(AtomicBool::new(false));
    let between_prompt_cost_seen = Arc::new(AtomicBool::new(false));
    // Wake `at` (ms) of the latest pending scheduled wake, 0 when none.
    let between_prompt_wake_at = Arc::new(AtomicI64::new(0));
    // In-flight tool calls for the between-prompt (agent-initiated) path,
    // keyed by tool_call_id -> the `run_in_background` flag observed at
    // ToolStarted. Mirrors the per-prompt SilentOrphanWatchdog's
    // `tool_calls_in_flight` map (by id, not a count, so duplicate or
    // unmatched completions cannot drift the state). See #2371.
    let between_prompt_tools = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        bool,
    >::new()));
    // Latched true when a successful tool completion carried an off-protocol
    // marker OR its launch set `run_in_background`: the work keeps running
    // after the ToolCall completes, so the watchdog holds the floor.
    let between_prompt_off_protocol = Arc::new(AtomicBool::new(false));
    let prompt_in_flight = Arc::new(AtomicBool::new(false));
    let last_event_at_for_notif = last_event_at.clone();
    let first_event_after_attach_for_notif = first_event_after_attach.clone();
    let last_lifecycle_at_for_notif = last_lifecycle_at.clone();
    let between_prompt_active_for_notif = between_prompt_active.clone();
    let between_prompt_cost_seen_for_notif = between_prompt_cost_seen.clone();
    let between_prompt_wake_at_for_notif = between_prompt_wake_at.clone();
    let between_prompt_tools_for_notif = between_prompt_tools.clone();
    let between_prompt_off_protocol_for_notif = between_prompt_off_protocol.clone();
    let prompt_in_flight_for_notif = prompt_in_flight.clone();

    // Per-session tracker that drops claude-agent-acp's leaked consolidated
    // agent_message_chunk restatement before it doubles the rendered message.
    // See AgentMessageDedup and #2281. std Mutex (not tokio) so the critical
    // section stays synchronous and the guard never crosses an await.
    let agent_msg_dedup = Arc::new(std::sync::Mutex::new(AgentMessageDedup::default()));
    let agent_msg_dedup_for_notif = agent_msg_dedup.clone();
    // The prompt loop resets the deduper at turn boundaries (a new prompt, and
    // the turn's terminal Stopped). Turn completion is not a SessionUpdate, so
    // without this an open text block could survive into the next turn and a
    // new turn that legitimately reuses the prior turn's trailing text under a
    // fresh message_id would be misclassified as a restatement. See #2281.
    let agent_msg_dedup_for_block = agent_msg_dedup.clone();

    let result = Client
        .builder()
        .name("aoe-acp")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let event_tx = event_tx_for_notif.clone();
                let suppress = suppress_for_notif.clone();
                let session_label = session_label_for_notif.clone();
                let last_event_at = last_event_at_for_notif.clone();
                let first_event_after_attach =
                    first_event_after_attach_for_notif.clone();
                let lifecycle_signal_tx = lifecycle_signal_tx_for_notif.clone();
                let current_prompt_epoch = current_prompt_epoch_for_notif.clone();
                let agent_msg_dedup = agent_msg_dedup_for_notif.clone();
                let last_lifecycle_at = last_lifecycle_at_for_notif.clone();
                let between_prompt_active = between_prompt_active_for_notif.clone();
                let between_prompt_cost_seen =
                    between_prompt_cost_seen_for_notif.clone();
                let between_prompt_wake_at =
                    between_prompt_wake_at_for_notif.clone();
                let between_prompt_tools = between_prompt_tools_for_notif.clone();
                let between_prompt_off_protocol =
                    between_prompt_off_protocol_for_notif.clone();
                let prompt_in_flight = prompt_in_flight_for_notif.clone();
                let tool_context_cache = tool_context_cache_for_notif.clone();
                async move {
                    last_event_at
                        .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
                    let suppressing = suppress.load(Ordering::Relaxed);
                    // Drop claude-agent-acp's leaked consolidated
                    // agent_message_chunk restatement before it reaches the
                    // watchdog, the event store, or any client (#2281). During
                    // post-load history replay the deduper is reset rather than
                    // fed, so replayed chunks can't poison live block tracking.
                    {
                        let mut dedup = agent_msg_dedup
                            .lock()
                            .expect("agent message dedup mutex poisoned");
                        if suppressing {
                            dedup.reset();
                        } else if dedup.observe(&notification.update) {
                            debug!(
                                target: "acp.protocol",
                                session = %session_label,
                                "dropping leaked consolidated agent_message_chunk restatement (#2281)"
                            );
                            return Ok(());
                        }
                    }
                    // Snapshot the prompt epoch ONCE per notification so
                    // every signal derived from this update shares the
                    // same epoch. If the prompt loop bumps the atomic
                    // between the classifier call and the send, the
                    // envelope's epoch reflects the prompt the signal
                    // semantically belongs to (the one current when
                    // the notification arrived), not the one that
                    // started racing it.
                    let envelope_epoch =
                        current_prompt_epoch.load(Ordering::Relaxed);
                    // Classify watchdog signals before consuming
                    // `notification.update` in the event mapping below.
                    // During post-load replay suppression this returns no
                    // signal so stale chunks from a prior turn cannot
                    // influence the current prompt's watchdog state.
                    let (lifecycle_signal, wakeup_signal) =
                        classify_watchdog_notification_signals(
                            &notification.update,
                            profile,
                            suppressing,
                        );
                    // Disarm resume-idle only on lifecycle-bearing
                    // notifications (progress/tool/terminal/wakeup). Pure
                    // ambient updates (mode, command list, metadata) are
                    // not proof of in-flight turn progress.
                    if lifecycle_signal.is_some() || wakeup_signal.is_some() {
                        first_event_after_attach.store(true, Ordering::Relaxed);
                    }
                    // Between-prompt idle tracking (#2325). Only while no
                    // aoe-issued prompt is in flight: a lifecycle signal here
                    // means the agent resumed itself (Monitor / scheduled
                    // wake), a turn the per-prompt watchdog never sees. Mirror
                    // its cost/progress/wake semantics so the outer loop's
                    // idle tick applies the same grace. During a real prompt
                    // the per-prompt watchdog owns this, so skip.
                    if !prompt_in_flight.load(Ordering::Relaxed) {
                        let now = chrono::Utc::now().timestamp_millis();
                        if let Some(u) = between_prompt_signal_update(
                            lifecycle_signal.as_ref(),
                            wakeup_signal.as_ref(),
                            now,
                            between_prompt_wake_at.load(Ordering::Relaxed),
                        ) {
                            between_prompt_active.store(true, Ordering::Relaxed);
                            between_prompt_cost_seen.store(u.cost_seen, Ordering::Relaxed);
                            // Refresh from `now` on every tracked signal,
                            // including TerminalUsage, so the fast grace
                            // measures from when the turn wrapped up rather
                            // than from a possibly-stale earlier progress
                            // event. See #2325 review.
                            last_lifecycle_at.store(u.last_lifecycle_at, Ordering::Relaxed);
                            between_prompt_wake_at.store(u.wake_at, Ordering::Relaxed);
                        }
                        // Track in-flight tool calls and off-protocol work for
                        // the between-prompt path so the idle watchdog never
                        // fires while a tool is open or backgrounded work is
                        // still running. Mirrors the per-prompt watchdog's
                        // `tool_calls_in_flight` + `off_protocol_work_seen`.
                        // See #2371, #1401.
                        match lifecycle_signal.as_ref() {
                            Some(LifecycleSignal::ToolStarted {
                                id,
                                is_background_task,
                            }) => {
                                let mut tools = between_prompt_tools
                                    .lock()
                                    .expect("between-prompt tools mutex poisoned");
                                let entry = tools.entry(id.clone()).or_insert(false);
                                *entry = *entry || *is_background_task;
                            }
                            Some(LifecycleSignal::ToolCompleted {
                                id,
                                succeeded,
                                off_protocol_work,
                            }) => {
                                let was_background = between_prompt_tools
                                    .lock()
                                    .expect("between-prompt tools mutex poisoned")
                                    .remove(id)
                                    .unwrap_or(false);
                                // A failed launch keeps no background work
                                // running, so it must not pin the floor.
                                if *succeeded
                                    && (off_protocol_work.is_some() || was_background)
                                {
                                    between_prompt_off_protocol
                                        .store(true, Ordering::Relaxed);
                                }
                            }
                            _ => {}
                        }
                    }
                    let update_for_tool_context = notification.update.clone();
                    let mapped_events = map_update_to_events(notification.update, profile);
                    // Deliver lifecycle signals BEFORE publishing the
                    // user-visible event vector. The watchdog uses
                    // ToolStarted / ToolCompleted / WakeupPending /
                    // TerminalUsage to decide whether to fire; if
                    // `event_tx.send().await` backpressures (slow web
                    // consumer, replay drain), the prompt-loop tick
                    // could otherwise evaluate `should_fire` before
                    // ever seeing the suppression-bearing signal and
                    // cancel a legitimate wait. Watchdog correctness
                    // wins; UI ordering is reconciled by the event
                    // store's monotonic seq anyway. See #1401 post-
                    // impl review.
                    if let Some(sig) = lifecycle_signal {
                        send_lifecycle_signal(
                            &lifecycle_signal_tx,
                            LifecycleEnvelope {
                                epoch: envelope_epoch,
                                signal: sig,
                            },
                            &session_label,
                        )
                        .await;
                    }
                    if let Some(sig) = wakeup_signal {
                        send_lifecycle_signal(
                            &lifecycle_signal_tx,
                            LifecycleEnvelope {
                                epoch: envelope_epoch,
                                signal: sig,
                            },
                            &session_label,
                        )
                        .await;
                    }
                    for event in mapped_events {
                        // An async sub-agent launch: spawn a tailer that
                        // follows the agent's on-disk transcript and emits
                        // BackgroundAgent{Progress,Completed}. The tailer
                        // owns its own lifecycle (self-terminates on
                        // completion, hard-idle, or when event_tx closes),
                        // so it can never outlive the session. Skipped on
                        // replay (the agent already finished). See
                        // src/acp/background_agent.rs.
                        if let Event::BackgroundAgentLaunched {
                            agent_id,
                            output_file,
                            ..
                        } = &event
                        {
                            if !suppressing && !output_file.is_empty() {
                                crate::acp::background_agent::spawn_tailer(
                                    agent_id.clone(),
                                    output_file.clone(),
                                    event_tx.clone(),
                                );
                            }
                        }
                        // During the post-load replay window, drop only
                        // events that would reproduce the prior turns'
                        // visible transcript (assistant chunks, tool
                        // calls, plans, etc.). Ambient state events
                        // (mode/usage/available_commands) and lifecycle
                        // events (stopped, errors) must pass through;
                        // otherwise the composer footer and pickers
                        // stay stale until the user types something.
                        if suppressing && is_transcript_event(&event) {
                            debug!(
                                target: "acp.protocol",
                                session = %session_label,
                                kind = transcript_event_kind(&event),
                                "dropping post-load history-replay event"
                            );
                            continue;
                        }
                        update_tool_context_cache(
                            &tool_context_cache,
                            &event,
                            &update_for_tool_context,
                        );
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _conn| {
                let event_tx = event_tx_for_perm.clone();
                let pending = pending_for_perm.clone();
                let tool_context_cache = tool_context_cache_for_perm.clone();
                async move {
                    handle_permission_request(
                        request,
                        responder,
                        event_tx,
                        pending,
                        profile,
                        tool_context_cache,
                    )
                    .await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: CreateElicitationRequest,
                  responder: Responder<CreateElicitationResponse>,
                  _conn| {
                let event_tx = event_tx_for_elicit.clone();
                let pending = pending_for_elicit.clone();
                async move {
                    handle_elicitation_request(request, responder, event_tx, pending).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: ReadTextFileRequest,
                  responder: Responder<ReadTextFileResponse>,
                  _conn| {
                let res = res_read.clone();
                async move { handle_read_text_file(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: WriteTextFileRequest,
                  responder: Responder<WriteTextFileResponse>,
                  _conn| {
                let res = res_write.clone();
                async move { handle_write_text_file(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: CreateTerminalRequest,
                  responder: Responder<CreateTerminalResponse>,
                  _conn| {
                let res = res_term_create.clone();
                async move { handle_create_terminal(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: TerminalOutputRequest,
                  responder: Responder<TerminalOutputResponse>,
                  _conn| {
                let res = res_term_output.clone();
                async move { handle_terminal_output(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: WaitForTerminalExitRequest,
                  responder: Responder<WaitForTerminalExitResponse>,
                  _conn| {
                let res = res_term_wait.clone();
                async move { handle_wait_for_terminal_exit(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: KillTerminalRequest,
                  responder: Responder<KillTerminalResponse>,
                  _conn| {
                let res = res_term_kill.clone();
                async move { handle_kill_terminal(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: ReleaseTerminalRequest,
                  responder: Responder<ReleaseTerminalResponse>,
                  _conn| {
                let res = res_term_release.clone();
                async move { handle_release_terminal(request, responder, res).await }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            info!(target: "acp.protocol", session = %session_label, "initializing ACP agent");
            let capabilities = ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(true)
                    .write_text_file(true))
                .terminal(true)
                // Advertise form-mode elicitation so claude-agent-acp
                // (>=0.44) re-enables AskUserQuestion and routes it to us as
                // an `elicitation/create` request. Without this the adapter
                // unconditionally blacklists the tool. See handle_elicitation_request.
                .elicitation(
                    ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
                );
            // `initialize` is sent in both Fresh and Resume modes.
            // It's idempotent on every ACP agent we ship against
            // (aoe-agent, claude-agent-acp); the response only carries
            // capability metadata; so re-sending it on attach is safe.
            let init = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(capabilities),
                )
                .block_task()
                .await?;

            // Per-adapter compatibility check (see src/acp/agent_compat.rs).
            // Currently only gates claude-agent-acp at >=0.37.0; other
            // adapters pass through. On rejection: route the structured
            // detail through the typed `AcpError::IncompatibleAgent`
            // variant on ready_tx so the supervisor sees it on the
            // spawn-failure path. The supervisor mirrors the detail into
            // `Event::IncompatibleAgent` + `Event::AgentStartupError`
            // through the broadcast sink (the in-process `event_tx`
            // here is dropped on the floor when spawn() returns Err, so
            // any events emitted from this closure would never reach the
            // reducer). The supervisor also terminates the detached
            // runner; we close the connection cleanly via `return Ok(())`
            // so the outer cleanup at line ~3500 doesn't double-emit an
            // AgentStartupError on top of the structured one.
            if let Err(err) = agent_compat::validate(expected_agent, &init) {
                let user_message = err.user_message();
                warn!(
                    target: "acp.protocol",
                    session = %session_label,
                    kind = err.kind(),
                    message = %user_message,
                    "agent compatibility check failed; refusing to enter session"
                );
                let detail = StartupErrorDetail::from(&err);
                if let Some(tx) = ready_for_block.lock().await.take() {
                    let _ = tx.send(Err(AcpError::IncompatibleAgent(Box::new(
                        IncompatibleAgentError {
                            detail,
                            message: user_message,
                        },
                    ))));
                }
                return Ok(());
            }

            let load_session_capable = init.agent_capabilities.load_session;
            // Surface the agent's prompt capabilities to the structured view so
            // the web composer can gate the attachment button on the
            // current agent, and the server prompt handler can reject
            // attachments the agent cannot accept. `initialize` runs on
            // both Fresh and Resume connects, so this re-emits on every
            // reconnect and replay always carries a current copy. See
            // #1000 / #965.
            let prompt_caps = &init.agent_capabilities.prompt_capabilities;
            let _ = event_tx_for_block
                .send(Event::PromptCapabilities {
                    image: prompt_caps.image,
                    audio: prompt_caps.audio,
                    embedded_context: prompt_caps.embedded_context,
                })
                .await;
            // Snapshot the watchdog-arming flag before `mode` is moved
            // into the match below.
            let arm_resume_watchdog = matches!(
                &mode,
                ConnectMode::Resume {
                    in_flight_turn: true,
                    ..
                }
            );
            info!(
                target: "acp.protocol",
                session = %session_label,
                load_session_capable,
                ?mode,
                "initialize handshake complete"
            );

            // Signal handshake-ready now: the ACP `initialize` handshake (what
            // the spawn timeout actually bounds) is done. session/new and
            // session/load run below and stream their results as events; for a
            // resumed/imported session the adapter replays the whole transcript
            // before answering session/load, which can take far longer than the
            // handshake timeout. Firing ready here keeps that replay out of the
            // timeout window (the events still reach the UI as they arrive), so
            // importing or resuming a large conversation no longer times out
            // and gets the worker killed. A later session/new failure surfaces
            // as an AgentStartupError event instead of a spawn() error. See
            // #2276.
            if let Some(tx) = ready_for_block.lock().await.take() {
                let _ = tx.send(Ok(()));
            }

            // Track the mode channels the agent advertised so we can skip
            // session/set_mode requests for modes the agent doesn't support.
            // When new_session.modes is present, only those IDs are valid.
            // When config-options have a Mode category, the agent uses
            // session/set_config_option for modes instead of session/set_mode.
            // If neither is present (e.g. test shim), allow all set_mode
            // requests through.
            let mut available_mode_ids: Option<Vec<String>> = None;
            let mut has_config_option_mode: bool = false;

            // Drop any http/sse servers the agent did not advertise before they
            // reach session/new or session/load; stdio is always kept. Computed
            // once here so both the load-attempt and the fresh-session fallback
            // forward the same gated list.
            let mcp_servers = mcp_config::filter_for_capabilities(
                mcp_servers,
                &init.agent_capabilities.mcp_capabilities,
                &session_label,
            );

            let acp_session_id: SessionId = match mode {
                ConnectMode::Resume {
                    acp_session_id: stored,
                    in_flight_turn: _,
                } => {
                    // INVARIANT: Resume mode MUST NOT send `session/new`
                    // or `session/load`. This is the load-bearing trick
                    // that makes mid-turn continuity work across
                    // `aoe serve --stop` + `aoe serve`. Do not "fix" it
                    // by adding either call here.
                    //
                    // Why: the runner kept the agent process alive
                    // across the daemon restart, so the ACP session is
                    // still loaded in the agent's memory and addressable
                    // via its original id. `session/load` would either
                    // fail (agents that advertise loadSession=false) or
                    // double-load against a still-busy session and
                    // replay the entire transcript at the user.
                    // `session/new` would split context onto a new id
                    // the in-flight `session/prompt` doesn't address,
                    // silently orphaning the turn the user is waiting
                    // on. See issue #1037 and the
                    // `tests/acp_midturn_resume.rs` integration
                    // coverage.
                    info!(
                        target: "acp.protocol",
                        session = %session_label,
                        stored_id = %stored,
                        "resume mode: reusing existing acp session id without handshake"
                    );
                    // Emit AcpSessionAssigned so the frontend reducer
                    // clears any sticky startupError/lastError from the
                    // crash. The server-side listener treats a same-id
                    // Assigned as a no-op, so this doesn't rewrite
                    // sessions.json.
                    let _ = event_tx_for_block
                        .send(Event::AcpSessionAssigned {
                            acp_session_id: stored.clone(),
                        })
                        .await;
                    SessionId::from(stored)
                }
                ConnectMode::Fresh {
                    stored_acp_session_id,
                    seed_history_replay,
                } => {
                    // Decide whether to resume the prior agent session or create
                    // a fresh one. session/load is only attempted when the agent
                    // advertises support AND we have a stored id to feed it. On
                    // load failure (id GC'd, agent state lost, etc.) we fall
                    // through to session/new and emit SessionContextReset so the
                    // UI can show a notice and clear stale token-usage hints.
                    let mut acp_session_id: Option<SessionId> = None;
                    if load_session_capable {
                        if let Some(stored) = stored_acp_session_id.clone() {
                            info!(
                                target: "acp.protocol",
                                session = %session_label,
                                stored_id = %stored,
                                "resuming session via session/load"
                            );
                            // Set the flag BEFORE sending the request: claude-agent-acp
                            // re-emits the prior transcript via session/update
                            // notifications *during* the load handshake, before the
                            // LoadSessionRequest response returns. Setting after .await
                            // would let those notifications leak through to the event
                            // store and produce duplicate ToolCallStarted rows on the
                            // next reload (assistant-ui then panics with "Duplicate
                            // key toolCallId-..."). Cleared on Err below if we fall
                            // back to session/new, which has no replay payload.
                            //
                            // Exception: an imported session (#2276) has an empty
                            // event store, so we WANT the replay to populate it and
                            // render the transcript. No existing rows means no
                            // duplicate-key risk. The server clears import_pending
                            // once this load lands, so a later reattach suppresses
                            // normally.
                            if !seed_history_replay {
                                suppress_for_block.store(true, Ordering::Relaxed);
                            }
                            let req = LoadSessionRequest::new(stored.clone(), cwd.clone())
                                .mcp_servers(mcp_servers.clone());
                            match connection.send_request(req).block_task().await {
                                Ok(resp) => {
                                    info!(
                                        target: "acp.protocol",
                                        session = %session_label,
                                        stored_id = %stored,
                                        "session/load succeeded; suppressing post-load history replay"
                                    );
                                    // Capture available mode info from the
                                    // load response before consuming resp.
                                    let modes = resp.modes.as_ref().map(|m| {
                                        m.available_modes
                                            .iter()
                                            .map(|mode| mode.id.0.to_string())
                                            .collect::<Vec<_>>()
                                    });
                                    if modes.is_some() {
                                        available_mode_ids = modes;
                                    }
                                    if resp
                                        .config_options
                                        .as_ref()
                                        .is_some_and(|opts| {
                                            opts.iter().any(|o| {
                                                o.category
                                                    == Some(
                                                        agent_client_protocol::schema::
                                                            SessionConfigOptionCategory::Mode,
                                                    )
                                            })
                                        })
                                    {
                                        has_config_option_mode = true;
                                    }
                                    // Emit AcpSessionAssigned even on resume so the
                                    // frontend reducer can clear any sticky
                                    // `startupError` / `lastError` from a prior crash
                                    // (e.g. a respawn after the user's prompt hit a
                                    // dead pipe). The server-side listener treats a
                                    // same-id Assigned as a no-op, so this doesn't
                                    // rewrite sessions.json.
                                    let _ = event_tx_for_block
                                        .send(Event::AcpSessionAssigned {
                                            acp_session_id: stored.clone(),
                                        })
                                        .await;
                                    // LoadSessionResponse carries config_options
                                    // (including the model selector, category
                                    // Model) so the structured view picker
                                    // hydrates on resume without waiting for a
                                    // notification. See #1403.
                                    if let Some(event) =
                                        config_options_event(resp.config_options)
                                    {
                                        let _ = event_tx_for_block.send(event).await;
                                    }
                                    acp_session_id = Some(SessionId::from(stored));
                                }
                                Err(e) if seed_history_replay => {
                                    // Import seed (#2276): the replay may have
                                    // partially populated the (otherwise empty)
                                    // event store before load failed. Falling
                                    // back to session/new would leave a fresh
                                    // session inheriting that partial external
                                    // transcript, so fail the import instead.
                                    // import_pending stays set (no
                                    // AcpSessionAssigned), and the next spawn
                                    // clears the store and re-seeds before
                                    // retrying.
                                    warn!(
                                        target: "acp.protocol",
                                        session = %session_label,
                                        stored_id = %stored,
                                        "session/load failed for imported session; failing import (no session/new fallback): {e}"
                                    );
                                    return Err(e);
                                }
                                Err(e) => {
                                    warn!(
                                        target: "acp.protocol",
                                        session = %session_label,
                                        stored_id = %stored,
                                        "session/load failed, falling back to session/new: {e}"
                                    );
                                    suppress_for_block.store(false, Ordering::Relaxed);
                                    let _ = event_tx_for_block
                                        .send(Event::SessionContextReset {
                                            reason: format!("session/load failed: {e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                    }

                    if let Some(id) = acp_session_id {
                        id
                    } else {
                        info!(
                            target: "acp.protocol",
                            session = %session_label,
                            "creating fresh session via session/new"
                        );
                        let new_session = connection
                            .send_request(NewSessionRequest::new(cwd).mcp_servers(mcp_servers))
                            .block_task()
                            .await?;
                        let id = new_session.session_id.clone();
                        info!(
                            target: "acp.protocol",
                            session = %session_label,
                            new_id = %id.0,
                            "session/new succeeded, captured acp_session_id"
                        );

                        // Capture available mode IDs and config-option mode
                        // category so the SetMode handlers below can skip
                        // modes the agent has not advertised.
                        if let Some(modes) = &new_session.modes {
                            available_mode_ids = Some(
                                modes
                                    .available_modes
                                    .iter()
                                    .map(|m| m.id.0.to_string())
                                    .collect(),
                            );
                        }
                        if new_session
                            .config_options
                            .as_ref()
                            .is_some_and(|opts| {
                                opts.iter().any(|o| {
                                    o.category
                                        == Some(
                                            agent_client_protocol::schema::
                                                SessionConfigOptionCategory::Mode,
                                        )
                                })
                            })
                        {
                            has_config_option_mode = true;
                        }

                        // Surface the agent-advertised modes (if any) so the UI
                        // can render the actual modes the agent supports rather
                        // than the hard-coded four. Claude's adapter typically
                        // ships a mode set with ids like "default" / "plan" /
                        // "accept_edits" / "bypass_permissions".
                        if let Some(modes) = &new_session.modes {
                            let infos: Vec<ModeInfo> = modes
                                .available_modes
                                .iter()
                                .map(|m| ModeInfo {
                                    id: m.id.0.to_string(),
                                    name: m.name.clone(),
                                    description: m.description.clone(),
                                })
                                .collect();
                            let _ = event_tx_for_block
                                .send(Event::ModesAvailable {
                                    current_mode_id: modes.current_mode_id.0.to_string(),
                                    modes: infos,
                                })
                                .await;
                        }

                        // NewSessionResponse carries config_options
                        // (claude-agent-acp emits the initial model + effort +
                        // mode set here, not as a subsequent notification), so
                        // the structured view pickers render immediately. See
                        // #1403.
                        let config_options = new_session.config_options.clone();
                        if let Some(event) = config_options_event(config_options.clone()) {
                            let _ = event_tx_for_block.send(event).await;
                        }

                        if let (Some(effort), Some(options)) =
                            (default_effort.as_deref(), config_options.as_deref())
                        {
                            if let Some(config_id) = thought_level_config_id(options) {
                                info!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    effort,
                                    "applying default structured view effort"
                                );
                                match connection
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        id.clone(),
                                        config_id,
                                        SessionConfigValueId::new(effort.to_string()),
                                    ))
                                    .block_task()
                                    .await
                                {
                                    Ok(resp) => {
                                        if let Some(event) =
                                            config_options_event(Some(resp.config_options))
                                        {
                                            let _ = event_tx_for_block.send(event).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            target: "acp.protocol",
                                            session = %session_label,
                                            "default structured view effort failed: {e}"
                                        );
                                    }
                                }
                            } else {
                                debug!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "default structured view effort skipped; no thought_level option"
                                );
                            }
                        }

                        // Tell the server-side listener so it can persist the
                        // new id on Instance.acp_session_id.
                        let _ = event_tx_for_block
                            .send(Event::AcpSessionAssigned {
                                acp_session_id: id.0.to_string(),
                            })
                            .await;

                        id
                    }
                }
            };

            // Arm the resume-idle watchdog. The agent's response to the
            // orphaned in-flight `session/prompt` (from the previous
            // daemon) carries a request id this client never issued and
            // is dropped silently by the transport. Without this
            // synthesized Stopped, the UI's "thinking" indicator never
            // clears until the user manually sends a new prompt.
            if arm_resume_watchdog {
                let event_tx_for_watchdog = event_tx_for_block.clone();
                let last_event_at = last_event_at.clone();
                let first_event_after_attach = first_event_after_attach.clone();
                let prompt_sent_since_attach = prompt_sent_since_attach.clone();
                let watchdog_fired = watchdog_fired.clone();
                let session_label_for_watchdog = session_label.clone();
                let grace = resume_idle_grace();
                tokio::spawn(async move {
                    let grace_ms = grace.as_millis() as i64;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if watchdog_fired.load(Ordering::Relaxed) {
                            return;
                        }
                        if prompt_sent_since_attach.load(Ordering::Relaxed) {
                            // User sent a new prompt; its real
                            // PromptRequest will own the next Stopped.
                            return;
                        }
                        if first_event_after_attach.load(Ordering::Relaxed) {
                            // The runner forwarded at least one notification
                            // for the in-flight turn, so the turn is
                            // observable; any further silence is normal
                            // mid-turn reasoning (Task subagents, slow Bash,
                            // long reads) rather than an orphaned turn. Disarm
                            // permanently. The narrow residual (the turn
                            // completes after attach and its PromptResponse is
                            // lost, leaving a stale spinner) is rare and
                            // recoverable via force-end-turn / a new prompt.
                            // See #1216.
                            info!(
                                target: "acp.protocol",
                                session = %session_label_for_watchdog,
                                "resume-idle watchdog: disarming, in-flight turn is observable"
                            );
                            return;
                        }
                        let last = last_event_at.load(Ordering::Relaxed);
                        let now = chrono::Utc::now().timestamp_millis();
                        if now - last >= grace_ms {
                            info!(
                                target: "acp.protocol",
                                session = %session_label_for_watchdog,
                                idle_ms = now - last,
                                "resume-idle watchdog: synthesizing Stopped for orphaned in-flight turn"
                            );
                            watchdog_fired.store(true, Ordering::Relaxed);
                            let _ = event_tx_for_watchdog
                                .send(Event::Stopped {
                                    reason: "reattach_idle".into(),
                                })
                                .await;
                            return;
                        }
                    }
                });
            }

            // The idle tick fires the between-prompt watchdog (#2325). It is
            // only polled while this loop is parked at `cmd_rx.recv()`, i.e.
            // between prompts; during a prompt the inner drain owns the
            // connection and this arm never runs, so the per-prompt watchdog
            // stays the sole idle authority there. Emitting Stopped from the
            // command loop (never a detached task) keeps it serialized with
            // every other command, so it can't race a new prompt's events.
            let mut between_prompt_idle_tick =
                tokio::time::interval(BETWEEN_PROMPT_IDLE_CHECK_INTERVAL);
            between_prompt_idle_tick
                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let cmd = tokio::select! {
                    cmd = cmd_rx.recv() => cmd,
                    _ = between_prompt_idle_tick.tick() => {
                        let now = chrono::Utc::now().timestamp_millis();
                        let wake_at = match between_prompt_wake_at.load(Ordering::Relaxed) {
                            0 => None,
                            at => Some(at),
                        };
                        let tools_in_flight = !between_prompt_tools
                            .lock()
                            .expect("between-prompt tools mutex poisoned")
                            .is_empty();
                        if between_prompt_should_fire(
                            between_prompt_active.load(Ordering::Relaxed),
                            now,
                            last_lifecycle_at.load(Ordering::Relaxed),
                            wake_at,
                            between_prompt_cost_seen.load(Ordering::Relaxed),
                            tools_in_flight,
                            between_prompt_off_protocol.load(Ordering::Relaxed),
                            BETWEEN_PROMPT_IDLE_GRACE,
                            OFF_PROTOCOL_WORK_GRACE_FLOOR,
                        ) {
                            // Clear all between-prompt state so a stale expired
                            // wake can't accelerate (or an off-protocol latch
                            // can't pin) the next agent-initiated turn. See #2371.
                            between_prompt_active.store(false, Ordering::Relaxed);
                            between_prompt_cost_seen.store(false, Ordering::Relaxed);
                            between_prompt_wake_at.store(0, Ordering::Relaxed);
                            between_prompt_off_protocol.store(false, Ordering::Relaxed);
                            between_prompt_tools
                                .lock()
                                .expect("between-prompt tools mutex poisoned")
                                .clear();
                            info!(
                                target: "acp.protocol",
                                session = %session_label,
                                "between-prompt idle watchdog: synthesizing Stopped for completed agent-initiated turn"
                            );
                            let _ = event_tx_for_block
                                .send(Event::Stopped {
                                    reason: "agent_idle".into(),
                                })
                                .await;
                        }
                        continue;
                    }
                };
                match cmd {
                    Some(ClientCmd::Prompt(blocks)) => {
                        // Scope the agent-message deduper to one turn: a new
                        // prompt starts a fresh assistant block, so forget any
                        // block left open by the prior turn. See #2281.
                        agent_msg_dedup_for_block
                            .lock()
                            .expect("agent message dedup mutex poisoned")
                            .reset();
                        // First user prompt after session/load: stop
                        // dropping notifications. The agent's history-
                        // replay window is over; everything from now on
                        // is live conversation.
                        if suppress_for_block.swap(false, Ordering::Relaxed) {
                            info!(
                                target: "acp.protocol",
                                session = %session_label,
                                "first user prompt after session/load; resuming notification pump"
                            );
                        }
                        // Stand the resume-idle watchdog down: the new
                        // prompt's real Stopped will own the next status
                        // transition, so we no longer need to synthesize
                        // one for the orphaned prior turn.
                        prompt_sent_since_attach.store(true, Ordering::Relaxed);
                        // A real prompt supersedes any agent-initiated turn the
                        // between-prompt idle watchdog was tracking; this
                        // prompt's own Stopped will own the next transition.
                        // The per-prompt watchdog owns idle detection until the
                        // Stopped emit below clears `prompt_in_flight`. See #2325.
                        prompt_in_flight.store(true, Ordering::Relaxed);
                        between_prompt_active.store(false, Ordering::Relaxed);
                        // A real prompt supersedes any agent-initiated turn the
                        // between-prompt watchdog was tracking; reset its state
                        // so a leftover wake / off-protocol latch / open tool
                        // from the prior turn can't skew this prompt. See #2371.
                        between_prompt_cost_seen.store(false, Ordering::Relaxed);
                        between_prompt_wake_at.store(0, Ordering::Relaxed);
                        between_prompt_off_protocol.store(false, Ordering::Relaxed);
                        between_prompt_tools
                            .lock()
                            .expect("between-prompt tools mutex poisoned")
                            .clear();
                        info!(target: "acp.protocol", "sending prompt ({} content blocks)", blocks.len());
                        // Drive the prompt request concurrently with the
                        // command channel so out-of-band notifications
                        // (Cancel, SetMode) can be delivered to the agent
                        // mid-turn. Per the ACP spec, session/cancel is a
                        // notification specifically designed to be sent
                        // while a session/prompt request is in flight; if
                        // we serialise the loop on the prompt's await, the
                        // cancel sits idle in the channel and only goes
                        // out after the turn already finished.
                        // Bump the prompt epoch BEFORE issuing the new
                        // `session/prompt`. Notification-handler tasks
                        // parked on a full lifecycle channel from the
                        // previous prompt may still wake and send their
                        // envelopes; tagged with the old epoch, they
                        // get discarded in the select arm below
                        // instead of contaminating this prompt's
                        // watchdog state. Drain any envelopes already
                        // sitting in the channel too, to bound the
                        // number we'd otherwise re-check via the
                        // discard path. See #1401 post-impl review.
                        let this_prompt_epoch = current_prompt_epoch
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        while lifecycle_signal_rx.try_recv().is_ok() {}

                        // Per-prompt silent-orphan state machine. Owned
                        // by the prompt loop, mutated only via
                        // `apply_signal`, queried only via `should_fire`.
                        // The watchdog stays disarmed until
                        // `saw_first_progress` becomes true (some
                        // progress notification arrived for this turn)
                        // AND `tool_calls_in_flight` is empty (no open
                        // tool to legitimately be silent for) AND no
                        // active `wakeup_suppress_until` deadline. The
                        // effective grace adapts to off-protocol work
                        // (async-agent, backgrounded Bash) and the
                        // cost-populated UsageUpdate "wrap up accounting"
                        // marker. See #1240, #1360, #1401.
                        let mut watchdog = SilentOrphanWatchdog::new();
                        let mut orphan_cancel_sent = false;
                        let mut prompt_orphaned = false;

                        let silent_orphan_grace_default =
                            silent_orphan_grace(source_profile.as_deref());
                        let silent_orphan_grace_fast =
                            silent_orphan_fast_grace(source_profile.as_deref());
                        let silent_orphan_enabled =
                            silent_orphan_grace_default > std::time::Duration::ZERO;
                        let silent_orphan_check_period = silent_orphan_check_interval();
                        let watchdog_cfg = SilentOrphanWatchdogConfig {
                            base_grace: silent_orphan_grace_default,
                            fast_grace: silent_orphan_grace_fast,
                            off_protocol_grace_floor: OFF_PROTOCOL_WORK_GRACE_FLOOR,
                        };
                        let silent_orphan_check =
                            tokio::time::sleep(silent_orphan_check_period);
                        tokio::pin!(silent_orphan_check);

                        let prompt_started_at_ms = chrono::Utc::now().timestamp_millis();
                        let prompt_fut = connection
                            .send_request(PromptRequest::new(acp_session_id.clone(), blocks))
                            .block_task();
                        tokio::pin!(prompt_fut);

                        // Debug-only fault injection: when this env var
                        // is set, the prompt_fut select arm is gated
                        // off so the response is never observed even
                        // if it arrives. The silent-orphan watchdog
                        // must then fire to break the loop, which is
                        // the entire point of the manual repro recipe
                        // for #1240. Single-shot: the env var is
                        // cleared after first read so subsequent
                        // prompts are healthy. Release builds set
                        // this to `false` const and the prompt_fut
                        // arm is unconditionally polled.
                        #[cfg(debug_assertions)]
                        let simulate_orphan = {
                            let on = std::env::var("AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT")
                                .ok()
                                .as_deref()
                                == Some("1");
                            if on {
                                warn!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT set; suppressing prompt_fut completion to trigger silent-orphan watchdog"
                                );
                                std::env::remove_var("AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT");
                            }
                            on
                        };
                        #[cfg(not(debug_assertions))]
                        let simulate_orphan = false;

                        let mut shutdown = false;
                        // Cancel-escalation watchdog. The first
                        // `session/cancel` sent while the prompt future is
                        // still pending arms a 10s timer; if the agent
                        // doesn't resolve the prompt before it fires (or
                        // the user submits a follow-up prompt while we're
                        // already cancelling, which means they've already
                        // clicked "Force end turn" and re-typed), we
                        // declare the agent unresponsive, end the
                        // connection task, and let the supervisor drain
                        // path SIGTERM the runner and respawn with
                        // session/load for transcript continuity. Without
                        // this, claude-agent-acp ignoring cancel in the
                        // middle of a `block: true` TaskOutput leaves the
                        // daemon's `prompt_fut` pinned forever and every
                        // follow-up prompt is silently dropped. See #1196.
                        let mut agent_unresponsive = false;
                        let mut rate_limited = false;
                        // True when the adapter resolves the in-flight
                        // session/prompt with `StopReason::Cancelled`,
                        // i.e. the user cancelled and the adapter
                        // acknowledged cleanly. claude-agent-acp >=0.37.0
                        // emits this natively per upstream #694; older
                        // adapters surfaced cancellation as `EndTurn` so
                        // the cancel-escalation watchdog was aoe's only
                        // signal. The 10s watchdog still runs as a
                        // transport-wedge defense; this flag only
                        // affects the terminal Stopped reason string so
                        // the reducer can distinguish a user-driven
                        // stop from a clean turn completion.
                        let mut prompt_cancelled = false;
                        let mut cancelling = false;
                        // Set when the user clicked "Force stop": ends the
                        // turn with `user_forced` so the drain task kills the
                        // process group + respawns, instead of waiting out
                        // the 10s grace. See #1727.
                        let mut force_stopped = false;
                        let cancel_grace = tokio::time::sleep(CANCEL_ESCALATION_GRACE);
                        tokio::pin!(cancel_grace);

                        loop {
                            tokio::select! {
                                res = &mut prompt_fut, if !simulate_orphan => {
                                    match res {
                                        Ok(resp) => {
                                            // Capture the native stop reason so
                                            // the terminal emission downstream
                                            // can distinguish a cancelled turn
                                            // (StopReason::Cancelled, claude-agent-acp
                                            // >=0.37.0 per upstream #694) from a
                                            // clean turn completion. EndTurn /
                                            // MaxTokens / MaxTurnRequests / Refusal
                                            // all collapse to `prompt_complete`
                                            // for compatibility with the existing
                                            // reducer; we only surface
                                            // `cancelled` because it has a
                                            // distinct UI implication.
                                            if matches!(resp.stop_reason, StopReason::Cancelled) {
                                                prompt_cancelled = true;
                                            }
                                        }
                                        Err(e) => {
                                            // Rate-limit on session/prompt is not
                                            // a worker crash. Emit a typed
                                            // RateLimit event so the UI banner
                                            // surfaces reset time, then mark the
                                            // turn rate_limited and exit the
                                            // connection task cleanly. The drain
                                            // task watches for Stopped{rate_limited}
                                            // and short-circuits restart_decision
                                            // so the supervisor doesn't burn
                                            // restart budget respawning a worker
                                            // that will hit the same limit
                                            // immediately on retry. See #1281.
                                            if let Some(info) = classify_rate_limit_error(&e) {
                                                info!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    resets_at = %info.resets_at,
                                                    "session/prompt returned rate_limit; parking session"
                                                );
                                                let _ = event_tx_for_block
                                                    .send(Event::RateLimit { info })
                                                    .await;
                                                rate_limited = true;
                                                shutdown = true;
                                                break;
                                            }
                                            return Err(e);
                                        }
                                    }
                                    break;
                                }
                                env = lifecycle_signal_rx.recv() => {
                                    if let Some(env) = env {
                                        if env.epoch != this_prompt_epoch {
                                            // Stale envelope from a prior
                                            // prompt (handler was parked on
                                            // a full channel and only
                                            // unblocked after the next
                                            // prompt began). Discard.
                                            trace!(
                                                target: "acp.protocol",
                                                session = %session_label,
                                                envelope_epoch = env.epoch,
                                                current_epoch = this_prompt_epoch,
                                                "discarding stale lifecycle envelope across prompt boundary"
                                            );
                                        } else {
                                            watchdog.apply_signal(
                                                env.signal,
                                                tokio::time::Instant::now(),
                                                chrono::Utc::now(),
                                                watchdog_cfg,
                                            );
                                        }
                                    }
                                    // None means the notification handler dropped; the
                                    // prompt_fut or cancel_grace arm will end the loop.
                                }
                                _ = &mut silent_orphan_check,
                                    if silent_orphan_enabled && !orphan_cancel_sent =>
                                {
                                    let now = tokio::time::Instant::now();
                                    let should_fire = watchdog.should_fire(now, watchdog_cfg);
                                    if should_fire
                                        && watchdog.cost_seen()
                                        && watchdog.off_protocol_work_seen().is_none()
                                    {
                                        // The turn emitted its cost-populated
                                        // end-of-turn UsageUpdate and then went
                                        // silent with no in-flight tools and no
                                        // off-protocol work: claude-agent-acp
                                        // finished but never returned the
                                        // PromptResponse. Cancelling and
                                        // restarting the worker here (the orphan
                                        // path below) restarts a turn that
                                        // actually succeeded and shows the
                                        // "Agent finished but didn't notify the
                                        // daemon" banner. Treat the cost marker
                                        // as authoritative and end the turn
                                        // cleanly as prompt_complete; the
                                        // connection task stays alive for the
                                        // next prompt. The genuinely-wedged
                                        // case (no cost marker) still falls
                                        // through to the orphan path. See #2237;
                                        // the off-protocol guard preserves the
                                        // monitor / async-agent grace behavior
                                        // of #1360 / #1401 / #1858.
                                        info!(
                                            target: "acp.protocol",
                                            session = %session_label,
                                            grace_secs = watchdog.effective_grace(watchdog_cfg).as_secs(),
                                            "silent-orphan watchdog: turn wrapped up (cost-populated usage) without PromptResponse; ending cleanly as prompt_complete"
                                        );
                                        // Break with NO orphan/shutdown flag set so the
                                        // terminal reason falls through to prompt_complete:
                                        // a clean end, no worker restart, connection task
                                        // survives for the next prompt. See #2237.
                                        break;
                                    }
                                    if should_fire {
                                        warn!(
                                            target: "acp.protocol",
                                            session = %session_label,
                                            off_protocol_work = ?watchdog.off_protocol_work_seen(),
                                            in_flight_tools = watchdog.tool_calls_in_flight_len(),
                                            grace_secs = watchdog.effective_grace(watchdog_cfg).as_secs(),
                                            "silent-orphan watchdog fired: no progress past grace and no in-flight tools; sending session/cancel"
                                        );
                                        // Best-effort cancel; reuse
                                        // existing escalation path. If
                                        // the adapter resolves within
                                        // CANCEL_ESCALATION_GRACE the
                                        // prompt_fut arm wins; if not,
                                        // the cancel_grace arm fires
                                        // and we synthesize Stopped
                                        // with reason "prompt_orphaned".
                                        if let Err(err) = connection.send_notification(
                                            CancelNotification::new(acp_session_id.clone()),
                                        ) {
                                            warn!(
                                                target: "acp.protocol",
                                                session = %session_label,
                                                error = %err,
                                                "silent-orphan: session/cancel send failed; escalating immediately"
                                            );
                                            prompt_orphaned = true;
                                            shutdown = true;
                                            break;
                                        }
                                        orphan_cancel_sent = true;
                                        prompt_orphaned = true;
                                        if !cancelling {
                                            cancelling = true;
                                            cancel_grace.as_mut().reset(
                                                tokio::time::Instant::now()
                                                    + CANCEL_ESCALATION_GRACE,
                                            );
                                        }
                                    }
                                    silent_orphan_check.as_mut().reset(
                                        tokio::time::Instant::now()
                                            + silent_orphan_check_period,
                                    );
                                }
                                _ = &mut cancel_grace, if cancelling => {
                                    warn!(
                                        target: "acp.protocol",
                                        session = %session_label,
                                        grace_secs = CANCEL_ESCALATION_GRACE.as_secs(),
                                        "agent ignored session/cancel past grace window; escalating to runner restart"
                                    );
                                    agent_unresponsive = true;
                                    shutdown = true;
                                    break;
                                }
                                cmd = cmd_rx.recv() => {
                                    match cmd {
                                        Some(ClientCmd::Cancel) => {
                                            info!(
                                                target: "acp.protocol",
                                                "sending session/cancel during in-flight prompt"
                                            );
                                            connection.send_notification(
                                                CancelNotification::new(acp_session_id.clone()),
                                            )?;
                                            // Arm the escalation watchdog on
                                            // the first cancel only; later
                                            // cancels just resend the
                                            // notification.
                                            if !cancelling {
                                                cancelling = true;
                                                cancel_grace.as_mut().reset(
                                                    tokio::time::Instant::now()
                                                        + CANCEL_ESCALATION_GRACE,
                                                );
                                                // Tell the UI a cancel is in
                                                // flight so it can show
                                                // "Stopping..." with an honest
                                                // escalation countdown instead
                                                // of a silent spinner. Once per
                                                // turn. See #1727.
                                                let escalates_at = chrono::Utc::now()
                                                    + chrono::Duration::from_std(
                                                        CANCEL_ESCALATION_GRACE,
                                                    )
                                                    .unwrap_or_else(|_| {
                                                        chrono::Duration::seconds(10)
                                                    });
                                                let _ = event_tx_for_block
                                                    .send(Event::CancelRequested { escalates_at })
                                                    .await;
                                            }
                                        }
                                        Some(ClientCmd::ForceStop) => {
                                            warn!(
                                                target: "acp.protocol",
                                                "force-stop requested during in-flight prompt; ending turn and restarting worker"
                                            );
                                            // Best-effort cancel notification
                                            // first (protocol politeness); the
                                            // real lever is ending the turn so
                                            // the drain task kills the process
                                            // group and respawns. See #1727.
                                            let _ = connection.send_notification(
                                                CancelNotification::new(acp_session_id.clone()),
                                            );
                                            force_stopped = true;
                                            shutdown = true;
                                            break;
                                        }
                                        Some(ClientCmd::SetConfigOption { config_id, value }) => {
                                            dispatch_set_config_option(
                                                &connection,
                                                &acp_session_id,
                                                config_id,
                                                value,
                                                event_tx_for_block.clone(),
                                            );
                                        }
                                        Some(ClientCmd::SetMode(mode_id)) => {
                                            // Skip when the agent has not
                                            // advertised this mode (see the
                                            // mode-tracking comments above).
                                            if !is_mode_advertised(
                                                &mode_id,
                                                &available_mode_ids,
                                                has_config_option_mode,
                                            ) {
                                                debug!(
                                                    target: "acp.protocol",
                                                    "skipping session/set_mode mode={mode_id}: not advertised (mid-turn)"
                                                );
                                                continue;
                                            }
                                            info!(
                                                target: "acp.protocol",
                                                "sending session/set_mode mode={mode_id} during in-flight prompt"
                                            );
                                            // Fire the request and hand the
                                            // response handling to a detached
                                            // task. Awaiting it here would
                                            // freeze this select loop for the
                                            // duration of the round-trip,
                                            // defeating the point of polling
                                            // cmd_rx concurrently; a Cancel
                                            // arriving while set_mode is in
                                            // flight would queue. The detached
                                            // task mirrors the success into the
                                            // event stream so the UI flips even
                                            // when the adapter (e.g.
                                            // claude-agent-acp) treats the
                                            // response as authoritative and
                                            // skips the follow-up
                                            // current_mode_update notification.
                                            let sent = connection.send_request(
                                                SetSessionModeRequest::new(
                                                    acp_session_id.clone(),
                                                    mode_id.clone(),
                                                ),
                                            );
                                            let tx = event_tx_for_block.clone();
                                            tokio::spawn(async move {
                                                match sent.block_task().await {
                                                    Ok(_) => {
                                                        let _ = tx
                                                            .send(Event::CurrentModeChanged {
                                                                current_mode_id: mode_id,
                                                            })
                                                            .await;
                                                    }
                                                    Err(e) => {
                                                        let reason = format!("{e}");
                                                        warn!(
                                                            target: "acp.protocol",
                                                            "session/set_mode failed mid-turn: {reason}"
                                                        );
                                                        let _ = tx
                                                            .send(Event::ModeSwitchFailed {
                                                                mode_id,
                                                                reason,
                                                            })
                                                            .await;
                                                    }
                                                }
                                            });
                                        }
                                        Some(ClientCmd::DeleteSession {
                                            acp_session_id: target_id,
                                            respond_to,
                                        }) => {
                                            handle_delete_session_cmd(
                                                &connection,
                                                target_id,
                                                respond_to,
                                            );
                                        }
                                        Some(ClientCmd::Prompt(rejected_blocks)) => {
                                            // Surface the dropped prompt
                                            // to the UI so the user can
                                            // retry from a Rejected pill
                                            // instead of having their
                                            // message vanish silently.
                                            // Client-side composer queueing
                                            // is tracked separately in
                                            // #1031; this event covers the
                                            // server-side gap when a prompt
                                            // does make it to the daemon
                                            // while another is in flight.
                                            // Recover the text from the
                                            // first text block; attachments
                                            // aren't carried back into the
                                            // retry pill (rare agent-busy
                                            // edge, text is the retry hook).
                                            let rejected_text = rejected_blocks
                                                .iter()
                                                .find_map(|b| match b {
                                                    ContentBlock::Text(t) => Some(t.text.clone()),
                                                    _ => None,
                                                })
                                                .unwrap_or_default();
                                            warn!(
                                                target: "acp.protocol",
                                                "received Prompt while one is in flight; rejecting"
                                            );
                                            let _ = event_tx_for_block
                                                .send(Event::PromptRejected {
                                                    reason: "agent_busy".into(),
                                                    text: rejected_text,
                                                })
                                                .await;
                                            // A follow-up arriving while
                                            // a cancel is in flight means
                                            // the user has clicked Force
                                            // end turn (which optimistically
                                            // unlocked the composer via the
                                            // supervisor's synthetic Stopped)
                                            // and then re-typed. That's a
                                            // strong signal the agent is
                                            // wedged; escalate immediately
                                            // without waiting for the 10s
                                            // grace.
                                            if cancelling {
                                                warn!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    "follow-up prompt arrived while cancel pending; escalating to runner restart"
                                                );
                                                agent_unresponsive = true;
                                                shutdown = true;
                                                break;
                                            }
                                        }
                                        Some(ClientCmd::Shutdown) | None => {
                                            info!(
                                                target: "acp.protocol",
                                                "shutdown received during in-flight prompt; aborting turn"
                                            );
                                            shutdown = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        // Always emit a terminal Stopped for this turn before
                        // leaving the Prompt arm, including the shutdown path.
                        // Consumers (reducer, persisted status) need a single
                        // turn-end event per turn or they sit on a stale
                        // "in flight" state forever.
                        //
                        // Reason precedence:
                        //   - `rate_limited` wins because it's a typed
                        //     non-crash signal from prompt_fut Err; the
                        //     drain task short-circuits respawn on it
                        //     (#1281), so collapsing it into a generic
                        //     reason would burn restart budget.
                        //   - `prompt_orphaned` next because the
                        //     silent-orphan path is the proximate cause;
                        //     if the cancel-escalation watchdog then
                        //     fires it's a downstream effect of the same
                        //     wedge, and collapsing both into
                        //     "agent_unresponsive" would lose the
                        //     failure signature in postmortems. See
                        //     #1240.
                        // Reason precedence lives in `terminal_stop_reason`
                        // (unit-tested). Notable orderings:
                        //   - `force_stopped` (user "Force stop") wins over
                        //     orphan/unresponsive: it's the proximate cause of
                        //     THIS turn ending and must not be masked by a
                        //     prompt_orphaned flag set earlier. The drain task
                        //     still kills + respawns, but the reason keeps the
                        //     user-initiated signal distinct in postmortems
                        //     (#1727).
                        //   - `prompt_cancelled` surfaces the adapter's clean
                        //     StopReason::Cancelled (upstream #694) distinctly
                        //     from prompt_complete.
                        //   - the finished-but-unacked recovery breaks with
                        //     no flag set, falling through to prompt_complete
                        //     so the turn does NOT restart the worker (#2237).
                        // Apply any lifecycle signal still queued when the loop
                        // broke. A cost-populated UsageUpdate can land in the
                        // same tick prompt_fut resolves, so without this drain
                        // the watchdog state read below could miss it and
                        // misclassify a finished turn. Mirrors the
                        // start-of-prompt drain. See #2370.
                        while let Ok(env) = lifecycle_signal_rx.try_recv() {
                            if env.epoch == this_prompt_epoch {
                                watchdog.apply_signal(
                                    env.signal,
                                    tokio::time::Instant::now(),
                                    chrono::Utc::now(),
                                    watchdog_cfg,
                                );
                            }
                        }
                        // A cost-populated UsageUpdate after a timer-driven
                        // orphan cancel proves the turn finished; the cancel was
                        // premature. terminal_stop_reason demotes it to
                        // prompt_complete unless the adapter is still RPC-wedged.
                        // See #2370.
                        let finished_after_orphan_cancel = orphan_cancel_sent
                            && watchdog.cost_seen()
                            && watchdog.off_protocol_work_seen().is_none();
                        if profile.key == "opencode"
                            && watchdog.cost_seen()
                            && !watchdog.saw_progress()
                            && watchdog.off_protocol_work_seen().is_none()
                        {
                            if let Some(message) = recover_opencode_prompt_error(
                                &acp_session_id.0,
                                prompt_started_at_ms,
                            ) {
                                let _ = event_tx_for_block
                                    .send(Event::PromptRuntimeError { message })
                                    .await;
                            }
                        }
                        let reason = terminal_stop_reason(
                            rate_limited,
                            force_stopped,
                            prompt_orphaned,
                            agent_unresponsive,
                            shutdown,
                            prompt_cancelled,
                            finished_after_orphan_cancel,
                        );
                        let _ = event_tx_for_block
                            .send(Event::Stopped {
                                reason: reason.into(),
                            })
                            .await;
                        // Turn ended: close any open assistant text block so a
                        // restatement can never be matched across the turn
                        // boundary. The next prompt also resets, this is the
                        // belt to that suspenders. See #2281.
                        agent_msg_dedup_for_block
                            .lock()
                            .expect("agent message dedup mutex poisoned")
                            .reset();
                        // The prompt drain is done; hand idle ownership back to
                        // the between-prompt watchdog for any agent-initiated
                        // turn that fires after this point. See #2325.
                        prompt_in_flight.store(false, Ordering::Relaxed);
                        if shutdown {
                            break;
                        }
                    }
                    Some(ClientCmd::Cancel) => {
                        info!(target: "acp.protocol", "sending session/cancel (no prompt in flight)");
                        // Best-effort, NOT `?`: a failed notification means
                        // the agent connection is likely already gone, which
                        // is exactly when the UI most needs the synthetic
                        // Stopped below to unstick. Propagating the error here
                        // would skip that emit and defeat the desync recovery.
                        if let Err(e) = connection
                            .send_notification(CancelNotification::new(acp_session_id.clone()))
                        {
                            warn!(
                                target: "acp.protocol",
                                error = %e,
                                "session/cancel (no prompt in flight) notification failed; still emitting Stopped"
                            );
                        }
                        // A cancel with no prompt in flight means the UI
                        // and the daemon have desynced: the client thinks
                        // a turn is running but this loop owns no
                        // prompt_fut, so no terminal Stopped will ever be
                        // emitted (the adopted/orphaned-turn residual of
                        // #1216). Publish one now so the spinner clears on
                        // the first Stop press instead of forcing the user
                        // onto `aoe acp restart`. Harmless when the UI is
                        // already idle: the reducer caps lastStoppedSeq at
                        // pendingUserPromptSeq, so a spurious Stopped while
                        // idle is a no-op. See #2237.
                        let _ = event_tx_for_block
                            .send(Event::Stopped {
                                reason: "cancelled".into(),
                            })
                            .await;
                    }
                    Some(ClientCmd::ForceStop) => {
                        // No prompt in flight: nothing to kill here. The
                        // supervisor's force_end_turn publishes a synthetic
                        // `Stopped` to free a wedged UI (#1100); we only send
                        // a best-effort cancel notification. See #1727.
                        info!(target: "acp.protocol", "force-stop requested with no prompt in flight; best-effort cancel only");
                        let _ = connection
                            .send_notification(CancelNotification::new(acp_session_id.clone()));
                    }
                    Some(ClientCmd::SetMode(mode_id)) => {
                        // Skip when the agent has not advertised this mode
                        // (see the mode-tracking comments above).
                        if !is_mode_advertised(
                            &mode_id,
                            &available_mode_ids,
                            has_config_option_mode,
                        ) {
                            debug!(
                                target: "acp.protocol",
                                "skipping session/set_mode mode={mode_id}: not advertised"
                            );
                            continue;
                        }
                        info!(target: "acp.protocol", "sending session/set_mode mode={mode_id}");
                        // Detached, same shape as the mid-turn path: don't
                        // freeze the cmd_rx loop on the round-trip.
                        let sent = connection.send_request(SetSessionModeRequest::new(
                            acp_session_id.clone(),
                            mode_id.clone(),
                        ));
                        let tx = event_tx_for_block.clone();
                        tokio::spawn(async move {
                            match sent.block_task().await {
                                Ok(_) => {
                                    let _ = tx
                                        .send(Event::CurrentModeChanged {
                                            current_mode_id: mode_id,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let reason = format!("{e}");
                                    warn!(target: "acp.protocol", "session/set_mode failed: {reason}");
                                    let _ = tx
                                        .send(Event::ModeSwitchFailed { mode_id, reason })
                                        .await;
                                }
                            }
                        });
                    }
                    Some(ClientCmd::DeleteSession {
                        acp_session_id: target_id,
                        respond_to,
                    }) => {
                        handle_delete_session_cmd(&connection, target_id, respond_to);
                    }
                    Some(ClientCmd::SetConfigOption { config_id, value }) => {
                        dispatch_set_config_option(
                            &connection,
                            &acp_session_id,
                            config_id,
                            value,
                            event_tx_for_block.clone(),
                        );
                    }
                    Some(ClientCmd::Shutdown) | None => {
                        info!(target: "acp.protocol", "shutdown received, exiting connection loop");
                        break;
                    }
                }
            }
            Ok(())
        })
        .await;

    match &result {
        Err(e) => {
            error!(
                target: "acp.protocol",
                session = %session_label_for_log,
                "ACP connection task ended with error: {:?}", e
            );
            let message = format!("ACP connection failed: {e}");
            // If the handshake never completed, hand the failure back so
            // `spawn()` can surface a typed error to the caller; otherwise
            // publish a synthetic event so the UI can show a remediation
            // hint instead of a silent dead session.
            if let Some(tx) = ready_tx.lock().await.take() {
                let _ = tx.send(Err(AcpError::Spawn(message.clone())));
            } else if let Some(info) = classify_rate_limit_from_message(&message) {
                // Defensive: rate-limit can also surface from paths the
                // prompt arm doesn't cover (handshake-time, mid-handshake
                // request). Treat it as a parked terminal state instead
                // of a generic startup error so the supervisor drain
                // task observes the same Stopped{rate_limited} signal
                // and skips the restart loop.
                info!(
                    target: "acp.protocol",
                    session = %session_label_for_log,
                    "connection task ended with rate_limit; emitting RateLimit + Stopped"
                );
                let _ = event_tx.send(Event::RateLimit { info }).await;
                let _ = event_tx
                    .send(Event::Stopped {
                        reason: "rate_limited".into(),
                    })
                    .await;
            } else {
                let _ = event_tx.send(Event::AgentStartupError { message }).await;
            }
        }
        Ok(()) => {
            info!(
                target: "acp.protocol",
                session = %session_label_for_log,
                "ACP connection task ended cleanly"
            );
        }
    }
    // In runner-managed mode (child is None) we deliberately don't kill
    // anything here: the per-worker `aoe __acp-runner` shim owns the
    // agent subprocess and outlives this daemon's connection. The socket
    // file also stays; the runner cleans it up on its own exit.
    if let Some(child) = child.as_ref() {
        let mut guard = child.lock().await;
        match guard.try_wait() {
            Ok(Some(status)) => info!(
                target: "acp.protocol",
                session = %session_label_for_log,
                "agent process already exited: status={status}"
            ),
            Ok(None) => info!(
                target: "acp.protocol",
                session = %session_label_for_log,
                "killing agent process after connection task end"
            ),
            Err(e) => warn!(
                target: "acp.protocol",
                session = %session_label_for_log,
                "try_wait failed before kill: {e}"
            ),
        }
        let _ = guard.kill().await;
        if let Some(path) = socket_path {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

/// Wait for the connection task to finish the ACP handshake (or fail).
/// Bounds the wait so a wedged agent (the classic `npx -y` first-run
/// download stall) returns a clear typed error instead of leaving the
/// supervisor parked indefinitely. Also watches for early child exit
/// and surfaces stderr in the message so callers see why it died.
///
/// `install_binary` is the binary name from `AgentSpec.command` so the
/// timeout message points users at the right install command for the
/// specific agent (codex-acp / opencode / gemini, not always
/// claude-agent-acp).
async fn wait_for_handshake(
    session_label: &str,
    ready_rx: oneshot::Receiver<Result<(), AcpError>>,
    child: Option<&Arc<Mutex<tokio::process::Child>>>,
    install_binary: &str,
) -> Result<(), AcpError> {
    let timeout = std::time::Duration::from_secs(30);
    match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => {
            warn!(target: "acp.protocol", session = %session_label, "ACP handshake failed: {e}");
            collect_child_failure(child).await;
            Err(e)
        }
        Ok(Err(_canceled)) => Err(AcpError::Spawn(
            "ACP connection task ended before completing the initialize handshake".into(),
        )),
        Err(_elapsed) => {
            warn!(
                target: "acp.protocol",
                session = %session_label,
                "ACP handshake timed out after {}s",
                timeout.as_secs()
            );
            if let Some(child) = child {
                let mut guard = child.lock().await;
                let _ = guard.kill().await;
            }
            let install_hint = super::install_hints::install_hint_for(install_binary)
                .unwrap_or("install the adapter for the configured agent and re-run");
            Err(AcpError::Spawn(format!(
                "agent did not complete the ACP initialize handshake within {}s. \
                 Common causes: the adapter is still downloading on first run, \
                 or the configured agent command isn't a real ACP server. \
                 Try `{}` and re-run.",
                timeout.as_secs(),
                install_hint
            )))
        }
    }
}

async fn collect_child_failure(child: Option<&Arc<Mutex<tokio::process::Child>>>) {
    if let Some(child) = child {
        let mut guard = child.lock().await;
        if let Ok(Some(status)) = guard.try_wait() {
            warn!(target: "acp.protocol", "agent process exited early: status={status}");
        }
    }
}

/// Issue #1147: monotonic ns-since-process-start, used as a thin
/// correlation token in the structured view ACP tool-dispatch trace. Wall-clock
/// fields like `chrono::Utc::now()` jitter under NTP slew and are too
/// coarse to detect interleaved entry/exit between concurrent handlers;
/// `Instant` is monotonic and ns-resolved on every supported platform.
/// Cast to `u64` because `Instant::elapsed()` returns `Duration` whose
/// `as_nanos()` is `u128`, which `tracing` formats less compactly. A
/// `u64` of ns gives ~584 years of headroom, which is plenty.
fn enter_timestamp_ns() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_nanos() as u64
}

/// Run a synchronous `fs_handler` operation on the blocking pool and
/// flatten the join + handler result into a single `FsError`. Centralizes
/// the panic / cancellation observability so future fs offload sites
/// stay consistent (the offload series spans seven PRs).
async fn spawn_blocking_fs<F, T>(handler: &'static str, f: F) -> Result<T, fs_handler::FsError>
where
    F: FnOnce() -> Result<T, fs_handler::FsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(inner) => inner,
        Err(e) => {
            warn!(
                target: "acp.protocol",
                handler,
                panic = e.is_panic(),
                cancelled = e.is_cancelled(),
                error = %e,
                "fs blocking task join failed"
            );
            Err(fs_handler::FsError::Io(std::io::Error::other(format!(
                "fs {handler} join: {e}"
            ))))
        }
    }
}

async fn handle_read_text_file(
    request: ReadTextFileRequest,
    responder: Responder<ReadTextFileResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    // Issue #1147: parallel-tool-call diagnostics. The `enter_ns` value is a
    // monotonic ns-since-process-start counter; if the model dispatches N
    // tool calls in parallel, the entries should interleave (close `enter_ns`
    // values across handlers) rather than strictly increasing per-handler.
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "read_text_file",
        path = %request.path.display(),
        enter_ns,
        "ACP request handler entered"
    );
    // Offload the synchronous file read to the blocking pool. ACP
    // `fs/read_text_file` is agent driven; a multi-MB file or a slow
    // disk would otherwise stall the runtime worker for the duration
    // of the read, blocking every other ACP handler scheduled on the
    // same worker. FsPolicy is Arc + Clone so the clone is cheap.
    let policy = Arc::clone(&res.fs_policy);
    let label = res.label.clone();
    let ReadTextFileRequest {
        path, line, limit, ..
    } = request;
    let read_outcome = spawn_blocking_fs("read", move || {
        fs_handler::handle_read(&policy, &label, &path)
    })
    .await;
    let result = match read_outcome {
        Ok(content) => {
            // Honor optional line/limit slicing for ACP semantics: 1-based.
            let sliced = if line.is_some() || limit.is_some() {
                let lines: Vec<&str> = content.lines().collect();
                let start = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                let limit = limit.map(|n| n as usize).unwrap_or(usize::MAX);
                let end = start.saturating_add(limit).min(lines.len());
                if start >= lines.len() {
                    String::new()
                } else {
                    lines[start..end].join("\n")
                }
            } else {
                content
            };
            responder.respond(ReadTextFileResponse::new(sliced))
        }
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "read_text_file",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_write_text_file(
    request: WriteTextFileRequest,
    responder: Responder<WriteTextFileResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "write_text_file",
        path = %request.path.display(),
        enter_ns,
        "ACP request handler entered"
    );
    // Offload the synchronous file write to the blocking pool. ACP
    // `fs/write_text_file` is agent driven; a large content payload
    // or a slow disk would otherwise stall the runtime worker for
    // the duration of the write.
    let policy = Arc::clone(&res.fs_policy);
    let label = res.label.clone();
    let WriteTextFileRequest { path, content, .. } = request;
    let write_outcome = spawn_blocking_fs("write", move || {
        fs_handler::handle_write(&policy, &label, &path, &content)
    })
    .await;
    let result = match write_outcome {
        Ok(()) => responder.respond(WriteTextFileResponse::new()),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "write_text_file",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_create_terminal(
    request: CreateTerminalRequest,
    responder: Responder<CreateTerminalResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "create_terminal",
        command = %request.command,
        argc = request.args.len(),
        enter_ns,
        "ACP request handler entered"
    );
    let cwd = request.cwd.clone().unwrap_or_else(|| res.cwd.clone());
    // Sandbox the cwd: must be inside session roots.
    if let Err(e) = res.fs_policy.resolve_inside(&cwd) {
        let r = responder.respond_with_error(agent_client_protocol::util::internal_error(format!(
            "terminal cwd outside session roots: {e}"
        )));
        trace!(
            target: "acp.protocol.tool_dispatch",
            handler = "create_terminal",
            enter_ns,
            elapsed_ns = enter_timestamp_ns() - enter_ns,
            outcome = "cwd_outside_roots",
            "ACP request handler exited"
        );
        return r;
    }
    let terminal_sandbox = res
        .sandbox
        .as_ref()
        .map(|s| super::terminal_handler::TerminalSandbox {
            container_name: s.container_name.clone(),
            env_entries: s.current_env_entries(),
        });
    let result = match res
        .terminals
        .create_and_run(
            &res.label,
            &request.command,
            request.args.clone(),
            cwd,
            terminal_sandbox.as_ref(),
        )
        .await
    {
        Ok(id) => responder.respond(CreateTerminalResponse::new(TerminalId::new(id))),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "create_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

fn build_exit_status(exit_code: Option<i32>) -> agent_client_protocol::schema::TerminalExitStatus {
    use agent_client_protocol::schema::TerminalExitStatus;
    let cast = exit_code.and_then(|c| u32::try_from(c).ok());
    TerminalExitStatus::new().exit_code(cast)
}

async fn handle_terminal_output(
    request: TerminalOutputRequest,
    responder: Responder<TerminalOutputResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "terminal_output",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    let result = match res.terminals.output(request.terminal_id.0.as_ref()).await {
        Ok(out) => {
            let combined = format!("{}{}", out.stdout, out.stderr);
            responder.respond(
                TerminalOutputResponse::new(combined, false)
                    .exit_status(build_exit_status(out.exit_code)),
            )
        }
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "terminal_output",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_wait_for_terminal_exit(
    request: WaitForTerminalExitRequest,
    responder: Responder<WaitForTerminalExitResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "wait_for_terminal_exit",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    // For our one-shot terminal model, the command has already finished by
    // the time `create_and_run` returns. So `output()` immediately yields
    // the captured exit status.
    let result = match res.terminals.output(request.terminal_id.0.as_ref()).await {
        Ok(out) => responder.respond(WaitForTerminalExitResponse::new(build_exit_status(
            out.exit_code,
        ))),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "wait_for_terminal_exit",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_kill_terminal(
    request: KillTerminalRequest,
    responder: Responder<KillTerminalResponse>,
    _res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "kill_terminal",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    // One-shot terminals are already finished; kill is a no-op.
    let result = responder.respond(KillTerminalResponse::new());
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "kill_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_release_terminal(
    request: ReleaseTerminalRequest,
    responder: Responder<ReleaseTerminalResponse>,
    res: SessionResources,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "release_terminal",
        terminal_id = %request.terminal_id.0,
        enter_ns,
        "ACP request handler entered"
    );
    let result = match res.terminals.release(request.terminal_id.0.as_ref()).await {
        Ok(()) => responder.respond(ReleaseTerminalResponse::new()),
        Err(e) => {
            responder.respond_with_error(agent_client_protocol::util::internal_error(e.to_string()))
        }
    };
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "release_terminal",
        enter_ns,
        elapsed_ns = enter_timestamp_ns() - enter_ns,
        "ACP request handler exited"
    );
    result
}

async fn handle_permission_request(
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
    profile: &'static agent_profiles::AgentProfile,
    tool_context_cache: ToolContextCache,
) -> agent_client_protocol::Result<()> {
    let enter_ns = enter_timestamp_ns();
    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        "ACP request handler entered"
    );
    // Build our structured view-side approval card.
    let title = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "tool call".into());
    let cached_raw_input = tool_context_cache
        .lock()
        .expect("tool context cache mutex poisoned")
        .get(&tool_call_id);
    // Empty (not the literal "null") when neither the permission request nor a
    // previously forwarded tool update has raw_input. Gemini's confirm-required
    // tools routinely do this. See #1713. opencode sometimes sends an
    // external_directory permission reusing a tool_call_id whose earlier tool
    // update had the command, so merge that context before emitting events.
    let enriched_raw_input = permission_raw_input_with_context(
        request.tool_call.fields.raw_input.as_ref(),
        cached_raw_input.as_ref(),
    );
    let args_preview = preview_optional_args(enriched_raw_input.as_ref());
    let tool_call = ToolCall {
        id: request.tool_call.tool_call_id.0.to_string(),
        name: title,
        kind: request
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(tool_kind_str)
            .unwrap_or_else(|| "other".into()),
        args_preview,
        started_at: chrono::Utc::now(),
        parent_tool_call_id: profile.parent_tool_use_id_from_meta(&request.tool_call.meta),
        memory_recall: None,
        diffs: Vec::new(),
    };
    // Gemini's confirm-required tools never send a standalone `tool_call`
    // start frame (only requestPermission, then a completion update), so
    // without this the approved tool would have no transcript card and
    // its later completion would render nothing. Emit a start frame from
    // the ToolCall we just built; the reducer dedupes tool_start by id,
    // so a later real start frame merges in place rather than doubling
    // the card. See #1713.
    let _ = event_tx
        .send(Event::ToolCallStarted {
            tool_call: tool_call.clone(),
        })
        .await;
    let approval = build_approval(tool_call);
    let nonce = approval.nonce.clone();

    let (resolve_tx, resolve_rx) = oneshot::channel::<ApprovalResolutionMessage>();
    pending.lock().await.insert(
        nonce.clone(),
        PendingResponder {
            resolver: PendingResolver::Approval(resolve_tx),
        },
    );

    if event_tx
        .send(Event::ApprovalRequested { approval })
        .await
        .is_err()
    {
        // Receiver gone: cancel.
        pending.lock().await.remove(&nonce);
        trace!(
            target: "acp.protocol.tool_dispatch",
            handler = "permission_request",
            tool_call_id = %tool_call_id,
            enter_ns,
            elapsed_ns = enter_timestamp_ns() - enter_ns,
            outcome = "receiver_gone",
            "ACP request handler exited"
        );
        return responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    }

    // Issue #1147: this `await` is the suspected serializer for the user-felt
    // slowness. Log the moment we begin awaiting so a wall-clock comparison
    // with later "responder.respond" emissions exposes how long each pending
    // approval blocked the agent's turn.
    let await_enter_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        await_offset_ns = await_enter_ns - enter_ns,
        "awaiting approval resolution"
    );
    // Build outcome + its label together so the exit event never re-matches on
    // a foreign `#[non_exhaustive]` enum it doesn't fully own.
    let (outcome, outcome_label): (RequestPermissionOutcome, &'static str) = match resolve_rx.await
    {
        Ok(ApprovalResolutionMessage::Decision { decision }) => {
            if let Some(option_id) = pick_option_id(&request.options, decision) {
                // Surface the resolution to UI clients via the typed event channel.
                let _ = event_tx
                    .send(Event::ApprovalResolved {
                        nonce: nonce.clone(),
                        decision,
                    })
                    .await;
                // A denied tool will not run, so the start frame emitted
                // above would otherwise hang on "running" until the turn
                // ends. Close it immediately with a terminal error row.
                // See #1713.
                if matches!(decision, ApprovalDecision::Deny) {
                    emit_permission_denied(&event_tx, &tool_call_id, "permission denied").await;
                }
                (
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                    "selected",
                )
            } else {
                warn!(
                    target: "acp.protocol",
                    "agent did not offer a {decision:?}-compatible option; cancelling"
                );
                // No compatible option: the agent gets Cancelled, but the
                // user still acted, so clear the approval card and close
                // the hanging start frame. See #1713.
                let _ = event_tx
                    .send(Event::ApprovalResolved {
                        nonce: nonce.clone(),
                        decision: ApprovalDecision::Cancelled,
                    })
                    .await;
                emit_permission_denied(&event_tx, &tool_call_id, "permission cancelled").await;
                (RequestPermissionOutcome::Cancelled, "cancelled")
            }
        }
        Ok(ApprovalResolutionMessage::Cancelled) | Err(_) => {
            // Cancellation (explicit cancel_permission, or the resolver
            // dropped on teardown) emits no agent completion, so close the
            // start frame and clear the approval here too. See #1713.
            let _ = event_tx
                .send(Event::ApprovalResolved {
                    nonce: nonce.clone(),
                    decision: ApprovalDecision::Cancelled,
                })
                .await;
            emit_permission_denied(&event_tx, &tool_call_id, "permission cancelled").await;
            (RequestPermissionOutcome::Cancelled, "cancelled")
        }
    };
    let exit_ns = enter_timestamp_ns();
    trace!(
        target: "acp.protocol.tool_dispatch",
        handler = "permission_request",
        tool_call_id = %tool_call_id,
        enter_ns,
        elapsed_ns = exit_ns - enter_ns,
        await_ns = exit_ns - await_enter_ns,
        outcome = outcome_label,
        "responding to permission request"
    );
    responder.respond(RequestPermissionResponse::new(outcome))
}

/// Handle an `elicitation/create` request (claude-agent-acp's
/// `AskUserQuestion`, surfaced because we advertise `elicitation.form`).
/// Mirrors `handle_permission_request`: normalize the form, park a
/// resolver under a fresh nonce, broadcast the card, await the user's
/// answer, then respond to the agent. Cancellation (resolver dropped on
/// teardown) and an unparseable schema both fall back to a graceful
/// response so the agent's turn never hangs.
async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    responder: Responder<CreateElicitationResponse>,
    event_tx: mpsc::Sender<Event>,
    pending: PendingResponders,
) -> agent_client_protocol::Result<()> {
    let nonce = Nonce::new();
    let elicitation = match parse_elicitation(nonce.clone(), &request, chrono::Utc::now()) {
        Ok(elicitation) => elicitation,
        Err(e) => {
            // A schema we can't render (URL mode, or an MCP-server form
            // with number/boolean fields). Cancel rather than Decline: the
            // question was never shown, so "user skipped" (Decline, empty
            // answer) would misrepresent it; Cancel tells the agent the
            // request could not be presented. Either way the turn does not
            // hang on a card we'll never show.
            warn!(target: "cockpit.acp", "unsupported elicitation, cancelling: {e}");
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    };

    let (resolve_tx, resolve_rx) = oneshot::channel::<ElicitationResolutionMessage>();
    pending.lock().await.insert(
        nonce.clone(),
        PendingResponder {
            resolver: PendingResolver::Elicitation {
                elicitation: Box::new(elicitation.clone()),
                resolver: resolve_tx,
            },
        },
    );

    if event_tx
        .send(Event::ElicitationRequested {
            elicitation: elicitation.clone(),
        })
        .await
        .is_err()
    {
        pending.lock().await.remove(&nonce);
        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }

    // Await the user's answer. `resolve_elicitation` validates server-side
    // before sending, so whatever arrives here is already a built, valid
    // response. A dropped resolver (daemon teardown, agent cancel) cancels
    // the tool call.
    let ElicitationResolutionMessage {
        response,
        outcome,
        answers,
    } = resolve_rx
        .await
        .unwrap_or_else(|_| ElicitationResolutionMessage {
            response: CreateElicitationResponse::new(ElicitationAction::Cancel),
            outcome: ElicitationOutcome::Cancelled,
            answers: Vec::new(),
        });

    let _ = event_tx
        .send(Event::ElicitationResolved {
            nonce: nonce.clone(),
            outcome,
            answers,
        })
        .await;

    responder.respond(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[tokio::test]
    async fn fake_client_round_trips_events() {
        let (mut client, tx) = AcpClient::fake_for_test(AcpSessionId("s-1".into()));
        tx.send(Event::ThinkingStarted).await.unwrap();
        let event = client.next_event().await.expect("event delivered");
        assert!(matches!(event, Event::ThinkingStarted));
    }

    // truncate_for_log is the adapter-error sanitizer in the
    // session/delete path: it caps a third-party-controlled string so
    // a chatty adapter can't bloat debug.log, and must never panic on
    // UTF-8 inputs. The cases below lock down the boundary semantics
    // (no-op under cap, exact cap, multibyte cut) so a future change
    // to the helper can't quietly regress the no-panic invariant.
    #[test]
    fn truncate_for_log_below_cap_returns_input() {
        assert_eq!(truncate_for_log("hello", 64), "hello");
    }

    #[test]
    fn truncate_for_log_at_exact_cap_returns_input() {
        assert_eq!(truncate_for_log("hello", 5), "hello");
    }

    #[test]
    fn truncate_for_log_cuts_on_utf8_boundary_without_panic() {
        // "é" is two bytes (0xC3 0xA9). With max_bytes=5 the naive
        // slice would land mid-codepoint; the helper must rewind to
        // the previous char boundary (byte 4) before appending the
        // ellipsis. "ééé" is 6 bytes total, so we expect "éé...".
        let out = truncate_for_log("ééé", 5);
        assert_eq!(out, "éé...");
    }

    fn create_opencode_error_test_db(rows: &[(&str, i64, Option<&str>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        for (idx, (session_id, created, error_message)) in rows.iter().enumerate() {
            let data = if let Some(message) = error_message {
                serde_json::json!({
                    "role": "assistant",
                    "time": { "created": created },
                    "error": { "data": { "message": message } },
                })
            } else {
                serde_json::json!({
                    "role": "assistant",
                    "time": { "created": created },
                })
            };
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    format!("msg-{idx}"),
                    session_id,
                    created,
                    created,
                    data.to_string()
                ],
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn recover_opencode_prompt_error_from_sqlite_returns_latest_matching_error() {
        let dir = create_opencode_error_test_db(&[
            ("ses-1", 99, Some("old error")),
            ("ses-1", 100, None),
            ("ses-1", 110, Some("new error")),
            ("ses-2", 120, Some("wrong session")),
        ]);
        let db_path = dir.path().join("opencode.db");
        let result = recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100);
        assert_eq!(result.as_deref(), Some("new error"));
    }

    #[test]
    fn recover_opencode_prompt_error_from_sqlite_returns_none_without_match() {
        let dir = create_opencode_error_test_db(&[
            ("ses-1", 90, Some("too early")),
            ("ses-1", 100, None),
            ("ses-2", 110, Some("wrong session")),
        ]);
        let db_path = dir.path().join("opencode.db");
        let result = recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100);
        assert_eq!(result, None);
    }

    // -------------------------------------------------------------------
    // SilentOrphanWatchdog: pure-state-machine unit tests
    //
    // The watchdog used to live inline in the prompt loop, where the only
    // way to verify behavior was through real `tokio::time::sleep` and
    // the integration shim. After #1401 the state machine is a free-
    // standing struct that takes synthetic `Instant` / `DateTime<Utc>`
    // inputs, so these tests can step the clock forward in microseconds
    // without flakiness. The covered shapes deliberately overlap the
    // production false-positive class so a regression would be caught
    // before it ever reached the shim.
    // -------------------------------------------------------------------

    fn watchdog_test_cfg() -> SilentOrphanWatchdogConfig {
        SilentOrphanWatchdogConfig {
            base_grace: std::time::Duration::from_secs(120),
            fast_grace: std::time::Duration::from_secs(20),
            off_protocol_grace_floor: std::time::Duration::from_secs(30 * 60),
        }
    }

    #[tokio::test]
    async fn watchdog_fires_on_cost_then_silence() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Fast grace is 20s; 25s after the last progress with cost_seen
        // and no in-flight work must fire.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
    }

    // #2237: when the watchdog fires on a turn that already emitted its
    // cost-populated end-of-turn UsageUpdate (and no off-protocol work),
    // the prompt loop ends the turn cleanly instead of cancel+restart.
    // The decision keys on cost_seen() + off_protocol_work_seen(); guard
    // both so the clean-completion branch is reachable only in that exact
    // shape.
    #[tokio::test]
    async fn watchdog_cost_seen_marks_completed_unacked_path() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        let fire_at = t0 + std::time::Duration::from_secs(25);
        assert!(w.should_fire(fire_at, cfg));
        // The clean-completion branch is gated on both signals.
        assert!(w.cost_seen(), "cost marker must be observable at fire");
        assert!(
            w.off_protocol_work_seen().is_none(),
            "no off-protocol work, so clean completion (not the monitor floor) applies"
        );
    }

    #[tokio::test]
    async fn watchdog_off_protocol_keeps_orphan_path_even_with_cost() {
        // A backgrounded command before the cost marker is dropped by
        // TerminalUsage (#1858), so cost_seen + no off-protocol holds and
        // the clean path applies. But an async-agent / scheduled wakeup is
        // NOT dropped, so those keep off_protocol set and must stay on the
        // orphan path. Lock that distinction down.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Now apply the end-of-turn cost marker. TerminalUsage sets
        // cost_seen, but (unlike a backgrounded command, #1858) a scheduled
        // wakeup is NOT dropped, so off-protocol work stays set. This is the
        // "with cost" case the test name promises: the clean-completion guard
        // (cost_seen && off_protocol none) is still false, so a scheduled-wake
        // turn keeps the orphan path even once its cost usage lands.
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(w.cost_seen());
        assert!(w.off_protocol_work_seen().is_some());
    }

    // #2237: the finished-but-unacked recovery breaks with no flag set, so
    // it must fall through to prompt_complete (the non-restart reason), NOT
    // prompt_orphaned. Guard the all-false fall-through here; the watchdog
    // test above guards the branch condition (cost_seen + no off-protocol).
    #[test]
    fn terminal_stop_reason_all_false_is_prompt_complete() {
        assert_eq!(
            terminal_stop_reason(false, false, false, false, false, false, false),
            "prompt_complete"
        );
    }

    #[test]
    fn terminal_stop_reason_precedence_is_preserved() {
        // rate_limited wins over everything.
        assert_eq!(
            terminal_stop_reason(true, true, true, true, true, true, true),
            "rate_limited"
        );
        // force_stopped beats a prompt_orphaned flag set earlier.
        assert_eq!(
            terminal_stop_reason(false, true, true, false, false, false, false),
            "user_forced"
        );
        // prompt_orphaned (genuine wedge) wins over a later shutdown/cancel.
        assert_eq!(
            terminal_stop_reason(false, false, true, false, true, false, false),
            "prompt_orphaned"
        );
        assert_eq!(
            terminal_stop_reason(false, false, false, false, false, true, false),
            "cancelled"
        );
    }

    #[test]
    fn terminal_stop_reason_late_cost_demotes_orphan_and_cancelled() {
        // #2370: the orphan cancel fired on the timer, the adapter then
        // resolved the prompt as Cancelled (because we cancelled), but a
        // cost-populated UsageUpdate proved the turn finished. The demotion
        // must outrank BOTH prompt_orphaned and prompt_cancelled.
        assert_eq!(
            terminal_stop_reason(false, false, true, false, false, true, true),
            "prompt_complete"
        );
    }

    #[test]
    fn terminal_stop_reason_late_cost_does_not_mask_rpc_wedge_or_user_intent() {
        // #2370: a cost frame proves the model work finished, but if the
        // adapter is still wedged on the RPC (cancel-escalation grace fired)
        // the worker connection may be unusable for the next prompt, so the
        // genuine-wedge restart must survive.
        assert_eq!(
            terminal_stop_reason(false, false, true, true, true, true, true),
            "prompt_orphaned"
        );
        // rate_limited and user force_stopped still win over the demotion.
        assert_eq!(
            terminal_stop_reason(true, false, true, false, false, true, true),
            "rate_limited"
        );
        assert_eq!(
            terminal_stop_reason(false, true, true, false, false, true, true),
            "user_forced"
        );
    }

    // Between-prompt idle watchdog fire decision (#2325). Wall-clock millis.
    // Bind to the production constants so the test tracks the real grace.
    const FAST: std::time::Duration = BETWEEN_PROMPT_IDLE_GRACE;
    const FLOOR: std::time::Duration = OFF_PROTOCOL_WORK_GRACE_FLOOR;

    #[test]
    fn between_prompt_inactive_never_fires() {
        // No agent-initiated turn tracked, even long past any grace.
        assert!(!between_prompt_should_fire(
            false, 10_000_000, 0, None, true, false, false, FAST, FLOOR
        ));
    }

    #[test]
    fn between_prompt_fires_after_fast_grace_when_cost_seen() {
        let last = 1_000_000;
        let grace_ms = FAST.as_millis() as i64;
        // Just under the fast grace: still waiting.
        assert!(!between_prompt_should_fire(
            true,
            last + grace_ms - 500,
            last,
            None,
            true,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the fast grace: the completed turn ends.
        assert!(between_prompt_should_fire(
            true,
            last + grace_ms + 500,
            last,
            None,
            true,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_uses_floor_without_cost() {
        let last = 1_000_000;
        // 21s idle but no cost marker and no expired wake: the generous floor
        // governs (a turn doing silent background work, #1858), no fire.
        assert!(!between_prompt_should_fire(
            true,
            last + 21_000,
            last,
            None,
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the 30-minute floor: fire even without a cost marker.
        assert!(between_prompt_should_fire(
            true,
            last + 30 * 60 * 1000 + 1,
            last,
            None,
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_suppressed_while_future_wake_pending() {
        // A future wake (#1401): the agent is deliberately asleep toward `at`.
        // Suppressed even long past the floor.
        let last = 1_000_000;
        let now = last + 60_000;
        let wake_at = now + 5_000; // still in the future relative to `now`
        assert!(!between_prompt_should_fire(
            true,
            now,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_fires_on_fast_grace() {
        // The #2371 bug: the agent scheduled a wake for `wake_at`, kept working
        // past it, then went quiet without resuming. Once the wake `at` is in
        // the past and no tool / off-protocol work protects the turn, it must
        // self-heal on the fast grace, NOT the 30-minute floor.
        let last = 1_000_000;
        let wake_at = last - 10_000; // already expired when the turn went quiet
                                     // Just under the fast grace: still waiting.
        assert!(!between_prompt_should_fire(
            true,
            last + FAST.as_millis() as i64 - 500,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the fast grace: fire, instead of holding the floor for 30 min.
        assert!(between_prompt_should_fire(
            true,
            last + FAST.as_millis() as i64 + 500,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_suppressed_while_tool_in_flight() {
        // Wake expired AND a tool is still open (a fired wake that resumed and
        // launched a long tool): never fire while a tool runs. Preserves #1401.
        let last = 1_000_000;
        let wake_at = last - 10_000;
        assert!(!between_prompt_should_fire(
            true,
            last + 60_000,
            last,
            Some(wake_at),
            false,
            true, // tool in flight
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_with_off_protocol_holds_floor() {
        // Wake expired and no tool open, but a backgrounded Bash / async agent
        // is still running off-protocol: hold the floor, do not fast-fire and
        // kill the background work. Preserves #1401 / #1858.
        let last = 1_000_000;
        let wake_at = last - 10_000;
        // 21s in: fast grace would have fired, but off-protocol holds the floor.
        assert!(!between_prompt_should_fire(
            true,
            last + 21_000,
            last,
            Some(wake_at),
            false,
            false,
            true, // off-protocol work latched
            FAST,
            FLOOR
        ));
        // Past the floor it still fires (the floor, not a permanent pin).
        assert!(between_prompt_should_fire(
            true,
            last + 30 * 60 * 1000 + 1,
            last,
            Some(wake_at),
            false,
            false,
            true,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_signal_update_terminal_usage_refreshes_timestamp() {
        // TerminalUsage marks cost_seen AND refreshes last_lifecycle_at to
        // `now`, so the fast grace measures from the cost marker, not a
        // stale earlier progress event. See #2325 review.
        let u =
            between_prompt_signal_update(Some(&LifecycleSignal::TerminalUsage), None, 500_000, 0)
                .expect("TerminalUsage is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: true,
                last_lifecycle_at: 500_000,
                wake_at: 0,
            }
        );
    }

    #[test]
    fn between_prompt_signal_update_progress_clears_cost_and_refreshes() {
        let u = between_prompt_signal_update(Some(&LifecycleSignal::Progress), None, 500_000, 0)
            .expect("Progress is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: false,
                last_lifecycle_at: 500_000,
                wake_at: 0,
            }
        );
    }

    #[test]
    fn between_prompt_signal_update_ambient_is_none() {
        // No lifecycle and no wakeup signal: ambient update, no state change.
        assert!(between_prompt_signal_update(None, None, 500_000, 42).is_none());
    }

    #[test]
    fn between_prompt_signal_update_wakeup_stores_at_and_never_shortens() {
        let at = chrono::DateTime::from_timestamp_millis(600_000).unwrap();
        // The wake `at` is stored directly (not `at + floor`), so an expired
        // wake can self-heal on the fast grace. See #2371.
        let u = between_prompt_signal_update(
            None,
            Some(&LifecycleSignal::WakeupPending { at }),
            500_000,
            1_000,
        )
        .expect("WakeupPending is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: false,
                last_lifecycle_at: 500_000,
                wake_at: 600_000,
            }
        );
        // A later pending wake does not shorten suppression: keep the max.
        let u2 = between_prompt_signal_update(
            None,
            Some(&LifecycleSignal::WakeupPending { at }),
            500_000,
            900_000,
        )
        .unwrap();
        assert_eq!(u2.wake_at, 900_000);
    }

    #[test]
    fn between_prompt_stale_progress_plus_cost_marker_does_not_fire_early() {
        // Regression for the state-update path (#2325 review): a progress
        // event 10s ago, then a cost-bearing UsageUpdate now. The cost marker
        // refreshes last_lifecycle_at, so 2s later (under the 3s grace) the
        // watchdog must NOT fire even though cost_seen is true and the prior
        // progress is older than the grace.
        let cost_now = 1_000_000;
        let stale_progress = cost_now - 10_000;
        let u =
            between_prompt_signal_update(Some(&LifecycleSignal::TerminalUsage), None, cost_now, 0)
                .unwrap();
        // The refresh, not the stale progress, governs the grace window.
        assert_eq!(u.last_lifecycle_at, cost_now);
        assert_ne!(u.last_lifecycle_at, stale_progress);
        assert!(!between_prompt_should_fire(
            true,
            cost_now + 2_000,
            u.last_lifecycle_at,
            None,
            u.cost_seen,
            false,
            false,
            FAST,
            FLOOR,
        ));
        // After the full grace it does fire.
        assert!(between_prompt_should_fire(
            true,
            cost_now + FAST.as_millis() as i64 + 1,
            u.last_lifecycle_at,
            None,
            u.cost_seen,
            false,
            false,
            FAST,
            FLOOR,
        ));
    }

    #[tokio::test]
    async fn watchdog_progress_after_terminal_usage_clears_fast_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // A later Progress event must clear cost_seen so the fast grace
        // no longer applies. The watchdog now waits for the full base
        // grace (120s) from the latest progress.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(30), cfg));
        // Still must not fire well past the old fast grace window.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        // And must eventually fire after the full base grace.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
    }

    #[tokio::test]
    async fn watchdog_cost_seen_clears_when_progress_resumes() {
        // #2370: finished_after_orphan_cancel reads cost_seen() directly to
        // demote a premature orphan cancel. If a cost marker arrives but the
        // turn then resumes real work, cost_seen() must flip back to false so a
        // turn that did NOT finish is never demoted to prompt_complete.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        assert!(
            w.cost_seen(),
            "cost marker must be observable after TerminalUsage"
        );
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(
            !w.cost_seen(),
            "progress after the cost marker means the turn resumed; must not demote"
        );
    }

    #[tokio::test]
    async fn watchdog_terminal_usage_clears_background_command_suppression() {
        // Regression for #1858. A backgrounded command lifts the grace to
        // the 30-minute off-protocol floor mid-turn (so a legit `cmd &`
        // is not killed), but a backgrounded command is fire-and-forget
        // and outlives the turn. Once the cost-resolved UsageUpdate
        // (TerminalUsage, the end-of-turn marker) arrives, the floor must
        // drop so a turn that streamed its final usage but never returned
        // the PromptResponse recovers on the fast grace instead of
        // hanging for 30 minutes.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Tool started without the background flag.
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Completion content carries the background marker.
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-1".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Before the terminal usage the floor holds: 60s in must not fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        // TerminalUsage cleared the background-command suppression.
        assert!(w.off_protocol_work_seen().is_none());
        // Now the fast grace (20s) applies, measured from the last
        // progress at t0+2s. Inside the window: no fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(10), cfg));
        // Past the fast grace (elapsed 23s > 20s): the wedge recovers
        // instead of waiting out the 30-minute floor.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
    }

    #[tokio::test]
    async fn watchdog_terminal_usage_then_background_command_rearms_floor() {
        // Self-correction: TerminalUsage clearing background suppression
        // must not be permanent. If activity resumes after the terminal
        // usage (cost_seen flips false on Progress) and a new backgrounded
        // command completes, the off-protocol floor re-arms.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-a".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(w.off_protocol_work_seen().is_none());
        // Turn continues: more progress, then another backgrounded tool.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-b".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(4),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "a fresh backgrounded command after terminal usage must re-arm the floor",
        );
        // Floor is back: must not fire well past the fast grace.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
    }

    #[tokio::test]
    async fn watchdog_async_agent_lifts_grace_above_fast_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-async-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-async-1".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::AsyncAgent),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 25), cfg));
    }

    #[tokio::test]
    async fn watchdog_background_via_raw_input_lifts_grace_without_content_marker() {
        // Defense in depth: even if the completion content marker is
        // missing (SDK string drift, content stripped), the
        // `is_background_task` flag captured at ToolStarted should
        // still flip `off_protocol_work_seen`.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-2".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-2".into(),
                succeeded: true,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "raw_input.run_in_background must trip off-protocol suppression alone"
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 20), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_suppresses_until_at_plus_off_protocol_floor() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Schedule wakeup 1 second in the future.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // A scheduled wake marks the turn as deliberate off-protocol
        // idling; the fast grace must never apply to it.
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::ScheduledWakeup)
        );
        // At the wakeup `at` itself: suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(2), cfg));
        // Well past the old 120s base-grace tail: the monitor turn must
        // still be suppressed now that the tail is the 30-minute
        // off-protocol floor (regression: a monitor used to die ~125s in).
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
        // Just inside `at + floor` (≈1802s): still suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1800), cfg));
        // Past `at + floor`: watchdog finally rearms (transport-wedge
        // backstop). elapsed since last progress (1805s) > floor (1800s).
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1805), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_after_cost_does_not_use_fast_grace() {
        // Regression for the monitor-killed-by-watchdog bug: a `/loop`
        // turn emits a cost-bearing `UsageUpdate` (cost_seen → fast
        // grace) and then schedules a wake. Before the fix the watchdog
        // fired ~20s after the wake window lapsed; now the scheduled-wake
        // off-protocol mark must override fast grace entirely.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Terminal accounting frame arrives first (flips cost_seen).
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Then the agent schedules a wake 2s out.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(2),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // 25s in (well past the 20s fast grace): must NOT fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
        // 200s in (past the old 120s base grace too): still suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(200), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_suppression_eventually_expires() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Step very far past the deadline; should_fire clears the
        // deadline as a side effect and rearms the watchdog.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
        // A subsequent check at any later time without new progress
        // must still fire (the deadline was cleared, so suppression
        // does not re-engage).
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(60 * 60 + 1), cfg));
    }

    #[tokio::test]
    async fn watchdog_later_wakeup_extends_not_shortens_suppression() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // First wakeup: 10s out.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(10),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Second wakeup: 100s out (further). The deadline must extend.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(100),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // At t0 + 50s: the first wakeup's tail (10s + 1800s) is still
        // alive, AND the second wakeup's tail (100s + 1800s = 1902s)
        // is alive. Watchdog must be suppressed by the larger.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(50), cfg));
        // At t0 + 1900s: still inside the second wakeup's tail (1902s).
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1900), cfg));
        // At t0 + 1905s: past the second wakeup's tail.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1905), cfg));
    }

    #[tokio::test]
    async fn watchdog_shorter_followup_wakeup_does_not_shorten_suppression() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // First wakeup: far in the future.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(100),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Second wakeup: closer (should NOT shorten suppression).
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(10),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // First wakeup's tail (100s + 1800s = 1901s) still wins.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(50), cfg));
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1900), cfg));
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1905), cfg));
    }

    #[tokio::test]
    async fn watchdog_tool_in_flight_suppresses_even_after_terminal_usage() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Tool still in flight: watchdog never fires regardless of grace.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
    }

    #[tokio::test]
    async fn watchdog_does_not_fire_without_first_progress() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        // No Progress signal ever received: watchdog stays disarmed.
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
    }

    #[test]
    fn classify_rate_limit_recognises_data_errorkind() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "You've hit your limit · resets 12:10pm (Europe/Paris)".into();
        err.data = Some(serde_json::json!({ "errorKind": "rate_limit" }));
        let info = classify_rate_limit_error(&err).expect("classified");
        assert_eq!(info.kind, "rate_limit");
        assert!(info.status.contains("hit your limit"));
        assert!(info.resets_at > chrono::Utc::now());
    }

    #[test]
    fn classify_rate_limit_prefers_rfc3339_resets_at() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "rate limited".into();
        err.data = Some(serde_json::json!({
            "errorKind": "rate_limit",
            "resets_at": "2099-01-01T00:00:00Z",
        }));
        let info = classify_rate_limit_error(&err).expect("classified");
        assert_eq!(info.resets_at.to_rfc3339(), "2099-01-01T00:00:00+00:00");
    }

    #[test]
    fn classify_rate_limit_ignores_unrelated_errors() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "transport closed".into();
        err.data = Some(serde_json::json!({ "errorKind": "internal" }));
        assert!(classify_rate_limit_error(&err).is_none());

        let err = agent_client_protocol::Error::invalid_params();
        assert!(classify_rate_limit_error(&err).is_none());
    }

    #[test]
    fn classify_rate_limit_from_message_matches_acp_fingerprint() {
        let msg = "ACP connection failed: Internal error: You've hit your limit · resets 12:10pm (Europe/Paris): {\n  \"errorKind\":\"rate_limit\"\n}";
        let info = classify_rate_limit_from_message(msg).expect("classified");
        assert_eq!(info.kind, "rate_limit");
        // Spaced variant the adapter sometimes emits.
        let info_spaced = classify_rate_limit_from_message("{\n  \"errorKind\": \"rate_limit\"\n}")
            .expect("classified");
        assert_eq!(info_spaced.kind, "rate_limit");
        assert!(classify_rate_limit_from_message("connection refused").is_none());
    }

    /// Regression for issue #2414 on the structured-view path: `from_info`
    /// must use the create-time-pinned `SandboxInfo::container_workdir`, not a
    /// live recompute. With the worktree's git linkage broken,
    /// `compute_volume_paths` collapses to `/workspace/<basename>` (a path the
    /// container never mounted), so the pin has to win.
    #[test]
    fn from_info_prefers_pinned_workdir_over_live_recompute() {
        let tmp = tempfile::tempdir().unwrap();
        // Orphaned worktree: a `.git` file whose gitdir points nowhere.
        let worktree = tmp.path().join("repo-worktrees").join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../does-not-exist/.git/worktrees/feature\n",
        )
        .unwrap();

        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-pinned1".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/repo-worktrees/feature".into()),
        };

        // Pin present, so no live inspect is attempted; the pinned value is used.
        let (resources, _map) = SessionSandbox::from_info(&sandbox, &worktree, None).unwrap();
        assert_eq!(
            resources.container_workdir,
            PathBuf::from("/workspace/repo-worktrees/feature"),
        );

        // The pin is load-bearing: the live recompute on this orphaned worktree
        // collapses to the basename, which is the path that never got mounted.
        let (_volumes, computed) = crate::session::container_config::compute_volume_paths(
            &worktree,
            &worktree.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(computed, "/workspace/feature");
    }

    /// Sandboxed structured view spawn must wrap the agent command in
    /// `docker exec` argv with `-i`, the container workdir, an `-e`
    /// flag per env entry, then the container name, then the agent
    /// argv. The docker binary must be argv[0]. Mirrors the tmux
    /// view's wrap so the same `claude-agent-acp` invocation
    /// goes inside the container instead of running on the host.
    #[test]
    fn build_sandbox_docker_argv_wraps_agent_in_docker_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-abc12345".into(),
            extra_env: Some(vec!["MY_LITERAL=hello".into()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            agent_key: "claude".into(),
            spec: AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec!["--stdio".into()],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd,
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: None,
            stored_acp_session_id: None,
            seed_history_replay: false,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");
        assert!(
            argv.docker_binary == "docker" || argv.docker_binary == "podman",
            "expected docker/podman binary, got {:?}",
            argv.docker_binary
        );
        assert_eq!(argv.docker_args[0], "exec");
        assert_eq!(argv.docker_args[1], "-i");
        assert_eq!(argv.docker_args[2], "-w");
        let cn_idx = argv
            .docker_args
            .iter()
            .position(|a| a == "aoe-sandbox-abc12345")
            .expect("container name in argv");
        let cmd_idx = cn_idx + 1;
        assert_eq!(argv.docker_args[cmd_idx], "claude-agent-acp");
        assert_eq!(argv.docker_args[cmd_idx + 1], "--stdio");
        // Literal env entry lands as `-e KEY=VALUE`.
        assert!(
            argv.docker_args.iter().any(|a| a == "MY_LITERAL=hello"),
            "literal env entry must be propagated as `-e KEY=VALUE`"
        );
        // The literal entry's KEY=VALUE form must NOT also appear in
        // `inherit_env` (that vec is for Inherit-style entries whose
        // value comes from the parent process env, not for literals).
        assert!(
            !argv.inherit_env.iter().any(|(k, _)| k == "MY_LITERAL"),
            "literal entries must not duplicate into inherit_env"
        );
    }

    /// Inherit-style env entries (provider auth keys) must lower into a
    /// pair of `-e KEY` (key only) in docker_args plus a `(KEY, VALUE)`
    /// pair in inherit_env so the runner can re-export the value and
    /// docker can forward it into the container.
    #[test]
    fn build_sandbox_docker_argv_inherit_env_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-abc12345".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            agent_key: "claude".into(),
            spec: AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd,
            additional_dirs: vec![],
            // Per-spawn provider_env entry: must end up Inherit-style.
            provider_env: vec![("ANTHROPIC_API_KEY".into(), "sk-test-value".into())],
            default_effort: None,
            socket_path: None,
            stored_acp_session_id: None,
            seed_history_replay: false,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");
        // The `-e KEY` flag (without value) must appear consecutively.
        let key_flag_idx = argv
            .docker_args
            .windows(2)
            .position(|w| w[0] == "-e" && w[1] == "ANTHROPIC_API_KEY")
            .expect("ANTHROPIC_API_KEY -e flag must be present");
        // Value-typed forms like `-e ANTHROPIC_API_KEY=...` must NOT
        // appear; that would leak the secret into argv.
        assert!(
            !argv
                .docker_args
                .iter()
                .any(|a| a.starts_with("ANTHROPIC_API_KEY=")),
            "secret must not appear as `KEY=VALUE` in argv (slot {key_flag_idx})"
        );
        // The value must travel via inherit_env so the parent process
        // sets it before exec-ing docker.
        assert_eq!(
            argv.inherit_env
                .iter()
                .find(|(k, _)| k == "ANTHROPIC_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("sk-test-value"),
        );
    }

    /// `CLAUDE_CONFIG_DIR` is a host filesystem path, not a value, so
    /// it must NOT be auto-forwarded into the container even when set
    /// on the host. The agent's config dir is bind-mounted at the
    /// canonical container path by `AGENT_CONFIG_MOUNTS`.
    ///
    /// Tagged `#[serial]` because the test mutates the process-wide
    /// env; parallel readers of `std::env::var` would race.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_drops_host_only_claude_config_dir() {
        // Set the env var to simulate the host having it; the function
        // under test must still skip it.
        let prev = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/Users/operator/.claude");
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-cfgdir".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            agent_key: "claude".into(),
            spec: AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd: tmp.path().to_path_buf(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: None,
            stored_acp_session_id: None,
            seed_history_replay: false,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert!(
            !argv.docker_args.iter().any(|a| a == "CLAUDE_CONFIG_DIR"),
            "CLAUDE_CONFIG_DIR is a host path and must not be forwarded as `-e KEY`"
        );
        assert!(
            !argv
                .docker_args
                .iter()
                .any(|a| a.starts_with("CLAUDE_CONFIG_DIR=")),
            "CLAUDE_CONFIG_DIR must not appear as a literal `KEY=VALUE` either"
        );
        assert!(
            !argv
                .inherit_env
                .iter()
                .any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "CLAUDE_CONFIG_DIR must not land in inherit_env"
        );
    }

    #[tokio::test]
    async fn spawn_with_nonexistent_command_errors_cleanly() {
        let config = SpawnConfig {
            agent_key: "claude".into(),
            spec: AgentSpec {
                command: "/nonexistent/agent/binary/aoe-test".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: None,
            stored_acp_session_id: None,
            seed_history_replay: false,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let result = AcpClient::spawn(config, AcpSessionId("s-1".into())).await;
        assert!(matches!(result, Err(AcpError::Spawn(_))));
    }

    /// Pre-flight cwd check: when `project_path` was renamed out from
    /// under the session, the supervisor's spawn fails with a typed
    /// `ProjectPathMissing` instead of a bare ENOENT-mapped `Spawn`.
    /// See #1089.
    #[tokio::test]
    async fn spawn_returns_project_path_missing_when_cwd_does_not_exist() {
        let missing =
            std::env::temp_dir().join(format!("aoe-test-missing-cwd-{}", std::process::id()));
        // Ensure the path truly does not exist.
        let _ = std::fs::remove_dir_all(&missing);
        let config = SpawnConfig {
            agent_key: "claude".into(),
            spec: AgentSpec {
                command: "/bin/true".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd: missing.clone(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: None,
            stored_acp_session_id: None,
            seed_history_replay: false,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let result = AcpClient::spawn(config, AcpSessionId("s-1".into())).await;
        match result {
            Err(AcpError::ProjectPathMissing { path }) => assert_eq!(path, missing),
            Err(other) => panic!("expected ProjectPathMissing, got {other:?}"),
            Ok(_) => panic!("expected ProjectPathMissing, got Ok"),
        }
    }

    /// Belt-and-suspenders: even if the pre-flight raced (cwd vanishes
    /// between `cwd.exists()` and `Command::spawn`), the classifier turns
    /// the raw ENOENT into `ProjectPathMissing` rather than the generic
    /// install-the-adapter message.
    #[test]
    fn classify_spawn_error_routes_missing_cwd_to_project_path_missing() {
        let missing =
            std::env::temp_dir().join(format!("aoe-test-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::classify_spawn_error(io_err, &missing, "/bin/true") {
            AcpError::ProjectPathMissing { path } => assert_eq!(path, missing),
            other => panic!("expected ProjectPathMissing, got {other:?}"),
        }
    }

    #[test]
    fn classify_spawn_error_keeps_spawn_when_cwd_exists() {
        let cwd = std::env::temp_dir();
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::classify_spawn_error(io_err, &cwd, "/nonexistent/bin/foo") {
            AcpError::Spawn(msg) => {
                assert!(
                    msg.contains("/nonexistent/bin/foo"),
                    "spawn message should echo command: {msg}"
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_spawn_error_appends_install_hint_for_known_agent() {
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::missing_binary_spawn_error(&io_err, "codex-acp") {
            AcpError::Spawn(msg) => {
                assert!(msg.contains("codex-acp"), "should echo the binary: {msg}");
                assert!(
                    msg.contains("Install with: npm install -g @zed-industries/codex-acp"),
                    "should append the exact install command: {msg}"
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_spawn_error_omits_hint_for_unknown_binary() {
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::missing_binary_spawn_error(&io_err, "totally-unknown-bin") {
            AcpError::Spawn(msg) => {
                assert!(
                    msg.contains("totally-unknown-bin"),
                    "should echo binary: {msg}"
                );
                assert!(!msg.contains("Install with:"), "no hint for unknown: {msg}");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn classify_spawn_error_passes_through_non_enoent() {
        let cwd = std::env::temp_dir();
        let io_err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        match AcpError::classify_spawn_error(io_err, &cwd, "/bin/true") {
            AcpError::Spawn(_) => {}
            other => panic!("expected Spawn for non-ENOENT, got {other:?}"),
        }
    }

    #[test]
    fn map_update_to_events_threads_parent_tool_call_id() {
        use agent_client_protocol::schema::{SessionUpdate, ToolCall as AcpToolCall};
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-task-1" }),
        );
        let mut tc = AcpToolCall::new("tc-child-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        tc.meta = Some(meta);
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CLAUDE);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        let started = started.expect("ToolCallStarted emitted");
        assert_eq!(started.parent_tool_call_id.as_deref(), Some("tc-task-1"),);
    }

    #[test]
    fn map_update_to_events_leaves_parent_none_when_meta_missing() {
        use agent_client_protocol::schema::{SessionUpdate, ToolCall as AcpToolCall};
        let mut tc = AcpToolCall::new("tc-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CLAUDE);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        assert!(started.unwrap().parent_tool_call_id.is_none());
    }

    fn text_chunk(text: &str, id: Option<&str>) -> SessionUpdate {
        use agent_client_protocol::schema::{ContentBlock, ContentChunk, TextContent};
        let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        if let Some(id) = id {
            chunk = chunk.message_id(id);
        }
        SessionUpdate::AgentMessageChunk(chunk)
    }

    fn tool_update() -> SessionUpdate {
        use agent_client_protocol::schema::ToolCall as AcpToolCall;
        SessionUpdate::ToolCall(AcpToolCall::new("t-dedup", "Read"))
    }

    #[test]
    fn dedup_drops_consolidated_restatement_after_deltas() {
        // The reported leak: empty marker + two streamed deltas sharing the
        // streamed id, then the whole block re-sent under a different id.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", Some("m1"))));
        assert!(!d.observe(&text_chunk(
            "Concrete repro. Let me inspect the events around lgtm and",
            Some("m1")
        )));
        assert!(!d.observe(&text_chunk(
            " the \"Plan approved\" message in that session.",
            Some("m1")
        )));
        // Consolidated copy carries the mismatched id and restates the block.
        assert!(d.observe(&text_chunk(
            "Concrete repro. Let me inspect the events around lgtm and the \"Plan approved\" message in that session.",
            Some("m2")
        )));
    }

    #[test]
    fn dedup_drops_restatement_when_delta_ids_absent() {
        // The recurrence (sessions 0c425453, 614231): when `message_start`
        // carries no id, the streamed deltas arrive id-less, but the leaked
        // consolidated copy still carries a fallback uuid. The ids differ by
        // presence, so the verbatim restatement must still be dropped.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", None)));
        assert!(!d.observe(&text_chunk("Getting the real failure log,", None)));
        assert!(!d.observe(&text_chunk(" not guessing this time.", None)));
        assert!(d.observe(&text_chunk(
            "Getting the real failure log, not guessing this time.",
            Some("uuid-1")
        )));
    }

    #[test]
    fn dedup_drops_single_delta_restatement() {
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("hello world", Some("m1"))));
        assert!(d.observe(&text_chunk("hello world", Some("m2"))));
    }

    #[test]
    fn dedup_keeps_legitimate_repeated_same_id_delta() {
        // Two identical deltas that share a message id are genuine streamed
        // output ("haha"), not a restatement. Never dropped.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", Some("m1"))));
        assert!(!d.observe(&text_chunk("ha", Some("m1"))));
        assert!(!d.observe(&text_chunk("ha", Some("m1"))));
    }

    #[test]
    fn dedup_resets_on_boundary_and_handles_adjacent_blocks() {
        let mut d = AgentMessageDedup::default();
        // Block 1: delta then restatement, dropped.
        assert!(!d.observe(&text_chunk("ab", Some("m1"))));
        assert!(d.observe(&text_chunk("ab", Some("m2"))));
        // A tool call ends the block.
        assert!(!d.observe(&tool_update()));
        // Block 2 reuses text "ab": the first chunk after the boundary must
        // not be mistaken for a restatement of the closed block.
        assert!(!d.observe(&text_chunk("ab", Some("m3"))));
        assert!(d.observe(&text_chunk("ab", Some("m4"))));
    }

    #[test]
    fn dedup_never_drops_when_ids_absent() {
        // Without message ids the delta-vs-restatement distinction is
        // ambiguous; degrade to never-drop so real output is never corrupted.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", None)));
        assert!(!d.observe(&text_chunk("done", None)));
        assert!(!d.observe(&text_chunk("done", None)));
    }

    #[test]
    fn dedup_reset_forgets_in_flight_block() {
        // Mirrors the suppression path: reset() between a block's deltas and
        // its restatement means the restatement is treated as a fresh block.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("ab", Some("m1"))));
        d.reset();
        assert!(!d.observe(&text_chunk("ab", Some("m2"))));
    }

    #[test]
    fn map_update_to_events_does_not_link_parent_for_unverified_agents() {
        use agent_client_protocol::schema::{SessionUpdate, ToolCall as AcpToolCall};
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-task-1" }),
        );
        let mut tc = AcpToolCall::new("tc-child-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        tc.meta = Some(meta);
        // Codex profile lists no parent_meta_namespaces, so the linkage
        // doesn't render even when claude's namespace happens to be on
        // the wire.
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CODEX);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        assert!(started.unwrap().parent_tool_call_id.is_none());
    }

    #[test]
    fn parse_plan_steps_extracts_dash_and_numbered_bullets() {
        let md = "Here's the plan:\n\n- First, **read** the file\n- Then patch it\n1. Run tests\n2. Commit\n\nOther prose.";
        let steps = parse_plan_steps(md);
        let titles: Vec<&str> = steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "First, read the file",
                "Then patch it",
                "Run tests",
                "Commit"
            ]
        );
        for s in &steps {
            assert!(matches!(s.status, PlanStepStatus::Pending));
        }
    }

    #[test]
    fn parse_plan_steps_returns_empty_when_no_bullets() {
        assert!(parse_plan_steps("Just a paragraph with no list.").is_empty());
        assert!(parse_plan_steps("").is_empty());
    }

    #[test]
    fn extract_plan_from_switch_mode_handles_missing_plan_field() {
        let v = serde_json::json!({});
        assert!(extract_plan_from_switch_mode(&v).is_none());
        let v = serde_json::json!({ "plan": 42 });
        assert!(extract_plan_from_switch_mode(&v).is_none());
    }

    #[test]
    fn extract_plan_from_switch_mode_builds_plan_when_input_has_bullets() {
        let v = serde_json::json!({
            "plan": "- Step one\n- Step two\n- Step three"
        });
        let plan = extract_plan_from_switch_mode(&v).expect("plan should parse");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].title, "Step one");
    }

    #[test]
    fn strip_markdown_emphasis_unwraps_bold_and_italic() {
        assert_eq!(strip_markdown_emphasis("**bold**"), "bold");
        assert_eq!(strip_markdown_emphasis("__bold__"), "bold");
        assert_eq!(strip_markdown_emphasis("*italic*"), "italic");
        assert_eq!(strip_markdown_emphasis("_italic_"), "italic");
        assert_eq!(
            strip_markdown_emphasis("mix of **bold** and *italic*"),
            "mix of bold and italic"
        );
        assert_eq!(strip_markdown_emphasis("plain"), "plain");
    }

    #[test]
    fn is_compact_completion_matches_adapter_string() {
        assert!(is_compact_completion("Compacting completed."));
        assert!(is_compact_completion("\n\nCompacting completed.\n"));
        assert!(!is_compact_completion("Compacting..."));
        assert!(!is_compact_completion("compact done"));
        assert!(!is_compact_completion(""));
    }

    #[test]
    fn resolve_agent_command_returns_none_for_absolute_path() {
        assert!(resolve_agent_command("/usr/local/bin/claude-agent-acp").is_none());
        assert!(resolve_agent_command("./relative/path").is_none());
    }

    #[test]
    fn resolve_agent_command_returns_none_for_placeholder() {
        assert!(resolve_agent_command("${aoe_data_dir}/acp-worker/dist/aoe-agent").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_agent_command_finds_binary_in_path_env() {
        // Build a temp dir with a fake binary, point PATH at it.
        // Tagged `#[serial]` because the test mutates the process-wide
        // PATH; any concurrent test that reads PATH (e.g. resolves a
        // real binary) would race.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("aoe-test-resolver-fake");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("PATH");
        let new_path = format!(
            "{}:{}",
            dir.path().display(),
            prev.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        // SAFETY: this test mutates the process-wide PATH. Other PATH
        // readers in the same test binary would race; `#[serial]` keeps
        // them apart.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }
        let resolved = resolve_agent_command("aoe-test-resolver-fake");
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var("PATH", prev);
            }
        }
        let (path, parent) = resolved.expect("binary should resolve from PATH");
        assert_eq!(path, bin);
        assert_eq!(parent, dir.path());
    }

    #[test]
    fn pick_option_id_finds_allow_once() {
        use agent_client_protocol::schema::{PermissionOption, PermissionOptionId};
        let options = vec![
            PermissionOption::new(
                PermissionOptionId::new("yes"),
                "Allow this once",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::new("no"),
                "Reject",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        let id = pick_option_id(&options, ApprovalDecision::Allow).unwrap();
        assert_eq!(id.0.as_ref(), "yes");
    }

    #[test]
    fn pick_option_id_falls_back() {
        use agent_client_protocol::schema::{PermissionOption, PermissionOptionId};
        let options = vec![PermissionOption::new(
            PermissionOptionId::new("always"),
            "Always",
            PermissionOptionKind::AllowAlways,
        )];
        // We asked for Allow (prefers AllowOnce); the agent only offered
        // AllowAlways. Falls back gracefully.
        let id = pick_option_id(&options, ApprovalDecision::Allow).unwrap();
        assert_eq!(id.0.as_ref(), "always");
    }

    #[test]
    fn preview_optional_args_empty_for_missing_or_null() {
        // #1713: a missing or explicitly-null raw_input must preview as
        // empty (so the UI shows a clean empty-state) rather than the
        // literal "null" that preview_args(&Value::Null) would produce.
        assert_eq!(preview_optional_args(None), "");
        assert_eq!(preview_optional_args(Some(&serde_json::Value::Null)), "");
        let obj = serde_json::json!({ "command": "ls" });
        assert_eq!(preview_optional_args(Some(&obj)), r#"{"command":"ls"}"#);
    }

    #[test]
    fn permission_raw_input_uses_cached_context_when_request_is_empty() {
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode", "workdir": "/tmp" });
        let enriched = permission_raw_input_with_context(None, Some(&cached))
            .expect("cached context should be used");

        assert_eq!(enriched.get("command"), cached.get("command"));
        assert_eq!(enriched.get("workdir"), cached.get("workdir"));
    }

    #[test]
    fn permission_raw_input_merges_cached_context_without_overwriting_request() {
        let permission =
            serde_json::json!({ "filepath": "/tmp/opencode", "command": "permission command" });
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode", "workdir": "/tmp" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("non-empty permission args should remain present");

        assert_eq!(enriched.get("command"), permission.get("command"));
        assert_eq!(enriched.get("filepath"), permission.get("filepath"));
        assert_eq!(enriched.get("workdir"), cached.get("workdir"));
    }

    #[test]
    fn permission_raw_input_preserves_aoe_metadata_when_falling_back() {
        let permission = serde_json::json!({ "_aoe_title": "external_directory" });
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("cached context should enrich bookkeeping-only requests");

        assert_eq!(enriched.get("_aoe_title"), permission.get("_aoe_title"));
        assert_eq!(enriched.get("command"), cached.get("command"));
    }

    #[test]
    fn permission_raw_input_keeps_non_empty_non_object_request() {
        let permission = serde_json::json!(["already", "specific"]);
        let cached = serde_json::json!({ "command": "mkdir -p /tmp/opencode" });
        let enriched = permission_raw_input_with_context(Some(&permission), Some(&cached))
            .expect("non-empty permission args should remain present");

        assert_eq!(enriched, permission);
    }

    #[test]
    fn permission_raw_input_ignores_empty_cached_context() {
        let cached = serde_json::json!({});
        let enriched = permission_raw_input_with_context(None, Some(&cached));

        assert!(enriched.is_none());
    }

    #[test]
    fn tool_context_cache_ignores_empty_context_entries() {
        let mut cache = ToolCallContextCache::default();
        cache.record("tc-1".to_string(), serde_json::json!({}));

        assert!(cache.get("tc-1").is_none());
    }

    #[test]
    fn tool_context_cache_removes_completed_entries() {
        let mut cache = ToolCallContextCache::default();
        cache.record("tc-1".to_string(), serde_json::json!({ "command": "ls" }));
        assert!(cache.get("tc-1").is_some());

        cache.remove("tc-1");

        assert!(cache.get("tc-1").is_none());
        assert!(!cache.insertion_order.iter().any(|id| id == "tc-1"));
    }

    #[test]
    fn tool_context_cache_enforces_bounded_size() {
        let mut cache = ToolCallContextCache::default();
        for idx in 0..=TOOL_CONTEXT_CACHE_LIMIT {
            cache.record(format!("tc-{idx}"), serde_json::json!({ "idx": idx }));
        }

        assert_eq!(cache.raw_inputs.len(), TOOL_CONTEXT_CACHE_LIMIT);
        assert!(cache.get("tc-0").is_none());
        assert!(cache
            .get(&format!("tc-{TOOL_CONTEXT_CACHE_LIMIT}"))
            .is_some());
    }

    #[test]
    fn preview_args_caps_to_16k() {
        let big = serde_json::Value::String("x".repeat(20_000));
        let preview = preview_args(&big);
        assert!(preview.len() <= 16 * 1024 + 32);
        assert!(preview.contains("[truncated]"));
    }

    #[test]
    fn extract_tool_content_text_concats_text_blocks() {
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![
            ToolCallContent::Content(Content::new("stdout line 1")),
            ToolCallContent::Content(Content::new("stdout line 2")),
        ];
        let text = extract_tool_content_text(&blocks);
        assert_eq!(text, "stdout line 1\nstdout line 2");
    }

    #[test]
    fn extract_tool_content_text_empty_for_no_text_blocks() {
        // No content → empty string. The reducer falls back to the
        // status word ("completed" / "tool failed") in that case so
        // the card still conveys state.
        assert_eq!(extract_tool_content_text(&[]), "");
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_async_agent_prefix() {
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "Async agent launched successfully.\nagentId: af2a6a5d46bc21f91 (internal ID)",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::AsyncAgent)
        );
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_background_command_prefix() {
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "Command running in background with ID: bgxe33hwb. Output is being written to: /tmp/x",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::BackgroundCommand)
        );
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_regular_completion() {
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "abc1234 first commit\nabc1235 second commit",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks)).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_none_content() {
        assert!(detect_off_protocol_work_completed(&None).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_empty_content() {
        assert!(detect_off_protocol_work_completed(&Some(vec![])).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_ignores_echoed_marker_mid_line() {
        // CodeRabbit regression on PR #1406: a regular foreground Bash that
        // prints the SDK marker substring as part of its output (e.g.
        // an echo or grep that includes the phrase) must NOT trip
        // off-protocol suppression. Match anchors at the start of a
        // line, not anywhere in the content.
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "user typed: Command running in background with ID: pretend\nbye",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks)).is_none());

        let blocks2 = vec![ToolCallContent::Content(Content::new(
            "log line: Async agent launched successfully but actually not",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks2)).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_marker_on_indented_line() {
        // The marker may not be the first character of the block;
        // a leading newline or whitespace must not break detection
        // as long as the marker starts the (trimmed) line.
        use agent_client_protocol::schema::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "\n  Command running in background with ID: btest. log: /tmp/x",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::BackgroundCommand)
        );
    }

    #[test]
    fn wakeup_lifecycle_signal_from_completed_tool_call_update() {
        use agent_client_protocol::schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-1", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(matches!(sig, Some(LifecycleSignal::WakeupPending { .. })));
    }

    #[test]
    fn wakeup_lifecycle_signal_none_on_initial_tool_call() {
        // The initial `ToolCall` frame is NOT yet a completion; the
        // tool could still fail. Watchdog suppression must wait until
        // a successful ToolCallUpdate { Completed }. See CodeRabbit
        // review on PR #1406.
        use agent_client_protocol::schema::ToolCall;
        let mut tc = ToolCall::new("tc-wake-2", "ScheduleWakeup");
        tc.raw_input = Some(serde_json::json!({ "delaySeconds": 60 }));
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCall(tc),
            &agent_profiles::CLAUDE,
        );
        assert!(sig.is_none());
    }

    #[test]
    fn wakeup_lifecycle_signal_none_on_failed_completion() {
        // A failed ScheduleWakeup means no wakeup was actually
        // registered; suppressing for `delay + base_grace` would
        // hide a real adapter wedge.
        use agent_client_protocol::schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-3", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(sig.is_none());
    }

    #[test]
    fn wakeup_lifecycle_signal_fires_on_in_progress_with_raw_input() {
        // Real `claude-agent-acp` typically populates `raw_input` on an
        // interim `ToolCallUpdate { status: InProgress }` and strips
        // it from the final `Completed` frame. Requiring strictly
        // Completed status would lose the wakeup; we gate only on
        // not-Failed.
        use agent_client_protocol::schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-4", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(matches!(sig, Some(LifecycleSignal::WakeupPending { .. })));
    }

    #[test]
    fn classify_watchdog_notification_signals_ignores_ambient_updates() {
        use agent_client_protocol::schema::{
            AvailableCommand as AcpAvailableCommand, AvailableCommandsUpdate,
        };
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AcpAvailableCommand::new("review", "Review changes"),
        ]));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, false);
        assert!(
            lifecycle.is_none() && wakeup.is_none(),
            "ambient updates must not count as watchdog activity"
        );
    }

    #[test]
    fn classify_watchdog_notification_signals_marks_lifecycle_updates() {
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-lifecycle-1",
            ToolCallUpdateFields::new(),
        ));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, false);
        assert!(
            lifecycle.is_some() && wakeup.is_none(),
            "tool lifecycle updates must disarm the resume-idle watchdog"
        );
    }

    #[test]
    fn classify_watchdog_notification_signals_suppresses_during_history_replay() {
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-suppressed-1",
            ToolCallUpdateFields::new(),
        ));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, true);
        assert!(
            lifecycle.is_none() && wakeup.is_none(),
            "post-load replay suppression must block watchdog signals"
        );
    }

    #[test]
    fn classify_lifecycle_signal_marks_async_agent_completion() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully. agentId: async-test-1",
            ))]);
        let update = ToolCallUpdate::new("tc-async-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-async-1");
                assert!(succeeded);
                assert_eq!(off_protocol_work, Some(OffProtocolWorkKind::AsyncAgent));
            }
            other => panic!(
                "expected ToolCompleted {{ off_protocol_work: Some(AsyncAgent) }}, got {other:?}"
            ),
        }
    }

    #[test]
    fn classify_lifecycle_signal_marks_background_command_completion() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Command running in background with ID: bgtest. Output is being written to: /tmp/x",
            ))]);
        let update = ToolCallUpdate::new("tc-bg-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-bg-1");
                assert!(succeeded);
                assert_eq!(
                    off_protocol_work,
                    Some(OffProtocolWorkKind::BackgroundCommand)
                );
            }
            other => panic!(
                "expected ToolCompleted {{ off_protocol_work: Some(BackgroundCommand) }}, got {other:?}"
            ),
        }
    }

    #[test]
    fn classify_lifecycle_signal_clears_off_protocol_on_regular_completion() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "ls /tmp/foo done",
            ))]);
        let update = ToolCallUpdate::new("tc-bash-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-bash-1");
                assert!(succeeded);
                assert!(off_protocol_work.is_none());
            }
            other => panic!("expected ToolCompleted {{ off_protocol_work: None }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_lifecycle_signal_failed_ignores_off_protocol_marker() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully. agentId: not-real",
            ))]);
        let update = ToolCallUpdate::new("tc-failed-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                succeeded,
                off_protocol_work,
                ..
            }) => {
                assert!(!succeeded, "Failed updates must mark succeeded=false");
                assert!(
                    off_protocol_work.is_none(),
                    "Failed updates must not activate off-protocol suppression"
                );
            }
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn watchdog_failed_background_tool_does_not_suppress() {
        // Regression for the post-impl review: a backgrounded Bash that
        // FAILS to launch (e.g., binary not found, raw_input parse
        // error) must not spuriously enable off-protocol suppression
        // via the raw_input fallback. The subprocess never actually
        // started, so the watchdog must keep its base grace.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-fail".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-fail".into(),
                succeeded: false,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(
            w.off_protocol_work_seen().is_none(),
            "Failed background tool must not enable off-protocol suppression",
        );
        // Watchdog uses base grace (120s) and fires after it elapses.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
    }

    #[tokio::test]
    async fn watchdog_later_in_progress_does_not_clobber_background_flag() {
        // Regression for the post-impl review: claude-agent-acp emits a
        // `ToolCall` carrying `raw_input.run_in_background` followed by
        // a `ToolCallUpdate { status: InProgress }` that lacks raw_input
        // and classifies as `is_background_task: false`. A blind
        // `insert` would overwrite the sticky `true` and silently
        // disable the raw-input arm of the defense-in-depth detection.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-3".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Later InProgress update with the same id but no background flag.
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-3".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Completion without content marker: only the raw-input flag is
        // available, and it must still be `true` after the InProgress
        // re-stamp.
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-3".into(),
                succeeded: true,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "background flag must survive an intervening ToolStarted without the flag",
        );
    }

    #[test]
    fn lifecycle_envelope_round_trips_epoch_and_signal() {
        // Smoke test that `LifecycleEnvelope` carries both fields as
        // expected: the prompt-loop discard path keys off `epoch`
        // mismatch, so a regression that loses or zeroes the field
        // would silently break cross-prompt stale-signal protection.
        let env = LifecycleEnvelope {
            epoch: 42,
            signal: LifecycleSignal::WakeupPending {
                at: chrono::Utc::now(),
            },
        };
        assert_eq!(env.epoch, 42);
        assert!(matches!(env.signal, LifecycleSignal::WakeupPending { .. }));
    }

    #[test]
    fn classify_lifecycle_signal_tool_call_carries_run_in_background_flag() {
        use agent_client_protocol::schema::ToolCall;
        let mut tc = ToolCall::new("tc-bg-2", "Bash");
        tc.raw_input = Some(serde_json::json!({
            "command": "npm install",
            "run_in_background": true,
        }));
        match classify_lifecycle_signal(&SessionUpdate::ToolCall(tc)) {
            Some(LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            }) => {
                assert_eq!(id, "tc-bg-2");
                assert!(
                    is_background_task,
                    "raw_input.run_in_background=true must propagate"
                );
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn background_agent_launched_parsed_from_agent_meta() {
        let payload = serde_json::json!({
            "_meta": { "claudeCode": {
                "toolName": "Agent",
                "toolResponse": {
                    "agentId": "a3d5ae46a7a0414b1",
                    "description": "grep tmux mentions repo-wide",
                    "prompt": "Grep the repo for tmux.",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "outputFile": "/tmp/x/tasks/a3d5ae46a7a0414b1.output",
                    "status": "async_launched"
                }
            }},
            "toolCallId": "toolu_012yUZykQT2vqFXZTvqWev5e"
        });
        match background_agent_launched_from_value(&payload) {
            Some(Event::BackgroundAgentLaunched {
                agent_id,
                tool_call_id,
                description,
                model,
                output_file,
                ..
            }) => {
                assert_eq!(agent_id, "a3d5ae46a7a0414b1");
                assert_eq!(tool_call_id, "toolu_012yUZykQT2vqFXZTvqWev5e");
                assert_eq!(description, "grep tmux mentions repo-wide");
                assert_eq!(model, "claude-opus-4-8[1m]");
                assert!(output_file.ends_with(".output"));
            }
            other => panic!("expected BackgroundAgentLaunched, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_meta_emits_background_agent_launched() {
        // The real path: the async launch arrives as a metadata-only
        // ToolCallUpdate (no status/content/title), carrying the Agent
        // payload under `_meta.claudeCode`. It must map to a typed
        // BackgroundAgentLaunched, not a raw passthrough. This is the
        // path the unit test on the helper alone did not cover.
        use agent_client_protocol::schema::{SessionUpdate, ToolCallUpdate, ToolCallUpdateFields};
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({
                "toolName": "Agent",
                "toolResponse": {
                    "agentId": "a6654829ea0a19032",
                    "description": "grep tmux mentions repo-wide",
                    "prompt": "Grep the repo for tmux.",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "outputFile": "/tmp/x/tasks/a6654829ea0a19032.output",
                    "status": "async_launched"
                }
            }),
        );
        let mut update = ToolCallUpdate::new("toolu_01HzYCZK", ToolCallUpdateFields::new());
        update.meta = Some(meta);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::BackgroundAgentLaunched {
                agent_id,
                description,
                output_file,
                ..
            } => Some((agent_id.clone(), description.clone(), output_file.clone())),
            _ => None,
        }) {
            Some((id, desc, out)) => {
                assert_eq!(id, "a6654829ea0a19032");
                assert_eq!(desc, "grep tmux mentions repo-wide");
                assert!(out.ends_with(".output"));
            }
            None => panic!("expected BackgroundAgentLaunched, got {events:?}"),
        }
        // It must NOT also leak a RawAgentUpdate for the same payload.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::RawAgentUpdate { .. })),
            "async launch should not also pass through as RawAgentUpdate"
        );
    }

    #[test]
    fn background_agent_launched_ignores_non_agent_meta() {
        // A normal tool-response RawAgentUpdate must not be promoted.
        let bash = serde_json::json!({
            "_meta": { "claudeCode": { "toolName": "Bash", "toolResponse": {} } }
        });
        assert!(background_agent_launched_from_value(&bash).is_none());
        // An Agent update that is not an async launch (no status) stays raw.
        let sync = serde_json::json!({
            "_meta": { "claudeCode": { "toolName": "Agent", "toolResponse": {
                "agentId": "x"
            }}}
        });
        assert!(background_agent_launched_from_value(&sync).is_none());
        assert!(background_agent_launched_from_value(&serde_json::json!({})).is_none());
    }

    #[test]
    fn classify_lifecycle_signal_tool_call_defaults_run_in_background_false() {
        use agent_client_protocol::schema::ToolCall;
        let mut tc = ToolCall::new("tc-fg-1", "Bash");
        tc.raw_input = Some(serde_json::json!({ "command": "ls" }));
        match classify_lifecycle_signal(&SessionUpdate::ToolCall(tc)) {
            Some(LifecycleSignal::ToolStarted {
                is_background_task, ..
            }) => assert!(!is_background_task),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_completed_carries_content() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "abc1234 first commit",
            ))]);
        let update = ToolCallUpdate::new("tc-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                completed_at: _,
                ..
            } => {
                assert_eq!(tool_call_id, "tc-1");
                assert!(!*is_error);
                assert_eq!(content, "abc1234 first commit");
            }
            other => panic!("expected ToolCallCompleted, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_flags_async_subagent_launch() {
        // Claude's async `Task` tool completes immediately with the SDK
        // marker while the sub-agent runs off-protocol. The completion
        // event must carry async_subagent so renderers draw a background
        // card and drop the marker body (it leaks an internal agent id).
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully\nagentId: ae6f0567246843e25 (internal ID)",
            ))]);
        let update = ToolCallUpdate::new("tc-async", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::ToolCallCompleted { async_subagent, .. } => Some(*async_subagent),
            _ => None,
        }) {
            Some(flag) => assert!(flag, "async sub-agent launch must set async_subagent"),
            None => panic!("expected a ToolCallCompleted event, got {events:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_normal_completion_is_not_async_subagent() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new("done"))]);
        let update = ToolCallUpdate::new("tc-normal", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::ToolCallCompleted { async_subagent, .. } => Some(*async_subagent),
            _ => None,
        }) {
            Some(flag) => assert!(!flag, "normal completion must not set async_subagent"),
            None => panic!("expected a ToolCallCompleted event, got {events:?}"),
        }
    }

    #[test]
    fn map_user_message_chunk_becomes_user_prompt_sent() {
        // Imported sessions replay prior user turns as user_message_chunk
        // (#2276); they must map to UserPromptSent so the user's bubbles
        // render, not get dropped to a raw event.
        use agent_client_protocol::schema::{ContentBlock, ContentChunk, TextContent};
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello from the past")));
        let events = map_update_to_events(
            SessionUpdate::UserMessageChunk(chunk),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::UserPromptSent { text, attachments } => {
                assert_eq!(text, "hello from the past");
                assert!(attachments.is_empty());
            }
            other => panic!("expected UserPromptSent, got {other:?}"),
        }
    }

    fn mode_from_current_mode_update(id: &str) -> SessionMode {
        use agent_client_protocol::schema::CurrentModeUpdate;
        let events = map_update_to_events(
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(id.to_string())),
            &agent_profiles::CLAUDE,
        );
        // The arm always emits the raw id alongside the legacy enum, in order.
        match events.as_slice() {
            [Event::CurrentModeChanged { current_mode_id }, Event::ModeChanged { mode }] => {
                assert_eq!(current_mode_id, id, "raw mode id must be preserved");
                *mode
            }
            other => panic!("expected [CurrentModeChanged, ModeChanged], got {other:?}"),
        }
    }

    #[test]
    fn current_mode_update_classifies_gemini_mode_ids() {
        // Gemini-cli ApprovalMode ids surfaced over `gemini --acp`. See #1819.
        assert_eq!(
            mode_from_current_mode_update("yolo"),
            SessionMode::BypassPermissions
        );
        assert_eq!(
            mode_from_current_mode_update("auto_edit"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("autoEdit"),
            SessionMode::AcceptEdits
        );
    }

    #[test]
    fn current_mode_update_keeps_existing_mode_ids() {
        // Regression guard: non-Gemini classification is unchanged.
        assert_eq!(
            mode_from_current_mode_update("default"),
            SessionMode::Default
        );
        assert_eq!(mode_from_current_mode_update("plan"), SessionMode::Plan);
        assert_eq!(
            mode_from_current_mode_update("accept_edits"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("acceptEdits"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("bypass_permissions"),
            SessionMode::BypassPermissions
        );
        assert_eq!(
            mode_from_current_mode_update("bypassPermissions"),
            SessionMode::BypassPermissions
        );
        // Unknown ids still fall back to Default.
        assert_eq!(
            mode_from_current_mode_update("some_future_mode"),
            SessionMode::Default
        );
    }

    #[test]
    fn is_mode_advertised_matches_normalized_ids() {
        let ids = Some(vec!["acceptEdits".to_string(), "plan".to_string()]);
        // Underscore + case folding both sides.
        assert!(is_mode_advertised("accept_edits", &ids, false));
        assert!(is_mode_advertised("acceptEdits", &ids, false));
        assert!(is_mode_advertised("PLAN", &ids, false));
        // Not in the advertised set.
        assert!(!is_mode_advertised("bypassPermissions", &ids, false));
    }

    #[test]
    fn profile_yolo_mode_ids_pass_the_advertised_guard() {
        use super::super::agent_profiles;
        // The supervisor's post-spawn `set_mode(profile.yolo_mode_id)` is
        // gated by this same `is_mode_advertised` guard. Pin each adapter's
        // YOLO id against the modes that adapter actually advertises, so a
        // mismatch (the #1142 codex bug: `bypassPermissions` vs `full-access`)
        // can't silently get dropped again.
        let claude_modes = Some(vec![
            "auto".to_string(),
            "default".to_string(),
            "acceptEdits".to_string(),
            "plan".to_string(),
            "bypassPermissions".to_string(),
        ]);
        let codex_modes = Some(vec![
            "read-only".to_string(),
            "auto".to_string(),
            "full-access".to_string(),
        ]);

        let claude_yolo = agent_profiles::resolve("claude").yolo_mode_id.unwrap();
        assert!(is_mode_advertised(claude_yolo, &claude_modes, false));

        let codex_yolo = agent_profiles::resolve("codex").yolo_mode_id.unwrap();
        assert!(is_mode_advertised(codex_yolo, &codex_modes, false));
        // The old hard-coded id would NOT survive the guard for codex.
        assert!(!is_mode_advertised(
            "bypassPermissions",
            &codex_modes,
            false
        ));
    }

    #[test]
    fn is_mode_advertised_without_mode_list_defers_to_config_option() {
        // No SessionMode list: the agent steers mode through a config option,
        // so set_mode must NOT be sent (returns false). Without a config-option
        // mode either, fall back to allowing the legacy set_mode (true).
        assert!(!is_mode_advertised("plan", &None, true));
        assert!(is_mode_advertised("plan", &None, false));
    }

    #[test]
    fn map_tool_call_update_in_progress_with_content_emits_streaming_event() {
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Content(Content::new(
                "partial output",
            ))]);
        let update = ToolCallUpdate::new("tc-2", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        // InProgress now emits a ToolCallUpdated re-stamping started_at
        // (#1060 follow-up) plus the streaming ToolCallContent.
        assert_eq!(events.len(), 2);
        match &events[0] {
            Event::ToolCallUpdated {
                tool_call_id,
                started_at,
                ..
            } => {
                assert_eq!(tool_call_id, "tc-2");
                assert!(started_at.is_some());
            }
            other => panic!("expected ToolCallUpdated, got {other:?}"),
        }
        match &events[1] {
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "tc-2");
                assert_eq!(content, "partial output");
            }
            other => panic!("expected ToolCallContent, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_in_progress_restamps_started_at() {
        use agent_client_protocol::schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let update = ToolCallUpdate::new("tc-3", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ToolCallUpdated {
                tool_call_id,
                started_at,
                title,
                args_preview,
                diffs,
            } => {
                assert_eq!(tool_call_id, "tc-3");
                assert!(
                    started_at.is_some(),
                    "InProgress must carry a re-stamped started_at"
                );
                assert!(title.is_none());
                assert!(args_preview.is_none());
                assert!(diffs.is_none());
            }
            other => panic!("expected ToolCallUpdated, got {other:?}"),
        }
    }

    #[test]
    fn extract_diffs_from_content_bridges_diff_blocks_and_ignores_others() {
        use agent_client_protocol::schema::{Content, Diff, ToolCallContent};
        let blocks = vec![
            ToolCallContent::Content(Content::new("some text")),
            ToolCallContent::Diff(Diff::new("src/foo.rs", "new body").old_text("old body")),
            // New-file diff: old_text is None.
            ToolCallContent::Diff(Diff::new("src/new.rs", "created")),
        ];
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), 2, "text blocks must be ignored");
        assert_eq!(diffs[0].path, "src/foo.rs");
        assert_eq!(diffs[0].old_text.as_deref(), Some("old body"));
        assert_eq!(diffs[0].new_text.as_deref(), Some("new body"));
        assert_eq!(diffs[1].path, "src/new.rs");
        assert_eq!(diffs[1].old_text, None, "new file carries no old_text");
        assert_eq!(diffs[1].new_text.as_deref(), Some("created"));
    }

    #[test]
    fn extract_diffs_from_content_caps_per_side_text() {
        use agent_client_protocol::schema::{Diff, ToolCallContent};
        let huge = "x".repeat(MAX_DIFF_TEXT_BYTES + 4096);
        let blocks = vec![ToolCallContent::Diff(
            Diff::new("src/big.rs", huge.clone()).old_text(huge),
        )];
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), 1);
        let new_len = diffs[0].new_text.as_deref().unwrap().len();
        let old_len = diffs[0].old_text.as_deref().unwrap().len();
        assert!(
            new_len < MAX_DIFF_TEXT_BYTES + 64,
            "new_text must be capped, got {new_len}"
        );
        assert!(
            old_len < MAX_DIFF_TEXT_BYTES + 64,
            "old_text must be capped, got {old_len}"
        );
        assert!(diffs[0]
            .new_text
            .as_deref()
            .unwrap()
            .contains("[truncated]"));
    }

    #[test]
    fn extract_tool_output_blocks_empty_for_text_only() {
        use agent_client_protocol::schema::{Content, ToolCallContent};
        // Pure text completion: the `content` string path renders it, so the
        // structured list stays empty and the existing path is untouched.
        let blocks = vec![ToolCallContent::Content(Content::new("just text"))];
        assert!(extract_tool_output_blocks(&blocks).is_empty());
    }

    #[test]
    fn extract_tool_output_blocks_preserves_media_and_resources() {
        use agent_client_protocol::schema::{
            AudioContent, Content, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
            ImageContent, ResourceLink, TextResourceContents, ToolCallContent,
        };
        let blocks =
            vec![
                ToolCallContent::Content(Content::new("a caption")),
                ToolCallContent::Content(Content::new(ContentBlock::Image(
                    ImageContent::new("BASE64IMG", "image/png").uri("file:///shot.png".to_string()),
                ))),
                ToolCallContent::Content(Content::new(ContentBlock::Audio(AudioContent::new(
                    "BASE64AUDIO",
                    "audio/wav",
                )))),
                ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                    ResourceLink::new("report.pdf", "file:///report.pdf"),
                ))),
                ToolCallContent::Content(Content::new(ContentBlock::Resource(
                    EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
                        TextResourceContents::new("inline body", "file:///note.txt"),
                    )),
                ))),
            ];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 5, "all blocks preserved in order: {out:?}");
        assert!(matches!(&out[0], ToolOutputBlock::Text { text } if text == "a caption"));
        match &out[1] {
            ToolOutputBlock::Image {
                mime_type,
                data,
                uri,
            } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data.as_deref(), Some("BASE64IMG"));
                assert_eq!(uri.as_deref(), Some("file:///shot.png"));
            }
            other => panic!("expected Image, got {other:?}"),
        }
        assert!(
            matches!(&out[2], ToolOutputBlock::Audio { mime_type, .. } if mime_type == "audio/wav")
        );
        assert!(
            matches!(&out[3], ToolOutputBlock::ResourceLink { name, uri, .. } if name == "report.pdf" && uri == "file:///report.pdf")
        );
        assert!(
            matches!(&out[4], ToolOutputBlock::Resource { text: Some(t), .. } if t == "inline body")
        );
    }

    #[test]
    fn extract_tool_output_blocks_keeps_blob_resource_payload() {
        // #1818 review: a binary (blob) embedded resource must keep its
        // inline bytes so it stays recoverable as a download.
        use agent_client_protocol::schema::{
            BlobResourceContents, Content, ContentBlock, EmbeddedResource,
            EmbeddedResourceResource, ToolCallContent,
        };
        let blocks = vec![ToolCallContent::Content(Content::new(
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("QkxPQg==", "file:///out.bin")
                        .mime_type(Some("application/octet-stream".to_string())),
                ),
            )),
        ))];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ToolOutputBlock::Resource {
                uri,
                data,
                text,
                mime_type,
            } => {
                assert_eq!(uri, "file:///out.bin");
                assert_eq!(data.as_deref(), Some("QkxPQg=="));
                assert!(text.is_none());
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
            }
            other => panic!("expected Resource, got {other:?}"),
        }
    }

    #[test]
    fn extract_tool_output_blocks_drops_oversized_inline_media() {
        use agent_client_protocol::schema::{Content, ContentBlock, ImageContent, ToolCallContent};
        let huge = "A".repeat(MAX_INLINE_MEDIA_B64 + 1);
        let blocks = vec![ToolCallContent::Content(Content::new(ContentBlock::Image(
            ImageContent::new(huge, "image/png"),
        )))];
        let out = extract_tool_output_blocks(&blocks);
        assert_eq!(out.len(), 1);
        // Oversized inline data is dropped (no uri to fall back on) but the
        // block survives so the card still shows the media placeholder.
        assert!(matches!(
            &out[0],
            ToolOutputBlock::Image {
                data: None,
                uri: None,
                ..
            }
        ));
    }

    #[test]
    fn extract_diffs_from_content_caps_diff_count() {
        use agent_client_protocol::schema::{Diff, ToolCallContent};
        let blocks: Vec<ToolCallContent> = (0..MAX_TOOL_DIFFS + 8)
            .map(|i| ToolCallContent::Diff(Diff::new(format!("f{i}.rs"), "x")))
            .collect();
        let diffs = extract_diffs_from_content(&blocks);
        assert_eq!(diffs.len(), MAX_TOOL_DIFFS, "diff count must be bounded");
    }

    #[test]
    fn map_tool_call_bridges_diff_content_onto_started_tool() {
        // Codex attaches the apply_patch diff to the initial `tool_call`
        // frame as ToolCallContent::Diff. The edit card reads path + preview
        // from ToolCall.diffs, so it must survive ingest. See #1721.
        use agent_client_protocol::schema::{Diff, ToolCall, ToolCallContent, ToolKind};
        let mut tc = ToolCall::new("tc-edit-1", "Edit src/foo.rs");
        tc.kind = ToolKind::Edit;
        tc.content = vec![ToolCallContent::Diff(
            Diff::new("src/foo.rs", "new").old_text("old"),
        )];
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CODEX);
        match &events[0] {
            Event::ToolCallStarted { tool_call } => {
                assert_eq!(tool_call.diffs.len(), 1);
                assert_eq!(tool_call.diffs[0].path, "src/foo.rs");
                assert_eq!(tool_call.diffs[0].new_text.as_deref(), Some("new"));
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_carries_diff_content() {
        // Codex also re-sends the diff on the in-progress and completion
        // updates; those must reach the reducer via ToolCallUpdated.diffs so
        // a late-arriving diff still lands on the card. See #1721.
        use agent_client_protocol::schema::{
            Diff, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Diff(
                Diff::new("src/foo.rs", "new").old_text("old"),
            )]);
        let update = ToolCallUpdate::new("tc-edit-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CODEX,
        );
        let updated = events
            .iter()
            .find_map(|e| match e {
                Event::ToolCallUpdated { diffs, .. } => Some(diffs),
                _ => None,
            })
            .expect("a ToolCallUpdated event must be emitted for a diff-only update");
        let diffs = updated.as_ref().expect("diffs must be Some");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "src/foo.rs");
    }

    #[test]
    fn map_tool_call_update_text_only_leaves_diffs_none() {
        // A text-only update must not carry Some([]) (which would wipe an
        // earlier frame's diffs in the reducer). See #1721.
        use agent_client_protocol::schema::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new("done"))]);
        let update = ToolCallUpdate::new("tc-edit-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CODEX,
        );
        for e in &events {
            if let Event::ToolCallUpdated { diffs, .. } = e {
                assert!(diffs.is_none(), "text-only update must leave diffs None");
            }
        }
    }

    #[test]
    fn map_tool_call_update_emits_wakeup_when_title_and_raw_input_land_in_update() {
        // claude-agent-acp sends the initial `ToolCall` for ScheduleWakeup
        // with `raw_input = {}`; the real `delaySeconds` arrives on a
        // follow-up `ToolCallUpdate` that carries both `title` and
        // `raw_input`. The initial-path emit therefore returns `None`
        // from `wakeup_event_from_raw`, and the update-path must pick up
        // the slack so `Event::WakeupScheduled` lands in the store
        // (sidebar `⏰ in Nm` chip + structured view "Asleep until…" banner
        // depend on it). Regression for #1091.
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({
                "delaySeconds": 600,
                "prompt": "Wake-up fired. Confirm.",
                "reason": "Test 10-minute wake-up card countdown",
            }));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        let wakeup = events
            .iter()
            .find(|e| matches!(e, Event::WakeupScheduled { .. }))
            .expect(
                "ToolCallUpdate with title=ScheduleWakeup + delaySeconds must emit WakeupScheduled",
            );
        match wakeup {
            Event::WakeupScheduled { at, reason } => {
                let delta = (*at - chrono::Utc::now()).num_seconds();
                assert!(
                    (590..=610).contains(&delta),
                    "wakeup `at` should be ~600s in the future, got {delta}s",
                );
                assert_eq!(
                    reason.as_deref(),
                    Some("Test 10-minute wake-up card countdown"),
                );
            }
            other => panic!("expected WakeupScheduled, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_skips_wakeup_when_raw_input_missing() {
        // Title-only update (the initial frame's mirror, before
        // raw_input arrives) must NOT emit a WakeupScheduled, otherwise
        // we'd publish a "wakeup at epoch zero" placeholder.
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new().title("ScheduleWakeup".to_string());
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::WakeupScheduled { .. })),
            "no WakeupScheduled should fire without delaySeconds",
        );
    }

    #[test]
    fn map_tool_call_update_emits_monitor_armed_when_title_and_args_land() {
        // Mirrors the ScheduleWakeup path: the Monitor tool's initial
        // `ToolCall` frame has empty args; the real `command` /
        // `description` arrive on a follow-up `ToolCallUpdate`. That update
        // must emit MonitorArmed so the sidebar shows a "monitoring" badge
        // instead of a plain grey idle dot.
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("Monitor".to_string())
            .raw_input(serde_json::json!({
                "command": "until cargo clippy; do sleep 5; done",
                "description": "clippy passes",
                "timeout_ms": 600000,
                "persistent": false,
            }));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        let armed = events
            .iter()
            .find(|e| matches!(e, Event::MonitorArmed { .. }))
            .expect("ToolCallUpdate with title=Monitor + args must emit MonitorArmed");
        match armed {
            Event::MonitorArmed { description } => {
                assert_eq!(description.as_deref(), Some("clippy passes"));
            }
            other => panic!("expected MonitorArmed, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_skips_monitor_when_args_empty() {
        // The initial title-only / empty-args frame must NOT arm the badge;
        // only the populated follow-up update does.
        use agent_client_protocol::schema::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("Monitor".to_string())
            .raw_input(serde_json::json!({}));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::MonitorArmed { .. })),
            "no MonitorArmed should fire without command or description",
        );
    }

    #[test]
    fn map_session_info_update_ignores_pushed_title() {
        use agent_client_protocol::schema::SessionInfoUpdate;
        let info = SessionInfoUpdate::new().title("Fix the flaky test".to_string());
        let events = map_update_to_events(
            SessionUpdate::SessionInfoUpdate(info),
            &agent_profiles::CLAUDE,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn map_session_info_update_without_title_emits_nothing() {
        use agent_client_protocol::schema::SessionInfoUpdate;
        // Null/undefined title (e.g. a timestamp-only update) yields no event.
        let info = SessionInfoUpdate::new().updated_at("2026-06-25T00:00:00Z".to_string());
        let events = map_update_to_events(
            SessionUpdate::SessionInfoUpdate(info),
            &agent_profiles::CLAUDE,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn map_usage_update_emits_typed_usage_event() {
        use agent_client_protocol::schema::{Cost, UsageUpdate};
        let u = UsageUpdate::new(12_345, 200_000).cost(Cost::new(0.42, "USD"));
        let events = map_update_to_events(SessionUpdate::UsageUpdate(u), &agent_profiles::CLAUDE);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::UsageUpdated { usage } => {
                assert_eq!(usage.used, 12_345);
                assert_eq!(usage.size, 200_000);
                let cost = usage.cost.as_ref().expect("cost present");
                assert!((cost.amount - 0.42).abs() < f64::EPSILON);
                assert_eq!(cost.currency, "USD");
            }
            other => panic!("expected UsageUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_available_commands_update_emits_typed_event() {
        use agent_client_protocol::schema::{
            AvailableCommand as AcpAvailableCommand, AvailableCommandInput,
            AvailableCommandsUpdate, UnstructuredCommandInput,
        };
        let cmds = vec![
            AcpAvailableCommand::new("review", "Review changes").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("PR url")),
            ),
            AcpAvailableCommand::new("clear", "Reset context"),
        ];
        let update = AvailableCommandsUpdate::new(cmds);
        let events = map_update_to_events(
            SessionUpdate::AvailableCommandsUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::AvailableCommandsUpdated { commands } => {
                assert_eq!(commands.len(), 2);
                assert_eq!(commands[0].name, "review");
                assert!(commands[0].accepts_input);
                assert_eq!(commands[1].name, "clear");
                assert!(!commands[1].accepts_input);
            }
            other => panic!("expected AvailableCommandsUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_config_option_update_emits_typed_event_with_categories() {
        use agent_client_protocol::schema::{
            ConfigOptionUpdate, SessionConfigKind, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions,
        };
        let model_option = SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "claude-opus-4-7",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("claude-opus-4-7", "Claude Opus 4.7"),
                    SessionConfigSelectOption::new("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::Model);
        let effort_option = SessionConfigOption::new(
            "effort",
            "Reasoning Effort",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "default",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("high", "High"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let mode_option = SessionConfigOption::new(
            "mode",
            "Mode",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "default",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::Mode);
        let update = ConfigOptionUpdate::new(vec![model_option, effort_option, mode_option]);

        let events = map_update_to_events(
            SessionUpdate::ConfigOptionUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ConfigOptionsUpdated { options } => {
                assert_eq!(options.len(), 3);
                assert_eq!(options[0].id, "model");
                assert_eq!(options[0].category, ConfigOptionCategory::Model);
                assert_eq!(options[0].current_value, "claude-opus-4-7");
                assert_eq!(options[0].options.len(), 2);
                assert_eq!(options[1].category, ConfigOptionCategory::ThoughtLevel);
                assert_eq!(options[1].current_value, "default");
                assert_eq!(options[2].category, ConfigOptionCategory::Mode);
            }
            other => panic!("expected ConfigOptionsUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_config_option_preserves_unknown_category_name() {
        // Forward-compat path for #1563: a category name aoe doesn't
        // recognize arrives via the upstream untagged `Other(String)`
        // arm. It must pass through as `Other(<name>)` and the option
        // must not be dropped from the descriptor list. (The wildcard
        // `_` arm that warns fires only for a genuinely new *named*
        // upstream variant, which cannot be constructed against the
        // current `#[non_exhaustive]` schema, so it is verified by
        // inspection rather than a unit test.)
        use agent_client_protocol::schema::{
            ConfigOptionUpdate, SessionConfigKind, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions,
        };
        let unknown = SessionConfigOption::new(
            "future",
            "Future Selector",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "a",
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    "a", "A",
                )]),
            )),
        )
        .category(SessionConfigOptionCategory::Other(
            "future_category".to_string(),
        ));
        let update = ConfigOptionUpdate::new(vec![unknown]);

        let events = map_update_to_events(
            SessionUpdate::ConfigOptionUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ConfigOptionsUpdated { options } => {
                assert_eq!(
                    options.len(),
                    1,
                    "unknown-category option must not be dropped"
                );
                assert_eq!(options[0].id, "future");
                assert_eq!(
                    options[0].category,
                    ConfigOptionCategory::Other("future_category".to_string()),
                );
            }
            other => panic!("expected ConfigOptionsUpdated, got {other:?}"),
        }
    }

    #[test]
    fn config_options_event_propagates_empty_snapshot() {
        // A present-but-empty config_options snapshot from the adapter is
        // a real full replacement and must clear stale cached selectors,
        // so it returns `Some(ConfigOptionsUpdated { options: [] })`
        // (not `None`). See #1403.
        let event =
            config_options_event(Some(Vec::new())).expect("Some(vec![]) should produce an event");
        match event {
            Event::ConfigOptionsUpdated { options } => {
                assert!(options.is_empty());
            }
            other => panic!("expected empty ConfigOptionsUpdated, got {other:?}"),
        }
        // No config_options field at all (the adapter omitted it) returns
        // None so callers skip the emit and cached selectors persist.
        assert!(config_options_event(None).is_none());
    }

    #[test]
    fn preview_args_strips_control_chars() {
        // Build the preview string by hand-injecting raw control chars
        // *into* the result of to_string (simulating agents that send
        // pre-serialised non-utf8 noise through). The function should
        // strip BEL/BS/etc. but preserve `\n` and `\t`.
        let arg = serde_json::Value::String("hello\x07world".into());
        let preview = preview_args(&arg);
        // The literal BEL (0x07) inside the string-data part of the JSON
        // gets escaped by to_string, so the preview never sees a raw
        // control char in this path. That's fine: the assertion we care
        // about is that the preview doesn't carry any unprintable bytes.
        for c in preview.chars() {
            assert!(
                !c.is_control() || c == '\n' || c == '\t',
                "unexpected control char {:?} in preview",
                c
            );
        }
        assert!(preview.contains("hello"));
        assert!(preview.contains("world"));
    }

    #[test]
    fn provider_env_denyreason_blocks_infra_and_linker_keys() {
        assert!(provider_env_denyreason("AOE_TOKEN").is_some());
        assert!(provider_env_denyreason("PATH").is_some());
        assert!(provider_env_denyreason("HOME").is_some());
        assert!(provider_env_denyreason("LD_PRELOAD").is_some());
        assert!(provider_env_denyreason("LD_LIBRARY_PATH").is_some());
        assert!(provider_env_denyreason("DYLD_INSERT_LIBRARIES").is_some());
        assert!(provider_env_denyreason("").is_some());
    }

    #[test]
    fn provider_env_denyreason_allows_provider_auth_keys() {
        // The legitimate use case: per-session auth override.
        assert!(provider_env_denyreason("ANTHROPIC_API_KEY").is_none());
        assert!(provider_env_denyreason("CLAUDE_CODE_OAUTH_TOKEN").is_none());
        assert!(provider_env_denyreason("OPENAI_API_KEY").is_none());
        assert!(provider_env_denyreason("AOE_AGENT_MODEL").is_none());
        // Custom provider keys should pass through.
        assert!(provider_env_denyreason("MY_CUSTOM_VAR").is_none());
    }

    #[test]
    fn scrub_stderr_secrets_redacts_known_prefixes() {
        let cases = [
            ("auth failed: sk-ant-abcdefghijklmnop1234567890", true),
            ("Bearer abcdefghijklmnop1234567890.signature", true),
            ("GitHub PAT: ghp_abcdefghijklmnop1234567890", true),
            ("legacy fine grained: github_pat_abcdefghijklmnop1234", true),
            ("AWS: AKIAIOSFODNN7EXAMPLE", true),
        ];
        for (input, should_redact) in cases {
            let scrubbed = scrub_stderr_secrets(input);
            if should_redact {
                assert!(
                    scrubbed.contains("<redacted-secret>"),
                    "expected redaction in {input:?}, got {scrubbed:?}"
                );
            } else {
                assert_eq!(scrubbed, input);
            }
        }
    }

    #[test]
    fn scrub_stderr_secrets_leaves_innocuous_lines_alone() {
        // Common-case debug lines that must not get false-positive
        // redaction or the log loses diagnostic value.
        let lines = [
            "agent connected at /tmp/aoe.sock",
            "session/initialize ok, capabilities: load_session=true",
            "user prompt: please refactor src/main.rs to use anyhow",
            // Even though "sk-" appears, the literal isn't long enough
            // to match the secret regex.
            "the variable sk-test is fine",
        ];
        for line in lines {
            assert_eq!(scrub_stderr_secrets(line), line);
        }
    }
}
