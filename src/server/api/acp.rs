//! REST endpoints for structured view sessions.
//!
//! Spawn / shutdown / send-prompt / resolve-approval. The structured view
//! WebSocket carries the read side; this module is the write side.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::acp::approvals::Nonce;
use crate::acp::event_store::AttachmentBlob;
use crate::acp::protocol::{
    ApprovalDecisionWire, ContextPrimerQuery, ContextPrimerResponse, DiffCommentsPromptRequest,
    FilesResponse, PromptAttachmentUpload, PromptRequest, ReplayQuery, ReplayResponse,
    ResolveApprovalRequest, SwitchAgentRequest, SwitchAgentResponse,
};
use crate::acp::state::PromptAttachmentKind;
use crate::acp::supervisor::SupervisorError;
use crate::server::AppState;

/// Maximum attachments per prompt.
const MAX_ATTACHMENTS: usize = 8;
/// Maximum decoded size of a single attachment (10 MiB).
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Maximum decoded size of all attachments on one prompt (20 MiB).
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// MIME types accepted per attachment kind. Conservative on purpose:
/// `image/svg+xml` is excluded (scriptable XML), and embedded resources
/// are limited to inert text/document types. The image kind is also
/// magic-byte sniffed; a declared MIME that the bytes don't back is
/// rejected. See #1000 / #965 and the design debate.
fn mime_allowed(kind: PromptAttachmentKind, mime: &str) -> bool {
    match kind {
        PromptAttachmentKind::Image => {
            matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        }
        PromptAttachmentKind::Audio => matches!(
            mime,
            "audio/mpeg" | "audio/wav" | "audio/x-wav" | "audio/webm" | "audio/ogg" | "audio/mp4"
        ),
        PromptAttachmentKind::Resource => matches!(
            mime,
            "text/plain" | "text/markdown" | "application/json" | "application/pdf"
        ),
    }
}

/// True if `bytes` start with a magic-number signature for a supported
/// raster image. Guards against a client mislabeling arbitrary bytes as
/// `image/png` to smuggle them past the allowlist.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Decode, size-check, MIME-check, magic-byte-sniff and capability-gate
/// the uploaded attachments. Returns the decoded blobs ready to persist
/// and forward, or an HTTP `(status, message)` to return verbatim.
/// Runs entirely before the prompt is published so a rejected prompt
/// never leaves a half-rendered attachment in the transcript.
fn validate_attachments(
    state: &AppState,
    session_id: &str,
    uploads: &[PromptAttachmentUpload],
) -> Result<Vec<AttachmentBlob>, (StatusCode, String)> {
    use base64::Engine as _;
    if uploads.is_empty() {
        return Ok(Vec::new());
    }
    if uploads.len() > MAX_ATTACHMENTS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many attachments (max {MAX_ATTACHMENTS})"),
        ));
    }
    // Capability gate: the agent must advertise the matching prompt
    // capability. `None` means the handshake hasn't reported caps yet;
    // reject rather than forward bytes the agent may not accept.
    let caps = state.acp_event_store.latest_prompt_capabilities(session_id);
    let (image_ok, audio_ok, embedded_ok) = match caps {
        Some(c) => c,
        None => {
            return Err((
                StatusCode::CONFLICT,
                "agent capabilities not known yet; cannot accept attachments".to_string(),
            ))
        }
    };

    let mut blobs = Vec::with_capacity(uploads.len());
    let mut total = 0usize;
    for up in uploads {
        let kind_ok = match up.kind {
            PromptAttachmentKind::Image => image_ok,
            PromptAttachmentKind::Audio => audio_ok,
            PromptAttachmentKind::Resource => embedded_ok,
        };
        if !kind_ok {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "the current agent does not accept {} attachments",
                    up.kind.as_str()
                ),
            ));
        }
        if !mime_allowed(up.kind, &up.mime_type) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unsupported attachment type: {}", up.mime_type),
            ));
        }
        // Reject oversized payloads before allocating the decoded buffer.
        // base64 expands 4/3, so an encoded string longer than the
        // encoded-equivalent of MAX_ATTACHMENT_BYTES can never fit the
        // decoded cap; bailing here stops a client forcing a huge
        // allocation (memory-pressure DoS). The decoded-size checks below
        // remain as the second line of defense. The +4 covers base64
        // rounding/padding so a legitimately max-sized blob is not rejected.
        let encoded_limit = MAX_ATTACHMENT_BYTES / 3 * 4 + 4;
        if up.data.len() > encoded_limit {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachment exceeds {} MiB limit",
                    MAX_ATTACHMENT_BYTES / (1024 * 1024)
                ),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(up.data.as_bytes())
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "attachment is not valid base64".to_string(),
                )
            })?;
        if bytes.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "empty attachment".to_string()));
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachment exceeds {} MiB limit",
                    MAX_ATTACHMENT_BYTES / (1024 * 1024)
                ),
            ));
        }
        if up.kind == PromptAttachmentKind::Image {
            let Some(sniffed_mime) = sniff_image_mime(&bytes) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "attachment bytes are not a supported image".to_string(),
                ));
            };
            if sniffed_mime != up.mime_type {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "attachment MIME does not match declared content-type".to_string(),
                ));
            }
        }
        total += bytes.len();
        if total > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachments exceed {} MiB total limit",
                    MAX_TOTAL_ATTACHMENT_BYTES / (1024 * 1024)
                ),
            ));
        }
        blobs.push(AttachmentBlob {
            id: uuid::Uuid::new_v4().to_string(),
            kind: up.kind,
            mime_type: up.mime_type.clone(),
            name: up.name.clone(),
            data: bytes,
        });
    }
    Ok(blobs)
}

#[derive(Debug, Deserialize)]
pub struct SpawnAcpRequest {
    /// Optional override; falls back to the acp_default_agent
    /// setting / aoe-agent.
    pub agent: Option<String>,
    /// Optional model override; forwarded to aoe-agent as
    /// AOE_AGENT_MODEL env var.
    pub model: Option<String>,
    /// Optional additional dirs the agent may read/write through
    /// fs/*. The session's worktree is always allowed.
    #[serde(default)]
    pub additional_dirs: Vec<PathBuf>,
    /// Provider env vars to forward (e.g., ANTHROPIC_API_KEY). Will be
    /// filtered against the agent's allowlist.
    #[serde(default)]
    pub provider_env: Vec<EnvPair>,
}

#[derive(Debug, Deserialize)]
pub struct EnvPair {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct SpawnAcpResponse {
    pub session_id: String,
    pub agent: String,
    pub status: &'static str,
}

/// 403 helper for `aoe serve --read-only`. Matches the response shape used
/// by `sessions.rs` write endpoints so the read-only contract is uniform
/// across the API surface.
pub(crate) fn read_only_block(state: &AppState) -> Option<axum::response::Response> {
    if state.read_only {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "read_only",
                    "message": "Server is in read-only mode",
                })),
            )
                .into_response(),
        );
    }
    None
}

