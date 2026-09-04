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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::acp::acp_client::AcpError;
use crate::acp::approvals::Nonce;
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::event_store::AttachmentBlob;
use crate::acp::protocol::{
    ApprovalDecisionWire, ContextPrimerQuery, ContextPrimerResponse, DiffCommentsPromptRequest,
    FilesResponse, PromptAttachmentUpload, PromptRequest, ReplayQuery, ReplayResponse,
    ResolveApprovalRequest, SwitchAgentRequest, SwitchAgentResponse,
};
use crate::acp::state::{Event, PromptAttachmentKind, RateLimitInfo};
use crate::acp::supervisor::SupervisorError;
use crate::server::session_service::{SendTurnError, SessionCaller};
use crate::server::AppState;

/// Maximum attachments per prompt.
const MAX_ATTACHMENTS: usize = 8;
/// Maximum decoded size of a single attachment (10 MiB).
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Maximum decoded size of all attachments on one prompt (20 MiB).
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Startup-error banner text for a failed detached structured-view spawn,
/// shared by the create-path (`create_session`) and enable-path
/// (`acp_enable`). `CapacityFull` is transient and user-actionable, so its
/// Display (which carries "capacity full" + "max_concurrent_workers", matching
/// the front-end capacity regex) is surfaced verbatim instead of the generic
/// crash-style message, so the session shows the capacity banner rather than a
/// misleading failure. The detached task runs before the reconciler sees the
/// session, so on `CapacityFull` the first reconciler tick re-publishes the
/// same banner once via its `capacity_deferred` gate: a single benign
/// duplicate the front-end reducer collapses idempotently. See #1027.
pub(crate) fn structured_spawn_error_message(err: &SupervisorError, agent: &str) -> String {
    match err {
        SupervisorError::CapacityFull { .. } => err.to_string(),
        _ => format!("Failed to start structured view agent {agent:?}: {err}"),
    }
}

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
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
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
pub(crate) fn validate_attachments(
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

fn rate_limit_resume_marker_resets_at(
    latest_status: Option<&Event>,
    latest_rate_limit: Option<&RateLimitInfo>,
    fallback_resets_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // #3688: an exhausted-retries park also continues the interrupted turn
    // on manual resume. No schedule applies (the reconciler already gave
    // up), so the marker's timestamp is only the resume-at instant the
    // breadcrumb reports.
    match latest_status {
        Some(Event::Stopped { reason }) if reason == "rate_limited" => Some(
            latest_rate_limit
                .and_then(|info| info.resets_at)
                .unwrap_or(fallback_resets_at),
        ),
        Some(Event::Stopped { reason })
            if reason == crate::server::acp_reconciler::RATE_LIMIT_EXHAUSTED_RETRIES_REASON =>
        {
            Some(fallback_resets_at)
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
pub struct SpawnAcpRequest {
    /// Optional override; falls back to `Supervisor::pick_agent_for_tool`.
    pub agent: Option<String>,
    /// Optional model override; forwarded to the agent as
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
    state.read_only.then(super::read_only_response)
}

/// Canonical status + body mapping for a `SupervisorError`. Bodies stay
/// plain text: every dashboard consumer of these endpoints reads the raw
/// body for display (`safeText` in `useAcpSession.ts`), and the behavioral
/// discriminators (the `worker_not_ready` prefix, nonce-echoing 404s) are
/// text matches. Context-specific overrides (retryable `worker_not_ready`,
/// nonce echoes) live as explicit pre-match arms at their call sites.
/// `context` prefixes the fallback 500 body so "prompt failed" vs "cancel
/// failed" stays distinguishable in banners and logs.
fn supervisor_error_response(context: &str, err: &SupervisorError) -> axum::response::Response {
    match err {
        SupervisorError::UnknownSession(_) => (
            StatusCode::NOT_FOUND,
            "session has no running structured view",
        )
            .into_response(),
        SupervisorError::UnknownAgent(name) => (
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {name}"),
        )
            .into_response(),
        // 403, not 400: the agent exists and is spelled correctly, the
        // operator's policy refuses it. A 400 would send the user looking for a
        // typo or a missing binary. See #3241.
        SupervisorError::AgentNotAllowed(_) => {
            (StatusCode::FORBIDDEN, err.to_string()).into_response()
        }
        SupervisorError::AlreadyRunning(_) => (
            StatusCode::CONFLICT,
            "structured view worker already running for session",
        )
            .into_response(),
        SupervisorError::CapacityFull { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string()).into_response()
        }
        // 503, not 500: the worker's connection task has ended but its
        // handle is still in the map, so the command send fails
        // instantly. That is the teardown window a user-initiated force
        // stop opens (kill the process group, respawn with
        // `session/load`), and the worker is on its way back, so the
        // request lost a race with a restart the user asked for rather
        // than hitting a fault. The `worker_not_ready` prefix is the
        // marker the structured view already treats as transient: it
        // re-queues and re-fires instead of retiring the prompt and
        // banner-ing the user. See #3401.
        SupervisorError::Acp(AcpError::AgentExited | AcpError::NotRunning) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_not_ready: {context}: {err}"),
        )
            .into_response(),
        SupervisorError::Acp(_)
        | SupervisorError::InvalidAgentCommand(_)
        | SupervisorError::SpawnCancelled(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{context}: {err}"),
        )
            .into_response(),
    }
}

fn not_structured_response() -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "not_structured",
            "message": "Switch the session to structured view before starting an ACP worker",
        })),
    )
        .into_response()
}

pub async fn spawn_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SpawnAcpRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Manual worker spawn is an admin/power operation with no composer surface.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    {
        let instances = state.instances.read().await;
        if !instances.iter().any(|i| i.id == id) {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        }
    }
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;
    let instance = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
            return (StatusCode::NOT_FOUND, "session not found").into_response();
        };
        if !instance.is_structured() {
            return not_structured_response();
        }
        instance
    };

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
    let effort = instance.acp_effort.clone();
    let stored_acp_session_id = instance.acp_session_id.clone();
    let yolo_mode = instance.yolo_mode;
    let acp_mode_id = instance.acp_mode_id.clone();
    // #2276: seed the transcript from the session/load replay when importing
    // an existing Claude session (import_pending set, empty store). The
    // supervisor clears any partial replay from a prior attempt after it
    // reserves the worker slot, so we only pass the flag here.
    let seed_history_replay = instance.import_pending == Some(true);
    // Structured fork: when set, the handshake sends session/fork against this
    // parent id instead of session/new. Cleared once the forked id lands.
    let fork_from = instance.fork_pending.clone();

    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session_locked(
        &state.instances,
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

    let rate_limit_resume_resets_at = {
        let store = Arc::clone(&state.acp_event_store);
        let id_for_probe = id.clone();
        tokio::task::spawn_blocking(move || {
            let latest_status = store.latest_status_event(&id_for_probe);
            let latest_rate_limit = store
                .latest_rate_limit_event(&id_for_probe)
                .map(|(info, _created_at)| info);
            rate_limit_resume_marker_resets_at(
                latest_status.as_ref(),
                latest_rate_limit.as_ref(),
                Utc::now(),
            )
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(target: "http.api.acp", session = %id, "rate-limit resume probe failed: {e}");
            None
        })
    };

    let spawn_result = state
        .acp_supervisor
        .spawn(crate::acp::supervisor::SpawnRequest {
            session_id: id.clone(),
            agent,
            tool: instance.tool.clone(),
            cwd,
            additional_dirs: req.additional_dirs,
            provider_env,
            model,
            effort,
            stored_acp_session_id,
            fork_from,
            sandbox_info,
            source_profile,
            yolo_mode,
            acp_mode_id,
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &instance.tool,
                &instance.command,
            ),
            seed_history_replay,
        })
        .await;

    match spawn_result {
        Ok(()) => {
            if let Some(resets_at) = rate_limit_resume_resets_at {
                // Continue the rate-limit-interrupted turn instead of leaving
                // the resumed agent idle; the pending-turn drain delivers it
                // once the worker is live (#3028).
                crate::server::acp_reconciler::enqueue_rate_limit_continuation(&state, &id).await;
                state
                    .acp_supervisor
                    .publish_rate_limit_auto_resumed(&id, resets_at, true);
            }
            Json(SpawnAcpResponse {
                session_id: id,
                agent: agent_for_response,
                status: "running",
            })
            .into_response()
        }
        Err(SupervisorError::AlreadyRunning(_)) if rate_limit_resume_resets_at.is_some() => {
            if let Some(resets_at) = rate_limit_resume_resets_at {
                crate::server::acp_reconciler::enqueue_rate_limit_continuation(&state, &id).await;
                state
                    .acp_supervisor
                    .publish_rate_limit_auto_resumed(&id, resets_at, true);
            }
            Json(SpawnAcpResponse {
                session_id: id,
                agent: agent_for_response,
                status: "running",
            })
            .into_response()
        }
        Err(e) => supervisor_error_response("spawn failed", &e),
    }
}

/// Process-wide guard so concurrent `npm install -g` runs (across any
/// sessions) never race the daemon user's shared global npm prefix. See #2109.
static INSTALL_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Cap per stream on the npm output echoed back to the web. `npm install -g`
/// runs arbitrary lifecycle scripts; a verbose or hostile package could emit
/// huge output, so truncate what we serialize and mark it. See #2109.
const MAX_INSTALL_LOG_BYTES: usize = 64 * 1024;

fn truncate_install_log(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    if text.len() <= MAX_INSTALL_LOG_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_INSTALL_LOG_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n... [truncated, output exceeded {MAX_INSTALL_LOG_BYTES} bytes]",
        &text[..cut]
    )
}

/// Result of a server-run agent install. The "& restart" half is the
/// client's job: on `success` the web re-POSTs `/acp/spawn` (the same
/// respawn path the Restart button uses), so this endpoint stays a pure
/// install with no server-side respawn duplication. See #2109.
#[derive(Serialize)]
pub struct InstallAgentResponse {
    pub session_id: String,
    pub package: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// How many *other* sessions parked on the same adapter's compatibility
    /// rejection were queued for an automatic respawn on the freshly
    /// installed version. The install is global, so one click clears every
    /// red X. The current session is excluded (the client respawns it
    /// directly). See #2109.
    pub recovered_sessions: usize,
}

/// How long a single `npm install -g` (host or in-container) may run before
/// it is killed and the held install lock released.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Run `npm install -g <package>` on the daemon host. Returns the process
/// output on completion, or a ready-to-send error response for the failure
/// modes (`npm` absent, spawn failure, timeout).
async fn install_on_host(package: &str) -> Result<std::process::Output, axum::response::Response> {
    let Ok(npm) = which::which("npm") else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "npm_missing",
                "message": "`npm` is not on the daemon's PATH. Start `aoe serve` from a shell where `which npm` resolves.",
            })),
        )
            .into_response());
    };

    // Bound the install so a network stall or a wedged lifecycle script
    // cannot hang the request (and the held lock) forever. kill_on_drop
    // reaps the child if the timeout fires.
    match tokio::time::timeout(
        INSTALL_TIMEOUT,
        tokio::process::Command::new(&npm)
            .arg("install")
            .arg("-g")
            .arg(package)
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "npm_start_failed",
                "message": format!("npm install failed to start: {e}"),
            })),
        )
            .into_response()),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": "install_timeout",
                "message": "`npm install -g` did not finish within 180s.",
            })),
        )
            .into_response()),
    }
}

