//! The long-lived connection task: initialize once, create one session,
//! then pump commands into ACP requests until shutdown.

use crate::acp::agent_compat::{self, ExpectedAgent};
use crate::acp::state::{Event, ModeInfo, StartupErrorDetail};
use crate::acp::{agent_profiles, control_protocol, mcp_config};
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    CreateTerminalRequest, CreateTerminalResponse, ForkSessionRequest, ForkSessionResponse,
    InitializeResponse, KillTerminalRequest, KillTerminalResponse, LoadSessionRequest,
    LoadSessionResponse, McpServer, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionRequest, RequestPermissionResponse, SessionConfigId,
    SessionConfigValueId, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, StopReason, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, trace, warn};

use super::between_prompt::{
    between_prompt_should_fire, between_prompt_signal_update, between_prompt_stop_reason,
    between_prompt_work_state, BETWEEN_PROMPT_IDLE_CHECK_INTERVAL, BETWEEN_PROMPT_IDLE_GRACE,
};
use super::commands::{ClientCmd, ConnectMode};
use super::config_options::{
    config_options_event, dispatch_set_config_option, dispatch_set_mode, mode_config_id,
    thought_level_config_id, ConfigOptionDispatchPurpose,
};
use super::control::{establish_session_v2, prompt_outcome_to_response, DaemonControlClient};
use super::delete::handle_delete_session_cmd;
use super::errors::{acp_internal_error, AcpError, IncompatibleAgentError};
use super::fs_handlers::{handle_read_text_file, handle_write_text_file};
use super::handshake::{build_initialize_request, should_fork};
use super::lifecycle::{
    forward_lifecycle_signals, LifecycleEnvelope, LifecycleSignal, OffProtocolWorkKind,
    TerminalClaim,
};
use super::opencode::recover_opencode_prompt_error;
use super::pending::PendingResponders;
use super::permission_handlers::{handle_elicitation_request, handle_permission_request};
use super::rate_limit::{
    captured_rate_limit_resets_at, classify_rate_limit_error, classify_rate_limit_from_message,
    rate_limit_rejection_from_meta,
};
use super::reset::{
    await_reset_request, ResetRequestError, ResetSessionOutcome, SESSION_RESET_IN_TASK_TIMEOUT,
};
use super::session_sandbox::agent_request_cwd;
use super::steer::{first_text_block, SteerOutcome, SteerRequest};
use super::terminal_handlers::{
    handle_create_terminal, handle_kill_terminal, handle_release_terminal, handle_terminal_output,
    handle_wait_for_terminal_exit,
};
use super::tool_context::{update_tool_context_cache, ToolCallContextCache, ToolContextCache};
use super::transcript_filter::{is_transcript_event, transcript_event_kind};
use super::update_events::{map_update_to_events, AgentMessageDedup};
use super::watchdog::{
    classify_watchdog_notification_signals, silent_orphan_check_interval, silent_orphan_fast_grace,
    silent_orphan_grace, terminal_stop_reason, SilentOrphanWatchdog, SilentOrphanWatchdogConfig,
    OFF_PROTOCOL_WORK_GRACE_FLOOR,
};
use super::SessionResources;