pub async fn spawn_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SpawnAcpRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    drop(instances);

    // Pick the structured view agent: explicit request override > stored
    // agent_name on the instance > registry entry keyed on the
    // tool name (so tool="opencode" → opencode-acp, etc).
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            explicit.as_deref(),
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;

    let cwd = PathBuf::from(&instance.project_path);
    let provider_env: Vec<(String, String)> = req
        .provider_env
        .into_iter()
        .map(|p| (p.key, p.value))
        .collect();
    let model = req.model.or_else(|| instance.agent_model.clone());
    let stored_acp_session_id = instance.acp_session_id.clone();
    let yolo_mode = instance.yolo_mode;

    let inst_lock = state.instance_lock(&id).await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
        &state.instances,
        &inst_lock,
        &id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sandbox container ensure failed: {e}"),
            )
                .into_response();
        }
    };
    // Pass the session profile through regardless of sandboxing so the
    // spawn path resolves agent_acp_cmd and worker env from the right
    // profile for non-sandbox sessions too.
    let source_profile = Some(instance.source_profile.clone());
    let agent_for_response = agent.clone();
    match state
        .acp_supervisor
        .spawn(crate::acp::supervisor::SpawnRequest {
            session_id: id.clone(),
            agent,
            cwd,
            additional_dirs: req.additional_dirs,
            provider_env,
            model,
            effort: None,
            stored_acp_session_id,
            sandbox_info,
            source_profile,
            yolo_mode,
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &instance.tool,
                &instance.command,
            ),
        })
        .await
    {
        Ok(()) => Json(SpawnAcpResponse {
            session_id: id,
            agent: agent_for_response,
            status: "running",
        })
        .into_response(),
        Err(SupervisorError::AlreadyRunning(_)) => (
            StatusCode::CONFLICT,
            "structured view already running for session",
        )
            .into_response(),
        Err(SupervisorError::UnknownAgent(name)) => (
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {name}"),
        )
            .into_response(),
        Err(e @ SupervisorError::CapacityFull { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, format!("{e}")).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("spawn failed: {e}"),
        )
            .into_response(),
    }
}

pub async fn shutdown_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    match state.acp_supervisor.shutdown(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(SupervisorError::UnknownSession(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("shutdown failed: {e}"),
        )
            .into_response(),
    }
}

/// One entry in the structured view ACP registry. Names match the `target`
/// field accepted by `/acp/switch-agent`. Used by the rate-limit
/// recovery modal to list available backends. See #1282.
#[derive(Debug, Serialize)]
pub struct AcpAgentInfo {
    pub name: String,
    pub description: String,
    pub command: String,
}

/// `GET /api/acp/agents`: list the ACP registry entries the
/// supervisor knows about. Distinct from `/api/agents` (which lists
/// session-tool agents like claude/codex/cursor for the wizard);
/// this returns the *structured view* ACP backend registry so the recovery
/// modal can show what the user can hand off to. See #1282.
pub async fn list_acp_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let registry = state.acp_supervisor.registry_snapshot().await;
    let mut entries: Vec<AcpAgentInfo> = registry
        .list()
        .into_iter()
        .map(|(name, spec)| AcpAgentInfo {
            name: name.clone(),
            description: spec.description.clone(),
            command: spec.command.clone(),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Json(entries).into_response()
}

/// Atomically move a structured view session from one ACP backend to another.
/// Two callers drive this: the rate-limit recovery flow (#1282), which
/// hands a Claude-rate-limited session off to `codex` (or another
/// installed backend), and explicit user-initiated switches from the
/// composer control or `aoe acp switch-agent`. Both keep the
/// transcript; only the recorded `reason` differs.
///
/// Sequence:
///   1. Validate `target` exists in the structured view registry.
///   2. Snapshot `before_seq` = highest seq in the event store, so the
///      handoff `AgentSwitched` event lands at a known cursor and the
///      frontend's primer fetch (`fetchContextPrimer(before_seq)`)
///      excludes the handoff itself from the recap.
///   3. `shutdown_and_wait` on the current worker so the runner
///      subprocess actually exits and releases its socket before the
///      new spawn binds the same path.
///   4. Spawn the target agent. On failure: do NOT mutate the
///      instance, return 5xx. The user keeps their prior
///      `agent_name` and can retry from the recovery banner.
///   5. Persist `agent_name = target`, clear
///      `acp_session_id` (the Claude session id is meaningless
///      to Codex, so a future `session/load` against it would fail and
///      surface a `SessionContextReset` we don't want).
///   6. Emit `AgentSwitched { from, to, reason }` so the reducer
///      clears agent-specific transient state and the UI renders a
///      transcript divider.
pub async fn switch_acp_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SwitchAgentRequest>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }

    let target = req.target.trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target is required").into_response();
    }
    if !state.acp_supervisor.registry_has_agent(&target).await {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {target}"),
        )
            .into_response();
    }

    let instance = {
        let instances = state.instances.read().await;
        match instances.iter().find(|i| i.id == id).cloned() {
            Some(inst) => inst,
            None => return (StatusCode::NOT_FOUND, "session not found").into_response(),
        }
    };
    let from_agent = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            instance.agent_name.as_deref(),
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;
    if from_agent == target {
        return (
            StatusCode::BAD_REQUEST,
            format!("session is already using {target}"),
        )
            .into_response();
    }
    let before_seq = state.acp_event_store.highest_seq(&id);

    if let Err(e) = state
        .acp_supervisor
        .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("shutdown failed before agent switch: {e}"),
        )
            .into_response();
    }

    let cwd = PathBuf::from(&instance.project_path);
    let inst_lock = state.instance_lock(&id).await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
        &state.instances,
        &inst_lock,
        &id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sandbox container ensure failed: {e}"),
            )
                .into_response();
        }
    };
    // Pass the session profile through regardless of sandboxing so the
    // spawn path resolves agent_acp_cmd and worker env from the right
    // profile for non-sandbox sessions too.
    let source_profile = Some(instance.source_profile.clone());

    let model = req.model.clone().or(instance.agent_model.clone());
    let spawn_result = state
        .acp_supervisor
        .spawn(crate::acp::supervisor::SpawnRequest {
            session_id: id.clone(),
            agent: target.clone(),
            cwd,
            additional_dirs: vec![],
            provider_env: vec![],
            model: model.clone(),
            effort: None,
            // Different ACP backend; the cached Claude session id would
            // be rejected by codex / opencode.
            stored_acp_session_id: None,
            sandbox_info,
            source_profile,
            yolo_mode: instance.yolo_mode,
            // Gated in the supervisor: only applies when the selected
            // agent equals the instance tool and its binary matches, so
            // an explicit switch to a different agent is unaffected.
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &instance.tool,
                &instance.command,
            ),
        })
        .await;
    if let Err(e) = spawn_result {
        return match e {
            SupervisorError::UnknownAgent(name) => (
                StatusCode::BAD_REQUEST,
                format!("unknown structured view agent: {name}"),
            )
                .into_response(),
            SupervisorError::AlreadyRunning(_) => (
                StatusCode::CONFLICT,
                "structured view worker already running for session",
            )
                .into_response(),
            e @ SupervisorError::CapacityFull { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, format!("{e}")).into_response()
            }
            e => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("spawn failed: {e}"),
            )
                .into_response(),
        };
    }

    // Spawn succeeded: a mid-session agent switch actually happened. Tally it
    // for the opt-in telemetry snapshot.
    state
        .telemetry_structured
        .agent_switches
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Persist the agent change AFTER spawn succeeded. The new agent's
    // session/new will emit a fresh AcpSessionAssigned which will then
    // populate acp_session_id via the existing listener.
    let profile_for_save = instance.source_profile.clone();
    let id_for_save = id.clone();
    let target_for_save = target.clone();
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.agent_name = Some(target_for_save.clone());
            inst.acp_session_id = None;
            if let Some(m) = &model {
                inst.agent_model = Some(m.clone());
            }
        }
    }
    if let Ok(storage) = crate::session::Storage::new(&profile_for_save) {
        if let Err(e) = storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
                inst.agent_name = Some(target_for_save.clone());
                inst.acp_session_id = None;
            }
            Ok(())
        }) {
            tracing::error!(
                target: "http.api.acp",
                session = %id_for_save,
                "failed to persist agent_name after switch: {e}"
            );
        }
    }

    let reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("manual")
        .to_string();
    let switch_seq = state.acp_supervisor.publish_agent_switched(
        &id,
        from_agent.clone(),
        target.clone(),
        reason,
    );

    Json(SwitchAgentResponse {
        session_id: id,
        agent: target,
        before_seq,
        switch_seq,
        status: "running".to_string(),
    })
    .into_response()
}