/// Run `npm install -g <package>` inside the session's sandbox container via
/// `<runtime> exec`. The container must be running for `exec` to land, so a
/// stopped/absent container returns a targeted error rather than a cryptic
/// runtime one. Mirrors [`install_on_host`]'s fixed-argv, no-shell, bounded
/// execution.
async fn install_in_container(
    session_id: &str,
    package: &str,
) -> Result<std::process::Output, axum::response::Response> {
    use crate::containers::DockerContainer;

    let sid = session_id.to_string();
    let running = tokio::task::spawn_blocking(move || {
        DockerContainer::from_session_id(&sid)
            .is_running()
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if !running {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "container_not_running",
                "message": "The session's sandbox container is not running. Open the session (which starts its container) and try again.",
            })),
        )
            .into_response());
    }

    let container_name = DockerContainer::generate_name(session_id);
    let runtime_bin = crate::containers::runtime_binary();
    match tokio::time::timeout(
        INSTALL_TIMEOUT,
        tokio::process::Command::new(runtime_bin)
            .arg("exec")
            .arg(&container_name)
            .arg("npm")
            .arg("install")
            .arg("-g")
            .arg(package)
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "exec_start_failed",
                "message": format!("`{runtime_bin} exec` failed to start: {e}"),
            })),
        )
            .into_response()),
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": "install_timeout",
                "message": "`npm install -g` in the sandbox did not finish within 180s.",
            })),
        )
            .into_response()),
    }
}

/// `POST /api/sessions/{id}/acp/install-agent`: run `npm install -g <pkg>`
/// for the session's agent where the adapter actually lives, then let the
/// client respawn.
///
/// A host-run session installs on the host; a sandboxed session installs
/// *inside its container* (`<runtime> exec … npm install -g`), because a
/// host `npm install -g` never reaches the containerized adapter. This is
/// the recovery path when a cached `aoe-sandbox` image ships a
/// `claude-agent-acp` below the current floor: the in-container install
/// lifts the running container's adapter without recreating it. The image
/// itself stays stale for *future* containers until refreshed; the
/// structured-view error screen points sandboxed users at that follow-up.
///
/// Hardened, opt-in (Tier 2 of #2109): blocked in read-only mode; gated on
/// the `acp.allow_agent_install` setting (default off, `local_only`); the
/// package is resolved server-side from the session's agent via a static
/// npm-only table, never from client input; npm runs with fixed argv and no
/// shell; the per-session instance lock serializes installs so a
/// double-click cannot race the global npm prefix (host) or the container.
pub async fn install_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Installing agent binaries is an admin operation, not a composer action.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    if !crate::session::Config::load_or_warn()
        .acp
        .allow_agent_install
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "install_disabled",
                "message": "Installing agents from the web is off. Enable acp.allow_agent_install (Settings, local only).",
            })),
        )
            .into_response();
    }

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    drop(instances);

    // Resolve the binary the session would spawn, then its npm package.
    let agent = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            instance.agent_name.as_deref(),
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;
    let binary = match state.acp_supervisor.resolve_agent(&agent).await {
        Ok(spec) => spec.command,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "agent_resolve_failed",
                    "message": format!("could not resolve agent `{agent}`: {e}"),
                })),
            )
                .into_response();
        }
    };
    let Some(package) = crate::acp::install_hints::npm_package_for(&binary) else {
        let hint =
            crate::acp::install_hints::install_hint_for(&binary).unwrap_or("(see project docs)");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "not_npm_installable",
                "message": format!("`{binary}` cannot be installed via npm from the daemon. Install it manually: {hint}"),
                "install_command": hint,
            })),
        )
            .into_response();
    };

    // `npm install -g` mutates the daemon user's shared global prefix, so two
    // *different* sessions installing at once would race. Serialize all
    // installs process-wide, and also hold the per-session lock so a same-
    // session spawn cannot run mid-install. `instance_lock` returns the lock
    // handle; hold the guard across the install.
    let _install_guard = INSTALL_LOCK.lock().await;
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    // Run the install where the adapter actually lives: on the host for a
    // host-run session, inside the container for a sandboxed one.
    let output = if instance.is_sandboxed() {
        match install_in_container(&id, package).await {
            Ok(output) => output,
            Err(resp) => return resp,
        }
    } else {
        match install_on_host(package).await {
            Ok(output) => output,
            Err(resp) => return resp,
        }
    };

    // On success a *host* install updates the global npm prefix shared by
    // every host-run session, so others parked on the same binary's
    // compatibility check can recover too; queue them for the reconciler to
    // fresh-spawn (the current session is excluded because the client
    // respawns it directly and a double-spawn would race). A *container*
    // install only touched this session's container, so no cross-session
    // recovery applies. See #2109.
    let mut recovered_sessions = 0;
    if output.status.success() && !instance.is_sandboxed() {
        for other in state
            .acp_supervisor
            .incompatible_sessions_for_binary(&binary)
            .await
        {
            if other == id {
                continue;
            }
            state.acp_supervisor.request_respawn(&other);
            recovered_sessions += 1;
        }
    }

    Json(InstallAgentResponse {
        session_id: id,
        package: package.to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: truncate_install_log(&output.stdout),
        stderr: truncate_install_log(&output.stderr),
        recovered_sessions,
    })
    .into_response()
}

pub async fn shutdown_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Worker shutdown is a lifecycle/admin action with no composer surface.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    // Worker-stopping barrier: submission guard before any teardown, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    match state.acp_supervisor.shutdown(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => supervisor_error_response("shutdown failed", &e),
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
    /// Registry lifecycle state, same contract as `/api/agents`: omitted
    /// while Active so existing consumers see no change. Lets the
    /// switch-agent modal label deprecated backends without depending on
    /// the static frontend mirror being current.
    #[serde(skip_serializing_if = "crate::agents::AgentLifecycle::is_active")]
    pub lifecycle: crate::agents::AgentLifecycle,
}

/// `GET /api/acp/agents`: list the built-in ACP registry entries the
/// supervisor knows about. Distinct from `/api/agents` (which lists
/// session-tool agents like claude/codex/cursor for the wizard);
/// this returns the *structured view* ACP backend registry so the recovery
/// modal can show what the user can hand off to. Custom `agent_acp_cmd`
/// agents are intentionally not included here. See #1282.
///
/// Filtered by the operator agent allowlist (#3241), so the switch-agent modal
/// offers only what the policy permits instead of listing a target that
/// `/acp/switch-agent` would then refuse. This is UX, not the enforcement
/// boundary; the supervisor is (see `crate::acp::agent_policy`).
pub async fn list_acp_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let registry = state.acp_supervisor.registry_snapshot().await;
    let policy = super::agent_policy().await;
    Json(acp_agent_entries(&registry, &policy)).into_response()
}

/// The `GET /api/acp/agents` response body: permitted registry entries, sorted
/// by name. Split out from the handler so the policy filter is testable without
/// standing up an `AppState` and a config file on disk.
fn acp_agent_entries(
    registry: &crate::acp::AgentRegistry,
    policy: &crate::acp::agent_policy::AgentPolicy,
) -> Vec<AcpAgentInfo> {
    let mut entries: Vec<AcpAgentInfo> = registry
        .list()
        .into_iter()
        .filter(|(name, _)| policy.allows(name))
        .map(|(name, spec)| AcpAgentInfo {
            name: name.clone(),
            description: spec.description.clone(),
            command: spec.command.clone(),
            lifecycle: crate::agents::registry_lifecycle(name),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// `GET /api/acp/option-catalog`: the recall cache of `config_options` each
/// agent last advertised (model / mode / thinking choices), keyed by agent
/// name. The per-agent defaults settings page reads this so its dropdowns can
/// be populated without a live session. Empty until an agent has run at least
/// once. See #2631.
pub async fn get_option_catalog() -> impl IntoResponse {
    let catalog = tokio::task::spawn_blocking(crate::acp::option_catalog::load)
        .await
        .unwrap_or_default();
    Json(catalog).into_response()
}

/// Atomically move a structured view session from one ACP backend to another.
/// Two callers drive this: the rate-limit recovery flow (#1282), which
/// hands a Claude-rate-limited session off to `codex` (or another
/// installed backend), and explicit user-initiated switches from the
/// composer control or `aoe acp switch-agent`. Both keep the
/// transcript; only the recorded `reason` differs.
///
/// Sequence:
///   1. Look up the instance, then validate `target` is a built-in
///      registry agent or a configured custom ACP agent for the
///      session's profile. Looking up the instance first is required
///      because custom agents are profile-specific; it also means a
///      non-existent session returns 404 before an unknown agent
///      returns 400.
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
    // CityHall sessions are pinned to their configured ACP agent; switching
    // agents (including to a non-ACP one) would break the locked-down mode.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }

    let target = req.target.trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target is required").into_response();
    }
    // Worker-stopping barrier: submission guard before any teardown, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };

    // Look up the instance first: custom agents are profile-specific, so
    // validation needs the session's source_profile and project_path. This
    // also means a non-existent session returns 404 before an unknown agent
    // returns 400, and a configured-but-disallowed custom agent returns 403
    // instead of 400.
    let instance = {
        let instances = state.instances.read().await;
        match instances.iter().find(|i| i.id == id).cloned() {
            Some(inst) => inst,
            None => return (StatusCode::NOT_FOUND, "session not found").into_response(),
        }
    };
    if !state
        .acp_supervisor
        .agent_is_valid_switch_target(
            &target,
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {target}"),
        )
            .into_response();
    }
    // Refuse a disallowed target here, before the shutdown below tears the
    // current worker down. The spawn choke point would refuse it anyway, but by
    // then the session has lost the worker it was happily using and the user is
    // left with nothing running. Same reason the validation check above is a
    // preflight. See #3241.
    if !super::agent_policy().await.allows(&target) {
        return (
            StatusCode::FORBIDDEN,
            SupervisorError::AgentNotAllowed(target).to_string(),
        )
            .into_response();
    }
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
            tool: instance.tool.clone(),
            cwd,
            additional_dirs: vec![],
            provider_env: vec![],
            model: model.clone(),
            // Effort vocabularies are adapter-specific ("high" on one adapter,
            // "xhigh" / "medium" naming on another), so the previous agent's
            // pick is meaningless here. The new agent starts on its configured
            // default and the persist below clears the stale value.
            effort: None,
            // Different ACP backend; the cached Claude session id would
            // be rejected by codex / opencode.
            stored_acp_session_id: None,
            // Switching ACP backend starts a fresh session, never a fork.
            fork_from: None,
            sandbox_info,
            source_profile,
            yolo_mode: instance.yolo_mode,
            acp_mode_id: instance.acp_mode_id.clone(),
            // Gated in the supervisor: only applies when the selected
            // agent equals the instance tool and its binary matches, so
            // an explicit switch to a different agent is unaffected.
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &instance.tool,
                &instance.command,
            ),
            // Switching ACP backend starts a fresh session, never an import.
            seed_history_replay: false,
        })
        .await;
    if let Err(e) = spawn_result {
        return supervisor_error_response("spawn failed", &e);
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
            // Switching backend abandons any pending import (#2276), else a
            // later spawn/reconciler pass treats this as an import and clears
            // the store before spawning.
            inst.import_pending = None;
            // Drop the old agent's effort pick: its vocabulary does not carry
            // to the new agent, so the session falls back to the new agent's
            // configured default until the user picks again.
            inst.acp_effort = None;
            if let Some(m) = &model {
                inst.agent_model = Some(m.clone());
            }
        }
    }
    match crate::session::Storage::new(&profile_for_save, state.file_watch.clone()) {
        Ok(storage) => {
            if let Err(e) = storage.update(|instances, _groups| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_save) {
                    inst.agent_name = Some(target_for_save.clone());
                    inst.acp_session_id = None;
                    inst.import_pending = None;
                    inst.acp_effort = None;
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
        Err(e) => {
            tracing::error!(
                target: "http.api.acp",
                session = %id_for_save,
                "failed to open storage to persist agent_name after switch: {e}"
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

/// Touches the row's `last_accessed_at` on every prompt (a prompt is a
/// user gesture, and structured rows have no other per-prompt recency
/// writer since the intent stamp was dropped for #3465), then wakes a
/// sunk row: archived_at/snoozed_until are cleared and an idle-dormant
/// worker respawns; the frontend's queue drains as soon as the fresh
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
async fn touch_on_prompt_and_wake_if_sunk(state: &Arc<AppState>, id: &str) -> bool {
    let inst_lock = state.instance_lock(id).await;
    let _guard = inst_lock.lock().await;
    let (found, wake, woke_idle_dormant) = {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            let was_idle_dormant = inst.is_idle_dormant();
            let wake = inst.is_archived() || inst.is_snoozed() || was_idle_dormant;
            // Memory side: a prompt is a user gesture, so refresh recency
            // and lift sinks unconditionally.
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
            (true, wake, was_idle_dormant)
        } else {
            (false, false, false)
        }
    };
    if found {
        let profile = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.source_profile.clone())
                .unwrap_or_default()
        };
        if let Ok(storage) = crate::session::Storage::new(&profile, state.file_watch.clone()) {
            let id_clone = id.to_string();
            let session_id_for_log = id.to_string();
            match tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) {
                        apply_prompt_persist_to_disk(inst, wake);
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
                    "failed to save after prompt touch and wake: {e}"
                ),
                Err(join_err) => tracing::warn!(
                    target: "http.api.acp",
                    session = %session_id_for_log,
                    "spawn_blocking join error during prompt touch save: {join_err}"
                ),
            }
        }
    }
    woke_idle_dormant
}