/// Fully silent grace after reattaching to an in-flight turn. Any inbound
/// notification disarms this watchdog because later silence may be reasoning.
pub(super) const RESUME_IDLE_GRACE_DEFAULT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// After a cancel, declare the adapter unresponsive if no prompt response
/// arrives. This remains a transport-wedge defense even for adapters that
/// normally resolve cancellation promptly.
pub(crate) const CANCEL_ESCALATION_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Read the resume-idle grace. In debug builds, honors
/// `AOE_RESUME_IDLE_GRACE_MS` so integration tests can short-circuit
/// the default 10s without making real failures racy. Values below
/// 100ms are clamped up so a typo can't effectively disable the
/// watchdog. Release builds always use `RESUME_IDLE_GRACE_DEFAULT`
/// so a misconfigured env var can't surface false-positive Stopped
/// events to real users.
pub(super) fn resume_idle_grace() -> std::time::Duration {
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
pub(super) fn resolved_acp_config(
    profile: Option<&str>,
) -> Option<crate::session::config::AcpConfig> {
    match profile {
        Some(p) => Some(crate::session::config::profile_config::resolve_config_or_warn(p).acp),
        None => crate::session::load_config().ok().flatten().map(|c| c.acp),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_connection_task<W, R>(
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
    default_mode: Option<String>,
    mcp_servers: Vec<McpServer>,
    // Shared terminal-Stopped guard, supplied when a runner control
    // channel (#1054 Phase A) may deliver the adopted turn's completion
    // natively. The control reader CAS-claims it before emitting
    // `Stopped`, so the resume-idle and between-prompt watchdogs below
    // see it already fired and stand down. `None` on paths with no
    // control channel (direct stdio), where the task owns its own guard.
    external_terminal_guard: Option<Arc<TerminalClaim>>,
    external_prompt_in_flight: Option<Arc<std::sync::atomic::AtomicBool>>,
    // #2976 Phase B: control client for a v2 runner. When Some, the task
    // drives `initialize` / `session/*` / `session/prompt` / cancel over it
    // instead of the crate connection (relay), which stays attached only
    // for `session/update` notifications and server->client callbacks. None
    // on the direct-stdio path and against an older (v1) runner, where the
    // task speaks the full protocol over the relay as before.
    control_client: Option<Arc<DaemonControlClient>>,
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
    // Sandboxed agents run in-container: the session/new|load|fork request
    // must carry the container workdir, not the host path (#2871).
    let agent_cwd = agent_request_cwd(
        resources
            .sandbox
            .as_ref()
            .map(|s| s.container_workdir.as_path()),
        &cwd,
    );

    // Async sub-agent transcripts live inside the container for a sandboxed
    // session (their path is not on a mounted volume), so the tailer must
    // read them via `<runtime> exec`, not host `tokio::fs`. Capture the
    // container name once; a host session reads the file directly. See
    // `background_agent::TranscriptSource`.
    let bg_transcript_source = match resources.sandbox.as_ref() {
        Some(sandbox) => crate::acp::background_agent::TranscriptSource::Container {
            runtime: crate::containers::get_container_runtime().base.binary,
            container: sandbox.container_name.clone(),
        },
        None => crate::acp::background_agent::TranscriptSource::Host,
    };

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
    //   - `terminal_claim`: ensures exactly one path publishes a given
    //     turn's terminal Stopped (see `TerminalClaim`).
    let now_ms = chrono::Utc::now().timestamp_millis();
    let last_event_at = Arc::new(AtomicI64::new(now_ms));
    let first_event_after_attach = Arc::new(AtomicBool::new(false));
    let prompt_sent_since_attach = Arc::new(AtomicBool::new(false));
    // Shared with the runner control reader (#1054 Phase A) when present, so
    // a native `prompt_complete` from the runner and the resume-idle /
    // between-prompt watchdogs all claim the same per-turn terminal.
    let terminal_claim = external_terminal_guard.unwrap_or_else(|| Arc::new(TerminalClaim::new()));
    // True for a turn adopted mid-flight via `Resume { in_flight_turn: true }`:
    // a prior connection issued the `session/prompt`, so this connection has no
    // owning `prompt_fut` and no real `ClientCmd::Prompt` will emit the turn's
    // terminal Stopped. Set true once the handshake resolves the mode (below).
    // Cleared when a real prompt starts or when a terminal path claims the
    // turn's terminal. Drives the cost-marker completion the between-
    // prompt watchdog emits for the adopted turn. See #2899.
    let adopted_turn_active = Arc::new(AtomicBool::new(false));
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
    // Reset epochs (unix SECONDS) for the quota windows the adapter reported
    // as `rejected`, keyed by `rateLimitType` so one window cannot overwrite
    // another. Fed from `usage_update`'s `_meta._claude/rateLimit` (the only
    // place the adapter puts a reset; the prompt error carries just
    // `errorKind`) and read at the rate-limit classify sites.
    //
    // Deliberately NOT cleared at prompt start: the reset belongs to the
    // quota window, not to one prompt, and the adapter suppresses the
    // rejection's `usage_update` on any turn that produced no assistant
    // usage. Wiping it meant a rejection captured on an earlier turn could
    // not answer the next prompt's rejection, which is the case the reporter
    // hit twice. Stale entries are filtered by reset-in-the-future at read
    // time instead. #3028, #3152.
    let last_rate_limit_rejections = Arc::new(std::sync::Mutex::new(HashMap::<String, i64>::new()));
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
    // Async background agents (claude `Agent` tool with `isAsync`) currently
    // tracked by a live tailer, keyed by agent_id. Non-empty means
    // agent-initiated work is still running off-protocol WITH a precise
    // terminal event (the tailer removes the id on completion / stall /
    // error), so the between-prompt idle watchdog must not fire while any is
    // in flight. Distinct from the 30-min off-protocol floor, which governs
    // untracked backgrounded Bash that has no completion signal. See #2573.
    let between_prompt_bg_agents = Arc::new(std::sync::Mutex::new(std::collections::HashSet::<
        String,
    >::new()));
    let prompt_in_flight =
        external_prompt_in_flight.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let last_event_at_for_notif = last_event_at.clone();
    let first_event_after_attach_for_notif = first_event_after_attach.clone();
    let last_lifecycle_at_for_notif = last_lifecycle_at.clone();
    let between_prompt_active_for_notif = between_prompt_active.clone();
    let terminal_claim_for_notif = terminal_claim.clone();
    let between_prompt_cost_seen_for_notif = between_prompt_cost_seen.clone();
    let between_prompt_wake_at_for_notif = between_prompt_wake_at.clone();
    let last_rate_limit_rejections_for_notif = last_rate_limit_rejections.clone();
    let last_rate_limit_rejections_for_block = last_rate_limit_rejections.clone();
    let between_prompt_tools_for_notif = between_prompt_tools.clone();
    let between_prompt_off_protocol_for_notif = between_prompt_off_protocol.clone();
    let between_prompt_bg_agents_for_notif = between_prompt_bg_agents.clone();
    let bg_transcript_source_for_notif = bg_transcript_source.clone();
    let adopted_turn_active_for_notif = adopted_turn_active.clone();
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
    let control_on_close = control_client.clone();

    let result = Client
        .builder()
        .name("aoe-acp")
        .on_close(move |_connection| async move {
            if let Some(control) = control_on_close {
                control.shutdown();
            }
            Err(acp_internal_error("agent transport closed".into()))
        })
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
                let bg_transcript_source = bg_transcript_source_for_notif.clone();
                let last_lifecycle_at = last_lifecycle_at_for_notif.clone();
                let between_prompt_active = between_prompt_active_for_notif.clone();
                let terminal_claim = terminal_claim_for_notif.clone();
                let between_prompt_cost_seen =
                    between_prompt_cost_seen_for_notif.clone();
                let between_prompt_wake_at =
                    between_prompt_wake_at_for_notif.clone();
                let last_rate_limit_rejections =
                    last_rate_limit_rejections_for_notif.clone();
                let between_prompt_tools = between_prompt_tools_for_notif.clone();
                let between_prompt_off_protocol =
                    between_prompt_off_protocol_for_notif.clone();
                let between_prompt_bg_agents =
                    between_prompt_bg_agents_for_notif.clone();
                let adopted_turn_active = adopted_turn_active_for_notif.clone();
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
                    let prompt_active = prompt_in_flight.load(Ordering::Relaxed);
                    // Between-prompt idle tracking (#2325). Only while no
                    // aoe-issued prompt is in flight: a lifecycle signal here
                    // means the agent resumed itself (Monitor / scheduled
                    // wake), a turn the per-prompt watchdog never sees. Mirror
                    // its cost/progress/wake semantics so the outer loop's
                    // idle tick applies the same grace. During a real prompt
                    // the per-prompt watchdog owns this, so skip.
                    if !prompt_active {
                        let now = chrono::Utc::now().timestamp_millis();
                        if let Some(u) = between_prompt_signal_update(
                            lifecycle_signal.as_ref(),
                            wakeup_signal.as_ref(),
                            now,
                            between_prompt_wake_at.load(Ordering::Relaxed),
                        ) {
                            // False -> true is an agent-initiated turn
                            // starting, so it gets its own terminal to claim.
                            // Logged because the arming decision was
                            // previously invisible: the watchdog only ever
                            // logged when it fired, which is exactly the
                            // information a stuck-Running investigation
                            // needs. See #3190.
                            if !between_prompt_active.swap(true, Ordering::Relaxed) {
                                terminal_claim.begin_turn();
                                debug!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "between-prompt watchdog armed for an agent-initiated turn"
                                );
                            }
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
                                // Async sub-agents are tracked precisely in
                                // between_prompt_bg_agents (a tailer removes
                                // them on their terminal event), so they must
                                // NOT also latch the 30-min off-protocol floor;
                                // that floor is only for untracked backgrounded
                                // work (Bash) with no completion signal. See
                                // #2573.
                                let is_tracked_async = matches!(
                                    off_protocol_work,
                                    Some(OffProtocolWorkKind::AsyncAgent)
                                );
                                if *succeeded
                                    && !is_tracked_async
                                    && (off_protocol_work.is_some() || was_background)
                                {
                                    between_prompt_off_protocol
                                        .store(true, Ordering::Relaxed);
                                }
                            }
                            Some(LifecycleSignal::TerminalUsage)
                                if adopted_turn_active.load(Ordering::Relaxed) =>
                            {
                                // Adopted-turn barrier (#2899). A tool that was
                                // in flight across the reattach boundary had its
                                // ToolCall start on the previous connection; this
                                // connection may see a trailing InProgress update
                                // (re-inserting it into between_prompt_tools) but
                                // never the terminal Completed/Failed frame, which
                                // went to the old connection or only rode the
                                // dropped PromptResponse. That leaks a stuck entry
                                // that pins `work_in_flight` true forever, so the
                                // between-prompt watchdog can never fire. A cost-
                                // populated end-of-turn UsageUpdate is the adapter's
                                // authoritative "turn wrapped up" marker (same signal
                                // the per-prompt watchdog trusts), so drop the
                                // unreliable inherited tool + untracked-background
                                // bookkeeping. A future scheduled wake
                                // (between_prompt_wake_at) and precisely tracked
                                // async agents (between_prompt_bg_agents) keep their
                                // own suppression: they carry real continuation
                                // semantics, unlike a stale ACP tool entry.
                                between_prompt_tools
                                    .lock()
                                    .expect("between-prompt tools mutex poisoned")
                                    .clear();
                                between_prompt_off_protocol.store(false, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                    // Capture the reset epoch the adapter forwards on a
                    // `usage_update` (#3028) before the update is consumed
                    // below. Only rejections are retained; a warning epoch
                    // cannot be attributed to whichever window later rejects
                    // (#3152). Log every observation either way: this is the
                    // only breadcrumb for diagnosing a wrong reset time from
                    // `debug.log`.
                    if let SessionUpdate::UsageUpdate(u) = &notification.update {
                        if let Some(raw) = u
                            .meta
                            .as_ref()
                            .and_then(|m| m.get("_claude/rateLimit"))
                        {
                            let rejection = rate_limit_rejection_from_meta(&u.meta);
                            debug!(
                                target: "acp.protocol",
                                session = %session_label,
                                observed = %raw,
                                retained = rejection.is_some(),
                                "observed adapter rate-limit meta"
                            );
                            if let Some(r) = rejection {
                                last_rate_limit_rejections
                                    .lock()
                                    .expect("rate-limit capture mutex poisoned")
                                    .insert(r.window, r.resets_at_secs);
                            }
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
                    // impl review. Skipped entirely between prompts:
                    // nothing drains the channel then, so at capacity
                    // the awaited send would wedge this handler, and
                    // every notification behind it, until the next
                    // prompt (#2888).
                    forward_lifecycle_signals(
                        prompt_active,
                        &lifecycle_signal_tx,
                        envelope_epoch,
                        lifecycle_signal,
                        wakeup_signal,
                        &session_label,
                    )
                    .await;
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
                                    bg_transcript_source.clone(),
                                    event_tx.clone(),
                                    between_prompt_bg_agents.clone(),
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
            // #2976 Phase B: against a v2 runner the runner owns
            // `initialize` (it runs it once and caches the result), so the
            // daemon drives it over the control channel and deserializes the
            // cached result. Against the direct-stdio path or an older (v1)
            // runner, send it over the relay via the crate connection as
            // before. `initialize` is idempotent on every ACP agent we ship
            // against (aoe-agent, claude-agent-acp); the response only
            // carries capability metadata.
            let init: InitializeResponse = if let Some(control) = control_client.as_ref() {
                let params = serde_json::to_value(build_initialize_request())
                    .map_err(|e| acp_internal_error(format!("serialize initialize params: {e}")))?;
                let result = control.initialize(params).await?;
                serde_json::from_value(result)
                    .map_err(|e| acp_internal_error(format!("deserialize initialize result: {e}")))?
            } else {
                connection
                    .send_request(build_initialize_request())
                    .block_task()
                    .await?
            };

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
            // Derived here rather than cached across connections: a
            // respawn can land on a different adapter build, and this
            // runs on every connect. Emitted even when false so replay
            // cannot leave a client holding a stale `true` after a
            // downgrade. See #2805.
            let steering_capable = agent_compat::supports_steering(expected_agent, &init);
            let prompt_caps = &init.agent_capabilities.prompt_capabilities;
            let _ = event_tx_for_block
                .send(Event::PromptCapabilities {
                    image: prompt_caps.image,
                    audio: prompt_caps.audio,
                    embedded_context: prompt_caps.embedded_context,
                    steering: steering_capable,
                })
                .await;
            if steering_capable {
                info!(
                    target: "acp.protocol",
                    session = %session_label,
                    "agent supports _session/steering; mid-turn prompts will be injected into the running turn"
                );
            }
            // Snapshot the watchdog-arming flag before `mode` is moved
            // into the match below.
            let arm_resume_watchdog = matches!(
                &mode,
                ConnectMode::Resume {
                    in_flight_turn: true,
                    ..
                }
            );
            // Mark the adopted turn so the notification handler applies the
            // cost-marker barrier and the between-prompt watchdog emits
            // `prompt_complete` for it. Set before any turn events arrive. See #2899.
            adopted_turn_active.store(arm_resume_watchdog, Ordering::Relaxed);
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

            // Track the mode channels the agent advertised so each switch uses
            // the matching protocol method. Config-option mode is authoritative
            // when present; otherwise SessionModeState constrains valid ids.
            let mut available_mode_ids: Option<Vec<String>> = None;
            let mut mode_config_option_id: Option<String> = None;
            // Thought-level (reasoning effort) option id, captured from whichever
            // establish call ran so `default_effort` can be applied once after
            // the match. Captured in all three Fresh branches, not just
            // session/new: a respawn resumes via session/load, and applying the
            // effort only on a fresh session is what made a picked effort revert
            // on every respawn.
            let mut thought_level_config_option_id: Option<String> = None;

            // Drop any http/sse servers the agent did not advertise before they
            // reach session/new or session/load; stdio is always kept. Computed
            // once here so both the load-attempt and the fresh-session fallback
            // forward the same gated list.
            let mcp_servers = mcp_config::filter_for_capabilities(
                mcp_servers,
                &init.agent_capabilities.mcp_capabilities,
                &session_label,
            );
            // Kept for the driven conversation reset (#2979): the handshake
            // below consumes `mcp_servers`, but a later `session/new` issued
            // for a clear command must forward the same gated list.
            let mcp_servers_for_reset = mcp_servers.clone();

            // Mutable: a driven conversation reset (#2979) swaps in the
            // fresh id from its `session/new` so every later
            // `session/prompt` / cancel / mode switch addresses the new
            // conversation.
            let mut acp_session_id: SessionId = match mode {
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
                    fork_from,
                } => {
                    // Decide whether to resume the prior agent session or create
                    // a fresh one. session/load is only attempted when the agent
                    // advertises support AND we have a stored id to feed it. On
                    // load failure (id GC'd, agent state lost, etc.) we fall
                    // through to session/new and emit SessionContextReset so the
                    // UI can show a notice and clear stale token-usage hints.
                    let mut acp_session_id: Option<SessionId> = None;

                    // Structured fork (when fork_pending is set and the agent
                    // advertises the capability): send session/fork against the
                    // parent id; the adapter mints a new child id we capture and
                    // persist via AcpSessionAssigned. Tried before the load/new
                    // decision so a fork never falls through to session/new
                    // (which would hand the user an empty session they believe
                    // is a fork). On fork failure we emit SessionContextReset
                    // (which clears the one-shot fork marker so the reconciler
                    // and supervisor stop re-forking) and then return Err to
                    // fail the spawn rather than silently masking it.
                    let fork_capable = init.agent_capabilities.session_capabilities.fork.is_some();
                    if should_fork(fork_from.as_deref(), fork_capable) {
                        let parent = fork_from.clone().unwrap();
                        info!(
                            target: "acp.protocol",
                            session = %session_label,
                            parent_acp_id = %parent,
                            "structured fork via session/fork"
                        );
                        let req = ForkSessionRequest::new(parent.clone(), agent_cwd.clone())
                            .mcp_servers(mcp_servers.clone());
                        // #2976 Phase B: a v2 runner owns session creation;
                        // drive session/fork over the control channel and
                        // deserialize the cached result. Else send over the
                        // relay via the crate connection.
                        let fork_result = if let Some(control) = control_client.as_ref() {
                            establish_session_v2::<ForkSessionResponse>(
                                control,
                                "session/fork",
                                &req,
                            )
                            .await
                        } else {
                            connection.send_request(req).block_task().await
                        };
                        match fork_result {
                            Ok(resp) => {
                                let new_id = resp.session_id.clone();
                                info!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    parent_acp_id = %parent,
                                    new_id = %new_id.0,
                                    "session/fork succeeded, captured forked acp_session_id"
                                );
                                // Capture available mode info and config-option
                                // mode category from the fork response (it carries
                                // the same modes/config_options as session/new), so
                                // SetMode chooses the authoritative channel and the
                                // pickers hydrate.
                                if let Some(modes) = resp.modes.as_ref() {
                                    available_mode_ids = Some(
                                        modes
                                            .available_modes
                                            .iter()
                                            .map(|m| m.id.0.to_string())
                                            .collect(),
                                    );
                                }
                                mode_config_option_id = resp
                                    .config_options
                                    .as_deref()
                                    .and_then(mode_config_id)
                                    .map(|id| id.0.to_string());
                                thought_level_config_option_id = resp
                                    .config_options
                                    .as_deref()
                                    .and_then(thought_level_config_id)
                                    .map(|id| id.0.to_string());
                                // Surface agent-advertised modes (when carried in
                                // the ACP `modes` field rather than the `mode`
                                // config option), mirroring session/new so a fork
                                // hydrates the mode picker too. See #1403.
                                if let Some(modes) = resp.modes.as_ref() {
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
                                let _ = event_tx_for_block
                                    .send(Event::AcpSessionAssigned {
                                        acp_session_id: new_id.0.to_string(),
                                    })
                                    .await;
                                if let Some(event) = config_options_event(resp.config_options) {
                                    let _ = event_tx_for_block.send(event).await;
                                }
                                acp_session_id = Some(new_id);
                            }
                            Err(e) => {
                                warn!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    parent_acp_id = %parent,
                                    "session/fork failed; failing spawn (no session/new fallback): {e}"
                                );
                                // Clear the one-shot fork marker via a reset
                                // event before failing: without it the reconciler
                                // re-reads fork_pending and re-issues the same
                                // failing session/fork on every reattach, wedging
                                // the instance in a retry loop. The reset also
                                // gives the dashboard a user-visible reason.
                                let _ = event_tx_for_block
                                    .send(Event::SessionContextReset {
                                        reason: format!("fork_failed: {e}"),
                                    })
                                    .await;
                                return Err(e);
                            }
                        }
                    } else if fork_from.as_deref().is_some_and(|s| !s.is_empty()) {
                        // A fork was requested but the connected agent does not
                        // advertise the fork capability (e.g. a resume-only
                        // adapter, or a claude-agent-acp build without fork).
                        // The create-time surfaces gate on this, but a runtime
                        // agent swap can still land here. Rather than silently
                        // presenting an empty session/new that the user believes
                        // is a fork, emit a reset so the marker clears (no retry
                        // loop) and the dashboard can explain the downgrade.
                        warn!(
                            target: "acp.protocol",
                            session = %session_label,
                            "fork requested but agent does not advertise fork; falling back to session/new"
                        );
                        let _ = event_tx_for_block
                            .send(Event::SessionContextReset {
                                reason: "fork_unsupported_by_agent".to_string(),
                            })
                            .await;
                    }

                    if acp_session_id.is_none() && load_session_capable {
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
                            let req = LoadSessionRequest::new(stored.clone(), agent_cwd.clone())
                                .mcp_servers(mcp_servers.clone());
                            // #2976 Phase B: v2 runner owns session/load.
                            let load_result = if let Some(control) = control_client.as_ref() {
                                establish_session_v2::<LoadSessionResponse>(
                                    control,
                                    "session/load",
                                    &req,
                                )
                                .await
                            } else {
                                connection.send_request(req).block_task().await
                            };
                            match load_result {
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
                                    mode_config_option_id = resp
                                        .config_options
                                        .as_deref()
                                        .and_then(mode_config_id)
                                        .map(|id| id.0.to_string());
                                    thought_level_config_option_id = resp
                                        .config_options
                                        .as_deref()
                                        .and_then(thought_level_config_id)
                                        .map(|id| id.0.to_string());
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
                        let req =
                            NewSessionRequest::new(agent_cwd.clone()).mcp_servers(mcp_servers);
                        // #2976 Phase B: v2 runner owns session/new.
                        let new_session = if let Some(control) = control_client.as_ref() {
                            establish_session_v2::<NewSessionResponse>(control, "session/new", &req)
                                .await?
                        } else {
                            connection.send_request(req).block_task().await?
                        };
                        let id = new_session.session_id.clone();
                        info!(
                            target: "acp.protocol",
                            session = %session_label,
                            new_id = %id.0,
                            "session/new succeeded, captured acp_session_id"
                        );

                        // Capture available mode IDs and config-option mode
                        // category so SetMode can choose the authoritative
                        // channel and gate legacy values.
                        if let Some(modes) = &new_session.modes {
                            available_mode_ids = Some(
                                modes
                                    .available_modes
                                    .iter()
                                    .map(|m| m.id.0.to_string())
                                    .collect(),
                            );
                        }
                        mode_config_option_id = new_session
                            .config_options
                            .as_deref()
                            .and_then(mode_config_id)
                            .map(|id| id.0.to_string());
                        thought_level_config_option_id = new_session
                            .config_options
                            .as_deref()
                            .and_then(thought_level_config_id)
                            .map(|id| id.0.to_string());

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

                        // Mode default. Strict:
                        // apply only when the agent advertises a live
                        // `category:"mode"` option; a stale/unknown value is
                        // rejected by the adapter and warned (no-op), never
                        // failing the spawn. Legacy set_mode / Claude hardcoded
                        // mode channels are intentionally not driven from
                        // defaults here (see #2631).
                        if let (Some(mode_value), Some(options)) =
                            (default_mode.as_deref(), config_options.as_deref())
                        {
                            if let Some(config_id) = mode_config_id(options) {
                                info!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    mode = mode_value,
                                    "applying default structured view mode"
                                );
                                match connection
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        id.clone(),
                                        config_id,
                                        SessionConfigValueId::new(mode_value.to_string()),
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
                                            "default structured view mode failed: {e}"
                                        );
                                    }
                                }
                            } else {
                                debug!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "default structured view mode skipped; no mode option"
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

            // Apply the session's reasoning effort ("thought level") once, after
            // whichever establish call ran. This sits outside the branches on
            // purpose: `Instance.acp_effort` is a pin the session carries across
            // respawns, and a respawn resumes via session/load (or session/fork),
            // so applying it only on session/new let a picked effort revert to the
            // agent default on every restart. Resume captures no option id (it
            // sends neither new nor load and the worker's session is still
            // configured), so it skips. Strict like the mode default above: a
            // stale value the agent no longer advertises is rejected and warned,
            // never failing the spawn.
            if let (Some(effort), Some(config_id)) = (
                default_effort.as_deref(),
                thought_level_config_option_id.as_deref(),
            ) {
                info!(
                    target: "acp.protocol",
                    session = %session_label,
                    effort,
                    "applying structured view effort"
                );
                match connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        acp_session_id.clone(),
                        SessionConfigId::new(config_id.to_string()),
                        SessionConfigValueId::new(effort.to_string()),
                    ))
                    .block_task()
                    .await
                {
                    Ok(resp) => {
                        if let Some(event) = config_options_event(Some(resp.config_options)) {
                            let _ = event_tx_for_block.send(event).await;
                        }
                    }
                    Err(e) => {
                        warn!(
                            target: "acp.protocol",
                            session = %session_label,
                            "structured view effort failed: {e}"
                        );
                    }
                }
            } else if default_effort.is_some() {
                debug!(
                    target: "acp.protocol",
                    session = %session_label,
                    "structured view effort skipped; no thought_level option"
                );
            }

            // #2976 Phase B: send session/cancel over the v2 control channel
            // when the runner owns the turn, else as a crate notification
            // over the relay. A macro (not a helper fn) so it expands at each
            // call site with that site's error-handling shape, and yields the
            // same `Result<(), Error>` the relay send did. Defined after
            // `acp_session_id` binds because macro_rules! resolves free
            // identifiers at the definition site, not the call site.
            macro_rules! send_session_cancel {
                () => {{
                    if let Some(control) = control_client.as_ref() {
                        control.cancel().await;
                        Ok::<(), agent_client_protocol::Error>(())
                    } else {
                        connection.send_notification(CancelNotification::new(acp_session_id.clone()))
                    }
                }};
            }

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
                let terminal_claim = terminal_claim.clone();
                let session_label_for_watchdog = session_label.clone();
                let grace = resume_idle_grace();
                tokio::spawn(async move {
                    let grace_ms = grace.as_millis() as i64;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if terminal_claim.claimed() {
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
                            // permanently: completion of the observable adopted
                            // turn is now owned by the between-prompt watchdog,
                            // which emits `prompt_complete` once the cost-
                            // populated end-of-turn UsageUpdate lands (the
                            // barrier at the TerminalUsage arm above clears the
                            // stale tool bookkeeping that used to pin it). This
                            // task stays the recovery path only for the fully
                            // silent case below. See #1216, #2899.
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
                            // Claim the shared terminal guard so the between-
                            // prompt watchdog can't also emit for this adopted
                            // turn in the narrow window where the first event
                            // and this grace expiry interleave. See #2899.
                            if !terminal_claim.claim() {
                                return;
                            }
                            info!(
                                target: "acp.protocol",
                                session = %session_label_for_watchdog,
                                idle_ms = now - last,
                                "resume-idle watchdog: synthesizing Stopped for orphaned in-flight turn"
                            );
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
            // Mid-turn prompts a steer handed back unconsumed (#2805).
            // The adapter answers `promptRequired` when the turn it was
            // meant to steer had already settled, so the message runs as
            // an ordinary next turn instead. Kept here rather than
            // self-sent through `cmd_tx`: this task holds only the
            // receiver, and sending into the bounded channel it is the
            // sole consumer of deadlocks once that channel fills.
            let mut pending_prompts: VecDeque<Vec<ContentBlock>> = VecDeque::new();
            loop {
                // Drain the fallback queue ahead of the channel so a
                // message the user sent first cannot be overtaken by one
                // they sent after it.
                let cmd = if let Some(blocks) = pending_prompts.pop_front() {
                    Some(ClientCmd::Prompt(blocks))
                } else {
                    tokio::select! {
                    cmd = cmd_rx.recv() => cmd,
                    _ = between_prompt_idle_tick.tick() => {
                        let now = chrono::Utc::now().timestamp_millis();
                        let wake_at = match between_prompt_wake_at.load(Ordering::Relaxed) {
                            0 => None,
                            at => Some(at),
                        };
                        // A tracked async background agent still running is
                        // work in flight just like an open tool: suppress the
                        // idle watchdog until its tailer reports terminal and
                        // removes it from the set. See #2573.
                        let work_in_flight = between_prompt_work_state(
                            &between_prompt_tools,
                            &between_prompt_bg_agents,
                        );
                        let cost_seen = between_prompt_cost_seen.load(Ordering::Relaxed);
                        if between_prompt_should_fire(
                            between_prompt_active.load(Ordering::Relaxed),
                            now,
                            last_lifecycle_at.load(Ordering::Relaxed),
                            wake_at,
                            cost_seen,
                            work_in_flight.is_busy(),
                            between_prompt_off_protocol.load(Ordering::Relaxed),
                            BETWEEN_PROMPT_IDLE_GRACE,
                            OFF_PROTOCOL_WORK_GRACE_FLOOR,
                        ) {
                            // An adopted turn (#2899) has no owning prompt_fut, so
                            // this watchdog owns its terminal Stopped. Claim the
                            // turn's shared terminal so the detached
                            // resume-idle task can't also fire in the narrow window
                            // where the first observable event and its grace expiry
                            // interleave; if that task already claimed, still reset
                            // state below but skip the emit. A non-adopted
                            // agent-initiated turn is serialized on this loop and
                            // needs no guard.
                            let adopted = adopted_turn_active.load(Ordering::Relaxed);
                            let claimed = !adopted || terminal_claim.claim();
                            if adopted {
                                adopted_turn_active.store(false, Ordering::Relaxed);
                            }
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
                            between_prompt_bg_agents
                                .lock()
                                .expect("between-prompt bg-agents mutex poisoned")
                                .clear();
                            if claimed {
                                let reason = between_prompt_stop_reason(adopted, cost_seen);
                                info!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    reason,
                                    "between-prompt idle watchdog: synthesizing Stopped for completed turn"
                                );
                                let _ = event_tx_for_block
                                    .send(Event::Stopped {
                                        reason: reason.into(),
                                    })
                                    .await;
                            }
                        }
                        continue;
                    }
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
                        // A real prompt supersedes an adopted in-flight turn: this
                        // prompt's own PromptResponse owns the next Stopped, so the
                        // adopted-turn completion path must stand down. See #2899.
                        adopted_turn_active.store(false, Ordering::Relaxed);
                        // A real prompt supersedes any agent-initiated turn the
                        // between-prompt idle watchdog was tracking; this
                        // prompt's own Stopped will own the next transition.
                        // The per-prompt watchdog owns idle detection until the
                        // Stopped emit below clears `prompt_in_flight`. See #2325.
                        prompt_in_flight.store(true, Ordering::Relaxed);
                        // This prompt is a new turn: its terminal is nobody's
                        // yet, whatever an earlier turn on this connection
                        // claimed.
                        terminal_claim.begin_turn();
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
                        let silent_orphan_grace_fast = silent_orphan_fast_grace();
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
                        // #2976 Phase B: against a v2 runner the turn is
                        // issued over the control channel (the runner assigns
                        // the session/prompt id and reports PromptCompleted,
                        // which the control reader routes into this future);
                        // otherwise it goes over the relay via the crate
                        // connection. Both arms resolve to the same
                        // `Result<PromptResponse, Error>` so the select body
                        // below is identical. Boxed as a `Pin<Box<dyn
                        // Future>>` (Unpin) so no `tokio::pin!` is needed.
                        let mut prompt_fut: std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = Result<PromptResponse, agent_client_protocol::Error>,
                                    > + Send,
                            >,
                        > = if let Some(control) = control_client.as_ref() {
                            match serde_json::to_value(PromptRequest::new(
                                acp_session_id.clone(),
                                blocks,
                            )) {
                                Ok(params) => {
                                    let rx = control.prompt(params).await;
                                    Box::pin(async move {
                                        match rx.await {
                                            Ok(outcome) => prompt_outcome_to_response(outcome),
                                            // Control channel closed before
                                            // completion: end the turn
                                            // cleanly; the dying connection
                                            // surfaces the underlying failure.
                                            Err(_) => prompt_outcome_to_response(
                                                control_protocol::PromptOutcome::Aborted,
                                            ),
                                        }
                                    })
                                }
                                Err(e) => Box::pin(async move {
                                    Err(acp_internal_error(format!("serialize prompt params: {e}")))
                                }),
                            }
                        } else {
                            Box::pin(
                                connection
                                    .send_request(PromptRequest::new(acp_session_id.clone(), blocks))
                                    .block_task(),
                            )
                        };

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

                        // Mid-turn steering (#2805). At most one
                        // `_session/steering` request is outstanding at a
                        // time, with any further mid-turn prompts waiting
                        // in `steer_backlog`. Running them concurrently
                        // would let two steers that both race the turn's
                        // end come back out of order, and the
                        // `promptRequired` fallback would then replay the
                        // user's messages in the wrong order. The future
                        // is polled as a select arm rather than awaited
                        // inline so Cancel and ForceStop stay responsive
                        // for the whole round trip.
                        #[allow(clippy::type_complexity)]
                        let mut steer_fut: Option<
                            std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = (
                                                Vec<ContentBlock>,
                                                Result<
                                                    serde_json::Value,
                                                    agent_client_protocol::Error,
                                                >,
                                            ),
                                        > + Send,
                                >,
                            >,
                        > = None;
                        let mut steer_backlog: VecDeque<Vec<ContentBlock>> = VecDeque::new();
                        // Issue a steer for `blocks`, or park it behind the
                        // one already in flight.
                        macro_rules! steer_or_backlog {
                            ($blocks:expr) => {{
                                let blocks: Vec<ContentBlock> = $blocks;
                                if steer_fut.is_some() {
                                    steer_backlog.push_back(blocks);
                                } else {
                                    info!(
                                        target: "acp.protocol",
                                        session = %session_label,
                                        "sending _session/steering during in-flight prompt ({} content blocks)",
                                        blocks.len()
                                    );
                                    let sent = connection.send_request(SteerRequest::new(
                                        acp_session_id.clone(),
                                        blocks.clone(),
                                    ));
                                    steer_fut = Some(Box::pin(async move {
                                        (blocks, sent.block_task().await)
                                    }));
                                }
                            }};
                        }

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
                                            let captured_resets_at =
                                                captured_rate_limit_resets_at(
                                                    &last_rate_limit_rejections_for_block,
                                                    chrono::Utc::now(),
                                                );
                                            if let Some(info) =
                                                classify_rate_limit_error(&e, captured_resets_at)
                                            {
                                                info!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    resets_at = ?info.resets_at,
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
                                        if let Err(err) = send_session_cancel!() {
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
                                (blocks, res) = async {
                                    steer_fut.as_mut().expect("guarded by the arm condition").await
                                }, if steer_fut.is_some() => {
                                    steer_fut = None;
                                    match res {
                                        Ok(value) => match SteerOutcome::from_response(&value) {
                                            SteerOutcome::Injected => {
                                                info!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    "_session/steering injected into the running turn"
                                                );
                                                // No event: the prompt handler
                                                // already published this text as
                                                // `UserPromptSent` before it
                                                // reached the daemon, and the
                                                // running turn's own
                                                // `PromptResponse` still owns the
                                                // terminal Stopped.
                                                //
                                                // An accepted steer proves the
                                                // agent is alive and took new
                                                // work, so it counts as progress.
                                                // Injection pre-empts the current
                                                // generation, which can swallow an
                                                // update the silent-orphan
                                                // watchdog was waiting on; without
                                                // this the watchdog could kill a
                                                // healthy agent right after a
                                                // successful course correction.
                                                watchdog.apply_signal(
                                                    LifecycleSignal::Progress,
                                                    tokio::time::Instant::now(),
                                                    chrono::Utc::now(),
                                                    watchdog_cfg,
                                                );
                                            }
                                            SteerOutcome::PromptRequired => {
                                                // The turn settled in the race
                                                // window. The adapter kept its
                                                // hands off the content, so run it
                                                // as an ordinary next turn. The
                                                // in-flight turn is over in all but
                                                // bookkeeping, so the outer loop
                                                // picks this up as soon as
                                                // `prompt_fut` resolves.
                                                info!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    "_session/steering raced the turn's end; re-dispatching as a normal prompt"
                                                );
                                                pending_prompts.push_back(blocks);
                                                // Anything still parked raced the
                                                // same boundary, so it follows the
                                                // same path, in order.
                                                pending_prompts.extend(steer_backlog.drain(..));
                                            }
                                            outcome @ (SteerOutcome::StartedNewTurn
                                            | SteerOutcome::Unknown) => {
                                                // The adapter cleared the version
                                                // gate yet ignored the
                                                // `promptRequired` opt-in, so it
                                                // consumed the content into a turn
                                                // no request owns. Resending would
                                                // duplicate the user's message, and
                                                // `PromptRejected` would offer a
                                                // Retry that does the same. Leave
                                                // the already-published
                                                // `UserPromptSent` standing and let
                                                // the between-prompt idle watchdog
                                                // synthesize the detached turn's
                                                // terminal Stopped once this turn's
                                                // own Stopped clears
                                                // `prompt_in_flight`.
                                                warn!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    ?outcome,
                                                    "_session/steering returned an outcome that consumed the message without an owning request; the between-prompt watchdog will close the detached turn"
                                                );
                                            }
                                        },
                                        Err(e) => {
                                            // Transport or agent error. Nothing
                                            // proves the message landed, but
                                            // nothing proves it did not either, so
                                            // surface it as the same retryable
                                            // rejection a non-steering agent gives
                                            // and let the user decide.
                                            warn!(
                                                target: "acp.protocol",
                                                session = %session_label,
                                                error = %e,
                                                "_session/steering failed; falling back to agent_busy rejection"
                                            );
                                            let _ = event_tx_for_block
                                                .send(Event::PromptRejected {
                                                    reason: "agent_busy".into(),
                                                    text: first_text_block(&blocks),
                                                })
                                                .await;
                                        }
                                    }
                                    // Start the next parked steer, if the
                                    // outcome above left any parked.
                                    if let Some(next) = steer_backlog.pop_front() {
                                        steer_or_backlog!(next);
                                    }
                                }
                                cmd = cmd_rx.recv() => {
                                    match cmd {
                                        Some(ClientCmd::Cancel) => {
                                            info!(
                                                target: "acp.protocol",
                                                "sending session/cancel during in-flight prompt"
                                            );
                                            send_session_cancel!()?;
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
                                            let _ = send_session_cancel!();
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
                                                ConfigOptionDispatchPurpose::Generic,
                                                event_tx_for_block.clone(),
                                            );
                                        }
                                        Some(ClientCmd::SetMode(mode_id)) => {
                                            dispatch_set_mode(
                                                &connection,
                                                &acp_session_id,
                                                mode_id,
                                                &available_mode_ids,
                                                mode_config_option_id.as_deref(),
                                                event_tx_for_block.clone(),
                                                true,
                                            );
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
                                        Some(ClientCmd::Prompt(followup_blocks)) => {
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
                                            //
                                            // Checked ahead of steering
                                            // (#2805): this turn is on its way
                                            // to forced termination, so
                                            // injecting into it would strand
                                            // the message in a turn nobody
                                            // finishes. Reject first so it is
                                            // never lost, then escalate.
                                            if cancelling {
                                                warn!(
                                                    target: "acp.protocol",
                                                    session = %session_label,
                                                    "follow-up prompt arrived while cancel pending; escalating to runner restart"
                                                );
                                                let _ = event_tx_for_block
                                                    .send(Event::PromptRejected {
                                                        reason: "agent_busy".into(),
                                                        text: first_text_block(&followup_blocks),
                                                    })
                                                    .await;
                                                agent_unresponsive = true;
                                                shutdown = true;
                                                break;
                                            }
                                            // Same reasoning as the cancel arm
                                            // above, for the same reason: a
                                            // `/compact` turn only summarizes
                                            // context, so there is nothing in it
                                            // to steer. The adapter would answer
                                            // `Injected` and swallow the message
                                            // into a turn that never replies to
                                            // it, and unlike the `PromptRequired`
                                            // and error outcomes that path emits
                                            // no Retry pill and re-dispatches
                                            // nothing. Reject so it is never
                                            // lost. Backstop only: both
                                            // composers park a mid-compaction
                                            // send locally, so this catches the
                                            // POST that was already in flight
                                            // when the marker landed, plus
                                            // direct API callers. No escalation,
                                            // the turn is healthy. See #3219.
                                            let compacting = watchdog
                                                .off_protocol_work_seen()
                                                == Some(OffProtocolWorkKind::Compaction);
                                            if steering_capable && !compacting {
                                                // Hand it to the running turn
                                                // instead of refusing it. The
                                                // adapter decides whether a turn
                                                // is still running; the outcome
                                                // arm above applies its answer.
                                                steer_or_backlog!(followup_blocks);
                                            } else {
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
                                                warn!(
                                                    target: "acp.protocol",
                                                    compacting,
                                                    "received Prompt while one is in flight and it cannot be steered into; rejecting"
                                                );
                                                let _ = event_tx_for_block
                                                    .send(Event::PromptRejected {
                                                        reason: "agent_busy".into(),
                                                        text: first_text_block(&followup_blocks),
                                                    })
                                                    .await;
                                            }
                                        }
                                        Some(ClientCmd::ResetSession {
                                            text, respond_to, ..
                                        }) => {
                                            // Resetting under an in-flight
                                            // `session/prompt` would orphan
                                            // the pending turn on the old
                                            // session id. Refuse; the user
                                            // can stop the turn and retry
                                            // the clear. Mirror the busy-
                                            // Prompt arm above: emit a
                                            // `PromptRejected` so the caller
                                            // gets a terminal frame (retry
                                            // pill) under the persisted
                                            // UserPromptSent, not just an
                                            // HTTP error. See #2979.
                                            warn!(
                                                target: "acp.protocol",
                                                "conversation reset requested during in-flight prompt; refusing"
                                            );
                                            let _ = event_tx_for_block
                                                .send(Event::PromptRejected {
                                                    reason: "agent_busy".into(),
                                                    text,
                                                })
                                                .await;
                                            let _ = respond_to.send(ResetSessionOutcome::Failed {
                                                message: "a turn is in flight; stop it before clearing the conversation"
                                                    .into(),
                                            });
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
                        //
                        // Logged because the absence of this line after a
                        // `Stopped` is the signature of a stranded prompt
                        // future: the terminal was emitted by some other path
                        // while this loop stayed parked in its prompt arm, so
                        // the between-prompt watchdog is still disarmed and
                        // the next agent-initiated turn will get no terminal.
                        // See #3190.
                        prompt_in_flight.store(false, Ordering::Relaxed);
                        debug!(
                            target: "acp.protocol",
                            session = %session_label,
                            reason,
                            "prompt drain complete; between-prompt idle ownership restored"
                        );
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
                        if let Err(e) = send_session_cancel!() {
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
                        //
                        // This cancel is now the turn's terminal, so stand down
                        // every idle-completion path: claim the shared guard so
                        // the detached resume-idle task can't fire, and clear the
                        // adopted / between-prompt tracking so a later tick can't
                        // add a duplicate. See #2899.
                        terminal_claim.claim();
                        adopted_turn_active.store(false, Ordering::Relaxed);
                        between_prompt_active.store(false, Ordering::Relaxed);
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
                        // The supervisor owns the terminal here, so stand down the
                        // local idle-completion paths to avoid a duplicate. See #2899.
                        terminal_claim.claim();
                        adopted_turn_active.store(false, Ordering::Relaxed);
                        between_prompt_active.store(false, Ordering::Relaxed);
                        let _ = send_session_cancel!();
                    }
                    Some(ClientCmd::SetMode(mode_id)) => {
                        dispatch_set_mode(
                            &connection,
                            &acp_session_id,
                            mode_id,
                            &available_mode_ids,
                            mode_config_option_id.as_deref(),
                            event_tx_for_block.clone(),
                            false,
                        );
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
                            ConfigOptionDispatchPurpose::Generic,
                            event_tx_for_block.clone(),
                        );
                    }
                    Some(ClientCmd::ResetSession {
                        text,
                        deadline: reset_deadline,
                        respond_to,
                    }) => {
                        let work_in_flight = between_prompt_work_state(
                            &between_prompt_tools,
                            &between_prompt_bg_agents,
                        );
                        if work_in_flight.is_busy() {
                            // The parent prompt may already be complete while an
                            // open tool or async sub-agent from that session is
                            // still producing events. Resetting here would move
                            // the connection onto a fresh session and attribute
                            // those old-session events to the new conversation.
                            warn!(
                                target: "acp.protocol",
                                tool_calls_in_flight = work_in_flight.tool_calls,
                                background_agents_in_flight = work_in_flight.background_agents,
                                "conversation reset requested while between-prompt work is in flight; refusing"
                            );
                            let _ = event_tx_for_block
                                .send(Event::PromptRejected {
                                    reason: "agent_busy".into(),
                                    text,
                                })
                                .await;
                            let _ = respond_to.send(ResetSessionOutcome::Failed {
                                message:
                                    "agent work is still in flight; wait for it to finish before clearing the conversation"
                                        .into(),
                            });
                            continue;
                        }
                        // Driven conversation reset (#2979): a clear command
                        // hit a profile whose adapter cannot hand back a
                        // durable post-reset id (codex `/new` has no native
                        // reset; claude `/clear` resets but keeps serving the
                        // pre-clear id), so open a genuinely fresh session on
                        // the live worker and swap onto its id.
                        //
                        // Deliberately over the byte relay, NOT the v2
                        // control channel: the runner's `EstablishSession`
                        // replays its cached handshake once a session
                        // exists, which would hand back the old id. The
                        // relay request reaches the agent directly; the
                        // runner watches for it and refreshes its own
                        // handshake cache from the response (see
                        // `process/runner.rs`).
                        // Use the caller-created shared deadline for
                        // session/new and both config re-application
                        // requests. Queueing time counts, and per-request
                        // deadlines cannot accumulate past the outer guard.
                        info!(
                            target: "acp.protocol",
                            session = %session_label,
                            old_id = %acp_session_id.0,
                            "conversation reset: issuing fresh session/new on the live worker"
                        );
                        let req = NewSessionRequest::new(agent_cwd.clone())
                            .mcp_servers(mcp_servers_for_reset.clone());
                        match await_reset_request(
                            reset_deadline,
                            || connection.send_request(req).block_task(),
                        )
                        .await
                        {
                            Ok(new_session)
                                if new_session.session_id.0 != acp_session_id.0 =>
                            {
                                let new_id = new_session.session_id.clone();
                                // session/new is the irreversible reset
                                // commit. Adopt its id before attempting
                                // best-effort config restoration so this
                                // client and the runner cannot disagree
                                // about which session owns later prompts.
                                acp_session_id = new_id.clone();
                                available_mode_ids =
                                    new_session.modes.as_ref().map(|modes| {
                                        modes
                                            .available_modes
                                            .iter()
                                            .map(|m| m.id.0.to_string())
                                            .collect()
                                    });
                                mode_config_option_id = new_session
                                    .config_options
                                    .as_deref()
                                    .and_then(mode_config_id)
                                    .map(|id| id.0.to_string());
                                info!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    new_id = %new_id.0,
                                    "conversation reset: session/new succeeded, swapped acp_session_id"
                                );
                                // Keep every success boundary on the same
                                // FIFO. SessionCleared folds the transcript
                                // only after session/new committed, then
                                // SessionContextReset clears the old resume
                                // id before AcpSessionAssigned persists the
                                // fresh one.
                                let _ = event_tx_for_block.send(Event::SessionCleared).await;
                                let _ = event_tx_for_block
                                    .send(Event::SessionContextReset {
                                        reason: "conversation cleared; the agent started a fresh session".into(),
                                    })
                                    .await;
                                let _ = event_tx_for_block
                                    .send(Event::AcpSessionAssigned {
                                        acp_session_id: new_id.0.to_string(),
                                    })
                                    .await;
                                // Re-announce the fresh session's modes and
                                // config options (mirroring the handshake):
                                // the new session starts on adapter defaults,
                                // so the old session's picker state is stale.
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
                                            current_mode_id: modes
                                                .current_mode_id
                                                .0
                                                .to_string(),
                                            modes: infos,
                                        })
                                        .await;
                                }
                                if let Some(event) =
                                    config_options_event(new_session.config_options.clone())
                                {
                                    let _ = event_tx_for_block.send(event).await;
                                }
                                // Re-apply the configured structured-view
                                // defaults exactly like the spawn path does
                                // after its session/new: the fresh session
                                // starts on adapter defaults, so a
                                // configured effort/mode pick must be
                                // re-sent or `/new` silently downgrades it
                                // until the next worker restart. Best-effort
                                // with a warn, mirroring spawn.
                                let reset_config_options =
                                    new_session.config_options.as_deref();
                                for (value, config_id) in [
                                    (
                                        default_effort.as_deref(),
                                        reset_config_options
                                            .and_then(thought_level_config_id),
                                    ),
                                    (
                                        default_mode.as_deref(),
                                        reset_config_options.and_then(mode_config_id),
                                    ),
                                ] {
                                    let (Some(value), Some(config_id)) = (value, config_id)
                                    else {
                                        debug!(
                                            "post-reset config option skipped: no configured value or matching option id"
                                        );
                                        continue;
                                    };
                                    match await_reset_request(
                                        reset_deadline,
                                        || {
                                            connection
                                                .send_request(SetSessionConfigOptionRequest::new(
                                                new_id.clone(),
                                                config_id,
                                                SessionConfigValueId::new(value.to_string()),
                                            ))
                                                .block_task()
                                        },
                                    )
                                    .await
                                    {
                                        Ok(resp) => {
                                            if let Some(event) = config_options_event(Some(
                                                resp.config_options,
                                            )) {
                                                let _ =
                                                    event_tx_for_block.send(event).await;
                                            }
                                        }
                                        Err(ResetRequestError::Acp(e)) => {
                                            warn!(
                                                target: "acp.protocol",
                                                session = %session_label,
                                                value,
                                                "re-applying structured view default after reset failed: {e}"
                                            );
                                        }
                                        Err(ResetRequestError::TimedOut) => {
                                            warn!(
                                                target: "acp.protocol",
                                                session = %session_label,
                                                value,
                                                timeout_secs = SESSION_RESET_IN_TASK_TIMEOUT.as_secs(),
                                                "post-reset config re-application timed out; skipping remaining defaults"
                                            );
                                            break;
                                        }
                                    }
                                }
                                let _ = event_tx_for_block
                                    .send(Event::Stopped {
                                        reason: "session_reset".into(),
                                    })
                                    .await;
                                let _ = respond_to.send(ResetSessionOutcome::Reset {
                                    new_acp_session_id: new_id.0.to_string(),
                                });
                            }
                            Ok(_) => {
                                // Same id back: a stale runner (predating
                                // this reset path) answered the relay
                                // session/new from its handshake cache.
                                // Report honestly rather than pretending the
                                // model forgot; a worker restart clears it.
                                let message = "the worker replayed the existing session \
                                     instead of creating a fresh one; restart the \
                                     structured view worker to clear context"
                                    .to_string();
                                warn!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "conversation reset failed: {message}"
                                );
                                let _ = event_tx_for_block
                                    .send(Event::PromptRuntimeError {
                                        message: message.clone(),
                                    })
                                    .await;
                                let _ = event_tx_for_block
                                    .send(Event::Stopped {
                                        reason: "session_reset_failed".into(),
                                    })
                                    .await;
                                let _ = respond_to
                                    .send(ResetSessionOutcome::Failed { message });
                            }
                            Err(ResetRequestError::Acp(e)) => {
                                let message = format!("session/new failed: {e}");
                                warn!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "conversation reset failed: {message}"
                                );
                                let _ = event_tx_for_block
                                    .send(Event::PromptRuntimeError {
                                        message: message.clone(),
                                    })
                                    .await;
                                let _ = event_tx_for_block
                                    .send(Event::Stopped {
                                        reason: "session_reset_failed".into(),
                                    })
                                    .await;
                                let _ = respond_to
                                    .send(ResetSessionOutcome::Failed { message });
                            }
                            Err(ResetRequestError::TimedOut) => {
                                let message =
                                    "agent did not answer session/new before the reset deadline"
                                        .to_string();
                                warn!(
                                    target: "acp.protocol",
                                    session = %session_label,
                                    "conversation reset failed: {message}"
                                );
                                let _ = event_tx_for_block
                                    .send(Event::PromptRuntimeError {
                                        message: message.clone(),
                                    })
                                    .await;
                                let _ = event_tx_for_block
                                    .send(Event::Stopped {
                                        reason: "session_reset_failed".into(),
                                    })
                                    .await;
                                let _ = respond_to
                                    .send(ResetSessionOutcome::Failed { message });
                            }
                        }
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
            } else if let Some(info) = classify_rate_limit_from_message(
                &message,
                captured_rate_limit_resets_at(&last_rate_limit_rejections, chrono::Utc::now()),
            ) {
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