/// Auto-wake an archived, snoozed, or idle-dormant session before a
/// prompt is forwarded, matching the tmux send path
/// (`/api/sessions/{id}/send`). `touch_last_accessed` clears
/// `archived_at`, `snoozed_until`, and `idle_dormant_since` so the
/// structured view reconciler stops skipping the session on its next ~2s tick and
/// respawns the worker; the frontend's queue drains as soon as the fresh
/// `AcpSessionAssigned` lands. The idle_dormant clear is the wake path for
/// auto-stopped idle workers (#1689); a worker reaped for inactivity
/// respawns on the next prompt. See #1581.
///
/// The in-memory mutation and the disk persistence are both held under
/// `state.instance_lock(&id)` so they serialize against other
/// session-mutating endpoints (archive / snooze / pin / rename) on the
/// same id. Without this guard, a concurrent archive PATCH could
/// interleave with the touch and produce a lost write (archive sets
/// archived_at = Some, touch clears it, archive's persist lands first,
/// touch's persist lands second and overwrites the archive). The lock is
/// dropped before the caller reaches the supervisor: publish/send take
/// their own locks downstream and holding ours across the agent forward
/// would serialize prompts unnecessarily and stall siblings.
/// Returns whether the wake cleared an idle-dormant marker, so the caller
/// can synchronously kick a background respawn (the reconciler's ~2s tick
/// is too slow for the prompt that triggered the wake; see #1748).
async fn touch_and_wake_if_sunk(state: &Arc<AppState>, id: &str) -> bool {
    let inst_lock = state.instance_lock(id).await;
    let _guard = inst_lock.lock().await;
    let (triage_changed, woke_idle_dormant) = {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            let was_idle_dormant = inst.is_idle_dormant();
            let was_sunk = inst.is_archived() || inst.is_snoozed() || was_idle_dormant;
            if was_sunk {
                inst.touch_last_accessed();
                if was_idle_dormant {
                    // Pairs with the "auto-stopped idle structured view worker"
                    // info log in the reconciler's reap pass (#1689) so the
                    // stop/resume cycle is traceable in the daemon log.
                    tracing::info!(
                        target: "acp.supervisor",
                        session = %id,
                        "waking idle-dormant structured view session on prompt; spawning a fresh worker"
                    );
                }
            }
            (was_sunk, was_idle_dormant)
        } else {
            (false, false)
        }
    };
    if triage_changed {
        let profile = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.source_profile.clone())
                .unwrap_or_default()
        };
        if let Ok(storage) = crate::session::Storage::new(&profile) {
            let id_clone = id.to_string();
            let session_id_for_log = id.to_string();
            match tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                        inst.touch_last_accessed();
                    }
                    Ok(())
                })
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    target: "http.api.acp",
                    session = %session_id_for_log,
                    "failed to save after triage auto-wake: {e}"
                ),
                Err(join_err) => tracing::warn!(
                    target: "http.api.acp",
                    session = %session_id_for_log,
                    "spawn_blocking join error during triage auto-wake save: {join_err}"
                ),
            }
        }
    }
    woke_idle_dormant
}