/// Disk-side half of [`touch_on_prompt_and_wake_if_sunk`], mirroring onto disk
/// whatever the memory side actually did. A waking prompt keeps touch semantics
/// and lifts the sink; any other prompt advances recency monotonically and
/// leaves sink state alone.
///
/// What the second tier buys, precisely: `wake` is computed from
/// `state.instances`, so a daemon whose memory predates a peer's archive would
/// otherwise clear a sink it has never observed. Tier 2 stops that, and only
/// that.
///
/// It does NOT make this save path safe against #3465's wipe, and it was a
/// mistake to claim otherwise. Advancing `last_accessed_at` on disk arms the
/// very signal the wipe keys on: `merge_user_action_diff` computes
/// `touched = self.last_accessed_at > pre.last_accessed_at`
/// (`session/instance/merge.rs`) and clears `archived_at` / `snoozed_until` /
/// `idle_dormant_since` when it holds, so a writer whose `pre` snapshot
/// predates this advance still loses its archive one hop later. That is the
/// documented invariant rather than a bug (a prompt is a real user gesture, and
/// a real touch is meant to dethrone a concurrent archive); what #3465 was
/// about is a *passive* stamp reaching the same arm with no gesture behind it.
fn apply_prompt_persist_to_disk(disk: &mut crate::session::Instance, wake: bool) {
    if wake {
        disk.touch_last_accessed();
    } else {
        let now = chrono::Utc::now();
        disk.last_accessed_at = disk.last_accessed_at.max(Some(now));
    }
}

/// `POST /api/sessions/{id}/acp/prompt` success body.
///
/// **Breaking**: this endpoint used to answer a bare 202 with no body. It now
/// reports what the daemon did with the prompt, because the daemon is what
/// decides (Tier 3). Flattened, so the wire shape is
/// `{"disposition":"sent"}` / `{"disposition":"steered"}` /
/// `{"disposition":"queued","reason":"cancelling","queued_id":"..."}`.
#[derive(Debug, serde::Serialize)]
pub struct PromptDispatchResponse {
    #[serde(flatten)]
    pub dispatch: crate::acp::dispatch::PromptDispatch,
    /// The queue row's id on a `queued` disposition, so a client reconciles its
    /// optimistic row the same way the transcript reconciles by row id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_id: Option<String>,
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
    let woke_idle_dormant = touch_on_prompt_and_wake_if_sunk(&state, &id).await;
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
    // Claim the session's prompt-submission authority and decide under it, so
    // every client follows the same send, steer, or queue rules and the
    // decision and the dispatch it picks are one atomic step. Releasing
    // between them let a queue drain read a fold this prompt had not published
    // into yet, so both delivered and whichever lost the agent's race came
    // back `agent_busy` after its queue row was already retired (#3621).
    // `woke_idle_dormant` is passed rather than re-read: the wake above
    // already cleared the marker, so the instance now says "awake".
    let Ok((_submission, dispatch)) = state
        .session_service
        .begin_prompt_submission(&SessionCaller::User, &id, woke_idle_dormant)
        .await
    else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    // A fresh user prompt supersedes any queued rate-limit resume
    // continuation, so drop it before sending: otherwise the reconciler could
    // later replay the older interrupted prompt after this newer one (#3028).
    // The pending-turn drain holds this same guard across its whole
    // snapshot -> reload -> send -> clear, so whichever side wins it, the
    // stale continuation can never be published after this newer prompt. If
    // the drain wins it delivers first; if we win, the drain then reads None
    // and returns. Resume + publish + forward live in the shared service so
    // the plugin host delivers turns through the same path (#2897).
    state.session_service.clear_pending_initial_turn(&id).await;
    if let crate::acp::dispatch::PromptDispatch::Queued { reason } = dispatch {
        // Park it on the server-owned queue the turn-end drain already
        // services. A client-minted id reconciles the caller's optimistic row;
        // without one the server mints the id it returns.
        let prompt_id = req
            .prompt_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        return match super::queue::buffer_and_enqueue(
            &state,
            &id,
            &prompt_id,
            req.text.clone(),
            &attachments,
            None,
            chrono::Utc::now().to_rfc3339(),
        )
        .await
        {
            Ok(entry) => (
                StatusCode::ACCEPTED,
                Json(PromptDispatchResponse {
                    dispatch,
                    queued_id: Some(entry.id),
                }),
            )
                .into_response(),
            Err((status, msg)) => {
                tracing::warn!(
                    target: "http.api.acp",
                    session = %id,
                    ?reason,
                    "prompt dispatch chose the queue but enqueue failed: {msg}"
                );
                (status, msg).into_response()
            }
        };
    }
    let outcome = state
        .session_service
        .send_turn(
            &SessionCaller::User,
            &id,
            &req.text,
            &attachments,
            woke_idle_dormant,
            req.prompt_id.clone(),
        )
        .await;
    // Smart-rename fires from `acp_event_listener` on the first clean
    // `prompt_complete` `Event::Stopped` (turn-end), so the one-shot never
    // races this handler's live worker for the provider API. See
    // `session::smart_rename` and #2348.
    match outcome {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(PromptDispatchResponse {
                dispatch,
                queued_id: None,
            }),
        )
            .into_response(),
        Err(SendTurnError::SessionNotFound) => {
            (StatusCode::NOT_FOUND, "session not found").into_response()
        }
        Err(SendTurnError::ResumeFailed(SupervisorError::CapacityFull { current, limit })) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_capacity_full ({current}/{limit})"),
        )
            .into_response(),
        Err(SendTurnError::ResumeFailed(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_not_ready: {e}"),
        )
            .into_response(),
        Err(SendTurnError::WorkerNotReady) => {
            (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
        }
        // Unreachable for a User caller; mapped defensively so the match
        // stays exhaustive as the service grows plugin-only failure modes.
        Err(SendTurnError::NotOwner) => {
            (StatusCode::FORBIDDEN, "session not owned by caller").into_response()
        }
        Err(SendTurnError::ModeApplication(e)) => {
            supervisor_error_response("mode application failed", &e)
        }
        Err(SendTurnError::Send(e)) => supervisor_error_response("prompt failed", &e),
    }
}