pub async fn acp_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let woke_idle_dormant = touch_and_wake_if_sunk(&state, &id).await;
    {
        let instances = state.instances.read().await;
        if !instances.iter().any(|i| i.id == id) {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        }
    }
    // Decode + validate + capability-gate attachments BEFORE publishing
    // so a rejected prompt never leaves a half-rendered attachment in
    // the transcript (the publish path is otherwise authoritative). See
    // #1000 / #965. Validating before the resume trigger below also
    // avoids respawning a worker for a request we are about to reject.
    let attachments = match validate_attachments(&state, &id, &req.attachments) {
        Ok(a) => a,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    // Idle-dormant wake: the worker was auto-stopped for inactivity
    // (#1689) and the reconciler will not respawn it until its next ~2s
    // tick. Reserve the resume slot synchronously and drive a fresh spawn
    // in a detached task NOW, so the `send_prompt` below blocks on
    // `wait_for_worker` until the worker is live instead of racing ahead
    // to a 404. The detached task survives this request being cancelled on
    // client disconnect. See #1748.
    if woke_idle_dormant {
        use crate::server::acp_reconciler::ResumeTrigger;
        match crate::server::acp_reconciler::trigger_resume_background(&state, &id).await {
            Ok(ResumeTrigger::NotFound) => {
                // The session was deleted (or triaged) between the wake and
                // the resume snapshot. Do not publish into a session that no
                // longer exists; a 404 is the honest answer, not a retryable
                // worker_not_ready. See #1748.
                return (StatusCode::NOT_FOUND, "session not found").into_response();
            }
            Ok(_) => {}
            Err(SupervisorError::CapacityFull { current, limit }) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("worker_capacity_full ({current}/{limit})"),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("worker_not_ready: {e}"),
                )
                    .into_response();
            }
        }
    }
    // Publish the user's prompt into the event stream BEFORE forwarding
    // to the agent so the replay buffer / on-disk store captures it
    // even if the agent forward fails. The frontend treats UserPromptSent
    // as authoritative and dedupes against its own optimistic row.
    state
        .acp_supervisor
        .publish_user_prompt_with_attachments(&id, req.text.clone(), &attachments)
        .await;
    match state
        .acp_supervisor
        .send_prompt(&id, &req.text, &attachments)
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(SupervisorError::UnknownSession(_)) => {
            if woke_idle_dormant {
                // The respawn we kicked above did not finish within
                // `send_prompt`'s wait window (slow sandbox / spawn). The
                // worker is still coming; signal a retryable typed status
                // so the frontend keeps the prompt queued and re-fires on
                // the next `AcpSessionAssigned`, rather than dropping it
                // on a 404. See #1748.
                (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    "session has no running structured view",
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("prompt failed: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/sessions/{id}/acp/prompt/diff-comments`: the typed
/// successor to the diff-comments sentinel hack. The frontend sends the
/// structured review plus the `assembled_markdown` it previewed; the
/// server records a typed `Event::UserDiffCommentsPrompt` (so the
/// transcript re-renders the rich card on replay) and forwards only
/// `assembled_markdown` to the agent, so the agent never sees the old
/// base64 sentinel noise. Mirrors `acp_prompt`'s auto-wake +
/// publish-before-forward ordering.
pub async fn acp_prompt_diff_comments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<DiffCommentsPromptRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let woke_idle_dormant = touch_and_wake_if_sunk(&state, &id).await;
    {
        let instances = state.instances.read().await;
        if !instances.iter().any(|i| i.id == id) {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        }
    }
    // Idle-dormant wake: respawn synchronously-reserved + detached so the
    // send_prompt below waits for the worker instead of 404ing. Mirrors
    // acp_prompt. See #1748.
    if woke_idle_dormant {
        use crate::server::acp_reconciler::ResumeTrigger;
        match crate::server::acp_reconciler::trigger_resume_background(&state, &id).await {
            Ok(ResumeTrigger::NotFound) => {
                return (StatusCode::NOT_FOUND, "session not found").into_response();
            }
            Ok(_) => {}
            Err(SupervisorError::CapacityFull { current, limit }) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("worker_capacity_full ({current}/{limit})"),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("worker_not_ready: {e}"),
                )
                    .into_response();
            }
        }
    }
    // Publish the typed event BEFORE forwarding so the replay buffer /
    // on-disk store captures the user's side even if the forward fails,
    // matching acp_prompt.
    state
        .acp_supervisor
        .publish_user_diff_comments_prompt(
            &id,
            req.intro,
            req.outro,
            req.is_multi_repo,
            req.comments,
            req.assembled_markdown.clone(),
        )
        .await;
    match state
        .acp_supervisor
        .send_prompt(&id, &req.assembled_markdown, &[])
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(SupervisorError::UnknownSession(_)) => {
            if woke_idle_dormant {
                (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    "session has no running structured view",
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("prompt failed: {e}"),
        )
            .into_response(),
    }
}

/// Serve one persisted prompt attachment's bytes for transcript replay.
/// Scoped by session id so a token valid for one session can't read
/// another's blob by guessing the attachment id. Inherits the global
/// auth middleware. See #1000 / #965.
pub async fn acp_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, attachment_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.acp_event_store.load_attachment(&id, &attachment_id) {
        Some((mime, bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, mime),
                (
                    axum::http::header::X_CONTENT_TYPE_OPTIONS,
                    "nosniff".to_string(),
                ),
                (
                    axum::http::header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable".to_string(),
                ),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "attachment not found").into_response(),
    }
}

pub async fn acp_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    match state.acp_supervisor.cancel_prompt(&id).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(SupervisorError::UnknownSession(_)) => (
            StatusCode::NOT_FOUND,
            "session has no running structured view",
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cancel failed: {e}"),
        )
            .into_response(),
    }
}

/// Escape hatch for the "stuck spinner" failure mode (#1100). Publishes
/// a synthetic `Stopped { reason: "user_forced" }` so every connected UI
/// drops `turnActive`, then best-effort cancels any in-flight agent
/// turn. Always 202: the publish is idempotent and the cancel is
/// fire-and-forget; any genuine read-only mode is rejected upstream.
pub async fn acp_force_end_turn(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    state.acp_supervisor.force_end_turn(&id).await;
    StatusCode::ACCEPTED.into_response()
}

/// List workspace files for the @-mention picker. Walks the session's
/// project_path tree, skipping VCS/build dirs and dot-files at the
/// top level. Capped at 5000 entries.
pub async fn acp_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let instances = state.instances.read().await;
    let Some(inst) = instances.iter().find(|i| i.id == id).cloned() else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    drop(instances);

    let root = std::path::PathBuf::from(&inst.project_path);
    let result = tokio::task::spawn_blocking(move || list_files(&root, 5000)).await;
    match result {
        Ok(Ok((files, truncated))) => Json(FilesResponse { files, truncated }).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("file listing failed: {e}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("blocking task failed: {e}"),
        )
            .into_response(),
    }
}

/// Default replay page size when the client omits `limit`. Bounds the
/// daemon's per-request buffer and response size for a long session.
const DEFAULT_REPLAY_PAGE: usize = 1000;
/// Hard cap on a client-requested replay page, so an oversized `limit`
/// can't reintroduce the unbounded-buffer footprint this paging removes.
const MAX_REPLAY_PAGE: usize = 2000;