/// Why a diff-comments prompt cannot start a turn right now. Each reason is
/// transient, and the dialog keeps the user's comments and shows the text, so
/// the answer is "try again", not a lost review.
fn diff_comments_not_now(reason: crate::acp::dispatch::QueueReason) -> axum::response::Response {
    use crate::acp::dispatch::QueueReason;
    match reason {
        QueueReason::WorkerDown => {
            (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
        }
        QueueReason::TurnActive => (
            StatusCode::CONFLICT,
            "the agent is mid-turn; wait for it to finish, then send the comments again",
        )
            .into_response(),
        QueueReason::Cancelling => (
            StatusCode::CONFLICT,
            "the turn is still cancelling; send the comments again once it stops",
        )
            .into_response(),
        QueueReason::Compacting => (
            StatusCode::CONFLICT,
            "the agent is compacting its context; send the comments again when it finishes",
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
    let woke_idle_dormant = touch_on_prompt_and_wake_if_sunk(&state, &id).await;
    // This opens a turn (`UserDiffCommentsPrompt` folds to `turn_active`) just
    // as an ordinary prompt does, so it takes the same submission authority
    // and settles the same disposition under it (#3621, #3649).
    let Ok((_submission, dispatch)) = state
        .session_service
        .begin_prompt_submission(&SessionCaller::User, &id, woke_idle_dormant)
        .await
    else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    // A typed diff-comments prompt has no queue row to park on, so anything
    // but send-or-steer is refused here rather than published and then
    // rejected asynchronously as `agent_busy`, which would strand the card in
    // the transcript and lose the review the user just wrote.
    if let crate::acp::dispatch::PromptDispatch::Queued { reason } = dispatch {
        return diff_comments_not_now(reason);
    }
    // Wake a worker that is not live: the idle-dormant reap (#1748) and the
    // rate-limit redelivery-cap park (#3688) are both sendable above with no
    // worker, and the dispatch already refused a cold session that is neither.
    // Respawn synchronously-reserved + detached so the send_prompt below waits
    // for the worker instead of 404ing. Mirrors `send_turn`'s `needs_resume`.
    let needs_resume = woke_idle_dormant || !state.acp_supervisor.is_running(&id).await;
    if needs_resume {
        use crate::server::acp_reconciler::ResumeTrigger;
        match crate::server::acp_reconciler::trigger_resume_background(&state.session_service, &id)
            .await
        {
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
    // Gate the publish on the worker actually being there, as `send_turn`
    // does: a resume that never finishes would otherwise leave a
    // `UserDiffCommentsPrompt` with no turn behind it, stranding the review
    // card on "running" (#3172). A live worker makes this a map lookup.
    if let Err(e) = state.acp_supervisor.wait_until_ready(&id).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_not_ready: {e}"),
        )
            .into_response();
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
        // Retryable worker_not_ready override; mirrors acp_prompt. See #1748.
        Err(SupervisorError::UnknownSession(_)) if needs_resume => {
            (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
        }
        Err(e) => supervisor_error_response("prompt failed", &e),
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
        Err(e) => supervisor_error_response("cancel failed", &e),
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
    // Enumerates the workspace tree for the Files pane, which CityHall hides.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
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
    // Raw worker logs are a debug surface, not part of the composer.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let instances = state.instances.read().await;
    let session_known = instances.iter().any(|i| i.id == id);
    drop(instances);
    if !session_known {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }

    let log_path = match crate::process::worker_registry::log_path_for(&id) {
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
/// How a structured-view spawn seeds its transcript when the view is enabled.
struct StructuredSeed {
    stored_acp_session_id: Option<String>,
    seed_history_replay: bool,
}

/// Decide the seed for an `acp_enable` spawn.
///
/// - An existing `acp_session_id` (a prior structured session, or a #2276
///   import) is loaded; history replay is seeded only when `import_pending`.
/// - Otherwise, #2252 direction B: a claude terminal session whose resumable
///   transcript sits in `agent_session_id` is carried into a seeded
///   `session/load`, so switching a terminal claude session into structured
///   view continues the same conversation. `transcript_present` gates this so a
///   stale id does not hard-fail the seeded spawn.
/// - Failing both, the spawn starts a fresh `session/new`.
fn resolve_structured_seed(
    tool: &str,
    acp_agent: &str,
    acp_session_id: Option<&str>,
    agent_session_id: Option<&str>,
    import_pending: bool,
    transcript_present: bool,
) -> StructuredSeed {
    if let Some(id) = acp_session_id {
        return StructuredSeed {
            stored_acp_session_id: Some(id.to_string()),
            seed_history_replay: import_pending,
        };
    }
    if crate::agents::acp_transcript_cli_resumable(tool, acp_agent) && transcript_present {
        if let Some(id) = agent_session_id.filter(|s| !s.trim().is_empty()) {
            return StructuredSeed {
                stored_acp_session_id: Some(id.to_string()),
                seed_history_replay: true,
            };
        }
    }
    StructuredSeed {
        stored_acp_session_id: None,
        seed_history_replay: import_pending,
    }
}

pub async fn acp_enable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Enabling ACP on a session is an admin toggle mirroring the gated disable.
    if let Some(resp) = super::cityhall_block(&state) {
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
    //
    // Judged against the session's explicit `agent_name` (or, with none, the
    // tool itself), NOT `pick_agent_for_tool`'s default-agent fallback: that
    // fallback always resolves to a registry key, so gating on it would accept
    // every tool and switch a terminal-only session into a structured one
    // running some other agent. Shares the create path's predicate so the two
    // cannot drift.
    let resolvable = {
        let profile = profile.clone();
        let project_path = std::path::PathBuf::from(&instance.project_path);
        let tool = instance.tool.clone();
        let agent_name = instance.agent_name.clone();
        tokio::task::spawn_blocking(move || {
            super::sessions::agent_is_acp_capable(
                &profile,
                &project_path,
                &tool,
                agent_name.as_deref(),
            )
        })
        .await
        .unwrap_or(false)
    };
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
    // already-acp and unresolvable-agent cases returned above).
    let agent_name = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            instance.agent_name.as_deref(),
            &profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;

    // Tear down the tmux side. Best-effort: a stale tmux name should
    // not block the swap. Run on a blocking pool worker because each
    // kill shells out. Warn on agent kill failure to keep signal for
    // this user-initiated action; ancillary kinds delegate to the
    // shared helper so any future kind picked up by the audit lands
    // here automatically.
    let inst_for_transition = instance.clone();
    let id_for_log = id.clone();
    let profile_for_transition = profile.clone();
    let file_watch_for_transition = state.file_watch.clone();
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let storage =
            crate::session::Storage::new(&profile_for_transition, file_watch_for_transition)?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&inst_for_transition.id)
            .map_err(|error| {
                anyhow::anyhow!("failed to acquire terminal-to-ACP lifecycle lock: {error}")
            })?;
        if let Err(e) = inst_for_transition.kill_locked() {
            tracing::warn!(target: "acp.switch", session = %inst_for_transition.id, "kill tmux failed: {e}");
        }
        inst_for_transition.kill_ancillary_tmux_sessions_locked();
        storage.update(|all, _groups| {
            let Some(slot) = all
                .iter_mut()
                .find(|candidate| candidate.id == inst_for_transition.id)
            else {
                anyhow::bail!("session disappeared during terminal-to-ACP transition");
            };
            slot.view = crate::session::View::Structured;
            slot.resume_intent = crate::session::ResumeIntent::Default;
            // Reset to Idle: killing tmux above removes the poller that would
            // settle a terminal session's status, and structured rows are
            // skipped by the tmux poller. Structured status is event-driven
            // (`derive_acp_status`), where only a `Stopped` maps back to Idle
            // and the worker's connect (`AcpSessionAssigned` -> HealError)
            // clears only Error/Stopped. So a session that was Running/Waiting
            // at conversion would otherwise stay stuck "working" with no live
            // turn to stop. The freshly spawned worker has no turn; a real
            // prompt drives Running -> Stopped -> Idle normally.
            slot.status = crate::session::Status::Idle;
            slot.lifecycle_generation = slot.lifecycle_generation.saturating_add(1);
            Ok(slot.lifecycle_generation)
        })
    })
    .await;
    let lifecycle_generation = match transition {
        Ok(Ok(generation)) => generation,
        Ok(Err(error)) => {
            tracing::error!(target: "acp.switch", session = %id_for_log, "terminal-to-ACP transition failed: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to switch session to structured view",
            )
                .into_response();
        }
        Err(join_error) => {
            tracing::error!(target: "acp.switch", session = %id_for_log, "terminal-to-ACP transition task panicked: {join_error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to switch session to structured view",
            )
                .into_response();
        }
    };
    instance.view = crate::session::View::Structured;
    instance.resume_intent = crate::session::ResumeIntent::Default;
    instance.status = crate::session::Status::Idle;
    instance.lifecycle_generation = lifecycle_generation;
    {
        let mut instances = state.instances.write().await;
        if let Some(slot) = instances.iter_mut().find(|candidate| candidate.id == id) {
            if lifecycle_generation >= slot.lifecycle_generation {
                slot.view = crate::session::View::Structured;
                slot.resume_intent = crate::session::ResumeIntent::Default;
                slot.status = crate::session::Status::Idle;
                slot.lifecycle_generation = lifecycle_generation;
            }
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
    let effort = instance.acp_effort.clone();
    let yolo_mode = instance.yolo_mode;
    let acp_mode_id = instance.acp_mode_id.clone();
    // #2252 direction B: a claude terminal session's resumable transcript lives
    // in `agent_session_id`, not `acp_session_id`. When present on the host (the
    // seeded `session/load` hard-fails on a missing id), carry it into the
    // structured spawn so the conversation continues in structured view. The
    // in-container transcript can't be probed from the host, so sandboxed
    // sessions attempt the load unconditionally.
    let transcript_present = instance.is_sandboxed()
        || instance
            .agent_session_id
            .as_deref()
            .map(|sid| {
                !crate::session::capture::claude_host_transcript_confirmed_absent(
                    &instance.project_path,
                    sid,
                    &instance.resolved_host_environment(),
                )
            })
            .unwrap_or(false);
    let seed = resolve_structured_seed(
        &instance.tool,
        &agent_name,
        instance.acp_session_id.as_deref(),
        instance.agent_session_id.as_deref(),
        instance.import_pending == Some(true),
        transcript_present,
    );
    let stored_acp_session_id = seed.stored_acp_session_id;
    // #2276: seed the transcript from the session/load replay when enabling the
    // structured view on an imported session, or on a direction-B keep-context
    // switch (empty store, load an existing transcript).
    let seed_history_replay = seed.seed_history_replay;
    // Structured fork: send session/fork against the parent id on first connect.
    let fork_from = instance.fork_pending.clone();
    let profile_for_spawn = profile.clone();
    let command_override = crate::server::acp_reconciler::command_override_for_spawn(
        &instance.tool,
        &instance.command,
    );
    let tool_for_spawn = instance.tool.clone();
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
                tool: tool_for_spawn,
                cwd,
                additional_dirs: vec![],
                provider_env: vec![],
                model,
                effort,
                stored_acp_session_id,
                fork_from,
                sandbox_info,
                source_profile,
                yolo_mode,
                acp_mode_id,
                agent_command_override: command_override,
                seed_history_replay,
            })
            .await
        {
            // Capacity-aware banner selection (and the benign first-tick
            // duplicate) is documented on `structured_spawn_error_message`.
            let message = structured_spawn_error_message(&e, &agent_name);
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
/// When the agent pairing shares a CLI-resumable transcript (claude, see
/// `agents::acp_transcript_cli_resumable`) and an `acp_session_id` is set,
/// the swap preserves context: the worker is shut down WITHOUT `session/delete`
/// so the transcript survives, the ACP session id is carried into the terminal
/// `agent_session_id` as a pinned resume target, and tmux comes back running
/// `claude --resume <id>` on the same conversation (#2252). Otherwise history
/// is destroyed: `session/delete` releases the transcript and tmux comes back
/// with an empty pane the agent fills as it runs.
pub async fn acp_disable(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Disabling ACP drops the session to the terminal view, which CityHall
    // mode forbids. Keep sessions in structured view.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    // Worker-stopping barrier: submission guard before any teardown, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    };
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;
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
    // already-terminal case returned above).

    // Decide whether this swap can preserve context. Resolve the ACTIVE
    // structured-view adapter (switch_acp_agent can point agent_name away
    // from the tool's default) and keep context only when it shares a
    // CLI-resumable transcript with the terminal `<tool> --resume`, and an
    // acp_session_id was actually captured. See #2252.
    let acp_agent = state
        .acp_supervisor
        .pick_agent_for_tool(
            &instance.tool,
            instance.agent_name.as_deref(),
            &profile,
            std::path::Path::new(&instance.project_path),
        )
        .await;
    let keep_context = crate::agents::acp_transcript_cli_resumable(&instance.tool, &acp_agent)
        && instance.acp_session_id.is_some();

    // Tear down the acp worker. `shutdown` preserves the agent's on-disk
    // transcript (no session/delete) so the terminal can resume it;
    // `shutdown_and_delete` releases it for the destructive path. UnknownSession
    // is fine, the supervisor may not have a worker if startup never completed.
    // See #1710.
    let shutdown_result = if keep_context {
        state.acp_supervisor.shutdown(&id).await
    } else {
        state.acp_supervisor.shutdown_and_delete(&id).await
    };
    match shutdown_result {
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
    // Drop the structured-view event projection either way: on keep-context,
    // the terminal `claude --resume` reprints the conversation itself, so the
    // tmux pane does not need the AoE event replay, and terminal turns would
    // otherwise leave it stale. See #2252.
    state.acp_event_store.delete_session(&id);
    if keep_context {
        tracing::debug!(
            target: "acp.switch",
            session = %id,
            "keeping context on disable: carrying acp_session_id into agent_session_id for claude --resume"
        );
        instance.switch_to_terminal_keep_context();
    } else {
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
            // Disabling structured view abandons any pending import (#2276).
            instance.import_pending = None;
        }
    }

    // Persist + start tmux. start() now no longer short-circuits for
    // structured_view, so it will create a fresh tmux session and run
    // the agent CLI in the pane.
    //
    // The on-disk and in-memory updates mutate ONLY the fields this handler
    // owns (view, acp_session_id, import_pending, and on keep-context the
    // resume target agent_session_id + resume_intent), copied from the
    // mutated in-memory `instance` so all three copies stay in sync.
    // Wholesale replacement with a pre-lock snapshot would clobber
    // concurrent writes to other fields made by the status poll loop or
    // other handlers between the snapshot and the lock acquisition.
    let persist_acp_session_id = instance.acp_session_id.clone();
    let persist_import_pending = instance.import_pending;
    let persist_agent_session_id = instance.agent_session_id.clone();
    let persist_resume_intent = instance.resume_intent.clone();
    {
        let mut instances = state.instances.write().await;
        if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
            slot.view = crate::session::View::Terminal;
            slot.acp_session_id = persist_acp_session_id.clone();
            slot.import_pending = persist_import_pending;
            if keep_context {
                slot.agent_session_id = persist_agent_session_id.clone();
                slot.resume_intent = persist_resume_intent.clone();
            }
        }
    }
    let id_for_save = id.clone();
    let profile_for_save = profile.clone();
    let file_watch_for_save = state.file_watch.clone();
    let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::new(&profile_for_save, file_watch_for_save)?;
        storage.update(|all, _groups| {
            if let Some(slot) = all.iter_mut().find(|i| i.id == id_for_save) {
                slot.view = crate::session::View::Terminal;
                slot.acp_session_id = persist_acp_session_id.clone();
                slot.import_pending = persist_import_pending;
                if keep_context {
                    slot.agent_session_id = persist_agent_session_id.clone();
                    slot.resume_intent = persist_resume_intent.clone();
                }
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

/// Whether a mode value selects plan mode. `"plan"` is the canonical value for
/// both the mode endpoint's `mode_id` and the config-option endpoint's `value`.
/// Routing both through this one check keeps the plan-mode telemetry tally from
/// drifting between the two paths. See the web `modeChannel.ts`, where the plan
/// choice id is `"plan"` regardless of channel.
fn is_plan_mode_value(value: &str) -> bool {
    value == "plan"
}

/// Set the active session mode (Default / Plan / AcceptEdits /
/// BypassPermissions) through the adapter's advertised mode channel.
pub async fn acp_set_mode(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetModeRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    // Agent mode switching is a power control the dumbed-down composer omits.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    match state.acp_supervisor.set_mode(&id, &req.mode_id).await {
        Ok(()) => {
            // The agent accepted the mode switch; tally plan-mode adoption for
            // the opt-in telemetry snapshot. Other modes are out of scope for now.
            if is_plan_mode_value(&req.mode_id) {
                state
                    .telemetry_structured
                    .plan_mode_seen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => supervisor_error_response("set_mode failed", &e),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetConfigOptionRequest {
    pub config_id: String,
    pub value: String,
}

/// Which persisted `Instance` field a successful config-option pick writes to.
/// Carries the field name for logging and the mutation together so the two
/// cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedSelector {
    /// `Instance.agent_model`, re-injected on every spawn.
    Model,
    /// `Instance.acp_mode_id`, re-asserted via `session/set_mode` on every
    /// worker (re)spawn (#2897).
    Mode,
    /// `Instance.acp_effort`, re-applied through the agent's thought-level
    /// config option after every handshake.
    ThoughtLevel,
}

impl PersistedSelector {
    fn field(self) -> &'static str {
        match self {
            Self::Model => "agent_model",
            Self::Mode => "acp_mode_id",
            Self::ThoughtLevel => "acp_effort",
        }
    }

    fn apply(self, inst: &mut crate::session::Instance, value: String) {
        match self {
            Self::Model => inst.agent_model = Some(value),
            Self::Mode => inst.acp_mode_id = Some(value),
            Self::ThoughtLevel => inst.acp_effort = Some(value),
        }
    }
}

/// Write a picked selector value back onto the instance, both in the in-memory
/// registry (what the reconciler reads to respawn a worker this daemon
/// lifetime) and on disk (what survives a daemon restart). Called from
/// `acp_set_config_option` after the live pick succeeds; see the comment there
/// for why the live call alone is not enough.
async fn persist_selector(
    state: &Arc<AppState>,
    id: &str,
    selector: PersistedSelector,
    value: &str,
) {
    let profile = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return;
        };
        selector.apply(inst, value.to_string());
        inst.source_profile.clone()
    };
    match crate::session::Storage::new(&profile, state.file_watch.clone()) {
        Ok(storage) => {
            let id_owned = id.to_string();
            let value_owned = value.to_string();
            if let Err(e) = storage.update(|instances, _groups| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == id_owned) {
                    selector.apply(inst, value_owned.clone());
                }
                Ok(())
            }) {
                tracing::error!(
                    target: "http.api.acp",
                    session = %id,
                    field = selector.field(),
                    "failed to persist selector after config-option pick: {e}"
                );
            }
        }
        Err(e) => {
            tracing::error!(
                target: "http.api.acp",
                session = %id,
                field = selector.field(),
                "failed to open storage to persist selector after config-option pick: {e}"
            );
        }
    }
}

/// Category the agent advertised for `config_id`, or `None` when the option is
/// unknown. Resolved from the agent's advertised option catalog rather than
/// hardcoded ids, so a mode pick is told apart from a model / thought-level
/// pick without assuming any adapter's naming. The daemon keeps no live
/// per-session `AcpState`, so the option catalog (recorded on
/// `ConfigOptionsUpdated`) is the only handler-reachable source of a session's
/// advertised options; if it has no entry yet we return `None` and skip
/// persistence (no regression).
async fn config_option_category(
    state: &Arc<AppState>,
    id: &str,
    config_id: &str,
) -> Option<crate::acp::state::ConfigOptionCategory> {
    let agent = {
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            i.agent_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(i.tool.as_str())
                .to_string()
        })
    }?;
    crate::acp::option_catalog::load()
        .agents
        .get(&agent)
        .into_iter()
        .flat_map(|entry| entry.options.iter())
        .find(|opt| opt.id == config_id)
        .map(|opt| opt.category.clone())
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
    // Agent config options are a power control the dumbed-down composer omits.
    if let Some(resp) = super::cityhall_block(&state) {
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
        Ok(()) => {
            // Tally plan-mode adoption. claude-agent-acp v0.37.0+ (and OpenCode)
            // advertise the mode picker as a config option of category "mode" and
            // the web picker writes directly through this endpoint, so the plan
            // tally has to live here too or the modern fleet reports zero. No other
            // config category (model,
            // thought level) carries a "plan" value, so keying on the value alone
            // is safe and stays in sync with the mode endpoint via the shared check.
            if is_plan_mode_value(&req.value) {
                state
                    .telemetry_structured
                    .plan_mode_seen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // Persist a model pick so it survives a worker respawn. The live
            // set_config_option above only reconfigures the running process; the
            // reconciler re-reads `agent_model` on every spawn and injects it as
            // AOE_AGENT_MODEL, so without this write-back a respawn reverts to
            // the stale stored model and silently overrides the agent's own
            // default. Mirrors the persist step of the agent-switch path above.
            // ponytail: keys on the well-known "model" config id, which every
            // current adapter and the frontend use; resolve category == Model
            // from the session's config_options if a future adapter uses another
            // id.
            // A Mode-category pick is persisted for the same reason: the
            // reconciler re-asserts `acp_mode_id` via `session/set_mode` on
            // every (re)spawn (#2897), so without the write-back the id stays
            // at its creation value (commonly None) and a respawn reverts the
            // session to the adapter's prompting default. See #3086.
            //
            // A ThoughtLevel pick is persisted so the handshake can re-apply it
            // through the agent's thought-level config option; without it a
            // respawn spawns with `effort: None` and the pick reverts to the
            // agent default.
            let selector = if req.config_id == "model" {
                Some(PersistedSelector::Model)
            } else {
                match config_option_category(&state, &id, &req.config_id).await {
                    Some(crate::acp::state::ConfigOptionCategory::Model) => {
                        Some(PersistedSelector::Model)
                    }
                    Some(crate::acp::state::ConfigOptionCategory::Mode) => {
                        Some(PersistedSelector::Mode)
                    }
                    Some(crate::acp::state::ConfigOptionCategory::ThoughtLevel) => {
                        Some(PersistedSelector::ThoughtLevel)
                    }
                    Some(crate::acp::state::ConfigOptionCategory::Other(_)) | None => None,
                }
            };
            if let Some(selector) = selector {
                persist_selector(&state, &id, selector, &req.value).await;
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => supervisor_error_response("set_config_option failed", &e),
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
    let nonce = Nonce(nonce_str.clone());
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
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::UnknownNonce)) => {
            // Intentional override of the canonical Acp 500: echo the nonce
            // so clients (web + native TUI) can confirm the 404 refers to
            // the card they resolved, not a generic miss. See #1821.
            (
                StatusCode::NOT_FOUND,
                format!("no pending approval with nonce {nonce_str}"),
            )
                .into_response()
        }
        Err(e) => supervisor_error_response("resolve failed", &e),
    }
}

/// Resolve a pending `AskUserQuestion` elicitation. The body is an
/// `ElicitationResolution` (`{"action":"accept","answers":{...}}`,
/// `{"action":"decline"}`, or `{"action":"cancel"}`); answers are
/// validated server-side before they reach the agent. Mirrors
/// `resolve_approval`: 204 on success, 404 for an unknown session or
/// nonce.
pub async fn resolve_elicitation(
    State(state): State<Arc<AppState>>,
    Path((id, nonce_str)): Path<(String, String)>,
    req: Result<Json<ElicitationResolution>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(resolution) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let nonce = Nonce(nonce_str.clone());
    match state
        .acp_supervisor
        .resolve_elicitation(&id, nonce, resolution)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        // Intentional override: nonce echo, mirrors resolve_approval. See #1821.
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::UnknownNonce)) => (
            StatusCode::NOT_FOUND,
            format!("no pending elicitation with nonce {nonce_str}"),
        )
            .into_response(),
        // Intentional override: a failed server-side validation leaves the
        // elicitation pending, so 422 (not 404): the client can correct the
        // answer and resubmit the same nonce.
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::InvalidAnswer(msg))) => {
            (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response()
        }
        Err(e) => supervisor_error_response("resolve failed", &e),
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
    // `before` switches to backward (older-first) paging for recent-first
    // load; absent it stays on the forward `since` contract WS catch-up
    // and existing clients rely on.
    let backward = q.before.is_some();
    let page = match q.before {
        Some(before) => state
            .acp_event_store
            .replay_page_before(&id, before, Some(limit)),
        None => state.acp_event_store.replay_page(&id, q.since, Some(limit)),
    };
    let highest_seq = page.highest_seq;
    let lowest_seq = page.lowest_seq;
    let next_cursor = page.last_scanned_seq;
    let has_more = page.has_more;
    // `view=rows` folds THIS page through `TranscriptModel` and returns the
    // built rows instead of raw frames, keeping the pagination metadata
    // identical so the client's recent-first / older-page cursor logic is
    // unchanged. Per-page folding cannot see earlier context, so a page whose
    // `tool_start` sits in an older page relies on `TranscriptModel`'s
    // synth-on-missing-start (already implemented; the intended behavior).
    let want_rows = q.view.as_deref() == Some("rows");
    let (frames, rows) = if want_rows {
        let mut model = crate::acp::transcript::TranscriptModel::new();
        for (seq, event) in &page.events {
            model.apply_event(*seq, event);
        }
        (Vec::new(), Some(model.rows().to_vec()))
    } else {
        let frames: Vec<crate::server::AcpBroadcastFrame> = page
            .events
            .into_iter()
            .map(|(seq, event)| crate::server::AcpBroadcastFrame {
                session_id: id.clone(),
                seq,
                event: Arc::new(event),
            })
            .collect();
        (frames, None)
    };
    // `lost = true` when the client's `since` cursor predates the oldest
    // seq still on disk. The retention cap can evict older events, so a
    // client that returns after a long absence may legitimately need a
    // full reload. With no events on disk yet, nothing is lost. Computed
    // per request so a mid-loop prune is caught on whatever page first
    // sees the gap, not only the first.
    // Backward paging walks down only within what's stored, so the
    // truncation signal doesn't apply; the client stops when `has_more`
    // clears. `lost` stays a forward-replay concern.
    let lost = match (backward, lowest_seq) {
        (false, Some(lo)) => q.since < lo.saturating_sub(1),
        _ => false,
    };
    Json(ReplayResponse {
        frames,
        lost,
        highest_seq,
        lowest_seq,
        next_cursor,
        has_more,
        rows,
    })
    .into_response()
}

/// List existing Claude Code sessions on disk, newest first, for the import
/// picker. Gated behind `read_only_block`: this exposes external Claude session
/// titles and working directories from outside AoE-managed state, and import
/// can't run in read-only mode anyway, so read-only dashboard users get no
/// historical prompt metadata. See #2276.
pub async fn list_claude_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let mut sessions = tokio::task::spawn_blocking(crate::session::claude_import::scan_sessions)
        .await
        .unwrap_or_default();
    // Drop sessions AoE owns: importing one is a no-op and they are noise in
    // the picker. Two signals (instances span all profiles, #2276):
    //   - id match: a managed session's on-disk id equals the instance's
    //     acp_session_id (structured) or agent_session_id (terminal resume).
    //   - cwd match: any session whose cwd is inside an AoE-PROVISIONED dir
    //     (scratch, a managed worktree, or a workspace). This catches the
    //     smart-rename one-shot AoE runs in that dir, which has its own id not
    //     stored anywhere. It deliberately does NOT cover a plain project_path:
    //     a user running `claude` directly in the same repo as an AoE session
    //     is a real importable conversation, not AoE's, so it stays in the list
    //     and is only filtered by id match.
    let (managed_ids, managed_dirs): (std::collections::HashSet<String>, Vec<PathBuf>) = {
        let instances = state.instances.read().await;
        let ids = instances
            .iter()
            .flat_map(|i| {
                i.acp_session_id
                    .iter()
                    .chain(i.agent_session_id.iter())
                    .cloned()
            })
            .collect();
        let dirs = instances
            .iter()
            .filter(|i| {
                i.scratch
                    || i.worktree_info.as_ref().is_some_and(|w| w.managed_by_aoe)
                    || i.workspace_info.is_some()
            })
            .map(|i| PathBuf::from(&i.project_path))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        (ids, dirs)
    };
    sessions.retain(|s| {
        if managed_ids.contains(&s.session_id) {
            return false;
        }
        let cwd = std::path::Path::new(&s.cwd);
        !managed_dirs.iter().any(|d| cwd.starts_with(d))
    });
    // Cap AFTER ownership filtering so a burst of AoE-managed sessions can't
    // push real imports off the (newest-first) list. See #2276.
    sessions.truncate(crate::session::claude_import::MAX_SESSIONS);
    Json(sessions).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn utc_ts(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// #3401: pressing Stop tears the worker down and respawns it, and a
    /// request that lands in that window fails its command send with
    /// `AgentExited`. Answering the user's own stop with a 500 reported a
    /// fault for a recovery that worked, so the window maps to the
    /// retryable `worker_not_ready` contract the structured view already
    /// re-queues on. A real protocol fault still has to reach 500.
    #[tokio::test]
    async fn agent_exited_maps_to_the_retryable_status_not_a_fault() {
        let cases = [
            (
                SupervisorError::Acp(AcpError::AgentExited),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SupervisorError::Acp(AcpError::NotRunning),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SupervisorError::Acp(AcpError::Protocol("bad frame".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                SupervisorError::UnknownSession("s-1".into()),
                StatusCode::NOT_FOUND,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(
                supervisor_error_response("prompt failed", &err).status(),
                expected,
                "{err}"
            );
        }

        // The prefix is load-bearing: the composer keys its re-queue (and
        // its banner suppression) on a 503 body that starts with it.
        let body = supervisor_error_response(
            "prompt failed",
            &SupervisorError::Acp(AcpError::AgentExited),
        )
        .into_body();
        let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).starts_with("worker_not_ready"),
            "body must carry the transient marker, got {bytes:?}"
        );
    }

    /// #3241: the switch-agent modal must not offer a target the policy refuses,
    /// so the response is filtered rather than listing everything the registry
    /// knows. Runs the real projection, not pre-filtered input.
    #[test]
    fn acp_agent_entries_drop_agents_the_policy_refuses() {
        use crate::acp::agent_policy::AgentPolicy;
        let registry = crate::acp::AgentRegistry::with_defaults();
        let names = |p: &AgentPolicy| -> Vec<String> {
            acp_agent_entries(&registry, p)
                .into_iter()
                .map(|e| e.name)
                .collect()
        };

        // Unrestricted: every registry entry survives, sorted.
        let all = names(&AgentPolicy::for_test(false, &[]));
        assert_eq!(all.len(), registry.agents.len());
        assert!(all.windows(2).all(|w| w[0] <= w[1]), "sorted: {all:?}");

        // Restricted: exactly the permitted keys. A listed name that is not in
        // the registry does not invent an entry.
        assert_eq!(
            names(&AgentPolicy::for_test(
                true,
                &["opencode", "claude", "not-a-real-agent"]
            )),
            vec!["claude".to_string(), "opencode".to_string()]
        );

        // The descriptive fields still ride along for what does survive.
        let entries = acp_agent_entries(&registry, &AgentPolicy::for_test(true, &["claude"]));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "claude-agent-acp");
        assert!(!entries[0].description.is_empty());

        assert!(names(&AgentPolicy::for_test(true, &[])).is_empty());
    }

    #[test]
    fn acp_agent_entries_lifecycle_wire_shape() {
        // Same contract as /api/agents: lifecycle omitted while Active,
        // full metadata for deprecated registry keys. Pinned so the
        // switch-agent modal's server-wins precedence has a stable shape.
        use crate::acp::agent_policy::AgentPolicy;
        let registry = crate::acp::AgentRegistry::with_defaults();
        let entries = acp_agent_entries(
            &registry,
            &AgentPolicy::for_test(true, &["claude", "gemini"]),
        );
        let cases = [("claude", false), ("gemini", true)];
        for (name, deprecated) in cases {
            let entry = entries.iter().find(|e| e.name == name).unwrap();
            let value = serde_json::to_value(entry).unwrap();
            assert_eq!(
                value.get("lifecycle").is_some(),
                deprecated,
                "{name}: {value}"
            );
            if deprecated {
                assert_eq!(value["lifecycle"]["state"], "deprecated");
                assert_eq!(value["lifecycle"]["replacement"], "antigravity");
            }
        }
    }

    #[test]
    fn resolve_structured_seed_covers_import_direction_b_and_fresh() {
        // Existing acp_session_id (prior structured / import): load it; replay
        // only when import_pending.
        let s = resolve_structured_seed(
            "claude",
            "claude",
            Some("acp-1"),
            Some("agent-1"),
            true,
            true,
        );
        assert_eq!(s.stored_acp_session_id.as_deref(), Some("acp-1"));
        assert!(s.seed_history_replay);
        let s = resolve_structured_seed("claude", "claude", Some("acp-1"), None, false, true);
        assert_eq!(s.stored_acp_session_id.as_deref(), Some("acp-1"));
        assert!(!s.seed_history_replay);

        // Direction B: terminal claude, no acp id, resumable transcript present.
        let s = resolve_structured_seed("claude", "claude", None, Some("agent-1"), false, true);
        assert_eq!(s.stored_acp_session_id.as_deref(), Some("agent-1"));
        assert!(s.seed_history_replay);

        // Transcript confirmed absent: do not attempt the seeded load.
        let s = resolve_structured_seed("claude", "claude", None, Some("agent-1"), false, false);
        assert_eq!(s.stored_acp_session_id, None);
        assert!(!s.seed_history_replay);

        // Non-resumable pairing (adapter swapped away, or non-claude): fresh.
        let s = resolve_structured_seed("claude", "codex", None, Some("agent-1"), false, true);
        assert_eq!(s.stored_acp_session_id, None);
        assert!(!s.seed_history_replay);
        let s = resolve_structured_seed("codex", "codex", None, Some("agent-1"), false, true);
        assert_eq!(s.stored_acp_session_id, None);
        assert!(!s.seed_history_replay);

        // No id anywhere: fresh session/new.
        let s = resolve_structured_seed("claude", "claude", None, None, false, true);
        assert_eq!(s.stored_acp_session_id, None);
        assert!(!s.seed_history_replay);
    }

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
    fn plan_mode_value_matches_only_plan() {
        // Both `acp_set_mode` (`mode_id`) and `acp_set_config_option`
        // (config-option `value`) tally plan-mode adoption through this check, so
        // it must accept the canonical "plan" value and reject every other mode or
        // selector value (Default / AcceptEdits / model ids / thought levels).
        assert!(is_plan_mode_value("plan"));
        assert!(!is_plan_mode_value("default"));
        assert!(!is_plan_mode_value("accept_edits"));
        assert!(!is_plan_mode_value("bypassPermissions"));
        assert!(!is_plan_mode_value("yolo"));
        assert!(!is_plan_mode_value("Plan"));
        assert!(!is_plan_mode_value(""));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_agent_model_updates_memory_and_storage() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let profile = "default";

        let mut inst = crate::session::Instance::new("t", "/tmp");
        inst.source_profile = profile.to_string();
        inst.agent_model = Some("claude-sonnet-4-6".to_string());
        let id = inst.id.clone();

        // Seed on disk so the storage write-back can find the row to update.
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed);
                Ok(())
            })
            .unwrap();

        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        persist_selector(&state, &id, PersistedSelector::Model, "claude-sonnet-5").await;

        // In-memory registry updated (what the reconciler reads to respawn).
        assert_eq!(
            state.instances.read().await[0].agent_model.as_deref(),
            Some("claude-sonnet-5")
        );

        // On disk too (what survives a daemon restart).
        let reloaded = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            reloaded
                .iter()
                .find(|i| i.id == id)
                .unwrap()
                .agent_model
                .as_deref(),
            Some("claude-sonnet-5")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_acp_mode_updates_memory_and_storage() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let profile = "default";

        // A Codex session created without an explicit mode: acp_mode_id is None,
        // which is the exact state that made a picked mode vanish on respawn.
        let mut inst = crate::session::Instance::new("t", "/tmp");
        inst.source_profile = profile.to_string();
        assert_eq!(inst.acp_mode_id, None);
        let id = inst.id.clone();

        // Seed on disk so the storage write-back can find the row to update.
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed);
                Ok(())
            })
            .unwrap();

        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        persist_selector(&state, &id, PersistedSelector::Mode, "agent-full-access").await;

        // In-memory registry updated (what the reconciler re-asserts on respawn).
        assert_eq!(
            state.instances.read().await[0].acp_mode_id.as_deref(),
            Some("agent-full-access")
        );

        // On disk too (what survives a daemon restart).
        let reloaded = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            reloaded
                .iter()
                .find(|i| i.id == id)
                .unwrap()
                .acp_mode_id
                .as_deref(),
            Some("agent-full-access")
        );
    }

    /// An explicit thought-level pick must reach both the in-memory registry
    /// and disk, or the handshake has nothing to re-apply and the pick reverts
    /// to the agent default on the next respawn.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_acp_effort_updates_memory_and_storage() {
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();
        let profile = "default";

        // A session created without an explicit effort: acp_effort is None, so
        // it inherits the configured default until the user picks.
        let mut inst = crate::session::Instance::new("t", "/tmp");
        inst.source_profile = profile.to_string();
        assert_eq!(inst.acp_effort, None);
        let id = inst.id.clone();

        // Seed on disk so the storage write-back can find the row to update.
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _groups| {
                instances.push(seed);
                Ok(())
            })
            .unwrap();

        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        persist_selector(&state, &id, PersistedSelector::ThoughtLevel, "high").await;

        // In-memory registry updated (what the reconciler reads to respawn).
        assert_eq!(
            state.instances.read().await[0].acp_effort.as_deref(),
            Some("high")
        );

        // On disk too (what survives a daemon restart).
        let reloaded = crate::session::Storage::new_unwatched(profile)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            reloaded
                .iter()
                .find(|i| i.id == id)
                .unwrap()
                .acp_effort
                .as_deref(),
            Some("high")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn config_option_category_resolves_categories_from_catalog() {
        use crate::acp::state::{ConfigOptionCategory, ConfigOptionChoice, ConfigOptionDescriptor};
        use crate::session::test_support::isolate_app_dir;
        let _tmp = isolate_app_dir();

        let mut inst = crate::session::Instance::new("codex", "/tmp");
        inst.agent_name = Some("codex".to_string());
        let id = inst.id.clone();

        let opts = vec![
            ConfigOptionDescriptor {
                id: "codex-mode".to_string(),
                name: "Mode".to_string(),
                description: None,
                category: ConfigOptionCategory::Mode,
                current_value: "agent".to_string(),
                options: vec![ConfigOptionChoice {
                    value: "agent-full-access".to_string(),
                    name: "Agent (full access)".to_string(),
                    description: None,
                }],
            },
            ConfigOptionDescriptor {
                id: "model".to_string(),
                name: "Model".to_string(),
                description: None,
                category: ConfigOptionCategory::Model,
                current_value: "gpt-5".to_string(),
                options: vec![],
            },
            ConfigOptionDescriptor {
                id: "reasoning-effort".to_string(),
                name: "Reasoning effort".to_string(),
                description: None,
                category: ConfigOptionCategory::ThoughtLevel,
                current_value: "medium".to_string(),
                options: vec![ConfigOptionChoice {
                    value: "high".to_string(),
                    name: "High".to_string(),
                    description: None,
                }],
            },
        ];
        crate::acp::option_catalog::record("codex", &opts, "2026-07-24T00:00:00Z".to_string())
            .unwrap();

        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        assert_eq!(
            config_option_category(&state, &id, "codex-mode").await,
            Some(ConfigOptionCategory::Mode)
        );
        assert_eq!(
            config_option_category(&state, &id, "model").await,
            Some(ConfigOptionCategory::Model)
        );
        assert_eq!(
            config_option_category(&state, &id, "reasoning-effort").await,
            Some(ConfigOptionCategory::ThoughtLevel)
        );
        assert_eq!(config_option_category(&state, &id, "unknown").await, None);
    }

    #[tokio::test]
    async fn spawn_acp_missing_session_does_not_create_instance_lock() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());

        let response = spawn_acp(
            State(state.clone()),
            Path("missing".to_string()),
            Ok(Json(SpawnAcpRequest {
                agent: None,
                model: None,
                additional_dirs: Vec::new(),
                provider_env: Vec::new(),
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(state.instance_locks.read().await.is_empty());
    }

    /// #3172: two invariants of the idle-dormant prompt-wake path, both of
    /// which the pre-fix tree violates.
    ///
    /// 1. `acp_prompt` must release `instance_lock` before it awaits the
    ///    worker. The detached spawn task the wake kicks needs that same
    ///    lock inside `build_spawn_request`, so a handler that keeps it
    ///    stalls its own resume for the whole `WORKER_READY_TIMEOUT` and
    ///    then reports the worker never arrived.
    /// 2. A prompt that never reaches a worker must not be published. A
    ///    `UserPromptSent` with no turn behind it renders as a session
    ///    stuck on "running" that only a stop plus re-send clears.
    ///
    /// A held `ResumeReservation` stands in for a spawn in flight: it makes
    /// `wait_for_worker` park exactly as it does mid-respawn, with no
    /// process, sandbox, or agent involved. It also pins the subtler of the
    /// two publish branches, because a reservation counts as `is_running`:
    /// `needs_resume` is false here even though no worker exists, so only
    /// an unconditional readiness gate catches it. Pre-fix that combination
    /// published the prompt and then answered 404; it is now a retryable
    /// 503 with nothing written.
    #[tokio::test]
    async fn wake_prompt_frees_instance_lock_and_publishes_nothing_without_a_worker() {
        use crate::acp::supervisor::{ResumeKind, ResumeReservationOutcome};
        use std::time::Duration;

        let mut inst = crate::session::Instance::new("wake-3172", "/tmp/aoe-3172-project");
        inst.id = "sess-3172".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // Hold the reservation for the whole probe so no worker can land.
        let reservation = match state
            .acp_supervisor
            .begin_resume(&id, ResumeKind::Spawn)
            .await
            .expect("begin_resume must not error under capacity")
        {
            ResumeReservationOutcome::Reserved(r) => r,
            ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
        };

        let handler = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                acp_prompt(
                    State(state),
                    Path(id),
                    Ok(Json(PromptRequest {
                        text: "lgtm".to_string(),
                        attachments: Vec::new(),
                        prompt_id: None,
                    })),
                )
                .await
                .into_response()
            }
        });

        // Let the handler reach its parked wait. It cannot return until the
        // reservation drops, so anything past the wake is enough.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The 2s budget is far under the 10s `WORKER_READY_TIMEOUT` the
        // pre-fix handler holds the lock for, and far over the microseconds
        // `touch_on_prompt_and_wake_if_sunk` holds it now.
        let inst_lock = state.instance_lock(&id).await;
        let acquired = tokio::time::timeout(Duration::from_secs(2), inst_lock.lock()).await;
        assert!(
            acquired.is_ok(),
            "acp_prompt must not hold instance_lock while it waits for the worker"
        );
        drop(acquired);

        drop(reservation);
        let response = tokio::time::timeout(Duration::from_secs(30), handler)
            .await
            .expect("handler must finish once the reservation drops")
            .expect("handler task must not panic");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        assert!(
            !state
                .acp_event_store
                .replay_from(&id, 0)
                .iter()
                .any(|(_, e)| matches!(e, Event::UserPromptSent { .. })),
            "a prompt no worker ever received must not reach the event store"
        );
    }

    /// #3688: the recovery lives in `dispatch::decide`, not in one handler,
    /// so every admission site inherits it. Pinning it here rather than
    /// through a handler keeps the contract on the shared decision point:
    /// a handler that starts refusing the park again fails this first.
    #[tokio::test]
    async fn exhausted_rate_limit_park_is_sendable_at_the_shared_decision_point() {
        let mut inst = crate::session::Instance::new("exhausted-shared", "/tmp/aoe-3688-shared");
        inst.id = "sess-3688-shared".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // A worker-less session with no park parks the prompt, as ever.
        let (guard, dispatch) = state
            .session_service
            .begin_prompt_submission(&SessionCaller::User, &id, false)
            .await
            .expect("session exists");
        assert_eq!(
            dispatch,
            crate::acp::dispatch::PromptDispatch::Queued {
                reason: crate::acp::dispatch::QueueReason::WorkerDown,
            },
        );
        drop(guard);

        assert!(state.acp_supervisor.publish_stopped_if_seq(
            &id,
            crate::server::acp_reconciler::RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
            0,
        ));
        let (_guard, dispatch) = state
            .session_service
            .begin_prompt_submission(&SessionCaller::User, &id, false)
            .await
            .expect("session exists");
        assert_eq!(
            dispatch,
            crate::acp::dispatch::PromptDispatch::Sent,
            "the cap park is terminal, so queueing here strands the prompt \
             the banner told the user to send"
        );
    }

    /// #3688: a fresh prompt is the recovery the give-up banner points at, so
    /// an exhausted park must drive a resume rather than buffer the prompt on
    /// a queue only a live or idle-dormant worker drains.
    ///
    /// No `ResumeReservation` is held here on purpose: one makes `is_running`
    /// true, which short-circuits the park probe entirely and leaves the test
    /// asserting nothing about this path. The spawn instead fails on the
    /// missing project path, and its `AgentStartupError` is the proof that a
    /// resume ran at all.
    #[tokio::test]
    async fn exhausted_rate_limit_prompt_resumes_instead_of_queueing() {
        let mut inst = crate::session::Instance::new("exhausted-3688", "/tmp/aoe-3688-project");
        inst.id = "sess-3688".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        assert!(state.acp_supervisor.publish_stopped_if_seq(
            &id,
            crate::server::acp_reconciler::RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
            0,
        ));

        let response = acp_prompt(
            State(Arc::clone(&state)),
            Path(id.clone()),
            Ok(Json(PromptRequest {
                text: "start a fresh retry budget".to_string(),
                attachments: Vec::new(),
                prompt_id: None,
            })),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the resume could not spawn, which is retryable, not a queued 202"
        );
        assert!(
            state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .is_empty(),
            "the recovery prompt must not be stranded on the server queue"
        );
        let events = state.acp_event_store.replay_from(&id, 0);
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, Event::AgentStartupError { .. })),
            "the park must have driven a resume; without one nothing ever \
             brings this session back"
        );
        assert!(
            !events
                .iter()
                .any(|(_, e)| matches!(e, Event::UserPromptSent { .. })),
            "a prompt with no worker must remain unpublished for retry"
        );
    }

    /// #3621: deciding a prompt's disposition and acting on it is one step, so
    /// a direct prompt cannot slip between a queue drain's idle check and the
    /// moment its prompt reaches the agent.
    ///
    /// The drain reads the control fold, reloads attachments, and only then
    /// sends; `send_turn` flips the fold to `turn_active` when it publishes. A
    /// direct prompt whose own fold read landed inside that window also
    /// decided "idle", so both pushed a `ClientCmd::Prompt`. The agent takes
    /// the first and answers the second `agent_busy` — but `send_prompt`
    /// reports success as soon as the command is queued, so the drain has
    /// already retired the rows it sent. The follow-up is then gone from
    /// durable queue state having never been delivered.
    ///
    /// Both halves are asserted: the direct prompt parks while the drain owns
    /// the session, and a drain that runs against the turn the direct prompt
    /// started leaves its row queued instead of retiring it into a rejection.
    #[tokio::test]
    async fn a_direct_prompt_and_the_queue_drain_cannot_both_own_the_same_turn() {
        use std::time::Duration;

        let mut inst = crate::session::Instance::new("race-3621", "/tmp/aoe-3621-race");
        inst.id = "sess-3621-race".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // A worker that records what actually reaches the ACP command loop.
        let cmds = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        state
            .session_service
            .enqueue_prompt(
                &id,
                "q1".into(),
                "queued follow-up".into(),
                vec![],
                None,
                "t0".into(),
            )
            .await
            .expect("session exists");

        // Stand in for a drain that has decided to deliver and has not
        // published yet: it owns the session's submission slot for that whole
        // span.
        let drain_owns_it = state.session_service.prompt_submission(&id).await;

        let handler = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                acp_prompt(
                    State(state),
                    Path(id),
                    Ok(Json(PromptRequest {
                        text: "typed while the drain was mid-delivery".to_string(),
                        attachments: Vec::new(),
                        prompt_id: None,
                    })),
                )
                .await
                .into_response()
            }
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !handler.is_finished(),
            "a direct prompt must not decide its disposition while a drain owns the session"
        );

        drop(drain_owns_it);
        let response = tokio::time::timeout(Duration::from_secs(30), handler)
            .await
            .expect("the handler must finish once the drain releases the session")
            .expect("handler task must not panic");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Its publish is what makes the fold read `turn_active`, so the drain
        // that follows must park the queued row rather than deliver it into
        // the turn this prompt just started.
        state.session_service.drain_queued_prompts_once(&id).await;
        assert_eq!(
            state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .len(),
            1,
            "the queued follow-up survives for the next tick instead of being retired into an agent_busy rejection"
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *cmds.lock().expect("cmd log mutex poisoned"),
            ["prompt"],
            "exactly one prompt reaches the agent; a second would be refused as agent_busy"
        );
    }

    /// #3688: the diff-comments endpoint is the other user surface that opens
    /// a turn, and the cap park is terminal, so refusing here loses the review
    /// the user just wrote with nothing scheduled to make a retry work. It
    /// must wake the park like an ordinary prompt does. Same no-reservation
    /// reasoning as the prompt test above.
    #[tokio::test]
    async fn diff_comments_on_an_exhausted_park_resume_instead_of_refusing() {
        let mut inst = crate::session::Instance::new("dc-3688", "/tmp/aoe-3688-diff");
        inst.id = "sess-3688-diff".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        assert!(state.acp_supervisor.publish_stopped_if_seq(
            &id,
            crate::server::acp_reconciler::RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
            0,
        ));

        let response = acp_prompt_diff_comments(
            State(Arc::clone(&state)),
            Path(id.clone()),
            Ok(Json(DiffCommentsPromptRequest {
                intro: "review".to_string(),
                outro: String::new(),
                is_multi_repo: false,
                comments: Vec::new(),
                assembled_markdown: "please address these".to_string(),
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let events = state.acp_event_store.replay_from(&id, 0);
        assert!(
            events
                .iter()
                .any(|(_, e)| matches!(e, Event::AgentStartupError { .. })),
            "the review must have driven a resume rather than being refused \
             against a park nothing un-parks"
        );
        assert!(
            !events
                .iter()
                .any(|(_, e)| matches!(e, Event::UserDiffCommentsPrompt { .. })),
            "a review card with no worker behind it must stay unpublished"
        );
    }

    /// #3649: the diff-comments endpoint is a turn-starting surface, so it
    /// must settle a disposition under the submission guard rather than
    /// publish and forward unconditionally once it gets the guard.
    ///
    /// The winner of the guard is standing in for a queue drain that publishes
    /// its prompt before releasing. `send_prompt` reports success as soon as
    /// the command is queued, so a loser that forwarded anyway would answer
    /// 202, leave a `UserDiffCommentsPrompt` card in the transcript, and only
    /// then have the agent refuse the prompt as `agent_busy`.
    #[tokio::test]
    async fn diff_comments_refuse_to_open_a_turn_another_submission_started() {
        use std::time::Duration;

        let mut inst = crate::session::Instance::new("dc-3649", "/tmp/aoe-3649-diff");
        inst.id = "sess-3649-diff".to_string();
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let cmds = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;

        let winner = state.session_service.prompt_submission(&id).await;
        let handler = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                acp_prompt_diff_comments(
                    State(state),
                    Path(id),
                    Ok(Json(DiffCommentsPromptRequest {
                        intro: String::new(),
                        outro: String::new(),
                        is_multi_repo: false,
                        comments: Vec::new(),
                        assembled_markdown: "review this".to_string(),
                    })),
                )
                .await
                .into_response()
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !handler.is_finished(),
            "diff comments must not decide their disposition while another submission owns the session"
        );

        // What the winner does before it releases: the publish is the choke
        // point that flips the fold to `turn_active`.
        state
            .acp_supervisor
            .publish_user_prompt_with_attachments(&id, "the winning turn".into(), &[], None)
            .await;
        drop(winner);

        let response = tokio::time::timeout(Duration::from_secs(10), handler)
            .await
            .expect("the handler must finish once the winner releases the session")
            .expect("handler task must not panic");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            *cmds.lock().expect("cmd log mutex poisoned"),
            Vec::<&'static str>::new(),
            "nothing may reach the agent: the refusal is the whole point"
        );
        let published = state
            .acp_event_store
            .replay_from(&id, 0)
            .into_iter()
            .any(|(_, e)| matches!(e, Event::UserDiffCommentsPrompt { .. }));
        assert!(
            !published,
            "a refused review must not leave a card in the transcript"
        );
    }

    /// The ACP-side half of the #3650 worker-stopping barrier. `shutdown_acp`,
    /// `switch_acp_agent` and `acp_disable` all tear the worker down, so a
    /// queue drain mid-delivery must finish first: `send_turn` respawns a
    /// worker it finds gone, which would undo the shutdown and, for the
    /// switch, deliver the prompt to the agent the user just switched away
    /// from.
    #[tokio::test]
    async fn worker_stopping_acp_endpoints_wait_for_an_in_flight_submission() {
        use std::time::Duration;

        async fn call(which: &str, state: Arc<AppState>, id: String) -> axum::response::Response {
            match which {
                "shutdown" => shutdown_acp(State(state), Path(id)).await.into_response(),
                "switch" => switch_acp_agent(
                    State(state),
                    Path(id),
                    Json(SwitchAgentRequest {
                        target: "codex".to_string(),
                        model: None,
                        reason: None,
                    }),
                )
                .await
                .into_response(),
                "disable" => acp_disable(State(state), Path(id)).await.into_response(),
                other => unreachable!("unknown handler {other}"),
            }
        }

        for which in ["shutdown", "switch", "disable"] {
            let mut inst = crate::session::Instance::new("acp-3650", "/tmp/aoe-3650-acp");
            inst.id = format!("sess-3650-acp-{which}");
            inst.view = crate::session::View::Structured;
            inst.status = crate::session::Status::Idle;
            let id = inst.id.clone();
            let state = crate::server::test_support::build_test_app_state(vec![inst]);

            let delivering = state.session_service.prompt_submission(&id).await;
            let handler = tokio::spawn({
                let state = Arc::clone(&state);
                let id = id.clone();
                async move { call(which, state, id).await }
            });

            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                !handler.is_finished(),
                "{which} must not tear the worker down under an in-flight submission"
            );

            drop(delivering);
            tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap_or_else(|_| panic!("{which} must finish once the submission releases"))
                .unwrap_or_else(|e| panic!("{which} task must not panic: {e}"));
        }
    }

    #[tokio::test]
    async fn acp_disable_missing_session_does_not_create_instance_lock() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());

        let response = acp_disable(State(state.clone()), Path("missing".to_string()))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(state.instance_locks.read().await.is_empty());
    }

    #[tokio::test]
    async fn acp_replay_view_rows_matches_frames_pagination() {
        use crate::acp::transcript::TranscriptRowKind;
        use axum::body::to_bytes;

        let inst = crate::session::Instance::new("t", "/tmp");
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // Scripted session: a prompt, two assistant chunks, and a terminal
        // Stopped. Recorded straight into the store the handler reads.
        let events = [
            Event::UserPromptSent {
                text: "hello".into(),
                attachments: Vec::new(),
                prompt_id: None,
            },
            Event::AgentMessageChunk { text: "hi".into() },
            Event::AgentMessageChunk {
                text: " there".into(),
            },
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
        ];
        for (i, ev) in events.iter().enumerate() {
            state
                .acp_event_store
                .record(&id, i as u64 + 1, ev)
                .expect("record");
        }

        // A small page so has_more / next_cursor are non-trivial and must
        // match between the two projections.
        let base = ReplayQuery {
            since: 0,
            limit: Some(2),
            before: None,
            view: None,
        };
        let read = |q: ReplayQuery| {
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                let resp = acp_replay(State(state), Path(id), axum::extract::Query(q))
                    .await
                    .into_response();
                let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                serde_json::from_slice::<ReplayResponse>(&bytes).unwrap()
            }
        };

        // Default projection: frames, no rows, byte-shape unchanged.
        let frames_resp = read(ReplayQuery { view: None, ..base }).await;
        assert_eq!(frames_resp.frames.len(), 2, "page holds the first 2 events");
        assert!(frames_resp.rows.is_none(), "no rows key on default replay");
        assert!(frames_resp.has_more);

        // `view=rows`: same page folded to rows, no frames, identical paging.
        let rows_resp = read(ReplayQuery {
            view: Some("rows".into()),
            ..base
        })
        .await;
        assert!(rows_resp.frames.is_empty(), "rows projection drops frames");
        let rows = rows_resp.rows.as_ref().expect("rows present");
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![TranscriptRowKind::UserPrompt, TranscriptRowKind::Message],
            "the 2-event page folds to a prompt row and one message row"
        );

        // Pagination metadata is identical across projections; only the payload
        // differs, so the client's cursor logic is unchanged.
        assert_eq!(rows_resp.next_cursor, frames_resp.next_cursor);
        assert_eq!(rows_resp.has_more, frames_resp.has_more);
        assert_eq!(rows_resp.highest_seq, frames_resp.highest_seq);
        assert_eq!(rows_resp.lowest_seq, frames_resp.lowest_seq);
        assert_eq!(rows_resp.lost, frames_resp.lost);
    }

    #[test]
    fn rate_limit_resume_marker_requires_latest_rate_limit_park() {
        let fallback = utc_ts("2099-01-01T00:00:00Z");
        assert_eq!(
            rate_limit_resume_marker_resets_at(None, None, fallback),
            None
        );

        let stopped = Event::Stopped {
            reason: "user_stopped".to_string(),
        };
        assert_eq!(
            rate_limit_resume_marker_resets_at(Some(&stopped), None, fallback),
            None
        );
    }

    #[test]
    fn rate_limit_resume_marker_uses_latest_rate_limit_reset() {
        let stopped = Event::Stopped {
            reason: "rate_limited".to_string(),
        };
        let resets_at = utc_ts("2099-02-03T04:05:06Z");
        let fallback = utc_ts("2099-01-01T00:00:00Z");
        let info = RateLimitInfo {
            status: "limited".to_string(),
            resets_at: Some(resets_at),
            kind: "rate_limit".to_string(),
        };

        assert_eq!(
            rate_limit_resume_marker_resets_at(Some(&stopped), Some(&info), fallback),
            Some(resets_at)
        );
    }

    #[test]
    fn rate_limit_resume_marker_falls_back_when_rate_limit_event_missing() {
        let stopped = Event::Stopped {
            reason: "rate_limited".to_string(),
        };
        let fallback = utc_ts("2099-01-01T00:00:00Z");

        assert_eq!(
            rate_limit_resume_marker_resets_at(Some(&stopped), None, fallback),
            Some(fallback)
        );
    }

    /// #3688: an exhausted-retries park still resumes manually. The marker
    /// gates whether the spawn path queues the interrupted prompt, so a
    /// None here would leave RESUME NOW spawning an idle worker.
    #[test]
    fn rate_limit_resume_marker_covers_exhausted_retries_park() {
        let stopped = Event::Stopped {
            reason: "rate_limit_exhausted_retries".to_string(),
        };
        let fallback = utc_ts("2099-01-01T00:00:00Z");

        assert_eq!(
            rate_limit_resume_marker_resets_at(Some(&stopped), None, fallback),
            Some(fallback)
        );
    }

    /// The park exists but the agent reported no reset: the marker has to
    /// fall through to the caller's fallback, or auto-resume loses its
    /// schedule for exactly the sessions #3152 is about.
    #[test]
    fn rate_limit_resume_marker_falls_back_when_reset_unknown() {
        let stopped = Event::Stopped {
            reason: "rate_limited".to_string(),
        };
        let fallback = utc_ts("2099-01-01T00:00:00Z");
        let info = RateLimitInfo {
            status: "limited".to_string(),
            resets_at: None,
            kind: "rate_limit".to_string(),
        };

        assert_eq!(
            rate_limit_resume_marker_resets_at(Some(&stopped), Some(&info), fallback),
            Some(fallback)
        );
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

    #[test]
    fn recency_only_persist_does_not_itself_clear_a_peer_sink() {
        // Scope, deliberately narrow: this pins that the recency-only tier
        // performs no clear of its own on a sink this daemon's memory has
        // not observed. It does NOT claim the archive survives everything
        // afterwards -- advancing last_accessed_at still arms
        // merge_user_action_diff's `touched` arm for a later writer holding
        // a pre-prompt snapshot. See apply_prompt_persist_to_disk's docs.
        let mut disk = crate::session::Instance::new("s", "/tmp/x");
        disk.view = crate::session::View::Structured;
        disk.last_accessed_at = Some(chrono::Utc::now() - chrono::Duration::seconds(60));
        disk.archived_at = Some(chrono::Utc::now() - chrono::Duration::seconds(30));

        apply_prompt_persist_to_disk(&mut disk, false);

        assert!(
            disk.archived_at.is_some(),
            "the recency-only tier must not clear peer sink state itself"
        );
        assert!(disk.last_accessed_at > disk.archived_at);
    }

    #[test]
    fn wake_persist_keeps_touch_semantics_for_sunk_rows() {
        let mut disk = crate::session::Instance::new("s", "/tmp/x");
        disk.view = crate::session::View::Structured;
        disk.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::minutes(10));

        apply_prompt_persist_to_disk(&mut disk, true);

        assert!(
            disk.snoozed_until.is_none(),
            "wake persist lifts the sunk row (messaging-unarchives)"
        );
        assert!(disk.last_accessed_at.is_some());
    }
}