const WORKER_LOG_DEFAULT_TAIL: usize = 200;
const WORKER_LOG_MAX_TAIL: usize = 2000;
/// Cap the read size so a runaway log file can't pin the daemon. A 4 MiB
/// window comfortably covers `WORKER_LOG_MAX_TAIL` lines worth of stderr
/// while keeping memory predictable.
const WORKER_LOG_MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct WorkerLogQuery {
    /// Number of trailing lines to return. Clamped to
    /// [1, `WORKER_LOG_MAX_TAIL`]; defaults to `WORKER_LOG_DEFAULT_TAIL`.
    pub tail: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct WorkerLogResponse {
    pub path: String,
    pub exists: bool,
    pub tail: String,
    pub lines_returned: usize,
    /// `true` when the file was larger than the read window and the
    /// returned tail starts mid-stream rather than at the beginning of
    /// the file.
    pub truncated: bool,
}

/// Tail of the per-session structured view runner log file. Surfaces the same
/// stream `aoe acp logs --session <id>` reads, so a dashboard user
/// (Funnel / no host terminal) can see the verbatim adapter error when
/// the structured view startup banner is otherwise opaque. Read-only; allowed
/// in `--read-only` mode.
pub async fn acp_worker_log(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<WorkerLogQuery>,
) -> impl IntoResponse {
    let instances = state.instances.read().await;
    let session_known = instances.iter().any(|i| i.id == id);
    drop(instances);
    if !session_known {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }

    let log_path = match crate::acp::worker_registry::log_path_for(&id) {
        Ok(p) => p,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid session id: {e}")).into_response();
        }
    };

    let tail = q
        .tail
        .unwrap_or(WORKER_LOG_DEFAULT_TAIL)
        .clamp(1, WORKER_LOG_MAX_TAIL);

    let log_path_display = log_path.display().to_string();
    let read_result = tokio::task::spawn_blocking(move || read_log_tail(&log_path, tail)).await;
    match read_result {
        Ok(Ok((lines, truncated, exists))) => {
            let lines_returned = lines.len();
            let body = lines.join("\n");
            Json(WorkerLogResponse {
                path: log_path_display,
                exists,
                tail: body,
                lines_returned,
                truncated,
            })
            .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("worker log read failed: {e}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("blocking task failed: {e}"),
        )
            .into_response(),
    }
}

pub(crate) fn read_log_tail(
    path: &std::path::Path,
    tail: usize,
) -> std::io::Result<(Vec<String>, bool, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false, false));
        }
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let read_from = len.saturating_sub(WORKER_LOG_MAX_READ_BYTES);
    let truncated = len > WORKER_LOG_MAX_READ_BYTES;

    // If the read window starts inside a line, the first line we parse
    // is a partial line. If the previous byte is '\n' the first line is
    // whole and we keep it. Probing one byte before `read_from` is the
    // cheap way to tell them apart without re-reading the prefix.
    let mut prev_byte = [0u8; 1];
    let prev_is_newline = if truncated && read_from > 0 {
        file.seek(SeekFrom::Start(read_from - 1))?;
        file.read_exact(&mut prev_byte)?;
        prev_byte[0] == b'\n'
    } else {
        false
    };

    file.seek(SeekFrom::Start(read_from))?;
    let window_len = len - read_from;
    let mut raw = Vec::with_capacity(window_len as usize);
    // Bound the read with `take` so a concurrent append between
    // `metadata()` and now cannot grow `raw` beyond the precomputed
    // window. Keeps the 4 MiB cap a hard ceiling, not a target.
    (&mut file).take(window_len).read_to_end(&mut raw)?;
    // Lossy decode so a partial UTF-8 boundary at the window edge cannot
    // 500 the endpoint; the tail is for human eyeballs, exact bytes are
    // not required.
    let buf = String::from_utf8_lossy(&raw);
    let mut lines: Vec<String> = buf.lines().map(|l| l.to_string()).collect();
    if truncated && !prev_is_newline && !lines.is_empty() {
        lines.remove(0);
    }
    let total = lines.len();
    let start = total.saturating_sub(tail);
    Ok((lines[start..].to_vec(), truncated, true))
}

fn list_files(root: &std::path::Path, cap: usize) -> std::io::Result<(Vec<String>, bool)> {
    // Names we never want to recurse into. Top-level only — a deep
    // `node_modules` inside a sub-package would still show up via its
    // parent path which is fine.
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        ".venv",
        ".cache",
        ".turbo",
        ".idea",
        ".vscode",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    let mut truncated = false;
    while let Some(dir) = stack.pop() {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        // Sort each directory's entries by name before walking them so
        // the traversal (and therefore which files survive the `cap`) is
        // deterministic; `read_dir` order is platform/filesystem
        // dependent and otherwise unspecified.
        let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(e) => e.flatten().collect(),
            Err(_) => continue,
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            if SKIP_DIRS.iter().any(|d| *d == name_str.as_ref()) {
                continue;
            }
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().to_string());
                    if out.len() >= cap {
                        truncated = true;
                        break;
                    }
                }
            }
        }
    }
    out.sort();
    Ok((out, truncated))
}

/* ── View switching: structured view ↔ tmux ─────────────────────── */

#[derive(Debug, Serialize)]
pub struct ViewSwitchResponse {
    pub session_id: String,
    pub view: crate::session::View,
}

/// Switch a tmux-mode session to structured view. Idempotent: a session that
/// is already structured view-mode returns 200 with no work done.
///
/// History is destroyed in the swap: the tmux scrollback is dropped
/// when the pane is killed; structured view starts with an empty conversation.
/// The frontend warns the user before calling this endpoint.
pub async fn acp_enable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let (mut instance, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id).cloned() else {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        };
        let profile = inst.source_profile.clone();
        (inst, profile)
    };

    if instance.is_structured() {
        return Json(ViewSwitchResponse {
            session_id: id,
            view: crate::session::View::Structured,
        })
        .into_response();
    }

    // Verify the tool has an ACP-capable agent. Otherwise there's no
    // agent to spawn and the swap would just produce a dead structured view.
    // Built-in tools resolve from the registry; a custom agent is valid
    // when it declares an `agent_acp_cmd` in its profile config.
    let agent_name = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            instance.agent_name.as_deref(),
            &profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;
    let registry = state.acp_supervisor.registry_snapshot().await;
    let resolvable = registry.get(&agent_name).is_some()
        || state
            .acp_supervisor
            .custom_agent_has_acp_cmd(
                &agent_name,
                &profile,
                std::path::Path::new(&instance.project_path),
            )
            .await;
    if !resolvable {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "no structured view agent registered for tool {:?}",
                instance.tool
            ),
        )
            .into_response();
    }

    // A real terminal -> acp transition is now committed (the idempotent
    // already-acp and unresolvable-agent cases returned above). Tally the
    // substrate toggle for the opt-in telemetry snapshot.
    state
        .telemetry_structured
        .view_toggles
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Tear down the tmux side. Best-effort: a stale tmux name should
    // not block the swap. Run on a blocking pool worker because each
    // kill shells out. Warn on agent kill failure to keep signal for
    // this user-initiated action; ancillary kinds delegate to the
    // shared helper so any future kind picked up by the audit lands
    // here automatically.
    let inst_for_kill = instance.clone();
    let id_for_log = id.clone();
    let kill_join = tokio::task::spawn_blocking(move || {
        if let Err(e) = inst_for_kill.kill() {
            tracing::warn!(target: "acp.switch", session = %inst_for_kill.id, "kill tmux failed: {e}");
        }
        inst_for_kill.kill_ancillary_tmux_sessions();
    })
    .await;
    if let Err(join_err) = kill_join {
        tracing::error!(target: "acp.switch", session = %id_for_log, "tmux teardown task panicked: {join_err}");
    }
    instance.view = crate::session::View::Structured;
    instance.resume_intent = crate::session::ResumeIntent::Default;

    // Persist before spawning so a crash mid-swap leaves us in the
    // declared end state, not a half-broken intermediate.
    //
    // The on-disk and in-memory updates mutate ONLY the structured view-specific
    // fields (`structured_view = true`, `resume_intent = Default`).
    // Wholesale replacement with a pre-lock snapshot would clobber
    // concurrent writes to other fields (status, last_accessed,
    // agent_session_id) made by the status poll loop or other handlers
    // between the snapshot and the lock acquisition.
    //
    // Clearing `resume_intent` here closes the dormant-intent gap: the
    // CLI `set-session-id` writes `Use(sid)` to disk, then the user
    // toggles structured view on; without this reset, a future `acp_disable`
    // would reload the stale `Use(sid)` and the next non-structured view launch
    // would honor a session id the user no longer expects. See #1745.
    {
        let mut instances = state.instances.write().await;
        if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
            slot.view = crate::session::View::Structured;
            slot.resume_intent = crate::session::ResumeIntent::Default;
        }
    }
    let id_for_save = id.clone();
    let profile_for_save = profile.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::new(&profile_for_save)?;
        storage.update(|all, _groups| {
            if let Some(slot) = all.iter_mut().find(|i| i.id == id_for_save) {
                slot.view = crate::session::View::Structured;
                slot.resume_intent = crate::session::ResumeIntent::Default;
            }
            Ok(())
        })?;
        Ok(())
    })
    .await;
    match save_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(target: "acp.switch", "save after enable: {e}");
        }
        Err(join_err) => {
            tracing::error!(target: "acp.switch", "save task panicked after enable: {join_err}");
        }
    }

    // Spawn the structured view worker. If this fails the supervisor publishes
    // an AgentStartupError that the UI surfaces as the red banner; we
    // still return 200 because the view swap itself succeeded.
    // Container ensure runs inside the spawned task so the HTTP
    // response isn't held open through a docker pull/create.
    let cwd = std::path::PathBuf::from(&instance.project_path);
    let supervisor = state.acp_supervisor.clone();
    let session_id = id.clone();
    let model = instance.agent_model.clone();
    let stored_acp_session_id = instance.acp_session_id.clone();
    let yolo_mode = instance.yolo_mode;
    let profile_for_spawn = profile.clone();
    let command_override = crate::server::acp_reconciler::command_override_for_spawn(
        &instance.tool,
        &instance.command,
    );
    let state_for_spawn = state.clone();
    tokio::spawn(async move {
        let inst_lock = state_for_spawn.instance_lock(&session_id).await;
        let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
            &state_for_spawn.instances,
            &inst_lock,
            &session_id,
            false,
        )
        .await
        {
            Ok(info) => info,
            Err(e) => {
                let message = format!("container start failed: {e}");
                tracing::warn!(target: "acp.switch", session = %session_id, "container ensure failed: {e}");
                supervisor.publish_startup_error(&session_id, message);
                return;
            }
        };
        // Pass the session profile through regardless of sandboxing so the
        // spawn path resolves agent_acp_cmd and worker env from the right
        // profile for non-sandbox sessions too.
        let source_profile = Some(profile_for_spawn);
        if let Err(e) = supervisor
            .spawn(crate::acp::supervisor::SpawnRequest {
                session_id: session_id.clone(),
                agent: agent_name.clone(),
                cwd,
                additional_dirs: vec![],
                provider_env: vec![],
                model,
                effort: None,
                stored_acp_session_id,
                sandbox_info,
                source_profile,
                yolo_mode,
                agent_command_override: command_override,
            })
            .await
        {
            let message = format!("Failed to start structured view agent {agent_name:?}: {e}");
            tracing::warn!(target: "acp.switch", session = %session_id, "spawn after enable: {message}");
            supervisor.publish_startup_error(&session_id, message);
        }
    });

    Json(ViewSwitchResponse {
        session_id: id,
        view: crate::session::View::Structured,
    })
    .into_response()
}

/// Switch a structured view session back to tmux. Idempotent: a session that
/// is already tmux-mode returns 200 with no work done.
///
/// History is destroyed in the swap: the structured view conversation log
/// (still in the broadcast replay buffer) is dropped, and tmux comes
/// back with an empty pane that the agent fills as it runs.
pub async fn acp_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let (mut instance, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id).cloned() else {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        };
        let profile = inst.source_profile.clone();
        (inst, profile)
    };

    if !instance.is_structured() {
        return Json(ViewSwitchResponse {
            session_id: id,
            view: crate::session::View::Terminal,
        })
        .into_response();
    }

    // A real acp -> terminal transition is now committed (the idempotent
    // already-terminal case returned above). Tally the substrate toggle for the
    // opt-in telemetry snapshot.
    state
        .telemetry_structured
        .view_toggles
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Tear down the acp worker. Disabling acp mode discards the
    // conversation (we delete on-disk history and clear the stored ACP
    // id below), so release the agent's persisted transcript too via
    // session/delete. UnknownSession is fine, the supervisor may not
    // have a worker if startup never completed. See #1710.
    match state.acp_supervisor.shutdown_and_delete(&id).await {
        Ok(()) | Err(SupervisorError::UnknownSession(_)) => {}
        Err(e) => {
            tracing::warn!(target: "acp.switch", session = %id, "shutdown structured view failed: {e}");
        }
    }
    // Drop per-session bookkeeping so a future re-enable starts a
    // fresh conversation (seq counter from 1, empty replay buffer).
    // Without this, the next acp_enable's first event would
    // collide on a stale seq with the buffer entry from this
    // conversation, and the client-side dedupe would silently eat it.
    state.acp_supervisor.forget_session(&id);
    // Drop on-disk history so the next acp_enable starts truly
    // fresh — without this, the seq=1 first publish would collide
    // with a row already on disk and INSERT OR IGNORE would silently
    // drop it.
    state.acp_event_store.delete_session(&id);
    instance.view = crate::session::View::Terminal;
    // Clear the stored ACP session id: the agent's transcript is
    // tied to the structured view-mode lifecycle. If the user re-enables
    // structured view later, the agent should start a fresh session/new
    // rather than try to resume an id that's no longer relevant.
    if instance.acp_session_id.is_some() {
        tracing::debug!(
            target: "acp.switch",
            session = %id,
            "clearing acp_session_id on disable"
        );
        instance.acp_session_id = None;
    }

    // Persist + start tmux. start() now no longer short-circuits for
    // structured_view, so it will create a fresh tmux session and run
    // the agent CLI in the pane.
    //
    // The on-disk and in-memory updates mutate ONLY the structured view-specific
    // fields (`structured_view = false`, `acp_session_id = None`).
    // Wholesale replacement with a pre-lock snapshot would clobber
    // concurrent writes to other fields made by the status poll loop or
    // other handlers between the snapshot and the lock acquisition.
    {
        let mut instances = state.instances.write().await;
        if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
            slot.view = crate::session::View::Terminal;
            slot.acp_session_id = None;
        }
    }
    let id_for_save = id.clone();
    let profile_for_save = profile.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::new(&profile_for_save)?;
        storage.update(|all, _groups| {
            if let Some(slot) = all.iter_mut().find(|i| i.id == id_for_save) {
                slot.view = crate::session::View::Terminal;
                slot.acp_session_id = None;
            }
            Ok(())
        })?;
        Ok(())
    })
    .await;
    match save_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(target: "acp.switch", "save after disable: {e}");
        }
        Err(join_err) => {
            tracing::error!(target: "acp.switch", "save task panicked after disable: {join_err}");
        }
    }

    let start_result = tokio::task::spawn_blocking(move || instance.start()).await;
    match start_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(target: "acp.switch", session = %id, "tmux start after disable: {e}");
        }
        Err(e) => {
            tracing::error!(target: "acp.switch", session = %id, "spawn_blocking failed: {e}");
        }
    }

    Json(ViewSwitchResponse {
        session_id: id,
        view: crate::session::View::Terminal,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    pub mode_id: String,
}

/// Set the active session mode (Default / Plan / AcceptEdits /
/// BypassPermissions). Sends an ACP `session/set_mode` request.
pub async fn acp_set_mode(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetModeRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    match state.acp_supervisor.set_mode(&id, &req.mode_id).await {
        Ok(()) => {
            // The agent accepted the mode switch. "plan" is the canonical ACP
            // mode id; tally plan-mode adoption for the opt-in telemetry
            // snapshot. Other modes are out of scope for now.
            if req.mode_id == "plan" {
                state
                    .telemetry_structured
                    .plan_mode_seen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(SupervisorError::UnknownSession(_)) => {
            (StatusCode::NOT_FOUND, "session has no running acp").into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set_mode failed: {e}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetConfigOptionRequest {
    pub config_id: String,
    pub value: String,
}

/// Set a per-session selector (model, reasoning effort, etc.) via ACP
/// `session/set_config_option`. The structured view treats every category
/// through this one endpoint; rejection surfaces as a non-blocking
/// `Event::ConfigOptionSwitchFailed` notice on the broadcast bus. See
/// #1403.
pub async fn acp_set_config_option(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetConfigOptionRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    match state
        .acp_supervisor
        .set_config_option(&id, &req.config_id, &req.value)
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(SupervisorError::UnknownSession(_)) => (
            StatusCode::NOT_FOUND,
            "session has no running structured view",
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set_config_option failed: {e}"),
        )
            .into_response(),
    }
}

pub async fn resolve_approval(
    State(state): State<Arc<AppState>>,
    Path((id, nonce_str)): Path<(String, String)>,
    req: Result<Json<ResolveApprovalRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let nonce = Nonce(nonce_str);
    let decision = req.decision;
    match state
        .acp_supervisor
        .resolve_permission(&id, nonce, decision.into())
        .await
    {
        Ok(()) => {
            record_approval_decision(&state, decision);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(SupervisorError::UnknownSession(_)) => {
            (StatusCode::NOT_FOUND, "session has no running acp").into_response()
        }
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::UnknownNonce)) => {
            (StatusCode::NOT_FOUND, "no pending approval with that nonce").into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("resolve failed: {e}"),
        )
            .into_response(),
    }
}

/// Tally a user-resolved approval for the opt-in telemetry snapshot. Only the
/// three real user decisions are counted; the synthetic daemon-restart
/// `Cancelled` decision is not a user choice and never reaches this endpoint,
/// but is matched explicitly so adding a wire variant is a compile error here.
fn record_approval_decision(state: &AppState, decision: ApprovalDecisionWire) {
    use std::sync::atomic::Ordering::Relaxed;
    let counter = match decision {
        ApprovalDecisionWire::Allow => &state.telemetry_structured.approvals_allow,
        ApprovalDecisionWire::AllowAlways => &state.telemetry_structured.approvals_allow_always,
        ApprovalDecisionWire::Deny => &state.telemetry_structured.approvals_deny,
        ApprovalDecisionWire::Cancelled => return,
    };
    counter.fetch_add(1, Relaxed);
}

/// Build a markdown context primer from the persisted acp event
/// log. Used after a `session/load` failure: the agent's model
/// context is empty, but the visible transcript is intact in SQLite,
/// so the user can opt in to sending a compact recap as their next
/// prompt. See #1004.
pub async fn acp_context_primer(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ContextPrimerQuery>,
) -> impl IntoResponse {
    let events = state.acp_event_store.replay_before(&id, q.before_seq);
    let primer = crate::acp::context_primer::build_context_primer(
        &events,
        crate::acp::context_primer::PrimerOptions {
            before_seq: Some(q.before_seq),
            ..Default::default()
        },
    );
    Json(ContextPrimerResponse {
        primer: primer.text,
        included_event_count: primer.included_event_count,
        included_turn_count: primer.included_turn_count,
        truncated: primer.truncated,
        max_chars: primer.max_chars,
        unprocessed_prompt: primer.unprocessed_prompt,
    })
    .into_response()
}

/// Reconnect/snapshot endpoint. Mobile clients drop their WebSocket
/// briefly any time a screen lock fires; this lets them resync without
/// a full page reload by replaying the buffered frames they missed.
///
/// Gating note: only the standard auth middleware applies. History is
/// read-only and contains nothing the live channel didn't already
/// broadcast, so there is nothing extra to gate per request.
pub async fn acp_replay(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReplayQuery>,
) -> impl IntoResponse {
    // Reads from the disk-backed event store so reload, session-switch,
    // and `aoe serve` restart all reconstruct the full conversation
    // (subject to the per-session retention cap). The in-memory replay
    // buffer is still consulted on WS connect for the hot path; this
    // endpoint backstops that when the in-memory ring is cold (server
    // just restarted) or the client lagged far enough to need older
    // events than the ring holds.
    // Bound the page so neither the daemon nor the response scales with
    // total history. Omitted `limit` falls back to the default rather
    // than unbounded, so a naive or older client can't make the daemon
    // buffer an entire long session. All in-repo consumers paginate via
    // `has_more`/`next_cursor`.
    let limit = q
        .limit
        .map(|l| l as usize)
        .unwrap_or(DEFAULT_REPLAY_PAGE)
        .clamp(1, MAX_REPLAY_PAGE);
    // One store call returns the page and its `highest_seq`/`lowest_seq`
    // under a single lock, so the response is a consistent snapshot and a
    // concurrent `record()` can't desync the cap from the page rows.
    let page = state.acp_event_store.replay_page(&id, q.since, Some(limit));
    let highest_seq = page.highest_seq;
    let lowest_seq = page.lowest_seq;
    let next_cursor = page.last_scanned_seq;
    let has_more = page.has_more;
    let frames: Vec<crate::server::AcpBroadcastFrame> = page
        .events
        .into_iter()
        .map(|(seq, event)| crate::server::AcpBroadcastFrame {
            session_id: id.clone(),
            seq,
            event: Arc::new(event),
        })
        .collect();
    // `lost = true` when the client's `since` cursor predates the oldest
    // seq still on disk. The retention cap can evict older events, so a
    // client that returns after a long absence may legitimately need a
    // full reload. With no events on disk yet, nothing is lost. Computed
    // per request so a mid-loop prune is caught on whatever page first
    // sees the gap, not only the first.
    let lost = match lowest_seq {
        Some(lo) => q.since < lo.saturating_sub(1),
        None => false,
    };
    Json(ReplayResponse {
        frames,
        lost,
        highest_seq,
        lowest_seq,
        next_cursor,
        has_more,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn mime_allowlist_gates_by_kind() {
        assert!(mime_allowed(PromptAttachmentKind::Image, "image/png"));
        assert!(mime_allowed(PromptAttachmentKind::Image, "image/webp"));
        // SVG is excluded on purpose (scriptable XML).
        assert!(!mime_allowed(PromptAttachmentKind::Image, "image/svg+xml"));
        // Cross-kind MIME is rejected.
        assert!(!mime_allowed(PromptAttachmentKind::Image, "audio/mpeg"));
        assert!(mime_allowed(PromptAttachmentKind::Audio, "audio/mpeg"));
        assert!(mime_allowed(
            PromptAttachmentKind::Resource,
            "application/pdf"
        ));
        assert!(!mime_allowed(PromptAttachmentKind::Resource, "text/html"));
    }

    #[test]
    fn image_magic_bytes_sniff() {
        assert_eq!(
            sniff_image_mime(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_mime(b"GIF89a....."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_image_mime(&webp), Some("image/webp"));
        // A text blob mislabeled as PNG must not pass.
        assert_eq!(sniff_image_mime(b"<svg>not an image</svg>"), None);
        assert_eq!(sniff_image_mime(b""), None);
    }

    #[test]
    fn image_magic_bytes_predicate() {
        assert!(sniff_image_mime(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]).is_some());
        assert!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]).is_some());
        assert!(sniff_image_mime(b"GIF89a.....").is_some());
        assert!(sniff_image_mime(b"<svg>not an image</svg>").is_none());
        assert!(sniff_image_mime(b"").is_none());
    }

    #[test]
    fn read_log_tail_missing_file_returns_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.log");
        let (lines, truncated, exists) = read_log_tail(&path, 100).unwrap();
        assert!(lines.is_empty());
        assert!(!truncated);
        assert!(!exists);
    }

    #[test]
    fn read_log_tail_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..10 {
            writeln!(f, "line {i}").unwrap();
        }
        drop(f);
        let (lines, truncated, exists) = read_log_tail(&path, 3).unwrap();
        assert_eq!(lines, vec!["line 7", "line 8", "line 9"]);
        assert!(!truncated);
        assert!(exists);
    }

    #[test]
    fn read_log_tail_tail_larger_than_file_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.log");
        std::fs::write(&path, "only\nthree\nlines\n").unwrap();
        let (lines, _, exists) = read_log_tail(&path, 999).unwrap();
        assert_eq!(lines, vec!["only", "three", "lines"]);
        assert!(exists);
    }

    #[test]
    fn read_log_tail_keeps_first_line_when_window_starts_on_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aligned.log");
        let mut f = std::fs::File::create(&path).unwrap();
        let big_line = "x".repeat((WORKER_LOG_MAX_READ_BYTES as usize) - 1);
        writeln!(f, "{big_line}").unwrap();
        writeln!(f, "first whole line").unwrap();
        writeln!(f, "second whole line").unwrap();
        drop(f);
        let (lines, truncated, exists) = read_log_tail(&path, 10).unwrap();
        assert!(truncated);
        assert!(exists);
        assert_eq!(lines.first().map(String::as_str), Some("first whole line"));
        assert_eq!(lines.last().map(String::as_str), Some("second whole line"));
    }

    #[test]
    fn read_log_tail_drops_partial_first_line_when_window_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut f = std::fs::File::create(&path).unwrap();
        let big_line = "x".repeat((WORKER_LOG_MAX_READ_BYTES as usize) + 64);
        writeln!(f, "{big_line}").unwrap();
        writeln!(f, "real first").unwrap();
        writeln!(f, "real second").unwrap();
        drop(f);
        let (lines, truncated, exists) = read_log_tail(&path, 10).unwrap();
        assert!(truncated);
        assert!(exists);
        assert_eq!(lines.last().map(String::as_str), Some("real second"));
        assert!(!lines.iter().any(|l| l == &big_line));
    }

    #[test]
    fn list_files_returns_sorted_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.rs"), "").unwrap();

        let (files, truncated) = list_files(dir.path(), 5000).unwrap();
        assert_eq!(files, vec!["a.rs", "b.rs", "sub/c.rs"]);
        assert!(!truncated);
    }

    #[test]
    fn list_files_skips_vcs_build_and_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("real.rs"), "").unwrap();
        // Dotfiles are skipped at every level, not just the top, so a
        // nested dotfile must be dropped while its sibling is kept.
        std::fs::write(dir.path().join("sub").join(".env"), "").unwrap();
        for skip in [".git", "node_modules", "target"] {
            std::fs::create_dir(dir.path().join(skip)).unwrap();
            std::fs::write(dir.path().join(skip).join("junk"), "").unwrap();
        }

        let (files, _) = list_files(dir.path(), 5000).unwrap();
        assert_eq!(files, vec!["keep.rs", "sub/real.rs"]);
    }

    #[test]
    fn list_files_reports_truncation_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "").unwrap();
        }
        // Per-directory sorting makes the truncated subset deterministic,
        // so we can pin the exact files, not just the count.
        let (files, truncated) = list_files(dir.path(), 3).unwrap();
        assert!(truncated);
        assert_eq!(files, vec!["f0.rs", "f1.rs", "f2.rs"]);
    }
}
